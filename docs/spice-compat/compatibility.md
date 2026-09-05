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
