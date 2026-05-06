#!/usr/bin/env bash
# =============================================================================
# macOS native E2E test for AFT plugin running inside OpenCode.
#
# Mirror of `tests/docker/test-e2e.sh` (which runs on Linux inside Docker),
# adapted for native macOS. The actual scenarios run by sourcing
# `tests/docker/test-e2e.sh` with AFT_E2E_PLATFORM=macos. This script is
# only responsible for the host setup that `Dockerfile.linux-x64` does on
# Linux: install OpenCode + Bun + aimock, write configs, place locally-built
# AFT binary + plugin dist, then invoke the shared harness.
#
# What this catches that the Linux + Windows harnesses cannot:
#   - FSEvents watcher behavior (different coalescing latency from inotify;
#     see callgraph::tests::callgraph_watcher_{add,remove}_caller flake fix
#     in v0.19.5 for an example we'd previously caught only via local dev).
#   - /var vs /private/var symlink canonicalization (issue fixed in v0.18.0).
#   - Broken-symlink-chain fallback in context.rs.
#   - Apple Silicon native ARM64 codegen for the aft binary.
#   - macOS dylib loading (.dylib extension, /usr/local/lib + /opt/homebrew/lib
#     probe paths) for ONNX Runtime.
#
# Required env (set by _e2e-suite.yml):
#   AFT_BINARY_PATH  — absolute path to the locally-built aft to test
#   AFT_PLUGIN_DIST  — absolute path to packages/opencode-plugin/dist/
#
# Exit codes:
#   0 — all checks passed
#   1 — at least one check failed
#   2 — environment setup failed
# =============================================================================

set -euo pipefail

# ---- Validate required env -------------------------------------------------
: "${AFT_BINARY_PATH:?AFT_BINARY_PATH must point at a built aft binary}"
: "${AFT_PLUGIN_DIST:?AFT_PLUGIN_DIST must point at packages/opencode-plugin/dist/}"

if [ ! -x "$AFT_BINARY_PATH" ]; then
    echo "AFT_BINARY_PATH does not exist or is not executable: $AFT_BINARY_PATH" >&2
    exit 2
fi
if [ ! -d "$AFT_PLUGIN_DIST" ]; then
    echo "AFT_PLUGIN_DIST is not a directory: $AFT_PLUGIN_DIST" >&2
    exit 2
fi

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
echo "── macOS E2E setup ──"
echo "  Repo root:    $REPO_ROOT"
echo "  AFT binary:   $AFT_BINARY_PATH"
echo "  Plugin dist:  $AFT_PLUGIN_DIST"

# ---- Install OpenCode ------------------------------------------------------
# Pin to a known-good version. The OpenCode installer's `latest` resolution
# hits api.github.com anonymously, which is rate-limited to 60 req/hr per
# egress IP — and macOS GH Actions runners share egress, so concurrent runs
# (or repeated re-runs) hit the limit and the installer prints
# "Failed to fetch version information" then exits 1. Pinning sidesteps the
# probe entirely. Bump alongside the Linux Docker image so both harnesses
# exercise the same OpenCode version.
OPENCODE_VERSION="${OPENCODE_VERSION:-1.14.39}"
echo "── Installing OpenCode v${OPENCODE_VERSION} ──"
curl -fsSL https://opencode.ai/install | bash -s -- --version "$OPENCODE_VERSION"
export PATH="$HOME/.opencode/bin:$PATH"
opencode --version

# ---- Install aimock --------------------------------------------------------
# Pinned to 1.17.0 to match the Linux harness — 1.18.0 renamed mock.onTurn(...)
# and breaks our fixtures with `mock.onTurn is not a function`.
echo "── Installing aimock ──"
npm install -g @copilotkit/aimock@1.17.0

# ---- Set up test project ---------------------------------------------------
# Mirror the structure Dockerfile.linux-x64 builds at /test/project.
TEST_PROJECT="$RUNNER_TEMP/aft-e2e-project"
rm -rf "$TEST_PROJECT"
mkdir -p "$TEST_PROJECT/src"
cp -R "$REPO_ROOT/tests/docker/fixtures/sample-project/." "$TEST_PROJECT/src/"
echo '{"name":"test","version":"1.0.0"}' > "$TEST_PROJECT/package.json"
(
    cd "$TEST_PROJECT"
    git init -q
    git config user.email "test@test.com"
    git config user.name "Test"
    git add -A
    git commit -q -m "init"
)
echo "  Test project: $TEST_PROJECT"

# ---- OpenCode + AFT config -------------------------------------------------
# OpenCode uses XDG_CONFIG_HOME on macOS just like Linux.
export XDG_CONFIG_HOME="$RUNNER_TEMP/aft-e2e-xdg"
OC_CONFIG_DIR="$XDG_CONFIG_HOME/opencode"
mkdir -p "$OC_CONFIG_DIR"

# AFT plugin pointed at @latest from npm + aimock provider
cat > "$OC_CONFIG_DIR/opencode.json" <<EOF
{
  "\$schema": "https://opencode.ai/config.json",
  "plugin": ["@cortexkit/aft-opencode@latest"],
  "provider": {
    "mock": {
      "api": "openai",
      "name": "aimock",
      "options": { "baseURL": "http://127.0.0.1:4010/v1" },
      "models": {
        "mock-model": {
          "name": "Mock Model"
        }
      }
    }
  }
}
EOF

# AFT — both experimentals on so we exercise the full ONNX/semantic path
cat > "$OC_CONFIG_DIR/aft.jsonc" <<'EOF'
{
  "experimental_search_index": true,
  "experimental_semantic_search": true
}
EOF

cat > "$OC_CONFIG_DIR/tui.json" <<'EOF'
{
  "$schema": "https://opencode.ai/tui.json",
  "plugin": ["@cortexkit/aft-opencode@latest"]
}
EOF

# ---- Pre-install plugin from npm + override binary/dist locally ------------
# Mirrors the Dockerfile flow: install @latest from npm so paths exist, then
# overwrite the plugin dist + binary cache with our locally-built artifacts so
# the test exercises the unreleased code under change.
PLUGIN_NPM_DIR="$HOME/.cache/opencode/packages"
mkdir -p "$PLUGIN_NPM_DIR"
(
    cd "$PLUGIN_NPM_DIR"
    npm install --silent @cortexkit/aft-opencode@latest @cortexkit/aft-darwin-arm64@latest 2>/dev/null || true
)

# Inject locally-built AFT binary into the versioned cache
AFT_VER=$(node -p "require('$PLUGIN_NPM_DIR/node_modules/@cortexkit/aft-opencode/package.json').version" 2>/dev/null || echo "0.19.5")
mkdir -p "$HOME/.cache/aft/bin/v${AFT_VER}"
cp "$AFT_BINARY_PATH" "$HOME/.cache/aft/bin/v${AFT_VER}/aft"
chmod +x "$HOME/.cache/aft/bin/v${AFT_VER}/aft"

# Inject locally-built plugin dist over the npm-installed one so TS-side fixes
# (e.g. onnx-runtime.ts) get exercised too.
PLUGIN_DIST_DEST=$(find "$PLUGIN_NPM_DIR" -path '*/node_modules/@cortexkit/aft-opencode/dist' -type d 2>/dev/null | head -1)
if [ -n "$PLUGIN_DIST_DEST" ]; then
    rm -rf "$PLUGIN_DIST_DEST"
    cp -R "$AFT_PLUGIN_DIST" "$PLUGIN_DIST_DEST"
    echo "  Injected local plugin dist into: $PLUGIN_DIST_DEST"
fi

# ---- Run shared harness ----------------------------------------------------
# We don't use aimock's own server.js — the Linux harness drives aimock via
# a custom mock-server.js fixture under tests/docker/. Reuse it directly so
# Linux + macOS exercise the same scripted turns.
export AFT_E2E_MOCK_SERVER="$REPO_ROOT/tests/docker/mock-server.js"
if [ ! -f "$AFT_E2E_MOCK_SERVER" ]; then
    echo "Mock server fixture missing: $AFT_E2E_MOCK_SERVER" >&2
    exit 2
fi

# Make NODE_PATH include the global aimock so its require() resolves.
NPM_PREFIX=$(npm config get prefix 2>/dev/null || echo "")
if [ -n "$NPM_PREFIX" ] && [ -d "$NPM_PREFIX/lib/node_modules" ]; then
    export NODE_PATH="$NPM_PREFIX/lib/node_modules${NODE_PATH:+:$NODE_PATH}"
fi

# Run the shared harness in macOS mode from inside the test project so
# `opencode run` resolves source files relative to a real project dir.
cd "$TEST_PROJECT"

export AFT_E2E_PLATFORM=macos
export AFT_E2E_PLUGIN_LOG="${TMPDIR:-/tmp}/aft-plugin.log"

bash "$REPO_ROOT/tests/docker/test-e2e.sh"
