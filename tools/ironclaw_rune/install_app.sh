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
if ! was_running=$(zlo_quit_bundle "$dest" 30); then
  echo "$name did not quit; quit it and run again." >&2
  exit 1
fi
mkdir -p "$(dirname "$dest")"
rm -rf "$dest"
cp -R "$src" "$dest"
echo "Installed $dest"
[ -n "$was_running" ] && open "$dest"
exit 0
