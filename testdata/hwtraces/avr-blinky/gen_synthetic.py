#!/usr/bin/env python3
"""Generator for the SYNTHETIC avr-blinky seed traces.

These captures are NOT from an instrument. They are constructed from the demo
firmware's documented behavior (testdata/firmware/demo: toggle D13 every
100 ms) plus datasheet-typical hardware artifacts a real capture would carry:

  - high level 4.70 V, not 5.00 (USB rail sag + AVR VOH drop driving the LED;
    datasheet VOH is >= 4.2 V at 20 mA / 5 V VCC, typically ~4.6-4.8 V)
  - low level 0.04 V (driver saturation)
  - +/-0.5 ms edge jitter (RC-oscillator MCU clock tolerance)
  - 12 mV rms additive noise (probe + frontend)
  - a 3 ms startup delay before the first edge (reset + banner)

They exist to prove the hwtrace harness end-to-end (trace.toml -> loader ->
feature extraction -> comparison -> report). Their trace.toml files say
`provenance = "synthetic"`; the tier's validation value begins only when a
real capture replaces them. Deterministic (fixed seed) so the committed files
are reproducible byte-for-byte.

Run from this directory:  python3 gen_synthetic.py
Writes: led-blink-scope/d13.csv  led-blink-la/d13.vcd
"""

import random

random.seed(20260708)

TOTAL_S = 1.0
HALF_PERIOD_S = 0.100  # firmware toggles every 100 ms
START_S = 0.003        # reset + UART banner before the first edge
HIGH_V = 4.70
LOW_V = 0.04
JITTER_S = 0.0005
NOISE_V = 0.012
DT_S = 0.0002          # 5 kSa/s scope-style sampling


def edge_times():
    """Toggle instants with per-edge jitter, starting LOW then first edge up."""
    times = []
    t = START_S
    while t < TOTAL_S:
        times.append(t + random.uniform(-JITTER_S, JITTER_S))
        t += HALF_PERIOD_S
    return times


def write_csv(path, edges):
    lines = [
        "# synthetic scope export - see gen_synthetic.py (provenance: synthetic)",
        "time_s,volts",
    ]
    t = 0.0
    while t <= TOTAL_S:
        # Level: LOW before first edge; edges alternate up/down after that.
        n = sum(1 for e in edges if e <= t)
        level = HIGH_V if n % 2 == 1 else LOW_V
        v = level + random.gauss(0.0, NOISE_V)
        lines.append(f"{t:.6f},{v:.5f}")
        t += DT_S
    with open(path, "w") as f:
        f.write("\n".join(lines) + "\n")
    print(f"wrote {path} ({len(lines) - 2} samples)")


def write_vcd(path, edges):
    out = [
        "$comment synthetic logic-analyzer export - see gen_synthetic.py $end",
        "$timescale 1us $end",
        "$scope module capture $end",
        "$var wire 1 ! D13 $end",
        "$upscope $end",
        "$enddefinitions $end",
        "#0",
        "0!",
    ]
    val = 0
    for e in edges:
        val ^= 1
        out.append(f"#{int(round(e * 1e6))}")
        out.append(f"{val}!")
    out.append(f"#{int(TOTAL_S * 1e6)}")
    with open(path, "w") as f:
        f.write("\n".join(out) + "\n")
    print(f"wrote {path} ({len(edges)} edges)")


if __name__ == "__main__":
    write_csv("led-blink-scope/d13.csv", edge_times())
    write_vcd("led-blink-la/d13.vcd", edge_times())
