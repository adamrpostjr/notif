#!/usr/bin/env bash
# notifctl IPC smoke test.
#
# Starts notifd on an ISOLATED D-Bus session (dbus-run-session) so it never
# touches the user's live notification daemon.  The IPC socket is on the REAL
# $XDG_RUNTIME_DIR because Wayland requires the real runtime dir; the socket
# is cleaned up by notifd on exit.
#
# Usage: bash bin/notifctl/tests/manual/ctl_smoke.sh
set -u
cd "$(dirname "$0")/../../../.."

echo "==> Building notifd and notifctl..."
cargo build -p notifd -p notifctl 2>&1 | tail -5 || exit 1
echo "Build OK"
echo ""

echo "==> Running IPC smoke tests (isolated D-Bus, real XDG_RUNTIME_DIR)..."

inner_result=$(dbus-run-session -- bash -c '
set -u
NOTIFCTL=./target/debug/notifctl
NOTIFD=./target/debug/notifd
PASS=0
FAIL=0

pass() { echo "PASS: $1"; PASS=$((PASS+1)); }
fail_test() { echo "FAIL: $1"; FAIL=$((FAIL+1)); }

# ── Start notifd ─────────────────────────────────────────────────────────────
"$NOTIFD" >/tmp/notifd-ctl-smoke.log 2>&1 &
NOTIFD_PID=$!
sleep 0.8   # let it bind D-Bus name and IPC socket

if ! kill -0 $NOTIFD_PID 2>/dev/null; then
    echo "FATAL: notifd did not start; log:"
    cat /tmp/notifd-ctl-smoke.log
    exit 1
fi
echo "notifd started (pid $NOTIFD_PID)"

# ── Send two notifications ────────────────────────────────────────────────────
# One persistent (no timeout), one short-lived (-t 500 ms → expires to history)
notify-send -a "smokeapp" "Persistent toast" "body1" 2>/dev/null || true
notify-send -a "smokeapp" -t 500 "Ephemeral toast" "body2" 2>/dev/null || true
sleep 1.2   # wait for the 500 ms one to expire and be moved to history

# ── status: active=1, history=1 ──────────────────────────────────────────────
STATUS=$("$NOTIFCTL" status)
echo "--- notifctl status ---"
echo "$STATUS"
echo "-----------------------"
if echo "$STATUS" | grep -q "active:  1" && echo "$STATUS" | grep -q "history: 1"; then
    pass "status active=1 history=1"
else
    fail_test "status unexpected output (want active=1 history=1)"
fi

# ── status --json: valid JSON ─────────────────────────────────────────────────
STATUS_JSON=$("$NOTIFCTL" status --json)
if echo "$STATUS_JSON" | python3 -m json.tool >/dev/null 2>&1; then
    pass "status --json is valid JSON"
else
    fail_test "status --json is not valid JSON: $STATUS_JSON"
fi

# ── history: expired notification appears ────────────────────────────────────
HIST=$("$NOTIFCTL" history)
echo "--- notifctl history ---"
echo "$HIST"
echo "------------------------"
if echo "$HIST" | grep -q "Ephemeral toast"; then
    pass "history contains expired notification"
else
    fail_test "history missing expected notification"
fi

# ── history --json: valid JSON ───────────────────────────────────────────────
HIST_JSON=$("$NOTIFCTL" history --json)
if echo "$HIST_JSON" | python3 -m json.tool >/dev/null 2>&1; then
    pass "history --json is valid JSON"
else
    fail_test "history --json is not valid JSON: $HIST_JSON"
fi

# ── dismiss-all → active=0 ───────────────────────────────────────────────────
DM_OUT=$("$NOTIFCTL" dismiss-all)
if [ "$DM_OUT" = "ok" ]; then
    pass "dismiss-all returned \"ok\""
else
    fail_test "dismiss-all returned unexpected: $DM_OUT"
fi
sleep 0.2
STATUS2=$("$NOTIFCTL" status)
if echo "$STATUS2" | grep -q "active:  0"; then
    pass "dismiss-all cleared active notifications"
else
    fail_test "dismiss-all did not clear active: $STATUS2"
fi

# ── dnd toggle: off → on ─────────────────────────────────────────────────────
DND1=$("$NOTIFCTL" dnd)
if echo "$DND1" | grep -q "do-not-disturb: on"; then
    pass "dnd first toggle → on"
else
    fail_test "dnd first toggle unexpected: $DND1"
fi

# ── dnd toggle: on → off ─────────────────────────────────────────────────────
DND2=$("$NOTIFCTL" dnd)
if echo "$DND2" | grep -q "do-not-disturb: off"; then
    pass "dnd second toggle → off"
else
    fail_test "dnd second toggle unexpected: $DND2"
fi

# ── close with unknown id → ok (silent per spec) ─────────────────────────────
CLOSE_OUT=$("$NOTIFCTL" close 99999)
if [ "$CLOSE_OUT" = "ok" ]; then
    pass "close 99999 → ok (unknown id is silent)"
else
    fail_test "close 99999 unexpected: $CLOSE_OUT"
fi

# ── stop notifd ──────────────────────────────────────────────────────────────
kill $NOTIFD_PID
wait $NOTIFD_PID 2>/dev/null || true
echo "notifd stopped"
sleep 0.2

# ── bogus subcommand → exit 2 ────────────────────────────────────────────────
"$NOTIFCTL" bogus >/dev/null 2>&1; CODE=$?
if [ "$CODE" -eq 2 ]; then
    pass "bogus subcommand → exit 2"
else
    fail_test "bogus subcommand exited $CODE (expected 2)"
fi

# ── connect failure (daemon stopped) → exit 1 ────────────────────────────────
"$NOTIFCTL" status >/dev/null 2>&1; CODE2=$?
if [ "$CODE2" -eq 1 ]; then
    pass "daemon stopped → exit 1"
else
    fail_test "daemon stopped exited $CODE2 (expected 1)"
fi

echo ""
echo "Results: $PASS passed, $FAIL failed"
if [ "$FAIL" -eq 0 ]; then
    echo "ALL TESTS PASSED"
else
    echo "SOME TESTS FAILED"
    exit 1
fi
')

echo "$inner_result"

if echo "$inner_result" | grep -q "ALL TESTS PASSED"; then
    echo ""
    echo "OVERALL: PASS"
    exit 0
else
    echo ""
    echo "OVERALL: FAIL"
    exit 1
fi
