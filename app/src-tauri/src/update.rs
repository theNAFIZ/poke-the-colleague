//! In-app updates from GitHub Releases (`latest.json`, published by CI).
//!
//! Every update is signed in CI; the app only installs files whose signature
//! matches the public key in `tauri.conf.json`.
//!
//! * Windows — the NSIS installer runs per-user (`passive`, no admin prompt).
//! * AppImage — replaced in place.
//! * .deb — installed through the graphical password prompt (pkexec).

use std::time::Duration;

use tauri::AppHandle;
use tauri_plugin_notification::NotificationExt as _;
use tauri_plugin_updater::{Updater, UpdaterExt as _};
use tracing::{info, warn};

use crate::state::Shared;

const FIRST_CHECK_AFTER: Duration = Duration::from_secs(20);
const CHECK_EVERY: Duration = Duration::from_secs(6 * 3600);

/// The updater, optionally pointed at `POKE_UPDATE_URL` (for testing against a
/// local latest.json; release builds still require https).
fn updater(app: &AppHandle) -> Result<Updater, String> {
    let mut builder = app.updater_builder();
    if let Some(url) = std::env::var("POKE_UPDATE_URL").ok().filter(|u| !u.is_empty()) {
        let url = url.parse().map_err(|e| format!("POKE_UPDATE_URL: {e}"))?;
        builder = builder.endpoints(vec![url]).map_err(|e| e.to_string())?;
    }
    builder.build().map_err(|e| e.to_string())
}

/// Whether installing needs the user's password (Linux .deb).
pub fn needs_password() -> bool {
    matches!(tauri::utils::platform::bundle_type(), Some(tauri::utils::config::BundleType::Deb))
}

/// Background check at startup and every few hours.
pub fn spawn_checker(app: AppHandle, state: Shared) {
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(FIRST_CHECK_AFTER).await;
        loop {
            if let Err(e) = check(&app, &state).await {
                warn!("update check failed: {e}");
            }
            tokio::time::sleep(CHECK_EVERY).await;
        }
    });
}

/// Returns the new version, if there is one.
pub async fn check(app: &AppHandle, state: &Shared) -> Result<Option<String>, String> {
    let update = updater(app)?.check().await.map_err(|e| e.to_string())?;
    let Some(update) = update else {
        state.set_update(None);
        return Ok(None);
    };
    info!("update available: {}", update.version);
    if state.set_update(Some((update.version.clone(), update.body.clone()))) {
        // First time we see this version: tell people whose window is hidden.
        let _ = app
            .notification()
            .builder()
            .title(format!("Poke the Colleague {} is available", update.version))
            .body("Open Poke and click “Update & restart”.")
            .show();
    }
    Ok(Some(update.version))
}

/// Download, verify, install and restart.
pub async fn install(app: &AppHandle, state: &Shared) -> Result<(), String> {
    let updater = updater(app)?;
    let Some(update) = updater.check().await.map_err(|e| e.to_string())? else {
        state.set_update(None);
        return Err("You already have the latest version.".into());
    };
    state.set_update_progress(Some("Downloading…".into()));
    let mut done: usize = 0;
    let mut last_pct: u64 = 0;
    let progress_state = state.clone();
    let finished_state = state.clone();
    let result = update
        .download_and_install(
            |chunk, total| {
                done += chunk;
                if let Some(total) = total.filter(|t| *t > 0) {
                    let pct = (done as u64 * 100 / total).min(100);
                    if pct >= last_pct + 5 {
                        last_pct = pct;
                        progress_state.set_update_progress(Some(format!("Downloading… {pct}%")));
                    }
                }
            },
            move || {
                let msg = if needs_password() { "Installing… enter your password if asked" } else { "Installing…" };
                finished_state.set_update_progress(Some(msg.into()));
            },
        )
        .await;
    match result {
        Ok(()) => {
            info!("update installed, restarting");
            state.shutdown();
            app.restart();
        }
        Err(e) => {
            state.set_update_progress(None);
            Err(format!("Update failed: {e}"))
        }
    }
}
