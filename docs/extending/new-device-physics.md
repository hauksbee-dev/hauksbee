# New device physics: the six-touchpoint checklist (VCVS worked example)

**Goal.** Add a new element to the solver, a new `Device` variant with its
own stamp, without shipping a missed integration site. This is the one
extension that is core Rust, deliberately: zero-dispatch stamping is a
performance pillar, so devices are enum variants, not trait objects. What
makes it safe is a checklist in which every step is enforced by a compile
error or a test, so forgetting one is impossible to ship, not merely
inadvisable.

The worked example is the VCVS/VCCS pair (SPICE `E`/`G` cards), which landed
as commits `82a818f` (IR + loader + terminal classification) and `ff00d94`
(stamps through every integration site). Read those two commits alongside
this doc; they are the checklist executed once, with design notes.

**Prerequisite reading:** `docs/dev-plans/04-spice-compat.md` §1, the hazard
table this checklist descends from. The one-sentence summary: several
consumers of the `Device` enum are silently wrong, not loudly broken, when
a new variant slips past them, and the worst failure is a plausible wrong
waveform.

## The six touchpoints

| # | Touchpoint | Where | Failure if missed | Enforced by |
|---|---|---|---|---|
| 1 | The enum variant + `examples()` | `crates/hauksbee-ir/src/lib.rs` |, | `strum::EnumCount` assert in `Device::examples` |
| 2 | `nodes()` / `map_nodes()` | same file | device dropped from every partitioned sub-circuit | exhaustive match, no `_` arm |
| 3 | `is_linear()` / `is_event_driven()` | same file | island misclassified; nonlinear device taints the fast path | doc-comment justification required; review |
| 4 | `conduction_nodes()` / `sense_nodes()` | same file | tear engine reasons wrongly about the cut | the zero-row cross-check test |
| 5 | The stamp + `reserve_pattern` | `crates/hauksbee-solve/src/stamp.rs` (`stamp_all`, `reserve_pattern`) | device contributes nothing, or `add_at` lands outside the frozen pattern | exhaustive matches + the deck gate |
| 6 | `LinearIsland::compile` | `crates/hauksbee-solve/src/linear.rs` | **silent drop from the A matrix**: a plausible wrong waveform | exhaustive match: model it or return `None` |

Plus, when the device owns a branch-current unknown (anything that fixes a
voltage): the unknown layout in `crates/hauksbee-solve/src/system.rs`, and
when it references *another device* (F/H control sources, K couplings):
`controlling_sources` / `retarget_controlling_source`.

## Step 1; the IR variant, with its physics in the doc comment

```rust
/// Voltage-controlled voltage source (SPICE `E` card):
/// `V(p,n) = gain * V(cp,cn)`. Owns a branch-current unknown exactly like
/// an ideal [`Device::Vsource`]; the control pair is read-only.
Vcvs { name: String, p: NodeId, n: NodeId, cp: NodeId, cn: NodeId, gain: f64 },
```

The doc comment is not decoration: the `is_linear` answer, the branch-unknown
decision, and the terminal roles must each carry a one-line justification the
next reader can audit.

## Step 2, extend `Device::examples()` (the enforcement anchor)

`Device::examples()` returns one representative instance per variant, and ends
with:

```rust
assert_eq!(out.len(), <Device as strum::EnumCount>::COUNT,
    "Device::examples() must ship exactly one instance per variant");
```

Adding a variant bumps the derived count, so *every test that calls
`examples()`* panics until your example ships, and once it ships, your
variant is automatically subjected to the serde round-trip
(`crates/hauksbee-ir/tests/serde_roundtrip.rs`), the node-walk coverage
checks, and the stamp/sense cross-check below. This is why the checklist
cannot be skipped by forgetfulness: step 1 makes step 2 mandatory, and step 2
drags your variant into every enforcement test.

> **Why a count assert and not just exhaustive matches?** An OR-arm
> (`Device::Vcvs { .. } | Device::Vccs { .. } => …`) satisfies a match
> without anyone writing a real instance of the new variant. The length check
> cannot be satisfied that way (the comment above the assert in `lib.rs` says
> exactly this). Matches force *handling*; the inventory forces *testing*.

If your device references another device by `DeviceId` (like F/H's
`ctrl_src`), follow the documented `examples()` convention: point it at
`DeviceId(0)` and let consumers install the control device at index 0 first.

## Step 3, `nodes()` and `map_nodes()`

`map_nodes` is *the one* node walk over a `Device`, partitioner sub-circuit
extraction and tear-engine island building all route through it. Both matches
are exhaustive **with no `_` arm on purpose**; your variant fails to compile
until wired in. For the VCVS all four terminals remap:

```rust
Device::Vcvs { p, n, cp, cn, .. } | Device::Vccs { p, n, cp, cn, .. } => {
    *p = f(*p); *n = f(*n); *cp = f(*cp); *cn = f(*cn);
}
```

**Trap, `DeviceId` references do not remap here.** `map_nodes` rewrites
*nodes*. A device that carries a `DeviceId` (F/H `ctrl_src`, B-source
`I(vname)` deps, K-coupling windings) must have that reference retargeted by
extraction passes via `Device::retarget_controlling_source`, otherwise the id
silently points at whatever occupies that index in the new sub-circuit. The
comments at the `Cccs`/`Ccvs` arms of `map_nodes` are the canonical statement
of this hazard.

## Step 4, conduction/sense terminal classification

Every node the device touches goes in exactly one of `conduction_nodes()`
(its KCL row receives current from this device) or `sense_nodes()` (read-only:
Jacobian columns of other rows, its own row untouched). The VCVS is the
canonical case the classifier was built for: output `p/n` conduct, control
`cp/cn` sense.

This is not paperwork, a *sense* terminal is what makes a free tear exact
(cutting the wire and replaying its voltage changes nothing, because no
current ever crossed it; `docs/dev-plans/02-tearing-architecture.md` §1). A
terminal declared sense whose stamp actually leaks current would break torn
solves silently. So the claim is enforced mechanically:

**The cross-check test**: `declared_sense_rows_receive_no_current` in
`crates/hauksbee-solve/src/decompose/conduction.rs`, iterates
`Device::examples()`, stamps each device at four probing operating points, and
asserts (a) conduction ∪ sense covers every terminal exactly once and (b)
every declared sense row receives *nothing*, matrix entries and RHS both.
Your variant enters this test automatically the moment `examples()` compiles.

If what your device "senses" is a branch current, not a node voltage (F/H, K),
declare it via `controlling_sources` and leave `sense_nodes` empty; the
comments at the `Cccs` arm explain why declaring the control source's nodes
would be a lie the cross-check would vacuously bless.

## Step 5; the stamp, the pattern, and the unknown layout

- **VCCS (G):** four transconductance entries, `+gm` at `(p,cp)`/`(n,cn)`,
  `−gm` at `(p,cn)`/`(n,cp)`, no RHS, no new unknown.
- **VCVS (E):** fixes its output-port voltage, so like an ideal Vsource it
  cannot be a conductance: it gets an appended branch-current unknown
  (`system.rs` branch allocation must count it), and its constraint row reads
  `v_p − v_n − gain·(v_cp − v_cn) = 0` with the control terms confined to the
  branch *row*. The control pair's own KCL rows stay empty, which is
  precisely the sense claim from step 4, now visible in the matrix.

Whatever you stamp, `reserve_pattern` must pre-touch every slot: the sparsity
pattern is frozen before solving, and an `add_at` outside it panics (the good
outcome) or corrupts a neighbor (the bad one). The VCVS reserves its branch
incidence plus the `(branch, cp)`/`(branch, cn)` control columns.

## Step 6, `LinearIsland::compile`: the most dangerous site

If your device is `is_linear() == true`, an island containing it still reaches
the state-space reducer under `Partitioning::Auto`, and the reducer models
only R/C/L/I. Its device walk used to end in `_ => {}`, which for a VCVS
would have compiled the island *with the controlled source silently missing
from the A matrix*: no crash, no error, a plausible wrong waveform. This is
the failure plan 04 §1 rates the worst of the six.

The walk is now an exhaustive match. Your options, in order of preference:

1. **Model the device in the state-space assembly** (only if you also build
   the exactness gate proving fast-path == monolithic for it), or
2. **Return `None`** for islands containing it, routing them to the MNA
   sub-solve, which stamps everything in full. Exact, merely slower. This is
   what E/G do (`ff00d94`'s design note).

"Linear" and "state-space-reducible" are different properties; the
`is_linear` doc comment for `Vcvs`/`Vccs` states this distinction so nobody
"optimizes" the `None` away without building the gate.

## Step 7; the loader, refusing what you don't implement

The E-card loader parses the plain linear card only. `POLY`/`VALUE`/`TABLE`
forms refuse with a line number rather than interning `poly` as a node name;
degenerate topologies that make the constraint row singular (a shorted VCVS
output port; the unity-gain self-sensing form) refuse at load, naming the
device, instead of dying later at a zero pivot. Follow the same discipline:
**everything your stamp does not model must refuse loudly at parse time**,
the honesty rule that separates "unsupported" from "silently wrong".

## Step 8; the tests that prove it

Three layers, all of which must be green:

1. **The enforcement suite** (free once `examples()` is extended):

   ```
   cargo test -p hauksbee-ir
   cargo test -p hauksbee-solve declared_sense_rows_receive_no_current
   ```

   Green: `test decompose::conduction::tests::declared_sense_rows_receive_no_current ... ok`
   plus the serde round-trip in `hauksbee-ir`.

2. **A monolithic + partitioned solve** of a small circuit containing the
   device (`crates/hauksbee-solve/tests/features.rs` has the E/G cases),
   this is what catches a `LinearIsland` mishandling: force
   `Partitioning::Auto` on a device-in-RC-island fixture and assert the
   result matches the monolithic solve.

3. **The ngspice deck gate**: a `.cir` deck plus a companion
   `<name>.expect.toml` in `crates/hauksbee-solve/tests/decks/`. The VCVS's
   is `vcvs_gain.cir` (a gain-4 block driving an RC low-pass) with
   `vcvs_gain.expect.toml`:

   ```toml
   analysis = "tran"
   description = "VCVS gain-4 block driving an RC low-pass (1 kHz sine)"
   full_scale = 2.0

   [[probe]]
   expr = "V(out)"
   reltol = 0.01
   ```

   The harness (`crates/hauksbee-solve/tests/ngspice.rs`) runs ngspice and
   hauksbee over the *same* deck and holds every probe to its declared
   tolerance. Design the deck to exercise what is novel: `vcvs_gain.cir`
   pins the branch-current unknown *and* the ideal (zero-current) control
   port, because those are the two things a VCVS stamp can get wrong.

   ```
   cargo test -p hauksbee-solve --test ngspice
   ```

   Green looks like a per-deck pass line with the worst relative error
   printed. (The harness needs `ngspice` on PATH; without it the comparison
   cannot run, install it rather than trusting the other two layers alone.)

## The checklist, as you'd actually run it

```
[ ] IR variant with justified doc comment          (compile forces the rest)
[ ] Device::examples() entry                       (count assert fails until done)
[ ] nodes() / map_nodes() arms                     (no _ arm; compile error until done)
[ ] is_linear / is_event_driven + one-line why
[ ] conduction_nodes / sense_nodes declared        (cross-check test verifies the claim)
[ ] controlling_sources + retarget, if DeviceId refs
[ ] stamp_all + reserve_pattern                    (every slot pre-touched)
[ ] system.rs unknown layout, if branch unknown
[ ] LinearIsland::compile: model it or return None (exhaustive match)
[ ] loader card; unsupported forms refuse with line numbers
[ ] deck + expect.toml in tests/decks/
[ ] cargo test -p hauksbee-ir -p hauksbee-solve green, ngspice deck green
```

---

Background: `docs/dev-plans/04-spice-compat.md` (the whole plan, §1
especially), `docs/dev-plans/02-tearing-architecture.md` for why
conduction/sense exists. For extensions that *don't* need core physics, start
at [README.md](README.md), most parts are data, not devices.
