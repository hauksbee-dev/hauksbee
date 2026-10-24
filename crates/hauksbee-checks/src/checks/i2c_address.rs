//! Two I2C targets on one bus that answer at the same 7-bit address.
//! Long-form how-and-why: docs/how-and-why/hauksbee-engine/checks.md.
//!
//! Both parts acknowledge the same address byte, so a read returns the wired-AND
//! of two replies and a write reaches both. Nothing about the supply, the
//! pull-ups or the levels looks wrong. The cards state each part's address
//! (`[models.i2c]`: the fixed or default address, the address-select pins and the
//! address each strap combination gives, whether firmware can move the address
//! and which pin that needs), and the board's wiring says which parts share a
//! bus and where their select pins are tied.
//!
//! Judged only on definite evidence:
//!
//! - **One bus.** Two parts share a bus when their SDA pads reach one net and
//!   their SCL pads reach one net, the nets taken through series parts of at
//!   most 1 kohm (a protection resistor, a ferrite). Nets behind an I2C mux
//!   channel, a switch, a buffer or a level translator are different nets and are
//!   never compared: the check abstains across any such boundary rather than
//!   guess which channel is open. A bus carried to a second board by a connector
//!   is not followed either.
//! - **Straps resolved.** An address-select pin is read as ground, a positive
//!   rail (by name, or the part's own supply pin), the bus's own SDA or SCL
//!   line, a single pull resistor to one of those, or unconnected. A pin on a net
//!   something else can drive, pulled both ways, or on a part the card does not
//!   map says nothing: the part's address is unknown and it is never reported as
//!   clashing, except in one Low abstention when one of the addresses it could
//!   take is already answered by another part on the bus.
//! - **Not movable by firmware.** A part whose card says its address can be
//!   changed in software (the VL53L0X and VL6180X `XSHUT` sequence) and whose
//!   required pin the board can drive is a Low note ("must be re-addressed at
//!   boot; firmware not read"). The same part with that pin tied to a rail or
//!   left open can never be sequenced, so the clash is High. A clash with at
//!   most one such fixed part is the note; two fixed parts are the finding.
//!
//! DNP parts take no part.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use hauksbee_extract::assembly::AssemblyState;
use hauksbee_extract::{
    is_capacitor, is_test_point_or_hole, Component, ExtractedBoard, LintCheck, LintFinding,
    NetLintReport, Severity, ABSTENTION_MARKER, REMEDY_MARKER,
};
use hauksbee_models::i2c_address::{I2cAddress, I2cStrap};
use hauksbee_models::netclass::is_positive_supply;
use hauksbee_models::{ModelEntry, ModelLibrary};

use super::envelope::role_net_id;
use super::straps::is_assembled_resistor;
use super::{is_ground_name, is_unconnected_net};

/// A part with an `[models.i2c]` block fitted on a bus.
struct Target<'a> {
    comp: &'a Component,
    model: ModelEntry,
    spec: I2cAddress,
    sda: i64,
    scl: i64,
}

/// What a strap role is tied to, or why that is unknown.
type Strap = Result<I2cStrap, String>;

/// The addresses a part answers at, or the strap that leaves them unknown.
enum Resolved {
    At(Vec<u8>),
    /// Every address the part could take, and why it is not one of them yet.
    Unknown {
        could_be: BTreeSet<u8>,
        why: String,
    },
}

/// Whether firmware can move a part off the contested address.
enum Mobility {
    /// Nothing the board does moves it, and why.
    Fixed(String),
    /// Firmware can sequence it; the check does not read firmware.
    Movable(String),
}

/// A disjoint-set over net ids, joined through series parts.
#[derive(Default)]
struct Nets {
    parent: HashMap<i64, i64>,
}

impl Nets {
    fn root(&mut self, net: i64) -> i64 {
        let mut at = net;
        while let Some(&up) = self.parent.get(&at) {
            if up == at {
                break;
            }
            at = up;
        }
        self.parent.insert(net, at);
        at
    }

    fn join(&mut self, a: i64, b: i64) {
        let (ra, rb) = (self.root(a), self.root(b));
        if ra != rb {
            self.parent.insert(ra.max(rb), ra.min(rb));
        }
    }
}

fn is_rail(board: &ExtractedBoard, id: i64) -> bool {
    board
        .net(id)
        .is_some_and(|n| is_ground_name(&n.name) || is_positive_supply(&n.name))
}

/// The nets of `board` joined into buses through series resistors and ferrites.
fn bus_nets(board: &ExtractedBoard) -> Nets {
    let mut nets = Nets::default();
    for comp in &board.components {
        if AssemblyState::of(comp).fitted().is_none() || !super::bus_swap::is_series_part(comp) {
            continue;
        }
        if let (Some(a), Some(b)) = (comp.pins[0].net, comp.pins[1].net) {
            if a != 0 && b != 0 && !is_rail(board, a) && !is_rail(board, b) {
                nets.join(a, b);
            }
        }
    }
    nets
}

/// The role of the pad of `comp` named `role`, and the pin's net (`None` when
/// the pad is present but on no net).
fn strap_pad<'c>(
    comp: &'c Component,
    model: &ModelEntry,
    role: &str,
) -> Option<&'c hauksbee_extract::Pin> {
    let (pad, _) = model
        .pins
        .iter()
        .find(|(_, r)| r.eq_ignore_ascii_case(role))?;
    comp.pins.iter().find(|p| p.number == *pad)
}

/// What the other pins of `comp` on a net say about its level, by their roles.
fn own_pin_level(comp: &Component, model: &ModelEntry, net: i64, pad: &str) -> Option<bool> {
    for p in comp
        .pins
        .iter()
        .filter(|p| p.net == Some(net) && p.number != pad)
    {
        let Some(role) = model.pins.get(&p.number).map(|r| r.to_ascii_lowercase()) else {
            continue;
        };
        if role.starts_with("gnd") || role.starts_with("vss") {
            return Some(false);
        }
        if role.starts_with("vdd") || role.starts_with("vcc") || role == "vio" {
            return Some(true);
        }
    }
    None
}

/// The level the net `net` holds the strap pad `pad` of `comp` at.
fn net_strap(
    board: &ExtractedBoard,
    (comp, model): (&Component, &ModelEntry),
    pad: &str,
    net: i64,
) -> Strap {
    let Some(info) = board.net(net) else {
        return Ok(I2cStrap::Open);
    };
    if is_ground_name(&info.name) {
        return Ok(I2cStrap::Low);
    }
    if is_positive_supply(&info.name) {
        return Ok(I2cStrap::High);
    }
    if let Some(high) = own_pin_level(comp, model, net, pad) {
        return Ok(if high { I2cStrap::High } else { I2cStrap::Low });
    }
    let (mut low, mut high) = (false, false);
    for (other, pin) in board.net_members(net) {
        if other.reference == comp.reference && pin.number == pad {
            continue;
        }
        if AssemblyState::of(other).fitted().is_none()
            || is_test_point_or_hole(other)
            || is_capacitor(other)
        {
            continue;
        }
        if !is_assembled_resistor(other) {
            return Err(format!(
                "it also reaches {} ({})",
                other.reference,
                other.value.trim()
            ));
        }
        for far in other
            .pins
            .iter()
            .filter_map(|p| p.net)
            .filter(|id| *id != net)
        {
            let name = board.net(far).map_or("", |n| n.name.as_str());
            if is_ground_name(name) {
                low = true;
            } else if is_positive_supply(name) {
                high = true;
            } else if !is_unconnected_net(name) && far != 0 {
                return Err(format!("it reaches {} through {}", name, other.reference));
            }
        }
    }
    match (low, high) {
        (true, false) => Ok(I2cStrap::Low),
        (false, true) => Ok(I2cStrap::High),
        (true, true) => Err("it is pulled both high and low".into()),
        (false, false) => Ok(I2cStrap::Open),
    }
}

/// What `role`'s pad on `target` is tied to.
fn strap_of(board: &ExtractedBoard, nets: &mut Nets, t: &Target<'_>, role: &str) -> Strap {
    let Some(pin) = strap_pad(t.comp, &t.model, role) else {
        return Err(format!("the card's '{role}' pad is not on this footprint"));
    };
    let Some(net) = pin.net.filter(|n| *n != 0) else {
        return Ok(I2cStrap::Open);
    };
    if board.net(net).is_some_and(|n| is_unconnected_net(&n.name)) {
        return Ok(I2cStrap::Open);
    }
    let root = nets.root(net);
    if root == nets.root(t.sda) {
        return Ok(I2cStrap::Sda);
    }
    if root == nets.root(t.scl) {
        return Ok(I2cStrap::Scl);
    }
    net_strap(board, (t.comp, &t.model), &pin.number, net)
}

fn net_name(board: &ExtractedBoard, id: i64) -> String {
    board.net(id).map_or_else(|| "?".into(), |n| n.name.clone())
}

fn resolve(board: &ExtractedBoard, nets: &mut Nets, t: &Target<'_>) -> Resolved {
    let mut levels = BTreeMap::new();
    for role in t.spec.strap_roles() {
        match strap_of(board, nets, t, role) {
            Ok(level) => {
                levels.insert(role.to_owned(), level);
            }
            Err(why) => {
                let could_be = t.spec.select.iter().map(|r| r.address).collect();
                return Resolved::Unknown {
                    could_be,
                    why: format!("its {role} pin is not fixed by the board: {why}"),
                };
            }
        }
    }
    t.spec.addresses_for(&levels).map_or_else(
        || Resolved::Unknown {
            could_be: t.spec.select.iter().map(|r| r.address).collect(),
            why: format!("the straps {levels:?} match no row of its card's address table"),
        },
        Resolved::At,
    )
}

/// Whether `t` can be moved off the contested address by firmware.
fn mobility(board: &ExtractedBoard, nets: &mut Nets, t: &Target<'_>) -> Mobility {
    let Some(re) = &t.spec.reassignable else {
        return Mobility::Fixed("its address is fixed".into());
    };
    let Some(pin) = &re.requires_pin else {
        return Mobility::Movable("its address is set by a register write".into());
    };
    match strap_of(board, nets, t, pin) {
        Ok(I2cStrap::Low | I2cStrap::High) => {
            let net = strap_pad(t.comp, &t.model, pin)
                .and_then(|p| p.net)
                .map_or_else(String::new, |n| net_name(board, n));
            Mobility::Fixed(format!(
                "it can be re-addressed only while {pin} is controlled, and {pin} is tied to {net}"
            ))
        }
        Ok(I2cStrap::Open) => Mobility::Fixed(format!(
            "it can be re-addressed only while {pin} is controlled, and {pin} is left open"
        )),
        _ => Mobility::Movable(format!(
            "it must be re-addressed at boot with {pin} sequenced; firmware not read"
        )),
    }
}

fn label(t: &Target<'_>) -> String {
    format!("{} ({})", t.comp.reference, t.comp.value.trim())
}

/// The clash findings for the targets that share one bus.
fn judge_bus(
    board: &ExtractedBoard,
    nets: &mut Nets,
    bus: &[&Target<'_>],
    report: &mut NetLintReport,
) {
    let resolved: Vec<Resolved> = bus.iter().map(|t| resolve(board, nets, t)).collect();
    let mut by_address: BTreeMap<u8, Vec<usize>> = BTreeMap::new();
    for (i, r) in resolved.iter().enumerate() {
        if let Resolved::At(at) = r {
            for a in at {
                by_address.entry(*a).or_default().push(i);
            }
        }
    }
    let bus_name = format!(
        "{}/{}",
        net_name(board, bus[0].sda),
        net_name(board, bus[0].scl)
    );
    let nets_named = vec![net_name(board, bus[0].sda), net_name(board, bus[0].scl)];
    for (address, idx) in &by_address {
        let idx: Vec<usize> = idx
            .iter()
            .copied()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        if idx.len() < 2 {
            continue;
        }
        let moves: Vec<(usize, Mobility)> = idx
            .iter()
            .map(|i| (*i, mobility(board, nets, bus[*i])))
            .collect();
        let fixed = moves
            .iter()
            .filter(|(_, m)| matches!(m, Mobility::Fixed(_)))
            .count();
        let parts: Vec<String> = idx.iter().map(|i| label(bus[*i])).collect();
        let mut why = Vec::new();
        for (i, m) in &moves {
            let (Mobility::Fixed(s) | Mobility::Movable(s)) = m;
            if bus[*i].spec.reassignable.is_some() {
                why.push(format!("{}: {s}", bus[*i].comp.reference));
            }
        }
        let cards: Vec<String> = idx
            .iter()
            .map(|i| {
                format!(
                    "{} card '{}': {}",
                    bus[*i].comp.reference,
                    bus[*i].model.id,
                    bus[*i].spec.basis.trim()
                )
            })
            .collect();
        let head = format!(
            "{} all answer at I2C address 0x{address:02x} on bus {bus_name}",
            parts.join(", ")
        );
        let detail = if why.is_empty() {
            String::new()
        } else {
            format!(" ({})", why.join("; "))
        };
        let (severity, tail) = if fixed >= 2 {
            (
                Severity::High,
                format!(
                    "The parts acknowledge the same address byte, so every read returns the two replies ANDed \
                     and every write reaches both. {REMEDY_MARKER}Move one part to another address \
                     with its address-select pin, put it on its own bus or behind an I2C mux \
                     channel, or use a part with a different fixed address."
                ),
            )
        } else {
            (
                Severity::Low,
                format!(
                    "The clash is avoidable only if the firmware re-addresses the movable part \
                     at boot before the other answers; the firmware was not read. \
                     {REMEDY_MARKER}Confirm the boot sequence moves it, or give the parts different addresses."
                ),
            )
        };
        report.findings.push(LintFinding {
            check: LintCheck::I2cAddressClash,
            severity,
            message: format!("{head}{detail}. {}. {tail}", cards.join("; ")),
            refs: idx.iter().map(|i| bus[*i].comp.reference.clone()).collect(),
            nets: nets_named.clone(),
        });
    }
    unresolved_notes(bus, &resolved, &bus_name, &nets_named, &by_address, report);
}

/// A Low abstention for a part whose address depends on a strap the board does
/// not fix, when an address it could take is already answered on the bus.
fn unresolved_notes(
    bus: &[&Target<'_>],
    resolved: &[Resolved],
    bus_name: &str,
    nets: &[String],
    by_address: &BTreeMap<u8, Vec<usize>>,
    report: &mut NetLintReport,
) {
    for (i, r) in resolved.iter().enumerate() {
        let Resolved::Unknown { could_be, why } = r else {
            continue;
        };
        let clash: Vec<(u8, &usize)> = could_be
            .iter()
            .filter_map(|a| by_address.get(a).and_then(|v| v.first()).map(|j| (*a, j)))
            .filter(|(_, j)| **j != i)
            .collect();
        if clash.is_empty() {
            continue;
        }
        let mut refs = vec![bus[i].comp.reference.clone()];
        let said: Vec<String> = clash
            .iter()
            .map(|(a, j)| format!("0x{a:02x} ({})", label(bus[**j])))
            .collect();
        refs.extend(clash.iter().map(|(_, j)| bus[**j].comp.reference.clone()));
        refs.sort();
        refs.dedup();
        report.findings.push(LintFinding {
            check: LintCheck::I2cAddressClash,
            severity: Severity::Low,
            message: format!(
                "The I2C address check for {} on bus {bus_name}{ABSTENTION_MARKER}: {why}. \
                 It could take an address another part on the bus already answers at: {}. \
                 {REMEDY_MARKER}Tie the address-select pin to ground, the supply or a bus line so \
                 the address is fixed, or confirm it differs from the others.",
                label(bus[i]),
                said.join(", ")
            ),
            refs,
            nets: nets.to_vec(),
        });
    }
}

/// Two parts on one I2C bus that answer at the same address.
#[must_use]
pub fn i2c_address_lint(board: &ExtractedBoard, lib: &ModelLibrary) -> NetLintReport {
    let mut report = NetLintReport::default();
    let mut seen = BTreeSet::new();
    let mut targets = Vec::new();
    for (comp, model) in super::resolved_components(board, lib) {
        let Some(spec) = model.i2c.clone() else {
            continue;
        };
        let (Some(sda), Some(scl)) = (
            role_net_id(comp, &model, &spec.sda_role),
            role_net_id(comp, &model, &spec.scl_role),
        ) else {
            continue;
        };
        if sda != scl && seen.insert(comp.reference.clone()) {
            targets.push(Target {
                comp,
                model,
                spec,
                sda,
                scl,
            });
        }
    }
    if targets.len() < 2 {
        return report;
    }
    let mut nets = bus_nets(board);
    let mut buses: BTreeMap<(i64, i64), Vec<&Target<'_>>> = BTreeMap::new();
    for t in &targets {
        let key = (nets.root(t.sda), nets.root(t.scl));
        if key.0 != key.1 {
            buses.entry(key).or_default().push(t);
        }
    }
    for bus in buses.values().filter(|b| b.len() > 1) {
        judge_bus(board, &mut nets, bus, &mut report);
    }
    report
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::OnceLock;

    use hauksbee_extract::Pin;

    use super::*;
    use crate::test_support::{board, pin, two_pin_resistor};

    const GND: i64 = 1;
    const V33: i64 = 2;
    const SDA: i64 = 3;
    const SCL: i64 = 4;
    const SDA2: i64 = 5;
    const SCL2: i64 = 6;
    const GPIO_A: i64 = 7;
    const XSHUT_A: i64 = 8;
    const XSHUT_B: i64 = 9;
    const CH0_SDA: i64 = 10;
    const CH0_SCL: i64 = 11;
    const CH1_SDA: i64 = 12;
    const CH1_SCL: i64 = 13;
    const SDA_FAR: i64 = 14;
    const A0_U2: i64 = 15;

    const NETS: &[(i64, &str)] = &[
        (GND, "GND"),
        (V33, "+3V3"),
        (SDA, "/SDA"),
        (SCL, "/SCL"),
        (SDA2, "/SDA2"),
        (SCL2, "/SCL2"),
        (GPIO_A, "/ADDR_GPIO"),
        (XSHUT_A, "/XSHUT_A"),
        (XSHUT_B, "/XSHUT_B"),
        (CH0_SDA, "/CH0_SDA"),
        (CH0_SCL, "/CH0_SCL"),
        (CH1_SDA, "/CH1_SDA"),
        (CH1_SCL, "/CH1_SCL"),
        (SDA_FAR, "/SDA_FAR"),
        (A0_U2, "/A0_U2"),
    ];

    /// Fixture cards: a fixed 0x29 part, a one-strap 0x29/0x2A part, a part
    /// firmware re-addresses with its `xshut` pin held, and a fixed 0x50 part.
    const CARDS: &str = r#"
[[models]]
id = "fix_fixed"
kind = "digital"
description = "fixed address fixture"
[models.match]
value_re = "^SENSA$"
[models.params]
identity_only = true
warning = "identity only"
unlocked_by = "validated behavior"
[models.i2c]
address = 0x29
basis = "fixture p.1 fixed address 0x29"
[models.pins]
"1" = "sda"
"2" = "scl"
"3" = "gnd"
"4" = "vdd"

[[models]]
id = "fix_strapped"
kind = "digital"
description = "strapped address fixture"
[models.match]
value_re = "^SENSB$"
[models.params]
identity_only = true
warning = "identity only"
unlocked_by = "validated behavior"
[models.i2c]
address = 0x29
basis = "fixture p.2 table: addr low 0x29, high 0x2A"
[[models.i2c.select]]
address = 0x29
straps = { addr = "low" }
[[models.i2c.select]]
address = 0x2A
straps = { addr = "high" }
[models.pins]
"1" = "sda"
"2" = "scl"
"3" = "addr"
"4" = "gnd"
"5" = "vdd"

[[models]]
id = "fix_movable"
kind = "digital"
description = "firmware re-addressable fixture"
[models.match]
value_re = "^SENSX$"
[models.params]
identity_only = true
warning = "identity only"
unlocked_by = "validated behavior"
[models.i2c]
address = 0x29
basis = "fixture p.3 default address 0x29"
[models.i2c.reassignable]
via = "software"
requires_pin = "xshut"
basis = "fixture p.3 programmable address"
[models.pins]
"1" = "sda"
"2" = "scl"
"3" = "xshut"
"4" = "gnd"
"5" = "vdd"

[[models]]
id = "fix_other"
kind = "digital"
description = "fixed 0x50 fixture"
[models.match]
value_re = "^SENSC$"
[models.params]
identity_only = true
warning = "identity only"
unlocked_by = "validated behavior"
[models.i2c]
address = 0x50
basis = "fixture p.4 fixed address 0x50"
[models.pins]
"1" = "sda"
"2" = "scl"
"3" = "gnd"
"4" = "vdd"
"#;

    fn fixture_dir() -> &'static PathBuf {
        static DIR: OnceLock<PathBuf> = OnceLock::new();
        DIR.get_or_init(|| {
            let dir =
                std::env::temp_dir().join(format!("hauksbee_i2c_address_{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("models.toml"), CARDS).unwrap();
            dir
        })
    }

    fn fixture_lib() -> ModelLibrary {
        let mut lib = ModelLibrary::empty();
        let errors = lib.load_user_dir(fixture_dir());
        assert!(errors.is_empty(), "fixture cards load: {errors:?}");
        lib
    }

    fn ic(reference: &str, value: &str, pads: &[(&str, i64)]) -> Component {
        Component {
            reference: reference.into(),
            value: value.into(),
            lib_id: "Sensor:Fixture".into(),
            footprint: "Package_SO:SOIC-8".into(),
            layer: "F.Cu".into(),
            pins: pads
                .iter()
                .map(|(n, net)| {
                    if *net == 0 {
                        Pin {
                            number: (*n).into(),
                            ..Default::default()
                        }
                    } else {
                        pin(n, *net)
                    }
                })
                .collect(),
            ..Default::default()
        }
    }

    /// A fixture part with its two bus pads on `sda`/`scl` and `extra` pads.
    fn part(reference: &str, value: &str, sda: i64, scl: i64, extra: &[(&str, i64)]) -> Component {
        let mut pads = vec![("1", sda), ("2", scl)];
        pads.extend_from_slice(extra);
        ic(reference, value, &pads)
    }

    fn clash(lib: &ModelLibrary, parts: Vec<Component>) -> Vec<LintFinding> {
        let b = board("i2c", NETS, parts);
        i2c_address_lint(&b, lib).findings
    }

    fn sensa(reference: &str, sda: i64, scl: i64) -> Component {
        part(reference, "SENSA", sda, scl, &[("3", GND), ("4", V33)])
    }

    fn sensb(reference: &str, addr: i64) -> Component {
        part(
            reference,
            "SENSB",
            SDA,
            SCL,
            &[("3", addr), ("4", GND), ("5", V33)],
        )
    }

    fn sensx(reference: &str, xshut: i64) -> Component {
        part(
            reference,
            "SENSX",
            SDA,
            SCL,
            &[("3", xshut), ("4", GND), ("5", V33)],
        )
    }

    /// Positive: two fixed 0x29 parts on one SDA/SCL pair are a High clash that
    /// names both parts, the address and the bus.
    #[test]
    fn two_parts_at_one_fixed_address_are_a_high_clash() {
        let f = clash(
            &fixture_lib(),
            vec![sensa("U1", SDA, SCL), sensa("U2", SDA, SCL)],
        );
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].check, LintCheck::I2cAddressClash);
        assert_eq!(f[0].severity, Severity::High);
        assert_eq!(f[0].refs, vec!["U1", "U2"]);
        assert!(f[0].message.contains("0x29"), "{}", f[0].message);
        assert!(f[0].message.contains("/SDA"), "{}", f[0].message);
        assert!(!f[0].is_abstention());
    }

    /// Silent control: the same two parts on different SDA/SCL pairs, a
    /// different fixed address, and a single part on a bus.
    #[test]
    fn different_buses_or_addresses_are_silent() {
        let lib = fixture_lib();
        let apart = clash(&lib, vec![sensa("U1", SDA, SCL), sensa("U2", SDA2, SCL2)]);
        assert!(apart.is_empty(), "different buses: {apart:?}");
        let other = clash(
            &lib,
            vec![
                sensa("U1", SDA, SCL),
                part("U2", "SENSC", SDA, SCL, &[("3", GND), ("4", V33)]),
            ],
        );
        assert!(other.is_empty(), "0x29 and 0x50: {other:?}");
        let alone = clash(&lib, vec![sensa("U1", SDA, SCL)]);
        assert!(alone.is_empty(), "{alone:?}");
    }

    /// The address-select pin decides: both tied low clash at 0x29, one tied
    /// high (0x2A) does not, and a pin tied through a resistor reads the same.
    #[test]
    fn address_straps_decide_the_clash() {
        let lib = fixture_lib();
        let same = clash(&lib, vec![sensb("U1", GND), sensb("U2", GND)]);
        assert_eq!(same.len(), 1, "{same:?}");
        assert_eq!(same[0].severity, Severity::High);
        assert!(same[0].message.contains("0x29"), "{}", same[0].message);
        let split = clash(&lib, vec![sensb("U1", GND), sensb("U2", V33)]);
        assert!(split.is_empty(), "0x29 against 0x2A: {split:?}");

        let b = board(
            "i2c",
            NETS,
            vec![sensb("U1", GND), sensb("U2", A0_U2), {
                let mut r = two_pin_resistor("R1", "10k");
                r.pins[0].net = Some(A0_U2);
                r.pins[1].net = Some(V33);
                r
            }],
        );
        assert!(
            i2c_address_lint(&b, &lib).findings.is_empty(),
            "a pull-up to +3V3 makes U2 0x2A"
        );
        let mut b = b;
        b.components[2].pins[1].net = Some(GND);
        let f = i2c_address_lint(&b, &lib).findings;
        assert_eq!(f.len(), 1, "a pull-down to GND makes U2 0x29 again: {f:?}");
        assert_eq!(f[0].severity, Severity::High);
    }

    /// A select pin tied to the bus's own SDA line reads as the SDA strap, which
    /// this card has no row for: the part's address is unknown, not assumed.
    #[test]
    fn a_strap_with_no_row_is_never_reported_as_a_clash() {
        let f = clash(&fixture_lib(), vec![sensb("U1", GND), sensb("U2", SDA)]);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(
            f[0].severity == Severity::Low && f[0].is_abstention(),
            "{f:?}"
        );
    }

    /// Abstention: a select pin driven from a GPIO leaves the address open; it
    /// could be 0x29, which another part uses, so one Low abstention says so. The
    /// control puts the other part at 0x50, where nothing overlaps.
    #[test]
    fn a_driven_select_pin_abstains_only_where_it_could_overlap() {
        let lib = fixture_lib();
        let mcu = ic("U9", "MCU", &[("1", GPIO_A)]);
        let f = clash(
            &lib,
            vec![sensa("U1", SDA, SCL), sensb("U2", GPIO_A), mcu.clone()],
        );
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].severity, Severity::Low);
        assert!(f[0].is_abstention(), "{}", f[0].message);
        assert!(f[0].message.contains("0x29"), "{}", f[0].message);
        assert!(
            f[0].message.contains("U9"),
            "names what drives it: {}",
            f[0].message
        );

        let c = part("U1", "SENSC", SDA, SCL, &[("3", GND), ("4", V33)]);
        let f = clash(&lib, vec![c, sensb("U2", GPIO_A), mcu]);
        assert!(f.is_empty(), "0x50 against {{0x29, 0x2A}}: {f:?}");
    }

    /// Silent control: a mux splits the bus. The same 0x29 part upstream and on
    /// each channel are three different net pairs and are never compared, while
    /// two on the same channel are.
    #[test]
    fn parts_behind_an_i2c_mux_are_on_other_buses() {
        let lib = fixture_lib();
        let mux = ic(
            "U5",
            "MUX",
            &[
                ("1", SDA),
                ("2", SCL),
                ("3", CH0_SDA),
                ("4", CH0_SCL),
                ("5", CH1_SDA),
                ("6", CH1_SCL),
            ],
        );
        let split = clash(
            &lib,
            vec![
                sensa("U1", SDA, SCL),
                sensa("U2", CH0_SDA, CH0_SCL),
                sensa("U3", CH1_SDA, CH1_SCL),
                mux.clone(),
            ],
        );
        assert!(split.is_empty(), "{split:?}");
        let same = clash(
            &lib,
            vec![
                sensa("U2", CH0_SDA, CH0_SCL),
                sensa("U3", CH0_SDA, CH0_SCL),
                mux,
            ],
        );
        assert_eq!(same.len(), 1, "two on one channel clash: {same:?}");
        assert_eq!(same[0].severity, Severity::High);
    }

    /// A small series resistor does not make a different bus; a DNP part takes
    /// no part in the bus.
    #[test]
    fn series_resistors_join_the_bus_and_dnp_parts_leave_it() {
        let lib = fixture_lib();
        let series = |net_pair: (i64, i64), r: &str| {
            let mut rs = two_pin_resistor("R2", r);
            rs.pins[0].net = Some(net_pair.0);
            rs.pins[1].net = Some(SDA_FAR);
            rs
        };
        let f = clash(
            &lib,
            vec![
                sensa("U1", SDA, SCL),
                sensa("U2", SDA_FAR, SCL),
                series((SDA, SCL), "100"),
            ],
        );
        assert_eq!(f.len(), 1, "a 100R series resistor stays on the bus: {f:?}");
        let f = clash(
            &lib,
            vec![
                sensa("U1", SDA, SCL),
                sensa("U2", SDA_FAR, SCL),
                series((SDA, SCL), "100k"),
            ],
        );
        assert!(f.is_empty(), "a 100k part is not a series link: {f:?}");

        let mut dnp = sensa("U2", SDA, SCL);
        dnp.dnp = true;
        let f = clash(&lib, vec![sensa("U1", SDA, SCL), dnp]);
        assert!(f.is_empty(), "a DNP part is not on the bus: {f:?}");
    }

    /// Repro B: a part firmware re-addresses is a High clash when the pin it
    /// needs is tied to a rail, and only a Low note when a GPIO drives it.
    #[test]
    fn a_re_addressable_part_is_a_note_when_a_gpio_can_sequence_it() {
        let lib = fixture_lib();
        let tied = clash(&lib, vec![sensa("U1", SDA, SCL), sensx("U2", V33)]);
        assert_eq!(tied.len(), 1, "{tied:?}");
        assert_eq!(tied[0].severity, Severity::High);
        assert!(
            tied[0].message.contains("xshut is tied to +3V3"),
            "{}",
            tied[0].message
        );

        let gpio = ic("U9", "MCU", &[("1", XSHUT_A)]);
        let driven = clash(
            &lib,
            vec![sensa("U1", SDA, SCL), sensx("U2", XSHUT_A), gpio.clone()],
        );
        assert_eq!(driven.len(), 1, "{driven:?}");
        assert_eq!(driven[0].severity, Severity::Low);
        assert!(!driven[0].is_abstention());
        assert!(
            driven[0].message.contains("firmware not read"),
            "{}",
            driven[0].message
        );

        let two = clash(
            &lib,
            vec![
                sensx("U1", XSHUT_A),
                sensx("U2", XSHUT_B),
                gpio,
                ic("U8", "MCU", &[("1", XSHUT_B)]),
            ],
        );
        assert_eq!(two.len(), 1, "{two:?}");
        assert_eq!(two[0].severity, Severity::Low);
        let both_tied = clash(&lib, vec![sensx("U1", V33), sensx("U2", GND)]);
        assert_eq!(both_tied[0].severity, Severity::High, "{both_tied:?}");
    }

    /// Shipped cards: two INA226 monitors with the same A1/A0 straps clash at
    /// 0x40, different straps (including a four-level pin on SDA) do not.
    #[test]
    fn shipped_ina226_cards_resolve_their_sixteen_addresses() {
        let lib = ModelLibrary::builtin();
        let ina = |reference: &str, a1: i64, a0: i64| {
            ic(
                reference,
                "INA226",
                &[
                    ("1", a1),
                    ("2", a0),
                    ("4", SDA),
                    ("5", SCL),
                    ("6", V33),
                    ("7", GND),
                ],
            )
        };
        let f = clash(&lib, vec![ina("U1", GND, GND), ina("U2", GND, GND)]);
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].severity, Severity::High);
        assert!(f[0].message.contains("0x40"), "{}", f[0].message);
        assert!(f[0].message.contains("INA226"), "{}", f[0].message);
        for other in [(GND, V33), (V33, GND), (GND, SDA), (GND, SCL)] {
            let f = clash(&lib, vec![ina("U1", GND, GND), ina("U2", other.0, other.1)]);
            assert!(f.is_empty(), "U2 straps {other:?}: {f:?}");
        }
    }

    /// Shipped cards: two VL53L0X with XSHUT tied high both answer at 0x29 and
    /// nothing can sequence them (High); with XSHUT on MCU GPIOs it is a note.
    #[test]
    fn shipped_vl53l0x_cards_need_a_sequenced_xshut() {
        let lib = ModelLibrary::builtin();
        let tof = |reference: &str, xshut: i64| {
            let mut c = ic(
                reference,
                "VL53L0X",
                &[
                    ("1", V33),
                    ("2", GND),
                    ("3", GND),
                    ("4", GND),
                    ("5", xshut),
                    ("6", GND),
                    ("7", 0),
                    ("8", 0),
                    ("9", SDA),
                    ("10", SCL),
                    ("11", V33),
                    ("12", GND),
                ],
            );
            c.footprint = "Package_LGA:LGA-12_4.4x2.4mm_P0.8mm".into();
            c
        };
        let f = clash(&lib, vec![tof("U1", V33), tof("U2", V33)]);
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].severity, Severity::High);
        assert!(f[0].message.contains("0x29"), "{}", f[0].message);

        let mcu = ic("U9", "MCU", &[("1", XSHUT_A), ("2", XSHUT_B)]);
        let f = clash(&lib, vec![tof("U1", XSHUT_A), tof("U2", XSHUT_B), mcu]);
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].severity, Severity::Low);
        assert!(!f[0].is_abstention());
    }
}
