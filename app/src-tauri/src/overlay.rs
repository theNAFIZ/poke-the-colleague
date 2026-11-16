//! Full-screen, always-on-top alert windows — one per monitor.
//!
//! * Windows: topmost borderless full-screen window.
//! * Linux/X11: `_NET_WM_STATE_ABOVE` + full-screen.
//! * Linux/GNOME Wayland: the app runs through XWayland (see `main.rs`), where
//!   Mutter honours "always on top" for X11 clients. A desktop notification is
//!   sent as well, as a fallback.

use tauri::{AppHandle, Manager, Monitor, UserAttentionType, WebviewUrl, WebviewWindowBuilder};
use tracing::warn;

const PREFIX: &str = "overlay-";

pub fn is_overlay(label: &str) -> bool {
    label.starts_with(PREFIX)
}

pub fn show(app: &AppHandle) {
    let existing: Vec<_> = app.webview_windows().into_iter().filter(|(l, _)| is_overlay(l)).collect();
    if !existing.is_empty() {
        for (i, (_, w)) in existing.iter().enumerate() {
            let _ = w.show();
            let _ = w.set_always_on_top(true);
            if i == 0 {
                let _ = w.set_focus();
            }
        }
        return;
    }

    let mut monitors = app.available_monitors().unwrap_or_default();
    if monitors.is_empty() {
        monitors.extend(app.primary_monitor().ok().flatten());
    }
    if monitors.is_empty() {
        create(app, 0, None);
    }
    for (i, m) in monitors.iter().enumerate() {
        create(app, i, Some(m));
    }
}

fn create(app: &AppHandle, index: usize, monitor: Option<&Monitor>) {
    let label = format!("{PREFIX}{index}");
    let built = WebviewWindowBuilder::new(app, &label, WebviewUrl::App("overlay.html".into()))
        .title("Poke — someone is calling you")
        .decorations(false)
        .resizable(false)
        .skip_taskbar(true)
        .always_on_top(true)
        .visible_on_all_workspaces(true)
        .shadow(false)
        .focused(index == 0)
        .visible(false)
        .build();
    let w = match built {
        Ok(w) => w,
        Err(e) => {
            warn!("could not create overlay window {label}: {e}");
            return;
        }
    };
    if let Some(m) = monitor {
        // Physical units avoid mixed-DPI rounding errors across monitors.
        let _ = w.set_position(*m.position());
        let _ = w.set_size(*m.size());
    }
    let _ = w.set_fullscreen(true);
    let _ = w.show();
    let _ = w.set_always_on_top(true);
    if index == 0 {
        let _ = w.set_focus();
        let _ = w.request_user_attention(Some(UserAttentionType::Critical));
    }
}

pub fn close_all(app: &AppHandle) {
    for (label, w) in app.webview_windows() {
        if is_overlay(&label) {
            let _ = w.destroy();
        }
    }
}
