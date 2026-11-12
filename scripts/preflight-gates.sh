#!/usr/bin/env bash
# The one local pre-push command for the deterministic gates (gate audit
# 2026-08, docs/internal/gate-audit-2026-08.md §4.2–4.3). Run it before
# pushing:
#
#   bash scripts/preflight-gates.sh                # every layer
#   bash scripts/preflight-gates.sh --layer script # script gates + their self-tests only (~1 min)
#   bash scripts/preflight-gates.sh --list         # print what would run, run nothing
#
# Scope: the gate classes that produced PR #7157's six red CI runs, plus the
# self-tests that keep those gates honest —
#   1. script gates: fmt, composition budget, panic baseline, target tree,
#      guidance, include_str!, hermetic env, docs boundary;
#   2. script-gate self-tests: the `Static-check self-tests` step of
#      `.github/workflows/code_style.yml`, read from that file at run time so
#      this list cannot drift from CI's, plus the panic checker's own
#      `--self-test`;
#   3. the architecture suite (`--no-fail-fast`: every gate binary runs);
#   4. the module-charter tests of crates your diff touches.
# It deliberately runs NO ordinary unit/integration tests and no clippy —
# `.githooks/pre-push` (quality_gate.sh) remains the full CI-parity gate; this
# is the minutes-long version that catches the deterministic-gate class.
#
# Every gate runs even after an earlier one fails — the point is ONE round-trip
# reporting EVERYTHING (CI's bucket shape stops at the first failing binary) —
# and the script exits non-zero if any gate failed, listing each one.
#
# Python: the panic gate and the LOC counter need Python >= 3.10, and several
# self-tests need `tomllib` (3.11+). $PYTHON wins if set; otherwise the newest
# versioned `python3.X` on PATH that has `tomllib` (3.15 down to 3.11), then a
# bare `python3` with `tomllib`, then the newest >= 3.10 without it. The choice
# is exported so the shell gates that shell out to Python use it too.
#
# Environmental SKIPs: four self-tests depend on host tools CI's Ubuntu image
# has and a stock Mac does not. They are reported SKIPPED (with the reason),
# never FAILED and never silently dropped, when the prerequisite is absent:
#   * test-check-wasm-artifact-freshness.sh — the `python3` on PATH needs
#     `tomllib`;
#   * test-ironclaw-integration-batch-runner.sh — forces
#     PATH=<stubs>:/usr/bin:/bin, so `/usr/bin/python3` needs `tomllib`;
#   * test-install-ci-apt-packages.sh — needs GNU `timeout`;
#   * test-reborn-docker-entrypoint.sh — needs GNU `readlink -m`.
# CI is unaffected: it runs these from code_style.yml directly.
#
# Self-test: scripts/ci/test-preflight-gates.sh (wired into code_style.yml).

set -uo pipefail

# Deliberately no `-e`: the gate-running section must keep going after a
# failing gate (one round-trip, full report). The setup below therefore
# checks its own plumbing explicitly — a setup failure must never let the
# script report OK over work it didn't do.

failures=()
list_only=false

skipped=()

# Record a gate that cannot run on this host. Printed in the verdict; not a
# failure, and never silent.
skip_gate() {
    local label="$1" reason="$2"
    if [ "${list_only}" = true ]; then
        printf '%s\tSKIP (%s)\n' "${label}" "${reason}"
        return 0
    fi
    echo "==> ${label}"
    echo "    SKIPPED: ${reason}"
    skipped+=("${label}: ${reason}")
}

run_gate() {
    local label="$1"
    shift
    if [ "${list_only}" = true ]; then
        printf '%s\t%s\n' "${label}" "$*"
        return 0
    fi
    echo "==> ${label}"
    if ! "$@"; then
        failures+=("${label}")
        echo "    FAILED: ${label}"
    fi
}

# Print the verdict for everything `run_gate` saw and return its exit code.
verdict() {
    [ "${list_only}" = true ] && return 0
    echo
    if [ "${#skipped[@]}" -gt 0 ]; then
        echo "preflight-gates: ${#skipped[@]} gate(s) SKIPPED on this host (CI still runs them):"
        local s
        for s in "${skipped[@]}"; do
            echo "  - ${s}"
        done
    fi
    if [ "${#failures[@]}" -eq 0 ]; then
        echo "preflight-gates: OK — every deterministic gate green"
        return 0
    fi
    echo "preflight-gates: ${#failures[@]} gate(s) FAILED:"
    local f
    for f in "${failures[@]}"; do
        echo "  - ${f}"
    done
    return 1
}

# $PYTHON if set; else the newest interpreter with `tomllib` (3.11+),
# versioned names first; else the newest >= 3.10; else empty.
select_python() {
    if [ -n "${PYTHON:-}" ]; then
        echo "${PYTHON}"
        return
    fi
    local candidate
    for candidate in python3.15 python3.14 python3.13 python3.12 python3.11 python3; do
        if command -v "${candidate}" >/dev/null 2>&1 \
            && "${candidate}" -c 'import tomllib' 2>/dev/null; then
            echo "${candidate}"
            return
        fi
    done
    for candidate in python3.10 python3; do
        if command -v "${candidate}" >/dev/null 2>&1 \
            && "${candidate}" -c 'import sys; sys.exit(0 if sys.version_info >= (3, 10) else 1)' \
                2>/dev/null; then
            echo "${candidate}"
            return
        fi
    done
}

# Why a self-test cannot run on this host, or empty when it can. Only the four
# environmental cases in the header; anything else always runs.
self_test_skip_reason() {
    case "$1" in
        *test-check-wasm-artifact-freshness.sh*)
            python3 -c 'import tomllib' >/dev/null 2>&1 \
                || echo "the python3 on PATH has no tomllib (needs 3.11+)"
            ;;
        *test-ironclaw-integration-batch-runner.sh*)
            /usr/bin/python3 -c 'import tomllib' >/dev/null 2>&1 \
                || echo "it forces PATH=/usr/bin:/bin and /usr/bin/python3 has no tomllib (needs 3.11+)"
            ;;
        *test-install-ci-apt-packages.sh*)
            timeout --version 2>/dev/null | grep -q GNU \
                || echo "needs GNU timeout (coreutils) on PATH"
            ;;
        *test-reborn-docker-entrypoint.sh*)
            readlink -m / >/dev/null 2>&1 \
                || echo "needs GNU readlink (-m) on PATH"
            ;;
    esac
}

# The commands of code_style.yml's `Static-check self-tests` step, one per
# line, comments and blanks dropped. Empty output is a plumbing failure the
# caller must refuse — a self-test layer that extracted nothing ran nothing.
static_check_self_tests() {
    local workflow="$1"
    awk '
        /^[[:space:]]*- name: Static-check self-tests[[:space:]]*$/ { in_step = 1; next }
        in_step && /^[[:space:]]*- name:/ { exit }
        in_step && /^[[:space:]]*run: \|[[:space:]]*$/ { in_run = 1; next }
        in_run {
            line = $0
            sub(/^[[:space:]]+/, "", line)
            if (line == "" || line ~ /^#/) next
            print line
        }
    ' "${workflow}"
}

layer_script_gates() {
    run_gate "fmt check" cargo fmt --all -- --check
    run_gate "composition mass budget" bash scripts/ci/check-composition-budget.sh
    run_gate "panic baseline (full closure scan)" \
        "${PYTHON}" scripts/check_no_panics.py --reborn-baseline
    run_gate "target tree vs documented tree" "${PYTHON}" scripts/ci/check-target-tree.py
    run_gate "guidance path references" "${PYTHON}" scripts/ci/check-guidance.py
    run_gate "include_str! + Docker COPY coverage" bash scripts/ci/check-include-str-paths.sh
    run_gate "hermetic env mutation (delta)" bash scripts/ci/check-hermetic-env.sh
    run_gate "docs publication boundary" "${PYTHON}" scripts/ci/docs_publication_boundary.py
}

layer_self_tests() {
    local commands
    commands="$(static_check_self_tests .github/workflows/code_style.yml)"
    if [ -z "${commands}" ]; then
        failures+=("self-test discovery (no commands found in code_style.yml)")
        echo "    FAILED: could not read the Static-check self-tests step from code_style.yml"
        return
    fi
    run_gate "self-test: panic checker" "${PYTHON}" scripts/check_no_panics.py --self-test
    # Its own code_style.yml step, outside the Static-check self-tests block.
    run_gate "self-test: composition budget" bash scripts/ci/test-check-composition-budget.sh
    local command
    while IFS= read -r command; do
        # `python3 ...` in the workflow runs CI's python; locally use the
        # selected >= 3.10 interpreter for the same command.
        case "${command}" in
            python3\ *) command="${PYTHON} ${command#python3 }" ;;
        esac
        local reason
        reason="$(self_test_skip_reason "${command}")"
        if [ -n "${reason}" ]; then
            skip_gate "self-test: ${command}" "${reason}"
            continue
        fi
        # Word-splitting is intended: every workflow line is a plain
        # `<runner> <path> [args]` command (asserted by the self-test).
        # shellcheck disable=SC2086
        run_gate "self-test: ${command}" ${command}
    done <<<"${commands}"
}

layer_architecture() {
    if command -v cargo-nextest >/dev/null 2>&1; then
        run_gate "architecture suite (nextest)" \
            cargo nextest run -p ironclaw_architecture_tests --no-fail-fast
    else
        run_gate "architecture suite (cargo; install cargo-nextest to parallelize)" \
            cargo test -p ironclaw_architecture_tests --no-fail-fast
    fi
}

# Charter maps live inside their crates, so only changed crates pay the
# compile. Diff base mirrors the pre-push hook's default (origin/main).
# Discovery fails CLOSED: when the base is missing or the diff plumbing
# errors, the fallback widens to all five charters — a broken setup may cost
# compile time, never a silent skip.
layer_charters() {
    local all_charter_crates="crates/product/ironclaw_webui crates/product/ironclaw_assistant \
         crates/domains/ironclaw_llm crates/domains/ironclaw_auth \
         crates/lanes/ironclaw_mcp"
    local base_ref="${IRONCLAW_PREFLIGHT_BASE:-origin/main}"
    local merge_base
    changed=""
    if git rev-parse --verify --quiet "${base_ref}^{commit}" >/dev/null; then
        if merge_base="$(git merge-base HEAD "${base_ref}")" \
            && changed="$(git diff --name-only "${merge_base}" --)"; then
            # Uncommitted and untracked work counts as changed too: preflight
            # runs before the push, often before the last commit.
            changed="${changed}"$'\n'"$(git ls-files --others --exclude-standard)"
        else
            echo "==> charter tests: cannot diff against ${base_ref}; running all five"
            changed="${all_charter_crates}"
        fi
    else
        echo "==> charter tests: base ${base_ref} not found; running all five"
        changed="${all_charter_crates}"
    fi

    charter "crates/product/ironclaw_webui" ironclaw_webui handlers_module_charter \
        SKIP_FRONTEND_BUILD=1
    charter "crates/product/ironclaw_assistant" ironclaw_assistant \
        reborn_services_module_charter
    charter "crates/domains/ironclaw_llm" ironclaw_llm module_charter
    charter "crates/domains/ironclaw_auth" ironclaw_auth module_charter
    charter "crates/lanes/ironclaw_mcp" ironclaw_mcp module_charter
}

charter() {
    local crate_dir="$1" package="$2" test_name="$3"
    shift 3
    if grep -q "${crate_dir}" <<<"${changed}"; then
        run_gate "charter: ${package}" \
            env "$@" cargo test -p "${package}" --test "${test_name}"
    fi
}

main() {
    local layer="all"
    while [ "$#" -gt 0 ]; do
        case "$1" in
            --list) list_only=true ;;
            --layer) layer="${2:-}"; shift ;;
            --layer=*) layer="${1#--layer=}" ;;
            -h|--help) sed -n '2,45p' "${BASH_SOURCE[0]}"; return 0 ;;
            *) echo "preflight-gates: unknown argument '$1'" >&2; return 2 ;;
        esac
        shift
    done
    case "${layer}" in
        all|script|architecture|charters) ;;
        *) echo "preflight-gates: --layer must be all|script|architecture|charters" >&2; return 2 ;;
    esac

    local repo_root
    repo_root="$(git rev-parse --show-toplevel)" || {
        echo "preflight-gates: not inside a git repository" >&2
        return 2
    }
    cd "${repo_root}" || return 2

    PYTHON="$(select_python)"
    if [ -z "${PYTHON}" ]; then
        echo "preflight-gates: no Python >= 3.10 found (set PYTHON=...); the panic gate and" \
             "the LOC counter cannot run, so refusing to report a partial pass" >&2
        return 2
    fi
    export PYTHON

    if [ "${layer}" = all ] || [ "${layer}" = script ]; then
        layer_script_gates
        layer_self_tests
    fi
    if [ "${layer}" = all ] || [ "${layer}" = architecture ]; then
        layer_architecture
    fi
    if [ "${layer}" = all ] || [ "${layer}" = charters ]; then
        layer_charters
    fi

    verdict
}

# Sourceable for the self-test; runs only when executed.
if [ "${BASH_SOURCE[0]}" = "$0" ]; then
    main "$@"
    exit $?
fi
