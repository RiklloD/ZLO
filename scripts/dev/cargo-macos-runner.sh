#!/usr/bin/env bash
# Cargo `runner` for macOS: stable-sign the binary, then run it.
#
# Cargo invokes this as `<runner> <binary> <args...>` for `cargo run`,
# `cargo test` and `cargo bench`. Test harness binaries (legacy `deps/` or
# Cargo 1.100+ `build/<pkg>/<hash>/out/`) are not signed: they never touch the
# OS keychain (see `os_keychain_suppressed` in
# crates/substrates/ironclaw_secrets/src/keychain.rs), and re-signing ad-hoc
# test harnesses can trigger macOS SIGKILL. See codesign-dev-binary.sh for the why.
set -euo pipefail
bin="$1"
case "$bin" in
  */deps/*|*/build/*/*/out/*) ;;
  *)
    "$(dirname "${BASH_SOURCE[0]}")/codesign-dev-binary.sh" "$bin" || true
    ;;
esac
exec "$@"
