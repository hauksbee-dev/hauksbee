# Device models

Every component the binder meets needs a model. Models arrive by four routes:

- **Built-in DB** (`crates/hauksbee-models/db/*.toml`): the curated library
  (BC847, 1N4148, 7805, 74HC595, ATmega328P, ...) plus passives resolved from
  the `Value` field.
- **Datasheet extraction**: an LLM drafts a model from a PDF
  (`hauksbee models extract`, the web report, or the standalone
  `model-extract` binary), always after asking. The draft carries provenance
  `datasheet-extracted` and lands in `~/.hauksbee/models/`.
- **Hand-written TOML**, including `[models.behavioral]` blocks for power ICs.
- **User SPICE**: a `.model`/`.subckt` card always wins.

## Resolution

Semantic source tier is ordered first; within a tier the storage layer breaks
ties, and match specificity breaks ties within a layer:

```
builtin(0) < pack(10) < user-dir(20) < user-config-dir(25) < models-dir(30) < spice(40)
```

`~/.hauksbee/models` (20) is where extraction writes; `~/.config/hauksbee/models`
(25) beats it, so a hand-corrected model of the same id wins; `--models-dir DIR`
(30) beats both. `hauksbee run`, `hauksbee-ci run` and `hauksbee-ci check`
all take `--models-dir`.

```
hauksbee models resolve <board> [--models-dir DIR] [--all] [--json]
    [--min-model-tier TIER] [--min-model-validation LEVEL] [--require-model-intervals]
```

`resolve` prints, per component, the winning entry, its layer and origin;
`--json` emits `{"components":[{"ref","value","model","layer","origin"}]}`
(model `UNRESOLVED` when nothing matched); `--all` shows bulk passive rows.
The three refusal flags exit 3 when any component resolves below a source
tier (`datasheet-derived`, `curated-library`, `vendor-spice`), below a
validation level (`physical-bounds-only`, `datasheet-curves`,
`vendor-qualified`), or without finite uncertainty intervals.

## Model entry schema

```toml
[[models]]
id = "acme_buck_9000"
kind = "vreg"                    # see kinds below
description = "..."

[models.match]                   # at least one rule; every regex must compile
value_re = "(?i)ACME.?BUCK.?9000"
mpn_re = "..."                   # optional
# footprint-only matches score 5; value_re adds 30, so a specific entry beats a catch-all

[models.params]                  # per-kind SPICE-level parameters
vout = 8.4
dropout_v = 0.3
iq_a = 0.001

[models.pins]                    # pin number -> role
"1" = "pvin"
"2" = "bat"

[models.ratings]                 # absolute maxima for the stress monitor; omitted = no limit known
max_current_a = 1.2
max_surge_current_a = 3.0
max_power_w = 0.5
max_voltage_v = 30
max_junction_temp_c = 125
max_ripple_current_a = 3.0      # capacitors, for the input-cap ripple check
theta_ja_c_per_w = 60            # thermal; theta_jc_c_per_w is informational

[models.coverage]
implements = ["board_feedback_divider"]
missing = ["switching_ripple"]

[[models.source.references]]
url = "https://vendor.example/part-datasheet.pdf"   # HTTPS only
title = "Part datasheet"
locator = "Electrical characteristics, table 6"
# sha256 = "..."   # only when those exact bytes were retained
```

Kinds: `passive`, `diode`, `bjt_npn`, `bjt_pnp`, `nmos`, `pmos`, `vreg`,
`opamp`, `comparator`, `analog_switch`, `digital`, `dac`, `adc`,
`shift_register`, `mcu`, `connector`, `i2c_sensor`, `spi_sensor`; the
extractor also accepts the behavioural families `charger`, `pmic`, `balancer`
(emitting `vreg`/`digital` plus a `[models.behavioral]` block).

Validate with `hauksbee models lint <file>` (exit 2 on any finding). Loading
a db file only checks TOML, a non-empty `[match]` and regex compilation;
lint runs the per-kind checks. Packs are fully validated by `models add`.

## Model packs

A pack is a directory with `pack.toml` and `models/*.toml`.

```
hauksbee models add <path|url>     # directory, .tar.gz/.tgz/.tar, or git URL -> ~/.hauksbee/packs
hauksbee models list [--builtin]   # --builtin also lists embedded MCU SoC descriptors
hauksbee models remove <name>
```

A pack's declared provenance decides its semantic tier, so a
datasheet-extracted pack does not displace the curated library.

## Scaffolding and coverage

```
hauksbee models new U3 --board board.kicad_pcb [--kind vreg] [--out U3.toml | --pack-dir acme-models]
hauksbee models coverage board.kicad_pcb [--models-dir DIR] [--json] [--require REF:CAPABILITY]...
hauksbee models prepare board.kicad_pcb --pack-dir DIR [--models-dir DIR] [--yes]
```

`new` copies the component's identity and exact value into a match rule and
writes `kind = "choose_kind"` with `user-model` / `unvalidated` provenance;
lint refuses it until a kind and its parameters are supplied. It never
overwrites. `--pack-dir` also writes `pack.toml` with the license as a TODO.

`coverage` reports every connected active device's stage (unresolved,
identity-only, executable with unspecified scope, executable with declared
implemented/missing capabilities), its winning model, the board-observed
pad/function/net map, and an authoring queue including load-bearing
discretes. `--require REF:CAPABILITY` exits non-zero unless the winning card
lists the capability under `[models.coverage].implements`; identity-only
cards, unspecified scope, `.missing` entries, typos and unknown references
all fail closed.

`prepare` writes a reviewed pack skeleton for every gap after printing the
plan and asking. See [BOARD_MODELING_WORKFLOW.md](BOARD_MODELING_WORKFLOW.md).

## Datasheet extraction

```
hauksbee models extract --pdf datasheet.pdf --part BC847 [--kind bjt_npn] [--out-dir ~/.hauksbee/models] [--yes]
    [--backend codex|claude-code|api] [--model NAME] [--api-base URL] [--api-key-env NAME]
cargo run -p hauksbee-models --bin model-extract -- --pdf <p> --part <PART>   # standalone; HAUKSBEE_EXTRACT_YES=1 for scripts
```

`--pdf` and `--part` are required. The command prints what leaves the machine
and waits for a yes; without a terminal it refuses unless `--yes`. `--kind`
is optional (the extractor identifies the kind from the datasheet and fails
validation on a mismatch).

| backend | requirement | flags |
|---|---|---|
| `codex` (default) | `codex` CLI in PATH, signed in | `--model` (default `gpt-5.6-sol`) |
| `claude-code` | `claude` CLI in PATH | `--model` |
| `api` | key in the env var named by `--api-key-env` (default `OPENAI_API_KEY`) | `--api-base` (default `HAUKSBEE_LLM_BASE_URL`, then `https://api.openai.com/v1`), `--model` (or `HAUKSBEE_LLM_MODEL`) |

With no `--backend`, a set `HAUKSBEE_LLM_API_KEY` selects `api`. The pipeline
(`crates/hauksbee-models/src/datasheet.rs`): copy the PDF into a scratch
sandbox; `pdftotext` and `pdftoppm` at 150 DPI (up to fourteen pages); a
per-kind prompt; the backend call (10-minute timeout); parse, kind check and
range check (`validation.rs`) with one retry on failure; write `<part>.toml`.
Every numeric line carries a citation comment; the ratings go to
`[models.ratings]`. Failures are explicit: missing tool or key, timeout, empty
reply, kind mismatch, out-of-range parameter.

From the browser: `GET /api/models/extract/ready` (is codex installed and
signed in), the consent notice (`hauksbee_models::datasheet::CONSENT_NOTICE`),
`POST /api/models/extract` (progress as Server-Sent events), review, then
`POST /api/models/save` (revalidates, refuses overwrite). The page also takes
hand-written TOML (validated as you type) and SPICE (`.lib`, `.mod`, `.cir`,
`.sp`, `.ckt`; `.subckt` is flattened at load).

Extracted models are validated by simulation against datasheet operating
points (`crates/hauksbee-engine/tests/datasheet_validation.rs`: diode Vf, BJT
beta and Vbe, LDO output under load).

## Behavioural device models (power ICs)

A `[models.behavioral]` block describes internal logic the SPICE kinds cannot:
it stamps controllable Thevenin legs and sense resistors once, and the
scheduler calls `update` between chunks to recompute them from the previous
chunk's solved voltages. Four optional facts:

1. **Pins**: `[models.behavioral.pins.<role>]` with `pull_to = "<rail role>"`
   + `pull_ohms`, or `open_drain = true` + `od_ohms`, or
   `enable_threshold_v` / `enable_active_high`.
2. **FSM**: `[models.behavioral.fsm]` with `states`, optional `initial`;
   `[[models.behavioral.fsm.transitions]]` with `from`, `to`, an `evalexpr`
   `guard` over `v_<pin>` / `t` / `t_in_state` / params, optional
   `min_dwell_s`; `[models.behavioral.fsm.state_pins.<state>.<pin>]` with
   `drive_volts`, `od_assert` or `hi_z`.
3. **Averaged converter**: `[models.behavioral.converter]` with `topology`
   (`buck`/`boost`/`buck_boost`), `out_pin`, `in_pin`, `vout_setpoint`,
   `efficiency`, optional `iout_limit_a`;
   `[models.behavioral.converter.iin_program]` with `rsense_refs`,
   `prog_ref`, `vprog_ref`, `prog_ref_ohms`, `v_sense_full` (resistors read
   off the board by reference; literal `rsense_ohms`/`prog_ohms` for
   literal-only models).
4. **Laws**: `[[models.behavioral.laws]]`, a `current` (pin `a` to `b`) or
   `voltage` (on `a` behind `r_ohms`) whose `expr` is `evalexpr` over
   `v_<pin>`, `t`, params and `state_<name>`; optional `only_in_state`.

Any param `<name>_from_ref = "Rxx"` becomes `ohms(Rxx)` at bind time (an
absent resistor substitutes a large open resistance). Expressions are
`evalexpr` with no functions, loops or I/O, compiled once at stamp time.

Model-owned loads: `[[models.behavioral.profiled_loads]]` with `name`,
`supply_pin`, `return_pin`, `start_s`, `seed`, and `[[...segment]]` entries
(`level_a`, `rise_s`, `duration_s`, `period_s`, `idle_a`, `jitter_s`, the
same evaluator as scenario profiles); the current is exposed to FSM guards as
`i_load_<name>`.

Custom Rust behaviour: implement `CustomBehavior` (`stamp`, `update`, `state`)
from `crates/hauksbee-bind/src/behavioral.rs`, register it in a
`CustomRegistry` by model id, value or MPN, and bind with `bind_board_with`.

### Board-programmed currents

`[models.current_program]` derives a regulated current or protection
threshold from the board's programming resistor network, on any board:

```toml
[models.current_program]
pin = "prog"                        # a role in [models.pins]
semantics = "regulated_current"     # or "protection_limit"
current_in_roles = ["in"]           # required for regulated_current
current_out_roles = ["out"]
max_operating_current_a = 1.0       # sourced equation domain; above_domain = "abstain" (default) | "saturate"
equation = "inverse_resistance"     # I = k_volts / R
k_volts = 1000.0
```

Other equations: `piecewise_inverse_resistance` (`low_k_volts`,
`transition_current_a`, `high_numerator_a`, `resistance_scale_ohms`,
`high_offset`) and `sense_scaled_resistance` (`sense_roles`,
`sense_far_roles`, `program_bias_a`, `program_full_scale_v`,
`sense_full_scale_v`). The populated DC-equivalent resistance from `pin` to
ground is solved over series/parallel/bridge branches; a fuse, thermistor or
unclassifiable part makes the result undetermined. `regulated_current`
results seed the ampacity and ripple checks; `protection_limit` never does,
and `ratings.max_current_a` is never read as a load.

## Register-map peripherals

A card can embed a validated sensor spec so the bus device auto-attaches:

```toml
[models.peripheral]
kind = "register_map"
scl_role = "scl"          # defaults; SPI uses cs/sck/mosi/miso
sda_role = "sda"
spec_toml = '''
[sensor]
name = "Example chip-id subset"
bus = "i2c"
i2c_address = 0x18
[[sensor.register]]
addr = 0x00
const = [0x13]
[sensor.protocol]
style = "i2c_pointer"
'''
```

Roles must exist in `[models.pins]`. Cards may declare required-high/low
strap roles and derive an I2C address from a resolved supply/ground strap;
floating straps leave the behaviour open. Standalone `[sensor]` specs
(`testdata/sensor-specs/`, `examples/models/lm75.toml`) attach from a spec's
`[[sensor]]` block ([CI.md](../ci/CI.md#peripherals-and-sensors)).

## Library notes

- Generic power-FET fallbacks (`generic_nmos_power_pkg`,
  `generic_pmos_power_pkg` in `db/mosfet.toml`) bind any unmodelled FET in a
  power package (DPAK, D2PAK, TO-220, PowerPAK, PQFN, LFPAK, DFN-8) with
  placeholder ratings so dissipation can be computed; any specific entry
  outscores them, and they never seed a current.
- Minimal-resolve entries (bq76952, LM5107, LM5109) give a part an id, kind
  and ratings without modelling its logic.
- Load profiles live in `db/load_profiles.toml` ([TRANSIENTS.md](../checks/TRANSIENTS.md)).
- MCU routing entries live in `db/mcu.toml` ([add-an-mcu-variant.md](../extending/add-an-mcu-variant.md)).
