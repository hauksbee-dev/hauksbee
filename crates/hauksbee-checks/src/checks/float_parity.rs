//! A floating-node detector driven by the DC solve, beside the hand checks.
//!
//! Seven hand checks read "nothing sets this net's level" from topology:
//! `floating_inputs`, `model_controls`, `node_drive` (a capacitor with no DC
//! path, a floating gate), `open_drain_pullup`, `pin_bias` (a pin with no
//! bias), `control_held_off` (an enable that floats or is pulled on while its
//! MCU is in reset) and `straps` (a boot level that is undefined). The solve
//! can say the same thing physically: a node nothing holds has no DC level,
//! so a tiny leakage decides where it rests, and two opposite leakages put it
//! in two different places.
//!
//! Each window is solved twice with a 100 Mohm leg on every net, first to
//! ground and then to the board's highest solved level (a Norton leg, so no
//! node is added). A disabled pin driver (1 Gohm) does not hold a node
//! against it, a 1 Mohm pull moves it about 1 %, and a node joined to a rail
//! only through a junction moves less than one diode drop. A node that moves
//! by more than [`MOVE_V`] and is not joined through links (resistors,
//! inductors, switches) to anything that holds it (ground, a source, a driven
//! pin, an op-amp output) is the root of an undefined cluster; a held node
//! that moves only follows a root upstream of it and is counted, not
//! reported. A root cluster that moves by more than [`RAIL_FRACTION`] of its
//! listeners' supply is reported at each listener on it (a MOSFET gate, a
//! card's logic input, control or must-not-float pin, an op-amp input, a
//! strap pad in reset), read against that listener's VIL/VIH (the card's
//! band at the solved supply, else 0.3/0.7 of it, marked assumed).
//!
//! Two windows are solved:
//!
//! - **Rest**: every bound MCU pad drives (firmware may drive any GPIO, so
//!   an MCU pad holds its net once firmware runs).
//! - **Reset window**: MCU pads take the card's `reset_mode`: high impedance,
//!   or the stated pull (the card's weak-pull row, else `io_pull_up_max_ohms`,
//!   else a 50 kOhm class value marked assumed) or drive. An MCU card with no
//!   `reset_mode` table is unknown there, so a cluster with its pads abstains.
//!
//! A cluster abstains, with the reason kept per net in
//! [`FloatShadow::why_not`], when its level may be set by something the solve
//! does not model: a connector pad (the level comes from off the board), or a
//! pin of a fitted part that can drive and has no device on the cluster. An
//! internal pull the card states for a listener (`logic.pulled_inputs`, a
//! reset pull-up, a strap pull, an enable's stated open state) defines the
//! cluster, except that an enable whose open state is on is reported as
//! switched on.
//!
//! [`float_ledger`] lays the findings beside the hand checks' in the same
//! (check, part) ledger as [`super::sim_parity::parity_ledger`]. The ones no
//! hand check made (the ledger's sim-only rows) also reach `check` and `run`:
//! [`float_promoted`] turns each into a Medium finding under the lint check
//! its target maps to, in plain words that name the reader, the pad, the
//! window and the pull to add. The hand checks all stay; where one already
//! reports the part on the net, the detector adds nothing.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;
use std::time::Instant;

use hauksbee_bind::bind_report::pin_can_drive;
use hauksbee_bind::binder::{bind_board, BoundBoard};
use hauksbee_bind::junctions::stamp_ic_junctions;
use hauksbee_extract::assembly::AssemblyState;
use hauksbee_extract::{
    is_connector_like, is_test_point_or_hole, Component, ExtractedBoard, LintCheck, LintFinding,
    Pin, Severity, REMEDY_MARKER,
};
use hauksbee_ir::evidence::{ModelSource, ModelUncertainty};
use hauksbee_ir::{BOutput, Circuit, Device, NodeId, SourceKind};
use hauksbee_models::netclass;
use hauksbee_models::{
    ComponentKind, ModelEntry, ModelLibrary, PeripheralSpec, ResetPadMode, StrapInternalPull,
};

use super::control_held_off::{control_of, Control, Kind};
use super::floating_inputs::is_cmos_logic_value;
use super::regulator_enable::OpenState;
use super::signal_direction::untyped_symbol;
use super::sim_parity::{settle, LedgerRow, Side, SimTarget, Solved};

/// The leakage leg's conductance (S): 100 Mohm.
const LEAK_S: f64 = 1e-8;
/// A node that moves by more than this between the two leakages has no
/// DC level of its own (V).
pub const MOVE_V: f64 = 0.15;
/// A root cluster is reported when it moves by more than this fraction of
/// its listeners' supply.
pub const RAIL_FRACTION: f64 = 0.25;
/// A link at or above this resistance is open for holding a node (ohms): a
/// disabled pin driver and the leakage legs themselves.
const OPEN_LINK_OHMS: f64 = 1e8;
/// The class weak-pull value used when an MCU card states none (ohms):
/// STM32 RPU 30-50 kOhm, RP2040 about 50 kOhm, ESP32 about 45 kOhm.
const CLASS_WEAK_PULL_OHMS: f64 = 50e3;
/// A pad the card says drives a level in reset (ohms).
const DRIVE_OHMS: f64 = 50.0;
/// A supply below this is off: its part's inputs are not judged (V).
const LIVE_V: f64 = 1.0;
/// Name prefix of the leakage legs.
const LEAK: &str = "~leak";
/// Name tag of a stamped reset-mode pull or drive (after the MCU reference).
const RESET_MODE: &str = "#rst";
/// Roles that name a part's logic supply, in order of preference.
///
/// A separate I/O supply comes first: it sets the levels a pad swings and a
/// pull goes to (an RP2040's `iovdd`), where the core supply (`dvdd`, 1.1 V)
/// sets neither.
const SUPPLY_ROLES: &[&str] = &[
    "vddio", "vdd_io", "iovdd", "vcc_io", "vio", "vdd", "vcc", "3v3", "dvdd", "avcc", "vin",
];
/// Roles that are open-drain outputs on most cards.
const OPEN_DRAIN_ROLES: &[&str] = &[
    "pg",
    "pgood",
    "power_good",
    "pg_n",
    "int",
    "int_n",
    "irq",
    "irq_n",
    "alert",
    "alert_n",
    "fault",
    "fault_n",
    "nfault",
    "flt",
    "flt_n",
];

/// Which solve a finding came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Window {
    /// Firmware running: every bound MCU pad drives.
    Rest,
    /// The MCUs held in reset: pads at their card's reset modes.
    Reset,
}

impl Window {
    /// The window's name in ledger rows.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Rest => "rest",
            Self::Reset => "reset window",
        }
    }

    const fn phrase(self) -> &'static str {
        match self {
            Self::Rest => "at rest (every MCU pad driven)",
            Self::Reset => "while the MCU is in reset (pads at their card's reset modes)",
        }
    }
}

/// One undefined listener the solve found.
#[derive(Clone, Debug)]
pub struct FloatFinding {
    pub target: SimTarget,
    pub part: String,
    pub pin: String,
    /// The root cluster's nets.
    pub nets: Vec<String>,
    pub window: Window,
    /// The listener's level with leakage to ground and to the rail (V).
    pub levels: (f64, f64),
    pub message: String,
    /// The MCU pads on the root cluster, as `U2 pad 14 (PB0)`.
    pub pads: Vec<String>,
    /// The finding as a user reads it: the reader, the pad, the window and
    /// the fix, ending in the [`REMEDY_MARKER`] sentence.
    pub plain: String,
}

/// What the detector found and where it could not speak.
#[derive(Clone, Debug, Default)]
pub struct FloatShadow {
    pub findings: Vec<FloatFinding>,
    /// Net -> why the solve said nothing about it (an abstention).
    pub why_not: BTreeMap<String, String>,
    /// Board-wide notes.
    pub notes: Vec<String>,
    /// The rest window did not solve or the board solves unpowered.
    pub unjudged: bool,
    /// DC solves run.
    pub solves: usize,
    /// Moved clusters that are held and only follow a root upstream.
    pub cascades: usize,
    /// Root clusters a listener's internal pull defines.
    pub pulled: usize,
    /// (window, net) -> its level with leakage to ground and to the rail (V).
    pub levels: BTreeMap<(Window, String), (f64, f64)>,
}

/// The board, its fitted parts and their cards.
struct Ctx<'a> {
    board: &'a ExtractedBoard,
    lib: &'a ModelLibrary,
    fitted: Vec<&'a Component>,
    models: HashMap<&'a str, ModelEntry>,
}

impl<'a> Ctx<'a> {
    fn new(board: &'a ExtractedBoard, lib: &'a ModelLibrary) -> Self {
        let fitted = (board.components.iter())
            .filter(|c| AssemblyState::of(c).fitted().is_some())
            .collect();
        let models = super::resolved_components(board, lib)
            .map(|(c, m)| (c.reference.as_str(), m))
            .collect();
        Self {
            board,
            lib,
            fitted,
            models,
        }
    }

    fn net_name(&self, pin: &Pin) -> Option<&'a str> {
        Some(self.board.net(pin.net?)?.name.as_str())
    }

    fn node(&self, bound: &BoundBoard, pin: &Pin) -> Option<NodeId> {
        let n = *bound.net_nodes.get(self.net_name(pin)?)?;
        (!n.is_ground()).then_some(n)
    }

    /// The node of `comp`'s logic supply: the card's level rail for `role`,
    /// else its first supply role, else a pin on a positive supply net.
    fn supply(
        &self,
        bound: &BoundBoard,
        comp: &Component,
        model: Option<&ModelEntry>,
        role: Option<&str>,
    ) -> Option<NodeId> {
        if let Some(m) = model {
            let rail = role.and_then(|r| m.level_rail_of(r));
            for want in rail.into_iter().chain(SUPPLY_ROLES.iter().copied()) {
                let pin = comp.pins.iter().find(|p| {
                    m.pins
                        .get(&p.number)
                        .is_some_and(|r| r.eq_ignore_ascii_case(want))
                });
                if let Some(n) = pin.and_then(|p| self.node(bound, p)) {
                    return Some(n);
                }
            }
        }
        comp.pins
            .iter()
            .filter(|p| self.net_name(p).is_some_and(netclass::is_positive_supply))
            .find_map(|p| self.node(bound, p))
    }
}

/// The weak pull of an MCU card's pads (ohms), and whether it is assumed.
///
/// The card's `rpu_weak_pull_up_resistor` / `rpd_weak_pull_down_resistor`
/// row comes first: the loader keeps a printed min-max row as an interval in
/// the card's provenance, so its midpoint stands in for the typical value.
/// The loader drops a row whose bounds repeat an earlier one's, so a
/// pull-down printed with the same range as the pull-up is found only as the
/// pull-up row; that value is used for the pull-down and marked assumed.
/// Then `io_pull_up_max_ohms` (assumed for a pull-down), then the class value.
fn weak_pull(model: &ModelEntry, source: Option<&ModelSource>, up: bool) -> (f64, bool) {
    let row = |name: &str| {
        (source.iter().flat_map(|s| s.uncertainty())).find_map(|u| match u {
            ModelUncertainty::Interval {
                parameter,
                low,
                high,
                ..
            } if parameter == name => Some(f64::midpoint(*low, *high)),
            _ => None,
        })
    };
    let up_row = row("rpu_weak_pull_up_resistor");
    let stated = if up {
        up_row
    } else {
        row("rpd_weak_pull_down_resistor")
    };
    let positive = |v: &f64| v.is_finite() && *v > 0.0;
    stated
        .filter(positive)
        .map(|v| (v, false))
        .or_else(|| {
            let param = model.params.get_f64("io_pull_up_max_ohms");
            up_row.or(param).filter(positive).map(|v| (v, !up))
        })
        .unwrap_or((CLASS_WEAK_PULL_OHMS, true))
}

/// A resistor `ohms` from `node` to a fixed `volts`, as a Norton leg to
/// ground (no node added). Returns the current source's device index.
fn norton(circuit: &mut Circuit, name: &str, node: NodeId, ohms: f64, volts: f64) -> usize {
    circuit.devices.push(Device::Resistor {
        name: format!("{name}.r"),
        a: node,
        b: NodeId::GROUND,
        ohms,
        tc1: None,
    });
    circuit.devices.push(Device::Isource {
        name: format!("{name}.i"),
        p: NodeId::GROUND,
        n: node,
        kind: SourceKind::Dc(volts / ohms),
    });
    circuit.devices.len() - 1
}

/// Whether device `name` belongs to part `reference` (the binder names a
/// part's devices after it; an MCU pad driver carries a `Rdrv_`/`Vdrv_`
/// prefix).
fn of_part(name: &str, reference: &str) -> bool {
    let name = (name.strip_prefix("Rdrv_"))
        .or_else(|| name.strip_prefix("Vdrv_"))
        .unwrap_or(name);
    name.strip_prefix(reference)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with([':', '.', '_', '#']))
}

/// Stamp each MCU pad's reset mode from its card: pulls and drives as
/// resistors to the MCU's solved supply or ground.
fn stamp_reset_modes(ctx: &Ctx<'_>, bound: &mut BoundBoard, bare: &Solved, out: &mut FloatShadow) {
    for comp in &ctx.fitted {
        let Some(model) = ctx.models.get(comp.reference.as_str()) else {
            continue;
        };
        if model.kind != ComponentKind::Mcu || model.pin_capability.reset_mode.is_none() {
            continue;
        }
        let level = ctx
            .supply(bound, comp, Some(model), None)
            .map(|n| bare.v(n))
            .unwrap_or_default();
        if level < LIVE_V {
            out.notes.push(format!(
                "{}: supply solves dead, its reset-mode pulls are not stamped",
                comp.reference
            ));
            continue;
        }
        let source = (AssemblyState::of(comp).fitted())
            .and_then(|part| hauksbee_bind::binder::resolve(ctx.lib, part).provenance);
        for pin in &comp.pins {
            let Some(node) = ctx.node(bound, pin) else {
                continue;
            };
            let (ohms, volts) = match model.reset_pad_mode(&pin.number) {
                ResetPadMode::PullUp => (weak_pull(model, source.as_ref(), true).0, level),
                ResetPadMode::PullDown => (weak_pull(model, source.as_ref(), false).0, 0.0),
                ResetPadMode::DrivesHigh => (DRIVE_OHMS, level),
                ResetPadMode::DrivesLow => (DRIVE_OHMS, 0.0),
                ResetPadMode::HiZ => continue,
            };
            let name = format!("{}{RESET_MODE}{}", comp.reference, pin.number);
            norton(&mut bound.circuit, &name, node, ohms, volts);
        }
    }
}

/// Solve one window; its findings, abstentions and levels go to `out`.
fn run_window(ctx: &Ctx<'_>, window: Window, out: &mut FloatShadow) {
    let mut bound = bind_board(ctx.board, ctx.lib);
    let fail = |out: &mut FloatShadow, what: &str| {
        out.notes
            .push(format!("{}: {what}: nothing judged", window.name()));
        if window == Window::Rest {
            out.unjudged = true;
        }
    };
    if bound.supplies.is_empty() && bound.report.regulators.is_empty() {
        fail(
            out,
            "no supply net names a voltage, the board solves unpowered",
        );
        return;
    }
    if window == Window::Rest {
        let BoundBoard { circuit, mcus, .. } = &mut bound;
        for d in mcus.iter_mut().flat_map(|m| m.gpio_drivers.values_mut()) {
            d.set_enabled(circuit, true);
        }
    }
    let Some(bare) = settle(&mut bound, None, &mut out.solves) else {
        fail(out, "the state did not solve");
        return;
    };
    let vhigh = (bound.net_nodes.values())
        .map(|&n| bare.v(n))
        .fold(LIVE_V, f64::max);
    if window == Window::Reset {
        stamp_reset_modes(ctx, &mut bound, &bare, out);
    }
    stamp_ic_junctions(&mut bound, &|n| bare.v(n));
    let nodes: BTreeSet<NodeId> = (bound.net_nodes.values().copied())
        .filter(|n| !n.is_ground())
        .collect();
    let legs: Vec<usize> = (nodes.iter().enumerate())
        .map(|(i, &n)| {
            norton(
                &mut bound.circuit,
                &format!("{LEAK}{i}"),
                n,
                1.0 / LEAK_S,
                0.0,
            )
        })
        .collect();
    let Some(lo) = settle(&mut bound, Some(&bare.x), &mut out.solves) else {
        fail(out, "the state with leakage to ground did not solve");
        return;
    };
    for &i in &legs {
        if let Some(Device::Isource { kind, .. }) = bound.circuit.devices.get_mut(i) {
            *kind = SourceKind::Dc(vhigh * LEAK_S);
        }
    }
    let Some(hi) = settle(&mut bound, Some(&lo.x), &mut out.solves) else {
        fail(out, "the state with leakage to the rail did not solve");
        return;
    };
    for (net, &n) in &bound.net_nodes {
        out.levels.insert((window, net.clone()), (lo.v(n), hi.v(n)));
    }
    judge(ctx, &bound, window, (&lo, &hi), vhigh, out);
}

/// The nodes something holds at DC, and each node's links (resistors,
/// inductors and switches below [`OPEN_LINK_OHMS`]).
fn holders(circuit: &Circuit) -> (HashSet<NodeId>, HashMap<NodeId, Vec<NodeId>>) {
    let mut held = HashSet::new();
    let mut links: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
    let mut link = |a: NodeId, b: NodeId| {
        links.entry(a).or_default().push(b);
        links.entry(b).or_default().push(a);
    };
    for dev in &circuit.devices {
        match dev {
            Device::Resistor {
                name, a, b, ohms, ..
            } if !name.starts_with(LEAK)
                && !name.contains(RESET_MODE)
                && *ohms < OPEN_LINK_OHMS =>
            {
                link(*a, *b);
            }
            Device::Inductor { a, b, .. } | Device::VSwitch { a, b, .. } => link(*a, *b),
            Device::Vsource { p, n, .. }
            | Device::Vcvs { p, n, .. }
            | Device::Behavioral {
                p,
                n,
                output: BOutput::Voltage,
                ..
            } => {
                held.extend([*p, *n]);
            }
            Device::OpAmp { out, .. } | Device::Comparator { out, .. } => {
                held.insert(*out);
            }
            _ => {}
        }
    }
    (held, links)
}

/// How a pin listens to its net.
enum Listen {
    /// A card's logic input, or a schematic input on a CMOS part.
    Input,
    /// An enable or reset.
    Control(Control),
    /// A pin the card says must not float.
    MustNotFloat,
    /// An op-amp or comparator input.
    Analog,
    /// An MCU's boot strap, read in the reset window.
    Strap,
}

/// One listener on a root cluster.
struct Listener<'a> {
    comp: &'a Component,
    pin: &'a Pin,
    role: String,
    listen: Listen,
    model: Option<&'a ModelEntry>,
    node: NodeId,
}

/// How `pin` of `comp` listens to its net in `window`, if it does.
fn listen(
    comp: &Component,
    model: Option<&ModelEntry>,
    pin: &Pin,
    window: Window,
) -> Option<Listen> {
    let Some(m) = model else {
        let input = matches!(
            pin.kind.trim().to_ascii_lowercase().as_str(),
            "input" | "in"
        );
        return (input && is_cmos_logic_value(&comp.value)).then_some(Listen::Input);
    };
    let role = m.pins.get(&pin.number)?.to_ascii_lowercase();
    let control = control_of(pin, Some(m));
    if m.kind == ComponentKind::Mcu {
        if window == Window::Reset && m.straps.iter().any(|s| s.role.eq_ignore_ascii_case(&role)) {
            return Some(Listen::Strap);
        }
        return control
            .filter(|c| c.kind == Kind::Reset)
            .map(Listen::Control);
    }
    if let Some(c) = control {
        return Some(Listen::Control(c));
    }
    let must_not_float = (m.params.get_str("must_not_float_roles").unwrap_or_default())
        .split(',')
        .any(|r| r.trim().eq_ignore_ascii_case(&role));
    if must_not_float {
        return Some(Listen::MustNotFloat);
    }
    if super::logic_levels::input_roles(m)
        .iter()
        .any(|r| r.eq_ignore_ascii_case(&role))
    {
        return Some(Listen::Input);
    }
    let analog = matches!(m.kind, ComponentKind::Opamp | ComponentKind::Comparator)
        && matches!(
            role.as_str(),
            "in_plus" | "in_minus" | "inp" | "inn" | "in+" | "in-"
        );
    analog.then_some(Listen::Analog)
}

/// The internal pull that defines a listener's net, from its card.
fn defining_pull(l: &Listener<'_>) -> Option<String> {
    let m = l.model?;
    if m.logic
        .pulled_inputs
        .iter()
        .any(|p| p.eq_ignore_ascii_case(&l.role))
    {
        return Some("its card lists an internal pull (logic.pulled_inputs)".to_string());
    }
    match &l.listen {
        Listen::Control(c) if c.kind == Kind::Reset => m
            .params
            .get_f64("reset_pull_up_ohms")
            .filter(|_| c.run_high)
            .map(|o| format!("its card states a {o:.0} Ohm reset pull-up")),
        Listen::Control(c) => match OpenState::of(m, c.run_high) {
            OpenState::Off => Some("its card says an open pin holds the part off".to_string()),
            _ => None,
        },
        Listen::Strap => {
            let strap = m
                .straps
                .iter()
                .find(|s| s.role.eq_ignore_ascii_case(&l.role))?;
            match strap.internal_pull {
                StrapInternalPull::None => None,
                StrapInternalPull::PullUp => Some("its strap has an internal pull-up".to_string()),
                StrapInternalPull::PullDown => {
                    Some("its strap has an internal pull-down".to_string())
                }
                StrapInternalPull::Unknown => {
                    Some("its card does not say whether the strap is pulled".to_string())
                }
            }
        }
        _ => None,
    }
}

/// The enable's open state when the card says an open pin turns it on.
fn open_on(l: &Listener<'_>) -> Option<String> {
    let (Listen::Control(c), Some(m)) = (&l.listen, l.model) else {
        return None;
    };
    if c.kind != Kind::Enable {
        return None;
    }
    match OpenState::of(m, c.run_high) {
        OpenState::On(why) => Some(why),
        _ => None,
    }
}

/// Whether `pin` of `comp` is an open-drain output by its card or symbol.
fn open_drain(model: Option<&ModelEntry>, pin: &Pin) -> bool {
    let kind = pin.kind.trim().to_ascii_lowercase();
    if kind.starts_with("open_collector") || kind.starts_with("open_emitter") {
        return true;
    }
    model.is_some_and(|m| {
        let listed = (m.pin_capability.open_drain.as_ref())
            .is_some_and(|t| t.pads.contains_key(&pin.number));
        let role = m.pins.get(&pin.number).map(|r| r.to_ascii_lowercase());
        listed || role.is_some_and(|r| OPEN_DRAIN_ROLES.contains(&r.as_str()))
    })
}

/// Active-low chip-select roles a card may name.
const CHIP_SELECT_ROLES: &[&str] = &["cs", "cs_n", "csn", "ncs", "csb", "ss", "ss_n", "nss"];

/// The chip-select role of a part's card: its SPI peripheral's `cs_role`,
/// else a pin role spelled as an active-low chip select.
fn chip_select_role(m: &ModelEntry) -> Option<String> {
    if let Some(
        PeripheralSpec::SpiNorFlash { cs_role, .. } | PeripheralSpec::RegisterMap { cs_role, .. },
    ) = &m.peripheral
    {
        return Some(cs_role.to_ascii_lowercase());
    }
    (m.pins.values())
        .map(|r| r.to_ascii_lowercase())
        .find(|r| CHIP_SELECT_ROLES.contains(&r.as_str()))
}

/// Why a SPI part ignores its other inputs in the reset window, if it does.
///
/// Its active-low chip select reads high in both leakage solves, so the part
/// is deselected: a deselected SPI device ignores CLK, DI and /HOLD (Winbond
/// W25Q32JV Rev G section 4.4 p.9, "/HOLD ... while /CS is low").
fn deselected(
    ctx: &Ctx<'_>,
    bound: &BoundBoard,
    l: &Listener<'_>,
    (lo, hi): (&Solved, &Solved),
) -> Option<String> {
    let m = l.model?;
    let cs = chip_select_role(m)?;
    if l.role == cs || matches!(l.listen, Listen::Control(_)) {
        return None;
    }
    let pin = (l.comp.pins.iter()).find(|p| {
        m.pins
            .get(&p.number)
            .is_some_and(|r| r.eq_ignore_ascii_case(&cs))
    })?;
    let node = ctx.node(bound, pin)?;
    let vcc = ctx
        .supply(bound, l.comp, Some(m), Some(&cs))
        .map(|n| lo.v(n))?;
    if vcc < LIVE_V {
        return None;
    }
    let (_, vih) = m.input_band_at(vcc).unwrap_or((0.3 * vcc, 0.7 * vcc));
    let (a, b) = (lo.v(node), hi.v(node));
    (a > vih && b > vih).then(|| {
        format!(
            "reset: {} pin {} ({}) is not read: its chip select (pin {}) sits at {:.2} V, above VIH {vih:.2} V, so the part is deselected",
            l.comp.reference,
            l.pin.number,
            l.role,
            pin.number,
            a.min(b)
        )
    })
}

/// A cluster's nets, in name order.
fn cluster_nets(cluster: &BTreeSet<NodeId>, names: &HashMap<NodeId, Vec<&str>>) -> Vec<String> {
    let mut nets: Vec<String> = (cluster.iter())
        .flat_map(|n| names.get(n).into_iter().flatten())
        .map(|s| (*s).to_string())
        .collect();
    nets.sort();
    nets.dedup();
    nets
}

/// The listener as a reader names it: `U1 (74HC14) pin 1 (1a)`, or
/// `U3 (TPS22918) enable pin 3` for a control.
fn reader_of(l: &Listener<'_>) -> String {
    let value = l.comp.value.trim();
    let part = if value.is_empty() {
        l.comp.reference.clone()
    } else {
        format!("{} ({value})", l.comp.reference)
    };
    match &l.listen {
        Listen::Control(c) => format!("{part} {} pin {}", c.label, l.pin.number),
        _ => format!("{part} pin {} ({})", l.pin.number, l.role),
    }
}

/// The pull a listener on an undefined net needs, in words ending with a stop.
///
/// An enable rests off (a pull-down when it runs high, else a pull-up to its
/// supply). Any other input rests at the level it should read, which only the
/// design knows, so the advice names both rails.
fn pull_fix(l: &Listener<'_>, net: &str, supply: &str, until_driven: bool) -> String {
    let reader = reader_of(l);
    let tail = if until_driven {
        " until firmware drives it"
    } else {
        ""
    };
    match &l.listen {
        Listen::Control(c) if c.kind == Kind::Enable && c.run_high => format!(
            "Add a {PULL_ADVICE} pull-down from '{net}' to ground so {reader} rests off{tail}."
        ),
        Listen::Control(c) if c.kind == Kind::Enable => format!(
            "Add a {PULL_ADVICE} pull-up from '{net}' to {supply} so {reader} rests off{tail}."
        ),
        _ => format!(
            "Add a {PULL_ADVICE} pull resistor from '{net}' to ground or to {supply}, whichever level {reader} should read{tail}, so the net has a level."
        ),
    }
}

/// The enables a cluster held only by MCU reset-mode pulls or drives sets to
/// their on level: the part is switched on while the MCU is in reset.
fn pulled_on_in_reset(
    listeners: &[Listener<'_>],
    supplies: &[(Option<f64>, String)],
    (nets, mcu_pads, pad_labels): (&[String], &[String], &[String]),
    (lo, vhigh): (&Solved, f64),
    out: &mut FloatShadow,
) {
    for (l, (vcc, supply)) in listeners.iter().zip(supplies) {
        let Listen::Control(c) = &l.listen else {
            continue;
        };
        if c.kind != Kind::Enable || l.model.is_some_and(|m| m.kind == ComponentKind::Mcu) {
            continue;
        }
        let vcc = vcc.unwrap_or(vhigh);
        if vcc < LIVE_V {
            continue;
        }
        let (vil, vih) =
            (l.model.and_then(|m| m.input_band_at(vcc))).unwrap_or((0.3 * vcc, 0.7 * vcc));
        let v = lo.v(l.node);
        let on = if c.run_high { v > vih } else { v < vil };
        if !on {
            continue;
        }
        let net = nets.first().map_or("", String::as_str);
        let reader = reader_of(l);
        let (way, level) = if c.run_high {
            ("up", "high")
        } else {
            ("down", "low")
        };
        let plain = format!(
            "{reader} (net '{net}') is driven only by {}. While the MCU is in reset its card says the pad is pulled or driven {way} ({level}), which is the pin's on level, and nothing else on the net sets it: the part is switched on while the MCU is in reset. {REMEDY_MARKER}Add a pull from '{net}' to {} stiff enough to beat the pad's internal pull (10 kOhm against a 30-50 kOhm pull), or move the pin to a pad that is high-impedance or pulled the other way in reset.",
            pad_labels.join(", "),
            if c.run_high { "ground" } else { supply.as_str() },
        );
        out.findings.push(FloatFinding {
            target: SimTarget::ControlHeldOff,
            part: l.comp.reference.clone(),
            pin: l.pin.number.clone(),
            nets: nets.to_vec(),
            window: Window::Reset,
            levels: (v, v),
            pads: pad_labels.to_vec(),
            plain,
            message: format!(
                "{} pin {} ({}) on net '{}' sits at {v:.2} V while the MCU is in reset: only MCU pads ({}) are on it and their card's reset modes pull or drive it there, which is the pin's on level, so the part is switched on while the MCU is in reset.",
                l.comp.reference,
                l.pin.number,
                l.role,
                nets.first().map_or("", String::as_str),
                mcu_pads.join(", ")
            ),
        });
    }
}

/// Read the solved pair and report each undefined listener on every root
/// cluster.
#[allow(clippy::too_many_lines)] // one pass over the clusters: splitting it scatters the state
fn judge(
    ctx: &Ctx<'_>,
    bound: &BoundBoard,
    window: Window,
    (lo, hi): (&Solved, &Solved),
    vhigh: f64,
    out: &mut FloatShadow,
) {
    let mut names: HashMap<NodeId, Vec<&str>> = HashMap::new();
    for (net, &n) in &bound.net_nodes {
        names.entry(n).or_default().push(net.as_str());
    }
    let swing = |n: NodeId| (hi.v(n) - lo.v(n)).abs();
    let moved: BTreeSet<NodeId> = (names.keys().copied())
        .filter(|n| !n.is_ground() && swing(*n) > MOVE_V)
        .collect();
    let (held, links) = holders(&bound.circuit);
    // In the reset window a net an MCU pad pulls or drives is judged too: it
    // does not move, but its level is what an enable on it sees.
    let pulled_nodes: BTreeSet<NodeId> = (bound.circuit.devices.iter())
        .filter(|d| window == Window::Reset && d.name().contains(RESET_MODE))
        .flat_map(Device::nodes)
        .filter(|n| names.contains_key(n) && !n.is_ground())
        .collect();
    let mut seen: HashSet<NodeId> = HashSet::new();
    for &start in moved.union(&pulled_nodes) {
        if !seen.insert(start) {
            continue;
        }
        let mut cluster = BTreeSet::from([start]);
        let mut todo = vec![start];
        let mut is_held = false;
        while let Some(n) = todo.pop() {
            is_held |= n.is_ground() || held.contains(&n);
            for &m in links.get(&n).into_iter().flatten() {
                if cluster.insert(m) {
                    seen.insert(m);
                    todo.push(m);
                }
            }
        }
        if is_held {
            out.cascades += usize::from(moved.contains(&start));
            continue;
        }
        let nets = cluster_nets(&cluster, &names);
        let mut abstain: Vec<String> = Vec::new();
        let mut listeners: Vec<Listener<'_>> = Vec::new();
        let mut mcu_pads: Vec<String> = Vec::new();
        let mut pad_labels: Vec<String> = Vec::new();
        let mut od_driver = false;
        let on_cluster: Vec<&str> = (bound.circuit.devices.iter())
            .filter(|d| !d.name().starts_with(LEAK))
            .filter(|d| d.nodes().iter().any(|n| cluster.contains(n)))
            .map(Device::name)
            .collect();
        for &comp in &ctx.fitted {
            let model = ctx.models.get(comp.reference.as_str());
            for pin in &comp.pins {
                let Some(node) = ctx.node(bound, pin).filter(|n| cluster.contains(n)) else {
                    continue;
                };
                if is_test_point_or_hole(comp) {
                    continue;
                }
                if is_connector_like(comp) {
                    abstain.push(format!(
                        "{} pin {} is a connector pad: its level is set off the board",
                        comp.reference, pin.number
                    ));
                    continue;
                }
                let is_mcu = model.is_some_and(|m| m.kind == ComponentKind::Mcu);
                if is_mcu {
                    mcu_pads.push(format!("{}.{}", comp.reference, pin.number));
                    pad_labels.push(pad_label(comp, pin));
                }
                if let Some(listen) = listen(comp, model, pin, window) {
                    let role = (model.and_then(|m| m.pins.get(&pin.number)))
                        .map_or_else(|| pin.function.clone(), |r| r.to_ascii_lowercase());
                    listeners.push(Listener {
                        comp,
                        pin,
                        role,
                        listen,
                        model,
                        node,
                    });
                    continue;
                }
                if open_drain(model, pin) {
                    od_driver = true;
                    continue;
                }
                if window == Window::Reset && is_mcu {
                    if model.is_some_and(|m| m.pin_capability.reset_mode.is_none()) {
                        abstain.push(format!(
                            "{}'s card has no reset_mode table: its pad {} is unknown in reset",
                            comp.reference, pin.number
                        ));
                    } else if model.is_some_and(|m| {
                        // An unlisted pad is high impedance only on a card
                        // that knows the pad: a partial pin map says nothing.
                        !m.pins.contains_key(&pin.number)
                            && (m.pin_capability.reset_mode.as_ref())
                                .is_some_and(|t| !t.pads.contains_key(&pin.number))
                    }) {
                        abstain.push(format!(
                            "{}'s card neither maps pad {} nor lists it in reset_mode: its reset state is unknown",
                            comp.reference, pin.number
                        ));
                    }
                    continue;
                }
                // A library that types every pin of a many-pin part `input`
                // never said which pads drive, so such a pad may.
                let modelled = on_cluster.iter().any(|d| of_part(d, &comp.reference));
                let may_drive = pin_can_drive(&pin.kind) || untyped_symbol(comp);
                if may_drive && !modelled {
                    abstain.push(format!(
                        "{} pin {} may drive the net and has no model there",
                        comp.reference, pin.number
                    ));
                }
            }
        }
        if !abstain.is_empty() {
            let why = format!("{}: {}", window.name(), abstain.join("; "));
            for net in &nets {
                out.why_not
                    .entry(net.clone())
                    .or_insert_with(|| why.clone());
            }
            continue;
        }
        if window == Window::Reset {
            listeners.retain(|l| {
                let why = deselected(ctx, bound, l, (lo, hi));
                if let Some(why) = &why {
                    out.notes.push(why.clone());
                }
                why.is_none()
            });
        }
        let gates: Vec<(&str, NodeId, NodeId, f64, bool)> = (bound.circuit.devices.iter())
            .filter_map(|d| match d {
                Device::Mosfet {
                    name, g, s, model, ..
                } if cluster.contains(g) => Some((
                    name.as_str(),
                    *g,
                    *s,
                    model.vto,
                    model.polarity == hauksbee_ir::Polarity::P,
                )),
                _ => None,
            })
            .collect();
        if listeners.is_empty() && gates.is_empty() {
            continue;
        }
        let supplies: Vec<(Option<f64>, String)> = (listeners.iter())
            .map(|l| {
                let node = ctx.supply(bound, l.comp, l.model, Some(&l.role));
                let name = (node.and_then(|n| names.get(&n)))
                    .and_then(|v| v.first())
                    .map_or_else(|| "its supply".to_string(), |s| format!("'{s}'"));
                (node.map(|n| lo.v(n)), name)
            })
            .collect();
        let rail = (supplies.iter().filter_map(|(v, _)| *v))
            .filter(|v| *v >= LIVE_V)
            .reduce(f64::min)
            .unwrap_or(vhigh);
        let cluster_swing = cluster.iter().map(|&n| swing(n)).fold(0.0, f64::max);
        if cluster_swing <= RAIL_FRACTION * rail {
            if on_cluster.iter().any(|d| d.contains(RESET_MODE)) {
                pulled_on_in_reset(
                    &listeners,
                    &supplies,
                    (&nets, &mcu_pads, &pad_labels),
                    (lo, vhigh),
                    out,
                );
            }
            continue;
        }
        let pulls: Vec<String> = (listeners.iter())
            .filter_map(|l| {
                defining_pull(l).map(|p| format!("{} pin {}: {p}", l.comp.reference, l.pin.number))
            })
            .collect();
        if !pulls.is_empty() {
            out.pulled += 1;
        }
        let first_net = nets.first().cloned().unwrap_or_default();
        let held_by = if mcu_pads.is_empty() {
            "nothing on the net holds it".to_string()
        } else {
            format!("only MCU pads ({}) are on it", mcu_pads.join(", "))
        };
        // The net as the reader's own pin sees it, for the plain text.
        let net_at = |node: NodeId| -> String {
            (names.get(&node).and_then(|v| v.first()))
                .map_or_else(|| first_net.clone(), |n| (*n).to_string())
        };
        let mut push =
            |target: SimTarget,
             part: &str,
             pin: &str,
             levels: (f64, f64),
             message: String,
             (reader, net, effect, fix): (&str, &str, &str, &str)| {
                let plain = plain_message(&Plain {
                    reader,
                    net,
                    pads: &pad_labels,
                    window,
                    effect,
                    fix,
                });
                out.findings.push(FloatFinding {
                    target,
                    part: part.to_string(),
                    pin: pin.to_string(),
                    nets: nets.clone(),
                    window,
                    levels,
                    message,
                    pads: pad_labels.clone(),
                    plain,
                });
            };
        for (l, (vcc, supply)) in listeners.iter().zip(&supplies) {
            let reader = reader_of(l);
            let net_name = net_at(l.node);
            let fix = pull_fix(l, &net_name, supply, !pad_labels.is_empty());
            let levels = (lo.v(l.node), hi.v(l.node));
            let target = match &l.listen {
                Listen::Strap => SimTarget::Straps,
                Listen::Control(_) if l.model.is_some_and(|m| m.kind == ComponentKind::Mcu) => {
                    SimTarget::ModelControls
                }
                Listen::Control(_) if window == Window::Reset && !mcu_pads.is_empty() => {
                    SimTarget::ControlHeldOff
                }
                Listen::Control(_) | Listen::MustNotFloat => SimTarget::ModelControls,
                Listen::Input | Listen::Analog if od_driver => SimTarget::OpenDrainPullup,
                Listen::Input | Listen::Analog => SimTarget::FloatingInputs,
            };
            if let Some(why) = open_on(l) {
                push(
                    target,
                    &l.comp.reference,
                    &l.pin.number,
                    levels,
                    format!(
                        "{} pin {} ({}) on net '{first_net}' has no level {}: {held_by}, and its card says an open pin turns the part on ({why}): the part is switched on there.",
                        l.comp.reference,
                        l.pin.number,
                        l.role,
                        window.phrase()
                    ),
                    (
                        &reader,
                        &net_name,
                        &format!("its card says an open pin turns the part on ({why}), so the part is switched on"),
                        &fix,
                    ),
                );
                continue;
            }
            if !pulls.is_empty() {
                continue;
            }
            let Some(vcc) = vcc.or(Some(vhigh)).filter(|v| *v >= LIVE_V) else {
                continue;
            };
            let (band, assumed) = l
                .model
                .and_then(|m| m.input_band_at(vcc))
                .map_or(((0.3 * vcc, 0.7 * vcc), true), |b| (b, false));
            let read = |v: f64| {
                if v < band.0 {
                    0
                } else if v > band.1 {
                    2
                } else {
                    1
                }
            };
            let (a, b) = (read(levels.0), read(levels.1));
            if a == b && a != 1 {
                continue;
            }
            let mut message = format!(
                "{} pin {} ({}) on net '{first_net}' has no DC level {}: {held_by}, and it solves at {:.2} V with 100 MOhm leakage to ground and {:.2} V with leakage to the {vhigh:.1} V rail, against its input band {:.2}-{:.2} V",
                l.comp.reference,
                l.pin.number,
                l.role,
                window.phrase(),
                levels.0,
                levels.1,
                band.0,
                band.1,
            );
            if assumed {
                message.push_str(" (0.3/0.7 of its supply, assumed: the card states no band)");
            }
            message.push('.');
            let effect = format!(
                "{}it solves at {:.2} V with 100 MOhm leakage to ground and {:.2} V with leakage to the {vhigh:.1} V rail, against its input band {:.2}-{:.2} V{}, so the part reads it as undefined or as either level",
                if od_driver {
                    "the only driver on it is open-drain and cannot pull it high, so "
                } else {
                    ""
                },
                levels.0,
                levels.1,
                band.0,
                band.1,
                if assumed { " (0.3/0.7 of its supply, assumed: the card states no band)" } else { "" },
            );
            push(
                target,
                &l.comp.reference,
                &l.pin.number,
                levels,
                message,
                (&reader, &net_name, &effect, &fix),
            );
        }
        if !pulls.is_empty() {
            continue;
        }
        for (name, g, s, vto, pmos) in gates {
            let on = |st: &Solved| {
                let vgs = st.v(g) - st.v(s);
                (if pmos { -vgs } else { vgs }) > vto
            };
            if on(lo) == on(hi) {
                continue;
            }
            let reference = name.split(['.', ':', '#']).next().unwrap_or(name);
            let value = (ctx.fitted.iter())
                .find(|c| c.reference == reference)
                .map_or("", |c| c.value.trim());
            let reader = if value.is_empty() {
                format!("{reference} gate")
            } else {
                format!("{reference} ({value}) gate")
            };
            let gate_net = net_at(g);
            let state = |on: bool| if on { "on" } else { "off" };
            let (at_lo, at_hi) = (on(lo), on(hi));
            let effect = format!(
                "the FET is {} with 100 MOhm leakage to ground ({:.2} V at the gate) and {} with leakage to the {vhigh:.1} V rail ({:.2} V), so it conducts or not on leakage alone",
                state(at_lo),
                lo.v(g),
                state(at_hi),
                hi.v(g),
            );
            let rests = if pmos {
                let source = (names.get(&s).and_then(|v| v.first()))
                    .map_or_else(|| "its source".to_string(), |n| format!("'{n}'"));
                format!("pull-up from '{gate_net}' to {source}")
            } else {
                format!("pull-down from '{gate_net}' to ground")
            };
            let tail = if pad_labels.is_empty() {
                ""
            } else {
                " until firmware drives it"
            };
            let fix = format!("Add a {PULL_ADVICE} {rests} so {reader} rests off{tail}.");
            push(
                SimTarget::NodeDrive,
                reference,
                "G",
                (lo.v(g), hi.v(g)),
                format!(
                    "{reference} gate on net '{first_net}' has no DC level {}: {held_by}, so the FET is off with leakage to ground and on with leakage to the {vhigh:.1} V rail.",
                    window.phrase()
                ),
                (&reader, &gate_net, &effect, &fix),
            );
        }
    }
}

/// The pull resistor a promoted finding asks for.
const PULL_ADVICE: &str = "100k";

/// A pad as a reader finds it on the schematic: `U2 pad 14 (PB0)`.
fn pad_label(comp: &Component, pin: &Pin) -> String {
    let function = pin.function.trim();
    if function.is_empty() || function == pin.number {
        format!("{} pad {}", comp.reference, pin.number)
    } else {
        format!("{} pad {} ({function})", comp.reference, pin.number)
    }
}

/// What a plain finding says, before it is put into words.
struct Plain<'a> {
    /// The part and pin that reads the net.
    reader: &'a str,
    net: &'a str,
    pads: &'a [String],
    window: Window,
    /// What the missing level does to the reader, ending without a stop.
    effect: &'a str,
    /// The fix, ending with a stop.
    fix: &'a str,
}

/// The finding in the words a user is shown: who reads the net, which pad
/// should drive it, which window it fails in and what to add.
fn plain_message(p: &Plain<'_>) -> String {
    let Plain {
        reader,
        net,
        pads,
        window,
        effect,
        fix,
    } = p;
    let feed = if pads.is_empty() {
        format!("{reader} sits on net '{net}', which nothing drives or pulls.")
    } else {
        format!(
            "{reader} (net '{net}') is driven only by {}.",
            pads.join(", ")
        )
    };
    let when = match (window, pads.len()) {
        (Window::Reset, 0) => "While the MCU is in reset nothing holds the net".to_string(),
        (Window::Reset, 1) => "While the MCU is in reset its pad is high-impedance and nothing else on the net holds it".to_string(),
        (Window::Reset, _) => "While the MCU is in reset its pads are high-impedance and nothing else on the net holds it".to_string(),
        (Window::Rest, _) => "Even with firmware running nothing holds the net".to_string(),
    };
    format!("{feed} {when}, so it has no DC level: {effect}. {REMEDY_MARKER}{fix}")
}

/// Run the detector in both windows.
#[must_use]
pub fn float_findings(board: &ExtractedBoard, lib: &ModelLibrary) -> FloatShadow {
    let ctx = Ctx::new(board, lib);
    let mut out = FloatShadow::default();
    run_window(&ctx, Window::Rest, &mut out);
    if !out.unjudged {
        run_window(&ctx, Window::Reset, &mut out);
    }
    out
}

/// The lint check a detector target reports under, where it has one.
#[must_use]
pub const fn lint_check(target: SimTarget) -> Option<LintCheck> {
    match target {
        SimTarget::FloatingInputs => Some(LintCheck::FloatingInput),
        SimTarget::ModelControls => Some(LintCheck::FloatingControlPin),
        SimTarget::NodeDrive => Some(LintCheck::FloatingGate),
        SimTarget::OpenDrainPullup => Some(LintCheck::MissingOpenDrainPullup),
        SimTarget::PinBias => Some(LintCheck::RequiredPinBias),
        SimTarget::ControlHeldOff => Some(LintCheck::ControlHeldOff),
        SimTarget::Straps => Some(LintCheck::StrapPin),
        SimTarget::FetOrientation | SimTarget::BackPower | SimTarget::PowerStates => None,
    }
}

/// The checks whose findings say "nothing sets this net's level".
const FLOAT_CHECKS: &[LintCheck] = &[
    LintCheck::FloatingInput,
    LintCheck::FloatingControlPin,
    LintCheck::CapNoDcPath,
    LintCheck::FloatingGate,
    LintCheck::MissingOpenDrainPullup,
    LintCheck::RequiredPinBias,
    LintCheck::ControlHeldOff,
    LintCheck::StrapPin,
    LintCheck::RegulatorHeldOff,
];

/// The detector's findings the hand checks did not make, as lint findings.
///
/// Each is Medium and routed through the lint check its target maps to
/// ([`lint_check`]). A detector finding is dropped where a hand finding of
/// a float-shaped check, of any severity, already names the same part on a
/// net of the same cluster: the hand finding stays, so nothing is reported
/// twice. A part and pin the detector found in both windows is reported once,
/// in the first.
#[must_use]
pub fn float_promoted(
    board: &ExtractedBoard,
    lib: &ModelLibrary,
    hand: &[LintFinding],
) -> Vec<LintFinding> {
    promote(&float_findings(board, lib), hand)
}

fn promote(shadow: &FloatShadow, hand: &[LintFinding]) -> Vec<LintFinding> {
    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut out = Vec::new();
    for f in &shadow.findings {
        let Some(check) = lint_check(f.target) else {
            continue;
        };
        let covered = hand.iter().any(|h| {
            FLOAT_CHECKS.contains(&h.check)
                && h.refs.contains(&f.part)
                && h.nets.iter().any(|n| f.nets.contains(n))
        });
        if covered || !seen.insert((f.part.clone(), f.pin.clone())) {
            continue;
        }
        out.push(LintFinding {
            check,
            severity: Severity::Medium,
            message: f.plain.clone(),
            refs: vec![f.part.clone()],
            nets: f.nets.clone(),
        });
    }
    out
}

/// One hand finding of the seven checks the detector shadows.
#[derive(Clone, Debug)]
pub struct FloatHand {
    pub target: SimTarget,
    pub part: String,
    pub nets: Vec<String>,
    pub message: String,
}

/// The hand findings the detector stands in for: the float-shaped findings
/// of each check, Low severity left out.
#[must_use]
pub fn float_hand(board: &ExtractedBoard, lib: &ModelLibrary) -> Vec<FloatHand> {
    let mut out = Vec::new();
    let mut add =
        |t: SimTarget, findings: Vec<LintFinding>, keep: &dyn Fn(&LintFinding) -> bool| {
            for f in findings {
                if f.severity == Severity::Low || !keep(&f) {
                    continue;
                }
                if let Some(part) = f.refs.first() {
                    out.push(FloatHand {
                        target: t,
                        part: part.clone(),
                        nets: f.nets.clone(),
                        message: f.message.clone(),
                    });
                }
            }
        };
    let is = |c: LintCheck| move |f: &LintFinding| f.check == c;
    add(
        SimTarget::FloatingInputs,
        super::floating_inputs::floating_input_lint(board, lib).findings,
        &is(LintCheck::FloatingInput),
    );
    add(
        SimTarget::ModelControls,
        super::model_controls::model_control_lint(board, lib).findings,
        &is(LintCheck::FloatingControlPin),
    );
    add(
        SimTarget::NodeDrive,
        super::node_drive::node_drive_lint(board, lib).findings,
        &|f| matches!(f.check, LintCheck::CapNoDcPath | LintCheck::FloatingGate),
    );
    add(
        SimTarget::OpenDrainPullup,
        super::open_drain_pullup::open_drain_pullup_lint(board, lib).findings,
        &is(LintCheck::MissingOpenDrainPullup),
    );
    add(
        SimTarget::PinBias,
        super::pin_bias::pin_bias_lint(board, lib).findings,
        &|f| f.check == LintCheck::RequiredPinBias && f.message.contains("It has no bias"),
    );
    add(
        SimTarget::ControlHeldOff,
        super::control_held_off::control_held_off_lint(board, lib).findings,
        &|f| {
            f.check == LintCheck::ControlHeldOff
                && (f.message.contains("floats through power-up and reset")
                    || f.message.contains("switched on while the MCU is in reset"))
        },
    );
    add(
        SimTarget::Straps,
        super::straps::strap_lint(board, lib).findings,
        &|f| {
            f.check == LintCheck::StrapPin
                && (f.message.contains("float") || f.message.contains("undefined"))
        },
    );
    out
}

/// The detector's ledger for one board, with its findings and cost.
#[derive(Clone, Debug, Default)]
pub struct FloatLedger {
    pub rows: Vec<LedgerRow>,
    pub shadow: FloatShadow,
    /// Wall time of the detector (s).
    pub shadow_s: f64,
}

/// Why the detector is silent on a hand finding, and whether that is an
/// abstention.
fn float_silence(shadow: &FloatShadow, nets: &[String]) -> (String, bool) {
    if let Some(why) = nets.iter().find_map(|n| shadow.why_not.get(n)) {
        return (why.clone(), true);
    }
    if shadow.unjudged {
        return (shadow.notes.join("; "), true);
    }
    let mut levels = String::new();
    for net in nets {
        for w in [Window::Rest, Window::Reset] {
            if let Some((a, b)) = shadow.levels.get(&(w, net.clone())) {
                let _ = write!(levels, " '{net}' {} {a:.2}/{b:.2} V;", w.name());
            }
        }
    }
    if levels.is_empty() {
        return ("no net the solve binds: nothing to judge".to_string(), true);
    }
    (
        format!("solved and silent (leak to ground / to rail):{levels}"),
        false,
    )
}

/// The parity ledger for the seven float-shaped checks.
///
/// Each hand (check, part) pair is matched by any detector finding sharing a
/// net with it (or naming its part when it names no net); unmatched detector
/// findings are sim-only rows under their own target.
#[must_use]
pub fn float_ledger(board: &ExtractedBoard, lib: &ModelLibrary) -> FloatLedger {
    let hand = float_hand(board, lib);
    let start = Instant::now();
    let shadow = float_findings(board, lib);
    let shadow_s = start.elapsed().as_secs_f64();
    let mut keyed: BTreeMap<(SimTarget, String), (Vec<String>, String)> = BTreeMap::new();
    for h in hand {
        let e = keyed
            .entry((h.target, h.part))
            .or_insert_with(|| (Vec::new(), h.message));
        e.0.extend(h.nets);
    }
    let mut used = vec![false; shadow.findings.len()];
    let mut rows = Vec::new();
    for ((t, part), (nets, _)) in &keyed {
        let hit = (shadow.findings.iter().enumerate()).find(|(_, f)| {
            if nets.is_empty() {
                &f.part == part
            } else {
                f.nets.iter().any(|n| nets.contains(n))
            }
        });
        let (side, detail) = if let Some((i, f)) = hit {
            for (j, g) in shadow.findings.iter().enumerate() {
                if g.nets == f.nets {
                    used[j] = true;
                }
            }
            used[i] = true;
            (Side::Both, format!("{}: {}", f.window.name(), f.message))
        } else {
            let (why, abstained) = float_silence(&shadow, nets);
            let side = if abstained {
                Side::Abstained
            } else {
                Side::HandOnly
            };
            (side, why)
        };
        rows.push(LedgerRow {
            check: *t,
            part: part.clone(),
            side,
            detail,
        });
    }
    let mut sim_only: BTreeMap<(SimTarget, String), String> = BTreeMap::new();
    for (f, _) in shadow.findings.iter().zip(&used).filter(|(_, u)| !**u) {
        sim_only
            .entry((f.target, f.part.clone()))
            .or_insert_with(|| format!("{}: {}", f.window.name(), f.message));
    }
    for ((t, part), detail) in sim_only {
        if keyed.contains_key(&(t, part.clone())) {
            continue;
        }
        rows.push(LedgerRow {
            check: t,
            part,
            side: Side::SimOnly,
            detail,
        });
    }
    FloatLedger {
        rows,
        shadow,
        shadow_s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checks::test_support::{board, part};

    const GND: i64 = 1;
    const V3: i64 = 2;
    const SIG: i64 = 3;
    const V5: i64 = 4;
    const NETS: &[(i64, &str)] = &[(GND, "GND"), (V3, "+3V3"), (SIG, "SIG"), (V5, "+5V")];

    fn schmitt(sig: i64) -> Component {
        // 74HC14 pins: 1 = 1A, 7 = GND, 14 = VCC.
        part("U1", "74HC14", &[("1", sig), ("7", GND), ("14", V3)])
    }

    fn of(shadow: &FloatShadow, part: &str) -> Vec<FloatFinding> {
        (shadow.findings.iter())
            .filter(|f| f.part == part)
            .cloned()
            .collect()
    }

    /// A CMOS input nothing drives is flagged, and its levels follow the two
    /// leakages; the same input with a pull-down is silent.
    #[test]
    fn a_cmos_input_with_nothing_on_it_floats_and_a_pull_defines_it() {
        let lib = ModelLibrary::builtin();
        let open = float_findings(&board(NETS, vec![schmitt(SIG)]), &lib);
        let found = of(&open, "U1");
        assert!(!found.is_empty(), "{open:#?}");
        let f = &found[0];
        assert_eq!(f.target, SimTarget::FloatingInputs, "{f:#?}");
        assert_eq!(f.window, Window::Rest);
        assert!(f.nets.contains(&"SIG".to_string()), "{f:#?}");
        assert!(f.levels.0 < 0.5 && f.levels.1 > 2.5, "{f:#?}");
        assert!(f.message.contains("has no DC level"), "{}", f.message);

        let pulled = float_findings(
            &board(
                NETS,
                vec![schmitt(SIG), part("R1", "10k", &[("1", SIG), ("2", GND)])],
            ),
            &lib,
        );
        assert!(of(&pulled, "U1").is_empty(), "{pulled:#?}");
    }

    /// An `ATmega328P` pad (high impedance in reset, by its card) is the only
    /// holder of a load switch's enable: once firmware runs the pad drives,
    /// so the rest window is silent, and the reset window flags it for
    /// `control_held_off`. A pull-down on the enable silences both.
    #[test]
    fn an_mcu_pad_high_z_in_reset_driving_an_enable_is_flagged_only_in_the_reset_window() {
        let card = "[[models]]\nid = \"float_sw\"\nkind = \"analog_switch\"\n\
             description = \"enable fixture\"\n[models.match]\n\
             value_re = \"^FLOATSW$\"\n[models.params]\nron = 0.04\nroff = 5.0e7\n\
             vth = 1.2\nen_on_level = \"high\"\nen_open = \"undefined\"\n\
             en_open_basis = \"fixture p.1: do not leave open\"\n\
             must_not_float_roles = \"ctrl\"\n\
             [models.pins]\n\"1\" = \"in_out_a\"\n\"2\" = \"vss\"\n\"3\" = \"ctrl\"\n\
             \"4\" = \"in_out_b\"\n\"5\" = \"vdd\"\n";
        let dir = hauksbee_testkit::fresh_scratch_dir("float_parity", "sw");
        std::fs::write(dir.join("models.toml"), card).unwrap();
        let mut lib = ModelLibrary::builtin();
        let errors = lib.load_user_dir(&dir);
        assert!(errors.is_empty(), "{errors:?}");
        let parts = |extra: Vec<Component>| {
            let mut all = vec![
                part(
                    "U1",
                    "FLOATSW",
                    &[("1", V5), ("2", GND), ("3", SIG), ("5", V5)],
                ),
                // DIP-28: 7 = VCC, 8 = GND, 14 = PB0.
                part("U2", "ATmega328P", &[("7", V5), ("8", GND), ("14", SIG)]),
            ];
            all.extend(extra);
            board(NETS, all)
        };
        let open = float_findings(&parts(vec![]), &lib);
        let found = of(&open, "U1");
        assert!(
            found.iter().all(|f| f.window == Window::Reset),
            "{found:#?}"
        );
        assert!(
            found
                .iter()
                .any(|f| f.target == SimTarget::ControlHeldOff && f.pin == "3"),
            "{open:#?}"
        );
        let pulled = float_findings(
            &parts(vec![part("R1", "10k", &[("1", SIG), ("2", GND)])]),
            &lib,
        );
        assert!(of(&pulled, "U1").is_empty(), "{pulled:#?}");
    }

    /// An MCU card whose pad 3 is pulled up and pad 4 pulled down in reset,
    /// on an active-high load-switch enable: the reset window stamps the
    /// pull, so pad 3 switches the part on while the MCU is in reset (and the
    /// hand check, with its polarity fix, says the same on the same net) and
    /// pad 4 holds it off.
    #[test]
    fn a_pad_pulled_toward_the_enables_on_level_in_reset_switches_it_on_and_toward_off_is_silent() {
        let card = "[[models]]\nid = \"float_sw2\"\nkind = \"analog_switch\"\n\
             description = \"enable fixture\"\n[models.match]\n\
             value_re = \"^FLOATSW2$\"\n[models.params]\nron = 0.04\nroff = 5.0e7\n\
             vth = 1.2\nen_on_level = \"high\"\n\
             [models.pins]\n\"1\" = \"in_out_a\"\n\"2\" = \"vss\"\n\"3\" = \"ctrl\"\n\
             \"4\" = \"in_out_b\"\n\"5\" = \"vdd\"\n\
             [[models]]\nid = \"float_mcu\"\nkind = \"mcu\"\n\
             description = \"reset mode fixture\"\n[models.match]\n\
             value_re = \"^FLOATMCU$\"\n[models.pins]\n\"1\" = \"vdd\"\n\
             \"2\" = \"gnd\"\n\"3\" = \"gpio_b\"\n\"4\" = \"gpio_c\"\n\
             [models.pin_capability.reset_mode]\nbasis = \"fixture\"\n\
             [models.pin_capability.reset_mode.pads]\n\"3\" = \"pull_up\"\n\"4\" = \"pull_down\"\n";
        let dir = hauksbee_testkit::fresh_scratch_dir("float_parity", "pulled");
        std::fs::write(dir.join("models.toml"), card).unwrap();
        let mut lib = ModelLibrary::builtin();
        let errors = lib.load_user_dir(&dir);
        assert!(errors.is_empty(), "{errors:?}");
        let on_pad = |pad: &str| {
            board(
                NETS,
                vec![
                    part(
                        "U1",
                        "FLOATSW2",
                        &[("1", V3), ("2", GND), ("3", SIG), ("5", V3)],
                    ),
                    part("U2", "FLOATMCU", &[("1", V3), ("2", GND), (pad, SIG)]),
                ],
            )
        };
        let up = float_ledger(&on_pad("3"), &lib);
        let found = of(&up.shadow, "U1");
        assert!(
            found.iter().any(|f| f.window == Window::Reset
                && f.target == SimTarget::ControlHeldOff
                && f.message.contains("switched on while the MCU is in reset")),
            "{up:#?}"
        );
        let row = (up.rows.iter()).find(|r| r.part == "U1" && r.check == SimTarget::ControlHeldOff);
        assert!(row.is_some_and(|r| r.side == Side::Both), "{:#?}", up.rows);
        let down = float_findings(&on_pad("4"), &lib);
        assert!(of(&down, "U1").is_empty(), "{down:#?}");
    }

    /// A W25Q32JV whose /HOLD hangs on an MCU pad that is high impedance in
    /// reset: with /CS pulled low in reset the flash is selected and /HOLD is
    /// flagged in the reset window; with /CS pulled high it is deselected,
    /// /HOLD is ignored (W25Q32JV Rev G section 4.4 p.9) and the detector is
    /// silent, with a note saying why.
    #[test]
    fn a_deselected_spi_flash_does_not_read_its_hold_pin_in_reset() {
        const CS: i64 = 5;
        const HOLD: i64 = 6;
        let card = "[[models]]\nid = \"float_mcu3\"\nkind = \"mcu\"\n\
             description = \"reset mode fixture\"\n[models.match]\n\
             value_re = \"^FLOATMCU3$\"\n[models.pins]\n\"1\" = \"vdd\"\n\
             \"2\" = \"gnd\"\n\"3\" = \"gpio_b\"\n\"4\" = \"gpio_c\"\n\
             \"5\" = \"gpio_d\"\n\
             [models.pin_capability.reset_mode]\nbasis = \"fixture\"\n\
             [models.pin_capability.reset_mode.pads]\n\"3\" = \"pull_up\"\n\"4\" = \"pull_down\"\n";
        let dir = hauksbee_testkit::fresh_scratch_dir("float_parity", "deselected");
        std::fs::write(dir.join("models.toml"), card).unwrap();
        let mut lib = ModelLibrary::builtin();
        let errors = lib.load_user_dir(&dir);
        assert!(errors.is_empty(), "{errors:?}");
        let nets = &[
            (GND, "GND"),
            (V3, "+3V3"),
            (CS, "FLASH_CS"),
            (HOLD, "FLASH_HOLD"),
        ];
        // W25Q32JV SOIC-8: 1 /CS, 4 GND, 7 /HOLD, 8 VCC. MCU pad 5 is high
        // impedance in reset (unlisted); pad 3 pulls up, pad 4 pulls down.
        let with_cs_on = |pad: &str| {
            board(
                nets,
                vec![
                    part(
                        "U1",
                        "W25Q32JVSSIQ",
                        &[("1", CS), ("4", GND), ("7", HOLD), ("8", V3)],
                    ),
                    part(
                        "U2",
                        "FLOATMCU3",
                        &[("1", V3), ("2", GND), (pad, CS), ("5", HOLD)],
                    ),
                ],
            )
        };
        let selected = float_findings(&with_cs_on("4"), &lib);
        assert!(
            of(&selected, "U1").iter().any(|f| f.window == Window::Reset
                && f.pin == "7"
                && f.target == SimTarget::ModelControls),
            "{selected:#?}"
        );
        let deselected = float_findings(&with_cs_on("3"), &lib);
        assert!(of(&deselected, "U1").is_empty(), "{deselected:#?}");
        assert!(
            (deselected.notes.iter()).any(|n| n.contains("U1 pin 7") && n.contains("deselected")),
            "{:#?}",
            deselected.notes
        );
    }

    /// A pad the MCU card maps but leaves out of its `reset_mode` table is high
    /// impedance in reset, so a CMOS input on it alone is flagged; a pad the
    /// card does not map at all has an unknown reset state, so the detector
    /// abstains and says why.
    #[test]
    fn an_unmapped_mcu_pad_abstains_in_reset_and_a_mapped_unlisted_pad_floats() {
        let card = "[[models]]\nid = \"float_mcu4\"\nkind = \"mcu\"\n\
             description = \"partial pin map fixture\"\n[models.match]\n\
             value_re = \"^FLOATMCU4$\"\n[models.pins]\n\"1\" = \"vdd\"\n\
             \"2\" = \"gnd\"\n\"3\" = \"gpio_b\"\n\"4\" = \"gpio_c\"\n\
             [models.pin_capability.reset_mode]\nbasis = \"fixture\"\n\
             [models.pin_capability.reset_mode.pads]\n\"3\" = \"pull_up\"\n";
        let dir = hauksbee_testkit::fresh_scratch_dir("float_parity", "unmapped");
        std::fs::write(dir.join("models.toml"), card).unwrap();
        let mut lib = ModelLibrary::builtin();
        let errors = lib.load_user_dir(&dir);
        assert!(errors.is_empty(), "{errors:?}");
        let on_pad = |pad: &str| {
            board(
                NETS,
                vec![
                    schmitt(SIG),
                    part("U2", "FLOATMCU4", &[("1", V3), ("2", GND), (pad, SIG)]),
                ],
            )
        };
        let mapped = float_findings(&on_pad("4"), &lib);
        assert!(
            of(&mapped, "U1")
                .iter()
                .any(|f| f.window == Window::Reset && f.target == SimTarget::FloatingInputs),
            "{mapped:#?}"
        );
        let unmapped = float_findings(&on_pad("5"), &lib);
        assert!(of(&unmapped, "U1").is_empty(), "{unmapped:#?}");
        // The first window's reason is kept: at rest the unmapped pad may drive.
        assert!(
            unmapped
                .why_not
                .get("SIG")
                .is_some_and(|w| w.contains("U2 pin 5")),
            "{:#?}",
            unmapped.why_not
        );
    }

    /// The ledger puts a hand floating-input finding and the detector's on
    /// the same row.
    #[test]
    fn the_ledger_matches_a_hand_floating_input_by_its_net() {
        let lib = ModelLibrary::builtin();
        let ledger = float_ledger(&board(NETS, vec![schmitt(SIG)]), &lib);
        let row =
            (ledger.rows.iter()).find(|r| r.part == "U1" && r.check == SimTarget::FloatingInputs);
        assert!(
            row.is_some_and(|r| r.side == Side::Both),
            "{:#?}",
            ledger.rows
        );
    }

    /// The STM32F411's printed RPU row (DS10314 Table 53 p.99, 30-50 kOhm,
    /// midpoint 40 kOhm) is used, and for its pull-down too (the loader keeps
    /// the RPD row's identical range only once), marked assumed. The
    /// `ATmega328P` card's `io_pull_up_max_ohms` (50 kOhm) stands in for a row
    /// it does not carry, and a card with neither gets the class value, marked
    /// assumed.
    #[test]
    fn a_card_weak_pull_row_wins_over_the_class_value() {
        let lib = ModelLibrary::builtin();
        // Resolved as the detector resolves a fitted part.
        let card = |value: &str, footprint: &str| {
            let mut comp = part("U9", value, &[("1", V3)]);
            comp.footprint = footprint.into();
            let fitted = AssemblyState::of(&comp).fitted().expect("fitted");
            let res = hauksbee_bind::binder::resolve(&lib, fitted);
            let model = res.model.unwrap_or_else(|| panic!("{value} resolves"));
            (model, res.provenance)
        };
        let (stm, source) = card("STM32F411VET6", "Package_QFP:LQFP-100_14x14mm_P0.5mm");
        assert_eq!(weak_pull(&stm, source.as_ref(), true), (40e3, false));
        assert_eq!(weak_pull(&stm, source.as_ref(), false), (40e3, true));
        let (avr, source) = card("ATmega328P", "Package_DIP:DIP-28_W7.62mm");
        assert_eq!(weak_pull(&avr, source.as_ref(), true), (50e3, false));
        let (hc, source) = card("74HC14", "Package_SO:SOIC-14_3.9x8.7mm_P1.27mm");
        assert_eq!(
            weak_pull(&hc, source.as_ref(), true),
            (CLASS_WEAK_PULL_OHMS, true)
        );
    }

    /// An N-channel FET on `gate`, source to ground, drain on a load net.
    fn nmos(gate: i64) -> Component {
        let mut q = part("Q2", "2N7002", &[("G", gate), ("S", GND), ("D", 9)]);
        q.lib_id = "Device:Q_NMOS".into();
        q.footprint = "X".into();
        q
    }

    /// DIP-28 `ATmega328P` with PD0 (pad 2) on `net`.
    fn avr(net: i64) -> Component {
        let mut u = part("U2", "ATmega328P", &[("2", net), ("7", V5), ("8", GND)]);
        u.lib_id = "MCU:ATmega328P".into();
        u.footprint = "Package_DIP:DIP-28_W7.62mm".into();
        u.pins[0].function = "PD0".into();
        u
    }

    /// The pad reaches the gate through a 1k series resistor, as a gate
    /// drive usually does: the hand floating-gate check reads the resistor
    /// as a defined load and says nothing.
    fn gate_board(extra: Vec<Component>) -> ExtractedBoard {
        let mut all = vec![
            nmos(10),
            avr(SIG),
            part("R2", "1k", &[("1", SIG), ("2", 10)]),
        ];
        all.extend(extra);
        board(
            &[
                (GND, "GND"),
                (V5, "+5V"),
                (SIG, "DRIVE"),
                (9, "LOAD"),
                (10, "GATE"),
            ],
            all,
        )
    }

    /// A FET gate fed only by an MCU pad that is high-impedance in reset is
    /// promoted: Medium, under `floating_gate`, naming the FET, the pad, the
    /// reset window and the pull-down; a pull-down on the gate is silent.
    #[test]
    fn a_fet_gate_on_a_hiz_mcu_pad_is_promoted_with_its_pad_window_and_fix() {
        let lib = ModelLibrary::builtin();
        let shadow = float_findings(&gate_board(vec![]), &lib);
        let found = promote(&shadow, &[]);
        assert_eq!(found.len(), 1, "{shadow:#?}");
        let f = &found[0];
        assert_eq!(f.check, LintCheck::FloatingGate);
        assert_eq!(f.severity, Severity::Medium);
        assert_eq!(f.refs, ["Q2"]);
        assert_eq!(f.nets, ["DRIVE", "GATE"]);
        for want in [
            "Q2 (2N7002) gate (net 'GATE') is driven only by U2 pad 2 (PD0)",
            "While the MCU is in reset its pad is high-impedance",
            "conducts or not on leakage alone",
            "Remedy: Add a 100k pull-down from 'GATE' to ground",
        ] {
            assert!(f.message.contains(want), "{want}: {}", f.message);
        }
        assert_eq!(
            f.own_remedy().map(|r| r.starts_with("Add a 100k")),
            Some(true)
        );

        let pulled = float_findings(
            &gate_board(vec![part("R1", "100k", &[("1", 10), ("2", GND)])]),
            &lib,
        );
        assert!(promote(&pulled, &[]).is_empty(), "{pulled:#?}");
    }

    /// An input nothing drives is promoted at rest without a pad, and a
    /// hand finding of a float-shaped check on the same part and net, of any
    /// severity, keeps the report to one finding; another part, another net
    /// or another kind of check does not hide it.
    #[test]
    fn a_hand_finding_on_the_same_part_and_net_leaves_nothing_to_add() {
        let lib = ModelLibrary::builtin();
        let shadow = float_findings(&board(NETS, vec![schmitt(SIG)]), &lib);
        let found = promote(&shadow, &[]);
        assert_eq!(found.len(), 1, "{shadow:#?}");
        assert_eq!(found[0].check, LintCheck::FloatingInput);
        assert!(
            found[0].message.contains("which nothing drives or pulls"),
            "{}",
            found[0].message
        );
        assert!(found[0].message.contains("Even with firmware running"));
        let hand = |severity| LintFinding {
            check: LintCheck::FloatingInput,
            severity,
            message: "hand".into(),
            refs: vec!["U1".into()],
            nets: vec!["SIG".into()],
        };
        for severity in [Severity::Low, Severity::Medium, Severity::High] {
            assert!(
                promote(&shadow, &[hand(severity)]).is_empty(),
                "{severity:?}"
            );
        }
        let other_part = LintFinding {
            refs: vec!["U9".into()],
            ..hand(Severity::High)
        };
        assert_eq!(promote(&shadow, &[other_part]).len(), 1);
        let other_check = LintFinding {
            check: LintCheck::RailConflict,
            ..hand(Severity::High)
        };
        assert_eq!(promote(&shadow, &[other_check]).len(), 1);
        let other_net = LintFinding {
            nets: vec!["OTHER".into()],
            ..hand(Severity::High)
        };
        assert_eq!(promote(&shadow, &[other_net]).len(), 1);
    }

    /// `check` carries the promoted finding: the whole lint on the gate
    /// fixture holds the detector's Medium finding, and none once the gate
    /// has a pull-down.
    #[test]
    fn the_engine_lint_reports_the_promoted_gate() {
        let lib = ModelLibrary::builtin();
        let report = crate::checks::engine_lint(&gate_board(vec![]), &lib);
        let gates: Vec<_> = (report.findings.iter())
            .filter(|f| f.check == LintCheck::FloatingGate && f.refs == ["Q2"])
            .collect();
        assert!(
            gates
                .iter()
                .any(|f| f.severity == Severity::Medium && f.message.contains("U2 pad 2 (PD0)")),
            "{gates:#?}"
        );
        let pulled = crate::checks::engine_lint(
            &gate_board(vec![part("R1", "100k", &[("1", 10), ("2", GND)])]),
            &lib,
        );
        assert!(
            (pulled.findings.iter()).all(|f| f.check != LintCheck::FloatingGate),
            "{:#?}",
            pulled.findings
        );
    }

    /// An MCU with a core supply and an I/O supply pulls its pad up to the
    /// I/O supply: a pull-up toward an active-high enable's on level switches
    /// it on even though the core rail (1.1 V) is the first supply pin.
    #[test]
    fn a_reset_pull_goes_to_the_io_supply_not_the_core_supply() {
        let card = "[[models]]\nid = \"float_sw4\"\nkind = \"analog_switch\"\n\
             description = \"enable fixture\"\n[models.match]\n\
             value_re = \"^FLOATSW4$\"\n[models.params]\nron = 0.04\nroff = 5.0e7\n\
             vth = 1.2\nen_on_level = \"high\"\n\
             [models.pins]\n\"1\" = \"in_out_a\"\n\"2\" = \"vss\"\n\"3\" = \"ctrl\"\n\
             \"4\" = \"in_out_b\"\n\"5\" = \"vdd\"\n\
             [[models]]\nid = \"float_mcu4\"\nkind = \"mcu\"\n\
             description = \"core and io supply fixture\"\n[models.match]\n\
             value_re = \"^FLOATMCU4$\"\n[models.pins]\n\"1\" = \"dvdd\"\n\
             \"2\" = \"gnd\"\n\"3\" = \"gpio_b\"\n\"4\" = \"iovdd\"\n\
             [models.pin_capability.reset_mode]\nbasis = \"fixture\"\n\
             [models.pin_capability.reset_mode.pads]\n\"3\" = \"pull_up\"\n";
        let dir = hauksbee_testkit::fresh_scratch_dir("float_parity", "iosupply");
        std::fs::write(dir.join("models.toml"), card).unwrap();
        let mut lib = ModelLibrary::builtin();
        let errors = lib.load_user_dir(&dir);
        assert!(errors.is_empty(), "{errors:?}");
        let core = 7;
        let nets = &[(GND, "GND"), (V3, "+3V3"), (SIG, "SIG"), (core, "+1V1")];
        let found = float_findings(
            &board(
                nets,
                vec![
                    part(
                        "U1",
                        "FLOATSW4",
                        &[("1", V3), ("2", GND), ("3", SIG), ("5", V3)],
                    ),
                    part(
                        "U2",
                        "FLOATMCU4",
                        &[("1", core), ("2", GND), ("3", SIG), ("4", V3)],
                    ),
                ],
            ),
            &lib,
        );
        assert!(
            of(&found, "U1").iter().any(|f| f.window == Window::Reset
                && f.message.contains("switched on while the MCU is in reset")),
            "{found:#?}"
        );
    }

    /// A library that types every pin of a big part `input` has not said
    /// which pads drive, so a gate on such a pad abstains; a small symbol
    /// typed `input` reads the net and drives nothing, so the gate is judged.
    #[test]
    fn a_pad_of_an_all_input_symbol_may_drive_and_the_net_abstains() {
        let all_input = |n: usize| {
            let mut u = part("U7", "BIGCHIP", &[]);
            u.pins = (1..=n)
                .map(|i| {
                    let mut p =
                        crate::test_support::pin(&i.to_string(), if i == 1 { SIG } else { GND });
                    p.kind = "input".into();
                    p
                })
                .collect();
            u
        };
        let lib = ModelLibrary::builtin();
        let nets = &[(GND, "GND"), (V5, "+5V"), (SIG, "GATE"), (9, "LOAD")];
        let found = float_findings(&board(nets, vec![nmos(SIG), all_input(24)]), &lib);
        assert!(of(&found, "Q2").is_empty(), "{found:#?}");
        assert!(
            (found.why_not.get("GATE")).is_some_and(|w| w.contains("U7 pin 1")),
            "{:#?}",
            found.why_not
        );
        let found = float_findings(&board(nets, vec![nmos(SIG), all_input(2)]), &lib);
        assert!(!of(&found, "Q2").is_empty(), "{found:#?}");
    }
}
