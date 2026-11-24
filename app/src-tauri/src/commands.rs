//! Commands invoked from the HTML UI via `window.__TAURI__.core.invoke`.
//!
//! Commands that create or destroy windows are `async` so they don't run on the
//! main thread (avoids a known deadlock on Windows).

use poke_core::ReplyKind;
use serde::Serialize;
use tauri::{AppHandle, State, WebviewWindow};

use crate::settings::QuickReply;
use crate::state::{Incoming, Shared, Snapshot};

type St<'a> = State<'a, Shared>;

#[tauri::command]
pub fn get_snapshot(state: St) -> Snapshot {
    state.snapshot()
}

#[tauri::command]
pub fn save_settings(
    state: St,
    display_name: String,
    relay_url: String,
    team_code: String,
    proxy: String,
) -> Result<Snapshot, String> {
    state.save_settings(display_name, relay_url, team_code, proxy)?;
    Ok(state.snapshot())
}

#[tauri::command]
pub fn generate_team_code() -> String {
    poke_core::generate_team_code()
}

#[derive(Serialize)]
pub struct InviteParts {
    relay_url: String,
    team_code: String,
}

#[tauri::command]
pub fn parse_invite(invite: String) -> Result<InviteParts, String> {
    let (relay_url, team_code) = poke_core::parse_invite(&invite).map_err(|e| e.to_string())?;
    Ok(InviteParts { relay_url, team_code })
}

#[tauri::command]
pub fn poke(state: St, to: String, message: Option<String>) -> Result<String, String> {
    state.poke(to, message)
}

/// Returns how many colleagues were poked.
#[tauri::command]
pub fn poke_all(state: St, message: Option<String>) -> Result<usize, String> {
    state.poke_all(message)
}

#[tauri::command]
pub fn set_dnd(state: St, dnd: bool) {
    state.set_dnd(dnd);
}

#[tauri::command]
pub fn set_prefs(
    state: St,
    sound: bool,
    volume: f32,
    alert_timeout_secs: u32,
    autostart: bool,
    detect_calls: bool,
) -> Result<(), String> {
    state.set_prefs(sound, volume, alert_timeout_secs, autostart, detect_calls)
}

#[tauri::command]
pub fn set_replies(state: St, replies: Vec<QuickReply>) -> Result<(), String> {
    state.set_replies(replies)
}

#[tauri::command]
pub fn set_quick_messages(state: St, messages: Vec<String>) -> Result<(), String> {
    state.set_quick_messages(messages)
}

/// Answer buttons for the full-screen alert.
#[tauri::command]
pub fn get_replies(state: St) -> Vec<QuickReply> {
    state.replies()
}

#[tauri::command]
pub fn clear_missed(state: St) {
    state.clear_missed();
}

#[tauri::command]
pub fn reconnect(state: St) {
    state.reconnect_now();
}

#[tauri::command]
pub async fn test_alert(state: St<'_>) -> Result<(), String> {
    state.inner().test_alert();
    Ok(())
}

#[tauri::command]
pub fn get_incoming(state: St) -> Vec<Incoming> {
    state.incoming()
}

#[tauri::command]
pub async fn respond(state: St<'_>, pid: String, kind: ReplyKind, label: Option<String>) -> Result<(), String> {
    state.respond(&pid, kind, label);
    Ok(())
}

#[tauri::command]
pub async fn respond_all(state: St<'_>, kind: ReplyKind) -> Result<(), String> {
    state.respond_all(kind);
    Ok(())
}

/// Returns the newer version, or `None` when up to date.
#[tauri::command]
pub async fn check_update(app: AppHandle, state: St<'_>) -> Result<Option<String>, String> {
    crate::update::check(&app, state.inner()).await
}

#[tauri::command]
pub async fn install_update(app: AppHandle, state: St<'_>) -> Result<(), String> {
    crate::update::install(&app, state.inner()).await
}

#[tauri::command]
pub fn hide_window(window: WebviewWindow) {
    let _ = window.hide();
}

#[tauri::command]
pub fn quit(state: St, app: AppHandle) {
    state.shutdown();
    app.exit(0);
}
