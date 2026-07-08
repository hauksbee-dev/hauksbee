# SPICE compatibility statement

*What "drop-in for the common cases" means, precisely — and how the promise is kept honest.*

Hauksbee reads a `.cir` netlist through one loader (`crates/hauksbee-ir/src/spice.rs`)
and simulates a **documented, enforced subset** of SPICE. The promise is narrow and
testable:

> **Drop-in for the documented subset; a loud, line-numbered refusal outside it.**

Two mechanisms back that up:

1. **Fidelity.** Every supported card is cross-checked against ngspice on a corpus of
   decks with per-quantity tolerances, in CI. The living results table is
   [`results.md`](results.md) (currently 33/33 decks passing against ngspice-46).
2. **No drift.** The "Supported" and "Refused" tables below are **generated from, and
   checked against, the loader itself** by `crates/hauksbee-ir/tests/compat_drift.rs`.
   That test loads a minimal snippet for every card the doc claims supported (it must
   parse) and for every card the doc claims refused (it must produce the documented
   error). The doc cannot claim a capability the loader lacks, or hide one it has,
   without turning the test red. The tables between the `GENERATED` markers are not
   hand-written; regenerate them with
   `UPDATE_COMPAT=1 cargo test -p hauksbee-ir --test compat_drift`.

Refusals are always line-numbered `SpiceError`s (`Syntax`, `UnknownElement`,
`MissingModel`, `BadNumber`) carrying the offending text. There is no silent misparse
of a *recognized* card: a card the loader understands is either honored or refused with
a reason. (The one honesty gap — a class of *directives* that is silently ignored
rather than refused — is called out in §4.)

---

## 1. Supported cards, exhaustively

Every row below is proven by a snippet that parses through the loader on every
`cargo test`. Node `0`/`gnd` is ground; SI suffixes (`k meg u n p f m g t mil`) apply to
bare values; `+` continues a line; `*` begins a comment; the first line of the deck is a
title and is ignored.

<!-- BEGIN GENERATED: supported (source: crates/hauksbee-ir/tests/compat_drift.rs) -->
<!-- Do not hand-edit between these markers: regenerate with
     UPDATE_COMPAT=1 cargo test -p hauksbee-ir --test compat_drift -->

### Element cards

| Card | What it does |
|------|--------------|
| `R` resistor | `Rxxx a b value [tc1=]` — linear resistor, optional linear temp-coefficient. |
| `C` capacitor | `Cxxx a b value [ic=]` — capacitor, optional initial voltage (honored under `uic`). |
| `L` inductor | `Lxxx a b value [ic=]` — inductor, optional initial current. |
| `V` voltage source | `Vxxx p n <dc|sin|pulse|pwl> [AC mag phase]` — independent voltage source. |
| `I` current source | `Ixxx p n <dc|sin|pulse|pwl> [AC mag phase]` — independent current source. |
| `D` diode | `Dxxx a k model` — Shockley diode with junction cap / transit time / breakdown from its `.model` (defaults if the model is missing). |
| `Q` BJT | `Qxxx c b e model` — Gummel-Poon BJT with charge storage (cje/cjc/tf/tr) and series rb/re/rc. |
| `M` MOSFET | `Mxxx d g s b model [L= W=]` — LEVEL-1 MOSFET (see caveats) with gate charge and body diode. |
| `S` voltage switch | `Sxxx a b nc+ nc- model` — voltage-controlled switch (`.model SW/VSWITCH`, defaults if absent). |
| `E` VCVS | `Exxx n+ n- nc+ nc- gain` — linear voltage-controlled voltage source. |
| `G` VCCS | `Gxxx n+ n- nc+ nc- gm` — linear voltage-controlled current source. |
| `F` CCCS | `Fxxx n+ n- vname gain` — current-controlled current source (controlled by a named V-source's branch current). |
| `H` CCVS | `Hxxx n+ n- vname transres` — current-controlled voltage source. |
| `B` behavioral source | `Bxxx n+ n- V={expr}` or `I={expr}` over `v()/i()/time/param` (evalexpr subset). |
| `K` coupled inductors | `Kxxx L1 L2 k` — lossless mutual coupling, `0 < k <= 1` (k=1 legal). |
| `X` subcircuit call | `Xxxx nodes... NAME [p=v]` — instantiates a `.subckt`, flattened at load with mangled internal names. |

### `.model` types

| Card | What it does |
|------|--------------|
| `.model ... D` | Diode model: `is n rs cjo vj m tt bv xti eg` (aliases `cj0`, `pb`). |
| `.model ... NPN/PNP` | BJT model: `is bf br vaf var nf nr rb re rc cje cjc tf tr xti eg`. |
| `.model ... NMOS/PMOS` | MOSFET model, LEVEL=1 only: `vto kp lambda gamma phi tox cgso cgdo is cbd cbs pb mj`. |
| `.model ... SW/VSWITCH` | Voltage-switch model: `vt vh ron roff`. |

### Analyses

| Card | What it does |
|------|--------------|
| `.op` | DC operating point (also the default when no analysis card is present). |
| `.tran` | `.tran tstep tstop [tstart] [tmax] [uic]` — transient analysis. |
| `.dc` | `.dc src start stop step [src2 ...]` — DC sweep of a V/I source, optional nested second sweep. |
| `.ac` | `.ac <dec|oct|lin> n fstart fstop` — small-signal AC sweep (needs an `AC` source stimulus). |

### Directives

| Card | What it does |
|------|--------------|
| `.print` / `.plot` | `.print ANALYSIS var...` selects outputs (`V(a)`, `V(a,b)`, `I(V1)`); `.plot` is treated as `.print`. |
| `.ic` (with `uic`) | `.ic V(node)=val` seeds transient node voltages; requires `uic` on `.tran`. |
| `.nodeset` | `.nodeset V(node)=val` — DC Newton start guess (never pinned/enforced). |
| `.param` | `.param name=expr` — named parameters, order-independent topological resolve. |
| `.include` / `.inc` | `.include <file>` splices another file inline before every other pass. |
| `.lib <file> <section>` | `.lib <file> <section>` splices one named `.lib/.endl` section (bare one-arg form is refused). |
| `.options` / `.option` | `.options reltol= abstol= vntol=` — solver tolerance overrides (other keys ignored). |
| `.temp` | `.temp <celsius>` — one global circuit temperature. |
| `.subckt` / `.ends` | `.subckt NAME ports [p=v]` ... `.ends` — subcircuit definition (nestable calls, per-instance params). |

### Source functions

| Card | What it does |
|------|--------------|
| `DC` | `DC value` (or a bare value) — constant source level. |
| `SIN` | `SIN(offset amp freq [delay theta phase])` — damped sinusoid. |
| `PULSE` | `PULSE(v1 v2 delay rise fall width period)` — pulse train. |
| `PWL` | `PWL(t1 v1 t2 v2 ...)` — piecewise-linear waveform. |
| `AC` stimulus | `AC [mag] [phase]` on a source card — the small-signal drive for `.ac` (bare `AC` = mag 1, phase 0). |

### Expressions

| Card | What it does |
|------|--------------|
| `{expr}` values | Curly-brace arithmetic over `.param` names anywhere a numeric value is taken (evalexpr, bare f64s). |
<!-- END GENERATED: supported -->

---

## 2. Supported, with the caveat

These cards work, but not identically to a full SPICE3 / ngspice front end. Each caveat
is deliberate and, where it affects a waveform, is quantified in [`results.md`](results.md).

- **MOSFETs are LEVEL-1 only — a switch model, not an analog model.** The DC channel is
  the Shichman-Hodges square law; `LEVEL=2/3/BSIM` cards are **refused** (§3), not
  silently downgraded. The implemented physics targets board-shaped switching, not
  analog precision:
  - **Gate charge** uses Meyer's *region-limit* charges (Cgs falls Cox→Cox/2 across
    threshold; Cgd rises c_ov→c_ov+Cox/2 entering triode). This keeps switching edges
    within **≈2 ns of ngspice** on the switch decks (`mos_load_switch`, `pmos_load_switch`),
    rather than matching Meyer's full two-voltage capacitances (which do not conserve
    charge).
  - **Subthreshold** current below `vth` is a smooth tail — a *documented deviation*:
    ngspice's LEVEL-1 has exactly zero current there.
  - **Gate oxide capacitance** is zero when the model omits `TOX` (ngspice materializes a
    default `TOX`/`W`/`L`); state `TOX` on the card to get intrinsic gate charge.
  - **Analog accuracy (gain, subthreshold slope, short-channel effects) is a known gap.**
    Use a switch, load switch, or synchronous rectifier deck; do not expect an amplifier
    small-signal match.
- **Coupled inductors `K` model lossless linear mutual coupling only.** `k=1` (a perfect
  transformer) is legal and solved without inverting the singular L-matrix. **Saturating
  cores are unsupported** — no core (BH) model card parses. Transformer/flyback decks are
  the payoff (`xfmr_1to2`, `xfmr_k1`, `flyback_diode`); a negative `k` is refused (swap a
  winding's terminals instead).
- **BJT charge storage uses SPICE-default junction grading.** `cje/cjc/tf/tr` and series
  `rb/re/rc` are honored, but per-junction `VJE/VJC`/`MJE/MJC` overrides are not parsed —
  the values default to `0.75`/`0.33` (what an ngspice card without them also gets).
- **Diode model is optional and defaults silently.** A `Dxxx a k model` whose model is
  undefined — or whose `.model` is not a diode — falls back to the built-in default diode
  parameters rather than erroring. This avoids a diode silently inheriting BJT params, but
  it also means a typo'd model name is not caught. (`Q`/`M` do error on a missing model;
  see §3 and the follow-up note in §4.)
- **Behavioral `B` sources use a fixed expression subset.** `V={expr}`/`I={expr}` over
  `v(node)`, `v(a,b)`, `i(vsource)`, `time`, and `.param` values, with the function set
  `ln log10 log2 exp pow sqrt cbrt abs sin cos tan asin acos atan atan2 sinh cosh tanh
  asinh acosh atanh hypot min max if floor round ceil` (and `**` for exponentiation).
  **No `POLY`, `TABLE`, or `VALUE` forms; no bare `log`** (write `ln`/`log10`). The
  expression must be brace-wrapped. B-source decks arm a damped (Armijo) Newton path and
  **refuse loudly on non-convergence** rather than emit a bad waveform.
- **`.plot` is treated as `.print`.** Output selection is honored; no ASCII plot is drawn.
- **Single global temperature.** `.temp` sets one circuit temperature; there is no
  per-device `TEMP`/`DTEMP`.
- **`.ic` requires `uic`.** Initial conditions seed the power-on (`uic`) transient start;
  pinning nodes *during* the DC operating-point solve is not implemented, so `.ic` without
  `uic` is refused (§3) rather than silently downgraded.
- **`.nodeset` is a guess, never enforced.** It influences which root Newton finds; the
  converged voltage may differ from the seed.
- **`.lib` requires an explicit section.** The bare one-argument `.lib <file>` form is
  ambiguous and refused; use `.include <file>` or `.lib <file> <section>`.

---

## 3. Refused, loudly

Everything the loader recognizes but does not implement refuses with a line-numbered
error. The fragment column is a substring of the exact message the user sees, and is
asserted by the drift test.

<!-- BEGIN GENERATED: refused (source: crates/hauksbee-ir/tests/compat_drift.rs) -->
<!-- Do not hand-edit between these markers: regenerate with
     UPDATE_COMPAT=1 cargo test -p hauksbee-ir --test compat_drift -->

| Card / form | Why it refuses | Error fragment (substring of the exact message) |
|-------------|----------------|--------------------------------------------------|
| `T` transmission line | Transmission lines were cut (dev-plan step 15); the letter is unknown. | `unknown element type `T`` |
| `J` JFET | JFETs are unsupported; the element letter is unrecognized. | `unknown element type `J`` |
| `Z` IGBT / MESFET | `Z` devices are unsupported. | `unknown element type `Z`` |
| `O` lossy line | Lossy transmission lines (`O`/LTRA) are unsupported. | `unknown element type `O`` |
| `U` uniform-RC line | URC lines are unsupported. | `unknown element type `U`` |
| `.model ... NMOS/PMOS LEVEL!=1` | Only LEVEL-1 MOSFETs are implemented; other levels refuse rather than silently stamp level 1. | `MOSFET LEVEL=3 is not implemented` |
| `E`/`G` POLY/VALUE/TABLE | Only the linear `n+ n- nc+ nc- gain` controlled-source form is supported. | `controlled-source form is unsupported` |
| `F`/`H` POLY | Only the linear `n+ n- vname gain` current-controlled form is supported. | ``POLY` controlled-source form is unsupported` |
| `B` POLY/TABLE/VALUE | Only `V={expr}`/`I={expr}` behavioral forms are supported (no POLY/TABLE/VALUE). | `B-source form is unsupported` |
| `B` unsupported function | Behavioral expressions accept only a fixed math/function subset. | `unsupported function `gamma` |
| `B` ambiguous `log` | `log` is refused as ambiguous across dialects; write `ln` or `log10`. | ``log` is ambiguous` |
| engineering suffix in `B={}` | Inside a behavioral `{}` expression the text is pure arithmetic over bare f64s; a suffix (`2k`) refuses rather than silently dropping the operator. | `engineering suffix inside a braced expression` |
| bare `.lib <file>` | The one-argument `.lib` form is ambiguous; use `.include` or `.lib <file> <section>`. | `is ambiguous` |
| `.ic` without `uic` | `.ic` is only honored on the power-on (`uic`) path; DC pinning is not implemented. | ``.ic` requires `uic`` |
| `F`/`H` non-source control | The controlling reference must be an independent V source (branch-current read). | `is not an independent voltage source` |
| `K` non-inductor referent | A K card must couple two `L` elements. | `not an inductor` |
| `.dc` on a non-source | `.dc` can only sweep an independent V or I source. | `can only sweep an independent V or I source` |
| degenerate VCVS | A VCVS shorting its own output port (or unity self-sense) is singular and refuses by name. | `shorts its own output port` |
| undefined subckt | An `X` call to a subcircuit that was never defined refuses with the name. | `undefined subckt` |
| missing BJT/MOS `.model` | A `Q`/`M` referencing an undefined model is refused (unlike a diode, which defaults). | `references undefined .model` |
| unknown `.ac` sweep type | `.ac` accepts only `dec`, `oct`, or `lin`. | `unknown `.ac` sweep type` |
| `.param` dependency cycle | Parameters that reference each other circularly are refused. | `dependency cycle` |
<!-- END GENERATED: refused -->

Also refused as `unknown element type` (the loader has no card for them): any element
letter outside `R C L V I D Q M S E G F H K B` and the `X` subckt call — including `W`,
`A`, `N`, `P`, `Y`, etc.

---

## 4. Behavioral differences from ngspice

Things a user migrating a deck must know, beyond the per-card caveats above:

- **Default tolerances.** `reltol=1e-3`, plus `abstol`/`vntol` floors, overridable via
  `.options reltol= abstol= vntol=`. Other `.options` keys are accepted but ignored.
- **Integration.** Transient uses the solver's companion-model integration (BE / trapezoidal
  / Gear2 available and cross-checked to agree); the matrix-exponential fast path is used
  only for islands it can model exactly, and any island containing a controlled source,
  coupled inductor, or nonlinear/behavioral device is routed to the MNA sub-solve
  (exact, merely slower) rather than silently dropped.
- **`uic` / `.ic` interaction.** With `uic`, `.ic V(node)=v` seeds the power-on start
  directly; without `uic`, `.ic` is refused (no DC-pinning machinery). Device-level
  capacitor `ic=` is honored under `uic`.
- **Node-name case handling.** SPICE names are case-insensitive: node and device names are
  matched case-insensitively, so `V(OUT)` and a later `R1 out 0` refer to the same node,
  and a control/coupling reference that matches two names differing only in case is
  refused as ambiguous rather than silently bound.
- **Oracle resampling / `TMAX`.** The ngspice cross-check resamples the oracle onto
  hauksbee's timebase; decks that need tight edge alignment cap the step via `.tran`'s
  `tmax` so both engines sample the same fast transitions (see the note pattern in the
  deck `expect.toml` files and [`results.md`](results.md)).
- **`body_is` (MOSFET body diode) defaults to 0, not ngspice's `1e-14`.** A deck that wants
  reverse body conduction must state `IS=` on the MOS model card. This is forced by the
  bit-identity bar against pre-existing decks.
- **Unsupported *analysis directives* are silently ignored, not refused.** `.tf`, `.noise`,
  `.disto`, `.pz`, `.sens`, `.four`, and `.meas` (and any other unrecognized `.`-directive)
  are currently dropped without error — the loader refuses unsupported *cards* loudly but
  not these directives. This is the one place the "no silent no-op" promise is not yet met;
  it is tracked as a follow-up (add `SpiceError::Unsupported` for the enumerated analysis
  directives). Until then, a deck relying on `.four`/`.meas` output will run its base
  analysis and produce nothing for the unsupported directive.

---

*This statement is the honesty gate for dev-plan `04-spice-compat.md` §7. The subset it
documents is exactly the subset `crates/hauksbee-ir/tests/compat_drift.rs` enforces; the
fidelity numbers live in [`results.md`](results.md).*
