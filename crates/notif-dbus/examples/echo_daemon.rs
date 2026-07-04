//! Echo daemon — answers DbusCmd and pretty-prints parsed notifications.

use notif_types::{CloseReason, DbusCmd, DbusSignal, ImageSource};

fn main() {
    if let Err(e) = async_io::block_on(async_main()) {
        eprintln!("echo_daemon: fatal: {e}");
        std::process::exit(1);
    }
}

async fn async_main() -> Result<(), Box<dyn std::error::Error>> {
    let (cmd_tx, cmd_rx) = async_channel::bounded::<DbusCmd>(32);
    let (signal_tx, signal_rx) = async_channel::bounded::<DbusSignal>(32);

    let core_task = async move {
        let mut next_id = 1u32;
        while let Ok(cmd) = cmd_rx.recv().await {
            match cmd {
                DbusCmd::Notify {
                    n,
                    replaces_id,
                    reply,
                } => {
                    let id = if replaces_id != 0 {
                        replaces_id
                    } else {
                        let id = next_id;
                        next_id = next_id.wrapping_add(1).max(1);
                        id
                    };
                    println!("NOTIFY id={id} replaces={replaces_id}");
                    println!("  app_name: {:?}", n.app_name);
                    println!("  summary: {:?}", n.summary);
                    println!("  body: {:?}", n.body);
                    println!("  urgency: {:?}", n.urgency);
                    println!("  timeout: {:?}", n.expire_timeout);
                    if let Some(img) = &n.image {
                        match img {
                            ImageSource::Data(raw) => {
                                println!("  image: {}x{} pixels (raw)", raw.width, raw.height)
                            }
                            ImageSource::Path(p) => println!("  image: path={p:?}"),
                            ImageSource::Icon(i) => println!("  image: icon={i:?}"),
                        }
                    }
                    println!("  actions: {:?}", n.actions);
                    let _ = reply.send(id).await;
                }
                DbusCmd::Close { id, reply } => {
                    println!("CLOSE id={id}");
                    let _ = reply.send(()).await;
                    let _ = signal_tx
                        .send(DbusSignal::NotificationClosed {
                            id,
                            reason: CloseReason::CloseCall,
                        })
                        .await;
                }
            }
        }
    };

    let dbus_task = notif_dbus::run(cmd_tx, signal_rx);

    let (core_res, dbus_res) = futures_lite::future::zip(
        async move {
            core_task.await;
            Ok::<(), Box<dyn std::error::Error>>(())
        },
        async move {
            dbus_task
                .await
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error>)
        },
    )
    .await;
    core_res?;
    dbus_res?;

    Ok(())
}
