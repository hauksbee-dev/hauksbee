# Observations: the bench view of a co-sim run

A co-sim run behaves like the board on a bench. Hauksbee shows what you would
see with a scope, a logic analyser and a serial adapter on it: what each pin
does, whether the core is running, what the serial port said and which bus
transactions crossed. You can then notice that an output is wrong and work back
from it to the net, the part and the firmware.

The same report feeds the terminal, the app strip, `--vcd`, `--bus-log` and the
JSON (`cosim.bench`). The words are defined once, in `hauksbee-ir`
(`observations.rs`), and the app reads them from the generated label table.

## Reading the report

`hauksbee run board.kicad_pcb --firmware fw.elf` prints one block per MCU. Each
pin row is a net on the board, so it can be clicked in the app (the net is
highlighted on the board and its lane opens in the scope).

```
bench view: what the board did over 1.00 s

U1  (renode:stm32f103)  running
  PC13   PC13_LED                 toggling 5.05 Hz, 49% duty
  PA5    PA5_OUT                  static high, set at 10.0 ms
  PA9    (unwired)                configured as an output, never changed
  UART   (net unknown)            "hello from stm32" (18 bytes, 100% text)
         (the baud rate is not read from the peripheral on this backend)

timeline:
       0 ns  U1   first serial output: "hello from stm32"
    10.0 ms  U1   PA5 went high
```

Without an MCU (a purely analog board) the old list of the most active nets is
kept.

## Pin states {#pin-states}

| Word | Meaning |
|---|---|
| Static high / Static low | The firmware drove the pin to that level and left it. |
| Toggling | The pin changes level. The row gives the frequency and duty. |
| Input or Hi-Z | The firmware never drove the pin. |

Frequency and duty come from running aggregates, so they stay exact after the
per-pin edge list is capped at 400 edges. On simavr times are exact. On Renode
and QEMU the emulator is polled once per co-sim chunk, so an edge is placed to
the chunk and a fast toggle is reported by its rate over the whole run rather
than edge by edge.

## Core state {#core-state}

| Word | Meaning |
|---|---|
| Running | The core was still producing output at the end of the run. |
| Parked | The core sits in a tight loop (at most three distinct program counters) and has produced nothing since the loop began. The row names the function from the ELF symbols. |
| Trapped | Parked in a function that means the firmware gave up: an error handler, `abort`, a panic. |
| Faulted | A fault handler was entered or the core crashed. |
| Core state unknown | The backend cannot say. |

The row also carries the number of resets and, on a watchdog reset, a marker on
the timeline.

## Serial port

The UART row shows the first line the firmware printed and the bytes sent. The
baud rate is shown only where the backend can read it from the peripheral. The
row is linked to the TX net when the board wires that pin and the part's pin map
names a transmit function (the STM32 and AVR maps do; the RP2040 card names pads
`gpioN`, so that row stays "net unknown" there until the card carries the
function).

## Bus log {#bus-log}

`--bus-log` prints every decoded transaction, in time order, one per line:

```
   400.0 ms  UART U1 UART "i2c_err"
     5.0 ms  I2C  U2 0x48 write [0x00] ACK
```

Only buses the co-sim can see are logged: an I2C or SPI device attached to the
emulated controller (a sensor card bound to the board, or a peripheral attached
in `hauksbee-engine`). A bus with no
device attached on the emulator backends is not seen, and the run says
"bus: 0 I2C" with the firmware's own error text (for example `i2c_err`) on the
serial line. An address that nothing answers shows as
`NACK (no device answered)` where the controller is observed. An SPI
transfer is one chip-select-framed exchange with the bytes out and back. With
`--json`, the log is in `cosim.bench.bus_log`.

## Timeline and `--vcd` {#timeline}

`--vcd out.vcd` writes the whole run as a VCD waveform that GTKWave, PulseView
and Surfer open:

* every MCU pin as a one-bit signal, using the net name where the board has one;
* a decoded string track for each I2C, SPI and UART bus (`s` values, the
  GTKWave/Surfer string extension), so the transactions read as text over the
  waveform;
* an event track with the reset, watchdog, stall, first pin edge and first
  serial byte markers.

Timestamps are picoseconds. The file is refused if it would overwrite the board,
BOM, placement, firmware or as-built input.

## Auto-seeded probes {#seeded-probes}

When a run opens in the app, the scope is seeded with the nets a person would
clip a probe to first, with the reason shown beside each:

* an indicator LED net or a pin that toggles at a steady rate;
* the UART TX net;
* an I2C or SPI line, with the decoded transactions under it;
* the reset line, when the part restarted during the run.

## Emulator runs in the app

The synchronous analysis never waits for Renode or QEMU. The app starts a job
with `POST /api/cosim-run` (same multipart body as the analysis), polls
`GET /api/cosim-run/{job_id}` for `{state, progress_s, total_s}` and receives
the finished `cosim` section, including the bench view, when the state is
`done`. One job runs at a time.

## Open-source record

| Candidate | Licence | Decision | Reason |
|---|---|---|---|
| `vcd` 0.7.0 | MIT | Adopt (dev-dependency) | An independent parser for the round-trip test of the VCD writer. |
| `wellen` | BSD-3 | Reject | Heavier, and FST support is not needed. |
| sigrok protocol decoders | GPL | Borrow semantics only | I2C/SPI/UART transaction vocabulary and framing rules; no code is copied. |
| GTKWave, PulseView, Surfer | various | Viewers only | They are what `--vcd` is written for. |

The VCD writer itself is small (about 300 lines) and is the product's own
format choice (string tracks for decoded buses), so it is written here rather
than depending on a writer crate.
