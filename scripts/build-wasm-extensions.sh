#!/usr/bin/env bash
# Build the first-party WASM extension guests under crates/extensions/packages/
# and copy each artifact over the committed `.wasm` its manifest names.
#
# The v1 `registry/{tools,channels}/*.json` legs (`--tools`, `--channels`) were
# retired with the `registry/` directory: every entry there published a v0.2x
# release tarball and declared no local source, so both legs only ever SKIPped.
# A retired flag now fails loudly instead of silently building nothing.
#
# Prerequisites:
#   rustup target add wasm32-wasip2
#   rustup target add wasm32-wasip1
#   cargo install cargo-component --locked
#
# Usage:
#   ./scripts/build-wasm-extensions.sh               # build all (first-party)
#   ./scripts/build-wasm-extensions.sh --first-party # same; kept for callers
#
# The package root is resolved from the crate inventory
# (scripts/ci/lib/crate_tree.py) by crate NAME, not from a literal path, so the
# target-architecture family move (crates/<family>/ironclaw_*, PROPOSAL §5)
# does not leave this script unable to find the manifests it is supposed to
# build (docs/internal/reborn/target-architecture/CHECKLIST.md WS10, #6963).
#
# WS2 moved the packages out of the support crate: they were
# `<support-crate>/assets/<ext>/`, they are now `packages/<ext>/` SIBLING to
# the support crate (PROPOSAL §5 — a package directory is self-contained and
# is not owned by any crate). So discovery still anchors on the crate name and
# then steps to its sibling, which keeps both properties: name-keyed (survives
# a family move) and following the code (survives this one).
#
# Override for tests: FIRST_PARTY_ASSETS_ROOT bypasses discovery.

set -euo pipefail
shopt -s nullglob

cd "$(dirname "$0")/.."

FIRST_PARTY_CRATE="${FIRST_PARTY_CRATE:-ironclaw_extension_support}"

# Resolve the crate that owns the first-party extension assets, at any depth.
# Failing here is deliberate: "cannot find the crate" must be an actionable
# repoint, not an empty build that looks like there was nothing to do.
resolve_first_party_assets_root() {
    if [ -n "${FIRST_PARTY_ASSETS_ROOT:-}" ]; then
        printf '%s\n' "${FIRST_PARTY_ASSETS_ROOT}"
        return 0
    fi

    local inventory match count discovery_error
    # stdout and stderr are captured separately on purpose: merging them with
    # `2>&1` would fold any Python warning into the inventory itself and turn a
    # benign diagnostic into a bogus crate directory.
    discovery_error="$(mktemp)"
    if ! inventory=$(python3 scripts/ci/lib/crate_tree.py . 2>"${discovery_error}"); then
        echo "  FAIL first-party discovery (crate discovery failed: $(cat "${discovery_error}"))" >&2
        rm -f "${discovery_error}"
        return 1
    fi
    rm -f "${discovery_error}"
    match=$(printf '%s\n' "${inventory}" | awk -F/ -v want="${FIRST_PARTY_CRATE}" '$NF == want')
    count=$(printf '%s' "${match}" | grep -c . || true)
    if [ "${count}" -ne 1 ]; then
        echo "  FAIL first-party discovery (expected exactly one crate named \
'${FIRST_PARTY_CRATE}', found ${count} — repoint FIRST_PARTY_CRATE in the same PR that \
renamed or moved it)" >&2
        return 1
    fi
    # `packages/` sits beside the support crate, not inside it.
    printf '%s/packages\n' "$(dirname "${match}")"
}

# cargo-component may be installed under ~/.cargo while Homebrew rustc wins PATH on
# developer machines. Force rustup rustc when available so installed WASI targets are
# visible to component builds.
RUSTUP_RUSTC=""
RUSTUP_TOOLCHAIN_NAME=""
if command -v rustup >/dev/null 2>&1; then
    RUSTUP_RUSTC=$(rustup which rustc 2>/dev/null || true)
    RUSTUP_TOOLCHAIN_NAME=$(rustup show active-toolchain 2>/dev/null | awk '{print $1}' || true)
    if [ -z "${RUSTC:-}" ] && [ -n "$RUSTUP_RUSTC" ]; then
        export RUSTC="$RUSTUP_RUSTC"
    fi
fi

cargo_component_build() {
    if [ -n "$RUSTUP_TOOLCHAIN_NAME" ]; then
        RUSTC="${RUSTC:-$RUSTUP_RUSTC}" rustup run "$RUSTUP_TOOLCHAIN_NAME" cargo component build "$@"
    else
        cargo component build "$@"
    fi
}

FAILED=()

case "${1:-}" in
    ""|--first-party) ;;
    *)
        echo "error: unknown argument '${1}' (only --first-party is accepted; the" \
            "v1 registry --tools/--channels legs were removed with registry/)" >&2
        exit 2
        ;;
esac

fail_build() {
    local name="$1"
    local reason="$2"

    echo "  FAIL $name ($reason)"
    FAILED+=("$name")
}

build_manifest_set() {
    local label="$1"
    local builder="$2"
    shift 2

    echo "Building $label..."
    if [ "$#" -eq 0 ]; then
        fail_build "$label" "no manifests found"
        return 0
    fi

    local manifest
    for manifest in "$@"; do
        "$builder" "$manifest"
    done
}

build_first_party_extension() {
    local manifest_path="$1"
    local extension_root
    local source_dir
    local module_path
    local runtime_kind
    local target_root
    local artifact
    local candidate
    local name

    extension_root=$(dirname "$manifest_path")
    source_dir="$extension_root/wasm-src"
    name="first-party/$(basename "$extension_root")"
    if ! runtime_kind=$(awk -F'"' '/^[[:space:]]*kind[[:space:]]*=/{ print $2; exit }' "$manifest_path"); then
        fail_build "$name" "could not read manifest"
        return 0
    fi
    if [ "$runtime_kind" != "wasm" ]; then
        echo "  SKIP $name (runtime kind $runtime_kind is host-native)"
        return 0
    fi
    if ! module_path=$(awk -F'"' '/^[[:space:]]*module[[:space:]]*=/{ print $2; exit }' "$manifest_path"); then
        fail_build "$name" "could not read manifest"
        return 0
    fi

    if [ ! -d "$source_dir" ]; then
        fail_build "$name" "source dir $source_dir not found"
        return 0
    fi
    if [ -z "$module_path" ]; then
        fail_build "$name" "manifest missing runtime module"
        return 0
    fi

    echo "  BUILD $name from $source_dir"
    if ! cargo_component_build --release --target wasm32-wasip2 --manifest-path "$source_dir/Cargo.toml" 2>&1; then
        fail_build "$name" "cargo component build failed"
        return 0
    fi
    target_root="${CARGO_TARGET_DIR:-$source_dir/target}"
    artifact=""
    for target_triple in wasm32-wasip2 wasm32-wasip1 wasm32-wasi; do
        candidate="$target_root/$target_triple/release/$(basename "$module_path")"
        if [ -f "$candidate" ]; then
            artifact="$candidate"
            break
        fi
    done
    if [ ! -f "$artifact" ]; then
        fail_build "$name" "expected artifact $(basename "$module_path") not found under $target_root"
        return 0
    fi

    if ! mkdir -p "$(dirname "$extension_root/$module_path")"; then
        fail_build "$name" "could not create output directory"
        return 0
    fi
    if ! cp "$artifact" "$extension_root/$module_path"; then
        fail_build "$name" "could not copy artifact"
        return 0
    fi
    echo "  OK   $name"
}

# `${arr[@]+"${arr[@]}"}` rather than `"${arr[@]}"`: under `set -u`, bash before
# 4.4 (macOS ships 3.2) treats an empty array expansion as an unbound variable
# and dies with a shell diagnostic instead of reaching build_manifest_set's
# "no manifests found" guard. Same outcome either way — non-zero — but only this
# form reports the actionable "no manifests found" message.
if first_party_assets=$(resolve_first_party_assets_root); then
    first_party_manifests=("${first_party_assets}"/*/manifest.toml)
else
    first_party_manifests=()
fi
build_manifest_set "first-party WASM extensions" build_first_party_extension \
    ${first_party_manifests[@]+"${first_party_manifests[@]}"}

echo ""
if [ ${#FAILED[@]} -gt 0 ]; then
    echo "FAILED: ${FAILED[*]}"
    exit 1
else
    echo "All WASM extensions built successfully."
fi
