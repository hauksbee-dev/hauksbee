# Device models: built-in, SPICE, and datasheet extraction

The source-selection, provenance, uncertainty, and fail-closed accuracy policy
is documented in [SOURCE_LADDER.md](SOURCE_LADDER.md). Source tier and storage
layer are deliberately separate; inspect both with `hauksbee models resolve`.

Components may resolve to executable models, identity-only records, or an
explicit unresolved/open result. Executable models arrive by **four** authoring
routes. The resolver records both the semantic source tier and the six storage
layers; semantic tier orders sources first, while layer and specificity make
selection deterministic inside one tier.

Simple `vreg` cards may set non-negative `cout_min_f` and an optional positive `cout_esr_min_ohm`/`cout_esr_max_ohm` pair. `cout_min_f = 0` records a sourced statement that no output capacitor is required for stability; omitting the field means the requirement is unknown. For positive minima, fitted OUT-to-ground capacitance is summed and shortfalls violate an operating limit. Use `cout_min_f` only for a stated minimum ("a minimum of 4.7 µF is required", a Recommended Operating Conditions row). A value the datasheet only recommends or calls sufficient ("22 µF tantalum will ensure stability", "stable with 1 µF") goes in positive `cout_recommended_f`, with optional `cout_recommended_type` naming the capacitor type it specifies; a shortfall against it is a note, not a violated limit, and only a regulator with no output capacitor at all is reported as outside every stated condition. A card may carry both (`cout_min_f` <= `cout_recommended_f`). Capacitors across the output whose value does not read as a capacitance make the check abstain, naming them. ESR remains a caveat because board files do not carry it and Hauksbee never guesses it.

Any card may add `[[models.supply_cap]]` rows (a minimum capacitance on a supply pin: `role`, `side` = `input` or `output`, `grade` = `required` or `recommended`, `min_f`, optional `min_single_f` when the datasheet asks for one capacitor of that size rather than a total, `basis`, `note`) and `[[models.pin_bias]]` rows (a pull-up, pull-down or defined idle level a non-strap pin needs: `role`, `level` = `high` or `low`, `needs` = `pull` or `idle`, optional `min_ohm`/`max_ohm`, `open_ok = true` for an open-drain output nothing need read, `opposed_only = true` for a level the part needs only while a mode is in use, as a ROM-bootloader UART receive pin that idles high: a missing or floating bias is then fine and only a pull to the opposite level is reported, `basis`, `note`). Both need a `basis` that names the datasheet table or page. A `vreg` output keeps `cout_min_f`/`cout_recommended_f` and may not repeat it as a `supply_cap` row. Use `required` only for wording the datasheet makes mandatory ("require", "must", a Recommended Operating Conditions minimum); "recommended", "typically" and "in most situations" are `recommended`. Straps stay in `[[models.straps]]`.

A linear `vreg` (fixed `vout` or adjustable) draws its output current plus `iq_a` from IN, the pass element's `Iin = Iout + Iq`. A DC solve draws the output current exactly; a co-simulation draws each chunk the mean output current of the chunk before, so an output capacitor's integration noise never reaches the upstream supply. The draw fades out below the card's `dropout_v` headroom, where the rail is already refused. A behavioural converter card draws `Vsw*Iout / efficiency` of input power. Like every regulating part it owns its rail: a supply-named net the part drives, or reaches through a series conductor (an inductor, a ferrite, a resistor of at most 1 Ω) or a switch, takes no ideal auto-rail (`hauksbee-bind/src/rail_owner.rs`). `crates/hauksbee-engine/tests/power_invariants.rs` checks every builtin `vreg` and `analog_switch` card for input power covering output power, efficiency and `iq_a` within 1%, and no fighting source.

A simple `vreg` (fixed, or adjustable with a `gnd` pin) whose `[models.pins]` names an `en` or `shdn` pin switches its rail on that pin (`hauksbee-bind/src/vreg_enable.rs`). The card states the guaranteed levels as `en_on_v`/`en_off_v` (both or neither), the polarity as `en_on_level` (`"high"`, the default, or `"low"` for a shutdown pin, with `en_off_v` then above `en_on_v`), at most one internal pull (`en_pull_down_ohms`, `en_pull_up_ohms`, `en_pull_down_a` or `en_pull_up_a`, a pull-up running to IN), and an active output discharge as `en_discharge_ohms`. The source returns to GND through a latching switch on the pin: it closes past the on level, opens past the off level and holds between them, so a slow edge does not chatter. Off, the rail floats and decays through its loads and the discharge. A card without levels switches at an assumed 1.2 V on / 0.4 V off (the reverse for an on-low pin), and the bind row says so as a partial model. A pin the board fixes is read without a solve: tied to IN, tied to GND, pulled to GND by resistors with nothing else on its net, or left open (the card's pull decides; with no pull, the datasheet's stated open-pin state `en_open` = `"off"` or `"on"` decides, for a part whose pin table says "logic low or open = shutdown" without printing a pull value, and must not contradict a stated pull, with the datasheet's own words and page as `en_open_basis`, which the board check `regulator_held_off` quotes when it finds the pin left open; with neither the part is assumed on, and the row says so). A regulator the board holds off that way makes its output an unpowered rail, which hauksbee-ci banners with the cause before any result. The supply current follows the same pin: `iq_a` is drawn from IN while the part is on, and while it is off only the optional `i_shdn_a` (the datasheet's shutdown current, below `iq_a`; a card without it draws 0 A off and the bind row says so as a partial model). A load switch is carded as an `analog_switch` with `ctrl` = the enable, `in_out_b` = IN, `in_out_a` = OUT and `vss` = GND, and states `en_on_level = "high"`; it takes the same `en_open`, `en_open_basis`, `en_pull_down_ohms`/`en_pull_up_ohms` (or `_a`) and `must_not_float_roles = "ctrl"` as a regulator, and `en_open = "undefined"` for a datasheet that says an open EN is undefined (it may not be combined with a pull; the board check then requires the pin driven). Its required input and output capacitance are `[[models.supply_cap]]` rows on `in_out_b` and `in_out_a`. Active-low load switches are not carded. An LM317-class part (no `gnd` pin) has no return to switch, so its enable stays unmodelled and its row says so.

A negative regulator (7905/7912/7915) draws the same current from its negative input, with `iq_a` taking the input's sign.

Every active output draws its current from its part's supply pins, so a load on an output shows on the rail that feeds the part. An `opamp` output, and a push-pull `comparator` output (`open_collector` unset), sources from `V+` (`vcc`, `vs`, `vdd`) and sinks into `V-` (`vss`, `vee`, or a ground pin, which carries nothing): the solver stamps a mirror of the output current onto the two supply rows each iteration, blended smoothly by output direction, so ground nets zero. An `opamp` card's optional `iq_a` is the package's supply current with the outputs unloaded, drawn `V+` to `V-`; a card without one draws none, and lists `quiescent_supply_current` under `[models.coverage] missing` so the gap is visible rather than guessed. A digital part's pin driver, a microcontroller's GPIO driver and a `dac` output draw their sourced current from the part's `vcc`/`vdd` (or the `[models.level_rails]` pin that powers that output) and return their sunk current into `vss`/`gnd`; the co-simulation sets these draws each chunk from the chunk-mean current the driver delivered, for the same reason as the regulator's. A card with `open_collector = true` (an open-collector `digital` array such as the ULN2003, or an open-collector `comparator`) only sinks: it never sources into a pull-down, draws nothing from a supply when off, and returns its sunk current into the part's `gnd`/`vss` pin.

A `vreg` card whose `[models.current_program]` is `regulated_current` is a linear charger (TP4054/TP4056/MCP7383x class), recognised by that data and never by part name, and binds as a CC/CV source rather than an ideal one. Its output is a Thevenin CC/CV source: the float voltage `vout` (held down to `V(IN) - dropout_v` when the input cannot pass it) behind `10 mV / I_prog` of output resistance up to the current limit, and a limit that holds into any load past it, so a short load draws `I_prog` and a cell below the float is charged at `I_prog`. `I_prog` is the card's equation evaluated at the resistance the PROG pin sees to ground on the bound board (the same reading the ampacity check cites, `hauksbee-bind/src/program_resistor.rs`). The input draws that same current plus `iq_a`, so a charging cell shows up as `I_prog + iq_a` on the USB input, and a cell declared on BAT is the charger's load instead of a fight (`rail_owner` treats the charger's output as a current-limited feed). When the PROG network cannot be read, or the equation declines its resistance, the limit is the card's `max_operating_current_a` and the bind row carries a partial-model warning saying the charge current and the input current are upper bounds read from the card, not the board. A power-path part (a `sys` rail plus a `bat` charge port, the BQ2407x/BQ25185 shape) says so in `current_out_roles`: its programmed current leaves at one role other than `out`, and its `out` is a system rail that carries more than the charge current. Its `out` keeps the plain regulator stamp, and the charge port gets a second copy of the CC/CV stamp above, from IN to the port: floating at the `vbat_reg_v` param, behind `dropout_v + bat_dropout_v` of headroom, at the programmed current capped by an `input_limit_a` param when the card has one, with no extra `iq_a`. Both paths draw from IN, so IN carries the rail's load plus the charge current, and the dissipation watch charges the package `(V(in) - V(sys)) * I_sys + (V(in) - V(bat)) * I_bat`. The float and the input limit usually come from a strap resistor (see `[[models.resistor_bins]]` under Board-programmable resistors); a port with no float selected is not stamped, and the bind row says why. The input limit caps the charge current alone, not the system load and the charge together (no DPPM/VINDPM), and the battery does not supplement the system rail. Still not modelled: trickle precharge, C/10 termination, recharge hysteresis, thermal foldback, the enable and thermistor pins.

An adjustable `vreg` card carries `vref` instead of `vout`, and the board's divider sets the rail. Its `[models.pins]` decides the law: with no `gnd` pin it is the LM317 class (`VOUT - VADJ = vref`, so `VOUT = vref*(1 + R2/R1) + iadj_a*R2` with R1 on OUT-ADJ; requires `iadj_a`), and with a `gnd` pin it is ground-referenced like most modern LDOs (`VFB - VGND = vref`, so `VOUT = vref*(1 + R1/R2)` with R1 on OUT-FB; requires `iq_a`, drawn from IN to GND). Both require `dropout_v`, checked against the solved rail.

The four authoring routes:

- **Built-in DB** (`crates/hauksbee-models/db/*.toml`): the curated library
  that ships with hauksbee. It covers the common families (BC847, 1N4148,
  7805, 74HC595, ATmega328P, and so on) plus passives resolved straight from
  the `Value` field.
- **Datasheet extraction**: when a part is not in the DB, point hauksbee at
  the part's PDF datasheet and an LLM backend (saved in
  `~/.config/hauksbee/extract.toml`, or auto-detected; see "Choosing a
  backend" below) drafts a model entry in the same TOML schema. It runs from
  the web report, from
  `hauksbee models extract`, or from the standalone `model-extract` binary.
  Every one of the three states what leaves your machine and asks before it
  sends anything. The result is a draft to check. It carries tier
  `datasheet-derived` (the machine-draft rung in
  [SOURCE_LADDER.md](SOURCE_LADDER.md); the web card labels it
  `datasheet-extracted`) and a `[[models.source.references]]` entry pinning the
  PDF's sha256. Electrical model cards land in
  `~/.hauksbee/models/` and load as user-dir entries; register-map sensor
  drafts use `<part>.sensor.toml` and must be reviewed and attached explicitly.
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

The shortest path is the web report: a part with no model carries a "Draft a
model from a datasheet" button, which states what is sent and asks before it
sends anything (see [From the browser](#from-the-browser)). Beside it, "Write a
part yourself" takes hauksbee's own TOML, validated as you type by the same
validator that runs on save, or SPICE pasted or loaded from a `.lib`, `.mod`,
`.cir`, `.sp` or `.ckt`. A `.subckt` is flattened at load (ports mapped to your
nodes, nested calls resolved). A SPICE deck is checked there rather than saved
as a model, because a deck is something to simulate; turning one into a reusable
part means writing the entry that claims your component.

From a terminal:

```bash
hauksbee models extract \
    --pdf testdata/datasheets/BC846.pdf \
    --part BC846 \
    --out-dir ~/.hauksbee/models       # default if omitted
```

It prints what will leave your machine and waits for a yes. In a script, where
there is nobody to ask, it refuses rather than assuming: pass `--yes` (or `-y`)
when you mean it.

If `<part>.toml` (or `<part>.sensor.toml`) already exists in the output
directory it refuses before sending anything, since that file may hold your
corrections: move it, pick another `--out-dir`, or pass `--force` to replace
it. A saved `[[models]]` draft is stamped by the tool, not the model:
`tier = "datasheet-derived"` and a reference naming the PDF file and its
sha256 (no URL: the tool only knows the local file; add one when you review).
A `[sensor]` spec has no source table and is saved as validated.
A `value_re` that is just the literal part name is rewritten to a
case- and separator-insensitive rule (`^PART[-_ ]?002DD$` style) that also
admits the ordering codes the draft lists under `[models.variant].orderable`,
and nothing with other digits. A value rule that lists the value outright (a
finite, anchored set of spellings) resolves with confidence `exact`; a family
pattern resolves `family`. `hauksbee doctor` shows the resolved extraction
backend on an `extract` row, from the same resolution as
`hauksbee models backend show`, without calling a model.

### Choosing a backend

`--backend` picks which LLM does the reading. All four run the same prompt,
the same validation, and the same retry-with-feedback loop:

| backend | requirement | default model / effort | flags |
|---------|-------------|-------------------------|-------|
| `claude-code` | `claude` CLI in PATH, signed in | `claude-sonnet-5-5` → `claude-opus-5-5`, high | `--backend claude-code`, `--model`, `--effort` |
| `agy` | `agy` CLI in PATH, signed in (Antigravity) | `gemini-3.8-flash`, high | `--backend agy`, `--model`, `--effort` |
| `codex` | `codex` CLI in PATH, signed in | `gpt-6.1-sol`, high | `--backend codex`, `--model`, `--effort` |
| `api` | key in the env var named by `--api-key-env` (default `OPENAI_API_KEY`; a set `HAUKSBEE_LLM_API_KEY` is honoured) | `gpt-6.1-sol` | `--backend api`, `--api-base`, `--model`, `--api-key-env` |

Every default is the most accurate current model at `high` effort (not `max`:
the extra cost has not been shown to buy a better pin map). GPT-6 Luna
(`gpt-6-luna`, on `codex` and `api`) is the recommended budget choice: very good
for most circuit extraction and much cheaper, so pick it to save money. At
OpenAI's list prices (2026-10-06, USD per million input / cached input /
output tokens) Sol is $2.00 / $0.10 / $10.00 and Luna $0.10 / $0.01 / $0.50,
about 20 times cheaper; the fast tier (`service_tier="fast"`, formerly
`"priority"`) costs double. Those prices apply to an API key. A `codex` signed in
with a ChatGPT account (`codex login status`) draws on the plan's Codex usage
instead, with no per-call charge. On a ChatGPT Plus plan, one measured board
(Glasgow revD1, high effort, standard tier, one run) took about 1% of the
5-hour limit and 0.1% of the weekly limit per part on Luna, and about 6% and
1% per part on Sol (around 13 times as much); extraction runs once per part and
the card is reused. Those are integer-percentage readings of one run, and plan
limits change. At list price the same runs came to about $0.05 per part on Luna
and $0.50 per part on Sol.
Codex and `api` runs end with a token count; metered ones (an API key, or
`api`) add a list-price estimate that includes the fast-tier doubling, and
`HAUKSBEE_LLM_PRICE=in,cached,out` prices any other model. `hauksbee models backend show` and the web
Settings page list each backend's picks with the same one-line notes. Older
names are not offered but still work: a saved or typed `--model` is used as
written, and the CLI and Settings page only note that a newer one is recommended.

`--api-key-env` takes the NAME of an environment variable, never the key
itself: the key is read from the environment at call time and is never stored
or logged. A missing CLI, a missing sign-in, or an unset key variable errors
up front with the exact fix (install the tool, sign in, or `export
OPENAI_API_KEY=...`), before anything is sent.

#### The config file

Which backend `models extract` uses, and on which model and effort, is a
persistent setting kept in `~/.config/hauksbee/extract.toml` (override the
path with `$HAUKSBEE_EXTRACT_CONFIG`, or it follows `$XDG_CONFIG_HOME`). The
file records only what you have chosen; anything left out takes hauksbee's
built-in default, so the file stays short and survives a version upgrade that
changes a default. Manage it with `hauksbee models backend`:

```bash
hauksbee models backend setup      # interactive: pick a preset, tweak model/effort, save
hauksbee models backend show       # config path, resolved settings, backend availability
hauksbee models backend presets    # list the built-in presets
hauksbee models backend use agy    # apply a preset (or a bare backend name)
hauksbee models backend set claude-code.effort=max
hauksbee models backend unset claude-code.effort
hauksbee models backend keys       # every settable key, its meaning and default
hauksbee models backend check      # is the configured backend ready to run?
hauksbee models backend check --send   # + one real connectivity call
hauksbee models backend reset      # delete the file, back to every default
hauksbee models backend path       # print the config path
```

Presets (`models backend presets`): `claude-code`, `claude-code-haiku`,
`claude-code-sonnet`, `claude-code-opus`, `agy`, `agy-pro`, `codex`, `codex-luna`,
`openai`, `openrouter`, `ollama`. Applying one changes the backend and its
model/effort/escalation, preserving advanced settings and other backends.

### Tiers and escalation

An agent backend runs a ladder: `<backend>.model` first, then each model in
`<backend>.escalate` (`HAUKSBEE_<CLAUDE|CODEX|AGY>_ESCALATE`, comma-separated;
`none` = never). The next tier runs only when a tier's draft still fails
validation after its retries, the run times out or passes its context budget,
the agent writes `low` to `confidence.txt`, or the pin map is left unresolved.
A doubtful but valid draft is kept as the fallback if every stronger tier fails.
The card's first line records the tier (`# extracted by claude-code
claude-sonnet-5-5 (tier 1 of 2)`) and each escalation's reason. An explicit
`--model` (or the web page's model field) runs that model alone.

The Claude Code default (`claude-code` preset) is Sonnet 5.5, then Opus 5.5 only
when Sonnet fails or doubts its pin map; `codex-luna` escalates Luna to Sol. The
cheapest Claude preset, `claude-code-haiku`, escalates to Sonnet on failure:
harnessed Haiku alone fell below the codex gpt-6-luna baseline on validity, so
it is never offered without a fallback. `qc/claude_tier_bench` chose the default
(2026-10-12; five parts, one run each, a small sample, so differences of one
part are noise; prices are API list prices):

| tier | exact pin maps | valid | approx. cost per part |
|------|----------------|-------|-----------------------|
| codex gpt-6-luna (baseline) | 4/5 | 5/5 | not measured here |
| Haiku 5.5 alone (staged) | 4/5 | 4/5 | $0.04 |
| Haiku 5.5, Sonnet on failure | 4/5 | 5/5 | $0.04, plus 2.3M tokens on the escalated part |
| Sonnet 5.5 alone | 5/5 | 5/5 | $0.46 (mean 546k tokens, 156 s) |

Haiku alone stopped SN74LVC1T45 at its 85k budget three times. With Sonnet as
fallback that part passed validation but named pin 1 `vcc` for `vcca`, and the
escalation cost 2.3M tokens and 17 minutes. Sonnet alone was the only tier with
every pin map exact. A model with a
context budget (Haiku: 85k tokens, so no request reaches 100k) runs the staged harness
(`extract_harness.rs`): a locate agent reads `page-index.md` and only the pages
it needs and writes compact `notes.md` (package, pin map, key pages and values,
ambiguities); a fresh draft agent writes the card from the notes, which its
prompt carries along with the list of pages (so it neither re-reads nor lists
them); a validator failure goes to a fresh fix agent that finds the failed draft
in `model.toml` (and a copy as `previous.toml`) and repairs it with small edits
rather than rewriting the card. Each agent is stopped if its streamed context
passes the budget; a card it had already written is validated and fixed like
any other, and one it had not written is started afresh from the notes. `HAUKSBEE_EXTRACT_HARNESS=staged|single`
forces the harness on or off.

**What a run costs.** Billing follows how the CLI is signed in, never a setting:
an API key (`ANTHROPIC_API_KEY`, or `claude auth status` / `codex login status`
naming one) is metered and shown in list-price dollars; a subscription sign-in
(claude.ai, ChatGPT) shows no dollars but the run's share of the plan: the
weekly and 5-hour usage read before and after (`claude -p /usage`; for codex,
`account/rateLimits/read` before the run and the last `token_count` rate limits
in its `--json` stream after it), account-wide. A failed reading is
"usage unavailable", an unrecognised sign-in "billing unknown"; neither shows
dollars.

`HAUKSBEE_EXTRACT_EVENTS=<file>` appends the run's activity feed (tier, stage,
pages read, tokens, billing and metered cost so far, validator verdicts,
escalations, the closing `plan` reading)
as JSONL; the web extraction streams the same events as `activity:` lines.

For a CLI-compatible wrapper, set `<backend>.command` to its executable name or
path (not a shell command), or use `HAUKSBEE_CLAUDE_COMMAND`, `HAUKSBEE_AGY_COMMAND`,
or `HAUKSBEE_CODEX_COMMAND`. Settings exposes this under Advanced → Executable.
For example: `hauksbee models backend set 'claude-code.command=/path/my agent'`.
The wrapper controls routing, authentication and charges; presets keep it.
Missing/nonexecutable wrappers fail without fallback. Unset the command to
restore the native CLI; ordinary defaults are unchanged.

`backend check --send` checks an agent's access as well as connectivity: it
must read the scratch prompt and write exactly `OK` to `model.toml`. An answer
on stdout alone does not pass. The API check remains a connectivity call.
A denied write is a backend-access failure, not evidence that the model cannot
extract a datasheet. Scratch paths are canonicalized before launching the CLI.

For an opt-in Claude Code automatic-permission trial, set
`claude-code.permission_mode=auto`; unattended callers can add
`extra_args = ["--permission-prompts", "none"]` in `[claude-code]`. Auto is not
bypass mode: explicit denials still apply, and CLI version, account and model
support determine availability. A successful read/write check alone does not
prove the classifier was active. Keep the default until that is verified for
your CLI-compatible wrapper. Codex CLI versions supporting `--approve-for-me`
can opt into automatic approval review with `permission_mode = "approve-for-me"`
in `[codex]`. Hauksbee emits that flag (with the read-confining permission
profile below) instead of `--sandbox`: the CLI rejects the two flags together.
Do not append `--sandbox` to `extra_args` either: an explicit `--sandbox`
overrides the profile and reopens every read. This differs from deprecated
`--full-auto`; check `codex exec --help` for support. An automatic reviewer can
still approve a command outside the sandbox, so `approve-for-me` is weaker than
the default.

Codex fast mode: `extra_args = ["-c", "service_tier=\"priority\""]` in `[codex]`.
Codex runs with `--ephemeral --ignore-user-config --ignore-rules` and with apps,
browser/computer use, plugins, skill search, memories, hooks, goals, subagents
and the shell snapshot disabled, so `~/.codex/config.toml` (MCP servers,
trusted projects) does not apply; sign-in still does. Skills need one more
switch: codex lists every skill under `$CODEX_HOME/skills` in the model's
context whatever the feature flags say (a real run opened
`~/.codex/skills/<name>/SKILL.md`), so hauksbee also passes
`-c skills.include_instructions=false` (checked offline with
`codex debug prompt-input`). Setting `codex.profile` / `HAUKSBEE_CODEX_PROFILE`
drops `--ignore-user-config`: your codex config and that profile then apply
(the backend summary says so); the other flags, and the read confinement, stay.

**What an extraction agent can read.** `--sandbox workspace-write` confines
codex's writes but not its reads: a run under it searched the home directory,
another project and `/tmp`, and ran a different `hauksbee` it found on PATH. So
in every mode except `danger-full-access` hauksbee selects a codex permission
profile instead (`-c default_permissions="hauksbee"` plus the profile as a
`-c permissions=...` override): shell commands may read codex's `:minimal`
OS/runtime paths, `/opt/homebrew` and `/usr/local` (so `pdftotext` and
`pdftoppm` work), the checker executable, and the scratch workspace (which they
may also write, except in `read-only` mode), and are refused everything else,
including your home directory, `~/.codex`, `/Users`, `/Volumes`, `/Library`,
`/tmp` and `$TMPDIR`. Verified against codex-cli 0.156.1 with
`codex sandbox -P hauksbee` (Seatbelt on macOS). `approve-for-me` adds a
reviewer that can approve a command outside the profile; `danger-full-access`
is `--sandbox danger-full-access` and confines nothing. The codex process itself
still reads its own sign-in from `CODEX_HOME`; no credentials are copied. The
standalone `resource-extract` tool launches codex through the same code, on a
scratch copy of the PDF. Claude Code gets `--restricted` (file tools confined
to the workspace; it also refuses `bypassPermissions`), no web tools, and a
strict Bash sandbox via `--settings` (`failIfUnavailable`, no unsandboxed
retry, reads denied under `~/`, `/Users`, `/home`, `/Volumes`, `/mnt`,
`/media`, `/tmp`, `/private/var/folders` and the temp root except the workspace
and checker); unlike codex's profile, the rest of the system (`/etc`,
`/Library`, `/Applications`) stays readable to its shell. agy gets `--sandbox`,
agy's own terminal sandbox (by agy's documentation: workspace-only files, no
network); hauksbee cannot verify that boundary or confine agy's own file tools,
so treat agy as able to read anything your account can. Headless agy auto-denies
any shell command without an allow-rule and then stops with no answer; it reads
rules only from `~/.gemini/antigravity-cli/settings.json`, never from the
workspace, so hauksbee tells it to use file tools and run only the lint command,
and reports a denial as an error naming the rule to add
(`command('<hauksbee>' models lint model.toml)` under `permissions.allow`). A custom `command`
receives the same flags, but it is your program: hauksbee cannot confine it.
`hauksbee models backend show`, doctor and the web page state the confinement
of the configured backend. What any agent reads can reach its vendor. Every agent CLI starts in
the scratch workspace with a PATH from which every directory holding a
`hauksbee` executable is removed, and the prompt names the product's own
executable by absolute path (`'<hauksbee>' models lint model.toml`) as the
only checker; the standalone `model-extract` binary names none.

`HAUKSBEE_EXTRACT_CONFIG` naming a file that does not exist is an error for
extraction, `show` and `check`; `set`, `use`, `setup` and Settings create it.

**Precedence**, per field, highest first: a command-line flag (`--backend`,
`--model`, `--effort`, ...); a `HAUKSBEE_*` environment variable (below); this
file; the built-in default. `hauksbee models backend show` prints exactly
which of these won and why.

With nothing chosen anywhere (no flag, no env var, no file), extraction takes
the API backend when `HAUKSBEE_LLM_API_KEY` is exported, otherwise the first
agent CLI found on PATH, in the order **codex, claude-code, agy**. Whichever
backend wins, `models extract` prints `Backend: <summary> (<source>)` before
asking for consent, so you are always told exactly what is about to read your
datasheet, and where the choice came from.

`HAUKSBEE_*` environment variables override the file for one run (a script or
CI job's way of overriding a saved developer choice): `HAUKSBEE_EXTRACT_BACKEND`,
`HAUKSBEE_LLM_API_KEY`, `HAUKSBEE_CLAUDE_MODEL`, `HAUKSBEE_CLAUDE_EFFORT`,
`HAUKSBEE_AGY_MODEL`, `HAUKSBEE_AGY_EFFORT`, `HAUKSBEE_CODEX_MODEL`,
`HAUKSBEE_CODEX_EFFORT`, `HAUKSBEE_CODEX_PROFILE`, `HAUKSBEE_LLM_MODEL`,
`HAUKSBEE_LLM_BASE_URL`, `HAUKSBEE_API_KEY_ENV`, `HAUKSBEE_EXTRACT_TIMEOUT_SECS`, `HAUKSBEE_EXTRACT_IDLE_SECS`,
`HAUKSBEE_EXTRACT_CONFIG`. No secret is ever written to the config file: the
`api` backend records the NAME of the environment variable holding its key,
never the key itself.

The web report has its own Settings page over the same file, so a choice made
in the terminal or in the browser is the same choice either way.

The standalone extractor uses the same contract: build it with
`cargo build -p hauksbee-models --bin model-extract`; scripted use additionally
requires `HAUKSBEE_EXTRACT_YES=1`.

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

For electrical model kinds the tool writes `<part>.toml`; the library loads it
from `~/.hauksbee/models/` on the next run. Register-map kinds write
`<part>.sensor.toml`; lint and attach that sensor specification to the intended
board or model instead of treating it as an automatically resolved device card.

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

Lint also refuses a pin map missing a role the binder reads for the kind (a
set-point `vreg` without `in`/`out`, including one that carries only
`[models.behavioral.pins]`, since no converter owns its output), so a card
that would bind OPEN at run time fails lint, and `resolve` prints that reason
and shows the part UNRESOLVED instead of bound.

For a shareable pack, use `--pack-dir` instead. It writes `pack.toml` and
`models/<id>.toml`, refuses to overwrite either, and leaves the license as an
explicit TODO. `hauksbee models add` remains the final pack validation step;
it must reject the untouched scaffold rather than install an unresolved model:

```bash
hauksbee models new U3 --board path/to/board.kicad_pcb --pack-dir ./acme-models
hauksbee models lint acme-models/models/u3-*.toml
hauksbee models add ./acme-models       # only after TODOs and license are complete
```

The board and the part documentation remain the authority; comments in a
scaffold are prompts, not evidence.

In the app, an unresolved part's **Write ... yourself** button opens the
hand-written model editor for that part (see
[Pointing hauksbee at a datasheet](#pointing-hauksbee-at-a-datasheet)). The
commands above are the route for scripts, agents and packs.

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

A load whose draw is set by data the board cannot show (an addressable LED's
colour, latched by firmware) declares its idle/quiescent draw as the segment
and the datasheet full-on draw as `unknown_state_max_a` (optional, at least
every segment's `level_a`):

```toml
[[models.behavioral.profiled_loads]]
name = "pixel"
supply_pin = "vdd"
return_pin = "vss"
unknown_state_max_a = 0.0366   # 3 x 12 mA channels + 0.6 mA, full white

[[models.behavioral.profiled_loads.segment]]
level_a = 0.0006               # quiescent, all channels off
```

Runs and the DC operating point use the segments. The DC stress check also
settles the board once with every such load at its maximum; a part
overstressed only there is a medium-severity CONDITIONAL finding naming that
state, not a pass and not a high-severity failure, because whether the state
is reached is up to firmware.

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

`hauksbee serve` puts model work on the board itself, so the steps are not
repeated here:

- **Coverage and gaps.** Click a coverage row, component or trace for the
  winning model, its validation stage, implemented and missing behaviour and
  source. A component with a gap carries **Extend**, which opens a local draft
  (for a partial executable model, a copy of the exact winning behaviour).
  Drafting is read-only and deterministic; **Save** is the separate write
  approval. None of this needs an LLM.
- **Writing a part.** "Write a part yourself" starts at a kind picker and
  validates as you type (see above).
- **Experiments.** A selected trace becomes a scope probe, assertion, 50-ohm
  source, button, switch or scenario supply. A selected I²C/SPI component opens a
  register-map row (paste TOML or pick a bundled behaviour); the browser attaches
  the exact validated bytes to the running bus, keeps the identical `[[sensor]]`
  entry for replay, and shows the engine's receipt, accepted or refused, beside
  them. Cards may instead embed a validated `[models.peripheral] kind =
  "register_map"` block, which then attaches automatically; strap roles can
  derive an I²C address from a resolved supply/ground strap, and floating or
  ambiguous straps leave the behaviour open with a named reason. Neither path
  guesses registers from a part name.
- **Datasheet extraction.** The panel checks the backend can run, shows the
  consent notice (served verbatim by the engine from
  `hauksbee_models::datasheet::CONSENT_NOTICE`, so the page cannot soften the
  CLI's wording) and requires a click, takes the datasheet, part number and kind,
  streams progress with an estimate, then shows the draft as a card with
  citations and flagged assumptions. **Nothing is written until Accept**, which
  calls `POST /api/models/save`: it re-validates the (editable) TOML, refuses to
  overwrite an existing model, and writes one file into `~/.hauksbee/models/`.
  Reject writes nothing.

The extraction is the same `hauksbee_models::datasheet` code the CLI runs. Local
agent backends (claude-code, agy, codex) read the copied PDF, text and selected
page renders in a scratch workspace; the API backend receives extracted text and
instructions at the configured endpoint. The web plumbing is
`crates/hauksbee-engine/src/webextract.rs` and
`crates/hauksbee-server/src/frontdoor.rs` (`datasheet_routes`).

## What gets extracted

The extractor pulls two things from the datasheet:

1. **SPICE-level parameters** for the device kind (`is`, `bf`, `nf`, `vaf`
   for a BJT; `is`, `n`, `rs` for a diode; `vout`, `dropout_v`, `iq_a` for
   an LDO; and so on). Where a value is not stated verbatim, the model
   derives it from a stated operating point (e.g. `is` from VBE at a known
   IC) or fits it to a typical-characteristics figure, labelled typical with
   the figure cited, and says so in a comment. If the required fact is absent,
   extraction abstains or emits an identity-only draft; it does not invent a
   family value.
2. **Ratings** into `[models.ratings]`: the prompt offers the `Ratings` keys that
   apply to the part's kind (the table below, from `Ratings::KEY_GUIDE`), and
   an unknown key is a parse error. These feed the
   **stress monitor** (`crates/hauksbee-bind/src/stress.rs`): it checks the
   live operating point against them and raises faults when a part runs past
   its limits. An omitted field means "no limit known".
3. **Published characteristic rows** with no param or rating home, including
   one-sided and typical-only rows, into `[[models.source.characteristic]]`
   (see `SOURCE_LADDER.md`).

<!-- BEGIN GENERATED: rating-keys (source: crates/hauksbee-models/tests/suite/docs_generated.rs) -->
| Key | Meaning | Kinds it applies to |
|-----|---------|---------------------|
| `max_current_a` | continuous rated current (A): diode IF, BJT IC, MOSFET ID, regulator or switch rated IOUT (a features list or operating-conditions row counts); never pulsed or peak, nor an abs-max surge unless it is the only current printed (then say so); for an IC the whole-device or supply/ground current limit, not per-pin | every kind |
| `max_surge_current_a` | non-repetitive surge / pulsed peak current (A): diode IFSM, BJT ICM, MOSFET IDM | every kind |
| `max_power_w` | total power dissipation (W): Ptot / PD for the selected package | every kind |
| `max_voltage_v` | maximum blocking/working voltage (V): diode VRRM, capacitor rated voltage, BJT VCEO, MOSFET VDS, regulator VIN abs max, IC supply abs max | every kind |
| `polarized` | true for a polarized (electrolytic/tantalum) capacitor, where reverse bias is a fault | passive |
| `max_pin_current_a` | per-pin source/sink absolute limit (A) for an IC or MCU | vreg, opamp, comparator, timer, analog_switch, digital, dac, adc, shift_register, mcu |
| `max_ripple_current_a` | capacitor rated RMS ripple current (A) at its stated frequency/temperature | passive |
| `max_junction_temp_c` | TJ maximum junction temperature (C) | every kind |
| `theta_ja_c_per_w` | Rth(j-a) junction-to-ambient thermal resistance (C/W = K/W): the row at the standard condition (25 °C, the standard test board), else the least favourable row the part number allows (highest across the packages and board conditions it leaves open) | every kind |
| `theta_jc_c_per_w` | Rth(j-c) junction-to-case thermal resistance (C/W = K/W), selected package and stated case reference only | every kind |
| `min_current_a` | FLOOR, not a ceiling (A): least current the datasheet guarantees regulation at (shunt-reference IKmin, LDO minimum load) | vreg |
| `rds_on_vgs` | MOSFET: lowest \|VGS\| (V) at which the datasheet specifies RDS(on) | nmos, pmos |
| `vgs_th_max` | MOSFET: MAXIMUM \|VGS(th)\| (V) from Electrical Characteristics | nmos, pmos |
| `vgs_max` | MOSFET: absolute-maximum gate-source voltage magnitude (V), the VGS / VGSS row (+-12 V gives 12) | nmos, pmos |
<!-- END GENERATED: rating-keys -->

Every numeric line carries a comment citing where in the datasheet it came
from, so an extracted model stays auditable.

## The pipeline, end to end

`crates/hauksbee-models/src/datasheet.rs`:

0. **A scratch workspace** is built first with a copy of the datasheet. The
   agent backends (claude-code, agy, codex) each run with their own
   non-interactive edit-permission controls; codex and claude-code reads are
   confined as described under "What an extraction agent can read". The API
   path sends its request to the configured provider.
1. **PDF to text** through `pdftotext` (if present), plus selected page images
   through `pdftoppm` at 200 DPI. At most seven pages are rendered: page one
   and the pages most relevant to pinout, ratings, and electrical tables. The
   complete copied PDF and text remain available to local agent backends.
2. **Prompt** built per kind, listing the required and optional params, the ratings to
   pull, and the physical bounds each value must respect. The bounds are
   generated from the checker's own `KindRules` (`validation::param_bounds`),
   so the prompt and the validator cannot disagree. The prompt also says how
   a min/max row with no typ supplies a required nominal (the labelled
   midpoint, with the printed bounds as an interval record), how a MOSFET
   RDS(on) row fixes `kp` and `rd + rs`, and, for sensors, which optional
   blocks exist and which interval and characteristic records sit beside the
   model-error record.
3. **Backend call** (see the backend matrix above; which one runs comes from
   `hauksbee models backend show`, unless overridden by `--backend`):
   - **claude-code** (`--backend claude-code`): headless `claude -p` in the
     sandbox, with a prompt file, an answer file (`model.toml`), a timeout,
     and the retry contract. The run is isolated from your own Claude Code
     setup (`--restricted --strict-mcp-config`): your CLAUDE.md, memory,
     custom agents, hooks, plugins, skills and MCP servers never reach the
     extraction. It gets only the Bash, Read, Edit and Write tools: no web
     access, no subagents, and no background tasks, which would keep
     `claude -p` running after the answer is written. Its Bash tool runs in
     Claude Code's sandbox (hauksbee's `--settings`); a `--settings` file
     added by a wrapper command still applies too.
   - **agy** (`--backend agy`): headless `agy --print` with agy's own
     `--sandbox`, the same contract again. hauksbee cannot verify or extend
     agy's confinement.
   - **codex** (`--backend codex`): `codex exec` with the read-confining
     permission profile, `--skip-git-repo-check --cd <workspace>`. A short pointer to `prompt.md` is
     written to stdin before EOF; the final agent message (clean TOML) comes back
     while session logging goes to stderr. A hard timeout (10 min) kills a
     stuck run.
   - **api** (`--backend api`): an OpenAI-compatible chat endpoint at
     `--api-base` (default `https://api.openai.com/v1`, or
     `HAUKSBEE_LLM_BASE_URL`), model from `--model` or `HAUKSBEE_LLM_MODEL`,
     key read at call time from the env var named by `--api-key-env`.
4. **Parse and validate**: the tool parses the reply as TOML, checks the
   device kind against the requested kind, and range-checks every param
   (`crates/hauksbee-models/src/validation.rs`). A failure feeds the error
   back to the backend for up to two retries (three attempts total).
5. **Write** `<part>.toml`.

### Failure modes

The extractor fails loudly and usefully:

- **Missing tool or key**: a selected CLI backend whose tool is not in PATH,
  or an api backend whose key variable is unset, errors with the exact fix
  (the install command, or `export OPENAI_API_KEY=...`) before anything is
  sent.
- **CLI timeout**: an agent-backend run is killed after 10 minutes (configurable via
  `hauksbee models backend set timeout_secs=...`) with a
  message to tighten the prompt or switch backend.
- **Idle stall**: codex runs stream JSONL events (`codex exec --json`). If no
  event arrives for `idle_secs` (default 300; `hauksbee models backend set
  idle_secs=...`, `0` turns it off), the run's whole process tree is killed and
  the call is retried once on the same prompt, spending one of the extraction's
  `retries` attempts. The retry is announced on stderr like any other retry
  (`attempt N stalled: ... retrying once`); a second stall fails the
  extraction. Never a different provider. agy prints only at the end of a run
  and claude-code streams once per turn (a long turn is normal), so neither is
  covered; `timeout_secs` still bounds them, and the staged harness's context
  budget bounds a budgeted Claude model.
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
| BJT   | DC current gain beta = Ic/Ib **and** Vbe at the bias (BC846: hFE in 110..450, Vbe ~0.66 V at 2 mA) |
| LDO   | output voltage under a simulated resistor load, within tolerance (AMS1117-3.3: 3.30 V) |

The same suite has a garbage-rejection test proving simulation rejects a
physically absurd model (a "transistor" with beta 5, or a junction that
never turns on) rather than silently binding it.

Example simulation checks from the reference fixtures (not hardware
measurements):

| Part         | Simulated                     | Datasheet truth                  |
|--------------|-------------------------------|----------------------------------|
| BC846 (NPN)  | beta 171, Vbe 0.660 V @ 2 mA  | hFE 110..450 (typ 180), Vbe 660 mV |
| 1N4148 (D)   | Vf 0.688 V @ 10.1 mA          | Vf max 1.0 V @ 10 mA (real ~0.7 V) |
| AMS1117-3.3  | Vout 3.300 V @ 33 mA load     | 3.300 V (3.201..3.399 V)         |

## Reset-state pad modes and externally required strap levels

An MCU card may state what a pad does while the chip is in reset, as data a board
check reads (`reset_state.rs`, shared by `control_held_off.rs` and later the
simulation floating-node detector):

```toml
[models.pin_capability.reset_mode]
basis = "Datasheet <part> <version>, Table <n> pin states in reset (PDF p.<n>)"
[models.pin_capability.reset_mode.pads]
"5" = "pull_up"     # pull_up, pull_down, drives_high or drives_low
```

A pad of an `mcu` card that the table does not list is high-impedance in reset,
which is what every GPIO of the common families does until firmware configures it.
`hi_z` is never written; listing a pad is how a card says it is not floating. A
table with a basis and no pads is allowed and means the datasheet states that every
pad of the card is high impedance in reset (the AVR sheets say the port pins are
tri-stated in reset); a card with no table at all says nothing, so its pads are
unknown, not stated. The `At Reset` column of the Espressif pin tables, the
reset-state column of the RP2040 pin list and the reference manual's debug-pin
paragraph (STM32) are the usual sources, quoted by table and page in `basis`. A
board enable pin that only such a pad drives, with no pull on the net, floats while
the MCU resets, and `control_held_off.rs` reads what the enable card says an open
pin does (`en_open`, `en_pull_*`, `en_on_level`; a load switch or a motor driver
alike) to report whether the part can be on during that window.

A `[[models.boot_inputs]]` row may add `external_level = "<datasheet words>"` when
the vendor says the pin must be given an external pull and not left to the chip's
own, as a strap that latches the wrong mode on some power-ups. Leave it unset when
the datasheet only says a bare pin settles to its internal pull (the ESP32 family
sheets say that, so no ESP card sets it today). The strap check then reports a pin
with nothing on the board that defines it, and the co-sim boot report uses the same
wording (`hauksbee_models::boot_mode::undriven_strap_note`).

## Pin capability: which pads are ADC inputs

An MCU card may state which pads of its `[models.pins]` are analogue-to-digital
inputs, as data a board check reads (`analog_inputs.rs`, an analogue source on a pad
with no ADC channel):

```toml
[models.pin_capability.adc]
basis = "Datasheet <part> <version>, Table <n> pin definitions (PDF p.<n>)"
[models.pin_capability.adc.pads]
"23" = "ADC0"      # pad number to the ADC channel on it
"24" = "ADC1"
```

The table is a complete statement about the card's own package: a pad listed has
that channel, and every other pad of `[models.pins]` has none. A part whose pad map
holds only some of the package's pads (a module card that names the strap and UART
pads) lists the ADC ones among those, and the rest of the package is not judged. A
card with no table says nothing and the check abstains. The `basis` cites the
datasheet table and page the pad list was read from; a table written from memory or
from another package's pin map is a wrong card, not a missing one, so read the pad
and channel off the pin-definition table of the card's own package. Validation
(`validation.rs`, `check_pin_capability`) requires an `mcu` card, a basis, pads that
exist in `[models.pins]`, none of them a supply or ground role, and a channel for
each. `card_coverage` lists the table (`adc_pins`) among an MCU card's needs when
the datasheet mentions an ADC.

### Partial tables: tri-state, open-drain and internal pull-up pads

Three more tables state a property of the pads they list and say nothing about the
rest (a pad not listed is unknown, never "no"). Each has a `basis` citing the
datasheet table or sentence and its page, and `pads` keyed by pad number of
`[models.pins]`. None may list a supply or ground pad.

```toml
[models.pin_capability.tri_state]      # non-MCU cards
basis = "Datasheet <part> sec 7.5 p.14: DO is high impedance while /CS is high"
[models.pin_capability.tri_state.pads]
"2" = "cs"          # output pad to the role of the pad whose inactive level releases it

[models.pin_capability.open_drain]     # non-MCU cards
basis = "Datasheet <part> Table 5-1 p.3: ALERT is open-drain"
[models.pin_capability.open_drain.pads]
"3" = "ALERT"       # output pad to the datasheet's name for the signal

[models.pin_capability.pull_up]        # MCU cards
basis = "Datasheet <part> sec 14.2 p.85: PORTxn=1 with DDxn=0 enables the pull-up"
[models.pin_capability.pull_up.pads]
"2" = "PORTD0"      # pad to the register or setting that enables the pull-up
```

`output_release.rs` reads `tri_state` to drop an output-contention finding when every
driver on the net is released by its own chip select (a shared SPI MISO line);
`open_drain_pullup.rs` reads `open_drain` and `pull_up` to find an open-drain output
with no resistor to a rail and no receiving MCU pin with a stated internal pull-up.
Validation (`check_listed_pads`) requires a basis, at least one pad, pads that exist,
a releasing role the card itself carries, `pull_up` on `mcu` cards only and the other
two on cards that are not `mcu`.
### Other peripheral tables

The same section holds one table per hardware peripheral function, for the board
check `pin_mux.rs` (a bus wired to pads that cannot carry it): `spi_sck`, `spi_mosi`,
`spi_miso`, `i2c_sda`, `i2c_scl`, `uart_tx`, `uart_rx` and `pwm` (the keys are
`hauksbee_ir::pin_functions::PeripheralFunction`).

```toml
[models.pin_capability.i2c_sda]
basis = "Datasheet <part> <version>, Table <n> alternate functions (PDF p.<n>)"
[models.pin_capability.i2c_sda.pads]
"27" = "SDA"        # pad number to the instance and signal on it (SPI1_SCK, OC1A)
```

Each is a complete statement about the package, like the ADC table: a pad listed can
carry the function in hardware, and every other pad of `[models.pins]` cannot. A
software (bit-banged) bus is not covered. A part whose pins reach any pad through a
routing matrix (an ESP32 GPIO matrix, an RP2040 PIO) declares no table for the
function. A mode of another peripheral that the datasheet states can carry the
function (the ATmega328P USART's SPI mode for SPI) belongs in the table with its
instance named. A pad that serves the function only when the part is the bus master
(that USART mode has no slave role) is also listed in the table's `master_only = ["6"]`
key, so a bus whose master is another part does not count it. Validation is the ADC table's: an `mcu` card, a basis, pads that exist
in `[models.pins]` and none a supply or ground role.

## Pin capability: on-chip crystal load capacitors

A card (an MCU's, a clock generator's such as the Si5351, an RTC's) states the
load capacitance built into an oscillator pad pair, so the
crystal check (`crystal_load_cap`, `docs/checks/SI_CHECKS.md`) can tell a crystal
with no external caps that the chip loads from one that is missing them. With none
fitted the bank reaches at most `max_f / 2` (the two pads in series), and a
crystal with a larger CL is flagged as needing external caps:

```toml
[[models.pin_capability.xtal_int_caps]]
oscillator = "LFXO"              # the oscillator's name in the datasheet
pads = ["39", "40"]              # exactly two pads of [models.pins]: its XI and XO
min_f = 4.0e-12                  # capacitance each pad has to ground, farads
max_f = 24.5e-12
includes_pin_capacitance = false # optional; true when the figure already counts the IC pin
max_crystal_cl_f = 12.0e-12      # optional; the largest crystal CL the datasheet supports
max_external_load_f = 2.0e-12    # optional; most external capacitance across the pair
basis = "Datasheet <part> <version>, Table <n> (PDF p.<n>): <what the row says>"
```

`max_crystal_cl_f` and `max_external_load_f` (each 0.1 pF to 100 pF) are the limits
the datasheet states for the crystal's load capacitance and for the capacitors the
board adds, the latter taken across the pair (the two external caps in series). The
crystal check flags a crystal or a pair of caps beyond them at medium, citing the
basis, even where the chip's load range would cover the crystal's CL.

`includes_pin_capacitance = true` (needs a `min_f`/`max_f` range) says the printed
load is the whole capacitance the IC puts on the pad, pin included. The check then
assumes only 0-1 pF of trace stray instead of 3-5 pF, so the pin is not counted
twice. The Si5351 cards set it (data sheet 4.1.1, p.15: under 2 pF of additional
external load).

`min_f`/`max_f` are the per-pad range the datasheet prints (give both or neither).
Leave them out when the bank is software-tuned and the datasheet prints no range:
the check then judges nothing for that oscillator. Validation requires a basis, two
distinct pads that exist in `[models.pins]` and are not supply or ground roles, and
`0.5 pF <= min_f <= max_f <= 100 pF`. When the datasheet gives the load across the
pair rather than per pad (the Si5351's 6/8/10 pF CL), `min_f`/`max_f` are twice it:
two equal pads in series present half of each.

The Si5351 has one card per package and variant, because the pad numbers differ:
`si5351{a,b,c}_qfn20` (20-QFN, data sheet Tables 14-16, pp.27-29),
`si5351{a,b,c}_qfn16` (16-QFN, Tables 17-19, pp.30-32) and `si5351a_msop10`
(10-MSOP, Table 20, p.33; A only). Each is matched on the ordering code (`-GM`,
`-GM1`, `-GT`, Figure 24 p.34) and on a footprint that names its package, so a bare
`SI5351A` value binds a card only where the footprint says which package it is;
otherwise it binds none rather than a pin map that may be the wrong package's.

A crystal card (`passive_class = "crystal"`, matched by `mpn_re`) carries the
crystal's own figures as params: `cl_f` (specified load capacitance), `c0_f`
(shunt capacitance; the datasheet's maximum is the safe figure) and `c1_f`
(motional capacitance, when printed). Each figure comes from the crystal's
datasheet, which the card's `[models.source]` cites with its sha256.

`startup_s` is the crystal's start-up time, enable to usable oscillation, when the
datasheet prints one. The co-simulation holds the clock the crystal feeds invalid
that long: a ready flag the oscillator raises itself (STM32 `HSERDY`) waits the
stated time, and firmware that trusts a fixed delay shorter than it (the RP2040's
`XOSC_STARTUP.DELAY`) is halted at the switch, with a `clock not ready` finding
naming the crystal, its start-up and the firmware's wait. Without the field a
crystal takes a class default marked assumed: 1 ms for an MHz crystal (the figure
the RP2040 datasheet s2.16.3 gives for its 12 MHz reference-design crystal) and
1 s for a 32.768 kHz tuning fork.

A ground-referenced adjustable regulator or converter card may state `ifb_a`, the
feedback pin's own current *into* the pin (signed, as the datasheet prints it). It
flows on through the divider's top resistor and moves the rail by `ifb_a * Rtop`,
which the divider checks add to `Vref * (1 + Rtop/Rbottom)`.

## Peripheral limits: UART baud generators, ADC timing, bus clocks

An MCU card may carry `[models.peripherals.uart]` and `[models.peripherals.adc]`
tables, the printed limits the co-simulation fidelity checks compare firmware
settings against (baud error against the datasheet's receiver tolerance, ADC clock
and source-impedance windows, conversion rate). Every block names the `source` PDF
(https URL plus `source_sha256`) and a `basis` that cites the table, section and
printed page; so does every row inside it. A value the datasheet does not print is
left out, never filled in.

```toml
[models.peripherals.uart.baud]
formula = "BAUD = fOSC / (S * (UBRRn + 1))"
divisor_offset = 1.0     # baud = f_clk / (oversampling * (register / 2^fraction_bits + offset))
fraction_bits = 0
register_bits = 12
oversampling = [16, 8]
basis = "Table 20-1 (p.182) ..."

[[models.peripherals.adc.accuracy]]
resolution_bits = 10
adc_clock_hz = 200000.0
abs_error_lsb = 2.0
column = "typ"          # the printed column, so a typical figure is never read as a limit
condition = "VREF = 4 V, VCC = 4 V, ADC clock = 200 kHz"
basis = "Table 29-15 (p.319) ..."
```

A conversion states its duration one way, `total_cycles` or `cycles_after_sample`.
The block is valid on `mcu` cards only. Slave peripherals (sensors, memories) state
their bus clock ceilings as ordinary printed rows in `[models.params]`:
`max_sclk_hz` for an SPI clock and `i2c_max_hz` for the fastest I2C mode, each with
`conditions` and a `basis`. The formulas (baud divisor and error, conversion rate,
RC settling, the STM32 source-resistance equation) and the vocabulary (table names,
parameter names, conversion modes) are defined once in
`hauksbee_ir::peripheral_limits`, and a drift test fails if another crate spells
them. Cards filled so far: ATmega328P and ATmega1284/P, ATmega32U4, ATtiny85
(ADC only) and their module carriers, STM32F103C8 and RP2040. The tables are
validated by `peripheral_limits.rs` in `hauksbee-models`.

## Package variants and fuse hold current

A part number whose suffix names a smaller package must not inherit the larger
package's ratings. The 1N4148 DO-35 card takes only the tape and grade suffixes;
1N4148W (SOD-123), 1N4148WS (SOD-323), 1N4148WT (SOD-523) and LL4148 (MiniMELF)
carry their own, and a `1N4148W...` value none of them claims binds
`1n4148w_unlisted_package`, which assumes the smallest listed package's ratings and
says so in its bind warning.

A small-signal diode card's `max_current_a` is the printed average forward current
(IF(AV) or IO), the limit a steady load is held to, and `max_voltage_v` the DC
reverse voltage VR. The continuous-IF row and the repetitive-peak rows (IFRM,
VRRM) sit above them on these sheets, so they stay as untagged data rows rather
than becoming the rating: the 1N4148 DO-35 card is 150 mA and 75 V, not 300 mA
and 100 V.

A fuse or polyfuse (`passive_class = "fuse"`) is held to its hold current: the
card's `max_current_a`, else the current in its value (`6V 0.5A`, `PTC 750mA / 33V`,
`500mA`). DC stress reports a load past it as a trip, since the rail the part feeds
collapses.

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
| INA180       | opamp   | VS 6 V abs max; IN+/IN- 26 V | TI SBOS741H (Jul 2022), SOT-23-5 (DBV) only. Two pinouts chosen by the order code's first letter (INA180A* pinout A, INA180B* pinout B, Table 6-1); a bare `INA180` binds no card because it names neither. Inputs rated -0.2 to 26 V (7.3), 26 V abs max (7.1). Gain 100 V/V representative. |
| INA181       | opamp   | VS 6 V abs max; IN+/IN- 26 V | TI SBOS793H (Nov 2023). SOT-23-6 (DBV) and SC70-6 (DCK) have different pin maps (Table 5-1); there is no 5-pin package. The order code (`...IDBVR`/`...IDCKR`) or the footprint picks the map; a bare value with no footprint binds no card. Inputs rated -0.2 to 26 V (6.3), 26 V abs max (6.1). |
| INA185       | opamp   | VS 6 V abs max; IN+/IN- 26 V | TI SBOS378A (Nov 2023). SOT-563 (DRL) and SC70-6 (DCK), same selection rule as INA181 (Table 4-1). Inputs rated -0.2 to 26 V (5.3), 26 V abs max (5.1). |
| INA186       | opamp   | VS 6 V abs max; IN+/IN- 42 V | TI SBOS318B (Jul 2021). Bidirectional current-sense. SC70-6 (DCK, gain by A1-A5 suffix) and SOT-23-8 (DDF, adds ENABLE) have different pin maps (Table 5-1); the DSBGA (YFD) has no card. Inputs rated GND - 0.2 to 40 V (6.3), 42 V abs max (6.1). |
| INA2181      | opamp   | VS 6 V abs max; IN+/IN- 26 V | TI SBOS793H (Nov 2023). DUAL (two INA181 channels) in VSSOP-10 (DGS) / WSON-10 (DSQ), Table 5-2. Inputs as the INA181. |

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
  - `hauksbee-models` `tests/suite/power_fet_afe_resolve.rs`: resolves
    each new specific part (IPA045N10N3G, IRF9358, SIR182DP, bq76952,
    LM5107, LM5109, INA181, INA2181) by value and asserts kind + sane
    ratings; it also asserts that the generic power-FET fallback binds an
    unknown FET-in-DPAK by footprint, and that a specific value entry beats
    the catch-all when both match.
- **Live (manual)**: `hauksbee-models` `extract_bc846_live` is `#[ignore]`d
  and runs the configured live backend against `testdata/datasheets/BC846.pdf`.
  `HAUKSBEE_LLM_API_KEY` selects the API; otherwise it uses Codex. See
  `crates/hauksbee-models/README_DATASHEET.md`.

# Behavioural device models (power ICs)

The SPICE-level kinds (R/C/L, diode, BJT, MOSFET, switches, simple
regulators) cannot capture the *internal logic* of power ICs: a charger's
input-current limit servo, a PMIC's ship-mode pull to a rail, a balancer's
bleed FETs, a sequencer's state machine. For those, a model entry carries a
declarative `[models.behavioral]` block, parsed by
`crates/hauksbee-models/src/behavioral.rs` and realised at run time by
`crates/hauksbee-bind/src/behavioral.rs`.

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
     `vout_setpoint`, `efficiency`, optional `iout_limit_a`, and for a buck
     optional `max_duty` (the datasheet maximum duty cycle; the averaged
     switch node never exceeds `max_duty * vin`, so a low input drops out
     below it rather than passing through; a minimum off-time converts as
     `1 - t_off_min * f_sw`). Without it the ceiling is `vin`.
   - `out_via = "sense_filter"` (optional), for a controller with no output
     pin of its own whose `out_pin` is a sense input that reads the output
     through a filter resistor (the LTC4020's CSP, 100 ohms to the DC/DC
     output, Rev E p.12). The output lands on the rail at that resistor's
     far side. When every other fitted part on the sense pin's net is a
     capacitor or a two-terminal resistor, the rail is the far net of the
     resistor that leads neither to ground nor back to one of the part's own
     pins (a feedback divider). A net with any other part on it (test points
     and other one-pad parts aside), or a pin with no such resistor, is the
     rail itself. Resistors to two different
     nets leave the part open, since which rail it regulates cannot be
     told. Validation refuses `out_via` on an input, supply, ground, output
     (`out`/`vout`) or switch-node pin. The binder stamps the converter on
     that rail and the envelope check judges the regulator's range there,
     both from the one rule in `hauksbee_bind::sense_filter`.
   - Input lockout: below the card's `supply_range` minimum on `in_pin` the
     stage is off. It neither sources nor sinks, so a dead input releases
     the output to whatever else holds it, such as a battery behind a
     power-path FET, instead of pulling it to the input's 0 V. The
     converter's update reads the solved input against the floor, as it
     reads the enable, and turns the stage on or off for the next solve, so
     no solve sees it switch part-way. A card with no `supply_range` row on
     `in_pin` has no lockout.
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
   - `[models.behavioral.converter.feedback]` (`pin`, `vref_v`): regulate the
     board's feedback divider midpoint to `vref_v` instead of `vout_setpoint`.
   - Regulation loop: `crossover_hz` (optional) and `switching_hz`
     (optional). The runtime closes an averaged current-mode loop inside the
     circuit: a PI controller commands the current into `out_pin`, crossing
     over at `crossover_hz` against the capacitance to ground on the
     regulated node (the far side of the one inductor leaving `out_pin`, else
     `out_pin` itself), with its zero at `crossover_hz / 4` (critically
     damped). A load step `dI` therefore dips the rail by about
     `0.74 * dI / (2 pi crossover_hz C_out)` and recovers within a few
     `1 / (2 pi crossover_hz)`; the inductor current slews at
     `(vin - vout) / L` instead of stepping, and the converter never sinks
     output current (diode emulation: the conservative overshoot case).
     State the crossover the part has on this board: the value is taken
     as-is, so an internally compensated part whose crossover moves as
     `1 / C_out` is not tracked. Absent `crossover_hz`, the binder assumes
     `switching_hz / 10` (30 kHz when `switching_hz` is absent too, a
     300 kHz part) and discloses it as the
     `default-parameter:<REF>.crossover_hz` assumption, which qualifies the
     verdicts on the part's nets and on the rail beyond its output inductor:
     conventional designs span
     `f_sw / 20 .. f_sw / 5`, so an unstated deviation is uncertain by about
     2x either way. Both must be positive, and `crossover_hz` below
     `switching_hz / 2`.

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

### Strap tables: `[[models.resistor_bins]]`

Some parts read a resistor to ground once and map it to a *setting* through a
printed table, with no formula between the rows (a charger's float voltage and
input limit, an output-voltage select). A card states such a table as data:

```toml
[[models.resistor_bins]]
pin = "ilim_vset"          # a role in [models.pins]
tolerance = 0.05           # +/-5 % window around each printed nominal

[[models.resistor_bins.bin]]
ohms = 18000.0
params = { vbat_reg_v = 4.2, input_limit_a = 0.475 }

[[models.resistor_bins.bin]]
above_ohms = 180000.0      # an open-ended row
params = { input_limit_a = 0.475 }
note = "charge disabled"   # a mode the params do not express
```

At bind time the pin's resistance is read with the same nodal solve as
`current_program`, and the row it selects adds its `params` to the card's
for that board, so every consumer that reads a param reads the board's
setting. A resistance within `tolerance` of no nominal and inside no range
selects nothing: the settings stay unset and the bind row carries a
partial-model warning, as it does for an unreadable strap and for a row with
a `note`. Validation requires each row to give exactly one of `ohms`,
`above_ohms`, `below_ohms`, param names the kind reads and the card does not
already fix in `[models.params]`, and windows that cannot overlap. Datasheets
usually print only nominals, so the card's comment says where its tolerance
comes from.

## Adding a custom part without recompiling

A custom behavioural part is just a TOML file dropped into a user directory.
Which directory sets its priority, and how it was authored does not:

- `~/.hauksbee/models/` (layer 20), where datasheet extraction writes.
- `~/.config/hauksbee/models/` (layer 25), your own custom models. Beats the
  extraction directory, so a hand-corrected model of the same id wins.
- any `--models-dir <dir>` passed to `hauksbee run` (layer 30, highest of the
  three).

A user SPICE card (layer 40) still beats all of them.

`--models-dir` takes any of the three shapes a model directory actually has:
a directory of loose `*.toml` cards, one pack root (`pack.toml` beside a
`models/` folder), or a directory that holds packs (`*/pack.toml`), where every
pack under it loads at layer 30. A directory with none of those in it says so
on stderr rather than loading nothing quietly.

### A vendor SPICE model file

`--spice-models <file>` (repeatable, on `hauksbee run` and `hauksbee-ci run`)
loads a vendor's own `.model` / `.subckt` cards at layer 40, the top of the
ladder. A card claims the parts whose `Value` or MPN **is the card's own
name**, matched exactly and case-insensitively: `.model 2SD1664R NPN(...)`
binds every part valued `2SD1664R` and nothing else, with no match rules to
write. Cards hauksbee cannot execute (a `.subckt`, or a `.model` type outside
`D` / `NPN` / `PNP` / `NMOS` / `PMOS` / `R` / `C` / `L`) leave the part
unresolved rather than binding a guess, and a file with no card, or that
cannot be read, is an error naming the path.

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
(`crates/hauksbee-bind/src/behavioral.rs`) in Rust and register it before
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

The LTC4020 fixture demonstrates the expected output: base kind `vreg`, exact
pin map, `topology = "buck_boost"`, `out_pin`/`in_pin`, `vout_setpoint = 28.8`
for an 8S LiFePO4 pack, `efficiency = 0.92`, and a populated `iin_program`
block. Unknown transfer-function constants remain zero instead of being fitted
to a desired result. The captured draft names BAT as the output; the checked-in
card lands it on the rail CSP senses (`out_pin = "csp"`,
`out_via = "sense_filter"`), the DC/DC output of the datasheet's PowerPath
diagram, with the battery behind the FET. The checked-in model sources its
ILIMIT transfer directly from the primary datasheet (`VILIMIT = 50 uA * RILIMIT`, effective over 0--1 V,
scaling the 50 mV input-sense threshold). Offline coverage lives in
`crates/hauksbee-models/tests/suite/codex_behavioral_fixture.rs`; the live
backend check is `extract_ltc4020_charger_live` and is ignored by default.
