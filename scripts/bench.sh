#!/usr/bin/env bash
# Resource + HTTP benchmark against the REAL sandbox-api binary (release
# build, real Seatbelt isolation). Usage:
#
#   scripts/bench.sh [port]      # default port 18094
#
# Prints a markdown table to stdout and leaves nothing running: the server
# is stopped with SIGTERM, which also exercises the graceful-shutdown path.

set -euo pipefail

PORT="${1:-18094}"
BASE="http://127.0.0.1:$PORT"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"

echo "Building release binary..." >&2
cargo build --release -p sandbox-api >&2

DATA_ROOT="$(mktemp -d)"
BIN="$ROOT/target/release/sandbox-api"

"$BIN" --data-root "$DATA_ROOT" --port "$PORT" --backend seatbelt >/dev/null 2>&1 &
SERVER_PID=$!
trap 'kill -TERM "$SERVER_PID" 2>/dev/null; wait "$SERVER_PID" 2>/dev/null; rm -rf "$DATA_ROOT"' EXIT

# Wait for /health to answer (max ~10 s).
for _ in $(seq 1 50); do
  if curl -sf "$BASE/health" >/dev/null 2>&1; then break; fi
  sleep 0.2
done

rss_kb() { ps -o rss= -p "$1" | tr -d ' '; }

IDLE_RSS_KB=$(rss_kb "$SERVER_PID")
HEALTH_S=$(curl -s -o /dev/null -w '%{time_total}' "$BASE/health")

# Create 20 live sandboxes (real rootfs dirs, real Seatbelt provisions).
N=20
CREATE_TOTAL_S=0
for _ in $(seq 1 "$N"); do
  T=$(curl -s -o /dev/null -w '%{time_total}' -X POST "$BASE/sandboxes" \
    -H 'content-type: application/json' -d '{}')
  CREATE_TOTAL_S=$(awk -v a="$CREATE_TOTAL_S" -v b="$T" 'BEGIN { print a + b }')
done
LOADED_RSS_KB=$(rss_kb "$SERVER_PID")
CREATE_MEAN_MS=$(awk -v t="$CREATE_TOTAL_S" -v n="$N" 'BEGIN { printf "%.1f", t * 1000 / n }')

# Grab one sandbox id for exec/read/delete probes.
SBX=$(curl -s -X POST "$BASE/sandboxes" -H 'content-type: application/json' -d '{}' \
  | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')

# Mean over 5 real Seatbelt echo execs.
EXEC_TOTAL_S=0
for _ in $(seq 1 5); do
  T=$(curl -s -o /dev/null -w '%{time_total}' -X POST "$BASE/sandboxes/$SBX/exec" \
    -H 'content-type: application/json' -d '{"command":"/bin/echo"}')
  EXEC_TOTAL_S=$(awk -v a="$EXEC_TOTAL_S" -v b="$T" 'BEGIN { print a + b }')
done
EXEC_MEAN_MS=$(awk -v t="$EXEC_TOTAL_S" 'BEGIN { printf "%.1f", t * 1000 / 5 }')

FILE_MS=$(curl -s -o /dev/null -w '%{time_total}' -X POST "$BASE/sandboxes/$SBX/files/write" \
  -H 'content-type: application/json' -d '{"path":"bench.txt","content":"payload"}')
METRICS_S=$(curl -s -o /dev/null -w '%{time_total}' "$BASE/metrics")
METRICS_BYTES=$(curl -s "$BASE/metrics" | wc -c | tr -d ' ')
DELETE_MS=$(curl -s -o /dev/null -w '%{time_total}' -X DELETE "$BASE/sandboxes/$SBX")

echo "# b-sandbox resource & latency benchmark (release build, seatbelt backend)"
echo
echo "| metric | value |"
echo "| --- | --- |"
echo "| RSS at idle | ${IDLE_RSS_KB} KB |"
echo "| RSS with $N live sandboxes | ${LOADED_RSS_KB} KB |"
echo "| /health latency | $(awk -v t="$HEALTH_S" 'BEGIN { printf "%.2f ms", t * 1000 }') |"
echo "| create (mean of $N) | ${CREATE_MEAN_MS} ms |"
echo "| exec /bin/echo through Seatbelt (mean of 5) | ${EXEC_MEAN_MS} ms |"
echo "| file write (1 KiB) | $(awk -v t="$FILE_MS" 'BEGIN { printf "%.1f ms", t * 1000 }') |"
echo "| /metrics scrape | $(awk -v t="$METRICS_S" 'BEGIN { printf "%.2f ms", t * 1000 }'), ${METRICS_BYTES} B body |"
echo "| DELETE (terminate + deprovision) | $(awk -v t="$DELETE_MS" 'BEGIN { printf "%.1f ms", t * 1000 }') |"

# The EXIT trap sends SIGTERM, waits for the drained exit, and cleans up.
