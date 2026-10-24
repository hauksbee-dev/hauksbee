//! A design of several boards, read as the one product they make.
//!
//! A product that stacks or plugs boards together (a carrier and an MCU
//! breakout) is not one design file. An assembly manifest names the files, and
//! [`crate::board_input::from_path`] given that manifest reads each board and
//! joins them ([`hauksbee_extract::mate::assemble`]) so every check, static or
//! co-simulated, sees the signal that crosses the connector.
//!
//! ```toml
//! [assembly]
//! boards = ["carrier.kicad_sch", "breakout.kicad_sch"]   # relative to this file
//!
//! # Optional: the connectors that mate, when their footprints do not say.
//! [[assembly.mate]]
//! a = "carrier:J1"        # <board file stem>:<designator>
//! b = "breakout:J2"
//! ```

use std::path::Path;

use serde::Deserialize;

use crate::board_input::{BoardInputError, InputKind, NormalizedBoard};

/// The manifest's `[assembly]` table.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    assembly: Assembly,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Assembly {
    boards: Vec<String>,
    #[serde(default)]
    mate: Vec<MateSpec>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MateSpec {
    a: String,
    b: String,
}

/// Whether `text` is an assembly manifest.
#[must_use]
pub fn is_manifest(text: &str) -> bool {
    toml::from_str::<toml::Table>(text).is_ok_and(|t| t.contains_key("assembly"))
}

/// `label:designator` as the two halves.
fn split_ref(s: &str) -> Option<(String, String)> {
    let (label, designator) = s.split_once(':')?;
    Some((label.trim().to_owned(), designator.trim().to_owned()))
}

/// Read the manifest at `path` (its text is `text`) and join the boards it names.
///
/// # Errors
///
/// [`BoardInputError::Extract`] when the manifest is malformed, names fewer than
/// two boards, or names a mate not written `<board>:<designator>`; any error of
/// reading one of the boards.
pub fn from_manifest(path: &Path, text: &str) -> Result<NormalizedBoard, BoardInputError> {
    let fail = |m: String| BoardInputError::Extract(format!("assembly '{}': {m}", path.display()));
    let manifest: Manifest = toml::from_str(text).map_err(|e| fail(e.to_string()))?;
    if manifest.assembly.boards.len() < 2 {
        return Err(fail(
            "[assembly] boards names fewer than two boards: a single board is read by its own file"
                .into(),
        ));
    }
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let mut boards = Vec::new();
    let mut notes = Vec::new();
    for file in &manifest.assembly.boards {
        let board_path = dir.join(file);
        let one = crate::board_input::from_path(&board_path)?;
        let label = board_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("board")
            .to_owned();
        notes.extend(one.notes.iter().map(|n| format!("{label}: {n}")));
        boards.push((label, one.board));
    }
    let mut named = Vec::new();
    for m in &manifest.assembly.mate {
        let (Some(a), Some(b)) = (split_ref(&m.a), split_ref(&m.b)) else {
            return Err(fail(format!(
                "mate '{}' with '{}' must be written <board file stem>:<designator>",
                m.a, m.b
            )));
        };
        named.push((a, b));
    }
    let assembled = hauksbee_extract::mate::assemble(&boards, &named);
    notes.extend(assembled.notes);
    notes.push(format!(
        "Assembled {} boards through {} mated connector(s): {}.",
        manifest.assembly.boards.len(),
        assembled.mates.len(),
        assembled
            .mates
            .iter()
            .map(|m| format!("{} to {}", m.a, m.b))
            .collect::<Vec<_>>()
            .join(", ")
    ));
    let mut board = assembled.board;
    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
        stem.clone_into(&mut board.name);
    }
    Ok(NormalizedBoard {
        board,
        // No single layout file describes the product: geometry checks say so.
        layout_text: None,
        raw: Vec::new(),
        kind: InputKind::Text,
        notes,
    })
}
