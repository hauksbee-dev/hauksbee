# Hauksbee versus ngspice: board-style first gate

This is a deliberately bounded, source-bound measurement harness. It compares
the checked-in Hauksbee SPICE front door with ngspice on seven transient board
subcircuits: supply/diode/load, gated BJT mirror, MOS load switch, charged
rectifier, flyback transfer, a decoupling RC ladder, and a controlled-source
op-amp macro. The source files are not copied or rewritten; their SHA-256
digests are recorded in the JSON result every time.

The harness uses the tools' machine-readable files only:

* Hauksbee writes its CSV result with `hauksbee sim --out`.
* ngspice writes an ASCII-independent binary rawfile with `-r`; the harness
  decodes its declared header and point-major doubles, never its terminal text.
* Both waveforms are linearly sampled at the same deterministic timestamps
  derived from the source deck's `.tran` card. The shared timestamp vector and
  its SHA-256 digest are emitted in the result.

Each case reports maximum, p95, RMS, and settled absolute and scale-normalised
errors, plus end-to-end median/p95 wall times. Tool order alternates by sample
to reduce a fixed first/second-process bias. A separate `--version` launch probe
is retained only as host diagnostics: it is never subtracted from a case. An
earlier subtraction experiment could produce zero-duration or wildly amplified
"corrected" results on short solves, so those values are deliberately not part
of this evidence. The harness reports a per-case winner, never an aggregate
winner. Paired ngspice/Hauksbee ratios include their observed p10-p90 spread;
that spread is host noise context, not an inferential confidence interval.
Eligible, attempted, measured, invalid, and refused
counts plus a flat raw case table are emitted at the top level. A failed
process, missing probe, malformed rawfile, non-finite value, or solver refusal is
kept as a structured failure record; it is never turned into a zero or a green
row. Agreement and disclosed-drift rows remain separate. The manifest contains
no speed or accuracy thresholds yet: a threshold is added only after the first
fresh run has a retained result and a reviewer can see the measured spread.
The output contract is described by `result.schema.json`; the artifact itself
also records the manifest hash and each source-deck hash.

Run from the repository root (build the binary first):

```sh
uv run --no-project python qc/benchmarks/ngspice_vs_hauksbee/run.py \
  --hauksbee target/debug/hauksbee \
  --output qc/results/ngspice-vs-hauksbee.json
```

Use `--warmups 2 --samples 9` for a quick local measurement, or increase both
for a release-quality timing campaign. `--negative` applies a deterministic
one-sample perturbation to each successful Hauksbee waveform and asserts that
the error metrics move; this is a cheap anti-gaming check on the measurement
path, not evidence about either solver.

This first matrix intentionally stops at seven circuits. The manifest's
`next_matrix` names the next source-bound cases: a Zener regulator, transformer,
and extracted MNT Reform/ESP32-PoE nets. Those should be added only with
explicit probe ownership and refusal handling, not by copying old headline
numbers.

## Debug versus optimized binaries

Do not use a debug binary to support a release-performance claim. The retained
seven-case artifact records the exact debug executable it measured. A separate
paired BJT-only rerun using a pinned optimized executable is retained at
`evidence/benchmarks/bjt-profile-release.json`: its ngspice/Hauksbee ratio was
1.20x at the median and 1.11x at p10, while keeping the same disclosed waveform
drift. That result explains the apparent debug slowdown, but it remains bound
to its recorded binary hash and source revision. Regenerate the full matrix
with the intended release artifact before publishing release numbers.
