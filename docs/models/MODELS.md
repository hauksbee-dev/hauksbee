# Device models: built-in, SPICE, and datasheet extraction

Every component the binder meets needs a simulation model. Physics arrives
by **four** authoring routes, but they collapse into **three** resolution
tiers: the codex-extracted models and the hand-written behavioural models
are both just TOML entries that land in the same user-TOML tier, so the
resolver does not treat them as separate sources.

The four authoring routes:

- **Built-in DB** (`crates/hauksbee-models/db/*.toml`): the curated library
  that ships with hauksbee. It covers the common families (BC847, 1N4148,
  7805, 74HC595, ATmega328P, and so on) plus passives resolved straight from
  the `Value` field.
- **Datasheet (codex) extraction** (`model-extract` binary): when a part is
  not in the DB, point hauksbee at the part's PDF datasheet and an LLM
  backend (codex by default) extracts a model entry in the same TOML
  schema. The result lands in `~/.hauksbee/models/` and loads as a
  user-dir entry.
- **Hand-written behavioural** TOML: a `[models.behavioral]` entry you
  author by hand for a power IC (see "Behavioural device models" below).
  hauksbee loads it from the same user model directories as the extracted
  models.
- **User SPICE**: a `.model` / `.subckt` card you supply always wins, so you
  can override anything with a vendor-provided SPICE deck.

The three resolution tiers, layered by priority (later wins):

```
builtin TOML DB   <   user TOML (extracted + hand-written)   <   user SPICE
   (lowest)                                                       (highest)
```

The resolution order itself lives in `ModelLibrary::resolve`
(`crates/hauksbee-models/src/lib.rs`), which returns a `source` of exactly
one of `"spice"`, `"user"`, or `"builtin"`: SPICE cards first, then user
TOML entries (where both extracted *and* hand-written behavioural models
land), then the built-in DB.

## Pointing hauksbee at a datasheet

```bash
# build the extractor
cargo build -p hauksbee-models --bin model-extract

# extract a model from a PDF datasheet
./target/debug/model-extract \
    --pdf testdata/datasheets/BC847.pdf \
    --part BC847 \
    --kind bjt_npn \
    --out-dir ~/.hauksbee/models       # default if omitted
```

`--kind` is one of: `passive | diode | bjt_npn | bjt_pnp | nmos | pmos |
vreg | opamp | comparator | analog_switch | digital | dac | adc |
shift_register | mcu | connector | ignore`.

The tool writes `<part>.toml` to the output directory. The library loads any
TOML in `~/.hauksbee/models/` as a user-dir entry the next time it builds,
so an extracted part becomes immediately resolvable by value/MPN.

## What gets extracted

The extractor pulls two things from the datasheet:

1. **SPICE-level parameters** for the device kind (`is`, `bf`, `nf`, `vaf`
   for a BJT; `is`, `n`, `rs` for a diode; `vout`, `dropout_v`, `iq_a` for
   an LDO; and so on). Where a value is not stated verbatim, the model
   derives it from a stated operating point (e.g. `is` from VBE at a known
   IC) and says so in a comment, or falls back to a family-typical value
   tagged `# estimated`.
2. **Absolute-maximum ratings** into `[models.ratings]`: `max_current_a`,
   `max_surge_current_a`, `max_power_w`, `max_voltage_v`,
   `max_junction_temp_c`. These feed the **stress monitor**
   (`crates/hauksbee-engine/src/stress.rs`): it checks the live operating
   point against them and raises faults when a part runs past its limits.
   An omitted field means "no limit known".

Every numeric line carries a comment citing where in the datasheet it came
from, so an extracted model stays auditable.

## The pipeline, end to end

`crates/hauksbee-models/src/bin/model_extract.rs`:

1. **PDF to text** through `pdftotext` (if present). When it is absent, the
   backend is told to read the PDF directly from its working directory.
2. **Prompt** built per kind, listing the required params, the ratings to
   pull, and the physical bounds each value must respect.
3. **Backend call**:
   - **codex** (default): `codex exec --sandbox workspace-write
     --skip-git-repo-check --cd <pdf_dir>`. stdin is closed so codex does
     not block; the final agent message (clean TOML) comes back on stdout
     while session logging goes to stderr. A hard timeout (10 min) kills a
     stuck run.
   - **API** (optional): set `HAUKSBEE_LLM_API_KEY` (+ `HAUKSBEE_LLM_MODEL`,
     `HAUKSBEE_LLM_BASE_URL`) to use an OpenAI-compatible chat endpoint
     instead.
4. **Parse and validate**: the tool parses the reply as TOML, checks the
   device kind against the requested kind, and range-checks every param
   (`crates/hauksbee-models/src/validation.rs`). A failure feeds the error
   back to the backend for one retry.
5. **Write** `<part>.toml`.

### Failure modes

The extractor fails loudly and usefully:

- **No backend**: a clear error listing codex / `HAUKSBEE_LLM_API_KEY` /
  the offline mock as options.
- **codex timeout**: killed after 10 minutes with a message to tighten the
  prompt or use the API backend.
- **Empty / prose reply**: rejected with "empty reply" or "no [[models]]
  table" rather than a confusing TOML parse error.
- **Wrong kind**: a diode card returned for a `bjt_npn` request is rejected
  ("kind mismatch") so the binder never stamps the wrong device.
- **Out-of-range params**: rejected by the static range check before
  writing.

## Physical validation

Parsing and range checks are necessary but not sufficient: a model can be
syntactically fine and still physically wrong (an `is` off by orders of
magnitude, an LDO that does not regulate). So extracted models are
validated by **simulation** against the datasheet's spec'd operating point,
in `crates/hauksbee-engine/tests/datasheet_validation.rs`:

| kind  | check                                                                 |
|-------|-----------------------------------------------------------------------|
| diode | forward voltage at a stated forward current (1N4148: ~0.7 V at 10 mA) |
| BJT   | DC current gain beta = Ic/Ib **and** Vbe at the bias (BC847: hFE in 110..450, Vbe ~0.66 V at 2 mA) |
| LDO   | output voltage under a real load, within tolerance (AMS1117-3.3: 3.30 V) |

The same suite has a garbage-rejection test proving simulation rejects a
physically absurd model (a "transistor" with beta 5, or a junction that
never turns on) rather than silently binding it.

Measured results from real codex extractions of the three reference
datasheets:

| Part         | Simulated                     | Datasheet truth                  |
|--------------|-------------------------------|----------------------------------|
| BC847 (NPN)  | beta 171, Vbe 0.660 V @ 2 mA  | hFE 110..450 (typ 180), Vbe 660 mV |
| 1N4148 (D)   | Vf 0.688 V @ 10.1 mA          | Vf max 1.0 V @ 10 mA (real ~0.7 V) |
| AMS1117-3.3  | Vout 3.300 V @ 33 mA load     | 3.300 V (3.201..3.399 V)         |

## Power-FET and AFE model coverage

### Generic power-FET fallbacks (catch-all by footprint)

Two generic placeholder entries in `db/mosfet.toml` bind any unmodeled power
FET in a recognised power-FET package (DPAK, D2PAK, TO-252, TO-263, TO-220,
PowerPAK SO-8, PQFN, TDSON, LFPAK, DFN-8) to conservative default ratings:

| Entry ID                  | Kind | Rds(on) model | Max voltage | Max current | Notes |
|---------------------------|------|---------------|-------------|-------------|-------|
| `generic_nmos_power_pkg`  | nmos | 20 mOhm (rd+rs) | 30 V (conservative) | 20 A | PLACEHOLDER |
| `generic_pmos_power_pkg`  | pmos | 20 mOhm (rd+rs) | 30 V (conservative) | 20 A | PLACEHOLDER |

These are **not datasheet-sourced**; hauksbee labels them as placeholders.
Their purpose is to make dissipation and Tj computation possible for
unmodeled power FETs, rather than leaving the thermal monitor blind. A
specific part entry (any entry with a `value_re` or `mpn_re`) always
outscores these catch-alls, because of the specificity scoring in
`matcher.rs` (footprint-only score = 5; value_re adds 30).

### Specific power-FET entries (datasheet-sourced)

| Part         | Kind | Vds (V) | Id (A) | Rds(on) | Package     | Datasheet |
|--------------|------|---------|--------|---------|-------------|-----------|
| IPA045N10N3G | nmos | 100     | 100    | 4.5 mOhm @ 10V | TO-220 | Infineon Rev 2.6 2019-02-15 |
| IRF9358      | pmos | -30     | -9.2/ch | 16.3 mOhm @ -10V | SO-8 DUAL | Infineon Rev 2.1 |
| SIR182DP     | nmos | 100     | 21     | 17 mOhm @ 10V | PowerPAK SO-8 | Vishay Doc 66664 Rev E 2022-07-13 |

IRF9358 is a dual P-channel device; the entry models single-channel
electrical behaviour. The TOML comment documents the DUAL nature and the
full pin map for both channels.

### AFE, gate drivers, and current-sense amplifiers

| Part         | Kind    | Key rating          | Notes |
|--------------|---------|---------------------|-------|
| bq76952      | digital | VPACK 85 V abs max  | TI SLUSDX9C Rev C 2021. Minimal-resolve: internal AFE, protection FET control, coulomb counter, and I2C register map are NOT modeled. Resolves the part so supply-voltage and Tj checking work. |
| LM5107       | digital | VDD 15 V abs max    | TI SNVSA29B Rev B 2015. Minimal-resolve: half-bridge level-shift and dead-time logic not modeled. |
| LM5109       | digital | VDD 15 V abs max    | TI SNVSA31C Rev C 2017. Matches LM5109BMA (mppt-1210 U1) and all LM5109 variants. Minimal-resolve: same as LM5107 but LIN is active-low. |
| INA180       | opamp   | VS 26 V abs max     | TI SBOS774D Rev D 2019. Gain by suffix (A1=20, A2=50, A3=100, A4=200 V/V). |
| INA181       | opamp   | VS 26 V abs max     | TI SBOS744C Rev C 2019. Same gain variants, SOT-23-5 pinout. |
| INA186       | opamp   | VS 26 V abs max     | TI SBOS791A Rev A 2020. Bidirectional current-sense. |
| INA2181      | opamp   | VS 26 V abs max     | TI SBOS831A Rev A 2021. DUAL (two INA181 channels in MSOP-10). |

The bq76952, LM5107, and LM5109 entries are explicitly minimal-resolve: they
give the part an id, kind, and ratings so it stops binding open, but
hauksbee does not model its internal logic. This is documented honestly in
the TOML comments and in the description field. A full behavioral model
would need a `[models.behavioral]` block (see the LTC4020 entry in
`db/power_ics.toml` for the pattern).

## Tests

- **Offline (always run in CI)**:
  - `hauksbee-models` `offline_pipeline_with_mock_reply` drives the whole
    extractor with a canned reply through `HAUKSBEE_EXTRACT_MOCK_REPLY=<file>`,
    no codex, no network.
  - `hauksbee-engine` `fixture_*` physical-validation tests simulate canned
    models and assert the datasheet numbers.
  - `hauksbee-models` `tests/power_fet_afe_resolve.rs` (10 tests): resolves
    each new specific part (IPA045N10N3G, IRF9358, SIR182DP, bq76952,
    LM5107, LM5109, INA181, INA2181) by value and asserts kind + sane
    ratings; it also asserts that the generic power-FET fallback binds an
    unknown FET-in-DPAK by footprint, and that a specific value entry beats
    the catch-all when both match.
- **Live (manual)**: `hauksbee-models` `extract_bc847_live` is `#[ignore]`d
  and runs real codex against `testdata/datasheets/BC847.pdf`. See
  `crates/hauksbee-models/README_DATASHEET.md`.

# Behavioural device models (power ICs)

The SPICE-level kinds (R/C/L, diode, BJT, MOSFET, switches, simple
regulators) cannot capture the *internal logic* of power ICs: a charger's
input-current limit servo, a PMIC's ship-mode pull to a rail, a balancer's
bleed FETs, a sequencer's state machine. For those, a model entry carries a
declarative `[models.behavioral]` block, parsed by
`crates/hauksbee-models/src/behavioral.rs` and realised at run time by
`crates/hauksbee-engine/src/behavioral.rs`.

A behavioural device participates in the solve loop exactly the way the
configurable power supplies do: it stamps controllable Thevenin legs and
sense resistors into the circuit once, and the scheduler calls its `update`
between solver chunks to recompute each leg from the previous chunk's solved
node voltages (iterate-to-consistency per chunk). It never adds device kinds
to the inner Newton loop. Every behaviour is expressed with the existing
`Vsource` / `Isource` / `Resistor` primitives, so the partitioned solver
stays untouched.

## The four declarative facts

A `[models.behavioral]` block is a bag of optional facts:

1. **Pins with semantics**: `[models.behavioral.pins.<role>]`:
   - `pull_to = "<rail role>"` + `pull_ohms = <ohms>`: an internal pull to
     another named pin's rail (the nPM1300 SHPHLD pull to VSYS). The
     runtime stamps a resistor from the pin to the rail node.
   - `open_drain = true` + `od_ohms = <ohms>`: an open-drain output the FSM
     can assert (a charger STAT pin).
   - `enable_threshold_v` / `enable_active_high`: an enable input read
     against a threshold.

2. **Finite state machine**: `[models.behavioral.fsm]`:
   - `states = [...]`, optional `initial`.
   - `[[models.behavioral.fsm.transitions]]` with `from`, `to`, an
     `evalexpr` `guard` over `v_<pin>` / `t` / `t_in_state` / params, and
     optional `min_dwell_s`.
   - `[models.behavioral.fsm.state_pins.<state>.<pin>]` overrides: drive a
     pin (`drive_volts`), assert its open drain (`od_assert`), or tri-state
     it (`hi_z`) while that state is active.

3. **Averaged converter**: `[models.behavioral.converter]`:
   - `topology` (`buck`/`boost`/`buck_boost`), `out_pin`, `in_pin`,
     `vout_setpoint`, `efficiency`, optional `iout_limit_a`.
   - `[models.behavioral.converter.iin_program]`: a programmable
     input-current limit set by a sense resistor and a programming
     resistor, both read off the board by reference designator
     (`rsense_ref`, `prog_ref`). The limit is `v_sense / rsense`, where
     `v_sense` scales linearly with the programming resistor up to
     `v_sense_full`. The runtime regulates the output, folds it back under
     the output limit, and throttles so the reflected input draw never
     exceeds the input limit.

4. **Expression laws**: `[[models.behavioral.laws]]`:
   - A `current` (from pin `a` to `b`) or `voltage` (forced on pin `a`
     behind `r_ohms`) whose value is an `evalexpr` `expr` over `v_<pin>`,
     `t`, the param keys, and `state_<name>` booleans. Optional
     `only_in_state`.

### Why `evalexpr` (and not `rhai`)

The laws and FSM guards are pure arithmetic / boolean expressions over a
bound context (pin voltages, the active state, params). `evalexpr` is a
small, dependency-light evaluator that does exactly that and **nothing
else**: no functions, loops, closures, filesystem, or network. `rhai` is a
full embedded scripting language, more power than these declarative laws
need and a much larger surface to sandbox. We pin `evalexpr` with
`default-features = false` (dropping its optional rand/regex/serde),
compile each expression once at stamp time, and evaluate it against a fresh
per-chunk context, so a law stays sandboxed arithmetic with no side
effects.

## Board-programmable resistors

A power IC's behaviour is often set by an external resistor (the LTC4020
ILIMIT pin, the LTC6803 cell-tie network). The binder reads those resistor
*values* off the actual board:

- A converter's `iin_program` names `rsense_ref` / `prog_ref` (e.g.
  `"R49"`, `"R8"`); the binder substitutes the on-board value.
- Any param `<name>_from_ref = "Rxx"` is rewritten to
  `<name> = ohms(Rxx)`. If the resistor is *absent* (the revision replaced
  it, e.g. the LTC6803 tie R52 replaced by a blocking diode), the binder
  substitutes a large open resistance, so a law dividing by it contributes
  ~0.

This is what lets one model produce different behaviour on two board
revisions with no model edit, the basis of the project's two-sided fault
validations.

## Adding a custom part without recompiling

Models layer `builtin < user TOML < user SPICE` (later wins), and both
datasheet-extracted and hand-written behavioural models share the user-TOML
tier. A custom behavioural part is just a TOML file dropped into a user
directory:

- `~/.hauksbee/models/`, where datasheet extraction writes.
- `~/.config/hauksbee/models/`, your own custom models.
- any `--models-dir <dir>` passed to `hauksbee run` (highest priority).

### Worked example: a "crazy" custom charger

Suppose you have a part `ACME-BUCK-9000`, a buck charger whose input-current
limit is programmed by a resistor `R42` against a 0.005 ohm shunt `R43`,
with a STAT open-drain pin that pulls low while charging. Drop this into
`~/.config/hauksbee/models/acme.toml`:

```toml
[[models]]
id = "acme_buck_9000"
kind = "vreg"
description = "ACME BUCK-9000 buck charger (custom behavioural)"

[models.match]
value_re = "(?i)ACME.?BUCK.?9000"

[models.params]
vout = 8.4
dropout_v = 0.3
iq_a = 0.001

[models.pins]
"1" = "pvin"     # power input
"2" = "bat"      # charge output
"3" = "ilimit"   # input-current-limit program pin (R42)
"4" = "stat"     # open-drain status

[models.behavioral.converter]
topology = "buck"
out_pin = "bat"
in_pin = "pvin"
vout_setpoint = 8.4        # 2S Li-ion
efficiency = 0.90

[models.behavioral.converter.iin_program]
rsense_ref = "R43"         # 0.005 ohm input shunt, read off the board
prog_ref = "R42"           # the ILIMIT resistor, read off the board
vprog_ref = 0.05           # sense threshold at prog = prog_ref_ohms
prog_ref_ohms = 50000.0
v_sense_full = 0.05        # full-scale sense voltage

[models.behavioral.pins.stat]
open_drain = true
od_ohms = 50.0

[models.behavioral.fsm]
states = ["idle", "charging"]

[[models.behavioral.fsm.transitions]]
from = "idle"
to = "charging"
guard = "v_pvin > 4.0"     # input present -> start charging

[models.behavioral.fsm.state_pins.charging.stat]
od_assert = true           # pull STAT low while charging
```

Then:

```bash
hauksbee run my_board.kicad_pcb --models-dir ~/.config/hauksbee/models
```

The part binds with no recompile: the converter regulates `bat` to 8.4 V
with an input-current limit read from `R42`/`R43`, and the STAT pin pulls
low once the FSM enters `charging`.

## The escape hatch: a custom Rust behaviour

Some parts have behaviour no finite declarative schema captures: a
closed-loop controller with internal state, a sequencer with data-dependent
timing, a part whose output depends on an I2C register the firmware wrote.
For those, implement the `CustomBehavior` trait
(`crates/hauksbee-engine/src/behavioral.rs`) in Rust and register it before
binding:

```rust
use hauksbee_engine::{CustomBehavior, CustomRegistry, bind_board_with};
use hauksbee_ir::{Circuit, Device, DeviceId, NodeId, SourceKind};
use hauksbee_models::{ModelLibrary, Params};
use std::collections::BTreeMap;

struct MyController { isrc: Option<DeviceId>, integ: f64 }

impl CustomBehavior for MyController {
    fn stamp(&mut self, circuit: &mut Circuit, reference: &str,
             _params: &Params, role_nodes: &BTreeMap<String, NodeId>) {
        if let Some(&out) = role_nodes.get("out") {
            self.isrc = Some(circuit.add(Device::Isource {
                name: format!("Icustom_{reference}"),
                p: out, n: NodeId::GROUND, kind: SourceKind::Dc(0.0),
            }));
        }
    }
    fn update(&mut self, circuit: &mut Circuit,
              node_v: &dyn Fn(NodeId) -> f64, _t: f64, dt: f64,
              _faults: &mut Vec<hauksbee_engine::FaultEvent>) {
        // Arbitrary stateful Rust: e.g. an integrating controller.
        self.integ += dt * (5.0 - node_v(NodeId(1)));
        if let Some(id) = self.isrc {
            if let Some(Device::Isource { kind, .. }) =
                circuit.devices.get_mut(id.0 as usize) {
                *kind = SourceKind::Dc(self.integ.clamp(0.0, 2.0));
            }
        }
    }
    fn state(&self) -> &str { "controlling" }
}

let mut reg = CustomRegistry::new();
reg.register("ACME-CTRL-X", || Box::new(MyController { isrc: None, integ: 0.0 }));
let bound = bind_board_with(&board, &ModelLibrary::builtin(), &reg);
```

The factory matches against the component's resolved model id, value, or
MPN. On a hit the binder builds the custom device instead of the
declarative one, and the scheduler then drives it each chunk exactly like a
declarative behavioural device. You only ever mutate source values between
chunks, the same convergence-per-chunk pattern the supplies and declarative
devices use, so the inner Newton loop stays untouched.

## Extracting a behavioural model from a datasheet

`model-extract` accepts the behavioural families `charger`, `pmic`,
`balancer` as `--kind`. Each emits the base kind in the TOML (`vreg` /
`digital`) plus a `[models.behavioral]` block, and the prompt is engineered
per family (a charger is asked for its converter and the ILIMIT/RSENSE
programming; a PMIC for its internal pin pulls; a balancer for its leak
law):

```bash
model-extract --pdf testdata/datasheets/LTC4020.pdf --part LTC4020 --kind charger
```

A live codex run against the LTC4020 datasheet produced a model that agreed
with the hand-written one on the load-bearing structure, base kind `vreg`,
the exact pin map, `topology = "buck_boost"`, `out_pin`/`in_pin`,
`vout_setpoint = 28.8` (8S LiFePO4), `efficiency = 0.92`, and a populated
`iin_program` block, and honestly left the ILIMIT transfer-function
constants at zero because the datasheet excerpt did not state the
programming equation (the hand model calibrated those from the documented
60 W/88 W revision evidence, which the datasheet alone does not contain).
The captured output is regression-locked offline in
`crates/hauksbee-models/tests/codex_behavioral_fixture.rs`; the live run is
the `#[ignore]`d `extract_ltc4020_charger_live`.
