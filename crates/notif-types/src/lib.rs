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
}

/// A config-change event carrying the newly loaded config.
#[derive(Debug, Clone)]
pub struct ConfigEvent(pub Arc<Config>);

/// IPC commands (Phase 2 placeholder).
#[derive(Debug, Clone)]
pub enum IpcCmd {
    /// Dismiss all active notifications.
    DismissAll,
    /// Retrieve notification history.
    History,
    /// Toggle Do-Not-Disturb mode.
    ToggleDnd,
    /// Toggle the notification center panel.
    ToggleCenter,
}
