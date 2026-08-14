#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import socket
import sys
import tarfile
import tempfile
import time
from typing import Any, Callable

import release as release_tool


COMMAND_TIMEOUT = 15
STARTUP_TIMEOUT = 20
DOCK_TIMEOUT = 15


class VerificationError(RuntimeError):
    pass


def _run(
    arguments: list[str],
    *,
    env: dict[str, str],
    timeout: int = COMMAND_TIMEOUT,
    check: bool = True,
) -> subprocess.CompletedProcess[str]:
    try:
        completed = subprocess.run(
            arguments,
            env=env,
            cwd=env.get("HERDR_CONTEXT_VERIFY_CWD"),
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            timeout=timeout,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        raise VerificationError(f"cannot run {arguments[0]}: {error}") from error
    if check and completed.returncode != 0:
        raise VerificationError(
            f"command failed ({completed.returncode}): {' '.join(arguments)}\n"
            f"stdout: {completed.stdout[-4096:]}\n"
            f"stderr: {completed.stderr[-4096:]}"
        )
    return completed


def _json_result(completed: subprocess.CompletedProcess[str]) -> dict[str, Any]:
    try:
        payload = json.loads(completed.stdout)
    except json.JSONDecodeError as error:
        raise VerificationError(f"Herdr returned invalid JSON: {completed.stdout[-4096:]}") from error
    if not isinstance(payload, dict) or not isinstance(payload.get("result"), dict):
        raise VerificationError(f"Herdr returned an unexpected response: {payload!r}")
    return payload["result"]


def _wait(description: str, predicate: Callable[[], Any], timeout: int) -> Any:
    deadline = time.monotonic() + timeout
    last_error: Exception | None = None
    while time.monotonic() < deadline:
        try:
            value = predicate()
            if value:
                return value
        except (VerificationError, OSError, KeyError, TypeError, ValueError) as error:
            last_error = error
        time.sleep(0.1)
    suffix = f": {last_error}" if last_error is not None else ""
    raise VerificationError(f"timed out waiting for {description}{suffix}")


def _native_target() -> str:
    system = platform.system()
    machine = platform.machine().lower()
    aliases = {"amd64": "x86_64", "arm64": "aarch64"}
    machine = aliases.get(machine, machine)
    targets = {
        ("Linux", "x86_64"): "x86_64-unknown-linux-gnu",
        ("Linux", "aarch64"): "aarch64-unknown-linux-gnu",
        ("Darwin", "x86_64"): "x86_64-apple-darwin",
        ("Darwin", "aarch64"): "aarch64-apple-darwin",
    }
    try:
        return targets[(system, machine)]
    except KeyError as error:
        raise VerificationError(f"unsupported clean-install host: {system} {machine}") from error


def _isolated_runtime_path(directory: Path) -> str:
    directory.mkdir()
    for command in (
        "cat",
        "chmod",
        "cp",
        "dirname",
        "git",
        "mkdir",
        "mktemp",
        "mv",
        "rm",
        "sleep",
    ):
        executable = shutil.which(command)
        if executable is None:
            raise VerificationError(f"required smoke-test tool is unavailable: {command}")
        directory.joinpath(command).symlink_to(Path(executable).resolve())
    return str(directory)


def _write_fake_jj(directory: Path) -> Path:
    executable = directory / "jj"
    executable.write_text(
        "#!/bin/sh\n"
        "case \" $* \" in\n"
        "  *\" root \"*) pwd -P ;;\n"
        "  *\" diff \"*) "
        "printf 'M\\000change.txt\\000change.txt\\000false\\000false\\000file\\000file\\000' ;;\n"
        "  *) exit 2 ;;\n"
        "esac\n"
    )
    executable.chmod(0o755)
    return executable


def _version_tuple(value: str) -> tuple[int, int, int]:
    match = re.fullmatch(r"([0-9]+)\.([0-9]+)\.([0-9]+)", value)
    if match is None:
        raise VerificationError(f"invalid version: {value}")
    return tuple(int(part) for part in match.groups())  # type: ignore[return-value]


def _herdr_command(herdr: Path, session: str, *arguments: str) -> list[str]:
    return [str(herdr), "--session", session, *arguments]


def _herdr_json(
    herdr: Path,
    session: str,
    env: dict[str, str],
    *arguments: str,
    check: bool = True,
) -> dict[str, Any]:
    completed = _run(_herdr_command(herdr, session, *arguments), env=env, check=check)
    if not check and completed.returncode != 0:
        return {}
    try:
        return _json_result(completed)
    except VerificationError as error:
        raise VerificationError(
            f"Herdr {' '.join(arguments)}: {error}"
        ) from error



def _report_live_session(
    socket_path: Path,
    pane_id: str,
    session_path: Path,
) -> None:
    seq = time.time_ns()
    common: dict[str, Any] = {
        "pane_id": pane_id,
        "source": "herdr:pi",
        "agent": "pi",
        "agent_session_path": str(session_path),
    }
    for method, extra in (
        ("pane.report_agent_session", {"session_start_source": "new"}),
        ("pane.report_agent", {"state": "idle"}),
    ):
        request = {
            "id": f"hdc16-{method}-{seq}",
            "method": method,
            "params": {**common, **extra, "seq": seq},
        }
        seq += 1
        try:
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
                client.settimeout(2)
                client.connect(str(socket_path))
                client.sendall((json.dumps(request) + "\n").encode())
                response = client.recv(4096)
        except OSError as error:
            raise VerificationError(f"cannot report sanitized live session: {error}") from error
        try:
            payload = json.loads(response)
        except json.JSONDecodeError as error:
            raise VerificationError("Herdr session report returned invalid JSON") from error
        if not isinstance(payload, dict) or "error" in payload:
            raise VerificationError(f"Herdr rejected sanitized live session: {payload!r}")

def _extract_verified(archive: Path, destination: Path) -> Path:
    with tarfile.open(archive, "r:gz") as package:
        package.extractall(destination, filter="data")
    children = list(destination.iterdir())
    if len(children) != 1 or not children[0].is_dir():
        raise VerificationError("verified archive did not extract to one package root")
    return children[0]


def _verify_corrupt_rejection(archive: Path, checksum: Path, root: Path) -> None:
    corrupt = root / archive.name
    data = bytearray(archive.read_bytes())
    if not data:
        raise VerificationError("release archive is empty")
    data[len(data) // 2] ^= 0x01
    corrupt.write_bytes(data)
    copied_checksum = root / checksum.name
    shutil.copyfile(checksum, copied_checksum)
    try:
        release_tool.verify_archive(corrupt, copied_checksum)
    except release_tool.ReleaseError:
        return
    raise VerificationError("corrupt release archive passed checksum verification")


def _write_fixtures(root: Path, home: Path) -> tuple[Path, Path, Path, Path]:
    no_vcs = root / "workspaces" / "no-vcs"
    local_history = no_vcs / ".herdr" / "conversations" / "local.jsonl"
    local_history.parent.mkdir(parents=True)
    (no_vcs / "visible.txt").write_text("sanitized file fixture\n")
    local_history.write_text(
        json.dumps(
            {
                "session_id": "hdc16-local",
                "cwd": str(no_vcs),
                "timestamp": "2026-08-14T17:00:00Z",
                "role": "user",
                "message": "sanitized local history fixture",
            },
            separators=(",", ":"),
        )
        + "\n"
    )

    pi_directory = "--" + str(no_vcs).lstrip("/").replace("/", "-") + "--"
    external_history = (
        home
        / ".pi"
        / "agent"
        / "sessions"
        / pi_directory
        / "2026-08-14T17-01-00-000Z_019c0000-0000-7000-8000-000000000016.jsonl"
    )
    external_history.parent.mkdir(parents=True)
    external_history.write_text(
        "\n".join(
            (
                json.dumps(
                    {
                        "type": "session",
                        "version": 3,
                        "id": "019c0000-0000-7000-8000-000000000016",
                        "timestamp": "2026-08-14T17:01:00.000Z",
                        "cwd": str(no_vcs),
                    },
                    separators=(",", ":"),
                ),
                json.dumps(
                    {
                        "type": "message",
                        "id": "10000000",
                        "parentId": None,
                        "timestamp": "2026-08-14T17:01:01.000Z",
                        "message": {
                            "role": "user",
                            "content": [{"type": "text", "text": "sanitized Pi fixture"}],
                            "timestamp": 1786726861000,
                        },
                    },
                    separators=(",", ":"),
                ),
                json.dumps(
                    {
                        "type": "session_info",
                        "id": "10000001",
                        "parentId": "10000000",
                        "timestamp": "2026-08-14T17:01:02.000Z",
                        "name": "HDC-16 external smoke",
                    },
                    separators=(",", ":"),
                ),
            )
        )
        + "\n"
    )

    git_workspace = root / "workspaces" / "git"
    git_workspace.mkdir(parents=True)
    (git_workspace / "tracked.txt").write_text("tracked\n")
    git = shutil.which("git")
    if git is None:
        raise VerificationError("required smoke-test tool is unavailable: git")
    _run([git, "init", "-q", str(git_workspace)], env=os.environ.copy())
    _run([git, "-C", str(git_workspace), "add", "tracked.txt"], env=os.environ.copy())
    _run(
        [
            git,
            "-C",
            str(git_workspace),
            "-c",
            "user.name=HDC-16",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-q",
            "-m",
            "fixture",
        ],
        env=os.environ.copy(),
    )
    (git_workspace / "tracked.txt").write_text("modified\n")

    jj_workspace = root / "workspaces" / "jj"
    jj_workspace.mkdir(parents=True)
    (jj_workspace / "change.txt").write_text("sanitized jj fixture\n")
    (jj_workspace / ".jj" / "repo").mkdir(parents=True)
    (jj_workspace / ".jj" / "working_copy").mkdir()
    return no_vcs, git_workspace, jj_workspace, external_history


def _workspace_create(
    herdr: Path,
    session: str,
    env: dict[str, str],
    cwd: Path,
    label: str,
) -> tuple[str, str, str]:
    result = _herdr_json(
        herdr,
        session,
        env,
        "workspace",
        "create",
        "--cwd",
        str(cwd),
        "--label",
        label,
        "--focus",
    )
    workspace = result.get("workspace")
    tab = result.get("tab")
    pane = result.get("root_pane")
    if not all(isinstance(item, dict) for item in (workspace, tab, pane)):
        raise VerificationError("workspace creation response is incomplete")
    return str(workspace["workspace_id"]), str(tab["tab_id"]), str(pane["pane_id"])


def _panes(
    herdr: Path,
    session: str,
    env: dict[str, str],
    workspace_id: str,
) -> list[dict[str, Any]]:
    result = _herdr_json(
        herdr, session, env, "pane", "list", "--workspace", workspace_id
    )
    panes = result.get("panes")
    if not isinstance(panes, list) or not all(isinstance(pane, dict) for pane in panes):
        raise VerificationError("pane list response is invalid")
    return panes


def _plugin_panes(
    herdr: Path,
    session: str,
    env: dict[str, str],
    workspace_id: str,
    tab_id: str,
    origin_id: str,
) -> list[dict[str, Any]]:
    owned: list[dict[str, Any]] = []
    for pane in _panes(herdr, session, env, workspace_id):
        pane_id = pane.get("pane_id")
        if pane_id == origin_id or pane.get("tab_id") != tab_id or not isinstance(pane_id, str):
            continue
        result = _herdr_json(
            herdr,
            session,
            env,
            "plugin",
            "pane",
            "focus",
            pane_id,
            check=False,
        )
        plugin_pane = result.get("plugin_pane") if result else None
        if (
            isinstance(plugin_pane, dict)
            and plugin_pane.get("plugin_id") == release_tool.PLUGIN_ID
            and plugin_pane.get("entrypoint") == "dock"
        ):
            owned.append(pane)
    return owned


def _invoke_toggle(herdr: Path, session: str, env: dict[str, str]) -> None:
    result = _herdr_json(
        herdr,
        session,
        env,
        "plugin",
        "action",
        "invoke",
        "herdr-context.toggle",
    )
    log = result.get("log")
    log_id = log.get("log_id") if isinstance(log, dict) else None
    if not isinstance(log_id, str):
        raise VerificationError("plugin action did not return a log id")

    def action_finished() -> bool:
        listed = _herdr_json(
            herdr,
            session,
            env,
            "plugin",
            "log",
            "list",
            "--plugin",
            release_tool.PLUGIN_ID,
        )
        logs = listed.get("logs")
        if not isinstance(logs, list):
            raise VerificationError("plugin log response is invalid")
        current = next(
            (
                item
                for item in logs
                if isinstance(item, dict) and item.get("log_id") == log_id
            ),
            None,
        )
        if current is None or current.get("status") == "running":
            return False
        if current.get("status") != "succeeded":
            raise VerificationError(
                f"packaged toggle failed: {current.get('stderr', '')}"
            )
        return True

    _wait("packaged toggle completion", action_finished, DOCK_TIMEOUT)


def _wait_for_plugin_panes(
    herdr: Path,
    session: str,
    env: dict[str, str],
    workspace_id: str,
    tab_id: str,
    origin_id: str,
    count: int,
) -> list[dict[str, Any]]:
    def matching_panes() -> tuple[list[dict[str, Any]]] | None:
        panes = _plugin_panes(
            herdr, session, env, workspace_id, tab_id, origin_id
        )
        return (panes,) if len(panes) == count else None

    return _wait(
        f"{count} packaged dock pane(s)",
        matching_panes,
        DOCK_TIMEOUT,
    )[0]


def _assert_dock_geometry(
    herdr: Path,
    session: str,
    env: dict[str, str],
    pane_id: str,
) -> None:
    def geometry_is_ready() -> bool:
        result = _herdr_json(
            herdr, session, env, "pane", "layout", "--pane", pane_id
        )
        layout = result.get("layout")
        if not isinstance(layout, dict) or not isinstance(layout.get("area"), dict):
            raise VerificationError("pane layout response is invalid")
        area = layout["area"]
        entries = layout.get("panes")
        if not isinstance(entries, list):
            raise VerificationError("pane layout has no panes")
        entry = next(
            (
                item
                for item in entries
                if isinstance(item, dict) and item.get("pane_id") == pane_id
            ),
            None,
        )
        rect = entry.get("rect") if isinstance(entry, dict) else None
        if not isinstance(rect, dict) or rect.get("width") != 40:
            return False
        coordinates = (rect.get("x"), area.get("x"), area.get("width"))
        if not all(isinstance(value, int) for value in coordinates):
            return False
        return rect["x"] + rect["width"] == area["x"] + area["width"]

    _wait("40-column right-edge dock geometry", geometry_is_ready, DOCK_TIMEOUT)


def _pane_text(
    herdr: Path,
    session: str,
    env: dict[str, str],
    pane_id: str,
) -> str:
    completed = _run(
        _herdr_command(
            herdr,
            session,
            "pane",
            "read",
            pane_id,
            "--source",
            "visible",
            "--format",
            "text",
        ),
        env=env,
    )
    return completed.stdout


def _exercise_workspace(
    herdr: Path,
    session: str,
    env: dict[str, str],
    cwd: Path,
    label: str,
    expected_text: tuple[str, ...],
) -> tuple[str, str, str, str]:
    workspace_id, tab_id, origin_id = _workspace_create(
        herdr, session, env, cwd, label
    )
    _invoke_toggle(herdr, session, env)
    dock = _wait_for_plugin_panes(
        herdr, session, env, workspace_id, tab_id, origin_id, 1
    )[0]
    pane_id = str(dock["pane_id"])
    _assert_dock_geometry(herdr, session, env, pane_id)
    def files_contract() -> str:
        text = _pane_text(herdr, session, env, pane_id)
        if "Files" in text and all(expected in text for expected in expected_text):
            return text
        raise VerificationError(f"last pane text did not satisfy contract: {text!r}")

    _wait(
        f"Files contract for {label}",
        files_contract,
        DOCK_TIMEOUT,
    )
    return workspace_id, tab_id, origin_id, pane_id


def _runtime_smoke(
    herdr: Path,
    session: str,
    env: dict[str, str],
    package_root: Path,
    root: Path,
) -> None:
    no_vcs, git_workspace, jj_workspace, external_history = _write_fixtures(root, Path(env["HOME"]))
    fake_jj = _write_fake_jj(Path(env["PATH"]))
    log = (root / "server.log").open("w", encoding="utf-8")
    server = subprocess.Popen(
        _herdr_command(herdr, session, "server"),
        env=env,
        cwd=env["HERDR_CONTEXT_VERIFY_CWD"],
        stdin=subprocess.DEVNULL,
        stdout=log,
        stderr=subprocess.STDOUT,
        text=True,
        start_new_session=True,
    )
    try:
        _wait(
            "isolated Herdr server",
            lambda: _herdr_json(herdr, session, env, "workspace", "list", check=False),
            STARTUP_TIMEOUT,
        )
        workspace_id, tab_id, origin_id = _workspace_create(
            herdr, session, env, no_vcs, "HDC-16 no VCS"
        )

        _invoke_toggle(herdr, session, env)
        first_dock = _wait_for_plugin_panes(
            herdr, session, env, workspace_id, tab_id, origin_id, 1
        )[0]
        first_dock_id = str(first_dock["pane_id"])
        _assert_dock_geometry(herdr, session, env, first_dock_id)

        _herdr_json(
            herdr,
            session,
            env,
            "pane",
            "focus",
            "--direction",
            "left",
            "--pane",
            first_dock_id,
        )
        _invoke_toggle(herdr, session, env)
        focused = _wait(
            "dock focus toggle",
            lambda: next(
                (
                    pane
                    for pane in _panes(herdr, session, env, workspace_id)
                    if pane.get("pane_id") == first_dock_id and pane.get("focused") is True
                ),
                None,
            ),
            DOCK_TIMEOUT,
        )
        if focused.get("tab_id") != tab_id:
            raise VerificationError("focus toggle changed tabs")
        _invoke_toggle(herdr, session, env)
        _wait_for_plugin_panes(herdr, session, env, workspace_id, tab_id, origin_id, 0)

        _invoke_toggle(herdr, session, env)
        dock_id = str(
            _wait_for_plugin_panes(
                herdr, session, env, workspace_id, tab_id, origin_id, 1
            )[0]["pane_id"]
        )
        _wait(
            "local Files fixture",
            lambda: (
                text
                if "visible.txt" in (text := _pane_text(herdr, session, env, dock_id))
                else None
            ),
            DOCK_TIMEOUT,
        )
        _run(
            _herdr_command(herdr, session, "pane", "send-keys", dock_id, "TAB"),
            env=env,
        )
        conversation_text = _wait(
            "local and external Conversations fixtures",
            lambda: (
                text
                if "hdc16-local" in (text := _pane_text(herdr, session, env, dock_id))
                and ("pi" in text.lower() or "HDC-16 external smoke" in text)
                else None
            ),
            DOCK_TIMEOUT,
        )
        if "Conversations" not in conversation_text:
            raise VerificationError("Conversations view did not render")

        second = _herdr_json(
            herdr,
            session,
            env,
            "tab",
            "create",
            "--workspace",
            workspace_id,
            "--cwd",
            str(no_vcs),
            "--label",
            "second",
            "--focus",
        )
        second_tab = second.get("tab")
        second_pane = second.get("root_pane")
        if not isinstance(second_tab, dict) or not isinstance(second_pane, dict):
            raise VerificationError("second tab response is incomplete")
        second_tab_id = str(second_tab["tab_id"])
        second_origin_id = str(second_pane["pane_id"])
        _invoke_toggle(herdr, session, env)
        _wait_for_plugin_panes(
            herdr, session, env, workspace_id, second_tab_id, second_origin_id, 1
        )
        if len(_plugin_panes(herdr, session, env, workspace_id, tab_id, origin_id)) != 1:
            raise VerificationError("first tab lost its isolated dock")

        _exercise_workspace(
            herdr,
            session,
            env,
            git_workspace,
            "HDC-16 Git",
            ("M tracked.txt",),
        )
        _exercise_workspace(
            herdr,
            session,
            env,
            jj_workspace,
            "HDC-16 Jujutsu",
            ("M change.txt",),
        )
        fake_jj.unlink()
        _exercise_workspace(
            herdr,
            session,
            env,
            jj_workspace,
            "HDC-16 Jujutsu unavailable",
            ("change.txt", "VCS: Jujutsu executable is unavailable"),
        )

        fake_pi = Path(env["PATH"]) / "pi"
        fake_pi.write_text(
            "#!/bin/sh\n"
            "trap 'exit 0' INT TERM\n"
            "printf 'Pi HDC-16 sanitized live fixture\\n'\n"
            "while :; do sleep 1; done\n"
        )
        fake_pi.chmod(0o755)
        _herdr_json(herdr, session, env, "workspace", "focus", workspace_id)
        _herdr_json(herdr, session, env, "tab", "focus", tab_id)
        _herdr_json(
            herdr,
            session,
            env,
            "pane",
            "focus",
            "--direction",
            "left",
            "--pane",
            dock_id,
        )
        _run(
            _herdr_command(
                herdr,
                session,
                "agent",
                "start",
                "hdc16-pi",
                "--kind",
                "pi",
                "--pane",
                origin_id,
                "--",
                "--session",
                str(external_history),
            ),
            env=env,
        )
        socket_path = (
            Path(env["HERDR_CONFIG_PATH"]).parent
            / "sessions"
            / session
            / "herdr.sock"
        )
        _report_live_session(socket_path, origin_id, external_history)
        def live_agent_session() -> dict[str, Any]:
            agents = _herdr_json(herdr, session, env, "agent", "list")
            serialized = json.dumps(agents)
            if str(external_history) in serialized and "pi" in serialized.lower():
                return agents
            terminal = _pane_text(herdr, session, env, origin_id)
            raise VerificationError(
                f"agent session missing; terminal tail: {terminal[-512:]!r}"
            )

        _wait(
            "sanitized live agent session",
            live_agent_session,
            DOCK_TIMEOUT,
        )
        _invoke_toggle(herdr, session, env)
        _wait_for_plugin_panes(
            herdr, session, env, workspace_id, tab_id, origin_id, 1
        )
        _run(
            _herdr_command(herdr, session, "pane", "send-keys", dock_id, "r"),
            env=env,
        )
        def live_conversation() -> str:
            text = _pane_text(herdr, session, env, dock_id)
            session_id = external_history.stem.split("_", 1)[1]
            if "▾ pi (1)" in text and session_id in text:
                return text
            raise VerificationError(
                f"live conversation missing; pane tail: {text[-1024:]!r}"
            )

        _wait(
            "live Conversations fixture",
            live_conversation,
            DOCK_TIMEOUT,
        )

        if not package_root.joinpath("herdr-context").is_file():
            raise VerificationError("installed package disappeared during runtime smoke")
    finally:
        _run(
            _herdr_command(herdr, session, "server", "stop"),
            env=env,
            timeout=5,
            check=False,
        )
        try:
            server.wait(timeout=10)
        except subprocess.TimeoutExpired:
            server.terminate()
            try:
                server.wait(timeout=5)
            except subprocess.TimeoutExpired:
                server.kill()
                server.wait(timeout=5)
        log.close()
        if server.returncode not in (0, None):
            server_log = (root / "server.log").read_text(errors="replace")
            raise VerificationError(
                f"isolated Herdr server exited {server.returncode}: {server_log[-4096:]}"
            )


def _uninstall_with_server(
    herdr: Path,
    session: str,
    env: dict[str, str],
    uninstall: Path,
    root: Path,
) -> None:
    log_path = root / "uninstall-server.log"
    with log_path.open("w", encoding="utf-8") as log:
        server = subprocess.Popen(
            _herdr_command(herdr, session, "server"),
            env=env,
            cwd=env["HERDR_CONTEXT_VERIFY_CWD"],
            stdin=subprocess.DEVNULL,
            stdout=log,
            stderr=subprocess.STDOUT,
            text=True,
            start_new_session=True,
        )
        try:
            _wait(
                "Herdr server for uninstall",
                lambda: _herdr_json(
                    herdr, session, env, "workspace", "list", check=False
                ),
                STARTUP_TIMEOUT,
            )
            _run([str(uninstall)], env=env)
        finally:
            _run(
                _herdr_command(herdr, session, "server", "stop"),
                env=env,
                timeout=5,
                check=False,
            )
            try:
                server.wait(timeout=10)
            except subprocess.TimeoutExpired:
                server.terminate()
                server.wait(timeout=5)
    if server.returncode != 0:
        raise VerificationError(
            f"uninstall Herdr server exited {server.returncode}: "
            f"{log_path.read_text(errors='replace')[-4096:]}"
        )


def verify_clean_install(
    archive: Path,
    checksum: Path,
    herdr: Path,
    *,
    runtime_smoke: bool,
) -> None:
    archive = archive.resolve()
    checksum = checksum.resolve()
    herdr = herdr.resolve()
    metadata = release_tool.verify_archive(archive, checksum)
    if metadata.target != _native_target():
        raise VerificationError(
            f"archive target {metadata.target} does not match host {_native_target()}"
        )
    version_output = _run([str(herdr), "--version"], env=os.environ.copy()).stdout.strip()
    match = re.fullmatch(r"herdr ([0-9]+\.[0-9]+\.[0-9]+)", version_output)
    if match is None or _version_tuple(match.group(1)) < _version_tuple(release_tool.MIN_HERDR_VERSION):
        raise VerificationError(
            f"Herdr {release_tool.MIN_HERDR_VERSION} or newer is required; got {version_output!r}"
        )

    with tempfile.TemporaryDirectory(prefix="herdr-context-clean-install-") as temporary:
        root = Path(temporary)
        corrupt_root = root / "corrupt"
        corrupt_root.mkdir()
        _verify_corrupt_rejection(archive, checksum, corrupt_root)
        extraction = root / "artifact"
        extraction.mkdir()
        artifact_root = _extract_verified(archive, extraction)
        home = root / "home"
        home.mkdir()
        session = f"hdc16-{os.getpid()}"
        runtime_path = _isolated_runtime_path(root / "runtime-bin")
        shell = shutil.which("sh")
        if shell is None:
            raise VerificationError("required smoke-test shell is unavailable")
        env = {
            "HOME": str(home),
            "PATH": runtime_path,
            "SHELL": str(Path(shell).resolve()),
            "TERM": "xterm-256color",
            "LC_ALL": "C",
            "XDG_CONFIG_HOME": str(root / "config"),
            "XDG_DATA_HOME": str(root / "data"),
            "XDG_STATE_HOME": str(root / "state"),
            "XDG_CACHE_HOME": str(root / "cache"),
            "HERDR_CONFIG_PATH": str(root / "config" / "herdr" / "config.toml"),
            "HERDR_CONTEXT_INSTALL_DIR": str(root / "plugins" / "herdr-context"),
            "HERDR_CONTEXT_VERIFY_CWD": str(root),
            "HERDR_BIN": str(herdr),
            "HERDR_SESSION": session,
        }
        install = artifact_root / "install.sh"
        uninstall = artifact_root / "uninstall.sh"
        _run([str(install)], env=env)
        install_root = Path(env["HERDR_CONTEXT_INSTALL_DIR"])
        if not install_root.joinpath("herdr-context").is_file():
            raise VerificationError("installer did not place the packaged binary")

        plugins = _herdr_json(herdr, session, env, "plugin", "list", "--json").get("plugins")
        if not isinstance(plugins, list) or len(plugins) != 1:
            raise VerificationError(f"isolated registry is not clean: {plugins!r}")
        plugin = plugins[0]
        if not isinstance(plugin, dict) or plugin.get("version") != metadata.version:
            raise VerificationError("installed plugin version does not match the archive")
        if plugin.get("plugin_root") != str(install_root):
            raise VerificationError("Herdr registered a path outside the isolated plugin directory")

        config_dir = _run(
            _herdr_command(herdr, session, "plugin", "config-dir", release_tool.PLUGIN_ID),
            env=env,
        ).stdout.strip()
        config_path = Path(config_dir) / "config.toml"
        config_path.parent.mkdir(parents=True, exist_ok=True)
        config_path.write_text("[dock]\ninitial_width = 40\n")
        state_sentinel = root / "state" / "herdr" / "plugins" / release_tool.PLUGIN_ID / "keep"
        state_sentinel.parent.mkdir(parents=True, exist_ok=True)
        state_sentinel.write_text("state survives upgrade and uninstall\n")
        project_sentinel = root / "project-history" / "keep.jsonl"
        project_sentinel.parent.mkdir(parents=True)
        project_sentinel.write_text("history survives upgrade and uninstall\n")

        binary_env = env.copy()
        binary_env.update(
            {
                "HERDR_BIN_PATH": str(herdr),
                "HERDR_PLUGIN_ROOT": str(install_root),
                "HERDR_PLUGIN_CONFIG_DIR": str(config_path.parent),
                "HERDR_PLUGIN_STATE_DIR": str(state_sentinel.parent),
                "HERDR_WORKSPACE_ID": "clean-workspace",
                "HERDR_TAB_ID": "clean-tab",
                "HERDR_PANE_ID": "clean-pane",
                "HERDR_PLUGIN_CONTEXT_JSON": json.dumps(
                    {
                        "workspace_id": "clean-workspace",
                        "tab_id": "clean-tab",
                        "focused_pane_id": "clean-pane",
                        "focused_pane_cwd": str(root),
                    },
                    separators=(",", ":"),
                ),
            }
        )
        _run([str(install_root / "herdr-context")], env=binary_env)
        _run([str(install)], env=env)
        if not config_path.is_file() or not state_sentinel.is_file() or not project_sentinel.is_file():
            raise VerificationError("package upgrade touched config, state, or history")

        if runtime_smoke:
            _runtime_smoke(herdr, session, env, install_root, root)
        _uninstall_with_server(herdr, session, env, uninstall, root)
        if install_root.exists():
            raise VerificationError("uninstall left plugin-owned installation files")
        if not config_path.is_file() or not state_sentinel.is_file() or not project_sentinel.is_file():
            raise VerificationError("uninstall touched config, state, or history")
        remaining_plugins = _herdr_json(
            herdr, session, env, "plugin", "list", "--json"
        ).get("plugins")
        if remaining_plugins != []:
            raise VerificationError(f"uninstall left plugin registration: {remaining_plugins!r}")


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description="Verify a clean packaged herdr-context install")
    parser.add_argument("archive", type=Path)
    parser.add_argument("checksum", type=Path)
    parser.add_argument("--herdr-bin", type=Path, default=Path(shutil.which("herdr") or "herdr"))
    parser.add_argument("--skip-runtime-smoke", action="store_true")
    return parser


def main(arguments: list[str] | None = None) -> int:
    args = _parser().parse_args(arguments)
    try:
        verify_clean_install(
            args.archive,
            args.checksum,
            args.herdr_bin,
            runtime_smoke=not args.skip_runtime_smoke,
        )
    except (VerificationError, release_tool.ReleaseError) as error:
        print(f"verify-clean-install: {error}", file=sys.stderr)
        return 1
    print("verify-clean-install: PASS")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
