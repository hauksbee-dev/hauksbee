# hauksbee-ci spec examples

A hauksbee-ci spec is a TOML file a hardware repo checks in: it describes one
headless co-simulation and the assertions that must hold for the build to pass.
Run one with:

```bash
hauksbee-ci run <spec.toml>            # exit 0 = GREEN, 1 = RED, 2 = bad spec, 3 = INVALID
hauksbee-ci run <spec.toml> --junit results.xml
```

Full spec reference: [`docs/ci/CI.md`](../../docs/ci/CI.md). Wire it into GitHub
Actions via [`integrations/github-action`](../../integrations/github-action) or
run it from any pipeline with [`scripts/ci.sh`](../../scripts/ci.sh).

## Public example specs

### Rail + UART + blink assertions (GREEN)

Boot the demo firmware on a small ATmega328P board and assert the three things
you would otherwise check by hand on the bench: the 5 V rail holds, the firmware
prints its banner over UART, and the D13 LED blinks at ~5 Hz.

```bash
hauksbee-ci run crates/hauksbee-ci/examples/blinky.toml
echo $?    # 0: rail >= 4.75 V, UART contains "hauksbee-demo v1", D13 ~5 Hz, no faults
```

Captured run: [green](../sessions/05_ci_spec_green.txt). This is the spec to
copy as a template: it uses `voltage`, `uart`, `toggle` and `no_faults`
assertions and a `usb` supply.

### Scenario / transient spec (GREEN)

[`olimex_wifi_burst_transient.toml`](./olimex_wifi_burst_transient.toml) (in this
directory). A static DC check cannot see a rail sag, because a sag only happens
during a current transient. This attaches a periodic 240 mA ESP32 WiFi-TX burst
to the +3.3V rail (with honest capacitor ESR/ESL turned on) and uses a
`rail_window` assertion to judge the rail over the burst window.

```bash
hauksbee-ci run examples/ci-specs/olimex_wifi_burst_transient.toml
echo $?    # 0: the stiff wall supply rides the burst (min stays ~3.27 V)
```

Captured run: [transient green](../sessions/08_ci_spec_transient.txt). It is the
calibration counterpart to a brownout: it passing on a robust supply is what
lets a genuine brownout on a weak supply be trusted as a real failure.

## Other shipped specs (in `crates/hauksbee-ci/examples/`)

| Spec | Demonstrates | Verdict |
|---|---|---|
| `boot_gate_pass.toml` / `boot_gate_fail.toml` | `boot_coverage`: does the firmware drive a Hi-Z MOSFET gate in time? | PASS (exit 0) / FAIL (exit 1) |
| `watchy_v15_display_res.toml` / `_undriven.toml` | `boot_coverage` on the real Watchy v1.5 e-paper RES# net (ESP32 QEMU backend) | PASS (exit 0) / FAIL (exit 1) |
| `pic_programmer_schematic.toml` | schematic-stage CI: assert a `.kicad_sch` before any PCB exists. Judges the rail ahead of the 7805 and the input diode only; the board's LT1373, 24Cxx and PIC sockets have no model, so VCC and `no_faults` would be INVALID and are left out. Both assertions pass; the run is RED only on the demo board's own `C4` (a capacitor position with value `0`) | FAIL, expected (exit 1) |
| `watchy.toml` | the first spec on a fresh clone: the vendored SQFMI Watchy board, intentionally unmodelled parts on +3V3 | INVALID (exit 3) until models are supplied |
| `power_resistor_cool.toml` / `power_resistor_hot.toml` | `max_temp`: a power resistor within, then past, its thermal limit at 90 C ambient | PASS (exit 0) / FAIL (exit 1) |
| `tolerance_divider.toml` | a `[[tolerance]]` ensemble: the nominal seed passes, the ensemble goes RED and names the failing seed | FAIL (exit 1) |
| `tolerance_divider_corners.toml` | `mode = "corners"`: a wide window holds on all four corners, a tight one fails and names the corner | FAIL (exit 1) |

A default (AVR-enabled) source build's bundle rewrites `boot_gate_pass.toml`
beside a small packaged AVR fixture, so that PASS example runs directly from an
extracted default bundle. The published permissive bundle has no avr backend, so
it packages neither the fixture nor the spec. `boot_gate_pass.toml`,
`boot_gate_fail.toml`, and the other firmware-heavy examples
all reference `testdata/`; run those from a hauksbee repository checkout.
The Watchy specs need an ESP32 QEMU (`HAUKSBEE_QEMU_XTENSA`); run
[`scripts/doctor.sh`](../../scripts/doctor.sh) to see what is present.
