# Unified local-first task runner for ZLO / IronClaw Reborn.
# Run `just --list` to see all available commands.

set shell := ["bash", "-euo", "pipefail", "-c"]

# List available recipes
default:
    @just --list

# First-time local setup: WASM target, git hooks, tool checks, cargo check
setup:
    ./scripts/dev-setup.sh

# Format all Rust crates
fmt:
    cargo fmt --all

# Verify formatting without modifying files
fmt-check:
    cargo fmt --all -- --check

# Fast workspace typecheck (all targets, all features)
check:
    cargo check --workspace --all-targets --all-features

# Zero-warnings Clippy across the entire workspace
lint:
    cargo clippy --all --benches --tests --examples --all-features -- -D warnings

# Run tests (prefers cargo-nextest when installed, falls back to cargo test)
test *ARGS:
    @if command -v cargo-nextest >/dev/null 2>&1; then \
        cargo nextest run {{ARGS}}; \
    else \
        cargo test {{ARGS}}; \
    fi

# Run tests for a single crate (e.g. `just test-pkg ironclaw_hooks`)
test-pkg PKG *ARGS:
    @if command -v cargo-nextest >/dev/null 2>&1; then \
        cargo nextest run -p {{PKG}} {{ARGS}}; \
    else \
        cargo test -p {{PKG}} {{ARGS}}; \
    fi

# Run architecture boundary & ratchet tests
test-arch *ARGS:
    @if command -v cargo-nextest >/dev/null 2>&1; then \
        cargo nextest run -p ironclaw_architecture_tests --no-fail-fast {{ARGS}}; \
    else \
        cargo test -p ironclaw_architecture_tests --no-fail-fast {{ARGS}}; \
    fi

# Full local-first deterministic preflight gate (scripts + self-tests + arch + module charters)
gate *ARGS:
    bash scripts/preflight-gates.sh {{ARGS}}

# Fast static/script-only preflight gates (~1 min, no Rust compile)
gate-fast:
    bash scripts/preflight-gates.sh --layer script

# Supply-chain & dependency policy check (advisories, licenses, bans, sources)
deny:
    cargo deny check

# Detect unused dependencies across the workspace
shear:
    cargo shear

# Audit GitHub workflow hygiene locally (actionlint + zizmor + ws12 contracts)
workflows-check:
    @if command -v actionlint >/dev/null 2>&1; then actionlint; else echo "actionlint not installed (brew install actionlint)"; fi
    @if command -v zizmor >/dev/null 2>&1; then zizmor --offline .github/workflows .github/actions; else echo "zizmor not installed (brew install zizmor)"; fi
    @PY=$(command -v python3.13 || command -v python3.12 || command -v python3.11 || echo python3); "$PY" scripts/ci/ws12_workflow_contracts.py

# WebUI frontend lint, typecheck, unit tests, and bundle budget check
webui:
    cd crates/product/ironclaw_webui/frontend && pnpm lint && pnpm test && pnpm build

# Run the ZLO server locally with debug logging
serve *ARGS:
    RUST_LOG=ironclaw=debug cargo run -p ironclaw -- serve {{ARGS}}

# Reclaim disk space: prune dead git worktrees and clean idle target/ directories in other worktrees
gc:
    #!/usr/bin/env bash
    set -euo pipefail
    CUR="$(git rev-parse --show-toplevel)"
    git worktree prune -v
    echo "==> Cleaning target/ directories in inactive worktrees (keeping $CUR/target)"
    git worktree list --porcelain | awk '/^worktree /{print $2}' | while read -r wt; do
        [ "$wt" = "$CUR" ] && continue
        for t in "$wt/target" "$wt/tools/ironclaw_rune/target" "$wt/tools/ironclaw_silk_decoder/target"; do
            if [ -d "$t" ]; then
                echo "  removing $t ($(du -sh "$t" | cut -f1))"
                rm -rf "$t"
            fi
        done
    done
    if command -v sccache >/dev/null 2>&1; then
        echo "==> sccache stats:"
        sccache --show-stats | head -12
    fi
    df -h /

# Cut a release using the canonical release script
release-cut *ARGS:
    @PY=$(command -v python3.13 || command -v python3.12 || command -v python3.11 || echo python3); "$PY" scripts/ci/cut_ironclaw_release.py {{ARGS}}
