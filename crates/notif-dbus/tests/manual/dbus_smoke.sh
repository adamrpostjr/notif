#!/usr/bin/env bash
# Manual smoke test for the notif-dbus crate.
#
# Runs the echo_daemon example on a private session bus (via dbus-run-session)
# and exercises the org.freedesktop.Notifications interface with busctl and
# notify-send. Prints PASS/FAIL per check.
#
# Requirements: dbus-run-session, busctl, cargo. notify-send is optional.
#
# Usage: crates/notif-dbus/tests/manual/dbus_smoke.sh
set -euo pipefail

PROJECT_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../../.." && pwd)"

# Re-exec ourselves inside a private session bus unless already there.
if [[ -z "${DBUS_SMOKE_INNER:-}" ]]; then
    echo "==> Building echo_daemon example"
    cargo build -p notif-dbus --example echo_daemon --manifest-path "$PROJECT_ROOT/Cargo.toml"
    echo "==> Launching private session bus"
    exec dbus-run-session -- env DBUS_SMOKE_INNER=1 "$BASH_SOURCE"
fi

DAEMON="$PROJECT_ROOT/target/debug/examples/echo_daemon"
DAEMON_LOG="$(mktemp)"
FAILURES=0

cleanup() {
    [[ -n "${DAEMON_PID:-}" ]] && kill "$DAEMON_PID" 2>/dev/null || true
    rm -f "$DAEMON_LOG"
}
trap cleanup EXIT

check() {
    local name="$1"; shift
    if "$@" >/dev/null 2>&1; then
        echo "PASS: $name"
    else
        echo "FAIL: $name"
        FAILURES=$((FAILURES + 1))
    fi
}

echo "==> Starting echo_daemon"
"$DAEMON" >"$DAEMON_LOG" 2>&1 &
DAEMON_PID=$!

# Wait for the daemon to own the well-known name.
for _ in $(seq 1 50); do
    if busctl --user status org.freedesktop.Notifications >/dev/null 2>&1; then
        break
    fi
    sleep 0.1
done

check "name org.freedesktop.Notifications is owned" \
    busctl --user status org.freedesktop.Notifications

echo "==> GetServerInformation"
INFO="$(busctl --user call org.freedesktop.Notifications /org/freedesktop/Notifications \
    org.freedesktop.Notifications GetServerInformation)"
echo "    $INFO"
check "GetServerInformation returns notif/1.2" \
    grep -q '"notif".*"1.2"' <<<"$INFO"

echo "==> GetCapabilities"
CAPS="$(busctl --user call org.freedesktop.Notifications /org/freedesktop/Notifications \
    org.freedesktop.Notifications GetCapabilities)"
echo "    $CAPS"
check "capabilities include body and actions" \
    grep -q '"body".*"actions"' <<<"$CAPS"

echo "==> Notify (basic)"
REPLY="$(busctl --user call org.freedesktop.Notifications /org/freedesktop/Notifications \
    org.freedesktop.Notifications Notify \
    susssasa\{sv\}i "smoke-test" 0 "" "Hello" "Smoke test body" 2 "default" "Open" 0 5000)"
echo "    $REPLY"
check "Notify returns id 1" test "$REPLY" = "u 1"

echo "==> Notify (replaces_id=1)"
REPLY2="$(busctl --user -- call org.freedesktop.Notifications /org/freedesktop/Notifications \
    org.freedesktop.Notifications Notify \
    susssasa\{sv\}i "smoke-test" 1 "" "Replaced" "Replaced body" 0 1 "urgency" y 2 -1)"
echo "    $REPLY2"
check "replacement keeps id 1" test "$REPLY2" = "u 1"

echo "==> CloseNotification"
check "CloseNotification succeeds" \
    busctl --user call org.freedesktop.Notifications /org/freedesktop/Notifications \
    org.freedesktop.Notifications CloseNotification u 1

echo "==> CloseNotification (unknown id must succeed silently)"
check "CloseNotification with unknown id succeeds" \
    busctl --user call org.freedesktop.Notifications /org/freedesktop/Notifications \
    org.freedesktop.Notifications CloseNotification u 424242

if command -v notify-send >/dev/null 2>&1; then
    echo "==> notify-send"
    check "notify-send delivers" notify-send -u critical "smoke" "via notify-send"
else
    echo "SKIP: notify-send not installed"
fi

echo "==> Second daemon (name taken must be a clean fatal error)"
SECOND_LOG="$(mktemp)"
if timeout 10 "$DAEMON" >"$SECOND_LOG" 2>&1; then
    echo "FAIL: second daemon exited 0 despite name being taken"
    FAILURES=$((FAILURES + 1))
else
    echo "PASS: second daemon exited non-zero"
fi
echo "    second daemon output:"
sed 's/^/        /' "$SECOND_LOG"
check "second daemon reported name-taken error" \
    grep -qi 'already taken' "$SECOND_LOG"
rm -f "$SECOND_LOG"

sleep 0.5
echo
echo "==> Daemon output:"
sed 's/^/    /' "$DAEMON_LOG"

check "daemon saw NOTIFY" grep -q '^NOTIFY id=1' "$DAEMON_LOG"
check "daemon saw CLOSE"  grep -q '^CLOSE id=1'  "$DAEMON_LOG"
check "daemon parsed critical urgency" grep -q 'urgency: Critical' "$DAEMON_LOG"

echo
if [[ "$FAILURES" -eq 0 ]]; then
    echo "SMOKE TEST: ALL CHECKS PASSED"
else
    echo "SMOKE TEST: $FAILURES CHECK(S) FAILED"
    exit 1
fi
