# Terminal session transcripts

Real captured `hauksbee` / `hauksbee-ci` output for the headline flows. These are
actual runs (not invented); regenerate any of them with the command shown on the
first line of the file.

| File | What it shows |
|---|---|
| `01_report_board.txt` | `hauksbee run --report`: the bind report table (every component bound to a device model, with confidence). |
| `02_drc.txt` | `hauksbee run --drc`: geometric short / clearance check on the blinky board (clean). |
| `03_lint_si_arsenal.txt` | `--lint` and `--si` on the real Olimex ESP32-EVB: a high-severity strap-pin lint finding (a 50 MHz clock on GPIO0 can mis-strap into download mode) and the signal-integrity notes (I2C rise time, antenna keepout, USB diff-pair skew). |
| `04_boot_firmware_headless.txt` | `hauksbee run --firmware --headless`: boots the demo firmware on the emulated ATmega328P, shows the most-active nets and the UART banner. |
| `05_ci_spec_green.txt` | `hauksbee-ci run blinky.toml`: rail + UART + blink + no-faults assertions, all GREEN, exit 0. |
| `06_ci_spec_red.txt` | `hauksbee-ci run tarski_brownout.toml`: the flagship brownout regression failing RED on a fuzzed power-up seed, exit 1. |
| `07_ci_spec_repaired_green.txt` | The same brownout spec with the milliohm-shunt repair: GREEN across all 8 seeds, exit 0. |
| `08_ci_spec_transient.txt` | `hauksbee-ci run olimex_wifi_burst_transient.toml`: a `rail_window` transient assertion riding an ESP32 WiFi burst, GREEN. |
| `09_board_as_code_loop.txt` | The board-as-code loop: `to-code` → `from-code --incremental` → `check-code` on the real stormduino board. |
| `10_doctor.txt` | `scripts/doctor.sh`: the environment report (which backends are present and what each unlocks). |
| `11_boot_coverage_pass_fail.txt` | `boot-coverage` on a Hi-Z MOSFET gate: PASS (firmware drives it in time) and FAIL (firmware leaves it floating), on the built-in AVR backend. |
| `12_miswire_repair_demo.txt` | The Tarski miswire repaired as a Board-as-Code edit: ~689 mA + 3 faults (as-wired) to ~0.42 µA + 0 faults (repaired). |

## Honest notes on the captured output

- The UART lines in `04` and `05` carry raw ANSI colour escapes (e.g. `^[[32m`)
  because the demo firmware prints a coloured banner; that is the real byte
  stream the firmware emitted, shown verbatim.
- `09` prints `avr_sadly_crashed` on stderr before the check report: stormduino
  carries an AVR that the firmware-less `check-code` run halts. It is benign,
  the structural check still completes and reports no faults. Shown as captured.
- `03`/`08` use the corpus board at `board-corpus/famous/olimex_esp32`; they need
  that corpus present.
- The Watchy v1.5 e-paper boot-coverage specs
  (`crates/hauksbee-ci/examples/watchy_v15_display_res*.toml`) need the **Espressif
  fork** of `qemu-system-xtensa` (the upstream Homebrew qemu rejects the `esp32`
  machine), pointed at via `HAUKSBEE_QEMU_XTENSA`. No transcript is captured here
  for them because that fork is not installed in this environment; `11` shows the
  same `boot-coverage` assertion on the built-in AVR backend instead, which needs
  no external emulator.
