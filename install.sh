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
# It also puts the `zlo` command on your PATH: <ZLO's data folder>/bin/zlo
# (normally ~/.zlo/bin/zlo, asked of the app's own CLI so a pre-ZLO
# ~/.ironclaw/reborn is moved first) links to the app's bundled CLI
# (ZLO.app/Contents/MacOS/zlo-backend), and one marked block in your login
# shell's profile (~/.zprofile, the first of ~/.bash_profile, ~/.bash_login,
# ~/.profile, or ~/.config/fish/conf.d/zlo.fish) adds that folder. An
# unrelated `zlo` file or link there is left alone. And it turns on
# self-update for the clone (on a branch; a tag pins it), so later:
#
#   zlo update        (or `zlo upgrade`) install the latest ZLO
#   zlo triage        investigate a problem with a coding agent
#   zlo doctor        check this installation
#
# Running this command again also updates.
#
# Without an Apple Development identity the app is signed ad-hoc, so macOS
# asks for its permissions again after each update (`zlo update` keeps
# signing it that way).
#
#   ZLO_SRC=/path          where the source lives (default ~/Library/Caches/ZLO/src,
#                          which the installer owns and resets; a checkout of
#                          your own is only fast-forwarded, and refused when
#                          it has local changes)
#   ZLO_REF=branch         branch or tag to build (default main)
#   ZLO_INSTALL_DEST=/path/ZLO.app   install the app there (default ~/Applications/ZLO.app)
#   ZLO_HOME=/path         ZLO's data folder (default ~/.zlo); the link goes in its bin/
#   ZLO_NO_MODIFY_PATH=1   leave shell profiles alone

say() { printf '\033[1mzlo:\033[0m %s\n' "$*"; }
warn() { printf '\033[33mzlo:\033[0m %s\n' "$*" >&2; }
die() { printf '\033[31mzlo:\033[0m %s\n' "$*" >&2; exit 1; }

# The markers that find (and so never repeat) the PATH block in a profile.
zlo_path_marker='# >>> zlo (added by the ZLO installer) >>>'
zlo_path_end='# <<< zlo <<<'
# $1 single-quoted for sh/bash/zsh, so `$`, spaces and quotes stay literal.
sh_quote() {
  printf "'%s'" "$(printf '%s' "$1" | sed "s/'/'\\\\''/g")"
}

# $1 single-quoted for fish (which escapes \ and ' inside single quotes).
fish_quote() {
  printf "'%s'" "$(printf '%s' "$1" | sed -e 's/\\/\\\\/g' -e "s/'/\\\\'/g")"
}

# Follow $1 through symlinks to the file they end at (it may not exist yet).
resolve_link() {
  p=$1
  n=0
  while [ -L "$p" ] && [ $n -lt 40 ]; do
    t=$(readlink "$p")
    case "$t" in
      /*) p=$t ;;
      *) p="$(dirname "$p")/$t" ;;
    esac
    n=$((n + 1))
  done
  printf '%s' "$p"
}

# ZLO's data folder as the backend $1 resolves it: `config path --home`
# prints only that path, after moving a pre-ZLO ~/.ironclaw/reborn to
# ~/.zlo. Fails when the backend fails or prints anything but one
# absolute path.
zlo_home_of() {
  h=$("$1" config path --home </dev/null 2>/dev/null) || return 1
  case "$h" in
    *'
'*) return 1 ;;
    /*) printf '%s' "$h" ;;
    *) return 1 ;;
  esac
}

# Where ZLO's data lives right now, without a ZLO build to ask and
# without moving anything: what the backend resolves (ZLO_HOME, else
# IRONCLAW_REBORN_HOME, else ~/.zlo), except that a pre-ZLO
# ~/.ironclaw/reborn still holding the data (~/.zlo absent or empty) is
# where it is until the backend moves it (ironclaw_config's
# current_data_location; its test pins the two equal). The update lock
# lives in <this>/update/, so a running `zlo update` is found even before
# the first build here. Never creates anything.
zlo_data_home() {
  if [ -n "${ZLO_HOME+set}" ]; then
    h=$ZLO_HOME
  elif [ -n "${IRONCLAW_REBORN_HOME+set}" ]; then
    h=$IRONCLAW_REBORN_HOME
  else
    [ -n "${HOME:-}" ] || return 1
    h="$HOME/.zlo"
    legacy="$HOME/.ironclaw/reborn"
    if [ -e "$legacy" ] || [ -L "$legacy" ]; then
      linked=
      if [ -L "$legacy" ] && [ -d "$h" ] &&
        [ "$(cd "$legacy" 2>/dev/null && pwd -P)" = "$(cd "$h" && pwd -P)" ]; then
        linked=1
      fi
      if [ -z "$linked" ]; then
        if [ ! -e "$h" ] && [ ! -L "$h" ]; then
          h=$legacy
        elif [ -d "$h" ] && [ ! -L "$h" ] &&
          [ -z "$(ls -A "$h" 2>/dev/null | grep -vx '.DS_Store')" ]; then
          # Empty or Finder's .DS_Store only, as the resolver counts it
          # (home_migration.rs `is_empty_dir`).
          h=$legacy
        fi
      fi
    fi
  fi
  case "$h" in
    /*) printf '%s' "$h" ;;
    *) return 1 ;;
  esac
}

# The link library: run in this shell and, under the link lock, in the
# `sh -c` that lockf(1) starts (so it is a string as well as code).
# (Single-quoted: it holds no single quote.)
zlo_link_lib='
warn() { printf "\033[33mzlo:\033[0m %s\n" "$*" >&2; }

# The bundle id of ZLO (tools/ironclaw_rune/bundle.sh).
zlo_bundle_id=com.numoh.desktop

# Whether $1 is `<X>.app/Contents/MacOS/zlo-backend` of an app whose
# Info.plist names the bundle id of ZLO.
is_zlo_backend() {
  case "$1" in
    /*.app/Contents/MacOS/zlo-backend) ;;
    *) return 1 ;;
  esac
  [ -f "$1" ] || return 1
  plist="${1%/MacOS/zlo-backend}/Info.plist"
  [ "$(plutil -extract CFBundleIdentifier raw -o - "$plist" 2>/dev/null)" = "$zlo_bundle_id" ]
}

# The target recorded in $1/.zlo-managed, when that is a plain file.
read_marker() {
  m="$1/.zlo-managed"
  [ -f "$m" ] && [ ! -L "$m" ] && cat "$m"
}

# Record $2 in $1/.zlo-managed: written to a temp file beside it and
# renamed over, so the marker path is never opened for writing; a marker
# that is a link (or anything but a plain file) is refused.
write_marker() {
  m="$1/.zlo-managed"
  if [ -L "$m" ] || { [ -e "$m" ] && [ ! -f "$m" ]; }; then
    warn "$m is not a plain file; not recording the link in it"
    return 1
  fi
  [ "$(read_marker "$1")" = "$2" ] && return 0
  t=$(mktemp "$1/.zlo-managed.XXXXXX") || return 1
  if printf "%s\n" "$2" >"$t" && mv -f "$t" "$m"; then
    return 0
  fi
  rm -f "$t"
  return 1
}

# Point $1/zlo at $2 when that place belongs to ZLO: absent (created exclusively),
# a link to the backend of a ZLO bundle, or a dangling link to exactly the target
# recorded in $1/.zlo-managed (the app, cli_link.rs, follows the same rule).
# A regular file, or a link to anything else, is left alone (returns 1).
# Run it under $1/.zlo-link.lock (link_under_lock), which the app takes too.
link_zlo_command() {
  dir=$1
  target=$2
  link="$dir/zlo"
  if [ -L "$link" ]; then
    raw=$(readlink "$link")
    case "$raw" in
      /*) cur=$raw ;;
      *) cur="$dir/$raw" ;;
    esac
    if [ "$cur" = "$target" ]; then
      write_marker "$dir" "$target" || true
      return 0
    fi
    owned=
    if is_zlo_backend "$cur"; then
      owned=1
    elif [ ! -e "$cur" ] && [ "$(read_marker "$dir")" = "$raw" ]; then
      owned=1
    fi
    if [ -z "$owned" ]; then
      warn "$link links to $raw, which is not from ZLO; left it as it is"
      return 1
    fi
    staged="$dir/.zlo-link-$$"
    rm -f "$staged"
    ln -s "$target" "$staged" || return 1
    # Re-check right before the swap: only the link we judged is replaced.
    if [ -L "$link" ] && [ "$(readlink "$link")" = "$raw" ] && [ ! -d "$link" ]; then
      mv -f "$staged" "$link" || { rm -f "$staged"; return 1; }
    else
      rm -f "$staged"
      warn "$link changed while ZLO was linking it; left it as it is"
      return 1
    fi
  elif [ -e "$link" ]; then
    warn "$link is a file ZLO did not make; left it as it is"
    return 1
  else
    # `ln -s` without -f fails if something appeared meanwhile.
    ln -s "$target" "$link" 2>/dev/null || {
      warn "could not create $link; left it as it is"
      return 1
    }
  fi
  write_marker "$dir" "$target" || true
  return 0
}
'
eval "$zlo_link_lib"

# link_zlo_command $1 $2 holding $1/.zlo-link.lock (an flock, which the
# app's cli_link.rs takes too), so the link and its marker change together.
link_under_lock() {
  lk="$1/.zlo-link.lock"
  if [ -L "$lk" ]; then
    warn "$lk is a link; left \`zlo\` as it is"
    return 1
  fi
  if ! command -v lockf >/dev/null 2>&1; then
    link_zlo_command "$1" "$2"
    return
  fi
  lockf -k -t 10 "$lk" sh -c "$zlo_link_lib
link_zlo_command \"\$1\" \"\$2\"" sh "$1" "$2" </dev/null
}

# The login profile to edit for the user's shell: sets $profile and
# $profile_kind (sh or fish).
pick_profile() {
  profile_kind=sh
  case "$(basename "${SHELL:-/bin/zsh}")" in
    fish)
      profile_kind=fish
      profile="${XDG_CONFIG_HOME:-$HOME/.config}/fish/conf.d/zlo.fish"
      ;;
    bash)
      # A login bash reads only the first of these that exists.
      profile="$HOME/.bash_profile"
      for f in "$HOME/.bash_profile" "$HOME/.bash_login" "$HOME/.profile"; do
        if [ -e "$f" ] || [ -L "$f" ]; then
          profile=$f
          break
        fi
      done
      ;;
    zsh)
      # ZDOTDIR is often set (unexported) inside ~/.zshenv; ask zsh. The
      # answer comes back on fd 3; whatever ~/.zshenv prints goes nowhere.
      zdot=$(zsh -c 'printf %s "${ZDOTDIR:-$HOME}" >&3' 3>&1 >/dev/null 2>/dev/null </dev/null) || zdot=
      case "$zdot" in
        /*) [ -d "$zdot" ] || zdot= ;;
        *) zdot= ;;
      esac
      [ -n "$zdot" ] || zdot=${ZDOTDIR:-$HOME}
      profile="$zdot/.zprofile"
      ;;
    *)
      profile="$HOME/.profile"
      ;;
  esac
}

# The PATH line for $1 (the bin folder) in a $profile_kind profile.
path_line() {
  if [ "$profile_kind" = fish ]; then
    q=$(fish_quote "$1")
    printf 'contains -- %s $PATH; or set -gx PATH %s $PATH' "$q" "$q"
  else
    printf 'export PATH=%s:"$PATH"' "$(sh_quote "$1")"
  fi
}

# Put the marked block holding $2 into profile $1, or rewrite an existing
# block in place. A symlinked profile is edited at its target. Never fails
# the install: an unwritable profile gets a warning and the line to add.
write_path_block() {
  file=$(resolve_link "$1")
  line=$2
  [ "$file" = "$1" ] || say "$1 is a link; editing its target $file"
  # none: no block; ok: exactly one opening line followed by its closing
  # line; bad: anything else (unclosed, repeated, out of order).
  shape=none
  if [ -f "$file" ]; then
    shape=$(ZLO_M="$zlo_path_marker" ZLO_E="$zlo_path_end" awk '
      $0 == ENVIRON["ZLO_M"] { if (seen) bad = 1; seen = 1; open = 1; next }
      $0 == ENVIRON["ZLO_E"] { if (!open) bad = 1; open = 0; next }
      END { if (bad || open) print "bad"; else if (seen) print "ok"; else print "none" }' "$file") || shape=bad
  fi
  if [ "$shape" = bad ]; then
    warn "$file has a ZLO PATH block that is unclosed, repeated or out of order; left it as it is. Make sure it has this line:"
    printf '  %s\n' "$line" >&2
    return 1
  fi
  if [ "$shape" = ok ]; then
    current=$(awk -v m="$zlo_path_marker" 'f { print; exit } $0 == m { f = 1 }' "$file")
    if [ "$current" = "$line" ]; then
      return 0
    fi
    tmp=$(mktemp "${TMPDIR:-/tmp}/zlo-profile.XXXXXX") || tmp=
    if [ -n "$tmp" ] &&
      ZLO_M="$zlo_path_marker" ZLO_E="$zlo_path_end" ZLO_L="$line" awk '
        skip && $0 == ENVIRON["ZLO_E"] { skip = 0; print; next }
        skip { next }
        $0 == ENVIRON["ZLO_M"] { print; print ENVIRON["ZLO_L"]; skip = 1; next }
        { print }' "$file" >"$tmp" &&
      # Write through the file (not a rename) to keep its mode and owner.
      { cat "$tmp" >"$file"; } 2>/dev/null; then
      rm -f "$tmp"
      say "updated the ZLO PATH block in $file"
      return 0
    fi
    [ -z "$tmp" ] || rm -f "$tmp"
  elif mkdir -p "$(dirname "$file")" 2>/dev/null &&
    {
      # Never glue the block onto a last line that has no newline.
      if [ -s "$file" ] && [ -n "$(tail -c 1 "$file")" ]; then printf '\n'; fi
      printf '%s\n%s\n%s\n' "$zlo_path_marker" "$line" "$zlo_path_end"
    } 2>/dev/null >>"$file"; then
    say "added ZLO to your PATH in $file"
    return 0
  fi
  warn "could not write $file; add this line to it yourself:"
  printf '  %s\n' "$line" >&2
  return 1
}

# Put `zlo` on the PATH: link <ZLO's data folder>/bin/zlo to $1 (the app's
# zlo-backend), then, unless that folder is already on the PATH or
# ZLO_NO_MODIFY_PATH=1, add it to the login shell's profile. Never fails
# the install.
install_zlo_command() {
  backend=$1
  if ! home=$(zlo_home_of "$backend"); then
    warn "could not ask ZLO for its data folder, so \`zlo\` is not linked; run $(sh_quote "$backend") directly"
    zlo_cmd_note="run $(sh_quote "$backend") where you would run \`zlo\`."
    return 0
  fi
  bin="$home/bin"
  # The last thing the installer says (main): new terminals find `zlo`;
  # this one, and the agent's shell that ran the install, do not.
  zlo_cmd_note="\`zlo\` works in new terminal windows. In this one, run $(sh_quote "$bin/zlo")."
  if ! mkdir -p "$bin" 2>/dev/null || ! link_under_lock "$bin" "$backend"; then
    zlo_cmd_note="run $(sh_quote "$backend") where you would run \`zlo\`."
    warn "\`zlo\` is not linked; run $(sh_quote "$backend") instead"
    return 0
  fi

  pick_profile
  line=$(path_line "$bin")
  on_path=
  case ":${PATH:-}:" in
    *":$bin:"*) on_path=1 ;;
  esac
  if [ -z "$on_path" ]; then
    if [ "${ZLO_NO_MODIFY_PATH:-}" = 1 ]; then
      say "add $bin to your PATH to use \`zlo\`:"
      printf '  %s\n' "$line"
      zlo_cmd_note="\`zlo\` is not on your PATH (ZLO_NO_MODIFY_PATH=1); run $(sh_quote "$bin/zlo")."
    elif write_path_block "$profile" "$line"; then
      say "open a new terminal (or run: $(path_line "$bin" | sed 's/^contains.*; or //')) to use \`zlo\`"
    else
      zlo_cmd_note="\`zlo\` is not on your PATH yet (see above); run $(sh_quote "$bin/zlo")."
    fi
  else
    zlo_cmd_note="\`zlo\` is ready in this terminal."
  fi

  # Another `zlo` (a cargo install, an older link) may answer first.
  other=$(command -v zlo 2>/dev/null) || other=
  if [ -n "$other" ] && [ "$other" != "$bin/zlo" ]; then
    if [ -n "$on_path" ]; then
      warn "\`zlo\` runs $other, which comes before $bin on your PATH; ZLO's is $bin/zlo"
    else
      warn "\`zlo\` runs $other in this terminal; new terminals put $bin first unless your shell config adds $(dirname "$other") later"
    fi
  fi
  return 0
}

# Sync the checkout to $ref, build, and set the self-update policy, as one
# step an update can't race: run with `sh -c` (under the updater's lock when
# there is one) so it is a single command. $1 src, $2 ref, $3 branch|tag,
# $4 repo URL, $5 1 when $1 is the installer's own cache, $6 the backend
# the build installs.
#
# The installer's cache (~/Library/Caches/ZLO/src, marked by
# .git/zlo-install-cache) is reset to $ref by force. A ZLO_SRC of your own
# is never reset: it is refused when it has local changes, a branch only
# fast-forwards, an existing tag is never moved, and git refuses rather than
# overwrite an ignored file of yours (--no-overwrite-ignore).
zlo_sync_and_build='
set -eu
src=$1 ref=$2 kind=$3 url=$4 default_cache=$5 backend=$6
fail() { printf "zlo: %s\n" "$*" >&2; exit 1; }
if [ ! -d "$src/.git" ]; then
  if [ -e "$src" ] && [ -n "$(ls -A "$src" 2>/dev/null)" ]; then
    fail "$src exists and is not a git checkout; move it away or set ZLO_SRC"
  fi
  mkdir -p "$src"
  git init -q "$src"
  git -C "$src" remote add origin "$url"
  # Only a checkout this installer creates is ever its own: an existing one
  # without the marker (an older install, or another clone) is treated
  # as yours below, never reset by force.
  [ "$default_cache" = 1 ] && : >"$src/.git/zlo-install-cache"
fi
owned=
[ "$default_cache" = 1 ] && [ -f "$src/.git/zlo-install-cache" ] && owned=1
if [ -n "$owned" ]; then
  if [ "$kind" = branch ]; then
    git -C "$src" fetch -q --depth 1 origin "+refs/heads/$ref:refs/remotes/origin/$ref"
    git -C "$src" checkout -q -f -B "$ref" "refs/remotes/origin/$ref"
  else
    git -C "$src" fetch -q --depth 1 origin "+refs/tags/$ref:refs/tags/$ref"
    git -C "$src" checkout -q -f --detach "refs/tags/$ref"
  fi
else
  [ -z "$(git -C "$src" status --porcelain)" ] ||
    fail "$src has local changes (see git status there); commit or stash them, or unset ZLO_SRC"
  if [ "$kind" = branch ]; then
    git -C "$src" fetch -q origin "+refs/heads/$ref:refs/remotes/origin/$ref"
    if git -C "$src" show-ref --verify --quiet "refs/heads/$ref"; then
      git -C "$src" merge-base --is-ancestor "refs/heads/$ref" "refs/remotes/origin/$ref" ||
        fail "$ref in $src has commits origin/$ref does not; it can only fast-forward, so ZLO left it as it is"
      git -C "$src" switch -q --no-overwrite-ignore "$ref"
      git -C "$src" merge -q --ff-only --no-overwrite-ignore "refs/remotes/origin/$ref"
    else
      git -C "$src" switch -q --no-overwrite-ignore -c "$ref" "refs/remotes/origin/$ref"
    fi
  else
    # No +: a tag of that name already here is never moved.
    git -C "$src" fetch -q origin "refs/tags/$ref:refs/tags/$ref"
    git -C "$src" checkout -q --no-overwrite-ignore --detach "refs/tags/$ref"
  fi
fi
echo "zlo: building (10 to 20 minutes the first time)"
ZLO_ALLOW_ADHOC=1 sh "$src/tools/ironclaw_rune/bundle.sh"
[ -x "$backend" ] || fail "the build finished but $backend is missing"

# `zlo update` and the app follow this clone; set while the lock is held,
# so an update cannot start from the policy the previous install left.
if [ "$kind" = tag ]; then
  if out=$("$backend" update disable </dev/null 2>&1); then
    echo "zlo: pinned to tag $ref, so self-update is off; run this installer again (with ZLO_REF) to move"
  else
    printf "zlo: could not turn self-update off for tag %s, so zlo update may move it: %s\n" "$ref" "$out" >&2
    exit 3
  fi
elif out=$("$backend" update enable --repo "$src" --branch "$ref" </dev/null 2>&1); then
  echo "zlo: self-update is on: zlo update (or zlo upgrade) installs new versions"
else
  printf "zlo: could not turn on self-update; run this installer again to update. (%s)\n" "$out" >&2
fi
'

# Everything runs from main() so a cut-off download runs nothing.
main() {
  set -eu
  repo_url=https://github.com/RiklloD/ZLO.git
  default_src="$HOME/Library/Caches/ZLO/src"
  src="${ZLO_SRC:-$default_src}"
  default_cache=0
  [ "$src" = "$default_src" ] && default_cache=1
  ref="${ZLO_REF:-main}"
  app="${ZLO_INSTALL_DEST:-$HOME/Applications/ZLO.app}"
  app=${app%/}
  backend="$app/Contents/MacOS/zlo-backend"

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

  # A branch is followed by self-update; a tag pins the install.
  url=$repo_url
  [ -d "$src/.git" ] && url=$(git -C "$src" remote get-url origin 2>/dev/null) || url=$repo_url
  if git ls-remote --exit-code --heads "$url" "refs/heads/$ref" >/dev/null 2>&1 </dev/null; then
    kind=branch
  elif git ls-remote --exit-code --tags "$url" "refs/tags/$ref" >/dev/null 2>&1 </dev/null; then
    kind=tag
  else
    die "$url has no branch or tag named $ref"
  fi

  # A running `zlo update` owns the checkout and the app: share its lock
  # (<data folder>/update/update.lock, an flock that lockf(1) takes too),
  # found without a ZLO build to ask (zlo_data_home), and held through the
  # self-update policy write.
  lock=
  if command -v lockf >/dev/null 2>&1 && home=$(zlo_data_home); then
    if (umask 077 && mkdir -p "$home/update") && [ ! -L "$home/update/update.lock" ]; then
      lock="$home/update/update.lock"
    else
      warn "could not use $home/update/update.lock; a running \`zlo update\` would not be waited for"
    fi
  fi
  if [ -d "$src/.git" ]; then say "updating $src to $ref"; else say "downloading ZLO to $src"; fi
  status=0
  if [ -n "$lock" ]; then
    lockf -k -t 0 "$lock" sh -c "$zlo_sync_and_build" sh "$src" "$ref" "$kind" "$url" "$default_cache" "$backend" </dev/null || status=$?
    if [ "$status" = 75 ]; then
      say "waiting for a running \`zlo update\` to finish"
      status=0
      lockf -k -t 3600 "$lock" sh -c "$zlo_sync_and_build" sh "$src" "$ref" "$kind" "$url" "$default_cache" "$backend" </dev/null || status=$?
    fi
  else
    sh -c "$zlo_sync_and_build" sh "$src" "$ref" "$kind" "$url" "$default_cache" "$backend" </dev/null || status=$?
  fi
  case "$status" in
    0) ;;
    3) warn "installed, but self-update could not be turned off for tag $ref (see above)" ;;
    *) die "the download, build or install failed (exit $status)" ;;
  esac

  zlo_cmd_note=
  install_zlo_command "$backend"

  open "$app"
  say "installed $app"
  say "ZLO is open: its setup connects your agents. Nothing else to run."
  [ -z "$zlo_cmd_note" ] || say "$zlo_cmd_note"
}

main "$@"
