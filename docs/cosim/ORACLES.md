# Oracles: cross-checking against ground-truth tools

Hauksbee produces its own DRC, analog solve and co-sim verdicts without any
EDA tool. An **oracle** is an independent tool used only to *verify* a result.
Oracles are detected on the machine and used on demand; none is bundled or
required at runtime.

## DRC oracle: `kicad-cli`

```
hauksbee run board.kicad_pcb --drc --oracle
```

Prints Hauksbee's DRC result, runs `kicad-cli pcb drc`, and prints a one-line
cross-check. `--oracle` is a no-op without `--drc`/`--check`, and does nothing
under `--json` (the oracle verdict has no place in the JSON schema, so
`kicad-cli` is not invoked).

Discovery: `kicad-cli` on `PATH`, then
`*/Applications/KiCad*/{,KiCad.app/}Contents/MacOS/kicad-cli` and the
Linux/Homebrew locations; the highest version wins. KiCad 10's cli is needed
for `version 20260206` boards.

"A short" differs between the tools: Hauksbee counts copper of two nets at
gap <= 0; KiCad counts `shorting_items` plus `clearance`/`hole_clearance` at
~0 mm. Other KiCad violations (annular ring, mask bridge, courtyard) are not
counted. Counts do not map 1:1 (one touch decomposes into different numbers of
rows), so the verdict is one of `agree`, `hauksbee likely over-reports`,
`likely hauksbee false positives`, or `hauksbee may be missing shorts`.

```
oracle (kicad-cli 10.0.3): 7 touching-copper violation(s), 26 total DRC violation(s), 6 unconnected.
hauksbee: 2 short(s), 0 clearance. -> agree: both find touching copper (2 hauksbee / 7 oracle; counts differ by decomposition).
```

Install: `brew install --cask kicad`, or copy `KiCad.app` from the cached dmg
into `~/Applications/KiCad10/` (found automatically, no PATH change).

## Analog oracle: `ngspice`

Transient and AC results are cross-checked against ngspice during development.
ngspice is never invoked at runtime.

## Simulator backends

Renode and Espressif QEMU follow the same pattern: detected, run as separate
processes over sockets, never linked. See [SIMULATORS.md](SIMULATORS.md).
