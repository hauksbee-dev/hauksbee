# Oracles: cross-checking hauksbee against ground-truth tools

> For the full capability map of hauksbee (including which layers use oracles, which run firmware, and which are commodity vs. differentiated) see [`docs/about/CAPABILITIES.md`](../about/CAPABILITIES.md).

hauksbee runs its own DRC, analog solve, and co-sim from the layout. To keep
these honest, we cross-check the results against independent, authoritative
tools we call **oracles**. hauksbee uses an oracle only for *verification*,
never at runtime: hauksbee's whole point is that it needs no EDA tool to
produce a verdict. The oracle is how we prove that verdict is right, and how
the Tarski meta-lesson ("presume a hauksbee defect until disproven") gets
judged.

## Why oracles are NOT bundled into hauksbee

KiCad is a large GPL-3.0 EDA suite. Vendoring it into hauksbee's distribution
would bloat the package, impose GPL redistribution terms on hauksbee, and
undercut the core premise: DRC from the copper, no EDA needed. So hauksbee
**detects** an existing oracle install and uses it on demand.
"Bundling the oracle" means *wiring it in cleanly*, not shipping the binary.

## The DRC oracle: `kicad-cli`

KiCad's own `kicad-cli pcb drc` is the ground truth for geometric DRC.

**hauksbee auto-detects it** (`find_kicad_cli` in `hauksbee-engine`). It
checks `kicad-cli` on `PATH` and standard platform application locations, and
**prefers the highest version** (a KiCad-10 CLI is required to read the
`version 20260206` board format; KiCad 9's CLI fails to load those files with
"Failed to load board").

**Use it** with `--oracle` on a DRC run:

```
hauksbee run board.kicad_pcb --drc --oracle
```

It prints hauksbee's result, then runs `kicad-cli pcb drc` and a one-line
cross-check. The cross-check is a text surface only: `--oracle` combined with
`--json` does nothing, because the oracle's verdict has no place in the JSON
schema, so `kicad-cli` is never invoked at all
(`crates/hauksbee-engine/src/reports/drc.rs`). Run the text form when you want
the cross-check.

"A short" means different things in each tool. hauksbee reconciles the two
honestly:

- hauksbee: copper of two nets at gap <= 0 (touching).
- KiCad: a `shorting_items` violation (connectivity merged the nets) **or** a
  `clearance`/`hole_clearance` at actual ~0 mm (geometrically touching, not
  merged). Both count toward the oracle's "touching-copper" total. KiCad's
  other violations (annular ring, solder-mask bridge, courtyard, positive
  sub-rule clearance) are not net shorts and do not count.

The counts do not map 1:1 (the tools split one touch into different numbers of
rows), so the verdict is about **presence and over-reporting**, not exact
equality: "agree", "hauksbee likely over-reports (N >> M)", "likely hauksbee
false positives", or "hauksbee may be missing shorts".

Worked example, on a board that ships in this repo so you can rerun it:

```
$ hauksbee run crates/hauksbee-ci/examples/boards/boot_gate.kicad_pcb --drc --oracle
note: this board has no routed copper (no track segments): the spacing check had only pads to compare, so a clean result here says nothing about routing that does not exist yet.
DRC: 20 primitive(s), clearance rule 0.200 mm

SHORTS (2):
  [SERIOUS] GND touches +5V on B.Cu (gap 0.0000 mm) at x=112.0, y=100.0
  [SERIOUS] GND touches +5V on F.Cu (gap 0.0000 mm) at x=112.0, y=100.0

2 short(s), 0 below-rule group(s), 0 at-limit group(s).

oracle (kicad-cli 10.0.3): 7 touching-copper violation(s), 26 total DRC violation(s), 6 unconnected.
hauksbee: 2 short(s), 0 clearance. -> agree: both find touching copper (2 hauksbee / 7 oracle; counts differ by decomposition).
note: gate-grade finding(s) above, but this is a report command so the exit code is 0. Add --strict to exit 2 on them (exit contract: 0 = clean or report-only, 1 = input error such as a missing or unreadable file, 2 = findings under --strict, 3 = invalid for analysis), or gate CI with hauksbee-ci.
```

That is the decomposition point made concrete. One deliberate GND-to-+5V touch
spanning both copper layers, and the two tools slice it into 2 rows and 7 rows
respectively, yet the verdict is **agree**, because both place touching copper on
the same nets. Reading the counts as a pass/fail equality would have called this
a disagreement; reading them for presence and over-reporting calls it what it is.

That decomposition gap is also how a residual false-positive count gets
adjudicated on a large board: when hauksbee's total runs an order of magnitude
above the oracle's, the excess is hauksbee artifact rather than silicon, and the
verdict string says so instead of averaging the two numbers into a fake
agreement.

## Installing the KiCad oracle

KiCad 10 stable is in Homebrew:

```
brew install --cask kicad          # needs sudo for the /Library demos artifact
```

If you cannot give sudo (for example in a headless or sandboxed environment),
install the app in a user-writable directory from the downloaded package. The
auto-detector searches `PATH` and standard application locations; verify the
resolved tool with `hauksbee doctor --backends`.

```
DMG=/path/to/KiCad.dmg
hdiutil attach "$DMG" -nobrowse -mountpoint /tmp/kicad-mnt
mkdir -p "$HOME/Applications"
cp -R /tmp/kicad-mnt/KiCad/KiCad.app "$HOME/Applications/"
hdiutil detach /tmp/kicad-mnt -force
```

## The analog oracle: `ngspice`

For the solver, the oracle is **ngspice**: we cross-check hauksbee's
transient/AC results against ngspice to fractions of a percent, and every
speed claim is gated behind one of these accuracy checks (see
`docs/about/COMPARISON.md`). ngspice is a small, BSD-licensed CLI. hauksbee
invokes it the same way: detected, used for validation, never bundled or
required at runtime.

## The MCU simulator backends follow the same pattern

Renode and the Espressif QEMU fork are not bundled for the same reasons: they
are large, GPL/EPL-licensed, and hauksbee's co-sim needs only a running
process on a TCP socket, not a linked library. hauksbee detects whichever is
installed and uses it on demand. Tests skip cleanly when neither is present.

Install instructions, the discovery order, and env-var overrides are in
[`docs/cosim/SIMULATORS.md`](SIMULATORS.md). The one-command installer is
`scripts/install-sims.sh`.

## Adding oracles

The pattern for any future oracle (a field solver for impedance, a thermal
solver): detect an existing install, use it to validate a specific hauksbee
output, report agreement honestly, and never make it a runtime dependency.
