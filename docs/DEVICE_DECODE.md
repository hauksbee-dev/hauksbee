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

In line with the project rule (`docs/KNOWN_FAULTS_VALIDATION.md`), the check fires
ONLY when:

1. the part is **positively identified** (its value / MPN string contains the
   part token, e.g. `CYPD3177`); identification is by value string, not by a
   model-DB entry, so it works on layout-only extraction and does not depend on a
   device model existing; AND
2. the configuration divider on the pin **resolves to concrete resistor values**:
   a parseable pull-up to the reference rail and a parseable pull-down to ground,
   so the pin voltage is actually computable.

If the divider cannot be resolved to known values, the check stays **silent**. A
check that cannot resolve the divider does not fire. It is **DNP-aware**: a
Do-Not-Populate resistor is not assembled, so it is not counted in the divider.

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

So a VBUS_MIN strapped higher than a selected VBUS_MAX detent silently clamps that
detent: the part requests VBUS_MAX, not the (higher) labelled voltage.

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

- It does not read the **silk-screened voltage label** next to each detent, so it
  cannot say "the detent labelled 15 V decodes to 12 V" from the netlist alone.
  It states which bands the selector can reach and which it cannot; a reviewer
  reads off that "20 V" is unreachable. The exact "detent N labelled X decodes Y"
  mismatch is proven in the unit tests against the hunt's hand-derived numbers.
- A selector wired through a part that does not bind as a multi-pad switch on the
  config net is not enumerated. The static (permanent) divider is still decoded
  and the Note-1 check still runs.

## The hunt this reproduces, and an important correction

The seed comes from `docs/hunts/pd-sink-trigger.md` CANDIDATE 4: the
INGBZGMBH PD-Sink-Trigger-Board's rotary VBUS selector.

The hunt's decode arithmetic is reproduced exactly by the unit tests
(`hunt_detent_voltages_decode`, `divider_voltage_matches_hand_math`,
`table2_band_edges_decode`): with a **permanent 10 k pull-down (R12) populated**,
detent 4 ("15 V") decodes to the 12 V band and detent 5 ("20 V") decodes to the
19 V band, and a hard-wired VBUS_MIN = 19 V triggers the Note-1 override. The
synthetic-board tests confirm the check fires (Medium unreachable-top-band + High
Note-1) on exactly that topology.

**However, running the as-built board surfaces a correction to the hunt.** In
both `USB-C_PD_Trigger.kicad_pcb` and `.kicad_sch`, **R12 (the permanent VBUS_MAX
pull-down) and R9 (the VBUS_MIN pull-up) are marked Do-Not-Populate**
(`(attr smd exclude_from_bom dnp)` / `(dnp yes)`). The hunt doc assumed R12 is
"always present"; the design files say it is not assembled.

With DNP honored:

- VBUS_MAX keeps only R11 (5.1 k pull-up) plus the per-detent switched single
  pull-down. That is the datasheet-correct single-pull-down-per-detent topology;
  the open detent floats to ~3.3 V (the 20 V band).
- VBUS_MIN loses its pull-up (R9 DNP), leaving only R10 (10 k to ground), so it
  sits near 0 V (the 5 V band) and never triggers Note 1.

So the device-decode check correctly produces **no finding** on the real board:
the permanent pull-down it would need to compute the faulty static VBUS_MAX
divider is unpopulated. Firing here would be a false positive. This is the
zero-false-positive discipline doing its job, and it is also a genuine result:
the hunt's "confirmed defect" was derived from treating two DNP parts as
populated. The check's decode math (the part that *is* portable and provable) is
locked down by the unit tests; the check's silence on the real board is the
honest, correct call given the assembled BOM.

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
- `dnp_permanent_pulldown_makes_check_silent` - the real board's actual state
  (R12 / R9 DNP) leaves the static divider uncomputable, so the check is silent.
- `non_cypd_part_is_ignored` - a non-CYPD board is never touched.

## Adding a part

Write a decoder function mirroring `check_cypd3177`: identify the part by value
string, find its config pins by function / net name, resolve each divider, decode
against the part's own band table, and emit `LintCheck::DeviceDecode` findings.
Add unit tests for the band edges and at least one fire / one silent case. Keep
the identification value-string based so it does not depend on a model-DB entry.
