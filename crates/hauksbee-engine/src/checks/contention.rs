//! Model-aware driver-contention lint: two MODELLED push-pull outputs on one net.
//! Long-form how-and-why: docs/how-and-why/hauksbee-engine/checks.md.
//!
//! The extract layer already has an output-contention check
//! ([`LintCheck::OutputContention`] in `hauksbee-extract/src/netlint.rs`). That
//! one reads the schematic's PIN ELECTRICAL TYPES: it fires when two pins a
//! symbol author typed `output` / `power_out` share a net with nothing to
//! resolve them. It is calibrated to zero false positives, and to get there it
//! treats an `input` or `bidirectional` pin anywhere on the net as a resolver,
//! because symbol authors routinely type shared/IRQ pins as outputs.
//!
//! That calibration leaves a hole exactly where hauksbee knows the most. On the
//! field board that motivated this check, a mis-mapped logic model put a 74HC08
//! (quad AND, push-pull, no output enable) onto a net an MCU also drove as a
//! GPIO. Two drivers, one net, no series element. The MCU's GPIO pad is typed
//! `bidirectional` in the symbol, so the pin-type check saw a resolver and went
//! quiet, and `--lint` printed "Looks healthy" over a real fight.
//!
//! This check answers the same question from the other side: not "what did the
//! symbol author type" but "what did hauksbee BIND". A part that resolves to a
//! digital model has a pad->role map and a declared output set; the binder
//! stamps a Thevenin [`PinDriver`](crate::drivers::PinDriver) on every connected
//! output role, which is precisely a push-pull driver. If two DIFFERENT parts
//! stamp a driver onto the same net, the co-simulation itself is about to solve
//! two voltage sources fighting through their output resistances, and the answer
//! it prints will be a meaningless mid-rail number. That is worth a High finding
//! whatever the symbol's pin types said.
//!
//! ## What counts as a push-pull output
//!
//! [`output_roles`](crate::digital::output_roles) is the binder's own answer, so
//! this check and the binder cannot disagree: a model with a `[models.logic]`
//! block declares `outputs` explicitly; a model without one falls back to the
//! `y*` role convention the synthesized passthrough mirrors onto. The check maps
//! each such role back to a pad through the model's `[models.pins]` map and
//! reads that pad's net off the extracted component.
//!
//! Only the model's OWN pad map is consulted, never the pin-function or
//! pin-rule-table inference the binder layers on top
//! (`role_node_map_guessed`). A guessed role is exactly the kind of soft
//! evidence a High-severity finding must not rest on.
//!
//! ## Exclusions
//!
//! - **Tri-state outputs.** Any output covered by a `[models.logic.tristate]`
//!   group is skipped. That is how the db expresses a 3-state part: the 74HC125
//!   quad 3-state buffer tri-states `y1..y4` on its per-gate `oe_n_*`, and the
//!   74HC595 tri-states `qa..qh` on `oe_n`. Sharing a net is the intended
//!   arrangement for those parts (that is what an output enable is FOR), so they
//!   must never fire here. Open-drain is not expressible for a digital-kind
//!   model at all (the `open_drain` flag lives on the behavioural pin schema, a
//!   different kind), so within this check's scope "declared output and not
//!   tri-stated" is push-pull.
//! - **One part driving its own net twice.** Two output pads of a single chip on
//!   one net is an internal short the symbol expresses, not an inter-part fight,
//!   and it is usually a deliberate parallel-drive idiom. Two DISTINCT
//!   references are required.
//! - **Ground, unconnected, and no-net.** Nothing to contend over.
//! - **DNP parts.** Not assembled, so electrically absent.
//! - **Series resistors do NOT need an exclusion.** Two outputs separated by a
//!   series R are on two different nets, and net extraction never merges across
//!   a two-terminal passive, so the case simply cannot arise inside one net.
//!   Writing a "is there an R on this net" escape hatch would be dead code that
//!   suppresses real findings: an R sitting on a net as a pull-up or a load is
//!   not a series element between the drivers.
//!
//! ## What it deliberately does not reach
//!
//! MCU GPIO direction is firmware state, not netlist state: whether PB5 is an
//! output at the moment the 74HC08 drives is decided by a DDR write the lint
//! cannot see. The binder itself stamps every MCU GPIO driver DISABLED
//! (high-impedance) and only the scheduler enables one, on the pin's first
//! firmware drive. Firing statically on "modelled output shares a net with an
//! MCU pad" would flag every logic output feeding an MCU input, which is the
//! single most common way a logic gate is used, so the model-to-MCU half of the
//! field case is out of static reach. This check catches MODEL-to-MODEL
//! contention only, and its finding prose says so; the model-to-firmware half
//! belongs to co-sim time, where the scheduler knows the real pin directions.
//! That half is covered by the scheduler's runtime monitor
//! (`Scheduler::detect_driver_contention`), which fires when a firmware-enabled
//! GPIO driver and an enabled modelled push-pull output share a net, using the
//! same `output_roles`/tri-state classification this check rests on.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use hauksbee_extract::{ExtractedBoard, LintCheck, LintFinding, NetLintReport, Severity};
use hauksbee_models::{ComponentKind, ModelEntry, ModelLibrary};

use crate::binder::resolve;
use crate::digital::output_roles;

/// One modelled push-pull output pin found on a net.
struct Driver {
    reference: String,
    pad: String,
    role: String,
}

/// One component that bound to a digital-kind model, as this check saw it.
/// This is the calibration's evidence surface: it says how far each candidate
/// part got through the classification, so a corpus sweep can prove its silence
/// is real (parts bound, outputs presented, exclusions applied) rather than
/// vacuous (nothing ever reached the classifier).
pub struct BoundDigitalPart {
    pub reference: String,
    pub value: String,
    /// The resolved model's stable id, e.g. `"74hc125"`.
    pub model_id: String,
    /// Output roles the model declares (what the binder would stamp drivers on).
    pub output_roles: usize,
    /// Of those, roles excluded here because a `[models.logic.tristate]` group
    /// covers them.
    pub tristateable_roles: usize,
    /// Push-pull output pads whose pad number is absent from the extracted
    /// footprint (the model maps a pad the board does not carry).
    pub pads_absent: usize,
    /// Push-pull output pads present but on no net.
    pub pads_unrouted: usize,
    /// Push-pull output pads on ground or KiCad-unconnected nets.
    pub pads_on_excluded_nets: usize,
    /// Push-pull output pads that survived onto an eligible net, i.e. the pins
    /// the contention logic actually reasons about.
    pub pushpull_pins: usize,
}

/// Everything the classification pass saw on one board: the per-part evidence
/// trail plus the net map `contention_lint` judges.
pub struct Scan {
    pub parts: Vec<BoundDigitalPart>,
    by_net: BTreeMap<i64, Vec<Driver>>,
}

impl Scan {
    /// Total modelled push-pull output pins on eligible nets.
    pub fn pushpull_driver_pins(&self) -> usize {
        self.by_net.values().map(Vec::len).sum()
    }

    /// Total output roles the bound digital parts presented to the classifier,
    /// tri-stateable ones included. This is the honest "did the check engage"
    /// measure: a board full of 3-state buffers exercises resolution, role
    /// enumeration, and the tri-state exclusion even though it yields zero
    /// push-pull pins.
    pub fn output_roles_presented(&self) -> usize {
        self.parts.iter().map(|p| p.output_roles).sum()
    }
}

/// Kinds the binder routes through `bind_digital`, i.e. the kinds whose
/// connected output roles get a stamped Thevenin driver. Kept in step with
/// `binder::bind_component`'s match arms.
fn is_digital_kind(kind: ComponentKind) -> bool {
    matches!(
        kind,
        ComponentKind::Digital | ComponentKind::ShiftRegister | ComponentKind::Adc
    )
}

/// Canonical-ground net name, mirroring the extract layer's netlint helper.
fn is_ground_net(name: &str) -> bool {
    let n = name
        .trim()
        .trim_start_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("")
        .to_ascii_uppercase();
    matches!(
        n.as_str(),
        "GND" | "GNDA" | "GNDD" | "AGND" | "DGND" | "PGND" | "VSS" | "GNDIO" | "0"
    ) || n.starts_with("GND")
}

/// KiCad's placeholder net for a pin that is on no net at all.
fn is_unconnected_net(name: &str) -> bool {
    name.trim_start_matches('/').starts_with("unconnected-")
}

/// The output roles of `model` that are tri-stateable, i.e. that the part can
/// release to high impedance under an output-enable pin. Expanded from the
/// `[models.logic.tristate]` group keys, which may be a single output name
/// (`"y1"`) or an inclusive range over the outputs declaration order
/// (`"qa..qh"`).
///
/// A group key that fails to expand means the spec is malformed in a way this
/// check cannot reason about; rather than guess, every output of the part is
/// reported as tri-stateable, which suppresses the part entirely. Staying quiet
/// on a spec we do not understand is the only safe direction for a
/// High-severity finding.
fn tristateable_outputs(model: &ModelEntry) -> HashSet<String> {
    let mut out = HashSet::new();
    for group in model.logic.tristate.keys() {
        match model.logic.expand_tristate_group(group) {
            Ok(names) => out.extend(names),
            Err(_) => return model.logic.outputs.iter().cloned().collect(),
        }
    }
    out
}

/// Walk the board once and classify every modelled digital output pad. The
/// whole of the check's classification lives here; `contention_lint` only
/// decides which nets in the result are worth a finding.
pub fn scan(board: &ExtractedBoard, lib: &ModelLibrary) -> Scan {
    let net_name: HashMap<i64, &str> = board.nets.iter().map(|n| (n.id, n.name.as_str())).collect();

    let mut parts = Vec::new();
    let mut by_net: BTreeMap<i64, Vec<Driver>> = BTreeMap::new();

    for comp in &board.components {
        if comp.dnp {
            continue; // not assembled, so electrically absent
        }
        let Some(model) = resolve(lib, comp).model else {
            continue; // no model, so nothing to say about its pin directions
        };
        if !is_digital_kind(model.kind) {
            continue;
        }
        let all_roles = output_roles(&model);
        let tri = tristateable_outputs(&model);
        let pushpull: HashSet<&str> = all_roles
            .iter()
            .filter(|r| !tri.contains(r.as_str()))
            .map(String::as_str)
            .collect();

        let mut part = BoundDigitalPart {
            reference: comp.reference.clone(),
            value: comp.value.clone(),
            model_id: model.id.clone(),
            output_roles: all_roles.len(),
            tristateable_roles: all_roles
                .iter()
                .filter(|r| tri.contains(r.as_str()))
                .count(),
            pads_absent: 0,
            pads_unrouted: 0,
            pads_on_excluded_nets: 0,
            pushpull_pins: 0,
        };

        // The model's own pad->role map is the only role source consulted; see
        // the module docs on why the binder's guessed roles are excluded.
        for (pad, role) in &model.pins {
            if !pushpull.contains(role.as_str()) {
                continue;
            }
            let Some(pin) = comp.pins.iter().find(|p| &p.number == pad) else {
                part.pads_absent += 1; // the footprint does not carry that pad
                continue;
            };
            let Some(id) = pin.net.filter(|&id| id != 0) else {
                part.pads_unrouted += 1;
                continue;
            };
            let name = net_name.get(&id).copied().unwrap_or("");
            if is_ground_net(name) || is_unconnected_net(name) {
                part.pads_on_excluded_nets += 1;
                continue;
            }
            part.pushpull_pins += 1;
            by_net.entry(id).or_default().push(Driver {
                reference: comp.reference.clone(),
                pad: pad.clone(),
                role: role.clone(),
            });
        }
        parts.push(part);
    }

    Scan { parts, by_net }
}

/// Flag every net carrying push-pull outputs of two or more different modelled
/// digital parts. See the module docs for the classification and the exclusions.
pub fn contention_lint(board: &ExtractedBoard, lib: &ModelLibrary) -> NetLintReport {
    let mut report = NetLintReport::default();

    let net_name: HashMap<i64, &str> = board.nets.iter().map(|n| (n.id, n.name.as_str())).collect();

    for (id, drivers) in scan(board, lib).by_net {
        let refs: BTreeSet<&str> = drivers.iter().map(|d| d.reference.as_str()).collect();
        if refs.len() < 2 {
            continue; // one part driving its own net is not an inter-part fight
        }
        let name = net_name.get(&id).copied().unwrap_or("").to_string();
        let listed: Vec<String> = drivers
            .iter()
            .map(|d| format!("{}.{} ({})", d.reference, d.pad, d.role))
            .collect();
        report.findings.push(LintFinding {
            check: LintCheck::OutputContention,
            severity: Severity::High,
            message: format!(
                "net '{name}' is driven by {} modelled push-pull outputs of {} different parts with \
                 no series element between them: {}. Two outputs driving one net fight: whichever \
                 one wins, the net sits at an indeterminate level, both parts pass current well \
                 outside their output ratings, and the simulation reports a mid-rail number that \
                 looks like data. Check the schematic for a genuine short, and check the model \
                 mapping with `hauksbee models resolve` in case a part bound to the wrong pinout. \
                 Only model-to-model contention is visible here: an MCU GPIO's direction is set by \
                 firmware, so a firmware-driven fight on this net surfaces at co-sim time instead.",
                drivers.len(),
                refs.len(),
                listed.join(", ")
            ),
            refs: refs.iter().map(|r| r.to_string()).collect(),
            nets: vec![name],
        });
    }

    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use hauksbee_extract::{Component, Net, Pin};

    /// A component whose pads are wired by `(pad number, net id)`.
    fn comp(reference: &str, value: &str, pads: &[(&str, i64)]) -> Component {
        Component {
            reference: reference.to_string(),
            value: value.to_string(),
            lib_id: "Logic:X".to_string(),
            footprint: String::new(),
            position: None,
            layer: String::new(),
            properties: Vec::new(),
            dnp: false,
            pins: pads
                .iter()
                .map(|(n, net)| Pin {
                    number: (*n).to_string(),
                    net: Some(*net),
                    function: String::new(),
                    kind: String::new(),
                    position: None,
                })
                .collect(),
        }
    }

    fn board(nets: &[(i64, &str)], components: Vec<Component>) -> ExtractedBoard {
        ExtractedBoard {
            name: "t".into(),
            nets: nets
                .iter()
                .map(|(id, n)| Net {
                    id: *id,
                    name: (*n).to_string(),
                })
                .collect(),
            components,
        }
    }

    fn run(b: &ExtractedBoard) -> NetLintReport {
        contention_lint(b, &ModelLibrary::builtin())
    }

    /// Two modelled push-pull outputs on one net fight and must fire: a 74HC08
    /// AND-gate output (pad 3 = `y1`) and a 74HC32 OR-gate output (pad 3 =
    /// `y1`) both land on `SHARED`. Neither part has an output enable.
    #[test]
    fn two_pushpull_outputs_on_one_net_fire() {
        let b = board(
            &[(1, "A"), (2, "B"), (3, "SHARED"), (4, "C"), (5, "D")],
            vec![
                comp("U1", "74HC08", &[("1", 1), ("2", 2), ("3", 3)]),
                comp("U2", "74HC32", &[("1", 4), ("2", 5), ("3", 3)]),
            ],
        );
        let r = run(&b);
        assert_eq!(
            r.findings.len(),
            1,
            "one contention finding: {:?}",
            r.findings
        );
        let f = &r.findings[0];
        assert_eq!(f.check, LintCheck::OutputContention);
        assert_eq!(f.severity, Severity::High);
        assert_eq!(f.refs, vec!["U1".to_string(), "U2".to_string()]);
        assert_eq!(f.nets, vec!["SHARED".to_string()]);
        assert!(f.message.contains("hauksbee models resolve"));
        assert!(f.message.contains("co-sim"));
    }

    /// THE FIELD CASE, as reported: a 74HC08 whose gate-4 output (pad 11 =
    /// `y4`) was mis-mapped onto net STROBE, which an ATmega328P also drives as
    /// a GPIO output (PB1, pad 13 on the TQFP-32) because the firmware set DDRB
    /// bit 1. Two push-pull drivers, one net, no series element.
    ///
    /// This check does NOT fire on it, and that is a documented limitation, not
    /// an oversight: at lint time there is no firmware, the binder stamps every
    /// MCU GPIO driver disabled (high-impedance), and a logic output sharing a
    /// net with an MCU pad is indistinguishable from the single most common
    /// healthy topology there is, a gate output feeding an MCU input. Firing
    /// here would put a High finding on virtually every board that wires logic
    /// to an MCU, which the zero-false-positive gate forbids. The model-to-MCU
    /// half of the field case is therefore out of STATIC reach; it is caught
    /// at co-sim time, where the scheduler learns real pin directions from the
    /// firmware's DDR writes (`Scheduler::detect_driver_contention`, which has
    /// its own tests). What the static check DOES catch is the other face
    /// of the same field failure: the mis-mapped model output landing on a net
    /// with any OTHER modelled push-pull output (see
    /// `two_pushpull_outputs_on_one_net_fire`).
    ///
    /// If this assertion ever starts failing because the check learned to fire
    /// here, that is a deliberate design change: re-read the paragraph above
    /// and be sure the false-positive argument has actually been answered.
    #[test]
    fn field_case_model_vs_mcu_gpio_is_out_of_static_reach() {
        let b = board(
            &[(1, "A4"), (2, "B4"), (3, "STROBE"), (4, "VCC"), (5, "GND")],
            vec![
                comp("U1", "74HC08", &[("1", 1), ("2", 2), ("11", 3)]),
                // The ATmega328P resolves to an MCU-kind model; its pad 13
                // (PB1) sits on STROBE. Direction is firmware state the lint
                // cannot see.
                comp("U2", "ATmega328P-AU", &[("13", 3), ("4", 4), ("3", 5)]),
            ],
        );
        let r = run(&b);
        assert!(
            r.findings.is_empty(),
            "model-vs-MCU contention is firmware-dependent and must not fire \
             statically: {:?}",
            r.findings
        );
    }

    /// An output feeding an input is the normal case and must stay silent: the
    /// 74HC08's `y1` (pad 3) drives the 74HC32's `a1` (pad 1).
    #[test]
    fn output_into_input_is_silent() {
        let b = board(
            &[(1, "A"), (2, "B"), (3, "MID"), (4, "C"), (5, "OUT")],
            vec![
                comp("U1", "74HC08", &[("1", 1), ("2", 2), ("3", 3)]),
                comp("U2", "74HC32", &[("1", 3), ("2", 4), ("3", 5)]),
            ],
        );
        assert!(run(&b).findings.is_empty(), "{:?}", run(&b).findings);
    }

    /// A 3-state part sharing a net is the intended arrangement, not a fault: the
    /// 74HC125's `y1`/`y2` (pads 3 and 6) are tri-stated by their own `oe_n_*`,
    /// so two of them on one bus net must stay silent.
    #[test]
    fn tristate_buffer_outputs_sharing_a_net_are_silent() {
        let b = board(
            &[(1, "A1"), (2, "A2"), (3, "BUS"), (4, "OE1"), (5, "OE2")],
            vec![
                comp("U1", "74HC125", &[("1", 4), ("2", 1), ("3", 3)]),
                comp("U2", "74HC125", &[("4", 5), ("5", 2), ("6", 3)]),
            ],
        );
        assert!(run(&b).findings.is_empty(), "{:?}", run(&b).findings);
    }

    /// The 74HC595's `qa..qh` are OE-gated, so two shift registers whose Q pins
    /// share a net are silent; but `qh_serial` (pad 9) is NOT OE-gated, so two of
    /// those on one net is a genuine push-pull fight and does fire. This pins the
    /// tri-state exclusion to the exact roles the db marks, not to the whole part.
    #[test]
    fn tristate_exclusion_is_per_role_not_per_part() {
        let shared_q = board(
            &[(1, "QSHARE")],
            vec![
                comp("U1", "74HC595", &[("15", 1)]), // qa
                comp("U2", "74HC595", &[("15", 1)]), // qa
            ],
        );
        assert!(
            run(&shared_q).findings.is_empty(),
            "OE-gated Q outputs are tri-stateable: {:?}",
            run(&shared_q).findings
        );

        let shared_serial = board(
            &[(1, "QHS")],
            vec![
                comp("U1", "74HC595", &[("9", 1)]), // qh_serial, never OE-gated
                comp("U2", "74HC595", &[("9", 1)]),
            ],
        );
        assert_eq!(
            run(&shared_serial).findings.len(),
            1,
            "the non-tri-stated cascade tap is push-pull"
        );
    }

    /// Two output pads of ONE part on a net is not an inter-part fight.
    #[test]
    fn same_part_twice_on_a_net_is_silent() {
        let b = board(
            &[(1, "A"), (2, "B"), (3, "PAR")],
            // 74HC08 y1 (pad 3) and y2 (pad 6) paralleled onto one net.
            vec![comp(
                "U1",
                "74HC08",
                &[("1", 1), ("2", 2), ("3", 3), ("6", 3)],
            )],
        );
        assert!(run(&b).findings.is_empty(), "{:?}", run(&b).findings);
    }

    /// A DNP part is not assembled, so it contributes no driver.
    #[test]
    fn dnp_part_contributes_no_driver() {
        let mut u2 = comp("U2", "74HC32", &[("3", 3)]);
        u2.dnp = true;
        let b = board(
            &[(3, "SHARED")],
            vec![comp("U1", "74HC08", &[("3", 3)]), u2],
        );
        assert!(run(&b).findings.is_empty(), "{:?}", run(&b).findings);
    }

    /// Ground is not a net to contend over (a mis-mapped pad landing on GND is
    /// another check's problem), and neither is KiCad's unconnected placeholder.
    #[test]
    fn ground_and_unconnected_nets_are_skipped() {
        let b = board(
            &[(1, "GND"), (2, "unconnected-(U3-Pad3)")],
            vec![
                comp("U1", "74HC08", &[("3", 1)]),
                comp("U2", "74HC32", &[("3", 1)]),
                comp("U3", "74HC08", &[("6", 2)]),
                comp("U4", "74HC32", &[("6", 2)]),
            ],
        );
        assert!(run(&b).findings.is_empty(), "{:?}", run(&b).findings);
    }

    /// A part with no model contributes nothing: the check speaks only about
    /// pins it actually bound.
    #[test]
    fn unmodelled_part_contributes_no_driver() {
        let b = board(
            &[(3, "SHARED")],
            vec![
                comp("U1", "74HC08", &[("3", 3)]),
                comp("U2", "SOME-UNKNOWN-PART-XYZ", &[("3", 3)]),
            ],
        );
        assert!(run(&b).findings.is_empty(), "{:?}", run(&b).findings);
    }

    /// The scan's evidence trail reports how far each part got, so the corpus
    /// calibration can assert engagement instead of counting on faith: here the
    /// 74HC125 presents 4 outputs, all tri-stateable, and contributes 0
    /// push-pull pins, while the 74HC08 presents 4, none tri-stateable, with 1
    /// on an eligible net, 1 on ground, and 2 pads absent from the footprint.
    #[test]
    fn scan_reports_the_evidence_trail() {
        let b = board(
            &[(1, "SIG"), (2, "GND"), (3, "BUS")],
            vec![
                comp("U1", "74HC08", &[("3", 1), ("6", 2)]),
                comp("U2", "74HC125", &[("3", 3), ("6", 3), ("8", 3), ("11", 3)]),
            ],
        );
        let s = scan(&b, &ModelLibrary::builtin());
        assert_eq!(s.parts.len(), 2);
        let u1 = s.parts.iter().find(|p| p.reference == "U1").unwrap();
        assert_eq!(
            (u1.output_roles, u1.tristateable_roles, u1.pushpull_pins),
            (4, 0, 1)
        );
        assert_eq!((u1.pads_absent, u1.pads_on_excluded_nets), (2, 1));
        let u2 = s.parts.iter().find(|p| p.reference == "U2").unwrap();
        assert_eq!(
            (u2.output_roles, u2.tristateable_roles, u2.pushpull_pins),
            (4, 4, 0)
        );
        assert_eq!(s.pushpull_driver_pins(), 1);
        assert_eq!(s.output_roles_presented(), 8);
    }
}
