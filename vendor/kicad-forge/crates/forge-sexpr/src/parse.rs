//! Byte-level lossless parser. Whitespace between tokens is attached to the
//! following token (or to the closing paren / end of file), so emission can
//! reproduce the source exactly.
//!
//! The source is copied once into an `Arc<str>` owned by the resulting
//! [`Document`]; every token's raw text and every trivia run is a borrowed
//! [`Text::Span`] into that buffer (see `text.rs`). Parsing therefore allocates
//! the source once plus the child `Vec`s, no `String`, and no refcount bump,
//! per token.

use std::sync::Arc;

use crate::{Document, List, Sexpr, Text, Token};

#[derive(Debug)]
pub struct ParseError {
    pub offset: usize,
    pub line: usize,
    pub message: String,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "parse error at line {} (byte {}): {}", self.line, self.offset, self.message)
    }
}

impl std::error::Error for ParseError {}

/// Deepest list nesting `node` will descend before bailing out. `node` recurses
/// once per open paren and each level costs a native stack frame, so an
/// adversarial file of tens of thousands of consecutive `(` would overflow the
/// stack and abort the process (uncatchable) instead of returning an `Err`.
/// Real KiCad structural nesting is well under 100 levels; 1000 is generous
/// headroom while staying far below any stack-overflow threshold.
const MAX_DEPTH: usize = 1000;

/// `src` is the buffer the spans borrow from; `bytes` is its byte view.
struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
    /// Current list-nesting depth inside `node` (0 at top level).
    depth: usize,
}

pub fn parse(text: &str) -> Result<Document, ParseError> {
    // Own the source once. Every span below borrows from *this* buffer, which
    // is moved into the returned Document, so the borrows live as long as the
    // tree. The borrow is re-derived through the Arc (not the caller's `text`).
    let src: Arc<str> = Arc::from(text);
    // `owned` borrows `src`'s heap buffer for the duration of the parse. Spans
    // store only raw pointers into that buffer (not this borrow), and `src` is
    // moved into the returned Document with its buffer untouched, so the
    // pointers stay valid for the tree's whole life. The borrow itself never
    // escapes this function.
    let owned: &str = &src;

    let mut p = Parser { bytes: owned.as_bytes(), pos: 0, depth: 0 };
    let mut nodes = Vec::new();
    loop {
        let trivia_start = p.pos;
        p.skip_trivia();
        if p.pos >= p.bytes.len() {
            let trailing = span(owned, trivia_start, p.pos);
            return Ok(Document { nodes, trailing, src: Some(src) });
        }
        let leading = span(owned, trivia_start, p.pos);
        let node = p.node(owned, leading)?;
        // Every KiCad format is a single root list. A file with a second
        // top-level list is malformed (e.g. two concatenated `(kicad_pcb ...)`
        // blocks); accepting it would silently drop everything after the first
        // via `Document::root`, reconstructing a truncated board with no error.
        if matches!(node, Sexpr::List(_)) && nodes.iter().any(|n| matches!(n, Sexpr::List(_))) {
            return Err(p.error("multiple top-level s-expressions; expected a single root list"));
        }
        nodes.push(node);
    }
}

/// A borrowed view `owned[start..end]`; empty ranges (≈80% of closing-paren
/// trivia) become a cheap owned-empty `Text`, avoiding even a pointer store.
#[inline]
fn span(owned: &str, start: usize, end: usize) -> Text {
    if start == end {
        Text::empty()
    } else {
        Text::view(&owned[start..end])
    }
}

impl<'a> Parser<'a> {
    #[inline]
    fn byte(&self, i: usize) -> u8 {
        self.bytes[i]
    }

    #[inline]
    fn skip_trivia(&mut self) {
        while self.pos < self.bytes.len() {
            match self.byte(self.pos) {
                b' ' | b'\t' | b'\r' | b'\n' => self.pos += 1,
                _ => break,
            }
        }
    }

    fn error(&self, message: impl Into<String>) -> ParseError {
        let upto = self.pos.min(self.bytes.len());
        let line = 1 + self.bytes[..upto].iter().filter(|&&b| b == b'\n').count();
        ParseError { offset: self.pos, line, message: message.into() }
    }

    /// Parse one node. `leading` is the trivia already consumed before it.
    fn node(&mut self, owned: &'a str, leading: Text) -> Result<Sexpr, ParseError> {
        match self.byte(self.pos) {
            b'(' => {
                self.pos += 1;
                self.depth += 1;
                if self.depth > MAX_DEPTH {
                    return Err(self.error("s-expression nesting too deep"));
                }
                let mut children = Vec::new();
                loop {
                    let trivia_start = self.pos;
                    self.skip_trivia();
                    if self.pos >= self.bytes.len() {
                        return Err(self.error("unclosed list"));
                    }
                    if self.byte(self.pos) == b')' {
                        let close_leading = span(owned, trivia_start, self.pos);
                        self.pos += 1;
                        self.depth -= 1;
                        return Ok(Sexpr::List(List { leading, children, close_leading }));
                    }
                    let child_leading = span(owned, trivia_start, self.pos);
                    children.push(self.node(owned, child_leading)?);
                }
            }
            b')' => Err(self.error("unexpected ')'")),
            b'"' => {
                let start = self.pos;
                self.pos += 1;
                while self.pos < self.bytes.len() {
                    match self.byte(self.pos) {
                        b'\\' => self.pos += 2,
                        b'"' => {
                            self.pos += 1;
                            let raw = span(owned, start, self.pos);
                            return Ok(Sexpr::Token(Token { leading, raw }));
                        }
                        _ => self.pos += 1,
                    }
                }
                Err(self.error("unterminated string"))
            }
            _ => {
                let start = self.pos;
                while self.pos < self.bytes.len() {
                    match self.byte(self.pos) {
                        b' ' | b'\t' | b'\r' | b'\n' | b'(' | b')' | b'"' => break,
                        _ => self.pos += 1,
                    }
                }
                let raw = span(owned, start, self.pos);
                Ok(Sexpr::Token(Token { leading, raw }))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deeply_nested_input_errors_instead_of_overflowing_the_stack() {
        // ~50k consecutive open parens used to abort the process with a stack
        // overflow (uncatchable). It must now return a catchable ParseError.
        let mut src = String::from("(kicad_pcb ");
        src.push_str(&"(".repeat(50_000));
        let err = parse(&src).expect_err("deep nesting must be a recoverable error");
        assert!(err.message.contains("too deep"), "unexpected message: {}", err.message);
    }

    #[test]
    fn nesting_within_the_limit_still_parses() {
        let src = format!("{}x{}", "(".repeat(100), ")".repeat(100));
        assert!(parse(&src).is_ok(), "100-deep nesting is well-formed and must parse");
    }

    #[test]
    fn multiple_top_level_lists_are_rejected() {
        // Two concatenated root blocks (botched merge / append) previously kept
        // only the first, silently dropping footprints B and C.
        let src = r#"(kicad_pcb (footprint "A")) (kicad_pcb (footprint "B") (footprint "C"))"#;
        let err = parse(src).expect_err("a second top-level list must be rejected");
        assert!(err.message.contains("multiple top-level"), "unexpected message: {}", err.message);
    }

    #[test]
    fn single_root_list_parses() {
        let src = r#"(kicad_pcb (footprint "A"))"#;
        assert!(parse(src).is_ok(), "a single root list is the normal, valid case");
    }
}
