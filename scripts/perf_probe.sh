#!/usr/bin/env bash
# On-machine performance probe for lion-session 0.2.0.
#
# Measures, against a private D-Bus + fake compositor (same shape the
# integration tests use):
#   1. time from process start until os.lionos.Session owns its bus name
#      (the "desktop is reachable" moment for shells)
#   2. time from start until the Wayland socket exists
#   3. steady-state RSS of the daemon
#   4. the same startup probe for `dbus-run-session` (the minimal Linux
#      baseline that ships with dbus itself) for calibration
#
# Usage: scripts/perf_probe.sh [path-to-lion-session]
set -u

BIN=${1:-./target/release/lion-session}
BIN=$(realpath "$BIN")
DIR=$(mktemp -d)
RUNTIME="$DIR/runtime"; mkdir -p "$RUNTIME"; chmod 700 "$RUNTIME"
SOCK="$DIR/bus"
CONF="$DIR/bus.conf"

cat > "$CONF" <<EOF
<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN" "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <listen>unix:path=$SOCK</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*"/><allow receive_sender="*"/><allow own="*"/><allow user="*"/>
  </policy>
</busconfig>
EOF

cat > "$DIR/compositor.py" <<'EOF'
import os, signal, socket, sys, time
path = os.path.join(os.environ["XDG_RUNTIME_DIR"], "wayland-1")
pidfile = sys.argv[1]
try: os.unlink(path)
except FileNotFoundError: pass
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.bind(path); s.listen(16)
with open(pidfile, "w") as f: f.write(str(os.getpid()))
done = False
def stop(*a):
    global done; done = True
signal.signal(signal.SIGTERM, stop)
s.settimeout(0.2)
while not done:
    try:
        c, _ = s.accept(); c.close()
    except socket.timeout: continue
    except OSError: break
try: os.unlink(path)
except FileNotFoundError: pass
EOF

cat > "$DIR/session.toml" <<EOF
logout-animation-ms = 20
[compositor]
command = "python3"
args = ["$DIR/compositor.py", "$DIR/compositor.pid"]
wayland-display = "wayland-1"
ready-timeout-ms = 10000
restart = true
max-restarts = 3
[session]
end-timeout-ms = 500
xdg-autostart = false
[[autostart]]
name = "true"
command = "true"
EOF

dbus-daemon --config-file "$CONF" --print-address=1 --nofork --nopidfile > "$DIR/addr" 2>/dev/null &
DBUS_PID=$!
for _ in $(seq 100); do [ -s "$DIR/addr" ] && break; sleep 0.05; done
ADDR=$(head -1 "$DIR/addr")

now_ns() { date +%s%N; }
ms_between() { echo $(( ($2 - $1) / 1000000 )); }

echo "lion-session binary: $BIN ($(stat -c%s "$BIN") bytes)"
echo

for i in 1 2 3; do
  T0=$(now_ns)
  env -i PATH="$PATH" HOME="$DIR" XDG_RUNTIME_DIR="$RUNTIME" \
      DBUS_SESSION_BUS_ADDRESS="$ADDR" DBUS_SYSTEM_BUS_ADDRESS="$ADDR" \
      LION_SESSION_CONFIG="$DIR/session.toml" \
      "$BIN" > "$DIR/log" 2>&1 &
  SESSION_PID=$!
  # poll for the bus name
  NAME_MS=-1
  for _ in $(seq 4000); do
    if busctl --address="$ADDR" call org.freedesktop.DBus /org/freedesktop/DBus \
        org.freedesktop.DBus NameHasOwner s os.lionos.Session 2>/dev/null | grep -q true; then
      NAME_MS=$(ms_between "$T0" "$(now_ns)"); break
    fi
    kill -0 "$SESSION_PID" 2>/dev/null || break
  done
  # poll for the wayland socket
  SOCK_MS=-1
  for _ in $(seq 2000); do
    [ -S "$RUNTIME/wayland-1" ] && { SOCK_MS=$(ms_between "$T0" "$(now_ns)"); break; }
  done
  sleep 0.3
  RSS=$(ps -o rss= -p "$SESSION_PID" 2>/dev/null | tr -d ' ')
  echo "run $i: bus-name after ${NAME_MS} ms, wayland socket after ${SOCK_MS} ms, RSS ${RSS:-?} kB"
  # graceful end through the public API
  busctl --address="$ADDR" call os.lionos.Session /os/lionos/Session os.lionos.Session1 Logout >/dev/null 2>&1
  wait "$SESSION_PID" 2>/dev/null
  CODE=$?
  echo "        exit code after Logout(): $CODE"
done
echo
echo "--- teardown ---"
kill "$DBUS_PID" 2>/dev/null
rm -rf "$DIR"

echo
echo "=== baseline: dbus-run-session (minimal session, no management) ==="
for i in 1 2 3; do
  T0=$(now_ns)
  dbus-run-session -- true >/dev/null 2>&1
  echo "dbus-run-session true: $(ms_between "$T0" "$(now_ns)") ms"
done
echo
echo "=== control: exec true ==="
for i in 1 2 3; do
  T0=$(now_ns)
  true
  echo "exec true: $(ms_between "$T0" "$(now_ns)") ms"
done
