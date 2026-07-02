//! Byte-level lossless parser. Whitespace between tokens is attached to the
//! following token (or to the closing paren / end of file), so emission can
//! reproduce the source exactly.
//!
//! The source is copied once into an `Arc<str>` owned by the resulting
//! [`Document`]; every token's raw text and every trivia run is a borrowed
//! [`Text::Span`] into that buffer (see `text.rs`). Parsing therefore allocates
//! the source once plus the child `Vec`s — no `String`, and no refcount bump,
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

/// `src` is the buffer the spans borrow from; `bytes` is its byte view.
struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
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

    let mut p = Parser { bytes: owned.as_bytes(), pos: 0 };
    let mut nodes = Vec::new();
    loop {
        let trivia_start = p.pos;
        p.skip_trivia();
        if p.pos >= p.bytes.len() {
            let trailing = span(owned, trivia_start, p.pos);
            return Ok(Document { nodes, trailing, src: Some(src) });
        }
        let leading = span(owned, trivia_start, p.pos);
        nodes.push(p.node(owned, leading)?);
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
