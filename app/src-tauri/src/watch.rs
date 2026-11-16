//! Automatic presence: "screen locked" and "in a call" (microphone in use).
//!
//! A background thread polls every couple of seconds and reports changes to
//! the app state. Nothing is recorded: we only ask the OS *whether* the screen
//! is locked and *which app* (if any) currently holds the microphone.
//!
//! * Lock — Windows: WTS session flags. Linux: logind `LockedHint` (GNOME, KDE, …),
//!   falling back to the freedesktop/GNOME ScreenSaver `GetActive`.
//! * Mic  — Windows: the per-user CapabilityAccessManager "microphone in use"
//!   records (same source as the taskbar mic icon). Linux: PipeWire capture
//!   streams via `pw-dump`, falling back to `pactl` on PulseAudio systems.

use std::time::Duration;

use tracing::info;

use crate::state::Shared;

const POLL: Duration = Duration::from_secs(2);
/// Mic must be seen in use this many polls in a row before we say "in a call"…
const CALL_ON_POLLS: u32 = 2;
/// …and unused this many polls in a row before the call is considered over.
const CALL_OFF_POLLS: u32 = 4;

pub fn spawn(state: Shared) {
    std::thread::Builder::new()
        .name("poke-watch".into())
        .spawn(move || {
            let mut lock = lock::Detector::default();
            let mut call: Option<String> = None;
            let (mut on, mut off) = (0u32, 0u32);
            loop {
                let locked = lock.is_locked();
                let mic = if state.detect_calls() { mic::active_app() } else { None };
                match (&call, mic) {
                    (None, Some(app)) => {
                        on += 1;
                        if on >= CALL_ON_POLLS {
                            info!("call started (microphone used by {app})");
                            call = Some(app);
                            on = 0;
                        }
                    }
                    (Some(_), Some(app)) => {
                        off = 0;
                        call = Some(app);
                    }
                    (Some(_), None) => {
                        off += 1;
                        if off >= CALL_OFF_POLLS {
                            info!("call ended");
                            call = None;
                            off = 0;
                        }
                    }
                    (None, None) => on = 0,
                }
                state.on_local_presence(locked, call.clone());
                std::thread::sleep(POLL);
            }
        })
        .expect("spawn presence watcher");
}

// ============================================================== lock: Linux

#[cfg(target_os = "linux")]
mod lock {
    use zbus::blocking::{proxy::Builder, Connection, Proxy};
    use zbus::proxy::CacheProperties;
    use zbus::zvariant::OwnedObjectPath;

    #[derive(Default)]
    pub struct Detector {
        system: Option<Connection>,
        session: Option<Connection>,
    }

    fn proxy<'a>(conn: &Connection, dest: &'a str, path: &'a str, iface: &'a str) -> zbus::Result<Proxy<'a>> {
        Builder::new(conn)
            .destination(dest)?
            .path(path)?
            .interface(iface)?
            .cache_properties(CacheProperties::No)
            .build()
    }

    impl Detector {
        pub fn is_locked(&mut self) -> bool {
            self.logind().unwrap_or(false) || self.screensaver().unwrap_or(false)
        }

        /// logind's LockedHint, set by GNOME, KDE and most lockers.
        fn logind(&mut self) -> Option<bool> {
            if self.system.is_none() {
                self.system = Connection::system().ok();
            }
            let conn = self.system.as_ref()?;
            let session = |path: &str| -> Option<bool> {
                proxy(conn, "org.freedesktop.login1", path, "org.freedesktop.login1.Session")
                    .ok()?
                    .get_property::<bool>("LockedHint")
                    .ok()
            };
            // "auto" = our session, or the user's display session if the app
            // runs outside one (e.g. started by systemd --user).
            if let Some(v) = session("/org/freedesktop/login1/session/auto") {
                return Some(v);
            }
            let display: (String, OwnedObjectPath) =
                proxy(conn, "org.freedesktop.login1", "/org/freedesktop/login1/user/self", "org.freedesktop.login1.User")
                    .ok()?
                    .get_property("Display")
                    .ok()?;
            let v = session(display.1.as_str());
            if v.is_none() {
                self.system = None; // reconnect next time
            }
            v
        }

        /// Fallback for desktops that don't set LockedHint.
        fn screensaver(&mut self) -> Option<bool> {
            if self.session.is_none() {
                self.session = Connection::session().ok();
            }
            let conn = self.session.as_ref()?;
            for (dest, path) in [
                ("org.gnome.ScreenSaver", "/org/gnome/ScreenSaver"),
                ("org.freedesktop.ScreenSaver", "/org/freedesktop/ScreenSaver"),
            ] {
                if let Ok(p) = proxy(conn, dest, path, dest) {
                    if let Ok(active) = p.call::<_, _, bool>("GetActive", &()) {
                        return Some(active);
                    }
                }
            }
            None
        }
    }
}

// ============================================================== lock: Windows

#[cfg(windows)]
mod lock {
    use windows_sys::Win32::System::RemoteDesktop::{
        WTSFreeMemory, WTSQuerySessionInformationW, WTSSessionInfoEx, WTSINFOEXW, WTS_CURRENT_SERVER_HANDLE,
        WTS_CURRENT_SESSION, WTS_SESSIONSTATE_LOCK,
    };

    #[derive(Default)]
    pub struct Detector;

    impl Detector {
        pub fn is_locked(&mut self) -> bool {
            // SAFETY: WTS allocates the buffer; we read it as the documented
            // WTSINFOEXW (level 1) and free it with WTSFreeMemory.
            unsafe {
                let mut buf: *mut u16 = std::ptr::null_mut();
                let mut len: u32 = 0;
                let ok = WTSQuerySessionInformationW(
                    WTS_CURRENT_SERVER_HANDLE,
                    WTS_CURRENT_SESSION,
                    WTSSessionInfoEx,
                    &mut buf,
                    &mut len,
                );
                if ok == 0 || buf.is_null() {
                    return false;
                }
                let mut locked = false;
                if len as usize >= std::mem::size_of::<WTSINFOEXW>() {
                    let info = &*(buf as *const WTSINFOEXW);
                    if info.Level == 1 {
                        locked = info.Data.WTSInfoExLevel1.SessionFlags == WTS_SESSIONSTATE_LOCK as i32;
                    }
                }
                WTSFreeMemory(buf as *mut core::ffi::c_void);
                locked
            }
        }
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
mod lock {
    #[derive(Default)]
    pub struct Detector;
    impl Detector {
        pub fn is_locked(&mut self) -> bool {
            false
        }
    }
}

// ============================================================== mic: Linux

#[cfg(target_os = "linux")]
mod mic {
    use std::process::Command;

    use serde_json::Value;

    /// Capture streams that are not calls (level meters, settings panels).
    const IGNORE: &[&str] = &["pavucontrol", "volume control", "gnome-control-center", "peak detect", "poke-the-colleague"];

    pub fn active_app() -> Option<String> {
        match pipewire() {
            Some(found) => found,
            None => pulse().flatten(),
        }
    }

    fn ignored(names: &[Option<&str>]) -> bool {
        names.iter().flatten().any(|n| {
            let n = n.to_lowercase();
            IGNORE.iter().any(|i| n.contains(i))
        })
    }

    fn own_pid(props: &Value) -> bool {
        let pid = std::process::id().to_string();
        match &props["application.process.id"] {
            Value::String(s) => *s == pid,
            Value::Number(n) => n.to_string() == pid,
            _ => false,
        }
    }

    /// `None` = PipeWire not available; `Some(None)` = no call.
    fn pipewire() -> Option<Option<String>> {
        let out = Command::new("pw-dump").output().ok()?;
        if !out.status.success() {
            return None;
        }
        let objects: Vec<Value> = serde_json::from_slice(&out.stdout).ok()?;
        for o in &objects {
            if o["type"] != "PipeWire:Interface:Node" {
                continue;
            }
            let info = &o["info"];
            let props = &info["props"];
            if props["media.class"] != "Stream/Input/Audio" || info["state"] != "running" {
                continue;
            }
            // Recording what is *played* (screen recorders), not the microphone.
            if props["stream.capture.sink"] == true || props["stream.monitor"] == true {
                continue;
            }
            let app = props["application.name"].as_str();
            let bin = props["application.process.binary"].as_str();
            let media = props["media.name"].as_str();
            if own_pid(props) || ignored(&[app, bin, media]) {
                continue;
            }
            let name = app.or(bin).or(props["node.name"].as_str()).unwrap_or("an app");
            tracing::debug!("microphone in use by {name}");
            return Some(Some(name.to_string()));
        }
        Some(None)
    }

    /// PulseAudio fallback (Debian 11 and older setups).
    fn pulse() -> Option<Option<String>> {
        let out = Command::new("pactl").args(["-f", "json", "list", "source-outputs"]).output().ok()?;
        if !out.status.success() {
            return None;
        }
        let outputs: Vec<Value> = serde_json::from_slice(&out.stdout).ok()?;
        for o in &outputs {
            if o["corked"] == true {
                continue;
            }
            let props = &o["properties"];
            let app = props["application.name"].as_str();
            let bin = props["application.process.binary"].as_str();
            let media = props["media.name"].as_str();
            if own_pid(props) || ignored(&[app, bin, media]) {
                continue;
            }
            return Some(Some(app.or(bin).unwrap_or("an app").to_string()));
        }
        Some(None)
    }
}

// ============================================================== mic: Windows

#[cfg(windows)]
mod mic {
    use std::time::{SystemTime, UNIX_EPOCH};

    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;

    const ROOT: &str = r"Software\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore\microphone";
    /// An app that crashed can leave a stale "in use" record; ignore very old ones.
    const STALE_SECS: u64 = 12 * 3600;

    pub fn active_app() -> Option<String> {
        let root = RegKey::predef(HKEY_CURRENT_USER).open_subkey(ROOT).ok()?;
        for name in root.enum_keys().flatten() {
            let Ok(key) = root.open_subkey(&name) else { continue };
            if name == "NonPackaged" {
                // Desktop apps, keyed by exe path with '\' replaced by '#'.
                for exe in key.enum_keys().flatten() {
                    if let Ok(sub) = key.open_subkey(&exe) {
                        if in_use(&sub) && !exe.to_lowercase().contains("poke-the-colleague") {
                            let file = exe.rsplit('#').next().unwrap_or(&exe);
                            return Some(file.trim_end_matches(".exe").trim_end_matches(".EXE").to_string());
                        }
                    }
                }
            } else if in_use(&key) {
                // Store apps (e.g. new Teams: MSTeams_8wekyb3d8bbwe).
                return Some(name.split('_').next().unwrap_or(&name).to_string());
            }
        }
        None
    }

    fn in_use(key: &RegKey) -> bool {
        let start: u64 = key.get_value("LastUsedTimeStart").unwrap_or(0);
        let stop: u64 = key.get_value("LastUsedTimeStop").unwrap_or(1);
        if start == 0 || stop != 0 {
            return false;
        }
        // FILETIME: 100 ns ticks since 1601-01-01.
        let now_unix = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let start_unix = (start / 10_000_000).saturating_sub(11_644_473_600);
        now_unix.saturating_sub(start_unix) < STALE_SECS
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
mod mic {
    pub fn active_app() -> Option<String> {
        None
    }
}
