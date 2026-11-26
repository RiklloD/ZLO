#!/bin/sh
# Build ZLO (the Rune app and the backend) on Linux and install it for this
# user: the Linux counterpart of bundle.sh.
#
#   ~/.local/share/zlo/app/zlo           the window (Rune)
#   ~/.local/share/zlo/app/zlo-backend   IronClaw, started by the window
#   ~/.local/bin/zlo-desktop             link to the window, for a terminal
#   ~/.local/share/applications/zlo.desktop and its icon, for the app menu
#
# The backend is also copied to ~/.zlo/bin/zlo (the `zlo` CLI: `zlo service`,
# `zlo pair`, `zlo remote ...`) unless one is already there.
#
#   sh tools/ironclaw_rune/install-linux.sh           build and install
#   sh tools/ironclaw_rune/install-linux.sh --light   the window only, for a
#                                                     backend on another computer
#
# Light installs to ~/.local/share/zlo/light/ with a `zlo-light` mark beside
# the binary and no zlo-backend, so the app runs app-only (service.rs
# `is_light_bundle`). Computer use is macOS-only and is not installed.
#
# Needs: Rust (rustup), a C toolchain and pkg-config, and the window-system
# headers Rune links against. On Debian/Ubuntu:
#   sudo apt install build-essential pkg-config libssl-dev libwayland-dev \
#     libxkbcommon-dev libx11-dev libdbus-1-dev libfontconfig1-dev
# Optional at run time: xdg-utils (open files and links), libnotify-bin
# (notifications), a terminal emulator.
set -eu

light=
for arg in "$@"; do
  case "$arg" in
    --light) light=1 ;;
    *) echo "unknown option: $arg (use --light)" >&2; exit 2 ;;
  esac
done

case "$(uname -s)" in
  Linux) ;;
  *) echo "install-linux.sh is for Linux; on macOS use bundle.sh" >&2; exit 2 ;;
esac

here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
# The app is its own workspace; both follow CARGO_TARGET_DIR when it is set.
app_target="${CARGO_TARGET_DIR:-$here/target}"
backend_target="${CARGO_TARGET_DIR:-$repo/target}"
share="${XDG_DATA_HOME:-$HOME/.local/share}"
if [ -n "$light" ]; then
  dest="$share/zlo/light"
  name="ZLO Light"
  entry=zlo-light
else
  dest="$share/zlo/app"
  name="ZLO"
  entry=zlo
fi

mkdir -p "$dest"
if [ -z "$light" ]; then
  echo "Building the backend…"
  (cd "$repo" && SKIP_FRONTEND_BUILD=1 cargo build --release --locked -p ironclaw --bin zlo)
  # Staged at once: the app's binary is also named zlo, and under a shared
  # CARGO_TARGET_DIR its build would overwrite this one.
  install -m 0755 "$backend_target/release/zlo" "$dest/zlo-backend.new"
fi
echo "Building the app…"
(cd "$here" && cargo build --release --locked -p ironclaw_rune)

# Replace, never write into a running binary.
install -m 0755 "$app_target/release/zlo" "$dest/zlo.new" && mv -f "$dest/zlo.new" "$dest/zlo"
if [ -n "$light" ]; then
  rm -f "$dest/zlo-backend"
  : > "$dest/zlo-light"
else
  rm -f "$dest/zlo-light"
  mv -f "$dest/zlo-backend.new" "$dest/zlo-backend"
  if [ ! -e "$HOME/.zlo/bin/zlo" ]; then
    mkdir -p "$HOME/.zlo/bin"
    install -m 0755 "$dest/zlo-backend" "$HOME/.zlo/bin/zlo"
    echo "Installed the zlo CLI to ~/.zlo/bin/zlo (add ~/.zlo/bin to PATH)."
  fi
fi

mkdir -p "$HOME/.local/bin"
ln -sf "$dest/zlo" "$HOME/.local/bin/$entry-desktop"

icons="$share/icons/hicolor/scalable/apps"
mkdir -p "$icons" "$share/applications"
cp "$here/app/assets/icon.svg" "$icons/$entry.svg"
cat > "$share/applications/$entry.desktop" <<DESKTOP
[Desktop Entry]
Type=Application
Name=$name
Comment=IronClaw agents
Exec="$dest/zlo"
Icon=$entry
Terminal=false
Categories=Development;
StartupWMClass=zlo
DESKTOP
command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database "$share/applications" >/dev/null 2>&1 || true

echo "Installed $name to $dest"
echo "Start it from the app menu, or run: $entry-desktop"
