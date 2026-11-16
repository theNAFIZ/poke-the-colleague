//! Application state and the glue between the relay client, the UI windows,
//! the full-screen overlay, the sound and the automatic presence watcher.
//!
//! What happens to an incoming poke depends on the receiver's state:
//!
//! | state          | alert                         | caller is told |
//! |----------------|-------------------------------|----------------|
//! | screen locked  | none, queued                  | `locked`       |
//! | do-not-disturb | none, queued                  | `dnd`          |
//! | in a call      | desktop notification + chime  | `in_call`      |
//! | otherwise      | full-screen + ring            | `seen`, then the answer |
//!
//! Pokes queued while locked / in a call are shown again ("while you were
//! away") as soon as the screen is unlocked or the call ends.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use poke_core::protocol::POKE_COOLDOWN_SECS;
use poke_core::{AckStatus, Client, ClientConfig, ConnState, Event, Peer, Presence, Profile, ReplyKind};
use serde::Serialize;
use tauri::menu::CheckMenuItem;
use tauri::tray::TrayIcon;
use tauri::{AppHandle, Emitter, Manager, Wry};
use tauri_plugin_autostart::ManagerExt as _;
use tauri_plugin_notification::NotificationExt as _;
use tracing::{info, warn};

use crate::overlay;
use crate::settings::Settings;
use crate::sound::Sound;

/// Nobody can be poked twice within a minute (the relay enforces it too).
const POKE_COOLDOWN: Duration = Duration::from_secs(POKE_COOLDOWN_SECS);
const MAX_HISTORY: usize = 50;
const MAX_OUTGOING: usize = 60;
/// Sender id of the local "Test alert" (never a valid relay client id).
const TEST_SENDER: &str = "test";

#[derive(Debug, Clone, Serialize)]
pub struct Incoming {
    pub pid: String,
    pub from: String,
    pub from_name: String,
    pub message: Option<String>,
    /// Unix milliseconds.
    pub received_at: u64,
    /// Sent to everyone ("poke all").
    pub all: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Outgoing {
    pub pid: String,
    pub to: String,
    pub to_name: String,
    pub message: Option<String>,
    pub sent_at: u64,
    /// sending | delivered | offline | rate_limited | too_soon | not_connected |
    /// seen | dnd | in_call | locked | missed | coming | five_min | busy | dismissed
    pub status: String,
    pub all: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct HistoryItem {
    pub pid: String,
    pub from: String,
    pub from_name: String,
    pub message: Option<String>,
    pub at: u64,
    /// coming | five_min | busy | dismissed | missed | dnd | in_call | locked
    pub outcome: String,
    pub all: bool,
    /// Not acknowledged yet (shown highlighted, counted as "missed").
    pub unread: bool,
}

/// Everything the main window renders.
#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub configured: bool,
    pub display_name: String,
    pub relay_url: String,
    pub team_code: String,
    pub proxy: String,
    pub dnd: bool,
    pub sound: bool,
    pub volume: f32,
    pub alert_timeout_secs: u32,
    pub detect_calls: bool,
    pub autostart: bool,
    pub team_id: Option<String>,
    pub invite: Option<String>,
    pub conn: ConnState,
    pub peers: Vec<Peer>,
    pub outgoing: Vec<Outgoing>,
    pub history: Vec<HistoryItem>,
    pub missed: usize,
    /// App holding the microphone, if we think we're in a call.
    pub in_call: Option<String>,
    /// Peer id -> unix ms until which they can't be poked again.
    pub cooldowns: HashMap<String, u64>,
    /// Unix ms until which "poke everyone" is unavailable (0 = available).
    pub poke_all_until: u64,
    pub version: &'static str,
    /// Newer version available on GitHub, with its release notes.
    pub update_version: Option<String>,
    pub update_notes: Option<String>,
    /// "Downloading… 40%", "Installing…" while an update runs.
    pub update_progress: Option<String>,
    /// Linux .deb: installing asks for the user's password.
    pub update_needs_password: bool,
}

struct Inner {
    settings: Settings,
    client: Option<Client>,
    /// Bumped on every client restart so events from an old client are ignored.
    generation: u64,
    team_id: Option<String>,
    conn: ConnState,
    peers: Vec<Peer>,
    incoming: Vec<Incoming>,
    outgoing: VecDeque<Outgoing>,
    history: VecDeque<HistoryItem>,
    last_poke: HashMap<String, Instant>,
    last_poke_all: Option<Instant>,
    /// Local automatic presence.
    locked: bool,
    in_call: Option<String>,
    /// Pokes that arrived while locked / in a call, announced when back.
    away: Vec<String>,
    dnd_item: Option<CheckMenuItem<Wry>>,
    tray: Option<TrayIcon<Wry>>,
    update: Option<(String, Option<String>)>,
    update_progress: Option<String>,
    /// Last update version we already notified about.
    update_notified: Option<String>,
}

impl Inner {
    fn profile(&self) -> Profile {
        Profile {
            name: self.settings.display_name.clone(),
            presence: if self.settings.dnd { Presence::Dnd } else { Presence::Available },
            in_call: self.in_call.is_some(),
            locked: self.locked,
        }
    }

    fn unread(&self) -> usize {
        self.history.iter().filter(|h| h.unread).count()
    }

    fn peer_name(&self, id: &str) -> String {
        self.peers.iter().find(|p| p.id == id).map(|p| p.name.clone()).unwrap_or_else(|| "Colleague".into())
    }

    /// Time left before `to` may be poked again.
    fn cooldown_left(&self, to: &str) -> Option<Duration> {
        let last = [self.last_poke.get(to).copied(), self.last_poke_all].into_iter().flatten().max()?;
        POKE_COOLDOWN.checked_sub(last.elapsed()).filter(|d| !d.is_zero())
    }

    fn poke_all_left(&self) -> Option<Duration> {
        POKE_COOLDOWN.checked_sub(self.last_poke_all?.elapsed()).filter(|d| !d.is_zero())
    }
}

pub struct AppState {
    handle: AppHandle,
    config_dir: PathBuf,
    sound: Sound,
    inner: Mutex<Inner>,
}

pub type Shared = Arc<AppState>;

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn reply_str(kind: ReplyKind) -> &'static str {
    match kind {
        ReplyKind::Seen => "seen",
        ReplyKind::Dnd => "dnd",
        ReplyKind::Missed => "missed",
        ReplyKind::InCall => "in_call",
        ReplyKind::Locked => "locked",
        ReplyKind::Coming => "coming",
        ReplyKind::FiveMin => "five_min",
        ReplyKind::Busy => "busy",
        ReplyKind::Dismissed => "dismissed",
        ReplyKind::Other => "other",
    }
}

fn ack_str(s: AckStatus) -> &'static str {
    match s {
        AckStatus::Delivered => "delivered",
        AckStatus::Offline => "offline",
        AckStatus::RateLimited => "rate_limited",
        AckStatus::TooSoon => "too_soon",
        AckStatus::NotConnected => "not_connected",
    }
}

fn wait_text(d: Duration) -> String {
    format!("{}s", d.as_secs() + 1)
}

impl AppState {
    pub fn new(handle: AppHandle, config_dir: PathBuf) -> Shared {
        let settings = Settings::load(&config_dir);
        Arc::new(AppState {
            handle,
            config_dir,
            sound: Sound::spawn(),
            inner: Mutex::new(Inner {
                settings,
                client: None,
                generation: 0,
                team_id: None,
                conn: ConnState::Offline { reason: "not set up yet".into(), retry_in_secs: 0 },
                peers: vec![],
                incoming: vec![],
                outgoing: VecDeque::new(),
                history: VecDeque::new(),
                last_poke: HashMap::new(),
                last_poke_all: None,
                locked: false,
                in_call: None,
                away: vec![],
                dnd_item: None,
                tray: None,
                update: None,
                update_progress: None,
                update_notified: None,
            }),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn attach_tray(&self, tray: TrayIcon<Wry>, dnd_item: CheckMenuItem<Wry>) {
        let dnd = {
            let mut g = self.lock();
            g.tray = Some(tray);
            g.dnd_item = Some(dnd_item.clone());
            g.settings.dnd
        };
        let _ = dnd_item.set_checked(dnd);
        self.refresh_tray();
    }

    pub fn is_configured(&self) -> bool {
        self.lock().settings.is_configured()
    }

    pub fn detect_calls(&self) -> bool {
        self.lock().settings.detect_calls
    }

    // ---------------------------------------------------------------- snapshot

    pub fn snapshot(&self) -> Snapshot {
        let g = self.lock();
        let s = &g.settings;
        let invite = if s.is_configured() { poke_core::make_invite(&s.relay_url, &s.team_code).ok() } else { None };
        let now = now_ms();
        let cooldowns = g
            .peers
            .iter()
            .filter_map(|p| g.cooldown_left(&p.id).map(|d| (p.id.clone(), now + d.as_millis() as u64)))
            .collect();
        Snapshot {
            configured: s.is_configured(),
            display_name: s.display_name.clone(),
            relay_url: s.relay_url.clone(),
            team_code: s.team_code.clone(),
            proxy: s.proxy.clone(),
            dnd: s.dnd,
            sound: s.sound,
            volume: s.volume,
            alert_timeout_secs: s.alert_timeout_secs,
            detect_calls: s.detect_calls,
            autostart: self.handle.autolaunch().is_enabled().unwrap_or(false),
            team_id: g.team_id.clone(),
            invite,
            conn: g.conn.clone(),
            peers: g.peers.clone(),
            outgoing: g.outgoing.iter().cloned().collect(),
            history: g.history.iter().cloned().collect(),
            missed: g.unread(),
            in_call: g.in_call.clone(),
            cooldowns,
            poke_all_until: g.poke_all_left().map(|d| now + d.as_millis() as u64).unwrap_or(0),
            version: env!("CARGO_PKG_VERSION"),
            update_version: g.update.as_ref().map(|u| u.0.clone()),
            update_notes: g.update.as_ref().and_then(|u| u.1.clone()),
            update_progress: g.update_progress.clone(),
            update_needs_password: crate::update::needs_password(),
        }
    }

    /// Record the result of an update check. Returns true the first time a
    /// given version is seen (so the caller can notify once).
    pub fn set_update(&self, update: Option<(String, Option<String>)>) -> bool {
        let mut g = self.lock();
        let new_version = update.as_ref().map(|u| u.0.clone());
        let first = new_version.is_some() && g.update_notified != new_version;
        if first {
            g.update_notified = new_version;
        }
        g.update = update;
        drop(g);
        self.publish();
        first
    }

    pub fn set_update_progress(&self, progress: Option<String>) {
        self.lock().update_progress = progress;
        self.publish();
    }

    /// Push a fresh snapshot to the main window.
    pub fn publish(&self) {
        let snap = self.snapshot();
        let _ = self.handle.emit_to("main", "snapshot", snap);
    }

    pub fn incoming(&self) -> Vec<Incoming> {
        self.lock().incoming.clone()
    }

    fn publish_incoming(&self) {
        let list = self.incoming();
        for (label, w) in self.handle.webview_windows() {
            if overlay::is_overlay(&label) {
                let _ = w.emit("incoming", list.clone());
            }
        }
    }

    fn refresh_tray(&self) {
        let g = self.lock();
        let status = if !g.settings.is_configured() {
            "not set up".to_string()
        } else if g.settings.dnd {
            "do not disturb".to_string()
        } else if g.in_call.is_some() {
            "in a call — pokes arrive quietly".to_string()
        } else {
            match &g.conn {
                ConnState::Online => format!("online · {} colleague(s)", g.peers.len()),
                ConnState::Connecting => "connecting…".to_string(),
                ConnState::Offline { .. } => "offline".to_string(),
            }
        };
        let unread = g.unread();
        let missed = if unread > 0 { format!(" · {unread} missed") } else { String::new() };
        // Tray calls block on the main thread: never make them while holding the lock.
        let tray = g.tray.clone();
        drop(g);
        if let Some(tray) = tray {
            let _ = tray.set_tooltip(Some(format!("Poke the Colleague — {status}{missed}")));
        }
    }

    // ---------------------------------------------------------------- client lifecycle

    /// (Re)start the relay client from the current settings.
    pub fn restart_client(self: &Arc<Self>) {
        let mut g = self.lock();
        g.generation += 1;
        if let Some(old) = g.client.take() {
            old.shutdown();
        }
        g.peers.clear();
        g.team_id = None;
        if !g.settings.is_configured() {
            g.conn = ConnState::Offline { reason: "not set up yet".into(), retry_in_secs: 0 };
            drop(g);
            self.refresh_tray();
            self.publish();
            return;
        }
        let s = &g.settings;
        let cfg = ClientConfig {
            relay_url: s.relay_url.clone(),
            team_code: s.team_code.clone(),
            client_id: s.client_id.clone(),
            profile: g.profile(),
            proxy: Some(s.proxy.clone()).filter(|p| !p.trim().is_empty()),
        };
        match Client::start_on(tauri::async_runtime::handle().inner(), cfg) {
            Ok((client, mut events)) => {
                g.team_id = Some(client.team_id().to_string());
                g.client = Some(client);
                g.conn = ConnState::Connecting;
                let gen = g.generation;
                drop(g);
                let me = Arc::clone(self);
                tauri::async_runtime::spawn(async move {
                    while let Some(ev) = events.recv().await {
                        if me.lock().generation != gen {
                            break;
                        }
                        me.on_event(ev);
                    }
                });
            }
            Err(e) => {
                warn!("cannot start client: {e}");
                g.conn = ConnState::Offline { reason: e.to_string(), retry_in_secs: 0 };
                drop(g);
            }
        }
        self.refresh_tray();
        self.publish();
    }

    pub fn reconnect_now(&self) {
        if let Some(c) = &self.lock().client {
            c.reconnect_now();
        }
    }

    pub fn shutdown(&self) {
        let mut g = self.lock();
        g.generation += 1;
        if let Some(c) = g.client.take() {
            c.shutdown();
        }
        self.sound.stop();
    }

    // ---------------------------------------------------------------- settings

    pub fn save_settings(
        self: &Arc<Self>,
        display_name: String,
        relay_url: String,
        team_code: String,
        proxy: String,
    ) -> Result<(), String> {
        let display_name = display_name.trim().to_string();
        if display_name.is_empty() {
            return Err("Please enter your name.".into());
        }
        let relay = poke_core::relay_display_url(&relay_url).map_err(|e| e.to_string())?;
        let code = poke_core::normalize_team_code(&team_code).map_err(|e| e.to_string())?;
        let first_setup;
        {
            let mut g = self.lock();
            first_setup = !g.settings.is_configured();
            g.settings.display_name = display_name.chars().take(poke_core::protocol::MAX_NAME_CHARS).collect();
            g.settings.relay_url = relay;
            g.settings.team_code = code;
            g.settings.proxy = proxy.trim().to_string();
            g.settings.save(&self.config_dir).map_err(|e| format!("Could not save settings: {e}"))?;
        }
        if first_setup {
            // Start with the session by default; can be switched off in Settings.
            let _ = self.handle.autolaunch().enable();
        }
        self.restart_client();
        Ok(())
    }

    pub fn set_prefs(
        &self,
        sound: bool,
        volume: f32,
        alert_timeout_secs: u32,
        autostart: bool,
        detect_calls: bool,
    ) -> Result<(), String> {
        {
            let mut g = self.lock();
            g.settings.sound = sound;
            g.settings.volume = volume.clamp(0.0, 1.0);
            g.settings.alert_timeout_secs = alert_timeout_secs.clamp(10, 600);
            g.settings.detect_calls = detect_calls;
            g.settings.save(&self.config_dir).map_err(|e| e.to_string())?;
        }
        let al = self.handle.autolaunch();
        let res = if autostart { al.enable() } else { al.disable() };
        if let Err(e) = res {
            warn!("autostart change failed: {e}");
        }
        self.publish();
        Ok(())
    }

    pub fn set_dnd(&self, dnd: bool) {
        let mut g = self.lock();
        if g.settings.dnd == dnd {
            return;
        }
        g.settings.dnd = dnd;
        let _ = g.settings.save(&self.config_dir);
        if let Some(c) = &g.client {
            c.set_profile(g.profile());
        }
        let item = g.dnd_item.clone();
        drop(g);
        if let Some(item) = item {
            let _ = item.set_checked(dnd);
        }
        self.refresh_tray();
        self.publish();
    }

    pub fn toggle_dnd(&self) {
        let dnd = self.lock().settings.dnd;
        self.set_dnd(!dnd);
    }

    /// Mark every received poke as read.
    pub fn clear_missed(&self) {
        for h in self.lock().history.iter_mut() {
            h.unread = false;
        }
        self.refresh_tray();
        self.publish();
    }

    // ---------------------------------------------------------------- automatic presence

    /// Called by the watcher thread every couple of seconds.
    pub fn on_local_presence(self: &Arc<Self>, locked: bool, in_call: Option<String>) {
        let mut g = self.lock();
        if g.locked == locked && g.in_call == in_call {
            return;
        }
        let (was_locked, was_in_call) = (g.locked, g.in_call.is_some());
        g.locked = locked;
        g.in_call = in_call.clone();
        if let Some(c) = &g.client {
            c.set_profile(g.profile());
        }

        // Locking with an alert on screen: nobody will see it, so tell the
        // callers and keep it for later instead of letting it time out.
        let mut hid_alerts = false;
        if locked && !was_locked && !g.incoming.is_empty() {
            let items: Vec<Incoming> = g.incoming.drain(..).collect();
            for item in items.into_iter().filter(|i| !i.is_test()) {
                if let Some(c) = &g.client {
                    c.reply(&item.from, &item.pid, ReplyKind::Locked);
                }
                g.away.push(item.pid.clone());
                push_history(&mut g, &item, "locked", true);
            }
            hid_alerts = true;
        }

        // Back at the desk (unlocked, or call over while unlocked): show what we missed.
        let back = (was_locked && !locked) || (was_in_call && in_call.is_none() && !locked);
        let announce: Vec<HistoryItem> = if back && !g.away.is_empty() {
            let away = std::mem::take(&mut g.away);
            g.history.iter().filter(|h| h.unread && away.contains(&h.pid)).cloned().collect()
        } else {
            vec![]
        };
        drop(g);
        info!("local presence: locked={locked} in_call={in_call:?}");

        if hid_alerts {
            self.sound.stop();
            overlay::close_all(&self.handle);
        }
        if !announce.is_empty() {
            self.announce_missed(&announce, was_locked);
        }
        self.refresh_tray();
        self.publish();
    }

    fn announce_missed(&self, items: &[HistoryItem], was_locked: bool) {
        let mut names: Vec<&str> = items.iter().map(|h| h.from_name.as_str()).collect();
        names.dedup();
        let who = match names.len() {
            1 => names[0].to_string(),
            2 => format!("{} and {}", names[0], names[1]),
            n => format!("{}, {} and {} more", names[0], names[1], n - 2),
        };
        let when = if was_locked { "while you were away" } else { "during your call" };
        let _ = self
            .handle
            .notification()
            .builder()
            .title(format!("{who} poked you {when}"))
            .body("Open Poke → Activity to answer.")
            .show();
        crate::show_main(&self.handle);
        let _ = self.handle.emit_to("main", "show-activity", ());
    }

    // ---------------------------------------------------------------- outgoing

    pub fn poke(&self, to: String, message: Option<String>) -> Result<String, String> {
        let mut g = self.lock();
        if g.client.is_none() {
            return Err("Not connected — finish setup first.".into());
        }
        let to_name = g.peer_name(&to);
        if let Some(left) = g.cooldown_left(&to) {
            return Err(format!("You poked {to_name} less than a minute ago — try again in {}.", wait_text(left)));
        }
        let message = message.map(|m| m.trim().to_string()).filter(|m| !m.is_empty());
        let pid = g.client.as_ref().expect("checked").poke(&to, message.as_deref());
        g.last_poke.insert(to.clone(), Instant::now());
        g.outgoing.push_front(Outgoing {
            pid: pid.clone(),
            to,
            to_name,
            message,
            sent_at: now_ms(),
            status: "sending".into(),
            all: false,
        });
        g.outgoing.truncate(MAX_OUTGOING);
        drop(g);
        self.publish();
        Ok(pid)
    }

    /// Poke everybody online with one message (e.g. "Join the meeting").
    pub fn poke_all(&self, message: Option<String>) -> Result<usize, String> {
        let mut g = self.lock();
        if g.client.is_none() {
            return Err("Not connected — finish setup first.".into());
        }
        if let Some(left) = g.poke_all_left() {
            return Err(format!("You poked everyone less than a minute ago — try again in {}.", wait_text(left)));
        }
        let peers: Vec<Peer> = g.peers.clone();
        if peers.is_empty() {
            return Err("Nobody else is online right now.".into());
        }
        let message = message.map(|m| m.trim().to_string()).filter(|m| !m.is_empty());
        let pid = g.client.as_ref().expect("checked").poke_all(message.as_deref());
        g.last_poke_all = Some(Instant::now());
        let sent_at = now_ms();
        for p in &peers {
            // People poked individually moments ago are skipped by the relay.
            let status = if g.last_poke.get(&p.id).is_some_and(|t| t.elapsed() < POKE_COOLDOWN) { "too_soon" } else { "sending" };
            g.outgoing.push_front(Outgoing {
                pid: pid.clone(),
                to: p.id.clone(),
                to_name: p.name.clone(),
                message: message.clone(),
                sent_at,
                status: status.into(),
                all: true,
            });
        }
        g.outgoing.truncate(MAX_OUTGOING);
        drop(g);
        self.publish();
        Ok(peers.len())
    }

    // ---------------------------------------------------------------- incoming

    /// Answer an incoming poke from the overlay.
    pub fn respond(&self, pid: &str, kind: ReplyKind) {
        let mut g = self.lock();
        let Some(idx) = g.incoming.iter().position(|i| i.pid == pid) else { return };
        let item = g.incoming.remove(idx);
        if let (Some(c), false) = (&g.client, item.is_test()) {
            c.reply(&item.from, &item.pid, kind);
        }
        push_history(&mut g, &item, reply_str(kind), false);
        let empty = g.incoming.is_empty();
        drop(g);
        self.after_incoming_change(empty);
    }

    pub fn respond_all(&self, kind: ReplyKind) {
        let pids: Vec<String> = self.lock().incoming.iter().map(|i| i.pid.clone()).collect();
        for pid in pids {
            self.respond(&pid, kind);
        }
    }

    fn expire(&self, pid: &str) {
        let mut g = self.lock();
        let Some(idx) = g.incoming.iter().position(|i| i.pid == pid) else { return };
        let item = g.incoming.remove(idx);
        if !item.is_test() {
            if let Some(c) = &g.client {
                c.reply(&item.from, &item.pid, ReplyKind::Missed);
            }
        }
        push_history(&mut g, &item, "missed", true);
        let empty = g.incoming.is_empty();
        drop(g);
        self.refresh_tray();
        self.after_incoming_change(empty);
    }

    fn after_incoming_change(&self, empty: bool) {
        if empty {
            self.sound.stop();
            overlay::close_all(&self.handle);
        } else {
            self.publish_incoming();
        }
        self.publish();
    }

    /// Show a local fake poke so people can check the overlay/sound on their machine.
    pub fn test_alert(self: &Arc<Self>) {
        let item = Incoming {
            pid: format!("test-{}", uuid::Uuid::new_v4().simple()),
            from: TEST_SENDER.into(),
            from_name: "Test alert".into(),
            message: Some("This is what a poke looks like. Press Esc or click a button to close it.".into()),
            received_at: now_ms(),
            all: false,
        };
        self.show_incoming(item);
    }

    fn show_incoming(self: &Arc<Self>, item: Incoming) {
        let (volume, sound, timeout) = {
            let mut g = self.lock();
            g.incoming.push(item.clone());
            (g.settings.volume, g.settings.sound, g.settings.alert_timeout_secs)
        };
        if sound {
            self.sound.play(volume);
        }
        overlay::show(&self.handle);
        self.publish_incoming();

        // Backup channel: on Linux the desktop notification also shows if the
        // compositor keeps the overlay behind another window.
        #[cfg(target_os = "linux")]
        {
            let body = item.message.clone().unwrap_or_else(|| "is calling you".into());
            let _ = self.handle.notification().builder().title(format!("👋 {}", item.from_name)).body(body).show();
        }

        let me = Arc::clone(self);
        let pid = item.pid.clone();
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(Duration::from_secs(timeout as u64)).await;
            me.expire(&pid);
        });
    }

    fn on_event(self: &Arc<Self>, ev: Event) {
        match ev {
            Event::State { state } => {
                if matches!(state, ConnState::Online) {
                    info!("connected to relay");
                }
                self.lock().conn = state;
                self.refresh_tray();
                self.publish();
            }
            Event::Peers { peers } => {
                self.lock().peers = peers;
                self.refresh_tray();
                self.publish();
            }
            Event::Poke { from, from_name, pid, message, all, .. } => {
                self.on_poke(Incoming { pid, from, from_name, message, received_at: now_ms(), all });
            }
            Event::Ack { pid, to, status } => {
                let mut g = self.lock();
                for o in g.outgoing.iter_mut().filter(|o| o.pid == pid && (to == "*" || o.to == to)) {
                    if o.status == "sending" {
                        o.status = ack_str(status).into();
                    }
                }
                drop(g);
                self.publish();
            }
            Event::Reply { pid, from, from_name, kind } => {
                let mut g = self.lock();
                let mut known = false;
                let mut all = false;
                if let Some(o) = g.outgoing.iter_mut().find(|o| o.pid == pid && o.to == from) {
                    known = true;
                    all = o.all;
                    // "seen" must never overwrite a real answer that raced ahead of it.
                    if kind != ReplyKind::Seen || matches!(o.status.as_str(), "sending" | "delivered") {
                        o.status = reply_str(kind).into();
                    }
                }
                drop(g);
                if known {
                    self.notify_reply(&from_name, kind, all);
                    self.publish();
                }
            }
        }
    }

    fn on_poke(self: &Arc<Self>, item: Incoming) {
        enum Mode {
            Alert,
            Locked,
            Dnd,
            InCall,
        }
        let (mode, volume, sound) = {
            let mut g = self.lock();
            let mode = if g.locked {
                Mode::Locked
            } else if g.settings.dnd {
                Mode::Dnd
            } else if g.in_call.is_some() {
                Mode::InCall
            } else {
                Mode::Alert
            };
            let (reply, outcome) = match mode {
                Mode::Alert => (ReplyKind::Seen, None),
                Mode::Locked => (ReplyKind::Locked, Some("locked")),
                Mode::Dnd => (ReplyKind::Dnd, Some("dnd")),
                Mode::InCall => (ReplyKind::InCall, Some("in_call")),
            };
            if let Some(c) = &g.client {
                c.reply(&item.from, &item.pid, reply);
            }
            if let Some(outcome) = outcome {
                push_history(&mut g, &item, outcome, true);
                if matches!(mode, Mode::Locked | Mode::InCall) {
                    g.away.push(item.pid.clone());
                }
            }
            (mode, g.settings.volume, g.settings.sound)
        };
        match mode {
            Mode::Alert => self.show_incoming(item),
            Mode::InCall => {
                // Quiet: no full-screen (could be on a screen share), no ring over the call.
                let body = item.message.clone().unwrap_or_else(|| "You're in a call — answer when you're done.".into());
                let _ = self
                    .handle
                    .notification()
                    .builder()
                    .title(format!("👋 {} poked you", item.from_name))
                    .body(body)
                    .show();
                if sound {
                    self.sound.chime(volume * 0.6);
                }
                self.refresh_tray();
                self.publish();
            }
            Mode::Locked | Mode::Dnd => {
                self.refresh_tray();
                self.publish();
            }
        }
    }

    fn notify_reply(&self, from_name: &str, kind: ReplyKind, all: bool) {
        let text = match kind {
            ReplyKind::Seen | ReplyKind::Dismissed | ReplyKind::Other => return,
            // For "poke everyone" only real answers notify, to avoid a burst of toasts.
            ReplyKind::Dnd | ReplyKind::Missed | ReplyKind::InCall | ReplyKind::Locked if all => return,
            ReplyKind::Coming => "is on the way 🏃",
            ReplyKind::FiveMin => "will come in 5 minutes ⏳",
            ReplyKind::Busy => "is busy right now ⛔",
            ReplyKind::Dnd => "is in do-not-disturb — they'll see it later 🔕",
            ReplyKind::Missed => "didn't respond to the alert",
            ReplyKind::InCall => "is in a call — they got a quiet notification 📞",
            ReplyKind::Locked => "is away (screen locked) — they'll see it when they're back 🔒",
        };
        let main_focused = self
            .handle
            .get_webview_window("main")
            .and_then(|w| Some(w.is_visible().ok()? && w.is_focused().ok()?))
            .unwrap_or(false);
        if !main_focused {
            let _ = self.handle.notification().builder().title(format!("{from_name} {text}")).show();
        }
    }
}

impl Incoming {
    fn is_test(&self) -> bool {
        self.from == TEST_SENDER
    }
}

fn push_history(g: &mut Inner, item: &Incoming, outcome: &str, unread: bool) {
    if item.is_test() {
        return;
    }
    g.history.push_front(HistoryItem {
        pid: item.pid.clone(),
        from: item.from.clone(),
        from_name: item.from_name.clone(),
        message: item.message.clone(),
        at: item.received_at,
        outcome: outcome.into(),
        all: item.all,
        unread,
    });
    g.history.truncate(MAX_HISTORY);
}
