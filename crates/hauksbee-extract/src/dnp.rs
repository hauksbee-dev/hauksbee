//! What to do about Do-Not-Populate parts, decided explicitly and reported.
//!
//! DNP means the assembler does not place the part. In practice it gets used
//! for two opposite things:
//!
//! 1. "Not on the assembly BOM, but it will be there." A socketed module (an
//!    Arduino Nano, an ESP32 carrier) bought separately and plugged into
//!    headers; a footprint stuffed by hand later; a part fitted at rework.
//!    Analysing the board without it answers a question nobody asked.
//! 2. "This link is deliberately open." A 0R bridge between two ground
//!    planes, a solder jumper selecting a mode, a config strap. Fitting one of
//!    these merges nets that the designer split on purpose, and the tool would
//!    then report one ground plane on a board that has two.
//!
//! The board file is the authority on what is built, so the default leaves
//! every such part out of the circuit and every check: a part the source marks
//! DNP, excludes from the BOM or the board, or whose value field says
//! `DNP`/`NC`/`DNF` is not on the board that comes back from assembly, and
//! simulating it produced rail conflicts and absurd dissipation from parts that
//! do not exist. A part the user knows will be placed is named (`--fit R7`), or
//! another policy is chosen. Every run reports exactly which parts were fitted
//! and which were left open, so the choice is never silent.

use crate::{Component, ExtractedBoard};
use std::fmt::Write as _;

/// Resistance at or below which a two-terminal part counts as a link rather
/// than a component: fitting it merges the nets on either side instead of
/// putting a meaningful impedance between them.
const LINK_OHMS: f64 = 0.5;

/// How to treat DNP parts that were not named individually.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DnpPolicy {
    /// Fit DNP parts, except near-zero-ohm links: for a board whose DNP
    /// footprints are all placed eventually.
    FitExceptLinks,
    /// Fit every DNP part, links included.
    FitAll,
    /// Leave every DNP part open, matching what the board file says a fab
    /// house would build. The default.
    #[default]
    Honour,
}

impl DnpPolicy {
    /// The one-line description a report prints so the default is never a
    /// surprise.
    #[must_use]
    pub const fn describe(self) -> &'static str {
        match self {
            Self::FitExceptLinks => {
                "DNP parts are simulated as fitted (they are usually placed eventually), \
                 except near-zero-ohm links, which stay open because fitting one merges \
                 the nets it bridges"
            }
            Self::FitAll => "every DNP part is simulated as fitted, including links",
            Self::Honour => {
                "parts the board file marks not-fitted (DNP, or kept off the board) \
                 are left out of the simulation, as a fab house would build it"
            }
        }
    }
}

/// Property key under which [`ExtractedBoard::apply_dnp_policy`] records WHY a
/// left-open part is absent.
///
/// It exists so downstream classification
/// ([`crate::assembly::AssemblyState::of`]) can hand the reason back without a
/// side channel. The value is [`DnpReason::policy_tag`].
pub const DNP_REASON_KEY: &str = "hauksbee.dnp_reason";

/// Property key marking a component that was DNP in the source layout and is
/// present only because the default fit policy assumed it will be placed.
///
/// Presence-class lints (a pull-up "exists") read this so a
/// simulation convenience cannot silently satisfy an as-assembled claim: the
/// board that ships does NOT carry the part. A part the user explicitly named
/// as fitted carries no mark; that is a human decision about this build.
pub const DNP_FITTED_KEY: &str = "hauksbee.dnp_fitted_by_policy";

/// True when this component was DNP in the source and is present only by the
/// default fit policy (see [`DNP_FITTED_KEY`]).
#[must_use]
pub fn fitted_from_dnp_policy(c: &crate::Component) -> bool {
    c.properties.iter().any(|(key, _)| key == DNP_FITTED_KEY)
}

/// The phrase every "schematic says DNP, layout does not" note carries, so a
/// consumer can recognise the note without parsing the references.
pub const SCHEMATIC_ONLY_DNP_MARKER: &str = "DNP in the schematic but fitted on the PCB";

/// The note for parts the sibling schematic marks DNP while their layout
/// footprint does not: treated as not fitted, with the fix. `None` for an empty
/// list. One note names every such part.
#[must_use]
pub fn schematic_only_dnp_note(references: &[String]) -> Option<String> {
    let (first, rest) = references.split_first()?;
    let (subject, verb) = if rest.is_empty() {
        (first.clone(), "is")
    } else {
        (references.join(", "), "are")
    };
    Some(format!(
        "{subject} {verb} {SCHEMATIC_ONLY_DNP_MARKER}; treated as not fitted. \
         Update PCB from schematic to sync"
    ))
}

/// Property key set by the KiCad readers on a part the source excludes from the
/// BOM (`(in_bom no)` / `(attr exclude_from_bom)`).
pub const EXCLUDED_FROM_BOM_KEY: &str = "hauksbee.excluded_from_bom";

/// Property key set by the KiCad readers on a symbol the source keeps off the
/// board (`(on_board no)`, a netlist `exclude_from_board` property).
pub const EXCLUDED_FROM_BOARD_KEY: &str = "hauksbee.excluded_from_board";

/// True when the part is not on the bill of materials as built.
///
/// Either the source marks it DNP (and the fit policy only assumed it) or it
/// excludes it from the BOM. Such a part carries no ordering value, so value
/// checks skip it.
#[must_use]
pub fn unbought(c: &crate::Component) -> bool {
    c.dnp || fitted_from_dnp_policy(c) || excluded_by_source(c)
}

/// The source keeps this part off the BOM or off the board.
fn excluded_by_source(c: &crate::Component) -> bool {
    (c.properties.iter()).any(|(k, _)| k == EXCLUDED_FROM_BOM_KEY || k == EXCLUDED_FROM_BOARD_KEY)
}

/// A value field that states a populate decision instead of naming a part.
///
/// `DNP`, `DNI`, `DNF`, `NC`, `N/C`, `not fitted`, or such a word beside a
/// value (`10k (DNP)`, `DNP_100n`). "Stuff" is the other assembly verb for
/// populate (`NOSTUFF`, `DNS`, `do not stuff`), so its negations count too.
/// Word-bounded, so a part number that merely contains the letters never
/// matches.
#[must_use]
pub fn value_marks_dnp(value: &str) -> bool {
    let lower = value.trim().to_ascii_lowercase();
    let words: Vec<&str> = (lower.split(|c: char| !c.is_ascii_alphanumeric()))
        .filter(|w| !w.is_empty())
        .collect();
    let padded = format!(" {} ", words.join(" "));
    words
        .iter()
        .any(|w| matches!(*w, "dnp" | "dni" | "dnf" | "dns" | "nopop" | "nostuff"))
        || matches!(
            padded.as_str(),
            " nc " | " n c " | " no load " | " noload " | " unpopulated " | " unstuffed "
        )
        || [
            " do not populate ",
            " do not fit ",
            " do not install ",
            " do not place ",
            " not fitted ",
            " not populated ",
            " no fit ",
            " no stuff ",
            " do not stuff ",
            " not stuffed ",
        ]
        .iter()
        .any(|phrase| padded.contains(phrase))
}

/// Whether the source says this part is absent from the board that is built:
/// DNP, kept off the board (with a connection to judge), or a value field that
/// says so.
///
/// "Excluded from the BOM" is deliberately not on this list. It means the part
/// is not bought, not that it is not there: card-edge fingers, test pads, net
/// ties, copper keys and logos are fitted hardware that no assembler orders.
/// Only an explicit do-not-populate statement, or `on_board no`, removes a part.
fn source_marks_absent(c: &crate::Component) -> bool {
    let connected = || c.pins.iter().any(|p| p.net.is_some_and(|n| n != 0));
    let off_board = || {
        c.properties
            .iter()
            .any(|(k, _)| k == EXCLUDED_FROM_BOARD_KEY)
    };
    c.dnp || value_marks_dnp(&c.value) || (off_board() && connected())
}

/// Why one DNP part ended up fitted or open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DnpReason {
    /// The policy fits DNP parts and this one is not a link.
    Policy,
    /// Named by the caller (`--fit R7`, `fit = ["R7"]`).
    NamedFit,
    /// Named by the caller (`--no-fit R7`, `no_fit = ["R7"]`).
    NamedOpen,
    /// A near-zero-ohm link under the default policy: fitting it would merge
    /// the nets on either side, which is a topology decision, not an omission.
    ZeroOhmLink,
    /// The policy honours DNP.
    HonouredPolicy,
    /// An alternative footprint under the default policy: a DNP part whose
    /// every connection (three or more nets) is also made by a fitted part
    /// of the same kind, so it is a second-source placement of that part,
    /// not a missing one. Fitting both would put two copies of one part on
    /// the same nets.
    AlternativeFootprint,
    /// A plug-in module (compute module, `SoM`, dev board) that mates with a
    /// fitted connector or socket on the board, marked DNP because it is bought
    /// separately. The socket is fitted hardware's interface, so under the
    /// default policy the module is treated as plugged in unless the board
    /// marks the socket itself DNP.
    SocketedModule,
}

impl DnpReason {
    #[must_use]
    pub const fn describe(&self) -> &'static str {
        match self {
            Self::Policy => "DNP, fitted by this run's policy",
            Self::NamedFit => "DNP, fitted because you asked for it",
            Self::NamedOpen => "DNP, left open because you asked for it",
            Self::ZeroOhmLink => "DNP link (near 0 ohm), left open: fitting it merges nets",
            Self::HonouredPolicy => "marked not-fitted in the source, left open",
            Self::AlternativeFootprint => {
                "DNP alternative footprint of a fitted part on the same nets, left open"
            }
            Self::SocketedModule => {
                "DNP plug-in module on a fitted socket, treated as plugged in (--no-fit to leave it out)"
            }
        }
    }

    /// The stable tag stored under [`DNP_REASON_KEY`]. Round-trips through
    /// [`DnpReason::from_policy_tag`].
    #[must_use]
    pub const fn policy_tag(&self) -> &'static str {
        match self {
            Self::Policy => "policy",
            Self::NamedFit => "named-fit",
            Self::NamedOpen => "named-open",
            Self::ZeroOhmLink => "zero-ohm-link",
            Self::HonouredPolicy => "honoured-policy",
            Self::AlternativeFootprint => "alternative-footprint",
            Self::SocketedModule => "socketed-module",
        }
    }

    /// Parse a [`DNP_REASON_KEY`] tag back into the reason. `None` for an
    /// unknown tag: a stale or foreign value must degrade to "the board file
    /// says DNP", never invent a decision.
    #[must_use]
    pub fn from_policy_tag(tag: &str) -> Option<Self> {
        match tag {
            "policy" => Some(Self::Policy),
            "named-fit" => Some(Self::NamedFit),
            "named-open" => Some(Self::NamedOpen),
            "zero-ohm-link" => Some(Self::ZeroOhmLink),
            "honoured-policy" => Some(Self::HonouredPolicy),
            "alternative-footprint" => Some(Self::AlternativeFootprint),
            "socketed-module" => Some(Self::SocketedModule),
            _ => None,
        }
    }
}

/// One DNP part and what was decided about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DnpPart {
    pub reference: String,
    pub value: String,
    pub reason: DnpReason,
}

/// What a policy did to a board's DNP parts, for reporting.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DnpDecision {
    pub policy_line: String,
    pub fitted: Vec<DnpPart>,
    pub left_open: Vec<DnpPart>,
}

impl DnpDecision {
    /// True when the board had no DNP parts at all, so there is nothing to say.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.fitted.is_empty() && self.left_open.is_empty()
    }

    /// The lines a human-facing report prints. Empty when the board has no DNP
    /// parts, so a board without any never mentions the subject.
    #[must_use]
    pub fn lines(&self) -> Vec<String> {
        if self.is_empty() {
            return Vec::new();
        }
        let mut out = vec![format!("do-not-populate: {}", self.policy_line)];
        for p in &self.fitted {
            out.push(format!(
                "  fitted:    {} ({}), {}",
                p.reference,
                p.value,
                p.reason.describe()
            ));
        }
        for p in &self.left_open {
            out.push(format!(
                "  left open: {} ({}), {}",
                p.reference,
                p.value,
                p.reason.describe()
            ));
        }
        out
    }
}

/// The single token family that names a copper-link part: solder jumpers,
/// solder bridges, net ties. `is_zero_ohm_link` applies it to the value string
/// and `is_jumper_or_net_tie` to the library/footprint ids, so the two callers
/// can never drift apart on what counts as a link.
fn has_link_token(lower: &str) -> bool {
    lower.contains("jumper")
        || lower.contains("solderbridge")
        || lower.contains("solder_bridge")
        || lower.contains("net_tie")
        || lower.contains("nettie")
}

/// A part that is a solder jumper / solder bridge / net tie by CLASS: its
/// library, footprint, or value names it a link part. Such a part carries no
/// electrical value BY DESIGN (the value field is empty or a bare mnemonic),
/// so value-centric checks (the placeholder-value lint) must exempt it rather
/// than demand a value be "set". Covers the KiCad conventions
/// ("`Jumper:SolderJumper_2`_...", "NetTie-2_SMD_..."), vendor libraries that
/// spell the word out ("OLIMEX_Jumpers-FP:SJ_1_SMALLER"), and the Eagle
/// solder-jumper package family whose name is just "SJ" (the Arduino Uno's
/// RESET-EN is library "jumper", package "SJ", value "").
pub(crate) fn is_jumper_or_net_tie_fields(value: &str, lib_id: &str, footprint: &str) -> bool {
    let value = value.to_ascii_lowercase();
    let lib = lib_id.to_ascii_lowercase();
    let fp = footprint.to_ascii_lowercase();
    if has_link_token(&value) || has_link_token(&lib) || has_link_token(&fp) {
        return true;
    }
    // The Eagle/Olimex "SJ" package family carries no spelled-out "jumper"
    // token in the footprint itself: "SJ" (the Arduino Uno's RESET-EN),
    // "SJ_1_SMALLER". Accept only the bare "SJ" leaf or an "SJ_..." underscore
    // form: looser prefix matching would sweep in real part families that
    // merely start with those letters, e.g. CUI's SJ1-35xx / SJ-352x 3.5mm
    // audio jack footprints.
    let leaf = fp.rsplit([':', '/']).next().unwrap_or(fp.as_str());
    leaf == "sj" || leaf.starts_with("sj_")
}

/// Whether two independent KiCad/EAGLE fields explicitly describe the legacy
/// zero-ohm copper-link convention used by a small number of board footprints.
///
/// A zero-ohm value alone is not enough: ordinary resistor land patterns must
/// still be checked. The footprint must independently name the 0R class, which
/// covers layouts that embed auxiliary copper in a dedicated `0R_...` package
/// without guessing from reference designators.
pub(crate) fn is_explicit_zero_ohm_copper_link_fields(value: &str, footprint: &str) -> bool {
    let value = value.trim().to_ascii_lowercase();
    let value_names_zero = matches!(value.as_str(), "0" | "0r" | "0ohm" | "0ohms")
        || value.starts_with("0r(")
        || value.starts_with("0ohm(");
    let footprint = footprint.to_ascii_lowercase();
    let leaf = footprint
        .rsplit([':', '/'])
        .next()
        .unwrap_or(footprint.as_str());
    let footprint_names_zero =
        leaf == "0r" || leaf.starts_with("0r_") || leaf == "0ohm" || leaf.starts_with("0ohm_");
    value_names_zero && footprint_names_zero
}

/// EAGLE does not serialise a native net-tie component type. Recognise only the
/// exact, corpus-backed library/package conventions whose footprints contain
/// closed copper; generic `jumper` substring matching is deliberately absent.
/// A new vendor convention belongs here only with a real-board fixture.
pub(crate) fn is_eagle_copper_link_fields(value: &str, library: &str, package: &str) -> bool {
    if is_explicit_zero_ohm_copper_link_fields(value, package) {
        return true;
    }

    let library = library.to_ascii_lowercase();
    let package = package.to_ascii_lowercase();
    let library_leaf = library
        .rsplit([':', '/'])
        .next()
        .unwrap_or(library.as_str());
    let package_leaf = package
        .rsplit([':', '/'])
        .next()
        .unwrap_or(package.as_str());

    let arduino_sj = library_leaf == "jumper" && package_leaf == "sj";
    let sparkfun_closed_trace = library_leaf == "sparkfun-jumpers"
        && package_leaf.starts_with("smt-jumper_")
        && package_leaf.contains("_trace");

    arduino_sj || sparkfun_closed_trace
}

#[must_use]
pub fn is_jumper_or_net_tie(comp: &Component) -> bool {
    is_jumper_or_net_tie_fields(&comp.value, &comp.lib_id, &comp.footprint)
}

/// A two-terminal part whose value is a near-zero resistance, or a footprint
/// that only exists to bridge copper. Fitting one merges the nets it sits
/// between, so under the default policy it stays open.
#[must_use]
pub fn is_zero_ohm_link(comp: &Component) -> bool {
    if comp.pins.len() > 2 {
        return false;
    }
    let value = comp.value.trim();
    let lower = value.to_ascii_lowercase();
    // Named links carry no numeric value: "JUMPER", "SOLDER_BRIDGE", "NET_TIE".
    if has_link_token(&lower) {
        return true;
    }
    // Ferrite beads are a few milliohms at DC and are used as rail splits for
    // exactly the same reason a 0R is.
    let footprint = comp.footprint.to_ascii_lowercase();
    if lower.starts_with("fb") || footprint.contains("ferrite") || lower.contains("ferrite") {
        return true;
    }
    // A resistance at or under the link threshold: "0", "0R", "0.0", "R010".
    // The unit-less spellings need one more filter: "100n" or "22p" parses
    // with no unit and a sub-unity SI value, but no discrete resistor is ever
    // written with a bare p/n/µ multiplier; those are the capacitor BOM
    // spellings, and calling a DNP decoupling cap a "near 0 ohm link" is a
    // false statement about the board. When the part's own evidence ladder
    // says capacitor/inductor, it is not a link whatever the number parses to.
    if matches!(
        crate::part_class::classify_two_terminal(comp),
        crate::part_class::PartClass::Passive(
            hauksbee_models::schema::PassiveClass::Capacitor
                | hauksbee_models::schema::PassiveClass::Inductor
        )
    ) {
        return false;
    }
    match hauksbee_models::value::parse_value(value) {
        Some(v) => {
            let is_ohms = v.unit.as_deref().is_none_or(|u| {
                let u = u.to_ascii_lowercase();
                u == "r" || u == "ohm" || u == "ohms" || u == "\u{3a9}"
            });
            // A bare sub-unity SI-multiplier spelling with no unit ("100n",
            // "4u7") is a capacitance/inductance, not a milliohm resistor.
            let bare_si_multiplier = v.unit.is_none()
                && v.si < 1.0
                && value
                    .chars()
                    .any(|ch| matches!(ch, 'p' | 'n' | 'u' | 'µ' | 'P' | 'N' | 'U'));
            is_ohms && v.si <= LINK_OHMS && !bare_si_multiplier
        }
        None => false,
    }
}

impl ExtractedBoard {
    /// Decide which DNP parts to simulate, clear the flag on those, and return
    /// the record of what was done so every surface can report it.
    ///
    /// `fit` and `no_fit` name individual references and always win over the
    /// policy. An unknown reference in either list is an error: a typo must
    /// fail loudly rather than quietly change nothing.
    ///
    /// # Errors
    /// [`crate::ExtractError::UnknownReference`] when `fit` or `no_fit` names a
    /// reference not on the board, or names one reference in both lists.
    pub fn apply_dnp_policy(
        &mut self,
        policy: DnpPolicy,
        fit: &[String],
        no_fit: &[String],
    ) -> Result<DnpDecision, crate::ExtractError> {
        let unknown: Vec<&str> = fit
            .iter()
            .chain(no_fit.iter())
            .filter(|r| !self.components.iter().any(|c| &c.reference == *r))
            .map(std::string::String::as_str)
            .collect();
        if !unknown.is_empty() {
            // Did-you-mean, matching the net-facing flags (--probe/--ac-node):
            // a one-character typo should hand back the intended designator.
            let known: Vec<&str> = self
                .components
                .iter()
                .map(|c| c.reference.as_str())
                .collect();
            let mut msg = format!("unknown reference(s): {}.", unknown.join(", "));
            for u in &unknown {
                let near = nearest_references(u, &known, 3);
                if !near.is_empty() {
                    let _ = write!(msg, " '{u}': did you mean {}?", near.join(", "));
                }
            }
            msg.push_str(" Check the reference designators against the board.");
            return Err(crate::ExtractError::UnknownReference(msg));
        }
        if let Some(clash) = fit.iter().find(|r| no_fit.contains(r)) {
            return Err(crate::ExtractError::UnknownReference(format!(
                "{clash} is named as both fitted and left open; pick one"
            )));
        }

        // Re-application would decide from already-mutated state: the first
        // pass rewrites `comp.dnp`, so a second pass cannot see the source
        // truth and would silently no-op (a user asking to honour DNP after a
        // fit pass would get a fitted board and an empty decision report).
        // Refuse loudly instead; callers hold the original board if they
        // genuinely need to re-decide.
        if self.components.iter().any(|c| {
            c.properties
                .iter()
                .any(|(key, _)| key == DNP_REASON_KEY || key == DNP_FITTED_KEY)
        }) {
            return Err(crate::ExtractError::Corrupt(
                "a DNP policy was already applied to this board; applying another would \
                 decide from mutated state, not the source layout. Re-extract the board \
                 and apply the intended policy once."
                    .to_string(),
            ));
        }
        let mut decision = DnpDecision {
            policy_line: policy.describe().to_string(),
            ..Default::default()
        };
        // Every reader's own way of saying "not built here" becomes the one
        // flag the rest of the pipeline reads.
        for comp in &mut self.components {
            comp.dnp = source_marks_absent(comp);
        }
        let alternatives = alternative_footprints(&self.components);
        let modules = if policy == DnpPolicy::Honour {
            socketed_modules(&self.components)
        } else {
            std::collections::BTreeSet::new()
        };
        for comp in &mut self.components {
            // An explicit `no_fit` is also the assembly-variant seam: the
            // layout may describe the superset of placements and carry no DNP
            // flag for a part omitted from this build. In that case the named
            // decision still has to make the component electrically absent.
            // Policy-only decisions continue to apply only to layout DNPs.
            let explicitly_open = no_fit.contains(&comp.reference);
            if !comp.dnp && !explicitly_open {
                continue;
            }
            let reason = if fit.contains(&comp.reference) {
                DnpReason::NamedFit
            } else if no_fit.contains(&comp.reference) {
                DnpReason::NamedOpen
            } else {
                policy_reason(policy, comp, &alternatives, &modules)
            };
            let part = DnpPart {
                reference: comp.reference.clone(),
                value: comp.value.clone(),
                reason: reason.clone(),
            };
            // Record the decision ON the component: a stale tag from an earlier
            // policy application is always stripped, and a left-open part keeps
            // its reason where `assembly::AssemblyState::of` can read it back.
            comp.properties
                .retain(|(key, _)| key != DNP_REASON_KEY && key != DNP_FITTED_KEY);
            match reason {
                DnpReason::Policy | DnpReason::NamedFit | DnpReason::SocketedModule => {
                    // A policy fit of a source-DNP part is a simulation
                    // assumption, not an assembly fact; mark it so
                    // presence-class lints can tell the two apart. A NamedFit
                    // is the user's own build decision and stays unmarked.
                    if comp.dnp && matches!(reason, DnpReason::Policy | DnpReason::SocketedModule) {
                        comp.properties
                            .push((DNP_FITTED_KEY.to_string(), "policy".to_string()));
                    }
                    comp.dnp = false;
                    decision.fitted.push(part);
                }
                _ => {
                    comp.dnp = true;
                    comp.properties
                        .push((DNP_REASON_KEY.to_string(), reason.policy_tag().to_string()));
                    decision.left_open.push(part);
                }
            }
        }
        Ok(decision)
    }
}

/// The reason a DNP part the user did not name is fitted or left open under
/// `policy`.
fn policy_reason(
    policy: DnpPolicy,
    comp: &Component,
    alternatives: &std::collections::BTreeSet<String>,
    modules: &std::collections::BTreeSet<String>,
) -> DnpReason {
    match policy {
        DnpPolicy::Honour if modules.contains(&comp.reference) => DnpReason::SocketedModule,
        DnpPolicy::Honour => DnpReason::HonouredPolicy,
        DnpPolicy::FitAll => DnpReason::Policy,
        DnpPolicy::FitExceptLinks => {
            if is_zero_ohm_link(comp) {
                DnpReason::ZeroOhmLink
            } else if alternatives.contains(&comp.reference) {
                DnpReason::AlternativeFootprint
            } else {
                DnpReason::Policy
            }
        }
    }
}

/// References of DNP plug-in modules that mate with a fitted connector or
/// socket on the board.
///
/// A compute module, `SoM` or dev board is bought separately and plugged into its
/// socket, so the assembly BOM marks it DNP while the socket is fitted. Leaving
/// it out deletes the board's host: its pins vanish from the pull-up, back-power
/// and bus checks and the loads it drives look unpowered. A DNP part qualifies
/// when
/// - it is a module by what its record says (`part_class::is_plugin_module`),
///   not a connector, test point or IC package, with six or more connected pads;
/// - a fitted connector or socket shares at least half of its connected nets
///   (two at least), which is the interface it plugs into;
/// - no connector or socket sharing its nets is itself marked DNP: the board
///   then says the socket is empty.
fn socketed_modules(components: &[Component]) -> std::collections::BTreeSet<String> {
    use std::collections::BTreeSet;
    let nets = |c: &Component| -> BTreeSet<i64> {
        c.pins
            .iter()
            .filter_map(|p| p.net)
            .filter(|&n| n != 0)
            .collect()
    };
    let socketlike =
        |c: &Component| crate::is_connector_like(c) && !crate::is_test_point_or_hole(c);
    let mut out = BTreeSet::new();
    for module in components.iter().filter(|c| c.dnp) {
        if socketlike(module) || !crate::part_class::is_plugin_module(module) {
            continue;
        }
        let own = nets(module);
        if own.len() < 6 || crate::part_class::connected_pads(module) < 6 {
            continue;
        }
        let shared = |c: &Component| nets(c).intersection(&own).count();
        let enough = |n: usize| n >= 2 && n * 2 >= own.len();
        let sockets: Vec<&Component> = components
            .iter()
            .filter(|c| c.reference != module.reference && socketlike(c) && enough(shared(c)))
            .collect();
        // Together the fitted sockets must cover half the module's nets (a
        // module with two edge connectors is split across both).
        let covered: BTreeSet<i64> = sockets
            .iter()
            .filter(|c| !c.dnp)
            .flat_map(|c| nets(c))
            .filter(|n| own.contains(n))
            .collect();
        let any_dnp_socket = sockets.iter().any(|c| c.dnp);
        if !any_dnp_socket && enough(covered.len()) {
            out.insert(module.reference.clone());
        }
    }
    out
}

/// References of DNP parts that are alternative footprints of a fitted part:
/// three or more connected nets, every one of which a single non-DNP part with
/// the same designator prefix (`U`, `Q`, ...) also connects. Two placements
/// of one part on the same nets are a second-source or package option; only
/// one is ever assembled.
fn alternative_footprints(components: &[Component]) -> std::collections::BTreeSet<String> {
    use std::collections::BTreeSet;
    let prefix = |r: &str| -> String {
        r.chars()
            .take_while(char::is_ascii_alphabetic)
            .collect::<String>()
            .to_ascii_uppercase()
    };
    let nets = |c: &Component| -> BTreeSet<i64> {
        c.pins
            .iter()
            .filter_map(|p| p.net)
            .filter(|&n| n != 0)
            .collect()
    };
    components
        .iter()
        .filter(|c| c.dnp)
        .filter(|c| {
            let own = nets(c);
            let pre = prefix(&c.reference);
            own.len() >= 3
                && !pre.is_empty()
                && components.iter().any(|o| {
                    !o.dnp
                        && o.reference != c.reference
                        && prefix(&o.reference) == pre
                        && own.is_subset(&nets(o))
                })
        })
        .map(|c| c.reference.clone())
        .collect()
}

/// Closest reference designators to a mistyped one, case-insensitive, capped at
/// `limit`, with a distance cutoff so wild typos suggest nothing rather than
/// noise. Small and local: the engine's net-name matcher is not visible from
/// this crate.
fn nearest_references(target: &str, known: &[&str], limit: usize) -> Vec<String> {
    fn dist(a: &str, b: &str) -> usize {
        let a: Vec<char> = a.chars().collect();
        let b: Vec<char> = b.chars().collect();
        let mut row: Vec<usize> = (0..=b.len()).collect();
        for (i, ca) in a.iter().enumerate() {
            let mut prev = row[0];
            row[0] = i + 1;
            for (j, cb) in b.iter().enumerate() {
                let cur = row[j + 1];
                row[j + 1] = (prev + usize::from(ca != cb)).min(row[j] + 1).min(cur + 1);
                prev = cur;
            }
        }
        row[b.len()]
    }
    let t = target.to_ascii_lowercase();
    let mut scored: Vec<(usize, &str)> = known
        .iter()
        .map(|k| (dist(&t, &k.to_ascii_lowercase()), *k))
        .collect();
    scored.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(b.1)));
    let cutoff = (target.len() / 2).max(2);
    scored
        .into_iter()
        .filter(|(d, _)| *d <= cutoff)
        .take(limit)
        .map(|(_, k)| k.to_string())
        .collect()
}

#[cfg(test)]
mod nearest_reference_tests {
    use super::nearest_references;

    #[test]
    fn one_char_typo_suggests_the_designator() {
        let known = ["R7", "R71", "C3", "U1"];
        let near = nearest_references("R17", &known, 3);
        assert!(near.contains(&"R71".to_string()) || near.contains(&"R7".to_string()));
        assert!(nearest_references("XYZZY99", &known, 3).is_empty());
    }
}

#[cfg(test)]
mod alternative_footprint_tests {
    use super::{alternative_footprints, DnpPolicy, DnpReason};
    use crate::testutil::{board, part, pin};

    fn ic(reference: &str, nets: &[i64], dnp: bool) -> crate::Component {
        let pins = nets
            .iter()
            .enumerate()
            .map(|(i, &n)| pin(&(i + 1).to_string(), Some(n)))
            .collect();
        let mut c = part(reference, "XCVR", "lib:XCVR", "SO8", pins);
        c.dnp = dnp;
        c
    }

    /// A DNP part whose every net a fitted part of the same kind also
    /// connects is a second-source footprint, left open by default; a DNP
    /// part with a connection of its own, a different designator class, or
    /// too few nets to tell is an ordinary DNP part.
    #[test]
    fn a_dnp_second_footprint_on_a_fitted_parts_nets_is_left_open() {
        let parts = vec![
            ic("U2", &[1, 2, 3, 4, 5], false),
            ic("U6", &[1, 2, 3, 5], true),
            ic("U8", &[1, 2, 3, 9], true),
            ic("Q3", &[1, 2, 3], true),
            ic("U9", &[1, 2], true),
        ];
        let alt = alternative_footprints(&parts);
        assert_eq!(alt.into_iter().collect::<Vec<_>>(), ["U6"]);

        let nets: Vec<(i64, &str)> = (1..=9).map(|i| (i, "N")).collect();
        let mut b = board(&nets, parts);
        let decision = b
            .apply_dnp_policy(DnpPolicy::FitExceptLinks, &[], &[])
            .expect("policy applies");
        let u6 = b.components.iter().find(|c| c.reference == "U6").unwrap();
        let u8 = b.components.iter().find(|c| c.reference == "U8").unwrap();
        assert!(u6.dnp, "the alternative footprint stays open");
        assert!(!u8.dnp, "an ordinary DNP part is fitted by this policy");
        assert!(format!("{decision:?}").contains("U6"));
        assert_eq!(
            DnpReason::from_policy_tag(DnpReason::AlternativeFootprint.policy_tag()),
            Some(DnpReason::AlternativeFootprint)
        );
    }
}

#[cfg(test)]
mod jumper_class_tests {
    use super::is_jumper_or_net_tie;
    use crate::Component;

    fn comp(reference: &str, value: &str, lib_id: &str, footprint: &str) -> Component {
        crate::testutil::part(reference, value, lib_id, footprint, Vec::new())
    }

    #[test]
    fn link_class_parts_are_recognised_by_library_and_footprint() {
        // The Arduino Uno R3 RESET-EN: Eagle library "jumper", package "SJ",
        // value "" - the shape that false-fired the placeholder-value lint.
        assert!(is_jumper_or_net_tie(&comp(
            "RESET-EN",
            "",
            "jumper:SJ",
            "SJ"
        )));
        // KiCad solder-jumper and net-tie footprint conventions.
        assert!(is_jumper_or_net_tie(&comp(
            "JP1",
            "",
            "Jumper:SolderJumper_2_P1.3mm_Open",
            "Jumper:SolderJumper_2_P1.3mm_Open_RoundedPad1.0x1.5mm"
        )));
        assert!(is_jumper_or_net_tie(&comp(
            "NT1",
            "",
            "Device:Net-Tie_2",
            "NetTie:NetTie-2_SMD_Pad0.5mm"
        )));
        // Olimex's own solder-jumper library (RP2040-PICO-PC, ESP32-EVB).
        assert!(is_jumper_or_net_tie(&comp(
            "SJ1",
            "",
            "OLIMEX_Jumpers:SJ_1",
            "OLIMEX_Jumpers-FP:SJ_1_SMALLER"
        )));
        // Value-named links still count (the is_zero_ohm_link token family).
        assert!(is_jumper_or_net_tie(&comp("J9", "SOLDER_BRIDGE", "", "")));
    }

    #[test]
    fn ordinary_parts_are_not_link_class() {
        // A genuine unset resistor must NOT be exempted.
        assert!(!is_jumper_or_net_tie(&comp(
            "R1",
            "",
            "Device:R",
            "Resistor_SMD:R_0603_1608Metric"
        )));
        // "SJ" must be the bare package name or an "SJ_..." form, not a loose
        // prefix: part families that merely start with those letters (CUI's
        // SJ1-35xx 3.5mm audio jacks) are not solder jumpers.
        assert!(!is_jumper_or_net_tie(&comp(
            "U1",
            "SJW1240",
            "",
            "SJW1240-QFN"
        )));
        assert!(!is_jumper_or_net_tie(&comp(
            "J2",
            "3.5mm jack",
            "",
            "SJ1-3533NG"
        )));
        assert!(!is_jumper_or_net_tie(&comp(
            "J3",
            "3.5mm jack",
            "",
            "SJ-3523-SMT"
        )));
        assert!(!is_jumper_or_net_tie(&comp(
            "C3", "100n", "Device:C", "C_0402"
        )));
    }

    /// "100n" parses unit-less to 1e-7, which sat under the link threshold
    /// and called every DNP decoupling cap a "near 0 ohm link" (printed as a
    /// false statement about the board on real fixtures). Capacitor spellings
    /// and capacitor-classified parts are never links; real link spellings
    /// still are.
    #[test]
    fn capacitor_values_are_not_zero_ohm_links() {
        use super::is_zero_ohm_link;
        let two_pins = |mut c: Component| {
            c.pins = vec![
                crate::Pin {
                    number: "1".into(),
                    net: Some(1),
                    ..Default::default()
                },
                crate::Pin {
                    number: "2".into(),
                    net: Some(2),
                    ..Default::default()
                },
            ];
            c
        };
        for value in ["100n", "22p", "10u", "4u7", "1n"] {
            assert!(
                !is_zero_ohm_link(&two_pins(comp("C16", value, "Device:C", "C_0402"))),
                "a {value} capacitor is not a near-zero-ohm link"
            );
        }
        for value in ["0", "0R", "0.0", "R010", "0R0"] {
            assert!(
                is_zero_ohm_link(&two_pins(comp("R7", value, "Device:R", "R_0402"))),
                "{value} is a link"
            );
        }
    }
}

#[cfg(test)]
mod source_absence_tests {
    use super::{
        unbought, value_marks_dnp, DnpPolicy, DnpReason, EXCLUDED_FROM_BOARD_KEY,
        EXCLUDED_FROM_BOM_KEY,
    };
    use crate::testutil::{board, part, pin};

    /// Legal control: real values and part numbers are never read as a populate
    /// decision, however many of the letters they share.
    #[test]
    fn real_values_are_not_dnp_marks() {
        for value in [
            "10k",
            "100n",
            "LM1117-3.3",
            "DNP3-FUSE",
            "ATmega328P",
            "BNC",
            "NCP1117",
            "1N4148",
            "STUFF",
            "STUFFING-1",
            "",
        ] {
            assert!(!value_marks_dnp(value), "{value:?} names a part");
        }
    }

    #[test]
    fn value_fields_that_state_a_populate_decision_are_marks() {
        for value in [
            "DNP",
            "dnf",
            "NC",
            "N/C",
            "10k (DNP)",
            "DNP_100n",
            "Do not populate",
            "not fitted",
            "NOSTUFF",
            "DNS",
            "10k NO STUFF",
            "do not stuff",
        ] {
            assert!(value_marks_dnp(value), "{value:?} says not fitted");
        }
    }

    fn two_regulators() -> crate::ExtractedBoard {
        let nets = [(1, "GND"), (2, "+5V"), (3, "+3V3")];
        let reg = |r: &str, value: &str| {
            let pins = vec![pin("1", Some(1)), pin("2", Some(3)), pin("3", Some(2))];
            part(r, value, "lib:LDO", "SOT-223", pins)
        };
        board(
            &nets,
            vec![reg("U1", "AMS1117-3.3"), reg("U2", "LM1117-3.3")],
        )
    }

    /// Faulty: a second regulator the source keeps off the build (by value
    /// field or board exclusion, not a `dnp` flag) is left out by the default
    /// policy, so two sources never share a rail; the fitted one and the
    /// named-fit override are unaffected.
    #[test]
    fn the_default_leaves_out_every_kind_of_source_exclusion() {
        assert_eq!(DnpPolicy::default(), DnpPolicy::Honour);
        for how in ["value", "board"] {
            let mut b = two_regulators();
            let u2 = b
                .components
                .iter_mut()
                .find(|c| c.reference == "U2")
                .unwrap();
            match how {
                "value" => u2.value = "DNP".into(),
                _ => u2
                    .properties
                    .push((EXCLUDED_FROM_BOARD_KEY.into(), String::new())),
            }
            let d = b.apply_dnp_policy(DnpPolicy::default(), &[], &[]).unwrap();
            assert_eq!(d.left_open.len(), 1, "{how}: {d:?}");
            assert_eq!(d.left_open[0].reference, "U2", "{how}");
            assert_eq!(d.left_open[0].reason, DnpReason::HonouredPolicy, "{how}");
            assert!(b.component("U2").unwrap().dnp, "{how}");
            assert!(!b.component("U1").unwrap().dnp, "{how}");
        }
        let mut b = two_regulators();
        b.components[1].value = "DNP".into();
        let d = b
            .apply_dnp_policy(DnpPolicy::default(), &["U2".into()], &[])
            .unwrap();
        assert_eq!(d.fitted[0].reason, DnpReason::NamedFit);
        assert!(!b.component("U2").unwrap().dnp);
    }

    /// Legal control: "excluded from the BOM" is not "not fitted". Card-edge
    /// fingers, test pads, net ties and the like are on the board but are not
    /// bought, so a connected part with only the BOM flag stays fitted and
    /// still counts as unbought for the ordering checks. Its twin with a real
    /// DNP marking is left open.
    #[test]
    fn a_bom_excluded_connected_part_is_still_fitted() {
        let mut b = two_regulators();
        let mut edge = part(
            "P1",
            "Edge",
            "lib:CardEdge_Fingers",
            "Edge",
            vec![pin("1", Some(1))],
        );
        edge.properties
            .push((EXCLUDED_FROM_BOM_KEY.into(), String::new()));
        b.components.push(edge);
        let d = b.apply_dnp_policy(DnpPolicy::default(), &[], &[]).unwrap();
        assert!(d.is_empty(), "{d:?}");
        let p1 = b.component("P1").unwrap();
        assert!(!p1.dnp);
        assert!(unbought(p1), "still not on the bill of materials");

        let mut b = two_regulators();
        let mut tp = part(
            "TP3",
            "TP",
            "lib:TestPoint_Pad",
            "TP",
            vec![pin("1", Some(1))],
        );
        tp.properties
            .push((EXCLUDED_FROM_BOM_KEY.into(), String::new()));
        tp.value = "DNP".into();
        b.components.push(tp);
        let d = b.apply_dnp_policy(DnpPolicy::default(), &[], &[]).unwrap();
        assert_eq!(d.left_open.len(), 1, "{d:?}");
        assert_eq!(d.left_open[0].reference, "TP3");
    }

    /// Legal control: a BOM-excluded part with no connection (a fiducial, a
    /// mounting hole) is inert and produces no decision line.
    #[test]
    fn an_unconnected_bom_excluded_part_is_left_alone() {
        let mut b = two_regulators();
        let mut h = part("H1", "MountingHole", "lib:H", "Hole", vec![pin("1", None)]);
        h.properties
            .push((EXCLUDED_FROM_BOM_KEY.into(), String::new()));
        b.components.push(h);
        let d = b.apply_dnp_policy(DnpPolicy::default(), &[], &[]).unwrap();
        assert!(d.is_empty(), "{d:?}");
        assert!(!b.component("H1").unwrap().dnp);
    }
}
