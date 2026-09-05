# MCU internal resource-conflict check

Two board functions can be wired to *different* MCU pins that map to the
*same* internal resource instance (one PWM slice+channel, one fixed QSPI pin
group), so the chip cannot serve both. The netlist is clean; only the MCU's
internal peripheral-to-pin binding reveals it. This check carries that binding
as a reference-manual-cited table and reasons over it.

```
hauksbee run <board> --resources   # this check only
hauksbee run <board> --lint        # connectivity lint + this check
hauksbee run <board> --check       # everything
```

Findings are `mcu_resource_conflict` entries in the lint report. Source:
`crates/hauksbee-extract/src/resource_conflict.rs`; table:
`crates/hauksbee-extract/db/mcu_resources.toml`.

## The resource map

A per-MCU table: a part matcher (symbol part name plus a `min_pins` guard, so
a loose name cannot match a buffer or a mounting hole) and a
`pad -> { resource bindings }` map keyed by **pad number** as the extracted
board carries it (netlists and layouts carry pad numbers, not pin functions).

| MCU | Resource | Rule |
|---|---|---|
| RP2040 (Pico module and bare QFN-56) | PWM slice/channel | GPIO n is slice `(n >> 1) & 7`, channel A if n even, B if odd (datasheet 4.5.2). One channel drives one pin |
| SAMD51 (TQFP64) | QSPI pin group | QSPI is pin-locked to PA08..PA11 (DATA0..3), PB10 (SCK), PB11 (CS) (SAM D5x Table 6-1, function H) |
| ESP32 | none | the GPIO matrix routes almost everything anywhere; marked `fully_routable = true`, and the check skips the part |

Other classes (SERCOM instances, STM32 TIMx_CHy sharing, ADC instances) fit
the same shape but are not authored.

## How a finding is decided

1. **Identify the MCU** against the table.
2. **Infer each resource pin's function** by tracing its signal net to an
   unambiguous target (HDMI/DVI connector, audio jack, flash chip) through
   series passives and small line buffers, never across a power/ground net
   and never back into the MCU. The evidence chain is recorded in the finding.
3. **Decide the peripheral**: reaching an HDMI connector is a PWM demand only
   for the pixel clock (`CK`/`CLK` net); TMDS data lanes are PIO and DDC is
   I2C.
4. **Check feasibility**: a PWM slice+channel demanded by two distinct
   functions is **High**. A QSPI group with >= 2 pads carrying two different
   non-QSPI functions is **High**. A group whose committed pads all belong to
   one self-consistent function is **Low** (nothing contends at runtime; the
   copper only proves QSPI can never reach those pads). No medium tier.

The SAMD51 discriminator: a flash on PA08..PA11 driven as quad-IO (`IO0..IO3`)
with SCK/CS on PB10/PB11 is the correct QSPI wiring and is silent. The check
fires only when a 4-wire-SPI-role net (`MOSI`/`MISO`/`SCK`/`CS`) lands on a
QSPI DATA pad.

Example (RP2040 carrier with PicoDVI clock on GP12 and PWM audio on GP28,
both slice 6A):

```
[high] mcu_resource_conflict - RP2040_PLATFORM1: two functions demand RP2040 PWM slice/channel 6A, which can serve only one pin at a time [RP2040 datasheet 4.5.2 ...]: PWM audio (GP28 pad 34, net '/PWM_L': ... reaches AUDIO_JACK_1); and PicoDVI PWM pixel clock (GP12 pad 16, net '/PICO_CK-': ... reaches HDMI1)
```

A `[low]` finding is not gate-grade: `--strict` stays green on it.

## Limits

- Function inference is deliberately narrow: a peripheral reached through an
  active codec, an FPGA or an unknown part is not attributed, and the check
  stays silent.
- PicoDVI clock vs data is decided by net naming (`CK`/`CLK`).
- Pad tables assume the authored package (Pico 40-pin castellation, TQFP64).
  A different package of the same die needs its own map; a mismatch is a
  silent miss, never a false positive.
- Only PWM-slice and QSPI-group instances are populated.

Audit tool: `cargo run -p hauksbee-extract --example resource_probe <board>`
dumps the per-pad inferred function and the table match.
