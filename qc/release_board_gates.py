"""Production orchestration and retained evidence for release board gates."""

from __future__ import annotations

import hashlib
import json
import os
import re
import subprocess
import sys
from dataclasses import replace
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
    materialize_firmware,
    reserve_iteration,
)
from qc.value_grading import ValueGrade, grade_board, input_facts, summarize

_REPOSITORY = Path(__file__).resolve().parent.parent
CANONICAL_HISTORY = _REPOSITORY / "qc/results/unseen-external-history.jsonl"
CANONICAL_EVIDENCE_DIR = _REPOSITORY / "qc/results/evidence-runs"
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


class BrowserJourneyFailure(SelectionError):
    """A journey failed, carrying the graded document for the boards that did not.

    A `SelectionError` so every existing caller and the CLI's exit-code handling
    keep working unchanged; the attached document is what lets the run retain the
    grades and unlocks of the boards beside the broken one.
    """

    def __init__(self, message: str, browser: dict) -> None:
        super().__init__(message)
        self.browser = browser


#: ``(board paths, output dir, base URL, cohort, expected refusals, firmware)``.
#: The firmware list runs parallel to the board paths: each entry is either
#: ``None`` or ``{"path": <staged firmware>, "expect": "cosim"|"load-only"}``.
BrowserRunner = Callable[
    [list[Path], Path, str, str, list[Path], list[dict | None]], int
]

# The manifest axis for an input hauksbee deliberately does not read (a
# pre-Eagle-6 binary Eagle drawing, today). In the CORPUS gate these are staged
# and dropped like any other board, but the journey holds them to the opposite
# contract: an honest refusal, no report, no export, no live launch. Demanding a
# report from them would report coverage over files nothing opened; dropping
# them from the gate would leave the refusal untested on real files, which is
# the only reason the corpus carries them. The external-five gate excludes them
# from its candidate pool instead: see `run_external_gate`.
REFUSAL_AXIS = "unreadable-by-design"


def _displayed_diagnostic(server_error: str) -> str:
    """The part of an engine refusal the app renders, per `board-formats.ts`."""

    return re.sub(r"\s*Supported:.*$", "", server_error, flags=re.S).strip() or server_error


def _expected_refusals(
    candidates: list[Candidate], staged_paths: list[Path]
) -> list[Path]:
    return [
        staged
        for candidate, staged in zip(candidates, staged_paths, strict=True)
        if REFUSAL_AXIS in candidate.axes
    ]


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


def _redaction_map(candidate: Candidate, staged: Path) -> dict[str, str]:
    """Local paths to rewrite before a browser row is retained."""

    return {
        str(staged): f"inputs/{staged.name}",
        str(staged.resolve()): f"inputs/{staged.name}",
        str(candidate.absolute_path): f"sources/{candidate.relative_path}",
        str(candidate.absolute_path.resolve()): f"sources/{candidate.relative_path}",
    }


def _graded_or_reduced(
    result_path: Path,
    *,
    candidates: list[Candidate],
    staged_paths: list[Path],
    base_url: str,
    cohort: str,
) -> tuple[dict, str | None]:
    """Grade the browser artifact, returning (document, failure reason or None).

    Never raises. A malformed artifact is a failure to record, not a traceback:
    letting one escape leaves a reserved iteration with no terminal ledger record
    and burns five unseen boards for nothing.
    """

    try:
        return (
            _validate_browser_results(
                result_path,
                candidates=candidates,
                staged_paths=staged_paths,
                base_url=base_url,
                cohort=cohort,
            ),
            None,
        )
    except BrowserJourneyFailure as error:
        return error.browser, str(error)
    except SelectionError as error:
        return _failed_browser_summary(result_path, staged_paths), str(error)
    except Exception as error:  # noqa: BLE001 - the artifact is untrusted input
        return (
            _failed_browser_summary(result_path, staged_paths),
            f"browser artifact could not be graded: "
            f"{type(error).__name__}: {error}",
        )


def _journey_honesty_failure(
    item: dict,
    candidate: Candidate,
    *,
    report: object,
    expects_refusal: bool,
) -> str | None:
    """Return why a completed journey is dishonest, or None if it is honest.

    Returned rather than raised so one dishonest board does not discard the value
    grades of the boards beside it. The run still fails; it fails at the end, with
    the whole graded document retained, exactly as a journey failure does.
    """

    try:
        if item.get("expected_refusal") is not expects_refusal:
            raise SelectionError(
                f"browser journey did not apply the {REFUSAL_AXIS} contract to "
                f"{candidate.source_id}"
            )
        if expects_refusal:
            # The refusal side of the same gate, and it must rest on retained
            # evidence rather than on the journey's own say-so. `refused: true`
            # with an empty row would otherwise record a passing journey for a
            # board nothing was ever dropped on, so the refusal payload the
            # server produced and the message the page rendered are both
            # required here.
            if item.get("refused") is not True:
                raise SelectionError(
                    f"browser journey did not refuse {candidate.source_id} honestly"
                )
            if not isinstance(report, dict) or report.get("ok") is not False:
                raise SelectionError(
                    f"browser journey retained no server refusal for {candidate.source_id}, "
                    f"which the corpus declares {REFUSAL_AXIS}"
                )
            server_error = str(report.get("error") or "").strip()
            if not server_error:
                raise SelectionError(
                    f"browser journey retained a refusal with no reason for "
                    f"{candidate.source_id}"
                )
            rendered = str(item.get("refusal_message") or "").strip()
            if not rendered:
                raise SelectionError(
                    f"browser journey retained no rendered refusal for {candidate.source_id}"
                )
            # The rendered text has to be the server's, not the journey's. The
            # app strips the engine's trailing "Supported: ..." clause before
            # displaying it (frontend/src/lib/board-formats.ts), so apply the
            # same rule here rather than trusting the journey to have compared
            # them: this validator audits that artifact, and re-deriving the one
            # relationship it rests on is the point.
            if _displayed_diagnostic(server_error) not in rendered:
                raise SelectionError(
                    f"retained refusal for {candidate.source_id} does not carry what "
                    f"the server said"
                )
            if (
                report.get("num_components")
                or report.get("num_nets")
                or report.get("sections")
            ):
                raise SelectionError(
                    f"refused {candidate.source_id} still came back carrying board content"
                )
            if item.get("exported") is not False:
                raise SelectionError(
                    f"browser journey exported JSON for a refused {candidate.source_id}"
                )
            if item.get("live_started") is not False:
                raise SelectionError(
                    f"browser journey started a live session for a refused "
                    f"{candidate.source_id}"
                )
        else:
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
    except SelectionError as error:
        return str(error)
    return None


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
    grades: list[tuple[str, ValueGrade]] = []
    journey_failures: list[str] = []
    honesty_failures: list[str] = []
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
            # Recorded, not raised. Aborting here threw away the grades of every
            # board that DID complete, so a run with one broken journey retained
            # no unlocks for the honestly degraded boards beside it. The run still
            # fails, at the end, with the whole graded document kept.
            journey_failures.append(
                f"browser journey failed for {candidate.source_id}: "
                + "; ".join(map(str, failures))
            )
            # The broken board keeps a row of its own. Dropping it would make the
            # graded document a REDUCTION of the failure summary it replaces,
            # losing the one row a reader most wants: what actually went wrong,
            # on which input, with what status.
            broken = dict(item)
            broken["path"] = f"inputs/{staged.name}"
            broken["board_id"] = candidate.board_id
            broken["sha256"] = candidate.sha256
            broken["value"] = {
                "grade": "journey-failed",
                "reasons": [str(text) for text in failures],
                "unlocks": [],
                "signals": {"input_format": candidate.input_format},
            }
            redacted.append(
                _redact_local_paths(broken, _redaction_map(candidate, staged))
            )
            continue
        expects_refusal = REFUSAL_AXIS in candidate.axes
        honesty = _journey_honesty_failure(
            item, candidate, report=report, expects_refusal=expects_refusal
        )
        if honesty is not None:
            honesty_failures.append(honesty)
            dishonest = dict(item)
            dishonest["path"] = f"inputs/{staged.name}"
            dishonest["board_id"] = candidate.board_id
            dishonest["sha256"] = candidate.sha256
            dishonest["value"] = {
                "grade": "honesty-failed",
                "reasons": [honesty],
                "unlocks": [],
                "signals": {"input_format": candidate.input_format},
            }
            redacted.append(
                _redact_local_paths(dishonest, _redaction_map(candidate, staged))
            )
            continue
        # Honest is necessary and not sufficient. Everything above establishes
        # that the run said true things about what it did; this says whether
        # what it did was worth a bench. See qc/value_grading.py.
        grade = grade_board(
            item,
            input_format=candidate.input_format,
            expects_refusal=expects_refusal,
            facts=input_facts(candidate.input_format, staged),
            firmware_expect=candidate.firmware_expect or None,
            axes=candidate.axes,
        )
        # `source_id` alone is a manifest entry, and one entry can contribute
        # several distinct PCBs, so a summary keyed on it would list two rows a
        # reader cannot tell apart. The source-relative path is what identifies
        # the board, and it is already open in the retained evidence.
        grades.append((f"{candidate.source_id}:{candidate.relative_path}", grade))

        safe = dict(item)
        safe["path"] = f"inputs/{staged.name}"
        safe["board_id"] = candidate.board_id
        safe["sha256"] = candidate.sha256
        safe["value"] = grade.as_dict()
        redacted.append(
            _redact_local_paths(safe, _redaction_map(candidate, staged))
        )
    summary = summarize(grades)
    if journey_failures or honesty_failures:
        # Every board that completed is graded and enumerated above; this is what
        # makes the run fail, and it is raised only after the document exists so
        # the caller can retain it.
        raise BrowserJourneyFailure(
            "; ".join(journey_failures + honesty_failures),
            {
                "base": base_url,
                "cohort": cohort,
                "results": redacted,
                "value_summary": summary,
            },
        )
    return {
        "base": base_url,
        "cohort": cohort,
        "results": redacted,
        "value_summary": summary,
    }


def value_failure_message(browser: dict) -> str | None:
    """The gate-failing sentence for a graded run, or None when none failed."""

    failed = browser.get("value_summary", {}).get("failed") or []
    if not failed:
        return None
    return "board journeys delivered no bench-grade value: " + "; ".join(
        f"{entry['board']} ({'; '.join(entry['reasons'])})" for entry in failed
    )


def describe_degraded(browser: dict) -> list[str]:
    """The warnings an iteration report has to show rather than round to green.

    Every degraded board with the upload that unlocks more, and every disclosure
    of something the gate could not check for itself: model binding, extraction
    coverage, component identity, connectivity coverage, net names, a weakened
    Gerber reconstruction floor, and a manifest that asked for less than a
    firmware co-simulation. A limitation only a per-board signal records is one nobody
    reads.
    """

    summary = browser.get("value_summary", {})
    lines = [
        f"{entry['board']}: DEGRADED-HONEST; unlocked by "
        + " | ".join(entry["unlocks"])
        for entry in summary.get("degraded") or []
    ]
    unverified = summary.get("unverified_extraction") or []
    if unverified:
        lines.append(
            f"extraction coverage UNVERIFIED against the input on {len(unverified)} "
            "board(s): "
            + ", ".join(
                f"{entry['board']} ({entry['input_format']})" for entry in unverified
            )
        )
    unverified_binding = summary.get("unverified_binding") or []
    if unverified_binding:
        lines.append(
            f"model binding UNCORROBORATED on {len(unverified_binding)} board(s), "
            "whose open-parts list is empty or absent, so nothing in the report "
            "checks what it claims to have bound: "
            + ", ".join(
                f"{entry['board']} ({entry['critical_parts_bound']})"
                for entry in unverified_binding
            )
        )
    unverified_ids = summary.get("unverified_identity") or []
    if unverified_ids:
        lines.append(
            f"component identity UNVERIFIED against the input on "
            f"{len(unverified_ids)} board(s): "
            + ", ".join(
                f"{entry['board']} ({entry['input_format']})"
                for entry in unverified_ids
            )
        )
    unverified_nets = summary.get("unverified_connectivity") or []
    if unverified_nets:
        lines.append(
            f"connectivity coverage UNVERIFIED against the input on "
            f"{len(unverified_nets)} board(s): "
            + ", ".join(
                f"{entry['board']} ({entry['input_format']})"
                for entry in unverified_nets
            )
        )
    unverified_names = summary.get("unverified_net_identity") or []
    if unverified_names:
        # The largest unverified dimension of the lot, and the one an operator
        # would otherwise never see: the net COUNT is checked on these boards
        # and the net NAMES are not, so a padded inventory reads as recovery.
        lines.append(
            f"net names UNCHECKED against the input on {len(unverified_names)} "
            f"board(s), so their connectivity rests on a count alone: "
            + ", ".join(
                f"{entry['board']} ({entry['input_format']})"
                for entry in unverified_names
            )
        )
    for entry in summary.get("unverified_reconstruction") or []:
        # The reason travels with the board. Three different conditions weaken
        # the reconstruction floor, and printing one of them for all three told
        # the operator the copper was unclassifiable on boards where it was not.
        lines.append(
            f"{entry['board']}: reconstruction floor UNVERIFIED because "
            + entry.get("because", "unstated")
        )
    lowered = summary.get("firmware_expectation_lowered") or []
    for entry in lowered:
        lines.append(
            f"{entry['board']}: firmware expectation LOWERED to {entry['expect']} "
            "by the manifest, so no co-simulation was demanded"
        )
    return lines


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


def _stage_firmware(
    candidates: list[Candidate],
    inputs: Path,
    reserved: list[str | None] | None = None,
) -> list[dict | None]:
    """Stage each board's paired firmware, keeping the list parallel to the boards.

    ``reserved`` carries the digest each image had when the iteration was
    reserved, so staging cannot substitute different bytes under a reservation
    that named the original.
    """

    staged: list[dict | None] = []
    digests = reserved or [None] * len(candidates)
    for candidate, reserved_sha256 in zip(candidates, digests, strict=True):
        path = materialize_firmware(
            candidate, inputs, reserved_sha256=reserved_sha256
        )
        staged.append(
            None
            if path is None
            else {"path": path, "expect": candidate.firmware_expect or "cosim"}
        )
    return staged


def _staged_digests(staged: list[Path], firmware: list[dict | None]) -> list[str]:
    """Content digests of everything the journey is about to be handed.

    Firmware is included: without it a paired image could change between
    reservation and execution, or during it, and nothing in the ledger would say
    which bytes the co-simulation actually ran.
    """

    digests = [hashlib.sha256(path.read_bytes()).hexdigest() for path in staged]
    digests.extend(
        hashlib.sha256(Path(item["path"]).read_bytes()).hexdigest()
        for item in firmware
        if item is not None
    )
    return digests


def _evidence_record(candidate: Candidate) -> dict:
    """A board's ledger record for EVIDENCE, which must always be producible.

    `_board_record` reads the paired firmware and raises when it cannot, which is
    right at reservation time: the ledger line has to name the bytes. It is wrong
    while writing a terminal record, because the reason we are writing one may be
    that the image just vanished, and an exception there would leave the
    reservation with no terminal result at all. Here the digest is simply dropped.
    """

    try:
        return _board_record(candidate)
    except SelectionError:
        record = _board_record(replace(candidate, firmware_absolute_path=None))
        record["firmware_sha256"] = None
        return record


def _board_evidence(
    candidates: list[Candidate],
    staged_hashes: list[str],
    firmware: list[dict | None] | None = None,
) -> list[dict]:
    records: list[dict] = []
    plans = firmware or [None] * len(candidates)
    for candidate, staged_sha256, plan in zip(
        candidates, staged_hashes, plans, strict=True
    ):
        record = _evidence_record(candidate)
        record["staged_sha256"] = staged_sha256
        # Named in the evidence beside the board, so a reader can tell which
        # image the retained co-simulation result belongs to.
        try:
            record["firmware_staged_sha256"] = (
                hashlib.sha256(Path(plan["path"]).read_bytes()).hexdigest()
                if plan is not None
                else None
            )
        except OSError:
            record["firmware_staged_sha256"] = None
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
    # An `unreadable-by-design` input cannot serve this gate. Its whole question
    # is whether five previously-unseen real boards analyse in a browser, and a
    # file hauksbee refuses by design answers nothing about that: five of them
    # would satisfy the run with five refusals and record a completed
    # external-five iteration containing no analysed board at all. The refusal
    # contract stays where the refusal fixtures live, in the corpus gate.
    eligible = [
        candidate
        for candidate in external
        if candidate.sha256 not in corpus_hashes and REFUSAL_AXIS not in candidate.axes
    ]
    manifest_sha256 = hashlib.sha256(external_manifest.read_bytes()).hexdigest()
    # Computed before anything is staged, so the handlers below that must write a
    # terminal ledger record never have to re-read the source tree to do it.
    eligible_pool_sha256 = candidate_pool_digest(eligible)
    corpus_pool_sha256 = candidate_pool_digest(corpus)
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
        firmware = _stage_firmware(
            selected,
            run_root / "inputs",
            [board.get("firmware_sha256") or None for board in reservation["boards"]],
        )
    except (OSError, SelectionError) as error:
        unstaged_boards = []
        for candidate in selected:
            record = _evidence_record(candidate)
            record["staged_sha256"] = None
            unstaged_boards.append(record)
        common = {
            "schema_version": 1,
            "gate": "external-five",
            "iteration_id": iteration_id,
            "planned_at": planned_at,
            "tool_commit": tool_commit,
            "manifest_sha256": manifest_sha256,
            "candidate_pool_sha256": eligible_pool_sha256,
            "known_corpus_pool_sha256": corpus_pool_sha256,
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
    try:
        staged_hashes = [
            hashlib.sha256(path.read_bytes()).hexdigest() for path in staged
        ]
        immutability_digests = _staged_digests(staged, firmware)
        board_evidence = _board_evidence(selected, staged_hashes, firmware)
    except (OSError, SelectionError) as error:
        # Reading a staged input can fail; the reservation must still close.
        unstaged = []
        for candidate in selected:
            record = _evidence_record(candidate)
            record["staged_sha256"] = None
            unstaged.append(record)
        _record_external_evidence(
            history_path=history_path,
            evidence_dir=evidence_dir,
            iteration_id=iteration_id,
            tool_commit=tool_commit,
            common={
                "schema_version": 1,
                "gate": "external-five",
                "iteration_id": iteration_id,
                "planned_at": planned_at,
                "tool_commit": tool_commit,
                "manifest_sha256": manifest_sha256,
                "candidate_pool_sha256": eligible_pool_sha256,
                "known_corpus_pool_sha256": corpus_pool_sha256,
                "boards": unstaged,
            },
            status="failed",
            browser={"artifact": "staged-inputs-unreadable", "results": []},
            validation_error=f"could not read staged inputs: {error}",
        )
        raise SelectionError(f"could not read staged inputs: {error}") from error
    common_evidence = {
        "schema_version": 1,
        "gate": "external-five",
        "iteration_id": iteration_id,
        "planned_at": planned_at,
        "tool_commit": tool_commit,
        "manifest_sha256": manifest_sha256,
        "candidate_pool_sha256": eligible_pool_sha256,
        "known_corpus_pool_sha256": corpus_pool_sha256,
        "boards": board_evidence,
    }
    result_dir = run_root / "browser"
    try:
        # Always empty: the eligibility filter above keeps `unreadable-by-design`
        # inputs out of this gate entirely. Derived rather than hard-coded so a
        # future change to that filter cannot silently leave the journey
        # demanding a report from a file the manifest says is unreadable.
        return_code = runner(
            staged,
            result_dir,
            base_url,
            "external",
            _expected_refusals(selected, staged),
            firmware,
        )
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
    try:
        digests_after = _staged_digests(staged, firmware)
    except OSError as error:
        # A staged input that vanished mid-run must not escape as an OSError:
        # the reservation still has to receive its one terminal record.
        digests_after = [f"unreadable: {error}"]
    if digests_after != immutability_digests:
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
    # Graded FIRST, whatever the exit code. The real runner exits non-zero on any
    # failing journey, so checking the code before grading meant a run with one
    # broken board retained no grades at all for the four beside it.
    browser, validation_error = _graded_or_reduced(
        result_dir / "results.json",
        candidates=selected,
        staged_paths=staged,
        base_url=base_url,
        cohort="external",
    )
    if return_code != 0 and validation_error is None:
        validation_error = f"browser runner exited with status {return_code}"
    if validation_error is not None:
        _record_external_evidence(
            history_path=history_path,
            evidence_dir=evidence_dir,
            iteration_id=iteration_id,
            tool_commit=tool_commit,
            common=common_evidence,
            status="failed",
            browser=browser,
            browser_exit_code=return_code,
            validation_error=validation_error,
        )
        # Printed before raising: a failing run is exactly when the other boards'
        # warnings matter, and raising first meant they were retained but never
        # shown.
        for line in describe_degraded(browser):
            print(f"unseen-board trial: {line}", file=sys.stderr)
        raise SelectionError(validation_error)
    # A board that passed every honesty check and still delivered nothing a
    # bench could use fails the run, and the graded evidence is retained in
    # full rather than reduced to a failure summary: the whole point of the
    # value contract is that the next reader can see WHICH boards collapsed and
    # on what signals.
    value_failure = value_failure_message(browser)
    evidence = _record_external_evidence(
        history_path=history_path,
        evidence_dir=evidence_dir,
        iteration_id=iteration_id,
        tool_commit=tool_commit,
        common=common_evidence,
        status="completed" if value_failure is None else "failed",
        browser=browser,
        validation_error=value_failure,
    )
    # stderr, not stdout: the command's stdout is a JSON document by contract,
    # and a human-readable warning in front of it would corrupt it.
    for line in describe_degraded(browser):
        print(f"unseen-board trial: {line}", file=sys.stderr)
    if value_failure is not None:
        raise SelectionError(value_failure)
    return evidence


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
    firmware = _stage_firmware(candidates, scratch_root / run_id / "inputs")
    staged_hashes = [hashlib.sha256(path.read_bytes()).hexdigest() for path in staged]
    immutability_digests = _staged_digests(staged, firmware)
    result_dir = scratch_root / run_id / "browser"
    return_code = runner(
        staged,
        result_dir,
        base_url,
        "corpus",
        _expected_refusals(candidates, staged),
        firmware,
    )
    try:
        if _staged_digests(staged, firmware) != immutability_digests:
            raise SelectionError("staged input changed during browser execution")
    except OSError as error:
        raise SelectionError(
            f"staged input became unreadable during browser execution: {error}"
        ) from error
    browser, validation_error = _graded_or_reduced(
        result_dir / "results.json",
        candidates=candidates,
        staged_paths=staged,
        base_url=base_url,
        cohort="corpus",
    )
    if return_code != 0 and validation_error is None:
        validation_error = f"browser runner exited with status {return_code}"
    manifest_sha256 = hashlib.sha256(corpus_manifest.read_bytes()).hexdigest()
    value_failure = validation_error or value_failure_message(browser)
    evidence = {
        "schema_version": 1,
        "gate": "corpus-exhaustive",
        "run_id": run_id,
        "status": "completed" if value_failure is None else "failed",
        "tool_commit": tool_commit,
        "manifest_sha256": manifest_sha256,
        "candidate_count": len(candidates),
        "candidate_pool_sha256": candidate_pool_digest(candidates),
        "boards": _board_evidence(candidates, staged_hashes, firmware),
        "browser": browser,
    }
    if value_failure is not None:
        evidence["validation_error"] = value_failure
    _write_evidence(evidence_dir / f"{run_id}.json", evidence)
    for line in describe_degraded(browser):
        print(f"corpus gate: {line}", file=sys.stderr)
    if value_failure is not None:
        raise SelectionError(value_failure)
    return evidence


def _utc_now() -> str:
    return (
        datetime.now(timezone.utc).isoformat(timespec="seconds").replace("+00:00", "Z")
    )


def _playwright_runner(
    paths: list[Path],
    output: Path,
    base_url: str,
    cohort: str,
    refusals: list[Path],
    firmware: list[dict | None],
) -> int:
    output.mkdir(parents=True, exist_ok=True)
    environment = os.environ.copy()
    environment.update(
        {
            "HB_BOARD_FILES": json.dumps([str(path.resolve()) for path in paths]),
            "HB_E2E_BASE": base_url,
            "HB_E2E_OUT": str(output.resolve()),
            "HB_RELEASE_COHORT": cohort,
            "HB_REFUSAL_FILES": json.dumps(
                [str(path.resolve()) for path in refusals]
            ),
            # Parallel to HB_BOARD_FILES: null where the manifest paired no
            # firmware with that board.
            "HB_FIRMWARE_FILES": json.dumps(
                [
                    None
                    if item is None
                    else {
                        "path": str(Path(item["path"]).resolve()),
                        "expect": item["expect"],
                    }
                    for item in firmware
                ]
            ),
        }
    )
    result = subprocess.run(
        ["bun", "run", str(_REPOSITORY / "frontend/tests/e2e/drag-drop-release.ts")],
        cwd=_REPOSITORY,
        env=environment,
        check=False,
    )
    return result.returncode
