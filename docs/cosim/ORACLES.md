# Oracles: cross-checking hauksbee against ground-truth tools

> For the full capability map of hauksbee (including which layers use oracles, which run firmware, and which are commodity vs. differentiated) see [`docs/about/CAPABILITIES.md`](../about/CAPABILITIES.md).

hauksbee does its own DRC, analog solve and co-sim from the layout. To keep it
honest we cross-check its results against independent, authoritative tools we call
**oracles**. An oracle is used for *verification*, never at runtime: hauksbee's
whole point is that it needs no EDA tool to produce a verdict. The oracle is how we
prove that verdict is right (and how the Tarski meta-lesson "presume a hauksbee
defect until disproven" gets adjudicated).

## Why oracles are NOT bundled into hauksbee

KiCad is a ~1.4 GB GPL-3.0 EDA suite. Vendoring it into hauksbee's distribution
would bloat it, impose GPL redistribution terms on hauksbee, and undercut the core
premise (DRC from the copper, no EDA needed). So hauksbee **detects** an existing
oracle install and uses it on demand. "Bundling the oracle" means *wiring it in
seamlessly*, not shipping the binary.

## The DRC oracle: `kicad-cli`

KiCad's own `kicad-cli pcb drc` is the ground truth for geometric DRC.

**hauksbee auto-detects it** (`find_kicad_cli` in `hauksbee-engine`): it checks
`kicad-cli` on `PATH`, then `*/Applications/KiCad*/{,KiCad.app/}Contents/MacOS/kicad-cli`
and the Linux/Homebrew locations, and **prefers the highest version** (a KiCad-10
cli is required to read the `version 20260206` board format; KiCad 9's cli fails to
load those files with "Failed to load board").

**Use it** with `--oracle` on a DRC run:

```
hauksbee run board.kicad_pcb --drc --oracle
```

It prints hauksbee's result, then runs `kicad-cli pcb drc` and a one-line
cross-check. "A short" means different things in each tool, reconciled honestly:

- hauksbee: copper of two nets at gap <= 0 (touching).
- KiCad: a `shorting_items` violation (connectivity merged the nets) **or** a
  `clearance`/`hole_clearance` at actual ~0 mm (geometrically touching, not merged).
  Both are counted as the oracle's "touching-copper" total. KiCad's other
  violations (annular ring, solder-mask bridge, courtyard, positive sub-rule
  clearance) are not net shorts and are not counted.

The counts do not map 1:1 (the tools split one touch into different numbers of
rows), so the verdict is about **presence and over-reporting**, not exact equality:
"agree", "hauksbee likely over-reports (N >> M)", "likely hauksbee false positives",
or "hauksbee may be missing shorts".

Worked examples (this repo's hunt boards):

| Board | hauksbee | oracle (kicad-cli 10.0.3) | verdict |
|-------|----------|---------------------------|---------|
| `bms-prototype` (REG1_3V3↔GND) | 4 shorts | 5 touching-copper | **agree**: real short confirmed |
| `VENDETTAESC` (KiCad 10) | 121 shorts | 12 touching-copper (10 `shorting_items`) | hauksbee over-reports ~109 (the U12 GND-pad antipad artifacts); 10 are real |
| `FUSB302Breakout` | 0 | 0 | **agree**: clean |

That ESC line is exactly how the residual false-positive count was adjudicated: the
oracle confirms ~10-12 genuine touches, so the other ~109 are hauksbee artifacts.

## Installing the KiCad oracle

KiCad 10 stable is in Homebrew:

```
brew install --cask kicad          # needs sudo for the /Library demos artifact
```

If you cannot give sudo (headless / sandbox), install just the app from the cached
dmg with no sudo (this is what is installed on this machine, at
`~/Applications/KiCad10/KiCad.app`, leaving any existing `/Applications/KiCad`
untouched):

```
DMG=$(brew --cache --cask kicad)
hdiutil attach "$DMG" -nobrowse -mountpoint /tmp/kicad10mnt
mkdir -p ~/Applications/KiCad10
cp -R /tmp/kicad10mnt/KiCad/KiCad.app ~/Applications/KiCad10/
hdiutil detach /tmp/kicad10mnt -force
```

hauksbee's auto-detect finds it in `~/Applications` automatically; no PATH change
needed.

## The analog oracle: `ngspice`

For the solver, the oracle is **ngspice**: hauksbee's transient/AC results are
cross-checked against ngspice to fractions of a percent, and every speed claim is
gated behind one of these accuracy checks (see `docs/about/COMPARISON.md`). ngspice is a
small, BSD-licensed CLI and is invoked the same way, detected, used for
validation, not bundled or required at runtime.

## The MCU simulator backends follow the same pattern

Renode and the Espressif QEMU fork are not bundled for the same reasons: they
are large, GPL/EPL-licensed, and hauksbee's co-sim needs only a running process
on a TCP socket, not a linked library. hauksbee detects whichever is installed
and uses it on demand; tests skip cleanly when neither is present.

Install instructions, the discovery order, and env-var overrides are in
[`docs/cosim/SIMULATORS.md`](SIMULATORS.md). The one-command installer is
`scripts/install-sims.sh`.

## Adding oracles

The pattern for any future oracle (a field solver for impedance, a thermal solver):
detect an existing install, use it to validate a specific hauksbee output, report
agreement honestly, and never make it a runtime dependency.
