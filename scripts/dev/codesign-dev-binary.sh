#!/usr/bin/env bash
# Re-sign locally built macOS binaries with a stable code-signing identity.
#
# Why: the Rust linker ad-hoc signs every build with an identifier that embeds
# a content hash (e.g. `ironclaw-6310a60dc50a4651`). macOS Keychain ties an
# "Always Allow" grant to the app's designated requirement, which for an
# ad-hoc signature is that exact hash, so every rebuild looks like a brand-new
# app and the OS keychain prompt comes back. Signing with a real certificate
# and a fixed identifier gives every build the same designated requirement,
# so one "Always Allow" covers all future builds.
#
# Usage: IRONCLAW_CODESIGN_IDENTITY="Apple Development: ..." \
#          scripts/dev/codesign-dev-binary.sh <binary>...
# With no identity configured, or off macOS, this is a no-op.
set -euo pipefail

identity="${IRONCLAW_CODESIGN_IDENTITY:-}"
[[ "$(uname -s)" == "Darwin" && -n "$identity" ]] || exit 0

for bin in "$@"; do
  [[ -f "$bin" ]] || continue
  name="$(basename "$bin")"
  ident="${IRONCLAW_CODESIGN_ID_PREFIX:-ai.ironclaw.dev}.${name}"
  # Fast path: already carries our identifier, so this build was signed
  # already (a fresh link resets it to the ad-hoc hash identifier).
  if codesign -d -v "$bin" 2>&1 | grep -qx "Identifier=${ident}"; then
    continue
  fi
  if ! codesign --force --sign "$identity" --identifier "$ident" "$bin" 2>/dev/null; then
    echo "warning: could not codesign ${bin} with \"${identity}\"; the OS keychain may prompt" >&2
  fi
done
