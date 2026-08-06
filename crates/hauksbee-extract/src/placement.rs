//! Pick-and-place and centroid files: where each part sits, and which parts the
//! assembler was told to place.
//!
//! A placement file is the assembly half of what a BOM is the purchasing half
//! of. It carries three things the layout alone does not always give: the list
//! of parts that were actually placed (so a part on the layout and absent from
//! the placement file was not assembled), the board side of each one, and a
//! value string. That last one matters more than it sounds. An Altium `.PcbDoc`
//! keeps values in the schematic, so a layout-only Altium read has no value for
//! anything and everything binds unresolved; the same project's pick-and-place
//! file has a `Comment` column with the value in it.
//!
//! Four shapes cover the field, all in [`PlacementDialect`]. Two are
//! fixed-width text whose columns must be sliced by the header's own offsets,
//! because a value containing a space (`"1uF 10V"`, `"Capacitor Tantalum SMD"`)
//! is ordinary and a whitespace split of the data would shift every later column.
//!
//! The reason this module cross-checks rather than just reads: a placement file
//! and a layout are two descriptions of the same board, and where they disagree
//! about WHERE a part is, one of them is from a different revision. That is
//! evidence, not noise, so [`PlacementFile::cross_check`] reports it and
//! [`PlacementCrossCheck::is_different_board`] answers the question a caller
//! actually has.
//!
//! A second, deliberately more permissive reader for the same files lives in
//! [`crate::gerber::placement`]. It serves the gerber-only path, where the fab
//! package is the whole design and there is no layout to reconcile against, so
//! guessing is the best answer available. This one refuses instead, because here
//! a layout exists and a wrong reading of it is worse than no reading.
//!
//! Long-form: `docs/ingest/BOM.md`.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::bom::{
    decode, fixed_width_columns, normalise_header, sha256_hex, slice_fixed_width, sniff_delimiter,
    split_delimited, Contribution, IdentityHint, IgnoredInput, EXIT_INVALID_FOR_ANALYSIS,
};
use crate::ExtractedBoard;

/// How far a placement may sit from the layout's own position for the two to
/// count as agreeing, in millimetres.
///
/// Every writer surveyed emits four decimal places or better, and the round trip
/// through a decimal string is exact to well under a micron, so the only thing
/// this tolerance absorbs is the writers that round to three places. It is
/// deliberately far tighter than any real placement difference: moving a part by
/// a tenth of a millimetre between revisions is a change, and this check exists
/// to notice changes.
pub const POSITION_TOLERANCE_MM: f64 = 0.01;

/// How many lines of banner a placement file may spend before its header.
/// Altium's pick-and-place spends up to thirteen.
const MAX_BANNER_LINES: usize = 40;

// ── Errors ──────────────────────────────────────────────────────────────────

/// Why a placement file could not be used. Every variant is a whole sentence
/// naming the file, the problem and the next action.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum PlacementError {
    #[error(
        "{name} is empty, so there are no placements to read. If the export produced \
         a zero-byte file, re-run it"
    )]
    Empty { name: String },

    #[error(
        "{name} does not read as a pick-and-place file: it has no X and Y position \
         columns. hauksbee reads KiCad `.pos` (ascii or csv), Altium's Pick and Place \
         export, and the generic CPL that JLCPCB and PCBWay accept. A file with \
         designators and values but no coordinates is a BOM, not a placement file: \
         pass it as the BOM instead"
    )]
    NotAPlacementFile { name: String },

    #[error(
        "{name} says its units are {units:?}, which hauksbee cannot convert. A \
         placement file must state millimetres, inches or mils. Re-export it in \
         millimetres"
    )]
    UnknownUnits { name: String, units: String },

    #[error(
        "{name} has {bad} rows out of {total} whose coordinates are not numbers, so \
         the positions in it cannot be trusted. The first is line {line}: {cell:?}. \
         Re-export the file"
    )]
    UnreadableCoordinates {
        name: String,
        bad: usize,
        total: usize,
        line: usize,
        cell: String,
    },

    #[error(
        "{name} names {rows} rows but none of them has a reference designator, so \
         nothing in it can be matched to a part on the board. Re-export it with the \
         designator column included"
    )]
    NoDesignators { name: String, rows: usize },

    /// A valid position file that places nothing. This is a refusal rather than
    /// an empty success because reconciling a board against it would report every
    /// part on the board as unplaced, which is a confident wrong answer about the
    /// assembly.
    #[error(
        "{name} is a {dialect} that places nothing: it has a header and no rows. A \
         side-specific export does this when that side of the board is empty. Pass \
         the file for the other side, or the combined one, instead"
    )]
    NoPlacements { name: String, dialect: &'static str },

    #[error("cannot read {name}: {detail}")]
    Io { name: String, detail: String },
}

impl PlacementError {
    /// Always [`EXIT_INVALID_FOR_ANALYSIS`]: each variant means the input cannot
    /// be analysed, not that an assertion failed.
    pub fn exit_code(&self) -> i32 {
        EXIT_INVALID_FOR_ANALYSIS
    }
}

// ── Dialects ────────────────────────────────────────────────────────────────

/// The placement-file shapes that actually exist. Frequencies are from the
/// survey recorded in `docs/ingest/BOM.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlacementDialect {
    /// KiCad's ascii `.pos`: a `###` banner naming the tool and the units, a
    /// `# Ref Val Package PosX PosY Rot Side` header, fixed-width rows, and a
    /// `## End` terminator. KiCad 5 calls the banner "Module positions" and
    /// KiCad 6 and later "Footprint positions"; both are read. 45 files.
    KicadPosAscii,
    /// KiCad's csv `.pos`: `Ref, Val, Package, PosX, PosY, Rot, Side`. 40 files.
    KicadPosCsv,
    /// Altium's Pick and Place export, `Designator, Comment, Layer, Footprint,
    /// Center-X(mm), Center-Y(mm), Rotation, Description`, under a banner naming
    /// the project path, the date and the units. Comes as csv and as fixed-width
    /// `.txt` from the same dialog; 81 files across both.
    AltiumPickPlace,
    /// The generic CPL that JLCPCB and PCBWay accept and that a dozen scripts
    /// generate: `Designator, Mid X, Mid Y, Rotation, Layer`, with or without
    /// `Val` and `Package`, and with the coordinate headers spelled `MidX` or
    /// `Mid X` interchangeably. 80 files, the most common shape.
    GenericCpl,
}

impl PlacementDialect {
    /// The stable machine name recorded in provenance.
    pub fn kind(self) -> &'static str {
        match self {
            PlacementDialect::KicadPosAscii => "kicad_pos_ascii",
            PlacementDialect::KicadPosCsv => "kicad_pos_csv",
            PlacementDialect::AltiumPickPlace => "altium_pick_and_place",
            PlacementDialect::GenericCpl => "cpl",
        }
    }

    pub fn describe(self) -> &'static str {
        match self {
            PlacementDialect::KicadPosAscii => "KiCad position file",
            PlacementDialect::KicadPosCsv => "KiCad position file (csv)",
            PlacementDialect::AltiumPickPlace => "Altium Pick and Place",
            PlacementDialect::GenericCpl => "CPL (JLCPCB / PCBWay)",
        }
    }
}

/// Which side of the board a part is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Top,
    Bottom,
}

impl Side {
    /// The KiCad copper-layer name, so a side can be compared against
    /// [`crate::Component::layer`] without either side of the comparison
    /// guessing.
    pub fn kicad_layer(self) -> &'static str {
        match self {
            Side::Top => "F.Cu",
            Side::Bottom => "B.Cu",
        }
    }

    /// The word a report prints, so a line reads as a sentence rather than as a
    /// Rust variant name.
    pub fn describe(self) -> &'static str {
        match self {
            Side::Top => "top",
            Side::Bottom => "bottom",
        }
    }

    /// Read a side out of whatever the file called it: `top`, `bottom`, `TOP`,
    /// `TopLayer`, `BottomSolder`, `F.Cu`, `B.Cu`, `T`, `B`.
    fn parse(s: &str) -> Option<Side> {
        let s = s.trim().to_ascii_lowercase();
        if s.starts_with("top") || s == "t" || s == "f.cu" || s == "f" || s == "front" {
            Some(Side::Top)
        } else if s.starts_with("bottom") || s == "b" || s == "b.cu" || s == "back" {
            Some(Side::Bottom)
        } else {
            None
        }
    }
}

/// The units a file states its coordinates in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Units {
    Millimetres,
    Inches,
    Mils,
}

impl Units {
    fn to_mm(self, v: f64) -> f64 {
        match self {
            Units::Millimetres => v,
            Units::Inches => v * 25.4,
            Units::Mils => v * 0.0254,
        }
    }

    fn parse(s: &str) -> Option<Units> {
        let s = s.trim().to_ascii_lowercase();
        match s.as_str() {
            "mm" | "millimeter" | "millimetre" | "millimeters" | "millimetres" | "metric" => {
                Some(Units::Millimetres)
            }
            "in" | "inch" | "inches" | "imperial" => Some(Units::Inches),
            "mil" | "mils" | "thou" => Some(Units::Mils),
            _ => None,
        }
    }
}

// ── One placement ───────────────────────────────────────────────────────────

/// Where one part was placed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Placement {
    pub reference: String,
    /// The value / comment column, empty when the file has none.
    pub value: String,
    /// The footprint / package column, empty when the file has none.
    pub package: String,
    /// Position in millimetres, converted from whatever the file stated.
    pub x_mm: f64,
    pub y_mm: f64,
    pub rotation_deg: f64,
    pub side: Side,
    /// One-based source line.
    pub line: usize,
}

/// What a placement read contributed, in the same shape as
/// [`crate::bom::BomProvenance`] so the two are absorbed by one evidence spine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlacementProvenance {
    pub path: String,
    pub kind: String,
    pub sha256: String,
    pub units: Units,
    pub rows: usize,
    pub contributed: Vec<Contribution>,
    pub ignored: Vec<IgnoredInput>,
}

/// A read pick-and-place file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlacementFile {
    pub dialect: PlacementDialect,
    pub placements: Vec<Placement>,
    pub provenance: PlacementProvenance,
}

impl PlacementFile {
    /// Read a placement file from a path. The dialect comes from the content:
    /// KiCad writes csv into a file called `.pos`, and Altium writes fixed-width
    /// text into a file called `.csv`, so the extension decides nothing.
    pub fn read(path: &Path) -> Result<Self, PlacementError> {
        let name = path.display().to_string();
        let bytes = std::fs::read(path).map_err(|e| PlacementError::Io {
            name: name.clone(),
            detail: e.to_string(),
        })?;
        Self::from_bytes(&bytes, &name)
    }

    /// Read from bytes, hashing them for provenance.
    pub fn from_bytes(bytes: &[u8], name: &str) -> Result<Self, PlacementError> {
        let text = decode(bytes);
        Self::parse(&text, name, &sha256_hex(bytes))
    }

    /// Read from text already in memory, with no hash.
    pub fn from_text(text: &str, name: &str) -> Result<Self, PlacementError> {
        Self::parse(text, name, "")
    }

    /// Does this file look like a placement file? Cheap enough to run over every
    /// file beside a board.
    pub fn detects(bytes: &[u8]) -> bool {
        let text = decode(bytes);
        Self::parse(&text, "", "").is_ok()
    }

    /// The placement of one reference designator, if the file has it.
    pub fn get(&self, reference: &str) -> Option<&Placement> {
        self.placements.iter().find(|p| p.reference == reference)
    }

    /// Every reference designator the file names, sorted.
    pub fn references(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .placements
            .iter()
            .map(|p| p.reference.clone())
            .collect();
        out.sort();
        out.dedup();
        out
    }

    /// Identity hints for the binder.
    ///
    /// A placement file's `Val` column is a VALUE, the same kind of claim the
    /// layout's own value field makes, so [`IdentityHint::mpn`] is always `None`
    /// here. That is the point: a placement file can fill a hole the layout left
    /// (an Altium layout has no values at all) but it must never outrank the
    /// layout, because it is not a more specific kind of statement.
    pub fn identity_hints(&self) -> Vec<IdentityHint> {
        self.placements
            .iter()
            .filter(|p| !p.value.is_empty())
            .map(|p| IdentityHint {
                reference: p.reference.clone(),
                value: Some(p.value.clone()),
                mpn: None,
                populate: None,
                source: self.provenance.path.clone(),
                source_kind: self.provenance.kind.to_string(),
            })
            .collect()
    }

    /// Reconcile the file against the board it claims to describe.
    ///
    /// Two whole-file conventions are resolved before any part is judged, because
    /// both are properties of the export and neither is a per-part fact.
    ///
    /// **The Y sign.** KiCad's exporters write Y negated relative to the board
    /// file's own frame. Deciding that per part, by taking whichever sign is
    /// closer, means a file that mirrors HALF the board matches everywhere and
    /// reports nothing. So the sign is chosen once, by which choice fits the whole
    /// file better, and then every part is judged under it.
    ///
    /// **The origin.** A position file exported against the drill/place origin is
    /// the same board shifted by one constant vector, which is ordinary practice
    /// and not a revision difference. So a constant translation is measured over
    /// the matched parts and removed before residuals are judged, and reported in
    /// [`PlacementCrossCheck::origin_offset_mm`] so the shift is stated rather than
    /// swallowed. A genuinely different revision is not explained by one constant
    /// vector, so its parts still disagree.
    pub fn cross_check(&self, board: &ExtractedBoard) -> PlacementCrossCheck {
        let mut check = PlacementCrossCheck::default();
        let (y_sign, offset) = self.frame_against(board);
        check.y_mirrored = y_sign < 0.0;
        if offset.0.abs() > POSITION_TOLERANCE_MM || offset.1.abs() > POSITION_TOLERANCE_MM {
            check.origin_offset_mm = Some(offset);
        }
        for p in &self.placements {
            let Some(comp) = board.component(&p.reference) else {
                check.only_in_placement.push(p.reference.clone());
                continue;
            };
            check.matched += 1;
            if let Some((x, y, _rot)) = comp.position {
                let dx = (x - (p.x_mm + offset.0)).abs();
                let dy = (y - (y_sign * p.y_mm + offset.1)).abs();
                if dx > POSITION_TOLERANCE_MM || dy > POSITION_TOLERANCE_MM {
                    check.position_disagreements.push(PositionDisagreement {
                        reference: p.reference.clone(),
                        board_mm: (x, y),
                        placement_mm: (p.x_mm, p.y_mm),
                    });
                }
            }
            if !comp.layer.is_empty() && comp.layer != p.side.kicad_layer() {
                check.side_disagreements.push(SideDisagreement {
                    reference: p.reference.clone(),
                    board_layer: comp.layer.clone(),
                    placement_side: p.side,
                });
            }
        }
        let placed: Vec<&str> = self
            .placements
            .iter()
            .map(|p| p.reference.as_str())
            .collect();
        check.only_on_board = board
            .components
            .iter()
            .filter(|c| !c.reference.is_empty() && !placed.contains(&c.reference.as_str()))
            .map(|c| c.reference.clone())
            .collect();
        check.only_in_placement.sort();
        check.only_on_board.sort();
        check
    }

    /// The Y sign and the constant translation that best carry this file's
    /// coordinate frame onto the board's, as `(y_sign, (dx, dy))` in millimetres.
    ///
    /// The offset is the MEDIAN of the per-part differences, not the mean: a
    /// median is unmoved by a handful of parts that genuinely did move between
    /// revisions, which is exactly the signal that must survive to be reported.
    /// It is only applied from three matched parts up, because one or two parts
    /// are always explained perfectly by a translation through them, and a check
    /// that cannot fail is not a check.
    fn frame_against(&self, board: &ExtractedBoard) -> (f64, (f64, f64)) {
        let pairs: Vec<((f64, f64), (f64, f64))> = self
            .placements
            .iter()
            .filter_map(|p| {
                let (x, y, _) = board.component(&p.reference)?.position?;
                Some(((x, y), (p.x_mm, p.y_mm)))
            })
            .collect();
        if pairs.len() < 3 {
            return (1.0, (0.0, 0.0));
        }
        let median = |mut v: Vec<f64>| -> f64 {
            v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            v[v.len() / 2]
        };
        let dx = median(pairs.iter().map(|((x, _), (px, _))| x - px).collect());
        let mut best = (1.0, (dx, 0.0), f64::INFINITY);
        for sign in [1.0f64, -1.0] {
            let dy = median(
                pairs
                    .iter()
                    .map(|((_, y), (_, py))| y - sign * py)
                    .collect(),
            );
            // Score by how many parts the frame explains, not by total error: one
            // part that really moved must not outvote the frame that fits the rest.
            let explained = pairs
                .iter()
                .filter(|((x, y), (px, py))| {
                    (x - (px + dx)).abs() <= POSITION_TOLERANCE_MM
                        && (y - (sign * py + dy)).abs() <= POSITION_TOLERANCE_MM
                })
                .count();
            let score = -(explained as f64);
            if score < best.2 {
                best = (sign, (dx, dy), score);
            }
        }
        (best.0, best.1)
    }

    fn parse(text: &str, name: &str, sha: &str) -> Result<Self, PlacementError> {
        if text.trim().is_empty() {
            return Err(PlacementError::Empty {
                name: name.to_string(),
            });
        }
        let lines: Vec<&str> = text.lines().collect();
        let units = stated_units(&lines, name)?;

        let (dialect, header_line, layout) =
            find_header(&lines).ok_or_else(|| PlacementError::NotAPlacementFile {
                name: name.to_string(),
            })?;

        let cols = &layout.roles;
        let mut placements = Vec::new();
        let mut bad_coords: Vec<(usize, String)> = Vec::new();
        let mut no_designator = 0usize;
        let mut no_side = 0usize;
        let mut rows = 0usize;

        for (n, line) in lines.iter().enumerate().skip(header_line + 1) {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let cells = layout.split(line);
            let cell = |k: Option<usize>| -> &str {
                k.and_then(|i| cells.get(i))
                    .map(|s| s.trim())
                    .unwrap_or("")
                    .trim_matches('"')
                    .trim()
            };
            let reference = cell(cols.reference).to_string();
            if reference.is_empty() {
                no_designator += 1;
                continue;
            }
            rows += 1;
            if reference.contains('*') || reference.contains('?') {
                no_designator += 1;
                continue;
            }
            let xs = cell(cols.x);
            let ys = cell(cols.y);
            let (Some(x), Some(y)) = (parse_coord(xs), parse_coord(ys)) else {
                bad_coords.push((
                    n + 1,
                    if parse_coord(xs).is_none() {
                        xs.to_string()
                    } else {
                        ys.to_string()
                    },
                ));
                continue;
            };
            let side = match Side::parse(cell(cols.side)) {
                Some(s) => s,
                None => {
                    no_side += 1;
                    Side::Top
                }
            };
            placements.push(Placement {
                reference,
                value: cell(cols.value).to_string(),
                package: cell(cols.package).to_string(),
                x_mm: units.to_mm(x),
                y_mm: units.to_mm(y),
                rotation_deg: parse_coord(cell(cols.rotation)).unwrap_or(0.0),
                side,
                line: n + 1,
            });
        }

        // A handful of unreadable coordinates is a broken export rather than a
        // dialect hauksbee does not know, and a file whose positions are mostly
        // unreadable cannot be used for anything. Refuse rather than report a
        // board assembled somewhere else.
        if !bad_coords.is_empty() && bad_coords.len() * 4 > rows {
            let (line, cell) = bad_coords[0].clone();
            return Err(PlacementError::UnreadableCoordinates {
                name: name.to_string(),
                bad: bad_coords.len(),
                total: rows,
                line,
                cell,
            });
        }
        if placements.is_empty() {
            return Err(if rows == 0 && no_designator == 0 {
                PlacementError::NoPlacements {
                    name: name.to_string(),
                    dialect: dialect.describe(),
                }
            } else {
                PlacementError::NoDesignators {
                    name: name.to_string(),
                    rows: rows.max(no_designator),
                }
            });
        }

        let sides = placements.iter().filter(|p| p.side == Side::Bottom).count();
        let mut contributed = vec![Contribution {
            what: "assembly placement".to_string(),
            detail: format!(
                "{} parts placed, {} of them on the bottom side",
                placements.len(),
                sides
            ),
        }];
        if placements.iter().any(|p| !p.value.is_empty()) {
            contributed.push(Contribution {
                what: "value strings".to_string(),
                detail: format!(
                    "{} placements carry a value the layout may not",
                    placements.iter().filter(|p| !p.value.is_empty()).count()
                ),
            });
        }

        let mut ignored = Vec::new();
        if no_designator > 0 {
            ignored.push(IgnoredInput {
                what: format!("{no_designator} rows with no usable designator"),
                why: "a logo, fiducial or unannotated placement names no part, so it \
                      cannot be matched to the board"
                    .to_string(),
            });
        }
        if no_side > 0 {
            ignored.push(IgnoredInput {
                what: format!("{no_side} rows with no readable board side"),
                why: "read as top, which is what every writer surveyed defaults to".to_string(),
            });
        }
        if !bad_coords.is_empty() {
            ignored.push(IgnoredInput {
                what: format!("{} rows with unreadable coordinates", bad_coords.len()),
                why: format!(
                    "the first is line {}, {:?}",
                    bad_coords[0].0, bad_coords[0].1
                ),
            });
        }

        Ok(PlacementFile {
            dialect,
            placements,
            provenance: PlacementProvenance {
                path: name.to_string(),
                kind: dialect.kind().to_string(),
                sha256: sha.to_string(),
                units,
                rows,
                contributed,
                ignored,
            },
        })
    }
}

// ── Cross-check against the board ───────────────────────────────────────────

/// One part the placement file and the layout put in different places.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PositionDisagreement {
    pub reference: String,
    pub board_mm: (f64, f64),
    pub placement_mm: (f64, f64),
}

/// One part the placement file and the layout put on different sides.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SideDisagreement {
    pub reference: String,
    pub board_layer: String,
    pub placement_side: Side,
}

/// What a placement file and the board it claims to describe agree and disagree
/// about.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PlacementCrossCheck {
    /// Designators the file places that the board does not have.
    pub only_in_placement: Vec<String>,
    /// Designators on the board the file does not place. Ordinary: a
    /// through-hole part, a part excluded from assembly, an artwork placeholder.
    pub only_on_board: Vec<String>,
    pub position_disagreements: Vec<PositionDisagreement>,
    pub side_disagreements: Vec<SideDisagreement>,
    /// Designators found on both sides.
    pub matched: usize,
    /// The constant translation from the file's frame to the board's, when the two
    /// use different origins. Reported rather than swallowed: a reader comparing a
    /// coordinate by hand needs to know the frames differ.
    pub origin_offset_mm: Option<(f64, f64)>,
    /// True when the file's Y axis runs the opposite way to the board's, which is
    /// what KiCad's own exporters do.
    pub y_mirrored: bool,
}

impl PlacementCrossCheck {
    /// True when this file describes a DIFFERENT board rather than a partly
    /// assembled one.
    ///
    /// The distinction is the whole value of the check. A placement file missing
    /// half the board's parts is ordinary (only the SMD side gets placed). A
    /// placement file that places parts the board does not have, or that puts
    /// the parts it does share somewhere else, is from another revision, and
    /// every conclusion drawn from the pair would describe a board that does not
    /// exist. The threshold is a majority of the parts the two share, because a
    /// single moved part is a change to report and a wholesale disagreement is a
    /// different board.
    pub fn is_different_board(&self) -> bool {
        if self.matched == 0 {
            return !self.only_in_placement.is_empty();
        }
        self.position_disagreements.len() * 2 > self.matched
            || self.only_in_placement.len() > self.matched
    }

    /// The lines a report prints. Empty when the two agree about everything,
    /// so a board whose placement file matches never mentions the subject.
    pub fn lines(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some((dx, dy)) = self.origin_offset_mm {
            out.push(format!(
                "  the placement file's origin is offset from the board's by \
                 ({dx:.4}, {dy:.4}) mm; positions are compared with that removed"
            ));
        }
        if !self.only_in_placement.is_empty() {
            out.push(format!(
                "  placed but not on the board: {}",
                self.only_in_placement.join(", ")
            ));
        }
        for d in &self.position_disagreements {
            out.push(format!(
                "  {} sits at ({:.4}, {:.4}) mm on the board and ({:.4}, {:.4}) mm in the \
                 placement file",
                d.reference, d.board_mm.0, d.board_mm.1, d.placement_mm.0, d.placement_mm.1
            ));
        }
        for d in &self.side_disagreements {
            out.push(format!(
                "  {} is on {} in the board and the {} side in the placement file",
                d.reference,
                d.board_layer,
                d.placement_side.describe()
            ));
        }
        out
    }
}

// ── Header detection ────────────────────────────────────────────────────────

/// Which column index holds which role.
#[derive(Debug, Clone, Default)]
struct Roles {
    reference: Option<usize>,
    value: Option<usize>,
    package: Option<usize>,
    x: Option<usize>,
    y: Option<usize>,
    rotation: Option<usize>,
    side: Option<usize>,
}

/// How to split the data rows, and where each role sits.
#[derive(Debug, Clone)]
struct Layout {
    roles: Roles,
    /// `None` for a fixed-width file, whose columns are sliced by offsets.
    delimiter: Option<char>,
    starts: Vec<usize>,
}

impl Layout {
    fn split(&self, line: &str) -> Vec<String> {
        match self.delimiter {
            Some(d) => split_delimited(line, d),
            None => slice_fixed_width(line, &self.starts),
        }
    }
}

/// The units the file states. A KiCad `.pos` says `## Unit = mm`; an Altium
/// export says `Units used: mm`. A file that states nothing is millimetres,
/// which is what every generic CPL writer emits and what both assembly houses
/// require.
fn stated_units(lines: &[&str], name: &str) -> Result<Units, PlacementError> {
    for line in lines.iter().take(MAX_BANNER_LINES) {
        let l = line.trim().trim_start_matches('#').trim();
        let claim = l
            .strip_prefix("Unit =")
            .or_else(|| l.strip_prefix("Units ="))
            .or_else(|| l.strip_prefix("Units used:"))
            .or_else(|| l.strip_prefix("Unit:"))
            .or_else(|| l.strip_prefix("Units:"));
        let Some(claim) = claim else { continue };
        // `## Unit = mm, Angle = deg.`: take the token before the comma, and
        // drop the trailing full stop KiCad writes.
        let token = claim
            .split(',')
            .next()
            .unwrap_or("")
            .trim()
            .trim_end_matches('.')
            .trim_matches(|c: char| c == ',' || c.is_whitespace());
        if token.is_empty() {
            continue;
        }
        return Units::parse(token).ok_or_else(|| PlacementError::UnknownUnits {
            name: name.to_string(),
            units: token.to_string(),
        });
    }
    Ok(Units::Millimetres)
}

/// Find the header row, decide the dialect, and work out how to split the rows.
fn find_header(lines: &[&str]) -> Option<(PlacementDialect, usize, Layout)> {
    for (i, line) in lines.iter().take(MAX_BANNER_LINES).enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        // Data rows only: a banner line is dense text that would blank out every
        // fixed-width column boundary.
        let data: Vec<&str> = lines
            .iter()
            .skip(i + 1)
            .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
            .copied()
            .collect();
        // A KiCad ascii header is a comment line, so it is tested first and
        // separately: `# Ref Val Package PosX PosY Rot Side`.
        let bare = line.trim_start().trim_start_matches('#').trim();
        if line.trim_start().starts_with('#') {
            let (names, starts) = fixed_width_columns(line, &data);
            if let Some(roles) = roles_from(&names) {
                if roles.x.is_some() && roles.y.is_some() {
                    // A KiCad 4-era file writes the same header comma-separated.
                    if bare.contains(',') {
                        let cells = split_delimited(bare, ',');
                        if let Some(roles) = roles_from(&cells) {
                            return Some((
                                PlacementDialect::KicadPosCsv,
                                i,
                                Layout {
                                    roles,
                                    delimiter: Some(','),
                                    starts: Vec::new(),
                                },
                            ));
                        }
                    }
                    return Some((
                        PlacementDialect::KicadPosAscii,
                        i,
                        Layout {
                            roles,
                            delimiter: None,
                            starts,
                        },
                    ));
                }
            }
            continue;
        }

        // Delimited.
        let delim = sniff_delimiter(line);
        let cells = split_delimited(line, delim);
        if cells.len() >= 3 {
            if let Some(roles) = roles_from(&cells) {
                if roles.x.is_some() && roles.y.is_some() && roles.reference.is_some() {
                    let keys: Vec<String> = cells.iter().map(|c| normalise_header(c)).collect();
                    let dialect = dialect_from(&keys);
                    return Some((
                        dialect,
                        i,
                        Layout {
                            roles,
                            delimiter: Some(delim),
                            starts: Vec::new(),
                        },
                    ));
                }
            }
        }

        // Fixed-width, which is Altium's `.txt` form.
        let (names, starts) = fixed_width_columns(line, &data);
        if names.len() >= 3 {
            if let Some(roles) = roles_from(&names) {
                if roles.x.is_some() && roles.y.is_some() && roles.reference.is_some() {
                    let keys: Vec<String> = names.iter().map(|c| normalise_header(c)).collect();
                    return Some((
                        dialect_from(&keys),
                        i,
                        Layout {
                            roles,
                            delimiter: None,
                            starts,
                        },
                    ));
                }
            }
        }
    }
    None
}

/// Which dialect a delimited placement header belongs to.
fn dialect_from(keys: &[String]) -> PlacementDialect {
    let has = |k: &str| keys.iter().any(|x| x == k);
    if has("center_x_mm") || has("center_y_mm") {
        PlacementDialect::AltiumPickPlace
    } else if has("posx") {
        PlacementDialect::KicadPosCsv
    } else {
        PlacementDialect::GenericCpl
    }
}

/// Map a placement header row to roles. `None` when nothing in it is a
/// coordinate, which is how a BOM handed to this reader is refused rather than
/// read as a placement file with every position at the origin.
fn roles_from(cells: &[String]) -> Option<Roles> {
    let mut r = Roles::default();
    for (i, cell) in cells.iter().enumerate() {
        let key = normalise_header(cell);
        let slot = match key.as_str() {
            "ref"
            | "refs"
            | "reference"
            | "designator"
            | "designators"
            | "refdes"
            | "reference_designator" => &mut r.reference,
            "val" | "value" | "comment" => &mut r.value,
            "package" | "footprint" => &mut r.package,
            "posx" | "midx" | "mid_x" | "center_x_mm" | "centerx" | "x" | "ref_x" | "x_mm" => {
                &mut r.x
            }
            "posy" | "midy" | "mid_y" | "center_y_mm" | "centery" | "y" | "ref_y" | "y_mm" => {
                &mut r.y
            }
            "rot" | "rotation" | "angle" => &mut r.rotation,
            "side" | "layer" | "tb" => &mut r.side,
            _ => continue,
        };
        // First column to claim a role keeps it: Altium writes both
        // `Center-X(mm)` and `Ref X`, and the centre is the placement.
        if slot.is_none() {
            *slot = Some(i);
        }
    }
    (r.x.is_some() && r.y.is_some()).then_some(r)
}

/// Read one coordinate cell. Altium writes the unit into the cell
/// (`"43.9420mm"`), and a rotation is sometimes written with a trailing degree
/// sign, so a numeric prefix is taken and the rest discarded.
fn parse_coord(s: &str) -> Option<f64> {
    let s = s.trim().trim_matches('"').trim();
    if s.is_empty() {
        return None;
    }
    let end = s
        .char_indices()
        .take_while(|(i, c)| {
            c.is_ascii_digit()
                || *c == '.'
                || ((*c == '-' || *c == '+') && *i == 0)
                || *c == 'e'
                || *c == 'E'
        })
        .map(|(i, c)| i + c.len_utf8())
        .last()?;
    s[..end].parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_coordinate_with_a_unit_suffix_still_parses() {
        assert_eq!(parse_coord("43.9420mm"), Some(43.942));
        assert_eq!(parse_coord("-13.05"), Some(-13.05));
        assert_eq!(parse_coord("\"90\""), Some(90.0));
        assert_eq!(parse_coord(""), None);
        assert_eq!(parse_coord("n/a"), None);
    }

    #[test]
    fn every_spelling_of_a_board_side_reads() {
        assert_eq!(Side::parse("top"), Some(Side::Top));
        assert_eq!(Side::parse("TOP"), Some(Side::Top));
        assert_eq!(Side::parse("TopLayer"), Some(Side::Top));
        assert_eq!(Side::parse("BottomSolder"), Some(Side::Bottom));
        assert_eq!(Side::parse("B.Cu"), Some(Side::Bottom));
        assert_eq!(Side::parse("middle"), None);
    }

    #[test]
    fn stated_units_are_honoured_and_an_unknown_one_refuses() {
        let inches = ["## Unit = in, Angle = deg."];
        assert_eq!(stated_units(&inches, "f").unwrap(), Units::Inches);
        let altium = ["Units used: mm"];
        assert_eq!(stated_units(&altium, "f").unwrap(), Units::Millimetres);
        let odd = ["## Unit = furlongs, Angle = deg."];
        let err = stated_units(&odd, "f.pos").unwrap_err();
        assert_eq!(err.exit_code(), 3);
        assert!(err.to_string().contains("furlongs"), "{err}");
    }

    #[test]
    fn an_inch_position_file_converts_to_millimetres() {
        let text = "### Footprint positions ###\n## Unit = in, Angle = deg.\n\
                    # Ref     Val       Package        PosX       PosY       Rot  Side\n\
                    R1        10k       R_0402        1.0000     2.0000     0.0000  top\n\
                    ## End\n";
        let file = PlacementFile::from_text(text, "in.pos").unwrap();
        assert_eq!(file.provenance.units, Units::Inches);
        let r1 = file.get("R1").unwrap();
        assert!((r1.x_mm - 25.4).abs() < 1e-9, "{}", r1.x_mm);
        assert!((r1.y_mm - 50.8).abs() < 1e-9, "{}", r1.y_mm);
    }
}
