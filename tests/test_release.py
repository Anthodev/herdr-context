from __future__ import annotations

import importlib.util
import json
from pathlib import Path
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

        self.assertEqual(contract.version, "0.19.0")
        self.assertEqual(contract.min_herdr_version, "0.8.0")
        self.assertEqual(len(contract.performance_metrics), 12)

    def test_trigger_tag_must_exactly_match_cargo_version(self) -> None:
        contract = release.validate_repository(ROOT)

        release.validate_trigger_tag(contract, "v0.19.0")
        with self.assertRaisesRegex(release.ReleaseError, "exactly v0.19.0"):
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


if __name__ == "__main__":
    unittest.main()
