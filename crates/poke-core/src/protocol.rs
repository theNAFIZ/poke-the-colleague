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
pub const MAX_MESSAGE_CHARS: usize = 280;

// ---------- Frames ----------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ClientMsg {
    Hello { v: u32, profile: String },
    Profile { profile: String },
    Poke { to: String, pid: String, body: String },
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
    Poke { from: String, pid: String, body: String },
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
    #[serde(default)]
    pub presence: Presence,
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
    Coming,
    FiveMin,
    Busy,
    Dismissed,
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

        let r = ReplyBody { kind: ReplyKind::FiveMin, ts: 1 };
        assert_eq!(serde_json::to_string(&r).unwrap(), r#"{"kind":"five_min","ts":1}"#);
    }
}
