# Claude tier bench

Runs 12 public datasheets, each with a reviewed card in `crates/hauksbee-models/db`,
through every extraction tier and scores the drafts against those cards. The answer
it gives is which tier should be the default: the cheapest one that does not give up
pin-map correctness or validator passes.

Tiers (`--tiers`, default all but `haiku-single` and `luna-ladder`): `luna` (codex
gpt-6-luna alone: the baseline), `haiku-harnessed` (staged harness), `haiku-sonnet`
(the `claude-code-haiku` preset: Haiku, Sonnet on failure), `sonnet`, `opus` (each
with escalation off), `ladder` (Haiku, then Sonnet, then Opus, harness auto), and the
optional `haiku-single` and Codex `luna-ladder`.

**Luna baseline.** Every Claude tier is compared with `luna` on the parts both ran,
with denominators (`8/12 vs 9/12`). No cached or replayed Luna results exist for these
12 public parts, so `luna` is a real codex run (on a ChatGPT plan, its weekly share is
shown); the held-out sets are not used here. A tier below Luna on pin-map exact count
or validator passes is never recommended as the default. The lowest tier is judged
too: `haiku-harnessed` alone at or above Luna may be offered alone; below it, only as
`haiku-sonnet`; if that is below Luna too, the report says to drop `claude-code-haiku`
from the picker or label it "below the Luna baseline". Without a `luna` run the
recommendation is printed as provisional.

## Run

    # build target/debug/hauksbee first; the bench never builds
    python3 qc/claude_tier_bench/run.py --dry-run    # print every command and env, spend nothing
    python3 qc/claude_tier_bench/run.py --yes --jobs 2

Other flags: `--case PART` (repeatable), `--repeat N`, `--effort high`, `--margin 1`,
`--bin target/debug/hauksbee`, `--timeout 1800`, `--force`.

**Cost.** 12 cases x 6 tiers = 72 extractions: 60 bill your Claude plan (Opus samples
dominate) and 12 your codex sign-in. Real runs refuse to start without `--yes`. Use `--case` and
`--tiers` for a cheap trial first. Run the offline tests with
`python3 -I qc/claude_tier_bench/test_score.py`.

## What is scored (per sample, against the reviewed card)

- Pin map: exact match of pad to role (case and punctuation ignored), and the fraction
  of the card's pads that match. The best-matching `[[models]]` entry of the draft is used.
- Numeric `params` and `ratings` shared with the card: median relative error, share
  within 10 %, and coverage of the card's keys.
- Validator: the draft is in `out/` (not `out/rejected/`) and `hauksbee models lint` exits 0.
- From `events.jsonl`: final model and tier, escalation count and reasons, tokens,
  confidence, plus wall time. Spend follows the CLI's sign-in: list-price dollars only
  for a metered (API-key) run; a plan run shows the weekly % before and after (and the
  5-hour window in `summary.json`), "usage unavailable" if the CLI could not be read,
  and "billing unknown" if the sign-in could not be told. Plan readings are
  account-wide whole percents, so per-item deltas are only clean with `--jobs 1` and
  with no other Claude Code or codex sessions running on the account.
- Pin roles are compared case- and punctuation-insensitively, and a lone channel index
  is ignored (`a1` matches `a` on a single-channel part; `a1`/`a2` stay distinct).

Each run uses an empty `HAUKSBEE_EXTRACT_CONFIG` and drops inherited `HAUKSBEE_CLAUDE_*`,
`CODEX_*`, `EXTRACT_*` and `LLM_*` variables, so saved settings cannot leak in.

## Recommendation

For each tier, count samples with an exact pin map and samples that pass the validator
(scaled to the largest sample count). The recommended default is the cheapest Claude tier
whose two counts are each within `--margin` samples (default 1) of the best tier and which
is not below `luna`. All compared tiers are ranked on one measure: metered dollars when
every tier is metered, else the plan's weekly % when every tier used at least 3 points of
it in total, else tokens (the summary line names the measure used). A plan reading under
that size is rounding noise, and tokens understate how much cheaper Haiku is than Opus.

## Caching

A sample is keyed by sha256 of the PDF bytes, part, kind, tier env, `hauksbee --version`
and a prompt version (hash of `datasheet.rs`, `extract_config.rs`, `extract_harness.rs`).
A finished sample with the same key is rescored, never rerun, unless `--force`.
Datasheets are cached in `datasheets/`; results in `runs/<tier>/<part>/rNN/`
(`result.json`, `extract.log`, `events.jsonl`, `out/`) and `runs/summary.json`.
Both directories are gitignored.
