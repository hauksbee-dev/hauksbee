#!/usr/bin/env python3

import subprocess
import tempfile
import unittest
from contextlib import chdir
from pathlib import Path

from ci_change_plan import OUTPUTS, changed_paths, classify


class CiChangePlanTests(unittest.TestCase):
    def test_changed_paths_select_exact_gates(self):
        for paths, full, enabled in [
            (["docs/START_HERE.md", "README.md"], False, set()),
            (["crates/hauksbee-engine/src/serve.rs"], False, {"rust", "frontend"}),
            (["crates/hauksbee-solve/src/lib.rs"], False, {"rust", "speed"}),
            (["new-release-surface/config.xyz"], False, set(OUTPUTS) - {"full"}),
            ([], False, set(OUTPUTS) - {"full"}),
            (["README.md"], True, set(OUTPUTS)),
        ]:
            with self.subTest(paths=paths, full=full):
                self.assertEqual(classify(paths, full=full), {key: key in enabled for key in OUTPUTS})

    def test_deletion_and_move_to_docs_retain_source_gates(self):
        with tempfile.TemporaryDirectory() as raw, chdir(raw):
            def git(*args):
                return subprocess.check_output(["git", *args], text=True).strip()

            git("init", "-q")
            source = Path("crates/hauksbee-solve/src/lib.rs")
            source.parent.mkdir(parents=True)
            source.write_text("pub fn solve() {}\n")
            git("add", ".")
            base = git("write-tree")
            source.unlink()
            Path("README.md").write_text("after\n")
            for moved in (False, True):
                if moved:
                    Path("docs").mkdir()
                    Path("docs/retired.rs").write_text("pub fn solve() {}\n")
                git("add", "-A")
                with self.subTest(moved=moved):
                    paths = changed_paths(base, git("write-tree"))
                    self.assertIn(source.as_posix(), paths)
                    self.assertEqual({key for key, value in classify(paths).items() if value}, {"rust", "speed"})


if __name__ == "__main__":
    unittest.main()
