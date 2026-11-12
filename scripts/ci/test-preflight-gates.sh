#!/usr/bin/env bash
# Self-test for scripts/preflight-gates.sh.
#
# Pins the three properties the preflight exists for:
#   1. it keeps going after a failing gate and reports EVERY failure, exiting
#      non-zero (the one-round-trip promise; CI's bucket shape does not);
#   2. an all-green run exits 0;
#   3. its self-test layer really reads code_style.yml's `Static-check
#      self-tests` step — a non-trivial command list, each line a plain
#      `<runner> <path>` command whose path exists — and refuses an empty
#      extraction instead of reporting a pass over nothing;
#   4. a self-test whose host prerequisite is missing (macOS lacks GNU
#      timeout/readlink and a tomllib python) is SKIPPED with a reason, not
#      FAILED, and not silently dropped;
#   5. the interpreter choice prefers one with `tomllib`.

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${ROOT_DIR}"

# shellcheck source=../preflight-gates.sh
source scripts/preflight-gates.sh

pass=0
fail() { echo "FAIL: $*"; exit 1; }
ok() { echo "OK: $*"; pass=$((pass + 1)); }

# 1. Two failing gates around a passing one: both failures reported, exit 1.
failures=()
list_only=false
output="$(
    run_gate "probe-fail-one" false
    run_gate "probe-pass" true
    run_gate "probe-fail-two" false
    rc=0
    verdict || rc=$?
    echo "status=${rc}"
)"
grep -q "2 gate(s) FAILED" <<<"${output}" || fail "verdict must count both failures: ${output}"
grep -q -- "- probe-fail-one" <<<"${output}" || fail "first failure missing from report"
grep -q -- "- probe-fail-two" <<<"${output}" || fail "a failure after a passing gate was not reported (fail-fast regression)"
grep -q "status=1" <<<"${output}" || fail "a failing run must exit non-zero"
ok "keeps going and reports every failing gate"

# 2. All green: exit 0.
failures=()
output="$(run_gate "probe-pass" true; rc=0; verdict || rc=$?; echo "status=${rc}")"
grep -q "every deterministic gate green" <<<"${output}" || fail "green run verdict: ${output}"
grep -q "status=0" <<<"${output}" || fail "a green run must exit 0"
ok "all-green run exits 0"

# 3. The self-test layer reads CI's list, and every entry is runnable.
commands="$(static_check_self_tests .github/workflows/code_style.yml)"
count="$(grep -c . <<<"${commands}" || true)"
[ "${count}" -ge 10 ] || fail "expected >= 10 self-test commands from code_style.yml, got ${count}"
while IFS= read -r command; do
    read -r -a words <<<"${command}"
    case "${words[0]}" in
        bash|python3) target="${words[1]}" ;;
        scripts/*|.github/*) target="${words[0]}" ;;
        *) fail "unexpected self-test command shape (preflight word-splits these): ${command}" ;;
    esac
    if [ "${target}" = "-m" ]; then
        # `python3 -m unittest pkg.module` / `python3 -m pkg.module`: the
        # last word names the module under test.
        module="${words[${#words[@]}-1]}"
        module_path="$(tr . / <<<"${module}").py"
        [ -f "${module_path}" ] || fail "self-test module ${module} has no file ${module_path}"
    else
        [ -f "${target}" ] || fail "self-test command names a missing file: ${command}"
    fi
done <<<"${commands}"
grep -q "scripts/ci/test-preflight-gates.sh" <<<"${commands}" \
    || fail "this self-test must itself be in code_style.yml's Static-check self-tests step"
ok "self-test layer reads ${count} runnable commands from code_style.yml"

# ...and an empty extraction is a failure, not a pass.
tmp="$(mktemp -d "${TMPDIR:-/tmp}/preflight-selftest.XXXXXX")"
trap 'rm -rf "${tmp}"' EXIT
printf 'jobs:\n  x:\n    steps:\n      - name: Something else\n        run: true\n' > "${tmp}/wf.yml"
[ -z "$(static_check_self_tests "${tmp}/wf.yml")" ] || fail "extraction must be empty for a workflow without the step"
failures=()
output="$(
    static_check_self_tests() { :; }
    layer_self_tests
    rc=0
    verdict || rc=$?
    echo "status=${rc}"
)"
grep -q "status=1" <<<"${output}" || fail "an empty self-test extraction must fail the run: ${output}"
ok "empty self-test extraction fails closed"

# 4. Environmental prerequisites: a self-test whose host tool is missing is
#    reported SKIPPED with its reason, the run stays green, and it is never
#    dropped silently; with the tool present it runs.
stub_bin="${tmp}/bin"
mkdir -p "${stub_bin}"
printf '#!/bin/sh\necho "timeout (BSD) 1.0"\n' > "${stub_bin}/timeout"
chmod +x "${stub_bin}/timeout"
reason="$(PATH="${stub_bin}:/usr/bin:/bin" self_test_skip_reason scripts/ci/test-install-ci-apt-packages.sh)"
[ -n "${reason}" ] || fail "a non-GNU timeout must yield a skip reason"
failures=()
skipped=()
output="$(
    PATH="${stub_bin}:/usr/bin:/bin"
    static_check_self_tests() { echo "scripts/ci/test-install-ci-apt-packages.sh"; }
    ran=()
    run_gate() { ran+=("$1"); }
    layer_self_tests
    rc=0
    verdict || rc=$?
    echo "ran=${ran[*]}"
    echo "status=${rc}"
)"
grep -q "SKIPPED: needs GNU timeout" <<<"${output}" || fail "missing prerequisite must be reported SKIPPED: ${output}"
grep -q "1 gate(s) SKIPPED" <<<"${output}" || fail "the verdict must list skipped gates: ${output}"
grep -q "status=0" <<<"${output}" || fail "an environmental skip must not fail the run: ${output}"
grep -q "ran=.*test-install-ci-apt-packages" <<<"${output}" && fail "a skipped self-test must not also run"
printf '#!/bin/sh\necho "timeout (GNU coreutils) 9.4"\n' > "${stub_bin}/timeout"
reason="$(PATH="${stub_bin}:/usr/bin:/bin" self_test_skip_reason scripts/ci/test-install-ci-apt-packages.sh)"
[ -z "${reason}" ] || fail "GNU timeout present must not skip: ${reason}"
[ -z "$(self_test_skip_reason scripts/ci/test-check-guidance.py)" ] || fail "an ordinary self-test must never skip"
ok "missing host prerequisites are reported SKIPPED, not FAILED"

# 5. Interpreter choice prefers one with tomllib, versioned names first,
#    even when a bare `python3` (macOS 3.9) comes first on PATH.
py_bin="${tmp}/py"
mkdir -p "${py_bin}"
printf '#!/bin/sh\ncase "$2" in *tomllib*) exit 1;; esac\nexit 0\n' > "${py_bin}/python3"
printf '#!/bin/sh\nexit 0\n' > "${py_bin}/python3.12"
chmod +x "${py_bin}/python3" "${py_bin}/python3.12"
chosen="$(PYTHON='' PATH="${py_bin}" select_python)"
[ "${chosen}" = "python3.12" ] || fail "select_python must prefer the tomllib interpreter, got '${chosen}'"
ok "select_python prefers an interpreter with tomllib"

# --list runs nothing and prints the gates.
listing="$(bash scripts/preflight-gates.sh --list --layer script)"
grep -q "composition mass budget" <<<"${listing}" || fail "--list must print the script gates"
grep -q "self-test: panic checker" <<<"${listing}" || fail "--list must print the self-test layer"
ok "--list prints the plan without running it"

echo "Passed: ${pass}"
