//! Net-name classification: ground, supply rails and rail voltages.
//!
//! The one table the binder, netlint, the SI checks and every board check read.
//! It used to exist as about nine local copies that disagreed: `GND_SENSE` was
//! ground in netlint but not in the USB-C check, `VEE` was ground only in the
//! binder, and bare `VDD` was 5 V in netlint but unknown in the binder. The
//! decisions below are made on electrical grounds, and the golden table in the
//! tests pins every one with its reason.
//!
//! Three rules hold throughout:
//! - Only the last path segment decides (`/Power/+3V3` reads `+3V3`).
//! - A name that does not state its voltage has no voltage here. Guessing one
//!   risks both a false pass and a false alarm; callers that only need "is this
//!   a supply" ask [`is_supply`], which does not need a magnitude.
//! - A net named after a rail but carrying a sense, monitor, enable or status
//!   word (`3V3_EN`, `12V_FB`, `GND_SENSE`) is a signal, not the rail.

/// The last path segment of a net name, trimmed and upper-cased:
/// `/Power/+3V3` reads `+3V3`.
#[must_use]
pub fn leaf(name: &str) -> String {
    let n = name.trim();
    n.rsplit('/')
        .next()
        .unwrap_or(n)
        .trim()
        .to_ascii_uppercase()
}

/// A ground-family net: the reference every other voltage is measured from,
/// or a named split of it (AGND, DGND, PGND, GNDA, ISOGND, a numbered GND2).
///
/// Not ground:
/// - a sense, LED-return, feedback or switched net named after ground
///   (`GND_SENSE`, `GNDLED`, `LED_GND`, `GND_SW`). It is a separate node that
///   reaches the plane through a shunt, a LED or a switch, and treating it as the
///   plane credits a pull-down or an Rd that is not there;
/// - `VEE`, a negative supply on a bipolar-rail board. Pinning it to 0 V would
///   short the rail.
#[must_use]
pub fn is_ground(name: &str) -> bool {
    let n = leaf(name);
    if n == "0" {
        return true;
    }
    // Numbering does not change what a net is: GND1, AGND_2, GND-3.
    let core =
        n.trim_end_matches(|c: char| c.is_ascii_digit() || matches!(c, '_' | '-' | '.' | ' '));
    let qualifier = ["GND", "GROUND", "VSS"]
        .iter()
        .find_map(|root| core.strip_prefix(root))
        .or_else(|| {
            ["GND", "GROUND"]
                .iter()
                .find_map(|root| core.strip_suffix(root))
        });
    qualifier.is_some_and(|q| {
        !q.split(|c: char| !c.is_ascii_alphanumeric())
            .map(|t| t.trim_end_matches(|c: char| c.is_ascii_digit()))
            .any(|t| NOT_THE_PLANE.contains(&t))
    })
}

/// Words that make a ground-named net something other than the ground plane.
const NOT_THE_PLANE: [&str; 11] = [
    "SENSE", "SENS", "SNS", "KELVIN", "LED", "LEDS", "FB", "MON", "MEAS", "DET", "SW",
];

/// The net the binder fuses onto node 0: `GND`, `VSS` or `0` exactly.
///
/// Deliberately narrower than [`is_ground`]: AGND, DGND, PGND, ISOGND and
/// `CHASSIS_GND` are distinct nets until something (a ferrite bead, a 0 ohm
/// link, a star point) joins them, and fusing them before binding erased the
/// board's ground topology. `VSS` is the IC-pin spelling of the ground itself,
/// so a board whose only ground is `VSS` still has a reference node.
#[must_use]
pub fn is_canonical_ground(name: &str) -> bool {
    matches!(leaf(name).as_str(), "0" | "GND" | "VSS")
}

/// The nominal voltage a supply net's name states, or `None` when the name
/// states none or is not a supply.
///
/// `None` is an answer, not a gap: bare `VDD` is the 3.3 V core rail on most
/// modern boards and 5 V on older ones, `VBAT` is anything from 2 V to 4.2 V, and
/// `VEE` is -5, -12 or -15 V. Stamping a guessed voltage overdrives or
/// underdrives every part on the net, which gives both false alarms and false
/// passes. Callers that only need to know the net is a supply ask [`is_supply`];
/// the binder names unpowered supplies through
/// [`names_a_supply_of_unknown_voltage`].
///
/// Bare `VCC` stays 5 V: it is the TTL convention and the one magnitude-free
/// name with a settled meaning.
#[must_use]
pub fn rail_voltage(name: &str) -> Option<f64> {
    let n = leaf(name);
    match n.as_str() {
        "+5V" | "5V" | "VCC" | "+VCC" | "VBUS" | "+5V0" | "VCC5" => Some(5.0),
        "+3V3" | "3V3" | "+3.3V" | "3.3V" | "VCC3V3" | "VDD3V3" | "VDD3P3" | "VCC3P3" | "+3P3"
        | "3P3" | "3P3V" => Some(3.3),
        "+1P8" | "1P8" | "VDD1P8" => Some(1.8),
        _ => {
            // A tap that watches a rail (`12V_FB`, `VCC_5V_MON`) is a divided
            // node below the rail, and an enable or status net named after it
            // (`3V3_EN`, `12V_PG`, `5V_DET`) is a signal. Neither is the rail,
            // and pinning one to the nominal shorts the divider or the driver.
            if name_is_rail_monitor_tap(&n) || has_signal_role_token(&n) {
                return None;
            }
            // '-'-prefixed rails first: "-5V_ANALOG" must resolve negative.
            let supply_token =
                n.starts_with('+') || n.contains("VCC") || n.contains("VBUS") || n.contains("VDD");
            negative_rail_fallback(&n)
                .or_else(|| positive_rail_fallback(&n))
                // "VCC_15V" contains "5V" and "VDD_13V3" contains "3V3": read the
                // whole embedded token before any substring test.
                .or_else(|| embedded_rail_magnitude(&n))
                // "VCC_5VOLTS": a supply token next to a 5V / 3V3 the parser
                // above cannot isolate. A bare domain-suffixed signal ("SDA_3V3")
                // carries no supply token and stays unresolved.
                .or_else(|| {
                    if supply_token && n.contains("5V") {
                        Some(5.0)
                    } else if supply_token && (n.contains("3V3") || n.contains("3.3V")) {
                        Some(3.3)
                    } else {
                        None
                    }
                })
        }
    }
}

/// Supply tokens a net name can carry without stating a voltage.
const SUPPLY_TOKENS: [&str; 15] = [
    "VDD", "VCC", "VEE", "VBAT", "VBUS", "VSYS", "VIN", "AVDD", "AVCC", "DVDD", "VREG", "VAUX",
    "VPP", "VIO", "VMOT",
];

/// A supply net, whether or not its name states the voltage.
///
/// `+3V3`, `VDD`, `VBAT`, `AVDD`, `-12V`. Never ground, never a tap or an
/// enable named after a rail. The predicate for "does this pull-up return to a rail", where the
/// magnitude does not matter.
#[must_use]
pub fn is_supply(name: &str) -> bool {
    rail_voltage(name).is_some() || names_a_supply_of_unknown_voltage(name)
}

/// A supply above ground: a positive stated voltage, or a supply that states
/// none and is not `VEE` (which is negative by convention). The predicate for
/// "this net sits at a logic high".
#[must_use]
pub fn is_positive_supply(name: &str) -> bool {
    rail_voltage(name).map_or_else(
        || names_a_supply_of_unknown_voltage(name) && !leaf(name).contains("VEE"),
        |v| v > 0.0,
    )
}

/// A net that names itself a supply but states no voltage: bare `VDD`, `VEE`,
/// `VBAT`, `VDD_A`, `AVDD`.
///
/// Name-only and loose on purpose: the binder pairs it with a count of the
/// parts on the net, so a short net that happens to carry a supply token does
/// not read as a dead rail.
#[must_use]
pub fn names_a_supply_of_unknown_voltage(name: &str) -> bool {
    if is_ground(name) || rail_voltage(name).is_some() {
        return false;
    }
    let n = leaf(name);
    if name_is_rail_monitor_tap(&n) || has_signal_role_token(&n) {
        return false;
    }
    // A leading '+' is `KiCad`'s power-symbol convention (`+BATT`, `+VDC`).
    n.starts_with('+') || SUPPLY_TOKENS.iter().any(|t| n.contains(t))
}

/// A name that may be a supply: [`is_supply`], or any other `V`-led name
/// (`VREF`, `VGATE`, `VCC_EN`) or a name led by a voltage (`9V_IN`).
///
/// The broad form, for the strap and boot checks, where a rail misread as a
/// signal raises a false "no bias" or "wrong pull" alarm and a signal misread as
/// a rail only silences an advisory. Never ground.
#[must_use]
pub fn may_be_supply(name: &str) -> bool {
    if is_ground(name) {
        return false;
    }
    let n = leaf(name);
    is_supply(name)
        || (n.starts_with('V') && n.len() >= 2)
        || (n.starts_with(|c: char| c.is_ascii_digit()) && n.contains('V'))
}

/// True when a net name carries an enable, status, monitor or select token.
///
/// `3V3_EN`, `1V8_PG`, `3V3_SEL`, `5V_DET`: a signal named after a rail rather
/// than the rail. Whole tokens only, so `VCCINT` is not an
/// interrupt line.
#[must_use]
pub fn has_signal_role_token(n: &str) -> bool {
    n.split(|c: char| !c.is_ascii_alphanumeric()).any(|t| {
        matches!(
            t.to_ascii_uppercase().as_str(),
            "EN" | "ENABLE"
                | "PG"
                | "PGOOD"
                | "POWERGOOD"
                | "GOOD"
                | "OK"
                | "SEL"
                | "SELECT"
                | "DET"
                | "DETECT"
                | "MON"
                | "MONITOR"
                | "STAT"
                | "STATUS"
                | "FLT"
                | "FAULT"
                | "INT"
                | "IRQ"
                | "RST"
                | "RESET"
                | "CTRL"
                | "CTL"
        )
    })
}

/// A magnitude embedded mid-name after a supply token (`VDD_1V8`, `AVCC_2V5`,
/// `VDD_1.2V`), in `KiCad`'s digit-V-digit form or the dotted form. Gated on a
/// supply token so a signal that merely contains "1V2" is not a rail. No upper
/// clamp: `VBUS_65V` is a 65 V rail, and a clamp let the loose "contains 5V"
/// branch read it as 5 V.
fn embedded_rail_magnitude(n: &str) -> Option<f64> {
    const TOKENS: [&str; 11] = [
        "VDD", "VCC", "VBUS", "VIN", "VSYS", "VBAT", "AVDD", "DVDD", "VOUT", "VREG", "VAUX",
    ];
    if !(n.starts_with('+') || TOKENS.iter().any(|t| n.contains(t))) {
        return None;
    }
    let b = n.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if !b[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i + 1 < b.len() && b[i] == b'.' && b[i + 1].is_ascii_digit() {
            i += 1;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
        }
        let head = &n[start..i];
        if i < b.len() && b[i] == b'V' {
            let mut j = i + 1;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            let frac = &n[i + 1..j];
            let mag: Option<f64> = if head.contains('.') || frac.is_empty() {
                head.parse().ok()
            } else {
                format!("{head}.{frac}").parse().ok()
            };
            if let Some(m) = mag.filter(|m| *m > 0.0 && m.is_finite()) {
                return Some(m);
            }
        }
    }
    None
}

/// A voltage token immediately followed by a tap suffix (`_MON`, `_FB`,
/// `_SENSE`): a divided tap of the rail, never the rail node.
fn name_is_rail_monitor_tap(n: &str) -> bool {
    let b = n.as_bytes();
    (1..b.len()).any(|i| {
        if b[i] != b'V' || !b[i - 1].is_ascii_digit() {
            return false;
        }
        let mut j = i + 1;
        while j < b.len() && b[j].is_ascii_digit() {
            j += 1;
        }
        is_rail_monitor_suffix(&n[j..])
    })
}

/// `+15V`, `+24V_RAIL`, `+15V0`, `9V`: an optional '+', then a magnitude in
/// plain or `KiCad` digit-V-digit form at the start of the name.
fn positive_rail_fallback(n: &str) -> Option<f64> {
    rail_magnitude(n.strip_prefix('+').unwrap_or(n))
}

/// `-5V`, `-12V_RAIL`, `-3V3_ANALOG`: the negative mirror of
/// [`positive_rail_fallback`]. Exactly one sign.
fn negative_rail_fallback(n: &str) -> Option<f64> {
    rail_magnitude(n.strip_prefix('-')?).map(|magnitude| -magnitude)
}

fn rail_magnitude(rest: &str) -> Option<f64> {
    let int_part: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    if int_part.is_empty() {
        return None;
    }
    let after = rest[int_part.len()..].strip_prefix('V')?;
    let frac: String = after.chars().take_while(char::is_ascii_digit).collect();
    if is_rail_monitor_suffix(&after[frac.len()..]) {
        return None;
    }
    let magnitude: f64 = if frac.is_empty() {
        int_part.parse().ok()?
    } else {
        format!("{}.{}", int_part.trim_end_matches('.'), frac)
            .parse()
            .ok()?
    };
    (magnitude > 0.0 && magnitude.is_finite()).then_some(magnitude)
}

/// True when the text after a rail's voltage token is a monitor, feedback or
/// sense suffix: the net is a divided tap of the rail. Matched on the root so
/// `DIVIDER`, `SENSED`, `MEASURE` and `MONITORED` count, except the rail-domain
/// words `SENSOR` and `FBUS`, which share a root's letters but name rails.
fn is_rail_monitor_suffix(tail: &str) -> bool {
    const RAIL_DOMAIN_EXCEPTIONS: [&str; 2] = ["SENSOR", "FBUS"];
    const TAP_ROOTS: [&str; 8] = ["FB", "FEEDBACK", "SENS", "MON", "DIV", "TAP", "MEAS", "SNS"];
    let first: String = tail
        .trim_start_matches(|c: char| !c.is_ascii_alphanumeric())
        .chars()
        .take_while(char::is_ascii_alphanumeric)
        .collect();
    if RAIL_DOMAIN_EXCEPTIONS.contains(&first.as_str()) {
        return false;
    }
    TAP_ROOTS.iter().any(|r| first.starts_with(r))
}

#[cfg(test)]
mod tests {
    use super::{
        is_canonical_ground, is_ground, is_supply, names_a_supply_of_unknown_voltage, rail_voltage,
    };

    /// What each class means, by name. One row per decision, with its reason.
    #[derive(Debug, Clone, Copy, PartialEq)]
    enum Class {
        /// The ground plane or a named split of it.
        Ground,
        /// A supply whose name states its voltage.
        Rail(f64),
        /// A supply whose name states no voltage: `rail_voltage` is `None`.
        Supply,
        /// Neither ground nor a supply.
        Signal,
    }
    use Class::{Ground, Rail, Signal, Supply};

    const GOLDEN: &[(&str, Class, &str)] = &[
        // Ground and its named splits.
        ("GND", Ground, "the reference"),
        ("/Power/GND", Ground, "only the leaf decides"),
        ("0", Ground, "SPICE node 0"),
        ("VSS", Ground, "the IC-pin spelling of ground"),
        ("VSSA", Ground, "analog VSS"),
        ("AGND", Ground, "a named split, still ground"),
        ("DGND", Ground, "a named split, still ground"),
        ("PGND", Ground, "power ground"),
        ("GNDA", Ground, "suffix form of AGND"),
        ("GNDD", Ground, "suffix form of DGND"),
        ("GNDIO", Ground, "I/O ground"),
        ("GNDPWR", Ground, "power ground"),
        ("GNDREF", Ground, "reference ground"),
        ("EGND", Ground, "earth ground"),
        ("SGND", Ground, "signal ground"),
        ("ISOGND", Ground, "isolated-side ground"),
        ("CHASSIS_GND", Ground, "chassis ground"),
        ("GND1", Ground, "numbering does not change the class"),
        ("AGND_2", Ground, "numbering does not change the class"),
        ("GND_ISO", Ground, "isolated-side ground"),
        // Ground-named nets that are not the plane.
        (
            "GND_SENSE",
            Signal,
            "a Kelvin sense node, reaches GND through a shunt",
        ),
        (
            "GNDLED",
            Signal,
            "an LED return, switched or current-limited",
        ),
        ("GND_LED1", Signal, "an LED return, numbered"),
        ("LED_GND", Signal, "an LED return, prefix form"),
        ("VSS_SENSE", Signal, "a sense node"),
        ("GND_SW", Signal, "a switched low-side return"),
        ("GND_FB", Signal, "a feedback node"),
        (
            "NET-(U1-GND)",
            Signal,
            "a tool-named net after a pin, not ground by name",
        ),
        // VEE is a negative supply.
        (
            "VEE",
            Supply,
            "negative rail, magnitude board-dependent (-5/-12/-15)",
        ),
        // Rails that state their voltage.
        ("+5V", Rail(5.0), "states 5 V"),
        ("VCC", Rail(5.0), "TTL convention"),
        ("VBUS", Rail(5.0), "USB bus voltage"),
        ("VCC5", Rail(5.0), "VCC 5 V spelling"),
        ("VCC5V", Rail(5.0), "embedded magnitude"),
        ("+3V3", Rail(3.3), "states 3.3 V"),
        ("/Power/+3V3", Rail(3.3), "only the leaf decides"),
        ("VDD3P3", Rail(3.3), "P as the decimal point"),
        ("+3V3A", Rail(3.3), "analog 3.3 V"),
        ("+3V0", Rail(3.0), "states 3.0 V"),
        ("+2V8", Rail(2.8), "states 2.8 V"),
        ("VDD1V8", Rail(1.8), "embedded magnitude"),
        ("1V8_CORE", Rail(1.8), "a domain suffix, not a signal role"),
        ("+12V", Rail(12.0), "states 12 V"),
        ("+15V", Rail(15.0), "not the 5V substring"),
        ("+15V0", Rail(15.0), "KiCad digit-V-digit"),
        ("+15V_ANALOG", Rail(15.0), "domain suffix"),
        ("+9V", Rail(9.0), "states 9 V"),
        ("+5V_USB", Rail(5.0), "domain suffix"),
        ("VCC_5V", Rail(5.0), "embedded magnitude"),
        ("-15V", Rail(-15.0), "negative rail"),
        ("-5V", Rail(-5.0), "negative rail"),
        ("-3V3_ANALOG", Rail(-3.3), "negative rail, domain suffix"),
        ("VDD_1V8", Rail(1.8), "embedded magnitude"),
        ("AVCC_2V5", Rail(2.5), "embedded magnitude"),
        ("DVDD_0V9", Rail(0.9), "embedded magnitude"),
        ("VDD_1.2V", Rail(1.2), "dotted form"),
        ("VCC_15V", Rail(15.0), "not the 5V substring"),
        ("VDD_13V3", Rail(13.3), "not the 3V3 substring"),
        ("VDD_3V3", Rail(3.3), "embedded magnitude"),
        ("VCC_3.3V", Rail(3.3), "dotted form"),
        ("VBUS_65V", Rail(65.0), "no upper clamp"),
        ("VDD_63V3", Rail(63.3), "no upper clamp"),
        ("+65V", Rail(65.0), "no upper clamp"),
        ("3V3", Rail(3.3), "states 3.3 V"),
        ("+3V3_DIG", Rail(3.3), "domain suffix"),
        ("12V", Rail(12.0), "states 12 V"),
        ("24V", Rail(24.0), "states 24 V"),
        ("+25V", Rail(25.0), "not the 5V substring"),
        ("+35V", Rail(35.0), "not the 5V substring"),
        ("15V0", Rail(15.0), "KiCad digit-V-digit"),
        ("48V", Rail(48.0), "states 48 V"),
        ("60V", Rail(60.0), "states 60 V"),
        (
            "100V",
            Rail(100.0),
            "no upper clamp: a high-voltage LED string",
        ),
        ("+400V", Rail(400.0), "no upper clamp: a PFC bus"),
        ("5V_SENSOR", Rail(5.0), "SENSOR is a domain, not a tap"),
        ("3V3_FBUS", Rail(3.3), "FBUS is a domain, not a tap"),
        // Supplies that state no voltage: unknown, never guessed.
        (
            "VDD",
            Supply,
            "3.3 V core on most boards, 5 V on others: unknown",
        ),
        ("VBAT", Supply, "anything from 2 V to 4.2 V: unknown"),
        ("VIN", Supply, "input of unknown magnitude"),
        ("VSYS", Supply, "charger system rail, unknown"),
        ("VPP", Supply, "programming supply, unknown"),
        ("VDDIO", Supply, "I/O supply, unknown"),
        ("VDD_IO", Supply, "I/O supply, unknown"),
        ("AVDD", Supply, "analog supply, unknown"),
        ("VDD_A", Supply, "analog supply, unknown"),
        ("VMOT", Supply, "motor supply, unknown"),
        ("+BATT", Supply, "a '+' power symbol of unknown voltage"),
        // Signals named after rails.
        (
            "SDA_3V3",
            Signal,
            "an I2C line on the 3.3 V side of a level shifter",
        ),
        ("SCL_3V3", Signal, "an I2C line"),
        ("IRQ_3.3V", Signal, "an interrupt line"),
        ("SDA_5V", Signal, "an I2C line"),
        (
            "3V3_EN",
            Signal,
            "an enable named after the rail it switches",
        ),
        ("5V_DET", Signal, "a detect line"),
        ("5V_DETECT", Signal, "a detect line"),
        ("1V8_EN", Signal, "an enable"),
        ("3V3_PG", Signal, "a power-good flag"),
        ("3V3_SEL", Signal, "a select line"),
        ("1V8_PGOOD", Signal, "a power-good flag"),
        (
            "MCU_3V3",
            Signal,
            "a voltage behind a word that is not a supply token: by name alone it \
             cannot be told from SDA_3V3; netlint credits such a local rail by its \
             bypass capacitor instead",
        ),
        ("12V_PG", Signal, "a power-good flag"),
        ("VDD_OK", Signal, "a power-good flag named after the rail"),
        ("PWR_3V3_OK", Signal, "a power-good flag"),
        ("VCC_EN", Signal, "an enable named after the rail"),
        ("SENSE_1V2_MON", Signal, "a monitor tap"),
        ("12V_FB", Signal, "a feedback tap"),
        ("3V3_SENSE", Signal, "a sense tap"),
        ("5V_MON", Signal, "a monitor tap"),
        ("24V_MON", Signal, "a monitor tap"),
        ("12V_DIV", Signal, "a divider tap"),
        ("-12V_MON", Signal, "a monitor tap"),
        ("VDD_1V8_MON", Signal, "a monitor tap"),
        ("VCC_5V_MON", Signal, "a monitor tap"),
        ("VOUT_1V0_FB", Signal, "a feedback tap"),
        ("12V_DIVIDER", Signal, "a divider tap"),
        ("3V3_SENSED", Signal, "a sense tap"),
        ("5V_SENSING", Signal, "a sense tap"),
        ("12V_MEASURE", Signal, "a measurement tap"),
        ("5V_MONITORED", Signal, "a monitor tap"),
        ("VDD_1V8_DIVIDER", Signal, "a divider tap"),
        ("-12V_SENSED", Signal, "a sense tap"),
        ("SDA", Signal, "plain signal"),
        ("NET-(R1-PAD2)", Signal, "tool-named net"),
    ];

    fn class_of(name: &str) -> Class {
        let ground = is_ground(name);
        let volts = rail_voltage(name);
        let unknown = names_a_supply_of_unknown_voltage(name);
        assert_eq!(
            is_supply(name),
            volts.is_some() || unknown,
            "{name}: is_supply"
        );
        assert!(
            !(ground && is_supply(name)),
            "{name}: ground and supply at once"
        );
        match (ground, volts, unknown) {
            (true, _, _) => Ground,
            (false, Some(v), _) => Rail(v),
            (false, None, true) => Supply,
            (false, None, false) => Signal,
        }
    }

    #[test]
    fn golden_net_name_table() {
        let mut wrong = Vec::new();
        for (name, want, why) in GOLDEN {
            let got = class_of(name);
            if got != *want {
                wrong.push(format!("{name}: got {got:?}, want {want:?} ({why})"));
            }
        }
        assert!(wrong.is_empty(), "misclassified:\n{}", wrong.join("\n"));
    }

    #[test]
    fn a_rail_magnitude_takes_one_sign_and_a_finite_positive_number() {
        for magnitude in ["+5V", "-5V", "NaNV", "infV", "0V", "1..2V", "5V_MON"] {
            assert_eq!(
                super::positive_rail_fallback(&format!("+{magnitude}")),
                None
            );
            assert_eq!(
                super::negative_rail_fallback(&format!("-{magnitude}")),
                None
            );
        }
    }

    #[test]
    fn canonical_ground_is_only_gnd_vss_and_node_zero() {
        for name in ["GND", "/sheet/GND", "gnd", "VSS", "0"] {
            assert!(is_canonical_ground(name), "{name}");
        }
        for name in ["AGND", "DGND", "PGND", "CHASSIS_GND", "VEE", "GND_SENSE"] {
            assert!(!is_canonical_ground(name), "{name}");
        }
    }
}
