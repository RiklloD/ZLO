#!/usr/bin/env bash
# Cargo `runner` for macOS: stable-sign the binary, then run it.
#
# Cargo invokes this as `<runner> <binary> <args...>` for `cargo run`,
# `cargo test` and `cargo bench`. Test harness binaries (in `deps/`) are not
# signed: they never touch the OS keychain (see `os_keychain_suppressed` in
# crates/substrates/ironclaw_secrets/src/keychain.rs), so signing them would
# only cost time. See codesign-dev-binary.sh for the why.
set -euo pipefail
bin="$1"
if [[ "$(basename "$(dirname "$bin")")" != "deps" ]]; then
  "$(dirname "${BASH_SOURCE[0]}")/codesign-dev-binary.sh" "$bin" || true
fi
exec "$@"
