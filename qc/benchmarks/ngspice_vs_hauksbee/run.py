#!/usr/bin/env python3
"""Machine-readable Hauksbee/ngspice board-style benchmark.

No claim in this script is a threshold. It records a fresh comparison, retains
all refusals/failures, and leaves threshold selection to a later measured
campaign. It intentionally parses Hauksbee CSV and the ngspice rawfile rather
than scraping human-facing process output.
"""

from __future__ import annotations

import argparse
import csv
import hashlib
import json
import math
import os
import platform
import re
import statistics
import struct
import subprocess
import sys
import tempfile
import time
from bisect import bisect_right
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[3]
MANIFEST = Path(__file__).with_name("manifest.json")


def sha256_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def sha256_file(path: Path) -> str:
    return sha256_bytes(path.read_bytes())


def command_version(command: Path) -> dict[str, Any]:
    """Capture version output as provenance, not as a data source."""
    try:
        p = subprocess.run(
            [str(command), "--version"],
            cwd=ROOT,
            text=True,
            capture_output=True,
            timeout=15,
            check=False,
        )
        text = (p.stdout + p.stderr).strip()
        return {"exit_code": p.returncode, "text": text[-2000:]}
    except Exception as exc:  # noqa: BLE001 - retained in JSON provenance
        return {"error": f"{type(exc).__name__}: {exc}"}


def launch_probe(command: Path, warmups: int, samples: int) -> dict[str, Any]:
    """Record a launch diagnostic without subtracting it from case timings.

    ``--version`` does not traverse the same startup path as a simulation.  It
    is useful host context, but subtracting it can create zero or wildly
    amplified "corrected" durations for short cases.  The benchmark therefore
    keeps this probe as diagnostics and makes speed comparisons only from the
    actual end-to-end case processes.
    """
    durations: list[float] = []
    for _ in range(max(1, warmups + samples)):
        started = time.perf_counter_ns()
        try:
            subprocess.run(
                [str(command), "--version"],
                cwd=ROOT,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                timeout=15,
                check=False,
            )
            durations.append((time.perf_counter_ns() - started) / 1e9)
        except Exception:
            # A missing executable is represented by the empty list. The real
            # case run will carry the actionable exception and exit code.
            break
    return {
        "samples_s": durations,
        "minimum_s": min(durations) if durations else None,
        "method": "diagnostic only: minimum of warmup+sample --version launches; never subtracted",
    }


def parse_tran_card(deck: str) -> tuple[float, float]:
    for line in deck.splitlines():
        stripped = line.strip()
        if not stripped or stripped.startswith("*"):
            continue
        match = re.match(r"^\.tran\s+(\S+)\s+(\S+)(?:\s+\S+)?(?:\s+\S+)?", stripped, re.I)
        if match:
            return parse_spice_number(match.group(1)), parse_spice_number(match.group(2))
    raise ValueError("source deck has no .tran card")


def parse_spice_number(value: str) -> float:
    suffixes = {
        "t": 1e12,
        "g": 1e9,
        "meg": 1e6,
        "k": 1e3,
        "m": 1e-3,
        "u": 1e-6,
        "n": 1e-9,
        "p": 1e-12,
        "f": 1e-15,
    }
    match = re.fullmatch(r"([+-]?(?:\d+(?:\.\d*)?|\.\d+)(?:[eE][+-]?\d+)?)([a-zA-Z]+)?", value)
    if not match:
        raise ValueError(f"invalid SPICE number {value!r}")
    number = float(match.group(1))
    suffix = (match.group(2) or "").lower()
    return number * suffixes.get(suffix, 1.0)


def shared_times(tstep: float, tstop: float) -> list[float]:
    if not (tstep > 0 and tstop >= 0):
        raise ValueError(f"invalid .tran interval tstep={tstep} tstop={tstop}")
    count = int(math.floor(tstop / tstep + 0.5))
    values = [i * tstep for i in range(count + 1)]
    if not values or abs(values[-1] - tstop) > max(1e-15, abs(tstop) * 1e-12):
        values.append(tstop)
    else:
        values[-1] = tstop
    return values


def parse_hauksbee_csv(path: Path, probe: str) -> tuple[list[float], list[float]]:
    with path.open(newline="") as handle:
        rows = list(csv.reader(handle))
    if not rows or len(rows[0]) < 2:
        raise ValueError("Hauksbee CSV has no header")
    headers = [h.strip().lower() for h in rows[0]]
    probe_key = probe.strip().lower()
    try:
        time_column = headers.index("time_s")
    except ValueError as exc:
        raise ValueError("Hauksbee CSV has no time_s column") from exc
    try:
        value_column = headers.index(probe_key)
    except ValueError as exc:
        raise ValueError(f"Hauksbee CSV has no {probe} column") from exc
    times: list[float] = []
    values: list[float] = []
    for row in rows[1:]:
        if len(row) <= max(time_column, value_column):
            raise ValueError("short Hauksbee CSV row")
        times.append(float(row[time_column]))
        values.append(float(row[value_column]))
    validate_series(times, values, "Hauksbee CSV")
    return times, values


def parse_ngspice_raw(path: Path) -> tuple[dict[str, list[float]], dict[str, Any]]:
    """Decode ngspice's declared binary rawfile; no human output is parsed."""
    blob = path.read_bytes()
    marker = b"Binary:\n"
    try:
        header_end = blob.index(marker) + len(marker)
    except ValueError as exc:
        raise ValueError("ngspice rawfile has no Binary marker") from exc
    header = blob[:header_end].decode("ascii", errors="strict")
    variables = []
    points = None
    for line in header.splitlines():
        if line.startswith("No. Points:"):
            points = int(line.split(":", 1)[1].strip())
        elif re.match(r"^\s*\d+\s+\S+\s+\S+", line):
            _, name, kind = line.split()[:3]
            variables.append((name.lower(), kind))
    if points is None or not variables:
        raise ValueError("ngspice rawfile header lacks points or variables")
    payload = blob[header_end:]
    needed = points * len(variables) * 8
    if len(payload) < needed:
        raise ValueError(f"ngspice rawfile truncated: need {needed}, have {len(payload)}")
    # ngspice writes native-endian doubles. macOS/Linux are little-endian, but
    # a source-bound harness should fail clearly (or decode correctly) on a
    # big-endian host rather than silently comparing nonsense.
    endian = "<"
    try:
        data = struct.unpack(endian + "d" * (points * len(variables)), payload[:needed])
    except struct.error as exc:
        raise ValueError(f"ngspice rawfile data is not doubles: {exc}") from exc
    time_offset = next((i for i, (name, _kind) in enumerate(variables) if name == "time"), None)
    if time_offset is not None:
        little_times = data[time_offset::len(variables)][: min(points, 4)]
        little_ok = all(math.isfinite(value) for value in little_times) and all(
            b >= a for a, b in zip(little_times, little_times[1:])
        )
        if not little_ok:
            endian = ">"
            data = struct.unpack(endian + "d" * (points * len(variables)), payload[:needed])
    columns = {name: [] for name, _ in variables}
    for point in range(points):
        offset = point * len(variables)
        for index, (name, _kind) in enumerate(variables):
            columns[name].append(data[offset + index])
    times = columns.get("time")
    if times is None:
        raise ValueError("ngspice rawfile has no time variable")
    validate_series(times, times, "ngspice rawfile time")
    return columns, {
        "format": "ngspice-binary-rawfile",
        "endianness": "little" if endian == "<" else "big",
        "header_sha256": sha256_bytes(blob[:header_end]),
        "points": points,
        "variables": [{"name": n, "kind": k} for n, k in variables],
    }


def validate_series(times: list[float], values: list[float], label: str) -> None:
    if len(times) != len(values) or not times:
        raise ValueError(f"{label} has no aligned samples")
    if any(not math.isfinite(x) for x in times + values):
        raise ValueError(f"{label} contains non-finite values")
    if any(b < a for a, b in zip(times, times[1:])):
        raise ValueError(f"{label} timestamps are not monotonic")


def interpolate(times: list[float], values: list[float], at: float) -> float:
    if at <= times[0]:
        return values[0]
    if at >= times[-1]:
        return values[-1]
    hi = bisect_right(times, at)
    lo = hi - 1
    span = times[hi] - times[lo]
    return values[lo] if span == 0 else values[lo] + (values[hi] - values[lo]) * ((at - times[lo]) / span)


def series_on_grid(times: list[float], values: list[float], grid: list[float]) -> list[float]:
    return [interpolate(times, values, at) for at in grid]


def percentile(values: list[float], p: float) -> float:
    if not values:
        return math.nan
    ordered = sorted(values)
    rank = (len(ordered) - 1) * p
    lo = int(math.floor(rank))
    hi = int(math.ceil(rank))
    if lo == hi:
        return ordered[lo]
    return ordered[lo] + (ordered[hi] - ordered[lo]) * (rank - lo)


def error_metrics(ours: list[float], oracle: list[float], grid: list[float], settled_fraction: float) -> dict[str, Any]:
    if len(ours) != len(oracle) or len(ours) != len(grid):
        raise ValueError("unaligned comparison series")
    abs_errors = [abs(a - b) for a, b in zip(ours, oracle)]
    scale = max(max((abs(x) for x in ours), default=0.0), max((abs(x) for x in oracle), default=0.0), 1e-12)
    rel_errors = [e / scale for e in abs_errors]
    settle_start = max(0, int(math.floor(len(abs_errors) * (1.0 - settled_fraction))))
    settled_abs = abs_errors[settle_start:]
    settled_rel = rel_errors[settle_start:]
    rms = math.sqrt(sum(x * x for x in abs_errors) / len(abs_errors))
    return {
        "sample_count": len(abs_errors),
        "scale": scale,
        "max_abs": max(abs_errors),
        "p95_abs": percentile(abs_errors, 0.95),
        "rms_abs": rms,
        "settled_max_abs": max(settled_abs) if settled_abs else math.nan,
        "settled_rms_abs": math.sqrt(sum(x * x for x in settled_abs) / len(settled_abs)) if settled_abs else math.nan,
        "max_relative": max(rel_errors),
        "p95_relative": percentile(rel_errors, 0.95),
        "rms_relative": math.sqrt(sum(x * x for x in rel_errors) / len(rel_errors)),
        "settled_max_relative": max(settled_rel) if settled_rel else math.nan,
        "settled_rms_relative": math.sqrt(sum(x * x for x in settled_rel) / len(settled_rel)) if settled_rel else math.nan,
        "settled_fraction": settled_fraction,
        "worst_at_s": grid[abs_errors.index(max(abs_errors))],
    }


def timing_stats(raw: list[float]) -> dict[str, Any]:
    return {
        "raw_s": raw,
        "raw_median_s": statistics.median(raw) if raw else None,
        "raw_p95_s": percentile(raw, 0.95),
        "method": "end-to-end child-process wall time; warmups excluded; tool order alternates by sample",
    }


def paired_speedup_summary(hauksbee: list[float], ngspice: list[float]) -> dict[str, Any]:
    """Summarise paired end-to-end ratios without treating spread as a CI."""
    if len(hauksbee) != len(ngspice) or not hauksbee or any(value <= 0 for value in hauksbee):
        return {"samples": [], "median": None, "p10": None, "p90": None, "classification": "not_measurable"}
    ratios = [oracle / ours for ours, oracle in zip(hauksbee, ngspice)]
    low = percentile(ratios, 0.10)
    high = percentile(ratios, 0.90)
    if low > 1.0:
        classification = "hauksbee_faster_across_interdecile_range"
    elif high < 1.0:
        classification = "ngspice_faster_across_interdecile_range"
    else:
        classification = "mixed_within_interdecile_range"
    return {
        "samples": ratios,
        "median": statistics.median(ratios),
        "p10": low,
        "p90": high,
        "classification": classification,
        "uncertainty_note": "p10-p90 is observed within-run spread, not a confidence interval",
    }


def timing_crossover(timing: dict[str, Any]) -> dict[str, Any]:
    """Report the winner per case; never collapse cases into one headline."""
    raw = timing.get("speedup_raw_ngspice_over_hauksbee")
    winner = lambda value: ("hauksbee" if value > 1 else "ngspice") if value is not None else "not_measurable"
    return {
        "end_to_end_winner": winner(raw),
        "policy": "per-case only; no aggregate winner",
    }


def run_once(command: list[str], cwd: Path, timeout: float) -> tuple[float, int, str, str]:
    started = time.perf_counter_ns()
    try:
        process = subprocess.run(command, cwd=cwd, text=True, capture_output=True, timeout=timeout, check=False)
        elapsed = (time.perf_counter_ns() - started) / 1e9
        return elapsed, process.returncode, process.stdout[-2000:], process.stderr[-4000:]
    except Exception as exc:  # noqa: BLE001 - retained as structured failure
        elapsed = (time.perf_counter_ns() - started) / 1e9
        return elapsed, -1, "", f"{type(exc).__name__}: {exc}"


def run_case(case: dict[str, Any], args: argparse.Namespace) -> dict[str, Any]:
    source = (ROOT / case["source"]).resolve()
    base: dict[str, Any] = {
        "id": case["id"],
        "source": str(source.relative_to(ROOT)),
        "source_sha256": None,
        "class": case["class"],
        "notes": case.get("notes", ""),
        "status": "failed",
        "failures": [],
        "refusals": [],
        "probes": {},
    }
    try:
        deck = source.read_text()
        base["source_sha256"] = sha256_file(source)
        expected_sha = case.get("source_sha256")
        if expected_sha and expected_sha != base["source_sha256"]:
            base["failures"].append({"stage": "source", "error": f"source hash drift: expected {expected_sha}, got {base['source_sha256']}"})
            return base
        dependencies = []
        for dependency in case.get("dependencies", []):
            dep_path = (ROOT / dependency["path"]).resolve()
            dep_sha = sha256_file(dep_path)
            dependencies.append({"path": str(dep_path.relative_to(ROOT)), "sha256": dep_sha})
            if dependency.get("sha256") and dependency["sha256"] != dep_sha:
                base["failures"].append({"stage": "source", "error": f"dependency hash drift for {dependency['path']}: expected {dependency['sha256']}, got {dep_sha}"})
        base["dependencies"] = dependencies
        if base["failures"]:
            return base
        tstep, tstop = parse_tran_card(deck)
        grid = shared_times(tstep, tstop)
        base["tran"] = {"tstep_s": tstep, "tstop_s": tstop, "shared_timestamp_count": len(grid), "shared_timestamp_sha256": sha256_bytes(json.dumps(grid, separators=(",", ":")).encode())}
    except Exception as exc:  # noqa: BLE001
        base["failures"].append({"stage": "source", "error": f"{type(exc).__name__}: {exc}"})
        return base

    with tempfile.TemporaryDirectory(prefix=f"hauksbee-ngspice-{case['id']}-") as work:
        workdir = Path(work)
        for probe in case["probes"]:
            key = probe.lower()
            row: dict[str, Any] = {"probe": key, "status": "failed", "failures": [], "refusals": []}
            hb_csv = workdir / "hauksbee.csv"
            ng_raw = workdir / "ngspice.raw"
            hb_command = [str(args.hauksbee), "sim", str(source), "--tran", "--print", probe, "--out", str(hb_csv), "--quiet"]
            ng_command = [str(args.ngspice), "-n", "-b", "-r", str(ng_raw), str(source)]
            hb_raw_times: list[float] = []
            ng_raw_times: list[float] = []
            hb_evidence: tuple[list[float], list[float]] | None = None
            ng_evidence: tuple[list[float], list[float]] | None = None
            process_orders: list[list[str]] = []
            for index in range(args.warmups + args.samples):
                # A failed child must not leave a previous sample looking like
                # fresh evidence. These are exact temporary paths, not a broad
                # cleanup target.
                if hb_csv.exists():
                    hb_csv.unlink()
                if ng_raw.exists():
                    ng_raw.unlink()
                if index % 2 == 0:
                    hb_elapsed, hb_code, hb_stdout, hb_stderr = run_once(hb_command, source.parent, args.timeout)
                    ng_elapsed, ng_code, ng_stdout, ng_stderr = run_once(ng_command, source.parent, args.timeout)
                    order = ["hauksbee", "ngspice"]
                else:
                    ng_elapsed, ng_code, ng_stdout, ng_stderr = run_once(ng_command, source.parent, args.timeout)
                    hb_elapsed, hb_code, hb_stdout, hb_stderr = run_once(hb_command, source.parent, args.timeout)
                    order = ["ngspice", "hauksbee"]
                if index >= args.warmups:
                    hb_raw_times.append(hb_elapsed)
                    ng_raw_times.append(ng_elapsed)
                    process_orders.append(order)
                if hb_code != 0:
                    text = hb_stderr or hb_stdout
                    if "refused" in text.lower() or hb_code == 3:
                        row["refusals"].append({"tool": "hauksbee", "exit_code": hb_code, "stderr_tail": text[-2000:]})
                    else:
                        row["failures"].append({"tool": "hauksbee", "exit_code": hb_code, "stderr_tail": text[-2000:]})
                if ng_code != 0:
                    row["failures"].append({"tool": "ngspice", "exit_code": ng_code, "stderr_tail": ng_stderr[-2000:]})
                # Successful runs may still carry warnings (for example a
                # model limitation). Keep one copy as diagnostics, but never
                # inspect it for numerical data.
                for tool, stream in (("hauksbee", hb_stderr), ("ngspice", ng_stderr)):
                    lower_stream = stream.lower()
                    if stream.strip() and any(token in lower_stream for token in ("warning", "error", "refus")):
                        row.setdefault("diagnostics", []).append({"tool": tool, "sample": index, "stderr_tail": stream[-2000:]})
                if hb_code == 0 and hb_csv.is_file():
                    try:
                        hb_evidence = parse_hauksbee_csv(hb_csv, probe)
                    except Exception as exc:  # noqa: BLE001
                        row["failures"].append({"tool": "hauksbee", "stage": "parse", "error": f"{type(exc).__name__}: {exc}"})
                if ng_code == 0 and ng_raw.is_file():
                    try:
                        columns, raw_meta = parse_ngspice_raw(ng_raw)
                        ng_key = probe.lower().replace(" ", "")
                        if ng_key not in columns:
                            row["failures"].append({"tool": "ngspice", "stage": "probe", "error": f"rawfile has no {ng_key}; available={sorted(columns)}"})
                        else:
                            ng_evidence = (columns["time"], columns[ng_key])
                            row["oracle_rawfile"] = raw_meta
                    except Exception as exc:  # noqa: BLE001
                        row["failures"].append({"tool": "ngspice", "stage": "parse", "error": f"{type(exc).__name__}: {exc}"})
            row["timing"] = {
                "hauksbee": timing_stats(hb_raw_times),
                "ngspice": timing_stats(ng_raw_times),
                "speedup_raw_ngspice_over_hauksbee": (statistics.median(ng_raw_times) / statistics.median(hb_raw_times)) if hb_raw_times and hb_raw_times and statistics.median(hb_raw_times) > 0 else None,
                "paired_speedup_ngspice_over_hauksbee": paired_speedup_summary(hb_raw_times, ng_raw_times),
                "sample_process_order": process_orders,
            }
            row["timing"]["per_case_crossover"] = timing_crossover(row["timing"])
            if hb_evidence and ng_evidence:
                try:
                    hb_grid = series_on_grid(*hb_evidence, grid)
                    ng_grid = series_on_grid(*ng_evidence, grid)
                    row["comparison"] = {
                        "timestamp_policy": "exact deterministic shared grid; linear interpolation from each tool's machine-readable waveform",
                        "metrics": error_metrics(hb_grid, ng_grid, grid, float(case.get("settled_fraction", 0.1))),
                        # Retained only in memory for the optional mutation gate;
                        # scrubbed before the JSON artifact is written.
                        "_ours_grid": hb_grid,
                        "_oracle_grid": ng_grid,
                    }
                    row["status"] = "measured" if not row["failures"] and not row["refusals"] else "measured_with_failures"
                except Exception as exc:  # noqa: BLE001
                    row["failures"].append({"stage": "comparison", "error": f"{type(exc).__name__}: {exc}"})
            if row["status"] != "measured" and not row["failures"] and not row["refusals"]:
                row["failures"].append({"stage": "comparison", "error": "one or both tools emitted no usable waveform"})
            base["probes"][key] = row
    statuses = [row["status"] for row in base["probes"].values()]
    base["status"] = "measured" if statuses and all(s == "measured" for s in statuses) else "failed"
    return base


def negative_mutation(case_results: list[dict[str, Any]]) -> dict[str, Any]:
    rows = []
    for case in case_results:
        for probe in case.get("probes", {}).values():
            comparison = probe.get("comparison")
            if not comparison or "_ours_grid" not in comparison or "_oracle_grid" not in comparison:
                continue
            metrics = comparison["metrics"]
            baseline = metrics["max_abs"]
            # Mutate a real sampled waveform, then send it back through the
            # same metric function. This proves the comparator is sensitive to
            # changed data, rather than merely adding a synthetic number to a
            # reported result.
            mutated_series = list(comparison["_ours_grid"])
            index = max(
                range(len(mutated_series)),
                key=lambda i: abs(mutated_series[i] - comparison["_oracle_grid"][i]),
            )
            amplitude = max(metrics["scale"] * 0.01, 1e-9)
            direction = 1.0 if mutated_series[index] >= comparison["_oracle_grid"][index] else -1.0
            mutated_series[index] += direction * amplitude
            mutated_metrics = error_metrics(
                mutated_series,
                comparison["_oracle_grid"],
                list(range(len(mutated_series))),
                metrics["settled_fraction"],
            )
            mutated = mutated_metrics["max_abs"]
            rows.append({"case": case["id"], "probe": probe["probe"], "index": index, "amplitude": amplitude, "baseline_max_abs": baseline, "mutated_max_abs": mutated, "detected": mutated > baseline})
    return {"enabled": True, "rows": rows, "detected_all": bool(rows) and all(row["detected"] for row in rows)}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path, default=MANIFEST)
    parser.add_argument("--hauksbee", type=Path, default=ROOT / "target/debug/hauksbee")
    parser.add_argument("--ngspice", type=Path, default=Path("/opt/homebrew/bin/ngspice"))
    parser.add_argument("--output", type=Path, default=ROOT / "qc/results/ngspice-vs-hauksbee.json")
    parser.add_argument("--warmups", type=int, default=2)
    parser.add_argument("--samples", type=int, default=9)
    parser.add_argument("--timeout", type=float, default=120.0)
    parser.add_argument("--case", action="append", dest="cases")
    parser.add_argument("--negative", action="store_true")
    args = parser.parse_args()
    args.hauksbee = args.hauksbee.resolve()
    args.ngspice = args.ngspice.resolve()
    manifest = json.loads(args.manifest.read_text())
    selected = [case for case in manifest["cases"] if not args.cases or case["id"] in args.cases]
    if not selected:
        raise SystemExit("no benchmark cases selected")
    probes = {"hauksbee": launch_probe(args.hauksbee, args.warmups, args.samples), "ngspice": launch_probe(args.ngspice, args.warmups, args.samples)}
    result: dict[str, Any] = {
        "schema_version": 1,
        "generated_at_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "threshold_status": "not_set_after_first_gate",
        "manifest_sha256": sha256_file(args.manifest),
        "tools": {
            "hauksbee": {"path": str(args.hauksbee), "exists": args.hauksbee.is_file(), "sha256": sha256_file(args.hauksbee) if args.hauksbee.is_file() else None, "version": command_version(args.hauksbee)},
            "ngspice": {"path": str(args.ngspice), "exists": args.ngspice.is_file(), "sha256": sha256_file(args.ngspice) if args.ngspice.is_file() else None, "version": command_version(args.ngspice)},
        },
        "launch_probes": probes,
        "timing_policy": "raw end-to-end child-process wall time; alternating tool order; no startup subtraction",
        "machine": {"platform": platform.platform(), "processor": platform.processor(), "python": sys.version, "cpu_count": os.cpu_count(), "load_average": list(os.getloadavg()) if hasattr(os, "getloadavg") else None},
        "cases": [],
        "next_matrix": manifest.get("next_matrix", []),
    }
    for case in selected:
        result["cases"].append(run_case(case, args))
    measured = sum(case["status"] == "measured" for case in result["cases"])
    failed = sum(case["status"] == "failed" for case in result["cases"])
    with_failures = sum(case["status"] == "measured_with_failures" for case in result["cases"])
    attempted = len(result["cases"])
    refusal_count = sum(
        len(probe.get("refusals", []))
        for case in result["cases"]
        for probe in case.get("probes", {}).values()
    )
    result["eligibility"] = {
        "eligible_count": len(selected),
        "attempted_count": attempted,
        "measured_count": measured,
        "measured_with_failures_count": with_failures,
        "invalid_count": failed,
        "refusal_record_count": refusal_count,
        "policy": "every manifest case is eligible; a refused or failed case remains attempted and cannot count as measured",
    }
    # A compact raw case table makes it possible to review the matrix without
    # interpreting prose or reconstructing nested JSON by hand. It includes
    # every probe, including failed/refused rows.
    raw_rows: list[dict[str, Any]] = []
    for case in result["cases"]:
        for probe_name, probe in case.get("probes", {}).items():
            metrics = probe.get("comparison", {}).get("metrics", {})
            timing = probe.get("timing", {})
            raw_rows.append({
                "case": case["id"],
                "class": case["class"],
                "source": case["source"],
                "probe": probe_name,
                "status": probe.get("status"),
                "max_abs": metrics.get("max_abs"),
                "p95_abs": metrics.get("p95_abs"),
                "rms_abs": metrics.get("rms_abs"),
                "settled_max_abs": metrics.get("settled_max_abs"),
                "raw_median_hauksbee_s": timing.get("hauksbee", {}).get("raw_median_s"),
                "raw_median_ngspice_s": timing.get("ngspice", {}).get("raw_median_s"),
                "speedup_raw_ngspice_over_hauksbee": timing.get("speedup_raw_ngspice_over_hauksbee"),
                "paired_speedup_ngspice_over_hauksbee": timing.get("paired_speedup_ngspice_over_hauksbee"),
                "per_case_crossover": timing.get("per_case_crossover"),
                "failure_count": len(probe.get("failures", [])),
                "refusal_count": len(probe.get("refusals", [])),
            })
    result["raw_case_table"] = raw_rows
    if args.negative:
        result["negative_mutation"] = negative_mutation(result["cases"])
    # Do not publish the full waveform twice. It was retained above solely so
    # `--negative` could exercise the actual comparator with a real mutation.
    for case in result["cases"]:
        for probe in case.get("probes", {}).values():
            comparison = probe.get("comparison")
            if comparison:
                comparison.pop("_ours_grid", None)
                comparison.pop("_oracle_grid", None)
    args.output = args.output.resolve()
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
    print(json.dumps({"output": str(args.output), "case_statuses": {c["id"]: c["status"] for c in result["cases"]}, "negative_mutation": result.get("negative_mutation")}, sort_keys=True))
    return 0 if all(c["status"] == "measured" for c in result["cases"]) else 2


if __name__ == "__main__":
    raise SystemExit(main())
