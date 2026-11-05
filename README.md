# glidekm

Use your Windows PC's mouse and keyboard on a Steam Deck (or any SteamOS /
Linux box) **as if the Deck were an extra monitor**: push the cursor off the
edge of your desktop and it appears on the Deck at the matching height, push
it back and it returns. Works in **SteamOS Game Mode** (gamescope) and
Desktop Mode.

- Tiny: ~0.5 MB Windows exe, ~0.6 MB static Linux binary. No runtime deps, no async runtime.
- **Zero configuration:** start it on both machines and they find each other on the local network.
- Encrypted (Noise `NNpsk0`, ChaCha20-Poly1305); an optional passphrase restricts who can pair.
- Mouse + keyboard, plus a **shared text clipboard** (copy on one machine, paste on the other, both ways, up to 256 KB). On the Deck this uses `wl-copy`/`wl-paste` (wl-clipboard) or `xclip`/`xsel`, whichever is installed (`sudo pacman -S wl-clipboard` in Desktop Mode, after `sudo steamos-readonly disable`). No file transfer.

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
2. Run `glidekm-server.exe`. It runs in the notification area (tray). On first run its **settings window**
   opens:
   - **Arrangement** - drag the Steam Deck next to your monitors, like Windows display settings.
     The green line shows where the cursor can cross.
   - **Connection** - nothing to fill in: with the address left empty, the PC **finds the Deck
     automatically** on your network. Enter an address (or pick one with *Scan*) only if discovery can't
     reach it, e.g. across subnets or a VPN. *Passphrase* is optional (see below).
   - **Behavior** - pointer speed on the Deck, hotkeys (click a box and press the combination),
     *Start with Windows*.
   - **About** - automatic updates, *Check for updates*, config folder and log.
   - *Save* applies everything immediately; the header shows the live connection status.
3. Minimizing or closing the window hides it - glidekm keeps running in the tray. Tray icon: gray = not set
   up, amber = connecting, green = connected, blue = Deck in use. Left-click opens settings; right-click for
   *Switch to Deck* / *Quit*. Starting `glidekm-server.exe` again also just opens the settings.
4. Optional: right-click `install-autostart.cmd` → *Run as administrator* to start at logon with
   highest privileges (needed for the hooks to work while an elevated window such as Task Manager has
   focus). The simpler *Start with Windows* checkbox runs it unelevated.

### 2a. Steam Deck - Decky Loader plugin (easiest, works in Game Mode)

1. In Decky settings enable *Developer mode*, then *Developer → Install plugin from ZIP* and pick `glidekm-decky.zip`
   (copy it to the Deck first, e.g. to `~/Downloads`).
2. Open the Quick Access menu → glidekm → toggle *Enabled*. (A passphrase is optional.)
   The panel shows the Deck's IP to put in the PC config.

The plugin runs as root, so no udev setup is needed. Decky installs are updated by re-installing the zip.

### 2b. Steam Deck - Desktop Mode (no Decky)

In Desktop Mode open Konsole:

```bash
tar xzf glidekm-steamos-x86_64.tar.gz && cd glidekm-steamos && ./install.sh
```

This installs `glidekm-client` (the small background service, a systemd **user** service so it keeps
running in Game Mode) and **glidekm Settings** (app menu → Settings / Utilities), then opens the settings:

- **This Deck** - the address to enter on the PC (with a Copy button), plus `hostname.local`.
- **Status** - live: connected PC, input access, screen size, network. If the virtual mouse/keyboard
  can't be created, *Fix now…* asks for your password and allows it (or copy the one-line command).
  It also tells you when a PC tried to connect with a different passphrase.
- **Settings** - optional passphrase, UDP port, automatic or manual screen size, *Start automatically*.
- **Updates** - automatic updates and *Check for updates*.

*Save* applies immediately: the settings app writes `~/.config/glidekm/client.conf` and the running client
picks the change up within a second. Uninstall with `./install.sh --uninstall`.

Use either the Decky plugin or the Desktop Mode service, not both - they would compete for the same port
(glidekm Settings says so if it happens).

## How the PC finds the Deck

With no address configured, the PC broadcasts a small discovery probe every few seconds, and an
unconnected Deck also announces itself every 3 seconds (UDP 4242, local network only). The PC then
attempts the encrypted handshake with every Deck it found and connects to the first one that completes
it. If the Deck's IP changes, it's simply found again.

**Passphrase (optional).** Without one, both sides use a built-in default key: traffic is still
encrypted with fresh keys per session, but any glidekm Deck on the same network can pair with your PC.
On a shared network (dorm, office) set the same passphrase on both sides; then only matching Decks
can complete the handshake, and a Deck with a different one shows as "Found …, but the passphrase
doesn't match".

Discovery doesn't cross routers/VLANs; set the address manually there. On first start Windows may ask
whether glidekm may use the network - allow it on private networks.

## Updates

The PC server and the Desktop Mode client update themselves from this repository's
[GitHub Releases](../../releases) (checked about a minute after start, then every 6 hours, or with
*Check for updates*). An update is installed only when:

- the release manifest `glidekm-release.txt` carries a valid **Ed25519 signature** made with the project's
  release key (the public key is built into the apps: `crates/core/release-key.pub`), and
- the downloaded binary's SHA-256 matches the signed manifest.

Updates install only while the Deck is not in use, then the app restarts itself. Turn off *Install updates
automatically* to only be told about new versions.

**Maintainers:** the release workflow signs the manifest with the `GLIDEKM_SIGNING_KEY` repository secret
(*Settings → Secrets and variables → Actions*). To publish a release, create a tag `vX.Y.Z` (for example from
*Releases → Draft a new release*) after bumping `version` in `Cargo.toml`; the workflow builds, signs and
attaches everything. Without the secret, releases still publish but the apps won't auto-install them.
A new key pair can be made with `cargo run -p glidekm-release -- keygen secret.txt crates/core/release-key.pub`
(apps built with the old public key will then no longer accept updates).

## Configuration

Normally you use the settings window. It writes `glidekm-server.conf` next to the exe, which you can also
edit by hand (every key can be passed as `--key value` too; see `glidekm-server.conf.example`):

| key | default | meaning |
|---|---|---|
| `client` | empty = automatic | Deck address, `host[:port]`; empty finds it on the LAN |
| `passphrase` | empty = built-in default | optional shared secret, same on both sides |
| `side` | `right` | where the Deck sits: `right`, `left`, `top`, `bottom` |
| `offset` | `0` | shift the Deck screen along that edge (Windows px) |
| `scale` | `1.0` | Windows px per Deck px (>1 = slower cursor on the Deck) |
| `hotkey_switch` | `ctrl+alt+shift+s` | jump to/from the Deck |
| `hotkey_capture` | `ctrl+alt+shift+m` | toggle game (relative) mouse mode |
| `ignore_injected` | `false` | don't let software-injected moves trigger edge switching |
| `auto_update` | `true` | install signed updates automatically |

Command line: `--open-ui`, `--no-tray`, `--console` (show the log in a console), `--log-file FILE`
(default `glidekm-server.log` next to the exe), `--verbose`.

Crossing only happens where a Windows monitor actually touches the Deck's
edge, like a real monitor arrangement; elsewhere the edge is a wall.

`~/.config/glidekm/client.conf` (written by glidekm Settings): `passphrase`, `port` (4242), `auto_update`, and
optional `width`/`height` to override the auto-detected logical screen size (the Deck panel's rotation
and docked external displays are handled automatically). Client command line: `--dry-run`, `--verbose`,
`--status-file FILE` (JSON status, used by the Decky plugin).

## Troubleshooting

- **Nothing happens at the edge**: the tray icon must be green (see the status line in the settings window).
  If you set a passphrase, it must be the same on both sides (or empty on both). UDP 4242 must be reachable on the Deck.
- **Cursor lands on the wrong spot on the Deck when docked with two displays (Desktop Mode)**: set the
  screen size manually in glidekm Settings to the combined desktop size.
- **Laggy on Wi-Fi**: disable Deck Wi-Fi power saving (`sudo iw dev wlan0 set power_save off`) or use ethernet.
- **Windows cursor stays invisible after a crash**: start and stop the server once (it restores cursors at start),
  or Settings → Mouse pointer → reapply the scheme.
- Ctrl+Alt+Del and UAC prompts (secure desktop) can't be captured by any user-mode program; use the hotkey/edge to come back.

## Building

```bash
cargo test
cargo build --release -p glidekm-client --target x86_64-unknown-linux-musl   # SteamOS client (static)
cargo build --release -p glidekm-settings                                     # SteamOS settings app
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
crates/core            wire format, Windows->evdev keymap, Noise session, config, logging,
                       client<->settings file IPC, signed self-update
crates/client          background client: uinput devices, screen detection, state machine, updater
crates/settings-deck   glidekm Settings for SteamOS / Linux (egui)
crates/server          engine (platform independent state machine), layout math, networking,
                       Windows hooks, tray icon, native Win32 settings window, WinHTTP updater
crates/release-tool    release manifest signing (used by CI)
decky/          Decky Loader plugin (Python backend + React panel)
dist/           install scripts and example configs
```
