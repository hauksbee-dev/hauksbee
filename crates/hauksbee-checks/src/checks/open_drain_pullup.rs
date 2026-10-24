//! An open-drain output with nothing to pull its line high.
//!
//! An open-drain or open-collector output (an alert, interrupt, fault or
//! power-good flag, an SPI data-out that is open-drain) can only pull its net
//! low. The high level is a resistor to a rail. The I2C and SD pull-up checks
//! cover those buses by net name; this one covers every other open-drain pad a
//! card lists under `[models.pin_capability.open_drain]`, whatever the net is
//! called.
//!
//! A net has a pull-up when
//!
//! - a resistor reaches a positive rail from it, directly or through at most
//!   two more resistors, LEDs or diodes (an LED fed from the rail through its
//!   resistor is the usual way a status output is "pulled up"), or
//! - a part on it is a microcontroller whose card lists the receiving pad under
//!   `[models.pin_capability.pull_up]`: firmware can enable its internal
//!   pull-up there. The card says the pad has one; whether firmware enables it
//!   is not on the board, so it is taken as the intended arrangement.
//!
//! It abstains where it cannot know: a connector on the net (the pull-up may
//! be off-board), and a net no other chip reads (an unused flag output).
//!
//! [`i2c_card_pullup_lint`] is the same question for an I2C bus the net names
//! and the symbol pins do not state: a card gives a pad the role `sda` or
//! `scl`. The extract layer's `MissingI2cPullup` judges the net.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use hauksbee_extract::{
    i2c_pullup_finding_for_net, i2c_role, is_capacitor, is_connector_like, is_led, is_resistor,
    is_unconnected_net, Component, ExtractedBoard, LintCheck, LintFinding, NetLintReport, Pin,
    Severity, REMEDY_MARKER,
};
use hauksbee_ir::pin_functions::{dedicated_i2c_pin, names_spi_line, PeripheralFunction};
use hauksbee_models::{ComponentKind, ModelLibrary};

use super::node_drive::Index;

/// Series parts a pull-up path may pass through.
const MAX_HOPS: usize = 3;

/// A resistor array (`RN1`, `RP2`): its pads are separate elements.
fn is_array(c: &Component) -> bool {
    let r = c.reference.to_ascii_uppercase();
    ["RN", "RP", "RM", "RA", "RZ"]
        .iter()
        .any(|p| r.starts_with(p))
        && c.pins
            .iter()
            .filter(|p| p.net.is_some_and(|n| n != 0))
            .count()
            >= 3
}

/// A part a pull-up path may run through: a resistor, an LED or a diode.
fn is_series_part(idx: &Index<'_>, c: &Component) -> bool {
    is_resistor(c)
        || is_array(c)
        || is_led(c)
        || idx.model(c).is_some_and(|m| m.kind == ComponentKind::Diode)
}

/// A jumper, net tie or link option: a part whose two sides the board may or
/// may not join, with a net other than ground on another pad.
fn is_possible_link(idx: &Index<'_>, c: &Component, at: &Pin) -> bool {
    let named_jumper = hauksbee_extract::dnp::is_jumper_or_net_tie(c)
        || matches!(
            hauksbee_models::refdes::prefix(&c.reference).as_str(),
            "JP" | "SJ"
        );
    named_jumper
        && c.pins.iter().any(|q| {
            !std::ptr::eq(q, at)
                && q.net
                    .is_some_and(|n| n != 0 && Some(n) != at.net && idx.level(n) != Some(0.0))
        })
}

/// Whether `net` is held at a positive supply level.
fn is_positive_rail(idx: &Index<'_>, net: i64) -> bool {
    if idx.level(net).is_some_and(|v| v > 0.0) || idx.feeds_a_supply_pin(net) {
        return true;
    }
    idx.members(net).iter().any(|(c, p)| {
        p.kind.trim().eq_ignore_ascii_case("power_out")
            || idx.model(c).is_some_and(|m| {
                m.kind == ComponentKind::Vreg
                    && idx
                        .role(c, p)
                        .is_some_and(|r| matches!(r, "out" | "vout" | "sw" | "lx"))
            })
    })
}

/// A pin the board's own symbol marks as an input.
fn is_input_pin(p: &Pin) -> bool {
    p.kind.trim().eq_ignore_ascii_case("input")
}

/// What the nets reachable from an open-drain net through series parts hold.
struct Reach {
    pulled: bool,
    external: bool,
    receivers: bool,
}

fn reach(idx: &Index<'_>, start: i64, od: &HashSet<(String, String)>) -> Reach {
    let mut out = Reach {
        pulled: false,
        external: false,
        receivers: false,
    };
    let mut seen = BTreeSet::from([start]);
    let mut frontier = vec![start];
    for _ in 0..MAX_HOPS {
        let mut next = Vec::new();
        for net in frontier {
            for (c, p) in idx.members(net) {
                if od.contains(&(c.reference.clone(), p.number.clone())) {
                    continue;
                }
                if is_connector_like(c) {
                    out.external = true;
                } else if is_possible_link(idx, c, p) {
                    // A solder jumper, net tie or pin jumper joins its pads only
                    // where the board closes it. Whatever sits behind it (an LED
                    // and its resistor to a rail, a pull-up, a receiver) may or
                    // may not be on this line, so the board cannot be judged.
                    out.external = true;
                } else if idx.model(c).is_some_and(|m| {
                    m.kind == ComponentKind::Mcu && m.has_internal_pull_up(&p.number)
                }) {
                    out.pulled = true;
                    out.receivers = true;
                } else if idx.model(c).is_some_and(|m| {
                    m.kind == ComponentKind::Mcu && m.pin_capability.pull_up.is_none()
                }) {
                    // An MCU whose card states no pull-up pads: firmware may
                    // well enable one, and the card cannot say, so abstain.
                    out.external = true;
                } else if is_series_part(idx, c) {
                    for q in &c.pins {
                        let Some(far) = q.net.filter(|&n| n != 0 && n != net) else {
                            continue;
                        };
                        if std::ptr::eq(q, *p) {
                            continue;
                        }
                        if is_positive_rail(idx, far) {
                            out.pulled = true;
                        } else if idx.level(far) != Some(0.0) && seen.insert(far) {
                            next.push(far);
                        }
                    }
                } else if !is_capacitor(c)
                    && (idx
                        .model(c)
                        .is_some_and(|m| m.kind != ComponentKind::Ignore)
                        || is_input_pin(p))
                {
                    // A part with a card, or a pin the board calls an input.
                    // A part nothing describes (a module, an unmodelled chip
                    // with bidirectional pins) may carry its own pull-up.
                    out.receivers = true;
                }
            }
        }
        if out.pulled {
            break;
        }
        frontier = next;
    }
    out
}

/// Open-drain outputs, from the cards, on a net with no way to go high.
#[must_use]
pub fn open_drain_pullup_lint(board: &ExtractedBoard, lib: &ModelLibrary) -> NetLintReport {
    let idx = Index::new(board, lib);
    // net -> the open-drain pads on it, as (reference, pad number, role)
    let mut by_net: BTreeMap<i64, Vec<(&Component, &Pin, String)>> = BTreeMap::new();
    for c in &board.components {
        let Some(model) = idx.model(c) else { continue };
        for p in &c.pins {
            let Some(net) = p.net.filter(|&n| n != 0) else {
                continue;
            };
            if model.is_open_drain_pad(&p.number) {
                let role = model.pins.get(&p.number).cloned().unwrap_or_default();
                // A pad the card also gives a `[[models.pin_bias]]` row is judged
                // by `pin_bias` (with the datasheet's resistor range): one finding.
                if model.pin_bias.iter().any(|b| b.role == role) {
                    continue;
                }
                by_net.entry(net).or_default().push((c, p, role));
            }
        }
    }
    let mut report = NetLintReport::default();
    for (net, pads) in by_net {
        let name = idx.name(net);
        if is_unconnected_net(name) || idx.level(net).is_some() {
            continue;
        }
        let od: HashSet<(String, String)> = pads
            .iter()
            .map(|(c, p, _)| (c.reference.clone(), p.number.clone()))
            .collect();
        let found = reach(&idx, net, &od);
        if found.pulled || found.external || !found.receivers {
            continue;
        }
        let who: Vec<String> = pads
            .iter()
            .map(|(c, p, role)| {
                if role.is_empty() {
                    format!("{} pin {}", c.reference, p.number)
                } else {
                    format!("{} pin {} ({role})", c.reference, p.number)
                }
            })
            .collect();
        let refs: BTreeSet<String> = pads.iter().map(|(c, _, _)| c.reference.clone()).collect();
        report.findings.push(LintFinding {
            check: LintCheck::MissingOpenDrainPullup,
            severity: Severity::Medium,
            message: format!(
                "net '{name}' is driven by the open-drain output {} and nothing pulls it high: no \
                 resistor to a power rail, and no receiving microcontroller pin whose card lists an \
                 internal pull-up. An open-drain output only pulls the line low, so the line floats \
                 whenever the part releases it and the receiver reads an undefined level. {REMEDY_MARKER}\
                 Add a pull-up resistor (10 kilohms is typical) from '{name}' to the rail the \
                 receiver reads, or enable an internal pull-up on the receiving pin if its \
                 datasheet gives one.",
                who.join(", "),
            ),
            refs: refs.into_iter().collect(),
            nets: vec![name.to_string()],
        });
    }
    report
}

/// The I2C line the cards state for the pads on `members`, with the pad that
/// says so (`U2 pin 3 (card role 'sda')`). `None` when no card does, or two
/// disagree.
fn card_i2c_line(
    idx: &Index<'_>,
    members: &[(&Component, &Pin)],
) -> Option<(&'static str, String)> {
    let mut found: Option<(&'static str, String)> = None;
    for (c, p) in members {
        if is_connector_like(c) {
            continue;
        }
        let Some(role) = idx.role(c, p) else { continue };
        let line = match dedicated_i2c_pin(role) {
            Some(PeripheralFunction::I2cSda) => "SDA",
            Some(PeripheralFunction::I2cScl) => "SCL",
            _ => continue,
        };
        // The card's role holds for either bus mode; the symbol may say SPI.
        if names_spi_line(&p.function) {
            continue;
        }
        match &found {
            Some((l, _)) if *l != line => return None,
            Some(_) => {}
            None => {
                found = Some((
                    line,
                    format!("{} pin {} (card role '{role}')", c.reference, p.number),
                ));
            }
        }
    }
    found
}

/// Serial display controllers with no I2C mode. Their data and clock pins are
/// often labelled `SDA` and `SCL` but are the SPI input and clock.
const SPI_ONLY_DISPLAYS: [&str; 16] = [
    "ST7735", "ST7789", "ST7796", "ST7701", "ILI9341", "ILI9340", "ILI9488", "ILI9486", "ILI9225",
    "GC9A01", "GC9307", "HX8357", "SSD1351", "SSD1331", "PCD8544", "ST7565",
];

/// The names of the nets labelled as an I2C line that are really an SPI link.
///
/// A net with a display controller that has no I2C mode (an ST7789, an
/// ILI9341) is SPI, whatever its pins are called. So is a net on a pad the card lists for
/// the SPI function the line would be (`SCL` as the clock, `SDA` as data out or
/// in) and does not list for the I2C one: that pad cannot be the hardware bus.
/// The missing-pull-up rule stays silent on these nets.
#[must_use]
pub fn spi_labelled_nets(board: &ExtractedBoard, lib: &ModelLibrary) -> BTreeSet<String> {
    let idx = Index::new(board, lib);
    let mut found = BTreeSet::new();
    for net in &board.nets {
        let members = idx.members(net.id);
        let display = members.iter().any(|(c, _)| {
            let text = format!("{} {}", c.value, c.lib_id).to_ascii_uppercase();
            SPI_ONLY_DISPLAYS.iter().any(|d| text.contains(d))
        });
        let wired_for_spi = members.iter().any(|(c, p)| {
            let Some(model) = idx.model(c) else {
                return false;
            };
            let line = i2c_role(&net.name).or_else(|| match dedicated_i2c_pin(&p.function) {
                Some(PeripheralFunction::I2cSda) => Some("SDA"),
                Some(PeripheralFunction::I2cScl) => Some("SCL"),
                _ => None,
            });
            let (spi, i2c): (&[PeripheralFunction], PeripheralFunction) = match line {
                Some("SCL") => (&[PeripheralFunction::SpiSck], PeripheralFunction::I2cScl),
                Some(_) => (
                    &[PeripheralFunction::SpiMosi, PeripheralFunction::SpiMiso],
                    PeripheralFunction::I2cSda,
                ),
                None => return false,
            };
            let listed = |f: PeripheralFunction| {
                model
                    .pin_capability
                    .table(f)
                    .map(|t| t.pads.contains_key(&p.number))
            };
            listed(i2c) == Some(false) && spi.iter().any(|f| listed(*f) == Some(true))
        });
        if display || wired_for_spi {
            found.insert(net.name.clone());
        }
    }
    found
}

/// I2C buses with no pull-up that only the cards identify.
///
/// A net whose name and symbol pins state no I2C line, but on which a card
/// gives a pad the role `sda` or `scl`, is judged by the extract layer's
/// missing-pull-up rule: at least two parts, no resistor, resistor network
/// element, translator with its own pull-ups or current-source transistor to a
/// rail, and no connector (abstain). A net the extract layer named by itself is
/// left to it. A part counts as read when its pin has a symbol name or its
/// pad has a card.
#[must_use]
pub fn i2c_card_pullup_lint(board: &ExtractedBoard, lib: &ModelLibrary) -> NetLintReport {
    let idx = Index::new(board, lib);
    let mut report = NetLintReport::default();
    for net in &board.nets {
        if net.id == 0 || is_unconnected_net(&net.name) || i2c_role(&net.name).is_some() {
            continue;
        }
        let members = idx.members(net.id);
        if members.len() < 2 {
            continue;
        }
        if members
            .iter()
            .any(|(_, p)| dedicated_i2c_pin(&p.function).is_some())
        {
            // The symbol pins state it: the extract layer has already judged.
            continue;
        }
        let Some((line, evidence)) = card_i2c_line(&idx, members) else {
            continue;
        };
        let readable =
            |c: &Component, p: &Pin| !p.function.trim().is_empty() || idx.model(c).is_some();
        report.findings.extend(i2c_pullup_finding_for_net(
            board, net.id, line, &evidence, &readable,
        ));
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checks::test_support::{board, part};

    const GND: i64 = 1;
    const V33: i64 = 2;
    const ALERT: i64 = 10;
    const MID: i64 = 11;

    /// An INA228 with ALERT (pad 3 here per the card) on `alert`.
    fn sensor(alert: i64) -> Component {
        part(
            "U1",
            "INA228AIDGSR",
            &[("3", alert), ("6", V33), ("7", GND)],
        )
    }

    fn resistor(reference: &str, a: i64, b: i64) -> Component {
        let mut c = part(reference, "10k", &[("1", a), ("2", b)]);
        c.lib_id = "Device:R".into();
        c
    }

    fn run(parts: Vec<Component>) -> Vec<LintFinding> {
        let nets = [
            (GND, "GND"),
            (V33, "+3V3"),
            (ALERT, "ALERT_N"),
            (MID, "MID"),
        ];
        open_drain_pullup_lint(&board(&nets, parts), &ModelLibrary::builtin()).findings
    }

    /// An `ATmega` whose pin 2 reads `net`.
    fn mcu(net: i64) -> Component {
        part("U2", "ATmega328P", &[("2", net), ("7", V33), ("8", GND)])
    }

    /// A receiver the library knows nothing about, on a pin the board calls an
    /// input: no stated pull.
    fn receiver(net: i64) -> Component {
        let mut c = part("U3", "XYZ-LOGIC-9", &[("1", net)]);
        c.pins[0].kind = "input".into();
        c
    }

    /// A module nothing describes, on a bidirectional pin: it may carry its own pull-up.
    fn module(net: i64) -> Component {
        let mut c = part("U3", "XYZ-MODULE-9", &[("1", net)]);
        c.pins[0].kind = "bidirectional".into();
        c
    }

    #[test]
    fn a_receiving_mcu_pin_with_a_stated_internal_pull_up_is_silent() {
        let found = run(vec![sensor(ALERT), mcu(ALERT)]);
        assert!(found.is_empty(), "{found:#?}");
    }

    #[test]
    fn an_open_drain_flag_with_no_pull_up_is_flagged() {
        let found = run(vec![sensor(ALERT), receiver(ALERT)]);
        assert_eq!(found.len(), 1, "{found:#?}");
        assert_eq!(found[0].check, LintCheck::MissingOpenDrainPullup);
        assert_eq!(found[0].refs, ["U1"]);
        assert_eq!(found[0].nets, ["ALERT_N"]);
    }

    /// A rail whose name states no voltage (`V5`) is still the rail when a chip's
    /// supply pin is on it.
    #[test]
    fn a_pull_up_to_a_rail_named_without_a_voltage_counts_when_a_chip_runs_from_it() {
        let nets = [(GND, "GND"), (V33, "V5"), (ALERT, "ALERT_N")];
        let b = board(
            &nets,
            vec![sensor(ALERT), receiver(ALERT), resistor("R1", ALERT, V33)],
        );
        let found = open_drain_pullup_lint(&b, &ModelLibrary::builtin()).findings;
        assert!(found.is_empty(), "{found:#?}");
        let b = board(&nets, vec![sensor(ALERT), receiver(ALERT)]);
        let found = open_drain_pullup_lint(&b, &ModelLibrary::builtin()).findings;
        assert_eq!(found.len(), 1, "{found:#?}");
    }

    #[test]
    fn an_undescribed_module_on_the_net_is_not_judged() {
        let found = run(vec![sensor(ALERT), module(ALERT)]);
        assert!(found.is_empty(), "{found:#?}");
    }

    #[test]
    fn a_pull_up_resistor_or_a_status_led_to_the_rail_is_silent() {
        let direct = run(vec![
            sensor(ALERT),
            receiver(ALERT),
            resistor("R1", ALERT, V33),
        ]);
        assert!(direct.is_empty(), "{direct:#?}");
        // The line pulled through a series resistor still reaches the rail.
        let series = run(vec![
            sensor(ALERT),
            receiver(ALERT),
            resistor("R1", ALERT, MID),
            resistor("R2", MID, V33),
        ]);
        assert!(series.is_empty(), "{series:#?}");
        // A pull-down alone is not a pull-up.
        let down = run(vec![
            sensor(ALERT),
            receiver(ALERT),
            resistor("R1", ALERT, GND),
        ]);
        assert_eq!(down.len(), 1, "{down:#?}");
    }

    /// A three-pad solder jumper (pad 1 on `a`, pad 2 on `b`, pad 3 on `c`).
    fn jumper3(reference: &str, a: i64, b: i64, c: i64) -> Component {
        let mut j = part(reference, "SolderJumper", &[("1", a), ("2", b), ("3", c)]);
        j.lib_id = "Jumper:SolderJumper_3_Open".into();
        j.footprint = "Jumper:SolderJumper-3_P1.3mm_Open_RoundedPad1.0x1.5mm".into();
        j
    }

    /// A status output that reaches only one pad of a solder jumper, with an LED
    /// and its resistor to the rail behind the jumper, is not judged: the jumper
    /// is a possible connection, not a receiver, and the pull-up may sit behind it.
    #[test]
    fn a_pull_up_behind_a_solder_jumper_is_not_reported_missing() {
        // Rail -> LED anode, LED cathode (MID) -> R1 -> LED_K -> jumper pad 2.
        let led_k = 12;
        let mut led = part("LED1", "LED", &[("1", MID), ("2", V33)]);
        led.lib_id = "Device:LED".into();
        let behind = vec![
            sensor(ALERT),
            receiver(ALERT),
            jumper3("SJ1", ALERT, led_k, GND),
            resistor("R1", MID, led_k),
            led,
        ];
        let nets = [
            (GND, "GND"),
            (V33, "+3V3"),
            (ALERT, "STAT"),
            (MID, "MID"),
            (led_k, "LED_K"),
        ];
        let found =
            open_drain_pullup_lint(&board(&nets, behind), &ModelLibrary::builtin()).findings;
        assert!(found.is_empty(), "{found:#?}");
        // Control: the jumper's other pads reach only ground, so nothing can pull
        // the line up, and the reader is a real receiver.
        let bare = vec![
            sensor(ALERT),
            receiver(ALERT),
            jumper3("SJ1", ALERT, GND, GND),
        ];
        let found = open_drain_pullup_lint(&board(&nets, bare), &ModelLibrary::builtin()).findings;
        assert_eq!(found.len(), 1, "{found:#?}");
    }

    /// A part whose model stands for "ignore" (artwork, a logo) is not a receiver.
    #[test]
    fn an_ignored_part_is_not_a_receiver() {
        let mut logo = part("G1", "Logo", &[("1", ALERT)]);
        logo.footprint = "Symbol:OSHW-Logo".into();
        let found = run(vec![sensor(ALERT), logo]);
        assert!(found.is_empty(), "{found:#?}");
    }

    #[test]
    fn nets_nothing_reads_or_a_connector_can_pull_are_not_judged() {
        let unused = run(vec![sensor(ALERT)]);
        assert!(unused.is_empty(), "{unused:#?}");
        let mut header = part("J1", "Conn_01x01", &[("1", ALERT)]);
        header.lib_id = "Connector:Conn_01x01".into();
        let off_board = run(vec![sensor(ALERT), header]);
        assert!(off_board.is_empty(), "{off_board:#?}");
    }

    /// The interrupt, alert, ready and fault outputs the datasheets call
    /// open-drain are marked on their cards: read by a receiver with no pull-up
    /// each is flagged, and the same net pulled up to the rail is silent.
    /// Each row is a part value and the pad of an open-drain output on its card.
    #[test]
    fn the_open_drain_outputs_the_datasheets_name_need_a_pull_up() {
        const OUTPUTS: [(&str, &str); 15] = [
            ("INA226AIDGSR", "3"),
            ("TMP117AIDRVR", "3"),
            ("ADS1115IDGSR", "2"),
            ("MAX17048G+T10", "5"),
            ("PCF8563T", "3"),
            ("DS3231SN", "3"),
            ("TSL25911FN", "2"),
            ("VL53L0X", "7"),
            ("PCF8574DWR", "13"),
            ("DRV8833PW", "8"),
            ("LM393", "1"),
            ("MCP73833-AMI/UN", "3"),
            ("MP2650", "9"),
            ("MAX17760", "5"),
            ("SN74LVC1G07DBVR", "4"),
        ];
        // A part of a `variant_of` family resolves only when every pad its card maps is
        // on the footprint, so each part carries all of its card's pads: the open-drain
        // one on the alert net and the rest on nets of their own.
        let lib = ModelLibrary::builtin();
        let out = |value: &str, pad: &str| {
            // A few cards also name the package they describe.
            let footprint = match value {
                "MAX17048G+T10" => "Package_DFN_QFN:TDFN-8-1EP_3x3mm",
                "PCF8574DWR" => "Package_SO:SOIC-16W_7.5x10.3mm_P1.27mm",
                _ => "",
            };
            let mut bare = part("U1", value, &[]);
            bare.footprint = footprint.into();
            let fitted = hauksbee_extract::assembly::AssemblyState::of(&bare).fitted();
            let card = fitted.and_then(|f| hauksbee_bind::binder::resolve(&lib, f).model);
            let pads: Vec<String> = card.map_or_else(
                || vec![pad.to_string()],
                |m| m.pins.keys().cloned().collect(),
            );
            let wired: Vec<(&str, i64)> = pads
                .iter()
                .zip(100..)
                .map(|(n, net)| (n.as_str(), if n == pad { ALERT } else { net }))
                .collect();
            let mut placed = part("U1", value, &wired);
            placed.footprint = footprint.into();
            placed
        };
        let mut failures = Vec::new();
        for (value, pad) in OUTPUTS {
            let flagged = run(vec![out(value, pad), receiver(ALERT)]);
            if flagged.len() != 1 || flagged[0].check != LintCheck::MissingOpenDrainPullup {
                failures.push(format!("{value} pad {pad} bare: {flagged:?}"));
            }
            let pulled = run(vec![
                out(value, pad),
                receiver(ALERT),
                resistor("R1", ALERT, V33),
            ]);
            if !pulled.is_empty() {
                failures.push(format!("{value} pad {pad} pulled up: {pulled:?}"));
            }
        }
        assert!(failures.is_empty(), "{failures:#?}");
    }
}

/// An I2C bus the nets do not name and the symbols do not state, found by the
/// cards' pad roles.
#[cfg(test)]
mod i2c_card_tests {
    use super::*;
    use crate::checks::test_support::{board, part};
    use crate::test_support::pin_fn;

    const V33: i64 = 2;
    const DATA: i64 = 20;
    const CLOCK: i64 = 21;

    /// An INA226: its card gives pad 4 the role `sda` and pad 5 `scl`.
    /// `names` are the symbol's pin names for them.
    fn monitor(names: (&str, &str)) -> Component {
        let mut c = part("U1", "INA226AIDGSR", &[]);
        c.pins = vec![pin_fn("4", DATA, names.0), pin_fn("5", CLOCK, names.1)];
        c
    }

    /// A host whose pins the symbol names but no card describes.
    fn host(names: (&str, &str)) -> Component {
        let mut c = part("U2", "XYZ-HOST-9", &[]);
        c.pins = vec![pin_fn("1", DATA, names.0), pin_fn("2", CLOCK, names.1)];
        c
    }

    fn pull(reference: &str, net: i64) -> Component {
        let mut c = part(reference, "4.7k", &[("1", V33), ("2", net)]);
        c.lib_id = "Device:R".into();
        c
    }

    fn run(parts: Vec<Component>) -> Vec<LintFinding> {
        let nets = [(V33, "+3V3"), (DATA, "SENS_D"), (CLOCK, "SENS_C")];
        i2c_card_pullup_lint(&board(&nets, parts), &ModelLibrary::builtin()).findings
    }

    #[test]
    fn a_card_role_makes_an_unnamed_net_an_i2c_bus_with_no_pull_up() {
        // The symbol names the pads `D` and `C`: only the card says they are I2C.
        let found = run(vec![monitor(("D", "C")), host(("GPIO1", "GPIO2"))]);
        assert_eq!(found.len(), 2, "{found:#?}");
        for f in &found {
            assert_eq!(f.check, LintCheck::MissingI2cPullup);
            assert_eq!(f.severity, Severity::High);
            assert!(f.message.contains("card role"), "{}", f.message);
        }
        assert!(found
            .iter()
            .any(|f| f.nets == ["SENS_D"] && f.message.contains("I2C SDA")));
        assert!(found
            .iter()
            .any(|f| f.nets == ["SENS_C"] && f.message.contains("I2C SCL")));
    }

    #[test]
    fn a_pull_up_a_connector_or_an_unreadable_part_keeps_it_silent() {
        let bare = || vec![monitor(("D", "C")), host(("GPIO1", "GPIO2"))];
        let mut pulled = bare();
        pulled.extend([pull("R1", DATA), pull("R2", CLOCK)]);
        assert!(run(pulled).is_empty());
        let mut with_header = bare();
        let mut header = part("J1", "Conn_01x02", &[]);
        header.lib_id = "Connector:Conn_01x02".into();
        header.pins = vec![pin_fn("1", DATA, ""), pin_fn("2", CLOCK, "")];
        with_header.push(header);
        assert!(run(with_header).is_empty());
        // A part with no card and no pin names cannot be read.
        assert!(run(vec![monitor(("D", "C")), host(("", ""))]).is_empty());
    }

    #[test]
    fn a_symbol_that_says_spi_or_states_the_bus_is_not_judged_here() {
        // The same card pads wired as SPI data in / clock: the symbol says so.
        assert!(run(vec![monitor(("SDI", "SPC")), host(("GPIO1", "GPIO2"))]).is_empty());
        // The symbol states I2C itself: the extract layer judges that net.
        assert!(run(vec![monitor(("SDA", "SCL")), host(("GPIO1", "GPIO2"))]).is_empty());
    }

    /// An `ATmega328P` (pads 17 and 19 are its SPI data out and clock, 27 and 28
    /// its I2C data and clock) wired by `pads` to a peer whose pins the symbol
    /// calls `SDA` and `SCL`, on nets called for the bus. The whole engine lint
    /// runs, so the name check and the filter are both in play.
    fn labelled_link(pads: (&str, &str), peer_value: &str) -> Vec<LintFinding> {
        let nets = [(V33, "+3V3"), (DATA, "LCD_SDA"), (CLOCK, "LCD_SCL")];
        let mut mcu = part("U1", "ATmega328P", &[]);
        mcu.pins = vec![pin_fn(pads.0, DATA, "PB3"), pin_fn(pads.1, CLOCK, "PB5")];
        let mut peer = part("U2", peer_value, &[]);
        peer.pins = vec![pin_fn("1", DATA, "SDA"), pin_fn("2", CLOCK, "SCL")];
        crate::checks::engine_lint(&board(&nets, vec![mcu, peer]), &ModelLibrary::builtin())
            .findings
            .into_iter()
            .filter(|f| f.check == LintCheck::MissingI2cPullup)
            .collect()
    }

    #[test]
    fn a_labelled_i2c_link_between_i2c_capable_parts_still_needs_a_pull_up() {
        // The control for the two cases below: same labels, the MCU pads are
        // its hardware I2C pads and the peer is not a display.
        let found = labelled_link(("27", "28"), "XYZ-HOST-9");
        assert_eq!(found.len(), 2, "{found:#?}");
    }

    #[test]
    fn a_net_labelled_sda_scl_on_the_mcus_spi_pads_is_not_an_i2c_bus() {
        // Pads 17 and 19 carry MOSI and SCK in hardware and no I2C function:
        // a display module's "SDA" and "SCL" inputs wired to them are SPI.
        let found = labelled_link(("17", "19"), "XYZ-HOST-9");
        assert!(found.is_empty(), "{found:#?}");
    }

    #[test]
    fn a_net_labelled_sda_scl_to_an_spi_display_controller_is_not_an_i2c_bus() {
        // The MCU is on its I2C pads here; the peer is a controller with no
        // I2C mode, so the labels are the display's SPI input and clock.
        for value in ["ST7789", "ILI9341", "GC9A01A"] {
            let found = labelled_link(("27", "28"), value);
            assert!(found.is_empty(), "{value}: {found:#?}");
        }
    }

    #[test]
    fn a_net_named_for_the_bus_is_left_to_the_name_check() {
        let nets = [(V33, "+3V3"), (DATA, "I2C_SDA"), (CLOCK, "I2C_SCL")];
        let b = board(&nets, vec![monitor(("D", "C")), host(("GPIO1", "GPIO2"))]);
        assert!(i2c_card_pullup_lint(&b, &ModelLibrary::builtin())
            .findings
            .is_empty());
    }
}
