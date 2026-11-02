# glidekm

Use your Windows PC's mouse and keyboard on a Steam Deck (or any SteamOS /
Linux box) **as if the Deck were an extra monitor**: push the cursor off the
edge of your desktop and it appears on the Deck at the matching height, push
it back and it returns. Works in **SteamOS Game Mode** (gamescope) and
Desktop Mode.

- Tiny: ~0.5 MB Windows exe, ~0.6 MB static Linux binary. No runtime deps, no async runtime.
- Encrypted + authenticated (Noise `NNpsk0`, ChaCha20-Poly1305) with a shared passphrase.
- One job only: mouse + keyboard. No clipboard, no file transfer.

## How it works

```
Windows (server)                                   SteamOS (client)
low-level hooks ─► engine (one global cursor) ─UDP─► uinput: absolute pointer  (desktop / Steam UI)
raw input                                            uinput: relative mouse    (games, "capture" mode)
                                                     uinput: keyboard
```

- The server keeps a single cursor position across your Windows monitors and a
  virtual "Deck monitor" placed on one side of your desktop. While the cursor
  is on the Deck, Windows input is blocked locally, the Windows cursor is
  hidden, and the Deck receives **exact absolute positions** - no drift.
- The client injects input through `/dev/uinput`, below the compositor, which
  is why it works under gamescope (where Lan Mouse/Synergy-style compositor
  protocols do not).
- **Game mouse (capture) mode**: games that lock the mouse need relative
  motion. Press `Ctrl+Alt+Shift+M` while on the Deck to toggle; the server
  then sends raw, unaccelerated mouse counts. Press again for normal mode.
- Safety: a heartbeat every 200 ms carries the set of held keys; the client
  releases anything stale, and releases everything if the PC goes silent for
  1 s. Keys held while switching are released on the side they were pressed.

## Install

Download from the [Releases](../../releases) page (or the latest CI run's artifacts).

### 1. Windows (server)

1. Unzip `glidekm-windows-x86_64.zip` somewhere permanent, e.g. `C:\Tools\glidekm`.
2. Edit `glidekm-server.conf`: set `client` (Deck IP or `steamdeck.local`), `passphrase`, and `side`.
3. Run `glidekm-server.exe` to test (a console shows the log).
4. Optional: right-click `install-autostart.cmd` → *Run as administrator* to start it at logon with
   highest privileges (needed for the hooks to work while an elevated window such as Task Manager has focus).

### 2a. Steam Deck - Decky Loader plugin (easiest, works in Game Mode)

1. In Decky settings enable *Developer mode*, then *Developer → Install plugin from ZIP* and pick `glidekm-decky.zip`
   (copy it to the Deck first, e.g. to `~/Downloads`).
2. Open the Quick Access menu → glidekm → enter the same passphrase → *Save* → toggle *Enabled*.
   The panel shows the Deck's IP to put in the PC config.

The plugin runs as root, so no udev setup is needed.

### 2b. Steam Deck - Desktop Mode (no Decky)

In Desktop Mode open Konsole:

```bash
tar xzf glidekm-steamos-x86_64.tar.gz && cd glidekm-steamos && ./install.sh
```

This installs `~/.local/bin/glidekm-client`, writes `~/.config/glidekm/client.conf`,
and enables a systemd **user** service, so it also runs after you switch back to Game Mode.
If `/dev/uinput` is not accessible to your user, the script installs a one-line udev rule
(needs `sudo`; set a password first with `passwd`). Uninstall with `./install.sh --uninstall`.

## Configuration

`glidekm-server.conf` (next to the exe; every key can also be passed as `--key value`):

| key | default | meaning |
|---|---|---|
| `client` | - | Deck address, `host[:port]` |
| `passphrase` | - | shared secret, same on both sides |
| `side` | `right` | where the Deck sits: `right`, `left`, `top`, `bottom` |
| `offset` | `0` | shift the Deck screen along that edge (Windows px) |
| `scale` | `1.0` | Windows px per Deck px (>1 = slower cursor on the Deck) |
| `hotkey_switch` | `ctrl+alt+shift+s` | jump to/from the Deck |
| `hotkey_capture` | `ctrl+alt+shift+m` | toggle game (relative) mouse mode |
| `log_file`, `verbose`, `background` | | logging / detach console |
| `ignore_injected` | `false` | don't let software-injected moves trigger edge switching |

Crossing only happens where a Windows monitor actually touches the Deck's
edge, like a real monitor arrangement; elsewhere the edge is a wall.

`~/.config/glidekm/client.conf`: `passphrase`, `port` (4242), and optional
`width`/`height` to override the auto-detected logical screen size (the Deck
panel's rotation and docked external displays are handled automatically).

## Troubleshooting

- **Nothing happens at the edge**: check the server log says `client online`. Both sides need the same passphrase;
  a mismatch logs `handshake failed`. UDP 4242 must be reachable on the Deck.
- **Cursor lands on the wrong spot on the Deck when docked with two displays (Desktop Mode)**: set `width`/`height`
  on the client to the combined desktop size.
- **Laggy on Wi-Fi**: disable Deck Wi-Fi power saving (`sudo iw dev wlan0 set power_save off`) or use ethernet.
- **Windows cursor stays invisible after a crash**: start and stop the server once (it restores cursors at start),
  or Settings → Mouse pointer → reapply the scheme.
- Ctrl+Alt+Del and UAC prompts (secure desktop) can't be captured by any user-mode program; use the hotkey/edge to come back.

## Building

```bash
cargo test
cargo build --release -p glidekm-client --target x86_64-unknown-linux-musl   # SteamOS
cargo build --release -p glidekm-server --target x86_64-pc-windows-gnu       # Windows (needs mingw-w64)
cd decky && pnpm install && pnpm build                                        # Decky frontend
```

Testing without hardware:

```bash
glidekm-client --passphrase x --dry-run          # logs instead of injecting
glidekm-server --simulate --client 127.0.0.1 --passphrase x   # (non-Windows) scripted input
```

CI also runs the real Windows binary under Wine, driving its hooks with `SendInput` (`crates/server/examples/inject.rs`).

## Layout

```
crates/core     wire format, Windows->evdev keymap, Noise session, config, logging
crates/client   uinput devices, screen detection, client state machine
crates/server   engine (platform independent state machine), layout math, networking, Windows hooks
decky/          Decky Loader plugin (Python backend + React panel)
dist/           install scripts and example configs
```
