#!/bin/sh
# Check the ZLO app (and its gateway client) on Linux: clippy and the tests.
# On Linux it runs natively; elsewhere (macOS) in a Docker container with the
# same toolchain, the repo mounted read-only and caches in named volumes.
#
#   sh tools/ironclaw_rune/linux-check.sh            clippy + tests
#   sh tools/ironclaw_rune/linux-check.sh check      cargo check only
#
# Docker on macOS: colima (`colima start`) or Docker Desktop. The repo must
# sit under your home folder, which colima shares with its VM.
set -eu

mode="${1:-full}"
here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)

case "$mode" in
  check) steps='cargo check --locked -p ironclaw_rune -p zlo_client --all-targets' ;;
  full) steps='cargo clippy --locked -p ironclaw_rune -p zlo_client --all-targets -- -D warnings && cargo test --locked -p ironclaw_rune -p zlo_client' ;;
  *) echo "unknown mode: $mode (use check, or nothing for clippy + tests)" >&2; exit 2 ;;
esac

if [ "$(uname -s)" = Linux ]; then
  cd "$here"
  exec sh -c "$steps"
fi

if [ -z "${DOCKER_HOST:-}" ] && [ -S "$HOME/.colima/default/docker.sock" ]; then
  export DOCKER_HOST="unix://$HOME/.colima/default/docker.sock"
fi
toolchain=$(sed -n 's/^channel *= *"\(.*\)"/\1/p' "$here/rust-toolchain.toml")
image="zlo-linux-check:$toolchain"
if ! docker image inspect "$image" >/dev/null 2>&1; then
  docker build -t "$image" - <<DOCKERFILE
FROM rust:1-bookworm
RUN apt-get update -qq && apt-get install -y -qq pkg-config libwayland-dev libxkbcommon-dev \\
    libx11-dev libdbus-1-dev libssl-dev libfontconfig1-dev mesa-vulkan-drivers \\
    && rm -rf /var/lib/apt/lists/*
RUN rustup toolchain install $toolchain --profile minimal -c clippy
DOCKERFILE
fi
exec docker run --rm \
  -v "$repo":/src:ro \
  -v zlo-linux-check-target:/build \
  -v zlo-linux-check-registry:/usr/local/cargo/registry \
  -v zlo-linux-check-git:/usr/local/cargo/git \
  -e CARGO_TARGET_DIR=/build \
  -w /src/tools/ironclaw_rune \
  "$image" sh -c "$steps"
