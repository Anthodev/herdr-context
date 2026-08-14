from __future__ import annotations

import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
MODULE_PATH = ROOT / "scripts" / "release.py"
SPEC = importlib.util.spec_from_file_location("herdr_context_release", MODULE_PATH)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError(f"cannot load {MODULE_PATH}")
release = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(release)


class ReleaseContractTests(unittest.TestCase):
    def test_repository_contract_is_release_ready(self) -> None:
        contract = release.validate_repository(ROOT)

        self.assertEqual(contract.version, "0.11.0")
        self.assertEqual(contract.binary_name, "herdr-context")
        self.assertEqual(contract.min_herdr_version, "0.8.0")
        self.assertEqual(len(contract.performance_metrics), 12)

    def test_trigger_tag_must_exactly_match_cargo_version(self) -> None:
        contract = release.validate_repository(ROOT)

        release.validate_trigger_tag(contract, "v0.11.0")
        with self.assertRaisesRegex(release.ReleaseError, "exactly v0.11.0"):
            release.validate_trigger_tag(contract, "vtest")

    def test_failed_budget_requires_complete_risk_acceptance(self) -> None:
        baseline = self._baseline(passed=False)
        review = self._review("risk-accepted")
        del review["budgets"][0]["accepted_risk"]["authority"]

        with self.assertRaisesRegex(release.ReleaseError, "authority"):
            release.validate_performance_review(
                json.dumps(baseline).encode(), review, expected_digest=None
            )

    def test_complete_risk_acceptance_allows_failed_budget(self) -> None:
        baseline = self._baseline(passed=False)
        review = self._review("risk-accepted")

        metrics = release.validate_performance_review(
            json.dumps(baseline).encode(), review, expected_digest=None
        )

        self.assertEqual(metrics, ("first_frame_p95_ms",))

    def test_package_is_deterministic_minimal_and_checksummed(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            binary = root / "herdr-context"
            binary.write_bytes(b"deterministic release binary\n")
            binary.chmod(0o755)
            first = root / "first"
            second = root / "second"

            first_archive, first_checksum = release.create_package(
                ROOT,
                binary,
                "x86_64-unknown-linux-gnu",
                first,
                allow_untagged=True,
            )
            second_archive, second_checksum = release.create_package(
                ROOT,
                binary,
                "x86_64-unknown-linux-gnu",
                second,
                allow_untagged=True,
            )

            self.assertEqual(first_archive.read_bytes(), second_archive.read_bytes())
            self.assertEqual(
                first_checksum.read_text().split()[0],
                second_checksum.read_text().split()[0],
            )
            metadata = release.verify_archive(first_archive, first_checksum)
            archive_root = metadata.archive_root
            self.assertEqual(
                set(metadata.members),
                {
                    f"{archive_root}/LICENSE",
                    f"{archive_root}/README.md",
                    f"{archive_root}/herdr-context",
                    f"{archive_root}/herdr-plugin.toml",
                    f"{archive_root}/install.sh",
                    f"{archive_root}/uninstall.sh",
                },
            )
            self.assertNotIn(str(Path.home()).encode(), first_archive.read_bytes())

    def test_corrupt_archive_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            binary = root / "herdr-context"
            binary.write_bytes(b"release binary\n")
            binary.chmod(0o755)
            archive, checksum = release.create_package(
                ROOT,
                binary,
                "x86_64-unknown-linux-gnu",
                root / "dist",
                allow_untagged=True,
            )
            data = bytearray(archive.read_bytes())
            data[len(data) // 2] ^= 0x01
            archive.write_bytes(data)

            with self.assertRaisesRegex(release.ReleaseError, "checksum"):
                release.verify_archive(archive, checksum)

    def test_uncompressed_member_limit_is_enforced_before_reading(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            binary = root / "herdr-context"
            binary.write_bytes(b"release binary\n")
            binary.chmod(0o755)
            archive, checksum = release.create_package(
                ROOT,
                binary,
                "x86_64-unknown-linux-gnu",
                root / "dist",
                allow_untagged=True,
            )
            original_limit = release.MAX_PACKAGE_MEMBER_BYTES["herdr-context"]
            release.MAX_PACKAGE_MEMBER_BYTES["herdr-context"] = 1
            try:
                with self.assertRaisesRegex(release.ReleaseError, "uncompressed limit"):
                    release.verify_archive(archive, checksum)
            finally:
                release.MAX_PACKAGE_MEMBER_BYTES["herdr-context"] = original_limit

    def test_install_upgrade_and_uninstall_touch_only_owned_files(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            fake_herdr = root / "herdr"
            fake_log = root / "herdr.log"
            fake_count = root / "herdr-link-count"
            fake_herdr.write_text(
                "#!/bin/sh\n"
                "printf '%s\\n' \"$*\" >> \"$HERDR_FAKE_LOG\"\n"
                "if [ \"$1 $2\" = \"plugin link\" ]; then\n"
                "  count=0\n"
                "  if [ -f \"$HERDR_FAKE_COUNT\" ]; then count=$(cat \"$HERDR_FAKE_COUNT\"); fi\n"
                "  count=$((count + 1))\n"
                "  printf '%s\\n' \"$count\" > \"$HERDR_FAKE_COUNT\"\n"
                "  if [ \"${HERDR_FAIL_LINK_CALL:-}\" = \"$count\" ]; then exit 1; fi\n"
                "fi\n"
                "exit 0\n"
            )
            fake_herdr.chmod(0o755)
            install_root = root / "data" / "herdr-context" / "plugin"
            project_history = root / "project" / ".herdr" / "conversations" / "keep.jsonl"
            project_history.parent.mkdir(parents=True)
            project_history.write_text("history must survive\n")
            plugin_config = root / "config" / "herdr" / "plugins" / "config" / "herdr-context" / "config.toml"
            plugin_config.parent.mkdir(parents=True)
            plugin_config.write_text("[files]\nshow_hidden = true\n")
            plugin_state = root / "state" / "herdr" / "plugins" / "herdr-context" / "keep"
            plugin_state.parent.mkdir(parents=True)
            plugin_state.write_text("state must survive\n")
            env = os.environ.copy()
            env.update(
                {
                    "HOME": str(root / "home"),
                    "XDG_CONFIG_HOME": str(root / "config"),
                    "XDG_DATA_HOME": str(root / "data"),
                    "XDG_STATE_HOME": str(root / "state"),
                    "HERDR_BIN": str(fake_herdr),
                    "HERDR_FAKE_LOG": str(fake_log),
                    "HERDR_FAKE_COUNT": str(fake_count),
                    "HERDR_CONTEXT_INSTALL_DIR": str(install_root),
                }
            )

            first = self._extract_package(root, b"first binary\n", "first")
            subprocess.run([str(first / "install.sh")], check=True, env=env)
            self.assertEqual((install_root / "herdr-context").read_bytes(), b"first binary\n")

            second = self._extract_package(root, b"second binary\n", "second")
            env["HERDR_FAIL_LINK_CALL"] = "2"
            failed_upgrade = subprocess.run(
                [str(second / "install.sh")], check=False, env=env
            )
            self.assertNotEqual(failed_upgrade.returncode, 0)
            self.assertEqual(
                (install_root / "herdr-context").read_bytes(), b"first binary\n"
            )
            del env["HERDR_FAIL_LINK_CALL"]
            subprocess.run([str(second / "install.sh")], check=True, env=env)
            self.assertEqual((install_root / "herdr-context").read_bytes(), b"second binary\n")
            self.assertEqual(project_history.read_text(), "history must survive\n")
            self.assertIn("show_hidden = true", plugin_config.read_text())
            self.assertEqual(plugin_state.read_text(), "state must survive\n")

            subprocess.run([str(second / "uninstall.sh")], check=True, env=env)
            self.assertFalse(install_root.exists())
            self.assertEqual(project_history.read_text(), "history must survive\n")
            self.assertTrue(plugin_config.exists())
            self.assertTrue(plugin_state.exists())
            commands = fake_log.read_text()
            self.assertIn("plugin link", commands)
            self.assertIn("plugin unlink herdr-context", commands)

    def _extract_package(self, root: Path, binary_content: bytes, name: str) -> Path:
        binary = root / f"binary-{name}"
        binary.write_bytes(binary_content)
        binary.chmod(0o755)
        archive, checksum = release.create_package(
            ROOT,
            binary,
            "x86_64-unknown-linux-gnu",
            root / f"dist-{name}",
            allow_untagged=True,
        )
        release.verify_archive(archive, checksum)
        destination = root / f"extract-{name}"
        destination.mkdir()
        with tarfile.open(archive, "r:gz") as package:
            package.extractall(destination, filter="data")
        children = list(destination.iterdir())
        self.assertEqual(len(children), 1)
        return children[0]

    @staticmethod
    def _baseline(*, passed: bool) -> dict[str, object]:
        return {
            "schema_version": 1,
            "ticket": "HDC-15",
            "verdicts": [
                {
                    "metric": "first_frame_p95_ms",
                    "observed": 101.0 if not passed else 1.0,
                    "limit": 100.0,
                    "unit": "ms",
                    "comparator": "<=",
                    "passed": passed,
                }
            ],
            "failures": [] if passed else [{"metric": "first_frame_p95_ms"}],
            "overall_pass": passed,
        }

    @staticmethod
    def _review(verdict: str) -> dict[str, object]:
        budget: dict[str, object] = {
            "metric": "first_frame_p95_ms",
            "verdict": verdict,
            "reviewer": "independent HDC-15 review",
            "reviewed_at": "2026-08-14",
        }
        if verdict == "risk-accepted":
            budget["accepted_risk"] = {
                "authority": "release owner",
                "rationale": "Measured exception accepted for this release.",
                "scope": "V1 on documented targets.",
                "follow_up": "HDC-999",
            }
        return {
            "schema_version": 1,
            "ticket": "HDC-15",
            "baseline_sha256": "test",
            "budgets": [budget],
        }


if __name__ == "__main__":
    unittest.main()
