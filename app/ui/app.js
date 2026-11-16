// Main panel: onboarding, colleague list, activity, settings.
// Talks to Rust through window.__TAURI__ (withGlobalTauri) — no bundler needed.
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);
let snap = null;
let tab = "people";

// ------------------------------------------------------------------ helpers

function esc(s) {
  return String(s ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);
}

function initials(name) {
  const parts = String(name || "?").trim().split(/\s+/);
  return ((parts[0]?.[0] || "?") + (parts.length > 1 ? parts[parts.length - 1][0] : "")).toUpperCase();
}

// Stable hue per colleague so avatars are distinguishable.
function hue(id) {
  let h = 0;
  for (const c of String(id)) h = (h * 31 + c.charCodeAt(0)) >>> 0;
  return h % 360;
}
function avatarStyle(id) {
  const h = hue(id);
  return `background: linear-gradient(135deg, hsl(${h} 75% 58%), hsl(${(h + 40) % 360} 70% 48%))`;
}

function ago(ms) {
  const s = Math.max(0, Math.round((Date.now() - ms) / 1000));
  if (s < 10) return "just now";
  if (s < 60) return `${s}s ago`;
  const m = Math.round(s / 60);
  if (m < 60) return `${m} min ago`;
  const d = new Date(ms);
  return d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
}

const STATUS = {
  sending: ["Sending…", ""],
  delivered: ["Delivered", ""],
  seen: ["Alert showing 👀", "ok"],
  coming: ["On the way 🏃", "ok"],
  five_min: ["In 5 min ⏳", "warn"],
  busy: ["Busy ⛔", "bad"],
  dismissed: ["Dismissed", ""],
  dnd: ["Do not disturb 🔕", "warn"],
  missed: ["No answer", "bad"],
  offline: ["Offline", "bad"],
  rate_limited: ["Slow down", "bad"],
  not_connected: ["Not sent — you're offline", "bad"],
};
const OUTCOME = {
  coming: "You answered: on the way",
  five_min: "You answered: in 5 min",
  busy: "You answered: busy",
  dismissed: "Dismissed",
  missed: "Missed",
  dnd: "Arrived during do-not-disturb",
};

let toastTimer;
function toast(text) {
  const t = $("toast");
  t.textContent = text;
  t.classList.add("show");
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => t.classList.remove("show"), 2600);
}

function showError(el, err) {
  el.textContent = String(err);
  el.classList.toggle("hidden", !err);
}

// ------------------------------------------------------------------ render

function render() {
  if (!snap) return;
  $("onboard").classList.toggle("hidden", snap.configured);
  $("app").classList.toggle("hidden", !snap.configured);
  if (!snap.configured) return;

  $("me-name").textContent = snap.display_name;
  $("me-avatar").textContent = initials(snap.display_name);
  $("dnd").checked = snap.dnd;

  const c = snap.conn;
  const dot = $("conn-dot");
  dot.className = "dot " + (snap.dnd && c.state === "online" ? "dnd" : c.state);
  $("conn-text").textContent =
    c.state === "online" ? (snap.dnd ? "Do not disturb" : "Online") : c.state === "connecting" ? "Connecting…" : "Offline";

  const offline = c.state === "offline";
  $("conn-banner").classList.toggle("hidden", !offline);
  if (offline) {
    $("conn-banner-text").textContent =
      `Can't reach the relay: ${c.reason}` + (c.retry_in_secs ? ` — retrying in ${c.retry_in_secs}s.` : "");
  }

  $("missed-banner").classList.toggle("hidden", snap.missed === 0);
  $("missed-text").textContent = `You missed ${snap.missed} poke${snap.missed === 1 ? "" : "s"}.`;
  const badge = $("missed-badge");
  badge.textContent = snap.missed;
  badge.classList.toggle("hidden", snap.missed === 0);

  renderPeers();
  renderActivity();
  if (tab === "settings") renderSettingsStatic();
}

function latestOutgoing(peerId) {
  return snap.outgoing.find((o) => o.to === peerId);
}

function renderPeers() {
  const q = $("search").value.trim().toLowerCase();
  const peers = snap.peers.filter((p) => !q || p.name.toLowerCase().includes(q));
  const box = $("peers");

  if (snap.conn.state !== "online") {
    box.innerHTML = `<div class="empty"><div class="big">📡</div>Waiting for the connection…</div>`;
    return;
  }
  if (snap.peers.length === 0) {
    box.innerHTML = `<div class="empty"><div class="big">👋</div>Nobody else is online yet.<br/>
      <button type="button" style="margin-top:12px" data-action="copy-invite">Copy invite for colleagues</button></div>`;
    return;
  }
  if (peers.length === 0) {
    box.innerHTML = `<div class="empty">No one matches “${esc(q)}”.</div>`;
    return;
  }
  box.innerHTML = peers
    .map((p) => {
      const out = latestOutgoing(p.id);
      const fresh = out && Date.now() - out.sent_at < 10 * 60 * 1000;
      const [label, cls] = fresh ? STATUS[out.status] || [out.status, ""] : [];
      const sub = [
        p.presence === "dnd" ? `<span class="chip warn">Do not disturb</span>` : "",
        fresh ? `<span class="chip ${cls}">${esc(label)}</span><span>${esc(ago(out.sent_at))}</span>` : "",
      ].join("");
      return `<div class="peer">
        <div class="avatar" style="${avatarStyle(p.id)}">${esc(initials(p.name))}</div>
        <div class="info"><div class="name">${esc(p.name)}</div><div class="sub">${sub || "Online"}</div></div>
        <button class="primary" type="button" data-poke="${esc(p.id)}">Poke</button>
      </div>`;
    })
    .join("");
}

function renderActivity() {
  const h = $("history");
  h.innerHTML = snap.history.length
    ? snap.history
        .map(
          (e) => `<div class="event">
          <div class="what"><b>${esc(e.from_name)}</b> · ${esc(OUTCOME[e.outcome] || e.outcome)}
            ${e.message ? `<div class="msg">“${esc(e.message)}”</div>` : ""}</div>
          <div class="when">${esc(ago(e.at))}</div></div>`,
        )
        .join("")
    : `<div class="hint">Nothing yet.</div>`;

  const s = $("sent");
  s.innerHTML = snap.outgoing.length
    ? snap.outgoing
        .map((o) => {
          const [label, cls] = STATUS[o.status] || [o.status, ""];
          return `<div class="event">
          <div class="what"><b>${esc(o.to_name)}</b> <span class="chip ${cls}">${esc(label)}</span>
            ${o.message ? `<div class="msg">“${esc(o.message)}”</div>` : ""}</div>
          <div class="when">${esc(ago(o.sent_at))}</div></div>`;
        })
        .join("")
    : `<div class="hint">Nothing yet.</div>`;
}

function renderSettingsStatic() {
  $("invite").textContent = snap.invite || "";
  $("team-id").textContent = snap.team_id || "—";
  $("version").textContent = snap.version;
}

// Editable fields are filled only when the tab opens, so live updates never
// overwrite what the user is typing.
function fillSettings() {
  $("set-name").value = snap.display_name;
  $("set-relay").value = snap.relay_url;
  $("set-code").value = snap.team_code;
  $("set-proxy").value = snap.proxy;
  $("pref-sound").checked = snap.sound;
  $("pref-volume").value = Math.round(snap.volume * 100);
  $("pref-autostart").checked = snap.autostart;
  const sel = $("pref-timeout");
  if (![...sel.options].some((o) => +o.value === snap.alert_timeout_secs)) {
    sel.add(new Option(`${snap.alert_timeout_secs} seconds`, snap.alert_timeout_secs));
  }
  sel.value = String(snap.alert_timeout_secs);
  showError($("set-error"), "");
  renderSettingsStatic();
}

function setTab(name) {
  tab = name;
  for (const b of document.querySelectorAll("nav.tabs button")) b.classList.toggle("active", b.dataset.tab === name);
  for (const t of ["people", "activity", "settings"]) $("tab-" + t).classList.toggle("hidden", t !== name);
  if (name === "settings") fillSettings();
  if (name === "activity" && snap?.missed) invoke("clear_missed");
  if (name === "people") $("search").focus();
}

// ------------------------------------------------------------------ actions

async function doPoke(id) {
  const msg = $("message").value.trim();
  try {
    await invoke("poke", { to: id, message: msg || null });
    $("message").value = "";
    const p = snap.peers.find((x) => x.id === id);
    toast(`Poked ${p ? p.name : "colleague"} 👋`);
  } catch (e) {
    toast(String(e));
  }
}

async function copyInvite() {
  if (!snap?.invite) return;
  try {
    await navigator.clipboard.writeText(snap.invite);
    toast("Invite copied — send it to your colleagues privately");
  } catch {
    setTab("settings");
    toast("Select the invite text and copy it");
  }
}

async function savePrefs() {
  try {
    await invoke("set_prefs", {
      sound: $("pref-sound").checked,
      volume: $("pref-volume").value / 100,
      alertTimeoutSecs: +$("pref-timeout").value,
      autostart: $("pref-autostart").checked,
    });
  } catch (e) {
    toast(String(e));
  }
}

// ------------------------------------------------------------------ onboarding

let obMode = "join";
function setObMode(mode) {
  obMode = mode;
  $("ob-tab-join").classList.toggle("active", mode === "join");
  $("ob-tab-create").classList.toggle("active", mode === "create");
  $("ob-join").classList.toggle("hidden", mode !== "join");
  $("ob-create").classList.toggle("hidden", mode !== "create");
  if (mode === "create" && !$("ob-code").textContent) regenCode();
}
async function regenCode() {
  $("ob-code").textContent = await invoke("generate_team_code");
}

async function finishOnboarding() {
  const err = $("ob-error");
  showError(err, "");
  const name = $("ob-name").value.trim();
  if (!name) return showError(err, "Please enter your name.");
  let relay, code;
  try {
    if (obMode === "join") {
      const parts = await invoke("parse_invite", { invite: $("ob-invite").value });
      relay = parts.relay_url;
      code = parts.team_code;
    } else {
      relay = $("ob-relay").value;
      code = $("ob-code").textContent;
    }
    snap = await invoke("save_settings", { displayName: name, relayUrl: relay, teamCode: code, proxy: $("ob-proxy").value });
    render();
    if (obMode === "create") {
      setTab("settings");
      toast("Team created — copy the invite for your colleagues");
    } else {
      setTab("people");
    }
  } catch (e) {
    showError(err, obMode === "join" && String(e).includes("team code") ? "That doesn't look like a valid invite." : e);
  }
}

// ------------------------------------------------------------------ wiring

function wire() {
  for (const b of document.querySelectorAll("nav.tabs button")) b.addEventListener("click", () => setTab(b.dataset.tab));

  $("peers").addEventListener("click", (e) => {
    const btn = e.target.closest("button");
    if (!btn) return;
    if (btn.dataset.poke) doPoke(btn.dataset.poke);
    if (btn.dataset.action === "copy-invite") copyInvite();
  });
  $("search").addEventListener("input", renderPeers);
  // Enter in the search box pokes the only/first match.
  const enterPokes = (e) => {
    if (e.key !== "Enter") return;
    const q = $("search").value.trim().toLowerCase();
    const match = snap?.peers.filter((p) => !q || p.name.toLowerCase().includes(q));
    if (match?.length === 1 || (q && match?.length)) doPoke(match[0].id);
  };
  $("search").addEventListener("keydown", enterPokes);
  $("message").addEventListener("keydown", enterPokes);

  $("dnd").addEventListener("change", (e) => invoke("set_dnd", { dnd: e.target.checked }));
  $("retry").addEventListener("click", () => invoke("reconnect"));
  $("missed-view").addEventListener("click", () => setTab("activity"));
  $("copy-invite").addEventListener("click", copyInvite);
  $("test-alert").addEventListener("click", () => invoke("test_alert"));
  $("quit").addEventListener("click", () => invoke("quit"));
  for (const id of ["pref-sound", "pref-timeout", "pref-autostart"]) $(id).addEventListener("change", savePrefs);
  $("pref-volume").addEventListener("change", savePrefs);

  $("set-save").addEventListener("click", async () => {
    showError($("set-error"), "");
    try {
      snap = await invoke("save_settings", {
        displayName: $("set-name").value,
        relayUrl: $("set-relay").value,
        teamCode: $("set-code").value,
        proxy: $("set-proxy").value,
      });
      render();
      toast("Saved — reconnecting");
    } catch (e) {
      showError($("set-error"), e);
    }
  });

  $("ob-tab-join").addEventListener("click", () => setObMode("join"));
  $("ob-tab-create").addEventListener("click", () => setObMode("create"));
  $("ob-regen").addEventListener("click", regenCode);
  $("ob-save").addEventListener("click", finishOnboarding);
  $("ob-invite").addEventListener("keydown", (e) => e.key === "Enter" && finishOnboarding());

  // Esc hides the panel (it keeps running in the tray).
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape" && snap?.configured) invoke("hide_window");
  });
  // Keep relative times fresh.
  setInterval(() => snap?.configured && (renderPeers(), renderActivity()), 15000);
}

async function init() {
  wire();
  await listen("snapshot", (e) => {
    snap = e.payload;
    render();
  });
  snap = await invoke("get_snapshot");
  render();
  if (!snap.configured) $("ob-name").focus();
  else setTab("people");
}

init();
