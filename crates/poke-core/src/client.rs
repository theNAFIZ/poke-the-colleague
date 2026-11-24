//! Long-running relay client: connects, keeps the connection alive, reconnects
//! with backoff, encrypts outgoing and decrypts incoming messages.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tracing::{debug, info, warn};
use url::Url;

use crate::crypto::TeamKeys;
use crate::protocol::*;
use crate::{proxy, relay_ws_url, Error};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const PING_EVERY: Duration = Duration::from_secs(25);
/// No frame at all (not even `pong`) for this long => connection is dead (e.g. after sleep).
const DEAD_AFTER: Duration = Duration::from_secs(70);
const MAX_BACKOFF_SECS: u64 = 30;
const REJECTED_BACKOFF_SECS: u64 = 120;
/// Accept pokes whose timestamp is within this window (clock skew + latency).
const FRESHNESS_SECS: u64 = 300;

#[derive(Debug, Clone)]
pub struct ClientConfig {
    pub relay_url: String,
    pub team_code: String,
    /// Stable per-installation id (`[A-Za-z0-9-]{8,64}`), e.g. a UUID.
    pub client_id: String,
    pub profile: Profile,
    /// `None`/empty = auto-detect, `"none"` = direct, or `http://host:port`.
    pub proxy: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ConnState {
    Connecting,
    Online,
    Offline { reason: String, retry_in_secs: u64 },
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Peer {
    pub id: String,
    pub name: String,
    pub presence: Presence,
    pub in_call: bool,
    pub locked: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    State { state: ConnState },
    /// Full snapshot of online teammates (excluding ourselves), sorted by name.
    Peers { peers: Vec<Peer> },
    /// `all` = this was a "poke everyone".
    Poke { from: String, from_name: String, pid: String, message: Option<String>, ts: u64, all: bool },
    /// `text` = the receiver's own wording for the answer, if they customised it.
    Reply { from: String, from_name: String, pid: String, kind: ReplyKind, text: Option<String> },
    /// For a poke-all there is one `Ack` per recipient; `to == "*"` means
    /// the poke-all reached nobody (or could not be sent at all).
    Ack { pid: String, to: String, status: AckStatus },
}

enum Cmd {
    Poke { to: String, pid: String, message: Option<String> },
    PokeAll { pid: String, message: Option<String> },
    Reply { to: String, pid: String, kind: ReplyKind, text: Option<String> },
    Profile(Profile),
    ReconnectNow,
    Shutdown,
}

enum End {
    Shutdown,
    Lost(String),
}

/// Handle to a running client. Dropping it shuts the client down.
pub struct Client {
    tx: mpsc::UnboundedSender<Cmd>,
    team_id: String,
}

impl Client {
    /// Start inside the current Tokio runtime.
    pub fn start(cfg: ClientConfig) -> Result<(Client, mpsc::UnboundedReceiver<Event>), Error> {
        Self::start_on(&tokio::runtime::Handle::current(), cfg)
    }

    pub fn start_on(
        rt: &tokio::runtime::Handle,
        cfg: ClientConfig,
    ) -> Result<(Client, mpsc::UnboundedReceiver<Event>), Error> {
        crate::init_tls();
        let keys = TeamKeys::derive(&cfg.team_code)?;
        let url = relay_ws_url(&cfg.relay_url)?;
        if !valid_client_id(&cfg.client_id) {
            return Err(Error::InvalidClientId);
        }
        let (tx, cmds) = mpsc::unbounded_channel();
        let (ev, events) = mpsc::unbounded_channel();
        let team_id = keys.team_id().to_string();
        let runner = Runner {
            keys,
            url,
            client_id: cfg.client_id,
            proxy: cfg.proxy,
            profile: sanitize_profile(cfg.profile),
            ev,
            cmds,
            seen: VecDeque::new(),
        };
        rt.spawn(runner.run());
        Ok((Client { tx, team_id }, events))
    }

    pub fn team_id(&self) -> &str {
        &self.team_id
    }

    /// Send a poke; returns the poke id used to correlate `Ack`/`Reply` events.
    pub fn poke(&self, to: &str, message: Option<&str>) -> String {
        let pid = uuid::Uuid::new_v4().simple().to_string();
        let message = message.map(|m| truncate_chars(m, MAX_MESSAGE_CHARS)).filter(|m| !m.is_empty());
        let _ = self.tx.send(Cmd::Poke { to: to.to_string(), pid: pid.clone(), message });
        pid
    }

    /// Poke everybody currently online in the team with one message.
    pub fn poke_all(&self, message: Option<&str>) -> String {
        let pid = uuid::Uuid::new_v4().simple().to_string();
        let message = message.map(|m| truncate_chars(m, MAX_MESSAGE_CHARS)).filter(|m| !m.is_empty());
        let _ = self.tx.send(Cmd::PokeAll { pid: pid.clone(), message });
        pid
    }

    pub fn reply(&self, to: &str, pid: &str, kind: ReplyKind) {
        self.reply_with_text(to, pid, kind, None);
    }

    /// Reply with the user's own wording; `kind` tells older clients what it means.
    pub fn reply_with_text(&self, to: &str, pid: &str, kind: ReplyKind, text: Option<&str>) {
        let text = text.map(|t| truncate_chars(t, MAX_REPLY_CHARS)).filter(|t| !t.is_empty());
        let _ = self.tx.send(Cmd::Reply { to: to.into(), pid: pid.into(), kind, text });
    }

    pub fn set_profile(&self, profile: Profile) {
        let _ = self.tx.send(Cmd::Profile(profile));
    }

    /// Skip the backoff wait / drop a possibly stale connection (e.g. after resume).
    pub fn reconnect_now(&self) {
        let _ = self.tx.send(Cmd::ReconnectNow);
    }

    pub fn shutdown(&self) {
        let _ = self.tx.send(Cmd::Shutdown);
    }
}

pub fn valid_client_id(id: &str) -> bool {
    (8..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn sanitize_profile(mut p: Profile) -> Profile {
    p.name = truncate_chars(&p.name, MAX_NAME_CHARS);
    p
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

struct Runner {
    keys: TeamKeys,
    url: Url,
    client_id: String,
    proxy: Option<String>,
    profile: Profile,
    ev: mpsc::UnboundedSender<Event>,
    cmds: mpsc::UnboundedReceiver<Cmd>,
    /// Recently seen poke ids (replay / duplicate protection).
    seen: VecDeque<String>,
}

impl Runner {
    fn emit(&self, e: Event) {
        let _ = self.ev.send(e);
    }

    async fn run(mut self) {
        let mut backoff = 1u64;
        loop {
            self.emit(Event::State { state: ConnState::Connecting });
            let mut rejected = false;
            let reason = match tokio::time::timeout(CONNECT_TIMEOUT, self.connect()).await {
                Err(_) => "connection timed out".to_string(),
                Ok(Err(e)) => {
                    rejected = matches!(e, Error::Rejected(_));
                    e.to_string()
                }
                Ok(Ok(ws)) => {
                    let started = Instant::now();
                    match self.session(ws).await {
                        End::Shutdown => return,
                        End::Lost(reason) => {
                            if started.elapsed() > Duration::from_secs(20) {
                                backoff = 1;
                            }
                            reason
                        }
                    }
                }
            };
            info!("relay offline: {reason}");

            let delay = if rejected {
                REJECTED_BACKOFF_SECS
            } else {
                backoff + rand::random_range(0..=backoff / 2)
            };
            self.emit(Event::Peers { peers: vec![] });
            self.emit(Event::State { state: ConnState::Offline { reason, retry_in_secs: delay } });

            let deadline = tokio::time::Instant::now() + Duration::from_secs(delay);
            loop {
                tokio::select! {
                    _ = tokio::time::sleep_until(deadline) => break,
                    cmd = self.cmds.recv() => match cmd {
                        None | Some(Cmd::Shutdown) => return,
                        Some(Cmd::Poke { to, pid, .. }) => {
                            self.emit(Event::Ack { pid, to, status: AckStatus::NotConnected })
                        }
                        Some(Cmd::PokeAll { pid, .. }) => {
                            self.emit(Event::Ack { pid, to: "*".into(), status: AckStatus::NotConnected })
                        }
                        Some(Cmd::Reply { .. }) => {}
                        Some(Cmd::Profile(p)) => self.profile = sanitize_profile(p),
                        Some(Cmd::ReconnectNow) => break,
                    }
                }
            }
            backoff = (backoff * 2).min(MAX_BACKOFF_SECS);
        }
    }

    async fn connect(&self) -> Result<Ws, Error> {
        let host = self.url.host_str().ok_or_else(|| Error::InvalidRelayUrl("missing host".into()))?;
        let host = host.trim_matches(['[', ']']);
        let port = self.url.port_or_known_default().unwrap_or(443);
        let proxy = proxy::resolve(self.proxy.as_deref(), &self.url);
        if let Some(p) = &proxy {
            debug!("using proxy {}:{}", p.host, p.port);
        }
        let tcp = proxy::connect_tcp(host, port, proxy.as_ref()).await?;

        let mut req = self.url.as_str().into_client_request().map_err(|e| Error::Connect(e.to_string()))?;
        let h = req.headers_mut();
        let bearer = format!("Bearer {}", self.keys.auth_token());
        h.insert("authorization", HeaderValue::from_str(&bearer).map_err(|e| Error::Connect(e.to_string()))?);
        h.insert("x-poke-client", HeaderValue::from_str(&self.client_id).map_err(|_| Error::InvalidClientId)?);
        h.insert("x-poke-version", HeaderValue::from_static("1"));

        match tokio_tungstenite::client_async_tls_with_config(req, tcp, None, None).await {
            Ok((ws, _)) => Ok(ws),
            Err(tungstenite::Error::Http(resp)) => {
                let status = resp.status().as_u16();
                let body = resp.body().as_deref().map(String::from_utf8_lossy).unwrap_or_default().to_string();
                let detail = if body.trim().is_empty() { String::new() } else { format!(": {}", body.trim()) };
                if status == 401 || status == 403 {
                    Err(Error::Rejected(format!("relay rejected this team (HTTP {status}){detail}")))
                } else {
                    Err(Error::Connect(format!("relay answered HTTP {status}{detail}")))
                }
            }
            Err(e) => Err(Error::Connect(e.to_string())),
        }
    }

    fn sealed_profile(&self) -> String {
        let json = serde_json::to_vec(&self.profile).expect("serializable");
        self.keys.seal(&aad_profile(&self.client_id), &json)
    }

    async fn session(&mut self, ws: Ws) -> End {
        let (mut sink, mut stream) = ws.split();
        let mut peers: HashMap<String, Peer> = HashMap::new();

        let hello = ClientMsg::Hello { v: PROTOCOL_VERSION, profile: self.sealed_profile() };
        if let Err(e) = send(&mut sink, &hello).await {
            return End::Lost(e);
        }

        let mut ping = tokio::time::interval(PING_EVERY);
        ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ping.tick().await;
        let mut last_rx = Instant::now();

        loop {
            tokio::select! {
                frame = stream.next() => {
                    let frame = match frame {
                        None => return End::Lost("relay closed the connection".into()),
                        Some(Err(e)) => return End::Lost(e.to_string()),
                        Some(Ok(f)) => f,
                    };
                    last_rx = Instant::now();
                    match frame {
                        Message::Text(t) if t.as_str() == PONG => {}
                        Message::Text(t) => self.handle_server(t.as_str(), &mut peers),
                        Message::Close(c) => {
                            let reason = match c {
                                Some(c) if u16::from(c.code) == 4001 => "this account connected from another app instance".to_string(),
                                Some(c) if !c.reason.is_empty() => format!("relay closed the connection: {}", c.reason),
                                _ => "relay closed the connection".to_string(),
                            };
                            return End::Lost(reason);
                        }
                        _ => {}
                    }
                }
                cmd = self.cmds.recv() => {
                    let msg = match cmd {
                        None | Some(Cmd::Shutdown) => {
                            let _ = sink.send(Message::Close(None)).await;
                            return End::Shutdown;
                        }
                        Some(Cmd::ReconnectNow) => {
                            if last_rx.elapsed() > Duration::from_secs(30) {
                                return End::Lost("reconnecting".into());
                            }
                            continue;
                        }
                        Some(Cmd::Profile(p)) => {
                            self.profile = sanitize_profile(p);
                            ClientMsg::Profile { profile: self.sealed_profile() }
                        }
                        Some(Cmd::Poke { to, pid, message }) => {
                            let body = PokeBody { message, ts: now_secs() };
                            let aad = aad_poke(&self.client_id, &to, &pid);
                            let body = self.keys.seal(&aad, &serde_json::to_vec(&body).expect("serializable"));
                            ClientMsg::Poke { to, pid, body }
                        }
                        Some(Cmd::PokeAll { pid, message }) => {
                            let body = PokeBody { message, ts: now_secs() };
                            let aad = aad_poke_all(&self.client_id, &pid);
                            let body = self.keys.seal(&aad, &serde_json::to_vec(&body).expect("serializable"));
                            ClientMsg::PokeAll { pid, body }
                        }
                        Some(Cmd::Reply { to, pid, kind, text }) => {
                            let body = ReplyBody { kind, ts: now_secs(), text };
                            let aad = aad_reply(&self.client_id, &to, &pid);
                            let body = self.keys.seal(&aad, &serde_json::to_vec(&body).expect("serializable"));
                            ClientMsg::Reply { to, pid, body }
                        }
                    };
                    if let Err(e) = send(&mut sink, &msg).await {
                        return End::Lost(e);
                    }
                }
                _ = ping.tick() => {
                    if last_rx.elapsed() > DEAD_AFTER {
                        return End::Lost("no response from relay".into());
                    }
                    if let Err(e) = sink.send(Message::text(PING)).await {
                        return End::Lost(e.to_string());
                    }
                }
            }
        }
    }

    fn open_peer(&self, id: &str, sealed: &str) -> Option<Peer> {
        let raw = self.keys.open(&aad_profile(id), sealed).ok()?;
        let p: Profile = serde_json::from_slice(&raw).ok()?;
        let name = truncate_chars(&p.name, MAX_NAME_CHARS);
        let name = if name.is_empty() { "Unnamed".to_string() } else { name };
        Some(Peer { id: id.to_string(), name, presence: p.presence, in_call: p.in_call, locked: p.locked })
    }

    fn emit_peers(&self, peers: &HashMap<String, Peer>) {
        let mut list: Vec<Peer> = peers.values().cloned().collect();
        list.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()).then(a.id.cmp(&b.id)));
        self.emit(Event::Peers { peers: list });
    }

    fn remember(&mut self, pid: &str) -> bool {
        if self.seen.iter().any(|p| p == pid) {
            return false;
        }
        self.seen.push_back(pid.to_string());
        if self.seen.len() > 512 {
            self.seen.pop_front();
        }
        true
    }

    fn handle_server(&mut self, text: &str, peers: &mut HashMap<String, Peer>) {
        let msg: ServerMsg = match serde_json::from_str(text) {
            Ok(m) => m,
            Err(e) => {
                debug!("ignoring unknown frame ({e}): {text:.200}");
                return;
            }
        };
        let name_of = |peers: &HashMap<String, Peer>, id: &str| {
            peers.get(id).map(|p| p.name.clone()).unwrap_or_else(|| "A colleague".into())
        };
        match msg {
            ServerMsg::Welcome { peers: list, .. } => {
                peers.clear();
                for wp in list {
                    if wp.id == self.client_id {
                        continue;
                    }
                    match self.open_peer(&wp.id, &wp.profile) {
                        Some(p) => {
                            peers.insert(p.id.clone(), p);
                        }
                        None => warn!("could not decrypt profile of {}", wp.id),
                    }
                }
                self.emit_peers(peers);
                self.emit(Event::State { state: ConnState::Online });
            }
            ServerMsg::Join { id, profile } => {
                if id != self.client_id {
                    if let Some(p) = self.open_peer(&id, &profile) {
                        peers.insert(id, p);
                        self.emit_peers(peers);
                    }
                }
            }
            ServerMsg::Leave { id } => {
                if peers.remove(&id).is_some() {
                    self.emit_peers(peers);
                }
            }
            ServerMsg::Poke { from, pid, body, all } => {
                let aad = if all { aad_poke_all(&from, &pid) } else { aad_poke(&from, &self.client_id, &pid) };
                let parsed = self.keys.open(&aad, &body).ok().and_then(|b| serde_json::from_slice::<PokeBody>(&b).ok());
                let Some(pb) = parsed else {
                    warn!("dropping undecryptable poke from {from}");
                    return;
                };
                let now = now_secs();
                if pb.ts + FRESHNESS_SECS < now || pb.ts > now + FRESHNESS_SECS {
                    warn!("dropping stale poke from {from} (ts {}, now {now})", pb.ts);
                    return;
                }
                if !self.remember(&pid) {
                    return;
                }
                let message = pb.message.map(|m| truncate_chars(&m, MAX_MESSAGE_CHARS)).filter(|m| !m.is_empty());
                self.emit(Event::Poke { from_name: name_of(peers, &from), from, pid, message, ts: pb.ts, all });
            }
            ServerMsg::Reply { from, pid, body } => {
                let aad = aad_reply(&from, &self.client_id, &pid);
                let parsed = self.keys.open(&aad, &body).ok().and_then(|b| serde_json::from_slice::<ReplyBody>(&b).ok());
                match parsed {
                    Some(rb) => {
                        let text = rb.text.map(|t| truncate_chars(&t, MAX_REPLY_CHARS)).filter(|t| !t.is_empty());
                        self.emit(Event::Reply { from_name: name_of(peers, &from), from, pid, kind: rb.kind, text })
                    }
                    None => warn!("dropping undecryptable reply from {from}"),
                }
            }
            ServerMsg::Ack { pid, to, status } => self.emit(Event::Ack { pid, to, status }),
            ServerMsg::Error { code, msg } => warn!("relay error {code}: {msg}"),
        }
    }
}

async fn send<S>(sink: &mut S, msg: &ClientMsg) -> Result<(), String>
where
    S: futures_util::Sink<Message, Error = tungstenite::Error> + Unpin,
{
    let json = serde_json::to_string(msg).expect("serializable");
    sink.send(Message::text(json)).await.map_err(|e| e.to_string())
}
