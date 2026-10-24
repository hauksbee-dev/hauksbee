//! The unified board-input normalizer.
//!
//! Every surface that accepts "a board" (the web front door, `hauksbee run`, `hauksbee models resolve`, and
//! `hauksbee-ci`) routes through this one module, so the set of accepted
//! formats can never drift between surfaces again.
//!
//! Normalizing per surface is how format sets drift apart: a surface that
//! reads the file as UTF-8 text rejects a binary Altium board outright, and
//! one that re-reads the ORIGINAL bytes with only the text/binary sniffers
//! fails co-sim with "could not re-read the board" on a gerber zip that just
//! produced a clean static report. One normalizer, two entry points:
//!
//! * [`from_bytes`]: the web path. A file name (display/sniff hint only) plus
//!   the file's RAW bytes. Binary formats are sniffed from the bytes first, a
//!   `PK\x03\x04` prefix routes through the zip classifier, and everything
//!   else falls through to the text sniffer. It never invents a board name: a titleless board keeps its
//!   empty name here, and the web report fills `board_name` from the file
//!   stem itself.
//! * [`from_path`]: the CLI/CI path. Adds what only a path can give you:
//!   gerber DIRECTORY detection, `.kicad_sch` loaded by path so sheet
//!   hierarchies recurse into sibling files, and the file-stem fallback for a
//!   board whose layout carries no title-block name.
//!
//! Both feed the same [`NormalizedBoard`], whose [`InputKind`] disambiguates
//! the `layout_text == None` cases: an Altium board gets its DRC from the raw
//! bytes twin ([`ExtractedBoard::altium_drc`]), while a gerber archive, a fab
//! ZIP using its IPC-D-356 netlist, an ODB++ job and an IPC-2581 document have
//! no layout file at all and their DRC/SI sections say "Not checked" instead of
//! a vacuous green.
//!
//! Two of the accepted formats can only be recognised HERE, not by the extract
//! crate's reader registry, because the registry claims a format from bytes:
//!
//! * an **unpacked ODB++ job** is a directory tree, so it is detected by the
//!   presence of `matrix/matrix` under the path before the directory is assumed
//!   to be a gerber folder;
//! * an **ODB++ archive** is a `.zip` or a `.tgz`, containers the zip branch
//!   would otherwise classify as gerbers, so its content sniff runs first.

// This module reads board files the user did not write, and `hauksbee serve`
// exposes it to a browser, so a panic here is a denial of service rather than
// a crash in a CLI. Failures must be typed errors that the caller can report.
// Test code below is exempt: an unwrap in a test is an assertion.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use hauksbee_ir::eprintln;
use std::borrow::Cow;
use std::path::Path;

use hauksbee_extract::{ExtractError, ExtractedBoard};

/// What kind of input the normalizer recognised.
///
/// Call sites use this where they would otherwise keep `is_binary` / `is_gerber`
/// flags; the `layout_text == None` kinds (Altium, Gerber, Odb, Ipc2581, Easyeda) need
/// different DRC handling from the text ones and from each other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputKind {
    /// A text layout/netlist format (`KiCad` `.kicad_pcb`, Eagle `.brd`,
    /// IPC-D-356, `.net`), sniffed from content.
    Text,
    /// A `KiCad` schematic loaded BY PATH so its sheet hierarchy recurses
    /// ([`from_path`] only; [`from_bytes`] content-sniffs a lone sheet as
    /// [`InputKind::Text`] because it cannot see sibling files).
    Schematic,
    /// A binary Altium `.PcbDoc` (OLE2 container), parsed from raw bytes.
    Altium,
    /// A gerber fab archive (directory or zip), reverse-extracted from copper.
    Gerber,
    /// A fab archive whose IPC-D-356 member supplied authoritative
    /// connectivity, so copper reconstruction was deliberately not used.
    Ipc356Archive,
    /// An ODB++ job (directory, `.tgz` or `.zip`). Like a gerber archive it has
    /// no `KiCad` layout text, but unlike one its connectivity is READ from the
    /// job's own EDA data rather than reconstructed from copper.
    Odb,
    /// An IPC-2581 (DPMX) design-exchange document. Text, but not KiCad-parseable
    /// text, so it carries no `layout_text` either.
    Ipc2581,
    /// An EasyEDA (JLCEDA) board: a Standard `.json` PCB, a Pro `.epcb`, or a
    /// project archive of either. The netlist is READ from the file's own part
    /// and pad records; copper is counted but not kept, so there is no layout text.
    Easyeda,
}

/// One board input, normalized: the extracted board plus everything the
/// downstream checks need to stay honest about what they did and did not see.
#[derive(Debug)]
pub struct NormalizedBoard {
    /// The extracted board (components + nets), whatever the input format.
    pub board: ExtractedBoard,
    /// The KiCad-parseable layout text for the geometry-bearing checks
    /// (DRC / SI): the input itself for a text format, the schematic source
    /// for a `.kicad_sch`.
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
    /// Carried out of the extract crate, because stopping at its boundary is
    /// the same failure as not computing them: a stale CAD netlist, a wrong
    /// package reference, or a `.Z` member that would not inflate would all be
    /// found and then invisible to the user. Surfaces render these alongside
    /// their own notes.
    pub notes: Vec<String>,
}

impl NormalizedBoard {
    /// The board with its parts and nets in canonical order
    /// ([`ExtractedBoard::canonicalize`]), so what the report says does not
    /// depend on the order the input file listed them in. Every loader above
    /// ends here, which makes the CLI, CI and the web front door agree.
    #[must_use]
    fn canonical(mut self) -> Self {
        self.board.canonicalize();
        self.notes.extend(cut_oversize_fields(&mut self.board));
        self
    }

    /// A binary Altium board: DRC comes from the raw bytes twin, and there is
    /// no layout text. What call sites mean by `is_binary` / `altium.is_some()`.
    #[must_use]
    pub fn is_binary(&self) -> bool {
        self.kind == InputKind::Altium
    }

    /// A fab/exchange input with **no `KiCad` layout text**: a gerber archive, a
    /// fab ZIP using IPC-D-356 connectivity, an ODB++ job or an IPC-2581
    /// document. Clearance DRC and trace-geometry SI must report "Not checked"
    /// for these rather than a vacuous pass, which is what call sites use this
    /// for.
    ///
    /// The three differ in how connectivity was obtained (reconstructed from
    /// copper for gerbers, read from the file for the other two) but not in what
    /// the geometry-bearing checks can do, which is nothing.
    #[must_use]
    pub const fn is_gerber(&self) -> bool {
        matches!(
            self.kind,
            InputKind::Gerber
                | InputKind::Ipc356Archive
                | InputKind::Odb
                | InputKind::Ipc2581
                | InputKind::Easyeda
        )
    }

    /// Specifically a gerber archive, as opposed to the other two geometry-less
    /// inputs [`is_gerber`](Self::is_gerber) also covers.
    #[must_use]
    pub fn is_gerber_archive(&self) -> bool {
        self.kind == InputKind::Gerber
    }
}

/// Longest part field (value, library id, footprint, designator) the run reads.
///
/// A real value or footprint name is a few dozen bytes; the model library
/// already matches only the first [`hauksbee_models::matcher::MAX_MATCH_FIELD_BYTES`]
/// of any field, but every other consumer (value parsing, class guesses,
/// report echoes) would read, copy and print all of a megabyte. Past this
/// length nothing more can identify a part, so the field is cut once, here.
const MAX_PART_FIELD_BYTES: usize = hauksbee_models::matcher::MAX_MATCH_FIELD_BYTES;
/// Longest value of a part's extra property (datasheet link, description, order code).
const MAX_PROPERTY_BYTES: usize = 4096;
/// The mark a cut field ends in. It is no unit or magnitude, so a cut value
/// reads as unparseable, which is what a megabyte of text in a value field is.
const CUT_MARK: char = '\u{2026}';

/// `text` cut to at most `limit` bytes (on a character boundary) plus [`CUT_MARK`];
/// `false` when it was already short enough.
fn cut_text(text: &mut String, limit: usize) -> bool {
    if text.len() <= limit {
        return false;
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text.push(CUT_MARK);
    true
}

/// Cut every part field past its limit and say so: one note for the board,
/// naming how many fields were cut and the first part, so the verdict does not
/// pretend it read text it did not.
fn cut_oversize_fields(board: &mut ExtractedBoard) -> Option<String> {
    let (mut cut, mut first) = (0usize, None::<String>);
    for comp in &mut board.components {
        let mut here = 0;
        for field in [
            &mut comp.value,
            &mut comp.lib_id,
            &mut comp.footprint,
            &mut comp.reference,
        ] {
            here += usize::from(cut_text(field, MAX_PART_FIELD_BYTES));
        }
        for (_, value) in &mut comp.properties {
            here += usize::from(cut_text(value, MAX_PROPERTY_BYTES));
        }
        if here > 0 {
            first.get_or_insert_with(|| comp.reference.chars().take(40).collect());
            cut += here;
        }
    }
    let first = first?;
    Some(format!(
        "{cut} part field(s) were longer than {MAX_PART_FIELD_BYTES} bytes, far past any real value or \
         footprint name, and were read only up to that length (first: part {first}); \
         the text after it is not evidence of what the part is"
    ))
}

/// How to name this input kind in a user-facing sentence.
///
/// The three geometry-less kinds share a capability ([`NormalizedBoard::is_gerber`])
/// but not a description, and a report that says "which a gerber archive does not
/// carry" over an ODB++ job is stating something false about where the
/// connectivity came from — on exactly the axis these readers exist to be honest
/// about.
#[must_use]
pub const fn input_kind_phrase(kind: InputKind) -> &'static str {
    match kind {
        InputKind::Gerber => "a gerber archive",
        InputKind::Ipc356Archive => "a fab archive carrying an IPC-D-356 netlist",
        InputKind::Odb => "an ODB++ job",
        InputKind::Ipc2581 => "an IPC-2581 document",
        InputKind::Easyeda => "an EasyEDA board",
        InputKind::Altium => "an Altium .PcbDoc",
        InputKind::Schematic => "a schematic",
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

/// Why a board input could not be normalized.
///
/// Variants keep the semantic content; `Display` renders the CLI-facing message and [`web_message`]
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
    /// Zip routing failed: not a readable archive, or (web path) the gerber
    /// reverse-extraction of the archive failed. The message already names the file and both zip forms.
    #[error("{0}")]
    Zip(String),
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
    /// The content was RECOGNISED as a format hauksbee deliberately does not
    /// read, and the message already names it and the action that unlocks the
    /// file. Distinct from [`BoardInputError::Extract`] purely so the web
    /// wording does not append the accepted-format list to it: that list is the
    /// answer for a file nobody could identify, and beside a refusal that has
    /// just said "re-save this in Eagle 6" it reads as a contradiction.
    #[error("{0}")]
    Unsupported(String),
}

/// Which intake a format goes through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormatRoute {
    /// Read by the board analysis (`/api/analyze`, `hauksbee run`).
    Board,
    /// A SPICE deck, run by `hauksbee sim` / `/api/sim` instead of analysed.
    Deck,
}

/// One row of the accepted-formats table.
#[derive(Debug, Clone, Copy)]
pub struct AcceptedFormat {
    /// The ECAD tool or standard that produces the file.
    pub vendor: &'static str,
    /// Extensions with the dot, in the casing a user would recognise. An
    /// extension two rows claim (`.net`: a KiCad netlist and a SPICE deck) is
    /// told apart by content, never by name.
    pub exts: &'static [&'static str],
    pub route: FormatRoute,
    /// Offered in the file picker but not advertised in prose: an extension
    /// that is only sometimes a board (an Excellon drill file named `.txt`).
    pub quiet: bool,
}

const fn board_format(vendor: &'static str, exts: &'static [&'static str]) -> AcceptedFormat {
    AcceptedFormat {
        vendor,
        exts,
        route: FormatRoute::Board,
        quiet: false,
    }
}

/// The one accepted-formats table.
///
/// Every refusal quotes it
/// ([`supported_formats_sentence`]) and the web app reads it from
/// `/api/startup` (`board_formats`) for its file picker, its drop-card copy and
/// its client-side extension precheck, so the set cannot drift between the
/// engine's messages and the browser (it already had: a "Supported:" clause
/// that omitted Altium while the diagnostic said it tried that reader, and
/// `.net` read as KiCad here and as SPICE in the browser). The reader
/// registry itself sniffs content, not names, so this table is the only
/// extension list.
pub const ACCEPTED_FORMATS: &[AcceptedFormat] = &[
    board_format("KiCad", &[".kicad_pcb", ".kicad_sch", ".net"]),
    board_format("Eagle", &[".brd"]),
    board_format("Altium", &[".PcbDoc"]),
    board_format("EasyEDA", &[".json", ".epcb", ".epro"]),
    board_format("IPC-2581", &[".xml"]),
    board_format("IPC-D-356", &[".d356"]),
    board_format("ODB++ job", &[".tgz", ".tar.gz", ".tar"]),
    board_format("fab archive (gerbers or ODB++)", &[".zip"]),
    AcceptedFormat {
        vendor: "SPICE deck",
        exts: &[".cir", ".sp", ".net"],
        route: FormatRoute::Deck,
        quiet: false,
    },
    AcceptedFormat {
        vendor: "Excellon drill",
        exts: &[".txt"],
        route: FormatRoute::Board,
        quiet: true,
    },
];

/// Compiled firmware, which the web board zone re-routes to the firmware jack
/// rather than refusing.
pub const FIRMWARE_EXTS: &[&str] = &[".elf", ".hex"];

/// The board formats as one sentence fragment for a refusal.
#[must_use]
pub fn supported_formats_sentence() -> String {
    ACCEPTED_FORMATS
        .iter()
        .filter(|f| !f.quiet && f.route == FormatRoute::Board)
        .map(|f| format!("{} {}", f.vendor, f.exts.join(" / ")))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The table as the JSON the web app reads (`board_formats` in `/api/startup`).
#[must_use]
pub fn accepted_formats_json() -> serde_json::Value {
    serde_json::json!({
        "formats": ACCEPTED_FORMATS.iter().map(|f| serde_json::json!({
            "vendor": f.vendor,
            "exts": f.exts,
            "route": match f.route { FormatRoute::Board => "board", FormatRoute::Deck => "deck" },
            "quiet": f.quiet,
        })).collect::<Vec<_>>(),
        "firmware_exts": FIRMWARE_EXTS,
    })
}

impl BoardInputError {
    /// The web front door's wording for this failure: what `/api/analyze`
    /// returns, phrased for someone who just dropped a file in a browser
    /// rather than for a terminal.
    #[must_use]
    pub fn web_message(&self) -> String {
        match self {
            Self::Extract(e) => {
                format!(
                    "Could not read this board file: {e}. Supported: {}.",
                    supported_formats_sentence()
                )
            }
            other => format!("Could not read this board file: {other}"),
        }
    }
}

/// Refuse a pre-Eagle-6 binary drawing, on the RAW bytes, before anything
/// decodes or sniffs them.
///
/// Both entry points hand the text sniffer `String::from_utf8_lossy`, which is
/// lossless for every text format and destructive here: the header is
/// `10 80 64 00`, and that `0x80` becomes U+FFFD before any detector sees it.
/// Two things go wrong downstream if this check is not made first.
///
/// * The refusal loses its diagnosis. A file that is one re-save away from
///   being readable gets the generic accepted-formats recital, which sends its
///   owner looking for a tool that does not exist.
/// * Worse, the file can be CLAIMED. The IPC-D-356 reader is detected by any
///   line beginning `317`/`327`/`367` anywhere in the input, and binary board
///   records can contain those three bytes after a newline. Such a file parsed
///   as a fab netlist and came back as a board report with invented parts —
///   confident nonsense over a file nothing opened, which is the one outcome
///   this format's refusal exists to prevent.
///
/// So it runs ahead of reader selection rather than as a nicer message for an
/// `Unrecognized` result.
fn refuse_eagle_binary(file_name: &str, raw: &[u8]) -> Result<(), BoardInputError> {
    unsupported_format_refusal(file_name, raw)
        .map_or(Ok(()), |message| Err(BoardInputError::Unsupported(message)))
}

/// The refusal for a format hauksbee RECOGNISES and deliberately does not read,
/// when `raw` is one of them.
///
/// Exposed because some surfaces read a board's text for themselves rather
/// than through [`from_path`]. They decode UTF-8 directly, so without this
/// they answered a pre-Eagle-6 board with "stream did not contain valid UTF-8"
/// or "this board file is binary (e.g. Altium .`PcbDoc`)": one meaningless, one
/// naming the wrong vendor, and neither telling the user the one thing that
/// would work.
#[must_use]
pub fn unsupported_format_refusal(file_name: &str, raw: &[u8]) -> Option<String> {
    if hauksbee_extract::looks_like_eagle_binary(raw) {
        return Some(format!(
            "'{file_name}': {}",
            hauksbee_extract::eagle_binary_message()
        ));
    }
    hauksbee_extract::easyeda::looks_like_easyeda_pro_v3(raw).then(|| {
        format!(
            "'{file_name}': {}",
            hauksbee_extract::easyeda::PRO_V3_MESSAGE
        )
    })
}

/// Normalize an uploaded board from its raw bytes: the web path.
///
/// The loading head behind `frontdoor::analyze`. `file_name` is
/// a display/sniff hint only; `contents` is the file's RAW bytes. Binary formats (Altium `.PcbDoc`, an
/// OLE2 container) are sniffed from the bytes first, exactly like the CLI
/// path; lossy-decoding before that point would corrupt a binary board before
/// it was ever parsed. Deliberately NO file-stem name fallback here: a
/// titleless board keeps its empty name, and the web report applies the
/// file-stem fallback when it renders `board_name`.
///
/// # Errors
///
/// Returns a [`BoardInputError`] when the bytes are a binary Eagle board, a
/// zip that cannot be routed, or content
/// no reader recognises or can parse.
pub fn from_bytes(file_name: &str, contents: &[u8]) -> Result<NormalizedBoard, BoardInputError> {
    from_bytes_in_file_order(file_name, contents).map(NormalizedBoard::canonical)
}

#[allow(clippy::too_many_lines)] // the format-dispatch ladder, in precedence order
fn from_bytes_in_file_order(
    file_name: &str,
    contents: &[u8],
) -> Result<NormalizedBoard, BoardInputError> {
    refuse_eagle_binary(file_name, contents)?;

    // An ODB++ archive, checked before anything else because its container is a
    // zip or a gzipped tar and BOTH of those would otherwise be classified as
    // something they are not (the zip branch below treats every zip as gerbers).
    // Keyed on the `matrix/matrix` member, which no gerber
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

    // An EasyEDA board or project archive, content-sniffed ahead of the zip
    // branch below (which would call any zip gerbers) and the text sniffer.
    if let Some(norm) = easyeda_input(file_name, contents, None)? {
        return Ok(norm);
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

    // A zip upload is a fab archive; route it before the text sniffer (which
    // does not know zips). Within a fab archive an IPC-D-356 netlist wins over
    // gerber inference; only the remaining zips are reverse-extracted from
    // copper.
    if contents.starts_with(b"PK\x03\x04") {
        if let Some(norm) = ipc356_from_fab_zip(file_name, contents)? {
            return Ok(norm);
        }
        let out = gerber_from_zip_bytes(file_name, contents)?;
        let notes = out.stats.coverage_notes();
        return Ok(NormalizedBoard {
            board: out.board,
            layout_text: None,
            raw: contents.to_vec(),
            kind: InputKind::Gerber,
            notes,
        });
    }

    let text: Cow<'_, str> = String::from_utf8_lossy(contents);
    // IPC-2581 is text but not KiCad-parseable text, so it must NOT be handed to
    // the geometry-bearing checks as `layout_text`: `ExtractedBoard::drc` returns
    // an empty report for content it does not recognise, and an empty report
    // renders as "no copper spacing problems found" — a vacuous green over a
    // board whose clearances were never examined. It goes through the reader
    // directly rather than through `from_auto` so the read's own coverage notes
    // survive.
    if hauksbee_extract::ipc2581::looks_like_ipc2581(text.as_bytes()) {
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
        layout_text: Some(text.into_owned()),
        raw: contents.to_vec(),
        kind: InputKind::Text,
        notes: Vec::new(),
    })
}

/// Normalize a board from a filesystem path: the CLI/CI path.
///
/// The loading head behind `hauksbee run`. It shares the zip
/// classification with the web path, and adds what only a path can give:
///
/// * gerber DIRECTORY detection (`is_dir`);
/// * `.kicad_sch` loaded via [`ExtractedBoard::from_kicad_schematic_path`] so
///   sheet hierarchies recurse into sibling files (content sniffing cannot);
/// * the file-stem name fallback: when the layout carries no title-block
///   name, `board.name` becomes the file stem so every downstream identifier
///   (JSON `board` field, report headers) is usable instead of blank. This
///   fallback lives HERE only; [`from_bytes`] must not invent a name.
///
/// # Errors
///
/// Returns [`BoardInputError::NotFound`] or [`BoardInputError::Io`] when the
/// path cannot be read, and the same format errors as [`from_bytes`] plus
/// gerber-directory and `.kicad_sch` hierarchy failures.
pub fn from_path(path: &Path) -> Result<NormalizedBoard, BoardInputError> {
    from_path_in_file_order(path).map(NormalizedBoard::canonical)
}

#[allow(clippy::too_many_lines)] // the format-dispatch ladder, in precedence order
fn from_path_in_file_order(path: &Path) -> Result<NormalizedBoard, BoardInputError> {
    let file_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("board")
        .to_string();

    // A directory: usually a gerber job folder, but a directory HOLDING a
    // board file is a common mistake (pointing at the project folder instead
    // of the layout inside it). Look for board files first, so a project folder
    // does not draw a baffling "no copper gerber layers" error right next to a
    // perfectly good .kicad_pcb.
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
            return Ok(normalized(
                path,
                out.board,
                None,
                Vec::new(),
                InputKind::Odb,
                notes,
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
        let out = ExtractedBoard::from_gerber_with_stats(path)
            .map_err(|e| BoardInputError::Gerber(gerber_path_message(path, &e.to_string())))?;
        return gerber_job(path, out, Vec::new());
    }

    // A KiCad PROJECT file (.kicad_pro settings / .kicad_prl local state) is
    // the file the OS file picker often surfaces first, but it carries no
    // layout. The project names its design by stem: open the sibling layout
    // (or, without one, the root schematic), saying so, exactly as a
    // directory holding one board file is opened.
    let ext_lower = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    if ext_lower == "kicad_pro" || ext_lower == "kicad_prl" {
        let design = ["kicad_pcb", "kicad_sch"]
            .into_iter()
            .map(|ext| path.with_extension(ext))
            .find(|p| p.is_file());
        let Some(design) = design else {
            return Err(BoardInputError::Extract(format!(
                "'{}' is a KiCad project file, not the board, and no .kicad_pcb or \
                 .kicad_sch of the same name is next to it; pass the layout (.kicad_pcb) \
                 or the root schematic instead",
                path.display()
            )));
        };
        eprintln!(
            "note: '{}' is a KiCad project file; using its design next to it: {}",
            path.display(),
            design.display()
        );
        return from_path(&design);
    }

    // Read raw bytes first: an Altium `.PcbDoc` is a binary OLE2 container and
    // would fail a UTF-8 read. Text formats are recovered losslessly from
    // these bytes. Keep the actionable not-found error.
    let raw = hauksbee_extract::read_regular_file(path).map_err(|e| {
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

    refuse_eagle_binary(&file_name, &raw)?;

    // An assembly manifest: several boards that mate through connectors, read
    // as the one product they make.
    if ext_lower == "toml" {
        if let Ok(text) = std::str::from_utf8(&raw) {
            if crate::board_assembly::is_manifest(text) {
                return crate::board_assembly::from_manifest(path, text);
            }
        }
    }

    // An ODB++ ARCHIVE (`.tgz`, `.tar.gz`, `.tar` or `.zip`), before the zip
    // branch below claims every zip as gerbers. Content-sniffed, so a job saved
    // under any extension still reads.
    if hauksbee_extract::odbpp::looks_like_odbpp_archive(&raw) {
        let out = hauksbee_extract::odbpp::from_odbpp_archive(&raw)
            .map_err(|e| BoardInputError::Extract(format!("'{file_name}': {e}")))?;
        let notes = out.stats.notes();
        return Ok(normalized(
            path,
            out.board,
            None,
            raw,
            InputKind::Odb,
            notes,
        ));
    }

    // An EasyEDA board or project archive, ahead of the zip branch below.
    if let Some(norm) = easyeda_input(&file_name, &raw, Some(path))? {
        return Ok(norm);
    }

    // A `.zip` is a fab archive, the same form the web drop zone accepts.
    // Within it, prefer its own IPC-D-356 connectivity before falling back to
    // gerber reconstruction.
    let is_zip = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("zip"));
    if is_zip {
        if let Some(norm) = ipc356_from_fab_zip(&file_name, &raw)? {
            return Ok(with_name_fallback(norm, path));
        }
        let out = ExtractedBoard::from_gerber_with_stats(path).map_err(|e| {
            // A zip that is not a gerber set is often
            // a FIRMWARE project zipped by mistake; say where firmware goes
            // instead of only rejecting.
            zip_firmware_marker(&raw).map_or_else(
                || BoardInputError::Gerber(gerber_path_message(path, &e.to_string())),
                |marker| BoardInputError::Zip(firmware_zip_message(&file_name, marker)),
            )
        })?;
        return gerber_job(path, out, raw);
    }

    // Binary board (Altium): auto-detected from the OLE2 magic + Altium
    // streams, exactly as the Eagle path is auto-detected from XML content.
    if let Some(binary) = ExtractedBoard::from_auto_bytes(&raw) {
        let board = binary.map_err(|e| BoardInputError::Extract(format!("'{file_name}': {e}")))?;
        return Ok(normalized(
            path,
            board,
            None,
            raw,
            InputKind::Altium,
            Vec::new(),
        ));
    }

    let text = String::from_utf8_lossy(&raw).into_owned();

    // IPC-2581: text, but not layout text. Same reasoning as the web path — it
    // must not reach `ExtractedBoard::drc`, whose empty report for unrecognised
    // content would render as a clean clearance check that never ran.
    if hauksbee_extract::ipc2581::looks_like_ipc2581(&raw) {
        let out = hauksbee_extract::ipc2581::extract(&text)
            .map_err(|e| BoardInputError::Extract(format!("'{file_name}': {e}")))?;
        let notes = out.stats.notes();
        return Ok(normalized(
            path,
            out.board,
            None,
            raw,
            InputKind::Ipc2581,
            notes,
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
        // A sub-sheet file that cannot be read is left out and said so: its
        // notes ride the reader-notes channel to the verdict as unchecked scope.
        let (board, notes) = ExtractedBoard::from_kicad_schematic_path_with_notes(path)
            .map_err(|e| BoardInputError::Schematic(e.to_string()))?;
        return Ok(normalized(
            path,
            board,
            Some(text),
            raw,
            InputKind::Schematic,
            notes,
        ));
    }

    let mut board = ExtractedBoard::from_auto(&text).map_err(|e| {
        // A `Corrupt` error means the file DID parse as its format and the
        // reader is refusing content it cannot analyse truthfully. Its message
        // already explains what is wrong and what to do, so wrapping it in "did
        // not parse" would state the opposite of what happened.
        if let ExtractError::Corrupt(msg) = &e {
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
            "epcb" | "epro" => Some("an EasyEDA board"),
            _ => None,
        };
        known.map_or_else(
            || BoardInputError::Extract(format!("'{file_name}': {e}")),
            |desc| {
                BoardInputError::Extract(format!(
                    "'{file_name}' looks like {desc} by extension, but its content \
                 did not parse: {e}"
                ))
            },
        )
    })?;
    let mut notes = Vec::new();
    match ext_lower.as_str() {
        "kicad_pcb" => {
            let sch = path.with_extension("kicad_sch");
            board.adopt_schematic_pin_names(&sch);
            notes.extend(board.adopt_schematic_dnp(&sch));
        }
        "brd" => board.adopt_schematic_pin_names(&path.with_extension("sch")),
        _ => {}
    }
    Ok(normalized(
        path,
        board,
        Some(text),
        raw,
        InputKind::Text,
        notes,
    ))
}

/// An EasyEDA board or project archive in `raw`, or `None` when it is neither.
/// `path` is `Some` for the CLI path, which names the board after its file when
/// the document carries no title.
fn easyeda_input(
    file_name: &str,
    raw: &[u8],
    path: Option<&Path>,
) -> Result<Option<NormalizedBoard>, BoardInputError> {
    let out = if hauksbee_extract::easyeda::looks_like_easyeda_archive(raw) {
        hauksbee_extract::easyeda::extract_archive(raw)
    } else if hauksbee_extract::easyeda::looks_like_easyeda(raw) {
        hauksbee_extract::easyeda::extract(raw)
    } else {
        return Ok(None);
    }
    .map_err(|e| BoardInputError::Extract(format!("'{file_name}': {e}")))?;
    let norm = NormalizedBoard {
        board: out.board,
        layout_text: None,
        raw: raw.to_vec(),
        kind: InputKind::Easyeda,
        notes: out.stats.notes(),
    };
    Ok(Some(match path {
        Some(p) => with_name_fallback(norm, p),
        None => norm,
    }))
}

/// Board-format files directly inside `dir` (no recursion), sorted. The
/// directory head of [`from_path`] consults this before assuming gerbers.
fn board_files_in(dir: &Path) -> Vec<std::path::PathBuf> {
    const BOARD_EXTS: &[&str] = &[
        "kicad_pcb",
        "kicad_sch",
        "brd",
        "pcbdoc",
        "d356",
        "epcb",
        "epro",
    ];
    let mut found: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            let ext = p
                .extension()
                .and_then(|x| x.to_str())
                .map(str::to_ascii_lowercase)
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
                continue;
            }
            // Likewise `.json`: only an EasyEDA Standard PCB document counts.
            if ext == "json" && easyeda_head(&p) {
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
    // Opening a FIFO with no writer blocks forever; only a regular file is a board.
    if !std::fs::metadata(path).is_ok_and(|m| m.is_file()) {
        return false;
    }
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let mut head = vec![0u8; 4096];
    f.read(&mut head)
        .is_ok_and(|n| hauksbee_extract::ipc2581::looks_like_ipc2581(&head[..n]))
}

/// Does this file's leading bytes declare an EasyEDA PCB document?
fn easyeda_head(path: &Path) -> bool {
    use std::io::Read;
    if !std::fs::metadata(path).is_ok_and(|m| m.is_file()) {
        return false;
    }
    let Ok(f) = std::fs::File::open(path) else {
        return false;
    };
    let mut head = Vec::new();
    f.take(64 * 1024).read_to_end(&mut head).is_ok()
        && hauksbee_extract::easyeda::looks_like_easyeda(&head)
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

/// A gerber job's normalized input, or, when it reconstructed copper but no
/// parts, a refusal saying exactly which half is missing.
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
fn gerber_job(
    path: &Path,
    out: hauksbee_extract::gerber::GerberExtraction,
    raw: Vec<u8>,
) -> Result<NormalizedBoard, BoardInputError> {
    let (board, notes) = (out.board, out.stats.coverage_notes());
    if !board.components.is_empty() {
        return Ok(normalized(path, board, None, raw, InputKind::Gerber, notes));
    }
    Err(BoardInputError::Gerber(format!(
        "'{}' is a gerber fab job: hauksbee reconstructed {} net(s) from the copper, \
         but a fab archive carries no part list, so there is nothing to bind or \
         simulate. Add the pick-and-place file (KiCad: 'Fabrication Outputs → \
         Component Placement', a `.pos` or `.csv` of references and XY positions) \
         to the same folder and retry, or pass the original layout file \
         (.kicad_pcb / .brd / .PcbDoc) which already has both. See {}.",
        path.display(),
        board.nets.len(),
        hauksbee_ir::docs_url("docs/ingest/GERBER.md")
    )))
}

/// Fall back to the source file stem when the layout carries no title-block
/// name (gerber and many real boards ship no title). [`from_path`] only.
/// One resolved input: the normalized board with the file-stem name fallback
/// applied, so a titleless export still carries a name.
fn normalized(
    path: &Path,
    board: ExtractedBoard,
    layout_text: Option<String>,
    raw: Vec<u8>,
    kind: InputKind,
    notes: Vec<String>,
) -> NormalizedBoard {
    with_name_fallback(
        NormalizedBoard {
            board,
            layout_text,
            raw,
            kind,
            notes,
        },
        path,
    )
}

fn with_name_fallback(mut norm: NormalizedBoard, path: &Path) -> NormalizedBoard {
    if norm.board.name.trim().is_empty() {
        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
            norm.board.name = stem.to_string();
        }
    }
    norm
}

/// Prefer an IPC-D-356 netlist carried inside a fab ZIP over inferring the
/// same package's connectivity from copper geometry.
///
/// This mirrors the directory path's `board_files_in` priority. The archive
/// bytes remain the primary input for evidence hashing; the member supplies
/// the connectivity answer and is named in a reader note. Multiple netlists
/// are ambiguous and refuse instead of choosing by ZIP order.
fn ipc356_from_fab_zip(
    file_name: &str,
    contents: &[u8],
) -> Result<Option<NormalizedBoard>, BoardInputError> {
    let reader = std::io::Cursor::new(contents);
    let mut archive = zip::ZipArchive::new(reader).map_err(|e| {
        BoardInputError::Zip(format!(
            "could not open '{file_name}' as a zip archive: {e}"
        ))
    })?;
    let mut candidates: Vec<(usize, String)> = Vec::new();
    for i in 0..archive.len() {
        let Ok(entry) = archive.by_index(i) else {
            continue;
        };
        if entry.is_dir() {
            continue;
        }
        let name = entry.name().to_string();
        if name.starts_with("__MACOSX/")
            || name.rsplit('/').next().is_some_and(|f| f.starts_with('.'))
        {
            continue;
        }
        if Path::new(&name)
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("d356"))
        {
            candidates.push((i, name));
        }
    }
    if candidates.len() > 1 {
        return Err(BoardInputError::Zip(format!(
            "'{file_name}' contains more than one IPC-D-356 netlist ({}); keep only the one that belongs to this fab package",
            candidates
                .iter()
                .map(|(_, name)| name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    let Some((index, member_name)) = candidates.pop() else {
        return Ok(None);
    };
    let mut entry = archive.by_index(index).map_err(|e| {
        BoardInputError::Zip(format!(
            "could not read IPC-D-356 member '{member_name}' from '{file_name}': {e}"
        ))
    })?;
    let member = hauksbee_extract::read_member_capped(&mut entry).map_err(|e| {
        BoardInputError::Zip(format!(
            "could not read IPC-D-356 member '{member_name}' from '{file_name}': {e}"
        ))
    })?;
    let board = ExtractedBoard::from_ipc_d356(&String::from_utf8_lossy(&member))
        .map_err(|e| BoardInputError::Extract(format!("'{member_name}' in '{file_name}': {e}")))?;
    Ok(Some(NormalizedBoard {
        board,
        layout_text: None,
        raw: contents.to_vec(),
        kind: InputKind::Ipc356Archive,
        notes: vec![format!(
            "Fab archive connectivity was read from its IPC-D-356 netlist '{member_name}'; gerber copper geometry was not used to infer nets."
        )],
    }))
}

/// Reverse-extract a gerber fab archive supplied as bytes (the web upload).
/// `from_gerber` wants a path, so the bytes are parked in a temp file.
///
/// The board is named from the UPLOAD's name, passed to the reader explicitly
/// rather than read back off the staging path. The reader names a board after
/// the archive it was handed, so staging as `hauksbee-web-gerber-<pid>-<n>.zip`
/// would put a process id and a call counter in `board_name`: the same upload
/// analysed twice in one session would export two different reports, which is
/// the scenario-08 determinism contract. Keeping the two apart also means the
/// staged name never has to survive a round trip, so an upload name with a
/// space, a `+`, or non-ASCII in it reaches the report intact, and matches what
/// the CLI reports for the same archive saved under that name on disk.
///
/// The staging file is a [`tempfile::NamedTempFile`] so it is removed however
/// the call leaves, including on a panic inside the reader. It keeps a `.zip`
/// suffix because that is how the reader recognises an archive.
fn gerber_from_zip_bytes(
    file_name: &str,
    contents: &[u8],
) -> Result<hauksbee_extract::gerber::GerberExtraction, BoardInputError> {
    let mut staged = tempfile::Builder::new()
        .prefix("hauksbee-web-gerber-")
        .suffix(".zip")
        .tempfile()
        .map_err(|e| BoardInputError::Zip(format!("could not stage the zip: {e}")))?;
    std::io::Write::write_all(&mut staged, contents)
        .and_then(|()| std::io::Write::flush(&mut staged))
        .map_err(|e| BoardInputError::Zip(format!("could not stage the zip: {e}")))?;
    ExtractedBoard::from_gerber_with_stats_named(staged.path(), &uploaded_board_name(file_name))
        .map_err(|e| {
            // A zip that fails the gerber read is often a FIRMWARE project zipped
            // by mistake; point at the firmware slot instead of only rejecting.
            if let Some(marker) = zip_firmware_marker(contents) {
                return BoardInputError::Zip(firmware_zip_message(file_name, marker));
            }
            BoardInputError::Zip(format!(
                "could not read '{file_name}' as a gerber archive: {e}. A board zip should \
             contain the gerber fab files (copper + drill)."
            ))
        })
}

/// The board name for an uploaded gerber archive: the upload's file name with
/// any directory part and one trailing `.zip` removed.
///
/// A pure function of the upload name, because it is the board's identity in
/// every report and export and must not move between two analyses of the same
/// upload. It never reaches the filesystem (the staging path is chosen
/// separately), so nothing here is sanitised away: the name the user gave the
/// file is the name they see back, and for an archive named `<x>.zip` it is the
/// same stem `hauksbee run <x>.zip` reports.
///
/// Backslash counts as a separator, unlike `Path` on unix, because a browser
/// file input can still hand over a Windows-style `C:\fakepath\job.zip`.
fn uploaded_board_name(file_name: &str) -> String {
    let leaf = file_name.rsplit(['/', '\\']).next().unwrap_or(file_name);
    // Strip ONE extension, not every repetition: `board.zip.zip` keeps
    // `board.zip`, which is what the user named it. `get` rather than an index
    // because a name ending in a multi-byte character would otherwise panic on
    // a slice landing mid-codepoint.
    let base = match leaf.len().checked_sub(4) {
        Some(cut)
            if leaf
                .get(cut..)
                .is_some_and(|ext| ext.eq_ignore_ascii_case(".zip")) =>
        {
            &leaf[..cut]
        }
        _ => leaf,
    };
    if base.is_empty() {
        "gerber".to_string()
    } else {
        base.to_string()
    }
}

/// What marks a zip as a firmware project rather than a gerber fab archive:
/// a `PlatformIO` config, a `.pio` build tree, or a compiled `.hex` image.
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
        if std::path::Path::new(&name)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("hex"))
        {
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
         (copper + drill)."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use hauksbee_ir::eprintln;
    use std::io::Write;

    const KICAD: &[u8] = include_bytes!("../../hauksbee-ci/examples/boards/boot_gate.kicad_pcb");
    const ALTIUM: &[u8] = include_bytes!("../../../testdata/boards/altium_two_resistor.PcbDoc");
    /// `boot_gate` exported by kicad-cli 9.0.3; the same fixtures the extract
    /// crate's cross-format agreement test uses (see its module docs for
    /// provenance). `boot_gate` reads as 3 components / 4 nets natively.
    const ODB_ZIP: &[u8] =
        include_bytes!("../../hauksbee-extract/tests/fixtures/exchange/boot_gate.odb.zip");
    const IPC2581: &[u8] =
        include_bytes!("../../hauksbee-extract/tests/fixtures/exchange/boot_gate.ipc2581.xml");
    const IPC356: &[u8] = include_bytes!("../../../testdata/pic_programmer.d356");

    /// A titleless one-resistor `KiCad` board: no title-block name for a
    /// reader to find, so tests can see whether a name was invented.
    const TINY: &[u8] = br#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "A")
  (net 2 "B")
  (module Resistor_SMD:R_0402_1005Metric (layer F.Cu)
    (at 0 0)
    (fp_text reference R1 (at 0 0) (layer F.SilkS))
    (fp_text value 10k (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (size 1 1) (layers F.Cu) (net 1 "A"))
    (pad 2 smd rect (at 1 0) (size 1 1) (layers F.Cu) (net 2 "B"))
  )
)
"#;

    /// The uploaded board name is a pure function of the upload name, keeps
    /// whatever characters the user chose, and agrees with the stem the CLI
    /// derives from the same archive on disk.
    #[test]
    fn uploaded_board_name_matches_the_uploads_own_stem() {
        assert_eq!(
            uploaded_board_name("inkplate6_gerber-0915a100.zip"),
            "inkplate6_gerber-0915a100"
        );
        assert_eq!(uploaded_board_name("EPD_board.ZIP"), "EPD_board");
        assert_eq!(uploaded_board_name("board.zip.zip"), "board.zip");
        assert_eq!(uploaded_board_name("no-extension"), "no-extension");
        // A browser that sends a path sends only the leaf onward.
        assert_eq!(uploaded_board_name("jobs/rev C/fab.zip"), "fab");
        assert_eq!(uploaded_board_name("C:\\jobs\\fab.zip"), "fab");
        // Nothing is scrubbed: spaces, `+` and non-ASCII survive, and the two
        // names below stay distinct where a path sanitiser would merge them.
        assert_eq!(uploaded_board_name("rev A+B.zip"), "rev A+B");
        assert_eq!(uploaded_board_name("rev A B.zip"), "rev A B");
        assert_eq!(uploaded_board_name("плата.zip"), "плата");
        // Never empty, and never a slice through a multi-byte character.
        assert_eq!(uploaded_board_name(".zip"), "gerber");
        assert_eq!(uploaded_board_name(""), "gerber");
        assert_eq!(uploaded_board_name("boardé"), "boardé");
        // The stem the CLI would report for the same archive on disk.
        for name in ["rev A+B.zip", "плата.zip", "board.zip.zip", "fab.ZIP"] {
            let from_path = std::path::Path::new(name)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap()
                .to_string();
            assert_eq!(uploaded_board_name(name), from_path, "disagreed on {name}");
        }
    }

    fn zip_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for (name, bytes) in entries {
            w.start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            w.write_all(bytes).unwrap();
        }
        w.finish().unwrap().into_inner()
    }

    /// Every file directly inside `dir`, zipped under `gerbers/`.
    fn zip_dir_files(dir: &Path) -> Vec<u8> {
        let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
        for entry in std::fs::read_dir(dir).unwrap() {
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
        zip_of(&refs)
    }

    /// An empty per-process scratch directory named for `tag`.
    fn fresh_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("hauksbee-bi-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
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
    fn fab_zip_prefers_its_ipc356_netlist_on_bytes_and_path_routes() {
        // The copper film is intentionally insufficient to reconstruct a
        // usable board. Success therefore proves the package netlist won; it
        // is not merely agreeing with a gerber result computed first.
        let bytes = zip_of(&[
            (
                "fab/board-F_Cu.gbr",
                b"%FSLAX46Y46*%\n%MOMM*%\n%ADD10C,1.0*%\nD10*\nX0Y0D03*\nM02*\n",
            ),
            ("fab/board.d356", IPC356),
        ]);
        let norm = from_bytes("fab.zip", &bytes).expect("IPC-D-356 member wins");
        assert_eq!(norm.kind, InputKind::Ipc356Archive);
        assert!(norm.is_gerber() && !norm.is_gerber_archive());
        assert!(norm.layout_text.is_none());
        assert_eq!(norm.raw, bytes, "evidence authenticates the whole archive");
        assert!(!norm.board.components.is_empty());
        assert!(
            norm.notes.iter().any(|note| {
                note.contains("board.d356") && note.contains("not used to infer nets")
            }),
            "the authority decision is user-visible: {:?}",
            norm.notes
        );

        let dir = fresh_dir("ipc356-zip");
        let path = dir.join("fab.zip");
        std::fs::write(&path, &bytes).unwrap();
        let path_norm = from_path(&path).expect("path ZIP uses the same authority");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(path_norm.kind, InputKind::Ipc356Archive);
        assert_eq!(
            path_norm.board.components.len(),
            norm.board.components.len()
        );
        assert_eq!(path_norm.board.nets.len(), norm.board.nets.len());
    }

    #[test]
    fn fab_zip_with_two_ipc356_netlists_refuses_ambiguity() {
        let bytes = zip_of(&[("fab/a.d356", IPC356), ("fab/b.D356", IPC356)]);
        let error = from_bytes("ambiguous.zip", &bytes).expect_err("must not choose by ZIP order");
        let message = error.to_string();
        assert!(message.contains("more than one IPC-D-356"), "{message}");
        assert!(
            message.contains("a.d356") && message.contains("b.D356"),
            "{message}"
        );
    }

    #[test]
    fn a_d356_named_zip_directory_is_not_a_netlist() {
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default();
        writer
            .add_directory("fab/not-a-file.d356/", options)
            .unwrap();
        writer.start_file("fab/board-F_Cu.gbr", options).unwrap();
        writer
            .write_all(b"%FSLAX46Y46*%\n%MOMM*%\n%ADD10C,1.0*%\nD10*\nX0Y0D03*\nM02*\n")
            .unwrap();
        let bytes = writer.finish().unwrap().into_inner();

        let norm = from_bytes("fab.zip", &bytes).expect("directory entry is ignored");
        assert_eq!(norm.kind, InputKind::Gerber);
    }

    #[test]
    fn a_titleless_board_keeps_its_empty_name_on_the_bytes_path() {
        // from_bytes must NOT invent a board name: the web report's board_name
        // for a titleless board is empty, and the golden parity test in
        // frontdoor.rs depends on that staying true.
        let norm = from_bytes("tiny.kicad_pcb", TINY).expect("kicad board normalizes");
        assert_eq!(norm.kind, InputKind::Text);
        assert!(
            norm.board.name.trim().is_empty(),
            "from_bytes must not apply the file-stem fallback: {:?}",
            norm.board.name
        );
    }

    /// The retired Board-as-Code text format gets the same refusal as any
    /// other unreadable file: the accepted formats, not a special message.
    #[test]
    fn a_dot_board_file_is_an_unrecognised_format_like_any_other() {
        let source = b"# Board-as-Code (hauksbee board DSL v1)
board version 20241229

fn main {
}
";
        for name in ["tarski.board", "exported.txt"] {
            let err = from_bytes(name, source).expect_err("the retired format must not load");
            assert!(matches!(err, BoardInputError::Extract(_)), "{name}: {err}");
            let msg = err.web_message();
            assert!(
                msg.contains("Supported:") && msg.contains(".kicad_pcb"),
                "{name}: the refusal lists the accepted formats: {msg}"
            );
            let listed = msg.split_once("Supported:").map_or("", |(_, rest)| rest);
            assert!(
                !listed.contains(".board"),
                "{name}: the retired format is no longer advertised: {msg}"
            );
        }
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
    fn accepted_formats_table_feeds_the_refusal_and_the_served_json() {
        let sentence = supported_formats_sentence();
        for ext in [
            ".kicad_pcb",
            ".brd",
            ".PcbDoc",
            ".xml",
            ".d356",
            ".tgz",
            ".zip",
        ] {
            assert!(sentence.contains(ext), "{ext} missing from: {sentence}");
        }
        assert!(!sentence.contains(".board"), "{sentence}");
        // A SPICE deck is not a board: a board refusal must not offer it.
        assert!(!sentence.contains(".cir"), "{sentence}");
        let json = accepted_formats_json();
        let formats = json["formats"].as_array().expect("formats array");
        assert!(formats.iter().any(|f| f["route"] == "deck"
            && f["exts"]
                .as_array()
                .is_some_and(|e| e.iter().any(|x| x == ".net"))));
        assert!(formats.iter().any(|f| f["route"] == "board"
            && f["exts"]
                .as_array()
                .is_some_and(|e| e.iter().any(|x| x == ".net"))));
        assert_eq!(json["firmware_exts"][0], ".elf");
        // Every single-file and archive input the CI workflow template globs
        // must be pickable in the browser (the picker's `accept` is this table).
        let all: Vec<&str> = ACCEPTED_FORMATS
            .iter()
            .flat_map(|f| f.exts.iter().copied())
            .collect();
        for ext in [".xml", ".zip", ".tgz", ".tar.gz", ".tar"] {
            assert!(all.contains(&ext), "{ext} missing from the served table");
        }
    }

    #[test]
    fn garbage_is_a_friendly_error() {
        let err = from_bytes("nope.txt", b"this is not a board file at all")
            .expect_err("garbage must not normalize");
        let msg = err.web_message();
        assert!(
            msg.contains("Supported:") && msg.contains(".kicad_pcb"),
            "the web message lists the supported formats: {msg}"
        );
        assert!(
            msg.contains(".PcbDoc"),
            "the supported list must include Altium: {msg}"
        );
    }

    const EASYEDA_STD: &[u8] = include_bytes!(
        "../../hauksbee-extract/tests/fixtures/easyeda/rpitx-coax-gp1212-gp731.json"
    );
    const EASYEDA_PRO: &[u8] =
        include_bytes!("../../hauksbee-extract/tests/fixtures/easyeda/nano-hub-pcb1.epcb");

    #[test]
    fn easyeda_is_listed_among_the_accepted_formats() {
        let sentence = supported_formats_sentence();
        assert!(sentence.contains("EasyEDA"), "{sentence}");
        for ext in [".json", ".epcb", ".epro"] {
            assert!(sentence.contains(ext), "{ext} missing from: {sentence}");
        }
        let json = accepted_formats_json();
        let easyeda = json["formats"]
            .as_array()
            .expect("formats array")
            .iter()
            .find(|f| f["vendor"] == "EasyEDA")
            .unwrap_or_else(|| panic!("EasyEDA row missing from {json}"));
        assert_eq!(easyeda["route"], "board");
        assert_eq!(input_kind_phrase(InputKind::Easyeda), "an EasyEDA board");
        let msg = from_bytes("nope.txt", b"this is not a board file at all")
            .expect_err("garbage must not normalize")
            .web_message();
        assert!(
            msg.contains("EasyEDA"),
            "the drop error names EasyEDA: {msg}"
        );
    }

    #[test]
    fn an_easyeda_board_normalizes_without_layout_text() {
        for (name, bytes, parts) in [
            ("gp1212.json", EASYEDA_STD, 4),
            ("hub.epcb", EASYEDA_PRO, 23),
        ] {
            let norm = from_bytes(name, bytes).expect("easyeda normalizes");
            assert_eq!(norm.kind, InputKind::Easyeda);
            // No checkable copper: it takes the geometry-less (spacing not
            // checked) path rather than being measured as if it had geometry.
            assert!(norm.is_gerber() && !norm.is_gerber_archive());
            assert!(norm.layout_text.is_none());
            assert_eq!(norm.raw, bytes, "raw bytes survive");
            assert_eq!(norm.board.components.len(), parts, "{name}");
            assert!(
                norm.notes
                    .iter()
                    .any(|n| n.contains("not reverse-engineered from copper")
                        && n.contains("were not run")),
                "the reader's copper limit travels with the board: {:?}",
                norm.notes
            );
        }
    }

    #[test]
    fn an_easyeda_board_reads_by_content_not_by_extension() {
        let norm = from_bytes("pasted.txt", EASYEDA_STD).expect("content decides");
        assert_eq!(norm.kind, InputKind::Easyeda);
    }

    #[test]
    fn an_easyeda_project_archive_normalizes() {
        let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opts = zip::write::SimpleFileOptions::default();
        w.start_file("project.json", opts).unwrap();
        w.write_all(b"{}").unwrap();
        w.start_file("PCB1.epcb", opts).unwrap();
        w.write_all(EASYEDA_PRO).unwrap();
        let zip = w.finish().unwrap().into_inner();
        let norm = from_bytes("hub.epro", &zip).expect("archive normalizes");
        assert_eq!(norm.kind, InputKind::Easyeda);
        assert_eq!(norm.board.components.len(), 23);
    }

    #[test]
    fn an_easyeda_refusal_is_specific_and_names_the_file() {
        let cut = &EASYEDA_STD[..EASYEDA_STD.len() / 2];
        let msg = from_bytes("cut.json", cut)
            .expect_err("a truncated export is refused")
            .to_string();
        assert!(
            msg.contains("'cut.json'") && msg.contains("truncated"),
            "{msg}"
        );
        // An ordinary JSON file is not an EasyEDA board.
        let msg = from_bytes("package.json", br#"{"name":"x","version":"1"}"#)
            .expect_err("not a board")
            .to_string();
        assert!(!msg.contains("EasyEDA file"), "{msg}");
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
    fn junk_zip_error_names_what_a_board_zip_holds() {
        let bytes = zip_of(&[("README.md", b"not a board")]);
        let err = from_bytes("junk.zip", &bytes).expect_err("junk zip fails");
        let msg = err.to_string();
        assert!(
            msg.contains("gerber") && !msg.contains(".board"),
            "error names what a board zip holds: {msg}"
        );
    }

    #[test]
    fn from_path_applies_the_file_stem_name_fallback() {
        let dir = fresh_dir("test");
        let p = dir.join("my_widget.kicad_pcb");
        std::fs::write(&p, TINY).unwrap();
        let norm = from_path(&p).expect("a titleless board normalizes from a path");
        assert_eq!(norm.kind, InputKind::Text);
        assert_eq!(
            norm.board.name, "my_widget",
            "a titleless board takes its file stem as its name on the path route"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_kicad_project_file_opens_the_design_it_names() {
        // The OS file picker surfaces the .kicad_pro first; refusing it left
        // whole real projects unjudged. It opens the same-stem layout, and
        // refuses only when no design of that name is beside it.
        let dir = fresh_dir("kicad_pro");
        let pro = dir.join("widget.kicad_pro");
        std::fs::write(&pro, b"{}").unwrap();
        let err = from_path(&pro).expect_err("no design beside the project yet");
        assert!(
            err.to_string().contains("no .kicad_pcb or .kicad_sch"),
            "{err}"
        );
        std::fs::write(dir.join("widget.kicad_pcb"), KICAD).unwrap();
        let via_pro = from_path(&pro).expect("the project opens its layout");
        let direct = from_path(&dir.join("widget.kicad_pcb")).expect("layout");
        assert_eq!(via_pro.kind, InputKind::Text);
        assert_eq!(via_pro.layout_text, direct.layout_text);
        assert_eq!(via_pro.board.components.len(), 3);
        assert_eq!(via_pro.board.nets.len(), direct.board.nets.len());
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

    /// Gerber job with a placement file through both entry points. Keep this
    /// on a checked-in fixture so a sibling developer corpus cannot change the
    /// unit test's input or make clean-room CI skip the coverage entirely.
    /// The zipped form of a Gerber directory, through the bytes path.
    fn assert_gerber_zip_normalizes(dir: &Path) {
        let bytes = zip_dir_files(dir);
        let norm = from_bytes("cm4_adapter_gerbers.zip", &bytes).expect("gerber zip normalizes");
        assert_eq!(norm.kind, InputKind::Gerber);
        assert!(norm.is_gerber());
        assert!(norm.layout_text.is_none());
        assert!(!norm.board.nets.is_empty(), "nets recovered from copper");
    }

    #[test]
    fn gerber_zip_and_dir_normalize_as_gerber() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata/ingest-robustness/gerber_kicad_pos");
        assert!(dir.is_dir(), "checked-in Gerber fixture is missing");
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

        assert_gerber_zip_normalizes(&dir);
    }

    /// Real fab archive through both entry points, which deliberately differ.
    /// The CM4 adapter ships no pick-and-place, so the local directory form
    /// refuses: the user is standing next to the folder and can add the part
    /// list, and the error names it after proving the copper was read. The
    /// zipped form is the upload surface behind the web drop-zone and B5
    /// specs, and it normalizes, carrying the reconstructed nets into the
    /// analysis that then reports the missing parts honestly. Corpus-gated
    /// like the hauksbee-ci gerber-zip test, including its licence rule: the
    /// board is outside the default fetch, so only the explicit uConsole
    /// opt-in makes absence a failure.
    #[test]
    fn gerber_dir_refuses_without_parts_and_the_zip_normalizes() {
        let Some(dir) = hauksbee_testkit::corpus_board(
            env!("CARGO_MANIFEST_DIR"),
            "famous/uconsole_cm4_adapter_gerber",
        ) else {
            assert!(
                std::env::var("HAUKSBEE_REQUIRE_UCONSOLE_CORPUS").is_err(),
                "HAUKSBEE_REQUIRE_UCONSOLE_CORPUS set but uconsole_cm4_adapter_gerber is absent"
            );
            eprintln!(
                "NOT RUN  gerber normalizer test: uconsole_cm4_adapter_gerber is \
                 not in the default fetch (licence unconfirmed)"
            );
            return;
        };
        // The directory form (from_path only).
        let err = from_path(&dir).expect_err("a fab job with no part list is refused");
        let msg = err.to_string();
        assert!(
            msg.contains("reconstructed"),
            "copper was read first: {msg}"
        );
        assert!(
            msg.contains("pick-and-place"),
            "the refusal names the unlocking input: {msg}"
        );

        assert_gerber_zip_normalizes(&dir);
    }

    /// The counts `boot_gate` reads as through the native `KiCad` path, so the two
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

        let dir = fresh_dir("odb");
        let file = dir.join("boot_gate.odb.zip");
        std::fs::write(&file, ODB_ZIP).unwrap();
        let norm = from_path(&file).expect("ODB++ zip normalizes from a path");
        assert_eq!(norm.kind, InputKind::Odb);
        assert_eq!(norm.board.components.len(), BOOT_GATE.0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unpacked_odbpp_job_directory_normalizes_instead_of_failing_as_gerbers() {
        let dir = fresh_dir("odbdir");
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

        let dir = fresh_dir("ipc");
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
        assert_eq!(
            input_kind_phrase(InputKind::Ipc356Archive),
            "a fab archive carrying an IPC-D-356 netlist"
        );
        assert_eq!(input_kind_phrase(InputKind::Odb), "an ODB++ job");
        assert_eq!(
            input_kind_phrase(InputKind::Ipc2581),
            "an IPC-2581 document"
        );
        for kind in [InputKind::Ipc356Archive, InputKind::Odb, InputKind::Ipc2581] {
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
