//! Emission: exact (trivia-preserving) and KiCad-style pretty printing.

use crate::{Document, List, Sexpr};

pub fn emit_document(doc: &Document) -> String {
    let mut out = String::new();
    for node in &doc.nodes {
        emit_node(node, &mut out);
    }
    out.push_str(&doc.trailing);
    out
}

fn emit_node(node: &Sexpr, out: &mut String) {
    match node {
        Sexpr::Token(t) => {
            // Two adjacent bare atoms would merge into one token; that is the
            // only case that genuinely requires an inserted separator (it can
            // only arise for freshly built nodes, parsed tokens carry their
            // original trivia). `)x`, `(x`, and `"a"x` all re-parse correctly.
            if t.leading.is_empty() && atoms_would_merge(out) {
                out.push(' ');
            }
            out.push_str(&t.leading);
            out.push_str(&t.raw);
        }
        Sexpr::List(l) => {
            out.push_str(&l.leading);
            out.push('(');
            for child in &l.children {
                emit_node(child, out);
            }
            out.push_str(&l.close_leading);
            out.push(')');
        }
    }
}

fn atoms_would_merge(out: &str) -> bool {
    matches!(out.as_bytes().last(), Some(b) if !b" \t\r\n()\"".contains(b))
}

/// KiCad-style formatting: lists containing child lists break onto multiple
/// lines with tab indentation; token-only lists stay inline.
pub fn pretty_document(doc: &Document) -> String {
    let mut out = String::new();
    for node in &doc.nodes {
        pretty_node(node, 0, &mut out);
        out.push('\n');
    }
    out
}

fn pretty_node(node: &Sexpr, depth: usize, out: &mut String) {
    match node {
        Sexpr::Token(t) => out.push_str(&t.raw),
        Sexpr::List(l) => pretty_list(l, depth, out),
    }
}

fn pretty_list(l: &List, depth: usize, out: &mut String) {
    let has_sublists = l.children.iter().any(|c| matches!(c, Sexpr::List(_)));
    out.push('(');
    if !has_sublists {
        for (i, child) in l.children.iter().enumerate() {
            if i > 0 {
                out.push(' ');
            }
            pretty_node(child, depth, out);
        }
        out.push(')');
        return;
    }
    // Leading run of tokens (keyword + positional args) stays on the open line.
    let mut iter = l.children.iter().peekable();
    let mut first = true;
    while let Some(Sexpr::Token(t)) = iter.peek() {
        if !first {
            out.push(' ');
        }
        out.push_str(&t.raw);
        first = false;
        iter.next();
    }
    for child in iter {
        out.push('\n');
        for _ in 0..=depth {
            out.push('\t');
        }
        pretty_node(child, depth + 1, out);
    }
    out.push('\n');
    for _ in 0..depth {
        out.push('\t');
    }
    out.push(')');
}
