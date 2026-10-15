#!/usr/bin/env python3
"""Drift tests for the lists and facts derived by workspace_crates.py and check-release-facts.py."""
from __future__ import annotations

import importlib.util
import sys
import unittest
from pathlib import Path

SCRIPTS = Path(__file__).resolve().parent
sys.path.insert(0, str(SCRIPTS))
import workspace_crates as wc  # noqa: E402

spec = importlib.util.spec_from_file_location("check_release_facts", SCRIPTS / "check-release-facts.py")
facts = importlib.util.module_from_spec(spec)
spec.loader.exec_module(facts)


def pkg(name, deps=(), publish=True, shard="s"):
    return {"name": name, "dir": f"crates/{name}", "publish": publish, "deps": set(deps), "shard": shard}


class WorkspaceLists(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.members, cls.external = wc.load()

    def test_publish_order_lists_dependencies_first(self):
        order = [p["name"] for p in wc.publish_order(self.members, self.external)]
        for p in self.members + self.external:
            if p["publish"]:
                self.assertIn(p["name"], order)
                for dep in p["deps"]:
                    self.assertLess(order.index(dep), order.index(p["name"]), f'{dep} before {p["name"]}')

    def test_publish_order_covers_the_crates_the_old_lists_dropped(self):
        dirs = [p["dir"] for p in wc.publish_order(self.members, self.external)]
        for crate in ("frontdoor-api", "bind", "checks", "cosim"):
            self.assertIn(f"crates/hauksbee-{crate}", dirs)
        self.assertNotIn("crates/hauksbee-testkit", dirs)
        self.assertTrue(any(d.startswith("vendor/kicad-forge/") for d in dirs))

    def test_every_workspace_crate_is_in_exactly_one_test_shard(self):
        self.assertEqual(wc.problems(self.members), [])
        shards = wc.test_shards(self.members)
        listed = [n for names in shards.values() for n in names]
        self.assertEqual(sorted(listed), sorted(m["name"] for m in self.members))

    def test_a_crate_without_a_shard_is_reported(self):
        self.assertEqual(len(wc.problems([pkg("a"), pkg("b", shard=None)])), 1)

    def test_publish_order_rejects_a_cycle(self):
        with self.assertRaises(SystemExit):
            wc.publish_order([pkg("a", deps="b"), pkg("b", deps="a")], [])

    def test_ci_workflow_takes_its_test_matrix_from_the_plan_job(self):
        ci = (wc.REPO / ".github/workflows/ci.yml").read_text()
        self.assertIn("matrix: ${{ fromJSON(needs.plan.outputs.test_matrix) }}", ci)
        self.assertIn("scripts/workspace_crates.py test-matrix", ci)
        self.assertNotIn("packages: >-", ci)

    def test_package_include_gate_uses_the_derived_publish_set(self):
        source = (SCRIPTS / "check-package-includes.py").read_text()
        self.assertIn("workspace_crates.publish_order", source)
        self.assertNotIn('"crates/hauksbee-ir"', source)


class ReleaseFacts(unittest.TestCase):
    def test_repository_is_consistent(self):
        members, _ = wc.load()
        self.assertEqual(facts.problems(members), [])

    def test_a_stale_literal_is_reported(self):
        members, _ = wc.load()
        real = facts.read

        def skewed(rel):
            text = real(rel)
            return text.replace(facts.workspace_version(), "0.0.0") if rel == "frontend/package.json" else text

        facts.read = skewed
        try:
            bad = facts.problems(members)
        finally:
            facts.read = real
        self.assertTrue(any("frontend/package.json" in b for b in bad), bad)


if __name__ == "__main__":
    unittest.main()
