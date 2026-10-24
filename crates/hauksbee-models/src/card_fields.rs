//! The card fields a board check needs from an extracted card, asked for in the
//! drafting prompt and checked on the draft.
//!
//! A drafted card passes the schema validator with all of these left out, and
//! then the board check that reads them has nothing to judge and says nothing.
//! Measured on automatically extracted cards, five groups were missing:
//!
//! 1. A load switch's or motor driver's enable behaviour (`en_open`,
//!    `en_pull_*`) and its supply capacitors (`[[models.supply_cap]]`).
//! 2. A motor driver's `[models.pins]` role map (its pinout lived in notes).
//! 3. `[[models.pin_bias]]` rows for a ROM-bootloader UART receive pin and for
//!    a driver's single-wire UART pin.
//! 4. `[models.pin_capability.open_drain]` pads, although coverage says the
//!    part has an open-drain characteristic.
//! 5. An MCU's recommended crystal load-capacitor range.
//!
//! [`warnings`] is the card-side check. It joins the lint warnings of
//! [`crate::datasheet`] (the same list `ds check` shows the drafting agent), so
//! a draft that leaves a group out is redrafted once with the warning quoted.
//! Every warning has the same honest exit as the coverage checklist: a
//! `[[models.source.uncertainty]]` `unknown` record whose `parameter` names the
//! group and whose reason says the datasheet prints no such row.
//!
//! [`prompt_guidance`] is the matching request, in the card schema the library
//! cards use ([`crate::schema`]); [`checklist_lines`] is the same list for the
//! `ds checklist` helper.

use hauksbee_ir::evidence::ModelUncertainty;

use crate::boot_mode::RomProtocol;
use crate::schema::{ComponentKind, ModelEntry};

/// Roles a digital part's enable pin takes for the floating-enable check
/// (the set `validate_digital_enable_params` and the board check read).
const ENABLE_ROLES: [&str; 7] = [
    "en", "en_n", "enable", "enable_n", "shdn", "shdn_n", "on_off",
];

/// Roles a motor driver's motor supply takes.
const MOTOR_SUPPLY_ROLES: [&str; 8] = [
    "vm", "vmot", "vbb", "vs", "vmotor", "vm1", "vbb1", "vsupply",
];

/// Words that make a card a motor driver.
const MOTOR_WORDS: [&str; 7] = [
    "motor driver",
    "motor controller",
    "motor-driver",
    "h-bridge",
    "half-bridge driver",
    "stepper",
    "brushed dc",
];

/// Words that say a card describes a ROM bootloader on a serial port.
const BOOTLOADER_WORDS: [&str; 5] = [
    "bootloader",
    "system memory",
    "rom boot",
    "serial boot",
    "uart boot",
];

fn has_role(entry: &ModelEntry, pred: impl Fn(&str) -> bool) -> bool {
    entry.pins.values().any(|r| pred(&r.to_ascii_lowercase()))
}

/// The card's own prose: description, id, source titles and coverage ids, lower-cased.
fn prose(entry: &ModelEntry) -> String {
    let mut text = format!("{} {}", entry.id, entry.description);
    if let Some(source) = &entry.source {
        for r in &source.references {
            text.push(' ');
            text.push_str(&r.title);
        }
    }
    for id in entry
        .coverage
        .implements
        .iter()
        .chain(&entry.coverage.missing)
    {
        text.push(' ');
        text.push_str(id);
    }
    text.to_ascii_lowercase()
}

/// An `unknown` or interval record whose parameter names one of `keys`: the
/// drafter looked and says the datasheet prints no such row.
fn recorded(entry: &ModelEntry, keys: &[&str]) -> bool {
    entry.source.as_ref().is_some_and(|s| {
        s.uncertainty.iter().any(|u| {
            let (ModelUncertainty::Interval { parameter, .. }
            | ModelUncertainty::Unknown { parameter, .. }) = u;
            let p = parameter.to_ascii_lowercase();
            keys.iter().any(|k| p.contains(k))
        })
    })
}

/// Whether the card is a high-side load switch.
///
/// It is an analog-switch card mapping a switched path (`in_out_a`, `in_out_b`) and
/// an enable (`ctrl`), that says it is one. A signal switch or multiplexer states no
/// enable level and is not asked for these fields.
#[must_use]
pub fn is_load_switch(entry: &ModelEntry) -> bool {
    entry.kind == ComponentKind::AnalogSwitch
        && has_role(entry, |r| r == "in_out_a")
        && has_role(entry, |r| r == "in_out_b")
        && (entry.params.get_str("en_on_level").is_some() || {
            let text = prose(entry);
            [
                "load switch",
                "power switch",
                "high-side switch",
                "load-switch",
            ]
            .iter()
            .any(|w| text.contains(w))
        })
}

/// A motor driver: a card whose prose names a motor driver, H-bridge or stepper
/// driver, or that maps a motor-supply role beside output-bridge roles. An LED
/// or gate driver is not one.
#[must_use]
pub fn is_motor_driver(entry: &ModelEntry) -> bool {
    if !matches!(entry.kind, ComponentKind::Digital | ComponentKind::Vreg) {
        return false;
    }
    let text = prose(entry);
    MOTOR_WORDS.iter().any(|w| text.contains(w))
        || (has_role(entry, |r| MOTOR_SUPPLY_ROLES.contains(&r))
            && has_role(entry, |r| {
                r.starts_with("aout") || r.starts_with("bout") || r.starts_with("out1")
            }))
}

/// Whether the card declares a serial ROM bootloader.
///
/// That is a `[models.rom_bootloader]` block with a serial protocol, else prose that
/// names one. A USB-only boot ROM (BOOTSEL) listens on no UART pin and is not asked
/// for a bias row.
#[must_use]
pub fn declares_serial_bootloader(entry: &ModelEntry) -> bool {
    if entry.kind != ComponentKind::Mcu {
        return false;
    }
    if let Some(rom) = &entry.rom_bootloader {
        return rom.protocol != RomProtocol::UsbOnly;
    }
    let mut text = prose(entry);
    for s in &entry.straps {
        text.push(' ');
        text.push_str(&s.note.to_ascii_lowercase());
    }
    for m in &entry.boot_modes {
        text.push(' ');
        text.push_str(&format!("{} {}", m.name, m.detail).to_ascii_lowercase());
    }
    BOOTLOADER_WORDS.iter().any(|w| text.contains(w))
}

/// Whether `kind` is an IC (a part with pins a datasheet describes), as opposed
/// to a passive, discrete or connector.
const fn is_ic_kind(kind: ComponentKind) -> bool {
    !matches!(
        kind,
        ComponentKind::Passive
            | ComponentKind::Diode
            | ComponentKind::BjtNpn
            | ComponentKind::BjtPnp
            | ComponentKind::Nmos
            | ComponentKind::Pmos
            | ComponentKind::Connector
            | ComponentKind::Ignore
    )
}

/// Whether coverage claims an open-drain characteristic (`open_drain_low_level`,
/// `open_drain_output`, `open_collector_flag`), listed as modelled or not.
fn claims_open_drain(entry: &ModelEntry) -> bool {
    entry
        .coverage
        .implements
        .iter()
        .chain(&entry.coverage.missing)
        .any(|id| {
            let id = id.to_ascii_lowercase().replace(['-', ' '], "_");
            id.contains("open_drain") || id.contains("open_collector") || id.contains("opendrain")
        })
}

fn has_open_drain_pad(entry: &ModelEntry) -> bool {
    entry
        .pin_capability
        .open_drain
        .as_ref()
        .is_some_and(|t| !t.pads.is_empty())
}

/// Whether the card states what its enable pin does when left open: `en_open`,
/// an internal pull, or a float rule the datasheet's words back.
fn states_open_enable(entry: &ModelEntry) -> bool {
    let p = &entry.params.0;
    p.contains_key("en_open")
        || p.keys().any(|k| k.starts_with("en_pull_"))
        || p.contains_key("must_not_float_roles")
        || p.contains_key("float_tolerated_roles")
        || recorded(entry, &["en_open", "enable"])
}

/// The honest exit every warning offers: an `unknown` record naming the field.
fn fix_unknown(param: &str) -> String {
    format!(
        "or, if the datasheet prints no such row, a [[models.source.uncertainty]] `unknown` \
         record with parameter = \"{param}\" whose reason says so"
    )
}

/// The motor-driver role-map and single-wire UART bias warnings.
fn motor_pin_warnings(entry: &ModelEntry, out: &mut Vec<String>) {
    let roles_ok = has_role(entry, |r| {
        MOTOR_SUPPLY_ROLES.contains(&r) || r.starts_with("vm")
    });
    if !roles_ok {
        out.push(
            "this motor driver card has no usable [models.pins] role map: key every pad of \
             the selected package by number to a lowercase role from the Pin Functions \
             table (the motor supply as vm / vbb, outputs as out1a/aout1 style roles, \
             enable as en or en_n, the logic supply as vcc or vdd, gnd): a pinout kept only \
             in notes binds nothing"
                .to_owned(),
        );
    }
    for (pad, role) in &entry.pins {
        let r = role.to_ascii_lowercase();
        if (r.contains("uart") || r == "pdn" || r.contains("single_wire"))
            && !entry
                .pin_bias
                .iter()
                .any(|b| b.role.eq_ignore_ascii_case(role))
            && !recorded(entry, &["pin_bias", r.as_str()])
        {
            out.push(format!(
                "pad {pad} ('{role}') is a single-wire UART pin and has no \
                 [[models.pin_bias]] row: if the datasheet gives the idle level or the pull it \
                 needs (an idle-high line wants a pull-up), add role = \"{r}\", level, needs \
                 (\"pull\" or \"idle\"), min_ohm / max_ohm when printed, and a basis with the \
                 page; {}",
                fix_unknown("pin_bias")
            ));
        }
    }
}

/// The crystal load-capacitor warning for an MCU with oscillator pads.
fn xtal_warning(entry: &ModelEntry) -> Option<String> {
    let has_osc = has_role(entry, |r| {
        r.contains("osc_in") || r.contains("osc_out") || r.contains("xtal") || r == "xin"
    });
    let stated = entry.params.0.contains_key("xtal_cap_min_f")
        || entry.params.0.contains_key("xtal_cap_max_f")
        || !entry.pin_capability.xtal_int_caps.is_empty()
        || recorded(entry, &["xtal"]);
    (entry.kind == ComponentKind::Mcu && has_osc && !stated).then(|| {
        format!(
            "this MCU card maps oscillator pins but states no crystal load-capacitor range: \
             add params xtal_cap_min_f / xtal_cap_max_f (farads) from the oscillator \
             characteristics or the clock chapter's range for the external capacitors C1 / C2, \
             or [[models.pin_capability.xtal_int_caps]] when the load capacitors are on the die; \
             {}",
            fix_unknown("xtal_cap_range")
        )
    })
}

/// What a card of one of the kinds above leaves out, worded as the fix.
#[must_use]
pub fn warnings(entry: &ModelEntry) -> Vec<String> {
    let mut out = Vec::new();
    let load_switch = is_load_switch(entry);
    let motor = is_motor_driver(entry);
    if load_switch || motor {
        let noun = if load_switch {
            "load switch"
        } else {
            "motor driver"
        };
        if entry.supply_cap.is_empty() && !recorded(entry, &["supply_cap"]) {
            out.push(format!(
                "this {noun} card has no [[models.supply_cap]] row: read the input/supply \
                 capacitor{} the datasheet requires or recommends (Recommended Operating \
                 Conditions CIN/COUT/CBULK rows, the pin description, Application Information) \
                 and write each as role, side, grade, min_f in farads and a basis with the page; \
                 {}",
                if load_switch {
                    " and output capacitor"
                } else {
                    ""
                },
                fix_unknown("supply_cap")
            ));
        }
    }

    // The enable pin: what an open one does. A load switch always has one; a
    // motor driver only when its pin map names an enable role the check reads.
    let enable_role = ENABLE_ROLES
        .iter()
        .find(|r| has_role(entry, |x| x == **r))
        .copied();
    if (load_switch || (motor && enable_role.is_some())) && !states_open_enable(entry) {
        let pin = if load_switch {
            "ctrl (the enable)".to_owned()
        } else {
            enable_role.unwrap_or("en").to_owned()
        };
        out.push(format!(
            "the enable pin {pin} states no open-pin behaviour: add param en_open = \"off\" or \
             \"on\" (or \"undefined\" with ctrl in must_not_float_roles, load switch only) or the \
             internal pull en_pull_down_ohms / en_pull_up_ohms the pin description prints, with \
             en_open_basis quoting the datasheet's words and page; {}",
            fix_unknown("en_open")
        ));
    }

    if motor {
        motor_pin_warnings(entry, &mut out);
    }

    if claims_open_drain(entry) && !has_open_drain_pad(entry) && !recorded(entry, &["open_drain"]) {
        out.push(format!(
            "coverage names an open-drain characteristic but [models.pin_capability.open_drain] \
             lists no pad: add `basis` (the pin-description table and page that say \"open \
             drain\" / type OD) and [models.pin_capability.open_drain.pads] mapping each \
             open-drain output pad number to the datasheet's name for it (\"4\" = \"ALERT\"); {}",
            fix_unknown("open_drain")
        ));
    }

    if declares_serial_bootloader(entry)
        && entry.pin_bias.is_empty()
        && !recorded(entry, &["pin_bias", "bootloader"])
    {
        out.push(format!(
            "this MCU card has a ROM bootloader but no [[models.pin_bias]] row: for each UART \
             receive pin the boot chapter (or the vendor's bootloader application note) says the \
             bootloader listens on, add role = <that pin's role>, level = \"high\", needs = \
             \"idle\", opposed_only = true and a basis quoting the document and page; {}",
            fix_unknown("pin_bias")
        ));
    }

    out.extend(xtal_warning(entry));
    out
}

/// The same asks as one-line checklist items, for the `ds checklist` helper,
/// from the kind and the roles the checklist is asked about.
#[must_use]
pub fn checklist_lines(kind: ComponentKind, roles: &[&str]) -> Vec<String> {
    let has = |wanted: &[&str]| {
        roles
            .iter()
            .any(|r| wanted.contains(&r.to_ascii_lowercase().as_str()))
    };
    let mut out = Vec::new();
    let load_switch =
        kind == ComponentKind::AnalogSwitch && has(&["in_out_a"]) && has(&["in_out_b"]);
    let motor =
        matches!(kind, ComponentKind::Digital | ComponentKind::Vreg) && has(&MOTOR_SUPPLY_ROLES);
    if load_switch || motor {
        out.push(
            "supply_cap: [[models.supply_cap]] per capacitor the datasheet requires or \
             recommends to ground (input / supply, and a load switch's output): role, side, \
             grade, min_f farads, basis  (look under: Recommended operating conditions, pin \
             descriptions, application information)"
                .to_owned(),
        );
    }
    if load_switch || (motor && has(&ENABLE_ROLES)) {
        out.push(
            "en_open: param en_open (off / on) or en_pull_down_ohms / en_pull_up_ohms for the \
             enable pin, with en_open_basis quoting the words  (look under: pin descriptions, \
             enable truth table)"
                .to_owned(),
        );
    }
    if kind == ComponentKind::Mcu {
        out.push(
            "pin_bias: a [[models.pin_bias]] row for each UART RX pin a ROM bootloader listens \
             on (level high, needs idle, opposed_only), from the boot chapter or the \
             bootloader application note"
                .to_owned(),
        );
        out.push(
            "xtal_cap: params xtal_cap_min_f / xtal_cap_max_f for the external crystal load \
             capacitors (or pin_capability.xtal_int_caps for on-die ones)  (look under: \
             oscillator characteristics)"
                .to_owned(),
        );
    }
    if is_ic_kind(kind) {
        out.push(
            "open_drain: [models.pin_capability.open_drain] pads for every output the pin \
             description types open-drain / open-collector (a part with none: leave it out)"
                .to_owned(),
        );
    }
    out
}

/// Prompt text asking for these fields, in the library cards' schema, for a card
/// of `kind`. Empty for a kind none of them applies to.
#[must_use]
pub fn prompt_guidance(kind: &str) -> String {
    let mut out = String::new();
    let ics = matches!(
        kind,
        "vreg"
            | "charger"
            | "pmic"
            | "analog_switch"
            | "digital"
            | "shift_register"
            | "mcu"
            | "comparator"
            | "opamp"
            | "adc"
            | "dac"
            | "timer"
    );
    if !ics {
        return out;
    }
    out.push_str(SUPPLY_CAP_GUIDANCE);
    out.push_str(OPEN_DRAIN_GUIDANCE);
    match kind {
        "analog_switch" => out.push_str(LOAD_SWITCH_GUIDANCE),
        "digital" => out.push_str(MOTOR_DRIVER_GUIDANCE),
        "mcu" => out.push_str(MCU_GUIDANCE),
        _ => {}
    }
    out
}

const SUPPLY_CAP_GUIDANCE: &str = r#"
CAPACITORS THE DATASHEET REQUIRES (`[[models.supply_cap]]`): read the Recommended
Operating Conditions (CIN, COUT, CBULK, CVM rows), each supply and output pin's
description ("bypass to GND with 1 uF", "a 10 uF minimum ceramic capacitor is
recommended") and the Application / Layout sections. One row per pin to ground:

  [[models.supply_cap]]
  role = "vm"                    # the pin's role in [models.pins]
  side = "input"                 # "input" (a supply or input) or "output"
  grade = "required"             # "required": must, minimum, a Recommended Operating
                                 # Conditions MIN; "recommended": a suggested value
  min_f = 1.0e-5                 # farads: 10 uF is 1.0e-5
  min_single_f = 1.0e-5          # only when ONE capacitor must reach it (a bulk part)
  basis = "<table or sentence, with the PDF page>"
  note = "<type or voltage condition, optional>"
  applies_when = "usb_downstream"  # only when the datasheet states the figure for one kind
                                 # of load ("For the USB application, each downstream
                                 # facing port requires at least 120 uF"); omit otherwise.
                                 # The general-application figure is its own row.

Only a capacitor from the pin to ground belongs here (a charge-pump capacitor
between two pins does not). A linear regulator's output capacitor is cout_min_f,
not a row. Leave the rows out when the datasheet states none, and say so in an
`unknown` source record with parameter = "supply_cap".
"#;

const OPEN_DRAIN_GUIDANCE: &str = r#"
OPEN-DRAIN OUTPUTS (`[models.pin_capability.open_drain]`): every pad whose pin
description types it open-drain or open-collector (type OD, "open-drain", ALERT,
INT, FAULT, PGOOD and flag outputs) is listed, so a board check can ask for its
pull-up. Coverage that names an open-drain behaviour without the table is refused:

  [models.pin_capability.open_drain]
  basis = "<pin-description table and PDF page that say open-drain>"
  [models.pin_capability.open_drain.pads]
  "8" = "nFAULT"                 # pad number = the datasheet's name for the signal

Pads by the number keyed in [models.pins]. A part with no open-drain output omits
the table.
"#;

const LOAD_SWITCH_GUIDANCE: &str = r#"
LOAD SWITCH (a high-side power switch with an enable pin and a switched output:
kind analog_switch with `in_out_b` = input, `in_out_a` = switched output, `ctrl` =
enable, `vss` = ground): the enable's behaviour when nothing drives it decides
whether the output is on during reset, so read the pin description, the Device
Functional Modes and the enable truth table, and write in [models.params]:

  en_on_level = "high"           # the enable polarity; the switch binder carries
                                 # active-high ordering options only, so match
                                 # an active-low option out in value_re
  en_open = "off"                # what an open enable does ("off", "on", or
                                 # "undefined" when the datasheet says it must not
                                 # float: then also must_not_float_roles = "ctrl")
  en_pull_down_ohms = 7.5e5      # OR en_pull_up_ohms, the internal pull the pin
                                 # description prints (at most one pull)
  en_open_basis = "<the datasheet's words and PDF page for the open pin>"

State en_open or a pull only when the datasheet prints it; if it prints neither,
record an `unknown` source record with parameter = "en_open". The input and output
capacitors are `[[models.supply_cap]]` rows (role in_out_b side input, role
in_out_a side output).
"#;

const MOTOR_DRIVER_GUIDANCE: &str = r#"
MOTOR DRIVER (H-bridge, stepper or brushed-DC driver): [models.pins] MUST be
written, one lowercase role per pad of the selected package from the Pin Functions
table (the motor supply `vm` or `vbb`, bridge outputs such as `aout1`, `bout2`,
the enable `en` / `en_n` / `enable`, sleep, inputs, the logic supply, gnd, the
exposed pad `gnd_ep`); a pinout left in notes binds nothing. Then read:
- The motor supply capacitor (bulk and bypass) as [[models.supply_cap]] rows on
  the motor-supply role, from the pin description and Application Information
  (a bulk value of 100 uF is `min_f = 1.0e-4`, `min_single_f = 1.0e-4`).
- The enable pin: params en_open ("off" / "on") or en_pull_down_ohms /
  en_pull_up_ohms with en_open_basis quoting the words, when the pin description
  states what an open enable does. `float_tolerated_roles` / `must_not_float_roles`
  carry a pin the datasheet says may, or must not, float.
- A single-wire UART pin (PDN_UART, UART, one-wire configuration) that needs an
  idle-high level or a pull-up: a [[models.pin_bias]] row
    role = "pdn_uart"  level = "high"  needs = "pull"  (or "idle")
    min_ohm / max_ohm = the printed pull range  basis = "<words and PDF page>"
  A pin the datasheet says nothing about gets no row.
"#;

const MCU_GUIDANCE: &str = r#"
MCU FIELDS THE BOARD CHECKS READ:
- ROM bootloader serial pins: when the boot chapter (or the vendor's bootloader
  application note, such as AN2606 / AN3155 for STM32) says the ROM bootloader
  listens on a UART receive pin, write a [[models.pin_bias]] row for that pin:
    role = "<the pin's role>"  level = "high"  needs = "idle"  opposed_only = true
    basis = "<document, table and PDF page>"  note = "<why the idle level matters>"
  `opposed_only` because the pin is an ordinary port pin when the bootloader is
  unused; a boot STRAP pin (BOOT0, GPIO0) is a [[models.straps]] row, never here.
- Crystal load capacitors: params xtal_cap_min_f / xtal_cap_max_f (farads) when the
  oscillator characteristics or the clock chapter print a recommended range for
  the external capacitors C1 / C2 (CL1 / CL2). A crystal's own load capacitance is
  not that range. On-die load capacitors are [[models.pin_capability.xtal_int_caps]].
- A pad with a software-enabled internal pull-up is listed under
  [models.pin_capability.pull_up] (basis, pads).
Record a field the datasheet does not print as an `unknown` source record naming it.
"#;

#[cfg(test)]
mod tests;
