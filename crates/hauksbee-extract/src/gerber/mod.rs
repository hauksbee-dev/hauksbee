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
///
/// Sorted by path within each directory, and directories descended in that same
/// sorted order, because `read_dir` yields whatever order the filesystem
/// happens to hold. Downstream this list decides which copper film gets which
/// provisional stack index when a job's names tie, so readdir order would leak
/// into the reconstruction. Two analyses of one archive extracted into two
/// different temp directories must not diverge on the order the kernel handed
/// the entries back.
fn collect_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut here: Vec<std::path::PathBuf> = entries.flatten().map(|e| e.path()).collect();
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

/// Reverse-extract from a directory of gerber/drill/P&P files.
///
/// Detection is by file name (see [`layers::classify`]), recursing into
/// sub-directories. An optional `layer_map.txt` / `*.map` mapping file in the
/// directory overrides the name-based role guess for exotic jobs. The board
/// name is the directory's file name. The pick-and-place is picked up from a
/// `.csv`/`.pos` that parses as one, or an Allegro `smt_loc.txt`.
pub fn from_gerber_dir(dir: &Path) -> Result<GerberExtraction, ExtractError> {
    let name = dir
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("gerber")
        .to_string();
    from_gerber_dir_named(dir, &name)
}

/// [`from_gerber_dir`] with the board name supplied rather than taken from the
/// directory.
///
/// The zip path needs this: it extracts into a throwaway directory whose name
/// exists only to be unique, so naming the board after it put a nanosecond
/// clock reading in every report and broke the "same board twice, same JSON"
/// contract. The archive names the board instead.
fn from_gerber_dir_named(dir: &Path, board_name: &str) -> Result<GerberExtraction, ExtractError> {
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

    // The exporter's own manifest, when the job ships one: a `.gbrjob` names
    // every file's role and each copper film's PHYSICAL layer number
    // (`Copper,L3,Inr`). That answers exactly what filename inference guesses
    // at: which files are copper and in what stack order, including
    // Allegro-style planes named without a stack digit and KiCad inner films
    // exported under the user's own label (`-GND_Cu.gbr`). The explicit
    // mapping file still outranks it; filename inference is the fallback when
    // no job file exists.
    let mut gbrjob: std::collections::HashMap<String, layers::GbrJobRole> =
        std::collections::HashMap::new();
    for p in &all_files {
        let is_job = p
            .extension()
            .and_then(|s| s.to_str())
            .map(|s| s.eq_ignore_ascii_case("gbrjob"))
            .unwrap_or(false);
        if is_job {
            if let Ok(text) = std::fs::read_to_string(p) {
                gbrjob.extend(layers::parse_gbrjob(&text));
            }
        }
    }
    // Rank the manifest's copper films into provisional stack indices. The
    // declared numbers ORDER the stack (side tags first, then the number:
    // both hold even on exporters whose numbers are not physical positions;
    // KiCad 9 writes internal layer IDs, so a four-layer manifest can read
    // L1, L5, L7, L4). The numbers are fed onward as PHYSICAL positions only
    // when they are exactly `1..=n` in that rank order, anything else is a
    // numbering scheme we cannot vouch for, and feeding it to the drill
    // layer-pair resolver would invent layers the board does not have.
    let present: std::collections::HashSet<String> = all_files
        .iter()
        .filter_map(|p| p.file_name().and_then(|s| s.to_str()).map(String::from))
        .collect();
    let mut job_copper: Vec<(String, u32, layers::GbrJobSide)> = gbrjob
        .iter()
        .filter_map(|(f, r)| match r {
            layers::GbrJobRole::Copper { layer, side } if present.contains(f) => {
                Some((f.clone(), *layer, *side))
            }
            _ => None,
        })
        .collect();
    // File name breaks the tie. `gbrjob` is a HashMap, so the vector above
    // arrives in whatever order the map hashed into, and a stable sort on
    // (side, number) alone would carry that order through wherever two films
    // declare the same side and number. The rank IS the provisional stack
    // index, so a tie resolved by hash order would move a film up or down the
    // stack between two runs of the same job and change which layers a blind
    // via stitches.
    job_copper.sort_by(|a, b| (a.2, a.1, &a.0).cmp(&(b.2, b.1, &b.0)));
    let gbrjob_numbers_physical = job_copper
        .iter()
        .map(|(_, n, _)| *n)
        .eq(1..=job_copper.len() as u32);
    let gbrjob_index: std::collections::HashMap<String, usize> = job_copper
        .iter()
        .enumerate()
        .map(|(i, (f, _, side))| {
            let idx = match side {
                layers::GbrJobSide::Bottom => usize::MAX,
                _ => i,
            };
            (f.clone(), idx)
        })
        .collect();

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
        // Mapping override first, then the job file's manifest, else
        // name-based classification.
        let role = mapping.get(&fname).cloned().unwrap_or_else(|| {
            match gbrjob.get(&fname) {
                // The provisional stack index is the film's RANK among the
                // manifest's copper entries (top first, bottom `usize::MAX`,
                // exactly the ordering contract `assign_inner_indices`
                // densifies), never the raw declared number.
                Some(layers::GbrJobRole::Copper { layer, .. }) => LayerRole::Copper {
                    index: gbrjob_index
                        .get(&fname)
                        .copied()
                        .unwrap_or((*layer - 1) as usize),
                    name: path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("")
                        .to_string(),
                },
                Some(layers::GbrJobRole::Drill { .. }) => LayerRole::Drill,
                Some(layers::GbrJobRole::Outline) => LayerRole::Outline,
                Some(layers::GbrJobRole::Ignored) => LayerRole::Ignored,
                None => layers::classify(&path),
            }
        });
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
    //
    // A film may also state its own PHYSICAL layer number in an X2 attribute
    // (`%TF.FileFunction,Copper,L4,Bot*%`). That is the only thing in a gerber
    // job that ties a film to a position in the real stackup, and it is what
    // makes a drill file's layer pair placeable: without it we only know which
    // films we found, not which of the board's layers they are.
    let mut layer_prims: Vec<Vec<rs274x::CopperPrim>> = vec![Vec::new(); ordered.len()];
    let mut physical_to_stack: std::collections::HashMap<u32, usize> =
        std::collections::HashMap::new();
    let mut declared_physical_max: u32 = 0;
    for (role, orig_idx) in &ordered {
        let LayerRole::Copper { index, .. } = role else {
            continue;
        };
        let path = &copper[*orig_idx].1;
        let text = std::fs::read_to_string(path)
            .map_err(|e| ExtractError::Xml(format!("read {}: {e}", path.display())))?;
        if let Some(l) = copper_physical_layer(&text) {
            physical_to_stack.insert(l, *index);
            declared_physical_max = declared_physical_max.max(l);
        }
        // The job manifest also states the film's physical layer, but only a
        // manifest whose copper numbers are exactly 1..=n is believed about
        // PHYSICAL positions (see the trust note where the manifest is read).
        // The film's own X2 attribute wins where both exist (it is the file
        // speaking for itself); the manifest fills in for films that carry no
        // attribute.
        if gbrjob_numbers_physical {
            let base = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if let Some(layers::GbrJobRole::Copper { layer, .. }) = gbrjob.get(base) {
                physical_to_stack.entry(*layer).or_insert(*index);
                declared_physical_max = declared_physical_max.max(*layer);
            }
        }
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
    // Reader notes: everything this job made us refuse or could not see, in the
    // order we found it. Surfaced on `ReconStats` and printed, so a refusal is
    // visible instead of looking like a clean extraction.
    let mut notes: Vec<String> = Vec::new();

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
        name: String,
        body: DrillBody,
        declared: excellon::DeclaredSpan,
        claim: SpanClaim,
    }
    // Does the job separate its plated and non-plated drilling into different
    // files? If it does, a sibling that is NOT the non-plated one is the plated
    // set by construction, which is a real signal and not an assumption.
    let job_has_a_named_npth_drill = drills.iter().any(|d| {
        let n = d
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        n.contains("npth") || n.contains("non-plated") || n.contains("nonplated")
    });
    // Drill files dropped because nothing said whether their holes are plated.
    let mut refused_plating_files = 0usize;
    let mut parsed: Vec<ParsedDrill> = Vec::new();
    for d in &drills {
        let text = std::fs::read_to_string(d)
            .map_err(|e| ExtractError::Xml(format!("read {}: {e}", d.display())))?;
        let n = d
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        // The job manifest's plating declaration for this file, if it made
        // one. `NonPlated` is the manifest saying these holes carry no
        // copper; that must not be washed out by the filename inference below
        // (a sibling named NPTH would otherwise mark the job split and
        // promote this file to plated, fabricating stitches out of a file
        // the manifest plainly declared mechanical).
        let manifest_plated: Option<bool> =
            match gbrjob.get(d.file_name().and_then(|s| s.to_str()).unwrap_or("")) {
                Some(layers::GbrJobRole::Drill { plated }) => Some(*plated),
                _ => None,
            };
        if manifest_plated == Some(false) {
            continue;
        }
        let plated = !(n.contains("npth") || n.contains("non-plated") || n.contains("nonplated"));
        if !plated {
            continue;
        }
        // Whether the NAME says these hits are plated. Weakest of the sources,
        // consulted only when the file itself says nothing. A manifest
        // `Plated` declaration counts as an explicit statement.
        let name_says_plated =
            n.contains("pth") || n.contains("plated") || manifest_plated == Some(true);
        let head: String = text.chars().take(256).collect();
        let is_gerber = drill_is_gerber_format(&head, d.extension().and_then(|s| s.to_str()));
        // An X2 attribute in the file body beats the file name; the name is
        // consulted only when the file itself is silent.
        let (body, declared) = if is_gerber {
            // A gerber-format drill film carries the same `TF.FileFunction`
            // attribute an Excellon file does, so it is read the same way: for
            // whether the holes are plated at all, and for the layer pair they
            // span. Discarding either lets a film that plainly states it drills
            // a mechanical hole, or a blind one, be read as a plated
            // through-hole and stitch the whole stack.
            if film_is_non_plated(&text) {
                continue;
            }
            // Plating decides whether these hits are conductors at all, so it
            // is taken from the strongest source the film offers and, when it
            // offers none, refused rather than assumed. Order: the film's own
            // `TF.FileFunction`; its `%TA.AperFunction` drill functions; the
            // file name; the job splitting plated from non-plated into separate
            // files. A film with none of those is a picture of some holes with
            // nothing saying whether they are plated.
            let functions = film_drill_functions(&text);
            if matches!(functions, FilmDrillFunctions::AllMechanical) {
                // Every drill aperture on the film declares itself mechanical.
                // That is the film saying these holes carry no copper.
                continue;
            }
            let says_plated = film_file_function(&text)
                .map(|f| f.contains("PLATED") || f.contains("PTH"))
                .unwrap_or(false);
            let derivable = says_plated
                || matches!(functions, FilmDrillFunctions::AllPlated)
                || name_says_plated
                || job_has_a_named_npth_drill;
            if !derivable {
                notes.push(format!(
                    "{}: nothing in this job says whether the holes on this drill film are \
                     plated. Its hits are recorded but stitch no layers, because a plated hole \
                     is a conductor and a mechanical one is not, and guessing either way is \
                     wrong half the time. Add a TF.FileFunction, a %TA.AperFunction on the drill \
                     apertures, or the usual PTH/NPTH split across two files.",
                    d.file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("drill film")
                ));
                refused_plating_files += 1;
                continue;
            }
            if matches!(functions, FilmDrillFunctions::Mixed) {
                notes.push(format!(
                    "{}: this drill film mixes plated and mechanical aperture functions, and \
                     this reader assigns plating per FILE rather than per aperture. Its hits are \
                     recorded but stitch no layers rather than have the mechanical ones read as \
                     conductors. Split the plated and non-plated drilling into separate files to \
                     recover them.",
                    d.file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("drill film")
                ));
                refused_plating_files += 1;
                continue;
            }
            let declared = film_declared_span(&text);
            (DrillBody::Film(text), declared)
        } else {
            let drill = excellon::parse(&text);
            // A body that declares itself non-plated contributes no hits, so it
            // must not reach the span analysis at all. Leaving it in let a
            // mechanical file's layer-pair name mark the job multi-span and
            // force its plated siblings into a refusal, losing real stitching
            // on the strength of a file that drills no copper.
            if drill.plated == Some(false) {
                continue;
            }
            // A file that drills nothing is evidence about nothing. Left in, an
            // empty pass declaring a blind pair would mark the job multi-span
            // and push every silent sibling into a refusal, losing real
            // stitching on the strength of a file with no hits in it.
            if drill.holes.is_empty() {
                continue;
            }
            // Same refusal as the film path: plating decides whether these
            // hits conduct, so it comes from the file's own declaration, its
            // name, or the job's plated/non-plated split, and from nowhere
            // else. Almost every drill file states it one of those ways; one
            // that states it in none is a list of coordinates with no way to
            // tell a via from a mounting hole.
            if drill.plated.is_none() && !name_says_plated && !job_has_a_named_npth_drill {
                notes.push(format!(
                    "{}: nothing in this job says whether the holes in this drill file are \
                     plated. Its hits are recorded but stitch no layers, because a plated hole \
                     is a conductor and a mechanical one is not, and guessing either way is \
                     wrong half the time. Add a TF.FileFunction line, name the file PTH or NPTH, \
                     or split the plated and non-plated drilling into two files.",
                    d.file_name().and_then(|s| s.to_str()).unwrap_or("drill")
                ));
                refused_plating_files += 1;
                continue;
            }
            let declared = drill.span;
            (DrillBody::Excellon(drill), declared)
        };
        parsed.push(ParsedDrill {
            path: d.clone(),
            name: n,
            body,
            declared,
            // Filled in below, once the whole set has been read.
            claim: SpanClaim::Silent,
        });
    }

    // How many copper layers the finished board has, as the files describe it.
    //
    // Two sources, and the answer is the DEEPER of them. The copper films we
    // classified are one lower bound. The deepest layer any drill declaration
    // names is the other, and it can exceed the film count: KiCad names an
    // inner layer's film after the user's label ("GND_Cu", "Power_Cu"), so a
    // six-layer job can classify only its two outer films while its drill still
    // says `1,6`.
    //
    // Taking the drill maximum ALONE would be a fabrication engine. A four-layer
    // job whose only drill is a blind `Plated,1,2,PTH` would imply a two-layer
    // board, make that pair look full-depth, and stitch all four layers: a
    // phantom short built out of a correct declaration. The film count is
    // evidence too, and it is the evidence that says this board has more layers
    // than that drill reaches.
    let implied_layers = parsed
        .iter()
        .filter_map(|p| match p.declared {
            excellon::DeclaredSpan::Pair(pair) => Some(pair.to as usize),
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
    // anything in the job says the board is deeper than the films we found, and
    // a drill declaration is one of the things that can say so.
    let token_to_stack = copper_layer_tokens(&ordered, &copper, &physical_to_stack, implied_layers);

    for p in parsed.iter_mut() {
        p.claim = match p.declared {
            excellon::DeclaredSpan::Pair(pair) => {
                // (a) Both ends name a film that told us its physical layer:
                // the placement is exact, whatever else is missing.
                if let (Some(&f), Some(&t)) = (
                    physical_to_stack.get(&pair.from),
                    physical_to_stack.get(&pair.to),
                ) {
                    SpanClaim::Resolved(f.min(t), f.max(t))
                } else if pair.from == 1 && pair.to as usize == implied_layers {
                    // (b) Top to bottom of the board the files describe: this
                    // hit goes right through, which stays true whatever subset
                    // of the films we classified.
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
                    // A partial span against a stack we know is incomplete. The
                    // film at index 1 is only "layer 2" if no layer is missing
                    // above it, and here one is, so indexing into our densified
                    // films would place the via somewhere it does not go. NOT
                    // silence either: a file that named its span is the last one
                    // whose hits may be assumed to reach everything.
                    SpanClaim::DeclaredButUnresolvable
                }
            }
            excellon::DeclaredSpan::Unreadable => SpanClaim::DeclaredButUnresolvable,
            excellon::DeclaredSpan::Absent => {
                match span_from_filename(&p.name, &token_to_stack, n_copper) {
                    NameSpan::Placed(f, t) => SpanClaim::Resolved(f, t),
                    NameSpan::NamesLayersButUnplaceable => SpanClaim::DeclaredButUnresolvable,
                    NameSpan::NoLayerNames if names_a_partial_span(&p.name) => {
                        SpanClaim::PartialButUnreadable
                    }
                    NameSpan::NoLayerNames => SpanClaim::Silent,
                }
            }
        };
    }

    // Does this job actually carry a multi-span drill set? Only then is a
    // silent file ambiguous. A job whose every declaration is the full stack is
    // a plain through-hole job, and reading a silent sibling as through-hole
    // there is not a guess, it is the only thing the set can mean.
    let job_is_multi_span = parsed.iter().any(|p| match p.claim {
        SpanClaim::Resolved(f, t) => (f, t) != (0, n_copper.saturating_sub(1)),
        // A declaration we could not place means this job has vias that stop
        // somewhere we cannot locate, so no silent sibling is safe either.
        SpanClaim::PartialButUnreadable | SpanClaim::DeclaredButUnresolvable => true,
        SpanClaim::Silent => false,
    });

    // Pass two: turn the hits into plated barrels with their resolved span.
    let mut holes: Vec<PlatedHole> = Vec::new();
    for p in parsed {
        let d = &p.path;
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
                    d.file_name().and_then(|s| s.to_str()).unwrap_or("drill")
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
                    d.file_name().and_then(|s| s.to_str()).unwrap_or("drill")
                ));
                LayerSpan::Unknown
            }
            SpanClaim::PartialButUnreadable | SpanClaim::Silent => LayerSpan::Through,
        };

        if let DrillBody::Film(text) = &p.body {
            // Gerber-format drill: each flash is a hole; its disc radius is the
            // drill radius. A drawn path on such a film may be a rout, but on
            // an ordinary drill film the draws are legend art, and reading
            // those as plated walls paints copper across the board.
            //
            // Only the film's OWN attribute settles it. A suggestive file name
            // is not enough: any board whose project name contains "slot" would
            // have its legend promoted to conductor, which is the invention
            // this module exists to avoid. Where the name suggests a rout and
            // the film does not declare one, the draws are left alone and the
            // reader says why, so the gap is visible rather than silent.
            let declares_rout = film_file_function(text)
                .map(|f| f.contains("ROUT") || f.contains("SLOT") || f.contains("MILL"))
                .unwrap_or(false);
            let name_suggests_rout = d
                .file_name()
                .and_then(|s| s.to_str())
                .map(|s| {
                    let l = s.to_ascii_lowercase();
                    l.contains("rout") || l.contains("slot") || l.contains("mill")
                })
                .unwrap_or(false);
            let rout_film = declares_rout;
            if name_suggests_rout && !declares_rout {
                notes.push(format!(
                    "{}: this gerber-format drill film is named as a rout or slot layer but does \
                     not declare itself one, so the paths drawn on it are left as artwork rather \
                     than read as plated walls. A drawn path is only a conductor if the film says \
                     it is; promoting it on the strength of a file name would turn any legend on \
                     a board whose project name contains \"slot\" into copper. Add a \
                     TF.FileFunction naming the layer's role to recover them.",
                    d.file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("drill film")
                ));
            }
            if let Ok(prims) = rs274x::parse_layer(text) {
                for pr in prims {
                    match pr.kind {
                        rs274x::PrimKind::Flash => {
                            // A drill film draws the finished CUTOUT, so an
                            // oblong flash is a slot: its narrow side is the
                            // tool and its long axis is the path that tool
                            // swept. Recovering both gives the whole plated
                            // wall. A round flash comes back with its two
                            // centres coincident, which is a round hole.
                            let (dia, from, to) = drill_flash_extent(&pr.shape);
                            let is_slot = (to.0 - from.0).hypot(to.1 - from.1) > 1e-9;
                            holes.push(PlatedHole {
                                x: from.0,
                                y: from.1,
                                diameter: dia,
                                to: is_slot.then_some(to),
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

    let name = board_name.to_string();

    let n_castellations = count_castellations(&holes, &outline_prims);
    let (mut board, mut stats) = connect::reconstruct(&name, layer_prims, holes, placements);
    stats.n_castellations = n_castellations;
    stats.refused_plating_files = refused_plating_files;
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
/// Reducing a slot to one inscribed circle is what makes a barrel miss copper
/// the real cutout plainly touches: a 3 mm by 1 mm slot would reach 0.5 mm from
/// its centre instead of the 1.5 mm it actually spans.
///
/// The narrow direction is found over the flash's own edge directions rather
/// than an axis-aligned box, because a slot drawn at 45 degrees has a square
/// bounding box and would otherwise read as a round hole of the diagonal's
/// width. For a convex outline the minimum-width orientation always lies along
/// an edge, so testing the edges finds it exactly.
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
        geo::Shape::MultiPolygon { contours } => {
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
///
/// Attribute lines open with `%`, and there are a handful of them among a
/// film's many thousands of drawing commands, so only those are uppercased.
/// The whole file is scanned rather than a fixed prefix: an exporter that
/// writes a page of `G04` banner text first would otherwise push the attribute
/// out of the window, and the reader would fall back to "says nothing" for a
/// film that plainly states it is mechanical.
fn film_file_function(text: &str) -> Option<String> {
    text.lines()
        .filter(|l| l.trim_start().starts_with('%'))
        .map(|l| l.to_ascii_uppercase())
        .find(|l| l.contains("FILEFUNCTION"))
}

/// The copper layer pair a gerber-format drill film declares, read from the
/// same `TF.FileFunction` attribute an Excellon file carries it in.
fn film_declared_span(text: &str) -> excellon::DeclaredSpan {
    match film_file_function(text) {
        Some(f) => excellon::parse_file_function_span(&f),
        None => excellon::DeclaredSpan::Absent,
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
    let digits: String = fields
        .next()?
        .strip_prefix('L')?
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    let n: u32 = digits.parse().ok()?;
    (n >= 1).then_some(n)
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
    physical_to_stack: &std::collections::HashMap<u32, usize>,
    implied_layers: usize,
) -> std::collections::HashMap<String, usize> {
    let mut out: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
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
    // name happens to contain the letters, and a substring test handed it the
    // `f_cu` token, which then placed a drill's F-to-In1 span on the wrong pair.
    //
    // And a token claimed by two different films names neither: it is dropped
    // rather than won by whichever came last. A span built on an ambiguous name
    // is a span put somewhere nobody said, and there is no way to tell from
    // here which film was meant.
    let mut claims: std::collections::HashMap<String, Vec<usize>> =
        std::collections::HashMap::new();
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
    let mut contested: std::collections::HashSet<String> = std::collections::HashSet::new();
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
fn span_from_filename(
    fname: &str,
    tokens: &std::collections::HashMap<String, usize>,
    n_copper: usize,
) -> NameSpan {
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
/// delegates to [`from_gerber_dir_named`].
///
/// The board is named after the ARCHIVE, never after the extraction directory.
/// The extraction directory has to be unique per call (two concurrent analyses
/// of different jobs must not tread on each other), which used to mean a
/// nanosecond clock reading in its name, and [`from_gerber_dir`] took the board
/// name from the directory it was handed: so a single byte-identical upload
/// analysed twice produced two different `board_name` values, and the exported
/// JSON differed run to run. The archive's stem is a property of the input, so
/// it holds across runs.
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
/// A web upload arrives as bytes and has to be parked on disk before the reader
/// can see it, and the name it is parked under is a staging detail, not the
/// board's identity. Passing the name here keeps the two apart: the caller can
/// stage the bytes under whatever the filesystem will accept and still report
/// the board under the name the user uploaded.
pub fn from_gerber_zip_named(
    zip_path: &Path,
    board_name: &str,
) -> Result<GerberExtraction, ExtractError> {
    let bytes = std::fs::read(zip_path)
        .map_err(|e| ExtractError::Xml(format!("read zip {}: {e}", zip_path.display())))?;
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
    // a corrupt archive: a service fed malformed zips used to accumulate
    // half-unpacked jobs under the system temp directory forever.
    let scratch = TempTree(tmp);
    unzip_into(&bytes, &scratch.0)?;
    // The zip may wrap a single sub-directory; descend if so.
    let root = single_subdir(&scratch.0).unwrap_or_else(|| scratch.0.clone());
    from_gerber_dir_named(&root, board_name)
}

/// A directory removed when it goes out of scope, however the scope is left.
struct TempTree(std::path::PathBuf);

impl Drop for TempTree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
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
    /// the filesystem's business, the board name is the user's, and reading the
    /// second off the first put staging details (a process id, a counter) into
    /// every report.
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
    path.extension()
        .and_then(|s| s.to_str())
        .is_some_and(|s| s.eq_ignore_ascii_case("zip"))
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
    use std::collections::HashMap;
    use std::path::PathBuf;

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
