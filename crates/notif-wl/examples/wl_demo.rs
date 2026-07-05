//! `wl_demo` — end-to-end demo for `notif-wl`.
//!
//! Feeds scripted `UiCommand::Sync`s to the Wayland layer shell and prints every
//! `UiEvent` it receives back.  Timeline:
//!
//! - t=0s: two notifications (Normal + Critical); surface appears top-right.
//! - t=3s: add a third notification.
//! - t=6s: remove all; surface disappears.
//! - t=8s: two notifications again; holds until process exits.
//! - t=9s: SetCenter{visible:true} — center panel appears top-right.
//! - t=12s: SetCenter{visible:false} — center panel disappears.
//!
//! Run with:
//! ```sh
//! WAYLAND_DEBUG=1 timeout 15 cargo run -p notif-wl --example wl_demo 2>debug.log
//! ```

use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, SystemTime},
};

use async_channel::{Receiver, Sender};
use async_executor::LocalExecutor;
use futures_lite::future;

use notif_render::StubRenderer;
use notif_types::{
    DisplayNotification, Notification, Timeout, UiCommand, UiEvent, Urgency, config::Config,
};

fn make_notif(id: u32, urgency: Urgency, summary: &str) -> DisplayNotification {
    DisplayNotification::new(Notification {
        id,
        app_name: "wl_demo".into(),
        app_icon: String::new(),
        summary: summary.into(),
        body: format!("Demo notification #{id}"),
        actions: Vec::new(),
        urgency,
        expire_timeout: Timeout::Never,
        image: None,
        transient: false,
        resident: false,
        category: None,
        desktop_entry: None,
        created_at: SystemTime::now(),
        raw_hints: HashMap::new(),
    })
}

fn main() {
    env_logger::builder()
        .filter_level(log::LevelFilter::Info)
        .init();

    let ex = LocalExecutor::new();
    future::block_on(ex.run(async_main()));
}

async fn async_main() {
    let cfg = Arc::new(Config::default());

    let (cmd_tx, cmd_rx): (Sender<UiCommand>, Receiver<UiCommand>) = async_channel::bounded(32);
    let (evt_tx, evt_rx): (Sender<UiEvent>, Receiver<UiEvent>) = async_channel::bounded(32);

    // Spawn the event printer task.
    let printer = async move {
        while let Ok(evt) = evt_rx.recv().await {
            println!("[UiEvent] {evt:?}");
        }
        println!("[wl_demo] UiEvent channel closed");
    };

    // Spawn the scripted command sender task.
    let sender = {
        let cmd_tx = cmd_tx.clone();
        async move {
            // t=0: two notifications.
            let items_a: Arc<[DisplayNotification]> = Arc::from(vec![
                make_notif(1, Urgency::Normal, "Hello from wl_demo"),
                make_notif(2, Urgency::Critical, "Critical alert!"),
            ]);
            println!("[wl_demo] t=0: sending two notifications");
            if cmd_tx.send(UiCommand::Sync(items_a)).await.is_err() {
                return;
            }

            // t=3: add a third.
            async_io::Timer::after(Duration::from_secs(3)).await;
            let items_b: Arc<[DisplayNotification]> = Arc::from(vec![
                make_notif(1, Urgency::Normal, "Hello from wl_demo"),
                make_notif(2, Urgency::Critical, "Critical alert!"),
                make_notif(3, Urgency::Normal, "Third notification"),
            ]);
            println!("[wl_demo] t=3: adding a third notification");
            if cmd_tx.send(UiCommand::Sync(items_b)).await.is_err() {
                return;
            }

            // t=6: remove all — surface must disappear.
            async_io::Timer::after(Duration::from_secs(3)).await;
            let items_empty: Arc<[DisplayNotification]> = Arc::from(vec![]);
            println!("[wl_demo] t=6: clearing all notifications (surface should disappear)");
            if cmd_tx.send(UiCommand::Sync(items_empty)).await.is_err() {
                return;
            }

            // t=8: two again.
            async_io::Timer::after(Duration::from_secs(2)).await;
            let items_c: Arc<[DisplayNotification]> = Arc::from(vec![
                make_notif(4, Urgency::Normal, "Back again"),
                make_notif(5, Urgency::Critical, "Still critical"),
            ]);
            println!("[wl_demo] t=8: showing two notifications again (hold)");
            if cmd_tx.send(UiCommand::Sync(items_c)).await.is_err() {
                return;
            }

            // t=9: show the notification center panel (reuse fake notifications
            // as active + history entries).
            async_io::Timer::after(Duration::from_secs(1)).await;
            let center_active: Arc<[DisplayNotification]> =
                Arc::from(vec![make_notif(4, Urgency::Normal, "Back again")]);
            let center_history: Arc<[DisplayNotification]> = Arc::from(vec![
                make_notif(5, Urgency::Critical, "Still critical"),
                make_notif(6, Urgency::Low, "Low priority item"),
            ]);
            println!("[wl_demo] t=9: showing notification center panel");
            if cmd_tx
                .send(UiCommand::SetCenter {
                    visible: true,
                    active: center_active,
                    history: center_history,
                })
                .await
                .is_err()
            {
                return;
            }

            // t=12: hide the center panel.
            async_io::Timer::after(Duration::from_secs(3)).await;
            println!("[wl_demo] t=12: hiding notification center panel");
            if cmd_tx
                .send(UiCommand::SetCenter {
                    visible: false,
                    active: Arc::from(vec![]),
                    history: Arc::from(vec![]),
                })
                .await
                .is_err()
            {
                return;
            }

            // Hold for a bit so screenshots can be taken.
            async_io::Timer::after(Duration::from_secs(3)).await;
            println!("[wl_demo] shutting down");
            let _ = cmd_tx.send(UiCommand::Shutdown).await;
        }
    };

    // Run all three concurrently.
    let wl = notif_wl::run(cfg, cmd_rx, evt_tx, Box::new(StubRenderer));

    future::or(future::or(printer, sender), async move {
        match wl.await {
            Ok(()) => println!("[wl_demo] notif-wl exited cleanly"),
            Err(e) => eprintln!("[wl_demo] notif-wl error: {e}"),
        }
    })
    .await;
}
