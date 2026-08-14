from pathlib import Path
import tempfile
import unittest

from qc import runner


class TranscriptRedactionTests(unittest.TestCase):
    def test_external_release_binary_paths_are_machine_independent(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            bin_dir = root / "extracted" / "bin"
            work = root / "work"
            session = runner.Session(bin_dir, work, runner.QC_DIR / "scenarios" / "fixture")

            rendered = session.redact(
                f"{bin_dir}/hauksbee run {work}/board.kicad_pcb {runner.REPO}/models"
            )

        self.assertEqual(
            rendered,
            "<EXTERNAL-BIN-DIR>/hauksbee run <WORK>/board.kicad_pcb <REPO>/models",
        )

    def test_repository_binary_paths_keep_their_stable_repo_prefix(self) -> None:
        bin_dir = runner.REPO / "target" / "release"
        session = runner.Session(
            bin_dir,
            runner.REPO / "qc" / "results" / "work",
            runner.QC_DIR / "scenarios" / "fixture",
        )

        self.assertEqual(
            session.redact(f"{bin_dir}/hauksbee"),
            "<REPO>/target/release/hauksbee",
        )


if __name__ == "__main__":
    unittest.main()
