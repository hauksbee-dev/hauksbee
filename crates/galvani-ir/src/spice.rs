//! A pragmatic SPICE netlist loader.
//!
//! Parses a useful subset of `.cir` files into a [`Circuit`]: element lines for
//! R/C/L/V/I/D/Q/M/S, `.model` cards for diodes, BJTs, and MOSFETs, the `sin`,
//! `pulse`, and `pwl` source functions, `.tran`, `.temp`, and `.options`. The
//! goal is to ingest real test vectors and user-supplied netlists, not to be a
//! complete SPICE3 front end; anything unsupported is reported with the line.
//!
//! Conventions: the first line is a title (ignored), `*` begins a comment,
//! `+` continues the previous line, and node `0`/`gnd` is ground. SI suffixes
//! (`k`, `meg`, `u`, `n`, `p`, `f`, `m`, `g`, `t`, `mil`) are understood.

use crate::models::{BjtModel, DiodeModel, MosLevel, MosfetModel, Polarity};
use crate::source::{PwlPoint, SourceKind};
use crate::{Circuit, Device};
use std::collections::HashMap;
use thiserror::Error;

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
    Syntax { line: usize, msg: String, text: String },
    #[error("line {line}: unknown element type `{ch}`: `{text}`")]
    UnknownElement { line: usize, ch: char, text: String },
    #[error("line {line}: references undefined .model `{model}`: `{text}`")]
    MissingModel { line: usize, model: String, text: String },
    #[error("line {line}: malformed number `{tok}`: `{text}`")]
    BadNumber { line: usize, tok: String, text: String },
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

        // First pass: collect .model cards so element lines can resolve them
        // regardless of order.
        let mut models: HashMap<String, ModelCard> = HashMap::new();
        for (lineno, raw) in &logical {
            let lower = raw.to_ascii_lowercase();
            if lower.starts_with(".model") {
                let card = parse_model_card(*lineno, raw)?;
                models.insert(card.name.to_ascii_lowercase(), card);
            } else if lower.starts_with(".temp") {
                let toks = tokenize(raw);
                if let Some(t) = toks.get(1) {
                    circuit.temp_c = number(*lineno, t, raw)?;
                }
            } else if lower.starts_with(".options") || lower.starts_with(".option") {
                parse_options(raw, &mut directives);
            } else if lower.starts_with(".tran") {
                directives.tran = Some(parse_tran(*lineno, raw, &mut directives)?);
            }
        }

        // Second pass: element lines.
        for (lineno, raw) in &logical {
            let trimmed = raw.trim_start();
            if trimmed.is_empty() || trimmed.starts_with('*') || trimmed.starts_with('.') {
                continue;
            }
            parse_element(*lineno, raw, &mut circuit, &models)?;
        }

        Ok((circuit, directives))
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

/// Split a line on whitespace, treating `=`, `(`, `)`, and `,` as separators
/// that vanish so `pulse(0 5 ...)` and `tc1=0.01` tokenize cleanly.
fn tokenize(line: &str) -> Vec<String> {
    line.split(|c: char| c.is_whitespace() || c == '(' || c == ')' || c == ',' || c == '=')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

/// Tokenize while keeping `key=value` pairs intact (for `.model` parameters).
fn tokenize_kv(line: &str) -> Vec<String> {
    line.split(|c: char| c.is_whitespace() || c == '(' || c == ')' || c == ',')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
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
) -> Result<(), SpiceError> {
    let toks = tokenize(raw);
    if toks.is_empty() {
        return Ok(());
    }
    let name = toks[0].clone();
    let kind = name.chars().next().unwrap().to_ascii_uppercase();

    match kind {
        'R' => parse_rcl(line, raw, &toks, circuit, RclKind::R),
        'C' => parse_rcl(line, raw, &toks, circuit, RclKind::C),
        'L' => parse_rcl(line, raw, &toks, circuit, RclKind::L),
        'V' => parse_source(line, raw, &toks, circuit, true),
        'I' => parse_source(line, raw, &toks, circuit, false),
        'D' => parse_diode(line, raw, &toks, circuit, models),
        'Q' => parse_bjt(line, raw, &toks, circuit, models),
        'M' => parse_mosfet(line, raw, &toks, circuit, models),
        'S' => parse_switch(line, raw, &toks, circuit, models),
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
    let value = number(line, &toks[3], raw)?;
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
    let kind = parse_source_kind(line, raw, &toks[3..])?;

    let device = if is_voltage {
        Device::Vsource { name, p, n, kind }
    } else {
        Device::Isource { name, p, n, kind }
    };
    circuit.add(device);
    Ok(())
}

fn parse_source_kind(line: usize, raw: &str, rest: &[String]) -> Result<SourceKind, SpiceError> {
    if rest.is_empty() {
        return Ok(SourceKind::Dc(0.0));
    }
    let head = rest[0].to_ascii_lowercase();
    // `Vx n+ n- DC 5`, `Vx n+ n- 5`, or a function.
    match head.as_str() {
        "dc" => {
            let v = rest.get(1).map(|t| number(line, t, raw)).transpose()?.unwrap_or(0.0);
            Ok(SourceKind::Dc(v))
        }
        "sin" | "sine" => {
            let nums = number_args(line, raw, &rest[1..])?;
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
            let nums = number_args(line, raw, &rest[1..])?;
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
            let nums = number_args(line, raw, &rest[1..])?;
            let mut points = Vec::new();
            for pair in nums.chunks(2) {
                if pair.len() == 2 {
                    points.push(PwlPoint { t: pair[0], v: pair[1] });
                }
            }
            Ok(SourceKind::Pwl(points))
        }
        _ => {
            // Bare numeric value: `Vx a b 5`.
            let v = number(line, &rest[0], raw)?;
            Ok(SourceKind::Dc(v))
        }
    }
}

/// Convert tokens to numbers, skipping trailing AC/transient spec keywords.
fn number_args(line: usize, raw: &str, toks: &[String]) -> Result<Vec<f64>, SpiceError> {
    let mut out = Vec::new();
    for t in toks {
        // Stop at non-numeric keywords (e.g. a following `ac`).
        match parse_spice_number(t) {
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
    let l = kv.get("l").copied().or_else(|| card.get("l")).unwrap_or(1.0);
    let w = kv.get("w").copied().or_else(|| card.get("w")).unwrap_or(1.0);
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

fn parse_tran(line: usize, raw: &str, directives: &mut Directives) -> Result<TranDirective, SpiceError> {
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
    fn pulse_source_roundtrip() {
        let net = "p\nV1 a 0 pulse(0 5 1m 1u 1u 2m 5m)\n.end\n";
        let c = SpiceLoader::load(net).unwrap();
        match &c.devices[0] {
            Device::Vsource { kind: SourceKind::Pulse { v2, period, .. }, .. } => {
                assert_eq!(*v2, 5.0);
                assert!((*period - 5e-3).abs() < 1e-12);
            }
            _ => panic!("expected pulse vsource"),
        }
    }
}
