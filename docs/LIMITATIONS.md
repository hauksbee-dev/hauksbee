# Limitations: triage and closure

hauksbee's other docs each record the honest limitations of their own surface.
Several of those felt feasible to close. This doc is the triage: what was
chased to ground truth and fixed, and what is deferred with the reason. The bar
for "fixed" is the project's: every change covered by a test, no false positive
introduced into any lint/check, and any flipped result two-sided with file-level
evidence.

The governing rule throughout is the Tarski meta-lesson: a surprising pass or
fail is presumed a defect in *our* tool until chased to ground truth. Two of the
fixes below began as recorded "honest limitations" and turned out, once chased,
to be hauksbee defects.

## Fixed

### 1. USB-C CC audit under-read on dual-receptacle boards

- **Was** (docs/FAMOUS_SWEEP.md, round 3): "the USB-C CC audit under-reads the
  device-side Rd on a board with two USB-C receptacles (Lily58's two halves);
  the board is clean ... the audit just does not credit them."
- **Chased to ground truth.** Two compounding causes, both hauksbee defects:
  1. `audit_cc_termination` resolved a *single* best-scoring receptacle, so the
     second receptacle's independent Rd was never read; and
  2. one half's Rd resistors (R11/R12 on J6) return to `GNDA`, a secondary
     analog ground, which the audit's GND lookup did not recognise, so even that
     half in isolation read as un-terminated.
- **Fix.** Audit every distinct receptacle (`all_receptacle_cc_nets`); credit a
  Rd that returns to any recognised ground (`is_ground_name` / `ground_net_ids`
  cover the GND family: GNDA/AGND, GNDD/DGND, GNDPWR/PGND, VSS, numbered
  grounds), kept conservative so a non-ground net merely containing "GND" is not
  credited. `CcTerminationAudit` keeps `cc1`/`cc2` (primary receptacle) for
  existing callers and adds `receptacles: Vec<ReceptacleCc>` plus
  `all_receptacles_terminated()`.
- **Two-sided evidence** on `lily58/Pro_V2/Pro_V2.kicad_pcb`:
  - Before: one receptacle, `ext_rd = None` on both CC pins (under-read).
  - After: J1 and J6 both reported, each with an independent 5.1 kΩ Rd on CC1 and
    CC2; `has_double_termination = false`, `all_receptacles_terminated = true`.
- **Tests.** `usb_c_double_termination::lily58_dual_receptacle_both_halves_terminated`
  (corpus-gated), `checks::usb_c::tests::ground_names_recognise_the_gnd_family_only`.
  The RPi 4 / ZSWatch DevKit / mainboard CC tests are unchanged and green.

### 2. Schematic bus-alias references not expanded

- **Was** (docs/SCHEMATICS.md): "Bus aliases referenced as `{ALIAS}` are not
  expanded ... untested rather than known-broken."
- **Fix.** Thread the per-sheet `bus_aliases` map through bus expansion:
  `expand_bus_aliased` takes an alias resolver, and `expand_bus_on_sheet` looks a
  bare group-token up as an alias (each alias member token itself expanded, since
  an alias member can be a vector like `A[7..0]`). All three label / sheet-pin
  expansion sites use it; plain `expand_bus` keeps its no-alias signature.
- **Tests.** Expander unit tests (`group_bus_expands_an_alias_reference`,
  `group_bus_mixes_alias_with_inline_members`, `unknown_alias_token_stays_a_literal_member`)
  and an end-to-end fixture pair (`bus_alias_top.kicad_sch` /
  `bus_alias_child.kicad_sch`) exercising a `MEM{ADDR}` reference crossing a sheet
  boundary; `fixture_bus_alias_crosses_sheet` would fail if the alias were left
  literal. The six corpus cross-validations are unchanged.

### 3. Capacitor ESR/ESL class inference too narrow

- **Was** (docs/TRANSIENTS.md): inference recognised only imperial MLCC size
  codes and a coarse polarised bucket.
- **Fix.** Broaden `class_from_footprint`: recognise metric size codes
  (1005/1608/2012/3216/3225 Metric) as their imperial-equivalent MLCC class; add
  a 0201 MLCC class (highest ESR of the ladder); distinguish tantalum from
  aluminium electrolytic by explicit markers (TANTALUM/TANT, EIA case codes,
  CASE-A..D) rather than only by value. Parasitics stay **opt-in**, so no default
  solver result changes.
- **Tests.** `footprint_inference_handles_metric_codes_and_0201`,
  `footprint_inference_distinguishes_tantalum_from_electrolytic`,
  `footprint_inference_falls_back_to_default_mlcc`; the existing bucket and
  ordering tests are unchanged.

### 4. Crystal / resonator mis-bound as a gigafarad capacitor, silently collapsing co-sim

- **Was** (`crates/hauksbee-engine/src/binder.rs`): a 2-pin crystal whose
  reference starts with `C` (`Crystal1`) hit the passive first-char fallback —
  `C` → capacitor — and `parse_value("16Mhz")` read `16M` as 16e6 while dropping
  the `hz`, binding the part as a **16-megafarad** capacitor (the `--report`
  table literally showed `analog C 16000000000000.000µF`). A capacitor that
  large makes the MNA solve singular/ill-conditioned, collapsing every node
  voltage to ~0. In firmware co-sim that reads as **every** MCU-driven net
  "never driven / Hi-Z" — a silent, board-wide false negative on essentially any
  crystal-clocked MCU board.
- **Chased to ground** on `explosion33/RocketryIgniter` (see
  [`hunts/HUNT_2026-06-30.md`](hunts/HUNT_2026-06-30.md)): a strong-output test
  firmware on the same pin also read 0 V (ruling out the firmware), and a board
  with the crystals stripped drove the pin to 5 V (isolating the cause).
- **Fix** (`39128bb`): detect crystals/resonators **before** the passive
  heuristic — a frequency-valued part (`value_is_frequency`, a whole-value match
  so a `600@100MHz` ferrite bead is *not* caught) or a crystal reference prefix
  (`Y`/`CRYSTAL`/`XTAL`/`RESONATOR`) — and bind them `ComponentKind::Ignore`
  (high-impedance). The clock comes from the MCU model and a crystal's motional
  R-L-C is negligible at the solver's operating point, so removal is exact here;
  load caps (genuine passives) stay. Genuine capacitors are untouched.
- **Tests.** frequency-vs-passive classification (incl. ferrite negatives); a
  fallback regression asserting a `C`-named crystal binds `Ignore` while
  `C7=22pF` stays `Passive`; the crystal-load-cap SI checks are unaffected.

### 5. Output-low pins read as "never driven" (`pinMode(OUTPUT)` not modelled)

- **Was** (`crates/hauksbee-mcu/src/avr.rs`): the AVR backend hooked only the
  PORT register, so `pinMode(pin, OUTPUT)` — a DDR-register write that leaves
  PORT at 0 — produced no edge. An output-low-held pin (a gate driven LOW and
  left there) thus looked Hi-Z, so a firmware *correctly* holding a control net
  safe-low could be flagged as floating (e.g. the RocketryIgniter `IgnitTwo`
  gate, the safe one).
- **Fix** (`a8d7b35`): also subscribe to `IOPORT_IRQ_DIRECTION_ALL`; when a pin
  becomes an output it fires a pin-change at its PORT level, so output-low pins
  are modelled as driven LOW. Verified against boot_gate pass/fail, blinky, the
  AVR I2C/SPI co-sims, and the igniter (the false `IgnitTwo` flag is gone; a
  genuinely-never-configured gate still flags).

## Deferred (genuinely hard or wrong to "fix"), with reason

### Legacy KiCad-5 `.sch` (non-s-expression) reader

The Olimex ESP32-EVB revs A..K2 ship `EESchema Schematic File Version 2` ASCII
schematics. A reader for this format would need a second, independent schematic
front-end: its own `$Comp`/`$Sheet`/wire/label parser, a fresh geometric
netliser, and library-cache handling, all to the same exactness bar as the
s-expr reader (the corpus standard is *zero* split/merged nets against the PCB).
That is a large, self-contained effort with no shared code path with the s-expr
reader, and the planned Olimex rev-B "I2C swap" validation it would unblock is
not in the corpus changelog text, so the payoff is a single uncertain row.
Deferred as the genuinely-hard, low-ROI item. The PCB extractor still handles
those boards' layouts; only their legacy schematics are out of scope.

### Renode ADC injection / I2C-SPI slave interception; QEMU ESP32 ADC and GPIO mailbox

- **Renode ADC (`set_analog_in`) and `on_i2c`/`on_spi`** are documented no-ops:
  Renode's ADC peripheral and bus-interception APIs are per-SoC, needing a
  per-platform peripheral map. **Renode is not installed in this environment**,
  so any change here could not be validated against ground truth, only written
  blind. The meta-lesson forbids shipping an emulator integration that cannot be
  run and checked. Deferred until it can be exercised against a live Renode.
- **QEMU ESP32 SAR ADC** is not modelled by the Espressif QEMU fork, so
  `set_analog_in` is a no-op there too (a silicon-model gap, not a wiring gap).
- **QEMU ESP32 GPIO** is observed through a firmware RAM mailbox because the fork's
  `esp32.gpio` model has no `GPIO_OUT_REG` read-back. **Empirically confirmed on
  the latest build** (QEMU 9.2.2 `esp_develop`, 2026-06-30): a host read of
  `GPIO_OUT_REG` (0x3FF44004) returns 0 via QMP `xp`, gdbstub `m`, *and*
  `qom-get` (the `esp32.gpio` object exposes no value property), and a host
  *write* to those registers is discarded — the model is write-effect-only with
  no host-visible state, while a RAM address round-trips perfectly. TCG memory
  plugins are disabled in the prebuilt (`-plugin help` → not enabled), and there
  is no QEMU source tree on disk, so the only real fix is a ~15-line device-model
  patch to the fork's `hw/gpio/esp32_gpio.c` (store + return `GPIO_OUT`/`ENABLE`,
  handling the W1TS/W1TC aliases) plus a rebuild — fully specified, with the
  exact registers and the matching backend change, in
  [`hunts/esp32-qemu-i2c-status.md`](hunts/esp32-qemu-i2c-status.md). Shipping the
  register-read backend path *without* a patched QEMU to validate it against
  would violate the no-unvalidatable-fixes rule, so it stays deferred — now with
  an exact, actionable spec rather than an open question. (The orthogonal
  discovery-path bug the spike surfaced — the loader still probing the
  pre-rename `~/.hauksbee-qemu-esp` — *was* fixed in `a8d7b35`: it now probes
  `~/.galvani-qemu-esp` first, legacy path as fallback.)

### Eagle copper-pour fidelity in DRC

An Eagle `.brd` stores only a signal pour's *requested outline*, never the poured
copper with its `isolate` antipads or `rank` arbitration. Checking pour-to-copper
shorts honestly would require re-pouring the board in Eagle and exporting the
computed polygons; that data is simply not in the source file. Treating the
outline as solid copper would manufacture false shorts on every trace that
legitimately crosses a pour, so pours stay excluded from the Eagle short test
(wires, vias and pads against each other are fully covered). This is a
data-availability limit, not a code limit. Deferred (unchanged).

### Gerber footprint inference (via-vs-pad ambiguity)

The reconstruction windows each component's pads from its package-name string,
with a flat 4 mm fallback. A stitching/thermal via inside that window is hard to
tell from a real pad without the netlist (the documented "honest weak spot"),
which inflates component pad counts. The *connectivity* the simulator needs is
already near-exact over located pads (net-partition agreement runs to ~99% on the
gated boards), so the residual is a component-match accounting figure, not a
correctness gap. Improving it robustly needs information the gerber alone does not
carry; the higher-ROI, lower-risk fixes were taken instead. Deferred (unchanged).

### Capacitor ESR/ESL applied "more broadly" by default

The class-inference *coverage* was broadened (see Fixed #3), but making
parasitics **on by default** was deliberately *not* done. The opt-in policy
exists precisely so the global solver behaviour stays unchanged; flipping it on
would alter every transient/operating-point result across the corpus and
introduce regressions. The right broadening was the inference quality, not the
default-on switch. Intentionally not changed.

## Recorded elsewhere (still open, owned by their own surface)

These are genuine open limitations the code carries today. Each is the
responsibility of the doc that owns the surface, and is listed here only so this
triage page is a complete index of what is *not* yet closed:

- **MCP4728 not emulated as an I2C slave** (`LOAD_DAC` NAKs): the QEMU backend
  does not intercept I2C, so an I2C DAC on the bus is not answered. See
  `docs/TARSKI_RESULTS.md` and `docs/TEST_CAMPAIGN.md`.
- **nRF5340 has no co-sim backend**: the Renode 1.16.1 portable build ships no
  nRF5340 platform, so the ZSWatch-class DISPLAY-EN fault stays a static miss.
  See `docs/MCU.md` and `docs/KNOWN_FAULTS_VALIDATION.md`.
- **Bit-banged SPI at sub-chunk timing collapses in full co-sim**: the 595 chain
  is covered by the model-level (PATH B) verification instead. See
  `docs/TEST_CAMPAIGN.md`.
- **PCB-only extraction has no pinfunctions**: multi-unit packages fall back to
  db pin maps when only a layout (no schematic netlist) is available; schematic
  netlists are authoritative when present. See `docs/TEST_CAMPAIGN.md` and
  `docs/FAMOUS_SWEEP.md`.
