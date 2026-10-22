//! Peripheral configuration snapshot: what the firmware actually configured.
//!
//! A co-simulation that only knows "the part runs at N MHz" cannot tell a
//! blinky that toggles at 1 Hz on silicon from one that toggles at 10 Hz in the
//! emulator, or a UART at 115200 baud from one that is 3.5 % off. This module
//! decodes the configuration registers the firmware wrote (RCC, USART BRR, SPI
//! CR1, ...) into a typed [`PeripheralConfig`], driven entirely by data in the
//! SoC descriptor (`[[soc.decode]]`). Nothing here knows a register layout.
//!
//! # The decode language
//!
//! A [`DecodeBlock`] names some registers (absolute addresses) and an ordered
//! list of `(name, expression)` variables. An expression is plain arithmetic
//! over the register values, earlier variables and a handful of functions:
//!
//! | form | meaning |
//! |---|---|
//! | `+ - * / %`, `( )`, unary `-` | real arithmetic (`f64`, exact for the integers a 32-bit register holds) |
//! | `== != < <= > >=`, `&&`, `\|\|`, `!` | comparisons and logic, yielding `1` or `0` |
//! | `bits(v, hi, lo)` | the field `v[hi:lo]` as an unsigned integer |
//! | `bit(v, n)` | bit `n` of `v` |
//! | `pick(i, a, b, ...)` | the `i`-th argument (0-based); only the chosen one is evaluated |
//! | `if(c, a, b)` | `a` when `c` is nonzero, else `b`; only the chosen one is evaluated |
//! | `min`, `max`, `floor`, `ceil` | as named |
//!
//! `pick` and `if` are lazy on purpose: a reserved encoding in an unselected
//! branch (an RCC multiplier table with holes) must not poison the whole tree.
//!
//! # Unknown, never guessed
//!
//! A register that could not be read, a reserved encoding (`pick` out of
//! range), a division by zero, or a variable the descriptor does not declare
//! all produce [`Decoded::Unknown`] carrying the reason. Everything downstream
//! abstains on `Unknown`; nothing substitutes a default.
//!
//! # Provenance
//!
//! The decode formulas in the shipped descriptors come from the reference
//! manuals (RM0008, RM0090, RM0091, RM0377, RM0440), each cited next to its
//! block in `db/mcu/*.soc.toml`. This module is the evaluator only.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

// ── Reading registers ────────────────────────────────────────────────

/// Something that can read a 32-bit register at an absolute bus address: a
/// Renode Monitor session, a QEMU gdbstub, simavr's data space, or a test map.
pub trait RegisterReader {
    /// The 32-bit little-endian word at `address`, or `None` when the read
    /// failed or the address is not readable on this backend.
    fn read_u32(&mut self, address: u32) -> Option<u32>;
}

impl<F: FnMut(u32) -> Option<u32>> RegisterReader for F {
    fn read_u32(&mut self, address: u32) -> Option<u32> {
        self(address)
    }
}

/// A fixed register image, for tests and for replaying a recorded run.
impl RegisterReader for BTreeMap<u32, u32> {
    fn read_u32(&mut self, address: u32) -> Option<u32> {
        self.get(&address).copied()
    }
}

// ── Decoded values ───────────────────────────────────────────────────

/// A decoded quantity, or the reason it could not be decoded.
#[derive(Debug, Clone, PartialEq)]
pub enum Decoded<T> {
    /// Decoded from the registers.
    Known(T),
    /// Not decodable; the string says why (which register, which encoding).
    Unknown(String),
}

impl<T: Copy> Decoded<T> {
    /// The value when known.
    #[must_use]
    pub fn known(&self) -> Option<T> {
        match self {
            Self::Known(v) => Some(*v),
            Self::Unknown(_) => None,
        }
    }
}

impl<T> Decoded<T> {
    /// True when the value could not be decoded.
    #[must_use]
    pub const fn is_unknown(&self) -> bool {
        matches!(self, Self::Unknown(_))
    }

    /// The reason a value is unknown.
    #[must_use]
    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Unknown(why) => Some(why),
            Self::Known(_) => None,
        }
    }
}

impl<T: fmt::Display> fmt::Display for Decoded<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Known(v) => v.fmt(f),
            Self::Unknown(why) => write!(f, "unknown ({why})"),
        }
    }
}

// ── The expression language ──────────────────────────────────────────

/// A decode expression that did not parse or evaluate.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ExprError(pub String);

fn err<T>(msg: impl Into<String>) -> Result<T, ExprError> {
    Err(ExprError(msg.into()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
}

#[derive(Debug, Clone, PartialEq)]
enum Node {
    Num(f64),
    Var(String),
    Neg(Box<Node>),
    Not(Box<Node>),
    Bin(Op, Box<Node>, Box<Node>),
    Call(String, Vec<Node>),
}

/// A parsed decode expression.
#[derive(Debug, Clone, PartialEq)]
pub struct Expr {
    root: Node,
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Num(f64),
    Ident(String),
    Sym(&'static str),
}

fn lex(src: &str) -> Result<Vec<Tok>, ExprError> {
    const SYMS: [&str; 17] = [
        "==", "!=", "<=", ">=", "&&", "||", "+", "-", "*", "/", "%", "<", ">", "!", "(", ")", ",",
    ];
    let bytes = src.as_bytes();
    let mut i = 0;
    let mut out = Vec::new();
    while i < bytes.len() {
        let c = bytes[i] as char;
        if c.is_whitespace() {
            i += 1;
        } else if c.is_ascii_digit() || (c == '.' && bytes.get(i + 1).is_some_and(u8::is_ascii_digit)) {
            let start = i;
            if c == '0' && matches!(bytes.get(i + 1), Some(b'x' | b'X')) {
                i += 2;
                while i < bytes.len() && ((bytes[i] as char).is_ascii_hexdigit() || bytes[i] == b'_') {
                    i += 1;
                }
                let digits: String = src[start + 2..i].chars().filter(|c| *c != '_').collect();
                let v = u64::from_str_radix(&digits, 16)
                    .map_err(|_| ExprError(format!("bad hex literal {:?}", &src[start..i])))?;
                #[allow(clippy::cast_precision_loss)] // register-sized literals are exact in f64
                out.push(Tok::Num(v as f64));
            } else {
                while i < bytes.len()
                    && ((bytes[i] as char).is_ascii_digit() || matches!(bytes[i], b'.' | b'_'))
                {
                    i += 1;
                }
                // An exponent only when followed by digits, so `1e6` lexes but an
                // identifier starting with `e` after a number does not.
                if matches!(bytes.get(i), Some(b'e' | b'E')) {
                    let mut j = i + 1;
                    if matches!(bytes.get(j), Some(b'+' | b'-')) {
                        j += 1;
                    }
                    if bytes.get(j).is_some_and(u8::is_ascii_digit) {
                        while bytes.get(j).is_some_and(u8::is_ascii_digit) {
                            j += 1;
                        }
                        i = j;
                    }
                }
                let text: String = src[start..i].chars().filter(|c| *c != '_').collect();
                let v: f64 = text
                    .parse()
                    .map_err(|_| ExprError(format!("bad number {:?}", &src[start..i])))?;
                out.push(Tok::Num(v));
            }
        } else if c.is_ascii_alphabetic() || c == '_' {
            let start = i;
            while i < bytes.len() && ((bytes[i] as char).is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            out.push(Tok::Ident(src[start..i].to_string()));
        } else {
            let rest = &src[i..];
            let Some(sym) = SYMS.iter().find(|s| rest.starts_with(**s)) else {
                return err(format!("unexpected character {c:?}"));
            };
            out.push(Tok::Sym(sym));
            i += sym.len();
        }
    }
    Ok(out)
}

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
}

impl Parser {
    fn peek_sym(&self) -> Option<&'static str> {
        match self.toks.get(self.pos) {
            Some(Tok::Sym(s)) => Some(s),
            _ => None,
        }
    }

    fn eat(&mut self, sym: &str) -> bool {
        if self.peek_sym() == Some(sym) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn binary_level(
        &mut self,
        ops: &[(&str, Op)],
        next: fn(&mut Self) -> Result<Node, ExprError>,
    ) -> Result<Node, ExprError> {
        let mut lhs = next(self)?;
        'outer: loop {
            for (sym, op) in ops {
                if self.eat(sym) {
                    let rhs = next(self)?;
                    lhs = Node::Bin(*op, Box::new(lhs), Box::new(rhs));
                    continue 'outer;
                }
            }
            return Ok(lhs);
        }
    }

    fn or(&mut self) -> Result<Node, ExprError> {
        self.binary_level(&[("||", Op::Or)], Self::and)
    }

    fn and(&mut self) -> Result<Node, ExprError> {
        self.binary_level(&[("&&", Op::And)], Self::cmp)
    }

    fn cmp(&mut self) -> Result<Node, ExprError> {
        self.binary_level(
            &[
                ("==", Op::Eq),
                ("!=", Op::Ne),
                ("<=", Op::Le),
                (">=", Op::Ge),
                ("<", Op::Lt),
                (">", Op::Gt),
            ],
            Self::sum,
        )
    }

    fn sum(&mut self) -> Result<Node, ExprError> {
        self.binary_level(&[("+", Op::Add), ("-", Op::Sub)], Self::product)
    }

    fn product(&mut self) -> Result<Node, ExprError> {
        self.binary_level(
            &[("*", Op::Mul), ("/", Op::Div), ("%", Op::Rem)],
            Self::unary,
        )
    }

    fn unary(&mut self) -> Result<Node, ExprError> {
        if self.eat("-") {
            return Ok(Node::Neg(Box::new(self.unary()?)));
        }
        if self.eat("!") {
            return Ok(Node::Not(Box::new(self.unary()?)));
        }
        self.primary()
    }

    fn primary(&mut self) -> Result<Node, ExprError> {
        match self.toks.get(self.pos).cloned() {
            Some(Tok::Num(v)) => {
                self.pos += 1;
                Ok(Node::Num(v))
            }
            Some(Tok::Ident(name)) => {
                self.pos += 1;
                if self.eat("(") {
                    let mut args = Vec::new();
                    if !self.eat(")") {
                        loop {
                            args.push(self.or()?);
                            if self.eat(")") {
                                break;
                            }
                            if !self.eat(",") {
                                return err(format!("expected `,` or `)` in call to {name}"));
                            }
                        }
                    }
                    check_arity(&name, args.len())?;
                    Ok(Node::Call(name, args))
                } else {
                    Ok(Node::Var(name))
                }
            }
            Some(Tok::Sym("(")) => {
                self.pos += 1;
                let inner = self.or()?;
                if !self.eat(")") {
                    return err("missing `)`");
                }
                Ok(inner)
            }
            Some(Tok::Sym(s)) => err(format!("unexpected `{s}`")),
            None => err("expression ends early"),
        }
    }
}

fn check_arity(name: &str, n: usize) -> Result<(), ExprError> {
    let (ok, want) = match name {
        "bits" | "if" => (n == 3, "3"),
        "bit" | "min" | "max" => (n == 2, "2"),
        "floor" | "ceil" => (n == 1, "1"),
        "pick" => (n >= 2, "at least 2"),
        _ => return err(format!("unknown function `{name}`")),
    };
    if ok {
        Ok(())
    } else {
        err(format!("`{name}` takes {want} argument(s), got {n}"))
    }
}

impl Expr {
    /// Parse `src`.
    ///
    /// # Errors
    ///
    /// A syntax error, an unknown function, or a wrong argument count.
    pub fn parse(src: &str) -> Result<Self, ExprError> {
        let toks = lex(src)?;
        if toks.is_empty() {
            return err("empty expression");
        }
        let mut p = Parser { toks, pos: 0 };
        let root = p.or()?;
        if p.pos != p.toks.len() {
            return err(format!("unexpected trailing input at token {}", p.pos));
        }
        Ok(Self { root })
    }

    /// Every variable name the expression mentions, in first-seen order.
    #[must_use]
    pub fn free_vars(&self) -> Vec<String> {
        fn walk(n: &Node, out: &mut Vec<String>) {
            match n {
                Node::Num(_) => {}
                Node::Var(v) => {
                    if !out.contains(v) {
                        out.push(v.clone());
                    }
                }
                Node::Neg(a) | Node::Not(a) => walk(a, out),
                Node::Bin(_, a, b) => {
                    walk(a, out);
                    walk(b, out);
                }
                Node::Call(_, args) => args.iter().for_each(|a| walk(a, out)),
            }
        }
        let mut out = Vec::new();
        walk(&self.root, &mut out);
        out
    }

    /// Evaluate with `lookup` resolving variable names.
    ///
    /// # Errors
    ///
    /// An unresolvable variable (the lookup's message), a division by zero, a
    /// `pick` index outside its choices, or a non-integer given to a bit
    /// function.
    pub fn eval(&self, lookup: &dyn Fn(&str) -> Result<f64, ExprError>) -> Result<f64, ExprError> {
        eval_node(&self.root, lookup)
    }
}

/// A value that must be a non-negative integer that fits a 64-bit register
/// image, for the bit functions.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // range-checked just above
#[allow(clippy::float_cmp)] // exact: the fraction of an integer-valued float is exactly 0.0
fn as_word(v: f64, what: &str) -> Result<u64, ExprError> {
    if v.is_finite() && v >= 0.0 && v.fract() == 0.0 && v < 18_446_744_073_709_551_616.0 {
        Ok(v as u64)
    } else {
        err(format!("{what} is {v}, not a non-negative integer"))
    }
}

#[allow(clippy::cast_precision_loss)] // bit fields are at most 32 bits wide, exact in f64
#[allow(clippy::float_cmp)] // truth tests compare against the exact 0.0/1.0 this evaluator produces
fn eval_node(
    n: &Node,
    lookup: &dyn Fn(&str) -> Result<f64, ExprError>,
) -> Result<f64, ExprError> {
    let truth = |b: bool| if b { 1.0 } else { 0.0 };
    match n {
        Node::Num(v) => Ok(*v),
        Node::Var(name) => lookup(name),
        Node::Neg(a) => Ok(-eval_node(a, lookup)?),
        Node::Not(a) => Ok(truth(eval_node(a, lookup)? == 0.0)),
        Node::Bin(op, a, b) => {
            let x = eval_node(a, lookup)?;
            // `&&` and `||` short-circuit so an unavailable operand on the
            // untaken side does not poison the result.
            match op {
                Op::And if x == 0.0 => return Ok(0.0),
                Op::Or if x != 0.0 => return Ok(1.0),
                _ => {}
            }
            let y = eval_node(b, lookup)?;
            Ok(match op {
                Op::Add => x + y,
                Op::Sub => x - y,
                Op::Mul => x * y,
                Op::Div | Op::Rem if y == 0.0 => return err("division by zero"),
                Op::Div => x / y,
                Op::Rem => x % y,
                Op::Eq => truth(x == y),
                Op::Ne => truth(x != y),
                Op::Lt => truth(x < y),
                Op::Le => truth(x <= y),
                Op::Gt => truth(x > y),
                Op::Ge => truth(x >= y),
                Op::And | Op::Or => truth(y != 0.0),
            })
        }
        Node::Call(name, args) => match name.as_str() {
            "bits" => {
                let v = as_word(eval_node(&args[0], lookup)?, "bits() operand")?;
                let hi = as_word(eval_node(&args[1], lookup)?, "bits() high bit")?;
                let lo = as_word(eval_node(&args[2], lookup)?, "bits() low bit")?;
                if hi < lo || hi > 63 {
                    return err(format!("bits({hi}, {lo}) is not a field of a 64-bit word"));
                }
                let width = hi - lo + 1;
                let mask = if width == 64 { u64::MAX } else { (1u64 << width) - 1 };
                Ok(((v >> lo) & mask) as f64)
            }
            "bit" => {
                let v = as_word(eval_node(&args[0], lookup)?, "bit() operand")?;
                let b = as_word(eval_node(&args[1], lookup)?, "bit() index")?;
                if b > 63 {
                    return err(format!("bit({b}) is outside a 64-bit word"));
                }
                Ok(((v >> b) & 1) as f64)
            }
            "pick" => {
                let i = as_word(eval_node(&args[0], lookup)?, "pick() index")?;
                let choices = &args[1..];
                match usize::try_from(i).ok().and_then(|i| choices.get(i)) {
                    Some(choice) => eval_node(choice, lookup),
                    None => err(format!(
                        "encoding {i} is reserved or undecoded (table has {} entries)",
                        choices.len()
                    )),
                }
            }
            "if" => {
                if eval_node(&args[0], lookup)? == 0.0 {
                    eval_node(&args[2], lookup)
                } else {
                    eval_node(&args[1], lookup)
                }
            }
            "min" => Ok(eval_node(&args[0], lookup)?.min(eval_node(&args[1], lookup)?)),
            "max" => Ok(eval_node(&args[0], lookup)?.max(eval_node(&args[1], lookup)?)),
            "floor" => Ok(eval_node(&args[0], lookup)?.floor()),
            "ceil" => Ok(eval_node(&args[0], lookup)?.ceil()),
            other => err(format!("unknown function `{other}`")),
        },
    }
}

// ── Descriptor schema ────────────────────────────────────────────────

/// What a decode block describes. The kind fixes which variable names the typed
/// views ([`ClockConfig`], [`UartConfig`], ...) read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockKind {
    /// A clock tree. Variables of the FIRST clock block are visible to every
    /// other block as `clk_<name>`.
    Clock,
    /// A UART: `baud`, `data_bits`, `stop_bits`.
    Uart,
    /// An SPI controller: `sck_hz`, `cpol`, `cpha`.
    Spi,
    /// An I2C controller: `scl_hz`.
    I2c,
    /// A timer: `tick_hz`, `period_s`.
    Timer,
    /// An ADC: `clock_hz`, `resolution_bits`, `sample_cycles`.
    Adc,
}

/// One `[[soc.decode]]` entry: registers to read and the variables to derive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecodeBlock {
    /// What the block describes.
    pub kind: BlockKind,
    /// The peripheral's name (`rcc`, `usart1`, `tim2`), unique per kind.
    pub name: String,
    /// Register name to absolute bus address. Each register is read once per
    /// snapshot and is a variable holding its 32-bit value.
    #[serde(default)]
    pub registers: BTreeMap<String, u32>,
    /// Ordered `[name, expression]` pairs. A variable may use registers and
    /// earlier variables only.
    #[serde(default)]
    pub vars: Vec<(String, String)>,
}

/// Why a [`DecodeBlock`] was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    /// A name that cannot be an identifier.
    #[error("decode block {block:?}: {name:?} is not a valid identifier")]
    BadName {
        /// The block.
        block: String,
        /// The offending name.
        name: String,
    },
    /// Two registers or variables, or a variable and a register, share a name.
    #[error("decode block {block:?}: {name:?} is declared twice")]
    Duplicate {
        /// The block.
        block: String,
        /// The repeated name.
        name: String,
    },
    /// An expression that does not parse.
    #[error("decode block {block:?}, variable {var:?}: {error}")]
    BadExpr {
        /// The block.
        block: String,
        /// The variable.
        var: String,
        /// The parse error.
        error: ExprError,
    },
    /// An expression naming something that is not a register or earlier variable.
    #[error("decode block {block:?}, variable {var:?} uses {name:?}, which is neither a register nor an earlier variable")]
    UnknownName {
        /// The block.
        block: String,
        /// The variable.
        var: String,
        /// The unresolved name.
        name: String,
    },
    /// Two blocks of the same kind and name.
    #[error("decode: two {kind:?} blocks are named {name:?}")]
    DuplicateBlock {
        /// The kind.
        kind: BlockKind,
        /// The name.
        name: String,
    },
}

fn is_ident(s: &str) -> bool {
    let mut chars = s.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The `clk_` prefix other blocks use to read the clock tree's variables.
pub const CLOCK_PREFIX: &str = "clk_";

/// Validate a descriptor's decode blocks: identifiers, uniqueness, parseable
/// expressions, and that every name an expression uses resolves.
///
/// # Errors
///
/// The first problem found, as a [`DecodeError`].
pub fn validate_blocks(blocks: &[DecodeBlock]) -> Result<(), DecodeError> {
    let first_clock = blocks.iter().find(|b| b.kind == BlockKind::Clock);
    let clock_vars: Vec<String> = first_clock
        .map(|c| c.vars.iter().map(|(n, _)| format!("{CLOCK_PREFIX}{n}")).collect())
        .unwrap_or_default();
    for (i, block) in blocks.iter().enumerate() {
        if blocks[..i]
            .iter()
            .any(|b| b.kind == block.kind && b.name == block.name)
        {
            return Err(DecodeError::DuplicateBlock {
                kind: block.kind,
                name: block.name.clone(),
            });
        }
        let mut known: Vec<&str> = Vec::new();
        for reg in block.registers.keys() {
            if !is_ident(reg) {
                return Err(DecodeError::BadName {
                    block: block.name.clone(),
                    name: reg.clone(),
                });
            }
            known.push(reg);
        }
        let outer: &[String] = if block.kind == BlockKind::Clock {
            &[]
        } else {
            &clock_vars
        };
        for (name, src) in &block.vars {
            if !is_ident(name) {
                return Err(DecodeError::BadName {
                    block: block.name.clone(),
                    name: name.clone(),
                });
            }
            if known.contains(&name.as_str()) {
                return Err(DecodeError::Duplicate {
                    block: block.name.clone(),
                    name: name.clone(),
                });
            }
            let expr = Expr::parse(src).map_err(|error| DecodeError::BadExpr {
                block: block.name.clone(),
                var: name.clone(),
                error,
            })?;
            for used in expr.free_vars() {
                if !known.contains(&used.as_str()) && !outer.contains(&used) {
                    return Err(DecodeError::UnknownName {
                        block: block.name.clone(),
                        var: name.clone(),
                        name: used,
                    });
                }
            }
            known.push(name);
        }
    }
    Ok(())
}

// ── Decoding ─────────────────────────────────────────────────────────

/// One decoded block: every declared variable, known or not.
#[derive(Debug, Clone, PartialEq)]
pub struct BlockSnapshot {
    /// What the block describes.
    pub kind: BlockKind,
    /// The peripheral's name.
    pub name: String,
    /// Variable values in declaration order.
    pub values: Vec<(String, Decoded<f64>)>,
}

impl BlockSnapshot {
    /// A variable by name. A name the descriptor never declared is `Unknown`
    /// with that reason, so a typed view for a part that decodes less still
    /// answers instead of panicking.
    #[must_use]
    pub fn get(&self, name: &str) -> Decoded<f64> {
        self.values
            .iter()
            .find(|(n, _)| n == name)
            .map_or_else(
                || Decoded::Unknown(format!("{} does not decode `{name}`", self.name)),
                |(_, v)| v.clone(),
            )
    }
}

/// The decoded configuration of every block the descriptor declares.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PeripheralConfig {
    /// Decoded blocks, in descriptor order.
    pub blocks: Vec<BlockSnapshot>,
}

/// The clock tree, as decoded.
#[derive(Debug, Clone, PartialEq)]
pub struct ClockConfig {
    /// The RCC (or equivalent) block's name.
    pub name: String,
    /// The core clock, in Hz.
    pub core_hz: Decoded<f64>,
    /// Every decoded variable of the tree, `core_hz` included.
    pub values: Vec<(String, Decoded<f64>)>,
}

/// A UART's decoded line settings.
#[derive(Debug, Clone, PartialEq)]
pub struct UartConfig {
    /// The peripheral's name.
    pub name: String,
    /// Baud rate, in bits per second.
    pub baud: Decoded<f64>,
    /// Data bits per frame.
    pub data_bits: Decoded<f64>,
    /// Stop bits per frame.
    pub stop_bits: Decoded<f64>,
}

/// An SPI controller's decoded mode.
#[derive(Debug, Clone, PartialEq)]
pub struct SpiConfig {
    /// The peripheral's name.
    pub name: String,
    /// SCK frequency, in Hz.
    pub sck_hz: Decoded<f64>,
    /// Clock polarity, 0 or 1.
    pub cpol: Decoded<f64>,
    /// Clock phase, 0 or 1.
    pub cpha: Decoded<f64>,
}

/// An I2C controller's decoded speed.
#[derive(Debug, Clone, PartialEq)]
pub struct I2cConfig {
    /// The peripheral's name.
    pub name: String,
    /// SCL frequency, in Hz.
    pub scl_hz: Decoded<f64>,
}

/// A timer's decoded rate.
#[derive(Debug, Clone, PartialEq)]
pub struct TimerConfig {
    /// The peripheral's name.
    pub name: String,
    /// The counter's tick rate after the prescaler, in Hz.
    pub tick_hz: Decoded<f64>,
    /// The update period, in seconds.
    pub period_s: Decoded<f64>,
}

/// An ADC's decoded conversion settings.
#[derive(Debug, Clone, PartialEq)]
pub struct AdcConfig {
    /// The peripheral's name.
    pub name: String,
    /// The ADC clock, in Hz.
    pub clock_hz: Decoded<f64>,
    /// Resolution, in bits.
    pub resolution_bits: Decoded<f64>,
    /// Sampling time, in ADC clock cycles.
    pub sample_cycles: Decoded<f64>,
}

impl PeripheralConfig {
    /// Read every register the blocks name from `reader` and decode them.
    ///
    /// Total: a failed read or a bad encoding becomes [`Decoded::Unknown`] on
    /// the variables that depend on it and nowhere else. The first clock block
    /// is decoded first so its variables are available to the others.
    pub fn decode(blocks: &[DecodeBlock], reader: &mut dyn RegisterReader) -> Self {
        let fc = first_clock(blocks);
        // The first clock block goes first so the others can read it.
        let order = blocks
            .get(fc)
            .map(|_| fc)
            .into_iter()
            .chain((0..blocks.len()).filter(|i| *i != fc));
        let mut decoded: Vec<Option<BlockSnapshot>> = vec![None; blocks.len()];
        let mut clock: Vec<(String, Decoded<f64>)> = Vec::new();
        for i in order {
            let block = &blocks[i];
            let outer: &[(String, Decoded<f64>)] = if block.kind == BlockKind::Clock {
                &[]
            } else {
                &clock
            };
            let snap = decode_block(block, outer, reader);
            if i == fc {
                clock = snap
                    .values
                    .iter()
                    .map(|(n, v)| (format!("{CLOCK_PREFIX}{n}"), v.clone()))
                    .collect();
            }
            decoded[i] = Some(snap);
        }
        Self {
            blocks: decoded.into_iter().flatten().collect(),
        }
    }

    fn of_kind(&self, kind: BlockKind) -> impl Iterator<Item = &BlockSnapshot> {
        self.blocks.iter().filter(move |b| b.kind == kind)
    }

    /// The first clock tree, when the descriptor declares one.
    #[must_use]
    pub fn clock(&self) -> Option<ClockConfig> {
        self.of_kind(BlockKind::Clock).next().map(|b| ClockConfig {
            name: b.name.clone(),
            core_hz: b.get("core_hz"),
            values: b.values.clone(),
        })
    }

    /// Every decoded UART.
    #[must_use]
    pub fn uarts(&self) -> Vec<UartConfig> {
        self.of_kind(BlockKind::Uart)
            .map(|b| UartConfig {
                name: b.name.clone(),
                baud: b.get("baud"),
                data_bits: b.get("data_bits"),
                stop_bits: b.get("stop_bits"),
            })
            .collect()
    }

    /// Every decoded SPI controller.
    #[must_use]
    pub fn spis(&self) -> Vec<SpiConfig> {
        self.of_kind(BlockKind::Spi)
            .map(|b| SpiConfig {
                name: b.name.clone(),
                sck_hz: b.get("sck_hz"),
                cpol: b.get("cpol"),
                cpha: b.get("cpha"),
            })
            .collect()
    }

    /// Every decoded I2C controller.
    #[must_use]
    pub fn i2cs(&self) -> Vec<I2cConfig> {
        self.of_kind(BlockKind::I2c)
            .map(|b| I2cConfig {
                name: b.name.clone(),
                scl_hz: b.get("scl_hz"),
            })
            .collect()
    }

    /// Every decoded timer.
    #[must_use]
    pub fn timers(&self) -> Vec<TimerConfig> {
        self.of_kind(BlockKind::Timer)
            .map(|b| TimerConfig {
                name: b.name.clone(),
                tick_hz: b.get("tick_hz"),
                period_s: b.get("period_s"),
            })
            .collect()
    }

    /// Every decoded ADC.
    #[must_use]
    pub fn adcs(&self) -> Vec<AdcConfig> {
        self.of_kind(BlockKind::Adc)
            .map(|b| AdcConfig {
                name: b.name.clone(),
                clock_hz: b.get("clock_hz"),
                resolution_bits: b.get("resolution_bits"),
                sample_cycles: b.get("sample_cycles"),
            })
            .collect()
    }
}

fn first_clock(blocks: &[DecodeBlock]) -> usize {
    blocks
        .iter()
        .position(|b| b.kind == BlockKind::Clock)
        .unwrap_or(usize::MAX)
}

/// The clock tree alone: the registers of the first clock block, decoded. The
/// retune loop calls this every chunk, so it reads only that block.
pub fn decode_clock(blocks: &[DecodeBlock], reader: &mut dyn RegisterReader) -> Option<ClockConfig> {
    let block = blocks.iter().find(|b| b.kind == BlockKind::Clock)?;
    let snap = decode_block(block, &[], reader);
    Some(ClockConfig {
        name: snap.name.clone(),
        core_hz: snap.get("core_hz"),
        values: snap.values,
    })
}

fn decode_block(
    block: &DecodeBlock,
    outer: &[(String, Decoded<f64>)],
    reader: &mut dyn RegisterReader,
) -> BlockSnapshot {
    let mut scope: BTreeMap<String, Decoded<f64>> = outer.iter().cloned().collect();
    for (name, address) in &block.registers {
        let value = reader.read_u32(*address).map_or_else(
            || Decoded::Unknown(format!("register {name} at 0x{address:08X} could not be read")),
            |w| Decoded::Known(f64::from(w)),
        );
        scope.insert(name.clone(), value);
    }
    let mut values = Vec::with_capacity(block.vars.len());
    for (name, src) in &block.vars {
        let value = match Expr::parse(src) {
            Err(e) => Decoded::Unknown(format!("{name}: {e}")),
            Ok(expr) => {
                let result = expr.eval(&|v| match scope.get(v) {
                    Some(Decoded::Known(x)) => Ok(*x),
                    Some(Decoded::Unknown(why)) => err(format!("{v} is unknown: {why}")),
                    None => err(format!("`{v}` is not defined")),
                });
                match result {
                    Ok(x) if x.is_finite() => Decoded::Known(x),
                    Ok(x) => Decoded::Unknown(format!("{name} evaluated to {x}")),
                    Err(e) => Decoded::Unknown(e.0),
                }
            }
        };
        scope.insert(name.clone(), value.clone());
        values.push((name.clone(), value));
    }
    BlockSnapshot {
        kind: block.kind,
        name: block.name.clone(),
        values,
    }
}

// ── Clock targets and readiness (descriptor schema shared by backends) ─

/// One `[[soc.timer_fixes]]` entry: a timer whose emulation departs from
/// silicon in two ways that every polled time base runs into.
///
/// 1. The update flag latches only while the update-interrupt enable is set.
///    Silicon sets UIF on every update event (RM0090 Rev 22 p.631, TIM2 to TIM5
///    TIMx_SR bit 0: "set by hardware on an update event"); TIMx_DIER UIE only
///    gates the interrupt request. Firmware that polls UIF with the interrupt
///    masked never sees it. The backend keeps the enable bit set inside the
///    emulator and shows the firmware its own value on reads.
/// 2. The counter wraps after ARR ticks, not ARR + 1. Silicon counts from 0 to
///    the auto-reload value and then restarts from 0 (RM0090 Rev 22 s18.3.2
///    p.589, TIM2 to TIM5 upcounting mode: "counts from 0 to the auto-reload
///    value (content of the TIMx_ARR register), then restarts from 0 and
///    generates a counter overflow event"), so the update period is ARR + 1
///    ticks. The backend writes ARR + 1 to the emulator and shows the firmware
///    its own value on reads. A value of 0xFFFF or more is left alone (the
///    field may be 16 bits wide, and the error is below 2e-5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TimerFix {
    /// The Renode peripheral name, e.g. `timer2`.
    pub peripheral: String,
    /// Offset of the interrupt-enable register (TIMx_DIER, +0x0C).
    pub enable_register: u32,
    /// The update-interrupt-enable bit (UIE, bit 0).
    pub enable_mask: u32,
    /// Offset of the auto-reload register (TIMx_ARR, +0x2C), when the
    /// emulated period is short by one tick.
    #[serde(default)]
    pub auto_reload_register: Option<u32>,
}

/// One `[[soc.clock_targets]]` entry: when the decoded clock variable `var`
/// changes, the backend runs `command` with the new value substituted.
///
/// Placeholders: `{hz}` (whole hertz), `{khz}` and `{mhz}` (whole units,
/// rounded to nearest, never below 1: a retune to zero would stop the machine).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClockTarget {
    /// A variable of the first clock block.
    pub var: String,
    /// The Monitor command that applies it.
    pub command: String,
}

/// Render a clock target's command for `hz`.
#[must_use]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // hz is positive and below 2^53
#[allow(clippy::literal_string_with_formatting_args)] // `{hz}` etc. are template placeholders
pub fn render_clock_command(template: &str, hz: f64) -> String {
    let rounded = hz.round().max(1.0) as u64;
    let unit = |div: f64| ((hz / div).round().max(1.0)) as u64;
    template
        .replace("{hz}", &rounded.to_string())
        .replace("{khz}", &unit(1e3).to_string())
        .replace("{mhz}", &unit(1e6).to_string())
}

/// A register condition: `(read & mask) == value`. Offsets are relative to the
/// readiness entry's peripheral.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegisterCondition {
    /// Byte offset within the peripheral.
    pub register: u32,
    /// Bits compared.
    pub mask: u32,
    /// Required value of the compared bits.
    pub value: u32,
}

/// A prerequisite of a readiness entry: another entry that must itself be
/// ready, optionally only while a register condition holds (a PLL needs its
/// HSE source only while `PLLSRC` selects HSE).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClockNeed {
    /// The prerequisite entry's `name`.
    pub entry: String,
    /// When present, the prerequisite applies only while this holds.
    #[serde(default)]
    pub when: Option<RegisterCondition>,
}

/// Startup time that scales with a firmware-written register, for oscillators
/// whose settling delay is programmable (RP2040 `XOSC_STARTUP.DELAY`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartupScale {
    /// Byte offset of the register within the peripheral.
    pub register: u32,
    /// Bits holding the count.
    pub mask: u32,
    /// Microseconds per count.
    pub us_per_count: f64,
}

/// One `[[soc.clock_ready]]` entry: an oscillator or PLL whose ready bit rises
/// only after its enable bit has been set for the startup time, and (for a
/// crystal) only if the board really has one.
///
/// This is the whole behaviour of an external oscillator, kept as data: the
/// backend installs one generic peripheral hook, and a part is described by
/// where its enable and ready bits are. See `renode::clock_ready_script`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClockReady {
    /// Peripheral that owns the registers, as in the platform (`rcc`).
    pub peripheral: String,
    /// Entry name (`hse`, `pll`, `xosc`), unique in the descriptor.
    pub name: String,
    /// Byte offset of the register holding the enable field.
    pub enable_register: u32,
    /// Bits of the enable field.
    pub enable_mask: u32,
    /// Value of the masked enable field that means enabled. Defaults to the
    /// whole mask (a single ON bit); RP2040 `XOSC_CTRL.ENABLE` is the 12-bit
    /// magic `0xFAB`.
    #[serde(default)]
    pub enable_value: Option<u32>,
    /// Byte offset of the register holding the ready bit.
    pub ready_register: u32,
    /// The ready bit.
    pub ready_mask: u32,
    /// True for a crystal or external oscillator: never ready on a board that
    /// has none at the clock pins.
    #[serde(default)]
    pub requires_external: bool,
    /// Startup time in microseconds from enable to ready.
    pub startup_us: f64,
    /// When set, the startup time is `(register & mask) * us_per_count` instead.
    #[serde(default)]
    pub startup_scale: Option<StartupScale>,
    /// Entries that must be ready first.
    #[serde(default)]
    pub needs: Vec<ClockNeed>,
}

impl ClockReady {
    /// The enable-field value that means enabled.
    #[must_use]
    pub fn enabled_value(&self) -> u32 {
        self.enable_value.unwrap_or(self.enable_mask)
    }
}

/// Why a readiness or target table was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ClockTableError {
    /// Two readiness entries share a name.
    #[error("clock_ready entry {0:?} is declared twice")]
    DuplicateEntry(String),
    /// A `needs` names an entry that does not exist.
    #[error("clock_ready entry {entry:?} needs {need:?}, which is not declared")]
    UnknownNeed {
        /// The entry.
        entry: String,
        /// The missing prerequisite.
        need: String,
    },
    /// A readiness entry with a mask of zero or a negative/zero startup.
    #[error("clock_ready entry {entry:?}: {reason}")]
    BadEntry {
        /// The entry.
        entry: String,
        /// What is wrong.
        reason: &'static str,
    },
    /// A clock target naming a variable the first clock block does not define.
    #[error("clock_targets var {0:?} is not a variable of the clock decode block")]
    UnknownTargetVar(String),
    /// A clock target command without a value placeholder.
    #[error("clock_targets command for {0:?} has no {{hz}}, {{khz}} or {{mhz}} placeholder")]
    NoPlaceholder(String),
}

/// Validate readiness entries and targets against the decode blocks.
///
/// # Errors
///
/// The first inconsistency found.
#[allow(clippy::literal_string_with_formatting_args)] // `{hz}` etc. are template placeholders
pub fn validate_clock_tables(
    blocks: &[DecodeBlock],
    targets: &[ClockTarget],
    ready: &[ClockReady],
) -> Result<(), ClockTableError> {
    for (i, entry) in ready.iter().enumerate() {
        if ready[..i].iter().any(|e| e.name == entry.name) {
            return Err(ClockTableError::DuplicateEntry(entry.name.clone()));
        }
        if entry.enable_mask == 0 || entry.ready_mask == 0 {
            return Err(ClockTableError::BadEntry {
                entry: entry.name.clone(),
                reason: "enable_mask and ready_mask must be nonzero",
            });
        }
        if !entry.startup_us.is_finite() || entry.startup_us < 0.0 {
            return Err(ClockTableError::BadEntry {
                entry: entry.name.clone(),
                reason: "startup_us must be a finite, non-negative number",
            });
        }
        for need in &entry.needs {
            if !ready.iter().any(|e| e.name == need.entry) || need.entry == entry.name {
                return Err(ClockTableError::UnknownNeed {
                    entry: entry.name.clone(),
                    need: need.entry.clone(),
                });
            }
        }
    }
    let clock_vars: Vec<&str> = blocks
        .iter()
        .find(|b| b.kind == BlockKind::Clock)
        .map(|b| b.vars.iter().map(|(n, _)| n.as_str()).collect())
        .unwrap_or_default();
    for t in targets {
        if !clock_vars.contains(&t.var.as_str()) {
            return Err(ClockTableError::UnknownTargetVar(t.var.clone()));
        }
        if !["{hz}", "{khz}", "{mhz}"].iter().any(|p| t.command.contains(p)) {
            return Err(ClockTableError::NoPlaceholder(t.var.clone()));
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::float_cmp)] // register-derived values are exact integers in f64
mod tests {
    use super::*;

    fn regs(pairs: &[(u32, u32)]) -> BTreeMap<u32, u32> {
        pairs.iter().copied().collect()
    }

    fn ev(src: &str) -> Result<f64, ExprError> {
        Expr::parse(src)?.eval(&|v| err(format!("no {v}")))
    }

    #[test]
    fn arithmetic_and_precedence() {
        assert_eq!(ev("1 + 2 * 3").unwrap(), 7.0);
        assert_eq!(ev("(1 + 2) * 3").unwrap(), 9.0);
        assert_eq!(ev("16_000_000 / 16 * 336 / 2").unwrap(), 168_000_000.0);
        assert_eq!(ev("0x10 + 1").unwrap(), 17.0);
        assert_eq!(ev("-2 + 5 % 3").unwrap(), 0.0);
        assert_eq!(ev("1e6 / 4").unwrap(), 250_000.0);
        assert_eq!(ev("2 < 3 && 3 <= 3 && !(1 == 2)").unwrap(), 1.0);
        assert_eq!(ev("0 || 0").unwrap(), 0.0);
    }

    #[test]
    fn bit_functions_extract_fields() {
        // 0x27005410: PLLM[5:0] = 0x10, PLLN[14:6] = 0x150, PLLP[17:16] = 0.
        assert_eq!(ev("bits(0x27005410, 5, 0)").unwrap(), 16.0);
        assert_eq!(ev("bits(0x27005410, 14, 6)").unwrap(), 336.0);
        assert_eq!(ev("bits(0x27005410, 17, 16)").unwrap(), 0.0);
        assert_eq!(ev("bits(0x27005410, 27, 24)").unwrap(), 7.0);
        assert_eq!(ev("bit(0x03000483, 25)").unwrap(), 1.0);
        assert_eq!(ev("bit(0x03000483, 17)").unwrap(), 0.0);
        assert!(ev("bits(1.5, 3, 0)").is_err());
        assert!(ev("bits(1, 2, 5)").is_err());
    }

    #[test]
    fn pick_and_if_are_lazy() {
        assert_eq!(ev("pick(1, 10, 20, 30)").unwrap(), 20.0);
        // The unselected branch divides by zero but is never evaluated.
        assert_eq!(ev("pick(0, 5, 1 / 0)").unwrap(), 5.0);
        assert_eq!(ev("if(1, 7, 1 / 0)").unwrap(), 7.0);
        assert_eq!(ev("if(0, 1 / 0, 8)").unwrap(), 8.0);
        let reserved = ev("pick(3, 1, 2)").unwrap_err();
        assert!(reserved.0.contains("reserved"), "{reserved}");
    }

    #[test]
    fn syntax_errors_are_named() {
        assert!(Expr::parse("").is_err());
        assert!(Expr::parse("1 +").is_err());
        assert!(Expr::parse("(1").is_err());
        assert!(Expr::parse("1 2").is_err());
        assert!(Expr::parse("nope(1)").unwrap_err().0.contains("unknown function"));
        assert!(Expr::parse("bits(1, 2)").unwrap_err().0.contains("3 argument"));
        assert!(ev("1 / 0").unwrap_err().0.contains("division by zero"));
    }

    #[test]
    fn free_vars_lists_each_name_once() {
        let e = Expr::parse("bits(cfgr, 7, 4) + cfgr + hsi").unwrap();
        assert_eq!(e.free_vars(), vec!["cfgr".to_string(), "hsi".to_string()]);
    }

    /// RM0090 Rev 22 pp.164-168 (PLLCFGR, CFGR): HSI 16 MHz, PLLM = 16,
    /// PLLN = 336, PLLP = /2, SYSCLK = PLL, HPRE = /1, PPRE1 = /4, PPRE2 = /2.
    /// VCO = 16 MHz x 336 / 16 = 336 MHz, SYSCLK = 168 MHz, APB1 = 42 MHz,
    /// APB2 = 84 MHz; timers x2 on both buses because the prescalers are not 1:
    /// 84 MHz and 168 MHz.
    fn f4_block() -> DecodeBlock {
        DecodeBlock {
            kind: BlockKind::Clock,
            name: "rcc".into(),
            registers: [
                ("cr".to_string(), 0x4002_3800),
                ("pllcfgr".to_string(), 0x4002_3804),
                ("cfgr".to_string(), 0x4002_3808),
            ]
            .into_iter()
            .collect(),
            vars: [
                ("hsi", "16000000"),
                ("hse", "8000000"),
                ("pll_in", "if(bit(pllcfgr, 22), hse, hsi)"),
                (
                    "pll_out",
                    "pll_in / bits(pllcfgr, 5, 0) * bits(pllcfgr, 14, 6) / pick(bits(pllcfgr, 17, 16), 2, 4, 6, 8)",
                ),
                ("sysclk", "pick(bits(cfgr, 3, 2), hsi, hse, pll_out)"),
                (
                    "hclk",
                    "sysclk / pick(bits(cfgr, 7, 4), 1, 1, 1, 1, 1, 1, 1, 1, 2, 4, 8, 16, 64, 128, 256, 512)",
                ),
                ("pclk1_div", "pick(bits(cfgr, 12, 10), 1, 1, 1, 1, 2, 4, 8, 16)"),
                ("pclk1", "hclk / pclk1_div"),
                ("pclk2_div", "pick(bits(cfgr, 15, 13), 1, 1, 1, 1, 2, 4, 8, 16)"),
                ("pclk2", "hclk / pclk2_div"),
                ("core_hz", "hclk"),
                ("tim1_hz", "if(pclk1_div == 1, pclk1, pclk1 * 2)"),
                ("tim2_hz", "if(pclk2_div == 1, pclk2, pclk2 * 2)"),
            ]
            .into_iter()
            .map(|(n, e)| (n.to_string(), e.to_string()))
            .collect(),
        }
    }

    fn f4_image(cfgr: u32) -> BTreeMap<u32, u32> {
        regs(&[
            (0x4002_3800, 0x0300_0483),
            (0x4002_3804, 0x2700_5410),
            (0x4002_3808, cfgr),
        ])
    }

    fn value(c: &ClockConfig, name: &str) -> f64 {
        c.values
            .iter()
            .find(|(n, _)| n == name)
            .and_then(|(_, v)| v.known())
            .unwrap_or_else(|| panic!("{name} not known in {c:?}"))
    }

    #[test]
    fn f4_pll_168_decodes_from_registers() {
        let blocks = [f4_block()];
        validate_blocks(&blocks).unwrap();
        let clock = decode_clock(&blocks, &mut f4_image(0x0000_940A)).unwrap();
        assert_eq!(clock.core_hz, Decoded::Known(168_000_000.0));
        assert_eq!(value(&clock, "pll_out"), 168_000_000.0);
        assert_eq!(value(&clock, "pclk1"), 42_000_000.0);
        assert_eq!(value(&clock, "pclk2"), 84_000_000.0);
        assert_eq!(value(&clock, "tim1_hz"), 84_000_000.0);
        assert_eq!(value(&clock, "tim2_hz"), 168_000_000.0);
    }

    #[test]
    fn f4_reset_state_is_hsi() {
        // CFGR reset value 0: SW = 00, SWS = 00 (HSI), all prescalers /1.
        let clock = decode_clock(&[f4_block()], &mut f4_image(0)).unwrap();
        assert_eq!(clock.core_hz, Decoded::Known(16_000_000.0));
        assert_eq!(value(&clock, "tim1_hz"), 16_000_000.0);
    }

    #[test]
    fn reserved_encoding_is_unknown_not_a_guess() {
        // SW = 0b11 is reserved on the F4 (RM0090 p.166).
        let clock = decode_clock(&[f4_block()], &mut f4_image(0x0000_000C)).unwrap();
        let why = clock.core_hz.reason().expect("reserved SW must be unknown");
        assert!(why.contains("reserved"), "{why}");
        // The sub-tree that does not depend on SW still decodes.
        assert_eq!(value(&clock, "pll_out"), 168_000_000.0);
    }

    #[test]
    fn unreadable_register_poisons_only_its_dependants() {
        let mut image = f4_image(0x0000_940A);
        image.remove(&0x4002_3808);
        let clock = decode_clock(&[f4_block()], &mut image).unwrap();
        assert!(clock.core_hz.is_unknown());
        assert!(clock.core_hz.reason().unwrap().contains("cfgr"));
        assert_eq!(value(&clock, "pll_out"), 168_000_000.0);
    }

    #[test]
    fn typed_views_read_declared_variables_and_abstain_on_the_rest() {
        let uart = DecodeBlock {
            kind: BlockKind::Uart,
            name: "usart1".into(),
            registers: [("brr".to_string(), 0x4001_100C)].into_iter().collect(),
            // baud = fPCLK / (16 x USARTDIV) and BRR holds USARTDIV as
            // mantissa[15:4].fraction[3:0] in 1/16ths, so 16 x USARTDIV = BRR.
            vars: vec![("baud".into(), "clk_pclk2 / brr".into())],
        };
        let blocks = [f4_block(), uart];
        validate_blocks(&blocks).unwrap();
        // USARTDIV = 45.5625 -> BRR = 0x2D9 (mantissa 45, fraction 9/16 = .5625).
        let mut image = f4_image(0x0000_940A);
        image.insert(0x4001_100C, 0x2D9);
        let cfg = PeripheralConfig::decode(&blocks, &mut image);
        let uarts = cfg.uarts();
        assert_eq!(uarts.len(), 1);
        // 84e6 / (16 x 45.5625) = 115_226.3 baud (the classic ~0.02 % error).
        let baud = uarts[0].baud.known().unwrap();
        assert!((baud - 115_226.337).abs() < 0.01, "{baud}");
        // The descriptor never declared data_bits: Unknown, with the reason.
        assert!(uarts[0].data_bits.reason().unwrap().contains("data_bits"));
        assert!(cfg.clock().unwrap().core_hz.known().is_some());
        assert!(cfg.spis().is_empty());
    }

    #[test]
    fn validation_names_the_problem() {
        let mut b = f4_block();
        b.vars.push(("bad".into(), "nosuch + 1".into()));
        assert!(matches!(
            validate_blocks(&[b]),
            Err(DecodeError::UnknownName { name, .. }) if name == "nosuch"
        ));
        let mut b = f4_block();
        b.vars.push(("oops".into(), "1 +".into()));
        assert!(matches!(validate_blocks(&[b]), Err(DecodeError::BadExpr { .. })));
        let mut b = f4_block();
        b.vars.push(("cr".into(), "1".into()));
        assert!(matches!(validate_blocks(&[b]), Err(DecodeError::Duplicate { .. })));
        assert!(matches!(
            validate_blocks(&[f4_block(), f4_block()]),
            Err(DecodeError::DuplicateBlock { .. })
        ));
    }

    #[test]
    fn clock_commands_render_whole_units() {
        assert_eq!(
            render_clock_command("sysbus.cpu PerformanceInMips {mhz}", 168e6),
            "sysbus.cpu PerformanceInMips 168"
        );
        assert_eq!(render_clock_command("f {hz}", 2_097_152.0), "f 2097152");
        // A sub-MHz clock never renders 0 MIPS, which would stop the machine.
        assert_eq!(render_clock_command("m {mhz}", 65_536.0), "m 1");
        assert_eq!(render_clock_command("k {khz}", 32_768.0), "k 33");
    }

    #[test]
    fn clock_tables_are_cross_checked() {
        let blocks = [f4_block()];
        let target = |var: &str, cmd: &str| ClockTarget {
            var: var.into(),
            command: cmd.into(),
        };
        assert!(validate_clock_tables(&blocks, &[target("core_hz", "x {hz}")], &[]).is_ok());
        assert_eq!(
            validate_clock_tables(&blocks, &[target("nope", "x {hz}")], &[]),
            Err(ClockTableError::UnknownTargetVar("nope".into()))
        );
        assert_eq!(
            validate_clock_tables(&blocks, &[target("core_hz", "x")], &[]),
            Err(ClockTableError::NoPlaceholder("core_hz".into()))
        );
        let entry = |name: &str, needs: Vec<ClockNeed>| ClockReady {
            peripheral: "rcc".into(),
            name: name.into(),
            enable_register: 0,
            enable_mask: 1,
            enable_value: None,
            ready_register: 0,
            ready_mask: 2,
            requires_external: false,
            startup_us: 10.0,
            startup_scale: None,
            needs,
        };
        let need = |e: &str| ClockNeed {
            entry: e.into(),
            when: None,
        };
        assert_eq!(
            validate_clock_tables(&blocks, &[], &[entry("a", vec![]), entry("a", vec![])]),
            Err(ClockTableError::DuplicateEntry("a".into()))
        );
        assert_eq!(
            validate_clock_tables(&blocks, &[], &[entry("a", vec![need("b")])]),
            Err(ClockTableError::UnknownNeed {
                entry: "a".into(),
                need: "b".into()
            })
        );
        assert!(validate_clock_tables(
            &blocks,
            &[],
            &[entry("a", vec![]), entry("b", vec![need("a")])]
        )
        .is_ok());
    }
}
