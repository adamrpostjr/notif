//! Headless daemon example — wires notif-dbus + notif-core together and prints
//! every `UiCommand::Sync` to stdout.  Useful for manual smoke testing without
//! a compositor.
//!
//! Run inside a transient D-Bus session:
//!   dbus-run-session -- cargo run --example headless_daemon -p notif-core

use std::sync::Arc;

use futures_lite::future;
use notif_config::Config;
use notif_core::{CoreHandles, run as core_run};
use notif_types::{ConfigEvent, IpcCmd, UiCommand, UiEvent};

fn main() {
    env_logger::init();

    let (dbus_cmd_tx, dbus_cmd_rx) = async_channel::unbounded();
    let (dbus_signal_tx, dbus_signal_rx) = async_channel::unbounded();
    let (ui_cmd_tx, ui_cmd_rx) = async_channel::unbounded();
    let (_ui_event_tx, ui_event_rx) = async_channel::unbounded::<UiEvent>();
    let (_config_tx, config_rx) = async_channel::unbounded::<ConfigEvent>();
    let (_ipc_tx, ipc_rx) = async_channel::unbounded::<IpcCmd>();

    let config = Arc::new(Config::default());
    let handles = CoreHandles {
        dbus_cmd_rx,
        dbus_signal_tx,
        ui_cmd_tx,
        ui_event_rx,
        config_rx,
        ipc_rx,
    };

    async_io::block_on(future::or(
        future::or(
            async move {
                if let Err(e) = notif_dbus::run(dbus_cmd_tx, dbus_signal_rx).await {
                    eprintln!("dbus error: {e}");
                }
            },
            core_run(config, handles),
        ),
        async move {
            while let Ok(cmd) = ui_cmd_rx.recv().await {
                match &cmd {
                    UiCommand::Sync(notifs) => {
                        let parts: Vec<String> = notifs
                            .iter()
                            .map(|n| {
                                format!(
                                    "[{}] {} ({:?})",
                                    n.notification.id,
                                    n.notification.summary,
                                    n.notification.urgency
                                )
                            })
                            .collect();
                        println!("SYNC({} visible): {}", notifs.len(), parts.join(", "));
                    }
                    UiCommand::ConfigChanged(_) => println!("CONFIG_CHANGED"),
                    UiCommand::Shutdown => {
                        println!("SHUTDOWN");
                        break;
                    }
                }
            }
        },
    ));
}
