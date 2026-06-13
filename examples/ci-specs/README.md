# hauksbee-ci spec examples

A hauksbee-ci spec is a TOML file a hardware repo checks in: it describes one
headless co-simulation and the assertions that must hold for the build to pass.
Run one with:

```bash
hauksbee-ci run <spec.toml>            # exit 0 = GREEN, 1 = RED, 2 = bad spec
hauksbee-ci run <spec.toml> --junit results.xml
```

Full spec reference: [`docs/CI.md`](../../docs/CI.md). Wire it into GitHub
Actions via [`integrations/github-action`](../../integrations/github-action) or
run it from any pipeline with [`scripts/ci.sh`](../../scripts/ci.sh).

## The three headline specs

### 1. Brownout regression (the flagship: RED, then GREEN after the repair)

The Tarski power-up brownout: the analogue rail is fed through a 1 kΩ part that
should have been milliohms, so one undefined power-up register bit collapses the
whole rail. Fuzzed across power-up states, at least one seed fails.

```bash
hauksbee-ci run crates/hauksbee-ci/examples/tarski_brownout.toml
echo $?    # 1: ANALOG_VDD collapses from ~4.96 V to ~0.76 V on a fuzzed seed

hauksbee-ci run crates/hauksbee-ci/examples/tarski_brownout_repaired.toml
echo $?    # 0: the milliohm-shunt repair holds the rail across all 8 seeds
```

Captured runs:
[red](../sessions/06_ci_spec_red.txt) ·
[repaired green](../sessions/07_ci_spec_repaired_green.txt).

### 2. Rail + UART + blink assertions (GREEN)

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

### 3. Scenario / transient spec (GREEN)

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
| `boot_gate_pass.toml` / `boot_gate_fail.toml` | `boot-coverage`: does the firmware drive a Hi-Z MOSFET gate in time? | PASS / FAIL |
| `watchy_v15_display_res.toml` / `_undriven.toml` | `boot-coverage` on the real Watchy v1.5 e-paper RES# net (ESP32 QEMU backend) | PASS / FAIL |
| `pic_programmer_schematic.toml` | schematic-stage CI: assert a `.kicad_sch` before any PCB exists | PASS |

The boot-coverage and brownout specs reference firmware/netlists under
`testdata/`, so run them from a hauksbee repo checkout (not the binary bundle).
The Watchy specs need an ESP32 QEMU (`HAUKSBEE_QEMU_XTENSA`); run
[`scripts/doctor.sh`](../../scripts/doctor.sh) to see what is present.
