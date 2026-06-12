//! Gerber + pick-and-place reverse extraction.
//!
//! A large tier of real hardware ships *manufacturing* files (RS-274X copper,
//! Excellon drill, a pick-and-place CSV, sometimes a BOM) but no native CAD.
//! This module reconstructs an [`ExtractedBoard`] (nets + components + pads)
//! from those, so the rest of galvani (bind, DRC, lint, stress, sim) works on
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

pub mod connect;
pub mod excellon;
pub mod geo;
pub mod layers;
pub mod macros;
pub mod placement;
pub mod rs274x;

use std::path::Path;

use crate::{ExtractError, ExtractedBoard};

use connect::{PlatedHole, ReconStats};
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
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect_files(&p, out);
        } else if p.is_file() {
            out.push(p);
        }
    }
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
            || p.extension().and_then(|s| s.to_str()).map(|s| s.eq_ignore_ascii_case("map")).unwrap_or(false);
        if is_map {
            if let Ok(text) = std::fs::read_to_string(p) {
                mapping.extend(layers::parse_mapping(&text));
            }
        }
    }

    let mut copper: Vec<(LayerRole, std::path::PathBuf)> = Vec::new();
    let mut drills: Vec<std::path::PathBuf> = Vec::new();
    let mut csvs: Vec<std::path::PathBuf> = Vec::new();
    let mut loc_files: Vec<std::path::PathBuf> = Vec::new();

    for path in all_files {
        let fname = path.file_name().and_then(|s| s.to_str()).unwrap_or("").to_string();
        // Mapping override first, else name-based classification.
        let role = mapping
            .get(&fname)
            .cloned()
            .unwrap_or_else(|| layers::classify(&path));
        match role {
            r @ LayerRole::Copper { .. } => copper.push((r, path)),
            LayerRole::Drill => drills.push(path),
            _ => {
                let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("").to_ascii_lowercase();
                let lname = fname.to_ascii_lowercase();
                if ext == "csv" || ext == "pos" {
                    csvs.push(path);
                } else if lname.contains("loc") || lname.contains("place") || lname.contains("pos") || lname.contains("pnp") || lname.contains("xy") {
                    // Allegro `smt_loc.txt` and similar component-location files.
                    loc_files.push(path);
                }
            }
        }
    }

    if copper.is_empty() {
        return Err(ExtractError::WrongRoot {
            expected: "a directory containing copper gerber files",
            found: None,
        });
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
        let LayerRole::Copper { index, .. } = role else { continue };
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
    let mut holes: Vec<PlatedHole> = Vec::new();
    for d in &drills {
        let text = std::fs::read_to_string(d)
            .map_err(|e| ExtractError::Xml(format!("read {}: {e}", d.display())))?;
        let n = d.file_name().and_then(|s| s.to_str()).unwrap_or("").to_ascii_lowercase();
        let plated = !(n.contains("npth") || n.contains("non-plated") || n.contains("nonplated"));
        if !plated {
            continue;
        }
        let head: String = text.chars().take(256).collect();
        let is_gerber = head.contains("%FS") || head.contains("Gerber") || d.extension().and_then(|s| s.to_str()).map(|s| s.eq_ignore_ascii_case("art")).unwrap_or(false);
        if is_gerber {
            // Gerber-format drill: each flash is a hole; its disc radius is the
            // drill radius. Tracks/regions on a drill film are legend art.
            if let Ok(prims) = rs274x::parse_layer(&text) {
                for p in prims {
                    if p.kind == rs274x::PrimKind::Flash {
                        let (x, y) = p.shape.center();
                        let dia = match &p.shape {
                            geo::Shape::Capsule(c) => c.r * 2.0,
                            geo::Shape::Polygon { .. } => 0.3,
                        };
                        holes.push(PlatedHole { x, y, diameter: dia });
                    }
                }
            }
        } else {
            let drill = excellon::parse(&text);
            let plated = drill.plated.unwrap_or(true);
            if plated {
                holes.extend(drill.holes.into_iter().map(|h| PlatedHole {
                    x: h.x,
                    y: h.y,
                    diameter: h.diameter,
                }));
            }
        }
    }

    // Parse P&P + BOM from the CSVs. A CSV that yields placements is the P&P;
    // the rest are tried as BOM.
    let mut placements: Vec<placement::Placement> = Vec::new();
    let mut bom: std::collections::HashMap<String, (String, String)> = std::collections::HashMap::new();
    for c in &csvs {
        let Ok(text) = std::fs::read_to_string(c) else { continue };
        let pnp = placement::parse_pnp(&text);
        if !pnp.is_empty() && placements.is_empty() {
            placements = pnp;
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
            let Ok(text) = std::fs::read_to_string(l) else { continue };
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

    let (mut board, stats) = connect::reconstruct(&name, layer_prims, holes, placements);

    // Enrich components from the BOM (value / part number).
    if !bom.is_empty() {
        for c in &mut board.components {
            if let Some((value, mpn)) = bom.get(&c.reference) {
                if !value.is_empty() {
                    c.value = value.clone();
                }
                if !mpn.is_empty() {
                    c.properties.push(("part_number".to_string(), mpn.clone()));
                }
            }
        }
    }

    Ok(GerberExtraction { board, stats })
}

/// Reverse-extract from a gerber job `.zip`. Extracts to a temp dir and
/// delegates to [`from_gerber_dir`].
pub fn from_gerber_zip(zip_path: &Path) -> Result<GerberExtraction, ExtractError> {
    let bytes = std::fs::read(zip_path)
        .map_err(|e| ExtractError::Xml(format!("read zip {}: {e}", zip_path.display())))?;
    let tmp = std::env::temp_dir().join(format!(
        "galvani_gerber_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&tmp)
        .map_err(|e| ExtractError::Xml(format!("mktemp: {e}")))?;
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
    let mut archive = zip::ZipArchive::new(reader)
        .map_err(|e| ExtractError::Xml(format!("zip open: {e}")))?;
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
        } else if path.extension().and_then(|s| s.to_str()).map(|s| s.eq_ignore_ascii_case("zip")).unwrap_or(false) {
            from_gerber_zip(path).map(|g| g.board)
        } else {
            Err(ExtractError::WrongRoot {
                expected: "a gerber directory or .zip",
                found: None,
            })
        }
    }
}
