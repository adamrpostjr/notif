# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

`notif` — a notification daemon + (future) notification center for Wayland, built from scratch in Rust. Optimized for Hyprland, portable to any wlr-layer-shell compositor via strict adherence to the org.freedesktop.Notifications D-Bus spec. Zero-bloat: no UI frameworks, smol-family async only (no tokio, no calloop — their absence from Cargo.lock is a hard invariant).

**PLAN.md is the architecture contract.** Module responsibilities, message types, crate choices, review criteria, and spec gotchas live there — read it before structural changes. Phase 2 (notif-ipc socket, notifctl, history/control-center panel) is designed but unbuilt; the seams for it already exist (`IpcCmd`, core's history ring).

## Commands

```sh
cargo build --workspace                                  # build everything
cargo test --workspace                                   # all unit + golden tests
cargo test -p notif-core                                 # one crate
cargo test -p notif-core test_body_click                 # one test by substring
cargo test -p notif-render -- --ignored                  # CJK/emoji shaping test (needs system fonts)
cargo clippy --workspace --all-targets -- -D warnings    # must be clean (gate)
cargo fmt --check                                        # must be clean (gate)
cargo run --release -p notifd                            # run the daemon (fails if another daemon owns the name)
```

Manual smoke scripts (all run on an ISOLATED bus via `dbus-run-session` — never against the user's real session bus, which likely has a live notification daemon holding the well-known name):

```sh
bash crates/notif-dbus/tests/manual/dbus_smoke.sh        # D-Bus interface conformance
bash crates/notif-core/tests/manual/core_smoke.sh        # headless dbus+core, expiry via notify-send
bash bin/notifd/tests/manual/shutdown_smoke.sh           # SIGINT/SIGTERM exit <2s
bash bin/notifd/tests/manual/e2e_smoke.sh                # full daemon rendering on the live compositor
```

The e2e script inherits WAYLAND_DISPLAY, so toasts render on the real compositor even under an isolated bus; verify visually with `grim` screenshots and `hyprctl layers | grep notif`.

## Architecture

Single process, single-threaded async (async-executor `LocalExecutor` + async-io reactor). Hub-and-spoke around **notif-core**: all subsystems communicate via typed messages over `async-channel`; no shared mutable state, no Mutex. The message vocabulary lives in **notif-types** (`DbusCmd`, `DbusSignal`, `UiCommand`, `UiEvent`, `ConfigEvent`, `IpcCmd`) and is the frozen contract between crates — think hard before changing it.

```
zbus ──▶ notif-dbus ──DbusCmd──▶ notif-core ◀──UiEvent── notif-wl ◀── Wayland
              ▲                   │  ▲                        │
              └────DbusSignal─────┘  └───UiCommand::Sync─────▶│──▶ notif-render
         inotify ──ConfigEvent──▶ core                        (Renderer trait)
```

Key invariants that span multiple files:

- **The UI is a pure projection.** Core owns all state (active list, waiting queue, history ring) and pushes full `UiCommand::Sync(Arc<[DisplayNotification]>)` snapshots after every mutation. notif-wl never decides visibility or interprets notification semantics — it reports intent (`BodyClicked`, `DismissRequested`, `HoverChanged`) and core decides (e.g. body click → invoke the `"default"` action if present, else dismiss).
- **Core is a pure state machine.** `Core<C: Clock>` is synchronous and channel-free; the async `run()` is a thin shell with ONE expiry timer re-armed at `min(deadline)` (never per-notification tasks). Tests drive `Core<MockClock>` directly.
- **IDs are allocated by core only** (never notif-dbus) — this is what makes `replaces_id` correct. All D-Bus hint parsing lives in notif-dbus/src/hints.rs and nowhere else.
- **Boundary rules (enforce in review):** notif-wl must not depend on zbus; notif-core must not depend on wayland-*/tiny-skia; only notif-types is universal. notif-types holds pure data only (config structs included) — I/O and validation live in notif-config.
- **Renderer trait seam** (notif-render/src/lib.rs): `measure()` returns LOGICAL dimensions but hit_regions in BUFFER pixels; notif-wl hit-tests pointer coords using `SurfaceState::layout_scale` (the scale the layout was measured at), not the possibly-newer `scale`. SkiaRenderer caches shaped text per frame keyed by (items, scale, config-hash) with hover deliberately outside the key — hover-only redraws must shape nothing (enforced by `shape_count_regression`).

## Hard gates (workspace-enforced, will fail CI-style review)

- Deny lints: `clippy::unwrap_used`, `clippy::expect_used`, `clippy::indexing_slicing` (allowed in `#[cfg(test)]`); `#![forbid(unsafe_code)]` everywhere. Pixel loops use `.get()`/`chunks_exact_mut`, never `[]`.
- Errors: thiserror enums in lib crates, anyhow only in bins. Peer misbehavior (malformed hints, bad config, protocol oddities) is logged-and-degraded, never fatal; only startup failures may exit.
- No new dependencies without checking PLAN.md's approved crate table.
- Golden-image tests (crates/notif-render/tests/) compare byte-exact against committed PNGs using bundled DejaVu test fonts. If a rendering refactor isn't supposed to change output, the PNGs must not change — fix the code, never regenerate the goldens to make a test pass.

## Wayland/spec gotchas (cost real debugging time; details in PLAN.md §Risks)

- SCTK 0.20 auto-acks layer-surface configures before invoking the handler — a manual `ack_configure` is a fatal double-ack protocol error on Hyprland. Never attach a buffer before the first configure.
- Fractional scaling: render at `ceil(logical × scale)` buffer px + `wp_viewport.set_destination(logical)`; never `set_buffer_scale` for fractional. wl_shm ARGB8888 is BGRA byte order on little-endian; the RGB(A)→premultiplied-BGRA swizzle lives in exactly one place per pipeline (`premultiply_rgba`).
- `expire_timeout`: `-1` = per-urgency default, `0` = never. Signal ordering: `ActionInvoked` before `NotificationClosed` for the same interaction. `CloseNotification` on an unknown id succeeds silently (dunst/mako behavior, deliberate).
- Layer surface namespace is `"notif"` (stable — users target it with Hyprland `layerrule = blur, notif`). Blur is the compositor's job: emit genuinely transparent premultiplied pixels, never render blur daemon-side.
- Testing environment: this machine runs Hyprland (`hyprctl`, `grim` available); sway is NOT installed, so the cross-compositor portability check is deferred — flag Wayland-touching changes as unverified-on-sway.
