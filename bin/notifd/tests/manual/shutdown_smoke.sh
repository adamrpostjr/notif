#!/usr/bin/env bash
# Shutdown smoke test: notifd must exit cleanly within 2s on SIGINT and SIGTERM.
# Runs on an isolated D-Bus session (never touches the real session bus).
# Usage: bash bin/notifd/tests/manual/shutdown_smoke.sh
set -u
cd "$(dirname "$0")/../../../.."

cargo build -p notifd 2>&1 | tail -1 || exit 1

fail=0
for sig in INT TERM; do
    result=$(dbus-run-session -- sh -c '
        ./target/debug/notifd >/dev/null 2>&1 &
        ND=$!
        sleep 1.5
        kill -'"$sig"' $ND
        for i in $(seq 1 20); do
            kill -0 $ND 2>/dev/null || { echo "exited"; break; }
            sleep 0.1
        done
        if kill -0 $ND 2>/dev/null; then
            kill -9 $ND
            echo "hung"
        fi
        wait $ND
        echo "code=$?"
    ')
    if echo "$result" | grep -q "exited" && echo "$result" | grep -q "code=0"; then
        echo "PASS: SIG$sig -> clean exit (code 0, <2s)"
    else
        echo "FAIL: SIG$sig -> $result"
        fail=1
    fi
done
exit $fail
