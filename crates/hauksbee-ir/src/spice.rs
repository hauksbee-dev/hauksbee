//! SPICE netlist loader for the `.cir` subset hauksbee simulates.
//! Long-form how-and-why: docs/how-and-why/hauksbee-ir/spice.md.
//!
//! Elements `R C L V I D Q M S E G F H B K` and `X` subcircuit calls; `.model`
//! cards for `D`, `NPN`/`PNP`, `NMOS`/`PMOS` (level 1) and `SW`; `DC`/`SIN`/
//! `PULSE`/`PWL` sources with an optional `AC <mag> [phase]` stimulus; SI
//! suffixes on bare values; `.param` with `{expr}` arithmetic (params fold to
//! constants; suffixes are not allowed inside braces); `.subckt`/`.ends`
//! flattening with per-instance parameter overrides; `.include`; and the
//! `.tran`/`.ac`/`.dc`/`.op`/`.print`/`.plot`/`.ic`/`.nodeset`/`.temp`/`.options`
//! directives. Every other card is a line-numbered error, never a silent no-op.
//! The first line of a deck is its title; `*` starts a comment; `+` continues
//! the previous line; node `0`/`gnd` is ground.

use crate::models::{BjtModel, DiodeModel, MosfetModel, Polarity};
use crate::source::{AcStim, PwlPoint, SourceKind};
use crate::{BDep, BOutput, Circuit, CompiledExpr, Device, DeviceId, NodeId};
use evalexpr::{build_operator_tree, Value};
use std::collections::HashMap;
use std::fmt;
use std::path::Path;
use std::rc::Rc;

type ParamEnv = HashMap<String, f64>;

/// Non-topology cards the solver may want.
#[derive(Debug, Clone, Default)]
pub struct Directives {
    /// `.tran <tstep> <tstop> [tstart] [tmax] [uic]`.
    pub tran: Option<TranDirective>,
    /// `.options reltol=... abstol=... vntol=...`.
    pub reltol: Option<f64>,
    pub abstol: Option<f64>,
    pub vntol: Option<f64>,
    /// Whether `.tran` carried `uic`.
    pub use_initial_conditions: bool,
    /// `.ac <dec|oct|lin> <n> <fstart> <fstop>`.
    pub ac: Option<AcDirective>,
    /// `.dc <src> <start> <stop> <step> [<src2> ...]`, sources resolved.
    pub dc: Option<DcDirective>,
    /// `.print`/`.plot ANALYSIS var...` requests, in source order.
    pub prints: Vec<PrintRequest>,
    /// Whether any `.plot` card was seen (treated as `.print`).
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
    Decade,
    Octave,
    Linear,
}

/// Parsed `.ac <dec|oct|lin> <n> <fstart> <fstop>`; `points` is per decade/
/// octave, or the total for a linear sweep.
#[derive(Debug, Clone, Copy)]
pub struct AcDirective {
    pub sweep: AcSweep,
    pub points: usize,
    pub fstart: f64,
    pub fstop: f64,
}

/// One swept source of a `.dc` analysis; `name` is the label as written.
#[derive(Debug, Clone)]
pub struct DcSweep {
    pub source: DeviceId,
    pub name: String,
    pub start: f64,
    pub stop: f64,
    pub step: f64,
}

/// `.dc`: `inner` sweeps fastest; `outer`, if present, wraps it.
#[derive(Debug, Clone)]
pub struct DcDirective {
    pub inner: DcSweep,
    pub outer: Option<DcSweep>,
}

/// A `.print`/`.plot ANALYSIS var...` request; `vars` are carried verbatim
/// (`V(out)`, `V(a,b)`, `I(V1)`) for the consumer's probe parser.
#[derive(Debug, Clone)]
pub struct PrintRequest {
    /// `op`/`dc`/`ac`/`tran`, lowercased.
    pub analysis: String,
    pub vars: Vec<String>,
    pub is_plot: bool,
}

/// A load error: the offending line number plus a message that names the
/// included file (if any) and quotes the card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpiceError {
    pub line: usize,
    pub msg: String,
}

impl fmt::Display for SpiceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.msg)
    }
}

impl std::error::Error for SpiceError {}

/// Loads SPICE netlists into the IR.
pub struct SpiceLoader;

impl SpiceLoader {
    /// Parse a netlist string into a [`Circuit`], discarding directives.
    pub fn load(text: &str) -> Result<Circuit, SpiceError> {
        Ok(Self::load_with_directives(text)?.0)
    }

    /// Parse a netlist string; `.include` paths resolve against the working
    /// directory (use [`SpiceLoader::load_file`] for deck-relative includes).
    pub fn load_with_directives(text: &str) -> Result<(Circuit, Directives), SpiceError> {
        load_deck(text, Path::new("."))
    }

    /// Parse a netlist file into a [`Circuit`].
    pub fn load_file<P: AsRef<Path>>(path: P) -> Result<Circuit, SpiceError> {
        Ok(Self::load_file_with_directives(path)?.0)
    }

    /// Parse a netlist file; `.include` paths resolve against its directory.
    pub fn load_file_with_directives<P: AsRef<Path>>(
        path: P,
    ) -> Result<(Circuit, Directives), SpiceError> {
        let path = path.as_ref();
        let msg = |e| format!("cannot read deck `{}`: {e}", path.display());
        let text = std::fs::read_to_string(path).map_err(|e| SpiceError {
            line: 0,
            msg: msg(e),
        })?;
        load_deck(&text, path.parent().unwrap_or(Path::new(".")))
    }
}

/// `bail!(line, "msg {x}")`: return a [`SpiceError`] quoting `line`.
macro_rules! bail {
    ($line:expr, $($arg:tt)*) => { return Err($line.err(format!($($arg)*))) };
}

fn load_deck(text: &str, dir: &Path) -> Result<(Circuit, Directives), SpiceError> {
    let mut lines = Vec::new();
    read_lines(text, dir, Rc::from(""), true, 0, &mut lines)?;
    let mut circuit = Circuit::new();
    let mut d = Directives::default();
    let deck = collect(lines, &mut circuit, &mut d)?;
    let env = Rc::new(resolve_params(&deck.params, &ParamEnv::new())?);
    let top = Scope {
        prefix: String::new(),
        ports: HashMap::new(),
        env,
    };
    let mut b = Build {
        circuit,
        deck: &deck,
        fixups: Vec::new(),
        seen: HashMap::new(),
    };
    for line in &deck.elems {
        b.element(line, &top, &mut Vec::new())?;
    }
    b.resolve_fixups()?;
    let mut circuit = b.circuit;
    if let Some(line) = &deck.dc {
        d.dc = Some(parse_dc(line, &circuit)?);
    }
    circuit.initial_conditions = ic_values(&deck.ic, &circuit, &top.env)?;
    circuit.nodesets = ic_values(&deck.nodeset, &circuit, &top.env)?;
    if !circuit.initial_conditions.is_empty() && !d.use_initial_conditions {
        bail!(deck.ic[0], "`.ic` requires `uic` on the `.tran` card (hauksbee seeds the power-on start; it does not pin nodes during a DC solve)");
    }
    Ok((circuit, d))
}

// --- lines -------------------------------------------------------------------

/// One logical (continuation-joined) card with its origin.
#[derive(Clone, Debug)]
struct Line {
    file: Rc<str>,
    no: usize,
    text: String,
}

impl Line {
    fn err(&self, msg: impl fmt::Display) -> SpiceError {
        let file = if self.file.is_empty() {
            String::new()
        } else {
            format!("{}: ", self.file)
        };
        SpiceError {
            line: self.no,
            msg: format!("{file}{msg}: `{}`", self.text),
        }
    }
}

/// Read a file's cards (title dropped for the top deck, comments and blanks
/// dropped, `+` continuations joined), splicing `.include` files in place.
fn read_lines(
    text: &str,
    dir: &Path,
    file: Rc<str>,
    is_top: bool,
    depth: usize,
    out: &mut Vec<Line>,
) -> Result<(), SpiceError> {
    let mut lines: Vec<Line> = Vec::new();
    for (i, raw) in text.lines().enumerate().skip(usize::from(is_top)) {
        let t = raw.split(';').next().unwrap_or("").trim();
        match (t.strip_prefix('+'), lines.last_mut()) {
            (Some(cont), Some(last)) => last.text.push_str(&format!(" {}", cont.trim())),
            _ if t.is_empty() || t.starts_with('*') => {}
            _ => lines.push(Line {
                file: file.clone(),
                no: i + 1,
                text: t.to_string(),
            }),
        }
    }
    for line in lines {
        let head = line
            .text
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        if head != ".include" && head != ".inc" && head != ".lib" {
            out.push(line);
            continue;
        }
        let args: Vec<&str> = line.text[head.len()..]
            .split_whitespace()
            .map(|a| a.trim_matches(['"', '\'']))
            .collect();
        let (arg, section) = match (head.as_str(), args.as_slice()) {
            (".lib", [file, section]) => (*file, Some(*section)),
            (".lib", _) => bail!(
                line,
                "`.lib <file>` is ambiguous; use `.include <file>` or `.lib <file> <section>`"
            ),
            (_, [file]) => (*file, None),
            _ => bail!(line, "`{head}` takes one file argument"),
        };
        let path = dir.join(arg);
        if depth >= 20 {
            bail!(line, "`.include` nesting deeper than 20 (a cycle?)");
        }
        let inc = std::fs::read_to_string(&path).map_err(|e| {
            line.err(format!(
                "cannot read included file `{}`: {e}",
                path.display()
            ))
        })?;
        // `.lib <file> <section>` splices only the `.lib <section>` .. `.endl`
        // block of the library file.
        let inc = match section {
            None => inc,
            Some(section) => {
                let mut inside = false;
                let mut picked = String::new();
                for raw in inc.lines() {
                    let t = raw.trim();
                    let mut words = t.split_whitespace();
                    match words.next().map(|w| w.to_ascii_lowercase()).as_deref() {
                        Some(".lib")
                            if words
                                .next()
                                .is_some_and(|n| n.eq_ignore_ascii_case(section)) =>
                        {
                            inside = true
                        }
                        Some(".endl") => inside = false,
                        _ if inside => {
                            picked.push_str(raw);
                            picked.push('\n');
                        }
                        _ => {}
                    }
                }
                if picked.is_empty() {
                    bail!(line, "section `{section}` not found in `{arg}`");
                }
                picked
            }
        };
        let sub = path.parent().unwrap_or(Path::new("."));
        read_lines(&inc, sub, Rc::from(arg), false, depth + 1, out)?;
    }
    Ok(())
}

/// Split a card on whitespace, commas and parentheses, keeping a `{...}`
/// expression atomic and `key=value` pairs together.
fn tokenize(s: &str) -> Vec<String> {
    let (mut out, mut cur, mut depth) = (Vec::new(), String::new(), 0i32);
    for c in s.chars() {
        if c == '{' || c == '}' {
            depth += if c == '{' { 1 } else { -1 };
        }
        if depth > 0 || !"(), \t".contains(c) || c == '}' {
            cur.push(c);
        } else if !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn braced(tok: &str) -> Option<&str> {
    tok.trim()
        .strip_prefix('{')
        .and_then(|s| s.strip_suffix('}'))
        .map(str::trim)
}

// --- numbers and expressions -------------------------------------------------

/// Length of the longest prefix of `t` that parses as a float (`[+-]digits
/// [.digits][e[+-]digits]`); 0 when `t` does not start with a number.
fn number_end(t: &str) -> usize {
    let digits = t.trim_start_matches(['+', '-']);
    if !digits.starts_with(|c: char| c.is_ascii_digit() || c == '.') {
        return 0;
    }
    let parses = |n: &usize| t.is_char_boundary(*n) && t[..*n].parse::<f64>().is_ok();
    (1..=t.len()).rev().find(parses).unwrap_or(0)
}

/// Parse a SPICE number with an optional engineering suffix (`1k`, `2.2u`,
/// `1meg`, `4.7nF`, `5A`). A stray tail (`1kk`, `1x`) is `None`, not silently
/// truncated. Public because the CLI accepts the same vocabulary.
pub fn parse_spice_number(tok: &str) -> Option<f64> {
    const SCALES: [&str; 10] = ["meg", "mil", "t", "g", "k", "m", "u", "n", "p", "f"];
    const EXP: [i32; 10] = [6, -6, 12, 9, 3, -3, -6, -9, -12, -15];
    let t = tok.trim();
    let end = number_end(t);
    let value: f64 = t[..end].parse().ok().filter(|_| end > 0)?;
    let suffix = t[end..].to_ascii_lowercase();
    let scale = SCALES.iter().position(|p| suffix.starts_with(p));
    let mult = scale.map_or(1.0, |i| {
        10f64.powi(EXP[i]) * if i == 1 { 25.4 } else { 1.0 }
    });
    let rest = &suffix[scale.map_or(0, |i| SCALES[i].len())..];
    matches!(
        rest,
        "" | "a" | "v" | "f" | "h" | "s" | "hz" | "ohm" | "ohms"
    )
    .then_some(value * mult)
}

/// Iterative Levenshtein edit distance; shared by did-you-mean hints elsewhere.
pub fn levenshtein(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut cur = vec![i + 1];
        for (j, &cb) in b.iter().enumerate() {
            let sub = prev[j] + usize::from(ca != cb);
            cur.push(sub.min(prev[j + 1] + 1).min(cur[j] + 1));
        }
        prev = cur;
    }
    prev[b.len()]
}

const MATH_FNS: &[&str] = &[
    "ln", "log10", "log2", "exp", "pow", "sqrt", "cbrt", "abs", "sin", "cos", "tan", "asin",
    "acos", "atan", "atan2", "sinh", "cosh", "tanh", "asinh", "acosh", "atanh", "hypot",
];
const BARE_FNS: &[&str] = &["min", "max", "if", "floor", "round", "ceil"];

/// A B-source hook: receives `v`/`i` and the call's arguments, returns the
/// replacement text (a `__d{k}` dependency slot).
type DepSink<'a> = &'a mut dyn FnMut(&str, &[String]) -> Result<String, String>;

/// Rewrite a `{...}` expression into evalexpr's dialect: bare integers become
/// floats, parameters fold to literals, SPICE function names map onto `math::`
/// builtins, `**` becomes `^`, and (with a `sink`) `v(...)`/`i(...)` become
/// dependency slots and `time` survives as a variable.
fn canonicalize(expr: &str, env: &ParamEnv, mut sink: Option<DepSink>) -> Result<String, String> {
    let s = expr.replace("**", "^");
    let b = s.as_bytes();
    let (mut out, mut i) = (String::with_capacity(s.len() + 8), 0);
    while i < b.len() {
        let c = b[i];
        if c == b'{' || c == b'}' {
            return Err("nested braces in expression".into());
        }
        if c.is_ascii_digit() || (c == b'.' && b.get(i + 1).is_some_and(u8::is_ascii_digit)) {
            let end = i + number_end(&s[i..]);
            let lit = &s[i..end];
            if b.get(end)
                .is_some_and(|d| d.is_ascii_alphabetic() || *d == b'_')
            {
                return Err(format!(
                    "engineering suffix inside a braced expression (`{lit}{}`)",
                    s[end..].chars().next().unwrap_or(' ')
                ));
            }
            out.push_str(lit);
            if !lit.contains(['.', 'e', 'E']) {
                out.push_str(".0");
            }
            i = end;
            continue;
        }
        if !(c.is_ascii_alphabetic() || c == b'_') {
            let ch = s[i..].chars().next().unwrap_or(' ');
            out.push(ch);
            i += ch.len_utf8();
            continue;
        }
        let start = i;
        i += b[i..]
            .iter()
            .take_while(|c| c.is_ascii_alphanumeric() || **c == b'_')
            .count();
        let (ident, low) = (&s[start..i], s[start..i].to_ascii_lowercase());
        let j = i + b[i..]
            .iter()
            .take_while(|c| c.is_ascii_whitespace())
            .count();
        if b.get(j) != Some(&b'(') {
            match env.get(&low) {
                _ if low == "time" && sink.is_some() => out.push_str("time"),
                Some(v) => out.push_str(&format!("({v:?})")),
                None => return Err(format!("undefined parameter `{low}`")),
            }
        } else if low == "v" || low == "i" {
            let Some(sink) = sink.as_mut() else {
                return Err(format!(
                    "`{ident}(...)` is only valid in a B-source expression"
                ));
            };
            let close = s[j..]
                .find(')')
                .map(|k| j + k)
                .ok_or_else(|| format!("unclosed `{ident}(`"))?;
            let args: Vec<String> = s[j + 1..close]
                .split(',')
                .map(|a| a.trim().to_string())
                .collect();
            if args.iter().any(|a| a.is_empty() || a.contains(['(', ' '])) {
                return Err(format!("malformed `{ident}(...)` argument list"));
            }
            out.push_str(&sink(&low, &args)?);
            i = close + 1;
        } else if low == "log" {
            return Err("`log` is ambiguous (ln vs log10); write `ln` or `log10`".into());
        } else if MATH_FNS.contains(&low.as_str()) {
            out.push_str(&format!("math::{low}"));
        } else if BARE_FNS.contains(&low.as_str()) {
            out.push_str(&low);
        } else {
            let (m, b) = (MATH_FNS.join(", "), BARE_FNS.join(", "));
            return Err(format!(
                "unsupported function `{ident}(` (supported: {m}, {b})"
            ));
        }
    }
    Ok(out)
}

/// Evaluate a braced expression's interior against `env`.
fn eval_expr(inner: &str, env: &ParamEnv) -> Result<f64, String> {
    let canon = canonicalize(inner, env, None)?;
    let tree =
        build_operator_tree(&canon).map_err(|e| format!("malformed expression `{inner}`: {e}"))?;
    match tree.eval() {
        Ok(Value::Float(f)) => Ok(f),
        Ok(Value::Int(i)) => Ok(i as f64),
        Ok(Value::Boolean(b)) => Ok(f64::from(u8::from(b))),
        Ok(v) => Err(format!("expression `{inner}` is not a number ({v:?})")),
        Err(e) => Err(format!("expression `{inner}` failed: {e}")),
    }
}

/// An element value token: `{expr}`, a suffixed number, or a parameter name.
fn value(line: &Line, tok: &str, env: &ParamEnv) -> Result<f64, SpiceError> {
    if let Some(inner) = braced(tok) {
        return eval_expr(inner, env).map_err(|e| line.err(e));
    }
    parse_spice_number(tok)
        .or_else(|| env.get(&tok.to_ascii_lowercase()).copied())
        .ok_or_else(|| line.err(format!("malformed number `{tok}`")))
}

/// A `.param`/default/override right-hand side: a suffixed number or an
/// expression, braces optional.
fn scalar(s: &str, env: &ParamEnv) -> Result<f64, String> {
    let inner = braced(s).unwrap_or(s);
    parse_spice_number(inner).map_or_else(|| eval_expr(inner, env), Ok)
}

fn number(line: &Line, tok: Option<&String>) -> Result<f64, SpiceError> {
    let t = tok.map_or("", String::as_str);
    parse_spice_number(t).ok_or_else(|| line.err(format!("expected a number, found `{t}`")))
}

/// `key=value` options after an element's positional fields, each evaluated
/// like a value; a bare token or an unknown key is an error.
fn options(
    line: &Line,
    toks: &[String],
    env: &ParamEnv,
    allowed: &[&str],
) -> Result<ParamEnv, SpiceError> {
    let mut out = HashMap::new();
    for t in toks {
        match t.split_once('=').map(|(k, v)| (k.to_ascii_lowercase(), v)) {
            Some((k, v)) if allowed.contains(&k.as_str()) => {
                out.insert(k, value(line, v, env)?);
            }
            _ => bail!(
                line,
                "unexpected token `{t}` (allowed options: {})",
                allowed.join(", ")
            ),
        }
    }
    Ok(out)
}

// --- first pass: cards -------------------------------------------------------

struct Param {
    name: String,
    value: String,
    line: Line,
}

struct Model {
    kind: String,
    params: ParamEnv,
}

impl Model {
    fn get(&self, k: &str) -> Option<f64> {
        self.params.get(k).copied()
    }
    fn get_or(&self, k: &str, d: f64) -> f64 {
        self.get(k).unwrap_or(d)
    }
}

struct Subckt {
    name: String,
    ports: Vec<String>,
    defaults: Vec<(String, String)>,
    params: Vec<Param>,
    body: Vec<Line>,
    header: Line,
}

#[derive(Default)]
struct Deck {
    models: HashMap<String, Model>,
    subckts: HashMap<String, Subckt>,
    params: Vec<Param>,
    elems: Vec<Line>,
    ic: Vec<Line>,
    nodeset: Vec<Line>,
    dc: Option<Line>,
}

/// Split `key=value` tokens; a malformed one is an error.
fn kv_pairs<'a>(line: &Line, toks: &'a [String]) -> Result<Vec<(String, &'a str)>, SpiceError> {
    toks.iter()
        .map(|t| match t.split_once('=') {
            Some((k, v)) if !k.is_empty() && !v.is_empty() => Ok((k.to_ascii_lowercase(), v)),
            _ => Err(line.err(format!("expected `name=value`, found `{t}`"))),
        })
        .collect()
}

fn parse_params(line: &Line, toks: &[String]) -> Result<Vec<Param>, SpiceError> {
    if toks.is_empty() {
        bail!(line, "`.param` card defines nothing");
    }
    let pair = |(name, v): (String, &str)| Param {
        name,
        value: v.to_string(),
        line: line.clone(),
    };
    Ok(kv_pairs(line, toks)?.into_iter().map(pair).collect())
}

fn add_model(
    models: &mut HashMap<String, Model>,
    line: &Line,
    toks: &[String],
) -> Result<(), SpiceError> {
    if toks.len() < 3 {
        bail!(line, "incomplete .model card (need name and type)");
    }
    let kind = toks[2].to_ascii_lowercase();
    let mut params = HashMap::new();
    for (k, v) in kv_pairs(line, &toks[3..])? {
        match parse_spice_number(v) {
            Some(x) => drop(params.insert(k, x)),
            None if v.starts_with(|c: char| c.is_ascii_digit() || "+-.{".contains(c)) => {
                bail!(line, "unparseable value for `{k}`: `{v}`")
            }
            None => {} // string metadata (mfg=, type=)
        }
    }
    if let Some(level) = params.get("level").filter(|l| **l != 1.0) {
        if matches!(kind.as_str(), "nmos" | "pmos") {
            bail!(
                line,
                "MOSFET LEVEL={level} is not implemented (only LEVEL=1)"
            );
        }
    }
    let name = toks[1].to_ascii_lowercase();
    if models
        .get(&name)
        .is_some_and(|p| p.kind != kind || p.params != params)
    {
        bail!(line, "conflicting redefinition of .model `{}`", toks[1]);
    }
    models.insert(name, Model { kind, params });
    Ok(())
}

/// Split `[params:] positional... k=v...` into the positional tokens and pairs.
type Pairs = Vec<(String, String)>;

/// Split `[params:] positional... k=v...` into the positional tokens and pairs.
fn positional_and_kv(line: &Line, toks: &[String]) -> Result<(Vec<String>, Pairs), SpiceError> {
    let rest: Vec<String> = toks
        .iter()
        .filter(|t| !t.eq_ignore_ascii_case("params:"))
        .cloned()
        .collect();
    let n = rest.iter().take_while(|t| !t.contains('=')).count();
    let pairs = kv_pairs(line, &rest[n..])?
        .into_iter()
        .map(|(k, v)| (k, v.to_string()))
        .collect();
    Ok((rest[..n].to_vec(), pairs))
}

fn subckt_header(line: &Line, toks: &[String]) -> Result<Subckt, SpiceError> {
    let Some(name) = toks.get(1).cloned() else {
        bail!(line, "`.subckt` needs a name");
    };
    let (ports, defaults) = positional_and_kv(line, &toks[2..])?;
    if ports
        .iter()
        .enumerate()
        .any(|(i, p)| ports[..i].iter().any(|q| q.eq_ignore_ascii_case(p)))
    {
        bail!(line, "a port is listed twice");
    }
    let (params, body, header) = (Vec::new(), Vec::new(), line.clone());
    Ok(Subckt {
        name,
        ports,
        defaults,
        params,
        body,
        header,
    })
}

fn collect(
    lines: Vec<Line>,
    circuit: &mut Circuit,
    d: &mut Directives,
) -> Result<Deck, SpiceError> {
    let mut deck = Deck::default();
    let mut cur: Option<Subckt> = None;
    for line in lines {
        let toks = tokenize(&line.text);
        let head = toks[0].to_ascii_lowercase();
        if let Some(sub) = cur.as_mut() {
            match head.as_str() {
                ".ends" => {
                    let s = cur.take().unwrap_or_else(|| unreachable!());
                    if deck
                        .subckts
                        .insert(s.name.to_ascii_lowercase(), s)
                        .is_some()
                    {
                        bail!(line, "duplicate `.subckt` definition");
                    }
                }
                ".model" => add_model(&mut deck.models, &line, &toks)?,
                ".param" => sub.params.extend(parse_params(&line, &toks[1..])?),
                h if h.starts_with('.') => {
                    bail!(line, "`{h}` is not allowed inside a `.subckt` body")
                }
                _ => sub.body.push(line),
            }
            continue;
        }
        match head.as_str() {
            ".subckt" => cur = Some(subckt_header(&line, &toks)?),
            ".ends" => bail!(line, "`.ends` without a matching `.subckt`"),
            ".model" => add_model(&mut deck.models, &line, &toks)?,
            ".param" => deck.params.extend(parse_params(&line, &toks[1..])?),
            ".temp" => circuit.temp_c = number(&line, toks.get(1))?,
            ".options" | ".option" => {
                for (k, v) in toks[1..].iter().filter_map(|t| t.split_once('=')) {
                    let slot = match k.to_ascii_lowercase().as_str() {
                        "reltol" => &mut d.reltol,
                        "abstol" => &mut d.abstol,
                        "vntol" => &mut d.vntol,
                        _ => continue,
                    };
                    *slot = Some(number(&line, Some(&v.to_string()))?);
                }
            }
            ".tran" => d.tran = Some(parse_tran(&line, &toks, d)?),
            ".ac" if d.ac.is_some() => bail!(line, "duplicate `.ac` card"),
            ".ac" => d.ac = Some(parse_ac(&line, &toks)?),
            ".dc" if deck.dc.is_some() => bail!(line, "duplicate `.dc` card"),
            ".dc" => deck.dc = Some(line),
            ".print" | ".plot" => {
                d.saw_plot |= head == ".plot";
                d.prints.push(parse_print(&line, head == ".plot")?);
            }
            ".ic" => deck.ic.push(line),
            ".nodeset" => deck.nodeset.push(line),
            ".end" | ".op" | ".title" | ".width" | ".save" => {}
            h if h.starts_with('.') => {
                let url = crate::docs_url("docs/spice-compat/compatibility.md");
                match h {
                    ".tf" | ".noise" | ".disto" | ".pz" | ".sens" | ".four" | ".meas"
                    | ".measure" => bail!(
                        line,
                        "unsupported directive `{h}`: this analysis is not implemented; see {url}"
                    ),
                    _ => bail!(
                        line,
                        "unrecognized directive `{h}`: refused, not ignored; see {url}"
                    ),
                }
            }
            _ => deck.elems.push(line),
        }
    }
    if let Some(s) = cur {
        bail!(
            s.header,
            "`.subckt {}` is never closed with `.ends`",
            s.name
        );
    }
    Ok(deck)
}

/// Resolve `.param` definitions on top of `base` in dependency order: a card
/// that fails (typically on a not-yet-resolved sibling) is retried after the
/// others; when a round makes no progress its error stands.
fn resolve_params(cards: &[Param], base: &ParamEnv) -> Result<ParamEnv, SpiceError> {
    let mut env = base.clone();
    let mut pending: Vec<&Param> = cards.iter().collect();
    while !pending.is_empty() {
        let mut next = Vec::new();
        for p in &pending {
            match scalar(&p.value, &env) {
                Ok(v) => drop(env.insert(p.name.clone(), v)),
                Err(e) => next.push((*p, e)),
            }
        }
        if next.len() == pending.len() {
            let (p, e) = &next[0];
            let names: Vec<&str> = next.iter().map(|(p, _)| p.name.as_str()).collect();
            let cycle = next.len() > 1
                || next
                    .iter()
                    .any(|(p, e)| e.contains(&format!("`{}`", p.name)));
            if cycle && e.contains("undefined parameter") {
                bail!(
                    p.line,
                    "`.param` dependency cycle among {}",
                    names.join(", ")
                );
            }
            bail!(p.line, "`.param {}`: {e}", p.name);
        }
        pending = next.into_iter().map(|(p, _)| p).collect();
    }
    Ok(env)
}

// --- second pass: elements ---------------------------------------------------

/// Name mapping for the deck being parsed: a subckt instance prefixes internal
/// nodes and element names and maps formal ports to the caller's nodes.
struct Scope {
    prefix: String,
    ports: HashMap<String, String>,
    env: Rc<ParamEnv>,
}

impl Scope {
    fn node(&self, tok: &str) -> String {
        if self.prefix.is_empty() || tok == "0" || tok.eq_ignore_ascii_case("gnd") {
            return tok.to_string();
        }
        self.ports
            .get(&tok.to_ascii_lowercase())
            .cloned()
            .unwrap_or_else(|| self.name(tok))
    }
    fn name(&self, tok: &str) -> String {
        if self.prefix.is_empty() {
            tok.to_string()
        } else {
            format!("{}.{tok}", self.prefix)
        }
    }
}

/// A deferred element-name reference (F/H control, B `i(...)`, K windings),
/// resolved once every device exists so forward references work. `want`
/// names the referent kind: `true` for an independent V source, else an
/// inductor.
struct Fixup {
    device: DeviceId,
    slot: usize,
    name: String,
    want_vsource: bool,
    line: Line,
}

struct Build<'a> {
    circuit: Circuit,
    deck: &'a Deck,
    fixups: Vec<Fixup>,
    /// Lowercased refdes -> defining line, to refuse duplicates.
    seen: HashMap<String, usize>,
}

/// Check a card has at least `n` tokens, naming the expected fields.
fn need(line: &Line, toks: &[String], n: usize, what: &str) -> Result<(), SpiceError> {
    if toks.len() < n {
        bail!(line, "need {what}");
    }
    Ok(())
}

/// Check a card has exactly `n` tokens.
fn exact(line: &Line, toks: &[String], n: usize, what: &str) -> Result<(), SpiceError> {
    need(line, toks, n, what)?;
    match toks.get(n) {
        Some(t) => bail!(line, "unexpected trailing token `{t}`"),
        None => Ok(()),
    }
}

impl Build<'_> {
    fn add(&mut self, line: &Line, dev: Device) -> Result<DeviceId, SpiceError> {
        if let Some(prev) = self.seen.insert(dev.name().to_ascii_lowercase(), line.no) {
            bail!(
                line,
                "duplicate element name `{}` (first defined at line {prev})",
                dev.name()
            );
        }
        Ok(self.circuit.add(dev))
    }

    /// The `N` node tokens after the name, interned through `scope`.
    fn nodes<const N: usize>(&mut self, toks: &[String], scope: &Scope) -> [NodeId; N] {
        std::array::from_fn(|i| self.circuit.node(&scope.node(&toks[i + 1])))
    }

    fn model(&self, line: &Line, name: &str, kinds: &[&str]) -> Result<&Model, SpiceError> {
        let m = self.deck.models.get(&name.to_ascii_lowercase());
        let m = m.ok_or_else(|| line.err(format!("references undefined .model `{name}`")))?;
        if !kinds.contains(&m.kind.as_str()) {
            let wanted = if kinds == ["d"] {
                "not a diode model".to_string()
            } else {
                format!("not one of {}", kinds.join("/"))
            };
            bail!(line, "`.model {name}` is a `{}` model, {wanted}", m.kind);
        }
        Ok(m)
    }

    fn fixup(
        &mut self,
        device: DeviceId,
        slot: usize,
        name: String,
        want_vsource: bool,
        line: &Line,
    ) {
        let line = line.clone();
        self.fixups.push(Fixup {
            device,
            slot,
            name,
            want_vsource,
            line,
        });
    }

    fn element(
        &mut self,
        line: &Line,
        scope: &Scope,
        chain: &mut Vec<String>,
    ) -> Result<(), SpiceError> {
        let toks = tokenize(&line.text);
        let env = &*scope.env;
        let name = scope.name(&toks[0]);
        let kind = toks[0].chars().next().unwrap_or(' ').to_ascii_uppercase();
        let poly =
            |t: &String| matches!(t.to_ascii_lowercase().as_str(), "poly" | "value" | "table");
        if matches!(kind, 'E' | 'G' | 'F' | 'H' | 'B') && toks.iter().skip(3).any(poly) {
            let what = match kind {
                'E' | 'G' => "the POLY/VALUE/TABLE controlled-source form is unsupported (only `n+ n- nc+ nc- gain`)",
                'F' | 'H' => "the `POLY` controlled-source form is unsupported (only `n+ n- vname gain`)",
                _ => "the POLY/TABLE/VALUE B-source form is unsupported (only `V={expr}` / `I={expr}`)",
            };
            bail!(line, "{what}");
        }
        let placeholder = DeviceId(u32::MAX);
        let dev = match kind {
            'R' | 'C' | 'L' => {
                need(line, &toks, 4, "two nodes and a value")?;
                let [a, b] = self.nodes(&toks, scope);
                let v = value(line, &toks[3], env)?;
                let keys: &[&str] = if kind == 'R' { &["tc", "tc1"] } else { &["ic"] };
                let kv = options(line, &toks[4..], env, keys)?;
                let ic = kv.get("ic").copied();
                match kind {
                    // R=0 is a jumper, never an open.
                    'R' => Device::Resistor {
                        name,
                        a,
                        b,
                        ohms: v.max(1e-6),
                        tc1: kv.get("tc1").or(kv.get("tc")).copied(),
                    },
                    'C' => Device::Capacitor {
                        name,
                        a,
                        b,
                        farads: v,
                        ic,
                    },
                    _ => Device::Inductor {
                        name,
                        a,
                        b,
                        henries: v,
                        ic,
                    },
                }
            }
            'V' | 'I' => {
                need(line, &toks, 3, "two nodes")?;
                let [p, n] = self.nodes(&toks, scope);
                let (kind, ac) = source_spec(line, &toks[3..], env)?;
                let is_v = toks[0].starts_with(['V', 'v']);
                let dev = if is_v {
                    Device::Vsource { name, p, n, kind }
                } else {
                    Device::Isource { name, p, n, kind }
                };
                let id = self.add(line, dev)?;
                self.circuit.ac_stimulus.extend(ac.map(|stim| (id, stim)));
                return Ok(());
            }
            'D' => {
                exact(line, &toks, 4, "anode, cathode, model")?;
                let [a, k] = self.nodes(&toks, scope);
                let model = diode_model(self.model(line, &toks[3], &["d"])?);
                Device::Diode { name, a, k, model }
            }
            'Q' => {
                exact(line, &toks, 5, "collector, base, emitter, model")?;
                let [c, b, e] = self.nodes(&toks, scope);
                let model = bjt_model(self.model(line, &toks[4], &["npn", "pnp"])?);
                Device::Bjt {
                    name,
                    c,
                    b,
                    e,
                    model,
                }
            }
            'M' => {
                need(line, &toks, 6, "drain, gate, source, bulk, model")?;
                let [d, g, s, b] = self.nodes(&toks, scope);
                let kv = options(line, &toks[6..], env, &["l", "w"])?;
                let model = mosfet_model(self.model(line, &toks[5], &["nmos", "pmos"])?, &kv);
                Device::Mosfet {
                    name,
                    d,
                    g,
                    s,
                    b: Some(b),
                    model,
                }
            }
            'S' => {
                exact(line, &toks, 6, "a, b, ctrl+, ctrl-, model")?;
                let [a, b, ctrl_p, ctrl_n] = self.nodes(&toks, scope);
                let m = self.model(line, &toks[5], &["sw", "vswitch"])?;
                let (vt, vh) = (m.get_or("vt", 0.0), m.get_or("vh", 0.0).abs());
                let (ron, roff) = (m.get_or("ron", 1.0), m.get_or("roff", 1e12));
                Device::VSwitch {
                    name,
                    a,
                    b,
                    ctrl_p,
                    ctrl_n,
                    von: vt + vh,
                    voff: vt - vh,
                    ron,
                    roff,
                }
            }
            'E' | 'G' => {
                exact(line, &toks, 6, "n+, n-, nc+, nc-, gain")?;
                let [p, n, cp, cn] = self.nodes(&toks, scope);
                let gain = value(line, &toks[5], env)?;
                if kind == 'G' {
                    Device::Vccs {
                        name,
                        p,
                        n,
                        cp,
                        cn,
                        gm: gain,
                    }
                } else if p == n {
                    bail!(line, "`{name}` shorts its own output port (n+ == n-)");
                } else {
                    Device::Vcvs {
                        name,
                        p,
                        n,
                        cp,
                        cn,
                        gain,
                    }
                }
            }
            'F' | 'H' => {
                exact(line, &toks, 5, "n+, n-, a controlling V-source name, gain")?;
                let [p, n] = self.nodes(&toks, scope);
                let (ctrl_src, gain) = (placeholder, value(line, &toks[4], env)?);
                let dev = if kind == 'F' {
                    Device::Cccs {
                        name,
                        p,
                        n,
                        ctrl_src,
                        gain,
                    }
                } else if p == n {
                    bail!(line, "`{name}` shorts its own output port (n+ == n-)");
                } else {
                    Device::Ccvs {
                        name,
                        p,
                        n,
                        ctrl_src,
                        transres: gain,
                    }
                };
                let id = self.add(line, dev)?;
                self.fixup(id, 0, scope.name(&toks[3]), true, line);
                return Ok(());
            }
            'K' => {
                exact(
                    line,
                    &toks,
                    4,
                    "two inductor names and a coupling coefficient",
                )?;
                let k = value(line, &toks[3], env)?;
                if toks[1].eq_ignore_ascii_case(&toks[2]) {
                    bail!(line, "coupling couples an inductor to itself");
                }
                if !(k > 0.0 && k <= 1.0) {
                    bail!(line, "coupling coefficient k={k} is outside 0 < k <= 1");
                }
                let (l1, l2) = (placeholder, placeholder);
                let id = self.add(line, Device::Coupling { name, l1, l2, k })?;
                for (slot, t) in toks[1..3].iter().enumerate() {
                    self.fixup(id, slot, scope.name(t), false, line);
                }
                return Ok(());
            }
            'B' => return self.behavioral(line, &toks, scope),
            'X' => return self.instance(line, &toks, scope, chain),
            other => bail!(line, "unknown element type `{other}`"),
        };
        self.add(line, dev).map(|_| ())
    }

    /// `Bxxx n+ n- V={expr}` / `I={expr}`: the expression is canonicalized with
    /// `v(...)`/`i(...)` turned into positional dependency slots.
    fn behavioral(
        &mut self,
        line: &Line,
        toks: &[String],
        scope: &Scope,
    ) -> Result<(), SpiceError> {
        need(line, toks, 4, "n+, n-, and `V={expr}` or `I={expr}`")?;
        let [p, n] = self.nodes(toks, scope);
        let name = scope.name(&toks[0]);
        let (out, expr) = toks[3].split_once('=').unwrap_or((&toks[3], ""));
        let output = match out.to_ascii_lowercase().as_str() {
            "v" => BOutput::Voltage,
            "i" => BOutput::Current,
            _ => bail!(line, "this B-source form is unsupported: the output must be `V={{expr}}` or `I={{expr}}`"),
        };
        let Some(inner) = braced(expr) else {
            bail!(
                line,
                "this B-source form is unsupported: the expression must be brace-wrapped (`V={{expr}}`)"
            );
        };
        exact(line, toks, 4, "")?;
        if output == BOutput::Voltage && p == n {
            bail!(
                line,
                "`{name}` shorts its own output port with a voltage output"
            );
        }
        let (mut deps, mut names): (Vec<BDep>, Vec<String>) = (Vec::new(), Vec::new());
        let mut slots: HashMap<String, usize> = HashMap::new();
        let circuit = &mut self.circuit;
        let mut sink = |f: &str, args: &[String]| -> Result<String, String> {
            let mut slot = |a: &str| {
                let (key, dep) = if f == "v" {
                    let id = circuit.node(&scope.node(a));
                    (format!("v:{}", id.0), BDep::Volt(id))
                } else {
                    (
                        format!("i:{}", scope.name(a).to_ascii_lowercase()),
                        BDep::Branch(DeviceId(u32::MAX)),
                    )
                };
                *slots.entry(key).or_insert_with(|| {
                    if f == "i" {
                        names.push(scope.name(a));
                    }
                    deps.push(dep);
                    deps.len() - 1
                })
            };
            match (f, args) {
                (_, [a]) => Ok(format!("__d{}", slot(a))),
                ("v", [a, b]) => Ok(format!("(__d{} - __d{})", slot(a), slot(b))),
                _ => Err(format!(
                    "`{f}(...)` takes one argument (or a differential pair for `v`)"
                )),
            }
        };
        let canon = canonicalize(inner, &scope.env, Some(&mut sink)).map_err(|e| line.err(e))?;
        let expr = CompiledExpr::compile(&canon).map_err(|e| line.err(e))?;
        let id = self.add(
            line,
            Device::Behavioral {
                name,
                p,
                n,
                output,
                expr,
                deps,
            },
        )?;
        for (slot, vname) in names.into_iter().enumerate() {
            self.fixup(id, slot, vname, true, line);
        }
        Ok(())
    }

    /// `Xxxx nodes... NAME [k=v ...]`: parse the subckt body under a nested scope.
    fn instance(
        &mut self,
        line: &Line,
        toks: &[String],
        scope: &Scope,
        chain: &mut Vec<String>,
    ) -> Result<(), SpiceError> {
        let inst = scope.name(&toks[0]);
        let (positional, overrides) = positional_and_kv(line, &toks[1..])?;
        let Some((sub_name, actual)) = positional.split_last() else {
            bail!(line, "`X` needs at least one node and a subckt name");
        };
        let key = sub_name.to_ascii_lowercase();
        let deck = self.deck;
        let def = deck.subckts.get(&key);
        let def =
            def.ok_or_else(|| line.err(format!("references undefined subckt `{sub_name}`")))?;
        if actual.len() != def.ports.len() {
            bail!(
                line,
                "connects {} nodes but subckt `{}` has {} ports",
                actual.len(),
                def.name,
                def.ports.len()
            );
        }
        if chain.contains(&key) || chain.len() > 50 {
            bail!(line, "subckt `{}` instantiates itself", def.name);
        }
        let mut env: ParamEnv = (*scope.env).clone();
        for (k, v) in &overrides {
            env.insert(k.clone(), scalar(v, &scope.env).map_err(|e| line.err(e))?);
        }
        for (k, v) in def
            .defaults
            .iter()
            .filter(|(k, _)| !overrides.iter().any(|(o, _)| o == k))
        {
            env.insert(k.clone(), scalar(v, &env).map_err(|e| line.err(e))?);
        }
        let ports = def
            .ports
            .iter()
            .zip(actual)
            .map(|(p, a)| (p.to_ascii_lowercase(), scope.node(a)))
            .collect();
        let env = Rc::new(resolve_params(&def.params, &env)?);
        let sub = Scope {
            prefix: inst.clone(),
            ports,
            env,
        };
        chain.push(key);
        for body in &def.body {
            self.element(body, &sub, chain).map_err(|mut e| {
                e.msg = format!(
                    "{} (in subckt {} as `{inst}`, line {})",
                    e.msg, def.name, line.no
                );
                e
            })?;
        }
        chain.pop();
        Ok(())
    }

    fn resolve_fixups(&mut self) -> Result<(), SpiceError> {
        let mut index: HashMap<String, Vec<DeviceId>> = HashMap::new();
        for (id, d) in self.circuit.iter() {
            index
                .entry(d.name().to_ascii_lowercase())
                .or_default()
                .push(id);
        }
        for f in &self.fixups {
            let id = match index.get(&f.name.to_ascii_lowercase()).map(Vec::as_slice) {
                Some([id]) => *id,
                Some(_) => bail!(f.line, "element name `{}` is ambiguous", f.name),
                None => bail!(f.line, "references undefined element `{}`", f.name),
            };
            let dev = &self.circuit.devices[id.0 as usize];
            if f.want_vsource && !matches!(dev, Device::Vsource { .. }) {
                bail!(f.line, "`{}` is not an independent voltage source (sense a current with a zero-volt `Vsense a b 0`)", f.name);
            }
            if !f.want_vsource && !matches!(dev, Device::Inductor { .. }) {
                bail!(f.line, "`{}` is not an inductor", f.name);
            }
            self.circuit.devices[f.device.0 as usize].retarget_controlling_source_slot(f.slot, id);
        }
        Ok(())
    }
}

/// A source card's spec: an optional `AC <mag> [phase]` stimulus anywhere on
/// the card, and the time-domain function (`DC v`, bare `v`, `SIN`, `PULSE`,
/// `PWL`); when several appear (`DC 0 SIN(...) AC 1`) the function wins.
fn source_spec(
    line: &Line,
    toks: &[String],
    env: &ParamEnv,
) -> Result<(SourceKind, Option<AcStim>), SpiceError> {
    let mut rest = toks.to_vec();
    let mut ac = None;
    if let Some(i) = rest.iter().position(|t| t.eq_ignore_ascii_case("ac")) {
        let vals: Vec<f64> = rest[i + 1..]
            .iter()
            .take(2)
            .map_while(|t| value(line, t, env).ok())
            .collect();
        ac = Some(AcStim {
            mag: vals.first().copied().unwrap_or(1.0),
            phase_deg: vals.get(1).copied().unwrap_or(0.0),
        });
        rest.drain(i..=i + vals.len());
    }
    let is_fn = |t: &String| {
        matches!(
            t.to_ascii_lowercase().as_str(),
            "sin" | "sine" | "pulse" | "pwl"
        )
    };
    let start = rest.iter().skip(1).position(is_fn).map_or(0, |p| p + 1);
    let rest = &rest[start..];
    let Some(head) = rest.first() else {
        return Ok((SourceKind::Dc(0.0), ac));
    };
    let mut a = rest[1..]
        .iter()
        .map(|t| value(line, t, env))
        .collect::<Result<Vec<_>, _>>()?;
    let given = a.len();
    a.resize(given.max(7), 0.0);
    let [a0, a1, a2, a3, a4, a5, a6, ..] = a[..] else {
        unreachable!()
    };
    let kind = match head.to_ascii_lowercase().as_str() {
        "dc" => SourceKind::Dc(a0),
        "sin" | "sine" => SourceKind::Sin {
            offset: a0,
            amplitude: a1,
            freq: a2,
            delay: a3,
            theta: a4,
            phase: a5,
        },
        "pulse" => SourceKind::Pulse {
            v1: a0,
            v2: a1,
            delay: a2,
            rise: a3,
            fall: a4,
            width: if given < 6 { f64::INFINITY } else { a5 },
            period: a6,
        },
        "pwl" => SourceKind::Pwl(
            a[..given]
                .chunks_exact(2)
                .map(|c| PwlPoint { t: c[0], v: c[1] })
                .collect(),
        ),
        _ if given > 0 => bail!(
            line,
            "unexpected token `{}` after the source value",
            rest[1]
        ),
        _ => SourceKind::Dc(value(line, head, env)?),
    };
    Ok((kind, ac))
}

// --- model cards -> device models ---------------------------------------------

/// `pick!(m, d; a, b)`: `d` with each named field replaced by the card's value
/// when the card carries it.
macro_rules! pick {
    ($m:expr, $d:expr; $($f:ident),*) => {{ let mut d = $d; $( d.$f = $m.get_or(stringify!($f), d.$f); )* d }};
}

fn diode_model(m: &Model) -> DiodeModel {
    let mut d = pick!(m, DiodeModel::default(); is, n, rs, cjo, vj, m, tt, bv, xti, eg);
    d.ibv = m.get("ibv").filter(|v| *v > 0.0);
    d
}

fn bjt_model(m: &Model) -> BjtModel {
    let mut d = pick!(m, BjtModel::default(); is, bf, br, vaf, var, nf, nr, rb, re, rc, cje, cjc, tf, tr, ise, ne, isc, nc, xti, eg);
    d.polarity = if m.kind == "pnp" {
        Polarity::P
    } else {
        Polarity::N
    };
    // SPICE spells "no high-injection knee" as IKF=0; the model uses INFINITY.
    let knee =
        |v: Option<f64>, dflt: f64| v.map_or(dflt, |x| if x > 0.0 { x } else { f64::INFINITY });
    d.ikf = knee(m.get("ikf"), d.ikf);
    d.ikr = knee(m.get("ikr"), d.ikr);
    d
}

fn mosfet_model(m: &Model, kv: &ParamEnv) -> MosfetModel {
    const EPS_OX: f64 = 3.9 * 8.854_214_871e-12;
    let mut d = pick!(m, MosfetModel::default(); kp, lambda, gamma, phi, cbd, cbs, pb, mj, rd, rs);
    d.polarity = if m.kind == "pmos" {
        Polarity::P
    } else {
        Polarity::N
    };
    // Cards state VTO in device convention (negative for an enhancement PMOS);
    // the solver stores it polarity-folded.
    d.vto = m.get("vto").map_or(d.vto, |v| d.polarity.sign() * v);
    let l = kv.get("l").copied().or(m.get("l")).unwrap_or(1.0);
    let w = kv.get("w").copied().or(m.get("w")).unwrap_or(1.0);
    d.w_over_l = if l != 0.0 { w / l } else { 1.0 };
    d.cgs_ov = m.get_or("cgso", 0.0) * w;
    d.cgd_ov = m.get_or("cgdo", 0.0) * w;
    d.c_ox = m
        .get("tox")
        .filter(|t| *t > 0.0)
        .map_or(0.0, |tox| EPS_OX / tox * w * l);
    d.body_is = m.get_or("is", d.body_is);
    d
}

// --- directives --------------------------------------------------------------

fn parse_tran(
    line: &Line,
    toks: &[String],
    d: &mut Directives,
) -> Result<TranDirective, SpiceError> {
    let mut nums = Vec::new();
    for t in &toks[1..] {
        if t.eq_ignore_ascii_case("uic") {
            d.use_initial_conditions = true;
        } else {
            nums.push(number(line, Some(t))?);
        }
    }
    let [tstep, tstop, ..] = nums[..] else {
        bail!(line, "`.tran` needs at least tstep and tstop");
    };
    let (tstart, tmax) = (nums.get(2).copied().unwrap_or(0.0), nums.get(3).copied());
    Ok(TranDirective {
        tstep,
        tstop,
        tstart,
        tmax,
    })
}

fn parse_ac(line: &Line, toks: &[String]) -> Result<AcDirective, SpiceError> {
    exact(
        line,
        toks,
        5,
        "`.ac <dec|oct|lin> <points> <fstart> <fstop>`",
    )?;
    let sweep = match toks[1].to_ascii_lowercase().as_str() {
        "dec" => AcSweep::Decade,
        "oct" => AcSweep::Octave,
        "lin" => AcSweep::Linear,
        other => bail!(line, "unknown `.ac` sweep type `{other}`"),
    };
    let points = number(line, toks.get(2))?;
    let (fstart, fstop) = (number(line, toks.get(3))?, number(line, toks.get(4))?);
    if points < 1.0 || points.fract() != 0.0 {
        bail!(line, "`.ac` point count must be a positive integer");
    }
    if fstart <= 0.0 || fstop <= fstart {
        bail!(line, "`.ac` needs 0 < fstart < fstop");
    }
    Ok(AcDirective {
        sweep,
        points: points as usize,
        fstart,
        fstop,
    })
}

fn parse_print(line: &Line, is_plot: bool) -> Result<PrintRequest, SpiceError> {
    let toks: Vec<String> = line.text.split_whitespace().map(str::to_string).collect();
    let analysis = toks
        .get(1)
        .map(|t| t.to_ascii_lowercase())
        .unwrap_or_default();
    if !matches!(analysis.as_str(), "op" | "dc" | "ac" | "tran") {
        bail!(
            line,
            "expected an analysis type (op/dc/ac/tran) and output variables"
        );
    }
    if toks.len() < 3 {
        bail!(line, "no output variables");
    }
    Ok(PrintRequest {
        analysis,
        vars: toks[2..].to_vec(),
        is_plot,
    })
}

fn parse_dc(line: &Line, circuit: &Circuit) -> Result<DcDirective, SpiceError> {
    let toks = tokenize(&line.text);
    let args = &toks[1..];
    if args.len() != 4 && args.len() != 8 {
        bail!(
            line,
            "`.dc` needs <src> <start> <stop> <step>, optionally twice for a nested sweep"
        );
    }
    let group = |g: &[String]| -> Result<DcSweep, SpiceError> {
        let name = g[0].clone();
        let (start, stop, step) = (
            number(line, g.get(1))?,
            number(line, g.get(2))?,
            number(line, g.get(3))?,
        );
        let mut ids = circuit
            .iter()
            .filter(|(_, d)| d.name().eq_ignore_ascii_case(&name))
            .map(|(id, _)| id);
        let source = match (ids.next(), ids.next()) {
            (Some(id), None) => id,
            (None, _) => bail!(line, "`.dc` sweep source `{name}` does not exist"),
            _ => bail!(line, "`.dc` sweep source `{name}` is ambiguous"),
        };
        if !matches!(
            circuit.devices[source.0 as usize],
            Device::Vsource { .. } | Device::Isource { .. }
        ) {
            bail!(
                line,
                "`.dc` can only sweep an independent V or I source; `{name}` is not one"
            );
        }
        if step == 0.0 || (stop != start && (stop - start).signum() != step.signum()) {
            bail!(
                line,
                "`.dc {name}` step {step} cannot reach stop {stop} from start {start}"
            );
        }
        Ok(DcSweep {
            source,
            name,
            start,
            stop,
            step,
        })
    };
    let outer = if args.len() == 8 {
        Some(group(&args[4..])?)
    } else {
        None
    };
    Ok(DcDirective {
        inner: group(&args[..4])?,
        outer,
    })
}

/// `.ic`/`.nodeset V(node)=value ...` against the flattened node table.
fn ic_values(
    cards: &[Line],
    circuit: &Circuit,
    env: &ParamEnv,
) -> Result<Vec<(NodeId, f64)>, SpiceError> {
    let mut out = Vec::new();
    for line in cards {
        let toks = tokenize(&line.text.replace(")=", ") "));
        if toks.len() < 4 {
            bail!(line, "card sets nothing");
        }
        for g in toks[1..].chunks(3) {
            let [v, node, val] = g else {
                bail!(line, "expected `V(node)=value` groups");
            };
            if !v.eq_ignore_ascii_case("v") {
                bail!(line, "expected `V(node)=value` groups");
            }
            let id = circuit
                .find_node(node)
                .ok_or_else(|| line.err(format!("unknown node `{node}`")))?;
            out.push((id, value(line, val, env)?));
        }
    }
    Ok(out)
}
#[cfg(test)]
mod tests {
    use super::*;

    fn load(net: &str) -> Circuit {
        SpiceLoader::load(net).unwrap_or_else(|e| panic!("{e}"))
    }
    fn err(net: &str) -> String {
        SpiceLoader::load(net).expect_err("must refuse").to_string()
    }
    fn dev<'a>(c: &'a Circuit, name: &str) -> &'a Device {
        c.devices
            .iter()
            .find(|d| d.name() == name)
            .unwrap_or_else(|| panic!("no {name}"))
    }
    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() <= 1e-9 * b.abs().max(1e-30)
    }

    #[test]
    fn numbers_and_suffixes() {
        for (s, v) in [
            ("1k", 1e3),
            ("1meg", 1e6),
            ("1m", 1e-3),
            ("2.2u", 2.2e-6),
            ("1e-12", 1e-12),
            ("4.7nF", 4.7e-9),
            ("5A", 5.0),
            ("100mA", 0.1),
            ("2fF", 2e-15),
            ("1kohm", 1e3),
            ("3.3V", 3.3),
            ("1mil", 25.4e-6),
            ("+5", 5.0),
            (".5", 0.5),
            ("7.", 7.0),
        ] {
            assert!(close(parse_spice_number(s).unwrap(), v), "{s}");
        }
        for s in ["1kk", "1x", "1kfoo", "2.2uu", "abc", "", "e5", "1e"] {
            assert_eq!(parse_spice_number(s), None, "{s}");
        }
        assert_eq!(levenshtein("kitten", "sitting"), 3);
    }

    #[test]
    fn passive_elements_with_options() {
        let c = load("t\nV1 in 0 5\nR1 in out 1k tc1=1e-3\nC1 out 0 10n IC=2\nL1 out x 1m ic=0.5\nR0 x 0 0\n.end\n");
        assert!(
            matches!(dev(&c, "R1"), Device::Resistor { ohms, tc1: Some(t), .. } if *ohms == 1e3 && *t == 1e-3)
        );
        assert!(
            matches!(dev(&c, "C1"), Device::Capacitor { farads, ic: Some(2.0), .. } if close(*farads, 1e-8))
        );
        assert!(
            matches!(dev(&c, "L1"), Device::Inductor { henries, ic: Some(0.5), .. } if close(*henries, 1e-3))
        );
        assert!(matches!(dev(&c, "R0"), Device::Resistor { ohms, .. } if *ohms == 1e-6));
        assert_eq!(c.node_count(), 4);
        assert!(err("t\nR1 a 0 1k oops\n").contains("unexpected token `oops`"));
        assert!(err("t\nR1 a 0 1k foo=2\n").contains("unexpected token"));
        assert!(err("t\nR1 a 0 1x\n").contains("malformed number `1x`"));
        assert!(err("t\nR1 a 0\n").contains("need two nodes and a value"));
        assert!(err("t\nR1 a 0 1k\nr1 b 0 2k\n").contains("duplicate element name"));
        assert!(err("t\nW1 a 0 1k\n").contains("unknown element type `W`"));
    }

    #[test]
    fn sources_functions_and_ac_stimulus() {
        let c = load("t\nV1 a 0 DC 5\nV2 b 0 3\nV3 c 0 SIN(0 1 1k 1u 2 30)\nI1 d 0 PULSE(0 1 1u 2n 3n 4u 5u)\n\
                      V4 e 0 PWL(0 0 1u 5 2u 0)\nV5 f 0 DC 0 SIN( 0 1 1k 0 0 0 ) AC 1 45\nV6 g 0 AC\nI2 h 0\n\
                      V7 j 0 PULSE(0 1)\n");
        assert!(matches!(
            dev(&c, "V1"),
            Device::Vsource {
                kind: SourceKind::Dc(5.0),
                ..
            }
        ));
        assert!(matches!(
            dev(&c, "V2"),
            Device::Vsource {
                kind: SourceKind::Dc(3.0),
                ..
            }
        ));
        assert!(matches!(
            dev(&c, "V3"),
            Device::Vsource {
                kind: SourceKind::Sin {
                    amplitude: 1.0,
                    freq: 1e3,
                    theta: 2.0,
                    phase: 30.0,
                    ..
                },
                ..
            }
        ));
        assert!(
            matches!(dev(&c, "I1"), Device::Isource { kind: SourceKind::Pulse { v2: 1.0, period, .. }, .. } if close(*period, 5e-6))
        );
        assert!(
            matches!(dev(&c, "V4"), Device::Vsource { kind: SourceKind::Pwl(p), .. } if p.len() == 3 && p[1].v == 5.0)
        );
        assert!(matches!(
            dev(&c, "V5"),
            Device::Vsource {
                kind: SourceKind::Sin { .. },
                ..
            }
        ));
        assert!(matches!(
            dev(&c, "I2"),
            Device::Isource {
                kind: SourceKind::Dc(0.0),
                ..
            }
        ));
        assert!(
            matches!(dev(&c, "V7"), Device::Vsource { kind: SourceKind::Pulse { width, .. }, .. } if width.is_infinite())
        );
        assert_eq!(c.ac_stimulus.len(), 2);
        assert_eq!(
            c.ac_stimulus[0].1,
            AcStim {
                mag: 1.0,
                phase_deg: 45.0
            }
        );
        assert_eq!(
            c.ac_stimulus[1].1,
            AcStim {
                mag: 1.0,
                phase_deg: 0.0
            }
        );
        assert!(err("t\nV1 a 0 DC ac=1\n").contains("malformed number"));
        assert!(err("t\nV1 a 0 5 6\n").contains("unexpected token `6`"));
    }

    #[test]
    fn model_cards_map_onto_device_models() {
        let c = load(
            "t\nD1 a 0 DX\nQ1 c b e QP\nM1 d g s s MP L=2u W=20u\nS1 a b c 0 SW1\n\
                      .model DX D(IS=1e-15 N=1.5 CJO=4p BV=6 IBV=1m)\n\
                      .MODEL QP PNP (IS=1e-14,BF=50,IKF=0)\n\
                      .model MP PMOS(VTO=-0.7 KP=1e-3 TOX=50n CGSO=1e-9)\n\
                      .model SW1 SW(VT=1.5 VH=0.5 RON=10 ROFF=1e9)\n",
        );
        let Device::Diode { model: d, .. } = dev(&c, "D1") else {
            panic!()
        };
        assert!(
            d.is == 1e-15
                && d.n == 1.5
                && close(d.cjo, 4e-12)
                && d.bv == 6.0
                && d.ibv == Some(1e-3)
        );
        let Device::Bjt { model: q, .. } = dev(&c, "Q1") else {
            panic!()
        };
        assert!(q.polarity == Polarity::P && q.bf == 50.0 && q.ikf.is_infinite());
        let Device::Mosfet {
            model: m,
            b: Some(_),
            ..
        } = dev(&c, "M1")
        else {
            panic!()
        };
        assert!(m.polarity == Polarity::P && m.vto == 0.7 && close(m.w_over_l, 10.0));
        assert!(close(m.cgs_ov, 2e-14) && m.c_ox > 0.0 && m.has_gate_charge());
        let Device::VSwitch {
            von,
            voff,
            ron,
            roff,
            ..
        } = dev(&c, "S1")
        else {
            panic!()
        };
        assert!(*von == 2.0 && *voff == 1.0 && *ron == 10.0 && *roff == 1e9);
        assert!(err("t\nD1 a 0 NOPE\n").contains("undefined .model `NOPE`"));
        assert!(err("t\nD1 a 0 Q\n.model Q NPN(BF=1)\n").contains("not a diode model"));
        assert!(err("t\nM1 d g s b MM\n.model MM NMOS(LEVEL=3)\n").contains("LEVEL=1"));
        assert!(err("t\n.model X D(IS=1x)\n").contains("unparseable value for `is`"));
        assert!(err("t\n.model X D(IS=1)\n.model X D(IS=2)\n").contains("conflicting redefinition"));
        assert!(SpiceLoader::load("t\n.model X D(IS=1)\n.model X D(IS=1)\n").is_ok());
    }

    #[test]
    fn controlled_sources_and_coupling_resolve_by_name() {
        let c = load(
            "t\nE1 a 0 b 0 2\nG1 c 0 d 0 1m\nF1 e 0 Vs 2\nH1 f 0 Vs 3k\nK1 L1 L2 0.9\n\
                      L1 p 0 1m\nL2 s 0 4m\nVs x 0 0\n",
        );
        let vs = c.iter().find(|(_, d)| d.name() == "Vs").unwrap().0;
        assert!(matches!(dev(&c, "E1"), Device::Vcvs { gain: 2.0, .. }));
        assert!(matches!(dev(&c, "G1"), Device::Vccs { gm, .. } if close(*gm, 1e-3)));
        assert!(
            matches!(dev(&c, "F1"), Device::Cccs { ctrl_src, gain: 2.0, .. } if *ctrl_src == vs)
        );
        assert!(
            matches!(dev(&c, "H1"), Device::Ccvs { ctrl_src, transres, .. } if *ctrl_src == vs && *transres == 3e3)
        );
        let Device::Coupling { l1, l2, k, .. } = dev(&c, "K1") else {
            panic!()
        };
        assert!(
            c.devices[l1.0 as usize].name() == "L1"
                && c.devices[l2.0 as usize].name() == "L2"
                && *k == 0.9
        );
        assert!(err("t\nF1 a 0 Vnope 1\n").contains("undefined element `Vnope`"));
        assert!(err("t\nF1 a 0 R1 1\nR1 a 0 1k\n").contains("not an independent voltage source"));
        assert!(err("t\nK1 L1 R1 0.5\nL1 a 0 1m\nR1 b 0 1\n").contains("not an inductor"));
        assert!(err("t\nK1 L1 L2 1.5\nL1 a 0 1m\nL2 b 0 1m\n").contains("outside 0 < k <= 1"));
        assert!(err("t\nE1 a 0 POLY(1) b 0 1 2\n").contains("POLY"));
        assert!(err("t\nE1 a a b 0 2\n").contains("shorts its own output"));
    }

    #[test]
    fn behavioral_sources_canonicalize() {
        let c = load("t\n.param g=2\nB1 out 0 V={g*tanh(v(a)) + 3*i(Vs) - v(a,b)**2 + 1/2}\nB2 y 0 I={sin(time)}\n\
                      Vs a 0 1\nRb b 0 1\n");
        let Device::Behavioral {
            expr,
            deps,
            output: BOutput::Voltage,
            ..
        } = dev(&c, "B1")
        else {
            panic!()
        };
        assert_eq!(
            expr.src(),
            "(2.0)*math::tanh(__d0) + 3.0*__d1 - (__d0 - __d2)^2.0 + 1.0/2.0"
        );
        let vs = c.iter().find(|(_, d)| d.name() == "Vs").unwrap().0;
        assert_eq!(deps[1], BDep::Branch(vs));
        assert!(matches!(deps[0], BDep::Volt(n) if c.node_name(n) == "a"));
        assert!(
            matches!(dev(&c, "B2"), Device::Behavioral { expr, .. } if expr.src() == "math::sin(time)")
        );
        assert!(err("t\nB1 a 0 V=v(a)\n").contains("brace-wrapped"));
        assert!(err("t\nB1 a 0 V={2k*v(a)}\n").contains("engineering suffix"));
        assert!(err("t\nB1 a 0 V={foo(v(a))}\n").contains("unsupported function `foo(`"));
        assert!(err("t\nB1 a 0 V={log(v(a))}\n").contains("ambiguous"));
        assert!(err("t\nB1 a 0 V={nope}\n").contains("undefined parameter `nope`"));
        assert!(err("t\nB1 a 0 V={i(Vx)}\n").contains("undefined element"));
        assert!(err("t\nB1 a 0 Q={1}\n").contains("`V={expr}` or `I={expr}`"));
        assert!(err("t\nB1 a 0 V=TABLE {v(a)} = (0,0) (1,1)\n").contains("TABLE"));
    }

    #[test]
    fn params_and_expressions() {
        let c = load("t\n.param c={a*2} a=1k b={c/4+0.5}\nR1 a 0 {b}\nR2 b 0 c\nR3 d 0 {3/2}\nV1 a 0 {if(a>5,1,0)}\n");
        assert!(matches!(dev(&c, "R1"), Device::Resistor { ohms, .. } if *ohms == 500.5));
        assert!(matches!(dev(&c, "R2"), Device::Resistor { ohms, .. } if *ohms == 2e3));
        assert!(matches!(dev(&c, "R3"), Device::Resistor { ohms, .. } if *ohms == 1.5));
        assert!(matches!(
            dev(&c, "V1"),
            Device::Vsource {
                kind: SourceKind::Dc(1.0),
                ..
            }
        ));
        assert!(err("t\n.param a={b} b={a}\n").contains("dependency cycle"));
        assert!(err("t\n.param a={zz*2}\n").contains("undefined parameter `zz`"));
        assert!(err("t\n.param a\n").contains("expected `name=value`"));
        assert!(err("t\nR1 a 0 {1k*2}\n").contains("engineering suffix"));
        assert!(err("t\nR1 a 0 {v(a)}\n").contains("only valid in a B-source"));
    }

    #[test]
    fn subckts_flatten_with_scoped_names_and_params() {
        let net = "t\n.subckt amp inp out gain=4 r=1k\nRin inp mid {r}\nE1 out 0 mid 0 {gain}\n\
                   Vs mid 0 0\nF1 0 out Vs 1\nB1 z 0 V={v(inp)+i(Vs)}\n.param k={gain*2}\nRk z 0 {k}\n.ends\n\
                   .subckt outer a b\nX1 a b amp gain=10\n.ends\n\
                   V1 in 0 1\nX1 in o1 amp\nX2 in o2 amp gain=2\nX3 in o3 outer\nF9 x 0 X1.Vs 1\n";
        let c = load(net);
        assert!(matches!(dev(&c, "X1.Rin"), Device::Resistor { ohms, .. } if *ohms == 1e3));
        assert!(matches!(dev(&c, "X1.E1"), Device::Vcvs { gain: 4.0, .. }));
        assert!(matches!(dev(&c, "X2.E1"), Device::Vcvs { gain: 2.0, .. }));
        assert!(matches!(
            dev(&c, "X3.X1.E1"),
            Device::Vcvs { gain: 10.0, .. }
        ));
        assert!(matches!(dev(&c, "X2.Rk"), Device::Resistor { ohms, .. } if *ohms == 4.0));
        let Device::Vcvs { p, cp, .. } = dev(&c, "X1.E1") else {
            panic!()
        };
        assert!(c.node_name(*p) == "o1" && c.node_name(*cp) == "X1.mid");
        let vs1 = c.iter().find(|(_, d)| d.name() == "X1.Vs").unwrap().0;
        assert!(matches!(dev(&c, "X1.F1"), Device::Cccs { ctrl_src, .. } if *ctrl_src == vs1));
        assert!(matches!(dev(&c, "F9"), Device::Cccs { ctrl_src, .. } if *ctrl_src == vs1));
        let Device::Behavioral { deps, .. } = dev(&c, "X2.B1") else {
            panic!()
        };
        assert!(matches!(deps[0], BDep::Volt(n) if c.node_name(n) == "in"));
        assert!(matches!(deps[1], BDep::Branch(id) if c.devices[id.0 as usize].name() == "X2.Vs"));
        assert!(err("t\nX1 a b nope\n").contains("undefined subckt `nope`"));
        assert!(err("t\n.subckt s a b\nR1 a b 1\n.ends\nX1 a s\n")
            .contains("connects 1 nodes but subckt `s` has 2 ports"));
        assert!(err("t\n.subckt s a\nR1 a 0 1\n").contains("never closed"));
        assert!(err("t\n.subckt s a\nX1 a s\n.ends\nX1 q s\n").contains("instantiates itself"));
        assert!(err("t\n.subckt s a\n.tran 1 2\n.ends\n").contains("not allowed inside"));
        assert!(err("t\n.subckt s a\nR1 a 0 1x\n.ends\nX1 q s\n").contains("in subckt s as `X1`"));
        assert!(err("t\n.subckt s a a\n.ends\n").contains("listed twice"));
    }

    #[test]
    fn include_resolves_relative_to_the_deck() {
        let dir = std::env::temp_dir().join(format!("hauksbee-spice-inc-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("lib")).unwrap();
        std::fs::write(
            dir.join("lib/d.lib"),
            "* lib\n.model DM D(IS=1e-12\n+ N=1.2)\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("main.cir"),
            "main\n.include \"lib/d.lib\"\nD1 a 0 DM\nR1 a 0 1k\n.end\n",
        )
        .unwrap();
        let c = SpiceLoader::load_file(dir.join("main.cir")).unwrap_or_else(|e| panic!("{e}"));
        assert!(matches!(dev(&c, "D1"), Device::Diode { model, .. } if model.n == 1.2));
        std::fs::write(
            dir.join("bad.cir"),
            "bad\n.include lib/d.lib\nD1 a 0 DM extra\n",
        )
        .unwrap();
        let e = SpiceLoader::load_file(dir.join("bad.cir"))
            .unwrap_err()
            .to_string();
        assert!(e.starts_with("line 3: unexpected trailing token"), "{e}");
        std::fs::write(dir.join("lib/bad.lib"), "R1 a 0 1x\n").unwrap();
        std::fs::write(dir.join("bad2.cir"), "bad\n.include lib/bad.lib\n").unwrap();
        let e = SpiceLoader::load_file(dir.join("bad2.cir"))
            .unwrap_err()
            .to_string();
        assert!(
            e.starts_with("line 1: lib/bad.lib: malformed number"),
            "{e}"
        );
        assert!(err("t\n.include /nonexistent/x.lib\n").contains("cannot read included file"));
        assert!(err("t\n.lib x.lib sec\n").contains("cannot read included file"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn analysis_directives() {
        let net = "t\nV1 in 0 AC 1\nVg g 0 0\nR1 in out 1k\nC1 out 0 1u IC=1\n.temp 85\n.options reltol=1e-4 abstol=1p nopage\n\
                   .tran 1u 2m 0 0.5u uic\n.ac dec 10 1 1meg\n.dc V1 0 5 0.5 Vg 0 1 1\n.print tran v(out) V(in,out)\n\
                   .plot ac v(out)\n.ic V(out)=2\n.nodeset v(in)={1+1}\n.op\n.end\n";
        let (c, d) = SpiceLoader::load_with_directives(net).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(c.temp_c, 85.0);
        assert!(d.reltol == Some(1e-4) && close(d.abstol.unwrap(), 1e-12) && d.vntol.is_none());
        let t = d.tran.unwrap();
        assert!(t.tstep == 1e-6 && t.tstop == 2e-3 && t.tstart == 0.0 && t.tmax == Some(5e-7));
        assert!(d.use_initial_conditions);
        let ac = d.ac.unwrap();
        assert!(ac.sweep == AcSweep::Decade && ac.points == 10 && ac.fstop == 1e6);
        let dc = d.dc.unwrap();
        assert!(
            dc.inner.name == "V1"
                && dc.inner.step == 0.5
                && dc.outer.as_ref().unwrap().name == "Vg"
        );
        assert_eq!(d.prints.len(), 2);
        assert_eq!(d.prints[0].vars, vec!["v(out)", "V(in,out)"]);
        assert!(d.prints[1].is_plot && d.saw_plot);
        assert_eq!(
            c.initial_conditions,
            vec![(c.find_node("out").unwrap(), 2.0)]
        );
        assert_eq!(c.nodesets, vec![(c.find_node("in").unwrap(), 2.0)]);
        assert!(err("t\nR1 a 0 1\n.ic V(a)=1\n.tran 1 2\n").contains("requires `uic`"));
        assert!(err("t\nR1 a 0 1\n.ic V(zz)=1\n").contains("unknown node `zz`"));
        assert!(err("t\n.tran 1\n").contains("tstep and tstop"));
        assert!(err("t\n.ac log 10 1 1k\n").contains("unknown `.ac` sweep type"));
        assert!(err("t\n.ac dec 10 1k 1\n").contains("0 < fstart < fstop"));
        assert!(err("t\nV1 a 0 1\n.dc V1 0 5 -1\n").contains("cannot reach stop"));
        assert!(err("t\nR1 a 0 1\n.dc R1 0 5 1\n").contains("independent V or I source"));
        assert!(err("t\nV1 a 0 1\n.dc Vz 0 5 1\n").contains("does not exist"));
        assert!(err("t\n.print foo v(a)\n").contains("analysis type"));
        assert!(err("t\n.four 1k v(out)\n").contains("unsupported directive `.four`"));
        assert!(err("t\n.control\nrun\n.endc\n").contains("unrecognized directive `.control`"));
        assert!(err("t\n.ends\n").contains("without a matching"));
    }
}
