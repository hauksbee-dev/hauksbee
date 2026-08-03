# SPICE compatibility statement

*What "drop-in for the common cases" means, and how the promise stays honest.*

Hauksbee reads a `.cir` netlist through one loader
(`crates/hauksbee-ir/src/spice.rs`) and simulates a **documented, enforced
subset** of SPICE. The promise is narrow and testable:

> **Drop-in for the documented subset. A loud, line-numbered refusal outside
> it.**

Two mechanisms back this promise:

1. **Fidelity.** CI cross-checks every supported card against ngspice on a
   corpus of decks with per-quantity tolerances. [`results.md`](results.md)
   holds the living results table.
2. **No drift.** The "Supported" and "Refused" tables below are *generated
   from* `crates/hauksbee-ir/tests/compat_drift.rs`, and every row in them is
   checked against the loader on every `cargo test`. Each row carries a minimal
   snippet: a supported row's snippet must parse, and a refused row's snippet
   must produce the documented error fragment. So the doc cannot claim a
   capability the loader lacks, nor keep claiming a refusal the loader stopped
   making, without turning the test red.

   The one thing that mechanism does *not* do is enumerate the loader. The row
   list is maintained by hand in that test file, so a card the loader gains and
   nobody adds a row for is simply absent from these tables rather than
   flagged. The tables between the `GENERATED` markers are therefore not
   hand-written but the *inventory behind them* is. Regenerate the tables with
   `UPDATE_COMPAT=1 cargo test -p hauksbee-ir --test compat_drift`; edit the
   inventory in `compat_drift.rs`.

Refusals are always line-numbered `SpiceError`s (`Syntax`, `UnknownElement`,
`MissingModel`, `BadNumber`, `Unsupported`) that carry the offending text.
There is no silent misparse of a *recognized* card, and no silent drop of an
*unrecognized* directive. The loader either honors or refuses, with a reason,
any card or directive it understands. It also refuses any dot-directive it
does not recognize. Two carve-outs, both deliberate, both worth knowing:

- A short allowlist of directives that change nothing when ignored (`.end`,
  `.op`, `.title`, `.width`, `.save`), which the loader accepts as a no-op.
  See §4.
- **Unrecognized trailing `key=value` parameters on an element card or a
  `.model` card are silently dropped.** This one is a real trap, so read §2's
  temperature note before you trust a vendor deck.

---

## 1. Supported cards, exhaustively

A snippet that parses through the loader on every `cargo test` proves every
row below. Node `0`/`gnd` is ground. SI suffixes (`k meg u n p f m g t mil`)
apply to bare values. `+` continues a line, and `*` starts a comment. The
loader ignores the deck's first line, which is its title.

<!-- BEGIN GENERATED: supported (source: crates/hauksbee-ir/tests/compat_drift.rs) -->
<!-- Do not hand-edit between these markers: regenerate with
     UPDATE_COMPAT=1 cargo test -p hauksbee-ir --test compat_drift -->

### Element cards

| Card | What it does |
|------|--------------|
| `R` resistor | `Rxxx a b value [tc1=]`: linear resistor, optional linear temp-coefficient. |
| `C` capacitor | `Cxxx a b value [ic=]`: capacitor, optional initial voltage (honored under `uic`). |
| `L` inductor | `Lxxx a b value [ic=]`: inductor, optional initial current. |
| `V` voltage source | `Vxxx p n <dc|sin|pulse|pwl> [AC mag phase]`: independent voltage source. |
| `I` current source | `Ixxx p n <dc|sin|pulse|pwl> [AC mag phase]`: independent current source. |
| `D` diode | `Dxxx a k model`: Shockley diode with junction cap / transit time / breakdown from its `.model` (the model is required and must be a diode model). |
| `Q` BJT | `Qxxx c b e model`: Gummel-Poon BJT with charge storage (cje/cjc/tf/tr) and series rb/re/rc. |
| `M` MOSFET | `Mxxx d g s b model [L= W=]`: LEVEL-1 MOSFET (see caveats) with gate charge and body diode. |
| `S` voltage switch | `Sxxx a b nc+ nc- model`: voltage-controlled switch (`.model SW/VSWITCH`, defaults if absent). |
| `E` VCVS | `Exxx n+ n- nc+ nc- gain`: linear voltage-controlled voltage source. |
| `G` VCCS | `Gxxx n+ n- nc+ nc- gm`: linear voltage-controlled current source. |
| `F` CCCS | `Fxxx n+ n- vname gain`: current-controlled current source (controlled by a named V-source's branch current). |
| `H` CCVS | `Hxxx n+ n- vname transres`: current-controlled voltage source. |
| `B` behavioral source | `Bxxx n+ n- V={expr}` or `I={expr}` over `v()/i()/time/param` (evalexpr subset). |
| `K` coupled inductors | `Kxxx L1 L2 k`: lossless mutual coupling, `0 < k <= 1` (k=1 legal). |
| `X` subcircuit call | `Xxxx nodes... NAME [p=v]`: instantiates a `.subckt`, flattened at load with mangled internal names. |

### `.model` types

| Card | What it does |
|------|--------------|
| `.model ... D` | Diode model: `is n rs cjo vj m tt bv ibv xti eg` (aliases `cj0`, `pb`). |
| `.model ... NPN/PNP` | BJT model: `is bf br vaf var nf nr rb re rc cje cjc tf tr ikf ikr ise ne isc nc xti eg` (aliases `va`/`vb`, `jbf`/`jbr`, `c2`/`c4`). |
| `.model ... NMOS/PMOS` | MOSFET model, LEVEL=1 only: `vto kp lambda gamma phi tox cgso cgdo is cbd cbs pb mj rd rs`. |
| `.model ... SW/VSWITCH` | Voltage-switch model: `vt vh ron roff`. |

### Analyses

| Card | What it does |
|------|--------------|
| `.op` | DC operating point (also the default when no analysis card is present). |
| `.tran` | `.tran tstep tstop [tstart] [tmax] [uic]`: transient analysis. |
| `.dc` | `.dc src start stop step [src2 ...]`: DC sweep of a V/I source, optional nested second sweep. |
| `.ac` | `.ac <dec|oct|lin> n fstart fstop`: small-signal AC sweep (needs an `AC` source stimulus). |

### Directives

| Card | What it does |
|------|--------------|
| `.print` / `.plot` | `.print ANALYSIS var...` selects outputs (`V(a)`, `V(a,b)`, `I(V1)`); `.plot` is treated as `.print`. |
| `.ic` (with `uic`) | `.ic V(node)=val` seeds transient node voltages; requires `uic` on `.tran`. |
| `.nodeset` | `.nodeset V(node)=val`: DC Newton start guess (never pinned/enforced). |
| `.param` | `.param name=expr`: named parameters, order-independent topological resolve. |
| `.include` / `.inc` | `.include <file>` splices another file inline before every other pass. |
| `.lib <file> <section>` | `.lib <file> <section>` splices one named `.lib/.endl` section (bare one-arg form is refused). |
| `.options` / `.option` | `.options reltol= abstol= vntol=`: solver tolerance overrides (other keys ignored). |
| `.temp` | `.temp <celsius>`: one global circuit temperature. |
| `.subckt` / `.ends` | `.subckt NAME ports [p=v]` ... `.ends`: subcircuit definition (nestable calls, per-instance params). |

### Source functions

| Card | What it does |
|------|--------------|
| `DC` | `DC value` (or a bare value): constant source level. |
| `SIN` | `SIN(offset amp freq [delay theta phase])`: damped sinusoid. |
| `PULSE` | `PULSE(v1 v2 delay rise fall width period)`: pulse train. |
| `PWL` | `PWL(t1 v1 t2 v2 ...)`: piecewise-linear waveform. |
| `AC` stimulus | `AC [mag] [phase]` on a source card: the small-signal drive for `.ac` (bare `AC` = mag 1, phase 0). |

### Expressions

| Card | What it does |
|------|--------------|
| `{expr}` values | Curly-brace arithmetic over `.param` names anywhere a numeric value is taken (evalexpr, bare f64s). |
<!-- END GENERATED: supported -->

---

## 2. Supported, with the caveat

These cards work, but not identically to a full SPICE3 / ngspice front end.
Each caveat is deliberate, and where it affects a waveform,
[`results.md`](results.md) quantifies it.

**`.model` parameters beyond the generated rows.** The row summaries in §1 are
the full accepted set per model type, including the BJT high-current-knee and
recombination parameters and the Berkeley aliases (`va`/`vb`, `jbf`/`jbr`,
`c2`/`c4`) and the diode `ibv`. The BJT knee is exercised against ngspice in
[`results.md`](results.md), so it is honored, not just parsed. Anything not in
those rows falls under the silently-dropped rule above.

- **MOSFETs are LEVEL-1 only: a switch model, not an analog model.** The DC
  channel uses the Shichman-Hodges square law. `LEVEL=2/3/BSIM` cards are
  **refused** (§3), not silently downgraded. The implemented physics targets
  board-shaped switching, not analog precision:
  - **Gate charge** uses Meyer's *region-limit* charges. Cgs falls Cox to
    Cox/2 across threshold, and Cgd rises c_ov to c_ov+Cox/2 entering
    triode. This keeps switching edges within **≈2 ns of ngspice** on the
    switch decks (`mos_load_switch`, `pmos_load_switch`), rather than
    matching Meyer's full two-voltage capacitances, which do not conserve
    charge.
  - **Subthreshold** current below `vth` is a smooth tail, a *documented
    deviation*: ngspice's LEVEL-1 has exactly zero current there.
  - **Gate oxide capacitance** is zero when the model omits `TOX` (ngspice
    materializes a default `TOX`/`W`/`L`). State `TOX` on the card to get
    intrinsic gate charge.
  - **Analog accuracy (gain, subthreshold slope, short-channel effects) is a
    known gap.** Use a switch, load switch, or synchronous rectifier deck.
    Do not expect an amplifier small-signal match.
  - **Datasheet `Rds(on)` is honored: supply it as `RD`/`RS`.** A power
    FET's on-state resistance lives mostly in the drain/source ohmic
    resistance, not the channel. Hauksbee reads `RD` and `RS` (SPICE ohmic
    drain/source resistance) from both a `.model` card and the part
    database, which splits each part's datasheet `Rds(on)` into `rd + rs`.
    It stamps them as series resistors, with the transistor intrinsic moved
    onto internal drain/source nodes, exactly the way ngspice level 1 wires
    them. On-state `Rds(on)` is therefore `rd + rs + channel`, and it
    tracks ngspice on the `mos_rds_on` cross-check deck. The default is
    `rd = rs = 0` (ideal), so a model without them behaves unchanged.
  - **A weakly-driven device still reads as high `Rds(on)`, and that is
    physics, not a bug.** Beyond `RD`/`RS`, the gate *over*drive `Vgs −
    Vth` sets the channel term. A hand-rolled LEVEL-1 model whose
    `KP`/`W`/`L` is small, or a gate barely above `Vth`, is genuinely
    resistive, and the operating point shows a large drain-source drop. If
    a switch you expect to be "on" sits at several ohms, raise the
    overdrive (`KP`, `W/L`, or `Vgs`), or state the part's `RD`/`RS`. The
    solver then reports the model you gave it.
- **Coupled inductors `K` model lossless linear mutual coupling only.**
  `k=1` (a perfect transformer) is legal, and the loader solves it without
  inverting the singular L-matrix. **Saturating cores are unsupported**: no
  core (BH) model card parses. Transformer/flyback decks are the payoff
  (`xfmr_1to2`, `xfmr_k1`, `flyback_diode`). A negative `k` is refused.
  Swap a winding's terminals instead.
- **BJT charge storage uses SPICE-default junction grading.** `cje/cjc/tf/tr`
  and series `rb/re/rc` are honored, but per-junction `VJE/VJC`/`MJE/MJC`
  overrides are not parsed. The values default to `0.75`/`0.33`, what an
  ngspice card without them also gets.
- **Diode model must resolve, like `Q`/`M`.** A `Dxxx a k model` whose named
  `.model` is undefined is refused (`references undefined .model`). One
  whose `.model` is not a diode (for example an `NPN`) is refused (`not a
  diode model`) rather than silently inheriting foreign parameters. The
  model token is required, so there is no bare `Dxxx a k` default-diode
  form. This catches a typo'd model name instead of silently defaulting it,
  and all three device classes (`D`/`Q`/`M`) refuse the same way. See §3.
- **Behavioral `B` sources use a fixed expression subset.** `V={expr}` and
  `I={expr}` work over `v(node)`, `v(a,b)`, `i(vsource)`, `time`, and
  `.param` values, with the function set `ln log10 log2 exp pow sqrt cbrt
  abs sin cos tan asin acos atan atan2 sinh cosh tanh asinh acosh atanh
  hypot min max if floor round ceil` (and `**` for exponentiation). **No
  `POLY`, `TABLE`, or `VALUE` forms, and no bare `log`** (write `ln` or
  `log10`). The expression must be brace-wrapped. B-source decks arm a
  damped (Armijo) Newton path and **refuse loudly on non-convergence**
  rather than emit a bad waveform.
- **`.plot` is treated as `.print`.** Output selection is honored. No ASCII
  plot is drawn.
- **Single global temperature, and a per-device `TEMP=` is dropped in
  silence.** `.temp` sets one circuit temperature. There is no per-device
  `TEMP`/`DTEMP`, and here is the part that can bite: an unrecognized trailing
  `key=value` on an element or `.model` card is **discarded without a word**,
  not refused. `R1 a 0 1k`, `R1 a 0 1k temp=125`, `R1 a 0 1k tc2=0.5` and
  `R1 a 0 1k bogus=9` all load and all solve to the same answer. A vendor deck
  that carries `TEMP=`, `DTEMP=` or `TC2=` on its devices therefore gets a
  *different answer than the deck asks for*, with no diagnostic, and nothing in
  the CLI surfaces the dropped keys today: there is no lint or verbose mode for
  this. If your deck relies on per-device temperature or a quadratic
  temperature coefficient, translate it by hand before trusting the result.

  The one thing that *is* refused is a dropped value that looks numeric but
  cannot be read, such as an unresolved brace expression
  (`.model MX NMOS(LEVEL=1 VTO={VT0})` fails with a line number naming `VTO`).
  Values that are plainly alphabetic are treated as metadata and dropped, which
  is what makes `mfg=Vishay` harmless and `IS=abc` quietly fall back to the
  default.
- **`.ic` requires `uic`.** Initial conditions seed the power-on (`uic`)
  transient start. Pinning nodes *during* the DC operating-point solve is
  not implemented, so `.ic` without `uic` is refused (§3) rather than
  silently downgraded.
- **`.nodeset` is a guess, never enforced.** It influences which root
  Newton finds. The converged voltage may differ from the seed.
- **`.lib` requires an explicit section.** The bare one-argument `.lib <file>`
  form is ambiguous and refused. Use `.include <file>` or `.lib <file>
  <section>`.

---

## 3. Refused, loudly

Everything the loader recognizes but does not implement refuses with a
line-numbered error. The fragment column is a substring of the exact
message the user sees. The drift test asserts this fragment.

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
| missing BJT/MOS `.model` | A `Q`/`M` referencing an undefined model is refused (a diode refuses the same way; see below). | `references undefined .model` |
| unknown `.ac` sweep type | `.ac` accepts only `dec`, `oct`, or `lin`. | `unknown `.ac` sweep type` |
| `.param` dependency cycle | Parameters that reference each other circularly are refused. | `dependency cycle` |
| `D` undefined `.model` | A diode naming a model that does not exist is refused, not silently defaulted. | `references undefined .model` |
| `D` non-diode `.model` | A diode naming a `.model` that is not a diode (e.g. an NPN) is refused rather than inheriting foreign params. | `not a diode model` |
| `.tf` | Small-signal transfer-function analysis is not implemented; refused rather than silently ignored. | `unsupported directive `.tf`` |
| `.noise` | Noise analysis is not implemented; refused rather than silently ignored. | `unsupported directive `.noise`` |
| `.disto` | Distortion analysis is not implemented; refused rather than silently ignored. | `unsupported directive `.disto`` |
| `.pz` | Pole-zero analysis is not implemented; refused rather than silently ignored. | `unsupported directive `.pz`` |
| `.sens` | Sensitivity analysis is not implemented; refused rather than silently ignored. | `unsupported directive `.sens`` |
| `.four` | Fourier analysis is not implemented; refused rather than silently ignored. | `unsupported directive `.four`` |
| `.meas` | Measurement statements are not implemented; refused rather than silently ignored. | `unsupported directive `.meas`` |
| unknown `.`-directive | Any dot-directive the loader does not recognize refuses rather than silently dropping (never fall through to a wrong parse). | `unrecognized directive` |
<!-- END GENERATED: refused -->

Also refused as `unknown element type` (the loader has no card for them): any element
letter outside `R C L V I D Q M S E G F H K B` and the `X` subckt call, including `W`,
`A`, `N`, `P`, `Y`, etc.

---

## 4. Behavioral differences from ngspice

A user migrating a deck must know these facts, beyond the per-card caveats
above:

- **Default tolerances.** `reltol=1e-3`, plus `abstol`/`vntol` floors,
  overridable through `.options reltol= abstol= vntol=`. The loader accepts
  other `.options` keys but ignores them.
- **Integration.** Transient analysis uses the solver's companion-model
  integration (BE, trapezoidal, and Gear2 are available and cross-checked
  to agree). The matrix-exponential fast path runs only on islands it can
  model exactly. Any island containing a controlled source, coupled
  inductor, or nonlinear/behavioral device routes to the MNA sub-solve
  instead, exact but slower, rather than being silently dropped.
- **`uic` / `.ic` interaction.** With `uic`, `.ic V(node)=v` seeds the
  power-on start directly. Without `uic`, `.ic` is refused, because the
  loader has no DC-pinning machinery. Device-level capacitor `ic=` is
  honored under `uic`.
- **Node-name case handling.** SPICE names are case-insensitive. The loader
  matches node and device names case-insensitively, so `V(OUT)` and a later
  `R1 out 0` refer to the same node. A control/coupling reference that
  matches two names differing only in case is refused as ambiguous rather
  than silently bound.
- **Oracle resampling / `TMAX`.** The ngspice cross-check resamples the
  oracle onto hauksbee's timebase. Decks that need tight edge alignment cap
  the step through `.tran`'s `tmax`, so both engines sample the same fast
  transitions (see the note pattern in the deck `expect.toml` files and
  [`results.md`](results.md)).
- **`body_is` (MOSFET body diode) defaults to 0, not ngspice's `1e-14`.** A
  deck that wants reverse body conduction must state `IS=` on the MOS model
  card. The bit-identity bar against pre-existing decks forces this default.
- **Unsupported *analysis directives* refuse loudly.** `.tf`, `.noise`,
  `.disto`, `.pz`, `.sens`, `.four`, and `.meas` each refuse with
  `SpiceError::Unsupported` and a per-card reason (`unsupported directive
  `.meas`: measurement statements are not implemented; …`) rather than
  being silently dropped. A deck that asked for one of these analyses will not
  quietly produce nothing. Any *other* unrecognized dot-directive is refused
  the same way (`unrecognized directive`), so nothing falls through to a
  silent no-op. The only directives accepted and ignored are the ones whose
  omission cannot change a computed value: `.end` (deck terminator), `.op`
  (the default DC operating point), `.title` (deck name), and `.width` /
  `.save` (output formatting and selection). Hauksbee retains every node.

---

*This statement documents exactly the subset
`crates/hauksbee-ir/tests/compat_drift.rs` enforces. The fidelity numbers
live in [`results.md`](results.md).*
