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

## Pin-role inference rules (`pin_rules.toml`)

A layout-only board source (a `.kicad_pcb`, or Board-as-Code decompiled from
one) gives a component pads with bare *numbers* and no electrode role: a diode
footprint has pads `1`/`2`, never `anode`/`cathode`. A netlist carries the role
(`pinfunction "A"`/`"K"`), but a layout round-trip drops it, and a role-dependent
binder (diode, BJT, MOSFET) can no longer bind the part.

`db/pin_rules.toml` is the configurable inference layer that recovers the role
from the footprint + part kind + pad count. Every role assigned this way is a
**guess**: the binder emits a warning naming the component, pad, role, and the
rule that matched (shown in `hauksbee run --report` and `--lint`). An explicit
pin-function always wins, so a part with `A`/`K` pins binds with no warning.

```toml
[[pin_rules]]
# Stable id, surfaced verbatim in the guess-warning.
id = "diode_2pin_k1_a2"
description = "2-pin diode: pad1=cathode, pad2=anode (KiCad Device:D 1=K 2=A)"

# A rule matches when EVERY populated condition holds. At least one is required.
footprint_re = "(?:^Diode_|SOD-?|SMA|SMB|MELF|DO-|^D_)"   # case-insensitive
kind         = "diode"        # resolved part kind (diode | bjt_npn | nmos | …)
pad_count    = 2              # exact pad count

# Pad number -> role. The roles a binder expects:
#   diode      : anode, cathode
#   bjt_npn/pnp: base, emitter, collector
#   nmos/pmos  : gate, source, drain
roles = { "1" = "cathode", "2" = "anode" }
```

### Extending or overriding (no recompile)

Drop a `pin_rules.toml` (or any file containing a `[[pin_rules]]` array) into a
model directory:

  1. `~/.hauksbee/models`
  2. `~/.config/hauksbee/models`
  3. any `--models-dir` path

Those rules are layered **ahead** of the built-ins, so a user rule with the same
footprint family overrides the default (e.g. to flip the diode convention for a
house footprint), and a rule for a new footprint family simply extends coverage.
Rules are tried in order; the first that matches and maps the requested pad wins.

## Physical-range validation

The `model-extract` tool and the test suite enforce these sanity bounds before
accepting a model entry:

These bounds are the single source of truth in `crates/hauksbee-models/src/validation.rs`
(regenerate this table from there if they drift — `hauksbee models lint` enforces them):

| param       | min    | max    | note                                  |
|-------------|--------|--------|---------------------------------------|
| is          | 1e-20  | 1e-3   | diode / BJT sat current               |
| n / nf      | 0.5    | 3.0    | emission coefficient                  |
| rs          | 0.0    | 1000.0 | diode series resistance (Ω)           |
| cjo         | 0.0    | 1e-6   | diode junction capacitance (F)        |
| bf          | 1.0    | 2000.0 | BJT forward beta                      |
| vaf         | 1.0    | 500.0  | Early voltage (V)                     |
| rb/rc/re    | 0.0    | 1e6    | BJT parasitic resistances (Ω)         |
| vto         | -10.0  | 10.0   | MOSFET threshold (V)                  |
| kp          | 1e-6   | 1000.0 | MOSFET transconductance (power FETs run into the tens–hundreds) |
| lambda      | 0.0    | 1.0    | MOSFET channel-length modulation      |
| vout        | 0.5    | 30.0   | LDO output voltage (V)                |
| dropout_v   | 0.0    | 10.0   | LDO dropout (V)                       |
| iq_a        | 0.0    | 1.0    | LDO quiescent current (A)             |
| gain        | 1.0    | 1e9    | op-amp open-loop gain                 |
| rail_lo/hi  | -60.0  | 60.0   | op-amp output rails (V, lo < hi)      |
| out_lo/hi   | -60.0  | 60.0   | comparator output levels (V, lo < hi) |
| hysteresis  | 0.0    | 5.0    | comparator hysteresis (V)             |
| ron         | 0.01   | 10000.0| switch on-resistance (Ω, ron < roff)  |
| roff        | 1e3    | 1e12   | switch off-resistance (Ω)             |

## `mpn_re` narrows, it does not widen

Worth knowing before adding a rule, because the shape of it is a trap. All
populated match rules are **ANDed**, and a component with no MPN property is
compared against `mpn_re` as the empty string. So adding `mpn_re` to an entry
that already has a `value_re` cannot make it match more parts; it makes the
entry match **nothing at all** on any board that carries no MPN, which is every
layout-only board (`.kicad_pcb`, `.brd`, gerbers).

Use `mpn_re` only when it is the *only* rule, or when every board that should
bind genuinely carries the MPN. Otherwise the portable rule is `value_re`, and a
part number that appears in the value field is already covered by it.
