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

    /// `needle` present as a whole word, i.e. not run together with other
    /// letters. Substring matching is wrong for the bare role names: `top`
    /// appears inside `stopmask` and `bot` inside `robot`, and a mask film read
    /// as copper poisons the whole reconstruction. Digits count as separators so
    /// `1 - Top`, `L2-GND` and `top2` all match their role.
    fn has_word(&self, needle: &str) -> bool {
        let hay = self.full.as_bytes();
        let ned = needle.as_bytes();
        let alpha = |b: u8| b.is_ascii_alphabetic();
        for i in 0..hay.len().saturating_sub(ned.len()) + 1 {
            if &hay[i..i + ned.len()] != ned {
                continue;
            }
            let before_ok = i == 0 || !alpha(hay[i - 1]);
            let j = i + ned.len();
            let after_ok = j >= hay.len() || !alpha(hay[j]);
            if before_ok && after_ok {
                return true;
            }
        }
        false
    }
}

/// Extensions that hold a plotted gerber film. A file with one of these that
/// survived every non-copper test is a candidate for the bare role-name rules.
fn is_gerber_film_ext(n: &Name) -> bool {
    matches!(
        n.ext.as_deref(),
        Some("gbr" | "ger" | "gdo" | "pho" | "grb" | "art" | "gerber")
    )
}

/// Classify a single fab file by its path. Inner-copper indices are
/// provisional (see [`assign_inner_indices`]).
pub fn classify(path: &Path) -> LayerRole {
    let n = Name::of(path);

    // ── Drill first: extension is the strongest signal ──────────────────────
    if n.ext_is("drl") || n.ext_is("txt") || n.ext_is("xln") || n.ext_is("nc") || n.ext_is("tap") {
        // `.txt` is sometimes a readme; require a drill-ish hint when ambiguous.
        // Altium exports the drill as `<board>-RoundHoles.TXT` /
        // `-RectHoles.TXT` / `-SlotHoles.TXT` (the Inkplate 6 set), so `holes`
        // is a drill hint too. A plain `README.TXT` has none of these tokens
        // and is still ignored.
        if n.ext_is("txt")
            && !(n.has("drill") || n.has("drl") || n.has("nc") || n.has("pth") || n.has("holes"))
        {
            return LayerRole::Ignored;
        }
        return LayerRole::Drill;
    }
    if n.has("drill") || n.has("-pth") || n.has("-npth") || n.has(".pth") || n.has(".npth") {
        return LayerRole::Drill;
    }
    // Allegro gerber-format drill film, e.g. `drill-1-6.art`: drilling drawn as
    // flashes on a gerber layer rather than as a separate Excellon file. The
    // `drill` substring above already catches the common names; this keeps the
    // `.art` ones routed to the drill role explicitly.
    if n.ext_is("art") && n.has("drill") {
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
        // The `Top.gbr`/`Bottom.gbr` exporters (DipTrace and friends) ship
        // `TopAssy.gbr` and `TopDimension.gbr` in the same folder. Those names
        // carry the same bare role word as the copper film, so the bare-name
        // copper rules below would read an assembly drawing as a copper layer
        // and reconstruct nets out of it. Exclude them by their OWN word first.
        ("assy", &[][..]),
        ("dimension", &[][..]),
        ("drawing", &[][..]),
        ("keepout", &[][..]),
        ("adhes", &["gma", "gba"][..]),
        ("glue", &[][..]),
    ];
    for (word, exts) in non_copper {
        if n.has(word) || exts.iter().any(|e| n.ext_is(e)) {
            return LayerRole::Ignored;
        }
    }

    // ── Outline / edge ──────────────────────────────────────────────────────
    if n.has("edge")
        || n.has("outline")
        || n.has("boardoutline")
        || n.has("-gko")
        || n.has("margin")
        || n.has_word("border")
        || n.ext_is("gko")
        || n.ext_is("gm1")
        || n.ext_is("gml")
    {
        return LayerRole::Outline;
    }

    // ── Copper by Protel/Altium extension ───────────────────────────────────
    if n.ext_is("gtl") {
        return LayerRole::Copper {
            index: 0,
            name: top_label(n.original),
        };
    }
    if n.ext_is("gbl") {
        return LayerRole::Copper {
            index: usize::MAX,
            name: bottom_label(n.original),
        };
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
    if n.has("f_cu")
        || n.has("f.cu")
        || (n.has("top") && has_copper(&n))
        || n.has("toplayer")
        || n.has("top layer")
    {
        return LayerRole::Copper {
            index: 0,
            name: top_label(n.original),
        };
    }
    if n.has("b_cu")
        || n.has("b.cu")
        || (n.has("bottom") && has_copper(&n))
        || n.has("bottomlayer")
        || n.has("bottom layer")
    {
        return LayerRole::Copper {
            index: usize::MAX,
            name: bottom_label(n.original),
        };
    }
    if let Some(idx) = kicad_inner_index(&n) {
        return LayerRole::Copper {
            index: idx,
            name: n.original.to_string(),
        };
    }

    // ── Bare role names on a plotted film ───────────────────────────────────
    // `Top.gbr` / `Bottom.gbr`, `1 - Top.gbr` / `2 - Bottom.gbr`,
    // `board-Front.gbr` / `board-Back.gbr`, `TOP.gbr` / `BOTTOM.gbr` with
    // `L2-GND.gbr` inners. DipTrace, Sprint Layout, PCB Elegance and several
    // house CAM scripts all plot copper this way, and it was the single most
    // common shape in a 60-job corpus of real fab folders: the KiCad and Protel
    // rules above matched none of them, so the whole job was refused with "no
    // copper gerber layers found here" while the copper sat right there.
    //
    // Safe only HERE, at the end: every mask / paste / silk / assembly /
    // dimension / outline film carrying the same role word has already been
    // claimed by the tests above, so what is left with a bare `top` is copper.
    if is_gerber_film_ext(&n) {
        if n.has_word("top") || n.has_word("front") {
            return LayerRole::Copper {
                index: 0,
                name: top_label(n.original),
            };
        }
        if n.has_word("bottom") || n.has_word("bot") || n.has_word("back") {
            return LayerRole::Copper {
                index: usize::MAX,
                name: bottom_label(n.original),
            };
        }
        // Inner films named by stack position: `L2-GND.gbr`, `l3.gbr`. L1 is the
        // top layer, so `L<n>` maps to inner index n-1.
        if let Some(idx) = bare_stack_index(&n) {
            return LayerRole::Copper {
                index: idx,
                name: n.original.to_string(),
            };
        }
    }

    // ── Allegro / Cadence `.art` exports (e.g. uConsole) ────────────────────
    // The copper films are named by role: `top.art`, `bottom.art`, and inner
    // plane layers `gnd02.art`, `pwr04.art`, `gnd05.art` where the number is the
    // stack position. The mask/silk/paste films (`solder_*`, `silk_*`,
    // `paste_*`) were already caught above; the assembly films (`adt`, `adb`)
    // and `art03` / `art_aper` are not copper.
    if n.ext_is("art") {
        let stem = &n.full;
        if stem.starts_with("top") {
            return LayerRole::Copper {
                index: 0,
                name: "TOP".to_string(),
            };
        }
        if stem.starts_with("bottom") || stem.starts_with("bot.") {
            return LayerRole::Copper {
                index: usize::MAX,
                name: "BOTTOM".to_string(),
            };
        }
        // Inner plane with an embedded stack number: gnd02, pwr04, gnd05, l3 …
        if stem.starts_with("gnd")
            || stem.starts_with("pwr")
            || stem.starts_with("pgnd")
            || stem.starts_with("power")
            || stem.starts_with("plane")
        {
            let digits: String = stem.chars().filter(|c| c.is_ascii_digit()).collect();
            if let Ok(k) = digits.parse::<usize>() {
                if k >= 1 {
                    return LayerRole::Copper {
                        index: k,
                        name: n.original.to_string(),
                    };
                }
            }
            // No number: still a copper plane, drop it after top, before bottom.
            return LayerRole::Copper {
                index: 1,
                name: n.original.to_string(),
            };
        }
    }

    LayerRole::Unknown
}

/// An explicit layer-role mapping file: the escape hatch for exotic jobs whose
/// file names defeat the conventions. Format is one `filename = role` per line
/// (`#` comments allowed), where role is `copper:<index>` (0 = top, larger =
/// deeper, `bottom` for the bottom layer), `drill`, `outline`, or `ignore`:
///
/// ```text
/// top.art    = copper:0
/// gnd02.art  = copper:1
/// pwr04.art  = copper:2
/// bottom.art = copper:bottom
/// drill-1-6.art = drill
/// ```
pub fn parse_mapping(text: &str) -> std::collections::HashMap<String, LayerRole> {
    let mut map = std::collections::HashMap::new();
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let Some((name, role)) = line.split_once('=') else {
            continue;
        };
        let name = name.trim().to_string();
        let role = role.trim().to_ascii_lowercase();
        let parsed = if let Some(idx) = role.strip_prefix("copper:") {
            let idx = idx.trim();
            if idx == "bottom" {
                LayerRole::Copper {
                    index: usize::MAX,
                    name: name.clone(),
                }
            } else if let Ok(k) = idx.parse::<usize>() {
                LayerRole::Copper {
                    index: k,
                    name: name.clone(),
                }
            } else {
                continue;
            }
        } else {
            match role.as_str() {
                "drill" => LayerRole::Drill,
                "outline" => LayerRole::Outline,
                "ignore" => LayerRole::Ignored,
                _ => continue,
            }
        };
        map.insert(name, parsed);
    }
    map
}

/// What a `.gbrjob` job file says one of its files is.
///
/// The job file is the exporter's own manifest (Ucamco Gerber Job File spec;
/// KiCad and Altium both write one): `FilesAttributes[]` lists every film with
/// its `Path` and `FileFunction`, including the copper films' PHYSICAL layer
/// number (`Copper,L3,Inr`). That is the authoritative answer to the two
/// questions filename inference can only guess at: which files are copper, and
/// where each sits in the stack. Filename inference stays as the fallback for
/// jobs that ship no `.gbrjob`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GbrJobRole {
    /// A copper film with its declared layer number and side.
    ///
    /// The NUMBER is not blindly trusted as a physical stack position: real
    /// exporters get it wrong (KiCad 9 writes its internal layer IDs for
    /// inner films, so a four-layer board's manifest reads L1, L5, L7, L4).
    /// What holds even then is the ORDER: the side tags are correct and the
    /// inner numbers increase with depth. The caller therefore ranks films by
    /// (side, number) and treats the numbers as physical only when they are
    /// contiguous `1..=n`.
    Copper { layer: u32, side: GbrJobSide },
    /// A drill/rout file, with the manifest's own plating declaration:
    /// `Plated,...` is `true`, `NonPlated,...` is `false`. Collapsing both to
    /// one role would throw the declaration away and leave plating to the
    /// filename inference, which can promote a manifest-declared mechanical
    /// file to plated (fabricating stitches) merely because a sibling is
    /// named NPTH.
    Drill { plated: bool },
    /// The board outline (`Profile`).
    Outline,
    /// Recognised and electrically irrelevant (mask, silk, paste, ...).
    Ignored,
}

/// The side tag of a `Copper,L<n>,<side>` manifest entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum GbrJobSide {
    Top,
    /// `Inr`, or a missing/unrecognised side field.
    Inner,
    Bottom,
}

/// Parse a `.gbrjob` file's `FilesAttributes` into basename -> role.
///
/// Only the fields the classifier needs are read. A malformed job file (or one
/// with no `FilesAttributes`) yields an empty map, which callers treat as "no
/// job file": the filename fallback then classifies everything, so a broken
/// manifest degrades to today's behavior instead of dropping the job.
pub fn parse_gbrjob(text: &str) -> std::collections::HashMap<String, GbrJobRole> {
    let mut out = std::collections::HashMap::new();
    // Entries are keyed by basename (matching how the job's files are
    // collected), so two manifest paths that differ only by directory can
    // collide. If their roles DISAGREE, the name identifies neither file and
    // is dropped entirely, the same discipline the layer-name tokens follow:
    // applying either role to both files would reconstruct artwork as copper
    // or vanish real copper, on nothing better than map insertion order.
    let mut contested = std::collections::HashSet::new();
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else {
        return out;
    };
    let Some(files) = v.get("FilesAttributes").and_then(|f| f.as_array()) else {
        return out;
    };
    for f in files {
        let Some(path) = f.get("Path").and_then(|p| p.as_str()) else {
            continue;
        };
        // Paths may carry sub-directories; the classifier keys by basename,
        // matching how the job's files were collected.
        let base = path.rsplit(['/', '\\']).next().unwrap_or(path).to_string();
        let Some(func) = f.get("FileFunction").and_then(|p| p.as_str()) else {
            continue;
        };
        let mut fields = func.split(',').map(|s| s.trim());
        let head = fields.next().unwrap_or("").to_ascii_uppercase();
        let role = match head.as_str() {
            "COPPER" => {
                // `Copper,L<n>,<side>`: the declared layer number + side.
                let Some(n) = fields.next().and_then(|l| {
                    let digits: String = l
                        .strip_prefix(['L', 'l'])?
                        .chars()
                        .take_while(|c| c.is_ascii_digit())
                        .collect();
                    digits.parse::<u32>().ok()
                }) else {
                    continue;
                };
                if n < 1 {
                    continue;
                }
                let side = match fields.next().unwrap_or("").to_ascii_uppercase().as_str() {
                    "TOP" => GbrJobSide::Top,
                    "BOT" | "BOTTOM" => GbrJobSide::Bottom,
                    _ => GbrJobSide::Inner,
                };
                GbrJobRole::Copper { layer: n, side }
            }
            "PLATED" => GbrJobRole::Drill { plated: true },
            "NONPLATED" => GbrJobRole::Drill { plated: false },
            "PROFILE" => GbrJobRole::Outline,
            // Everything else the job names is by definition not copper and
            // not drilling: mask, paste, legend, drawings. Marking it Ignored
            // stops the bare-role-name fallback from reading, say, a
            // `Top.gbr` assembly drawing as top copper.
            _ => GbrJobRole::Ignored,
        };
        if contested.contains(&base) {
            continue;
        }
        match out.get(&base) {
            Some(existing) if *existing != role => {
                out.remove(&base);
                contested.insert(base);
            }
            _ => {
                out.insert(base, role);
            }
        }
    }
    out
}

fn top_label(orig: &str) -> String {
    if orig.is_empty() {
        "F.Cu".to_string()
    } else {
        orig.to_string()
    }
}
fn bottom_label(orig: &str) -> String {
    if orig.is_empty() {
        "B.Cu".to_string()
    } else {
        orig.to_string()
    }
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
/// `inner1`, `signal2`, `Copper_Signal_1`, etc.
fn kicad_inner_index(n: &Name) -> Option<usize> {
    // Only a plotted film can be copper. The `signal`/`inner` markers below do
    // not require a `cu` token (Altium and Protel-lineage exporters omit it), so
    // without this gate a `signal_1.csv` pick-and-place or an `inner_2.pdf`
    // drawing classified as a copper layer, and the directory scan claims
    // copper before it looks for placement data: the CSV was swallowed as an
    // empty copper film and the components never bound. Every inner-copper film
    // in the corpus (KiCad `-In1_Cu.gbr`, Altium `_Copper_Signal_1.gbr`,
    // Protel-lineage `-Inner1.gbr`) carries a film extension or none at all.
    if !is_gerber_film_ext(n) && n.ext.is_some() {
        return None;
    }
    /// The layer index that follows a marker, allowing ONE separator between
    /// the word and the number. Altium 24 plots its inner copper as
    /// `<board>_Copper_Signal_1.gbr`; requiring the digit to butt straight up
    /// against `signal` matched `signal1` and missed `signal_1`, so the inner
    /// layers of every Altium four-layer job fell through to `Unknown` and
    /// their copper was silently discarded. A single `-`, `_` or space is the
    /// separator every exporter uses; `.` is not, because that is the
    /// extension boundary.
    fn index_after(tail: &str) -> Option<usize> {
        let digits = |s: &str| -> Option<usize> {
            let d: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
            d.parse().ok()
        };
        digits(tail).or_else(|| match tail.as_bytes().first() {
            Some(b'-' | b'_' | b' ') => digits(&tail[1..]),
            _ => None,
        })
    }
    // Look for "in<k>" followed by "cu", or "inner<k>", or "signal<k>".
    for marker in ["in", "inner", "signal", "layer"] {
        // Scan EVERY occurrence of the marker, not just the first: a project
        // name that itself contains the marker (e.g. "mainboard-In2_Cu",
        // "arduino-In1_Cu") puts a non-digit-tailed "in" ahead of the real
        // `In<k>_Cu` token. `find` stopped at that first match and gave up,
        // silently dropping the inner copper layer; walk all positions and take
        // the first whose tail actually begins with a layer index.
        for (pos, _) in n.full.match_indices(marker) {
            let tail = &n.full[pos + marker.len()..];
            if let Some(k) = index_after(tail) {
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

/// A bare stack-position film name: `L2-GND.gbr`, `l3.gbr`, `layer4.gbr`.
/// `L1` is the top layer by convention, so `L<k>` is inner index `k - 1`.
/// Returns `None` for `L1` (the top rule already claimed it) and for anything
/// past a plausible stack depth, so a project called `L500` cannot invent 500
/// layers of copper.
fn bare_stack_index(n: &Name) -> Option<usize> {
    for marker in ["l", "layer"] {
        for (pos, _) in n.full.match_indices(marker) {
            // Must start the name or follow a separator, else the `l` inside
            // `flrp` or `plain` matches.
            if pos > 0 && n.full.as_bytes()[pos - 1].is_ascii_alphanumeric() {
                continue;
            }
            let tail = &n.full[pos + marker.len()..];
            let digits: String = tail.chars().take_while(|c| c.is_ascii_digit()).collect();
            if digits.is_empty() {
                continue;
            }
            if let Ok(k) = digits.parse::<usize>() {
                if (2..=32).contains(&k) {
                    return Some(k - 1);
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
        assert!(matches!(
            role("board-F_Cu.gbr"),
            LayerRole::Copper { index: 0, .. }
        ));
        assert!(matches!(
            role("board-B_Cu.gbr"),
            LayerRole::Copper {
                index: usize::MAX,
                ..
            }
        ));
        assert!(matches!(
            role("board-In1_Cu.gbr"),
            LayerRole::Copper { index: 1, .. }
        ));
        assert!(matches!(
            role("board-In2_Cu.gbr"),
            LayerRole::Copper { index: 2, .. }
        ));
        assert_eq!(role("board-F_Mask.gbr"), LayerRole::Ignored);
        assert_eq!(role("board-F_Silkscreen.gbr"), LayerRole::Ignored);
        assert_eq!(role("board-F_Paste.gbr"), LayerRole::Ignored);
    }

    #[test]
    fn altium_inner_signal_films_are_copper() {
        // Altium 24 plots inner copper as `<board>_Copper_Signal_1.gbr`, with a
        // separator between the word and the layer index. Requiring the digit
        // to butt straight up against `signal` matched none of them, so both
        // inner films of every Altium four-layer job classified Unknown and
        // their copper never reached the reconstruction: a real four-layer
        // board came back as two layers.
        assert!(matches!(
            role("ARDEP_Mainboard_Copper_Signal_1.gbr"),
            LayerRole::Copper { index: 1, .. }
        ));
        assert!(matches!(
            role("ARDEP_Mainboard_Copper_Signal_2.gbr"),
            LayerRole::Copper { index: 2, .. }
        ));
        assert!(matches!(
            role("ARDEP_Mainboard_Copper_Signal_Top.gbr"),
            LayerRole::Copper { index: 0, .. }
        ));
        assert!(matches!(
            role("ARDEP_Mainboard_Copper_Signal_Bot.gbr"),
            LayerRole::Copper {
                index: usize::MAX,
                ..
            }
        ));
        // The separator is one character of `-`, `_` or space, never a run and
        // never a dot: `signal.1` is a stem/extension boundary, not an index.
        assert!(matches!(
            role("board Inner 3 Cu.gbr"),
            LayerRole::Copper { index: 3, .. }
        ));
        assert!(matches!(
            role("board-signal-2.gbr"),
            LayerRole::Copper { index: 2, .. }
        ));
        // And only a plotted FILM can be copper. The signal/inner markers need
        // no `cu` token, so without an extension gate a placement CSV or a
        // drawing whose name happens to carry one became a copper layer; the
        // directory scan claims copper before it looks for placement data, so
        // the CSV was swallowed as an empty copper film and no component bound.
        assert!(!role("signal_1.csv").is_copper());
        assert!(!role("inner_2.pdf").is_copper());
        assert!(!role("board-Inner1.xlsx").is_copper());
    }

    #[test]
    fn inner_copper_survives_a_project_name_containing_the_marker() {
        // Round-27: a project name whose text contains "in" (or inner/signal/
        // layer) put a non-digit-tailed marker ahead of the real `In<k>_Cu`
        // token. `find` stopped at that first match and dropped the inner copper
        // layer to Unknown, silently vanishing it from reconstruction. Every
        // occurrence must be scanned so the genuine layer index is recovered.
        assert!(
            matches!(
                role("mainboard-In1_Cu.gbr"),
                LayerRole::Copper { index: 1, .. }
            ),
            "the 'in' inside 'mainboard' must not shadow the real In1 token"
        );
        assert!(matches!(
            role("mainboard-In2_Cu.gbr"),
            LayerRole::Copper { index: 2, .. }
        ));
        assert!(matches!(
            role("arduino-In1_Cu.gbr"),
            LayerRole::Copper { index: 1, .. }
        ));
        // A name with the marker but no real inner token stays non-copper.
        assert_ne!(
            role("arduino-F_Silkscreen.gbr"),
            LayerRole::Copper {
                index: 1,
                name: String::new()
            }
        );
        assert_eq!(role("board-Edge_Cuts.gbr"), LayerRole::Outline);
        assert_eq!(role("board-PTH.drl"), LayerRole::Drill);
        assert_eq!(role("board-NPTH.drl"), LayerRole::Drill);
    }

    #[test]
    fn protel_extensions() {
        assert!(matches!(
            role("design.GTL"),
            LayerRole::Copper { index: 0, .. }
        ));
        assert!(matches!(
            role("design.gbl"),
            LayerRole::Copper {
                index: usize::MAX,
                ..
            }
        ));
        assert!(matches!(
            role("design.G1L"),
            LayerRole::Copper { index: 1, .. }
        ));
        assert_eq!(role("design.GTS"), LayerRole::Ignored);
        assert_eq!(role("design.GTO"), LayerRole::Ignored);
        assert_eq!(role("design.GKO"), LayerRole::Outline);
        assert_eq!(role("design.TXT"), LayerRole::Ignored); // bare .txt = readme
        assert_eq!(role("design-drill.txt"), LayerRole::Drill);
        assert_eq!(role("design.drl"), LayerRole::Drill);
    }

    #[test]
    fn generic_words() {
        assert!(matches!(
            role("TopLayer.gbr"),
            LayerRole::Copper { index: 0, .. }
        ));
        assert!(matches!(
            role("Bottom Copper.gbr"),
            LayerRole::Copper {
                index: usize::MAX,
                ..
            }
        ));
    }

    #[test]
    fn allegro_art_names() {
        assert!(matches!(
            role("top.art"),
            LayerRole::Copper { index: 0, .. }
        ));
        assert!(matches!(
            role("bottom.art"),
            LayerRole::Copper {
                index: usize::MAX,
                ..
            }
        ));
        assert!(matches!(
            role("gnd02.art"),
            LayerRole::Copper { index: 2, .. }
        ));
        assert!(matches!(
            role("pwr04.art"),
            LayerRole::Copper { index: 4, .. }
        ));
        assert_eq!(role("drill-1-6.art"), LayerRole::Drill);
        assert_eq!(role("silk_top.art"), LayerRole::Ignored);
        assert_eq!(role("solder_bot.art"), LayerRole::Ignored);
        assert_eq!(role("paste_top.art"), LayerRole::Ignored);
    }

    #[test]
    fn gbrjob_files_attributes_parse_to_roles() {
        let text = r#"{
  "Header": {"GenerationSoftware": {"Vendor": "KiCad"}},
  "FilesAttributes": [
    {"Path": "brd-F_Cu.gbr", "FileFunction": "Copper,L1,Top", "FilePolarity": "Positive"},
    {"Path": "sub/inner_gnd.gbr", "FileFunction": "Copper,L2,Inr"},
    {"Path": "brd-B_Cu.gbr", "FileFunction": "Copper,L4,Bot"},
    {"Path": "brd-PTH.drl", "FileFunction": "Plated,1,4,PTH"},
    {"Path": "brd-NPTH.drl", "FileFunction": "NonPlated,1,4,NPTH"},
    {"Path": "brd-Edge_Cuts.gbr", "FileFunction": "Profile,NP"},
    {"Path": "brd-F_Mask.gbr", "FileFunction": "SolderMask,Top"}
  ]
}"#;
        let m = parse_gbrjob(text);
        assert_eq!(
            m.get("brd-F_Cu.gbr"),
            Some(&GbrJobRole::Copper {
                layer: 1,
                side: GbrJobSide::Top
            })
        );
        // Sub-directory paths key by basename, like the file walk does.
        assert_eq!(
            m.get("inner_gnd.gbr"),
            Some(&GbrJobRole::Copper {
                layer: 2,
                side: GbrJobSide::Inner
            })
        );
        assert_eq!(
            m.get("brd-B_Cu.gbr"),
            Some(&GbrJobRole::Copper {
                layer: 4,
                side: GbrJobSide::Bottom
            })
        );
        assert_eq!(
            m.get("brd-PTH.drl"),
            Some(&GbrJobRole::Drill { plated: true })
        );
        assert_eq!(
            m.get("brd-NPTH.drl"),
            Some(&GbrJobRole::Drill { plated: false })
        );
        assert_eq!(m.get("brd-Edge_Cuts.gbr"), Some(&GbrJobRole::Outline));
        assert_eq!(m.get("brd-F_Mask.gbr"), Some(&GbrJobRole::Ignored));
        // Garbage degrades to an empty map, never a panic or a wrong role.
        assert!(parse_gbrjob("not json").is_empty());
        assert!(parse_gbrjob("{}").is_empty());
    }

    #[test]
    fn gbrjob_basename_collisions_with_conflicting_roles_name_neither() {
        // Entries are keyed by basename; two sub-directory paths that collide
        // with DIFFERENT roles identify nothing (applying either role to both
        // files would read artwork as copper or vanish real copper on map
        // order). Agreeing duplicates keep their shared role.
        let text = r#"{"FilesAttributes": [
    {"Path": "copper/layer.gbr", "FileFunction": "Copper,L1,Top"},
    {"Path": "documentation/layer.gbr", "FileFunction": "AssemblyDrawing,Top"},
    {"Path": "a/mask.gbr", "FileFunction": "SolderMask,Top"},
    {"Path": "b/mask.gbr", "FileFunction": "SolderMask,Top"}
  ]}"#;
        let m = parse_gbrjob(text);
        assert_eq!(
            m.get("layer.gbr"),
            None,
            "a contested basename names neither"
        );
        assert_eq!(m.get("mask.gbr"), Some(&GbrJobRole::Ignored));
    }

    #[test]
    fn mapping_file() {
        let text = "\
# explicit overrides\n\
weird_top.gbr = copper:0\n\
weird_in1.gbr = copper:1\n\
weird_bot.gbr = copper:bottom\n\
holes.txt = drill\n\
edge.gbr = outline\n";
        let m = parse_mapping(text);
        assert!(matches!(
            m.get("weird_top.gbr"),
            Some(LayerRole::Copper { index: 0, .. })
        ));
        assert!(matches!(
            m.get("weird_in1.gbr"),
            Some(LayerRole::Copper { index: 1, .. })
        ));
        assert!(matches!(
            m.get("weird_bot.gbr"),
            Some(LayerRole::Copper {
                index: usize::MAX,
                ..
            })
        ));
        assert_eq!(m.get("holes.txt"), Some(&LayerRole::Drill));
        assert_eq!(m.get("edge.gbr"), Some(&LayerRole::Outline));
    }

    #[test]
    fn dense_reorder() {
        let coppers = vec![
            (
                LayerRole::Copper {
                    index: usize::MAX,
                    name: "B".into(),
                },
                0,
            ),
            (
                LayerRole::Copper {
                    index: 0,
                    name: "F".into(),
                },
                1,
            ),
            (
                LayerRole::Copper {
                    index: 2,
                    name: "In2".into(),
                },
                2,
            ),
            (
                LayerRole::Copper {
                    index: 1,
                    name: "In1".into(),
                },
                3,
            ),
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
