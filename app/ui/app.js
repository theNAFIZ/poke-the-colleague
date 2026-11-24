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
  too_soon: ["Already poked < 1 min ago", "warn"],
  in_call: ["In a call — notified quietly 📞", "warn"],
  locked: ["Away — sees it when back 🔒", "warn"],
};
const OUTCOME = {
  coming: "You answered: on the way",
  five_min: "You answered: in 5 min",
  busy: "You answered: busy",
  dismissed: "Dismissed",
  missed: "Missed",
  dnd: "Arrived during do-not-disturb",
  in_call: "Arrived during your call",
  locked: "Arrived while your screen was locked",
};

// Shorter wording inside a colleague's row, where their status chip already
// says "In a call" / "Away".
const ROW_STATUS = { in_call: ["Notified quietly", "warn"], locked: ["Sees it later", "warn"] };

// A colleague's custom answer ("After lunch") is shown in their own words,
// coloured by what it means (on the way / later / busy).
function statusOf(o) {
  const [label, cls] = STATUS[o.status] || [o.status, ""];
  return o.reply_text ? [`“${o.reply_text}”`, cls] : [label, cls];
}

const REPLY_MEANS = [
  ["coming", "On my way"],
  ["five_min", "Later"],
  ["busy", "Busy"],
];
const MAX_REPLIES = 6;
const MAX_QUICK = 8;

function secsLeft(untilMs) {
  return Math.max(0, Math.ceil((untilMs - Date.now()) / 1000));
}

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
  const quiet = c.state === "online" && (snap.dnd || snap.in_call);
  dot.className = "dot " + (quiet ? "dnd" : c.state);
  $("conn-text").textContent =
    c.state === "online"
      ? snap.dnd
        ? "Do not disturb"
        : snap.in_call
          ? `In a call (${snap.in_call}) · pokes arrive quietly`
          : "Online"
      : c.state === "connecting"
        ? "Connecting…"
        : "Offline";

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

  renderUpdate();
  renderQuick();
  renderPeers();
  renderPokeAll();
  renderActivity();
  if (tab === "settings") renderSettingsStatic();
}

function renderUpdate() {
  const v = snap.update_version;
  $("update-banner").classList.toggle("hidden", !v);
  if (!v) return;
  $("update-title").textContent = `Version ${v} is available`;
  const busy = !!snap.update_progress;
  $("update-hint").textContent =
    snap.update_progress ||
    (snap.update_needs_password ? "You'll be asked for your password to install it." : "Takes a few seconds; Poke restarts by itself.");
  const btn = $("update-install");
  btn.disabled = busy;
  btn.textContent = busy ? "Updating…" : "Update & restart";
}

async function installUpdate() {
  try {
    await invoke("install_update");
  } catch (e) {
    toast(String(e));
  }
}

async function checkUpdate() {
  const btn = $("check-update");
  btn.disabled = true;
  btn.textContent = "Checking…";
  try {
    const v = await invoke("check_update");
    toast(v ? `Version ${v} is available — see the banner at the top` : "You have the latest version.");
  } catch (e) {
    toast(`Update check failed: ${e}`);
  } finally {
    btn.disabled = false;
    btn.textContent = "Check for updates";
  }
}

function renderPokeAll() {
  const btn = $("poke-all");
  const wait = secsLeft(snap.poke_all_until);
  const online = snap.conn.state === "online" && snap.peers.length > 0;
  btn.disabled = !online || wait > 0;
  btn.textContent = wait > 0 ? `📣 Poke everyone (again in ${wait}s)` : `📣 Poke everyone${online ? ` (${snap.peers.length})` : ""}`;
  if (btn.disabled) $("poke-all-confirm").classList.add("hidden");
}

let quickKey = "";
function renderQuick() {
  const key = JSON.stringify(snap.quick_messages);
  if (key === quickKey) return;
  quickKey = key;
  $("quick").innerHTML = snap.quick_messages
    .map((m) => `<button type="button" class="chip-btn" data-msg="${esc(m)}">${esc(m)}</button>`)
    .join("");
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
      const [label, cls] = fresh ? (!out.reply_text && ROW_STATUS[out.status]) || statusOf(out) : [];
      const wait = secsLeft(snap.cooldowns[p.id] || 0);
      const sub = [
        p.locked ? `<span class="chip">Away 🔒</span>` : "",
        p.in_call ? `<span class="chip warn">In a call 📞</span>` : "",
        p.presence === "dnd" ? `<span class="chip warn">Do not disturb</span>` : "",
        fresh ? `<span class="chip ${cls}">${esc(label)}</span><span>${esc(ago(out.sent_at))}</span>` : "",
      ].join("");
      const title = p.locked
        ? "Their screen is locked — they'll see the poke when they're back"
        : p.in_call
          ? "They're in a call — they'll get a quiet notification"
          : "";
      return `<div class="peer" title="${esc(title)}">
        <div class="avatar" style="${avatarStyle(p.id)}">${esc(initials(p.name))}</div>
        <div class="info"><div class="name">${esc(p.name)}</div><div class="sub">${sub || "Online"}</div></div>
        <button class="primary" type="button" data-poke="${esc(p.id)}" ${wait > 0 ? "disabled" : ""}
          title="${wait > 0 ? "You can poke the same person once a minute" : ""}">${wait > 0 ? `${wait}s` : "Poke"}</button>
      </div>`;
    })
    .join("");
}

function renderActivity() {
  $("mark-read").classList.toggle("hidden", snap.missed === 0);
  const online = new Set(snap.peers.map((p) => p.id));
  const h = $("history");
  h.innerHTML = snap.history.length
    ? snap.history
        .map((e) => {
          const canPokeBack = e.unread && online.has(e.from);
          const wait = secsLeft(snap.cooldowns[e.from] || 0);
          return `<div class="event ${e.unread ? "unread" : ""}">
          <div class="what"><b>${esc(e.from_name)}</b>${e.all ? " → everyone" : ""} · ${esc(e.reply_text ? `You answered: “${e.reply_text}”` : OUTCOME[e.outcome] || e.outcome)}
            ${e.message ? `<div class="msg">“${esc(e.message)}”</div>` : ""}
            ${canPokeBack ? `<div class="actions"><button type="button" data-poke="${esc(e.from)}" ${wait ? "disabled" : ""}>👋 Poke back${wait ? ` (${wait}s)` : ""}</button></div>` : ""}</div>
          <div class="when">${esc(ago(e.at))}</div></div>`;
        })
        .join("")
    : `<div class="hint">Nothing yet.</div>`;

  // A "poke everyone" is one row with a summary of everybody's answers.
  const groups = [];
  const byPid = new Map();
  for (const o of snap.outgoing) {
    if (!o.all) {
      groups.push([o]);
      continue;
    }
    if (!byPid.has(o.pid)) {
      byPid.set(o.pid, []);
      groups.push(byPid.get(o.pid));
    }
    byPid.get(o.pid).push(o);
  }
  const s = $("sent");
  s.innerHTML = groups.length
    ? groups
        .map((g) => {
          const o = g[0];
          let who, chips;
          if (o.all) {
            who = `Everyone (${g.length})`;
            // Group by what is shown, so different custom answers stay apart.
            const buckets = new Map();
            for (const x of g) {
              const [label, cls] = statusOf(x);
              const k = `${cls}|${label}`;
              if (!buckets.has(k)) buckets.set(k, { label, cls, names: [] });
              buckets.get(k).names.push(x.to_name);
            }
            chips = [...buckets.values()]
              .map((b) => `<span class="chip ${b.cls}" title="${esc(b.names.join(", "))}">${b.names.length} × ${esc(b.label)}</span>`)
              .join(" ");
          } else {
            const [label, cls] = statusOf(o);
            who = o.to_name;
            chips = `<span class="chip ${cls}">${esc(label)}</span>`;
          }
          return `<div class="event">
          <div class="what"><b>${esc(who)}</b> ${chips}
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
  $("pref-calls").checked = snap.detect_calls;
  const sel = $("pref-timeout");
  if (![...sel.options].some((o) => +o.value === snap.alert_timeout_secs)) {
    sel.add(new Option(`${snap.alert_timeout_secs} seconds`, snap.alert_timeout_secs));
  }
  sel.value = String(snap.alert_timeout_secs);
  fillReplies(snap.replies);
  fillQuick(snap.quick_messages);
  showError($("set-error"), "");
  renderSettingsStatic();
}

function setTab(name) {
  // Leaving Activity marks what was shown there as read.
  if (tab === "activity" && name !== "activity" && snap?.missed) invoke("clear_missed");
  tab = name;
  for (const b of document.querySelectorAll("nav.tabs button")) b.classList.toggle("active", b.dataset.tab === name);
  for (const t of ["people", "activity", "settings"]) $("tab-" + t).classList.toggle("hidden", t !== name);
  if (name === "settings") fillSettings();
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

function askPokeAll() {
  const msg = $("message").value.trim();
  $("poke-all-text").textContent =
    `Poke all ${snap.peers.length} online colleague${snap.peers.length === 1 ? "" : "s"}` + (msg ? ` with “${msg}”?` : "?");
  $("poke-all-confirm").classList.remove("hidden");
  $("poke-all-yes").focus();
}

async function doPokeAll() {
  $("poke-all-confirm").classList.add("hidden");
  const msg = $("message").value.trim();
  try {
    const n = await invoke("poke_all", { message: msg || null });
    $("message").value = "";
    toast(`Poked ${n} colleague${n === 1 ? "" : "s"} 📣`);
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
      detectCalls: $("pref-calls").checked,
    });
  } catch (e) {
    toast(String(e));
  }
}

// ------------------------------------------------------------------ alert answers & quick messages

function replyRow(r = { label: "", kind: "coming" }) {
  const row = document.createElement("div");
  row.className = "edit-row";
  row.innerHTML = `<input type="text" maxlength="60" placeholder="e.g. 🍵 After my tea" autocomplete="off" />
    <select>${REPLY_MEANS.map(([k, t]) => `<option value="${k}">${t}</option>`).join("")}</select>
    <button type="button" class="ghost remove" title="Remove">✕</button>`;
  row.querySelector("input").value = r.label;
  row.querySelector("select").value = r.kind;
  return row;
}

function quickRow(m = "") {
  const row = document.createElement("div");
  row.className = "edit-row";
  row.innerHTML = `<input type="text" maxlength="280" placeholder="e.g. Coffee? ☕" autocomplete="off" />
    <button type="button" class="ghost remove" title="Remove">✕</button>`;
  row.querySelector("input").value = m;
  return row;
}

function fillReplies(list) {
  $("replies-edit").replaceChildren(...list.map(replyRow));
  updateEditButtons();
}

function fillQuick(list) {
  $("quick-edit").replaceChildren(...list.map((m) => quickRow(m)));
  updateEditButtons();
}

function updateEditButtons() {
  const replies = $("replies-edit").children;
  $("reply-add").disabled = replies.length >= MAX_REPLIES;
  // At least one answer button must stay.
  for (const b of $("replies-edit").querySelectorAll(".remove")) b.disabled = replies.length <= 1;
  $("quick-add").disabled = $("quick-edit").children.length >= MAX_QUICK;
}

async function saveReplies() {
  const replies = [...$("replies-edit").children]
    .map((row) => ({ label: row.querySelector("input").value.trim(), kind: row.querySelector("select").value }))
    .filter((r) => r.label);
  if (!replies.length) return; // keep the last saved list until something is typed
  try {
    await invoke("set_replies", { replies });
  } catch (e) {
    toast(String(e));
  }
}

async function saveQuick() {
  const messages = [...$("quick-edit").querySelectorAll("input")].map((i) => i.value.trim()).filter(Boolean);
  try {
    await invoke("set_quick_messages", { messages });
  } catch (e) {
    toast(String(e));
  }
}

function wireEditList(listId, save) {
  const list = $(listId);
  list.addEventListener("change", save);
  list.addEventListener("click", (e) => {
    const btn = e.target.closest("button.remove");
    if (!btn || btn.disabled) return;
    btn.closest(".edit-row").remove();
    updateEditButtons();
    save();
  });
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
  $("history").addEventListener("click", (e) => {
    const btn = e.target.closest("button[data-poke]");
    if (btn) doPoke(btn.dataset.poke);
  });
  $("quick").addEventListener("click", (e) => {
    const chip = e.target.closest("button[data-msg]");
    if (!chip) return;
    $("message").value = chip.dataset.msg;
    $("message").focus();
  });
  $("poke-all").addEventListener("click", askPokeAll);
  $("poke-all-yes").addEventListener("click", doPokeAll);
  $("poke-all-no").addEventListener("click", () => $("poke-all-confirm").classList.add("hidden"));
  $("mark-read").addEventListener("click", () => invoke("clear_missed"));
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
  $("update-install").addEventListener("click", installUpdate);
  $("check-update").addEventListener("click", checkUpdate);
  for (const id of ["pref-sound", "pref-timeout", "pref-autostart", "pref-calls"]) $(id).addEventListener("change", savePrefs);
  $("pref-volume").addEventListener("change", savePrefs);

  wireEditList("replies-edit", saveReplies);
  wireEditList("quick-edit", saveQuick);
  $("reply-add").addEventListener("click", () => {
    const row = replyRow();
    $("replies-edit").append(row);
    updateEditButtons();
    row.querySelector("input").focus();
  });
  $("quick-add").addEventListener("click", () => {
    const row = quickRow();
    $("quick-edit").append(row);
    updateEditButtons();
    row.querySelector("input").focus();
  });
  $("reply-reset").addEventListener("click", () => {
    fillReplies(snap.default_replies);
    saveReplies();
  });
  $("quick-reset").addEventListener("click", () => {
    fillQuick(snap.default_quick_messages);
    saveQuick();
  });

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
  // Keep relative times and cooldown countdowns fresh.
  let ticks = 0;
  setInterval(() => {
    if (!snap?.configured) return;
    ticks++;
    const counting = snap.poke_all_until > Date.now() || Object.values(snap.cooldowns).some((t) => t > Date.now() - 1000);
    if (counting || ticks % 15 === 0) {
      renderPeers();
      renderPokeAll();
      renderActivity();
    }
  }, 1000);
}

async function init() {
  wire();
  await listen("show-activity", () => setTab("activity"));
  await listen("toast", (e) => toast(String(e.payload)));
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
