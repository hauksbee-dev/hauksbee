"""Production orchestration and retained evidence for release board gates."""

from __future__ import annotations

import hashlib
import json
import os
import re
import subprocess
from datetime import datetime, timezone
from pathlib import Path
from typing import Callable

from qc.unseen_boards import (
    Candidate,
    HistoryError,
    SelectionError,
    _board_record,
    _parse_history,
    candidate_pool_digest,
    discover_candidates,
    load_history,
    materialize_candidate,
    reserve_iteration,
)

_REPOSITORY = Path(__file__).resolve().parent.parent
CANONICAL_HISTORY = _REPOSITORY / "qc/evidence/unseen-external-history.jsonl"
CANONICAL_EVIDENCE_DIR = _REPOSITORY / "qc/evidence/runs"
RELEASE_SCRATCH = _REPOSITORY / "qc/results/release-gates"


def append_iteration_result(
    history_path: Path,
    *,
    iteration_id: str,
    status: str,
    recorded_at: str,
    evidence_sha256: str,
    evidence_file: str,
    tool_commit: str,
) -> dict:
    """Append the terminal evidence record for a reserved iteration."""

    if status not in {"completed", "failed"}:
        raise HistoryError(f"invalid terminal status {status!r}")
    if re.fullmatch(r"[0-9a-f]{64}", evidence_sha256) is None:
        raise HistoryError("evidence_sha256 must be a lowercase SHA-256 digest")
    if re.fullmatch(r"[0-9a-f]{40}", tool_commit) is None:
        raise HistoryError("tool_commit must be a full lowercase Git commit")
    safe_evidence = Path(evidence_file)
    if safe_evidence.is_absolute() or ".." in safe_evidence.parts:
        raise HistoryError("evidence_file must be a safe relative path")

    with history_path.open("a+", encoding="utf-8") as handle:
        try:
            import fcntl

            fcntl.flock(handle.fileno(), fcntl.LOCK_EX)
        except ImportError:  # pragma: no cover - Windows portability path
            pass
        handle.seek(0)
        existing_text = handle.read()
        history = _parse_history(existing_text, history_path)
        reservation = next(
            (
                entry
                for entry in history.iterations
                if entry["iteration_id"] == iteration_id
            ),
            None,
        )
        if reservation is None:
            raise HistoryError(f"iteration id {iteration_id!r} does not exist")
        if any(entry["iteration_id"] == iteration_id for entry in history.results):
            raise HistoryError(
                f"iteration id {iteration_id!r} already has a terminal result"
            )
        entry = {
            "schema_version": 2,
            "record_type": "result",
            "iteration_id": iteration_id,
            "recorded_at": recorded_at,
            "status": status,
            "prior_history_sha256": hashlib.sha256(
                existing_text.encode("utf-8")
            ).hexdigest(),
            "evidence_sha256": evidence_sha256,
            "evidence_file": safe_evidence.as_posix(),
            "tool_commit": tool_commit,
            "board_sha256s": sorted(board["sha256"] for board in reservation["boards"]),
        }
        handle.seek(0, os.SEEK_END)
        handle.write(json.dumps(entry, sort_keys=True, separators=(",", ":")) + "\n")
        handle.flush()
        os.fsync(handle.fileno())
        return entry


def audit_release_history(
    history_path: Path,
    evidence_dir: Path,
    *,
    require_completed: bool = False,
) -> dict:
    """Verify the ledger chain, terminal coverage, and retained artifact digests."""

    history = load_history(history_path)
    results = {entry["iteration_id"]: entry for entry in history.results}
    missing = [
        entry["iteration_id"]
        for entry in history.iterations
        if entry["iteration_id"] not in results
    ]
    if missing:
        raise HistoryError(
            "reserved iteration has no terminal result: " + ", ".join(sorted(missing))
        )
    completed = 0
    failed = 0
    reservations = {entry["iteration_id"]: entry for entry in history.iterations}
    for result in history.results:
        artifact = evidence_dir / result["evidence_file"]
        if not artifact.is_file() or artifact.is_symlink():
            raise HistoryError(
                f"iteration {result['iteration_id']!r} evidence file is missing"
            )
        digest = hashlib.sha256(artifact.read_bytes()).hexdigest()
        if digest != result["evidence_sha256"]:
            raise HistoryError(
                f"iteration {result['iteration_id']!r} evidence digest does not match"
            )
        try:
            evidence = json.loads(artifact.read_text(encoding="utf-8"))
        except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
            raise HistoryError(
                f"iteration {result['iteration_id']!r} evidence is not valid JSON"
            ) from error
        if not isinstance(evidence, dict):
            raise HistoryError(
                f"iteration {result['iteration_id']!r} evidence must be an object"
            )
        if evidence.get("gate") != "external-five":
            raise HistoryError(
                f"iteration {result['iteration_id']!r} evidence is not external-five"
            )
        if evidence.get("iteration_id") != result["iteration_id"]:
            raise HistoryError(
                f"iteration {result['iteration_id']!r} evidence iteration does not match"
            )
        if evidence.get("status") != result["status"]:
            raise HistoryError(
                f"iteration {result['iteration_id']!r} evidence status does not match"
            )
        if evidence.get("tool_commit") != result["tool_commit"]:
            raise HistoryError(
                f"iteration {result['iteration_id']!r} evidence tool commit does not match"
            )
        evidence_boards = evidence.get("boards")
        if not isinstance(evidence_boards, list) or any(
            not isinstance(board, dict) or not isinstance(board.get("sha256"), str)
            for board in evidence_boards
        ):
            raise HistoryError(
                f"iteration {result['iteration_id']!r} evidence boards are invalid"
            )
        evidence_hashes = sorted(board["sha256"] for board in evidence_boards)
        reserved_hashes = sorted(
            board["sha256"] for board in reservations[result["iteration_id"]]["boards"]
        )
        if evidence_hashes != reserved_hashes:
            raise HistoryError(
                f"iteration {result['iteration_id']!r} evidence boards do not match reservation"
            )
        if result["status"] == "completed":
            completed += 1
        else:
            failed += 1
    if require_completed and completed == 0:
        raise HistoryError("no completed external-five iteration is retained")
    return {
        "reservations": len(history.iterations),
        "completed": completed,
        "failed": failed,
    }


BrowserRunner = Callable[[list[Path], Path, str, str], int]


def _redact_local_paths(value: object, replacements: dict[str, str]) -> object:
    if isinstance(value, dict):
        return {
            key: _redact_local_paths(item, replacements) for key, item in value.items()
        }
    if isinstance(value, list):
        return [_redact_local_paths(item, replacements) for item in value]
    if not isinstance(value, str):
        return value
    redacted = value
    for local, public in sorted(replacements.items(), key=lambda item: -len(item[0])):
        redacted = redacted.replace(local, public)
    redacted = re.sub(
        r"(?<![A-Za-z0-9:])/(?:Users|private|tmp|var|home)/[^\s\"']+",
        "<redacted-absolute-path>",
        redacted,
    )
    redacted = re.sub(
        r"(?<![A-Za-z0-9])[A-Za-z]:\\[^\s\"']+",
        "<redacted-absolute-path>",
        redacted,
    )
    return redacted


def _validate_browser_results(
    result_path: Path,
    *,
    candidates: list[Candidate],
    staged_paths: list[Path],
    base_url: str,
    cohort: str,
) -> dict:
    try:
        document = json.loads(result_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise SelectionError(
            f"browser result is unavailable or invalid: {error}"
        ) from error
    if not isinstance(document, dict) or document.get("base") != base_url:
        raise SelectionError(
            "browser result base URL does not match the requested server"
        )
    if document.get("cohort", cohort) != cohort:
        raise SelectionError("browser result cohort does not match the requested gate")
    results = document.get("results")
    if not isinstance(results, list) or len(results) != len(candidates):
        raise SelectionError(
            f"browser result contains {len(results) if isinstance(results, list) else 0} "
            f"journeys; expected {len(candidates)}"
        )
    expected_paths = [str(path.resolve()) for path in staged_paths]
    actual_paths = [
        item.get("path") if isinstance(item, dict) else None for item in results
    ]
    if actual_paths != expected_paths or len(set(expected_paths)) != len(
        expected_paths
    ):
        raise SelectionError(
            "browser result inputs do not exactly match the staged board set"
        )

    redacted: list[dict] = []
    for index, (item, candidate, staged) in enumerate(
        zip(results, candidates, staged_paths, strict=True)
    ):
        if not isinstance(item, dict):
            raise SelectionError(f"browser result {index + 1} is not an object")
        failures = item.get("failures")
        report = item.get("report")
        if not isinstance(failures, list):
            raise SelectionError(f"browser result {index + 1} has no failures array")
        if failures:
            raise SelectionError(
                f"browser journey failed for {candidate.source_id}: "
                + "; ".join(map(str, failures))
            )
        if not isinstance(report, dict) or report.get("ok") is not True:
            raise SelectionError(
                f"browser journey did not produce an OK report for {candidate.source_id}"
            )
        if item.get("exported") is not True:
            raise SelectionError(
                f"browser journey did not validate JSON export for {candidate.source_id}"
            )
        status = item.get("response_status")
        if not isinstance(status, int) or not 200 <= status < 300:
            raise SelectionError(
                f"browser journey returned invalid HTTP status for {candidate.source_id}"
            )
        safe = dict(item)
        safe["path"] = f"inputs/{staged.name}"
        safe["board_id"] = candidate.board_id
        safe["sha256"] = candidate.sha256
        replacements = {
            str(staged): f"inputs/{staged.name}",
            str(staged.resolve()): f"inputs/{staged.name}",
            str(candidate.absolute_path): f"sources/{candidate.relative_path}",
            str(
                candidate.absolute_path.resolve()
            ): f"sources/{candidate.relative_path}",
        }
        redacted.append(_redact_local_paths(safe, replacements))
    return {"base": base_url, "cohort": cohort, "results": redacted}


def _write_evidence(path: Path, document: dict) -> str:
    path.parent.mkdir(parents=True, exist_ok=True)
    encoded = (json.dumps(document, indent=2, sort_keys=True) + "\n").encode("utf-8")
    if re.search(
        rb"(?<![A-Za-z0-9:])/(?:Users|private|tmp|var|home)/[^\s\"']+",
        encoded,
    ) or re.search(rb"(?<![A-Za-z0-9])[A-Za-z]:\\\\[^\s\"']+", encoded):
        raise HistoryError("retained evidence contains an unredacted absolute path")
    if path.exists():
        if path.is_symlink() or path.read_bytes() != encoded:
            raise HistoryError(
                f"evidence file already exists with different content: {path.name}"
            )
    else:
        with path.open("xb") as handle:
            handle.write(encoded)
            handle.flush()
            os.fsync(handle.fileno())
    return hashlib.sha256(encoded).hexdigest()


def _materialize_all(candidates: list[Candidate], inputs: Path) -> list[Path]:
    staged = [materialize_candidate(candidate, inputs) for candidate in candidates]
    if len(staged) != len(set(staged)):
        raise SelectionError("materialized board paths are not unique")
    return staged


def _board_evidence(
    candidates: list[Candidate], staged_hashes: list[str]
) -> list[dict]:
    records: list[dict] = []
    for candidate, staged_sha256 in zip(candidates, staged_hashes, strict=True):
        record = _board_record(candidate)
        record["staged_sha256"] = staged_sha256
        records.append(record)
    return records


def _failed_browser_summary(result_path: Path, staged_paths: list[Path]) -> dict:
    """Retain useful failure facts without retaining local absolute paths."""

    try:
        document = json.loads(result_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return {"artifact": "missing-or-invalid", "results": []}
    raw_results = document.get("results") if isinstance(document, dict) else None
    if not isinstance(raw_results, list):
        return {"artifact": "invalid-shape", "results": []}
    safe_results: list[dict] = []
    for index, staged in enumerate(staged_paths):
        raw = raw_results[index] if index < len(raw_results) else {}
        if not isinstance(raw, dict):
            raw = {}
        safe_results.append(
            {
                "path": f"inputs/{staged.name}",
                "file": staged.name,
                "elapsed_ms": raw.get("elapsed_ms"),
                "response_status": raw.get("response_status"),
                "exported": raw.get("exported") is True,
                "console_error_count": len(raw.get("console_errors", []))
                if isinstance(raw.get("console_errors"), list)
                else None,
                "failures": [
                    _redact_local_paths(str(item), {})
                    for item in raw.get("failures", [])
                ]
                if isinstance(raw.get("failures"), list)
                else ["browser artifact has no failures array"],
            }
        )
    return {"artifact": "captured", "results": safe_results}


def _record_external_evidence(
    *,
    history_path: Path,
    evidence_dir: Path,
    iteration_id: str,
    tool_commit: str,
    common: dict,
    status: str,
    browser: dict,
    browser_exit_code: int | None = None,
    validation_error: str | None = None,
) -> dict:
    evidence = {**common, "status": status, "browser": browser}
    if browser_exit_code is not None:
        evidence["browser_exit_code"] = browser_exit_code
    if validation_error is not None:
        evidence["validation_error"] = validation_error
    evidence = _redact_local_paths(evidence, {})
    if not isinstance(evidence, dict):  # pragma: no cover - recursive type invariant
        raise HistoryError("evidence redaction changed the document shape")
    evidence_path = evidence_dir / f"{iteration_id}.json"
    digest = _write_evidence(evidence_path, evidence)
    append_iteration_result(
        history_path,
        iteration_id=iteration_id,
        status=status,
        recorded_at=_utc_now(),
        evidence_sha256=digest,
        evidence_file=evidence_path.name,
        tool_commit=tool_commit,
    )
    return evidence


def run_external_gate(
    *,
    external_root: Path,
    external_manifest: Path,
    corpus_root: Path,
    corpus_manifest: Path,
    history_path: Path,
    evidence_dir: Path,
    scratch_root: Path,
    iteration_id: str,
    planned_at: str,
    base_url: str,
    entropy: str,
    tool_commit: str,
    runner: BrowserRunner,
) -> dict:
    """Run exactly five external, content-unseen browser journeys."""

    external = discover_candidates(external_root, manifest_path=external_manifest)
    if not external or any(candidate.cohort != "external" for candidate in external):
        raise SelectionError("external-five gate requires an external-cohort manifest")
    corpus = discover_candidates(corpus_root, manifest_path=corpus_manifest)
    corpus_hashes = {candidate.sha256 for candidate in corpus}
    eligible = [
        candidate for candidate in external if candidate.sha256 not in corpus_hashes
    ]
    manifest_sha256 = hashlib.sha256(external_manifest.read_bytes()).hexdigest()
    reservation = reserve_iteration(
        history_path,
        eligible,
        entropy=entropy,
        iteration_id=iteration_id,
        planned_at=planned_at,
        manifest_sha256=manifest_sha256,
        tool_commit=tool_commit,
    )
    by_id = {candidate.board_id: candidate for candidate in eligible}
    selected = [by_id[board["board_id"]] for board in reservation["boards"]]
    run_root = scratch_root / iteration_id
    try:
        staged = _materialize_all(selected, run_root / "inputs")
    except (OSError, SelectionError) as error:
        unstaged_boards = []
        for candidate in selected:
            record = _board_record(candidate)
            record["staged_sha256"] = None
            unstaged_boards.append(record)
        common = {
            "schema_version": 1,
            "gate": "external-five",
            "iteration_id": iteration_id,
            "planned_at": planned_at,
            "tool_commit": tool_commit,
            "manifest_sha256": manifest_sha256,
            "candidate_pool_sha256": candidate_pool_digest(eligible),
            "known_corpus_pool_sha256": candidate_pool_digest(corpus),
            "boards": unstaged_boards,
        }
        _record_external_evidence(
            history_path=history_path,
            evidence_dir=evidence_dir,
            iteration_id=iteration_id,
            tool_commit=tool_commit,
            common=common,
            status="failed",
            browser={"artifact": "inputs-not-staged", "results": []},
            validation_error=f"could not stage selected inputs: {error}",
        )
        raise SelectionError(f"could not stage selected inputs: {error}") from error
    staged_hashes = [hashlib.sha256(path.read_bytes()).hexdigest() for path in staged]
    board_evidence = _board_evidence(selected, staged_hashes)
    common_evidence = {
        "schema_version": 1,
        "gate": "external-five",
        "iteration_id": iteration_id,
        "planned_at": planned_at,
        "tool_commit": tool_commit,
        "manifest_sha256": manifest_sha256,
        "candidate_pool_sha256": candidate_pool_digest(eligible),
        "known_corpus_pool_sha256": candidate_pool_digest(corpus),
        "boards": board_evidence,
    }
    result_dir = run_root / "browser"
    try:
        return_code = runner(staged, result_dir, base_url, "external")
    except OSError as error:
        _record_external_evidence(
            history_path=history_path,
            evidence_dir=evidence_dir,
            iteration_id=iteration_id,
            tool_commit=tool_commit,
            common=common_evidence,
            status="failed",
            browser={"artifact": "runner-did-not-start", "results": []},
            validation_error=f"browser runner could not start: {error}",
        )
        raise SelectionError(f"browser runner could not start: {error}") from error
    current_staged_hashes = [
        hashlib.sha256(path.read_bytes()).hexdigest() for path in staged
    ]
    if current_staged_hashes != staged_hashes:
        _record_external_evidence(
            history_path=history_path,
            evidence_dir=evidence_dir,
            iteration_id=iteration_id,
            tool_commit=tool_commit,
            common=common_evidence,
            status="failed",
            browser=_failed_browser_summary(result_dir / "results.json", staged),
            browser_exit_code=return_code,
            validation_error="staged input changed during browser execution",
        )
        raise SelectionError("staged input changed during browser execution")
    if return_code != 0:
        _record_external_evidence(
            history_path=history_path,
            evidence_dir=evidence_dir,
            iteration_id=iteration_id,
            tool_commit=tool_commit,
            common=common_evidence,
            status="failed",
            browser=_failed_browser_summary(result_dir / "results.json", staged),
            browser_exit_code=return_code,
        )
        raise SelectionError(f"browser runner exited with status {return_code}")
    try:
        browser = _validate_browser_results(
            result_dir / "results.json",
            candidates=selected,
            staged_paths=staged,
            base_url=base_url,
            cohort="external",
        )
    except SelectionError as error:
        _record_external_evidence(
            history_path=history_path,
            evidence_dir=evidence_dir,
            iteration_id=iteration_id,
            tool_commit=tool_commit,
            common=common_evidence,
            status="failed",
            browser=_failed_browser_summary(result_dir / "results.json", staged),
            browser_exit_code=return_code,
            validation_error=str(error),
        )
        raise
    return _record_external_evidence(
        history_path=history_path,
        evidence_dir=evidence_dir,
        iteration_id=iteration_id,
        tool_commit=tool_commit,
        common=common_evidence,
        status="completed",
        browser=browser,
    )


def run_corpus_gate(
    *,
    corpus_root: Path,
    corpus_manifest: Path,
    evidence_dir: Path,
    scratch_root: Path,
    run_id: str,
    base_url: str,
    tool_commit: str,
    runner: BrowserRunner,
) -> dict:
    """Run the browser journey over every supported confirmed corpus input."""

    candidates = discover_candidates(corpus_root, manifest_path=corpus_manifest)
    if not candidates or any(candidate.cohort != "corpus" for candidate in candidates):
        raise SelectionError("corpus gate requires a non-empty corpus-cohort manifest")
    staged = _materialize_all(candidates, scratch_root / run_id / "inputs")
    staged_hashes = [hashlib.sha256(path.read_bytes()).hexdigest() for path in staged]
    result_dir = scratch_root / run_id / "browser"
    return_code = runner(staged, result_dir, base_url, "corpus")
    if return_code != 0:
        raise SelectionError(f"browser runner exited with status {return_code}")
    if [
        hashlib.sha256(path.read_bytes()).hexdigest() for path in staged
    ] != staged_hashes:
        raise SelectionError("staged input changed during browser execution")
    browser = _validate_browser_results(
        result_dir / "results.json",
        candidates=candidates,
        staged_paths=staged,
        base_url=base_url,
        cohort="corpus",
    )
    manifest_sha256 = hashlib.sha256(corpus_manifest.read_bytes()).hexdigest()
    evidence = {
        "schema_version": 1,
        "gate": "corpus-exhaustive",
        "run_id": run_id,
        "status": "completed",
        "tool_commit": tool_commit,
        "manifest_sha256": manifest_sha256,
        "candidate_count": len(candidates),
        "candidate_pool_sha256": candidate_pool_digest(candidates),
        "boards": _board_evidence(candidates, staged_hashes),
        "browser": browser,
    }
    _write_evidence(evidence_dir / f"{run_id}.json", evidence)
    return evidence


def _utc_now() -> str:
    return (
        datetime.now(timezone.utc).isoformat(timespec="seconds").replace("+00:00", "Z")
    )


def _playwright_runner(
    paths: list[Path], output: Path, base_url: str, cohort: str
) -> int:
    output.mkdir(parents=True, exist_ok=True)
    environment = os.environ.copy()
    environment.update(
        {
            "HB_BOARD_FILES": json.dumps([str(path.resolve()) for path in paths]),
            "HB_E2E_BASE": base_url,
            "HB_E2E_OUT": str(output.resolve()),
            "HB_RELEASE_COHORT": cohort,
        }
    )
    result = subprocess.run(
        ["bun", "run", str(_REPOSITORY / "frontend/tests/e2e/drag-drop-release.ts")],
        cwd=_REPOSITORY,
        env=environment,
        check=False,
    )
    return result.returncode
