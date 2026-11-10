#!/usr/bin/env bash
# One-shot refresh of WebUI v2 frontend artifacts for local inspection or
# vendored dependency updates. Vite writes generated output into ignored
# dist/, which Cargo does not read: build.rs runs its own Vite build into
# OUT_DIR and embeds that. Only commit public/vendor/ changes when refreshing
# pinned vendor assets.
#
#   ./build.sh           # vendor + pnpm install + Vite build
#   ./build.sh --no-vendor   # skip re-downloading vendored CDN assets
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

if [[ "${1:-}" != "--no-vendor" ]]; then
  bash vendor.sh
fi

echo "Installing build dependencies (pnpm install --frozen-lockfile)…"
if [[ ! -f pnpm-lock.yaml ]]; then
  echo "Error: pnpm-lock.yaml is missing — refusing to install without a lockfile." >&2
  echo "Artifacts must build from the committed lockfile. Restore it and re-run." >&2
  exit 1
fi
corepack pnpm install --frozen-lockfile

echo "Building SPA with Vite…"
pnpm build

echo "All WebUI v2 frontend artifacts rebuilt."
