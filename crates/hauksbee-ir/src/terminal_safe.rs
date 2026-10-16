//! The one place that decides which characters may reach a terminal or a CI log.
//!
//! A board file is third-party input, and net names, reference designators,
//! values and file names from it are printed back verbatim. A terminal does not
//! show some of those characters, it OBEYS them: an escape sequence can recolour,
//! move the cursor over, or conceal the `DO NOT ORDER` line above it, and a bidi
//! override reorders what is printed so a line reads differently from what it
//! says. `--json` escapes control characters already; the human surfaces did
//! not, so they pass through [`terminal_safe`] on the way out.

use std::borrow::Cow;
use std::fmt::Write as _;

/// Whether `c` is active in a terminal or reorders the text around it.
///
/// Newline and tab are layout, not control: every report is made of lines.
/// Everything else in C0 (including ESC, BEL, BS and CR, which overwrite what
/// was printed), DEL, the C1 block (U+009B is a one-character CSI on a terminal
/// that honours 8-bit controls) and the bidirectional controls (marks,
/// embeddings, overrides, isolates) is not.
#[must_use]
pub const fn is_terminal_active(c: char) -> bool {
    matches!(
        c,
        '\u{0}'..='\u{8}'
            | '\u{b}'..='\u{1f}'
            | '\u{7f}'..='\u{9f}'
            | '\u{61c}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}'
    )
}

/// `text` with every terminal-active character replaced by its visible `\uXXXX`
/// escape (also the JSON escape, so a JSON document passed through here stays
/// valid and decodes to the same string). Borrows when there is nothing to
/// replace, which is nearly always.
#[must_use]
pub fn terminal_safe(text: &str) -> Cow<'_, str> {
    if !text.chars().any(is_terminal_active) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len() + 16);
    for c in text.chars() {
        if is_terminal_active(c) {
            let _ = write!(out, "\\u{:04x}", u32::from(c));
        } else {
            out.push(c);
        }
    }
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::{is_terminal_active, terminal_safe};

    #[test]
    fn escape_sequences_are_shown_not_obeyed() {
        let hostile = "net \"PROG\u{1b}[31mRED\u{1b}[0m\" bell\u{7} cr\r";
        let safe = terminal_safe(hostile);
        assert_eq!(
            safe,
            "net \"PROG\\u001b[31mRED\\u001b[0m\" bell\\u0007 cr\\u000d"
        );
        assert!(!safe.chars().any(is_terminal_active));
    }

    #[test]
    fn bidi_overrides_and_c1_controls_are_shown_not_obeyed() {
        assert_eq!(terminal_safe("a\u{202e}b"), "a\\u202eb");
        assert_eq!(terminal_safe("a\u{2066}b\u{2069}"), "a\\u2066b\\u2069");
        assert_eq!(terminal_safe("csi\u{9b}"), "csi\\u009b");
        assert_eq!(terminal_safe("del\u{7f}"), "del\\u007f");
    }

    #[test]
    fn ordinary_text_is_untouched_and_not_copied() {
        for ordinary in ["", "GND", "line one\n\tline two\n", "Ω µF nét 電源 ✓ ‑"] {
            assert!(
                matches!(terminal_safe(ordinary), std::borrow::Cow::Borrowed(_)),
                "{ordinary:?}"
            );
        }
    }

    #[test]
    fn a_json_document_stays_valid_and_decodes_to_the_same_string() {
        let name = "x\u{202e}y\u{7f}z";
        let document = format!("{{\"net\":\"{name}\"}}");
        let safe = terminal_safe(&document).into_owned();
        let parsed: serde_json::Value = serde_json::from_str(&safe).expect("still JSON");
        assert_eq!(parsed["net"], name);
    }
}
