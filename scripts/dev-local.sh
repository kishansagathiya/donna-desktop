#!/usr/bin/env bash
# Run Donna Desktop against the Railway API (default) or a local server.
#
#   npm run dev:desktop
#   npm run dev:local
#
# Optional:
#   DONNA_USE_LOCAL_API=1   start donna-server-go on :8787 instead of Railway
#   SKIP_BROWSER=1          skip Playwright Chromium install
#   FORCE_SIDECAR=1         rebuild donna-agent-local even if the binary exists
#   DONNA_PORT=8787         local API port when DONNA_USE_LOCAL_API=1
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
DESKTOP_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
ROOT="$(cd "$DESKTOP_DIR/.." && pwd)"
SERVER_DIR="$ROOT/donna-server-go"
WEB_DIR="$ROOT/donna-web"
ENV_FILE="$ROOT/.env"
RAILWAY_API="https://donna-server-go-production.up.railway.app"
USE_LOCAL="${DONNA_USE_LOCAL_API:-0}"
API_PORT="${DONNA_PORT:-8787}"
WEB_BASE="https://donnadoesit.com"
ARCH="$(uname -m)"
TRIPLE="aarch64-apple-darwin"
if [[ "$ARCH" == "x86_64" ]]; then
  TRIPLE="x86_64-apple-darwin"
fi
SIDECAR="$DESKTOP_DIR/src-tauri/binaries/donna-agent-local-$TRIPLE"

SERVER_PID=""
STARTED_SERVER=0

if [[ "$USE_LOCAL" == "1" ]]; then
  API_BASE="http://127.0.0.1:${API_PORT}"
  API_PROXY="$API_BASE"
else
  API_BASE="$RAILWAY_API"
  API_PROXY="$RAILWAY_API"
fi

log() { printf '==> %s\n' "$*"; }
fail() { printf 'error: %s\n' "$*" >&2; exit 1; }

need_cmd() {
  command -v "$1" >/dev/null 2>&1 || fail "Missing $1. $2"
}

cleanup() {
  local code=$?
  if [[ "$STARTED_SERVER" == "1" && -n "$SERVER_PID" ]]; then
    log "Stopping local API (pid ${SERVER_PID})..."
    kill "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
  fi
  exit "$code"
}

trap cleanup EXIT INT TERM

[[ "$(uname -s)" == "Darwin" ]] || fail "Donna Desktop v1 is macOS-only."
need_cmd node "Install Node 20+."
need_cmd npm "Install npm."
need_cmd cargo "Install Rust: https://rustup.rs/"
[[ -d "$SERVER_DIR" ]] || fail "Expected sibling checkout at $SERVER_DIR"
[[ -d "$WEB_DIR" ]] || fail "Expected sibling checkout at $WEB_DIR"

if [[ "$USE_LOCAL" == "1" || "${FORCE_SIDECAR:-}" == "1" || ! -x "$SIDECAR" ]]; then
  need_cmd go "Install Go: https://go.dev/dl/"
fi

if [[ "$USE_LOCAL" == "1" ]]; then
  [[ -f "$ENV_FILE" ]] || fail "Missing $ENV_FILE — run: cp .env.example .env and set OPENROUTER_API_KEY + SUPABASE_*"
  if ! grep -q '^OPENROUTER_API_KEY=.\+' "$ENV_FILE"; then
    fail "Set OPENROUTER_API_KEY in $ENV_FILE"
  fi
fi

log "Installing npm dependencies..."
if [[ ! -d "$DESKTOP_DIR/node_modules" ]]; then
  (cd "$DESKTOP_DIR" && npm install)
fi
if [[ ! -d "$WEB_DIR/node_modules" ]]; then
  (cd "$WEB_DIR" && npm install)
fi

if [[ "${SKIP_BROWSER:-}" != "1" ]]; then
  if [[ ! -d "$DESKTOP_DIR/browser/node_modules" ]]; then
    log "Installing Playwright Chromium for local browser tools..."
    (cd "$DESKTOP_DIR/browser" && npm install && npx playwright install chromium)
  fi
fi

if [[ "${FORCE_SIDECAR:-}" == "1" || ! -x "$SIDECAR" ]]; then
  log "Building donna-agent-local sidecar..."
  (cd "$DESKTOP_DIR" && bash scripts/build-sidecar.sh)
else
  log "Sidecar already built: $SIDECAR"
fi

health_ok() {
  curl -sf "$API_BASE/health" >/dev/null 2>&1
}

if [[ "$USE_LOCAL" == "1" ]]; then
  if health_ok; then
    log "Reusing API already running at $API_BASE"
  else
    if curl -sf --max-time 1 "$API_BASE/" >/dev/null 2>&1; then
      fail "Port ${API_PORT} is in use but /health is not responding."
    fi
    log "Starting donna-server-go on :${API_PORT}..."
    (
      cd "$SERVER_DIR"
      exec env DONNA_PORT="${API_PORT}" DONNA_LOCAL_AGENTS_V1=true go run ./cmd/server
    ) &
    SERVER_PID=$!
    STARTED_SERVER=1
    log "Waiting for ${API_BASE}/health..."
    for _ in $(seq 1 60); do
      if health_ok; then
        break
      fi
      if ! kill -0 "$SERVER_PID" 2>/dev/null; then
        fail "API process exited before becoming healthy. Check .env and the server logs above."
      fi
      sleep 0.5
    done
    health_ok || fail "API did not become healthy at $API_BASE/health"
    log "API healthy."
  fi
else
  log "Using Railway API: $API_BASE"
  if ! health_ok; then
    log "Warning: $API_BASE/health did not return 200. Continuing anyway."
  fi
fi

log ""
log "Launching Donna Desktop."
log "API: $API_BASE"
log "After sign-in: Profile → Experimental → Local agents (macOS)."
log "Production needs supabase/migrations/0031_desktop_local_agents.sql applied."
log ""

cd "$DESKTOP_DIR"
export DONNA_API_BASE="$API_BASE"
export DONNA_API_PROXY="$API_PROXY"
export DONNA_WEB_APP_BASE="$WEB_BASE"
export DONNA_PORT="${API_PORT}"
npm run tauri -- dev
