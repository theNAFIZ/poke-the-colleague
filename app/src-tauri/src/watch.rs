//! Automatic presence: "screen locked" and "in a call" (microphone in use).
//!
//! A background thread polls every couple of seconds and reports changes to
//! the app state. Nothing is recorded: we only ask the OS *whether* the screen
//! is locked and *which app* (if any) currently holds the microphone.
//!
//! * Lock — Windows: WTS session flags. Linux: logind `LockedHint` (GNOME, KDE, …),
//!   falling back to the freedesktop/GNOME ScreenSaver `GetActive`.
//! * Mic  — Windows: active WASAPI capture sessions on every microphone, with
//!   the CapabilityAccessManager "microphone in use" registry records as a
//!   fallback. Linux: PipeWire capture
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

    /// Capture sessions that are not calls (level meters, audio tools).
    const IGNORE: &[&str] = &["poke-the-colleague", "audiodg", "sndvol", "nvidia broadcast", "realtek"];

    pub fn active_app() -> Option<String> {
        // WASAPI sees every app that is recording right now. The registry
        // records are kept as a fallback: they are what the taskbar mic icon
        // used on older builds, but newer Windows 11 versions stopped updating
        // them reliably.
        wasapi::active_app().or_else(registry::active_app)
    }

    fn ignored(name: &str) -> bool {
        let n = name.to_lowercase();
        IGNORE.iter().any(|i| n.contains(i))
    }

    /// `C:\…\ms-teams.exe` → `Teams`, other apps → file name without `.exe`.
    fn pretty(exe: &str) -> String {
        let file = exe.rsplit(['\\', '/', '#']).next().unwrap_or(exe);
        let stem = file.strip_suffix(".exe").or_else(|| file.strip_suffix(".EXE")).unwrap_or(file);
        match stem.to_lowercase().as_str() {
            "ms-teams" | "msteams" | "teams" => "Teams".into(),
            "zoom" => "Zoom".into(),
            "slack" => "Slack".into(),
            "chrome" => "Chrome".into(),
            "msedge" => "Edge".into(),
            "msedgewebview2" => "a web app".into(),
            "firefox" => "Firefox".into(),
            "webex" | "ciscocollabhost" | "webexmta" => "Webex".into(),
            "discord" => "Discord".into(),
            "skype" => "Skype".into(),
            _ => stem.to_string(),
        }
    }

    /// Active capture sessions on every enabled microphone.
    mod wasapi {
        use std::cell::Cell;

        use windows::core::{Interface, PWSTR};
        use windows::Win32::Foundation::{CloseHandle, S_OK};
        use windows::Win32::Media::Audio::{
            eCapture, AudioSessionStateActive, IAudioSessionControl2, IAudioSessionManager2, IMMDeviceEnumerator,
            MMDeviceEnumerator, DEVICE_STATE_ACTIVE,
        };
        use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED};
        use windows::Win32::System::Threading::{
            OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
        };

        thread_local!(static COM: Cell<bool> = const { Cell::new(false) });

        pub fn active_app() -> Option<String> {
            COM.with(|c| {
                if !c.get() {
                    // SAFETY: initialises COM for this (watcher) thread once; it lives
                    // as long as the app, so it is never uninitialised.
                    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
                    c.set(true);
                }
            });
            match unsafe { scan() } {
                Ok(found) => found,
                Err(e) => {
                    tracing::debug!("WASAPI session scan failed: {e}");
                    None
                }
            }
        }

        /// SAFETY: plain COM calls on interfaces we own; COM is initialised above.
        unsafe fn scan() -> windows::core::Result<Option<String>> {
            let own = std::process::id();
            let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let devices = enumerator.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE)?;
            for d in 0..devices.GetCount()? {
                let Ok(device) = devices.Item(d) else { continue };
                let Ok(manager) = device.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) else { continue };
                let Ok(sessions) = manager.GetSessionEnumerator() else { continue };
                for s in 0..sessions.GetCount()? {
                    let Ok(control) = sessions.GetSession(s) else { continue };
                    if control.GetState().ok() != Some(AudioSessionStateActive) {
                        continue;
                    }
                    let Ok(control) = control.cast::<IAudioSessionControl2>() else { continue };
                    if control.IsSystemSoundsSession() == S_OK {
                        continue;
                    }
                    let pid = control.GetProcessId().unwrap_or(0);
                    if pid == own {
                        continue;
                    }
                    let exe = if pid == 0 { None } else { process_path(pid) };
                    if exe.as_deref().is_some_and(super::ignored) {
                        continue;
                    }
                    let name = exe.as_deref().map(super::pretty).unwrap_or_else(|| "an app".into());
                    tracing::debug!("microphone in use by {name} (pid {pid})");
                    return Ok(Some(name));
                }
            }
            Ok(None)
        }

        fn process_path(pid: u32) -> Option<String> {
            // SAFETY: the handle is only used for the query below and closed right after.
            unsafe {
                let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
                let mut buf = [0u16; 1024];
                let mut len = buf.len() as u32;
                let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).is_ok();
                let _ = CloseHandle(h);
                ok.then(|| String::from_utf16_lossy(&buf[..len as usize]))
            }
        }
    }

    /// The per-user CapabilityAccessManager "microphone in use" records.
    mod registry {
        use super::*;

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
                            if in_use(&sub) && !ignored(&exe) {
                                return Some(pretty(&exe));
                            }
                        }
                    }
                } else if in_use(&key) {
                    // Store apps (e.g. new Teams: MSTeams_8wekyb3d8bbwe).
                    return Some(pretty(name.split('_').next().unwrap_or(&name)));
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
}

#[cfg(not(any(windows, target_os = "linux")))]
mod mic {
    pub fn active_app() -> Option<String> {
        None
    }
}
