#![forbid(unsafe_code)]

//! `notif-ipc` — Unix-domain socket server for the notif control interface.
//!
//! # Protocol
//! One JSON object per line, request → response:
//!
//! | Request                          | Response                              |
//! |----------------------------------|---------------------------------------|
//! | `{"cmd":"dismiss-all"}`          | `{"ok":true}`                         |
//! | `{"cmd":"close","id":5}`         | `{"ok":true}`                         |
//! | `{"cmd":"history"}`              | `{"ok":true,"history":[…]}`           |
//! | `{"cmd":"clear-history"}`        | `{"ok":true}`                         |
//! | `{"cmd":"toggle-dnd"}`           | `{"ok":true,"dnd":<new>}`             |
//! | `{"cmd":"toggle-center"}`        | `{"ok":true,"visible":<new>}`         |
//! | `{"cmd":"status"}`               | `{"ok":true,"status":{…}}`            |
//! | unknown / malformed              | `{"ok":false,"error":"…"}`            |
//!
//! The connection stays open after a response; the client may send multiple
//! requests before closing.  Malformed requests do **not** close the
//! connection.
//!
//! # Socket path
//! `$XDG_RUNTIME_DIR/notif.sock`.  Any stale socket at that path is removed
//! before binding.  The socket is removed again on clean exit.

use std::path::Path;

use futures_lite::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use notif_types::{HistoryEntry, IpcCmd, StatusInfo};

// ── Error type ────────────────────────────────────────────────────────────────

/// Errors from the IPC subsystem.
#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    /// `$XDG_RUNTIME_DIR` is not set in the environment.
    #[error("$XDG_RUNTIME_DIR is not set; cannot determine socket path")]
    NoRuntimeDir,
    /// Failed to bind the Unix socket.
    #[error("failed to bind IPC socket: {0}")]
    Bind(#[source] std::io::Error),
    /// The core channel was closed (daemon shutting down).
    #[error("core IPC channel closed")]
    CoreClosed,
}

// ── Public wire protocol types ────────────────────────────────────────────────

/// Public wire-protocol types shared between the IPC server and `notifctl`.
///
/// Every type here implements both [`serde::Serialize`] and
/// [`serde::Deserialize`] so that both sender and receiver can use the same
/// definitions.  The `serde` attributes are the canonical source of truth for
/// the JSON wire format — do **not** change them without a corresponding
/// protocol-version bump.
pub mod protocol {
    use notif_types::{HistoryEntry, StatusInfo};

    /// Outgoing JSON request (client → server).
    ///
    /// Serialized as a tagged object with `"cmd"` as the tag field, using
    /// kebab-case variant names (e.g. `{"cmd":"dismiss-all"}`).
    #[derive(serde::Serialize, serde::Deserialize)]
    #[serde(tag = "cmd", rename_all = "kebab-case")]
    pub enum Request {
        /// Dismiss all active notifications.
        DismissAll,
        /// Close a specific notification by ID.
        Close {
            /// The notification ID to close.
            id: u32,
        },
        /// Retrieve the notification history, newest first.
        History,
        /// Clear the entire notification history.
        ClearHistory,
        /// Toggle Do-Not-Disturb mode.
        ToggleDnd,
        /// Toggle the notification center panel.
        ToggleCenter,
        /// Query the current daemon status.
        Status,
    }

    /// `{"ok":true}` — fire-and-forget success response.
    #[derive(Debug, serde::Serialize, serde::Deserialize)]
    pub struct OkResponse {
        /// Always `true` for this response type.
        pub ok: bool,
    }

    /// `{"ok":false,"error":"…"}` — error response.
    #[derive(Debug, serde::Serialize, serde::Deserialize)]
    pub struct ErrResponse {
        /// Always `false` for this response type.
        pub ok: bool,
        /// Human-readable error description.
        pub error: String,
    }

    /// `{"ok":true,"history":[…]}` — history query response.
    #[derive(Debug, serde::Serialize, serde::Deserialize)]
    pub struct HistoryResponse {
        /// `true` on success.
        pub ok: bool,
        /// Notification history entries, newest first.
        pub history: Vec<HistoryEntry>,
    }

    /// `{"ok":true,"dnd":<bool>}` — DND toggle response.
    #[derive(Debug, serde::Serialize, serde::Deserialize)]
    pub struct DndResponse {
        /// `true` on success.
        pub ok: bool,
        /// The **new** Do-Not-Disturb state after the toggle.
        pub dnd: bool,
    }

    /// `{"ok":true,"visible":<bool>}` — center toggle response.
    #[derive(Debug, serde::Serialize, serde::Deserialize)]
    pub struct VisibleResponse {
        /// `true` on success.
        pub ok: bool,
        /// The **new** visibility state of the notification center panel.
        pub visible: bool,
    }

    /// `{"ok":true,"status":{…}}` — status query response.
    #[derive(Debug, serde::Serialize, serde::Deserialize)]
    pub struct StatusResponse {
        /// `true` on success.
        pub ok: bool,
        /// Snapshot of the current daemon status.
        pub status: StatusInfo,
    }
}

// Re-export the protocol types under shorter aliases for internal use.
use protocol::{
    DndResponse, ErrResponse, HistoryResponse, OkResponse, Request, StatusResponse, VisibleResponse,
};

// ── Public API ────────────────────────────────────────────────────────────────

/// Run the IPC server at `$XDG_RUNTIME_DIR/notif.sock`.
///
/// Returns only on fatal setup errors ([`IpcError::NoRuntimeDir`],
/// [`IpcError::Bind`]) or when the core channel closes
/// ([`IpcError::CoreClosed`]).
pub async fn run(ipc_tx: async_channel::Sender<IpcCmd>) -> Result<(), IpcError> {
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR").ok_or(IpcError::NoRuntimeDir)?;
    let path = std::path::PathBuf::from(runtime_dir).join("notif.sock");
    run_at(&path, ipc_tx).await
}

/// Run the IPC server at an explicit socket `path`.
///
/// Intended for tests that supply a temporary path without relying on
/// `$XDG_RUNTIME_DIR`.
///
/// Any stale socket file at `path` is removed before binding.  The socket is
/// removed again (best-effort) when this function returns **or is cancelled**:
/// notifd shuts the IPC task down by dropping its executor task, which drops
/// this future mid-await — cleanup therefore lives in a `Drop` guard, not in
/// straight-line code after the accept loop.
pub async fn run_at(path: &Path, ipc_tx: async_channel::Sender<IpcCmd>) -> Result<(), IpcError> {
    /// Unlinks the socket file when dropped (normal return AND task cancellation).
    struct SocketGuard(std::path::PathBuf);
    impl Drop for SocketGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    // Remove stale socket (best-effort — ignore errors).
    let _ = std::fs::remove_file(path);

    // Bind and wrap in the async reactor.
    let listener = std::os::unix::net::UnixListener::bind(path).map_err(IpcError::Bind)?;
    let _guard = SocketGuard(path.to_path_buf());
    let listener = async_io::Async::new(listener).map_err(IpcError::Bind)?;

    log::info!("notif-ipc: listening at {path:?}");

    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let alive = handle_connection(stream, &ipc_tx).await;
                if !alive {
                    log::info!("notif-ipc: core channel closed; exiting");
                    break;
                }
            }
            Err(e) => {
                log::error!("notif-ipc: accept error: {e}");
                if ipc_tx.is_closed() {
                    log::info!("notif-ipc: core channel closed after accept error; exiting");
                    break;
                }
            }
        }
    }

    // Socket file removal happens in SocketGuard::drop.
    Err(IpcError::CoreClosed)
}

// ── Per-connection handler ────────────────────────────────────────────────────

/// Handle one client connection.
///
/// Reads JSON request lines until the client closes the connection, and writes
/// one JSON response line per request.  Returns `false` if the core channel is
/// closed (the accept loop should stop); returns `true` to continue accepting.
async fn handle_connection(
    stream: async_io::Async<std::os::unix::net::UnixStream>,
    ipc_tx: &async_channel::Sender<IpcCmd>,
) -> bool {
    let mut reader = BufReader::new(&stream);
    let mut line = String::new();

    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => {
                // Client closed connection.
                return true;
            }
            Ok(_) => {}
            Err(e) => {
                log::debug!("notif-ipc: read error on connection: {e}");
                return true;
            }
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Parse request.
        let request: Result<Request, _> = serde_json::from_str(trimmed);
        let response_json = match request {
            Err(e) => {
                log::debug!("notif-ipc: malformed request {trimmed:?}: {e}");
                match serde_json::to_string(&ErrResponse {
                    ok: false,
                    error: format!("malformed request: {e}"),
                }) {
                    Ok(s) => s,
                    Err(e2) => {
                        log::error!("notif-ipc: failed to serialize error response: {e2}");
                        continue;
                    }
                }
            }
            Ok(req) => {
                match dispatch(req, ipc_tx).await {
                    DispatchResult::Response(json) => json,
                    DispatchResult::CoreClosed => {
                        // Write an error response so the client gets something,
                        // then signal the accept loop to stop.
                        if let Ok(s) = serde_json::to_string(&ErrResponse {
                            ok: false,
                            error: "daemon shutting down".into(),
                        }) {
                            let _ = write_line(&stream, &s).await;
                        }
                        return false;
                    }
                }
            }
        };

        if write_line(&stream, &response_json).await.is_err() {
            // Client disappeared mid-write.
            return true;
        }
    }
}

/// Write `payload` followed by a newline to `stream`.
async fn write_line(
    stream: &async_io::Async<std::os::unix::net::UnixStream>,
    payload: &str,
) -> std::io::Result<()> {
    let mut buf = String::with_capacity(payload.len() + 1);
    buf.push_str(payload);
    buf.push('\n');
    (&*stream).write_all(buf.as_bytes()).await
}

// ── Dispatch ──────────────────────────────────────────────────────────────────

enum DispatchResult {
    Response(String),
    CoreClosed,
}

async fn dispatch(req: Request, ipc_tx: &async_channel::Sender<IpcCmd>) -> DispatchResult {
    match req {
        Request::DismissAll => {
            if ipc_tx.send(IpcCmd::DismissAll).await.is_err() {
                return DispatchResult::CoreClosed;
            }
            ok_response()
        }

        Request::Close { id } => {
            if ipc_tx.send(IpcCmd::Close { id }).await.is_err() {
                return DispatchResult::CoreClosed;
            }
            ok_response()
        }

        Request::History => {
            let (reply_tx, reply_rx) = async_channel::bounded::<Vec<HistoryEntry>>(1);
            if ipc_tx
                .send(IpcCmd::History { reply: reply_tx })
                .await
                .is_err()
            {
                return DispatchResult::CoreClosed;
            }
            match reply_rx.recv().await {
                Ok(entries) => serialize_response(&HistoryResponse {
                    ok: true,
                    history: entries,
                }),
                Err(_) => DispatchResult::CoreClosed,
            }
        }

        Request::ClearHistory => {
            if ipc_tx.send(IpcCmd::ClearHistory).await.is_err() {
                return DispatchResult::CoreClosed;
            }
            ok_response()
        }

        Request::ToggleDnd => {
            let (reply_tx, reply_rx) = async_channel::bounded::<bool>(1);
            if ipc_tx
                .send(IpcCmd::ToggleDnd { reply: reply_tx })
                .await
                .is_err()
            {
                return DispatchResult::CoreClosed;
            }
            match reply_rx.recv().await {
                Ok(dnd) => serialize_response(&DndResponse { ok: true, dnd }),
                Err(_) => DispatchResult::CoreClosed,
            }
        }

        Request::ToggleCenter => {
            let (reply_tx, reply_rx) = async_channel::bounded::<bool>(1);
            if ipc_tx
                .send(IpcCmd::ToggleCenter { reply: reply_tx })
                .await
                .is_err()
            {
                return DispatchResult::CoreClosed;
            }
            match reply_rx.recv().await {
                Ok(visible) => serialize_response(&VisibleResponse { ok: true, visible }),
                Err(_) => DispatchResult::CoreClosed,
            }
        }

        Request::Status => {
            let (reply_tx, reply_rx) = async_channel::bounded::<StatusInfo>(1);
            if ipc_tx
                .send(IpcCmd::Status { reply: reply_tx })
                .await
                .is_err()
            {
                return DispatchResult::CoreClosed;
            }
            match reply_rx.recv().await {
                Ok(status) => serialize_response(&StatusResponse { ok: true, status }),
                Err(_) => DispatchResult::CoreClosed,
            }
        }
    }
}

fn ok_response() -> DispatchResult {
    serialize_response(&OkResponse { ok: true })
}

fn serialize_response<T: serde::Serialize>(val: &T) -> DispatchResult {
    match serde_json::to_string(val) {
        Ok(s) => DispatchResult::Response(s),
        Err(e) => {
            log::error!("notif-ipc: failed to serialize response: {e}");
            // Fallback error JSON (hand-crafted to avoid another serde call).
            DispatchResult::Response(
                r#"{"ok":false,"error":"internal serialization error"}"#.into(),
            )
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use notif_types::{HistoryEntry, IpcCmd, StatusInfo, Urgency};
    use std::time::Duration;

    /// Fake core task: receives IpcCmd values and sends canned replies.
    async fn fake_core(ipc_rx: async_channel::Receiver<IpcCmd>) {
        while let Ok(cmd) = ipc_rx.recv().await {
            match cmd {
                IpcCmd::DismissAll => {}
                IpcCmd::Close { .. } => {}
                IpcCmd::ClearHistory => {}
                IpcCmd::History { reply } => {
                    let _ = reply
                        .send(vec![HistoryEntry {
                            id: 42,
                            app_name: "testapp".into(),
                            summary: "Test notification".into(),
                            body: "body text".into(),
                            urgency: Urgency::Normal,
                            created_at_unix: 1_000_000,
                        }])
                        .await;
                }
                IpcCmd::ToggleDnd { reply } => {
                    let _ = reply.send(true).await;
                }
                IpcCmd::ToggleCenter { reply } => {
                    let _ = reply.send(false).await;
                }
                IpcCmd::Status { reply } => {
                    let _ = reply
                        .send(StatusInfo {
                            dnd: true,
                            active: 2,
                            waiting: 1,
                            history: 5,
                            center_visible: false,
                        })
                        .await;
                }
            }
        }
    }

    /// Send one request line to the socket and return the trimmed response line.
    async fn do_cmd(path: &Path, request: &str) -> String {
        let stream = async_io::Async::<std::os::unix::net::UnixStream>::connect(path)
            .await
            .expect("connect to IPC socket");

        // Write request.
        (&stream)
            .write_all(format!("{request}\n").as_bytes())
            .await
            .expect("write request");

        // Read response line.
        let mut reader = BufReader::new(&stream);
        let mut line = String::new();
        reader.read_line(&mut line).await.expect("read response");
        line.trim().to_owned()
    }

    #[test]
    fn test_ipc_protocol() {
        async_io::block_on(async {
            let ex = async_executor::LocalExecutor::new();
            let (ipc_tx, ipc_rx) = async_channel::unbounded::<IpcCmd>();
            let tmpdir = tempfile::TempDir::new().expect("tmpdir");
            let socket_path = tmpdir.path().join("notif-test.sock");

            let path_server = socket_path.clone();
            let tx_server = ipc_tx.clone();

            // Run everything inside the executor so spawned tasks are driven.
            ex.run(async {
                // Spawn the fake core handler.
                ex.spawn(fake_core(ipc_rx)).detach();

                // Spawn the IPC server.
                let path_for_server = path_server.clone();
                ex.spawn(async move {
                    let _ = run_at(&path_for_server, tx_server).await;
                })
                .detach();

                // Give the server a moment to bind.
                async_io::Timer::after(Duration::from_millis(50)).await;

                // ── dismiss-all ──────────────────────────────────────────────
                let resp = do_cmd(&path_server, r#"{"cmd":"dismiss-all"}"#).await;
                assert_eq!(resp, r#"{"ok":true}"#, "dismiss-all response");

                // ── close ────────────────────────────────────────────────────
                let resp = do_cmd(&path_server, r#"{"cmd":"close","id":5}"#).await;
                assert_eq!(resp, r#"{"ok":true}"#, "close response");

                // ── history ──────────────────────────────────────────────────
                let resp = do_cmd(&path_server, r#"{"cmd":"history"}"#).await;
                let v: serde_json::Value =
                    serde_json::from_str(&resp).expect("history response JSON");
                assert_eq!(v["ok"], true, "history ok");
                let entries = v["history"].as_array().expect("history array");
                assert_eq!(entries.len(), 1, "history entry count");
                assert_eq!(entries[0]["id"], 42, "history entry id");
                assert_eq!(
                    entries[0]["summary"], "Test notification",
                    "history entry summary"
                );
                assert_eq!(entries[0]["urgency"], "normal", "history entry urgency");

                // ── clear-history ────────────────────────────────────────────
                let resp = do_cmd(&path_server, r#"{"cmd":"clear-history"}"#).await;
                assert_eq!(resp, r#"{"ok":true}"#, "clear-history response");

                // ── toggle-dnd ───────────────────────────────────────────────
                let resp = do_cmd(&path_server, r#"{"cmd":"toggle-dnd"}"#).await;
                let v: serde_json::Value =
                    serde_json::from_str(&resp).expect("toggle-dnd response JSON");
                assert_eq!(v["ok"], true, "toggle-dnd ok");
                assert_eq!(v["dnd"], true, "toggle-dnd new state");

                // ── toggle-center ────────────────────────────────────────────
                let resp = do_cmd(&path_server, r#"{"cmd":"toggle-center"}"#).await;
                let v: serde_json::Value =
                    serde_json::from_str(&resp).expect("toggle-center response JSON");
                assert_eq!(v["ok"], true, "toggle-center ok");
                assert_eq!(v["visible"], false, "toggle-center new state");

                // ── status ───────────────────────────────────────────────────
                let resp = do_cmd(&path_server, r#"{"cmd":"status"}"#).await;
                let v: serde_json::Value =
                    serde_json::from_str(&resp).expect("status response JSON");
                assert_eq!(v["ok"], true, "status ok");
                assert_eq!(v["status"]["dnd"], true, "status dnd");
                assert_eq!(v["status"]["active"], 2, "status active");
                assert_eq!(v["status"]["waiting"], 1, "status waiting");
                assert_eq!(v["status"]["history"], 5, "status history");
                assert_eq!(
                    v["status"]["center_visible"], false,
                    "status center_visible"
                );

                // ── malformed JSON ────────────────────────────────────────────
                let resp = do_cmd(&path_server, r#"not-valid-json"#).await;
                let v: serde_json::Value =
                    serde_json::from_str(&resp).expect("error response JSON");
                assert_eq!(v["ok"], false, "malformed: ok=false");
                assert!(
                    v["error"].as_str().is_some(),
                    "malformed: error field present"
                );

                // ── unknown command ───────────────────────────────────────────
                let resp = do_cmd(&path_server, r#"{"cmd":"frobnicate"}"#).await;
                let v: serde_json::Value =
                    serde_json::from_str(&resp).expect("unknown cmd response JSON");
                assert_eq!(v["ok"], false, "unknown cmd: ok=false");
                assert!(
                    v["error"].as_str().is_some(),
                    "unknown cmd: error field present"
                );

                // ── multi-request on same connection ──────────────────────────
                {
                    let stream =
                        async_io::Async::<std::os::unix::net::UnixStream>::connect(&path_server)
                            .await
                            .expect("connect for multi-request test");

                    (&stream)
                        .write_all(b"{\"cmd\":\"dismiss-all\"}\n{\"cmd\":\"toggle-dnd\"}\n")
                        .await
                        .expect("write two requests");

                    let mut reader = BufReader::new(&stream);

                    let mut line1 = String::new();
                    reader.read_line(&mut line1).await.expect("read line 1");
                    assert_eq!(line1.trim(), r#"{"ok":true}"#, "multi-req line 1");

                    let mut line2 = String::new();
                    reader.read_line(&mut line2).await.expect("read line 2");
                    let v2: serde_json::Value =
                        serde_json::from_str(line2.trim()).expect("multi-req line 2 JSON");
                    assert_eq!(v2["ok"], true, "multi-req line 2 ok");
                    assert_eq!(v2["dnd"], true, "multi-req line 2 dnd");
                }
            })
            .await;
        });
    }
}
