# Device models: built-in, SPICE, and datasheet extraction

For the end-to-end human, CLI, MCP, and evidence workflow, including the public
Pedalboard reference journey, see
[`BOARD_MODELING_WORKFLOW.md`](BOARD_MODELING_WORKFLOW.md).

The source-selection, provenance, uncertainty, and fail-closed accuracy policy
is documented in [SOURCE_LADDER.md](SOURCE_LADDER.md). Source tier and storage
layer are deliberately separate; inspect both with `hauksbee models resolve`.

Every component the binder meets needs a simulation model. Physics arrives by
**four** authoring routes. The resolver records both the semantic source tier
and the six storage layers; semantic tier orders sources first, while layer and
specificity make selection deterministic inside one tier.

The four authoring routes:

- **Built-in DB** (`crates/hauksbee-models/db/*.toml`): the curated library
  that ships with hauksbee. It covers the common families (BC847, 1N4148,
  7805, 74HC595, ATmega328P, and so on) plus passives resolved straight from
  the `Value` field.
- **Datasheet extraction**: when a part is not in the DB, point hauksbee at
  the part's PDF datasheet and an LLM backend (codex by default) drafts a
  model entry in the same TOML schema. It runs from the web report, from
  `hauksbee models extract`, or from the standalone `model-extract` binary.
  Every one of the three states what leaves your machine and asks before it
  sends anything. The result is a draft to check, carries provenance
  `datasheet-extracted`, lands in `~/.hauksbee/models/`, and loads as a
  user-dir entry.
- **Hand-written behavioural** TOML: a `[models.behavioral]` entry you
  author by hand for a power IC (see "Behavioural device models" below).
  hauksbee loads it from the same user model directories as the extracted
  models.
- **User SPICE**: a `.model` / `.subckt` card you supply always wins, so you
  can override anything with a vendor-provided SPICE deck.

The storage layers, used after semantic tier (higher breaks a same-tier tie;
specificity only breaks ties *within* a layer):

```
builtin(0)  <  pack(10)  <  user-dir(20)  <  user-config-dir(25)
            <  models-dir(30)  <  spice(40)
```

`SourceLayer` in `crates/hauksbee-models/src/lib.rs` is the authority for that
list, and [PACKS.md](PACKS.md#resolution-priority) is the reference page for it.
Two things about it catch people out. Installed **packs** are a layer of their
own between the built-ins and your own directories. A pack's declared
provenance decides its semantic tier, so a datasheet-extracted pack does not
silently displace the curated library. The two standing user directories are
*distinct* layers: `~/.config/hauksbee/models` (25) beats `~/.hauksbee/models`
(20), so a model you hand-corrected in your config dir deterministically wins
over an auto-extracted one of the same id.

`ModelLibrary::resolve` retains the coarse compatibility `source` string and
also returns the canonical `ModelSource` record. When you need the real tier,
layer, origin, validation and uncertainty, `hauksbee models resolve <board>`
prints them and `--json` exposes the same record without collapsing it.

## Pointing hauksbee at a datasheet

The shortest path is the web report: drop a board, and any part with no model
carries a "draft a model from a datasheet" button. It states what gets sent
before it asks for the file.

Beside it is "write a part yourself", for the two cases extraction does not
serve: you already know the part, or you already have a model. It takes
hauksbee's own TOML, checked as you type by the same validator that runs on
save, so what passes there will save. It also takes SPICE, pasted or loaded
from a `.lib`, `.mod`, `.cir`, `.sp` or `.ckt`. Subcircuits are supported:
hauksbee flattens a `.subckt` at load, maps its ports to your nodes and
recurses through nested calls, so a vendor part that ships as a subcircuit runs
like any other. A SPICE deck is checked there rather than saved as a model,
because a deck is something to simulate; turning one into a reusable part means
writing the entry that claims your component.

From a terminal:

```bash
hauksbee models extract \
    --pdf testdata/datasheets/BC847.pdf \
    --part BC847 \
    --out-dir ~/.hauksbee/models       # default if omitted
```

It prints what will leave your machine and waits for a yes. In a script, where
there is nobody to ask, it refuses rather than assuming: pass `--yes` (or `-y`)
when you mean it.

### Choosing a backend

`--backend` picks which LLM does the reading. All three run the same prompt,
the same validation, and the same retry-with-feedback loop:

| backend | requirement | flags |
|---------|-------------|-------|
| `codex` (default) | `codex` CLI in PATH, signed in | `--backend codex`, `--model` |
| `claude-code` | `claude` CLI in PATH, signed in | `--backend claude-code`, `--model` |
| `api` | key in the env var named by `--api-key-env` (default `OPENAI_API_KEY`; a set `HAUKSBEE_LLM_API_KEY` is honoured) | `--backend api`, `--api-base`, `--model`, `--api-key-env` |

With no `--backend`, a set `HAUKSBEE_LLM_API_KEY` selects the api backend,
matching the behaviour from before the flag existed. `--api-key-env` takes the
NAME of an environment variable, never the key itself: the key is read from
the environment at call time and is never stored or logged. A missing CLI or
an unset key variable errors up front with the exact fix (install the tool, or
`export OPENAI_API_KEY=...`), before anything is sent.

The older standalone binary still works and holds the same contract
(`cargo build -p hauksbee-models --bin model-extract`, then
`HAUKSBEE_EXTRACT_YES=1` for the scripted case).

`--kind` is optional, and leaving it off is usually right: the datasheet says
what the part is on its first page, and the model is about to read that page.
It identifies the kind, prints which one it chose before committing to it, and
extracts against that kind's schema, so a wrong identification fails validation
rather than producing a plausible model of the wrong device. A part that fits
none of the supported kinds is reported as exactly that, rather than forced
into the nearest one.

Pass it when you know better than the model, which is a real case: a part that
reads like a regulator and behaves like a charger is where a human override
earns its place. `--kind` is one of `passive | diode | bjt_npn | bjt_pnp |
nmos | pmos | vreg | opamp | comparator | analog_switch | digital | dac | adc |
shift_register | mcu | connector | i2c_sensor | spi_sensor`, plus the
behavioural families `charger | pmic | balancer`.

The tool writes `<part>.toml` to the output directory. The library loads any
TOML in `~/.hauksbee/models/` as a user-dir entry the next time it builds,
so an extracted part becomes immediately resolvable by value/MPN.

## Start from an unresolved board part (without invented physics)

When a run names an unresolved reference, start with the board itself. The
scaffold copies only the component identity and its exact Value field into a
literal match rule. It does **not** infer a device kind from `R`, `D`, `Q`, or
`U`, and it writes no guessed electrical number:

```bash
hauksbee models new U3 --board path/to/board.kicad_pcb --out U3-model.toml
```

The result is valid TOML with a deliberate `kind = "choose_kind"` placeholder,
an explicit `user-model` / `unvalidated` source record, an `unknown` model interval,
and TODOs for the evidence that is still missing. That
placeholder is not a model: `hauksbee models lint U3-model.toml` must refuse it
until you choose a supported kind and fill the required fields. After editing,
run the same lint again, then check the board binding:

```bash
hauksbee models lint U3-model.toml
hauksbee models resolve path/to/board.kicad_pcb --models-dir .
```

For a shareable pack, use `--pack-dir` instead. It writes `pack.toml` and
`models/<id>.toml`, refuses to overwrite either, and leaves the license as an
explicit TODO. `hauksbee models add` remains the final pack validation step;
it must reject the untouched scaffold rather than install an unresolved model:

```bash
hauksbee models new U3 --board path/to/board.kicad_pcb --pack-dir ./acme-models
hauksbee models lint acme-models/models/u3-*.toml
hauksbee models add ./acme-models       # only after TODOs and license are complete
```

The proving tests cover both paths: the generated file parses as TOML, the
unresolved placeholder exits non-zero under the shared linter, explicit
`--kind` still requires the kind's parameters, pack output has the expected
layout, and a second invocation cannot overwrite an existing model or manifest.
The board and the part documentation remain the authority; comments in a
scaffold are prompts, not evidence.

### Inspect behavior, and gate the capability you actually need

Extraction and behavior are separate facts. `coverage` reports every connected
active U/IC/MCU device, its winning model/source, the board-observed pad/function/net
map, and four deliberately different states: unresolved, identity-only,
executable with unspecified scope, and executable with declared implemented and
missing capabilities. Its separate authoring queue also includes connected
load-bearing discretes and module boundaries such as Q/F/L/CM references:

```bash
hauksbee models coverage path/to/board.kicad_pcb
hauksbee models coverage path/to/board.kicad_pcb --json > coverage.json
```

“Full behavior” is meaningful only relative to a question. A source-bound DC
converter model may be complete for feedback-divider regulation while explicitly
missing switching ripple; an RP2040 backend may be complete for GPIO input while
explicitly missing an external SPI-slave path. Gate that exact scope with one or
more `REF:CAPABILITY` requirements:

```bash
hauksbee models coverage board.kicad_pcb \
  --require U6:board_feedback_divider \
  --require U3:gpio_external_input
```

The command exits non-zero unless every winning card explicitly lists the named
capability under `[models.coverage].implements`. An identity-only card, an
unspecified legacy scope, a capability listed under `.missing`, a typo, or an
unknown reference all fail closed; none is guessed complete. JSON retains the
per-requirement model id, stage, result, and reason.

Executable cards can combine the ordinary solver kinds with `[models.logic]`,
`[models.behavioral]` pins/FSM/converter/expression laws/state-controlled series
paths/model-owned current profiles, firmware-visible `[models.peripheral]`
EEPROM/flash/register-map behavior, and board-resistor-driven `[models.current_program]`
laws. A peripheral and an analogue behavioural block may coexist on the same
resolved part: protocol behavior does not prevent that part from loading a rail
or driving a protection state machine. These are reusable model behavior, not
CI-only special cases. CI scenarios remain the right place for product/workload
stimuli that the fitted part and datasheet do not determine (firmware image,
traffic, ambient, module variant, or a particular user load).

A reusable declarative bus device embeds the already-validated sensor TOML in
the card. Role names must exist in `[models.pins]`; malformed maps and unwired
roles fail lint/binding rather than becoming a zero-valued peripheral:

```toml
[models.peripheral]
kind = "register_map"
scl_role = "scl"          # defaults shown; SPI uses cs/sck/mosi/miso
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

[models.coverage]
implements = ["i2c_chip_id"]
missing = ["measurement_registers", "interrupt_behavior"]
```

This exact map auto-attaches when the part resolves. It is intentionally
partial: declaring one WHO_AM_I register does not claim an accelerometer,
timing model, interrupt engine, or complete silicon.

A model-owned load uses the same waveform evaluator as scenario loads. Its
first segment is the documented pre-start/DC current; later segments can ramp,
hold, or repeat with deterministic jitter:

```toml
[[models.behavioral.profiled_loads]]
name = "core"
supply_pin = "vdd"
return_pin = "gnd"       # omit only when circuit ground is genuinely correct
start_s = 0.0
seed = 0

[[models.behavioral.profiled_loads.segment]]
level_a = 0.000010        # source-bound standby current
duration_s = 0.005

[[models.behavioral.profiled_loads.segment]]
level_a = 0.240           # source-bound active/burst current
rise_s = 0.0005
duration_s = 0.010
period_s = 0.100
idle_a = 0.040
```

The runtime stamps the sink on the resolved supply/return pins and exposes its
current to FSM guards as `i_load_core`. Validation rejects missing pin roles,
negative/non-finite current or time, impossible burst timing, duplicate names,
and jitter large enough to erase a period. A datasheet maximum can therefore
drive a conservative rail/protection test, while a firmware-dependent workload
remains an explicit scenario input instead of being disguised as universal
part behavior.

Exact source citations belong in the card, not only in prose:

```toml
[[models.source.references]]
url = "https://vendor.example/part-datasheet.pdf"
title = "Part datasheet"
locator = "Electrical characteristics, table 6"
# sha256 = "..."         # include only when those exact bytes were retained
```

`models coverage --json` and `--require REF:CAPABILITY` retain those references.
Malformed metadata, non-HTTPS URLs, or malformed hashes reject the model file;
they are not silently discarded while the electrical model keeps running.

### Prepare a pack for every actionable model gap

When a board has several unresolved, identity-only, or partial executable
models, `prepare` turns them into one reviewed plan:

```bash
hauksbee models prepare path/to/board.kicad_pcb --pack-dir ./acme-models
```

The command resolves against the local model library, lists the broader
authoring queue (including connected FETs, fuses, inductors, and modules),
references and the exact `pack.toml`, `inventory.json`, `workplan.json`, and
`models/<id>.toml` paths it would write, then asks `Write exactly these files?
[y/N]`. A default answer, a non-terminal
stdin, or an existing target file writes nothing. Use `--yes` only when a
script has deliberately reviewed that plan; it is the explicit non-interactive
opt-in. Preparation is local and deterministic: it performs no network or LLM
request, does not install/register the pack, and never invents device facts.
An unresolved or identity-only target remains `choose_kind`, `user-model` /
`unvalidated`, and `unknown` until its author supplies evidence. A partial
executable target is different: preparation copies the exact winning card,
narrows its match to the board-observed identity, preserves every behavior that
already runs, and demotes the copy to `user-model` / `unvalidated` while it is
edited. This prevents “add one missing behavior” from accidentally throwing
away the working model. `workplan.json` maps each component to that prepared
file, its before-stage, implemented and missing capabilities, and the exact
lint/coverage/run commands for the handoff. The board pin/net inventory stays
in `inventory.json`; it is evidence, not an invented pin map.

## From the browser

`hauksbee serve` makes model coverage part of the board itself. Click a coverage
row, component, or trace to see the winning model, its validation stage,
implemented and missing behavior, source reference, exact nets, and affected
devices. A trace can become a live scope probe or a repeatable assertion in the
same card. A component with a gap carries **Extend**: that opens a local draft;
for a partial executable model the server copies the exact winning behavior
before the user edits it. Drafting is read-only and deterministic. **Save** is
the separate explicit write approval. None of this requires an LLM.

The same board interaction builds the experiment. A selected trace can become
a live scope probe, assertion, 50-ohm source, button, switch, or ideal scenario
supply. A selected I²C/SPI component can open a register-map row: choose/paste
local declarative TOML or select an exact checked-in behavior from the bundled
picker, override physical inputs, and optionally provide an SPI
controller and chip-select net. The browser can attach those exact validated
bytes immediately to the running bus and keeps the identical `[[sensor]]` entry
for replay. The row waits for a correlated engine receipt and shows acceptance
or refusal beside the exact bytes; “sent” is not treated as success. Model
cards may instead embed a validated
`[models.peripheral] kind = "register_map"` block; exact resolution then makes
the bus behavior automatic. Reusable cards can require strap roles high/low and
derive an I²C address from a resolved supply/ground strap. Floating or ambiguous
straps leave the behavior open with a named reason. Neither path guesses
registers from a part name. The bundled picker is local and read-only, so this
ordinary path needs neither a separate file nor an LLM.

Where a datasheet can accelerate authoring, the page also offers the optional
extraction workflow. It holds the same consent contract as the CLI. The flow is
fixed in this order, and the order is the contract:

1. **Can it run at all**, from `GET /api/models/extract/ready`. If codex is
   missing, or installed but not signed in, the blocker and the one command
   that fixes it are shown here and the file picker is never reached. Learning
   that codex is unauthenticated *after* choosing a datasheet would be a
   consent question asked for nothing.
2. **The consent notice**, served verbatim from
   `hauksbee_models::datasheet::CONSENT_NOTICE` so the page cannot soften the
   CLI's wording, plus the cost line: codex signs in with a ChatGPT account,
   so for anyone already paying for one this costs nothing extra. An explicit
   click is required.
3. **The datasheet**, the part number (prefilled from the board's value field)
   and the kind (a picker generated from the engine's own kind list, so it
   cannot offer a kind the extractor rejects).
4. **Progress**, streamed from `POST /api/models/extract` as Server-Sent
   events, the same framing the dependency installs use. codex is silent for
   one to three minutes, so the stream heartbeats elapsed time rather than
   inventing a percentage.
5. **Review**. The draft comes back as a model card: every value with the
   datasheet citation beside it, every value the model admitted it assumed
   flagged separately, the provenance shown as `datasheet-extracted`, and the
   whole TOML editable. **Nothing has been written at this point.**
6. **Accept or reject**. Accept calls `POST /api/models/save`, which
   re-validates the TOML it is handed (the card is editable, so it is not
   necessarily what the extractor produced), refuses to overwrite an existing
   model, and writes one file into `~/.hauksbee/models/`. Reject writes
   nothing.

The extraction itself is the same `hauksbee_models::datasheet` code the CLI
runs, with the same scratch sandbox: the agent gets a copy of the PDF and its
page renders, never the directory the user's file came from. The web plumbing
is `crates/hauksbee-engine/src/webextract.rs` (the engine hooks) and
`crates/hauksbee-server/src/frontdoor.rs` (`datasheet_routes`).

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

`crates/hauksbee-models/src/datasheet.rs`:

0. **A sandbox** is built first: a scratch directory holding a copy of the
   datasheet and nothing of yours. The agent runs there, so what it can write
   is bounded even though it runs unattended. Read the module doc for what
   that does and does not buy: the profile confines writes and kills network,
   it does not confine reads.
1. **PDF to text** through `pdftotext` (if present), and **one image per page**
   through `pdftoppm` at 150 DPI, capped at fourteen pages. The renders matter
   more than the text: an absolute-maximum table survives a render and becomes
   a column of loose numbers in a text dump.
2. **Prompt** built per kind, listing the required params, the ratings to
   pull, and the physical bounds each value must respect.
3. **Backend call** (see the backend matrix above):
   - **codex** (default): `codex exec --sandbox workspace-write
     --skip-git-repo-check --cd <pdf_dir>`. stdin is closed so codex does
     not block; the final agent message (clean TOML) comes back on stdout
     while session logging goes to stderr. A hard timeout (10 min) kills a
     stuck run.
   - **claude-code** (`--backend claude-code`): headless `claude -p` in the
     same sandbox, with the same prompt file, answer file (`model.toml`),
     timeout, and retry contract.
   - **api** (`--backend api`): an OpenAI-compatible chat endpoint at
     `--api-base` (default `https://api.openai.com/v1`, or
     `HAUKSBEE_LLM_BASE_URL`), model from `--model` or `HAUKSBEE_LLM_MODEL`,
     key read at call time from the env var named by `--api-key-env`.
4. **Parse and validate**: the tool parses the reply as TOML, checks the
   device kind against the requested kind, and range-checks every param
   (`crates/hauksbee-models/src/validation.rs`). A failure feeds the error
   back to the backend for one retry.
5. **Write** `<part>.toml`.

### Failure modes

The extractor fails loudly and usefully:

- **Missing tool or key**: a selected CLI backend whose tool is not in PATH,
  or an api backend whose key variable is unset, errors with the exact fix
  (the install command, or `export OPENAI_API_KEY=...`) before anything is
  sent.
- **CLI timeout**: a codex/claude run is killed after 10 minutes with a
  message to tighten the prompt or switch backend.
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
  - `hauksbee-models` `tests/suite/power_fet_afe_resolve.rs` (10 tests): resolves
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
     (`rsense_refs`, `prog_ref`). `rsense_refs` lists every matched shunt in a
     Kelvin topology. Every named resistor must resolve, and all listed shunts
     must agree, or the limit is unknown; named parts never fall back to a
     literal. The limit is `v_sense / rsense`, where
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

- A converter's `iin_program` names `rsense_refs` / `prog_ref` (e.g.
  `["R49", "R50"]`, `"R8"`); the binder substitutes the on-board values.
  Missing, DNP, duplicate, identity-ambiguous, unparsable, or unequal named
  resistors make the dynamic limit unknown. Literal `rsense_ohms` and
  `prog_ohms` remain available only for literal-only models; each is mutually
  exclusive with its named form.
- Any param `<name>_from_ref = "Rxx"` is rewritten to
  `<name> = ohms(Rxx)`. If the resistor is *absent* (the revision replaced
  it, e.g. the LTC6803 tie R52 replaced by a blocking diode), the binder
  substitutes a large open resistance, so a law dividing by it contributes
  ~0.

This is what lets one model produce different behaviour on two board
revisions with no model edit, the basis of the project's two-sided fault
validations.

Both mechanisms above name a resistor by reference designator, which ties the
model to one board's schematic. A third, `[models.current_program]`, is derived
from topology instead and so works on any board:

```toml
[models.ratings]
max_current_a = 1.2        # absolute maximum; stress monitoring only

# I_REG = 1000 V / R_PROG (datasheet section 5.2)
[models.current_program]
pin = "prog"               # a role in [models.pins]
semantics = "regulated_current"
current_in_roles = ["in"]  # exact main-power roles; no name inference
current_out_roles = ["out"]
max_operating_current_a = 1.0
equation = "inverse_resistance"
k_volts = 1000.0
```

Declare it for any part whose regulated current or protection threshold the
*board* chooses with a resistor network. `semantics` is mandatory and keeps two
different physical statements separate:

- `regulated_current` is an operating state that can flow continuously (for
  example, a charger's constant-current phase). It may support a steady-state
  ampacity attribution.
- `protection_limit` is an OCP/trip/current-limit threshold (for example,
  AP22615 ISET or LTC4020 ILIMIT). The type records and validates that physical
  meaning, but the generic `current_program` consumer does not simulate
  protection thresholds or turn them into loads. A model-specific
  `[models.behavioral]` block must separately declare any dynamic protection
  behaviour it actually implements.

Every `regulated_current` entry also declares non-empty `current_in_roles` and
`current_out_roles`. Each item must name a role in `[models.pins]`; duplicates
and overlap between the two directions fail validation. These are the exact
main-power terminals to which the computed current may be attributed. Control,
ground, enable, programming, and Kelvin-sense pins stay out unless the model
author explicitly (and correctly) declares them. Protection thresholds need no
power-role lists because they never become steady-state loads.

For a regulated current, checks compute the populated DC-equivalent resistance
from the programming pin to ground. The bounded nodal-conductance solve includes
all simultaneous series/parallel/bridge branches; a closed solder link is a
short and a capacitor/open jumper is open. A numeric fuse, thermistor,
conflicting identity, unknown two-terminal part, or network beyond the supported
topology makes the result undetermined rather than assumed. The block's
`max_operating_current_a` marks the sourced equation domain. Above it, the
default `above_domain = "abstain"` returns no current; it does not silently clamp
an undersized programming resistor to a precise ceiling. A model may declare
`above_domain = "saturate"` only when the source actually says the transfer
saturates at `max_operating_current_a`. The limit is intentionally not inferred from
`ratings.max_current_a`, which is a separate device-level analysis threshold and
may be higher. It is normally an absolute limit; model entries that deliberately
use a lower recommended-operating ceiling say so beside the value.

The other supported shape is a continuous two-branch inverse-resistance law:

```toml
[models.current_program]
pin = "prog"
semantics = "regulated_current"
current_in_roles = ["in"]
current_out_roles = ["out"]
max_operating_current_a = 0.401
equation = "piecewise_inverse_resistance"
low_k_volts = 1000.0
transition_current_a = 0.15
high_numerator_a = 1.2
resistance_scale_ohms = 1000.0
high_offset = 1.3333333333333333
```

This evaluates `low_k_volts / R` through the transition, then
`high_numerator_a / (R / resistance_scale_ohms + high_offset)`. Validation
rejects non-physical constants, discontinuous branches, a missing pin role, or
an operating limit above the declared device current threshold. The TP4054 Rev
2.1 is
the checked-in example: Watchy's fitted 10 kOhm resistor programs 100 mA, while
5.1 kOhm correctly uses the high-current branch and yields about 186.5 mA.

A sense-programmed controller uses the third equation shape:

```toml
[models.current_program]
pin = "ilimit"
semantics = "protection_limit"
equation = "sense_scaled_resistance"
sense_roles = ["senstop", "sensbot"]
sense_far_roles = ["sensvin", "sensgnd"]
program_bias_a = 0.00005
program_full_scale_v = 1.0
sense_full_scale_v = 0.05
```

For a `regulated_current` law, the engine finds the programming network from
`pin` to ground and exactly one populated shunt beside every named sense role,
connected to its paired far-side role. All declared shunts must have equal
nominal resistance; a mismatch, extra/multi-terminal branch, or wrong far-side
net is undetermined. It then evaluates
`min(program_bias_a * Rprogram, program_full_scale_v) /
program_full_scale_v * sense_full_scale_v / Rsense`. Missing roles, duplicate
roles, non-physical constants, an unreadable programming path, or an unreadable
shunt leave the value explicitly undetermined. The LTC4020 is the checked-in
`protection_limit` schema example: its *separate* behavioural converter derives
1.7875 A from 7.15 kOhm/10 mOhm and 5 A at nominal full scale. The generic
`current_program` consumer deliberately does not evaluate that protection row,
and static ampacity does not charge the threshold to power rails or Kelvin
stubs.

These equations are datasheet point relationships. They do not silently turn a
typical-only table row into a guaranteed upper bound: unless a model carries a
separately sourced limit or interval, the result is a nominal programmed-current
estimate. For example, AP22615 publishes no maximum for its 6.8 kOhm row, so its
1 A equation result is a nominal OCP threshold and never a 1 A load assertion.

For these parts, the device rating is never read as a load. The Olimex
ESP32-EVB programs its MCP73833 at 200 mA, and treating the 1 A capability as the
rail current over-reported it fivefold. A part with a recognizable programming
pin and no equation is likewise excluded from rating-based attribution: adding
the block gains coverage, while omitting it costs only coverage, never
correctness.

## Adding a custom part without recompiling

A custom behavioural part is just a TOML file dropped into a user directory.
Which directory sets its priority, and how it was authored does not:

- `~/.hauksbee/models/` (layer 20), where datasheet extraction writes.
- `~/.config/hauksbee/models/` (layer 25), your own custom models. Beats the
  extraction directory, so a hand-corrected model of the same id wins.
- any `--models-dir <dir>` passed to `hauksbee run` (layer 30, highest of the
  three).

A user SPICE card (layer 40) still beats all of them.

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
rsense_refs = ["R43"]      # every matched input shunt, read off the board
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
programming equation. The checked-in hand model now uses the primary datasheet
transfer (`VILIMIT = 50 uA * RILIMIT`, effective over 0--1 V, scaling the 50 mV
input-sense threshold) rather than fitting constants to the reported fault
wattage.
The captured output is regression-locked offline in
`crates/hauksbee-models/tests/suite/codex_behavioral_fixture.rs`; the live run is
the `#[ignore]`d `extract_ltc4020_charger_live`.
