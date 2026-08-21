# rustdesk-mvp

A minimal, self-hosted remote-desktop tool in Rust. No third-party service,
no account, no cloud — three small binaries you run yourself.

- **`rd-host`** — runs on the machine you want to control (your laptop at home).
  Streams the screen and injects mouse/keyboard input it receives. Double-click
  it and it just starts hosting immediately — no window, just a system tray
  icon (right-click it for the address/password, or `--window` if you want
  the old visible Start/Stop window instead). See "Running rd-host" below.
- **`rd-client`** — runs on the machine you have with you. A small GUI app:
  enter an address and password and click Connect. Shows you the remote
  screen and forwards your mouse/keyboard to it.
- **`rd-relay`** — an optional rendezvous server. Run this on any machine
  with a public IP (a cheap VPS is the usual choice) and both `rd-host` and
  `rd-client` connect *out* to it, so neither one needs a forwarded router
  port or a known IP. `rd-host` registers and gets back a short **code**;
  give that code to `rd-client` and it finds the host through the relay.
  This is the "AnyDesk-style" flow.

`rd-client` connects to a plain address:port + password — it doesn't have a
relay/code field. So "connecting" always comes down to getting an
address:port that reaches `rd-host` to type into the client. A few ways to
get one:

| | Needs | Best for |
|---|---|---|
| **Direct** (`rd-host listen`) | The host's IP:port, and a router port-forward if you're off the home network | Same LAN, or you're comfortable with router config |
| **ngrok tunnel** (`rd-host listen` + `ngrok tcp`) | Nothing on the connecting side; `ngrok` installed on the host | A quick one-off session, no router config — see the dedicated section below |
| **SSH tunnel** | SSH access into the host's network | Real confidentiality, since it also encrypts the session — see "Security" |

`rd-relay`/`rd-host relay`/`rd-host with-relay` (the short-code,
"AnyDesk-style" rendezvous flow) still exist in this repo and work at the
protocol level, but the current `rd-client` GUI has no way to redeem a
code — it's a **known gap**, not yet reachable end-to-end. Use one of the
paths above until that's wired back up. This is an MVP, not a hardened
product either way — read the "Security" section before connecting over
the open internet.

## Requirements to build

- Rust (`rustup.rs`, or your OS package manager) — any reasonably recent
  stable toolchain (1.70+).
- **Linux only:** X11 dev headers, since `rd-host` uses X11 APIs for screen
  capture and input injection:
  ```
  sudo apt-get install -y libxcb1-dev libxcb-randr0-dev libxcb-shm0-dev \
      libxrandr-dev libx11-dev libxi-dev libxtst-dev libxdo-dev pkg-config
  ```
- **Windows / macOS:** no extra system packages needed — `cargo build` is
  enough (screen capture and input injection use native OS APIs there).

## Build

```
cargo build --release --workspace
```

This produces:
- `target/release/rd-host` (or `rd-host.exe` on Windows)
- `target/release/rd-client` (or `rd-client.exe` on Windows)
- `target/release/rd-relay` (Linux is the natural choice for this one — it
  just needs to run headless on a VPS)

Build each binary **on the OS it will run on** — this project doesn't set up
cross-compilation. Build `rd-host` on your laptop's OS, `rd-client` on
whatever machine you're using while traveling, and `rd-relay` on whatever
server you're using as the rendezvous point.

## Setting up the relay (do this once, ahead of time)

You need one machine with a public IP that both ends can reach — any cheap
VPS (Linode, DigitalOcean, a spare AWS Lightsail instance, etc.) works.
This is infrastructure you own and run — not a third-party remote-desktop
service, just a plain box relaying bytes.

On that box:
```
cargo build --release -p rd-relay
./target/release/rd-relay            # listens on 0.0.0.0:5910 by default
```
Open port `5910` (or whatever port you pass as an argument) in that host's
firewall/security-group settings. That's the only port that needs to be
open anywhere for the relay flow — no router configuration on your home
network at all.

Do this step **before** the emergency, so on the day it's just "run
`rd-host relay <that-server>`."

### If the relay and the host are the same machine

You don't need two terminals for that. `rd-host with-relay` runs the relay
inside its own process and registers with it over loopback, so one command
gets you both, and one Ctrl-C stops both:

```
./target/release/rd-host with-relay              # relay on 5910, random password
./target/release/rd-host with-relay --port 5911 --password hunter2
```

It prints the code and password the same way `relay` mode does, plus the
exact `rd-client` line to use.

Be clear on what this does and doesn't buy you: because the relay is
listening on the host machine, the connecting side still needs **this
machine's IP address** and port `5910` still has to be reachable — the same
firewall and router-forwarding work that direct `listen` mode needs. Putting
the relay on the host doesn't get you out of that; only a relay on a
separate box with a public IP does. This mode is for exercising the relay
code path (codes, matching, byte-piping) on a single machine — for a real
"just give them a code" setup, run `rd-relay` on a VPS and use plain `relay`
mode.

## Quick one-off session: host + ngrok tunnel (no VPS, no router config)

If you just need to reach one machine for a single session and don't want
to stand up a relay VPS or touch your router, tunnel `rd-host`'s direct
port through [ngrok](https://ngrok.com) instead. This needs the `ngrok`
CLI and a (free) ngrok account, set up **on the host machine only** — the
connecting side needs nothing extra beyond `rd-client` itself.

On the host, two things need to be running at once:

```
./target/release/rd-host listen
```
This prints a `Port` (5900 by default) and a `Password` — leave it running.

In a second terminal, on the same host machine:
```
ngrok tcp 5900
```
(use whatever port `rd-host` printed, if not the default). ngrok prints a
`Forwarding` line that looks like:
```
Forwarding    tcp://2.tcp.eu.ngrok.io:18802 -> localhost:5900
```

Give the connecting side that `host:port` (e.g. `2.tcp.eu.ngrok.io:18802`)
and the password `rd-host` printed. In `rd-client`, put the ngrok address
in the **Address** field and the password in **Password**, then click
**Connect** — same as any direct connection, just tunneled.

Keep both terminal windows open for the whole session — closing either one
(the `rd-host` window or the `ngrok` window) ends it immediately.

Things to know about this path:
- **The ngrok address changes every time you restart the tunnel** (free
  tier hands out a new random `host:port` each run). Re-read the
  `Forwarding` line each session rather than reusing an old address.
- This routes your session through ngrok's infrastructure. ngrok wraps the
  connection in TLS on their end, but the rd-protocol traffic inside it is
  still the same unencrypted MVP protocol described in "Security" below —
  ngrok (like a relay operator) is a third party that could observe it if
  it wanted to. Fine for an occasional one-off remote-help session; if this
  becomes routine, prefer your own relay VPS or an SSH tunnel instead.
- ngrok's free tier also has connection-count and session-length limits —
  if the tunnel drops after a few hours, just restart `ngrok tcp 5900` and
  give the new address to whoever's connecting.

## Running rd-host

Double-click `rd-host.exe` (or run it with no arguments) and it starts
hosting immediately — binds the port, generates a password, and launches
an ngrok tunnel automatically if `ngrok` is installed. **No window opens
at all** — just a small icon in the system tray (it may land in the
hidden/overflow tray icons the first time, which is normal Windows
behavior for any new tray icon; drag it out if you want it always
visible). Right-click it for:
- **Copy Address** — the tunnel address once ready, or the LAN address
  meanwhile/if ngrok isn't available.
- **Copy Password**
- **Open Log File** — see below.
- **Quit** — stops hosting and the tunnel cleanly.

Double-clicking the icon shows a quick status popup (addresses + password)
without needing the full window.

Once the tunnel finishes starting (or gives up), the address and password
are also appended to **`rd-host-connections.log`**, next to the exe, so
they're not lost just because there's no window showing them:
```
[2026-08-21 09:24:12] rd-host is running
  LAN address: 192.168.100.3:5900
  Tunnel:      7.tcp.eu.ngrok.io:24527
  Password:    ujtL6oP4X6
```

If you want the old interactive window instead (a Start/Stop button you
click yourself), run `rd-host --window`. CLI modes (`listen`/`relay`/
`with-relay`, for scripted or headless use e.g. on a VPS) are unchanged.

## Getting your kid to install it on your laptop

1. Get the source to them (USB drive, email, cloud link — whatever's fastest).
2. Have them install Rust from https://rustup.rs (one installer, a few
   minutes) if it isn't already on the machine.
3. Have them open a terminal in the project folder and run:
   ```
   cargo build --release -p rd-host
   ```
4. Have them double-click `target/release/rd-host.exe` (or run it with no
   arguments) — it starts hosting on its own, no further action needed on
   their end. Have them right-click the new tray icon, choose **Open Log
   File**, and read you the address and password over the phone (see
   "Connecting" below for what you then type into `rd-client`). If ngrok
   is installed on their machine, the logged address already works from
   anywhere — no port forwarding needed.

If a full Rust install is too slow for the emergency, it's worth having
your kid enable your OS's built-in remote desktop (Windows Remote Desktop /
macOS Screen Sharing) as a stopgap while this builds — those are OS-native,
not third-party services either.

## Connecting

Build `rd-client` on your end (`cargo build --release -p rd-client`), then
run `./target/release/rd-client` (double-click `rd-client.exe` on Windows —
no arguments needed). A small dark-themed window opens with **Address**
and **Password** fields and a **Connect** button.

Fill in whatever address:port actually reaches `rd-host` — its LAN IP:port
for direct/same-network, its port-forwarded public IP:port, `127.0.0.1` and
the forwarded local port if you're going over an SSH tunnel, or the
`host:port` ngrok printed (see the ngrok section above) — plus the password
`rd-host` printed, and click **Connect**.

Example, connecting through the ngrok tunnel from earlier:
```
Address:  2.tcp.eu.ngrok.io:18802
Password: aB3xQ9kLp2
```

Once connected, the remote screen fills the main pane; move your mouse and
type inside it to control the remote machine. Click **Disconnect**, or
close the window, to end the session.

### Blank host screen (privacy screen)

Next to Disconnect there's a **Blank host screen** toggle. Turning it on
shows a full-screen fake "Working on updates" Windows Update screen (spinner,
slow-crawling percentage, "Don't turn off your computer") on the *host's own
physical monitor*, with the real mouse cursor hidden too — useful for
setting up someone's machine remotely without anyone nearby seeing what's
on screen or watching the cursor move on its own. You (the client) keep
seeing and controlling the real desktop the whole time; only the host's
local screen is covered.

This works via a Windows API
([`SetWindowDisplayAffinity`](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-setwindowdisplayaffinity)
with `WDA_EXCLUDEFROMCAPTURE`) that excludes a window from screen capture
entirely, so `rd-host`'s own capture keeps seeing the real desktop
underneath, plus a temporary swap of the system cursor for a blank one
(the cursor is a separate compositor layer, unaffected by the window
exclusion above, so it needs its own fix). Needs **Windows 10 version 2004
or later** on the host; on older Windows or on macOS/Linux the toggle will
show an error instead of silently doing nothing. Turning the host off, or
the connection dropping for any reason, always clears the overlay and
restores the real cursor — neither can be left stuck.

## Security — read this

This MVP protocol is **not encrypted**, in either direct or relay mode. A
password gates the connection (hashed before comparison, so the plaintext
password itself isn't sent — but the hash, plus your screen frames and
keystrokes, cross the network in the clear). The same goes for clipboard
content if you turn on **Share clipboard** — including anything sensitive
you copy, like a password from a password manager. That's fine on a trusted LAN
or over a VPN. It is **not** fine over the open internet — anyone who can
observe that traffic (including, in relay mode, whoever operates the relay
box — hopefully just you) can see your screen and your keystrokes.

**If you need real confidentiality, tunnel over SSH** instead of relying on
either mode's raw TCP. If the laptop has SSH access set up:
```
ssh -L 5900:localhost:5900 you@your-home-ip
```
then in `rd-client`, Address `127.0.0.1:5900`, Password whatever `rd-host`
printed. This encrypts the whole session via SSH; it only requires the SSH
port to be reachable instead of the remote-desktop port itself, and
sidesteps the relay entirely.

Other things worth knowing about this MVP:
- Only one controlling connection is handled at a time, per host.
- Relay codes are freed as soon as the host disconnects, but there's no
  rate limiting on connection attempts or lockout after failed passwords —
  don't leave `rd-host` running longer than you need it, in either mode.
- The relay never decrypts anything (there's nothing to decrypt) — it just
  copies bytes once a code is matched. It can't tell an authorized session
  from an unauthorized one beyond that match, so treat the relay box itself
  as sensitive infrastructure (keep its OS patched, restrict SSH access to it).
- Frame quality is fixed at JPEG quality 60 and ~12 fps to keep bandwidth
  reasonable; tune `bgra_to_jpeg`'s quality value and the frame-rate
  constant in `host/src/main.rs` if you want higher fidelity.

## Project layout

```
protocol/   shared message types + wire framing, incl. the relay handshake
host/       rd-host binary: screen capture (scrap) + input injection (enigo)
client/     rd-client binary: dark-themed GUI (eframe/egui), JPEG decode (image)
relay/      rendezvous server (code <-> host matching, then a blind byte pipe).
            The logic is a lib so it can run either as the standalone rd-relay
            binary or in-process inside rd-host's `with-relay` mode.
```

## Known limitations (it's an MVP)

- Primary display only (no multi-monitor picker).
- No clipboard sync, no file transfer.
- No audio.
- No reconnect-on-drop — if the connection dies, just click **Connect**
  again in `rd-client`.
- The `rd-client` GUI has no relay/code field yet, so `rd-relay`/`rd-host
  relay`/`with-relay` aren't reachable end-to-end from it right now — see
  the note near the top of this README.
- One client at a time per host, in either mode.
- Blank host screen is Windows-only (needs version 2004+) — the toggle
  reports an error on older Windows, macOS, and Linux rather than doing
  anything.
- Windows/macOS input-injection permissions: macOS will need you to grant
  the terminal (or the built binary) **Accessibility** and **Screen
  Recording** permissions in System Settings the first time you run
  `rd-host`.
