#![forbid(unsafe_code)]
//! `notifctl` — command-line client for the notif daemon IPC socket.
//!
//! Connects to `$XDG_RUNTIME_DIR/notif.sock`, sends one JSON request line,
//! reads one JSON response line, and prints it human-readably (or raw JSON
//! with `--json`).

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use notif_ipc::protocol::{
    DndResponse, HistoryResponse, OkResponse, Request, StatusResponse, VisibleResponse,
};
use notif_types::Urgency;

// ── Subcommands ───────────────────────────────────────────────────────────────

#[derive(Debug, PartialEq)]
enum Cmd {
    DismissAll,
    Close { id: u32 },
    History { json: bool },
    ClearHistory,
    Dnd,
    Center,
    Status { json: bool },
}

// ── Usage ─────────────────────────────────────────────────────────────────────

fn print_usage() {
    eprintln!(
        "Usage: notifctl <subcommand> [options]\n\
         \n\
         Subcommands:\n\
           dismiss-all         Dismiss all active notifications\n\
           close <id>          Close notification by ID\n\
           history [--json]    Show notification history\n\
           clear-history       Clear notification history\n\
           dnd                 Toggle do-not-disturb mode\n\
           center              Toggle notification center panel\n\
           status [--json]     Show daemon status\n\
         \n\
         Options:\n\
           -h, --help          Show this help\n\
           -V, --version       Show version"
    );
}

// ── Arg parsing (pure — no I/O, no process::exit) ─────────────────────────────

/// Parse a subcommand from the given argument slice.
///
/// This function is pure (no I/O, no `process::exit`) and is called by both
/// `parse_args()` and the unit tests.  It does **not** handle `--help` or
/// `--version`; those are intercepted by `parse_args()` before reaching here.
fn parse_cmd(args: &[&str]) -> std::result::Result<Cmd, String> {
    let subcmd = args
        .first()
        .copied()
        .ok_or_else(|| "no subcommand given".to_owned())?;

    match subcmd {
        "dismiss-all" => {
            if args.len() > 1 {
                let extra = args.get(1).copied().unwrap_or("");
                return Err(format!("unexpected argument: {extra}"));
            }
            Ok(Cmd::DismissAll)
        }
        "close" => {
            let id_str = args
                .get(1)
                .copied()
                .ok_or_else(|| "close requires <id>".to_owned())?;
            if args.len() > 2 {
                let extra = args.get(2).copied().unwrap_or("");
                return Err(format!("unexpected argument: {extra}"));
            }
            let id: u32 = id_str
                .parse()
                .map_err(|_| format!("invalid id {id_str:?}: expected a positive integer"))?;
            Ok(Cmd::Close { id })
        }
        "history" => {
            let mut json = false;
            for &arg in args.iter().skip(1) {
                match arg {
                    "--json" => json = true,
                    other => return Err(format!("unexpected argument: {other}")),
                }
            }
            Ok(Cmd::History { json })
        }
        "clear-history" => {
            if args.len() > 1 {
                let extra = args.get(1).copied().unwrap_or("");
                return Err(format!("unexpected argument: {extra}"));
            }
            Ok(Cmd::ClearHistory)
        }
        "dnd" => {
            if args.len() > 1 {
                let extra = args.get(1).copied().unwrap_or("");
                return Err(format!("unexpected argument: {extra}"));
            }
            Ok(Cmd::Dnd)
        }
        "center" => {
            if args.len() > 1 {
                let extra = args.get(1).copied().unwrap_or("");
                return Err(format!("unexpected argument: {extra}"));
            }
            Ok(Cmd::Center)
        }
        "status" => {
            let mut json = false;
            for &arg in args.iter().skip(1) {
                match arg {
                    "--json" => json = true,
                    other => return Err(format!("unexpected argument: {other}")),
                }
            }
            Ok(Cmd::Status { json })
        }
        other => Err(format!("unknown subcommand: {other}")),
    }
}

/// Read command-line arguments, handle `--help`/`--version`, or exit 2 on error.
fn parse_args() -> Cmd {
    let args_owned: Vec<String> = std::env::args().skip(1).collect();

    // Handle --help and --version before anything else.
    for arg in &args_owned {
        match arg.as_str() {
            "--help" | "-h" => {
                print_usage();
                std::process::exit(0);
            }
            "--version" | "-V" => {
                println!("notifctl {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            _ => {}
        }
    }

    let refs: Vec<&str> = args_owned.iter().map(String::as_str).collect();
    parse_cmd(&refs).unwrap_or_else(|e| {
        eprintln!("error: {e}");
        eprintln!();
        print_usage();
        std::process::exit(2);
    })
}

// ── IPC connection ────────────────────────────────────────────────────────────

fn connect() -> Result<UnixStream> {
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
        .ok_or_else(|| anyhow::anyhow!("$XDG_RUNTIME_DIR is not set; is notifd running?"))?;
    let path = std::path::PathBuf::from(runtime_dir).join("notif.sock");
    UnixStream::connect(&path)
        .with_context(|| format!("cannot connect to {path:?}; is notifd running?"))
}

/// Send one JSON request line and return the trimmed response line.
fn send_recv(stream: &UnixStream, request: &str) -> Result<String> {
    // `impl Write for &UnixStream` requires a mutable binding so the auto-deref
    // can produce `&mut &UnixStream`.  A separate local alias keeps the original
    // `stream` binding available for the `BufReader` below.
    let mut writer = stream;
    writer
        .write_all(format!("{request}\n").as_bytes())
        .context("write request to IPC socket")?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .context("read response from IPC socket")?;
    if line.is_empty() {
        bail!("daemon closed connection without sending a response");
    }
    Ok(line.trim().to_owned())
}

// ── Response helpers ──────────────────────────────────────────────────────────

/// Deserialize a success response of type `T`, or extract and return the daemon
/// error string when `"ok":false`.
///
/// The function first checks the `"ok"` field.  When `ok` is `false` it
/// returns the `"error"` string from the daemon.  When `ok` is `true` it
/// deserializes into `T`.  This avoids needing optional extra fields on the
/// typed protocol structs.
fn parse_response<T: serde::de::DeserializeOwned>(resp: &str) -> Result<T> {
    // Parse the bare ok/error envelope first.
    let envelope: serde_json::Value =
        serde_json::from_str(resp).context("invalid response JSON")?;
    if envelope.get("ok").and_then(|v| v.as_bool()) == Some(false) {
        let msg = envelope
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("daemon returned ok:false");
        bail!("{msg}");
    }
    serde_json::from_str::<T>(resp).context("invalid response JSON")
}

// ── Human-readable formatting ─────────────────────────────────────────────────

fn urgency_str(u: Urgency) -> &'static str {
    match u {
        Urgency::Low => "low",
        Urgency::Normal => "normal",
        Urgency::Critical => "critical",
    }
}

// ── Request builder ───────────────────────────────────────────────────────────

/// Serialize a [`Request`] to a JSON string (single line, no trailing newline).
fn build_request(req: &Request) -> Result<String> {
    serde_json::to_string(req).context("serialize IPC request")
}

// ── Main logic ────────────────────────────────────────────────────────────────

fn run() -> Result<()> {
    let cmd = parse_args();
    let stream = connect()?;

    match cmd {
        Cmd::DismissAll => {
            let req = build_request(&Request::DismissAll)?;
            let resp = send_recv(&stream, &req)?;
            parse_response::<OkResponse>(&resp)?;
            println!("ok");
        }

        Cmd::Close { id } => {
            let req = build_request(&Request::Close { id })?;
            let resp = send_recv(&stream, &req)?;
            parse_response::<OkResponse>(&resp)?;
            println!("ok");
        }

        Cmd::History { json } => {
            let req = build_request(&Request::History)?;
            let resp = send_recv(&stream, &req)?;
            if json {
                println!("{resp}");
                return Ok(());
            }
            let r = parse_response::<HistoryResponse>(&resp)?;
            if r.history.is_empty() {
                println!("history is empty");
            } else {
                let now_secs = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                for entry in &r.history {
                    let age_secs = now_secs.saturating_sub(entry.created_at_unix);
                    println!(
                        "[{}] {}: {} ({}, {})",
                        entry.id,
                        entry.app_name,
                        entry.summary,
                        urgency_str(entry.urgency),
                        notif_types::relative_age(age_secs),
                    );
                }
            }
        }

        Cmd::ClearHistory => {
            let req = build_request(&Request::ClearHistory)?;
            let resp = send_recv(&stream, &req)?;
            parse_response::<OkResponse>(&resp)?;
            println!("ok");
        }

        Cmd::Dnd => {
            let req = build_request(&Request::ToggleDnd)?;
            let resp = send_recv(&stream, &req)?;
            let r = parse_response::<DndResponse>(&resp)?;
            let state = if r.dnd { "on" } else { "off" };
            println!("do-not-disturb: {state}");
        }

        Cmd::Center => {
            let req = build_request(&Request::ToggleCenter)?;
            let resp = send_recv(&stream, &req)?;
            let r = parse_response::<VisibleResponse>(&resp)?;
            let state = if r.visible { "shown" } else { "hidden" };
            println!("center: {state}");
        }

        Cmd::Status { json } => {
            let req = build_request(&Request::Status)?;
            let resp = send_recv(&stream, &req)?;
            if json {
                println!("{resp}");
                return Ok(());
            }
            let r = parse_response::<StatusResponse>(&resp)?;
            let s = r.status;
            println!("dnd:     {}", if s.dnd { "on" } else { "off" });
            println!("active:  {}", s.active);
            println!("waiting: {}", s.waiting);
            println!("history: {}", s.history);
            println!(
                "center:  {}",
                if s.center_visible { "shown" } else { "hidden" }
            );
        }
    }

    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use notif_ipc::protocol::ErrResponse;

    // ── relative_age (delegates to notif_types::relative_age) ────────────────

    #[test]
    fn age_seconds() {
        assert_eq!(notif_types::relative_age(0), "0s");
        assert_eq!(notif_types::relative_age(1), "1s");
        assert_eq!(notif_types::relative_age(59), "59s");
    }

    #[test]
    fn age_minutes() {
        assert_eq!(notif_types::relative_age(60), "1m");
        assert_eq!(notif_types::relative_age(300), "5m");
        assert_eq!(notif_types::relative_age(3599), "59m");
    }

    #[test]
    fn age_hours() {
        assert_eq!(notif_types::relative_age(3600), "1h");
        assert_eq!(notif_types::relative_age(7200), "2h");
        assert_eq!(notif_types::relative_age(86399), "23h");
    }

    #[test]
    fn age_days() {
        assert_eq!(notif_types::relative_age(86400), "1d");
        assert_eq!(notif_types::relative_age(259200), "3d");
    }

    // ── parse_cmd ─────────────────────────────────────────────────────────────

    #[test]
    fn parse_empty_args() {
        let err = parse_cmd(&[]).unwrap_err();
        assert!(err.contains("no subcommand"), "got: {err}");
    }

    #[test]
    fn parse_unknown_subcommand() {
        let err = parse_cmd(&["bogus"]).unwrap_err();
        assert!(err.contains("unknown subcommand"), "got: {err}");
    }

    #[test]
    fn parse_dismiss_all() {
        assert_eq!(parse_cmd(&["dismiss-all"]), Ok(Cmd::DismissAll));
    }

    #[test]
    fn parse_dismiss_all_extra_arg_is_error() {
        assert!(parse_cmd(&["dismiss-all", "extra"]).is_err());
    }

    #[test]
    fn parse_close_valid() {
        assert_eq!(parse_cmd(&["close", "42"]), Ok(Cmd::Close { id: 42 }));
    }

    #[test]
    fn parse_close_max_u32() {
        assert_eq!(
            parse_cmd(&["close", "4294967295"]),
            Ok(Cmd::Close { id: u32::MAX })
        );
    }

    #[test]
    fn parse_close_missing_id() {
        let err = parse_cmd(&["close"]).unwrap_err();
        assert!(err.contains("requires <id>"), "got: {err}");
    }

    #[test]
    fn parse_close_invalid_id_alpha() {
        let err = parse_cmd(&["close", "abc"]).unwrap_err();
        assert!(err.contains("invalid id"), "got: {err}");
    }

    #[test]
    fn parse_close_negative_id() {
        // Negative integers cannot parse as u32.
        assert!(parse_cmd(&["close", "-1"]).is_err());
    }

    #[test]
    fn parse_close_extra_arg_is_error() {
        assert!(parse_cmd(&["close", "5", "extra"]).is_err());
    }

    #[test]
    fn parse_history_no_flags() {
        assert_eq!(parse_cmd(&["history"]), Ok(Cmd::History { json: false }));
    }

    #[test]
    fn parse_history_json_flag() {
        assert_eq!(
            parse_cmd(&["history", "--json"]),
            Ok(Cmd::History { json: true })
        );
    }

    #[test]
    fn parse_history_unknown_flag() {
        assert!(parse_cmd(&["history", "--xml"]).is_err());
    }

    #[test]
    fn parse_clear_history() {
        assert_eq!(parse_cmd(&["clear-history"]), Ok(Cmd::ClearHistory));
    }

    #[test]
    fn parse_dnd() {
        assert_eq!(parse_cmd(&["dnd"]), Ok(Cmd::Dnd));
    }

    #[test]
    fn parse_dnd_extra_arg_is_error() {
        assert!(parse_cmd(&["dnd", "--on"]).is_err());
    }

    #[test]
    fn parse_center() {
        assert_eq!(parse_cmd(&["center"]), Ok(Cmd::Center));
    }

    #[test]
    fn parse_status_no_flags() {
        assert_eq!(parse_cmd(&["status"]), Ok(Cmd::Status { json: false }));
    }

    #[test]
    fn parse_status_json_flag() {
        assert_eq!(
            parse_cmd(&["status", "--json"]),
            Ok(Cmd::Status { json: true })
        );
    }

    // ── Request serialization via shared Request enum ─────────────────────────

    #[test]
    fn request_dismiss_all() {
        let s = build_request(&Request::DismissAll).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["cmd"], "dismiss-all");
    }

    #[test]
    fn request_close() {
        let s = build_request(&Request::Close { id: 5 }).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["cmd"], "close");
        assert_eq!(v["id"], 5);
    }

    #[test]
    fn request_history() {
        let s = build_request(&Request::History).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["cmd"], "history");
    }

    #[test]
    fn request_clear_history() {
        let s = build_request(&Request::ClearHistory).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["cmd"], "clear-history");
    }

    #[test]
    fn request_toggle_dnd() {
        let s = build_request(&Request::ToggleDnd).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["cmd"], "toggle-dnd");
    }

    #[test]
    fn request_toggle_center() {
        let s = build_request(&Request::ToggleCenter).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["cmd"], "toggle-center");
    }

    #[test]
    fn request_status() {
        let s = build_request(&Request::Status).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["cmd"], "status");
    }

    // ── Response parsing via shared protocol types ────────────────────────────

    #[test]
    fn resp_ok_true() {
        let r = parse_response::<OkResponse>(r#"{"ok":true}"#).unwrap();
        assert!(r.ok);
    }

    #[test]
    fn resp_ok_false_with_error() {
        let r: ErrResponse =
            serde_json::from_str(r#"{"ok":false,"error":"something went wrong"}"#).unwrap();
        assert!(!r.ok);
        assert_eq!(r.error, "something went wrong");
    }

    #[test]
    fn resp_error_bubbled_by_parse_response() {
        let err = parse_response::<OkResponse>(r#"{"ok":false,"error":"kaboom"}"#).unwrap_err();
        assert!(err.to_string().contains("kaboom"));
    }

    #[test]
    fn resp_dnd_on() {
        let r = parse_response::<DndResponse>(r#"{"ok":true,"dnd":true}"#).unwrap();
        assert!(r.ok);
        assert!(r.dnd);
    }

    #[test]
    fn resp_dnd_off() {
        let r = parse_response::<DndResponse>(r#"{"ok":true,"dnd":false}"#).unwrap();
        assert!(r.ok);
        assert!(!r.dnd);
    }

    #[test]
    fn resp_center_visible() {
        let r = parse_response::<VisibleResponse>(r#"{"ok":true,"visible":true}"#).unwrap();
        assert!(r.ok);
        assert!(r.visible);
    }

    #[test]
    fn resp_center_hidden() {
        let r = parse_response::<VisibleResponse>(r#"{"ok":true,"visible":false}"#).unwrap();
        assert!(!r.visible);
    }

    #[test]
    fn resp_history_one_entry() {
        let json = r#"{"ok":true,"history":[{
            "id":1,"app_name":"test","summary":"hello","body":"world",
            "urgency":"normal","created_at_unix":1000000
        }]}"#;
        let r = parse_response::<HistoryResponse>(json).unwrap();
        assert!(r.ok);
        assert_eq!(r.history.len(), 1);
        assert_eq!(r.history[0].id, 1);
        assert_eq!(r.history[0].summary, "hello");
        assert_eq!(r.history[0].urgency, Urgency::Normal);
        assert_eq!(r.history[0].created_at_unix, 1_000_000);
    }

    #[test]
    fn resp_history_empty() {
        let r = parse_response::<HistoryResponse>(r#"{"ok":true,"history":[]}"#).unwrap();
        assert!(r.ok);
        assert_eq!(r.history.len(), 0);
    }

    #[test]
    fn resp_history_urgency_variants() {
        let mk = |u: &str| {
            format!(
                r#"{{"ok":true,"history":[{{"id":1,"app_name":"a","summary":"s","body":"b","urgency":"{u}","created_at_unix":0}}]}}"#
            )
        };
        let low = parse_response::<HistoryResponse>(&mk("low")).unwrap();
        assert_eq!(low.history[0].urgency, Urgency::Low);

        let critical = parse_response::<HistoryResponse>(&mk("critical")).unwrap();
        assert_eq!(critical.history[0].urgency, Urgency::Critical);
    }

    #[test]
    fn resp_status_full() {
        let json = r#"{"ok":true,"status":{
            "dnd":true,"active":2,"waiting":1,"history":5,"center_visible":false
        }}"#;
        let r = parse_response::<StatusResponse>(json).unwrap();
        assert!(r.ok);
        let s = r.status;
        assert!(s.dnd);
        assert_eq!(s.active, 2);
        assert_eq!(s.waiting, 1);
        assert_eq!(s.history, 5);
        assert!(!s.center_visible);
    }
}
