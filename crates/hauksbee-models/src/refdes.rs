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

/// What a part says about itself beyond its designator: the fields
/// [`is_active_ic_part`] reads.
#[derive(Debug, Clone, Copy, Default)]
pub struct PartIdentity<'a> {
    pub reference: &'a str,
    /// Symbol id (`Library:Symbol`), or empty.
    pub lib_id: &'a str,
    pub value: &'a str,
    pub footprint: &'a str,
    /// A manufacturer part number property, when the record carries one.
    pub mpn: Option<&'a str>,
    /// Distinct numbered pads on the part.
    pub pads: usize,
}

/// Footprint package families that integrated circuits land on, as the letters
/// before a pin count (`SOIC-8`, `QFN32`, `DIP-14`).
const IC_PACKAGES: &[&str] = &[
    "SOIC", "SOP", "SSOP", "TSSOP", "MSOP", "VSSOP", "QSOP", "QFN", "VQFN", "WQFN", "UQFN", "DFN",
    "TDFN", "QFP", "LQFP", "TQFP", "PQFP", "BGA", "FBGA", "VFBGA", "UFBGA", "TFBGA", "LFBGA",
    "CABGA", "LGA", "WLCSP", "CSP", "PLCC", "PDIP", "DIP", "SO",
];

/// Small-outline families a transistor or diode also lands on: an IC only with
/// five or more pads (`SOT-23-6`).
const SMALL_OUTLINES: &[&str] = &["SOT", "SC", "SOD"];

/// Words in a symbol name or footprint that mark a mechanical part.
const MECHANICAL_WORDS: &[&str] = &[
    "SW",
    "BUTTON",
    "PUSHBUTTON",
    "PUSH",
    "TACT",
    "TACTILE",
    "ENCODER",
    "JOYSTICK",
    "TOGGLE",
    "SLIDE",
    "ROCKER",
    "CONNECTOR",
    "CONN",
    "HEADER",
    "PINHEADER",
    "PINSOCKET",
    "JUMPER",
    "TESTPOINT",
    "MOUNTINGHOLE",
    "FIDUCIAL",
    "CRYSTAL",
    "RELAY",
    "BUZZER",
    "SPEAKER",
    "POTENTIOMETER",
    "TRIMMER",
];

/// Upper-cased alphanumeric words of `text`.
fn words(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_ascii_uppercase)
}

/// The footprint is a package only integrated circuits land on: `SOIC-8`,
/// `TSSOP-16`, `QFN-32`, `DIP-14`; a small outline (`SOT-23`) counts only with
/// five or more pads.
#[must_use]
pub fn lands_an_ic_package(footprint: &str, pads: usize) -> bool {
    let leaf = footprint.rsplit(':').next().unwrap_or(footprint);
    words(leaf).any(|w| {
        let base = w.trim_end_matches(|c: char| c.is_ascii_digit());
        IC_PACKAGES.contains(&base) || (pads >= 5 && SMALL_OUTLINES.contains(&base))
    })
}

/// Whether the symbol, footprint or value names a mechanical part (switch,
/// button, connector) rather than a chip. An analog-switch IC is not one: its
/// library is `Analog_Switch`, not `Switch`.
fn names_a_mechanical_part(p: &PartIdentity<'_>) -> bool {
    let (library, symbol) = p.lib_id.split_once(':').unwrap_or(("", p.lib_id));
    let lib = library.to_ascii_uppercase();
    let sym = symbol.to_ascii_uppercase();
    let (fp_lib, fp_leaf) = p.footprint.split_once(':').unwrap_or(("", p.footprint));
    let fp_lib = fp_lib.to_ascii_uppercase();
    lib == "SWITCH"
        || lib.starts_with("BUTTON")
        || lib.starts_with("CONNECTOR")
        || sym.starts_with("SW_")
        || fp_lib.starts_with("BUTTON_SWITCH")
        || fp_lib.starts_with("CONNECTOR")
        || fp_lib.starts_with("SWITCH")
        || words(fp_leaf)
            .chain(words(&sym))
            .any(|w| MECHANICAL_WORDS.contains(&w.as_str()))
        || words(p.value).any(|w| matches!(w.as_str(), "SW" | "SWITCH" | "BUTTON" | "TACT"))
}

/// Symbol libraries (the part before the `:`) of integrated circuits.
fn names_an_ic_library(lib_id: &str) -> bool {
    let Some((library, _)) = lib_id.split_once(':') else {
        return false;
    };
    let lib = library.to_ascii_uppercase();
    [
        "ANALOG_SWITCH",
        "AMPLIFIER",
        "AUDIO",
        "COMPARATOR",
        "CONVERTER",
        "DRIVER",
        "INTERFACE",
        "LOGIC",
        "MCU_",
        "MEMORY",
        "MOTOR_DRIVER",
        "POWER_MANAGEMENT",
        "REGULATOR",
        "REFERENCE",
        "SENSOR",
        "TIMER",
        "RF_MODULE",
        "74",
        "4XXX",
        "ISOLATOR",
        "RTC",
        "FPGA",
        "CPU",
        "DSP",
        "ANALOG",
        "BATTERY_MANAGEMENT",
        "LED_DRIVER",
        "VOLTAGE_REFERENCE",
    ]
    .iter()
    .any(|k| lib.starts_with(k))
}

/// Whether the record is an active IC, judged by what the part says about itself
/// and not by its designator alone.
///
/// A `U`/`IC`/`MCU` designator names an IC, as before. A designator that reads
/// as something else (`S1`, `SW2`, `US3`, Eagle's `U$7`) is an IC when the
/// record gives positive evidence of one and none of a mechanical part: three
/// or more pads, a symbol, footprint and value that do not name a switch,
/// button or connector, and at least one of an IC package footprint, an IC
/// symbol library or a part-number property. A part labelled `S1` that is a
/// SPDT analog-switch chip in an `SOIC-8` is one; a tactile button is not.
#[must_use]
pub fn is_active_ic_part(p: &PartIdentity<'_>) -> bool {
    match RefClass::of(p.reference) {
        RefClass::ActiveIc => return true,
        RefClass::Switch | RefClass::Other => {}
        _ => return false,
    }
    let d = designator(p.reference);
    let letter = d.chars().next();
    let ambiguous_prefix = is_tool_generated_ref(p.reference) || matches!(letter, Some('S' | 'U'));
    if !ambiguous_prefix || p.pads < 3 || names_a_mechanical_part(p) {
        return false;
    }
    // Hole, fiducial and logo placeholders have no pads; a part with a pad
    // count and an IC footprint or library is a chip.
    let part_number = p.mpn.is_some_and(|m| {
        let m = m.trim();
        m.len() >= 4
            && m.chars().any(|c| c.is_ascii_digit())
            && m.chars().any(|c| c.is_ascii_alphabetic())
    });
    lands_an_ic_package(p.footprint, p.pads)
        || names_an_ic_library(p.lib_id)
        || (part_number && p.pads >= 5)
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

    fn identity<'a>(
        reference: &'a str,
        lib_id: &'a str,
        value: &'a str,
        footprint: &'a str,
        pads: usize,
    ) -> super::PartIdentity<'a> {
        super::PartIdentity {
            reference,
            lib_id,
            value,
            footprint,
            mpn: None,
            pads,
        }
    }

    /// A chip drawn under a switch-like designator is an active IC by what the
    /// record says it is; a real switch under the same designator is not.
    #[test]
    fn an_ic_under_a_switch_like_designator_is_an_active_ic() {
        use super::is_active_ic_part as ic;
        // SPDT analog switch chip labelled S1 / SW1 / US2.
        assert!(ic(&identity(
            "S1",
            "Analog_Switch:TS5A3159",
            "TS5A3159",
            "Package_TO_SOT_SMD:SOT-23-6",
            6
        )));
        assert!(ic(&identity(
            "SW3",
            "",
            "TPS22918",
            "Package_SO:SOIC-8_3.9x4.9mm_P1.27mm",
            8
        )));
        assert!(ic(&identity(
            "US2",
            "Device:Foo",
            "XC9535",
            "Package_DFN_QFN:QFN-16-1EP_3x3mm",
            17
        )));
        assert!(ic(&identity(
            "U$7",
            "",
            "ADS1115",
            "Package_SO:TSSOP-10_3x3mm",
            10
        )));
        // A part number property and enough pads is evidence too.
        let mut with_mpn = identity("S9", "Device:Foo", "X", "Custom:Pkg", 8);
        with_mpn.mpn = Some("MAX4617");
        assert!(ic(&with_mpn));
    }

    #[test]
    fn mechanical_parts_and_passives_are_not_active_ics_whatever_their_footprint() {
        use super::is_active_ic_part as ic;
        assert!(!ic(&identity(
            "S1",
            "Switch:SW_Push",
            "SW_Push",
            "Button_Switch_SMD:SW_SPST_TL3342",
            2
        )));
        // A four-pad tactile button with a part number is still a button.
        assert!(!ic(&identity(
            "SW4",
            "Switch:SW_Push_4P",
            "SKRPACE010",
            "Button_Switch_SMD:SW_Push_1P1T_NO_6x6mm",
            4
        )));
        // A DIP switch lands a DIP package and is not a chip.
        assert!(!ic(&identity(
            "SW1",
            "Switch:SW_DIP_x04",
            "SW_DIP_x04",
            "Button_Switch_THT:SW_DIP_SPSTx04_Slide_9.78x12.34mm_W7.62mm_P2.54mm",
            8
        )));
        // A connector named U-something.
        assert!(!ic(&identity(
            "USB1",
            "Connector:USB_C",
            "USB_C",
            "Connector_USB:USB_C_Receptacle",
            16
        )));
        // A transistor-sized SOT-23 (three pads) under an S designator.
        assert!(!ic(&identity(
            "S2",
            "Device:Q",
            "BC847",
            "Package_TO_SOT_SMD:SOT-23",
            3
        )));
        // Passives keep their class whatever else the record says.
        assert!(!ic(&identity("R1", "", "10k", "Package_SO:SOIC-8", 8)));
        assert!(!ic(&identity("J1", "", "Conn", "Package_SO:SOIC-8", 8)));
        // No pads, no chip (a logo placeholder).
        assert!(!ic(&identity("U$1", "", "LOGO", "Package_SO:SOIC-8", 0)));
        // The designator rule is unchanged.
        assert!(ic(&identity("U1", "", "", "", 0)));
    }
}
