//! The structured refusal for a board the readers would not take.
//!
//! `/api/analyze` and the CLI's JSON both answer an unreadable input with an
//! `import_failure` record. Besides the parser stage and the located excerpt
//! it names *what kind* of refusal this was ([`ImportFailureKind`]), the file,
//! where the parser stopped, what the content turned out to be, the supported
//! formats as data, and the plain words for a person: a title, one sentence of
//! cause, ordered steps and a one-line `suggested_fix`. This module is the one
//! place those words live; the browser prefers them and keeps its own
//! phrase-matching only as a fallback for a server that predates them.
//!
//! The readers hand back strings (`BoardInputError::Extract(String)`), so the
//! kind is decided from three things in order: the error variant, whether a
//! reader claims the bytes (the content sniff the analysis itself uses), and
//! the reader's message. Tests pin each kind against a real refusal so a
//! reworded reader message fails here, not silently in the app.

use hauksbee_bind::board_input::{BoardInputError, FormatRoute, ACCEPTED_FORMATS};
use hauksbee_extract::reader::{format_phrase, Registry};
use serde::Serialize;

/// Which refusal this was. Closed set; serialised in `snake_case`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportFailureKind {
    /// A reader claimed the file but it stops inside a bracket or string.
    Truncated,
    /// A reader claimed the file and the parser stopped at a syntax error.
    SyntaxError,
    /// No reader claims the contents, though the extension is a board one (or
    /// the contents are a recognised non-board such as a Git LFS pointer).
    WrongContent,
    /// No reader claims the contents and the extension is not a board format.
    UnsupportedExtension,
    /// The file has no contents.
    EmptyFile,
    /// A format hauksbee recognises and deliberately does not read.
    UnsupportedFormat,
    /// A reader claimed the file but refused its contents (not a syntax
    /// error: a value that cannot be analysed truthfully, a refused entity).
    InvalidContent,
    /// A fab package (zip or gerber folder) that could not be reconstructed.
    FabPackage,
    /// A schematic whose sheet hierarchy could not be read.
    SchematicSheets,
    /// The input could not be opened at all.
    Unreadable,
}

/// One supported board format, as data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WebSupportedFormat {
    pub vendor: String,
    pub exts: Vec<String>,
}

/// Actionable context for a board the parser refused.
///
/// `excerpt` is present only when the underlying parser identified an exact
/// line; no arbitrary first line is presented as the cause when the parser did
/// not localise it.
#[derive(Debug, Clone, Serialize)]
pub struct WebImportFailure {
    pub stage: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub excerpt: Option<String>,
    /// One plain sentence: the first thing to do.
    pub suggested_fix: String,
    pub kind: ImportFailureKind,
    /// The file as named on upload.
    pub file: String,
    /// Its extension, lower case with the dot; absent when it has none.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub extension: String,
    /// Where the parser stopped, when it said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub byte: Option<usize>,
    /// For `wrong_content`: what the contents are, when they are recognisably
    /// something else ("a Git LFS pointer"). Absent means no known format.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detected_format: Option<String>,
    /// For `wrong_content`: what the extension promised ("a KiCad board").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_format: Option<String>,
    /// A person's heading.
    pub title: String,
    /// One sentence naming the file and what is wrong with it.
    pub cause: String,
    /// Concrete steps, in order. Never empty.
    pub steps: Vec<String>,
    /// The board formats hauksbee reads, so a refusal can list them without
    /// the list being embedded in prose.
    pub supported_formats: Vec<WebSupportedFormat>,
}

/// The board formats a refusal lists: every advertised board-route format.
#[must_use]
pub fn supported_formats() -> Vec<WebSupportedFormat> {
    ACCEPTED_FORMATS
        .iter()
        .filter(|f| !f.quiet && f.route == FormatRoute::Board)
        .map(|f| WebSupportedFormat {
            vendor: f.vendor.to_string(),
            exts: f.exts.iter().map(ToString::to_string).collect(),
        })
        .collect()
}

const RESAVE: &str = "Open the project in the tool that made it, save it again (KiCad: File > Save), and load the new file.";
const RECOPY: &str =
    "If you copied or downloaded it, copy it again and check the size matches the original.";

fn quoted(name: &str) -> String {
    format!("\u{201c}{name}\u{201d}")
}

/// The extension, lower case with the dot (`.tar.gz` counts as one).
fn extension_of(file: &str) -> String {
    let leaf = file
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(file)
        .to_lowercase();
    if leaf.ends_with(".tar.gz") {
        return ".tar.gz".to_string();
    }
    leaf.rfind('.')
        .filter(|&i| i + 1 < leaf.len())
        .map(|i| leaf[i..].to_string())
        .unwrap_or_default()
}

fn is_known_extension(ext: &str) -> bool {
    ACCEPTED_FORMATS
        .iter()
        .flat_map(|f| f.exts.iter())
        .any(|e| e.eq_ignore_ascii_case(ext))
}

/// What a board extension promises, in the words of `format_phrase`.
fn expected_for(ext: &str) -> Option<&'static str> {
    let reader = match ext {
        ".kicad_pcb" => "kicad-pcb",
        ".kicad_sch" => "kicad-schematic",
        ".net" => "kicad-netlist",
        ".brd" => "eagle",
        ".pcbdoc" => "altium",
        ".json" | ".epcb" | ".epro" => "easyeda",
        ".xml" => "ipc-2581",
        ".d356" => "ipc-d356",
        ".tgz" | ".tar" | ".tar.gz" => "odb++",
        _ => return None,
    };
    Some(format_phrase(reader))
}

fn first_upper(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |c| {
        c.to_uppercase().collect::<String>() + chars.as_str()
    })
}

/// The `line N` / `byte N` a parser message names.
fn number_after(message: &str, word: &str) -> Option<usize> {
    let lower = message.to_ascii_lowercase();
    for (index, _) in lower.match_indices(word) {
        if index > 0 && lower.as_bytes()[index - 1].is_ascii_alphanumeric() {
            continue;
        }
        let tail = &message[index + word.len()..];
        let Some(tail) = tail.strip_prefix(':').or_else(|| tail.strip_prefix(' ')) else {
            continue;
        };
        let digits: String = tail
            .chars()
            .skip_while(char::is_ascii_whitespace)
            .take_while(char::is_ascii_digit)
            .collect();
        if let Ok(n) = digits.parse::<usize>() {
            if n > 0 || word == "byte" {
                return Some(n);
            }
        }
    }
    None
}

fn says_truncated(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    [
        "unclosed list",
        "unclosed string",
        "unexpected end",
        "unexpected eof",
        "premature end",
        "ends early",
        "truncated",
        "unterminated",
        "eof while",
    ]
    .iter()
    .any(|p| lower.contains(p))
}

fn says_syntax(message: &str, line: Option<usize>) -> bool {
    let lower = message.to_ascii_lowercase();
    line.is_some()
        || ["parse error", "xml:", "syntax", "malformed", "unexpected"]
            .iter()
            .any(|p| lower.contains(p))
}

/// What an unrecognised file turned out to be, from the reader's own message.
fn detected_from_message(message: &str) -> Option<&'static str> {
    let lower = message.to_ascii_lowercase();
    if lower.contains("git lfs pointer") {
        Some("a Git LFS pointer")
    } else if lower.contains("single gerber layer") {
        Some("a single gerber layer")
    } else if lower.contains("single excellon drill") {
        Some("a single Excellon drill program")
    } else if lower.contains("ascii protel export") {
        Some("an ASCII Protel export")
    } else {
        None
    }
}

/// Everything the classifier decided before it writes words.
struct Parts {
    stage: &'static str,
    kind: ImportFailureKind,
    detected: Option<String>,
    expected: Option<String>,
}

fn classify_kind(
    error: &BoardInputError,
    ext: &str,
    contents: &[u8],
    message: &str,
    line: Option<usize>,
) -> Parts {
    let plain = |stage, kind| Parts {
        stage,
        kind,
        detected: None,
        expected: None,
    };
    match error {
        BoardInputError::Zip(_) | BoardInputError::Gerber(_) => {
            plain("fab-package reader", ImportFailureKind::FabPackage)
        }
        BoardInputError::Schematic(_) => {
            plain("schematic reader", ImportFailureKind::SchematicSheets)
        }
        BoardInputError::Unsupported(_) => plain(
            "format capability check",
            ImportFailureKind::UnsupportedFormat,
        ),
        BoardInputError::NotFound { .. } | BoardInputError::Io { .. } => {
            plain("input read", ImportFailureKind::Unreadable)
        }
        BoardInputError::Extract(_) => {
            let stage = "board reader";
            if message.contains("this file is empty") {
                return plain(stage, ImportFailureKind::EmptyFile);
            }
            if let Some(reader) = Registry::builtin().detect(contents, None) {
                let kind = if says_truncated(message) {
                    ImportFailureKind::Truncated
                } else if says_syntax(message, line) {
                    ImportFailureKind::SyntaxError
                } else {
                    ImportFailureKind::InvalidContent
                };
                return Parts {
                    stage,
                    kind,
                    detected: Some(format_phrase(reader.name()).to_string()),
                    expected: None,
                };
            }
            if let Some(found) = detected_from_message(message) {
                return Parts {
                    stage,
                    kind: ImportFailureKind::WrongContent,
                    detected: Some(found.to_string()),
                    expected: expected_for(ext).map(ToString::to_string),
                };
            }
            if message.contains("unrecognized board format") {
                let known = is_known_extension(ext);
                return Parts {
                    stage,
                    kind: if known {
                        ImportFailureKind::WrongContent
                    } else {
                        ImportFailureKind::UnsupportedExtension
                    },
                    detected: None,
                    expected: expected_for(ext).map(ToString::to_string),
                };
            }
            plain(stage, ImportFailureKind::InvalidContent)
        }
    }
}

/// The words for a refusal: `(title, cause, steps)`.
#[allow(clippy::too_many_lines)] // one arm of wording per kind, kept side by side
fn wording(
    parts: &Parts,
    file: &str,
    ext: &str,
    message: &str,
    line: Option<usize>,
) -> (String, String, Vec<String>) {
    let name = quoted(file);
    // The reader's own sentence, without the file prefix or the web wrapper.
    let own = {
        let m = message
            .strip_prefix("Could not read this board file: ")
            .unwrap_or(message);
        let m = m.strip_prefix(&format!("'{file}': ")).unwrap_or(m);
        first_upper(m.trim())
    };
    let steps = |list: &[&str]| list.iter().map(ToString::to_string).collect::<Vec<_>>();
    match parts.kind {
        ImportFailureKind::Truncated => (
            "This file ends early".to_string(),
            format!("{name} stops in the middle of its contents, so it looks cut off. That usually means a save, copy or download was interrupted."),
            steps(&[RESAVE, RECOPY]),
        ),
        ImportFailureKind::SyntaxError => {
            let place = line.map_or_else(
                || "the place named in the technical detail".to_string(),
                |n| format!("line {n}"),
            );
            (
                "This file has a syntax error".to_string(),
                format!("{name} could not be read past {place}. A hand edit or a bad merge can break the file there."),
                vec![
                    format!("Open the file at {place} and fix or undo the last edit made there."),
                    format!("Or {}{}", RESAVE[..1].to_lowercase(), &RESAVE[1..]),
                ],
            )
        }
        ImportFailureKind::EmptyFile => (
            "This file is empty".to_string(),
            format!("{name} has no contents. An export or a copy probably failed part-way."),
            steps(&[RESAVE, RECOPY]),
        ),
        ImportFailureKind::WrongContent => match (&parts.detected, &parts.expected) {
            (Some(found), _) => (
                "This is not a board file".to_string(),
                format!("{name} is {found}, not a board."),
                vec![own],
            ),
            (None, Some(expected)) if ext == ".kicad_pcb" => (
                format!("This is not {expected}"),
                format!("{name} ends in .kicad_pcb, but a KiCad board starts with \u{201c}(kicad_pcb\u{201d}, and this file does not. It may be a different file that was renamed, or an export that came out empty."),
                steps(&[
                    "Open the project in KiCad, open the PCB editor, and save the board (File > Save).",
                    "Load that .kicad_pcb from your project folder, not the .kicad_pro project file or a PDF.",
                ]),
            ),
            (None, Some(expected)) => (
                format!("This is not {expected}"),
                format!("{name} ends in {ext}, but its contents are not {expected}. It may be a different file that was renamed, or an export that came out empty."),
                steps(&[
                    "Open the design in the tool that made it and export the board again.",
                    "Load the board file itself, not a project file or a PDF.",
                ]),
            ),
            (None, None) => (
                format!("Hauksbee cannot read a {ext} file"),
                format!("{name} is not one of the board formats hauksbee reads, or its contents do not match what a {ext} file should hold."),
                board_steps(),
            ),
        },
        ImportFailureKind::UnsupportedExtension if ext.is_empty() => (
            "Hauksbee cannot read this file".to_string(),
            format!("{name} has no file extension, and its contents are not any board format hauksbee reads."),
            board_steps(),
        ),
        ImportFailureKind::UnsupportedExtension => (
            format!("Hauksbee cannot read a {ext} file"),
            format!("{name} is not one of the board formats hauksbee reads."),
            board_steps(),
        ),
        ImportFailureKind::UnsupportedFormat => (
            "Hauksbee does not read this format".to_string(),
            own,
            steps(&["Do what the message above names, then load the converted file."]),
        ),
        ImportFailureKind::InvalidContent => (
            "That file did not load".to_string(),
            own,
            steps(&[
                RESAVE,
                "Still stuck? Include the technical detail when you report the file.",
            ]),
        ),
        ImportFailureKind::FabPackage => (
            "Hauksbee cannot read this fab package".to_string(),
            own,
            steps(&["Zip one folder holding every copper layer and the drill file, one unambiguous film per layer, or load the original CAD board instead."]),
        ),
        ImportFailureKind::SchematicSheets => (
            "The schematic is incomplete".to_string(),
            own,
            steps(&["Open and re-save the schematic hierarchy in KiCad, and make sure every sheet it refers to sits beside the root file."]),
        ),
        ImportFailureKind::Unreadable => (
            "That file could not be opened".to_string(),
            own,
            steps(&["Check the path and file permissions, then try the exact file again."]),
        ),
    }
}

fn board_steps() -> Vec<String> {
    vec![
        "Load the board design itself: for KiCad the .kicad_pcb in your project folder, for Eagle the .brd, for Altium the .PcbDoc.".to_string(),
        "Only have fab files? Zip the folder holding the gerbers and the drill file and load the zip.".to_string(),
    ]
}

/// The structured refusal for `error` on the file `file_name` with `contents`.
///
/// `excerpt` is the located source line, computed by the caller (it owns the
/// 300-character cap) and only present when the parser named a line.
#[must_use]
pub fn classify(
    error: &BoardInputError,
    file_name: &str,
    contents: &[u8],
    excerpt: Option<String>,
) -> WebImportFailure {
    let message = error.to_string();
    let ext = extension_of(file_name);
    let line = number_after(&message, "line");
    let byte = number_after(&message, "byte");
    let parts = classify_kind(error, &ext, contents, &message, line);
    let (title, cause, steps) = wording(&parts, file_name, &ext, &message, line);
    let suggested_fix = steps.first().cloned().unwrap_or_default();
    WebImportFailure {
        stage: parts.stage.to_string(),
        excerpt,
        suggested_fix,
        kind: parts.kind,
        file: file_name.to_string(),
        extension: ext,
        line,
        byte,
        detected_format: parts.detected,
        expected_format: parts.expected,
        title,
        cause,
        steps,
        supported_formats: supported_formats(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refuse(file: &str, bytes: &[u8]) -> WebImportFailure {
        let error = hauksbee_bind::board_input::from_bytes(file, bytes)
            .err()
            .unwrap_or_else(|| panic!("{file} must be refused"));
        classify(&error, file, bytes, None)
    }

    #[test]
    fn a_cut_off_kicad_board_is_truncated_with_its_place() {
        let f = refuse(
            "cut.kicad_pcb",
            b"(kicad_pcb (version 20240108)\n  (footprint \"R\"",
        );
        assert_eq!(f.kind, ImportFailureKind::Truncated);
        assert_eq!(f.file, "cut.kicad_pcb");
        assert_eq!(f.line, Some(2));
        assert!(f.byte.is_some(), "{f:?}");
        assert_eq!(f.detected_format.as_deref(), Some("a KiCad board"));
        assert_eq!(f.title, "This file ends early");
        assert!(f.cause.contains("cut.kicad_pcb"));
        assert!(f.suggested_fix.contains("KiCad: File > Save"));
    }

    #[test]
    fn a_broken_kicad_board_is_a_syntax_error() {
        let f = refuse("bad.kicad_pcb", b"(kicad_pcb (version 1))\n)\n");
        assert_eq!(f.kind, ImportFailureKind::SyntaxError, "{f:?}");
        assert_eq!(f.title, "This file has a syntax error");
        assert!(f.steps.len() >= 2);
    }

    #[test]
    fn contents_that_are_no_board_are_wrong_content_when_the_extension_is_one() {
        let f = refuse("fake.kicad_pcb", b"hello this is not a board");
        assert_eq!(f.kind, ImportFailureKind::WrongContent);
        assert_eq!(f.detected_format, None);
        assert_eq!(f.expected_format.as_deref(), Some("a KiCad board"));
        assert_eq!(f.title, "This is not a KiCad board");
        assert!(f.cause.contains("(kicad_pcb"));
    }

    #[test]
    fn a_recognised_non_board_names_what_it_is() {
        let f = refuse(
            "board.kicad_pcb",
            b"version https://git-lfs.github.com/spec/v1\noid sha256:abc\nsize 12345\n",
        );
        assert_eq!(f.kind, ImportFailureKind::WrongContent);
        assert_eq!(f.detected_format.as_deref(), Some("a Git LFS pointer"));
        assert!(f.steps[0].contains("git lfs pull"), "{:?}", f.steps);
    }

    #[test]
    fn an_unknown_extension_with_unknown_contents_is_an_unsupported_extension() {
        let f = refuse("design.xyz", b"hello world, definitely not a board");
        assert_eq!(f.kind, ImportFailureKind::UnsupportedExtension);
        assert_eq!(f.extension, ".xyz");
        assert_eq!(f.title, "Hauksbee cannot read a .xyz file");
        let none = refuse("README", b"hello world, definitely not a board");
        assert_eq!(none.kind, ImportFailureKind::UnsupportedExtension);
        assert!(none.cause.contains("no file extension"));
    }

    #[test]
    fn an_empty_file_says_so() {
        let f = refuse("empty.kicad_pcb", b"   \n");
        assert_eq!(f.kind, ImportFailureKind::EmptyFile);
        assert_eq!(f.title, "This file is empty");
    }

    #[test]
    fn a_pre_eagle_6_drawing_is_an_unsupported_format_with_its_own_fix() {
        let f = refuse("old.brd", &[0x10, 0x80, 0x64, 0x00, 0x21, 0x13, 0x00, 0x00]);
        assert_eq!(f.kind, ImportFailureKind::UnsupportedFormat);
        assert!(f.cause.to_lowercase().contains("eagle 6"), "{}", f.cause);
    }

    #[test]
    fn a_junk_zip_is_a_fab_package_failure() {
        use std::io::Write;
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        zip.start_file("README.md", zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(b"not a board").unwrap();
        let bytes = zip.finish().unwrap().into_inner();
        let f = refuse("junk.zip", &bytes);
        assert_eq!(f.kind, ImportFailureKind::FabPackage, "{f:?}");
        assert_eq!(f.stage, "fab-package reader");
    }

    #[test]
    fn the_supported_list_is_data_not_prose() {
        let f = refuse("x.kicad_pcb", b"nope");
        assert!(f
            .supported_formats
            .iter()
            .any(|s| s.vendor == "KiCad" && s.exts.contains(&".kicad_pcb".to_string())));
        assert!(
            !f.supported_formats.iter().any(|s| s.vendor == "SPICE deck"),
            "decks are not boards"
        );
        for text in [&f.title, &f.cause, &f.suggested_fix] {
            assert!(!text.contains("Supported:"), "{text}");
        }
    }

    #[test]
    fn extensions_are_lower_case_with_the_dot() {
        assert_eq!(extension_of("a/b/Board.PcbDoc"), ".pcbdoc");
        assert_eq!(extension_of("job.TAR.GZ"), ".tar.gz");
        assert_eq!(extension_of("README"), "");
        assert_eq!(extension_of("trailing."), "");
    }
}
