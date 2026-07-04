#![forbid(unsafe_code)]

//! `notif-types` — shared vocabulary types for the notif daemon.
//! No I/O, no logic beyond constructors and `From` impls.

pub mod config;

pub use config::Config;

use std::collections::HashMap;
use std::sync::Arc;
use zvariant::OwnedValue;

/// A reply channel (bounded(1) sender) for request/response patterns.
pub type ReplyTx<T> = async_channel::Sender<T>;

/// Notification urgency level per the freedesktop spec.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    serde::Serialize,
    serde::Deserialize,
    Default,
)]
#[serde(rename_all = "lowercase")]
pub enum Urgency {
    /// Low urgency — advisory notifications.
    Low = 0,
    /// Normal urgency (default).
    #[default]
    Normal = 1,
    /// Critical urgency — must not be auto-dismissed.
    Critical = 2,
}

/// Notification expiry timeout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Timeout {
    /// Use the server's configured default for this urgency.
    Default,
    /// Never expire.
    Never,
    /// Expire after this many milliseconds.
    Millis(u32),
}

impl From<i32> for Timeout {
    fn from(v: i32) -> Self {
        match v {
            -1 => Timeout::Default,
            0 => Timeout::Never,
            ms if ms > 0 => Timeout::Millis(ms as u32),
            _ => Timeout::Default,
        }
    }
}

/// Raw image data from the `image-data` D-Bus hint (spec: `(iiibiiay)`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawImage {
    /// Image width in pixels.
    pub width: i32,
    /// Image height in pixels.
    pub height: i32,
    /// Row stride in bytes (may exceed width × channels).
    pub rowstride: i32,
    /// Whether the image has an alpha channel.
    pub has_alpha: bool,
    /// Bits per sample (usually 8).
    pub bits_per_sample: i32,
    /// Number of channels (3 for RGB, 4 for RGBA).
    pub channels: i32,
    /// Raw pixel data.
    pub data: Vec<u8>,
}

/// Image source attached to a notification.
#[derive(Debug, Clone)]
pub enum ImageSource {
    /// Inline pixel data from the `image-data` hint.
    Data(RawImage),
    /// Filesystem path to an image file.
    Path(String),
    /// Freedesktop icon name.
    Icon(String),
}

/// A user-visible action attached to a notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Action {
    /// Unique action key (e.g. `"default"` or `"reply"`).
    pub key: String,
    /// Human-readable label shown on the button.
    pub label: String,
}

/// A fully resolved notification, owned by Core.
#[derive(Debug, Clone)]
pub struct Notification {
    /// Server-assigned unique ID (never 0).
    pub id: u32,
    /// Application name.
    pub app_name: String,
    /// Application icon name or path.
    pub app_icon: String,
    /// One-line summary text.
    pub summary: String,
    /// Optional multi-line body (may contain markup).
    pub body: String,
    /// Available actions; empty if none.
    pub actions: Vec<Action>,
    /// Urgency level.
    pub urgency: Urgency,
    /// Configured expiry timeout.
    pub expire_timeout: Timeout,
    /// Optional image override.
    pub image: Option<ImageSource>,
    /// Notification is transient (not stored in history).
    pub transient: bool,
    /// Notification is resident (not closed after action invocation).
    pub resident: bool,
    /// Optional notification category string.
    pub category: Option<String>,
    /// Optional desktop entry name.
    pub desktop_entry: Option<String>,
    /// Wall-clock creation time.
    pub created_at: std::time::SystemTime,
    /// Raw D-Bus hints not parsed into typed fields.
    pub raw_hints: HashMap<String, OwnedValue>,
}

impl Notification {
    /// Build a [`Notification`] from a [`NewNotification`], assigning `id` and
    /// recording `created_at`.
    ///
    /// The caller supplies `created_at` (typically `std::time::SystemTime::now()`)
    /// so that `notif-types` remains free of I/O-ish calls.
    pub fn from_new(n: NewNotification, id: u32, created_at: std::time::SystemTime) -> Self {
        Self {
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
            created_at,
            raw_hints: n.raw_hints,
        }
    }
}

/// Pre-ID form produced by the D-Bus layer before Core assigns an ID.
#[derive(Debug, Clone)]
pub struct NewNotification {
    /// Application name.
    pub app_name: String,
    /// Application icon name or path.
    pub app_icon: String,
    /// One-line summary text.
    pub summary: String,
    /// Optional multi-line body.
    pub body: String,
    /// Available actions.
    pub actions: Vec<Action>,
    /// Urgency level.
    pub urgency: Urgency,
    /// Configured expiry timeout.
    pub expire_timeout: Timeout,
    /// Optional image override.
    pub image: Option<ImageSource>,
    /// Notification is transient.
    pub transient: bool,
    /// Notification is resident.
    pub resident: bool,
    /// Optional category.
    pub category: Option<String>,
    /// Optional desktop entry.
    pub desktop_entry: Option<String>,
    /// Raw D-Bus hints not parsed into typed fields.
    pub raw_hints: HashMap<String, OwnedValue>,
}

/// Reason a notification was closed (freedesktop spec, §3.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum CloseReason {
    /// The notification expired.
    Expired = 1,
    /// The notification was dismissed by the user.
    Dismissed = 2,
    /// The notification was closed via `CloseNotification`.
    CloseCall = 3,
    /// Undefined / catch-all.
    Undefined = 4,
}

impl From<CloseReason> for u32 {
    fn from(r: CloseReason) -> u32 {
        r as u32
    }
}

/// A notification as projected for the UI layer.
#[derive(Debug, Clone)]
pub struct DisplayNotification {
    /// The underlying notification data.
    pub notification: Arc<Notification>,
    /// Whether the pointer is currently hovering over this notification.
    pub hovered: bool,
}

impl DisplayNotification {
    /// Create a new `DisplayNotification` with hover cleared.
    pub fn new(notification: Notification) -> Self {
        Self {
            notification: Arc::new(notification),
            hovered: false,
        }
    }

    /// Create a `DisplayNotification` from an already-Arc-wrapped notification,
    /// with hover cleared.  Zero additional allocation when the Arc is simply
    /// cloned from the history ring.
    pub fn from_arc(notification: Arc<Notification>) -> Self {
        Self {
            notification,
            hovered: false,
        }
    }
}

/// Serializable summary of a notification for IPC / history consumers.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct HistoryEntry {
    /// The notification's server-assigned ID.
    pub id: u32,
    /// Application name.
    pub app_name: String,
    /// One-line summary text.
    pub summary: String,
    /// Optional multi-line body.
    pub body: String,
    /// Urgency level.
    pub urgency: Urgency,
    /// Creation time as seconds since the Unix epoch.
    pub created_at_unix: u64,
}

impl From<&Notification> for HistoryEntry {
    fn from(n: &Notification) -> Self {
        // duration_since returns Err if created_at is before UNIX_EPOCH; fall
        // back to 0 rather than panicking.
        let created_at_unix = n
            .created_at
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        Self {
            id: n.id,
            app_name: n.app_name.clone(),
            summary: n.summary.clone(),
            body: n.body.clone(),
            urgency: n.urgency,
            created_at_unix,
        }
    }
}

/// Snapshot of daemon state for the `status` IPC command.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StatusInfo {
    /// Whether Do-Not-Disturb mode is currently active.
    pub dnd: bool,
    /// Number of notifications currently displayed.
    pub active: usize,
    /// Number of notifications waiting in the overflow queue.
    pub waiting: usize,
    /// Number of entries in the history ring.
    pub history: usize,
    /// Whether the notification center panel is currently visible.
    pub center_visible: bool,
}

/// Commands sent from the D-Bus layer to Core.
#[derive(Debug)]
pub enum DbusCmd {
    /// A new (or replacing) notification arrived.
    Notify {
        /// The parsed notification data.
        n: Box<NewNotification>,
        /// If non-zero, the ID of an existing notification to replace.
        replaces_id: u32,
        /// Channel to send the assigned ID back on.
        reply: ReplyTx<u32>,
    },
    /// Request to close a notification by ID.
    Close {
        /// The notification ID to close.
        id: u32,
        /// Channel to signal completion on.
        reply: ReplyTx<()>,
    },
}

/// Signals emitted by Core back to the D-Bus layer.
#[derive(Debug, Clone)]
pub enum DbusSignal {
    /// A notification was closed.
    NotificationClosed {
        /// The closed notification's ID.
        id: u32,
        /// The reason it was closed.
        reason: CloseReason,
    },
    /// The user invoked an action.
    ActionInvoked {
        /// The notification ID.
        id: u32,
        /// The action key string.
        action_key: String,
    },
    /// An XDG activation token was produced (for focus-on-action).
    ActivationToken {
        /// The notification ID.
        id: u32,
        /// The activation token string.
        token: String,
    },
}

/// Commands sent from Core to the UI layer.
#[derive(Debug, Clone)]
pub enum UiCommand {
    /// Push a full snapshot of the currently visible notifications.
    Sync(Arc<[DisplayNotification]>),
    /// The active config changed — re-layout and re-render.
    ConfigChanged(Arc<Config>),
    /// Shut down the UI cleanly.
    Shutdown,
    /// Show or hide the notification center panel with the given history entries.
    ///
    /// Pushed on every `ToggleCenter` and after any history mutation while the
    /// center is visible.
    SetCenter {
        /// Whether the center panel is now visible.
        visible: bool,
        /// History entries ordered newest-first, as stripped `DisplayNotification`s.
        entries: Arc<[DisplayNotification]>,
    },
}

/// Events sent from the UI layer back to Core.
#[derive(Debug, Clone)]
pub enum UiEvent {
    /// The user dismissed a notification.
    DismissRequested(u32),
    /// The user clicked the notification body (not the close button).
    /// Core decides whether this invokes the "default" action or dismisses.
    BodyClicked(u32),
    /// The user invoked an action on a notification.
    ActionInvoked {
        /// The notification ID.
        id: u32,
        /// The action key string.
        key: String,
    },
    /// The hover state of a notification changed.
    HoverChanged {
        /// The notification ID.
        id: u32,
        /// Whether the pointer is now hovering.
        hovered: bool,
    },
    /// The set of connected Wayland outputs changed.
    OutputsChanged,
    /// The user clicked the '×' button on a history entry in the center panel.
    HistoryRemoveRequested(u32),
    /// The user clicked 'clear all' in the notification center panel.
    ClearHistoryRequested,
}

/// A config-change event carrying the newly loaded config.
#[derive(Debug, Clone)]
pub struct ConfigEvent(pub Arc<Config>);

/// IPC commands sent from the control socket to Core (Phase 2).
///
/// Variants that carry a `reply` sender use a bounded(1) channel so Core can
/// reply without blocking.  Fire-and-forget variants have no reply.
#[derive(Debug)]
pub enum IpcCmd {
    /// Dismiss all currently active notifications.
    DismissAll,
    /// Close a specific notification by ID (reason: Dismissed).
    Close {
        /// The notification ID to close.
        id: u32,
    },
    /// Retrieve the notification history, newest first.
    History {
        /// Reply channel — Core sends the history entries here.
        reply: ReplyTx<Vec<HistoryEntry>>,
    },
    /// Clear the entire notification history.
    ClearHistory,
    /// Toggle Do-Not-Disturb mode; replies with the **new** DND state.
    ToggleDnd {
        /// Reply channel — Core sends the new DND state here.
        reply: ReplyTx<bool>,
    },
    /// Toggle the notification center panel; replies with the **new** visibility.
    ToggleCenter {
        /// Reply channel — Core sends the new visibility here.
        reply: ReplyTx<bool>,
    },
    /// Query the current daemon status.
    Status {
        /// Reply channel — Core sends a [`StatusInfo`] snapshot here.
        reply: ReplyTx<StatusInfo>,
    },
}
