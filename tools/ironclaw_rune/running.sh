# Sourced by bundle.sh: find, quit and wait for the copy of ZLO that runs
# from one bundle, by executable path. Never by command line (the detached
# updater's own argv names the bundle: `--install-path …/zlo-backend`) and
# never by bundle id (shared by every copy of ZLO). Paths are compared as
# fixed strings, never as patterns, so `[`, `*` or spaces in them are fine.

# The executable file of process $1 (lsof's first `txt` entry), or nothing.
zlo_exe_of() {
  lsof -a -p "$1" -d txt -Fn 2>/dev/null | sed -n 's/^n//p' | head -n 1
}

# This shell and its ancestors, space-separated: never ours to wait on.
zlo_self_and_ancestors() {
  p=$$
  out=
  n=0
  while [ -n "$p" ] && [ "$p" -gt 1 ] 2>/dev/null && [ $n -lt 64 ]; do
    out="$out $p"
    p=$(ps -o ppid= -p "$p" 2>/dev/null | tr -d ' ')
    n=$((n + 1))
  done
  printf '%s ' "$out"
}

# PIDs (one per line) whose executable is <$1>/Contents/MacOS/zlo or
# …/zlo-backend, $1 being a .app; the window first. Excludes this shell and
# its ancestors. The candidates come from the kernel's process names (`zlo`
# also covers the `~/.zlo/bin/zlo` link), then each one's executable path
# must match exactly.
zlo_bundle_pids() {
  real=$(cd "$1" 2>/dev/null && pwd -P) || return 0
  skip=$(zlo_self_and_ancestors)
  for want in zlo zlo-backend; do
    for pid in $( { pgrep -x zlo; pgrep -x zlo-backend; } 2>/dev/null | sort -un); do
      case "$skip" in *" $pid "*) continue ;; esac
      [ "$(zlo_exe_of "$pid")" = "$real/Contents/MacOS/$want" ] && printf '%s\n' "$pid"
    done
  done
  return 0
}

# Whether any of the PIDs in $1 (whitespace-separated) still runs.
zlo_any_alive() {
  for pid in $1; do
    kill -0 "$pid" 2>/dev/null && return 0
  done
  return 1
}

# Quit the ZLO that runs from bundle $1 and wait until those processes are
# gone: ask the window to quit (it stops its backend), then SIGTERM what is
# left. Prints nothing and returns 0 when nothing ran from $1; returns 1
# when they would not stop. $2: seconds to wait (default 30).
zlo_quit_bundle() {
  pids=$(zlo_bundle_pids "$1")
  [ -n "$pids" ] || return 0
  real=$(cd "$1" && pwd -P)
  window=
  for pid in $pids; do
    [ "$(zlo_exe_of "$pid")" = "$real/Contents/MacOS/zlo" ] && window="$window $pid"
  done
  # ZLO_QUIT_SIGNAL_ONLY=1 (tests): signals only, no Apple event.
  if [ -n "$window" ] && [ -z "${ZLO_QUIT_SIGNAL_ONLY:-}" ]; then
    # By path, so another copy of ZLO (same bundle id) is not the one asked.
    osascript -e 'on run argv' -e 'tell application (POSIX file (item 1 of argv) as text) to quit' \
      -e 'end run' "$real" >/dev/null 2>&1 || kill -TERM $window 2>/dev/null || true
  else
    kill -TERM ${window:-$pids} 2>/dev/null || true
  fi
  limit=$(( ${2:-30} * 2 ))
  i=0
  while zlo_any_alive "$pids"; do
    i=$((i + 1))
    # Halfway: whatever is left (a window that ignored the request, an
    # orphaned backend) gets SIGTERM.
    [ $i -eq $((limit / 2)) ] && kill -TERM $pids 2>/dev/null
    [ $i -gt $limit ] && return 1
    sleep 0.5
  done
  printf '%s\n' "$pids"
  return 0
}
