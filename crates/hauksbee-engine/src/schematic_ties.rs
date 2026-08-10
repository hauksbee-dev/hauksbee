//! Resolving the companion Eagle `.sch` whose net ties qualify a `.brd`'s
//! copper shorts.
//!
//! An Eagle `.brd` carries no net-tie field of any kind, so on that format the
//! geometric DRC genuinely cannot tell a deliberate star ground from a solder
//! bridge: both are copper joining two differently named nets. The declaration
//! lives in the schematic, which the DRC is not handed. This resolves that
//! companion so it can be.
//!
//! Scoped to Eagle deliberately. A `.kicad_pcb` declares its ties in the layout
//! itself (`net_tie_pad_groups`, `(attr net_tie)`), so nothing is missing from
//! the input the DRC already has, and a `.kicad_sch` has no construct for
//! "these two nets are joined on purpose" to read even if it were supplied (see
//! `docs/ingest/SCHEMATICS.md`, "Net-tie footprints ... have no schematic
//! counterpart"). Altium's `.PcbDoc` carries the native `COMPONENTTYPE=Net Tie`
//! field. Eagle is the one format with a hole here.
//!
//! Discovery follows the two conventions already in the tree rather than a new
//! one: the same-folder sibling lookup `reports::kicad_pro_clearance_rules` uses
//! for a `.kicad_pro` (`board_path.with_extension`), and an explicit
//! `--schematic <FILE>` flag in the shape of `--bom` / `--placement`.

use hauksbee_extract::eagle_sch::DeclaredNetTie;
use std::path::{Path, PathBuf};

/// A companion schematic that was read, with the bytes behind it so the report
/// inventory can hash exactly what contributed.
#[derive(Debug)]
pub struct SchematicTies {
    /// The path as the user gave it (or as it was discovered), not canonicalized:
    /// the inventory row reads back in the user's own vocabulary.
    pub path: PathBuf,
    /// The exact bytes read, for the inventory's SHA-256.
    pub raw: Vec<u8>,
    /// Every tie the schematic declares. May be empty: "this schematic declares
    /// no tie" is a real, reportable result and is not the same as not looking.
    pub ties: Vec<DeclaredNetTie>,
    /// True when the file was found beside the board rather than named by the
    /// user, so a surface can say which happened.
    pub auto_discovered: bool,
}

/// Resolve the companion schematic for `board_path`.
///
/// `explicit` is the `--schematic` path when the user gave one. An explicit path
/// that cannot be read or is not an Eagle schematic is an ERROR: the user named
/// a file and expects it to have been used, and silently ignoring it would let a
/// serious short be presented as unqualified when the input that would qualify
/// it was supplied and dropped.
///
/// `board_is_eagle` is the caller's content sniff of the layout, the same
/// `<eagle>` test `ExtractedBoard::drc_with_clearance_rules` dispatches on.
///
/// Auto-discovery is silent on absence, which is the common case, and silent on
/// a sibling `.sch` that turns out to be some other tool's format. A KiCad 5
/// legacy `.sch` sits beside plenty of boards and is not an Eagle file; guessing
/// from the extension alone and then failing the run would break every such
/// project for no gain.
pub fn resolve(
    board_path: &Path,
    explicit: Option<&Path>,
    board_is_eagle: bool,
) -> anyhow::Result<Option<SchematicTies>> {
    // A companion is only meaningful for an Eagle `.brd`, the one format that
    // cannot declare a net tie itself. Without this, `--schematic eagle.sch`
    // beside a `.kicad_pcb` would be accepted and could reclassify that board's
    // shorts on a net-name coincidence, using a file that describes a different
    // design entirely.
    if !board_is_eagle {
        if let Some(path) = explicit {
            anyhow::bail!(
                "--schematic {}: this board is not an Eagle .brd. The flag exists because an \
                 Eagle .brd records no net ties; a KiCad layout declares them in the .kicad_pcb \
                 (net_tie_pad_groups) and Altium in its Components record, so both are already \
                 read from the board itself. Remove the flag.",
                path.display()
            );
        }
        return Ok(None);
    }
    if let Some(path) = explicit {
        let raw = std::fs::read(path)
            .map_err(|e| anyhow::anyhow!("--schematic {}: {e}", path.display()))?;
        let text = String::from_utf8_lossy(&raw);
        if hauksbee_extract::looks_like_eagle_binary(&raw) {
            anyhow::bail!(
                "--schematic {}: {}",
                path.display(),
                hauksbee_extract::eagle_binary_message()
            );
        }
        if !hauksbee_extract::looks_like_eagle_schematic(&text) {
            anyhow::bail!(
                "--schematic {}: not an Eagle .sch (XML, Eagle 6+). This flag reads the net ties \
                 an Eagle schematic declares, so a copper contact the design intends is reported \
                 as a note rather than a serious short. A KiCad layout declares its ties in the \
                 .kicad_pcb itself and needs no companion here.",
                path.display()
            );
        }
        let ties = hauksbee_extract::declared_net_ties(&text)
            .map_err(|e| anyhow::anyhow!("--schematic {}: {e}", path.display()))?;
        return Ok(Some(SchematicTies {
            path: path.to_path_buf(),
            raw,
            ties,
            auto_discovered: false,
        }));
    }

    // Same-folder sibling, exactly as the `.kicad_pro` clearance lookup does it.
    // Eagle names the pair `<project>.brd` / `<project>.sch`, so the extension
    // swap is the whole convention.
    let sibling = board_path.with_extension("sch");
    let Ok(raw) = std::fs::read(&sibling) else {
        return Ok(None);
    };
    let text = String::from_utf8_lossy(&raw);
    if !hauksbee_extract::looks_like_eagle_schematic(&text) {
        return Ok(None);
    }
    // A malformed sibling nobody asked for is not worth failing a run over, but
    // it must not be reported as "no ties declared" either: returning None keeps
    // the unlocking-input hint on the shorts, which is the honest state.
    let Ok(ties) = hauksbee_extract::declared_net_ties(&text) else {
        return Ok(None);
    };
    Ok(Some(SchematicTies {
        path: sibling,
        raw,
        ties,
        auto_discovered: true,
    }))
}

impl SchematicTies {
    /// Apply these declarations to a DRC report and describe what happened, for
    /// the inventory's contribution row.
    ///
    /// Reclassifies; never deletes. See
    /// [`hauksbee_extract::DrcReport::qualify_with_declared_ties`].
    pub fn apply(&self, report: &mut hauksbee_extract::DrcReport) -> String {
        let qualified = report.qualify_with_declared_ties(&self.path.to_string_lossy(), &self.ties);
        format!(
            "{} declared net tie{} read from the schematic's supply symbols; {qualified} copper \
             contact{} reclassified from serious short to a declared tie",
            self.ties.len(),
            if self.ties.len() == 1 { "" } else { "s" },
            if qualified == 1 { "" } else { "s" },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_non_eagle_board_never_looks_for_a_companion() {
        // A `.kicad_pcb` declares its ties in the layout the DRC already reads, so
        // there is no companion to find and none is looked for, even if a `.sch`
        // happens to sit beside it (KiCad 5 legacy `.sch` files often do).
        let resolved = resolve(Path::new("/nonexistent/board.kicad_pcb"), None, false)
            .expect("no error without an explicit path");
        assert!(resolved.is_none());
    }

    #[test]
    fn an_explicit_schematic_on_a_non_eagle_board_is_refused() {
        // Silently ignoring it would be worse: the user named a file and would
        // read an unqualified serious short believing the schematic had been
        // consulted. Accepting it would be worse still, since an Eagle schematic
        // could reclassify a KiCad board's short on a net-name coincidence.
        let err = resolve(
            Path::new("/nonexistent/board.kicad_pcb"),
            Some(Path::new("/nonexistent/other.sch")),
            false,
        )
        .expect_err("an explicit companion on a non-Eagle board is an error");
        let text = format!("{err}");
        assert!(text.contains("not an Eagle .brd"), "{text}");
        assert!(
            text.contains("net_tie_pad_groups"),
            "the refusal must say where KiCad ties actually live: {text}"
        );
    }

    #[test]
    fn a_missing_explicit_schematic_is_an_error_not_a_shrug() {
        let err = resolve(
            Path::new("/nonexistent/board.brd"),
            Some(Path::new("/nonexistent/absent.sch")),
            true,
        )
        .expect_err("a named file that does not exist is an error");
        assert!(format!("{err}").contains("absent.sch"), "{err}");
    }

    #[test]
    fn an_explicit_path_that_is_not_an_eagle_schematic_is_refused() {
        let dir = std::env::temp_dir().join("hauksbee-sch-ties-test");
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("not-a-schematic.sch");
        // An Eagle BOARD, not a schematic: the discriminator is `<schematic>`.
        std::fs::write(
            &path,
            r#"<?xml version="1.0"?><eagle version="6.6.0"><drawing><board><signals/></board></drawing></eagle>"#,
        )
        .expect("write fixture");
        let err = resolve(Path::new("/nonexistent/board.brd"), Some(&path), true)
            .expect_err("a .brd passed as the schematic is an error");
        assert!(format!("{err}").contains("not an Eagle .sch"), "{err}");
    }

    #[test]
    fn a_missing_sibling_is_silent() {
        // The overwhelmingly common case for an Eagle board: no companion on
        // disk. That is not an error, and the shorts keep their unlocking hint.
        let resolved = resolve(Path::new("/nonexistent/board.brd"), None, true)
            .expect("absence is not an error");
        assert!(resolved.is_none());
    }
}
