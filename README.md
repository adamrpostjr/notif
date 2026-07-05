# notif

A lightweight notification daemon and control center for Wayland, built from
scratch in Rust. Implements the `org.freedesktop.Notifications` D-Bus spec,
so it's a drop-in replacement for dunst/mako. Optimized for Hyprland, portable
to any wlr-layer-shell compositor.

Zero-bloat by design: no UI frameworks, no tokio/calloop — just
[smol](https://github.com/smol-rs)-family async, [tiny-skia](https://github.com/RazrFalcon/tiny-skia)
for rendering, and [cosmic-text](https://github.com/pop-os/cosmic-text) for
font shaping (emoji/CJK/RTL all work out of the box).

## Features

- Full `org.freedesktop.Notifications` D-Bus server — urgency levels, actions,
  body markup, images/icons, `replaces_id`, transient/resident notifications.
- Popup toasts rendered directly onto Wayland layer-shell surfaces (no
  compositor-side blur — set `layerrule = blur, notif` in Hyprland instead).
- Fractional scaling done correctly (crisp at 1.25x, 1.5x, etc.).
- Notification history ring, kept from the first notification.
- Do Not Disturb mode — normal notifications are silently filed to history;
  critical notifications always break through.
- A notification-center panel (second layer surface) listing history, with
  per-entry dismiss and "clear all".
- `notifctl`, a small CLI to control the daemon over a local socket:
  dismiss/close, toggle DND, toggle the center panel, query history/status.
- Hot-reloading config file — edit `config.toml`, save, and the running
  daemon picks it up immediately.

## Architecture

Single process, single-threaded async (smol's `LocalExecutor` + `async-io`
reactor). Everything is a message over `async-channel` — no shared mutable
state, no `Mutex`. A central **core** state machine owns all notification
state and is the only source of truth; the D-Bus and Wayland layers are pure
translators in and pure projections out.

```
                 ┌────────────┐   DbusCmd    ┌──────────────┐
 D-Bus (zbus) ──▶│ notif-dbus │─────────────▶│              │
                 │            │◀─────────────│  notif-core  │
                 └────────────┘  DbusSignal  │ (state, IDs, │
 inotify ───────▶ ConfigEvent ──────────────▶│  expiry,     │
                 ┌────────────┐   UiEvent    │  history,    │
 Wayland ───────▶│  notif-wl  │─────────────▶│  DND)        │
 (layer-shell,   │  + render  │◀─────────────│              │
  seat, shm)     └────────────┘  UiCommand   └──────────────┘
                                                    ▲
                     notifctl ── IPC socket ────────┘
```

| Crate | Responsibility |
|---|---|
| `notif-types` | Shared message/data vocabulary. No I/O, no policy. |
| `notif-config` | Config loading, validation, inotify hot-reload. |
| `notif-dbus` | The `org.freedesktop.Notifications` D-Bus server (zbus). All hint parsing lives here. |
| `notif-core` | The state machine — notification lifecycle, expiry timers, history, DND, IDs. Synchronous, channel-free, 100% unit-testable. |
| `notif-render` | `Renderer` trait + a `tiny-skia`/`cosmic-text` implementation (toasts + center panel). |
| `notif-wl` | Wayland: layer-shell surfaces, seat/pointer input, shm buffers (via smithay-client-toolkit). |
| `notif-ipc` | The `notifctl` control socket protocol + server. |
| `notifd` | The daemon binary — wires everything together. |
| `notifctl` | CLI client for the control socket. |

## Installing

### Arch Linux (AUR)

```sh
yay -S notif-git
```

Or manually with the `PKGBUILD` in this repo:

```sh
git clone https://github.com/adamrpostjr/notif.git
cd notif
makepkg -si
```

This installs `notifd`, `notifctl`, and a systemd user unit
(`notifd.service`).

### Building from source

Requires a recent stable Rust toolchain.

```sh
cargo build --release --workspace
# binaries at target/release/notifd and target/release/notifctl
```

## Running

Claim the notification bus name directly:

```sh
notifd
```

Or via systemd (recommended — auto-restarts, starts with your graphical
session):

```sh
systemctl --user enable --now notifd.service
```

`notifd` will exit if another notification daemon (dunst, mako, ...) already
owns `org.freedesktop.Notifications` on the session bus — disable/mask it
first.

### Hyprland integration

Add to your Hyprland config so blur/rounding are handled compositor-side and
the panel doesn't grab focus:

```
layerrule = blur, notif
layerrule = blur, notif-center
layerrule = ignorezero, notif
```

## `notifctl`

```
Usage: notifctl <subcommand> [options]

Subcommands:
  dismiss-all         Dismiss all active notifications
  close <id>          Close notification by ID
  history [--json]    Show notification history
  clear-history       Clear notification history
  dnd                 Toggle do-not-disturb mode
  center              Toggle notification center panel
  status [--json]     Show daemon status
```

Bind whichever of these you want to a key, e.g. in Hyprland:

```
bind = $mainMod, N, exec, notifctl center
bind = $mainMod SHIFT, N, exec, notifctl dnd
```

## Customization

`notifd` reads `$XDG_CONFIG_HOME/notif/config.toml` (usually
`~/.config/notif/config.toml`) on startup, and hot-reloads it on save —
no restart needed. A missing file just means defaults. Point it elsewhere
with `notifd --config <path>`.

Every field below is optional; unset fields fall back to the default shown.

```toml
# Corner to anchor the notification stack.
# Options: top_left, top_right, bottom_left, bottom_right
anchor = "top_right"

# Margins from the screen edge, and gap between stacked notifications (px).
margin_x = 12
margin_y = 12
gap = 8

# Toast size bounds (px), and how many can be visible at once (others queue).
max_width = 400
max_height = 200
max_visible = 5

# Font.
font_family = "sans-serif"
font_size = 13.0

# Icon size (px).
icon_size = 48

# Wayland output name to render on. Omit/null = compositor-chosen (usually
# the focused output).
# output = "DP-1"

# How many notifications the history ring keeps (oldest dropped past this).
history_limit = 100

# Parse the small <b>/<i>/<u>/<a> markup subset in notification bodies.
body_markup = true

# Notification-center panel width (logical px, 1-8192).
center_width = 400

# Per-urgency appearance. Sections: [low], [normal], [critical].
[low]
background = "#1e1e2e"
foreground = "#cdd6f4"
border_color = "#313244"
border_width = 1
corner_radius = 8
default_timeout_ms = 5000   # 0 = never expire
ignore_timeout = false      # if true, always use default_timeout_ms

[normal]
background = "#1e1e2e"
foreground = "#cdd6f4"
border_color = "#89b4fa"
border_width = 1
corner_radius = 8
default_timeout_ms = 8000
ignore_timeout = false

[critical]
background = "#1e1e2e"
foreground = "#f38ba8"
border_color = "#f38ba8"
border_width = 2
corner_radius = 8
default_timeout_ms = 0      # never auto-expire critical notifications
ignore_timeout = true
```

A fully-commented copy of this file (kept in sync with the actual defaults by
a test) lives at `crates/notif-config/examples/config.toml`.

## Development

See [PLAN.md](PLAN.md) for the full architecture contract — module
responsibilities, message types, crate choices, and the invariants a change
must not break. [CLAUDE.md](CLAUDE.md) has the condensed version plus the
command cheat-sheet:

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
```

## License

MIT — see [LICENSE](LICENSE).
