#![forbid(unsafe_code)]

//! `notifd` — the notification daemon entry point.
//!
//! Wires up the D-Bus layer, Core state machine, Wayland UI, config watcher,
//! and IPC listener into a single async executor.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use async_channel::{Receiver, Sender, unbounded};

use notif_core::CoreHandles;
use notif_render::SkiaRenderer;
use notif_types::{ConfigEvent, DbusCmd, DbusSignal, IpcCmd, UiCommand, UiEvent};

// ── Argument parsing ─────────────────────────────────────────────────────────

fn default_config_path() -> PathBuf {
    // $XDG_CONFIG_HOME/notif/config.toml, falling back to ~/.config/notif/config.toml
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default();
            home.join(".config")
        });
    base.join("notif").join("config.toml")
}

struct Args {
    config_path: PathBuf,
}

fn parse_args() -> Result<Option<Args>> {
    let mut args_iter = std::env::args().skip(1);
    let mut config_path = None;

    while let Some(arg) = args_iter.next() {
        match arg.as_str() {
            "--version" | "-V" => {
                println!("notifd {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            "--help" | "-h" => {
                println!("Usage: notifd [--config <path>] [--version]");
                return Ok(None);
            }
            "--config" | "-c" => {
                let path = args_iter
                    .next()
                    .context("--config requires a path argument")?;
                config_path = Some(PathBuf::from(path));
            }
            other => {
                anyhow::bail!("unknown argument: {other}");
            }
        }
    }

    Ok(Some(Args {
        config_path: config_path.unwrap_or_else(default_config_path),
    }))
}

// ── Main ─────────────────────────────────────────────────────────────────────

fn main() -> Result<()> {
    // Parse args first (may print version and exit)
    let args = match parse_args()? {
        Some(a) => a,
        None => return Ok(()),
    };

    // Initialise logging
    env_logger::init();

    // Load configuration (or use defaults if file absent)
    let config = notif_config::load(&args.config_path).context("failed to load configuration")?;
    let config = Arc::new(config);

    log::info!("notifd starting (config: {:?})", args.config_path);

    // Create all channels
    let (dbus_cmd_tx, dbus_cmd_rx): (Sender<DbusCmd>, Receiver<DbusCmd>) = unbounded();
    let (dbus_signal_tx, dbus_signal_rx): (Sender<DbusSignal>, Receiver<DbusSignal>) = unbounded();
    let (ui_cmd_tx, ui_cmd_rx): (Sender<UiCommand>, Receiver<UiCommand>) = unbounded();
    let (ui_event_tx, ui_event_rx): (Sender<UiEvent>, Receiver<UiEvent>) = unbounded();
    let (config_tx, config_rx): (Sender<ConfigEvent>, Receiver<ConfigEvent>) = unbounded();
    let (ipc_tx, ipc_rx): (Sender<IpcCmd>, Receiver<IpcCmd>) = unbounded();

    // Set up Ctrl-C handler — sends Shutdown to UI
    {
        let ui_cmd_tx_ctrlc = ui_cmd_tx.clone();
        ctrlc::set_handler(move || {
            log::info!("received Ctrl-C, shutting down");
            let _ = ui_cmd_tx_ctrlc.try_send(UiCommand::Shutdown);
        })
        .context("failed to set Ctrl-C handler")?;
    }

    // Build core handles
    let core_handles = CoreHandles {
        dbus_cmd_rx,
        dbus_signal_tx,
        ui_cmd_tx: ui_cmd_tx.clone(),
        ui_event_rx,
        config_rx,
        ipc_rx,
    };

    let config_for_core = Arc::clone(&config);
    let config_for_wl = Arc::clone(&config);
    let config_path = args.config_path.clone();

    // Run everything on a single-threaded executor
    futures_lite::future::block_on(async {
        let ex = async_executor::LocalExecutor::new();

        // Spawn core task
        let config_for_core2 = Arc::clone(&config_for_core);
        let _core_task = ex.spawn(async move {
            notif_core::run(config_for_core2, core_handles).await;
        });

        // Spawn D-Bus task
        let dbus_cmd_tx2 = dbus_cmd_tx.clone();
        let _dbus_task = ex.spawn(async move {
            match notif_dbus::run(dbus_cmd_tx2, dbus_signal_rx).await {
                Ok(()) => log::info!("notif-dbus: exited cleanly"),
                Err(e) => log::error!("notif-dbus: error: {e}"),
            }
        });

        // Spawn config watcher task
        let _config_task = ex.spawn(async move {
            notif_config::watch(config_path, config_tx).await;
        });

        // Spawn IPC socket task.  IPC failures are logged-and-degraded: a bind
        // error (e.g. $XDG_RUNTIME_DIR unset) or unexpected socket error must
        // not kill the daemon.
        let ipc_tx_for_task = ipc_tx.clone();
        let _ipc_task = ex.spawn(async move {
            match notif_ipc::run(ipc_tx_for_task).await {
                Ok(()) => log::info!("notif-ipc: exited cleanly"),
                Err(e) => log::error!("notif-ipc: {e}"),
            }
        });

        // Run Wayland UI in the foreground (returns when done)
        let renderer: Box<dyn notif_render::Renderer> = Box::new(SkiaRenderer::new());
        let wl_result = ex
            .run(notif_wl::run(
                config_for_wl,
                ui_cmd_rx,
                ui_event_tx,
                renderer,
            ))
            .await;

        if let Err(e) = wl_result {
            log::error!("notif-wl: error: {e}");
        }
    });

    log::info!("notifd exiting");
    Ok(())
}
