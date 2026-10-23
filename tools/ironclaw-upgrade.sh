#!/bin/sh
# Rebuild IronClaw and swap it into the launchd service.
#
# The service runs ~/.ironclaw/bin/ironclaw, a copy of the release build, so
# day-to-day `cargo build` never changes what is running. This script is the
# one deliberate upgrade step.
set -eu

repo=$(cd "$(dirname "$0")/.." && pwd)
bin="$HOME/.ironclaw/bin/ironclaw"

cd "$repo"
SKIP_FRONTEND_BUILD= cargo build --release -p ironclaw
mkdir -p "$(dirname "$bin")"
if [ -f "$bin" ]; then
  cp "$bin" "$bin.previous"
fi
# Replace, never overwrite in place: macOS caches the code signature per file
# and SIGKILLs a binary whose bytes changed under the same inode.
rm -f "$bin"
cp target/release/ironclaw "$bin"
"$bin" service restart
"$bin" --version
echo "Previous binary kept at $bin.previous"
