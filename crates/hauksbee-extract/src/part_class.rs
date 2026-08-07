//! What a two-terminal part actually *is*, answered from model evidence rather
//! than from the reference designator the CAD user typed.
//!
//! Several checks need one narrow fact: "is this component a plain resistor?"
//! A pull-up must be a resistor (net-lint's missing-pull-up and the SI I2C
//! rise-time model both divide by its resistance); a device-count must exclude
//! passives; a crystal load cap must be a capacitor. For a long time the answer
//! was a reference-designator prefix test with a growing exclusion list
//! (`R` but not `RV`/`RT`/`RN`/`RP`/`RM`, plus a `lib_id` substring scan for
//! "ferrite"/"inductor"). Both inputs are free text a CAD user typed. That
//! produced two symmetric errors:
//!
//! - A genuine resistor that does not sit in an `R`-prefixed slot (a part
//!   numbered into an `RN` range, a mirrored/renamed designator, a symbol whose
//!   reference is `X1`) was not counted, so a real pull-up read as missing.
//! - A part that merely *looks* R-prefixed was counted: a capacitor a designer
//!   labelled `R5` was accepted as a pull-up and its farads were read as ohms.
//!
//! The fix is to ask the model DB, which is curated in-tree, instead of the
//! board file. [`hauksbee_models::schema::PassiveClass`] is the DB's own
//! statement of which two-terminal element an entry is, and model resolution is
//! reached through the [`AssemblyState`] witness, the same ticket the engine's
//! binder requires: a record whose identity is refused, or that is not on the
//! assembled board, cannot vouch for what part it is, so its strings are not
//! promoted to evidence.
//!
//! ## The evidence ladder
//!
//! [`classify_two_terminal`] walks these in order and stops at the first rung
//! that answers. Each rung is strictly better evidence than the one below it.
//!
//! 1. **Terminal count.** A part with anything other than two net-carrying pads
//!    is not a two-terminal passive at all ([`PartClass::NotTwoTerminal`]).
//!    Distinct pad *numbers* are counted, not raw pin entries (see
//!    [`connected_pads`]).
//! 2. **The model DB's declared class**, via the witness. When the fitted part
//!    resolves to an entry carrying a `passive_class`, that is the answer.
//! 3. **The model DB's kind.** A resolved entry that is not
//!    [`ComponentKind::Passive`] (a diode, a FET, a connector, an ignored
//!    mechanical part) is decisively not a passive.
//! 4. **Value dimension**, via the canonical value parser: an explicit farad or
//!    henry unit settles it regardless of the designator, which is what catches
//!    the capacitor labelled `R5` whose value is `100nF`. A bare magnitude
//!    (`10k`) and a rating (`25V`) carry no dimension and are not evidence here.
//! 5. **Capacitor-shaped pin roles.** The DB writes `pos`/`neg` for capacitor
//!    pads and `a`/`b` for everything else; `pos`/`neg` therefore rules a
//!    resistor out. (`a`/`b` is shared by resistors, inductors, ferrites,
//!    crystals and fuses, so it rules nothing *in*.)
//! 6. **The designator and `lib_id` strings**, exactly as before. This rung is
//!    a last-resort hint, kept so a board with no resolvable model and a bare
//!    magnitude for a value behaves the way it always did rather than losing a
//!    pull-up it used to find.
//!
//! Resolution is memoised per distinct (lib_id, value, footprint, mpn) tuple:
//! `is_resistor` is called inside per-net member loops, and a full library scan
//! per call would be a per-component cost multiplied by net degree. The number
//! of distinct tuples on a board is the number of distinct part types, which is
//! small and bounded.

use std::cell::RefCell;
use std::collections::HashMap;

use hauksbee_models::schema::PassiveClass;
use hauksbee_models::{ComponentKind, ComponentQuery, ModelLibrary};

use crate::assembly::AssemblyState;
use crate::Component;

/// What a component record is, as far as the two-terminal-passive question goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PartClass {
    /// A two-terminal passive of a known class.
    Passive(PassiveClass),
    /// Two terminals, but the evidence says it is not a passive at all (a
    /// diode, a transistor, a connector, a mechanical part).
    NotPassive,
    /// Not a two-terminal part: no pads, one pad, or three or more.
    NotTwoTerminal,
    /// Two terminals and nothing in the evidence ladder answered. The caller
    /// decides what an unknown two-terminal part means for it; no check may
    /// treat this as a resistor.
    Unknown,
}

impl PartClass {
    /// True only for a plain resistor: the kind of part that can be a pull-up,
    /// a pull-down, or a series termination.
    pub(crate) fn is_resistor(self) -> bool {
        matches!(self, PartClass::Passive(c) if c.is_resistor())
    }
}

/// Distinct pad numbers that carry a net.
///
/// Counts *distinct* pad numbers, not raw pin entries: footprints add net-less
/// mechanical pads, and the Eagle `.brd` extractor lists each pad once per
/// signal contact, so a two-terminal part can show four pin entries (pad 1 x2,
/// pad 2 x2). An IPC-356 both-sided through-hole access record does the same.
/// All of those must resolve to "two terminals".
pub(crate) fn connected_pads(c: &Component) -> usize {
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for p in &c.pins {
        if p.net.is_some() {
            seen.insert(p.number.as_str());
        }
    }
    seen.len()
}

/// The reference designator with a single mirror-prefix stripped, uppercased.
///
/// Split-keyboard layouts (Corne / crkbd, Lily58) duplicate the right half with
/// a lowercase `r` prefix (`rC2`, `rY1`, `rR3`, `rU1`), so the string rung must
/// see `C2`/`Y1`/`R3`/`U1` underneath, otherwise `rC2` reads as an `R`-prefixed
/// part and a mirrored decoupling cap is misclassified. Only a lowercase `r`
/// immediately before an uppercase designator letter counts, so a genuine
/// `R5` / `RV1` is untouched.
pub(crate) fn ref_designator(reference: &str) -> String {
    let r = reference.trim();
    let bytes = r.as_bytes();
    if bytes.len() >= 2 && bytes[0] == b'r' && bytes[1].is_ascii_uppercase() {
        return r[1..].to_ascii_uppercase();
    }
    r.to_ascii_uppercase()
}

/// Classify one component record against the ladder in the module doc.
pub(crate) fn classify_two_terminal(c: &Component) -> PartClass {
    // Rung 1: terminal count.
    if connected_pads(c) != 2 {
        return PartClass::NotTwoTerminal;
    }

    // Rungs 2-5 read the model DB, and model resolution is reachable only
    // through the assembly witness: an identity-refused record's value and
    // lib_id say nothing trustworthy about which part is on the board, and an
    // unfitted one is not on it at all.
    if let Some(part) = AssemblyState::of(c).fitted() {
        if let Some(class) = from_model(part.component()) {
            return class;
        }
        // Rung 4: value dimension.
        if let Some(p) = hauksbee_models::value::parse_value(&part.value) {
            match p.unit.as_deref() {
                Some("F") => return PartClass::Passive(PassiveClass::Capacitor),
                Some("H") => return PartClass::Passive(PassiveClass::Inductor),
                _ => {}
            }
        }
    }

    // Rung 6: the strings, as a last-resort hint.
    from_strings(c)
}

/// Rungs 2, 3 and 5: whatever the resolved model entry can say.
fn from_model(c: &Component) -> Option<PartClass> {
    let entry = resolved_shape(c)?;
    // Rung 2: the DB's declared class.
    if let Some(class) = entry.passive_class {
        return Some(PartClass::Passive(class));
    }
    // Rung 3: a resolved non-passive kind.
    if entry.kind != ComponentKind::Passive {
        return Some(PartClass::NotPassive);
    }
    // Rung 5: capacitor-shaped pin roles. `a`/`b` is shared by resistors,
    // inductors, ferrites, crystals and fuses, so it answers nothing.
    entry
        .capacitor_pin_roles
        .then_some(PartClass::Passive(PassiveClass::Capacitor))
}

/// The part of a resolved model entry this module reads. Cached, so it must be
/// small and owned rather than a borrow into the library.
#[derive(Clone, Copy)]
struct ResolvedShape {
    kind: ComponentKind,
    passive_class: Option<PassiveClass>,
    /// The entry's pad-role map uses the capacitor spelling (`pos`/`neg`).
    capacitor_pin_roles: bool,
}

thread_local! {
    /// Memoised resolutions keyed by the CAD strings the query is built from.
    /// Thread-local rather than a global lock: extraction is single-threaded per
    /// board, and a `Mutex` here would serialise parallel board runs.
    static SHAPE_CACHE: RefCell<HashMap<(String, String, String, String), Option<ResolvedShape>>> =
        RefCell::new(HashMap::new());
}

/// Resolve `c` against the builtin model library, memoised.
fn resolved_shape(c: &Component) -> Option<ResolvedShape> {
    let mpn = mpn_of(c);
    let key = (
        c.lib_id.clone(),
        c.value.clone(),
        c.footprint.clone(),
        mpn.clone().unwrap_or_default(),
    );
    SHAPE_CACHE.with(|cache| {
        if let Some(hit) = cache.borrow().get(&key) {
            return *hit;
        }
        let query = ComponentQuery {
            lib_id: non_empty(&c.lib_id),
            value: non_empty(&c.value),
            footprint: non_empty(&c.footprint),
            mpn,
            reference: None,
        };
        let shape = ModelLibrary::builtin_shared()
            .resolve(&query)
            .model
            .map(|entry| ResolvedShape {
                kind: entry.kind,
                passive_class: entry.passive_class,
                capacitor_pin_roles: entry
                    .pins
                    .values()
                    .any(|role| role == "pos" || role == "neg"),
            });
        cache.borrow_mut().insert(key, shape);
        shape
    })
}

/// The manufacturer part number to match on: a BOM-supplied one under the
/// reserved property key, else the value field (which is what most `mpn_re`
/// rules are actually written against). Mirrors the engine binder's choice so
/// the two do not resolve the same part differently.
fn mpn_of(c: &Component) -> Option<String> {
    c.properties
        .iter()
        .find(|(k, _)| k == crate::bom::MPN_PROPERTY)
        .map(|(_, v)| v.clone())
        .filter(|v| !v.trim().is_empty())
        .or_else(|| non_empty(&c.value))
}

fn non_empty(s: &str) -> Option<String> {
    (!s.trim().is_empty()).then(|| s.to_string())
}

/// Rung 6: the designator prefix plus the `lib_id` substring scan, i.e. exactly
/// what this question used to be answered with. Reached only when no better
/// evidence exists, so a board the model DB cannot resolve keeps the behaviour
/// it has always had.
fn from_strings(c: &Component) -> PartClass {
    let lib = c.lib_id.to_ascii_lowercase();
    if lib.contains("ferrite") {
        return PartClass::Passive(PassiveClass::FerriteBead);
    }
    if lib.contains("inductor") {
        return PartClass::Passive(PassiveClass::Inductor);
    }
    let r = ref_designator(&c.reference);
    // Capacitors first: `CN`/`CON` are connector conventions, not capacitors.
    if r.starts_with('C') && !r.starts_with("CN") && !r.starts_with("CON") {
        return PartClass::Passive(PassiveClass::Capacitor);
    }
    // Resistors, minus the historical exclusion list: varistors (RV),
    // thermistors (RT), and resistor networks / arrays (RN/RP/RM), none of
    // which is a plain two-terminal resistor. A part in one of those ranges
    // that IS a plain resistor is now caught by rung 2 instead of being lost
    // here.
    let excluded = ["RV", "RT", "RN", "RP", "RM"]
        .iter()
        .any(|p| r.starts_with(p));
    if r.starts_with('R') && !excluded {
        return PartClass::Passive(PassiveClass::Resistor);
    }
    PartClass::Unknown
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Component, Pin};

    fn part(reference: &str, value: &str, lib_id: &str, footprint: &str) -> Component {
        Component {
            reference: reference.into(),
            value: value.into(),
            lib_id: lib_id.into(),
            footprint: footprint.into(),
            position: None,
            layer: "F.Cu".into(),
            properties: Vec::new(),
            dnp: false,
            pins: vec![pad("1", 1), pad("2", 2)],
        }
    }

    fn pad(number: &str, net: i64) -> Pin {
        Pin {
            number: number.into(),
            net: Some(net),
            function: String::new(),
            kind: String::new(),
            position: None,
        }
    }

    #[test]
    fn a_resistor_outside_the_r_designator_range_is_still_a_resistor() {
        // The designator rung rejects an `RN`-prefixed reference (it reads as a
        // resistor NETWORK) and rejects `X1` outright, so both of these used to
        // be lost: a real pull-up read as missing purely because of the slot it
        // was numbered into.
        let network_slot = part("RN1", "4k7", "Device:R", "Resistor_SMD:R_0402_1005Metric");
        assert!(
            classify_two_terminal(&network_slot).is_resistor(),
            "a two-terminal Device:R in an RN slot is a resistor"
        );
        let odd_reference = part("X1", "5.1k", "Device:R", "");
        assert!(
            classify_two_terminal(&odd_reference).is_resistor(),
            "the model, not the designator, decides"
        );
        // And the ordinary case still classifies, by the same rung.
        assert!(classify_two_terminal(&part("R7", "10k", "Device:R", "")).is_resistor());
    }

    #[test]
    fn a_capacitor_named_r5_is_not_a_resistor() {
        // Symmetric error: the designator rung accepted `R5` and a downstream
        // pull-up check then read 100 nF as 100 nano-ohms.
        let by_model = part("R5", "100nF", "Device:C", "Capacitor_SMD:C_0402_1005Metric");
        assert_eq!(
            classify_two_terminal(&by_model),
            PartClass::Passive(PassiveClass::Capacitor),
            "the DB says Device:C is a capacitor"
        );
        assert!(!classify_two_terminal(&by_model).is_resistor());
        // With no resolvable lib_id, the farad unit on the value still settles it.
        let by_value = part("R5", "100nF", "", "");
        assert!(!classify_two_terminal(&by_value).is_resistor());
        assert_eq!(
            classify_two_terminal(&by_value),
            PartClass::Passive(PassiveClass::Capacitor)
        );
    }

    #[test]
    fn a_ferrite_bead_is_never_a_resistor_even_with_an_ohmic_value() {
        // A bead is sold as an impedance at a frequency, so its value reads as a
        // resistance. The DB's class is what keeps it out of the pull-up path.
        let bead = part("FB1", "600R", "Device:Ferrite_Bead", "");
        assert!(!classify_two_terminal(&bead).is_resistor());
        // And an inductor's henry value excludes it even unresolved.
        assert!(!classify_two_terminal(&part("L1", "10uH", "", "")).is_resistor());
    }

    #[test]
    fn the_string_rung_still_answers_when_nothing_better_exists() {
        // A layout-only extraction: no lib_id the DB matches, a bare magnitude
        // with no unit. This must keep behaving as it always did, or boards the
        // DB cannot resolve would silently lose every pull-up they had.
        assert!(classify_two_terminal(&part("R1", "10k", "", "")).is_resistor());
        assert!(!classify_two_terminal(&part("RV1", "", "", "")).is_resistor());
        assert!(!classify_two_terminal(&part("RT1", "", "", "")).is_resistor());
        assert!(!classify_two_terminal(&part("U1", "", "", "")).is_resistor());
    }

    #[test]
    fn a_part_without_two_connected_pads_is_not_a_two_terminal_passive() {
        let mut three = part("R1", "10k", "Device:R", "");
        three.pins.push(pad("3", 3));
        assert_eq!(classify_two_terminal(&three), PartClass::NotTwoTerminal);
        // Repeated pad numbers still count as two terminals.
        let mut doubled = part("R1", "10k", "Device:R", "");
        doubled.pins.push(pad("1", 1));
        assert!(classify_two_terminal(&doubled).is_resistor());
    }

    #[test]
    fn an_identity_refused_record_does_not_reach_model_evidence() {
        // The witness is the ticket into resolution: a record with a duplicate
        // designator conflict cannot vouch for its own value/lib_id, so it falls
        // to the string rung rather than being promoted by the DB.
        let mut refused = part("X1", "5.1k", "Device:R", "");
        refused
            .properties
            .push((crate::DUPLICATE_REFERENCE_CONFLICT_KEY.into(), "two".into()));
        assert_eq!(classify_two_terminal(&refused), PartClass::Unknown);
    }
}
