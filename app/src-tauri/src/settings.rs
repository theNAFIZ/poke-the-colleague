//! Persistent per-user settings (JSON in the OS config dir, no admin rights needed).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

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
