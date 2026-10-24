//! Node-drive checks: three DC faults read from topology and pin roles.
//! Long-form how-and-why: docs/how-and-why/hauksbee-engine/checks.md.
//!
//! - **Unlimited base drive** ([`LintCheck::UnlimitedBaseDrive`]): a BJT whose
//!   base sits on the same node as a low-impedance driver (a supply rail, an
//!   analog-switch channel to a rail, a logic or MCU output) while its emitter
//!   is tied to a rail, with no resistor between. The base-emitter junction is
//!   forward-biased by the full driver level and conducts whatever the driver
//!   can source. The levels are the rail names the binder already knows; an
//!   unknown voltage or an emitter that is not on a rail abstains. A dual
//!   transistor is judged per unit through the card's `_qN` roles.
//! - **No DC discharge path** ([`LintCheck::CapNoDcPath`]): a node whose only
//!   connections are capacitor plates, high-impedance inputs (MOSFET gate,
//!   op-amp or comparator input, CMOS logic input, analog-switch select) and
//!   diode cathodes (which charge a node but never drain it), with at least
//!   one capacitor and at least one input or cathode. Nothing sets its level
//!   or drains charge that leakage, bias current or a power-up edge leaves
//!   there. Any other part on the node (a resistor, an unmodelled IC pin, a
//!   connector, a supply) is treated as a possible path and the node is not
//!   reported. Capacitors alone (a series pair for voltage rating) are a
//!   deliberate pattern and stay silent. When the node's whole island (every
//!   net its parts join) reaches no ground or supply and meets the board only
//!   at test pads or connector pins, the circuit is powered from whatever is
//!   wired to those pads, so there is no on-board level to miss: the node is
//!   stated as a note naming the pads instead of a warning.
//! - **Floating gate, select or enable** ([`LintCheck::FloatingGate`]): a
//!   MOSFET gate, analog-switch select or regulator/driver enable (active high
//!   or low) whose node holds nothing but MCU pins, connector pins and other
//!   such inputs: no pull resistor, supply or other driver. While the MCU is in
//!   reset, or the cable is unplugged, the node is undefined and the switch may
//!   conduct, or sit half-on and draw shoot-through current, and a regulator or
//!   driver may be on or off. An enable whose card states what an open pin does
//!   (`en_open`) or an internal pull (`en_pull_*`) has a defined level and is
//!   not reported.
//!
//! Only fitted parts count. Test points and DNP parts are not connections.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use hauksbee_bind::binder::{gpio_of_role, names_a_supply_of_unknown_voltage, power_rail_voltage};
use hauksbee_extract::assembly::AssemblyState;
use hauksbee_extract::{
    drives_a_level, is_capacitor, is_connector_like, is_ground, is_test_point_or_hole,
    is_unconnected_net, Component, ExtractedBoard, LintCheck, LintFinding, NetLintReport, Pin,
    Severity,
};
use hauksbee_models::{ComponentKind, ModelEntry, ModelLibrary};

use super::floating_inputs::is_cmos_logic_value;
use hauksbee_ir::units::format_volts;

/// Forward drive (V) across a base-emitter junction that counts as conducting:
/// one junction drop plus margin.
const JUNCTION_DRIVE_V: f64 = 0.9;
/// Level assumed for a supply whose name carries no voltage (`VDD_A`).
const UNNAMED_SUPPLY_V: f64 = 3.3;
/// Nets listed in one finding.
const MAX_NETS: usize = 8;

/// Pad lookup, models and a net-to-pins index built once per board.
pub(super) struct Index<'a> {
    pub(super) board: &'a ExtractedBoard,
    models: HashMap<&'a str, ModelEntry>,
    by_net: HashMap<i64, Vec<(&'a Component, &'a Pin)>>,
    names: HashMap<i64, &'a str>,
}

impl<'a> Index<'a> {
    pub(super) fn new(board: &'a ExtractedBoard, lib: &'a ModelLibrary) -> Self {
        let models = super::resolved_components(board, lib)
            .map(|(c, m)| (c.reference.as_str(), m))
            .collect();
        let mut by_net: HashMap<i64, Vec<(&Component, &Pin)>> = HashMap::new();
        for c in &board.components {
            if !AssemblyState::of(c).is_present() || is_test_point_or_hole(c) {
                continue;
            }
            for p in &c.pins {
                if let Some(net) = p.net.filter(|&n| n != 0) {
                    by_net.entry(net).or_default().push((c, p));
                }
            }
        }
        let names = board.nets.iter().map(|n| (n.id, n.name.as_str())).collect();
        Self {
            board,
            models,
            by_net,
            names,
        }
    }

    pub(super) fn name(&self, net: i64) -> &'a str {
        self.names.get(&net).copied().unwrap_or("")
    }

    pub(super) fn model(&self, c: &Component) -> Option<&ModelEntry> {
        self.models.get(c.reference.as_str())
    }

    /// The card's role for a pad, by pad number then by schematic pin name.
    pub(super) fn role(&self, c: &Component, p: &Pin) -> Option<&str> {
        let m = self.model(c)?;
        m.pins
            .get(&p.number)
            .or_else(|| m.pins.get(p.function.trim()))
            .map(String::as_str)
    }

    pub(super) fn members(&self, net: i64) -> &[(&'a Component, &'a Pin)] {
        self.by_net.get(&net).map_or(&[], Vec::as_slice)
    }

    /// The level a net is held at by name: ground, a voltage-named rail, or a
    /// supply whose voltage is unnamed (assumed positive).
    pub(super) fn level(&self, net: i64) -> Option<f64> {
        let name = self.name(net);
        if is_ground(name) {
            return Some(0.0);
        }
        if let Some(v) = power_rail_voltage(name) {
            return Some(v);
        }
        let negative = hauksbee_extract::norm(name).contains("VEE");
        (names_a_supply_of_unknown_voltage(name) && !negative).then_some(UNNAMED_SUPPLY_V)
    }

    /// Whether a carded part takes its supply from `net`: one of its
    /// supply-role pads (`vcc`, `vdd`...) is on it. A net named `V5` or `RAIL_A`
    /// states no voltage by name, but what powers a chip is a positive rail.
    pub(super) fn feeds_a_supply_pin(&self, net: i64) -> bool {
        self.members(net)
            .iter()
            .any(|(c, p)| self.role(c, p).is_some_and(is_supply_role))
    }

    /// The highest supply level among a part's supply-role pads.
    fn supply_of(&self, c: &Component) -> Option<f64> {
        c.pins
            .iter()
            .filter(|p| self.role(c, p).is_some_and(is_supply_role))
            .filter_map(|p| p.net.filter(|&n| n != 0).and_then(|n| self.level(n)))
            .max_by(f64::total_cmp)
    }
}

/// A card role that is a chip's positive supply input.
fn is_supply_role(role: &str) -> bool {
    matches!(
        role,
        "vcc" | "vdd" | "vddio" | "avcc" | "vdda" | "vio" | "5v"
    )
}

/// A microcontroller signal pin: a port or GPIO role, or an analog-capable
/// header pin (`a0`, `ain1`) that doubles as a digital output.
pub(super) fn is_gpio_role(role: &str) -> bool {
    gpio_of_role(role, false).is_some()
        || gpio_of_role(role, true).is_some()
        || role
            .strip_prefix("ain")
            .or_else(|| role.strip_prefix('a'))
            .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit()))
}

fn is_channel_role(role: &str) -> bool {
    role.starts_with("com")
        || role.starts_with("in_out")
        || (role.starts_with('s') && role[1..].starts_with(|c: char| c.is_ascii_digit()))
}

/// A low-impedance driver on a node: how it reads without the instance
/// (`switch channel to 'X'`), the driving part's reference if any, its level.
struct Driver {
    kind: String,
    part: Option<String>,
    level: f64,
}

impl Driver {
    fn text(&self) -> String {
        self.part
            .as_ref()
            .map_or_else(|| self.kind.clone(), |p| format!("{p}'s {}", self.kind))
    }
}

/// Levels a low-impedance driver on `net` can hold it at, excluding pins of
/// `except`.
fn drivers(idx: &Index<'_>, net: i64, except: &Component) -> Vec<Driver> {
    let mut out = Vec::new();
    if let Some(level) = idx.level(net) {
        out.push(Driver {
            kind: format!("the supply net '{}'", idx.name(net)),
            part: None,
            level,
        });
    }
    for (c, p) in idx.members(net) {
        if c.reference == except.reference {
            continue;
        }
        let (Some(model), Some(role)) = (idx.model(c), idx.role(c, p)) else {
            continue;
        };
        let mut push = |kind: String, level: f64| {
            out.push(Driver {
                kind,
                part: Some(c.reference.clone()),
                level,
            });
        };
        match model.kind {
            ComponentKind::AnalogSwitch if is_channel_role(role) => {
                // A closed channel passes whichever side holds a rail.
                for q in &c.pins {
                    let far = idx.role(c, q).is_some_and(is_channel_role);
                    let Some(far_net) = q.net.filter(|&n| n != 0 && n != net) else {
                        continue;
                    };
                    if let (true, Some(v)) = (far, idx.level(far_net)) {
                        push(format!("switch channel to '{}'", idx.name(far_net)), v);
                    }
                }
            }
            ComponentKind::Digital | ComponentKind::ShiftRegister
                if model.logic.outputs.iter().any(|o| o == role) =>
            {
                if let Some(vcc) = idx.supply_of(c) {
                    push(format!("output {role}"), vcc);
                    push(format!("output {role} (low)"), 0.0);
                }
            }
            ComponentKind::Mcu if is_gpio_role(role) => {
                if let Some(vcc) = idx.supply_of(c) {
                    push(format!("pin {role}"), vcc);
                    push(format!("pin {role} (low)"), 0.0);
                }
            }
            _ => {}
        }
    }
    out
}

/// One BJT unit: a single transistor, or one `_qN` half of a package.
pub(super) struct Unit<'a> {
    pub(super) label: String,
    pub(super) base: &'a Pin,
    pub(super) collector: &'a Pin,
    pub(super) emitter: &'a Pin,
}

pub(super) fn bjt_units<'a>(idx: &Index<'_>, c: &'a Component) -> Vec<Unit<'a>> {
    let mut by_suffix: std::collections::BTreeMap<String, [Option<&Pin>; 3]> =
        std::collections::BTreeMap::new();
    for p in &c.pins {
        let Some(role) = idx.role(c, p) else { continue };
        let (stem, suffix) = role
            .rsplit_once("_q")
            .map_or((role, String::new()), |(s, n)| (s, format!("Q{n}")));
        let slot = match stem {
            "base" | "b" => 0,
            "emitter" | "e" => 1,
            "collector" | "c" => 2,
            _ => continue,
        };
        by_suffix.entry(suffix).or_default()[slot] = Some(p);
    }
    by_suffix
        .into_iter()
        .filter_map(|(suffix, [b, e, col])| {
            Some(Unit {
                label: if suffix.is_empty() {
                    c.reference.clone()
                } else {
                    format!("{} {suffix}", c.reference)
                },
                base: b?,
                collector: col?,
                emitter: e?,
            })
        })
        .collect()
}

/// One BJT unit found driven straight from a low-impedance node.
struct BaseHit {
    label: String,
    part: String,
    driver: Driver,
    drive: f64,
    base: String,
    emitter: String,
}

/// BJTs whose base is driven straight from a low-impedance node, one finding
/// per distinct (part type, driver kind, emitter rail) so a repeated cell is
/// reported once with its instances listed.
fn base_drive_findings(idx: &Index<'_>) -> Vec<LintFinding> {
    let mut groups: BTreeMap<(String, String, String, String), Vec<BaseHit>> = BTreeMap::new();
    for c in &idx.board.components {
        let Some(model) = idx.model(c) else { continue };
        let pnp = match model.kind {
            ComponentKind::BjtNpn => false,
            ComponentKind::BjtPnp => true,
            _ => continue,
        };
        for unit in bjt_units(idx, c) {
            let (Some(base), Some(emitter)) = (
                unit.base.net.filter(|&n| n != 0),
                unit.emitter.net.filter(|&n| n != 0),
            ) else {
                continue;
            };
            let Some(v_e) = idx.level(emitter) else {
                continue;
            };
            let forward = |v: f64| if pnp { v_e - v } else { v - v_e };
            let Some((driver, drive)) = drivers(idx, base, c)
                .into_iter()
                .map(|d| {
                    let f = forward(d.level);
                    (d, f)
                })
                .filter(|(_, f)| *f >= JUNCTION_DRIVE_V)
                .max_by(|a, b| a.1.total_cmp(&b.1))
            else {
                continue;
            };
            let unit_kind = unit
                .label
                .strip_prefix(c.reference.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            groups
                .entry((
                    c.value.trim().to_string(),
                    unit_kind,
                    driver.kind.clone(),
                    idx.name(emitter).to_string(),
                ))
                .or_default()
                .push(BaseHit {
                    label: unit.label,
                    part: c.reference.clone(),
                    driver,
                    drive,
                    base: idx.name(base).to_string(),
                    emitter: idx.name(emitter).to_string(),
                });
        }
    }
    groups
        .into_values()
        .map(|hits| base_group_finding(&hits))
        .collect()
}

fn base_group_finding(hits: &[BaseHit]) -> LintFinding {
    let first = &hits[0];
    let labels: Vec<&str> = hits.iter().map(|h| h.label.as_str()).collect();
    let subject = if hits.len() == 1 {
        format!(
            "{} base is on the same node ('{}') as {}",
            first.label,
            first.base,
            first.driver.text()
        )
    } else {
        format!(
            "{} transistor units ({}) each have their base on the same node as a driver of the same \
             kind (for {}: {})",
            hits.len(),
            list_text(&labels),
            first.label,
            first.driver.text(),
        )
    };
    let mut nets: BTreeSet<String> = BTreeSet::new();
    for h in hits.iter().take(MAX_NETS) {
        nets.insert(h.base.clone());
        nets.insert(h.emitter.clone());
    }
    LintFinding {
        check: LintCheck::UnlimitedBaseDrive,
        severity: Severity::High,
        message: format!( "{subject}, holding it at {} with nothing in series, and the emitter is tied to '{}': \
             the base-emitter junction is forward-biased by about {} and conducts whatever the \
             driver can supply, far past the transistor's rating. Put a series base resistor (a few \
             kilohms for a small-signal transistor) between the driver and the base.", format_volts(first.driver.level), first.emitter, format_volts(first.drive)),
        refs: hits
            .iter()
            .map(|h| h.part.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect(),
        nets: nets.into_iter().take(MAX_NETS).collect(),
    }
}

/// Up to four items, then a count of the rest.
fn list_text(items: &[&str]) -> String {
    if items.len() > 4 {
        format!("{} and {} more", items[..4].join(", "), items.len() - 4)
    } else {
        items.join(", ")
    }
}

/// What a pin on a node is, for the DC-path question.
#[derive(PartialEq, Eq)]
enum Load {
    Capacitor,
    HighZ,
    /// An op-amp or comparator input whose card says nothing about the input
    /// (no common-mode range, no internal DC path): high impedance by the pin's
    /// kind, but not shown to be, so it never raises a fault alone.
    SilentInput,
    /// A diode cathode: it can charge the node but never drains it.
    ChargeOnly,
    /// Anything that may set or drain the node's level.
    Other,
}

fn classify_load(idx: &Index<'_>, c: &Component, p: &Pin) -> Load {
    if is_capacitor(c) {
        return Load::Capacitor;
    }
    let (Some(model), Some(role)) = (idx.model(c), idx.role(c, p)) else {
        return Load::Other;
    };
    if model.kind == ComponentKind::Diode && role == "cathode" {
        return Load::ChargeOnly;
    }
    let analog_input = matches!(model.kind, ComponentKind::Opamp | ComponentKind::Comparator)
        && (role.starts_with("in_plus") || role.starts_with("in_minus"));
    if analog_input {
        // The card gives the input a DC path of its own (an internally biased
        // amplifier input): it drains the node.
        if model
            .input_dc_path
            .iter()
            .any(|d| d.role.eq_ignore_ascii_case(role))
        {
            return Load::Other;
        }
        // A card that states the input's common-mode range describes a true
        // op-amp or comparator input; a silent one may be a biased amplifier.
        let described = model.envelope.iter().any(|e| {
            matches!(e, hauksbee_models::OperatingEnvelope::InputCommonMode { inputs, .. }
                if inputs.iter().any(|i| i.eq_ignore_ascii_case(role)))
        });
        return if described {
            Load::HighZ
        } else {
            Load::SilentInput
        };
    }
    let high_z = match model.kind {
        ComponentKind::Nmos | ComponentKind::Pmos => role == "gate" || is_gate_n(role),
        ComponentKind::AnalogSwitch => role.starts_with("ctrl"),
        ComponentKind::Digital | ComponentKind::ShiftRegister => {
            is_cmos_logic_value(&c.value) && model.logic.inputs.iter().any(|i| i == role)
        }
        _ => false,
    };
    if high_z {
        Load::HighZ
    } else {
        Load::Other
    }
}

fn is_gate_n(role: &str) -> bool {
    role.strip_prefix('g')
        .is_some_and(|n| !n.is_empty() && n.chars().all(|ch| ch.is_ascii_digit()))
}

/// A node with no DC discharge path: its name, its capacitors, and the
/// high-impedance inputs and diode cathodes that touch it.
struct CapNode {
    name: String,
    caps: BTreeSet<String>,
    others: BTreeSet<String>,
    /// Whether some high-impedance member is firmly so (a gate, a CMOS input, a
    /// described op-amp input, a diode cathode) and not only an input the card is
    /// silent about.
    firm: bool,
    /// Test pads and connectors that are the island's only way to the
    /// outside, when the island reaches no ground or supply on the board.
    off_board: Option<BTreeSet<String>>,
}

/// For each net, the test pads and connectors of its island when that island
/// (nets joined through any fitted part, pads and connectors included) holds
/// no ground or supply net. Absent when the island reaches a level or has no
/// pad at all (a fully closed island stays a warning).
fn off_board_islands(idx: &Index<'_>) -> HashMap<i64, BTreeSet<String>> {
    fn find(parent: &mut HashMap<i64, i64>, n: i64) -> i64 {
        let mut root = n;
        while let Some(&p) = parent.get(&root).filter(|&&p| p != root) {
            root = p;
        }
        parent.insert(n, root);
        root
    }
    let mut parent: HashMap<i64, i64> = HashMap::new();
    let fitted = || {
        idx.board
            .components
            .iter()
            .filter(|c| AssemblyState::of(c).is_present())
    };
    for c in fitted() {
        let nets: Vec<i64> = c
            .pins
            .iter()
            .filter_map(|p| p.net.filter(|&n| n != 0))
            .collect();
        for w in nets.windows(2) {
            let (a, b) = (find(&mut parent, w[0]), find(&mut parent, w[1]));
            if a != b {
                parent.insert(a, b);
            }
        }
        if let Some(&n) = nets.first() {
            find(&mut parent, n);
        }
    }
    let nets: Vec<i64> = parent.keys().copied().collect();
    let mut leveled: BTreeSet<i64> = BTreeSet::new();
    for &n in &nets {
        if idx.level(n).is_some() {
            leveled.insert(find(&mut parent, n));
        }
    }
    let mut pads: HashMap<i64, BTreeSet<String>> = HashMap::new();
    for c in fitted().filter(|c| is_test_point_or_hole(c) || is_connector_like(c)) {
        for n in c.pins.iter().filter_map(|p| p.net.filter(|&n| n != 0)) {
            let root = find(&mut parent, n);
            pads.entry(root).or_default().insert(c.reference.clone());
        }
    }
    nets.into_iter()
        .filter_map(|n| {
            let root = find(&mut parent, n);
            if leveled.contains(&root) {
                None
            } else {
                pads.get(&root).map(|p| (n, p.clone()))
            }
        })
        .collect()
}

/// Nodes held only by capacitor plates, high-impedance inputs and diode
/// cathodes, one finding per distinct wiring so a repeated cell is reported
/// once with its nodes listed.
fn cap_node_findings(idx: &Index<'_>) -> Vec<LintFinding> {
    let mut nets: Vec<i64> = idx.by_net.keys().copied().collect();
    nets.sort_unstable();
    let islands = off_board_islands(idx);
    let mut groups: BTreeMap<Vec<String>, Vec<CapNode>> = BTreeMap::new();
    for net in nets {
        let name = idx.name(net);
        if is_unconnected_net(name) || idx.level(net).is_some() {
            continue;
        }
        let mut node = CapNode {
            name: name.to_string(),
            caps: BTreeSet::new(),
            others: BTreeSet::new(),
            firm: false,
            off_board: islands.get(&net).cloned(),
        };
        let mut shape = vec![format!("off-board:{}", node.off_board.is_some())];
        let mut open = false;
        for (c, p) in idx.members(net) {
            match classify_load(idx, c, p) {
                Load::Capacitor => node.caps.insert(c.reference.clone()),
                Load::HighZ | Load::ChargeOnly => {
                    node.firm = true;
                    node.others.insert(c.reference.clone())
                }
                Load::SilentInput => node.others.insert(c.reference.clone()),
                Load::Other => {
                    open = true;
                    false
                }
            };
            shape.push(format!(
                "{}|{}|{}",
                c.lib_id.trim(),
                c.value.trim(),
                p.number
            ));
        }
        if open || node.caps.is_empty() || node.others.is_empty() {
            continue;
        }
        shape.sort();
        groups.entry(shape).or_default().push(node);
    }
    groups
        .into_values()
        .map(|nodes| cap_group_finding(&nodes))
        .collect()
}

fn cap_group_finding(nodes: &[CapNode]) -> LintFinding {
    let first = &nodes[0];
    let text =
        |set: &BTreeSet<String>| list_text(&set.iter().map(String::as_str).collect::<Vec<_>>());
    let names: Vec<&str> = nodes.iter().map(|n| n.name.as_str()).collect();
    let subject = if nodes.len() == 1 {
        format!("net '{}' has", first.name)
    } else {
        format!("{} nets ({}) each have", nodes.len(), list_text(&names))
    };
    let held = format!(
        "only capacitor plates ({}){} on them",
        text(&first.caps),
        if first.others.is_empty() {
            String::new()
        } else {
            format!(
                " and high-impedance inputs or diode cathodes ({})",
                text(&first.others)
            )
        },
    );
    if !first.firm {
        // Only inputs whose card says nothing about them: the amplifier may bias
        // them internally, so name the doubt instead of a fault.
        return LintFinding {
            check: LintCheck::CapNoDcPath,
            severity: Severity::Low,
            message: format!(
                "{subject} {held}, and the card for the input does not say whether the part biases \
                 it internally: an internally biased amplifier input sets its own DC level behind a \
                 coupling capacitor, a plain op-amp or comparator input does not. Confirm the \
                 input's DC path in the datasheet; if there is none, add a bias resistor to a \
                 reference or ground.",
            ),
            refs: nodes
                .iter()
                .flat_map(|n| n.caps.iter().chain(&n.others))
                .cloned()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect(),
            nets: names
                .iter()
                .take(MAX_NETS)
                .map(|n| (*n).to_string())
                .collect(),
        };
    }
    let (severity, message) = first.off_board.as_ref().map_or_else(
        || {
            (
                Severity::Medium,
                format!(
                    "{subject} {held}: no resistor, supply or driver gives the node a DC level or drains \
                     it, so charge from leakage, input bias current, a diode or a power-up edge stays on \
                     it and its voltage is undefined (a diode only ratchets it one way). Add a bias or \
                     bleed resistor to a rail or ground, or remove the capacitor.",
                ),
            )
        },
        |pads| {
            (
                Severity::Low,
                format!(
                    "{subject} {held}, and the circuit they sit in reaches no ground or supply on this \
                     board: it meets the board only at {}. Its DC level is set by whatever is wired to \
                     those pads, which this board cannot show. Assumed: the external wiring gives the \
                     node a DC path; if nothing does, add a bleed resistor across the capacitor.",
                    text(pads),
                ),
            )
        },
    );
    LintFinding {
        check: LintCheck::CapNoDcPath,
        severity,
        message,
        refs: nodes
            .iter()
            .flat_map(|n| n.caps.iter().chain(&n.others))
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect(),
        nets: names
            .iter()
            .take(MAX_NETS)
            .map(|n| (*n).to_string())
            .collect(),
    }
}

/// What a floating-input node feeds.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Sink {
    Gate,
    Select,
    /// The enable or shutdown input of a regulator or driver whose card states
    /// neither what an open pin does nor an internal pull.
    Enable,
}

/// An enable, shutdown or on/off input, active high or low, by card role.
fn is_enable_role(role: &str) -> bool {
    matches!(
        role,
        "en" | "en_n" | "enable" | "enable_n" | "shdn" | "shdn_n" | "on_off"
    ) || role
        .strip_prefix("en")
        .is_some_and(|n| !n.is_empty() && n.chars().all(|ch| ch.is_ascii_digit()))
}

/// Whether the card says what an open enable pin does: an `en_open` state or
/// an internal `en_pull_*`. Either makes the reset level defined.
fn enable_state_stated(model: &ModelEntry) -> bool {
    model
        .params
        .0
        .keys()
        .any(|k| k == "en_open" || k.starts_with("en_pull_") || k == "keeper_r_ohms")
}

/// What kind of input the pad `role` of a `model` part is, when it is one a
/// floating node leaves undefined.
fn sink_of(model: &ModelEntry, role: &str) -> Option<Sink> {
    match model.kind {
        ComponentKind::Nmos | ComponentKind::Pmos => {
            (role == "gate" || is_gate_n(role)).then_some(Sink::Gate)
        }
        ComponentKind::AnalogSwitch => role.starts_with("ctrl").then_some(Sink::Select),
        ComponentKind::Vreg | ComponentKind::Digital => {
            (is_enable_role(role) && !enable_state_stated(model)).then_some(Sink::Enable)
        }
        _ => None,
    }
}

/// What a node feeds, keyed by (net, input kind): the driver pins, the
/// input-bearing parts, whether a part is not a regulator, and the pads of a
/// plug-in module that drive the node (its level at boot is the module's).
type NodeInputs = (BTreeSet<String>, BTreeSet<String>, bool, BTreeSet<String>);

/// The nodes whose gate, select or enable inputs only MCU, connector or module
/// pins hold, keyed by (net, input kind).
fn floating_gate_nodes(idx: &Index<'_>) -> BTreeMap<(i64, Sink), NodeInputs> {
    let mut nodes: BTreeMap<(i64, Sink), NodeInputs> = BTreeMap::new();
    for c in &idx.board.components {
        let Some(model) = idx.model(c) else { continue };
        for p in &c.pins {
            let Some(role) = idx.role(c, p) else { continue };
            let Some(sink) = sink_of(model, role) else {
                continue;
            };
            let Some(net) = p.net.filter(|&n| n != 0) else {
                continue;
            };
            if idx.level(net).is_some() || is_unconnected_net(idx.name(net)) {
                continue;
            }
            let mut drivers = BTreeSet::new();
            let mut modules = BTreeSet::new();
            let mut defined = false;
            for (o, q) in idx.members(net) {
                if std::ptr::eq(*o, c) && std::ptr::eq(*q, p) {
                    continue;
                }
                let mcu = idx
                    .model(o)
                    .zip(idx.role(o, q))
                    .is_some_and(|(m, r)| m.kind == ComponentKind::Mcu && is_gpio_role(r));
                let other_input = sink == Sink::Enable
                    && idx
                        .model(o)
                        .zip(idx.role(o, q))
                        .is_some_and(|(m, r)| sink_of(m, r) == Some(Sink::Enable));
                if is_connector_like(o) && drives_a_level(o, q) {
                    modules.insert(format!("{} pin {}", o.reference, q.number));
                } else if mcu || is_connector_like(o) {
                    drivers.insert(format!("{} pin {}", o.reference, q.number));
                } else if other_input {
                    // Another enable on the node floats with it.
                } else if !matches!(classify_load(idx, o, q), Load::HighZ | Load::SilentInput) {
                    defined = true;
                }
            }
            if !defined && (!drivers.is_empty() || !modules.is_empty()) {
                let entry = nodes.entry((net, sink)).or_default();
                entry.0.extend(drivers);
                entry.3.extend(modules);
                entry.1.insert(c.reference.clone());
                entry.2 |= model.kind != ComponentKind::Vreg;
            }
        }
    }
    nodes
}

/// MOSFET gates, analog-switch selects and enable inputs held only by MCU or
/// connector pins, one finding per node: the inputs a node feeds share one
/// cause and one fix.
fn floating_gate_findings(idx: &Index<'_>) -> Vec<LintFinding> {
    floating_gate_nodes(idx)
        .into_iter()
        .map(|((net, sink), (drivers, parts, not_regulator, modules))| {
            let (what, severity) = match sink {
                Sink::Gate => ("MOSFET gate", Severity::Low),
                Sink::Select => ("analog-switch select", Severity::Medium),
                Sink::Enable if not_regulator => ("enable input", Severity::Low),
                Sink::Enable => ("regulator enable input", Severity::Medium),
            };
            let name = idx.name(net);
            let plural = if parts.len() == 1 { "" } else { "s" };
            if drivers.is_empty() {
                // Only a module drives the node: a driver, but no card states its boot level.
                return LintFinding {
                    check: LintCheck::FloatingGate,
                    severity: Severity::Low,
                    message: format!(
                        "net '{name}' drives {} {what}{plural} ({}) and is driven by {}; its level at boot \
                         is the module's own and no card states it, so confirm it holds the off level \
                         until the module is up.",
                        parts.len(),
                        list_text(&parts.iter().map(String::as_str).collect::<Vec<_>>()),
                        list_text(&modules.iter().map(String::as_str).collect::<Vec<_>>()),
                    ),
                    refs: parts.into_iter().collect(),
                    nets: vec![name.to_string()],
                };
            }
            let (effect, fix) = match sink {
                Sink::Gate => (
                    "the transistor may conduct or sit half-on",
                    "Add a pull resistor (about 100 kilohms) to the level that is safe at power-up.",
                ),
                Sink::Select => (
                    "the switch may connect either throw, or draw shoot-through current half-on",
                    "Add a pull resistor (about 100 kilohms) to the level that is safe at power-up.",
                ),
                Sink::Enable => (
                    "the part may be on, off or chattering through the reset, and whatever it feeds \
                     powers up in an undefined state (the card states neither what an open pin does \
                     nor an internal pull; if the datasheet gives the pin one, record it on the card)",
                    "Add a pull resistor (about 100 kilohms) to the off level for a load that must \
                     stay off until the firmware runs.",
                ),
            };
            LintFinding {
                check: LintCheck::FloatingGate,
                severity,
                message: format!(
                    "net '{name}' drives {} {what}{plural} ({}) and is held only by {} with no pull-up or \
                     pull-down resistor. While the MCU is in reset or booting (or the cable is unplugged) \
                     the pin is high-impedance, so the node floats and {effect}. {fix}",
                    parts.len(),
                    list_text(&parts.iter().map(String::as_str).collect::<Vec<_>>()),
                    list_text(&drivers.iter().map(String::as_str).collect::<Vec<_>>()),
                ),
                refs: parts.into_iter().collect(),
                nets: vec![name.to_string()],
            }
        })
        .collect()
}

/// Run the node-drive checks and the sibling diff, which share one index.
#[must_use]
pub fn node_drive_lint(board: &ExtractedBoard, lib: &ModelLibrary) -> NetLintReport {
    let idx = Index::new(board, lib);
    let mut report = NetLintReport::default();
    report
        .findings
        .extend(super::siblings::sibling_findings(&idx));
    report.findings.extend(base_drive_findings(&idx));
    report.findings.extend(cap_node_findings(&idx));
    report.findings.extend(floating_gate_findings(&idx));
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{board, pin_fn};

    const GND: i64 = 1;
    const V5: i64 = 2;

    fn part(reference: &str, value: &str, lib_id: &str, pins: &[(&str, i64)]) -> Component {
        Component {
            reference: reference.into(),
            value: value.into(),
            lib_id: lib_id.into(),
            footprint: "X".into(),
            pins: pins.iter().map(|(n, net)| pin_fn(n, *net, "")).collect(),
            ..Default::default()
        }
    }

    fn resistor(reference: &str, a: i64, b: i64) -> Component {
        part(reference, "10k", "Device:R", &[("1", a), ("2", b)])
    }

    fn npn(base: i64) -> Component {
        part(
            "Q1",
            "BC847",
            "Device:Q_NPN",
            &[("1", base), ("2", GND), ("3", 20)],
        )
    }

    /// An SPDT analog switch on +5V: COM on `com`, B2 on the +5V rail,
    /// select on `sel`.
    fn spdt(com: i64, sel: i64) -> Component {
        part(
            "U1",
            "SN74LVC1G3157",
            "Switch:SPDT",
            &[
                ("1", V5),
                ("2", GND),
                ("3", 30),
                ("4", com),
                ("5", V5),
                ("6", sel),
            ],
        )
    }

    /// An `ATmega` whose pin 2 is a GPIO and whose pin 7 is on +5V.
    fn mcu(pin2: i64) -> Component {
        part(
            "U2",
            "ATmega328P",
            "MCU:ATmega328P",
            &[("2", pin2), ("7", V5), ("8", GND)],
        )
    }

    fn run(parts: Vec<Component>, extra: &[(i64, &str)]) -> Vec<LintFinding> {
        let mut nets = vec![(GND, "GND"), (V5, "+5V")];
        nets.extend_from_slice(extra);
        let b = board("t", &nets, parts);
        node_drive_lint(&b, &ModelLibrary::builtin()).findings
    }

    fn of(found: &[LintFinding], check: LintCheck) -> Vec<&LintFinding> {
        found.iter().filter(|f| f.check == check).collect()
    }

    #[test]
    fn a_base_driven_straight_from_a_rail_or_a_switch_to_a_rail_is_named() {
        // Base on the +5V rail itself.
        let found = run(vec![npn(V5)], &[(20, "OUT")]);
        let hits = of(&found, LintCheck::UnlimitedBaseDrive);
        assert_eq!(hits.len(), 1, "{found:#?}");
        assert_eq!(hits[0].severity, Severity::High);
        assert_eq!(hits[0].refs, ["Q1"]);
        // Base on a switch's COM, whose throw is tied to +5V.
        let found = run(
            vec![npn(10), spdt(10, 11)],
            &[(10, "DRV"), (11, "SEL"), (20, "OUT")],
        );
        let hits = of(&found, LintCheck::UnlimitedBaseDrive);
        assert_eq!(hits.len(), 1, "{found:#?}");
        assert!(
            hits[0].message.contains("U1's switch channel"),
            "{}",
            hits[0].message
        );
    }

    #[test]
    fn a_series_base_resistor_or_an_undriven_base_is_clean() {
        let with_resistor = run(
            vec![npn(12), resistor("R1", 10, 12), spdt(10, 11)],
            &[(10, "DRV"), (11, "SEL"), (12, "BASE"), (20, "OUT")],
        );
        assert!(of(&with_resistor, LintCheck::UnlimitedBaseDrive).is_empty());
        // A base left to a plain signal net: nothing known drives it.
        let signal = run(vec![npn(10)], &[(10, "SIG"), (20, "OUT")]);
        assert!(of(&signal, LintCheck::UnlimitedBaseDrive).is_empty());
    }

    fn cap(reference: &str, node: i64) -> Component {
        part(reference, "100n", "Device:C", &[("1", node), ("2", GND)])
    }

    #[test]
    fn a_capacitor_coupled_gate_with_no_bias_has_no_dc_path() {
        let found = run(
            vec![cap("C1", 10), cap("C2", 10), nmos(10)],
            &[(10, "MID"), (21, "LOAD")],
        );
        let hits = of(&found, LintCheck::CapNoDcPath);
        assert_eq!(hits.len(), 1, "{found:#?}");
        assert_eq!(hits[0].nets, ["MID"]);
        assert!(
            hits[0].message.contains("C1, C2") && hits[0].message.contains("Q2"),
            "{}",
            hits[0].message
        );
    }

    fn diode(reference: &str, anode: i64, cathode: i64) -> Component {
        part(
            reference,
            "1N4148",
            "Device:D",
            &[("A", anode), ("K", cathode)],
        )
    }

    /// An amplifier card (`value`) with `+IN` on pad 3, `-IN` on pad 4 and
    /// `extra` TOML lines (a DC path, a common-mode row) under `[models]`.
    fn amplifier_lib(value: &str, extra: &str) -> ModelLibrary {
        let dir = hauksbee_testkit::fresh_scratch_dir("node_drive", value);
        let card = format!(
            r#"
[[models]]
id = "{id}"
kind = "opamp"
description = "amplifier fixture"
[models.match]
value_re = "^{value}$"
[models.pins]
"1" = "out"
"2" = "vcc"
"3" = "in_plus"
"4" = "in_minus"
"5" = "gnd"
[models.params]
gain = 100000.0
rail_lo = 0.0
rail_hi = 5.0
{extra}
"#,
            id = value.to_ascii_lowercase()
        );
        std::fs::write(dir.join("models.toml"), card).unwrap();
        let mut lib = ModelLibrary::builtin();
        let errors = lib.load_user_dir(&dir);
        assert!(errors.is_empty(), "test card must load: {errors:?}");
        lib
    }

    /// `AUDIO_IN` through a 1 uF series capacitor to the amplifier's `+IN`.
    fn ac_coupled(lib: &ModelLibrary, value: &str) -> Vec<LintFinding> {
        let mut amp = part(
            "U1",
            value,
            "Amplifier:X",
            &[("1", 20), ("2", V5), ("3", 11), ("4", 20), ("5", GND)],
        );
        amp.reference = "U1".into();
        let coupling = part("C1", "1u", "Device:C", &[("1", 10), ("2", 11)]);
        let jack = part("J1", "AUDIO", "Connector:Conn_01x01", &[("1", 10)]);
        let nets = [
            (GND, "GND"),
            (V5, "+5V"),
            (10, "AUDIO_IN"),
            (11, "IN_P"),
            (20, "OUT"),
        ];
        let b = board("t", &nets, vec![amp, coupling, jack]);
        node_drive_lint(&b, lib).findings
    }

    #[test]
    fn an_internally_biased_amplifier_input_behind_a_coupling_capacitor_has_a_dc_path() {
        // The card gives +IN a 100 kohm path to an internal bias: silent.
        let biased = amplifier_lib(
            "AMPBIAS",
            "[[models.input_dc_path]]\nrole = \"in_plus\"\nto = \"bias\"\nohms = 100000.0\nbasis = \"fixture datasheet: input impedance 100 kohm\"",
        );
        let found = ac_coupled(&biased, "AMPBIAS");
        assert!(of(&found, LintCheck::CapNoDcPath).is_empty(), "{found:#?}");
        // A card that says nothing about the input: a note to confirm, not a fault.
        let silent = amplifier_lib("AMPSILENT", "");
        let found = ac_coupled(&silent, "AMPSILENT");
        let hits = of(&found, LintCheck::CapNoDcPath);
        assert_eq!(hits.len(), 1, "{found:#?}");
        assert_eq!(hits[0].severity, Severity::Low);
        assert!(
            hits[0].message.contains("does not say"),
            "{}",
            hits[0].message
        );
        // A card that describes a true op-amp input (its common-mode range) and
        // gives no DC path: still a fault.
        let plain = amplifier_lib(
            "AMPPLAIN",
            "[[models.envelope]]\nkind = \"input_common_mode\"\nsupply = \"vcc\"\ninputs = [\"in_plus\", \"in_minus\"]\nheadroom_v = 1.5\nbasis = \"fixture datasheet VCM\"",
        );
        let found = ac_coupled(&plain, "AMPPLAIN");
        let hits = of(&found, LintCheck::CapNoDcPath);
        assert_eq!(hits.len(), 1, "{found:#?}");
        assert_eq!(hits[0].severity, Severity::Medium);
    }

    #[test]
    fn a_capacitor_and_a_diode_cathode_ratchet_with_no_way_down() {
        // The diode can only charge MID; nothing ever discharges it.
        let found = run(
            vec![cap("C1", 10), diode("D1", 11, 10)],
            &[(10, "MID"), (11, "IN")],
        );
        let hits = of(&found, LintCheck::CapNoDcPath);
        assert_eq!(hits.len(), 1, "{found:#?}");
        assert!(hits[0].message.contains("D1"), "{}", hits[0].message);
        // Anode on the node is a discharge path, as is a bleed resistor.
        let found = run(
            vec![cap("C1", 10), diode("D1", 10, 11)],
            &[(10, "MID"), (11, "IN")],
        );
        assert!(of(&found, LintCheck::CapNoDcPath).is_empty(), "{found:#?}");
        let found = run(
            vec![cap("C1", 10), diode("D1", 11, 10), resistor("R1", 10, GND)],
            &[(10, "MID"), (11, "IN")],
        );
        assert!(of(&found, LintCheck::CapNoDcPath).is_empty(), "{found:#?}");
    }

    fn test_pad(reference: &str, node: i64) -> Component {
        part(
            reference,
            "TestPoint",
            "Connector:TestPoint",
            &[("1", node)],
        )
    }

    #[test]
    fn a_capacitor_and_diode_fed_only_through_test_pads_is_a_stated_condition() {
        // An LED with a capacitor across it, wired to the outside world only
        // through pads: nothing on this board can set its level, so the node
        // is stated as a note naming the pads, not raised as a warning.
        let across = |r: &str| part(r, "100n", "Device:C", &[("1", 11), ("2", 10)]);
        let found = run(
            vec![
                across("C1"),
                diode("D1", 11, 10),
                test_pad("TP1", 10),
                test_pad("TP2", 11),
            ],
            &[(10, "K"), (11, "A")],
        );
        let hits = of(&found, LintCheck::CapNoDcPath);
        assert_eq!(hits.len(), 1, "{found:#?}");
        assert_eq!(hits[0].severity, Severity::Low);
        assert!(hits[0].message.contains("TP1, TP2"), "{}", hits[0].message);
        // The same cell with its anode pad replaced by a ground return reaches
        // a level on the board: the undefined node is the board's own fault.
        let found = run(
            vec![
                across("C1"),
                diode("D1", GND, 10),
                cap("C2", 10),
                test_pad("TP1", 10),
            ],
            &[(10, "K")],
        );
        let hits = of(&found, LintCheck::CapNoDcPath);
        assert_eq!(hits.len(), 1, "{found:#?}");
        assert_eq!(hits[0].severity, Severity::Medium);
        // A closed island with no pad at all stays a warning too.
        let found = run(
            vec![across("C1"), diode("D1", 11, 10)],
            &[(10, "K"), (11, "A")],
        );
        let hits = of(&found, LintCheck::CapNoDcPath);
        assert_eq!(hits.len(), 1, "{found:#?}");
        assert_eq!(hits[0].severity, Severity::Medium);
    }

    #[test]
    fn a_bleed_resistor_or_a_single_capacitor_is_clean() {
        let bled = run(
            vec![
                cap("C1", 10),
                cap("C2", 10),
                nmos(10),
                resistor("R1", 10, GND),
            ],
            &[(10, "MID"), (21, "LOAD")],
        );
        assert!(of(&bled, LintCheck::CapNoDcPath).is_empty());
        // Two capacitors in series with nothing else are a deliberate pattern.
        let series = run(vec![cap("C1", 10), cap("C2", 10)], &[(10, "MID")]);
        assert!(of(&series, LintCheck::CapNoDcPath).is_empty());
        // One capacitor on a signal node is an ordinary AC-coupled filter pin.
        let lone = run(vec![cap("C1", 10)], &[(10, "MID")]);
        assert!(of(&lone, LintCheck::CapNoDcPath).is_empty());
        // A node tied to a supply rail has a DC level by definition.
        let rail = run(vec![cap("C1", V5), cap("C2", V5)], &[]);
        assert!(of(&rail, LintCheck::CapNoDcPath).is_empty());
    }

    fn nmos(gate: i64) -> Component {
        part(
            "Q2",
            "2N7002",
            "Device:Q_NMOS",
            &[("G", gate), ("S", GND), ("D", 21)],
        )
    }

    #[test]
    fn a_gate_or_select_driven_only_by_an_mcu_pin_floats_at_boot() {
        let found = run(vec![nmos(10), mcu(10)], &[(10, "GATE"), (21, "LOAD")]);
        let hits = of(&found, LintCheck::FloatingGate);
        assert_eq!(hits.len(), 1, "{found:#?}");
        assert_eq!(hits[0].refs, ["Q2"]);
        assert_eq!(hits[0].severity, Severity::Low);
        let found = run(vec![spdt(10, 11), mcu(11)], &[(10, "COM"), (11, "SEL")]);
        let hits = of(&found, LintCheck::FloatingGate);
        assert_eq!(hits.len(), 1, "{found:#?}");
        assert_eq!(hits[0].refs, ["U1"]);
        assert_eq!(hits[0].severity, Severity::Medium);
    }

    /// A regulator by `value` with its enable pad on `en` (pad 3) and its
    /// input and ground on the rails.
    fn ldo(value: &str, en: i64) -> Component {
        part(
            "U3",
            value,
            "Regulator:LDO",
            &[("1", V5), ("2", GND), ("3", en), ("5", 22)],
        )
    }

    #[test]
    fn an_enable_driven_only_by_an_mcu_pin_floats_in_reset() {
        let found = run(
            vec![ldo("RT9080-33", 10), mcu(10)],
            &[(10, "LDO_EN"), (22, "OUT")],
        );
        let hits = of(&found, LintCheck::FloatingGate);
        assert_eq!(hits.len(), 1, "{found:#?}");
        assert_eq!(hits[0].refs, ["U3"]);
        assert_eq!(hits[0].severity, Severity::Medium);
        assert!(
            hits[0].message.contains("regulator enable input"),
            "{}",
            hits[0].message
        );
        // An active-low on/off pin is held to the same rule.
        let low = part("U3", "LM2596S-5.0", "Regulator:LM2596", &[("5", 10)]);
        let found = run(vec![low, mcu(10)], &[(10, "BUCK_OFF")]);
        assert_eq!(of(&found, LintCheck::FloatingGate).len(), 1, "{found:#?}");
    }

    /// An LDO enable held only by a board-to-board part whose pad is typed `kind`.
    fn enable_held_by_connector(value: &str, lib_id: &str, kind: &str) -> Vec<LintFinding> {
        let mut conn = part("Module1", value, lib_id, &[("75", 10)]);
        conn.footprint = "Connector_Hirose:Hirose_DF40C-100DS".into();
        conn.pins[0].kind = kind.into();
        run(
            vec![ldo("RT9080-33", 10), conn],
            &[(10, "LDO_EN"), (22, "OUT")],
        )
    }

    #[test]
    fn a_module_driven_enable_is_a_note_but_a_header_driven_one_stays() {
        // The compute module's output pad drives the enable: a Low note that
        // names the module, not a Medium "high-impedance" warning.
        let found = enable_held_by_connector("CM4", "Module:CM4", "output");
        let hits = of(&found, LintCheck::FloatingGate);
        assert_eq!(hits.len(), 1, "{found:#?}");
        assert_eq!(hits[0].severity, Severity::Low);
        assert!(hits[0].message.contains("Module1 pin 75"));
        assert!(!hits[0].message.contains("high-impedance"));
        // Control: a header pin (passive) is a possible driver that floats unplugged.
        let found = enable_held_by_connector("Conn_01x02", "Connector:Conn_01x02", "passive");
        let hits = of(&found, LintCheck::FloatingGate);
        assert_eq!(hits.len(), 1, "{found:#?}");
        assert_eq!(hits[0].severity, Severity::Medium);
        assert!(hits[0].message.contains("high-impedance"));
    }

    #[test]
    fn a_pulled_or_stated_enable_is_clean() {
        let extra = [(10, "LDO_EN"), (22, "OUT")];
        let pulled = run(
            vec![ldo("RT9080-33", 10), mcu(10), resistor("R1", 10, GND)],
            &extra,
        );
        assert!(
            of(&pulled, LintCheck::FloatingGate).is_empty(),
            "{pulled:#?}"
        );
        // Cards that state what an open pin does, or an internal pull, say
        // what the reset level is.
        for value in ["MIC5205-3.3", "AP2112K-3.3"] {
            let found = run(vec![ldo(value, 10), mcu(10)], &extra);
            assert!(
                of(&found, LintCheck::FloatingGate).is_empty(),
                "{value}: {found:#?}"
            );
        }
        // Tied to a rail, or left to a net nothing drives, is not this check.
        let rail = run(vec![ldo("RT9080-33", V5), mcu(10)], &extra);
        assert!(of(&rail, LintCheck::FloatingGate).is_empty(), "{rail:#?}");
        let undriven = run(vec![ldo("RT9080-33", 10)], &extra);
        assert!(
            of(&undriven, LintCheck::FloatingGate).is_empty(),
            "{undriven:#?}"
        );
    }

    #[test]
    fn a_driver_enable_follows_the_cards_stated_pull() {
        // A MAX3237E states its input keeper: a floating EN or SHDN reads low.
        let xcvr = part(
            "U4",
            "MAX3237ECAI",
            "Interface:MAX3237E",
            &[("2", GND), ("13", 10), ("14", 10), ("26", V5)],
        );
        let found = run(vec![xcvr, mcu(10)], &[(10, "XCVR_EN")]);
        assert!(of(&found, LintCheck::FloatingGate).is_empty(), "{found:#?}");
        // A DRV8353S states its 100 kOhm ENABLE pull-down (SLVSDY6A p.12): an
        // open ENABLE reads low and the part sleeps, a defined state.
        let drv = part(
            "U4",
            "DRV8353SRTAR",
            "Driver_FET:DRV8353S",
            &[("31", 10), ("39", GND), ("3", V5)],
        );
        let found = run(vec![drv, mcu(10)], &[(10, "DRV_EN")]);
        assert!(of(&found, LintCheck::FloatingGate).is_empty(), "{found:#?}");
        // A driver whose card states no pull is a Low note.
        let haptic = part(
            "U4",
            "DRV2603RUNT",
            "Driver_Haptic:DRV2603RUNT",
            &[("1", 10), ("5", GND), ("7", V5)],
        );
        let found = run(vec![haptic, mcu(10)], &[(10, "HAPTIC_EN")]);
        let hits = of(&found, LintCheck::FloatingGate);
        assert_eq!(hits.len(), 1, "{found:#?}");
        assert_eq!(hits[0].severity, Severity::Low);
    }

    #[test]
    fn a_pulled_gate_or_select_is_clean() {
        let pulled = run(
            vec![nmos(10), mcu(10), resistor("R1", 10, GND)],
            &[(10, "GATE"), (21, "LOAD")],
        );
        assert!(of(&pulled, LintCheck::FloatingGate).is_empty());
        let pulled = run(
            vec![spdt(10, 11), mcu(11), resistor("R1", 11, V5)],
            &[(10, "COM"), (11, "SEL")],
        );
        assert!(of(&pulled, LintCheck::FloatingGate).is_empty());
    }
}
