# Limitations: triage and closure

hauksbee's other docs each record the honest limitations of their own surface.
This doc is the triage: what is open today and why, and what was chased to
ground truth and fixed. The bar for "fixed" is the project's: every change
covered by a test, no false positive introduced into any lint/check, and any
flipped result proven two-sided, that is, shown both to fire on the fault and
to stay silent on a clean counterpart, with file-level evidence.

The governing rule throughout: a surprising pass or fail is presumed a defect
in *our* tool until chased to ground truth. Several of the closed items below
began as recorded "honest limitations" and turned out, once chased, to be
hauksbee defects.

## Structural ceilings

Two limits are not defects and will never appear under "closed". They bound the
approach rather than the implementation, so the useful thing is to say where
they bind and what is done about the part that can be.

### Model availability caps analogue coverage

A circuit hauksbee cannot model, it cannot simulate. Many vendors ship SPICE
and IBIS models only under NDA, or encrypted, and no amount of engineering here
changes that. If a meaningful fraction of a design is unmodellable, coverage is
capped by something outside the project.

Where it binds is lower than it first sounds, and the reason is what hauksbee
asks. A transistor-level simulator needs the vendor's die model. Board-level CI
asks whether a rail sags, whether a part runs past its rating, whether a pin
conflicts, and whether the firmware drives what it thinks it drives. Those are
answerable from terminal behaviour at datasheet grade: dropout, quiescent
current, thermal resistance, logic thresholds, protection trip points. A
behavioural model built from a public datasheet answers them, and it runs fast
enough to sit in a commit hook, which an encrypted die model would not.

Three things follow from that, and all three ship:

- **The gap is named, not averaged away.** A part that does not bind is
  reported by reference, and the ones sitting on a connected net are separated
  from the ones that do not change the solve. `hauksbee run` prints the ratio.
- **Closing a gap needs one TOML file and no recompile.** Analogue parts,
  sensors, logic ICs and MCU variants are all declarative. See
  [`../extending/README.md`](../extending/README.md). Model packs let a team
  keep its own parts together, and the codex-backed extraction (an LLM coding
  agent used during development) reads a datasheet into a first-draft model.
- **Coverage is gateable.** The `model_coverage` assertion pins the fraction of
  active ICs that must bind, so the day a new part drops coverage the build
  says so. See [`../ci/CI.md`](../ci/CI.md).

The residue is real. Parts whose useful behaviour is genuinely not in the
public datasheet, switching converters modelled past the averaged loop, and
anything RF, stay out of reach. Those are named in the open section below and in
[`CAPABILITIES.md`](CAPABILITIES.md) rather than papered over.

### One false positive costs more than one miss

A verification tool that cries wolf gets switched off, and a switched-off tool
catches nothing. The bar for adoption is therefore higher than the bar for
usefulness, and there is no second first impression. This is the sharpest risk
the project carries.

A check that misfires on your board can be overruled one finding at a time,
without switching the check off for everything else it catches. A waiver
carries a required reason and a required expiry, so the finding comes back on
a date rather than staying silenced forever, and waived findings are printed
rather than hidden. See [`../ci/CI.md`](../ci/CI.md).

The rest is aimed at the same thing. A check that fires must be right,
and that is enforced rather than promised: every check is run across a corpus
of real, working, shipped boards, and a check that lights up on a healthy board
does not land, however good it is at finding the fault it was written for. See
[`../../CONTRIBUTING.md`](../../CONTRIBUTING.md). Beyond that, a run that
cannot produce a trustworthy answer exits 3 instead of passing, coverage holes
travel as data fields rather than prose, and a report that would mislead says
so instead of rendering.

What is honestly missing:

- **The corpus is not your board.** Zero false positives across the boards
  measured is a real claim and a narrow one. It does not promise the same on a
  design nobody here has seen.
- **Precision is claimed globally, not per check.** A measured number for each
  check would be a stronger and more falsifiable statement than one figure for
  the suite, and it would tell a new user which checks to trust first.

## Open limitations

These are the genuine open limitations the code carries today, each with the
reason it is open and what would close it.

### A co-sim chunk no fallback can solve holds stale voltages

The co-sim advances the circuit in chunks. A chunk whose primary analog solve
fails to converge is retried on a fallback ladder before the run gives up, in
order of increasing desperation: the same integration with the maximum step
bounded to a small fraction of the chunk, backward Euler at that bounded step,
backward Euler from a cold start (the warm seed dropped so the operating point
re-runs the full gmin and source-stepping continuation), and finally the chunk
subdivided into quarters marched back to back. A chunk a rung carries is a real
converged solve, and the run says which: the window and the rung that produced
it are recorded per chunk and surfaced as
`fallback_windows` in the co-sim JSON and the default text summary. A backward
Euler window is first order and numerically dissipative, so fast transients and
ringing inside it are damped relative to the second-order primary solve; the
record states the method and carries a measured `error_estimate_v`, a
conservative step-doubling estimate of the chunk-end voltage error obtained by
re-solving the window at a shifted accuracy dial and differencing the end
states (absent, never invented, when no companion re-solve converges). The
typed [`error_budget`](../analysis/ERROR_BUDGETS.md) partitions solved methods
and marks unsolved spans invalid; it does not yet carry the per-window
estimate, which today lives on the window record, the co-sim JSON, the CLI
text summary, and the CI evidence assumptions.

What remains open is the chunk no rung can carry. Its node voltages are the
previous chunk's, not a solved answer, so every voltage, current and fault
reading inside that window is fiction, and the run says so rather than hiding
it: a warning names the failed chunk count and the time spans, `--strict` turns
it into a failing exit, and the web report raises it as a finding on the co-sim
card. It will not invent a number for those windows. The usual cause there is
structural rather than numerical: unresolved active parts leaving nodes
floating (`hauksbee models --help` lists what is unresolved), a section with no
reachable operating point, or conflicting rails, none of which any integration
method can solve. Resolve the parts or simplify the offending section and
re-run. The two-sided gate is
`crates/hauksbee-engine/tests/cosim_fallback_chunk.rs`: a board whose fire step
kills the primary march is rescued and recorded, and a structurally singular
board still refuses loudly.

### A backend's clock rate is verified on five of six, and QEMU is approximate

Time-based co-sim results rest on the emulator advancing at the part's clock
rate. `simavr:atmega328p`, `renode:rp2040`, `renode:stm32f103`,
`renode:stm32f4_discovery` and `renode:nrf52840` are each measured at the part's
rate to within the 0.2% quantization of the measurement, and
`crates/hauksbee-mcu/tests/clock_truth.rs` re-measures the Renode STM32, nRF and
FE310 parts on every run. Four platforms previously ran 4.5x to 9x fast, because
the stock platform file declared a 72 MHz SysTick and a 100 MIPS core whatever
part the descriptor claimed, and `frequency_hz` cancelled out of the engine's
arithmetic so nothing disagreed. Each descriptor now declares the part's
reset-default core clock inline, and the loader refuses a descriptor whose
declarations disagree with `frequency_hz` or which declares no core clock at all,
bundled platforms included, so the defect cannot be re-added as data.

The two former gaps here are closed and gated. `renode:sifive_fe310` now
declares the real FE310's 32768 Hz `mtime` (the stock platform had it 1892x
wrong at 62 MHz), held by a two-sided measurement in the same clock-truth
suite: an mtime-timed oracle firmware measures 1.00x on the corrected platform
and the identical measurement fails loudly against the old rate. `qemu:esp32`,
`-s3` and `-c3` credit each chunk from the measured QMP RESUME/STOP window
instead of the slept one, so the control-channel slack that used to make them
read 1.35x-1.6x biased is now priced in
(`crates/hauksbee-mcu/tests/qemu_clock_truth.rs` measures both crediting
schemes from one run). What remains on the ESP32 family is wall-clock pacing
itself: virtual time tracks the host clock only approximately and degrades
under load, which every affected run states through the same
`timing_limitation` coverage channel the watchdog gaps use. The STM32F103's
TIMx blocks likewise stay at the post-PLL 72 MHz on purpose (a stock HAL
project cannot boot otherwise) and every F103 run says so.

One trap worth keeping, because it hides the defect in the flattering direction:
the GPIO poll aliases any half-period near the chunk width. At 5 ms chunks the
9x-fast STM32F103 firmware measured a perfect 100 edges. A clock measurement must
use a chunk finer than the half-period a WRONG sim would produce, not merely
finer than the right one.

### Parallel EEPROM programming models digital busy status, not charge-pump physics

The built-in AT28C256 path models the real bidirectional bus, final stored
bytes, software protection, 64-byte page boundaries, and the 150 µs inter-byte
deadline on cycle-exact simavr. It does not model the internal 3/10 ms charge-
pump physics, endurance, retention, or optional 12 V chip erase. It does defer
cell commits for the declared maximum program interval and returns the I/O7
data-polling complement and alternating I/O6 toggle bit while busy. The generic
AT28C256 identity uses the conservative 10 ms maximum; a shorter delay must be
justified by an explicitly resolved faster part, not inferred from the family.

### An unserviced watchdog does not reset the MCU on most Renode parts or on QEMU

`simavr` is the exception and now behaves: a starved `wdt_enable(WDTO_15MS)`
reboots the core at the right virtual time, repeatedly, and the reboots are
reported rather than treated as a silent restart, because an assertion that
passed across a reboot was not measuring the run it claimed. It previously
livelocked the co-sim, because the reset rewound a cycle counter the chunk loop
was waiting on.

Elsewhere the watchdog is a coverage hole and is surfaced as one per part. On
`renode:nrf52840` a watchdog arms and reads back as running with a correct
32768 Hz reload and never fires: zero resets in 1.000 s of simulated time where
silicon gives twenty. On `renode:stm32f103` the IWDG does fire and reset once,
and the core then goes quiet where the part would reboot every timeout forever.
On the ESP32 family the timer-group watchdogs are disabled at launch, which is
the right call because a paused guest would otherwise trip them. On
`renode:stm32f4_discovery`, `renode:sifive_fe310` and `renode:rp2040` nobody has
run a starved watchdog to its timeout, and the two parts that were measured
disagree with each other, so nothing is inferred. Firmware that hangs still runs
forever on those parts, so any assertion about behaviour after a hang is fiction,
but a run on them says so rather than reading healthy.

### Power-up state is not modelled: no brownout reset, no strap latch, no fuses

Three related gaps, all on the digital side:

- **No POR or BOR.** Nothing resets the MCU in response to a rail event. A board
  that browns out on inrush has that collapse caught by a `rail` assertion while
  its firmware keeps executing as though the supply held.
- **Straps are not sampled at the reset latch.** Input injection is skipped on
  the first chunk, so a board's strap bias reaches the core only after the boot
  ROM has already latched. hauksbee ships a strap-pin lint, and that lint is
  entirely static: co-sim does not corroborate it.
- **No non-volatile configuration.** Fuse bytes, option bytes, eFuse and UICR are
  not modelled and the descriptor format has no field for them. A factory-fuse
  ATmega328P runs at 1 MHz where hauksbee assumes 16. The STM32F103 is the one
  external-clock bring-up path that is now falsifiable: a populated crystal
  bridging the modelled `OSC_IN`/`OSC_OUT` pads makes `HSERDY` rise after the
  descriptor's nominal 2 ms startup, then an HSE-sourced PLL locks after 200 us;
  a missing or DNP crystal leaves both waits blocked. That is presence and
  timing evidence, not oscillator physics: wrong load capacitors, insufficient
  drive, wrong frequency, and a dead or marginal crystal are not reproduced,
  and other MCU backends still do not derive clock readiness from the board.

### nRF5340 has no co-sim backend

The Renode 1.16.1 portable build ships no nRF5340 platform, so the
ZSWatch-class DISPLAY-EN fault stays a static miss. nRF52840 is the closest
proven platform. See `docs/cosim/MCU.md`.

### PCB-only extraction has no pinfunctions

Multi-unit packages fall back to db pin maps when only a layout (no schematic
netlist) is available. Schematic netlists are authoritative when present.

### Device-decode is per-part and grows one part at a time

The configurable-controller decode check (config-pin divider vs the part's
datasheet band table) has no generic engine. Each supported part is a
hand-written decoder, seeded with the CYPD3177 USB-C PD sink only. It also does
not read the silk-screened voltage label next to a rotary detent, so it reports
which bands a selector can and cannot reach rather than "detent N labelled X
codes Y". See `docs/checks/DEVICE_DECODE.md`.

### Legacy KiCad-5 `.sch` (non-s-expression) reader

The Olimex ESP32-EVB revs A..K2 ship `EESchema Schematic File Version 2` ASCII
schematics. A reader for this format would need a second, independent schematic
front-end: its own `$Comp`/`$Sheet`/wire/label parser, a fresh geometric
netliser, and library-cache handling, all to the same exactness bar as the
s-expr reader (the corpus standard is *zero* split/merged nets against the PCB).
That is a large, self-contained effort with no shared code path with the s-expr
reader, so it is deferred as the genuinely-hard, low-ROI item. The PCB extractor
still handles those boards' layouts. Only their legacy schematics are out of
scope.

### I2C/SPI slave co-sim coverage, and what remains open around it

The working coverage first, because the boundary matters: I2C/SPI slave
co-simulation intercepts the hardware TWI/SPI in-process on AVR, so the byte
stream itself is exact (no timing model, no reimplemented controller); on Renode it runs through generated C# bridge peripherals on
every platform whose SoC descriptor names bus controllers (STM32F103 `i2c1`/
`spi1`, STM32F4 Discovery `i2c1`/`spi2`-`spi3`, nRF52840 `twi0`/`twi1`/`spi2`,
RP2040 `i2c0`/`i2c1`); and on QEMU-ESP32 it runs through a firmware mailbox
contract. FE310 declares no bus controllers, and RP2040 declares none for SPI
because the vendored PL022 model bit-bangs onto GPIO pins and never dispatches
to a registered slave, so a bridge there would silently see nothing. A sensor
bound to a bus with no controller is recorded as unexercised on every report
surface, and a `peripheral` assertion against it fails rather than
green-passing. Shipped proof: the
`i2c_sensor_cosim_renode.rs` and `spi_sensor_cosim_renode.rs` integration
tests in `crates/hauksbee-engine/tests/`.

What remains open:

- **SPI transaction framing when the chip-select net does not resolve.** The
  simavr SPI IRQ does not carry CS, so framing comes from the CS pin when the
  binder resolves the net (exact, off the real GPIO edge stream) or from the
  backend when it surfaces CS itself (Renode hardware-NSS). Failing both, the
  chunk boundary is the only frame available, and it is wrong in two ways: two
  transactions inside one chunk merge, and one spanning a boundary truncates.
  Each bus reports which of the three it got (`framing_mode`, on every report
  surface), and a heuristic bus is only correct as its controller's lone slave:
  nothing stops two of them being attached, and the dispatcher would give every
  byte to the first. Resolving the CS nets is the fix, and it is what the
  `--check` co-sim coverage points at.
- **Default Renode ADC maps.** `set_analog_in` injects for real through a
  per-platform `AdcChannelMap` (validated against a live Renode 1.16.1), but no
  *default* map ships for the stock STM32/nRF52/FE310 configs: those Renode
  platform descriptions model no ADC peripheral, and Renode's
  `Analog.STM32_ADC` speaks the F0/L0 register layout, so pretending it is an
  F1 ADC would be fake fidelity. Unmapped channels drop loudly (once-per-channel
  stderr warning, plus every report surface). A board that knows where its
  counts land adds `[[soc.adc]]` to its own descriptor, no recompile.
- **QEMU ESP32 SAR ADC** is not modeled by the Espressif QEMU fork, a
  silicon-model gap, so `set_analog_in` writes the count into a RAM-mailbox
  slot instead, a firmware contract like the GPIO mailbox below. Only
  mailbox-aware firmware reads it. The same applies to the I2C/SPI byte
  callbacks: request/response mailbox cells gated on `BUS_MAGIC`, serviced once
  per chunk, surfacing through the standard `on_i2c`/`on_spi` trait callbacks.
  Unmodified vendor firmware's real-controller bus traffic stays host-invisible
  until the fork grows a peripheral hook.
- **QEMU ESP32 GPIO** is observed through a firmware RAM mailbox because the
  fork's `esp32.gpio` model has no `GPIO_OUT_REG` read-back. Empirically
  confirmed on the latest build (QEMU 9.2.2 `esp_develop`, 2026-06-30): a host
  read of `GPIO_OUT_REG` (0x3FF44004) returns 0 via QMP `xp`, gdbstub `m`,
  *and* `qom-get` (the `esp32.gpio` object exposes no value property), and a
  host *write* to those registers is discarded. The model is write-effect-only
  with no host-visible state, while a RAM address round-trips perfectly. The
  only real fix is a ~15-line device-model patch to the fork's
  `hw/gpio/esp32_gpio.c` (store and return `GPIO_OUT`/`ENABLE`, handling the
  W1TS/W1TC aliases) plus a rebuild. The exact registers and the matching
  backend change are specified. Shipping the register-read backend path
  *without* a patched QEMU to validate it against would violate the
  no-unvalidatable-fixes rule, so it stays open. (The loader probes the current
  `~/.hauksbee-qemu-esp` first, with the legacy `~/.galvani-qemu-esp` kept as a
  fallback so existing installs keep resolving.)

### KiCad 10 boards: exact native-DRC parity remains unvalidated

KiCad 10's format (`20260206`) uses name-only nets and represents baked antipad
voids as keyholes in a filled contour. Both are handled. A focused fixture
checked with kicad-cli 10.0.5 keeps a correctly isolated keyhole-antipad pad
silent while reporting a pad under solid different-net fill, and the VENDETTA
ESC oracle has no Zone-Pad findings.

The remaining limitation is narrower but still important: the complete finding
set does not yet match native KiCad DRC exactly (VENDETTA reports 67 Hauksbee
shorts versus 60 native `shorting_items`). Format versions at or above
`20260000` therefore retain a `version_warning`; their findings are demoted and
do not fail strict CI gates.

KiCad 10 project clearance rules, which live in the sibling `.kicad_pro` rather
than in the board text, are read. The CLI and engine reports resolve them to the
board's own net names before running DRC
(`kicad_pro_clearance_rules` in `crates/hauksbee-engine/src/reports/mod.rs`, over
`clearance_rules_from_kicad_pro` in `crates/hauksbee-extract/src/drc.rs`); a
missing or malformed project file simply leaves DRC on the board/default rules.
The narrower gap is at the library boundary: `drc_from_text` takes board text
alone, so a caller driving that API directly has to read the project file and
pass the rules through `drc_from_text_with_clearance_rules` itself.
Cross-check them with KiCad 10's own DRC. Details and oracle evidence are in
[`../checks/SHORTS.md`](../checks/SHORTS.md).

### Eagle copper-pour fidelity in DRC

An Eagle `.brd` stores a signal pour's requested outline and all of its pour
settings (`isolate`, `rank`, `thermals`, `orphans`, `pour`: all parsed); only
the *computed* fill polygon is absent, because Eagle re-derives it on every
ratsnest / CAM run. That derivation is what keeps the fill out of the
pour-to-copper short test: Eagle carves max(`isolate`, the applicable
design-rule / net-class clearance) around every foreign-net wire, pad and via
(an `isolate` below the rules distance is ignored there; the pour-to-POUR case
below is measured to behave differently), thermal spokes only remove
same-net copper, and orphan removal only deletes fill pockets. Every setting
keeps or widens gaps, so a correctly derived fill cannot short or crowd foreign
copper in the same file. Treating the drawn outline as solid copper instead
would manufacture false shorts on every trace the fill legitimately carves
around. To be precise about what is and is not computed: the fill itself is
never reconstructed and the isolate distance is never numerically re-verified;
the settings are parsed, drive the reasoning above, and travel verbatim on a pour
finding's `Item::owner` field, though `--drc` does not render that field. Pour-to-copper pairs are therefore *not checked* (rather than
checked and found clean), and same-rank pours that approach each other without
ring overlap are not distance-checked either, since the fill extent near the
boundary depends on those settings. The argument above says a fill Eagle
derives from these settings would not violate; a hand-edited file whose fill
was never re-derived is outside it.

One pour-to-pour construct IS checked, and it was narrowed after being measured
against real fabrication output. Two overlapping same-rank pours of different
signals get no arbitration from their rank, and the file names no yielder either,
but whichever pour gives way is carved back by its own `isolate`, so the outline
overlap alone does not put copper in contact. The check reads the smaller of the
two, as the conservative choice. What is reported is an overlap where the smaller `isolate` is
below one micrometre (`POUR_MERGE_ISOLATE_MM`), meaning nothing in the file asks
for a gap at all. The measurement is in
[`../evidence/KNOWN_FAULTS_VALIDATION.md`](../evidence/KNOWN_FAULTS_VALIDATION.md):
the emonTx V3.4.5 overlaps its GND and AGND pour outlines in the same band on
both copper layers, and the gerbers it ships beside the `.brd` show the pair
separate on the layer where both pours hold `isolate="0.3048"` and joined on the
layer where one holds `0.00030625`.

**A measured false positive the narrowing does NOT remove.** A near-zero `isolate`
turns out to be necessary but not sufficient for the fills to meet. The emonTx
V3.4.0 puts both of its top-layer ground pours at `isolate="0.00030625"` with
overlapping outlines, and the copper gerbers it ships show the AGND body as a
separate component from the GND body. Four independent rasters put the separation
between about 2.6 mm and 29 mm, the low end being the better-resolved ones; the
number is unsettled and the separateness is not, which is why the finding rests on
component membership rather than on a distance. hauksbee flags that layer. It is wrong to, and the finding stands
only because the same rule is right about V3.4.0's bottom layer and V3.4.5's, and
because it is silent on V3.4.5's top layer where the old rule was wrong: a strict
improvement carrying one known error. No threshold on `isolate` can fix it, since
`isolate` does not determine the answer. Closing it needs Eagle's fill
reconstructed from the outline, the pour settings and the foreign copper, which is
not implemented. See the emonTx section of
[`../evidence/KNOWN_FAULTS_VALIDATION.md`](../evidence/KNOWN_FAULTS_VALIDATION.md).

**A second, smaller one.** A pour with no `isolate` attribute at all parses as
zero and still flags. That extrapolates a measurement taken at 0.00030625 down to
zero, in the direction where the same file's rules-floor reasoning says an absent
`isolate` means "rules only". Kept on the same grounds, and stated rather than
left in the code.

**The false-negative class the narrowing buys.** An overlap whose smaller
`isolate` is at or above one micrometre is now silent, and the fill is still never reconstructed,
so a pour pair that a CAM run merges anyway is not caught. Nor is a sub-rule but
non-zero `isolate` reported as a clearance finding, because answering that needs
the fill. The band is measured on both sides, and the trouble is elsewhere. Across the six
layer-instances with fabrication data, 0.00030625 merges three times, so the
threshold has to sit above it or all three real contacts go unreported; 0.3048 does
not merge twice, so it has to sit at or below that or those two become false
positives. Both bounds hold. What no threshold can fix is that the SAME value,
0.00030625, appears on a merge and on a non-merge (V3.4.0's two layers), so the
outcome is not a function of `isolate` and no cut through it gets all six right.
The micrometre inside the measured band is a manufacturing judgement, not a
documented Eagle boundary.

That overlap keys on the polygons' vertex rings: same-rank pours whose rings
miss by less than their drawn boundary stroke widths are not flagged, and
pinning whether Eagle treats a stroke graze as overlap needs an
Eagle-generated oracle board the corpus does not yet carry. Wires, vias and
pads against each other are fully covered.

### Gerber footprint inference on stripped films (via-vs-pad ambiguity)

An X2 gerber job carries the answer in the files: `%TO.P` names each pad's
refdes and pin, `%TO.N` names its net, and `%TA.AperFunction,ViaPad` marks a
via as a via. The reader uses all three, so on an X2 export (KiCad's default)
pad→refdes→pin→net binding and via-vs-pad classification come from the film
itself, exactly: no window, no inference.

The limitation is now confined to **stripped films** (exports run with
`--no-x2 --no-netlist`, or legacy CAM output that never carried attributes).
There the reconstruction windows each component's pads from its package-name
string, with a flat 4 mm fallback, and a stitching/thermal via inside that
window is hard to tell from a real pad, which inflates component pad counts.
The *connectivity* the simulator needs is already near-exact over located pads
(net-partition agreement runs to about 99% on the gated boards), so the
residual is a component-match accounting figure, not a correctness gap. If your
job hits it, the unlocking upload is one of: re-export the gerbers **with X2
attributes enabled** (KiCad: don't pass `--no-x2`/`--no-netlist`; Altium:
tick "Generate X2 attributes"), or supply the native layout file alongside the
fab folder.

### Capacitor ESR/ESL parasitics stay opt-in by design

The class inference was broadened (see "Recently closed"), but parasitics are
deliberately **not** on by default. The opt-in policy exists precisely so the
global solver behavior stays unchanged; flipping it on would alter every
transient/operating-point result across the corpus. The right broadening was
the inference quality, not the default-on switch. Intentionally not changed.

## Recently closed

Each of these was chased to ground truth, fixed with two-sided evidence, and is
pinned by tests. One line each; the tests are the record.

- **Gerber X2 attributes discarded wholesale**: the reader now parses
  `%TA.AperFunction` / `%TO.P` / `%TO.N` / `%TO.C` / `%TD` and binds
  pad→refdes→pin→net from the film (via flashes classify as vias, nets take
  the film's names, geometry-vs-film disagreements are named in notes),
  proven against the ZSWatch same-batch netlist oracle (405 pads, 100% name
  and partition agreement) with the stripped-film path pinned unchanged, by
  `tests/gerber_x2.rs` and the rs274x/connect unit tests.
- **Exposure-off macro primitives hulled over as solid copper**: a macro's
  punched-out void is now a real hole contour (paint-order aware; repainted
  or boundary-crossing clears stay conservatively solid), pinned two-sided by
  `macro_void_does_not_swallow_foreign_copper` and the macros unit tests.
- **`.gbrjob` manifest unread while inner-layer order was guessed from
  filename digits**: the manifest now classifies files and orders copper by
  the rank of (side, number), trusting numbers as physical positions only
  when contiguous (KiCad 9 writes internal IDs: L1/L5/L7/L4 on a four-layer
  board), pinned by `tests/gerber_gbrjob.rs` and the restored watchy
  closed-loop floor.
- **Aperture hole diameters read as copper and grid-footprint windows
  ignoring rotation**: a holed flash now carries its hole contour, and a
  header's pad window follows the P&P rotation, pinned by
  `aperture_hole_diameter_is_bare_board_not_copper` and
  `a_header_window_follows_its_stored_rotation`.
- **USB-C CC audit under-read on dual-receptacle boards**: the audit now reads
  every distinct receptacle and credits an Rd returning to any recognized
  ground (GNDA/AGND, GNDD/DGND, GNDPWR/PGND, VSS, numbered grounds), proven on
  Lily58 Pro V2 (both halves independently terminated, clean) and pinned by
  `usb_c_double_termination::lily58_dual_receptacle_both_halves_terminated`
  and `checks::usb_c::tests::ground_names_recognise_the_gnd_family_only`.
- **Schematic bus-alias references not expanded**: `{ALIAS}` references now
  expand through the per-sheet alias map, including vector members and
  cross-sheet references, pinned by the expander unit tests and the
  `bus_alias_top.kicad_sch`/`bus_alias_child.kicad_sch` fixture pair.
- **Capacitor ESR/ESL class inference too narrow**: metric size codes, a 0201
  class, and explicit tantalum-vs-electrolytic markers are now recognized
  (still opt-in, no default solver change), pinned by the
  `footprint_inference_*` tests.
- **Crystal/resonator mis-bound as a gigafarad capacitor**: a frequency-valued
  or crystal-prefixed 2-pin part (`Y`/`CRYSTAL`/`XTAL`/`RESONATOR`) now binds
  `Ignore` before the passive first-char fallback could read `16Mhz` as 16
  megafarads and collapse the whole co-sim solve to ~0 V; a `600@100MHz`
  ferrite bead is not caught, load caps stay, and the fix is pinned by the
  classification and fallback-regression tests.
- **MCP4728 not emulated as an I2C slave**: it now is, as a spec-driven
  `RegisterMapSensor` (`testdata/sensor-specs/mcp4728.toml`) with three
  instances auto-attached at 0x60/0x61/0x62 and VOUT drivers into the analog
  solve. The pinning tests (`mcp4728_cosim.rs`,
  `tarski_firmware_cosim.rs::host_programs_dacs_over_serial`) live in the
  development repository only, because their board fixtures cannot be
  redistributed (see [PRIVATE_SUITE.md](PRIVATE_SUITE.md)).
- **Bit-banged SPI at sub-chunk timing collapsed in full co-sim**: the
  scheduler now resolves ordered pin edges within a chunk, so firmware-driven
  bit-banged buses work end-to-end, pinned by the shipped tests
  `crates/hauksbee-engine/tests/bitbang_spi_cosim.rs` (sub-chunk SCLK edges
  clock a register-exact WHO_AM_I + burst read, twice, proving CS re-framing)
  and `soft_i2c_cosim.rs`, plus `cosim_spi_cs_frames_transactions.rs` for
  chunk-boundary framing.
