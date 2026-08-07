from __future__ import annotations

import json
import hashlib
import inspect
import io
import tarfile
import subprocess
import tempfile
import unittest
import zipfile
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path
from unittest.mock import patch

from qc.unseen_boards import (
    Candidate,
    HistoryError,
    SelectionError,
    candidate_pool_digest,
    discover_candidates,
    load_history,
    main,
    materialize_candidate,
    reserve_iteration,
    select_unseen,
)
import qc.unseen_boards as unseen_boards
import qc.release_board_gates as release_gates


def candidate(
    number: int,
    *,
    source: str | None = None,
    input_format: str = "kicad_pcb",
    axes: tuple[str, ...] = (),
    bundle_members: tuple[str, ...] = (),
) -> Candidate:
    digest = f"{number:064x}"
    return Candidate(
        board_id=f"board:{digest}",
        sha256=digest,
        source_id=source or f"source-{number}",
        revision=f"revision-{number}",
        relative_path=f"board-{number}.kicad_pcb",
        absolute_path=Path(f"/corpus/source-{number}/board-{number}.kicad_pcb"),
        input_format=input_format,
        axes=axes,
        bundle_members=bundle_members,
    )


def board_record(number: int) -> dict[str, str]:
    item = candidate(number)
    return {
        "board_id": item.board_id,
        "sha256": item.sha256,
        "source_id": item.source_id,
        "revision": item.revision,
        "relative_path": item.relative_path,
        "input_format": item.input_format,
        "bundle_members": list(item.bundle_members),
    }


def iteration_record(iteration_id: str, boards: list[dict[str, str]]) -> dict:
    return {
        "iteration_id": iteration_id,
        "planned_at": "2026-08-06T12:00:00Z",
        "seed": "test-seed",
        "entropy": "a" * 64,
        "prior_history_sha256": hashlib.sha256(b"").hexdigest(),
        "status": "planned",
        "selector_version": 1,
        "candidate_count": 12,
        "candidate_pool_sha256": "c" * 64,
        "manifest_sha256": "d" * 64,
        "tool_commit": "e" * 40,
        "boards": boards,
    }


def filesystem_candidates(root: Path, count: int) -> list[Candidate]:
    root.mkdir(parents=True, exist_ok=True)
    result = []
    for number in range(count):
        path = root / f"board-{number}.kicad_pcb"
        contents = f"(kicad_pcb board-{number})".encode()
        path.write_bytes(contents)
        digest = hashlib.sha256(contents).hexdigest()
        logical = hashlib.sha256(
            f"source-{number}\0board-{number}".encode()
        ).hexdigest()
        result.append(
            Candidate(
                board_id=f"board:{logical}",
                sha256=digest,
                source_id=f"source-{number}",
                revision=f"revision-{number}",
                relative_path=path.name,
                absolute_path=path,
                input_format="kicad_pcb",
                axes=(),
                bundle_members=(),
            )
        )
    return result


def manifest_pool(
    base: Path,
    name: str,
    count: int,
    *,
    cohort: str,
    duplicate_payload: bytes | None = None,
) -> tuple[Path, Path]:
    root = base / name
    root.mkdir()
    rows = [f'cohort = "{cohort}"', ""]
    for number in range(count):
        source_id = f"{name}-{number}"
        source = root / source_id
        source.mkdir()
        payload = (
            duplicate_payload
            if number == 0 and duplicate_payload is not None
            else (f"(kicad_pcb {name}-{number})".encode())
        )
        (source / "board.kicad_pcb").write_bytes(payload)
        if cohort == "external":
            subprocess.run(["git", "init", "-q", str(source)], check=True)
            subprocess.run(
                [
                    "git",
                    "-C",
                    str(source),
                    "remote",
                    "add",
                    "origin",
                    f"https://example.invalid/{source_id}",
                ],
                check=True,
            )
            subprocess.run(
                ["git", "-C", str(source), "add", "board.kicad_pcb"], check=True
            )
            subprocess.run(
                [
                    "git",
                    "-C",
                    str(source),
                    "-c",
                    "user.name=Hauksbee Test",
                    "-c",
                    "user.email=hauksbee-test@example.invalid",
                    "commit",
                    "-q",
                    "-m",
                    "fixture",
                ],
                check=True,
            )
            revision = subprocess.run(
                ["git", "-C", str(source), "rev-parse", "HEAD"],
                check=True,
                capture_output=True,
                text=True,
            ).stdout.strip()
        else:
            revision = f"{number + 1:040x}"
        (source / ".hauksbee-rev").write_text(revision + "\n")
        rows.extend(
            [
                "[[board]]",
                f'id = "{source_id}"',
                f'url = "https://example.invalid/{source_id}"',
                f'rev = "{revision}"',
                'license = "MIT"',
                "license_confirmed = true",
                'axes = ["kicad", "dev-board"]',
                'expect = ["board.kicad_pcb"]',
                "",
            ]
        )
    manifest = base / f"{name}.toml"
    manifest.write_text("\n".join(rows))
    return root, manifest


def successful_browser_runner(captured: list[Path] | None = None):
    def run(paths: list[Path], output: Path, base_url: str, cohort: str) -> int:
        if captured is not None:
            captured.extend(paths)
        output.mkdir(parents=True, exist_ok=True)
        results = []
        for path in paths:
            results.append(
                {
                    "path": str(path.resolve()),
                    "file": path.name,
                    "elapsed_ms": 12,
                    "response_status": 200,
                    "response_capture_error": None,
                    "report": {
                        "ok": True,
                        "file_name": path.name,
                        "num_components": 2,
                        "num_nets": 3,
                        "headline": "Useful report",
                        "sections": [{"name": "connectivity"}],
                    },
                    "exported": True,
                    "console_errors": [],
                    "failures": [],
                }
            )
        (output / "results.json").write_text(
            json.dumps({"base": base_url, "cohort": cohort, "results": results})
        )
        return 0

    return run


class DiscoveryTests(unittest.TestCase):
    def test_discovers_primary_board_inputs_and_deduplicates_the_same_content(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as raw:
            corpus = Path(raw)
            alpha = corpus / "alpha"
            beta = corpus / "beta"
            gamma = corpus / "nested" / "gamma"
            for source, revision in (
                (alpha, "a" * 40),
                (beta, "b" * 40),
                (gamma, "c" * 40),
            ):
                source.mkdir(parents=True)
                (source / ".hauksbee-rev").write_text(revision + "\n")

            # A project with both schematic and PCB is one board. The PCB is the
            # drag-and-drop input because it carries the layout checks too.
            (alpha / "controller.kicad_pcb").write_text("(kicad_pcb alpha)")
            (alpha / "controller.kicad_sch").write_text("(kicad_sch alpha)")

            # Backup trees are not current board revisions and must not turn a
            # single upstream project into dozens of supposedly unseen boards.
            backups = alpha / "controller-backups"
            backups.mkdir()
            (backups / "controller-2025.kicad_pcb").write_text("old")

            # Extension matching is case-insensitive for real Altium exports.
            (beta / "motor.PcbDoc").write_bytes(b"binary-altium")

            # Identical files in two repositories are the same unseen input,
            # not two independent trials.
            (gamma / "motor-copy.PcbDoc").write_bytes(b"binary-altium")

            # The release trial follows the corpus manifest, not a blind file
            # crawl. Upstreams routinely carry panels, block diagrams, and old
            # revisions beside the actual board named by `expect`.
            (alpha / "controller-panel.kicad_pcb").write_text("manufacturing panel")
            (alpha / "Block Diagram.kicad_sch").write_text("not a board")
            manifest = corpus / "corpus.toml"
            manifest.write_text(
                """
                [[board]]
                id = "alpha"
                url = "https://example.invalid/alpha"
                rev = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                license = "MIT"
                axes = ["kicad"]
                expect = ["controller.kicad_pcb", "controller.kicad_sch"]

                [[board]]
                id = "beta"
                url = "https://example.invalid/beta"
                rev = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                license = "MIT"
                axes = ["altium"]
                expect = ["motor.PcbDoc"]

                [[board]]
                id = "gamma"
                dest = "nested/gamma"
                url = "https://example.invalid/gamma"
                rev = "cccccccccccccccccccccccccccccccccccccccc"
                license = "MIT"
                axes = ["altium"]
                expect = ["motor-copy.PcbDoc"]
                """
            )

            found = discover_candidates(corpus, manifest_path=manifest)

            self.assertEqual(2, len(found))
            by_format = {item.input_format: item for item in found}
            self.assertEqual(
                "alpha/controller.kicad_pcb", by_format["kicad_pcb"].relative_path
            )
            self.assertEqual("alpha", by_format["kicad_pcb"].source_id)
            self.assertEqual("a" * 40, by_format["kicad_pcb"].revision)
            self.assertEqual("kicad_pcb", by_format["kicad_pcb"].input_format)
            self.assertEqual("altium_pcbdoc", by_format["altium_pcbdoc"].input_format)
            self.assertTrue(all(item.board_id.startswith("board:") for item in found))
            self.assertTrue(
                all(len(item.board_id) == len("board:") + 64 for item in found)
            )
            self.assertFalse(any("backup" in item.relative_path for item in found))
            self.assertFalse(any("panel" in item.relative_path for item in found))
            self.assertFalse(
                any("Block Diagram" in item.relative_path for item in found)
            )

    def test_manifest_discovery_refuses_an_incomplete_corpus(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            corpus = Path(raw) / "board-corpus"
            corpus.mkdir()
            manifest = Path(raw) / "corpus.toml"
            manifest.write_text(
                """
                [[board]]
                id = "missing-board"
                url = "https://example.invalid/missing"
                rev = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                license = "MIT"
                axes = ["kicad"]
                expect = ["missing.kicad_pcb"]
                """
            )

            with self.assertRaisesRegex(
                SelectionError,
                r"corpus is incomplete: missing-board/ does not exist",
            ):
                discover_candidates(corpus, manifest_path=manifest)

    def test_manifest_discovery_refuses_a_revision_marker_that_does_not_match_the_pin(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as raw:
            base = Path(raw)
            corpus = base / "board-corpus"
            source = corpus / "alpha"
            source.mkdir(parents=True)
            (source / "board.kicad_pcb").write_text("(kicad_pcb alpha)")
            (source / ".hauksbee-rev").write_text("b" * 40 + "\n")
            manifest = base / "corpus.toml"
            manifest.write_text(
                """
                [[board]]
                id = "alpha"
                url = "https://example.invalid/alpha"
                rev = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                license = "MIT"
                axes = ["kicad"]
                expect = ["board.kicad_pcb"]
                """
            )

            with self.assertRaisesRegex(
                SelectionError,
                r"alpha: fetched revision b{40} does not match manifest pin a{40}",
            ):
                discover_candidates(corpus, manifest_path=manifest)

    def test_logical_board_identity_survives_a_content_only_change(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            base = Path(raw)
            corpus = base / "board-corpus"
            source = corpus / "alpha"
            source.mkdir(parents=True)
            board = source / "board.kicad_pcb"
            board.write_text("(kicad_pcb first)")
            (source / ".hauksbee-rev").write_text("a" * 40 + "\n")
            manifest = base / "corpus.toml"
            manifest.write_text(
                """
                [[board]]
                id = "alpha"
                url = "https://example.invalid/alpha"
                rev = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                license = "MIT"
                axes = ["kicad"]
                expect = ["board.kicad_pcb"]
                """
            )

            first = discover_candidates(corpus, manifest_path=manifest)[0]
            board.write_text("(kicad_pcb second)")
            second = discover_candidates(corpus, manifest_path=manifest)[0]

            self.assertEqual(first.board_id, second.board_id)
            self.assertNotEqual(first.sha256, second.sha256)

    def test_crawl_does_not_follow_a_board_symlink_outside_the_candidate_root(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as raw:
            base = Path(raw)
            corpus = base / "candidate-pool"
            corpus.mkdir()
            private_board = base / "private.kicad_pcb"
            private_board.write_text("(kicad_pcb private)")
            (corpus / "looks-public.kicad_pcb").symlink_to(private_board)

            self.assertEqual([], discover_candidates(corpus))

    def test_manifest_groups_loose_gerber_films_into_one_reproducible_drop(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as raw:
            base = Path(raw)
            corpus = base / "board-corpus"
            source = corpus / "films"
            source.mkdir(parents=True)
            (source / ".hauksbee-rev").write_text("a" * 40 + "\n")
            (source / "board.GTL").write_text("top copper")
            (source / "board.GBL").write_text("bottom copper")
            (source / "board-RoundHoles.TXT").write_text("drill")
            manifest = base / "corpus.toml"
            manifest.write_text(
                """
                [[board]]
                id = "films"
                url = "https://example.invalid/films"
                rev = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                license = "MIT"
                axes = ["gerber-only"]
                expect = ["board.GTL", "board.GBL", "board-RoundHoles.TXT"]
                """
            )

            found = discover_candidates(corpus, manifest_path=manifest)

            self.assertEqual(1, len(found))
            self.assertEqual("gerber_bundle", found[0].input_format)
            self.assertEqual(
                ("board-RoundHoles.TXT", "board.GBL", "board.GTL"),
                found[0].bundle_members,
            )
            staged = materialize_candidate(found[0], base / "staged")
            self.assertEqual(".zip", staged.suffix)
            with zipfile.ZipFile(staged) as archive:
                self.assertEqual(
                    list(found[0].bundle_members), sorted(archive.namelist())
                )

    def test_manifest_rejects_an_arbitrary_zip_as_a_board_input(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            base = Path(raw)
            corpus = base / "board-corpus"
            source = corpus / "archive"
            source.mkdir(parents=True)
            (source / ".hauksbee-rev").write_text("a" * 40 + "\n")
            with zipfile.ZipFile(source / "not-a-board.zip", "w") as archive:
                archive.writestr("README.txt", "no board here")
            manifest = base / "corpus.toml"
            manifest.write_text(
                """
                [[board]]
                id = "archive"
                url = "https://example.invalid/archive"
                rev = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                license = "MIT"
                axes = ["gerber-only"]
                expect = ["not-a-board.zip"]
                """
            )

            with self.assertRaisesRegex(
                SelectionError,
                r"archive: no supported drag-and-drop board input",
            ):
                discover_candidates(corpus, manifest_path=manifest)

    def test_archive_sniffing_rejects_unsafe_directory_entries(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            base = Path(raw)
            zip_path = base / "unsafe.zip"
            with zipfile.ZipFile(zip_path, "w") as archive:
                archive.writestr("../escape/", b"")
                archive.writestr("board.board", b"valid board payload")
            self.assertIsNone(unseen_boards._input_format(zip_path))

            tar_path = base / "unsafe.tar.gz"
            with tarfile.open(tar_path, "w:gz") as archive:
                unsafe = tarfile.TarInfo("../escape/")
                unsafe.type = tarfile.DIRTYPE
                archive.addfile(unsafe)
                for name in ("matrix/matrix", "steps/main/data"):
                    payload = b"odb payload"
                    member = tarfile.TarInfo(name)
                    member.size = len(payload)
                    archive.addfile(member, io.BytesIO(payload))
            self.assertIsNone(unseen_boards._input_format(tar_path))

    def test_external_manifest_requires_explicit_redistribution_provenance(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as raw:
            base = Path(raw)
            corpus = base / "external"
            source = corpus / "alpha"
            source.mkdir(parents=True)
            (source / ".hauksbee-rev").write_text("a" * 40 + "\n")
            (source / "board.kicad_pcb").write_text("(kicad_pcb alpha)")
            manifest = base / "external.toml"
            manifest.write_text(
                """cohort = "external"

[[board]]
id = "alpha"
url = "https://example.invalid/alpha"
rev = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
axes = ["kicad"]
expect = ["board.kicad_pcb"]
"""
            )

            with self.assertRaisesRegex(
                SelectionError, r"alpha: external candidate needs"
            ):
                discover_candidates(corpus, manifest_path=manifest)

    def test_external_git_candidate_must_be_a_clean_checkout_at_the_pinned_revision(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as raw:
            base = Path(raw)
            root = base / "external"
            source = root / "alpha"
            source.mkdir(parents=True)
            (source / "board.kicad_pcb").write_text("(kicad_pcb alpha)")
            (source / ".hauksbee-rev").write_text("a" * 40 + "\n")
            manifest = base / "external.toml"
            manifest.write_text(
                """cohort = "external"

[[board]]
id = "alpha"
url = "https://example.invalid/alpha"
rev = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
license = "MIT"
license_confirmed = true
axes = ["kicad"]
expect = ["board.kicad_pcb"]
"""
            )

            with self.assertRaisesRegex(SelectionError, r"not a Git checkout"):
                discover_candidates(root, manifest_path=manifest)

    def test_manifest_expect_path_must_stay_inside_its_declared_source(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            base = Path(raw)
            corpus = base / "pool"
            source = corpus / "alpha"
            other = corpus / "other"
            source.mkdir(parents=True)
            other.mkdir()
            (source / ".hauksbee-rev").write_text("a" * 40 + "\n")
            (other / "board.GTL").write_text("top copper")
            (other / "board.TXT").write_text("drill")
            manifest = base / "pool.toml"
            manifest.write_text(
                """[[board]]
id = "alpha"
url = "https://example.invalid/alpha"
rev = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
license = "MIT"
axes = ["gerber-only"]
expect = ["../other/board.GTL", "../other/board.TXT"]
"""
            )

            with self.assertRaisesRegex(SelectionError, r"escapes declared source"):
                discover_candidates(corpus, manifest_path=manifest)

    def test_arbitrary_tar_gz_is_not_claimed_as_odbpp(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            base = Path(raw)
            corpus = base / "pool"
            source = corpus / "alpha"
            source.mkdir(parents=True)
            (source / ".hauksbee-rev").write_text("a" * 40 + "\n")
            archive_path = source / "not-odb.tar.gz"
            payload = base / "README.txt"
            payload.write_text("not a board")
            with tarfile.open(archive_path, "w:gz") as archive:
                archive.add(payload, arcname="README.txt")
            manifest = base / "pool.toml"
            manifest.write_text(
                """[[board]]
id = "alpha"
url = "https://example.invalid/alpha"
rev = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
license = "MIT"
axes = ["odb"]
expect = ["not-odb.tar.gz"]
"""
            )

            with self.assertRaisesRegex(SelectionError, r"no supported drag-and-drop"):
                discover_candidates(corpus, manifest_path=manifest)

    def test_materialization_copies_a_direct_input_into_the_staging_directory(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as raw:
            base = Path(raw)
            source = base / "source" / "board.kicad_pcb"
            source.parent.mkdir()
            source.write_text("(kicad_pcb direct)")
            digest = hashlib.sha256(source.read_bytes()).hexdigest()
            item = Candidate(
                board_id="board:" + "1" * 64,
                sha256=digest,
                source_id="alpha",
                revision="a" * 40,
                relative_path="alpha/board.kicad_pcb",
                absolute_path=source,
                input_format="kicad_pcb",
            )

            staged = materialize_candidate(item, base / "staged")

            self.assertEqual(base / "staged", staged.parent)
            self.assertNotEqual(source, staged)
            self.assertEqual(source.read_bytes(), staged.read_bytes())


class SelectionTests(unittest.TestCase):
    def test_seeded_selection_is_repeatable_diverse_and_never_reuses_seen_boards(
        self,
    ) -> None:
        candidates = [candidate(i) for i in range(12)]
        already_seen = {candidates[0].board_id, candidates[1].board_id}

        first = select_unseen(candidates, already_seen, count=5, seed="iteration-one")
        repeat = select_unseen(candidates, already_seen, count=5, seed="iteration-one")

        self.assertEqual(first, repeat)
        self.assertEqual(5, len(first))
        self.assertEqual(5, len({item.source_id for item in first}))
        self.assertTrue(already_seen.isdisjoint(item.board_id for item in first))

        second = select_unseen(
            candidates,
            already_seen | {item.board_id for item in first},
            count=5,
            seed="iteration-two",
        )
        self.assertTrue(
            {item.board_id for item in first}.isdisjoint(
                item.board_id for item in second
            )
        )

    def test_refuses_to_claim_an_iteration_when_fewer_than_five_unseen_boards_exist(
        self,
    ) -> None:
        candidates = [candidate(i) for i in range(6)]
        seen = {item.board_id for item in candidates[:2]}

        with self.assertRaisesRegex(
            SelectionError,
            r"requested 5 unseen boards, but only 4 remain out of 6 candidates",
        ):
            select_unseen(candidates, seen, count=5, seed="not-enough")

    def test_selection_covers_available_formats_before_filling_the_sample(self) -> None:
        candidates = [
            candidate(0, input_format="kicad_pcb"),
            candidate(1, input_format="kicad_pcb"),
            candidate(2, input_format="kicad_pcb"),
            candidate(3, input_format="eagle_brd"),
            candidate(4, input_format="eagle_brd"),
            candidate(5, input_format="altium_pcbdoc"),
            candidate(6, input_format="ipc_2581"),
        ]

        selected = select_unseen(candidates, set(), count=5, seed="format-strata")

        self.assertEqual(
            {"kicad_pcb", "eagle_brd", "altium_pcbdoc", "ipc_2581"},
            {item.input_format for item in selected},
        )

    def test_selection_spreads_across_manifest_axes(self) -> None:
        candidates = [
            candidate(0, axes=("dev-board", "avr")),
            candidate(1, axes=("dev-board", "samd")),
            candidate(2, axes=("keyboard", "avr")),
            candidate(3, axes=("power", "stm32")),
            candidate(4, axes=("industrial-sensor", "nrf")),
            candidate(5, axes=("dev-board", "avr")),
            candidate(6, axes=("dev-board", "avr")),
        ]

        selected = select_unseen(candidates, set(), count=5, seed="axis-strata")
        selected_axes = {axis for item in selected for axis in item.axes}

        self.assertTrue(
            {"keyboard", "power", "industrial-sensor"}.issubset(selected_axes),
            selected,
        )

    def test_candidate_pool_digest_is_order_independent_and_content_sensitive(
        self,
    ) -> None:
        candidates = [candidate(i) for i in range(6)]
        same_reversed = candidate_pool_digest(reversed(candidates))

        changed = list(candidates)
        changed[0] = Candidate(**{**changed[0].__dict__, "sha256": "f" * 64})

        self.assertEqual(candidate_pool_digest(candidates), same_reversed)
        self.assertNotEqual(
            candidate_pool_digest(candidates), candidate_pool_digest(changed)
        )

    def test_selection_never_counts_duplicate_content_as_distinct_boards(self) -> None:
        original = candidate(1)
        aliases = [
            Candidate(
                **{
                    **original.__dict__,
                    "board_id": "board:" + f"{number + 100:064x}",
                    "source_id": f"alias-{number}",
                }
            )
            for number in range(5)
        ]
        with self.assertRaisesRegex(SelectionError, r"only 1 remain"):
            select_unseen(aliases, set(), count=5, seed="duplicate-content")


class HistoryTests(unittest.TestCase):
    def test_reservation_is_append_only_and_the_next_iteration_cannot_reuse_its_boards(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as raw:
            history = Path(raw) / "iterations.jsonl"
            candidates = filesystem_candidates(Path(raw) / "pool", 12)

            first = reserve_iteration(
                history,
                candidates,
                entropy="1" * 64,
                iteration_id="2026-08-06-01",
                planned_at="2026-08-06T12:00:00Z",
                manifest_sha256="d" * 64,
                tool_commit="e" * 40,
            )
            second = reserve_iteration(
                history,
                candidates,
                entropy="2" * 64,
                iteration_id="2026-08-06-02",
                planned_at="2026-08-06T13:00:00Z",
                manifest_sha256="d" * 64,
                tool_commit="e" * 40,
            )

            first_ids = {item["board_id"] for item in first["boards"]}
            second_ids = {item["board_id"] for item in second["boards"]}
            self.assertEqual(5, len(first_ids))
            self.assertEqual(5, len(second_ids))
            self.assertTrue(first_ids.isdisjoint(second_ids))

            loaded = load_history(history)
            self.assertEqual(2, len(loaded.iterations))
            self.assertEqual(first_ids | second_ids, loaded.seen_board_ids)
            self.assertEqual(
                ["2026-08-06-01", "2026-08-06-02"],
                [entry["iteration_id"] for entry in loaded.iterations],
            )

            lines = history.read_text().splitlines()
            self.assertEqual(2, len(lines))
            self.assertTrue(
                all(json.loads(line)["status"] == "planned" for line in lines)
            )
            self.assertEqual(1, first["selector_version"])
            self.assertEqual(12, first["candidate_count"])
            self.assertEqual(
                candidate_pool_digest(candidates), first["candidate_pool_sha256"]
            )
            self.assertEqual("d" * 64, first["manifest_sha256"])
            self.assertEqual("e" * 40, first["tool_commit"])

    def test_malformed_history_refuses_instead_of_forgetting_what_was_seen(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as raw:
            history = Path(raw) / "iterations.jsonl"
            boards = [board_record(i) for i in range(5)]
            history.write_text(
                json.dumps(iteration_record("ok", boards)) + "\nnot json\n"
            )

            with self.assertRaisesRegex(
                HistoryError, r"iterations\.jsonl:2: invalid JSON"
            ):
                load_history(history)

    def test_duplicate_iteration_id_refuses_before_reserving_more_boards(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            history = Path(raw) / "iterations.jsonl"
            candidates = filesystem_candidates(Path(raw) / "pool", 12)
            reserve_iteration(
                history,
                candidates,
                entropy="1" * 64,
                iteration_id="same-id",
                planned_at="2026-08-06T12:00:00Z",
                manifest_sha256="d" * 64,
                tool_commit="e" * 40,
            )

            with self.assertRaisesRegex(
                HistoryError, r"iteration id 'same-id' already exists"
            ):
                reserve_iteration(
                    history,
                    candidates,
                    entropy="2" * 64,
                    iteration_id="same-id",
                    planned_at="2026-08-06T13:00:00Z",
                    manifest_sha256="d" * 64,
                    tool_commit="e" * 40,
                )

            self.assertEqual(1, len(history.read_text().splitlines()))

    def test_history_refuses_any_iteration_that_does_not_contain_five_unique_boards(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as raw:
            history = Path(raw) / "iterations.jsonl"
            history.write_text(
                json.dumps(
                    iteration_record("short", [board_record(i) for i in range(4)])
                )
                + "\n"
            )

            with self.assertRaisesRegex(
                HistoryError,
                r"iterations\.jsonl:1: iteration must contain exactly 5 unique boards",
            ):
                load_history(history)

    def test_history_refuses_a_board_reused_across_iterations(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            history = Path(raw) / "iterations.jsonl"
            first = [board_record(i) for i in range(5)]
            second = [board_record(i) for i in range(4, 9)]
            history.write_text(
                json.dumps(iteration_record("one", first))
                + "\n"
                + json.dumps(iteration_record("two", second))
                + "\n"
            )

            with self.assertRaisesRegex(
                HistoryError,
                r"iterations\.jsonl:2: board .* was already used by an earlier iteration",
            ):
                load_history(history)

    def test_history_refuses_duplicate_content_inside_one_iteration(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            history = Path(raw) / "iterations.jsonl"
            boards = [board_record(i) for i in range(5)]
            boards[4]["sha256"] = boards[0]["sha256"]
            history.write_text(
                json.dumps(iteration_record("duplicate-content", boards)) + "\n"
            )
            with self.assertRaisesRegex(
                HistoryError, r"exactly 5 content-distinct boards"
            ):
                load_history(history)

    def test_release_reservation_has_no_caller_controlled_count_or_seed(self) -> None:
        parameters = inspect.signature(reserve_iteration).parameters
        self.assertNotIn("count", parameters)
        self.assertNotIn("seed", parameters)

    def test_history_refuses_a_board_record_with_missing_audit_metadata(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            history = Path(raw) / "iterations.jsonl"
            boards = [board_record(i) for i in range(5)]
            del boards[2]["sha256"]
            history.write_text(json.dumps(iteration_record("damaged", boards)) + "\n")

            with self.assertRaisesRegex(
                HistoryError,
                r"iterations\.jsonl:1: boards\[2\] is missing sha256",
            ):
                load_history(history)

    def test_reservation_refuses_a_candidate_changed_after_discovery(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            candidates = filesystem_candidates(Path(raw) / "pool", 6)
            candidates[0].absolute_path.write_text("changed after discovery")

            with self.assertRaisesRegex(
                SelectionError, r"candidate changed after discovery"
            ):
                reserve_iteration(
                    Path(raw) / "iterations.jsonl",
                    candidates,
                    entropy="3" * 64,
                    iteration_id="mutation",
                    planned_at="2026-08-06T12:00:00Z",
                    manifest_sha256="d" * 64,
                    tool_commit="e" * 40,
                )

    def test_history_refuses_missing_population_metadata(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            history = Path(raw) / "iterations.jsonl"
            entry = iteration_record(
                "damaged-population", [board_record(i) for i in range(5)]
            )
            del entry["manifest_sha256"]
            history.write_text(json.dumps(entry) + "\n")

            with self.assertRaisesRegex(
                HistoryError,
                r"iterations\.jsonl:1: iteration is missing manifest_sha256",
            ):
                load_history(history)

    def test_content_cannot_be_reintroduced_under_new_logical_board_ids(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            history = Path(raw) / "iterations.jsonl"
            original = filesystem_candidates(Path(raw) / "pool", 5)
            reserve_iteration(
                history,
                original,
                entropy="1" * 64,
                iteration_id="first",
                planned_at="2026-08-06T12:00:00Z",
                manifest_sha256="d" * 64,
                tool_commit="e" * 40,
            )
            relabelled = [
                Candidate(
                    **{
                        **item.__dict__,
                        "board_id": "board:" + f"{index + 100:064x}",
                        "source_id": f"renamed-{index}",
                    }
                )
                for index, item in enumerate(original)
            ]

            with self.assertRaisesRegex(SelectionError, r"only 0 remain"):
                reserve_iteration(
                    history,
                    relabelled,
                    entropy="2" * 64,
                    iteration_id="second",
                    planned_at="2026-08-06T13:00:00Z",
                    manifest_sha256="d" * 64,
                    tool_commit="e" * 40,
                )

    def test_history_detects_a_rewritten_prefix_and_a_tampered_derived_seed(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as raw:
            history = Path(raw) / "iterations.jsonl"
            candidates = filesystem_candidates(Path(raw) / "pool", 12)
            reserve_iteration(
                history,
                candidates,
                entropy="1" * 64,
                iteration_id="first",
                planned_at="2026-08-06T12:00:00Z",
                manifest_sha256="d" * 64,
                tool_commit="e" * 40,
            )
            reserve_iteration(
                history,
                candidates,
                entropy="2" * 64,
                iteration_id="second",
                planned_at="2026-08-06T13:00:00Z",
                manifest_sha256="d" * 64,
                tool_commit="e" * 40,
            )
            records = [json.loads(line) for line in history.read_text().splitlines()]
            records[0]["planned_at"] = "rewritten"
            history.write_text(
                "\n".join(
                    json.dumps(row, sort_keys=True, separators=(",", ":"))
                    for row in records
                )
                + "\n"
            )

            with self.assertRaisesRegex(HistoryError, r"prior_history_sha256"):
                load_history(history)

            # A one-record ledger has no later prefix link, so its derived seed
            # has to be independently verified too.
            one = Path(raw) / "one.jsonl"
            record = records[0]
            record["planned_at"] = "2026-08-06T12:00:00Z"
            record["seed"] = "f" * 64
            one.write_text(
                json.dumps(record, sort_keys=True, separators=(",", ":")) + "\n"
            )
            with self.assertRaisesRegex(HistoryError, r"derived seed"):
                load_history(one)

    def test_tool_commit_refuses_a_dirty_checkout(self) -> None:
        clean = unittest.mock.Mock(returncode=0, stdout="e" * 40 + "\n", stderr="")
        dirty = unittest.mock.Mock(
            returncode=0, stdout=" M qc/unseen_boards.py\n", stderr=""
        )
        with (
            patch("qc.unseen_boards.subprocess.run", side_effect=[clean, dirty]) as run,
            self.assertRaisesRegex(SelectionError, r"working tree is dirty"),
        ):
            unseen_boards.current_tool_commit()
        self.assertIn("--untracked-files=normal", run.call_args_list[1].args[0])


class CommandTests(unittest.TestCase):
    def test_release_orchestrator_api_is_available(self) -> None:
        self.assertTrue(hasattr(release_gates, "run_external_gate"))
        self.assertTrue(hasattr(release_gates, "run_corpus_gate"))
        self.assertTrue(hasattr(release_gates, "append_iteration_result"))

    def test_external_gate_binds_five_staged_content_hashes_to_redacted_completion_evidence(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as raw:
            base = Path(raw)
            external_root, external_manifest = manifest_pool(
                base, "external", 7, cohort="external"
            )
            corpus_root, corpus_manifest = manifest_pool(
                base, "corpus", 2, cohort="corpus"
            )
            captured: list[Path] = []
            history = base / "history.jsonl"
            evidence_dir = base / "evidence"

            evidence = release_gates.run_external_gate(
                external_root=external_root,
                external_manifest=external_manifest,
                corpus_root=corpus_root,
                corpus_manifest=corpus_manifest,
                history_path=history,
                evidence_dir=evidence_dir,
                scratch_root=base / "scratch",
                iteration_id="release-01",
                planned_at="2026-08-06T12:00:00Z",
                base_url="http://127.0.0.1:37651",
                entropy="a" * 64,
                tool_commit="e" * 40,
                runner=successful_browser_runner(captured),
            )

            self.assertEqual("completed", evidence["status"])
            self.assertEqual(5, len(evidence["boards"]))
            self.assertEqual(5, len({row["sha256"] for row in evidence["boards"]}))
            self.assertTrue(
                all(len(row["staged_sha256"]) == 64 for row in evidence["boards"])
            )
            self.assertTrue(
                all(row["cohort"] == "external" for row in evidence["boards"])
            )
            self.assertEqual(5, len(captured))
            self.assertEqual(5, len(set(captured)))
            self.assertTrue(all(path.parent.name == "inputs" for path in captured))
            retained = (evidence_dir / "release-01.json").read_text()
            self.assertNotIn(str(base), retained)
            loaded = load_history(history)
            self.assertEqual(1, len(loaded.iterations))
            self.assertEqual(1, len(loaded.results))
            self.assertEqual("completed", loaded.results[0]["status"])
            self.assertEqual(
                hashlib.sha256(
                    (evidence_dir / "release-01.json").read_bytes()
                ).hexdigest(),
                loaded.results[0]["evidence_sha256"],
            )

    def test_external_gate_excludes_content_already_present_in_the_known_corpus(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as raw:
            base = Path(raw)
            shared = b"(kicad_pcb already-known)"
            corpus_root, corpus_manifest = manifest_pool(
                base, "corpus", 1, cohort="corpus", duplicate_payload=shared
            )
            external_root, external_manifest = manifest_pool(
                base, "external", 5, cohort="external", duplicate_payload=shared
            )

            with self.assertRaisesRegex(SelectionError, r"only 4 remain"):
                release_gates.run_external_gate(
                    external_root=external_root,
                    external_manifest=external_manifest,
                    corpus_root=corpus_root,
                    corpus_manifest=corpus_manifest,
                    history_path=base / "history.jsonl",
                    evidence_dir=base / "evidence",
                    scratch_root=base / "scratch",
                    iteration_id="release-01",
                    planned_at="2026-08-06T12:00:00Z",
                    base_url="http://127.0.0.1:37651",
                    entropy="a" * 64,
                    tool_commit="e" * 40,
                    runner=successful_browser_runner(),
                )

    def test_external_gate_appends_failed_terminal_evidence_when_browser_gate_fails(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as raw:
            base = Path(raw)
            external_root, external_manifest = manifest_pool(
                base, "external", 5, cohort="external"
            )
            corpus_root, corpus_manifest = manifest_pool(
                base, "corpus", 1, cohort="corpus"
            )
            history = base / "history.jsonl"
            evidence_dir = base / "evidence"

            def failing(
                paths: list[Path], output: Path, base_url: str, cohort: str
            ) -> int:
                successful_browser_runner()(paths, output, base_url, cohort)
                artifact = json.loads((output / "results.json").read_text())
                artifact["results"][0]["failures"] = ["report was not useful"]
                (output / "results.json").write_text(json.dumps(artifact))
                return 1

            with self.assertRaisesRegex(SelectionError, r"browser runner exited"):
                release_gates.run_external_gate(
                    external_root=external_root,
                    external_manifest=external_manifest,
                    corpus_root=corpus_root,
                    corpus_manifest=corpus_manifest,
                    history_path=history,
                    evidence_dir=evidence_dir,
                    scratch_root=base / "scratch",
                    iteration_id="release-failed",
                    planned_at="2026-08-06T12:00:00Z",
                    base_url="http://127.0.0.1:37651",
                    entropy="a" * 64,
                    tool_commit="e" * 40,
                    runner=failing,
                )

            loaded = load_history(history)
            self.assertEqual("failed", loaded.results[0]["status"])
            retained_path = evidence_dir / "release-failed.json"
            retained = json.loads(retained_path.read_text())
            self.assertEqual("failed", retained["status"])
            self.assertEqual(1, retained["browser_exit_code"])
            self.assertNotIn(str(base), retained_path.read_text())

    def test_external_gate_records_a_runner_launch_exception_as_failed(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            base = Path(raw)
            external_root, external_manifest = manifest_pool(
                base, "external", 5, cohort="external"
            )
            corpus_root, corpus_manifest = manifest_pool(
                base, "corpus", 1, cohort="corpus"
            )
            history = base / "history.jsonl"

            def unavailable(
                paths: list[Path], output: Path, base_url: str, cohort: str
            ) -> int:
                raise OSError("browser executable is unavailable")

            with self.assertRaisesRegex(SelectionError, r"could not start"):
                release_gates.run_external_gate(
                    external_root=external_root,
                    external_manifest=external_manifest,
                    corpus_root=corpus_root,
                    corpus_manifest=corpus_manifest,
                    history_path=history,
                    evidence_dir=base / "evidence",
                    scratch_root=base / "scratch",
                    iteration_id="runner-unavailable",
                    planned_at="2026-08-06T12:00:00Z",
                    base_url="http://127.0.0.1:37651",
                    entropy="a" * 64,
                    tool_commit="e" * 40,
                    runner=unavailable,
                )
            self.assertEqual("failed", load_history(history).results[0]["status"])

    def test_external_gate_records_a_staging_failure_as_failed(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            base = Path(raw)
            external_root, external_manifest = manifest_pool(
                base, "external", 5, cohort="external"
            )
            corpus_root, corpus_manifest = manifest_pool(
                base, "corpus", 1, cohort="corpus"
            )
            history = base / "history.jsonl"
            with (
                patch(
                    "qc.release_board_gates.materialize_candidate",
                    side_effect=OSError("staging disk is unavailable"),
                ),
                self.assertRaisesRegex(SelectionError, r"could not stage"),
            ):
                release_gates.run_external_gate(
                    external_root=external_root,
                    external_manifest=external_manifest,
                    corpus_root=corpus_root,
                    corpus_manifest=corpus_manifest,
                    history_path=history,
                    evidence_dir=base / "evidence",
                    scratch_root=base / "scratch",
                    iteration_id="staging-unavailable",
                    planned_at="2026-08-06T12:00:00Z",
                    base_url="http://127.0.0.1:37651",
                    entropy="a" * 64,
                    tool_commit="e" * 40,
                    runner=successful_browser_runner(),
                )
            self.assertEqual("failed", load_history(history).results[0]["status"])

    def test_external_gate_marks_a_zero_exit_artifact_mismatch_failed(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            base = Path(raw)
            external_root, external_manifest = manifest_pool(
                base, "external", 5, cohort="external"
            )
            corpus_root, corpus_manifest = manifest_pool(
                base, "corpus", 1, cohort="corpus"
            )
            history = base / "history.jsonl"

            def mismatched(
                paths: list[Path], output: Path, base_url: str, cohort: str
            ) -> int:
                successful_browser_runner()(paths, output, base_url, cohort)
                artifact = json.loads((output / "results.json").read_text())
                artifact["results"][0]["path"] = str(base / "different.kicad_pcb")
                (output / "results.json").write_text(json.dumps(artifact))
                return 0

            with self.assertRaisesRegex(SelectionError, r"do not exactly match"):
                release_gates.run_external_gate(
                    external_root=external_root,
                    external_manifest=external_manifest,
                    corpus_root=corpus_root,
                    corpus_manifest=corpus_manifest,
                    history_path=history,
                    evidence_dir=base / "evidence",
                    scratch_root=base / "scratch",
                    iteration_id="artifact-mismatch",
                    planned_at="2026-08-06T12:00:00Z",
                    base_url="http://127.0.0.1:37651",
                    entropy="a" * 64,
                    tool_commit="e" * 40,
                    runner=mismatched,
                )

            self.assertEqual("failed", load_history(history).results[0]["status"])

    def test_external_gate_refuses_if_a_staged_input_changes_during_the_browser_run(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as raw:
            base = Path(raw)
            external_root, external_manifest = manifest_pool(
                base, "external", 5, cohort="external"
            )
            corpus_root, corpus_manifest = manifest_pool(
                base, "corpus", 1, cohort="corpus"
            )
            history = base / "history.jsonl"

            def mutating(
                paths: list[Path], output: Path, base_url: str, cohort: str
            ) -> int:
                code = successful_browser_runner()(paths, output, base_url, cohort)
                paths[0].write_text("changed during run")
                return code

            with self.assertRaisesRegex(SelectionError, r"staged input changed"):
                release_gates.run_external_gate(
                    external_root=external_root,
                    external_manifest=external_manifest,
                    corpus_root=corpus_root,
                    corpus_manifest=corpus_manifest,
                    history_path=history,
                    evidence_dir=base / "evidence",
                    scratch_root=base / "scratch",
                    iteration_id="mutated-input",
                    planned_at="2026-08-06T12:00:00Z",
                    base_url="http://127.0.0.1:37651",
                    entropy="a" * 64,
                    tool_commit="e" * 40,
                    runner=mutating,
                )
            self.assertEqual("failed", load_history(history).results[0]["status"])

    def test_retained_browser_evidence_redacts_absolute_paths_at_any_depth(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as raw:
            base = Path(raw)
            external_root, external_manifest = manifest_pool(
                base, "external", 5, cohort="external"
            )
            corpus_root, corpus_manifest = manifest_pool(
                base, "corpus", 1, cohort="corpus"
            )

            def path_leaking(
                paths: list[Path], output: Path, base_url: str, cohort: str
            ) -> int:
                successful_browser_runner()(paths, output, base_url, cohort)
                artifact = json.loads((output / "results.json").read_text())
                artifact["results"][0]["report"]["diagnostic"] = {
                    "detail": f"reader opened {paths[0].resolve()}",
                    "windows": r"reader opened C:\Users\release\board.kicad_pcb",
                }
                (output / "results.json").write_text(json.dumps(artifact))
                return 0

            evidence_dir = base / "evidence"
            release_gates.run_external_gate(
                external_root=external_root,
                external_manifest=external_manifest,
                corpus_root=corpus_root,
                corpus_manifest=corpus_manifest,
                history_path=base / "history.jsonl",
                evidence_dir=evidence_dir,
                scratch_root=base / "scratch",
                iteration_id="redaction",
                planned_at="2026-08-06T12:00:00Z",
                base_url="http://127.0.0.1:37651",
                entropy="a" * 64,
                tool_commit="e" * 40,
                runner=path_leaking,
            )
            retained = (evidence_dir / "redaction.json").read_text()
            self.assertNotIn(str(base), retained)
            self.assertNotIn(r"C:\\Users\\release", retained)
            self.assertIn("inputs/", retained)

    def test_corpus_gate_runs_every_discovered_candidate_not_a_five_board_sample(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as raw:
            base = Path(raw)
            corpus_root, corpus_manifest = manifest_pool(
                base, "corpus", 7, cohort="corpus"
            )
            captured: list[Path] = []

            evidence = release_gates.run_corpus_gate(
                corpus_root=corpus_root,
                corpus_manifest=corpus_manifest,
                evidence_dir=base / "evidence",
                scratch_root=base / "scratch",
                run_id="corpus-01",
                base_url="http://127.0.0.1:37651",
                tool_commit="e" * 40,
                runner=successful_browser_runner(captured),
            )

            self.assertEqual(7, len(captured))
            self.assertEqual(7, evidence["candidate_count"])
            self.assertEqual(7, len(evidence["boards"]))
            self.assertNotIn(
                str(base), (base / "evidence" / "corpus-01.json").read_text()
            )

    def test_release_cli_uses_the_canonical_ledger_and_has_a_distinct_corpus_mode(
        self,
    ) -> None:
        repository = Path(unseen_boards.__file__).resolve().parent.parent
        stdout = io.StringIO()
        with (
            patch("qc.unseen_boards.current_tool_commit", return_value="e" * 40),
            patch(
                "qc.release_board_gates.run_external_gate",
                return_value={"status": "completed"},
            ) as external,
            redirect_stdout(stdout),
        ):
            code = main(
                [
                    "run-external-five",
                    "--external-root",
                    "/external",
                    "--external-manifest",
                    "/external.toml",
                    "--corpus-root",
                    "/corpus",
                    "--corpus-manifest",
                    "/corpus.toml",
                    "--iteration-id",
                    "release-01",
                    "--base-url",
                    "http://127.0.0.1:37651",
                    "--planned-at",
                    "2026-08-06T12:00:00Z",
                ]
            )
        self.assertEqual(0, code)
        self.assertEqual(
            repository / "qc/evidence/unseen-external-history.jsonl",
            external.call_args.kwargs["history_path"],
        )
        self.assertEqual(
            repository / "qc/evidence/runs", external.call_args.kwargs["evidence_dir"]
        )

        with (
            patch("qc.unseen_boards.current_tool_commit", return_value="e" * 40),
            patch(
                "qc.release_board_gates.run_corpus_gate",
                return_value={"status": "completed"},
            ) as corpus,
            redirect_stdout(io.StringIO()),
        ):
            code = main(
                [
                    "run-corpus",
                    "--corpus-root",
                    "/corpus",
                    "--corpus-manifest",
                    "/corpus.toml",
                    "--run-id",
                    "corpus-01",
                    "--base-url",
                    "http://127.0.0.1:37651",
                ]
            )
        self.assertEqual(0, code)
        self.assertEqual("corpus-01", corpus.call_args.kwargs["run_id"])

        with (
            patch(
                "qc.release_board_gates.audit_release_history",
                return_value={"reservations": 1, "completed": 1, "failed": 0},
            ) as audit,
            redirect_stdout(io.StringIO()),
        ):
            code = main(["audit-history", "--require-completed"])
        self.assertEqual(0, code)
        self.assertTrue(audit.call_args.kwargs["require_completed"])

    def test_release_history_audit_requires_terminal_results_and_matching_evidence(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as raw:
            base = Path(raw)
            with self.assertRaisesRegex(HistoryError, r"no completed external-five"):
                release_gates.audit_release_history(
                    base / "empty.jsonl", base / "evidence", require_completed=True
                )
            history = base / "history.jsonl"
            candidates = filesystem_candidates(base / "pool", 5)
            reservation = reserve_iteration(
                history,
                candidates,
                entropy="a" * 64,
                iteration_id="planned-only",
                planned_at="2026-08-06T12:00:00Z",
                manifest_sha256="d" * 64,
                tool_commit="e" * 40,
            )
            with self.assertRaisesRegex(HistoryError, r"has no terminal result"):
                release_gates.audit_release_history(history, base / "evidence")

            evidence_dir = base / "evidence"
            evidence_dir.mkdir()
            artifact = evidence_dir / "planned-only.json"
            artifact.write_text(
                json.dumps(
                    {
                        "schema_version": 1,
                        "gate": "external-five",
                        "iteration_id": "planned-only",
                        "status": "completed",
                        "tool_commit": "e" * 40,
                        "boards": reservation["boards"],
                    }
                )
                + "\n"
            )
            release_gates.append_iteration_result(
                history,
                iteration_id="planned-only",
                status="completed",
                recorded_at="2026-08-06T12:01:00Z",
                evidence_sha256=hashlib.sha256(artifact.read_bytes()).hexdigest(),
                evidence_file=artifact.name,
                tool_commit="e" * 40,
            )
            self.assertEqual(
                1,
                release_gates.audit_release_history(history, evidence_dir)["completed"],
            )
            artifact.write_text('{"status":"rewritten"}\n')
            with self.assertRaisesRegex(HistoryError, r"digest does not match"):
                release_gates.audit_release_history(history, evidence_dir)

    def test_release_history_audit_binds_terminal_status_and_boards_to_evidence(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as raw:
            base = Path(raw)
            history = base / "history.jsonl"
            candidates = filesystem_candidates(base / "pool", 5)
            reservation = reserve_iteration(
                history,
                candidates,
                entropy="a" * 64,
                iteration_id="status-mismatch",
                planned_at="2026-08-06T12:00:00Z",
                manifest_sha256="d" * 64,
                tool_commit="e" * 40,
            )
            evidence_dir = base / "evidence"
            evidence_dir.mkdir()
            artifact = evidence_dir / "status-mismatch.json"
            artifact.write_text(
                json.dumps(
                    {
                        "gate": "external-five",
                        "iteration_id": "status-mismatch",
                        "status": "failed",
                        "boards": reservation["boards"],
                    }
                )
                + "\n"
            )
            release_gates.append_iteration_result(
                history,
                iteration_id="status-mismatch",
                status="completed",
                recorded_at="2026-08-06T12:01:00Z",
                evidence_sha256=hashlib.sha256(artifact.read_bytes()).hexdigest(),
                evidence_file=artifact.name,
                tool_commit="e" * 40,
            )
            with self.assertRaisesRegex(HistoryError, r"status does not match"):
                release_gates.audit_release_history(history, evidence_dir)

    def test_plan_command_requires_a_manifest_and_commits_its_own_random_seed(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as raw:
            base = Path(raw)
            pool = base / "pool"
            pool.mkdir()
            manifest = base / "candidates.toml"
            entries = []
            for number in range(6):
                source_id = f"source-{number}"
                revision = f"{number + 1:040x}"
                source = pool / source_id
                source.mkdir()
                (source / ".hauksbee-rev").write_text(revision + "\n")
                (source / f"board-{number}.kicad_pcb").write_text(
                    f"(kicad_pcb board-{number})"
                )
                entries.append(
                    f'''[[board]]
id = "{source_id}"
url = "https://example.invalid/{source_id}"
rev = "{revision}"
license = "MIT"
axes = ["kicad", "dev-board"]
expect = ["board-{number}.kicad_pcb"]
'''
                )
            manifest.write_text("\n".join(entries))
            history = base / "iterations.jsonl"
            stdout = io.StringIO()

            with (
                patch("qc.unseen_boards.secrets.token_hex", return_value="a" * 64),
                patch("qc.unseen_boards.current_tool_commit", return_value="e" * 40),
                redirect_stdout(stdout),
            ):
                exit_code = main(
                    [
                        "plan",
                        "--candidate-root",
                        str(pool),
                        "--manifest",
                        str(manifest),
                        "--history",
                        str(history),
                        "--iteration-id",
                        "release-01",
                        "--planned-at",
                        "2026-08-06T12:00:00Z",
                    ]
                )

            self.assertEqual(0, exit_code)
            planned = json.loads(stdout.getvalue())
            self.assertEqual(5, len(planned["boards"]))
            self.assertEqual("a" * 64, planned["entropy"])
            self.assertEqual(
                hashlib.sha256(b"").hexdigest(), planned["prior_history_sha256"]
            )
            self.assertEqual(
                hashlib.sha256(manifest.read_bytes()).hexdigest(),
                planned["manifest_sha256"],
            )
            self.assertEqual(planned, load_history(history).iterations[0])

            stderr = io.StringIO()
            with redirect_stderr(stderr):
                bad_exit = main(
                    [
                        "plan",
                        "--candidate-root",
                        str(pool),
                        "--history",
                        str(history),
                        "--iteration-id",
                        "missing-manifest",
                    ]
                )
            self.assertNotEqual(0, bad_exit)
            self.assertIn("--manifest", stderr.getvalue())

    def test_module_execution_reports_gate_errors_without_a_traceback(self) -> None:
        # `python -m qc.unseen_boards` executes this file as `__main__`, while
        # `qc.release_board_gates` imports it a second time as
        # `qc.unseen_boards`. The gate functions raise the canonical module's
        # HistoryError/SelectionError, which are distinct classes from the ones
        # defined in `__main__`; the CLI's error handler must catch both and
        # exit 2 rather than let a traceback escape. Calling `main()` directly
        # (as the other tests here do) cannot reproduce the split, so this test
        # re-runs the module the way production invokes it.
        import runpy
        import sys

        with tempfile.TemporaryDirectory() as raw:
            history = Path(raw) / "history.jsonl"
            history.write_text("")
            evidence = Path(raw) / "runs"
            evidence.mkdir()
            stderr = io.StringIO()
            argv = ["qc.unseen_boards", "audit-history", "--require-completed"]
            with (
                patch.object(release_gates, "CANONICAL_HISTORY", history),
                patch.object(release_gates, "CANONICAL_EVIDENCE_DIR", evidence),
                patch.object(sys, "argv", argv),
                redirect_stderr(stderr),
            ):
                with self.assertRaises(SystemExit) as caught:
                    runpy.run_module("qc.unseen_boards", run_name="__main__")

            self.assertEqual(2, caught.exception.code)
            self.assertIn(
                "no completed external-five iteration", stderr.getvalue()
            )
            self.assertNotIn("Traceback", stderr.getvalue())

    def test_show_command_replays_the_existing_planned_iteration(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            history = Path(raw) / "iterations.jsonl"
            entry = iteration_record("resume-me", [board_record(i) for i in range(5)])
            history.write_text(json.dumps(entry) + "\n")
            stdout = io.StringIO()

            with redirect_stdout(stdout):
                exit_code = main(
                    ["show", "--history", str(history), "--iteration-id", "resume-me"]
                )

            self.assertEqual(0, exit_code)
            self.assertEqual(entry, json.loads(stdout.getvalue()))


if __name__ == "__main__":
    unittest.main()
