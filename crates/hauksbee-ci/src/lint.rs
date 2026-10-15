//! Live spec lint for the in-app editor: the same checks `hauksbee-ci check` makes
//! on a spec file, as diagnostics with a character range and, where the cure is
//! mechanical, a one-click fix.
//!
//! The editor sends the buffer as it stands, which usually has no `board` and no
//! `firmware` (the app supplies them from the upload), so the loader sees a spec
//! with placeholders in their place; every range is mapped back to the buffer.
//! Ranges are UTF-16 code units, the unit a browser editor indexes text in.

use std::fmt::Write as _;
use std::ops::Range;

use serde_json::{json, Value};
use toml_edit::{Document, Item, TableLike};

use crate::check::{classify_invalid, split_did_you_mean};
use crate::error::SpecError;
use crate::spec::Spec;

/// A one-click edit: replace `replace_start..replace_end` with `text`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LintFix {
    pub replace_start: usize,
    pub replace_end: usize,
    pub text: String,
    pub label: String,
}

/// One finding, with the range of text it is about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LintDiagnostic {
    /// `"error"` (the spec will not load) or `"warning"` (it loads, but should change).
    pub severity: &'static str,
    pub code: String,
    pub start: usize,
    pub end: usize,
    pub message: String,
    pub fix: Option<LintFix>,
}

/// A finding in byte offsets, before the UTF-16 conversion.
struct Raw {
    severity: &'static str,
    code: String,
    span: Range<usize>,
    message: String,
    fix: Option<(Range<usize>, String, String)>,
}

/// The `[[assert]]` tables of a parsed buffer, in order.
fn assert_tables<'a>(doc: &'a Document<&str>) -> Vec<&'a dyn TableLike> {
    match doc.get("assert") {
        Some(Item::ArrayOfTables(all)) => all.iter().map(|t| t as &dyn TableLike).collect(),
        Some(Item::Value(v)) => v
            .as_array()
            .into_iter()
            .flat_map(|a| a.iter().filter_map(|v| v.as_inline_table()))
            .map(|t| t as &dyn TableLike)
            .collect(),
        _ => Vec::new(),
    }
}

/// The first line of `text[from..]`, as a range (the whole rest if it has no newline).
fn first_line(text: &str, from: usize) -> Range<usize> {
    let from = from.min(text.len());
    let end = text[from..].find('\n').map_or(text.len(), |i| from + i);
    from..end
}

/// The range inside the quotes of a string value (the whole range if it is not quoted).
fn inside_quotes(text: &str, span: Range<usize>) -> Range<usize> {
    match text.as_bytes().get(span.start) {
        Some(b'"' | b'\'') if span.len() >= 2 => span.start + 1..span.end - 1,
        _ => span,
    }
}

/// The name a `did you mean 'x'?` clause suggests.
fn suggestion(fix: &str) -> Option<String> {
    fix.split('\'').nth(1).map(str::to_string)
}

/// Where a validation message about assertion `table` belongs: the first backticked
/// key it names that the table sets, else the `[[assert]]` header line.
fn locate_in_assert(text: &str, table: &dyn TableLike, message: &str) -> Range<usize> {
    message
        .split('`')
        .skip(1)
        .step_by(2)
        .find_map(|key| table.get_key_value(key)?.0.span())
        .or_else(|| table.get("kind").and_then(Item::span))
        .unwrap_or_else(|| first_line(text, 0))
}

/// Where a message about something outside one assertion belongs: the first
/// identifier it quotes that the buffer contains, else the first line.
fn locate_by_name(text: &str, message: &str) -> Range<usize> {
    message
        .split('\'')
        .skip(1)
        .step_by(2)
        .filter(|name| !name.is_empty())
        .find_map(|name| {
            let quoted = format!("\"{name}\"");
            text.find(&quoted)
                .map(|i| i + 1..i + 1 + name.len())
                .or_else(|| text.find(name).map(|i| i..i + name.len()))
        })
        .unwrap_or_else(|| first_line(text, 0))
}

/// Every independent error inside a possibly nested [`SpecError`].
fn flatten(err: SpecError, out: &mut Vec<SpecError>) {
    match err {
        SpecError::Many(all) => all.into_iter().for_each(|e| flatten(e, out)),
        other => out.push(other),
    }
}

/// The spellings the loader accepts but would rather not: `boot-coverage`.
fn alias_warnings(text: &str, tables: &[&dyn TableLike], out: &mut Vec<Raw>) {
    for table in tables {
        let Some(kind) = table
            .get("kind")
            .filter(|k| k.as_str().is_some_and(|s| s == "boot-coverage"))
        else {
            continue;
        };
        let Some(span) = kind.span() else { continue };
        let inner = inside_quotes(text, span.clone());
        out.push(Raw {
            severity: "warning",
            code: "alias".into(),
            span,
            message: "`boot-coverage` still works, but `boot_coverage` is the spelling every \
                      other kind uses."
                .into(),
            fix: Some((
                inner,
                "boot_coverage".into(),
                "Replace with boot_coverage".into(),
            )),
        });
    }
}

/// A refusal from the TOML deserializer (an unknown or mistyped key), mapped back from
/// the buffer-plus-stand-ins text it saw to the buffer. A misspelt key gets its fix.
fn deserialize_finding(text: &str, prefix_len: usize, e: &toml::de::Error) -> Raw {
    let span = e
        .span()
        .filter(|s| s.start >= prefix_len)
        .map(|s| s.start - prefix_len..s.end - prefix_len)
        .map_or_else(
            || first_line(text, 0),
            |s| s.start.min(text.len())..s.end.min(text.len()),
        );
    let message = e.message().to_string();
    let known = message.starts_with("unknown field");
    let fix = known
        .then(|| crate::check::unknown_field_fix(&message))
        .flatten()
        .and_then(|f| suggestion(&f))
        .filter(|_| {
            text.get(span.clone()).is_some_and(|s| {
                s.chars()
                    .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
            })
        })
        .map(|name| (span.clone(), name.clone(), format!("Replace with {name}")));
    Raw {
        severity: "error",
        code: if known { "unknown-field" } else { "toml-parse" }.into(),
        span,
        message,
        fix,
    }
}

/// One validation error of the assertion whose `[[assert]]` table is `table`: pinned to
/// the key it names, or to the kind of an unknown kind (which also gets its fix).
fn assert_finding(text: &str, table: Option<&dyn TableLike>, full: &str) -> Raw {
    let (message, hint) = split_did_you_mean(full);
    let kind_span = table.and_then(|t| t.get("kind")).and_then(Item::span);
    let unknown_kind = message.contains("unknown assertion kind");
    let span = match (table, &kind_span) {
        (Some(_), Some(k)) if unknown_kind => k.clone(),
        (Some(t), _) => locate_in_assert(text, t, &message),
        (None, _) => first_line(text, 0),
    };
    let fix = unknown_kind
        .then(|| hint.as_deref().and_then(suggestion))
        .flatten()
        .zip(kind_span)
        .map(|(name, k)| {
            (
                inside_quotes(text, k),
                name.clone(),
                format!("Replace with {name}"),
            )
        });
    Raw {
        severity: "error",
        code: classify_invalid(full).into(),
        span,
        message,
        fix,
    }
}

/// Run the checks on `text`; byte offsets.
fn lint_raw(text: &str) -> Vec<Raw> {
    let doc = match Document::parse(text) {
        Ok(doc) => doc,
        Err(e) => {
            let span = e.span().unwrap_or_else(|| first_line(text, 0));
            return vec![Raw {
                severity: "error",
                code: "toml-parse".into(),
                span,
                message: e.message().to_string(),
                fix: None,
            }];
        }
    };
    let tables = assert_tables(&doc);
    let mut out = Vec::new();
    alias_warnings(text, &tables, &mut out);

    // The loader insists on a board (and a boot_coverage check on firmware); the app
    // supplies both from the upload, so stand in for whichever the buffer omits.
    let mut prefix = String::new();
    for (key, stand_in) in [("board", "_"), ("firmware", "_")] {
        if !doc.contains_key(key) {
            let _ = writeln!(prefix, "{key} = \"{stand_in}\"");
        }
    }
    let shifted = format!("{prefix}{text}");
    let mut spec: Spec = match toml::from_str(&shifted) {
        Ok(spec) => spec,
        Err(e) => {
            out.push(deserialize_finding(text, prefix.len(), &e));
            return out;
        }
    };
    spec.normalize();

    // Per-assertion checks first, so each lands on its own `[[assert]]`.
    let mut seen: Vec<String> = Vec::new();
    for (a, table) in spec
        .asserts
        .iter()
        .zip(tables.iter().map(Some).chain(std::iter::repeat(None)))
    {
        let Err(err) = a.validate() else { continue };
        let mut errs = Vec::new();
        flatten(err, &mut errs);
        for err in errs {
            let full = err.to_string();
            seen.push(full.clone());
            out.push(assert_finding(text, table.copied(), &full));
        }
    }
    // Then whatever else the loader refuses that no single assertion owns.
    let mut rest = Vec::new();
    spec.validate_all()
        .into_iter()
        .for_each(|e| flatten(e, &mut rest));
    for err in rest {
        let full = err.to_string();
        if seen.contains(&full) {
            continue;
        }
        let (message, hint) = split_did_you_mean(&full);
        let span = locate_by_name(text, &message);
        out.push(Raw {
            severity: "error",
            code: classify_invalid(&full).into(),
            span,
            message: hint.map_or_else(|| message.clone(), |h| format!("{message} ({h})")),
            fix: None,
        });
    }
    out
}

/// UTF-16 code units in `text[..byte]`.
fn utf16_at(text: &str, byte: usize) -> usize {
    let byte = byte.min(text.len());
    // A span can end mid-character only if a caller mis-slices; round down rather than panic.
    let byte = (0..=byte)
        .rev()
        .find(|&b| text.is_char_boundary(b))
        .unwrap_or(0);
    text[..byte].encode_utf16().count()
}

/// Lint `text`; ranges are UTF-16 code units.
#[must_use]
pub fn lint(text: &str) -> Vec<LintDiagnostic> {
    lint_raw(text)
        .into_iter()
        .map(|r| LintDiagnostic {
            severity: r.severity,
            code: r.code,
            start: utf16_at(text, r.span.start),
            end: utf16_at(text, r.span.end),
            message: r.message,
            fix: r.fix.map(|(range, replacement, label)| LintFix {
                replace_start: utf16_at(text, range.start),
                replace_end: utf16_at(text, range.end),
                text: replacement,
                label,
            }),
        })
        .collect()
}

/// The JSON `hauksbee-ci lint` prints and `POST /api/ci/lint` returns:
/// `{ok, diagnostics:[{severity, code, start, end, message, fix?:{replace_start, replace_end, text, label}}]}`.
#[must_use]
pub fn lint_json(text: &str) -> Value {
    let diagnostics: Vec<Value> = lint(text)
        .into_iter()
        .map(|d| {
            let mut v = json!({
                "severity": d.severity, "code": d.code,
                "start": d.start, "end": d.end, "message": d.message,
            });
            if let Some(f) = d.fix {
                v["fix"] = json!({
                    "replace_start": f.replace_start, "replace_end": f.replace_end,
                    "text": f.text, "label": f.label,
                });
            }
            v
        })
        .collect();
    json!({ "ok": true, "diagnostics": diagnostics })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(text: &str, fix: &LintFix) -> String {
        // The ranges are UTF-16; the tests use ASCII, where they are byte offsets.
        format!(
            "{}{}{}",
            &text[..fix.replace_start],
            fix.text,
            &text[fix.replace_end..]
        )
    }

    #[test]
    fn a_valid_spec_with_no_board_has_no_findings() {
        let text = "[[assert]]\nkind = \"voltage\"\nnet = \"+5V\"\nmin = 4.5\n";
        assert_eq!(lint(text), vec![]);
    }

    #[test]
    fn the_old_boot_coverage_spelling_gets_a_one_click_fix() {
        let text = "[[assert]]\nkind=\"boot-coverage\"";
        let warn = lint(text)
            .into_iter()
            .find(|d| d.severity == "warning")
            .unwrap();
        let fix = warn.fix.expect("a fix");
        assert_eq!(fix.text, "boot_coverage");
        assert_eq!(fix.label, "Replace with boot_coverage");
        // The range is the text inside the quotes, on line 2.
        assert_eq!(&text[fix.replace_start..fix.replace_end], "boot-coverage");
        assert!(fix.replace_start > text.find('\n').unwrap());
        assert_eq!(apply(text, &fix), "[[assert]]\nkind=\"boot_coverage\"");
    }

    #[test]
    fn a_misspelt_kind_is_pinned_to_its_value_with_a_fix() {
        let text = "[[assert]]\nkind = \"voltag\"\nnet = \"A\"\nmin = 1\n";
        let d = lint(text)
            .into_iter()
            .find(|d| d.code == "unknown-kind")
            .unwrap();
        assert_eq!(&text[d.start..d.end], "\"voltag\"");
        let fix = d.fix.expect("a fix");
        assert_eq!(apply(text, &fix), text.replace("voltag", "voltage"));
    }

    #[test]
    fn a_bad_bound_is_pinned_to_the_assertion() {
        let text = "[[assert]]\nkind = \"voltage\"\nnet = \"A\"\n[[assert]]\nkind = \"toggle\"\nnet = \"B\"\nfreq_hz = 5.0\ntolerance = 7.0\n";
        let found = lint(text);
        assert!(!found.is_empty());
        let second = text.rfind("[[assert]]").unwrap();
        assert!(found.iter().all(|d| d.start >= second), "{found:?}");
    }

    #[test]
    fn a_misspelt_key_is_pinned_to_the_key_with_a_fix() {
        let text = "[[assert]]\nkind = \"voltage\"\nnet = \"A\"\nmim = 1\n";
        let d = lint(text)
            .into_iter()
            .find(|d| d.code == "unknown-field")
            .unwrap();
        assert_eq!(&text[d.start..d.end], "mim");
        assert_eq!(
            apply(text, &d.fix.expect("a fix")),
            text.replace("mim", "min")
        );
    }

    #[test]
    fn a_syntax_error_has_a_position_and_nothing_else_is_said() {
        let d = lint("[[assert\nkind = 1");
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].code, "toml-parse");
    }

    #[test]
    fn ranges_count_utf16_units() {
        // 'é' is two bytes but one UTF-16 unit; the astral emoji is four bytes, two units.
        let text = "# é\u{1F600}\n[[assert]]\nkind = \"voltag\"\nnet = \"A\"\nmin = 1\n";
        let d = lint(text)
            .into_iter()
            .find(|d| d.code == "unknown-kind")
            .unwrap();
        let units: Vec<u16> = text.encode_utf16().collect();
        assert_eq!(
            String::from_utf16(&units[d.start..d.end]).unwrap(),
            "\"voltag\""
        );
    }
}
