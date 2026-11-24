//! Persistent per-user settings (JSON in the OS config dir, no admin rights needed).

use std::path::{Path, PathBuf};

use poke_core::ReplyKind;
use serde::{Deserialize, Serialize};

pub const MAX_REPLIES: usize = 6;
pub const MAX_QUICK_MESSAGES: usize = 8;

/// One answer button on the full-screen alert.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuickReply {
    pub label: String,
    /// What it means for the caller: `coming`, `five_min` (later) or `busy`.
    pub kind: ReplyKind,
}

impl QuickReply {
    fn new(label: &str, kind: ReplyKind) -> Self {
        Self { label: label.into(), kind }
    }

    /// The built-in buttons. Choosing one of these sends no custom text, so the
    /// caller sees the usual wording ("On the way 🏃").
    pub fn defaults() -> Vec<QuickReply> {
        vec![
            Self::new("🏃 Coming", ReplyKind::Coming),
            Self::new("⏳ In 5 min", ReplyKind::FiveMin),
            Self::new("⛔ Busy", ReplyKind::Busy),
        ]
    }

    pub fn is_default(&self) -> bool {
        Self::defaults().contains(self)
    }

    pub fn kind_allowed(kind: ReplyKind) -> bool {
        matches!(kind, ReplyKind::Coming | ReplyKind::FiveMin | ReplyKind::Busy)
    }
}

pub fn default_quick_messages() -> Vec<String> {
    ["Join the meeting 📅", "Lunch time 🍽️", "Nasta time 🥟"].map(String::from).to_vec()
}

/// Trim, drop empty/invalid entries, cap lengths and counts. Empty → defaults.
pub fn clean_replies(list: Vec<QuickReply>) -> Vec<QuickReply> {
    let out: Vec<QuickReply> = list
        .into_iter()
        .filter(|r| QuickReply::kind_allowed(r.kind))
        .map(|r| QuickReply { label: poke_core::protocol::truncate_chars(&r.label, poke_core::protocol::MAX_REPLY_CHARS), kind: r.kind })
        .filter(|r| !r.label.is_empty())
        .take(MAX_REPLIES)
        .collect();
    if out.is_empty() {
        QuickReply::defaults()
    } else {
        out
    }
}

pub fn clean_quick_messages(list: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for m in list {
        let m = poke_core::protocol::truncate_chars(&m, poke_core::protocol::MAX_MESSAGE_CHARS);
        if !m.is_empty() && !out.contains(&m) && out.len() < MAX_QUICK_MESSAGES {
            out.push(m);
        }
    }
    out
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Stable id of this installation, generated on first run.
    pub client_id: String,
    pub display_name: String,
    /// e.g. `https://poke-relay.example.workers.dev`
    pub relay_url: String,
    pub team_code: String,
    /// Empty = auto-detect, `none` = direct, or `http://host:port`.
    pub proxy: String,
    pub dnd: bool,
    pub sound: bool,
    /// 0.0 ..= 1.0
    pub volume: f32,
    /// Full-screen alert closes itself (and tells the caller "missed") after this.
    pub alert_timeout_secs: u32,
    /// Treat "microphone in use" as "in a call": quiet notification instead of full-screen.
    pub detect_calls: bool,
    /// Answer buttons on the full-screen alert (first one = Enter).
    pub replies: Vec<QuickReply>,
    /// One-click messages above the colleague list. May be empty.
    pub quick_messages: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            client_id: uuid::Uuid::new_v4().to_string(),
            display_name: String::new(),
            relay_url: String::new(),
            team_code: String::new(),
            proxy: String::new(),
            dnd: false,
            sound: true,
            volume: 0.8,
            alert_timeout_secs: 60,
            detect_calls: true,
            replies: QuickReply::defaults(),
            quick_messages: default_quick_messages(),
        }
    }
}

impl Settings {
    pub fn is_configured(&self) -> bool {
        !self.display_name.trim().is_empty() && !self.relay_url.trim().is_empty() && !self.team_code.trim().is_empty()
    }

    pub fn path(config_dir: &Path) -> PathBuf {
        config_dir.join("settings.json")
    }

    pub fn load(config_dir: &Path) -> Settings {
        let path = Self::path(config_dir);
        let mut s: Settings = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        if !poke_core::client::valid_client_id(&s.client_id) {
            s.client_id = uuid::Uuid::new_v4().to_string();
        }
        s.volume = s.volume.clamp(0.0, 1.0);
        s.alert_timeout_secs = s.alert_timeout_secs.clamp(10, 600);
        s.replies = clean_replies(std::mem::take(&mut s.replies));
        s.quick_messages = clean_quick_messages(std::mem::take(&mut s.quick_messages));
        // Persist the generated client id right away so it stays stable.
        if !path.exists() {
            let _ = s.save(config_dir);
        }
        s
    }

    pub fn save(&self, config_dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(config_dir)?;
        let path = Self::path(config_dir);
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self).expect("serializable"))?;
        std::fs::rename(tmp, path)
    }
}
