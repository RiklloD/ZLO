#!/bin/sh
# Install a built ZLO.app over another one, the way bundle.sh does at its
# end (and `zlo update`'s install step does with a downloaded build):
#
#   sh tools/ironclaw_rune/install_app.sh /path/to/built/ZLO.app /path/to/installed/ZLO.app
#
# Replace, never overwrite in place: macOS kills a running binary whose
# signed file changed underneath it. Only the ZLO that runs from the
# destination is quit and waited for, found by executable path (running.sh):
# never the updater running this script, whose arguments name the
# destination, and never another copy of ZLO sharing the bundle id.
# Quitting ZLO also stops its backend. ZLO reopens when it was running.
set -eu

[ $# -eq 2 ] || { echo "usage: install_app.sh <built .app> <installed .app>" >&2; exit 2; }
src=${1%/}
dest=${2%/}
case "$dest" in
  /*.app) ;;
  *) echo "the destination must be an absolute path to a .app (got $dest)" >&2; exit 2 ;;
esac
[ -d "$src/Contents/MacOS" ] || { echo "$src is not a built app" >&2; exit 2; }

here=$(cd "$(dirname "$0")" && pwd)
. "$here/running.sh"
name=$(basename "$dest" .app)
parent=$(dirname "$dest")
mkdir -p "$parent"

# Copy next to the destination first (same volume, so the swap below is a
# rename), while the old ZLO still runs: a full disk or a crash here leaves
# the installed app untouched.
new="$parent/.$name.app.new.$$"
old="$parent/.$name.app.old.$$"
trap 'rm -rf "$new"' EXIT
rm -rf "$new"
cp -R "$src" "$new"
[ -x "$new/Contents/MacOS/zlo" ] || { echo "the copy of $src is incomplete" >&2; exit 1; }

if ! was_running=$(zlo_quit_bundle "$dest" 30); then
  echo "$name did not quit; quit it and run again." >&2
  exit 1
fi
# Swap: the old app moves aside, the new one takes its place, and only
# then is the old one removed. A failed rename puts the old one back.
if [ -e "$dest" ]; then
  mv "$dest" "$old"
fi
if ! mv "$new" "$dest"; then
  [ -e "$old" ] && mv "$old" "$dest"
  echo "could not move the new $name into place" >&2
  exit 1
fi
rm -rf "$old"
echo "Installed $dest"
[ -n "$was_running" ] && open "$dest"
exit 0
