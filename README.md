# edgehop

[![CI](https://github.com/Tjabooo/edgehop/actions/workflows/ci.yml/badge.svg)](https://github.com/Tjabooo/edgehop/actions/workflows/ci.yml) [![Release](https://img.shields.io/github/v/release/Tjabooo/edgehop?include_prereleases&sort=semver)](https://github.com/Tjabooo/edgehop/releases) [![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

**A software KVM for Wayland: push your cursor off a screen edge and your keyboard, mouse and touchpad drive the machine next to it.**

edgehop links two (or more) Linux machines running Hyprland. Move the pointer off the edge that faces the other machine and it appears on that machine's screens; your keyboard goes with it. Push it back out the same way to come home. Your laptop touchpad works on the desktop like a real touchpad: gestures, tap-to-click, two-finger scroll and all.

## Features

- **Edge crossing.** Configure which side each machine sits on. Multi-monitor layouts work; only the outer edges count, not the seams between your own monitors.
- **Real devices on the other side.** Each captured device is recreated through uinput with the same capabilities, so the far compositor (libinput) handles acceleration, touchpad gestures, keyboard layout and repeat itself. Nothing is re-interpreted on the way.
- **Swipes carry over.** A touchpad swipe that crosses the edge keeps going on the other screen.
- **Encrypted and authenticated.** Noise protocol (`NNpsk0`, ChaCha20-Poly1305, BLAKE2s) with a pre-shared key: only machines holding the key can connect, and keystrokes are never sent in the clear.
- **One link, both directions.** Only one machine has to accept connections, so a firewalled desktop can dial out to a laptop.
- **Safe by default.**
  - Hold <kbd>Left Ctrl</kbd>+<kbd>Left Alt</kbd>+<kbd>Esc</kbd> to snap back home.
  - Losing the link (4 s without a heartbeat) gives your input back.
  - Crossing waits until no key or button is held.
  - Power, sleep and suspend keys are never forwarded.
  - No crossing while a fullscreen window (a game) is focused.
- **Always know the way home.** While your input is away, a persistent notification says where it is and how to come back. The machine it arrives at shows a short hint too.
- **Remote view.** Driving a machine that isn't in front of you? Press a key and its screen shows full screen on the one you're at, following the cursor from monitor to monitor. Once on, it opens every time you cross over and closes when you come back. The video uses its own encrypted link, so it never delays your input.
- **Keybind switching.** `edgehop switch` jumps over without the edge, and the same bind brings you back.
- **No idle lock while away.** While you drive another machine, the local one holds a systemd idle inhibitor, so hypridle doesn't lock or suspend it under you.
- **Hotplug.** Devices plugged in later are picked up within a couple of seconds.

## Requirements

- Linux with Hyprland (it uses Hyprland's IPC to read the cursor position and screen layout, and to place the cursor). Tested on Hyprland 0.57 (Lua config); older Hyprland versions fall back to the classic `movecursor` dispatcher (untested).
- Read access to `/dev/input/event*` and write access to `/dev/uinput` for your user (see [Permissions](#permissions)).
- Rust 1.88+ to build.
- For remote view: [gpu-screen-recorder](https://git.dec05eba.com/gpu-screen-recorder/about/) on the machine being watched and [mpv](https://mpv.io) on the one watching (or your own commands, see [Remote view](#remote-view)).

## Install

On every machine:

```sh
git clone https://github.com/Tjabooo/edgehop && cd edgehop
./install.sh
```

The first run writes `~/.config/edgehop/config.toml` with a freshly generated key. Edit it (below), copy the **same `psk`** into the other machine's config, then:

```sh
systemctl --user enable --now edgehop
```

Re-run `./install.sh` after `git pull` to update; it restarts the service. To install a binary built on another machine of the same architecture: `EDGEHOP_BINARY=/path/to/edgehop ./install.sh`.

## Configuration

A laptop with the desktop's screens to its left:

```toml
# laptop: ~/.config/edgehop/config.toml
name = "laptop"
listen = "0.0.0.0:24801"
psk = "…same on both…"

[[peers]]
name = "desktop"
side = "left"        # desktop's screens are left of mine
```

```toml
# desktop: ~/.config/edgehop/config.toml
name = "desktop"
psk = "…same on both…"

[[peers]]
name = "laptop"
side = "right"
address = "192.168.1.30:24801"   # dial the laptop
```

| Key | Default | Meaning |
| --- | --- | --- |
| `name` | (required) | This machine's name; peers list it under exactly this name. |
| `listen` | none | Accept peers on this address (port defaults to 24801). Leave it out on a machine that only dials out. |
| `psk` | (required) | Shared 32-byte key, base64. `edgehop keygen` makes one. Keep the file `chmod 600`. |
| `release_keys` | `["KEY_LEFTCTRL","KEY_LEFTALT","KEY_ESC"]` | Held together while driving another machine: snap back home. Names are Linux `KEY_*` codes. |
| `block_when_fullscreen` | `true` | Don't cross over while the focused window is fullscreen. |
| `notify` | `true` | Desktop notifications on crossing: a persistent "Driving …" note on the machine whose input is away, and a short hint on the machine it arrived at, both saying how to get back. |
| `switch_hint` | none | Your `edgehop switch` keybind (e.g. `"Super+Alt+K"`), named in those notifications. |
| `ignore_devices` | `[]` | Device names (case-insensitive substring) never captured. |
| `[[peers]] name` | (required) | The peer's `name`. |
| `[[peers]] side` | (required) | `left`, `right`, `top` or `bottom`: where its screens are, seen from here. |
| `[[peers]] monitors` | `[]` (all) | Only these outputs' edges lead to the peer, e.g. `["DP-3"]`: arrivals land on that monitor and only its edge leads back. |
| `[[peers]] edge_delay_ms` | `0` | Keep pushing against the edge this long before crossing, so a quick flick to a bar on that edge doesn't throw you over. |
| `[[peers]] address` | none | `host[:port]` to dial. One side dialing is enough. |

`edgehop check` shows which devices will be captured, whether `/dev/uinput` is writable, your screen layout, and how far the cursor is from each peer's edge.

### Keybind and per-device feel

Bind `edgehop switch` to jump over without the edge. Pressed on the far machine (where your keys now go), the same bind hands control back:

```lua
-- Hyprland Lua config
hl.bind("SUPER + ALT + K", hl.dsp.exec_cmd(os.getenv("HOME") .. "/.local/bin/edgehop switch"))
```
```ini
# classic hyprland.conf
bind = SUPER ALT, K, exec, ~/.local/bin/edgehop switch
```

Bind `edgehop view` too, on every machine, to switch [remote view](#remote-view) on and off. While you drive another machine your keys land there, so its bind passes the toggle back to yours:

```lua
hl.bind("SUPER + ALT + L", hl.dsp.exec_cmd(os.getenv("HOME") .. "/.local/bin/edgehop view"))
```

Mirrored devices appear on the far machine as `edgehop <original name>` (Hyprland: `edgehop-<name>`), so you can give them their own settings. For example, a laptop touchpad on a desktop that uses a flat mouse profile:

```lua
hl.device({ name = "edgehop-asup1207:00-093a:3012-touchpad", sensitivity = 0.2, accel_profile = "adaptive" })
```

### Remote view

When you drive a machine whose screens you can't see (a desktop in another room, from a laptop), `edgehop view` shows its screen on the machine you're at:

- It streams the monitor the cursor is on, full screen. Move the cursor to another monitor and the view switches to it (about 0.15 s).
- The setting stays on: every crossing opens the view, coming back closes it. `edgehop view` (or `view on` / `view off`) changes it, also while you're over there.
- The watched machine encodes with `gpu-screen-recorder` (hardware H.264/HEVC, cursor included) and sends a raw stream over a second Noise link, dialed the same way as the main one. The viewer pipes it into `mpv` tuned for low latency (`--untimed`, no cache).

```toml
[view]
on = false          # start with remote view on
fps = 60
bitrate_kbps = 20000
codec = "h264"      # or "hevc"
hint = "Super+Alt+L"  # named in the notifications
```

`capture` and `player` override the commands; see `config.example.toml` for the placeholders. A stream that breaks (recorder missing, monitor gone) shows a notification saying why, and remote view stays on for the next crossing.

## Usage

```
edgehop run [--config FILE] [-v]   run the daemon (the systemd unit does this)
edgehop check                      devices, permissions, screen layout
edgehop status                     what the running daemon is doing
edgehop switch [PEER]              jump to a peer now (or back home)
edgehop view [on|off]              remote view: see the screen of the machine you drive
edgehop toggle | enable | disable  switch edge crossing on/off
edgehop release                    take local input back right now
edgehop keygen                     print a fresh key for `psk`
```

Logs: `journalctl --user -u edgehop -f`.

## Permissions

edgehop runs as your user. It needs to:

- read `/dev/input/event*`, normally group `input`: `sudo usermod -aG input $USER`, then log out and in;
- write `/dev/uinput`: many distros already allow the `input` group. If `edgehop check` says it isn't writable, add a udev rule and reload:

  ```sh
  echo 'KERNEL=="uinput", GROUP="input", MODE="0660", OPTIONS+="static_node=uinput"' | sudo tee /etc/udev/rules.d/80-uinput.rules
  sudo udevadm control --reload && sudo udevadm trigger
  ```

Membership in `input` lets a program read every keystroke, which is also exactly what a KVM has to do. Only add users you trust.

## How it works

1. **Watch.** edgehop reads your keyboards, mice and touchpads passively (the compositor still gets everything). When pointer motion heads toward a peer's side, it asks Hyprland where the cursor is, often near an edge and rarely when far from one.
2. **Cross.** When the cursor is pinned against the outer edge and still being pushed, edgehop grabs the devices (`EVIOCGRAB`, so the local compositor stops seeing them) and tells the peer `Enter(side, position along the edge)`. The peer moves its cursor to the matching spot on its facing edge.
3. **Drive.** Raw evdev events stream over the encrypted link. The peer replays them into its uinput copies of your devices, so its own libinput and Hyprland process them like local hardware.
4. **Return.** The peer watches its cursor. When it is pushed out through the edge facing you, it lifts every held key and touch and sends `Released(position)`. You ungrab, and your cursor appears at the same spot on your edge.

5. **Watch (remote view).** The driver sends `ViewStart`. The driven machine records the monitor under its cursor and streams it over its own encrypted link. Each time the cursor lands on another monitor it restarts the recorder there, and the player carries on with the new stream.

Devices are announced when the link comes up (and on hotplug), so the copies already exist and are configured by the time you cross.

## Limitations

- Hyprland only for now: the cursor and layout queries go through its IPC. Capture and injection are compositor-independent, so other compositors need only a small adapter.
- Clipboard sharing isn't built in.
- Remote view is video only (no sound) and needs both machines on the same protocol version (0.2+).
- A fullscreen game that locks the pointer can't be left through the edge; use the release keys or the `switch` bind.

## License

MIT
