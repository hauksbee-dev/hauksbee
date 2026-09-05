# SPICE compatibility statement

`hauksbee sim <deck.cir>` reads a netlist through one loader
(`crates/hauksbee-ir/src/spice.rs`) and simulates the subset below. Anything
outside it is refused with a line-numbered error that quotes the card
(`[file ]line N: <reason>: \`<card>\``); nothing is silently misparsed or
dropped. A malformed deck exits 2; a well-formed deck the solver cannot
honestly answer (an `.ac` with no AC source) exits 3.

```
hauksbee sim deck.cir [--op | --tran | --ac | --dc] [--print V(out) I(V1) ...] [--out FILE] [--format csv|raw|both]
hauksbee sim --example rlc_ringdown --tran
```

With no analysis flag, a `.tran` card runs a transient, else the operating
point. Decks: `examples/decks/`.

## Deck syntax

- The first line is the title and is ignored. `*` starts a comment line; `;`
  starts a trailing comment. `+` continues the previous card.
- Names are case-insensitive; node `0` or `gnd` is ground. Duplicate element
  names are refused.
- SI suffixes `k meg u n p f m g t mil` on bare values. `{expr}` anywhere a
  value is taken: arithmetic over `.param` names and the functions below;
  no suffixes inside braces.
- `.include` / `.inc <file>` splices a file (relative to the including file;
  for a deck read from a string, the working directory). Cycles and nesting
  beyond 50 are refused. **`.lib` is refused**; use `.include`.

## Element cards

| Card | Form |
|---|---|
| `R` | `Rxxx a b value [tc=\|tc1=]` (0 ohm becomes 1 uOhm, a jumper) |
| `C`, `L` | `Cxxx a b value [ic=]`, `Lxxx a b value [ic=]` (`ic` honoured under `uic`) |
| `V`, `I` | `Vxxx p n [DC] value \| SIN(...) \| PULSE(...) \| PWL(...) [AC mag phase]` |
| `D` | `Dxxx a k model` (model required, must be a `D` model) |
| `Q` | `Qxxx c b e model` (`NPN`/`PNP`) |
| `M` | `Mxxx d g s b model [L= W=]` (`NMOS`/`PMOS`, LEVEL 1 only) |
| `S` | `Sxxx a b nc+ nc- model` (`SW`/`VSWITCH`) |
| `E`, `G` | `Exxx n+ n- nc+ nc- gain`, `Gxxx n+ n- nc+ nc- gm` (linear only) |
| `F`, `H` | `Fxxx n+ n- vname gain`, `Hxxx n+ n- vname transres`; `vname` must be an independent V source |
| `K` | `Kxxx L1 L2 k`, `0 < k <= 1`, both names inductors |
| `B` | `Bxxx n+ n- V={expr}` or `I={expr}` over `v(a)`, `v(a,b)`, `i(Vname)`, `time`, params |
| `X` | `Xxxx nodes... NAME [params: k=v ...]`; flattened at load, port count must match |

Any other element letter (`T`, `J`, `Z`, `O`, `U`, `W`, `A`, ...) is
`unknown element type`. An unknown or extra `key=value` after an element's
positional fields is refused (`unexpected token ... (allowed options: ...)`).

Source functions: `DC v` or a bare value; `SIN(offset amp freq [delay theta
phase])` (`SINE` accepted); `PULSE(v1 v2 delay rise fall [width period])`
(a missing width is infinite); `PWL(t1 v1 t2 v2 ...)`; `AC [mag] [phase]`
anywhere on the card (bare `AC` is 1, 0). When a function follows a `DC`
value the function wins.

B-source expressions accept `ln log10 log2 exp pow sqrt cbrt abs sin cos tan
asin acos atan atan2 sinh cosh tanh asinh acosh atanh hypot min max if floor
round ceil` and `**`. `log` is refused as ambiguous; `POLY`, `VALUE` and
`TABLE` forms are refused on `E`/`G`/`F`/`H`/`B`; a `V=` output shorting its
own port is refused.

## `.model` cards

| Type | Parameters read |
|---|---|
| `D` | `is n rs cjo(cj0) vj(pb) m tt bv ibv xti eg` |
| `NPN` / `PNP` | `is bf br vaf(va) var(vb) nf nr rb re rc cje cjc tf tr ikf(jbf) ikr(jbr) ise(c2) ne isc(c4) nc xti eg` |
| `NMOS` / `PMOS` | `vto(vt0) kp lambda gamma phi tox cgso cgdo is cbd cbs pb mj rd rs l w`; `LEVEL` other than 1 is refused |
| `SW` / `VSWITCH` | `vt vh ron roff` (defaults 0, 0, 1, 1e12) |

Parameters not in these lists are dropped when their value is alphabetic
(`mfg=Vishay`) and refused when it looks numeric but does not parse
(`VTO={VT0}` with no such param). Redefining a model with different
parameters is refused; a device naming an undefined model, or a model of the
wrong type, is refused. MOSFET notes: Shichman-Hodges DC with Meyer
region-limit gate charge; gate oxide capacitance is zero unless `TOX` is
given; `body_is` defaults to 0 (state `IS=` for body conduction);
`RD`/`RS` are stamped as series resistors, so datasheet `Rds(on)` is
`rd + rs + channel`. BJT per-junction `VJE/VJC/MJE/MJC` are not read
(defaults 0.75 / 0.33).

## Analyses and directives

| Card | Form |
|---|---|
| `.op` | operating point (the default) |
| `.tran` | `.tran tstep tstop [tstart] [tmax] [uic]` |
| `.dc` | `.dc src start stop step [src2 start stop step]`; only an independent V/I source; one card per deck |
| `.ac` | `.ac dec\|oct\|lin n fstart fstop`, `0 < fstart < fstop`; one card per deck |
| `.print` / `.plot` | `.print op\|dc\|ac\|tran V(a) V(a,b) I(V1) ...`; `.plot` is `.print` |
| `.ic` | `.ic V(node)=v ...`; requires `uic` on `.tran`, refused otherwise |
| `.nodeset` | `.nodeset V(node)=v ...`; a Newton start guess, never enforced |
| `.param` | `.param name=expr ...`; order-independent, cycles refused |
| `.options` / `.option` | `reltol= abstol= vntol=`; other keys ignored |
| `.temp` | one global temperature (no per-device `TEMP`) |
| `.subckt NAME ports [params: k=v] ... .ends` | nestable; only `.model` and `.param` allowed inside the body |
| `.end`, `.title`, `.width`, `.save` | accepted and ignored |

`.tf`, `.noise`, `.disto`, `.pz`, `.sens`, `.four`, `.meas`/`.measure`
refuse as `unsupported directive` (not implemented); any other
dot-directive refuses as unrecognized.

## Behaviour notes

- Default `reltol=1e-3`; transient integration uses the solver's companion
  models (BE, trapezoidal, Gear2); islands with controlled sources, coupled
  inductors or nonlinear devices route to the MNA sub-solve.
- `K` is lossless linear coupling only; no saturating core models. `k=1` is
  legal.
- B-source decks use damped Newton and refuse loudly on non-convergence.
- Output: CSV by default (one column per probe); `--format raw` writes an
  ngspice ASCII rawfile; `both` needs `--out`. Every node is retained.

This page describes `spice.rs` as read on 2026-09-04 while the loader was
being reduced; the former drift test (`compat_drift.rs`) no longer exists, so
recheck the card set against the loader after that work lands.

## Supported cards (generated)

The tables below are generated by `crates/hauksbee-ir/tests/compat_drift.rs`,
which loads every SUPPORTED row and proves every REFUSED row is refused with
the documented message. Regenerate with
`UPDATE_COMPAT=1 cargo test -p hauksbee-ir --test it compat_drift`.

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

## Refused cards (generated)

<!-- BEGIN GENERATED: refused (source: crates/hauksbee-ir/tests/compat_drift.rs) -->
<!-- Do not hand-edit between these markers: regenerate with
     UPDATE_COMPAT=1 cargo test -p hauksbee-ir --test compat_drift -->

| Card / form | Why it refuses | Error fragment (substring of the exact message) |
|-------------|----------------|--------------------------------------------------|
| `T` transmission line | Transmission lines are not implemented; the letter is unknown. | `unknown element type `T`` |
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
