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
import qc.value_grading as value_grading


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
    refusal_numbers: frozenset[int] = frozenset(),
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
            # A minimal but ANCHORABLE board: two placements with reference
            # designators and three declared nets, matching what
            # `successful_browser_runner` reports for it. A one-line stub with no
            # footprints and no nets is a board the gate can verify nothing
            # about, which it deliberately caps below `delivered`.
            else (
                f'(kicad_pcb (version 20221018) (generator "{name}-{number}")\n'
                '  (net 0 "")\n  (net 1 "GND")\n  (net 2 "VCC")\n'
                '  (net 3 "SIG")\n'
                '  (footprint "Package_QFP:TQFP-32" (at 10 10)\n'
                '    (property "Reference" "U1"))\n'
                '  (footprint "Resistor_SMD:R_0402" (at 20 10)\n'
                '    (property "Reference" "R1"))\n)\n'
            ).encode()
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
                'axes = ["kicad", "dev-board"]'
                if number not in refusal_numbers
                else f'axes = ["kicad", "{release_gates.REFUSAL_AXIS}"]',
                'expect = ["board.kicad_pcb"]',
                "",
            ]
        )
    manifest = base / f"{name}.toml"
    manifest.write_text("\n".join(rows))
    return root, manifest


# What the engine returns for a pre-Eagle-6 binary board, in the shape the
# validator checks: the page renders this text, so a stub whose rendered message
# does not carry the server's diagnostic must fail the same way a real journey
# would.
REFUSAL_ERROR = (
    "Could not read this board file: this is an Eagle drawing in the pre-Eagle-6 "
    "BINARY format, which hauksbee does not read. Open it in Eagle 6 or later and "
    "re-save it."
)


def successful_browser_runner(captured: list[Path] | None = None):
    def run(
        paths: list[Path],
        output: Path,
        base_url: str,
        cohort: str,
        refusals: list[Path],
        firmware: list[dict | None] | None = None,
    ) -> int:
        if captured is not None:
            captured.extend(paths)
        output.mkdir(parents=True, exist_ok=True)
        expected_refusals = {path.resolve() for path in refusals}
        results = []
        for path in paths:
            refused = path.resolve() in expected_refusals
            results.append(
                {
                    "path": str(path.resolve()),
                    "file": path.name,
                    "elapsed_ms": 12,
                    "response_status": 200,
                    "response_capture_error": None,
                    "report": {
                        "ok": False,
                        "file_name": path.name,
                        "error": REFUSAL_ERROR,
                        "num_components": 0,
                        "num_nets": 0,
                        "headline": "Could not read the file.",
                        "sections": [],
                    }
                    if refused
                    else {
                        "ok": True,
                        "file_name": path.name,
                        "num_components": 2,
                        "num_nets": 3,
                        "headline": "Useful report",
                        # Three sections, because that is what every real report
                        # in the retained evidence carries and the contract holds
                        # a report to reaching more than one conclusion.
                        "sections": [
                            {
                                "title": "Connectivity & wiring",
                                "verdict": "Looks healthy.",
                                "findings": [],
                            },
                            {
                                "title": "Power & decoupling",
                                "verdict": "Rails are decoupled.",
                                "findings": [],
                            },
                            {
                                "title": "Thermal",
                                "verdict": "Within limits.",
                                "findings": [],
                            },
                        ],
                        "components": [{"reference": "U1"}, {"reference": "R1"}],
                        "nets": ["GND", "VCC", "SIG"],
                        # Bench-grade by the value contract: every critical
                        # part bound, checks run, and the clock advances below.
                        "bind": {"critical_parts_bound": "2/2"},
                    },
                    "exported": not refused,
                    "expected_refusal": refused,
                    "refused": refused,
                    "refusal_message": REFUSAL_ERROR if refused else None,
                    "live_started": not refused,
                    "sim_time_before_s": None if refused else 0.0,
                    "sim_time_after_s": None if refused else 0.002,
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

    def test_manifest_firmware_path_must_stay_inside_its_declared_source(
        self,
    ) -> None:
        # The `expect` twin of this check is tested above; the firmware path is a
        # second way into the same staging directory, and it is the one that would
        # hand the browser an arbitrary file off this machine.
        with tempfile.TemporaryDirectory() as raw:
            base = Path(raw)
            corpus = base / "pool"
            source = corpus / "alpha"
            source.mkdir(parents=True)
            (source / ".hauksbee-rev").write_text("a" * 40 + "\n")
            (source / "board.kicad_pcb").write_text("(kicad_pcb alpha)")
            (base / "secret.elf").write_text("not firmware from this source")
            manifest = base / "pool.toml"
            manifest.write_text(
                """[[board]]
id = "alpha"
url = "https://example.invalid/alpha"
rev = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
license = "MIT"
axes = ["kicad8"]
expect = ["board.kicad_pcb"]
firmware = "../../secret.elf"
"""
            )

            with self.assertRaisesRegex(SelectionError, r"firmware escapes declared"):
                discover_candidates(corpus, manifest_path=manifest)

    def test_an_unreadable_by_design_entry_cannot_carry_firmware(self) -> None:
        # The entry exists to prove hauksbee refuses the format. Pairing firmware
        # with it asks for a co-simulation on a board nothing will read, and left
        # unchecked it surfaced as a bare non-zero runner exit with no reason.
        with tempfile.TemporaryDirectory() as raw:
            base = Path(raw)
            corpus = base / "pool"
            source = corpus / "alpha"
            source.mkdir(parents=True)
            (source / ".hauksbee-rev").write_text("a" * 40 + "\n")
            (source / "board.brd").write_bytes(b"\x10\x80binary eagle")
            (source / "app.elf").write_text("firmware")
            manifest = base / "pool.toml"
            manifest.write_text(
                """[[board]]
id = "alpha"
url = "https://example.invalid/alpha"
rev = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
license = "MIT"
axes = ["eagle-binary", "unreadable-by-design"]
expect = ["board.brd"]
firmware = "app.elf"
"""
            )

            with self.assertRaisesRegex(
                SelectionError, r"unreadable-by-design entry cannot carry firmware"
            ):
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
                paths: list[Path],
                output: Path,
                base_url: str,
                cohort: str,
                refusals: list[Path],
                firmware: list[dict | None] | None = None,
            ) -> int:
                successful_browser_runner()(paths, output, base_url, cohort, refusals, firmware)
                artifact = json.loads((output / "results.json").read_text())
                artifact["results"][0]["failures"] = ["report was not useful"]
                (output / "results.json").write_text(json.dumps(artifact))
                return 1

            # The artifact is graded before the exit code is consulted, so the
            # failure names the board and what went wrong on it rather than only
            # the runner's status.
            with self.assertRaisesRegex(
                SelectionError, r"browser journey failed for external-0"
            ):
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
                paths: list[Path],
                output: Path,
                base_url: str,
                cohort: str,
                refusals: list[Path],
                firmware: list[dict | None] | None = None,
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
                paths: list[Path],
                output: Path,
                base_url: str,
                cohort: str,
                refusals: list[Path],
                firmware: list[dict | None] | None = None,
            ) -> int:
                successful_browser_runner()(paths, output, base_url, cohort, refusals, firmware)
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
                paths: list[Path],
                output: Path,
                base_url: str,
                cohort: str,
                refusals: list[Path],
                firmware: list[dict | None] | None = None,
            ) -> int:
                code = successful_browser_runner()(paths, output, base_url, cohort, refusals, firmware)
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
                paths: list[Path],
                output: Path,
                base_url: str,
                cohort: str,
                refusals: list[Path],
                firmware: list[dict | None] | None = None,
            ) -> int:
                successful_browser_runner()(paths, output, base_url, cohort, refusals, firmware)
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

    def test_corpus_gate_demands_an_honest_refusal_from_an_unreadable_by_design_input(
        self,
    ) -> None:
        # The corpus carries formats hauksbee deliberately does not read (a
        # pre-Eagle-6 binary Eagle drawing) so the refusal is held to its word
        # on real files. The gate used to demand a report and a JSON export from
        # every staged input without exception, so those boards failed five
        # journey checks each for behaving exactly as documented, and the
        # corpus-exhaustive gate could never go green.
        #
        # Both directions, because either one alone is a hole: a readable board
        # must still produce its export, and a declared-unreadable input must
        # still be refused rather than quietly analysed.
        with tempfile.TemporaryDirectory() as raw:
            base = Path(raw)
            corpus_root, corpus_manifest = manifest_pool(
                base, "corpus", 3, cohort="corpus", refusal_numbers=frozenset({1})
            )
            handed_refusals: list[list[Path]] = []

            def recording(
                paths: list[Path],
                output: Path,
                base_url: str,
                cohort: str,
                refusals: list[Path],
                firmware: list[dict | None] | None = None,
            ) -> int:
                handed_refusals.append(list(refusals))
                return successful_browser_runner()(
                    paths, output, base_url, cohort, refusals, firmware
                )

            evidence = release_gates.run_corpus_gate(
                corpus_root=corpus_root,
                corpus_manifest=corpus_manifest,
                evidence_dir=base / "evidence",
                scratch_root=base / "scratch",
                run_id="corpus-refusal",
                base_url="http://127.0.0.1:37651",
                tool_commit="e" * 40,
                runner=recording,
            )

            # The axis, not a filename, decides which inputs carry the refusal
            # contract, and exactly the declared one does.
            self.assertEqual(1, len(handed_refusals[0]))
            rows = evidence["browser"]["results"]
            self.assertEqual([False, True, False], [row["expected_refusal"] for row in rows])
            self.assertEqual([True, False, True], [row["exported"] for row in rows])

            # A refusal that turns into a report is a failure, not a bonus.
            def analysing_the_unreadable(
                paths: list[Path],
                output: Path,
                base_url: str,
                cohort: str,
                refusals: list[Path],
                firmware: list[dict | None] | None = None,
            ) -> int:
                successful_browser_runner()(paths, output, base_url, cohort, [])
                artifact = json.loads((output / "results.json").read_text())
                for row in artifact["results"]:
                    row["expected_refusal"] = Path(row["path"]).resolve() in {
                        path.resolve() for path in refusals
                    }
                (output / "results.json").write_text(json.dumps(artifact))
                return 0

            with self.assertRaisesRegex(SelectionError, r"did not refuse"):
                release_gates.run_corpus_gate(
                    corpus_root=corpus_root,
                    corpus_manifest=corpus_manifest,
                    evidence_dir=base / "evidence",
                    scratch_root=base / "scratch",
                    run_id="corpus-refusal-inverted",
                    base_url="http://127.0.0.1:37651",
                    tool_commit="e" * 40,
                    runner=analysing_the_unreadable,
                )

            # Nor can a journey pass by asserting `refused` over an empty row.
            # The refusal has to be backed by the server's payload and the
            # message the page rendered, or the gate is recording a board
            # nothing was ever dropped on.
            for stripped in ("report", "refusal_message"):

                def hollow(
                    paths: list[Path],
                    output: Path,
                    base_url: str,
                    cohort: str,
                    refusals: list[Path],
                    firmware: list[dict | None] | None = None,
                    field: str = stripped,
                ) -> int:
                    successful_browser_runner()(
                        paths, output, base_url, cohort, refusals, firmware
                    )
                    artifact = json.loads((output / "results.json").read_text())
                    for row in artifact["results"]:
                        if row["expected_refusal"]:
                            row[field] = None
                    (output / "results.json").write_text(json.dumps(artifact))
                    return 0

                with self.assertRaisesRegex(SelectionError, r"retained no|no reason"):
                    release_gates.run_corpus_gate(
                        corpus_root=corpus_root,
                        corpus_manifest=corpus_manifest,
                        evidence_dir=base / "evidence",
                        scratch_root=base / "scratch",
                        run_id=f"corpus-refusal-hollow-{stripped}",
                        base_url="http://127.0.0.1:37651",
                        tool_commit="e" * 40,
                        runner=hollow,
                    )

            # And a journey that never applied the contract at all cannot pass
            # by simply omitting the field.
            def contract_never_applied(
                paths: list[Path],
                output: Path,
                base_url: str,
                cohort: str,
                refusals: list[Path],
                firmware: list[dict | None] | None = None,
            ) -> int:
                return successful_browser_runner()(paths, output, base_url, cohort, [])

            with self.assertRaisesRegex(SelectionError, r"did not apply the"):
                release_gates.run_corpus_gate(
                    corpus_root=corpus_root,
                    corpus_manifest=corpus_manifest,
                    evidence_dir=base / "evidence",
                    scratch_root=base / "scratch",
                    run_id="corpus-refusal-ignored",
                    base_url="http://127.0.0.1:37651",
                    tool_commit="e" * 40,
                    runner=contract_never_applied,
                )

    def test_external_five_gate_will_not_spend_a_slot_on_an_unreadable_by_design_input(
        self,
    ) -> None:
        # The external gate asks one question: do five previously-unseen real
        # boards analyse in a browser? A file hauksbee refuses by design answers
        # nothing about that, so an external pool that tagged its entries
        # `unreadable-by-design` could otherwise satisfy a completed iteration
        # with five refusals and no analysed board at all.
        with tempfile.TemporaryDirectory() as raw:
            base = Path(raw)
            external_root, external_manifest = manifest_pool(
                base,
                "external",
                6,
                cohort="external",
                refusal_numbers=frozenset({0, 1}),
            )
            corpus_root, corpus_manifest = manifest_pool(
                base, "corpus", 1, cohort="corpus"
            )
            captured: list[Path] = []

            with self.assertRaisesRegex(SelectionError, r"only 4 remain"):
                release_gates.run_external_gate(
                    external_root=external_root,
                    external_manifest=external_manifest,
                    corpus_root=corpus_root,
                    corpus_manifest=corpus_manifest,
                    history_path=base / "history.jsonl",
                    evidence_dir=base / "evidence",
                    scratch_root=base / "scratch",
                    iteration_id="external-refusals",
                    planned_at="2026-08-06T12:00:00Z",
                    base_url="http://127.0.0.1:37651",
                    entropy="a" * 64,
                    tool_commit="e" * 40,
                    runner=successful_browser_runner(captured),
                )
            self.assertEqual([], captured, "no journey may run on a refusal input")

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
            repository / "qc/results/unseen-external-history.jsonl",
            external.call_args.kwargs["history_path"],
        )
        self.assertEqual(
            repository / "qc/results/evidence-runs", external.call_args.kwargs["evidence_dir"]
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

    def test_real_browser_harness_emits_every_field_the_validator_consumes(
        self,
    ) -> None:
        # The gate tests in this file exercise the validator against stub
        # runners, so a schema drift between the REAL harness
        # (frontend/tests/e2e/drag-drop-release.ts) and
        # `_validate_browser_results` passes every stub test while making the
        # production gate impossible to complete. That exact drift shipped
        # once: the harness wrote `file` but never `path`, and every corpus
        # and external-five run failed after a green browser journey. Pin the
        # emitted result fields to the ones the validator reads.
        harness = (
            Path(unseen_boards.__file__).resolve().parent.parent
            / "frontend/tests/e2e/drag-drop-release.ts"
        )
        source = harness.read_text(encoding="utf-8")
        interface = source.split("interface BoardResult", 1)[1].split("}", 1)[0]
        # `path`, `failures`, `report`, `exported`, and `response_status` are
        # hard requirements of `_validate_browser_results`; the rest feed the
        # retained-evidence document.
        for field in (
            "path",
            "file",
            "input_sha256",
            "report",
            "failures",
            "exported",
            "expected_refusal",
            "refused",
            "refusal_message",
            "response_status",
            "live_started",
            # The value contract reads these. A rename would leave every board
            # graded on a clock that never appears to advance and a firmware
            # that never appears to have been staged, and the gate would fail
            # after a green browser run: the same drift this test exists for.
            "sim_time_before_s",
            "sim_time_after_s",
            "firmware",
        ):
            self.assertRegex(
                interface,
                rf"\b{field}\b",
                f"BoardResult must declare {field!r}; the release validator "
                "refuses results without it",
            )
        # The firmware side of the same journey: what the grader reads off a
        # firmware row has to be what the harness writes into it.
        firmware_interface = source.split("interface FirmwareResult", 1)[1].split(
            "}", 1
        )[0]
        for field in (
            "staged",
            "loaded",
            "expect",
            "detail",
            "pin_activity",
            "serial_activity",
            "pin_activity_rendered",
            "analog_valid",
        ):
            self.assertRegex(
                firmware_interface,
                rf"\b{field}\b",
                f"FirmwareResult must declare {field!r}; the value contract "
                "grades firmware on it",
            )
        # The refusal expectations reach the harness over one env var, and a
        # rename on either side would silently drop the contract: the journey
        # would demand a report from every input again and the gate would fail
        # after a green run, which is the same drift this test exists for.
        for variable in ("HB_REFUSAL_FILES", "HB_FIRMWARE_FILES"):
            self.assertIn(variable, source)
            self.assertIn(
                variable,
                inspect.getsource(release_gates._playwright_runner),
            )
        # The payload SHAPE too, not just the variable name: renaming `path` to
        # `file` on one side would leave both these names in place and hand the
        # journey a plan it cannot read.
        runner_source = inspect.getsource(release_gates._playwright_runner)
        signals = (
            harness.parent / "value-signals.ts"
        ).read_text(encoding="utf-8")
        for key in ('"path"', '"expect"'):
            self.assertIn(key, runner_source)
            self.assertIn(key.strip('"'), signals)
        # And the two expectation spellings the manifest may use: the journey has
        # to accept exactly the set the manifest schema defines, or an entry could
        # declare an expectation the browser refuses to parse.
        for expectation in unseen_boards.FIRMWARE_EXPECTATIONS:
            self.assertIn(f"'{expectation}'", signals)

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


def gerber_archive(
    path: Path,
    *,
    copper_layers: int,
    flashes_per_layer: int,
    layer_names: list[str] | None = None,
) -> None:
    """A minimal but real Gerber package, in KiCad's every-layer-is-`.gbr` shape."""

    per_layer = "\n".join(
        f"X{1000 + index}Y{2000 + index}D03*" for index in range(flashes_per_layer)
    )
    names = layer_names or (
        ["board-F_Cu.gbr", "board-B_Cu.gbr"]
        + [f"board-In{index + 1}_Cu.gbr" for index in range(max(0, copper_layers - 2))]
    )[:copper_layers]
    with zipfile.ZipFile(path, "w") as archive:
        for name in names:
            archive.writestr(
                name, f"%FSLAX46Y46*%\n%MOMM*%\n{per_layer}\nM02*\n"
            )
        # An Excellon drill file and a stray README must not be counted as
        # layers, or the applicability guard would fire on a two-file package.
        archive.writestr("board.drl", "M48\nT1C0.300\n%\nG90\nM30\n")
        archive.writestr("README.txt", "fab notes\n")


#: Facts for a board the gate can anchor: a placement count and a declared-net
#: count that the default `journey_row` recovers in full. Used by tests whose
#: subject is something other than the input-anchored floors, so they do not sit
#: inside the cap that keeps an unverifiable board below `delivered`.
ANCHORED_KICAD = {
    "kind": "native",
    "input_format": "kicad_pcb",
    "input_placements": 40,
    "input_references": None,
    "input_declared_nets": 30,
}


def journey_row(
    *,
    ok: bool = True,
    components: int = 40,
    nets: int = 30,
    critical: str | None = "8/8",
    sections: int = 3,
    before: float | None = 0.0,
    after: float | None = 0.01,
    live: bool = True,
    unlocks: list[str] | None = None,
    coverage_note: str | None = None,
    cosim: dict | None = None,
    firmware: dict | None = None,
) -> dict:
    report: dict = {
        "ok": ok,
        "num_components": components,
        "num_nets": nets,
        "headline": "Useful report",
        # The real engine's shape: a titled section that reached a verdict, and
        # inventories whose lengths match the totals in the header.
        "sections": [
            {"title": f"Check {index}", "verdict": "Looks healthy.", "findings": []}
            for index in range(sections)
        ],
        "components": [{"reference": f"R{index}"} for index in range(components)],
        "nets": [f"NET{index}" for index in range(nets)],
        "assumptions": [
            {"kind": "open_part", "replacement": text} for text in (unlocks or [])
        ],
        "notes": (
            [{"kind": "coverage", "message": coverage_note}]
            if coverage_note is not None
            else []
        ),
    }
    if coverage_note is not None:
        # The real pairing: a descriptive `coverage` note next to a
        # `reduced_fidelity` assumption that carries the instruction. Only the
        # instruction counts as an unlock.
        report["assumptions"].append(
            {"kind": "reduced_fidelity", "replacement": coverage_note}
        )
    if critical is not None:
        report["bind"] = {"critical_parts_bound": critical}
        bound, total = (int(part) for part in critical.split("/"))
        # The engine's real shape: every unbound critical part named
        # individually in `open_parts`, and one `open_part` assumption per part
        # telling the reader what to upload for THAT part.
        unbound = [f"U{index}" for index in range(max(0, total - bound))]
        report["bind"]["open_parts"] = [
            {"reference": reference, "active_ic": True, "reason": "no model"}
            for reference in unbound
        ]
        report["assumptions"].extend(
            {
                "kind": "open_part",
                "replacement": f"Add a model for {reference} to your models directory.",
            }
            for reference in unbound
        )
    if cosim is not None:
        report["cosim"] = cosim
    row: dict = {
        "report": report,
        "live_started": live,
        "sim_time_before_s": before,
        "sim_time_after_s": after,
    }
    if firmware is not None:
        row["firmware"] = firmware
    return row


class ValueContractTests(unittest.TestCase):
    """The gate grades delivered value, not only honesty."""

    def grade(self, row: dict, *, input_format: str = "kicad_pcb", **kwargs):
        facts = kwargs.pop("facts", None)
        if facts is None:
            # Default to a file that declares exactly what the report found: the
            # honest board, matching the 1.00 net recovery measured across the
            # real corpus. A board the gate can anchor NOTHING about is
            # deliberately capped below `delivered`, so a test about some other
            # rule must not accidentally sit inside that cap.
            report = row.get("report") or {}
            listed = report.get("components")
            facts = {
                "kind": "native",
                "input_format": input_format,
                "input_placements": max(
                    1,
                    value_grading._distinct_parts(listed)
                    if isinstance(listed, list)
                    else int(report.get("num_components") or 0),
                ),
                "input_references": None,
                "input_declared_nets": max(1, int(report.get("num_nets") or 0)),
            }
        return value_grading.grade_board(
            row,
            input_format=input_format,
            expects_refusal=kwargs.pop("expects_refusal", False),
            facts=facts,
            firmware_expect=kwargs.pop("firmware_expect", None),
        )

    def test_a_bound_and_simulating_board_is_delivered(self) -> None:
        grade = self.grade(journey_row())

        self.assertEqual(value_grading.DELIVERED, grade.grade)
        self.assertEqual([], grade.reasons)

    def test_unbound_criticals_each_with_an_unlock_are_degraded_not_failed(
        self,
    ) -> None:
        grade = self.grade(journey_row(critical="1/14"))

        self.assertEqual(value_grading.DEGRADED, grade.grade)
        self.assertEqual(13, grade.signals["open_parts"])
        self.assertEqual([], grade.signals["open_parts_without_an_unlock"])
        self.assertIn("Add a model for U0 to your models directory.", grade.unlocks)

    def test_one_unlock_does_not_cover_thirteen_unbound_parts(self) -> None:
        # An upload that names U1 does nothing for the other twelve, so a single
        # model-shaped sentence cannot excuse a board-wide binding collapse.
        row = journey_row(critical="1/14")
        row["report"]["assumptions"] = [
            {
                "kind": "open_part",
                "replacement": "Add a model for U1 to your models directory.",
            }
        ]
        grade = self.grade(row)

        self.assertEqual(value_grading.FAILED, grade.grade)
        self.assertTrue(
            any("named no model upload for" in reason for reason in grade.reasons)
        )

    def test_unbound_criticals_with_no_named_unlock_fail(self) -> None:
        row = journey_row(critical="1/14")
        row["report"]["assumptions"] = []
        grade = self.grade(row)

        self.assertEqual(value_grading.FAILED, grade.grade)
        self.assertTrue(
            any("named no model upload for" in reason for reason in grade.reasons)
        )

    def test_one_unbound_critical_is_degraded_not_delivered(self) -> None:
        # 4 of 5 bound is not bench-grade, it is bench-grade on most of the
        # board: the fifth active IC still makes its nets untrustworthy.
        grade = self.grade(journey_row(critical="4/5"))

        self.assertEqual(value_grading.DEGRADED, grade.grade)

    def test_a_layout_that_extracts_no_parts_fails(self) -> None:
        grade = self.grade(journey_row(components=0))

        self.assertEqual(value_grading.FAILED, grade.grade)
        self.assertTrue(
            any("none survived extraction" in reason for reason in grade.reasons)
        )

    def test_a_report_without_a_binding_summary_fails_on_a_partful_input(self) -> None:
        grade = self.grade(journey_row(critical=None))

        self.assertEqual(value_grading.FAILED, grade.grade)
        self.assertTrue(
            any("no model-binding summary" in reason for reason in grade.reasons)
        )

    def test_a_simulation_clock_that_never_advances_fails(self) -> None:
        grade = self.grade(journey_row(before=0.0, after=0.0))

        self.assertEqual(value_grading.FAILED, grade.grade)
        self.assertTrue(
            any("never advanced" in reason for reason in grade.reasons)
        )

    #: A real KiCad board this repository ships, with a golden web report beside
    #: it naming exactly three components: U1, Q1, R1. It is also MINIFIED onto a
    #: single line, which is why the placement pattern must not be anchored to a
    #: line start.
    REAL_BOARD = Path("frontend/public/samples/boot_gate.kicad_pcb")
    REAL_GOLDEN = Path("testdata/golden/boot_gate_web_report.json")

    def test_extraction_facts_match_a_real_board_and_its_golden_report(self) -> None:
        root = Path(unseen_boards.__file__).resolve().parent.parent
        board = root / self.REAL_BOARD
        golden = json.loads((root / self.REAL_GOLDEN).read_text(encoding="utf-8"))
        facts = value_grading.input_facts("kicad_pcb", board)

        # Minified onto one line: a line-anchored pattern reports zero here.
        self.assertEqual(1, len(board.read_bytes().splitlines()))
        self.assertEqual(golden["num_components"], facts["input_placements"])
        self.assertEqual(
            sorted(c["reference"] for c in golden["components"]),
            facts["input_references"],
        )

    def test_a_repeated_designator_does_not_disable_the_identity_check(self) -> None:
        # Real boards repeat a designator: `crkbd` has two `G***` graphics,
        # `watchy` two `TP` test points, `mnt_reform` fifty repeats. Comparing the
        # designator SET against the placement COUNT read those as an incomplete
        # extraction and switched identity checking off on 17 of 61 real boards,
        # `watchy` among them, after which a wholly fabricated component list
        # graded `delivered`. The comparison is on the record count instead.
        with tempfile.TemporaryDirectory() as raw:
            board = Path(raw) / "repeats.kicad_pcb"
            board.write_text(
                "(kicad_pcb\n"
                '  (footprint "R" (property "Reference" "R1"))\n'
                '  (footprint "R" (property "Reference" "R2"))\n'
                '  (footprint "G" (property "Reference" "G***"))\n'
                '  (footprint "G" (property "Reference" "G***"))\n'
                "  (net 1 \"GND\") (net 2 \"VCC\")\n)\n"
            )
            facts = value_grading.input_facts("kicad_pcb", board)

            self.assertEqual(4, facts["input_placements"])
            # Three distinct names for four placements, and still complete.
            self.assertEqual(["G***", "R1", "R2"], facts["input_references"])

            # The file repeats a designator; the report lists each once, which is
            # what all 67 real reports do.
            row = journey_row(components=3, nets=2)
            row["report"]["components"] = [
                {"reference": r} for r in ("R1", "R2", "G***")
            ]
            row["report"]["nets"] = ["GND", "VCC"]
            self.assertEqual(
                value_grading.DELIVERED, self.grade(row, facts=facts).grade
            )

            # And the check is live, so an invented part is caught.
            forged = journey_row(components=3, nets=2)
            forged["report"]["components"] = [
                {"reference": r} for r in ("R1", "R2", "ZZ9")
            ]
            forged["report"]["nets"] = ["GND", "VCC"]
            grade = self.grade(forged, facts=facts)
            self.assertEqual(value_grading.FAILED, grade.grade)
            self.assertEqual(["ZZ9"], grade.signals["components_not_in_the_input"])

    def test_an_engine_disambiguated_reference_is_not_an_invention(self) -> None:
        # KiCad leaves an unset reference as `REF**`, and the engine disambiguates
        # a second one as `REF**@conflict-2`; Altium hierarchical parts arrive as
        # `Q9@Top/AUX/M`. `@` is not legal in a designator, so everything after it
        # is the engine's own annotation. Comparing verbatim accused two real
        # corpus boards of inventing a part they had merely renamed.
        with tempfile.TemporaryDirectory() as raw:
            board = Path(raw) / "unset.kicad_pcb"
            board.write_text(
                "(kicad_pcb\n"
                '  (footprint "X" (property "Reference" "REF**"))\n'
                '  (footprint "X" (property "Reference" "REF**"))\n'
                '  (footprint "R" (property "Reference" "R1"))\n'
                "  (net 1 \"GND\") (net 2 \"VCC\")\n)\n"
            )
            facts = value_grading.input_facts("kicad_pcb", board)
            self.assertEqual(3, facts["input_placements"])
            self.assertEqual(["R1", "REF**"], facts["input_references"])

            row = journey_row(components=3, nets=2)
            row["report"]["components"] = [
                {"reference": r} for r in ("REF**", "REF**@conflict-2", "R1")
            ]
            row["report"]["nets"] = ["GND", "VCC"]
            grade = self.grade(row, facts=facts)

            self.assertEqual(value_grading.DELIVERED, grade.grade, grade.reasons)
            self.assertEqual([], grade.signals["components_not_in_the_input"])

            # A genuinely invented part is still caught, suffix or not.
            forged = journey_row(components=3, nets=2)
            forged["report"]["components"] = [
                {"reference": r} for r in ("R1", "ZZ9@conflict-2", "REF**")
            ]
            forged["report"]["nets"] = ["GND", "VCC"]
            self.assertEqual(
                value_grading.FAILED, self.grade(forged, facts=facts).grade
            )

    def test_fabricated_components_are_caught_by_identity(self) -> None:
        # Inflating the total and fabricating list entries to match would still
        # have to name parts the input file does not contain.
        root = Path(unseen_boards.__file__).resolve().parent.parent
        facts = value_grading.input_facts("kicad_pcb", root / self.REAL_BOARD)
        row = journey_row(components=3, nets=4)
        row["report"]["components"] = [
            {"reference": "U1"}, {"reference": "Q1"}, {"reference": "R99"}
        ]
        grade = self.grade(row, facts=facts)

        self.assertEqual(value_grading.FAILED, grade.grade)
        self.assertEqual(["R99"], grade.signals["components_not_in_the_input"])
        self.assertTrue(
            any("does not contain: R99" in r for r in grade.reasons)
        )

    def test_the_real_board_grades_delivered_on_its_golden_report(self) -> None:
        root = Path(unseen_boards.__file__).resolve().parent.parent
        facts = value_grading.input_facts("kicad_pcb", root / self.REAL_BOARD)
        golden = json.loads((root / self.REAL_GOLDEN).read_text(encoding="utf-8"))
        row = {
            "report": golden,
            "live_started": True,
            "sim_time_before_s": 0.0,
            "sim_time_after_s": 0.01,
        }
        grade = value_grading.grade_board(
            row, input_format="kicad_pcb", expects_refusal=False, facts=facts
        )

        # The engine's own golden output on a real board must not be graded
        # `failed` by a rule that misreads its shape.
        self.assertEqual(value_grading.DELIVERED, grade.grade, grade.reasons)
        self.assertEqual(1.0, grade.signals["placement_recovery_fraction"])

    def test_the_gate_counts_placements_in_the_input_file_itself(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            kicad = Path(raw) / "board.kicad_pcb"
            kicad.write_text(
                "(kicad_pcb (version 20221018)\n"
                + "".join(f'  (footprint "R_0402" (at {i} 0))\n' for i in range(30))
                + "  (module OLD_STYLE (at 0 0))\n)\n"
            )
            eagle = Path(raw) / "board.brd"
            eagle.write_text(
                "<eagle><drawing><board><elements>"
                + "".join(f'<element name="R{i}" package="0402"/>' for i in range(12))
                + "</elements></board></drawing></eagle>"
            )

            self.assertEqual(
                31, value_grading.native_placement_count("kicad_pcb", kicad)
            )
            self.assertEqual(
                12, value_grading.native_placement_count("eagle_brd", eagle)
            )
            self.assertIsNone(
                value_grading.native_placement_count("altium_pcbdoc", eagle)
            )

    def test_losing_most_of_the_placements_the_input_names_fails(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            kicad = Path(raw) / "board.kicad_pcb"
            kicad.write_text(
                "(kicad_pcb\n"
                + "".join(f'  (footprint "R_0402" (at {i} 0))\n' for i in range(100))
                + ")\n"
            )
            facts = value_grading.input_facts("kicad_pcb", kicad)
            self.assertEqual(100, facts["input_placements"])

            # A tool cannot disclose this to itself: the report looks complete.
            grade = self.grade(journey_row(components=12), facts=facts)
            self.assertEqual(value_grading.FAILED, grade.grade)
            self.assertTrue(
                any("of the 100 placements" in reason for reason in grade.reasons)
            )

            # Dropping the logos, fiducials and mounting holes is not a failure.
            kept = self.grade(journey_row(components=91), facts=facts)
            self.assertEqual(value_grading.DELIVERED, kept.grade)

    def test_recovery_counts_the_listed_parts_not_the_claimed_total(self) -> None:
        # Otherwise inflating `num_components` clears the floor on its own: a
        # 100-placement input could claim 50 and list 12.
        with tempfile.TemporaryDirectory() as raw:
            kicad = Path(raw) / "board.kicad_pcb"
            kicad.write_text(
                "(kicad_pcb\n"
                + "".join(f'  (footprint "R" (at {i} 0))\n' for i in range(100))
                + ")\n"
            )
            facts = value_grading.input_facts("kicad_pcb", kicad)
            row = journey_row(components=50)
            row["report"]["components"] = [{"reference": f"R{i}"} for i in range(12)]
            grade = self.grade(row, facts=facts)

            self.assertEqual(value_grading.FAILED, grade.grade)
            self.assertEqual(12, grade.signals["recovered_components"])
            self.assertTrue(
                any("recovered 12 of the 100" in reason for reason in grade.reasons)
            )

    def test_unverified_identity_is_enumerated(self) -> None:
        # How often the identity check goes inert is itself worth disclosing: it
        # is inert on 20 of the 64 partful corpus boards.
        grade = self.grade(
            journey_row(components=4),
            facts={"kind": "native", "input_format": "kicad_pcb",
                   "input_placements": 4, "input_declared_nets": 30},
        )

        self.assertIs(False, grade.signals["component_identity_verified"])
        summary = value_grading.summarize([("no-ids", grade)])
        self.assertEqual(
            [{"board": "no-ids", "input_format": "kicad_pcb"}],
            summary["unverified_identity"],
        )
        self.assertTrue(
            any(
                "component identity UNVERIFIED" in line
                for line in release_gates.describe_degraded(
                    {"value_summary": summary}
                )
            )
        )

    def test_the_summary_surfaces_unverified_extraction_and_lowered_firmware(
        self,
    ) -> None:
        # A limitation only a per-board signal records is one nobody reads, so
        # both disclosures ride at summary level next to the degraded list.
        altium = self.grade(
            journey_row(),
            input_format="altium_pcbdoc",
            facts={"kind": "native", "input_format": "altium_pcbdoc"},
        )
        lowered = value_grading.grade_board(
            journey_row(
                firmware={
                    "staged": True,
                    "loaded": True,
                    "detail": "ELF, e_machine 0x1234",
                },
                cosim={"ran": False},
            ),
            input_format="kicad_pcb",
            expects_refusal=False,
            facts=ANCHORED_KICAD,
            firmware_expect="load-only",
        )
        summary = value_grading.summarize(
            [("altium-board", altium), ("firmware-board", lowered)]
        )

        self.assertEqual(
            [{"board": "altium-board", "input_format": "altium_pcbdoc"}],
            summary["unverified_extraction"],
        )
        self.assertEqual(
            [{"board": "firmware-board", "expect": "load-only"}],
            summary["firmware_expectation_lowered"],
        )
        lines = release_gates.describe_degraded({"value_summary": summary})
        self.assertTrue(any("UNVERIFIED against the input" in line for line in lines))
        self.assertTrue(any("expectation LOWERED" in line for line in lines))

    def test_an_unclassified_format_does_not_escape_the_unanchored_cap(self) -> None:
        # The cap keyed on PARTFUL_FORMATS, so a format nobody had classified
        # reached `delivered` with nothing anchored and nothing disclosed.
        row = journey_row(components=1, nets=2, critical="0/0")
        row["report"]["components"] = [{"reference": "U1"}]
        row["report"]["nets"] = ["A", "B"]
        grade = self.grade(
            row,
            input_format="some_future_format",
            facts={"kind": "native", "input_format": "some_future_format"},
        )

        self.assertEqual(value_grading.DEGRADED, grade.grade)
        self.assertIn(value_grading.UNANCHORED_INPUT_UNLOCK, grade.unlocks)

    def test_a_board_the_gate_cannot_anchor_is_capped_below_delivered(self) -> None:
        # Four partful formats have neither an exact placement token nor a
        # declared-net record, so every number behind them comes from the tool.
        # Those boards pass and are enumerated, but must not wear the top grade.
        unanchored = {
            "kind": "native", "input_format": "altium_pcbdoc",
            "input_placements": None, "input_references": None,
            "input_declared_nets": None,
        }
        row = journey_row(components=1, nets=2, critical="0/0")
        row["report"]["components"] = [{"reference": "U1"}]
        row["report"]["nets"] = ["A", "B"]
        grade = self.grade(row, input_format="altium_pcbdoc", facts=unanchored)

        self.assertEqual(value_grading.DEGRADED, grade.grade)
        self.assertIn(value_grading.UNANCHORED_INPUT_UNLOCK, grade.unlocks)

        # Anchor either dimension and the cap lifts.
        for anchor in ({"input_placements": 1}, {"input_declared_nets": 2}):
            lifted = self.grade(
                row, input_format="altium_pcbdoc", facts={**unanchored, **anchor}
            )
            self.assertEqual(value_grading.DELIVERED, lifted.grade, anchor)

    def test_only_model_kind_assumptions_excuse_unbound_parts(self) -> None:
        # The kind restriction, exercised on its own rather than incidentally: the
        # same sentence naming the same part, under four different kinds.
        for kind, expected in (
            ("open_part", value_grading.DEGRADED),
            ("inferred_pin_role", value_grading.DEGRADED),
            ("reduced_fidelity", value_grading.FAILED),
            ("something_else", value_grading.FAILED),
        ):
            row = journey_row(critical="0/1")
            row["report"]["bind"]["open_parts"] = [{"reference": "U0"}]
            row["report"]["assumptions"] = [
                {"kind": kind, "replacement": "Add a model for U0."}
            ]
            self.assertEqual(expected, self.grade(row).grade, kind)

    def test_a_long_unlock_list_is_truncated_with_a_pointer(self) -> None:
        row = journey_row(critical="0/40")
        grade = self.grade(row)

        self.assertEqual(value_grading.DEGRADED, grade.grade)
        self.assertEqual(value_grading.MAX_SUMMARIZED_UNLOCKS + 1, len(grade.unlocks))
        self.assertIn("retained report", grade.unlocks[-1])
        self.assertIn(
            str(40 - value_grading.MAX_SUMMARIZED_UNLOCKS), grade.unlocks[-1]
        )

    def test_truncation_keeps_one_of_every_kind_of_upload(self) -> None:
        # A plain prefix showed eight near-identical "add a model for Rnnn" lines
        # and dropped the one different instruction entirely, which is what
        # happened to a real `reduced_fidelity` upload on 49 of 61 corpus boards.
        row = journey_row(critical="0/40")
        rare = "Put the manufacturer part number on the component."
        row["report"]["assumptions"].append(
            {"kind": "reduced_fidelity", "replacement": rare}
        )
        grade = self.grade(row)

        self.assertEqual(value_grading.DEGRADED, grade.grade)
        self.assertIn(rare, grade.unlocks)
        # And the per-part sentences are still represented, not crowded out.
        self.assertTrue(any("Add a model for" in u for u in grade.unlocks))
        self.assertIn("retained report", grade.unlocks[-1])

    def test_an_uncorroborated_binding_claim_is_enumerated(self) -> None:
        # Binding is graded on the tool's own words; the open-parts list is the
        # only corroboration, because the unlocks have to agree with it part by
        # part. Both a "0/0" and a "40/40" with no list have nothing behind them,
        # and the second is the stronger claim, so both must be disclosed.
        for ratio in ("0/0", "40/40"):
            row = journey_row(critical=ratio)
            row["report"]["bind"]["open_parts"] = []
            grade = self.grade(row)

            self.assertEqual(value_grading.DELIVERED, grade.grade, ratio)
            self.assertIs(False, grade.signals["binding_verified"], ratio)
            self.assertEqual(
                [{"board": "b", "critical_parts_bound": ratio}],
                value_grading.summarize([("b", grade)])["unverified_binding"],
                ratio,
            )

        # A report that does list what it left open has something to check.
        listed = journey_row(critical="1/2")
        self.assertNotIn("binding_verified", self.grade(listed).signals)

    def test_an_unverified_net_ratio_is_enumerated_in_the_summary(self) -> None:
        # One real corpus board (rp2040_minimal_kicad) declares no nets of its
        # own, so its connectivity coverage goes unchecked. That has to be listed,
        # not buried in a per-board signal.
        grade = self.grade(
            journey_row(),
            facts={"kind": "native", "input_format": "kicad_pcb",
                   "input_placements": 40, "input_declared_nets": 0},
        )

        self.assertIs(False, grade.signals["net_recovery_verified"])
        summary = value_grading.summarize([("netless", grade)])
        self.assertEqual(
            [{"board": "netless", "input_format": "kicad_pcb"}],
            summary["unverified_connectivity"],
        )
        self.assertTrue(
            any(
                "connectivity coverage UNVERIFIED" in line
                for line in release_gates.describe_degraded(
                    {"value_summary": summary}
                )
            )
        )

    def test_an_unclassified_format_discloses_its_net_names_too(self) -> None:
        # The disclosure has to default ON for a format nobody has classified yet,
        # or the one board shape the gate understands least would be the one that
        # omits itself from the list. A netlist input takes neither the native nor
        # the copper branch.
        grade = self.grade(
            journey_row(components=20, nets=12, critical="0/0"),
            input_format="ipc_356",
        )

        self.assertIs(False, grade.signals["net_identity_verified"])
        self.assertEqual(
            [{"board": "netlist", "input_format": "ipc_356"}],
            value_grading.summarize([("netlist", grade)])["unverified_net_identity"],
        )

    def test_unchecked_net_names_are_disclosed_not_silent(self) -> None:
        # The largest unverified dimension: on every KiCad board the net COUNT is
        # checked and the net NAMES are not. A board whose connectivity rests on
        # a count alone has to say so, or the one dimension nobody can see is the
        # one covering the most boards.
        grade = self.grade(journey_row(), facts=ANCHORED_KICAD)

        self.assertIs(False, grade.signals["net_identity_verified"])
        summary = value_grading.summarize([("counted-only", grade)])
        self.assertEqual(
            [{"board": "counted-only", "input_format": "kicad_pcb"}],
            summary["unverified_net_identity"],
        )
        self.assertTrue(
            any(
                "net names UNCHECKED" in line
                for line in release_gates.describe_degraded(
                    {"value_summary": summary}
                )
            )
        )

    def test_an_unverified_placement_ratio_is_recorded_not_silent(self) -> None:
        # A reader of the evidence has to be able to see which boards had their
        # extraction ratio checked against the input and which did not.
        unavailable = self.grade(
            journey_row(),
            input_format="altium_pcbdoc",
            facts={"kind": "native", "input_format": "altium_pcbdoc"},
        )
        self.assertIs(False, unavailable.signals["placement_recovery_verified"])

        counted = self.grade(
            journey_row(components=40),
            facts={"kind": "native", "input_format": "kicad_pcb",
                   "input_placements": 44},
        )
        self.assertIs(True, counted.signals["placement_recovery_verified"])
        self.assertEqual(0.9091, counted.signals["placement_recovery_fraction"])

    def test_a_repeated_inventory_entry_does_not_back_a_total(self) -> None:
        # Nine copies of "GND" is nine entries and one net; fifty copies of R1 is
        # one recovered part. Padding a list with repeats is the cheapest way to
        # make a total look backed by something.
        row = journey_row(nets=9)
        row["report"]["nets"] = ["GND"] * 9
        grade = self.grade(row)
        self.assertEqual(value_grading.FAILED, grade.grade)
        self.assertTrue(any("net inventory repeats" in r for r in grade.reasons))

        row = journey_row(components=50)
        row["report"]["components"] = [{"reference": "R1"}] * 50
        grade = self.grade(
            row,
            facts={"kind": "native", "input_format": "kicad_pcb",
                   "input_placements": 100},
        )
        self.assertEqual(value_grading.FAILED, grade.grade)
        self.assertTrue(
            any("component inventory repeats" in r for r in grade.reasons)
        )

    def test_unnamed_placements_neither_repeat_nor_pad(self) -> None:
        # Two things at once. A real board carries placements with no reference
        # designator (a logo, a fiducial, a mounting hole) and KiCad permits it:
        # one olimex_esp32 revision in the retained corpus has two, so demanding
        # uniqueness across them would fail a good board. But they also cannot
        # COUNT toward coverage, or a list padded with anonymous entries would
        # clear the floor without naming one real part.
        # The real shape, from olimex_esp32: 151 placements, 149 named and 2 not.
        row = journey_row(components=151)
        row["report"]["components"] = (
            [{"reference": f"R{i}"} for i in range(149)]
            + [{"reference": ""}, {"reference": ""}]
        )
        grade = self.grade(
            row,
            facts={"kind": "native", "input_format": "kicad_pcb",
                   "input_placements": 151},
        )

        self.assertEqual(value_grading.DELIVERED, grade.grade, grade.reasons)
        self.assertEqual(149, grade.signals["recovered_components"])

    def test_anonymous_entries_cannot_pad_the_coverage_floor(self) -> None:
        row = journey_row(components=400)
        row["report"]["components"] = [{"position": [0, 0]} for _ in range(200)]
        grade = self.grade(
            row,
            facts={"kind": "native", "input_format": "kicad_pcb",
                   "input_placements": 373},
        )

        self.assertEqual(value_grading.FAILED, grade.grade)
        self.assertEqual(0, grade.signals["recovered_components"])

    def test_a_report_with_no_inventory_cannot_buy_a_grade(self) -> None:
        # Omitting an inventory would otherwise make its total unverifiable and
        # therefore worth inflating: a claimed 50 components against a
        # 100-placement input, or a claimed nine nets against 1924 flashes.
        for field in ("components", "nets"):
            row = journey_row(components=50, nets=40)
            del row["report"][field]
            grade = self.grade(
                row,
                facts={"kind": "native", "input_format": "kicad_pcb",
                       "input_placements": 100},
            )
            self.assertEqual(value_grading.FAILED, grade.grade, field)
            self.assertTrue(
                any(f"no {field} inventory" in r for r in grade.reasons), field
            )

    def test_a_missing_component_inventory_recovers_nothing(self) -> None:
        # The recovery numerator must not fall back to the claimed total.
        row = journey_row(components=50)
        del row["report"]["components"]
        grade = self.grade(
            row,
            facts={"kind": "native", "input_format": "kicad_pcb",
                   "input_placements": 100},
        )

        self.assertEqual(0, grade.signals["recovered_components"])

    def test_a_positioned_list_longer_than_the_total_fails(self) -> None:
        row = journey_row(components=40)
        row["report"]["components"] = [{"reference": f"R{i}"} for i in range(41)]
        grade = self.grade(row)

        self.assertEqual(value_grading.FAILED, grade.grade)
        self.assertTrue(
            any("claims 40 components but lists 41" in r for r in grade.reasons)
        )

    def test_unpositioned_components_are_not_a_defect(self) -> None:
        # `components` carries only what the reader could place, so a shorter
        # list is the engine's normal shape and must not fail a run.
        row = journey_row(components=40)
        row["report"]["components"] = [{"reference": f"R{i}"} for i in range(12)]

        self.assertEqual(value_grading.DELIVERED, self.grade(row).grade)

    def test_shrinking_the_critical_denominator_is_not_an_escape_route(self) -> None:
        row = journey_row(critical="0/0")
        row["report"]["bind"]["open_parts"] = [
            {"reference": "U1", "active_ic": True, "reason": "no model"}
        ]
        grade = self.grade(row)

        self.assertEqual(value_grading.FAILED, grade.grade)
        self.assertTrue(
            any("claims no critical parts" in reason for reason in grade.reasons)
        )

    def test_a_board_with_genuinely_no_critical_parts_is_delivered(self) -> None:
        grade = self.grade(journey_row(critical="0/0"))

        self.assertEqual(value_grading.DELIVERED, grade.grade)

    def test_a_count_of_unbound_parts_with_no_list_still_needs_an_unlock(self) -> None:
        # The engine leaves a part out of `open_parts` when it is off the
        # connected path, so a name cannot be demanded for it. Something that
        # would bind it still can be.
        row = journey_row(critical="1/14")
        row["report"]["bind"]["open_parts"] = []
        row["report"]["assumptions"] = []
        grade = self.grade(row)

        self.assertEqual(value_grading.FAILED, grade.grade)
        self.assertTrue(any("named nowhere" in reason for reason in grade.reasons))

        row["report"]["assumptions"] = [
            {"kind": "open_part", "replacement": "Add models for the unbound parts."}
        ]
        self.assertEqual(value_grading.DEGRADED, self.grade(row).grade)

    def test_an_open_power_transistor_is_not_bench_grade(self) -> None:
        # `critical_parts_bound` counts active ICs only, so a MOSFET left OPEN
        # never appears in it, yet its own consequence line says the nets through
        # it are isolated in simulation. Binding is graded on the open-parts list
        # for exactly this case.
        row = journey_row(critical="2/2")
        row["report"]["bind"]["open_parts"] = [
            {
                "reference": "Q1",
                "active_ic": False,
                "reason": "no model",
                "consequence": "Q1 defaults to OPEN; nets through it are isolated",
            }
        ]
        row["report"]["assumptions"] = [
            {"kind": "open_part", "replacement": "Add a model for Q1."}
        ]
        grade = self.grade(row)

        self.assertEqual(value_grading.DEGRADED, grade.grade)
        self.assertEqual(1, grade.signals["open_parts"])

    def test_an_unrelated_unlock_does_not_excuse_unbound_models(self) -> None:
        # "Upload the original layout to run DRC" would not bind a single
        # model, so it cannot turn a binding collapse into honest degradation.
        row = journey_row(critical="0/10")
        row["report"]["assumptions"] = [
            {
                "kind": "reduced_fidelity",
                "replacement": "Upload the original layout to run DRC.",
            }
        ]
        grade = self.grade(row)

        self.assertEqual(value_grading.FAILED, grade.grade)
        self.assertTrue(
            any("named no model upload for" in reason for reason in grade.reasons)
        )

    def test_a_placeholder_check_section_is_not_a_check(self) -> None:
        row = journey_row()
        row["report"]["sections"] = [{}, {"title": "Copper spacing (DRC)"}]
        grade = self.grade(row)

        self.assertEqual(value_grading.FAILED, grade.grade)
        self.assertTrue(any("has no title" in reason for reason in grade.reasons))
        self.assertTrue(any("reached no verdict" in reason for reason in grade.reasons))

    def test_a_name_derived_declared_count_cannot_fail_a_board(self) -> None:
        # A count taken from quoted names under-states, because KiCad leaves
        # single-pad nets unnamed. Exceeding it is therefore not evidence of
        # invention, so the failure is withheld and the board is disclosed.
        facts = {
            "kind": "native", "input_format": "kicad_pcb",
            "input_placements": 10, "input_references": None,
            "input_declared_nets": 10, "declared_nets_exact": False,
        }
        row = journey_row(components=10, nets=13)
        row["report"]["components"] = [{"reference": f"R{i}"} for i in range(10)]
        row["report"]["nets"] = [f"N{i}" for i in range(13)]
        grade = self.grade(row, facts=facts)

        self.assertEqual(value_grading.DELIVERED, grade.grade, grade.reasons)
        self.assertIs(False, grade.signals["net_recovery_verified"])

        # An EXACT count still fails it.
        exact = self.grade(row, facts={**facts, "declared_nets_exact": True})
        self.assertEqual(value_grading.FAILED, exact.grade)

    def test_more_nets_than_the_file_declares_fails(self) -> None:
        # The floor alone was clearable with fabricated names: the gate reads the
        # declared count exactly, and honest recovery is 1.00 on all 61 measured
        # boards, so a report claiming more than the file declares invented them.
        facts = {
            "kind": "native", "input_format": "kicad_pcb",
            "input_placements": 10, "input_references": None,
            "input_declared_nets": 10,
        }
        row = journey_row(components=10, nets=13)
        row["report"]["components"] = [{"reference": f"R{i}"} for i in range(10)]
        row["report"]["nets"] = [f"N{i}" for i in range(13)]
        grade = self.grade(row, facts=facts)

        self.assertEqual(value_grading.FAILED, grade.grade)
        self.assertTrue(
            any("from a file that declares 10" in r for r in grade.reasons)
        )

    def test_an_unnamed_open_part_still_keeps_a_board_off_delivered(self) -> None:
        # Skipping reference-less open parts entirely let an empty reference buy
        # `delivered`, which is the one value an inflating report would write.
        for reference in ("Q1", "?", ""):
            row = journey_row(critical="2/2")
            row["report"]["bind"]["open_parts"] = [
                {"reference": reference, "active_ic": False}
            ]
            row["report"]["assumptions"] = []
            grade = self.grade(row)
            self.assertEqual(value_grading.FAILED, grade.grade, repr(reference))

        # Named, with its upload: honest degradation.
        row = journey_row(critical="2/2")
        row["report"]["bind"]["open_parts"] = [{"reference": "Q1"}]
        row["report"]["assumptions"] = [
            {"kind": "open_part", "replacement": "Add a model for Q1."}
        ]
        self.assertEqual(value_grading.DEGRADED, self.grade(row).grade)

        # Unnamed, but something is offered that would close it.
        row = journey_row(critical="2/2")
        row["report"]["bind"]["open_parts"] = [{"reference": ""}]
        row["report"]["assumptions"] = [
            {"kind": "open_part", "replacement": "Add models for the open parts."}
        ]
        self.assertEqual(value_grading.DEGRADED, self.grade(row).grade)

    def test_a_net_total_smaller_than_its_inventory_is_not_a_defect(self) -> None:
        # Only a total LARGER than its inventory can clear the connectivity floor
        # on names the report never produced; a smaller one buys nothing, and
        # failing it would be stricter than the rule's purpose.
        row = journey_row(nets=3)
        row["report"]["nets"] = [f"N{i}" for i in range(10)]
        self.assertEqual(value_grading.DELIVERED, self.grade(row).grade)

    def test_a_net_total_that_outruns_its_inventory_fails(self) -> None:
        # Without this the connectivity floor would trust a total the report
        # never backed with names, which is a number worth inflating.
        row = journey_row(nets=30)
        row["report"]["nets"] = ["GND", "VCC"]
        grade = self.grade(row)

        self.assertEqual(value_grading.FAILED, grade.grade)
        self.assertTrue(
            any("claims 30 nets and names 2" in reason for reason in grade.reasons)
        )

    def test_connectivity_is_measured_against_the_nets_the_file_declares(
        self,
    ) -> None:
        # The ardep collapse on the commonest format: a layout whose file
        # declares a hundred nets and whose report returns two. Without this the
        # only native net rule was "at least two", which that report satisfies.
        facts = {
            "kind": "native", "input_format": "kicad_pcb",
            "input_placements": 100, "input_references": None,
            "input_declared_nets": 120,
        }
        row = journey_row(components=100, nets=2)
        row["report"]["components"] = [{"reference": f"R{i}"} for i in range(100)]
        row["report"]["nets"] = ["GND", "VCC"]
        grade = self.grade(row, facts=facts)

        self.assertEqual(value_grading.FAILED, grade.grade)
        self.assertTrue(any("connectivity collapsed" in r for r in grade.reasons))

        # A full reconstruction of the same file is fine.
        kept = journey_row(components=100, nets=120)
        kept["report"]["components"] = [{"reference": f"R{i}"} for i in range(100)]
        self.assertEqual(
            value_grading.DELIVERED, self.grade(kept, facts=facts).grade
        )

    def test_both_kicad_net_spellings_are_read(self) -> None:
        # KiCad writes `(net <id> "NAME")` up to 9 and the bare `(net "NAME")`
        # from 10. Matching only the numbered form read ZERO nets out of a real
        # KiCad 10 corpus board with 52 of them, and the docs then blamed the
        # board for declaring none. Matching only names undercounts instead,
        # because KiCad leaves single-pad nets unnamed.
        with tempfile.TemporaryDirectory() as raw:
            numbered = Path(raw) / "old.kicad_pcb"
            numbered.write_text(
                '(kicad_pcb (net 0 "") (net 1 "GND") (net 2 "VCC")\n'
                '  (pad "1" (net 1 "GND")) (pad "2" (net 3 "")))\n'
            )
            bare = Path(raw) / "new.kicad_pcb"
            bare.write_text(
                '(kicad_pcb (net "GND") (net "VCC") (net "SDA")\n'
                '  (pad "1" (net "GND")) (pad "2" (net "")))\n'
            )

            # Three ids besides the unconnected 0, including one unnamed net.
            self.assertEqual(
                3, value_grading.native_declared_nets("kicad_pcb", numbered)
            )
            # Three names, the empty one being the unconnected pseudo-net.
            self.assertEqual(
                3, value_grading.native_declared_nets("kicad_pcb", bare)
            )

    def test_declared_nets_are_read_from_a_real_board(self) -> None:
        root = Path(unseen_boards.__file__).resolve().parent.parent
        board = root / self.REAL_BOARD
        golden = json.loads((root / self.REAL_GOLDEN).read_text(encoding="utf-8"))
        facts = value_grading.input_facts("kicad_pcb", board)

        # The golden report names four nets; the file declares the same four.
        self.assertEqual(golden["num_nets"], facts["input_declared_nets"])

    def test_xml_entities_in_eagle_names_are_read_the_way_the_reader_reads_them(
        self,
    ) -> None:
        # The Eagle extractor unescapes attribute values (quick-xml's
        # `unescape_value`, crates/hauksbee-extract/src/eagle.rs), so a net the
        # file spells `STX-&gt;` reaches the report as `STX->`. Comparing raw bytes
        # against that decoded output made the gate call the difference invention
        # and FAIL an honest report: the real `solokeys_solo_usb_a` board in the
        # external pool carries exactly this, on a two-pad net and a test point.
        with tempfile.TemporaryDirectory() as raw:
            eagle = Path(raw) / "board.brd"
            eagle.write_text(
                "<eagle><drawing><board><elements>"
                '<element name="STX-&gt;1" package="TP"/>'
                '<element name="R1" package="0402"/>'
                '<element name="A&amp;B2" package="0402"/>'
                "</elements><signals>"
                '<signal name="STX-&gt;"/><signal name="GND"/>'
                '<signal name="VBUS&amp;5"/>'
                "</signals></board></drawing></eagle>"
            )
            facts = value_grading.input_facts("eagle_brd", eagle)

            self.assertEqual(
                ["A&B2", "R1", "STX->1"], sorted(facts["input_references"])
            )
            self.assertEqual(["GND", "STX->", "VBUS&5"], facts["input_net_names"])

            # The report names what the reader would name. Nothing is invented.
            row = journey_row(components=3, nets=3, critical="0/0")
            row["report"]["components"] = [
                {"reference": "STX->1"}, {"reference": "R1"}, {"reference": "A&B2"}
            ]
            row["report"]["nets"] = ["STX->", "GND", "VBUS&5"]
            grade = self.grade(row, input_format="eagle_brd", facts=facts)

            self.assertEqual(value_grading.DELIVERED, grade.grade, grade.reasons)
            self.assertEqual([], grade.reasons)

            # Still two-sided: a name neither spelling accounts for fails.
            row["report"]["nets"] = ["STX->", "GND", "INVENTED"]
            self.assertEqual(
                value_grading.FAILED,
                self.grade(row, input_format="eagle_brd", facts=facts).grade,
            )

    def test_padding_the_net_inventory_up_to_the_declared_total_fails(self) -> None:
        # The one way a floor on a tool-written COUNT can be cleared without
        # reconstructing anything: keep `num_nets` honest and fill `nets` with
        # names the file never declared. Only identity catches this, and Eagle is
        # where the report's names are comparable to the file's.
        with tempfile.TemporaryDirectory() as raw:
            eagle = Path(raw) / "board.brd"
            eagle.write_text(
                "<eagle><drawing><board><elements>"
                + "".join(f'<element name="R{i}" package="0402"/>' for i in range(4))
                + "</elements><signals>"
                + "".join(f'<signal name="NET{i}"/>' for i in range(20))
                + "</signals></board></drawing></eagle>"
            )
            facts = value_grading.input_facts("eagle_brd", eagle)
            self.assertEqual(20, facts["input_declared_nets"])
            self.assertEqual(20, len(facts["input_net_names"]))

            honest = journey_row(components=4, nets=20, critical="0/0")
            grade = self.grade(honest, input_format="eagle_brd", facts=facts)
            self.assertEqual(value_grading.DELIVERED, grade.grade)
            self.assertTrue(grade.signals["net_identity_verified"])

            padded = journey_row(components=4, nets=20, critical="0/0")
            padded["report"]["nets"] = ["NET0", "NET1"] + [
                f"INVENTED{index}" for index in range(18)
            ]
            grade = self.grade(padded, input_format="eagle_brd", facts=facts)
            self.assertEqual(value_grading.FAILED, grade.grade)
            self.assertTrue(
                any("does not declare" in reason for reason in grade.reasons)
            )

    def test_net_identity_is_not_claimed_where_the_names_are_not_comparable(
        self,
    ) -> None:
        # KiCad reports names that are not literal strings in the file (older
        # writers leave them unquoted, the engine synthesises `Net-(U4-…)`), so
        # the check does not run and the board says so rather than implying it
        # was checked.
        grade = self.grade(journey_row(), facts=ANCHORED_KICAD)

        self.assertFalse(grade.signals["net_identity_verified"])
        self.assertNotIn("nets_not_in_the_input", grade.signals)

    def test_whether_a_declared_net_count_is_exact_is_read_from_the_file(
        self,
    ) -> None:
        # `declared_nets_are_exact` is what keeps the floor off boards whose net
        # count could only be read from quoted names, which under-state: 17 real
        # corpus boards carry single-pad nets with empty names.
        with tempfile.TemporaryDirectory() as raw:
            quoted = Path(raw) / "quoted.kicad_pcb"
            quoted.write_text(
                '(kicad_pcb (version 4)\n  (net "GND")\n  (net "VCC")\n)\n'
            )
            ided = Path(raw) / "ided.kicad_pcb"
            ided.write_text(
                "(kicad_pcb (version 20221018)\n"
                + "".join(f'  (net {i} "N{i}")\n' for i in range(1, 21))
                + ")\n"
            )

            self.assertFalse(value_grading.declared_nets_are_exact("kicad_pcb", quoted))
            self.assertTrue(value_grading.declared_nets_are_exact("kicad_pcb", ided))

            # Two names read, twenty nets reported: no failure, because the
            # count the gate holds is known to be a floor on the truth.
            facts = value_grading.input_facts("kicad_pcb", quoted)
            self.assertEqual(2, facts["input_declared_nets"])
            self.assertFalse(facts["declared_nets_exact"])
            grade = self.grade(
                journey_row(components=4, nets=20, critical="0/0"), facts=facts
            )
            self.assertNotEqual(value_grading.FAILED, grade.grade)

            # The exact count does drive one: 20 declared, 2 recovered.
            facts = value_grading.input_facts("kicad_pcb", ided)
            self.assertTrue(facts["declared_nets_exact"])
            grade = self.grade(
                journey_row(components=4, nets=2, critical="0/0"), facts=facts
            )
            self.assertEqual(value_grading.FAILED, grade.grade)

    def test_a_report_that_ran_no_checks_at_all_fails(self) -> None:
        # Components extracted, nets found, models bound, and not one check run.
        # Nothing was concluded about the board, so nothing was delivered.
        grade = self.grade(journey_row(sections=0))

        self.assertEqual(value_grading.FAILED, grade.grade)
        self.assertTrue(any("no check" in reason for reason in grade.reasons))

    def test_a_section_that_is_not_a_section_fails(self) -> None:
        # A string where a check belongs is not a conclusion about the board, and
        # counting it would let `["ok", "ok", "ok"]` satisfy the section floor.
        row = journey_row()
        row["report"]["sections"] = [
            {"title": "Connectivity", "verdict": "Looks healthy.", "findings": []},
            "Power is fine",
            {"title": "Thermal", "verdict": "Within limits.", "findings": []},
        ]
        grade = self.grade(row)

        self.assertEqual(value_grading.FAILED, grade.grade)
        self.assertTrue(
            any("is not a section" in reason for reason in grade.reasons),
            grade.reasons,
        )

    def test_an_annotated_designator_cannot_pad_the_coverage_floor(self) -> None:
        # `@` is the engine's own annotation (`REF**@conflict-2`, `Q9@Top/AUX/M`),
        # so it passes the identity check by base name and used to pass the
        # uniqueness rule too, because the raw strings differ. Counting raw strings
        # therefore let one real designator stand in for a whole board.
        with tempfile.TemporaryDirectory() as raw:
            kicad = Path(raw) / "board.kicad_pcb"
            kicad.write_text(
                "(kicad_pcb (version 20221018)\n"
                + "".join(
                    f'  (footprint "R_0402" (at {index} 0)\n'
                    f'    (property "Reference" "R{index}"))\n'
                    for index in range(40)
                )
                + '  (net 1 "GND")\n  (net 2 "VCC")\n)\n'
            )
            facts = value_grading.input_facts("kicad_pcb", kicad)
            self.assertEqual(40, facts["input_placements"])

            row = journey_row(components=40, nets=2, critical="0/0")
            # Four real parts, thirty-six annotated copies of one of them.
            row["report"]["components"] = [
                {"reference": f"R{index}"} for index in range(4)
            ] + [{"reference": f"R0@{index}"} for index in range(36)]
            grade = self.grade(row, facts=facts)

            self.assertEqual(value_grading.FAILED, grade.grade)
            self.assertEqual(4, grade.signals["recovered_components"])
            self.assertEqual(0.1, grade.signals["placement_recovery_fraction"])

    def test_a_lone_placeholder_section_is_not_a_set_of_checks(self) -> None:
        # "It ran checks" was satisfied by one titled section with a verdict, so a
        # regression that dropped DRC, thermal and SI and kept only a summary
        # graded `delivered` on a real board. All 64 successful reports in the
        # retained evidence carry three sections or four.
        grade = self.grade(journey_row(sections=1))

        self.assertEqual(value_grading.FAILED, grade.grade)
        self.assertTrue(
            any("check conclusion" in reason for reason in grade.reasons),
            grade.reasons,
        )

        # Two is the floor, a full section below every real board, so a format
        # that legitimately reaches fewer conclusions is not punished.
        self.assertEqual(
            value_grading.DELIVERED, self.grade(journey_row(sections=2)).grade
        )

    def test_a_netlist_is_held_to_the_two_net_floor(self) -> None:
        grade = self.grade(
            journey_row(components=20, nets=1, critical="0/0"),
            input_format="ipc_356",
        )

        self.assertEqual(value_grading.FAILED, grade.grade)
        self.assertTrue(any("netlist input yielded 1" in r for r in grade.reasons))

    def test_a_bound_part_with_open_pins_degrades_rather_than_fails(self) -> None:
        # `bind.open_parts` holds two buckets, told apart by `bound`. A `bound`
        # entry is `resolved_but_open_active`: the model IS there and the input's
        # own wiring leaves pins undriven. The engine emits an `open_part`
        # assumption only for UNRESOLVED parts, so no model-kind unlock can ever
        # name one of these; demanding one failed a truthful report and told the
        # reader no model had been uploaded for a part that has a model.
        row = journey_row(critical="8/8")
        row["report"]["bind"]["open_parts"] = [
            {
                "reference": "Q3",
                "value": "IRLML6402",
                "reason": "pin 2 undriven",
                "consequence": "Q3 is a resolved active IC with open pins",
                "active_ic": True,
                "bound": True,
            }
        ]
        grade = self.grade(row)

        self.assertEqual(value_grading.DEGRADED, grade.grade, grade.reasons)
        self.assertEqual([], grade.reasons)
        self.assertEqual(["Q3"], grade.signals["resolved_open_parts"])
        # The unlock names a design change, because no upload closes this.
        self.assertTrue(
            any("Drive or connect the open pins of Q3" in u for u in grade.unlocks)
        )

        # The UNBOUND bucket is unchanged: no model, nothing offered, fails.
        unbound = journey_row(critical="8/8")
        unbound["report"]["bind"]["open_parts"] = [
            {"reference": "U9", "active_ic": True, "reason": "no model",
             "bound": False}
        ]
        failed = self.grade(unbound)
        self.assertEqual(value_grading.FAILED, failed.grade)
        self.assertTrue(any("no model upload for U9" in r for r in failed.reasons))

    def test_unbound_parts_named_nowhere_with_nothing_offered_fail(self) -> None:
        # A shortfall the report never explained is not honest degradation.
        row = journey_row(critical="0/3")
        row["report"]["assumptions"] = []
        row["report"]["bind"]["open_parts"] = []
        grade = self.grade(row)

        self.assertEqual(value_grading.FAILED, grade.grade)
        self.assertTrue(any("named nowhere" in r for r in grade.reasons))

    def test_a_layout_with_fewer_than_two_nets_fails(self) -> None:
        # The degenerate report no real layout produces: one part, one net.
        grade = self.grade(journey_row(components=1, nets=1, critical="0/0"))

        self.assertEqual(value_grading.FAILED, grade.grade)
        self.assertTrue(any("fewer than the 2" in r for r in grade.reasons))

    def test_a_parallel_bank_with_many_parts_and_two_nets_is_fine(self) -> None:
        # A hundred capacitors across two rails is a real design. "More parts
        # implies more nets" is not true, so nothing here may infer it.
        grade = self.grade(journey_row(components=100, nets=2, critical="0/0"))

        self.assertEqual(value_grading.DELIVERED, grade.grade)

    def test_a_malformed_binding_fraction_fails(self) -> None:
        grade = self.grade(journey_row(critical="9/2"))

        self.assertEqual(value_grading.FAILED, grade.grade)
        self.assertTrue(any("malformed" in reason for reason in grade.reasons))

    def test_an_honest_refusal_still_passes(self) -> None:
        grade = self.grade(
            {"report": {"ok": False, "error": "not a format hauksbee reads"}},
            expects_refusal=True,
        )

        self.assertEqual(value_grading.REFUSED_HONEST, grade.grade)
        self.assertEqual([], grade.reasons)


class GerberValueContractTests(unittest.TestCase):
    """The ardep class: honest, exportable, and worth nothing at a bench."""

    def test_a_gerber_package_reconstructing_no_nets_fails_without_a_floor(
        self,
    ) -> None:
        # Below the 500-flash threshold, and on a `no-mcu` board, no derived floor
        # applies. Zero nets is still nothing: a copper package whose whole value
        # is connectivity delivered none of it.
        facts = {
            "kind": "gerber", "gerber_layers": 2, "copper_layers": 2,
            "identified_layers": 2, "copper_classified": True,
            "aperture_flashes": 120, "total_gerber_flashes": 200,
            "input_readable_by_gate": True,
        }
        grade = value_grading.grade_board(
            journey_row(components=0, nets=0, critical=None),
            input_format="gerber_archive", expects_refusal=False,
            facts=facts, axes=("gerber-only", "no-mcu"),
        )

        self.assertEqual(value_grading.FAILED, grade.grade)
        self.assertIsNone(grade.signals.get("expected_min_nets"))
        self.assertEqual(
            ["no connectivity was reconstructed from the copper"], grade.reasons
        )

    def test_the_derived_floor_never_falls_below_two_nets(self) -> None:
        # At the applicability minimum the derived floor is 500 // 200 = 2, and the
        # clamp is what keeps it there rather than at one. A single-net report on a
        # flashed two-layer package is the collapse this contract exists for, so
        # the floor may never round down to something a collapse satisfies.
        facts = {
            "kind": "gerber", "gerber_layers": 2, "copper_layers": 2,
            "identified_layers": 2, "copper_classified": True,
            "aperture_flashes": 500, "total_gerber_flashes": 500,
            "input_readable_by_gate": True,
        }
        self.assertEqual(2, value_grading.expected_min_nets(facts, ("stm32",)))
        grade = value_grading.grade_board(
            journey_row(components=0, nets=1, critical=None),
            input_format="gerber_archive", expects_refusal=False,
            facts=facts, axes=("gerber-only", "stm32"),
        )
        self.assertEqual(value_grading.FAILED, grade.grade)

    def test_applicability_is_judged_on_the_whole_package_not_the_lower_bound(
        self,
    ) -> None:
        # Where copper cannot be told apart, the flash ESTIMATE is deliberately
        # the two smallest films, which is the conservative number to divide. It
        # is the wrong number to decide "is this a real board at all": a package
        # of 1000/1000/10/10-flash unrecognisable films has 2020 flashes and a
        # two-smallest total of 20, so judging applicability on the estimate
        # switched the floor off on the very package the estimate exists for.
        with tempfile.TemporaryDirectory() as raw:
            archive = Path(raw) / "unrecognisable.zip"
            with zipfile.ZipFile(archive, "w") as package:
                for name, count in (
                    ("l1_route.gbr", 1000),
                    ("l2_route.gbr", 1000),
                    ("l3_route.gbr", 10),
                    ("l4_route.gbr", 10),
                ):
                    flashes = "\n".join(
                        f"X{1000 + index}Y2000D03*" for index in range(count)
                    )
                    package.writestr(name, f"%FSLAX46Y46*%\n{flashes}\nM02*\n")

            facts = value_grading.gerber_input_facts(archive)
            self.assertIs(False, facts["copper_classified"])
            self.assertEqual(20, facts["aperture_flashes"])
            self.assertEqual(2020, facts["total_gerber_flashes"])

            # A floor applies, derived from the conservative estimate.
            self.assertEqual(2, value_grading.expected_min_nets(facts, ("stm32",)))
            grade = value_grading.grade_board(
                journey_row(components=0, nets=1, critical=None),
                input_format="gerber_archive", expects_refusal=False,
                facts=facts, axes=("gerber-only", "stm32"),
            )
            self.assertEqual(value_grading.FAILED, grade.grade)

    def test_a_dense_package_that_escapes_the_floor_says_so(self) -> None:
        # The silent variant of the ardep escape. One film looks like copper and
        # the rest positively mis-identify as never-copper by name (Allegro writes
        # `l2_route.gbr`), so the package reads as ONE classified copper layer,
        # falls below the two-layer guard, and keeps `copper_classified: True` —
        # which meant no floor, and no word about it either.
        with tempfile.TemporaryDirectory() as raw:
            archive = Path(raw) / "allegro.zip"
            with zipfile.ZipFile(archive, "w") as package:
                for name, count in (
                    ("top_cu.gbr", 700),
                    ("l2_route.gbr", 700),
                    ("l3_route.gbr", 700),
                    ("bot_route.gbr", 700),
                    ("soldermask_top.gbr", 100),
                ):
                    flashes = "\n".join(
                        f"X{1000 + index}Y2000D03*" for index in range(count)
                    )
                    package.writestr(name, f"%FSLAX46Y46*%\n{flashes}\nM02*\n")

            facts = value_grading.gerber_input_facts(archive)
            self.assertEqual(1, facts["copper_layers"])
            self.assertIs(True, facts["copper_classified"])
            self.assertIsNone(value_grading.expected_min_nets(facts, ("stm32",)))

            grade = value_grading.grade_board(
                journey_row(components=0, nets=1, critical=None),
                input_format="gerber_archive", expects_refusal=False,
                facts=facts, axes=("gerber-only", "stm32"),
            )
            self.assertIs(False, grade.signals["reconstruction_floor_verified"])
            self.assertIn(
                "5 films carrying 2900 aperture flashes",
                grade.signals["reconstruction_floor_caveat"],
            )
            self.assertEqual(
                1,
                len(
                    value_grading.summarize([("allegro", grade)])[
                        "unverified_reconstruction"
                    ]
                ),
            )

    def test_a_small_package_is_not_disclosed_as_a_limitation(self) -> None:
        # The fourth reason a floor does not apply is deliberately silent. A small
        # board draws its copper with D01 and flashes a handful of vias, so
        # reporting "no floor applied" on every one of them is noise, not honesty.
        facts = {
            "kind": "gerber", "gerber_layers": 2, "copper_layers": 2,
            "identified_layers": 2, "copper_classified": True,
            "aperture_flashes": 137, "total_gerber_flashes": 200,
            "input_readable_by_gate": True,
        }
        grade = value_grading.grade_board(
            journey_row(components=0, nets=9, critical=None),
            input_format="gerber_archive", expects_refusal=False,
            facts=facts, axes=("gerber-only", "stm32"),
        )

        self.assertEqual(value_grading.DEGRADED, grade.grade)
        self.assertNotIn("reconstruction_floor_caveat", grade.signals)
        self.assertEqual(
            [],
            value_grading.summarize([("small", grade)])["unverified_reconstruction"],
        )

    def test_a_caveat_never_replaces_the_one_beside_it(self) -> None:
        # Two things can weaken the same floor at once. Keying this on
        # `setdefault` reported only the first, so a package whose copper could
        # not be classified AND whose flash count the reader disputes disclosed
        # the dispute and hid the classification.
        facts = {
            "kind": "gerber", "gerber_layers": 4, "copper_layers": 0,
            "identified_layers": 0, "copper_classified": False,
            "aperture_flashes": 600, "total_gerber_flashes": 2400,
            "input_readable_by_gate": True,
        }
        row = journey_row(components=0, nets=40, critical=None,
                          coverage_note="12 of 2400 aperture flashes (1%) matched.")
        grade = value_grading.grade_board(
            row, input_format="gerber_archive", expects_refusal=False,
            facts=facts, axes=("gerber-only", "stm32"),
        )

        caveat = grade.signals["reconstruction_floor_caveat"]
        self.assertIn("could not classify", caveat)
        self.assertIn("; and ", caveat)

    def test_an_exemption_over_a_dense_package_prints_how_dense(self) -> None:
        # `no-mcu` is a manifest claim and the only switch that turns the sole
        # connectivity check on a copper package off. Honouring it is allowed;
        # honouring it quietly over an ardep-sized package is not.
        facts = {
            "kind": "gerber", "gerber_layers": 11, "copper_layers": 4,
            "identified_layers": 10, "copper_classified": True,
            "aperture_flashes": 2575, "total_gerber_flashes": 3000,
            "input_readable_by_gate": True,
        }
        grade = value_grading.grade_board(
            journey_row(components=0, nets=1, critical=None),
            input_format="gerber_archive", expects_refusal=False,
            facts=facts, axes=("gerber-only", "no-mcu"),
        )

        # Passes, because a passive board really can have one net.
        self.assertEqual(value_grading.DEGRADED, grade.grade)
        self.assertIs(False, grade.signals["reconstruction_floor_verified"])
        caveat = grade.signals["reconstruction_floor_caveat"]
        self.assertIn("2575 aperture flashes across 4 copper film(s)", caveat)
        self.assertTrue(
            any(
                "2575 aperture flashes" in line
                for line in release_gates.describe_degraded(
                    {
                        "value_summary": value_grading.summarize(
                            [("exempt", grade)]
                        )
                    }
                )
            )
        )

        # A package small enough for the exemption to be routine says only that.
        sparse = value_grading.grade_board(
            journey_row(components=0, nets=1, critical=None),
            input_format="gerber_archive", expects_refusal=False,
            facts={**facts, "aperture_flashes": 120, "copper_layers": 2},
            axes=("gerber-only", "no-mcu"),
        )
        self.assertNotIn(
            "aperture flashes", sparse.signals["reconstruction_floor_caveat"]
        )

    def test_a_gerber_package_discloses_that_its_net_names_are_unchecked(
        self,
    ) -> None:
        # A fabrication package declares no net names, so the count is all the
        # gate has and the reader has to be told. This is the format the contract
        # was written for, and it was the one left out of the disclosure.
        facts = {
            "kind": "gerber", "gerber_layers": 4, "copper_layers": 4,
            "identified_layers": 4, "copper_classified": True,
            "aperture_flashes": 2575, "total_gerber_flashes": 3000,
            "input_readable_by_gate": True,
        }
        grade = value_grading.grade_board(
            journey_row(components=0, nets=40, critical=None),
            input_format="gerber_archive", expects_refusal=False,
            facts=facts, axes=("gerber-only", "stm32"),
        )

        self.assertIs(False, grade.signals["net_identity_verified"])
        summary = value_grading.summarize([("copper-only", grade)])
        self.assertEqual(
            [{"board": "copper-only", "input_format": "gerber_archive"}],
            summary["unverified_net_identity"],
        )

    def test_an_unpacked_gerber_directory_is_read_like_an_archive(self) -> None:
        # Staging zips a bundle before the drop, so the archive shape is the live
        # one; this pins the directory shape so that stays true by choice rather
        # than by luck. Reading a directory as zero layers and "floor not
        # applicable" is a silent fail-open. Same films, same numbers, either way.
        with tempfile.TemporaryDirectory() as raw:
            base = Path(raw)
            archive = base / "package.zip"
            gerber_archive(archive, copper_layers=2, flashes_per_layer=400)

            unpacked = base / "unpacked"
            unpacked.mkdir()
            with zipfile.ZipFile(archive) as opened:
                opened.extractall(unpacked)

            from_archive = value_grading.gerber_input_facts(archive)
            from_directory = value_grading.gerber_input_facts(unpacked)

            self.assertEqual(from_archive, from_directory)
            self.assertEqual(2, from_directory["copper_layers"])
            self.assertEqual(800, from_directory["aperture_flashes"])
            self.assertTrue(from_directory["input_readable_by_gate"])

            # And the floor it feeds actually applies, which is the point.
            grade = value_grading.grade_board(
                journey_row(components=0, nets=1, critical=None),
                input_format="gerber_archive",
                expects_refusal=False,
                facts=from_directory,
                axes=("gerber-only", "stm32"),
            )
            self.assertEqual(value_grading.FAILED, grade.grade)
            self.assertEqual(4, grade.signals["expected_min_nets"])

    def test_the_gate_counts_copper_layers_and_flashes_from_the_input_itself(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as raw:
            staged = Path(raw) / "package.zip"
            gerber_archive(staged, copper_layers=4, flashes_per_layer=481)
            facts = value_grading.input_facts("gerber_archive", staged)

            self.assertEqual(4, facts["gerber_layers"])
            self.assertEqual(4, facts["copper_layers"])
            self.assertEqual(1924, facts["aperture_flashes"])
            self.assertTrue(facts["input_readable_by_gate"])
            self.assertEqual(9, value_grading.expected_min_nets(facts, ("stm32",)))

    def test_copper_classification_matches_the_roles_the_engine_reads(self) -> None:
        # Over-matching here is not harmless: a mask film counted as copper adds
        # its apertures to the flash total and raises the reconstruction floor
        # above what the real copper supports, which fails a good board.
        for name, expected in (
            ("board-F_Cu.gbr", True),
            ("board-In1_Cu.gbr", True),
            ("board.gtl", True),
            ("board.g1l", True),
            ("copper_top.gbr", True),
            ("board-F_Mask.gbr", False),
            ("board-F_Silkscreen.gbr", False),
            ("board-F_Paste.gbr", False),
            ("board-Edge_Cuts.gbr", False),
            ("board-F_Fab.gbr", False),
            ("soldermask_over_copper.gbr", False),
            ("a1.gbr", False),
        ):
            self.assertIs(
                expected, value_grading._looks_like_copper(name, b""), name
            )
        # The layer's own X2 declaration outranks any filename convention.
        self.assertTrue(
            value_grading._looks_like_copper(
                "a1.gbr", b"%TF.FileFunction,Copper,L1,Top*%"
            )
        )

    def test_an_x2_file_function_names_copper_whatever_the_filename_is(self) -> None:
        # The standard's own answer beats any filename convention, and it is what
        # the engine reads, so it is consulted first.
        with tempfile.TemporaryDirectory() as raw:
            staged = Path(raw) / "package.zip"
            flashes = "\n".join(f"X{1000 + i}Y2000D03*" for i in range(300))
            with zipfile.ZipFile(staged, "w") as archive:
                for index in range(2):
                    archive.writestr(
                        f"a{index}.gbr",
                        "%FSLAX46Y46*%\n%MOMM*%\n"
                        f"%TF.FileFunction,Copper,L{index + 1},Top*%\n"
                        f"{flashes}\nM02*\n",
                    )
                # Declares itself too, so the package classifies completely.
                archive.writestr(
                    "a9.gbr",
                    "%FSLAX46Y46*%\n%MOMM*%\n"
                    "%TF.FileFunction,Soldermask,Top*%\nM02*\n",
                )
            facts = value_grading.input_facts("gerber_archive", staged)

            self.assertEqual(2, facts["copper_layers"])
            self.assertTrue(facts["copper_classified"])
            self.assertEqual(600, facts["aperture_flashes"])
            self.assertEqual(3, value_grading.expected_min_nets(facts, ("stm32",)))

    def test_a_disputed_flash_count_is_disclosed(self) -> None:
        # The reader publishes its own flash count. When it and the gate's differ
        # by more than a factor of two they are not describing the same copper,
        # and which is wrong cannot be settled from the report, so the floor is
        # marked as resting on a disputed number rather than trusted silently.
        facts = {
            "kind": "gerber", "gerber_layers": 4, "copper_layers": 4,
            "copper_classified": True, "aperture_flashes": 1924,
            "total_gerber_flashes": 1924, "input_readable_by_gate": True,
        }
        row = journey_row(
            components=0, nets=40, critical="0/0",
            coverage_note="0 of 200 aperture flashes (0%) were matched.",
        )
        grade = value_grading.grade_board(
            row, input_format="gerber_archive", expects_refusal=False,
            facts=facts, axes=("gerber-only", "stm32"),
        )

        self.assertEqual(200, grade.signals["reader_reported_flashes"])
        self.assertIs(False, grade.signals["reconstruction_floor_verified"])
        self.assertEqual(
            1, len(value_grading.summarize([("disputed", grade)])[
                "unverified_reconstruction"])
        )

    def test_the_floor_separates_the_two_real_gerber_packages(self) -> None:
        # The calibration this threshold rests on: two real packages in the
        # retained evidence, nearly the same size, on opposite sides of the floor
        # by a wide margin. If a change to the divisor ever collapses that gap,
        # this fails rather than the next release run discovering it.
        for label, flashes, nets, floor, expected in (
            ("inkplate6", 1731, 18, 8, value_grading.DEGRADED),
            ("ardep", 1924, 1, 9, value_grading.FAILED),
        ):
            facts = {
                "kind": "gerber",
                "gerber_layers": 8,
                "copper_layers": 4,
                "copper_classified": True,
                "aperture_flashes": flashes,
                "total_gerber_flashes": flashes,
                "input_readable_by_gate": True,
            }
            self.assertEqual(
                floor, value_grading.expected_min_nets(facts, ("esp32",)), label
            )
            grade = value_grading.grade_board(
                journey_row(
                    components=0,
                    nets=nets,
                    critical="0/0",
                    coverage_note="Supply the original native layout to run DRC.",
                ),
                input_format="gerber_archive",
                expects_refusal=False,
                facts=facts,
                axes=("gerber-only", "esp32"),
            )
            self.assertEqual(expected, grade.grade, label)

    def test_a_passive_two_net_board_is_not_held_to_the_floor(self) -> None:
        # Three hundred capacitors across two rails is 600 flashed pads and
        # exactly two nets, and so is a power-distribution board or an LED
        # backplane. A flash count cannot tell that apart from a collapse, so the
        # floor additionally requires the manifest to declare a microcontroller,
        # whose dozens of pins must land on distinct nets.
        with tempfile.TemporaryDirectory() as raw:
            staged = Path(raw) / "power-plane.zip"
            gerber_archive(staged, copper_layers=2, flashes_per_layer=300)
            facts = value_grading.input_facts("gerber_archive", staged)

            self.assertEqual(600, facts["aperture_flashes"])
            self.assertEqual(3, value_grading.expected_min_nets(facts, ("stm32",)))
            self.assertIsNone(value_grading.expected_min_nets(facts, ("no-mcu",)))
            # Fail-CLOSED: an unfamiliar MCU family, or none stated at all, is
            # still held to the floor. An allowlist of families would have let
            # `efm32` (which the real external pool uses) switch it off silently.
            self.assertEqual(3, value_grading.expected_min_nets(facts, ("efm32",)))
            self.assertEqual(3, value_grading.expected_min_nets(facts, ()))

            grade = value_grading.grade_board(
                journey_row(
                    components=0,
                    nets=2,
                    critical="0/0",
                    coverage_note="Supply the original native layout to run DRC.",
                ),
                input_format="gerber_archive",
                expects_refusal=False,
                facts=facts,
                axes=("gerber-only", "power-electronics", "no-mcu", "medium"),
            )

            self.assertEqual(value_grading.DEGRADED, grade.grade)
            # Skipping the floor is a limit on what the gate can conclude, so it
            # is disclosed rather than left silent.
            self.assertIs(False, grade.signals["reconstruction_floor_verified"])

    def test_a_single_sided_board_is_not_estimated_as_two_layer(self) -> None:
        # One copper film beside its mask, silk and paste films is a POSITIVE
        # identification of a single-sided board, not a classification failure.
        # Estimating a second copper layer for it would invent copper that does
        # not exist, and an LED matrix or backplane is legitimately thousands of
        # pads on three or four nets.
        with tempfile.TemporaryDirectory() as raw:
            staged = Path(raw) / "single-sided.zip"
            pads = "\n".join(f"X{1000 + i}Y2000D03*" for i in range(1200))
            with zipfile.ZipFile(staged, "w") as archive:
                for name in (
                    "matrix-F_Cu.gbr",
                    "matrix-F_Mask.gbr",
                    "matrix-F_Silkscreen.gbr",
                    "matrix-F_Paste.gbr",
                ):
                    archive.writestr(
                        name, f"%FSLAX46Y46*%\n%MOMM*%\n{pads}\nM02*\n"
                    )
            facts = value_grading.input_facts("gerber_archive", staged)

            self.assertEqual(1, facts["copper_layers"])
            self.assertEqual(4, facts["gerber_layers"])
            self.assertTrue(facts["copper_classified"])
            self.assertEqual(1200, facts["aperture_flashes"])
            # One copper layer is below the applicability guard, so no floor.
            self.assertIsNone(value_grading.expected_min_nets(facts, ("stm32",)))

            grade = value_grading.grade_board(
                journey_row(
                    components=0,
                    nets=3,
                    critical="0/0",
                    coverage_note="Supply the original native layout to run DRC.",
                ),
                input_format="gerber_archive",
                expects_refusal=False,
                facts=facts,
                axes=("kicad9", "gerber-only", "automotive", "stm32", "large"),
            )
            self.assertEqual(value_grading.DEGRADED, grade.grade)

    def test_the_ceiling_is_exempt_below_the_flash_minimum(self) -> None:
        # Copper drawn with D01 flashes only a handful of vias and fiducials, so a
        # ceiling derived from forty of those would fail a board with forty real
        # nets. Both bounds share the flash minimum for that reason.
        with tempfile.TemporaryDirectory() as raw:
            staged = Path(raw) / "drawn.zip"
            vias = "\n".join(f"X{1000 + i}Y2000D03*" for i in range(20))
            with zipfile.ZipFile(staged, "w") as archive:
                for layer in ("board-F_Cu.gbr", "board-B_Cu.gbr"):
                    archive.writestr(
                        layer,
                        "%FSLAX46Y46*%\n%MOMM*%\nG36*\nX1000Y1000D02*\n"
                        f"X9000Y9000D01*\nG37*\n{vias}\nM02*\n",
                    )
            facts = value_grading.input_facts("gerber_archive", staged)

            self.assertEqual(40, facts["aperture_flashes"])
            grade = value_grading.grade_board(
                journey_row(
                    components=0, nets=60, critical="0/0",
                    coverage_note="Supply the original native layout to run DRC.",
                ),
                input_format="gerber_archive",
                expects_refusal=False,
                facts=facts,
                axes=("gerber-only", "stm32"),
            )

            self.assertEqual(value_grading.DEGRADED, grade.grade, grade.reasons)

    def test_the_ceiling_uses_an_upper_bound_not_the_floor_estimate(self) -> None:
        # Where copper cannot be classified the floor deliberately reads the two
        # SMALLEST films, which is the wrong number for the ceiling: an Allegro
        # package of two 3-flash and two 900-flash films reconstructing 30 nets
        # was failed for exceeding a ceiling of six.
        with tempfile.TemporaryDirectory() as raw:
            staged = Path(raw) / "allegro.zip"
            def film(flashes: int) -> str:
                pads = "\n".join(f"X{1000 + i}Y2000D03*" for i in range(flashes))
                return f"%FSLAX46Y46*%\n%MOMM*%\n{pads}\nM02*\n"
            with zipfile.ZipFile(staged, "w") as archive:
                archive.writestr("art1.ph", film(3))
                archive.writestr("art2.ph", film(3))
                archive.writestr("art3.ph", film(900))
                archive.writestr("art4.ph", film(900))
            facts = value_grading.input_facts("gerber_archive", staged)

            self.assertFalse(facts["copper_classified"])
            self.assertEqual(6, facts["aperture_flashes"])
            self.assertEqual(1806, facts["total_gerber_flashes"])

            grade = value_grading.grade_board(
                journey_row(
                    components=0, nets=30, critical="0/0",
                    coverage_note="Supply the original native layout to run DRC.",
                ),
                input_format="gerber_archive",
                expects_refusal=False,
                facts=facts,
                axes=("gerber-only", "stm32"),
            )
            self.assertEqual(value_grading.DEGRADED, grade.grade, grade.reasons)

            # The ceiling still bites on the whole package's flash count.
            over = value_grading.grade_board(
                journey_row(
                    components=0, nets=1807, critical="0/0",
                    coverage_note="Supply the original native layout to run DRC.",
                ),
                input_format="gerber_archive",
                expects_refusal=False,
                facts=facts,
                axes=("gerber-only", "stm32"),
            )
            self.assertEqual(value_grading.FAILED, over.grade)
            self.assertTrue(
                any("implausible reconstruction" in r for r in over.reasons)
            )

    def test_the_unclassified_estimate_is_a_real_lower_bound(self) -> None:
        # An AVERAGE share is not a bound: two 1000-flash copper films beside
        # eight 10000-flash unidentified ones average to 16400, eight times the
        # real copper, which would raise the floor and fail a good 20-net board.
        # The two smallest films are a genuine lower bound on two copper films.
        with tempfile.TemporaryDirectory() as raw:
            staged = Path(raw) / "mixed.zip"
            def film(flashes: int) -> str:
                pads = "\n".join(f"X{1000 + i}Y2000D03*" for i in range(flashes))
                return f"%FSLAX46Y46*%\n%MOMM*%\n{pads}\nM02*\n"
            with zipfile.ZipFile(staged, "w") as archive:
                for index in range(2):
                    archive.writestr(f"a{index}.gbr", film(1000))
                for index in range(8):
                    archive.writestr(f"b{index}.gbr", film(10000))
            facts = value_grading.input_facts("gerber_archive", staged)

            self.assertFalse(facts["copper_classified"])
            self.assertEqual(82000, facts["total_gerber_flashes"])
            # The bound, not the average (which would have been 16400).
            self.assertEqual(2000, facts["aperture_flashes"])
            self.assertEqual(10, value_grading.expected_min_nets(facts, ("stm32",)))

            grade = value_grading.grade_board(
                journey_row(
                    components=0,
                    nets=20,
                    critical="0/0",
                    coverage_note="Supply the original native layout to run DRC.",
                ),
                input_format="gerber_archive",
                expects_refusal=False,
                facts=facts,
                axes=("gerber-only", "stm32", "large"),
            )
            self.assertEqual(value_grading.DEGRADED, grade.grade, grade.reasons)

    def test_films_that_are_all_definitely_not_copper_do_not_exempt_a_package(
        self,
    ) -> None:
        # Accepting "every film is accounted for" on its own was fail-open: a
        # package whose films all match a never-copper name (`l1_route.gbr`, which
        # is exactly what Allegro writes) had zero copper layers with everything
        # identified, so it reported zero copper flashes, skipped BOTH bounds and
        # disclosed nothing. That is the ardep escape hatch reachable by naming.
        def film(flashes: int) -> str:
            pads = "\n".join(f"X{1000 + i}Y2000D03*" for i in range(flashes))
            return f"%FSLAX46Y46*%\n%MOMM*%\n{pads}\nM02*\n"

        with tempfile.TemporaryDirectory() as raw:
            staged = Path(raw) / "all-route.zip"
            with zipfile.ZipFile(staged, "w") as archive:
                for index in range(4):
                    archive.writestr(f"l{index}_route.gbr", film(1000))
            facts = value_grading.input_facts("gerber_archive", staged)

            self.assertEqual(0, facts["copper_layers"])
            self.assertEqual(facts["identified_layers"], facts["gerber_layers"])
            self.assertFalse(facts["copper_classified"])
            self.assertEqual(10, value_grading.expected_min_nets(facts, ("stm32",)))

            grade = value_grading.grade_board(
                journey_row(
                    components=0, nets=1, critical="0/0",
                    coverage_note="Supply the original native layout to run DRC.",
                ),
                input_format="gerber_archive",
                expects_refusal=False,
                facts=facts,
                axes=("gerber-only", "stm32"),
            )
            self.assertEqual(value_grading.FAILED, grade.grade)
            self.assertIs(False, grade.signals["reconstruction_floor_verified"])

    def test_enough_named_copper_films_is_a_classification(self) -> None:
        # The real ardep mainboard: Altium writes X2 attributes as `G04 #@! TF.…`
        # comments and emits no `FileFunction`, so one of its eleven films is
        # unidentifiable. Demanding all eleven be accounted for collapsed its
        # copper flash count to the lower bound and its floor to the clamp
        # minimum, on the very board this contract exists to catch. Four films
        # naming themselves as copper is enough to read the copper directly.
        def film(flashes: int, header: str = "") -> str:
            pads = "\n".join(f"X{1000 + i}Y2000D03*" for i in range(flashes))
            return f"%FSLAX46Y46*%\n%MOMM*%\n{header}{pads}\nM02*\n"

        with tempfile.TemporaryDirectory() as raw:
            staged = Path(raw) / "altium.zip"
            with zipfile.ZipFile(staged, "w") as archive:
                for index in range(4):
                    archive.writestr(f"B_Copper_Signal_{index}.gbr", film(500))
                # One film nothing can classify, as Altium leaves it.
                archive.writestr("B.unknown", film(200, "G04 #@! TF.FilePolarity*\n"))
            facts = value_grading.input_facts("gerber_archive", staged)

            self.assertEqual(4, facts["copper_layers"])
            self.assertEqual(5, facts["gerber_layers"])
            self.assertNotEqual(
                facts["identified_layers"], facts["gerber_layers"]
            )
            self.assertTrue(facts["copper_classified"])
            # The copper's own flashes, not a lower bound over the two smallest.
            self.assertEqual(2000, facts["aperture_flashes"])
            self.assertEqual(10, value_grading.expected_min_nets(facts, ("stm32",)))

    def test_one_recognised_film_among_unknowns_is_not_a_classification(
        self,
    ) -> None:
        # Knowing a little must not be weaker than knowing nothing. One
        # recognised copper film beside three unidentified ones gave a collapsed
        # package no floor and no disclosure, while the same package with zero
        # recognised films was correctly failed.
        def film(flashes: int) -> str:
            pads = "\n".join(f"X{1000 + i}Y2000D03*" for i in range(flashes))
            return f"%FSLAX46Y46*%\n%MOMM*%\n{pads}\nM02*\n"

        with tempfile.TemporaryDirectory() as raw:
            ambiguous = Path(raw) / "ambiguous.zip"
            with zipfile.ZipFile(ambiguous, "w") as archive:
                archive.writestr("top.gtl", film(500))
                for index in range(3):
                    archive.writestr(f"x{index}.gbr", film(500))
            facts = value_grading.input_facts("gerber_archive", ambiguous)

            self.assertEqual(1, facts["copper_layers"])
            self.assertEqual(1, facts["identified_layers"])
            self.assertEqual(4, facts["gerber_layers"])
            self.assertFalse(facts["copper_classified"])
            self.assertIsNotNone(value_grading.expected_min_nets(facts, ("stm32",)))

            grade = value_grading.grade_board(
                journey_row(
                    components=0, nets=1, critical="0/0",
                    coverage_note="Supply the original native layout to run DRC.",
                ),
                input_format="gerber_archive",
                expects_refusal=False,
                facts=facts,
                axes=("gerber-only", "stm32"),
            )
            self.assertEqual(value_grading.FAILED, grade.grade)

            # A genuine single-sided board, whose other films say what they are,
            # classifies completely and keeps its exemption.
            single = Path(raw) / "single.zip"
            with zipfile.ZipFile(single, "w") as archive:
                archive.writestr("board-F_Cu.gbr", film(600))
                for name in (
                    "board-F_Mask.gbr", "board-F_Silkscreen.gbr", "board-F_Paste.gbr"
                ):
                    archive.writestr(name, film(600))
            honest = value_grading.input_facts("gerber_archive", single)

            self.assertTrue(honest["copper_classified"])
            self.assertIsNone(value_grading.expected_min_nets(honest, ("stm32",)))

    def test_unclassifiable_copper_still_gets_a_floor_and_is_disclosed(self) -> None:
        # Switching the floor off here would hand every collapsed package an
        # escape hatch behind an unusual naming scheme, which is the same
        # worthless outcome this contract exists to reject. The floor applies on
        # a deliberately under-stated flash count instead, and the fact that an
        # estimate was used rides in the run summary.
        with tempfile.TemporaryDirectory() as raw:
            staged = Path(raw) / "package.zip"
            gerber_archive(
                staged,
                copper_layers=4,
                flashes_per_layer=481,
                layer_names=["a1.gbr", "a2.gbr", "a3.gbr", "a4.gbr"],
            )
            facts = value_grading.input_facts("gerber_archive", staged)

            self.assertEqual(0, facts["copper_layers"])
            self.assertEqual(4, facts["gerber_layers"])
            self.assertFalse(facts["copper_classified"])
            # Four layers of 481 flashes, of which only two are assumed copper.
            self.assertEqual(1924, facts["total_gerber_flashes"])
            self.assertEqual(962, facts["aperture_flashes"])
            self.assertEqual(4, value_grading.expected_min_nets(facts, ("stm32",)))

            grade = value_grading.grade_board(
                journey_row(
                    components=0,
                    nets=1,
                    critical="0/0",
                    coverage_note="Supply the original native layout to run DRC.",
                ),
                input_format="gerber_archive",
                expects_refusal=False,
                facts=facts,
                axes=("kicad9", "gerber-only", "automotive", "stm32", "large"),
            )
            self.assertEqual(value_grading.FAILED, grade.grade)
            self.assertTrue(
                any("reconstruction collapsed" in r for r in grade.reasons)
            )
            self.assertIs(False, grade.signals["reconstruction_floor_verified"])
            summary = value_grading.summarize([("mystery-package", grade)])
            self.assertEqual(1, len(summary["unverified_reconstruction"]))

    def test_a_flash_at_every_offset_around_a_read_boundary_is_counted(self) -> None:
        # Cutting the stream at a fixed offset with an overlap window lost any
        # command that straddled the offset: it fell in the gap between the
        # scanned region and the carried tail. Chunks are cut on the last `*`
        # instead, so sweep every offset across the boundary and require exactly
        # one flash from each.
        chunk = value_grading._READ_CHUNK
        for offset in range(chunk - 24, chunk + 8):
            payload = (
                b"%FS" + b"A" * (offset - 3) + b"D03*" + b"B" * 40
            )
            counted, is_gerber, _ = value_grading._count_flashes(io.BytesIO(payload))
            self.assertTrue(is_gerber)
            self.assertEqual(1, counted, f"offset {offset}")

    def test_a_buffer_with_no_command_terminator_is_not_accumulated(self) -> None:
        # A stream with no `*` in it is not Gerber, and must not be held in
        # memory while the reader waits for a terminator that never comes.
        payload = b"%FS" + b"A" * (value_grading._MAX_UNTERMINATED + 64)
        counted, is_gerber, _ = value_grading._count_flashes(io.BytesIO(payload))

        self.assertEqual(0, counted)
        self.assertTrue(is_gerber)

    def test_a_flash_split_across_a_read_boundary_is_counted_once(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            staged = Path(raw) / "package.zip"
            # Long enough to cross several 1 MiB reads at an arbitrary offset.
            gerber_archive(staged, copper_layers=2, flashes_per_layer=90_000)
            facts = value_grading.input_facts("gerber_archive", staged)

            self.assertEqual(180_000, facts["aperture_flashes"])

    def test_one_net_from_four_copper_layers_fails(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            staged = Path(raw) / "package.zip"
            gerber_archive(staged, copper_layers=4, flashes_per_layer=481)
            grade = value_grading.grade_board(
                journey_row(
                    components=0,
                    nets=1,
                    critical="0/0",
                    coverage_note=(
                        "It came from gerber reconstruction: 0 of 1924 aperture "
                        "flashes (0%) were matched to a placed component."
                    ),
                ),
                input_format="gerber_archive",
                expects_refusal=False,
                facts=value_grading.input_facts("gerber_archive", staged),
                axes=("kicad9", "gerber-only", "automotive", "stm32", "large"),
            )

            self.assertEqual(value_grading.FAILED, grade.grade)
            self.assertTrue(
                any("reconstruction collapsed" in reason for reason in grade.reasons)
            )
            self.assertEqual(1924, grade.signals["reader_reported_flashes"])

    def test_rich_reconstruction_from_the_same_package_is_degraded_honest(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as raw:
            staged = Path(raw) / "package.zip"
            gerber_archive(staged, copper_layers=4, flashes_per_layer=481)
            grade = value_grading.grade_board(
                journey_row(
                    components=0,
                    nets=220,
                    critical="0/0",
                    coverage_note=(
                        "Gerber input: clearance DRC needs the original layout "
                        "file and was not run."
                    ),
                ),
                input_format="gerber_archive",
                expects_refusal=False,
                facts=value_grading.input_facts("gerber_archive", staged),
                axes=("kicad9", "gerber-only", "automotive", "stm32", "large"),
            )

            self.assertEqual(value_grading.DEGRADED, grade.grade)
            self.assertTrue(grade.unlocks)

    def test_shattering_the_net_count_is_caught_but_shading_is_not(self) -> None:
        # Named for what it establishes. The ceiling catches a package reporting
        # more nets than it has flashed features; it does NOT catch a report that
        # shades its count just over the floor, which is engine follow-up 2 and is
        # documented as open rather than closed.
        facts = {
            "kind": "gerber", "gerber_layers": 4, "copper_layers": 4,
            "copper_classified": True, "aperture_flashes": 2575,
            "total_gerber_flashes": 2575, "input_readable_by_gate": True,
        }

        def grade_with(nets: int) -> str:
            row = journey_row(
                components=0, nets=nets, critical="0/0",
                coverage_note="Supply the original native layout to run DRC.",
            )
            row["report"]["nets"] = [f"N{i}" for i in range(nets)]
            return value_grading.grade_board(
                row, input_format="gerber_archive", expects_refusal=False,
                facts=facts, axes=("gerber-only", "stm32"),
            ).grade

        self.assertEqual(12, value_grading.expected_min_nets(facts, ("stm32",)))
        self.assertEqual(value_grading.FAILED, grade_with(1))
        self.assertEqual(value_grading.FAILED, grade_with(11))
        # Shading just over the floor passes, and the docs say so.
        self.assertEqual(value_grading.DEGRADED, grade_with(12))
        # Shattering does not.
        self.assertEqual(value_grading.FAILED, grade_with(2576))

    def test_inflating_the_net_count_is_not_an_escape_route(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            staged = Path(raw) / "package.zip"
            gerber_archive(staged, copper_layers=4, flashes_per_layer=481)
            grade = value_grading.grade_board(
                journey_row(components=0, nets=99_999, critical="0/0"),
                input_format="gerber_archive",
                expects_refusal=False,
                facts=value_grading.input_facts("gerber_archive", staged),
                axes=("kicad9", "gerber-only", "automotive", "stm32", "large"),
            )

            self.assertEqual(value_grading.FAILED, grade.grade)
            self.assertTrue(
                any("implausible reconstruction" in reason for reason in grade.reasons)
            )

    def test_the_ceiling_sits_exactly_at_one_net_per_flashed_feature(self) -> None:
        # A net needs at least one flashed feature to sit on, so the ceiling is
        # the flash count itself. Exercise both sides of that exact boundary
        # rather than a number that would pass under any nearby rule.
        with tempfile.TemporaryDirectory() as raw:
            staged = Path(raw) / "package.zip"
            gerber_archive(staged, copper_layers=2, flashes_per_layer=300)
            facts = value_grading.input_facts("gerber_archive", staged)
            self.assertEqual(600, facts["aperture_flashes"])

            def grade_with(nets: int) -> str:
                return value_grading.grade_board(
                    journey_row(
                        components=0,
                        nets=nets,
                        critical="0/0",
                        coverage_note="Supply the original layout to run DRC.",
                    ),
                    input_format="gerber_archive",
                    expects_refusal=False,
                    facts=facts,
                    axes=("gerber-only", "stm32", "medium"),
                ).grade

            # One net per flash is the most the copper can carry, and one more
            # than that cannot be true.
            self.assertEqual(value_grading.DEGRADED, grade_with(600))
            self.assertEqual(value_grading.FAILED, grade_with(601))

    def test_fragmenting_the_copper_over_the_floor_is_not_an_escape_route(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            staged = Path(raw) / "package.zip"
            gerber_archive(staged, copper_layers=2, flashes_per_layer=300)
            facts = value_grading.input_facts("gerber_archive", staged)
            grade = value_grading.grade_board(
                journey_row(components=0, nets=900, critical="0/0"),
                input_format="gerber_archive",
                expects_refusal=False,
                facts=facts,
                axes=("kicad9", "gerber-only", "automotive", "stm32", "large"),
            )

            self.assertEqual(value_grading.FAILED, grade.grade)
            self.assertTrue(
                any("implausible reconstruction" in r for r in grade.reasons)
            )

    def test_a_model_unlock_is_not_quoted_for_a_fabrication_package(
        self,
    ) -> None:
        # "Add a model for U1" would not place a single pad from copper, so it
        # must not appear as the unlock. The cap itself needs no unlock from the
        # report, so the gate states the format-level fact instead.
        with tempfile.TemporaryDirectory() as raw:
            staged = Path(raw) / "package.zip"
            gerber_archive(staged, copper_layers=4, flashes_per_layer=481)
            row = journey_row(components=0, nets=220, critical="0/0")
            row["report"]["assumptions"] = [
                {"kind": "open_part", "replacement": "Add a model for U1."}
            ]
            grade = value_grading.grade_board(
                row,
                input_format="gerber_archive",
                expects_refusal=False,
                facts=value_grading.input_facts("gerber_archive", staged),
                axes=("kicad9", "gerber-only", "automotive", "stm32", "large"),
            )

            self.assertEqual(value_grading.DEGRADED, grade.grade)
            self.assertEqual(
                [value_grading.GERBER_STRUCTURAL_UNLOCK], grade.unlocks
            )
            self.assertFalse(any("Add a model" in u for u in grade.unlocks))

    def test_one_invented_component_does_not_buy_a_delivered_gerber(self) -> None:
        # DRC needs the native layout's rules whatever the component count is,
        # so copper alone is capped at `degraded` and no component count can lift
        # it. That is what leaves nothing to gain by inventing one.
        with tempfile.TemporaryDirectory() as raw:
            staged = Path(raw) / "package.zip"
            gerber_archive(staged, copper_layers=4, flashes_per_layer=481)
            facts = value_grading.input_facts("gerber_archive", staged)
            for components in (0, 1, 40):
                grade = value_grading.grade_board(
                    journey_row(
                        components=components,
                        nets=220,
                        critical="0/0",
                        coverage_note="Supply the original native layout to run DRC.",
                    ),
                    input_format="gerber_archive",
                    expects_refusal=False,
                    facts=facts,
                )
                self.assertEqual(
                    value_grading.DEGRADED, grade.grade, f"{components} components"
                )

    def test_drawn_and_filled_copper_is_not_failed_for_having_no_flashes(self) -> None:
        # Hauksbee's reader treats D01 interpolation and G36/G37 regions as
        # copper. A package built that way has no flashes to divide, so both
        # bounds are arithmetic on nothing and must not apply.
        with tempfile.TemporaryDirectory() as raw:
            staged = Path(raw) / "drawn.zip"
            with zipfile.ZipFile(staged, "w") as archive:
                for layer in ("board-F_Cu.gbr", "board-B_Cu.gbr"):
                    archive.writestr(
                        layer,
                        "%FSLAX46Y46*%\n%MOMM*%\nG36*\nX1000Y1000D02*\n"
                        "X2000Y1000D01*\nX2000Y2000D01*\nG37*\nM02*\n",
                    )
                archive.writestr("board.drl", "M48\nM30\n")
            facts = value_grading.input_facts("gerber_archive", staged)

            self.assertEqual(0, facts["aperture_flashes"])
            self.assertEqual(2, facts["gerber_layers"])
            self.assertIsNone(value_grading.expected_min_nets(facts, ("stm32",)))
            grade = value_grading.grade_board(
                journey_row(
                    components=0,
                    nets=2,
                    critical="0/0",
                    coverage_note="Supply the original native layout to run DRC.",
                ),
                input_format="gerber_archive",
                expects_refusal=False,
                facts=facts,
                axes=("kicad9", "gerber-only", "automotive", "stm32", "large"),
            )

            self.assertEqual(value_grading.DEGRADED, grade.grade)

    def test_a_small_package_is_not_held_to_the_reconstruction_floor(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            staged = Path(raw) / "coupon.zip"
            gerber_archive(staged, copper_layers=2, flashes_per_layer=12)
            facts = value_grading.input_facts("gerber_archive", staged)
            self.assertIsNone(value_grading.expected_min_nets(facts, ("stm32",)))
            grade = value_grading.grade_board(
                journey_row(
                    components=0,
                    nets=2,
                    critical="0/0",
                    coverage_note="Supply the original native layout to run DRC.",
                ),
                input_format="gerber_archive",
                expects_refusal=False,
                facts=facts,
                axes=("kicad9", "gerber-only", "automotive", "stm32", "large"),
            )

            self.assertEqual(value_grading.DEGRADED, grade.grade)


class FirmwareJourneyContractTests(unittest.TestCase):
    def test_a_manifest_may_pair_firmware_with_a_board(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            base = Path(raw)
            root = base / "corpus"
            source = root / "kit"
            source.mkdir(parents=True)
            (source / "board.kicad_pcb").write_bytes(b"(kicad_pcb kit)")
            (source / "firmware.elf").write_bytes(b"\x7fELF fixture")
            (source / ".hauksbee-rev").write_text(f"{1:040x}\n")
            manifest = base / "corpus.toml"
            manifest.write_text(
                'cohort = "corpus"\n\n'
                "[[board]]\n"
                'id = "kit"\n'
                f'rev = "{1:040x}"\n'
                'license = "MIT"\n'
                "license_confirmed = true\n"
                'axes = ["kicad"]\n'
                'expect = ["board.kicad_pcb"]\n'
                'firmware = "firmware.elf"\n'
                'firmware_expect = "cosim"\n'
            )

            candidates = discover_candidates(root, manifest_path=manifest)

            self.assertEqual(1, len(candidates))
            self.assertEqual("kit/firmware.elf", candidates[0].firmware_relative_path)
            self.assertEqual("cosim", candidates[0].firmware_expect)
            staged = unseen_boards.materialize_firmware(candidates[0], base / "staged")
            self.assertIsNotNone(staged)
            self.assertEqual(b"\x7fELF fixture", staged.read_bytes())

    def test_the_reservation_records_the_firmware_digest(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            base = Path(raw)
            root = base / "corpus"
            source = root / "kit"
            source.mkdir(parents=True)
            (source / "board.kicad_pcb").write_bytes(b"(kicad_pcb kit)")
            (source / "firmware.elf").write_bytes(b"\x7fELF fixture")
            (source / ".hauksbee-rev").write_text(f"{1:040x}\n")
            manifest = base / "corpus.toml"
            manifest.write_text(
                'cohort = "corpus"\n\n'
                "[[board]]\n"
                'id = "kit"\n'
                f'rev = "{1:040x}"\n'
                'license = "MIT"\n'
                "license_confirmed = true\n"
                'axes = ["kicad"]\n'
                'expect = ["board.kicad_pcb"]\n'
                'firmware = "firmware.elf"\n'
            )
            candidate = discover_candidates(root, manifest_path=manifest)[0]

            record = unseen_boards._board_record(candidate)

            # The ledger line names the bytes, not just the path.
            self.assertEqual(
                hashlib.sha256(b"\x7fELF fixture").hexdigest(),
                record["firmware_sha256"],
            )

    def test_a_vanished_firmware_still_closes_the_reservation(self) -> None:
        # `_board_record` and `candidate_pool_digest` both read the paired image,
        # and both are called from the handler whose job is to write the terminal
        # ledger record. An OSError escaping there leaves a reserved iteration
        # with no terminal result, which an append-only ledger can never correct.
        with tempfile.TemporaryDirectory() as raw:
            base = Path(raw)
            root = base / "corpus"
            source = root / "kit"
            source.mkdir(parents=True)
            (source / "board.kicad_pcb").write_bytes(b"(kicad_pcb kit)")
            (source / "firmware.elf").write_bytes(b"\x7fELF")
            (source / ".hauksbee-rev").write_text(f"{1:040x}\n")
            manifest = base / "corpus.toml"
            manifest.write_text(
                'cohort = "corpus"\n\n[[board]]\nid = "kit"\n'
                f'rev = "{1:040x}"\nlicense = "MIT"\nlicense_confirmed = true\n'
                'axes = ["kicad"]\nexpect = ["board.kicad_pcb"]\n'
                'firmware = "firmware.elf"\n'
            )
            candidate = discover_candidates(root, manifest_path=manifest)[0]
            (source / "firmware.elf").unlink()

            # A SelectionError, which every caller and the CLI already handle,
            # rather than an OSError that escapes them.
            with self.assertRaisesRegex(SelectionError, r"firmware is unreadable"):
                unseen_boards._board_record(candidate)
            with self.assertRaisesRegex(SelectionError, r"firmware is unreadable"):
                candidate_pool_digest([candidate])

    def test_firmware_swapped_after_reservation_is_refused_at_staging(self) -> None:
        # A reservation names specific firmware bytes. Staging different ones
        # would let the journey co-simulate an image the ledger never recorded.
        with tempfile.TemporaryDirectory() as raw:
            base = Path(raw)
            root = base / "corpus"
            source = root / "kit"
            source.mkdir(parents=True)
            (source / "board.kicad_pcb").write_bytes(b"(kicad_pcb kit)")
            (source / "firmware.elf").write_bytes(b"\x7fELF original")
            (source / ".hauksbee-rev").write_text(f"{1:040x}\n")
            manifest = base / "corpus.toml"
            manifest.write_text(
                'cohort = "corpus"\n\n'
                "[[board]]\n"
                'id = "kit"\n'
                f'rev = "{1:040x}"\n'
                'license = "MIT"\n'
                "license_confirmed = true\n"
                'axes = ["kicad"]\n'
                'expect = ["board.kicad_pcb"]\n'
                'firmware = "firmware.elf"\n'
            )
            candidate = discover_candidates(root, manifest_path=manifest)[0]
            reserved = hashlib.sha256(b"\x7fELF original").hexdigest()

            # The image the reservation named still stages.
            self.assertIsNotNone(
                unseen_boards.materialize_firmware(
                    candidate, base / "ok", reserved_sha256=reserved
                )
            )

            (source / "firmware.elf").write_bytes(b"\x7fELF swapped")
            with self.assertRaisesRegex(
                SelectionError, r"firmware changed after reservation"
            ):
                unseen_boards.materialize_firmware(
                    candidate, base / "swapped", reserved_sha256=reserved
                )

    def test_a_firmware_pairing_cannot_be_lost_to_content_dedup(self) -> None:
        # Content dedup keeps the first path-ordered candidate, so a later entry
        # naming identical bytes WITH firmware would silently lose the pairing.
        with tempfile.TemporaryDirectory() as raw:
            base = Path(raw); root = base / "corpus"
            for name in ("aaa", "zzz"):
                source = root / name
                source.mkdir(parents=True)
                (source / "board.kicad_pcb").write_bytes(b"(kicad_pcb same)")
                (source / ".hauksbee-rev").write_text(f"{1:040x}\n")
            (root / "zzz" / "firmware.elf").write_bytes(b"\x7fELF")
            manifest = base / "corpus.toml"
            rows = ['cohort = "corpus"', ""]
            for name in ("aaa", "zzz"):
                rows += [
                    "[[board]]", f'id = "{name}"', f'dest = "{name}"',
                    f'rev = "{1:040x}"', 'license = "MIT"',
                    "license_confirmed = true", 'axes = ["kicad"]',
                    'expect = ["board.kicad_pcb"]',
                ]
                if name == "zzz":
                    rows.append('firmware = "firmware.elf"')
                rows.append("")
            manifest.write_text("\n".join(rows))

            with self.assertRaisesRegex(SelectionError, r"declares firmware for board content"):
                discover_candidates(root, manifest_path=manifest)

    def test_two_entries_pairing_different_firmware_with_one_board_is_refused(
        self,
    ) -> None:
        # The same board with two firmwares is two trials. Keeping whichever
        # sorted first would drop one of them without a word, and checking only
        # for "the first carries none" missed exactly that case.
        with tempfile.TemporaryDirectory() as raw:
            base = Path(raw); root = base / "corpus"
            for name, image in (("aaa", b"\x7fELF one"), ("zzz", b"\x7fELF two")):
                source = root / name
                source.mkdir(parents=True)
                (source / "board.kicad_pcb").write_bytes(b"(kicad_pcb same)")
                (source / ".hauksbee-rev").write_text(f"{1:040x}\n")
                (source / "firmware.elf").write_bytes(image)
            manifest = base / "corpus.toml"
            rows = ['cohort = "corpus"', ""]
            for name in ("aaa", "zzz"):
                rows += [
                    "[[board]]", f'id = "{name}"', f'dest = "{name}"',
                    f'rev = "{1:040x}"', 'license = "MIT"',
                    "license_confirmed = true", 'axes = ["kicad"]',
                    'expect = ["board.kicad_pcb"]',
                    'firmware = "firmware.elf"', "",
                ]
            manifest.write_text("\n".join(rows))

            with self.assertRaisesRegex(SelectionError, r"with different firmware"):
                discover_candidates(root, manifest_path=manifest)

            # The SAME image on both entries is a genuine duplicate, not an error.
            (root / "zzz" / "firmware.elf").write_bytes(b"\x7fELF one")
            candidates = discover_candidates(root, manifest_path=manifest)
            self.assertEqual(1, len(candidates))

    def test_an_expectation_without_a_firmware_path_is_refused(self) -> None:
        # An entry that lost its firmware path would otherwise keep a
        # `firmware_expect` line, produce no firmware plan, and quietly stop
        # exercising the co-simulation it still claims to demand.
        with tempfile.TemporaryDirectory() as raw:
            base = Path(raw)
            source = base / "corpus" / "kit"
            source.mkdir(parents=True)
            (source / "board.kicad_pcb").write_bytes(b"(kicad_pcb kit)")
            (source / ".hauksbee-rev").write_text(f"{1:040x}\n")
            manifest = base / "corpus.toml"
            manifest.write_text(
                'cohort = "corpus"\n\n'
                "[[board]]\n"
                'id = "kit"\n'
                f'rev = "{1:040x}"\n'
                'license = "MIT"\n'
                "license_confirmed = true\n"
                'axes = ["kicad"]\n'
                'expect = ["board.kicad_pcb"]\n'
                'firmware_expect = "cosim"\n'
            )

            with self.assertRaisesRegex(
                SelectionError, r"firmware_expect is declared without a firmware path"
            ):
                discover_candidates(base / "corpus", manifest_path=manifest)

    def test_an_unknown_firmware_expectation_is_refused(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            base = Path(raw)
            root = base / "corpus"
            source = root / "kit"
            source.mkdir(parents=True)
            (source / "board.kicad_pcb").write_bytes(b"(kicad_pcb kit)")
            (source / "firmware.elf").write_bytes(b"\x7fELF fixture")
            (source / ".hauksbee-rev").write_text(f"{1:040x}\n")
            manifest = base / "corpus.toml"
            manifest.write_text(
                'cohort = "corpus"\n\n'
                "[[board]]\n"
                'id = "kit"\n'
                f'rev = "{1:040x}"\n'
                'license = "MIT"\n'
                "license_confirmed = true\n"
                'axes = ["kicad"]\n'
                'expect = ["board.kicad_pcb"]\n'
                'firmware = "firmware.elf"\n'
                'firmware_expect = "vibes"\n'
            )

            with self.assertRaisesRegex(SelectionError, r"firmware_expect must be"):
                discover_candidates(root, manifest_path=manifest)

    def test_staged_firmware_that_never_cosimulates_fails(self) -> None:
        grade = value_grading.grade_board(
            journey_row(
                firmware={
                    "staged": True,
                    "loaded": True,
                    "detail": "ELF, AVR",
                    "pin_activity": False,
                    "serial_activity": False,
                    "pin_activity_rendered": None,
                },
                cosim=None,
            ),
            input_format="kicad_pcb",
            expects_refusal=False,
            facts=ANCHORED_KICAD,
            firmware_expect="cosim",
        )

        self.assertEqual(value_grading.FAILED, grade.grade)
        self.assertTrue(
            any("co-simulated nothing" in reason for reason in grade.reasons)
        )

    def test_cosim_that_drives_no_pin_fails(self) -> None:
        # No upload fixes an inert co-sim, so it cannot be honest degradation:
        # a `degraded` grade there would need an unlock the gate invented for
        # itself, which is exactly what this contract must not do.
        grade = value_grading.grade_board(
            journey_row(
                firmware={
                    "staged": True,
                    "loaded": True,
                    "detail": "ELF, AVR",
                    "pin_activity": False,
                    "serial_activity": False,
                    "pin_activity_rendered": None,
                },
                cosim={
                    "ran": True,
                    "seconds_simulated": 0.25,
                    "uart_output": "",
                    "gpio_nets": [{"name": "PB0", "driven": False}],
                },
            ),
            input_format="kicad_pcb",
            expects_refusal=False,
            facts=ANCHORED_KICAD,
            firmware_expect="cosim",
        )

        self.assertEqual(value_grading.FAILED, grade.grade)
        self.assertTrue(any("drove no pin" in reason for reason in grade.reasons))

    def test_load_only_without_a_stated_reason_fails(self) -> None:
        # Otherwise `load-only` excuses a malformed image, a loader crash and a
        # missing backend equally, which is the escape hatch it must not be.
        grade = value_grading.grade_board(
            journey_row(
                firmware={
                    "staged": True,
                    "loaded": True,
                    "detail": "ELF, e_machine 0x1234",
                },
                cosim={"ran": False},
            ),
            input_format="kicad_pcb",
            expects_refusal=False,
            facts=ANCHORED_KICAD,
            firmware_expect="load-only",
        )

        self.assertEqual(value_grading.FAILED, grade.grade)
        self.assertTrue(any("gave no reason" in reason for reason in grade.reasons))

    def test_load_only_carries_the_reason_the_report_stated(self) -> None:
        row = journey_row(
            firmware={
                "staged": True,
                "loaded": True,
                "detail": "ELF, e_machine 0x1234",
            },
            cosim={"ran": False},
        )
        # The engine's real C5.3 refusal contract, not an invented note shape.
        row["report"]["refusal"] = {
            "claim": "firmware behaviour on this board",
            "missing_prerequisite": "a co-simulation target for that MCU",
            "valid_partial_conclusions": ["the static checks still hold"],
            "next_action": "Rebuild the firmware for a target this build supports.",
        }
        grade = value_grading.grade_board(
            row,
            input_format="kicad_pcb",
            expects_refusal=False,
            facts=ANCHORED_KICAD,
            firmware_expect="load-only",
        )

        self.assertEqual(value_grading.DEGRADED, grade.grade)
        self.assertIn("a co-simulation target for that MCU", grade.unlocks)
        self.assertIn(
            "Rebuild the firmware for a target this build supports.", grade.unlocks
        )

    def test_load_only_still_grades_a_cosim_that_did_happen(self) -> None:
        # `load-only` lowers the bar for what must happen, never the bar for the
        # quality of what did: an image that co-simulates after all is held to
        # the same activity contract as one that was expected to.
        grade = value_grading.grade_board(
            journey_row(
                firmware={
                    "staged": True,
                    "loaded": True,
                    "detail": "ELF, AVR",
                    "pin_activity": False,
                    "serial_activity": False,
                    "pin_activity_rendered": None,
                },
                cosim={"ran": True, "seconds_simulated": 0.25, "gpio_nets": []},
            ),
            input_format="kicad_pcb",
            expects_refusal=False,
            facts=ANCHORED_KICAD,
            firmware_expect="load-only",
        )

        self.assertEqual(value_grading.FAILED, grade.grade)
        self.assertTrue(any("drove no pin" in reason for reason in grade.reasons))

    def test_a_cosim_that_states_no_analog_verdict_fails(self) -> None:
        # Omitting the verdict must not grade better than declaring it invalid,
        # which is what a rule keying only on the literal False would allow.
        grade = value_grading.grade_board(
            journey_row(
                critical="0/1",
                firmware={
                    "staged": True,
                    "loaded": True,
                    "detail": "ELF, AVR",
                    "pin_activity": True,
                    "serial_activity": False,
                    "pin_activity_rendered": True,
                },
                cosim={
                    "ran": True,
                    "seconds_simulated": 0.25,
                    "gpio_nets": [{"name": "PB0", "driven": True}],
                },
            ),
            input_format="kicad_pcb",
            expects_refusal=False,
            facts=ANCHORED_KICAD,
            firmware_expect="cosim",
        )

        self.assertEqual(value_grading.FAILED, grade.grade)
        self.assertTrue(
            any("no analog validity verdict" in r for r in grade.reasons)
        )

    def test_a_cosim_whose_analog_solve_was_invalidated_is_degraded(self) -> None:
        # The unlock is the report's own per-part sentence, not one the gate
        # writes: what makes the voltages usable is binding the open parts.
        grade = value_grading.grade_board(
            journey_row(
                critical="0/1",
                firmware={
                    "staged": True,
                    "loaded": True,
                    "detail": "ELF, AVR",
                    "pin_activity": True,
                    "serial_activity": False,
                    "pin_activity_rendered": True,
                },
                cosim={
                    "ran": True,
                    "seconds_simulated": 0.25,
                    "analog_valid": False,
                    "gpio_nets": [{"name": "PB0", "driven": True}],
                },
            ),
            input_format="kicad_pcb",
            expects_refusal=False,
            facts=ANCHORED_KICAD,
            firmware_expect="cosim",
        )

        self.assertEqual(value_grading.DEGRADED, grade.grade)
        self.assertTrue(any("Add a model for U0" in unlock for unlock in grade.unlocks))

    def test_pin_activity_the_page_never_rendered_fails(self) -> None:
        grade = value_grading.grade_board(
            journey_row(
                firmware={
                    "staged": True,
                    "loaded": True,
                    "detail": "ELF, AVR",
                    "pin_activity": True,
                    "serial_activity": False,
                    "pin_activity_rendered": False,
                },
                cosim={
                    "ran": True,
                    "seconds_simulated": 0.25,
                    "analog_valid": True,
                    "gpio_nets": [{"name": "PB0", "driven": True}],
                },
            ),
            input_format="kicad_pcb",
            expects_refusal=False,
            facts=ANCHORED_KICAD,
            firmware_expect="cosim",
        )

        self.assertEqual(value_grading.FAILED, grade.grade)
        self.assertTrue(
            any("page did not render" in reason for reason in grade.reasons)
        )

    def test_serial_output_alone_is_not_pin_activity(self) -> None:
        # UART output is worth recording and is not a substitute for the one
        # observation that says firmware and hardware interacted.
        grade = value_grading.grade_board(
            journey_row(
                firmware={
                    "staged": True,
                    "loaded": True,
                    "detail": "ELF, AVR",
                    "pin_activity": False,
                    "serial_activity": True,
                    "pin_activity_rendered": None,
                },
                cosim={
                    "ran": True,
                    "seconds_simulated": 0.25,
                    "analog_valid": True,
                    "uart_output": "boot\n",
                    "gpio_nets": [],
                },
            ),
            input_format="kicad_pcb",
            expects_refusal=False,
            facts=ANCHORED_KICAD,
            firmware_expect="cosim",
        )

        self.assertEqual(value_grading.FAILED, grade.grade)
        self.assertTrue(any("drove no pin" in reason for reason in grade.reasons))

    def _firmware_grade(self, **kwargs):
        return value_grading.grade_board(
            journey_row(**kwargs),
            input_format="kicad_pcb",
            expects_refusal=False,
            facts=ANCHORED_KICAD,
            firmware_expect=kwargs.pop("expect", "cosim"),
        )

    def test_every_firmware_failure_branch_is_exercised(self) -> None:
        # One case per reason the firmware dimension can fail, so none of them is
        # only reachable in production.
        cases = [
            ("the manifest declared firmware", None, {"ran": True}, "cosim"),
            (
                "never reached the app's firmware slot",
                {"staged": True, "loaded": False},
                {"ran": True},
                "cosim",
            ),
            (
                "did not report what the image is",
                {"staged": True, "loaded": True, "detail": ""},
                {"ran": False},
                "load-only",
            ),
            (
                "zero simulated seconds",
                {
                    "staged": True, "loaded": True, "detail": "ELF",
                    "pin_activity": True, "serial_activity": False,
                    "pin_activity_rendered": True,
                },
                {"ran": True, "seconds_simulated": 0, "analog_valid": True},
                "cosim",
            ),
        ]
        for fragment, firmware, cosim, expect in cases:
            grade = value_grading.grade_board(
                journey_row(firmware=firmware, cosim=cosim),
                input_format="kicad_pcb",
                expects_refusal=False,
                facts=ANCHORED_KICAD,
                firmware_expect=expect,
            )
            self.assertEqual(value_grading.FAILED, grade.grade, fragment)
            self.assertTrue(
                any(fragment in reason for reason in grade.reasons),
                f"{fragment}: {grade.reasons}",
            )

    def test_an_invalidated_analog_solve_with_nothing_offered_fails(self) -> None:
        grade = value_grading.grade_board(
            journey_row(
                critical="2/2",
                firmware={
                    "staged": True, "loaded": True, "detail": "ELF",
                    "pin_activity": True, "serial_activity": False,
                    "pin_activity_rendered": True,
                },
                cosim={
                    "ran": True, "seconds_simulated": 0.25, "analog_valid": False,
                    "gpio_nets": [{"name": "PB0", "driven": True}],
                },
            ),
            input_format="kicad_pcb",
            expects_refusal=False,
            facts=ANCHORED_KICAD,
            firmware_expect="cosim",
        )

        self.assertEqual(value_grading.FAILED, grade.grade)
        self.assertTrue(
            any("named nothing that would make them usable" in r for r in grade.reasons)
        )

    def test_a_driven_pin_and_a_running_cosim_is_delivered(self) -> None:
        grade = value_grading.grade_board(
            journey_row(
                firmware={
                    "staged": True,
                    "loaded": True,
                    "detail": "ELF, AVR",
                    "pin_activity": True,
                    "serial_activity": True,
                    "pin_activity_rendered": True,
                },
                cosim={
                    "ran": True,
                    "seconds_simulated": 0.25,
                    "analog_valid": True,
                    "uart_output": "boot\n",
                    "gpio_nets": [{"name": "PB0", "driven": True}],
                },
            ),
            input_format="kicad_pcb",
            expects_refusal=False,
            facts=ANCHORED_KICAD,
            firmware_expect="cosim",
        )

        self.assertEqual(value_grading.DELIVERED, grade.grade)


class ValueGateWiringTests(unittest.TestCase):
    """The grades reach the evidence document, the summary, and the exit code."""

    def _pools(self, base: Path) -> tuple[Path, Path, Path, Path]:
        external_root, external_manifest = manifest_pool(
            base, "external", 5, cohort="external"
        )
        corpus_root, corpus_manifest = manifest_pool(base, "corpus", 1, cohort="corpus")
        return external_root, external_manifest, corpus_root, corpus_manifest

    def _run(self, base: Path, runner):
        external_root, external_manifest, corpus_root, corpus_manifest = self._pools(
            base
        )
        return release_gates.run_external_gate(
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
            runner=runner,
        )

    def test_one_dishonest_board_does_not_discard_the_grades_of_the_others(
        self,
    ) -> None:
        # An honesty violation used to abort the whole validation, so a run with
        # one bad journey retained no value grades at all: the operator lost the
        # unlocks for the four boards that were fine. The run must still fail, and
        # it must fail with the graded document kept.
        with tempfile.TemporaryDirectory() as raw:
            base = Path(raw)

            def dishonest(paths, output, base_url, cohort, refusals, firmware=None):
                code = successful_browser_runner()(
                    paths, output, base_url, cohort, refusals, firmware
                )
                results_path = output / "results.json"
                document = json.loads(results_path.read_text(encoding="utf-8"))
                # Exported JSON it never validated: honest on its face, false.
                document["results"][2]["exported"] = False
                results_path.write_text(json.dumps(document))
                return code

            with self.assertRaises(SelectionError) as caught:
                self._run(base, dishonest)
            self.assertIn("did not validate JSON export", str(caught.exception))

            evidence = json.loads(
                (base / "evidence" / "release-01.json").read_text(encoding="utf-8")
            )
            self.assertEqual("failed", evidence["status"])

            rows = evidence["browser"]["results"]
            self.assertEqual(5, len(rows))
            grades = [row["value"]["grade"] for row in rows]
            self.assertEqual("honesty-failed", grades[2])
            # The other four keep the grade they earned, in the document.
            self.assertEqual(4, sum(grade == "delivered" for grade in grades))
            self.assertEqual(
                4, len(evidence["browser"]["value_summary"]["delivered"])
            )

    def test_a_delivered_run_records_the_grade_in_retained_evidence(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            base = Path(raw)
            evidence = self._run(base, successful_browser_runner())

            self.assertEqual("completed", evidence["status"])
            summary = evidence["browser"]["value_summary"]
            self.assertEqual(5, len(summary["delivered"]))
            self.assertEqual([], summary["failed"])
            self.assertEqual([], summary["degraded"])
            self.assertTrue(
                all(
                    row["value"]["grade"] == "delivered"
                    for row in evidence["browser"]["results"]
                )
            )

    def test_the_command_exits_nonzero_on_a_collapsed_board(self) -> None:
        # Through the real CLI, not the gate function: a value failure has to
        # reach the shell as exit 2 with a readable reason and no traceback, the
        # same way every other gate error does.
        with tempfile.TemporaryDirectory() as raw:
            base = Path(raw)
            external_root, external_manifest = manifest_pool(
                base, "external", 5, cohort="external"
            )
            corpus_root, corpus_manifest = manifest_pool(
                base, "corpus", 1, cohort="corpus"
            )

            def collapsing(
                paths: list[Path],
                output: Path,
                base_url: str,
                cohort: str,
                refusals: list[Path],
                firmware: list[dict | None] | None = None,
            ) -> int:
                successful_browser_runner()(
                    paths, output, base_url, cohort, refusals, firmware
                )
                artifact = json.loads((output / "results.json").read_text())
                artifact["results"][2]["report"]["num_components"] = 0
                (output / "results.json").write_text(json.dumps(artifact))
                return 0

            stderr = io.StringIO()
            with (
                patch.object(release_gates, "CANONICAL_HISTORY", base / "h.jsonl"),
                patch.object(release_gates, "CANONICAL_EVIDENCE_DIR", base / "ev"),
                patch.object(release_gates, "RELEASE_SCRATCH", base / "s"),
                patch.object(release_gates, "_playwright_runner", collapsing),
                patch.object(unseen_boards, "current_tool_commit", lambda: "e" * 40),
                redirect_stderr(stderr),
            ):
                exit_code = main(
                    [
                        "run-external-five",
                        "--external-root",
                        str(external_root),
                        "--external-manifest",
                        str(external_manifest),
                        "--corpus-root",
                        str(corpus_root),
                        "--corpus-manifest",
                        str(corpus_manifest),
                        "--iteration-id",
                        "cli-value-01",
                        "--base-url",
                        "http://127.0.0.1:37651",
                    ]
                )

            self.assertEqual(2, exit_code)
            self.assertIn("no bench-grade value", stderr.getvalue())
            self.assertIn("none survived extraction", stderr.getvalue())
            self.assertNotIn("Traceback", stderr.getvalue())
            self.assertEqual(
                "failed", load_history(base / "h.jsonl").results[0]["status"]
            )

    def test_a_nonzero_runner_exit_fails_a_run_whose_artifact_looks_clean(
        self,
    ) -> None:
        # The runner's own exit code is the only signal for a failure that never
        # reached the artifact (a crashed browser, a missing server). An
        # otherwise-valid artifact beside a non-zero exit must not pass.
        with tempfile.TemporaryDirectory() as raw:
            base = Path(raw)

            def crashing(paths, output, base_url, cohort, refusals, firmware=None):
                successful_browser_runner()(
                    paths, output, base_url, cohort, refusals, firmware
                )
                return 3

            with self.assertRaisesRegex(SelectionError, r"exited with status 3"):
                self._run(base, crashing)

            retained = json.loads(
                (base / "evidence" / "release-01.json").read_text(encoding="utf-8")
            )
            self.assertEqual("failed", retained["status"])
            self.assertEqual(3, retained["browser_exit_code"])

    def test_the_corpus_gate_fails_on_a_collapsed_board_too(self) -> None:
        # `run-corpus` is the EXHAUSTIVE gate, and it has its own wiring of the
        # value check. Testing only the five-board gate left a one-line edit able
        # to record a collapsed board as `completed` over the whole corpus.
        with tempfile.TemporaryDirectory() as raw:
            base = Path(raw)
            corpus_root, corpus_manifest = manifest_pool(
                base, "corpus", 3, cohort="corpus"
            )

            def collapsing(paths, output, base_url, cohort, refusals, firmware=None):
                successful_browser_runner()(
                    paths, output, base_url, cohort, refusals, firmware
                )
                artifact = json.loads((output / "results.json").read_text())
                artifact["results"][1]["report"]["num_components"] = 0
                (output / "results.json").write_text(json.dumps(artifact))
                return 0

            with self.assertRaisesRegex(SelectionError, r"no bench-grade value"):
                release_gates.run_corpus_gate(
                    corpus_root=corpus_root,
                    corpus_manifest=corpus_manifest,
                    evidence_dir=base / "evidence",
                    scratch_root=base / "scratch",
                    run_id="corpus-value-01",
                    base_url="http://127.0.0.1:37651",
                    tool_commit="e" * 40,
                    runner=collapsing,
                )

            # And the graded document is retained, not lost with the exception.
            retained = json.loads(
                (base / "evidence" / "corpus-value-01.json").read_text(
                    encoding="utf-8"
                )
            )
            self.assertEqual("failed", retained["status"])
            self.assertEqual(
                1, len(retained["browser"]["value_summary"]["failed"])
            )

    def test_a_collapsed_board_fails_the_run_and_keeps_the_graded_evidence(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as raw:
            base = Path(raw)

            def collapsing(
                paths: list[Path],
                output: Path,
                base_url: str,
                cohort: str,
                refusals: list[Path],
                firmware: list[dict | None] | None = None,
            ) -> int:
                successful_browser_runner()(
                    paths, output, base_url, cohort, refusals, firmware
                )
                artifact = json.loads((output / "results.json").read_text())
                # Honest, exportable, and worthless: every check outside the
                # value contract still passes on this row.
                artifact["results"][0]["report"]["num_components"] = 0
                (output / "results.json").write_text(json.dumps(artifact))
                return 0

            with self.assertRaisesRegex(SelectionError, r"no bench-grade value"):
                self._run(base, collapsing)

            retained = json.loads(
                (base / "evidence" / "release-01.json").read_text()
            )
            self.assertEqual("failed", retained["status"])
            summary = retained["browser"]["value_summary"]
            self.assertEqual(1, len(summary["failed"]))
            self.assertEqual(4, len(summary["delivered"]))
            self.assertEqual(
                "failed", load_history(base / "history.jsonl").results[0]["status"]
            )

    def test_a_malformed_report_still_closes_the_reservation(self) -> None:
        # A report is untrusted input. A TypeError escaping the gate would leave a
        # reserved iteration with no terminal record, burning five unseen boards
        # and breaking the append-only invariant the ledger documents.
        with tempfile.TemporaryDirectory() as raw:
            base = Path(raw)

            def malformed(
                paths: list[Path],
                output: Path,
                base_url: str,
                cohort: str,
                refusals: list[Path],
                firmware: list[dict | None] | None = None,
            ) -> int:
                successful_browser_runner()(
                    paths, output, base_url, cohort, refusals, firmware
                )
                artifact = json.loads((output / "results.json").read_text())
                artifact["results"][0]["report"]["assumptions"] = 3
                artifact["results"][1]["report"]["notes"] = "not a list"
                artifact["results"][2]["report"]["bind"] = {
                    "critical_parts_bound": "1/2", "open_parts": 7
                }
                (output / "results.json").write_text(json.dumps(artifact))
                return 0

            # Either outcome is acceptable; what must NOT happen is a TypeError
            # escaping, which is why this asserts the invariant and not a grade.
            try:
                self._run(base, malformed)
            except SelectionError:
                pass

            history = load_history(base / "history.jsonl")
            self.assertEqual(1, len(history.iterations))
            self.assertEqual(1, len(history.results))
            self.assertTrue((base / "evidence" / "release-01.json").is_file())

    def test_an_ungradeable_artifact_is_recorded_rather_than_raised_raw(self) -> None:
        # The last line of defence: whatever the artifact does to the grader, the
        # reservation still gets its one terminal record and the operator gets a
        # sentence rather than a traceback.
        with tempfile.TemporaryDirectory() as raw:
            base = Path(raw)

            def exploding(
                paths: list[Path],
                output: Path,
                base_url: str,
                cohort: str,
                refusals: list[Path],
                firmware: list[dict | None] | None = None,
            ) -> int:
                successful_browser_runner()(
                    paths, output, base_url, cohort, refusals, firmware
                )
                return 0

            with patch.object(
                release_gates,
                "grade_board",
                side_effect=RuntimeError("grader blew up"),
            ):
                with self.assertRaisesRegex(SelectionError, r"could not be graded"):
                    self._run(base, exploding)

            history = load_history(base / "history.jsonl")
            self.assertEqual(1, len(history.results))
            self.assertEqual("failed", history.results[0]["status"])
            evidence = json.loads(
                (base / "evidence" / "release-01.json").read_text()
            )
            self.assertIn("RuntimeError", evidence["validation_error"])

    def test_a_broken_journey_keeps_the_grades_of_the_boards_beside_it(self) -> None:
        # Aborting on the first failed journey threw away the grades of every
        # board that completed, so a run with one broken journey retained no
        # unlocks for the honestly degraded boards next to it.
        with tempfile.TemporaryDirectory() as raw:
            base = Path(raw)

            def one_broken(
                paths: list[Path],
                output: Path,
                base_url: str,
                cohort: str,
                refusals: list[Path],
                firmware: list[dict | None] | None = None,
            ) -> int:
                successful_browser_runner()(
                    paths, output, base_url, cohort, refusals, firmware
                )
                artifact = json.loads((output / "results.json").read_text())
                artifact["results"][0]["failures"] = ["the drop target never armed"]
                report = artifact["results"][1]["report"]
                report["bind"] = {
                    "critical_parts_bound": "0/2",
                    "open_parts": [
                        {"reference": f"U{i}", "active_ic": True, "reason": "no model"}
                        for i in range(2)
                    ],
                }
                report["assumptions"] = [
                    {"kind": "open_part",
                     "replacement": f"Add a model for U{i} to your models directory."}
                    for i in range(2)
                ]
                (output / "results.json").write_text(json.dumps(artifact))
                # What the real runner does: frontend/tests/e2e/drag-drop-release.ts
                # throws on any failing row, so the process exits non-zero. A gate
                # that checked the exit code before grading retained nothing here.
                return 1

            with self.assertRaisesRegex(SelectionError, r"drop target never armed"):
                self._run(base, one_broken)

            retained = json.loads((base / "evidence" / "release-01.json").read_text())
            self.assertEqual("failed", retained["status"])
            summary = retained["browser"]["value_summary"]
            # The four boards that completed are graded, and the degraded one
            # still carries the upload that would unlock more.
            self.assertEqual(1, len(summary["degraded"]))
            self.assertEqual(
                [f"Add a model for U{i} to your models directory." for i in range(2)],
                summary["degraded"][0]["unlocks"],
            )
            self.assertEqual(3, len(summary["delivered"]))

    def test_a_degraded_board_passes_and_is_enumerated_with_its_unlock(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            base = Path(raw)

            def degrading(
                paths: list[Path],
                output: Path,
                base_url: str,
                cohort: str,
                refusals: list[Path],
                firmware: list[dict | None] | None = None,
            ) -> int:
                successful_browser_runner()(
                    paths, output, base_url, cohort, refusals, firmware
                )
                artifact = json.loads((output / "results.json").read_text())
                report = artifact["results"][0]["report"]
                report["bind"] = {
                    "critical_parts_bound": "0/6",
                    "open_parts": [
                        {
                            "reference": f"U{index}",
                            "active_ic": True,
                            "reason": "no model",
                        }
                        for index in range(6)
                    ],
                }
                report["assumptions"] = [
                    {
                        "kind": "open_part",
                        "replacement": f"Add a model for U{index} to your models "
                        "directory.",
                    }
                    for index in range(6)
                ]
                (output / "results.json").write_text(json.dumps(artifact))
                return 0

            stderr = io.StringIO()
            with redirect_stderr(stderr):
                evidence = self._run(base, degrading)

            self.assertEqual("completed", evidence["status"])
            summary = evidence["browser"]["value_summary"]
            self.assertEqual(1, len(summary["degraded"]))
            self.assertEqual(
                [
                    f"Add a model for U{index} to your models directory."
                    for index in range(6)
                ],
                summary["degraded"][0]["unlocks"],
            )
            self.assertIn("DEGRADED-HONEST", stderr.getvalue())

    def _firmware_corpus(self, base: Path) -> tuple[Path, Path]:
        corpus_root, corpus_manifest = manifest_pool(base, "corpus", 1, cohort="corpus")
        (corpus_root / "corpus-0" / "firmware.elf").write_bytes(b"\x7fELF")
        corpus_manifest.write_text(
            corpus_manifest.read_text().replace(
                'expect = ["board.kicad_pcb"]',
                'expect = ["board.kicad_pcb"]\nfirmware = "firmware.elf"',
                1,
            )
        )
        return corpus_root, corpus_manifest

    def test_firmware_that_changes_mid_run_is_caught_and_digested(self) -> None:
        # Hashing only the boards would let a paired image change during the run
        # with nothing in the ledger saying which bytes the co-sim exercised.
        with tempfile.TemporaryDirectory() as raw:
            base = Path(raw)
            corpus_root, corpus_manifest = self._firmware_corpus(base)

            def swapping(
                paths: list[Path],
                output: Path,
                base_url: str,
                cohort: str,
                refusals: list[Path],
                firmware: list[dict | None] | None = None,
            ) -> int:
                successful_browser_runner()(
                    paths, output, base_url, cohort, refusals, firmware
                )
                Path(firmware[0]["path"]).write_bytes(b"\x7fELF-DIFFERENT")
                return 0

            with self.assertRaisesRegex(SelectionError, r"staged input changed"):
                release_gates.run_corpus_gate(
                    corpus_root=corpus_root,
                    corpus_manifest=corpus_manifest,
                    evidence_dir=base / "evidence",
                    scratch_root=base / "scratch",
                    run_id="corpus-swap",
                    base_url="http://127.0.0.1:37651",
                    tool_commit="e" * 40,
                    runner=swapping,
                )

    def test_the_gate_hands_the_journey_the_firmware_it_staged(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            base = Path(raw)
            corpus_root, corpus_manifest = manifest_pool(
                base, "corpus", 1, cohort="corpus"
            )
            (corpus_root / "corpus-0" / "firmware.elf").write_bytes(b"\x7fELF")
            corpus_manifest.write_text(
                corpus_manifest.read_text().replace(
                    'expect = ["board.kicad_pcb"]',
                    'expect = ["board.kicad_pcb"]\nfirmware = "firmware.elf"',
                    1,
                )
            )
            handed: list[list[dict | None] | None] = []

            def capturing(
                paths: list[Path],
                output: Path,
                base_url: str,
                cohort: str,
                refusals: list[Path],
                firmware: list[dict | None] | None = None,
            ) -> int:
                handed.append(firmware)
                successful_browser_runner()(
                    paths, output, base_url, cohort, refusals, firmware
                )
                artifact = json.loads((output / "results.json").read_text())
                artifact["results"][0]["firmware"] = {
                    "staged": True,
                    "loaded": True,
                    "detail": "ELF, AVR",
                    "pin_activity": True,
                    "serial_activity": True,
                    "pin_activity_rendered": True,
                }
                artifact["results"][0]["report"]["cosim"] = {
                    "ran": True,
                    "seconds_simulated": 0.5,
                    "analog_valid": True,
                    "uart_output": "hello\n",
                    "gpio_nets": [{"name": "PB0", "driven": True}],
                }
                (output / "results.json").write_text(json.dumps(artifact))
                return 0

            evidence = release_gates.run_corpus_gate(
                corpus_root=corpus_root,
                corpus_manifest=corpus_manifest,
                evidence_dir=base / "evidence",
                scratch_root=base / "scratch",
                run_id="corpus-firmware",
                base_url="http://127.0.0.1:37651",
                tool_commit="e" * 40,
                runner=capturing,
            )

            self.assertEqual(1, len(handed))
            self.assertEqual("cosim", handed[0][0]["expect"])
            self.assertTrue(Path(handed[0][0]["path"]).is_file())
            self.assertEqual("completed", evidence["status"])
            self.assertEqual(
                "delivered", evidence["browser"]["results"][0]["value"]["grade"]
            )
            # The image's own digest sits beside the board it was paired with,
            # so the retained co-sim result names the bytes that produced it.
            self.assertEqual(
                hashlib.sha256(b"\x7fELF").hexdigest(),
                evidence["boards"][0]["firmware_staged_sha256"],
            )


if __name__ == "__main__":
    unittest.main()
