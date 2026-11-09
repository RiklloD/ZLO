#!/bin/sh
# Install ZLO from source on an Apple Silicon Mac:
#
#   curl -fsSL https://raw.githubusercontent.com/RiklloD/ZLO/main/install.sh | sh
#
# Checks for the Xcode command-line tools, installs Rust through rustup if
# it is missing, clones (or updates) the repo and runs bundle.sh, which
# builds, signs and installs ~/Applications/ZLO.app. Run it again to update.
# The first build takes 10 to 20 minutes; later ones reuse the build cache.
#
# Without an Apple Development identity the app is signed ad-hoc, so macOS
# asks for its permissions again after each update.
#
#   ZLO_SRC=/path    where the source lives (default ~/Library/Caches/ZLO/src)
#   ZLO_REF=branch   branch or tag to build (default main)

# Everything sits in main() so a cut-off download runs nothing.
main() {
  set -eu
  repo_url=https://github.com/RiklloD/ZLO.git
  src="${ZLO_SRC:-$HOME/Library/Caches/ZLO/src}"
  ref="${ZLO_REF:-main}"

  say() { printf '\033[1mzlo:\033[0m %s\n' "$*"; }
  die() { printf '\033[31mzlo:\033[0m %s\n' "$*" >&2; exit 1; }

  [ "$(uname -s)" = Darwin ] || die "ZLO runs on macOS only."
  [ "$(uname -m)" = arm64 ] || die "ZLO needs an Apple Silicon Mac (M1 or later)."

  if ! xcode-select -p >/dev/null 2>&1; then
    xcode-select --install >/dev/null 2>&1 || true
    die "Install the Xcode command-line tools in the window that just opened, then run this command again."
  fi

  if ! command -v cargo >/dev/null 2>&1; then
    [ -x "$HOME/.cargo/bin/cargo" ] || {
      say "installing Rust (rustup)"
      # The repo pins its toolchain in rust-toolchain.toml; cargo fetches it on first build.
      curl -fsSL https://sh.rustup.rs | sh -s -- -y --no-modify-path --default-toolchain none
    }
    PATH="$HOME/.cargo/bin:$PATH"
    export PATH
  fi

  if [ -d "$src/.git" ]; then
    say "updating $src"
    git -C "$src" fetch --depth 1 origin "$ref"
    git -C "$src" reset --hard FETCH_HEAD
  else
    say "downloading ZLO to $src"
    mkdir -p "$(dirname "$src")"
    git clone --depth 1 --branch "$ref" "$repo_url" "$src"
  fi

  say "building (10 to 20 minutes the first time)"
  ZLO_ALLOW_ADHOC=1 sh "$src/tools/ironclaw_rune/bundle.sh"

  open "$HOME/Applications/ZLO.app"
  say "installed ~/Applications/ZLO.app"
}

main "$@"
