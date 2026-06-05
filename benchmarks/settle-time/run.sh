#!/usr/bin/env bash
set -euo pipefail

if [ $# -lt 1 ] || [ $# -gt 2 ]; then
  echo "usage: ./run.sh <github-url> [ref]" >&2
  echo "env: AFT_SETTLE_SEMANTIC=on|off AFT_SETTLE_TIMEOUT_SECS=1800 AFT_SETTLE_IDLE_SECS=30" >&2
  exit 2
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
BUILD_IMAGE="${AFT_SETTLE_BUILD_IMAGE:-aft-build-linux}"
RUNTIME_IMAGE="${AFT_SETTLE_IMAGE:-aft-settle-time}"
PLATFORM="${AFT_SETTLE_PLATFORM:-linux/amd64}"

mkdir -p "$SCRIPT_DIR/.docker" "$SCRIPT_DIR/results" "$SCRIPT_DIR/cache"

echo "==> Building Linux AFT binary with tests/docker/Dockerfile.build-linux ($PLATFORM)"
docker build --platform "$PLATFORM" -t "$BUILD_IMAGE" -f "$REPO_ROOT/tests/docker/Dockerfile.build-linux" "$REPO_ROOT"
CID="$(docker create "$BUILD_IMAGE" true)"
trap 'docker rm -f "$CID" >/dev/null 2>&1 || true' EXIT
docker cp "$CID:/build/target/release/aft" "$SCRIPT_DIR/.docker/aft-linux-x64"
docker rm -f "$CID" >/dev/null
trap - EXIT
chmod +x "$SCRIPT_DIR/.docker/aft-linux-x64"

echo "==> Building settle-time benchmark image ($PLATFORM)"
docker build --platform "$PLATFORM" -t "$RUNTIME_IMAGE" -f "$SCRIPT_DIR/Dockerfile" "$REPO_ROOT"

args=("$1")
if [ $# -eq 2 ]; then
  args+=("$2")
fi

echo "==> Running settle-time benchmark"
docker run --rm --platform "$PLATFORM" \
  -e AFT_SETTLE_SEMANTIC="${AFT_SETTLE_SEMANTIC:-on}" \
  -e AFT_SETTLE_TIMEOUT_SECS="${AFT_SETTLE_TIMEOUT_SECS:-1800}" \
  -e AFT_SETTLE_IDLE_SECS="${AFT_SETTLE_IDLE_SECS:-30}" \
  -e AFT_SETTLE_CPU_THRESHOLD="${AFT_SETTLE_CPU_THRESHOLD:-5}" \
  -e AFT_SETTLE_STATUS_INTERVAL="${AFT_SETTLE_STATUS_INTERVAL:-5}" \
  -e AFT_SETTLE_SAMPLE_INTERVAL="${AFT_SETTLE_SAMPLE_INTERVAL:-5}" \
  -e AFT_SETTLE_PROGRESS_INTERVAL="${AFT_SETTLE_PROGRESS_INTERVAL:-30}" \
  -e AFT_SETTLE_CLEAR_STORAGE="${AFT_SETTLE_CLEAR_STORAGE:-1}" \
  -e RUST_LOG="${RUST_LOG:-info}" \
  -v "$SCRIPT_DIR/results:/results" \
  -v "$SCRIPT_DIR/cache:/cache" \
  "$RUNTIME_IMAGE" "${args[@]}"
