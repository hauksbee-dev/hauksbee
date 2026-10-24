//! Circuit extraction: turn a KiCad design into the connectivity graph the
//! simulator binds models onto.
//!
//! Several sources, one output shape:
//! - [`ExtractedBoard::from_kicad_pcb`], layout only. Every pad in a
//!   `.kicad_pcb` carries its net, so the board file alone fully describes
//!   connectivity. This is the "hand us any PCB" path.
//! - [`ExtractedBoard::from_kicad_netlist`], a `kicad-cli sch export
//!   netlist --format kicadsexpr` export. Richer (pin names/types), used
//!   when the schematic is available.
//! - [`ExtractedBoard::from_kicad_schematic`] /
//!   [`ExtractedBoard::from_kicad_schematic_path`], a `.kicad_sch` directly.
//!   The schematic carries no nets, so connectivity is *derived* geometrically
//!   the way eeschema derives it (wires, pins, junctions, labels, power
//!   symbols, hierarchy). This is the "simulate before there's a layout" path;
//!   see `docs/ingest/SCHEMATICS.md`.
//! - [`ExtractedBoard::from_eagle_brd`] / [`ExtractedBoard::from_ipc_d356`],
//!   other EDA formats.
//! - [`ExtractedBoard::from_altium_pcb`], Altium Designer `.PcbDoc` (binary
//!   OLE2). This unlocks the professional / enterprise / regulated tier; see
//!   `docs/ingest/ALTIUM.md`.
//! - [`ExtractedBoard::from_odbpp`] / [`ExtractedBoard::from_ipc2581`], the two
//!   fab/assembly *exchange* formats. Both state the netlist rather than needing
//!   it reverse-engineered from copper, so a board that only ever leaves its CAD
//!   tool as an ODB++ `.tgz` or an IPC-2581 XML is fully ingestible. See
//!   [`odbpp`] and [`ipc2581`] for what each carries and what is dropped.
//! - [`ExtractedBoard::from_easyeda`], an EasyEDA / JLCEDA board: a Standard
//!   edition `.json` PCB document or a Pro edition `.epcb` / `.epro`. See
//!   [`easyeda`] for what it reads and what it names as unsupported.
//!
//! Long-form how-and-why: docs/how-and-why/hauksbee-extract/README.md (the
//! crate tour) and docs/how-and-why/hauksbee-extract/netlist.md (the
//! canonical form defined here).

pub mod altium;
pub mod assembly;
pub mod bom;
pub mod datasheet_identity;
pub mod dnp;
pub mod drc;
mod drc_rules;
mod dsu;
mod eagle;
pub mod easyeda;
/// The pre-Eagle-6 BINARY drawing detector and its refusal message. Exported
/// because the board-input normalizer has to refuse this format on the RAW
/// bytes, ahead of reader selection: the lossy UTF-8 decode every text reader
/// works from destroys the header, and one of those readers will then claim the
/// file and report invented parts from it.
pub use eagle::{eagle_binary_message, looks_like_eagle_binary};
pub use odbpp::tree::MAX_INFLATED_BYTES;
/// Declared net ties from a companion Eagle `.sch`.
///
/// Not a schematic extractor: the `.brd` already carries the netlist, and this answers only whether the
/// designer DECLARED two named nets joined on purpose, which the `.brd` cannot
/// say and which decides whether a copper contact between them is a defect.
pub mod eagle_sch;
pub mod gerber;
pub mod ipc2581;
mod ipc356;
mod kelvin_sense;
pub mod kicad_dru;
mod layout_lint;
pub mod mate;
pub use layout_lint::{NOT_ROUTED_FIX, NOT_ROUTED_MESSAGE_PREFIX, PANEL_COPIES_MESSAGE_PREFIX};
mod netlint;
mod netlist;
pub mod netname;
pub mod odbpp;
mod part_class;
pub use part_class::{is_active_ic, is_plain_inductor, is_plain_resistor, is_plugin_module};
mod pcb;
pub mod placement;
mod protel_ascii;
pub mod reader;
pub mod repeat;
pub mod resource_conflict;
mod schematic;
/// Whether a parsed `.kicad_sch` is the ROOT of its hierarchy rather than a
/// sub-sheet. Exported because the distinction decides whether a file can be
/// extracted at all (a sub-sheet is refused, since its connectivity is only
/// complete when read through its root), so a caller or a test needs to be able
/// to ask the same question the reader asks.
pub use schematic::is_root_sheet as schematic_is_root;
pub mod si;
pub mod trace_current;

pub use drc::{
    clearance_rules_from_kicad_pro, drc_from_text, eagle_drc_from_text, is_touching, run_drc,
    run_drc_with_clearance_rules, ClearanceRules, DeclaredTie, DrcFinding, DrcReport,
    DrcTieQualification, Item, ItemKind, NetClassRule, UnjudgedClearance, ViolationKind,
    DEFAULT_CLEARANCE_MM, SHORT_TOUCH_EPS_MM,
};
pub use drc_rules::CustomClearanceRule;
pub use eagle_sch::{
    declared_net_ties, looks_like_eagle_schematic, schematic_pad_names, schematic_part_identities,
    schematic_pin_net_identities, DeclaredNetTie, SchematicPartIdentity, SchematicPinNetIdentity,
};
pub use kicad_dru::{
    parse_kicad_dru, KicadDruConstraint, KicadDruConstraintKind, KicadDruError, KicadDruRule,
    KicadDruRules, KicadDruSkippedRule,
};
pub use netlint::{
    control_role, i2c_pullup_finding_for_net, i2c_role, is_capacitor, is_connector_like, is_ground,
    is_led, is_led_identity, is_logic_or_switch_ref, is_placeholder_value, is_resistor,
    is_test_point_or_hole, is_unconnected_net, model_card_file_in, norm, pin_is_no_connect,
    render_netlint, uart_pin_role, I2cMode, LintCheck, LintFinding, NetLintReport, Severity,
    ABSTENTION_MARKER, CHECK_BEFORE_ORDERING, DNP_FITTED_NOTE_MARKER, DNP_PULLUP_MESSAGE_MARKER,
    INFORMATIONAL_MARKER, MODEL_CARD_MESSAGE_MARKER, REMEDY_MARKER, SPI_PULLUP_MESSAGE_MARKER,
};
pub use si::{
    cl_board_pf, cl_series, i2c_rise_time_ns, render_si, routed_length_mm, SiCheck, SiFinding,
    SiReport, SiSeverity,
};
pub use trace_current::{
    audit_trace_currents, ipc2221_ampacity, ipc2221_min_width_mm, net_copper_from_root,
    net_copper_from_text, render_trace_capacity_report, render_trace_capacity_report_with_context,
    trace_capacity_report, CopperKind, CopperSource, CopperWeights, NetCopper, TraceAudit,
    TraceCapacityRow, TraceCurrentFinding,
};

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum ExtractError {
    // The inner ParseError already says "parse error at line N"; a "parse:"
    // template here rendered the doubled "parse: parse error" prefix.
    #[error("{0}")]
    Parse(#[from] forge_sexpr::ParseError),
    #[error("xml: {0}")]
    Xml(String),
    /// The file parsed as its format but carries content that cannot be
    /// analysed truthfully, so continuing would produce a confident wrong
    /// answer rather than an error (a coordinate that is not a finite number is
    /// the case this exists for). Already phrased as a whole human sentence,
    /// including what to do next.
    #[error("{0}")]
    Corrupt(String),
    #[error("altium: {0}")]
    Altium(String),
    /// An ODB++ job input problem, already phrased as a whole human sentence.
    #[error("{0}")]
    Odb(String),
    /// An IPC-2581 document problem, already phrased as a whole human sentence.
    #[error("{0}")]
    Ipc2581(String),
    /// An EasyEDA (JLCEDA) board problem, already phrased as a whole human
    /// sentence that names what the file is and what to do next.
    #[error("{0}")]
    Easyeda(String),
    #[error("{}", wrong_root_text(.expected, .found.as_deref()))]
    WrongRoot {
        expected: &'static str,
        found: Option<String>,
    },
    /// A gerber job input problem, already phrased as a whole human sentence.
    #[error("{0}")]
    Gerber(String),
    /// No registered [`reader::BoardReader`] recognised the input. The message
    /// is built by [`reader::unrecognized_message`]: it special-cases the
    /// common look-alikes (empty file, Git LFS pointer, ASCII Protel exports)
    /// and otherwise lists the accepted formats in user words.
    #[error("{message}")]
    Unrecognized { message: String },
    /// A caller-supplied reference designator does not exist on the board.
    #[error("{0}")]
    UnknownReference(String),
    /// A hierarchical sub-sheet was handed over with no parent to be found.
    ///
    /// A sub-sheet's hierarchical labels connect to sheet pins in its parent, so
    /// on its own it reports nets that touch one pin and look floating when they
    /// are driven from a sibling sheet. Refusing is the honest answer: a netlist
    /// derived from a fragment would be read as a fact about the board.
    #[error("{sheet} is a hierarchical sub-sheet, not a complete design. It needs {needs}")]
    OrphanSubSheet { sheet: String, needs: String },
}

/// How the coverage note for an unread sub-sheet file opens.
///
/// The reader writes it ([`unread_sheet_note`]) and the evidence layer
/// recognises it ([`split_unread_sheet_note`]) from this one definition, so the
/// note travels the reader-notes channel and arrives at the verdict as
/// unchecked scope.
pub const UNREAD_SHEET_NOTE_PREFIX: &str = "Sub-sheet not read: ";

/// The coverage note for a sub-sheet file the reader had to leave out.
///
/// `subject` is the file and the sheet that references it; `detail` is the
/// whole-sentence account of what is wrong and how to fix it.
#[must_use]
pub fn unread_sheet_note(subject: &str, detail: &str) -> String {
    format!("{UNREAD_SHEET_NOTE_PREFIX}{subject}: {detail}")
}

/// The `(subject, detail)` of a note written by [`unread_sheet_note`], or
/// `None` for any other note.
#[must_use]
pub fn split_unread_sheet_note(note: &str) -> Option<(&str, &str)> {
    note.strip_prefix(UNREAD_SHEET_NOTE_PREFIX)?
        .split_once(": ")
        .filter(|(subject, detail)| !subject.is_empty() && !detail.is_empty())
}

/// The sentence for a file whose top-level section is not the expected one.
/// `None` means the parser found no top-level section at all (an empty or
/// whitespace-only file), which is said in those words, not as a Rust `None`.
fn wrong_root_text(expected: &str, found: Option<&str>) -> String {
    found.map_or_else(
        || {
            format!(
                "this file is empty or is not a {expected} file: it has no top-level \
                 ({expected} ...) section. Re-export or re-save it from your design \
                 tool, or check that you named the right file"
            )
        },
        |found| {
            format!(
                "not a {expected} file: it starts with ({found} ...) where \
                 ({expected} ...) was expected. Check that you named the right file"
            )
        },
    )
}

/// Refuse a file still carrying Git merge-conflict markers.
///
/// The s-expression formats swallow these without complaint: `<<<<<<<` and
/// `>>>>>>>` are legal bare atoms, so a conflicted board parses, both sides of
/// the conflict land in the netlist at once, and every number in the report is
/// computed over a board that never existed. The file is not a board yet, and
/// the fix is to finish the merge, so say exactly that.
///
/// The markers are matched at the start of a line with the exact seven-character
/// run Git writes, so a board with a `<<<` silkscreen label or a `=======`
/// divider in a comment is unaffected.
pub(crate) fn reject_merge_conflict(text: &str) -> Result<(), ExtractError> {
    for (i, line) in text.lines().enumerate() {
        let is_marker = line.starts_with("<<<<<<< ")
            || line.starts_with(">>>>>>> ")
            || line == "======="
            || line.starts_with("||||||| ");
        if is_marker {
            return Err(ExtractError::Corrupt(format!(
                "this file still has an unresolved Git merge conflict (marker \
                 '{}' on line {}). Both sides of the conflict are in the file, so \
                 anything read out of it would describe a board that does not exist. \
                 Resolve the conflict (`git checkout --theirs`/`--ours`, or open it \
                 in KiCad and re-save), then retry",
                line.chars().take(7).collect::<String>(),
                i + 1
            )));
        }
    }
    Ok(())
}

/// Split placements that carry no pad out of a component list.
///
/// A placement with no pad is board artwork, not a part: a silkscreen logo, a
/// mechanical outline, a display keep-out. The native KiCad reader drops these
/// for the same reason (a `Logo-` footprint binding as an inductor, and
/// bind-rate denominators padded with decoration), so the exchange readers must
/// drop them too or two readings of one board disagree on the part count.
/// Returns the surviving components (duplicate designators merged) and the
/// references that went, so the loss is reported rather than silent.
pub(crate) fn split_board_artwork(components: Vec<Component>) -> (Vec<Component>, Vec<String>) {
    let artwork: Vec<String> = components
        .iter()
        .filter(|c| c.pins.is_empty())
        .map(|c| c.reference.clone())
        .collect();
    let kept = merge_duplicate_references(
        components
            .into_iter()
            .filter(|c| !c.pins.is_empty())
            .collect(),
    );
    (kept, artwork)
}

/// Names of nets no component pin sits on. Computed from the FINISHED board, so
/// it catches a net lost to any resolution step as well as one the source really
/// declares unused.
pub(crate) fn nets_without_pads(nets: &[Net], components: &[Component]) -> Vec<String> {
    let attached: HashSet<i64> = components
        .iter()
        .flat_map(|c| c.pins.iter())
        .filter_map(|p| p.net)
        .collect();
    nets.iter()
        .filter(|n| !attached.contains(&n.id))
        .map(|n| n.name.clone())
        .collect()
}

/// The two disclosure sentences every exchange reader owes: which placements
/// were read as board artwork, and which declared nets touch no pad.
pub(crate) fn exchange_disclosures(
    artwork: &[String],
    nets_without_pads: &[String],
) -> Vec<String> {
    let mut out = Vec::new();
    if !artwork.is_empty() {
        out.push(format!(
            "{} placement(s) have no pad and were read as board artwork \
             rather than parts: {}.",
            artwork.len(),
            sample_list(artwork)
        ));
    }
    if !nets_without_pads.is_empty() {
        out.push(format!(
            "{} net(s) are declared but touch no component pad, so nothing \
             downstream can see them: {}.",
            nets_without_pads.len(),
            sample_list(nets_without_pads)
        ));
    }
    out
}

/// A comma list truncated to a sentence-sized sample, so a 600-net
/// disagreement stays readable rather than filling the report.
pub(crate) fn sample_list(items: &[String]) -> String {
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

/// One electrical net. `id` is the KiCad net number (0 = the unconnected
/// net in PCB files); `name` like "GND", "/`Debugger/nRF52_VDD`".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Net {
    pub id: i64,
    pub name: String,
}

impl Net {
    #[must_use]
    pub fn new(id: i64, name: impl Into<String>) -> Self {
        Self {
            id,
            name: name.into(),
        }
    }
}

/// A component pin/pad connection point.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Pin {
    /// Pad number / pin number as printed ("1", "A8", "EP").
    pub number: String,
    /// Net id this pin is on, if connected.
    pub net: Option<i64>,
    /// Pin name from the schematic ("VCC", "GPIO4"); empty for PCB-only.
    pub function: String,
    /// Electrical type from the schematic ("passive", "input", ...); empty
    /// for PCB-only extraction.
    pub kind: String,
    /// Absolute board position in mm, when extracted from a layout.
    pub position: Option<(f64, f64)>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Component {
    /// Reference designator ("R1", "U101").
    pub reference: String,
    /// Value field ("10k", "BCM857BS").
    pub value: String,
    /// Symbol or footprint library id ("Device:R",
    /// "`Resistor_SMD:R_0402_1005Metric`").
    pub lib_id: String,
    /// Footprint name when known.
    pub footprint: String,
    /// Board position (x mm, y mm, rotation degrees) when from a layout.
    pub position: Option<(f64, f64, f64)>,
    /// Board side ("F.Cu"/"B.Cu") when from a layout.
    pub layer: String,
    /// Extra properties (part number, datasheet, ...).
    pub properties: Vec<(String, String)>,
    /// True when the component is marked Do-Not-Populate / excluded from the BOM
    /// (KiCad `(dnp yes)` on a schematic symbol, or `(attr ... dnp)` on a PCB
    /// footprint). A DNP footprint is on the layout but not assembled, so it is
    /// electrically absent: checks that reason about populated parts (e.g. the
    /// USB-C CC termination audit) must skip these.
    #[serde(default)]
    pub dnp: bool,
    pub pins: Vec<Pin>,
}

impl Component {
    /// How many distinct pad numbers the footprint has.
    ///
    /// A footprint repeats a number for one electrical pad (an exposed pad's
    /// thermal vias and paste apertures, a TO-263 tab sharing the centre lead's
    /// number) and gives a mechanical pad no number at all; neither is another
    /// pin of the part. Anything comparing a footprint's pad count to a package's
    /// pin count or a rule's `pad_count` uses this, not `pins.len()`.
    #[must_use]
    pub fn distinct_pad_count(&self) -> usize {
        let numbers: std::collections::HashSet<&str> = (self.pins.iter())
            .map(|p| p.number.trim())
            .filter(|n| !n.is_empty())
            .collect();
        numbers.len()
    }

    /// Whether the input this part came from does not carry values at all, as
    /// opposed to the designer having left one unset.
    ///
    /// A reader marks a part with [`altium::VALUE_UNRESOLVED_KEY`] when its
    /// format has nowhere to hold the value (an Altium `.PcbDoc` keeps values in
    /// the `.SchDoc`, an EasyEDA Pro `.epcb` in the project's device library, an
    /// IPC-D-356 netlist has none). A part that is marked and still empty is not
    /// a placeholder the designer entered: telling them to "set the value" is
    /// wrong, and the fix is to supply a BOM or the schematic. A value that is
    /// present, even a placeholder such as `R` or `?`, is never "not carried".
    #[must_use]
    pub fn value_not_carried(&self) -> bool {
        self.value.trim().is_empty()
            && self
                .properties
                .iter()
                .any(|(key, _)| key == altium::VALUE_UNRESOLVED_KEY)
    }
}

/// The extraction result: everything the binder and renderer need to know
/// about what the board is, electrically.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractedBoard {
    pub name: String,
    pub nets: Vec<Net>,
    pub components: Vec<Component>,
}

impl ExtractedBoard {
    /// KiCad layout (`.kicad_pcb`).
    ///
    /// # Errors
    /// [`ExtractError::Parse`] when the text is not a well-formed s-expression, or
    /// another variant when it is not a readable KiCad board.
    pub fn from_kicad_pcb(text: &str) -> Result<Self, ExtractError> {
        pcb::extract(text)
    }

    /// KiCad netlist export (`.net`).
    ///
    /// # Errors
    /// [`ExtractError::Parse`] when the text is not a well-formed s-expression, or
    /// another variant when it is not a KiCad netlist.
    pub fn from_kicad_netlist(text: &str) -> Result<Self, ExtractError> {
        netlist::extract(text)
    }

    /// KiCad s-expression schematic (`.kicad_sch`, KiCad 6 through 10). The
    /// netlist is derived geometrically from wires, pins, junctions, labels
    /// and power symbols, since a schematic carries no copper. Sub-sheets are
    /// not followed (single sheet only); use [`Self::from_kicad_schematic_path`]
    /// to recurse a hierarchy that lives in sibling files.
    ///
    /// # Errors
    /// Fails when the text is not a well-formed KiCad schematic.
    pub fn from_kicad_schematic(text: &str) -> Result<Self, ExtractError> {
        schematic::extract(text)
    }

    /// KiCad schematic from a file path, recursing into the sheet hierarchy
    /// (sub-sheets resolved relative to the file's directory).
    ///
    /// # Errors
    /// Fails when the file cannot be read or is not a well-formed KiCad
    /// schematic. A sub-sheet that cannot be read is left out of the read
    /// (see [`Self::from_kicad_schematic_path_with_notes`] for the note).
    pub fn from_kicad_schematic_path(path: &Path) -> Result<Self, ExtractError> {
        schematic::extract_from_path(path)
    }

    /// [`Self::from_kicad_schematic_path`], plus the coverage notes the read
    /// owes the verdict: one sentence for each sub-sheet file that could not be
    /// read (missing, cut short, empty or not a schematic). The sheets that were
    /// read are still extracted, so a fault on them is still found; a caller
    /// that reports a verdict must carry the notes with it, because a design
    /// with an unread sheet is not an unconditionally clean one.
    ///
    /// # Errors
    /// As [`Self::from_kicad_schematic_path`] for the root and for hierarchy
    /// limits; an unreadable sub-sheet is a note, not an error.
    pub fn from_kicad_schematic_path_with_notes(
        path: &Path,
    ) -> Result<(Self, Vec<String>), ExtractError> {
        schematic::extract_from_path_with_notes(path)
    }

    /// Name the pads of layout components that carry no pin names (a scripted,
    /// converted or KiCad 5 `.kicad_pcb`, or any Eagle `.brd`) from the
    /// same-stem schematic `sch` (`.kicad_sch`, or an Eagle `.sch`), so the
    /// pin-function lints and MCU GPIO derivation can read them. Only a
    /// matching reference and pad number is copied, and a component whose
    /// layout names any of its pads is left untouched. An Eagle component with
    /// a blank value also takes its schematic library device name as value.
    pub fn adopt_schematic_pin_names(&mut self, sch: &Path) {
        let unnamed =
            |c: &Component| !c.pins.is_empty() && c.pins.iter().all(|p| p.function.is_empty());
        if !self.components.iter().any(unnamed) || !sch.is_file() {
            return;
        }
        // (reference, pad) -> (pin name, electrical kind)
        let mut names: std::collections::HashMap<(String, String), (String, String)> =
            std::collections::HashMap::new();
        if sch
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("sch"))
        {
            let Ok(text) = std::fs::read_to_string(sch) else {
                return;
            };
            let Ok(pads) = eagle_sch::schematic_pad_pins(&text) else {
                return;
            };
            // A blank layout value takes the library device the schematic placed,
            // when that names a part number (ATTINY85) rather than a class (R, C)
            // whose identity is the value a placement file may still supply.
            for (reference, name) in
                eagle_sch::schematic_blank_value_device_names(&text).unwrap_or_default()
            {
                if !name.chars().any(|c| c.is_ascii_digit()) {
                    continue;
                }
                let blank =
                    |c: &&mut Component| c.reference == reference && c.value.trim().is_empty();
                if let Some(c) = self.components.iter_mut().find(blank) {
                    c.value = name;
                }
            }
            // A part number the library device name embeds, for a value that is
            // a rating string (`20V/4.2A`): a candidate the binder may try.
            for (reference, number) in
                eagle_sch::schematic_device_part_numbers(&text).unwrap_or_default()
            {
                let key = bom::LIBRARY_DEVICE_PROPERTY;
                for c in (self.components.iter_mut()).filter(|c| c.reference == reference) {
                    if !c.properties.iter().any(|(k, _)| k == key) {
                        c.properties.push((key.to_string(), number.clone()));
                    }
                    // A family name the library device completes with its
                    // variant (`MIC5205` + `MIC5205-3.3V`): the device is the
                    // part, and the bare family would read as another variant.
                    let head = c.value.trim().len();
                    if head > 0
                        && number.len() > head + 1
                        && number.is_char_boundary(head)
                        && number[..head].eq_ignore_ascii_case(c.value.trim())
                        && number[head..].starts_with('-')
                    {
                        c.value.clone_from(&number);
                    }
                }
            }
            for (reference, pad, name, kind) in pads {
                names.insert((reference, pad), (name, kind));
            }
        } else {
            let Ok(sheet) = Self::from_kicad_schematic_path(sch) else {
                return;
            };
            for c in sheet.components {
                for q in c.pins.into_iter().filter(|q| q.function != "~") {
                    names.insert((c.reference.clone(), q.number), (q.function, q.kind));
                }
            }
        }
        for t in self.components.iter_mut().filter(|c| unnamed(c)) {
            for p in &mut t.pins {
                if let Some((function, kind)) = names.get(&(t.reference.clone(), p.number.clone()))
                {
                    p.function.clone_from(function);
                    p.kind.clone_from(kind);
                }
            }
        }
    }

    /// A part the sibling schematic `sch` marks do-not-populate is not fitted on
    /// the assembled board, whatever its layout footprint says: the footprint
    /// is only synchronised when someone runs "Update PCB from Schematic". So a
    /// part DNP on EITHER side is DNP.
    ///
    /// Marks every layout component whose reference the schematic places only
    /// as DNP, and returns the one coverage note that names them
    /// ([`dnp::schematic_only_dnp_note`]), or `None` when nothing disagreed (no
    /// schematic, an unreadable one, or both sides already agree). A layout
    /// record whose identity is contested (a duplicate designator with
    /// conflicting contents) is left alone.
    pub fn adopt_schematic_dnp(&mut self, sch: &Path) -> Option<String> {
        if !sch.is_file() {
            return None;
        }
        let sheet = Self::from_kicad_schematic_path(sch).ok()?;
        // reference -> every placement of it is DNP
        let mut dnp_by_ref: std::collections::HashMap<&str, bool> =
            std::collections::HashMap::new();
        for c in &sheet.components {
            let all = dnp_by_ref.entry(c.reference.as_str()).or_insert(true);
            *all &= c.dnp;
        }
        let mut disagreeing: Vec<String> = Vec::new();
        for c in self.components.iter_mut().filter(|c| {
            !c.dnp
                && !c
                    .properties
                    .iter()
                    .any(|(key, _)| key == DUPLICATE_REFERENCE_CONFLICT_KEY)
        }) {
            if dnp_by_ref.get(c.reference.as_str()) == Some(&true) {
                c.dnp = true;
                if !disagreeing.contains(&c.reference) {
                    disagreeing.push(c.reference.clone());
                }
            }
        }
        dnp::schematic_only_dnp_note(&disagreeing)
    }

    /// Geometric copper short / clearance check on the raw board text.
    ///
    /// Connectivity extraction works off pad nets alone, but a *short* is a
    /// geometric fact (two nets' copper touching) that only the layout carries.
    /// This dispatches on the file content and runs the same detection engine
    /// over either a KiCad `.kicad_pcb` ([`drc::run_drc`]) or an Eagle `.brd`
    /// ([`drc::eagle_drc_from_text`]); an unrecognised format returns an empty
    /// report.
    ///
    /// # Errors
    /// Fails when a KiCad board does not parse.
    pub fn drc(text: &str) -> Result<drc::DrcReport, ExtractError> {
        Self::drc_with_clearance(text, None)
    }

    /// [`Self::drc`] with an explicit copper clearance rule (mm) for the KiCad
    /// path. KiCad 10 (format 20260206) stores the design-rule clearance in the
    /// sibling `.kicad_pro` (`net_settings.classes[].clearance`), not the
    /// `.kicad_pcb`, so a caller that knows the board path can read the
    /// Default-class clearance and pass it here. `None` keeps the board's own /
    /// default behaviour. The Eagle path reads its own rules and ignores this.
    ///
    /// # Errors
    /// Fails when a KiCad board does not parse.
    pub fn drc_with_clearance(
        text: &str,
        clearance_override: Option<f64>,
    ) -> Result<drc::DrcReport, ExtractError> {
        Self::drc_with_clearance_rules(text, clearance_override.map(drc::ClearanceRules::new))
    }

    /// [`Self::drc`] with project-derived per-net clearance rules for KiCad
    /// boards. Other input formats keep their own scalar-rule behavior.
    ///
    /// # Errors
    /// Fails when a KiCad board does not parse.
    pub fn drc_with_clearance_rules(
        text: &str,
        rules: Option<drc::ClearanceRules>,
    ) -> Result<drc::DrcReport, ExtractError> {
        let head: String = text.chars().take(512).collect();
        if head.contains("(kicad_pcb") {
            Ok(drc::drc_from_text_with_clearance_rules(text, rules)?)
        } else if head.contains("<eagle") {
            Ok(drc::eagle_drc_from_text(text))
        } else {
            Ok(drc::DrcReport::default())
        }
    }

    /// Eagle `.brd` (XML, Eagle 6+): Arduino, Adafruit, SparkFun designs.
    ///
    /// # Errors
    /// [`ExtractError::Xml`] or [`ExtractError::WrongRoot`] when the text is
    /// not a readable Eagle board.
    pub fn from_eagle_brd(text: &str) -> Result<Self, ExtractError> {
        eagle::extract(text)
    }

    /// Clear the DNP flag on the named references: "these parts ARE populated
    /// on the board I am asking about".
    ///
    /// DNP in an ECAD file means "the assembler does not fit this", which is
    /// not always the same as "absent from the working system". The common
    /// case is a socketed module (an Arduino Nano, an ESP32 carrier) marked
    /// DNP because it is bought separately and plugged into headers: the
    /// design intends it to be there, and simulating without it silently
    /// deletes the board's processor. Depopulated bridge resistors and config
    /// straps are the opposite case, and stay skipped.
    ///
    /// Returns the number of parts actually un-DNP'd, and errors naming every
    /// reference that is not on the board, so a typo'd override fails loudly
    /// instead of quietly doing nothing.
    ///
    /// This fits exactly the named parts and nothing else. For the policy that
    /// decides the rest of the board's DNP parts, see
    /// [`apply_dnp_policy`](Self::apply_dnp_policy).
    ///
    /// # Errors
    /// [`ExtractError::UnknownReference`] naming every reference in `refs` that
    /// is not on the board.
    pub fn fit(&mut self, refs: &[String]) -> Result<usize, ExtractError> {
        let decision = self.apply_dnp_policy(dnp::DnpPolicy::Honour, refs, &[])?;
        Ok(decision.fitted.len())
    }

    /// IPC-D-356/356A fab netlist: the universal fallback any EDA exports.
    ///
    /// # Errors
    /// Fails when the text carries no IPC-D-356 records to read.
    pub fn from_ipc_d356(text: &str) -> Result<Self, ExtractError> {
        ipc356::extract(text)
    }

    /// Altium Designer `.PcbDoc` (binary OLE2 / Compound File Binary). Reads the
    /// nets, components and pads (with their net assignment) straight out of the
    /// `Nets6` / `Components6` / `Pads6` record streams. The layout carries full
    /// net connectivity, so the board file alone fully describes the circuit,
    /// the same way a `.kicad_pcb` does. See [`altium`] and `docs/ingest/ALTIUM.md`.
    ///
    /// # Errors
    /// [`ExtractError::Altium`] when the bytes are not a readable Compound File
    /// or lack the Altium record streams.
    pub fn from_altium_pcb(bytes: &[u8]) -> Result<Self, ExtractError> {
        altium::extract(bytes)
    }

    /// ODB++ (Siemens/Valor) design archive: a directory, a `.tgz` or a `.zip`.
    /// Reads nets, components and pads from the job's own EDA data rather than
    /// reverse-engineering them from copper, and cross-checks that data against
    /// the job's CAD netlist. See [`odbpp`] for the accounting this discards
    /// ([`odbpp::OdbExtraction::stats`] keeps it) and what is deliberately not
    /// modelled.
    ///
    /// # Errors
    /// [`ExtractError::Odb`] when the job cannot be opened or lacks the data a
    /// netlist needs.
    pub fn from_odbpp(path: &Path) -> Result<Self, ExtractError> {
        odbpp::from_odbpp(path).map(|e| e.board)
    }

    /// ODB++ from archive bytes (`.tgz` / `.tar` / `.zip`), for the web path
    /// that has an upload rather than a path.
    ///
    /// # Errors
    /// [`ExtractError::Odb`] when the archive cannot be unpacked or lacks the
    /// data a netlist needs.
    pub fn from_odbpp_archive(bytes: &[u8]) -> Result<Self, ExtractError> {
        odbpp::from_odbpp_archive(bytes).map(|e| e.board)
    }

    /// IPC-2581 (DPMX) design-exchange XML, revision B or C, namespaced or not.
    /// See [`ipc2581`]; [`ipc2581::extract`] keeps the read's accounting.
    ///
    /// # Errors
    /// [`ExtractError::Ipc2581`] or [`ExtractError::Xml`] when the document is
    /// not readable IPC-2581.
    pub fn from_ipc2581(text: &str) -> Result<Self, ExtractError> {
        ipc2581::extract(text).map(|e| e.board)
    }

    /// EasyEDA (JLCEDA) board: a Standard edition `.json` PCB document or a Pro
    /// edition `.epcb` layout. See [`easyeda`]; [`easyeda::extract`] keeps the
    /// read's accounting.
    ///
    /// # Errors
    /// [`ExtractError::Easyeda`] when the bytes are not a readable EasyEDA PCB.
    pub fn from_easyeda(bytes: &[u8]) -> Result<Self, ExtractError> {
        easyeda::extract(bytes).map(|e| e.board)
    }

    /// EasyEDA project archive (Pro `.epro`, or a zip of Standard `.json` PCBs).
    ///
    /// # Errors
    /// [`ExtractError::Easyeda`] when the archive cannot be unpacked, holds no
    /// board, or the board it holds does not read.
    pub fn from_easyeda_archive(bytes: &[u8]) -> Result<Self, ExtractError> {
        easyeda::extract_archive(bytes).map(|e| e.board)
    }

    /// ASCII Protel board export (`|RECORD=Board|KIND=Protel_Advanced_PCB`
    /// pipe-delimited text): the `.pcbdoc` form EasyEDA and several converters
    /// produce instead of Altium Designer's binary OLE2 container. Reads nets,
    /// components, pads and comment texts; carries no copper geometry for DRC.
    ///
    /// # Errors
    /// Fails when the text is not an ASCII Protel board export.
    pub fn from_protel_ascii(text: &str) -> Result<Self, ExtractError> {
        protel_ascii::extract(text)
    }

    /// Altium `.PcbDoc` geometric short / clearance DRC, the binary-format twin
    /// of [`Self::drc`]. Reads copper geometry (tracks, arcs, vias, pads,
    /// polygons) per net and feeds the same detection engine the KiCad and Eagle
    /// paths use.
    ///
    /// # Errors
    /// [`ExtractError::Altium`] when the bytes are not a readable `.PcbDoc`.
    pub fn altium_drc(bytes: &[u8]) -> Result<drc::DrcReport, ExtractError> {
        drc::altium_drc_from_bytes(bytes)
    }

    /// Sniff a *binary* board file and extract. Currently this is the Altium
    /// `.PcbDoc` path (OLE2 magic + Altium streams); everything else is text and
    /// goes through [`Self::from_auto`]. Returns `None` when the bytes are not a
    /// recognised binary board, so the caller can fall back to the text sniffer.
    ///
    /// Routes through the [`reader::Registry`], consulting only the binary
    /// readers so a text file handed here is not force-parsed as bytes (it
    /// returns `None` and the caller falls back to [`Self::from_auto`]).
    #[must_use]
    pub fn from_auto_bytes(bytes: &[u8]) -> Option<Result<Self, ExtractError>> {
        let registry = reader::Registry::builtin();
        registry
            .detect_binary(bytes, None)
            .map(|r| r.read(bytes, None))
    }

    /// Sniff the format from content and extract accordingly.
    ///
    /// Delegates to the [`reader::Registry`]: each format is a
    /// [`reader::BoardReader`] that owns its own detection, rather than one
    /// hard-coded substring ladder. An input no reader recognises fails with
    /// [`ExtractError::Unrecognized`], whose message describes the accepted
    /// formats in user words (see [`reader::unrecognized_message`]).
    ///
    /// # Errors
    /// [`ExtractError::Unrecognized`] when no reader claims the input, or the
    /// claiming reader's own error when the file does not read.
    pub fn from_auto(text: &str) -> Result<Self, ExtractError> {
        reader::Registry::builtin().read(text.as_bytes(), None)
    }

    /// Put the board's lists in canonical order: components by designator
    /// (natural, then every other field), nets by name.
    ///
    /// A board file lists its parts and nets in whatever order the CAD tool
    /// wrote them, and reordering the file does not change the circuit. Every
    /// consumer downstream (the binder's duplicate-designator pairs, the checks'
    /// iteration, the lists in the report) reads these two vectors, so putting
    /// them in one order here makes the whole report independent of the file's
    /// order instead of fixing each list where it surfaces. Net ids are left as
    /// the reader assigned them: pins refer to them, and nothing indexes `nets`
    /// by id.
    pub fn canonicalize(&mut self) {
        self.components.sort_by(cmp_components);
        self.nets
            .sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
    }

    #[must_use]
    pub fn net(&self, id: i64) -> Option<&Net> {
        self.nets.iter().find(|n| n.id == id)
    }

    #[must_use]
    pub fn net_by_name(&self, name: &str) -> Option<&Net> {
        self.nets.iter().find(|n| n.name == name)
    }

    #[must_use]
    pub fn component(&self, reference: &str) -> Option<&Component> {
        self.components.iter().find(|c| c.reference == reference)
    }

    /// (component, pin) pairs attached to a net.
    #[must_use]
    pub fn net_members(&self, net_id: i64) -> Vec<(&Component, &Pin)> {
        let mut out = Vec::new();
        for c in &self.components {
            for p in &c.pins {
                if p.net == Some(net_id) {
                    out.push((c, p));
                }
            }
        }
        out
    }

    /// Consistency report: problems worth surfacing before simulation.
    #[must_use]
    pub fn lint(&self) -> Lint {
        let mut lint = Lint::default();
        let net_ids: std::collections::HashSet<i64> = self.nets.iter().map(|n| n.id).collect();
        let mut degree: std::collections::HashMap<i64, usize> = std::collections::HashMap::new();
        for c in &self.components {
            let mut connected = 0usize;
            for p in &c.pins {
                match p.net {
                    Some(id) => {
                        connected += 1;
                        if !net_ids.contains(&id) {
                            lint.undeclared_nets
                                .push((c.reference.clone(), p.number.clone(), id));
                        }
                        *degree.entry(id).or_default() += 1;
                    }
                    None => lint
                        .unconnected_pins
                        .push((c.reference.clone(), p.number.clone())),
                }
            }
            if connected == 0 && !c.pins.is_empty() {
                lint.floating_components.push(c.reference.clone());
            }
        }
        for net in &self.nets {
            // Net 0 is KiCad's "no net" bucket; skip it.
            if net.id != 0 && degree.get(&net.id).copied().unwrap_or(0) == 1 {
                lint.single_pin_nets.push(net.name.clone());
            }
        }
        lint
    }
}

/// Total order on components: designator first (natural, so R2 precedes R10),
/// then every other field, so two parts that differ in anything sort the same
/// way whatever order the file listed them in. Parts equal in every field are
/// interchangeable.
fn cmp_components(a: &Component, b: &Component) -> std::cmp::Ordering {
    let position = |c: &Component| c.position.map(<[f64; 3]>::from);
    let cmp_position = |x: Option<[f64; 3]>, y: Option<[f64; 3]>| match (x, y) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Less,
        (Some(_), None) => std::cmp::Ordering::Greater,
        (Some(x), Some(y)) => x
            .iter()
            .zip(y.iter())
            .map(|(p, q)| p.total_cmp(q))
            .find(|o| o.is_ne())
            .unwrap_or(std::cmp::Ordering::Equal),
    };
    let pins = |c: &Component| -> Vec<(String, Option<i64>, String, String)> {
        c.pins
            .iter()
            .map(|p| (p.number.clone(), p.net, p.function.clone(), p.kind.clone()))
            .collect()
    };
    hauksbee_ir::order::cmp_refs(&a.reference, &b.reference)
        .then_with(|| a.value.cmp(&b.value))
        .then_with(|| a.lib_id.cmp(&b.lib_id))
        .then_with(|| a.footprint.cmp(&b.footprint))
        .then_with(|| a.layer.cmp(&b.layer))
        .then_with(|| a.dnp.cmp(&b.dnp))
        .then_with(|| cmp_position(position(a), position(b)))
        .then_with(|| a.properties.cmp(&b.properties))
        .then_with(|| pins(a).cmp(&pins(b)))
}

/// Reverse KiCad's `{token}` name escape.
///
/// ODB++ and IPC-2581 both restrict the characters a name may contain, and
/// KiCad's exporters push every name through their own `EscapeString`, which
/// replaces the offending characters with brace tokens. A hierarchical net
/// `Net-(U4-LNA_IN/RF)` therefore leaves KiCad as `Net-(U4-LNA_IN{slash}RF)` in
/// BOTH exchange formats, and a reader that takes the name literally reports a
/// net the rest of the design does not have — the netlint's `/SHEET/SIGNAL`
/// hierarchy matching, and any comparison against the same board's native
/// reading, both break on it.
///
/// The table is KiCad's (`string_utils.cpp`). This is applied ONLY when the file
/// says KiCad wrote it ([`odbpp::OdbStats::producer`] /
/// [`ipc2581::Ipc2581Stats::producer`]), because the tokens are KiCad's
/// convention and not the formats': a net another tool genuinely named
/// `A{slash}B` must survive intact.
/// The name mapping an exchange reader applies, given the file's stated
/// producer: KiCad's `{token}` escapes are undone only when KiCad wrote it.
pub(crate) fn name_unescaper(producer: &str) -> impl Fn(&str) -> String {
    let kicad = producer.to_ascii_uppercase().contains("KICAD");
    move |s: &str| {
        if kicad {
            unescape_kicad_name(s)
        } else {
            s.to_string()
        }
    }
}

use netname::unescape_net_name as unescape_kicad_name;

/// Collapse components that share a reference designator into one.
///
/// Multiple records under one designator may describe one electrical part with
/// several physical instances (for example, a test point placed on both board
/// sides). The KiCad layout reader has always reconciled that case (see
/// `pcb.rs`), because every downstream count — bind rows, `num_components`,
/// resolve-rate denominators, [`ExtractedBoard::component`] lookups — assumes
/// one part per designator. The exchange readers ([`odbpp`], [`ipc2581`]) route
/// through this so a board re-exported to another format keeps the same part
/// list when its records are actually compatible.
///
/// Compatible placements reconcile their metadata and contribute all pads. A
/// part is DNP only when *every* instance is. Conflicting values, footprints,
/// library ids, properties, or one pin number mapped to different nets are not
/// silently made first-wins: both records are preserved under collision-safe
/// identities carrying an explicit `duplicate_reference_conflict` property.
pub const DUPLICATE_REFERENCE_CONFLICT_KEY: &str = "duplicate_reference_conflict";

fn append_property(component: &mut Component, key: &str, value: String) {
    if !component
        .properties
        .iter()
        .any(|(existing_key, existing_value)| existing_key == key && existing_value == &value)
    {
        component.properties.push((key.to_string(), value));
    }
}

fn hard_conflicts(previous: &Component, incoming: &Component) -> Vec<String> {
    let mut conflicts = Vec::new();
    let fields = [
        ("values", &previous.value, &incoming.value),
        ("footprints", &previous.footprint, &incoming.footprint),
        ("library ids", &previous.lib_id, &incoming.lib_id),
    ];
    for (what, a, b) in fields {
        if !a.is_empty() && !b.is_empty() && a != b {
            conflicts.push(format!("{what} differ ('{a}' versus '{b}')"));
        }
    }
    for previous_pin in &previous.pins {
        for incoming_pin in &incoming.pins {
            if previous_pin.number == incoming_pin.number
                && matches!(
                    (previous_pin.net, incoming_pin.net),
                    (Some(previous_net), Some(incoming_net)) if previous_net != incoming_net
                )
            {
                conflicts.push(format!(
                    "pin '{}' maps to {:?} versus {:?}",
                    previous_pin.number, previous_pin.net, incoming_pin.net
                ));
            }
        }
    }
    for (previous_key, previous_value) in &previous.properties {
        for (incoming_key, incoming_value) in &incoming.properties {
            if previous_key == incoming_key && previous_value != incoming_value {
                conflicts.push(format!(
                    "property '{previous_key}' differs ('{previous_value}' versus '{incoming_value}')"
                ));
            }
        }
    }
    conflicts.sort();
    conflicts.dedup();
    conflicts
}

// Metadata order in ODB++, IPC-2581, binary Altium and ASCII Protel is not
// guaranteed to agree. Pick the unsuffixed representative from content,
// not arrival order, so reading the same conflict through another path
// cannot silently change what `board.component("J1")` means.
fn stable_component_key(component: &Component) -> String {
    let mut properties: Vec<_> = component
        .properties
        .iter()
        .filter(|(key, _)| key != DUPLICATE_REFERENCE_CONFLICT_KEY)
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    properties.sort_unstable();
    let mut pins: Vec<_> = component
        .pins
        .iter()
        .map(|pin| {
            (
                pin.number.as_str(),
                pin.net,
                pin.function.as_str(),
                pin.kind.as_str(),
            )
        })
        .collect();
    pins.sort_unstable();
    format!(
        "{:?}",
        (
            component.value.as_str(),
            component.footprint.as_str(),
            component.lib_id.as_str(),
            component.dnp,
            properties,
            pins,
        )
    )
}

fn merge_compatible(previous: &mut Component, incoming: Component) {
    previous.dnp = previous.dnp && incoming.dnp;
    if previous.value.is_empty() && !incoming.value.is_empty() {
        previous.value = incoming.value;
    }
    if previous.footprint.is_empty() && !incoming.footprint.is_empty() {
        previous.footprint = incoming.footprint;
    }
    if previous.lib_id.is_empty() && !incoming.lib_id.is_empty() {
        previous.lib_id = incoming.lib_id;
    }
    if previous.position.is_none() {
        previous.position = incoming.position;
    }
    if previous.layer.is_empty() && !incoming.layer.is_empty() {
        previous.layer = incoming.layer;
    }

    for (key, value) in incoming.properties {
        if key == altium::VALUE_UNRESOLVED_KEY && !previous.value.is_empty() {
            continue;
        }
        append_property(previous, &key, value);
    }
    if !previous.value.is_empty() {
        previous
            .properties
            .retain(|(key, _)| key != altium::VALUE_UNRESOLVED_KEY);
    }
    for mut pin in incoming.pins {
        // Readers can recover connectivity from different streams. The same
        // physical pad may therefore be unknown in one record and known in
        // another; enrich that record instead of manufacturing an identity
        // conflict or two logical pins. Pads at distinct positions remain
        // distinct physical placements even when they share pin and net.
        if let Some(existing) = previous
            .pins
            .iter_mut()
            .find(|existing| existing.number == pin.number && existing.position == pin.position)
        {
            if existing.net.is_none() {
                existing.net = pin.net;
            } else if pin.net.is_none() {
                pin.net = existing.net;
            }
            if existing.function.is_empty() {
                existing.function.clone_from(&pin.function);
            } else if pin.function.is_empty() {
                pin.function.clone_from(&existing.function);
            }
            if existing.kind.is_empty() {
                existing.kind.clone_from(&pin.kind);
            } else if pin.kind.is_empty() {
                pin.kind.clone_from(&existing.kind);
            }
        }
        // Do not deduplicate physical pad placements. A real split
        // footprint can place the same numbered pad at the same coordinate
        // on opposite layers (Watchy TP4/TP5); downstream electrical walks
        // deduplicate by pin/net where needed, while geometry consumers need
        // both records.
        previous.pins.push(pin);
    }
}

pub(crate) fn merge_duplicate_references(components: Vec<Component>) -> Vec<Component> {
    let mut used_references: HashSet<String> = components
        .iter()
        .filter(|component| !component.reference.is_empty())
        .map(|component| component.reference.clone())
        .collect();
    let mut groups: Vec<(String, Vec<Component>)> = Vec::new();
    let mut group_index: HashMap<String, usize> = HashMap::new();
    for component in components {
        if component.reference.is_empty() {
            groups.push((String::new(), vec![component]));
            continue;
        }
        let reference = component.reference.clone();
        let index = *group_index.entry(reference.clone()).or_insert_with(|| {
            groups.push((reference, Vec::new()));
            groups.len() - 1
        });
        groups[index].1.push(component);
    }

    let mut out: Vec<Component> = Vec::new();
    for (reference, mut records) in groups {
        if reference.is_empty() || records.len() == 1 {
            out.extend(records);
            continue;
        }

        records.sort_by_key(stable_component_key);
        let mut representatives: Vec<Component> = Vec::new();
        for incoming in records {
            if let Some(index) = representatives
                .iter()
                .position(|previous| hard_conflicts(previous, &incoming).is_empty())
            {
                merge_compatible(&mut representatives[index], incoming);
            } else {
                representatives.push(incoming);
            }
        }

        if representatives.len() == 1 {
            out.push(representatives.pop().unwrap());
            continue;
        }

        representatives.sort_by_key(stable_component_key);
        let mut conflicts = Vec::new();
        for left in 0..representatives.len() {
            for right in (left + 1)..representatives.len() {
                conflicts.extend(hard_conflicts(
                    &representatives[left],
                    &representatives[right],
                ));
            }
        }
        conflicts.sort();
        conflicts.dedup();
        let note = format!(
            "records named '{reference}' were kept distinct: {}",
            conflicts.join("; ")
        );

        for (index, mut component) in representatives.into_iter().enumerate() {
            append_property(
                &mut component,
                DUPLICATE_REFERENCE_CONFLICT_KEY,
                note.clone(),
            );
            if index > 0 {
                let ordinal = index + 1;
                let candidate = format!("{reference}@conflict-{ordinal}");
                let mut generated = candidate.clone();
                let mut collision = 1usize;
                while !used_references.insert(generated.clone()) {
                    collision += 1;
                    generated = format!("{candidate}@generated-{collision}");
                }
                if generated != candidate {
                    append_property(
                        &mut component,
                        altium::REFERENCE_IDENTITY_NOTE_KEY,
                        format!(
                            "generated conflict identity '{candidate}' collided with a genuine source designator; using '{generated}'"
                        ),
                    );
                }
                component.reference = generated;
            }
            out.push(component);
        }
    }
    out
}

/// Read an input file whole, but only if it is a regular file.
///
/// `std::fs::read` on a device or pipe has no end: a board path that is (or
/// links to) `/dev/zero` grows the buffer until the process is killed, and a
/// FIFO with no writer blocks forever. Neither is a board, so both are refused
/// up front with a message that says why, for every reader that opens a path.
///
/// # Errors
///
/// An [`std::io::ErrorKind::InvalidInput`] error for anything that is not a
/// regular file, otherwise whatever opening or reading the file raises (a
/// missing file is still [`std::io::ErrorKind::NotFound`]).
pub fn read_regular_file(path: &std::path::Path) -> std::io::Result<Vec<u8>> {
    let kind = std::fs::metadata(path)?.file_type();
    if !kind.is_file() {
        let what = if kind.is_dir() {
            "a directory"
        } else {
            "not a regular file (a device, pipe or socket)"
        };
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{what}; hauksbee reads board files, not streams"),
        ));
    }
    std::fs::read(path)
}

/// [`read_regular_file`] decoded as UTF-8, with the same error `read_to_string`
/// gives for text that is not.
///
/// # Errors
///
/// As [`read_regular_file`], plus [`std::io::ErrorKind::InvalidData`] for bytes
/// that are not UTF-8.
pub fn read_regular_string(path: &std::path::Path) -> std::io::Result<String> {
    String::from_utf8(read_regular_file(path)?).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "stream did not contain valid UTF-8",
        )
    })
}

/// [`read_regular_file`] decoded as UTF-8 with invalid bytes replaced.
///
/// For fab and CAM text, where a Latin-1 degree sign or micro sign in a comment
/// is common and says nothing about the geometry: refusing the whole job over it
/// would be a false refusal, and dropping the file would lose a layer.
///
/// # Errors
///
/// As [`read_regular_file`].
pub fn read_regular_text_lossy(path: &std::path::Path) -> std::io::Result<String> {
    let bytes = read_regular_file(path)?;
    Ok(match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
    })
}

/// Read one archive member whole, refusing one that inflates past
/// [`MAX_INFLATED_BYTES`].
///
/// A zip member's declared size is the archive's own claim and a bomb lies
/// about it, so the read is bounded by the ceiling (`take(cap + 1)` so a member
/// that exactly fills it succeeds and a bigger one is detected rather than
/// truncated). The error carries the shared expansion-limit wording used by the
/// gerber and ODB++ readers, so the same hostile upload gets the same answer
/// from every door. Callers that open a single member out of a user's zip
/// (a netlist, a layout file) use this instead of `read_to_end`.
///
/// # Errors
///
/// [`std::io::ErrorKind::InvalidData`] with the expansion-limit message when the
/// member is too large, otherwise whatever reading the member raises.
pub fn read_member_capped(member: impl std::io::Read) -> std::io::Result<Vec<u8>> {
    use std::io::Read as _;
    let mut buf = Vec::new();
    member.take(MAX_INFLATED_BYTES + 1).read_to_end(&mut buf)?;
    if buf.len() as u64 > MAX_INFLATED_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            odbpp::tree::expansion_limit_message("archive member"),
        ));
    }
    Ok(buf)
}

/// Builders for the hand-made boards the unit tests use.
#[cfg(test)]
pub(crate) mod testutil {
    use crate::{Component, ExtractedBoard, Net, Pin};

    pub fn pin(number: &str, net: Option<i64>) -> Pin {
        Pin {
            number: number.into(),
            net,
            ..Default::default()
        }
    }

    pub fn part(
        reference: &str,
        value: &str,
        lib_id: &str,
        footprint: &str,
        pins: Vec<Pin>,
    ) -> Component {
        Component {
            reference: reference.into(),
            value: value.into(),
            lib_id: lib_id.into(),
            footprint: footprint.into(),
            pins,
            ..Default::default()
        }
    }

    pub fn board(nets: &[(i64, &str)], components: Vec<Component>) -> ExtractedBoard {
        ExtractedBoard {
            name: "b".into(),
            nets: nets.iter().map(|&(id, name)| Net::new(id, name)).collect(),
            components,
        }
    }
}

#[cfg(test)]
mod duplicate_reference_merge_tests {
    use super::*;
    use crate::testutil::{part, pin};

    fn component(
        reference: &str,
        value: &str,
        footprint: &str,
        properties: Vec<(&str, &str)>,
        pins: Vec<(&str, Option<i64>)>,
    ) -> Component {
        let pins = pins.into_iter().map(|(n, net)| pin(n, net)).collect();
        let mut c = part(reference, value, footprint, footprint, pins);
        c.properties = properties
            .into_iter()
            .map(|(key, value)| (key.into(), value.into()))
            .collect();
        c
    }

    #[test]
    fn conflicting_values_or_rating_properties_are_preserved_as_distinct_parts() {
        let merged = merge_duplicate_references(vec![
            component(
                "R1",
                "10k",
                "R0603",
                vec![("voltage_rating", "25V")],
                vec![("1", Some(1))],
            ),
            component(
                "R1",
                "22k",
                "R0603",
                vec![("voltage_rating", "50V")],
                vec![("2", Some(2))],
            ),
        ]);

        assert_eq!(
            merged.len(),
            2,
            "conflicting metadata must not be first-wins"
        );
        assert_eq!(merged[0].reference, "R1");
        assert!(merged[1].reference.starts_with("R1@conflict-"));
        for part in &merged {
            assert!(part
                .properties
                .iter()
                .any(|(key, _)| key == "duplicate_reference_conflict"));
        }
    }

    #[test]
    fn one_pin_number_on_different_nets_is_never_merged() {
        let merged = merge_duplicate_references(vec![
            component("U1", "IC", "QFN", vec![], vec![("1", Some(10))]),
            component("U1", "IC", "QFN", vec![], vec![("1", Some(11))]),
        ]);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].pins[0].net, Some(10));
        assert_eq!(merged[1].pins[0].net, Some(11));
    }

    #[test]
    fn differing_footprints_are_conflicts_in_both_input_orders() {
        let mut identity_map = None;
        for reverse in [false, true] {
            let unresolved = component(
                "J1",
                "",
                "Header_A",
                vec![(altium::VALUE_UNRESOLVED_KEY, "missing")],
                vec![("1", Some(1))],
            );
            let resolved = component(
                "J1",
                "Conn_02x10",
                "Header_B",
                vec![("voltage_rating", "50V")],
                vec![("2", Some(2))],
            );
            let input = if reverse {
                vec![resolved, unresolved]
            } else {
                vec![unresolved, resolved]
            };
            let merged = merge_duplicate_references(input);

            assert_eq!(merged.len(), 2);
            assert_eq!(merged[0].reference, "J1");
            assert!(merged[1].reference.starts_with("J1@conflict-"));
            assert_eq!(
                merged
                    .iter()
                    .map(|part| part.footprint.as_str())
                    .collect::<std::collections::BTreeSet<_>>(),
                std::collections::BTreeSet::from(["Header_A", "Header_B"]),
                "neither stream order may choose a different primary footprint"
            );
            assert!(merged.iter().all(|part| part
                .properties
                .iter()
                .any(|(key, _)| key == "duplicate_reference_conflict")));
            let this_map: std::collections::BTreeMap<_, _> = merged
                .iter()
                .map(|part| (part.reference.clone(), part.footprint.clone()))
                .collect();
            if let Some(expected) = &identity_map {
                assert_eq!(
                    &this_map, expected,
                    "the unsuffixed identity and every conflict identity must mean the same thing in either stream order"
                );
            } else {
                identity_map = Some(this_map);
            }
        }
    }

    #[test]
    fn generated_conflict_names_never_replace_a_genuine_designator() {
        let merged = merge_duplicate_references(vec![
            component("J1", "A", "Header_A", vec![], vec![("1", Some(1))]),
            component("J1", "B", "Header_B", vec![], vec![("1", Some(2))]),
            component(
                "J1@conflict-2",
                "REAL",
                "Header_REAL",
                vec![],
                vec![("1", Some(3))],
            ),
        ]);

        assert_eq!(
            merged
                .iter()
                .find(|part| part.reference == "J1@conflict-2")
                .map(|part| part.value.as_str()),
            Some("REAL"),
            "a source designator always wins over a generated candidate"
        );
        assert!(merged.iter().any(|part| {
            part.reference.starts_with("J1@conflict-2@generated-")
                && part
                    .properties
                    .iter()
                    .any(|(key, _)| key == altium::REFERENCE_IDENTITY_NOTE_KEY)
        }));
    }

    #[test]
    fn repeated_pin_records_preserve_physical_placements() {
        let merged = merge_duplicate_references(vec![
            component("R1", "10k", "R0603", vec![], vec![("1", Some(1))]),
            component("R1", "10k", "R0603", vec![], vec![("1", Some(1))]),
        ]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].pins.len(), 2);
    }

    #[test]
    fn a_missing_pin_net_is_enriched_not_misreported_as_a_conflict() {
        let merged = merge_duplicate_references(vec![
            component("J1", "Connector", "Header", vec![], vec![("1", None)]),
            component("J1", "Connector", "Header", vec![], vec![("1", Some(42))]),
        ]);

        assert_eq!(merged.len(), 1, "unknown versus known is not contradictory");
        assert_eq!(merged[0].pins.len(), 2);
        assert!(merged[0].pins.iter().all(|pin| pin.net == Some(42)));
        assert!(!merged[0]
            .properties
            .iter()
            .any(|(key, _)| key == DUPLICATE_REFERENCE_CONFLICT_KEY));
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Lint {
    /// Pins whose net id has no declaration: (reference, pin, net id).
    pub undeclared_nets: Vec<(String, String, i64)>,
    /// Pins on no net at all: (reference, pin).
    pub unconnected_pins: Vec<(String, String)>,
    /// Components with pins but no connected pin.
    pub floating_components: Vec<String>,
    /// Named nets touching exactly one pin.
    pub single_pin_nets: Vec<String>,
}

impl Lint {
    #[must_use]
    pub const fn is_clean(&self) -> bool {
        self.undeclared_nets.is_empty() && self.floating_components.is_empty()
    }
}

#[cfg(test)]
mod schematic_pin_name_tests {
    use crate::testutil::{part, pin};
    use crate::ExtractedBoard;

    /// Only a component whose layout pads are all unnamed takes names from
    /// the sibling schematic; one the layout already names keeps them.
    #[test]
    fn unnamed_components_adopt_eagle_schematic_pin_names() {
        let sch = r#"<?xml version="1.0"?>
<eagle><drawing><schematic>
<libraries><library name="l">
  <symbols><symbol name="S"><pin name="IO2"/><pin name="GND"/></symbol></symbols>
  <devicesets><deviceset name="D"><gates><gate name="G" symbol="S"/></gates><devices>
    <device name="" package="P"><connects>
      <connect gate="G" pin="IO2" pad="24"/><connect gate="G" pin="GND" pad="1"/>
    </connects></device></devices></deviceset></devicesets>
</library></libraries>
<parts><part name="X2" library="l" deviceset="D" device=""/>
<part name="U1" library="l" deviceset="D" device=""/></parts>
<sheets/>
</schematic></drawing></eagle>"#;
        let path = std::env::temp_dir().join(format!("hauksbee-adopt-{}.sch", std::process::id()));
        std::fs::write(&path, sch).unwrap();
        let module = part(
            "X2",
            "WROOM",
            "P",
            "P",
            vec![pin("1", Some(1)), pin("24", Some(2))],
        );
        let mut named = part("U1", "WROOM", "P", "P", vec![pin("24", Some(2))]);
        named.pins[0].function = "LAYOUT_NAME".into();
        let mut board = ExtractedBoard {
            name: "adopt".into(),
            nets: Vec::new(),
            components: vec![module, named],
        };
        board.adopt_schematic_pin_names(&path);
        std::fs::remove_file(&path).ok();
        let functions = |i: usize| -> Vec<&str> {
            board.components[i]
                .pins
                .iter()
                .map(|p| p.function.as_str())
                .collect()
        };
        assert_eq!(functions(0), ["GND", "IO2"]);
        assert_eq!(functions(1), ["LAYOUT_NAME"]);
    }

    /// A layout part with a blank value takes the library device name the
    /// schematic placed it from (`*` = technology, `?` = device); a valued
    /// part keeps its own value.
    #[test]
    fn blank_eagle_values_adopt_the_schematic_device_name() {
        let sch = r#"<?xml version="1.0"?>
<eagle><drawing><schematic>
<libraries><library name="l">
  <symbols><symbol name="S"><pin name="A"/></symbol></symbols>
  <devicesets><deviceset name="74*245?"><gates><gate name="G" symbol="S"/></gates><devices>
    <device name="DW" package="P"><connects><connect gate="G" pin="A" pad="1"/></connects>
    </device></devices></deviceset>
  <deviceset name="TINY85"><gates><gate name="G" symbol="S"/></gates><devices>
    <device name="-MU" package="P"><connects><connect gate="G" pin="A" pad="1"/></connects>
    </device></devices></deviceset></devicesets>
</library></libraries>
<parts><part name="U1" library="l" deviceset="74*245?" device="DW" technology="HCT"/>
<part name="U2" library="l" deviceset="TINY85" device="-MU" value=""/>
<part name="U3" library="l" deviceset="TINY85" device="-MU" value="KEEP"/></parts>
<sheets/>
</schematic></drawing></eagle>"#;
        let path = std::env::temp_dir().join(format!("hauksbee-blank-{}.sch", std::process::id()));
        std::fs::write(&path, sch).unwrap();
        let mut board = ExtractedBoard {
            name: "blank".into(),
            nets: Vec::new(),
            components: ["U1", "U2", "U3"]
                .iter()
                .map(|r| {
                    part(
                        r,
                        if *r == "U3" { "OWN" } else { "" },
                        "P",
                        "P",
                        vec![pin("1", Some(1))],
                    )
                })
                .collect(),
        };
        board.adopt_schematic_pin_names(&path);
        std::fs::remove_file(&path).ok();
        let values: Vec<&str> = board.components.iter().map(|c| c.value.as_str()).collect();
        assert_eq!(values, ["74HCT245DW", "TINY85-MU", "OWN"]);
    }

    /// A rating-string value keeps its text, and the part number the library
    /// device name embeds rides along as a candidate identity property.
    #[test]
    fn eagle_library_device_part_numbers_become_candidate_properties() {
        let sch = r#"<?xml version="1.0"?>
<eagle><drawing><schematic>
<libraries><library name="l">
  <symbols><symbol name="S"><pin name="A"/></symbol></symbols>
  <devicesets><deviceset name="MOSFET_PCH"><gates><gate name="G" symbol="S"/></gates><devices>
    <device name="-XQ1234UX-7" package="P"><connects><connect gate="G" pin="A" pad="1"/></connects>
    </device></devices></deviceset>
  <deviceset name="RESISTOR"><gates><gate name="G" symbol="S"/></gates><devices>
    <device name="-0402T-1/16W-1%" package="P"><connects><connect gate="G" pin="A" pad="1"/></connects>
    </device></devices></deviceset></devicesets>
</library></libraries>
<parts><part name="Q1" library="l" deviceset="MOSFET_PCH" device="-XQ1234UX-7" value="20V/4.2A"/>
<part name="R1" library="l" deviceset="RESISTOR" device="-0402T-1/16W-1%" value="10k"/></parts>
<sheets/>
</schematic></drawing></eagle>"#;
        let path = std::env::temp_dir().join(format!("hauksbee-devpn-{}.sch", std::process::id()));
        std::fs::write(&path, sch).unwrap();
        let mut board = ExtractedBoard {
            name: "devpn".into(),
            nets: Vec::new(),
            components: [("Q1", "20V/4.2A"), ("R1", "10k")]
                .iter()
                .map(|(r, v)| part(r, v, "P", "P", vec![pin("1", Some(1))]))
                .collect(),
        };
        board.adopt_schematic_pin_names(&path);
        std::fs::remove_file(&path).ok();
        let device = |r: &str| {
            let c = board.components.iter().find(|c| c.reference == r).unwrap();
            let key = crate::bom::LIBRARY_DEVICE_PROPERTY;
            (
                c.value.clone(),
                c.properties
                    .iter()
                    .find(|(k, _)| k == key)
                    .map(|p| p.1.clone()),
            )
        };
        assert_eq!(device("Q1"), ("20V/4.2A".into(), Some("XQ1234UX-7".into())));
        assert_eq!(device("R1"), ("10k".into(), None));
    }

    /// The part number an Eagle device name spells, and the names that spell
    /// none: package and value codes, tolerances, class words.
    #[test]
    fn embedded_part_numbers_skip_package_value_and_class_codes() {
        let pn = crate::eagle_sch::embedded_part_number;
        #[rustfmt::skip]
        let cases = [
            (("MOSFET_PCH", "-XQ2305UX-7", ""), Some("XQ2305UX-7")),
            (("V_REG_XR2112", "K-3.3V", ""), Some("XR2112K-3.3V")),
            (("DIODE-SCHOTTKY", "-XB20J", ""), Some("XB20J")),
            (("MOSFET-NCH", "-2N7002PW", ""), Some("2N7002PW")),
            (("0.1UF", "-0402T-16V-10%", ""), None),
            (("PPTC", "_6V500MA", ""), None),
            (("LED-BLUE", "0603", ""), None),
            (("MCU", "1:1-LESS_PASTE", ""), None),
            (("CONN", "-SMD-4.6X2.8MM", ""), None),
            (("IC", "DFN8", ""), None),
            (("LED", "1206", ""), None),
        ];
        for ((set, device, tech), want) in cases {
            assert_eq!(pn(set, device, tech).as_deref(), want, "{set} {device}");
        }
    }
}

#[cfg(test)]
mod read_regular_file_tests {
    use super::{read_member_capped, read_regular_file};

    #[test]
    fn an_archive_member_under_the_ceiling_reads_whole() {
        let bytes = read_member_capped(std::io::Cursor::new(b"P1 N1 U1 *".to_vec())).unwrap();
        assert_eq!(bytes, b"P1 N1 U1 *");
    }

    #[test]
    fn a_regular_file_reads_whole() {
        let path = std::env::temp_dir().join(format!("hauksbee_regular_{}", std::process::id()));
        std::fs::write(&path, b"(kicad_pcb)").unwrap();
        assert_eq!(read_regular_file(&path).unwrap(), b"(kicad_pcb)");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_missing_file_stays_not_found() {
        let err = read_regular_file(std::path::Path::new("/no/such/hauksbee/board")).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }

    #[cfg(unix)]
    #[test]
    fn an_endless_device_is_refused_instead_of_read_until_the_process_dies() {
        // /dev/zero never ends: `fs::read` on it grows until the OOM killer
        // answers. A symlink to it is the same file once metadata follows it.
        let err = read_regular_file(std::path::Path::new("/dev/zero")).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains("not a regular file"), "{err}");
        let link = std::env::temp_dir().join(format!("hauksbee_zero_link_{}", std::process::id()));
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink("/dev/zero", &link).unwrap();
        let through = read_regular_file(&link).unwrap_err();
        let _ = std::fs::remove_file(&link);
        assert_eq!(through.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn a_directory_is_refused_with_its_own_wording() {
        let err = read_regular_file(&std::env::temp_dir()).unwrap_err();
        assert!(err.to_string().contains("a directory"), "{err}");
    }
}

#[cfg(test)]
mod canonical_order_tests {
    use super::{Component, ExtractedBoard, Net};

    fn part(reference: &str, value: &str) -> Component {
        Component {
            reference: reference.into(),
            value: value.into(),
            ..Component::default()
        }
    }

    fn board(refs: &[(&str, &str)], nets: &[(i64, &str)]) -> ExtractedBoard {
        ExtractedBoard {
            name: "t".into(),
            nets: nets.iter().map(|&(i, n)| Net::new(i, n)).collect(),
            components: refs.iter().map(|&(r, v)| part(r, v)).collect(),
        }
    }

    #[test]
    fn components_and_nets_come_out_in_one_order_whatever_the_file_order() {
        let mut a = board(
            &[("R10", "1k"), ("R2", "1k"), ("C1", "1u"), ("R2", "0")],
            &[(2, "VCC"), (1, "GND"), (3, "AREF")],
        );
        let mut b = board(
            &[("R2", "0"), ("C1", "1u"), ("R2", "1k"), ("R10", "1k")],
            &[(3, "AREF"), (1, "GND"), (2, "VCC")],
        );
        a.canonicalize();
        b.canonicalize();
        let refs = |x: &ExtractedBoard| -> Vec<(String, String)> {
            x.components
                .iter()
                .map(|c| (c.reference.clone(), c.value.clone()))
                .collect()
        };
        assert_eq!(refs(&a), refs(&b));
        // Natural order (R2 before R10), duplicates ordered by their contents.
        let order: Vec<&str> = a.components.iter().map(|c| c.reference.as_str()).collect();
        assert_eq!(order, ["C1", "R2", "R2", "R10"]);
        let names = |x: &ExtractedBoard| -> Vec<(i64, String)> {
            x.nets.iter().map(|n| (n.id, n.name.clone())).collect()
        };
        assert_eq!(names(&a), names(&b));
        assert_eq!(a.nets[0].name, "AREF");
    }
}

#[cfg(test)]
mod schematic_dnp_merge_tests {
    use crate::testutil::{part, pin};
    use crate::ExtractedBoard;
    use std::fmt::Write as _;

    /// A schematic placing `parts` ((reference, dnp)) of a two-pin symbol
    /// whose library definition carries a Description and keywords.
    fn schematic(parts: &[(&str, bool)]) -> String {
        let mut text = String::from(
            r#"(kicad_sch (version 20250114) (generator "t") (uuid "00000000-0000-0000-0000-000000000010")
(lib_symbols (symbol "Lib:Z"
  (property "Description" "Bidirectional TVS, 5 V" (at 0 0 0))
  (property "ki_keywords" "DIODE TVS" (at 0 0 0))
  (symbol "Z_1_1"
    (pin passive line (at 0 3.81 270) (length 1.27) (name "A") (number "1"))
    (pin passive line (at 0 -3.81 90) (length 1.27) (name "C") (number "2")))))
"#,
        );
        for (i, (reference, dnp)) in parts.iter().enumerate() {
            let flag = if *dnp { "yes" } else { "no" };
            write!(
                text,
                r#"(symbol (lib_id "Lib:Z") (at {x} 100 0) (unit 1) (dnp {flag})
  (uuid "00000000-0000-0000-0000-0000000000{i:02}")
  (property "Reference" "{reference}" (at 0 0 0)) (property "Value" "0R" (at 0 0 0))
  (pin "1" (uuid "00000000-0000-0000-0000-0000000001{i:02}"))
  (pin "2" (uuid "00000000-0000-0000-0000-0000000002{i:02}"))
  (instances (project "t" (path "/00000000-0000-0000-0000-000000000010" (reference "{reference}") (unit 1)))))
"#,
                x = 50 + 20 * i
            )
            .unwrap();
        }
        text.push_str("(sheet_instances (path \"/\" (page \"1\"))))\n");
        text
    }

    fn pcb(parts: &[(&str, bool)]) -> ExtractedBoard {
        ExtractedBoard {
            name: "b".into(),
            nets: Vec::new(),
            components: parts
                .iter()
                .map(|(r, dnp)| {
                    let mut c = part(r, "0R", "", "R_0603", vec![pin("1", None), pin("2", None)]);
                    c.dnp = *dnp;
                    c
                })
                .collect(),
        }
    }

    fn merged(
        sch: &[(&str, bool)],
        pcb_parts: &[(&str, bool)],
    ) -> (ExtractedBoard, Option<String>) {
        let mut tag = String::new();
        for (r, d) in sch {
            write!(tag, "{r}{d}").unwrap();
        }
        let path = std::env::temp_dir().join(format!(
            "hauksbee-dnp-merge-{}-{tag}.kicad_sch",
            std::process::id()
        ));
        std::fs::write(&path, schematic(sch)).unwrap();
        let mut board = pcb(pcb_parts);
        let note = board.adopt_schematic_dnp(&path);
        std::fs::remove_file(&path).ok();
        (board, note)
    }

    /// Both sides say DNP: DNP, nothing to report.
    #[test]
    fn schematic_and_pcb_agreeing_on_dnp_say_nothing() {
        let (b, note) = merged(&[("R1", true)], &[("R1", true)]);
        assert!(b.components[0].dnp);
        assert_eq!(note, None);
    }

    /// Schematic DNP, footprint fitted: DNP, and one note names the part and
    /// the fix.
    #[test]
    fn a_schematic_dnp_wins_over_a_fitted_footprint_with_one_note() {
        let (b, note) = merged(
            &[("R1", true), ("R2", true)],
            &[("R1", false), ("R2", true)],
        );
        assert!(b.components.iter().all(|c| c.dnp));
        assert_eq!(
            note.as_deref(),
            Some(
                "R1 is DNP in the schematic but fitted on the PCB; treated as not fitted. \
                 Update PCB from schematic to sync"
            )
        );
        let (_, plural) = merged(
            &[("R3", true), ("R4", true)],
            &[("R3", false), ("R4", false)],
        );
        assert!(plural
            .unwrap()
            .starts_with("R3, R4 are DNP in the schematic"));
    }

    /// Neither side marks it: fitted, no note. A DNP PCB footprint the
    /// schematic fits stays DNP (either side wins).
    #[test]
    fn unmarked_parts_stay_fitted_and_a_pcb_dnp_stays_dnp() {
        let (b, note) = merged(
            &[("R1", false), ("R2", false)],
            &[("R1", false), ("R2", true)],
        );
        assert!(!b.components[0].dnp);
        assert!(b.components[1].dnp);
        assert_eq!(note, None);
        // A reference the schematic does not place is untouched.
        let (b, note) = merged(&[("R1", true)], &[("R9", false)]);
        assert!(!b.components[0].dnp);
        assert_eq!(note, None);
    }

    /// The library symbol's Description and keywords reach the placed part.
    #[test]
    fn library_description_and_keywords_reach_the_placed_component() {
        let b = ExtractedBoard::from_kicad_schematic(&schematic(&[("D1", false)])).unwrap();
        let props = &b.component("D1").unwrap().properties;
        let get = |k: &str| {
            props
                .iter()
                .find(|(pk, _)| pk == k)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("Description"), Some("Bidirectional TVS, 5 V"));
        assert_eq!(get("ki_keywords"), Some("DIODE TVS"));
    }
}
