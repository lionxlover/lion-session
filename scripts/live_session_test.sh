#!/usr/bin/env bash
# Live end-to-end test for lion-session: runs the integration suite with
# full output. Every CHECK line is a real assertion against the running
# binary over a private D-Bus with a mock logind, mock locker/power, and
# a real (script) compositor that gets SIGKILLed to test crash recovery.
#
# Usage: scripts/live_session_test.sh
set -u
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"

echo "== building =="
cargo build 2>&1 | tail -1

echo
echo "== unit tests =="
cargo test --lib 2>&1 | grep "test result"

echo
echo "== live end-to-end (mock logind + fake compositor, --nocapture) =="
cargo test --test integration -- --nocapture 2>&1 | grep -E "^(CHECK|test |test result)"

echo
echo "== metrics probe =="
bash scripts/perf_probe.sh 2>/dev/null | grep -E "run [0-9]|exit code|dbus-run-session|exec true"
