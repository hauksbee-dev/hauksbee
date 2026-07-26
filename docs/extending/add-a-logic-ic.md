# Add a logic IC: the 74HC74, as data

**Goal.** Add a digital IC, gates, flip-flops, shift registers, tri-state
buffers, as a `[models.logic]` block in a model TOML entry. No Rust: the
engine's generic evaluator (`LogicComponent` in
`crates/hauksbee-engine/src/logic.rs`) compiles the block at bind time. The
worked example is the 74HC74 dual D flip-flop, which ships in
`crates/hauksbee-models/db/digital.toml` with a datasheet-cited test in
`crates/hauksbee-engine/tests/logic_gates_74hc.rs`.

**What you need:** the part's datasheet function table, and
`hauksbee models lint`.

## How the model works

A logic spec declares input/output **pins**, **combinational expressions**
(output = boolean expression over pins), clocked **registers** (the sequential
state), and **tri-state** groups. The schema and validator live in
`crates/hauksbee-models/src/logic_spec.rs`; the module doc there is the
grammar reference. There is no substring classification and no per-part Rust,
the model entry itself declaring `[models.logic]` is what makes the part
digital-behavioral.

## Step 1; the model entry around the logic block

A logic IC is a normal `[[models]]` db entry; the logic block rides inside it.
The shipped 74HC74 entry, abridged:

```toml
[[models]]
id = "74hc74"
kind = "digital"
description = "74HC74 dual D-type flip-flop with preset and clear"

[models.match]
value_re = "(?i)^(SN)?74HC74"       # matches the component's Value field

[models.params]
# Electrical envelope: TI SN74HC74 datasheet, electrical characteristics
# at Vcc = 4.5 V.
voh       = 4.4
vol       = 0.1
vih       = 3.15
vil       = 1.35
tpd_s     = 1.6e-8
supply_pin = "14"
gnd_pin    = "7"

[models.pins]
"1"  = "clr_n1"
"2"  = "d1"
"3"  = "clk1"
# ... every pin, mapping pad number -> role name
"14" = "vcc"
```

The `[models.pins]` role names are the vocabulary the logic block speaks,
every pin the logic references must be mapped here. Cite the datasheet next to
the electrical numbers; the db convention is that every constant carries its
source.

## Step 2; the `[models.logic]` block

The 74HC74 is two independent D flip-flops, each with asynchronous preset and
clear. One 1-bit register per flop:

```toml
[models.logic]
# Behaviour source: TI SN74HC74 datasheet, function table.
inputs  = ["d1", "clk1", "pre_n1", "clr_n1", "d2", "clk2", "pre_n2", "clr_n2"]
outputs = ["q1", "q_n1", "q2", "q_n2"]

[[models.logic.register]]
name = "ff1"
bits = 1
clock = { pin = "clk1", edge = "rising" }
reset = [
  { pin = "clr_n1", active = "low", value = 0 },
  { pin = "pre_n1", active = "low", value = 1 },
]
op = "load"
data_in = "d1"

# ff2: identical shape on the *2 pins

[models.logic.comb]
"q1"   = "ff1[0]"
"q_n1" = "!ff1[0]"
"q2"   = "ff2[0]"
"q_n2" = "!ff2[0]"
```

The pieces, and where each is specified:

- **Expressions** are boolean: identifiers (digit-led names like `1a` are
  legal, real 74HC02 pins), `0`/`1`, `!`, `&`, `^`, `|`, parentheses, and
  `register[bit]`. Precedence tightest-first: `!`, `&`, `^`, `|`.
- **Registers** hold up to 64 bits. Per step, priority is: active `reset`
  (asynchronous, dominant; the 74HC595's SRCLR row), then active `load`
  (async parallel load, the 74HC165's PL), then a qualifying clock edge
  applies `op` (`shift_left`, `shift_right`, `load`, `count_up`,
  `count_down`); otherwise hold. All registers commit simultaneously, so a
  register loading from another register captures the *pre-step* value,
  tied-clock silicon behaves the same way.
- **Multiple resets**: the 74HC74 declares CLR and PRE as two entries. When
  both are active the *first declared* wins. The silicon's both-asserted race
  (Q = Q̄ = HIGH, unstable) is not representable in a 1-bit register and is
  **deliberately not faked**: the db entry's comment says so, and yours
  should too when you make the same call.
- **Tri-state**: `[models.logic.tristate]` maps an output (or an `a..b` range
  over the outputs declaration order) to an enable pin + polarity. See the
  74HC125 entry for per-gate independent enables.
- **Latches**: cross-coupled `comb` expressions are legal (the SR latch); give
  cycle participants a power-on level via `[models.logic.init]`.

> **Why `outputs` order is load-bearing.** Outputs forming a combinational
> cycle are resolved by fixpoint iteration *in outputs-declaration order*, and
> convergence is verified **exhaustively at compile time**: the evaluator
> enumerates the cycle's input space when the block binds, so a spec that
> could oscillate fails at bind, not mid-simulation. Reorder `outputs` and you
> have changed the declared resolution order, which is semantics, not style.
> (`logic_spec.rs` module docs, "Register semantics".)

## Step 3, lint it

Put your entry in its own file (or a pack; see
[make-a-model-pack.md](make-a-model-pack.md)) and run:

```
cargo run -p hauksbee-engine --bin hauksbee -- models lint my_part.toml
```

Green looks like:

```
model '74hc74': ok
1 item(s) checked, 0 finding(s) — clean
```

Lint compiles the logic block through the **same** `LogicComponent::compile`
path board binding uses, schema validation, expression lowering, and the
exhaustive convergence check, so "lint said ok" and "the board binds it"
cannot disagree. Every failure category is a named `LogicSpecError`:
undeclared names, unassigned outputs, width mismatches, bit indexes out of
range, dead registers (no clock, no load, no reset), tri-state without a
declared enable, and more.

**Trap, a clock pin cannot also be combinational.** An expression reading a
register's clock pin is rejected (`ClockAlsoComb`): edge semantics would be
ambiguous (does the output see the level before or after the edge?). Route the
level through a register or a separate pin. You will hit this on parts whose
datasheet draws the clock into gating logic; the model has to make the
ordering explicit instead.

## Step 4, resolve it against a board

Drop the file in `~/.config/hauksbee/models/` (the standing user dir,
priority 20) or pass `--models-dir` (priority 30), then check the binder sees
it and which layer won:

```
hauksbee models resolve my_board.kicad_pcb --models-dir ./my-models
```

The report prints, per component, the winning model id, layer, and origin
file. Higher layers beat builtins outright; specificity only breaks ties
within a layer.

## Step 5; the test that proves it

The proving pattern is the **datasheet function table, row by row**, driven
through the same builtin/library entry a board bind resolves (so the test pins
the shipped data, not a copy). The shipped 74HC74 test
(`hc74_dff_function_table` in
`crates/hauksbee-engine/tests/logic_gates_74hc.rs`) walks every row: async
preset dominates the clock, async clear, rising edge captures D, D changes at
CLK high/low are ignored, falling edge holds, and clocking FF2 leaves FF1
undisturbed.

```
cargo test -p hauksbee-engine --test logic_gates_74hc
```

Green looks like:

```
test hc74_dff_function_table ... ok
test hc00_nand_function_table ... ok
...
test result: ok. 9 passed; 0 failed
```

For your own part: compile via `LogicComponent::compile`, `tick` with a
pin→level map per function-table row, and assert `output_level` (and
`output_enabled` for tri-state) per row, citing the datasheet table in the
test's comment. If a row can't be expressed, that is a finding about the
schema, record it, don't approximate silently.

One honest caveat: this closing proof pattern is a Rust test, so it needs a
hauksbee **checkout** to run in. The data-only promise holds for *using* the
part, writing the entry, `hauksbee models lint`, `models resolve`, and the
co-sim attaching it at runtime need no checkout, but pinning it with a
function-table test the way the shipped parts are pinned does.

## The honest boundary

Not everything digital is a boolean-comb data entry, on purpose:

- **Muxes, the 74HC138 decoder, the 74HC245 transceiver** are deferred, their
  select/direction semantics are not boolean combinational logic (a design
  decision recorded in `docs/dev-plans/06-extensibility-sdk.md` §8.2).
- **MCU-facing chain controllers** (`Hc595Chain`/`Hc165Chain`) and the
  binder's 74HC02 cross-couple fusion stay Rust, documented in
  `crates/hauksbee-engine/src/digital.rs`, net-level feedback cannot settle
  at chunk granularity, so it is fused at bind instead.

---

Next: [add-a-sensor.md](add-a-sensor.md) for bus peripherals, or
[make-a-model-pack.md](make-a-model-pack.md) to ship your entries.
