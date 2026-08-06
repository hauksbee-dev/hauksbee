"""Select reproducible, never-before-used boards for release UX trials.

The release loop records every selected board in an append-only JSONL ledger.
Selection is content-addressed, so a renamed or duplicated checkout cannot be
presented as a new trial, and seeded so a failed run can be reproduced exactly.
"""

from __future__ import annotations

import hashlib
import json
import os
import tomllib
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable


class SelectionError(ValueError):
    """The requested unseen sample cannot be selected honestly."""


class HistoryError(ValueError):
    """The iteration ledger is malformed or would become ambiguous."""


@dataclass(frozen=True)
class Candidate:
    """One content-distinct board input in a pinned corpus source."""

    board_id: str
    sha256: str
    source_id: str
    revision: str
    relative_path: str
    absolute_path: Path
    input_format: str


@dataclass(frozen=True)
class History:
    """Validated contents of an unseen-board iteration ledger."""

    iterations: tuple[dict, ...]
    seen_board_ids: frozenset[str]


_INPUT_FORMATS = {
    ".kicad_pcb": "kicad_pcb",
    ".kicad_sch": "kicad_sch",
    ".brd": "eagle_brd",
    ".pcbdoc": "altium_pcbdoc",
    ".d356": "ipc_356",
    ".board": "hauksbee_board",
    ".zip": "archive",
    ".tgz": "odb_archive",
}


def _is_backup_path(relative: Path) -> bool:
    """Match backup directory forms produced by supported EDA tools.

    KiCad creates ``<project>-backups`` directories, and the corpus fetcher
    explicitly removes those same directories. Exact ``backup(s)`` directory
    names cover equivalent exports without rejecting an ordinary filename that
    happens to contain the word.
    """

    for part in relative.parts[:-1]:
        folded = part.casefold()
        if folded in {"backup", "backups"} or folded.endswith("-backups"):
            return True
    return False


def _source_metadata(path: Path, corpus_root: Path) -> tuple[str, str]:
    parent = path.parent
    while True:
        marker = parent / ".hauksbee-rev"
        if marker.is_file():
            lines = [line.strip() for line in marker.read_text(encoding="utf-8").splitlines()]
            revision = next((line for line in lines if line), "unversioned")
            relative = parent.relative_to(corpus_root)
            source_id = relative.as_posix() if relative.parts else "."
            return source_id, revision
        if parent == corpus_root:
            break
        if corpus_root not in parent.parents:
            break
        parent = parent.parent

    relative = path.relative_to(corpus_root)
    source_id = relative.parts[0] if len(relative.parts) > 1 else "."
    return source_id, "unversioned"


def _input_format(path: Path) -> str | None:
    suffix = path.suffix.casefold()
    direct = _INPUT_FORMATS.get(suffix)
    if direct is not None:
        return direct
    if suffix == ".xml":
        head = path.read_bytes()[:16_384].lower()
        if b"ipc-2581" in head or b"ipc2581" in head:
            return "ipc_2581"
    if path.name.casefold().endswith(".tar.gz"):
        return "odb_archive"
    return None


def _manifest_candidates(root: Path, manifest_path: Path) -> list[Path]:
    try:
        document = tomllib.loads(manifest_path.read_text(encoding="utf-8"))
    except (OSError, tomllib.TOMLDecodeError) as error:
        raise SelectionError(f"cannot read corpus manifest {manifest_path}: {error}") from error

    entries = document.get("board")
    if not isinstance(entries, list):
        raise SelectionError(f"corpus manifest {manifest_path} has no [[board]] entries")

    paths: list[Path] = []
    for index, entry in enumerate(entries):
        if not isinstance(entry, dict):
            raise SelectionError(f"corpus manifest board entry {index + 1} is not a table")
        source_id = entry.get("dest", entry.get("id"))
        expected = entry.get("expect")
        if not isinstance(source_id, str) or not isinstance(expected, list):
            raise SelectionError(
                f"corpus manifest board entry {index + 1} needs string id/dest and expect array"
            )
        if entry.get("license_confirmed", True) is False:
            continue
        source_root = (root / source_id).resolve()
        if not source_root.is_dir():
            raise SelectionError(f"corpus is incomplete: {source_id}/ does not exist")
        if source_root != root and root not in source_root.parents:
            raise SelectionError(f"corpus manifest source escapes the corpus root: {source_id}")
        for relative in expected:
            if not isinstance(relative, str):
                raise SelectionError(
                    f"corpus manifest board entry {index + 1} has a non-string expect path"
                )
            declared_path = root / source_id / relative
            path = declared_path.resolve()
            if path != root and root not in path.parents:
                raise SelectionError(
                    f"corpus manifest expect path escapes the corpus root: {source_id}/{relative}"
                )
            if not declared_path.is_file():
                raise SelectionError(
                    f"corpus is incomplete: expected file is missing: {source_id}/{relative}"
                )
            if declared_path.is_symlink():
                raise SelectionError(
                    f"corpus manifest input must not be a symlink: {source_id}/{relative}"
                )
            if _input_format(path) is not None:
                paths.append(path)
    return paths


def discover_candidates(
    corpus_root: Path, *, manifest_path: Path | None = None
) -> list[Candidate]:
    """Return content-distinct, current board inputs below ``corpus_root``.

    A KiCad PCB supersedes its same-directory, same-stem schematic for this UX
    trial: dropping the PCB exercises both electrical extraction and layout
    checks, while treating both files as two independent boards would overstate
    sample size.
    """

    root = corpus_root.resolve()
    if not root.is_dir():
        raise SelectionError(f"corpus directory does not exist: {corpus_root}")

    if manifest_path is not None:
        supported = _manifest_candidates(root, manifest_path.resolve())
    else:
        supported = []
        for path in root.rglob("*"):
            if path.is_symlink() or not path.is_file() or _input_format(path) is None:
                continue
            relative = path.relative_to(root)
            if not _is_backup_path(relative):
                supported.append(path)

    pcb_stems = {
        (path.parent, path.stem.casefold())
        for path in supported
        if path.suffix.casefold() == ".kicad_pcb"
    }

    by_digest: dict[str, Candidate] = {}
    for path in sorted(supported, key=lambda item: item.relative_to(root).as_posix().casefold()):
        suffix = path.suffix.casefold()
        if suffix == ".kicad_sch" and (path.parent, path.stem.casefold()) in pcb_stems:
            continue

        digest = hashlib.sha256(path.read_bytes()).hexdigest()
        if digest in by_digest:
            continue
        source_id, revision = _source_metadata(path, root)
        relative_path = path.relative_to(root).as_posix()
        by_digest[digest] = Candidate(
            board_id=f"sha256:{digest}",
            sha256=digest,
            source_id=source_id,
            revision=revision,
            relative_path=relative_path,
            absolute_path=path,
            input_format=_input_format(path) or "unknown",
        )

    return sorted(by_digest.values(), key=lambda item: item.relative_path.casefold())


def _rank(seed: str, *parts: str) -> bytes:
    material = "\0".join((seed, *parts)).encode("utf-8")
    return hashlib.sha256(material).digest()


def select_unseen(
    candidates: Iterable[Candidate],
    seen_board_ids: Iterable[str],
    *,
    count: int,
    seed: str,
) -> list[Candidate]:
    """Select a deterministic unseen sample, maximizing source diversity."""

    if count < 1:
        raise SelectionError(f"requested board count must be positive, got {count}")

    unique = {item.board_id: item for item in candidates}
    seen = set(seen_board_ids)
    available = [item for item in unique.values() if item.board_id not in seen]
    if len(available) < count:
        raise SelectionError(
            f"requested {count} unseen boards, but only {len(available)} remain "
            f"out of {len(unique)} candidates"
        )

    by_source: dict[str, list[Candidate]] = {}
    by_format: dict[str, list[Candidate]] = {}
    for item in available:
        by_source.setdefault(item.source_id, []).append(item)
        by_format.setdefault(item.input_format, []).append(item)

    source_order = sorted(by_source, key=lambda source: _rank(seed, "source", source))
    for source, items in by_source.items():
        items.sort(key=lambda item: _rank(seed, "board", source, item.board_id))
    for input_format, items in by_format.items():
        items.sort(key=lambda item: _rank(seed, "format-board", input_format, item.board_id))

    selected: list[Candidate] = []
    selected_ids: set[str] = set()
    selected_sources: set[str] = set()

    # Format diversity is the first useful dimension in an ingestion trial.
    # Prefer a fresh source within each format so diversity on one axis does not
    # needlessly collapse diversity on the other.
    format_order = sorted(by_format, key=lambda fmt: _rank(seed, "format", fmt))
    for input_format in format_order[:count]:
        choices = by_format[input_format]
        choice = next(
            (item for item in choices if item.source_id not in selected_sources),
            choices[0],
        )
        selected.append(choice)
        selected_ids.add(choice.board_id)
        selected_sources.add(choice.source_id)

    # Fill from sources not represented yet before taking a second board from
    # any source repository.
    for source in source_order:
        if len(selected) == count:
            break
        if source in selected_sources:
            continue
        choice = next((item for item in by_source[source] if item.board_id not in selected_ids), None)
        if choice is None:
            continue
        selected.append(choice)
        selected_ids.add(choice.board_id)
        selected_sources.add(choice.source_id)

    if len(selected) < count:
        remainder = [item for item in available if item.board_id not in selected_ids]
        remainder.sort(key=lambda item: _rank(seed, "remainder", item.board_id))
        selected.extend(remainder[: count - len(selected)])
    return selected


def _parse_history(text: str, path: Path) -> History:
    iterations: list[dict] = []
    seen: set[str] = set()
    iteration_ids: set[str] = set()

    for line_number, raw_line in enumerate(text.splitlines(), start=1):
        if not raw_line.strip():
            continue
        try:
            entry = json.loads(raw_line)
        except json.JSONDecodeError as error:
            raise HistoryError(f"{path.name}:{line_number}: invalid JSON: {error.msg}") from error
        if not isinstance(entry, dict):
            raise HistoryError(f"{path.name}:{line_number}: iteration must be a JSON object")

        iteration_id = entry.get("iteration_id")
        boards = entry.get("boards")
        if not isinstance(iteration_id, str) or not iteration_id:
            raise HistoryError(f"{path.name}:{line_number}: missing non-empty iteration_id")
        if iteration_id in iteration_ids:
            raise HistoryError(
                f"{path.name}:{line_number}: duplicate iteration id {iteration_id!r}"
            )
        if not isinstance(boards, list):
            raise HistoryError(f"{path.name}:{line_number}: boards must be an array")

        for board_index, board in enumerate(boards):
            if not isinstance(board, dict):
                raise HistoryError(
                    f"{path.name}:{line_number}: boards[{board_index}] must be an object"
                )
            board_id = board.get("board_id")
            if not isinstance(board_id, str) or not board_id:
                raise HistoryError(
                    f"{path.name}:{line_number}: boards[{board_index}] has no board_id"
                )
            if board_id in seen:
                raise HistoryError(
                    f"{path.name}:{line_number}: board {board_id!r} was already used "
                    "by an earlier iteration"
                )
            seen.add(board_id)

        if len(boards) != 5 or len({board["board_id"] for board in boards}) != 5:
            raise HistoryError(
                f"{path.name}:{line_number}: iteration must contain exactly 5 unique boards"
            )

        iteration_ids.add(iteration_id)
        iterations.append(entry)

    return History(tuple(iterations), frozenset(seen))


def load_history(path: Path) -> History:
    """Load and validate an append-only iteration ledger."""

    if not path.exists():
        return History((), frozenset())
    try:
        text = path.read_text(encoding="utf-8")
    except UnicodeDecodeError as error:
        raise HistoryError(f"{path.name}: history is not valid UTF-8") from error
    return _parse_history(text, path)


def _board_record(item: Candidate) -> dict[str, str]:
    return {
        "board_id": item.board_id,
        "sha256": item.sha256,
        "source_id": item.source_id,
        "revision": item.revision,
        "relative_path": item.relative_path,
        "input_format": item.input_format,
    }


def reserve_iteration(
    history_path: Path,
    candidates: Iterable[Candidate],
    *,
    count: int,
    seed: str,
    iteration_id: str,
    planned_at: str,
) -> dict:
    """Append one planned iteration after checking its ID and unseen sample."""

    if count != 5:
        raise SelectionError(f"release iterations require exactly 5 boards, got {count}")
    if not iteration_id:
        raise HistoryError("iteration id must not be empty")

    history_path.parent.mkdir(parents=True, exist_ok=True)
    # Opening once in append/read mode keeps the validation and append on one
    # file descriptor. An advisory lock prevents two local release loops from
    # selecting the same boards concurrently on Unix; JSONL's single write
    # remains safe on platforms where fcntl is unavailable.
    with history_path.open("a+", encoding="utf-8") as handle:
        try:
            import fcntl

            fcntl.flock(handle.fileno(), fcntl.LOCK_EX)
        except ImportError:  # pragma: no cover - Windows portability path
            pass

        handle.seek(0)
        history = _parse_history(handle.read(), history_path)
        if any(entry["iteration_id"] == iteration_id for entry in history.iterations):
            raise HistoryError(f"iteration id {iteration_id!r} already exists")

        selected = select_unseen(
            candidates,
            history.seen_board_ids,
            count=count,
            seed=seed,
        )
        entry = {
            "iteration_id": iteration_id,
            "planned_at": planned_at,
            "seed": seed,
            "status": "planned",
            "boards": [_board_record(item) for item in selected],
        }
        encoded = json.dumps(entry, sort_keys=True, separators=(",", ":")) + "\n"
        handle.seek(0, os.SEEK_END)
        handle.write(encoded)
        handle.flush()
        os.fsync(handle.fileno())
        return entry
