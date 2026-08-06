from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path

from qc.unseen_boards import (
    Candidate,
    HistoryError,
    SelectionError,
    discover_candidates,
    load_history,
    reserve_iteration,
    select_unseen,
)


def candidate(
    number: int,
    *,
    source: str | None = None,
    input_format: str = "kicad_pcb",
) -> Candidate:
    digest = f"{number:064x}"
    return Candidate(
        board_id=f"sha256:{digest}",
        sha256=digest,
        source_id=source or f"source-{number}",
        revision=f"revision-{number}",
        relative_path=f"board-{number}.kicad_pcb",
        absolute_path=Path(f"/corpus/source-{number}/board-{number}.kicad_pcb"),
        input_format=input_format,
    )


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
            self.assertTrue(all(item.board_id == f"sha256:{item.sha256}" for item in found))
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

    def test_crawl_does_not_follow_a_board_symlink_outside_the_candidate_root(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            base = Path(raw)
            corpus = base / "candidate-pool"
            corpus.mkdir()
            private_board = base / "private.kicad_pcb"
            private_board.write_text("(kicad_pcb private)")
            (corpus / "looks-public.kicad_pcb").symlink_to(private_board)

            self.assertEqual([], discover_candidates(corpus))


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


class HistoryTests(unittest.TestCase):
    def test_reservation_is_append_only_and_the_next_iteration_cannot_reuse_its_boards(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            history = Path(raw) / "iterations.jsonl"
            candidates = [candidate(i) for i in range(12)]

            first = reserve_iteration(
                history,
                candidates,
                count=5,
                seed="one",
                iteration_id="2026-08-06-01",
                planned_at="2026-08-06T12:00:00Z",
            )
            second = reserve_iteration(
                history,
                candidates,
                count=5,
                seed="two",
                iteration_id="2026-08-06-02",
                planned_at="2026-08-06T13:00:00Z",
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

    def test_malformed_history_refuses_instead_of_forgetting_what_was_seen(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            history = Path(raw) / "iterations.jsonl"
            boards = [{"board_id": f"sha256:{i:064x}"} for i in range(5)]
            history.write_text(
                json.dumps({"iteration_id": "ok", "boards": boards}) + "\nnot json\n"
            )

            with self.assertRaisesRegex(HistoryError, r"iterations\.jsonl:2: invalid JSON"):
                load_history(history)

    def test_duplicate_iteration_id_refuses_before_reserving_more_boards(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            history = Path(raw) / "iterations.jsonl"
            candidates = [candidate(i) for i in range(12)]
            reserve_iteration(
                history,
                candidates,
                count=5,
                seed="one",
                iteration_id="same-id",
                planned_at="2026-08-06T12:00:00Z",
            )

            with self.assertRaisesRegex(HistoryError, r"iteration id 'same-id' already exists"):
                reserve_iteration(
                    history,
                    candidates,
                    count=5,
                    seed="two",
                    iteration_id="same-id",
                    planned_at="2026-08-06T13:00:00Z",
                )

            self.assertEqual(1, len(history.read_text().splitlines()))

    def test_history_refuses_any_iteration_that_does_not_contain_five_unique_boards(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            history = Path(raw) / "iterations.jsonl"
            history.write_text(
                json.dumps(
                    {
                        "iteration_id": "short",
                        "boards": [{"board_id": f"sha256:{i:064x}"} for i in range(4)],
                    }
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
            first = [{"board_id": f"sha256:{i:064x}"} for i in range(5)]
            second = [{"board_id": f"sha256:{i:064x}"} for i in range(4, 9)]
            history.write_text(
                json.dumps({"iteration_id": "one", "boards": first})
                + "\n"
                + json.dumps({"iteration_id": "two", "boards": second})
                + "\n"
            )

            with self.assertRaisesRegex(
                HistoryError,
                r"iterations\.jsonl:2: board .* was already used by an earlier iteration",
            ):
                load_history(history)

    def test_reservation_enforces_the_release_sample_size(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            with self.assertRaisesRegex(SelectionError, r"release iterations require exactly 5"):
                reserve_iteration(
                    Path(raw) / "iterations.jsonl",
                    [candidate(i) for i in range(6)],
                    count=4,
                    seed="too-short",
                    iteration_id="short",
                    planned_at="2026-08-06T12:00:00Z",
                )


if __name__ == "__main__":
    unittest.main()
