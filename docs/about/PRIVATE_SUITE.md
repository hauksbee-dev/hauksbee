# The part of the suite you cannot run

hauksbee was built for a board it cannot publish. That board belongs to
Tarski, not to this project. So its netlists, as-built overlay, firmware
images, and synapse map are absent from the release mirror, and so is
every test that reads them.

This page exists to make the gap visible. A suite that quietly shrinks reads
as a suite that was always this size, and a tool whose whole argument is that
it refuses to report results it cannot stand behind should not open with a
misleading test count.

## What is missing

**66 tests: 56 in the 15 absent files below, plus 10 removed from files that
otherwise ship. Also absent: 11 engine examples.**

| Suite | Tests | What it covers |
|---|---|---|
| `tarski_firmware_cosim` | 13 | Firmware co-simulation end to end: real ELF on an emulated MCU driving a 3.4k-device analogue mesh |
| `tarski_bind` | 5 | Binder against a board with socketed and do-not-populate parts |
| `tarski_full` | 4 | Whole-board extract, bind, check, report |
| `tarski_general_e2e` | 4 | The general decomposition layer carrying a real torn solve |
| `tarski_private_acceptance` | 4 | Runtime-only release-candidate bind, preparation certificate, firmware transport and decomposed inference acceptance without publishing the input |
| `tarski_595_chain` | 3 | Shift-register chain decode under firmware |
| `tarski_revision_identity` | 2 | As-built preparation, cut selection and weight routing surviving KiCad re-annotation |
| `tarski_stretcher_transient` | 3 | Pulse-stretcher transient against measured hardware |
| `flagship_brownout` | 3 | The brownout CI scenario, red and repaired |
| `inhibitory_miswire` | 3 | A miswire found in hardware, then derived from the netlist |
| `asbuilt_equivalence` | 5 | That the declarative as-built overlay reproduces the imperative rework it replaced |
| `boardcode_miswire` | 2 | Round-trip through the board-as-code form |
| `hardware_history` | 2 | Replay against recorded hardware traces |
| `mcp4728_cosim` | 2 | DAC peripheral co-simulation |
| `tarski_decomposition_analysis` | 1 | Tearing choices on a mesh that does not converge fused |

The ten tests missing from files that otherwise ship are in
`spec_and_assertions`, `cli_boardcode`, `dnp_processor`, `diode_fallback` and
`extract`. Those files keep the rest of their coverage.

Two engine modules leave with the data, because they encode the board's
topology and its validated rework instead of merely referring to it.

## What this does not cost you

Independent coverage exercises the same general machinery, against
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
one large, difficult, real board. `README.md` makes that claim, and it asks
you to take it on trust. That is why this page states it plainly instead of
leaving it to be inferred from a test count.

## If you have the board

The suites above run unchanged in the development repository. Nothing about
them is special-cased for privacy. They are ordinary tests whose fixtures
cannot be redistributed.
