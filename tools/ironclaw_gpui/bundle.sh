#!/bin/sh
# Build ZLO (app, backend and computer-use engine) and install it to
# ~/Applications. This is the one install step; nothing else goes to
# ~/.ironclaw/bin or launchd any more.
#
#   ZLO.app/Contents/MacOS/zlo                        the window (GPUI)
#   ZLO.app/Contents/MacOS/zlo-backend                IronClaw, started by zlo
#   ZLO.app/Contents/Helpers/ZLO Computer Use.app     the engine, started by the backend
#
# Everything runs as a child of ZLO, so macOS asks for Accessibility and
# Screen Recording once, for ZLO. That grant is tied to the signing
# identity: IRONCLAW_SIGN_IDENTITY, else the first "Apple Development"
# identity in the keychain. With neither, the build stops: ad-hoc signing
# makes macOS ask again after every rebuild, re-prompts for the keychain and
# refuses notifications (ZLO_ALLOW_ADHOC=1 forces it). zlo-backend keeps
# the identifier com.numoh.backend, which the keychain item trusts.
#
# The engine is our patched Open Computer Use, a pinned release of
# RiklloD/hermes-plugin-computer-use-plus checked against ENGINE_SHA256.
# ZLO_ENGINE_APP=/path/to/Some.app uses a local build instead.
#
#   sh tools/ironclaw_gpui/bundle.sh             build, install, reopen if it was open
#   sh tools/ironclaw_gpui/bundle.sh --no-install build only (target/ZLO.app)
set -eu

ENGINE_TAG=engine-v0.6.0
ENGINE_URL=https://github.com/RiklloD/hermes-plugin-computer-use-plus/releases/download/$ENGINE_TAG/open-computer-use-engine-arm64.zip
ENGINE_SHA256=098dc77a2ae1a9e817a2c4e7967ae016c11ed01cf315874b5f7dd44a8bfbebf0

here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
icon_src="$here/app/assets/icon.png"
app="$here/target/ZLO.app"
dest="$HOME/Applications/ZLO.app"
old_dest="$HOME/Applications/Numoh.app"
engine_name="ZLO Computer Use.app"

(cd "$repo" && SKIP_FRONTEND_BUILD= cargo build --release -p ironclaw)
cd "$here"
cargo build --release -p ironclaw_gpui
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)

rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources" "$app/Contents/Helpers"
cp target/release/zlo "$app/Contents/MacOS/zlo"
cp "$repo/target/release/ironclaw" "$app/Contents/MacOS/zlo-backend"

# Engine: a local build, or the pinned release (cached, checksum verified).
engine_override="${ZLO_ENGINE_APP:-${NUMOH_ENGINE_APP:-}}"
if [ -n "$engine_override" ]; then
  cp -R "$engine_override" "$app/Contents/Helpers/$engine_name"
else
  cache="$HOME/Library/Caches/ZLO/$ENGINE_TAG.zip"
  old_cache="$HOME/Library/Caches/Numoh/$ENGINE_TAG.zip"
  if [ ! -f "$cache" ] && [ -f "$old_cache" ]; then
    mkdir -p "$(dirname "$cache")"
    cp "$old_cache" "$cache"
  fi
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
engine_plist="$app/Contents/Helpers/$engine_name/Contents/Info.plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleName ZLO Computer Use" \
  -c "Set :CFBundleDisplayName ZLO Computer Use" "$engine_plist"

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
    <string>ZLO</string>
    <key>CFBundleDisplayName</key>
    <string>ZLO</string>
    <key>CFBundleIdentifier</key>
    <string>com.numoh.desktop</string>
    <key>CFBundleExecutable</key>
    <string>zlo</string>
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
</dict>
</plist>
EOF

identity="${IRONCLAW_SIGN_IDENTITY:-$(security find-identity -v -p codesigning 2>/dev/null | awk '/Apple Development/{print $2; exit}')}"
if [ -z "$identity" ]; then
  [ -n "${ZLO_ALLOW_ADHOC:-${NUMOH_ALLOW_ADHOC:-}}" ] || {
    echo "no Apple Development identity: ad-hoc signing resets macOS permissions and the keychain trust on every rebuild. Set ZLO_ALLOW_ADHOC=1 to build anyway." >&2
    exit 1; }
  echo "warning: ZLO_ALLOW_ADHOC set, signing ad-hoc" >&2
fi
# Inside out: nested code first, the outer seal last.
codesign --force --sign "${identity:--}" --timestamp=none "$app/Contents/Helpers/$engine_name"
codesign --force --sign "${identity:--}" --timestamp=none --identifier com.numoh.backend \
  "$app/Contents/MacOS/zlo-backend"
codesign --force --sign "${identity:--}" --timestamp=none "$app"
codesign --verify --deep --strict "$app"
echo "Built $app ($version)"

[ "${1:-}" = "--no-install" ] && exit 0

# Replace, never overwrite in place: macOS kills a running binary whose
# signed file changed underneath it. Quitting ZLO also stops its backend.
was_running=
if pgrep -f "$dest/Contents/MacOS/zlo" >/dev/null || pgrep -f "$old_dest/Contents/MacOS/numoh" >/dev/null; then
  was_running=1
  osascript -e 'quit app "ZLO"' >/dev/null 2>&1 || true
  osascript -e 'quit app "Numoh"' >/dev/null 2>&1 || true
  i=0
  while pgrep -f "$dest/Contents/MacOS/" >/dev/null || pgrep -f "$old_dest/Contents/MacOS/" >/dev/null; do
    i=$((i + 1))
    [ $i -gt 60 ] && { echo "ZLO did not quit; quit it and run again." >&2; exit 1; }
    sleep 0.5
  done
fi
mkdir -p "$HOME/Applications"
rm -rf "$dest" "$old_dest"
cp -R "$app" "$dest"
echo "Installed $dest"
[ -n "$was_running" ] && open "$dest"
exit 0
