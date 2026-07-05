# Master Project Plan — `notif`: Rust Wayland Notification Daemon + Notification Center

## Context

Greenfield project in an empty directory (`/home/apost/Documents/Projects/notif`). Goal: a highly customizable notification daemon + (later) notification center, built from scratch in Rust for Arch Linux / Wayland, optimized for Hyprland but portable to any wlr-layer-shell compositor via strict adherence to the `org.freedesktop.Notifications` D-Bus spec. Zero-bloat: no UI frameworks; small curated crates. Implementation will be delegated to smaller LLM agents via tightly scoped prompts, so modules need crisp single responsibilities and explicit message-passing interfaces.

## Locked decisions (confirmed with user)

1. **Rendering**: CPU rasterization — tiny-skia into `wl_shm` buffers on layer-shell surfaces, behind a `Renderer` trait so a GPU backend can be added later. Blur/shadows on Hyprland via compositor `layerrule`, not daemon-side.
2. **Phase-1 scope**: popup toasts only; state manager retains history from day one so the control-center panel later is purely a rendering feature.
3. **Runtime**: smol family (`async-io`/`async-executor`/`async-channel`/`futures-lite` individual crates, not the umbrella). No tokio anywhere in the lock file.

## Architecture Blueprint

Single daemon process, single-threaded async (`LocalExecutor` + async-io reactor), hub-and-spoke around a **Core state manager task**. All subsystems communicate via typed messages over `async-channel`; no shared mutable state crosses module boundaries. Core is the only owner of notification state. **The UI is a pure projection**: Core pushes full `UiCommand::Sync(Arc<[DisplayNotification]>)` snapshots after every mutation; the UI never decides visibility, never mutates data, never touches D-Bus.

```
                 ┌────────────┐   DbusCmd    ┌──────────────┐
 D-Bus (zbus) ──▶│ notif-dbus │─────────────▶│              │
                 │            │◀─────────────│  notif-core  │
                 └────────────┘  DbusSignal  │ (state, IDs, │
 inotify ───────▶ ConfigEvent ──────────────▶│  expiry,     │
                 ┌────────────┐   UiEvent    │  history)    │
 Wayland ───────▶│  notif-wl  │─────────────▶│              │
 (layer-shell,   │  + render  │◀─────────────│              │
  seat, shm)     └────────────┘  UiCommand   └──────────────┘
                                                    ▲
                 notifctl (Phase 2) ── IPC socket ──┘
```

### Cargo workspace layout

```
notif/
├── Cargo.toml              # [workspace]: members, shared deny-lints, pinned versions
├── crates/
│   ├── notif-types/        # shared vocabulary — zero logic, serde only
│   ├── notif-config/       # Config (serde/toml), loader, validator, inotify hot-reload
│   ├── notif-dbus/         # org.freedesktop.Notifications server (zbus)
│   ├── notif-core/         # state manager: the hub
│   ├── notif-render/       # Renderer trait + tiny-skia/cosmic-text impl
│   ├── notif-wl/           # everything Wayland (SCTK, layer-shell, input, shm pools)
│   └── notif-ipc/          # Phase-2 control socket (stub crate; IpcCmd enum defined now)
└── bin/
    ├── notifd/             # wiring + executor only (~150 lines)
    └── notifctl/           # Phase 2
```

### Module contracts

**`notif-types`** — all cross-module types; no I/O, no policy. Key types:
- `Notification { id, app_name, app_icon, summary, body, actions: Vec<Action>, urgency, expire_timeout: Timeout{Default|Never|Millis(u32)}, image: Option<ImageSource{Data(RawImage)|Path|Icon}>, transient, resident, category, desktop_entry, created_at, raw_hints: HashMap<String, OwnedValue> }`
- `RawImage { width, height, rowstride, has_alpha, bits_per_sample, channels, data }` (spec `(iiibiiay)`)
- `CloseReason { Expired=1, Dismissed=2, CloseCall=3, Undefined=4 }`
- Channel enums: `DbusCmd { Notify{n, replaces_id, reply: oneshot<u32>}, Close{id, reply} }`; `DbusSignal { NotificationClosed{id, reason}, ActionInvoked{id, action_key}, ActivationToken{...} }`; `UiCommand { Sync(Arc<[DisplayNotification]>), ConfigChanged(Arc<Config>), Shutdown }`; `UiEvent { DismissRequested(u32), ActionInvoked{id, key}, HoverChanged{id, hovered}, OutputsChanged }`; `ConfigEvent(Arc<Config>)`.

**`notif-config`** — `Config::load(path)`, defaults, validation; `watch(path, tx)` inotify task watching the config *directory* (editors rename-swap); invalid reloads are logged and never sent. Surface: anchor corner, margins/gap, max width/height, max visible, per-urgency style (colors, border, radius, default timeout, ignore_timeout), font, icon size, output selection, body-markup toggle. Must not apply config to anything.

**`notif-dbus`** — owns the session-bus connection; claims `org.freedesktop.Notifications` (fail hard if taken, no queueing); implements the full interface; ALL hint parsing lives here (urgency byte, `image-data`/`image_data`/`icon_data` precedence + byte layout, transient/resident, category, desktop-entry); forwards `DbusCmd`, emits `DbusSignal`s. Capabilities const: `body, body-markup, actions, icon-static, persistence`. Must NOT allocate IDs (Core does — critical for `replaces_id`) or store state.

**`notif-core`** — the hub. One async task selecting over DbusCmd/UiEvent/ConfigEvent/IPC rx + a single expiry `async_io::Timer` armed at `min(deadline)` and re-armed on every state change (no per-notification tasks). Owns: monotonic `next_id` (wrap, skip 0), `active: Vec<ActiveNotification{n, deadline, paused}>`, waiting queue when over max_visible, `history: VecDeque<Notification>` ring (transient excluded — history from day one). Hover pauses expiry storing remaining duration. Takes a `Clock` trait for deterministic tests. Must NOT touch D-Bus/Wayland/files/rendering — 100% unit-testable over channels.

**`notif-wl`** — everything Wayland via smithay-client-toolkit (`default-features = false` to drop calloop): registry/output/seat/pointer/`SlotPool` shm handling + `wlr_layer` module. Binds fractional-scale-v1, viewporter, cursor-shape-v1. One anchored layer surface sized to content (namespace `notif`, keyboard interactivity none, exclusive zone 0); destroyed when the visible set is empty. Integrates with the smol reactor by wrapping the connection fd in `Async<T>`: `prepare_read()` → readable await → `read()` → `dispatch_pending()`. Translates pointer events into hit-tested `UiEvent`s using layout rects from the renderer. Must NOT interpret notification semantics or manage timeouts.

**`notif-render`** — the trait:
```rust
trait Renderer {
    fn measure(&mut self, items, cfg, scale) -> Layout;   // Layout { width, height, hit_regions: Vec<HitRegion> }
    fn render(&mut self, buf: &mut [u8], stride, layout, items, cfg, scale, hover);
}
```
tiny-skia into ARGB8888 (wl_shm little-endian = BGRA byte order; tiny-skia channel swizzle documented in exactly one function). cosmic-text for shaping/fallback (`FontSystem` built once at startup — construction scans system fonts, ~100ms). Body markup parsed to a small span model (`<b> <i> <u> <a>` only, strip the rest). Icon pipeline: resolve (freedesktop-icons / path / raw) → decode (image crate; resvg behind an `svg` cargo feature) → cached scaled Pixmap. Must NOT touch Wayland or own buffers.

**`notifd`** — CLI args, logging, config load, channel creation, spawn tasks on `LocalExecutor`, block until SIGINT/SIGTERM. The only place that knows every module.

## Tech Stack (versions verified July 2026, all actively maintained)

| Crate | Version | Notes |
|---|---|---|
| zbus | 5.16.0 | `default-features=false, features=["async-io"]` |
| wayland-client / wayland-protocols / wayland-protocols-wlr | 0.31.14 / 0.32.x / 0.3.12 | protocols features: `client`, `staging` |
| smithay-client-toolkit | 0.20.0 | `default-features=false` (drops calloop); saves hundreds of lines of subtle registry/seat/shm boilerplate |
| tiny-skia | 0.12.0 | ~200 KiB; shared dep with resvg |
| cosmic-text | 0.19.0 | The one bloat-vs-correctness call where correctness wins: HarfRust shaping + system fallback = emoji/CJK/RTL work. fontdue rejected (no shaping/fallback, stale since Feb 2025); swash rejected (would mean rebuilding fallback+itemization+line layout ourselves) |
| serde / toml | 1.x / 1.1.2 | |
| inotify | 0.11 | Direct, not `notify` — Linux-only target, async-io native, avoids cross-platform machinery |
| image | 0.25.x | `default-features=false, features=["png","jpeg"]` |
| resvg | 0.47.0 | Behind cargo feature `svg`; renders into tiny-skia we already ship |
| freedesktop-icons | 0.4.0 | Quiet but the spec is frozen; wrap behind our own `resolve_icon()` |
| smol family | async-io 2, async-executor 1, async-channel 2, futures-lite 2 | Individual crates, not the umbrella |
| misc | thiserror 2 (libs), anyhow (bins), log + env_logger (minimal features) | |

Rejected: **calloop** (redundant with async-io), **softbuffer** (adds nothing over SCTK SlotPool on wl_shm), **tokio**.

## Agent Delegation Roadmap — first five tasks

Order: vocabulary first, then the two headless-testable modules, then Wayland shell with a stub renderer, then the real renderer + final assembly.

1. **Workspace scaffold + `notif-types` + `notif-config`.** Types verbatim from this plan; Config with serde defaults, validator, inotify watcher. Done when clippy `-D warnings` clean and unit tests cover defaults/invalid-TOML rejection/partial override/rename-swap reload/no-emit-on-invalid.
2. **`notif-dbus`** + throwaway `examples/echo_daemon.rs` (sequential IDs, prints parsed notifications). Done when `notify-send -u critical -t 5000 "hi" "body"` parses correctly; `busctl` verifies `GetServerInformation` = `("notif","notif","0.1.0","1.2")` and `GetCapabilities`; unknown-id `CloseNotification` succeeds silently (dunst/mako behavior); name-taken at startup is a clean fatal error.
3. **`notif-core`.** Full state machine with mock clock. Done when channel-only unit tests cover: replaces_id keeps id + resets deadline (unknown id → new, per spec); expiry → `NotificationClosed(1)`; dismiss → 2; CloseNotification → 3; ActionInvoked signal + non-resident close; hover pause/resume; queue promotion; history cap + transient exclusion; config reload changes future defaults without touching live deadlines. Then headless integration: dbus + core driven by `notify-send`, observed via `busctl --user monitor`.
4. **`notif-wl` shell bring-up** with a stub rect renderer. Done when `examples/wl_demo.rs` (fake Syncs on a timer) shows clickable/hoverable boxes on Hyprland AND sway; output hotplug doesn't crash; fractional scale 1.25 is crisp (buffer at `ceil(logical*scale)` px + `wp_viewport.set_destination` — never `set_buffer_scale`); no protocol errors under `WAYLAND_DEBUG=1`.
5. **`notif-render` + assemble `notifd`.** Full styling, markup subset, wrap/ellipsis, icon pipeline, action buttons, hover states; swap out the stub; wire everything. Done when golden-image tests pass (offscreen render vs committed PNGs, bundled test font for determinism); CJK+emoji renders without tofu; end-to-end `notify-send` flows work including actions via `busctl ... Notify` and config hot-reload of colors.

## Sub-Agent Code Review Criteria

1. **No panics in daemon paths** — deny `clippy::unwrap_used`, `clippy::expect_used`, `clippy::indexing_slicing` at workspace level (allowed in tests/main startup).
2. **Errors**: thiserror enums in libs, anyhow in bins; peer misbehavior (bad hints, bad config, protocol oddities) is logged-and-degraded, never fatal; only startup failures may exit.
3. **Unsafe**: `#![forbid(unsafe_code)]` everywhere (SCTK SlotPool should make even notif-wl safe); any exception needs a `// SAFETY:` comment and explicit sign-off.
4. **Gates**: `cargo fmt --check` && `cargo clippy --workspace --all-targets -- -D warnings` && `cargo test --workspace`; **no new dependencies without approval** (diff Cargo.toml explicitly — cheap agents love adding crates).
5. **Boundary discipline**: notif-wl must not depend on zbus; notif-core must not depend on wayland-*/tiny-skia; only notif-types is universal; no Mutex where a message would do; no tokio in the lock file.
6. **Async hygiene**: no blocking calls in tasks (icon decode in the documented render-cache path excepted); every task has an owner and shutdown path; channel `Closed` handled without panic.
7. **Spec fidelity**: signatures match the interface XML exactly; CloseReason values 1–4; actions as flat `[key, label, ...]` pairs, order preserved.

## Known Risks & Gotchas (bake into task prompts)

- **replaces_id**: return the same id; reset expiry; update in place (no re-stack jump); never emit NotificationClosed for replaced content.
- **image-data**: rowstride may exceed width×channels — copy row by row; data is RGB(A), needs swizzle to BGRA-in-memory ARGB8888; precedence `image-data` > `image-path` > `app_icon` > `icon_data`, honoring underscore variants from old libnotify.
- **expire_timeout**: `-1` = server default, `0` = never — don't invert. Emit `ActionInvoked` *before* `NotificationClosed(2)` for the same click.
- **Layer-surface configure dance**: must ack_configure and commit a buffer of exactly the configured size before mapping; `set_size(w,h)` explicitly since toast size is content-driven.
- **Buffers**: never reuse before `wl_buffer.release` (SlotPool handles, but request a fresh slot per frame).
- **Multi-output**: surfaces on removed outputs must be destroyed/recreated. "Follow focused output" is not portable — Hyprland IPC (`.socket2.sock` events) as an optional enhancement; portable default is configured-output-name else compositor-chosen.
- **Pointer**: hit regions are in buffer pixels; apply inverse fractional-scale transform for logical-space events; treat `pointer.leave` as unhover-everything.
- **Hyprland**: document `layerrule = blur, notif` / `ignorezero, notif` (keep namespace stable); test each Hyprland major, keep sway as the spec-correct reference; `keyboard_interactivity = none` always.
- **History memory**: downscale image-data to thumbnails in history, drop originals after render caching.

## Verification

- Per-task gates as listed in each delegation task (clippy/fmt/test + the specific `notify-send`/`busctl` checks).
- End-to-end on Hyprland: urgency levels, expiry, hover-pause, actions, replaces_id (`notify-send -r`), markup, CJK/emoji, config hot-reload, fractional scaling, output hotplug.
- Portability check on sway (nested session) before calling any Wayland-touching task done.

## Execution note

On approval, work begins with **Task 1** (workspace scaffold + notif-types + notif-config): I author the tightly scoped prompt from this plan and delegate to a builder agent, then review against the criteria above before moving to Task 2. This plan document also gets committed into the repo as `PLAN.md` so builder agents can be pointed at the frozen contracts.

---

# Phase 2 — IPC, notifctl, Notification Center

Phase 1 delivered the daemon (popups). Phase 2 adds external control and the history panel. Three tasks, same protocol: architect-authored prompts, builder implements, architect reviews against §Review Criteria, one commit per task.

## Phase-2 message vocabulary (notif-types additions — frozen once Task 6 lands)

```rust
/// Serializable summary of a notification for IPC/history consumers.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct HistoryEntry {
    pub id: u32,
    pub app_name: String,
    pub summary: String,
    pub body: String,
    pub urgency: Urgency,
    pub created_at_unix: u64,   // seconds since epoch (SystemTime is not serde-friendly)
}

pub struct StatusInfo { pub dnd: bool, pub active: usize, pub waiting: usize, pub history: usize, pub center_visible: bool } // serde too

// IpcCmd REPLACES the Phase-1 placeholder (request/response where needed):
pub enum IpcCmd {
    DismissAll,
    Close { id: u32 },
    History { reply: ReplyTx<Vec<HistoryEntry>> },
    ClearHistory,
    ToggleDnd { reply: ReplyTx<bool> },          // replies with the NEW dnd state
    ToggleCenter { reply: ReplyTx<bool> },       // replies with the NEW visibility
    Status { reply: ReplyTx<StatusInfo> },
}

// UiCommand gains one variant:
UiCommand::SetCenter { visible: bool, entries: Arc<[DisplayNotification]> }
// UiEvent gains two:
UiEvent::HistoryRemoveRequested(u32)   // '×' on a center entry
UiEvent::ClearHistoryRequested        // 'clear all' button in center
```

## Core Phase-2 semantics (notif-core)

- **DND**: `dnd: bool` state (starts false). While on: incoming non-Critical notifications are assigned an id, added directly to history (image stripped, transient still excluded), NOT displayed, no signals emitted. Critical notifications bypass DND and display normally. replaces_id targeting a DND-hidden (history) entry → treat as new (spec-safe). ToggleDnd replies with the new state.
- **History query**: `History` replies with newest-first `Vec<HistoryEntry>` mapped from the ring.
- **Center**: core tracks `center_visible: bool`. ToggleCenter flips it and (always, plus after any history mutation while visible) pushes `UiCommand::SetCenter { visible, entries }` where entries = history newest-first as stripped DisplayNotifications. `HistoryRemoveRequested(id)` removes from history; `ClearHistoryRequested`/`ClearHistory` empties it; both re-push SetCenter when visible.
- DismissAll/Close(id) via IPC reuse existing dismiss paths (reason Dismissed).

## Task 6 — notif-ipc socket server + core Phase-2 semantics
Socket: `$XDG_RUNTIME_DIR/notif.sock` (error if XDG_RUNTIME_DIR unset). Unlink stale socket on bind; unlink on clean shutdown. Protocol: one JSON object per line, request→response:
`{"cmd":"dismiss-all"}` → `{"ok":true}` · `{"cmd":"close","id":5}` → `{"ok":true}` · `{"cmd":"history"}` → `{"ok":true,"history":[HistoryEntry...]}` · `{"cmd":"clear-history"}` → `{"ok":true}` · `{"cmd":"toggle-dnd"}` → `{"ok":true,"dnd":<new>}` · `{"cmd":"toggle-center"}` → `{"ok":true,"visible":<new>}` · `{"cmd":"status"}` → `{"ok":true,"status":StatusInfo}` · unknown/malformed → `{"ok":false,"error":"..."}` (connection stays open; one request per connection is also fine for the client).
notif-ipc: `pub async fn run(ipc_tx: Sender<IpcCmd>) -> Result<(), IpcError>` using async-io Async<UnixListener>; sequential accept loop; serde_json (NEW APPROVED DEP: serde_json, workspace-wide). notifd spawns it; wl gains a no-op/log arm for SetCenter (real rendering is Task 8). Tests: protocol unit tests over an in-process socketpair; core unit tests for DND (hidden non-critical, critical bypass), history query mapping, clear/remove.

## Task 7 — notifctl
bin/notifctl: subcommands `dismiss-all | close <id> | history [--json] | clear-history | dnd | center | status [--json]`. Hand-rolled arg parsing (match on args — no clap; zero-bloat). Connects, sends one JSON line, prints reply human-readably (or raw JSON with --json). Exit 0 on ok:true, 1 on ok:false/connect failure with message to stderr. Smoke script `bin/notifctl/tests/manual/ctl_smoke.sh`: isolated bus, start notifd, notify-send, then notifctl status/history/dismiss-all/dnd round-trips asserting outputs.

## Task 8 — notification-center panel (notif-wl + notif-render)
Second layer surface, namespace `"notif-center"` (documented for layerrules), same output, Layer::Top, anchored to a single corner (like the toast stack — never a two-edge span such as top+bottom, which would make the compositor vertically center a fixed-height surface instead of pinning it to an edge) and sized to its content height, keyboard_interactivity none, fixed logical width `config.center_width` / `[center].width` (validated ≤ 8192; superseded top-level `center_width` is deprecated but still honored as a fallback). Rendered by the same SkiaRenderer via a new trait method with default impl OR a CenterRenderer wrapper — prefer: extend Renderer with `measure_center`/`render_center` (default impls returning empty so StubRenderer stays valid). Entries: compact rows (summary bold, app_name + relative age line, body one-line ellipsis, per-urgency accent border-left, '×' hit region → HistoryRemoveRequested), header with count + 'Clear all' hit region → ClearHistoryRequested. Empty history → 'No notifications' placeholder. Center surface lifecycle mirrors the toast surface (created on SetCenter{visible:true}, destroyed on false); toasts and center coexist (two SurfaceStates — refactor SurfaceState handling to a small map/two-slot struct). Scale/viewport/hit-test discipline identical to toasts (layout_scale per surface). Golden tests for the center rendering (same bundled fonts); e2e: toggle via notifctl on live Hyprland, grim screenshot, verify panel + entry removal.

## Phase-2 verification
Full gates per task; after Task 8: live e2e — notify-send ×3, expire, `notifctl center` shows them in the panel, '×' removes one (manual note if un-clickable in test), `notifctl dnd` hides subsequent normal notifications but not critical, `notifctl status --json` sane, SIGTERM clean, goldens stable.
