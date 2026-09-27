# WSL2 daemon runbook (lab for the Clinch engine)

> Status: `clinch-daemon` is a standalone Linux binary; the Windows Tauri app
> becomes a thin client when `CLINCH_DAEMON_URL` is set. Wire protocol:
> `packages/clinch-protocol/PROTOCOL.md`.
>
> Constitution alignment (2026-09-27): Linux is the worker OS and Windows is
> a client; WSL is how *we* develop the worker, not how customers install.
> This lab is the path toward the constitution's hybrid model
> ([CONSTITUTION.md](CONSTITUTION.md) §6).

## 1. Goal + the deal

The engine moves to a clean Linux lab (WSL2 Ubuntu) so we can tell a Windows
problem apart from an engine problem. Engine behavior is identical there —
same funnel, verifier, consent gates, cookie fallback; this is a hosting
change only, and the frontend is untouched (same event shapes).

- **Success** = the daemon runs, frames stream clean (no static garbage),
  and the run journal shows the `click_hit_test:` line after clicks.
- **Failure** = garbage frames or dead sessions *on Linux*. If frames are
  clean but the Reddit avatar menu still won't open, the lab is working —
  that's a logic bug, and no cloud machine would fix it either.
- **Escalation**: Windows → WSL2 daemon → (only if WSL2 can't produce a
  clean signal) VPS. The consumer-cloud question is a separate,
  much-later decision and stays decoupled from this lab.

## 2. One-time WSL2 / Ubuntu setup (on Windows)

In an **elevated** PowerShell:

```powershell
wsl --install
```

This installs WSL2 and the default Ubuntu distro. Reboot if prompted.
Launch **Ubuntu** from the Start menu on first boot and create your Linux
user when asked (pick a username + password; this becomes the sudo user).

Verify from PowerShell:

```powershell
wsl --status        # expect: Default Version: 2
wsl -l -v           # expect: Ubuntu ... VERSION 2
```

Troubleshooting for this step: on Windows 10/11 21H2+, `wsl --install`
works out of the box. If WSL2 complains about virtualization, enable
Virtual Machine Platform: `dism.exe /online /enable-feature /featurename:VirtualMachinePlatform /all /norestart`, then reboot.

## 3. Inside Ubuntu — toolchain

All commands below run **inside the Ubuntu shell** (`wsl` from PowerShell
drops you in).

```bash
sudo apt update && sudo apt upgrade -y

# build toolchain
sudo apt install -y git curl build-essential pkg-config libsqlite3-dev

# Rust (stable default)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
source "$HOME/.cargo/env"
rustc --version   # expect 1.8x+
```

Why these deps: the workspace uses `sqlx` 0.8 with sqlite
(`default-features = false`, no bundled sqlite), so the crate links the
**system** sqlite3 — hence `pkg-config` + `libsqlite3-dev`. HTTP in the
engine goes through `ureq` 3 (rustls), so no `libssl-dev` is needed. Node
is **not** required to build the daemon (it is pure Rust; the frontend is
built on the Windows side by the normal dev ritual — only needed if you
ever build the web UI inside WSL, in which case use Node 20 via nvm).

Clone the repo:

```bash
mkdir -p ~/code && cd ~/code
git clone https://github.com/pratyush2514/Clinch.git
cd Clinch
git checkout main
```

> The repo path inside WSL is a normal Linux path
> (`/home/<user>/code/Clinch`) — not under `/mnt/c`. The daemon and its
> data live in Linux-land; the Windows side reaches them over the
> localhost relay (section 7).

## 4. Chromium + Xvfb for the daemon

The daemon launches Chromium **off-screen headed** exactly the way the
desktop app does; the only thing that differs on Linux is `DISPLAY`.
Use Google's official .deb (a real binary, no snap wrapper — Ubuntu's
`apt install chromium` ships a snap shim, don't use it):

```bash
cd /tmp
wget https://dl.google.com/linux/direct/google-chrome-stable_current_amd64.deb
sudo apt install -y ./google-chrome-stable_current_amd64.deb
google-chrome --version   # sanity check
```

Tell the daemon where it is:

```bash
# add to ~/.bashrc (or export in the shell you launch the daemon from)
export CLINCH_CHROMIUM_PATH=/usr/bin/google-chrome
```

Xvfb provides the display the off-screen headed browser needs:

```bash
sudo apt install -y xvfb
Xvfb :99 -screen 0 1920x1080x24 &
export DISPLAY=:99
```

(Alternative one-shot: `xvfb-run -a -s "-screen 0 1920x1080x24" ./target/release/clinch-daemon`.
Prefer the persistent `:99` server — restarts of the daemon don't then
need a new display.)

## 5. Build + launch the daemon

From the repo root inside WSL:

```bash
cd ~/code/Clinch
cargo build --release -p clinch-daemon
```

> If `cargo` can't find the `clinch-daemon` package, its crate
> (`packages/clinch-daemon`) hasn't landed on main yet — pull latest main
> (`git pull`) before treating anything below as broken.

The **first build takes a while** (several minutes, big dependency tree);
incremental rebuilds after pulls are fast — same rule as the Windows
dev-loop: never `cargo clean` unless `target/` is corrupted.

Launch it (keep this shell open — logs go to stderr here):

```bash
export DISPLAY=:99
export CLINCH_CHROMIUM_PATH=/usr/bin/google-chrome
./target/release/clinch-daemon
# or a custom port:
./target/release/clinch-daemon --port 18791
# env alternative:
CLINCH_DAEMON_PORT=18791 ./target/release/clinch-daemon
```

Expected output on start: it binds **127.0.0.1:18790** (or your override)
for the engine API and **127.0.0.1:9223** for the Companion extension
bridge. If you see an `address in use` error, another daemon is already
running — see Troubleshooting.

**Data dir**: `~/.local/share/clinch-daemon/` inside WSL. Playbooks and
site shortcuts start **fresh** there — nothing migrates from the Windows
profile in v1 (known limitation, section 10).

Useful aliases for the daemon shell:

```bash
alias clinchd='DISPLAY=:99 CLINCH_CHROMIUM_PATH=/usr/bin/google-chrome ~/code/Clinch/target/release/clinch-daemon'
```

## 6. Windows side — run the app in remote mode

The Windows ritual stays the same `scripts/dev.ps1` flow (pull latest
main, env from `.env.local`, incremental launch), with **one addition**:

```powershell
# in the PowerShell you launch the app from (Process scope, not committed):
$env:CLINCH_DAEMON_URL = "ws://127.0.0.1:18790"
.\scripts\dev.ps1
```

- With `CLINCH_DAEMON_URL` set → the Tauri app runs in **remote mode**:
  every command proxies to the daemon over WebSocket, and frame/cursor/
  progress events feed the same emitters the frontend already uses.
- With it **unset** → embedded mode works exactly as today (nothing
  regresses).
- In remote mode the Windows-side `CLINCH_CHROMIUM_PATH` is irrelevant:
  the daemon launches the browser on the Linux side.
- Keep `GROQ_API_KEY` / `CLINCH_GROUNDER_PROVIDER=groq` on the **Windows**
  side as before (routing/grounding decisions happen where the app runs;
  the daemon executes the engine). Never commit keys; `.env.local`
  stays the home for them.

## 7. Companion extension reachability (explicit check)

The extension runs in **Windows Chrome** and dials `ws://127.0.0.1:9223`
(hardcoded in `packages/extension-bridge/offscreen.js`). The daemon
serves the bridge on that port from inside WSL2. WSL2's localhost relay
forwards Windows-loopback → WSL2 for ports bound inside WSL2, so this
works — **verify it explicitly** while the daemon is running, from
Windows PowerShell:

```powershell
Test-NetConnection -ComputerName 127.0.0.1 -Port 18790   # engine API
Test-NetConnection -ComputerName 127.0.0.1 -Port 9223    # companion bridge
```

Both must report **TcpTestSucceeded: True**. If either fails:

1. Confirm the daemon is actually running in WSL and bound to
   127.0.0.1 (not erroring on startup).
2. Restart the WSL2 localhost relay: from elevated PowerShell,
   `wsl --shutdown`, wait ~10s, re-enter Ubuntu, relaunch the daemon,
   re-test.
3. If it still fails, the relay didn't pick up the port — record which
   port failed and the daemon's startup log; that's the signal to pause
   and report rather than hack around it.

## 8. Smoke test sequence

1. In the app (remote mode), run `initialize` — expect a `StorageStatus`
   result (fresh store under the daemon's data dir).
2. Prompt: **"log out from reddit"** (or "open reddit for me" first, then
   logout — same as the Windows proof).
3. Watch the browser view:
   - **Frames must be clean** — no static garbage, no frozen-corrupt
     frames. If they stream cleanly, the pipeline is healthy and the
     Windows flakiness was environmental.
4. The run journal should contain, after each click, a line of the form
   (landed in `e1ccda13`):
   ```
   click_hit_test: (X, Y) -> TAG role=<role> name="<name>"
   ```
   - **Line missing entirely** → the Windows box is running the old
     build; pull + full `dev.ps1` rebuild (frontend compile included),
     not just a pull.
   - **`hit` == the avatar** (e.g. `BUTTON role=button name="User avatar"` at
     header coordinates) → targeting is correct; if the menu still won't
     open it's an **actuation** problem.
   - **`hit` != the avatar** (an ad button, a "..." menu, body) →
     **targeting wander**; the retry/re-ground path (`logout_opener_retry`)
     is what should fire.
5. Expected end state: `COMPLETED` with `auth_state_detected:
   www.reddit.com · logged out` (cookie-clear fallback still guards the
   outcome even if the UI path fails).

## 9. Troubleshooting

| Symptom | Check |
|---|---|
| Daemon exits: `address in use` | Another daemon holds 18790/9223. In WSL: `ss -ltnp \| grep -E '18790\|9223'` then kill the PID, or relaunch with `--port`. |
| No frames at all in the app | `acquire_browser_context` must have been issued; check the daemon's stderr for CDP errors; confirm `DISPLAY=:99` is exported in the daemon's shell and `Xvfb :99` is running (`ps aux \| grep Xvfb`). |
| Frames still static/garbage on Linux | Then it's the **pipeline**, not Windows — the lab worked. Report it: note the daemon commit, the site, and whether `click_hit_test` lines look sane. |
| Extension shows "not connected" | The extension only syncs when reachable; run the section-7 `Test-NetConnection` checks. Note: bridge works only while the daemon is running — "not connected" with a stopped daemon is correct behavior. |
| `CLINCH_CHROMIUM_PATH` ignored | Must be exported in the **daemon's** shell, not just the Windows one. Verify with `echo $CLINCH_CHROMIUM_PATH` in WSL before launching. |
| WSL2 localhost relay dies after sleep/resume | `wsl --shutdown` from elevated PowerShell, relaunch daemon, re-run section-7 checks. |
| Where are the logs? | The daemon's **stderr** (the terminal you launched it from). For a persistent run: `./target/release/clinch-daemon > ~/clinch-daemon.log 2>&1 &`. |
| Menu opens but logout never completes | Read the journal: avatar→verify→one retry→max 3 model steps→cookie fallback is the bounded sequence; a step that silently repeats is the bug to file. |

## 10. What's NOT in v1

- **Take Control** needs a display on the daemon host (`take_control`
  documents this in PROTOCOL.md). From the Windows thin client it can't
  show a real window — use it from a Linux session with a display, or
  wait for a later iteration.
- **File reveal**: downloads land in the daemon's data dir; the thin
  client resolves them via `downloaded_file_path` and translates to
  `\\wsl$\<distro>\...` for the local OS opener. Distro name comes from
  `CLINCH_WSL_DISTRO` (default `Ubuntu`).
- **No memory migration**: playbooks/shortcuts saved on Windows do not
  move to `~/.local/share/clinch-daemon/`; re-save the ones you need via
  the consent card.
- **VPS escalation only if WSL2 fails**: a cloud box is the next step
  solely when WSL2 cannot produce a clean test signal (section 1's
  definition of failure). If Linux gives clean frames, the engine work
  stays on the daemon and the VPS question never opens.
