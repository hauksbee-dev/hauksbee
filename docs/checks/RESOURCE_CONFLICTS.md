# MCU internal resource-conflict check

A class of board bug that no connectivity sweep can see: two board-level
functions are wired to *different* MCU pins, the netlist is clean (no short, no
missing pull, no contention), but the two pins map to the *same shared silicon
resource instance* inside the MCU - one PWM slice+channel, one QSPI pin group -
so the chip physically cannot serve both at once. You only catch it if you know
the MCU's internal peripheral-to-pin binding, which the design files do not
carry. This check supplies that binding (a hand-authored, RM-cited resource map)
and reasons over it.

Two real, shipped, *documented* bugs define and validate the check:

1. **Olimex RP2040-PICO-PC** - open issue #1 on `OLIMEX/RP2040-PICO-PC`,
   unfixed across the shipped revisions. The PicoDVI pixel/bit clock is generated
   with PWM on GP12/GP13, while the board's PWM stereo audio sits on GP27/GP28.
   GP12 and GP28 both map to RP2040 **PWM slice 6, channel A**, so the DVI clock
   and the left audio channel cannot both have a PWM output: they want the same
   slice and channel.

2. **SparkFun SAMD51 Thing Plus** - `sparkfun/Arduino_Boards` issue #82. The
   on-board AT25SF041 SPI flash is wired to PA08..PA11, which are the SAM D5x
   **QSPI DATA0..3** pins. Used as an ordinary SERCOM/SPI device they commit the
   fixed QSPI pin group to a non-QSPI peripheral, so the QSPI controller cannot
   use it and the flash ends up inaccessible from the QSPI driver.

## 1. The metadata model

`crates/hauksbee-extract/db/mcu_resources.toml` is a per-MCU resource map: a part
matcher plus a `pad -> { resource bindings }` table, hand-authored from the
reference manuals with the section cited per block. Keys are **pad/pin numbers**
exactly as the extracted board carries them (`Pin.number`), because the netlist
and PCB paths reliably carry pad numbers but *not* pin functions (the KiCad-5
`.net` and the Eagle `.brd` both drop `pinfunction`). So the table is the source
of truth for "pad N of this part is GPIO X, on PWM slice S channel C".

Resource kinds modelled:

- **RP2040 PWM slices/channels.** Each GPIO n is hardwired to PWM slice
  `(n >> 1) & 7`, channel A when n is even / B when n is odd (RP2040 datasheet
  4.5.2, the GPIO-function table). The pair `{slice, channel}` is the resource
  instance: one channel can be driven out of exactly one pin at a time, and the
  two channels of a slice share the slice counter/TOP (4.5.2.1), so two PWM
  functions on one `{slice, channel}` is a hard conflict. Authored for both the
  Pico-form module (the Olimex carrier, pad = the module's 40-pin castellation)
  and the bare QFN-56 chip (the RP2040-minimal board, pad = the datasheet pin).

- **SAMD51 QSPI pin group.** The QSPI controller on SAM D5x is NOT routable
  through the PORT mux: its six signals are fixed to PA08..PA11 (DATA0..3), PB10
  (SCK), PB11 (CS) (SAM D5x/E5x Data Sheet, Table 6-1 function "H", section 36).
  Those pads also carry SERCOM/other functions in the mux, so a designer *can*
  wire an ordinary SPI device to them, but then the pad is committed to whichever
  peripheral is selected - a flash on these pads over SERCOM blocks QSPI.

  **The discriminator (this is the load-bearing subtlety).** Putting a flash on
  PA08..PA11 is ALSO exactly how you wire a flash you *intend* to drive over the
  QSPI controller - the normal, correct SAMD51 design (Adafruit Metro/Feather M4
  do this). So "flash on the QSPI data pads" alone is NOT the bug. The bug is
  wiring the flash as a 4-wire SERCOM SPI device on those pads. The physical
  discriminator, read from the net names: a CORRECT QSPI flash drives PA08..PA11
  as the quad-IO data lanes (`IO0..IO3` / `D0..D3`) with SCK/CS on the dedicated
  PB10/PB11. The SparkFun BUG wires 4-wire SPI (`MOSI`/`MISO`/`SCK`/`CS`) onto the
  PA08..PA11 data pads. The check fires ONLY when a 4-wire-SPI-role net
  (MOSI/MISO/SCK/CS) lands on a QSPI DATA pad (`data = true` in the table, and
  `net_role_is_4wire_spi`), so the Metro/Feather-M4 quad-IO flash stays silent.
  Verified against the corpus: SparkFun SAMD51 fires, Adafruit Metro M4 (same
  pads, IO0..IO3 naming) is silent.

- **ESP32 - recorded honestly as having no fixed conflicts of this class.** The
  ESP32 GPIO matrix routes almost every digital peripheral to almost any GPIO,
  so two functions never fight over a fixed silicon instance the way RP2040 PWM
  slices or SAMD51 QSPI pins do. The only true pin-bound constraints are the
  strapping pins and the ADC2/Wi-Fi exclusion, which are a *different* class
  (boot-strap / analog), handled elsewhere. The table marks ESP32
  `fully_routable = true`, and the check skips a part so marked before any pin
  inference runs: it emits nothing at all rather than manufacturing a conflict
  out of a routable matrix. The negative is written down in the table, not in
  the findings, and `resource_probe` surfaces it as `fully_routable=true` next
  to the match so the opinion is inspectable. (Honest encoding of the negative,
  per the calibration discipline.)

The map is extensible to SERCOM-instance pad constraints, STM32 TIMx_CHy timer
sharing, and ADC instances by the same `pad -> {resource instance}` shape.
Only the two kinds the two known bugs exercise are populated and validated
here.

### Hand table vs codex extraction (the cross-check)

The hand table is the source of truth. The automated extraction is a *check
on* the hand authoring, not a replacement. `resource-extract`
(`crates/hauksbee-extract/src/bin/resource_extract.rs`) extends the
`model-extract` codex pipeline: `pdftotext` the datasheet, slice the text around
the GPIO/PWM section, prompt codex (`codex exec --sandbox workspace-write
--skip-git-repo-check --cd <pdf-dir>`, stdin closed, background poll, 10-min
timeout) for the GPIO -> slice+channel table, parse and validate every PWM value
(slice 0..7, channel A/B), then diff against the hand table.

Run **live on the RP2040 datasheet** (5.3 MB, downloaded on demand):

```
cargo build -p hauksbee-extract --bin resource-extract
curl -sL -o testdata/datasheets/rp2040-datasheet.pdf \
  https://datasheets.raspberrypi.com/rp2040/rp2040-datasheet.pdf
target/debug/resource-extract --pdf testdata/datasheets/rp2040-datasheet.pdf \
  --part rp2040 --compare crates/hauksbee-extract/db/mcu_resources.toml
```

**Result: 30/30 agreement.** codex read the datasheet's GPIO-function table and
produced exactly the `(n>>1)&7 / A|B` mapping for GP0..GP29, including the
bug-defining `GP12 = 6A` and `GP28 = 6A`. The machine extraction is committed as
the offline fixture `testdata/resource/rp2040_pwm_extracted.toml`, and
`committed_rp2040_extraction_matches_hand_table` re-checks the agreement on every
runner with no codex and no network. The live test
(`live_rp2040_extraction_agrees`) is `#[ignore]`d (it shells out to codex).

## 2. The check

`ExtractedBoard::resource_conflicts()`
(`crates/hauksbee-extract/src/resource_conflict.rs`) does:

1. **Identify the MCU.** Match each component against the table by value/lib id,
   guarded by a `min_pins` count so a loose name match cannot fire on a small
   part. (This guard is load-bearing: a KiCad rescue-library id like
   `RP2040-PICO-PC_rev_C-rescue:74LVC125...` contains "PICO", which without the
   guard matched a 14-pin buffer and 1-pin mounting holes - see the kill below.)

2. **Determine which MCU pin is USED for WHAT.** For each resource-bearing pin,
   trace the *signal* net to a classifiable target - an HDMI/DVI connector, an
   audio jack, a flash chip - and decide the demanded function and the peripheral
   it uses. The trace follows the signal path only: it never crosses a
   power/ground net and never re-enters the MCU (so it cannot wander), and it
   hops through genuine series passives (a TMDS series-termination resistor, a
   PWM-audio RC reconstruction filter, including the AC-coupling cap) and a small
   line buffer (the 74LVC125 on the audio path). The evidence chain is recorded
   in the finding.

3. **Decide the peripheral honestly.** Reaching the HDMI connector is *not*
   automatically a PWM demand: on PicoDVI only the pixel/bit **clock** (the
   `CK`/`CLK` net) is PWM-generated. The three TMDS **data** lanes are driven
   by PIO+DMA and the DDC/CEC lines are I2C. The check classifies by the pin's own
   net plus the target, and only a function whose peripheral is PWM occupies a
   PWM slice. (Conflating them manufactured false PWM conflicts on the data and
   control pins - see the kill below.)

4. **Check feasibility.** A PWM slice+channel demanded by two *distinct*
   functions has no valid assignment (one channel, one pin) -> **High**. A QSPI
   group with >= 2 pads carrying two *different* non-QSPI functions has no
   assignment that serves both either -> **High**. A group whose >= 2 committed
   pads all belong to ONE self-consistent function is a weaker claim: nothing
   contends at runtime, firmware driving that device as plain SERCOM SPI works,
   and all the copper proves is that the QSPI controller can never reach those
   pads -> **Low**, stated as exactly that. No medium tier is emitted:
   contention on these instances is binary, so a half-conflict does not arise.
   The tier was dropped rather than defined loosely.

Reported through the same `NetLintReport`/`LintFinding` shape the connectivity
lint uses (`LintCheck::McuResourceConflict`), so the CLI and callers treat it
uniformly.

### CLI

```
hauksbee run <board> --resources   # this check only
hauksbee run <board> --lint        # connectivity lint + this check
```

## 3. Validation (two-sided)

Corpus-gated tests in
`crates/hauksbee-extract/tests/resource_conflict_corpus.rs`
(`HAUKSBEE_REQUIRE_CORPUS=1` makes a missing corpus a hard failure).

### Olimex RP2040-PICO-PC: PWM slice 6A (FLAGGED, known issue #1)

`hauksbee run .../RP2040-PICO-PC_rev_D.net --resources`:

```
resource-conflicts: 1 finding(s)
[high] mcu_resource_conflict - RP2040_PLATFORM1 (2X(PN1X20(F-D-1X20-LF))): two
  functions demand RP2040 PWM slice/channel 6A, which can serve only one pin at
  a time [RP2040 datasheet 4.5.2, GPIO->slice = (n>>1)&7, ch A/B by parity]:
  PWM audio (GP28 pad 34, net '/PWM_L': net '/PWM_L' -> U3 -> net
    'Net-(R18-Pad2)' -> U3 -> net 'Net-(R23-Pad2)' -> U3 -> net
    'Net-(R22-Pad2)' -> U3 -> net 'Net-(R19-Pad2)' -> R19 -> net
    'Net-(C6-Pad2)' -> C7 -> net '/PWM_AUDIO_R' reaches AUDIO_JACK_1
    (3.5mm/PJ-W47S-05D2-LF)); and PicoDVI PWM pixel clock (GP12 pad 16, net
    '/PICO_CK-': net '/PICO_CK-' -> R10 -> net '/CK-' reaches HDMI1
    (HDMI-SWM-19))
note: run the same command with --plain for a plain-language explanation of each finding and how to fix it.
note: gate-grade finding(s) above, but this is a report command so the exit code is 0. Add --strict to exit 2 on them (exit contract: 0 = clean or report-only, 1 = input error such as a missing or unreadable file, 2 = findings under --strict, 3 = invalid for analysis), or gate CI with hauksbee-ci.
```

(The finding is one long line in the real output; it is wrapped here to fit the
page. The two `note:` lines are on stderr, and the second one is why this run
still exits 0: `--resources` reports, `--strict` gates.)

The full evidence chain to the s-expression level: GP12 (slice 6A) drives the
DVI clock to the HDMI connector through the series-termination resistor R10.
GP28 (also slice 6A) carries PWM audio into the 74LVC125 buffer (U3), and the
traced path leaves the buffer through the RC reconstruction filter (R19 and the
AC-coupling cap C7) onto `/PWM_AUDIO_R` at the jack. Both demand slice 6
channel A. Flagged on **rev C and rev D**.

**The rev-B-netlist discriminator (and the ground-truth that makes the rev-C/D
finding real).** The rev-B `.net` is **SILENT, and that is correct for that
input**: there the DVI clock is on GP14/GP15 (`/PICO_CK+/-` -> module pins
19/20), which are PWM **slice 7**, while the audio is on GP28/GP27 (slice
6A/5B). No two PWM functions share a slice+channel, so that netlist genuinely
has no slice-6A conflict - which proves the slice-6A fire elsewhere is real,
not an any-RP2040-with-DVI-and-audio false positive.

But that `.net` does not describe the manufactured rev B board. Its own
`(source ...)` header says it was exported from `RP2040-PICO-PC_rev_A.sch`, and
the rev-B **layout disagrees with it**: `RP2040-PICO-PC_rev_B.kicad_pcb` routes
the DVI clock on GP12/GP13 (slice 6), and the shipped Gerbers in the same
folder agree pad-for-pad (X2 attributes in `RP2040-PICO-PC_rev_B-F_Cu.gbr`:
`U2` pad 16 = `/PICO_CK-`, pad 17 = `/PICO_CK+`). So the **rev B layout fires
the slice-6A conflict, and that is a true positive about the physical board**
(consistent with the open issue being reported against rev B). The check judges
each input on what it actually says: silent on the stale rev-A netlist export,
flagged on the layout the board was fabricated from and on every rev C/D input.
Both sides are pinned by `tests/resource_conflict_corpus.rs`.

### SparkFun SAMD51 Thing Plus: QSPI group spent on plain SPI (NOTED, known issue #82)

`hauksbee run .../SAMD51_Thing_Plus.brd --resources`:

```
resource-conflicts: 1 finding(s)
[low] mcu_resource_conflict - U2 (ATSAMD51J20A-A): the fixed QSPI pin group
  'qspi_data' is committed to one non-QSPI function across 4 pads (SAM D5x QSPI
  is pin-locked to PA08..PA11/PB10/PB11, not PORT-routable) [SAM D5x/E5x Data
  Sheet, Table 6-1 function H, section 36]: SPI flash on PA11 (pad 20, net
  'FLASH_MISO': net 'FLASH_MISO' reaches U4 (4Mb Flash)); SPI flash on PA09
  (pad 18, net 'FLASH_SCK': net 'FLASH_SCK' reaches U4 (4Mb Flash)); SPI flash
  on PA10 (pad 19, net 'FLASH_CS': net 'FLASH_CS' reaches U4 (4Mb Flash)); SPI
  flash on PA08 (pad 17, net 'FLASH_MOSI': net 'FLASH_MOSI' reaches U4 (4Mb
  Flash)). Nothing contends at runtime: firmware that drives this device as
  plain SERCOM SPI works, and boards shipping this wiring deliberately exist
  (the SparkFun Thing Plus SAMD51's vendor firmware declares plain SPI flash on
  exactly these pads). What the netlist does prove is that the QSPI peripheral
  can never drive these pads, so quad-rate access to this device is off the
  table; the netlist cannot tell deliberate plain-SPI from an unintended loss of
  QSPI.
note: run the same command with --plain for a plain-language explanation of each finding and how to fix it.
```

Note what is *absent* next to the Olimex run above: there is no "gate-grade
finding(s)" note here, because a `[low]` finding is not gate-grade. Even under
`--strict` this board stays green, which is the intended answer for a wiring the
vendor chose on purpose.

Verified at file level both ways: the Eagle `.brd` puts the flash (U4) on U2 pads
17..20 (= TQFP64 pins 17..20 = PA08..PA11), and the `.sch` independently names
the same MCU pins PA08..PA11. Those are the QSPI DATA0..3 pins, and the four
nets carry 4-wire-SPI roles (`FLASH_MOSI`/`MISO`/`SCK`/`CS`), so the group is
spent on a SERCOM device.

The severity is **Low**, and that is the calibrated call. All four pads carry one
self-consistent function, so nothing fights for the pins at runtime: this board
ships and its flash works, because the vendor firmware drives it as plain SERCOM
SPI on exactly these pads. What the copper does settle is that the QSPI
controller can never reach that flash, so quad-rate access to it is unavailable
for the life of the design. The netlist cannot distinguish a deliberate plain-SPI
choice from a botched QSPI intent, so the check reports the part it can prove and
leaves the intent to the reader. Calling a famous working board **High** would be
a wolf-cry against this project's own calibration bar.

## 4. Calibration (zero false positives or it does not ship)

`clean_corpus_boards_raise_no_resource_conflict` sweeps every corpus board
carrying an RP2040 / SAMD51 / ESP32, plus unrelated designs, and asserts the
check is **silent on all of them**:

| Board | MCU | Result |
|-------|-----|--------|
| RP2040 minimal (official) | RP2040 QFN-56 (matched) | silent (no DVI, no PWM-audio collision) |
| SparkFun Thing Plus RP2040 | RP2040 QFN-56 (matched) | silent |
| Olimex ESP32-EVB REV-L / REV-K1 | ESP32 (fully routable) | silent |
| ZSWatch mainboard, Watchy, LumenPnP mobo, Lily58, MNT Reform mobo 3.0 | various | silent (no table match) |
| Adafruit Feather M0 | ATSAMD21 (NOT a D5x J-variant) | silent (correctly does not match) |
| **Adafruit Metro M4** | **ATSAMD51J (QSPI flash on PA08..PA11)** | **silent (correct QSPI flash: quad-IO naming, the discriminator)** |
| Adafruit QT Py | ATSAMD21 + flash | silent |
| SparkFun RedBoard | ATmega328 | silent |

**Total false positives on known-good boards: 0.** The two RP2040 boards and the
ESP32 boards are the meaningful negatives: the MCU IS matched (verified via the
`resource_probe` example) and the check still finds nothing, so the silence is a
true negative, not a failure to match.

### The Tarski meta-lesson, earned twice

The first runs on the Olimex board were silent for the *wrong* reasons, and
chasing that to ground (presume the tool is broken first) exposed two real
defects in this check, both fixed before the finding was trusted:

1. **Loose MCU matching.** The match regex (a bare `Pico` substring + a loose
   `lib_re`) matched the 74LVC125 buffer and the mounting holes, because their
   KiCad rescue-library id contains `RP2040-PICO-PC...-rescue`. Fixed by matching
   the actual symbol part name (`:RP2040_PLATFORM`, `:RP_pico2040` for the
   renamed rev-B symbol, or a genuine Pico) and a `min_pins >= 30` guard.

2. **The inference traversed power/ground nets.** Following a bridge onto GND or
   +3V3 made every MCU pin "reach" the HDMI connector (the rail touches
   everything), so every pad resolved to "DVI" and the real audio/DVI distinction
   was lost. Fixed by refusing to traverse any rail/ground net and by
   distinguishing the *peripheral* a reached target uses (PicoDVI clock = PWM,
   TMDS data = PIO, DDC = I2C), so only genuinely-PWM functions occupy a PWM
   slice.

A check that fired on "any RP2040 with DVI and audio" would be a confident
false positive. The rev-B silence proves it does not. The `resource_probe` example
(`cargo run -p hauksbee-extract --example resource_probe <board>`) dumps the
per-pad inferred function and is the re-runnable audit trail for both kills.

## 5. Honest limitations

- **Function inference is the weak link, by construction.** The check assigns a
  function only when a used pin's signal path reaches an *unambiguous* target (an
  HDMI/DVI connector, an audio jack, a flash chip) through passives/buffers. It
  is deliberately narrow: a peripheral reached through an active codec/DAC IC, an
  FPGA, or a part the classifier does not know is not attributed, and the check
  stays silent rather than guess. So the check finds the *modelled* conflict
  classes on boards whose function wiring it can read, not every conceivable
  internal conflict.

- **PicoDVI clock vs data is decided by net naming.** "Is this HDMI pin the PWM
  clock or a PIO data lane" is read from the pin's net name (`CK`/`CLK` = clock).
  A board that named its DVI clock net unconventionally could be mis-binned. The
  audio side is more robust (the buffer + RC-to-jack topology is the evidence,
  not the net name, which is why rev B's generically-named `/GPIO28` audio net
  still resolves).

- **Pad-number tables assume a known package/pinout.** The RP2040 module table is
  keyed by the Pico 40-pin castellation order and the SAMD51 table by the TQFP64
  datasheet pin numbers. A board using a different package of the same die would
  need its own pad map. The table matches on part identity, so a mismatch is a
  silent miss, not a false positive.

- **Only PWM-slice and QSPI-group instances are populated.** SERCOM-instance pad
  constraints, STM32 timer-channel sharing, and ADC-instance conflicts fit the
  same `pad -> {instance}` shape but are not authored or validated here. The two
  known bugs exercise exactly the two kinds that are.

## Reproduce

```
cd hauksbee
cargo build --release -p hauksbee-engine
BIN=target/release/hauksbee; C=../board-corpus/famous

# FLAGGED: the two known bugs.
$BIN run "$C/olimex_rp2040_pico_pc/HARDWARE/RP2040-PICO-PC hardware revision D/RP2040-PICO-PC_rev_D.net" --resources
$BIN run "$C/sparkfun_thingplus_samd51/Hardware/SAMD51_Thing_Plus.brd" --resources

# FLAGGED on the layout path too, including rev B (the fabricated board's
# .kicad_pcb and Gerbers route the DVI clock on slice 6; see the
# rev-B-netlist discriminator section above).
$BIN run "$C/olimex_rp2040_pico_pc/HARDWARE/RP2040-PICO-PC hardware revision B/RP2040-PICO-PC_rev_B.kicad_pcb" --resources

# SILENT: the rev B .net (a stale rev-A schematic export: DVI clock on slice 7)
# and the clean RP2040 boards.
$BIN run "$C/olimex_rp2040_pico_pc/HARDWARE/RP2040-PICO-PC hardware revision B/RP2040-PICO-PC_rev_B.net" --resources
$BIN run "$C/rp2040_minimal_kicad/minimal/RP2040_minimal_r2/RP2040_minimal_r2.kicad_sch" --resources

# Per-pad inference audit (why a board fires or stays silent).
cargo run -q -p hauksbee-extract --example resource_probe \
  "$C/olimex_rp2040_pico_pc/HARDWARE/RP2040-PICO-PC hardware revision D/RP2040-PICO-PC_rev_D.net"

# Tests (corpus-gated; REQUIRE_CORPUS makes absence a hard fail).
HAUKSBEE_REQUIRE_CORPUS=1 cargo test -p hauksbee-extract --test resource_conflict_corpus
cargo test -p hauksbee-extract --bin resource-extract   # incl. the offline extraction fixture
```
