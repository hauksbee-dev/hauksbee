//! A pragmatic SPICE netlist loader.
//!
//! Parses a useful subset of `.cir` files into a [`Circuit`]: element lines for
//! R/C/L/V/I/D/Q/M/S/E/G/F/H, `.model` cards for diodes, BJTs, and MOSFETs, the
//! `sin`, `pulse`, and `pwl` source functions, `.tran`, `.temp`, and `.options`.
//! The goal is to ingest real test vectors and user-supplied netlists, not to be
//! a complete SPICE3 front end; anything unsupported is reported with the line.
//!
//! # Element-name references (dev-plan 04 §2.2, resolve-by-name)
//!
//! `F`/`H` cards name ANOTHER ELEMENT — the voltage source whose branch current
//! controls them: `F1 n+ n- Vsense gain`. The referent may appear later in the
//! deck, so these resolve in a deferred second pass: parsing records a
//! [`NameFixup`] with a placeholder id, and after the whole (flattened) deck is
//! parsed, [`resolve_name_fixups`] builds one case-insensitive name index and
//! patches each reference — a dangling name, an ambiguous name (two devices
//! differing only in case), or a referent that is not an independent `V` source
//! is a line-numbered error. SPICE allows only V-source control; to read the
//! current of anything else, insert the idiomatic zero-volt ammeter
//! (`Vsense a b 0`) in series and name that. The pass is deliberately generic
//! (any card kind can defer any device-name field) because the behavioral
//! B-source (§2.5) will reuse it.
//!
//! **Scoping across subckts:** a `vname` inside a `.subckt` body is LOCAL —
//! flattening prefixes it with the instance path exactly like a refdes
//! (`Vsense` in instance `X3` resolves to `X3.Vsense`), matching ngspice's
//! subckt name translation. There is no fallback to a same-named global source:
//! a typo'd local name fails loudly instead of silently binding outside the
//! subckt, and a subckt that wants a global control current should take it in
//! through a port with a local ammeter. (Dotted references compose: `X9.Vs`
//! written inside a body becomes `X3.X9.Vs`.) A TOP-LEVEL card may reference a
//! source inside an instance by its flattened name (`F1 a b X3.Vsense 2`) —
//! unambiguous, though not portable to other SPICE dialects.
//!
//! Conventions: the first line is a title (ignored), `*` begins a comment,
//! `+` continues the previous line, and node `0`/`gnd` is ground. SI suffixes
//! (`k`, `meg`, `u`, `n`, `p`, `f`, `m`, `g`, `t`, `mil`) are understood.
//!
//! # Parameters and expressions (dev-plan 04 §4.2)
//!
//! `.param name=expr` cards define named parameters; `{expr}` curly-brace
//! expressions may appear wherever an element takes a numeric value. Parameters
//! resolve topologically, so `.param a={b*2}` works regardless of card order; a
//! cycle or an undefined name is a line-numbered error. **Suffix rule:** SPICE
//! engineering suffixes (`k`, `u`, ...) apply only to bare value tokens (an
//! element value or a `.param` right-hand side written *without* braces). Inside
//! `{...}` the text is pure `evalexpr` arithmetic over bare `f64`s — a parameter
//! referenced there is its already-resolved bare number. This keeps one rule:
//! a braced expression yields a bare `f64`; suffixes are a tokenizer convenience
//! outside braces only. A mixed `1k*2` (suffix inside arithmetic) refuses loudly
//! rather than silently dropping the `*2`.
//!
//! # Subcircuits (dev-plan 04 §2.4, flatten-at-load)
//!
//! `.subckt NAME ports... [param=val ...] ... .ends` blocks are collected in the
//! first pass, then every `Xxxx nodes... NAME [param=val ...]` call is spliced
//! into the flat device list: internal node `foo` in instance `X3` becomes
//! `X3.foo`, formal ports map to the caller's actual nodes, `0`/`gnd` stays
//! global ground, and refdes are prefixed (`R1` -> `X3.R1`). Nested `X` calls
//! recurse (with a depth guard and a self-instantiation cycle check). Parameter
//! substitution is per-instance: `.subckt` defaults plus the `X`-line overrides
//! feed the expression environment before the body is parsed, so an inner param
//! never leaks to the outer scope or across sibling instances. The solver never
//! sees hierarchy; a flattened deck is indistinguishable from a hand-written
//! flat one. Errors inside a spliced body point at both the `.subckt` body line
//! and the instantiation site. `.model` cards inside a subckt are hoisted to a
//! single global table with a collision check: identical redefinitions are
//! allowed, conflicting same-name definitions refuse loudly (never silently
//! shadow).
//!
//! # File inclusion (dev-plan 04 §4.1, `.include` / `.lib`)
//!
//! `.include <file>` splices another file's text in place; `.lib <file>
//! <section>` splices only the named `.lib <section> ... .endl` block from a
//! library file. Inclusion happens FIRST, at the physical-line level, before
//! any other pass — so included `.model`/`.subckt`/`.param` cards and elements
//! participate in every later pass exactly as if they had been typed inline.
//!
//! **Search path:** a relative include resolves against the INCLUDING file's
//! directory first, then the top deck's directory; an absolute path is used
//! as-is. When the deck is loaded from a string (`load_with_directives`) there
//! is no file path, so both locations collapse to the process working
//! directory; load via [`SpiceLoader::load_file`] to get directory-relative
//! resolution. Missing files, inclusion cycles (direct or transitive), and a
//! depth past [`MAX_INCLUDE_DEPTH`] are line-numbered refusals that name the
//! resolved-path attempts / the cycle chain. Errors inside an included file
//! name the file, its own line, and the inclusion site (the same
//! provenance-breadcrumb discipline subckt splicing uses).
//!
//! **Bare `.lib <file>` (one argument) is refused**, not silently treated like
//! `.include`: the one-argument form is ambiguous (a section-open inside a
//! library vs. a whole-file pull), so the loader points the user at
//! `.include <file>` for a whole file or `.lib <file> <section>` for a section.
//!
//! # Initial conditions (dev-plan 04 §4.1, `.ic` / `.nodeset`)
//!
//! `.ic V(node)=val` seeds transient node voltages; `.nodeset V(node)=val`
//! seeds the DC Newton start vector. Both are parsed AFTER subckt flattening so
//! a node name resolves against the final (flattened) node table — including a
//! mangled internal node like `.ic V(X1.out)=2` (the flattened-name contract).
//! Values accept `{expr}` and suffixed numbers through the same [`ParamEnv`] as
//! element values. An unknown node is a line-numbered error with did-you-mean
//! candidates.
//!
//! `.ic` semantics: with `uic` on the `.tran` card the named node voltages seed
//! the power-on start directly (the solver's `FromZero` path, extended to read
//! these values). WITHOUT `uic`, SPICE pins the named nodes DURING the DC solve
//! — machinery the solver does not have (only device-level capacitor `ic=`
//! pinning exists) — so the loader REFUSES `.ic` without `uic` loudly rather
//! than silently downgrading it to a start-vector seed. `.nodeset` is a
//! convergence GUESS only: it influences which root Newton finds but is never
//! enforced (the final voltage may differ from the seed).

use crate::models::{BjtModel, DiodeModel, MosLevel, MosfetModel, Polarity};
use crate::source::{AcStim, PwlPoint, SourceKind};
use crate::{Circuit, Device, DeviceId, NodeId};
use evalexpr::{
    build_operator_tree, ContextWithMutableVariables, DefaultNumericTypes, HashMapContext,
    Node as EvalNode, Value,
};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use thiserror::Error;

/// The resolved parameter environment: parameter name (lowercased, SPICE is
/// case-insensitive) to its numeric value. Built once from `.param` cards and
/// re-scoped per subckt instance during expansion. This is the shared
/// environment dev-plan 04 §4.2 calls for — the future B-source (§2.5) consumes
/// the same map.
type ParamEnv = HashMap<String, f64>;

/// A directive recovered from the netlist that is not part of the circuit
/// topology but the solver may want (e.g. the requested transient window).
#[derive(Debug, Clone, Default)]
pub struct Directives {
    /// `.tran <tstep> <tstop> [tstart] [tmax] [uic]` if present.
    pub tran: Option<TranDirective>,
    /// `.options reltol=...` overrides the loader saw.
    pub reltol: Option<f64>,
    pub abstol: Option<f64>,
    pub vntol: Option<f64>,
    /// Whether `.tran` carried the `uic` flag.
    pub use_initial_conditions: bool,
    /// `.ac <dec|oct|lin> <n> <fstart> <fstop>` if present.
    pub ac: Option<AcDirective>,
    /// `.dc <src> <start> <stop> <step> [<src2> ...]` if present, with the swept
    /// source(s) already resolved to a device.
    pub dc: Option<DcDirective>,
    /// `.print`/`.plot ANALYSIS var...` output requests, in source order.
    pub prints: Vec<PrintRequest>,
    /// Whether any `.plot` card was seen (treated as `.print` — no ASCII plot).
    pub saw_plot: bool,
}

/// Parsed `.tran` parameters (seconds).
#[derive(Debug, Clone, Copy)]
pub struct TranDirective {
    pub tstep: f64,
    pub tstop: f64,
    pub tstart: f64,
    pub tmax: Option<f64>,
}

/// How a `.ac` sweep spaces its frequency points.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcSweep {
    /// `dec`: `n` points per decade (log spacing).
    Decade,
    /// `oct`: `n` points per octave (log spacing).
    Octave,
    /// `lin`: `n` points total, linearly spaced.
    Linear,
}

/// Parsed `.ac <dec|oct|lin> <n> <fstart> <fstop>`.
#[derive(Debug, Clone, Copy)]
pub struct AcDirective {
    pub sweep: AcSweep,
    /// Points per decade/octave (log) or total points (linear).
    pub points: usize,
    pub fstart: f64,
    pub fstop: f64,
}

/// One swept source of a `.dc` analysis: a resolved V/I source and its range.
#[derive(Debug, Clone)]
pub struct DcSweep {
    /// The resolved swept source (a `Vsource` or `Isource`).
    pub source: DeviceId,
    /// The source's name as written, for the output column label.
    pub name: String,
    pub start: f64,
    pub stop: f64,
    pub step: f64,
}

/// Parsed `.dc <src> <start> <stop> <step> [<src2> <start2> <stop2> <step2>]`.
/// `inner` sweeps fastest (SPICE convention); `outer`, if present, wraps it.
#[derive(Debug, Clone)]
pub struct DcDirective {
    pub inner: DcSweep,
    pub outer: Option<DcSweep>,
}

/// A `.print`/`.plot ANALYSIS var...` request. Variable expressions are carried
/// verbatim (e.g. `V(out)`, `V(a,b)`, `I(V1)`) and parsed into probes by the
/// consumer (`hauksbee_solve::Probe::parse`) — the loader does not duplicate that
/// parser.
#[derive(Debug, Clone)]
pub struct PrintRequest {
    /// The analysis this line applies to, lowercased: `op`/`dc`/`ac`/`tran`.
    pub analysis: String,
    /// The output-variable expressions, verbatim.
    pub vars: Vec<String>,
    /// True if this came from `.plot` (rendered as a `.print`, never an ASCII plot).
    pub is_plot: bool,
}

/// Loads SPICE netlists into the IR.
pub struct SpiceLoader;

/// Errors raised while parsing a netlist, all carrying the offending line.
#[derive(Debug, Error)]
pub enum SpiceError {
    #[error("line {line}: {msg}: `{text}`")]
    Syntax {
        line: usize,
        msg: String,
        text: String,
    },
    #[error("line {line}: unknown element type `{ch}`: `{text}`")]
    UnknownElement { line: usize, ch: char, text: String },
    #[error("line {line}: references undefined .model `{model}`: `{text}`")]
    MissingModel {
        line: usize,
        model: String,
        text: String,
    },
    #[error("line {line}: malformed number `{tok}`: `{text}`")]
    BadNumber {
        line: usize,
        tok: String,
        text: String,
    },
}

impl SpiceLoader {
    /// Parse a netlist into a [`Circuit`], discarding directives.
    pub fn load(text: &str) -> Result<Circuit, SpiceError> {
        Ok(Self::load_with_directives(text)?.0)
    }

    /// Parse a netlist STRING into a [`Circuit`] plus the [`Directives`] it
    /// carried. `.include`/`.lib` file paths resolve against the process working
    /// directory (a bare string has no file location of its own); use
    /// [`SpiceLoader::load_file`] for directory-relative inclusion.
    pub fn load_with_directives(text: &str) -> Result<(Circuit, Directives), SpiceError> {
        load_deck(text, Path::new("."), "<deck>", None)
    }

    /// Parse a netlist FILE into a [`Circuit`]. `.include`/`.lib` paths resolve
    /// against this file's directory first, then the top deck's directory.
    pub fn load_file<P: AsRef<Path>>(path: P) -> Result<Circuit, SpiceError> {
        Ok(Self::load_file_with_directives(path)?.0)
    }

    /// [`SpiceLoader::load_file`], also returning the parsed [`Directives`].
    pub fn load_file_with_directives<P: AsRef<Path>>(
        path: P,
    ) -> Result<(Circuit, Directives), SpiceError> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).map_err(|e| SpiceError::Syntax {
            line: 0,
            msg: format!("cannot read deck `{}`: {e}", path.display()),
            text: String::new(),
        })?;
        let dir = path.parent().unwrap_or_else(|| Path::new("."));
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        let canon = path.canonicalize().ok();
        load_deck(&text, dir, &name, canon)
    }
}

/// The multi-pass core, shared by the string and file entry points.
fn load_deck(
    text: &str,
    top_dir: &Path,
    top_name: &str,
    top_canon: Option<PathBuf>,
) -> Result<(Circuit, Directives), SpiceError> {
    let mut circuit = Circuit::new();
    let mut directives = Directives::default();

    // Pass 0 (NEW): expand `.include`/`.lib` at the physical-line level so
    // included content participates in EVERY later pass, then join
    // continuations. Each line carries a provenance breadcrumb naming its file
    // and inclusion site, appended to any error raised from it.
    let mut ctx = IncludeCtx {
        top_dir: top_dir.to_path_buf(),
        stack: top_canon.into_iter().collect(),
    };
    let mut phys: Vec<PhysLine> = Vec::new();
    read_source(text, top_dir, top_name, Rc::from(""), true, &mut ctx, &mut phys)?;
    let logical = join_continuations(&phys);

    // First pass: collect `.model`/`.subckt`/`.param`/directives, set aside
    // top-level elements, and stash `.ic`/`.nodeset` cards (resolved after
    // flattening, once every node exists).
    let mut col = Collector::default();
    for pl in &logical {
        collect_line(pl, &mut col, &mut circuit, &mut directives)
            .map_err(|e| with_provenance(e, &pl.origin))?;
    }
    if let Some(def) = col.current {
        return Err(SpiceError::Syntax {
            line: def.def_line,
            msg: format!("`.subckt {}` is never closed with `.ends`", def.name),
            text: String::new(),
        });
    }

    // Build the global parameter environment (order-independent topological
    // resolve; cycles and undefined names error with a line number).
    let global_env: Rc<ParamEnv> = Rc::new(resolve_params(&col.param_cards, &ParamEnv::new())?);

    // Flatten: splice every `X` call into a flat list of element lines, each
    // carrying its parameter environment and a provenance breadcrumb.
    let mut expanded: Vec<SplicedLine> = Vec::new();
    for pl in &col.top_elems {
        if starts_with_letter(&pl.text, 'x') {
            expand_instance(
                pl.lineno,
                &pl.text,
                &col.subckts,
                global_env.clone(),
                &pl.origin,
                &mut Vec::new(),
                &mut expanded,
            )?;
        } else {
            expanded.push(SplicedLine {
                lineno: pl.lineno,
                text: pl.text.clone(),
                provenance: pl.origin.to_string(),
                env: global_env.clone(),
            });
        }
    }

    // Second pass: parse the flattened element lines. Errors from a spliced body
    // are annotated with where the body came from and where it was instantiated.
    let mut fixups: Vec<NameFixup> = Vec::new();
    for sl in &expanded {
        let before = fixups.len();
        parse_element(sl.lineno, &sl.text, &mut circuit, &col.models, &sl.env, &mut fixups)
            .map_err(|e| with_provenance(e, &sl.provenance))?;
        for fx in &mut fixups[before..] {
            fx.provenance = sl.provenance.clone();
        }
    }

    // Third pass: resolve element-name references (§2.2).
    resolve_name_fixups(&mut circuit, &fixups)?;

    // `.dc`: resolve the swept source name(s) against the flattened device table
    // (case-insensitive, mangled-name composing), reusing the same conventions
    // the F/H control-source fixups use. Refuses a name that is not a source, or
    // a step that cannot reach its stop.
    if let Some(pl) = col.dc_cards.first() {
        if col.dc_cards.len() > 1 {
            return Err(with_provenance(
                SpiceError::Syntax {
                    line: col.dc_cards[1].lineno,
                    msg: "duplicate `.dc` card (only one DC sweep per deck)".into(),
                    text: col.dc_cards[1].text.clone(),
                },
                &col.dc_cards[1].origin,
            ));
        }
        directives.dc = Some(parse_dc(pl, &circuit).map_err(|e| with_provenance(e, &pl.origin))?);
    }

    // Fourth pass (NEW): `.ic`/`.nodeset`, resolved now that every flattened
    // node exists. Values evaluate against the global parameter environment; a
    // mangled internal node (`X1.out`) resolves by its flattened name.
    let mut ic_out: Vec<(NodeId, f64)> = Vec::new();
    for pl in &col.ic_cards {
        parse_ic_values(pl, &circuit, &global_env, "`.ic`", &mut ic_out)
            .map_err(|e| with_provenance(e, &pl.origin))?;
    }
    let mut ns_out: Vec<(NodeId, f64)> = Vec::new();
    for pl in &col.nodeset_cards {
        parse_ic_values(pl, &circuit, &global_env, "`.nodeset`", &mut ns_out)
            .map_err(|e| with_provenance(e, &pl.origin))?;
    }

    // `.ic` is only honestly supported on the `uic` power-on path: hauksbee has
    // no machinery to PIN a node during the DC operating-point solve (only
    // device-level capacitor `ic=` pinning exists). Refuse `.ic` without `uic`
    // loudly rather than silently downgrading it to a start-vector seed.
    if !ic_out.is_empty() && !directives.use_initial_conditions {
        let first = &col.ic_cards[0];
        return Err(with_provenance(
            SpiceError::Syntax {
                line: first.lineno,
                msg: "`.ic` requires `uic` on the `.tran` card. hauksbee seeds the named \
                      node voltages at the power-on (uic) start; it does NOT implement \
                      pinning them during a DC operating-point solve. Add `uic` to the \
                      `.tran` card, or remove the `.ic`."
                    .into(),
                text: first.text.clone(),
            },
            &first.origin,
        ));
    }

    circuit.initial_conditions = ic_out;
    circuit.nodesets = ns_out;

    Ok((circuit, directives))
}

/// Accumulators for the first pass, extracted so each line's processing can be
/// wrapped once with its provenance breadcrumb (see [`load_deck`]).
#[derive(Default)]
struct Collector {
    models: HashMap<String, ModelCard>,
    subckts: HashMap<String, SubcktDef>,
    param_cards: Vec<ParamCard>,
    /// Top-level element / `X`-instantiation lines, in source order.
    top_elems: Vec<PhysLine>,
    /// `.ic` cards (raw), resolved against the flattened node table afterward.
    ic_cards: Vec<PhysLine>,
    /// `.nodeset` cards (raw), resolved the same way.
    nodeset_cards: Vec<PhysLine>,
    /// `.dc` cards (raw); the swept source name resolves against the flattened
    /// device table afterward, exactly as `.ic` resolves against nodes.
    dc_cards: Vec<PhysLine>,
    /// The subckt currently being collected (definitions do not nest).
    current: Option<SubcktDef>,
}

/// Process one logical line in the first pass. Returns a bare (un-provenanced)
/// error; [`load_deck`] wraps it with the line's breadcrumb.
fn collect_line(
    pl: &PhysLine,
    col: &mut Collector,
    circuit: &mut Circuit,
    directives: &mut Directives,
) -> Result<(), SpiceError> {
    let lineno = pl.lineno;
    let raw = &pl.text;
    let trimmed = raw.trim_start();
    let lower = trimmed.to_ascii_lowercase();

    if lower.starts_with(".subckt") {
        if col.current.is_some() {
            return Err(SpiceError::Syntax {
                line: lineno,
                msg: "nested `.subckt` definitions are unsupported".into(),
                text: raw.clone(),
            });
        }
        col.current = Some(parse_subckt_header(lineno, raw)?);
        return Ok(());
    }
    if lower.starts_with(".ends") {
        match col.current.take() {
            Some(def) => {
                if let Some(prev) = col.subckts.insert(def.name.to_ascii_lowercase(), def) {
                    return Err(SpiceError::Syntax {
                        line: lineno,
                        msg: format!("duplicate `.subckt {}` definition", prev.name),
                        text: raw.clone(),
                    });
                }
            }
            None => {
                return Err(SpiceError::Syntax {
                    line: lineno,
                    msg: "`.ends` without a matching `.subckt`".into(),
                    text: raw.clone(),
                });
            }
        }
        return Ok(());
    }

    // Inside a subckt body: hoist `.model`, keep `.param`/elements/X for the
    // body, refuse analysis/topology directives that make no sense in a subckt.
    if let Some(def) = col.current.as_mut() {
        if lower.starts_with(".model") {
            let card = parse_model_card(lineno, raw)?;
            insert_model(&mut col.models, card, lineno, raw)?;
        } else if lower.starts_with('.') && !lower.starts_with(".param") {
            return Err(SpiceError::Syntax {
                line: lineno,
                msg: format!(
                    "directive `{}` is not allowed inside a `.subckt` body",
                    first_token(trimmed)
                ),
                text: raw.clone(),
            });
        } else if !trimmed.is_empty() && !trimmed.starts_with('*') {
            def.body.push(pl.clone());
        }
        return Ok(());
    }

    // Top level.
    if lower.starts_with(".model") {
        let card = parse_model_card(lineno, raw)?;
        insert_model(&mut col.models, card, lineno, raw)?;
    } else if lower.starts_with(".param") {
        parse_param_card(lineno, raw, &mut col.param_cards)?;
    } else if lower.starts_with(".temp") {
        let toks = tokenize(raw);
        if let Some(t) = toks.get(1) {
            circuit.temp_c = number(lineno, t, raw)?;
        }
    } else if lower.starts_with(".options") || lower.starts_with(".option") {
        parse_options(raw, directives);
    } else if lower.starts_with(".tran") {
        directives.tran = Some(parse_tran(lineno, raw, directives)?);
    } else if lower.starts_with(".ic") {
        col.ic_cards.push(pl.clone());
    } else if lower.starts_with(".nodeset") {
        col.nodeset_cards.push(pl.clone());
    } else if lower.starts_with(".ac") {
        if directives.ac.is_some() {
            return Err(SpiceError::Syntax {
                line: lineno,
                msg: "duplicate `.ac` card (only one AC sweep per deck)".into(),
                text: raw.clone(),
            });
        }
        directives.ac = Some(parse_ac(lineno, raw)?);
    } else if lower.starts_with(".dc") {
        // The swept-source name cannot resolve until flattening; stash the raw
        // card and resolve it against the flattened device table afterward,
        // exactly as `.ic` resolves against the flattened node table.
        col.dc_cards.push(pl.clone());
    } else if lower.starts_with(".print") || lower.starts_with(".plot") {
        let is_plot = lower.starts_with(".plot");
        if is_plot {
            directives.saw_plot = true;
        }
        directives.prints.push(parse_print(lineno, raw, is_plot)?);
    } else if trimmed.is_empty() || trimmed.starts_with('*') || trimmed.starts_with('.') {
        // Other directives (`.end`, `.width`, ...) are consumed elsewhere or out
        // of scope for this step; skip them here.
    } else {
        col.top_elems.push(pl.clone());
    }
    Ok(())
}

// --- element-name references (§2.2) ------------------------------------------

/// A deferred element-name reference: `device`'s control field names `name`,
/// to be resolved once the whole deck is parsed. Generic on purpose — today
/// only F/H defer (their `ctrl_src`), but the behavioral B-source will defer
/// `V(...)`/`I(...)` references through the same pass.
struct NameFixup {
    /// The device whose reference needs patching.
    device: DeviceId,
    /// The referenced element name, as written (matched case-insensitively).
    name: String,
    /// Line of the referring card, for errors.
    line: usize,
    /// Raw card text, for errors.
    raw: String,
    /// Subckt breadcrumb of the referring card (filled by the load loop).
    provenance: String,
}

/// Resolve every [`NameFixup`] against one case-insensitive index of the
/// final (flattened) device names, patching each referring device in place.
///
/// Errors, all line-numbered against the referring card:
/// * the name matches nothing (dangling reference);
/// * the name matches two devices differing only in case (SPICE names are
///   case-insensitive, so this is genuinely ambiguous — refuse, don't pick);
/// * the referent is not an independent `V` source. Every branch-current
///   carrier (an `E`, an inductor, another `H`) is refused with the same
///   pointer: SPICE control semantics are V-source-only, and the zero-volt
///   series ammeter is the idiom that works everywhere. This also settles
///   self-reference for free: an `F`/`H` can never name itself because it is
///   not a `V` source.
fn resolve_name_fixups(circuit: &mut Circuit, fixups: &[NameFixup]) -> Result<(), SpiceError> {
    if fixups.is_empty() {
        return Ok(());
    }
    // name (lowercased) -> every device wearing it. Built once; O(devices).
    let mut index: HashMap<String, Vec<DeviceId>> = HashMap::new();
    for (id, dev) in circuit.iter() {
        index
            .entry(dev.name().to_ascii_lowercase())
            .or_default()
            .push(id);
    }
    for fx in fixups {
        let err = |msg: String| {
            with_provenance(
                SpiceError::Syntax {
                    line: fx.line,
                    msg,
                    text: fx.raw.clone(),
                },
                &fx.provenance,
            )
        };
        let matches = index
            .get(&fx.name.to_ascii_lowercase())
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let target = match matches {
            [] => {
                return Err(err(format!(
                    "controlling source `{}` does not exist in the deck",
                    fx.name
                )))
            }
            [one] => *one,
            many => {
                let names: Vec<&str> = many
                    .iter()
                    .map(|id| circuit.devices[id.0 as usize].name())
                    .collect();
                return Err(err(format!(
                    "controlling source `{}` is ambiguous: matches {} \
                     (SPICE names are case-insensitive; rename one)",
                    fx.name,
                    names.join(", ")
                )));
            }
        };
        if !matches!(circuit.devices[target.0 as usize], Device::Vsource { .. }) {
            return Err(err(format!(
                "controlling source `{}` is not an independent voltage source; \
                 only a V element's branch current can control an F/H — insert \
                 a zero-volt ammeter (`Vsense a b 0`) in series and name that",
                fx.name
            )));
        }
        circuit.devices[fx.device.0 as usize].retarget_controlling_source(target);
    }
    Ok(())
}

// --- parameters & expressions (§4.2) ----------------------------------------

/// One `.param name=value` definition, with the line it came from for errors.
struct ParamCard {
    /// Parameter name, lowercased (SPICE is case-insensitive).
    name: String,
    /// Right-hand side as written: a suffix number, a bare identifier, or an
    /// arithmetic expression (with or without surrounding braces).
    value: String,
    line: usize,
    /// The raw card text, for error messages.
    raw: String,
}

/// Parse a `.param a=1 b={a*2} ...` card, appending each definition.
fn parse_param_card(
    line: usize,
    raw: &str,
    out: &mut Vec<ParamCard>,
) -> Result<(), SpiceError> {
    // Keep `=` so `key=value` pairs (and braced expressions) survive tokenizing.
    let toks = tokenize_kv(raw);
    let mut any = false;
    for tok in &toks[1..] {
        let Some((k, v)) = tok.split_once('=') else {
            // A stray bare token on a `.param` card is a malformed definition.
            return Err(SpiceError::Syntax {
                line,
                msg: format!("`.param` expects `name=value`, found `{tok}`"),
                text: raw.into(),
            });
        };
        if k.is_empty() || v.is_empty() {
            return Err(SpiceError::Syntax {
                line,
                msg: format!("malformed `.param` assignment `{tok}`"),
                text: raw.into(),
            });
        }
        out.push(ParamCard {
            name: k.to_ascii_lowercase(),
            value: v.to_string(),
            line,
            raw: raw.into(),
        });
        any = true;
    }
    if !any {
        return Err(SpiceError::Syntax {
            line,
            msg: "`.param` card defines nothing".into(),
            text: raw.into(),
        });
    }
    Ok(())
}

/// Strip a single layer of surrounding `{ }` from an expression string.
fn strip_braces(s: &str) -> &str {
    let t = s.trim();
    if t.starts_with('{') && t.ends_with('}') && t.len() >= 2 {
        t[1..t.len() - 1].trim()
    } else {
        t
    }
}

/// A strict SPICE value number: the existing lenient parser, but the suffix must
/// be purely alphabetic (a unit like `ohm`/`f`/`h`, or empty). This rejects
/// `1k*2` — a suffix mixed with an operator — so it refuses loudly at the value
/// site instead of silently parsing `1000` and dropping the `*2`.
fn parse_value_number(tok: &str) -> Option<f64> {
    let v = parse_spice_number(tok)?;
    // Recover the suffix the lenient parser skipped over.
    let t = tok.trim();
    let bytes = t.as_bytes();
    let mut i = 0;
    if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
        i += 1;
    }
    let mut seen_dot = false;
    while i < bytes.len() {
        match bytes[i] {
            b'0'..=b'9' => i += 1,
            b'.' if !seen_dot => {
                seen_dot = true;
                i += 1;
            }
            b'e' | b'E' => {
                let mut j = i + 1;
                if j < bytes.len() && (bytes[j] == b'+' || bytes[j] == b'-') {
                    j += 1;
                }
                if j < bytes.len() && bytes[j].is_ascii_digit() {
                    i = j + 1;
                    while i < bytes.len() && bytes[i].is_ascii_digit() {
                        i += 1;
                    }
                }
                break;
            }
            _ => break,
        }
    }
    let suffix = &t[i..];
    if suffix.chars().all(|c| c.is_ascii_alphabetic()) {
        Some(v)
    } else {
        None
    }
}

/// If `tok` is a `{...}` expression, return its interior; else `None`.
fn braced_inner(tok: &str) -> Option<&str> {
    let t = tok.trim();
    if t.starts_with('{') && t.ends_with('}') && t.len() >= 2 {
        Some(t[1..t.len() - 1].trim())
    } else {
        None
    }
}

/// Evaluate an already-parsed expression tree against a parameter environment.
/// Every identifier the expression references must resolve (case-insensitively)
/// in `env`, or it is a line-numbered "undefined parameter" error.
fn eval_tree(
    tree: &EvalNode<DefaultNumericTypes>,
    env: &ParamEnv,
    line: usize,
    raw: &str,
) -> Result<f64, SpiceError> {
    let mut ctx = HashMapContext::<DefaultNumericTypes>::new();
    for ident in tree.iter_variable_identifiers() {
        let key = ident.to_ascii_lowercase();
        let val = env.get(&key).ok_or_else(|| SpiceError::Syntax {
            line,
            msg: format!("expression references undefined parameter `{ident}`"),
            text: raw.into(),
        })?;
        let _ = ctx.set_value(ident.to_string(), Value::from_float(*val));
    }
    match tree.eval_with_context(&ctx) {
        Ok(Value::Float(f)) => Ok(f),
        Ok(Value::Int(i)) => Ok(i as f64),
        Ok(Value::Boolean(b)) => Ok(if b { 1.0 } else { 0.0 }),
        Ok(other) => Err(SpiceError::Syntax {
            line,
            msg: format!("expression did not evaluate to a number (got {other:?})"),
            text: raw.into(),
        }),
        Err(e) => Err(SpiceError::Syntax {
            line,
            msg: format!("expression evaluation failed: {e}"),
            text: raw.into(),
        }),
    }
}

/// Evaluate a scalar right-hand side (a `.param` value, a subckt default, an
/// `X`-line override): an arithmetic expression if `evalexpr` can parse it, else
/// a bare suffix number. Braces are optional and stripped first.
fn eval_scalar(line: usize, s: &str, raw: &str, env: &ParamEnv) -> Result<f64, SpiceError> {
    let inner = strip_braces(s);
    // A bare suffix number (`2k`, `4.7`) is a value, not an expression — try it
    // first, because `evalexpr` would otherwise read the `k` in `2k` as a
    // variable. Only genuinely non-numeric text is handed to the expression
    // parser.
    if let Some(v) = parse_value_number(inner) {
        return Ok(v);
    }
    match build_operator_tree::<DefaultNumericTypes>(inner) {
        Ok(tree) => eval_tree(&tree, env, line, raw),
        Err(_) => Err(SpiceError::BadNumber {
            line,
            tok: s.to_string(),
            text: raw.into(),
        }),
    }
}

/// Evaluate a single element-value TOKEN. A `{expr}` token is arithmetic over
/// the environment; a bare token is a suffix number or (failing that) a
/// parameter name. Unlike [`eval_scalar`], a bare token is NOT treated as an
/// expression — element values use `{...}` for expressions by convention.
fn eval_value(line: usize, tok: &str, raw: &str, env: &ParamEnv) -> Result<f64, SpiceError> {
    if let Some(inner) = braced_inner(tok) {
        let tree =
            build_operator_tree::<DefaultNumericTypes>(inner).map_err(|e| SpiceError::Syntax {
                line,
                msg: format!("malformed expression `{{{inner}}}`: {e}"),
                text: raw.into(),
            })?;
        eval_tree(&tree, env, line, raw)
    } else if let Some(v) = parse_value_number(tok) {
        Ok(v)
    } else if let Some(v) = env.get(&tok.to_ascii_lowercase()) {
        Ok(*v)
    } else {
        Err(SpiceError::BadNumber {
            line,
            tok: tok.to_string(),
            text: raw.into(),
        })
    }
}

/// Topologically resolve a set of `.param`/default definitions against a base
/// environment. Order-independent: a definition is evaluated once all the
/// parameters it references are known. A reference to a name that is neither in
/// the base nor defined here is an undefined-name error; a set that never fully
/// resolves is a cycle. Both carry a line number.
fn resolve_params(cards: &[ParamCard], base: &ParamEnv) -> Result<ParamEnv, SpiceError> {
    let mut env = base.clone();
    let names: HashSet<String> = cards.iter().map(|c| c.name.clone()).collect();
    let mut pending: Vec<usize> = (0..cards.len()).collect();

    loop {
        let mut progressed = false;
        let mut still = Vec::new();
        for &i in &pending {
            let card = &cards[i];
            let inner = strip_braces(&card.value);
            // Bare suffix number first (see `eval_scalar`): `2k` is a value, not
            // an `evalexpr` variable read.
            if let Some(v) = parse_value_number(inner) {
                env.insert(card.name.clone(), v);
                progressed = true;
                continue;
            }
            match build_operator_tree::<DefaultNumericTypes>(inner) {
                Ok(tree) => {
                    let deps: Vec<String> = tree
                        .iter_variable_identifiers()
                        .map(|s| s.to_ascii_lowercase())
                        .collect();
                    // A dependency that is neither resolvable nor a declared
                    // parameter is undefined — report immediately.
                    if let Some(u) = deps
                        .iter()
                        .find(|d| !env.contains_key(*d) && !names.contains(*d))
                    {
                        return Err(SpiceError::Syntax {
                            line: card.line,
                            msg: format!(
                                "`.param {}` references undefined parameter `{u}`",
                                card.name
                            ),
                            text: card.raw.clone(),
                        });
                    }
                    if deps.iter().all(|d| env.contains_key(d)) {
                        let v = eval_tree(&tree, &env, card.line, &card.raw)?;
                        env.insert(card.name.clone(), v);
                        progressed = true;
                    } else {
                        still.push(i);
                    }
                }
                Err(_) => {
                    // Not valid expression syntax: it must be a bare suffix
                    // number (e.g. `1k`). Anything else refuses loudly.
                    let v = parse_value_number(inner).ok_or_else(|| SpiceError::BadNumber {
                        line: card.line,
                        tok: card.value.clone(),
                        text: card.raw.clone(),
                    })?;
                    env.insert(card.name.clone(), v);
                    progressed = true;
                }
            }
        }
        if still.is_empty() {
            break;
        }
        if !progressed {
            // No definition resolved this round, but some remain: they depend on
            // each other (a cycle).
            let cycle: Vec<String> = still.iter().map(|&i| cards[i].name.clone()).collect();
            let first = &cards[still[0]];
            return Err(SpiceError::Syntax {
                line: first.line,
                msg: format!(
                    "`.param` definitions form a dependency cycle: {}",
                    cycle.join(", ")
                ),
                text: first.raw.clone(),
            });
        }
        pending = still;
    }
    Ok(env)
}

// --- subcircuits (§2.4, flatten-at-load) ------------------------------------

/// A parsed `.subckt` block: its formal ports, default parameters, and body.
struct SubcktDef {
    /// Subckt name, as written (case preserved for messages).
    name: String,
    /// Formal port node names, in order.
    ports: Vec<String>,
    /// `(param_lower, raw_value)` defaults, in declaration order.
    defaults: Vec<(String, String)>,
    /// Body cards (elements, nested `X`, and local `.param`), each keeping its
    /// file line and inclusion provenance (a subckt defined in an included file
    /// carries that file's breadcrumb into every spliced instance).
    body: Vec<PhysLine>,
    /// The line of the `.subckt` header, for "never closed" errors.
    def_line: usize,
}

/// One flattened element line ready for the element parser.
struct SplicedLine {
    /// Line to report: the `.subckt` body line for spliced cards, else the file
    /// line for top-level cards.
    lineno: usize,
    /// The (possibly node-mangled) card text.
    text: String,
    /// A breadcrumb appended to error text for spliced cards; empty at top level.
    provenance: String,
    /// The parameter environment this card resolves `{expr}` values against.
    env: Rc<ParamEnv>,
}

/// The maximum subckt nesting depth, a backstop beyond the exact cycle check.
const MAX_SUBCKT_DEPTH: usize = 100;

/// Parse a `.subckt NAME p1 p2 ... [k=v ...]` header.
fn parse_subckt_header(line: usize, raw: &str) -> Result<SubcktDef, SpiceError> {
    let toks = tokenize_kv(raw);
    if toks.len() < 2 {
        return Err(SpiceError::Syntax {
            line,
            msg: "`.subckt` needs a name".into(),
            text: raw.into(),
        });
    }
    let name = toks[1].clone();
    let mut ports = Vec::new();
    let mut defaults = Vec::new();
    // Ports come first; once a `key=value` token appears, the rest are defaults.
    let mut in_params = false;
    for tok in &toks[2..] {
        if let Some((k, v)) = tok.split_once('=') {
            in_params = true;
            if k.is_empty() || v.is_empty() {
                return Err(SpiceError::Syntax {
                    line,
                    msg: format!("malformed `.subckt` default `{tok}`"),
                    text: raw.into(),
                });
            }
            defaults.push((k.to_ascii_lowercase(), v.to_string()));
        } else if in_params {
            return Err(SpiceError::Syntax {
                line,
                msg: format!("`.subckt` port `{tok}` cannot follow a default parameter"),
                text: raw.into(),
            });
        } else {
            ports.push(tok.clone());
        }
    }
    Ok(SubcktDef {
        name,
        ports,
        defaults,
        body: Vec::new(),
        def_line: line,
    })
}

/// Map a body node token through an instance: ground stays ground, a formal
/// port becomes the caller's actual node, an internal node is prefixed.
fn map_node(tok: &str, port_map: &HashMap<String, String>, inst_path: &str) -> String {
    if tok == "0" || tok.eq_ignore_ascii_case("gnd") {
        tok.to_string()
    } else if let Some(actual) = port_map.get(&tok.to_ascii_lowercase()) {
        actual.clone()
    } else {
        format!("{inst_path}.{tok}")
    }
}

/// Token positions that name nodes for a given element letter (name is index 0;
/// values/models follow the nodes). `X` is handled separately. Unknown letters
/// return an empty slice so an unsupported card is still spliced verbatim and
/// then refused by the element parser (with provenance).
fn node_indices_for(kind: char) -> &'static [usize] {
    match kind {
        'R' | 'C' | 'L' | 'V' | 'I' | 'D' => &[1, 2],
        // F/H: only the output pair are nodes — token 3 is an ELEMENT NAME
        // (the controlling V source), rewritten separately in
        // `expand_instance` under the local-scope rule.
        'F' | 'H' => &[1, 2],
        'Q' => &[1, 2, 3],
        'M' | 'S' | 'E' | 'G' => &[1, 2, 3, 4],
        _ => &[],
    }
}

/// Expand one `Xxxx ... NAME [k=v ...]` instantiation into `out`, recursing for
/// nested `X` calls. `chain` is the stack of subckt names currently being
/// expanded, for the self-instantiation cycle check.
#[allow(clippy::too_many_arguments)]
fn expand_instance(
    lineno: usize,
    raw: &str,
    subckts: &HashMap<String, SubcktDef>,
    caller_env: Rc<ParamEnv>,
    caller_origin: &Rc<str>,
    chain: &mut Vec<String>,
    out: &mut Vec<SplicedLine>,
) -> Result<(), SpiceError> {
    let toks = tokenize_kv(raw);
    if toks.len() < 3 {
        return Err(SpiceError::Syntax {
            line: lineno,
            msg: "`X` needs at least one node and a subckt name".into(),
            text: raw.into(),
        });
    }
    let inst_name = toks[0].clone();

    // Split trailing `k=v` params from the positional (node / subckt-name)
    // tokens. The subckt name is the LAST positional token.
    let mut positional: Vec<&str> = Vec::new();
    let mut overrides: Vec<(String, String)> = Vec::new();
    for tok in &toks[1..] {
        if let Some((k, v)) = tok.split_once('=') {
            if k.is_empty() || v.is_empty() {
                return Err(SpiceError::Syntax {
                    line: lineno,
                    msg: format!("malformed parameter `{tok}` on `{inst_name}`"),
                    text: raw.into(),
                });
            }
            overrides.push((k.to_ascii_lowercase(), v.to_string()));
        } else {
            positional.push(tok);
        }
    }
    if positional.len() < 2 {
        return Err(SpiceError::Syntax {
            line: lineno,
            msg: format!("`{inst_name}` needs at least one node and a subckt name"),
            text: raw.into(),
        });
    }
    let subckt_name = positional.pop().unwrap();
    let actual_nodes = positional;

    let def = subckts
        .get(&subckt_name.to_ascii_lowercase())
        .ok_or_else(|| SpiceError::Syntax {
            line: lineno,
            msg: format!("`{inst_name}` references undefined subckt `{subckt_name}`"),
            text: raw.into(),
        })?;

    if actual_nodes.len() != def.ports.len() {
        return Err(SpiceError::Syntax {
            line: lineno,
            msg: format!(
                "`{inst_name}` connects {} nodes but subckt `{}` has {} ports",
                actual_nodes.len(),
                def.name,
                def.ports.len()
            ),
            text: raw.into(),
        });
    }

    // Cycle check: a subckt that instantiates itself, directly or transitively.
    let key = def.name.to_ascii_lowercase();
    if chain.iter().any(|c| c == &key) {
        let mut path = chain.clone();
        path.push(key.clone());
        return Err(SpiceError::Syntax {
            line: lineno,
            msg: format!(
                "subckt instantiation cycle: {} (via `{inst_name}`)",
                path.join(" -> ")
            ),
            text: raw.into(),
        });
    }
    if chain.len() >= MAX_SUBCKT_DEPTH {
        return Err(SpiceError::Syntax {
            line: lineno,
            msg: format!("subckt nesting exceeds depth {MAX_SUBCKT_DEPTH}"),
            text: raw.into(),
        });
    }

    // Instance parameter environment (per-instance; siblings never share).
    // Base = the global params carried by the caller. X-line overrides are
    // evaluated in the CALLER's environment (so a value can thread down) and
    // applied FIRST — an override always wins. Defaults then fill in only the
    // params the caller did not override, evaluated top to bottom against the
    // growing instance env (so a default may reference globals, an override, or
    // an earlier default).
    let mut inst_env: ParamEnv = (*caller_env).clone();
    for (k, v) in &overrides {
        let val = eval_scalar(lineno, v, raw, &caller_env)?;
        inst_env.insert(k.clone(), val);
    }
    for (k, v) in &def.defaults {
        if overrides.iter().any(|(ok, _)| ok == k) {
            continue; // the X-line override takes precedence
        }
        let val = eval_scalar(def.def_line, v, raw, &inst_env)?;
        inst_env.insert(k.clone(), val);
    }
    // Body-local `.param` cards resolve last, in the instance scope.
    let local_cards: Vec<ParamCard> = def
        .body
        .iter()
        .filter(|pl| pl.text.trim_start().to_ascii_lowercase().starts_with(".param"))
        .map(|pl| -> Result<Vec<ParamCard>, SpiceError> {
            let mut tmp = Vec::new();
            parse_param_card(pl.lineno, &pl.text, &mut tmp)?;
            Ok(tmp)
        })
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .flatten()
        .collect();
    let inst_env = Rc::new(resolve_params(&local_cards, &inst_env)?);

    // Port name -> caller's actual node.
    let port_map: HashMap<String, String> = def
        .ports
        .iter()
        .zip(&actual_nodes)
        .map(|(p, a)| (p.to_ascii_lowercase(), a.to_string()))
        .collect();

    // The subckt breadcrumb, plus the inclusion breadcrumb of the `X` call site
    // (empty at top level) so an error names the file the instantiation lives in
    // as well as the subckt. Passed down as the nested caller's origin so deeper
    // errors chain the whole instantiation path.
    let breadcrumb: Rc<str> = Rc::from(
        format!(
            " (in subckt {}, instantiated at line {} as {}{})",
            def.name, lineno, inst_name, caller_origin
        )
        .as_str(),
    );

    chain.push(key);
    for pl in &def.body {
        let blineno = pl.lineno;
        let bline = &pl.text;
        let lower = bline.trim_start().to_ascii_lowercase();
        if lower.starts_with(".param") {
            continue; // folded into inst_env above
        }
        let btoks = tokenize_kv(bline);
        if btoks.is_empty() {
            continue;
        }

        if starts_with_letter(bline, 'x') {
            // Nested instantiation: mangle its name + nodes, then recurse. Its
            // positional tokens are nodes except the last (the subckt name),
            // and `k=v` params pass through unchanged (they resolve in inst_env).
            let mut new_toks = btoks.clone();
            new_toks[0] = format!("{}.{}", inst_name, btoks[0]);
            // Identify positional (non `k=v`) token indices.
            let positional_idx: Vec<usize> = (1..btoks.len())
                .filter(|&i| !btoks[i].contains('='))
                .collect();
            // All positional except the last are nodes to map.
            if let Some((_, node_idxs)) = positional_idx.split_last() {
                for &i in node_idxs {
                    new_toks[i] = map_node(&btoks[i], &port_map, &inst_name);
                }
            }
            let rewritten = new_toks.join(" ");
            expand_instance(
                blineno,
                &rewritten,
                subckts,
                inst_env.clone(),
                &breadcrumb,
                chain,
                out,
            )?;
        } else {
            let kind = bline
                .trim_start()
                .chars()
                .next()
                .unwrap()
                .to_ascii_uppercase();
            let mut new_toks = btoks.clone();
            new_toks[0] = format!("{}.{}", inst_name, btoks[0]);
            for &i in node_indices_for(kind) {
                if i < btoks.len() {
                    new_toks[i] = map_node(&btoks[i], &port_map, &inst_name);
                }
            }
            // F/H control references are LOCAL to the subckt body (see the
            // module doc's scoping rule): prefix the vname with the instance
            // path exactly like a refdes, so `Vsense` written in the body of
            // instance `X3` resolves to the spliced `X3.Vsense`. Unconditional
            // on purpose — no fallback to a same-named global source, so a
            // typo'd local name dangles and fails loudly at resolution instead
            // of silently binding outside the subckt. (Skip `poly`: it must
            // survive verbatim for the parser's refusal to name it.)
            if matches!(kind, 'F' | 'H')
                && btoks.len() > 3
                && !btoks[3].eq_ignore_ascii_case("poly")
            {
                new_toks[3] = format!("{}.{}", inst_name, btoks[3]);
            }
            out.push(SplicedLine {
                lineno: blineno,
                // The breadcrumb (subckt + instantiation site) plus THIS body
                // line's own inclusion origin, so an error names the subckt AND
                // the file the body card physically lives in.
                text: new_toks.join(" "),
                provenance: format!("{}{}", breadcrumb, pl.origin),
                env: inst_env.clone(),
            });
        }
    }
    chain.pop();
    Ok(())
}

/// Insert a `.model` card, hoisting subckt-local models to one global table.
/// An identical redefinition is silently accepted; a conflicting same-name
/// definition refuses loudly (never a silent shadow — honesty doctrine §4.3).
fn insert_model(
    models: &mut HashMap<String, ModelCard>,
    card: ModelCard,
    line: usize,
    raw: &str,
) -> Result<(), SpiceError> {
    let key = card.name.to_ascii_lowercase();
    match models.get(&key) {
        Some(existing) if !existing.same_as(&card) => Err(SpiceError::Syntax {
            line,
            msg: format!(
                "conflicting `.model {}` definitions (same name, different parameters)",
                card.name
            ),
            text: raw.into(),
        }),
        Some(_) => Ok(()), // identical redefinition: harmless
        None => {
            models.insert(key, card);
            Ok(())
        }
    }
}

/// The first whitespace-delimited token of a line (for directive error text).
fn first_token(line: &str) -> &str {
    line.split_whitespace().next().unwrap_or("")
}

/// Whether a card's first non-space character is `letter` (case-insensitive).
fn starts_with_letter(raw: &str, letter: char) -> bool {
    raw.trim_start()
        .chars()
        .next()
        .map(|c| c.eq_ignore_ascii_case(&letter))
        .unwrap_or(false)
}

/// Append a provenance breadcrumb to an error's text field, so a failure inside
/// a spliced subckt body names both the body line (already the error's `line`)
/// and the instantiation site.
fn with_provenance(err: SpiceError, prov: &str) -> SpiceError {
    if prov.is_empty() {
        return err;
    }
    match err {
        SpiceError::Syntax { line, msg, text } => SpiceError::Syntax {
            line,
            msg,
            text: format!("{text}{prov}"),
        },
        SpiceError::UnknownElement { line, ch, text } => SpiceError::UnknownElement {
            line,
            ch,
            text: format!("{text}{prov}"),
        },
        SpiceError::MissingModel { line, model, text } => SpiceError::MissingModel {
            line,
            model,
            text: format!("{text}{prov}"),
        },
        SpiceError::BadNumber { line, tok, text } => SpiceError::BadNumber {
            line,
            tok,
            text: format!("{text}{prov}"),
        },
    }
}

// --- file inclusion (§4.1, `.include` / `.lib`) ------------------------------

/// The maximum `.include`/`.lib` nesting depth (a backstop beyond the exact
/// cycle check).
pub const MAX_INCLUDE_DEPTH: usize = 50;

/// One physical source line tagged with where it came from. `origin` is a
/// provenance breadcrumb (empty for the top deck) appended to any error raised
/// from this line, so a failure inside an included file names the file, its
/// own line, and the inclusion site — the same discipline subckt splicing uses.
#[derive(Clone)]
struct PhysLine {
    /// Line number within the file the line came from (1-based).
    lineno: usize,
    /// The raw physical text (post-continuation-join for logical lines).
    text: String,
    /// Inclusion breadcrumb; empty for the top deck.
    origin: Rc<str>,
}

/// State threaded through the recursive include expansion.
struct IncludeCtx {
    /// The top deck's directory — the second search location for every include.
    top_dir: PathBuf,
    /// Canonicalized paths currently open on the include stack (cycle check).
    stack: Vec<PathBuf>,
}

/// Build a line-numbered [`SpiceError::Syntax`] carrying an inclusion breadcrumb.
fn provenanced_syntax(line: usize, raw: &str, origin: &Rc<str>, msg: String) -> SpiceError {
    with_provenance(
        SpiceError::Syntax {
            line,
            msg,
            text: raw.to_string(),
        },
        origin,
    )
}

/// Split a `.include`/`.lib` argument list: a possibly quoted path, optionally
/// followed by a bare section token. Quotes let a path contain spaces.
fn parse_directive_args(rest: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut chars = rest.trim().chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
            continue;
        }
        if c == '"' || c == '\'' {
            let q = c;
            chars.next();
            let mut s = String::new();
            for ch in chars.by_ref() {
                if ch == q {
                    break;
                }
                s.push(ch);
            }
            args.push(s);
        } else {
            let mut s = String::new();
            while let Some(&ch) = chars.peek() {
                if ch.is_whitespace() {
                    break;
                }
                s.push(ch);
                chars.next();
            }
            args.push(s);
        }
    }
    args
}

/// Resolve an include path: the including file's directory first, then the top
/// deck's directory; an absolute path is used as-is. On failure returns the
/// list of paths tried (for a not-found error).
fn resolve_include(arg: &str, this_dir: &Path, top_dir: &Path) -> Result<PathBuf, Vec<PathBuf>> {
    let p = Path::new(arg);
    if p.is_absolute() {
        return if p.is_file() {
            Ok(p.to_path_buf())
        } else {
            Err(vec![p.to_path_buf()])
        };
    }
    let mut attempts = Vec::new();
    for base in [this_dir, top_dir] {
        let cand = base.join(p);
        if cand.is_file() {
            return Ok(cand);
        }
        if !attempts.contains(&cand) {
            attempts.push(cand);
        }
    }
    Err(attempts)
}

/// Read a source file's lines, expanding `.include`/`.lib` inline into `out`.
/// `is_top` drops the SPICE title line (line 1) of the TOP deck only — included
/// files have no title. `origin` is the breadcrumb for lines from this file.
fn read_source(
    text: &str,
    this_dir: &Path,
    this_name: &str,
    origin: Rc<str>,
    is_top: bool,
    ctx: &mut IncludeCtx,
    out: &mut Vec<PhysLine>,
) -> Result<(), SpiceError> {
    for (i, raw) in text.lines().enumerate() {
        let lineno = i + 1;
        if is_top && lineno == 1 {
            continue; // SPICE title line (top deck only)
        }
        let trimmed = raw.trim_start();
        let tok = first_token(trimmed);
        if tok.eq_ignore_ascii_case(".include") || tok.eq_ignore_ascii_case(".inc") {
            let args = parse_directive_args(&trimmed[tok.len()..]);
            include_file(&args, lineno, raw, this_dir, this_name, &origin, ctx, out)?;
        } else if tok.eq_ignore_ascii_case(".lib") {
            let args = parse_directive_args(&trimmed[tok.len()..]);
            lib_call(&args, lineno, raw, this_dir, this_name, &origin, ctx, out)?;
        } else if tok.eq_ignore_ascii_case(".endl") {
            return Err(provenanced_syntax(
                lineno,
                raw,
                &origin,
                "`.endl` without an open `.lib` section".into(),
            ));
        } else {
            out.push(PhysLine {
                lineno,
                text: raw.to_string(),
                origin: origin.clone(),
            });
        }
    }
    Ok(())
}

/// Handle a `.include <file>` card.
#[allow(clippy::too_many_arguments)]
fn include_file(
    args: &[String],
    site: usize,
    raw: &str,
    this_dir: &Path,
    this_name: &str,
    origin: &Rc<str>,
    ctx: &mut IncludeCtx,
    out: &mut Vec<PhysLine>,
) -> Result<(), SpiceError> {
    if args.len() != 1 {
        return Err(provenanced_syntax(
            site,
            raw,
            origin,
            format!("`.include` takes exactly one file path (got {})", args.len()),
        ));
    }
    let path = resolve_include(&args[0], this_dir, &ctx.top_dir).map_err(|attempts| {
        provenanced_syntax(
            site,
            raw,
            origin,
            format!(
                "`.include` file `{}` not found (tried: {})",
                args[0],
                attempts
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        )
    })?;
    let child_origin: Rc<str> = Rc::from(
        format!(
            " (in included file \"{}\", included from {} line {})",
            args[0], this_name, site
        )
        .as_str(),
    );
    splice_file(&path, &args[0], child_origin, None, site, raw, origin, ctx, out)
}

/// Handle a `.lib <file> <section>` call — or refuse the ambiguous one-arg form.
#[allow(clippy::too_many_arguments)]
fn lib_call(
    args: &[String],
    site: usize,
    raw: &str,
    this_dir: &Path,
    this_name: &str,
    origin: &Rc<str>,
    ctx: &mut IncludeCtx,
    out: &mut Vec<PhysLine>,
) -> Result<(), SpiceError> {
    match args.len() {
        2 => {
            let (file, section) = (&args[0], &args[1]);
            let path = resolve_include(file, this_dir, &ctx.top_dir).map_err(|attempts| {
                provenanced_syntax(
                    site,
                    raw,
                    origin,
                    format!(
                        "`.lib` file `{}` not found (tried: {})",
                        file,
                        attempts
                            .iter()
                            .map(|p| p.display().to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                )
            })?;
            let child_origin: Rc<str> = Rc::from(
                format!(
                    " (from .lib section \"{}\" of file \"{}\", included from {} line {})",
                    section, file, this_name, site
                )
                .as_str(),
            );
            splice_file(&path, file, child_origin, Some(section), site, raw, origin, ctx, out)
        }
        1 => Err(provenanced_syntax(
            site,
            raw,
            origin,
            format!(
                "one-argument `.lib {0}` is ambiguous: use `.include {0}` to pull the whole \
                 file, or `.lib {0} <section>` to pull a named `.lib`/`.endl` section",
                args[0]
            ),
        )),
        n => Err(provenanced_syntax(
            site,
            raw,
            origin,
            format!("`.lib` takes `<file> <section>` (got {n} arguments)"),
        )),
    }
}

/// Read `path`, guarding cycles/depth, then dispatch to whole-file or
/// section-only splicing.
#[allow(clippy::too_many_arguments)]
fn splice_file(
    path: &Path,
    display_arg: &str,
    child_origin: Rc<str>,
    section: Option<&str>,
    site: usize,
    site_raw: &str,
    site_origin: &Rc<str>,
    ctx: &mut IncludeCtx,
    out: &mut Vec<PhysLine>,
) -> Result<(), SpiceError> {
    let canon = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if ctx.stack.contains(&canon) {
        let chain = ctx
            .stack
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(" -> ");
        return Err(provenanced_syntax(
            site,
            site_raw,
            site_origin,
            format!(
                "include cycle: `{}` is already open ({} -> {})",
                display_arg,
                chain,
                canon.display()
            ),
        ));
    }
    if ctx.stack.len() >= MAX_INCLUDE_DEPTH {
        return Err(provenanced_syntax(
            site,
            site_raw,
            site_origin,
            format!("include nesting exceeds depth {MAX_INCLUDE_DEPTH} at `{display_arg}`"),
        ));
    }
    let text = std::fs::read_to_string(path).map_err(|e| {
        provenanced_syntax(
            site,
            site_raw,
            site_origin,
            format!("cannot read included file `{}`: {e}", path.display()),
        )
    })?;
    let child_dir = path.parent().unwrap_or_else(|| Path::new(".")).to_path_buf();
    let child_name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    ctx.stack.push(canon);
    let r = match section {
        Some(sec) => read_section(&text, &child_dir, &child_name, sec, display_arg, child_origin, ctx, out),
        None => read_source(&text, &child_dir, &child_name, child_origin, false, ctx, out),
    };
    ctx.stack.pop();
    r
}

/// Splice ONLY the named `.lib <section> ... .endl` block from a library file.
/// Nested `.include`/`.lib <file> <section>` calls inside the section expand;
/// nested `.lib <section>` DEFINITIONS are refused (unsupported).
#[allow(clippy::too_many_arguments)]
fn read_section(
    text: &str,
    this_dir: &Path,
    this_name: &str,
    section: &str,
    file_arg: &str,
    origin: Rc<str>,
    ctx: &mut IncludeCtx,
    out: &mut Vec<PhysLine>,
) -> Result<(), SpiceError> {
    let mut available: Vec<String> = Vec::new();
    let mut in_section = false;
    let mut found = false;
    for (i, raw) in text.lines().enumerate() {
        let lineno = i + 1;
        let trimmed = raw.trim_start();
        let tok = first_token(trimmed);
        if tok.eq_ignore_ascii_case(".lib") {
            let args = parse_directive_args(&trimmed[tok.len()..]);
            if args.len() == 1 {
                // A section-open inside the library file.
                if in_section {
                    return Err(provenanced_syntax(
                        lineno,
                        raw,
                        &origin,
                        "nested `.lib` sections are unsupported".into(),
                    ));
                }
                available.push(args[0].clone());
                if args[0].eq_ignore_ascii_case(section) {
                    in_section = true;
                    found = true;
                }
                continue;
            } else if in_section && args.len() == 2 {
                // A `.lib file section` CALL inside our section: expand it.
                let path = resolve_include(&args[0], this_dir, &ctx.top_dir).map_err(|att| {
                    provenanced_syntax(
                        lineno,
                        raw,
                        &origin,
                        format!(
                            "`.lib` file `{}` not found (tried: {})",
                            args[0],
                            att.iter()
                                .map(|p| p.display().to_string())
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                    )
                })?;
                let child_origin: Rc<str> = Rc::from(
                    format!(
                        " (from .lib section \"{}\" of file \"{}\", included from {} line {})",
                        args[1], args[0], this_name, lineno
                    )
                    .as_str(),
                );
                splice_file(&path, &args[0], child_origin, Some(&args[1]), lineno, raw, &origin, ctx, out)?;
                continue;
            } else if in_section {
                return Err(provenanced_syntax(
                    lineno,
                    raw,
                    &origin,
                    format!("malformed `.lib` inside section `{section}`"),
                ));
            } else {
                continue; // belongs to a section we are not pulling
            }
        }
        if tok.eq_ignore_ascii_case(".endl") {
            if in_section {
                return Ok(()); // our section closed
            }
            continue; // `.endl` for a section we are not pulling
        }
        if tok.eq_ignore_ascii_case(".include") || tok.eq_ignore_ascii_case(".inc") {
            if in_section {
                let args = parse_directive_args(&trimmed[tok.len()..]);
                include_file(&args, lineno, raw, this_dir, this_name, &origin, ctx, out)?;
            }
            continue;
        }
        if in_section {
            out.push(PhysLine {
                lineno,
                text: raw.to_string(),
                origin: origin.clone(),
            });
        }
    }
    if !found {
        return Err(provenanced_syntax(
            0,
            "",
            &origin,
            format!(
                "`.lib` section `{}` not found in file `{}`; available sections: {}",
                section,
                file_arg,
                if available.is_empty() {
                    "(none)".to_string()
                } else {
                    available.join(", ")
                }
            ),
        ));
    }
    Err(provenanced_syntax(
        0,
        "",
        &origin,
        format!("`.lib` section `{section}` in file `{file_arg}` is not closed with `.endl`"),
    ))
}

// --- .ic / .nodeset (§4.1) ---------------------------------------------------

/// Parse a `.ic`/`.nodeset` card's `V(node)=value` groups, resolving each node
/// against the (flattened) circuit and evaluating each value against `env`.
/// A bare (un-provenanced) error; the caller wraps it with the card's breadcrumb.
fn parse_ic_values(
    pl: &PhysLine,
    circuit: &Circuit,
    env: &ParamEnv,
    what: &str,
    out: &mut Vec<(NodeId, f64)>,
) -> Result<(), SpiceError> {
    // `tokenize` drops `(`, `)`, `,`, `=`, keeping `{expr}` atomic, so
    // `.ic V(out)={a*2} V(b)=1` becomes [".ic","V","out","{a*2}","V","b","1"].
    let toks = tokenize(&pl.text);
    let syn = |msg: String| SpiceError::Syntax {
        line: pl.lineno,
        msg,
        text: pl.text.clone(),
    };
    let mut i = 1;
    let mut any = false;
    while i < toks.len() {
        if !toks[i].eq_ignore_ascii_case("v") {
            return Err(syn(format!(
                "{what} expects `V(node)=value` groups, found `{}`",
                toks[i]
            )));
        }
        let node = toks
            .get(i + 1)
            .ok_or_else(|| syn(format!("{what}: `V(` without a node name")))?;
        let valtok = toks
            .get(i + 2)
            .ok_or_else(|| syn(format!("{what}: `V({node})` without a value")))?;
        let value = eval_value(pl.lineno, valtok, &pl.text, env)?;
        let nid = circuit
            .find_node(node)
            .ok_or_else(|| syn(format!(
                "{what} references unknown node `{node}`{}",
                did_you_mean(circuit, node)
            )))?;
        out.push((nid, value));
        any = true;
        i += 3;
    }
    if !any {
        return Err(syn(format!("{what} card sets nothing")));
    }
    Ok(())
}

/// Suggest close node names for an unresolved `.ic`/`.nodeset` reference. Empty
/// when nothing is within a small edit distance.
fn did_you_mean(circuit: &Circuit, target: &str) -> String {
    let t = target.to_ascii_lowercase();
    let mut cands: Vec<String> = circuit
        .node_names()
        .filter(|n| *n != "0")
        .filter(|n| {
            let nl = n.to_ascii_lowercase();
            levenshtein(&nl, &t) <= 2
        })
        .map(|s| s.to_string())
        .collect();
    cands.sort();
    cands.dedup();
    if cands.is_empty() {
        String::new()
    } else {
        format!(
            " (did you mean {}?)",
            cands
                .iter()
                .map(|c| format!("`{c}`"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

/// Iterative Levenshtein edit distance (small strings; node names).
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, &ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            cur[j + 1] = (prev[j + 1] + 1).min(cur[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

// --- line joining -----------------------------------------------------------

/// Strip comments and join `+` continuation lines across the already-expanded
/// physical-line list. The SPICE title line was dropped during inclusion
/// (top-deck line 1), so there is none to drop here. Each logical line keeps
/// the file line number and inclusion breadcrumb of its FIRST physical line.
fn join_continuations(phys: &[PhysLine]) -> Vec<PhysLine> {
    let mut out: Vec<PhysLine> = Vec::new();
    for pl in phys {
        // Inline `;` and trailing `$` comments are stripped; full-line `*` too.
        let stripped = strip_inline_comment(&pl.text);
        let t = stripped.trim_end();
        if t.trim_start().starts_with('+') {
            if let Some(last) = out.last_mut() {
                let cont = t.trim_start().trim_start_matches('+');
                last.text.push(' ');
                last.text.push_str(cont.trim());
                continue;
            }
        }
        if t.trim().is_empty() {
            continue;
        }
        out.push(PhysLine {
            lineno: pl.lineno,
            text: t.to_string(),
            origin: pl.origin.clone(),
        });
    }
    out
}

fn strip_inline_comment(line: &str) -> String {
    // `;` starts a comment anywhere; `$ ` (dollar-space) is the ngspice style.
    let mut result = line;
    if let Some(idx) = result.find(';') {
        result = &result[..idx];
    }
    if let Some(idx) = result.find("$ ") {
        result = &result[..idx];
    }
    result.to_string()
}

// --- tokenizing & numbers ---------------------------------------------------

/// Split a line into tokens, treating whitespace, `(`, `)`, `,` (and, when
/// `keep_eq` is false, `=`) as separators that vanish. A `{...}` curly-brace
/// expression is kept ATOMIC — its interior (which may contain spaces, parens,
/// `=`, and operators, e.g. `{ (a+b) * 2 }`) is copied verbatim as one token,
/// braces included, so downstream can recognize and evaluate it. Nesting is
/// tracked so nested braces do not close early.
fn split_tokens(line: &str, keep_eq: bool) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut depth: i32 = 0;
    for ch in line.chars() {
        if ch == '{' {
            depth += 1;
            cur.push(ch);
        } else if ch == '}' {
            depth = (depth - 1).max(0);
            cur.push(ch);
        } else if depth > 0 {
            // Inside an expression: preserve everything verbatim.
            cur.push(ch);
        } else {
            let sep = ch.is_whitespace()
                || ch == '('
                || ch == ')'
                || ch == ','
                || (!keep_eq && ch == '=');
            if sep {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            } else {
                cur.push(ch);
            }
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Split a line on whitespace, treating `=`, `(`, `)`, and `,` as separators
/// that vanish so `pulse(0 5 ...)` and `tc1=0.01` tokenize cleanly. `{expr}`
/// stays a single token.
fn tokenize(line: &str) -> Vec<String> {
    split_tokens(line, false)
}

/// Tokenize while keeping `key=value` pairs intact (for `.model`/`.subckt`/`X`
/// parameters). `{expr}` stays a single token.
fn tokenize_kv(line: &str) -> Vec<String> {
    split_tokens(line, true)
}

/// Parse a SPICE number with optional engineering suffix.
fn parse_spice_number(tok: &str) -> Option<f64> {
    let t = tok.trim();
    if t.is_empty() {
        return None;
    }
    // Find where the numeric prefix ends.
    let bytes = t.as_bytes();
    let mut i = 0;
    if bytes[i] == b'+' || bytes[i] == b'-' {
        i += 1;
    }
    let mut seen_digit = false;
    let mut seen_dot = false;
    while i < bytes.len() {
        match bytes[i] {
            b'0'..=b'9' => {
                seen_digit = true;
                i += 1;
            }
            b'.' if !seen_dot => {
                seen_dot = true;
                i += 1;
            }
            b'e' | b'E' => {
                // Exponent: e[+/-]digits, but only if followed by a digit/sign.
                let mut j = i + 1;
                if j < bytes.len() && (bytes[j] == b'+' || bytes[j] == b'-') {
                    j += 1;
                }
                if j < bytes.len() && bytes[j].is_ascii_digit() {
                    i = j + 1;
                    while i < bytes.len() && bytes[i].is_ascii_digit() {
                        i += 1;
                    }
                }
                break;
            }
            _ => break,
        }
    }
    if !seen_digit {
        return None;
    }
    let value: f64 = t[..i].parse().ok()?;
    let suffix = t[i..].to_ascii_lowercase();
    let mult = scale_suffix(&suffix);
    Some(value * mult)
}

/// Engineering-suffix multiplier. Order matters: `meg`/`mil` before `m`.
fn scale_suffix(suffix: &str) -> f64 {
    if suffix.is_empty() {
        return 1.0;
    }
    // Match the longest known prefix; trailing junk (units like "ohm") ignored.
    let s = suffix;
    if s.starts_with("meg") {
        1e6
    } else if s.starts_with("mil") {
        25.4e-6
    } else if s.starts_with('t') {
        1e12
    } else if s.starts_with('g') {
        1e9
    } else if s.starts_with('k') {
        1e3
    } else if s.starts_with('m') {
        1e-3
    } else if s.starts_with('u') {
        1e-6
    } else if s.starts_with('n') {
        1e-9
    } else if s.starts_with('p') {
        1e-12
    } else if s.starts_with('f') {
        1e-15
    } else if s.starts_with('a') {
        1e-18
    } else {
        1.0
    }
}

fn number(line: usize, tok: &str, text: &str) -> Result<f64, SpiceError> {
    parse_spice_number(tok).ok_or_else(|| SpiceError::BadNumber {
        line,
        tok: tok.to_string(),
        text: text.to_string(),
    })
}

// --- .model -----------------------------------------------------------------

#[derive(Debug, Clone)]
struct ModelCard {
    name: String,
    kind: String,
    params: HashMap<String, f64>,
    /// Raw type keyword like `npn`, `pnp`, `nmos`, `pmos` (for polarity).
    type_word: Option<String>,
}

impl ModelCard {
    fn get(&self, key: &str) -> Option<f64> {
        self.params.get(key).copied()
    }
    fn get_or(&self, key: &str, default: f64) -> f64 {
        self.get(key).unwrap_or(default)
    }
    /// Whether two model cards are the same definition (for the hoist collision
    /// check): same kind, same type keyword, and the same parameter set.
    fn same_as(&self, other: &ModelCard) -> bool {
        self.kind == other.kind && self.type_word == other.type_word && self.params == other.params
    }
}

fn parse_model_card(line: usize, raw: &str) -> Result<ModelCard, SpiceError> {
    // .model NAME TYPE(p1=v1 p2=v2 ...)
    let toks = tokenize_kv(raw);
    if toks.len() < 3 {
        return Err(SpiceError::Syntax {
            line,
            msg: "incomplete .model card".into(),
            text: raw.into(),
        });
    }
    let name = toks[1].clone();
    let type_full = toks[2].to_ascii_lowercase();
    // The type token may be glued to the first param if no space: handled by
    // tokenize_kv stripping parens, so toks[2] is the type keyword.
    let kind = classify_model(&type_full);

    let mut params = HashMap::new();
    for tok in &toks[3..] {
        if let Some((k, v)) = tok.split_once('=') {
            if let Some(num) = parse_spice_number(v) {
                params.insert(k.to_ascii_lowercase(), num);
            }
        }
    }
    Ok(ModelCard {
        name,
        kind,
        params,
        type_word: Some(type_full),
    })
}

fn classify_model(type_word: &str) -> String {
    match type_word {
        "d" => "d".into(),
        "npn" | "pnp" => "bjt".into(),
        "nmos" | "pmos" => "mos".into(),
        "sw" | "vswitch" => "sw".into(),
        other => other.into(),
    }
}

// --- elements ---------------------------------------------------------------

fn parse_element(
    line: usize,
    raw: &str,
    circuit: &mut Circuit,
    models: &HashMap<String, ModelCard>,
    env: &ParamEnv,
    fixups: &mut Vec<NameFixup>,
) -> Result<(), SpiceError> {
    let toks = tokenize(raw);
    if toks.is_empty() {
        return Ok(());
    }
    let name = toks[0].clone();
    // The element type is the first letter of the refdes. After subckt
    // flattening a refdes is instance-qualified (`X1.R1`), so the type letter is
    // the first character of the final dot-segment, not of the whole name.
    let seg = name.rsplit('.').next().unwrap_or(&name);
    let kind = seg
        .chars()
        .next()
        .map(|c| c.to_ascii_uppercase())
        .unwrap_or(' ');

    match kind {
        'R' => parse_rcl(line, raw, &toks, circuit, RclKind::R, env),
        'C' => parse_rcl(line, raw, &toks, circuit, RclKind::C, env),
        'L' => parse_rcl(line, raw, &toks, circuit, RclKind::L, env),
        'V' => parse_source(line, raw, &toks, circuit, true, env),
        'I' => parse_source(line, raw, &toks, circuit, false, env),
        'D' => parse_diode(line, raw, &toks, circuit, models),
        'Q' => parse_bjt(line, raw, &toks, circuit, models),
        'M' => parse_mosfet(line, raw, &toks, circuit, models),
        'S' => parse_switch(line, raw, &toks, circuit, models),
        'E' => parse_controlled(line, raw, &toks, circuit, true, env),
        'G' => parse_controlled(line, raw, &toks, circuit, false, env),
        'F' => parse_current_controlled(line, raw, &toks, circuit, true, env, fixups),
        'H' => parse_current_controlled(line, raw, &toks, circuit, false, env, fixups),
        other => Err(SpiceError::UnknownElement {
            line,
            ch: other,
            text: raw.into(),
        }),
    }
}

enum RclKind {
    R,
    C,
    L,
}

fn parse_rcl(
    line: usize,
    raw: &str,
    toks: &[String],
    circuit: &mut Circuit,
    kind: RclKind,
    env: &ParamEnv,
) -> Result<(), SpiceError> {
    if toks.len() < 4 {
        return Err(SpiceError::Syntax {
            line,
            msg: "need name, two nodes, and a value".into(),
            text: raw.into(),
        });
    }
    let a = circuit.node(&toks[1]);
    let b = circuit.node(&toks[2]);
    let value = eval_value(line, &toks[3], raw, env)?;
    let name = toks[0].clone();

    // Trailing key=value options (tc1=, ic=) — re-scan with `=` kept.
    let kv = scan_trailing_kv(raw);

    let device = match kind {
        RclKind::R => Device::Resistor {
            name,
            a,
            b,
            ohms: value,
            tc1: kv.get("tc1").copied().or_else(|| kv.get("tc").copied()),
        },
        RclKind::C => Device::Capacitor {
            name,
            a,
            b,
            farads: value,
            ic: kv.get("ic").copied(),
        },
        RclKind::L => Device::Inductor {
            name,
            a,
            b,
            henries: value,
            ic: kv.get("ic").copied(),
        },
    };
    circuit.add(device);
    Ok(())
}

/// Collect trailing `key=value` pairs from a raw line (SPICE numbers).
fn scan_trailing_kv(raw: &str) -> HashMap<String, f64> {
    let mut map = HashMap::new();
    for tok in raw.split_whitespace() {
        if let Some((k, v)) = tok.split_once('=') {
            if let Some(num) = parse_spice_number(v) {
                map.insert(k.to_ascii_lowercase(), num);
            }
        }
    }
    map
}

fn parse_source(
    line: usize,
    raw: &str,
    toks: &[String],
    circuit: &mut Circuit,
    is_voltage: bool,
    env: &ParamEnv,
) -> Result<(), SpiceError> {
    if toks.len() < 3 {
        return Err(SpiceError::Syntax {
            line,
            msg: "need name and two nodes".into(),
            text: raw.into(),
        });
    }
    let p = circuit.node(&toks[1]);
    let n = circuit.node(&toks[2]);
    let name = toks[0].clone();
    // Peel off the `AC <mag> [phase]` small-signal stimulus before parsing the
    // time-domain function: previously the AC keyword was silently dropped, so a
    // `.ac` analysis had no honest drive.
    let (ac, kind_toks) = extract_ac_spec(&toks[3..], env);
    let kind = parse_source_kind(line, raw, &kind_toks, env)?;

    let device = if is_voltage {
        Device::Vsource { name, p, n, kind }
    } else {
        Device::Isource { name, p, n, kind }
    };
    let id = circuit.add(device);
    if let Some(stim) = ac {
        circuit.ac_stimulus.push((id, stim));
    }
    Ok(())
}

fn parse_source_kind(
    line: usize,
    raw: &str,
    rest: &[String],
    env: &ParamEnv,
) -> Result<SourceKind, SpiceError> {
    if rest.is_empty() {
        return Ok(SourceKind::Dc(0.0));
    }
    // KiCad and vendor netlists combine specs: `DC 0 SIN( 0 1 1k ) AC 1`.
    // The transient function wins for us, wherever it sits in the line.
    if let Some(pos) = rest.iter().skip(1).position(|t| {
        matches!(
            t.to_ascii_lowercase().as_str(),
            "sin" | "sine" | "pulse" | "pwl"
        )
    }) {
        return parse_source_kind(line, raw, &rest[pos + 1..], env);
    }
    let head = rest[0].to_ascii_lowercase();
    // `Vx n+ n- DC 5`, `Vx n+ n- 5`, or a function.
    match head.as_str() {
        "dc" => {
            let v = rest
                .get(1)
                .map(|t| eval_value(line, t, raw, env))
                .transpose()?
                .unwrap_or(0.0);
            Ok(SourceKind::Dc(v))
        }
        "sin" | "sine" => {
            let nums = number_args(line, raw, &rest[1..], env)?;
            Ok(SourceKind::Sin {
                offset: nums.first().copied().unwrap_or(0.0),
                amplitude: nums.get(1).copied().unwrap_or(0.0),
                freq: nums.get(2).copied().unwrap_or(0.0),
                delay: nums.get(3).copied().unwrap_or(0.0),
                theta: nums.get(4).copied().unwrap_or(0.0),
                phase: nums.get(5).copied().unwrap_or(0.0),
            })
        }
        "pulse" => {
            let nums = number_args(line, raw, &rest[1..], env)?;
            Ok(SourceKind::Pulse {
                v1: nums.first().copied().unwrap_or(0.0),
                v2: nums.get(1).copied().unwrap_or(0.0),
                delay: nums.get(2).copied().unwrap_or(0.0),
                rise: nums.get(3).copied().unwrap_or(0.0),
                fall: nums.get(4).copied().unwrap_or(0.0),
                width: nums.get(5).copied().unwrap_or(f64::INFINITY),
                period: nums.get(6).copied().unwrap_or(0.0),
            })
        }
        "pwl" => {
            let nums = number_args(line, raw, &rest[1..], env)?;
            let mut points = Vec::new();
            for pair in nums.chunks(2) {
                if pair.len() == 2 {
                    points.push(PwlPoint {
                        t: pair[0],
                        v: pair[1],
                    });
                }
            }
            Ok(SourceKind::Pwl(points))
        }
        _ => {
            // Bare numeric value: `Vx a b 5`.
            let v = eval_value(line, &rest[0], raw, env)?;
            Ok(SourceKind::Dc(v))
        }
    }
}

/// Convert tokens to numbers, skipping trailing AC/transient spec keywords. A
/// `{expr}` token is evaluated against the parameter environment (a malformed
/// one errors); a bare token is a suffix number or a parameter name; anything
/// else stops the scan once at least one number has been read.
fn number_args(
    line: usize,
    raw: &str,
    toks: &[String],
    env: &ParamEnv,
) -> Result<Vec<f64>, SpiceError> {
    let mut out = Vec::new();
    for t in toks {
        if braced_inner(t).is_some() {
            // An expression is an explicit value: evaluate or error.
            out.push(eval_value(line, t, raw, env)?);
            continue;
        }
        match parse_spice_number(t).or_else(|| env.get(&t.to_ascii_lowercase()).copied()) {
            Some(v) => out.push(v),
            None => {
                if out.is_empty() {
                    return Err(SpiceError::BadNumber {
                        line,
                        tok: t.clone(),
                        text: raw.into(),
                    });
                }
                break;
            }
        }
    }
    Ok(out)
}

fn parse_diode(
    line: usize,
    raw: &str,
    toks: &[String],
    circuit: &mut Circuit,
    models: &HashMap<String, ModelCard>,
) -> Result<(), SpiceError> {
    if toks.len() < 4 {
        return Err(SpiceError::Syntax {
            line,
            msg: "need anode, cathode, model".into(),
            text: raw.into(),
        });
    }
    let a = circuit.node(&toks[1]);
    let k = circuit.node(&toks[2]);
    let model_name = &toks[3];
    // Only bind a card whose type is actually a diode; otherwise fall back to
    // defaults so a mistyped reference doesn't silently inherit BJT params.
    let card = models
        .get(&model_name.to_ascii_lowercase())
        .filter(|c| c.kind == "d");
    let model = diode_from_card(card);
    circuit.add(Device::Diode {
        name: toks[0].clone(),
        a,
        k,
        model,
    });
    let _ = (line, raw);
    Ok(())
}

fn diode_from_card(card: Option<&ModelCard>) -> DiodeModel {
    let d = DiodeModel::default();
    match card {
        None => d,
        Some(c) => DiodeModel {
            is: c.get_or("is", d.is),
            n: c.get_or("n", d.n),
            rs: c.get_or("rs", d.rs),
            cjo: c.get("cjo").or_else(|| c.get("cj0")).unwrap_or(d.cjo),
            vj: c.get("vj").or_else(|| c.get("pb")).unwrap_or(d.vj),
            m: c.get_or("m", d.m),
            tt: c.get_or("tt", d.tt),
            bv: c.get("bv").unwrap_or(d.bv),
            xti: c.get_or("xti", d.xti),
            eg: c.get_or("eg", d.eg),
        },
    }
}

fn parse_bjt(
    line: usize,
    raw: &str,
    toks: &[String],
    circuit: &mut Circuit,
    models: &HashMap<String, ModelCard>,
) -> Result<(), SpiceError> {
    if toks.len() < 5 {
        return Err(SpiceError::Syntax {
            line,
            msg: "need collector, base, emitter, model".into(),
            text: raw.into(),
        });
    }
    let c = circuit.node(&toks[1]);
    let b = circuit.node(&toks[2]);
    let e = circuit.node(&toks[3]);
    let model_name = &toks[4];
    let card = models
        .get(&model_name.to_ascii_lowercase())
        .ok_or_else(|| SpiceError::MissingModel {
            line,
            model: model_name.clone(),
            text: raw.into(),
        })?;
    let model = bjt_from_card(card);
    circuit.add(Device::Bjt {
        name: toks[0].clone(),
        c,
        b,
        e,
        model,
    });
    Ok(())
}

fn bjt_from_card(card: &ModelCard) -> BjtModel {
    let d = BjtModel::default();
    let polarity = match card.type_word.as_deref() {
        Some("pnp") => Polarity::P,
        _ => Polarity::N,
    };
    BjtModel {
        polarity,
        is: card.get_or("is", d.is),
        bf: card.get_or("bf", d.bf),
        br: card.get_or("br", d.br),
        vaf: card.get("vaf").or_else(|| card.get("va")).unwrap_or(d.vaf),
        var: card.get("var").or_else(|| card.get("vb")).unwrap_or(d.var),
        nf: card.get_or("nf", d.nf),
        nr: card.get_or("nr", d.nr),
        rb: card.get_or("rb", d.rb),
        re: card.get_or("re", d.re),
        rc: card.get_or("rc", d.rc),
        cje: card.get_or("cje", d.cje),
        cjc: card.get_or("cjc", d.cjc),
        tf: card.get_or("tf", d.tf),
        tr: card.get_or("tr", d.tr),
        xti: card.get_or("xti", d.xti),
        eg: card.get_or("eg", d.eg),
    }
}

fn parse_mosfet(
    line: usize,
    raw: &str,
    toks: &[String],
    circuit: &mut Circuit,
    models: &HashMap<String, ModelCard>,
) -> Result<(), SpiceError> {
    // M<name> d g s b model [L=.. W=..]
    if toks.len() < 6 {
        return Err(SpiceError::Syntax {
            line,
            msg: "need drain, gate, source, bulk, model".into(),
            text: raw.into(),
        });
    }
    let d = circuit.node(&toks[1]);
    let g = circuit.node(&toks[2]);
    let s = circuit.node(&toks[3]);
    let bulk = circuit.node(&toks[4]);
    let model_name = &toks[5];
    let card = models
        .get(&model_name.to_ascii_lowercase())
        .ok_or_else(|| SpiceError::MissingModel {
            line,
            model: model_name.clone(),
            text: raw.into(),
        })?;
    let kv = scan_trailing_kv(raw);
    let model = mosfet_from_card(card, &kv);
    circuit.add(Device::Mosfet {
        name: toks[0].clone(),
        d,
        g,
        s,
        b: Some(bulk),
        model,
    });
    Ok(())
}

fn mosfet_from_card(card: &ModelCard, kv: &HashMap<String, f64>) -> MosfetModel {
    let d = MosfetModel::default();
    let polarity = match card.type_word.as_deref() {
        Some("pmos") => Polarity::P,
        _ => Polarity::N,
    };
    // W/L from the instance line if present, else the model, else 1.
    let l = kv
        .get("l")
        .copied()
        .or_else(|| card.get("l"))
        .unwrap_or(1.0);
    let w = kv
        .get("w")
        .copied()
        .or_else(|| card.get("w"))
        .unwrap_or(1.0);
    let w_over_l = if l != 0.0 { w / l } else { 1.0 };
    MosfetModel {
        level: MosLevel::Level1,
        polarity,
        vto: card.get("vto").or_else(|| card.get("vt0")).unwrap_or(d.vto),
        kp: card.get_or("kp", d.kp),
        lambda: card.get_or("lambda", d.lambda),
        gamma: card.get_or("gamma", d.gamma),
        phi: card.get_or("phi", d.phi),
        w_over_l,
        n_sub: card.get_or("nsub_factor", d.n_sub),
    }
}

fn parse_switch(
    line: usize,
    raw: &str,
    toks: &[String],
    circuit: &mut Circuit,
    models: &HashMap<String, ModelCard>,
) -> Result<(), SpiceError> {
    // S<name> a b ctrl+ ctrl- model
    if toks.len() < 6 {
        return Err(SpiceError::Syntax {
            line,
            msg: "need a, b, ctrl+, ctrl-, model".into(),
            text: raw.into(),
        });
    }
    let a = circuit.node(&toks[1]);
    let b = circuit.node(&toks[2]);
    let ctrl_p = circuit.node(&toks[3]);
    let ctrl_n = circuit.node(&toks[4]);
    let card = models.get(&toks[5].to_ascii_lowercase());
    let (von, voff, ron, roff) = match card {
        Some(c) => (
            c.get_or("vt", 0.0) + c.get_or("vh", 0.0).abs(),
            c.get_or("vt", 0.0) - c.get_or("vh", 0.0).abs(),
            c.get_or("ron", 1.0),
            c.get_or("roff", 1e12),
        ),
        None => (1.0, 0.0, 1.0, 1e12),
    };
    circuit.add(Device::VSwitch {
        name: toks[0].clone(),
        a,
        b,
        ctrl_p,
        ctrl_n,
        von,
        voff,
        ron,
        roff,
    });
    let _ = line;
    Ok(())
}

fn parse_controlled(
    line: usize,
    raw: &str,
    toks: &[String],
    circuit: &mut Circuit,
    is_vcvs: bool,
    env: &ParamEnv,
) -> Result<(), SpiceError> {
    // E<name> n+ n- nc+ nc- gain   (VCVS)
    // G<name> n+ n- nc+ nc- gm     (VCCS)
    // The POLY / VALUE / TABLE behavioral forms are recognized and refused:
    // a silent misparse (interning "poly" as a node) is exactly the failure
    // mode the loader's line-numbered errors exist to prevent.
    for t in &toks[1..] {
        let l = t.to_ascii_lowercase();
        if matches!(l.as_str(), "poly" | "value" | "table") {
            return Err(SpiceError::Syntax {
                line,
                msg: format!(
                    "`{}` controlled-source form is unsupported (only the linear \
                     `n+ n- nc+ nc- gain` form is)",
                    l.to_ascii_uppercase()
                ),
                text: raw.into(),
            });
        }
    }
    if toks.len() < 6 {
        return Err(SpiceError::Syntax {
            line,
            msg: "need n+, n-, nc+, nc-, and a gain".into(),
            text: raw.into(),
        });
    }
    let name = toks[0].clone();
    let p = circuit.node(&toks[1]);
    let n = circuit.node(&toks[2]);
    let cp = circuit.node(&toks[3]);
    let cn = circuit.node(&toks[4]);
    let gain = eval_value(line, &toks[5], raw, env)?;

    if is_vcvs {
        // Degenerate VCVS topologies make the MNA constraint row singular; the
        // honest move is a named refusal, not a zero-pivot mystery at solve
        // time. (A self-referential VCCS `G a b a b gm` is a legitimate
        // conductance idiom and stays accepted.)
        if p == n {
            return Err(SpiceError::Syntax {
                line,
                msg: format!(
                    "VCVS `{name}` shorts its own output port (n+ == n-); its \
                     branch current is indeterminate"
                ),
                text: raw.into(),
            });
        }
        if (cp == p && cn == n && gain == 1.0) || (cp == n && cn == p && gain == -1.0) {
            return Err(SpiceError::Syntax {
                line,
                msg: format!(
                    "VCVS `{name}` senses its own output at unity gain; the \
                     constraint row is identically zero (singular)"
                ),
                text: raw.into(),
            });
        }
        circuit.add(Device::Vcvs {
            name,
            p,
            n,
            cp,
            cn,
            gain,
        });
    } else {
        circuit.add(Device::Vccs {
            name,
            p,
            n,
            cp,
            cn,
            gm: gain,
        });
    }
    Ok(())
}

fn parse_current_controlled(
    line: usize,
    raw: &str,
    toks: &[String],
    circuit: &mut Circuit,
    is_cccs: bool,
    env: &ParamEnv,
    fixups: &mut Vec<NameFixup>,
) -> Result<(), SpiceError> {
    // F<name> n+ n- vname gain       (CCCS: I(n+ -> n-) = gain * I(vname))
    // H<name> n+ n- vname transres   (CCVS: V(n+, n-) = transres * I(vname))
    //
    // POLY is recognized and refused like the E/G behavioral forms — but ONLY
    // at the vname position: unlike E/G there is no VALUE/TABLE form for F/H,
    // and a blanket token scan would refuse a legitimately named source (a
    // `Vtable`, or literally `Value`). `poly` cannot be a vname (a V-source
    // name starts with `V`), so the check is unambiguous.
    if toks
        .get(3)
        .is_some_and(|t| t.eq_ignore_ascii_case("poly"))
    {
        return Err(SpiceError::Syntax {
            line,
            msg: "`POLY` controlled-source form is unsupported (only the linear \
                  `n+ n- vname gain` form is)"
                .into(),
            text: raw.into(),
        });
    }
    if toks.len() < 5 {
        return Err(SpiceError::Syntax {
            line,
            msg: "need n+, n-, a controlling V-source name, and a gain".into(),
            text: raw.into(),
        });
    }
    let name = toks[0].clone();
    let p = circuit.node(&toks[1]);
    let n = circuit.node(&toks[2]);
    let vname = toks[3].clone();
    let gain = eval_value(line, &toks[4], raw, env)?;

    if !is_cccs && p == n {
        // A CCVS with a shorted output port has an indeterminate branch
        // current (its constraint row collapses and its branch column cancels
        // to zero) — the same singularity as the shorted VCVS, refused with a
        // name instead of dying at a zero pivot. The CCCS variant is harmless:
        // `F a a ...` injects and withdraws the same current at one node (a
        // no-op, like the legal self-referential VCCS idiom).
        return Err(SpiceError::Syntax {
            line,
            msg: format!(
                "CCVS `{name}` shorts its own output port (n+ == n-); its \
                 branch current is indeterminate"
            ),
            text: raw.into(),
        });
    }

    // The referent may appear later in the deck: park a placeholder id and
    // defer the lookup to `resolve_name_fixups` (which also enforces that the
    // name exists, is unambiguous, and is an independent V source).
    let placeholder = DeviceId(u32::MAX);
    let id = if is_cccs {
        circuit.add(Device::Cccs {
            name,
            p,
            n,
            ctrl_src: placeholder,
            gain,
        })
    } else {
        circuit.add(Device::Ccvs {
            name,
            p,
            n,
            ctrl_src: placeholder,
            transres: gain,
        })
    };
    fixups.push(NameFixup {
        device: id,
        name: vname,
        line,
        raw: raw.into(),
        provenance: String::new(),
    });
    Ok(())
}

// --- control cards ----------------------------------------------------------

fn parse_options(raw: &str, directives: &mut Directives) {
    for (k, v) in scan_trailing_kv(raw) {
        match k.as_str() {
            "reltol" => directives.reltol = Some(v),
            "abstol" => directives.abstol = Some(v),
            "vntol" => directives.vntol = Some(v),
            _ => {}
        }
    }
}

fn parse_tran(
    line: usize,
    raw: &str,
    directives: &mut Directives,
) -> Result<TranDirective, SpiceError> {
    let toks = tokenize(raw);
    let mut nums = Vec::new();
    for t in &toks[1..] {
        let lower = t.to_ascii_lowercase();
        if lower == "uic" {
            directives.use_initial_conditions = true;
            continue;
        }
        if let Some(v) = parse_spice_number(t) {
            nums.push(v);
        }
    }
    if nums.len() < 2 {
        return Err(SpiceError::Syntax {
            line,
            msg: ".tran needs at least tstep and tstop".into(),
            text: raw.into(),
        });
    }
    Ok(TranDirective {
        tstep: nums[0],
        tstop: nums[1],
        tstart: nums.get(2).copied().unwrap_or(0.0),
        tmax: nums.get(3).copied(),
    })
}

/// A source-card AC token: a plain number or a parameter name.
fn ac_num_tok(t: &str, env: &ParamEnv) -> Option<f64> {
    parse_spice_number(t).or_else(|| env.get(&t.to_ascii_lowercase()).copied())
}

/// Split a source card's spec tokens into an optional AC small-signal stimulus
/// (`AC [mag] [phase]`) and the remaining tokens (the time-domain function),
/// so [`parse_source_kind`] never sees the AC keyword it would misparse. A bare
/// `AC` defaults to magnitude 1, phase 0 (SPICE convention).
fn extract_ac_spec(rest: &[String], env: &ParamEnv) -> (Option<AcStim>, Vec<String>) {
    let mut out = Vec::with_capacity(rest.len());
    let mut ac = None;
    let mut i = 0;
    while i < rest.len() {
        if rest[i].eq_ignore_ascii_case("ac") {
            let mut mag = 1.0;
            let mut phase_deg = 0.0;
            let mut consumed = 1;
            if let Some(m) = rest.get(i + 1).and_then(|t| ac_num_tok(t, env)) {
                mag = m;
                consumed += 1;
                if let Some(p) = rest.get(i + 2).and_then(|t| ac_num_tok(t, env)) {
                    phase_deg = p;
                    consumed += 1;
                }
            }
            ac = Some(AcStim { mag, phase_deg });
            i += consumed;
        } else {
            out.push(rest[i].clone());
            i += 1;
        }
    }
    (ac, out)
}

/// Parse `.ac <dec|oct|lin> <n> <fstart> <fstop>`.
fn parse_ac(line: usize, raw: &str) -> Result<AcDirective, SpiceError> {
    let toks = tokenize(raw);
    // toks[0] == ".ac"
    if toks.len() < 5 {
        return Err(SpiceError::Syntax {
            line,
            msg: ".ac needs <dec|oct|lin> <points> <fstart> <fstop>".into(),
            text: raw.into(),
        });
    }
    let sweep = match toks[1].to_ascii_lowercase().as_str() {
        "dec" => AcSweep::Decade,
        "oct" => AcSweep::Octave,
        "lin" => AcSweep::Linear,
        other => {
            return Err(SpiceError::Syntax {
                line,
                msg: format!("unknown `.ac` sweep type `{other}` (use dec, oct, or lin)"),
                text: raw.into(),
            });
        }
    };
    let points = number(line, &toks[2], raw)?;
    if points < 1.0 || points.fract() != 0.0 {
        return Err(SpiceError::Syntax {
            line,
            msg: format!("`.ac` point count must be a positive integer, got `{}`", toks[2]),
            text: raw.into(),
        });
    }
    let fstart = number(line, &toks[3], raw)?;
    let fstop = number(line, &toks[4], raw)?;
    if fstart <= 0.0 || fstop <= fstart {
        return Err(SpiceError::Syntax {
            line,
            msg: format!("`.ac` needs 0 < fstart < fstop, got fstart={fstart}, fstop={fstop}"),
            text: raw.into(),
        });
    }
    Ok(AcDirective {
        sweep,
        points: points as usize,
        fstart,
        fstop,
    })
}

/// Split a `.print`/`.plot` card into whitespace-separated tokens, but keep a
/// parenthesized group (`V(out)`, `V(a,b)`, `I(V1)`) intact — the general
/// tokenizer strips parentheses, which would shatter `V(out)` into `V` + `out`.
fn split_print_tokens(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut depth = 0i32;
    for c in s.chars() {
        match c {
            '(' => {
                depth += 1;
                cur.push(c);
            }
            ')' => {
                depth -= 1;
                cur.push(c);
            }
            c if c.is_whitespace() && depth == 0 => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Parse `.print`/`.plot ANALYSIS var...`, carrying the output-variable
/// expressions verbatim (the consumer parses them into probes).
fn parse_print(line: usize, raw: &str, is_plot: bool) -> Result<PrintRequest, SpiceError> {
    let toks = split_print_tokens(raw);
    // toks[0] == ".print"/".plot"; toks[1] == analysis type; the rest are vars.
    let kind = if is_plot { ".plot" } else { ".print" };
    if toks.len() < 2 {
        return Err(SpiceError::Syntax {
            line,
            msg: format!("{kind} needs an analysis type (op/dc/ac/tran) and output variables"),
            text: raw.into(),
        });
    }
    let analysis = toks[1].to_ascii_lowercase();
    if !matches!(analysis.as_str(), "op" | "dc" | "ac" | "tran") {
        return Err(SpiceError::Syntax {
            line,
            msg: format!(
                "{kind}: unknown analysis type `{}` (expected op, dc, ac, or tran)",
                toks[1]
            ),
            text: raw.into(),
        });
    }
    let vars: Vec<String> = toks[2..].to_vec();
    if vars.is_empty() {
        return Err(SpiceError::Syntax {
            line,
            msg: format!("{kind} {analysis} has no output variables"),
            text: raw.into(),
        });
    }
    Ok(PrintRequest {
        analysis,
        vars,
        is_plot,
    })
}

/// Parse `.dc <src> <start> <stop> <step> [<src2> <start2> <stop2> <step2>]`,
/// resolving each swept source name against the flattened device table
/// (case-insensitive), refusing a name that is not a V/I source and a step that
/// cannot reach its stop.
fn parse_dc(pl: &PhysLine, circuit: &Circuit) -> Result<DcDirective, SpiceError> {
    let line = pl.lineno;
    let raw = &pl.text;
    let toks = tokenize(raw);
    // toks[0] == ".dc"; then groups of 4: <src> <start> <stop> <step>.
    let args = &toks[1..];
    if args.len() != 4 && args.len() != 8 {
        return Err(SpiceError::Syntax {
            line,
            msg: format!(
                "`.dc` needs <src> <start> <stop> <step> (optionally a second such \
                 group for a nested sweep); got {} argument(s)",
                args.len()
            ),
            text: raw.clone(),
        });
    }
    let inner = parse_dc_group(line, raw, &args[0..4], circuit)?;
    let outer = if args.len() == 8 {
        Some(parse_dc_group(line, raw, &args[4..8], circuit)?)
    } else {
        None
    };
    Ok(DcDirective { inner, outer })
}

fn parse_dc_group(
    line: usize,
    raw: &str,
    group: &[String],
    circuit: &Circuit,
) -> Result<DcSweep, SpiceError> {
    let name = &group[0];
    let start = number(line, &group[1], raw)?;
    let stop = number(line, &group[2], raw)?;
    let step = number(line, &group[3], raw)?;
    // Resolve the swept source name case-insensitively, exactly as the F/H
    // control-source fixups resolve their referent.
    let key = name.to_ascii_lowercase();
    let matches: Vec<DeviceId> = circuit
        .iter()
        .filter(|(_, d)| d.name().to_ascii_lowercase() == key)
        .map(|(id, _)| id)
        .collect();
    let source = match matches.as_slice() {
        [] => {
            return Err(SpiceError::Syntax {
                line,
                msg: format!("`.dc` sweep source `{name}` does not exist in the deck"),
                text: raw.into(),
            });
        }
        [one] => *one,
        _ => {
            return Err(SpiceError::Syntax {
                line,
                msg: format!(
                    "`.dc` sweep source `{name}` is ambiguous (SPICE names are \
                     case-insensitive; rename one)"
                ),
                text: raw.into(),
            });
        }
    };
    if !matches!(
        circuit.devices[source.0 as usize],
        Device::Vsource { .. } | Device::Isource { .. }
    ) {
        return Err(SpiceError::Syntax {
            line,
            msg: format!(
                "`.dc` can only sweep an independent V or I source; `{name}` is not one. \
                 To sweep a current through a device, put a zero-volt ammeter \
                 (`Vsense a b 0`) in series and sweep that."
            ),
            text: raw.into(),
        });
    }
    // A step must actually march from start toward stop. Zero step, or a sign
    // that points away from stop, would loop forever or emit a single point that
    // silently ignores the range — refuse rather than fake it.
    if step == 0.0 {
        return Err(SpiceError::Syntax {
            line,
            msg: format!("`.dc {name}` step is zero, so the sweep cannot advance"),
            text: raw.into(),
        });
    }
    if (stop - start).signum() != step.signum() && stop != start {
        return Err(SpiceError::Syntax {
            line,
            msg: format!(
                "`.dc {name}` step {step} has the wrong sign to reach stop {stop} from \
                 start {start}"
            ),
            text: raw.into(),
        });
    }
    Ok(DcSweep {
        source,
        name: name.clone(),
        start,
        stop,
        step,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rc_divider() {
        let net = "RC test\nV1 in 0 DC 5\nR1 in out 1k\nC1 out 0 1u\n.tran 1u 1m\n.end\n";
        let (c, d) = SpiceLoader::load_with_directives(net).unwrap();
        assert_eq!(c.devices.len(), 3);
        assert!(d.tran.is_some());
        let tran = d.tran.unwrap();
        assert!((tran.tstop - 1e-3).abs() < 1e-12);
    }

    #[test]
    fn captures_ac_stimulus_from_source_card() {
        // A bare `AC` defaults to mag 1, phase 0; `AC 2 30` is captured verbatim;
        // a combined `DC 0 AC 1 SIN(...)` keeps the transient function too.
        let net = "ac cap\nV1 a 0 AC\nV2 b 0 AC 2 30\nV3 c 0 DC 0 AC 1 SIN(0 1 1k)\n\
                   R1 a 0 1k\nR2 b 0 1k\nR3 c 0 1k\n.end\n";
        let c = SpiceLoader::load(net).unwrap();
        assert_eq!(c.ac_stimulus.len(), 3);
        let stim: std::collections::HashMap<_, _> = c
            .ac_stimulus
            .iter()
            .map(|(id, s)| (c.devices[id.0 as usize].name().to_string(), *s))
            .collect();
        assert_eq!(stim["V1"], AcStim { mag: 1.0, phase_deg: 0.0 });
        assert_eq!(stim["V2"], AcStim { mag: 2.0, phase_deg: 30.0 });
        assert_eq!(stim["V3"], AcStim { mag: 1.0, phase_deg: 0.0 });
        // V3 still carries its SIN transient function (AC was peeled off).
        match &c.devices[2] {
            Device::Vsource { kind: SourceKind::Sin { freq, .. }, .. } => {
                assert!((freq - 1e3).abs() < 1e-9)
            }
            other => panic!("expected V3 to keep SIN, got {other:?}"),
        }
    }

    #[test]
    fn parses_ac_directive_variants() {
        for (card, sweep, pts) in [
            (".ac dec 10 10 1e6", AcSweep::Decade, 10),
            (".ac oct 5 20 20k", AcSweep::Octave, 5),
            (".ac lin 100 1 1000", AcSweep::Linear, 100),
        ] {
            let net = format!("ac\nV1 a 0 AC 1\nR1 a 0 1k\n{card}\n.end\n");
            let (_, d) = SpiceLoader::load_with_directives(&net).unwrap();
            let ac = d.ac.expect("ac directive");
            assert_eq!(ac.sweep, sweep);
            assert_eq!(ac.points, pts);
        }
        // Unknown sweep type and duplicate cards are line-numbered refusals.
        let bad = "ac\nV1 a 0 AC 1\nR1 a 0 1k\n.ac bogus 5 10 1e6\n.end\n";
        assert!(SpiceLoader::load(bad).unwrap_err().to_string().contains("sweep type"));
        let dup = "ac\nV1 a 0 AC 1\nR1 a 0 1k\n.ac dec 5 10 1e6\n.ac lin 5 10 1e6\n.end\n";
        assert!(SpiceLoader::load(dup).unwrap_err().to_string().contains("duplicate `.ac`"));
    }

    #[test]
    fn parses_and_resolves_dc_sweep() {
        let net = "dc\nVin a 0 DC 0\nVg g 0 DC 0\nR1 a 0 1k\n\
                   .dc Vin 0 5 0.5 Vg 0 3 1\n.end\n";
        let (c, d) = SpiceLoader::load_with_directives(net).unwrap();
        let dc = d.dc.expect("dc directive");
        assert_eq!(dc.inner.name, "Vin");
        assert_eq!(c.devices[dc.inner.source.0 as usize].name(), "Vin");
        assert!((dc.inner.stop - 5.0).abs() < 1e-12);
        let outer = dc.outer.expect("nested sweep");
        assert_eq!(outer.name, "Vg");
    }

    #[test]
    fn dc_sweep_guards_are_line_numbered() {
        // Unknown source.
        let e = SpiceLoader::load("dc\nVin a 0 DC 0\nR1 a 0 1k\n.dc Vnope 0 5 1\n.end\n")
            .unwrap_err()
            .to_string();
        assert!(e.contains("does not exist"), "{e}");
        // Sweeping a non-source names the ammeter idiom.
        let e = SpiceLoader::load("dc\nVin a 0 DC 0\nR1 a 0 1k\n.dc R1 0 5 1\n.end\n")
            .unwrap_err()
            .to_string();
        assert!(e.contains("ammeter") || e.contains("V or I source"), "{e}");
        // Zero step cannot advance.
        let e = SpiceLoader::load("dc\nVin a 0 DC 0\nR1 a 0 1k\n.dc Vin 0 5 0\n.end\n")
            .unwrap_err()
            .to_string();
        assert!(e.contains("step is zero"), "{e}");
        // Wrong-sign step cannot reach stop.
        let e = SpiceLoader::load("dc\nVin a 0 DC 0\nR1 a 0 1k\n.dc Vin 0 5 -1\n.end\n")
            .unwrap_err()
            .to_string();
        assert!(e.contains("wrong sign"), "{e}");
    }

    #[test]
    fn parses_print_and_plot_vars_preserving_parens() {
        let net = "p\nV1 a 0 DC 1\nR1 a b 1k\nR2 b 0 1k\n\
                   .print dc V(b) V(a,b) I(V1)\n.plot ac V(b)\n.end\n";
        let (_, d) = SpiceLoader::load_with_directives(net).unwrap();
        assert!(d.saw_plot);
        let dc = d.prints.iter().find(|p| p.analysis == "dc").unwrap();
        assert_eq!(dc.vars, vec!["V(b)", "V(a,b)", "I(V1)"]);
        assert!(!dc.is_plot);
        let ac = d.prints.iter().find(|p| p.analysis == "ac").unwrap();
        assert!(ac.is_plot);
        assert_eq!(ac.vars, vec!["V(b)"]);
    }

    #[test]
    fn suffixes_scale() {
        assert_eq!(parse_spice_number("1k"), Some(1e3));
        assert_eq!(parse_spice_number("1meg"), Some(1e6));
        assert_eq!(parse_spice_number("1m"), Some(1e-3));
        assert_eq!(parse_spice_number("2.2u"), Some(2.2e-6));
        assert_eq!(parse_spice_number("1e-12"), Some(1e-12));
        assert!((parse_spice_number("4.7nF").unwrap() - 4.7e-9).abs() < 1e-20);
    }

    #[test]
    fn parses_diode_model() {
        let net = "diode\nD1 a 0 DMOD\n.model DMOD D(IS=2e-15 N=1.2 RS=0.5)\n.end\n";
        let c = SpiceLoader::load(net).unwrap();
        match &c.devices[0] {
            Device::Diode { model, .. } => {
                assert!((model.is - 2e-15).abs() < 1e-20);
                assert!((model.n - 1.2).abs() < 1e-12);
            }
            _ => panic!("expected diode"),
        }
    }

    #[test]
    fn parses_npn() {
        let net = "bjt\nQ1 c b e QMOD\n.model QMOD NPN(IS=1e-15 BF=200)\n.end\n";
        let c = SpiceLoader::load(net).unwrap();
        match &c.devices[0] {
            Device::Bjt { model, .. } => {
                assert_eq!(model.polarity, Polarity::N);
                assert!((model.bf - 200.0).abs() < 1e-9);
            }
            _ => panic!("expected bjt"),
        }
    }

    #[test]
    fn parses_vcvs_and_vccs() {
        let net = "controlled\nE1 out 0 a 0 4\nG1 0 out2 a 0 2.5m\n.end\n";
        let c = SpiceLoader::load(net).unwrap();
        match &c.devices[0] {
            Device::Vcvs { name, gain, .. } => {
                assert_eq!(name, "E1");
                assert!((gain - 4.0).abs() < 1e-12);
            }
            other => panic!("expected VCVS, got {other:?}"),
        }
        match &c.devices[1] {
            Device::Vccs { name, gm, .. } => {
                assert_eq!(name, "G1");
                assert!((gm - 2.5e-3).abs() < 1e-15);
            }
            other => panic!("expected VCCS, got {other:?}"),
        }
    }

    #[test]
    fn refuses_poly_controlled_sources() {
        let net = "poly\nE1 out 0 POLY(2) a 0 b 0 0 1 1\n.end\n";
        let err = SpiceLoader::load(net).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("POLY"), "want a loud POLY refusal, got: {msg}");
        assert!(msg.contains("line 2"), "error must carry the line: {msg}");
    }

    #[test]
    fn refuses_degenerate_vcvs() {
        // Self-referential unity gain: constraint row identically zero.
        let net = "deg\nE1 out 0 out 0 1.0\n.end\n";
        let err = SpiceLoader::load(net).unwrap_err().to_string();
        assert!(err.contains("E1") && err.contains("unity gain"), "{err}");
        // Shorted output port: branch current indeterminate.
        let net2 = "deg\nE1 x x a 0 2.0\n.end\n";
        let err2 = SpiceLoader::load(net2).unwrap_err().to_string();
        assert!(err2.contains("E1") && err2.contains("shorts"), "{err2}");
        // The VCCS resistor idiom `G a b a b gm` stays legal.
        let net3 = "ok\nG1 a b a b 1m\n.end\n";
        assert!(SpiceLoader::load(net3).is_ok());
        // A non-unity self-referential VCVS is solvable (forces v_p == v_n).
        let net4 = "ok\nE1 out 0 out 0 2.0\n.end\n";
        assert!(SpiceLoader::load(net4).is_ok());
    }

    // --- F/H cards and the resolve-by-name pass (§2.2) ----------------------

    /// Find a device id by exact name (test helper).
    fn dev_id(c: &Circuit, name: &str) -> DeviceId {
        c.iter()
            .find(|(_, d)| d.name() == name)
            .map(|(id, _)| id)
            .unwrap_or_else(|| panic!("no device named {name}"))
    }

    #[test]
    fn parses_cccs_and_ccvs_with_forward_reference() {
        // Both cards name Vs BEFORE it appears in the deck: the deferred pass
        // must resolve it anyway. Vs is the idiomatic zero-volt ammeter.
        let net = "f/h\nF1 out 0 Vs 2\nH1 x 0 vs 50\nR1 in m 1k\nVs m 0 0\n.end\n";
        let c = SpiceLoader::load(net).unwrap();
        let vs = dev_id(&c, "Vs");
        match &c.devices[dev_id(&c, "F1").0 as usize] {
            Device::Cccs { ctrl_src, gain, .. } => {
                assert_eq!(*ctrl_src, vs, "F1 must resolve to Vs");
                assert!((gain - 2.0).abs() < 1e-12);
            }
            other => panic!("expected CCCS, got {other:?}"),
        }
        // `vs` (lowercase) resolves too: SPICE names are case-insensitive.
        match &c.devices[dev_id(&c, "H1").0 as usize] {
            Device::Ccvs {
                ctrl_src, transres, ..
            } => {
                assert_eq!(*ctrl_src, vs, "H1 must resolve to Vs");
                assert!((transres - 50.0).abs() < 1e-12);
            }
            other => panic!("expected CCVS, got {other:?}"),
        }
    }

    #[test]
    fn refuses_dangling_control_name() {
        let net = "dangle\nF1 out 0 Vnope 2\nR1 out 0 1k\n.end\n";
        let err = SpiceLoader::load(net).unwrap_err().to_string();
        assert!(err.contains("Vnope") && err.contains("does not exist"), "{err}");
        assert!(err.contains("line 2"), "error must carry the line: {err}");
    }

    #[test]
    fn refuses_non_vsource_control() {
        // Every branch-current carrier that is not an independent V source is
        // refused with the ammeter hint: an inductor, an E source (both own
        // branch currents!), and a plain resistor alike. This also covers
        // self-reference: an F/H is never a V source, so `H1 ... H1 ...`
        // lands here too.
        for (card, referent) in [
            ("F1 out 0 L1 2", "L1 a 0 1m"),
            ("F1 out 0 E1 2", "E1 x 0 a 0 4"),
            ("H1 out 0 R1 50", "R1 a 0 1k"),
            ("H1 out 0 H1 50", "R1 a 0 1k"),
        ] {
            let net = format!("bad\n{card}\n{referent}\n.end\n");
            let err = SpiceLoader::load(&net).unwrap_err().to_string();
            assert!(
                err.contains("not an independent voltage source")
                    && err.contains("zero-volt ammeter"),
                "card `{card}`: {err}"
            );
        }
    }

    #[test]
    fn refuses_ambiguous_control_name() {
        // Two sources differing only in case: genuinely ambiguous under
        // SPICE's case-insensitive names — refuse, never pick one.
        let net = "amb\nVs a 0 1\nVS b 0 2\nF1 out 0 vs 2\n.end\n";
        let err = SpiceLoader::load(net).unwrap_err().to_string();
        assert!(err.contains("ambiguous"), "{err}");
    }

    #[test]
    fn refuses_poly_and_degenerate_current_controlled() {
        let net = "poly\nF1 out 0 POLY(2) V1 V2 0 1 1\n.end\n";
        let err = SpiceLoader::load(net).unwrap_err().to_string();
        assert!(err.contains("POLY"), "want a loud POLY refusal: {err}");
        // CCVS shorted output port: branch current indeterminate.
        let net2 = "deg\nH1 x x Vs 50\nVs a 0 0\n.end\n";
        let err2 = SpiceLoader::load(net2).unwrap_err().to_string();
        assert!(err2.contains("H1") && err2.contains("shorts"), "{err2}");
        // The CCCS no-op form `F a a ...` stays legal (injects and withdraws
        // at one node), mirroring the legal self-referential VCCS idiom.
        let net3 = "ok\nF1 a a Vs 2\nVs a 0 0\n.end\n";
        assert!(SpiceLoader::load(net3).is_ok());
    }

    /// The subckt-composition rule: a vname inside a body is LOCAL — it
    /// resolves to the instance-mangled source (`X3.Vsense`), per instance.
    #[test]
    fn subckt_local_control_name_resolves_to_mangled_instance() {
        let net = "mirror\n\
                   .subckt mir inp outp\n\
                   Vsense inp 0 0\n\
                   F1 0 outp Vsense 2\n\
                   .ends\n\
                   X3 a b mir\n\
                   X4 c d mir\n\
                   R1 a 0 1k\nR2 b 0 1k\nR3 c 0 1k\nR4 d 0 1k\n\
                   .end\n";
        let c = SpiceLoader::load(net).unwrap();
        for inst in ["X3", "X4"] {
            let vs = dev_id(&c, &format!("{inst}.Vsense"));
            match &c.devices[dev_id(&c, &format!("{inst}.F1")).0 as usize] {
                Device::Cccs { ctrl_src, .. } => assert_eq!(
                    *ctrl_src, vs,
                    "{inst}.F1 must bind its OWN instance's ammeter"
                ),
                other => panic!("expected CCCS, got {other:?}"),
            }
        }
    }

    /// No global fallback: a body vname that does not exist locally dangles
    /// (as the mangled name) even when a same-named global source exists.
    #[test]
    fn subckt_control_name_never_binds_global() {
        let net = "scope\n\
                   Vs top 0 1\n\
                   .subckt blk outp\n\
                   F1 0 outp Vs 2\n\
                   .ends\n\
                   X1 w blk\n\
                   R1 w 0 1k\n\
                   .end\n";
        let err = SpiceLoader::load(net).unwrap_err().to_string();
        assert!(
            err.contains("X1.Vs") && err.contains("does not exist"),
            "the local-scope rule must dangle as `X1.Vs`, not bind global Vs: {err}"
        );
        // ...and the error names the instantiation site (provenance).
        assert!(err.contains("in subckt blk"), "{err}");
    }

    /// A TOP-LEVEL card may name a source inside an instance by its flattened
    /// dotted name (unambiguous, documented as non-portable).
    #[test]
    fn top_level_card_may_reference_flattened_name() {
        let net = "dotted\n\
                   .subckt probe inp\n\
                   Vsense inp 0 0\n\
                   .ends\n\
                   X1 a probe\n\
                   R1 a 0 1k\n\
                   F1 0 out X1.Vsense 2\n\
                   RL out 0 1k\n\
                   .end\n";
        let c = SpiceLoader::load(net).unwrap();
        let vs = dev_id(&c, "X1.Vsense");
        match &c.devices[dev_id(&c, "F1").0 as usize] {
            Device::Cccs { ctrl_src, .. } => assert_eq!(*ctrl_src, vs),
            other => panic!("expected CCCS, got {other:?}"),
        }
    }

    // --- helpers for param/subckt tests ------------------------------------

    /// Find a device by refdes and return its two-terminal ohms/farads/etc.
    fn resistor_ohms(c: &Circuit, name: &str) -> f64 {
        for d in &c.devices {
            if let Device::Resistor { name: n, ohms, .. } = d {
                if n == name {
                    return *ohms;
                }
            }
        }
        panic!("no resistor named {name}");
    }

    /// Return the (a, b) node names of a resistor by refdes.
    fn resistor_nodes(c: &Circuit, name: &str) -> (String, String) {
        for d in &c.devices {
            if let Device::Resistor { name: n, a, b, .. } = d {
                if n == name {
                    return (c.node_name(*a).to_string(), c.node_name(*b).to_string());
                }
            }
        }
        panic!("no resistor named {name}");
    }

    // --- .param + {expr} ----------------------------------------------------

    #[test]
    fn param_resolution_is_order_independent() {
        // `b` is defined BEFORE the `a` it depends on; the topo resolve must
        // still get b = a*2 = 6, so R1 = 6 ohms.
        let net = "p\n.param b={a*2}\n.param a=3\nR1 n 0 {b}\n.end\n";
        let c = SpiceLoader::load(net).unwrap();
        assert!((resistor_ohms(&c, "R1") - 6.0).abs() < 1e-12);
    }

    #[test]
    fn param_suffix_on_bare_value() {
        // A bare `.param` RHS keeps SPICE suffix scaling; `{r*2}` is pure
        // arithmetic over the resolved bare f64 (no suffix inside braces).
        let net = "p\n.param r=4.7k\nR1 a 0 {r*2}\n.end\n";
        let c = SpiceLoader::load(net).unwrap();
        assert!((resistor_ohms(&c, "R1") - 9400.0).abs() < 1e-9);
    }

    #[test]
    fn param_cycle_is_rejected() {
        let net = "p\n.param a={b}\n.param b={a}\nR1 n 0 {a}\n.end\n";
        let err = SpiceLoader::load(net).unwrap_err().to_string();
        assert!(err.contains("cycle"), "want a cycle refusal, got: {err}");
        assert!(err.contains("line"), "cycle error must carry a line: {err}");
    }

    #[test]
    fn param_undefined_name_is_rejected() {
        let net = "p\n.param a={q+1}\nR1 n 0 {a}\n.end\n";
        let err = SpiceLoader::load(net).unwrap_err().to_string();
        assert!(err.contains("undefined parameter") && err.contains('q'), "{err}");
        assert!(err.contains("line 2"), "must point at the .param line: {err}");
    }

    #[test]
    fn suffix_mixed_with_operator_refuses() {
        // `1k*2` is neither a valid expression (evalexpr rejects `1k`) nor a
        // pure suffix number — it must refuse, not silently parse 1000.
        let net = "p\nR1 a 0 {1k*2}\n.end\n";
        let err = SpiceLoader::load(net).unwrap_err().to_string();
        assert!(err.contains("line 2"), "loud, line-numbered refusal: {err}");
    }

    // --- .subckt / X --------------------------------------------------------

    #[test]
    fn subckt_node_mangling_internal_port_ground() {
        // R1 spans port `in` -> internal `mid`; R2 spans `mid` -> ground `0`.
        let net = "s\n\
                   .subckt DIV in out\n\
                   R1 in mid 1k\n\
                   R2 mid 0 2k\n\
                   .ends\n\
                   X1 a out DIV\n\
                   .end\n";
        let c = SpiceLoader::load(net).unwrap();
        // Refdes are prefixed by the instance name.
        assert!((resistor_ohms(&c, "X1.R1") - 1e3).abs() < 1e-9);
        assert!((resistor_ohms(&c, "X1.R2") - 2e3).abs() < 1e-9);
        // Port `in` -> caller node `a`; internal `mid` -> `X1.mid`.
        assert_eq!(resistor_nodes(&c, "X1.R1"), ("a".into(), "X1.mid".into()));
        // Internal `mid` -> `X1.mid`; ground `0` stays global ground.
        assert_eq!(resistor_nodes(&c, "X1.R2"), ("X1.mid".into(), "0".into()));
    }

    #[test]
    fn subckt_param_scoping_per_instance() {
        // Two instances of the same subckt with different `r`: the override on
        // X1 must not leak to X2 (which takes the default), and vice-versa.
        let net = "s\n\
                   .subckt RB a b r=1k\n\
                   R1 a b {r}\n\
                   .ends\n\
                   X1 n1 0 RB r=2k\n\
                   X2 n2 0 RB\n\
                   .end\n";
        let c = SpiceLoader::load(net).unwrap();
        assert!((resistor_ohms(&c, "X1.R1") - 2e3).abs() < 1e-9, "override");
        assert!((resistor_ohms(&c, "X2.R1") - 1e3).abs() < 1e-9, "default");
    }

    #[test]
    fn subckt_override_visible_to_dependent_default() {
        // `rload` defaults to `rbase*2`; overriding `rbase` must be visible to
        // that default (override wins and threads into the dependent default).
        let net = "s\n\
                   .subckt SC a b rbase=1k rload={rbase*2}\n\
                   R1 a b {rload}\n\
                   .ends\n\
                   X1 n 0 SC rbase=2k\n\
                   X2 m 0 SC\n\
                   .end\n";
        let c = SpiceLoader::load(net).unwrap();
        assert!((resistor_ohms(&c, "X1.R1") - 4e3).abs() < 1e-9, "override->default");
        assert!((resistor_ohms(&c, "X2.R1") - 2e3).abs() < 1e-9, "pure defaults");
    }

    #[test]
    fn nested_subckt_expands_and_mangles() {
        let net = "s\n\
                   .subckt INNER a b\n\
                   R1 a b 1k\n\
                   .ends\n\
                   .subckt OUTER x y\n\
                   X1 x mid INNER\n\
                   X2 mid y INNER\n\
                   .ends\n\
                   Xt p q OUTER\n\
                   .end\n";
        let c = SpiceLoader::load(net).unwrap();
        // Two resistors, both fully qualified through the instance path.
        assert_eq!(
            resistor_nodes(&c, "Xt.X1.R1"),
            ("p".into(), "Xt.mid".into())
        );
        assert_eq!(
            resistor_nodes(&c, "Xt.X2.R1"),
            ("Xt.mid".into(), "q".into())
        );
    }

    #[test]
    fn self_instantiation_is_refused() {
        let net = "s\n\
                   .subckt LOOP a b\n\
                   X1 a b LOOP\n\
                   .ends\n\
                   X0 p q LOOP\n\
                   .end\n";
        let err = SpiceLoader::load(net).unwrap_err().to_string();
        assert!(err.contains("cycle"), "want a cycle refusal, got: {err}");
        assert!(err.contains("LOOP") || err.contains("loop"), "{err}");
    }

    #[test]
    fn unsupported_card_in_subckt_errors_with_provenance() {
        // `Z` (IGBT) is unsupported; inside a subckt body it must still refuse
        // with the body line AND the instantiation breadcrumb.
        let net = "s\n\
                   .subckt BAD a b\n\
                   Z1 a b 5\n\
                   .ends\n\
                   X9 x 0 BAD\n\
                   .end\n";
        let err = SpiceLoader::load(net).unwrap_err().to_string();
        assert!(err.contains("line 3"), "points at the body line: {err}");
        assert!(err.contains("in subckt BAD"), "names the subckt: {err}");
        assert!(
            err.contains("instantiated at line 5"),
            "names the instantiation site: {err}"
        );
        assert!(err.contains("X9"), "names the instance: {err}");
    }

    #[test]
    fn subckt_arity_mismatch_is_refused() {
        let net = "s\n\
                   .subckt TWO a b\n\
                   R1 a b 1k\n\
                   .ends\n\
                   X1 only TWO\n\
                   .end\n";
        let err = SpiceLoader::load(net).unwrap_err().to_string();
        assert!(err.contains("nodes") && err.contains("ports"), "{err}");
    }

    #[test]
    fn conflicting_hoisted_models_refuse() {
        // Two subckts define `DMOD` with different IS: hoisting must refuse
        // rather than silently shadow one with the other.
        let net = "s\n\
                   .subckt A a b\n\
                   D1 a b DMOD\n\
                   .model DMOD D(IS=1e-15)\n\
                   .ends\n\
                   .subckt B a b\n\
                   D1 a b DMOD\n\
                   .model DMOD D(IS=2e-14)\n\
                   .ends\n\
                   X1 p 0 A\n\
                   X2 q 0 B\n\
                   .end\n";
        let err = SpiceLoader::load(net).unwrap_err().to_string();
        assert!(err.contains("conflicting") && err.contains("DMOD"), "{err}");
    }

    #[test]
    fn subckt_vcvs_opamp_macro_expands() {
        // A VCVS-based opamp macro: the E card resolves through the instance's
        // gain parameter and the ports/internal nodes mangle correctly.
        let net = "s\n\
                   .subckt OPAMP inp inn out gain=1e5\n\
                   Rin inp inn 1meg\n\
                   E1 out 0 inp inn {gain}\n\
                   .ends\n\
                   X1 a b y OPAMP gain=50k\n\
                   .end\n";
        let c = SpiceLoader::load(net).unwrap();
        // The gain parameter threaded into the VCVS.
        let mut found = false;
        for d in &c.devices {
            if let Device::Vcvs { name, gain, .. } = d {
                if name == "X1.E1" {
                    assert!((gain - 50e3).abs() < 1.0, "gain threaded: {gain}");
                    found = true;
                }
            }
        }
        assert!(found, "X1.E1 VCVS not found");
    }

    #[test]
    fn pulse_source_roundtrip() {
        let net = "p\nV1 a 0 pulse(0 5 1m 1u 1u 2m 5m)\n.end\n";
        let c = SpiceLoader::load(net).unwrap();
        match &c.devices[0] {
            Device::Vsource {
                kind: SourceKind::Pulse { v2, period, .. },
                ..
            } => {
                assert_eq!(*v2, 5.0);
                assert!((*period - 5e-3).abs() < 1e-12);
            }
            _ => panic!("expected pulse vsource"),
        }
    }

    // --- .include / .lib (§4.1) ---------------------------------------------

    use std::path::PathBuf;

    /// A throwaway directory under the OS temp dir, cleaned on drop, for the
    /// file-inclusion tests (which genuinely need files on disk).
    struct TmpDir(PathBuf);
    impl TmpDir {
        fn new(tag: &str) -> TmpDir {
            use std::sync::atomic::{AtomicUsize, Ordering};
            static N: AtomicUsize = AtomicUsize::new(0);
            let p = std::env::temp_dir().join(format!(
                "hauksbee_inc_{}_{}_{}",
                std::process::id(),
                tag,
                N.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&p).unwrap();
            TmpDir(p)
        }
        fn write(&self, name: &str, body: &str) -> PathBuf {
            let path = self.0.join(name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&path, body).unwrap();
            path
        }
    }
    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn include_pulls_model_library_end_to_end() {
        // The main deck references a diode model DEFINED in an included file.
        // Inclusion must run before `.model` collection so the diode binds it.
        let d = TmpDir::new("modlib");
        d.write("models.lib", "* a model library\n.model DFAST D(IS=3e-15 N=1.7)\n");
        let main = d.write(
            "main.cir",
            "main deck\nD1 a 0 DFAST\nR1 a 0 1k\n.include models.lib\n.end\n",
        );
        let c = SpiceLoader::load_file(&main).unwrap();
        let diode = c
            .devices
            .iter()
            .find(|dev| matches!(dev, Device::Diode { .. }))
            .expect("diode present");
        match diode {
            Device::Diode { model, .. } => {
                assert!((model.is - 3e-15).abs() < 1e-20, "IS from included .model");
                assert!((model.n - 1.7).abs() < 1e-12, "N from included .model");
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn include_missing_file_is_line_numbered_refusal() {
        let d = TmpDir::new("missing");
        let main = d.write("main.cir", "m\nR1 a 0 1k\n.include no_such.lib\n.end\n");
        let err = SpiceLoader::load_file(&main).unwrap_err().to_string();
        assert!(err.contains("not found"), "{err}");
        assert!(err.contains("no_such.lib"), "names the file: {err}");
        assert!(err.contains("line 3"), "carries the include line: {err}");
        assert!(err.contains("tried"), "lists resolved-path attempts: {err}");
    }

    #[test]
    fn include_cycle_is_refused() {
        let d = TmpDir::new("cycle");
        d.write("a.cir", "* a\nR1 x 0 1k\n.include b.cir\n");
        d.write("b.cir", "* b\nR2 y 0 1k\n.include a.cir\n");
        let top = d.write("top.cir", "top\nR0 z 0 1k\n.include a.cir\n.end\n");
        let err = SpiceLoader::load_file(&top).unwrap_err().to_string();
        assert!(err.contains("cycle"), "want a cycle refusal, got: {err}");
    }

    #[test]
    fn lib_section_pulls_only_the_named_block() {
        // Two sections; pulling `fast` must bring ONLY the fast model, not slow.
        let d = TmpDir::new("libsec");
        d.write(
            "corner.lib",
            "* corner library\n\
             .lib fast\n.model MM NMOS(VTO=0.5 KP=5e-3)\n.endl\n\
             .lib slow\n.model MM NMOS(VTO=1.5 KP=1e-3)\n.endl\n",
        );
        let main = d.write(
            "main.cir",
            "m\nM1 d g 0 0 MM\n.lib corner.lib fast\n.end\n",
        );
        let c = SpiceLoader::load_file(&main).unwrap();
        match c.devices.iter().find(|x| matches!(x, Device::Mosfet { .. })).unwrap() {
            Device::Mosfet { model, .. } => {
                assert!((model.vto - 0.5).abs() < 1e-9, "fast VTO, not slow: {}", model.vto);
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn lib_unknown_section_lists_available() {
        let d = TmpDir::new("libunk");
        d.write(
            "corner.lib",
            ".lib fast\n.model MM NMOS(VTO=0.5)\n.endl\n.lib slow\n.model MM NMOS(VTO=1.5)\n.endl\n",
        );
        let main = d.write("main.cir", "m\nM1 d g 0 0 MM\n.lib corner.lib typ\n.end\n");
        let err = SpiceLoader::load_file(&main).unwrap_err().to_string();
        assert!(err.contains("not found"), "{err}");
        assert!(err.contains("fast") && err.contains("slow"), "lists sections: {err}");
    }

    #[test]
    fn bare_lib_one_arg_is_refused_not_treated_as_include() {
        let d = TmpDir::new("libbare");
        d.write("stuff.lib", ".model MM NMOS(VTO=0.5)\n");
        let main = d.write("main.cir", "m\nM1 d g 0 0 MM\n.lib stuff.lib\n.end\n");
        let err = SpiceLoader::load_file(&main).unwrap_err().to_string();
        assert!(err.contains("ambiguous"), "{err}");
        assert!(err.contains(".include") && err.contains("<section>"), "guides the user: {err}");
    }

    #[test]
    fn error_in_included_file_names_file_and_site() {
        // A malformed element inside the included file must report the included
        // file's own line AND the inclusion site.
        let d = TmpDir::new("incerr");
        d.write("bad.lib", "* bad lib\nZ9 a b 5\n"); // Z is unsupported
        let main = d.write("main.cir", "m\nR1 a 0 1k\n.include bad.lib\n.end\n");
        let err = SpiceLoader::load_file(&main).unwrap_err().to_string();
        assert!(err.contains("line 2"), "included-file line: {err}");
        assert!(err.contains("bad.lib"), "names the included file: {err}");
        assert!(err.contains("line 3"), "names the inclusion site: {err}");
    }

    // --- .ic / .nodeset (§4.1) ----------------------------------------------

    /// Find a node id by name (test helper).
    fn node_of(c: &Circuit, name: &str) -> NodeId {
        c.find_node(name).unwrap_or_else(|| panic!("no node {name}"))
    }

    #[test]
    fn ic_requires_uic_or_refuses() {
        // Without `uic`: loud refusal (no pin-during-DC machinery).
        let net = "rc\nC1 out 0 1u\nR1 out 0 1k\n.ic V(out)=5\n.tran 1u 1m\n.end\n";
        let err = SpiceLoader::load(net).unwrap_err().to_string();
        assert!(err.contains("uic"), "must cite uic: {err}");
        assert!(err.contains("line 4"), "points at the .ic line: {err}");

        // With `uic`: accepted, and the node voltage is recorded.
        let net2 = "rc\nC1 out 0 1u\nR1 out 0 1k\n.ic V(out)=5\n.tran 1u 1m uic\n.end\n";
        let (c, _d) = SpiceLoader::load_with_directives(net2).unwrap();
        let out = node_of(&c, "out");
        assert_eq!(c.initial_conditions, vec![(out, 5.0)]);
    }

    #[test]
    fn ic_value_through_param_env() {
        let net = "rc\n.param vstart=3.3\nC1 out 0 1u\nR1 out 0 1k\n\
                   .ic V(out)={vstart*2}\n.tran 1u 1m uic\n.end\n";
        let (c, _d) = SpiceLoader::load_with_directives(net).unwrap();
        let out = node_of(&c, "out");
        assert_eq!(c.initial_conditions.len(), 1);
        assert_eq!(c.initial_conditions[0].0, out);
        assert!((c.initial_conditions[0].1 - 6.6).abs() < 1e-9, "expr value");
    }

    #[test]
    fn ic_unknown_node_suggests_candidate() {
        let net = "rc\nC1 out 0 1u\nR1 out 0 1k\n.ic V(ott)=5\n.tran 1u 1m uic\n.end\n";
        let err = SpiceLoader::load(net).unwrap_err().to_string();
        assert!(err.contains("unknown node") && err.contains("ott"), "{err}");
        assert!(err.contains("did you mean") && err.contains("out"), "candidate: {err}");
    }

    #[test]
    fn ic_targets_flattened_subckt_node() {
        // `.ic V(X1.out)=2` must resolve against the mangled internal node.
        let net = "s\n\
                   .subckt BUF inp\n\
                   R1 inp out 1k\n\
                   C1 out 0 1u\n\
                   .ends\n\
                   X1 a BUF\n\
                   Vd a 0 1\n\
                   .ic V(X1.out)=2\n\
                   .tran 1u 1m uic\n\
                   .end\n";
        let (c, _d) = SpiceLoader::load_with_directives(net).unwrap();
        let n = node_of(&c, "X1.out");
        assert_eq!(c.initial_conditions, vec![(n, 2.0)], "flattened-name contract");
    }

    #[test]
    fn nodeset_populates_without_uic() {
        // `.nodeset` is a DC guess; it needs no `uic` and is not an `.ic`.
        let net = "n\nR1 a 0 1k\nR2 a b 1k\nVd b 0 5\n.nodeset V(a)=2.5\n.op\n.end\n";
        let (c, _d) = SpiceLoader::load_with_directives(net).unwrap();
        let a = node_of(&c, "a");
        assert_eq!(c.nodesets, vec![(a, 2.5)]);
        assert!(c.initial_conditions.is_empty());
    }
}
