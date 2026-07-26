# The part of the suite you cannot run

hauksbee was built for a board it cannot publish. That board belongs to Tarski,
not to this project, so its netlists, as-built overlay, firmware images and
synapse map are absent from the public repository, and so is every test that
reads them.

This page exists so the gap is visible. A suite that quietly shrinks reads as a
suite that was always this size, and a tool whose whole argument is that it
refuses to report results it cannot stand behind should not open with a
misleading test count.

## What is missing

**60 tests across 13 files, plus 11 engine examples.**

| Suite | Tests | What it covers |
|---|---|---|
| `tarski_firmware_cosim` | 13 | Firmware co-simulation end to end: real ELF on an emulated MCU driving a 3.4k-device analogue mesh |
| `tarski_bind` | 5 | Binder against a board with socketed and do-not-populate parts |
| `tarski_full` | 4 | Whole-board extract, bind, check, report |
| `tarski_general_e2e` | 4 | The general decomposition layer carrying a real torn solve |
| `tarski_595_chain` | 3 | Shift-register chain decode under firmware |
| `tarski_stretcher_transient` | 3 | Pulse-stretcher transient against measured hardware |
| `flagship_brownout` | 3 | The brownout CI scenario, red and repaired |
| `inhibitory_miswire` | 3 | A miswire found in hardware, then derived from the netlist |
| `asbuilt_equivalence` | 5 | That the declarative as-built overlay reproduces the imperative rework it replaced |
| `boardcode_miswire` | 2 | Round-trip through the board-as-code form |
| `hardware_history` | 2 | Replay against recorded hardware traces |
| `mcp4728_cosim` | 2 | DAC peripheral co-simulation |
| `tarski_decomposition_analysis` | 1 | Tearing choices on a mesh that does not converge fused |

Ten more tests are excised from files that otherwise ship, in
`spec_and_assertions`, `cli_boardcode`, `dnp_processor`, `diode_fallback` and
`extract`. Those files keep the rest of their coverage.

Two engine modules leave with the data, because they encode the board's topology
and its validated rework rather than merely referring to it.

## What this does not cost you

The general machinery those tests exercise is covered independently, against
boards that do ship:

- The decomposition and staged-solve layer: `hauksbee-solve`'s `staged_dc`,
  `staged_property`, `rail_tear`, `power_ramp`, `stretcher_transient` and
  `bjt_physics_torn`.
- Extraction, binding, checks and reporting: the ten demo boards under
  `testdata/boards`, plus the public corpus that `scripts/fetch-corpus.sh`
  fetches. See `CONTRIBUTING.md`.
- Firmware co-simulation: the AVR and ESP demo boards and their firmware, which
  ship in full.

What is genuinely unavailable is the evidence that all of it holds together on
one large, difficult, real board. That claim is made in `README.md` and you are
being asked to take it on trust, which is why it is written down here rather
than left to be inferred from a test count.

## If you have the board

The suites above run unchanged in the development repository. Nothing about them
is special-cased for privacy; they are ordinary tests whose fixtures cannot be
redistributed.
