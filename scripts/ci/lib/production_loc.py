#!/usr/bin/env python3
"""Count production Rust lines: test-context lines excluded, test files excluded.

The LOC ceilings (``scripts/ci/check-composition-budget.sh`` and the
contracts size ceiling in ``reborn_dependency_boundaries.rs``) used to count
every line of every non-test-*named* ``.rs`` file. Inline ``#[cfg(test)]``
modules therefore counted as production mass (25-41% of each governed crate,
gate audit 2026-08 §3.2), so moving a test module into its own file "fixed" a
ceiling trip while changing no behavior — test relocation minted headroom. A
file named ``*_tests.rs`` was excluded by basename even when nothing gated it
behind ``cfg(test)``, so production code could hide there (§6.2).

This helper applies the panic gate's definition instead
(``scripts/check_no_panics.py`` — one self-tested lexer and test-context
tracker, not a second one):

* a file is test-only when ``is_test_only_path`` says so — under a ``tests``
  directory, or a ``tests.rs`` / ``*_test.rs`` / ``*_tests.rs`` whose real
  parent module declares it (lexed code, comments and strings excluded) only
  behind a test-only cfg, or the feature-gated ``src/test_support`` root;
* inside every other file, a line is production unless the lexer places it in
  a test context: an item under a test-only cfg (``#[cfg(test)]``,
  ``#[cfg(all(test, ...))]``, ``feature = "test-support"``) or a ``#[test]``
  function. A module merely *named* ``tests`` is production unless gated —
  ``pub mod tests { ... }`` without a cfg ships in release builds.

Comment and blank lines inside production context still count, matching the
``wc -l`` semantics the ceilings were seeded with; only *where* a line lives
decides whether it counts, so relocating a test module is LOC-neutral.

Usage::

    production_loc.py loc [--exclude REGEX] PATH...      # production line count
    production_loc.py arc-dyn [--exclude REGEX] PATH...  # `Arc<dyn` sites in production code
    production_loc.py --self-test

``--exclude`` drops files whose POSIX path matches REGEX (``re.search``).

PATHs are files or directories (walked for ``*.rs``, skipping ``target``).
Paths are resolved relative to the current directory, which must be the
repository root (``is_test_only_path`` reads sibling module declarations
relative to it). Exit 1 on an unreadable file — a counter that skips what it
cannot read reports a smaller crate than exists.
"""

from __future__ import annotations

import os
import pathlib
import re
import sys
import tempfile
import unittest

REPO_ROOT = pathlib.Path(__file__).resolve().parents[3]
sys.path.insert(0, str(REPO_ROOT / "scripts"))

import check_no_panics as panics  # noqa: E402


def rust_files(paths: list[str], exclude: str | None = None) -> list[pathlib.Path]:
    files = _rust_files(paths)
    if exclude is None:
        return files
    pattern = re.compile(exclude)
    return [path for path in files if not pattern.search(path.as_posix())]


def _rust_files(paths: list[str]) -> list[pathlib.Path]:
    files: list[pathlib.Path] = []
    for raw in paths:
        path = pathlib.Path(raw)
        if path.is_file():
            if path.suffix == ".rs":
                files.append(path)
            continue
        if not path.is_dir():
            # A missing directory is a valid zero (the shell gate checks the
            # numerator is non-zero itself); a missing *file* argument is not.
            continue
        for dirpath, dirnames, filenames in os.walk(path):
            dirnames[:] = sorted(d for d in dirnames if d != "target")
            for name in sorted(filenames):
                if name.endswith(".rs"):
                    files.append(pathlib.Path(dirpath) / name)
    return files


def production_lines(path: pathlib.Path) -> list[str]:
    """The lexed code of every production-context line of ``path``."""
    relative = path.as_posix()
    if panics.is_test_only_path(relative):
        return []
    lines = path.read_text(encoding="utf-8").splitlines()
    code, _comments = panics.lex_lines(lines)
    contexts = panics.line_test_contexts_from_code(code, lines)
    kept = []
    for raw, code_line, in_test in zip(lines, code, contexts):
        if in_test:
            continue
        # The tracker marks the gated item, not the gating attribute line
        # before it; the `#[cfg(test)]` / `#[test]` line belongs to the test.
        if code_line.strip().startswith("#[") and panics.attribute_is_test_only(raw.strip()):
            continue
        kept.append(code_line)
    return kept


def count_loc(paths: list[str], exclude: str | None = None) -> int:
    return sum(len(production_lines(path)) for path in rust_files(paths, exclude))


def count_arc_dyn(paths: list[str], exclude: str | None = None) -> int:
    # Lexed code, so `Arc<dyn` in a comment or string literal is not a site.
    return sum(
        line.count("Arc<dyn")
        for path in rust_files(paths, exclude)
        for line in production_lines(path)
    )


class ProductionLocTests(unittest.TestCase):
    def setUp(self) -> None:
        self._previous = pathlib.Path.cwd()
        self._temp = tempfile.TemporaryDirectory()
        os.chdir(self._temp.name)
        pathlib.Path("src").mkdir()

    def tearDown(self) -> None:
        os.chdir(self._previous)
        self._temp.cleanup()

    def write(self, relative: str, text: str) -> None:
        path = pathlib.Path(relative)
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")

    def test_inline_test_module_does_not_count(self) -> None:
        self.write(
            "src/lib.rs",
            "pub fn a() {}\n"
            "\n"
            "#[cfg(test)]\n"
            "mod tests {\n"
            "    #[test]\n"
            "    fn t() { assert!(true); }\n"
            "}\n",
        )
        self.assertEqual(count_loc(["src"]), 2)

    def test_relocating_a_test_module_is_loc_neutral(self) -> None:
        # The relocation-minting class: the same production code with its
        # tests inline, then split to `tests.rs`, must measure the same.
        inline = (
            "pub fn a() -> u8 { 1 }\n"
            "#[cfg(test)]\n"
            "mod tests {\n"
            "    #[test]\n"
            "    fn t() { assert_eq!(super::a(), 1); }\n"
            "}\n"
        )
        self.write("src/lib.rs", inline)
        before = count_loc(["src"])
        self.write("src/lib.rs", "pub fn a() -> u8 { 1 }\n#[cfg(test)]\nmod tests;\n")
        self.write("src/tests.rs", "#[test]\nfn t() { assert_eq!(super::a(), 1); }\n")
        after = count_loc(["src"])
        # The bodiless declaration line is itself test context, so both
        # spellings measure exactly the one production line.
        self.assertEqual(before, 1)
        self.assertEqual(after, 1)

    def test_ungated_tests_named_file_counts_as_production(self) -> None:
        # The basename fail-open: `*_tests.rs` that nothing gates behind
        # `cfg(test)` is production code and must count.
        self.write("src/lib.rs", "pub mod probe_tests;\n")
        self.write("src/probe_tests.rs", "pub fn shipped() {}\npub fn also() {}\n")
        self.assertEqual(count_loc(["src"]), 3)
        # ...while a gated one does not.
        self.write("src/lib.rs", "#[cfg(test)]\nmod probe_tests;\n")
        self.assertEqual(count_loc(["src"]), 0)

    def test_ungated_inline_tests_module_counts_as_production(self) -> None:
        # Gate audit follow-up: `mod tests` used to be test context by NAME.
        self.write(
            "src/lib.rs",
            "pub mod tests {\n    pub fn shipped(_: Arc<dyn A>) {}\n}\n",
        )
        self.assertEqual(count_loc(["src"]), 3)
        self.assertEqual(count_arc_dyn(["src"]), 1)
        self.write(
            "src/lib.rs",
            "#[cfg(all(test, unix))]\nmod tests {\n    fn t(_: Arc<dyn A>) {}\n}\n",
        )
        self.assertEqual(count_loc(["src"]), 0)
        self.assertEqual(count_arc_dyn(["src"]), 0)

    def test_commented_declaration_does_not_gate_a_tests_file(self) -> None:
        # Gate audit follow-up: a comment matching the declaration regex hid an
        # ungated `*_tests.rs` from the count.
        self.write(
            "src/lib.rs",
            "pub mod probe_tests;\n// #[cfg(test)] mod probe_tests;\n",
        )
        self.write("src/probe_tests.rs", "pub fn shipped() {}\n")
        self.assertEqual(count_loc(["src"]), 3)

    def test_arc_dyn_counts_production_code_only(self) -> None:
        self.write(
            "src/lib.rs",
            "pub struct S { a: Arc<dyn A>, b: Arc<dyn B> }\n"
            "// Arc<dyn Commented>\n"
            "const DOC: &str = \"Arc<dyn Quoted>\";\n"
            "#[cfg(test)]\n"
            "mod tests { fn f(_: Arc<dyn T>) {} }\n",
        )
        self.assertEqual(count_arc_dyn(["src"]), 2)

    def test_missing_directory_is_zero_not_error(self) -> None:
        self.assertEqual(count_loc(["does-not-exist"]), 0)

    def test_exclude_drops_matching_paths(self) -> None:
        self.write("src/lib.rs", "pub fn a(_: Arc<dyn A>) {}\n")
        self.write("src/slack/mod.rs", "pub fn b(_: Arc<dyn B>) {}\n")
        self.assertEqual(count_arc_dyn(["src"]), 2)
        self.assertEqual(count_arc_dyn(["src"], "/(slack|telegram)/"), 1)
        self.assertEqual(count_loc(["src"], "/(slack|telegram)/"), 1)


def main(argv: list[str]) -> int:
    if argv[:1] == ["--self-test"]:
        suite = unittest.defaultTestLoader.loadTestsFromTestCase(ProductionLocTests)
        result = unittest.TextTestRunner(verbosity=1).run(suite)
        return 0 if result.wasSuccessful() else 1
    if len(argv) < 2 or argv[0] not in {"loc", "arc-dyn"}:
        print(__doc__, file=sys.stderr)
        return 2
    command, rest = argv[0], argv[1:]
    exclude = None
    if rest[:1] == ["--exclude"]:
        if len(rest) < 2:
            print("production_loc: --exclude needs a REGEX", file=sys.stderr)
            return 2
        exclude, rest = rest[1], rest[2:]
    try:
        if command == "loc":
            print(count_loc(rest, exclude))
        else:
            print(count_arc_dyn(rest, exclude))
    except (OSError, UnicodeError) as error:
        print(f"production_loc: cannot read a source file: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
