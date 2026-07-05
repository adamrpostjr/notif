#![forbid(unsafe_code)]

//! `notif-core` — single-task notification state manager.
//!
//! This crate is the architectural hub of the notification daemon.  It owns all
//! mutable state (active notifications, waiting queue, history) and mediates
//! between the D-Bus layer, the UI layer, the config watcher, and the IPC layer.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use notif_types::config::Config;
use notif_types::{
    CloseReason, ConfigEvent, DbusCmd, DbusSignal, DisplayNotification, HistoryEntry, ImageSource,
    IpcCmd, NewNotification, Notification, StatusInfo, Timeout, UiCommand, UiEvent, Urgency,
};

// ── Clock abstraction (for testing) ──────────────────────────────────────────

/// Abstraction over the system clock so tests can inject a fake.
pub trait Clock {
    /// Return the current instant.
    fn now(&self) -> Instant;
}

/// Production clock backed by [`std::time::Instant::now`].
pub struct RealClock;

impl Clock for RealClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

// ── Public handle bundle ──────────────────────────────────────────────────────

/// Channel endpoints that the async [`run`] function drives.
pub struct CoreHandles {
    /// Commands arriving from the D-Bus layer.
    pub dbus_cmd_rx: async_channel::Receiver<DbusCmd>,
    /// Signals that Core emits to the D-Bus layer.
    pub dbus_signal_tx: async_channel::Sender<DbusSignal>,
    /// Commands that Core sends to the UI layer.
    pub ui_cmd_tx: async_channel::Sender<UiCommand>,
    /// Events arriving from the UI layer.
    pub ui_event_rx: async_channel::Receiver<UiEvent>,
    /// Config-change events from the file watcher.
    pub config_rx: async_channel::Receiver<ConfigEvent>,
    /// IPC commands from `notifctl` or similar.
    pub ipc_rx: async_channel::Receiver<IpcCmd>,
}

// ── Internal state ────────────────────────────────────────────────────────────

struct ActiveNotification {
    n: Arc<Notification>,
    /// Absolute expiry instant, or `None` if the notification never expires.
    deadline: Option<Instant>,
    /// When the notification is hovered: the remaining time at the moment the
    /// hover started.  `None` when not hovered.
    paused: Option<Duration>,
    /// Current hover state (mirrored into `DisplayNotification`).
    hovered: bool,
}

/// The pure-logic state machine.  Completely synchronous; the `run` function
/// wraps it in an async select loop.
pub struct Core<C: Clock> {
    config: Arc<Config>,
    clock: C,
    /// Next candidate ID (wraps u32::MAX → 1, never 0).
    next_id: u32,
    /// Currently visible notifications; index 0 = newest / topmost.
    active: Vec<ActiveNotification>,
    /// Overflow queue; front = oldest waiting.
    waiting: VecDeque<Notification>,
    /// Closed-notification history; front = oldest.
    history: VecDeque<Arc<Notification>>,
    /// Do-Not-Disturb mode.  When on, non-Critical incoming notifications are
    /// silently added to history instead of being displayed.
    dnd: bool,
    /// Whether the notification center panel is currently shown.
    center_visible: bool,
    /// Set to `true` whenever anything the center panel displays changes:
    /// the active list (new/replace/promote/demote) or history (added,
    /// removed, cleared).  The async run loop reads and resets this flag to
    /// decide whether to push a `UiCommand::SetCenter` update.
    center_dirty: bool,
}

impl<C: Clock> Core<C> {
    /// Construct a fresh [`Core`] with the given config and clock.
    pub fn new(config: Arc<Config>, clock: C) -> Self {
        Self {
            config,
            clock,
            next_id: 1,
            active: Vec::new(),
            waiting: VecDeque::new(),
            history: VecDeque::new(),
            dnd: false,
            center_visible: false,
            center_dirty: false,
        }
    }

    // ── ID assignment ─────────────────────────────────────────────────────────

    fn assign_id(&mut self, n: Box<NewNotification>) -> Notification {
        let id = self.next_fresh_id();
        Notification::from_new(*n, id, std::time::SystemTime::now())
    }

    fn next_fresh_id(&mut self) -> u32 {
        loop {
            if self.next_id == 0 {
                self.next_id = 1;
            }
            let candidate = self.next_id;
            self.next_id = self.next_id.wrapping_add(1);
            if self.next_id == 0 {
                self.next_id = 1;
            }
            // Skip IDs already in use.
            let in_active = self.active.iter().any(|a| a.n.id == candidate);
            let in_waiting = self.waiting.iter().any(|w| w.id == candidate);
            if !in_active && !in_waiting {
                return candidate;
            }
        }
    }

    // ── Deadline computation ──────────────────────────────────────────────────

    fn compute_deadline(&self, n: &Notification, now: Instant) -> Option<Instant> {
        let style = match n.urgency {
            Urgency::Low => &self.config.low,
            Urgency::Normal => &self.config.normal,
            Urgency::Critical => &self.config.critical,
        };
        let timeout = if style.ignore_timeout {
            if style.default_timeout_ms == 0 {
                Timeout::Never
            } else {
                Timeout::Millis(style.default_timeout_ms)
            }
        } else {
            match n.expire_timeout {
                Timeout::Default => {
                    if style.default_timeout_ms == 0 {
                        Timeout::Never
                    } else {
                        Timeout::Millis(style.default_timeout_ms)
                    }
                }
                other => other,
            }
        };
        match timeout {
            Timeout::Never | Timeout::Default => None,
            Timeout::Millis(ms) => Some(now + Duration::from_millis(u64::from(ms))),
        }
    }

    // ── Handle D-Bus Notify ───────────────────────────────────────────────────

    /// Process a `Notify` D-Bus call.
    ///
    /// Returns `(assigned_id, signals_to_emit, ui_command)`.
    ///
    /// **DND semantics**: while DND is active, non-Critical incoming
    /// notifications that do not replace an existing active/waiting entry are
    /// sent directly to history (image stripped, transient excluded) and are
    /// never displayed.  Critical notifications and replacements of
    /// active/waiting entries bypass DND.
    pub fn handle_notify(
        &mut self,
        n: Box<NewNotification>,
        replaces_id: u32,
        now: Instant,
    ) -> (u32, Vec<DbusSignal>, UiCommand) {
        // Replace in active if replaces_id is found there.
        if replaces_id != 0 {
            if let Some(pos) = self.active.iter().position(|a| a.n.id == replaces_id) {
                let notification = Arc::new(Notification::from_new(
                    *n,
                    replaces_id,
                    std::time::SystemTime::now(),
                ));
                let deadline = self.compute_deadline(&notification, now);
                if let Some(entry) = self.active.get_mut(pos) {
                    entry.n = notification;
                    entry.deadline = deadline;
                    entry.paused = None;
                    // Keep hovered state if the pointer is still there.
                }
                self.center_dirty = true;
                return (replaces_id, vec![], self.sync_cmd());
            }

            // Replace in waiting queue.
            if let Some(pos) = self.waiting.iter().position(|w| w.id == replaces_id) {
                let notification =
                    Notification::from_new(*n, replaces_id, std::time::SystemTime::now());
                if let Some(slot) = self.waiting.get_mut(pos) {
                    *slot = notification;
                }
                return (replaces_id, vec![], self.sync_cmd());
            }
            // replaces_id not found in active or waiting (e.g. DND-hidden entry
            // in history) → fall through and treat as a new notification.
        }

        // Assign an ID to the incoming notification.
        let notification = self.assign_id(n);
        let id = notification.id;

        // DND: non-Critical new notifications go straight to history without display.
        if self.dnd && notification.urgency != Urgency::Critical {
            // add_to_history handles transient exclusion and image stripping.
            self.add_to_history(Arc::new(notification));
            // Active list is unchanged; return an unmodified sync.
            return (id, vec![], self.sync_cmd());
        }

        if self.active.len() < self.config.max_visible {
            let deadline = self.compute_deadline(&notification, now);
            self.active.insert(
                0,
                ActiveNotification {
                    n: Arc::new(notification),
                    deadline,
                    paused: None,
                    hovered: false,
                },
            );
            self.center_dirty = true;
        } else {
            self.waiting.push_back(notification);
        }

        (id, vec![], self.sync_cmd())
    }

    // ── Handle D-Bus Close ────────────────────────────────────────────────────

    /// Process a `CloseNotification` D-Bus call.  Returns any signals to emit.
    pub fn handle_close(&mut self, id: u32) -> Vec<DbusSignal> {
        // Look in active first.
        if let Some(pos) = self.active.iter().position(|a| a.n.id == id) {
            let entry = self.active.remove(pos);
            self.center_dirty = true;
            self.add_to_history(entry.n);
            let now = self.clock.now();
            self.promote_from_waiting(now);
            return vec![DbusSignal::NotificationClosed {
                id,
                reason: CloseReason::CloseCall,
            }];
        }
        // Look in waiting.
        if let Some(pos) = self.waiting.iter().position(|w| w.id == id) {
            let n = self.waiting.remove(pos).unwrap_or_else(|| {
                // Unreachable: we just found it at `pos`.
                Notification {
                    id: 0,
                    app_name: String::new(),
                    app_icon: String::new(),
                    summary: String::new(),
                    body: String::new(),
                    actions: vec![],
                    urgency: Urgency::Normal,
                    expire_timeout: Timeout::Default,
                    image: None,
                    transient: false,
                    resident: false,
                    category: None,
                    desktop_entry: None,
                    created_at: std::time::SystemTime::now(),
                    raw_hints: Default::default(),
                }
            });
            self.add_to_history(Arc::new(n));
            return vec![DbusSignal::NotificationClosed {
                id,
                reason: CloseReason::CloseCall,
            }];
        }
        // Unknown id — succeed silently.
        vec![]
    }

    // ── Handle UI Dismiss ─────────────────────────────────────────────────────

    /// Process a `DismissRequested` UI event.
    pub fn handle_dismiss(&mut self, id: u32) -> Vec<DbusSignal> {
        if let Some(pos) = self.active.iter().position(|a| a.n.id == id) {
            let entry = self.active.remove(pos);
            self.center_dirty = true;
            self.add_to_history(entry.n);
            let now = self.clock.now();
            self.promote_from_waiting(now);
            return vec![DbusSignal::NotificationClosed {
                id,
                reason: CloseReason::Dismissed,
            }];
        }
        vec![]
    }

    // ── Handle UI ActionInvoked ───────────────────────────────────────────────

    /// Process an `ActionInvoked` UI event.
    ///
    /// The `ActionInvoked` D-Bus signal is emitted **first**, followed by
    /// `NotificationClosed` (unless the notification is resident).
    pub fn handle_action_invoked(&mut self, id: u32, key: String) -> Vec<DbusSignal> {
        let pos = match self.active.iter().position(|a| a.n.id == id) {
            Some(p) => p,
            None => return vec![],
        };

        let mut signals = vec![DbusSignal::ActionInvoked {
            id,
            action_key: key,
        }];

        let resident = self.active.get(pos).map(|a| a.n.resident).unwrap_or(false);
        if !resident {
            let entry = self.active.remove(pos);
            self.center_dirty = true;
            self.add_to_history(entry.n);
            let now = self.clock.now();
            self.promote_from_waiting(now);
            signals.push(DbusSignal::NotificationClosed {
                id,
                reason: CloseReason::Dismissed,
            });
        }

        signals
    }

    // ── Handle UI BodyClicked ─────────────────────────────────────────────────

    /// Process a `BodyClicked` UI event.
    ///
    /// If the notification has an action with key `"default"`, this delegates to
    /// [`handle_action_invoked`] (which emits `ActionInvoked` before
    /// `NotificationClosed` and respects the `resident` flag).  If there is no
    /// `"default"` action, this behaves exactly like [`handle_dismiss`].
    pub fn handle_body_click(&mut self, id: u32) -> Vec<DbusSignal> {
        let has_default = self
            .active
            .iter()
            .find(|a| a.n.id == id)
            .map(|a| a.n.actions.iter().any(|act| act.key == "default"))
            .unwrap_or(false);

        if has_default {
            self.handle_action_invoked(id, "default".into())
        } else {
            self.handle_dismiss(id)
        }
    }

    // ── Handle UI HoverChanged ────────────────────────────────────────────────

    /// Process a `HoverChanged` UI event (pause/resume the expiry timer).
    pub fn handle_hover(&mut self, id: u32, hovered: bool, now: Instant) {
        if let Some(entry) = self.active.iter_mut().find(|a| a.n.id == id) {
            entry.hovered = hovered;
            if hovered {
                // Pause: store remaining time, clear deadline.
                if let Some(dl) = entry.deadline {
                    let remaining = dl.saturating_duration_since(now);
                    entry.paused = Some(remaining);
                    entry.deadline = None;
                }
                // If deadline was already None (never-expiring), no-op on timer.
            } else {
                // Resume: restore deadline from remaining time.
                if let Some(remaining) = entry.paused.take() {
                    entry.deadline = Some(now + remaining);
                }
            }
        }
    }

    // ── Handle Config reload ──────────────────────────────────────────────────

    /// Process a config-change event.  Returns `[ConfigChanged, Sync]` commands.
    pub fn handle_config(&mut self, config: Arc<Config>, now: Instant) -> Vec<UiCommand> {
        let new_max_visible = config.max_visible;
        let new_history_limit = config.history_limit;

        self.config = config.clone();

        // Re-cap history.  Mark dirty if any entries are evicted.
        let pre_len = self.history.len();
        while self.history.len() > new_history_limit {
            self.history.pop_front();
        }
        if self.history.len() < pre_len {
            self.center_dirty = true;
        }

        // Demote newest notifications if max_visible shrank.
        while self.active.len() > new_max_visible {
            // active[0] is the newest; send it back to front of waiting.
            let entry = self.active.remove(0);
            // Arc::try_unwrap to avoid a clone when there's only one reference;
            // fall back to (*n).clone() if the Arc is shared.
            let n = Arc::try_unwrap(entry.n).unwrap_or_else(|arc| (*arc).clone());
            self.waiting.push_front(n);
            self.center_dirty = true;
        }

        // Promote from waiting if max_visible grew.
        self.promote_from_waiting(now);

        vec![UiCommand::ConfigChanged(config), self.sync_cmd()]
    }

    // ── Handle IPC DismissAll ─────────────────────────────────────────────────

    /// Dismiss every active notification at once.
    pub fn handle_dismiss_all(&mut self, now: Instant) -> Vec<DbusSignal> {
        let mut signals = Vec::with_capacity(self.active.len());
        let drained: Vec<ActiveNotification> = self.active.drain(..).collect();
        if !drained.is_empty() {
            self.center_dirty = true;
        }
        for entry in drained {
            let id = entry.n.id;
            self.add_to_history(entry.n);
            signals.push(DbusSignal::NotificationClosed {
                id,
                reason: CloseReason::Dismissed,
            });
        }
        // waiting notifications are not dismissed by DismissAll; the freed
        // slots must show them immediately rather than on the next event.
        self.promote_from_waiting(now);
        signals
    }

    // ── Handle IPC ToggleDnd ──────────────────────────────────────────────────

    /// Toggle Do-Not-Disturb mode.  Returns the **new** DND state.
    pub fn handle_toggle_dnd(&mut self) -> bool {
        self.dnd = !self.dnd;
        self.dnd
    }

    // ── Handle IPC ToggleCenter ───────────────────────────────────────────────

    /// Toggle the notification center panel visibility.  Returns the **new**
    /// visibility state.
    pub fn handle_toggle_center(&mut self) -> bool {
        self.center_visible = !self.center_visible;
        self.center_visible
    }

    // ── Handle IPC History query ──────────────────────────────────────────────

    /// Return the history ring as a `Vec<HistoryEntry>`, newest first.
    pub fn handle_query_history(&self) -> Vec<HistoryEntry> {
        self.history
            .iter()
            .rev()
            .map(|n| HistoryEntry::from(&**n))
            .collect()
    }

    // ── Handle IPC Status query ───────────────────────────────────────────────

    /// Return a snapshot of the current daemon status.
    pub fn handle_query_status(&self) -> StatusInfo {
        StatusInfo {
            dnd: self.dnd,
            active: self.active.len(),
            waiting: self.waiting.len(),
            history: self.history.len(),
            center_visible: self.center_visible,
        }
    }

    // ── Handle IPC / UiEvent remove / clear history ───────────────────────────

    /// Remove a single entry from history by ID.  No-op if the ID is not found.
    pub fn handle_remove_history(&mut self, id: u32) {
        if let Some(pos) = self.history.iter().position(|n| n.id == id) {
            self.history.remove(pos);
            self.center_dirty = true;
        }
    }

    /// Empty the entire history ring.
    pub fn handle_clear_history(&mut self) {
        if !self.history.is_empty() {
            self.history.clear();
            self.center_dirty = true;
        }
    }

    // ── Tick (expiry) ─────────────────────────────────────────────────────────

    /// Expire any notifications whose deadline has passed.
    ///
    /// Returns `(signals, did_expire)`.
    pub fn tick(&mut self, now: Instant) -> (Vec<DbusSignal>, bool) {
        let expired_ids: Vec<u32> = self
            .active
            .iter()
            .filter(|a| a.paused.is_none() && a.deadline.map(|dl| dl <= now).unwrap_or(false))
            .map(|a| a.n.id)
            .collect();

        if expired_ids.is_empty() {
            return (vec![], false);
        }

        let mut signals = Vec::with_capacity(expired_ids.len());
        for id in &expired_ids {
            if let Some(pos) = self.active.iter().position(|a| a.n.id == *id) {
                let entry = self.active.remove(pos);
                self.center_dirty = true;
                self.add_to_history(entry.n);
                signals.push(DbusSignal::NotificationClosed {
                    id: *id,
                    reason: CloseReason::Expired,
                });
            }
        }

        self.promote_from_waiting(now);

        (signals, true)
    }

    // ── Sync command ──────────────────────────────────────────────────────────

    /// Build a `UiCommand::Sync` snapshot of the current active list.
    pub fn sync_cmd(&self) -> UiCommand {
        let display: Vec<DisplayNotification> = self
            .active
            .iter()
            .map(|a| DisplayNotification {
                notification: a.n.clone(),
                hovered: a.hovered,
            })
            .collect();
        UiCommand::Sync(Arc::from(display))
    }

    // ── Center command ────────────────────────────────────────────────────────

    /// Build a `UiCommand::SetCenter` with the active list and history, each
    /// newest first, jointly capped at `center_resolved().max_entries`.
    pub fn center_cmd(&self) -> UiCommand {
        let max = self.config.center_resolved().max_entries;
        let active: Vec<DisplayNotification> = self
            .active
            .iter()
            .take(max)
            .map(|a| DisplayNotification::from_arc(Arc::clone(&a.n)))
            .collect();
        let remaining = max.saturating_sub(active.len());
        let history: Vec<DisplayNotification> = self
            .history
            .iter()
            .rev()
            .take(remaining)
            .map(|n| DisplayNotification::from_arc(Arc::clone(n)))
            .collect();
        UiCommand::SetCenter {
            visible: self.center_visible,
            active: Arc::from(active),
            history: Arc::from(history),
        }
    }

    /// Return `true` and reset the dirty flag if anything the center displays
    /// (active list or history) was mutated since the last call to this
    /// method.  Used by the async run loop to decide whether to push a
    /// `SetCenter` update.
    pub fn take_center_dirty(&mut self) -> bool {
        let dirty = self.center_dirty;
        self.center_dirty = false;
        dirty
    }

    // ── Earliest deadline ─────────────────────────────────────────────────────

    /// The earliest expiry deadline across all active (non-paused) notifications.
    pub fn earliest_deadline(&self) -> Option<Instant> {
        self.active
            .iter()
            .filter(|a| a.paused.is_none())
            .filter_map(|a| a.deadline)
            .min()
    }

    // ── History helpers ───────────────────────────────────────────────────────

    /// Add a notification to the history ring.
    ///
    /// Transient notifications are silently dropped.  Raw image data is
    /// stripped (it can be large and is not needed for display in the history
    /// panel).  Sets `center_dirty` to `true` when an entry is actually added.
    fn add_to_history(&mut self, n: Arc<Notification>) {
        if n.transient {
            return;
        }
        // Only clone the inner value when we need to strip raw image data.
        // For path/icon images and no-image notifications, store the Arc directly
        // with zero additional allocations.
        let arc = if matches!(n.image, Some(ImageSource::Data(_))) {
            let mut stripped = (*n).clone();
            stripped.image = None;
            Arc::new(stripped)
        } else {
            n
        };
        self.history.push_back(arc);
        while self.history.len() > self.config.history_limit {
            self.history.pop_front();
        }
        self.center_dirty = true;
    }

    // ── Promotion ─────────────────────────────────────────────────────────────

    fn promote_from_waiting(&mut self, now: Instant) {
        while self.active.len() < self.config.max_visible {
            match self.waiting.pop_front() {
                Some(n) => {
                    let deadline = self.compute_deadline(&n, now);
                    // Promoted notifications are older; push to the end (lowest prominence).
                    self.active.push(ActiveNotification {
                        n: Arc::new(n),
                        deadline,
                        paused: None,
                        hovered: false,
                    });
                    self.center_dirty = true;
                }
                None => break,
            }
        }
    }
}

// ── Async run loop ────────────────────────────────────────────────────────────

/// Run the Core state machine, driving all channels until both the D-Bus and UI
/// channels close.
pub async fn run(initial_config: Arc<Config>, handles: CoreHandles) {
    use async_io::Timer;
    use futures_lite::future;

    let CoreHandles {
        dbus_cmd_rx,
        dbus_signal_tx,
        ui_cmd_tx,
        ui_event_rx,
        config_rx,
        ipc_rx,
    } = handles;

    let mut core = Core::new(initial_config, RealClock);

    // Far-future sentinel so the timer never fires spuriously.
    let far_future = Instant::now() + Duration::from_secs(365 * 24 * 3600);
    let mut timer = Timer::at(far_future);

    let mut dbus_open = true;
    let mut ui_open = true;

    // Helper: rearm the timer to the next notification deadline.
    macro_rules! rearm {
        () => {
            let at = core.earliest_deadline().unwrap_or(far_future);
            timer = Timer::at(at);
        };
    }

    // Helper: send a UiCommand, logging on error.
    macro_rules! send_ui {
        ($cmd:expr) => {
            if let Err(e) = ui_cmd_tx.send($cmd).await {
                log::warn!("notif-core: ui channel closed: {e}");
            }
        };
    }

    // Helper: send all signals in a Vec.
    macro_rules! send_signals {
        ($sigs:expr) => {
            for sig in $sigs {
                if let Err(e) = dbus_signal_tx.send(sig).await {
                    log::warn!("notif-core: dbus signal channel closed: {e}");
                }
            }
        };
    }

    // Helper: push SetCenter if active/history changed and the center is visible.
    macro_rules! push_center_if_dirty {
        () => {
            if core.take_center_dirty() && core.center_visible {
                let c = core.center_cmd();
                send_ui!(c);
            }
        };
    }

    enum Event {
        DbusCmd(DbusCmd),
        UiEvent(UiEvent),
        Config(ConfigEvent),
        Ipc(IpcCmd),
        Tick,
        DbusClosed,
        UiClosed,
    }

    loop {
        if !dbus_open && !ui_open {
            log::info!("notif-core: both primary channels closed; shutting down");
            break;
        }

        // Build per-branch futures.  Closed channels use `pending()` so they
        // never win the race.
        let dbus_fut = async {
            if dbus_open {
                match dbus_cmd_rx.recv().await {
                    Ok(cmd) => Event::DbusCmd(cmd),
                    Err(_) => Event::DbusClosed,
                }
            } else {
                future::pending().await
            }
        };

        let ui_fut = async {
            if ui_open {
                match ui_event_rx.recv().await {
                    Ok(ev) => Event::UiEvent(ev),
                    Err(_) => Event::UiClosed,
                }
            } else {
                future::pending().await
            }
        };

        let config_fut = async {
            match config_rx.recv().await {
                Ok(ev) => Event::Config(ev),
                Err(_) => future::pending().await,
            }
        };

        let ipc_fut = async {
            match ipc_rx.recv().await {
                Ok(cmd) => Event::Ipc(cmd),
                Err(_) => future::pending().await,
            }
        };

        let tick_fut = async {
            (&mut timer).await;
            Event::Tick
        };

        // Race all five branches.
        let event = future::or(
            future::or(
                future::or(dbus_fut, ui_fut),
                future::or(config_fut, ipc_fut),
            ),
            tick_fut,
        )
        .await;

        match event {
            Event::DbusClosed => {
                log::info!("notif-core: dbus channel closed");
                dbus_open = false;
            }

            Event::UiClosed => {
                log::info!("notif-core: ui channel closed");
                ui_open = false;
            }

            Event::DbusCmd(cmd) => {
                let now = core.clock.now();
                match cmd {
                    DbusCmd::Notify {
                        n,
                        replaces_id,
                        reply,
                    } => {
                        let (id, signals, ui_cmd) = core.handle_notify(n, replaces_id, now);
                        let _ = reply.send(id).await;
                        send_signals!(signals);
                        send_ui!(ui_cmd);
                    }
                    DbusCmd::Close { id, reply } => {
                        let signals = core.handle_close(id);
                        let _ = reply.send(()).await;
                        send_signals!(signals);
                        let sync = core.sync_cmd();
                        send_ui!(sync);
                    }
                }
                rearm!();
            }

            Event::UiEvent(ev) => {
                let now = core.clock.now();
                match ev {
                    UiEvent::DismissRequested(id) => {
                        let signals = core.handle_dismiss(id);
                        send_signals!(signals);
                        let sync = core.sync_cmd();
                        send_ui!(sync);
                    }
                    UiEvent::BodyClicked(id) => {
                        let signals = core.handle_body_click(id);
                        send_signals!(signals);
                        let sync = core.sync_cmd();
                        send_ui!(sync);
                    }
                    UiEvent::ActionInvoked { id, key } => {
                        let signals = core.handle_action_invoked(id, key);
                        send_signals!(signals);
                        let sync = core.sync_cmd();
                        send_ui!(sync);
                    }
                    UiEvent::HoverChanged { id, hovered } => {
                        core.handle_hover(id, hovered, now);
                        let sync = core.sync_cmd();
                        send_ui!(sync);
                        // hover does not change history — skip tail dirty-check
                        rearm!();
                        continue;
                    }
                    UiEvent::OutputsChanged => {
                        // Nothing to do in core; UI handles relayout.
                        // No history mutation — skip tail dirty-check.
                        rearm!();
                        continue;
                    }
                    UiEvent::HistoryRemoveRequested(id) => {
                        core.handle_remove_history(id);
                    }
                    UiEvent::ClearHistoryRequested => {
                        core.handle_clear_history();
                    }
                }
                rearm!();
            }

            Event::Config(ConfigEvent(config)) => {
                let now = core.clock.now();
                let cmds = core.handle_config(config, now);
                for cmd in cmds {
                    send_ui!(cmd);
                }
                rearm!();
            }

            Event::Ipc(ipc) => {
                let now = core.clock.now();
                match ipc {
                    IpcCmd::DismissAll => {
                        let signals = core.handle_dismiss_all(now);
                        send_signals!(signals);
                        let sync = core.sync_cmd();
                        send_ui!(sync);
                    }
                    IpcCmd::Close { id } => {
                        let signals = core.handle_close(id);
                        send_signals!(signals);
                        let sync = core.sync_cmd();
                        send_ui!(sync);
                    }
                    IpcCmd::History { reply } => {
                        let entries = core.handle_query_history();
                        let _ = reply.send(entries).await;
                        // Query-only — no history mutation; skip tail dirty-check.
                        rearm!();
                        continue;
                    }
                    IpcCmd::ClearHistory => {
                        core.handle_clear_history();
                    }
                    IpcCmd::ToggleDnd { reply } => {
                        let new_state = core.handle_toggle_dnd();
                        let _ = reply.send(new_state).await;
                        // DND toggle does not mutate history; skip tail dirty-check.
                        rearm!();
                        continue;
                    }
                    IpcCmd::ToggleCenter { reply } => {
                        let new_state = core.handle_toggle_center();
                        let _ = reply.send(new_state).await;
                        // Always push SetCenter on toggle (regardless of history dirty).
                        let center = core.center_cmd();
                        send_ui!(center);
                        // Clear dirty so the loop-tail dirty-check does not double-send.
                        core.take_center_dirty();
                    }
                    IpcCmd::Status { reply } => {
                        let status = core.handle_query_status();
                        let _ = reply.send(status).await;
                        // Query-only — no history mutation; skip tail dirty-check.
                        rearm!();
                        continue;
                    }
                }
                rearm!();
            }

            Event::Tick => {
                let now = core.clock.now();
                let (signals, changed) = core.tick(now);
                send_signals!(signals);
                if changed {
                    let sync = core.sync_cmd();
                    send_ui!(sync);
                }
                rearm!();
            }
        }

        // ── Loop tail: single dirty-check ────────────────────────────────────
        // Any event that mutated the active list or history reaches here; arms
        // that do NOT mutate either (HoverChanged, OutputsChanged, ToggleDnd,
        // History query, Status query) use `continue` to skip this check.
        // ToggleCenter clears the dirty flag before reaching here so it cannot
        // double-send.
        push_center_if_dirty!();
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;

    // ── MockClock ──────────────────────────────────────────────────────────────

    #[derive(Clone)]
    struct MockClock(Rc<Cell<Instant>>);

    impl MockClock {
        fn new() -> Self {
            Self(Rc::new(Cell::new(Instant::now())))
        }

        fn advance(&self, d: Duration) {
            self.0.set(self.0.get() + d);
        }
    }

    impl Clock for MockClock {
        fn now(&self) -> Instant {
            self.0.get()
        }
    }

    // ── Helpers ────────────────────────────────────────────────────────────────

    fn default_config() -> Arc<Config> {
        Arc::new(Config::default())
    }

    fn make_core(config: Arc<Config>) -> Core<MockClock> {
        Core::new(config, MockClock::new())
    }

    fn simple_new(summary: &str) -> Box<NewNotification> {
        Box::new(NewNotification {
            app_name: "test".into(),
            app_icon: String::new(),
            summary: summary.into(),
            body: String::new(),
            actions: vec![],
            urgency: Urgency::Normal,
            expire_timeout: Timeout::Default,
            image: None,
            transient: false,
            resident: false,
            category: None,
            desktop_entry: None,
            raw_hints: Default::default(),
        })
    }

    fn simple_new_with(
        summary: &str,
        urgency: Urgency,
        timeout: Timeout,
        transient: bool,
        resident: bool,
        image: Option<ImageSource>,
    ) -> Box<NewNotification> {
        Box::new(NewNotification {
            app_name: "test".into(),
            app_icon: String::new(),
            summary: summary.into(),
            body: String::new(),
            actions: vec![],
            urgency,
            expire_timeout: timeout,
            image,
            transient,
            resident,
            category: None,
            desktop_entry: None,
            raw_hints: Default::default(),
        })
    }

    fn notify(core: &mut Core<MockClock>, summary: &str) -> u32 {
        let now = core.clock.now();
        let (id, _, _) = core.handle_notify(simple_new(summary), 0, now);
        id
    }

    fn simple_new_with_actions(
        summary: &str,
        actions: Vec<notif_types::Action>,
        resident: bool,
    ) -> Box<NewNotification> {
        Box::new(NewNotification {
            app_name: "test".into(),
            app_icon: String::new(),
            summary: summary.into(),
            body: String::new(),
            actions,
            urgency: Urgency::Normal,
            expire_timeout: Timeout::Default,
            image: None,
            transient: false,
            resident,
            category: None,
            desktop_entry: None,
            raw_hints: Default::default(),
        })
    }

    fn notify_with_actions(
        core: &mut Core<MockClock>,
        summary: &str,
        actions: Vec<notif_types::Action>,
        resident: bool,
    ) -> u32 {
        let now = core.clock.now();
        let (id, _, _) =
            core.handle_notify(simple_new_with_actions(summary, actions, resident), 0, now);
        id
    }

    // ── Phase-1 tests (unchanged) ──────────────────────────────────────────────

    #[test]
    fn test_notify_assigns_id() {
        let mut core = make_core(default_config());
        let now = core.clock.now();
        let (id, signals, cmd) = core.handle_notify(simple_new("hello"), 0, now);
        assert_eq!(id, 1);
        assert!(signals.is_empty());
        assert_eq!(core.active.len(), 1);
        assert!(matches!(cmd, UiCommand::Sync(_)));
    }

    #[test]
    fn test_replaces_id_in_place() {
        let mut core = make_core(default_config());
        let now = core.clock.now();
        let (id1, _, _) = core.handle_notify(simple_new("first"), 0, now);
        // Insert a second to check position is preserved.
        let (id2, _, _) = core.handle_notify(simple_new("second"), 0, now);
        // Replace id2 in place.
        let (returned_id, signals, _) = core.handle_notify(simple_new("second-updated"), id2, now);
        assert_eq!(returned_id, id2);
        assert!(signals.is_empty());
        assert_eq!(core.active.len(), 2);
        // id1 is still in active.
        assert!(core.active.iter().any(|a| a.n.id == id1));
        // Updated summary visible.
        assert!(
            core.active
                .iter()
                .any(|a| a.n.id == id2 && a.n.summary == "second-updated")
        );
    }

    #[test]
    fn test_replaces_id_unknown_new_id() {
        let mut core = make_core(default_config());
        let now = core.clock.now();
        let (id, _, _) = core.handle_notify(simple_new("first"), 999, now);
        // 999 is not in active/waiting, so a fresh id should be assigned.
        assert_eq!(id, 1);
    }

    #[test]
    fn test_expiry() {
        let config = default_config();
        let mut core = make_core(config);
        let now = core.clock.now();
        // Normal urgency default timeout is 8000 ms.
        let (id, _, _) = core.handle_notify(simple_new("expiring"), 0, now);
        // Advance past 8000 ms.
        core.clock.advance(Duration::from_millis(9000));
        let now2 = core.clock.now();
        let (signals, changed) = core.tick(now2);
        assert!(changed);
        assert_eq!(signals.len(), 1);
        assert!(matches!(
            &signals[0],
            DbusSignal::NotificationClosed { id: sid, reason: CloseReason::Expired }
            if *sid == id
        ));
        assert!(core.active.is_empty());
    }

    #[test]
    fn test_dismiss() {
        let mut core = make_core(default_config());
        let id = notify(&mut core, "to-dismiss");
        let signals = core.handle_dismiss(id);
        assert_eq!(signals.len(), 1);
        assert!(matches!(
            &signals[0],
            DbusSignal::NotificationClosed { id: sid, reason: CloseReason::Dismissed }
            if *sid == id
        ));
        assert!(core.active.is_empty());
    }

    #[test]
    fn test_close_known() {
        let mut core = make_core(default_config());
        let id = notify(&mut core, "to-close");
        let signals = core.handle_close(id);
        assert_eq!(signals.len(), 1);
        assert!(matches!(
            &signals[0],
            DbusSignal::NotificationClosed { id: sid, reason: CloseReason::CloseCall }
            if *sid == id
        ));
    }

    #[test]
    fn test_close_unknown() {
        let mut core = make_core(default_config());
        let signals = core.handle_close(9999);
        assert!(signals.is_empty());
    }

    #[test]
    fn test_action_invoked_signal_order() {
        let mut core = make_core(default_config());
        let id = notify(&mut core, "actionable");
        let signals = core.handle_action_invoked(id, "default".into());
        assert_eq!(signals.len(), 2);
        assert!(matches!(&signals[0], DbusSignal::ActionInvoked { .. }));
        assert!(matches!(
            &signals[1],
            DbusSignal::NotificationClosed {
                reason: CloseReason::Dismissed,
                ..
            }
        ));
        assert!(core.active.is_empty());
    }

    #[test]
    fn test_action_invoked_resident() {
        let mut core = make_core(default_config());
        let now = core.clock.now();
        let (id, _, _) = core.handle_notify(
            simple_new_with(
                "resident",
                Urgency::Normal,
                Timeout::Default,
                false,
                true,
                None,
            ),
            0,
            now,
        );
        let signals = core.handle_action_invoked(id, "reply".into());
        // Only ActionInvoked, no close.
        assert_eq!(signals.len(), 1);
        assert!(matches!(&signals[0], DbusSignal::ActionInvoked { .. }));
        assert_eq!(core.active.len(), 1);
    }

    #[test]
    fn test_hover_pause_resume() {
        let config = default_config();
        let mut core = make_core(config);
        let now = core.clock.now();
        let (id, _, _) = core.handle_notify(simple_new("hover-me"), 0, now);

        // Advance 2 s and hover.
        core.clock.advance(Duration::from_secs(2));
        let hover_start = core.clock.now();
        core.handle_hover(id, true, hover_start);

        // Deadline should be gone while hovered.
        let entry = core.active.iter().find(|a| a.n.id == id).unwrap();
        assert!(entry.deadline.is_none());
        assert!(entry.paused.is_some());

        // Advance another 10 s (still hovered) — should NOT expire.
        core.clock.advance(Duration::from_secs(10));
        let (signals, _) = core.tick(core.clock.now());
        assert!(signals.is_empty());

        // Un-hover: remaining should be ~6 s (8000 - 2000 ms used before hover).
        let un_hover = core.clock.now();
        core.handle_hover(id, false, un_hover);
        let entry = core.active.iter().find(|a| a.n.id == id).unwrap();
        assert!(entry.deadline.is_some());
        let remaining = entry.deadline.unwrap().saturating_duration_since(un_hover);
        // remaining ≈ 6000 ms; allow some slack.
        assert!(remaining > Duration::from_millis(5500));
        assert!(remaining < Duration::from_millis(6500));
    }

    #[test]
    fn test_queue_overflow() {
        let config = Arc::new(Config {
            max_visible: 2,
            ..Config::default()
        });
        let mut core = make_core(config);
        let now = core.clock.now();

        let id1 = notify(&mut core, "n1");
        let id2 = notify(&mut core, "n2");
        let _id3 = notify(&mut core, "n3"); // goes to waiting

        assert_eq!(core.active.len(), 2);
        assert_eq!(core.waiting.len(), 1);

        // Close one active → n3 should be promoted.
        let _ = core.handle_close(id1);
        assert_eq!(core.active.len(), 2);
        assert!(core.waiting.is_empty());
        // id2 still active, n3 promoted.
        assert!(core.active.iter().any(|a| a.n.id == id2));
        let _ = now; // suppress warning
    }

    #[test]
    fn test_dismiss_all_promotes_waiting() {
        let config = Arc::new(Config {
            max_visible: 2,
            ..Config::default()
        });
        let mut core = make_core(config);
        let now = core.clock.now();

        let id1 = notify(&mut core, "n1");
        let id2 = notify(&mut core, "n2");
        let _id3 = notify(&mut core, "n3"); // goes to waiting
        assert_eq!(core.waiting.len(), 1);

        let signals = core.handle_dismiss_all(now);

        // Both active dismissed, with signals.
        assert_eq!(signals.len(), 2);
        // Active is ordered newest-first, so n2 is drained before n1.
        for (sig, want) in signals.iter().zip([id2, id1]) {
            assert!(matches!(
                sig,
                DbusSignal::NotificationClosed { id, reason: CloseReason::Dismissed } if *id == want
            ));
        }
        // Waiting notification is promoted immediately with a fresh deadline.
        assert!(core.waiting.is_empty());
        assert_eq!(core.active.len(), 1);
        let promoted = &core.active[0];
        assert_eq!(promoted.n.summary, "n3");
        assert!(promoted.deadline.is_some_and(|dl| dl > now));
    }

    #[test]
    fn test_history_cap() {
        let config = Arc::new(Config {
            history_limit: 3,
            max_visible: 100,
            ..Config::default()
        });
        let mut core = make_core(config);

        for i in 0..5u32 {
            let id = notify(&mut core, &format!("n{i}"));
            core.handle_dismiss(id);
        }

        assert_eq!(core.history.len(), 3);
        // Oldest two evicted; newest three kept.
        assert!(core.history.iter().any(|n| n.summary == "n2"));
        assert!(core.history.iter().any(|n| n.summary == "n3"));
        assert!(core.history.iter().any(|n| n.summary == "n4"));
    }

    #[test]
    fn test_transient_excluded_from_history() {
        let mut core = make_core(default_config());
        let now = core.clock.now();
        let (id, _, _) = core.handle_notify(
            simple_new_with(
                "transient",
                Urgency::Normal,
                Timeout::Default,
                true,
                false,
                None,
            ),
            0,
            now,
        );
        core.handle_dismiss(id);
        assert!(core.history.is_empty());
    }

    #[test]
    fn test_raw_image_stripped_in_history() {
        use notif_types::RawImage;
        let raw = RawImage {
            width: 1,
            height: 1,
            rowstride: 4,
            has_alpha: true,
            bits_per_sample: 8,
            channels: 4,
            data: vec![255, 0, 0, 255],
        };
        let mut core = make_core(default_config());
        let now = core.clock.now();
        let (id, _, _) = core.handle_notify(
            simple_new_with(
                "with-image",
                Urgency::Normal,
                Timeout::Default,
                false,
                false,
                Some(ImageSource::Data(raw)),
            ),
            0,
            now,
        );
        core.handle_dismiss(id);
        let hist = core.history.front().unwrap();
        assert!(
            hist.image.is_none(),
            "raw image should be stripped from history"
        );
    }

    #[test]
    fn test_config_reload_future_defaults() {
        let mut core = make_core(default_config());
        let now = core.clock.now();
        // Notify with default timeout (uses config 8000 ms).
        let (id, _, _) = core.handle_notify(simple_new("n1"), 0, now);
        let original_dl = core.active.iter().find(|a| a.n.id == id).unwrap().deadline;

        // Change config: low timeout to 1000 ms (but n1 is Normal, not Low).
        let mut new_cfg = Config::default();
        new_cfg.low.default_timeout_ms = 1000;
        core.handle_config(Arc::new(new_cfg), now);

        // Existing deadline should be unchanged (live notifications keep their deadline).
        let current_dl = core.active.iter().find(|a| a.n.id == id).unwrap().deadline;
        assert_eq!(original_dl, current_dl);
    }

    #[test]
    fn test_config_reload_history_recap() {
        let mut core = make_core(Arc::new(Config {
            history_limit: 10,
            max_visible: 20,
            ..Config::default()
        }));

        for i in 0..8u32 {
            let id = notify(&mut core, &format!("n{i}"));
            core.handle_dismiss(id);
        }
        assert_eq!(core.history.len(), 8);

        // Reduce history_limit to 3.
        let new_cfg = Config {
            history_limit: 3,
            max_visible: 20,
            ..Config::default()
        };
        core.handle_config(Arc::new(new_cfg), core.clock.now());
        assert_eq!(core.history.len(), 3);
    }

    #[test]
    fn test_id_wrap() {
        let mut core = make_core(default_config());
        // Manually set next_id near u32::MAX.
        core.next_id = u32::MAX;
        // Occupy id u32::MAX by adding a fake active notification.
        // (We do this by calling handle_notify once to get id u32::MAX assigned,
        //  then bumping next_id back to MAX-1 so the next call gets MAX.)
        // Simpler: just set next_id to MAX and let assign_id find u32::MAX free.
        let now = core.clock.now();
        let (id_max, _, _) = core.handle_notify(simple_new("at-max"), 0, now);
        // next_id should have wrapped: after MAX it goes to 1.
        let (id_one, _, _) = core.handle_notify(simple_new("wrapped"), 0, now);
        assert_eq!(id_max, u32::MAX);
        assert_eq!(id_one, 1);
    }

    #[test]
    fn test_ignore_timeout() {
        // ignore_timeout=true: config default_timeout_ms wins over client Millis.
        let mut cfg = Config::default();
        cfg.normal.ignore_timeout = true;
        cfg.normal.default_timeout_ms = 2000;
        let config = Arc::new(cfg);
        let mut core = make_core(config);
        let now = core.clock.now();
        // Client sends Millis(60000) but config says ignore_timeout.
        let (id, _, _) = core.handle_notify(
            simple_new_with(
                "ignored",
                Urgency::Normal,
                Timeout::Millis(60_000),
                false,
                false,
                None,
            ),
            0,
            now,
        );
        let entry = core.active.iter().find(|a| a.n.id == id).unwrap();
        let dl = entry.deadline.expect("should have deadline from config");
        let dur = dl.saturating_duration_since(now);
        // Should be ~2000 ms, not 60 000 ms.
        assert!(
            dur < Duration::from_millis(3000),
            "timeout should be capped by config"
        );
        assert!(dur > Duration::from_millis(1000));
    }

    #[test]
    fn test_replaces_id_in_waiting() {
        let config = Arc::new(Config {
            max_visible: 1,
            ..Config::default()
        });
        let mut core = make_core(config);
        let now = core.clock.now();

        let _id1 = notify(&mut core, "active");
        let (id2, _, _) = core.handle_notify(simple_new("waiting"), 0, now);
        assert_eq!(core.waiting.len(), 1);

        // Replace the waiting notification in-place.
        let (returned, signals, _) = core.handle_notify(simple_new("waiting-updated"), id2, now);
        assert_eq!(returned, id2);
        assert!(signals.is_empty());
        assert_eq!(core.waiting.len(), 1);
        assert_eq!(core.waiting[0].summary, "waiting-updated");
    }

    // ── A1: body-click / default-action tests ─────────────────────────────────

    #[test]
    fn test_body_click_with_default_action_invokes_and_closes() {
        let mut core = make_core(default_config());
        let actions = vec![notif_types::Action {
            key: "default".into(),
            label: "Open".into(),
        }];
        let id = notify_with_actions(&mut core, "clickable", actions, false);
        let signals = core.handle_body_click(id);
        // ActionInvoked BEFORE NotificationClosed.
        assert_eq!(signals.len(), 2, "expected exactly 2 signals");
        assert!(
            matches!(&signals[0], DbusSignal::ActionInvoked { id: sid, action_key } if *sid == id && action_key == "default"),
            "first signal must be ActionInvoked{{default}}, got: {:?}",
            &signals[0]
        );
        assert!(
            matches!(&signals[1], DbusSignal::NotificationClosed { id: sid, reason: CloseReason::Dismissed } if *sid == id),
            "second signal must be NotificationClosed{{Dismissed}}, got: {:?}",
            &signals[1]
        );
        assert!(core.active.is_empty(), "notification must be removed");
    }

    #[test]
    fn test_body_click_with_default_action_resident_stays() {
        let mut core = make_core(default_config());
        let actions = vec![notif_types::Action {
            key: "default".into(),
            label: "Open".into(),
        }];
        let id = notify_with_actions(&mut core, "resident-click", actions, true);
        let signals = core.handle_body_click(id);
        // Only ActionInvoked — no close for resident.
        assert_eq!(
            signals.len(),
            1,
            "expected 1 signal for resident notification"
        );
        assert!(
            matches!(&signals[0], DbusSignal::ActionInvoked { id: sid, action_key } if *sid == id && action_key == "default"),
            "signal must be ActionInvoked{{default}}, got: {:?}",
            &signals[0]
        );
        assert_eq!(
            core.active.len(),
            1,
            "resident notification must remain active"
        );
    }

    #[test]
    fn test_body_click_no_default_action_plain_dismiss() {
        let mut core = make_core(default_config());
        // Notification has an action, but NOT named "default".
        let actions = vec![notif_types::Action {
            key: "reply".into(),
            label: "Reply".into(),
        }];
        let id = notify_with_actions(&mut core, "no-default", actions, false);
        let signals = core.handle_body_click(id);
        // Plain dismiss: only NotificationClosed, no ActionInvoked.
        assert_eq!(signals.len(), 1, "expected 1 signal (plain dismiss)");
        assert!(
            matches!(&signals[0], DbusSignal::NotificationClosed { id: sid, reason: CloseReason::Dismissed } if *sid == id),
            "signal must be NotificationClosed{{Dismissed}}, got: {:?}",
            &signals[0]
        );
        assert!(core.active.is_empty(), "notification must be removed");
    }

    #[test]
    fn test_close_button_with_default_action_no_action_invoked() {
        let mut core = make_core(default_config());
        // Notification has a "default" action — close button must NOT invoke it.
        let actions = vec![notif_types::Action {
            key: "default".into(),
            label: "Open".into(),
        }];
        let id = notify_with_actions(&mut core, "close-btn-test", actions, false);
        let signals = core.handle_dismiss(id);
        // handle_dismiss (close-button path) must emit only NotificationClosed, no ActionInvoked.
        assert_eq!(signals.len(), 1, "expected 1 signal from close-button path");
        assert!(
            matches!(&signals[0], DbusSignal::NotificationClosed { id: sid, reason: CloseReason::Dismissed } if *sid == id),
            "signal must be NotificationClosed{{Dismissed}}, got: {:?}",
            &signals[0]
        );
        assert!(core.active.is_empty());
    }

    // ── Phase-2 tests: DND semantics ───────────────────────────────────────────

    #[test]
    fn test_dnd_hides_normal_goes_to_history_image_stripped_no_signal_id_assigned() {
        use notif_types::RawImage;
        let mut core = make_core(default_config());
        // Enable DND.
        let new_state = core.handle_toggle_dnd();
        assert!(new_state, "DND should now be on");

        let raw = RawImage {
            width: 1,
            height: 1,
            rowstride: 3,
            has_alpha: false,
            bits_per_sample: 8,
            channels: 3,
            data: vec![255, 0, 0],
        };
        let now = core.clock.now();
        let n = simple_new_with(
            "dnd-hidden",
            Urgency::Normal,
            Timeout::Default,
            false,
            false,
            Some(ImageSource::Data(raw)),
        );
        let (id, signals, _cmd) = core.handle_notify(n, 0, now);

        // ID must be assigned (non-zero).
        assert!(id > 0, "id must be assigned");
        // No signals emitted.
        assert!(signals.is_empty(), "no signals emitted while DND");
        // Not displayed.
        assert!(core.active.is_empty(), "notification must not be active");
        // In history.
        assert_eq!(core.history.len(), 1, "notification must be in history");
        // Image stripped.
        let hist = core.history.front().unwrap();
        assert!(hist.image.is_none(), "image must be stripped in history");
        // History dirty.
        assert!(core.take_center_dirty(), "center_dirty must be set");
    }

    #[test]
    fn test_dnd_critical_bypass() {
        let mut core = make_core(default_config());
        // Enable DND.
        core.handle_toggle_dnd();

        let now = core.clock.now();
        let n = simple_new_with(
            "critical",
            Urgency::Critical,
            Timeout::Default,
            false,
            false,
            None,
        );
        let (id, signals, _cmd) = core.handle_notify(n, 0, now);

        assert!(id > 0, "id assigned");
        assert!(
            signals.is_empty(),
            "no signals (critical has no automatic signal)"
        );
        // Critical bypasses DND — must appear in active.
        assert_eq!(core.active.len(), 1, "critical shown despite DND");
        assert!(core.history.is_empty(), "critical not added to history yet");
    }

    #[test]
    fn test_toggle_dnd_replies_new_state() {
        let mut core = make_core(default_config());

        // Initially off.
        assert!(!core.dnd);

        let state1 = core.handle_toggle_dnd();
        assert!(state1, "toggle from off → on");

        let state2 = core.handle_toggle_dnd();
        assert!(!state2, "toggle from on → off");
    }

    #[test]
    fn test_dnd_transient_still_excluded() {
        let mut core = make_core(default_config());
        core.handle_toggle_dnd(); // DND on

        let now = core.clock.now();
        let n = simple_new_with(
            "transient-dnd",
            Urgency::Normal,
            Timeout::Default,
            true, // transient
            false,
            None,
        );
        let (id, _, _) = core.handle_notify(n, 0, now);
        assert!(id > 0);
        // Transient → not added to history even under DND.
        assert!(
            core.history.is_empty(),
            "transient excluded from history even under DND"
        );
        assert!(
            !core.take_center_dirty(),
            "no history mutation for transient"
        );
    }

    #[test]
    fn test_replaces_dnd_hidden_id_treated_as_new() {
        let mut core = make_core(default_config());
        core.handle_toggle_dnd(); // DND on

        let now = core.clock.now();
        // First notification → DND-hidden, goes to history with id=1.
        let (id1, _, _) = core.handle_notify(simple_new("hidden"), 0, now);
        assert_eq!(id1, 1);
        assert_eq!(core.history.len(), 1);

        // Second notification with replaces_id=1 (the DND-hidden entry).
        // id1 is not in active or waiting, so this is treated as a new notification.
        let (id2, _, _) = core.handle_notify(simple_new("replacement"), id1, now);
        // Fresh id assigned (since 1 is not in active/waiting).
        assert_ne!(id2, id1, "must be treated as new notification");
        // Also goes to history under DND.
        assert_eq!(core.history.len(), 2, "both in history");
    }

    // ── Phase-2 tests: history query / remove / clear ──────────────────────────

    #[test]
    fn test_history_query_newest_first() {
        let mut core = make_core(default_config());
        let ids: Vec<u32> = (0..3)
            .map(|i| {
                let id = notify(&mut core, &format!("n{i}"));
                core.handle_dismiss(id);
                id
            })
            .collect();

        // History ring: front = oldest (n0), back = newest (n2).
        let entries = core.handle_query_history();
        assert_eq!(entries.len(), 3);
        // Newest first: n2, n1, n0.
        assert_eq!(entries[0].id, ids[2], "newest first");
        assert_eq!(entries[1].id, ids[1]);
        assert_eq!(entries[2].id, ids[0], "oldest last");
    }

    #[test]
    fn test_remove_from_history() {
        let mut core = make_core(default_config());
        let id1 = notify(&mut core, "keep");
        core.handle_dismiss(id1);
        let id2 = notify(&mut core, "remove-me");
        core.handle_dismiss(id2);
        assert_eq!(core.history.len(), 2);

        core.handle_remove_history(id2);

        assert_eq!(core.history.len(), 1);
        assert!(core.history.front().map(|n| n.id == id1).unwrap_or(false));
        assert!(core.take_center_dirty(), "dirty after remove");
    }

    #[test]
    fn test_remove_from_history_unknown_id_is_noop() {
        let mut core = make_core(default_config());
        let id = notify(&mut core, "entry");
        core.handle_dismiss(id);
        core.take_center_dirty(); // reset flag

        core.handle_remove_history(9999);
        assert_eq!(core.history.len(), 1, "unchanged");
        assert!(!core.take_center_dirty(), "not dirty for unknown id");
    }

    #[test]
    fn test_clear_history() {
        let mut core = make_core(default_config());
        for _ in 0..3 {
            let id = notify(&mut core, "entry");
            core.handle_dismiss(id);
        }
        assert_eq!(core.history.len(), 3);

        core.handle_clear_history();
        assert!(core.history.is_empty(), "history cleared");
        assert!(core.take_center_dirty(), "dirty after clear");
    }

    #[test]
    fn test_clear_history_empty_not_dirty() {
        let mut core = make_core(default_config());
        core.handle_clear_history(); // already empty
        assert!(!core.take_center_dirty(), "not dirty when already empty");
    }

    // ── Phase-2 tests: center tracking ────────────────────────────────────────

    #[test]
    fn test_toggle_center_replies_new_state() {
        let mut core = make_core(default_config());
        assert!(!core.center_visible);

        let v1 = core.handle_toggle_center();
        assert!(v1, "toggle from off → on");
        assert!(core.center_visible);

        let v2 = core.handle_toggle_center();
        assert!(!v2, "toggle from on → off");
        assert!(!core.center_visible);
    }

    #[test]
    fn test_center_cmd_entries_newest_first() {
        let mut core = make_core(default_config());

        let id1 = notify(&mut core, "oldest");
        core.handle_dismiss(id1);
        let id2 = notify(&mut core, "newer");
        core.handle_dismiss(id2);
        let id3 = notify(&mut core, "newest");
        core.handle_dismiss(id3);

        core.handle_toggle_center(); // visible = true

        let cmd = core.center_cmd();
        match cmd {
            UiCommand::SetCenter {
                visible,
                active,
                history,
            } => {
                assert!(visible);
                assert!(active.is_empty(), "all three were dismissed");
                assert_eq!(history.len(), 3);
                assert_eq!(history[0].notification.id, id3, "newest first");
                assert_eq!(history[2].notification.id, id1, "oldest last");
            }
            _ => panic!("expected SetCenter"),
        }
    }

    #[test]
    fn test_center_cmd_active_section_newest_first() {
        let mut core = make_core(default_config());
        let id1 = notify(&mut core, "first");
        let id2 = notify(&mut core, "second");
        core.handle_toggle_center();

        let cmd = core.center_cmd();
        match cmd {
            UiCommand::SetCenter {
                active, history, ..
            } => {
                assert!(history.is_empty());
                assert_eq!(active.len(), 2);
                // active[0] = newest (id2), matching self.active ordering.
                assert_eq!(active[0].notification.id, id2);
                assert_eq!(active[1].notification.id, id1);
            }
            _ => panic!("expected SetCenter"),
        }
    }

    #[test]
    fn test_center_cmd_caps_at_max_entries() {
        let mut cfg = Config::default();
        cfg.center.max_entries = Some(2);
        cfg.max_visible = 10;
        let mut core = make_core(Arc::new(cfg));

        let id1 = notify(&mut core, "active1");
        let _id2 = notify(&mut core, "active2");
        core.handle_toggle_center();

        let cmd = core.center_cmd();
        match cmd {
            UiCommand::SetCenter {
                active, history, ..
            } => {
                assert_eq!(active.len(), 2, "capped at max_entries, none for history");
                assert!(history.is_empty());
                let _ = id1;
            }
            _ => panic!("expected SetCenter"),
        }
    }

    #[test]
    fn test_notify_while_center_visible_sets_center_dirty() {
        let mut core = make_core(default_config());
        core.handle_toggle_center();
        core.take_center_dirty(); // reset from toggle

        notify(&mut core, "live");
        assert!(
            core.take_center_dirty(),
            "new active notification must dirty the center"
        );
    }

    #[test]
    fn test_dismiss_moves_entry_from_active_to_history_section() {
        let mut core = make_core(default_config());
        let id = notify(&mut core, "n");
        core.handle_toggle_center();

        let before = core.center_cmd();
        match before {
            UiCommand::SetCenter { active, .. } => assert_eq!(active.len(), 1),
            _ => panic!("expected SetCenter"),
        }

        core.handle_dismiss(id);
        let after = core.center_cmd();
        match after {
            UiCommand::SetCenter {
                active, history, ..
            } => {
                assert!(active.is_empty());
                assert_eq!(history.len(), 1);
                assert_eq!(history[0].notification.id, id);
            }
            _ => panic!("expected SetCenter"),
        }
    }

    #[test]
    fn test_center_dirty_set_on_add_via_dismiss() {
        let mut core = make_core(default_config());
        core.take_center_dirty(); // reset

        let id = notify(&mut core, "n");
        core.handle_dismiss(id);
        assert!(
            core.take_center_dirty(),
            "dirty after dismiss adds to history"
        );
    }

    /// Regression: dismissing a *transient* active notification must still
    /// dirty the center, even though `add_to_history` skips transient
    /// entries (they never reach history, but they must still disappear
    /// from the panel's active section).
    #[test]
    fn test_center_dirty_set_on_transient_dismiss_even_though_not_added_to_history() {
        let mut core = make_core(default_config());
        let now = core.clock.now();
        let (id, _, _) = core.handle_notify(
            simple_new_with(
                "transient-live",
                Urgency::Normal,
                Timeout::Default,
                true, // transient
                false,
                None,
            ),
            0,
            now,
        );
        core.take_center_dirty(); // reset after setup

        core.handle_dismiss(id);
        assert!(core.history.is_empty(), "transient never enters history");
        assert!(
            core.take_center_dirty(),
            "dirty must be set on active removal even for a transient entry"
        );
    }

    #[test]
    fn test_center_dirty_set_after_config_recap() {
        let mut core = make_core(Arc::new(Config {
            history_limit: 10,
            max_visible: 20,
            ..Config::default()
        }));
        for _ in 0..5 {
            let id = notify(&mut core, "entry");
            core.handle_dismiss(id);
        }
        core.take_center_dirty(); // reset after setup

        // Reduce history_limit — should evict entries and set dirty.
        let new_cfg = Config {
            history_limit: 2,
            max_visible: 20,
            ..Config::default()
        };
        core.handle_config(Arc::new(new_cfg), core.clock.now());
        assert!(core.take_center_dirty(), "dirty after history re-cap");
    }

    #[test]
    fn test_center_dirty_not_set_when_no_recap_needed() {
        let mut core = make_core(Arc::new(Config {
            history_limit: 10,
            max_visible: 20,
            ..Config::default()
        }));
        for _ in 0..3 {
            let id = notify(&mut core, "entry");
            core.handle_dismiss(id);
        }
        core.take_center_dirty(); // reset

        // Increase limit — no eviction, should NOT set dirty.
        let new_cfg = Config {
            history_limit: 20,
            max_visible: 20,
            ..Config::default()
        };
        core.handle_config(Arc::new(new_cfg), core.clock.now());
        assert!(!core.take_center_dirty(), "not dirty when no recap needed");
    }

    // ── Async smoke test ───────────────────────────────────────────────────────

    #[test]
    fn test_run_expiry_smoke() {
        async_io::block_on(async {
            use notif_types::{ConfigEvent, IpcCmd, UiCommand, UiEvent};

            let (dbus_cmd_tx, dbus_cmd_rx) = async_channel::unbounded::<DbusCmd>();
            let (dbus_signal_tx, dbus_signal_rx) = async_channel::unbounded::<DbusSignal>();
            let (ui_cmd_tx, ui_cmd_rx) = async_channel::unbounded::<UiCommand>();
            let (_ui_event_tx, ui_event_rx) = async_channel::unbounded::<UiEvent>();
            let (_config_tx, config_rx) = async_channel::unbounded::<ConfigEvent>();
            let (_ipc_tx, ipc_rx) = async_channel::unbounded::<IpcCmd>();

            let mut cfg = Config::default();
            cfg.normal.default_timeout_ms = 50; // 50 ms timeout
            cfg.normal.ignore_timeout = true;
            let config = Arc::new(cfg);

            let handles = CoreHandles {
                dbus_cmd_rx,
                dbus_signal_tx,
                ui_cmd_tx,
                ui_event_rx,
                config_rx,
                ipc_rx,
            };

            // Spawn core as a background future.
            let core_fut = run(config.clone(), handles);

            // Send one notification and wait for expiry.
            let driver = async {
                // Send Notify.
                let (reply_tx, reply_rx) = async_channel::bounded::<u32>(1);
                dbus_cmd_tx
                    .send(DbusCmd::Notify {
                        n: Box::new(NewNotification {
                            app_name: "smoke".into(),
                            app_icon: String::new(),
                            summary: "smoke-test".into(),
                            body: String::new(),
                            actions: vec![],
                            urgency: Urgency::Normal,
                            expire_timeout: Timeout::Default,
                            image: None,
                            transient: false,
                            resident: false,
                            category: None,
                            desktop_entry: None,
                            raw_hints: Default::default(),
                        }),
                        replaces_id: 0,
                        reply: reply_tx,
                    })
                    .await
                    .unwrap();
                let id = reply_rx.recv().await.unwrap();
                assert!(id > 0);

                // First UiCommand::Sync should have 1 notification.
                let cmd = ui_cmd_rx.recv().await.unwrap();
                assert!(matches!(&cmd, UiCommand::Sync(ns) if ns.len() == 1));

                // Wait for Expired signal (≤500 ms).
                let sig =
                    futures_lite::future::or(async { dbus_signal_rx.recv().await.ok() }, async {
                        async_io::Timer::after(Duration::from_millis(500)).await;
                        None
                    })
                    .await;
                assert!(
                    matches!(
                        &sig,
                        Some(DbusSignal::NotificationClosed {
                            reason: CloseReason::Expired,
                            ..
                        })
                    ),
                    "expected Expired signal, got: {sig:?}"
                );

                // Second UiCommand::Sync should have 0 notifications.
                let cmd2 = ui_cmd_rx.recv().await.unwrap();
                assert!(matches!(&cmd2, UiCommand::Sync(ns) if ns.is_empty()));

                // Close channels so core exits.
                drop(dbus_cmd_tx);
            };

            futures_lite::future::or(core_fut, driver).await;
        });
    }

    // ── E4 regression tests: loop-tail center-push discipline ─────────────────

    /// Helper: spin up `run()` and return the channel endpoints.
    fn spawn_core(
        ex: &async_executor::LocalExecutor<'_>,
        config: Arc<Config>,
    ) -> (
        async_channel::Sender<DbusCmd>,
        async_channel::Sender<UiEvent>,
        async_channel::Receiver<UiCommand>,
        async_channel::Sender<IpcCmd>,
    ) {
        let (dbus_cmd_tx, dbus_cmd_rx) = async_channel::unbounded::<DbusCmd>();
        let (dbus_signal_tx, _dbus_signal_rx) = async_channel::unbounded::<DbusSignal>();
        let (ui_cmd_tx, ui_cmd_rx) = async_channel::unbounded::<UiCommand>();
        let (ui_event_tx, ui_event_rx) = async_channel::unbounded::<UiEvent>();
        let (_config_tx, config_rx) = async_channel::unbounded::<ConfigEvent>();
        let (ipc_tx, ipc_rx) = async_channel::unbounded::<IpcCmd>();

        let handles = CoreHandles {
            dbus_cmd_rx,
            dbus_signal_tx,
            ui_cmd_tx,
            ui_event_rx,
            config_rx,
            ipc_rx,
        };

        ex.spawn(run(config, handles)).detach();
        (dbus_cmd_tx, ui_event_tx, ui_cmd_rx, ipc_tx)
    }

    /// Drain all currently pending messages from a receiver without blocking.
    async fn drain_pending<T>(rx: &async_channel::Receiver<T>) -> Vec<T> {
        let mut out = Vec::new();
        while let Ok(v) = rx.try_recv() {
            out.push(v);
        }
        out
    }

    /// Count `SetCenter` commands in a slice of `UiCommand`s.
    fn count_set_center(cmds: &[UiCommand]) -> usize {
        cmds.iter()
            .filter(|c| matches!(c, UiCommand::SetCenter { .. }))
            .count()
    }

    /// (a) A history-mutating event (dismiss) while center visible yields exactly
    /// ONE SetCenter — not zero, not two.
    #[test]
    fn e4_history_mutation_while_center_visible_yields_one_set_center() {
        async_io::block_on(async {
            let ex = async_executor::LocalExecutor::new();
            ex.run(async {
                let (dbus_tx, _ui_event_tx, ui_cmd_rx, ipc_tx) = spawn_core(&ex, default_config());

                // Give core a tick to start.
                async_io::Timer::after(std::time::Duration::from_millis(10)).await;

                // Toggle center open.
                let (reply_tx, reply_rx) = async_channel::bounded::<bool>(1);
                ipc_tx
                    .send(IpcCmd::ToggleCenter { reply: reply_tx })
                    .await
                    .unwrap();
                assert!(reply_rx.recv().await.unwrap(), "center should be visible");

                // Drain pending commands (the SetCenter from ToggleCenter + initial Sync).
                async_io::Timer::after(std::time::Duration::from_millis(10)).await;
                drain_pending(&ui_cmd_rx).await;

                // Send a notification and then dismiss it (puts it in history).
                let (id_tx, id_rx) = async_channel::bounded::<u32>(1);
                dbus_tx
                    .send(DbusCmd::Notify {
                        n: Box::new(NewNotification {
                            app_name: "e4test".into(),
                            app_icon: String::new(),
                            summary: "e4".into(),
                            body: String::new(),
                            actions: vec![],
                            urgency: Urgency::Normal,
                            expire_timeout: Timeout::Never,
                            image: None,
                            transient: false,
                            resident: false,
                            category: None,
                            desktop_entry: None,
                            raw_hints: Default::default(),
                        }),
                        replaces_id: 0,
                        reply: id_tx,
                    })
                    .await
                    .unwrap();
                let id = id_rx.recv().await.unwrap();

                // Drain the Sync from Notify.
                async_io::Timer::after(std::time::Duration::from_millis(10)).await;
                drain_pending(&ui_cmd_rx).await;

                // Dismiss (puts notification into history → center_dirty = true).
                let (close_tx, close_rx) = async_channel::bounded::<()>(1);
                dbus_tx
                    .send(DbusCmd::Close {
                        id,
                        reply: close_tx,
                    })
                    .await
                    .unwrap();
                close_rx.recv().await.unwrap();

                // Give core time to process.
                async_io::Timer::after(std::time::Duration::from_millis(20)).await;

                let cmds = drain_pending(&ui_cmd_rx).await;
                let set_center_count = count_set_center(&cmds);
                assert_eq!(
                    set_center_count, 1,
                    "expected exactly 1 SetCenter, got {set_center_count}; cmds={cmds:?}"
                );
            })
            .await;
        });
    }

    /// (a2) A new notification arriving while center visible yields exactly
    /// ONE SetCenter — the panel must update live, not just on close/dismiss.
    #[test]
    fn e4_notify_while_center_visible_yields_exactly_one_set_center() {
        async_io::block_on(async {
            let ex = async_executor::LocalExecutor::new();
            ex.run(async {
                let (dbus_tx, _ui_event_tx, ui_cmd_rx, ipc_tx) = spawn_core(&ex, default_config());

                async_io::Timer::after(std::time::Duration::from_millis(10)).await;

                let (reply_tx, reply_rx) = async_channel::bounded::<bool>(1);
                ipc_tx
                    .send(IpcCmd::ToggleCenter { reply: reply_tx })
                    .await
                    .unwrap();
                assert!(reply_rx.recv().await.unwrap());

                async_io::Timer::after(std::time::Duration::from_millis(10)).await;
                drain_pending(&ui_cmd_rx).await;

                let (id_tx, id_rx) = async_channel::bounded::<u32>(1);
                dbus_tx
                    .send(DbusCmd::Notify {
                        n: Box::new(NewNotification {
                            app_name: "e4test".into(),
                            app_icon: String::new(),
                            summary: "live".into(),
                            body: String::new(),
                            actions: vec![],
                            urgency: Urgency::Normal,
                            expire_timeout: Timeout::Never,
                            image: None,
                            transient: false,
                            resident: false,
                            category: None,
                            desktop_entry: None,
                            raw_hints: Default::default(),
                        }),
                        replaces_id: 0,
                        reply: id_tx,
                    })
                    .await
                    .unwrap();
                let _id = id_rx.recv().await.unwrap();

                async_io::Timer::after(std::time::Duration::from_millis(20)).await;

                let cmds = drain_pending(&ui_cmd_rx).await;
                let set_center_count = count_set_center(&cmds);
                assert_eq!(
                    set_center_count, 1,
                    "expected exactly 1 SetCenter on new notification, got {set_center_count}; cmds={cmds:?}"
                );
                let has_active_entry = cmds.iter().any(|c| {
                    matches!(c, UiCommand::SetCenter { active, .. } if !active.is_empty())
                });
                assert!(has_active_entry, "SetCenter must include the live active entry");
            })
            .await;
        });
    }

    /// (b) HoverChanged does NOT produce any SetCenter (hover never touches history).
    #[test]
    fn e4_hover_changed_yields_no_set_center() {
        async_io::block_on(async {
            let ex = async_executor::LocalExecutor::new();
            ex.run(async {
                let (dbus_tx, ui_event_tx, ui_cmd_rx, ipc_tx) = spawn_core(&ex, default_config());

                async_io::Timer::after(std::time::Duration::from_millis(10)).await;

                // Toggle center open.
                let (reply_tx, reply_rx) = async_channel::bounded::<bool>(1);
                ipc_tx
                    .send(IpcCmd::ToggleCenter { reply: reply_tx })
                    .await
                    .unwrap();
                reply_rx.recv().await.unwrap();

                // Create a notification to hover over.
                let (id_tx, id_rx) = async_channel::bounded::<u32>(1);
                dbus_tx
                    .send(DbusCmd::Notify {
                        n: Box::new(NewNotification {
                            app_name: "hover-test".into(),
                            app_icon: String::new(),
                            summary: "hover".into(),
                            body: String::new(),
                            actions: vec![],
                            urgency: Urgency::Normal,
                            expire_timeout: Timeout::Never,
                            image: None,
                            transient: false,
                            resident: false,
                            category: None,
                            desktop_entry: None,
                            raw_hints: Default::default(),
                        }),
                        replaces_id: 0,
                        reply: id_tx,
                    })
                    .await
                    .unwrap();
                let id = id_rx.recv().await.unwrap();

                // Drain all pending before hover.
                async_io::Timer::after(std::time::Duration::from_millis(10)).await;
                drain_pending(&ui_cmd_rx).await;

                // Hover on and off.
                ui_event_tx
                    .send(UiEvent::HoverChanged { id, hovered: true })
                    .await
                    .unwrap();
                ui_event_tx
                    .send(UiEvent::HoverChanged { id, hovered: false })
                    .await
                    .unwrap();

                async_io::Timer::after(std::time::Duration::from_millis(20)).await;

                let cmds = drain_pending(&ui_cmd_rx).await;
                let set_center_count = count_set_center(&cmds);
                assert_eq!(
                    set_center_count, 0,
                    "HoverChanged must not produce SetCenter, got {set_center_count}"
                );
            })
            .await;
        });
    }

    /// (c) ToggleCenter yields exactly ONE SetCenter (unconditional push, no double-send).
    #[test]
    fn e4_toggle_center_yields_exactly_one_set_center() {
        async_io::block_on(async {
            let ex = async_executor::LocalExecutor::new();
            ex.run(async {
                let (_dbus_tx, _ui_event_tx, ui_cmd_rx, ipc_tx) = spawn_core(&ex, default_config());

                async_io::Timer::after(std::time::Duration::from_millis(10)).await;

                // Drain any initial commands.
                drain_pending(&ui_cmd_rx).await;

                // Toggle center — must emit exactly one SetCenter.
                let (reply_tx, reply_rx) = async_channel::bounded::<bool>(1);
                ipc_tx
                    .send(IpcCmd::ToggleCenter { reply: reply_tx })
                    .await
                    .unwrap();
                reply_rx.recv().await.unwrap();

                async_io::Timer::after(std::time::Duration::from_millis(20)).await;

                let cmds = drain_pending(&ui_cmd_rx).await;
                let set_center_count = count_set_center(&cmds);
                assert_eq!(
                    set_center_count, 1,
                    "ToggleCenter must produce exactly 1 SetCenter, got {set_center_count}"
                );
            })
            .await;
        });
    }
}
