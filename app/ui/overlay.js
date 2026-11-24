// Full-screen alert. Several monitors each get one of these windows; they all
// show the same queue and any of them can answer.
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const $ = (id) => document.getElementById(id);

let items = [];
// Answer buttons from Settings: [{ label, kind }]; the first one is Enter.
let replies = [
  { label: "🏃 Coming", kind: "coming" },
  { label: "⏳ In 5 min", kind: "five_min" },
  { label: "⛔ Busy", kind: "busy" },
];
let busy = false;
// The alert grabs keyboard focus while people are typing. Ignore input briefly
// after it appears (or a new caller moves to the top) so a keystroke meant for
// another app doesn't answer the poke by accident.
const GUARD_MS = 1200;
let shownAt = Date.now();
let topPid = null;
const ready = () => Date.now() - shownAt >= GUARD_MS;

function initials(name) {
  const parts = String(name || "?").trim().split(/\s+/);
  return ((parts[0]?.[0] || "?") + (parts.length > 1 ? parts[parts.length - 1][0] : "")).toUpperCase();
}

function ago(ms) {
  const s = Math.max(0, Math.round((Date.now() - ms) / 1000));
  if (s < 5) return "just now";
  if (s < 60) return `${s} seconds ago`;
  const m = Math.floor(s / 60);
  return `${m} minute${m === 1 ? "" : "s"} ago`;
}

function render() {
  const top = items[0];
  if (!top) return;
  if (top.pid !== topPid) {
    topPid = top.pid;
    shownAt = Date.now();
  }
  $("avatar").textContent = initials(top.from_name);
  $("kicker").textContent = top.all ? "📣 Calling everyone" : "Calling you";
  $("title").textContent = top.from_name;
  document.title = top.all ? `${top.from_name} is calling everyone` : `${top.from_name} is calling you`;
  const msg = $("message");
  const text = top.message || "";
  msg.hidden = !text;
  msg.textContent = text;
  msg.classList.toggle("long", text.length > 140);
  msg.classList.toggle("medium", text.length > 60 && text.length <= 140);
  document.body.classList.toggle("no-msg", !text);
  $("when").textContent = ago(top.received_at);

  const rest = items.slice(1);
  const q = $("queue");
  q.hidden = rest.length === 0;
  q.innerHTML = "";
  if (rest.length) {
    q.append(`Also waiting: `);
    for (const r of rest) {
      const s = document.createElement("span");
      s.textContent = r.from_name;
      q.append(s);
    }
  }
}

async function respond(kind, label = null) {
  const top = items[0];
  if (!top || busy || !ready()) return;
  busy = true;
  try {
    await invoke("respond", { pid: top.pid, kind, label });
  } finally {
    busy = false;
  }
}

function renderButtons() {
  const box = $("actions");
  for (const b of box.querySelectorAll("button[data-reply]")) b.remove();
  replies.forEach((r, i) => {
    const b = document.createElement("button");
    b.type = "button";
    b.dataset.reply = i;
    if (i === 0) b.className = "primary";
    b.append(r.label);
    if (i < 9) {
      const k = document.createElement("kbd");
      k.textContent = i === 0 ? "Enter" : String(i + 1);
      b.append(k);
    }
    b.addEventListener("click", () => respond(r.kind, r.label));
    box.insertBefore(b, $("dismiss"));
  });
}

$("dismiss").addEventListener("click", () => respond("dismissed"));

document.addEventListener("keydown", (e) => {
  if (e.repeat) return;
  if (e.key === "Escape") return respond("dismissed");
  let r = null;
  if (e.key === "Enter" && !(e.target instanceof HTMLButtonElement)) r = replies[0];
  else if (/^[1-9]$/.test(e.key)) r = replies[+e.key - 1];
  if (r) respond(r.kind, r.label);
});

(async () => {
  await listen("incoming", (e) => {
    items = e.payload;
    render();
  });
  try {
    const configured = await invoke("get_replies");
    if (configured?.length) replies = configured;
  } catch {}
  renderButtons();
  items = await invoke("get_incoming");
  render();
  document.querySelector("button[data-reply]")?.focus();
  setInterval(() => items[0] && ($("when").textContent = ago(items[0].received_at)), 1000);
})();
