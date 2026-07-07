//! A pragmatic SPICE netlist loader.
//!
//! Parses a useful subset of `.cir` files into a [`Circuit`]: element lines for
//! R/C/L/V/I/D/Q/M/S/E/G, `.model` cards for diodes, BJTs, and MOSFETs, the `sin`,
//! `pulse`, and `pwl` source functions, `.tran`, `.temp`, and `.options`. The
//! goal is to ingest real test vectors and user-supplied netlists, not to be a
//! complete SPICE3 front end; anything unsupported is reported with the line.
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

use crate::models::{BjtModel, DiodeModel, MosLevel, MosfetModel, Polarity};
use crate::source::{PwlPoint, SourceKind};
use crate::{Circuit, Device};
use evalexpr::{
    build_operator_tree, ContextWithMutableVariables, DefaultNumericTypes, HashMapContext,
    Node as EvalNode, Value,
};
use std::collections::{HashMap, HashSet};
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
}

/// Parsed `.tran` parameters (seconds).
#[derive(Debug, Clone, Copy)]
pub struct TranDirective {
    pub tstep: f64,
    pub tstop: f64,
    pub tstart: f64,
    pub tmax: Option<f64>,
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

    /// Parse a netlist into a [`Circuit`] plus the [`Directives`] it carried.
    pub fn load_with_directives(text: &str) -> Result<(Circuit, Directives), SpiceError> {
        let logical = join_continuations(text);
        let mut circuit = Circuit::new();
        let mut directives = Directives::default();

        // First pass: collect `.model` cards (top-level and hoisted from subckt
        // bodies), `.subckt` definitions, global `.param` cards, and the
        // top-level `.temp`/`.options`/`.tran` directives — so element lines can
        // resolve models and parameters regardless of order.
        let mut models: HashMap<String, ModelCard> = HashMap::new();
        let mut subckts: HashMap<String, SubcktDef> = HashMap::new();
        let mut param_cards: Vec<ParamCard> = Vec::new();
        // Top-level element / X-instantiation lines, in source order.
        let mut top_elems: Vec<(usize, String)> = Vec::new();
        // The subckt currently being collected (subckt definitions do not nest).
        let mut current: Option<SubcktDef> = None;

        for (lineno, raw) in &logical {
            let trimmed = raw.trim_start();
            let lower = trimmed.to_ascii_lowercase();

            if lower.starts_with(".subckt") {
                if current.is_some() {
                    return Err(SpiceError::Syntax {
                        line: *lineno,
                        msg: "nested `.subckt` definitions are unsupported".into(),
                        text: raw.clone(),
                    });
                }
                current = Some(parse_subckt_header(*lineno, raw)?);
                continue;
            }
            if lower.starts_with(".ends") {
                match current.take() {
                    Some(def) => {
                        if let Some(prev) = subckts.insert(def.name.to_ascii_lowercase(), def) {
                            return Err(SpiceError::Syntax {
                                line: *lineno,
                                msg: format!("duplicate `.subckt {}` definition", prev.name),
                                text: raw.clone(),
                            });
                        }
                    }
                    None => {
                        return Err(SpiceError::Syntax {
                            line: *lineno,
                            msg: "`.ends` without a matching `.subckt`".into(),
                            text: raw.clone(),
                        });
                    }
                }
                continue;
            }

            // Inside a subckt body: hoist `.model`, keep `.param`/elements/X for
            // the body, and refuse analysis/topology directives that make no
            // sense inside a subckt.
            if let Some(def) = current.as_mut() {
                if lower.starts_with(".model") {
                    let card = parse_model_card(*lineno, raw)?;
                    insert_model(&mut models, card, *lineno, raw)?;
                } else if lower.starts_with('.') && !lower.starts_with(".param") {
                    return Err(SpiceError::Syntax {
                        line: *lineno,
                        msg: format!(
                            "directive `{}` is not allowed inside a `.subckt` body",
                            first_token(trimmed)
                        ),
                        text: raw.clone(),
                    });
                } else if !trimmed.is_empty() && !trimmed.starts_with('*') {
                    def.body.push((*lineno, raw.clone()));
                }
                continue;
            }

            // Top level.
            if lower.starts_with(".model") {
                let card = parse_model_card(*lineno, raw)?;
                insert_model(&mut models, card, *lineno, raw)?;
            } else if lower.starts_with(".param") {
                parse_param_card(*lineno, raw, &mut param_cards)?;
            } else if lower.starts_with(".temp") {
                let toks = tokenize(raw);
                if let Some(t) = toks.get(1) {
                    // `.temp` may reference a parameter, but the environment is
                    // not built yet; support only a literal here (parameterized
                    // temperature is out of scope for this step).
                    circuit.temp_c = number(*lineno, t, raw)?;
                }
            } else if lower.starts_with(".options") || lower.starts_with(".option") {
                parse_options(raw, &mut directives);
            } else if lower.starts_with(".tran") {
                directives.tran = Some(parse_tran(*lineno, raw, &mut directives)?);
            } else if trimmed.is_empty() || trimmed.starts_with('*') || trimmed.starts_with('.') {
                // Other directives (`.print`, `.ac`, `.end`, ...) are consumed
                // elsewhere or out of scope for this step; skip them here.
            } else {
                top_elems.push((*lineno, raw.clone()));
            }
        }

        if let Some(def) = current {
            return Err(SpiceError::Syntax {
                line: def.def_line,
                msg: format!("`.subckt {}` is never closed with `.ends`", def.name),
                text: String::new(),
            });
        }

        // Build the global parameter environment (order-independent topological
        // resolve; cycles and undefined names error with a line number).
        let global_env: Rc<ParamEnv> =
            Rc::new(resolve_params(&param_cards, &ParamEnv::new())?);

        // Flatten: splice every `X` call into a flat list of element lines, each
        // carrying its parameter environment and a provenance breadcrumb.
        let mut expanded: Vec<SplicedLine> = Vec::new();
        for (lineno, raw) in &top_elems {
            if starts_with_letter(raw, 'x') {
                expand_instance(
                    *lineno,
                    raw,
                    &subckts,
                    global_env.clone(),
                    &mut Vec::new(),
                    &mut expanded,
                )?;
            } else {
                expanded.push(SplicedLine {
                    lineno: *lineno,
                    text: raw.clone(),
                    provenance: String::new(),
                    env: global_env.clone(),
                });
            }
        }

        // Second pass: parse the flattened element lines. Errors from a spliced
        // body are annotated with where the body came from and where it was
        // instantiated.
        for sl in &expanded {
            parse_element(sl.lineno, &sl.text, &mut circuit, &models, &sl.env)
                .map_err(|e| with_provenance(e, &sl.provenance))?;
        }

        Ok((circuit, directives))
    }
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
    /// Body cards (elements, nested `X`, and local `.param`), with file lines.
    body: Vec<(usize, String)>,
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
        'Q' => &[1, 2, 3],
        'M' | 'S' | 'E' | 'G' => &[1, 2, 3, 4],
        _ => &[],
    }
}

/// Expand one `Xxxx ... NAME [k=v ...]` instantiation into `out`, recursing for
/// nested `X` calls. `chain` is the stack of subckt names currently being
/// expanded, for the self-instantiation cycle check.
fn expand_instance(
    lineno: usize,
    raw: &str,
    subckts: &HashMap<String, SubcktDef>,
    caller_env: Rc<ParamEnv>,
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
        .filter(|(_, b)| b.trim_start().to_ascii_lowercase().starts_with(".param"))
        .map(|(bl, b)| -> Result<Vec<ParamCard>, SpiceError> {
            let mut tmp = Vec::new();
            parse_param_card(*bl, b, &mut tmp)?;
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

    let breadcrumb = format!(
        " (in subckt {}, instantiated at line {} as {})",
        def.name, lineno, inst_name
    );

    chain.push(key);
    for (blineno, bline) in &def.body {
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
                *blineno,
                &rewritten,
                subckts,
                inst_env.clone(),
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
            out.push(SplicedLine {
                lineno: *blineno,
                text: new_toks.join(" "),
                provenance: breadcrumb.clone(),
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

// --- line joining -----------------------------------------------------------

/// Strip comments, join `+` continuation lines, and drop the title line.
/// Returns `(line_number, text)` for each logical line.
fn join_continuations(text: &str) -> Vec<(usize, String)> {
    let mut out: Vec<(usize, String)> = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let lineno = i + 1;
        // The very first non-blank line is the title in SPICE.
        if out.is_empty() && lineno == 1 {
            continue;
        }
        // Inline `;` and trailing `$` comments are stripped; full-line `*` too.
        let stripped = strip_inline_comment(line);
        let t = stripped.trim_end();
        if t.trim_start().starts_with('+') {
            if let Some(last) = out.last_mut() {
                let cont = t.trim_start().trim_start_matches('+');
                last.1.push(' ');
                last.1.push_str(cont.trim());
                continue;
            }
        }
        if t.trim().is_empty() {
            continue;
        }
        out.push((lineno, t.to_string()));
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
    let kind = parse_source_kind(line, raw, &toks[3..], env)?;

    let device = if is_voltage {
        Device::Vsource { name, p, n, kind }
    } else {
        Device::Isource { name, p, n, kind }
    };
    circuit.add(device);
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
}
