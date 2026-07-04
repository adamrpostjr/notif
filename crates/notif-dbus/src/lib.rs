#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

//! `notif-dbus` — implements `org.freedesktop.Notifications` via zbus.

mod hints;

use std::collections::HashMap;

use async_channel::{Receiver, Sender};
use notif_types::{DbusCmd, DbusSignal, NewNotification};
use zbus::{
    connection, fdo,
    fdo::{RequestNameFlags, RequestNameReply},
    interface,
    object_server::SignalEmitter,
};
use zvariant::OwnedValue;

const CAPABILITIES: &[&str] = &[
    "body",
    "body-markup",
    "actions",
    "icon-static",
    "persistence",
];

/// Errors produced by the D-Bus layer.
#[derive(Debug, thiserror::Error)]
pub enum DbusError {
    /// A zbus-level error (connection, proxy, etc.).
    #[error("D-Bus connection error: {0}")]
    Zbus(#[from] zbus::Error),
    /// The well-known name is already taken by another process.
    #[error("well-known name org.freedesktop.Notifications is already taken: {0}")]
    NameTaken(String),
}

struct NotifInterface {
    cmd_tx: Sender<DbusCmd>,
}

#[interface(name = "org.freedesktop.Notifications")]
impl NotifInterface {
    #[allow(clippy::too_many_arguments)]
    async fn notify(
        &self,
        app_name: String,
        replaces_id: u32,
        app_icon: String,
        summary: String,
        body: String,
        actions: Vec<String>,
        hints: HashMap<String, OwnedValue>,
        expire_timeout: i32,
    ) -> fdo::Result<u32> {
        let n: NewNotification = hints::parse_hints(
            app_name,
            app_icon,
            summary,
            body,
            actions,
            hints,
            expire_timeout,
        );

        let (reply_tx, reply_rx) = async_channel::bounded::<u32>(1);
        let cmd = DbusCmd::Notify {
            n: Box::new(n),
            replaces_id,
            reply: reply_tx,
        };

        self.cmd_tx
            .send(cmd)
            .await
            .map_err(|e| fdo::Error::Failed(format!("core channel closed: {e}")))?;

        let id = reply_rx
            .recv()
            .await
            .map_err(|e| fdo::Error::Failed(format!("core reply channel closed: {e}")))?;

        Ok(id)
    }

    async fn close_notification(&self, id: u32) -> fdo::Result<()> {
        let (reply_tx, reply_rx) = async_channel::bounded::<()>(1);
        let cmd = DbusCmd::Close {
            id,
            reply: reply_tx,
        };

        self.cmd_tx
            .send(cmd)
            .await
            .map_err(|e| fdo::Error::Failed(format!("core channel closed: {e}")))?;

        // Await reply but always succeed (unknown IDs silently ignored by core)
        let _ = reply_rx.recv().await;

        Ok(())
    }

    fn get_capabilities(&self) -> Vec<String> {
        CAPABILITIES.iter().map(|s| s.to_string()).collect()
    }

    async fn get_server_information(&self) -> fdo::Result<(String, String, String, String)> {
        Ok((
            "notif".to_string(),
            "notif".to_string(),
            "0.1.0".to_string(),
            "1.2".to_string(),
        ))
    }

    #[zbus(signal)]
    async fn notification_closed(
        emitter: &SignalEmitter<'_>,
        id: u32,
        reason: u32,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn action_invoked(
        emitter: &SignalEmitter<'_>,
        id: u32,
        action_key: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn activation_token(
        emitter: &SignalEmitter<'_>,
        id: u32,
        activation_token: &str,
    ) -> zbus::Result<()>;
}

/// Run the D-Bus notification server.
///
/// Sends parsed [`DbusCmd`]s to `cmd_tx` and emits signals received via `signal_rx`.
/// Returns when `signal_rx` is closed (i.e. the core task has shut down).
pub async fn run(
    cmd_tx: Sender<DbusCmd>,
    signal_rx: Receiver<DbusSignal>,
) -> Result<(), DbusError> {
    let iface = NotifInterface { cmd_tx };

    let conn = connection::Builder::session()?
        .serve_at("/org/freedesktop/Notifications", iface)?
        .build()
        .await?;

    // Request the well-known name with DoNotQueue flag only.
    // We do not request AllowReplacement or ReplaceExisting.
    let reply = conn
        .request_name_with_flags(
            "org.freedesktop.Notifications",
            RequestNameFlags::DoNotQueue.into(),
        )
        .await;

    match reply {
        Ok(RequestNameReply::PrimaryOwner) => {
            // We are now the primary owner; continue.
        }
        Ok(other) => {
            return Err(DbusError::NameTaken(format!("{other:?}")));
        }
        // zbus maps a DoNotQueue rejection to its own NameTaken error.
        Err(zbus::Error::NameTaken) => {
            return Err(DbusError::NameTaken(
                "another notification daemon owns it".to_string(),
            ));
        }
        Err(e) => return Err(DbusError::Zbus(e)),
    }

    // Signal emission loop.
    let iface_ref = conn
        .object_server()
        .interface::<_, NotifInterface>("/org/freedesktop/Notifications")
        .await?;

    while let Ok(signal) = signal_rx.recv().await {
        match signal {
            DbusSignal::NotificationClosed { id, reason } => {
                let reason_u32: u32 = reason.into();
                if let Err(e) =
                    NotifInterface::notification_closed(iface_ref.signal_emitter(), id, reason_u32)
                        .await
                {
                    log::warn!("notif-dbus: failed to emit NotificationClosed: {e}");
                }
            }
            DbusSignal::ActionInvoked { id, action_key } => {
                if let Err(e) =
                    NotifInterface::action_invoked(iface_ref.signal_emitter(), id, &action_key)
                        .await
                {
                    log::warn!("notif-dbus: failed to emit ActionInvoked: {e}");
                }
            }
            DbusSignal::ActivationToken { id, token } => {
                if let Err(e) =
                    NotifInterface::activation_token(iface_ref.signal_emitter(), id, &token).await
                {
                    log::warn!("notif-dbus: failed to emit ActivationToken: {e}");
                }
            }
        }
    }

    Ok(())
}
