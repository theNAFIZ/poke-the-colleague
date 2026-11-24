# Poke the Colleague

Colleagues wearing headphones don't hear you calling their name. **Poke** fixes that: click their name and their screen shows a **full-screen alert** while a **ring plays through their headphones**. They answer with one click (*Coming* / *In 5 min* / *Busy*, or their own wording) and you see the answer.

- Runs on **Windows 10/11** and **Debian/Ubuntu** (X11 and GNOME on Wayland)
- **No admin rights** needed to install on Windows (per-user installer)
- Works in the office, from home and over VPN: everyone connects **outbound over HTTPS (443)** to a small relay
- The relay runs on **Cloudflare's free plan** and is **end-to-end encrypted**: it only ever sees ciphertext

```
 Alice's PC                 Cloudflare (free)                 Bob's PC
┌────────────┐  WSS:443  ┌─────────────────────────┐  WSS:443  ┌────────────┐
│ Poke app   │◄─────────►│ Worker + Durable Object │◄─────────►│ Poke app   │
│ encrypts   │           │ per team: who's online, │           │ full-screen│
│            │           │ forwards sealed blobs   │           │ + ring     │
└────────────┘           └─────────────────────────┘           └────────────┘
```

## Repository layout

| Path | What |
|---|---|
| `crates/poke-core/` | Rust library: protocol, team-code key derivation, E2E encryption, reconnecting relay client, proxy support. Also contains a headless CLI client (`examples/cli.rs`) for testing |
| `relay/` | Cloudflare Worker + Durable Object relay (TypeScript) |
| `app/src-tauri/` | Desktop app (Tauri 2): tray, overlay windows, sound, settings |
| `app/ui/` | Plain HTML/CSS/JS UI (no bundler): main panel and full-screen alert |
| `.github/workflows/build.yml` | Tests, then builds the Windows installer, `.deb` and AppImage |

---

## 1. Deploy the relay (once per company, free)

You need a free Cloudflare account. No credit card is required for the Workers free plan.

```bash
cd relay
npm ci
npx wrangler login        # opens the browser once
npx wrangler deploy       # prints https://poke-relay.<your-subdomain>.workers.dev
curl https://poke-relay.<your-subdomain>.workers.dev/health   # {"ok":true,"protocol":1}
```

**Optional: lock the relay to your team.** By default any valid team code works, but nobody can see or decrypt another team's traffic. To keep strangers from using your free quota, open the app, go to **Settings**, copy the **Team ID**, and set it in `relay/wrangler.toml`:

```toml
[vars]
ALLOWED_TEAMS = "0501137013ef1fc56fd042e5ac6cb1a6"   # comma-separated for several teams
```

Then run `npx wrangler deploy` again.

**Free-plan headroom.** Keep-alive pings are answered by Cloudflare without waking the relay, and idle connections hibernate. By my estimate, daily use by a team of 50 people stays well under 1% of the free limits. Check the Workers dashboard after the first week to confirm. If a limit were ever hit, the relay stops until 00:00 UTC and you are never billed.

## 2. Install the app

**Updates are built in (from 0.2.0).** Poke checks GitHub Releases at startup and every 6 hours, and you can also use **Settings → Check for updates** or the tray menu. When a new version exists, a banner offers **Update & restart**.
- Windows updates install per user, with no admin prompt.
- AppImages replace themselves.
- `.deb` installs ask for your password.

Every update is signed in CI, and the app refuses files whose signature doesn't match. Version 0.1.0 has no updater, so install 0.2.0 by hand once.

Download the installers from the repository's **Releases** page, or build them yourself (see below). CI publishes a release named `v<version>` automatically on every push to `main`. Bump `version` in `Cargo.toml` and `app/src-tauri/tauri.conf.json` to create a new release. Pushing again without bumping the version replaces the files in the existing release.

| File | For |
|---|---|
| `poke-the-colleague_<version>_windows_x64-setup.exe` | Windows 10/11 |
| `poke-the-colleague_<version>_amd64.deb` | Debian/Ubuntu (installs with `sudo`) |
| `poke-the-colleague_<version>_amd64.AppImage` | Any Linux, no root needed |

**Windows:** run `Poke the Colleague_x.y.z_x64-setup.exe`.
- It installs per user into `%LOCALAPPDATA%`, so **no admin prompt** appears.
- It starts at login through the per-user `Run` registry key.
- The installer is not code-signed yet, so SmartScreen may say *"Windows protected your PC"*. Click **More info → Run anyway**.
- It needs the Microsoft Edge WebView2 runtime. Windows 11 and up-to-date Windows 10 already have it.

**Debian / Ubuntu**, either:
- `sudo apt install ./poke-the-colleague_x.y.z_amd64.deb`, or
- **without root:** `chmod +x Poke*.AppImage && ./Poke*.AppImage`

**Debian with GNOME** has no tray-icon support out of the box. Enable the *AppIndicator and KStatusNotifierItem Support* extension (`gnome-shell-extension-appindicator`). Without it the app still works: open the window from the application menu.

## 3. Set up your team

1. **The first person** opens Poke, enters their name, chooses **Create a team**, pastes the relay address and clicks **Start**. In **Settings → Invite colleagues** they click **Copy**.
2. They send the invite privately, by chat or email, to colleagues. It looks like `https://poke-relay.example.workers.dev#ABCD-EFGH-IJKL-MNOP-QRST-UVWX`.
3. **Everyone else** chooses **Join my team** and pastes the invite.

The part after `#` is the **team code**. Anyone who has it can join the team, so share it like a password. To revoke access, create a new team code and share a new invite.

## Using it

- **Poke someone:** click **Poke** next to their name. You can add a short message first. Typing a name and pressing **Enter** also works.
- **What the caller sees:** *Delivered → Alert showing 👀 → On the way 🏃 / In 5 min ⏳ / Busy ⛔*, or *No answer* if the alert times out. You also get *Offline* immediately if they aren't connected, which beats shouting.
- **When you're poked:** a full-screen alert appears on every monitor and a ring plays in your headphones. Answer with **Enter** (Coming), **2** (5 min), **3** (Busy) or **Esc** (Dismiss).
- **Your own answers:** under **Settings → Alert answers** you can rename, add (up to 6) or remove the answer buttons, e.g. *🍵 After my tea*. Each one says what it means for the caller (*on my way*, *later* or *busy*). The caller sees your wording, while colleagues on older versions see the meaning.
- **Poke everyone:** use **📣 Poke everyone** for things like "Join the meeting" or "Lunch time". The quick buttons fill in the message (change them under **Settings → Quick messages**), and the app asks you to confirm before sending. Everyone online gets the alert, and Activity shows a single row summarising their answers.
- **Once a minute per person:** after you poke someone, their button shows a countdown until you can poke them again. *Poke everyone* is also limited to once a minute, and it skips anyone you poked individually in the last minute. The relay enforces the same rule.
- **In a call (automatic):** Poke treats "microphone in use" as being in a call. That covers Teams, Zoom, Meet in a browser, Slack huddles and so on.
  - Your colleagues see **In a call 📞** next to your name.
  - A poke during a call arrives as a quiet desktop notification with a soft chime, not the full-screen alert, so it can't appear on a screen share.
  - Poke only checks *whether* the microphone is in use and by which app. It never listens.
  - You can turn this off under **Settings → Detect calls**.
- **Screen locked (automatic):** colleagues see **Away 🔒** next to your name. Pokes that arrive while your screen is locked are held back. When you unlock, Poke opens **Activity** with everything you missed, each with a **👋 Poke back** button. The same summary appears when a call ends.
- **Do not disturb (DND):** use it for demos and screen sharing. Pokes are collected quietly and shown under **Activity**, and the caller is told you're in DND.
- **Settings → Show a test alert** previews the alert on your own machine.
- Closing the window keeps Poke running in the tray. Use **Quit** in the tray menu or in Settings to exit.

## Security & privacy

- The team code is a 120-bit random secret. Each client derives three values from it with HKDF-SHA256:
  - an **auth token**, which the relay hashes into the team room id, so the relay needs no configuration;
  - an **XChaCha20-Poly1305 key**, which never leaves the clients.
- Names, messages and replies are encrypted on the sender's PC. The routing metadata (sender, recipient, poke id) is bound into the ciphertext, so the relay can't redirect or replay messages unnoticed. Receivers also reject pokes more than 5 minutes old, and duplicates.
- The relay sees random client ids, the room id and encrypted data it can't read. It stores nothing and doesn't log message content.
- The relay allows 12 pokes per sender per minute. The app also stops you poking the same person again within 10 s.

## Network notes

- Only an **outbound HTTPS/WebSocket connection to port 443** is needed. There are no inbound ports and no firewall rules.
- **HTTP proxy:** the app picks up `HTTPS_PROXY`/`ALL_PROXY` (respecting `NO_PROXY`) and, on Windows, the proxy from Internet Options. Under **Settings → HTTP proxy** you can instead enter `http://host:port`, or `none` to connect directly. PAC/WPAD scripts are not evaluated, so set the proxy explicitly if your company uses one.
- If `*.workers.dev` is blocked, attach a custom domain to the Worker in the Cloudflare dashboard. This is also free.
- After sleep or resume, the app notices the clock jump and reconnects immediately.

## Build from source

Prerequisites: Rust (stable) and Node 20+. On Debian/Ubuntu you also need:

```bash
sudo apt install -y build-essential pkg-config libwebkit2gtk-4.1-dev libxdo-dev libssl-dev \
  libayatana-appindicator3-dev librsvg2-dev libasound2-dev
```

```bash
cargo test -p poke-core                 # unit tests

cd relay && npm ci && npm run dev       # local relay on http://localhost:8787

cd app && npm ci
npx tauri dev                           # run the app (use relay address localhost:8787)
npx tauri build --bundles deb,appimage  # Linux packages   -> target/release/bundle/
npx tauri build --bundles nsis          # Windows installer (run on Windows)
```

**Update signing key (for maintainers).** CI signs every release with a private key stored in the GitHub secret `TAURI_SIGNING_PRIVATE_KEY`. The matching public key is in `app/src-tauri/tauri.conf.json` (`plugins.updater.pubkey`).
- **Back up the private key** (`~/.tauri/poke-the-colleague.key` on the machine that created it) somewhere safe, such as a password manager.
- If the key is lost, installed apps can no longer be updated. You'd have to generate a new key and everyone would reinstall by hand.
- **Never commit the private key.**

To test the updater locally, point a debug build at your own manifest: `POKE_UPDATE_URL=http://localhost:8000/latest.json`.

**Headless test client:** useful for testing without a second PC.

```bash
cargo run -p poke-core --example cli -- gen-code
cargo run -p poke-core --example cli -- --relay localhost:8787 --code <CODE> --name Bob --auto-reply coming
cargo run -p poke-core --example cli -- --relay localhost:8787 --code <CODE> --name Alice --poke bob --msg "Lunch?"
```

**Troubleshooting**

- Set `POKE_LOG=debug` to get verbose logs on stderr.
- On GNOME/Wayland the app runs through XWayland so the alert can stay on top. Set `POKE_NATIVE_WAYLAND=1` to opt out.
- On Linux, the app's settings live in `~/.config/dev.poke.colleague/settings.json`. On Windows they are in `%APPDATA%\dev.poke.colleague\settings.json`.
