//! Extraction from ODB++ (Siemens/Valor) design archives.
//!
//! ODB++ is the format a board goes to *fabrication and assembly* in, and for a
//! large tier of professional designs it is the only machine-readable form of
//! the design that leaves the CAD tool: the customer gets a `.tgz`, not the
//! Allegro or Xpedition database. It is also, unlike gerbers, an **electrical**
//! format: the job carries its own net list and its own component placement with
//! per-pad net assignment, so connectivity is *read*, not reverse-engineered
//! from copper the way [`crate::gerber`] must.
//!
//! ## Shape of a job
//!
//! ```text
//! matrix/matrix                      the step and layer table
//! misc/info                          job name, units, producing tool
//! steps/<step>/eda/data              net names, package defs, (often) placement
//! steps/<step>/netlists/<n>/netlist  the CAD netlist, as net points
//! steps/<step>/layers/<layer>/features   per-layer geometry
//! steps/<step>/layers/comp_+_top/components   placement, when not in eda/data
//! ```
//!
//! It arrives as a directory, a `.tgz` (the spec's form, and Altium's and
//! Cadence's default) or a `.zip` (KiCad's default). All three are normalized to
//! one in-memory file map first; see [`tree`], which also bounds how much an
//! archive may inflate to ([`tree::MAX_INFLATED_BYTES`]) — a `.tgz` of
//! compressible text is a natural decompression bomb, and one measured at 400 KiB
//! expanded to 400 MiB.
//!
//! A job may hold several steps, and a fab deliverable routinely ships
//! `steps/panel` (an array of board instances, with no components of its own)
//! ahead of `steps/pcb`. The step that is read is the first one carrying a
//! PLACEMENT, and the others are named in [`OdbStats::disagreements`].
//!
//! ## Two places the placement can live, and why both are read
//!
//! The spec allows `CMP` records in `eda/data`, and allows the component layers
//! (`comp_+_top` / `comp_+_bot`) to carry a `components` file with the same
//! `CMP`/`PRP`/`TOP` grammar. Producers disagree: KiCad 9 and Valor NPI both
//! write the component layers and leave `eda/data` with nets and packages only.
//! `eda/data` wins when it has `CMP` records (it is the electrical view, and it
//! is the one the net ordinals belong to) and the component layers are then used
//! only to CROSS-CHECK it; when it has none, the component layers are the
//! source. Which one was used is recorded in [`OdbStats::placement_source`], so
//! a report never has to guess.
//!
//! ## Verified, not trusted
//!
//! The job states the same facts more than once, and a real export can disagree
//! with itself. Every cross-check that the data supports is run and every
//! failure is *named* in [`OdbStats::disagreements`] rather than silently
//! resolved (the same discipline the gerber reader applies to X2 metadata):
//!
//! * net names in `netlists/*/netlist` vs `eda/data`'s `NET` sequence;
//! * netlist points per net vs pads per net (a net with pads but no point, or
//!   fewer points than pads, means one of the two views is stale);
//! * each pad's own name vs the name at that index in the package it claims;
//! * the component layers' placement vs `eda/data`'s, when both exist;
//! * every `features` file's `F <n>` header vs the features actually parsed —
//!   but only where every record in the file was recognised, so an unmodelled
//!   record type does not get reported as the header lying;
//! * `.Z`/`.gz` members that would not inflate;
//! * every pad's net ordinal against the net table's length, and every net
//!   against the pads that reference it.
//!
//! A pad whose net ordinal is out of range, or whose net field is not a number,
//! or a job with no net table at all: these three used to become "no net"
//! silently, which reports a connected board as a disconnected one. The first two
//! are now named; the third is a refusal, because without `eda/data`'s `NET`
//! sequence the ordinals on every pad mean nothing at all.
//!
//! ## Deliberately dropped
//!
//! ODB++ is far richer than hauksbee's IR ([`ExtractedBoard`]: nets, components,
//! pads and their net). These are read past on purpose, not missed:
//!
//! * **Fabrication attributes** — the whole `.attr` namespace beyond `.no_pop`
//!   (`.comp_mount_type`, `.comp_height`, `.drill`, `.pad_usage`, `.smd`,
//!   impedance and tolerance attributes), `misc/attrlist`, `misc/sysattr*`.
//! * **Copper and drill geometry.** The IR holds no polygons, so `features`
//!   files are parsed only to *count* what is on each layer
//!   ([`OdbStats::layers`]) and to check the `F` header. Clearance DRC needs the
//!   original layout file, and ODB++ input therefore reports DRC as not checked
//!   rather than green.
//! * **Stackup and materials** (`stephdr`, `DIELECTRIC` rows, `tools` drill
//!   tables beyond the hole count), **panelization** (multi-step `SR` step
//!   repeats), **profile/rout**, **silkscreen, paste and mask layers**,
//!   **`fonts/`**, **`user/`**, and the subnet/feature-id back-references
//!   (`SNT`/`FID`) that tie a net to individual features.
//! * **Non-`PRP` component metadata**: `PRP` properties are kept verbatim, but
//!   tool-specific routing/strategy properties are not interpreted.
//!
//! ## Two things worth knowing about the KiCad producer specifically
//!
//! Its ODB++ export carries no populate flag, so a DNP part is
//! indistinguishable from a fitted one where the same board's IPC-2581 export
//! marks it `populate="false"`. [`crate::ipc2581`] therefore recovers DNP and
//! this reader cannot; `dnp` is set only from an explicit `.no_pop` attribute.
//!
//! And it escapes the characters ODB++ restricts in a name, so a hierarchical
//! net `Net-(U4-LNA_IN/RF)` is written `Net-(U4-LNA_IN{slash}RF)`. That is undone
//! ([`crate::unescape_kicad_name`]) but ONLY when `misc/info` says KiCad wrote
//! the job, because the brace tokens are KiCad's convention and not the format's.

pub(crate) mod records;
pub(crate) mod tree;

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use crate::altium::VALUE_UNRESOLVED_KEY;
use crate::{Component, ExtractError, ExtractedBoard, Net, Pin};

/// Why a part read from an ODB++ job has no value, for the bind report to print
/// next to its UNRESOLVED verdict.
///
/// It must say *ODB++*: reusing the Altium reason told users of an ODB++ job to
/// go and look in a `.SchDoc` they do not have. ODB++ carries a component's
/// value only as a `PRP` property, and plenty of producers (Valor NPI writes
/// `PRP CLASS` and routing rules but no `PRP Value`) simply do not write one.
/// The value is not fabricated from the `part_name` field, because that field is
/// a footprint string in KiCad's export and inventing "R_0603" as a resistance
/// is how phantom faults get reported.
pub const VALUE_UNRESOLVED_REASON: &str =
    "no value in the ODB++ job: it carries no `PRP Value` property for this part, \
     and the part name is a library/footprint id rather than a value";

use records::{parse_features, FeatureCounts, RawComponent};
use tree::OdbTree;

/// Where the placed components were read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlacementSource {
    /// `steps/<step>/eda/data` `CMP` records: the electrical view.
    EdaData,
    /// The `components` file on the `comp_+_top` / `comp_+_bot` layers.
    ComponentLayers,
}

impl PlacementSource {
    pub fn as_str(self) -> &'static str {
        match self {
            PlacementSource::EdaData => "eda/data",
            PlacementSource::ComponentLayers => "component layers",
        }
    }
}

/// One layer's accounting, for reports that need to say what was on the board
/// without the IR being able to hold the geometry itself.
#[derive(Debug, Clone)]
pub struct OdbLayer {
    pub name: String,
    /// The matrix `TYPE` (`SIGNAL`, `POWER_GROUND`, `DRILL`, …).
    pub layer_type: String,
    pub lines: usize,
    pub pads: usize,
    pub arcs: usize,
    pub surfaces: usize,
}

impl OdbLayer {
    pub fn features(&self) -> usize {
        self.lines + self.pads + self.arcs + self.surfaces
    }
}

/// The honest accounting of an ODB++ read: what was in the job, which of the two
/// placement sources was used, and every cross-check that failed.
#[derive(Debug, Clone)]
pub struct OdbStats {
    /// `misc/info`'s `ODB_SOURCE` / `SAVE_APP`: the tool that wrote the job.
    pub producer: String,
    /// The step that was read.
    pub step: String,
    /// Every step the matrix declares. A multi-step job is read as its first
    /// board step; the others are named here rather than silently ignored.
    pub steps: Vec<String>,
    pub placement_source: PlacementSource,
    /// Per-layer feature accounting for the copper and drill layers.
    pub layers: Vec<OdbLayer>,
    /// Plated + non-plated holes counted on the `DRILL` layers.
    pub drills: usize,
    /// Pads (toeprints) attached to a component.
    pub pads: usize,
    /// Nets the CAD netlist declares, when the job ships one.
    pub netlist_nets: Option<usize>,
    /// Placements dropped as board artwork because they have no pad at all (a
    /// silkscreen logo, a mechanical keep-out). Named, not silently discarded.
    pub artwork: Vec<String>,
    /// True when no placement in the job carried a `.no_pop` attribute, so
    /// `dnp == false` on every part means "the format did not say" rather than
    /// "the part is fitted".
    pub no_pop_attribute_absent: bool,
    /// Nets the job declares that touch no component pad. Legitimate for a net
    /// carried only by vias or a plane, but invisible to every downstream check,
    /// so it is counted and said rather than passed along as if it were wired.
    pub nets_without_pads: Vec<String>,
    /// Cross-checks that did not agree, each phrased as a whole sentence.
    pub disagreements: Vec<String>,
}

impl OdbStats {
    /// Total copper features across the `SIGNAL`/`POWER_GROUND`/`MIXED` layers.
    pub fn copper_features(&self) -> usize {
        self.layers
            .iter()
            .filter(|l| l.layer_type != "DRILL")
            .map(OdbLayer::features)
            .sum()
    }

    /// The whole-sentence notes a report must carry for this read: how the
    /// connectivity was obtained, what the format could not tell us, and every
    /// cross-check that disagreed.
    ///
    /// Computing a disagreement and then leaving it inside the extractor is the
    /// same failure as not computing it, so this is the accessor the
    /// board-input normalizer copies onto its own notes.
    pub fn notes(&self) -> Vec<String> {
        let mut out = vec![format!(
            "ODB++ input: the circuit was read from the job's own EDA data \
             ({}){}, not reverse-engineered from copper. Clearance DRC and \
             trace-geometry SI need the original layout file and were not run.",
            self.placement_source.as_str(),
            if self.netlist_nets.is_some() {
                " and cross-checked against its CAD netlist"
            } else {
                ", which ships no CAD netlist to cross-check it against"
            }
        )];
        // ODB++ has no populate flag unless a producer writes `.no_pop`, so
        // "nothing is DNP" must not be read as a positive statement.
        if self.no_pop_attribute_absent {
            out.push(
                "This ODB++ job carries no populate/`.no_pop` attribute, so \
                 hauksbee cannot tell a do-not-populate part from a fitted one \
                 and has treated every placed part as fitted."
                    .to_string(),
            );
        }
        if !self.artwork.is_empty() {
            out.push(format!(
                "{} placement(s) have no pad and were read as board artwork \
                 rather than parts: {}.",
                self.artwork.len(),
                sample_list(&self.artwork)
            ));
        }
        if !self.nets_without_pads.is_empty() {
            out.push(format!(
                "{} net(s) are declared but touch no component pad, so nothing \
                 downstream can see them: {}.",
                self.nets_without_pads.len(),
                sample_list(&self.nets_without_pads)
            ));
        }
        out.extend(self.disagreements.clone());
        out
    }
}

/// An ODB++ read: the board, plus the accounting that lets a caller report how
/// much of the job was understood and where it contradicted itself.
#[derive(Debug)]
pub struct OdbExtraction {
    pub board: ExtractedBoard,
    pub stats: OdbStats,
}

// ── Entry points ──────────────────────────────────────────────────────────────

/// Read an ODB++ job from a path: an unpacked job directory (or any directory
/// containing exactly one), a `.tgz`/`.tar.gz`/`.tar`, or a `.zip`.
pub fn from_odbpp(path: &Path) -> Result<OdbExtraction, ExtractError> {
    if path.is_dir() {
        return extract_tree(OdbTree::from_dir(path)?);
    }
    let bytes = std::fs::read(path)
        .map_err(|e| ExtractError::Odb(format!("read {}: {e}", path.display())))?;
    from_odbpp_archive(&bytes)
}

/// Read an ODB++ job from archive bytes (`.tgz`, `.tar`, or `.zip`).
pub fn from_odbpp_archive(bytes: &[u8]) -> Result<OdbExtraction, ExtractError> {
    extract_tree(OdbTree::from_archive(bytes)?)
}

/// Content sniff for the archive forms: an archive holding a `matrix/matrix`
/// member. Keyed on the matrix file because it is what makes a tree an ODB++
/// job, and because a zip of gerbers (the other archive hauksbee accepts) never
/// has one.
pub fn looks_like_odbpp_archive(bytes: &[u8]) -> bool {
    tree::archive_has_matrix(bytes)
}

/// Content sniff for the directory form: does this directory (or a single job
/// directory inside it) hold `matrix/matrix`?
pub fn looks_like_odbpp_dir(dir: &Path) -> bool {
    if dir.join("matrix").join("matrix").is_file() {
        return true;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries
        .flatten()
        .any(|e| e.path().join("matrix").join("matrix").is_file())
}

// ── The read ──────────────────────────────────────────────────────────────────

fn extract_tree(tree: OdbTree) -> Result<OdbExtraction, ExtractError> {
    if !tree.has_matrix() {
        return Err(ExtractError::Odb(
            "this is not an ODB++ job: it has no matrix/matrix file, which every \
             ODB++ job carries and which names the job's steps and layers"
                .into(),
        ));
    }
    let matrix_text = tree.text(tree::MATRIX_PATH).unwrap_or_default();
    let matrix = records::parse_matrix(&matrix_text);

    // The steps the matrix declares, intersected with the steps that actually
    // have a directory: producers leave stale matrix rows behind.
    let on_disk: HashSet<String> = tree.dirs_under("steps/").into_iter().collect();
    let mut steps: Vec<String> = matrix
        .steps
        .iter()
        .filter(|s| on_disk.contains(*s))
        .cloned()
        .collect();
    // Steps present on disk but missing from the matrix, in a DETERMINISTIC
    // order: iterating the `HashSet` directly made the step choice depend on
    // hash order, so a job whose matrix omits its step rows read a different
    // step run to run.
    let mut unlisted: Vec<&String> = on_disk.iter().filter(|s| !steps.contains(s)).collect();
    unlisted.sort();
    steps.extend(unlisted.into_iter().cloned());

    // The step to read: the first that carries a PLACEMENT, not simply the
    // first. A fab deliverable routinely ships `steps/panel` (an array of board
    // instances, with no components of its own) ahead of `steps/pcb`, and taking
    // the first step read the panel and reported a one-component board.
    // No step with a placement falls through to the first step, so the
    // "carries no component placement" refusal below can name what that step DID
    // contain instead of a vaguer message here.
    let with_placement = steps.iter().any(|s| step_has_placement(&tree, s, &matrix));
    let step = match steps
        .iter()
        .find(|s| step_has_placement(&tree, s, &matrix))
        .or_else(|| steps.first())
    {
        Some(s) => s.clone(),
        None => {
            return Err(ExtractError::Odb(format!(
                "this ODB++ job has no readable step: matrix/matrix declares {} and \
                 steps/ holds {}. hauksbee needs one step directory with an eda/data \
                 or a component layer in it",
                list_or_none(&matrix.steps),
                list_or_none(&tree.dirs_under("steps/")),
            )));
        }
    };
    let mut disagreements: Vec<String> = Vec::new();
    if with_placement && steps.len() > 1 {
        let others: Vec<&str> = steps
            .iter()
            .filter(|s| **s != step)
            .map(String::as_str)
            .collect();
        disagreements.push(format!(
            "this job has {} steps ({}); step '{step}' is the first that carries a \
             placement and was read, and the other {} ({}) were not",
            steps.len(),
            steps.join(", "),
            others.len(),
            others.join(", ")
        ));
    }
    for path in &tree.undecompressed {
        disagreements.push(format!(
            "{path} is stored compressed in a form this reader cannot inflate \
             (ODB++ permits gzip; this member is not gzip), so its contents were \
             not read"
        ));
    }

    // The producing tool, read before anything else because KiCad escapes every
    // name it writes and that has to be undone as the names are read.
    let info = tree.text("misc/info").unwrap_or_default();
    let producer = info_field(&info, "ODB_SOURCE")
        .or_else(|| info_field(&info, "SAVE_APP"))
        .unwrap_or_default();
    let kicad = producer.to_ascii_uppercase().contains("KICAD");
    let unescape = |s: &str| -> String {
        if kicad {
            crate::unescape_kicad_name(s)
        } else {
            s.to_string()
        }
    };

    // `misc/info`'s `UNITS` is the job-wide default for any file that declares
    // none of its own; see [`records::declared_units`].
    let job_units = info_field(&info, "UNITS")
        .map(|u| records::unit_scale(&u))
        .unwrap_or(1.0);

    let eda_path = format!("steps/{step}/eda/data");
    let eda_text = tree.text(&eda_path);
    let eda = eda_text
        .as_deref()
        .map(|t| records::parse_eda_data(t, job_units));

    let comp_layers = component_layers(&matrix);
    let mut layer_components: Vec<RawComponent> = Vec::new();
    for layer in &comp_layers {
        let path = format!("steps/{step}/layers/{layer}/components");
        let Some(text) = tree.text(&path) else { continue };
        let is_bottom = layer.contains("bot");
        layer_components.extend(records::parse_components_file(&text, is_bottom, job_units));
    }

    let eda_components = eda.as_ref().map(|e| e.components.len()).unwrap_or(0);
    let (raw_components, placement_source) = if eda_components > 0 {
        // eda/data is the electrical view and owns the net ordinals; the
        // component layers become a cross-check.
        if !layer_components.is_empty() && layer_components.len() != eda_components {
            disagreements.push(format!(
                "the job states its placement twice and disagrees: eda/data has \
                 {eda_components} components, the component layers have {}. \
                 eda/data was used",
                layer_components.len()
            ));
        }
        (
            eda.as_ref().map(|e| e.components.clone()).unwrap_or_default(),
            PlacementSource::EdaData,
        )
    } else {
        (layer_components, PlacementSource::ComponentLayers)
    };

    let net_names = eda.as_ref().map(|e| e.nets.clone()).unwrap_or_default();
    let packages = eda.as_ref().map(|e| e.packages.clone()).unwrap_or_default();

    if raw_components.is_empty() {
        return Err(ExtractError::Odb(format!(
            "this ODB++ job carries no component placement: {} '{step}' has {} \
             and no CMP records in either eda/data or a component layer \
             ({}). Without placement there is no connectivity to check: \
             re-export the job with EDA data included",
            if steps.len() > 1 {
                format!("none of its {} steps ({}) does, and step", steps.len(), steps.join(", "))
            } else {
                "step".to_string()
            },
            if eda_text.is_some() {
                format!("an eda/data with {} net records", net_names.len())
            } else {
                "no eda/data".to_string()
            },
            comp_layers.join(", ")
        )));
    }

    // ── Nets ─────────────────────────────────────────────────────────────────
    //
    // A placement's `TOP` records address nets by their ORDINAL in `eda/data`'s
    // `NET` sequence, so without that table the ordinals mean nothing. A job
    // whose `eda/data` is absent, or whose `eda/data` would not inflate, used to
    // read as a board with zero nets and every pad unconnected — a complete
    // circuit reported as a completely disconnected one. Refuse instead: the
    // whole point of reading ODB++ rather than gerbers is that the connectivity
    // is stated.
    let netted_toeprints = raw_components
        .iter()
        .flat_map(|c| c.toeprints.iter())
        .filter(|t| !matches!(t.net, records::NetRef::None))
        .count();
    if net_names.is_empty() {
        return Err(ExtractError::Odb(format!(
            "this ODB++ job places {} component(s) with {netted_toeprints} pad(s) \
             on a net, but carries no net table: {} declares no NET records, so \
             the net numbers on those pads cannot be resolved to net names. \
             hauksbee will not report on a board whose nets it cannot name{}",
            raw_components.len(),
            if eda_text.is_some() {
                format!("'{eda_path}'")
            } else {
                format!("there is no '{eda_path}' and it")
            },
            if tree.undecompressed.iter().any(|p| p == &eda_path) {
                format!(
                    ". '{eda_path}' IS present but stored in a compression this \
                     reader cannot inflate (ODB++ permits gzip; this member is \
                     not gzip)"
                )
            } else {
                String::new()
            }
        )));
    }
    // ODB++ net ordinal 0 is `$NONE$` (a pad on no net), which is exactly
    // hauksbee's "id 0 = no net" convention, so ordinals carry over unchanged.
    // That equivalence is the whole basis of the mapping below, so it is checked
    // rather than assumed: a producer that starts its table at a real net would
    // otherwise have that net silently deleted and every later net shifted onto
    // the wrong pads.
    let none_first = net_names[0].eq_ignore_ascii_case("$NONE$");
    if !none_first {
        disagreements.push(format!(
            "this job's net table does not begin with the `$NONE$` placeholder \
             ODB++ reserves for net 0 (it begins with '{}'), so a pad recorded on \
             net 0 cannot be told apart from one on the first real net; those pads \
             are reported unconnected",
            net_names[0]
        ));
    }
    let nets: Vec<Net> = net_names
        .iter()
        .enumerate()
        .skip(1)
        .map(|(i, name)| Net {
            id: i as i64,
            name: unescape(name),
        })
        .collect();

    // ── Components ───────────────────────────────────────────────────────────
    let mut components: Vec<Component> = Vec::with_capacity(raw_components.len());
    let mut pads = 0usize;
    let mut pin_name_mismatches = 0usize;
    let mut pads_per_net: BTreeMap<i64, usize> = BTreeMap::new();
    // Pads whose net number is outside the net table, and pads whose net field
    // was not a number at all. Both used to become "no net" without a word, which
    // reports a connected pad as floating.
    let mut out_of_range: Vec<String> = Vec::new();
    let mut unparseable: Vec<String> = Vec::new();
    let mut synthesised_names = 0usize;

    for (idx, c) in raw_components.iter().enumerate() {
        let pkg = c.pkg_index.and_then(|i| packages.get(i));
        let mut pins: Vec<Pin> = Vec::with_capacity(c.toeprints.len());
        for t in &c.toeprints {
            let pkg_pin = pkg.and_then(|p| p.pins.get(t.pin_index)).map(String::as_str);
            // The pad's own name is authoritative; the package pin name at the
            // same index is the check. When only one exists, it is the name.
            let number = match (t.name.is_empty(), pkg_pin) {
                (false, Some(p)) => {
                    if p != t.name {
                        pin_name_mismatches += 1;
                    }
                    t.name.clone()
                }
                (false, None) => t.name.clone(),
                (true, Some(p)) => p.to_string(),
                (true, None) => {
                    // Neither the placement nor the package names this pad. A
                    // 1-based positional name is the only thing left, and it is
                    // a GUESS: counted, so the caller can say so.
                    synthesised_names += 1;
                    (t.pin_index + 1).to_string()
                }
            };
            let number = unescape(&number);
            let net = match t.net {
                records::NetRef::None => None,
                records::NetRef::Unparseable(ref raw) => {
                    unparseable.push(format!("{}.{number} ('{raw}')", c.refdes));
                    None
                }
                records::NetRef::Num(n) if n < net_names.len() => Some(n as i64),
                records::NetRef::Num(n) => {
                    out_of_range.push(format!("{}.{number} (net {n})", c.refdes));
                    None
                }
            };
            if let Some(id) = net {
                *pads_per_net.entry(id).or_default() += 1;
            }
            pads += 1;
            pins.push(Pin {
                number,
                net,
                function: String::new(),
                kind: String::new(),
                position: Some((t.x_mm, t.y_mm)),
            });
        }

        // Reference designators are kept VERBATIM: an unannotated `REF**` or
        // `G***` is what the CAD tool holds, and rewriting it would disagree
        // with the native reading of the same board. Two placements under one
        // designator are merged into one part below
        // ([`crate::merge_duplicate_references`]) rather than renamed.
        let reference = if c.refdes.is_empty() {
            format!("UNK{idx}")
        } else {
            unescape(&c.refdes)
        };

        let mut properties: Vec<(String, String)> = Vec::new();
        let mut value = String::new();
        for (k, v) in &c.props {
            if v.is_empty() {
                continue;
            }
            if k.eq_ignore_ascii_case("value") && value.is_empty() {
                value = v.clone();
            } else {
                properties.push((k.clone(), v.clone()));
            }
        }
        if value.is_empty() {
            // The `part_name` field is a manufacturer part number in most tools
            // but a `<library>_<footprint>` string in KiCad's export, so it is
            // never promoted to `value`: doing so would fabricate "R_0603" as
            // the resistance of every resistor.
            properties.push((
                VALUE_UNRESOLVED_KEY.to_string(),
                VALUE_UNRESOLVED_REASON.to_string(),
            ));
        }

        let bottom = c.layer_is_bottom.unwrap_or(c.mirrored);
        components.push(Component {
            reference,
            value,
            lib_id: c.part.clone(),
            footprint: pkg.map(|p| p.name.clone()).unwrap_or_default(),
            position: Some((c.x_mm, c.y_mm, c.rotation)),
            layer: if bottom { "B.Cu" } else { "F.Cu" }.to_string(),
            properties,
            dnp: c.no_pop,
            pins,
        });
    }
    // A placement with no pads is board artwork, not a part: a silkscreen logo,
    // a mechanical outline, a display's keep-out. The native KiCad reader drops
    // these for the same reason (a `Logo-` footprint binding as an inductor, and
    // bind-rate denominators padded with decoration), so an ODB++ re-export of
    // one board must drop them too or the two readings disagree on the part
    // count. How many went is recorded rather than silently lost.
    let artwork: Vec<String> = components
        .iter()
        .filter(|c| c.pins.is_empty())
        .map(|c| c.reference.clone())
        .collect();
    let components = crate::merge_duplicate_references(
        components.into_iter().filter(|c| !c.pins.is_empty()).collect(),
    );
    // Nets nothing is attached to, computed from the FINISHED board so it catches
    // a net lost anywhere above as well as one the job really declares unused.
    let attached: HashSet<i64> = components
        .iter()
        .flat_map(|c| c.pins.iter())
        .filter_map(|p| p.net)
        .collect();
    let nets_without_pads: Vec<String> = nets
        .iter()
        .filter(|n| !attached.contains(&n.id))
        .map(|n| n.name.clone())
        .collect();

    if pads == 0 {
        return Err(ExtractError::Odb(format!(
            "this ODB++ job places {} components but not one of them has a pad \
             (no TOP toeprint records in {}), so it carries no connectivity. \
             hauksbee will not report on a board it cannot wire up",
            raw_components.len(),
            placement_source.as_str()
        )));
    }

    if pin_name_mismatches > 0 {
        disagreements.push(format!(
            "{pin_name_mismatches} pad(s) are named differently by the placement \
             and by the package they are an instance of; the placement's name was \
             used"
        ));
    }
    if !out_of_range.is_empty() {
        disagreements.push(format!(
            "{} pad(s) claim a net number the job's {}-entry net table does not \
             have, so they are reported unconnected: {}",
            out_of_range.len(),
            net_names.len(),
            sample_list(&out_of_range)
        ));
    }
    if !unparseable.is_empty() {
        disagreements.push(format!(
            "{} pad(s) have a net field that is not a net number, so they are \
             reported unconnected: {}",
            unparseable.len(),
            sample_list(&unparseable)
        ));
    }
    if synthesised_names > 0 {
        disagreements.push(format!(
            "{synthesised_names} pad(s) are named by neither the placement nor the \
             package they instance; they were numbered by position, which is a \
             guess and may not match the part's datasheet pin numbering"
        ));
    }

    // ── Cross-check against the CAD netlist ──────────────────────────────────
    // The netlist file escapes its names the same way `eda/data` does, so it is
    // un-escaped on the same terms: comparing an escaped name against an
    // un-escaped one would report every hierarchical net as a disagreement.
    let netlist = find_netlist(&tree, &step).map(|t| {
        let parsed = records::parse_netlist(&t);
        records::CadNetlist {
            points_per_net: parsed
                .points_per_net
                .into_iter()
                .map(|(k, v)| (unescape(&k), v))
                .collect(),
        }
    });
    let netlist_nets = netlist.as_ref().map(|n| {
        n.points_per_net
            .keys()
            .filter(|k| k.as_str() != "$NONE$")
            .count()
    });
    if let Some(nl) = &netlist {
        let eda_set: HashSet<&str> = nets.iter().map(|n| n.name.as_str()).collect();
        let nl_set: HashSet<&str> = nl
            .points_per_net
            .keys()
            .map(String::as_str)
            .filter(|k| *k != "$NONE$")
            .collect();
        let only_eda = sorted_diff(&eda_set, &nl_set);
        let only_nl = sorted_diff(&nl_set, &eda_set);
        if !only_eda.is_empty() {
            disagreements.push(format!(
                "{} net(s) exist in eda/data but not in the CAD netlist: {}",
                only_eda.len(),
                sample_list(&only_eda)
            ));
        }
        if !only_nl.is_empty() {
            disagreements.push(format!(
                "{} net(s) exist in the CAD netlist but not in eda/data: {}",
                only_nl.len(),
                sample_list(&only_nl)
            ));
        }
        // A net's netlist points include vias and test points, so points may
        // exceed pads; FEWER points than pads means one of the two views is
        // stale, which is worth naming.
        let mut short: Vec<String> = Vec::new();
        for net in &nets {
            let Some(&points) = nl.points_per_net.get(&net.name) else {
                continue;
            };
            let pads_here = pads_per_net.get(&net.id).copied().unwrap_or(0);
            if points < pads_here {
                short.push(format!("{} ({points} points, {pads_here} pads)", net.name));
            }
        }
        if !short.is_empty() {
            disagreements.push(format!(
                "{} net(s) have fewer points in the CAD netlist than pads in the \
                 placement, so one of the two views is stale: {}",
                short.len(),
                sample_list(&short)
            ));
        }
    }

    // ── Layer accounting ─────────────────────────────────────────────────────
    let mut layers: Vec<OdbLayer> = Vec::new();
    let mut drills = 0usize;
    for l in matrix.layers.iter().filter(|l| l.is_copper() || l.is_drill()) {
        let path = format!("steps/{step}/layers/{}/features", l.name);
        let counts = match tree.text(&path) {
            Some(text) => parse_features(&text),
            None => FeatureCounts::default(),
        };
        // Only when this reader recognised every record in the file: otherwise a
        // shortfall is its own gap, not the header lying, and reporting it as a
        // disagreement teaches the reader to be ignored.
        if let Some(declared) = counts.declared.filter(|_| counts.unknown_records == 0) {
            if declared != counts.total() {
                disagreements.push(format!(
                    "layer {} declares {declared} features in its `F` header but \
                     {} parsed",
                    l.name,
                    counts.total()
                ));
            }
        }
        if l.is_drill() {
            drills += counts.pads;
        }
        layers.push(OdbLayer {
            name: l.name.clone(),
            layer_type: l.layer_type.clone(),
            lines: counts.lines,
            pads: counts.pads,
            arcs: counts.arcs,
            surfaces: counts.surfaces,
        });
    }

    // `JOB_NAME=job` is KiCad 9's fixed placeholder, not a board name; leaving
    // the name empty lets the caller fall back to the file stem the way it does
    // for a titleless KiCad layout.
    let name = info_field(&info, "JOB_NAME")
        .filter(|n| !n.eq_ignore_ascii_case("job"))
        .unwrap_or_default();

    Ok(OdbExtraction {
        board: ExtractedBoard {
            name,
            nets,
            components,
        },
        stats: OdbStats {
            producer,
            step,
            steps,
            placement_source,
            layers,
            drills,
            pads,
            netlist_nets,
            artwork,
            no_pop_attribute_absent: !raw_components.iter().any(|c| c.no_pop),
            nets_without_pads,
            disagreements,
        },
    })
}

/// The component-layer names to look for: the matrix's `COMPONENT` rows, falling
/// back to the conventional pair when a job's matrix omits them.
fn component_layers(matrix: &records::Matrix) -> Vec<String> {
    let declared: Vec<String> = matrix
        .layers
        .iter()
        .filter(|l| l.is_component())
        .map(|l| l.name.clone())
        .collect();
    if declared.is_empty() {
        return ["comp_+_top", "comp_+_bot"]
            .iter()
            .map(|s| s.to_string())
            .collect();
    }
    declared
}

/// Does this step carry a component placement? A cheap `CMP ` line-start scan
/// over the step's `eda/data` and its component layers, so choosing the right
/// step out of a panelized job costs one byte scan per step rather than a full
/// parse of each.
fn step_has_placement(tree: &OdbTree, step: &str, matrix: &records::Matrix) -> bool {
    let has_cmp = |path: &str| -> bool {
        tree.get(path).is_some_and(|b| {
            b.split(|&c| c == b'\n')
                .any(|line| line.starts_with(b"CMP ") || line.starts_with(b"CMP\t"))
        })
    };
    if has_cmp(&format!("steps/{step}/eda/data")) {
        return true;
    }
    component_layers(matrix)
        .iter()
        .any(|l| has_cmp(&format!("steps/{step}/layers/{l}/components")))
}

/// The step's CAD netlist. ODB++ puts it under `netlists/<name>/netlist`; some
/// producers use the singular `netlist/<name>/netlist`, and a few write
/// `netlist/netlist` directly.
fn find_netlist(tree: &OdbTree, step: &str) -> Option<String> {
    for dir in ["netlists", "netlist"] {
        let prefix = format!("steps/{step}/{dir}/");
        let mut candidates: Vec<&str> = tree
            .paths_under(&prefix)
            .into_iter()
            .filter(|p| p.ends_with("/netlist") || p.ends_with("/netlist_optimize"))
            .collect();
        // Prefer the un-optimized CAD netlist; `cadnet` is the conventional name.
        candidates.sort_by_key(|p| {
            (
                !p.contains("cadnet"),
                p.ends_with("netlist_optimize"),
                p.len(),
            )
        });
        if let Some(p) = candidates.first() {
            return tree.text(p);
        }
        if let Some(t) = tree.text(&format!("{prefix}netlist")) {
            return Some(t);
        }
    }
    None
}

fn info_field(info: &str, key: &str) -> Option<String> {
    info.lines()
        .filter_map(|l| l.split_once('='))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case(key))
        .map(|(_, v)| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn sorted_diff(a: &HashSet<&str>, b: &HashSet<&str>) -> Vec<String> {
    let mut v: Vec<String> = a.difference(b).map(|s| s.to_string()).collect();
    v.sort();
    v
}

/// A comma list, truncated so a 600-net disagreement stays a sentence.
fn sample_list(items: &[String]) -> String {
    const MAX: usize = 6;
    if items.len() <= MAX {
        return items.join(", ");
    }
    format!(
        "{}, and {} more",
        items[..MAX].join(", "),
        items.len() - MAX
    )
}

fn list_or_none(items: &[String]) -> String {
    if items.is_empty() {
        "nothing".to_string()
    } else {
        items.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal but structurally complete two-resistor job, written into a zip
    /// the way KiCad writes one (placement on the component layers, `eda/data`
    /// holding nets and packages).
    pub(crate) fn tiny_job() -> Vec<(&'static str, String)> {
        vec![
            (
                "matrix/matrix",
                "STEP {\n    COL=1\n    NAME=PCB\n}\n\n\
                 LAYER {\n    ROW=1\n    CONTEXT=BOARD\n    TYPE=COMPONENT\n    NAME=COMP_+_TOP\n}\n\
                 LAYER {\n    ROW=2\n    CONTEXT=BOARD\n    TYPE=SIGNAL\n    NAME=F.CU\n}\n\
                 LAYER {\n    ROW=3\n    CONTEXT=BOARD\n    TYPE=DRILL\n    NAME=DRILL_1\n}\n"
                    .to_string(),
            ),
            (
                "misc/info",
                "JOB_NAME=divider\nUNITS=MM\nODB_SOURCE=Test Suite 1.0\n".to_string(),
            ),
            (
                "steps/pcb/eda/data",
                "HDR Test Suite\nUNITS=MM\nLYR f.cu drill_1\n\
                 #NET 0\nNET $NONE$ \n#NET 1\nNET VIN \n#NET 2\nNET MID \n#NET 3\nNET GND \n\
                 # PKG 0\nPKG R_0603 1.6 -0.8 -0.4 0.8 0.4;\nPIN 1 S -0.8 0.0 0 E S\nPIN 2 S 0.8 0.0 0 E S\n#\n"
                    .to_string(),
            ),
            (
                "steps/pcb/layers/comp_+_top/components",
                "UNITS=MM\n#\n# CMP 0\nCMP 0 1.0 1.0 0 N R1 Device_R_0603 ;\n\
                 PRP Value '10k'\nTOP 0 0.2 1.0 0.0 N 1 0 1\nTOP 1 1.8 1.0 0.0 N 2 0 2\n#\n\
                 # CMP 1\nCMP 0 4.0 1.0 0 N R2 Device_R_0603 ;\n\
                 PRP Value '4k7'\nTOP 0 3.2 1.0 0.0 N 2 1 1\nTOP 1 4.8 1.0 0.0 N 3 0 2\n#\n"
                    .to_string(),
            ),
            (
                "steps/pcb/netlists/cadnet/netlist",
                "H optimize n staggered n\n$1 VIN\n$2 MID\n$3 GND\n#\n\
                 1 0 0.2 1.0 T 1.0 1.0 e c\n2 0 1.8 1.0 T 1.0 1.0 e c\n\
                 2 0 3.2 1.0 T 1.0 1.0 e c\n3 0 4.8 1.0 T 1.0 1.0 e c\n"
                    .to_string(),
            ),
            (
                "steps/pcb/layers/f.cu/features",
                "UNITS=MM\n#\n#Num Features\n#\nF 5\n#\n#Layer features\n#\n\
                 L 1.8 1.0 3.2 1.0 0 P 0 \nP 0.2 1.0 0 P 0 8 0.0 \nP 1.8 1.0 0 P 0 8 0.0 \n\
                 P 3.2 1.0 0 P 0 8 0.0 \nP 4.8 1.0 0 P 0 8 0.0 \n"
                    .to_string(),
            ),
            (
                "steps/pcb/layers/drill_1/features",
                "UNITS=MM\nF 2\n#\nP 0.2 1.0 0 P 0 8 0.0 \nP 4.8 1.0 0 P 0 8 0.0 \n".to_string(),
            ),
        ]
    }

    /// Zip the given members, so the archive path is exercised end to end.
    pub(crate) fn zip_members(members: &[(&str, String)]) -> Vec<u8> {
        let raw: Vec<(&str, Vec<u8>)> = members
            .iter()
            .map(|(n, b)| (*n, b.as_bytes().to_vec()))
            .collect();
        zip_raw(&raw)
    }

    /// [`zip_members`] for members that are not valid UTF-8 (a gzip payload).
    pub(crate) fn zip_raw(members: &[(&str, Vec<u8>)]) -> Vec<u8> {
        use std::io::Write;
        let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opts: zip::write::FileOptions<'_, ()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Stored);
        for (name, body) in members {
            w.start_file(*name, opts).expect("zip entry");
            w.write_all(body).expect("zip write");
        }
        w.finish().expect("zip finish").into_inner()
    }

    #[test]
    fn reads_nets_components_pads_and_values_from_a_zip_job() {
        let zip = zip_members(&tiny_job());
        assert!(looks_like_odbpp_archive(&zip), "the matrix is the sniff");
        let out = from_odbpp_archive(&zip).expect("job reads");
        assert_eq!(out.board.name, "divider");
        assert_eq!(out.board.nets.len(), 3, "$NONE$ is not a net");
        assert_eq!(out.board.components.len(), 2);
        let r1 = out.board.component("R1").expect("R1");
        assert_eq!(r1.value, "10k", "PRP Value is the value");
        assert_eq!(r1.footprint, "R_0603", "the PKG name is the footprint");
        assert_eq!(r1.layer, "F.Cu");
        assert_eq!(r1.pins.len(), 2);
        assert_eq!(r1.pins[0].number, "1");
        // MID joins R1.2 and R2.1.
        let mid = out.board.net_by_name("MID").expect("MID");
        assert_eq!(out.board.net_members(mid.id).len(), 2);
        // Accounting.
        assert_eq!(out.stats.pads, 4);
        assert_eq!(out.stats.drills, 2);
        assert_eq!(out.stats.copper_features(), 5);
        assert_eq!(out.stats.netlist_nets, Some(3));
        assert_eq!(out.stats.placement_source, PlacementSource::ComponentLayers);
        assert_eq!(out.stats.producer, "Test Suite 1.0");
        assert!(
            out.stats.disagreements.is_empty(),
            "a self-consistent job must report no disagreement: {:?}",
            out.stats.disagreements
        );
    }

    #[test]
    fn a_stale_cad_netlist_is_reported_not_reconciled() {
        // Drop GND from the netlist and give MID one point where two pads sit:
        // both are real staleness and both must be named.
        let mut members = tiny_job();
        for m in &mut members {
            if m.0 == "steps/pcb/netlists/cadnet/netlist" {
                m.1 = "H optimize n staggered n\n$1 VIN\n$2 MID\n$9 EXTRA\n#\n\
                       1 0 0.2 1.0 T 1.0 1.0 e c\n2 0 1.8 1.0 T 1.0 1.0 e c\n\
                       9 0 9.0 9.0 T 1.0 1.0 e c\n"
                    .to_string();
            }
        }
        let out = from_odbpp_archive(&zip_members(&members)).expect("job still reads");
        let joined = out.stats.disagreements.join(" | ");
        assert!(
            joined.contains("exist in eda/data but not in the CAD netlist") && joined.contains("GND"),
            "a net missing from the netlist must be named: {joined}"
        );
        assert!(
            joined.contains("exist in the CAD netlist but not in eda/data")
                && joined.contains("EXTRA"),
            "a net only in the netlist must be named: {joined}"
        );
        assert!(
            joined.contains("fewer points in the CAD netlist than pads") && joined.contains("MID"),
            "a net with fewer points than pads must be named: {joined}"
        );
        // The board itself still reads: a disagreement is reported, not fatal.
        assert_eq!(out.board.components.len(), 2);
    }

    #[test]
    fn a_pad_named_differently_by_its_package_is_reported() {
        let mut members = tiny_job();
        for m in &mut members {
            if m.0 == "steps/pcb/layers/comp_+_top/components" {
                // R1's second pad calls itself "K" where the package says "2".
                m.1 = m.1.replace("TOP 1 1.8 1.0 0.0 N 2 0 2", "TOP 1 1.8 1.0 0.0 N 2 0 K");
            }
        }
        let out = from_odbpp_archive(&zip_members(&members)).expect("job reads");
        assert_eq!(
            out.board.component("R1").expect("R1").pins[1].number,
            "K",
            "the placement's own pad name wins"
        );
        assert!(
            out.stats
                .disagreements
                .iter()
                .any(|d| d.contains("named differently by the placement")),
            "the mismatch must be reported: {:?}",
            out.stats.disagreements
        );
    }

    #[test]
    fn eda_data_placement_wins_and_the_component_layers_become_the_check() {
        let mut members = tiny_job();
        for m in &mut members {
            if m.0 == "steps/pcb/eda/data" {
                m.1.push_str(
                    "# CMP 0\nCMP 0 1.0 1.0 0 N R1 Device_R_0603 ;\nPRP Value '10k'\n\
                     TOP 0 0.2 1.0 0.0 N 1 0 1\nTOP 1 1.8 1.0 0.0 N 3 0 2\n#\n",
                );
            }
        }
        let out = from_odbpp_archive(&zip_members(&members)).expect("job reads");
        assert_eq!(out.stats.placement_source, PlacementSource::EdaData);
        assert_eq!(out.board.components.len(), 1, "eda/data's one CMP wins");
        assert!(
            out.stats
                .disagreements
                .iter()
                .any(|d| d.contains("states its placement twice")),
            "the two-source count mismatch must be reported: {:?}",
            out.stats.disagreements
        );
    }

    #[test]
    fn a_job_with_no_placement_refuses_and_names_what_was_missing() {
        let members: Vec<(&str, String)> = tiny_job()
            .into_iter()
            .filter(|(n, _)| *n != "steps/pcb/layers/comp_+_top/components")
            .collect();
        let err = from_odbpp_archive(&zip_members(&members))
            .expect_err("a job with nets but no placement must refuse");
        let msg = err.to_string();
        assert!(msg.contains("no component placement"), "got: {msg}");
        assert!(msg.contains("4 net records"), "names what it did find: {msg}");
        assert!(msg.contains("comp_+_top"), "names where it looked: {msg}");
    }

    #[test]
    fn a_placement_with_no_pads_refuses_rather_than_returning_a_half_board() {
        let mut members = tiny_job();
        for m in &mut members {
            if m.0 == "steps/pcb/layers/comp_+_top/components" {
                m.1 = m
                    .1
                    .lines()
                    .filter(|l| !l.starts_with("TOP "))
                    .collect::<Vec<_>>()
                    .join("\n");
            }
        }
        let err = from_odbpp_archive(&zip_members(&members))
            .expect_err("components without pads must refuse");
        let msg = err.to_string();
        assert!(msg.contains("not one of them has a pad"), "got: {msg}");
        assert!(msg.contains("no connectivity"), "got: {msg}");
    }

    #[test]
    fn a_job_whose_net_table_is_missing_refuses_instead_of_reading_every_pad_open() {
        // The placement addresses nets by ORDINAL into eda/data's NET sequence.
        // Without that file the ordinals mean nothing, and the reader used to
        // return a complete board with zero nets and every pad unconnected.
        let members: Vec<(&str, String)> = tiny_job()
            .into_iter()
            .filter(|(n, _)| *n != "steps/pcb/eda/data")
            .collect();
        let err = from_odbpp_archive(&zip_members(&members))
            .expect_err("a placement with no net table must refuse");
        let msg = err.to_string();
        assert!(msg.contains("carries no net table"), "got: {msg}");
        assert!(msg.contains("2 component(s)"), "got: {msg}");
        assert!(msg.contains("4 pad(s) on a net"), "got: {msg}");
        assert!(msg.contains("cannot be resolved to net names"), "got: {msg}");
    }

    #[test]
    fn an_eda_data_that_will_not_inflate_says_so_in_the_refusal() {
        // The same refusal, but the file IS there and merely unreadable: the
        // message must distinguish "absent" from "present but not inflatable",
        // because the fixes differ.
        let mut members: Vec<(&str, Vec<u8>)> = tiny_job()
            .into_iter()
            .filter(|(n, _)| *n != "steps/pcb/eda/data")
            .map(|(n, b)| (n, b.into_bytes()))
            .collect();
        // A Unix-`compress` `.Z` (magic 1f 9d), which ODB++ permits producers to
        // use but which is not gzip.
        members.push((
            "steps/pcb/eda/data.Z",
            vec![0x1f, 0x9d, 0x90, 0x01, 0x02, 0x03],
        ));
        let refs: Vec<(&str, String)> = members
            .iter()
            .map(|(n, b)| (*n, String::from_utf8_lossy(b).into_owned()))
            .collect();
        let err =
            from_odbpp_archive(&zip_members(&refs)).expect_err("an unreadable net table refuses");
        let msg = err.to_string();
        assert!(msg.contains("carries no net table"), "got: {msg}");
        assert!(
            msg.contains("IS present but stored in a compression"),
            "the refusal must distinguish absent from unreadable: {msg}"
        );
    }

    #[test]
    fn a_pad_on_a_net_the_job_never_declares_is_reported_not_silently_floated() {
        let mut members = tiny_job();
        for m in &mut members {
            if m.0 == "steps/pcb/layers/comp_+_top/components" {
                // Net 77 against a 4-entry table, and a net field that is not a
                // number at all.
                m.1 = m
                    .1
                    .replace("TOP 1 1.8 1.0 0.0 N 2 0 2", "TOP 1 1.8 1.0 0.0 N 77 0 2")
                    .replace("TOP 0 3.2 1.0 0.0 N 2 1 1", "TOP 0 3.2 1.0 0.0 N MID 1 1");
            }
        }
        let out = from_odbpp_archive(&zip_members(&members)).expect("the job still reads");
        let joined = out.stats.disagreements.join(" | ");
        assert!(
            joined.contains("claim a net number the job's 4-entry net table does not have")
                && joined.contains("R1.2 (net 77)"),
            "got: {joined}"
        );
        assert!(
            joined.contains("net field that is not a net number") && joined.contains("R2.1 ('MID')"),
            "got: {joined}"
        );
        // And those pads really are reported unconnected, not guessed.
        assert!(out.board.component("R1").expect("R1").pins[1].net.is_none());
    }

    #[test]
    fn a_net_table_that_does_not_start_with_none_is_reported() {
        let mut members = tiny_job();
        for m in &mut members {
            if m.0 == "steps/pcb/eda/data" {
                m.1 = m.1.replace("#NET 0\nNET $NONE$ \n", "");
            }
        }
        let out = from_odbpp_archive(&zip_members(&members)).expect("the job reads");
        assert!(
            out.stats
                .disagreements
                .iter()
                .any(|d| d.contains("does not begin with the `$NONE$` placeholder")),
            "got: {:?}",
            out.stats.disagreements
        );
    }

    #[test]
    fn a_panelized_job_reads_the_board_step_not_the_panel() {
        // Fab deliverables ship `steps/panel` (an array of board instances, no
        // components of its own) alongside `steps/pcb`. Taking the first step
        // read the panel and reported a one-component board.
        let mut members: Vec<(&str, String)> = tiny_job();
        for m in &mut members {
            if m.0 == "matrix/matrix" {
                m.1 = format!(
                    "STEP {{\n    COL=1\n    NAME=PANEL\n}}\n\n{}",
                    m.1.replace("COL=1", "COL=2")
                );
            }
        }
        members.push((
            "steps/panel/stephdr",
            "AFFECTING_BOM=\nUNITS=MM\n".to_string(),
        ));
        let out = from_odbpp_archive(&zip_members(&members)).expect("the job reads");
        assert_eq!(out.stats.step, "pcb", "the board step, not the panel");
        assert_eq!(out.stats.steps, vec!["panel", "pcb"]);
        assert_eq!(out.board.components.len(), 2);
        assert!(
            out.stats
                .disagreements
                .iter()
                .any(|d| d.contains("first that carries a placement") && d.contains("panel")),
            "the skipped step must be named: {:?}",
            out.stats.disagreements
        );
    }

    #[test]
    fn a_job_that_inflates_past_the_ceiling_refuses_and_names_the_limit() {
        use std::io::Write;
        // A gzip member of highly compressible zeros, larger than the ceiling.
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        let chunk = vec![0u8; 1 << 20];
        for _ in 0..(tree::MAX_INFLATED_BYTES / (1 << 20) + 4) {
            enc.write_all(&chunk).expect("gz write");
        }
        let bomb = enc.finish().expect("gz finish");
        assert!(
            (bomb.len() as u64) < tree::MAX_INFLATED_BYTES / 100,
            "the bomb must be small compressed: {} bytes",
            bomb.len()
        );
        // Built from raw bytes: a gzip payload is not valid UTF-8, so the
        // string-based helper would corrupt it before it was ever inflated.
        let mut raw: Vec<(&str, Vec<u8>)> = tiny_job()
            .into_iter()
            .map(|(n, b)| (n, b.into_bytes()))
            .collect();
        raw.push(("steps/pcb/layers/f.cu/features.Z", bomb));
        let err = from_odbpp_archive(&zip_raw(&raw)).expect_err("a bomb must refuse");
        let msg = err.to_string();
        assert!(msg.contains("expands to more than"), "got: {msg}");
        assert!(msg.contains("MiB"), "the limit is named: {msg}");
    }

    #[test]
    fn a_non_finite_coordinate_never_reaches_the_ir() {
        // `parse::<f64>()` accepts `NaN`, `inf` and `1e400`. A distance compared
        // against any of them is silently false, which is how a clearance or
        // length check turns into a meaningless pass; the native readers refuse
        // such a file outright, so nothing non-finite may get in here either.
        let mut members = tiny_job();
        for m in &mut members {
            if m.0 == "steps/pcb/layers/comp_+_top/components" {
                m.1 = m
                    .1
                    .replace("CMP 0 1.0 1.0 0 N R1", "CMP 0 NaN 1.0 0 N R1")
                    .replace("TOP 1 4.8 1.0 0.0 N 3 0 2", "TOP 1 1e400 1.0 0.0 N 3 0 2");
            }
        }
        let out = from_odbpp_archive(&zip_members(&members)).expect("the job still reads");
        for c in &out.board.components {
            if let Some((x, y, r)) = c.position {
                assert!(
                    x.is_finite() && y.is_finite() && r.is_finite(),
                    "{}: position {x},{y},{r} is not finite",
                    c.reference
                );
            }
            for p in &c.pins {
                if let Some((x, y)) = p.position {
                    assert!(
                        x.is_finite() && y.is_finite(),
                        "{}.{}: pad position {x},{y} is not finite",
                        c.reference,
                        p.number
                    );
                }
            }
        }
        // The records carrying them are dropped, not silently zeroed: R1's
        // placement and R2's second pad are gone rather than sitting at the origin
        // pretending to be real geometry.
        assert!(
            out.board.component("R1").is_none(),
            "the component whose placement was not a number must not be invented"
        );
    }

    #[test]
    fn a_non_odbpp_archive_is_not_claimed() {
        let zip = zip_members(&[("gerbers/top.gbr", "%FSLAX46Y46*%\n".to_string())]);
        assert!(
            !looks_like_odbpp_archive(&zip),
            "a gerber zip must not be claimed as ODB++"
        );
        let err = from_odbpp_archive(&zip).expect_err("and must not read as one");
        assert!(err.to_string().contains("no matrix/matrix file"));
    }

    #[test]
    fn a_features_header_that_lies_about_its_count_is_reported() {
        let mut members = tiny_job();
        for m in &mut members {
            if m.0 == "steps/pcb/layers/f.cu/features" {
                m.1 = m.1.replace("F 5", "F 9");
            }
        }
        let out = from_odbpp_archive(&zip_members(&members)).expect("job reads");
        assert!(
            out.stats
                .disagreements
                .iter()
                .any(|d| d.contains("declares 9 features") && d.contains("5 parsed")),
            "got: {:?}",
            out.stats.disagreements
        );
    }

    #[test]
    fn a_directory_job_reads_the_same_as_its_zip() {
        let dir = std::env::temp_dir().join(format!("hauksbee-odb-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // Wrap the job in a sub-directory, the way an unpacked .tgz lands.
        for (name, body) in tiny_job() {
            let p = dir.join("sample_design").join(name);
            std::fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
            std::fs::write(&p, body).expect("write");
        }
        assert!(looks_like_odbpp_dir(&dir), "a directory holding one job");
        let from_dir = from_odbpp(&dir).expect("directory job reads");
        let from_zip = from_odbpp_archive(&zip_members(&tiny_job())).expect("zip job reads");
        assert_eq!(from_dir.board.components.len(), from_zip.board.components.len());
        assert_eq!(from_dir.board.nets.len(), from_zip.board.nets.len());
        assert_eq!(from_dir.stats.pads, from_zip.stats.pads);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_tgz_job_reads_and_is_sniffed() {
        use std::io::Write;
        let mut tar = tar::Builder::new(Vec::new());
        for (name, body) in tiny_job() {
            let mut h = tar::Header::new_ustar();
            h.set_size(body.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            tar.append_data(&mut h, format!("sample_design/{name}"), body.as_bytes())
                .expect("tar append");
        }
        let tar = tar.into_inner().expect("tar finish");
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        enc.write_all(&tar).expect("gz write");
        let tgz = enc.finish().expect("gz finish");

        assert!(looks_like_odbpp_archive(&tgz), "a .tgz job is sniffed");
        let out = from_odbpp_archive(&tgz).expect(".tgz job reads");
        assert_eq!(out.board.components.len(), 2);
        assert_eq!(out.stats.pads, 4);
    }
}
