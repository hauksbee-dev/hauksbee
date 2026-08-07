//! Gerber + pick-and-place reverse extraction.
//!
//! A large tier of real hardware ships *manufacturing* files (RS-274X copper,
//! Excellon drill, a pick-and-place CSV, sometimes a BOM) but no native CAD.
//! This module reconstructs an [`ExtractedBoard`] (nets + components + pads)
//! from those, so the rest of hauksbee (bind, DRC, lint, stress, sim) works on
//! boards that otherwise couldn't be ingested at all.
//!
//! ## Pipeline
//!
//! 1. **Classify** every file in the job directory by name
//!    ([`layers`]): which are copper, which are the drill, what to ignore.
//! 2. **Parse** copper layers ([`rs274x`]) into solid primitives, the drill
//!    ([`excellon`]) into plated/unplated holes, the P&P + BOM ([`placement`]).
//! 3. **Reconstruct** connectivity ([`connect`]): copper that touches is one
//!    net (R-tree union-find), plated holes stitch layers, placed components
//!    claim nearby flashes as pads.
//!
//! ## What degrades without each input
//!
//! - **No P&P**: nets and geometry (DRC) still reconstruct from copper alone,
//!   but components cannot be bound (we have pads with nets, but nothing tells
//!   us which pads form which part, nor the part's value). [`from_gerber_dir`]
//!   returns the nets with zero components in that case.
//! - **No BOM**: components still bind; their `value`/part-number is only the
//!   P&P `Val`/`Package` field rather than an enriched MPN.
//! - **No drill**: single-layer boards are fine; multi-layer boards lose
//!   layer-to-layer stitching (each layer's copper becomes separate nets).
//!
//! Long-form how-and-why: docs/how-and-why/hauksbee-extract/gerber.md.

pub mod connect;
pub mod excellon;
pub mod geo;
pub mod layers;
pub mod macros;
pub mod placement;
pub mod rs274x;

use std::path::Path;

use crate::{ExtractError, ExtractedBoard};

use connect::{LayerSpan, PlatedHole, ReconStats};
use layers::LayerRole;

/// A reverse extraction, plus the honest accounting that lets callers report
/// how much was recovered.
pub struct GerberExtraction {
    pub board: ExtractedBoard,
    pub stats: ReconStats,
}

/// Recursively collect every file under `dir` (fab jobs sometimes nest the
/// copper / drill / assembly films in sub-directories, e.g. Allegro's
/// `*_CAM` / `*_SMT` / `*_ASM` split).
fn collect_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect_files(&p, out);
        } else if p.is_file() {
            out.push(p);
        }
    }
}

/// Decide whether a drill file is actually an RS-274X "gerber-format" drill film
/// (each flash is a hole) rather than an Excellon drill program. Keys on
/// STRUCTURAL RS-274X markers only, `%FS` (format spec) and `%AD` (aperture
/// definition), which an Excellon file never contains, plus the `.art`
/// extension. A plain-text word like "Gerber" is NOT a marker: Excellon
/// exporters routinely write a generator/description banner mentioning Gerber,
/// and matching that substring misrouted real drill files into the gerber parser
/// (dropping every hole). `head` is the file's leading text.
fn drill_is_gerber_format(head: &str, ext: Option<&str>) -> bool {
    head.contains("%FS")
        || head.contains("%AD")
        || ext.map(|s| s.eq_ignore_ascii_case("art")).unwrap_or(false)
}

/// Reverse-extract from a directory of gerber/drill/P&P files.
///
/// Detection is by file name (see [`layers::classify`]), recursing into
/// sub-directories. An optional `layer_map.txt` / `*.map` mapping file in the
/// directory overrides the name-based role guess for exotic jobs. The board
/// name is the directory's file name. The pick-and-place is picked up from a
/// `.csv`/`.pos` that parses as one, or an Allegro `smt_loc.txt`.
pub fn from_gerber_dir(dir: &Path) -> Result<GerberExtraction, ExtractError> {
    let mut all_files = Vec::new();
    collect_files(dir, &mut all_files);

    // Optional mapping-file escape hatch: `layer_map.txt` or any `*.map`.
    let mut mapping: std::collections::HashMap<String, LayerRole> =
        std::collections::HashMap::new();
    for p in &all_files {
        let n = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
        let is_map = n.eq_ignore_ascii_case("layer_map.txt")
            || p.extension()
                .and_then(|s| s.to_str())
                .map(|s| s.eq_ignore_ascii_case("map"))
                .unwrap_or(false);
        if is_map {
            if let Ok(text) = std::fs::read_to_string(p) {
                mapping.extend(layers::parse_mapping(&text));
            }
        }
    }

    let mut copper: Vec<(LayerRole, std::path::PathBuf)> = Vec::new();
    let mut drills: Vec<std::path::PathBuf> = Vec::new();
    let mut outlines: Vec<std::path::PathBuf> = Vec::new();
    let mut csvs: Vec<std::path::PathBuf> = Vec::new();
    let mut loc_files: Vec<std::path::PathBuf> = Vec::new();

    for path in all_files {
        let fname = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        // Mapping override first, else name-based classification.
        let role = mapping
            .get(&fname)
            .cloned()
            .unwrap_or_else(|| layers::classify(&path));
        match role {
            r @ LayerRole::Copper { .. } => copper.push((r, path)),
            LayerRole::Drill => drills.push(path),
            LayerRole::Outline => outlines.push(path),
            _ => {
                let ext = path
                    .extension()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_ascii_lowercase();
                let lname = fname.to_ascii_lowercase();
                if ext == "csv" || ext == "pos" {
                    csvs.push(path);
                } else if lname.contains("loc")
                    || lname.contains("place")
                    || lname.contains("pos")
                    || lname.contains("pnp")
                    || lname.contains("xy")
                {
                    // Allegro `smt_loc.txt` and similar component-location files.
                    loc_files.push(path);
                }
            }
        }
    }

    if copper.is_empty() {
        return Err(ExtractError::Gerber(
            "no copper gerber layers found here; point hauksbee at the fab output \
             folder (or a zip of it) that contains the copper layer files \
             (.gtl/.gbl, or *-F_Cu.gbr style) alongside the drill file"
                .to_string(),
        ));
    }

    // Resolve copper layer order (top -> bottom).
    let role_only: Vec<(LayerRole, usize)> = copper
        .iter()
        .enumerate()
        .map(|(i, (r, _))| (r.clone(), i))
        .collect();
    let ordered = layers::assign_inner_indices(role_only);

    // Parse each copper layer into primitives, in stack order.
    let mut layer_prims: Vec<Vec<rs274x::CopperPrim>> = vec![Vec::new(); ordered.len()];
    for (role, orig_idx) in &ordered {
        let LayerRole::Copper { index, .. } = role else {
            continue;
        };
        let path = &copper[*orig_idx].1;
        let text = std::fs::read_to_string(path)
            .map_err(|e| ExtractError::Xml(format!("read {}: {e}", path.display())))?;
        match rs274x::parse_layer(&text) {
            Ok(prims) => layer_prims[*index] = prims,
            Err(e) => {
                return Err(ExtractError::Xml(format!(
                    "parse copper {}: {e}",
                    path.display()
                )))
            }
        }
    }

    // Parse drills -> plated holes only (NPTH are mechanical). A drill is
    // usually Excellon, but some tools (Allegro `.art`) emit it as a *gerber*
    // film with the holes drawn as flashes. We sniff: a gerber drill carries
    // RS-274X markers (`%FS`/`%MO`/`G04`), in which case the flash centres are
    // the hole locations; otherwise it is Excellon.
    let n_copper = ordered.len();
    let token_to_stack = copper_layer_tokens(&ordered, &copper);

    // Pass one: read every plated drill file and work out what it says about
    // the copper layers its hits reach. Nothing is stitched yet, because
    // whether SILENCE means "through-hole" depends on the rest of the job.
    enum DrillBody {
        /// An Excellon program, already parsed into hits.
        Excellon(excellon::DrillFile),
        /// A gerber film whose flashes are the hole locations. Kept as text
        /// because the RS-274X plotter is the thing that reads it.
        Film(String),
    }
    struct ParsedDrill {
        path: std::path::PathBuf,
        body: DrillBody,
        claim: SpanClaim,
    }
    let mut parsed: Vec<ParsedDrill> = Vec::new();
    for d in &drills {
        let text = std::fs::read_to_string(d)
            .map_err(|e| ExtractError::Xml(format!("read {}: {e}", d.display())))?;
        let n = d
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let plated = !(n.contains("npth") || n.contains("non-plated") || n.contains("nonplated"));
        if !plated {
            continue;
        }
        let head: String = text.chars().take(256).collect();
        let is_gerber = drill_is_gerber_format(&head, d.extension().and_then(|s| s.to_str()));
        // An X2 attribute in the file body beats the file name; the name is
        // consulted only when the file itself is silent.
        let (body, declared) = if is_gerber {
            (DrillBody::Film(text), None)
        } else {
            let drill = excellon::parse(&text);
            let declared = drill
                .span
                .and_then(|p| resolve_pair(p.from, p.to, n_copper));
            (DrillBody::Excellon(drill), declared)
        };
        let claim = match declared {
            Some((f, t)) => SpanClaim::Resolved(f, t),
            None => match span_from_filename(&n, &token_to_stack, n_copper) {
                Some((f, t)) => SpanClaim::Resolved(f, t),
                None if names_a_partial_span(&n) => SpanClaim::PartialButUnreadable,
                None => SpanClaim::Silent,
            },
        };
        parsed.push(ParsedDrill {
            path: d.clone(),
            body,
            claim,
        });
    }

    // Does this job actually carry a multi-span drill set? Only then is a
    // silent file ambiguous. A job whose every declaration is the full stack is
    // a plain through-hole job, and reading a silent sibling as through-hole
    // there is not a guess, it is the only thing the set can mean.
    let job_is_multi_span = parsed.iter().any(|p| match p.claim {
        SpanClaim::Resolved(f, t) => (f, t) != (0, n_copper.saturating_sub(1)),
        SpanClaim::PartialButUnreadable => true,
        SpanClaim::Silent => false,
    });

    // Pass two: turn the hits into plated barrels with their resolved span.
    let mut holes: Vec<PlatedHole> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    for p in parsed {
        let d = &p.path;
        let span = match p.claim {
            SpanClaim::Resolved(f, t) => LayerSpan::Range { from: f, to: t },
            SpanClaim::PartialButUnreadable | SpanClaim::Silent if job_is_multi_span => {
                notes.push(format!(
                    "{}: this job's drill set spans several layer pairs, and this file does not \
                     say which pair its hits reach. Its plated hits are recorded but stitch no \
                     layers, so nets that only meet through them are reported separately. \
                     Reading them as through-holes would merge nets the stackup keeps apart. \
                     Supply the X2 TF.FileFunction layer pair, or name the file after its pair \
                     (for example -L1-L2.drl or -F_Cu-In1_Cu.drl), to recover them.",
                    d.file_name().and_then(|s| s.to_str()).unwrap_or("drill")
                ));
                LayerSpan::Unknown
            }
            SpanClaim::PartialButUnreadable | SpanClaim::Silent => LayerSpan::Through,
        };

        if let DrillBody::Film(text) = &p.body {
            // Gerber-format drill: each flash is a hole; its disc radius is the
            // drill radius. A drawn path is a rout, but only on a film the job
            // names as a rout/slot layer: on an ordinary drill film the draws
            // are legend art, and reading those as plated slots would paint
            // copper across the whole board.
            let rout_film = d
                .file_name()
                .and_then(|s| s.to_str())
                .map(|s| {
                    let l = s.to_ascii_lowercase();
                    l.contains("rout") || l.contains("slot") || l.contains("mill")
                })
                .unwrap_or(false);
            if let Ok(prims) = rs274x::parse_layer(text) {
                for pr in prims {
                    match pr.kind {
                        rs274x::PrimKind::Flash => {
                            let (x, y) = pr.shape.center();
                            let dia = match &pr.shape {
                                geo::Shape::Capsule(c) => c.r * 2.0,
                                geo::Shape::Polygon { .. } | geo::Shape::MultiPolygon { .. } => 0.3,
                            };
                            holes.push(PlatedHole {
                                x,
                                y,
                                diameter: dia,
                                to: None,
                                span,
                            });
                        }
                        rs274x::PrimKind::Track if rout_film => {
                            if let geo::Shape::Capsule(c) = &pr.shape {
                                holes.push(PlatedHole {
                                    x: c.ax,
                                    y: c.ay,
                                    diameter: c.r * 2.0,
                                    to: Some((c.bx, c.by)),
                                    span,
                                });
                            }
                        }
                        _ => {}
                    }
                }
            }
        } else if let DrillBody::Excellon(drill) = p.body {
            if drill.plated.unwrap_or(true) {
                holes.extend(drill.holes.into_iter().map(|h| PlatedHole {
                    x: h.x,
                    y: h.y,
                    diameter: h.diameter,
                    to: h.to,
                    span,
                }));
            }
        }
    }

    // The board outline, for the castellation count. Not connectivity: an
    // outline is a cut, and a cut joins nothing.
    let outline_prims = read_outline(&outlines);

    // Parse P&P + BOM from the CSVs. Any CSV that yields placements is a P&P
    // file, a job with separate top and bottom placement CSVs contributes both,
    // so we EXTEND rather than keep only the first (the second is not a BOM). A
    // CSV that yields no placements (no X/Y columns) is tried as a BOM.
    let mut placements: Vec<placement::Placement> = Vec::new();
    let mut bom: std::collections::HashMap<String, placement::BomEntry> =
        std::collections::HashMap::new();
    for c in &csvs {
        let Ok(text) = std::fs::read_to_string(c) else {
            continue;
        };
        let pnp = placement::parse_pnp(&text);
        if !pnp.is_empty() {
            placements.extend(pnp);
        } else {
            let b = placement::parse_bom(&text);
            if !b.is_empty() {
                bom.extend(b);
            }
        }
    }
    // Allegro-style component-location files (`smt_loc.txt`): tried when no CSV
    // P&P was found, or when a `*loc*`/`*place*` text file exists.
    if placements.is_empty() {
        for l in &loc_files {
            let Ok(text) = std::fs::read_to_string(l) else {
                continue;
            };
            let pnp = placement::parse_allegro_loc(&text);
            if !pnp.is_empty() {
                placements = pnp;
                break;
            }
        }
    }

    let name = dir
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("gerber")
        .to_string();

    let n_castellations = count_castellations(&holes, &outline_prims);
    let (mut board, mut stats) = connect::reconstruct(&name, layer_prims, holes, placements);
    stats.n_castellations = n_castellations;
    if stats.refused_span_holes > 0 {
        notes.push(format!(
            "{} plated hit(s) on this job stitch no layers because their copper layer span is \
             not derivable from the files provided. The net count is therefore an over-estimate: \
             conductors that meet only through those hits are reported as separate nets.",
            stats.refused_span_holes
        ));
    }
    for n in &notes {
        eprintln!("hauksbee: {n}");
    }
    stats.notes = notes;

    // Enrich components from the BOM (value / part number / do-not-populate).
    if !bom.is_empty() {
        for c in &mut board.components {
            if let Some(entry) = bom.get(&c.reference) {
                if !entry.value.is_empty() {
                    c.value = entry.value.clone();
                }
                if !entry.mpn.is_empty() {
                    c.properties
                        .push(("part_number".to_string(), entry.mpn.clone()));
                }
                // A BOM marking is authoritative for populate state; never clear
                // a DNP already established by the P&P side.
                c.dnp = c.dnp || entry.dnp;
            }
        }
    }

    warn_if_nets_are_fragmented(&board);
    Ok(GerberExtraction { board, stats })
}

/// What one drill file told us about the copper layers its hits reach.
enum SpanClaim {
    /// A layer pair we resolved to concrete stack indices (inclusive).
    Resolved(usize, usize),
    /// The file's name says the hits are blind or buried, so they do NOT reach
    /// the whole stack, but it does not say which layers they do reach. There
    /// is no safe reading of this, only a refusal.
    PartialButUnreadable,
    /// The file said nothing about a span. Safe as a through-hole on a job with
    /// no other span in it; ambiguous on a job that has one.
    Silent,
}

/// Turn a 1-based X2 layer pair into inclusive 0-based stack indices, rejecting
/// any pair that names a layer this job does not have. A pair we cannot place
/// in the stack is unresolved; it is never clamped into one, because clamping
/// `1,6` on a four-layer job silently produces the through-hole we are trying
/// not to invent.
fn resolve_pair(from: u32, to: u32, n_copper: usize) -> Option<(usize, usize)> {
    if from == 0 || to <= from || n_copper == 0 {
        return None;
    }
    let (f, t) = ((from - 1) as usize, (to - 1) as usize);
    (t < n_copper).then_some((f, t))
}

/// Map the copper layer name tokens a drill file might be named after
/// (`f_cu`, `b_cu`, `in1_cu`, `l1`, `l2`, ...) onto stack indices, using the
/// copper files this job actually carries. KiCad names a blind/buried drill
/// file after the two layers it joins (`board-F_Cu-In1_Cu.drl`), so the token
/// has to resolve against the same stack the copper resolved into rather than
/// against a fixed table.
fn copper_layer_tokens(
    ordered: &[(LayerRole, usize)],
    copper: &[(LayerRole, std::path::PathBuf)],
) -> std::collections::HashMap<String, usize> {
    let mut out: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let n = ordered.len();
    for (role, orig_idx) in ordered {
        let LayerRole::Copper { index, .. } = role else {
            continue;
        };
        // `L<n>` is the X2 physical numbering: L1 is the top copper.
        out.insert(format!("l{}", index + 1), *index);
        let Some((_, path)) = copper.get(*orig_idx) else {
            continue;
        };
        let fname = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if fname.contains("f_cu") || fname.contains("f.cu") {
            out.insert("f_cu".to_string(), *index);
        }
        if fname.contains("b_cu") || fname.contains("b.cu") {
            out.insert("b_cu".to_string(), *index);
        }
        if let Some(k) = inner_layer_number(&fname) {
            out.insert(format!("in{k}_cu"), *index);
        }
    }
    // A job whose copper files are not KiCad-named still resolves `f_cu`/`b_cu`
    // to the ends of the stack it does have.
    if n > 0 {
        out.entry("f_cu".to_string()).or_insert(0);
        out.entry("b_cu".to_string()).or_insert(n - 1);
    }
    out
}

/// The `N` in an `in<N>_cu` / `in<N>.cu` token inside a file name.
fn inner_layer_number(fname: &str) -> Option<u32> {
    let b: Vec<char> = fname.chars().collect();
    for i in 0..b.len().saturating_sub(2) {
        if b[i] != 'i' || b[i + 1] != 'n' || !b[i + 2].is_ascii_digit() {
            continue;
        }
        // Must be a whole token: no letter immediately before the `in`.
        if i > 0 && b[i - 1].is_ascii_alphabetic() {
            continue;
        }
        let digits: String = b[i + 2..].iter().take_while(|c| c.is_ascii_digit()).collect();
        let after = i + 2 + digits.len();
        let tail: String = b[after..].iter().take(3).collect();
        if tail.starts_with("_cu") || tail.starts_with(".cu") {
            return digits.parse().ok();
        }
    }
    None
}

/// Recover a layer pair from a drill file's name: `-L1-L2.drl`,
/// `-F_Cu-In1_Cu.drl`, `_l2_l3.txt`. Returns inclusive 0-based stack indices.
///
/// Only *two* distinct resolvable layer tokens count, and they must be ordered
/// top-to-bottom after resolution. A name carrying one token, or three, or a
/// bare number pair with no layer marker, resolves to nothing: a filename is
/// weak evidence and a wrong pair is a wrong stackup.
fn span_from_filename(
    fname: &str,
    tokens: &std::collections::HashMap<String, usize>,
    n_copper: usize,
) -> Option<(usize, usize)> {
    if n_copper == 0 {
        return None;
    }
    let mut found: Vec<usize> = Vec::new();
    // Longest tokens first so `in1_cu` is not eaten by a shorter prefix.
    let mut keys: Vec<&String> = tokens.keys().collect();
    keys.sort_by_key(|k| std::cmp::Reverse(k.len()));
    let mut consumed = vec![false; fname.len()];
    for k in keys {
        let mut from = 0usize;
        while let Some(rel) = fname[from..].find(k.as_str()) {
            let at = from + rel;
            from = at + 1;
            if consumed[at..at + k.len()].iter().any(|c| *c) {
                continue;
            }
            // Whole-token match: no digit or letter may run into it.
            let before_ok = at == 0 || !fname.as_bytes()[at - 1].is_ascii_alphanumeric();
            let end = at + k.len();
            let after_ok = end >= fname.len() || !fname.as_bytes()[end].is_ascii_alphanumeric();
            if !before_ok || !after_ok {
                continue;
            }
            for c in consumed[at..end].iter_mut() {
                *c = true;
            }
            found.push(tokens[k]);
        }
    }
    found.sort_unstable();
    found.dedup();
    match found.as_slice() {
        [a, b] => Some((*a, *b)),
        _ => None,
    }
}

/// The file name says these hits are blind or buried, i.e. definitely not a
/// through-hole, without saying which layers they reach.
fn names_a_partial_span(fname: &str) -> bool {
    fname.contains("blind") || fname.contains("buried")
}

/// Parse the board outline films into primitives. Purely for the castellation
/// count: an outline is a cut line, never a conductor, so these primitives are
/// deliberately kept out of the connectivity graph.
fn read_outline(outlines: &[std::path::PathBuf]) -> Vec<geo::Shape> {
    let mut out = Vec::new();
    for p in outlines {
        let Ok(text) = std::fs::read_to_string(p) else {
            continue;
        };
        if let Ok(prims) = rs274x::parse_layer(&text) {
            out.extend(prims.into_iter().map(|p| p.shape));
        }
    }
    out
}

/// Count plated hits whose barrel the board outline cuts through: castellations
/// and plated edge slots.
///
/// A castellation is a half-hole on the board edge. Its copper ring is sliced
/// by the outline, so a reader that decides "this pad owns this hole" by
/// testing whether the hole sits wholly inside a closed pad ring finds no
/// owner and drops the connection. Hauksbee never asks that question: the
/// barrel is copper and joins whatever copper it touches, which is what a
/// castellation physically is. This count exists so the claim can be checked
/// against the board rather than assumed, and so a job with castellations is
/// visible as such in the reconstruction stats.
fn count_castellations(holes: &[PlatedHole], outline: &[geo::Shape]) -> usize {
    if outline.is_empty() {
        return 0;
    }
    holes
        .iter()
        .filter(|h| {
            let r = (h.diameter / 2.0).max(0.05);
            let barrel = match h.to {
                None => geo::Shape::disc(h.x, h.y, r),
                Some((tx, ty)) => geo::Shape::Capsule(geo::Capsule {
                    ax: h.x,
                    ay: h.y,
                    bx: tx,
                    by: ty,
                    r,
                }),
            };
            outline
                .iter()
                .any(|o| geo::shape_gap(&barrel, o) <= 0.0)
        })
        .count()
}

/// Say out loud when the reconstructed net count is mostly copper fragments.
///
/// Reverse extraction unions copper that touches. Where it cannot follow the
/// geometry (a pour whose region the parser does not close, an arc it
/// approximates too coarsely, a thermal relief), one real net comes out as
/// several, and the net COUNT is then an over-estimate. Measured on 13 real fab
/// jobs that carried a placement file, the ratio ranged from 1.6 to 21 nets per
/// part: the high end is over-segmentation, not a board with 21 nets per part.
///
/// This is an exact statement, not a heuristic: a reconstructed net that no
/// component pad sits on cannot be a net anybody routed to, so it is a fragment
/// the reconstruction failed to attach. Reporting how many keeps the net count
/// honest instead of letting a plausible-looking number stand unqualified.
fn warn_if_nets_are_fragmented(board: &ExtractedBoard) {
    use std::collections::HashSet;
    if board.components.is_empty() || board.nets.is_empty() {
        return;
    }
    let on_a_pad: HashSet<i64> = board
        .components
        .iter()
        .flat_map(|c| c.pins.iter().filter_map(|p| p.net))
        .collect();
    let orphans = board
        .nets
        .iter()
        .filter(|n| !on_a_pad.contains(&n.id))
        .count();
    if orphans * 2 > board.nets.len() {
        eprintln!(
            "hauksbee: {orphans} of {} nets reconstructed from this gerber job touch no \
             component pad, so they are copper fragments that did not merge into a \
             routed net (a pour or thermal relief the geometry pass could not follow). \
             The net count is an upper bound and net-by-net results on this job are \
             unreliable; the {} placed part(s) and their pad connections are unaffected.",
            board.nets.len(),
            board.components.len()
        );
    }
}

/// Reverse-extract from a gerber job `.zip`. Extracts to a temp dir and
/// delegates to [`from_gerber_dir`].
pub fn from_gerber_zip(zip_path: &Path) -> Result<GerberExtraction, ExtractError> {
    let bytes = std::fs::read(zip_path)
        .map_err(|e| ExtractError::Xml(format!("read zip {}: {e}", zip_path.display())))?;
    let tmp = std::env::temp_dir().join(format!(
        "hauksbee_gerber_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&tmp).map_err(|e| ExtractError::Xml(format!("mktemp: {e}")))?;
    unzip_into(&bytes, &tmp)?;
    // The zip may wrap a single sub-directory; descend if so.
    let root = single_subdir(&tmp).unwrap_or(tmp.clone());
    let res = from_gerber_dir(&root);
    let _ = std::fs::remove_dir_all(&tmp);
    res
}

/// If `dir` contains exactly one entry and it's a directory, return it.
fn single_subdir(dir: &Path) -> Option<std::path::PathBuf> {
    let mut it = std::fs::read_dir(dir).ok()?.flatten();
    let first = it.next()?.path();
    if it.next().is_none() && first.is_dir() {
        Some(first)
    } else {
        None
    }
}

/// Minimal zip extractor (stored + deflate) so we don't pull a zip crate for
/// one use. Most fab zips are deflate.
fn unzip_into(bytes: &[u8], out: &Path) -> Result<(), ExtractError> {
    use std::io::Read;
    let reader = std::io::Cursor::new(bytes);
    let mut archive =
        zip::ZipArchive::new(reader).map_err(|e| ExtractError::Xml(format!("zip open: {e}")))?;
    for i in 0..archive.len() {
        let mut file = archive
            .by_index(i)
            .map_err(|e| ExtractError::Xml(format!("zip entry {i}: {e}")))?;
        if file.is_dir() {
            continue;
        }
        let name = file
            .enclosed_name()
            .ok_or_else(|| ExtractError::Xml("zip: unsafe path".into()))?;
        // Flatten into the output dir by file name (job zips are flat anyway).
        let dest = out.join(name.file_name().unwrap_or(name.as_os_str()));
        let mut buf = Vec::with_capacity(file.size() as usize);
        file.read_to_end(&mut buf)
            .map_err(|e| ExtractError::Xml(format!("zip read: {e}")))?;
        std::fs::write(&dest, buf)
            .map_err(|e| ExtractError::Xml(format!("zip write {}: {e}", dest.display())))?;
    }
    Ok(())
}

impl ExtractedBoard {
    /// Reverse-extract from a gerber job directory or a gerber `.zip`. The
    /// universal "hand us only the fab files" entry point.
    pub fn from_gerber(path: &Path) -> Result<Self, ExtractError> {
        if path.is_dir() {
            from_gerber_dir(path).map(|g| g.board)
        } else if path
            .extension()
            .and_then(|s| s.to_str())
            .map(|s| s.eq_ignore_ascii_case("zip"))
            .unwrap_or(false)
        {
            from_gerber_zip(path).map(|g| g.board)
        } else {
            Err(ExtractError::Gerber(
                "not a gerber job: expected a directory of gerber files, or a .zip of one"
                    .to_string(),
            ))
        }
    }
}

#[cfg(test)]
mod error_message_tests {
    use super::from_gerber_dir;

    #[test]
    fn no_copper_error_is_one_human_sentence() {
        // The old template collision rendered "not a a directory containing
        // copper gerber files file (root is None)". The message must be a
        // whole sentence with no Rust Option debug in it.
        let dir =
            std::env::temp_dir().join(format!("hauksbee-gerber-empty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let msg = match from_gerber_dir(&dir) {
            Ok(_) => panic!("an empty dir has no copper"),
            Err(e) => e.to_string(),
        };
        assert!(msg.contains("no copper gerber layers"), "got: {msg}");
        assert!(!msg.contains("None"), "no Option debug: {msg}");
        assert!(!msg.contains("not a a"), "no doubled article: {msg}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod drill_sniff_tests {
    use super::drill_is_gerber_format;

    #[test]
    fn excellon_with_gerber_in_banner_is_not_misrouted() {
        // A real Excellon drill program whose header comment names Gerber must
        // NOT be classified as a gerber-format film; it starts with M48 and has
        // no RS-274X structural markers. A `contains("Gerber")` catch-all would
        // send it to the gerber parser and drop every hole.
        let excellon =
            "M48\n; Generated by SomeTool Gerber/Excellon exporter\nFMAT,2\nT1C0.300\n%\n";
        assert!(!drill_is_gerber_format(excellon, Some("drl")));
        assert!(!drill_is_gerber_format(excellon, Some("txt")));
    }

    #[test]
    fn genuine_gerber_drill_film_still_detected() {
        // Real RS-274X markers (or the .art extension) still classify as gerber.
        assert!(drill_is_gerber_format(
            "%FSLAX34Y34*%\n%MOMM*%\n",
            Some("gbr")
        ));
        assert!(drill_is_gerber_format("%ADD10C,0.3*%\n", Some("drl")));
        assert!(drill_is_gerber_format("G04 drill*\n", Some("art")));
    }
}
