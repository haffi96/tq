#!/usr/bin/env bash
# Publishes the local test sources (H.264 :5010, H.265 :5011) to the GCP
# relay, tq-relay-1.haffi.dev. The token secret comes from Terraform state.
#   dev/publish-gcp.sh
# Ctrl+C stops everything.
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT=$PWD
STREAMING_TOOLS=${STREAMING_TOOLS:-$ROOT/../streaming-tools}
CONFIG=${CONFIG:-config/publisher.gcp.json}
TQ_SHARED_SECRET=$(terraform -chdir=infra/gcp output -raw shared_secret)
export TQ_SHARED_SECRET

cargo build --release -q -p tq-publisher

pids=()
cleanup() { kill "${pids[@]}" 2>/dev/null || true; }
trap cleanup EXIT INT TERM

(cd "$STREAMING_TOOLS/tools/gstreamer_source" && exec uv run gstreamer-source --port 5010 --timestamps) &
pids+=($!)
(cd "$STREAMING_TOOLS/tools/gstreamer_source" && exec uv run gstreamer-source --codec h265 --stream-format avc --port 5011 --timestamps) &
pids+=($!)
sleep 2

./target/release/tq-publisher --config "$CONFIG" &
pids+=($!)

# Stop everything when any one exits, with its status. (No `wait -n`: macOS
# ships bash 3.2.)
while :; do
  for pid in "${pids[@]}"; do
    if ! kill -0 "$pid" 2>/dev/null; then
      status=0
      wait "$pid" || status=$?
      echo "process $pid exited with status $status; stopping the rest" >&2
      exit "$status"
    fi
  done
  sleep 1
done
