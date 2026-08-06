from __future__ import annotations

import json
import hashlib
import inspect
import io
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
        logical = hashlib.sha256(f"source-{number}\0board-{number}".encode()).hexdigest()
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


class DiscoveryTests(unittest.TestCase):
    def test_discovers_primary_board_inputs_and_deduplicates_the_same_content(self) -> None:
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
            self.assertEqual("alpha/controller.kicad_pcb", by_format["kicad_pcb"].relative_path)
            self.assertEqual("alpha", by_format["kicad_pcb"].source_id)
            self.assertEqual("a" * 40, by_format["kicad_pcb"].revision)
            self.assertEqual("kicad_pcb", by_format["kicad_pcb"].input_format)
            self.assertEqual("altium_pcbdoc", by_format["altium_pcbdoc"].input_format)
            self.assertTrue(all(item.board_id.startswith("board:") for item in found))
            self.assertTrue(all(len(item.board_id) == len("board:") + 64 for item in found))
            self.assertFalse(any("backup" in item.relative_path for item in found))
            self.assertFalse(any("panel" in item.relative_path for item in found))
            self.assertFalse(any("Block Diagram" in item.relative_path for item in found))

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

    def test_manifest_discovery_refuses_a_revision_marker_that_does_not_match_the_pin(self) -> None:
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

    def test_crawl_does_not_follow_a_board_symlink_outside_the_candidate_root(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            base = Path(raw)
            corpus = base / "candidate-pool"
            corpus.mkdir()
            private_board = base / "private.kicad_pcb"
            private_board.write_text("(kicad_pcb private)")
            (corpus / "looks-public.kicad_pcb").symlink_to(private_board)

            self.assertEqual([], discover_candidates(corpus))

    def test_manifest_groups_loose_gerber_films_into_one_reproducible_drop(self) -> None:
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
                self.assertEqual(list(found[0].bundle_members), sorted(archive.namelist()))

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


class SelectionTests(unittest.TestCase):
    def test_seeded_selection_is_repeatable_diverse_and_never_reuses_seen_boards(self) -> None:
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
        self.assertTrue({item.board_id for item in first}.isdisjoint(item.board_id for item in second))

    def test_refuses_to_claim_an_iteration_when_fewer_than_five_unseen_boards_exist(self) -> None:
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

    def test_candidate_pool_digest_is_order_independent_and_content_sensitive(self) -> None:
        candidates = [candidate(i) for i in range(6)]
        same_reversed = candidate_pool_digest(reversed(candidates))

        changed = list(candidates)
        changed[0] = Candidate(
            **{**changed[0].__dict__, "sha256": "f" * 64}
        )

        self.assertEqual(candidate_pool_digest(candidates), same_reversed)
        self.assertNotEqual(candidate_pool_digest(candidates), candidate_pool_digest(changed))


class HistoryTests(unittest.TestCase):
    def test_reservation_is_append_only_and_the_next_iteration_cannot_reuse_its_boards(self) -> None:
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
            self.assertTrue(all(json.loads(line)["status"] == "planned" for line in lines))
            self.assertEqual(1, first["selector_version"])
            self.assertEqual(12, first["candidate_count"])
            self.assertEqual(candidate_pool_digest(candidates), first["candidate_pool_sha256"])
            self.assertEqual("d" * 64, first["manifest_sha256"])
            self.assertEqual("e" * 40, first["tool_commit"])

    def test_malformed_history_refuses_instead_of_forgetting_what_was_seen(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            history = Path(raw) / "iterations.jsonl"
            boards = [board_record(i) for i in range(5)]
            history.write_text(
                json.dumps(iteration_record("ok", boards)) + "\nnot json\n"
            )

            with self.assertRaisesRegex(HistoryError, r"iterations\.jsonl:2: invalid JSON"):
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

            with self.assertRaisesRegex(HistoryError, r"iteration id 'same-id' already exists"):
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

    def test_history_refuses_any_iteration_that_does_not_contain_five_unique_boards(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            history = Path(raw) / "iterations.jsonl"
            history.write_text(
                json.dumps(iteration_record("short", [board_record(i) for i in range(4)]))
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

    def test_release_reservation_has_no_caller_controlled_count_or_seed(self) -> None:
        parameters = inspect.signature(reserve_iteration).parameters
        self.assertNotIn("count", parameters)
        self.assertNotIn("seed", parameters)

    def test_history_refuses_a_board_record_with_missing_audit_metadata(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            history = Path(raw) / "iterations.jsonl"
            boards = [board_record(i) for i in range(5)]
            del boards[2]["sha256"]
            history.write_text(
                json.dumps(iteration_record("damaged", boards)) + "\n"
            )

            with self.assertRaisesRegex(
                HistoryError,
                r"iterations\.jsonl:1: boards\[2\] is missing sha256",
            ):
                load_history(history)

    def test_reservation_refuses_a_candidate_changed_after_discovery(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            candidates = filesystem_candidates(Path(raw) / "pool", 6)
            candidates[0].absolute_path.write_text("changed after discovery")

            with self.assertRaisesRegex(SelectionError, r"candidate changed after discovery"):
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
            entry = iteration_record("damaged-population", [board_record(i) for i in range(5)])
            del entry["manifest_sha256"]
            history.write_text(json.dumps(entry) + "\n")

            with self.assertRaisesRegex(
                HistoryError,
                r"iterations\.jsonl:1: iteration is missing manifest_sha256",
            ):
                load_history(history)


class CommandTests(unittest.TestCase):
    def test_plan_command_requires_a_manifest_and_commits_its_own_random_seed(self) -> None:
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
            self.assertEqual(hashlib.sha256(b"").hexdigest(), planned["prior_history_sha256"])
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
