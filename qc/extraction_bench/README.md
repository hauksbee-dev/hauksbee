# Datasheet extraction model benchmark

This benchmark measures how reliably the Sol, Luna, and Terra Codex models draft
Hauksbee model cards from the same four TI datasheets. It measures extraction
success, schema validity, selected datasheet facts, provenance, scope honesty,
and Hauksbee lint acceptance. It does not measure physical hardware accuracy or
prove that a generated card is safe to use without engineering review.

## Run it

The release binary must already exist at `target/release/hauksbee`; the benchmark
fails clearly rather than building it. Run the full matrix with repeated samples:

```sh
source ~/.azure-ai.env && python3 qc/extraction_bench/run.py --repeat 3
```

Run one cell while developing or checking credentials:

```sh
source ~/.azure-ai.env && python3 qc/extraction_bench/run.py \
  --only azure-terra --case TXB0101 --repeat 1
```

`--only` accepts `azure`, `azure-luna`, or `azure-terra`; `--case` selects one
declared part; `--force` reruns successful samples. A successful existing
`(profile, case, repeat index)` is otherwise rescored and skipped. Failed cells
do not stop the matrix. An extraction that reports `exceeded rate limit` is
retried up to three times after 60, 180, and 300 seconds.

The harness sets both `HAUKSBEE_CODEX_PROFILE` and the intended
`HAUKSBEE_CODEX_MODEL` for every cell after sourcing `~/.azure-ai.env`:

| Profile | Model |
|---|---|
| `azure` | `gpt-5.6-sol` |
| `azure-luna` | `gpt-5.6-luna` |
| `azure-terra` | `gpt-5.6-terra` |

The explicit model binding matters because the current extractor resolves an
unset model to `gpt-5.6-sol` and passes that model to `codex exec`; relying on a
profile's configured model alone could therefore mislabel a sample.

For each sample, the harness also copies only that profile's non-secret
`~/.codex/<profile>.config.toml` into a writable, run-local `codex-home/` and
sets `CODEX_HOME` to it. This keeps Codex runtime state with the ignored sample,
avoids modifying the user's standing Codex state, and lets the nested CLI run in
managed environments where `~/.codex` is read-only. The Azure key remains only
in the sourced environment and is not copied into the run directory.

## Inputs and retained results

`cases.toml` is the source-controlled benchmark definition. Datasheets are
downloaded from the declared TI URLs into `datasheets/` only when absent.
`datasheets/` is gitignored because the vendor PDFs are not ours to
redistribute.

Each sample is retained under
`runs/<profile>/<part>/repeat-NNN/`, including `extract.log`, the generated card,
and `result.json`. `runs/summary.json` contains the complete matrix summary.
`runs/` is gitignored because it contains bulky, nondeterministic LLM output and
logs rather than source. For multiple repeats, a single case/profile table row
shows comma-separated per-repeat values in repeat-index order.

## Scoring

Each sample receives one point for each applicable criterion:

1. A model-card file was produced.
2. It parses as TOML and contains a non-empty `[[models]]` array.
3. The first model's `kind` equals the case's expected kind.
4. Every numeric assertion in `cases.toml` matches a parsed numeric TOML value
   within that fact's absolute tolerance. Numbers mentioned only in strings or
   comments do not count. A fact can require a path fragment; BQ24075 uses this
   to require its 4.20 V charge setpoint inside the behavioral block. TPS25982
   accepts either both ILIM-equation constants or all three values in the
   declared 100-ohm current-limit row.
5. At least one `[[models.source.uncertainty]]` entry has a `basis` string that
   names a datasheet section, table, figure, or page.
6. A model `description` or TOML comment declares at least one behavior that is
   not modeled.
7. `target/release/hauksbee models lint <card.toml>` exits zero.

The present release binary provides `models lint`, so no criterion is scored
N/A. The harness still detects its availability: if a different binary lacks
the subcommand, criterion 7 is reported as N/A and removed from that sample's
denominator rather than replaced with an invented command.

The printed score is per sample and is not a claim that one model is universally
better. A small number of stochastic LLM drafts is indicative, not conclusive;
use repeated samples, inspect the retained cards and logs, and expand the case
set before making a durable routing or cost decision.
