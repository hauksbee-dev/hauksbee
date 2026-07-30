# Bundled samples for the landing page's "no board handy?" chips

Staged here because Vite's `public/` must hold real files.

Mostly copies of repo fixtures, with one deliberate exception. A sample is the
first board a stranger ever sees, and a test fixture is chosen to fail in a
specific way. Those two jobs pull apart, so where they conflict the sample wins
here and the fixture stays as it is in the crate.

`boot_gate.kicad_pcb` is the case: the fixture carries two deliberate GND/+5V
shorts, because `crates/hauksbee-engine/tests/waiver_gate.rs` asserts it gates
red before any waiver exists, and that premise is what stops the waiver tests
passing vacuously. The sample is the same board routed clean, so a first run
shows the co-simulation rather than a red verdict. Both bind identically: four
parts, one active IC, the MCU found.

| File | Source | Why this sample |
|---|---|---|
| `blinky.kicad_pcb` | `crates/hauksbee-ci/examples/boards/` | Small clean board: a healthy report in seconds. Routed with hauksbee's own board-as-code plus freerouting path, so it has real copper and a board outline rather than floating pads. |
| `watchy.kicad_pcb` + `watchy.LICENSE` | `crates/hauksbee-ci/examples/boards/` | A real shipped product (sqfmi Watchy): what a report looks like on a non-trivial board. |
| `boot_gate.kicad_pcb` + `boot_gate.hex` | examples boards + `testdata/firmware/boot_gate_a/` | Board + firmware pair: the co-sim demo. The firmware drives a MOSFET gate at power-up, and the report says so. |
