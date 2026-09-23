//! Dimensional sanity for free-form `evalexpr` expressions (behavioral laws and
//! FSM guards).
//!
//! An expression can parse, evaluate and pass every structural gate while still
//! computing the wrong quantity: amps divided by a pin voltage instead of by a
//! resistance. This pass gives each identifier the dimension its name already
//! declares under the conventions the runtime and the cards use, propagates it
//! through the arithmetic, and reports a sum, comparison or declared result that
//! mixes dimensions.
//!
//! Names: `v_<role>` is a pin voltage and `i_<name>` a path or load current (both
//! bound by the behavioral runtime); `t`, `time` and `t_in_state` are seconds;
//! `state_<name>` is a 0/1 selector. Params carry their unit as a suffix
//! (`_v`, `_a`/`_ma`/`_ua`, `_ohm`/`_ohms`, `_s`, `_f`/`_pf`, `_hz`, `_w`); a
//! `_per_` name is a ratio this table does not model. Anything else is unknown.
//!
//! What it cannot judge, and therefore never rejects: an identifier with no
//! convention (its dimension is unknown, and so is every product it enters);
//! the argument of a transcendental function (a thermal voltage hides in the
//! literal of `exp(v / 0.025)`); an expression the small parser below does not
//! understand. A numeric literal is a pure number: dimensionless as a factor,
//! so it cannot carry a hidden unit (`540 / v_x` is 1/V, never a current), but
//! free in a sum or comparison (`v_x - 0.7`), and a literal zero is free
//! everywhere. A constant multiplier's magnitude is never checked.

/// Exponents of volts, amps and seconds; every quantity the cards name is a
/// product of these (ohm = V/A, watt = V*A, farad = A*s/V, hertz = 1/s).
type Exponents = [i8; 3];

const VOLT: Exponents = [1, 0, 0];
const AMP: Exponents = [0, 1, 0];
const SECOND: Exponents = [0, 0, 1];
const OHM: Exponents = [1, -1, 0];
const NONE: Exponents = [0, 0, 0];

/// What the checker knows about a sub-expression.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dim {
    Known(Exponents),
    /// A pure number: dimensionless as a factor, adopts the other side in a sum.
    Number,
    /// The literal zero, which has every dimension.
    Zero,
    Unknown,
}

/// The unit a name declares, per the module-level conventions.
fn name_dim(name: &str) -> Dim {
    if name.starts_with("v_") {
        return Dim::Known(VOLT);
    }
    if name.starts_with("i_") {
        return Dim::Known(AMP);
    }
    if name.starts_with("state_") {
        return Dim::Number;
    }
    if matches!(name, "t" | "time" | "t_in_state") {
        return Dim::Known(SECOND);
    }
    if name.contains("_per_") {
        return Dim::Unknown;
    }
    const SUFFIXES: &[(&str, Exponents)] = &[
        ("_v", VOLT),
        ("_a", AMP),
        ("_ma", AMP),
        ("_ua", AMP),
        ("_ohm", OHM),
        ("_ohms", OHM),
        ("_s", SECOND),
        ("_f", [-1, 1, 1]),
        ("_pf", [-1, 1, 1]),
        ("_hz", [0, 0, -1]),
        ("_w", [1, 1, 0]),
    ];
    SUFFIXES
        .iter()
        .find(|(suffix, _)| name.ends_with(suffix))
        .map_or(Dim::Unknown, |&(_, e)| Dim::Known(e))
}

fn unit_name(e: Exponents) -> String {
    const NAMED: &[(Exponents, &str)] = &[
        (VOLT, "V"),
        (AMP, "A"),
        (SECOND, "s"),
        (OHM, "ohm"),
        (NONE, "dimensionless"),
        ([-1, 0, 0], "1/V"),
        ([0, -1, 0], "1/A"),
        ([1, 1, 0], "W"),
        ([-1, 1, 0], "1/ohm"),
        ([0, 0, -1], "1/s"),
    ];
    if let Some((_, name)) = NAMED.iter().find(|(x, _)| *x == e) {
        return (*name).to_string();
    }
    let parts: Vec<String> = ["V", "A", "s"]
        .iter()
        .zip(e)
        .filter(|(_, p)| *p != 0)
        .map(|(u, p)| format!("{u}^{p}"))
        .collect();
    parts.join("*")
}

/// The unit a behavioral law's result must carry.
pub(crate) const CURRENT: (Exponents, &str) = (AMP, "current law");
pub(crate) const VOLTAGE: (Exponents, &str) = (VOLT, "voltage law");

/// Check `expr` for internal dimensional consistency and, when `expected` is
/// given, that its result has that dimension. `Err` names the first mismatch;
/// anything the checker cannot judge is `Ok`.
pub(crate) fn check_dimensions(
    expr: &str,
    expected: Option<(Exponents, &str)>,
) -> Result<(), String> {
    let Some(tokens) = tokenize(expr) else {
        return Ok(());
    };
    let mut p = Parser {
        tokens,
        pos: 0,
        error: None,
    };
    let Some(result) = p.binary(0).filter(|_| p.pos == p.tokens.len()) else {
        return Ok(());
    };
    if let Some(error) = p.error {
        return Err(format!("`{expr}` {error}"));
    }
    match (result, expected) {
        (Dim::Known(got), Some((want, what))) if got != want => Err(format!(
            "`{expr}` has dimension {} but a {what} must produce {}; a numeric literal is \
             dimensionless, so carry a unit-bearing constant as a named param",
            unit_name(got),
            unit_name(want)
        )),
        _ => Ok(()),
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Num(f64),
    Ident(String),
    Op(&'static str),
}

/// Split into numbers, identifiers (`math::exp` included) and operators, or
/// `None` for anything outside the arithmetic subset (strings, assignment,
/// chaining), which then goes unjudged.
fn tokenize(expr: &str) -> Option<Vec<Tok>> {
    const OPS: &[&str] = &[
        "&&", "||", "==", "!=", "<=", ">=", "<", ">", "+", "-", "*", "/", "%", "^", "!", "(", ")",
        ",",
    ];
    let mut out = Vec::new();
    let mut rest = expr.trim_start();
    while !rest.is_empty() {
        let c = rest.chars().next()?;
        let len = if c.is_ascii_digit() || c == '.' {
            let mut end = rest
                .find(|ch: char| !(ch.is_ascii_digit() || ch == '.'))
                .unwrap_or(rest.len());
            if rest[end..].starts_with(['e', 'E']) {
                let exp = &rest[end + 1..];
                let sign = usize::from(exp.starts_with(['+', '-']));
                let digits = exp[sign..]
                    .find(|ch: char| !ch.is_ascii_digit())
                    .unwrap_or(exp.len() - sign);
                end += 1 + sign + digits;
            }
            out.push(Tok::Num(rest[..end].parse().ok()?));
            end
        } else if c.is_alphabetic() || c == '_' {
            let end = rest
                .find(|ch: char| !(ch.is_alphanumeric() || ch == '_' || ch == ':'))
                .unwrap_or(rest.len());
            out.push(Tok::Ident(rest[..end].to_string()));
            end
        } else {
            let op = OPS.iter().find(|op| rest.starts_with(**op))?;
            out.push(Tok::Op(op));
            op.len()
        };
        rest = rest[len..].trim_start();
    }
    Some(out)
}

struct Parser {
    tokens: Vec<Tok>,
    pos: usize,
    error: Option<String>,
}

/// Binary operators, loosest first.
const LEVELS: &[&[&str]] = &[
    &["||"],
    &["&&"],
    &["==", "!=", "<=", ">=", "<", ">"],
    &["+", "-"],
    &["*", "/", "%"],
    &["^"],
];

impl Parser {
    fn peek_is(&self, op: &str) -> bool {
        matches!(self.tokens.get(self.pos), Some(Tok::Op(o)) if *o == op)
    }

    fn eat(&mut self, op: &str) -> bool {
        let hit = self.peek_is(op);
        self.pos += usize::from(hit);
        hit
    }

    fn binary(&mut self, level: usize) -> Option<Dim> {
        if level == LEVELS.len() {
            return self.unary();
        }
        let mut lhs = self.binary(level + 1)?;
        while let Some(&op) = LEVELS[level].iter().find(|op| self.peek_is(op)) {
            self.pos += 1;
            let rhs = self.binary(level + 1)?;
            lhs = match op {
                "+" | "-" | "%" => self.unify(op, lhs, rhs),
                "*" | "/" => product(lhs, rhs, op == "/"),
                "^" => self.call(op, &[lhs, rhs]),
                _ if level == 2 => {
                    self.unify(op, lhs, rhs);
                    Dim::Number
                }
                _ => Dim::Number,
            };
        }
        Some(lhs)
    }

    fn unary(&mut self) -> Option<Dim> {
        if self.eat("-") {
            return self.unary();
        }
        if self.eat("!") {
            self.unary()?;
            return Some(Dim::Number);
        }
        self.primary()
    }

    fn primary(&mut self) -> Option<Dim> {
        let tok = self.tokens.get(self.pos)?.clone();
        self.pos += 1;
        match tok {
            Tok::Num(v) => Some(if v == 0.0 { Dim::Zero } else { Dim::Number }),
            Tok::Op("(") => {
                let inner = self.binary(0)?;
                self.eat(")").then_some(inner)
            }
            Tok::Ident(name) if self.eat("(") => {
                let mut args = Vec::new();
                if !self.eat(")") {
                    loop {
                        args.push(self.binary(0)?);
                        if self.eat(")") {
                            break;
                        }
                        if !self.eat(",") {
                            return None;
                        }
                    }
                }
                Some(self.call(&name, &args))
            }
            Tok::Ident(name) if matches!(name.as_str(), "true" | "false") => Some(Dim::Number),
            Tok::Ident(name) => Some(name_dim(&name)),
            Tok::Op(_) => None,
        }
    }

    fn call(&mut self, name: &str, args: &[Dim]) -> Dim {
        let fold = |p: &mut Self, args: &[Dim]| {
            args.iter()
                .copied()
                .reduce(|a, b| p.unify(name, a, b))
                .unwrap_or(Dim::Unknown)
        };
        match name {
            "min" | "max" => fold(self, args),
            "if" if args.len() == 3 => fold(self, &args[1..]),
            "abs" | "round" | "floor" | "ceil" | "math::abs" if args.len() == 1 => args[0],
            // Transcendentals and `^` want a dimensionless argument, but a
            // literal scale (a thermal voltage) can hide a unit, so only a
            // pure-number call is judged.
            _ if args
                .iter()
                .all(|a| matches!(a, Dim::Number | Dim::Zero | Dim::Known(NONE))) =>
            {
                Dim::Number
            }
            _ => Dim::Unknown,
        }
    }

    /// Both sides of a sum, comparison or `min`/`max`/`if` must agree.
    fn unify(&mut self, op: &str, a: Dim, b: Dim) -> Dim {
        match (a, b) {
            (Dim::Known(x), Dim::Known(y)) if x != y => {
                self.error.get_or_insert_with(|| {
                    format!(
                        "combines {} with {} under `{op}`",
                        unit_name(x),
                        unit_name(y)
                    )
                });
                Dim::Unknown
            }
            (Dim::Known(x), _) | (_, Dim::Known(x)) => Dim::Known(x),
            (Dim::Unknown, _) | (_, Dim::Unknown) => Dim::Unknown,
            (Dim::Number, _) | (_, Dim::Number) => Dim::Number,
            (Dim::Zero, Dim::Zero) => Dim::Zero,
        }
    }
}

fn product(a: Dim, b: Dim, divide: bool) -> Dim {
    let sign: i8 = if divide { -1 } else { 1 };
    let as_exponents = |d: Dim| match d {
        Dim::Known(e) => Some(e),
        Dim::Number => Some(NONE),
        _ => None,
    };
    match (a, b) {
        (Dim::Zero, _) | (_, Dim::Zero) => Dim::Zero,
        (Dim::Number, Dim::Number) => Dim::Number,
        _ => match (as_exponents(a), as_exponents(b)) {
            (Some(x), Some(y)) => Dim::Known([0, 1, 2].map(|i| x[i] + sign * y[i])),
            _ => Dim::Unknown,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrong_units_are_named_and_correct_or_unjudgeable_laws_pass() {
        let err = check_dimensions("540.0 / v_iset", Some(CURRENT)).unwrap_err();
        assert!(err.contains("1/V") && err.contains("current law"), "{err}");
        let err = check_dimensions("v_a - i_path", None).unwrap_err();
        assert!(err.contains("combines V with A"), "{err}");
        assert!(check_dimensions("if(v_x > i_load_a, 1.0, 0.0)", None).is_err());
        assert!(check_dimensions("prog_gain_v / sense_ohms", Some(VOLTAGE)).is_err());
        for (expr, want) in [
            ("540.0 / iset_ohms * 1.2", None),
            ("v_vplus / tie_ohms", Some(CURRENT)),
            (
                "max(0.0, v_d1p - vbreak_v) / rdyn_ohms + min(0.0, v_d1p + vf_v) / rdyn_ohms",
                Some(CURRENT),
            ),
            (
                "max(0.0, (v_io - v_k - vt_clamp) / rd_clamp)",
                Some(CURRENT),
            ),
            ("0.0016", Some(CURRENT)),
            ("state_on * 0.05", Some(CURRENT)),
            ("2.0 + 0.0 * t_in_state", Some(VOLTAGE)),
            (
                "if(v_en > 1.2 && t_in_state > 1e-3, v_vin - 0.3, 0.0)",
                Some(VOLTAGE),
            ),
            ("1e-12 * (math::exp(v_a / 0.02585) - 1.0)", Some(CURRENT)),
            ("-v_x * gain", Some(CURRENT)),
            ("theta_ja_c_per_w * 2.0", Some(VOLTAGE)),
            ("x = 3; x", Some(CURRENT)),
        ] {
            assert_eq!(check_dimensions(expr, want), Ok(()), "{expr}");
        }
    }
}
