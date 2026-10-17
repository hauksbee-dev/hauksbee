//! The one place that decides which characters may reach a terminal or a CI log.
//!
//! A board file is third-party input, and net names, reference designators,
//! values and file names from it are printed back verbatim. A terminal does not
//! show some of those characters, it OBEYS them: an escape sequence can recolour,
//! move the cursor over, or conceal the `DO NOT ORDER` line above it, and a bidi
//! override reorders what is printed so a line reads differently from what it
//! says. `--json` escapes control characters already; the human surfaces did
//! not, so they pass through [`terminal_safe`] on the way out.
//!
//! The same module owns the print macros every crate that echoes board text
//! uses (`print!`, `println!`, `eprint!`, `eprintln!`, defined once here). A
//! crate opts in with `use hauksbee_ir::{eprintln, println};` in each file that
//! prints, which shadows the std macro of the same name for that file; the macro
//! formats its arguments and passes the result through [`terminal_safe`].

use std::borrow::Cow;
use std::fmt::{Arguments, Write as _};

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

/// `text` with every terminal-active character shown as a `\uXXXX` escape.
///
/// The escape is also the JSON one, so a JSON document passed through here stays
/// valid and decodes to the same string. Borrows when there is nothing to
/// replace, which is nearly always. A carriage return directly before a line
/// feed is a Windows line ending, not an overwrite, so it is left alone.
#[must_use]
pub fn terminal_safe(text: &str) -> Cow<'_, str> {
    let active = |i: usize, c: char| {
        is_terminal_active(c) && !(c == '\r' && text.as_bytes().get(i + 1) == Some(&b'\n'))
    };
    if !text.char_indices().any(|(i, c)| active(i, c)) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len() + 16);
    for (i, c) in text.char_indices() {
        if active(i, c) {
            let _ = write!(out, "\\u{:04x}", u32::from(c));
        } else {
            out.push(c);
        }
    }
    Cow::Owned(out)
}

/// `print!` with the text made terminal-safe. Use through the macro.
#[doc(hidden)]
pub fn print(args: Arguments<'_>) {
    // A progress bar on this line would run into the text.
    crate::progress::clear_stderr_line();
    let text = std::fmt::format(args);
    ::std::print!("{}", terminal_safe(&text));
}

/// `println!` with the text made terminal-safe; one trailing carriage return
/// (a CRLF line ending carried in from a file) is dropped, not shown as an escape.
#[doc(hidden)]
pub fn println(args: Arguments<'_>) {
    // A progress bar on this line would run into the text.
    crate::progress::clear_stderr_line();
    let text = std::fmt::format(args);
    let text = text.strip_suffix('\r').unwrap_or(&text);
    ::std::println!("{}", terminal_safe(text));
}

/// `eprint!` with the text made terminal-safe. Use through the macro.
#[doc(hidden)]
pub fn eprint(args: Arguments<'_>) {
    // A progress bar on this line would run into the text.
    crate::progress::clear_stderr_line();
    let text = std::fmt::format(args);
    ::std::eprint!("{}", terminal_safe(&text));
}

/// `eprintln!` with the text made terminal-safe; see [`println`] for the CR rule.
#[doc(hidden)]
pub fn eprintln(args: Arguments<'_>) {
    // A progress bar on this line would run into the text.
    crate::progress::clear_stderr_line();
    let text = std::fmt::format(args);
    let text = text.strip_suffix('\r').unwrap_or(&text);
    ::std::eprintln!("{}", terminal_safe(text));
}

/// Terminal-safe `print!`.
#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => { $crate::terminal_safe::print(::std::format_args!($($arg)*)) };
}

/// Terminal-safe `println!`.
#[macro_export]
macro_rules! println {
    () => { $crate::terminal_safe::println(::std::format_args!("")) };
    ($($arg:tt)*) => { $crate::terminal_safe::println(::std::format_args!($($arg)*)) };
}

/// Terminal-safe `eprint!`.
#[macro_export]
macro_rules! eprint {
    ($($arg:tt)*) => { $crate::terminal_safe::eprint(::std::format_args!($($arg)*)) };
}

/// Terminal-safe `eprintln!`.
#[macro_export]
macro_rules! eprintln {
    () => { $crate::terminal_safe::eprintln(::std::format_args!("")) };
    ($($arg:tt)*) => { $crate::terminal_safe::eprintln(::std::format_args!($($arg)*)) };
}

#[cfg(test)]
mod tests {
    use super::{is_terminal_active, terminal_safe};

    #[test]
    fn a_crlf_line_ending_is_layout_but_a_bare_carriage_return_is_not() {
        assert_eq!(terminal_safe("one\r\ntwo\r\n"), "one\r\ntwo\r\n");
        assert_eq!(terminal_safe("over\rwrite"), "over\\u000dwrite");
        assert_eq!(terminal_safe("end\r"), "end\\u000d");
    }

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
