#!/usr/bin/env bash
# bench.sh - run the S2 graded-board benchmark harness and print a median table.
#
# The harness itself is `crates/hauksbee-solve/benches/graded_boards.rs`, driven
# by criterion. This wrapper runs it,
# then reads criterion's own JSON estimates and prints one compact table of
# medians, so you get the headline numbers without digging through
# target/criterion/ or opening the HTML report (which we do not build).
#
# USAGE
#   scripts/bench.sh                 # run every benchmark, print the median table
#   scripts/bench.sh -- 240          # pass a filter through to criterion (only
#                                    # benchmarks whose id contains "240")
#
#   BASELINE=name scripts/bench.sh   # run and SAVE results as criterion baseline
#                                    # "name" (target/criterion/*/*/name/). Use
#                                    # this to record a reference point.
#
#   COMPARE=name scripts/bench.sh    # run and COMPARE against a previously saved
#                                    # baseline "name" WITHOUT overwriting it.
#                                    # criterion prints per-benchmark change %,
#                                    # and the table below gains a baseline
#                                    # column plus a delta.
#
# Anything after `--` is forwarded verbatim to the benchmark binary, so all of
# criterion's flags work (e.g. `-- --sample-size 50`, `-- --test` for a smoke
# run that just executes each benchmark once).
#
# HONEST-BASELINE RULE (08-validation-and-test-campaign.md §4): a saved baseline
# is a committed reference. Moving it is a deliberate act - land a baseline
# change in its OWN commit with the flamegraph diff or an explanation, never
# folded into an unrelated change.md.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

CRIT_DIR="target/criterion"
BENCH_ARGS=()
# Everything after a literal `--` is forwarded to the criterion binary.
if [[ "${1:-}" == "--" ]]; then
    shift
    BENCH_ARGS=("$@")
elif [[ $# -gt 0 ]]; then
    BENCH_ARGS=("$@")
fi

CARGO_TAIL=()
if [[ -n "${BASELINE:-}" ]]; then
    echo "==> saving criterion baseline: ${BASELINE}"
    CARGO_TAIL=(--save-baseline "${BASELINE}")
elif [[ -n "${COMPARE:-}" ]]; then
    echo "==> comparing against criterion baseline: ${COMPARE} (not overwriting)"
    CARGO_TAIL=(--baseline "${COMPARE}")
fi

# `${arr[@]+...}` guards empty-array expansion under `set -u` on bash 3.2 (the
# macOS default), where a bare `"${arr[@]}"` on an empty array is an error.
echo "==> running: cargo bench -p hauksbee-solve --bench graded_boards -- ${BENCH_ARGS[*]:-} ${CARGO_TAIL[*]:-}"
cargo bench -p hauksbee-solve --bench graded_boards -- \
    ${BENCH_ARGS[@]+"${BENCH_ARGS[@]}"} ${CARGO_TAIL[@]+"${CARGO_TAIL[@]}"}

# --test smoke runs write no estimates, so there is nothing to tabulate.
for a in ${BENCH_ARGS[@]+"${BENCH_ARGS[@]}"}; do
    if [[ "$a" == "--test" ]]; then
        echo "(smoke run: no medians to tabulate)"
        exit 0
    fi
done

echo
echo "==> median wall-times"
# criterion stores median.point_estimate (nanoseconds) per benchmark at
# <group>/<id>/new/estimates.json, and a saved baseline at <group>/<id>/<name>/.
# The COMPARE column, when present, reads that saved baseline's median.
COMPARE="${COMPARE:-}" CRIT_DIR="$CRIT_DIR" python3 - <<'PY'
import json, os, sys

crit = os.environ["CRIT_DIR"]
compare = os.environ.get("COMPARE", "")

def median_ns(path):
    try:
        with open(path) as f:
            return json.load(f)["median"]["point_estimate"]
    except (OSError, KeyError, ValueError):
        return None

def human(ns):
    if ns is None:
        return "-"
    for unit, scale in (("s", 1e9), ("ms", 1e6), ("us", 1e3), ("ns", 1.0)):
        if ns >= scale:
            return f"{ns / scale:.3f} {unit}"
    return f"{ns:.3f} ns"

rows = []
for group in sorted(os.listdir(crit)) if os.path.isdir(crit) else []:
    gdir = os.path.join(crit, group)
    if not os.path.isdir(gdir) or group == "report":
        continue
    for bid in sorted(os.listdir(gdir)):
        newest = os.path.join(gdir, bid, "new", "estimates.json")
        if not os.path.isfile(newest):
            continue
        cur = median_ns(newest)
        base = median_ns(os.path.join(gdir, bid, compare, "estimates.json")) if compare else None
        rows.append((f"{group}/{bid}", cur, base))

if not rows:
    print("  (no criterion estimates found; run the benchmarks first)")
    sys.exit(0)

name_w = max(len(r[0]) for r in rows)
name_w = max(name_w, len("benchmark"))
cur_w = 12

if compare:
    hdr = f"| {'benchmark':<{name_w}} | {'median':>{cur_w}} | {'baseline':>{cur_w}} | {'delta':>8} |"
    sep = f"|{'-' * (name_w + 2)}|{'-' * (cur_w + 2)}|{'-' * (cur_w + 2)}|{'-' * 10}|"
    print(hdr); print(sep)
    for name, cur, base in rows:
        if cur is not None and base:
            delta = f"{(cur / base - 1.0) * 100:+.1f}%"
        else:
            delta = "-"
        print(f"| {name:<{name_w}} | {human(cur):>{cur_w}} | {human(base):>{cur_w}} | {delta:>8} |")
else:
    hdr = f"| {'benchmark':<{name_w}} | {'median':>{cur_w}} |"
    sep = f"|{'-' * (name_w + 2)}|{'-' * (cur_w + 2)}|"
    print(hdr); print(sep)
    for name, cur, _ in rows:
        print(f"| {name:<{name_w}} | {human(cur):>{cur_w}} |")
PY
