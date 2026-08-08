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
//!
//! Long-form how-and-why: docs/how-and-why/hauksbee-extract/README.md (the
//! crate tour) and docs/how-and-why/hauksbee-extract/netlist.md (the
//! canonical form defined here).

pub mod altium;
pub mod assembly;
pub mod bom;
pub mod dnp;
pub mod drc;
mod eagle;
pub mod gerber;
pub mod ipc2581;
mod ipc356;
mod netlint;
mod netlist;
pub mod netname;
pub mod odbpp;
mod part_class;
pub use part_class::is_plain_resistor;
mod pcb;
pub mod placement;
mod protel_ascii;
pub mod reader;
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
    run_drc_with_clearance_rules, ClearanceRules, DrcFinding, DrcReport, Item, ItemKind,
    NetClassRule, ViolationKind, DEFAULT_CLEARANCE_MM, SHORT_TOUCH_EPS_MM,
};
pub use netlint::{render_netlint, LintCheck, LintFinding, NetLintReport, Severity};
pub use si::{
    cl_board_pf, cl_series, i2c_rise_time_ns, render_si, routed_length_mm, SiCheck, SiFinding,
    SiReport, SiSeverity,
};
pub use trace_current::{
    audit_trace_currents, ipc2221_ampacity, ipc2221_min_width_mm, net_copper_from_root,
    net_copper_from_text, render_trace_capacity_report, trace_capacity_report, CopperKind,
    NetCopper, TraceAudit, TraceCapacityRow, TraceCurrentFinding,
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
    #[error("not a {expected} file (root is {found:?})")]
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

/// One electrical net. `id` is the KiCad net number (0 = the unconnected
/// net in PCB files); `name` like "GND", "/Debugger/nRF52_VDD".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Net {
    pub id: i64,
    pub name: String,
}

/// A component pin/pad connection point.
#[derive(Debug, Clone, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Component {
    /// Reference designator ("R1", "U101").
    pub reference: String,
    /// Value field ("10k", "BCM857BS").
    pub value: String,
    /// Symbol or footprint library id ("Device:R",
    /// "Resistor_SMD:R_0402_1005Metric").
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

/// The extraction result: everything the binder and renderer need to know
/// about what the board is, electrically.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractedBoard {
    pub name: String,
    pub nets: Vec<Net>,
    pub components: Vec<Component>,
}

impl ExtractedBoard {
    pub fn from_kicad_pcb(text: &str) -> Result<Self, ExtractError> {
        pcb::extract(text)
    }

    pub fn from_kicad_netlist(text: &str) -> Result<Self, ExtractError> {
        netlist::extract(text)
    }

    /// KiCad s-expression schematic (`.kicad_sch`, KiCad 6 through 10). The
    /// netlist is derived geometrically from wires, pins, junctions, labels
    /// and power symbols, since a schematic carries no copper. Sub-sheets are
    /// not followed (single sheet only); use [`Self::from_kicad_schematic_path`]
    /// to recurse a hierarchy that lives in sibling files.
    pub fn from_kicad_schematic(text: &str) -> Result<Self, ExtractError> {
        schematic::extract(text)
    }

    /// KiCad schematic from a file path, recursing into the sheet hierarchy
    /// (sub-sheets resolved relative to the file's directory).
    pub fn from_kicad_schematic_path(path: &Path) -> Result<Self, ExtractError> {
        schematic::extract_from_path(path)
    }

    /// Geometric copper short / clearance check on the raw board text.
    ///
    /// Connectivity extraction works off pad nets alone, but a *short* is a
    /// geometric fact (two nets' copper touching) that only the layout carries.
    /// This dispatches on the file content and runs the same detection engine
    /// over either a KiCad `.kicad_pcb` ([`drc::run_drc`]) or an Eagle `.brd`
    /// ([`drc::eagle_drc_from_text`]); an unrecognised format returns an empty
    /// report.
    pub fn drc(text: &str) -> Result<drc::DrcReport, ExtractError> {
        Self::drc_with_clearance(text, None)
    }

    /// [`Self::drc`] with an explicit copper clearance rule (mm) for the KiCad
    /// path. KiCad 10 (format 20260206) stores the design-rule clearance in the
    /// sibling `.kicad_pro` (`net_settings.classes[].clearance`), not the
    /// `.kicad_pcb`, so a caller that knows the board path can read the
    /// Default-class clearance and pass it here. `None` keeps the board's own /
    /// default behaviour. The Eagle path reads its own rules and ignores this.
    pub fn drc_with_clearance(
        text: &str,
        clearance_override: Option<f64>,
    ) -> Result<drc::DrcReport, ExtractError> {
        Self::drc_with_clearance_rules(text, clearance_override.map(drc::ClearanceRules::new))
    }

    /// [`Self::drc`] with project-derived per-net clearance rules for KiCad
    /// boards. Other input formats keep their own scalar-rule behavior.
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
    pub fn fit(&mut self, refs: &[String]) -> Result<usize, ExtractError> {
        let decision = self.apply_dnp_policy(dnp::DnpPolicy::Honour, refs, &[])?;
        Ok(decision.fitted.len())
    }

    /// IPC-D-356/356A fab netlist: the universal fallback any EDA exports.
    pub fn from_ipc_d356(text: &str) -> Result<Self, ExtractError> {
        ipc356::extract(text)
    }

    /// Altium Designer `.PcbDoc` (binary OLE2 / Compound File Binary). Reads the
    /// nets, components and pads (with their net assignment) straight out of the
    /// `Nets6` / `Components6` / `Pads6` record streams. The layout carries full
    /// net connectivity, so the board file alone fully describes the circuit,
    /// the same way a `.kicad_pcb` does. See [`altium`] and `docs/ingest/ALTIUM.md`.
    pub fn from_altium_pcb(bytes: &[u8]) -> Result<Self, ExtractError> {
        altium::extract(bytes)
    }

    /// ODB++ (Siemens/Valor) design archive: a directory, a `.tgz` or a `.zip`.
    /// Reads nets, components and pads from the job's own EDA data rather than
    /// reverse-engineering them from copper, and cross-checks that data against
    /// the job's CAD netlist. See [`odbpp`] for the accounting this discards
    /// ([`odbpp::OdbExtraction::stats`] keeps it) and what is deliberately not
    /// modelled.
    pub fn from_odbpp(path: &Path) -> Result<Self, ExtractError> {
        odbpp::from_odbpp(path).map(|e| e.board)
    }

    /// ODB++ from archive bytes (`.tgz` / `.tar` / `.zip`), for the web path
    /// that has an upload rather than a path.
    pub fn from_odbpp_archive(bytes: &[u8]) -> Result<Self, ExtractError> {
        odbpp::from_odbpp_archive(bytes).map(|e| e.board)
    }

    /// IPC-2581 (DPMX) design-exchange XML, revision B or C, namespaced or not.
    /// See [`ipc2581`]; [`ipc2581::extract`] keeps the read's accounting.
    pub fn from_ipc2581(text: &str) -> Result<Self, ExtractError> {
        ipc2581::extract(text).map(|e| e.board)
    }

    /// ASCII Protel board export (`|RECORD=Board|KIND=Protel_Advanced_PCB`
    /// pipe-delimited text): the `.pcbdoc` form EasyEDA and several converters
    /// produce instead of Altium Designer's binary OLE2 container. Reads nets,
    /// components, pads and comment texts; carries no copper geometry for DRC.
    pub fn from_protel_ascii(text: &str) -> Result<Self, ExtractError> {
        protel_ascii::extract(text)
    }

    /// Altium `.PcbDoc` geometric short / clearance DRC, the binary-format twin
    /// of [`Self::drc`]. Reads copper geometry (tracks, arcs, vias, pads,
    /// polygons) per net and feeds the same detection engine the KiCad and Eagle
    /// paths use.
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
    pub fn from_auto_bytes(bytes: &[u8]) -> Option<Result<Self, ExtractError>> {
        let registry = reader::Registry::builtin();
        registry
            .detect_binary(bytes, None)
            .map(|r| r.read(bytes, None))
    }

    /// Extract from an already-parsed forge-sexpr [`Document`], avoiding a
    /// re-parse when the caller has already built the CST (e.g. for lossless
    /// editing). Dispatches on the root keyword: `kicad_pcb` → layout
    /// extraction, `export` → netlist extraction.
    ///
    /// [`Document`]: forge_sexpr::Document
    pub fn from_document(doc: &forge_sexpr::Document) -> Result<Self, ExtractError> {
        match doc.root().and_then(|r| r.name()) {
            Some("kicad_pcb") | Some("module") => pcb::extract_from_doc(doc),
            Some("export") => netlist::extract_from_doc(doc),
            // Schematic: single sheet only here (no directory to find
            // sub-sheets from). The path-based entry point recurses.
            Some("kicad_sch") => schematic::extract_from_doc(doc, None),
            other => Err(ExtractError::WrongRoot {
                expected: "kicad_pcb, export or kicad_sch",
                found: other.map(str::to_string),
            }),
        }
    }

    /// Sniff the format from content and extract accordingly.
    ///
    /// Delegates to the [`reader::Registry`]: each format is a
    /// [`reader::BoardReader`] that owns its own detection, rather than one
    /// hard-coded substring ladder. An input no reader recognises fails with
    /// [`ExtractError::Unrecognized`], whose message describes the accepted
    /// formats in user words (see [`reader::unrecognized_message`]).
    pub fn from_auto(text: &str) -> Result<Self, ExtractError> {
        reader::Registry::builtin().read(text.as_bytes(), None)
    }

    pub fn net(&self, id: i64) -> Option<&Net> {
        self.nets.iter().find(|n| n.id == id)
    }

    pub fn net_by_name(&self, name: &str) -> Option<&Net> {
        self.nets.iter().find(|n| n.name == name)
    }

    pub fn component(&self, reference: &str) -> Option<&Component> {
        self.components.iter().find(|c| c.reference == reference)
    }

    /// (component, pin) pairs attached to a net.
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
pub(crate) fn unescape_kicad_name(s: &str) -> String {
    if !s.contains('{') {
        return s.to_string();
    }
    const TABLE: &[(&str, &str)] = &[
        ("{dblquote}", "\""),
        ("{quote}", "'"),
        ("{lt}", "<"),
        ("{gt}", ">"),
        ("{backslash}", "\\"),
        ("{slash}", "/"),
        ("{bar}", "|"),
        ("{colon}", ":"),
        ("{space}", " "),
        ("{dollar}", "$"),
        ("{tab}", "\t"),
        ("{return}", "\n"),
        // `{brace}` last: a `{` restored earlier must not start a new token.
        ("{brace}", "{"),
    ];
    let mut out = s.to_string();
    for (token, ch) in TABLE {
        if out.contains(token) {
            out = out.replace(token, ch);
        }
    }
    out
}

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

pub(crate) fn merge_duplicate_references(components: Vec<Component>) -> Vec<Component> {
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
        if !previous.value.is_empty()
            && !incoming.value.is_empty()
            && previous.value != incoming.value
        {
            conflicts.push(format!(
                "values differ ('{}' versus '{}')",
                previous.value, incoming.value
            ));
        }
        if !previous.footprint.is_empty()
            && !incoming.footprint.is_empty()
            && previous.footprint != incoming.footprint
        {
            conflicts.push(format!(
                "footprints differ ('{}' versus '{}')",
                previous.footprint, incoming.footprint
            ));
        }
        if !previous.lib_id.is_empty()
            && !incoming.lib_id.is_empty()
            && previous.lib_id != incoming.lib_id
        {
            conflicts.push(format!(
                "library ids differ ('{}' versus '{}')",
                previous.lib_id, incoming.lib_id
            ));
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
                        "property '{}' differs ('{}' versus '{}')",
                        previous_key, previous_value, incoming_value
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
            previous.value = incoming.value.clone();
        }
        if previous.footprint.is_empty() && !incoming.footprint.is_empty() {
            previous.footprint = incoming.footprint.clone();
        }
        if previous.lib_id.is_empty() && !incoming.lib_id.is_empty() {
            previous.lib_id = incoming.lib_id.clone();
        }
        if previous.position.is_none() {
            previous.position = incoming.position;
        }
        if previous.layer.is_empty() && !incoming.layer.is_empty() {
            previous.layer = incoming.layer.clone();
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
                    existing.function = pin.function.clone();
                } else if pin.function.is_empty() {
                    pin.function = existing.function.clone();
                }
                if existing.kind.is_empty() {
                    existing.kind = pin.kind.clone();
                } else if pin.kind.is_empty() {
                    pin.kind = existing.kind.clone();
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
        let index = match group_index.get(&reference).copied() {
            Some(index) => index,
            None => {
                let index = groups.len();
                groups.push((reference.clone(), Vec::new()));
                group_index.insert(reference, index);
                index
            }
        };
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

#[cfg(test)]
mod duplicate_reference_merge_tests {
    use super::*;

    fn component(
        reference: &str,
        value: &str,
        footprint: &str,
        properties: Vec<(&str, &str)>,
        pins: Vec<(&str, Option<i64>)>,
    ) -> Component {
        Component {
            reference: reference.into(),
            value: value.into(),
            lib_id: footprint.into(),
            footprint: footprint.into(),
            position: None,
            layer: String::new(),
            properties: properties
                .into_iter()
                .map(|(key, value)| (key.into(), value.into()))
                .collect(),
            dnp: false,
            pins: pins
                .into_iter()
                .map(|(number, net)| Pin {
                    number: number.into(),
                    net,
                    function: String::new(),
                    kind: String::new(),
                    position: None,
                })
                .collect(),
        }
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
    pub fn is_clean(&self) -> bool {
        self.undeclared_nets.is_empty() && self.floating_components.is_empty()
    }
}
