#!/usr/bin/env bash
# Runs the whole stack on one machine:
#   gstreamer-source (H.264 :5010, H.265 :5011) -> tq-publisher -> [tq-impair] -> tq-relay -> browser
#
#   dev/run-local.sh                 # direct
#   IMPAIR=starlink dev/run-local.sh # publisher uplink through the Starlink profile
#
# Needs streaming-tools (https://github.com/haffi96/streaming-tools) next to
# this repo, or STREAMING_TOOLS=...
# Ctrl+C stops everything.
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT=$PWD
STREAMING_TOOLS=${STREAMING_TOOLS:-$ROOT/../streaming-tools}
export TQ_SHARED_SECRET=${TQ_SHARED_SECRET:-dev-secret-change-me}
CONFIG=${CONFIG:-config/publisher.example.json}

cargo build --release -q
(cd web && bun install --silent && bun run build >/dev/null)

pids=()
cleanup() { kill "${pids[@]}" 2>/dev/null || true; }
trap cleanup EXIT INT TERM

if [[ -z "${NO_SOURCES:-}" ]]; then
  (cd "$STREAMING_TOOLS/tools/gstreamer_source" && exec uv run gstreamer-source --port 5010 --timestamps) &
  pids+=($!)
  (cd "$STREAMING_TOOLS/tools/gstreamer_source" && exec uv run gstreamer-source --codec h265 --stream-format avc --port 5011 --timestamps) &
  pids+=($!)
fi

./target/release/tq-relay --dev --web-dir web/dist &
pids+=($!)
sleep 1

RELAY_URL=https://127.0.0.1:4443
if [[ -n "${IMPAIR:-}" ]]; then
  ./target/release/tq-impair --profile "$IMPAIR" --listen 127.0.0.1:4444 --upstream 127.0.0.1:4443 &
  pids+=($!)
  RELAY_URL=https://127.0.0.1:4444
fi

./target/release/tq-publisher --config "$CONFIG" --relay-url "$RELAY_URL" &
pids+=($!)

echo
echo "operator: http://localhost:8080/?room=robot-01&role=operator"
echo "viewer:   http://localhost:8080/?room=robot-01&role=viewer"
echo
wait
