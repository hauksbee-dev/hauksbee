//! Declarative digital-logic specification (`[models.logic]`).
//!
//! Instead of hand-coding each digital IC's behaviour in Rust,
//! a part's logic is described DECLARATIVELY here: its input/output pins,
//! combinational expressions, clocked registers, and tri-state groups. The
//! engine's generic `LogicComponent` evaluator realizes the spec against a
//! board's nets; this module owns the *format* and *validation* only, no
//! evaluation and no `evalexpr` (that lives engine-side, where the expression
//! evaluator already is, same split as `sensor_spec.rs`).
//! Long-form how-and-why: docs/how-and-why/hauksbee-models/logic_spec.md.
//!
//!
//! ## TOML shape
//!
//! ```toml
//! [models.logic]
//! inputs  = ["ser", "srclk", "rclk", "srclr_n", "oe_n"]
//! outputs = ["qa", "qb", "qc", "qd", "qe", "qf", "qg", "qh", "qh_serial"]
//!
//! [[models.logic.register]]
//! name = "shift"
//! bits = 8
//! clock = { pin = "srclk", edge = "rising" }
//! reset = { pin = "srclr_n", active = "low", value = 0 }
//! op = "shift_left"            # shift_left | shift_right | load | count_up | count_down
//! data_in = "ser"              # a pin, or (op = "load") another register's name
//!
//! [[models.logic.register]]
//! name = "store"
//! bits = 8
//! clock = { pin = "rclk", edge = "rising" }
//! op = "load"
//! data_in = "shift"            # a register can load from another register
//!
//! [models.logic.comb]
//! "qa" = "store[0]"            # ... through "qh" = "store[7]"
//! "qh_serial" = "shift[7]"     # pre-latch cascade tap for daisy chains
//!
//! [models.logic.tristate]
//! "qa..qh" = { enable = "oe_n", active = "low" }
//! ```
//!
//! Parallel-bus memories (28C EEPROMs, 62256 SRAMs, 27C ROMs) declare their
//! data array with `[[models.logic.memory]]` (see [`MemorySpec`]): an
//! addressable `words` x `bits` store, address pins LSB first, data read onto
//! tri-stated outputs, and an optional write strobe that samples the (same,
//! bidirectional) bus pins. The construct is feature-gated: it validates only
//! under [`Logic::validate_with_features`] with [`Logic::FEATURE_MEMORY`], so
//! an engine without a memory evaluator refuses the spec loudly instead of
//! binding a part whose bus it will never drive.
//!
//! ## Expressions
//!
//! Combinational expressions are boolean expressions over pin, output, and
//! register-bit names. The grammar is deliberately small (this is gate logic,
//! not arithmetic): identifiers (which may start with a digit, `1a` is a real
//! 74HC02 pin name), the literals `0`/`1`, `!` (NOT), `&` (AND), `^` (XOR),
//! `|` (OR), parentheses, and `name[i]` register-bit references. `&&`/`||`
//! are accepted as aliases. Precedence, tightest first: `!`, `&`, `^`, `|`.
//!
//! The engine cannot hand these strings to `evalexpr` directly (digit-led
//! identifiers, bitwise-style operators, and indexing are not evalexpr
//! syntax), so this module parses them into a [`LogicExpr`] AST that is the
//! shared contract: the validator walks it for undeclared names and width
//! errors, and the engine compiles it (once, at bind time) into an evalexpr
//! operator tree over sanitized boolean variables.
//!
//! ## Register semantics (the contract the engine implements)
//!
//! Registers hold up to 64 bits as an unsigned integer (bit 0 = LSB). No part
//! in the 74HC-class corpus carries a wider single register; a spec asking for
//! more fails validation rather than silently truncating. Per evaluation step:
//!
//! 1. every declared control pin is sampled;
//! 2. every register computes its NEXT value from the PRE-step state,
//!    priority: active `reset` (asynchronous, dominant: clock edges while
//!    reset is active leave the register at the reset value, matching the
//!    74HC595 datasheet's SRCLR row), then active `load` (asynchronous
//!    parallel load, dominant over the clock, matching the 74HC165's PL), then
//!    a qualifying clock edge applies `op`; otherwise hold;
//! 3. all registers commit simultaneously, a register loading from another
//!    register captures the pre-step value, so simultaneous shift+latch clocks
//!    behave like real tied-clock silicon (store one step behind shift);
//! 4. combinational outputs evaluate in dependency order; outputs forming a
//!    genuine combinational cycle (a cross-coupled latch) are resolved by
//!    fixpoint iteration in `outputs`-declaration order (the declared
//!    resolution order), seeded from the previous stable values (`init` at
//!    power-on). Convergence is verified exhaustively at bind time; the
//!    runtime iteration bound is a backstop, not a correctness knob.
//!

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::{Deserialize, Serialize};

/// Clock edge selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Edge {
    Rising,
    Falling,
}

/// Active level for a control pin (reset / load / enable).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Low,
    High,
}

impl Level {
    /// Is a sampled logic level "active" for this polarity?
    pub fn is_active(self, level: bool) -> bool {
        match self {
            Level::Low => !level,
            Level::High => level,
        }
    }
}

/// What a qualifying clock edge does to the register.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RegisterOp {
    /// `reg = (reg << 1) | data_in`, serial data enters at bit 0, exits at
    /// bit `bits-1` (the 74HC595 shift direction: bit 0 = QA).
    ShiftLeft,
    /// `reg = (reg >> 1) | (data_in << (bits-1))`, serial data enters at bit
    /// `bits-1`, exits at bit 0.
    ShiftRight,
    /// `reg = data_in` (another register of the same width, or, for a 1-bit
    /// register, a pin; the D flip-flop shape).
    Load,
    CountUp,
    CountDown,
}

/// The clock of a sequential register.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClockSpec {
    pub pin: String,
    pub edge: Edge,
}

/// An asynchronous, level-sensitive reset/preset to a constant value. A
/// register may declare several (the 74HC74 has independent CLR and PRE);
/// when more than one is active simultaneously the FIRST declared wins; the
/// silicon's both-asserted race (both outputs high) is not representable in a
/// single register and is deliberately not faked.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResetSpec {
    pub pin: String,
    pub active: Level,
    #[serde(default)]
    pub value: u64,
}

/// An asynchronous, level-sensitive parallel load from pins (the 74HC165 PL
/// shape). `data[i]` names the input pin captured into bit `i`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AsyncLoadSpec {
    pub pin: String,
    pub active: Level,
    pub data: Vec<String>,
}

/// A level-sensitive clock enable (the 74HC165 CLK_INH shape): clock edges
/// only take effect while the enable is active.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnableSpec {
    pub pin: String,
    pub active: Level,
}

/// One sequential register.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LogicRegister {
    pub name: String,
    /// Width in bits, 1..=64. The 64-bit bound is a deliberate refusal, not a
    /// convenience: no 74HC-class part carries a wider single register, and a
    /// spec that needs one should say so and get a real design decision, not a
    /// silent truncation.
    pub bits: u32,
    #[serde(default)]
    pub clock: Option<ClockSpec>,
    /// Asynchronous reset(s)/preset(s). TOML may write a single table
    /// (`reset = { pin = "...", ... }`) or an array of them.
    #[serde(
        default,
        rename = "reset",
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "one_or_many_resets"
    )]
    pub resets: Vec<ResetSpec>,
    #[serde(default)]
    pub op: Option<RegisterOp>,
    /// Serial/parallel data source for `shift_*` (an input pin) and `load`
    /// (another register's name, or an input pin for a 1-bit register).
    #[serde(default)]
    pub data_in: Option<String>,
    /// Asynchronous parallel load from pins.
    #[serde(default)]
    pub load: Option<AsyncLoadSpec>,
    #[serde(default)]
    pub clock_enable: Option<EnableSpec>,
    /// Power-on value.
    #[serde(default)]
    pub init: u64,
}

/// A tri-state group: while `enable` is NOT active the listed outputs go
/// high-impedance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TristateSpec {
    pub enable: String,
    pub active: Level,
}

/// One address/value write in a memory's command protocol. Parallel EEPROMs
/// use ordinary bus writes to enter and leave protected modes; declaring the
/// sequence keeps that device-specific protocol in model data rather than in
/// the engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryCommandWrite {
    pub address: u64,
    pub value: u64,
}

/// Software-data-protection command sequences for an EEPROM-like memory.
/// Writes are blocked while protection is enabled except for the exact
/// `disable` sequence. The engine consumes command writes rather than storing
/// them in the array, just as the physical device does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SoftwareDataProtectionSpec {
    /// Power-on protection state.
    #[serde(default)]
    pub initial: bool,
    /// Exact sequence that enables protection.
    pub enable: Vec<MemoryCommandWrite>,
    /// Exact sequence that disables protection.
    pub disable: Vec<MemoryCommandWrite>,
}

/// One addressable memory array (`[[models.logic.memory]]`): the data store
/// of a parallel-bus memory part (a 28C-series EEPROM, a 62256-class SRAM, a
/// 27C-series ROM), which is categorically wider than the 64-bit
/// `[[models.logic.register]]` ceiling. The array is declared, not enumerated:
/// `words` x `bits` cells, addressed by the `address` pins (LSB first), read
/// onto the `data_out` output pins, and (when `write` is present) committed
/// from the `data_in` pins on the write strobe's qualifying edge.
///
/// The data bus of a real memory is bidirectional. That is expressed by
/// declaring the bus pins as OUTPUTS (tri-stated via `[models.logic.tristate]`
/// on the read-enable condition) and naming those same pins in `data_in`: the
/// engine samples the pin's net level on the write edge, when the part itself
/// is not driving the bus. `data_in` is the one place a spec may reference an
/// output pin as a sampled value.
///
/// Evaluating a memory requires engine support that plain registers/comb do
/// not (backing storage, write-edge capture off the bus, and edge-exact
/// replay of µs-wide strobes). A spec using `[[models.logic.memory]]` only
/// validates when the caller passes [`Logic::FEATURE_MEMORY`] to
/// [`Logic::validate_with_features`]; the plain [`Logic::validate`] rejects it
/// loudly instead of letting an engine without memory support bind the part
/// and silently never drive its bus.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemorySpec {
    /// Array name; shares the one namespace with inputs, outputs, registers.
    pub name: String,
    /// Word count (32768 for a 28C256). `address` must carry exactly
    /// ceil(log2(words)) pins.
    pub words: u64,
    /// Bits per word (8 for a byte-wide EEPROM), 1..=64.
    pub bits: u32,
    /// Optional page size in words. Command-protected EEPROMs use this to
    /// bound the data phase armed by an enable sequence. It must be a power
    /// of two, no larger than the array, and divide the array exactly.
    #[serde(default)]
    pub page_words: Option<u64>,
    /// Maximum interval between successive byte loads in one page operation.
    /// Cycle-exact MCU responders use this to close an armed command-protected
    /// write window after the device's documented inter-byte timeout.
    #[serde(default)]
    pub byte_load_timeout_s: Option<f64>,
    /// Address pins, LSB first (`address[0]` = A0). Declared inputs.
    pub address: Vec<String>,
    /// Write strobe: the edge that commits `data_in` at the sampled address
    /// (the 28C256 latches address on WE falling, data on WE rising; the
    /// commit is modelled at the declared edge). Absent for a ROM.
    #[serde(default)]
    pub write: Option<ClockSpec>,
    /// Level gates that must ALL be active for a write edge to commit (CE
    /// low, for a chip-enable-qualified write). Declared inputs.
    #[serde(default)]
    pub write_gates: Vec<EnableSpec>,
    /// Level gates that must ALL be active for the memory to drive
    /// `data_out` (for example CE low, OE low, and WE high on a 28C256).
    /// Keeping the complete read condition here prevents a partially-gated
    /// bidirectional bus from fighting its master during writes.
    #[serde(default)]
    pub read_gates: Vec<EnableSpec>,
    /// Data pins sampled on the write edge, LSB first, one per bit. May name
    /// output pins (the bidirectional bus case above). Required with `write`,
    /// forbidden without it.
    #[serde(default)]
    pub data_in: Vec<String>,
    /// Output pins driven with the addressed word, LSB first, one per bit.
    /// Each must be a declared output and must not also be comb-assigned.
    pub data_out: Vec<String>,
    /// Initial value of every word (the erased state; 0xFF for an EEPROM).
    #[serde(default)]
    pub init: u64,
    /// Optional command-driven write protection, such as the AT28C256
    /// software-data-protection protocol.
    #[serde(default)]
    pub software_data_protection: Option<SoftwareDataProtectionSpec>,
}

/// The `[models.logic]` block.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Logic {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<String>,
    /// Output names in DECLARATION ORDER; this order is also the fixpoint
    /// resolution order for combinational cycles, so it is semantically
    /// load-bearing, not cosmetic.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outputs: Vec<String>,
    /// Output name -> boolean expression.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub comb: BTreeMap<String, String>,
    #[serde(default, rename = "register", skip_serializing_if = "Vec::is_empty")]
    pub registers: Vec<LogicRegister>,
    /// Output name or `a..b` inclusive range (over the `outputs` declaration
    /// order) -> tri-state control.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tristate: BTreeMap<String, TristateSpec>,
    /// Power-on levels (0/1) for comb outputs that participate in cycles (a
    /// latch must start in a legal state; a symmetric all-zero seed is the
    /// classic SR metastability). Non-cycle outputs need no init.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub init: BTreeMap<String, u8>,
    /// Addressable memory arrays (`[[models.logic.memory]]`); see
    /// [`MemorySpec`]. Feature-gated: only validates under
    /// [`Logic::validate_with_features`] with [`Logic::FEATURE_MEMORY`].
    #[serde(default, rename = "memory", skip_serializing_if = "Vec::is_empty")]
    pub memories: Vec<MemorySpec>,
}

impl Logic {
    /// True when no logic block was declared (the serde default).
    pub fn is_empty(&self) -> bool {
        self.inputs.is_empty()
            && self.outputs.is_empty()
            && self.comb.is_empty()
            && self.registers.is_empty()
            && self.tristate.is_empty()
            && self.init.is_empty()
            && self.memories.is_empty()
    }
}

/// Accept `reset = { ... }` (one table) or `reset = [{ ... }, ...]` (array).
fn one_or_many_resets<'de, D>(de: D) -> Result<Vec<ResetSpec>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(ResetSpec),
        Many(Vec<ResetSpec>),
    }
    Ok(match OneOrMany::deserialize(de)? {
        OneOrMany::One(r) => vec![r],
        OneOrMany::Many(v) => v,
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// Expression AST + parser
// ─────────────────────────────────────────────────────────────────────────────

/// A parsed combinational expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogicExpr {
    Const(bool),
    /// A pin, output, or (invalidly; the validator rejects it) register name.
    Name(String),
    /// `register[bit]`.
    Bit(String, u32),
    Not(Box<LogicExpr>),
    And(Box<LogicExpr>, Box<LogicExpr>),
    Xor(Box<LogicExpr>, Box<LogicExpr>),
    Or(Box<LogicExpr>, Box<LogicExpr>),
}

impl LogicExpr {
    /// Walk the AST collecting every referenced name: plain names and bit
    /// references separately.
    pub fn collect_refs<'a>(&'a self, names: &mut Vec<&'a str>, bits: &mut Vec<(&'a str, u32)>) {
        match self {
            LogicExpr::Const(_) => {}
            LogicExpr::Name(n) => names.push(n),
            LogicExpr::Bit(n, i) => bits.push((n, *i)),
            LogicExpr::Not(a) => a.collect_refs(names, bits),
            LogicExpr::And(a, b) | LogicExpr::Xor(a, b) | LogicExpr::Or(a, b) => {
                a.collect_refs(names, bits);
                b.collect_refs(names, bits);
            }
        }
    }
}

/// An expression parse failure (position is a byte offset into the source).
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
#[error("{msg} at offset {at} in {src:?}")]
pub struct LogicExprError {
    pub msg: String,
    pub at: usize,
    pub src: String,
}

struct Parser<'a> {
    src: &'a str,
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn err<T>(&self, msg: impl Into<String>) -> Result<T, LogicExprError> {
        Err(LogicExprError {
            msg: msg.into(),
            at: self.pos,
            src: self.src.to_string(),
        })
    }

    fn skip_ws(&mut self) {
        while self.pos < self.bytes.len() && self.bytes[self.pos].is_ascii_whitespace() {
            self.pos += 1;
        }
    }

    fn peek(&mut self) -> Option<u8> {
        self.skip_ws();
        self.bytes.get(self.pos).copied()
    }

    /// `expr := xor ( '|' xor )*`  (accepts `||` as an alias)
    fn parse_or(&mut self) -> Result<LogicExpr, LogicExprError> {
        let mut lhs = self.parse_xor()?;
        while self.peek() == Some(b'|') {
            self.pos += 1;
            if self.bytes.get(self.pos) == Some(&b'|') {
                self.pos += 1;
            }
            let rhs = self.parse_xor()?;
            lhs = LogicExpr::Or(Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    /// `xor := and ( '^' and )*`
    fn parse_xor(&mut self) -> Result<LogicExpr, LogicExprError> {
        let mut lhs = self.parse_and()?;
        while self.peek() == Some(b'^') {
            self.pos += 1;
            let rhs = self.parse_and()?;
            lhs = LogicExpr::Xor(Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    /// `and := unary ( '&' unary )*`  (accepts `&&` as an alias)
    fn parse_and(&mut self) -> Result<LogicExpr, LogicExprError> {
        let mut lhs = self.parse_unary()?;
        while self.peek() == Some(b'&') {
            self.pos += 1;
            if self.bytes.get(self.pos) == Some(&b'&') {
                self.pos += 1;
            }
            let rhs = self.parse_unary()?;
            lhs = LogicExpr::And(Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    /// `unary := '!' unary | atom`
    fn parse_unary(&mut self) -> Result<LogicExpr, LogicExprError> {
        if self.peek() == Some(b'!') {
            self.pos += 1;
            return Ok(LogicExpr::Not(Box::new(self.parse_unary()?)));
        }
        self.parse_atom()
    }

    /// `atom := '(' expr ')' | '0' | '1' | ident ( '[' int ']' )?`
    fn parse_atom(&mut self) -> Result<LogicExpr, LogicExprError> {
        match self.peek() {
            Some(b'(') => {
                self.pos += 1;
                let inner = self.parse_or()?;
                if self.peek() != Some(b')') {
                    return self.err("expected ')'");
                }
                self.pos += 1;
                Ok(inner)
            }
            Some(c) if c.is_ascii_alphanumeric() || c == b'_' => {
                let start = self.pos;
                while self
                    .bytes
                    .get(self.pos)
                    .map(|b| b.is_ascii_alphanumeric() || *b == b'_')
                    .unwrap_or(false)
                {
                    self.pos += 1;
                }
                let ident = &self.src[start..self.pos];
                // A bare 0/1 is a constant; any other token (including
                // digit-led pin names like `1a`) is an identifier.
                if ident == "0" {
                    return Ok(LogicExpr::Const(false));
                }
                if ident == "1" {
                    return Ok(LogicExpr::Const(true));
                }
                if self.bytes.get(self.pos) == Some(&b'[') {
                    self.pos += 1;
                    let istart = self.pos;
                    while self
                        .bytes
                        .get(self.pos)
                        .map(|b| b.is_ascii_digit())
                        .unwrap_or(false)
                    {
                        self.pos += 1;
                    }
                    if istart == self.pos {
                        return self.err("expected a bit index after '['");
                    }
                    let idx: u32 =
                        self.src[istart..self.pos]
                            .parse()
                            .map_err(|_| LogicExprError {
                                msg: "bit index does not fit in u32".into(),
                                at: istart,
                                src: self.src.to_string(),
                            })?;
                    if self.bytes.get(self.pos) != Some(&b']') {
                        return self.err("expected ']'");
                    }
                    self.pos += 1;
                    return Ok(LogicExpr::Bit(ident.to_string(), idx));
                }
                Ok(LogicExpr::Name(ident.to_string()))
            }
            Some(_) => self.err("expected an identifier, literal, '!', or '('"),
            None => self.err("unexpected end of expression"),
        }
    }
}

/// Parse one combinational expression. See the module docs for the grammar.
pub fn parse_logic_expr(src: &str) -> Result<LogicExpr, LogicExprError> {
    let mut p = Parser {
        src,
        bytes: src.as_bytes(),
        pos: 0,
    };
    let expr = p.parse_or()?;
    p.skip_ws();
    if p.pos != p.bytes.len() {
        return p.err("trailing characters after expression");
    }
    Ok(expr)
}

// ─────────────────────────────────────────────────────────────────────────────
// Validation
// ─────────────────────────────────────────────────────────────────────────────

/// A named `[models.logic]` validation failure. Every category the evaluator
/// depends on gets its own variant so a broken spec (hand-written or
/// LLM-extracted) fails with a diagnosis, not a generic parse error.
#[derive(Debug, Clone, thiserror::Error)]
pub enum LogicSpecError {
    #[error("logic block declares no outputs (a part that drives nothing is a spec bug)")]
    NoOutputs,

    #[error("duplicate logic name '{name}' ({first} and {second}); inputs, outputs, and registers share one namespace")]
    DuplicateName {
        name: String,
        first: &'static str,
        second: &'static str,
    },

    #[error("{context} references undeclared name '{name}'")]
    UndeclaredName { context: String, name: String },

    #[error("{context}: pin '{pin}' is not a declared input")]
    UndeclaredPin { context: String, pin: String },

    #[error("comb assigns '{name}', which is not a declared output")]
    CombTargetNotOutput { name: String },

    #[error("output '{name}' is declared but never assigned by comb (unreachable output)")]
    UnassignedOutput { name: String },

    #[error(
        "width mismatch: {context} uses register '{register}' ({bits} bits) as a 1-bit value; \
         reference a single bit ('{register}[i]') instead"
    )]
    RegisterAsScalar {
        context: String,
        register: String,
        bits: u32,
    },

    #[error(
        "{context}: bit index {index} is out of range for register '{register}' ({bits} bits)"
    )]
    BitIndexOutOfRange {
        context: String,
        register: String,
        index: u32,
        bits: u32,
    },

    #[error("{context}: '{name}[{index}]' indexes a non-register name (only registers have bits)")]
    BitRefNotRegister {
        context: String,
        name: String,
        index: u32,
    },

    #[error(
        "clock pin '{pin}' of register '{register}' is also referenced combinationally by \
         output '{output}': edge semantics would be ambiguous (does the output see the level \
         before or after the edge?). Route the level through a register or a separate pin."
    )]
    ClockAlsoComb {
        pin: String,
        register: String,
        output: String,
    },

    #[error(
        "register '{name}' declares {bits} bits; the evaluator holds register state in a u64 \
         (64-bit bound, wide enough for every 74HC-class part). Widen the bound deliberately \
         rather than truncating."
    )]
    RegisterTooWide { name: String, bits: u32 },

    #[error("register '{name}' declares zero bits")]
    RegisterZeroWidth { name: String },

    #[error(
        "width mismatch: register '{register}' reset value {value:#x} does not fit in {bits} bits"
    )]
    ResetValueTooWide {
        register: String,
        value: u64,
        bits: u32,
    },

    #[error(
        "width mismatch: register '{register}' init value {value:#x} does not fit in {bits} bits"
    )]
    InitValueTooWide {
        register: String,
        value: u64,
        bits: u32,
    },

    #[error(
        "register '{register}' async load lists {got} data pins but the register is {bits} bits \
         (one pin per bit)"
    )]
    LoadWidthMismatch {
        register: String,
        bits: u32,
        got: usize,
    },

    #[error(
        "width mismatch: register '{register}' (op = load, {bits} bits) loads from register \
         '{data_in}' ({from_bits} bits); widths must match"
    )]
    DataInWidthMismatch {
        register: String,
        bits: u32,
        data_in: String,
        from_bits: u32,
    },

    #[error("register '{register}': op {op:?} requires data_in")]
    DataInMissing { register: String, op: RegisterOp },

    #[error("register '{register}': op {op:?} takes no data_in")]
    DataInForbidden { register: String, op: RegisterOp },

    #[error(
        "register '{register}': data_in '{data_in}' must be a declared input pin for op \
         {op:?} (register-to-register data is op = \"load\" only)"
    )]
    DataInNotPin {
        register: String,
        op: RegisterOp,
        data_in: String,
    },

    #[error(
        "width mismatch: register '{register}' (op = load, {bits} bits) cannot load from pin \
         '{data_in}' (1 bit); pin-fed load requires a 1-bit register"
    )]
    DataInPinWidth {
        register: String,
        bits: u32,
        data_in: String,
    },

    #[error("register '{register}' has a clock but no op (what does the edge do?)")]
    OpMissing { register: String },

    #[error("register '{register}' has an op but no clock (nothing generates the edge)")]
    ClockMissing { register: String },

    #[error(
        "register '{register}' has no clock, no async load, and no reset: its value can never \
         change (dead state is a spec bug)"
    )]
    DeadRegister { register: String },

    #[error("register '{register}' declares clock_enable without a clock")]
    EnableWithoutClock { register: String },

    #[error("tristate group '{group}' names '{name}', which is not a declared output")]
    TristateUnknownOutput { group: String, name: String },

    #[error(
        "tristate range '{group}' is invalid: both endpoints must be declared outputs and the \
         start must not come after the end in the outputs declaration order"
    )]
    TristateRangeInvalid { group: String },

    #[error("tristate group '{group}': enable pin '{enable}' is not a declared input")]
    TristateEnableUndeclared { group: String, enable: String },

    #[error("init names '{name}', which is not a comb-assigned output")]
    InitNotCombOutput { name: String },

    #[error("init value for '{name}' must be 0 or 1, got {value}")]
    InitValueInvalid { name: String, value: u8 },

    #[error("comb expression for '{output}': {source}")]
    ExprParse {
        output: String,
        source: LogicExprError,
    },

    #[error(
        "memory '{name}': this engine does not evaluate [[models.logic.memory]] arrays \
         (validate_with_features was not passed \"memory\"); the part must be left unmodeled \
         loudly, not bound with a bus that never drives"
    )]
    MemoryNotSupported { name: String },

    #[error("memory '{name}' declares {bits} bits per word; the valid range is 1..=64")]
    MemoryWidthInvalid { name: String, bits: u32 },

    #[error("memory '{name}' declares {words} word(s); an addressable array needs at least 2")]
    MemoryWordsInvalid { name: String, words: u64 },

    #[error(
        "memory '{name}' declares page_words={page_words}; page size must be a nonzero power of \
         two, no larger than and evenly dividing the {words}-word array"
    )]
    MemoryPageWordsInvalid {
        name: String,
        page_words: u64,
        words: u64,
    },

    #[error(
        "memory '{name}' declares byte_load_timeout_s={seconds}; it must be finite and positive"
    )]
    MemoryByteLoadTimeoutInvalid { name: String, seconds: f64 },

    #[error("memory '{name}' declares byte_load_timeout_s without page_words")]
    MemoryPageTimingWithoutPage { name: String },

    #[error(
        "memory '{name}' declares {words} x {bits} bits, over the 128 Mbit cap; widen the \
         bound deliberately rather than allocating it by accident"
    )]
    MemoryTooLarge { name: String, words: u64, bits: u32 },

    #[error(
        "memory '{name}': {words} words need exactly {expected} address pin(s) \
         (ceil(log2(words)), LSB first), got {got}"
    )]
    MemoryAddressWidthMismatch {
        name: String,
        words: u64,
        expected: usize,
        got: usize,
    },

    #[error(
        "memory '{name}': {field} lists {got} pin(s) but words are {bits} bits (one pin per bit)"
    )]
    MemoryDataWidthMismatch {
        name: String,
        field: &'static str,
        bits: u32,
        got: usize,
    },

    #[error(
        "memory '{name}' lists data_in pins but declares no write strobe (a ROM takes no data)"
    )]
    MemoryDataInWithoutWrite { name: String },

    #[error("memory '{name}': data_out pin '{pin}' is not a declared output")]
    MemoryDataOutNotOutput { name: String, pin: String },

    #[error("output '{output}' is driven by more than one memory's data_out")]
    MemoryOutputConflict { output: String },

    #[error("output '{name}' is assigned both by comb and by a memory's data_out")]
    MemoryOutputAlsoComb { name: String },

    #[error("memory '{name}' declares software_data_protection but has no write strobe")]
    MemoryProtectionWithoutWrite { name: String },

    #[error("memory '{name}' software_data_protection {sequence} sequence is empty")]
    MemoryCommandSequenceEmpty {
        name: String,
        sequence: &'static str,
    },

    #[error(
        "memory '{name}' software_data_protection {sequence} command address {address:#x} is \
         outside its {words}-word array"
    )]
    MemoryCommandAddressOutOfRange {
        name: String,
        sequence: &'static str,
        address: u64,
        words: u64,
    },

    #[error(
        "memory '{name}' software_data_protection {sequence} command value {value:#x} does not \
         fit its {bits}-bit data bus"
    )]
    MemoryCommandValueTooWide {
        name: String,
        sequence: &'static str,
        value: u64,
        bits: u32,
    },

    #[error(
        "{context} references memory '{name}'; a memory array has no scalar value (its word \
         appears on the data_out pins)"
    )]
    MemoryInComb { context: String, name: String },
}

/// The validated, parsed form: every comb expression as an AST, in `outputs`
/// declaration order (the evaluation / fixpoint resolution order), plus the
/// cycle warnings validation raised.
#[derive(Debug, Clone)]
pub struct ValidatedLogic {
    /// `(output name, parsed expression)` in outputs-declaration order.
    pub comb: Vec<(String, LogicExpr)>,
    /// Human-readable warnings (combinational cycles found, legal, but the
    /// engine must verify fixpoint convergence at bind time).
    pub warnings: Vec<String>,
    /// Outputs participating in at least one combinational cycle.
    pub cyclic_outputs: HashSet<String>,
    /// The validated memory arrays, in declaration order. Non-empty only when
    /// validation was run with [`Logic::FEATURE_MEMORY`]; the engine's memory
    /// evaluator consumes these.
    pub memories: Vec<MemorySpec>,
}

impl Logic {
    /// The feature string an engine passes to [`Logic::validate_with_features`]
    /// to declare it evaluates `[[models.logic.memory]]` arrays.
    pub const FEATURE_MEMORY: &'static str = "memory";

    /// Structural validation with no optional engine features declared.
    /// Returns the parsed comb expressions (so the engine compiles the same
    /// ASTs the validator checked) plus cycle warnings. Every failure is a
    /// named [`LogicSpecError`]. A spec using feature-gated constructs
    /// (memory arrays) FAILS here; an engine that implements them calls
    /// [`Logic::validate_with_features`] instead.
    pub fn validate(&self) -> Result<ValidatedLogic, LogicSpecError> {
        self.validate_with_features(&[])
    }

    /// Structural validation, with `features` naming the optional spec
    /// constructs the calling engine actually evaluates (see
    /// [`Logic::FEATURE_MEMORY`]). A spec using a construct outside
    /// `features` is rejected loudly, never silently half-run: an engine
    /// without memory support must not bind a memory part whose bus it will
    /// never drive.
    pub fn validate_with_features(
        &self,
        features: &[&str],
    ) -> Result<ValidatedLogic, LogicSpecError> {
        if self.outputs.is_empty() {
            return Err(LogicSpecError::NoOutputs);
        }

        // One namespace: inputs, outputs, registers, memories.
        let declared: Vec<(&str, &'static str)> = self
            .inputs
            .iter()
            .map(|n| (n.as_str(), "input"))
            .chain(self.outputs.iter().map(|n| (n.as_str(), "output")))
            .chain(self.registers.iter().map(|r| (r.name.as_str(), "register")))
            .chain(self.memories.iter().map(|m| (m.name.as_str(), "memory")))
            .collect();
        let mut seen: HashMap<&str, &'static str> = HashMap::new();
        for (name, kind) in &declared {
            if let Some(first) = seen.insert(name, kind) {
                return Err(LogicSpecError::DuplicateName {
                    name: name.to_string(),
                    first,
                    second: kind,
                });
            }
        }

        let inputs: HashSet<&str> = self.inputs.iter().map(|s| s.as_str()).collect();
        let outputs: HashSet<&str> = self.outputs.iter().map(|s| s.as_str()).collect();
        let reg_bits: HashMap<&str, u32> = self
            .registers
            .iter()
            .map(|r| (r.name.as_str(), r.bits))
            .collect();

        let need_input = |context: String, pin: &str| -> Result<(), LogicSpecError> {
            if inputs.contains(pin) {
                Ok(())
            } else {
                Err(LogicSpecError::UndeclaredPin {
                    context,
                    pin: pin.to_string(),
                })
            }
        };

        // ── Registers ──
        for r in &self.registers {
            let ctx = |what: &str| format!("register '{}' {what}", r.name);
            if r.bits == 0 {
                return Err(LogicSpecError::RegisterZeroWidth {
                    name: r.name.clone(),
                });
            }
            if r.bits > 64 {
                return Err(LogicSpecError::RegisterTooWide {
                    name: r.name.clone(),
                    bits: r.bits,
                });
            }
            let mask = if r.bits == 64 {
                u64::MAX
            } else {
                (1u64 << r.bits) - 1
            };
            if r.init & !mask != 0 {
                return Err(LogicSpecError::InitValueTooWide {
                    register: r.name.clone(),
                    value: r.init,
                    bits: r.bits,
                });
            }
            match (&r.clock, &r.op) {
                (Some(_), None) => {
                    return Err(LogicSpecError::OpMissing {
                        register: r.name.clone(),
                    })
                }
                (None, Some(_)) => {
                    return Err(LogicSpecError::ClockMissing {
                        register: r.name.clone(),
                    })
                }
                _ => {}
            }
            if r.clock.is_none() && r.load.is_none() && r.resets.is_empty() {
                return Err(LogicSpecError::DeadRegister {
                    register: r.name.clone(),
                });
            }
            if r.clock.is_none() && r.clock_enable.is_some() {
                return Err(LogicSpecError::EnableWithoutClock {
                    register: r.name.clone(),
                });
            }
            if let Some(c) = &r.clock {
                need_input(ctx("clock"), &c.pin)?;
            }
            for rst in &r.resets {
                need_input(ctx("reset"), &rst.pin)?;
                if rst.value & !mask != 0 {
                    return Err(LogicSpecError::ResetValueTooWide {
                        register: r.name.clone(),
                        value: rst.value,
                        bits: r.bits,
                    });
                }
            }
            if let Some(en) = &r.clock_enable {
                need_input(ctx("clock_enable"), &en.pin)?;
            }
            if let Some(l) = &r.load {
                need_input(ctx("load"), &l.pin)?;
                if l.data.len() != r.bits as usize {
                    return Err(LogicSpecError::LoadWidthMismatch {
                        register: r.name.clone(),
                        bits: r.bits,
                        got: l.data.len(),
                    });
                }
                for p in &l.data {
                    need_input(ctx("load data"), p)?;
                }
            }
            match (&r.op, &r.data_in) {
                (Some(op @ (RegisterOp::ShiftLeft | RegisterOp::ShiftRight)), Some(d)) => {
                    if !inputs.contains(d.as_str()) {
                        return Err(LogicSpecError::DataInNotPin {
                            register: r.name.clone(),
                            op: *op,
                            data_in: d.clone(),
                        });
                    }
                }
                (Some(op @ (RegisterOp::ShiftLeft | RegisterOp::ShiftRight)), None) => {
                    return Err(LogicSpecError::DataInMissing {
                        register: r.name.clone(),
                        op: *op,
                    });
                }
                (Some(RegisterOp::Load), Some(d)) => {
                    if let Some(&from_bits) = reg_bits.get(d.as_str()) {
                        if from_bits != r.bits {
                            return Err(LogicSpecError::DataInWidthMismatch {
                                register: r.name.clone(),
                                bits: r.bits,
                                data_in: d.clone(),
                                from_bits,
                            });
                        }
                    } else if inputs.contains(d.as_str()) {
                        if r.bits != 1 {
                            return Err(LogicSpecError::DataInPinWidth {
                                register: r.name.clone(),
                                bits: r.bits,
                                data_in: d.clone(),
                            });
                        }
                    } else {
                        return Err(LogicSpecError::UndeclaredName {
                            context: format!("register '{}' data_in", r.name),
                            name: d.clone(),
                        });
                    }
                }
                (Some(RegisterOp::Load), None) => {
                    return Err(LogicSpecError::DataInMissing {
                        register: r.name.clone(),
                        op: RegisterOp::Load,
                    });
                }
                (Some(op @ (RegisterOp::CountUp | RegisterOp::CountDown)), Some(_)) => {
                    return Err(LogicSpecError::DataInForbidden {
                        register: r.name.clone(),
                        op: *op,
                    });
                }
                _ => {}
            }
        }

        // ── Memory arrays ──
        // Structural checks first, so a spec author gets the real defect even
        // on an engine that will then refuse the feature.
        let mut memory_outputs: HashSet<&str> = HashSet::new();
        for m in &self.memories {
            let ctx = |what: &str| format!("memory '{}' {what}", m.name);
            if m.bits == 0 || m.bits > 64 {
                return Err(LogicSpecError::MemoryWidthInvalid {
                    name: m.name.clone(),
                    bits: m.bits,
                });
            }
            if m.words < 2 {
                return Err(LogicSpecError::MemoryWordsInvalid {
                    name: m.name.clone(),
                    words: m.words,
                });
            }
            if let Some(page_words) = m.page_words {
                if page_words == 0
                    || !page_words.is_power_of_two()
                    || page_words > m.words
                    || m.words % page_words != 0
                {
                    return Err(LogicSpecError::MemoryPageWordsInvalid {
                        name: m.name.clone(),
                        page_words,
                        words: m.words,
                    });
                }
            }
            if let Some(seconds) = m.byte_load_timeout_s {
                if !seconds.is_finite() || seconds <= 0.0 {
                    return Err(LogicSpecError::MemoryByteLoadTimeoutInvalid {
                        name: m.name.clone(),
                        seconds,
                    });
                }
                if m.page_words.is_none() {
                    return Err(LogicSpecError::MemoryPageTimingWithoutPage {
                        name: m.name.clone(),
                    });
                }
            }
            // Total size cap: 128 Mbit covers every 27C/28C/62xx/39SF-class
            // part by a wide margin while refusing an absurd allocation.
            const MAX_TOTAL_BITS: u128 = 128 * 1024 * 1024;
            if (m.words as u128) * (m.bits as u128) > MAX_TOTAL_BITS {
                return Err(LogicSpecError::MemoryTooLarge {
                    name: m.name.clone(),
                    words: m.words,
                    bits: m.bits,
                });
            }
            let expected_addr = (64 - (m.words - 1).leading_zeros()) as usize;
            if m.address.len() != expected_addr {
                return Err(LogicSpecError::MemoryAddressWidthMismatch {
                    name: m.name.clone(),
                    words: m.words,
                    expected: expected_addr,
                    got: m.address.len(),
                });
            }
            for pin in &m.address {
                need_input(ctx("address"), pin)?;
            }
            if let Some(w) = &m.write {
                need_input(ctx("write strobe"), &w.pin)?;
                if m.data_in.len() != m.bits as usize {
                    return Err(LogicSpecError::MemoryDataWidthMismatch {
                        name: m.name.clone(),
                        field: "data_in",
                        bits: m.bits,
                        got: m.data_in.len(),
                    });
                }
                // A bidirectional bus: data_in may sample an OUTPUT pin (the
                // part tri-states while the bus master drives the write data).
                for pin in &m.data_in {
                    if !inputs.contains(pin.as_str()) && !outputs.contains(pin.as_str()) {
                        return Err(LogicSpecError::UndeclaredName {
                            context: ctx("data_in"),
                            name: pin.clone(),
                        });
                    }
                }
            } else if !m.data_in.is_empty() {
                return Err(LogicSpecError::MemoryDataInWithoutWrite {
                    name: m.name.clone(),
                });
            }
            for g in &m.write_gates {
                need_input(ctx("write gate"), &g.pin)?;
            }
            for g in &m.read_gates {
                need_input(ctx("read gate"), &g.pin)?;
            }
            if m.data_out.len() != m.bits as usize {
                return Err(LogicSpecError::MemoryDataWidthMismatch {
                    name: m.name.clone(),
                    field: "data_out",
                    bits: m.bits,
                    got: m.data_out.len(),
                });
            }
            for pin in &m.data_out {
                if !outputs.contains(pin.as_str()) {
                    return Err(LogicSpecError::MemoryDataOutNotOutput {
                        name: m.name.clone(),
                        pin: pin.clone(),
                    });
                }
                if !memory_outputs.insert(pin.as_str()) {
                    return Err(LogicSpecError::MemoryOutputConflict {
                        output: pin.clone(),
                    });
                }
            }
            let mask = if m.bits == 64 {
                u64::MAX
            } else {
                (1u64 << m.bits) - 1
            };
            if m.init & !mask != 0 {
                return Err(LogicSpecError::InitValueTooWide {
                    register: m.name.clone(),
                    value: m.init,
                    bits: m.bits,
                });
            }
            if let Some(protection) = &m.software_data_protection {
                if m.write.is_none() {
                    return Err(LogicSpecError::MemoryProtectionWithoutWrite {
                        name: m.name.clone(),
                    });
                }
                for (sequence, commands) in [
                    ("enable", protection.enable.as_slice()),
                    ("disable", protection.disable.as_slice()),
                ] {
                    if commands.is_empty() {
                        return Err(LogicSpecError::MemoryCommandSequenceEmpty {
                            name: m.name.clone(),
                            sequence,
                        });
                    }
                    for command in commands {
                        if command.address >= m.words {
                            return Err(LogicSpecError::MemoryCommandAddressOutOfRange {
                                name: m.name.clone(),
                                sequence,
                                address: command.address,
                                words: m.words,
                            });
                        }
                        if command.value & !mask != 0 {
                            return Err(LogicSpecError::MemoryCommandValueTooWide {
                                name: m.name.clone(),
                                sequence,
                                value: command.value,
                                bits: m.bits,
                            });
                        }
                    }
                }
            }
        }

        // ── Comb ──
        // Every comb target must be a declared output; every output must be
        // assigned exactly once, by comb or by a memory's data_out.
        for name in self.comb.keys() {
            if !outputs.contains(name.as_str()) {
                return Err(LogicSpecError::CombTargetNotOutput { name: name.clone() });
            }
            if memory_outputs.contains(name.as_str()) {
                return Err(LogicSpecError::MemoryOutputAlsoComb { name: name.clone() });
            }
        }
        for name in &self.outputs {
            if !self.comb.contains_key(name) && !memory_outputs.contains(name.as_str()) {
                return Err(LogicSpecError::UnassignedOutput { name: name.clone() });
            }
        }

        let clock_pins: HashMap<&str, &str> = self
            .registers
            .iter()
            .filter_map(|r| r.clock.as_ref().map(|c| (c.pin.as_str(), r.name.as_str())))
            .collect();

        let memory_names: HashSet<&str> = self.memories.iter().map(|m| m.name.as_str()).collect();

        // Parse in outputs-declaration order; the evaluation order contract.
        // Memory-driven outputs have no comb expression to parse.
        let mut comb: Vec<(String, LogicExpr)> = Vec::with_capacity(self.outputs.len());
        for out in &self.outputs {
            let Some(src) = self.comb.get(out) else {
                continue;
            };
            let expr = parse_logic_expr(src).map_err(|source| LogicSpecError::ExprParse {
                output: out.clone(),
                source,
            })?;
            let mut names = Vec::new();
            let mut bit_refs = Vec::new();
            expr.collect_refs(&mut names, &mut bit_refs);
            let context = format!("comb expression for '{out}'");
            for n in names {
                if let Some(&bits) = reg_bits.get(n) {
                    return Err(LogicSpecError::RegisterAsScalar {
                        context: context.clone(),
                        register: n.to_string(),
                        bits,
                    });
                }
                if memory_names.contains(n) {
                    return Err(LogicSpecError::MemoryInComb {
                        context: context.clone(),
                        name: n.to_string(),
                    });
                }
                if !inputs.contains(n) && !outputs.contains(n) {
                    return Err(LogicSpecError::UndeclaredName {
                        context: context.clone(),
                        name: n.to_string(),
                    });
                }
                if let Some(reg) = clock_pins.get(n) {
                    return Err(LogicSpecError::ClockAlsoComb {
                        pin: n.to_string(),
                        register: reg.to_string(),
                        output: out.clone(),
                    });
                }
            }
            for (n, i) in bit_refs {
                match reg_bits.get(n) {
                    Some(&bits) if i < bits => {}
                    Some(&bits) => {
                        return Err(LogicSpecError::BitIndexOutOfRange {
                            context: context.clone(),
                            register: n.to_string(),
                            index: i,
                            bits,
                        });
                    }
                    None => {
                        return Err(LogicSpecError::BitRefNotRegister {
                            context: context.clone(),
                            name: n.to_string(),
                            index: i,
                        });
                    }
                }
            }
            comb.push((out.clone(), expr));
        }

        // ── Tristate ──
        for (group, ts) in &self.tristate {
            if !inputs.contains(ts.enable.as_str()) {
                return Err(LogicSpecError::TristateEnableUndeclared {
                    group: group.clone(),
                    enable: ts.enable.clone(),
                });
            }
            self.expand_tristate_group(group)?;
        }

        // ── Init ──
        for (name, &v) in &self.init {
            if !self.comb.contains_key(name) {
                return Err(LogicSpecError::InitNotCombOutput { name: name.clone() });
            }
            if v > 1 {
                return Err(LogicSpecError::InitValueInvalid {
                    name: name.clone(),
                    value: v,
                });
            }
        }

        // ── Feature gate (last: real structural defects surface first) ──
        // An engine that has not declared memory support must refuse the spec
        // rather than bind a part whose bus it will never drive.
        if let Some(m) = self.memories.first() {
            if !features.contains(&Self::FEATURE_MEMORY) {
                return Err(LogicSpecError::MemoryNotSupported {
                    name: m.name.clone(),
                });
            }
        }

        // ── Cycle detection (warning, not an error: cross-coupled latches are
        //    the point). The engine verifies convergence at bind time. ──
        let (warnings, cyclic_outputs) = self.comb_cycles(&comb);

        Ok(ValidatedLogic {
            comb,
            warnings,
            cyclic_outputs,
            memories: self.memories.clone(),
        })
    }

    /// Expand one tristate group key (`"qa"` or `"qa..qh"`) into output names
    /// over the outputs-declaration order.
    pub fn expand_tristate_group(&self, group: &str) -> Result<Vec<String>, LogicSpecError> {
        if let Some((a, b)) = group.split_once("..") {
            let ia = self.outputs.iter().position(|o| o == a);
            let ib = self.outputs.iter().position(|o| o == b);
            match (ia, ib) {
                (Some(ia), Some(ib)) if ia <= ib => Ok(self.outputs[ia..=ib].to_vec()),
                _ => Err(LogicSpecError::TristateRangeInvalid {
                    group: group.to_string(),
                }),
            }
        } else if self.outputs.iter().any(|o| o == group) {
            Ok(vec![group.to_string()])
        } else {
            Err(LogicSpecError::TristateUnknownOutput {
                group: group.to_string(),
                name: group.to_string(),
            })
        }
    }

    /// Find combinational cycles: outputs whose expressions (transitively)
    /// reference themselves through other outputs. Returns human-readable
    /// warnings and the set of cyclic outputs.
    fn comb_cycles(&self, comb: &[(String, LogicExpr)]) -> (Vec<String>, HashSet<String>) {
        // out -> outputs it references directly.
        let mut deps: HashMap<&str, Vec<&str>> = HashMap::new();
        let out_set: HashSet<&str> = self.outputs.iter().map(|s| s.as_str()).collect();
        for (out, expr) in comb {
            let mut names = Vec::new();
            let mut bits = Vec::new();
            expr.collect_refs(&mut names, &mut bits);
            deps.insert(
                out.as_str(),
                names.into_iter().filter(|n| out_set.contains(n)).collect(),
            );
        }
        // An output is cyclic if it can reach itself.
        let mut cyclic: HashSet<String> = HashSet::new();
        for start in self.outputs.iter().map(|s| s.as_str()) {
            let mut stack: Vec<&str> = deps.get(start).cloned().unwrap_or_default();
            let mut visited: HashSet<&str> = HashSet::new();
            while let Some(n) = stack.pop() {
                if n == start {
                    cyclic.insert(start.to_string());
                    break;
                }
                if visited.insert(n) {
                    stack.extend(deps.get(n).cloned().unwrap_or_default());
                }
            }
        }
        let mut warnings = Vec::new();
        if !cyclic.is_empty() {
            let mut members: Vec<&String> = cyclic.iter().collect();
            members.sort();
            warnings.push(format!(
                "combinational cycle through {:?}: resolved by bounded fixpoint iteration in \
                 outputs-declaration order; convergence is verified exhaustively at bind time",
                members
            ));
        }
        (warnings, cyclic)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_logic(toml_src: &str) -> Logic {
        toml::from_str(toml_src).expect("logic TOML parses")
    }

    const HC595: &str = r#"
inputs  = ["ser", "srclk", "rclk", "srclr_n", "oe_n"]
outputs = ["qa", "qb", "qc", "qd", "qe", "qf", "qg", "qh", "qh_serial"]

[[register]]
name = "shift"
bits = 8
clock = { pin = "srclk", edge = "rising" }
reset = { pin = "srclr_n", active = "low", value = 0 }
op = "shift_left"
data_in = "ser"

[[register]]
name = "store"
bits = 8
clock = { pin = "rclk", edge = "rising" }
op = "load"
data_in = "shift"

[comb]
"qa" = "store[0]"
"qb" = "store[1]"
"qc" = "store[2]"
"qd" = "store[3]"
"qe" = "store[4]"
"qf" = "store[5]"
"qg" = "store[6]"
"qh" = "store[7]"
"qh_serial" = "shift[7]"

[tristate]
"qa..qh" = { enable = "oe_n", active = "low" }
"#;

    const NOR_LATCH: &str = r#"
inputs  = ["set", "reset"]
outputs = ["q", "qb"]

[comb]
"q"  = "!(set | qb)"
"qb" = "!(reset | q)"

[init]
"q" = 1
"qb" = 0
"#;

    #[test]
    fn hc595_spec_validates() {
        let logic = parse_logic(HC595);
        let v = logic.validate().expect("595 spec is valid");
        assert!(v.warnings.is_empty(), "no cycles in the 595");
        assert_eq!(v.comb.len(), 9, "one parsed expr per output");
        // Evaluation order is outputs-declaration order.
        assert_eq!(v.comb[0].0, "qa");
        assert_eq!(v.comb[8].0, "qh_serial");
        // The tristate range expands over declaration order.
        assert_eq!(
            logic.expand_tristate_group("qa..qh").unwrap(),
            vec!["qa", "qb", "qc", "qd", "qe", "qf", "qg", "qh"]
        );
    }

    #[test]
    fn nor_latch_cycle_is_a_warning_not_an_error() {
        let logic = parse_logic(NOR_LATCH);
        let v = logic.validate().expect("latch spec is valid");
        assert_eq!(v.warnings.len(), 1, "cycle warning raised");
        assert!(v.cyclic_outputs.contains("q"));
        assert!(v.cyclic_outputs.contains("qb"));
    }

    #[test]
    fn expression_grammar_parses_gate_shapes() {
        // 74HC02-style digit-led pin names and NOR.
        let e = parse_logic_expr("!(1a | 1b)").unwrap();
        assert_eq!(
            e,
            LogicExpr::Not(Box::new(LogicExpr::Or(
                Box::new(LogicExpr::Name("1a".into())),
                Box::new(LogicExpr::Name("1b".into()))
            )))
        );
        // Precedence: ! > & > ^ > |.
        let e = parse_logic_expr("a & b ^ c | d").unwrap();
        assert_eq!(
            e,
            LogicExpr::Or(
                Box::new(LogicExpr::Xor(
                    Box::new(LogicExpr::And(
                        Box::new(LogicExpr::Name("a".into())),
                        Box::new(LogicExpr::Name("b".into()))
                    )),
                    Box::new(LogicExpr::Name("c".into()))
                )),
                Box::new(LogicExpr::Name("d".into()))
            )
        );
        // && / || aliases, literals, bit refs.
        assert_eq!(
            parse_logic_expr("x && 1 || y[3]").unwrap(),
            LogicExpr::Or(
                Box::new(LogicExpr::And(
                    Box::new(LogicExpr::Name("x".into())),
                    Box::new(LogicExpr::Const(true))
                )),
                Box::new(LogicExpr::Bit("y".into(), 3))
            )
        );
    }

    #[test]
    fn rejects_undeclared_name_in_comb() {
        let mut logic = parse_logic(NOR_LATCH);
        logic
            .comb
            .insert("q".into(), "!(set | qb | phantom)".into());
        let e = logic.validate().unwrap_err();
        assert!(
            matches!(e, LogicSpecError::UndeclaredName { ref name, .. } if name == "phantom"),
            "got: {e}"
        );
    }

    #[test]
    fn rejects_register_used_as_scalar() {
        let mut logic = parse_logic(HC595);
        logic.comb.insert("qa".into(), "store".into());
        let e = logic.validate().unwrap_err();
        assert!(
            matches!(e, LogicSpecError::RegisterAsScalar { ref register, .. } if register == "store"),
            "got: {e}"
        );
    }

    #[test]
    fn rejects_bit_index_out_of_range() {
        let mut logic = parse_logic(HC595);
        logic.comb.insert("qa".into(), "store[8]".into());
        let e = logic.validate().unwrap_err();
        assert!(
            matches!(
                e,
                LogicSpecError::BitIndexOutOfRange {
                    index: 8,
                    bits: 8,
                    ..
                }
            ),
            "got: {e}"
        );
    }

    #[test]
    fn rejects_clock_pin_in_comb() {
        let mut logic = parse_logic(HC595);
        logic
            .comb
            .insert("qh_serial".into(), "srclk & shift[7]".into());
        let e = logic.validate().unwrap_err();
        assert!(
            matches!(e, LogicSpecError::ClockAlsoComb { ref pin, .. } if pin == "srclk"),
            "got: {e}"
        );
    }

    #[test]
    fn rejects_unassigned_output() {
        let mut logic = parse_logic(NOR_LATCH);
        logic.outputs.push("q_extra".into());
        let e = logic.validate().unwrap_err();
        assert!(
            matches!(e, LogicSpecError::UnassignedOutput { ref name } if name == "q_extra"),
            "got: {e}"
        );
    }

    #[test]
    fn rejects_tristate_without_declared_enable() {
        let mut logic = parse_logic(HC595);
        logic.tristate.insert(
            "qa".into(),
            TristateSpec {
                enable: "nonexistent_oe".into(),
                active: Level::Low,
            },
        );
        let e = logic.validate().unwrap_err();
        assert!(
            matches!(e, LogicSpecError::TristateEnableUndeclared { ref enable, .. } if enable == "nonexistent_oe"),
            "got: {e}"
        );
    }

    #[test]
    fn rejects_reversed_tristate_range() {
        let mut logic = parse_logic(HC595);
        logic.tristate.insert(
            "qh..qa".into(),
            TristateSpec {
                enable: "oe_n".into(),
                active: Level::Low,
            },
        );
        let e = logic.validate().unwrap_err();
        assert!(
            matches!(e, LogicSpecError::TristateRangeInvalid { .. }),
            "got: {e}"
        );
    }

    #[test]
    fn rejects_register_wider_than_u64() {
        let mut logic = parse_logic(HC595);
        logic.registers[0].bits = 65;
        let e = logic.validate().unwrap_err();
        assert!(
            matches!(e, LogicSpecError::RegisterTooWide { bits: 65, .. }),
            "got: {e}"
        );
    }

    #[test]
    fn rejects_load_width_mismatch() {
        let toml_src = r#"
inputs  = ["pl_n", "clk", "a", "b"]
outputs = ["qh"]
[[register]]
name = "reg"
bits = 8
clock = { pin = "clk", edge = "rising" }
op = "shift_left"
data_in = "a"
load = { pin = "pl_n", active = "low", data = ["a", "b"] }
[comb]
"qh" = "reg[7]"
"#;
        let e = parse_logic(toml_src).validate().unwrap_err();
        assert!(
            matches!(
                e,
                LogicSpecError::LoadWidthMismatch {
                    bits: 8,
                    got: 2,
                    ..
                }
            ),
            "got: {e}"
        );
    }

    #[test]
    fn rejects_register_load_width_mismatch() {
        let mut logic = parse_logic(HC595);
        logic.registers[1].bits = 4;
        // store[4..7] comb refs now also out of range; trim them so the
        // data_in width check is what fires.
        for k in ["qe", "qf", "qg", "qh"] {
            logic.comb.insert(k.into(), "store[0]".into());
        }
        let e = logic.validate().unwrap_err();
        assert!(
            matches!(
                e,
                LogicSpecError::DataInWidthMismatch {
                    bits: 4,
                    from_bits: 8,
                    ..
                }
            ),
            "got: {e}"
        );
    }

    #[test]
    fn rejects_dead_register() {
        let toml_src = r#"
inputs  = ["d"]
outputs = ["q"]
[[register]]
name = "ff"
bits = 1
[comb]
"q" = "ff[0]"
"#;
        let e = parse_logic(toml_src).validate().unwrap_err();
        assert!(matches!(e, LogicSpecError::DeadRegister { .. }), "got: {e}");
    }

    #[test]
    fn accepts_74hc74_shape_with_two_resets() {
        let toml_src = r#"
inputs  = ["d", "clk", "pre_n", "clr_n"]
outputs = ["q", "q_n"]
[[register]]
name = "ff"
bits = 1
clock = { pin = "clk", edge = "rising" }
reset = [
  { pin = "clr_n", active = "low", value = 0 },
  { pin = "pre_n", active = "low", value = 1 },
]
op = "load"
data_in = "d"
[comb]
"q" = "ff[0]"
"q_n" = "!ff[0]"
"#;
        let logic = parse_logic(toml_src);
        assert_eq!(
            logic.registers[0].resets.len(),
            2,
            "both async controls parsed"
        );
        logic.validate().expect("74HC74 shape validates");
    }

    #[test]
    fn duplicate_names_rejected_across_kinds() {
        let mut logic = parse_logic(HC595);
        logic.inputs.push("store".into());
        let e = logic.validate().unwrap_err();
        assert!(
            matches!(e, LogicSpecError::DuplicateName { .. }),
            "got: {e}"
        );
    }

    /// A 28C256-shaped parallel EEPROM: 32K x 8, bidirectional bus expressed
    /// as tri-stated outputs sampled by data_in, CE-gated WE-rising write.
    const EEPROM_28C256: &str = r#"
inputs  = ["a0", "a1", "a2", "a3", "a4", "a5", "a6", "a7", "a8", "a9",
           "a10", "a11", "a12", "a13", "a14", "ce_n", "oe_n", "we_n"]
outputs = ["io0", "io1", "io2", "io3", "io4", "io5", "io6", "io7"]

[[memory]]
name = "cell"
words = 32768
bits = 8
page_words = 64
byte_load_timeout_s = 0.00015
init = 0xFF
address  = ["a0", "a1", "a2", "a3", "a4", "a5", "a6", "a7", "a8", "a9",
            "a10", "a11", "a12", "a13", "a14"]
write = { pin = "we_n", edge = "rising" }
write_gates = [{ pin = "ce_n", active = "low" }]
read_gates = [
  { pin = "ce_n", active = "low" },
  { pin = "oe_n", active = "low" },
  { pin = "we_n", active = "high" },
]
data_in  = ["io0", "io1", "io2", "io3", "io4", "io5", "io6", "io7"]
data_out = ["io0", "io1", "io2", "io3", "io4", "io5", "io6", "io7"]
[memory.software_data_protection]
initial = false
enable = [
    { address = 0x5555, value = 0xAA },
    { address = 0x2AAA, value = 0x55 },
    { address = 0x5555, value = 0xA0 },
]
disable = [
    { address = 0x5555, value = 0xAA },
    { address = 0x2AAA, value = 0x55 },
    { address = 0x5555, value = 0x80 },
    { address = 0x5555, value = 0xAA },
    { address = 0x2AAA, value = 0x55 },
    { address = 0x5555, value = 0x20 },
]
"#;

    #[test]
    fn memory_spec_validates_only_under_the_memory_feature() {
        let logic = parse_logic(EEPROM_28C256);
        // Plain validate (an engine that never declared memory support) must
        // refuse loudly, never bind a part whose bus it will not drive.
        let e = logic.validate().unwrap_err();
        assert!(
            matches!(e, LogicSpecError::MemoryNotSupported { ref name } if name == "cell"),
            "got: {e}"
        );
        // A memory-capable engine passes the feature and gets the arrays.
        let v = logic
            .validate_with_features(&[Logic::FEATURE_MEMORY])
            .expect("28C256 shape validates under the memory feature");
        assert_eq!(v.memories.len(), 1);
        assert_eq!(v.memories[0].words, 32768);
        assert_eq!(v.memories[0].data_out.len(), 8);
        let sdp = v.memories[0]
            .software_data_protection
            .as_ref()
            .expect("28C256 SDP declaration survives validation");
        assert_eq!(sdp.enable.len(), 3);
        assert_eq!(sdp.disable.len(), 6);
        // The bus outputs are memory-driven, so no comb ASTs exist for them.
        assert!(v.comb.is_empty());
    }

    #[test]
    fn software_data_protection_rejects_out_of_range_commands() {
        let mut logic = parse_logic(EEPROM_28C256);
        logic.memories[0]
            .software_data_protection
            .as_mut()
            .unwrap()
            .enable[0]
            .address = 32768;
        let e = logic
            .validate_with_features(&[Logic::FEATURE_MEMORY])
            .unwrap_err();
        assert!(
            matches!(e, LogicSpecError::MemoryCommandAddressOutOfRange {
                ref name,
                sequence: "enable",
                address: 32768,
                words: 32768,
            } if name == "cell"),
            "got: {e}"
        );
    }

    #[test]
    fn software_data_protection_rejects_values_wider_than_the_bus() {
        let mut logic = parse_logic(EEPROM_28C256);
        logic.memories[0]
            .software_data_protection
            .as_mut()
            .unwrap()
            .disable[5]
            .value = 0x120;
        let e = logic
            .validate_with_features(&[Logic::FEATURE_MEMORY])
            .unwrap_err();
        assert!(
            matches!(e, LogicSpecError::MemoryCommandValueTooWide {
                ref name,
                sequence: "disable",
                value: 0x120,
                bits: 8,
            } if name == "cell"),
            "got: {e}"
        );
    }

    #[test]
    fn software_data_protection_requires_a_writable_memory_and_both_sequences() {
        let mut rom = parse_logic(EEPROM_28C256);
        rom.memories[0].write = None;
        rom.memories[0].data_in.clear();
        let e = rom
            .validate_with_features(&[Logic::FEATURE_MEMORY])
            .unwrap_err();
        assert!(
            matches!(e, LogicSpecError::MemoryProtectionWithoutWrite { ref name } if name == "cell"),
            "got: {e}"
        );

        let mut empty = parse_logic(EEPROM_28C256);
        empty.memories[0]
            .software_data_protection
            .as_mut()
            .unwrap()
            .disable
            .clear();
        let e = empty
            .validate_with_features(&[Logic::FEATURE_MEMORY])
            .unwrap_err();
        assert!(
            matches!(e, LogicSpecError::MemoryCommandSequenceEmpty {
                ref name,
                sequence: "disable",
            } if name == "cell"),
            "got: {e}"
        );
    }

    #[test]
    fn memory_page_size_must_be_an_aligned_power_of_two() {
        for page_words in [0, 63, 65536] {
            let mut logic = parse_logic(EEPROM_28C256);
            logic.memories[0].page_words = Some(page_words);
            let e = logic
                .validate_with_features(&[Logic::FEATURE_MEMORY])
                .unwrap_err();
            assert!(
                matches!(e, LogicSpecError::MemoryPageWordsInvalid {
                    ref name,
                    page_words: got,
                    words: 32768,
                } if name == "cell" && got == page_words),
                "page_words={page_words}: got {e}"
            );
        }
    }

    #[test]
    fn memory_page_timing_must_be_positive_and_have_a_page() {
        for seconds in [0.0, -1.0, f64::NAN] {
            let mut logic = parse_logic(EEPROM_28C256);
            logic.memories[0].byte_load_timeout_s = Some(seconds);
            let e = logic
                .validate_with_features(&[Logic::FEATURE_MEMORY])
                .unwrap_err();
            assert!(
                matches!(e, LogicSpecError::MemoryByteLoadTimeoutInvalid { ref name, .. }
                    if name == "cell"),
                "byte_load_timeout_s={seconds:?}: got {e}"
            );
        }

        let mut no_page = parse_logic(EEPROM_28C256);
        no_page.memories[0].page_words = None;
        let e = no_page
            .validate_with_features(&[Logic::FEATURE_MEMORY])
            .unwrap_err();
        assert!(
            matches!(e, LogicSpecError::MemoryPageTimingWithoutPage { ref name }
                if name == "cell"),
            "got: {e}"
        );
    }

    #[test]
    fn memory_structural_defects_beat_the_feature_gate() {
        // A wrong address width errors as itself even WITHOUT the feature, so
        // spec authors debug the real defect, not the gate.
        let mut logic = parse_logic(EEPROM_28C256);
        logic.memories[0].address.pop();
        let e = logic.validate().unwrap_err();
        assert!(
            matches!(
                e,
                LogicSpecError::MemoryAddressWidthMismatch {
                    expected: 15,
                    got: 14,
                    ..
                }
            ),
            "got: {e}"
        );
    }

    #[test]
    fn memory_data_widths_must_match_word_bits() {
        let mut logic = parse_logic(EEPROM_28C256);
        logic.memories[0].data_out.pop();
        let e = logic
            .validate_with_features(&[Logic::FEATURE_MEMORY])
            .unwrap_err();
        assert!(
            matches!(
                e,
                LogicSpecError::MemoryDataWidthMismatch {
                    field: "data_out",
                    bits: 8,
                    got: 7,
                    ..
                }
            ),
            "got: {e}"
        );
    }

    #[test]
    fn memory_read_gates_must_name_declared_inputs() {
        let mut logic = parse_logic(EEPROM_28C256);
        logic.memories[0].read_gates[1].pin = "missing_oe".into();
        let e = logic
            .validate_with_features(&[Logic::FEATURE_MEMORY])
            .unwrap_err();
        assert!(
            matches!(e, LogicSpecError::UndeclaredPin { ref context, ref pin }
                if context == "memory 'cell' read gate" && pin == "missing_oe"),
            "got: {e}"
        );
    }

    #[test]
    fn rom_shape_forbids_data_in_without_write() {
        let mut logic = parse_logic(EEPROM_28C256);
        logic.memories[0].write = None;
        let e = logic
            .validate_with_features(&[Logic::FEATURE_MEMORY])
            .unwrap_err();
        assert!(
            matches!(e, LogicSpecError::MemoryDataInWithoutWrite { .. }),
            "got: {e}"
        );
        // Dropping data_in too makes it a legal ROM.
        let mut rom = parse_logic(EEPROM_28C256);
        rom.memories[0].write = None;
        rom.memories[0].data_in.clear();
        rom.memories[0].software_data_protection = None;
        rom.validate_with_features(&[Logic::FEATURE_MEMORY])
            .expect("27C-style ROM shape validates");
    }

    #[test]
    fn memory_output_cannot_also_be_comb_assigned() {
        let mut logic = parse_logic(EEPROM_28C256);
        logic.comb.insert("io0".into(), "a0".into());
        let e = logic
            .validate_with_features(&[Logic::FEATURE_MEMORY])
            .unwrap_err();
        assert!(
            matches!(e, LogicSpecError::MemoryOutputAlsoComb { ref name } if name == "io0"),
            "got: {e}"
        );
    }

    #[test]
    fn memory_shares_the_one_namespace() {
        let mut logic = parse_logic(EEPROM_28C256);
        logic.memories[0].name = "io0".into();
        let e = logic
            .validate_with_features(&[Logic::FEATURE_MEMORY])
            .unwrap_err();
        assert!(
            matches!(e, LogicSpecError::DuplicateName { .. }),
            "got: {e}"
        );
    }

    #[test]
    fn memory_size_cap_refuses_absurd_allocations() {
        let mut logic = parse_logic(EEPROM_28C256);
        logic.memories[0].words = 1 << 40;
        logic.memories[0].address = (0..40).map(|i| format!("a{i}")).collect();
        logic.inputs = (0..40)
            .map(|i| format!("a{i}"))
            .chain(["ce_n".into(), "oe_n".into(), "we_n".into()])
            .collect();
        let e = logic
            .validate_with_features(&[Logic::FEATURE_MEMORY])
            .unwrap_err();
        assert!(
            matches!(e, LogicSpecError::MemoryTooLarge { .. }),
            "got: {e}"
        );
    }

    #[test]
    fn memory_ignores_do_not_regress_existing_specs() {
        // The HC595 spec has no memory: it must validate identically with and
        // without the feature declared.
        let logic = parse_logic(HC595);
        logic.validate().expect("plain validate unaffected");
        logic
            .validate_with_features(&[Logic::FEATURE_MEMORY])
            .expect("feature declaration is not a requirement");
    }
}
