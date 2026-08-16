#!/usr/bin/env python3
"""Focused tests for the machine-readable benchmark path."""

from __future__ import annotations

import json
import struct
import tempfile
import unittest
from pathlib import Path

import run


class BenchmarkHelpersTest(unittest.TestCase):
    def test_spice_suffix_and_shared_grid(self) -> None:
        self.assertAlmostEqual(run.parse_spice_number("20u"), 20e-6)
        self.assertAlmostEqual(run.parse_spice_number("1meg"), 1e6)
        self.assertEqual(len(run.shared_times(20e-6, 100e-6)), 6)
        for got, expected in zip(run.shared_times(20e-6, 100e-6), [0, 20e-6, 40e-6, 60e-6, 80e-6, 100e-6]):
            self.assertAlmostEqual(got, expected)

    def test_binary_rawfile_decoder_is_machine_data_only(self) -> None:
        header = (
            "Title: test\nNo. Variables: 2\nNo. Points: 3\nVariables:\n"
            "\t0\ttime\ttime\n\t1\tv(out)\tvoltage\nBinary:\n"
        ).encode("ascii")
        payload = struct.pack("<6d", 0.0, 1.0, 1e-6, 2.0, 2e-6, 3.0)
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "fixture.raw"
            path.write_bytes(header + payload)
            columns, metadata = run.parse_ngspice_raw(path)
        self.assertEqual(columns["time"], [0.0, 1e-6, 2e-6])
        self.assertEqual(columns["v(out)"], [1.0, 2.0, 3.0])
        self.assertEqual(metadata["endianness"], "little")

    def test_error_metrics_and_mutation_are_sensitive(self) -> None:
        grid = [0.0, 1.0, 2.0, 3.0]
        reference = [0.0, 1.0, 2.0, 3.0]
        same = run.error_metrics(reference, reference, grid, 0.5)
        changed = run.error_metrics([0.0, 1.0, 2.5, 3.0], reference, grid, 0.5)
        self.assertEqual(same["max_abs"], 0.0)
        self.assertGreater(changed["max_abs"], same["max_abs"])
        self.assertGreater(changed["p95_abs"], 0.0)

    def test_timing_is_raw_and_not_pseudo_corrected(self) -> None:
        stats = run.timing_stats([0.01, 0.02, 0.03])
        self.assertEqual(stats["raw_median_s"], 0.02)
        self.assertNotIn("startup_corrected_median_s", stats)
        speed = run.paired_speedup_summary([1.0, 1.0, 1.0], [2.0, 2.5, 3.0])
        self.assertEqual(speed["median"], 2.5)
        self.assertEqual(speed["classification"], "hauksbee_faster_across_interdecile_range")

    def test_manifest_is_source_bound_and_threshold_free(self) -> None:
        manifest = json.loads(run.MANIFEST.read_text())
        self.assertEqual(len(manifest["cases"]), 7)
        self.assertTrue(all(Path(run.ROOT / case["source"]).is_file() for case in manifest["cases"]))
        self.assertTrue(all(run.sha256_file(run.ROOT / case["source"]) == case["source_sha256"] for case in manifest["cases"]))
        self.assertTrue(all(run.sha256_file(run.ROOT / dep["path"]) == dep["sha256"] for case in manifest["cases"] for dep in case.get("dependencies", [])))
        self.assertEqual({case["class"] for case in manifest["cases"]}, {"agreement", "disclosed_drift"})
        self.assertNotIn("threshold", manifest)


if __name__ == "__main__":
    unittest.main()
