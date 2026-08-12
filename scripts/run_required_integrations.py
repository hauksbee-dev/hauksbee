#!/usr/bin/env python3
"""Run the small co-simulation tier that release evidence is required to earn."""

from __future__ import annotations

import argparse
import ctypes
import json
import os
import re
import secrets
import signal
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass
from pathlib import Path, PureWindowsPath


@dataclass(frozen=True)
class Gate:
    """One named, externally-backed integration proof."""

    name: str
    command: tuple[str, ...]
    expected_test: str
    timeout_seconds: int = 600


GATES = (
    Gate(
        name="renode-rp2040-adc",
        command=(
            "cargo",
            "test",
            "-p",
            "hauksbee-mcu",
            "--no-default-features",
            "--features",
            "renode",
            "--test",
            "renode_rp2040_adc",
            "rp2040_adc_injection_reaches_firmware",
            "--",
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ),
        expected_test="rp2040_adc_injection_reaches_firmware",
    ),
    Gate(
        name="qemu-xtensa-i2c",
        command=(
            "cargo",
            "test",
            "-p",
            "hauksbee-engine",
            "--no-default-features",
            "--features",
            "qemu",
            "--test",
            "i2c_sensor_cosim_qemu",
            "esp32_i2c_firmware_drives_gpio_from_temperature",
            "--",
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ),
        expected_test="esp32_i2c_firmware_drives_gpio_from_temperature",
    ),
    Gate(
        name="qemu-riscv32-circuit",
        command=(
            "cargo",
            "test",
            "-p",
            "hauksbee-engine",
            "--no-default-features",
            "--features",
            "qemu",
            "--test",
            "esp32_qemu_cosim",
            "esp32c3_full_cosim_through_solved_circuit",
            "--",
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ),
        expected_test="esp32c3_full_cosim_through_solved_circuit",
    ),
)


def evaluate_result(gate: Gate, returncode: int, output: str) -> list[str]:
    """Return every reason this command failed to prove its named integration."""

    problems: list[str] = []
    if returncode != 0:
        problems.append(f"{gate.name}: cargo exited with status {returncode}")
    if "SKIP:" in output:
        skipped = [line.strip() for line in output.splitlines() if "SKIP:" in line]
        problems.append(f"{gate.name}: reported SKIP: {'; '.join(skipped)}")

    # `--nocapture` may place the test's own evidence between Cargo's
    # `test name ...` prefix and libtest's eventual `ok`. Every command above
    # filters to exactly one named test, so require both that exact start and a
    # one-test green summary rather than assuming `... ok` stays on one line.
    named_start = re.compile(
        rf"^test {re.escape(gate.expected_test)} \.\.\.", re.MULTILINE
    )
    one_pass_summary = re.compile(
        r"^test result: ok\. 1 passed; 0 failed(?:;|$)", re.MULTILINE
    )
    if named_start.search(output) is None or one_pass_summary.search(output) is None:
        problems.append(
            f"{gate.name}: did not observe named test {gate.expected_test!r} passing"
        )
    return problems


def _text(output: str | bytes | None) -> str:
    """Normalize subprocess timeout capture, which is bytes even in text mode."""

    if output is None:
        return ""
    if isinstance(output, bytes):
        return output.decode("utf-8", errors="replace")
    return output


def _merge_capture(earlier: str | bytes | None, final: str | bytes | None) -> str:
    """Keep timeout output once when communicate returns overlapping captures."""

    earlier_text = _text(earlier)
    final_text = _text(final)
    if final_text.startswith(earlier_text):
        return final_text
    if earlier_text.startswith(final_text):
        return earlier_text
    return earlier_text + final_text


RUN_TOKEN_ENV = "HAUKSBEE_REQUIRED_PROCESS_TOKEN"


def _darwin_process_arguments(pid: int) -> bytes:
    """Return argv+environment for one same-user Darwin process."""

    libc = ctypes.CDLL(None, use_errno=True)
    mib = (ctypes.c_int * 3)(1, 49, pid)  # CTL_KERN, KERN_PROCARGS2, pid
    size = ctypes.c_size_t()
    if libc.sysctl(mib, 3, None, ctypes.byref(size), None, 0) != 0:
        return b""
    if size.value <= 0 or size.value > 16 * 1024 * 1024:
        return b""
    buffer = ctypes.create_string_buffer(size.value)
    if libc.sysctl(mib, 3, buffer, ctypes.byref(size), None, 0) != 0:
        return b""
    return buffer.raw[: size.value]


def _process_has_run_token(pid: int, run_token: str) -> bool:
    """Recognize descendants after they reparent or create a new session."""

    assignment = f"{RUN_TOKEN_ENV}={run_token}".encode()
    try:
        if sys.platform.startswith("linux"):
            environment = Path(f"/proc/{pid}/environ").read_bytes()
        elif sys.platform == "darwin":
            environment = _darwin_process_arguments(pid)
        else:
            return False
    except (OSError, PermissionError):
        return False
    return assignment in environment.split(b"\0")


def _process_table(
    run_token: str | None = None,
) -> list[tuple[int, int, int, int, bool]]:
    """Return a POSIX pid/ppid/pgid/session snapshot or fail closed."""

    if os.name != "posix":
        return []
    try:
        table = subprocess.run(
            ("ps", "-axo", "pid=,ppid=,pgid=,uid="),
            text=True,
            capture_output=True,
            check=True,
        ).stdout
    except (OSError, subprocess.SubprocessError):
        raise RuntimeError(
            "cannot enumerate POSIX descendants; refusing an uncontained integration run"
        )

    rows = []
    for line in table.splitlines():
        try:
            pid, parent, group, uid = map(int, line.split())
        except ValueError:
            continue
        try:
            session = os.getsid(pid)
        except ProcessLookupError:
            continue
        except PermissionError:
            # Other-user processes are never descendants of this runner. Keep
            # them in the ancestry table but make them ineligible for session
            # ownership rather than weakening cleanup for our own children.
            session = -1
        tagged = bool(
            run_token and uid == os.geteuid() and _process_has_run_token(pid, run_token)
        )
        rows.append((pid, parent, group, session, tagged))
    return rows


def _direct_children(parent_pid: int) -> set[int]:
    candidates = {
        pid for pid, parent, _, _, _ in _process_table() if parent == parent_pid
    }
    live = set()
    for pid in candidates:
        try:
            os.kill(pid, 0)
        except ProcessLookupError:
            continue
        live.add(pid)
    return live


def _enable_child_subreaper() -> None:
    """On Linux, adopt daemonized grandchildren so timeout cleanup owns them."""

    if not sys.platform.startswith("linux"):
        return
    # PR_SET_CHILD_SUBREAPER is process-scoped and inherited by neither fork nor
    # exec. Failure is fatal: silently continuing would make a release job able
    # to leave a double-forked emulator behind after reporting a bounded timeout.
    libc = ctypes.CDLL(None, use_errno=True)
    if libc.prctl(36, 1, 0, 0, 0) != 0:
        error = ctypes.get_errno()
        raise OSError(error, os.strerror(error))


def _owned_processes(
    root_pid: int, baseline_runner_children: set[int], run_token: str
) -> tuple[set[int], set[int]]:
    """Snapshot groups/PIDs rooted at cargo or newly adopted by this runner."""

    groups: set[int] = set()
    pids: set[int] = {root_pid}
    rows = _process_table(run_token)

    children: dict[int, list[tuple[int, int]]] = {}
    for pid, parent, group, _, _ in rows:
        children.setdefault(parent, []).append((pid, group))

    adopted = {
        pid
        for pid, parent, _, _, _ in rows
        if parent == os.getpid()
        and pid not in baseline_runner_children
        and pid != root_pid
    }
    session_members = {pid for pid, _, _, session, _ in rows if session == root_pid}
    tagged_members = {pid for pid, _, _, _, tagged in rows if tagged}
    group_by_pid = {pid: group for pid, _, group, _, _ in rows}
    if root_pid not in group_by_pid:
        try:
            os.kill(root_pid, 0)
        except ProcessLookupError:
            pass
        else:
            # start_new_session=True makes the command PID its initial PGID.
            groups.add(root_pid)
    pending = [root_pid, *adopted, *session_members, *tagged_members]
    seen: set[int] = set()
    while pending:
        parent = pending.pop()
        if parent in seen:
            continue
        seen.add(parent)
        pids.add(parent)
        if parent in group_by_pid:
            groups.add(group_by_pid[parent])
        for pid, group in children.get(parent, ()):
            if pid in seen:
                continue
            pending.append(pid)
            groups.add(group)
    # Never signal the required-integration runner's own process group even if
    # a corrupt process table somehow attributes it below the child.
    groups.discard(os.getpgrp())
    return groups, pids


def _signal_process_groups(
    groups: set[int], root_group: int, sig: signal.Signals
) -> None:
    """Signal descendant groups before the cargo group that parents them."""

    ordered = sorted(groups - {root_group})
    if root_group in groups:
        ordered.append(root_group)
    for group in ordered:
        try:
            os.killpg(group, sig)
        except ProcessLookupError:
            pass


def _signal_known_root_group(
    process: subprocess.Popen[str], sig: signal.Signals
) -> None:
    """Best-effort fallback that never depends on process enumeration."""

    try:
        os.killpg(process.pid, sig)
    except (ProcessLookupError, PermissionError):
        pass


def _known_root_group_is_live(process: subprocess.Popen[str]) -> bool:
    try:
        os.killpg(process.pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    return True


def _stop_known_root_group(process: subprocess.Popen[str]) -> tuple[str, bool]:
    """Terminate the session root when descendant enumeration is unavailable."""

    _signal_known_root_group(process, signal.SIGTERM)
    killed = False
    try:
        output, _ = process.communicate(timeout=1)
    except subprocess.TimeoutExpired as error:
        _signal_known_root_group(process, signal.SIGKILL)
        killed = True
        output, _ = process.communicate()
        return _merge_capture(error.output, output), killed
    if _known_root_group_is_live(process):
        _signal_known_root_group(process, signal.SIGKILL)
        killed = True
    return _text(output), killed


def _live_process_groups(groups: set[int]) -> set[int]:
    live = set()
    for group in groups:
        try:
            os.killpg(group, 0)
        except ProcessLookupError:
            continue
        except PermissionError:
            # It still exists; inability to signal it is a cleanup failure, not
            # evidence that the emulator exited.
            pass
        live.add(group)
    return live


def _reap_owned_children(pids: set[int], *, wait: bool = False) -> None:
    if not sys.platform.startswith("linux"):
        return
    for pid in pids:
        if pid == os.getpid():
            continue
        try:
            os.waitpid(pid, 0 if wait else os.WNOHANG)
        except (ChildProcessError, ProcessLookupError):
            pass


def _wait_for_process_groups(
    groups: set[int], pids: set[int], timeout_seconds: float
) -> bool:
    deadline = time.monotonic() + timeout_seconds
    while True:
        _reap_owned_children(pids)
        if not _live_process_groups(groups):
            return True
        if time.monotonic() >= deadline:
            return False
        time.sleep(0.05)


def _audit_completed_command(
    process: subprocess.Popen[str], baseline_runner_children: set[int], run_token: str
) -> tuple[bool, bool]:
    """Terminate any group a completed command left behind and prove it exited."""

    try:
        groups, pids = _owned_processes(
            process.pid, baseline_runner_children, run_token
        )
    except RuntimeError:
        _signal_known_root_group(process, signal.SIGTERM)
        killed = False
        if _known_root_group_is_live(process):
            _signal_known_root_group(process, signal.SIGKILL)
            killed = True
        # The known session is stopped, but detached groups could not be
        # enumerated. Evidence must fail because cleanup cannot be proved.
        return False, killed
    live = _live_process_groups(groups)
    if not live:
        _reap_owned_children(pids)
        return True, False

    _signal_process_groups(live, process.pid, signal.SIGTERM)
    if _wait_for_process_groups(live, pids, 1.0):
        return True, False

    live = _live_process_groups(live)
    _signal_process_groups(live, process.pid, signal.SIGKILL)
    return _wait_for_process_groups(live, pids, 2.0), True


def _stop_process_group(
    process: subprocess.Popen[str], baseline_runner_children: set[int], run_token: str
) -> tuple[str, bool, bool]:
    """Stop cargo/emulator groups; return output, KILL use, and cleanup proof."""

    try:
        groups, pids = _owned_processes(
            process.pid, baseline_runner_children, run_token
        )
    except RuntimeError:
        output, killed = _stop_known_root_group(process)
        return output, killed, False
    _signal_process_groups(groups, process.pid, signal.SIGTERM)
    try:
        output, _ = process.communicate(timeout=5)
        # The root can double-fork between the first snapshot and TERM. Once
        # communicate confirms it exited, every surviving orphan has been
        # adopted by this Linux subreaper and is visible in this second scan.
        try:
            later_groups, later_pids = _owned_processes(
                process.pid, baseline_runner_children, run_token
            )
        except RuntimeError:
            _signal_known_root_group(process, signal.SIGKILL)
            return _text(output), True, False
        groups.update(later_groups)
        pids.update(later_pids)
        live_descendants = _live_process_groups(groups)
        if live_descendants:
            _signal_process_groups(live_descendants, process.pid, signal.SIGKILL)
            clean = _wait_for_process_groups(live_descendants, pids, 2.0)
            return _text(output), True, clean
        _reap_owned_children(pids)
        return _text(output), False, True
    except subprocess.TimeoutExpired as error:
        # Retain the first snapshot: cargo commonly exits on TERM while Renode
        # or QEMU keeps a separately-created process group alive and becomes
        # reparented before this second lookup can see it.
        try:
            later_groups, later_pids = _owned_processes(
                process.pid, baseline_runner_children, run_token
            )
        except RuntimeError:
            _signal_known_root_group(process, signal.SIGKILL)
            output, _ = process.communicate()
            return _merge_capture(error.output, output), True, False
        groups.update(later_groups)
        pids.update(later_pids)
        _signal_process_groups(groups, process.pid, signal.SIGKILL)
        output, _ = process.communicate()
        remaining = _live_process_groups(groups)
        clean = _wait_for_process_groups(remaining, pids, 2.0)
        return _merge_capture(error.output, output), True, clean


def run_command(
    command: tuple[str, ...],
    repo: Path,
    *,
    timeout_seconds: int = 600,
    env_updates: dict[str, str] | None = None,
) -> subprocess.CompletedProcess[str]:
    env = os.environ.copy()
    # Stable evidence: GitHub's global CARGO_TERM_COLOR=always otherwise places
    # ANSI escapes inside the exact pass line this contract validates.
    env["CARGO_TERM_COLOR"] = "never"
    if env_updates:
        env.update(env_updates)
    run_token = secrets.token_hex(32)
    env[RUN_TOKEN_ENV] = run_token
    _enable_child_subreaper()
    baseline_runner_children = _direct_children(os.getpid())
    process = subprocess.Popen(
        command,
        cwd=repo,
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        start_new_session=True,
    )
    try:
        output, _ = process.communicate(timeout=timeout_seconds)
        clean, killed = _audit_completed_command(
            process, baseline_runner_children, run_token
        )
        if killed:
            output += (
                "\nREQUIRED INTEGRATION CLEANUP: a completed command left a "
                "descendant process group; it was terminated with SIGKILL\n"
            )
        if not clean:
            output += (
                "\nREQUIRED INTEGRATION CLEANUP FAILED: descendant process "
                "groups remain after SIGKILL\n"
            )
            return subprocess.CompletedProcess(command, 125, output)
        return subprocess.CompletedProcess(command, process.returncode, output)
    except subprocess.TimeoutExpired:
        output, killed, clean = _stop_process_group(
            process, baseline_runner_children, run_token
        )
        termination = "SIGTERM then SIGKILL" if killed else "SIGTERM"
        output += (
            f"\nREQUIRED INTEGRATION TIMEOUT: exceeded {timeout_seconds}s; "
            f"cargo and its emulator process group were terminated with {termination}\n"
        )
        if not clean:
            output += (
                "REQUIRED INTEGRATION CLEANUP FAILED: descendant process groups "
                "remain after timeout cleanup\n"
            )
            return subprocess.CompletedProcess(command, 125, output)
        return subprocess.CompletedProcess(command, 124, output)
    except KeyboardInterrupt:
        _stop_process_group(process, baseline_runner_children, run_token)
        raise


def _git_sha(repo: Path) -> str:
    result = subprocess.run(
        ("git", "rev-parse", "HEAD"),
        cwd=repo,
        text=True,
        capture_output=True,
        check=True,
    )
    return result.stdout.strip()


REQUIRED_BACKEND_KEYS = (
    "HAUKSBEE_RENODE",
    "HAUKSBEE_QEMU_XTENSA",
    "HAUKSBEE_QEMU_RISCV32",
)

WINDOWS_BACKEND_CONTRACT = {
    "HAUKSBEE_RENODE": (
        "renode-portable",
        "Renode.exe",
        ("d09b7934cfd560cd06bde8f131ef78f521f10d423d5aac6096f2a583224aeb3e",),
        "895fddb36f65237af5a47928e49984cf1e1992e27e0d37546b3b8ea29ad57385",
        "3b12f1dd7b613cd9b73994a985fcd77107f471c352c52b4f3f2ff1528d4e7e8d",
    ),
    "HAUKSBEE_QEMU_XTENSA": (
        ".hauksbee-qemu-esp\\qemu\\bin",
        "qemu-system-xtensa.exe",
        (
            "3c483d77f5350a568df1faf4d8dbc82c95d6bc2b826d0d4be910485e0a68ca2a",
            "697aa4800a1f52be0b1693b30e22a684f7ea93c46c489e619384cae7b0e9b87b",
        ),
        "7716f734130a20193ab45a4c14581918822e5ae684eb5cf3073b9429bee29825",
        "4f02f4495f50ddf3baed71de29192932bd09053f0a1df498b854e0f5be0d8171",
    ),
    "HAUKSBEE_QEMU_RISCV32": (
        ".hauksbee-qemu-esp\\qemu\\bin",
        "qemu-system-riscv32.exe",
        (
            "3c483d77f5350a568df1faf4d8dbc82c95d6bc2b826d0d4be910485e0a68ca2a",
            "697aa4800a1f52be0b1693b30e22a684f7ea93c46c489e619384cae7b0e9b87b",
        ),
        "ec900387a3f7b54800d4690db575b86162769add55aa3b09056a943b29ec6644",
        "4f02f4495f50ddf3baed71de29192932bd09053f0a1df498b854e0f5be0d8171",
    ),
}


def _verified_backend_paths(
    output: str, required_root: Path | None = None
) -> tuple[dict[str, str], list[str]]:
    """Parse exact executable paths emitted by the pinned simulator preflight."""

    paths: dict[str, str] = {}
    problems: list[str] = []
    prefix = "REQUIRED_BACKEND_PATH "
    for line in output.splitlines():
        if not line.startswith(prefix):
            continue
        assignment = line.removeprefix(prefix)
        key, separator, value = assignment.partition("=")
        if not separator or key not in REQUIRED_BACKEND_KEYS or not value:
            problems.append(f"invalid required backend path record: {line!r}")
            continue
        if key in paths:
            problems.append(f"duplicate required backend path record for {key}")
            continue
        candidate = Path(value)
        if not candidate.is_absolute():
            problems.append(f"required backend path for {key} is not absolute: {value}")
            continue
        if required_root is not None:
            try:
                verified_root = required_root.resolve(strict=True)
                resolved = candidate.resolve(strict=True)
                resolved.relative_to(verified_root)
            except (OSError, ValueError) as error:
                problems.append(
                    f"required backend path for {key} is outside the runner-owned "
                    f"root or unavailable: {value}: {error}"
                )
                continue
            if not resolved.is_file() or not os.access(resolved, os.X_OK):
                problems.append(
                    f"required backend path for {key} is not an executable regular file: "
                    f"{resolved}"
                )
                continue
            value = str(resolved)
        paths[key] = value
    missing = sorted(set(REQUIRED_BACKEND_KEYS) - paths.keys())
    if missing:
        problems.append(
            "required simulator preflight did not return exact paths for: "
            + ", ".join(missing)
        )
    return paths, problems


def _verify_evidence(
    path: Path, expected_sha: str, expected_platform: str | None = None
) -> list[str]:
    problems: list[str] = []
    try:
        # Windows PowerShell 5.1's `Set-Content -Encoding utf8` prepends a BOM;
        # `utf-8-sig` accepts both that native artifact and BOM-less UTF-8.
        document = json.loads(path.read_text(encoding="utf-8-sig"))
    except (OSError, json.JSONDecodeError) as error:
        return [f"cannot read required integration evidence {path}: {error}"]
    if document.get("schema_version") != 1:
        problems.append("required integration evidence has unsupported schema_version")
    if document.get("commit_sha") != expected_sha:
        problems.append(
            "required integration evidence commit SHA mismatch: "
            f"expected {expected_sha}, found {document.get('commit_sha')!r}"
        )
    if expected_platform is not None and document.get("platform") != expected_platform:
        problems.append(
            "required integration evidence platform mismatch: "
            f"expected {expected_platform}, found {document.get('platform')!r}"
        )
    if expected_platform == "windows-x86_64":
        backends = document.get("backends")
        if not isinstance(backends, dict) or set(backends) != set(
            WINDOWS_BACKEND_CONTRACT
        ):
            problems.append(
                "Windows required integration evidence does not contain the exact backend set"
            )
        else:
            digest_pattern = re.compile(r"^[0-9a-f]{64}$")
            for key, (
                parent_fragment,
                filename,
                archive_sha256s,
                expected_artifact_sha256,
                expected_install_tree_sha256,
            ) in WINDOWS_BACKEND_CONTRACT.items():
                row = backends.get(key)
                if not isinstance(row, dict):
                    problems.append(
                        f"Windows backend evidence for {key} is not an object"
                    )
                    continue
                raw_path = row.get("path")
                parsed = (
                    PureWindowsPath(raw_path) if isinstance(raw_path, str) else None
                )
                path_parts = (
                    tuple(part.lower() for part in parsed.parts) if parsed else ()
                )
                fragment_parts = tuple(
                    part.lower() for part in PureWindowsPath(parent_fragment).parts
                )
                contains_fragment = any(
                    path_parts[index : index + len(fragment_parts)] == fragment_parts
                    for index in range(len(path_parts) - len(fragment_parts) + 1)
                )
                if (
                    parsed is None
                    or not parsed.is_absolute()
                    or not re.fullmatch(r"[A-Za-z]:", parsed.drive)
                    or parsed.name.lower() != filename.lower()
                    or not contains_fragment
                ):
                    problems.append(
                        f"Windows backend evidence for {key} has an invalid exact path"
                    )
                artifact_sha256 = row.get("artifact_sha256")
                if (
                    not isinstance(artifact_sha256, str)
                    or not digest_pattern.fullmatch(artifact_sha256)
                    or artifact_sha256 != expected_artifact_sha256
                ):
                    problems.append(
                        f"Windows backend evidence for {key} has the wrong artifact SHA-256"
                    )
                install_tree_sha256 = row.get("install_tree_sha256")
                if (
                    not isinstance(install_tree_sha256, str)
                    or not digest_pattern.fullmatch(install_tree_sha256)
                    or install_tree_sha256 != expected_install_tree_sha256
                ):
                    problems.append(
                        f"Windows backend evidence for {key} has the wrong install-tree SHA-256"
                    )
                if row.get("archive_sha256s") != list(archive_sha256s):
                    problems.append(
                        f"Windows backend evidence for {key} has the wrong pinned archive SHA-256 set"
                    )
    expected_gates = [gate.name for gate in GATES]
    if document.get("gates") != expected_gates:
        problems.append(
            "required integration evidence does not contain the complete ordered gate set"
        )
    return problems


def _write_evidence(path: Path, commit_sha: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f".{path.name}.tmp-{os.getpid()}")
    temporary.write_text(
        json.dumps(
            {
                "schema_version": 1,
                "commit_sha": commit_sha,
                "gates": [gate.name for gate in GATES],
            },
            indent=2,
            sort_keys=True,
        )
        + "\n"
    )
    os.replace(temporary, path)


def _run_required_gates(
    args: argparse.Namespace, repo: Path, commit_sha: str, required_root: Path
) -> int:
    preflight = run_command(
        (
            str(repo / "scripts/install-sims.sh"),
            "--require-pinned",
            "--emit-required-paths",
        ),
        repo,
        env_updates={"HAUKSBEE_REQUIRED_RUN_ROOT": str(required_root)},
    )
    sys.stdout.write(preflight.stdout)
    if preflight.returncode != 0:
        print(
            "required integrations: simulator preflight failed; no release evidence was earned",
            file=sys.stderr,
        )
        return 1
    verified_backends, path_problems = _verified_backend_paths(
        preflight.stdout, required_root
    )
    if path_problems:
        for problem in path_problems:
            print(f"required integrations: {problem}", file=sys.stderr)
        return 1

    failures: list[str] = []
    for gate in GATES:
        print(f"\n==> required integration: {gate.name}", flush=True)
        result = run_command(
            gate.command,
            repo,
            timeout_seconds=gate.timeout_seconds,
            env_updates=verified_backends,
        )
        sys.stdout.write(result.stdout)
        failures.extend(evaluate_result(gate, result.returncode, result.stdout))

    if failures:
        print("\nrequired integration evidence FAILED:", file=sys.stderr)
        for failure in failures:
            print(f"  - {failure}", file=sys.stderr)
        return 1

    print(f"\nrequired integration evidence: {len(GATES)}/{len(GATES)} gates passed")
    if args.evidence_out is not None:
        _write_evidence(args.evidence_out, commit_sha)
        print(f"required integration evidence retained at {args.evidence_out}")
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--expected-sha")
    parser.add_argument("--evidence-out", type=Path)
    parser.add_argument("--verify-evidence", type=Path)
    parser.add_argument("--expected-platform")
    args = parser.parse_args(argv)

    if args.verify_evidence is not None:
        if not args.expected_sha:
            parser.error("--verify-evidence requires --expected-sha")
        problems = _verify_evidence(
            args.verify_evidence, args.expected_sha, args.expected_platform
        )
        if problems:
            for problem in problems:
                print(problem, file=sys.stderr)
            return 1
        print(
            f"required integration evidence matches release commit {args.expected_sha}"
        )
        return 0

    repo = Path(__file__).resolve().parent.parent
    commit_sha = _git_sha(repo)
    if args.expected_sha and commit_sha != args.expected_sha:
        print(
            "required integrations: checkout commit SHA mismatch: "
            f"expected {args.expected_sha}, found {commit_sha}",
            file=sys.stderr,
        )
        return 1
    run_base = Path(os.environ.get("RUNNER_TEMP", tempfile.gettempdir())).resolve()
    run_base.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(
        prefix="hauksbee-required-sims.", dir=run_base
    ) as raw_required_root:
        required_root = Path(raw_required_root).resolve(strict=True)
        return _run_required_gates(args, repo, commit_sha, required_root)


if __name__ == "__main__":
    raise SystemExit(main())
