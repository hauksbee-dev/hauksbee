# Device models: built-in, SPICE, and datasheet extraction

Every component the binder meets needs a simulation model. Galvani resolves one
from three sources, layered by priority (later wins):

```
builtin TOML DB   <   datasheet extraction   <   user SPICE
   (lowest)                                          (highest)
```

- **Built-in DB** (`crates/galvani-models/db/*.toml`): the curated library that
  ships with galvani. Covers the common families (BC847, 1N4148, 7805, 74HC595,
  ATmega328P, ...) plus passives resolved straight from the `Value` field.
- **Datasheet extraction** (`model-extract` binary): when a part is not in the
  DB, point galvani at the part's PDF datasheet and an LLM backend (codex by
  default) extracts a model entry in the same TOML schema. The result is dropped
  into `~/.galvani/models/` and loaded as a user-dir entry.
- **User SPICE**: a `.model` / `.subckt` card you supply always wins, so you can
  override anything with a vendor-provided SPICE deck.

The resolution order itself lives in `ModelLibrary::resolve`
(`crates/galvani-models/src/lib.rs`): SPICE cards first, then user TOML entries
(which is where extracted models land), then the built-in DB.

## Pointing galvani at a datasheet

```bash
# build the extractor
cargo build -p galvani-models --bin model-extract

# extract a model from a PDF datasheet
./target/debug/model-extract \
    --pdf testdata/datasheets/BC847.pdf \
    --part BC847 \
    --kind bjt_npn \
    --out-dir ~/.galvani/models       # default if omitted
```

`--kind` is one of: `passive | diode | bjt_npn | bjt_pnp | nmos | pmos | vreg |
opamp | comparator | analog_switch | digital | dac | adc | shift_register | mcu
| connector | ignore`.

The tool writes `<part>.toml` to the output directory. Any TOML in
`~/.galvani/models/` is loaded as a user-dir entry the next time the library is
built, so an extracted part is immediately resolvable by value/MPN.

## What gets extracted

The extractor pulls two things from the datasheet:

1. **SPICE-level parameters** for the device kind (`is`, `bf`, `nf`, `vaf` for a
   BJT; `is`, `n`, `rs` for a diode; `vout`, `dropout_v`, `iq_a` for an LDO; and
   so on). Where a value is not stated verbatim, the model derives it from a
   stated operating point (e.g. `is` from VBE at a known IC) and says so in a
   comment, or falls back to a family-typical value tagged `# estimated`.
2. **Absolute-maximum ratings** into `[models.ratings]` — `max_current_a`,
   `max_surge_current_a`, `max_power_w`, `max_voltage_v`,
   `max_junction_temp_c`. These feed the **stress monitor**
   (`crates/galvani-engine/src/stress.rs`): the live operating point is checked
   against them and faults are raised when a part is driven past its limits. An
   omitted field means "no limit known".

Every numeric line carries a comment citing where in the datasheet it came from,
so an extracted model is auditable.

## The pipeline, end to end

`crates/galvani-models/src/bin/model_extract.rs`:

1. **PDF → text** via `pdftotext` (if present). When it is absent the backend is
   told to read the PDF directly from its working directory.
2. **Prompt** built per kind, listing the required params, the ratings to pull,
   and the physical bounds each value must respect.
3. **Backend call**:
   - **codex** (default): `codex exec --sandbox workspace-write
     --skip-git-repo-check --cd <pdf_dir>`. stdin is closed so codex does not
     block; the final agent message (clean TOML) comes back on stdout while
     session logging goes to stderr. A hard timeout (10 min) kills a stuck run.
   - **API** (optional): set `GALVANI_LLM_API_KEY` (+ `GALVANI_LLM_MODEL`,
     `GALVANI_LLM_BASE_URL`) to use an OpenAI-compatible chat endpoint instead.
4. **Parse + validate**: the reply is parsed as TOML, the device kind is checked
   against the requested kind, and every param is range-checked
   (`crates/galvani-models/src/validation.rs`). A failure feeds the error back
   to the backend for one retry.
5. **Write** `<part>.toml`.

### Failure modes

The extractor fails loudly and usefully:

- **No backend**: clear error listing codex / `GALVANI_LLM_API_KEY` / the
  offline mock as options.
- **codex timeout**: killed after 10 minutes with a message to tighten the
  prompt or use the API backend.
- **Empty / prose reply**: rejected with "empty reply" or "no [[models]] table"
  rather than a confusing TOML parse error.
- **Wrong kind**: a diode card returned for a `bjt_npn` request is rejected
  ("kind mismatch") so the binder never stamps the wrong device.
- **Out-of-range params**: rejected by the static range check before writing.

## Physical validation

Parsing and range checks are necessary but not sufficient: a model can be
syntactically fine and still physically wrong (an `is` off by orders of
magnitude, an LDO that does not regulate). So extracted models are validated by
**simulation** against the datasheet's spec'd operating point, in
`crates/galvani-engine/tests/datasheet_validation.rs`:

| kind  | check                                                                 |
|-------|-----------------------------------------------------------------------|
| diode | forward voltage at a stated forward current (1N4148: ~0.7 V at 10 mA) |
| BJT   | DC current gain beta = Ic/Ib **and** Vbe at the bias (BC847: hFE in 110..450, Vbe ~0.66 V at 2 mA) |
| LDO   | output voltage under a real load, within tolerance (AMS1117-3.3: 3.30 V) |

The same suite has a garbage-rejection test proving a physically absurd model
(a "transistor" with beta 5, or a junction that never turns on) is rejected by
simulation rather than silently bound.

Measured results from real codex extractions of the three reference datasheets:

| Part         | Simulated                     | Datasheet truth                  |
|--------------|-------------------------------|----------------------------------|
| BC847 (NPN)  | beta 171, Vbe 0.660 V @ 2 mA  | hFE 110..450 (typ 180), Vbe 660 mV |
| 1N4148 (D)   | Vf 0.688 V @ 10.1 mA          | Vf max 1.0 V @ 10 mA (real ~0.7 V) |
| AMS1117-3.3  | Vout 3.300 V @ 33 mA load     | 3.300 V (3.201..3.399 V)         |

## Tests

- **Offline (always run in CI)**:
  - `galvani-models` `offline_pipeline_with_mock_reply` drives the whole
    extractor with a canned reply via `GALVANI_EXTRACT_MOCK_REPLY=<file>` — no
    codex, no network.
  - `galvani-engine` `fixture_*` physical-validation tests simulate canned
    models and assert the datasheet numbers.
- **Live (manual)**: `galvani-models` `extract_bc847_live` is `#[ignore]`d and
  runs real codex against `testdata/datasheets/BC847.pdf`. See
  `crates/galvani-models/README_DATASHEET.md`.
