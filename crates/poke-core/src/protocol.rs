//! Wire protocol between clients and the relay (JSON text frames over WebSocket).
//!
//! Authentication happens during the HTTP upgrade:
//!   `Authorization: Bearer <auth_token>` and `X-Poke-Client: <client id>`.
//! Everything inside `profile` / `body` fields is sealed with the team key, so
//! the relay only routes opaque blobs. Keep in sync with `relay/src/index.ts`.
//!
//! Keep-alive: the client sends the raw text frame `ping`, the relay answers `pong`.

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;
pub const PING: &str = "ping";
pub const PONG: &str = "pong";

pub const MAX_NAME_CHARS: usize = 64;
/// Nobody can be poked twice within this window (enforced by app *and* relay).
pub const POKE_COOLDOWN_SECS: u64 = 60;
pub const MAX_MESSAGE_CHARS: usize = 280;

// ---------- Frames ----------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ClientMsg {
    Hello { v: u32, profile: String },
    Profile { profile: String },
    Poke { to: String, pid: String, body: String },
    /// One sealed body for everybody online in the team (sealed with [`aad_poke_all`]).
    PokeAll { pid: String, body: String },
    Reply { to: String, pid: String, body: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WirePeer {
    pub id: String,
    pub profile: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ServerMsg {
    Welcome { you: String, peers: Vec<WirePeer> },
    Join { id: String, profile: String },
    Leave { id: String },
    Poke {
        from: String,
        pid: String,
        body: String,
        /// Part of a "poke everyone"; body is sealed with [`aad_poke_all`].
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        all: bool,
    },
    Reply { from: String, pid: String, body: String },
    Ack { pid: String, to: String, status: AckStatus },
    Error { code: String, msg: String },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AckStatus {
    /// Relay handed the poke to the recipient's open connection.
    Delivered,
    /// Recipient is not connected to the relay.
    Offline,
    /// Relay refused because the sender is poking too fast.
    RateLimited,
    /// This colleague was already poked by us within the last minute.
    TooSoon,
    /// Generated locally: we are not connected to the relay.
    NotConnected,
}

// ---------- Encrypted payloads ----------

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Presence {
    #[default]
    Available,
    /// Do-not-disturb: pokes are queued silently instead of going full-screen.
    Dnd,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct Profile {
    pub name: String,
    /// Manual status (available / do-not-disturb).
    #[serde(default)]
    pub presence: Presence,
    /// Detected automatically: the microphone is in use (Teams, Zoom, Meet, ...).
    #[serde(default)]
    pub in_call: bool,
    /// Detected automatically: the screen is locked.
    #[serde(default)]
    pub locked: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PokeBody {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Unix seconds at the sender; used for replay protection.
    pub ts: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReplyKind {
    /// Automatic: the full-screen alert is showing.
    Seen,
    /// Automatic: receiver is in do-not-disturb, poke was queued.
    Dnd,
    /// Automatic: alert timed out without an answer.
    Missed,
    /// Automatic: receiver is in a call; they got a quiet notification instead.
    InCall,
    /// Automatic: receiver's screen is locked; they'll see it when they return.
    Locked,
    Coming,
    FiveMin,
    Busy,
    Dismissed,
    /// A kind added by a newer version.
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReplyBody {
    pub kind: ReplyKind,
    pub ts: u64,
}

// ---------- AAD contexts (bind ciphertext to its routing metadata) ----------

pub fn aad_profile(client_id: &str) -> String {
    format!("profile|{client_id}")
}
pub fn aad_poke(from: &str, to: &str, pid: &str) -> String {
    format!("poke|{from}|{to}|{pid}")
}
pub fn aad_poke_all(from: &str, pid: &str) -> String {
    format!("poke-all|{from}|{pid}")
}
pub fn aad_reply(from: &str, to: &str, pid: &str) -> String {
    format!("reply|{from}|{to}|{pid}")
}

pub fn truncate_chars(s: &str, max: usize) -> String {
    s.trim().chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_format_is_stable() {
        let m = ClientMsg::Poke { to: "b".into(), pid: "p1".into(), body: "x".into() };
        assert_eq!(serde_json::to_string(&m).unwrap(), r#"{"t":"poke","to":"b","pid":"p1","body":"x"}"#);

        let s: ServerMsg =
            serde_json::from_str(r#"{"t":"ack","pid":"p1","to":"b","status":"rate_limited"}"#).unwrap();
        assert_eq!(s, ServerMsg::Ack { pid: "p1".into(), to: "b".into(), status: AckStatus::RateLimited });

        let all = ClientMsg::PokeAll { pid: "p2".into(), body: "y".into() };
        assert_eq!(serde_json::to_string(&all).unwrap(), r#"{"t":"poke_all","pid":"p2","body":"y"}"#);

        let p: ServerMsg = serde_json::from_str(r#"{"t":"poke","from":"a","pid":"p","body":"b"}"#).unwrap();
        assert!(matches!(p, ServerMsg::Poke { all: false, .. }));

        let r = ReplyBody { kind: ReplyKind::FiveMin, ts: 1 };
        assert_eq!(serde_json::to_string(&r).unwrap(), r#"{"kind":"five_min","ts":1}"#);
    }

    #[test]
    fn profiles_stay_compatible() {
        // A 0.1.0 profile (no in_call/locked) still parses...
        let old: Profile = serde_json::from_str(r#"{"name":"A","presence":"dnd"}"#).unwrap();
        assert!(!old.in_call && !old.locked && old.presence == Presence::Dnd);
        // ...and unknown future reply kinds don't break parsing.
        let r: ReplyBody = serde_json::from_str(r#"{"kind":"teleported","ts":1}"#).unwrap();
        assert_eq!(r.kind, ReplyKind::Other);
    }
}
