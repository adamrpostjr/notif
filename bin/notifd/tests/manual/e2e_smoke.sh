#!/usr/bin/env bash
# End-to-end smoke test for notifd on a live Wayland (Hyprland) session.
#
# Runs notifd on a PRIVATE session bus (dbus-run-session) so an existing
# notification daemon on the real bus is not disturbed; WAYLAND_DISPLAY is
# inherited, so toasts render on the real compositor.
#
# Screenshots are captured with grim into $SHOT_DIR (default: /tmp) — inspect
# them manually. NotificationClosed signals are logged via busctl monitor.
#
# Requirements: dbus-run-session, busctl, notify-send, gdbus, grim, jq.
# Usage: SHOT_DIR=/path/to/shots bin/notifd/tests/manual/e2e_smoke.sh
set -euo pipefail

PROJECT_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../../.." && pwd)"
SHOT_DIR="${SHOT_DIR:-/tmp}"
mkdir -p "$SHOT_DIR"

# Re-exec inside a private session bus unless already there.
if [[ -z "${E2E_INNER:-}" ]]; then
    echo "==> Building notifd"
    cargo build -p notifd --manifest-path "$PROJECT_ROOT/Cargo.toml"
    echo "==> Launching private session bus"
    exec dbus-run-session -- env E2E_INNER=1 SHOT_DIR="$SHOT_DIR" "${BASH_SOURCE[0]}"
fi

DAEMON="$PROJECT_ROOT/target/debug/notifd"
CFG_DIR="$(mktemp -d)"
CFG="$CFG_DIR/config.toml"
MONITOR_LOG="$SHOT_DIR/busctl_monitor.log"

# Start with an explicit config so hot-reload can edit it later.
cat > "$CFG" <<'EOF'
[normal]
background = "#1e1e2e"
EOF

cleanup() {
    [[ -n "${DAEMON_PID:-}" ]] && kill "$DAEMON_PID" 2>/dev/null || true
    [[ -n "${MONITOR_PID:-}" ]] && kill "$MONITOR_PID" 2>/dev/null || true
    rm -rf "$CFG_DIR"
}
trap cleanup EXIT

# Crop region: top-right corner of the first monitor (logical coordinates).
read -r MON_X MON_Y MON_W SCALE < <(hyprctl monitors -j |
    jq -r '.[0] | "\(.x) \(.y) \(.width) \(.scale)"')
LOGICAL_W=$(python3 -c "print(int($MON_W / $SCALE))")
CROP_X=$((MON_X + LOGICAL_W - 620))
CROP="$CROP_X,$MON_Y 620x520"

shot() { sleep "${2:-1}"; grim -g "$CROP" "$SHOT_DIR/$1.png"; echo "shot: $1"; }

echo "==> Starting busctl monitor (NotificationClosed log)"
busctl --user monitor org.freedesktop.Notifications > "$MONITOR_LOG" 2>&1 &
MONITOR_PID=$!

echo "==> Starting notifd"
RUST_LOG=info "$DAEMON" --config "$CFG" &
DAEMON_PID=$!
sleep 1

echo "==> 1. Wrapped body"
notify-send "Build finished" "line one and a much longer second line that must wrap"
shot 1_wrap

echo "==> 2. CJK + emoji"
notify-send "标题 🎉 émoji" "CJK 中文 and emoji 🚀 body"
shot 2_cjk_emoji

echo "==> 3. Critical style"
notify-send -u critical -t 0 "Critical"
shot 3_critical

echo "==> 4. Action button"
gdbus call --session --dest org.freedesktop.Notifications \
    --object-path /org/freedesktop/Notifications \
    --method org.freedesktop.Notifications.Notify \
    "e2e" 0 "" "Choose" "Body with an action" '["open", "Open"]' '{}' 8000
shot 4_action

# Clear the stack for the expiry test.
sleep 9
echo "==> 5. Expiry + NotificationClosed reason 1"
notify-send -t 1000 "gone soon"
shot 5_expiry_before 0.3
shot 5_expiry_after 2

echo "==> 6. Config hot-reload (background color)"
sed -i 's/#1e1e2e/#402060/' "$CFG"
sleep 0.5
notify-send "Recolored" "background should be purple now"
shot 6_hot_reload

kill "$DAEMON_PID" 2>/dev/null || true
wait "$DAEMON_PID" 2>/dev/null || true

echo "==> NotificationClosed entries in monitor log:"
grep -c "NotificationClosed" "$MONITOR_LOG" || true
echo "==> Done. Screenshots in $SHOT_DIR"
