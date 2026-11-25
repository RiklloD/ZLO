#!/bin/sh
# Install ZLO from source on an Apple Silicon Mac:
#
#   curl -fsSL https://raw.githubusercontent.com/RiklloD/ZLO/main/install.sh | sh
#
# Checks for the Xcode command-line tools, installs Rust through rustup if
# it is missing, clones (or updates) the repo and runs bundle.sh, which
# builds, signs and installs ~/Applications/ZLO.app. The first build takes
# 10 to 20 minutes; later ones reuse the build cache.
#
# It also puts the `zlo` command on your PATH: ~/.zlo/bin/zlo links to the
# app's bundled CLI (ZLO.app/Contents/MacOS/zlo-backend), and one marked
# block in your login shell's profile (~/.zprofile, ~/.bash_profile, or
# ~/.config/fish/conf.d/zlo.fish) adds ~/.zlo/bin. And it turns on
# self-update for the clone, so later:
#
#   zlo update        (or `zlo upgrade`) install the latest ZLO
#   zlo triage        investigate a problem with a coding agent
#   zlo doctor        check this installation
#
# Running this command again also updates.
#
# Without an Apple Development identity the app is signed ad-hoc, so macOS
# asks for its permissions again after each update.
#
#   ZLO_SRC=/path          where the source lives (default ~/Library/Caches/ZLO/src)
#   ZLO_REF=branch         branch or tag to build (default main)
#   ZLO_HOME=/path         ZLO's data folder (default ~/.zlo); the link goes in its bin/
#   ZLO_NO_MODIFY_PATH=1   leave shell profiles alone

say() { printf '\033[1mzlo:\033[0m %s\n' "$*"; }
warn() { printf '\033[33mzlo:\033[0m %s\n' "$*" >&2; }
die() { printf '\033[31mzlo:\033[0m %s\n' "$*" >&2; exit 1; }

# The marker that finds (and so never repeats) the PATH block in a profile.
zlo_path_marker='# >>> zlo (added by the ZLO installer) >>>'

# Put `zlo` on the PATH: link <ZLO_HOME>/bin/zlo to $1 (the app's
# zlo-backend), then, unless `zlo` already resolves or ZLO_NO_MODIFY_PATH=1,
# add that folder to the login shell's profile once.
install_zlo_command() {
  backend=$1
  bin="${ZLO_HOME:-$HOME/.zlo}/bin"
  mkdir -p "$bin"
  if [ -e "$bin/zlo" ] && [ ! -L "$bin/zlo" ]; then
    warn "$bin/zlo is a file ZLO did not make; left it as it is"
  else
    ln -sfn "$backend" "$bin/zlo"
  fi

  case "$bin" in
    "$HOME"/*) shown="\$HOME/${bin#"$HOME"/}" ;;
    *) shown=$bin ;;
  esac
  case ":${PATH:-}:" in
    *":$bin:"*) return 0 ;;
  esac
  if command -v zlo >/dev/null 2>&1; then
    return 0
  fi
  if [ "${ZLO_NO_MODIFY_PATH:-}" = 1 ]; then
    say "add $bin to your PATH to use \`zlo\` (export PATH=\"$shown:\$PATH\")"
    return 0
  fi

  case "$(basename "${SHELL:-/bin/zsh}")" in
    fish)
      profile="$HOME/.config/fish/conf.d/zlo.fish"
      line="contains \"$bin\" \$PATH; or set -gx PATH \"$bin\" \$PATH"
      ;;
    bash)
      profile="$HOME/.bash_profile"
      line="export PATH=\"$shown:\$PATH\""
      ;;
    zsh)
      profile="${ZDOTDIR:-$HOME}/.zprofile"
      line="export PATH=\"$shown:\$PATH\""
      ;;
    *)
      profile="$HOME/.profile"
      line="export PATH=\"$shown:\$PATH\""
      ;;
  esac
  if [ -f "$profile" ] && grep -qF "$zlo_path_marker" "$profile"; then
    :
  else
    mkdir -p "$(dirname "$profile")"
    # Never glue the block onto a last line that has no newline.
    if [ -s "$profile" ] && [ -n "$(tail -c 1 "$profile")" ]; then
      printf '\n' >>"$profile"
    fi
    printf '%s\n%s\n%s\n' "$zlo_path_marker" "$line" '# <<< zlo <<<' >>"$profile"
    say "added $bin to your PATH in $profile"
  fi
  say "open a new terminal (or run: export PATH=\"$shown:\$PATH\") to use \`zlo\`"
}

# Everything runs from main() so a cut-off download runs nothing.
main() {
  set -eu
  repo_url=https://github.com/RiklloD/ZLO.git
  src="${ZLO_SRC:-$HOME/Library/Caches/ZLO/src}"
  ref="${ZLO_REF:-main}"

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

  app="$HOME/Applications/ZLO.app"
  backend="$app/Contents/MacOS/zlo-backend"
  [ -x "$backend" ] || die "the build finished but $backend is missing"
  install_zlo_command "$backend"

  # `zlo update` and the app's "Update available" follow this clone.
  if enabled=$("$backend" update enable --repo "$src" --branch "$ref" 2>&1); then
    say "self-update is on: \`zlo update\` (or \`zlo upgrade\`) installs new versions"
  else
    warn "could not turn on self-update; run this installer again to update. ($enabled)"
  fi

  open "$app"
  say "installed ~/Applications/ZLO.app"
}

main "$@"
