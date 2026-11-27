#!/usr/bin/env bash
# Developer setup script for IronClaw.
#
# Gets a fresh checkout ready for development without requiring
# Docker, PostgreSQL, or any external services.
#
# Usage:
#   ./scripts/dev-setup.sh
#
# After running, you can:
#   cargo check           # default build
#   cargo test            # default test suite
#   cargo test --all-features         # full test suite (requires Node.js 22 + Corepack/pnpm for WebUI bundle)

set -euo pipefail

cd "$(dirname "$0")/.."

echo "=== IronClaw Developer Setup ==="
echo ""

# 1. Check rustup
if ! command -v rustup &>/dev/null; then
    echo "ERROR: rustup not found. Install from https://rustup.rs"
    exit 1
fi
echo "[1/6] rustup found: $(rustup --version 2>/dev/null | head -1)"

# 2. Add WASM target (required by build.rs for channel compilation)
echo "[2/6] Adding wasm32-wasip2 target..."
rustup target add wasm32-wasip2

# 3. Install wasm-tools (required by build.rs for WASM component model)
echo "[3/6] Installing wasm-tools..."
if command -v wasm-tools &>/dev/null; then
    echo "  wasm-tools already installed: $(wasm-tools --version)"
else
    cargo install wasm-tools --locked
fi

# 4. Configure git hooks via core.hooksPath (shared across all worktrees)
echo "[4/6] Configuring git hooks (core.hooksPath = .githooks)..."
if git rev-parse --git-dir &>/dev/null; then
    git config core.hooksPath .githooks
    echo "  core.hooksPath set to .githooks (pre-commit safety + WIT version check, commit-msg; no pre-push hook)"
else
    echo "  Skipped: not a git repository"
fi

# 5. Check recommended local-first developer tools
echo "[5/6] Checking recommended local tools (sccache, cargo-nextest, just, cargo-deny, cargo-shear)..."
for tool in sccache cargo-nextest just cargo-deny cargo-shear; do
    if command -v "$tool" &>/dev/null; then
        echo "  ok: $tool"
    else
        echo "  missing (recommended): $tool  (macOS: brew install $tool)"
    fi
done

# 6. Verify workspace check
echo "[6/6] Running cargo check..."
cargo check

echo ""
echo "=== Setup complete ==="
echo ""
echo "Quick start (see `just --list` for all local-first commands):"
echo "  just check          # Fast workspace check"
echo "  just lint           # Clippy (all targets, all features, -D warnings)"
echo "  just test           # Nextest/cargo test suite"
echo "  just gate           # Full local preflight gate"
echo "  just gc             # Reclaim disk from stale worktrees & target/ dirs"
