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
    CloseReason, ConfigEvent, DbusCmd, DbusSignal, DisplayNotification, ImageSource, IpcCmd,
    NewNotification, Notification, Timeout, UiCommand, UiEvent, Urgency,
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
    n: Notification,
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
    history: VecDeque<Notification>,
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
        }
    }

    // ── ID assignment ─────────────────────────────────────────────────────────

    fn assign_id(&mut self, n: Box<NewNotification>) -> Notification {
        let id = self.next_fresh_id();
        Notification {
            id,
            app_name: n.app_name,
            app_icon: n.app_icon,
            summary: n.summary,
            body: n.body,
            actions: n.actions,
            urgency: n.urgency,
            expire_timeout: n.expire_timeout,
            image: n.image,
            transient: n.transient,
            resident: n.resident,
            category: n.category,
            desktop_entry: n.desktop_entry,
            created_at: std::time::SystemTime::now(),
            raw_hints: n.raw_hints,
        }
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
    pub fn handle_notify(
        &mut self,
        n: Box<NewNotification>,
        replaces_id: u32,
        now: Instant,
    ) -> (u32, Vec<DbusSignal>, UiCommand) {
        // Replace in active if replaces_id is found there.
        if replaces_id != 0 {
            if let Some(pos) = self.active.iter().position(|a| a.n.id == replaces_id) {
                let notification = Notification {
                    id: replaces_id,
                    app_name: n.app_name,
                    app_icon: n.app_icon,
                    summary: n.summary,
                    body: n.body,
                    actions: n.actions,
                    urgency: n.urgency,
                    expire_timeout: n.expire_timeout,
                    image: n.image,
                    transient: n.transient,
                    resident: n.resident,
                    category: n.category,
                    desktop_entry: n.desktop_entry,
                    created_at: std::time::SystemTime::now(),
                    raw_hints: n.raw_hints,
                };
                let deadline = self.compute_deadline(&notification, now);
                if let Some(entry) = self.active.get_mut(pos) {
                    entry.n = notification;
                    entry.deadline = deadline;
                    entry.paused = None;
                    // Keep hovered state if the pointer is still there.
                }
                return (replaces_id, vec![], self.sync_cmd());
            }

            // Replace in waiting queue.
            if let Some(pos) = self.waiting.iter().position(|w| w.id == replaces_id) {
                let notification = Notification {
                    id: replaces_id,
                    app_name: n.app_name,
                    app_icon: n.app_icon,
                    summary: n.summary,
                    body: n.body,
                    actions: n.actions,
                    urgency: n.urgency,
                    expire_timeout: n.expire_timeout,
                    image: n.image,
                    transient: n.transient,
                    resident: n.resident,
                    category: n.category,
                    desktop_entry: n.desktop_entry,
                    created_at: std::time::SystemTime::now(),
                    raw_hints: n.raw_hints,
                };
                if let Some(slot) = self.waiting.get_mut(pos) {
                    *slot = notification;
                }
                return (replaces_id, vec![], self.sync_cmd());
            }
        }

        // New notification.
        let notification = self.assign_id(n);
        let id = notification.id;

        if self.active.len() < self.config.max_visible {
            let deadline = self.compute_deadline(&notification, now);
            self.active.insert(
                0,
                ActiveNotification {
                    n: notification,
                    deadline,
                    paused: None,
                    hovered: false,
                },
            );
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
            self.add_to_history(n);
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

        // Re-cap history.
        while self.history.len() > new_history_limit {
            self.history.pop_front();
        }

        // Demote newest notifications if max_visible shrank.
        while self.active.len() > new_max_visible {
            // active[0] is the newest; send it back to front of waiting.
            let entry = self.active.remove(0);
            self.waiting.push_front(entry.n);
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

    fn add_to_history(&mut self, mut n: Notification) {
        if n.transient {
            return;
        }
        // Strip raw image data — it can be large and is not needed in history.
        if matches!(n.image, Some(ImageSource::Data(_))) {
            n.image = None;
        }
        self.history.push_back(n);
        while self.history.len() > self.config.history_limit {
            self.history.pop_front();
        }
    }

    // ── Promotion ─────────────────────────────────────────────────────────────

    fn promote_from_waiting(&mut self, now: Instant) {
        while self.active.len() < self.config.max_visible {
            match self.waiting.pop_front() {
                Some(n) => {
                    let deadline = self.compute_deadline(&n, now);
                    // Promoted notifications are older; push to the end (lowest prominence).
                    self.active.push(ActiveNotification {
                        n,
                        deadline,
                        paused: None,
                        hovered: false,
                    });
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
                    }
                    UiEvent::OutputsChanged => {
                        // Nothing to do in core; UI handles relayout.
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
                match ipc {
                    IpcCmd::DismissAll => {
                        let now = core.clock.now();
                        let signals = core.handle_dismiss_all(now);
                        send_signals!(signals);
                        let sync = core.sync_cmd();
                        send_ui!(sync);
                    }
                    IpcCmd::History | IpcCmd::ToggleDnd | IpcCmd::ToggleCenter => {
                        // Phase 2 placeholder — not yet implemented.
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

    #[allow(dead_code)]
    fn active_summaries(core: &Core<MockClock>) -> Vec<String> {
        core.active.iter().map(|a| a.n.summary.clone()).collect()
    }

    // ── Tests ──────────────────────────────────────────────────────────────────

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
}
