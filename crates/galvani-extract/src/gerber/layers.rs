//! Layer-role inference from fab filenames.
//!
//! A gerber job ships a directory of files whose *names* are the only clue to
//! what each one is. There is no single convention, so we recognise the common
//! ones and let an explicit mapping file override when a board is exotic:
//!
//!   - **KiCad long names**: `board-F_Cu.gbr`, `board-B_Cu.gbr`, inner
//!     `board-In1_Cu.gbr`.
//!   - **Protel / legacy extensions**: `.GTL` (top copper), `.GBL` (bottom),
//!     `.G1L`/`.G2L`/`.GP1`… (inner), `.GTP`/`.GBP` (paste), `.GTO`/`.GBO`
//!     (silk), `.GTS`/`.GBS` (mask), `.GKO`/`.GM1` (outline), `.TXT`/`.DRL`
//!     (drill), `.GBR`/`.GB?` ambiguous.
//!   - **Altium-ish**: `.GTL`/`.GBL` plus `Top Layer.gbr` etc.
//!   - **Generic words**: a filename containing `top`+`copper`, `bottom`+`cu`,
//!     `signal`, `inner`, etc.
//!
//! The classifier only needs to find the *copper* and *drill* roles for
//! electrical reconstruction; everything else (silk, mask, paste, courtyard,
//! fab, outline) is recognised so it can be deliberately ignored rather than
//! mis-read as copper.

use std::path::Path;

/// What a fab file is, for the purposes of electrical reconstruction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayerRole {
    /// Copper signal layer. `index` is the stack position: 0 = top (F.Cu),
    /// `n-1` = bottom (B.Cu), 1..n-1 = inner layers top-to-bottom. We don't
    /// always know the total layer count up front, so inner indices are
    /// assigned provisionally and re-ordered once the whole set is seen.
    Copper { index: usize, name: String },
    /// Excellon drill file (plated or non-plated; the file's own records say
    /// which). Gerber-format drills are also possible but rare.
    Drill,
    /// Board outline / edge cuts. Useful for bounds, not connectivity.
    Outline,
    /// Recognised but electrically irrelevant (silk, mask, paste, fab,
    /// courtyard, adhesive, assembly drawing, ...).
    Ignored,
    /// Could not be classified.
    Unknown,
}

impl LayerRole {
    pub fn is_copper(&self) -> bool {
        matches!(self, LayerRole::Copper { .. })
    }
}

/// Lowercased file *stem*-and-extension view used by the matchers.
struct Name<'a> {
    /// Full lowercased file name (`board-f_cu.gbr`).
    full: String,
    /// Lowercased extension without the dot (`gbr`, `gtl`, `drl`), if any.
    ext: Option<String>,
    /// Original (case-preserved) layer label we surface to the user.
    original: &'a str,
}

impl<'a> Name<'a> {
    fn of(path: &'a Path) -> Self {
        let file = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        let ext = path
            .extension()
            .and_then(|s| s.to_str())
            .map(|s| s.to_ascii_lowercase());
        Name {
            full: file.to_ascii_lowercase(),
            ext,
            original: path.file_stem().and_then(|s| s.to_str()).unwrap_or(""),
        }
    }

    fn has(&self, needle: &str) -> bool {
        self.full.contains(needle)
    }

    fn ext_is(&self, e: &str) -> bool {
        self.ext.as_deref() == Some(e)
    }
}

/// Classify a single fab file by its path. Inner-copper indices are
/// provisional (see [`assign_inner_indices`]).
pub fn classify(path: &Path) -> LayerRole {
    let n = Name::of(path);

    // ── Drill first: extension is the strongest signal ──────────────────────
    if n.ext_is("drl") || n.ext_is("txt") || n.ext_is("xln") || n.ext_is("nc") || n.ext_is("tap") {
        // `.txt` is sometimes a readme; require a drill-ish hint when ambiguous.
        if n.ext_is("txt") && !(n.has("drill") || n.has("drl") || n.has("nc") || n.has("pth")) {
            return LayerRole::Ignored;
        }
        return LayerRole::Drill;
    }
    if n.has("drill") || n.has("-pth") || n.has("-npth") || n.has(".pth") || n.has(".npth") {
        return LayerRole::Drill;
    }

    // ── Things that are explicitly NOT copper (check before generic copper) ──
    // Order matters: paste/mask/silk often also contain "top"/"bottom".
    let non_copper = [
        ("paste", &["gtp", "gbp"][..]),
        ("mask", &["gts", "gbs"][..]),
        ("solder", &["gts", "gbs", "gtp", "gbp"][..]), // soldermask/solderpaste
        ("silk", &["gto", "gbo"][..]),
        ("legend", &["gto", "gbo"][..]),
        ("overlay", &["gto", "gbo"][..]),
        ("courtyard", &[][..]),
        ("fab", &[][..]),
        ("assembly", &[][..]),
        ("adhes", &["gma", "gba"][..]),
        ("glue", &[][..]),
    ];
    for (word, exts) in non_copper {
        if n.has(word) || exts.iter().any(|e| n.ext_is(e)) {
            return LayerRole::Ignored;
        }
    }

    // ── Outline / edge ──────────────────────────────────────────────────────
    if n.has("edge") || n.has("outline") || n.has("boardoutline") || n.has("-gko") || n.has("margin")
        || n.ext_is("gko") || n.ext_is("gm1") || n.ext_is("gml")
    {
        return LayerRole::Outline;
    }

    // ── Copper by Protel/Altium extension ───────────────────────────────────
    if n.ext_is("gtl") {
        return LayerRole::Copper { index: 0, name: top_label(n.original) };
    }
    if n.ext_is("gbl") {
        return LayerRole::Copper { index: usize::MAX, name: bottom_label(n.original) };
    }
    // Inner copper: .G1L/.G2L… or .GP1/.GP2… or .G1/.G2…
    if let Some(idx) = protel_inner_index(&n) {
        return LayerRole::Copper {
            index: idx,
            name: n.original.to_string(),
        };
    }

    // ── Copper by KiCad/long name ───────────────────────────────────────────
    // `*-F_Cu.gbr`, `*-B_Cu.gbr`, `*-In1_Cu.gbr` (case-insensitive).
    let has_copper = |n: &Name| n.has("cu") || n.has("copper");
    if n.has("f_cu") || n.has("f.cu") || (n.has("top") && has_copper(&n)) || n.has("toplayer")
        || n.has("top layer")
    {
        return LayerRole::Copper { index: 0, name: top_label(n.original) };
    }
    if n.has("b_cu") || n.has("b.cu") || (n.has("bottom") && has_copper(&n)) || n.has("bottomlayer")
        || n.has("bottom layer")
    {
        return LayerRole::Copper { index: usize::MAX, name: bottom_label(n.original) };
    }
    if let Some(idx) = kicad_inner_index(&n) {
        return LayerRole::Copper {
            index: idx,
            name: n.original.to_string(),
        };
    }

    LayerRole::Unknown
}

fn top_label(orig: &str) -> String {
    if orig.is_empty() { "F.Cu".to_string() } else { orig.to_string() }
}
fn bottom_label(orig: &str) -> String {
    if orig.is_empty() { "B.Cu".to_string() } else { orig.to_string() }
}

/// Protel inner-copper extension: `.g1l`/`.g2l`…, `.gp1`/`.gp2`…, `.g1`/`.g2`…
/// Returns the 1-based inner position mapped to a provisional stack index.
fn protel_inner_index(n: &Name) -> Option<usize> {
    let ext = n.ext.as_deref()?;
    // gNl  (g1l, g2l, ... g14l)
    if let Some(rest) = ext.strip_prefix('g') {
        if let Some(digits) = rest.strip_suffix('l') {
            if let Ok(k) = digits.parse::<usize>() {
                if k >= 1 {
                    return Some(k); // provisional: inner k sits at stack pos k
                }
            }
        }
        // gpN (internal plane N)
        if let Some(digits) = rest.strip_prefix('p') {
            if let Ok(k) = digits.parse::<usize>() {
                return Some(k);
            }
        }
        // gN (g1, g2 ... bare)
        if let Ok(k) = rest.parse::<usize>() {
            if k >= 1 {
                return Some(k);
            }
        }
    }
    None
}

/// KiCad inner-copper name: `*-In1_Cu.gbr`, `*-In2_Cu.gbr`, … or
/// `inner1`, `signal2`, etc.
fn kicad_inner_index(n: &Name) -> Option<usize> {
    // Look for "in<k>" followed by "cu", or "inner<k>", or "signal<k>".
    for marker in ["in", "inner", "signal", "layer"] {
        if let Some(pos) = n.full.find(marker) {
            let tail = &n.full[pos + marker.len()..];
            let digits: String = tail.chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(k) = digits.parse::<usize>() {
                // Require it to actually be a copper layer (mask/silk also have
                // "layer" words; those were filtered already above).
                if k >= 1 && (n.has("cu") || marker == "signal" || marker == "inner") {
                    return Some(k);
                }
            }
        }
    }
    None
}

/// Once every copper file is classified, resolve the provisional indices into a
/// dense top-to-bottom ordering. Top stays 0; bottom (the `usize::MAX`
/// sentinel) becomes the last index; inner layers keep their relative order.
///
/// Input/output: a list of `(role, original_index_into_caller_vec)` for the
/// copper files only. Returns the same list with `Copper.index` rewritten to a
/// dense `0..n` stack position.
pub fn assign_inner_indices(mut coppers: Vec<(LayerRole, usize)>) -> Vec<(LayerRole, usize)> {
    let n = coppers.len();
    if n == 0 {
        return coppers;
    }
    // Stable sort by provisional index (top=0, inner=1..,, bottom=MAX last).
    coppers.sort_by_key(|(role, _)| match role {
        LayerRole::Copper { index, .. } => *index,
        _ => usize::MAX,
    });
    for (dense, (role, _)) in coppers.iter_mut().enumerate() {
        if let LayerRole::Copper { index, .. } = role {
            *index = dense;
        }
    }
    coppers
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn role(name: &str) -> LayerRole {
        classify(&PathBuf::from(name))
    }

    #[test]
    fn kicad_names() {
        assert!(matches!(role("board-F_Cu.gbr"), LayerRole::Copper { index: 0, .. }));
        assert!(matches!(role("board-B_Cu.gbr"), LayerRole::Copper { index: usize::MAX, .. }));
        assert!(matches!(role("board-In1_Cu.gbr"), LayerRole::Copper { index: 1, .. }));
        assert!(matches!(role("board-In2_Cu.gbr"), LayerRole::Copper { index: 2, .. }));
        assert_eq!(role("board-F_Mask.gbr"), LayerRole::Ignored);
        assert_eq!(role("board-F_Silkscreen.gbr"), LayerRole::Ignored);
        assert_eq!(role("board-F_Paste.gbr"), LayerRole::Ignored);
        assert_eq!(role("board-Edge_Cuts.gbr"), LayerRole::Outline);
        assert_eq!(role("board-PTH.drl"), LayerRole::Drill);
        assert_eq!(role("board-NPTH.drl"), LayerRole::Drill);
    }

    #[test]
    fn protel_extensions() {
        assert!(matches!(role("design.GTL"), LayerRole::Copper { index: 0, .. }));
        assert!(matches!(role("design.gbl"), LayerRole::Copper { index: usize::MAX, .. }));
        assert!(matches!(role("design.G1L"), LayerRole::Copper { index: 1, .. }));
        assert_eq!(role("design.GTS"), LayerRole::Ignored);
        assert_eq!(role("design.GTO"), LayerRole::Ignored);
        assert_eq!(role("design.GKO"), LayerRole::Outline);
        assert_eq!(role("design.TXT"), LayerRole::Ignored); // bare .txt = readme
        assert_eq!(role("design-drill.txt"), LayerRole::Drill);
        assert_eq!(role("design.drl"), LayerRole::Drill);
    }

    #[test]
    fn generic_words() {
        assert!(matches!(role("TopLayer.gbr"), LayerRole::Copper { index: 0, .. }));
        assert!(matches!(role("Bottom Copper.gbr"), LayerRole::Copper { index: usize::MAX, .. }));
    }

    #[test]
    fn dense_reorder() {
        let coppers = vec![
            (LayerRole::Copper { index: usize::MAX, name: "B".into() }, 0),
            (LayerRole::Copper { index: 0, name: "F".into() }, 1),
            (LayerRole::Copper { index: 2, name: "In2".into() }, 2),
            (LayerRole::Copper { index: 1, name: "In1".into() }, 3),
        ];
        let out = assign_inner_indices(coppers);
        // Order: F(0), In1(1), In2(2), B(3)
        let idxs: Vec<usize> = out
            .iter()
            .map(|(r, _)| match r {
                LayerRole::Copper { index, .. } => *index,
                _ => 999,
            })
            .collect();
        assert_eq!(idxs, vec![0, 1, 2, 3]);
        // The original-vec back-references follow the names.
        let names: Vec<&str> = out
            .iter()
            .map(|(r, _)| match r {
                LayerRole::Copper { name, .. } => name.as_str(),
                _ => "",
            })
            .collect();
        assert_eq!(names, vec!["F", "In1", "In2", "B"]);
    }
}
