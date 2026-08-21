#!/usr/bin/env python3

import unittest

from ci_change_plan import classify


class CiChangePlanTests(unittest.TestCase):
    def test_docs_only_uses_only_cheap_always_on_gates(self):
        self.assertEqual(
            classify(["docs/START_HERE.md", "README.md"]),
            {"rust": False, "frontend": False, "vscode": False, "speed": False, "full": False},
        )

    def test_frontdoor_rust_runs_rust_and_browser_gates(self):
        plan = classify(["crates/hauksbee-engine/src/serve.rs"])
        self.assertTrue(plan["rust"])
        self.assertTrue(plan["frontend"])
        self.assertFalse(plan["vscode"])

    def test_solver_change_runs_the_claim_gate(self):
        plan = classify(["crates/hauksbee-solve/src/lib.rs"])
        self.assertTrue(plan["rust"])
        self.assertTrue(plan["speed"])

    def test_unknown_path_fails_conservative(self):
        plan = classify(["new-release-surface/config.xyz"])
        self.assertEqual(plan, {"rust": True, "frontend": True, "vscode": True, "speed": True, "full": False})

    def test_empty_or_unavailable_diff_fails_conservative(self):
        self.assertEqual(classify([]), {"rust": True, "frontend": True, "vscode": True, "speed": True, "full": False})

    def test_full_candidate_enables_every_gate(self):
        self.assertTrue(all(classify(["README.md"], full=True).values()))


if __name__ == "__main__":
    unittest.main()
