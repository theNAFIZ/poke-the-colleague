// Full-screen alert. Several monitors each get one of these windows; they all
// show the same queue and any of them can answer.
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const $ = (id) => document.getElementById(id);

let items = [];
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
  $("kicker").textContent = top.all ? "📣 Message for everyone" : "Someone is calling you";
  $("title").textContent = top.all ? `${top.from_name} is calling everyone` : `${top.from_name} is calling you`;
  document.title = $("title").textContent;
  const msg = $("message");
  msg.hidden = !top.message;
  msg.textContent = top.message ? `“${top.message}”` : "";
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

async function respond(kind) {
  const top = items[0];
  if (!top || busy || !ready()) return;
  busy = true;
  try {
    await invoke("respond", { pid: top.pid, kind });
  } finally {
    busy = false;
  }
}

$("coming").addEventListener("click", () => respond("coming"));
$("five").addEventListener("click", () => respond("five_min"));
$("busy").addEventListener("click", () => respond("busy"));
$("dismiss").addEventListener("click", () => respond("dismissed"));

document.addEventListener("keydown", (e) => {
  if (e.repeat) return;
  if (e.key === "Escape") respond("dismissed");
  else if (e.key === "Enter" && !(e.target instanceof HTMLButtonElement)) respond("coming");
  else if (e.key === "2") respond("five_min");
  else if (e.key === "3") respond("busy");
});

(async () => {
  await listen("incoming", (e) => {
    items = e.payload;
    render();
  });
  items = await invoke("get_incoming");
  render();
  $("coming").focus();
  setInterval(() => items[0] && ($("when").textContent = ago(items[0].received_at)), 1000);
})();
