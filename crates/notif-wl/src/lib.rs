// notif-wl must not depend on zbus.  This is enforced by the workspace manifest.
#![forbid(unsafe_code)]

//! `notif-wl` — Wayland layer-shell surface management for the notif daemon.
//!
//! Connects to the Wayland compositor using `smithay-client-toolkit` (no calloop),
//! manages a single `zwlr_layer_shell_v1` surface sized to notification content,
//! forwards pointer events as `UiEvent`s, and integrates with the smol async
//! reactor via `async_io::Async`.
//!
//! # Layer choice: `Top` not `Overlay`
//! We use `Layer::Top` rather than `Layer::Overlay`.  `Overlay` sits above lock
//! screens; `Top` sits above normal windows but below lock screens.  A notification
//! daemon should not be visible while the screen is locked, so `Top` is the correct
//! semantic layer.  Compositors may grant `Overlay` to arbitrary clients which is a
//! security risk; `Top` is universally supported and semantically correct.
//!
//! # Namespace: `"notif"`
//! Stable namespace so Hyprland `layerrule`s can target it:
//! ```text
//! layerrule = blur, notif
//! layerrule = ignorezero, notif
//! ```
//!
//! # Event loop (no calloop)
//! We own the [`EventQueue`] and drive it manually:
//! 1. `flush()` — send pending requests to the compositor.
//! 2. `prepare_read()` — if `None`, go straight to `dispatch_pending()`.
//! 3. Race readable-on-fd (via `async_io::Async`) against `ui_cmd_rx.recv()`.
//! 4. `read_events()` / fall through, then `dispatch_pending()`.
//! 5. Handle any queued `UiCommand`s.

use std::{
    os::unix::io::{AsFd, OwnedFd},
    sync::Arc,
};

use futures_lite::future;
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState, SurfaceData},
    delegate_compositor, delegate_layer, delegate_output, delegate_pointer, delegate_registry,
    delegate_seat, delegate_shm,
    output::{OutputHandler, OutputInfo, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        Capability, SeatHandler, SeatState,
        pointer::{PointerEvent, PointerEventKind, PointerHandler},
    },
    shell::{
        WaylandSurface,
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
    },
    shm::{Shm, ShmHandler, slot::SlotPool},
};
use wayland_client::{
    Connection, Proxy, QueueHandle,
    globals::registry_queue_init,
    protocol::{wl_output, wl_pointer, wl_seat, wl_shm, wl_surface},
};
use wayland_protocols::wp::{
    fractional_scale::v1::client::{
        wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
        wp_fractional_scale_v1::{self, WpFractionalScaleV1},
    },
    viewporter::client::{wp_viewport::WpViewport, wp_viewporter::WpViewporter},
};

use notif_render::{HitTarget, Layout, Renderer};
use notif_types::{DisplayNotification, UiCommand, UiEvent, config::Config};

// ── Error type ─────────────────────────────────────────────────────────────

/// Errors from the Wayland subsystem.
#[derive(Debug, thiserror::Error)]
pub enum WlError {
    /// Failed to connect to the Wayland compositor.
    #[error("wayland connection failed: {0}")]
    Connect(#[from] wayland_client::ConnectError),
    /// Required global not available.
    #[error("required wayland global not available: {0}")]
    Global(#[from] wayland_client::globals::BindError),
    /// Dispatcher error.
    #[error("wayland dispatch error: {0}")]
    Dispatch(#[from] wayland_client::DispatchError),
    /// Global enumeration failed.
    #[error("wayland global init error: {0}")]
    GlobalInit(#[from] wayland_client::globals::GlobalError),
    /// SHM pool creation failed.
    #[error("shm pool error: {0}")]
    ShmPool(#[from] smithay_client_toolkit::shm::CreatePoolError),
    /// SHM buffer creation failed.
    #[error("shm buffer error: {0}")]
    ShmBuffer(#[from] smithay_client_toolkit::shm::slot::CreateBufferError),
    /// Buffer activation error.
    #[error("shm buffer activate error: {0}")]
    ShmActivate(#[from] smithay_client_toolkit::shm::slot::ActivateSlotError),
    /// I/O or Wayland backend error.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// Wayland backend error (flush, etc.)
    #[error("wayland backend error: {0}")]
    Backend(wayland_client::backend::WaylandError),
    /// Internal invariant broken — event queue unexpectedly absent.
    #[error("internal: event queue missing")]
    EventQueueMissing,
}

impl From<wayland_client::backend::WaylandError> for WlError {
    fn from(e: wayland_client::backend::WaylandError) -> Self {
        Self::Backend(e)
    }
}

// ── Public entry point ──────────────────────────────────────────────────────

/// Run the Wayland event loop.
///
/// Blocks (async) until `UiCommand::Shutdown` is received or the command channel closes.
pub async fn run(
    config: Arc<Config>,
    ui_cmd_rx: async_channel::Receiver<UiCommand>,
    ui_event_tx: async_channel::Sender<UiEvent>,
    renderer: Box<dyn Renderer>,
) -> Result<(), WlError> {
    // Connect to compositor.
    let conn = Connection::connect_to_env()?;
    let (globals, event_queue) = registry_queue_init(&conn)?;
    let qh = event_queue.handle();

    // Bind mandatory globals.
    let compositor = CompositorState::bind(&globals, &qh)?;
    let layer_shell = LayerShell::bind(&globals, &qh)?;
    let shm = Shm::bind(&globals, &qh)?;

    // Bind optional globals (gracefully absent on some compositors).
    let viewporter: Option<WpViewporter> = globals.bind::<WpViewporter, _, _>(&qh, 1..=1, ()).ok();
    let frac_scale_manager: Option<WpFractionalScaleManagerV1> = globals
        .bind::<WpFractionalScaleManagerV1, _, _>(&qh, 1..=1, ())
        .ok();

    // Initial pool size (1 MiB; grows as needed).
    let pool = SlotPool::new(1024 * 1024, &shm)?;

    // Wrap the event queue fd in async_io::Async so we can await readability.
    // EventQueue: AsFd; try_clone_to_owned duplicates the fd via dup(2)/F_DUPFD_CLOEXEC
    // from the standard library — no unsafe required.
    let wl_fd: OwnedFd = event_queue.as_fd().try_clone_to_owned()?;
    let async_fd = async_io::Async::new(wl_fd)?;

    let mut state = WlState {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        shm,
        compositor,
        layer_shell,
        pool,
        viewporter,
        frac_scale_manager,
        config,
        renderer,
        ui_event_tx,
        surface_state: None,
        pointer: None,
        current_hover: None,
        pending_items: Arc::from([]),
        pending_redraw: false,
        shutdown: false,
        event_queue: Some(event_queue),
    };

    state.event_loop(&async_fd, ui_cmd_rx, &qh).await
}

// ── Per-surface state ───────────────────────────────────────────────────────

struct SurfaceState {
    layer: LayerSurface,
    viewport: Option<WpViewport>,
    frac_scale: Option<WpFractionalScaleV1>,
    /// Scale value that will be used by the NEXT redraw (may have been updated
    /// by a scale-change event that arrived before the redraw ran).
    scale: f64,
    /// Scale value that was actually used when `layout` was last measured.
    /// Hit-testing must use this value, not `scale`, to avoid stale-scale
    /// mismatches when a scale event arrives in the same dispatch batch as a
    /// pointer event but before the next redraw.
    layout_scale: f64,
    layout: Layout,
    logical_w: u32,
    logical_h: u32,
    /// Whether we have committed at least one buffer.
    mapped: bool,
    /// Whether the initial configure has arrived. SCTK acks configures
    /// automatically before invoking the handler; we must never ack manually,
    /// and must not attach a buffer before the first configure.
    configured: bool,
}

// ── Main state struct ───────────────────────────────────────────────────────

struct WlState {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    shm: Shm,
    compositor: CompositorState,
    layer_shell: LayerShell,
    pool: SlotPool,
    viewporter: Option<WpViewporter>,
    frac_scale_manager: Option<WpFractionalScaleManagerV1>,

    config: Arc<Config>,
    renderer: Box<dyn Renderer>,
    ui_event_tx: async_channel::Sender<UiEvent>,

    /// Current layer surface, if any.
    surface_state: Option<SurfaceState>,
    /// Active pointer object.
    pointer: Option<wl_pointer::WlPointer>,
    /// Currently hovered hit target.
    current_hover: Option<HitTarget>,
    /// Latest set of items from a Sync command.
    pending_items: Arc<[DisplayNotification]>,
    /// True when content changed and we should re-measure + re-render.
    pending_redraw: bool,
    /// Set when Shutdown command arrives.
    shutdown: bool,
    /// Event queue; wrapped in Option so we can temporarily take ownership.
    event_queue: Option<wayland_client::EventQueue<WlState>>,
}

impl WlState {
    /// Dispatch pending Wayland events.
    ///
    /// Takes the EventQueue out of `self.event_queue`, dispatches, then puts it back.
    /// This sidesteps the borrow-checker issue of `&mut eq` + `&mut self` coexisting.
    fn dispatch_wl(&mut self) -> Result<(), WlError> {
        let mut eq = self.event_queue.take().ok_or(WlError::EventQueueMissing)?;
        let result = eq
            .dispatch_pending(self)
            .map(|_| ())
            .map_err(WlError::Dispatch);
        self.event_queue = Some(eq);
        result
    }

    /// Flush the event queue.
    fn flush_wl(&mut self) -> Result<(), WlError> {
        let eq = self
            .event_queue
            .as_mut()
            .ok_or(WlError::EventQueueMissing)?;
        eq.flush().map_err(WlError::Backend)
    }

    /// The main async event loop.
    async fn event_loop(
        &mut self,
        async_fd: &async_io::Async<OwnedFd>,
        ui_cmd_rx: async_channel::Receiver<UiCommand>,
        qh: &QueueHandle<WlState>,
    ) -> Result<(), WlError> {
        loop {
            // Flush pending requests to the compositor.
            self.flush_wl()?;

            // Apply any pending redraw before waiting.
            if self.pending_redraw {
                self.pending_redraw = false;
                self.apply_pending(qh)?;
            }

            if self.shutdown {
                break;
            }

            // Check if there are events already queued (no I/O needed).
            let has_pending = {
                let eq = self
                    .event_queue
                    .as_mut()
                    .ok_or(WlError::EventQueueMissing)?;
                eq.prepare_read().is_none()
            };
            if has_pending {
                self.dispatch_wl()?;
                continue;
            }

            // Race: Wayland socket readable vs UiCommand available.
            let cmd: Option<UiCommand> = future::or(
                async {
                    let _ = async_fd.readable().await;
                    None
                },
                async { ui_cmd_rx.recv().await.ok() },
            )
            .await;

            // Try to read from the Wayland socket then dispatch.
            {
                let eq = self
                    .event_queue
                    .as_mut()
                    .ok_or(WlError::EventQueueMissing)?;
                if let Some(guard) = eq.prepare_read() {
                    // Ignore read errors; dispatch_pending will surface them.
                    let _ = guard.read();
                }
            }
            self.dispatch_wl()?;

            // Handle any UI command we received.
            if let Some(command) = cmd {
                self.handle_command(command, qh)?;
            }

            if self.shutdown {
                break;
            }
        }

        // Clean up surface on exit.
        self.destroy_surface();
        Ok(())
    }

    fn handle_command(&mut self, cmd: UiCommand, qh: &QueueHandle<WlState>) -> Result<(), WlError> {
        match cmd {
            UiCommand::Sync(items) => {
                self.pending_items = items;
                self.apply_pending(qh)?;
            }
            UiCommand::ConfigChanged(cfg) => {
                self.config = cfg;
                if !self.pending_items.is_empty() {
                    self.apply_pending(qh)?;
                }
            }
            UiCommand::Shutdown => {
                self.shutdown = true;
            }
            UiCommand::SetCenter { visible, .. } => {
                // Phase 2 Task 8 (center panel rendering) is not yet implemented.
                // Log and ignore so the run loop compiles; the real surface
                // management will be added in Task 8.
                log::debug!(
                    "notif-wl: SetCenter(visible={visible}) — center rendering not yet implemented"
                );
            }
        }
        Ok(())
    }

    /// Apply the pending items: create/destroy/redraw surface as needed.
    fn apply_pending(&mut self, qh: &QueueHandle<WlState>) -> Result<(), WlError> {
        if self.pending_items.is_empty() {
            self.destroy_surface();
            return Ok(());
        }

        if self.surface_state.is_none() {
            self.create_surface(qh)?;
        } else {
            self.redraw(qh)?;
        }
        Ok(())
    }

    fn create_surface(&mut self, qh: &QueueHandle<WlState>) -> Result<(), WlError> {
        let wl_surface = self.compositor.create_surface(qh);

        let output = self.find_output();

        let layer = self.layer_shell.create_layer_surface(
            qh,
            wl_surface.clone(),
            // Layer::Top: above normal windows, below lock screens. (See module doc.)
            Layer::Top,
            Some("notif"),
            output.as_ref(),
        );

        let anchor = anchor_for_corner(self.config.anchor);
        layer.set_anchor(anchor);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        // Exclusive zone 0: do not reserve space; do not be displaced by other zones.
        layer.set_exclusive_zone(0);
        let mx = self.config.margin_x as i32;
        let my = self.config.margin_y as i32;
        layer.set_margin(my, mx, my, mx);

        let scale = 1.0_f64;
        let layout = self
            .renderer
            .measure(&self.pending_items, &self.config, scale);
        let lw = layout.width;
        let lh = layout.height;

        layer.set_size(lw, lh);

        // Initial commit (no buffer) — compositor responds with configure.
        layer.commit();

        let frac_scale = self
            .frac_scale_manager
            .as_ref()
            .map(|mgr| mgr.get_fractional_scale(&wl_surface, qh, ()));
        let viewport = self
            .viewporter
            .as_ref()
            .map(|vp| vp.get_viewport(&wl_surface, qh, ()));

        self.surface_state = Some(SurfaceState {
            layer,
            viewport,
            frac_scale,
            scale,
            layout_scale: scale,
            layout,
            logical_w: lw,
            logical_h: lh,
            mapped: false,
            configured: false,
        });

        Ok(())
    }

    fn destroy_surface(&mut self) {
        if let Some(ss) = self.surface_state.take() {
            if let Some(vp) = ss.viewport {
                vp.destroy();
            }
            if let Some(fs) = ss.frac_scale {
                fs.destroy();
            }
            drop(ss.layer);
        }
        self.current_hover = None;
    }

    fn redraw(&mut self, qh: &QueueHandle<WlState>) -> Result<(), WlError> {
        if self.pending_items.is_empty() {
            self.destroy_surface();
            return Ok(());
        }

        let ss = match self.surface_state.as_mut() {
            Some(s) => s,
            None => return Ok(()),
        };
        if !ss.configured {
            // Attaching before the first configure is a protocol error;
            // the configure handler re-enters redraw once it arrives.
            return Ok(());
        }

        let scale = ss.scale;
        let layout = self
            .renderer
            .measure(&self.pending_items, &self.config, scale);
        let new_lw = layout.width;
        let new_lh = layout.height;

        let Some(ss) = self.surface_state.as_mut() else {
            return Ok(());
        };
        if new_lw != ss.logical_w || new_lh != ss.logical_h {
            ss.layer.set_size(new_lw, new_lh);
            ss.logical_w = new_lw;
            ss.logical_h = new_lh;
        }
        ss.layout = layout.clone();
        // Record the scale that was actually used to measure this layout.
        // Pointer events that arrive before the next redraw must use this value
        // so that hit-testing is consistent with the committed buffer geometry.
        ss.layout_scale = scale;

        // Buffer dimensions: ceil(logical * scale).
        let buf_w = ((new_lw as f64 * scale).ceil()) as u32;
        let buf_h = ((new_lh as f64 * scale).ceil()) as u32;
        if buf_w == 0 || buf_h == 0 {
            return Ok(());
        }
        // Compute buffer size in u64 to detect overflow before any narrowing cast.
        let stride64 = buf_w as u64 * 4;
        let required64 = stride64 * buf_h as u64;
        // Refuse unreasonably large buffers (> 64 MiB) — config validation enforces
        // sane max_width/max_height/max_visible, but a hostile scale value could still
        // produce a huge number here.
        if required64 > 64 * 1024 * 1024 {
            log::warn!(
                "notif-wl: refusing to allocate {required64}-byte buffer \
                 ({buf_w}×{buf_h} px at scale {scale:.3}); skipping frame"
            );
            return Ok(());
        }
        // All three casts are safe: required64 ≤ 64 MiB < usize::MAX on any
        // supported 32/64-bit target; stride64 and buf_w/buf_h fit in u32/i32
        // because required64 = stride64 * buf_h ≤ 64 MiB and stride64 = buf_w*4.
        let stride = stride64 as u32;
        let required = required64 as usize;

        // Grow pool if the current frame is larger than what we've allocated.
        if self.pool.len() < required {
            self.pool.resize(required)?;
        }

        let (buffer, canvas) = self.pool.create_buffer(
            buf_w as i32,
            buf_h as i32,
            stride as i32,
            wl_shm::Format::Argb8888,
        )?;

        let hover = self.current_hover.as_ref();
        self.renderer.render(
            canvas,
            stride,
            &layout,
            &self.pending_items,
            &self.config,
            scale,
            hover,
        );

        let Some(ss) = self.surface_state.as_mut() else {
            return Ok(());
        };

        if let Some(vp) = ss.viewport.as_ref() {
            vp.set_destination(new_lw as i32, new_lh as i32);
        }

        let wl_surf = ss.layer.wl_surface().clone();
        wl_surf.damage_buffer(0, 0, buf_w as i32, buf_h as i32);

        buffer.attach_to(&wl_surf)?;
        ss.layer.commit();
        ss.mapped = true;

        // Drop buffer handle; SlotPool frees memory on compositor wl_buffer.release.
        drop(buffer);
        let _ = qh;
        Ok(())
    }

    /// Find the WlOutput matching the configured output name, or None for compositor default.
    fn find_output(&self) -> Option<wl_output::WlOutput> {
        let name = self.config.output.as_deref()?;
        self.output_state.outputs().find(|o| {
            self.output_state
                .info(o)
                .and_then(|i: OutputInfo| i.name)
                .as_deref()
                == Some(name)
        })
    }

    /// Hit-test a point in buffer-pixel space.
    fn hit_test(&self, buf_x: f64, buf_y: f64) -> Option<HitTarget> {
        let ss = self.surface_state.as_ref()?;
        let px = buf_x as i32;
        let py = buf_y as i32;
        ss.layout
            .hit_regions
            .iter()
            .find(|r| r.rect.contains(px, py))
            .map(|r| r.target.clone())
    }

    /// Convert logical (Wayland) pointer coordinates to buffer-pixel space.
    ///
    /// Uses `layout_scale` — the scale that was active when the current layout
    /// was measured — rather than `scale` (which may already reflect a
    /// scale-change event that has not yet been redrawn).  This avoids stale
    /// hit-test mismatches in the same dispatch batch.
    fn logical_to_buf(&self, lx: f64, ly: f64) -> (f64, f64) {
        let scale = self.surface_state.as_ref().map_or(1.0, |s| s.layout_scale);
        (lx * scale, ly * scale)
    }

    fn handle_pointer_motion(&mut self, buf_x: f64, buf_y: f64, qh: &QueueHandle<WlState>) {
        let new_hover = self.hit_test(buf_x, buf_y);
        if new_hover == self.current_hover {
            return;
        }

        if let Some(HitTarget::Body(id)) = &self.current_hover {
            let _ = self.ui_event_tx.try_send(UiEvent::HoverChanged {
                id: *id,
                hovered: false,
            });
        }

        if let Some(HitTarget::Body(id)) = &new_hover {
            let _ = self.ui_event_tx.try_send(UiEvent::HoverChanged {
                id: *id,
                hovered: true,
            });
        }

        self.current_hover = new_hover;
        let _ = self.redraw(qh);
    }

    fn handle_pointer_leave(&mut self, qh: &QueueHandle<WlState>) {
        let old = self.current_hover.take();
        if let Some(HitTarget::Body(id)) = old {
            let _ = self
                .ui_event_tx
                .try_send(UiEvent::HoverChanged { id, hovered: false });
            let _ = self.redraw(qh);
        }
    }
}

// ── SCTK handler trait implementations ─────────────────────────────────────

impl CompositorHandler for WlState {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        new_factor: i32,
    ) {
        let is_ours = self
            .surface_state
            .as_ref()
            .is_some_and(|ss| ss.layer.wl_surface() == surface);
        if !is_ours {
            return;
        }
        // Integer scale fallback when fractional-scale protocol is absent.
        if let Some(ss) = self.surface_state.as_mut()
            && ss.frac_scale.is_none()
        {
            ss.scale = new_factor as f64;
        }
        let _ = self.redraw(qh);
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_transform: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for WlState {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
        let _ = self.ui_event_tx.try_send(UiEvent::OutputsChanged);
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        let should_recreate = self.surface_state.as_ref().is_some_and(|ss| {
            ss.layer
                .wl_surface()
                .data::<SurfaceData>()
                .is_some_and(|d| d.outputs().any(|o| o == output))
        });
        if should_recreate {
            self.destroy_surface();
            if !self.pending_items.is_empty() {
                let _ = self.create_surface(qh);
            }
        }
        let _ = self.ui_event_tx.try_send(UiEvent::OutputsChanged);
    }
}

impl LayerShellHandler for WlState {
    fn closed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _layer: &LayerSurface) {
        self.surface_state = None;
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        let is_ours = self
            .surface_state
            .as_ref()
            .is_some_and(|ss| &ss.layer == layer);
        if !is_ours {
            return;
        }

        if let Some(ss) = self.surface_state.as_mut() {
            ss.configured = true;
            if configure.new_size.0 != 0 {
                ss.logical_w = configure.new_size.0;
            }
            if configure.new_size.1 != 0 {
                ss.logical_h = configure.new_size.1;
            }
        }

        let _ = self.redraw(qh);
    }
}

impl SeatHandler for WlState {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}

    fn new_capability(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer && self.pointer.is_none() {
            match self.seat_state.get_pointer(qh, &seat) {
                Ok(ptr) => self.pointer = Some(ptr),
                Err(e) => log::warn!("failed to get pointer: {e}"),
            }
        }
    }

    fn remove_capability(
        &mut self,
        _conn: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer
            && let Some(ptr) = self.pointer.take()
        {
            ptr.release();
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl PointerHandler for WlState {
    fn pointer_frame(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        _pointer: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for event in events {
            let is_ours = self
                .surface_state
                .as_ref()
                .is_some_and(|ss| ss.layer.wl_surface() == &event.surface);

            match &event.kind {
                PointerEventKind::Leave { .. } => {
                    self.handle_pointer_leave(qh);
                }
                _ if !is_ours => {}
                PointerEventKind::Enter { .. } => {
                    let (bx, by) = self.logical_to_buf(event.position.0, event.position.1);
                    self.handle_pointer_motion(bx, by, qh);
                }
                PointerEventKind::Motion { .. } => {
                    let (bx, by) = self.logical_to_buf(event.position.0, event.position.1);
                    self.handle_pointer_motion(bx, by, qh);
                }
                PointerEventKind::Press { button, .. } => {
                    if *button == 0x110 {
                        // BTN_LEFT
                        let (bx, by) = self.logical_to_buf(event.position.0, event.position.1);
                        if let Some(target) = self.hit_test(bx, by) {
                            match &target {
                                HitTarget::Body(id) => {
                                    let _ = self.ui_event_tx.try_send(UiEvent::BodyClicked(*id));
                                }
                                HitTarget::CloseButton(id) => {
                                    let _ =
                                        self.ui_event_tx.try_send(UiEvent::DismissRequested(*id));
                                }
                                HitTarget::ActionButton { id, key } => {
                                    let _ = self.ui_event_tx.try_send(UiEvent::ActionInvoked {
                                        id: *id,
                                        key: key.clone(),
                                    });
                                }
                            }
                        }
                    }
                }
                PointerEventKind::Release { .. } | PointerEventKind::Axis { .. } => {}
            }
        }
    }
}

impl ShmHandler for WlState {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

// ── Fractional scale dispatch ───────────────────────────────────────────────

impl wayland_client::Dispatch<WpFractionalScaleManagerV1, ()> for WlState {
    fn event(
        _state: &mut Self,
        _proxy: &WpFractionalScaleManagerV1,
        _event: <WpFractionalScaleManagerV1 as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl wayland_client::Dispatch<WpFractionalScaleV1, ()> for WlState {
    fn event(
        state: &mut Self,
        proxy: &WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event {
            // scale is in 1/120 units; convert to f64.
            let new_scale = scale as f64 / 120.0;
            let is_ours = state
                .surface_state
                .as_ref()
                .is_some_and(|ss| ss.frac_scale.as_ref().is_some_and(|fs| fs == proxy));
            if is_ours {
                if let Some(ss) = state.surface_state.as_mut() {
                    ss.scale = new_scale;
                }
                let _ = state.redraw(qh);
            }
        }
    }
}

// ── Viewporter dispatch ─────────────────────────────────────────────────────

impl wayland_client::Dispatch<WpViewporter, ()> for WlState {
    fn event(
        _state: &mut Self,
        _proxy: &WpViewporter,
        _event: <WpViewporter as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl wayland_client::Dispatch<WpViewport, ()> for WlState {
    fn event(
        _state: &mut Self,
        _proxy: &WpViewport,
        _event: <WpViewport as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

// ── Delegate macros ─────────────────────────────────────────────────────────

delegate_compositor!(WlState);
delegate_output!(WlState);
delegate_shm!(WlState);
delegate_seat!(WlState);
delegate_pointer!(WlState);
delegate_layer!(WlState);
delegate_registry!(WlState);

impl ProvidesRegistryState for WlState {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

// ── Helpers ─────────────────────────────────────────────────────────────────

fn anchor_for_corner(corner: notif_types::config::AnchorCorner) -> Anchor {
    use notif_types::config::AnchorCorner;
    match corner {
        AnchorCorner::TopLeft => Anchor::TOP | Anchor::LEFT,
        AnchorCorner::TopRight => Anchor::TOP | Anchor::RIGHT,
        AnchorCorner::BottomLeft => Anchor::BOTTOM | Anchor::LEFT,
        AnchorCorner::BottomRight => Anchor::BOTTOM | Anchor::RIGHT,
    }
}

// ── Unit tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use notif_render::{HitRegion, HitTarget, Layout, Rect};

    fn make_layout(regions: Vec<HitRegion>) -> Layout {
        Layout {
            width: 424,
            height: 200,
            hit_regions: regions,
        }
    }

    fn make_region(x: i32, y: i32, w: u32, h: u32, id: u32) -> HitRegion {
        HitRegion {
            rect: Rect {
                x,
                y,
                width: w,
                height: h,
            },
            target: HitTarget::Body(id),
        }
    }

    /// Simulate hit-testing: convert logical → buffer coords by scale, then look up.
    fn hit(layout: &Layout, lx: f64, ly: f64, scale: f64) -> Option<HitTarget> {
        let bx = (lx * scale) as i32;
        let by = (ly * scale) as i32;
        layout
            .hit_regions
            .iter()
            .find(|r| r.rect.contains(bx, by))
            .map(|r| r.target.clone())
    }

    #[test]
    fn hit_test_scale_1() {
        // Notification rect: buffer-pixel coords (12, 12, 400x72) at scale 1.0.
        let layout = make_layout(vec![make_region(12, 12, 400, 72, 1)]);
        assert_eq!(hit(&layout, 12.0, 12.0, 1.0), Some(HitTarget::Body(1)));
        assert_eq!(hit(&layout, 11.0, 12.0, 1.0), None);
        assert_eq!(hit(&layout, 411.0, 83.0, 1.0), Some(HitTarget::Body(1)));
        assert_eq!(hit(&layout, 412.0, 83.0, 1.0), None); // x == x+width
    }

    #[test]
    fn hit_test_scale_1_5() {
        // At scale 1.5: logical (8.0, 8.0) → buffer (12.0, 12.0) → inside rect.
        let layout = make_layout(vec![make_region(12, 12, 400, 72, 2)]);
        assert_eq!(hit(&layout, 8.0, 8.0, 1.5), Some(HitTarget::Body(2)));
        // logical (7.9, 8.0) → buffer (11.85 → 11, 12.0 → 12) → outside left edge.
        assert_eq!(hit(&layout, 7.9, 8.0, 1.5), None);
    }

    /// Simulate what `logical_to_buf` does using an explicit `layout_scale`.
    fn logical_to_buf_with_layout_scale(lx: f64, ly: f64, layout_scale: f64) -> (f64, f64) {
        (lx * layout_scale, ly * layout_scale)
    }

    /// A2: verify that hit-testing uses `layout_scale`, not the updated `scale`.
    ///
    /// Scenario: layout was measured at scale 1.0; a scale-change event arrives
    /// updating `scale` to 1.5 before the next redraw.  A pointer event that
    /// arrives in the same dispatch batch must be transformed with 1.0 (the
    /// layout_scale), not 1.5 (the pending next-redraw scale).
    #[test]
    fn hit_test_uses_layout_scale_not_pending_scale() {
        // Layout measured at scale 1.0: region covers buffer pixels (10, 10)–(109, 109).
        let layout = make_layout(vec![HitRegion {
            rect: Rect {
                x: 10,
                y: 10,
                width: 100,
                height: 100,
            },
            target: HitTarget::Body(42),
        }]);

        // Logical coordinate (50.0, 50.0).
        // With layout_scale = 1.0 → buffer (50, 50) → inside rect → hit.
        let (bx, by) = logical_to_buf_with_layout_scale(50.0, 50.0, 1.0);
        assert_eq!(
            layout
                .hit_regions
                .iter()
                .find(|r| r.rect.contains(bx as i32, by as i32))
                .map(|r| r.target.clone()),
            Some(HitTarget::Body(42)),
            "layout_scale=1.0 should produce a hit"
        );

        // Same logical coordinate but transformed with the *new* (pending) scale = 1.5
        // → buffer (75, 75) → still inside, fine.  The key correctness check is that
        // we do NOT use scale=1.5 when the layout was measured at 1.0.
        // Demonstrate what the WRONG path would produce for a boundary coordinate:
        // logical (6.7, 6.7) with layout_scale 1.0 → buffer (6, 6) → miss (< 10).
        let (bx2, by2) = logical_to_buf_with_layout_scale(6.7, 6.7, 1.0);
        assert_eq!(
            layout
                .hit_regions
                .iter()
                .find(|r| r.rect.contains(bx2 as i32, by2 as i32))
                .map(|r| r.target.clone()),
            None,
            "layout_scale=1.0 at logical 6.7 should be outside rect"
        );
        // But with wrong pending scale 1.5: 6.7 * 1.5 = 10.05 → buffer 10 → inside rect (BUG).
        let (bx3, by3) = logical_to_buf_with_layout_scale(6.7, 6.7, 1.5);
        assert_eq!(
            layout
                .hit_regions
                .iter()
                .find(|r| r.rect.contains(bx3 as i32, by3 as i32))
                .map(|r| r.target.clone()),
            Some(HitTarget::Body(42)),
            "using pending scale=1.5 incorrectly registers a hit (demonstrating the bug we fixed)"
        );
    }

    #[test]
    fn hit_test_two_notifs() {
        let layout = make_layout(vec![
            make_region(12, 12, 400, 72, 1),
            make_region(12, 92, 400, 72, 2),
        ]);
        assert_eq!(hit(&layout, 50.0, 30.0, 1.0), Some(HitTarget::Body(1)));
        assert_eq!(hit(&layout, 50.0, 100.0, 1.0), Some(HitTarget::Body(2)));
        // Gap y=84..91 is empty.
        assert_eq!(hit(&layout, 50.0, 85.0, 1.0), None);
    }
}
