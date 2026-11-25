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
#   ZLO_SRC=/path          where the source lives (default ~/Library/Caches/ZLO/src)
#   ZLO_REF=branch         branch or tag to build (default main)
#   ZLO_HOME=/path         ZLO's data folder (default ~/.zlo); the link goes in its bin/
#   ZLO_NO_MODIFY_PATH=1   leave shell profiles alone

say() { printf '\033[1mzlo:\033[0m %s\n' "$*"; }
warn() { printf '\033[33mzlo:\033[0m %s\n' "$*" >&2; }
die() { printf '\033[31mzlo:\033[0m %s\n' "$*" >&2; exit 1; }

# The markers that find (and so never repeat) the PATH block in a profile.
zlo_path_marker='# >>> zlo (added by the ZLO installer) >>>'
zlo_path_end='# <<< zlo <<<'
# ZLO's bundle id (tools/ironclaw_rune/bundle.sh).
zlo_bundle_id=com.numoh.desktop

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

# ZLO's data folder as the backend $1 resolves it (`zlo config path` moves
# a pre-ZLO ~/.ironclaw/reborn to ~/.zlo first). Fails when it cannot say.
zlo_home_of() {
  h=$("$1" config path </dev/null 2>/dev/null | sed -n 's/^reborn_home: //p' | head -n 1) || return 1
  case "$h" in
    /*) printf '%s' "$h" ;;
    *) return 1 ;;
  esac
}

# Whether $1 is `<X>.app/Contents/MacOS/zlo-backend` of an app whose
# Info.plist names ZLO's bundle id.
is_zlo_backend() {
  case "$1" in
    /*.app/Contents/MacOS/zlo-backend) ;;
    *) return 1 ;;
  esac
  [ -f "$1" ] || return 1
  plist="${1%/MacOS/zlo-backend}/Info.plist"
  [ "$(plutil -extract CFBundleIdentifier raw -o - "$plist" 2>/dev/null)" = "$zlo_bundle_id" ]
}

# Point $1/zlo at $2 when that place is ZLO's: absent (created exclusively),
# a link to a ZLO bundle's backend, or a dangling link to exactly the target
# recorded in $1/.zlo-managed (the app, cli_link.rs, follows the same rule).
# A regular file, or a link to anything else, is left alone (returns 1).
link_zlo_command() {
  dir=$1
  target=$2
  link="$dir/zlo"
  marker="$dir/.zlo-managed"
  if [ -L "$link" ]; then
    raw=$(readlink "$link")
    case "$raw" in
      /*) cur=$raw ;;
      *) cur="$dir/$raw" ;;
    esac
    if [ "$cur" = "$target" ]; then
      printf '%s\n' "$target" 2>/dev/null >"$marker" || true
      return 0
    fi
    owned=
    if is_zlo_backend "$cur"; then
      owned=1
    elif [ ! -e "$cur" ] && [ -f "$marker" ] && [ "$(cat "$marker")" = "$raw" ]; then
      owned=1
    fi
    if [ -z "$owned" ]; then
      warn "$link links to $raw, which is not ZLO's; left it as it is"
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
  printf '%s\n' "$target" 2>/dev/null >"$marker" || true
  return 0
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
      # ZDOTDIR is often set (unexported) inside ~/.zshenv; ask zsh.
      zdot=$(zsh -c 'printf %s "${ZDOTDIR:-$HOME}"' </dev/null 2>/dev/null) || zdot=
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
  if [ -f "$file" ] && grep -qF "$zlo_path_marker" "$file"; then
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
    return 0
  fi
  bin="$home/bin"
  if ! mkdir -p "$bin" 2>/dev/null || ! link_zlo_command "$bin" "$backend"; then
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
    elif write_path_block "$profile" "$line"; then
      say "open a new terminal (or run: $(path_line "$bin" | sed 's/^contains.*; or //')) to use \`zlo\`"
    fi
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

# Sync the checkout to $ref and build, as one step an update can't race:
# run with `sh -c` (under the updater's lock when there is one) so it is a
# single command. $1 src, $2 ref, $3 branch|tag, $4 repo URL.
zlo_sync_and_build='
set -eu
src=$1 ref=$2 kind=$3 url=$4
if [ -d "$src/.git" ]; then
  if [ "$kind" = branch ]; then
    git -C "$src" fetch --depth 1 origin "+refs/heads/$ref:refs/remotes/origin/$ref"
    git -C "$src" checkout -q -f -B "$ref" "refs/remotes/origin/$ref"
  else
    git -C "$src" fetch --depth 1 origin "+refs/tags/$ref:refs/tags/$ref"
    git -C "$src" checkout -q -f --detach "refs/tags/$ref"
  fi
else
  mkdir -p "$(dirname "$src")"
  git clone --depth 1 --branch "$ref" "$url" "$src"
fi
echo "zlo: building (10 to 20 minutes the first time)"
ZLO_ALLOW_ADHOC=1 sh "$src/tools/ironclaw_rune/bundle.sh"
'

# Everything runs from main() so a cut-off download runs nothing.
main() {
  set -eu
  repo_url=https://github.com/RiklloD/ZLO.git
  src="${ZLO_SRC:-$HOME/Library/Caches/ZLO/src}"
  ref="${ZLO_REF:-main}"
  app="$HOME/Applications/ZLO.app"
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
  # (<data folder>/update/update.lock, an flock that lockf(1) takes too).
  lock=
  if [ -x "$backend" ] && command -v lockf >/dev/null 2>&1 && home=$(zlo_home_of "$backend"); then
    (umask 077 && mkdir -p "$home/update") && lock="$home/update/update.lock"
  fi
  if [ -d "$src/.git" ]; then say "updating $src to $ref"; else say "downloading ZLO to $src"; fi
  if [ -n "$lock" ]; then
    status=0
    lockf -k -t 0 "$lock" sh -c "$zlo_sync_and_build" sh "$src" "$ref" "$kind" "$url" </dev/null || status=$?
    if [ "$status" = 75 ]; then
      say "waiting for a running \`zlo update\` to finish"
      status=0
      lockf -k -t 3600 "$lock" sh -c "$zlo_sync_and_build" sh "$src" "$ref" "$kind" "$url" </dev/null || status=$?
    fi
    [ "$status" = 0 ] || die "the download or build failed (exit $status)"
  else
    sh -c "$zlo_sync_and_build" sh "$src" "$ref" "$kind" "$url" </dev/null
  fi

  [ -x "$backend" ] || die "the build finished but $backend is missing"
  install_zlo_command "$backend"

  # `zlo update` and the app's "Update available" follow this clone.
  if [ "$kind" = tag ]; then
    "$backend" update disable </dev/null >/dev/null 2>&1 || true
    say "pinned to tag $ref, so self-update is off; run this installer again (with ZLO_REF) to move"
  elif enabled=$("$backend" update enable --repo "$src" --branch "$ref" </dev/null 2>&1); then
    say "self-update is on: \`zlo update\` (or \`zlo upgrade\`) installs new versions"
  else
    warn "could not turn on self-update; run this installer again to update. ($enabled)"
  fi

  open "$app"
  say "installed ~/Applications/ZLO.app"
}

main "$@"
