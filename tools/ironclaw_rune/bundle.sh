#!/bin/sh
# Build ZLO (the Rune app, backend and computer-use engine) and install it to
# ~/Applications.
#
#   ZLO.app/Contents/MacOS/zlo                        the window (Rune)
#   ZLO.app/Contents/MacOS/zlo-backend                IronClaw, started by zlo
#   ZLO.app/Contents/Helpers/ZLO Computer Use.app     the engine, started by the backend
#
# Bundle id com.numoh.desktop, inherited from the archived GPUI app (branch
# `archive/gpui`), so macOS keeps the permissions (microphone, speech,
# Accessibility, Screen Recording), notification settings, defaults and the
# login item (LaunchAgent com.numoh.desktop, app/src/login_item.rs) already
# granted to ZLO. Installing replaces any earlier ZLO.app, GPUI or Rune.
# zlo-backend keeps the identifier com.numoh.backend, which the keychain
# item trusts.
#
# Signing: IRONCLAW_SIGN_IDENTITY, else the first "Apple Development"
# identity; ZLO_ALLOW_ADHOC=1 forces ad-hoc signing.
#
# The backend is built from this repo (cargo build --release -p ironclaw
# --bin zlo: the package keeps its IronClaw name, the binary is `zlo`)
# unless ZLO_BACKEND=/path/to/zlo names a binary to copy (for example
# ~/Applications/ZLO.app/Contents/MacOS/zlo-backend). It is built without the
# web SPA (SKIP_FRONTEND_BUILD=1): the window is the Rune app, and skipping
# the SPA keeps Node and pnpm out of the build. The engine is the pinned
# release, cached in ~/Library/Caches/ZLO and checksum verified, unless
# ZLO_ENGINE_APP=/path/to/Some.app names a local build.
#
#   sh tools/ironclaw_rune/bundle.sh              build, install, reopen if it was open
#   sh tools/ironclaw_rune/bundle.sh --no-install build only (tools/ironclaw_rune/target/ZLO.app)
#   sh tools/ironclaw_rune/bundle.sh --light      ZLO Light.app: the window only
#
# Light (--light, or ZLO_LIGHT=1; combines with --no-install): no
# zlo-backend and no computer-use engine inside, so ZLO runs app-only and
# works on other computers (paired over Tailscale or SSH). ZLO notices the
# missing zlo-backend at runtime (app/src/service.rs `light_bundle`); there
# is no cargo feature. It is its own app, "ZLO Light.app" with bundle id
# com.numoh.desktop.light, so it never replaces the full ZLO.app.
set -eu

light="${ZLO_LIGHT:-}"
install=1
for arg in "$@"; do
  case "$arg" in
    --light) light=1 ;;
    --no-install) install= ;;
    *) echo "unknown option: $arg (use --light, --no-install)" >&2; exit 2 ;;
  esac
done

ENGINE_TAG=engine-v0.6.0
ENGINE_URL=https://github.com/RiklloD/hermes-plugin-computer-use-plus/releases/download/$ENGINE_TAG/open-computer-use-engine-arm64.zip
ENGINE_SHA256=098dc77a2ae1a9e817a2c4e7967ae016c11ed01cf315874b5f7dd44a8bfbebf0

BUNDLE_ID=com.numoh.desktop
NAME=ZLO
EXE=zlo
if [ -n "$light" ]; then
  BUNDLE_ID=com.numoh.desktop.light
  NAME="ZLO Light"
fi

here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
icon_src="$here/app/assets/icon.png"
app="$here/target/$NAME.app"
dest="$HOME/Applications/$NAME.app"
engine_name="ZLO Computer Use.app"

if [ -z "$light" ]; then
  backend="${ZLO_BACKEND:-}"
  if [ -z "$backend" ]; then
    (cd "$repo" && SKIP_FRONTEND_BUILD=1 cargo build --release -p ironclaw --bin zlo)
    backend="$repo/target/release/zlo"
  fi
  [ -f "$backend" ] || { echo "no backend binary at $backend" >&2; exit 1; }
fi
cd "$here"
cargo build --release -p ironclaw_rune
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)

rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "target/release/$EXE" "$app/Contents/MacOS/$EXE"
if [ -z "$light" ]; then
  mkdir -p "$app/Contents/Helpers"
  cp "$backend" "$app/Contents/MacOS/zlo-backend"
fi

# Engine: a local build, or the pinned release (cached, checksum verified).
# The backend starts it, so a Light bundle has none.
if [ -n "$light" ]; then
  :
elif [ -n "${ZLO_ENGINE_APP:-}" ]; then
  cp -R "$ZLO_ENGINE_APP" "$app/Contents/Helpers/$engine_name"
else
  cache="$HOME/Library/Caches/ZLO/$ENGINE_TAG.zip"
  if [ ! -f "$cache" ] || ! echo "$ENGINE_SHA256  $cache" | shasum -a 256 -c - >/dev/null 2>&1; then
    mkdir -p "$(dirname "$cache")"
    curl -fsSL "$ENGINE_URL" -o "$cache.part"
    mv "$cache.part" "$cache"
  fi
  echo "$ENGINE_SHA256  $cache" | shasum -a 256 -c - >/dev/null || {
    echo "engine checksum mismatch for $cache" >&2; rm -f "$cache"; exit 1; }
  unpack=$(mktemp -d)
  ditto -x -k "$cache" "$unpack"
  mv "$unpack"/*.app "$app/Contents/Helpers/$engine_name"
  rm -rf "$unpack"
fi
if [ -z "$light" ]; then
  engine_plist="$app/Contents/Helpers/$engine_name/Contents/Info.plist"
  /usr/libexec/PlistBuddy -c "Set :CFBundleName ZLO Computer Use" \
    -c "Set :CFBundleDisplayName ZLO Computer Use" "$engine_plist"
fi

# Icon: every size macOS asks for, scaled from the 1024 px app icon.
iconset=$(mktemp -d)/AppIcon.iconset
mkdir -p "$iconset"
for size in 16 32 128 256 512; do
  sips -z $size $size "$icon_src" --out "$iconset/icon_${size}x${size}.png" >/dev/null
  double=$((size * 2))
  sips -z $double $double "$icon_src" --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$iconset" -o "$app/Contents/Resources/AppIcon.icns"
rm -rf "$(dirname "$iconset")"

cat > "$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>
    <string>$NAME</string>
    <key>CFBundleDisplayName</key>
    <string>$NAME</string>
    <key>CFBundleIdentifier</key>
    <string>$BUNDLE_ID</string>
    <key>CFBundleExecutable</key>
    <string>$EXE</string>
    <key>CFBundleIconFile</key>
    <string>AppIcon</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>$version</string>
    <key>CFBundleVersion</key>
    <string>$version</string>
    <key>LSMinimumSystemVersion</key>
    <string>13.0</string>
    <key>LSApplicationCategoryType</key>
    <string>public.app-category.productivity</string>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>NSMicrophoneUsageDescription</key>
    <string>ZLO listens while you dictate, talk to it, or (if you turn it on) for its wake word.</string>
    <key>NSSpeechRecognitionUsageDescription</key>
    <string>ZLO turns your speech into text on this Mac so you can dictate messages.</string>
</dict>
</plist>
EOF
plutil -lint "$app/Contents/Info.plist" >/dev/null

identity="${IRONCLAW_SIGN_IDENTITY:-$(security find-identity -v -p codesigning 2>/dev/null | awk '/Apple Development/{print $2; exit}')}"
if [ -z "$identity" ]; then
  [ -n "${ZLO_ALLOW_ADHOC:-}" ] || {
    echo "no Apple Development identity: ad-hoc signing resets macOS permissions and the keychain trust on every rebuild. Set ZLO_ALLOW_ADHOC=1 to build anyway." >&2
    exit 1; }
  echo "warning: ZLO_ALLOW_ADHOC set, signing ad-hoc" >&2
fi
# Inside out: nested code first, the outer seal last.
if [ -z "$light" ]; then
  codesign --force --sign "${identity:--}" --timestamp=none "$app/Contents/Helpers/$engine_name"
  codesign --force --sign "${identity:--}" --timestamp=none --identifier com.numoh.backend \
    "$app/Contents/MacOS/zlo-backend"
fi
codesign --force --sign "${identity:--}" --timestamp=none "$app"
codesign --verify --deep --strict "$app"
echo "Built $app ($version)"

[ -z "$install" ] && exit 0

# Replace, never overwrite in place: macOS kills a running binary whose
# signed file changed underneath it. Quitting ZLO also stops its backend.
was_running=
if pgrep -f "$dest/Contents/MacOS/$EXE" >/dev/null; then
  was_running=1
  osascript -e "quit app id \"$BUNDLE_ID\"" >/dev/null 2>&1 || true
  i=0
  while pgrep -f "$dest/Contents/MacOS/" >/dev/null; do
    i=$((i + 1))
    [ $i -gt 60 ] && { echo "$NAME did not quit; quit it and run again." >&2; exit 1; }
    sleep 0.5
  done
fi
mkdir -p "$HOME/Applications"
rm -rf "$dest"
cp -R "$app" "$dest"
echo "Installed $dest"
[ -n "$was_running" ] && open "$dest"
exit 0
