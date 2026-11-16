//! Application state and the glue between the relay client, the UI windows,
//! the full-screen overlay and the sound.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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

/// Minimum gap between two pokes to the same colleague.
const POKE_COOLDOWN: Duration = Duration::from_secs(10);
const MAX_HISTORY: usize = 50;
const MAX_OUTGOING: usize = 20;
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
}

#[derive(Debug, Clone, Serialize)]
pub struct Outgoing {
    pub pid: String,
    pub to: String,
    pub to_name: String,
    pub message: Option<String>,
    pub sent_at: u64,
    /// sending | delivered | offline | rate_limited | not_connected | seen | dnd | missed | coming | five_min | busy | dismissed
    pub status: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct HistoryItem {
    pub pid: String,
    pub from_name: String,
    pub message: Option<String>,
    pub at: u64,
    /// coming | five_min | busy | dismissed | missed | dnd
    pub outcome: String,
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
    pub autostart: bool,
    pub team_id: Option<String>,
    pub invite: Option<String>,
    pub conn: ConnState,
    pub peers: Vec<Peer>,
    pub outgoing: Vec<Outgoing>,
    pub history: Vec<HistoryItem>,
    pub missed: u32,
    pub version: &'static str,
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
    missed: u32,
    last_poke: HashMap<String, Instant>,
    dnd_item: Option<CheckMenuItem<Wry>>,
    tray: Option<TrayIcon<Wry>>,
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
        ReplyKind::Coming => "coming",
        ReplyKind::FiveMin => "five_min",
        ReplyKind::Busy => "busy",
        ReplyKind::Dismissed => "dismissed",
    }
}

fn ack_str(s: AckStatus) -> &'static str {
    match s {
        AckStatus::Delivered => "delivered",
        AckStatus::Offline => "offline",
        AckStatus::RateLimited => "rate_limited",
        AckStatus::NotConnected => "not_connected",
    }
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
                missed: 0,
                last_poke: HashMap::new(),
                dnd_item: None,
                tray: None,
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

    // ---------------------------------------------------------------- snapshot

    pub fn snapshot(&self) -> Snapshot {
        let g = self.lock();
        let s = &g.settings;
        let invite = if s.is_configured() { poke_core::make_invite(&s.relay_url, &s.team_code).ok() } else { None };
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
            autostart: self.handle.autolaunch().is_enabled().unwrap_or(false),
            team_id: g.team_id.clone(),
            invite,
            conn: g.conn.clone(),
            peers: g.peers.clone(),
            outgoing: g.outgoing.iter().cloned().collect(),
            history: g.history.iter().cloned().collect(),
            missed: g.missed,
            version: env!("CARGO_PKG_VERSION"),
        }
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
        } else {
            match &g.conn {
                ConnState::Online => format!("online · {} colleague(s)", g.peers.len()),
                ConnState::Connecting => "connecting…".to_string(),
                ConnState::Offline { .. } => "offline".to_string(),
            }
        };
        let missed = if g.missed > 0 { format!(" · {} missed", g.missed) } else { String::new() };
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
            profile: Profile {
                name: s.display_name.clone(),
                presence: if s.dnd { Presence::Dnd } else { Presence::Available },
            },
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

    pub fn set_prefs(&self, sound: bool, volume: f32, alert_timeout_secs: u32, autostart: bool) -> Result<(), String> {
        {
            let mut g = self.lock();
            g.settings.sound = sound;
            g.settings.volume = volume.clamp(0.0, 1.0);
            g.settings.alert_timeout_secs = alert_timeout_secs.clamp(10, 600);
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
        if !dnd {
            g.missed = 0;
        }
        let _ = g.settings.save(&self.config_dir);
        if let Some(c) = &g.client {
            c.set_profile(Profile {
                name: g.settings.display_name.clone(),
                presence: if dnd { Presence::Dnd } else { Presence::Available },
            });
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

    pub fn clear_missed(&self) {
        self.lock().missed = 0;
        self.refresh_tray();
        self.publish();
    }

    // ---------------------------------------------------------------- outgoing

    pub fn poke(&self, to: String, message: Option<String>) -> Result<String, String> {
        let mut g = self.lock();
        let Some(client) = &g.client else {
            return Err("Not connected — finish setup first.".into());
        };
        if let Some(t) = g.last_poke.get(&to) {
            if t.elapsed() < POKE_COOLDOWN {
                let wait = (POKE_COOLDOWN - t.elapsed()).as_secs() + 1;
                return Err(format!("You just poked them — wait {wait}s."));
            }
        }
        let message = message.map(|m| m.trim().to_string()).filter(|m| !m.is_empty());
        let pid = client.poke(&to, message.as_deref());
        let to_name = g.peers.iter().find(|p| p.id == to).map(|p| p.name.clone()).unwrap_or_else(|| "Colleague".into());
        g.last_poke.insert(to.clone(), Instant::now());
        g.outgoing.push_front(Outgoing { pid: pid.clone(), to, to_name, message, sent_at: now_ms(), status: "sending".into() });
        g.outgoing.truncate(MAX_OUTGOING);
        drop(g);
        self.publish();
        Ok(pid)
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
        push_history(&mut g, &item, reply_str(kind));
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
            g.missed += 1;
        }
        push_history(&mut g, &item, "missed");
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
            Event::Poke { from, from_name, pid, message, .. } => {
                let item = Incoming { pid, from, from_name, message, received_at: now_ms() };
                let dnd = {
                    let mut g = self.lock();
                    let dnd = g.settings.dnd;
                    if let Some(c) = &g.client {
                        c.reply(&item.from, &item.pid, if dnd { ReplyKind::Dnd } else { ReplyKind::Seen });
                    }
                    if dnd {
                        push_history(&mut g, &item, "dnd");
                        g.missed += 1;
                    }
                    dnd
                };
                if dnd {
                    self.refresh_tray();
                    self.publish();
                } else {
                    self.show_incoming(item);
                }
            }
            Event::Ack { pid, status, .. } => {
                let mut g = self.lock();
                if let Some(o) = g.outgoing.iter_mut().find(|o| o.pid == pid) {
                    if o.status == "sending" {
                        o.status = ack_str(status).into();
                    }
                }
                drop(g);
                self.publish();
            }
            Event::Reply { pid, from_name, kind, .. } => {
                let mut g = self.lock();
                let known = g.outgoing.iter_mut().find(|o| o.pid == pid).map(|o| {
                    // "seen" must never overwrite a real answer that raced ahead of it.
                    if kind != ReplyKind::Seen || matches!(o.status.as_str(), "sending" | "delivered") {
                        o.status = reply_str(kind).into();
                    }
                });
                drop(g);
                if known.is_some() {
                    self.notify_reply(&from_name, kind);
                    self.publish();
                }
            }
        }
    }

    fn notify_reply(&self, from_name: &str, kind: ReplyKind) {
        let text = match kind {
            ReplyKind::Seen | ReplyKind::Dismissed => return,
            ReplyKind::Coming => "is on the way 🏃",
            ReplyKind::FiveMin => "will come in 5 minutes ⏳",
            ReplyKind::Busy => "is busy right now ⛔",
            ReplyKind::Dnd => "is in do-not-disturb — they'll see it later 🔕",
            ReplyKind::Missed => "didn't respond to the alert",
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

fn push_history(g: &mut Inner, item: &Incoming, outcome: &str) {
    if item.is_test() {
        return;
    }
    g.history.push_front(HistoryItem {
        pid: item.pid.clone(),
        from_name: item.from_name.clone(),
        message: item.message.clone(),
        at: item.received_at,
        outcome: outcome.into(),
    });
    g.history.truncate(MAX_HISTORY);
}
