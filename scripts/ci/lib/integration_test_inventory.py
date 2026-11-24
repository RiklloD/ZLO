#!/usr/bin/env python3
"""Canonical Cargo inventory for the integration-test CI topology."""

from __future__ import annotations

import argparse
import json
import pathlib
import posixpath
import re
import sys
import tomllib
from typing import Any

SCHEMA_VERSION = 1
INTEGRATION_PARTITION_COUNT = 4
INTEGRATION_PATH_PREFIX = "tests/integration/"
FLAT_NAME_PREFIXES = ("reborn_integration_", "reborn_generated_")
GROUP_NAME_PREFIX = "reborn_group_"
# Flat-scenario suite binaries: `reborn_integration_suite_<domain>` rooted at
# `tests/integration/suite_<domain>/main.rs`, mounting each member scenario file
# with `#[path = "../<member>.rs"] mod <member>;`. They run on the flat lanes;
# their member files route to the suite's lane.
SUITE_NAME_PREFIX = "reborn_integration_suite_"
INTEGRATION_SUPPORT_PREFIX = f"{INTEGRATION_PATH_PREFIX}support/"
_PATH_MOUNT = re.compile(
    r'#\[\s*path\s*=\s*"(?P<path>[^"]+)"\s*\]\s*'
    r"(?:pub(?:\([^)]*\))?\s+)?mod\s+\w+\s*;"
)

ROOT = pathlib.Path(__file__).resolve().parents[3]


def _registered_test_records(
    repo_root: str | pathlib.Path = ROOT,
) -> list[tuple[str, str]]:
    root = pathlib.Path(repo_root)
    with (root / "Cargo.toml").open("rb") as manifest:
        data = tomllib.load(manifest)
    return [
        (entry["path"], entry["name"])
        for entry in data.get("test", [])
        if isinstance(entry, dict)
        and isinstance(entry.get("name"), str)
        and isinstance(entry.get("path"), str)
    ]


def _registered_tests(repo_root: str | pathlib.Path = ROOT) -> list[tuple[str, str]]:
    return [
        (path, name)
        for path, name in _registered_test_records(repo_root)
        if path.startswith(INTEGRATION_PATH_PREFIX)
    ]


def _strip_line_comments(source: str) -> str:
    return "\n".join(line.split("//", 1)[0] for line in source.splitlines())


def suite_member_paths(
    repo_root: str | pathlib.Path, suite_path: str
) -> list[str]:
    """Return the scenario files a suite root mounts, repo-relative, sorted.

    Members are the `#[path]`-mounted modules that resolve inside
    `tests/integration/` but outside its shared `support/` tree. A suite that
    mounts nothing is a broken registration, not an empty one.
    """

    expected_prefix = f"{INTEGRATION_PATH_PREFIX}suite_"
    if not (suite_path.startswith(expected_prefix) and suite_path.endswith("/main.rs")):
        raise ValueError(
            f"integration suite root must be {expected_prefix}<domain>/main.rs, "
            f"found {suite_path!r}"
        )
    main = pathlib.Path(repo_root) / suite_path
    try:
        source = main.read_text(encoding="utf-8")
    except OSError as error:
        raise ValueError(f"integration suite root {suite_path!r} is unreadable: {error}")
    members = set()
    suite_directory = posixpath.dirname(suite_path)
    for match in _PATH_MOUNT.finditer(_strip_line_comments(source)):
        resolved = posixpath.normpath(posixpath.join(suite_directory, match["path"]))
        if not resolved.startswith(INTEGRATION_PATH_PREFIX) or resolved.startswith(
            INTEGRATION_SUPPORT_PREFIX
        ):
            continue
        if not (pathlib.Path(repo_root) / resolved).is_file():
            raise ValueError(
                f"integration suite {suite_path!r} mounts missing file {resolved!r}"
            )
        members.add(resolved)
    if not members:
        raise ValueError(f"integration suite {suite_path!r} mounts no scenario files")
    return sorted(members)


def cargo_test_names(repo_root: str | pathlib.Path = ROOT) -> list[str]:
    """Return the shell selector's existing sorted-unique name projection."""

    return sorted({name for _, name in _registered_tests(repo_root)})


def _planner_registrations(
    repo_root: str | pathlib.Path = ROOT,
) -> dict[str, str]:
    """Return the planner's existing last-registration-per-path projection."""

    return {path: name for path, name in _registered_tests(repo_root)}


def planner_test_lanes(
    repo_root: str | pathlib.Path = ROOT,
) -> dict[str, str | int]:
    """Map registered integration paths to their current planner lanes."""

    tests = _planner_registrations(repo_root)
    for name in tests.values():
        if not (
            name.startswith(FLAT_NAME_PREFIXES)
            or name.startswith(GROUP_NAME_PREFIX)
        ):
            raise ValueError(f"unsupported integration test name: {name!r}")
    flat_names = sorted(
        name for name in tests.values() if name.startswith(FLAT_NAME_PREFIXES)
    )
    flat_lanes = {
        name: index % INTEGRATION_PARTITION_COUNT
        for index, name in enumerate(flat_names)
    }
    lanes: dict[str, str | int] = {
        path: "groups" if name.startswith(GROUP_NAME_PREFIX) else flat_lanes[name]
        for path, name in tests.items()
    }
    # A suite member is compiled and run only through its suite, so a change to
    # the member file selects the suite's lane exactly as a change to a
    # standalone registration selects its own.
    member_owners: dict[str, str] = {}
    for path, name in tests.items():
        if not name.startswith(SUITE_NAME_PREFIX):
            continue
        expected = (
            f"{INTEGRATION_PATH_PREFIX}suite_"
            f"{name.removeprefix(SUITE_NAME_PREFIX)}/main.rs"
        )
        if path != expected:
            raise ValueError(
                f"suite registration path mismatch for {name!r}: "
                f"expected {expected!r}, found {path!r}"
            )
        for member in suite_member_paths(repo_root, path):
            if member in tests:
                raise ValueError(
                    f"{member!r} is mounted by suite {name!r} and also registered "
                    f"as its own target {tests[member]!r}"
                )
            if member in member_owners:
                raise ValueError(
                    f"{member!r} is mounted by both {member_owners[member]!r} "
                    f"and {name!r}"
                )
            member_owners[member] = name
            lanes[member] = lanes[path]
    orphans = _orphan_scenario_files(repo_root, lanes)
    if orphans:
        raise ValueError(
            "integration scenario file(s) compiled by no target — mount each in a "
            "tests/integration/suite_*/main.rs or register it: " + ", ".join(orphans)
        )
    return lanes


def _orphan_scenario_files(
    repo_root: str | pathlib.Path, lanes: dict[str, str | int]
) -> list[str]:
    """Rust files under `tests/integration/` that no target compiles.

    Group directories own their scenario modules and `support/` is shared
    plumbing; every other `.rs` file must be a registered target root or a
    suite member, or it would silently never build or run.
    """

    root = pathlib.Path(repo_root)
    integration_root = root / INTEGRATION_PATH_PREFIX
    if not integration_root.is_dir():
        return []
    orphans = []
    for source in sorted(integration_root.rglob("*.rs")):
        relative = source.relative_to(root).as_posix()
        top = relative.removeprefix(INTEGRATION_PATH_PREFIX).split("/", 1)[0]
        if relative.startswith(INTEGRATION_SUPPORT_PREFIX) or top.startswith("group_"):
            continue
        if relative not in lanes:
            orphans.append(relative)
    return orphans


def inventory_document(repo_root: str | pathlib.Path = ROOT) -> dict[str, Any]:
    """Return the versioned normalized inventory consumed by later slices."""

    registrations_by_name = {
        name: path for path, name in _registered_tests(repo_root)
    }
    lanes = planner_test_lanes(repo_root)
    document: dict[str, Any] = {
        "schema_version": SCHEMA_VERSION,
        "partition_count": INTEGRATION_PARTITION_COUNT,
        "tests": [
            {
                "path": path,
                "name": name,
                "kind": "group" if name.startswith(GROUP_NAME_PREFIX) else "flat",
                "lane": lanes[path],
            }
            for name, path in sorted(registrations_by_name.items())
        ],
    }
    return validate_inventory_document(document)


def _required_integer(document: dict[str, Any], field: str, expected: int) -> int:
    value = document.get(field)
    if type(value) is not int or value != expected:
        raise ValueError(f"integration inventory {field} must be {expected}")
    return value


def validate_inventory_document(document: Any) -> dict[str, Any]:
    """Validate the generated cross-language inventory contract fail-closed."""

    if not isinstance(document, dict):
        raise ValueError("integration inventory must be an object")
    _required_integer(document, "schema_version", SCHEMA_VERSION)
    partition_count = _required_integer(
        document, "partition_count", INTEGRATION_PARTITION_COUNT
    )
    tests = document.get("tests")
    if not isinstance(tests, list):
        raise ValueError("integration inventory tests must be an array")
    for test in tests:
        if not isinstance(test, dict):
            raise ValueError("integration inventory test records must be objects")
        path = test.get("path")
        name = test.get("name")
        kind = test.get("kind")
        lane = test.get("lane")
        if not isinstance(path, str) or not path.startswith(INTEGRATION_PATH_PREFIX):
            raise ValueError("integration inventory test path is invalid")
        if not isinstance(name, str):
            raise ValueError("integration inventory test name is invalid")
        if kind == "group":
            if not name.startswith(GROUP_NAME_PREFIX) or lane != "groups":
                raise ValueError("integration inventory group record is invalid")
        elif kind == "flat":
            if (
                not name.startswith(FLAT_NAME_PREFIXES)
                or not isinstance(lane, int)
                or isinstance(lane, bool)
                or lane < 0
                or lane >= partition_count
            ):
                raise ValueError("integration inventory flat record is invalid")
        else:
            raise ValueError("integration inventory test kind is invalid")
    return document


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    output = parser.add_mutually_exclusive_group()
    output.add_argument("--json", action="store_true", help="print versioned JSON")
    output.add_argument(
        "--validate-group-topology",
        action="store_true",
        help="validate integration group registrations and entry points",
    )
    parser.add_argument("repo_root", nargs="?", default=str(ROOT))
    args = parser.parse_args(argv)

    if args.validate_group_topology:
        try:
            root = pathlib.Path(args.repo_root)
            registered_names: set[str] = set()
            for path, name in _registered_test_records(root):
                if not name.startswith(GROUP_NAME_PREFIX):
                    continue
                suffix = name.removeprefix(GROUP_NAME_PREFIX)
                expected_path = f"{INTEGRATION_PATH_PREFIX}group_{suffix}/main.rs"
                if path != expected_path:
                    raise ValueError(
                        f"group registration path mismatch for {name!r}: "
                        f"expected {expected_path!r}, found {path!r}"
                    )
                registered_names.add(name)

            integration_root = root / INTEGRATION_PATH_PREFIX
            group_directories = sorted(
                entry
                for entry in integration_root.glob("group_*")
                if entry.is_dir()
            )
            incomplete = [
                entry.name for entry in group_directories if not (entry / "main.rs").is_file()
            ]
            if incomplete:
                raise ValueError(
                    "integration test group directory missing main.rs: "
                    + ", ".join(incomplete)
                )
            filesystem_names = {
                f"{GROUP_NAME_PREFIX}{entry.name.removeprefix('group_')}"
                for entry in group_directories
            }
            unregistered = sorted(filesystem_names - registered_names)
            if unregistered:
                raise ValueError(
                    "unregistered integration test group(s): "
                    + ", ".join(unregistered)
                )
            missing = sorted(registered_names - filesystem_names)
            if missing:
                raise ValueError(
                    "registered integration test group(s) missing main.rs: "
                    + ", ".join(missing)
                )
            if not group_directories:
                raise ValueError("No integration test group directories found")
        except (OSError, tomllib.TOMLDecodeError, ValueError) as error:
            print(f"integration group topology error: {error}", file=sys.stderr)
            return 1
        return 0

    if args.json:
        print(json.dumps(inventory_document(args.repo_root), sort_keys=True))
        return 0

    names = cargo_test_names(args.repo_root)
    if not names:
        print("No Reborn integration-tier test binaries discovered", file=sys.stderr)
        return 1
    for name in names:
        print("--test")
        print(name)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
