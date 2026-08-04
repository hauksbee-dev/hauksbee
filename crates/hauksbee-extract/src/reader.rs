//! The board-format reader registry.
//!
//! Extraction picks a format through a registry rather than through one
//! hard-coded substring sniff. A 512-char `head.contains(...)` ladder makes
//! adding or reordering a format an edit to shared code, and makes the failure
//! mode for an unknown file whatever the last fallback (`ipc356`) happens to
//! say.
//!
//! So each format is a
//! [`BoardReader`] that owns its own detection ([`BoardReader::detects`]) and
//! its own parse ([`BoardReader::read`]). The [`Registry`] holds them in a
//! documented order and, when nothing matches, reports *what it tried*
//! ([`ReadError::Unrecognized`]).
//!
//! # Third-party formats (no dynamic loading)
//!
//! A `.so` plugin ABI is deliberately out of scope (Rust's unstable ABI makes
//! it a maintenance sink; see plan 06 §4). A fork adds a format with one
//! registration line against the small stable trait:
//!
//! ```no_run
//! use hauksbee_extract::reader::{BoardReader, Registry};
//! # struct MyReader;
//! # impl BoardReader for MyReader {
//! #     fn name(&self) -> &str { "my-format" }
//! #     fn detects(&self, _b: &[u8], _p: Option<&std::path::Path>) -> bool { false }
//! #     fn read(&self, _b: &[u8], _p: Option<&std::path::Path>)
//! #         -> Result<hauksbee_extract::ExtractedBoard, hauksbee_extract::reader::ReadError> {
//! #         unimplemented!()
//! #     }
//! # }
//! let mut registry = Registry::builtin();
//! registry.register(Box::new(MyReader)); // consulted before the builtins
//! ```

use crate::{ExtractError, ExtractedBoard};
use std::path::Path;

/// The error a [`BoardReader::read`] returns. Aliased to the crate's
/// [`ExtractError`] so every existing per-format extractor plugs in unchanged;
/// the trait sketch in plan 06 §4 names it `ReadError`.
pub type ReadError = ExtractError;

/// One board file format: how to recognise it, and how to read it.
///
/// Implementations must keep [`detects`](BoardReader::detects) cheap (a magic /
/// structural prefix check, never a full parse) and must **not** false-positive
/// on another format's files; the detection-matrix test
/// (`tests/reader_matrix.rs`) enforces this pairwise across every fixture.
pub trait BoardReader: Send + Sync {
    /// Stable short identifier, e.g. `"kicad-pcb"`. Shown in
    /// [`ReadError::Unrecognized`] and used as the matrix-test key.
    fn name(&self) -> &str;

    /// Cheap magic / structure check. Must not false-positive on other formats.
    ///
    /// `bytes` is the file content; `path` is a filename hint when the caller
    /// has one (the content sniff is authoritative, path is only ever a
    /// tie-break, and the builtin readers do not need it).
    fn detects(&self, bytes: &[u8], path: Option<&Path>) -> bool;

    /// Parse a file this reader has claimed into an [`ExtractedBoard`].
    fn read(&self, bytes: &[u8], path: Option<&Path>) -> Result<ExtractedBoard, ReadError>;

    /// True for formats whose content is binary (OLE2 etc.). The byte-input
    /// entry point ([`crate::ExtractedBoard::from_auto_bytes`]) only claims
    /// binary readers, so a text file handed to it falls through to the text
    /// sniffer instead of being parsed as bytes.
    fn is_binary(&self) -> bool {
        false
    }
}

/// The largest content prefix any builtin reader inspects for a magic string
/// (kicad/eagle/netlist magics all sit at byte 0; this window is generous).
const MAGIC_WINDOW: usize = 2048;

/// The first ≤512 chars of the content, lossy-decoded from the leading
/// [`MAGIC_WINDOW`] bytes. This reproduces the legacy sniff's
/// `text.chars().take(512)` window for the ASCII headers every text board
/// format uses.
fn magic_head(bytes: &[u8]) -> String {
    let window = &bytes[..bytes.len().min(MAGIC_WINDOW)];
    String::from_utf8_lossy(window).chars().take(512).collect()
}

// ── The six builtin readers ───────────────────────────────────────────────────

/// KiCad `.kicad_pcb` layout (`(kicad_pcb ...`).
pub struct KicadPcbReader;
impl BoardReader for KicadPcbReader {
    fn name(&self) -> &str {
        "kicad-pcb"
    }
    fn detects(&self, bytes: &[u8], _path: Option<&Path>) -> bool {
        magic_head(bytes).contains("(kicad_pcb")
    }
    fn read(&self, bytes: &[u8], _path: Option<&Path>) -> Result<ExtractedBoard, ReadError> {
        ExtractedBoard::from_kicad_pcb(&String::from_utf8_lossy(bytes))
    }
}

/// KiCad `.kicad_sch` schematic (`(kicad_sch ...`). When a real file `path` is
/// supplied the reader recurses the sub-sheet hierarchy; without one it reads
/// the single sheet in `bytes` (the historical `from_auto` behaviour).
pub struct KicadSchematicReader;
impl BoardReader for KicadSchematicReader {
    fn name(&self) -> &str {
        "kicad-schematic"
    }
    fn detects(&self, bytes: &[u8], _path: Option<&Path>) -> bool {
        magic_head(bytes).contains("(kicad_sch")
    }
    fn read(&self, bytes: &[u8], path: Option<&Path>) -> Result<ExtractedBoard, ReadError> {
        match path {
            Some(p) if p.is_file() => ExtractedBoard::from_kicad_schematic_path(p),
            _ => ExtractedBoard::from_kicad_schematic(&String::from_utf8_lossy(bytes)),
        }
    }
}

/// KiCad s-expression netlist export (`(export ...`).
pub struct KicadNetlistReader;
impl BoardReader for KicadNetlistReader {
    fn name(&self) -> &str {
        "kicad-netlist"
    }
    fn detects(&self, bytes: &[u8], _path: Option<&Path>) -> bool {
        magic_head(bytes).trim_start().starts_with("(export")
    }
    fn read(&self, bytes: &[u8], _path: Option<&Path>) -> Result<ExtractedBoard, ReadError> {
        ExtractedBoard::from_kicad_netlist(&String::from_utf8_lossy(bytes))
    }
}

/// Eagle `.brd` board XML (`<eagle ...`).
pub struct EagleReader;
impl BoardReader for EagleReader {
    fn name(&self) -> &str {
        "eagle"
    }
    fn detects(&self, bytes: &[u8], _path: Option<&Path>) -> bool {
        magic_head(bytes).contains("<eagle")
    }
    fn read(&self, bytes: &[u8], _path: Option<&Path>) -> Result<ExtractedBoard, ReadError> {
        ExtractedBoard::from_eagle_brd(&String::from_utf8_lossy(bytes))
    }
}

/// IPC-D-356/356A fab netlist. Detected by its fixed-column test records
/// (`317`/`327`/`367` at column 0); the same records
/// [`ExtractedBoard::from_ipc_d356`] requires, so detection and a successful
/// read coincide exactly.
pub struct Ipc356Reader;
impl BoardReader for Ipc356Reader {
    fn name(&self) -> &str {
        "ipc-d356"
    }
    fn detects(&self, bytes: &[u8], _path: Option<&Path>) -> bool {
        // Scan the WHOLE file at line starts (a cheap byte scan, no allocation).
        // The parser reads the entire file, so detection must too: a large `C`
        // comment / `P` parameter header can push the first `3xx` test record
        // past any fixed window, and a windowed scan would regress that file to
        // Unrecognized even though `from_ipc_d356` parses it fine.
        bytes.split(|&b| b == b'\n').any(|line| {
            line.starts_with(b"317") || line.starts_with(b"327") || line.starts_with(b"367")
        })
    }
    fn read(&self, bytes: &[u8], _path: Option<&Path>) -> Result<ExtractedBoard, ReadError> {
        ExtractedBoard::from_ipc_d356(&String::from_utf8_lossy(bytes))
    }
}

/// ASCII Protel board export: pipe-delimited `|RECORD=` text declaring
/// `KIND=Protel_Advanced_PCB` (the `.pcbdoc` form EasyEDA produces). Detection
/// requires that KIND, so ASCII exports of other Protel documents fall through
/// to [`unrecognized_message`]'s explanation instead of a garbled parse.
pub struct ProtelAsciiReader;
impl BoardReader for ProtelAsciiReader {
    fn name(&self) -> &str {
        "protel-ascii"
    }
    fn detects(&self, bytes: &[u8], _path: Option<&Path>) -> bool {
        crate::protel_ascii::looks_like_protel_ascii(bytes)
    }
    fn read(&self, bytes: &[u8], _path: Option<&Path>) -> Result<ExtractedBoard, ReadError> {
        ExtractedBoard::from_protel_ascii(&String::from_utf8_lossy(bytes))
    }
}

/// Altium Designer `.PcbDoc` (binary OLE2). Detection is the container +
/// Altium-stream check ([`crate::altium::looks_like_pcbdoc`]); the OLE2 magic
/// `D0 CF 11 E0` cannot appear in any text format, so this never contends with
/// the text readers.
pub struct AltiumReader;
impl BoardReader for AltiumReader {
    fn name(&self) -> &str {
        "altium"
    }
    fn detects(&self, bytes: &[u8], _path: Option<&Path>) -> bool {
        crate::altium::looks_like_pcbdoc(bytes)
    }
    fn read(&self, bytes: &[u8], _path: Option<&Path>) -> Result<ExtractedBoard, ReadError> {
        ExtractedBoard::from_altium_pcb(bytes)
    }
    fn is_binary(&self) -> bool {
        true
    }
}

// ── The registry ──────────────────────────────────────────────────────────────

/// An ordered set of [`BoardReader`]s. Detection walks the list front-to-back
/// and the first reader that claims the bytes wins.
///
/// ## Order
///
/// The builtin readers are mutually exclusive by construction (distinct magics;
/// the Altium reader keys on the OLE2 container, which no text format contains),
/// so order only ever matters if a third-party reader overlaps a builtin.
/// [`register`](Registry::register) therefore inserts at the **front**, so a
/// fork can deliberately shadow a builtin. Among the builtins the order mirrors
/// the legacy sniff precedence, eagle → netlist → schematic → pcb → ipc356,
/// with the binary Altium reader consulted first (its check is a couple of
/// bytes and it can never match text).
pub struct Registry {
    readers: Vec<Box<dyn BoardReader>>,
}

impl Registry {
    /// The seven formats hauksbee reads natively.
    pub fn builtin() -> Self {
        Registry {
            readers: vec![
                Box::new(AltiumReader),
                Box::new(ProtelAsciiReader),
                Box::new(EagleReader),
                Box::new(KicadNetlistReader),
                Box::new(KicadSchematicReader),
                Box::new(KicadPcbReader),
                Box::new(Ipc356Reader),
            ],
        }
    }

    /// Add a reader, consulted **before** the builtins (see the order note on
    /// [`Registry`]). This is the third-party extension point; see the module
    /// docs for the one-line fork example.
    pub fn register(&mut self, reader: Box<dyn BoardReader>) {
        self.readers.insert(0, reader);
    }

    /// The reader that claims these bytes, if any.
    pub fn detect(&self, bytes: &[u8], path: Option<&Path>) -> Option<&dyn BoardReader> {
        self.readers
            .iter()
            .find(|r| r.detects(bytes, path))
            .map(|b| b.as_ref())
    }

    /// The *binary* reader that claims these bytes, if any. Used by the
    /// byte-input entry point so text handed to it is not force-parsed as a
    /// binary format.
    pub fn detect_binary(&self, bytes: &[u8], path: Option<&Path>) -> Option<&dyn BoardReader> {
        self.readers
            .iter()
            .find(|r| r.is_binary() && r.detects(bytes, path))
            .map(|b| b.as_ref())
    }

    /// Detect and read in one step. When nothing matches, the error carries
    /// [`unrecognized_message`]'s user-facing explanation of what the bytes
    /// look like and what hauksbee accepts.
    pub fn read(&self, bytes: &[u8], path: Option<&Path>) -> Result<ExtractedBoard, ReadError> {
        match self.detect(bytes, path) {
            Some(r) => r.read(bytes, path),
            None => Err(ExtractError::Unrecognized {
                message: unrecognized_message(bytes),
            }),
        }
    }

    /// The names of every registered reader, in consultation order.
    pub fn reader_names(&self) -> Vec<&str> {
        self.readers.iter().map(|r| r.name()).collect()
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::builtin()
    }
}

/// The user-facing message for content no reader recognised. Special-cases
/// the look-alikes users actually upload (an empty file, a Git LFS pointer
/// that was never pulled, an ASCII Protel export that is not a board), and
/// otherwise names the accepted formats in user words rather than internal
/// reader ids.
pub fn unrecognized_message(bytes: &[u8]) -> String {
    if bytes.iter().all(|b| b.is_ascii_whitespace()) {
        return "this file is empty".to_string();
    }
    let head = magic_head(bytes);
    let head = head.trim_start_matches('\u{feff}').trim_start();
    if head.starts_with("version https://git-lfs") {
        return "this is a Git LFS pointer, not the board file itself: the repository \
                stores the real file in Git LFS and it was never downloaded. Run \
                `git lfs install && git lfs pull` in the repository, then retry \
                with the real file"
            .to_string();
    }
    if crate::protel_ascii::looks_like_pipe_records(bytes) {
        return format!(
            "this is an ASCII Protel export (what EasyEDA produces); hauksbee reads \
             the Protel_Advanced_PCB board form of it directly, and this file is \
             not one. Open it in Altium Designer and re-save as a binary .PcbDoc, \
             or see {}",
            hauksbee_ir::docs_url("docs/ingest/ALTIUM.md")
        );
    }
    "unrecognized board format: hauksbee reads a KiCad board, schematic or netlist, \
     an Eagle board, an Altium .PcbDoc (binary or ASCII), an IPC-D-356 netlist, or \
     a folder or zip of gerbers"
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::{BoardReader, Ipc356Reader};

    #[test]
    fn ipc356_detected_even_with_a_header_past_64kib() {
        // R13: the parser reads the whole file, so detection must too. A large
        // `C`-comment header that pushes the first test record past 64 KiB
        // slips past a windowed detector and regresses to Unrecognized.
        let mut doc = String::new();
        for i in 0..3000 {
            doc.push_str(&format!(
                "C  a long comment header line number {i} padding padding\n"
            ));
        }
        assert!(doc.len() > 64 * 1024, "header must exceed the old window");
        doc.push_str("317GND              R1    -1    D0472PA00X+019000Y+029450X0945Y0945R180S0\n");
        assert!(
            Ipc356Reader.detects(doc.as_bytes(), None),
            "an IPC record past 64 KiB must still be detected"
        );
        // A file with no test record at all is still rejected.
        assert!(!Ipc356Reader.detects(b"C only comments\nP JOB test\n", None));
    }

    #[test]
    fn unrecognized_message_special_cases() {
        use super::unrecognized_message;
        // Empty (and whitespace-only) files get the plain-truth message.
        assert_eq!(unrecognized_message(b""), "this file is empty");
        assert_eq!(unrecognized_message(b"  \n\t"), "this file is empty");
        // A Git LFS pointer names itself and the fix.
        let lfs = b"version https://git-lfs.github.com/spec/v1\noid sha256:abc\nsize 12345\n";
        let msg = unrecognized_message(lfs);
        assert!(msg.contains("Git LFS pointer"), "got: {msg}");
        assert!(
            msg.contains("git lfs install && git lfs pull"),
            "got: {msg}"
        );
        // Pipe-record text that is NOT a Protel_Advanced_PCB board gets the
        // ASCII Protel explanation.
        let msg = unrecognized_message(b"|RECORD=Sheet|KIND=Protel_Schematic|X=1");
        assert!(msg.contains("ASCII Protel export"), "got: {msg}");
        assert!(msg.contains("EasyEDA"), "got: {msg}");
        // Everything else lists the accepted formats in user words, with no
        // internal reader ids.
        let msg = unrecognized_message(b"hello world, definitely not a board");
        assert!(
            msg.contains("KiCad board, schematic or netlist"),
            "got: {msg}"
        );
        assert!(msg.contains("Eagle"), "got: {msg}");
        assert!(msg.contains("gerbers"), "got: {msg}");
        assert!(!msg.contains("kicad-pcb"), "no reader ids: {msg}");
    }

    #[test]
    fn protel_ascii_board_is_detected_and_read() {
        use super::{BoardReader, ProtelAsciiReader, Registry};
        let text = "|RECORD=Board|KIND=Protel_Advanced_PCB|VERSION=5.00\n\
                    |RECORD=Net|ID=0|NAME=GND\n\
                    |RECORD=Component|ID=0|LAYER=TOP|X=0mil|Y=0mil|ROTATION=0|PATTERN=R0603|SOURCEDESIGNATOR=R1\n\
                    |RECORD=Pad|COMPONENT=0|NET=0|LAYER=TOP|NAME=1|X=0mil|Y=0mil\n";
        assert!(ProtelAsciiReader.detects(text.as_bytes(), None));
        let board = Registry::builtin()
            .read(text.as_bytes(), None)
            .expect("ASCII Protel board reads through the registry");
        assert_eq!(board.components.len(), 1);
        assert_eq!(board.components[0].reference, "R1");
    }
}
