# Device-decode checks

A class of parts read an analog **resistor-divider voltage on a configuration
pin** and decode it against a published table of bands to pick an operating mode.
USB-PD sink controllers (the VBUS voltage they request), programmable LDOs (the
output voltage they regulate to), address-strapped peripherals, and mode-pin
codecs all work this way. The board author chooses divider resistors to land the
pin in the band for the mode they want.

If the chosen values miss the intended band, the part **silently selects the
wrong mode**. This is invisible to a value/short sweep: every resistor is in
spec, every net connects. The fault is purely that the divider decodes to a band
the author did not intend. The device-decode check class catches exactly that.

Source: `crates/hauksbee-engine/src/checks/device_decode.rs`. Surfaced through
`--lint` (it emits `NetLintReport` findings under the `device_decode` check tag),
alongside the strap-pin lint and the MCU resource-conflict check.

## Honest scope: per-part, grows incrementally

There is **no generic "decode any config pin" engine**. Each part has its own
config pins, its own band table, and its own consistency rules. So each supported
part is a hand-written decoder seeded from its datasheet. Adding coverage means
adding a part, deliberately, one at a time. This is by design, not a stub.

Today exactly **one** part is seeded: the Cypress / Infineon **CYPD3177**
(EZ-PD BCR) USB-C PD sink controller.

## Zero-false-positive discipline (binding)

In line with the project's zero-false-positive rule, the check fires
ONLY when:

1. the part is **positively identified** (its value / MPN string contains the
   part token, e.g. `CYPD3177`). Identification is by value string, not by a
   model-DB entry, so it works on layout-only extraction and does not depend
   on a device model existing. AND
2. the configuration divider on the pin **resolves to concrete resistor values**:
   a parseable pull-up to the reference rail and a parseable pull-down to ground,
   so the pin voltage is actually computable.

If the divider cannot be resolved to known values, the check stays **silent**. A
check that cannot resolve the divider does not fire.

It is **DNP-aware**, but through the shared DNP policy rather than a rule of its
own: `resistor_ohms` skips any component still carrying the Do-Not-Populate flag
when the check runs. What that means on a given run depends on the policy, and the
default is `FitExceptLinks` (`crates/hauksbee-extract/src/dnp.rs`), which clears
the flag on every DNP part that is not a near-zero-ohm link *before* the checks see
the board. So by default a DNP divider resistor **is** counted, and
`--honour-dnp` is what leaves it out. The walkthrough below is exactly this
difference on a real board.

## CYPD3177 seed

### Table 2 (EZ-PD BCR, VBUS_MAX bands, verbatim, mV; VDDD = 3.3 V reference)

| Setting | Band (mV)     |
|---------|---------------|
| 5 V     | 0 - 248       |
| 9 V     | 249 - 786     |
| 12 V    | 787 - 1347    |
| 15 V    | 1348 - 1920   |
| 19 V    | 1921 - 2778   |
| 20 V    | >= 2779       |

VBUS_MIN uses the same band structure, so one decoder serves both pins. The pin
voltage of a single-pull-up / single-pull-down divider is
`Vpin = VDDD * Rpd / (Rpu + Rpd)`, with `Rpd` the effective pull-down (the
permanent pull-down in parallel with any switched leg).

### Datasheet Note 1

> If VBUS_MIN is more than VBUS_MAX, the setting on VBUS_MAX is used as both
> minimum and maximum.

So a VBUS_MIN strapped higher than a selected VBUS_MAX detent silently clamps
that detent: the part requests VBUS_MAX, not the (higher) labeled voltage.

### What the check resolves automatically

- VBUS_MAX / VBUS_MIN pins, by pin function name (`pinfunction "VBUS_MAX"`), or
  by net name fallback (a net whose leaf name is `VBUS_MAX` / `VBUS_MIN`).
- The fixed divider on each pin: the pull-up resistor to the 3.3 V reference and
  the permanent pull-down resistor to ground (each may combine parallel
  resistors).
- **Switch-selectable detents**, when the selector is a multi-pad switch
  component (rotary / slide, ref `SW*`, or a switch footprint) whose common pad
  sits on the config net: each of its other pads is decoded as one detent, the
  extra pull-down being a direct ground (0 ohm), a single resistor to ground, or
  an open leg.
- Two findings:
  - **Unreachable top band** (Medium): the selector's reachable detents do not
    include the 15 V and/or 20 V band, so the headline capability cannot be
    requested. Reported as the set of reachable bands plus the missing one(s).
  - **Note-1 override** (High): VBUS_MIN decodes above one or more reachable
    VBUS_MAX detents, so those detents are silently clamped.

### What the check cannot resolve (honest limits)

- It does not read the **silk-screened voltage label** next to each detent, so
  it cannot say "the detent labeled 15 V decodes to 12 V" from the netlist
  alone. It states which bands the selector can reach and which it cannot. A
  reviewer reads off that "20 V" is unreachable. The exact "detent N labeled
  X decodes Y" mismatch is proven in the unit tests against hand-derived
  numbers.
- A selector wired through a part that does not bind as a multi-pad switch on
  the config net is not enumerated. The static (permanent) divider is still
  decoded and the Note-1 check still runs.

## The board this was calibrated on

The seed case is the INGBZGMBH PD-Sink-Trigger-Board's rotary VBUS selector
(a CYPD3177 with a per-detent switched divider).

The decode arithmetic is locked down by the unit tests
(`hunt_detent_voltages_decode`, `divider_voltage_matches_hand_math`,
`table2_band_edges_decode`): with a **permanent 10 k pull-down (R12) populated**,
detent 4 ("15 V") decodes to the 12 V band and detent 5 ("20 V") decodes to the
19 V band, and a hard-wired VBUS_MIN = 19 V triggers the Note-1 override. The
synthetic-board tests confirm the check fires (Medium unreachable-top-band + High
Note-1) on exactly that topology.

On the board **as shipped for assembly**, that topology is not built. In both
`USB-C_PD_Trigger.kicad_pcb` and `.kicad_sch`, **R12 (the permanent VBUS_MAX
pull-down, 10 k) and R9 (the VBUS_MIN pull-up, 5.1 k) are marked Do-Not-Populate**
(`(attr smd exclude_from_bom dnp)` / `(dnp yes)`), along with R16 and R19 on the
ISNK current-sense straps, which this check does not read.

So what the run reports depends entirely on the DNP policy, and both answers are
correct answers to different questions.

**Default policy: the check fires.** hauksbee assumes an unpopulated footprint
gets stuffed eventually, so R12 and R9 come back and the faulty static divider is
the one that gets decoded:

```
$ hauksbee run USB-C_PD_Trigger.kicad_pcb --lint
do-not-populate: DNP parts are simulated as fitted (they are usually placed eventually), except near-zero-ohm links, which stay open because fitting one merges the nets it bridges
  fitted:    R19 (10k), DNP, fitted by default
  fitted:    R12 (10k), DNP, fitted by default
  fitted:    R16 (5k1), DNP, fitted by default
  fitted:    R9 (5k1), DNP, fitted by default
net-lint: 2 finding(s)
  [medium] device_decode - U1 VBUS_MAX selector on net 'VBUS_max' decodes (Table 2) to {5V [SW1.1 (GND) -> 0 mV], 9V [SW1.2 (R13) -> 499 mV], 12V [SW1.3 (R14) -> 908 mV], 12V [SW1.4 (R15) -> 1315 mV], 19V [SW1.5 (open) -> 2185 mV], 19V [SW1.NC -> 2185 mV]}, but cannot reach {15V, 20V}: the divider's permanent pull-down plus switched-parallel legs cannot reproduce the datasheet's per-detent codes, so the top setting(s) are unreachable
  [high] device_decode - U1 VBUS_MIN net 'Net-(U1-VBUS_MIN)' decodes (Table 2) to 19V (2185 mV), GREATER than 4 of the VBUS_MAX selector's reachable detents (top band 19V): per EZ-PD BCR datasheet Note 1 (VBUS_MIN > VBUS_MAX), VBUS_MAX is used as both minimum and maximum, so the hard-wired VBUS_MIN silently overrides and defeats those selector positions
note: gate-grade finding(s) above, but this is a report command so the exit code is 0. Add --strict to exit 2 on them (exit contract: 0 = clean or report-only, 1 = input error such as a missing or unreadable file, 2 = findings under --strict, 3 = invalid for analysis), or gate CI with hauksbee-ci.
```

Those are the same two findings, with the same detent voltages, that the unit
tests derive by hand. The real board reproduces them on the default policy.

**`--honour-dnp`: the check goes quiet.** Build only what the fab would build and
the divider the finding rests on is not there:

```
$ hauksbee run USB-C_PD_Trigger.kicad_pcb --lint --honour-dnp
do-not-populate: DNP parts are left out of the simulation, matching the board file
  left open: R19 (10k), DNP, left open
  left open: R12 (10k), DNP, left open
  left open: R16 (5k1), DNP, left open
  left open: R9 (5k1), DNP, left open
net-lint: no findings.
```

- VBUS_MAX keeps only R11 (5.1 k pull-up) plus the per-detent switched single
  pull-down. That is the datasheet-correct single-pull-down-per-detent topology,
  and with no static pull-down the fixed divider is uncomputable, so there is
  nothing to judge. The open detent floats to ~3.3 V (the 20 V band).
- VBUS_MIN loses its pull-up (R9 DNP), leaving only R10 (10 k to ground), so it
  sits near 0 V (the 5 V band) and never triggers Note 1.

The split is deliberate. DNP carries two opposite meanings in practice, "not on
this assembly BOM but it will be there" and "this link is deliberately open", and
hauksbee cannot tell which one a given footprint means. Defaulting to fitted
surfaces the latent fault, which is the useful answer for a board still in design:
stuff R12 and R9 and the selector really is mis-coded. `--honour-dnp` answers the
other question, what the fab builds today. Neither is a false positive, because
every run prints the policy line and the per-part fitted/left-open decision above
the findings, so which board was analysed is never in doubt. `--fit R12` /
`--no-fit R12` override a single part by name.

## Tests

`crates/hauksbee-engine/src/checks/device_decode.rs` `#[cfg(test)]`:

- `table2_band_edges_decode` - the six Table-2 bands at every edge.
- `hunt_detent_voltages_decode` - the five hand-derived detent voltages
  (0/499/908/1315/2185 mV) decode to 5/9/12/12/19 V, proving the detent-4 and
  detent-5 mis-decode.
- `divider_voltage_matches_hand_math` - `Vpin` for each detent's effective
  pull-down, within ~2 mV of the hand math.
- `note1_override_fires_when_min_exceeds_max` - synthetic faulty board (R12
  populated, VBUS_MIN = 19 V) fires the High Note-1 finding.
- `unreachable_top_band_fires_medium` - the same board fires the Medium finding.
- `clean_config_is_silent` - a consistent / unresolvable config is silent.
- `dnp_permanent_pulldown_makes_check_silent` - the same board with R12 / R9 still
  flagged DNP, the state `--honour-dnp` produces: the static divider is
  uncomputable, so the check is silent.
- `non_cypd_part_is_ignored` - a non-CYPD board is never touched.

## Adding a part

Write a decoder function mirroring `check_cypd3177`: identify the part by
value string, find its config pins by function / net name, resolve each
divider, decode against the part's own band table, and emit
`LintCheck::DeviceDecode` findings. Add unit tests for the band edges and at
least one fire / one silent case. Keep the identification value-string based
so it does not depend on a model-DB entry.
