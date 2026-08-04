#!/usr/bin/env bash
# Re-run the ingest-robustness corpus end to end.
#
# This is the harness behind the ingest pass-rate claim: it harvests a few
# thousand real board files from public repositories, adds the hostile-but-real
# edge cases, then runs every file through every read surface and classifies the
# outcome as OK / HONEST-REFUSAL / FALSE-REFUSAL / SILENTLY-WRONG / HANG /
# CRASH-OR-PANIC. Only the first two are passes.
#
#   ./run_corpus.sh                 # harvest (if needed) + run the matrix
#   ./run_corpus.sh --matrix-only   # re-run the matrix over an existing corpus
#
# Requirements: python3, an authenticated `gh` (GitHub code search is used for
# the harvest), curl, and a release build of `hauksbee`.
#
# Environment:
#   CORPUS_DIR      where the corpus lives (default: ./ingest-corpus next to this
#                   script). Several GB; keep it OUT of the repository.
#   HAUKSBEE_BIN    the binary under test (default: ../../../target/release/hauksbee)
#   JOBS            parallel processes for the matrix (default: 8)
#   TIMEOUT         per-invocation seconds before a run counts as a HANG
#                   (default: 60)
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$here/../../.." && pwd)"
: "${CORPUS_DIR:=$here/ingest-corpus}"
: "${HAUKSBEE_BIN:=$repo_root/target/release/hauksbee}"
: "${JOBS:=8}"
: "${TIMEOUT:=60}"
export HAUKSBEE_BIN

if [[ ! -x "$HAUKSBEE_BIN" ]]; then
  echo "no binary at $HAUKSBEE_BIN; run: cargo build --release --bin hauksbee" >&2
  exit 1
fi

# The harvest scripts resolve the corpus relative to themselves, so run them
# from a directory whose `ingest-corpus` is the one we want.
work="$(dirname "$CORPUS_DIR")"
mkdir -p "$CORPUS_DIR"
for s in harvest.py harvest_gerber_jobs.py harvest_pnp_jobs.py gen_edge.py runmatrix.py; do
  cp "$here/$s" "$work/$s"
done

if [[ "${1:-}" != "--matrix-only" ]]; then
  echo "== harvesting (GitHub code search is rate-limited; this takes a while)"
  ( cd "$work" && python3 harvest.py all )
  ( cd "$work" && python3 harvest_gerber_jobs.py )
  ( cd "$work" && python3 harvest_pnp_jobs.py )
  echo "== generating edge cases"
  ( cd "$work" && python3 gen_edge.py )
fi

echo "== running the matrix against $HAUKSBEE_BIN"
( cd "$work" && python3 runmatrix.py --jobs "$JOBS" --timeout "$TIMEOUT" \
    --out "$work/matrix-results.jsonl" )

echo
echo "results: $work/matrix-results.jsonl"
echo "provenance (repo + commit + path per file): $CORPUS_DIR/manifest.jsonl"
