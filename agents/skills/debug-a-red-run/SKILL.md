---
name: debug-a-red-run
description: Diagnose a failed Hauksbee run - a RED hauksbee-ci spec, an INVALID (exit 3) result, or a failing `hauksbee run --strict` gate. Use to read the evidence and assumptions, separate a real board fault from a model gap or a spec mistake, and pick the next action.
---

# Debug a RED or INVALID run

First decide which of four outcomes you have. They need different fixes.

| outcome | `hauksbee-ci run` | `hauksbee run --strict` | meaning |
|---|---|---|---|
| GREEN | exit 0 | exit 0 | held, possibly with caveats (`evidence: qualified`) |
| RED | exit 1 | exit 2 | an assertion or gating finding failed |
| input error | exit 2 | exit 1 | the spec, board, or firmware could not be used |
| INVALID | exit 3 | exit 3 | the run refused to vouch for the result |

Never average INVALID into pass or fail, and never retry it expecting a
different answer. It changes only when an input changes.

## 1. Get the machine record

```bash
hauksbee-ci run spec.toml --json > run.json; echo $?
```

Read `passed`, `assertions_passed`, `run_valid`, and `exit_code` together.
Then, for each `results[]` entry with `passed: false` or `invalid: true`, read
`label`, `kind`, `detail`, `failing_seeds`, and `evidence`.

## 2. Read the evidence line

Every assertion prints an `evidence:` status and the assumptions under it.

- `clean`: no caveats on this assertion's causal path.
- `qualified`: a documented caveat that does not change the verdict, such as
  `[reduced-fidelity:R1-theta-ja]` (a theta_JA inferred from the package).
  A RED with `qualified` evidence is still RED. Report the caveat.
- `undermined`: the verdict is withheld (INVALID, exit 3). The two common
  causes are `[open-part:U1]`, an unmodelled active part that can drive the
  asserted net (printed as `co-sim COVERAGE HOLE`), and
  `operating_limit_violated`, a datasheet requirement the run measured as
  broken.

Each assumption line has `Why:`, `Effect:`, and `Fix:`. Quote the `Fix:`.

An INVALID run also prints a refusal block:

```text
refused claim: an overall CI verdict for the requested assertions
missing prerequisite: ...
valid partial conclusions: ...
next action: ...
```

## 3. Classify the failure

- **Real board fault.** A clean or qualified RED whose `detail` names a
  physical cause, for example `Tj(R1) peak 156.7 C > ceiling 125 C`, or
  `control net 'GATE_CTRL' was never driven to >= 3 V`. Report it with the
  measured value and the `why:` line.
- **Model gap.** INVALID with `open-part`, or `INCONCLUSIVE:` notes from
  `hauksbee run`. Run `hauksbee models coverage BOARD` and hand off to the
  `model-a-hardware-board` skill. Do not invent a model to make it pass.
- **Spec mistake.** Exit 2, or a RED caused by the spec itself: a wrong net, a
  window before the rail settles (`after_ms`), a toggle tolerance tighter than
  the firmware's timing, or firmware that is not the image you meant. Run
  `hauksbee-ci check spec.toml` first; it reports unknown nets and refs with
  suggestions.
- **Missing backend.** `hauksbee run --firmware` exits 1 with `Renode not
  found` or a QEMU message. `hauksbee doctor` shows what is installed, and
  `hauksbee install renode` or `hauksbee install esp-qemu` installs it.
- **Possible Hauksbee false positive.** A RED on a board the user has built
  and measured as working. Say so plainly and keep the reproduction.

## 4. Narrow it down

```bash
hauksbee-ci run spec.toml --seed 8                 # rerun one failing ensemble seed exactly
hauksbee-ci run spec.toml --waveforms out/         # CSV of every asserted net, per seed
hauksbee run BOARD --firmware fw.elf --headless --seconds 0.5 \
  --probe D13,+5V --probe-csv probe.csv            # raw waveforms without a spec
hauksbee run BOARD --report --plain                # what each part bound to
hauksbee run BOARD --list-nets                     # exact net names
```

For a "never driven" or all-zero co-sim result, check that the firmware image
matches the MCU, that the net is a GPIO the firmware drives, and that
`--report` shows no passive with an absurd value. `docs/cosim/MCU.md` has a
fidelity-notes section on this.

## 5. Hand off

Keep a replayable record of the failure:

```bash
hauksbee-ci run spec.toml --emit-manifest red.manifest.json
hauksbee reproduce red.manifest.json               # same verdict, same exit code
```

Report: the outcome class, the exit code, the failing assertion and its
measured value, the evidence status and assumptions, the class from step 3,
and one next action. If the user judges a failure acceptable, the route is a
`hauksbee-waivers.toml` entry with `reason` and `until`, not a deleted
assertion.

Bundled examples to practise on (run from a repository checkout):

```bash
hauksbee-ci run crates/hauksbee-ci/examples/boot_gate_fail.toml      # RED, exit 1
hauksbee-ci run crates/hauksbee-ci/examples/power_resistor_hot.toml  # RED, qualified evidence
hauksbee-ci run crates/hauksbee-ci/examples/tolerance_divider.toml   # RED on 5 of 24 seeds
hauksbee-ci run crates/hauksbee-ci/tests/fixtures/exit3/spec.toml    # INVALID, exit 3
```
