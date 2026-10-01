//! Power-transfer invariants over every builtin power card.
//!
//! Every `vreg` and `analog_switch` card in `hauksbee-models/db` is bound on
//! the same three-part board: an ideal supply on a supply-named input rail,
//! the part, and a resistive load on a supply-named output rail. The output
//! name is the point: an ideal auto-rail landing there would fight the part
//! and carry its load, which is how a buck once read -1.8 A of input current.
//! For each card, after the board settles:
//!
//! - the output regulates (or passes through) within 2%;
//! - input power covers output power (`Pin >= Pout`), to the card's stated
//!   efficiency within 1% where it states one, and for a linear part the
//!   input current is the load's plus the card's `iq_a`, within 1%;
//! - the input current is positive and the input rail's supply carries it;
//! - no other ideal source fights: every supply leg but the input's carries
//!   under 1% of the load current.
//!
//! The same board idea covers the active models that deliver current from an
//! output: every builtin op-amp and every digital part with outputs is bound
//! on a supply-named rail, its outputs loaded by 1 k to ground, and the
//! current each output delivers must appear in the part's supply pin within
//! 1% (see `opamp_output_current_is_drawn_from_the_supply_pins` and
//! `digital_output_current_is_drawn_from_the_supply_pin`). A control run with
//! the routing removed must fail the same check, and a buck -> LDO -> op-amp
//! chain must still power up.
//!
//! The cards are enumerated from the db and classified by what the binder
//! stamped, never hand-picked, so a new card joins the matrix by existing.
//! A path on the known-gap list is asserted to still fail, so a fix landing
//! turns this test red until the list is updated.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use hauksbee_engine::binder::bind_board_with;
use hauksbee_engine::power_rail_voltage;
use hauksbee_engine::scheduler::Scheduler;
use hauksbee_engine::CustomRegistry;
use hauksbee_extract::{Component, ExtractedBoard, Pin};
use hauksbee_ir::{Device, SourceKind};
use hauksbee_models::{ModelLibrary, SourceLayer};
use hauksbee_solve::SolverOptions;

use crate::support::passive_pin as pin;

/// Binder paths whose cards are expected to fail, with why. A path leaves
/// this list when its fix lands, and the test says so.
const KNOWN_GAP_PATHS: &[(&str, &str)] = &[(
    "no-output",
    "an identity-only card (bq24075_identity) states no output voltage, reference or \
         converter, so no power stage is stamped and the part's rail is the board's to supply",
)];

/// The relative tolerance of every balance check.
const TOL: f64 = 0.01;

const RES: &str = "Resistor_SMD:R_0402_1005Metric";
const CAP: &str = "Capacitor_SMD:C_1206_3216Metric";
const IND: &str = "Inductor_SMD:L_1210_3225Metric";

/// One db card, kept as raw TOML so its probe copy differs only in `match`.
struct Card {
    id: String,
    kind: String,
    table: toml::Table,
}

impl Card {
    fn pins(&self) -> Vec<(String, String)> {
        let pins = self.table.get("pins").and_then(toml::Value::as_table);
        pins.into_iter()
            .flatten()
            .filter_map(|(pad, r)| Some((pad.clone(), r.as_str()?.to_string())))
            .collect()
    }

    fn param(&self, key: &str) -> Option<f64> {
        num(self.table.get("params")?.get(key)?)
    }

    fn converter(&self) -> Option<&toml::Table> {
        self.table.get("behavioral")?.get("converter")?.as_table()
    }

    /// The roles `[models.logic]` names as `key` (`inputs` or `outputs`).
    fn logic(&self, key: &str) -> Vec<String> {
        let list = (self.table.get("logic")).and_then(|l| l.get(key));
        let list = list.and_then(toml::Value::as_array).into_iter().flatten();
        list.filter_map(|r| r.as_str().map(String::from)).collect()
    }

    fn has_role(&self, role: &str) -> bool {
        self.pins().iter().any(|(_, r)| r == role)
    }
}

fn float(t: &toml::Table, key: &str) -> Option<f64> {
    num(t.get(key)?)
}

/// A TOML number, float or integer.
fn num(v: &toml::Value) -> Option<f64> {
    let int = || {
        v.as_integer()
            .and_then(|i| i32::try_from(i).ok())
            .map(f64::from)
    };
    v.as_float().or_else(int)
}

fn text<'a>(t: &'a toml::Table, key: &str) -> Option<&'a str> {
    t.get(key)?.as_str()
}

/// Every power card in the builtin db, in file then entry order.
fn power_cards() -> Vec<Card> {
    cards_of(|k, _| k == "vreg" || k == "analog_switch")
}

/// Every builtin card `keep` accepts (by kind and table), in file then entry
/// order.
fn cards_of(keep: impl Fn(&str, &toml::Table) -> bool) -> Vec<Card> {
    let db = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../hauksbee-models/db");
    let mut files: Vec<_> = std::fs::read_dir(&db)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "toml"))
        .collect();
    files.sort();
    let mut cards = Vec::new();
    for f in files {
        let doc: toml::Table = toml::from_str(&std::fs::read_to_string(&f).unwrap()).unwrap();
        let models = doc.get("models").and_then(toml::Value::as_array);
        for m in models.into_iter().flatten() {
            let Some(table) = m.as_table() else { continue };
            let kind = text(table, "kind").unwrap_or_default().to_string();
            if keep(&kind, table) {
                let id = text(table, "id").unwrap().to_string();
                cards.push(Card {
                    id,
                    kind,
                    table: table.clone(),
                });
            }
        }
    }
    cards
}

/// The builtin library plus one probe copy of each card, matched only by
/// the value `PROBE<i>`, so a board can name exactly that card.
fn probe_library(cards: &[Card], tag: &str) -> ModelLibrary {
    let dir = hauksbee_testkit::fresh_scratch_dir("power_invariants", tag);
    let mut doc = toml::Table::new();
    let probes = cards.iter().enumerate().map(|(i, c)| {
        let mut t = c.table.clone();
        let mut m = toml::Table::new();
        m.insert("value_re".into(), format!("^PROBE{i}$").into());
        t.insert("match".into(), m.into());
        toml::Value::Table(t)
    });
    doc.insert("models".into(), toml::Value::Array(probes.collect()));
    std::fs::write(dir.join("probes.toml"), toml::to_string(&doc).unwrap()).unwrap();
    let mut lib = ModelLibrary::builtin();
    // Builtin cards, loaded as builtin: the user layers validate more strictly.
    let errors = lib.load_dir_layer(&dir, SourceLayer::Builtin);
    assert!(errors.is_empty(), "probe cards failed to load: {errors:?}");
    lib
}

/// A board under construction: named nets and parts.
#[derive(Default)]
struct Rig {
    nets: Vec<(i64, String)>,
    parts: Vec<Component>,
    /// Resistors to ground off the output side: (name, net, ohms).
    loads: Vec<(String, String, f64)>,
}

impl Rig {
    fn net(&mut self, name: &str) -> i64 {
        if let Some((id, _)) = self.nets.iter().find(|(_, n)| n == name) {
            return *id;
        }
        let id = i64::try_from(self.nets.len()).unwrap() + 1;
        self.nets.push((id, name.to_string()));
        id
    }

    fn two(&mut self, reference: &str, value: &str, footprint: &str, a: &str, b: &str) {
        let (a, b) = (self.net(a), self.net(b));
        self.parts.push(component(
            reference,
            value,
            footprint,
            vec![pin("1", a), pin("2", b)],
        ));
    }

    fn load(&mut self, reference: &str, net: &str, ohms: f64) {
        self.two(reference, &format!("{ohms}R"), RES, net, "GND");
        self.loads.push((reference.into(), net.into(), ohms));
    }

    fn board(&self) -> ExtractedBoard {
        let nets = self
            .nets
            .iter()
            .map(|(id, n)| crate::support::net(*id, n.clone()));
        ExtractedBoard {
            name: "power_invariants".into(),
            nets: nets.collect(),
            components: self.parts.clone(),
        }
    }
}

fn component(reference: &str, value: &str, footprint: &str, pins: Vec<Pin>) -> Component {
    crate::support::component(
        reference.into(),
        value.into(),
        String::new(),
        footprint.into(),
        pins,
    )
}

/// A supply-named net for `volts` that the binder reads back as `volts`.
fn rail(volts: f64) -> String {
    let sign = if volts < 0.0 { '-' } else { '+' };
    let name = format!("{sign}{}V", volts.abs());
    let read = power_rail_voltage(&name);
    assert!(
        read.is_some_and(|r| (r - volts).abs() < 1e-6),
        "test rail name {name} reads {read:?}, not {volts} V"
    );
    name
}

/// What a card's board is expected to do.
enum Expect {
    /// A linear part: input current is the load's plus `iq`.
    Linear { iq: f64 },
    /// A converter: input power is output power over `eff`.
    Converter { eff: Option<f64> },
    /// A shunt reference fed through `feed` ohms from the input.
    Shunt { feed: f64 },
    /// A pass switch: input current is the load's.
    Switch,
}

struct Plan {
    rig: Rig,
    input: String,
    output: String,
    target: f64,
    expect: Expect,
}

/// The vreg board: input rail, the part, a load drawing about 100 mA (or a
/// quarter of a converter's current limit), with an LC after a converter's
/// switch node and a divider on a feedback pin.
fn vreg_plan(card: &Card, value: &str) -> Option<Plan> {
    let conv = card.converter();
    let shunt = card
        .table
        .get("params")
        .and_then(|p| p.get("shunt"))
        .and_then(toml::Value::as_bool)
        .unwrap_or(false);
    if shunt {
        return Some(shunt_plan(card, value));
    }
    let feedback = conv
        .and_then(|c| c.get("feedback"))
        .and_then(toml::Value::as_table);
    let vref = feedback
        .and_then(|f| float(f, "vref_v"))
        .or_else(|| conv.is_none().then(|| card.param("vref")).flatten());
    let fixed = conv.map_or_else(|| card.param("vout"), |c| float(c, "vout_setpoint"));
    let mut target = if vref.is_some() { 5.0 } else { fixed? };
    let dropout = card.param("dropout_v").unwrap_or(1.0);
    let topology = conv.and_then(|c| text(c, "topology")).unwrap_or("linear");
    let vin = match topology {
        "buck" => (2.0 * target).max(12.0),
        "boost" => (target / 2.0).ceil(),
        "buck_boost" => (0.8 * target).round(),
        _ => (target.abs() + dropout + 1.0).ceil().copysign(target),
    };
    let (input, output) = (rail(vin), rail(target));
    let mut rig = Rig::default();
    rig.net("GND");
    let (inp, outp) = (input.clone(), output.clone());
    let in_pin = conv.and_then(|c| text(c, "in_pin")).unwrap_or("in");
    let out_pin = conv.and_then(|c| text(c, "out_pin"));
    let (in_node, prog) = input_program(conv, &mut rig, &inp);
    // Each pin's net by role: power and enables on the input, sense and
    // feedback of a fixed part on the output, programming pins on a resistor.
    let net_of = |role: &str| -> Option<String> {
        Some(match role {
            r if r == in_pin || r == "senstop" => in_node.clone(),
            r if Some(r) == out_pin => "SW".to_string(),
            "gnd" | "gnd_ep" | "sensgnd" | "sensbot" => "GND".into(),
            "in" | "in2" | "vbus" | "pvin" | "sensvin" | "en" | "en1" | "en2" | "ce" | "shdn" => {
                inp.clone()
            }
            "fb" if vref.is_some() => "FB".into(),
            "out" | "out2" | "outs" | "sense" | "csp" | "csn" | "fb" => outp.clone(),
            "bst" => "BST".into(),
            "prog" | "iset" | "ilim" | "ilimit" => "PROG".into(),
            _ => return None,
        })
    };
    let pins = card
        .pins()
        .into_iter()
        .filter_map(|(pad, role)| Some(pin(&pad, rig.net(&net_of(&role)?))))
        .collect();
    rig.parts
        .push(component("U1", value, "Package_SO:SOIC-8", pins));
    if rig.nets.iter().any(|(_, n)| n == "PROG") {
        rig.two(&prog, "10k", RES, "PROG", "GND");
    }
    if rig.nets.iter().any(|(_, n)| n == "SW") {
        rig.two("L1", "10uH", IND, "SW", &outp);
    }
    if rig.nets.iter().any(|(_, n)| n == "BST") {
        rig.two("C3", "100nF", CAP, "BST", "SW");
    }
    rig.two("C1", "10uF", CAP, &inp, "GND");
    rig.two("C2", "22uF", CAP, &outp, "GND");
    if let Some(vref) = vref {
        // A grounded part (or a converter) holds FB at `vref`; a three-terminal
        // adjustable holds OUT `vref` above ADJ, so its divider is inverted.
        let k = target / vref - 1.0;
        let (top, bot) = if conv.is_some() || card.has_role("gnd") {
            (1e3 * k, 1e3)
        } else {
            (1e3, 1e3 * k)
        };
        rig.two("R2", &format!("{top}R"), RES, &outp, "FB");
        rig.load("R3", "FB", bot);
        // The ADJ pin's own current runs through the bottom resistor too.
        target += card.param("iadj_a").unwrap_or(0.0) * bot;
    }
    let limit = conv.and_then(|c| float(c, "iout_limit_a")).unwrap_or(0.4);
    rig.load("R1", &outp, target.abs() / limit.min(0.4) * 4.0);
    let iq = card.param("iq_a").filter(|_| card.has_role("gnd"));
    let expect = conv.map_or_else(
        || Expect::Linear {
            iq: iq.unwrap_or(0.0),
        },
        |c| Expect::Converter {
            eff: float(c, "efficiency"),
        },
    );
    Some(Plan {
        rig,
        input,
        output,
        target,
        expect,
    })
}

/// A converter's input-current program: its sense resistors, named as the
/// card names them, in series from `input` to the part's input node, and the
/// name of its programming resistor. Returns that node and that name.
fn input_program(conv: Option<&toml::Table>, rig: &mut Rig, input: &str) -> (String, String) {
    let program = conv
        .and_then(|c| c.get("iin_program"))
        .and_then(toml::Value::as_table);
    let senses: Vec<String> = program
        .and_then(|p| p.get("rsense_refs"))
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|r| r.as_str().map(String::from))
        .collect();
    let node = if senses.is_empty() { input } else { "PVIN" };
    for s in &senses {
        rig.two(s, "10m", RES, input, node);
    }
    let prog = program.and_then(|p| text(p, "prog_ref")).unwrap_or("RPROG");
    (node.to_string(), prog.to_string())
}

/// A shunt reference set to 5 V by its divider, fed from +12V through 330R,
/// loaded by 1k: it sinks the rest of the feed's 21 mA.
fn shunt_plan(card: &Card, value: &str) -> Plan {
    let (input, output) = (rail(12.0), rail(5.0));
    let mut rig = Rig::default();
    rig.net("GND");
    let mut pins = Vec::new();
    for (pad, role) in card.pins() {
        let net = match role.as_str() {
            "out" => output.as_str(),
            "fb" => "FB",
            "gnd" => "GND",
            _ => continue,
        };
        let id = rig.net(net);
        pins.push(pin(&pad, id));
    }
    rig.parts
        .push(component("U1", value, "Package_TO_SOT_SMD:SOT-23", pins));
    let vref = card.param("vref").unwrap_or(2.495);
    let bot = 10e3;
    rig.two(
        "R2",
        &format!("{}R", bot * (5.0 / vref - 1.0)),
        RES,
        &output,
        "FB",
    );
    rig.load("R3", "FB", bot);
    rig.two("R4", "330R", RES, &input, &output);
    rig.load("R1", &output, 1e3);
    Plan {
        rig,
        input,
        output,
        target: 5.0,
        expect: Expect::Shunt { feed: 330.0 },
    }
}

/// A pass switch from `+2.5V` onto a supply-named `+2.5V_SW`, its logic
/// supply on `+3.3V`, enabled by `on` ("+3.3V" or "GND", for either polarity), and
/// loaded at a hundred times its on-resistance.
fn switch_plan(card: &Card, value: &str, on: &str) -> Plan {
    let (input, output) = ("+2.5V".to_string(), "+2.5V_SW".to_string());
    assert_eq!(power_rail_voltage(&output), Some(2.5));
    let mut rig = Rig::default();
    rig.net("GND");
    let mut pins = Vec::new();
    let roles = card.pins();
    let has = |r: &str| roles.iter().any(|(_, x)| x == r);
    let (a, b) = if has("in_out_a") {
        ("in_out_b", "in_out_a")
    } else if has("in_out_1a") {
        ("in_out_1a", "in_out_1b")
    } else if has("com_1") {
        ("com_1", "s0_1")
    } else {
        ("com", "s0")
    };
    let mut second = None;
    for (pad, role) in &roles {
        let net = match role.as_str() {
            r if r == a => input.clone(),
            r if r == b => output.clone(),
            "s1" | "s1_1" => {
                second = Some("OUT2");
                "OUT2".to_string()
            }
            "vcc" | "vcca" | "vccb" | "vref1" | "vref2" => "+3.3V".into(),
            "ctrl" | "ctrl_1" => on.into(),
            "vss" | "gnd" | "oe_n" | "ctrl_2" | "ctrl_3" | "ctrl_4" => "GND".into(),
            "ilim" | "iset" => "ILIM".into(),
            _ => continue,
        };
        let id = rig.net(&net);
        pins.push(pin(pad, id));
    }
    rig.parts
        .push(component("U1", value, "Package_TO_SOT_SMD:SOT-23-6", pins));
    if rig.nets.iter().any(|(_, n)| n == "ILIM") {
        rig.two("R9", "10k", RES, "ILIM", "GND");
    }
    let ron = card.param("ron").unwrap_or(1.0);
    let ohms = (100.0 * ron).max(10.0);
    rig.load("R1", &output, ohms);
    if let Some(n) = second {
        rig.load("R2", n, ohms);
    }
    Plan {
        rig,
        input,
        output,
        target: 2.5,
        expect: Expect::Switch,
    }
}

/// How the binder stamped U1, read off the bound circuit.
fn path_of(devices: &[Device]) -> &'static str {
    let named = |p: &str| devices.iter().any(|d| d.name() == p);
    if named("Vbeh_U1_conv") {
        "converter"
    } else if named("Gka_U1") {
        "shunt"
    } else if named("Vref_U1") {
        "adjustable"
    } else if let Some(Device::Vsource { kind, .. }) =
        devices.iter().find(|d| d.name() == "Vreg_U1")
    {
        if matches!(kind, SourceKind::Dc(v) if *v < 0.0) {
            "fixed-negative"
        } else {
            "fixed"
        }
    } else if devices
        .iter()
        .any(|d| matches!(d, Device::VSwitch { name, .. } if name.contains("U1")))
    {
        "switch"
    } else {
        "unbound"
    }
}

/// One card's run: its binder path and every invariant it broke.
struct Outcome {
    path: &'static str,
    broken: Vec<String>,
}

fn run(plan: &Plan, lib: &ModelLibrary, model_id: &str) -> Outcome {
    let board = plan.rig.board();
    let bound = bind_board_with(&board, lib, &CustomRegistry::new());
    let path = path_of(&bound.circuit.devices);
    let mut broken = Vec::new();
    let row = bound.report.row("U1");
    if row.and_then(|r| r.model_id.as_deref()) != Some(model_id) {
        broken.push(format!("U1 bound {:?}", row.map(|r| &r.model_id)));
    }
    if path == "unbound" {
        let why = row.and_then(|r| r.warning.clone()).unwrap_or_default();
        broken.push(format!("no power stage stamped: {why}"));
        return Outcome { path, broken };
    }
    let Ok(mut sched) = Scheduler::new(bound, None, SolverOptions::default()) else {
        broken.push("scheduler did not build".into());
        return Outcome { path, broken };
    };
    for _ in 0..20 {
        sched.step(1e-4);
    }
    if !sched.analog_valid() {
        broken.push(format!("solve failed: {:?}", sched.failed_windows()));
        return Outcome { path, broken };
    }
    let v = |n: &str| sched.net_voltage(n).unwrap_or(0.0);
    let (vin, vo) = (v(&plan.input), v(&plan.output));
    // Every load and divider resistor returns to ground, so their current
    // is all the current the board draws past the part.
    let to_ground: f64 = plan.rig.loads.iter().map(|(_, n, r)| v(n) / r).sum();
    let p_out: f64 = plan
        .rig
        .loads
        .iter()
        .map(|(_, n, r)| v(n).powi(2) / r)
        .sum();
    let supplies = sched.supply_states();
    let iin = supplies.get(&plan.input).map_or(f64::NAN, |s| s.1);
    let p_in = vin * iin;
    if (vo - plan.target).abs() > 0.02 * plan.target.abs() {
        broken.push(format!("output {vo:.4} V, want {} V", plan.target));
    }
    if iin.is_nan() || iin * vin.signum() <= 0.0 {
        broken.push(format!(
            "input current {iin:.4e} A is not drawn from {}",
            plan.input
        ));
    }
    if p_in < p_out * (1.0 - TOL) {
        broken.push(format!("Pin {p_in:.4e} W < Pout {p_out:.4e} W"));
    }
    let want = match plan.expect {
        Expect::Linear { iq } => Some(("input current", iin.abs(), to_ground.abs() + iq)),
        Expect::Converter { eff } => eff.map(|e| ("input power", p_in, p_out / e)),
        Expect::Shunt { feed } => Some(("feed current", iin, (vin - vo) / feed)),
        Expect::Switch => Some(("input current", iin, to_ground)),
    };
    if let Some((what, got, want)) = want {
        if (got / want - 1.0).abs() > TOL {
            broken.push(format!("{what} {got:.5e}, want {want:.5e}"));
        }
    }
    for (net, (_, i, _)) in &supplies {
        if net != &plan.input && i.abs() > TOL * to_ground.abs().max(1e-4) {
            broken.push(format!(
                "supply on {net} fights the part, carrying {i:.4e} A"
            ));
        }
    }
    Outcome { path, broken }
}

/// The matrix: every power card's binder path and verdict, failing on any
/// card outside a known-gap path that breaks an invariant, and on any card
/// inside one that no longer does.
#[test]
fn every_power_card_balances_power_and_owns_its_rail() {
    let cards = power_cards();
    assert!(cards.len() > 60, "only {} power cards found", cards.len());
    let lib = probe_library(&cards, "probes");
    let mut table = String::new();
    let mut failures = Vec::new();
    let mut counts: BTreeMap<(&str, &str), usize> = BTreeMap::new();
    for (i, card) in cards.iter().enumerate() {
        let value = format!("PROBE{i}");
        let outcome = if card.kind == "analog_switch" {
            let high = run(&switch_plan(card, &value, "+3.3V"), &lib, &card.id);
            if high.broken.iter().any(|b| b.starts_with("output")) {
                run(&switch_plan(card, &value, "GND"), &lib, &card.id)
            } else {
                high
            }
        } else if let Some(plan) = vreg_plan(card, &value) {
            run(&plan, &lib, &card.id)
        } else {
            Outcome {
                path: "no-output",
                broken: vec!["card states no output voltage or reference".into()],
            }
        };
        let gap = KNOWN_GAP_PATHS.iter().find(|(p, _)| *p == outcome.path);
        let verdict = match (gap, outcome.broken.is_empty()) {
            (None, true) => "pass",
            (Some(_), false) => "known-gap",
            (None, false) => {
                failures.push(format!(
                    "{} [{}]: {}",
                    card.id,
                    outcome.path,
                    outcome.broken.join("; ")
                ));
                "FAIL"
            }
            (Some(_), true) => {
                failures.push(format!(
                    "{} [{}] now passes: take '{}' off KNOWN_GAP_PATHS",
                    card.id, outcome.path, outcome.path
                ));
                "FIXED"
            }
        };
        *counts.entry((outcome.path, verdict)).or_default() += 1;
        let _ = writeln!(
            table,
            "{:<24} {:<14} {:<9} {}",
            card.id,
            outcome.path,
            verdict,
            outcome.broken.join("; ")
        );
    }
    eprintln!("{table}\n{counts:?}");
    assert!(
        failures.is_empty(),
        "{} cards broke an invariant:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// The rig's own arithmetic, so a broken harness cannot pass every card:
/// a plain resistive divider on +5V is no regulator and must fail.
#[test]
fn the_harness_fails_a_board_with_no_power_stage() {
    let mut rig = Rig::default();
    rig.net("GND");
    rig.load("R1", "+5V", 100.0);
    let plan = Plan {
        rig,
        input: "+12V".into(),
        output: "+5V".into(),
        target: 5.0,
        expect: Expect::Linear { iq: 0.0 },
    };
    let outcome = run(&plan, &ModelLibrary::builtin(), "none");
    assert_eq!(outcome.path, "unbound");
    assert!(!outcome.broken.is_empty());
}

// ---------------------------------------------------------------------------
// Active outputs: the current a part delivers is drawn from its supply pins.
// ---------------------------------------------------------------------------

/// The load on every output, ohms.
const LOAD: f64 = 1e3;
/// Below this an output delivers nothing worth checking, amps.
const MIN_IOUT: f64 = 5e-4;

/// Settle `rig` and read every net in `nets` plus the supply states.
fn settle(
    rig: &Rig,
    lib: &ModelLibrary,
    edit: impl FnOnce(&mut hauksbee_engine::binder::BoundBoard),
) -> Option<Scheduler> {
    let mut bound = bind_board_with(&rig.board(), lib, &CustomRegistry::new());
    edit(&mut bound);
    let mut sched = Scheduler::new(bound, None, SolverOptions::default()).ok()?;
    for _ in 0..20 {
        sched.step(1e-4);
    }
    sched.analog_valid().then_some(sched)
}

/// One op-amp scenario: the net the op-amp's `V-` pin sits on, and the signed
/// voltage on every non-inverting input (the inverting input is on ground).
struct OpampRun {
    vss: Option<f64>,
    drive: f64,
    /// Supply current the part is expected to draw beyond its outputs.
    iq: f64,
}

/// What an op-amp board measured: the current its outputs delivered into
/// ground, summed by sign, and what each supply pin carried.
#[derive(Debug)]
struct Delivered {
    sourced: f64,
    sunk: f64,
    hi: f64,
    lo: Option<f64>,
}

fn opamp_rig(card: &Card, value: &str, run: &OpampRun) -> (Rig, Vec<String>) {
    let mut rig = Rig::default();
    rig.net("GND");
    let mut pins = Vec::new();
    let mut outs = Vec::new();
    for (pad, role) in card.pins() {
        let net = match role.as_str() {
            r if r.starts_with("in_plus") => rail(run.drive),
            r if r.starts_with("in_minus") || r.starts_with("ref") || r == "gnd" => "GND".into(),
            "vcc" | "vs" | "vdd" => rail(5.0),
            "vss" => run.vss.map_or_else(|| "GND".into(), rail),
            r if r.starts_with("out") => {
                let net = format!("OUT_{r}");
                outs.push(net.clone());
                net
            }
            _ => continue,
        };
        let id = rig.net(&net);
        pins.push(pin(&pad, id));
    }
    rig.parts
        .push(component("U1", value, "Package_SO:SOIC-14", pins));
    for (i, net) in outs.iter().enumerate() {
        rig.load(&format!("RL{i}"), net, LOAD);
    }
    (rig, outs)
}

/// Bind and settle one op-amp scenario; `strip` removes the supply routing
/// (the control).
fn opamp_delivered(
    card: &Card,
    lib: &ModelLibrary,
    value: &str,
    run: &OpampRun,
    strip: bool,
) -> Result<Delivered, String> {
    let (rig, outs) = opamp_rig(card, value, run);
    let edit = |bound: &mut hauksbee_engine::binder::BoundBoard| {
        for d in bound.circuit.devices.iter_mut().filter(|_| strip) {
            if let Device::OpAmp { supply, .. } = d {
                *supply = hauksbee_ir::Supply::default();
            }
        }
    };
    let sched = settle(&rig, lib, edit).ok_or("solve failed")?;
    let currents: Vec<f64> = (outs.iter())
        .map(|n| sched.net_voltage(n).unwrap_or(0.0) / LOAD)
        .collect();
    let states = sched.supply_states();
    let pin = |v: f64| states.get(&rail(v)).map_or(f64::NAN, |s| s.1);
    Ok(Delivered {
        sourced: currents.iter().filter(|i| **i > 0.0).sum(),
        sunk: currents.iter().filter(|i| **i < 0.0).sum(),
        hi: pin(5.0),
        lo: run.vss.map(pin),
    })
}

/// Every broken invariant of one op-amp measurement, empty when it holds.
/// The positive pin carries what the outputs source plus the card's `iq`; the
/// negative pin, when it is a rail, carries what they sink plus `iq`, against
/// its sign.
fn opamp_broken(d: &Delivered, iq: f64) -> Vec<String> {
    let mut broken = Vec::new();
    let near = |got: f64, want: f64| (got - want).abs() <= TOL * want.abs().max(MIN_IOUT * 0.01);
    if !near(d.hi, d.sourced + iq) {
        broken.push(format!(
            "V+ carries {:.5e} A, outputs source {:.5e} A (+ iq {iq:.1e})",
            d.hi, d.sourced
        ));
    }
    if let Some(lo) = d.lo {
        // The negative rail's source delivers current out of its negative
        // pin, so a part sinking `I` into V- reads `-I` there.
        if !near(-lo, iq - d.sunk) {
            broken.push(format!(
                "V- carries {lo:.5e} A, outputs sink {:.5e} A",
                d.sunk
            ));
        }
    }
    broken
}

/// Every builtin op-amp card: driven positive on a single supply and, where
/// the part has a `V-` pin, negative on split supplies. The positive pin
/// carries the sourced current, the negative pin the sunk current, and a
/// control with the routing stripped fails the same check.
#[test]
fn opamp_output_current_is_drawn_from_the_supply_pins() {
    let cards = cards_of(|k, _| k == "opamp");
    assert!(cards.len() >= 30, "only {} op-amp cards", cards.len());
    let lib = probe_library(&cards, "opamps");
    let mut failures = Vec::new();
    let mut unexercised = Vec::new();
    for (i, card) in cards.iter().enumerate() {
        let value = format!("PROBE{i}");
        let iq = card.param("iq_a").unwrap_or(0.0);
        let mut exercised = false;
        let mut scenarios = vec![OpampRun {
            vss: None,
            drive: 0.1,
            iq,
        }];
        if card.has_role("vss") {
            scenarios.push(OpampRun {
                vss: Some(-5.0),
                drive: -0.1,
                iq,
            });
        }
        for run in &scenarios {
            let got = opamp_delivered(card, &lib, &value, run, false);
            let d = match got {
                Ok(d) => d,
                Err(e) => {
                    failures.push(format!("{} (vss {:?}): {e}", card.id, run.vss));
                    continue;
                }
            };
            let delivering = d.sourced.max(-d.sunk) >= MIN_IOUT;
            exercised |= delivering;
            // Single supply sinks nothing past ground; a split supply's
            // positive pin is checked as well, so a sunk output cannot pass.
            let mut broken = opamp_broken(&d, run.iq);
            if run.vss.is_none() && d.sunk != 0.0 {
                broken.push(format!("sinks {:.3e} A on a single supply", d.sunk));
            }
            if !broken.is_empty() {
                failures.push(format!(
                    "{} (vss {:?}): {}",
                    card.id,
                    run.vss,
                    broken.join("; ")
                ));
            }
            // The control: without the routing the same measurement must
            // break, so a check that cannot fail cannot pass a card.
            if delivering {
                let bare = opamp_delivered(card, &lib, &value, run, true).unwrap();
                if opamp_broken(&bare, run.iq).is_empty() {
                    failures.push(format!(
                        "{} (vss {:?}): control with no supply routing still passes: {bare:?}",
                        card.id, run.vss
                    ));
                }
            }
        }
        if !exercised {
            unexercised.push(card.id.clone());
        }
    }
    eprintln!("op-amp cards never delivering {MIN_IOUT} A: {unexercised:?}");
    assert!(unexercised.is_empty(), "inert scenarios: {unexercised:?}");
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// The card's quiescent current is the part's own draw: a probe op-amp with
/// `iq_a = 2 mA` shows Iout + 2 mA on V+ (and returns it through V- on split
/// supplies), a card without one shows exactly Iout.
#[test]
fn opamp_quiescent_current_is_drawn_when_the_card_states_it() {
    let mut card = cards_of(|k, t| k == "opamp" && text(t, "id") == Some("lm358")).remove(0);
    let params = card
        .table
        .get_mut("params")
        .and_then(toml::Value::as_table_mut);
    params.unwrap().insert("iq_a".into(), 2e-3.into());
    let lib = probe_library(std::slice::from_ref(&card), "opamp_iq");
    let stated = card;
    let run = OpampRun {
        vss: None,
        drive: 0.1,
        iq: 2e-3,
    };
    let d = opamp_delivered(&stated, &lib, "PROBE0", &run, false).unwrap();
    assert!(d.sourced > MIN_IOUT, "{d:?}");
    let broken = opamp_broken(&d, run.iq);
    assert!(broken.is_empty(), "{broken:?} {d:?}");
    // A card with no `iq_a` draws none, rather than an invented figure.
    assert!(
        opamp_broken(&d, 0.0).len() == 1,
        "the 2 mA must be visible: {d:?}"
    );
}

/// One logic-input pattern for a digital part's board.
#[derive(Clone, Copy, Debug)]
enum Drive {
    AllLow,
    AllHigh,
    /// Active-low inputs (`*_n`) low, the rest high.
    EnableLowDataHigh,
    /// Active-low inputs high, the rest low.
    EnableHighDataLow,
    /// Active-low inputs low, the rest alternating high and low (an XOR).
    EnableLowDataAlternating,
}

impl Drive {
    const ALL: [Self; 5] = [
        Self::AllLow,
        Self::AllHigh,
        Self::EnableLowDataHigh,
        Self::EnableHighDataLow,
        Self::EnableLowDataAlternating,
    ];

    fn high(self, role: &str, index: usize) -> bool {
        let active_low = role.contains("_n") || role.starts_with("oe") || role.starts_with("pre");
        match self {
            Self::AllLow => false,
            Self::AllHigh => true,
            Self::EnableLowDataHigh => !active_low,
            Self::EnableHighDataLow => active_low,
            Self::EnableLowDataAlternating => !active_low && index.is_multiple_of(2),
        }
    }
}

/// A digital part on `+3.3V`, inputs per `drive`, every output loaded by
/// `ohms` to ground (`None`: unloaded). Returns the rig and the output nets.
fn digital_rig(card: &Card, value: &str, drive: Drive, ohms: Option<f64>) -> (Rig, Vec<String>) {
    let outputs = card.logic("outputs");
    let inputs = card.logic("inputs");
    let mut rig = Rig::default();
    rig.net("GND");
    let mut pins = Vec::new();
    let mut outs = Vec::new();
    for (pad, role) in card.pins() {
        let net = match role.as_str() {
            "vcc" | "vdd" | "vcca" | "vccb" => rail(3.3),
            "gnd" | "vss" => "GND".into(),
            r if outputs.iter().any(|o| o == r) => {
                let net = format!("OUT_{r}");
                outs.push(net.clone());
                net
            }
            r if inputs.iter().any(|i| i == r) => {
                let index = inputs.iter().position(|i| i == r).unwrap_or(0);
                if drive.high(r, index) {
                    rail(3.3)
                } else {
                    "GND".into()
                }
            }
            _ => continue,
        };
        let id = rig.net(&net);
        pins.push(pin(&pad, id));
    }
    rig.parts
        .push(component("U1", value, "Package_SO:SOIC-14", pins));
    for (i, net) in outs.iter().enumerate() {
        rig.load(&format!("RL{i}"), net, ohms.unwrap_or(1e9));
    }
    (rig, outs)
}

/// The sourced current and the +3.3V supply current of one digital board.
fn digital_measure(
    card: &Card,
    lib: &ModelLibrary,
    value: &str,
    drive: Drive,
    ohms: Option<f64>,
    strip: bool,
) -> Option<(f64, f64)> {
    let (rig, outs) = digital_rig(card, value, drive, ohms);
    let edit = |bound: &mut hauksbee_engine::binder::BoundBoard| {
        for d in bound.digital.iter_mut().filter(|_| strip) {
            d.drivers.values_mut().for_each(|drv| drv.supply = None);
        }
    };
    let sched = settle(&rig, lib, edit)?;
    let iout: f64 = outs
        .iter()
        .map(|n| sched.net_voltage(n).unwrap_or(0.0) / ohms.unwrap_or(1e9))
        .sum();
    let supply = sched.supply_states().get(&rail(3.3)).map(|s| s.1)?;
    Some((iout, supply))
}

/// Digital cards whose outputs no static input pattern drives high into a
/// load to ground, so the check cannot exercise them: the EEPROMs hold no
/// stored ones, and a bidirectional translator's output port follows its other
/// port, which this board loads to ground too. Asserted to stay unexercised,
/// so a pattern that reaches one turns this red until the entry goes.
const STATIC_INERT: &[&str] = &[
    "eeprom_28c256",
    "eeprom_at28c256f",
    "sn74avc1t45",
    "sn74lvc1t45",
    "74axp1t45",
];

/// Every digital card with outputs: for each input pattern that drives an
/// output high into 1 kOhm, the +3.3V pin carries that current on top of what
/// the same board draws unloaded, within 1%. The control routes nothing and
/// must fail.
#[test]
fn digital_output_current_is_drawn_from_the_supply_pin() {
    let cards =
        cards_of(|k, t| matches!(k, "digital" | "shift_register") && t.contains_key("logic"));
    // A sink-only open-collector array (no `voh`, no supply pin: ULN2003) has
    // no positive pin to draw a sourced current from; its outputs are driven
    // by a Thevenin leg at `vol` that sources into a pull-down load, which is
    // no legal control for a part that can only sink.
    let (sink_only, cards): (Vec<Card>, Vec<Card>) = cards
        .into_iter()
        .filter(|c| !c.logic("outputs").is_empty())
        .partition(|c| c.param("voh").is_none());
    assert!(
        sink_only
            .iter()
            .all(|c| !c.has_role("vcc") && !c.has_role("vdd")),
        "a sink-only card with a supply pin needs a sink check"
    );
    assert!(
        cards.len() >= 30,
        "only {} digital output cards",
        cards.len()
    );
    let lib = probe_library(&cards, "digitals");
    let (mut failures, mut inert) = (Vec::new(), Vec::new());
    for (i, card) in cards.iter().enumerate() {
        let value = format!("PROBE{i}");
        let mut exercised = false;
        for drive in Drive::ALL {
            let measure = |ohms, strip| digital_measure(card, &lib, &value, drive, ohms, strip);
            let (Some((iout, loaded)), Some((_, idle))) =
                (measure(Some(LOAD), false), measure(None, false))
            else {
                failures.push(format!("{} {drive:?}: solve failed", card.id));
                continue;
            };
            if iout < MIN_IOUT {
                continue;
            }
            exercised = true;
            let drawn = loaded - idle;
            if (drawn / iout - 1.0).abs() > TOL {
                failures.push(format!(
                    "{} {drive:?}: outputs source {iout:.4e} A, +3.3V gained {drawn:.4e} A",
                    card.id
                ));
            }
            let bare = measure(Some(LOAD), true).map_or(f64::NAN, |(_, s)| s - idle);
            if (bare / iout - 1.0).abs() <= TOL {
                failures.push(format!(
                    "{} {drive:?}: control with no routing still passes",
                    card.id
                ));
            }
        }
        if !exercised {
            inert.push(card.id.as_str());
        }
    }
    eprintln!("digital cards no pattern drives high: {inert:?}");
    let expected: Vec<&str> = STATIC_INERT.to_vec();
    let unexpected: Vec<_> = inert.iter().filter(|c| !expected.contains(c)).collect();
    let fixed: Vec<_> = expected.iter().filter(|c| !inert.contains(c)).collect();
    assert!(unexpected.is_empty(), "inert digital cards: {unexpected:?}");
    assert!(
        fixed.is_empty(),
        "now exercised, drop from STATIC_INERT: {fixed:?}"
    );
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Power-up through a regulator chain with active loads on its last rail:
/// +12V -> buck -> +5V -> LDO -> +3.3V feeding an op-amp and a gate, each
/// delivering into 1 kOhm. The supply draws these now put on the rails are
/// chunk means, so the buck's loop sees a smooth load and the board powers up
/// without a failed window; and the 12 V input carries the whole chain.
#[test]
fn a_buck_ldo_chain_with_active_loads_powers_up_and_balances() {
    let lib = ModelLibrary::builtin();
    let nets = [
        "+12V", "+5V", "+3.3V", "GND", "SW", "FB", "BST", "VP", "OUTA", "OUTB",
    ];
    let mut rig = Rig::default();
    for n in nets {
        rig.net(n);
    }
    let pins = |rig: &mut Rig, spec: &str| -> Vec<Pin> {
        let pad = |p: &str| p.split_once(':').map(|(a, n)| pin(a, rig.net(n)));
        spec.split(' ').filter_map(pad).collect()
    };
    let soic = "Package_SO:SOIC-8-1EP_3.9x4.9mm_P1.27mm_EP2.29x3mm";
    let buck = pins(&mut rig, "1:BST 2:+12V 3:+12V 5:FB 7:GND 8:SW 9:GND");
    rig.parts.push(component("U1", "AP64501SP-13", soic, buck));
    let ldo = pins(&mut rig, "3:+5V 2:+3.3V 1:GND");
    rig.parts
        .push(component("U2", "AMS1117-3.3", "SOT-223", ldo));
    // LM358 pins 8 (V+), 4 (V-), 3 (IN1+), 2 (IN1-), 1 (OUT1): a comparator-style
    // open loop with the positive input on the 3.3 V rail through 1k/1k.
    let amp = pins(&mut rig, "8:+3.3V 4:GND 3:VP 2:GND 1:OUTA");
    rig.parts
        .push(component("U3", "LM358", "Package_SO:SOIC-8", amp));
    let gate = pins(&mut rig, "14:+3.3V 7:GND 1:GND 2:OUTB");
    rig.parts
        .push(component("U4", "74HC04", "Package_SO:SOIC-14", gate));
    rig.two("L1", "10uH", IND, "SW", "+5V");
    rig.two("C1", "22uF", CAP, "+5V", "GND");
    rig.two("C2", "100nF", CAP, "BST", "SW");
    rig.two("R2", "52.3k", RES, "+5V", "FB");
    rig.load("R3", "FB", 10e3);
    rig.two("C3", "10uF", CAP, "+3.3V", "GND");
    rig.two("R4", "1k", RES, "+3.3V", "VP");
    rig.load("R5", "VP", 1e3);
    rig.load("R6", "OUTA", LOAD);
    rig.load("R7", "OUTB", LOAD);
    let bound = bind_board_with(&rig.board(), &lib, &CustomRegistry::new());
    let mut sched = Scheduler::new(bound, None, SolverOptions::default()).unwrap();
    for _ in 0..40 {
        sched.step(1e-4);
    }
    assert!(sched.analog_valid(), "{:?}", sched.failed_windows());
    assert!(
        sched.failed_windows().is_empty(),
        "{:?}",
        sched.failed_windows()
    );
    let v = |n: &str| sched.net_voltage(n).unwrap_or(f64::NAN);
    let (v12, v5, v33) = (v("+12V"), v("+5V"), v("+3.3V"));
    assert!(
        (v5 - 5.0).abs() < 0.1 && (v33 - 3.3).abs() < 0.07,
        "{v5} {v33}"
    );
    let iout = v("OUTA") / LOAD + v("OUTB") / LOAD;
    assert!(iout > 4e-3, "active loads deliver {iout}");
    let states = sched.supply_states();
    // The 3.3 V rail carries both outputs and the VP divider; the 5 V rail
    // that plus the LDO's 5 mA quiescent current and the buck's feedback
    // divider; the buck's input power is that over its 90% efficiency.
    let i33 = iout + v33 / 2e3;
    let i5 = i33 + 5e-3 + v5 / (52.3e3 + 10e3);
    let want = v5 * i5 / (0.9 * v12);
    let got = states["+12V"].1;
    assert!(
        (got / want - 1.0).abs() < 0.03,
        "+12V carries {got:.5e} A, the chain needs {want:.5e} A (i33 {i33:.4e}, i5 {i5:.4e})"
    );
}
