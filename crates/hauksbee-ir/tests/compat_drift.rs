//! The doc/code drift test for the SPICE compatibility statement (dev-plan 04
//! §7). This file is THE SINGLE SOURCE OF TRUTH: the exhaustive "Supported
//! cards" and "Refused, loudly" tables in `docs/spice-compat/compatibility.md`
//! are GENERATED from the [`claims`] list below, and every claim is exercised
//! against the real loader on every `cargo test`. The doc therefore cannot
//! drift from the code:
//!
//! * `supported_claims_round_trip`, every card the doc claims SUPPORTED parses
//!   successfully (a minimal snippet per claim, loaded through `SpiceLoader`).
//! * `refused_claims_refuse`, every card the doc claims REFUSED produces an
//!   error whose message contains the documented fragment.
//! * `doc_matches_claims`; the generated tables between the `BEGIN GENERATED`
//!   / `END GENERATED` markers in the doc equal what this claim list renders,
//!   so the human-readable doc can never disagree with the enforced list.
//!   Regenerate after editing the claims with:
//!       UPDATE_COMPAT=1 cargo test -p hauksbee-ir --test compat_drift
//!
//! Adding a supported/refused card to the loader means adding a claim here (and
//! regenerating the doc); removing one makes a claim fail loudly. That coupling
//! is the whole point of the step.

use hauksbee_ir::SpiceLoader;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Which exhaustive table a supported claim renders into, in doc order.
#[derive(Clone, Copy, PartialEq)]
enum Cat {
    Element,
    Model,
    Analysis,
    Directive,
    SourceFn,
    Expression,
}

impl Cat {
    fn heading(self) -> &'static str {
        match self {
            Cat::Element => "Element cards",
            Cat::Model => "`.model` types",
            Cat::Analysis => "Analyses",
            Cat::Directive => "Directives",
            Cat::SourceFn => "Source functions",
            Cat::Expression => "Expressions",
        }
    }
    /// Iteration order for the generated doc.
    fn all() -> &'static [Cat] {
        &[
            Cat::Element,
            Cat::Model,
            Cat::Analysis,
            Cat::Directive,
            Cat::SourceFn,
            Cat::Expression,
        ]
    }
}

enum Expect {
    /// The deck must load successfully.
    Accept,
    /// The deck must be refused; the error message must contain this fragment.
    Refuse(&'static str),
}

struct Claim {
    /// Category (supported claims only; refusals share one table).
    cat: Cat,
    /// The card / form as written in the doc's left column.
    syntax: &'static str,
    /// One-line description (supported) or the refusal reason (refused).
    summary: &'static str,
    /// A minimal complete deck (the first line is a title and is dropped by the
    /// loader, matching SPICE convention).
    deck: &'static str,
    /// Auxiliary files written alongside the deck (name, contents), for
    /// `.include` / `.lib` claims.
    aux: &'static [(&'static str, &'static str)],
    expect: Expect,
}

const fn accept(
    cat: Cat,
    syntax: &'static str,
    summary: &'static str,
    deck: &'static str,
) -> Claim {
    Claim {
        cat,
        syntax,
        summary,
        deck,
        aux: &[],
        expect: Expect::Accept,
    }
}

const fn refuse(
    syntax: &'static str,
    summary: &'static str,
    deck: &'static str,
    fragment: &'static str,
) -> Claim {
    Claim {
        cat: Cat::Element,
        syntax,
        summary,
        deck,
        aux: &[],
        expect: Expect::Refuse(fragment),
    }
}

/// THE accept/refuse list. Left column = what the doc claims; the deck proves it.
fn claims() -> Vec<Claim> {
    vec![
        // --- Elements -------------------------------------------------------
        accept(Cat::Element, "`R` resistor", "`Rxxx a b value [tc1=]`: linear resistor, optional linear temp-coefficient.", "t\nR1 a b 1k tc1=0.001\nV1 a 0 1\n.end\n"),
        accept(Cat::Element, "`C` capacitor", "`Cxxx a b value [ic=]`: capacitor, optional initial voltage (honored under `uic`).", "t\nC1 a b 1u ic=2\nV1 a 0 1\n.end\n"),
        accept(Cat::Element, "`L` inductor", "`Lxxx a b value [ic=]`: inductor, optional initial current.", "t\nL1 a b 1m ic=0\nV1 a 0 1\n.end\n"),
        accept(Cat::Element, "`V` voltage source", "`Vxxx p n <dc|sin|pulse|pwl> [AC mag phase]`: independent voltage source.", "t\nV1 a 0 DC 5\n.end\n"),
        accept(Cat::Element, "`I` current source", "`Ixxx p n <dc|sin|pulse|pwl> [AC mag phase]`: independent current source.", "t\nI1 a 0 1m\nR1 a 0 1k\n.end\n"),
        accept(Cat::Element, "`D` diode", "`Dxxx a k model`: Shockley diode with junction cap / transit time / breakdown from its `.model` (the model is required and must be a diode model).", "t\nV1 a 0 1\nD1 a 0 DM\n.model DM D(IS=1e-14 CJO=2p TT=5n BV=50)\n.end\n"),
        accept(Cat::Element, "`Q` BJT", "`Qxxx c b e model`: Gummel-Poon BJT with charge storage (cje/cjc/tf/tr) and series rb/re/rc.", "t\nQ1 c b e QM\n.model QM NPN(BF=100 CJE=2p CJC=1p TF=1n RB=10)\nV1 c 0 5\n.end\n"),
        accept(Cat::Element, "`M` MOSFET", "`Mxxx d g s b model [L= W=]`: LEVEL-1 MOSFET (see caveats) with gate charge and body diode.", "t\nM1 d g 0 0 MM L=1u W=10u\n.model MM NMOS(VTO=1 KP=2e-5 GAMMA=0.5)\nV1 d 0 5\n.end\n"),
        accept(Cat::Element, "`S` voltage switch", "`Sxxx a b nc+ nc- model`: voltage-controlled switch (`.model SW/VSWITCH`, defaults if absent).", "t\nS1 a b cp cn SM\n.model SM SW(VT=1 VH=0.2 RON=1 ROFF=1e9)\nV1 a 0 5\n.end\n"),
        accept(Cat::Element, "`E` VCVS", "`Exxx n+ n- nc+ nc- gain`: linear voltage-controlled voltage source.", "t\nE1 out 0 in 0 2\nV1 in 0 1\n.end\n"),
        accept(Cat::Element, "`G` VCCS", "`Gxxx n+ n- nc+ nc- gm`: linear voltage-controlled current source.", "t\nG1 out 0 in 0 1m\nV1 in 0 1\nR1 out 0 1k\n.end\n"),
        accept(Cat::Element, "`F` CCCS", "`Fxxx n+ n- vname gain`: current-controlled current source (controlled by a named V-source's branch current).", "t\nF1 out 0 Vs 2\nVs cp cn 0\nV1 cp cn 1\nR1 out 0 1k\n.end\n"),
        accept(Cat::Element, "`H` CCVS", "`Hxxx n+ n- vname transres`: current-controlled voltage source.", "t\nH1 out 0 Vs 100\nVs cp cn 0\nV1 cp cn 1\n.end\n"),
        accept(Cat::Element, "`B` behavioral source", "`Bxxx n+ n- V={expr}` or `I={expr}` over `v()/i()/time/param` (evalexpr subset).", "t\nB1 out 0 V={2*v(in) + tanh(v(in))}\nV1 in 0 1\n.end\n"),
        accept(Cat::Element, "`K` coupled inductors", "`Kxxx L1 L2 k`: lossless mutual coupling, `0 < k <= 1` (k=1 legal).", "t\nL1 a 0 1m\nL2 b 0 1m\nK1 L1 L2 0.99\nV1 a 0 1\n.end\n"),
        accept(Cat::Element, "`X` subcircuit call", "`Xxxx nodes... NAME [p=v]`: instantiates a `.subckt`, flattened at load with mangled internal names.", "t\n.subckt SUB a b\nR1 a b 1k\n.ends\nX1 in 0 SUB\nV1 in 0 1\n.end\n"),
        // --- .model types ---------------------------------------------------
        accept(Cat::Model, "`.model ... D`", "Diode model: `is n rs cjo vj m tt bv xti eg` (aliases `cj0`, `pb`).", "t\nV1 a 0 1\nD1 a 0 DM\n.model DM D(IS=1e-14 N=1 RS=0.1 CJO=2p VJ=0.7 M=0.5 TT=5n BV=75)\n.end\n"),
        accept(Cat::Model, "`.model ... NPN/PNP`", "BJT model: `is bf br vaf var nf nr rb re rc cje cjc tf tr xti eg`.", "t\nQ1 c b e QM\n.model QM PNP(BF=150 VAF=80 CJE=2p TF=1n)\nV1 c 0 -5\n.end\n"),
        accept(Cat::Model, "`.model ... NMOS/PMOS`", "MOSFET model, LEVEL=1 only: `vto kp lambda gamma phi tox cgso cgdo is cbd cbs pb mj rd rs`.", "t\nM1 d g 0 0 MM\n.model MM PMOS(VTO=-1.1 KP=2e-5 TOX=20n RD=0.05 RS=0.05)\nV1 d 0 -5\n.end\n"),
        accept(Cat::Model, "`.model ... SW/VSWITCH`", "Voltage-switch model: `vt vh ron roff`.", "t\nS1 a b cp cn SM\n.model SM VSWITCH(VT=2.5 VH=0.5 RON=0.5 ROFF=1e12)\nV1 a 0 5\n.end\n"),
        // --- analyses -------------------------------------------------------
        accept(Cat::Analysis, "`.op`", "DC operating point (also the default when no analysis card is present).", "t\nV1 a 0 1\nR1 a 0 1k\n.op\n.end\n"),
        accept(Cat::Analysis, "`.tran`", "`.tran tstep tstop [tstart] [tmax] [uic]`: transient analysis.", "t\nV1 a 0 SIN(0 1 1k)\nR1 a 0 1k\n.tran 1u 1m\n.end\n"),
        accept(Cat::Analysis, "`.dc`", "`.dc src start stop step [src2 ...]`: DC sweep of a V/I source, optional nested second sweep.", "t\nV1 a 0 1\nR1 a 0 1k\n.dc V1 0 5 0.1\n.end\n"),
        accept(Cat::Analysis, "`.ac`", "`.ac <dec|oct|lin> n fstart fstop`: small-signal AC sweep (needs an `AC` source stimulus).", "t\nV1 a 0 AC 1\nR1 a b 1k\nC1 b 0 1u\n.ac dec 10 1 100k\n.end\n"),
        // --- directives -----------------------------------------------------
        accept(Cat::Directive, "`.print` / `.plot`", "`.print ANALYSIS var...` selects outputs (`V(a)`, `V(a,b)`, `I(V1)`); `.plot` is treated as `.print`.", "t\nV1 a 0 1\nR1 a 0 1k\n.op\n.print op V(a) I(V1)\n.plot op V(a)\n.end\n"),
        accept(Cat::Directive, "`.ic` (with `uic`)", "`.ic V(node)=val` seeds transient node voltages; requires `uic` on `.tran`.", "t\nV1 a 0 1\nC1 a 0 1u\n.tran 1u 1m uic\n.ic V(a)=2\n.end\n"),
        accept(Cat::Directive, "`.nodeset`", "`.nodeset V(node)=val`: DC Newton start guess (never pinned/enforced).", "t\nV1 a 0 1\nR1 a 0 1k\n.nodeset V(a)=1\n.end\n"),
        accept(Cat::Directive, "`.param`", "`.param name=expr`: named parameters, order-independent topological resolve.", "t\n.param rl=1k gain={rl/500}\nR1 a 0 {rl}\nV1 a 0 {gain}\n.end\n"),
        accept(Cat::Directive, "`.include` / `.inc`", "`.include <file>` splices another file inline before every other pass.", "t\n.include sub.inc\nX1 in 0 SUB\nV1 in 0 1\n.end\n"),
        accept(Cat::Directive, "`.lib <file> <section>`", "`.lib <file> <section>` splices one named `.lib/.endl` section (bare one-arg form is refused).", "t\n.lib models.lib npn\nQ1 c b e QM\nV1 c 0 5\n.end\n"),
        accept(Cat::Directive, "`.options` / `.option`", "`.options reltol= abstol= vntol=`: solver tolerance overrides (other keys ignored).", "t\n.options reltol=1e-4 abstol=1e-12 vntol=1e-6\nV1 a 0 1\nR1 a 0 1k\n.end\n"),
        accept(Cat::Directive, "`.temp`", "`.temp <celsius>`: one global circuit temperature.", "t\n.temp 50\nV1 a 0 1\nR1 a 0 1k\n.end\n"),
        accept(Cat::Directive, "`.subckt` / `.ends`", "`.subckt NAME ports [p=v]` ... `.ends`: subcircuit definition (nestable calls, per-instance params).", "t\n.subckt DIV a b out r=1k\nR1 a out {r}\nR2 out b {r}\n.ends\nX1 in 0 mid DIV r=2k\nV1 in 0 1\n.end\n"),
        // --- source functions ----------------------------------------------
        accept(Cat::SourceFn, "`DC`", "`DC value` (or a bare value): constant source level.", "t\nV1 a 0 DC 5\n.end\n"),
        accept(Cat::SourceFn, "`SIN`", "`SIN(offset amp freq [delay theta phase])`: damped sinusoid.", "t\nV1 a 0 SIN(0 1 1k 0 0 0)\n.end\n"),
        accept(Cat::SourceFn, "`PULSE`", "`PULSE(v1 v2 delay rise fall width period)`: pulse train.", "t\nV1 a 0 PULSE(0 5 0 1n 1n 1u 2u)\n.end\n"),
        accept(Cat::SourceFn, "`PWL`", "`PWL(t1 v1 t2 v2 ...)`: piecewise-linear waveform.", "t\nV1 a 0 PWL(0 0 1u 5 2u 0)\n.end\n"),
        accept(Cat::SourceFn, "`AC` stimulus", "`AC [mag] [phase]` on a source card: the small-signal drive for `.ac` (bare `AC` = mag 1, phase 0).", "t\nV1 a 0 AC 1 90\nR1 a 0 1k\n.end\n"),
        // --- expressions ----------------------------------------------------
        accept(Cat::Expression, "`{expr}` values", "Curly-brace arithmetic over `.param` names anywhere a numeric value is taken (evalexpr, bare f64s).", "t\n.param w=3\nR1 a 0 {1000*w}\nV1 a 0 {w-1}\n.end\n"),

        // ===================================================================
        // REFUSED, LOUDLY, each proves the exact message fragment the user sees.
        // ===================================================================
        refuse("`T` transmission line", "Transmission lines were cut (dev-plan step 15); the letter is unknown.", "t\nT1 a 0 b 0 Z0=50 TD=1n\nR1 b 0 50\n.end\n", "unknown element type `T`"),
        refuse("`J` JFET", "JFETs are unsupported; the element letter is unrecognized.", "t\nJ1 d g s JM\n.model JM NJF\nV1 d 0 5\n.end\n", "unknown element type `J`"),
        refuse("`Z` IGBT / MESFET", "`Z` devices are unsupported.", "t\nZ1 c g e ZM\nV1 c 0 5\n.end\n", "unknown element type `Z`"),
        refuse("`O` lossy line", "Lossy transmission lines (`O`/LTRA) are unsupported.", "t\nO1 a 0 b 0 OM\nV1 a 0 1\n.end\n", "unknown element type `O`"),
        refuse("`U` uniform-RC line", "URC lines are unsupported.", "t\nU1 a b 0 UM\nV1 a 0 1\n.end\n", "unknown element type `U`"),
        refuse("`.model ... NMOS/PMOS LEVEL!=1`", "Only LEVEL-1 MOSFETs are implemented; other levels refuse rather than silently stamp level 1.", "t\nM1 d g 0 0 MX\n.model MX NMOS(LEVEL=3 VTO=1)\nV1 d 0 5\n.end\n", "MOSFET LEVEL=3 is not implemented"),
        refuse("`E`/`G` POLY/VALUE/TABLE", "Only the linear `n+ n- nc+ nc- gain` controlled-source form is supported.", "t\nE1 out 0 POLY(1) in 0 0 1\nV1 in 0 1\n.end\n", "controlled-source form is unsupported"),
        refuse("`F`/`H` POLY", "Only the linear `n+ n- vname gain` current-controlled form is supported.", "t\nF1 out 0 POLY Vs 1\nVs a b 0\nV1 a b 1\n.end\n", "`POLY` controlled-source form is unsupported"),
        refuse("`B` POLY/TABLE/VALUE", "Only `V={expr}`/`I={expr}` behavioral forms are supported (no POLY/TABLE/VALUE).", "t\nB1 out 0 V=TABLE\nV1 in 0 1\n.end\n", "B-source form is unsupported"),
        refuse("`B` unsupported function", "Behavioral expressions accept only a fixed math/function subset.", "t\nB1 out 0 V={gamma(v(in))}\nV1 in 0 1\n.end\n", "unsupported function `gamma"),
        refuse("`B` ambiguous `log`", "`log` is refused as ambiguous across dialects; write `ln` or `log10`.", "t\nB1 out 0 V={log(v(in))}\nV1 in 0 1\n.end\n", "`log` is ambiguous"),
        refuse("engineering suffix in `B={}`", "Inside a behavioral `{}` expression the text is pure arithmetic over bare f64s; a suffix (`2k`) refuses rather than silently dropping the operator.", "t\nB1 out 0 V={2k*3}\nV1 in 0 1\n.end\n", "engineering suffix inside a braced expression"),
        refuse("bare `.lib <file>`", "The one-argument `.lib` form is ambiguous; use `.include` or `.lib <file> <section>`.", "t\n.lib somelib.lib\nV1 a 0 1\n.end\n", "is ambiguous"),
        refuse("`.ic` without `uic`", "`.ic` is only honored on the power-on (`uic`) path; DC pinning is not implemented.", "t\nV1 a 0 1\nC1 a 0 1u\nR1 a 0 1k\n.tran 1u 1m\n.ic V(a)=2\n.end\n", "`.ic` requires `uic`"),
        refuse("`F`/`H` non-source control", "The controlling reference must be an independent V source (branch-current read).", "t\nF1 out 0 R1 2\nR1 a 0 1k\nV1 a 0 1\n.end\n", "is not an independent voltage source"),
        refuse("`K` non-inductor referent", "A K card must couple two `L` elements.", "t\nK1 R1 R2 0.5\nR1 a 0 1k\nR2 b 0 1k\nV1 a 0 1\n.end\n", "not an inductor"),
        refuse("`.dc` on a non-source", "`.dc` can only sweep an independent V or I source.", "t\nV1 a 0 1\nR1 a 0 1k\n.dc R1 0 1 0.1\n.end\n", "can only sweep an independent V or I source"),
        refuse("degenerate VCVS", "A VCVS shorting its own output port (or unity self-sense) is singular and refuses by name.", "t\nE1 out out in 0 2\nV1 in 0 1\n.end\n", "shorts its own output port"),
        refuse("undefined subckt", "An `X` call to a subcircuit that was never defined refuses with the name.", "t\nX1 a b MISSING\nV1 a 0 1\n.end\n", "undefined subckt"),
        refuse("missing BJT/MOS `.model`", "A `Q`/`M` referencing an undefined model is refused (a diode refuses the same way; see below).", "t\nQ1 c b e NOPE\nV1 c 0 5\n.end\n", "references undefined .model"),
        refuse("unknown `.ac` sweep type", "`.ac` accepts only `dec`, `oct`, or `lin`.", "t\nV1 a 0 AC 1\nR1 a 0 1k\n.ac log 10 1 100k\n.end\n", "unknown `.ac` sweep type"),
        refuse("`.param` dependency cycle", "Parameters that reference each other circularly are refused.", "t\n.param a={b}\n.param b={a}\nR1 a 0 {a}\nV1 a 0 1\n.end\n", "dependency cycle"),
        // Diode model resolution now matches Q/M: a named model that is missing
        // or is not a diode refuses instead of silently defaulting.
        refuse("`D` undefined `.model`", "A diode naming a model that does not exist is refused, not silently defaulted.", "t\nV1 a 0 1\nD1 a 0 NOPE\n.end\n", "references undefined .model"),
        refuse("`D` non-diode `.model`", "A diode naming a `.model` that is not a diode (e.g. an NPN) is refused rather than inheriting foreign params.", "t\nV1 c 0 5\nD1 c 0 QM\n.model QM NPN(BF=100)\n.end\n", "not a diode model"),
        // Unsupported analysis directives now refuse loudly, each with its own
        // reason, rather than being silently ignored.
        refuse("`.tf`", "Small-signal transfer-function analysis is not implemented; refused rather than silently ignored.", "t\nV1 a 0 1\nR1 a 0 1k\n.tf V(a) V1\n.end\n", "unsupported directive `.tf`"),
        refuse("`.noise`", "Noise analysis is not implemented; refused rather than silently ignored.", "t\nV1 a 0 AC 1\nR1 a 0 1k\n.noise V(a) V1 dec 10 1 100k\n.end\n", "unsupported directive `.noise`"),
        refuse("`.disto`", "Distortion analysis is not implemented; refused rather than silently ignored.", "t\nV1 a 0 AC 1\nR1 a 0 1k\n.disto dec 10 1 100k\n.end\n", "unsupported directive `.disto`"),
        refuse("`.pz`", "Pole-zero analysis is not implemented; refused rather than silently ignored.", "t\nV1 a 0 1\nR1 a 0 1k\n.pz a 0 a 0 cur pz\n.end\n", "unsupported directive `.pz`"),
        refuse("`.sens`", "Sensitivity analysis is not implemented; refused rather than silently ignored.", "t\nV1 a 0 1\nR1 a 0 1k\n.sens V(a)\n.end\n", "unsupported directive `.sens`"),
        refuse("`.four`", "Fourier analysis is not implemented; refused rather than silently ignored.", "t\nV1 a 0 SIN(0 1 1k)\nR1 a 0 1k\n.tran 1u 1m\n.four 1k V(a)\n.end\n", "unsupported directive `.four`"),
        refuse("`.meas`", "Measurement statements are not implemented; refused rather than silently ignored.", "t\nV1 a 0 1\nR1 a 0 1k\n.tran 1u 1m\n.meas tran vmax MAX V(a)\n.end\n", "unsupported directive `.meas`"),
        refuse("unknown `.`-directive", "Any dot-directive the loader does not recognize refuses rather than silently dropping (never fall through to a wrong parse).", "t\nV1 a 0 1\nR1 a 0 1k\n.bogus foo bar\n.end\n", "unrecognized directive"),
    ]
}

// Auxiliary files for the `.include` / `.lib` accept claims, matched to the
// deck by syntax label (kept out of the const claim so the fixtures can be
// multi-line without escaping churn).
fn aux_for(syntax: &str) -> Vec<(&'static str, &'static str)> {
    match syntax {
        "`.include` / `.inc`" => vec![("sub.inc", ".subckt SUB a b\nR1 a b 1k\n.ends\n")],
        "`.lib <file> <section>`" => vec![(
            "models.lib",
            ".lib npn\n.model QM NPN(BF=100)\n.endl\n.lib pnp\n.model QP PNP(BF=50)\n.endl\n",
        )],
        _ => vec![],
    }
}

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// Write the deck (and any aux files) into a fresh temp dir and load it through
/// the file entry point, so `.include`/`.lib` resolve. Returns the loader result
/// as a stringly-typed Result for uniform assertion.
fn load_claim(claim: &Claim) -> Result<(), String> {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir: PathBuf =
        std::env::temp_dir().join(format!("hauksbee_compat_{}_{}", std::process::id(), n));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    for (name, contents) in aux_for(claim.syntax).iter().chain(claim.aux.iter()) {
        std::fs::write(dir.join(name), contents).expect("write aux file");
    }
    let deck_path = dir.join("deck.cir");
    std::fs::write(&deck_path, claim.deck).expect("write deck");
    let result = SpiceLoader::load_file_with_directives(&deck_path)
        .map(|_| ())
        .map_err(|e| e.to_string());
    let _ = std::fs::remove_dir_all(&dir);
    result
}

#[test]
fn supported_claims_round_trip() {
    let mut n = 0usize;
    for claim in claims() {
        if !matches!(claim.expect, Expect::Accept) {
            continue;
        }
        n += 1;
        match load_claim(&claim) {
            Ok(()) => {}
            Err(e) => panic!(
                "SUPPORTED claim `{}` failed to load:\n  deck:\n{}\n  error: {e}",
                claim.syntax, claim.deck
            ),
        }
    }
    println!("supported claims verified: {n}");
    assert!(n >= 30, "expected the full supported list, got {n}");
}

#[test]
fn refused_claims_refuse() {
    let mut n = 0usize;
    for claim in claims() {
        let Expect::Refuse(fragment) = claim.expect else {
            continue;
        };
        n += 1;
        match load_claim(&claim) {
            Ok(()) => panic!(
                "REFUSED claim `{}` was accepted; the loader no longer refuses it:\n{}",
                claim.syntax, claim.deck
            ),
            Err(e) => assert!(
                e.contains(fragment),
                "REFUSED claim `{}`: error did not contain documented fragment.\n  \
                 wanted substring: {fragment:?}\n  got: {e}",
                claim.syntax
            ),
        }
    }
    println!("refusal claims verified: {n}");
    assert!(n >= 20, "expected the full refusal list, got {n}");
}

// --- doc generation ---------------------------------------------------------

const REGEN_HINT: &str =
    "<!-- Do not hand-edit between these markers: regenerate with\n     UPDATE_COMPAT=1 cargo test -p hauksbee-ir --test compat_drift -->";

/// A generated region is a `(begin_marker, end_marker, body)` triple.
struct Region {
    begin: &'static str,
    end: &'static str,
    body: String,
}

fn supported_region() -> Region {
    let all = claims();
    let mut s = String::new();
    s.push_str(&format!("{REGEN_HINT}\n\n"));
    for cat in Cat::all() {
        let rows: Vec<&Claim> = all
            .iter()
            .filter(|c| matches!(c.expect, Expect::Accept) && c.cat == *cat)
            .collect();
        if rows.is_empty() {
            continue;
        }
        s.push_str(&format!("### {}\n\n", cat.heading()));
        s.push_str("| Card | What it does |\n|------|--------------|\n");
        for c in rows {
            s.push_str(&format!("| {} | {} |\n", c.syntax, c.summary));
        }
        s.push('\n');
    }
    Region {
        begin:
            "<!-- BEGIN GENERATED: supported (source: crates/hauksbee-ir/tests/compat_drift.rs) -->",
        end: "<!-- END GENERATED: supported -->",
        body: s.trim_end().to_string(),
    }
}

fn refused_region() -> Region {
    let all = claims();
    let mut s = String::new();
    s.push_str(&format!("{REGEN_HINT}\n\n"));
    s.push_str(
        "| Card / form | Why it refuses | Error fragment (substring of the exact message) |\n",
    );
    s.push_str(
        "|-------------|----------------|--------------------------------------------------|\n",
    );
    for c in &all {
        if let Expect::Refuse(fragment) = c.expect {
            s.push_str(&format!(
                "| {} | {} | `{}` |\n",
                c.syntax, c.summary, fragment
            ));
        }
    }
    Region {
        begin:
            "<!-- BEGIN GENERATED: refused (source: crates/hauksbee-ir/tests/compat_drift.rs) -->",
        end: "<!-- END GENERATED: refused -->",
        body: s.trim_end().to_string(),
    }
}

fn doc_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/spice-compat/compatibility.md")
}

#[test]
fn doc_matches_claims() {
    let path = doc_path();
    let update = std::env::var("UPDATE_COMPAT").is_ok();
    let mut doc = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    let mut stale: Vec<&str> = Vec::new();

    for region in [supported_region(), refused_region()] {
        let want = format!("{}\n{}\n{}", region.begin, region.body, region.end);
        let b = doc.find(region.begin);
        let e = doc.find(region.end).map(|i| i + region.end.len());
        let current = match (b, e) {
            (Some(b), Some(e)) if e > b => doc[b..e].to_string(),
            _ => panic!(
                "markers not found in {}; add\n{}\n{}\nwhere this table belongs",
                path.display(),
                region.begin,
                region.end
            ),
        };
        if current == want {
            continue;
        }
        if update {
            let (b, e) = (b.unwrap(), e.unwrap());
            doc = format!("{}{}{}", &doc[..b], want, &doc[e..]);
        } else {
            stale.push(region.begin);
        }
    }

    if update {
        std::fs::write(&path, &doc).expect("write doc");
        eprintln!("regenerated compat tables in {}", path.display());
        return;
    }
    assert!(
        stale.is_empty(),
        "docs/spice-compat/compatibility.md is out of sync with the claim list \
         (stale regions: {stale:?}).\n\
         Regenerate with: UPDATE_COMPAT=1 cargo test -p hauksbee-ir --test compat_drift"
    );
}
