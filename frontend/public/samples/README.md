# Bundled samples for the landing page's "no board handy?" chips

Copies of repo fixtures, staged here because Vite's `public/` must hold real
files. If a source changes, re-copy it.

| File | Source | Why this sample |
|---|---|---|
| `blinky.kicad_pcb` | `crates/hauksbee-ci/examples/boards/` | Small clean board: a healthy report in seconds. |
| `watchy.kicad_pcb` + `watchy.LICENSE` | `crates/hauksbee-ci/examples/boards/` | A real shipped product (sqfmi Watchy): what a report looks like on a non-trivial board. |
| `boot_gate.kicad_pcb` + `boot_gate.hex` | examples boards + `testdata/firmware/boot_gate_a/` | Board + firmware pair: the co-sim demo. The firmware drives a MOSFET gate at power-up, and the report says so. |
