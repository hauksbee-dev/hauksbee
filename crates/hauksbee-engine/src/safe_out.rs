//! The output boundary: every `print!`/`println!`/`eprint!`/`eprintln!` in this
//! crate goes through [`hauksbee_ir::terminal_safe`] before it reaches a
//! terminal or a CI log.
//!
//! Board files are third-party input and their net names, references, values and
//! file names are printed back verbatim. A terminal obeys an escape sequence in
//! one of them (recolour, move the cursor, conceal the `DO NOT ORDER` line) and a
//! bidi override reorders a line, so the characters are replaced by their visible
//! `\uXXXX` form on the way out. Doing it here, once, covers every report surface
//! (plain, text, JSON, lists, errors) without each renderer having to remember.
//!
//! The crate shadows the std macros by name (this module is `#[macro_use]`d first
//! in `lib.rs`, so the textual scope reaches every module after it), which keeps
//! every call site, and libtest's output capture, exactly as they were. The
//! binary imports the same macros, so its own prints are covered too.

use std::fmt::Arguments;

use hauksbee_ir::terminal_safe::terminal_safe;

#[doc(hidden)]
pub fn print(args: Arguments<'_>) {
    let text = std::fmt::format(args);
    ::std::print!("{}", terminal_safe(&text));
}

#[doc(hidden)]
pub fn println(args: Arguments<'_>) {
    let text = std::fmt::format(args);
    ::std::println!("{}", terminal_safe(&text));
}

#[doc(hidden)]
pub fn eprint(args: Arguments<'_>) {
    let text = std::fmt::format(args);
    ::std::eprint!("{}", terminal_safe(&text));
}

#[doc(hidden)]
pub fn eprintln(args: Arguments<'_>) {
    let text = std::fmt::format(args);
    ::std::eprintln!("{}", terminal_safe(&text));
}

#[doc(hidden)]
#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => { $crate::safe_out::print(::std::format_args!($($arg)*)) };
}

#[doc(hidden)]
#[macro_export]
macro_rules! println {
    ($($arg:tt)*) => { $crate::safe_out::println(::std::format_args!($($arg)*)) };
}

#[doc(hidden)]
#[macro_export]
macro_rules! eprint {
    ($($arg:tt)*) => { $crate::safe_out::eprint(::std::format_args!($($arg)*)) };
}

#[doc(hidden)]
#[macro_export]
macro_rules! eprintln {
    ($($arg:tt)*) => { $crate::safe_out::eprintln(::std::format_args!($($arg)*)) };
}
