//! Core of Poke the Colleague: everything that is not GUI.
//!
//! * [`crypto`]   – team code -> keys, end-to-end encryption
//! * [`protocol`] – wire frames and encrypted payload types
//! * [`client`]   – reconnecting relay client producing [`client::Event`]s
//! * [`proxy`]    – HTTP CONNECT proxy support

pub mod client;
pub mod crypto;
pub mod protocol;
pub mod proxy;

pub use client::{Client, ClientConfig, ConnState, Event, Peer};
pub use crypto::{generate_team_code, normalize_team_code, TeamKeys};
pub use protocol::{AckStatus, Presence, Profile, ReplyKind};

use url::Url;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid team code (expected 24 letters/digits like ABCD-EFGH-IJKL-MNOP-QRST-UVWX)")]
    InvalidTeamCode,
    #[error("invalid relay address: {0}")]
    InvalidRelayUrl(String),
    #[error("invalid client id")]
    InvalidClientId,
    #[error("could not decrypt message")]
    Decrypt,
    #[error("{0}")]
    Connect(String),
    #[error("{0}")]
    Rejected(String),
}

/// Install the rustls crypto provider (idempotent).
pub fn init_tls() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// Turn whatever the user typed (`relay.example.workers.dev`, `https://…`,
/// `http://localhost:8787`) into the relay's WebSocket URL.
pub fn relay_ws_url(input: &str) -> Result<Url, Error> {
    let s = input.trim().trim_end_matches('/');
    if s.is_empty() {
        return Err(Error::InvalidRelayUrl("empty".into()));
    }
    let with_scheme = if s.contains("://") {
        s.to_string()
    } else if s.starts_with("localhost") || s.starts_with("127.") || s.starts_with("[::1]") {
        format!("ws://{s}")
    } else {
        format!("wss://{s}")
    };
    let mut url = Url::parse(&with_scheme).map_err(|e| Error::InvalidRelayUrl(e.to_string()))?;
    let scheme_ok = match url.scheme() {
        "https" => url.set_scheme("wss").is_ok(),
        "http" => url.set_scheme("ws").is_ok(),
        "ws" | "wss" => true,
        _ => false,
    };
    if !scheme_ok || url.host_str().is_none() {
        return Err(Error::InvalidRelayUrl(input.to_string()));
    }
    if url.path().is_empty() || url.path() == "/" {
        url.set_path("/ws");
    }
    url.set_fragment(None);
    url.set_query(None);
    Ok(url)
}

/// Human-friendly relay address (`https://host` form) for display and invites.
pub fn relay_display_url(input: &str) -> Result<String, Error> {
    let mut url = relay_ws_url(input)?;
    let scheme = if url.scheme() == "ws" { "http" } else { "https" };
    let _ = url.set_scheme(scheme);
    if url.path() == "/ws" {
        url.set_path("");
    }
    Ok(url.as_str().trim_end_matches('/').to_string())
}

/// Single string to share with colleagues: `https://relay.example#ABCD-…`.
pub fn make_invite(relay: &str, team_code: &str) -> Result<String, Error> {
    Ok(format!("{}#{}", relay_display_url(relay)?, normalize_team_code(team_code)?))
}

/// Parse an invite into `(relay display url, team code)`.
pub fn parse_invite(invite: &str) -> Result<(String, String), Error> {
    let (relay, code) = invite.trim().rsplit_once('#').ok_or(Error::InvalidTeamCode)?;
    Ok((relay_display_url(relay)?, normalize_team_code(code)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relay_urls() {
        assert_eq!(relay_ws_url("poke.acme.workers.dev").unwrap().as_str(), "wss://poke.acme.workers.dev/ws");
        assert_eq!(relay_ws_url("https://poke.acme.workers.dev/").unwrap().as_str(), "wss://poke.acme.workers.dev/ws");
        assert_eq!(relay_ws_url("localhost:8787").unwrap().as_str(), "ws://localhost:8787/ws");
        assert_eq!(relay_ws_url("http://127.0.0.1:8787").unwrap().as_str(), "ws://127.0.0.1:8787/ws");
        assert!(relay_ws_url("ftp://x").is_err());
        assert!(relay_ws_url("  ").is_err());
    }

    #[test]
    fn invites_roundtrip() {
        let code = generate_team_code();
        let inv = make_invite("poke.acme.workers.dev", &code).unwrap();
        assert_eq!(inv, format!("https://poke.acme.workers.dev#{code}"));
        assert_eq!(parse_invite(&inv).unwrap(), ("https://poke.acme.workers.dev".to_string(), code.clone()));
        let local = make_invite("localhost:8787", &code).unwrap();
        assert_eq!(local, format!("http://localhost:8787#{code}"));
        assert!(parse_invite("no-hash-here").is_err());
    }
}
