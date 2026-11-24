"""Contract tests for the canonical integration-test inventory."""

import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts/ci/lib"))

import integration_test_inventory as inventory  # noqa: E402


class IntegrationTestInventoryTests(unittest.TestCase):
    def run_group_validator(
        self, root: Path, registrations: list[tuple[str, str]]
    ) -> subprocess.CompletedProcess[str]:
        manifest = "".join(
            f'[[test]]\nname = "{name}"\npath = "{path}"\n\n'
            for name, path in registrations
        )
        (root / "Cargo.toml").write_text(manifest, encoding="utf-8")

        return subprocess.run(
            [
                sys.executable,
                str(ROOT / "scripts/ci/lib/integration_test_inventory.py"),
                "--validate-group-topology",
                str(root),
            ],
            cwd=root,
            capture_output=True,
            text=True,
            check=False,
        )

    def test_preserves_current_registration_projections(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "Cargo.toml").write_text(
                """
test = [
  { name = "reborn_integration_before", path = "tests/integration/duplicate.rs" },
  { name = 7, path = "tests/integration/ignored_name.rs" },
  { name = "ignored_path", path = 7 },
  { name = "outside_scope", path = "tests/other.rs" },
  { name = "reborn_group_shared", path = "tests/integration/group_shared/main.rs" },
  { name = "reborn_integration_after", path = "tests/integration/duplicate.rs" },
  { name = "reborn_integration_after", path = "tests/integration/second.rs" },
]
""",
                encoding="utf-8",
            )

            self.assertEqual(
                inventory.cargo_test_names(root),
                ["reborn_group_shared", "reborn_integration_after", "reborn_integration_before"],
            )
            self.assertEqual(
                inventory.planner_test_lanes(root),
                {
                    "tests/integration/duplicate.rs": 1,
                    "tests/integration/group_shared/main.rs": "groups",
                    "tests/integration/second.rs": 1,
                },
            )
            document = inventory.inventory_document(root)
            self.assertEqual(
                [test["name"] for test in document["tests"]],
                [
                    "reborn_group_shared",
                    "reborn_integration_after",
                    "reborn_integration_before",
                ],
                "execution inventory must preserve every unique Cargo target name",
            )
            self.assertEqual(
                {test["name"]: test["lane"] for test in document["tests"]},
                {
                    "reborn_group_shared": "groups",
                    "reborn_integration_after": 1,
                    "reborn_integration_before": 1,
                },
                "targets sharing a source path must share that path's planner lane",
            )

            (root / "Cargo.toml").write_text(
                'test = [{ name = "new_test", path = "tests/integration/new_test.rs" }]\n',
                encoding="utf-8",
            )
            self.assertEqual(inventory.cargo_test_names(root), ["new_test"])
            for projection in (inventory.planner_test_lanes, inventory.inventory_document):
                with self.subTest(projection=projection.__name__):
                    with self.assertRaisesRegex(ValueError, "unsupported.*new_test"):
                        projection(root)

    def write_suite_repo(
        self,
        root: Path,
        registrations: list[tuple[str, str]],
        files: dict[str, str],
    ) -> None:
        (root / "Cargo.toml").write_text(
            "".join(
                f'[[test]]\nname = "{name}"\npath = "{path}"\n\n'
                for name, path in registrations
            ),
            encoding="utf-8",
        )
        for relative, text in files.items():
            path = root / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(text, encoding="utf-8")

    def test_suite_members_route_to_their_suite_lane(self) -> None:
        suite_main = (
            '#[allow(dead_code)]\n#[path = "../support/mod.rs"]\nmod reborn_support;\n'
            '#[allow(dead_code)]\n#[path = "../../support/mod.rs"]\nmod support;\n'
            '#[path = "../alpha.rs"]\nmod alpha;\n'
            '#[path = "../auth/beta.rs"]\nmod beta;\n'
            '// #[path = "../commented_out.rs"] mod commented_out;\n'
        )
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            self.write_suite_repo(
                root,
                [
                    ("reborn_generated_first", "tests/integration/first.rs"),
                    (
                        "reborn_integration_suite_core",
                        "tests/integration/suite_core/main.rs",
                    ),
                ],
                {
                    "tests/integration/first.rs": "",
                    "tests/integration/suite_core/main.rs": suite_main,
                    "tests/integration/alpha.rs": "",
                    "tests/integration/auth/beta.rs": "",
                    "tests/integration/support/mod.rs": "",
                },
            )

            self.assertEqual(
                inventory.suite_member_paths(
                    root, "tests/integration/suite_core/main.rs"
                ),
                ["tests/integration/alpha.rs", "tests/integration/auth/beta.rs"],
            )
            self.assertEqual(
                inventory.planner_test_lanes(root),
                {
                    "tests/integration/first.rs": 0,
                    "tests/integration/suite_core/main.rs": 1,
                    "tests/integration/alpha.rs": 1,
                    "tests/integration/auth/beta.rs": 1,
                },
            )
            self.assertEqual(
                [test["name"] for test in inventory.inventory_document(root)["tests"]],
                ["reborn_generated_first", "reborn_integration_suite_core"],
                "suite members are modules, not execution targets",
            )

    def test_suite_topology_fails_closed(self) -> None:
        suite = ("reborn_integration_suite_core", "tests/integration/suite_core/main.rs")
        mount_alpha = '#[path = "../alpha.rs"]\nmod alpha;\n'
        cases = (
            (
                [("reborn_integration_suite_core", "tests/integration/core.rs")],
                {"tests/integration/core.rs": mount_alpha, "tests/integration/alpha.rs": ""},
                "suite registration path mismatch",
            ),
            ([suite], {"tests/integration/suite_core/main.rs": mount_alpha}, "mounts missing file"),
            (
                [suite],
                {"tests/integration/suite_core/main.rs": '#[path = "../support/mod.rs"]\nmod s;\n'},
                "mounts no scenario files",
            ),
            (
                [suite, ("reborn_integration_alpha", "tests/integration/alpha.rs")],
                {"tests/integration/suite_core/main.rs": mount_alpha, "tests/integration/alpha.rs": ""},
                "also registered as its own target",
            ),
            (
                [suite, ("reborn_integration_suite_more", "tests/integration/suite_more/main.rs")],
                {
                    "tests/integration/suite_core/main.rs": mount_alpha,
                    "tests/integration/suite_more/main.rs": mount_alpha,
                    "tests/integration/alpha.rs": "",
                },
                "mounted by both",
            ),
            (
                [suite],
                {
                    "tests/integration/suite_core/main.rs": mount_alpha,
                    "tests/integration/alpha.rs": "",
                    "tests/integration/forgotten.rs": "",
                },
                "compiled by no target.*tests/integration/forgotten.rs",
            ),
            (
                [suite],
                {
                    "tests/integration/suite_core/main.rs": mount_alpha,
                    "tests/integration/alpha.rs": "",
                    "tests/integration/suite_unregistered/main.rs": mount_alpha,
                },
                "compiled by no target.*suite_unregistered/main.rs",
            ),
        )
        for registrations, files, error in cases:
            with self.subTest(error=error), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                self.write_suite_repo(root, registrations, files)
                with self.assertRaisesRegex(ValueError, error):
                    inventory.planner_test_lanes(root)

    def test_live_suites_cover_their_member_files(self) -> None:
        lanes = inventory.planner_test_lanes(ROOT)
        suites = {
            path: name
            for path, name in inventory._planner_registrations(ROOT).items()
            if name.startswith(inventory.SUITE_NAME_PREFIX)
        }
        self.assertTrue(suites, "the live tree registers no suite binaries")
        for path in suites:
            for member in inventory.suite_member_paths(ROOT, path):
                with self.subTest(member=member):
                    self.assertEqual(lanes[member], lanes[path])

    def test_document_is_versioned_and_self_validating(self) -> None:
        document = inventory.inventory_document(ROOT)

        self.assertEqual(inventory.INTEGRATION_PARTITION_COUNT, 4)
        self.assertEqual(document["schema_version"], 1)
        self.assertEqual(
            document["partition_count"], inventory.INTEGRATION_PARTITION_COUNT
        )
        self.assertEqual(inventory.validate_inventory_document(document), document)

        invalid_fields = (
            ("schema_version", True),
            ("schema_version", 1.0),
            ("partition_count", 0),
            ("partition_count", 4.0),
        )
        for field, value in invalid_fields:
            malformed = dict(document)
            malformed[field] = value
            with self.subTest(field=field, value=value):
                with self.assertRaisesRegex(ValueError, field):
                    inventory.validate_inventory_document(malformed)

    def test_live_repository_group_topology_is_valid(self) -> None:
        completed = subprocess.run(
            [
                sys.executable,
                str(ROOT / "scripts/ci/lib/integration_test_inventory.py"),
                "--validate-group-topology",
                str(ROOT),
            ],
            cwd=ROOT,
            capture_output=True,
            text=True,
            check=False,
        )

        self.assertEqual(
            completed.returncode,
            0,
            "live integration group topology validation failed:\n"
            f"stdout:\n{completed.stdout}\n"
            f"stderr:\n{completed.stderr}",
        )

    def test_group_runner_rejects_invalid_topology_before_execution(self) -> None:
        cases = (
            ([], (), "No integration test group directories"),
            ([], ("group_orphan/main.rs",), "unregistered integration test group"),
            ([], ("group_incomplete/scenario.rs",), "missing main.rs"),
            (
                [("reborn_group_declared", "tests/integration/group_actual/main.rs")],
                ("group_actual/main.rs",),
                "group registration path mismatch",
            ),
            (
                [("reborn_group_missing", "tests/integration/group_missing/main.rs")],
                (),
                "missing main.rs",
            ),
            (
                [
                    ("reborn_group_valid", "tests/integration/group_valid/main.rs"),
                    ("reborn_group_outside", "tests/other.rs"),
                ],
                ("group_valid/main.rs",),
                "group registration path mismatch",
            ),
        )
        for registrations, group_entries, error in cases:
            with self.subTest(error=error), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                for entry in group_entries:
                    group_file = root / "tests/integration" / entry
                    group_file.parent.mkdir(parents=True)
                    group_file.touch()

                completed = self.run_group_validator(root, registrations)

                self.assertNotEqual(completed.returncode, 0)
                self.assertIn(error, completed.stderr)

    def test_group_topology_accepts_sorted_registered_directories(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            for suffix in ("zeta", "alpha"):
                group = root / f"tests/integration/group_{suffix}"
                group.mkdir(parents=True)
                (group / "main.rs").touch()

            completed = self.run_group_validator(
                root,
                [
                    ("reborn_group_zeta", "tests/integration/group_zeta/main.rs"),
                    ("reborn_group_alpha", "tests/integration/group_alpha/main.rs"),
                ],
            )

            self.assertEqual(completed.returncode, 0, completed.stderr)


if __name__ == "__main__":
    unittest.main()
