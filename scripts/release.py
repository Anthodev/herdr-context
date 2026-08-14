#!/usr/bin/env python3
from __future__ import annotations

import argparse
import gzip
import hashlib
import io
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import tomllib
from typing import Any, NamedTuple


PLUGIN_ID = "herdr-context"
MIN_HERDR_VERSION = "0.8.0"
SUPPORTED_TARGETS = {
    "x86_64-unknown-linux-gnu": "linux",
    "aarch64-unknown-linux-gnu": "linux",
    "x86_64-apple-darwin": "macos",
    "aarch64-apple-darwin": "macos",
}
PACKAGE_FILES = (
    "LICENSE",
    "README.md",
    "herdr-context",
    "herdr-plugin.toml",
    "install.sh",
    "uninstall.sh",
)
EXECUTABLE_FILES = {"herdr-context", "install.sh", "uninstall.sh"}
MAX_ARCHIVE_BYTES = 128 * 1024 * 1024
MAX_PACKAGE_MEMBER_BYTES = {
    "LICENSE": 1024 * 1024,
    "README.md": 2 * 1024 * 1024,
    "herdr-context": 128 * 1024 * 1024,
    "herdr-plugin.toml": 1024 * 1024,
    "install.sh": 1024 * 1024,
    "uninstall.sh": 1024 * 1024,
}
MAX_UNCOMPRESSED_BYTES = sum(MAX_PACKAGE_MEMBER_BYTES.values())
VERSION_PATTERN = re.compile(
    r"(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\Z"
)
ARCHIVE_ROOT_PATTERN = re.compile(
    r"herdr-context-v(?P<version>[0-9]+\.[0-9]+\.[0-9]+)-"
    r"(?P<target>[A-Za-z0-9_.-]+)\Z"
)
SENSITIVE_MARKERS = (
    b"BEGIN OPENSSH PRIVATE KEY",
    b"BEGIN RSA PRIVATE KEY",
    b"AWS_SECRET_ACCESS_KEY=",
    b"GITHUB_TOKEN=",
)


class ReleaseError(RuntimeError):
    pass


class ReleaseContract(NamedTuple):
    version: str
    binary_name: str
    min_herdr_version: str
    performance_metrics: tuple[str, ...]
    manifest: dict[str, Any]


class ArchiveMetadata(NamedTuple):
    archive_root: str
    version: str
    target: str
    members: tuple[str, ...]


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


def _required_tables(table: dict[str, Any], key: str, context: str) -> list[dict[str, Any]]:
    value = table.get(key)
    if not isinstance(value, list) or not value or not all(isinstance(item, dict) for item in value):
        raise ReleaseError(f"{context}.{key} must be a non-empty array of tables")
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
    package = _required_table(cargo, "package", "Cargo.toml")
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
            "title": "herdr-context dock",
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
        release_workflow = (root / ".github" / "workflows" / "release.yml").read_text(
            encoding="utf-8"
        )
    except (OSError, UnicodeDecodeError) as error:
        raise ReleaseError(f"cannot read release documentation or workflow: {error}") from error
    documented = (
        f"Version `{version}`",
        f"`v{version}`",
        f"Herdr `{MIN_HERDR_VERSION}`",
        *SUPPORTED_TARGETS,
    )
    missing_documentation = [
        snippet for snippet in documented if snippet not in readme
    ]
    if missing_documentation:
        raise ReleaseError(
            f"README release metadata is incomplete: {missing_documentation}"
        )
    missing_matrix = [
        target for target in SUPPORTED_TARGETS if target not in release_workflow
    ]
    if missing_matrix:
        raise ReleaseError(f"release workflow target matrix is incomplete: {missing_matrix}")

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
        binary_name=package_name,
        min_herdr_version=min_herdr_version,
        performance_metrics=performance_metrics,
        manifest=manifest,
    )


def validate_trigger_tag(contract: ReleaseContract, tag: str) -> None:
    expected = f"v{contract.version}"
    if tag != expected:
        raise ReleaseError(f"release tag must be exactly {expected}; got {tag!r}")


def _toml_string(value: str) -> str:
    return json.dumps(value, ensure_ascii=True)


def _toml_array(values: list[str]) -> str:
    return "[" + ", ".join(_toml_string(value) for value in values) + "]"


def render_binary_manifest(contract: ReleaseContract) -> bytes:
    source = contract.manifest
    description = _required_string(source, "description", "manifest")
    pane = source["panes"][0]
    action = source["actions"][0]
    text = "\n".join(
        (
            f'id = {_toml_string(PLUGIN_ID)}',
            f'name = {_toml_string(PLUGIN_ID)}',
            f'version = {_toml_string(contract.version)}',
            f'min_herdr_version = {_toml_string(contract.min_herdr_version)}',
            f'description = {_toml_string(description)}',
            'platforms = ["linux", "macos"]',
            "",
            "[[panes]]",
            f'id = {_toml_string(pane["id"])}',
            f'title = {_toml_string(pane["title"])}',
            f'placement = {_toml_string(pane["placement"])}',
            'command = ["./herdr-context", "dock"]',
            "",
            "[[actions]]",
            f'id = {_toml_string(action["id"])}',
            f'title = {_toml_string(action["title"])}',
            f'contexts = {_toml_array(action["contexts"])}',
            'command = ["./herdr-context", "toggle"]',
            "",
        )
    )
    rendered = text.encode()
    parsed = tomllib.loads(text)
    if "build" in parsed or parsed["panes"][0]["command"][0] != "./herdr-context":
        raise ReleaseError("generated binary manifest is invalid")
    return rendered


def _run_git(root: Path, arguments: list[str]) -> str:
    try:
        completed = subprocess.run(
            ["git", *arguments],
            cwd=root,
            check=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
    except (OSError, subprocess.CalledProcessError) as error:
        raise ReleaseError(f"cannot inspect release Git state: {error}") from error
    return completed.stdout.strip()


def validate_tagged_source(root: Path, version: str) -> None:
    dirty = _run_git(root, ["status", "--porcelain", "--untracked-files=all"])
    if dirty:
        raise ReleaseError("release source is dirty")
    expected_tag = f"v{version}"
    tags = set(_run_git(root, ["tag", "--points-at", "HEAD"]).splitlines())
    if expected_tag not in tags:
        raise ReleaseError(f"release source must be tagged {expected_tag}")


def _safe_source_file(path: Path, context: str) -> bytes:
    if path.is_symlink() or not path.is_file():
        raise ReleaseError(f"{context} must be a regular file: {path}")
    try:
        return path.read_bytes()
    except OSError as error:
        raise ReleaseError(f"cannot read {context} {path}: {error}") from error


def _scan_package_content(content: dict[str, bytes], root: Path) -> None:
    forbidden_paths = {str(root.resolve()).encode(), str(Path.home().resolve()).encode()}
    for name, data in content.items():
        for marker in SENSITIVE_MARKERS:
            if marker in data:
                raise ReleaseError(f"sensitive marker found in package member {name}")
        for forbidden in forbidden_paths:
            if forbidden and forbidden in data:
                raise ReleaseError(
                    f"developer path {forbidden.decode(errors='replace')} found in package member {name}"
                )


def _write_deterministic_archive(archive: Path, archive_root: str, content: dict[str, bytes]) -> None:
    with archive.open("wb") as raw:
        with gzip.GzipFile(filename="", mode="wb", fileobj=raw, compresslevel=9, mtime=0) as compressed:
            with tarfile.open(fileobj=compressed, mode="w", format=tarfile.USTAR_FORMAT) as package:
                root_info = tarfile.TarInfo(archive_root)
                root_info.type = tarfile.DIRTYPE
                root_info.mode = 0o755
                root_info.mtime = 0
                root_info.uid = root_info.gid = 0
                root_info.uname = root_info.gname = ""
                package.addfile(root_info)
                for name in sorted(content):
                    data = content[name]
                    info = tarfile.TarInfo(f"{archive_root}/{name}")
                    info.mode = 0o755 if name in EXECUTABLE_FILES else 0o644
                    info.size = len(data)
                    info.mtime = 0
                    info.uid = info.gid = 0
                    info.uname = info.gname = ""
                    package.addfile(info, io.BytesIO(data))


def create_package(
    root: Path,
    binary: Path,
    target: str,
    output_dir: Path,
    *,
    allow_untagged: bool,
) -> tuple[Path, Path]:
    root = root.resolve()
    contract = validate_repository(root)
    if target not in SUPPORTED_TARGETS:
        raise ReleaseError(f"unsupported release target: {target}")
    if not allow_untagged:
        validate_tagged_source(root, contract.version)
    binary_data = _safe_source_file(binary.resolve(), "release binary")
    if not os.access(binary, os.X_OK):
        raise ReleaseError(f"release binary is not executable: {binary}")

    content = {
        "LICENSE": _safe_source_file(root / "LICENSE", "license"),
        "README.md": _safe_source_file(root / "README.md", "README"),
        "herdr-context": binary_data,
        "herdr-plugin.toml": render_binary_manifest(contract),
        "install.sh": _safe_source_file(root / "scripts" / "release-install.sh", "installer"),
        "uninstall.sh": _safe_source_file(root / "scripts" / "release-uninstall.sh", "uninstaller"),
    }
    if set(content) != set(PACKAGE_FILES):
        raise ReleaseError("internal package whitelist mismatch")
    _scan_package_content(content, root)

    output_dir.mkdir(parents=True, exist_ok=True)
    archive_root = f"herdr-context-v{contract.version}-{target}"
    archive = output_dir / f"{archive_root}.tar.gz"
    checksum = output_dir / f"{archive.name}.sha256"
    _write_deterministic_archive(archive, archive_root, content)
    digest = hashlib.sha256(archive.read_bytes()).hexdigest()
    checksum.write_text(f"{digest}  {archive.name}\n", encoding="ascii", newline="\n")
    verify_archive(archive, checksum)
    return archive, checksum


def verify_archive(archive: Path, checksum: Path) -> ArchiveMetadata:
    if archive.is_symlink() or checksum.is_symlink() or not archive.is_file() or not checksum.is_file():
        raise ReleaseError("archive and checksum must be regular files")
    if archive.stat().st_size > MAX_ARCHIVE_BYTES:
        raise ReleaseError("archive exceeds the 128 MiB release limit")
    try:
        checksum_text = checksum.read_text(encoding="ascii")
    except (OSError, UnicodeDecodeError) as error:
        raise ReleaseError(f"cannot read checksum: {error}") from error
    match = re.fullmatch(r"([0-9a-f]{64})  ([^/\n]+)\n", checksum_text)
    if match is None or match.group(2) != archive.name:
        raise ReleaseError("checksum file has an invalid format or filename")
    actual_digest = hashlib.sha256(archive.read_bytes()).hexdigest()
    if actual_digest != match.group(1):
        raise ReleaseError("archive checksum mismatch")

    try:
        with tarfile.open(archive, "r:gz") as package:
            entries = package.getmembers()
            roots = [entry for entry in entries if entry.isdir()]
            files = [entry for entry in entries if entry.isfile()]
            if len(roots) != 1 or roots[0].name.count("/") != 0:
                raise ReleaseError("archive must contain exactly one root directory")
            archive_root = roots[0].name
            root_match = ARCHIVE_ROOT_PATTERN.fullmatch(archive_root)
            if root_match is None or root_match.group("target") not in SUPPORTED_TARGETS:
                raise ReleaseError("archive root has an invalid versioned target name")
            expected_names = {f"{archive_root}/{name}" for name in PACKAGE_FILES}
            actual_names = {entry.name for entry in files}
            if actual_names != expected_names or len(files) != len(expected_names):
                raise ReleaseError("archive contents differ from the release whitelist")
            if any(not (entry.isfile() or entry.isdir()) for entry in entries):
                raise ReleaseError("archive contains links or special files")
            total_size = 0
            for entry in files:
                member_name = Path(entry.name).name
                maximum = MAX_PACKAGE_MEMBER_BYTES[member_name]
                if entry.size < 0 or entry.size > maximum:
                    raise ReleaseError(
                        f"archive member exceeds its uncompressed limit: {entry.name}"
                    )
                total_size += entry.size
            if total_size > MAX_UNCOMPRESSED_BYTES:
                raise ReleaseError("archive exceeds the aggregate uncompressed limit")
            content: dict[str, bytes] = {}
            for entry in files:
                expected_mode = 0o755 if Path(entry.name).name in EXECUTABLE_FILES else 0o644
                if entry.mode != expected_mode or entry.uid != 0 or entry.gid != 0 or entry.mtime != 0:
                    raise ReleaseError(f"archive metadata is not deterministic: {entry.name}")
                extracted = package.extractfile(entry)
                if extracted is None:
                    raise ReleaseError(f"cannot read archive member: {entry.name}")
                content[Path(entry.name).name] = extracted.read()
    except (OSError, tarfile.TarError) as error:
        raise ReleaseError(f"cannot read release archive: {error}") from error

    manifest = tomllib.loads(content["herdr-plugin.toml"].decode("utf-8"))
    if manifest.get("build") is not None:
        raise ReleaseError("binary archive manifest must not contain build commands")
    if manifest.get("version") != root_match.group("version"):
        raise ReleaseError("archive manifest version does not match its filename")
    if manifest.get("panes", [{}])[0].get("command") != ["./herdr-context", "dock"]:
        raise ReleaseError("archive pane command does not use the packaged binary")
    if manifest.get("actions", [{}])[0].get("command") != ["./herdr-context", "toggle"]:
        raise ReleaseError("archive action command does not use the packaged binary")
    _scan_package_content(content, Path("/__herdr_context_no_repository_path__"))
    return ArchiveMetadata(
        archive_root=archive_root,
        version=root_match.group("version"),
        target=root_match.group("target"),
        members=tuple(sorted(f"{archive_root}/{name}" for name in content)),
    )


def _default_root() -> Path:
    return Path(__file__).resolve().parents[1]


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description="Validate and package herdr-context releases")
    subparsers = parser.add_subparsers(dest="command", required=True)
    validate = subparsers.add_parser(
        "validate", help="validate repository release contracts"
    )
    validate.add_argument("--tag", help="require the exact triggering release tag")
    package = subparsers.add_parser("package", help="create a deterministic binary archive")
    package.add_argument("--target", required=True, choices=sorted(SUPPORTED_TARGETS))
    package.add_argument("--binary", type=Path, required=True)
    package.add_argument("--output-dir", type=Path, default=Path("target/release-dist"))
    package.add_argument("--allow-untagged", action="store_true")
    verify = subparsers.add_parser("verify", help="verify an archive and checksum")
    verify.add_argument("archive", type=Path)
    verify.add_argument("checksum", type=Path)
    return parser


def main(arguments: list[str] | None = None) -> int:
    args = _parser().parse_args(arguments)
    try:
        if args.command == "validate":
            contract = validate_repository(_default_root())
            if args.tag is not None:
                validate_trigger_tag(contract, args.tag)
            print(f"release contract PASS: herdr-context v{contract.version}")
        elif args.command == "package":
            archive, checksum = create_package(
                _default_root(),
                args.binary,
                args.target,
                args.output_dir,
                allow_untagged=args.allow_untagged,
            )
            print(archive)
            print(checksum)
        else:
            metadata = verify_archive(args.archive, args.checksum)
            print(
                f"archive PASS: v{metadata.version} {metadata.target} "
                f"({len(metadata.members)} files)"
            )
    except ReleaseError as error:
        print(f"release: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
