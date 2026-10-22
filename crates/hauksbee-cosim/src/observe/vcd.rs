//! A Value Change Dump (IEEE 1364) writer: the one place the co-sim spells VCD.
//!
//! Two callers share it: the analog-net [`VcdSink`](crate::peripherals::VcdSink)
//! (nets sampled after every solve) and the bench export (`hauksbee run --vcd`:
//! MCU pins, decoded bus and serial tracks, event markers). Both produce a file
//! gtkwave, Surfer and `PulseView` open directly.
//!
//! Reuse record (see `docs/cosim/OBSERVATIONS.md`): the `vcd` crate (MIT) writes
//! and parses VCD too; it is a dev-dependency here, used as the independent
//! parser the round-trip tests read this writer's output with. The writer
//! itself stays in-tree because it is ~150 lines, shared with the net sink, and
//! the string-valued decoded tracks (`$var string`, a GTKWave/Surfer extension
//! to IEEE 1364) need control over the exact escaping.

use std::fmt::Write as _;

/// A variable handle returned by [`VcdWriter::var`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VarId(usize);

/// What a VCD variable holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VarKind {
    /// A one-bit wire.
    Bit,
    /// A `width`-bit vector wire.
    Vector(u8),
    /// A string (`$var string 1`): decoded protocol text.
    Text,
}

/// One value a variable takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Bit(bool),
    /// Unknown (`x`): before a pin's first edge it is not known whether it was
    /// driven.
    Unknown,
    Vector(u64),
    Text(String),
}

#[derive(Debug, Clone)]
enum Def {
    Scope(String),
    Up,
    Var {
        kind: VarKind,
        name: String,
        code: String,
    },
}

#[derive(Debug, Clone)]
struct Change {
    t_ps: u64,
    seq: usize,
    var: usize,
    value: Value,
}

/// Builds a VCD document: declare scopes and variables, add timestamped
/// changes in any order, then [`VcdWriter::render`].
#[derive(Debug, Clone, Default)]
pub struct VcdWriter {
    defs: Vec<Def>,
    /// Kind of each declared variable, by [`VarId`].
    kinds: Vec<VarKind>,
    codes: Vec<String>,
    changes: Vec<Change>,
    date: Option<String>,
    version: String,
}

/// Map a variable index to a unique VCD identifier code.
///
/// VCD identifier codes are strings of printable ASCII (33..=126); a single
/// char only spans 92 usable glyphs (we exclude `"` and `$`, which delimit VCD
/// tokens), so variables past the 92nd roll over to multi-character codes. This
/// is a bijective base-92 numeral over that alphabet: indices 0..92 are one
/// char, 92..(92+92^2) are two, and so on, every index gets a distinct code,
/// none ever collides on `!`.
#[must_use]
pub fn vcd_code(index: usize) -> String {
    const EXCLUDED: [char; 2] = ['"', '$'];
    let alphabet: Vec<char> = ('!'..='~').filter(|c| !EXCLUDED.contains(c)).collect();
    let k = alphabet.len();
    let mut n = index;
    let mut out = Vec::new();
    loop {
        out.push(alphabet[n % k]);
        n /= k;
        if n == 0 {
            break;
        }
        n -= 1; // bijective: no leading "zero" digit
    }
    out.iter().rev().collect()
}

/// A scope or variable name made safe for a VCD reference: whitespace and the
/// `$` token delimiter become `_`; everything else printable stays, so
/// `PC13:PC13_LED` survives.
#[must_use]
pub fn sanitize_ref(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_graphic() && c != '$' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "_".into()
    } else {
        cleaned
    }
}

/// The restricted alphanumeric-plus-underscore form the analog-net sink has
/// always written (`CLK`, `NET_1`).
#[must_use]
pub fn sanitize_ident(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// A string value made safe for a VCD `s` record: the record ends at the first
/// whitespace, so spaces become `_` (the convention waveform viewers expect).
#[must_use]
pub fn sanitize_text(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_ascii_graphic() {
                c
            } else if c == ' ' {
                '_'
            } else if c.is_ascii() {
                '.'
            } else {
                '?'
            }
        })
        .collect()
}

impl VcdWriter {
    /// A writer stamped with the producing component, e.g. `hauksbee-vcd-sink`.
    #[must_use]
    pub fn new(version: &str) -> Self {
        Self {
            version: version.to_string(),
            ..Self::default()
        }
    }

    /// Stamp the `$date` line (omitted by default so output is reproducible).
    #[must_use]
    pub fn with_date(mut self, date: &str) -> Self {
        self.date = Some(date.to_string());
        self
    }

    /// Open a scope (`$scope module`).
    pub fn begin_scope(&mut self, name: &str) {
        self.defs.push(Def::Scope(sanitize_ref(name)));
    }

    /// Close the innermost scope.
    pub fn end_scope(&mut self) {
        self.defs.push(Def::Up);
    }

    /// Declare a variable in the current scope. `name` is written verbatim
    /// after [`sanitize_ref`].
    pub fn var(&mut self, kind: VarKind, name: &str) -> VarId {
        let index = self.kinds.len();
        let code = vcd_code(index);
        self.defs.push(Def::Var {
            kind,
            name: sanitize_ref(name),
            code: code.clone(),
        });
        self.kinds.push(kind);
        self.codes.push(code);
        VarId(index)
    }

    /// Like [`Self::var`] with the restricted name form of the net sink.
    pub fn var_ident(&mut self, kind: VarKind, name: &str) -> VarId {
        self.var(kind, &sanitize_ident(name))
    }

    /// The identifier code of a variable (for assertions in tests).
    #[must_use]
    pub fn code(&self, id: VarId) -> &str {
        &self.codes[id.0]
    }

    /// Record `value` on `id` at `t_ps` picoseconds. Changes may be added in
    /// any order; [`Self::render`] sorts them (stable, so same-time changes to
    /// one variable keep their insertion order).
    pub fn change(&mut self, t_ps: u64, id: VarId, value: Value) {
        let seq = self.changes.len();
        self.changes.push(Change {
            t_ps,
            seq,
            var: id.0,
            value,
        });
    }

    /// Number of recorded changes.
    #[must_use]
    pub const fn change_count(&self) -> usize {
        self.changes.len()
    }

    fn value_text(&self, kind: VarKind, var: usize, value: &Value) -> String {
        let code = &self.codes[var];
        match (kind, value) {
            (VarKind::Bit, Value::Bit(b)) => format!("{}{code}", u8::from(*b)),
            (VarKind::Bit, _) => format!("x{code}"),
            (VarKind::Vector(w), Value::Vector(v)) => {
                let w = usize::from(w.max(1));
                format!("b{v:0w$b} {code}")
            }
            (VarKind::Vector(_), _) => format!("bx {code}"),
            // A string record's value is one token: an empty string would run
            // into the id code, so a cleared track reads `-`.
            (VarKind::Text, Value::Text(s)) if s.is_empty() => format!("s- {code}"),
            (VarKind::Text, Value::Text(s)) => format!("s{} {code}", sanitize_text(s)),
            (VarKind::Text, _) => format!("s- {code}"),
        }
    }

    /// Render the document. Timestamps are picoseconds (`$timescale 1ps`).
    #[must_use]
    pub fn render(&self) -> String {
        let mut s = String::new();
        if let Some(d) = &self.date {
            let _ = writeln!(s, "$date {d} $end");
        }
        let _ = writeln!(s, "$version {} $end", self.version);
        s.push_str("$timescale 1ps $end\n");
        for def in &self.defs {
            match def {
                Def::Scope(n) => {
                    let _ = writeln!(s, "$scope module {n} $end");
                }
                Def::Up => s.push_str("$upscope $end\n"),
                Def::Var { kind, name, code } => {
                    let _ = match kind {
                        VarKind::Bit => writeln!(s, "$var wire 1 {code} {name} $end"),
                        VarKind::Vector(w) => {
                            writeln!(s, "$var wire {} {code} {name} $end", (*w).max(1))
                        }
                        VarKind::Text => writeln!(s, "$var string 1 {code} {name} $end"),
                    };
                }
            }
        }
        s.push_str("$enddefinitions $end\n");

        let mut ordered: Vec<&Change> = self.changes.iter().collect();
        ordered.sort_by_key(|c| (c.t_ps, c.seq));

        // Initial values: every variable's value at the start, from its change
        // at t=0 if it has one, otherwise unknown.
        s.push_str("#0\n$dumpvars\n");
        let mut at_zero: Vec<Option<&Change>> = vec![None; self.kinds.len()];
        for c in ordered.iter().take_while(|c| c.t_ps == 0) {
            at_zero[c.var] = Some(c);
        }
        for (var, kind) in self.kinds.iter().enumerate() {
            match (at_zero[var], kind) {
                (Some(c), _) => {
                    let _ = writeln!(s, "{}", self.value_text(*kind, var, &c.value));
                }
                (None, VarKind::Text) => {} // a string with no value yet has no record
                (None, _) => {
                    let _ = writeln!(s, "{}", self.value_text(*kind, var, &Value::Unknown));
                }
            }
        }
        s.push_str("$end\n");

        let mut last_t = 0_u64;
        for c in ordered.iter().filter(|c| c.t_ps > 0) {
            if c.t_ps != last_t {
                let _ = writeln!(s, "#{}", c.t_ps);
                last_t = c.t_ps;
            }
            let _ = writeln!(s, "{}", self.value_text(self.kinds[c.var], c.var, &c.value));
        }
        s
    }
}

/// Convert seconds of simulated time to VCD picoseconds, clamped at zero.
#[must_use]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // clamped non-negative; far below 2^63 ps for any run
pub fn seconds_to_ps(t_s: f64) -> u64 {
    if t_s.is_finite() && t_s > 0.0 {
        (t_s * 1e12).round() as u64
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_unique_and_roll_over() {
        let codes: Vec<String> = (0..500).map(vcd_code).collect();
        let unique: std::collections::HashSet<&String> = codes.iter().collect();
        assert_eq!(unique.len(), 500);
        assert_eq!(codes[91].len(), 1);
        assert_eq!(codes[92].len(), 2);
    }

    #[test]
    fn renders_header_dump_and_sorted_changes() {
        let mut w = VcdWriter::new("test");
        w.begin_scope("mcu U1");
        let a = w.var(VarKind::Bit, "PC13:LED");
        let b = w.var(VarKind::Vector(8), "TX_byte");
        let t = w.var(VarKind::Text, "I2C");
        w.end_scope();
        w.change(2_000, a, Value::Bit(true));
        w.change(0, a, Value::Bit(false));
        w.change(1_000, b, Value::Vector(0x48));
        w.change(1_500, t, Value::Text("LM75 write [0x00]".into()));
        let doc = w.render();
        assert!(doc.contains("$scope module mcu_U1 $end"));
        assert!(doc.contains("$var wire 1 ! PC13:LED $end"));
        assert!(doc.contains("$var wire 8 # TX_byte $end"));
        assert!(doc.contains("$var string 1 % I2C $end"));
        let first_change = doc.find("#1000").unwrap();
        let later = doc.find("#2000").unwrap();
        assert!(first_change < later, "changes are time ordered");
        assert!(doc.contains("b01001000 "), "vector is zero padded to width");
        assert!(
            doc.contains("sLM75_write_[0x00] "),
            "spaces in text become _"
        );
    }

    #[test]
    fn seconds_convert_to_picoseconds() {
        assert_eq!(seconds_to_ps(1.0), 1_000_000_000_000);
        assert_eq!(seconds_to_ps(-1.0), 0);
        assert_eq!(seconds_to_ps(f64::NAN), 0);
    }
}
