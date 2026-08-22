#!/usr/bin/env python3
"""Validate the herdr-context release contracts.

The plugin installs from source through `herdr plugin install`; a pushed
`v*` tag runs these checks and publishes generated release notes without
attaching any packaged assets.
"""
from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import re
import sys
import tomllib
from typing import Any, NamedTuple


PLUGIN_ID = "herdr-context"
MIN_HERDR_VERSION = "0.8.0"
VERSION_PATTERN = re.compile(
    r"(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\Z"
)


class ReleaseError(RuntimeError):
    pass


class ReleaseContract(NamedTuple):
    version: str
    min_herdr_version: str
    performance_metrics: tuple[str, ...]


def _load_toml(path: Path) -> dict[str, Any]:
    try:
        with path.open("rb") as source:
            value = tomllib.load(source)
    except (OSError, tomllib.TOMLDecodeError) as error:
        raise ReleaseError(f"cannot read TOML {path}: {error}") from error
    if not isinstance(value, dict):
        raise ReleaseError(f"TOML root must be a table: {path}")
    return value


def _required_string(table: dict[str, Any], key: str, context: str) -> str:
    value = table.get(key)
    if not isinstance(value, str) or not value.strip():
        raise ReleaseError(f"{context}.{key} must be a non-empty string")
    return value


def _required_table(table: dict[str, Any], key: str, context: str) -> dict[str, Any]:
    value = table.get(key)
    if not isinstance(value, dict):
        raise ReleaseError(f"{context}.{key} must be a table")
    return value


def _repository_path(root: Path, value: str, context: str) -> Path:
    candidate = Path(value)
    if candidate.is_absolute() or ".." in candidate.parts:
        raise ReleaseError(f"{context} must be a repository-relative path")
    resolved_root = root.resolve()
    resolved = (root / candidate).resolve()
    if resolved != resolved_root and resolved_root not in resolved.parents:
        raise ReleaseError(f"{context} escapes the repository")
    return resolved


def validate_performance_review(
    baseline_bytes: bytes,
    review: dict[str, Any],
    *,
    expected_digest: str | None,
) -> tuple[str, ...]:
    if review.get("schema_version") != 1:
        raise ReleaseError("performance review schema_version must be 1")
    if review.get("ticket") != "HDC-15":
        raise ReleaseError("performance review ticket must be HDC-15")
    actual_digest = hashlib.sha256(baseline_bytes).hexdigest()
    if expected_digest is not None and actual_digest != expected_digest:
        raise ReleaseError(
            f"performance baseline digest mismatch: expected {expected_digest}, got {actual_digest}"
        )
    try:
        baseline = json.loads(baseline_bytes)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise ReleaseError(f"invalid performance baseline JSON: {error}") from error
    if not isinstance(baseline, dict) or baseline.get("schema_version") != 1:
        raise ReleaseError("performance baseline schema_version must be 1")
    if baseline.get("ticket") != "HDC-15":
        raise ReleaseError("performance baseline ticket must be HDC-15")
    verdicts = baseline.get("verdicts")
    if not isinstance(verdicts, list) or not verdicts:
        raise ReleaseError("performance baseline must contain verdicts")

    baseline_by_metric: dict[str, dict[str, Any]] = {}
    ordered_metrics: list[str] = []
    for verdict in verdicts:
        if not isinstance(verdict, dict):
            raise ReleaseError("performance verdict entries must be tables")
        metric = _required_string(verdict, "metric", "performance verdict")
        if metric in baseline_by_metric:
            raise ReleaseError(f"duplicate performance verdict: {metric}")
        if not isinstance(verdict.get("passed"), bool):
            raise ReleaseError(f"performance verdict {metric}.passed must be boolean")
        if verdict.get("comparator") != "<=":
            raise ReleaseError(f"unsupported comparator for performance verdict {metric}")
        baseline_by_metric[metric] = verdict
        ordered_metrics.append(metric)

    budgets = review.get("budgets")
    if not isinstance(budgets, list) or not budgets:
        raise ReleaseError("performance review must contain budgets")
    review_by_metric: dict[str, dict[str, Any]] = {}
    for budget in budgets:
        if not isinstance(budget, dict):
            raise ReleaseError("performance review budget entries must be tables")
        metric = _required_string(budget, "metric", "performance review budget")
        if metric in review_by_metric:
            raise ReleaseError(f"duplicate reviewed budget: {metric}")
        review_by_metric[metric] = budget

    missing = sorted(set(baseline_by_metric) - set(review_by_metric))
    extra = sorted(set(review_by_metric) - set(baseline_by_metric))
    if missing or extra:
        raise ReleaseError(f"reviewed performance budgets differ: missing={missing}, extra={extra}")

    for metric in ordered_metrics:
        measured = baseline_by_metric[metric]
        reviewed = review_by_metric[metric]
        _required_string(reviewed, "reviewer", f"reviewed budget {metric}")
        _required_string(reviewed, "reviewed_at", f"reviewed budget {metric}")
        verdict = _required_string(reviewed, "verdict", f"reviewed budget {metric}")
        if measured["passed"]:
            if verdict != "pass":
                raise ReleaseError(f"passing budget {metric} must have verdict 'pass'")
            if "accepted_risk" in reviewed:
                raise ReleaseError(f"passing budget {metric} must not have accepted_risk")
            continue
        if verdict != "risk-accepted":
            raise ReleaseError(f"failed budget {metric} requires verdict 'risk-accepted'")
        accepted_risk = _required_table(reviewed, "accepted_risk", f"reviewed budget {metric}")
        for field in ("authority", "rationale", "scope", "follow_up"):
            _required_string(accepted_risk, field, f"reviewed budget {metric}.accepted_risk")

    measured_overall = all(bool(item["passed"]) for item in baseline_by_metric.values())
    if baseline.get("overall_pass") is not measured_overall:
        raise ReleaseError("performance baseline overall_pass disagrees with its verdicts")
    return tuple(ordered_metrics)


def validate_repository(root: Path) -> ReleaseContract:
    root = root.resolve()
    cargo = _load_toml(root / "Cargo.toml")
    package = _required_table(cargo, "package", "Cargo.toml.package")
    package_name = _required_string(package, "name", "Cargo.toml.package")
    version = _required_string(package, "version", "Cargo.toml.package")
    if package_name != PLUGIN_ID:
        raise ReleaseError(f"Cargo package name must be {PLUGIN_ID}")
    if not VERSION_PATTERN.fullmatch(version):
        raise ReleaseError(f"Cargo version is not stable SemVer: {version}")
    release_profile = _required_table(cargo, "profile", "Cargo.toml")
    release_settings = _required_table(
        release_profile, "release", "Cargo.toml.profile"
    )
    if release_settings.get("strip") != "symbols":
        raise ReleaseError("Cargo release profile must strip symbols")

    lock = _load_toml(root / "Cargo.lock")
    lock_packages = lock.get("package")
    if not isinstance(lock_packages, list):
        raise ReleaseError("Cargo.lock package list is missing")
    matching_lock = [item for item in lock_packages if isinstance(item, dict) and item.get("name") == package_name]
    if len(matching_lock) != 1 or matching_lock[0].get("version") != version:
        raise ReleaseError("Cargo.lock package version does not match Cargo.toml")

    manifest = _load_toml(root / "herdr-plugin.toml")
    if manifest.get("id") != PLUGIN_ID or manifest.get("name") != PLUGIN_ID:
        raise ReleaseError(f"manifest id and name must be {PLUGIN_ID}")
    if manifest.get("version") != version:
        raise ReleaseError("manifest version does not match Cargo.toml")
    min_herdr_version = _required_string(manifest, "min_herdr_version", "manifest")
    if min_herdr_version != MIN_HERDR_VERSION:
        raise ReleaseError(f"manifest min_herdr_version must be {MIN_HERDR_VERSION}")
    if manifest.get("platforms") != ["linux", "macos"]:
        raise ReleaseError("manifest platforms must be exactly linux and macos")
    if manifest.get("build") != [{"command": ["cargo", "build", "--release", "--locked"]}]:
        raise ReleaseError("manifest must build the locked release binary exactly once")
    if manifest.get("panes") != [
        {
            "id": "dock",
            "title": "herdr-context",
            "placement": "split",
            "command": ["./target/release/herdr-context", "dock"],
        }
    ]:
        raise ReleaseError("manifest must declare exactly one split dock pane")
    if manifest.get("actions") != [
        {
            "id": "toggle",
            "title": "herdr-context: toggle dock",
            "contexts": ["workspace", "tab", "pane"],
            "command": ["./target/release/herdr-context", "toggle"],
        }
    ]:
        raise ReleaseError("manifest must declare the workspace/tab/pane toggle action")

    try:
        readme = (root / "README.md").read_text(encoding="utf-8")
    except (OSError, UnicodeDecodeError) as error:
        raise ReleaseError(f"cannot read release documentation: {error}") from error
    documented = (
        f"Version `{version}`",
        f"`v{version}`",
        f"Herdr `{MIN_HERDR_VERSION}`",
    )
    missing_documentation = [
        snippet for snippet in documented if snippet not in readme
    ]
    if missing_documentation:
        raise ReleaseError(
            f"README release metadata is incomplete: {missing_documentation}"
        )

    review_path = root / "release" / "performance-review.toml"
    review = _load_toml(review_path)
    reviewed_revision = _required_string(
        review, "reviewed_revision", "performance review"
    )
    if not re.fullmatch(r"[0-9a-f]{8,40}", reviewed_revision):
        raise ReleaseError("performance review reviewed_revision must be a Git revision")
    baseline_relative = _required_string(review, "baseline", "performance review")
    baseline_path = _repository_path(root, baseline_relative, "performance review baseline")
    try:
        baseline_bytes = baseline_path.read_bytes()
    except OSError as error:
        raise ReleaseError(f"cannot read performance baseline {baseline_path}: {error}") from error
    expected_digest = _required_string(review, "baseline_sha256", "performance review")
    if not re.fullmatch(r"[0-9a-f]{64}", expected_digest):
        raise ReleaseError("performance review baseline_sha256 must be lowercase SHA-256")
    performance_metrics = validate_performance_review(
        baseline_bytes, review, expected_digest=expected_digest
    )
    return ReleaseContract(
        version=version,
        min_herdr_version=min_herdr_version,
        performance_metrics=performance_metrics,
    )


def validate_trigger_tag(contract: ReleaseContract, tag: str) -> None:
    expected = f"v{contract.version}"
    if tag != expected:
        raise ReleaseError(f"release tag must be exactly {expected}; got {tag!r}")


def _default_root() -> Path:
    return Path(__file__).resolve().parents[1]


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description="Validate herdr-context release contracts")
    subparsers = parser.add_subparsers(dest="command", required=True)
    validate = subparsers.add_parser(
        "validate", help="validate repository release contracts"
    )
    validate.add_argument("--tag", help="require the exact triggering release tag")
    return parser


def main(arguments: list[str] | None = None) -> int:
    args = _parser().parse_args(arguments)
    try:
        contract = validate_repository(_default_root())
        if args.tag is not None:
            validate_trigger_tag(contract, args.tag)
        print(f"release contract PASS: herdr-context v{contract.version}")
    except ReleaseError as error:
        print(f"release: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
