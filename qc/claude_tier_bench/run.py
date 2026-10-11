#!/usr/bin/env python3
"""Benchmark Hauksbee's Claude extraction tiers against reviewed db cards.

Each sample is one `hauksbee models extract` run billed to the user's Claude plan
(or the codex sign-in for the luna baseline and luna-ladder). `--dry-run` prints every command and spends nothing.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shutil
import statistics
import subprocess
import sys
import time
import tomllib
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from typing import Any
from urllib.request import Request, urlopen

BENCH_DIR = Path(__file__).resolve().parent
REPO_ROOT = BENCH_DIR.parents[1]
DB_DIR = REPO_ROOT / "crates" / "hauksbee-models" / "db"
PROMPT_FILES = ("datasheet.rs", "extract_config.rs", "extract_harness.rs")
HAIKU, SONNET, OPUS = "claude-haiku-5-5", "claude-sonnet-5-5", "claude-opus-5-5"


def _claude(model: str, escalate: str, harness: str) -> dict[str, str]:
    return {
        "HAUKSBEE_EXTRACT_BACKEND": "claude-code",
        "HAUKSBEE_CLAUDE_MODEL": model,
        "HAUKSBEE_CLAUDE_ESCALATE": escalate,
        "HAUKSBEE_EXTRACT_HARNESS": harness,
    }


TIERS: dict[str, dict[str, str]] = {
    # The baseline every Claude tier is held to: codex gpt-6-luna alone.
    "luna": {
        "HAUKSBEE_EXTRACT_BACKEND": "codex",
        "HAUKSBEE_CODEX_MODEL": "gpt-6-luna",
        "HAUKSBEE_CODEX_ESCALATE": "none",
    },
    "haiku-harnessed": _claude(HAIKU, "none", "staged"),
    # The `claude-code-haiku` preset: the lowest tier, escalating on failure.
    "haiku-sonnet": _claude(HAIKU, SONNET, "auto"),
    "haiku-single": _claude(HAIKU, "none", "single"),
    "sonnet": _claude(SONNET, "none", "single"),
    "opus": _claude(OPUS, "none", "single"),
    "ladder": _claude(HAIKU, f"{SONNET},{OPUS}", "auto"),
    "luna-ladder": {
        "HAUKSBEE_EXTRACT_BACKEND": "codex",
        "HAUKSBEE_CODEX_MODEL": "gpt-6-luna",
        "HAUKSBEE_CODEX_ESCALATE": "gpt-6.1-sol",
    },
}
DEFAULT_TIERS = [t for t in TIERS if t not in ("luna-ladder", "haiku-single")]
BASELINE = "luna"
LOWEST, LOWEST_PRESET = "haiku-harnessed", "haiku-sonnet"
# Inherited settings that would leak into a run; all are stripped before the tier env.
LEAKY_ENV = re.compile(r"^HAUKSBEE_(CLAUDE|CODEX|AGY|EXTRACT|LLM|API_KEY)")


# ---------------------------------------------------------------- cases, datasheets


def load_cases() -> list[dict[str, Any]]:
    doc = tomllib.loads((BENCH_DIR / "cases.toml").read_text(encoding="utf-8"))
    cases = doc.get("cases") or []
    for c in cases:
        for f in ("part", "url", "kind", "db", "card", "why"):
            if not isinstance(c.get(f), str) or not c[f]:
                raise SystemExit(f"error: case {c.get('part')} needs {f}")
    if len({c["part"] for c in cases}) != len(cases):
        raise SystemExit("error: duplicate part in cases.toml")
    return cases


def datasheet_path(case: dict[str, Any], root: Path) -> Path:
    return root / (re.sub(r"[^A-Za-z0-9._-]", "_", case["part"]) + ".pdf")


def download(url: str, dest: Path) -> Path:
    """Fetch `url` (http, https or file) to `dest` unless cached; require a PDF."""
    if dest.exists():
        return dest
    dest.parent.mkdir(parents=True, exist_ok=True)
    part = dest.with_suffix(".pdf.download")
    req = Request(url, headers={"User-Agent": "hauksbee-claude-tier-bench/1.0"})
    with urlopen(req, timeout=90) as resp, part.open("wb") as out:  # noqa: S310
        shutil.copyfileobj(resp, out)
    if part.read_bytes()[:5] != b"%PDF-":
        part.unlink()
        raise RuntimeError(f"{url} did not return a PDF")
    part.replace(dest)
    return dest


# ---------------------------------------------------------------- cache key


def prompt_version(root: Path = REPO_ROOT) -> str:
    h = hashlib.sha256()
    for name in PROMPT_FILES:
        f = root / "crates" / "hauksbee-models" / "src" / name
        if f.is_file():
            h.update(name.encode() + b"\0" + f.read_bytes())
    return h.hexdigest()[:16]


def binary_version(binary: Path) -> str:
    try:
        r = subprocess.run([str(binary), "--version"], capture_output=True, text=True, timeout=60)
        return (r.stdout or r.stderr).strip() or "unknown"
    except (OSError, subprocess.TimeoutExpired):
        return "unknown"


def cache_key(pdf: bytes, part: str, kind: str, tier_env: dict[str, str], version: str, prompt: str) -> str:
    h = hashlib.sha256()
    for piece in (pdf, part.encode(), kind.encode(), json.dumps(tier_env, sort_keys=True).encode(),
                  version.encode(), prompt.encode()):
        h.update(hashlib.sha256(piece).digest())
    return h.hexdigest()


# ---------------------------------------------------------------- scoring


def is_num(v: Any) -> bool:
    return isinstance(v, (int, float)) and not isinstance(v, bool)


def models_of(path: Path) -> list[dict[str, Any]]:
    return [m for m in tomllib.loads(path.read_text(encoding="utf-8")).get("models", []) if isinstance(m, dict)]


def _role(v: Any) -> str:
    return re.sub(r"[^a-z0-9]", "", str(v).lower())


def _unindexed(roles: dict[str, str]) -> dict[str, str]:
    """Drops a trailing "1" from a role whose stem has no other numbered sibling in
    the same map, so a single-channel `a1` and `a` agree while `a1`/`a2` stay apart."""
    stems = [m.group(1) for v in roles.values() if (m := re.fullmatch(r"(.*\D)\d+", v))]

    def bare(v: str) -> str:
        m = re.fullmatch(r"(.*\D)1", v)
        lone = m and stems.count(m.group(1)) == 1 and m.group(1) not in roles.values()
        return m.group(1) if lone else v

    return {k: bare(v) for k, v in roles.items()}


def pin_score(ref: dict[str, Any], draft: dict[str, Any]) -> tuple[bool, float]:
    """(exact, fraction of reference pads whose role matches). Pads and roles are
    compared case- and punctuation-insensitively, and a lone channel index is
    ignored (`a1` == `a`); extra draft pads break exactness."""
    r = _unindexed({str(k).strip().upper(): _role(v) for k, v in ref.items()})
    d = _unindexed({str(k).strip().upper(): _role(v) for k, v in draft.items()})
    if not r:
        return False, 0.0
    hit = sum(1 for k, v in r.items() if d.get(k) == v)
    return hit == len(r) == len(d), hit / len(r)


def param_score(ref: dict[str, Any], draft: dict[str, Any]) -> dict[str, Any]:
    """Relative error on numeric `params` and `ratings` keys present in both."""
    errs: dict[str, float] = {}
    n_ref = 0
    for section in ("params", "ratings"):
        rs, ds = ref.get(section) or {}, draft.get(section) or {}
        for k, rv in rs.items():
            if not is_num(rv):
                continue
            n_ref += 1
            dv = ds.get(k)
            if is_num(dv):
                errs[f"{section}.{k}"] = abs(dv - rv) / abs(rv) if rv else float(dv != 0)
    vals = list(errs.values())
    near = sum(1 for e in vals if e <= 0.10)
    return {
        "n_ref": n_ref,
        "n_common": len(vals),
        "coverage": len(vals) / n_ref if n_ref else None,
        "median_rel_err": statistics.median(vals) if vals else None,
        "frac_within_10": near / len(vals) if vals else None,
        "within_10_of_ref": near / n_ref if n_ref else None,
        "errors": errs,
    }


def best_draft_model(ref: dict[str, Any], drafts: list[dict[str, Any]]) -> dict[str, Any] | None:
    """The draft entry closest to the reference (several entries are allowed)."""
    return max(drafts, key=lambda m: pin_score(ref.get("pins") or {}, m.get("pins") or {})[1],
               default=None)


def read_events(path: Path) -> dict[str, Any]:
    ev: list[dict[str, Any]] = []
    if path.is_file():
        for line in path.read_text(encoding="utf-8", errors="replace").splitlines():
            try:
                obj = json.loads(line)
            except json.JSONDecodeError:
                continue
            if isinstance(obj, dict):
                ev.append(obj)
    usage = [e for e in ev if e.get("kind") == "usage"]
    final = next((e for e in reversed(ev) if e.get("kind") == "final"), {})
    costs = [e["cost_usd"] for e in usage if is_num(e.get("cost_usd"))]
    totals = [e["total_cost_usd"] for e in usage if is_num(e.get("total_cost_usd"))]
    toks = [e["total_tokens"] for e in usage if is_num(e.get("total_tokens"))]
    reasons = [e.get("reason") for e in ev if e.get("kind") == "escalate"]
    billing = next((e.get("billing") for e in reversed(usage) if e.get("billing")), None)
    plan = next((e for e in reversed(ev) if e.get("kind") == "plan"), None)
    def window(w: str) -> tuple[Any, Any, Any]:
        b, a = ((plan or {}).get(k) or {} for k in ("before", "after"))
        d = a.get(w) - b.get(w) if is_num(a.get(w)) and is_num(b.get(w)) else None
        return b.get(w), a.get(w), d
    week, session = window("week"), window("session")
    return {
        "billing": billing,
        # Under a plan the run is its share of the plan's limits, never dollars.
        "plan_available": None if plan is None else bool(plan.get("available")),
        "week_before": week[0], "week_after": week[1], "week_delta": week[2],
        "session_delta": session[2],
        "events": len(ev),
        "final_model": final.get("model"),
        "final_tier": final.get("tier"),
        "tiers": final.get("tiers"),
        "confidence": final.get("confidence"),
        "escalations": len(reasons),
        "escalation_reasons": reasons or list(final.get("escalations") or []),
        "total_tokens": toks[-1] if toks else (sum(
            (e.get("input") or 0) + (e.get("output") or 0) for e in usage) if usage else None),
        "cost_usd": (sum(costs) if costs else (totals[-1] if totals else None)) if billing == "metered" else None,
    }


def find_draft(out: Path) -> tuple[Path | None, bool]:
    """(draft path, rejected). An accepted card wins over a rejected one."""
    for folder, rejected in ((out, False), (out / "rejected", True)):
        found = sorted(folder.glob("*.toml")) if folder.is_dir() else []
        if found:
            return found[0], rejected
    return None, False


def score_sample(case: dict[str, Any], sdir: Path, binary: Path) -> dict[str, Any]:
    ref_models = [m for m in models_of(DB_DIR / case["db"]) if m.get("id") == case["card"]]
    ref = ref_models[0] if ref_models else {}
    draft_path, rejected = find_draft(sdir / "out")
    res: dict[str, Any] = {"draft": str(draft_path.relative_to(sdir)) if draft_path else None,
                           "rejected": rejected}
    draft: dict[str, Any] | None = None
    if draft_path:
        try:
            draft = best_draft_model(ref, models_of(draft_path))
        except (OSError, tomllib.TOMLDecodeError) as exc:
            res["parse_error"] = str(exc)
    exact, frac = pin_score(ref.get("pins") or {}, (draft or {}).get("pins") or {})
    res.update(pin_exact=exact, pin_frac=frac, params=param_score(ref, draft or {}))
    lint = None
    if draft_path and not rejected and "parse_error" not in res and binary.is_file():
        try:
            lint = subprocess.run([str(binary), "models", "lint", str(draft_path)],
                                  capture_output=True, text=True, timeout=120).returncode == 0
        except (OSError, subprocess.TimeoutExpired):
            lint = False
    res["lint_ok"] = lint
    res["validator_pass"] = bool(draft_path and not rejected and lint is not False and "parse_error" not in res)
    res.update(read_events(sdir / "events.jsonl"))
    if res["final_model"] is None and draft_path:  # fall back to the provenance comment
        m = re.search(r"^# extracted by \S+ (\S+) \(tier (\d+) of (\d+)\)", draft_path.read_text(
            encoding="utf-8", errors="replace"), re.M)
        if m:
            res.update(final_model=m[1], final_tier=int(m[2]), tiers=int(m[3]))
    return res


# ---------------------------------------------------------------- running


def sample_env(tier: str, effort: str, sdir: Path) -> tuple[dict[str, str], dict[str, str]]:
    """(full process env, the tier-defining part that enters the cache key)."""
    tier_env = {**TIERS[tier], "HAUKSBEE_CLAUDE_EFFORT": effort}
    full = {k: v for k, v in os.environ.items() if not LEAKY_ENV.match(k)}
    full.update(tier_env)
    full["HAUKSBEE_EXTRACT_CONFIG"] = str(sdir / "empty-config.toml")
    full["HAUKSBEE_EXTRACT_EVENTS"] = str(sdir / "events.jsonl")
    return full, tier_env


def command(binary: Path, pdf: Path, case: dict[str, Any], sdir: Path) -> list[str]:
    # --yes: no terminal, so consent must be explicit. The CLI hands itself to the
    # agent as the lint checker, so there is no --checker flag to pass.
    return [str(binary), "models", "extract", "--pdf", str(pdf), "--part", case["part"],
            "--kind", case["kind"], "--out-dir", str(sdir / "out"), "--force", "--yes"]


def run_one(a: argparse.Namespace, ctx: dict[str, Any], tier: str, case: dict[str, Any], rep: int) -> dict[str, Any]:
    sdir = a.runs / tier / re.sub(r"[^A-Za-z0-9._-]", "_", case["part"]) / f"r{rep:02d}"
    pdf = datasheet_path(case, a.datasheets)
    env, tier_env = sample_env(tier, a.effort, sdir)
    cmd = command(a.bin, pdf, case, sdir)
    if a.dry_run:
        shown = {k: v for k, v in env.items() if k.startswith("HAUKSBEE_")}
        cached = "" if pdf.exists() else f"# would download {case['url']} -> {pdf}\n"
        return {"dry": f"{cached}# {tier} / {case['part']} / r{rep:02d} ({sdir})\n"
                       f"env {json.dumps(shown, sort_keys=True)}\n$ {' '.join(cmd)}\n"}
    key = cache_key(pdf.read_bytes(), case["part"], case["kind"], tier_env, ctx["version"], ctx["prompt"])
    prev: dict[str, Any] = {}
    try:
        prev = json.loads((sdir / "result.json").read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        pass
    base = {"tier": tier, "part": case["part"], "kind": case["kind"], "repeat": rep, "key": key}
    if not a.force and prev.get("key") == key and prev.get("completed"):
        res = {**prev, **score_sample(case, sdir, a.bin), "cached": True}
    else:
        shutil.rmtree(sdir / "out", ignore_errors=True)
        (sdir / "events.jsonl").unlink(missing_ok=True)
        sdir.mkdir(parents=True, exist_ok=True)
        (sdir / "empty-config.toml").write_text("", encoding="utf-8")
        t0 = time.monotonic()
        try:
            r = subprocess.run(cmd, env=env, capture_output=True, text=True, errors="replace", timeout=a.timeout)
            code, log = r.returncode, f"$ {' '.join(cmd)}\n--- stdout\n{r.stdout}\n--- stderr\n{r.stderr}"
        except subprocess.TimeoutExpired as exc:
            tail = exc.stderr.decode(errors="replace") if isinstance(exc.stderr, bytes) else (exc.stderr or "")
            code, log = 124, f"timed out after {a.timeout}s\n{tail}"
        except OSError as exc:
            code, log = 127, f"failed to launch: {exc}"
        wall = time.monotonic() - t0
        (sdir / "extract.log").write_text(log, encoding="utf-8")
        scored = score_sample(case, sdir, a.bin)
        res = {**base, "exit_code": code, "wall_s": round(wall, 2), "cached": False,
               "completed": code == 0 or scored["draft"] is not None, **scored}
    res.update(base)
    (sdir / "result.json").write_text(json.dumps(res, indent=2) + "\n", encoding="utf-8")
    return res


# ---------------------------------------------------------------- aggregation


def mean(xs: list[float]) -> float | None:
    return sum(xs) / len(xs) if xs else None


def aggregate(results: list[dict[str, Any]]) -> dict[str, Any]:
    errs = [e for r in results for e in (r.get("params") or {}).get("errors", {}).values()]
    n = len(results)
    return {
        "n": n,
        "pin_exact": sum(bool(r.get("pin_exact")) for r in results),
        "pin_frac_mean": mean([r.get("pin_frac") or 0.0 for r in results]),
        "median_param_err": statistics.median(errs) if errs else None,
        "within_10_of_ref": mean([x for x in ((r.get("params") or {}).get("within_10_of_ref") for r in results) if x is not None]),
        "validator_pass": sum(bool(r.get("validator_pass")) for r in results),
        "mean_cost_usd": mean([r["cost_usd"] for r in results if is_num(r.get("cost_usd"))]),
        "mean_tokens": mean([r["total_tokens"] for r in results if is_num(r.get("total_tokens"))]),
        "mean_wall_s": mean([r["wall_s"] for r in results if is_num(r.get("wall_s"))]),
        "billing": sorted({str(r.get("billing")) for r in results}),
        "mean_week_delta": mean([r["week_delta"] for r in results if is_num(r.get("week_delta"))]),
        "week_total": sum(r["week_delta"] for r in results if is_num(r.get("week_delta")))
        if any(is_num(r.get("week_delta")) for r in results) else None,
        "plan_unavailable": sum(r.get("plan_available") is False for r in results),
    }


def versus(results: list[dict[str, Any]], tier: str, base: str = BASELINE) -> dict[str, Any] | None:
    """`tier` against the baseline on the parts both ran (first repeat), with denominators."""
    by = {t: {r["part"]: r for r in results if r.get("tier") == t and r.get("repeat", 1) == 1} for t in (tier, base)}
    shared = sorted(set(by[tier]) & set(by[base]))
    if not shared or tier == base:
        return None
    count = lambda t, f: sum(bool(by[t][p].get(f)) for p in shared)  # noqa: E731
    v = {"n": len(shared), "pin_exact": count(tier, "pin_exact"), "base_pin_exact": count(base, "pin_exact"),
         "valid": count(tier, "validator_pass"), "base_valid": count(base, "validator_pass")}
    v["below"] = v["pin_exact"] < v["base_pin_exact"] or v["valid"] < v["base_valid"]
    return v


# Plan readings are whole percent: below this many points a tier's total is rounding noise.
PLAN_RESOLUTION = 3
COST_LABEL = {"mean_cost_usd": "mean list-price $", "mean_week_delta": "mean plan week %",
              "mean_tokens": "mean tokens (plan readings too coarse to rank on)"}


def cost_basis(aggs: dict[str, dict[str, Any]]) -> str:
    """One cost measure for every compared tier: metered $ when all are metered, else
    the plan's weekly % when every tier used enough of it to measure, else tokens."""
    if aggs and all(is_num(a.get("mean_cost_usd")) for a in aggs.values()):
        return "mean_cost_usd"
    if aggs and all(is_num(a.get("week_total")) and a["week_total"] >= PLAN_RESOLUTION for a in aggs.values()):
        return "mean_week_delta"
    return "mean_tokens"


def live_tiers(aggs: dict[str, dict[str, Any]], results: list[dict[str, Any]]) -> dict[str, dict[str, Any]]:
    """Claude tiers with results that are not below the Luna baseline."""
    return {t: a for t, a in aggs.items() if a["n"] and not t.startswith("luna")
            and not (versus(results, t) or {}).get("below")}


def recommend(aggs: dict[str, dict[str, Any]], margin: float, results: list[dict[str, Any]] | None = None) -> str | None:
    """Cheapest Claude tier whose pin-map exact count and validator pass count are
    each within `margin` samples of the best tier (rates scaled to the largest n),
    and which is not below the Luna baseline on the parts both ran. Without a
    Luna baseline in `results` the pick is provisional (see `tables`)."""
    results = results or []
    live = live_tiers(aggs, results)
    if not live:
        return None
    top = max(a["n"] for a in live.values())
    def scaled(t: str, f: str) -> float:
        return live[t][f] / live[t]["n"] * top
    best_pin = max(scaled(t, "pin_exact") for t in live)
    best_val = max(scaled(t, "validator_pass") for t in live)
    ok = [t for t in live if best_pin - scaled(t, "pin_exact") <= margin and best_val - scaled(t, "validator_pass") <= margin]
    order, basis = list(TIERS), cost_basis(live)
    return min(ok, key=lambda t: (live[t].get(basis) if is_num(live[t].get(basis)) else float("inf"), order.index(t)))


def fmt(v: Any, spec: str = ".2f") -> str:
    return "-" if v is None else format(v, spec)


def tables(results: list[dict[str, Any]], aggs: dict[str, dict[str, Any]], pick: str | None, margin: float) -> str:
    out = ["| tier | part | pins exact | pin frac | median err | cover | valid | model | esc | tokens | cost (metered $ / plan week %) | wall s |",
           "|---|---|:-:|--:|--:|--:|:-:|---|--:|--:|--:|--:|"]
    for r in results:
        p = r.get("params") or {}
        out.append(f"| {r['tier']} | {r['part']} | {'Y' if r.get('pin_exact') else 'n'} | {fmt(r.get('pin_frac'))} "
                   f"| {fmt(p.get('median_rel_err'), '.3f')} | {fmt(p.get('coverage'))} "
                   f"| {'Y' if r.get('validator_pass') else ('rej' if r.get('rejected') else 'n')} "
                   f"| {r.get('final_model') or '-'} | {r.get('escalations') or 0} | {fmt(r.get('total_tokens'), ',.0f')} "
                   f"| {spend(r)} | {fmt(r.get('wall_s'), '.0f')} |")
    out += ["", "| tier | n | pin exact | vs Luna pins | valid | vs Luna valid | pin frac | median param err | within 10% of ref "
                "| billing | mean $ (metered) | mean plan week % | mean tokens | mean wall s |",
            "|---|--:|--:|--:|--:|--:|--:|--:|--:|---|--:|--:|--:|--:|"]
    for t, a in aggs.items():
        v = versus(results, t)
        vp = f"{v['pin_exact']}/{v['n']} vs {v['base_pin_exact']}/{v['n']}" if v else "-"
        vv = f"{v['valid']}/{v['n']} vs {v['base_valid']}/{v['n']}" if v else "-"
        out.append(f"| {t} | {a['n']} | {a['pin_exact']}/{a['n']} | {vp} | {a['validator_pass']}/{a['n']} | {vv} "
                   f"| {fmt(a['pin_frac_mean'])} | {fmt(a['median_param_err'], '.3f')} | {fmt(a['within_10_of_ref'])} "
                   f"| {','.join(a['billing'])} | {fmt(a['mean_cost_usd'], '.3f')} | {fmt(a['mean_week_delta'], '.2f')} "
                   f"| {fmt(a['mean_tokens'], ',.0f')} | {fmt(a['mean_wall_s'], '.0f')} |")
    has_base = any(r.get("tier") == BASELINE for r in results)
    out += ["", f"Recommended default: {pick or 'none (no tier had results)'} "
                f"(cheapest by {COST_LABEL[cost_basis(live_tiers(aggs, results))]} among Claude tiers "
                f"within {margin:g} sample(s) of the best pin-map exact count and validator "
                f"pass count, and not below the {BASELINE} baseline)"
                + ("" if has_base else f"; PROVISIONAL: no {BASELINE} baseline was run, so no tier was checked against it")]
    out += [lowest_verdict(results) if has_base else
            f"Lowest tier: unchecked (needs the {BASELINE} baseline; run with --tiers {BASELINE},{LOWEST},{LOWEST_PRESET})"]
    return "\n".join(out)


def spend(r: dict[str, Any]) -> str:
    """Dollars only for a metered run; a plan run is its weekly-% share."""
    if r.get("billing") == "metered":
        return f"${fmt(r.get('cost_usd'), '.3f')}"
    if r.get("billing") == "plan":
        if not is_num(r.get("week_delta")):
            return "usage unavailable"
        return f"week {fmt(r.get('week_before'), '.0f')}->{fmt(r.get('week_after'), '.0f')}%"
    return "billing unknown"


def lowest_verdict(results: list[dict[str, Any]]) -> str:
    """Whether the lowest tier may be offered: alone if at/above Luna, else only
    escalating, and not at all (or labelled) if even escalating is below."""
    alone, escalating = versus(results, LOWEST), versus(results, LOWEST_PRESET)
    if alone and not alone["below"]:
        return f"Lowest tier: {LOWEST} is at or above {BASELINE}; it may be offered alone."
    if escalating is None:
        return f"Lowest tier: {LOWEST} is below {BASELINE} or unmeasured; run {LOWEST_PRESET} to check the escalating preset."
    if escalating["below"]:
        return (f"Lowest tier: even {LOWEST_PRESET} is below {BASELINE}; drop claude-code-haiku from the "
                "picker or label it 'below the Luna baseline'.")
    return f"Lowest tier: {LOWEST} alone is below {BASELINE}; offer it only as {LOWEST_PRESET} (escalates on failure)."


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--tiers", default=",".join(DEFAULT_TIERS), help=f"comma list from: {', '.join(TIERS)}")
    ap.add_argument("--case", action="append", default=[], metavar="PART", help="only this part (repeatable)")
    ap.add_argument("--repeat", type=int, default=1)
    ap.add_argument("--jobs", type=int, default=1)
    ap.add_argument("--effort", default="high")
    ap.add_argument("--margin", type=float, default=1.0, help="samples of slack for the recommendation")
    ap.add_argument("--timeout", type=int, default=1800, help="seconds per extraction")
    ap.add_argument("--bin", type=Path, default=REPO_ROOT / "target" / "debug" / "hauksbee")
    ap.add_argument("--runs", type=Path, default=BENCH_DIR / "runs")
    ap.add_argument("--datasheets", type=Path, default=BENCH_DIR / "datasheets")
    ap.add_argument("--force", action="store_true", help="rerun samples even when cached")
    ap.add_argument("--dry-run", action="store_true", help="print commands and env, run nothing")
    ap.add_argument("--yes", action="store_true", help="required for a real run: every sample bills your Claude plan")
    a = ap.parse_args(argv)
    if not a.dry_run and not a.yes:
        ap.error("a real run bills your Claude plan per sample; pass --yes to proceed, or --dry-run to preview")

    tiers = [t.strip() for t in a.tiers.split(",") if t.strip()]
    bad = [t for t in tiers if t not in TIERS]
    if bad or not tiers or a.repeat < 1 or a.jobs < 1:
        ap.error(f"bad --tiers/--repeat/--jobs (unknown tiers: {bad})")
    cases = load_cases()
    if a.case:
        want = {c.upper() for c in a.case}
        cases = [c for c in cases if c["part"].upper() in want or c["card"].upper() in want]
        if not cases:
            ap.error(f"no case matches {a.case}")
    if not a.bin.is_file() and not a.dry_run:
        raise SystemExit(f"error: hauksbee binary not found at {a.bin}; build it (the bench never builds) or pass --bin")
    ctx = {"version": binary_version(a.bin) if a.bin.is_file() and not a.dry_run else "dry-run",
           "prompt": prompt_version()}

    if not a.dry_run:  # datasheets first so a bad URL costs nothing
        for c in cases:
            try:
                download(c["url"], datasheet_path(c, a.datasheets))
            except Exception as exc:  # noqa: BLE001 - report and drop the case
                print(f"warning: skipping {c['part']}: {exc}", file=sys.stderr)
                cases = [x for x in cases if x is not c]

    jobs = [(t, c, r) for c in cases for t in tiers for r in range(1, a.repeat + 1)]
    if a.dry_run:
        for t, c, r in jobs:
            print(run_one(a, ctx, t, c, r)["dry"])
        print(f"{len(jobs)} sample(s); {len(cases)} case(s) x {len(tiers)} tier(s) x {a.repeat} repeat(s). Nothing was run.")
        return 0

    def safe(job: tuple[str, dict[str, Any], int]) -> dict[str, Any]:
        t, c, r = job
        print(f"running {t} / {c['part']} / r{r:02d}", file=sys.stderr, flush=True)
        try:
            return run_one(a, ctx, t, c, r)
        except Exception as exc:  # noqa: BLE001 - one bad sample must not end the sweep
            return {"tier": t, "part": c["part"], "repeat": r, "error": str(exc), "completed": False}

    with ThreadPoolExecutor(max_workers=a.jobs) as pool:
        results = list(pool.map(safe, jobs))
    aggs = {t: aggregate([r for r in results if r["tier"] == t]) for t in tiers}
    pick = recommend(aggs, a.margin, results)
    a.runs.mkdir(parents=True, exist_ok=True)
    (a.runs / "summary.json").write_text(json.dumps({
        "binary": str(a.bin), "version": ctx["version"], "prompt_version": ctx["prompt"], "effort": a.effort,
        "margin": a.margin, "recommended": pick, "tiers": aggs,
        "versus_luna": {t: versus(results, t) for t in tiers}, "results": results}, indent=2) + "\n", encoding="utf-8")
    print(tables(results, aggs, pick, a.margin))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
