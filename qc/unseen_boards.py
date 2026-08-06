"""Select reproducible, never-before-used boards for release UX trials.

The release loop records every selected board in an append-only JSONL ledger.
Logical board identity prevents a metadata-only re-save from being presented as
a new board, while a separate content digest preserves exact reproducibility.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import secrets
import subprocess
import sys
import tomllib
import zipfile
from dataclasses import dataclass
from datetime import datetime, timezone
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
    axes: tuple[str, ...] = ()
    bundle_members: tuple[str, ...] = ()


@dataclass(frozen=True)
class _DiscoveredPath:
    path: Path
    source_id: str
    revision: str
    axes: tuple[str, ...]
    input_format: str
    bundle_members: tuple[str, ...] = ()


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
}

SELECTOR_VERSION = 1
_GERBER_COPPER_EXTENSIONS = {".gbr", ".gtl", ".gbl", ".art"}
_GERBER_DRILL_EXTENSIONS = {".drl", ".txt"}
_GERBER_MEMBER_EXTENSIONS = _GERBER_COPPER_EXTENSIONS | _GERBER_DRILL_EXTENSIONS


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
    if suffix == ".zip":
        try:
            with zipfile.ZipFile(path) as archive:
                names = [name for name in archive.namelist() if not name.endswith("/")]
        except (OSError, zipfile.BadZipFile):
            return None
        folded = [name.casefold() for name in names]
        if sum(name.endswith(".board") for name in folded) == 1:
            return "hauksbee_board_archive"
        if any("/matrix/matrix" in f"/{name}" for name in folded) and any(
            "/steps/" in f"/{name}" for name in folded
        ):
            return "odb_archive"
        suffixes = {Path(name).suffix.casefold() for name in names}
        if suffixes & _GERBER_COPPER_EXTENSIONS and suffixes & _GERBER_DRILL_EXTENSIONS:
            return "gerber_archive"
        return None
    if path.name.casefold().endswith(".tar.gz"):
        return "odb_archive"
    return None


def _manifest_revision(source_root: Path, source_id: str, entry: dict) -> str:
    marker = source_root / ".hauksbee-rev"
    if not marker.is_file() or marker.is_symlink():
        raise SelectionError(f"corpus is incomplete: {source_id}/.hauksbee-rev is missing")
    lines = [line.strip() for line in marker.read_text(encoding="utf-8").splitlines() if line.strip()]
    kind = entry.get("kind", "git")
    if kind == "git":
        expected = entry.get("rev")
        if not isinstance(expected, str) or not expected:
            raise SelectionError(f"{source_id}: git corpus entry has no revision pin")
        actual = lines[0] if lines else ""
        if actual != expected:
            raise SelectionError(
                f"{source_id}: fetched revision {actual or '<missing>'} "
                f"does not match manifest pin {expected}"
            )
        return expected
    if kind == "zip":
        expected = entry.get("sha256")
        wanted = f"sha256:{expected}" if isinstance(expected, str) else ""
        if not wanted or wanted not in lines:
            actual = next((line for line in lines if line.startswith("sha256:")), "<missing>")
            raise SelectionError(
                f"{source_id}: fetched archive pin {actual} does not match manifest pin {wanted}"
            )
        return wanted
    raise SelectionError(f"{source_id}: unsupported corpus source kind {kind!r}")


def _manifest_candidates(root: Path, manifest_path: Path) -> list[_DiscoveredPath]:
    try:
        document = tomllib.loads(manifest_path.read_text(encoding="utf-8"))
    except (OSError, tomllib.TOMLDecodeError) as error:
        raise SelectionError(f"cannot read corpus manifest {manifest_path}: {error}") from error

    entries = document.get("board")
    if not isinstance(entries, list):
        raise SelectionError(f"corpus manifest {manifest_path} has no [[board]] entries")

    paths: list[_DiscoveredPath] = []
    for index, entry in enumerate(entries):
        if not isinstance(entry, dict):
            raise SelectionError(f"corpus manifest board entry {index + 1} is not a table")
        manifest_id = entry.get("id")
        destination = entry.get("dest", manifest_id)
        expected = entry.get("expect")
        axes = entry.get("axes")
        if (
            not isinstance(manifest_id, str)
            or not isinstance(destination, str)
            or not isinstance(expected, list)
            or not isinstance(axes, list)
            or not all(isinstance(axis, str) and axis for axis in axes)
        ):
            raise SelectionError(
                f"corpus manifest board entry {index + 1} needs string id/dest and string arrays "
                "for expect and axes"
            )
        if entry.get("license_confirmed", True) is False:
            continue
        source_root = (root / destination).resolve()
        if not source_root.is_dir():
            raise SelectionError(f"corpus is incomplete: {destination}/ does not exist")
        if source_root != root and root not in source_root.parents:
            raise SelectionError(f"corpus manifest source escapes the corpus root: {destination}")
        revision = _manifest_revision(source_root, manifest_id, entry)
        entry_start = len(paths)
        gerber_members: list[str] = []
        for relative in expected:
            if not isinstance(relative, str):
                raise SelectionError(
                    f"corpus manifest board entry {index + 1} has a non-string expect path"
                )
            declared_path = root / destination / relative
            path = declared_path.resolve()
            if path != root and root not in path.parents:
                raise SelectionError(
                    f"corpus manifest expect path escapes the corpus root: "
                    f"{destination}/{relative}"
                )
            if not declared_path.is_file():
                raise SelectionError(
                    f"corpus is incomplete: expected file is missing: {destination}/{relative}"
                )
            if declared_path.is_symlink():
                raise SelectionError(
                    f"corpus manifest input must not be a symlink: {destination}/{relative}"
                )
            input_format = _input_format(path)
            if input_format is not None:
                paths.append(
                    _DiscoveredPath(
                        path,
                        manifest_id,
                        revision,
                        tuple(axes),
                        input_format,
                    )
                )
            elif path.suffix.casefold() in _GERBER_MEMBER_EXTENSIONS:
                gerber_members.append(relative)
        if len(paths) == entry_start and gerber_members:
            paths.append(
                _DiscoveredPath(
                    source_root,
                    manifest_id,
                    revision,
                    tuple(axes),
                    "gerber_bundle",
                    tuple(sorted(gerber_members)),
                )
            )
        if len(paths) == entry_start:
            raise SelectionError(
                f"{manifest_id}: no supported drag-and-drop board input among expect paths"
            )
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
        discovered = _manifest_candidates(root, manifest_path.resolve())
    else:
        discovered = []
        for path in root.rglob("*"):
            if path.is_symlink() or not path.is_file() or _input_format(path) is None:
                continue
            relative = path.relative_to(root)
            if not _is_backup_path(relative):
                source_id, revision = _source_metadata(path, root)
                discovered.append(
                    _DiscoveredPath(
                        path,
                        source_id,
                        revision,
                        (),
                        _input_format(path) or "unknown",
                    )
                )

    pcb_stems = {
        (item.path.parent, item.path.stem.casefold())
        for item in discovered
        if item.path.suffix.casefold() == ".kicad_pcb"
    }

    by_digest: dict[str, Candidate] = {}
    for item in sorted(
        discovered, key=lambda value: value.path.relative_to(root).as_posix().casefold()
    ):
        path = item.path
        suffix = path.suffix.casefold()
        if suffix == ".kicad_sch" and (path.parent, path.stem.casefold()) in pcb_stems:
            continue

        digest = _content_digest(path, item.bundle_members)
        if digest in by_digest:
            continue
        relative_path = path.relative_to(root).as_posix()
        if item.bundle_members:
            logical_name = "gerber-bundle"
        else:
            logical_path = Path(relative_path)
            if suffix in {".kicad_pcb", ".kicad_sch"}:
                logical_path = logical_path.with_suffix("")
            logical_name = logical_path.as_posix().casefold()
        logical_digest = hashlib.sha256(
            f"{item.source_id}\0{logical_name}".encode("utf-8")
        ).hexdigest()
        by_digest[digest] = Candidate(
            board_id=f"board:{logical_digest}",
            sha256=digest,
            source_id=item.source_id,
            revision=item.revision,
            relative_path=relative_path,
            absolute_path=path,
            input_format=item.input_format,
            axes=item.axes,
            bundle_members=item.bundle_members,
        )

    return sorted(by_digest.values(), key=lambda item: item.relative_path.casefold())


def _content_digest(path: Path, bundle_members: tuple[str, ...]) -> str:
    if not bundle_members:
        return hashlib.sha256(path.read_bytes()).hexdigest()
    digest = hashlib.sha256()
    for member in bundle_members:
        member_path = path / member
        digest.update(member.encode("utf-8"))
        digest.update(b"\0")
        digest.update(member_path.read_bytes())
        digest.update(b"\0")
    return digest.hexdigest()


def materialize_candidate(candidate: Candidate, destination: Path) -> Path:
    """Return a directly droppable file, packaging loose Gerbers deterministically."""

    if not candidate.bundle_members:
        return candidate.absolute_path
    destination.mkdir(parents=True, exist_ok=True)
    safe_source = re.sub(r"[^a-zA-Z0-9._-]+", "-", candidate.source_id).strip("-") or "board"
    archive_path = destination / f"{safe_source}-{candidate.board_id[6:14]}.zip"
    with zipfile.ZipFile(archive_path, "w", compression=zipfile.ZIP_DEFLATED) as archive:
        for member in candidate.bundle_members:
            info = zipfile.ZipInfo(member, date_time=(1980, 1, 1, 0, 0, 0))
            info.compress_type = zipfile.ZIP_DEFLATED
            info.external_attr = 0o100644 << 16
            archive.writestr(info, (candidate.absolute_path / member).read_bytes())
    return archive_path


def _rank(seed: str, *parts: str) -> bytes:
    material = "\0".join((seed, *parts)).encode("utf-8")
    return hashlib.sha256(material).digest()


def _unique_candidates(candidates: Iterable[Candidate]) -> dict[str, Candidate]:
    unique: dict[str, Candidate] = {}
    for item in candidates:
        previous = unique.get(item.board_id)
        if previous is not None and previous != item:
            raise SelectionError(f"conflicting candidates share board id {item.board_id!r}")
        unique[item.board_id] = item
    return unique


def candidate_pool_digest(candidates: Iterable[Candidate]) -> str:
    """Fingerprint every fact that can affect selection or reproduction."""

    unique = _unique_candidates(candidates)
    records = [
        {
            "board_id": item.board_id,
            "sha256": item.sha256,
            "source_id": item.source_id,
            "revision": item.revision,
            "relative_path": item.relative_path,
            "input_format": item.input_format,
            "axes": sorted(item.axes),
            "bundle_members": list(item.bundle_members),
        }
        for item in unique.values()
    ]
    records.sort(key=lambda record: record["board_id"])
    encoded = json.dumps(records, sort_keys=True, separators=(",", ":")).encode("utf-8")
    return hashlib.sha256(encoded).hexdigest()


def select_unseen(
    candidates: Iterable[Candidate],
    seen_board_ids: Iterable[str],
    *,
    count: int,
    seed: str,
) -> list[Candidate]:
    """Select a seeded unseen sample stratified across formats, sources, and axes."""

    if count < 1:
        raise SelectionError(f"requested board count must be positive, got {count}")

    unique = _unique_candidates(candidates)
    seen = set(seen_board_ids)
    available = [item for item in unique.values() if item.board_id not in seen]
    if len(available) < count:
        raise SelectionError(
            f"requested {count} unseen boards, but only {len(available)} remain "
            f"out of {len(unique)} candidates"
        )

    selected: list[Candidate] = []
    selected_sources: set[str] = set()
    selected_formats: set[str] = set()
    selected_axes: set[str] = set()

    remaining = list(available)
    while len(selected) < count:
        def score(item: Candidate) -> int:
            return (
                (10_000 if item.input_format not in selected_formats else 0)
                + (1_000 if item.source_id not in selected_sources else 0)
                + 100 * len(set(item.axes) - selected_axes)
            )

        choice = min(
            remaining,
            key=lambda item: (-score(item), _rank(seed, "choice", item.board_id)),
        )
        selected.append(choice)
        selected_sources.add(choice.source_id)
        selected_formats.add(choice.input_format)
        selected_axes.update(choice.axes)
        remaining.remove(choice)
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

        required_iteration_fields: dict[str, type] = {
            "iteration_id": str,
            "planned_at": str,
            "seed": str,
            "entropy": str,
            "prior_history_sha256": str,
            "status": str,
            "selector_version": int,
            "candidate_count": int,
            "candidate_pool_sha256": str,
            "manifest_sha256": str,
            "tool_commit": str,
            "boards": list,
        }
        for field, expected_type in required_iteration_fields.items():
            value = entry.get(field)
            if not isinstance(value, expected_type) or (
                expected_type is str and not value
            ):
                raise HistoryError(f"{path.name}:{line_number}: iteration is missing {field}")
        if entry["status"] != "planned":
            raise HistoryError(f"{path.name}:{line_number}: unsupported status {entry['status']!r}")
        if entry["selector_version"] != SELECTOR_VERSION:
            raise HistoryError(
                f"{path.name}:{line_number}: unsupported selector_version "
                f"{entry['selector_version']!r}"
            )
        if entry["candidate_count"] < 5:
            raise HistoryError(f"{path.name}:{line_number}: candidate_count must be at least 5")
        for digest_field in (
            "entropy",
            "prior_history_sha256",
            "candidate_pool_sha256",
            "manifest_sha256",
        ):
            if re.fullmatch(r"[0-9a-f]{64}", entry[digest_field]) is None:
                raise HistoryError(
                    f"{path.name}:{line_number}: iteration has invalid {digest_field}"
                )
        if re.fullmatch(r"[0-9a-f]{40}", entry["tool_commit"]) is None:
            raise HistoryError(f"{path.name}:{line_number}: iteration has invalid tool_commit")

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

        if len(boards) != 5:
            raise HistoryError(
                f"{path.name}:{line_number}: iteration must contain exactly 5 unique boards"
            )

        entry_board_ids: list[str] = []
        required_board_fields = (
            "board_id",
            "sha256",
            "source_id",
            "revision",
            "relative_path",
            "input_format",
        )
        for board_index, board in enumerate(boards):
            if not isinstance(board, dict):
                raise HistoryError(
                    f"{path.name}:{line_number}: boards[{board_index}] must be an object"
                )
            for field in required_board_fields:
                value = board.get(field)
                if not isinstance(value, str) or not value:
                    raise HistoryError(
                        f"{path.name}:{line_number}: boards[{board_index}] is missing {field}"
                    )
            board_id = board["board_id"]
            if re.fullmatch(r"board:[0-9a-f]{64}", board_id) is None:
                raise HistoryError(
                    f"{path.name}:{line_number}: boards[{board_index}] has invalid board_id"
                )
            if re.fullmatch(r"[0-9a-f]{64}", board["sha256"]) is None:
                raise HistoryError(
                    f"{path.name}:{line_number}: boards[{board_index}] has invalid sha256"
                )
            relative = Path(board["relative_path"])
            if relative.is_absolute() or ".." in relative.parts:
                raise HistoryError(
                    f"{path.name}:{line_number}: boards[{board_index}] has unsafe relative_path"
                )
            entry_board_ids.append(board_id)

        if len(set(entry_board_ids)) != 5:
            raise HistoryError(
                f"{path.name}:{line_number}: iteration must contain exactly 5 unique boards"
            )
        for board_id in entry_board_ids:
            if board_id in seen:
                raise HistoryError(
                    f"{path.name}:{line_number}: board {board_id!r} was already used "
                    "by an earlier iteration"
                )
            seen.add(board_id)

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


def _board_record(item: Candidate) -> dict:
    return {
        "board_id": item.board_id,
        "sha256": item.sha256,
        "source_id": item.source_id,
        "revision": item.revision,
        "relative_path": item.relative_path,
        "input_format": item.input_format,
        "axes": list(item.axes),
        "bundle_members": list(item.bundle_members),
    }


def reserve_iteration(
    history_path: Path,
    candidates: Iterable[Candidate],
    *,
    entropy: str,
    iteration_id: str,
    planned_at: str,
    manifest_sha256: str,
    tool_commit: str,
) -> dict:
    """Append one planned iteration after checking its ID and unseen sample."""

    if not iteration_id:
        raise HistoryError("iteration id must not be empty")
    if re.fullmatch(r"[0-9a-f]{64}", entropy) is None:
        raise SelectionError("entropy must be 32 random bytes encoded as lowercase hex")
    if re.fullmatch(r"[0-9a-f]{64}", manifest_sha256) is None:
        raise SelectionError("manifest_sha256 must be a lowercase SHA-256 digest")
    if re.fullmatch(r"[0-9a-f]{40}", tool_commit) is None:
        raise SelectionError("tool_commit must be a full lowercase Git commit")

    candidate_list = list(candidates)
    for item in candidate_list:
        expected_kind_exists = (
            item.absolute_path.is_dir() if item.bundle_members else item.absolute_path.is_file()
        )
        if item.absolute_path.is_symlink() or not expected_kind_exists:
            raise SelectionError(f"candidate changed after discovery: {item.relative_path}")
        try:
            current_digest = _content_digest(item.absolute_path, item.bundle_members)
        except OSError as error:
            raise SelectionError(
                f"candidate changed after discovery: {item.relative_path}"
            ) from error
        if current_digest != item.sha256:
            raise SelectionError(f"candidate changed after discovery: {item.relative_path}")

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
        existing_text = handle.read()
        history = _parse_history(existing_text, history_path)
        if any(entry["iteration_id"] == iteration_id for entry in history.iterations):
            raise HistoryError(f"iteration id {iteration_id!r} already exists")

        prior_history_sha256 = hashlib.sha256(existing_text.encode("utf-8")).hexdigest()
        seed = hashlib.sha256(
            "\0".join(
                (
                    f"unseen-five-selector-v{SELECTOR_VERSION}",
                    iteration_id,
                    prior_history_sha256,
                    entropy,
                )
            ).encode("utf-8")
        ).hexdigest()
        selected = select_unseen(
            candidate_list,
            history.seen_board_ids,
            count=5,
            seed=seed,
        )
        entry = {
            "iteration_id": iteration_id,
            "planned_at": planned_at,
            "seed": seed,
            "entropy": entropy,
            "prior_history_sha256": prior_history_sha256,
            "status": "planned",
            "selector_version": SELECTOR_VERSION,
            "candidate_count": len(_unique_candidates(candidate_list)),
            "candidate_pool_sha256": candidate_pool_digest(candidate_list),
            "manifest_sha256": manifest_sha256,
            "tool_commit": tool_commit,
            "boards": [_board_record(item) for item in selected],
        }
        encoded = json.dumps(entry, sort_keys=True, separators=(",", ":")) + "\n"
        handle.seek(0, os.SEEK_END)
        handle.write(encoded)
        handle.flush()
        os.fsync(handle.fileno())
        return entry


def current_tool_commit() -> str:
    """Return the exact repository commit running the release selector."""

    repository = Path(__file__).resolve().parent.parent
    result = subprocess.run(
        ["git", "-C", str(repository), "rev-parse", "HEAD"],
        check=False,
        capture_output=True,
        text=True,
    )
    commit = result.stdout.strip()
    if result.returncode != 0 or re.fullmatch(r"[0-9a-f]{40}", commit) is None:
        detail = result.stderr.strip() or "git did not return a full commit"
        raise SelectionError(f"cannot identify selector commit: {detail}")
    return commit


def _utc_now() -> str:
    return datetime.now(timezone.utc).isoformat(timespec="seconds").replace("+00:00", "Z")


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="python -m qc.unseen_boards",
        description="Plan or resume an auditable five-board release UX trial.",
    )
    commands = parser.add_subparsers(dest="command", required=True)

    plan = commands.add_parser("plan", help="reserve five never-before-used boards")
    plan.add_argument("--candidate-root", type=Path, required=True)
    plan.add_argument("--manifest", type=Path, required=True)
    plan.add_argument("--history", type=Path, required=True)
    plan.add_argument("--iteration-id", required=True)
    plan.add_argument("--planned-at", default=None)

    show = commands.add_parser("show", help="print a reserved iteration for an exact retry")
    show.add_argument("--history", type=Path, required=True)
    show.add_argument("--iteration-id", required=True)
    return parser


def main(argv: list[str] | None = None) -> int:
    """Command entry point; returns 2 for invalid evidence rather than traceback."""

    parser = _parser()
    try:
        args = parser.parse_args(argv)
    except SystemExit as error:
        return int(error.code)

    try:
        if args.command == "show":
            history = load_history(args.history)
            entry = next(
                (
                    item
                    for item in history.iterations
                    if item["iteration_id"] == args.iteration_id
                ),
                None,
            )
            if entry is None:
                raise HistoryError(f"iteration id {args.iteration_id!r} does not exist")
        else:
            manifest_bytes = args.manifest.read_bytes()
            candidates = discover_candidates(
                args.candidate_root,
                manifest_path=args.manifest,
            )
            entry = reserve_iteration(
                args.history,
                candidates,
                entropy=secrets.token_hex(32),
                iteration_id=args.iteration_id,
                planned_at=args.planned_at or _utc_now(),
                manifest_sha256=hashlib.sha256(manifest_bytes).hexdigest(),
                tool_commit=current_tool_commit(),
            )
    except (HistoryError, SelectionError, OSError) as error:
        print(f"unseen-board trial: {error}", file=sys.stderr)
        return 2

    print(json.dumps(entry, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
