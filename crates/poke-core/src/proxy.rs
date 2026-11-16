//! Outbound connection with optional HTTP CONNECT proxy.
//!
//! Proxy resolution order:
//! 1. explicit setting (`"none"`/`"direct"` disables proxying),
//! 2. `HTTPS_PROXY` / `ALL_PROXY` (or `HTTP_PROXY` for plain `ws://`), honouring `NO_PROXY`,
//! 3. Windows: the per-user WinINet proxy (Internet Options), if enabled.
//!
//! PAC / WPAD auto-config scripts are not evaluated; set the proxy explicitly in that case.

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use url::Url;

use crate::Error;

#[derive(Debug, Clone, PartialEq)]
pub struct Proxy {
    pub host: String,
    pub port: u16,
    pub basic_auth: Option<String>,
}

impl Proxy {
    pub fn parse(s: &str) -> Option<Proxy> {
        let s = s.trim();
        if s.is_empty() {
            return None;
        }
        let with_scheme = if s.contains("://") { s.to_string() } else { format!("http://{s}") };
        let url = Url::parse(&with_scheme).ok()?;
        if !matches!(url.scheme(), "http" | "https") {
            return None; // socks etc. not supported
        }
        let host = url.host_str()?.trim_matches(['[', ']']).to_string();
        let port = url.port().unwrap_or(if url.scheme() == "https" { 443 } else { 8080 });
        let basic_auth = (!url.username().is_empty()).then(|| {
            let user = urldecode(url.username());
            let pass = urldecode(url.password().unwrap_or(""));
            B64.encode(format!("{user}:{pass}"))
        });
        Some(Proxy { host, port, basic_auth })
    }
}

fn urldecode(s: &str) -> String {
    url::form_urlencoded::parse(format!("x={s}").as_bytes())
        .next()
        .map(|(_, v)| v.into_owned())
        .unwrap_or_default()
}

pub fn resolve(setting: Option<&str>, target: &Url) -> Option<Proxy> {
    if let Some(s) = setting.map(str::trim).filter(|s| !s.is_empty()) {
        if s.eq_ignore_ascii_case("none") || s.eq_ignore_ascii_case("direct") {
            return None;
        }
        return Proxy::parse(s);
    }
    let host = target.host_str().unwrap_or_default();
    if no_proxy_matches(host) {
        return None;
    }
    let vars: &[&str] = if target.scheme() == "wss" {
        &["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"]
    } else {
        &["HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"]
    };
    for v in vars {
        if let Some(p) = std::env::var(v).ok().as_deref().and_then(Proxy::parse) {
            return Some(p);
        }
    }
    system_proxy(target)
}

fn no_proxy_matches(host: &str) -> bool {
    let list = std::env::var("NO_PROXY").or_else(|_| std::env::var("no_proxy")).unwrap_or_default();
    list.split(',').map(str::trim).filter(|e| !e.is_empty()).any(|entry| {
        entry == "*" || host == entry.trim_start_matches('.') || host.ends_with(&format!(".{}", entry.trim_start_matches('.')))
    })
}

#[cfg(windows)]
fn system_proxy(target: &Url) -> Option<Proxy> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;
    let key = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(r"Software\Microsoft\Windows\CurrentVersion\Internet Settings")
        .ok()?;
    let enabled: u32 = key.get_value("ProxyEnable").ok()?;
    if enabled == 0 {
        return None;
    }
    let server: String = key.get_value("ProxyServer").ok()?;
    let host = target.host_str().unwrap_or_default();
    let overrides: String = key.get_value("ProxyOverride").unwrap_or_default();
    for o in overrides.split(';').map(str::trim) {
        let local = o == "<local>" && !host.contains('.');
        let pattern = o.trim_start_matches('*').trim_start_matches('.');
        if local || (!pattern.is_empty() && o != "<local>" && (host == pattern || host.ends_with(&format!(".{pattern}")))) {
            return None;
        }
    }
    // Either "host:port" or "http=h:p;https=h:p;..."
    if !server.contains('=') {
        return Proxy::parse(&server);
    }
    let wanted = if target.scheme() == "wss" { "https" } else { "http" };
    server
        .split(';')
        .filter_map(|kv| kv.split_once('='))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case(wanted))
        .and_then(|(_, v)| Proxy::parse(v))
}

#[cfg(not(windows))]
fn system_proxy(_target: &Url) -> Option<Proxy> {
    None
}

/// Open a TCP stream to `host:port`, tunnelling through `proxy` if given.
pub async fn connect_tcp(host: &str, port: u16, proxy: Option<&Proxy>) -> Result<TcpStream, Error> {
    let Some(p) = proxy else {
        let s = TcpStream::connect((host, port)).await.map_err(|e| Error::Connect(e.to_string()))?;
        let _ = s.set_nodelay(true);
        return Ok(s);
    };
    let mut s = TcpStream::connect((p.host.as_str(), p.port))
        .await
        .map_err(|e| Error::Connect(format!("proxy {}:{}: {e}", p.host, p.port)))?;
    let _ = s.set_nodelay(true);
    let authority = if host.contains(':') { format!("[{host}]:{port}") } else { format!("{host}:{port}") };
    let mut req = format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n");
    if let Some(auth) = &p.basic_auth {
        req.push_str(&format!("Proxy-Authorization: Basic {auth}\r\n"));
    }
    req.push_str("\r\n");
    s.write_all(req.as_bytes()).await.map_err(|e| Error::Connect(e.to_string()))?;

    // Read the response head byte-wise so we never consume TLS bytes.
    let mut head = Vec::with_capacity(256);
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() > 8192 {
            return Err(Error::Connect("proxy response too large".into()));
        }
        let n = s.read(&mut byte).await.map_err(|e| Error::Connect(e.to_string()))?;
        if n == 0 {
            return Err(Error::Connect("proxy closed the connection".into()));
        }
        head.push(byte[0]);
    }
    let status_line = String::from_utf8_lossy(&head);
    let status_line = status_line.lines().next().unwrap_or_default();
    let ok = status_line.split_whitespace().nth(1).is_some_and(|c| c.starts_with('2'));
    if !ok {
        return Err(Error::Connect(format!("proxy refused tunnel: {status_line}")));
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_proxy_strings() {
        assert_eq!(
            Proxy::parse("proxy.corp:3128"),
            Some(Proxy { host: "proxy.corp".into(), port: 3128, basic_auth: None })
        );
        let p = Proxy::parse("http://bob:p%40ss@10.0.0.1:8080").unwrap();
        assert_eq!(p.host, "10.0.0.1");
        assert_eq!(p.basic_auth.as_deref(), Some(B64.encode("bob:p@ss").as_str()));
        assert_eq!(Proxy::parse("socks5://x:1"), None);
        assert_eq!(Proxy::parse(""), None);
    }

    #[test]
    fn explicit_none_disables() {
        let u = Url::parse("wss://relay.example/ws").unwrap();
        assert_eq!(resolve(Some("none"), &u), None);
        assert_eq!(resolve(Some("p:1"), &u).unwrap().port, 1);
    }
}
