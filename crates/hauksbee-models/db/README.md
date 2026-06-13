# hauksbee-models built-in database

Each TOML file in this directory defines one or more model entries. All files
are embedded at compile time via `include_str!` and merged into the built-in
library at startup.

## File organisation

Files are grouped by device family for maintainability. Names are informative
only; all entries across all files are merged into a flat lookup table.

## Entry schema

```toml
[[models]]
# Unique identifier used in diagnostics and cross-references.
id = "bc847"

# Component kind — drives which params fields are required.
# passive | diode | bjt_npn | bjt_pnp | nmos | pmos | vreg | opamp |
# comparator | analog_switch | digital | dac | adc | shift_register |
# mcu | connector | ignore
kind = "bjt_npn"

# Human-readable description shown in resolution reports.
description = "BC847 NPN small-signal SOT-23"

# ── Match rules ──────────────────────────────────────────────────────────────
# All populated rules are ANDed; the entry matches only when every rule fires.
# At least one match rule must be present.

[models.match]
# Exact KiCad lib_id string ("Device:Q_NPN_BCE") or prefix ending with ":".
lib_id   = "Device:Q_NPN_BCE"            # exact
# lib_id = "Device:"                     # prefix — matches any Device:* lib_id

# Regex matched against the component Value field (case-insensitive).
value_re = "^BC84[5-9]$"

# Regex matched against the Footprint string, e.g. "SOT-23".
footprint_re = "SOT-23"

# Regex matched against a MPN / part-number property if present.
mpn_re   = "BC847"

# ── Parameters ───────────────────────────────────────────────────────────────
# Required fields depend on `kind`; extra fields are silently preserved.
# Cite the datasheet revision and table in comments wherever you fill in a
# specific numeric value so future maintainers can verify it.

[models.params]
# BJT Gummel-Poon basics (NPN and PNP share the same param names).
# Source: BC847 datasheet, Nexperia, Rev. 11 2019-01-30, Table "SPICE model"
is    = 1.0e-14   # saturation current (A)
bf    = 150.0     # forward beta (ideal)
nf    = 1.0       # forward emission coefficient
vaf   = 80.0      # forward Early voltage (V); "VAF" in SPICE
br    = 4.0       # reverse beta
rb    = 10.0      # base ohmic resistance (Ω)
rc    = 1.0       # collector ohmic resistance (Ω)
re    = 0.5       # emitter ohmic resistance (Ω)
cje   = 9.0e-13   # B-E junction capacitance (F)
cjc   = 5.0e-13   # B-C junction capacitance (F)
tf    = 3.0e-10   # forward transit time (s)

# ── Pin map ───────────────────────────────────────────────────────────────────
# Maps KiCad pad numbers (strings) to simulation-node roles.
[models.pins]
"1" = "base"
"2" = "emitter"
"3" = "collector"
```

## Kind-specific required params

| kind            | required params (minimum)                                        |
|-----------------|------------------------------------------------------------------|
| passive         | (none — value is parsed from the component Value field)          |
| diode           | is, n, rs                                                        |
| bjt_npn/pnp     | is, bf, nf, vaf                                                  |
| nmos/pmos       | vto, kp                                                          |
| vreg            | vout, dropout_v, iq_a                                            |
| opamp           | gain, rail_lo, rail_hi                                           |
| comparator      | out_lo, out_hi, hysteresis                                       |
| analog_switch   | ron, roff, vth                                                   |
| digital         | voh, vol, vih, vil, tpd_s, supply_pin, gnd_pin                  |
| dac             | bits, vref_pin, sda_pin, scl_pin                                 |
| adc             | bits, vref, channels                                             |
| shift_register  | bits, tpd_s, supply_pin, gnd_pin                                 |
| mcu             | backend (e.g. "simavr:atmega328p")                               |
| connector       | (none — pad map is the useful part)                              |
| ignore          | (none — component is silently skipped during extraction)         |

## Physical-range validation

The `model-extract` tool and the test suite enforce these sanity bounds before
accepting a model entry:

| param  | min        | max      | note                       |
|--------|------------|----------|----------------------------|
| is     | 1e-20      | 1e-3     | diode / BJT sat current    |
| n      | 0.5        | 3.0      | emission coefficient       |
| bf     | 1.0        | 2000.0   | BJT forward beta           |
| vaf    | 1.0        | 500.0    | Early voltage (V)          |
| vto    | -10.0      | 10.0     | MOSFET threshold (V)       |
| kp     | 1e-6       | 1.0      | MOSFET transconductance    |
| vout   | 0.5        | 30.0     | LDO output voltage (V)     |
| ron    | 0.01       | 10000.0  | switch on-resistance (Ω)   |
| roff   | 1e3        | 1e12     | switch off-resistance (Ω)  |
