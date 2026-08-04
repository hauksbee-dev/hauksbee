//! The unified board-input normalizer: every surface that accepts "a board"
//! (the web front door, `hauksbee run`, `hauksbee models resolve`, and
//! `hauksbee-ci`) routes through this one module, so the set of accepted
//! formats can never drift between surfaces again.
//!
//! Normalizing per surface is how format sets drift apart: a surface that
//! reads the file as UTF-8 text rejects a binary Altium board and a `.board`
//! export outright, and one that re-reads the ORIGINAL bytes with only the
//! text/binary sniffers fails co-sim with "could not re-read the board" on a
//! `.board` or gerber zip that just produced a clean static report. One
//! normalizer, two entry points:
//!
//! * [`from_bytes`]: the web path. A file name (display/sniff hint only) plus
//!   the file's RAW bytes. Binary formats are sniffed from the bytes first, a
//!   `PK\x03\x04` prefix routes through the zip classifier, Board-as-Code is
//!   compiled to KiCad board text, and everything else falls through to the
//!   text sniffer. It never invents a board name: a titleless board keeps its
//!   empty name so the web report's `board_name` is unchanged.
//! * [`from_path`]: the CLI/CI path. Adds what only a path can give you:
//!   gerber DIRECTORY detection, `.kicad_sch` loaded by path so sheet
//!   hierarchies recurse into sibling files, and the file-stem fallback for a
//!   board whose layout carries no title-block name.
//!
//! Both feed the same [`NormalizedBoard`], whose [`InputKind`] disambiguates
//! the `layout_text == None` cases: an Altium board gets its DRC from the raw
//! bytes twin ([`ExtractedBoard::altium_drc`]), while a gerber archive, an ODB++
//! job and an IPC-2581 document have no layout file at all and their DRC/SI
//! sections say "Not checked" instead of a vacuous green.
//!
//! Two of the accepted formats can only be recognised HERE, not by the extract
//! crate's reader registry, because the registry claims a format from bytes:
//!
//! * an **unpacked ODB++ job** is a directory tree, so it is detected by the
//!   presence of `matrix/matrix` under the path before the directory is assumed
//!   to be a gerber folder;
//! * an **ODB++ archive** is a `.zip` or a `.tgz`, containers the zip branch
//!   would otherwise classify as gerbers or Board-as-Code, so its content sniff
//!   runs first.
//!
//! This lives in `hauksbee-engine` (not `hauksbee-extract`) because
//! Board-as-Code compilation ([`crate::boardcode::code_to_board_text`]) needs
//! forge-codegen, which the extract crate must not depend on.

// This module reads board files the user did not write, and `hauksbee serve`
// exposes it to a browser, so a panic here is a denial of service rather than
// a crash in a CLI. Failures must be typed errors that the caller can report.
// Test code below is exempt: an unwrap in a test is an assertion.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use std::borrow::Cow;
use std::path::Path;

use hauksbee_extract::ExtractedBoard;

/// What kind of input the normalizer recognised. Call sites use this where
/// they would otherwise keep `is_binary` / `is_gerber` flags; the
/// `layout_text == None` kinds (Altium, Gerber, Odb, Ipc2581) need different DRC
/// handling from the text ones and from each other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputKind {
    /// A text layout/netlist format (KiCad `.kicad_pcb`, Eagle `.brd`,
    /// IPC-D-356, `.net`), sniffed from content.
    Text,
    /// A KiCad schematic loaded BY PATH so its sheet hierarchy recurses
    /// ([`from_path`] only; [`from_bytes`] content-sniffs a lone sheet as
    /// [`InputKind::Text`] because it cannot see sibling files).
    Schematic,
    /// A binary Altium `.PcbDoc` (OLE2 container), parsed from raw bytes.
    Altium,
    /// A gerber fab archive (directory or zip), reverse-extracted from copper.
    Gerber,
    /// An ODB++ job (directory, `.tgz` or `.zip`). Like a gerber archive it has
    /// no KiCad layout text, but unlike one its connectivity is READ from the
    /// job's own EDA data rather than reconstructed from copper.
    Odb,
    /// An IPC-2581 (DPMX) design-exchange document. Text, but not KiCad-parseable
    /// text, so it carries no `layout_text` either.
    Ipc2581,
    /// A Board-as-Code `.board` source (bare or zipped), compiled to KiCad
    /// board text; `layout_text` carries the COMPILED text.
    BoardCode,
}

/// One board input, normalized: the extracted board plus everything the
/// downstream checks need to stay honest about what they did and did not see.
#[derive(Debug)]
pub struct NormalizedBoard {
    /// The extracted board (components + nets), whatever the input format.
    pub board: ExtractedBoard,
    /// The KiCad-parseable layout text for the geometry-bearing checks
    /// (DRC / SI): the input itself for a text format, the schematic source
    /// for a `.kicad_sch`, the COMPILED `.kicad_pcb` text for Board-as-Code.
    /// `None` for Altium (geometry lives in the raw bytes twin) and gerber
    /// (no layout file exists; the checks must say "Not checked").
    pub layout_text: Option<String>,
    /// The input file's raw bytes: [`ExtractedBoard::altium_drc`] and
    /// `apply_drc_shorts` read copper geometry from these for a binary board.
    /// Empty for a gerber DIRECTORY (there is no single file to keep).
    pub raw: Vec<u8>,
    /// What the input was recognised as.
    pub kind: InputKind,
    /// Whole-sentence coverage notes the READER produced for this input: where
    /// the connectivity came from, what the format could not state, and every
    /// cross-check inside the file that disagreed with itself.
    ///
    /// The exchange readers compute all of that and it used to stop at the
    /// extract crate's boundary, which is the same failure as not computing it:
    /// a stale CAD netlist, a wrong package reference, or a `.Z` member that
    /// would not inflate were all found, unit-tested, and invisible to the user.
    /// Surfaces render these alongside their own notes.
    pub notes: Vec<String>,
}

impl NormalizedBoard {
    /// A binary Altium board: DRC comes from the raw bytes twin, and there is
    /// no layout text. What call sites mean by `is_binary` / `altium.is_some()`.
    pub fn is_binary(&self) -> bool {
        self.kind == InputKind::Altium
    }

    /// A fab/exchange input with **no KiCad layout text**: a gerber archive, an
    /// ODB++ job or an IPC-2581 document. Clearance DRC and trace-geometry SI
    /// must report "Not checked" for these rather than a vacuous pass, which is
    /// what call sites use this for.
    ///
    /// The three differ in how connectivity was obtained (reconstructed from
    /// copper for gerbers, read from the file for the other two) but not in what
    /// the geometry-bearing checks can do, which is nothing.
    pub fn is_gerber(&self) -> bool {
        matches!(
            self.kind,
            InputKind::Gerber | InputKind::Odb | InputKind::Ipc2581
        )
    }

    /// Specifically a gerber archive, as opposed to the other two geometry-less
    /// inputs [`is_gerber`](Self::is_gerber) also covers.
    pub fn is_gerber_archive(&self) -> bool {
        self.kind == InputKind::Gerber
    }
}

/// How to name this input kind in a user-facing sentence.
///
/// The three geometry-less kinds share a capability ([`NormalizedBoard::is_gerber`])
/// but not a description, and a report that says "which a gerber archive does not
/// carry" over an ODB++ job is stating something false about where the
/// connectivity came from — on exactly the axis these readers exist to be honest
/// about.
pub fn input_kind_phrase(kind: InputKind) -> &'static str {
    match kind {
        InputKind::Gerber => "a gerber archive",
        InputKind::Odb => "an ODB++ job",
        InputKind::Ipc2581 => "an IPC-2581 document",
        InputKind::Altium => "an Altium .PcbDoc",
        InputKind::Schematic => "a schematic",
        InputKind::BoardCode => "a Board-as-Code source",
        InputKind::Text => "this input",
    }
}

/// The missing-board message. Never suggests an unrunnable command: the
/// checkout-relative example path only exists inside a hauksbee source tree;
/// from a bare binary, the embedded example is the one that always works.
fn board_not_found_message(path: &str) -> String {
    let checkout = Path::new("crates/hauksbee-ci/examples/boards/blinky.kicad_pcb");
    let suggestion = if checkout.exists() {
        "hauksbee run crates/hauksbee-ci/examples/boards/blinky.kicad_pcb --report"
    } else {
        "hauksbee run --example blinky --report"
    };
    format!("no board file at '{path}'. Check the path, or try a bundled example:\n  {suggestion}")
}

/// Why a board input could not be normalized. Variants keep the semantic
/// content; `Display` renders the CLI-facing message and [`web_message`]
/// renders the web front door's wording, so one normalizer still speaks to
/// each surface in the wording that surface needs.
///
/// [`web_message`]: BoardInputError::web_message
#[derive(Debug, thiserror::Error)]
pub enum BoardInputError {
    /// The path does not exist ([`from_path`] only).
    #[error("{}", board_not_found_message(path))]
    NotFound { path: String },
    /// The file exists but could not be read ([`from_path`] only).
    #[error("reading '{path}': {message}")]
    Io { path: String, message: String },
    /// Zip routing failed: not a readable archive, more than one `.board`
    /// inside, or (web path) the gerber reverse-extraction of the archive
    /// failed. The message already names the file and both zip forms.
    #[error("{0}")]
    Zip(String),
    /// Board-as-Code compilation failed; carries the compiler's error.
    #[error("{0}")]
    BoardCode(String),
    /// Gerber reverse-extraction from a path (directory or zip) failed.
    #[error("{0}")]
    Gerber(String),
    /// `.kicad_sch` hierarchy extraction failed ([`from_path`] only).
    #[error("{0}")]
    Schematic(String),
    /// The content matched no reader (text sniffer and binary sniffer both
    /// declined, or the matched reader failed to parse).
    #[error("{0}")]
    Extract(String),
}

/// The one supported-formats list every refusal quotes, so the set can never
/// drift between messages (it already had: a "Supported:" clause that omitted
/// Altium while the diagnostic said it tried the altium reader).
const SUPPORTED_FORMATS: &str = "KiCad .kicad_pcb / .kicad_sch / netlist, Eagle .brd, \
     Altium .PcbDoc, IPC-2581 .xml, an ODB++ job (folder / .tgz / .zip), IPC-D-356 \
     .d356, Board-as-Code .board, or a zip of gerbers / a .board export";

impl BoardInputError {
    /// The web front door's wording for this failure: what `/api/analyze`
    /// returns, phrased for someone who just dropped a file in a browser
    /// rather than for a terminal.
    pub fn web_message(&self) -> String {
        match self {
            BoardInputError::BoardCode(e) => {
                format!("Could not compile this Board-as-Code file: {e}.")
            }
            BoardInputError::Extract(e) => {
                format!("Could not read this board file: {e}. Supported: {SUPPORTED_FORMATS}.")
            }
            other => format!("Could not read this board file: {other}"),
        }
    }
}

/// Normalize an uploaded board from its raw bytes: the web path.
///
/// The loading head behind `frontdoor::analyze`. `file_name` is
/// a display/sniff hint only (the `.board` extension routes the compile path);
/// `contents` is the file's RAW bytes. Binary formats (Altium `.PcbDoc`, an
/// OLE2 container) are sniffed from the bytes first, exactly like the CLI
/// path; lossy-decoding before that point would corrupt a binary board before
/// it was ever parsed. Deliberately NO file-stem name fallback here: a
/// titleless board keeps its empty name so the web report's `board_name` does
/// not change under the normalizer.
pub fn from_bytes(file_name: &str, contents: &[u8]) -> Result<NormalizedBoard, BoardInputError> {
    // An ODB++ archive, checked before anything else because its container is a
    // zip or a gzipped tar and BOTH of those would otherwise be classified as
    // something they are not (the zip branch below treats every zip as gerbers
    // or Board-as-Code). Keyed on the `matrix/matrix` member, which no gerber
    // archive has.
    if hauksbee_extract::odbpp::looks_like_odbpp_archive(contents) {
        let out = hauksbee_extract::odbpp::from_odbpp_archive(contents)
            .map_err(|e| BoardInputError::Extract(format!("'{file_name}': {e}")))?;
        return Ok(NormalizedBoard {
            board: out.board,
            layout_text: None,
            raw: contents.to_vec(),
            kind: InputKind::Odb,
            notes: out.stats.notes(),
        });
    }

    // Binary-first: `from_auto_bytes` claims a recognised binary board (OLE2
    // magic + Altium streams) or returns None so text formats keep their exact
    // behaviour through `from_auto`.
    if let Some(binary) = ExtractedBoard::from_auto_bytes(contents) {
        let board = binary.map_err(|e| BoardInputError::Extract(format!("'{file_name}': {e}")))?;
        return Ok(NormalizedBoard {
            board,
            layout_text: None,
            raw: contents.to_vec(),
            kind: InputKind::Altium,
            notes: Vec::new(),
        });
    }

    // A zip upload is a gerber fab archive or a zipped Board-as-Code export;
    // route it before the text sniffer (which knows neither). A `.board`
    // export inside wins (a fab archive never carries one); anything else is
    // treated as a gerber job zip.
    let mut zip_code: Option<String> = None;
    if contents.starts_with(b"PK\x03\x04") {
        match zip_board_code(file_name, contents)? {
            Some(src) => zip_code = Some(src),
            None => {
                let board = gerber_from_zip_bytes(file_name, contents)?;
                return Ok(NormalizedBoard {
                    board,
                    layout_text: None,
                    raw: contents.to_vec(),
                    kind: InputKind::Gerber,
                    notes: Vec::new(),
                });
            }
        }
    }

    let text: Cow<'_, str> = match &zip_code {
        Some(src) => Cow::Borrowed(src.as_str()),
        None => String::from_utf8_lossy(contents),
    };
    // Board-as-Code (`hauksbee to-code` output, usually `*.board`, possibly
    // zipped) is a DSL, not a CAD format, so the extractor cannot sniff it.
    // Compile it to KiCad board text first; the emitted text then flows
    // through the untouched text path (parse, DRC, SI) as if a `.kicad_pcb`
    // was uploaded.
    let is_board_code = zip_code.is_some()
        || Path::new(file_name).extension().and_then(|e| e.to_str()) == Some("board")
        || crate::commands::common::is_board_code_header(&text);
    let text: String = if is_board_code {
        crate::boardcode::code_to_board_text(&text)
            .map_err(|e| BoardInputError::BoardCode(e.to_string()))?
    } else {
        text.into_owned()
    };
    // IPC-2581 is text but not KiCad-parseable text, so it must NOT be handed to
    // the geometry-bearing checks as `layout_text`: `ExtractedBoard::drc` returns
    // an empty report for content it does not recognise, and an empty report
    // renders as "no copper spacing problems found" — a vacuous green over a
    // board whose clearances were never examined. It goes through the reader
    // directly rather than through `from_auto` so the read's own coverage notes
    // survive.
    if !is_board_code && hauksbee_extract::ipc2581::looks_like_ipc2581(text.as_bytes()) {
        let out = hauksbee_extract::ipc2581::extract(&text)
            .map_err(|e| BoardInputError::Extract(format!("'{file_name}': {e}")))?;
        return Ok(NormalizedBoard {
            board: out.board,
            layout_text: None,
            raw: contents.to_vec(),
            kind: InputKind::Ipc2581,
            notes: out.stats.notes(),
        });
    }
    let board = ExtractedBoard::from_auto(&text)
        .map_err(|e| BoardInputError::Extract(format!("'{file_name}': {e}")))?;
    Ok(NormalizedBoard {
        board,
        layout_text: Some(text),
        raw: contents.to_vec(),
        kind: if is_board_code {
            InputKind::BoardCode
        } else {
            InputKind::Text
        },
        notes: Vec::new(),
    })
}

/// Normalize a board from a filesystem path: the CLI/CI path.
///
/// The loading head behind `hauksbee run`. It shares the zip
/// classification with the web path (so `run <x.zip>` with a `.board`
/// export inside compiles instead of failing as gerber), and adds what only a
/// path can give:
///
/// * gerber DIRECTORY detection (`is_dir`);
/// * `.kicad_sch` loaded via [`ExtractedBoard::from_kicad_schematic_path`] so
///   sheet hierarchies recurse into sibling files (content sniffing cannot);
/// * the file-stem name fallback: when the layout carries no title-block
///   name, `board.name` becomes the file stem so every downstream identifier
///   (JSON `board` field, report headers) is usable instead of blank. This
///   fallback lives HERE only; [`from_bytes`] must not invent a name.
pub fn from_path(path: &Path) -> Result<NormalizedBoard, BoardInputError> {
    let file_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("board")
        .to_string();

    // A directory: usually a gerber job folder, but a directory HOLDING a
    // board file is a common mistake (pointing at the project folder instead
    // of the layout inside it). Look for board files first: assuming gerbers
    // used to produce a baffling "no copper gerber layers" error right next
    // to a perfectly good .kicad_pcb.
    if path.is_dir() {
        // An UNPACKED ODB++ job, checked first: it is a directory tree whose
        // `matrix/matrix` identifies it, and it holds no board file, so without
        // this check it would fall through to the gerber path and fail with "no
        // copper gerber layers" while sitting on a complete netlist. This is the
        // one format the reader registry cannot claim, because a directory has no
        // bytes to sniff.
        if hauksbee_extract::odbpp::looks_like_odbpp_dir(path) {
            let out = hauksbee_extract::odbpp::from_odbpp(path)
                .map_err(|e| BoardInputError::Extract(format!("'{}': {e}", path.display())))?;
            let notes = out.stats.notes();
            return Ok(with_name_fallback(
                NormalizedBoard {
                    board: out.board,
                    layout_text: None,
                    raw: Vec::new(),
                    kind: InputKind::Odb,
                    notes,
                },
                path,
            ));
        }
        let boards = board_files_in(path);
        match boards.as_slice() {
            [] => {}
            [one] => {
                // Exactly one board file: use it, saying so (stderr, so report
                // and --json stdout stay clean).
                eprintln!(
                    "note: '{}' is a directory; using the board file inside it: {}",
                    path.display(),
                    one.display()
                );
                return from_path(one);
            }
            many => {
                return Err(BoardInputError::Gerber(format!(
                    "'{}' is a directory holding {} board files ({}); pass the one you mean",
                    path.display(),
                    many.len(),
                    many.iter()
                        .filter_map(|p| p.file_name().and_then(|s| s.to_str()))
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
            }
        }
        // Gerber job input, directory form: reverse-extracted from copper
        // geometry.
        let board = ExtractedBoard::from_gerber(path)
            .map_err(|e| BoardInputError::Gerber(gerber_path_message(path, &e.to_string())))?;
        if board.components.is_empty() {
            return Err(BoardInputError::Gerber(gerber_without_parts_message(
                path, &board,
            )));
        }
        return Ok(with_name_fallback(
            NormalizedBoard {
                board,
                layout_text: None,
                raw: Vec::new(),
                kind: InputKind::Gerber,
                notes: Vec::new(),
            },
            path,
        ));
    }

    // A KiCad PROJECT file (.kicad_pro settings / .kicad_prl local state) is
    // the file the OS file picker often surfaces first, but it carries no
    // layout. Name the sibling board by stem when it exists.
    let ext_lower = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    if ext_lower == "kicad_pro" || ext_lower == "kicad_prl" {
        let sibling = path.with_extension("kicad_pcb");
        let msg = if sibling.exists() {
            format!(
                "'{}' is a KiCad project file, not the board. The layout is next to it; try: {}",
                path.display(),
                sibling.display()
            )
        } else {
            format!(
                "'{}' is a KiCad project file, not the board; pass the matching \
                 .kicad_pcb (the layout) instead",
                path.display()
            )
        };
        return Err(BoardInputError::Extract(msg));
    }

    // Read raw bytes first: an Altium `.PcbDoc` is a binary OLE2 container and
    // would fail a UTF-8 read. Text formats are recovered losslessly from
    // these bytes. Keep the actionable not-found error.
    let raw = std::fs::read(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            BoardInputError::NotFound {
                path: path.display().to_string(),
            }
        } else {
            BoardInputError::Io {
                path: path.display().to_string(),
                message: e.to_string(),
            }
        }
    })?;

    // An ODB++ ARCHIVE (`.tgz`, `.tar.gz`, `.tar` or `.zip`), before the zip
    // branch below claims every zip as gerbers. Content-sniffed, so a job saved
    // under any extension still reads.
    if hauksbee_extract::odbpp::looks_like_odbpp_archive(&raw) {
        let out = hauksbee_extract::odbpp::from_odbpp_archive(&raw)
            .map_err(|e| BoardInputError::Extract(format!("'{file_name}': {e}")))?;
        let notes = out.stats.notes();
        return Ok(with_name_fallback(
            NormalizedBoard {
                board: out.board,
                layout_text: None,
                raw,
                kind: InputKind::Odb,
                notes,
            },
            path,
        ));
    }

    // A `.zip` is a gerber fab archive or a zipped Board-as-Code export, the
    // same two forms the web drop zone accepts.
    let is_zip = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("zip"));
    if is_zip {
        if let Some(src) = zip_board_code(&file_name, &raw)? {
            let compiled = crate::boardcode::code_to_board_text(&src)
                .map_err(|e| BoardInputError::BoardCode(e.to_string()))?;
            let board = ExtractedBoard::from_auto(&compiled)
                .map_err(|e| BoardInputError::Extract(format!("'{file_name}': {e}")))?;
            return Ok(with_name_fallback(
                NormalizedBoard {
                    board,
                    layout_text: Some(compiled),
                    raw,
                    kind: InputKind::BoardCode,
                    notes: Vec::new(),
                },
                path,
            ));
        }
        let board = ExtractedBoard::from_gerber(path).map_err(|e| {
            // A zip that is neither a gerber set nor a .board export is often
            // a FIRMWARE project zipped by mistake; say where firmware goes
            // instead of only rejecting.
            if let Some(marker) = zip_firmware_marker(&raw) {
                BoardInputError::Zip(firmware_zip_message(&file_name, marker))
            } else {
                BoardInputError::Gerber(gerber_path_message(path, &e.to_string()))
            }
        })?;
        if board.components.is_empty() {
            return Err(BoardInputError::Gerber(gerber_without_parts_message(
                path, &board,
            )));
        }
        return Ok(with_name_fallback(
            NormalizedBoard {
                board,
                layout_text: None,
                raw,
                kind: InputKind::Gerber,
                notes: Vec::new(),
            },
            path,
        ));
    }

    // Binary board (Altium): auto-detected from the OLE2 magic + Altium
    // streams, exactly as the Eagle path is auto-detected from XML content.
    if let Some(binary) = ExtractedBoard::from_auto_bytes(&raw) {
        let board = binary.map_err(|e| BoardInputError::Extract(format!("'{file_name}': {e}")))?;
        return Ok(with_name_fallback(
            NormalizedBoard {
                board,
                layout_text: None,
                raw,
                kind: InputKind::Altium,
                notes: Vec::new(),
            },
            path,
        ));
    }

    let text = String::from_utf8_lossy(&raw).into_owned();
    // Board-as-Code (`.board`): detected by extension or the DSL header.
    // Parse the DSL, recompile it to `.kicad_pcb` text, then feed the same
    // analysis path the layout formats use.
    let is_board_code = path.extension().and_then(|e| e.to_str()) == Some("board")
        || crate::commands::common::is_board_code_header(&text);
    if is_board_code {
        let compiled = crate::boardcode::code_to_board_text(&text)
            .map_err(|e| BoardInputError::BoardCode(e.to_string()))?;
        let board = ExtractedBoard::from_auto(&compiled)
            .map_err(|e| BoardInputError::Extract(format!("'{file_name}': {e}")))?;
        return Ok(with_name_fallback(
            NormalizedBoard {
                board,
                layout_text: Some(compiled),
                raw,
                kind: InputKind::BoardCode,
                notes: Vec::new(),
            },
            path,
        ));
    }

    // IPC-2581: text, but not layout text. Same reasoning as the web path — it
    // must not reach `ExtractedBoard::drc`, whose empty report for unrecognised
    // content would render as a clean clearance check that never ran.
    if hauksbee_extract::ipc2581::looks_like_ipc2581(&raw) {
        let out = hauksbee_extract::ipc2581::extract(&text)
            .map_err(|e| BoardInputError::Extract(format!("'{file_name}': {e}")))?;
        let notes = out.stats.notes();
        return Ok(with_name_fallback(
            NormalizedBoard {
                board: out.board,
                layout_text: None,
                raw,
                kind: InputKind::Ipc2581,
                notes,
            },
            path,
        ));
    }

    // A `.kicad_sch` may reference sub-sheets that live in sibling files, so
    // it must be loaded by path to recurse the hierarchy; everything else is
    // self-contained and sniffed from its content.
    let is_sch = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("kicad_sch"));
    if is_sch {
        let board = ExtractedBoard::from_kicad_schematic_path(path)
            .map_err(|e| BoardInputError::Schematic(e.to_string()))?;
        return Ok(with_name_fallback(
            NormalizedBoard {
                board,
                layout_text: Some(text),
                raw,
                kind: InputKind::Schematic,
                notes: Vec::new(),
            },
            path,
        ));
    }

    let board = ExtractedBoard::from_auto(&text).map_err(|e| {
        // A `Corrupt` error means the file DID parse as its format and the
        // reader is refusing content it cannot analyse truthfully. Its message
        // already explains what is wrong and what to do, so wrapping it in "did
        // not parse" would state the opposite of what happened.
        if let hauksbee_extract::ExtractError::Corrupt(msg) = &e {
            return BoardInputError::Extract(format!("'{file_name}': {msg}"));
        }
        // When the EXTENSION already names a format we support, the problem is
        // the content, not the format: say that, instead of reciting the
        // generic supported-formats list at someone holding a corrupt board.
        let known = match ext_lower.as_str() {
            "kicad_pcb" => Some("a KiCad board"),
            "brd" => Some("an Eagle board"),
            "d356" => Some("an IPC-D-356 netlist"),
            "net" => Some("a KiCad netlist"),
            _ => None,
        };
        match known {
            Some(desc) => BoardInputError::Extract(format!(
                "'{file_name}' looks like {desc} by extension, but its content \
                 did not parse: {e}"
            )),
            None => BoardInputError::Extract(format!("'{file_name}': {e}")),
        }
    })?;
    Ok(with_name_fallback(
        NormalizedBoard {
            board,
            layout_text: Some(text),
            raw,
            kind: InputKind::Text,
            notes: Vec::new(),
        },
        path,
    ))
}

/// Board-format files directly inside `dir` (no recursion), sorted. The
/// directory head of [`from_path`] consults this before assuming gerbers.
fn board_files_in(dir: &Path) -> Vec<std::path::PathBuf> {
    const BOARD_EXTS: &[&str] = &["kicad_pcb", "kicad_sch", "brd", "pcbdoc", "d356", "board"];
    let mut found: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            let ext = p
                .extension()
                .and_then(|x| x.to_str())
                .map(|x| x.to_ascii_lowercase())
                .unwrap_or_default();
            if !p.is_file() {
                continue;
            }
            if BOARD_EXTS.contains(&ext.as_str()) {
                found.push(p);
                continue;
            }
            // An IPC-2581 document is a board file, but `.xml` is far too generic
            // an extension to claim on its name alone (an Eagle library, a Maven
            // POM and a KiCad worksheet are all `.xml`). Read the head and let the
            // root element decide, the same way the reader registry does.
            if ext == "xml" && ipc2581_head(&p) {
                found.push(p);
            }
        }
    }
    found.sort();
    found
}

/// Does this file's leading 4 KiB declare an IPC-2581 document? Reads only the
/// head, so scanning a directory of large XML never reads them whole.
fn ipc2581_head(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let mut head = vec![0u8; 4096];
    match f.read(&mut head) {
        Ok(n) => hauksbee_extract::ipc2581::looks_like_ipc2581(&head[..n]),
        Err(_) => false,
    }
}

/// The CLI/CI wording for a failed gerber reverse-extraction from a path.
/// The extractor's own message may already say where to point (its "no copper
/// gerber layers" refusal does); repeating a near-identical sentence after it
/// reads as a stutter, so the guidance is added only when the inner error
/// does not already carry it.
fn gerber_path_message(path: &Path, e: &str) -> String {
    let guidance = if e.contains("point hauksbee at") {
        String::new()
    } else {
        " Point at the gerber job folder (or a .zip of it) containing the \
         copper/drill files."
            .to_string()
    };
    // Some extractor errors already name the path ("read zip <path>: ...");
    // naming it a second time in the wrapper reads as a stutter (L6).
    let shown = path.display().to_string();
    let from = if e.contains(&shown) {
        String::new()
    } else {
        format!(" from '{shown}'")
    };
    format!(
        "gerber extraction{from} failed: {e}.{guidance} See {}.",
        hauksbee_ir::docs_url("docs/ingest/GERBER.md")
    )
}

/// Refuse a gerber job that reconstructed copper but no parts, saying exactly
/// which half is missing.
///
/// A fab archive holds copper, drill and films. It does not hold a part list, so
/// the reverse extraction recovers nets and pad geometry but has nothing that
/// says which pads form which component: that lives in the pick-and-place file,
/// which most published fab folders leave out. Measured on 60 real fab folders
/// harvested from public repositories, not one shipped a P&P, so this is the
/// NORMAL outcome for a gerber job, not an exotic one.
///
/// Without this, such a job fell through to the generic empty-board refusal,
/// "this board parsed, but is empty", about a folder holding a fully routed
/// two-layer board. That sends the user looking for a corrupt file instead of
/// the one input they are actually missing.
fn gerber_without_parts_message(path: &Path, board: &ExtractedBoard) -> String {
    format!(
        "'{}' is a gerber fab job: hauksbee reconstructed {} net(s) from the copper, \
         but a fab archive carries no part list, so there is nothing to bind or \
         simulate. Add the pick-and-place file (KiCad: 'Fabrication Outputs → \
         Component Placement', a `.pos` or `.csv` of references and XY positions) \
         to the same folder and retry, or pass the original layout file \
         (.kicad_pcb / .brd / .PcbDoc) which already has both. See {}.",
        path.display(),
        board.nets.len(),
        hauksbee_ir::docs_url("docs/ingest/GERBER.md")
    )
}

/// Fall back to the source file stem when the layout carries no title-block
/// name (gerber, DSL, and many real boards ship no title). [`from_path`] only.
fn with_name_fallback(mut norm: NormalizedBoard, path: &Path) -> NormalizedBoard {
    if norm.board.name.trim().is_empty() {
        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
            norm.board.name = stem.to_string();
        }
    }
    norm
}

/// Scan a zip for a Board-as-Code export and read it out: `Ok(Some(src))`
/// when the archive holds exactly one `.board` (macOS resource-fork noise
/// skipped), `Ok(None)` when it holds none (a gerber fab archive never
/// carries one), an error when it holds several or cannot be read.
fn zip_board_code(file_name: &str, contents: &[u8]) -> Result<Option<String>, BoardInputError> {
    use std::io::Read;
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(contents)).map_err(|e| {
        BoardInputError::Zip(format!(
            "could not open '{file_name}' as a zip archive: {e}"
        ))
    })?;
    let mut code_entries: Vec<usize> = Vec::new();
    for i in 0..archive.len() {
        if let Ok(entry) = archive.by_index(i) {
            let name = entry.name();
            if name.starts_with("__MACOSX/")
                || name.rsplit('/').next().is_some_and(|f| f.starts_with('.'))
            {
                continue;
            }
            if name.to_ascii_lowercase().ends_with(".board") {
                code_entries.push(i);
            }
        }
    }
    if code_entries.len() > 1 {
        return Err(BoardInputError::Zip(format!(
            "'{file_name}' contains more than one .board file; zip (or upload) just the one you mean."
        )));
    }
    let Some(&i) = code_entries.first() else {
        return Ok(None);
    };
    let mut entry = archive.by_index(i).map_err(|e| {
        BoardInputError::Zip(format!(
            "could not read the .board inside '{file_name}': {e}"
        ))
    })?;
    let mut src = String::new();
    entry.read_to_string(&mut src).map_err(|e| {
        BoardInputError::Zip(format!(
            "could not read the .board inside '{file_name}': {e}"
        ))
    })?;
    Ok(Some(src))
}

/// Reverse-extract a gerber fab archive supplied as bytes (the web upload).
/// `from_gerber` wants a path, so the bytes are parked in a temp file.
fn gerber_from_zip_bytes(
    file_name: &str,
    contents: &[u8],
) -> Result<ExtractedBoard, BoardInputError> {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let tmp = std::env::temp_dir().join(format!(
        "hauksbee-web-gerber-{}-{}.zip",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&tmp, contents)
        .map_err(|e| BoardInputError::Zip(format!("could not stage the zip: {e}")))?;
    let result = ExtractedBoard::from_gerber(&tmp);
    let _ = std::fs::remove_file(&tmp);
    result.map_err(|e| {
        // A zip that fails the gerber read is often a FIRMWARE project zipped
        // by mistake; point at the firmware slot instead of only rejecting.
        if let Some(marker) = zip_firmware_marker(contents) {
            return BoardInputError::Zip(firmware_zip_message(file_name, marker));
        }
        BoardInputError::Zip(format!(
            "could not read '{file_name}' as a gerber archive: {e}. A board zip should \
             contain the gerber fab files (copper + drill), or one .board export."
        ))
    })
}

/// What marks a zip as a firmware project rather than a gerber fab archive:
/// a PlatformIO config, a `.pio` build tree, or a compiled `.hex` image.
/// Returns a human name for the first marker found, `None` for anything else.
fn zip_firmware_marker(contents: &[u8]) -> Option<&'static str> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(contents)).ok()?;
    for i in 0..archive.len() {
        let Ok(entry) = archive.by_index(i) else {
            continue;
        };
        let name = entry.name().to_ascii_lowercase();
        let base = name.rsplit('/').next().unwrap_or("");
        if base == "platformio.ini" {
            return Some("a platformio.ini");
        }
        if name.starts_with(".pio/") || name.contains("/.pio/") {
            return Some("a .pio build tree");
        }
        if name.ends_with(".hex") {
            return Some("compiled .hex firmware");
        }
    }
    None
}

/// The rejection wording for a firmware project uploaded where a board was
/// expected: names what was found and where firmware actually goes.
fn firmware_zip_message(file_name: &str, marker: &str) -> String {
    format!(
        "'{file_name}' looks like a firmware project ({marker} inside), not a board. \
         Drop it in the firmware slot next to the board file (CLI: pass it with \
         --firmware); the board zip should contain the gerber fab files \
         (copper + drill), or one .board export."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const KICAD: &[u8] = include_bytes!("../../hauksbee-ci/examples/boards/boot_gate.kicad_pcb");
    const ALTIUM: &[u8] = include_bytes!("../../../testdata/boards/altium_two_resistor.PcbDoc");
    /// boot_gate exported by kicad-cli 9.0.3; the same fixtures the extract
    /// crate's cross-format agreement test uses (see its module docs for
    /// provenance). boot_gate reads as 3 components / 4 nets natively.
    const ODB_ZIP: &[u8] =
        include_bytes!("../../hauksbee-extract/tests/fixtures/exchange/boot_gate.odb.zip");
    const IPC2581: &[u8] =
        include_bytes!("../../hauksbee-extract/tests/fixtures/exchange/boot_gate.ipc2581.xml");

    const DSL: &[u8] = br#"# Board-as-Code (hauksbee board DSL v1)
board version 20241229

fn main {
    net "A"
    net "B"
    comp R1 lib "Resistor_SMD:R_0402_1005Metric" val "10k" layer "F.Cu" at 0 0 rot 0 {
        pad "1" smd rect at 0 0 size 1 1 layers [F.Cu] net "A"
        pad "2" smd rect at 1 0 size 1 1 layers [F.Cu] net "B"
    }
}
"#;

    fn zip_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for (name, bytes) in entries {
            w.start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            w.write_all(bytes).unwrap();
        }
        w.finish().unwrap().into_inner()
    }

    #[test]
    fn kicad_text_normalizes_byte_identical() {
        let norm = from_bytes("boot_gate.kicad_pcb", KICAD).expect("kicad text normalizes");
        assert_eq!(norm.kind, InputKind::Text);
        assert!(!norm.is_binary() && !norm.is_gerber());
        // The layout text is the input, byte-identical: the DRC/SI checks must
        // see exactly what the user uploaded.
        assert_eq!(
            norm.layout_text.as_deref().map(str::as_bytes),
            Some(KICAD),
            "text input must pass through untouched"
        );
        assert_eq!(norm.raw, KICAD, "raw bytes preserved");
        assert!(!norm.board.components.is_empty());
    }

    #[test]
    fn zipped_board_code_compiles() {
        let bytes = zip_of(&[("export/tarski.board", DSL)]);
        let norm = from_bytes("tarski-export.zip", &bytes).expect("zipped .board normalizes");
        assert_eq!(norm.kind, InputKind::BoardCode);
        let text = norm
            .layout_text
            .as_deref()
            .expect("compiled layout text present");
        assert!(
            text.contains("kicad_pcb"),
            "layout_text is the COMPILED KiCad text, not the DSL: {}",
            &text[..text.len().min(120)]
        );
        assert_eq!(
            norm.board.components.len(),
            1,
            "R1 survives the zip + compile"
        );
    }

    #[test]
    fn bare_board_code_compiles_and_keeps_empty_name_on_bytes_path() {
        // from_bytes must NOT invent a board name: the web report's board_name
        // for a titleless board is empty, and the golden parity test in
        // frontdoor.rs depends on that staying true.
        let norm = from_bytes("tarski.board", DSL).expect(".board normalizes");
        assert_eq!(norm.kind, InputKind::BoardCode);
        assert!(
            norm.board.name.trim().is_empty(),
            "from_bytes must not apply the file-stem fallback: {:?}",
            norm.board.name
        );
        // Header sniff works without the extension too.
        let norm2 = from_bytes("exported.txt", DSL).expect("header sniff works");
        assert_eq!(norm2.kind, InputKind::BoardCode);
    }

    #[test]
    fn altium_bytes_normalize_with_raw_intact() {
        let norm = from_bytes("two_resistor.PcbDoc", ALTIUM).expect("altium normalizes");
        assert_eq!(norm.kind, InputKind::Altium);
        assert!(norm.is_binary());
        assert!(
            norm.layout_text.is_none(),
            "a binary board has no layout text"
        );
        // raw must be the EXACT container bytes: altium_drc reads copper from it.
        assert_eq!(norm.raw, ALTIUM, "raw bytes must survive for the DRC twin");
        assert_eq!(norm.board.components.len(), 2, "R1 and R2 survive");
    }

    #[test]
    fn garbage_is_a_friendly_error() {
        let err = from_bytes("nope.txt", b"this is not a board file at all")
            .expect_err("garbage must not normalize");
        let msg = err.web_message();
        assert!(
            msg.contains("Supported:") && msg.contains(".board"),
            "the web message lists the supported formats: {msg}"
        );
        assert!(
            msg.contains(".PcbDoc"),
            "the supported list must include Altium: {msg}"
        );
    }

    #[test]
    fn extract_error_names_the_file() {
        let err = from_bytes("nope.txt", b"this is not a board file at all")
            .expect_err("garbage must not normalize");
        assert!(
            err.to_string().contains("'nope.txt'"),
            "the failing file is named: {err}"
        );
    }

    #[test]
    fn empty_upload_says_the_file_is_empty() {
        let err = from_bytes("blank.kicad_pcb", b"").expect_err("empty must not normalize");
        assert!(err.to_string().contains("this file is empty"), "got: {err}");
    }

    #[test]
    fn firmware_zip_gets_pointed_at_the_firmware_slot() {
        for (label, entry) in [
            (
                "platformio",
                ("firmware/platformio.ini", b"[env:uno]".as_slice()),
            ),
            (
                "pio tree",
                (
                    "proj/.pio/build/uno/firmware.hex",
                    b":00000001FF".as_slice(),
                ),
            ),
            ("hex image", ("blink.hex", b":00000001FF".as_slice())),
        ] {
            let bytes = zip_of(&[entry]);
            let err = from_bytes("project.zip", &bytes)
                .expect_err("a firmware zip must not normalize as a board");
            let msg = err.to_string();
            assert!(
                msg.contains("firmware project") && msg.contains("firmware slot"),
                "{label}: the rejection points at the firmware path: {msg}"
            );
            assert!(
                msg.contains("--firmware"),
                "{label}: the CLI flag is named too: {msg}"
            );
        }
    }

    #[test]
    fn multi_board_zip_is_a_disambiguation_error() {
        let bytes = zip_of(&[("a.board", DSL), ("b.board", DSL)]);
        let err = from_bytes("two.zip", &bytes).expect_err("two .board files is ambiguous");
        assert!(
            err.to_string().contains("more than one .board"),
            "the error asks the user to disambiguate: {err}"
        );
    }

    #[test]
    fn junk_zip_error_names_both_zip_forms() {
        let bytes = zip_of(&[("README.md", b"not a board")]);
        let err = from_bytes("junk.zip", &bytes).expect_err("junk zip fails");
        let msg = err.to_string();
        assert!(
            msg.contains("gerber") && msg.contains(".board"),
            "error names both zip forms: {msg}"
        );
    }

    #[test]
    fn from_path_applies_the_file_stem_name_fallback() {
        let dir = std::env::temp_dir().join(format!("hauksbee-bi-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("my_widget.board");
        std::fs::write(&p, DSL).unwrap();
        let norm = from_path(&p).expect(".board file normalizes from a path");
        assert_eq!(norm.kind, InputKind::BoardCode);
        assert_eq!(
            norm.board.name, "my_widget",
            "a titleless board takes its file stem as its name on the path route"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn from_path_zip_with_a_board_export_compiles_instead_of_failing_as_gerber() {
        // Treating EVERY .zip as a gerber archive kills a zipped Board-as-Code
        // export with a gerber error. The zip classifier routes it exactly like
        // the web drop zone does.
        let dir = std::env::temp_dir().join(format!("hauksbee-bi-zip-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("export.zip");
        std::fs::write(&p, zip_of(&[("export/tarski.board", DSL)])).unwrap();
        let norm = from_path(&p).expect("a zipped .board export must compile");
        assert_eq!(norm.kind, InputKind::BoardCode);
        assert_eq!(norm.board.components.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn from_path_missing_file_keeps_the_actionable_error() {
        let err = from_path(Path::new("/definitely/not/here.kicad_pcb"))
            .expect_err("missing file errors");
        assert!(matches!(err, BoardInputError::NotFound { .. }));
        assert!(
            err.to_string().contains("no board file at"),
            "keeps the CLI's actionable wording: {err}"
        );
    }

    /// Real gerber archive through both entry points. Corpus-gated like the
    /// frontdoor test: skips when board-corpus is absent.
    #[test]
    fn gerber_zip_and_dir_normalize_as_gerber() {
        let dir = hauksbee_testkit::corpus_dir(env!("CARGO_MANIFEST_DIR"))
            .unwrap_or_default()
            .join("famous/uconsole_cm4_adapter_gerber");
        if !dir.exists() {
            if std::env::var("HAUKSBEE_REQUIRE_CORPUS").is_ok() {
                panic!("corpus required but uconsole_cm4_adapter_gerber missing");
            }
            eprintln!("skipping gerber normalizer test (corpus absent)");
            return;
        }
        // The directory form (from_path only).
        let norm = from_path(&dir).expect("gerber directory normalizes");
        assert_eq!(norm.kind, InputKind::Gerber);
        assert!(
            norm.layout_text.is_none(),
            "a gerber archive has no layout text"
        );
        assert!(
            norm.raw.is_empty(),
            "no single file to keep for a directory"
        );

        // The zipped form through the bytes path.
        let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
        for entry in std::fs::read_dir(&dir).unwrap() {
            let p = entry.unwrap().path();
            if p.is_file() {
                entries.push((
                    format!("gerbers/{}", p.file_name().unwrap().to_str().unwrap()),
                    std::fs::read(&p).unwrap(),
                ));
            }
        }
        let refs: Vec<(&str, &[u8])> = entries
            .iter()
            .map(|(n, b)| (n.as_str(), b.as_slice()))
            .collect();
        let bytes = zip_of(&refs);
        let norm = from_bytes("cm4_adapter_gerbers.zip", &bytes).expect("gerber zip normalizes");
        assert_eq!(norm.kind, InputKind::Gerber);
        assert!(norm.is_gerber());
        assert!(norm.layout_text.is_none());
        assert!(!norm.board.nets.is_empty(), "nets recovered from copper");
    }

    /// The counts boot_gate reads as through the native KiCad path, so the two
    /// exchange normalizer paths are checked against a real number and not just
    /// against "non-empty".
    const BOOT_GATE: (usize, usize) = (3, 4);

    #[test]
    fn an_odbpp_zip_normalizes_as_odb_and_not_as_gerbers() {
        // Both entry points, because the zip branch each one owns would
        // otherwise claim the archive as a gerber job and fail on its absent
        // copper films.
        let norm = from_bytes("boot_gate.odb.zip", ODB_ZIP).expect("ODB++ zip normalizes");
        assert_eq!(norm.kind, InputKind::Odb);
        assert_eq!(norm.board.components.len(), BOOT_GATE.0);
        assert_eq!(norm.board.nets.len(), BOOT_GATE.1);
        assert!(
            norm.layout_text.is_none(),
            "an ODB++ job carries no KiCad layout text"
        );
        assert!(
            norm.is_gerber() && !norm.is_gerber_archive(),
            "geometry-less like a gerber archive, but not one"
        );
        assert!(!norm.is_binary());

        let dir = std::env::temp_dir().join(format!("hauksbee-bi-odb-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("boot_gate.odb.zip");
        std::fs::write(&file, ODB_ZIP).unwrap();
        let norm = from_path(&file).expect("ODB++ zip normalizes from a path");
        assert_eq!(norm.kind, InputKind::Odb);
        assert_eq!(norm.board.components.len(), BOOT_GATE.0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unpacked_odbpp_job_directory_normalizes_instead_of_failing_as_gerbers() {
        let dir = std::env::temp_dir().join(format!("hauksbee-bi-odbdir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // Unpack the fixture the way a user who double-clicked the archive would.
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(ODB_ZIP)).unwrap();
        for i in 0..zip.len() {
            let mut f = zip.by_index(i).unwrap();
            if f.is_dir() {
                continue;
            }
            let out = dir.join("boot_gate").join(f.name());
            std::fs::create_dir_all(out.parent().unwrap()).unwrap();
            let mut buf = Vec::new();
            std::io::Read::read_to_end(&mut f, &mut buf).unwrap();
            std::fs::write(&out, buf).unwrap();
        }
        // The job root itself, and the directory holding it: both must work.
        for target in [dir.join("boot_gate"), dir.clone()] {
            let norm = from_path(&target).expect("ODB++ directory normalizes");
            assert_eq!(norm.kind, InputKind::Odb, "for {}", target.display());
            assert_eq!(norm.board.components.len(), BOOT_GATE.0);
            assert_eq!(norm.board.nets.len(), BOOT_GATE.1);
            assert!(norm.layout_text.is_none());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_ipc2581_document_normalizes_with_no_layout_text() {
        // `layout_text: None` is the load-bearing part: handing the XML to
        // `ExtractedBoard::drc` returns an EMPTY report (it recognises neither
        // KiCad nor Eagle), and an empty report renders as "no copper spacing
        // problems found" over a board nobody checked.
        let norm = from_bytes("boot_gate.ipc2581.xml", IPC2581).expect("IPC-2581 normalizes");
        assert_eq!(norm.kind, InputKind::Ipc2581);
        assert_eq!(norm.board.components.len(), BOOT_GATE.0);
        assert_eq!(norm.board.nets.len(), BOOT_GATE.1);
        assert!(norm.layout_text.is_none());
        assert!(norm.is_gerber() && !norm.is_gerber_archive());

        let dir = std::env::temp_dir().join(format!("hauksbee-bi-ipc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("boot_gate.ipc2581.xml");
        std::fs::write(&file, IPC2581).unwrap();
        let norm = from_path(&file).expect("IPC-2581 normalizes from a path");
        assert_eq!(norm.kind, InputKind::Ipc2581);
        assert!(norm.layout_text.is_none());
        // And a directory holding only it resolves to it, rather than being
        // treated as a gerber folder: `.xml` is claimed on content, not name.
        let norm = from_path(&dir).expect("the directory resolves to the document");
        assert_eq!(norm.kind, InputKind::Ipc2581);
        // An unrelated `.xml` in the same directory is NOT claimed.
        std::fs::write(
            dir.join("pom.xml"),
            b"<project><groupId>x</groupId></project>",
        )
        .unwrap();
        let norm = from_path(&dir).expect("still exactly one board file");
        assert_eq!(norm.kind, InputKind::Ipc2581);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_three_exchange_readings_of_boot_gate_agree_through_the_normalizer() {
        // The normalizer is the chokepoint every surface uses, so the agreement
        // the extract crate proves must survive it.
        let native = from_bytes("boot_gate.kicad_pcb", KICAD).expect("native");
        let odb = from_bytes("boot_gate.odb.zip", ODB_ZIP).expect("odb");
        let ipc = from_bytes("boot_gate.ipc2581.xml", IPC2581).expect("ipc");
        for other in [&odb, &ipc] {
            assert_eq!(
                other.board.components.len(),
                native.board.components.len(),
                "components"
            );
            assert_eq!(other.board.nets.len(), native.board.nets.len(), "nets");
            let pads =
                |b: &ExtractedBoard| -> usize { b.components.iter().map(|c| c.pins.len()).sum() };
            assert_eq!(pads(&other.board), pads(&native.board), "pads");
        }
    }

    #[test]
    fn an_exchange_input_carries_the_readers_own_coverage_notes() {
        // The readers compute where the connectivity came from and every
        // cross-check inside the file that disagreed. Leaving that inside the
        // extract crate is the same as not computing it.
        for (label, norm) in [
            ("ODB++", from_bytes("b.odb.zip", ODB_ZIP).expect("odb")),
            ("IPC-2581", from_bytes("b.xml", IPC2581).expect("ipc")),
        ] {
            assert!(
                !norm.notes.is_empty(),
                "{label}: the read's notes must survive normalization"
            );
            let joined = norm.notes.join(" | ");
            assert!(
                joined.contains("not reverse-engineered from copper"),
                "{label}: the note must say the netlist was READ: {joined}"
            );
            assert!(
                joined.contains("were not run"),
                "{label}: and that the geometry checks did not run: {joined}"
            );
        }
        // ODB++ from KiCad has no populate flag, so the note must say the DNP
        // state is unknown rather than let `dnp: false` read as "fitted".
        let odb = from_bytes("b.odb.zip", ODB_ZIP).expect("odb");
        assert!(
            odb.notes
                .iter()
                .any(|n| n.contains("cannot tell a do-not-populate")),
            "got: {:?}",
            odb.notes
        );
        // A native KiCad board has nothing to add, and must not gain a note.
        assert!(from_bytes("b.kicad_pcb", KICAD)
            .expect("native")
            .notes
            .is_empty());
    }

    #[test]
    fn the_input_kind_phrase_never_calls_a_non_gerber_input_a_gerber() {
        // The DRC "Not checked" verdict and the coverage note both name the
        // input; naming an ODB++ job "a gerber archive" states something false
        // about where its connectivity came from.
        assert_eq!(input_kind_phrase(InputKind::Gerber), "a gerber archive");
        assert_eq!(input_kind_phrase(InputKind::Odb), "an ODB++ job");
        assert_eq!(
            input_kind_phrase(InputKind::Ipc2581),
            "an IPC-2581 document"
        );
        for kind in [InputKind::Odb, InputKind::Ipc2581] {
            assert!(
                !input_kind_phrase(kind).contains("gerber"),
                "{kind:?} must not be described as a gerber input"
            );
        }
    }

    #[test]
    fn a_zip_that_is_neither_odbpp_nor_gerbers_still_gets_its_own_message() {
        // Adding the ODB++ branch must not swallow the existing zip diagnostics.
        let bytes = zip_of(&[("README.md", b"nothing to see" as &[u8])]);
        let err = from_bytes("mystery.zip", &bytes).expect_err("an empty zip is not a board");
        let msg = err.to_string();
        assert!(
            !msg.contains("matrix/matrix"),
            "a non-ODB++ zip must not be described as a broken ODB++ job: {msg}"
        );
    }
}
