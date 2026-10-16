//! Reference-designator classes: what a designator prefix says a part is.
//!
//! The one prefix table. It is the weakest evidence there is (a designator is
//! whatever the designer typed), so `hauksbee_extract::part_class` consults it
//! only after the model DB and the assembly witness, and connector detection
//! (`hauksbee_extract::is_connector_like`) adds the library, footprint and value.
//! But every check that falls back to a prefix reads it here, so `LED1` is never
//! an inductor in one check and a diode in another.

/// What a designator prefix claims a part is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefClass {
    /// `R*`, apart from the arrays, varistors and thermistors below.
    Resistor,
    /// `RN`, `RP`, `RM`, `RA`: several resistor elements in one package.
    ResistorArray,
    /// `RV`: a varistor (or a trimmer in some libraries): not a fixed resistor.
    Varistor,
    /// `RT`: a thermistor.
    Thermistor,
    /// `C*`, apart from `CN`/`CON` (connectors) and `CR` (a diode).
    Capacitor,
    /// `L*`, apart from `LED`, `LS` (speaker), `LDR` (photoresistor), `LCD`.
    Inductor,
    /// `FB`: a ferrite bead.
    FerriteBead,
    /// `D` or `CR`, the IEEE 315 designator for a diode or rectifier.
    Diode,
    /// `LED`.
    Led,
    /// `Q`.
    Transistor,
    /// `U`, `IC`, `MCU`.
    ActiveIc,
    /// `SW`, `S`, `BTN`: a switch or push button.
    Switch,
    /// Anything else, including a CAD tool's `U$12` placeholder.
    Other,
}

impl RefClass {
    /// The class `reference`'s designator prefix claims.
    #[must_use]
    pub fn of(reference: &str) -> Self {
        let d = designator(reference);
        let starts = |p: &str| d.starts_with(p);
        if starts("LED") {
            Self::Led
        } else if starts("L") && !["LS", "LDR", "LCD"].iter().any(|p| starts(p)) {
            Self::Inductor
        } else if starts("FB") {
            Self::FerriteBead
        } else if starts("CR") {
            Self::Diode
        } else if starts("C") && !starts("CN") && !starts("CON") {
            Self::Capacitor
        } else if starts("RV") {
            Self::Varistor
        } else if starts("RT") {
            Self::Thermistor
        } else if ["RN", "RP", "RM", "RA"].iter().any(|p| starts(p)) {
            Self::ResistorArray
        } else if starts("R") {
            Self::Resistor
        } else {
            match prefix(&d).as_str() {
                "D" => Self::Diode,
                "Q" => Self::Transistor,
                "SW" | "S" | "BTN" => Self::Switch,
                // Eagle's `U$12` placeholder is a hole, fiducial or logo.
                "U" | "IC" | "MCU" if !is_tool_generated_ref(reference) => Self::ActiveIc,
                _ => Self::Other,
            }
        }
    }

    /// A plain resistor, capacitor or inductor: the series elements of a filter
    /// or divider.
    #[must_use]
    pub const fn is_rlc(self) -> bool {
        matches!(self, Self::Resistor | Self::Capacitor | Self::Inductor)
    }
}

/// The designator, upper-cased, with a split-keyboard mirror prefix removed.
///
/// Split-keyboard layouts (Corne, Lily58) duplicate the right half with a
/// lower-case `r` prefix (`rC2`, `rR3`, `rU1`), so `rC2` is a capacitor. Only a
/// lower-case `r` right before an upper-case letter counts, so `R5` and `RV1`
/// are untouched.
#[must_use]
pub fn designator(reference: &str) -> String {
    let r = reference.trim();
    let bytes = r.as_bytes();
    if bytes.len() >= 2 && bytes[0] == b'r' && bytes[1].is_ascii_uppercase() {
        return r[1..].to_ascii_uppercase();
    }
    r.to_ascii_uppercase()
}

/// The upper-cased alphabetic prefix of a designator (`JP3` reads `JP`).
#[must_use]
pub fn prefix(reference: &str) -> String {
    reference
        .chars()
        .take_while(char::is_ascii_alphabetic)
        .collect::<String>()
        .to_ascii_uppercase()
}

/// Whether a reference designator names an active IC (prefix `U`, `IC` or
/// `MCU`), the binder's MCU-candidate convention.
///
/// Eagle names any element the designer left unnamed `U$12`, and that covers
/// mounting holes, fiducials, logos, frames and test points, so a
/// tool-generated reference never counts (it inflated one board's active-IC
/// count from four to 69).
#[must_use]
pub fn is_active_ic_ref(reference: &str) -> bool {
    RefClass::of(reference) == RefClass::ActiveIc
}

/// Whether a reference is a CAD tool's placeholder for an unnamed element
/// (Eagle's `U$12` form) rather than a designator a person assigned.
#[must_use]
pub fn is_tool_generated_ref(reference: &str) -> bool {
    let Some((head, tail)) = reference.split_once('$') else {
        return false;
    };
    !head.is_empty()
        && head.chars().all(|c| c.is_ascii_alphabetic())
        && !tail.is_empty()
        && tail.chars().all(|c| c.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::RefClass::{
        self, ActiveIc, Capacitor, Diode, FerriteBead, Inductor, Led, Other, Resistor,
        ResistorArray, Switch, Thermistor, Transistor, Varistor,
    };

    #[test]
    fn golden_designator_table() {
        let golden: &[(&str, RefClass, &str)] = &[
            ("R1", Resistor, "resistor"),
            ("rR3", Resistor, "split-keyboard mirror of R3"),
            ("RN1", ResistorArray, "resistor network"),
            ("RP2", ResistorArray, "resistor pack"),
            ("RA1", ResistorArray, "resistor array"),
            ("RM1", ResistorArray, "resistor module"),
            ("RV1", Varistor, "varistor, not a fixed resistor"),
            ("RT1", Thermistor, "thermistor"),
            ("C1", Capacitor, "capacitor"),
            ("rC2", Capacitor, "split-keyboard mirror of C2"),
            (
                "CR1",
                Diode,
                "IEEE 315: CR is a diode or rectifier, never a capacitor",
            ),
            ("CN1", Other, "connector"),
            ("CON3", Other, "connector"),
            ("L1", Inductor, "inductor"),
            ("LED1", Led, "a light-emitting diode, never an inductor"),
            ("LS1", Other, "loudspeaker"),
            ("LDR1", Other, "photoresistor"),
            ("LCD1", Other, "display"),
            ("FB1", FerriteBead, "ferrite bead"),
            ("D1", Diode, "diode"),
            ("Q1", Transistor, "transistor"),
            ("U1", ActiveIc, "IC"),
            ("IC2", ActiveIc, "IC"),
            ("MCU1", ActiveIc, "IC"),
            ("U$12", Other, "Eagle's placeholder for an unnamed element"),
            ("J1", Other, "connector"),
            ("SW1", Switch, "switch or push button"),
            ("S2", Switch, "switch (IEEE 315 S)"),
            ("BTN1", Switch, "push button"),
            ("SPK1", Other, "speaker, not a switch"),
            (
                "USB1",
                Other,
                "a connector named after its bus, not a U-prefix IC",
            ),
        ];
        let wrong: Vec<String> = golden
            .iter()
            .filter(|(r, want, _)| RefClass::of(r) != *want)
            .map(|(r, want, why)| format!("{r}: got {:?}, want {want:?} ({why})", RefClass::of(r)))
            .collect();
        assert!(wrong.is_empty(), "misclassified:\n{}", wrong.join("\n"));
    }
}
