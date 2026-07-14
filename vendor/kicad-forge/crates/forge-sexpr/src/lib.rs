//! Lossless s-expression concrete syntax tree for KiCad file formats.
//!
//! Every byte of the source is retained: each node stores the trivia
//! (whitespace) that precedes it and tokens keep their raw text, so
//! `parse(text).emit() == text` for any well-formed input. Editing is done on
//! the tree; emitted edits use KiCad-style formatting for new nodes while
//! untouched regions keep their original bytes.

mod parse;
mod print;
mod text;

pub use parse::{parse, ParseError};
pub use text::Text;

/// A node in the CST: either a parenthesised list or a single token.
#[derive(Debug, Clone, PartialEq)]
pub enum Sexpr {
    List(List),
    Token(Token),
}

/// `( child child ... )` with the exact trivia around the parens.
///
/// The `leading`/`close_leading` trivia is a [`Text`]: a zero-copy span into
/// the source for parsed lists, or owned bytes for built/edited ones. `Text`
/// derefs to `&str`, so reads work as before; writes take `impl Into<Text>`
/// (a `String` or `&str` assigns directly via `From`).
#[derive(Debug, Clone, PartialEq)]
pub struct List {
    /// Source text (whitespace) immediately before the `(`.
    pub leading: Text,
    pub children: Vec<Sexpr>,
    /// Source text (whitespace) immediately before the `)`.
    pub close_leading: Text,
}

/// An atom (`net`, `1.6`, `-3.14`) or quoted string (`"Net-(C1-Pad1)"`),
/// stored exactly as it appeared in the source (as a [`Text`] span when parsed).
#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    /// Source text (whitespace) immediately before the token.
    pub leading: Text,
    /// Raw token text, including surrounding quotes for strings.
    pub raw: Text,
}

/// A whole file: top-level nodes (usually one list) plus trailing trivia.
///
/// When produced by [`parse`], the Document owns the source buffer (`src`) that
/// every node's [`Text`] spans borrow from; that buffer must outlive the nodes,
/// which it does because they live inside this Document. Documents built by
/// hand (via `Document::new` or the builders) have no source and all-`Owned`
/// text.
#[derive(Debug, Clone, PartialEq)]
pub struct Document {
    pub nodes: Vec<Sexpr>,
    /// Trivia after the final node (typically `"\n"`).
    pub trailing: Text,
    /// The owned source buffer that parsed nodes' spans point into. `None` for
    /// hand-built documents. Keep this alive as long as any borrowed span is.
    #[doc(hidden)]
    pub src: Option<std::sync::Arc<str>>,
}

impl Token {
    /// New bare atom. The caller must ensure `text` needs no quoting.
    pub fn atom(text: impl Into<Text>) -> Self {
        Token { leading: Text::empty(), raw: text.into() }
    }

    /// New string token, quoted and escaped per KiCad rules.
    pub fn string(text: &str) -> Self {
        Token { leading: Text::empty(), raw: Text::from(quote(text)) }
    }

    /// New token holding a value: quoted only if KiCad would require it.
    pub fn value_token(text: &str) -> Self {
        if needs_quoting(text) { Self::string(text) } else { Self::atom(text) }
    }

    pub fn is_string(&self) -> bool {
        self.raw.starts_with('"')
    }

    /// The decoded value: unquoted and unescaped for strings, raw for atoms.
    pub fn value(&self) -> String {
        let raw = self.raw.as_str();
        if !raw.starts_with('"') {
            return raw.to_string();
        }
        let inner = &raw[1..raw.len().saturating_sub(1)];
        unescape(inner)
    }

    /// Parse the token as a number (atoms and quoted numbers both work).
    ///
    /// Rust's `f64::from_str` accepts `"nan"`, `"inf"`, `"infinity"` (any case,
    /// optionally signed) as valid floats. KiCad coordinates are never any of
    /// those, and a non-finite value would propagate silently through geometry
    /// (e.g. every DRC clearance comparison against NaN is false), so a
    /// non-finite parse is treated as no value — callers' `unwrap_or` fallbacks
    /// then behave as intended.
    pub fn as_f64(&self) -> Option<f64> {
        self.value().parse::<f64>().ok().filter(|v| v.is_finite())
    }

    pub fn as_i64(&self) -> Option<i64> {
        self.value().parse().ok()
    }
}

impl List {
    pub fn new(children: Vec<Sexpr>) -> Self {
        List { leading: Text::empty(), children, close_leading: Text::empty() }
    }

    /// The list's keyword: its first child, when that child is an atom.
    pub fn name(&self) -> Option<&str> {
        match self.children.first() {
            Some(Sexpr::Token(t)) if !t.is_string() => Some(t.raw.as_str()),
            _ => None,
        }
    }

    /// First child list named `name` (searching after the keyword).
    pub fn find(&self, name: &str) -> Option<&List> {
        self.lists().find(|l| l.name() == Some(name))
    }

    pub fn find_mut(&mut self, name: &str) -> Option<&mut List> {
        self.children.iter_mut().skip(1).find_map(|c| match c {
            Sexpr::List(l) if l.name() == Some(name) => Some(l),
            _ => None,
        })
    }

    /// All child lists named `name`.
    pub fn find_all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a List> + 'a {
        self.lists().filter(move |l| l.name() == Some(name))
    }

    /// All child lists (skipping the keyword position is unnecessary: it is a token).
    pub fn lists(&self) -> impl Iterator<Item = &List> {
        self.children.iter().filter_map(|c| match c {
            Sexpr::List(l) => Some(l),
            _ => None,
        })
    }

    /// Positional argument `i` (0 = first node after the keyword) as a token.
    pub fn arg(&self, i: usize) -> Option<&Token> {
        match self.children.get(i + 1) {
            Some(Sexpr::Token(t)) => Some(t),
            _ => None,
        }
    }

    /// Decoded value of positional argument `i`.
    pub fn arg_value(&self, i: usize) -> Option<String> {
        self.arg(i).map(|t| t.value())
    }

    pub fn arg_f64(&self, i: usize) -> Option<f64> {
        self.arg(i).and_then(|t| t.as_f64())
    }

    pub fn arg_i64(&self, i: usize) -> Option<i64> {
        self.arg(i).and_then(|t| t.as_i64())
    }

    /// Value of the single argument of child list `name`:
    /// `(thickness 1.6)` → `find_value("thickness") == Some("1.6")`.
    pub fn find_value(&self, name: &str) -> Option<String> {
        self.find(name).and_then(|l| l.arg_value(0))
    }

    pub fn find_f64(&self, name: &str) -> Option<f64> {
        self.find(name).and_then(|l| l.arg_f64(0))
    }

    pub fn find_i64(&self, name: &str) -> Option<i64> {
        self.find(name).and_then(|l| l.arg_i64(0))
    }

    /// True if a bare-atom flag child like `locked` is present, or a KiCad 7+
    /// style `(locked yes)` child list says yes.
    pub fn has_flag(&self, name: &str) -> bool {
        let bare = self.children.iter().skip(1).any(|c| match c {
            Sexpr::Token(t) => !t.is_string() && t.raw == name,
            _ => false,
        });
        bare || matches!(self.find_value(name).as_deref(), Some("yes") | Some("true"))
    }

    pub fn push(&mut self, node: Sexpr) {
        self.children.push(node);
    }

    /// Remove all child lists named `name`; returns how many were removed.
    pub fn remove_all(&mut self, name: &str) -> usize {
        let before = self.children.len();
        self.children.retain(|c| !matches!(c, Sexpr::List(l) if l.name() == Some(name)));
        before - self.children.len()
    }
}

impl Sexpr {
    pub fn as_list(&self) -> Option<&List> {
        match self {
            Sexpr::List(l) => Some(l),
            _ => None,
        }
    }

    pub fn as_token(&self) -> Option<&Token> {
        match self {
            Sexpr::Token(t) => Some(t),
            _ => None,
        }
    }

    /// Build `(name arg arg ...)` from tokens/lists, no trivia (printer adds it).
    pub fn list(name: &str, args: Vec<Sexpr>) -> Sexpr {
        let mut children = vec![Sexpr::Token(Token::atom(name))];
        children.extend(args);
        Sexpr::List(List::new(children))
    }

    pub fn atom(text: impl Into<Text>) -> Sexpr {
        Sexpr::Token(Token::atom(text))
    }

    pub fn string(text: &str) -> Sexpr {
        Sexpr::Token(Token::string(text))
    }
}

impl Document {
    /// A hand-built document (no borrowed source; all text is owned).
    pub fn new(nodes: Vec<Sexpr>, trailing: impl Into<Text>) -> Document {
        Document { nodes, trailing: trailing.into(), src: None }
    }

    /// The root list (e.g. the `kicad_pcb` node).
    pub fn root(&self) -> Option<&List> {
        self.nodes.iter().find_map(|n| n.as_list())
    }

    pub fn root_mut(&mut self) -> Option<&mut List> {
        self.nodes.iter_mut().find_map(|n| match n {
            Sexpr::List(l) => Some(l),
            _ => None,
        })
    }

    /// Emit the document. Byte-identical to the source if unmodified.
    pub fn emit(&self) -> String {
        print::emit_document(self)
    }

    /// Emit with KiCad-style formatting regardless of stored trivia.
    pub fn emit_pretty(&self) -> String {
        print::pretty_document(self)
    }
}

/// KiCad requires quoting when the text is empty or contains any of these.
pub fn needs_quoting(text: &str) -> bool {
    text.is_empty()
        || text
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, '(' | ')' | '"' | '{' | '}' | '%' | '#'))
}

/// Quote and escape per KiCad string rules.
pub fn quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_finite_floats_are_not_accepted_as_numbers() {
        // "nan"/"inf" parse as valid f64 in std, but a coordinate is never one
        // and NaN would silently defeat downstream geometry checks.
        for token in ["nan", "NaN", "inf", "-inf", "infinity", "Infinity"] {
            assert_eq!(Token::atom(token).as_f64(), None, "{token} must not parse as a number");
        }
    }

    #[test]
    fn ordinary_floats_still_parse() {
        assert_eq!(Token::atom("1.5").as_f64(), Some(1.5));
        assert_eq!(Token::atom("-2").as_f64(), Some(-2.0));
        assert_eq!(Token::atom("0").as_f64(), Some(0.0));
        assert_eq!(Token::atom("42").as_i64(), Some(42));
    }
}

fn unescape(inner: &str) -> String {
    if !inner.contains('\\') {
        return inner.to_string();
    }
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            // Unknown escape: keep both characters verbatim.
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}
