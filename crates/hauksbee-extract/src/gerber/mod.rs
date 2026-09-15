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
//! 1. **Classify** every file from the job's own metadata first, then by name
//!    ([`layers`]): which are copper, which are the drill, what to ignore.
//! 2. **Parse** copper layers ([`rs274x`]) into solid primitives, the drill
//!    ([`excellon`]) into plated/unplated holes, the P&P + BOM ([`placement`]).
//! 3. **Reconstruct** connectivity ([`connect`]): copper that touches is one
//!    net (R-tree union-find), a plated hit stitches the layers its declared
//!    span reaches (and refuses to stitch when the files never declared one),
//!    placed components claim nearby flashes as pads.
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

pub mod connect;
pub mod excellon;
pub mod geo;
pub mod layers;
pub mod macros;
pub mod placement;
pub mod rs274x;

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::{ExtractError, ExtractedBoard};

use connect::{LayerSpan, PlatedHole, ReconStats};
use excellon::{DeclaredSpan, Hole, LayerPair};
use layers::{ExtRepRole, GbrJobRole, GbrJobSide, LayerRole};

/// A reverse extraction, plus the honest accounting that lets callers report
/// how much was recovered.
pub struct GerberExtraction {
    pub board: ExtractedBoard,
    pub stats: ReconStats,
}

/// The file's own name, for a message about it. Never the full path: a web
/// upload is read out of a throwaway directory whose name is a pid, a counter
/// and a clock reading, so quoting the path leaks a local absolute path into the
/// report and makes two analyses of one malformed archive differ. The film name
/// is also the only part the user can act on.
fn film(path: &Path) -> std::borrow::Cow<'_, str> {
    path.file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
}

/// Whether `path` carries extension `ext`, case-insensitively.
fn ext_is(path: &Path, ext: &str) -> bool {
    path.extension()
        .and_then(|s| s.to_str())
        .is_some_and(|s| s.eq_ignore_ascii_case(ext))
}

fn read_text(path: &Path, what: &str) -> Result<String, ExtractError> {
    std::fs::read_to_string(path)
        .map_err(|e| ExtractError::Xml(format!("{what} {}: {e}", film(path))))
}

/// A copper role at provisional stack `index`, named after the film's stem.
fn copper_role(index: usize, path: &Path) -> LayerRole {
    LayerRole::Copper {
        index,
        name: path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string(),
    }
}

fn same_layer_role(left: &LayerRole, right: &LayerRole) -> bool {
    match (left, right) {
        (LayerRole::Copper { index: l, .. }, LayerRole::Copper { index: r, .. }) => l == r,
        _ => std::mem::discriminant(left) == std::mem::discriminant(right),
    }
}

/// Resolve one Altium `.LDP` layer walk into the physical pair used by the
/// drill stitcher. `g1` is physical L2, and a terminal `gbl` sits one layer
/// below the deepest named inner layer (or at L2 on a two-layer job).
fn ldp_declared_span(role: &layers::LdpDrillRole, classified_copper_layers: usize) -> DeclaredSpan {
    let bottom = role
        .layers
        .iter()
        .filter_map(|layer| match layer {
            layers::LdpLayer::Inner(n) => Some(n.saturating_add(2)),
            _ => None,
        })
        .max()
        .unwrap_or(2)
        .max(classified_copper_layers.min(u32::MAX as usize) as u32);
    let physical: Vec<u32> = role
        .layers
        .iter()
        .map(|layer| match layer {
            layers::LdpLayer::Top => 1,
            layers::LdpLayer::Inner(n) => n.saturating_add(1),
            layers::LdpLayer::Bottom => bottom,
        })
        .collect();
    match (physical.first(), physical.last()) {
        (Some(&from), Some(&to)) if physical.windows(2).all(|pair| pair[0] < pair[1]) => {
            DeclaredSpan::Pair(LayerPair { from, to })
        }
        _ => DeclaredSpan::Unreadable,
    }
}

/// The file body is closest to the holes and therefore never gets silently
/// overwritten. A package-level `.LDP` fills silence; disagreement between two
/// explicit spans becomes unreadable rather than choosing either one. The flag
/// says whether that happened.
fn merge_declared_span(file: DeclaredSpan, ldp: Option<DeclaredSpan>) -> (DeclaredSpan, bool) {
    match ldp {
        None => (file, false),
        Some(ldp) if file == DeclaredSpan::Absent => (ldp, false),
        Some(ldp) if file == ldp => (file, false),
        Some(_) => (DeclaredSpan::Unreadable, true),
    }
}

fn declared_span_phrase(span: DeclaredSpan) -> String {
    match span {
        DeclaredSpan::Pair(pair) => format!("physical copper span L{}-L{}", pair.from, pair.to),
        DeclaredSpan::Unreadable => "an unreadable copper span".to_string(),
        DeclaredSpan::Absent => "no copper span".to_string(),
    }
}

/// Recursively collect every file under `dir` (fab jobs sometimes nest the
/// copper / drill / assembly films in sub-directories, e.g. Allegro's
/// `*_CAM` / `*_SMT` / `*_ASM` split).
///
/// Sorted by path within each directory, and directories descended in that same
/// sorted order, because `read_dir` yields whatever order the filesystem holds.
/// Downstream this list decides which copper film gets which provisional stack
/// index when a job's names tie, so readdir order would otherwise leak into the
/// reconstruction and two extractions of one archive could disagree.
fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut here: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    here.sort();
    for p in here {
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

/// Up to 64 KiB of the file's leading text, lossily decoded, for
/// classification. Every X2 attribute and Excellon header sits at the top of
/// its file; a binary member yields something no rule matches.
fn read_head(path: &Path) -> String {
    use std::io::Read;
    let Ok(file) = std::fs::File::open(path) else {
        return String::new();
    };
    let mut buf = Vec::new();
    let _ = file.take(64 * 1024).read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

/// The refusal for a job with no copper film, naming every file seen and how
/// it was read, so the fix is a rename and not a search.
fn no_copper_error(inventory: &[(String, LayerRole)]) -> ExtractError {
    const SHOWN: usize = 40;
    let mut seen: Vec<String> = inventory
        .iter()
        .take(SHOWN)
        .map(|(name, role)| format!("{name} ({})", layers::role_phrase(role)))
        .collect();
    if inventory.len() > SHOWN {
        seen.push(format!("and {} more", inventory.len() - SHOWN));
    }
    let listing = if seen.is_empty() {
        "no files at all".to_string()
    } else {
        format!("{} file(s): {}", inventory.len(), seen.join(", "))
    };
    ExtractError::Gerber(format!(
        "no copper gerber layers found here; point hauksbee at the fab output folder (or a \
         zip of it) that contains the copper layer files alongside the drill file. Seen: \
         {listing}. A copper film is recognised by its exporter's own statement (a .gbrjob \
         entry, or an X2 TF.FileFunction,Copper attribute in the film) or by name: \
         .GTL/.GBL/.G1, *-F_Cu.gbr/*-B_Cu.gbr/*-In1_Cu.gbr, .cmp/.sol, Top.gbr/Bottom.gbr. \
         Rename the copper films to one of those, or add a layer_map.txt beside them with \
         one line per film, such as `Top.ger = copper:0` and `Bottom.ger = copper:bottom`"
    ))
}

/// Whether a drill file's name says its holes are NOT plated.
fn name_says_non_plated(fname: &str) -> bool {
    let n = fname.to_ascii_lowercase();
    n.contains("npth") || n.contains("non-plated") || n.contains("nonplated")
}

/// One drill file read into hits, with what its own body said about them.
struct DrillRead {
    hits: Vec<Hole>,
    /// Plated, non-plated, or unstated, from the body alone.
    plated: Option<bool>,
    declared: DeclaredSpan,
    /// A gerber-format film whose drill apertures mix plated and mechanical
    /// functions; plating is assigned per file here, so the film settles nothing.
    mixed_functions: bool,
}

/// Read an Excellon program, carrying its reader notes into the job's.
fn read_excellon(text: &str, fname: &str, notes: &mut Vec<String>) -> DrillRead {
    let drill = excellon::parse(text);
    notes.extend(drill.notes.iter().map(|n| format!("{fname}: {n}")));
    DrillRead {
        hits: drill.holes,
        plated: drill.plated,
        declared: drill.span,
        mixed_functions: false,
    }
}

/// Read a gerber-format drill film: each flash is a hole (an oblong flash a
/// slot), and on a film that declares itself a rout layer each drawn path is
/// a plated wall.
///
/// Plating is taken from the strongest source the film offers: its own
/// `TF.FileFunction`, then its `%TA.AperFunction` drill functions. Whether the
/// drawn paths are routs is settled by the film's OWN attribute only: any
/// board whose project name contains "slot" would otherwise have its legend
/// promoted to conductor.
fn read_drill_film(text: &str, fname: &str, notes: &mut Vec<String>) -> DrillRead {
    let functions = film_drill_functions(text);
    let function = film_file_function(text).unwrap_or_default();
    let plated = if film_is_non_plated(text) || functions == FilmDrillFunctions::AllMechanical {
        Some(false)
    } else if function.contains("PLATED")
        || function.contains("PTH")
        || functions == FilmDrillFunctions::AllPlated
    {
        Some(true)
    } else {
        None
    };
    let declares_rout = ["ROUT", "SLOT", "MILL"]
        .iter()
        .any(|w| function.contains(w));
    let lower = fname.to_ascii_lowercase();
    if !declares_rout && ["rout", "slot", "mill"].iter().any(|w| lower.contains(w)) {
        notes.push(format!(
            "{fname}: this gerber-format drill film is named as a rout or slot layer but does \
             not declare itself one, so the paths drawn on it are left as artwork rather \
             than read as plated walls. A drawn path is only a conductor if the film says \
             it is; promoting it on the strength of a file name would turn any legend on \
             a board whose project name contains \"slot\" into copper. Add a \
             TF.FileFunction naming the layer's role to recover them."
        ));
    }
    let mut hits = Vec::new();
    for pr in rs274x::parse_layer(text).unwrap_or_default() {
        match (pr.kind, &pr.shape) {
            (rs274x::PrimKind::Flash, shape) => {
                // A drill film draws the finished CUTOUT, so an oblong flash is
                // a slot: its narrow side is the tool and its long axis the path
                // that tool swept. A round flash comes back with its two
                // centres coincident, which is a round hole.
                let (dia, from, to) = drill_flash_extent(shape);
                let is_slot = (to.0 - from.0).hypot(to.1 - from.1) > 1e-9;
                hits.push(Hole {
                    x: from.0,
                    y: from.1,
                    diameter: dia,
                    to: is_slot.then_some(to),
                });
            }
            (rs274x::PrimKind::Track, geo::Shape::Capsule(c)) if declares_rout => {
                hits.push(Hole {
                    x: c.ax,
                    y: c.ay,
                    diameter: c.r * 2.0,
                    to: Some((c.bx, c.by)),
                });
            }
            _ => {}
        }
    }
    DrillRead {
        hits,
        plated,
        declared: film_declared_span(text),
        mixed_functions: functions == FilmDrillFunctions::Mixed,
    }
}

/// The hits a pad flash covers on two or more copper layers: the ring CAD
/// draws around a via or a plated pad, and never around a mounting hole.
/// Pours and tracks do not count; a mechanical hole through two planes is
/// exactly the phantom short this rule must not create.
fn hits_under_pad_rings(hits: Vec<Hole>, layers: &[Vec<rs274x::CopperPrim>]) -> Vec<Hole> {
    let ringed = |x: f64, y: f64| {
        layers
            .iter()
            .filter(|prims| {
                prims.iter().any(|p| {
                    let b = p.shape.bounds();
                    p.kind == rs274x::PrimKind::Flash
                        && x >= b[0]
                        && x <= b[2]
                        && y >= b[1]
                        && y <= b[3]
                        && geo::shape_contains_point(&p.shape, x, y)
                })
            })
            .count()
            >= 2
    };
    hits.into_iter().filter(|h| ringed(h.x, h.y)).collect()
}

/// The package-level authorities a job may ship, read before any film is
/// classified. In order of rank: the user's explicit mapping file, the
/// exporter's `.gbrjob` manifest, Altium's `.LDP` drill-pair manifest, and
/// Altium's `.EXTREP` extension report. The file's own attribute, name and
/// body are the fallback.
#[derive(Default)]
struct PackageMetadata {
    /// `layer_map.txt` / `*.map`: one `filename = role` per line.
    mapping: HashMap<String, LayerRole>,
    /// `.LDP` rows by lower-cased basename (manifest and ZIP member routinely
    /// disagree only in case). Names the drill file, whether the set is plated,
    /// and the exact ordered copper layers it reaches.
    ldp_drills: HashMap<String, layers::LdpDrillRole>,
    /// `.EXTREP` extension -> role, with any extension the report assigns
    /// several roles removed (named-output reports reuse `.gbr` for everything).
    extrep_roles: HashMap<String, ExtRepRole>,
    /// `.gbrjob` basename -> role, naming each copper film's declared layer.
    gbrjob: HashMap<String, GbrJobRole>,
    /// Provisional stack index per manifest copper film: its RANK among the
    /// manifest's copper entries (top first, bottom `usize::MAX`), never the
    /// raw declared number. The numbers ORDER the stack even on exporters
    /// whose numbers are not physical positions (KiCad 9 writes internal layer
    /// IDs, so a four-layer manifest can read L1, L5, L7, L4).
    gbrjob_index: HashMap<String, usize>,
    /// Whether the manifest's copper numbers are exactly `1..=n` in rank order,
    /// the only case they are believed as PHYSICAL positions: any other scheme
    /// handed to the drill layer-pair resolver would invent layers the board
    /// does not have.
    gbrjob_numbers_physical: bool,
}

impl PackageMetadata {
    fn read(dir: &Path, files: &[PathBuf], notes: &mut Vec<String>) -> Result<Self, ExtractError> {
        let mut meta = PackageMetadata::default();
        let mut ldp_sources: HashMap<String, String> = HashMap::new();
        let mut extrep_contested: BTreeSet<String> = BTreeSet::new();
        for p in files {
            let name = film(p);
            if name.eq_ignore_ascii_case("layer_map.txt") || ext_is(p, "map") {
                if let Ok(text) = std::fs::read_to_string(p) {
                    meta.mapping.extend(layers::parse_mapping(&text));
                }
            } else if ext_is(p, "gbrjob") {
                if let Ok(text) = std::fs::read_to_string(p) {
                    meta.gbrjob.extend(layers::parse_gbrjob(&text));
                }
            } else if ext_is(p, "ldp") {
                // Conflicting declarations refuse: picking one would fabricate
                // or erase a barrel connection.
                let Ok(text) = std::fs::read_to_string(p) else {
                    notes.push(format!(
                        "{name} could not be read, so its drill-layer metadata was not used."
                    ));
                    continue;
                };
                let rows = layers::parse_ldp(&text);
                if rows.is_empty() {
                    notes.push(format!(
                        "{name} contains no complete DrillFile/DrillLayers declaration, so filename and file-body inference remain the fallback."
                    ));
                }
                for (drill, role) in rows {
                    match meta.ldp_drills.get(&drill) {
                        Some(existing) if *existing != role => {
                            return Err(ExtractError::Gerber(format!(
                                "conflicting .LDP declarations for drill file {drill}: {} and {name}. Resolve the package metadata; choosing either span could merge nets the stack keeps apart",
                                ldp_sources
                                    .get(&drill)
                                    .map(String::as_str)
                                    .unwrap_or("an earlier .LDP file"),
                            )));
                        }
                        Some(_) => {}
                        None => {
                            ldp_sources.insert(drill.clone(), name.to_string());
                            meta.ldp_drills.insert(drill, role);
                        }
                    }
                }
            } else if ext_is(p, "extrep") {
                let Ok(text) = std::fs::read_to_string(p) else {
                    notes.push(format!(
                        "{name} could not be read, so its extension-role metadata was not used."
                    ));
                    continue;
                };
                let parsed = layers::parse_extrep(&text);
                for extension in parsed.contested {
                    meta.extrep_roles.remove(&extension);
                    extrep_contested.insert(extension);
                }
                for (extension, role) in parsed.roles {
                    if extrep_contested.contains(&extension) {
                        continue;
                    }
                    match meta.extrep_roles.get(&extension) {
                        Some(existing) if *existing != role => {
                            meta.extrep_roles.remove(&extension);
                            extrep_contested.insert(extension);
                        }
                        _ => {
                            meta.extrep_roles.insert(extension, role);
                        }
                    }
                }
            }
        }
        for name in meta.ldp_drills.keys() {
            let claimants: Vec<String> = files
                .iter()
                .filter(|p| film(p).eq_ignore_ascii_case(name))
                .map(|p| {
                    p.strip_prefix(dir)
                        .unwrap_or(p)
                        .to_string_lossy()
                        .into_owned()
                })
                .collect();
            match claimants.len() {
                0 => notes.push(format!(
                    ".LDP names drill file {name}, but no package member has that basename; the declaration was not applied."
                )),
                1 => {}
                _ => {
                    return Err(ExtractError::Gerber(format!(
                        ".LDP names drill file {name}, but more than one package member has that basename ({}); preserve unique names or paths before using the declared span",
                        claimants.join(", ")
                    )))
                }
            }
        }
        for extension in &extrep_contested {
            notes.push(format!(
                ".EXTREP assigns more than one layer role to .{extension}; that extension is ambiguous, so each file falls back to its own declaration and filename instead of an arbitrary report row."
            ));
        }

        // Rank the manifest's copper films: side tags first, then the number,
        // then the file name. `gbrjob` is a HashMap, so a tie on (side, number)
        // would otherwise carry hash order into the provisional stack index and
        // change which layers a blind via stitches between two runs.
        let present: HashSet<&str> = files
            .iter()
            .filter_map(|p| p.file_name()?.to_str())
            .collect();
        let mut job_copper: Vec<(&String, u32, GbrJobSide)> = meta
            .gbrjob
            .iter()
            .filter_map(|(f, r)| match r {
                GbrJobRole::Copper { layer, side } if present.contains(f.as_str()) => {
                    Some((f, *layer, *side))
                }
                _ => None,
            })
            .collect();
        job_copper.sort_by(|a, b| (a.2, a.1, a.0).cmp(&(b.2, b.1, b.0)));
        meta.gbrjob_numbers_physical = job_copper
            .iter()
            .map(|(_, n, _)| *n)
            .eq(1..=job_copper.len() as u32);
        meta.gbrjob_index = job_copper
            .iter()
            .enumerate()
            .map(|(i, (f, _, side))| {
                let idx = if *side == GbrJobSide::Bottom {
                    usize::MAX
                } else {
                    i
                };
                ((*f).clone(), idx)
            })
            .collect();
        Ok(meta)
    }

    /// The role of one file: the explicit mapping, then `.gbrjob`, then `.LDP`
    /// drill identity, then a usable unique `.EXTREP` extension. The file's own
    /// attribute, its name and its body (in that order, see `classify_file`)
    /// are the fallback.
    fn role_of(&self, path: &Path, notes: &mut Vec<String>) -> Result<LayerRole, ExtractError> {
        let fname = film(path);
        if let Some(mapped) = self.mapping.get(&*fname) {
            return Ok(mapped.clone());
        }
        let inferred = || layers::classify_file(path, &read_head(path));
        let extrep = path
            .extension()
            .and_then(|s| s.to_str())
            .and_then(|e| self.extrep_roles.get(&e.to_ascii_lowercase()))
            .map(|role| match role {
                ExtRepRole::Copper { index } => copper_role(*index, path),
                ExtRepRole::Drill => LayerRole::Drill,
                ExtRepRole::Outline => LayerRole::Outline,
                ExtRepRole::Ignored => LayerRole::Ignored,
            });
        let in_ldp = self.ldp_drills.contains_key(&fname.to_ascii_lowercase());
        if let Some(job_role) = self.gbrjob.get(&*fname) {
            let declared = match job_role {
                GbrJobRole::Drill { .. } => LayerRole::Drill,
                _ if in_ldp => {
                    return Err(ExtractError::Gerber(format!(
                        ".gbrjob and .LDP disagree about the role of {fname}; one names it as {} and the other as drilling. Resolve the package metadata before reconstructing connectivity",
                        match job_role {
                            GbrJobRole::Copper { .. } => "copper",
                            GbrJobRole::Outline => "the board outline",
                            _ => "electrically irrelevant artwork",
                        }
                    )));
                }
                GbrJobRole::Copper { layer, .. } => copper_role(
                    self.gbrjob_index
                        .get(&*fname)
                        .copied()
                        .unwrap_or(layer.saturating_sub(1) as usize),
                    path,
                ),
                GbrJobRole::Outline => LayerRole::Outline,
                GbrJobRole::Ignored => LayerRole::Ignored,
            };
            if let Some(extrep) = extrep.filter(|e| !same_layer_role(&declared, e)) {
                notes.push(format!(
                    ".gbrjob declares {fname} as {}, while .EXTREP says {}; the exact-file .gbrjob entry was used instead of the extension-wide report.",
                    layers::role_phrase(&declared),
                    layers::role_phrase(&extrep)
                ));
            }
            return Ok(declared);
        }
        if in_ldp {
            return Ok(LayerRole::Drill);
        }
        match extrep {
            Some(declared) => {
                let inferred = inferred();
                if !same_layer_role(&declared, &inferred) {
                    notes.push(format!(
                        ".EXTREP declares {fname} as {}, while filename inference says {}; exporter metadata was used.",
                        layers::role_phrase(&declared),
                        layers::role_phrase(&inferred)
                    ));
                }
                Ok(declared)
            }
            None => Ok(inferred()),
        }
    }

    /// What the manifests say about a drill file's plating: `.gbrjob` and
    /// `.LDP` must agree where both speak, since silently choosing one can turn
    /// a mechanical hole into a conductor or erase a real barrel.
    fn manifest_plated(&self, fname: &str) -> Result<Option<bool>, ExtractError> {
        let from_job = match self.gbrjob.get(fname) {
            Some(GbrJobRole::Drill { plated }) => Some(*plated),
            _ => None,
        };
        let from_ldp = self
            .ldp_drills
            .get(&fname.to_ascii_lowercase())
            .and_then(|role| role.plated);
        if matches!((from_job, from_ldp), (Some(a), Some(b)) if a != b) {
            return Err(ExtractError::Gerber(format!(
                ".gbrjob and .LDP disagree about whether {fname} is plated; resolve the package metadata before reconstructing connectivity"
            )));
        }
        Ok(from_job.or(from_ldp))
    }
}

/// Every file of a job sorted into what the reconstruction reads it as.
#[derive(Default)]
struct Classified {
    copper: Vec<(LayerRole, PathBuf)>,
    drills: Vec<PathBuf>,
    outlines: Vec<PathBuf>,
    csvs: Vec<PathBuf>,
    /// Allegro `smt_loc.txt` and similar component-location files.
    loc_files: Vec<PathBuf>,
    /// Every file and how it was read, for a refusal that can say what it saw.
    inventory: Vec<(String, LayerRole)>,
}

fn classify_files(
    files: Vec<PathBuf>,
    meta: &PackageMetadata,
    notes: &mut Vec<String>,
) -> Result<Classified, ExtractError> {
    let mut out = Classified::default();
    for path in files {
        let role = meta.role_of(&path, notes)?;
        out.inventory.push((film(&path).into_owned(), role.clone()));
        match role {
            r @ LayerRole::Copper { .. } => out.copper.push((r, path)),
            LayerRole::Drill => out.drills.push(path),
            LayerRole::Outline => out.outlines.push(path),
            _ => {
                let lname = film(&path).to_ascii_lowercase();
                if ext_is(&path, "csv") || ext_is(&path, "pos") {
                    out.csvs.push(path);
                } else if ["loc", "place", "pos", "pnp", "xy"]
                    .iter()
                    .any(|w| lname.contains(w))
                {
                    out.loc_files.push(path);
                }
            }
        }
    }
    Ok(out)
}

/// Placements and BOM enrichment from a job's assembly files. Any CSV that
/// yields placements is a P&P file; a job with separate top and bottom
/// placement CSVs contributes both, so placements EXTEND. A CSV that yields
/// none (no X/Y columns) is tried as a BOM. Allegro-style location files
/// (`smt_loc.txt`) are the fallback when no CSV placed anything.
fn read_placements(
    csvs: &[PathBuf],
    loc_files: &[PathBuf],
) -> (
    Vec<placement::Placement>,
    HashMap<String, placement::BomEntry>,
) {
    let mut placements = Vec::new();
    let mut bom = HashMap::new();
    for text in csvs.iter().filter_map(|c| std::fs::read_to_string(c).ok()) {
        let pnp = placement::parse_pnp(&text);
        if pnp.is_empty() {
            bom.extend(placement::parse_bom(&text));
        } else {
            placements.extend(pnp);
        }
    }
    if placements.is_empty() {
        placements = loc_files
            .iter()
            .filter_map(|l| std::fs::read_to_string(l).ok())
            .map(|text| placement::parse_allegro_loc(&text))
            .find(|pnp| !pnp.is_empty())
            .unwrap_or_default();
    }
    (placements, bom)
}

/// Reverse-extract from a directory of gerber/drill/P&P files.
///
/// Detection consults `.gbrjob`, Altium `.LDP`/`.EXTREP`, then the file name
/// (see [`layers::classify`]), recursing into sub-directories. An optional
/// `layer_map.txt` / `*.map` mapping file overrides package metadata and the
/// name-based role guess for exotic jobs. The board name is the directory's
/// file name. The pick-and-place is picked up from a `.csv`/`.pos` that parses
/// as one, or an Allegro `smt_loc.txt`.
pub fn from_gerber_dir(dir: &Path) -> Result<GerberExtraction, ExtractError> {
    let name = dir
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("gerber")
        .to_string();
    from_gerber_dir_named(dir, &name)
}

/// One drill file read and resolved as far as the whole set allows.
struct ParsedDrill {
    /// The file name as shipped, for notes.
    file: String,
    /// The same lower-cased, for the layer-name tokens.
    name: String,
    hits: Vec<Hole>,
    declared: DeclaredSpan,
    claim: SpanClaim,
}

/// [`from_gerber_dir`] with the board name supplied rather than taken from the
/// directory. The zip path needs it: extraction goes to a throwaway directory
/// whose name exists only to be unique, so naming the board after it would put a
/// nanosecond clock reading in every report.
fn from_gerber_dir_named(dir: &Path, board_name: &str) -> Result<GerberExtraction, ExtractError> {
    let mut all_files = Vec::new();
    collect_files(dir, &mut all_files);
    // Reader notes begin with authority decisions made before any geometry is
    // parsed. They are carried into ReconStats at the end so a metadata
    // conflict or fallback is visible in every report surface.
    let mut notes: Vec<String> = Vec::new();
    let meta = PackageMetadata::read(dir, &all_files, &mut notes)?;
    let files = classify_files(all_files, &meta, &mut notes)?;
    if files.copper.is_empty() {
        return Err(no_copper_error(&files.inventory));
    }

    // Resolve copper layer order (top -> bottom).
    let role_only: Vec<(LayerRole, usize)> = files
        .copper
        .iter()
        .enumerate()
        .map(|(i, (r, _))| (r.clone(), i))
        .collect();
    let ordered = layers::assign_inner_indices(role_only);
    let n_copper = ordered.len();

    // Parse each copper layer into primitives, in stack order.
    //
    // A film may also state its own PHYSICAL layer number in an X2 attribute
    // (`%TF.FileFunction,Copper,L4,Bot*%`). That is the only thing in a gerber
    // job that ties a film to a position in the real stackup, and it is what
    // makes a drill file's layer pair placeable. A trusted manifest (copper
    // numbers exactly 1..=n) fills in for films that carry no attribute; the
    // film's own attribute wins where both exist.
    let mut layer_prims: Vec<Vec<rs274x::CopperPrim>> = vec![Vec::new(); n_copper];
    let mut physical_to_stack: HashMap<u32, usize> = HashMap::new();
    let mut physical_layer_sources: HashMap<u32, String> = HashMap::new();
    let mut declared_physical_max: u32 = 0;
    for (role, orig_idx) in &ordered {
        let LayerRole::Copper { index, .. } = role else {
            continue;
        };
        let path = &files.copper[*orig_idx].1;
        let text = read_text(path, "read")?;
        let manifest_layer = match meta.gbrjob.get(&*film(path)) {
            Some(GbrJobRole::Copper { layer, .. }) if meta.gbrjob_numbers_physical => Some(*layer),
            _ => None,
        };
        if let Some(l) = copper_physical_layer(&text).or(manifest_layer) {
            register_physical_layer(
                &mut physical_to_stack,
                &mut physical_layer_sources,
                l,
                *index,
                film(path).to_string(),
            )?;
            declared_physical_max = declared_physical_max.max(l);
        }
        layer_prims[*index] = rs274x::parse_layer(&text)
            .map_err(|e| ExtractError::Xml(format!("parse copper {}: {e}", film(path))))?;
    }

    // Read every drill file into hits and work out what it says about plating
    // and about the copper layers its hits reach. Nothing is stitched yet,
    // because whether SILENCE means "through-hole" depends on the rest of the
    // job.
    //
    // Does the job separate its plated and non-plated drilling into different
    // files? If it does, a sibling that is NOT the non-plated one is the plated
    // set by construction, which is a real signal and not an assumption.
    let job_has_a_named_npth_drill = files.drills.iter().any(|d| name_says_non_plated(&film(d)));
    // Drill files dropped whole because nothing said whether their holes are
    // plated, and hits kept on pad-ring evidence alone.
    let mut refused_plating_files = 0usize;
    let mut inferred_plating_holes = 0usize;
    let mut parsed: Vec<ParsedDrill> = Vec::new();
    for d in &files.drills {
        let text = read_text(d, "read")?;
        let fname = film(d).to_string();
        let n = fname.to_ascii_lowercase();
        let ldp_role = meta.ldp_drills.get(&n);
        let manifest_plated = meta.manifest_plated(&fname)?;
        // Whether the NAME says these hits are plated. Weakest of the sources,
        // consulted only when the file itself says nothing. A manifest
        // `Plated` declaration counts as an explicit statement.
        let name_says_plated =
            n.contains("pth") || n.contains("plated") || manifest_plated == Some(true);
        let head: String = text.chars().take(256).collect();
        let read = if drill_is_gerber_format(&head, d.extension().and_then(|s| s.to_str())) {
            read_drill_film(&text, &fname, &mut notes)
        } else {
            read_excellon(&text, &fname, &mut notes)
        };
        // An X2 attribute in the file body beats the file name; the name is
        // consulted only when the file itself is silent.
        if matches!((read.plated, manifest_plated), (Some(a), Some(b)) if a != b) {
            notes.push(format!(
                "{fname}: the drill file and package metadata disagree about whether its holes are plated. Its hits are refused instead of choosing the reading that would either invent or erase a conductor."
            ));
            refused_plating_files += 1;
            continue;
        }
        // A body that declares itself non-plated contributes no hits, so it
        // must not reach the span analysis at all: a mechanical file's
        // layer-pair name would otherwise mark the job multi-span and force
        // its plated siblings into a refusal. A file that drills nothing is
        // evidence about nothing, for the same reason.
        let non_plated = read.plated == Some(false)
            || manifest_plated == Some(false)
            || (read.plated.is_none() && manifest_plated.is_none() && name_says_non_plated(&fname));
        if non_plated || read.hits.is_empty() {
            continue;
        }
        if read.mixed_functions {
            notes.push(format!(
                "{fname}: this drill film mixes plated and mechanical aperture functions, and \
                 this reader assigns plating per FILE rather than per aperture. Its hits are \
                 recorded but stitch no layers rather than have the mechanical ones read as \
                 conductors. Split the plated and non-plated drilling into separate files to \
                 recover them."
            ));
            refused_plating_files += 1;
            continue;
        }
        // Plating decides whether these hits conduct. It comes from the file's
        // own declaration, its name, or the job's plated/non-plated split. With
        // none of those, the copper is the last witness: a hit that a pad flash
        // covers on two or more layers is a via or a plated pad, because that
        // is the only thing CAD draws a ring around; a hit no ring covers may
        // be a mounting hole and stitches nothing.
        let stated = read.plated == Some(true) || name_says_plated || job_has_a_named_npth_drill;
        let hits = if stated {
            read.hits
        } else {
            let total = read.hits.len();
            let ringed = hits_under_pad_rings(read.hits, &layer_prims);
            if ringed.is_empty() {
                notes.push(format!(
                    "{fname}: nothing in this job says whether the holes in this drill file are \
                     plated, and none of its {total} hit(s) sits under a pad flash on two copper \
                     layers, so nothing here can be read as plated. Its hits stitch no layers, \
                     because a plated hole is a conductor and a mechanical one is not, and \
                     guessing either way is wrong half the time. Add a TF.FileFunction line, name \
                     the file PTH or NPTH, or split the plated and non-plated drilling into two \
                     files."
                ));
                refused_plating_files += 1;
                continue;
            }
            notes.push(format!(
                "{fname}: nothing in this job says whether the holes in this drill file are \
                 plated. {} of its {total} hit(s) sit under a pad flash on two or more copper \
                 layers, which is how CAD draws a via or a plated pad and never a mounting hole, \
                 so those were read as plated; the other {} touch no such ring and stitch \
                 nothing. State it outright (a TF.FileFunction line, PTH/NPTH file names, or \
                 ;TYPE=PLATED sections) to replace the inference.",
                ringed.len(),
                total - ringed.len()
            ));
            inferred_plating_holes += ringed.len();
            ringed
        };
        let ldp_span = ldp_role.map(|role| ldp_declared_span(role, n_copper));
        let (declared, span_conflict) = merge_declared_span(read.declared, ldp_span);
        if let Some(span) = ldp_span {
            notes.push(format!(
                ".LDP declares {fname} as {} with {}; package metadata was consulted before filename span inference.",
                match ldp_role.and_then(|role| role.plated) {
                    Some(false) => "non-plated",
                    Some(true) => "plated",
                    None => "plating unstated",
                },
                declared_span_phrase(span)
            ));
        }
        if span_conflict {
            notes.push(format!(
                "{fname}: the drill file and .LDP declare different or unreadable copper spans, so its hits stitch no layers instead of choosing one authority."
            ));
        }
        parsed.push(ParsedDrill {
            file: fname,
            name: n,
            hits,
            declared,
            // Filled in below, once the whole set has been read.
            claim: SpanClaim::Silent,
        });
    }

    // How many copper layers the finished board has, as the files describe it:
    // the DEEPER of the classified films and the deepest layer any drill or
    // film declaration names. The declarations can exceed the film count (KiCad
    // names an inner film after the user's label, so a six-layer job can
    // classify only its two outer films while its drill still says `1,6`);
    // the drill maximum ALONE would fabricate (a four-layer job whose only
    // drill is a blind `Plated,1,2,PTH` would imply a two-layer board and
    // stitch all four layers).
    let implied_layers = parsed
        .iter()
        .filter_map(|p| match p.declared {
            DeclaredSpan::Pair(pair) => Some(pair.to as usize),
            _ => None,
        })
        .max()
        .unwrap_or(0)
        .max(n_copper)
        .max(declared_physical_max as usize);
    if implied_layers > n_copper {
        notes.push(format!(
            "the drill files describe a {implied_layers}-layer board but only {n_copper} copper \
             layer(s) were classified in this job. Copper this reader did not recognise carries \
             no nets, so the reconstruction is missing whatever routing lives on those layers. \
             Name the missing films with their stack position, or add a layer_map.txt, to \
             recover them."
        ));
    }

    // The layer-name table is built HERE, not before the drills were read,
    // because whether a positional `L<n>` reading is safe depends on whether
    // anything in the job says the board is deeper than the films we found.
    let token_to_stack =
        copper_layer_tokens(&ordered, &files.copper, &physical_to_stack, implied_layers);

    for p in parsed.iter_mut() {
        p.claim = match p.declared {
            DeclaredSpan::Pair(pair) => {
                if let (Some(&f), Some(&t)) = (
                    physical_to_stack.get(&pair.from),
                    physical_to_stack.get(&pair.to),
                ) {
                    // (a) Both ends name a film that told us its physical
                    // layer: the placement is exact, whatever else is missing.
                    SpanClaim::Resolved(f.min(t), f.max(t))
                } else if pair.from == 1 && pair.to as usize == implied_layers {
                    // (b) Top to bottom of the board the files describe: this
                    // hit goes right through, whatever subset of the films we
                    // classified.
                    SpanClaim::Resolved(0, n_copper.saturating_sub(1))
                } else if implied_layers == n_copper {
                    // (c) Nothing says the board has more layers than the films
                    // we found, so the films ARE the stack and the 1-based pair
                    // indexes straight into it.
                    match resolve_pair(pair.from, pair.to, n_copper) {
                        Some((f, t)) => SpanClaim::Resolved(f, t),
                        None => SpanClaim::DeclaredButUnresolvable,
                    }
                } else {
                    // A partial span against a stack we know is incomplete:
                    // indexing into the densified films would place the via
                    // somewhere it does not go. NOT silence either: a file that
                    // named its span is the last one whose hits may be assumed
                    // to reach everything.
                    SpanClaim::DeclaredButUnresolvable
                }
            }
            DeclaredSpan::Unreadable => SpanClaim::DeclaredButUnresolvable,
            DeclaredSpan::Absent => match span_from_filename(&p.name, &token_to_stack, n_copper) {
                NameSpan::Placed(f, t) => SpanClaim::Resolved(f, t),
                NameSpan::NamesLayersButUnplaceable => SpanClaim::DeclaredButUnresolvable,
                NameSpan::NoLayerNames if names_a_partial_span(&p.name) => {
                    SpanClaim::PartialButUnreadable
                }
                NameSpan::NoLayerNames => SpanClaim::Silent,
            },
        };
    }

    // Does this job actually carry a multi-span drill set? Only then is a
    // silent file ambiguous. A job whose every declaration is the full stack is
    // a plain through-hole job, and reading a silent sibling as through-hole
    // there is not a guess, it is the only thing the set can mean. A
    // declaration we could not place means this job has vias that stop
    // somewhere we cannot locate, so no silent sibling is safe either.
    let job_is_multi_span = parsed.iter().any(|p| match p.claim {
        SpanClaim::Resolved(f, t) => (f, t) != (0, n_copper.saturating_sub(1)),
        SpanClaim::PartialButUnreadable | SpanClaim::DeclaredButUnresolvable => true,
        SpanClaim::Silent => false,
    });

    // Turn the hits into plated barrels with their resolved span.
    let mut holes: Vec<PlatedHole> = Vec::new();
    for p in parsed {
        let span = match p.claim {
            SpanClaim::Resolved(f, t) => LayerSpan::Range { from: f, to: t },
            // A declaration we could not resolve refuses unconditionally: the
            // rest of the job cannot vouch for a span this file got wrong.
            SpanClaim::DeclaredButUnresolvable => {
                notes.push(format!(
                    "{}: this file declares a copper layer pair that does not resolve against \
                     the {n_copper} copper layer(s) found in this job. Its plated hits are \
                     recorded but stitch no layers. Reading them as through-holes would merge \
                     nets the stackup keeps apart, and the declaration itself says they are not \
                     through-holes. Check that every copper layer of this job is present and \
                     classified, then re-run.",
                    p.file
                ));
                LayerSpan::Unknown
            }
            SpanClaim::PartialButUnreadable | SpanClaim::Silent if job_is_multi_span => {
                notes.push(format!(
                    "{}: this job's drill set spans several layer pairs, and this file does not \
                     say which pair its hits reach. Its plated hits are recorded but stitch no \
                     layers, so nets that only meet through them are reported separately. \
                     Reading them as through-holes would merge nets the stackup keeps apart. \
                     Supply the X2 TF.FileFunction layer pair, or name the file after its pair \
                     (for example -L1-L2.drl or -F_Cu-In1_Cu.drl), to recover them.",
                    p.file
                ));
                LayerSpan::Unknown
            }
            SpanClaim::PartialButUnreadable | SpanClaim::Silent => LayerSpan::Through,
        };
        holes.extend(p.hits.into_iter().map(|h| PlatedHole {
            x: h.x,
            y: h.y,
            diameter: h.diameter,
            to: h.to,
            span,
        }));
    }

    // The board outline, for the castellation count. Not connectivity: an
    // outline is a cut, and a cut joins nothing.
    let outline_prims = read_outline(&files.outlines);
    let (placements, bom) = read_placements(&files.csvs, &files.loc_files);

    let n_castellations = count_castellations(&holes, &outline_prims);
    let (mut board, mut stats) = connect::reconstruct(board_name, layer_prims, holes, placements);
    stats.n_castellations = n_castellations;
    stats.refused_plating_files = refused_plating_files;
    stats.inferred_plating_holes = inferred_plating_holes;
    if stats.refused_span_holes > 0 {
        notes.push(format!(
            "{} plated hit(s) on this job stitch no layers because their copper layer span is \
             not derivable from the files provided. The net count is therefore an over-estimate: \
             conductors that meet only through those hits are reported as separate nets.",
            stats.refused_span_holes
        ));
    }
    // The connectivity pass writes its own notes (X2 disagreements); merge
    // rather than overwrite, with the file-level notes first.
    notes.append(&mut stats.notes);
    for n in &notes {
        eprintln!("hauksbee: {n}");
    }
    stats.notes = notes;

    // Enrich components from the BOM (value / part number / do-not-populate).
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

    warn_if_nets_are_fragmented(&board);
    Ok(GerberExtraction { board, stats })
}

/// What one drill file told us about the copper layers its hits reach.
enum SpanClaim {
    /// A layer pair we resolved to concrete stack indices (inclusive).
    Resolved(usize, usize),
    /// The file DID declare a layer pair and we could not use it: malformed,
    /// reversed, or naming a layer this job does not carry. Distinct from
    /// silence, and never widened into a through-hole.
    DeclaredButUnresolvable,
    /// The file's name says the hits are blind or buried, so they do NOT reach
    /// the whole stack, but it does not say which layers they do reach. There
    /// is no safe reading of this, only a refusal.
    PartialButUnreadable,
    /// The file said nothing about a span. Safe as a through-hole on a job with
    /// no other span in it; ambiguous on a job that has one.
    Silent,
}

/// The tool diameter and swept path a drill flash describes, in board mm.
///
/// A drill film draws each hit as the shape of the finished CUTOUT, so a slot
/// arrives as an oblong or a rectangle, not as a circle. Both facts about it
/// are recoverable and both matter:
///
/// - the **narrow side is the tool diameter**, exactly, because a slot is
///   machined by a bit of that width, and
/// - the **long axis is the path that bit swept**, so the plated wall is the
///   whole stadium, not a circle at the flash's centre.
///
/// Reducing a slot to one inscribed circle makes the barrel miss copper the real
/// cutout touches: a 3 mm by 1 mm slot would reach 0.5 mm from its centre
/// instead of the 1.5 mm it spans. The narrow direction is found over the
/// flash's own edge directions rather than an axis-aligned box, because a slot
/// drawn at 45 degrees has a square bounding box; for a convex outline the
/// minimum-width orientation always lies along an edge, so testing the edges
/// finds it exactly.
///
/// Returns `(diameter, start, end)`; `start == end` for a round hit.
fn drill_flash_extent(shape: &geo::Shape) -> (f64, (f64, f64), (f64, f64)) {
    let pts: &[(f64, f64)] = match shape {
        geo::Shape::Capsule(c) => {
            // Already a stadium: the aperture radius is the tool, the segment
            // is the path.
            return (c.r * 2.0, (c.ax, c.ay), (c.bx, c.by));
        }
        geo::Shape::Polygon { pts, .. } => pts,
        geo::Shape::MultiPolygon { contours, .. } => {
            contours.first().map(|c| c.as_slice()).unwrap_or(&[])
        }
    };
    let inflate = match shape {
        geo::Shape::Polygon { r, .. } => *r,
        _ => 0.0,
    };
    if pts.len() < 3 {
        let b = shape.bounds();
        let (cx, cy) = ((b[0] + b[2]) / 2.0, (b[1] + b[3]) / 2.0);
        let narrow = (b[2] - b[0]).min(b[3] - b[1]);
        let dia = if narrow > 0.0 { narrow } else { 0.1 };
        return (dia, (cx, cy), (cx, cy));
    }
    // Minimum-width orientation over the outline's own edges.
    let mut best: Option<(f64, f64, f64, (f64, f64), (f64, f64))> = None; // (w, umin, umax, dir, perp)
    for i in 0..pts.len() {
        let (ax, ay) = pts[i];
        let (bx, by) = pts[(i + 1) % pts.len()];
        let (dx, dy) = (bx - ax, by - ay);
        let len = dx.hypot(dy);
        if len <= f64::EPSILON {
            continue;
        }
        let dir = (dx / len, dy / len);
        let perp = (-dir.1, dir.0);
        let (mut umin, mut umax) = (f64::INFINITY, f64::NEG_INFINITY);
        let (mut vmin, mut vmax) = (f64::INFINITY, f64::NEG_INFINITY);
        for &(px, py) in pts {
            let u = px * dir.0 + py * dir.1;
            let v = px * perp.0 + py * perp.1;
            umin = umin.min(u);
            umax = umax.max(u);
            vmin = vmin.min(v);
            vmax = vmax.max(v);
        }
        let w = vmax - vmin;
        if !w.is_finite() {
            continue;
        }
        if best.map(|(bw, ..)| w < bw).unwrap_or(true) {
            let mid_v = (vmin + vmax) / 2.0;
            best = Some((w, umin, umax, dir, (perp.0 * mid_v, perp.1 * mid_v)));
        }
    }
    let Some((w, umin, umax, dir, perp_off)) = best else {
        let b = shape.bounds();
        let (cx, cy) = ((b[0] + b[2]) / 2.0, (b[1] + b[3]) / 2.0);
        return (0.1, (cx, cy), (cx, cy));
    };
    let dia = w + 2.0 * inflate;
    let length = (umax - umin) + 2.0 * inflate;
    // The stadium's two centres sit half a tool diameter in from each end.
    let half_path = ((length - dia) / 2.0).max(0.0);
    let mid_u = (umin + umax) / 2.0;
    let cx = dir.0 * mid_u + perp_off.0;
    let cy = dir.1 * mid_u + perp_off.1;
    let dia = if dia > 0.0 { dia } else { 0.1 };
    (
        dia,
        (cx - dir.0 * half_path, cy - dir.1 * half_path),
        (cx + dir.0 * half_path, cy + dir.1 * half_path),
    )
}

/// What a gerber drill film's aperture attributes say about plating.
///
/// `%TA.AperFunction,MechanicalDrill` is the film stating outright that a hit
/// drills no copper; `ViaDrill`, `ComponentDrill` and `CastellatedDrill` all
/// state the opposite. This is an explicit source and beats any inference from
/// the file's name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FilmDrillFunctions {
    /// Every drill aperture declares a plated function.
    AllPlated,
    /// Every drill aperture declares a mechanical one.
    AllMechanical,
    /// Both kinds appear. Which flash is which needs a per-aperture read this
    /// reader does not do, so the film as a whole settles nothing.
    Mixed,
    /// No drill aperture function on the film.
    Unstated,
}

fn film_drill_functions(text: &str) -> FilmDrillFunctions {
    let mut plated = false;
    let mut mechanical = false;
    for line in text.lines() {
        let t = line.trim_start();
        if !t.starts_with('%') && !t.starts_with("G04") {
            continue;
        }
        let up = t.to_ascii_uppercase();
        let Some(at) = up.find("APERFUNCTION") else {
            continue;
        };
        let rest = up[at + "APERFUNCTION".len()..].trim_start_matches([',', ' ']);
        if rest.starts_with("MECHANICALDRILL") {
            mechanical = true;
        } else if rest.starts_with("VIADRILL")
            || rest.starts_with("COMPONENTDRILL")
            || rest.starts_with("CASTELLATEDDRILL")
            || rest.starts_with("BACKDRILL")
        {
            plated = true;
        }
    }
    match (plated, mechanical) {
        (true, false) => FilmDrillFunctions::AllPlated,
        (false, true) => FilmDrillFunctions::AllMechanical,
        (true, true) => FilmDrillFunctions::Mixed,
        (false, false) => FilmDrillFunctions::Unstated,
    }
}

/// The `TF.FileFunction` attribute of a gerber film, uppercased, if it has one.
fn film_file_function(text: &str) -> Option<String> {
    layers::file_function(text).map(|(attribute, _)| attribute)
}

/// The copper layer pair a gerber-format drill film declares, read from the
/// same `TF.FileFunction` attribute an Excellon file carries it in.
fn film_declared_span(text: &str) -> DeclaredSpan {
    match film_file_function(text) {
        Some(f) => excellon::parse_file_function_span(&f),
        None => DeclaredSpan::Absent,
    }
}

/// Whether a gerber-format drill film declares its own holes non-plated.
///
/// The plated/unplated split decides whether a hole is a conductor at all, so
/// it has to be read wherever the file states it, not only from the file name.
/// A film saying `NonPlated` while being called `board-drill.gbr` is exactly
/// the case a name-only check gets wrong, in the direction that invents a net.
fn film_is_non_plated(text: &str) -> bool {
    let Some(f) = film_file_function(text) else {
        return false;
    };
    let Some(at) = f.find("FILEFUNCTION").map(|i| i + "FILEFUNCTION".len()) else {
        return false;
    };
    let rest = f[at..].trim_start_matches([',', ' ']);
    rest.starts_with("NONPLATED") || rest.split(',').any(|x| x.trim().starts_with("NPTH"))
}

/// The physical layer number a copper film states for itself in its X2
/// `%TF.FileFunction,Copper,L<n>,<side>*%` attribute, if it carries one.
///
/// This is the film saying where it sits in the real stackup, which is exactly
/// what the filename cannot be trusted to say: KiCad writes an inner layer's
/// film under the user's own label, so `-GND_Cu.gbr` gives no clue that it is
/// L3 while its `Copper,L3,Inr` attribute says so outright.
fn copper_physical_layer(text: &str) -> Option<u32> {
    let head = film_file_function(text)?;
    let at = head.find("FILEFUNCTION")? + "FILEFUNCTION".len();
    let rest = head[at..].trim_start_matches([',', ' ']);
    let mut fields = rest.split(',').map(|f| f.trim());
    if fields.next()? != "COPPER" {
        return None;
    }
    let layer = fields.next()?.strip_prefix('L')?;
    if layer.is_empty() || !layer.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let side = fields.next()?.trim_end_matches(['*', '%']).trim();
    if !matches!(side, "TOP" | "INR" | "BOT" | "BOTTOM") {
        return None;
    }
    let digits = layer;
    let n: u32 = digits.parse().ok()?;
    (n >= 1).then_some(n)
}

/// Register one physical copper-layer claim without allowing either X2 or a
/// trusted job manifest to overwrite a different film's claim by iteration
/// order. That ambiguity controls blind/buried-via attachment, so it must be a
/// refusal whichever of the two authorities supplied the duplicate.
fn register_physical_layer(
    physical_to_stack: &mut HashMap<u32, usize>,
    physical_layer_sources: &mut HashMap<u32, String>,
    layer: u32,
    stack_index: usize,
    source: String,
) -> Result<(), ExtractError> {
    if let Some(previous) = physical_layer_sources.get(&layer) {
        return Err(ExtractError::Gerber(format!(
            "two copper films declare physical layer L{layer}: {previous} and {source}. \
             Fix the films' TF.FileFunction or .gbrjob attributes, or supply a layer_map.txt; \
             choosing either one would make blind-via connectivity depend on file order"
        )));
    }
    physical_layer_sources.insert(layer, source);
    physical_to_stack.insert(layer, stack_index);
    Ok(())
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
    copper: &[(LayerRole, PathBuf)],
    physical_to_stack: &HashMap<u32, usize>,
    implied_layers: usize,
) -> HashMap<String, usize> {
    let mut out: HashMap<String, usize> = HashMap::new();
    let n = ordered.len();
    // `L<n>` is PHYSICAL numbering, so a film that declared its own position
    // owns that token outright.
    for (physical, stack) in physical_to_stack {
        out.insert(format!("l{physical}"), *stack);
    }
    // Where no film declared anything, the stack position is the only reading
    // of `L<n>` available. It is exact when the films ARE the whole stack and
    // wrong the moment one is missing, so it is only offered when NOTHING in
    // the job, film attribute or drill declaration alike, says the board is
    // deeper than the films we found. Otherwise a drill named `-L1-L2.drl` on a
    // gapped job would place layer 2 on whatever film happened to land at index
    // 1, which could be the bottom of the board.
    if implied_layers <= n {
        for (role, _) in ordered {
            if let LayerRole::Copper { index, .. } = role {
                out.entry(format!("l{}", index + 1)).or_insert(*index);
            }
        }
    }
    // Name-derived tokens. Two rules keep these honest.
    //
    // The scan is by whole token, the same one the drill names go through: a
    // film called `proj_f_cu_rev.g2l` is an inner Protel layer whose project
    // name merely contains the letters, and a substring test would put a drill's
    // F-to-In1 span on the wrong pair. And a token claimed by two different
    // films names neither: it is dropped rather than won by whichever came
    // last, because nothing here can tell which film was meant.
    let mut claims: HashMap<String, Vec<usize>> = HashMap::new();
    for (role, orig_idx) in ordered {
        let LayerRole::Copper { index, .. } = role else {
            continue;
        };
        let Some((_, path)) = copper.get(*orig_idx) else {
            continue;
        };
        let fname = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        for tok in layer_names_in(&fname) {
            let e = claims.entry(tok).or_default();
            if !e.contains(index) {
                e.push(*index);
            }
        }
    }
    let mut contested: HashSet<String> = HashSet::new();
    for (tok, films) in claims {
        // A physical declaration already settled this token; it outranks a name.
        match films.as_slice() {
            [only] => {
                out.entry(tok).or_insert(*only);
            }
            _ => {
                contested.insert(tok);
            }
        }
    }
    // A job whose copper films are not KiCad-named still resolves `f_cu`/`b_cu`
    // to the ends of the stack it does have. Not for a contested token though:
    // there the trouble is that two films answer to the name, and quietly
    // handing it to the top or bottom of the stack is the same wrong answer by
    // a different route.
    if n > 0 {
        if !contested.contains("f_cu") {
            out.entry("f_cu".to_string()).or_insert(0);
        }
        if !contested.contains("b_cu") {
            out.entry("b_cu".to_string()).or_insert(n - 1);
        }
    }
    out
}

/// What a drill file's name said about the layers its hits reach.
#[derive(Debug, PartialEq, Eq)]
enum NameSpan {
    /// Exactly two layer names, both placed in this job's stack (inclusive).
    Placed(usize, usize),
    /// The name is built out of layer names, but they do not resolve to two
    /// films this job carries. `-F_Cu-In1_Cu.drl` on a job with no In1 film
    /// says the hits are blind between two layers, one of which is missing:
    /// unusable, and certainly not a licence to stitch what IS present.
    NamesLayersButUnplaceable,
    /// No layer names in the file name at all.
    NoLayerNames,
}

/// Recover a layer pair from a drill file's name: `-L1-L2.drl`,
/// `-F_Cu-In1_Cu.drl`, `_l2_l3.txt`.
///
/// The name is read in two steps, and keeping them apart is the point. First,
/// which layer NAMES does the file carry, purely lexically. Second, do those
/// name films this job actually has. A name that clears the first step and
/// fails the second is evidence that these hits are NOT through-holes, so it
/// must not fall back to being read as one.
fn span_from_filename(fname: &str, tokens: &HashMap<String, usize>, n_copper: usize) -> NameSpan {
    if n_copper == 0 {
        return NameSpan::NoLayerNames;
    }
    let names = layer_names_in(fname);
    // One layer word on its own is as likely to be part of the board's name as
    // a span; a span needs two ends.
    if names.len() < 2 {
        return NameSpan::NoLayerNames;
    }
    let mut placed: Vec<usize> = Vec::new();
    let mut all_placed = true;
    for n in &names {
        match tokens.get(n) {
            Some(&i) => placed.push(i),
            None => all_placed = false,
        }
    }
    placed.sort_unstable();
    placed.dedup();
    match placed.as_slice() {
        [a, b] if all_placed => NameSpan::Placed(*a, *b),
        _ => NameSpan::NamesLayersButUnplaceable,
    }
}

/// Every copper layer name appearing as a whole token in `fname`, normalised to
/// the `f_cu` / `b_cu` / `in<N>_cu` / `l<N>` forms the token table is keyed by.
///
/// This is lexical on purpose: it has to notice `l2` in `-L1-L2.drl` even on a
/// job that carries no layer 2, because that absence is exactly what makes the
/// name unusable. Resolving first and counting after would read the file as
/// naming one layer and quietly move on.
fn layer_names_in(fname: &str) -> Vec<String> {
    let b = fname.as_bytes();
    let alnum = |i: usize| i < b.len() && b[i].is_ascii_alphanumeric();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < b.len() {
        if i > 0 && alnum(i - 1) {
            i += 1;
            continue;
        }
        let mut len: Option<usize> = None;
        // `in<digits>_cu`, longest form first so it is not read as a bare `in`.
        if b[i] == b'i' && i + 2 < b.len() && b[i + 1] == b'n' && b[i + 2].is_ascii_digit() {
            let mut j = i + 2;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            let tail = &fname[j..];
            if (tail.starts_with("_cu") || tail.starts_with(".cu")) && !alnum(j + 3) {
                len = Some(j + 3 - i);
            }
        }
        // `f_cu` / `b_cu`, and the dotted spellings.
        if len.is_none() && (b[i] == b'f' || b[i] == b'b') {
            let s = &fname[i..];
            let hit = s.starts_with("f_cu")
                || s.starts_with("b_cu")
                || s.starts_with("f.cu")
                || s.starts_with("b.cu");
            if hit && !alnum(i + 4) {
                len = Some(4);
            }
        }
        // `l<digits>`.
        if len.is_none() && b[i] == b'l' && i + 1 < b.len() && b[i + 1].is_ascii_digit() {
            let mut j = i + 1;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            if !alnum(j) {
                len = Some(j - i);
            }
        }
        match len {
            Some(l) => {
                out.push(fname[i..i + l].replace('.', "_"));
                i += l;
            }
            None => i += 1,
        }
    }
    out
}

/// The file name says these hits are blind or buried, i.e. definitely not a
/// through-hole, without saying which layers they reach.
fn names_a_partial_span(fname: &str) -> bool {
    fname.contains("blind") || fname.contains("buried")
}

/// Parse the board outline films into primitives. Purely for the castellation
/// count: an outline is a cut line, never a conductor, so these primitives are
/// deliberately kept out of the connectivity graph.
fn read_outline(outlines: &[PathBuf]) -> Vec<geo::Shape> {
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
/// A castellation is a half-hole on the board edge. Its copper ring is sliced by
/// the outline, so a reader that decides pad ownership by testing whether the
/// hole sits wholly inside a closed pad ring finds no owner and drops the
/// connection. Here the barrel is copper and joins whatever copper it touches,
/// which is what a castellation physically is; the count makes a job with
/// castellations visible in the reconstruction stats.
fn count_castellations(holes: &[PlatedHole], outline: &[geo::Shape]) -> usize {
    use rstar::{RTree, RTreeObject, AABB};
    if outline.is_empty() {
        return 0;
    }
    // A board outline is a polyline of hundreds of short segments and a job can
    // carry thousands of hits, so pairing them off directly is quadratic. Index
    // the outline the way the rest of the module indexes copper, so each hit
    // only pays for the segments its own barrel could possibly reach.
    struct Seg {
        bounds: [f64; 4],
        idx: usize,
    }
    impl RTreeObject for Seg {
        type Envelope = AABB<[f64; 2]>;
        fn envelope(&self) -> Self::Envelope {
            AABB::from_corners(
                [self.bounds[0], self.bounds[1]],
                [self.bounds[2], self.bounds[3]],
            )
        }
    }
    let tree = RTree::bulk_load(
        outline
            .iter()
            .enumerate()
            .map(|(idx, s)| Seg {
                bounds: s.bounds(),
                idx,
            })
            .collect(),
    );
    holes
        .iter()
        .filter(|h| {
            // The drill's own radius, matching `PlatedHole::barrel`, so the
            // shape counted here is the shape the connectivity pass used.
            let r = (h.diameter / 2.0).max(0.0);
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
            let b = barrel.bounds();
            tree.locate_in_envelope_intersecting(AABB::from_corners([b[0], b[1]], [b[2], b[3]]))
                .any(|s| geo::shape_gap(&barrel, &outline[s.idx]) <= 0.0)
        })
        .count()
}

/// Say out loud when the reconstructed net count is mostly copper fragments.
///
/// Reverse extraction unions copper that touches. Where it cannot follow the
/// geometry (a pour whose region the parser does not close, an arc approximated
/// too coarsely, a thermal relief), one real net comes out as several and the
/// net COUNT is an over-estimate; measured on real fab jobs the ratio ranges
/// from 1.6 to 21 nets per part. The statement is exact rather than heuristic: a
/// reconstructed net that no component pad sits on cannot be a net anybody
/// routed to, so it is a fragment the reconstruction failed to attach.
fn warn_if_nets_are_fragmented(board: &ExtractedBoard) {
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
/// delegates to `from_gerber_dir_named`.
///
/// The board is named after the ARCHIVE, never after the extraction directory,
/// which is unique per call so two concurrent analyses cannot tread on each
/// other. The archive's stem is a property of the input; the directory's is not.
pub fn from_gerber_zip(zip_path: &Path) -> Result<GerberExtraction, ExtractError> {
    let name = zip_path
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("gerber")
        .to_string();
    from_gerber_zip_named(zip_path, &name)
}

/// [`from_gerber_zip`] with the board name supplied rather than taken from the
/// archive's own path.
///
/// A web upload arrives as bytes and is parked on disk before the reader can see
/// it, and the name it is parked under is a staging detail, not the board's
/// identity. Passing the name keeps the two apart.
pub fn from_gerber_zip_named(
    zip_path: &Path,
    board_name: &str,
) -> Result<GerberExtraction, ExtractError> {
    let bytes = std::fs::read(zip_path)
        .map_err(|e| ExtractError::Xml(format!("read zip {}: {e}", film(zip_path))))?;
    // Uniqueness only, and nothing here reaches the report: pid plus a
    // process-local counter plus the clock, because a bare clock reading can
    // repeat across two calls in the same nanosecond and two jobs unzipping
    // into one directory would mix their films.
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let tmp = std::env::temp_dir().join(format!(
        "hauksbee_gerber_{}_{}_{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&tmp).map_err(|e| ExtractError::Xml(format!("mktemp: {e}")))?;
    // From here every exit removes the directory, including the early return on
    // a corrupt archive, so a service fed malformed zips cannot accumulate
    // half-unpacked jobs under the system temp directory.
    let scratch = TempTree(tmp);
    unzip_into(&bytes, &scratch.0)?;
    // The zip may wrap a single sub-directory; descend if so.
    let root = single_subdir(&scratch.0).unwrap_or_else(|| scratch.0.clone());
    from_gerber_dir_named(&root, board_name)
}

/// A directory removed when it goes out of scope, however the scope is left.
struct TempTree(PathBuf);

impl Drop for TempTree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Archive members that are the packer's, not the job's: macOS resource forks
/// (`__MACOSX/`, `._name`), Finder and Explorer metadata. A `._board-F_Cu.gbr`
/// fork read as a film classifies as top copper by name and parses as an
/// empty layer, adding a phantom copper layer to the stack.
fn is_archive_noise(name: &Path) -> bool {
    name.components()
        .any(|c| c.as_os_str().eq_ignore_ascii_case("__MACOSX"))
        || name
            .file_name()
            .and_then(|f| f.to_str())
            .is_some_and(layers::is_filesystem_noise)
}

/// If `dir` contains exactly one entry and it's a directory, return it.
fn single_subdir(dir: &Path) -> Option<PathBuf> {
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
        if is_archive_noise(&name) {
            continue;
        }
        // The archive's own layout is kept: two films with one basename in
        // different folders must not overwrite each other, and the directory
        // reader recurses anyway.
        let dest = out.join(&name);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| ExtractError::Xml(format!("zip mkdir {}: {e}", film(parent))))?;
        }
        let mut buf = Vec::with_capacity(file.size() as usize);
        file.read_to_end(&mut buf)
            .map_err(|e| ExtractError::Xml(format!("zip read: {e}")))?;
        std::fs::write(&dest, buf)
            .map_err(|e| ExtractError::Xml(format!("zip write {}: {e}", film(&dest))))?;
    }
    Ok(())
}

impl ExtractedBoard {
    /// Reverse-extract from a gerber job directory or a gerber `.zip`, keeping
    /// the reconstruction accounting.
    ///
    /// Prefer this over [`from_gerber`](Self::from_gerber) anywhere the result
    /// reaches a user: the stats carry the reader's refusal notes and the
    /// pad-location accounting, and dropping them makes a partial
    /// reconstruction indistinguishable from a complete one.
    pub fn from_gerber_with_stats(path: &Path) -> Result<GerberExtraction, ExtractError> {
        if path.is_dir() {
            from_gerber_dir(path)
        } else if is_zip_path(path) {
            from_gerber_zip(path)
        } else {
            Err(not_a_gerber_job())
        }
    }

    /// [`from_gerber_with_stats`](Self::from_gerber_with_stats) with the board
    /// name supplied rather than taken from the path.
    ///
    /// For a caller that has parked bytes in a temp file: the staging name is
    /// the filesystem's business, the board name is the user's.
    pub fn from_gerber_with_stats_named(
        path: &Path,
        board_name: &str,
    ) -> Result<GerberExtraction, ExtractError> {
        if path.is_dir() {
            from_gerber_dir_named(path, board_name)
        } else if is_zip_path(path) {
            from_gerber_zip_named(path, board_name)
        } else {
            Err(not_a_gerber_job())
        }
    }

    /// Reverse-extract from a gerber job directory or a gerber `.zip`. The
    /// universal "hand us only the fab files" entry point.
    pub fn from_gerber(path: &Path) -> Result<Self, ExtractError> {
        Self::from_gerber_with_stats(path).map(|g| g.board)
    }
}

/// Whether `path` names a `.zip`, case-insensitively.
fn is_zip_path(path: &Path) -> bool {
    ext_is(path, "zip")
}

fn not_a_gerber_job() -> ExtractError {
    ExtractError::Gerber(
        "not a gerber job: expected a directory of gerber files, or a .zip of one".to_string(),
    )
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
mod span_tests {
    use super::*;

    /// The token table for a job whose two films declare that they are layers
    /// 1 and 4 of a four-layer board, so the inner two are missing.
    fn gapped_l1_l4() -> HashMap<String, usize> {
        let copper: Vec<(LayerRole, PathBuf)> = vec![
            (
                LayerRole::Copper {
                    index: 0,
                    name: "F".into(),
                },
                PathBuf::from("brd-F_Cu.gbr"),
            ),
            (
                LayerRole::Copper {
                    index: 1,
                    name: "B".into(),
                },
                PathBuf::from("brd-B_Cu.gbr"),
            ),
        ];
        let ordered: Vec<(LayerRole, usize)> = copper
            .iter()
            .enumerate()
            .map(|(i, (r, _))| (r.clone(), i))
            .collect();
        let physical: HashMap<u32, usize> = [(1u32, 0usize), (4u32, 1usize)].into_iter().collect();
        copper_layer_tokens(&ordered, &copper, &physical, 4)
    }

    /// The token table a four-layer KiCad job produces.
    fn kicad4() -> HashMap<String, usize> {
        let copper: Vec<(LayerRole, PathBuf)> = vec![
            (
                LayerRole::Copper {
                    index: 0,
                    name: "F".into(),
                },
                PathBuf::from("brd-F_Cu.gbr"),
            ),
            (
                LayerRole::Copper {
                    index: 1,
                    name: "In1".into(),
                },
                PathBuf::from("brd-In1_Cu.gbr"),
            ),
            (
                LayerRole::Copper {
                    index: 2,
                    name: "In2".into(),
                },
                PathBuf::from("brd-In2_Cu.gbr"),
            ),
            (
                LayerRole::Copper {
                    index: 3,
                    name: "B".into(),
                },
                PathBuf::from("brd-B_Cu.gbr"),
            ),
        ];
        let ordered: Vec<(LayerRole, usize)> = copper
            .iter()
            .enumerate()
            .map(|(i, (r, _))| (r.clone(), i))
            .collect();
        // No film declares its own layer number, so the stack positions are all
        // there is, which is exact on a complete four-film job.
        copper_layer_tokens(&ordered, &copper, &HashMap::new(), 0)
    }

    #[test]
    fn x2_pairs_resolve_to_stack_indices_and_out_of_range_ones_do_not() {
        // `1,4` on a four-layer board is the whole stack; `1,2` is a blind via.
        assert_eq!(resolve_pair(1, 4, 4), Some((0, 3)));
        assert_eq!(resolve_pair(1, 2, 4), Some((0, 1)));
        assert_eq!(resolve_pair(2, 3, 4), Some((1, 2)));
        // A pair naming a layer the job does not carry must NOT be clamped into
        // the stack: clamping turns `1,6` into a through-hole, the exact
        // fabrication the span logic exists to prevent.
        assert_eq!(resolve_pair(1, 6, 4), None);
        assert_eq!(resolve_pair(0, 2, 4), None);
        assert_eq!(resolve_pair(3, 3, 4), None);
        assert_eq!(resolve_pair(2, 1, 4), None);
    }

    #[test]
    fn kicad_layer_named_drill_files_resolve_their_pair() {
        let t = kicad4();
        assert_eq!(t.get("f_cu"), Some(&0));
        assert_eq!(t.get("in1_cu"), Some(&1));
        assert_eq!(t.get("in2_cu"), Some(&2));
        assert_eq!(t.get("b_cu"), Some(&3));
        assert_eq!(
            span_from_filename("brd-f_cu-in1_cu.drl", &t, 4),
            NameSpan::Placed(0, 1)
        );
        assert_eq!(
            span_from_filename("brd-in1_cu-in2_cu.drl", &t, 4),
            NameSpan::Placed(1, 2)
        );
        assert_eq!(
            span_from_filename("brd-f_cu-b_cu.drl", &t, 4),
            NameSpan::Placed(0, 3)
        );
        assert_eq!(
            span_from_filename("brd-pth-l1-l2.drl", &t, 4),
            NameSpan::Placed(0, 1)
        );
        assert_eq!(
            span_from_filename("brd-pth-l2-l3.drl", &t, 4),
            NameSpan::Placed(1, 2)
        );
    }

    #[test]
    fn a_name_with_no_layer_pair_yields_no_span() {
        let t = kicad4();
        // The ordinary drill names. None of these may be read as a pair, and in
        // particular a project name carrying digits must not become a stackup.
        for n in [
            "brd-pth.drl",
            "brd-npth.drl",
            "esp32-evb_rev_f-pth.drl",
            "rp2040-pico-pc-pth.drl",
            "brd-drill.drl",
            "brd-f_cu.drl",
            "vac-adapter-pth.drl",
            "reform2-motherboard30-pth.drl",
        ] {
            assert_eq!(
                span_from_filename(n, &t, 4),
                NameSpan::NoLayerNames,
                "{n} is not a layer pair"
            );
        }
        // Three layer names is not a pair, and it is not silence either: the
        // name is clearly about layers and no span can be read out of it.
        assert_eq!(
            span_from_filename("brd-f_cu-in1_cu-b_cu.drl", &t, 4),
            NameSpan::NamesLayersButUnplaceable
        );
    }

    #[test]
    fn a_layer_pair_naming_a_film_this_job_lacks_is_unplaceable_not_silent() {
        // Two films that say they are layers 1 and 4 of a four-layer board.
        let t = gapped_l1_l4();
        assert_eq!(t.get("l1"), Some(&0));
        assert_eq!(t.get("l4"), Some(&1));
        // The POSITIONAL reading of `l2` must not be offered here. Stack index
        // 1 is the board's layer 4, and handing it out under the name `l2` is
        // how a blind L1-L2 drill ends up shorting the top of the board to the
        // bottom of it.
        assert_eq!(t.get("l2"), None);

        // Both these names describe a blind via to a layer this job does not
        // carry. Neither may fall back to being read as a through-hole.
        assert_eq!(
            span_from_filename("brd-pth-l1-l2.drl", &t, 2),
            NameSpan::NamesLayersButUnplaceable
        );
        assert_eq!(
            span_from_filename("brd-f_cu-in1_cu.drl", &t, 2),
            NameSpan::NamesLayersButUnplaceable
        );
        // Top to bottom of what we do have is still placeable.
        assert_eq!(
            span_from_filename("brd-f_cu-b_cu.drl", &t, 2),
            NameSpan::Placed(0, 1)
        );
        assert_eq!(
            span_from_filename("brd-l1-l4.drl", &t, 2),
            NameSpan::Placed(0, 1)
        );
    }

    #[test]
    fn layer_names_are_found_lexically_even_when_the_job_lacks_the_layer() {
        // The scan has to see `l2` in a name whose job has no layer 2, because
        // that absence is precisely what makes the name unusable. Resolving
        // first and counting afterwards would read this as naming one layer and
        // quietly move on to the through-hole default.
        assert_eq!(layer_names_in("brd-pth-l1-l2.drl"), vec!["l1", "l2"]);
        assert_eq!(
            layer_names_in("brd-f_cu-in1_cu.drl"),
            vec!["f_cu", "in1_cu"]
        );
        assert_eq!(layer_names_in("brd-f.cu-b.cu.drl"), vec!["f_cu", "b_cu"]);
        // Words that merely contain a layer letter are not layer names.
        assert!(layer_names_in("esp32-evb_rev_f-pth.drl").is_empty());
        assert!(layer_names_in("reform2-motherboard30-pth.drl").is_empty());
        assert!(layer_names_in("vac-adapter-pth.drl").is_empty());
        assert!(layer_names_in("brd-slotholes.txt").is_empty());
    }

    #[test]
    fn a_layer_token_two_films_both_claim_names_neither() {
        // A Protel inner layer whose project name happens to contain "f_cu".
        // A substring test handed it the `f_cu` token, overwriting the real top
        // film, and a drill named `-F_Cu-In1_Cu.drl` then placed its span on
        // the wrong pair. Two claims mean the token identifies nothing, and
        // there is no way from here to tell which film was meant.
        let copper: Vec<(LayerRole, PathBuf)> = vec![
            (
                LayerRole::Copper {
                    index: 0,
                    name: "F".into(),
                },
                PathBuf::from("board-F_Cu.gbr"),
            ),
            (
                LayerRole::Copper {
                    index: 1,
                    name: "In1".into(),
                },
                PathBuf::from("board-In1_Cu.gbr"),
            ),
            (
                LayerRole::Copper {
                    index: 2,
                    name: "In2".into(),
                },
                PathBuf::from("proj_f_cu_rev.g2l"),
            ),
        ];
        let ordered: Vec<(LayerRole, usize)> = copper
            .iter()
            .enumerate()
            .map(|(i, (r, _))| (r.clone(), i))
            .collect();
        let t = copper_layer_tokens(&ordered, &copper, &HashMap::new(), 3);
        assert_eq!(t.get("f_cu"), None, "an ambiguous token names nothing");
        assert_eq!(
            t.get("in1_cu"),
            Some(&1),
            "an unambiguous one still resolves"
        );
        // So a drill named after the ambiguous token cannot be placed, and is
        // refused rather than put on whichever film happened to win.
        assert_eq!(
            span_from_filename("board-f_cu-in1_cu.drl", &t, 3),
            NameSpan::NamesLayersButUnplaceable
        );
    }

    #[test]
    fn blind_and_buried_names_without_a_pair_are_flagged_unreadable() {
        assert!(names_a_partial_span("brd-blind.drl"));
        assert!(names_a_partial_span("brd-buriedvias.drl"));
        assert!(!names_a_partial_span("brd-pth.drl"));
    }

    #[test]
    fn inner_layer_number_reads_only_a_whole_in_n_cu_token() {
        let inner_layer_number = |f: &str| {
            layer_names_in(f).into_iter().find_map(|t| {
                t.strip_prefix("in")
                    .and_then(|r| r.strip_suffix("_cu"))
                    .and_then(|d| d.parse::<u32>().ok())
            })
        };
        assert_eq!(inner_layer_number("brd-in1_cu.gbr"), Some(1));
        assert_eq!(inner_layer_number("brd-in12_cu.gbr"), Some(12));
        assert_eq!(inner_layer_number("brd-in3.cu.gbr"), Some(3));
        // `main1_cu` is a project name, not an inner layer.
        assert_eq!(inner_layer_number("main1_cu.gbr"), None);
        assert_eq!(inner_layer_number("brd-in1_mask.gbr"), None);
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
