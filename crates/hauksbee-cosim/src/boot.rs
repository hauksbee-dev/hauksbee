//! Boot-mode resolution: what the solved board makes a chip do at reset.
//!
//! A part picks its boot source from strap pins sampled as reset is released.
//! The card carries the vendor's table ([`hauksbee_models::boot_mode`]); this
//! module reads each strap pin off the board and looks the levels up. A strap
//! net the board drives reads at its solved voltage against the pin's own
//! thresholds. A net nothing drives (unconnected, or a floating node whose ~0 V
//! solve is only the solver's leakage legs) reads at the chip's internal pull,
//! the way the silicon would, or as undefined when the pin has none.
//!
//! The result is a [`BootDecision`] (hauksbee-ir's vocabulary, so every surface
//! words it the same way) plus the levels behind it, which the QEMU and ROM
//! bootloader paths consume.

use std::collections::{BTreeMap, HashMap};

use hauksbee_bind::binder::McuBinding;
use hauksbee_ir::boot::{BootDecision, BootOutcome, StrapReading, StrapReadingLevel};
use hauksbee_ir::NodeId;
use hauksbee_models::boot_mode::{
    level_from_volts, pull_level, select_mode, BootInput, BootSpec, PinLevel, Selection,
};
use hauksbee_models::schema::StrapInternalPull;

/// What the board holds at one instant, as the resolver needs it.
pub struct BoardSnapshot<'a> {
    /// Role -> board node, for connected pins only.
    pub role_nets: &'a HashMap<String, NodeId>,
    /// Solved node voltages, indexed by node.
    pub volts: &'a [f64],
    /// The chip's I/O supply, which the affine thresholds scale with.
    pub vdd: f64,
    /// A board net's name.
    pub net_name: &'a dyn Fn(NodeId) -> String,
    /// Whether something on the net actually drives it (a rail, ground, a
    /// resistor to either, a source), as opposed to a floating node.
    pub driven: &'a dyn Fn(NodeId) -> bool,
}

/// How a pin was read, before the table is consulted.
#[derive(Debug, Clone, PartialEq)]
pub struct PinRead {
    pub reading: StrapReading,
    /// `None` when the pin has no defined level.
    pub level: Option<PinLevel>,
    /// Whether the board itself set the level (as opposed to an internal pull).
    pub board_driven: bool,
}

const fn reading_level(level: Option<PinLevel>) -> StrapReadingLevel {
    match level {
        Some(PinLevel::Low) => StrapReadingLevel::Low,
        Some(PinLevel::High) => StrapReadingLevel::High,
        None => StrapReadingLevel::Indeterminate,
    }
}

const fn pull_words(pull: StrapInternalPull) -> &'static str {
    match pull {
        StrapInternalPull::PullUp => "internal pull-up",
        StrapInternalPull::PullDown => "internal pull-down",
        StrapInternalPull::None => "no internal pull",
        StrapInternalPull::Unknown => "no documented internal pull",
    }
}

/// Read one pin of the card off the board.
#[must_use]
pub fn read_pin(role: &str, input: Option<&BootInput>, snap: &BoardSnapshot<'_>) -> PinRead {
    let pin = input.map_or_else(|| role.to_uppercase(), BootInput::display_name);
    let (vil, vih, pull) = input.map_or((None, None, StrapInternalPull::Unknown), |i| {
        (i.vil, i.vih, i.internal_pull)
    });
    let node = snap.role_nets.get(role).copied();
    let mut reading = StrapReading {
        role: role.to_string(),
        pin,
        net: None,
        volts: None,
        level: StrapReadingLevel::Indeterminate,
        basis: String::new(),
    };
    if let Some(node) = node.filter(|n| n.is_ground() || (snap.driven)(*n)) {
        let volts = if node.is_ground() {
            0.0
        } else {
            snap.volts.get(node.0 as usize).copied().unwrap_or(0.0)
        };
        reading.net = Some((snap.net_name)(node));
        reading.volts = Some(volts);
        let level = level_from_volts(volts, snap.vdd, vil, vih);
        reading.level = reading_level(level);
        if level.is_none() {
            reading.basis = "between the chip's low and high thresholds".to_string();
        }
        return PinRead {
            reading,
            level,
            board_driven: true,
        };
    }
    // Nothing drives the pin: the silicon pull decides, if there is one.
    let level = pull_level(pull);
    reading.net = node.map(|n| (snap.net_name)(n));
    let state = if node.is_some() {
        "net not driven"
    } else {
        "not connected"
    };
    reading.basis = format!("{state}, {}", pull_words(pull));
    reading.level = reading_level(level);
    PinRead {
        reading,
        level,
        board_driven: false,
    }
}

/// The boot mode the board selects, with the levels behind it.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
    pub decision: BootDecision,
    /// Role -> level for the pins the table reads (undefined pins are `None`).
    pub levels: BTreeMap<String, Option<PinLevel>>,
    /// Every strap pin that has a defined level, whether the board drives it
    /// or the silicon pull settles it: what a QEMU strap value overlays on the
    /// machine's default.
    pub latched: BTreeMap<String, PinLevel>,
}

/// Whether the reset pin reads as asserted, released or unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetLevel {
    Asserted,
    Released,
    /// Nothing drives the net, or it sits between the thresholds.
    Unknown,
}

/// Read the reset pin: asserted means the chip is held in reset.
#[must_use]
pub fn read_reset(spec: &BootSpec, snap: &BoardSnapshot<'_>) -> Option<(ResetLevel, StrapReading)> {
    let reset = spec.reset.as_ref()?;
    let node = snap.role_nets.get(&reset.role).copied()?;
    if !node.is_ground() && !(snap.driven)(node) {
        return None;
    }
    let volts = if node.is_ground() {
        0.0
    } else {
        snap.volts.get(node.0 as usize).copied().unwrap_or(0.0)
    };
    let level = level_from_volts(volts, snap.vdd, reset.vil, reset.vih);
    let asserted = match (level, reset.active_high) {
        (Some(PinLevel::Low), false) | (Some(PinLevel::High), true) => ResetLevel::Asserted,
        (Some(_), _) => ResetLevel::Released,
        (None, _) => ResetLevel::Unknown,
    };
    let reading = StrapReading {
        role: reset.role.clone(),
        pin: reset.role.to_uppercase(),
        net: Some((snap.net_name)(node)),
        volts: Some(volts),
        level: reading_level(level),
        basis: String::new(),
    };
    Some((asserted, reading))
}

/// Resolve the boot mode from the board as it stands. `None` when the card
/// carries no boot table.
#[must_use]
pub fn resolve(spec: &BootSpec, snap: &BoardSnapshot<'_>) -> Option<Resolved> {
    let table = spec.table();
    if table.is_empty() {
        return None;
    }
    let mut levels: BTreeMap<String, Option<PinLevel>> = BTreeMap::new();
    let mut latched: BTreeMap<String, PinLevel> = BTreeMap::new();
    let mut reads: Vec<PinRead> = Vec::new();
    for role in table.strap_roles() {
        let read = read_pin(role, table.input(role), snap);
        levels.insert(role.to_string(), read.level);
        if let Some(level) = read.level {
            latched.insert(role.to_string(), level);
        }
        reads.push(read);
    }
    // Pins the table does not branch on can still change what a machine's own
    // ROM does (a QEMU strap value carries every strap bit), so the ones with a
    // defined level join the overlay.
    for input in &spec.inputs {
        if !levels.contains_key(&input.role) {
            let read = read_pin(&input.role, Some(input), snap);
            if let Some(level) = read.level {
                latched.insert(input.role.clone(), level);
            }
        }
    }
    let cite_of = |row: &hauksbee_models::boot_mode::BootModeRow| row.cite.clone();
    let mut caveats: Vec<String> = reads
        .iter()
        .filter(|r| !r.board_driven)
        .map(|r| {
            format!(
                "{} is not driven by the board ({}); its level here follows the chip's own pull.",
                r.reading.pin, r.reading.basis
            )
        })
        .collect();
    let selection = select_mode(&spec.modes, &levels);
    let (outcome, mode, cite, shown) = match &selection {
        Selection::Row(row) => {
            // Name only the pins this row actually reads: a don't-care pin
            // would only add noise to the sentence.
            let shown: Vec<StrapReading> = reads
                .iter()
                .filter(|r| row.when.contains_key(&r.reading.role))
                .map(|r| r.reading.clone())
                .collect();
            (row.outcome, row.name.clone(), cite_of(row), shown)
        }
        Selection::Indeterminate { roles, rows } => {
            caveats.push(format!(
                "{} {} no defined level, so the chip's boot mode cannot be told.",
                roles.join(", "),
                if roles.len() == 1 { "has" } else { "have" }
            ));
            let shown: Vec<StrapReading> = reads
                .iter()
                .filter(|r| roles.contains(&r.reading.role))
                .map(|r| r.reading.clone())
                .collect();
            let cite = rows.first().map(|r| cite_of(r)).unwrap_or_default();
            (BootOutcome::Undefined, "undefined".to_string(), cite, shown)
        }
        Selection::NoRow => (
            BootOutcome::Undefined,
            "not in the vendor's table".to_string(),
            spec.modes.first().map(cite_of).unwrap_or_default(),
            reads.iter().map(|r| r.reading.clone()).collect(),
        ),
    };
    let decision = BootDecision::new(outcome, &mode, shown, &cite, caveats);
    Some(Resolved {
        decision,
        levels,
        latched,
    })
}

/// The chip's I/O supply for threshold scaling: the highest solved voltage on
/// a direct supply pad, else the backend's nominal logic rail.
fn supply_for(binding: &McuBinding, volts: &[f64], fallback: f64) -> f64 {
    let solved = binding
        .role_nets
        .iter()
        .filter(|(role, _)| crate::scheduler::is_direct_supply_role_name(role))
        .filter_map(|(_, n)| volts.get(n.0 as usize).copied())
        .fold(0.0_f64, f64::max);
    if solved > 1.0 {
        solved
    } else {
        fallback
    }
}

/// Decide each firmware-bearing MCU's boot mode from the board's DC operating point.
///
/// Supplies stand at their start-up values and capacitors are open, the state
/// the straps are read in when nothing has yet ramped. Keyed by MCU reference;
/// an MCU whose card has no boot table is absent.
#[must_use]
pub fn plan_boots<S: std::hash::BuildHasher>(
    mcus: &[McuBinding],
    specs: &HashMap<String, BootSpec, S>,
    circuit: &hauksbee_ir::Circuit,
    opts: &hauksbee_solve::SolverOptions,
) -> HashMap<String, Resolved> {
    let wanted: Vec<&McuBinding> = mcus
        .iter()
        .filter(|m| specs.contains_key(&m.reference))
        .collect();
    if wanted.is_empty() {
        return HashMap::new();
    }
    let volts = crate::scheduler::dc_node_volts(circuit, opts)
        .unwrap_or_else(|| vec![0.0; circuit.node_count()]);
    let refs: Vec<&McuBinding> = mcus.iter().collect();
    let evidence = crate::scheduler::drive_evidence_for(circuit, &refs);
    let driven =
        |n: NodeId| crate::scheduler::net_has_drive_evidence(&evidence, &circuit.devices, n.0);
    let name = |n: NodeId| circuit.node_name(n).to_string();
    wanted
        .into_iter()
        .filter_map(|m| {
            let spec = specs.get(&m.reference)?;
            let snap = BoardSnapshot {
                role_nets: &m.role_nets,
                volts: &volts,
                vdd: supply_for(
                    m,
                    &volts,
                    crate::scheduler::logic_high_for_backend(&m.backend),
                ),
                net_name: &name,
                driven: &driven,
            };
            Some((m.reference.clone(), resolve_with_reset(spec, &snap)?))
        })
        .collect()
}

/// [`resolve`], with a reset pin held asserted taking precedence: a chip held
/// in reset never samples its straps.
#[must_use]
pub fn resolve_with_reset(spec: &BootSpec, snap: &BoardSnapshot<'_>) -> Option<Resolved> {
    let mut resolved = resolve(spec, snap)?;
    if let Some((ResetLevel::Asserted, reading)) = read_reset(spec, snap) {
        let cite = spec
            .reset
            .as_ref()
            .map(|r| r.cite.clone())
            .filter(|c| !c.is_empty())
            .unwrap_or_else(|| resolved.decision.cite.clone());
        resolved.decision = BootDecision::new(
            BootOutcome::HeldInReset,
            "held in reset",
            vec![reading],
            &cite,
            resolved.decision.caveats.clone(),
        );
    }
    Some(resolved)
}

/// What core a resolved boot mode puts on the bus.
pub enum CorePlan {
    /// Not the user firmware, and no emulated ROM to run instead: the ROM
    /// bootloader stand-in.
    RomStub(hauksbee_mcu::boot_mode::RomPersonality),
    /// The normal emulated core; `strap` is handed to a backend that latches
    /// the pins itself (QEMU), so its own ROM takes the boot path.
    Emulated(Option<hauksbee_mcu::boot_mode::QemuStrapMode>),
}

/// ROM-stub clock when the board names none: the 8 MHz internal oscillator
/// every STM32 starts on.
pub const ROM_STUB_HZ: u64 = 8_000_000;

/// Choose the core for a resolved boot mode.
///
/// A chip whose model is a QEMU machine runs its own emulated ROM, so the
/// strap value goes to QEMU and the ROM's behaviour is the real one. Any other
/// chip that does not run the user firmware gets the stand-in. An undefined or
/// held mode runs the normal core (held is gated per chunk; undefined is a
/// caveat, not a guess).
#[must_use]
pub fn plan_core(spec: &BootSpec, resolved: &Resolved) -> CorePlan {
    use hauksbee_mcu::boot_mode::{RomPersonality, StBootloaderInfo, StUsartBootloader};
    use hauksbee_models::boot_mode::RomProtocol;
    if let Some(q) = &spec.qemu {
        // The machine's own ROM takes the boot path the straps select. A board
        // that boots from flash (or whose straps are undefined) leaves QEMU's
        // default flash-boot value alone.
        let strap = matches!(
            resolved.decision.outcome,
            BootOutcome::RomBootloader | BootOutcome::Sram
        )
        .then(|| hauksbee_mcu::boot_mode::QemuStrapMode {
            device: q.device.clone(),
            property: q.property.clone(),
            value: q.strap_value(&resolved.latched),
        });
        return CorePlan::Emulated(strap);
    }
    match resolved.decision.outcome {
        BootOutcome::RomBootloader | BootOutcome::Sram => {
            let personality = match &spec.rom {
                Some(rom) if rom.protocol == RomProtocol::StUsart => {
                    match (rom.version, rom.product_id) {
                        (Some(version), Some(product_id))
                            if resolved.decision.outcome == BootOutcome::RomBootloader =>
                        {
                            RomPersonality::StUsart(StUsartBootloader::new(StBootloaderInfo {
                                version,
                                product_id,
                                commands: rom.commands.clone(),
                            }))
                        }
                        _ => RomPersonality::Silent,
                    }
                }
                _ => RomPersonality::Silent,
            };
            CorePlan::RomStub(personality)
        }
        _ => CorePlan::Emulated(None),
    }
}

/// Build the core a resolved boot mode calls for, with its bookkeeping.
/// `instantiate` makes the normal emulated core (given the QEMU strap value).
///
/// # Errors
///
/// When `instantiate` fails.
pub fn build_core(
    spec: &BootSpec,
    resolved: &Resolved,
    instantiate: impl FnOnce(
        Option<&hauksbee_mcu::boot_mode::QemuStrapMode>,
    ) -> anyhow::Result<Box<dyn hauksbee_mcu::Mcu + Send>>,
) -> anyhow::Result<(Box<dyn hauksbee_mcu::Mcu + Send>, BootState)> {
    let rom_outcome = matches!(
        resolved.decision.outcome,
        BootOutcome::RomBootloader | BootOutcome::Sram
    );
    let core: Box<dyn hauksbee_mcu::Mcu + Send> = match plan_core(spec, resolved) {
        CorePlan::RomStub(personality) => Box::new(hauksbee_mcu::boot_mode::RomBootMcu::new(
            personality,
            ROM_STUB_HZ,
        )),
        CorePlan::Emulated(strap) => instantiate(strap.as_ref())?,
    };
    Ok((core, BootState::new(spec.clone(), resolved, rom_outcome)))
}

/// Fold the boot decision into an MCU's execution evidence.
///
/// A chip that did not boot the user firmware (ROM bootloader, SRAM, held in
/// reset) is not an execution, whatever its ROM or the stand-in said on the
/// UART, and the boot-mode sentence is the reason a report shows.
pub fn annotate(evidence: &mut crate::execution::ExecutionEvidence, state: &BootState) {
    let decision = &state.decision;
    evidence.boot = Some(decision.clone());
    if decision.user_firmware_runs {
        return;
    }
    evidence.executed = false;
    evidence.firmware_pass_eligible = false;
    // The boot mode is the cause; these are only its symptoms.
    evidence.blockers.retain(|b| {
        !(b.starts_with("the core never reached")
            || b.starts_with("no evidence the firmware executed")
            || b.starts_with("the board held the reset line"))
    });
    evidence
        .blockers
        .insert(0, format!("boot straps: {}.", decision.summary));
}

/// One line per MCU whose boot straps kept the user firmware from running
/// (`U1: BOOT0 = 3.30 V at reset release -> ...; user firmware not run`).
#[must_use]
pub fn stop_lines(evidence: &[crate::execution::ExecutionEvidence]) -> Vec<String> {
    evidence
        .iter()
        .filter_map(|e| {
            let boot = e.boot.as_ref().filter(|b| !b.user_firmware_runs)?;
            Some(format!("{}: {}", e.mcu_ref, boot.summary))
        })
        .collect()
}

/// Whether every MCU with execution evidence was kept from the user firmware
/// by its boot straps: whatever its ROM then put on the UART is not the
/// firmware's activity.
#[must_use]
pub fn all_stopped(evidence: &[crate::execution::ExecutionEvidence]) -> bool {
    !evidence.is_empty()
        && evidence
            .iter()
            .all(|e| e.boot.as_ref().is_some_and(|b| !b.user_firmware_runs))
}

/// One MCU's boot bookkeeping across the run.
#[derive(Debug, Clone)]
pub struct BootState {
    pub spec: BootSpec,
    /// The decision reported for this MCU (updated when reset releases late).
    pub decision: BootDecision,
    /// Whether the core is the ROM bootloader stand-in (user firmware never
    /// loaded).
    pub rom: bool,
    /// Whether reset was asserted on the last chunk.
    pub held: bool,
    /// Simulated seconds the chip spent held in reset.
    pub held_s: f64,
    /// Simulated time reset released, when it was held at the start.
    pub released_at_s: Option<f64>,
}

impl BootState {
    #[must_use]
    pub fn new(spec: BootSpec, resolved: &Resolved, rom: bool) -> Self {
        Self {
            held: resolved.decision.outcome == BootOutcome::HeldInReset,
            spec,
            decision: resolved.decision.clone(),
            rom,
            held_s: 0.0,
            released_at_s: None,
        }
    }

    /// Whether the chip is held in reset for this chunk (so its core must not
    /// advance). Called once per chunk, before the core runs, with the board
    /// as the previous chunk solved it. When reset releases, the straps are
    /// read again at that moment: they are what the chip latches.
    pub fn hold_core(&mut self, snap: &BoardSnapshot<'_>, now_s: f64, chunk_s: f64) -> bool {
        if self.rom {
            return false;
        }
        let Some((level, _)) = read_reset(&self.spec, snap) else {
            return false;
        };
        match level {
            ResetLevel::Asserted => {
                self.held = true;
                self.held_s += chunk_s;
                true
            }
            ResetLevel::Released | ResetLevel::Unknown => {
                if self.held {
                    self.held = false;
                    self.released_at_s = Some(now_s);
                    if let Some(at_release) = resolve(&self.spec, snap) {
                        self.relatch(at_release, now_s);
                    }
                }
                false
            }
        }
    }

    /// Replace the decision with the one read at reset release, noting when it
    /// differs from what the start-up point showed.
    fn relatch(&mut self, at_release: Resolved, now_s: f64) {
        let mut decision = at_release.decision;
        decision.caveats.push(format!(
            "Reset was held until {:.3} ms; the boot straps were read when it released.",
            now_s * 1e3
        ));
        self.decision = decision;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hauksbee_models::boot_mode::{BootModeRow, ResetInput};

    fn spec_f1() -> BootSpec {
        let row = |name: &str, when: &[(&str, PinLevel)], outcome| BootModeRow {
            name: name.into(),
            when: when.iter().map(|&(r, l)| (r.to_string(), l)).collect(),
            outcome,
            detail: String::new(),
            cite: "RM0008 Table 9".into(),
        };
        let input = |role: &str, pin: &str| BootInput {
            role: role.into(),
            pin: Some(pin.into()),
            internal_pull: StrapInternalPull::None,
            vil: Some([0.0, 0.8]),
            vih: Some([0.0, 2.0]),
            note: String::new(),
            cite: String::new(),
        };
        BootSpec {
            latch: None,
            reset: Some(ResetInput {
                role: "nrst".into(),
                vil: Some([0.0, 0.8]),
                vih: Some([0.0, 2.0]),
                active_high: false,
                cite: String::new(),
            }),
            inputs: vec![input("boot0", "BOOT0"), input("pb2_boot1", "BOOT1")],
            modes: vec![
                row(
                    "Main flash memory",
                    &[("boot0", PinLevel::Low)],
                    BootOutcome::UserFlash,
                ),
                row(
                    "System memory",
                    &[("boot0", PinLevel::High), ("pb2_boot1", PinLevel::Low)],
                    BootOutcome::RomBootloader,
                ),
                row(
                    "Embedded SRAM",
                    &[("boot0", PinLevel::High), ("pb2_boot1", PinLevel::High)],
                    BootOutcome::Sram,
                ),
            ],
            qemu: None,
            rom: None,
        }
    }

    struct Board {
        role_nets: HashMap<String, NodeId>,
        volts: Vec<f64>,
        driven: Vec<bool>,
    }

    impl Board {
        fn resolve(&self, spec: &BootSpec) -> Option<Resolved> {
            let name = |n: NodeId| format!("N{}", n.0);
            let driven = |n: NodeId| self.driven.get(n.0 as usize).copied().unwrap_or(false);
            resolve(
                spec,
                &BoardSnapshot {
                    role_nets: &self.role_nets,
                    volts: &self.volts,
                    vdd: 3.3,
                    net_name: &name,
                    driven: &driven,
                },
            )
        }
    }

    fn board(boot0: Option<(f64, bool)>, boot1: Option<(f64, bool)>) -> Board {
        let mut role_nets = HashMap::new();
        let mut volts = vec![0.0];
        let mut driven = vec![true];
        for (role, v) in [("boot0", boot0), ("pb2_boot1", boot1)] {
            if let Some((volt, d)) = v {
                role_nets.insert(
                    role.to_string(),
                    NodeId(u32::try_from(volts.len()).unwrap()),
                );
                volts.push(volt);
                driven.push(d);
            }
        }
        Board {
            role_nets,
            volts,
            driven,
        }
    }

    #[test]
    fn boot0_high_with_boot1_low_enters_the_system_bootloader() {
        let r = board(Some((3.3, true)), Some((0.0, true)))
            .resolve(&spec_f1())
            .unwrap();
        assert_eq!(r.decision.outcome, BootOutcome::RomBootloader);
        assert!(!r.decision.user_firmware_runs);
        assert_eq!(
            r.decision.summary,
            "BOOT0 = 3.30 V (high), BOOT1 = 0.00 V (low) at reset release -> System memory; user firmware not run"
        );
    }

    #[test]
    fn boot0_low_runs_the_user_firmware_and_ignores_boot1() {
        let r = board(Some((0.0, true)), None).resolve(&spec_f1()).unwrap();
        assert_eq!(r.decision.outcome, BootOutcome::UserFlash);
        assert!(r.decision.user_firmware_runs);
        assert_eq!(r.decision.straps.len(), 1);
    }

    #[test]
    fn a_floating_boot0_with_no_internal_pull_is_undefined_not_a_guess() {
        let r = board(Some((0.0, false)), Some((0.0, true)))
            .resolve(&spec_f1())
            .unwrap();
        assert_eq!(r.decision.outcome, BootOutcome::Undefined);
        assert!(r.decision.caveats.iter().any(|c| c.contains("BOOT0")));
        assert!(r.latched.is_empty() || !r.latched.contains_key("boot0"));
    }

    #[test]
    fn an_unconnected_pin_with_an_internal_pull_reads_at_the_pull() {
        let mut spec = spec_f1();
        spec.inputs[0].internal_pull = StrapInternalPull::PullDown;
        let r = board(None, None).resolve(&spec).unwrap();
        assert_eq!(r.decision.outcome, BootOutcome::UserFlash);
        assert!(r
            .decision
            .caveats
            .iter()
            .any(|c| c.contains("internal pull-down")));
    }

    #[test]
    fn a_mid_rail_strap_is_not_called_either_level() {
        let r = board(Some((1.4, true)), Some((0.0, true)))
            .resolve(&spec_f1())
            .unwrap();
        assert_eq!(r.decision.outcome, BootOutcome::Undefined);
    }

    #[test]
    fn reset_held_low_reads_as_asserted() {
        let spec = spec_f1();
        let mut b = board(Some((0.0, true)), None);
        b.role_nets.insert("nrst".into(), NodeId(9));
        b.volts.resize(10, 0.0);
        b.driven.resize(10, true);
        let name = |n: NodeId| format!("N{}", n.0);
        let driven = |n: NodeId| b.driven[n.0 as usize];
        let snap = BoardSnapshot {
            role_nets: &b.role_nets,
            volts: &b.volts,
            vdd: 3.3,
            net_name: &name,
            driven: &driven,
        };
        let (level, _) = read_reset(&spec, &snap).unwrap();
        assert_eq!(level, ResetLevel::Asserted);
    }
}
