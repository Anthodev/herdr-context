from __future__ import annotations

import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
MODULE_PATH = ROOT / "scripts" / "release.py"
SPEC = importlib.util.spec_from_file_location("herdr_context_release", MODULE_PATH)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError(f"cannot load {MODULE_PATH}")
release = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(release)


TAGGED_CHANGELOG = (
    "# Changelog\n"
    "\n"
    "## [0.19.6] - 2026-09-14\n"
    "\n"
    "Dock restoration now reopens the previous layout after a Herdr restart.\n"
    "\n"
    "### Features\n"
    "\n"
    "- feat(dock): restore docks on startup (`a1b2c3d`)\n"
    "\n"
    "## [0.19.5] - 2026-08-31\n"
    "\n"
    "### Fixes\n"
    "\n"
    "- fix(vcs): ignore structural Jujutsu tree entries (`fc08145`)\n"
)

TAGGED_NOTES = (
    "Dock restoration now reopens the previous layout after a Herdr restart.\n"
    "\n"
    "### Features\n"
    "\n"
    "- feat(dock): restore docks on startup (`a1b2c3d`)\n"
)

NEXT_CYCLE_CHANGELOG = (
    "# Changelog\n"
    "\n"
    "## [0.19.5] - 2026-08-31\n"
    "\n"
    "### Fixes\n"
    "\n"
    "- fix(vcs): ignore structural Jujutsu tree entries (`fc08145`)\n"
)

WORKING_TREE_CHANGELOG = (
    "# Changelog\n"
    "\n"
    "## [0.19.7] - 2026-10-01\n"
    "\n"
    "Unreleased working tree state that must never reach the release notes.\n"
)

PRERELEASE_CHANGELOG = (
    "# Changelog\n"
    "\n"
    "## [0.19.6] - 2026-09-14\n"
    "\n"
    "stable body\n"
    "\n"
    "## [0.19.6-rc1] - 2026-09-10\n"
    "\n"
    "candidate body\n"
    "\n"
    "## [0.19] - 2026-09-01\n"
    "\n"
    "short body\n"
)


class ReleaseContractTests(unittest.TestCase):
    def test_repository_contract_is_release_ready(self) -> None:
        contract = release.validate_repository(ROOT)

        self.assertEqual(contract.version, "0.19.7")
        self.assertEqual(contract.min_herdr_version, "0.8.0")
        self.assertEqual(len(contract.performance_metrics), 12)

    def test_trigger_tag_must_exactly_match_cargo_version(self) -> None:
        contract = release.validate_repository(ROOT)

        release.validate_trigger_tag(contract, "v0.19.7")
        with self.assertRaisesRegex(release.ReleaseError, "exactly v0.19.7"):
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


class ExtractChangelogSectionTests(unittest.TestCase):
    def test_returns_complete_release_section(self) -> None:
        self.assertEqual(
            release.extract_changelog_section(TAGGED_CHANGELOG, "0.19.6"), TAGGED_NOTES
        )

    def test_returns_section_without_date_suffix(self) -> None:
        changelog = "# Changelog\n\n## [0.19.6]\n\nInitial dock restoration.\n"

        self.assertEqual(
            release.extract_changelog_section(changelog, "0.19.6"),
            "Initial dock restoration.\n",
        )

    def test_matches_exact_version_only(self) -> None:
        changelog = (
            "# Changelog\n"
            "\n"
            "## [0.19.60] - 2026-09-20\n"
            "\n"
            "prefix collision\n"
            "\n"
            "## [v0.19.6] - 2026-09-19\n"
            "\n"
            "tag-prefixed collision\n"
            "\n"
            "## [0.19] - 2026-09-18\n"
            "\n"
            "short collision\n"
            "\n"
            "## [0.19.6]-beta\n"
            "\n"
            "suffix collision\n"
            "\n"
            "## [0.19.6] - 2026-09-14\n"
            "\n"
            "release body\n"
        )

        self.assertEqual(
            release.extract_changelog_section(changelog, "0.19.6"), "release body\n"
        )

    def test_stops_at_any_level_two_heading(self) -> None:
        changelog = (
            "# Changelog\n"
            "\n"
            "## [0.19.6] - 2026-09-14\n"
            "\n"
            "release body\n"
            "\n"
            "## Unreleased\n"
            "\n"
            "future work\n"
        )

        self.assertEqual(
            release.extract_changelog_section(changelog, "0.19.6"), "release body\n"
        )

    def test_ignores_headings_inside_fenced_blocks(self) -> None:
        changelog = (
            "# Changelog\n"
            "\n"
            "## [0.19.6] - 2026-09-14\n"
            "\n"
            "Release highlights.\n"
            "\n"
            "```text\n"
            "## [0.19.6] - 2026-09-14\n"
            "## [0.19.5] - 2026-08-31\n"
            "```\n"
            "\n"
            "~~~markdown\n"
            "## [0.19.6]\n"
            "~~~\n"
            "\n"
            "- feat(dock): restore docks on startup (`a1b2c3d`)\n"
            "\n"
            "## [0.19.5] - 2026-08-31\n"
            "\n"
            "older release\n"
        )
        expected = (
            "Release highlights.\n"
            "\n"
            "```text\n"
            "## [0.19.6] - 2026-09-14\n"
            "## [0.19.5] - 2026-08-31\n"
            "```\n"
            "\n"
            "~~~markdown\n"
            "## [0.19.6]\n"
            "~~~\n"
            "\n"
            "- feat(dock): restore docks on startup (`a1b2c3d`)\n"
        )

        self.assertEqual(
            release.extract_changelog_section(changelog, "0.19.6"), expected
        )

    def test_normalizes_crlf_line_endings(self) -> None:
        changelog = (
            "# Changelog\r\n"
            "\r\n"
            "## [0.19.6] - 2026-09-14\r\n"
            "\r\n"
            "Release highlights.\r\n"
            "\r\n"
            "- feat(dock): restore docks on startup (`a1b2c3d`)\r\n"
            "\r\n"
            "## [0.19.5] - 2026-08-31\r\n"
            "\r\n"
            "older release\r\n"
        )

        notes = release.extract_changelog_section(changelog, "0.19.6")

        self.assertEqual(
            notes,
            "Release highlights.\n\n- feat(dock): restore docks on startup (`a1b2c3d`)\n",
        )
        self.assertNotIn("\r", notes)

    def test_rejects_missing_section(self) -> None:
        with self.assertRaises(release.ReleaseError):
            release.extract_changelog_section(TAGGED_CHANGELOG, "0.19.7")

    def test_rejects_empty_section(self) -> None:
        adjacent = "# Changelog\n\n## [0.19.6] - 2026-09-14\n## [0.19.5] - 2026-08-31\n\nolder release\n"
        blank_lines = "# Changelog\n\n## [0.19.6] - 2026-09-14\n\n\n\n## [0.19.5] - 2026-08-31\n\nolder release\n"
        end_of_file = "# Changelog\n\n## [0.19.6] - 2026-09-14\n"
        for name, changelog in (
            ("adjacent heading", adjacent),
            ("blank lines only", blank_lines),
            ("end of file", end_of_file),
        ):
            with self.subTest(name):
                with self.assertRaises(release.ReleaseError):
                    release.extract_changelog_section(changelog, "0.19.6")

    def test_rejects_duplicate_sections(self) -> None:
        changelog = (
            "# Changelog\n\n## [0.19.6] - 2026-09-14\n\nfirst copy\n\n"
            "## [0.19.6] - 2026-09-14\n\nsecond copy\n"
        )

        with self.assertRaises(release.ReleaseError):
            release.extract_changelog_section(changelog, "0.19.6")

    def test_rejects_duplicates_separated_by_another_release(self) -> None:
        changelog = (
            "# Changelog\n\n## [0.19.6] - 2026-09-14\n\nfirst copy\n\n"
            "## [0.19.5] - 2026-08-31\n\nolder release\n\n"
            "## [0.19.6] - 2026-09-20\n\nsecond copy\n"
        )

        with self.assertRaises(release.ReleaseError):
            release.extract_changelog_section(changelog, "0.19.6")


def _git_environment(home: Path) -> dict[str, str]:
    """Return a hermetic, deterministic environment for fixture git commands."""
    environment = {
        key: value for key, value in os.environ.items() if not key.startswith("GIT_")
    }
    environment.update(
        {
            "HOME": str(home),
            "XDG_CONFIG_HOME": str(home / "xdg"),
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_ATTR_NOSYSTEM": "1",
            "GIT_AUTHOR_NAME": "Release Notes Tests",
            "GIT_AUTHOR_EMAIL": "release-notes@example.invalid",
            "GIT_COMMITTER_NAME": "Release Notes Tests",
            "GIT_COMMITTER_EMAIL": "release-notes@example.invalid",
            "GIT_AUTHOR_DATE": "2026-09-01T12:00:00+00:00",
            "GIT_COMMITTER_DATE": "2026-09-01T12:00:00+00:00",
        }
    )
    return environment


def _run_git(root: Path, home: Path, *arguments: str) -> str:
    completed = subprocess.run(
        ["git", "-C", str(root), *arguments],
        capture_output=True,
        encoding="utf-8",
        check=True,
        env=_git_environment(home),
    )
    return completed.stdout


def _commit_files(root: Path, home: Path, files: dict[str, str], message: str) -> str:
    for name, content in files.items():
        path = root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content, encoding="utf-8", newline="\n")
    _run_git(root, home, "add", "--all")
    _run_git(root, home, "commit", "-m", message)
    return _run_git(root, home, "rev-parse", "HEAD").strip()


def _create_tag(root: Path, home: Path, tag: str, revision: str, *, annotate: bool) -> None:
    arguments = ["tag"]
    if annotate:
        arguments += ["--annotate", "--message", f"release {tag}"]
    arguments += [tag, revision]
    _run_git(root, home, *arguments)


class GenerateReleaseNotesTests(unittest.TestCase):
    def setUp(self) -> None:
        sandbox = tempfile.TemporaryDirectory()
        self.addCleanup(sandbox.cleanup)
        base = Path(sandbox.name)
        self.home = base / "home"
        self.home.mkdir()
        self.repository = base / "repository"
        self.repository.mkdir()
        _run_git(self.repository, self.home, "init", "--initial-branch=main")

    def test_annotated_tag_reads_notes_from_the_tagged_commit(self) -> None:
        tagged = _commit_files(
            self.repository,
            self.home,
            {"CHANGELOG.md": TAGGED_CHANGELOG},
            "feat(dock): restore docks on startup",
        )
        _create_tag(self.repository, self.home, "v0.19.6", tagged, annotate=True)
        _commit_files(
            self.repository,
            self.home,
            {"CHANGELOG.md": NEXT_CYCLE_CHANGELOG},
            "docs: start next release cycle",
        )
        (self.repository / "CHANGELOG.md").write_text(
            WORKING_TREE_CHANGELOG, encoding="utf-8", newline="\n"
        )

        notes = release.generate_release_notes(self.repository, "v0.19.6", tagged)

        self.assertEqual(notes, TAGGED_NOTES)

    def test_lightweight_tag_reads_notes_from_the_tagged_commit(self) -> None:
        tagged = _commit_files(
            self.repository,
            self.home,
            {"CHANGELOG.md": TAGGED_CHANGELOG},
            "feat(dock): restore docks on startup",
        )
        _create_tag(self.repository, self.home, "v0.19.6", tagged, annotate=False)

        notes = release.generate_release_notes(self.repository, "v0.19.6", tagged)

        self.assertEqual(notes, TAGGED_NOTES)

    def test_revision_must_match_the_tagged_commit(self) -> None:
        tagged = _commit_files(
            self.repository,
            self.home,
            {"CHANGELOG.md": TAGGED_CHANGELOG},
            "feat(dock): restore docks on startup",
        )
        _create_tag(self.repository, self.home, "v0.19.6", tagged, annotate=True)
        moved = _commit_files(
            self.repository,
            self.home,
            {"CHANGELOG.md": NEXT_CYCLE_CHANGELOG},
            "docs: start next release cycle",
        )
        self.assertNotEqual(tagged, moved)

        with self.assertRaises(release.ReleaseError):
            release.generate_release_notes(self.repository, "v0.19.6", moved)

    def test_rejects_tags_that_are_not_strict_stable_versions(self) -> None:
        commit = _commit_files(
            self.repository,
            self.home,
            {"CHANGELOG.md": PRERELEASE_CHANGELOG},
            "feat: prepare release candidates",
        )
        for tag in ("v0.19", "v0.19.6-rc1"):
            _create_tag(self.repository, self.home, tag, commit, annotate=False)

        for tag in ("0.19.6", "v0.19", "v0.19.6-rc1", "v0.19.6+build.1", "v00.19.6"):
            with self.subTest(tag):
                with self.assertRaises(release.ReleaseError):
                    release.generate_release_notes(self.repository, tag, commit)

    def test_rejects_tagged_commit_without_changelog(self) -> None:
        commit = _commit_files(
            self.repository,
            self.home,
            {"README.md": "# herdr-context\n"},
            "chore: initial import",
        )
        _create_tag(self.repository, self.home, "v0.19.6", commit, annotate=True)

        with self.assertRaises(release.ReleaseError):
            release.generate_release_notes(self.repository, "v0.19.6", commit)


if __name__ == "__main__":
    unittest.main()
