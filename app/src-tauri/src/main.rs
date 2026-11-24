// No console window on Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;
mod overlay;
mod settings;
mod sound;
mod state;
mod update;
mod watch;

use std::time::{Duration, SystemTime};

use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, RunEvent, WindowEvent};
use tauri_plugin_autostart::MacosLauncher;

use state::{AppState, Shared};

/// Passed by the autostart entry so login starts silently in the tray.
const HIDDEN_ARG: &str = "--hidden";

fn main() {
    #[cfg(target_os = "linux")]
    linux_env();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("POKE_LOG").unwrap_or_else(|_| "info".into()),
        )
        .init();

    let start_hidden = std::env::args().any(|a| a == HIDDEN_ARG);
    // TLS crypto provider for both the relay client and the updater.
    poke_core::init_tls();

    tauri::Builder::default()
        // Must be first: a second launch just brings the existing window forward.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| show_main(app)))
        .plugin(tauri_plugin_autostart::init(MacosLauncher::LaunchAgent, Some(vec![HIDDEN_ARG])))
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .invoke_handler(tauri::generate_handler![
            commands::get_snapshot,
            commands::save_settings,
            commands::generate_team_code,
            commands::parse_invite,
            commands::poke,
            commands::poke_all,
            commands::set_dnd,
            commands::set_prefs,
            commands::clear_missed,
            commands::reconnect,
            commands::test_alert,
            commands::get_incoming,
            commands::respond,
            commands::respond_all,
            commands::set_replies,
            commands::set_quick_messages,
            commands::get_replies,
            commands::check_update,
            commands::install_update,
            commands::hide_window,
            commands::quit,
        ])
        .setup(move |app| {
            let config_dir = app.path().app_config_dir()?;
            let state = AppState::new(app.handle().clone(), config_dir);
            app.manage(state.clone());

            build_tray(app.handle(), &state)?;
            state.restart_client();
            watch_for_resume(state.clone());
            watch::spawn(state.clone());
            update::spawn_checker(app.handle().clone(), state.clone());

            if !start_hidden || !state.is_configured() {
                show_main(app.handle());
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            // Closing the main window only hides it; the app lives in the tray.
            if window.label() == "main" {
                if let WindowEvent::CloseRequested { api, .. } = event {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        .build(tauri::generate_context!())
        .expect("failed to build the application")
        .run(|_app, event| {
            // Keep running when the last overlay closes; only an explicit Quit exits.
            if let RunEvent::ExitRequested { api, code: None, .. } = event {
                api.prevent_exit();
            }
        });
}

pub fn show_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
}

fn build_tray(app: &AppHandle, state: &Shared) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "Open Poke", true, None::<&str>)?;
    let dnd = CheckMenuItem::with_id(app, "dnd", "Do not disturb", true, false, None::<&str>)?;
    let test = MenuItem::with_id(app, "test", "Show test alert", true, None::<&str>)?;
    let updates = MenuItem::with_id(app, "updates", "Check for updates", true, None::<&str>)?;
    let sep = PredefinedMenuItem::separator(app)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open, &dnd, &test, &updates, &sep, &quit])?;

    let mut builder = TrayIconBuilder::with_id("main")
        .tooltip("Poke the Colleague")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| {
            // Runs on the main thread: hand state work to the async runtime so
            // the main thread never waits on the state lock.
            let state = app.state::<Shared>().inner().clone();
            match event.id.as_ref() {
                "open" => show_main(app),
                "dnd" => {
                    tauri::async_runtime::spawn(async move { state.toggle_dnd() });
                }
                "test" => {
                    tauri::async_runtime::spawn(async move { state.test_alert() });
                }
                "updates" => {
                    let app = app.clone();
                    tauri::async_runtime::spawn(async move {
                        show_main(&app);
                        match update::check(&app, &state).await {
                            Ok(Some(_)) => {} // banner appears in the main window
                            Ok(None) => {
                                let _ = app.emit_to("main", "toast", "You have the latest version.");
                            }
                            Err(e) => {
                                let _ = app.emit_to("main", "toast", format!("Update check failed: {e}"));
                            }
                        }
                    });
                }
                "quit" => {
                    let app = app.clone();
                    tauri::async_runtime::spawn(async move {
                        state.shutdown();
                        app.exit(0);
                    });
                }
                _ => {}
            }
        })
        .on_tray_icon_event(|tray, event| {
            // Not emitted on Linux (AppIndicator); there the menu has "Open Poke".
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
                show_main(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    let tray = builder.build(app)?;
    state.attach_tray(tray, dnd);
    Ok(())
}

/// After suspend/resume the TCP connection is usually dead but looks alive.
/// Wall-clock time jumps while the monotonic sleep doesn't, so detect that
/// and reconnect immediately instead of waiting for the keep-alive timeout.
fn watch_for_resume(state: Shared) {
    tauri::async_runtime::spawn(async move {
        let tick = Duration::from_secs(10);
        let mut last = SystemTime::now();
        loop {
            tokio::time::sleep(tick).await;
            let now = SystemTime::now();
            let elapsed = now.duration_since(last).unwrap_or_default();
            last = now;
            if elapsed > tick + Duration::from_secs(20) {
                tracing::info!("resume detected ({}s gap), reconnecting", elapsed.as_secs());
                state.reconnect_now();
            }
        }
    });
}

/// GNOME on Wayland does not let apps keep a window on top. Running through
/// XWayland restores that. Set POKE_NATIVE_WAYLAND=1 to opt out.
#[cfg(target_os = "linux")]
fn linux_env() {
    // Launched from a terminal of a snap app (e.g. VS Code)? Its GTK/GIO module
    // paths leak in and crash WebKitGTK with glibc symbol errors. Undo that.
    scrub_snap_env();

    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    if wayland && std::env::var_os("GDK_BACKEND").is_none() && std::env::var_os("POKE_NATIVE_WAYLAND").is_none() {
        std::env::set_var("GDK_BACKEND", "x11");
    }
    // Avoids blank WebKitGTK windows on some GPU/driver combinations.
    if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none() {
        std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
    }
}

/// Remove module/library search paths that point into a snap's runtime.
///
/// Decided by the variables' *values*, not by `$SNAP`: some terminals keep
/// `GTK_PATH` & co. from the snap that launched them but drop `$SNAP` itself.
#[cfg(target_os = "linux")]
fn scrub_snap_env() {
    use std::ffi::OsString;

    let exe = std::env::current_exe().unwrap_or_default();
    if exe.starts_with("/snap/") {
        return; // we are the snap; its paths are ours
    }
    let snappy = |v: &OsString| v.to_string_lossy().contains("/snap/");
    let filter_list = |v: &OsString| -> String {
        v.to_string_lossy().split(':').filter(|p| !p.is_empty() && !p.contains("/snap/")).collect::<Vec<_>>().join(":")
    };

    for var in [
        "GTK_PATH",
        "GTK_EXE_PREFIX",
        "GTK_IM_MODULE_FILE",
        "GTK_MODULES",
        "GDK_PIXBUF_MODULE_FILE",
        "GDK_PIXBUF_MODULEDIR",
        "GIO_MODULE_DIR",
        "GIO_EXTRA_MODULES",
        "GSETTINGS_SCHEMA_DIR",
        "GST_PLUGIN_PATH",
        "GST_PLUGIN_SYSTEM_PATH",
        "LOCPATH",
    ] {
        if std::env::var_os(var).is_some_and(|v| snappy(&v)) {
            std::env::remove_var(var);
        }
    }
    for var in ["XDG_DATA_DIRS", "XDG_CONFIG_DIRS"] {
        let orig = std::env::var_os(format!("{var}_VSCODE_SNAP_ORIG")).filter(|o| !o.is_empty());
        if let Some(orig) = orig {
            std::env::set_var(var, orig);
        } else if let Some(v) = std::env::var_os(var).filter(|v| snappy(v)) {
            std::env::set_var(var, filter_list(&v));
        }
    }
}
