# Device-decode checks

Some parts read a resistor-divider voltage on a configuration pin and decode
it against a datasheet band table to pick a mode: USB-PD sink controllers,
programmable LDOs, address-strapped peripherals. If the divider lands in the
wrong band the part silently selects the wrong mode, and no short/value sweep
can see it. This check class decodes the divider.

Invoke: `hauksbee run <board> --lint` (findings carry check tag
`device_decode`), or `--check`. Source:
`crates/hauksbee-engine/src/checks/device_decode.rs`.

## Scope

There is no generic engine; each part is a hand-written decoder. Seeded parts:

- **CYPD3177** (Infineon EZ-PD BCR) USB-C PD sink: VBUS_MAX / VBUS_MIN
  dividers decoded against datasheet Table 2 (5 V: 0-248 mV, 9 V: 249-786,
  12 V: 787-1347, 15 V: 1348-1920, 19 V: 1921-2778, 20 V: >= 2779, with
  VDDD = 3.3 V). `Vpin = VDDD * Rpd / (Rpu + Rpd)`, `Rpd` the permanent
  pull-down in parallel with any switched leg.
- **BQ2407x** charger TMR safety-timer resistor: fires only when the resistor
  lands outside the 18-72 kOhm band (`tMAXCHG = 10 x 48 s/kOhm x R`); a
  floating pin and a VSS tie are documented modes and stay silent.

## When it fires

Only when both hold:

1. the part is positively identified by its value/MPN string (no model-DB
   entry needed), and
2. the divider resolves to concrete resistor values (a parseable pull-up to
   the reference rail and a pull-down to ground).

Otherwise the check is silent. DNP resistors follow the shared DNP policy:
by default a DNP divider resistor is counted (`fit-except-links`), and
`--honour-dnp` leaves it out ([DNP.md](../ingest/DNP.md)).

## CYPD3177 findings

- Pins found by pin function (`VBUS_MAX` / `VBUS_MIN`) or by net leaf name.
- A multi-pad switch (`SW*` or a switch footprint) whose common pad sits on the
  config net is enumerated as detents: each other pad is a direct ground, a
  single resistor to ground, or open.
- **Unreachable top band** (medium): the reachable detents do not include the
  15 V and/or 20 V band.
- **Note-1 override** (high): VBUS_MIN decodes above one or more reachable
  VBUS_MAX detents, so those detents are clamped (datasheet Note 1).

Not resolved: the silk-screened label beside a detent (the check reports which
bands are reachable, not "detent N labelled X decodes Y"), and a selector
wired through a part that does not bind as a multi-pad switch.

Example (default policy, the DNP pull-down fitted):

```
[medium] device_decode - U1 VBUS_MAX selector on net 'VBUS_max' decodes (Table 2) to {5V [SW1.1 (GND) -> 0 mV], 9V [...], 12V [...], 12V [...], 19V [...]}, but cannot reach {15V, 20V}: ...
[high] device_decode - U1 VBUS_MIN net 'Net-(U1-VBUS_MIN)' decodes (Table 2) to 19V (2185 mV), GREATER than 4 of the VBUS_MAX selector's reachable detents ...
```

## Adding a part

Write a decoder mirroring `check_cypd3177`: identify the part by value string,
find its config pins by function or net name, resolve each divider, decode
against the part's band table, emit `LintCheck::DeviceDecode` findings, and
add unit tests for the band edges plus one fire and one silent case.
