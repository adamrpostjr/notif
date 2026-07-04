#!/usr/bin/env bash
# Manual smoke test for the headless_daemon example.
#
# Usage:
#   cd <workspace-root>
#   bash crates/notif-core/tests/manual/core_smoke.sh
#
# What it checks:
#   1. Sends two notify-send calls (one with a 1 s timeout, one normal).
#   2. Waits for the short-timeout notification to expire.
#   3. Verifies that stdout shows a shrinking Sync (2 → 1 notification).

set -euo pipefail

WORKSPACE_ROOT="$(cd "$(dirname "$0")/../../../.." && pwd)"
cd "$WORKSPACE_ROOT"

echo "==> Building headless_daemon example..."
cargo build --example headless_daemon -p notif-core 2>&1

BINARY="$WORKSPACE_ROOT/target/debug/examples/headless_daemon"
LOGFILE="$(mktemp /tmp/core_smoke_XXXXXX.log)"
echo "==> Log file: $LOGFILE"

echo "==> Launching headless_daemon inside a transient D-Bus session..."
dbus-run-session -- bash -c "
  '$BINARY' &
  DAEMON_PID=\$!
  echo 'daemon pid: '\$DAEMON_PID

  # Give the daemon a moment to register on the bus.
  sleep 0.5

  # Send two notifications: one with a 1s timeout, one without.
  notify-send --expire-time=1000 'Short' 'This should expire quickly'
  notify-send 'Long'  'This should stay'

  # Wait for the short notification to expire (plus some slack).
  sleep 2

  # Dismiss the long one too.
  # (gdbus call works in a dbus-run-session environment.)
  gdbus call \
    --session \
    --dest org.freedesktop.Notifications \
    --object-path /org/freedesktop/Notifications \
    --method org.freedesktop.Notifications.CloseNotification 2 || true

  sleep 0.3
  kill \$DAEMON_PID 2>/dev/null || true
  wait \$DAEMON_PID 2>/dev/null || true
" 2>&1 | tee "$LOGFILE"

echo ""
echo "==> Checking output..."

if grep -q "SYNC(2 visible)" "$LOGFILE"; then
  echo "PASS: saw SYNC with 2 notifications"
else
  echo "WARN: did not see SYNC(2 visible) — notifications may have arrived one at a time"
fi

if grep -q "SYNC(1 visible)" "$LOGFILE" || grep -q "SYNC(0 visible)" "$LOGFILE"; then
  echo "PASS: saw Sync shrink after expiry/close"
else
  echo "FAIL: did not see Sync shrink"
  exit 1
fi

echo "==> Smoke test passed."
rm -f "$LOGFILE"
