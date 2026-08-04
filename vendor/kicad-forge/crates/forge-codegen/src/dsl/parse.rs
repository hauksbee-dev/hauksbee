//! Parser: Board-as-Code text -> [`Program`].
//!
//! The grammar is line-oriented and deliberately small. Each line is tokenised
//! into bare words, quoted strings, and bracket lists; structure comes from
//! `fn`/`instance`/`comp` blocks delimited by `{` and `}`. Comments start with
//! `#`. The parser is forgiving about whitespace but strict about structure: a
//! malformed line returns a [`ParseError`] with the line number, so an editor
//! can point at the mistake.

use crate::dsl::model::{
    Block, Comp, Edge, Instance, Outline, Pad, Program, SlotSpec, Space, Stmt,
};

/// A parse error with a 1-based line number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub line: usize,
    pub msg: String,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "line {}: {}", self.line, self.msg)
    }
}

impl std::error::Error for ParseError {}

impl Program {
    /// Parse Board-as-Code text into a [`Program`].
    pub fn parse(text: &str) -> Result<Program, ParseError> {
        Parser::new(text).parse_program()
    }
}

struct Parser<'a> {
    lines: Vec<(usize, &'a str)>,
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(text: &'a str) -> Self {
        let lines = text
            .lines()
            .enumerate()
            .map(|(i, l)| (i + 1, l))
            .filter(|(_, l)| {
                let t = l.trim();
                !t.is_empty() && !t.starts_with('#')
            })
            .collect();
        Parser { lines, pos: 0 }
    }

    fn peek(&self) -> Option<(usize, &'a str)> {
        self.lines.get(self.pos).copied()
    }

    fn next(&mut self) -> Option<(usize, &'a str)> {
        let r = self.lines.get(self.pos).copied();
        if r.is_some() {
            self.pos += 1;
        }
        r
    }

    fn parse_program(&mut self) -> Result<Program, ParseError> {
        let mut version = 20241229;
        let mut blocks = Vec::new();
        let mut body = Vec::new();
        let mut outline: Option<Outline> = None;

        while let Some((ln, line)) = self.peek() {
            let toks = tokenize(line, ln)?;
            match toks.first().map(|s| s.as_str()) {
                Some("board") => {
                    self.next();
                    match toks.get(1).map(|s| s.as_str()) {
                        // `board version N`
                        Some("version") => {
                            version = toks
                                .get(2)
                                .and_then(|s| s.parse().ok())
                                .ok_or_else(|| err(ln, "board version: expected integer"))?;
                        }
                        // `board size W H` - a size constraint anchored at the
                        // origin (a board W mm wide by H mm tall).
                        Some("size") => {
                            let w: f64 = toks
                                .get(2)
                                .and_then(|s| unq(s).parse().ok())
                                .ok_or_else(|| err(ln, "board size: expected width"))?;
                            let h: f64 = toks
                                .get(3)
                                .and_then(|s| unq(s).parse().ok())
                                .ok_or_else(|| err(ln, "board size: expected height"))?;
                            outline = Some(Outline {
                                min_x: 0.0,
                                min_y: 0.0,
                                max_x: w,
                                max_y: h,
                            });
                        }
                        // `board outline X0 Y0 X1 Y1` - an explicit rectangle.
                        Some("outline") => {
                            let v: Vec<f64> = (2..6)
                                .map(|i| toks.get(i).and_then(|s| unq(s).parse().ok()))
                                .collect::<Option<Vec<f64>>>()
                                .ok_or_else(|| {
                                    err(ln, "board outline: expected X0 Y0 X1 Y1")
                                })?;
                            outline = Some(Outline {
                                min_x: v[0].min(v[2]),
                                min_y: v[1].min(v[3]),
                                max_x: v[0].max(v[2]),
                                max_y: v[1].max(v[3]),
                            });
                        }
                        _ => {
                            return Err(err(
                                ln,
                                "expected `board version N`, `board size W H`, or `board outline X0 Y0 X1 Y1`",
                            ))
                        }
                    }
                }
                Some("fn") => {
                    let name = toks
                        .get(1)
                        .map(|s| unq(s))
                        .ok_or_else(|| err(ln, "fn: missing name"))?;
                    if name == "main" {
                        body = self.parse_main()?;
                    } else {
                        blocks.push(self.parse_block(name)?);
                    }
                }
                Some(other) => return Err(err(ln, &format!("unexpected `{other}` at top level"))),
                None => {
                    self.next();
                }
            }
        }

        // Recompute each block's instance count from the body so the field is
        // truthful after a parse (the header carries only a doc comment).
        for b in &mut blocks {
            b.instances = body
                .iter()
                .filter(|s| matches!(s, Stmt::Instance(i) if i.block == b.name))
                .count();
        }

        Ok(Program {
            version,
            blocks,
            body,
            outline,
        })
    }

    /// `fn <name> {` ... `slot ...` ... `}`
    fn parse_block(&mut self, name: String) -> Result<Block, ParseError> {
        let (ln, line) = self.next().unwrap();
        let toks = tokenize(line, ln)?;
        if toks.last().map(|s| s.as_str()) != Some("{") {
            return Err(err(ln, "block header must end with `{`"));
        }
        let mut slots = Vec::new();
        loop {
            let (ln, line) = self.next().ok_or_else(|| err(ln, "unterminated block"))?;
            let toks = tokenize(line, ln)?;
            match toks.first().map(|s| s.as_str()) {
                Some("}") => break,
                Some("slot") => {
                    // slot <i> lib "..." val "..." pads N
                    let lib = kv(&toks, "lib", ln)?;
                    let value = kv(&toks, "val", ln)?;
                    let pad_count = kv(&toks, "pads", ln)?
                        .parse()
                        .map_err(|_| err(ln, "slot pads: expected integer"))?;
                    slots.push(SlotSpec {
                        lib_id: lib,
                        value,
                        pad_count,
                    });
                }
                _ => return Err(err(ln, "expected `slot` or `}` in block")),
            }
        }
        let instances = 0; // recomputed below from body; placeholder
        Ok(Block {
            name,
            slots,
            instances,
        })
    }

    /// `fn main {` ... `}`
    fn parse_main(&mut self) -> Result<Vec<Stmt>, ParseError> {
        let (ln, line) = self.next().unwrap();
        let toks = tokenize(line, ln)?;
        if toks.last().map(|s| s.as_str()) != Some("{") {
            return Err(err(ln, "fn main must end with `{`"));
        }
        let mut body = Vec::new();
        loop {
            let (ln, line) = self.peek().ok_or_else(|| err(ln, "unterminated fn main"))?;
            let toks = tokenize(line, ln)?;
            match toks.first().map(|s| s.as_str()) {
                Some("}") => {
                    self.next();
                    break;
                }
                Some("net") => {
                    self.next();
                    let name = toks
                        .get(1)
                        .map(|s| unq(s))
                        .ok_or_else(|| err(ln, "net: missing name"))?;
                    body.push(Stmt::Net(name));
                }
                Some("space") => {
                    self.next();
                    // space fn <block> <dist>
                    if toks.get(1).map(|s| s.as_str()) != Some("fn") {
                        return Err(err(ln, "expected `space fn <block> <dist>`"));
                    }
                    let block = toks
                        .get(2)
                        .map(|s| unq(s))
                        .ok_or_else(|| err(ln, "space fn: missing block name"))?;
                    let dist = toks
                        .get(3)
                        .and_then(|s| s.parse().ok())
                        .ok_or_else(|| err(ln, "space fn: expected distance"))?;
                    body.push(Stmt::BlockSpace { block, dist });
                }
                Some("pin") => {
                    self.next();
                    // pin <ref> edge <left|right|top|bottom>
                    let reference = toks
                        .get(1)
                        .map(|s| unq(s))
                        .ok_or_else(|| err(ln, "pin: missing reference"))?;
                    if toks.get(2).map(|s| s.as_str()) != Some("edge") {
                        return Err(err(ln, "expected `pin <ref> edge <side>`"));
                    }
                    let edge = toks
                        .get(3)
                        .map(|s| unq(s))
                        .and_then(|s| Edge::parse(&s))
                        .ok_or_else(|| err(ln, "pin edge: expected left|right|top|bottom"))?;
                    body.push(Stmt::Pin { reference, edge });
                }
                Some("lock") => {
                    self.next();
                    // lock <ref>
                    let reference = toks
                        .get(1)
                        .map(|s| unq(s))
                        .ok_or_else(|| err(ln, "lock: missing reference"))?;
                    body.push(Stmt::Lock { reference });
                }
                Some("instance") => {
                    body.push(Stmt::Instance(self.parse_instance()?));
                }
                Some("comp") => {
                    body.push(Stmt::Single(self.parse_comp()?));
                }
                _ => return Err(err(ln, "expected net/space/pin/lock/instance/comp or `}`")),
            }
        }
        Ok(body)
    }

    /// `instance <block> {` ... comps/missing ... `}`
    fn parse_instance(&mut self) -> Result<Instance, ParseError> {
        let (ln, line) = self.next().unwrap();
        let toks = tokenize(line, ln)?;
        let block = toks
            .get(1)
            .map(|s| unq(s))
            .ok_or_else(|| err(ln, "instance: missing block name"))?;
        if toks.last().map(|s| s.as_str()) != Some("{") {
            return Err(err(ln, "instance header must end with `{`"));
        }
        let mut comps = Vec::new();
        loop {
            let (ln2, line2) = self
                .peek()
                .ok_or_else(|| err(ln, "unterminated instance"))?;
            let toks2 = tokenize(line2, ln2)?;
            match toks2.first().map(|s| s.as_str()) {
                Some("}") => {
                    self.next();
                    break;
                }
                Some("comp") => comps.push(Some(self.parse_comp()?)),
                _ => return Err(err(ln2, "expected `comp` or `}` in instance")),
            }
        }
        Ok(Instance { block, comps })
    }

    /// `comp <ref> lib "..." val "..." layer "..." at X Y rot R {` ... pads ... `}`
    fn parse_comp(&mut self) -> Result<Comp, ParseError> {
        let (ln, line) = self.next().unwrap();
        let toks = tokenize(line, ln)?;
        let reference = toks
            .get(1)
            .map(|s| unq(s))
            .ok_or_else(|| err(ln, "comp: missing reference"))?;
        let lib_id = kv(&toks, "lib", ln)?;
        let value = kv(&toks, "val", ln)?;
        let layer = kv(&toks, "layer", ln).unwrap_or_else(|_| "F.Cu".to_string());
        let (ax, ay) = kv_xy(&toks, "at", ln)?;
        let rot = kv(&toks, "rot", ln)
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.0);
        if toks.last().map(|s| s.as_str()) != Some("{") {
            return Err(err(ln, "comp header must end with `{`"));
        }

        let mut space = None;
        let mut pads = Vec::new();
        loop {
            let (ln2, line2) = self.next().ok_or_else(|| err(ln, "unterminated comp"))?;
            let toks2 = tokenize(line2, ln2)?;
            match toks2.first().map(|s| s.as_str()) {
                Some("}") => break,
                Some("space") => {
                    let dist = toks2
                        .get(1)
                        .and_then(|s| s.parse().ok())
                        .ok_or_else(|| err(ln2, "space: expected distance"))?;
                    space = Some(Space { dist });
                }
                Some("pad") => pads.push(parse_pad(&toks2, ln2)?),
                _ => return Err(err(ln2, "expected `pad`, `space` or `}` in comp")),
            }
        }

        Ok(Comp {
            reference,
            lib_id,
            value,
            layer,
            at: (ax, ay),
            rot,
            space,
            pads,
        })
    }
}

/// The closed set of pad kinds the DSL accepts (KiCad pad attributes).
const PAD_KINDS: [&str; 4] = ["smd", "thru_hole", "np_thru_hole", "connect"];

/// The closed set of pad shapes the DSL accepts (KiCad pad shapes; `custom`
/// is excluded because the DSL carries no shape primitives).
const PAD_SHAPES: [&str; 5] = ["rect", "roundrect", "circle", "oval", "trapezoid"];

/// `pad <num> <kind> <shape> at X Y size W H [drill D] layers [..] (net "N" | nonet)`
fn parse_pad(toks: &[String], ln: usize) -> Result<Pad, ParseError> {
    let number = toks
        .get(1)
        .map(|s| unq(s))
        .ok_or_else(|| err(ln, "pad: missing number"))?;
    // `kind` and `shape` are positional, so both are validated against their
    // closed sets here: an omitted shape must fail loudly instead of silently
    // consuming the next token (`at`) as the shape.
    let kind = toks
        .get(2)
        .map(|s| unq(s))
        .ok_or_else(|| err(ln, "pad: missing kind"))?;
    if !PAD_KINDS.contains(&kind.as_str()) {
        return Err(err(
            ln,
            &format!(
                "pad kind: expected {}, got `{kind}`",
                PAD_KINDS.join("|")
            ),
        ));
    }
    let shape = toks
        .get(3)
        .map(|s| unq(s))
        .ok_or_else(|| err(ln, "pad: missing shape"))?;
    if !PAD_SHAPES.contains(&shape.as_str()) {
        return Err(err(
            ln,
            &format!(
                "pad shape: expected {}, got `{shape}`",
                PAD_SHAPES.join("|")
            ),
        ));
    }
    let (ax, ay) = kv_xy(toks, "at", ln)?;
    let (sw, sh) = kv_xy(toks, "size", ln)?;
    let drill = kv(toks, "drill", ln).ok().and_then(|s| s.parse().ok());
    let layers = bracket_list(toks, "layers", ln)?;
    // `nonet` must be a *bare* token (a quoted "nonet" net name carries the
    // sentinel and is treated as a real net, not the unconnected keyword).
    let net = if toks.iter().any(|t| t.as_str() == "nonet") {
        None
    } else {
        Some(kv(toks, "net", ln)?)
    };
    Ok(Pad {
        number,
        kind,
        shape,
        at: (ax, ay),
        size: (sw, sh),
        drill,
        layers,
        net,
    })
}

// ---------------------------------------------------------------------------
// Token helpers
// ---------------------------------------------------------------------------

/// Find the token following the *bare* keyword `key` and return its value
/// (sentinel-stripped). Quoted tokens never match `key`, so a quoted value that
/// happens to read like a keyword cannot be mistaken for the keyword.
fn kv(toks: &[String], key: &str, ln: usize) -> Result<String, ParseError> {
    for i in 0..toks.len() {
        if toks[i] == key {
            return toks
                .get(i + 1)
                .map(|s| unq(s))
                .ok_or_else(|| err(ln, &format!("`{key}` missing value")));
        }
    }
    Err(err(ln, &format!("missing `{key}`")))
}

/// Find `key X Y` and return `(X, Y)` as floats.
fn kv_xy(toks: &[String], key: &str, ln: usize) -> Result<(f64, f64), ParseError> {
    for i in 0..toks.len() {
        if toks[i] == key {
            let x = toks
                .get(i + 1)
                .and_then(|s| unq(s).parse().ok())
                .ok_or_else(|| err(ln, &format!("`{key}` x: expected number")))?;
            let y = toks
                .get(i + 2)
                .and_then(|s| unq(s).parse().ok())
                .ok_or_else(|| err(ln, &format!("`{key}` y: expected number")))?;
            return Ok((x, y));
        }
    }
    Err(err(ln, &format!("missing `{key}`")))
}

/// Parse a `key [ a b c ]` bracket list of tokens (sentinel-stripped).
fn bracket_list(toks: &[String], key: &str, ln: usize) -> Result<Vec<String>, ParseError> {
    let mut i = 0;
    while i < toks.len() && toks[i] != key {
        i += 1;
    }
    if i >= toks.len() {
        return Err(err(ln, &format!("missing `{key}`")));
    }
    if toks.get(i + 1).map(|s| s.as_str()) != Some("[") {
        return Err(err(ln, &format!("`{key}` must be followed by `[`")));
    }
    let mut out = Vec::new();
    let mut j = i + 2;
    while j < toks.len() && toks[j] != "]" {
        out.push(unq(&toks[j]));
        j += 1;
    }
    if j >= toks.len() {
        return Err(err(ln, &format!("unterminated `{key} [`")));
    }
    Ok(out)
}

/// Sentinel byte prepended to *quoted* string tokens so they can never collide
/// with bare structural keywords (`net`, `at`, `size`, ...). The source grammar
/// never contains a literal `\u{1}` (the tokenizer rejects control-class
/// delimiters and a real net could not carry one), so this is collision-proof.
/// Stripped from any token that is consumed as a *value* via [`unq`].
const QUOTED: char = '\u{1}';

/// Strip the quoted-token sentinel, yielding the literal value.
fn unq(s: &str) -> String {
    s.strip_prefix(QUOTED).unwrap_or(s).to_string()
}

/// Tokenise a line into bare words, quoted strings, and bracket delimiters.
/// `[`, `]`, `{`, `}` are always their own tokens. Quoted strings are prefixed
/// with [`QUOTED`] so a quoted value equal to a keyword (`net "net"`,
/// `net "nonet"`) is never mistaken for the keyword.
fn tokenize(line: &str, ln: usize) -> Result<Vec<String>, ParseError> {
    let mut out = Vec::new();
    let mut chars = line.chars().peekable();
    while let Some(&c) = chars.peek() {
        match c {
            ' ' | '\t' | '\r' => {
                chars.next();
            }
            '#' => break, // trailing comment
            '"' => {
                chars.next();
                let mut s = String::new();
                s.push(QUOTED);
                let mut closed = false;
                while let Some(c) = chars.next() {
                    match c {
                        '\\' => {
                            if let Some(n) = chars.next() {
                                s.push(n);
                            }
                        }
                        '"' => {
                            closed = true;
                            break;
                        }
                        other => s.push(other),
                    }
                }
                if !closed {
                    return Err(err(ln, "unterminated string"));
                }
                out.push(s);
            }
            '[' | ']' | '{' | '}' => {
                chars.next();
                out.push(c.to_string());
            }
            _ => {
                let mut s = String::new();
                while let Some(&c) = chars.peek() {
                    if c.is_whitespace()
                        || c == '"'
                        || c == '['
                        || c == ']'
                        || c == '{'
                        || c == '}'
                        || c == '#'
                    {
                        break;
                    }
                    s.push(c);
                    chars.next();
                }
                out.push(s);
            }
        }
    }
    Ok(out)
}

fn err(line: usize, msg: &str) -> ParseError {
    ParseError {
        line,
        msg: msg.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A one-comp program with the given pad line spliced in.
    fn program_with_pad(pad_line: &str) -> String {
        format!(
            r#"board version 20241229

fn main {{
    net "A"
    comp R1 lib "Lib:R_TEST" val "10k" layer "F.Cu" at 100 50 rot 0 {{
        {pad_line}
    }}
}}
"#
        )
    }

    #[test]
    fn invalid_pad_kind_is_rejected_with_valid_values() {
        let code = program_with_pad(
            r#"pad "2" banana lozenge at 0 0 size 1 1 layers [F.Cu] net "A""#,
        );
        let e = Program::parse(&code).unwrap_err();
        assert_eq!(e.line, 6);
        assert_eq!(
            e.msg,
            "pad kind: expected smd|thru_hole|np_thru_hole|connect, got `banana`"
        );
    }

    #[test]
    fn invalid_pad_shape_is_rejected_with_valid_values() {
        let code = program_with_pad(
            r#"pad "2" smd lozenge at 0 0 size 1 1 layers [F.Cu] net "A""#,
        );
        let e = Program::parse(&code).unwrap_err();
        assert_eq!(e.line, 6);
        assert_eq!(
            e.msg,
            "pad shape: expected rect|roundrect|circle|oval|trapezoid, got `lozenge`"
        );
    }

    #[test]
    fn omitted_pad_shape_is_an_error_not_a_slurped_token() {
        // Without validation the shape slot would silently consume `at`.
        let code = program_with_pad(
            r#"pad "1" smd at -0.9375 0 size 0.975 1.4 layers [F.Cu] net "A""#,
        );
        let e = Program::parse(&code).unwrap_err();
        assert_eq!(e.line, 6);
        assert_eq!(
            e.msg,
            "pad shape: expected rect|roundrect|circle|oval|trapezoid, got `at`"
        );
    }

    #[test]
    fn all_valid_pad_kind_and_shape_tokens_parse() {
        for kind in PAD_KINDS {
            for shape in PAD_SHAPES {
                let drill = if kind.ends_with("thru_hole") { "drill 1.0 " } else { "" };
                let code = program_with_pad(&format!(
                    r#"pad "1" {kind} {shape} at 0 0 size 1.7 1.7 {drill}layers [F.Cu] net "A""#
                ));
                let prog = Program::parse(&code)
                    .unwrap_or_else(|e| panic!("`{kind} {shape}` failed: {e}"));
                let pad = &prog.comps().next().unwrap().pads[0];
                assert_eq!(pad.kind, kind);
                assert_eq!(pad.shape, shape);
            }
        }
    }
}
