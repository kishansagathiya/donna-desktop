#!/usr/bin/env bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SERVER="$(cd "$ROOT/../donna-server-go" && pwd)"
OUT="$ROOT/src-tauri/binaries"
mkdir -p "$OUT"

build() {
  local arch="$1"
  local triple="$2"
  echo "building donna-agent-local ($arch → $triple)"
  (
    cd "$SERVER"
    GOOS=darwin GOARCH="$arch" CGO_ENABLED=0 \
      go build -trimpath -ldflags="-s -w" \
      -o "$OUT/donna-agent-local-$triple" \
      ./cmd/donna-agent-local
  )
}

build arm64 aarch64-apple-darwin
if [[ "${DONNA_SIDECAR_INTEL:-}" == "1" ]]; then
  build amd64 x86_64-apple-darwin
fi

echo "sidecars in $OUT"
