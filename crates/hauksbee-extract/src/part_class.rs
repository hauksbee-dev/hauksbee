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
//! statement of which two-terminal element an entry is.
//!
//! What gates that evidence is [`AssemblyState`] *identity trust*, not presence:
//! a record whose identity is refused (a contradictory duplicate designator, an
//! inferred reference with no authoritative source UID) cannot vouch for which
//! part it names, so nothing about it is promoted to evidence and the answer is
//! [`PartClass::Unknown`] outright. That has to include the designator: falling
//! through to the string rung would let a conflicting duplicate `R5` be called a
//! resistor with confidence, which is the opposite of what refusing its identity
//! meant. A DNP part's identity is perfectly good, though, and it
//! is deliberately NOT excluded here: whether the part is fitted is a different
//! question, which each caller answers for itself. Gating on the fitted witness
//! would reintroduce the bug in miniature, sending a DNP capacitor labelled `R5`
//! past the rung that reads its farads and back onto the designator string.
//!
//! One known limitation: resolution uses the builtin library only
//! ([`ModelLibrary::builtin_shared`]), because extraction has no plumbing for
//! `--models-dir` / pack layers. A user model that declares a `passive_class` for
//! their part therefore informs the engine's binder but not this classifier,
//! which falls to a lower rung instead of being wrong.
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
//! 2. **The model DB's declared class.** When an identity-trusted record resolves
//!    to an entry carrying a `passive_class`, that is the answer.
//! 3. **The model DB's kind.** A resolved entry that is not
//!    [`ComponentKind::Passive`] (a diode, a FET, a connector, an ignored
//!    mechanical part) is decisively not a passive.
//! 4. **Value dimension**, via the canonical value parser: an explicit farad or
//!    henry unit settles it, which is what catches the capacitor labelled `R5`
//!    whose value is `100nF`, and so does a bare sub-unity SI multiplier, which
//!    catches the same part valued `100n`, `4u7` or `470u`. An ohm mark rules a capacitor out
//!    but rules nothing in (a ferrite bead reads ohmic). An ordinary bare
//!    magnitude (`10k`) and a rating (`25V`) are not evidence here. When this rung
//!    CONTRADICTS the model's declared class, the answer is `Unknown` rather than
//!    either witness: two disagreeing sources are not a confident answer.
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

    /// True only for a capacitor: the kind of part that can be a crystal load
    /// cap, a decoupling cap, or the bypass that makes a net read as a local
    /// supply rail.
    pub(crate) fn is_capacitor(self) -> bool {
        matches!(self, PartClass::Passive(PassiveClass::Capacitor))
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

    // Rungs 2-5 read the model DB, and what gates them is IDENTITY TRUST, not
    // presence. The question here is "what part does this record name?", which
    // is answerable for a DNP part (it is a known part that is not fitted) and
    // unanswerable for an identity-refused one (a contradictory duplicate
    // designator, an inferred reference with no authoritative UID): there,
    // nothing about the record, including its value and lib_id, is evidence.
    //
    // Gating on the fitted witness instead would have been a subtler version of
    // the bug this module exists to fix: a DNP capacitor a designer had labelled
    // `R5` would skip the value-dimension rung that reads its farads and land on
    // the designator string, classifying as a resistor again.
    // A refused identity stops here. "Nothing about the record is evidence" has
    // to include its designator: falling through to the string rung would let a
    // conflicting duplicate `R5` be called a resistor with confidence, which is
    // the opposite of what refusing its identity meant.
    if !identity_is_trusted(c) {
        return PartClass::Unknown;
    }

    if let Some(class) = from_model(c) {
        return class;
    }
    // Rung 4: value dimension.
    if let Some(class) = from_value_dimension(c) {
        return class;
    }

    // Rung 6: the strings, as a last-resort hint.
    from_strings(c)
}

/// Is this record's *identity* trustworthy, i.e. can it vouch for which physical
/// part it names?
///
/// True for an ordinary record and for a DNP one; false only for
/// [`AssemblyState::IdentityUnknown`]. Presence is a separate question that each
/// caller answers for itself (the pull-up paths all reject non-present parts
/// before they get here, because an unfitted resistor conducts nothing).
fn identity_is_trusted(c: &Component) -> bool {
    !matches!(AssemblyState::of(c), AssemblyState::IdentityUnknown(_))
}

/// Rung 4: what the value's dimension says on its own.
///
/// An explicit farad or henry unit is decisive. So is a *sub-unity SI multiplier*
/// with the unit left off, which is the common BOM spelling: `100n`, `22p`,
/// `4u7`, `10u`, `470u`. No discrete resistor is written that way. Milliohm
/// shunts, the one resistor class with a sub-unity value, are spelled `1m`,
/// `0.001` or `R001`, so `m` is deliberately NOT in the set; and a magnitude
/// threshold cannot do this job, because a 470 uF electrolytic (4.7e-4) and a
/// milliohm shunt (1e-3) are the same order of magnitude while their spellings are
/// unambiguous.
///
/// A sub-unity multiplier says "reactive", not "capacitive": `10u` is also how
/// some libraries spell 10 uH. Which one it is comes from the string hint, so an
/// inductor is not recorded as a capacitor. Either way it is not a resistor, which
/// is the answer that matters most to the callers.
fn from_value_dimension(c: &Component) -> Option<PartClass> {
    let parsed = hauksbee_models::value::parse_value(&c.value)?;
    match parsed.unit.as_deref() {
        Some("F") => return Some(PartClass::Passive(PassiveClass::Capacitor)),
        Some("H") => return Some(PartClass::Passive(PassiveClass::Inductor)),
        // An explicit ohm mark rules a capacitor out, but does NOT rule a
        // resistor in: a ferrite bead is sold as an impedance and reads ohmic.
        Some("\u{03a9}") | Some("V") | Some("A") => return None,
        _ => {}
    }
    if !has_sub_unity_multiplier(&c.value) {
        return None;
    }
    Some(match from_strings(c) {
        PartClass::Passive(PassiveClass::Inductor)
        | PartClass::Passive(PassiveClass::FerriteBead) => {
            PartClass::Passive(PassiveClass::Inductor)
        }
        _ => PartClass::Passive(PassiveClass::Capacitor),
    })
}

/// Does this value string carry a pico / nano / micro multiplier, with no unit?
///
/// Matches the letter that acts as the multiplier, in either the trailing form
/// (`100n`) or the RKM decimal-point form (`4u7`), and accepts both micro glyphs
/// libraries use. `m` (milli) is excluded on purpose: it is how milliohm shunts
/// are written.
fn has_sub_unity_multiplier(value: &str) -> bool {
    // Only the leading magnitude token matters; "/25V" style qualifiers and
    // trailing tolerance annotations are not part of it.
    let head = value
        .split(['/', ',', '@', ' '])
        .next()
        .unwrap_or(value)
        .trim();
    let mut seen_digit = false;
    for ch in head.chars() {
        if ch.is_ascii_digit() {
            seen_digit = true;
            continue;
        }
        if ch == '.' {
            continue;
        }
        // The first non-numeric character after a digit is the multiplier.
        if seen_digit {
            return matches!(
                ch,
                'p' | 'P' | 'n' | 'N' | 'u' | 'U' | '\u{00b5}' | '\u{03bc}'
            );
        }
        return false;
    }
    false
}

/// Rungs 2, 3 and 5: whatever the resolved model entry can say./// Rungs 2, 3 and 5: whatever the resolved model entry can say.
fn from_model(c: &Component) -> Option<PartClass> {
    let entry = resolved_shape(c)?;
    // Rung 2: the DB's declared class.
    if let Some(class) = entry.passive_class {
        // Unless the value's dimension contradicts it. A `Device:R` symbol valued
        // `100nF` is a schematic error either way, and this codebase would rather
        // say "I cannot tell" than pick one of two contradicting witnesses and
        // hand a downstream check a confident wrong answer.
        if let Some(PartClass::Passive(by_value)) = from_value_dimension(c) {
            if by_value != class {
                return Some(PartClass::Unknown);
            }
        }
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
    // Capacitors first. `CN`/`CON` are connector conventions, and `CR` is the
    // MIL-STD/ANSI designator for a DIODE, which the engine's binder already
    // documents ("a `CR1` zener must never reach the C-first-letter capacitor
    // heuristic"); reading a zener as a bypass cap is the same class of error as
    // reading a capacitor as a pull-up.
    if r.starts_with('C') && !r.starts_with("CN") && !r.starts_with("CON") && !r.starts_with("CR") {
        return PartClass::Passive(PassiveClass::Capacitor);
    }
    // Resistors, minus the exclusion list: varistors (RV), thermistors (RT), and
    // resistor networks / arrays (RN/RP/RM/RA), none of which is a plain
    // two-terminal resistor. `RA` belongs with the other array prefixes and was
    // simply missing from the historical list. A part in one of those ranges
    // that IS a plain resistor is now caught by rung 2 instead of being lost
    // here.
    let excluded = ["RV", "RT", "RN", "RP", "RM", "RA"]
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
    fn the_capacitor_question_uses_the_same_ladder_as_the_resistor_one() {
        // The crystal-load-cap and local-rail paths ask "is this a capacitor?",
        // and leaving that on the designator alone while the resistor question
        // moved to the ladder would have let the two disagree about one part.
        // A Device:C in an oddly-named slot counts...
        let odd = part("X7", "18pF", "Device:C", "");
        assert!(classify_two_terminal(&odd).is_capacitor());
        assert!(!classify_two_terminal(&odd).is_resistor());
        // ...and a resistor a designer labelled C5 does not.
        let mislabelled = part("C5", "4k7", "Device:R", "");
        assert!(!classify_two_terminal(&mislabelled).is_capacitor());
        assert!(classify_two_terminal(&mislabelled).is_resistor());
        // The historical connector exclusions survive on the string rung.
        assert!(!classify_two_terminal(&part("CN1", "", "", "")).is_capacitor());
        assert!(!classify_two_terminal(&part("CON2", "", "", "")).is_capacitor());
        // And the ordinary case still classifies.
        assert!(classify_two_terminal(&part("C12", "100nF", "", "")).is_capacitor());
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
    fn a_bare_sub_unity_multiplier_is_a_reactive_value_not_a_resistance() {
        // No discrete resistor is written `100n` / `4u7` / `470u`; capacitors
        // routinely are. Handling only the `100nF` spelling left the same CAD
        // error live one character away, and a magnitude threshold could not fix
        // it, since a 470 uF electrolytic and a milliohm shunt are the same order.
        for value in ["100n", "22p", "4u7", "10u", "470u", "1u", "0.1u"] {
            let mislabelled = part("R5", value, "", "");
            assert!(
                !classify_two_terminal(&mislabelled).is_resistor(),
                "{value} is not a resistance"
            );
            assert!(
                classify_two_terminal(&mislabelled).is_capacitor(),
                "{value} reads as a capacitance"
            );
        }
        // An inductor spelled the same way is reactive but NOT a capacitor: the
        // string hint disambiguates, and either way it is not a resistor.
        let coil = part("L1", "10u", "Device:L_Small", "");
        assert!(!classify_two_terminal(&coil).is_resistor());
        assert!(!classify_two_terminal(&coil).is_capacitor());
        // Ordinary resistance spellings are untouched, milliohm shunts included:
        // `m` is deliberately not a sub-unity multiplier for this rule.
        for value in ["10k", "0", "4k7", "1m", "0.001", "R001", "100"] {
            assert!(
                classify_two_terminal(&part("R7", value, "", "")).is_resistor(),
                "{value} is a resistance"
            );
        }
    }

    #[test]
    fn contradicting_witnesses_produce_no_confident_answer() {
        // A `Device:R` symbol carrying `100nF` is a schematic error either way.
        // Picking one of two contradicting witnesses would hand a downstream
        // check a confident wrong answer, so the ladder abstains.
        let contradictory = part("R5", "100nF", "Device:R", "");
        assert_eq!(classify_two_terminal(&contradictory), PartClass::Unknown);
        assert!(!classify_two_terminal(&contradictory).is_resistor());
        assert!(!classify_two_terminal(&contradictory).is_capacitor());
        // Agreement is still a confident answer.
        assert!(classify_two_terminal(&part("R5", "10k", "Device:R", "")).is_resistor());
    }

    #[test]
    fn a_refused_identity_is_never_string_guessed_into_a_resistor() {
        // "Nothing about the record is evidence" has to include the designator.
        // The dangerous case is an `R`-prefixed reference, which the string rung
        // would happily call a resistor.
        let mut refused = part("R5", "100nF", "Device:C", "");
        refused
            .properties
            .push((crate::DUPLICATE_REFERENCE_CONFLICT_KEY.into(), "two".into()));
        assert_eq!(classify_two_terminal(&refused), PartClass::Unknown);
        assert!(!classify_two_terminal(&refused).is_resistor());
    }

    #[test]
    fn a_cr_designator_is_a_diode_not_a_capacitor() {
        // `CR` is the MIL-STD/ANSI diode designator, which the engine's binder
        // already documents. Reading a zener as a bypass cap is the same class of
        // error as reading a capacitor as a pull-up.
        let zener = part("CR1", "5.1V", "", "");
        assert!(!classify_two_terminal(&zener).is_capacitor());
        assert!(!classify_two_terminal(&zener).is_resistor());
        // Ordinary C-prefixed parts are unaffected.
        assert!(classify_two_terminal(&part("C1", "100nF", "", "")).is_capacitor());
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
    fn a_dnp_record_still_reaches_model_evidence() {
        // Presence is not identity. A DNP part is a KNOWN part that happens not
        // to be fitted, so its value and lib_id are still evidence about what it
        // is. Gating the model rungs on the fitted witness would have sent this
        // DNP capacitor past the rung that reads its farads and back onto its
        // misleading `R5` designator.
        let mut dnp_cap = part("R5", "100nF", "Device:C", "");
        dnp_cap.dnp = true;
        assert_eq!(
            classify_two_terminal(&dnp_cap),
            PartClass::Passive(PassiveClass::Capacitor),
            "a DNP capacitor is still a capacitor"
        );
        // Same for the value-dimension rung with no resolvable lib_id.
        let mut bare = part("R5", "100nF", "", "");
        bare.dnp = true;
        assert!(!classify_two_terminal(&bare).is_resistor());
        // And a DNP resistor is still classified a resistor: callers reject it
        // on presence, which is their job, not this function's.
        let mut dnp_r = part("RN1", "4k7", "Device:R", "");
        dnp_r.dnp = true;
        assert!(classify_two_terminal(&dnp_r).is_resistor());
    }

    #[test]
    fn an_identity_refused_record_does_not_reach_model_evidence() {
        // A record with a duplicate designator conflict cannot vouch for its own
        // value or lib_id, so the ladder stops and answers Unknown rather than
        // promoting either the DB's class or the designator.
        let mut refused = part("X1", "5.1k", "Device:R", "");
        refused
            .properties
            .push((crate::DUPLICATE_REFERENCE_CONFLICT_KEY.into(), "two".into()));
        assert_eq!(classify_two_terminal(&refused), PartClass::Unknown);
    }
}
