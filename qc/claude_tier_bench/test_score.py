#!/usr/bin/env python3
"""Offline tests for the tier bench: scoring, events, cache key, cases, dry run."""

import contextlib
import importlib.util
import io
import json
import tempfile
import tomllib
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("tier_bench_run", HERE / "run.py")
run = importlib.util.module_from_spec(spec)
spec.loader.exec_module(run)

REF = """
[[models]]
id = "ref"
kind = "vreg"
[models.params]
vout = 3.3
iq = 0.000100
label = "x"
[models.ratings]
max_voltage_v = 6.0
[models.pins]
1 = "in"
2 = "gnd"
3 = "out"
"""
GOOD = """
# extracted by claude-code claude-sonnet-5-5 (tier 2 of 3); escalated: validator
[[models]]
id = "draft"
kind = "vreg"
[models.params]
vout = 3.3
iq = 0.00012
[models.ratings]
max_voltage_v = 6.6
[models.pins]
1 = "IN"
2 = "GND"
3 = "OUT"
"""
SWAPPED = GOOD.replace('2 = "GND"', '2 = "OUT"').replace('3 = "OUT"', '3 = "GND"')
EVENTS = [
    {"t": 0, "kind": "tier", "model": "claude-haiku-5-5", "tier": 1, "tiers": 2},
    {"t": 5, "kind": "usage", "billing": "metered", "cost_usd": 0.10, "total_tokens": 1000, "total_cost_usd": 0.10},
    {"t": 9, "kind": "escalate", "from": "claude-haiku-5-5", "to": "claude-sonnet-5-5", "reason": "validator"},
    {"t": 20, "kind": "usage", "billing": "metered", "cost_usd": 0.25, "total_tokens": 4000, "total_cost_usd": 0.35},
    {"t": 21, "kind": "final", "model": "claude-sonnet-5-5", "tier": 2, "tiers": 2,
     "escalations": ["validator"], "confidence": "high"},
]


class ScoreTests(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        db = self.tmp / "db"
        db.mkdir()
        (db / "ref.toml").write_text(REF)
        self.old_db = run.DB_DIR
        run.DB_DIR = db
        self.case = {"part": "P", "kind": "vreg", "db": "ref.toml", "card": "ref"}
        self.nobin = self.tmp / "no-such-hauksbee"

    def tearDown(self):
        run.DB_DIR = self.old_db

    def sample(self, name, text, rejected=False, events=True):
        sdir = self.tmp / name
        out = sdir / "out" / ("rejected" if rejected else "")
        out.mkdir(parents=True)
        (out / "P.toml").write_text(text)
        if events:
            (sdir / "events.jsonl").write_text("\n".join(json.dumps(e) for e in EVENTS) + "\n")
        return sdir

    def test_pin_score(self):
        ref = {"1": "in", "2": "gnd", "3": "out"}
        self.assertEqual(run.pin_score(ref, {"1": "IN", "2": "Gnd", "3": "out"}), (True, 1.0))
        exact, frac = run.pin_score(ref, {"1": "in", "2": "out", "3": "gnd"})
        self.assertFalse(exact)
        self.assertAlmostEqual(frac, 1 / 3)
        self.assertFalse(run.pin_score(ref, {**ref, "4": "nc"})[0])  # extra pad
        # The binder reads vee and v_minus as the vss rail; so does the score.
        rail = {"1": "out", "2": "vss"}
        self.assertEqual(run.pin_score(rail, {"1": "out", "2": "V-"}), (False, 0.5))
        self.assertEqual(run.pin_score(rail, {"1": "out", "2": "vee"}), (True, 1.0))
        self.assertEqual(run.pin_score(rail, {"1": "out", "2": "v_minus"}), (True, 1.0))
        self.assertEqual(run.pin_score({}, ref), (False, 0.0))
        # A lone channel index is a naming choice; a real second channel is not.
        one = {"1": "vcca", "2": "gnd", "3": "a1", "4": "b1", "5": "dir", "6": "vccb"}
        self.assertEqual(run.pin_score(one, {**one, "3": "a", "4": "b"}), (True, 1.0))
        two = {"1": "a1", "2": "a2", "3": "gnd"}
        self.assertEqual(run.pin_score(two, {"1": "a", "2": "a2", "3": "gnd"})[1], 2 / 3)
        self.assertEqual(run.pin_score(two, {"1": "a2", "2": "a1", "3": "gnd"})[1], 1 / 3)

    def test_pads_are_renamed_through_the_drafts_own_pinout(self):
        ref = {"pins": {"G": "gate", "S": "source", "D": "drain"}}
        draft = {"pins": {"1": "gate", "2": "source", "3": "drain"},
                 "source": {"pinout": [{"package": "SOT23", "pins": {"1": "G", "2": "S", "3": "D"}}]}}
        self.assertEqual(run.pin_score(ref["pins"], run.comparable_pins(ref, draft)), (True, 1.0))
        swapped = {**draft, "pins": {"1": "source", "2": "gate", "3": "drain"}}
        self.assertFalse(run.pin_score(ref["pins"], run.comparable_pins(ref, swapped))[0])
        # Keys already comparable, or no pinout to rename through: left as drafted.
        self.assertEqual(run.comparable_pins({"pins": {"1": "gate"}}, draft), draft["pins"])
        self.assertEqual(run.comparable_pins(ref, {"pins": draft["pins"]}), draft["pins"])

    def test_param_score(self):
        ref, draft = tomllib.loads(REF)["models"][0], tomllib.loads(GOOD)["models"][0]
        p = run.param_score(ref, draft)
        self.assertEqual((p["n_ref"], p["n_common"]), (3, 3))  # label is not numeric
        self.assertAlmostEqual(p["errors"]["params.iq"], 0.2)
        self.assertAlmostEqual(p["errors"]["ratings.max_voltage_v"], 0.1)
        self.assertAlmostEqual(p["median_rel_err"], 0.1)
        self.assertAlmostEqual(p["frac_within_10"], 2 / 3)
        self.assertEqual(p["coverage"], 1.0)
        none = run.param_score(ref, {})
        self.assertEqual((none["coverage"], none["median_rel_err"]), (0.0, None))

    def test_events(self):
        sdir = self.sample("ev", GOOD)
        e = run.read_events(sdir / "events.jsonl")
        self.assertEqual((e["final_model"], e["final_tier"], e["confidence"]), ("claude-sonnet-5-5", 2, "high"))
        self.assertEqual((e["escalations"], e["escalation_reasons"]), (1, ["validator"]))
        self.assertAlmostEqual(e["cost_usd"], 0.35)
        self.assertEqual(e["total_tokens"], 4000)
        self.assertIsNone(run.read_events(self.tmp / "missing.jsonl")["cost_usd"])

    def test_score_sample(self):
        r = run.score_sample(self.case, self.sample("ok", GOOD), self.nobin)
        self.assertTrue(r["pin_exact"] and r["validator_pass"] and not r["rejected"])
        self.assertEqual(r["final_model"], "claude-sonnet-5-5")
        bad = run.score_sample(self.case, self.sample("sw", SWAPPED), self.nobin)
        self.assertFalse(bad["pin_exact"])
        rej = run.score_sample(self.case, self.sample("rej", GOOD, rejected=True), self.nobin)
        self.assertTrue(rej["rejected"] and not rej["validator_pass"])
        self.assertTrue(rej["pin_exact"])  # a rejected draft is still scored
        empty = self.tmp / "empty"
        empty.mkdir()
        none = run.score_sample(self.case, empty, self.nobin)
        self.assertFalse(none["validator_pass"] or none["pin_exact"])
        # provenance comment fills in the tier when there are no events
        nev = run.score_sample(self.case, self.sample("nev", GOOD, events=False), self.nobin)
        self.assertEqual((nev["final_model"], nev["final_tier"], nev["tiers"]), ("claude-sonnet-5-5", 2, 3))

    def test_recommend(self):
        def agg(n, pins, valid, cost):
            return {"n": n, "pin_exact": pins, "validator_pass": valid, "mean_cost_usd": cost}
        aggs = {"haiku-single": agg(12, 8, 10, 0.1), "sonnet": agg(12, 11, 12, 0.5), "opus": agg(12, 12, 12, 2.0)}
        self.assertEqual(run.recommend(aggs, 1), "sonnet")
        self.assertEqual(run.recommend(aggs, 0), "opus")
        self.assertEqual(run.recommend(aggs, 4), "haiku-single")
        self.assertIsNone(run.recommend({"opus": agg(0, 0, 0, None)}, 1))

    def test_plan_tiers_rank_on_one_measurable_basis(self):
        def plan(week_total, n, tokens):
            return {"n": n, "pin_exact": n, "validator_pass": n, "mean_cost_usd": None,
                    "week_total": week_total, "mean_week_delta": week_total / n, "mean_tokens": tokens}
        # One reading rounded to +0% must not make Opus look free next to Sonnet.
        coarse = {"sonnet": plan(1, 2, 800_000), "opus": plan(0, 1, 2_300_000)}
        self.assertEqual(run.cost_basis(coarse), "mean_tokens")
        self.assertEqual(run.recommend(coarse, 1), "sonnet")
        sized = {"haiku-harnessed": plan(6, 12, 1_000_000), "sonnet": plan(12, 12, 800_000)}
        self.assertEqual(run.cost_basis(sized), "mean_week_delta")
        self.assertEqual(run.recommend(sized, 1), "haiku-harnessed")

    def test_luna_baseline_gates_the_default_and_the_lowest_tier(self):
        def res(tier, part, pins, valid):
            return {"tier": tier, "part": part, "repeat": 1, "pin_exact": pins, "validator_pass": valid}
        parts = ["A", "B", "C"]
        results = [res("luna", p, True, True) for p in parts]
        results += [res("haiku-harnessed", p, p != "C", True) for p in parts]
        results += [res("haiku-sonnet", p, True, True) for p in parts]
        v = run.versus(results, "haiku-harnessed")
        self.assertEqual((v["n"], v["pin_exact"], v["base_pin_exact"], v["below"]), (3, 2, 3, True))
        self.assertFalse(run.versus(results, "haiku-sonnet")["below"])
        aggs = {t: run.aggregate([r for r in results if r["tier"] == t]) for t in ("luna", "haiku-harnessed", "haiku-sonnet")}
        aggs["haiku-harnessed"]["mean_tokens"], aggs["haiku-sonnet"]["mean_tokens"] = 10, 20
        # Below Luna is never the default, however cheap; Luna itself is not a Claude default.
        self.assertEqual(run.recommend(aggs, 5, results), "haiku-sonnet")
        self.assertIn("offer it only as haiku-sonnet", run.lowest_verdict(results))
        worse = [r if r["tier"] != "haiku-sonnet" else {**r, "validator_pass": r["part"] != "A"} for r in results]
        self.assertIn("drop claude-code-haiku", run.lowest_verdict(worse))
        self.assertIn("PROVISIONAL", run.tables([], {}, None, 1))

    def test_plan_runs_show_weekly_share_never_dollars(self):
        ev = self.tmp / "plan.jsonl"
        ev.write_text("\n".join(json.dumps(e) for e in [
            {"kind": "usage", "billing": "plan", "cost_usd": None, "total_tokens": 900, "total_cost_usd": 3.0},
            {"kind": "plan", "available": True, "before": {"week": 46, "session": 4}, "after": {"week": 47, "session": 6}},
        ]))
        e = run.read_events(ev)
        self.assertEqual((e["billing"], e["cost_usd"], e["week_delta"], e["session_delta"]), ("plan", None, 1, 2))
        self.assertEqual(run.spend(e), "week 46->47%")
        ev.write_text(json.dumps({"kind": "plan", "available": False}) + "\n" + json.dumps({"kind": "usage", "billing": "plan"}))
        self.assertEqual(run.spend(run.read_events(ev)), "usage unavailable")
        self.assertEqual(run.spend({"billing": None}), "billing unknown")


class CacheAndDownloadTests(unittest.TestCase):
    def test_cache_key(self):
        env = dict(run.TIERS["ladder"], HAUKSBEE_CLAUDE_EFFORT="high")
        k = run.cache_key(b"pdf", "P", "vreg", env, "v1", "pv")
        self.assertEqual(k, run.cache_key(b"pdf", "P", "vreg", dict(reversed(list(env.items()))), "v1", "pv"))
        for changed in (
            run.cache_key(b"pdf2", "P", "vreg", env, "v1", "pv"),
            run.cache_key(b"pdf", "Q", "vreg", env, "v1", "pv"),
            run.cache_key(b"pdf", "P", "opamp", env, "v1", "pv"),
            run.cache_key(b"pdf", "P", "vreg", run.TIERS["opus"], "v1", "pv"),
            run.cache_key(b"pdf", "P", "vreg", env, "v2", "pv"),
            run.cache_key(b"pdf", "P", "vreg", env, "v1", "pv2"),
        ):
            self.assertNotEqual(k, changed)
        self.assertEqual(run.prompt_version(), run.prompt_version())

    def test_download_file_url_and_pdf_check(self):
        tmp = Path(tempfile.mkdtemp())
        src, notpdf = tmp / "src.pdf", tmp / "bad.pdf"
        src.write_bytes(b"%PDF-1.4 fixture")
        notpdf.write_bytes(b"<html>")
        dest = run.download(src.as_uri(), tmp / "cache" / "a.pdf")
        self.assertEqual(dest.read_bytes(), b"%PDF-1.4 fixture")
        self.assertEqual(run.download("file:///nonexistent", dest), dest)  # cached: no fetch
        with self.assertRaises(RuntimeError):
            run.download(notpdf.as_uri(), tmp / "cache" / "b.pdf")
        self.assertFalse((tmp / "cache" / "b.pdf").exists())


FAKE_BIN = """#!/bin/sh
# Stand-in for hauksbee: records an invocation count, writes a card and events.
[ "$1" = "--version" ] && { echo "fake 1.0"; exit 0; }
[ "$2" = "lint" ] && exit 0
echo x >> "$(dirname "$HAUKSBEE_EXTRACT_EVENTS")/../../../../calls"
while [ $# -gt 0 ]; do [ "$1" = "--out-dir" ] && out="$2"; shift; done
mkdir -p "$out"
cat > "$out/p.toml" <<'EOF'
# extracted by claude-code claude-haiku-5-5 (tier 1 of 1)
[[models]]
id = "p"
kind = "comparator"
[models.pins]
1 = "out"
2 = "vss"
EOF
echo '{"t":1,"kind":"usage","billing":"metered","cost_usd":0.5,"total_tokens":10}' > "$HAUKSBEE_EXTRACT_EVENTS"
"""


class EndToEndWithFakeBinaryTests(unittest.TestCase):
    def test_run_cache_and_summary(self):
        tmp = Path(tempfile.mkdtemp())
        binary = tmp / "fake-hauksbee"
        binary.write_text(FAKE_BIN)
        binary.chmod(0o755)
        (tmp / "ds").mkdir()
        (tmp / "ds" / "TLV3601IDBVR.pdf").write_bytes(b"%PDF-1.4 fixture")
        argv = ["--yes", "--case", "TLV3601IDBVR", "--tiers", "sonnet,opus", "--bin", str(binary),
                "--runs", str(tmp / "runs"), "--datasheets", str(tmp / "ds")]
        for expected_calls in (2, 2, 4):  # second pass is cached; --force reruns both tiers
            with contextlib.redirect_stdout(io.StringIO()) as buf, contextlib.redirect_stderr(io.StringIO()):
                run.main(argv + (["--force"] if expected_calls == 4 else []))
            self.assertEqual(len((tmp / "calls").read_text().split()), expected_calls)
        summary = json.loads((tmp / "runs" / "summary.json").read_text())
        self.assertEqual(summary["tiers"]["sonnet"]["n"], 1)
        self.assertAlmostEqual(summary["tiers"]["opus"]["mean_cost_usd"], 0.5)
        self.assertIn(summary["recommended"], ("sonnet", "opus"))
        self.assertIn("Recommended default:", buf.getvalue())
        with self.assertRaises(SystemExit):  # a real run without --yes is refused
            with contextlib.redirect_stderr(io.StringIO()):
                run.main(["--case", "TLV3601IDBVR", "--bin", str(binary)])


class CasesTests(unittest.TestCase):
    def test_cases_match_reviewed_cards(self):
        cases = run.load_cases()
        self.assertGreaterEqual(len(cases), 12)
        for c in cases:
            text = (run.DB_DIR / c["db"]).read_text(encoding="utf-8")
            cards = [m for m in tomllib.loads(text)["models"] if m["id"] == c["card"]]
            self.assertEqual(len(cards), 1, c["part"])
            self.assertEqual(cards[0]["kind"], c["kind"], c["part"])
            self.assertTrue(cards[0].get("pins"), c["part"])
            self.assertIn(c["url"].split("://", 1)[1], text, f"{c['part']} url not in {c['db']}")

    def test_dry_run_prints_commands_without_running(self):
        buf = io.StringIO()
        with contextlib.redirect_stdout(buf):
            code = run.main(["--dry-run", "--case", "TLV3601IDBVR", "--tiers", "haiku-harnessed,ladder,luna-ladder",
                             "--runs", str(Path(tempfile.mkdtemp())), "--datasheets", str(Path(tempfile.mkdtemp()))])
        out = buf.getvalue()
        self.assertEqual(code, 0)
        self.assertEqual(out.count("models extract --pdf"), 3)
        for needle in ("HAUKSBEE_EXTRACT_HARNESS\": \"staged", "HAUKSBEE_CLAUDE_ESCALATE\": \"claude-sonnet-5-5,claude-opus-5-5",
                       "HAUKSBEE_CODEX_MODEL\": \"gpt-6-luna", "HAUKSBEE_EXTRACT_EVENTS", "HAUKSBEE_EXTRACT_CONFIG",
                       "--kind comparator", "--force", "would download https://www.ti.com/lit/ds/symlink/tlv3601.pdf"):
            self.assertIn(needle, out)
        self.assertEqual(out.count("HAUKSBEE_CLAUDE_EFFORT\": \"high"), 3)  # effort rides on every tier env
        self.assertNotIn("luna-ladder", run.DEFAULT_TIERS)


if __name__ == "__main__":
    unittest.main()
