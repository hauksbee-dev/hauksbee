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
//! output: every builtin op-amp, push-pull comparator, DAC and digital part
//! with outputs is bound on a supply-named rail, its outputs loaded by 1 k to
//! ground, and the current each output delivers must appear in the part's
//! supply pin within 1% (see `opamp_output_current_is_drawn_from_the_supply_pins`,
//! `digital_output_current_is_drawn_from_the_supply_pin` and
//! `dac_output_current_is_drawn_from_the_supply_pin`). A control run with the
//! routing removed must fail the same check, and a buck -> LDO -> op-amp chain
//! must still power up. An open-collector array only sinks, into its ground pin
//! (`open_collector_outputs_only_sink_into_the_ground_pin`).
//!
//! A linear charger (a `vreg` card tagged `regulated_current`) is a CC/CV
//! source, so its rig differs: a PROG resistor sets a limit above the load,
//! and the checks above hold with the limit idle. Further tests drive the limit
//! itself (a short load holds the PROG current), and put a declared battery on
//! the charger's output, which is legal and takes the charge current.
//!
//! The cards are enumerated from the db and classified by what the binder
//! stamped, never hand-picked, so a new card joins the matrix by existing.
//! A path on the known-gap list is asserted to still fail, so a fix landing
//! turns this test red until the list is updated.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use hauksbee_engine::binder::bind_board_with;
use hauksbee_engine::power_rail_voltage;
use hauksbee_engine::power_supply::{PowerSupply, SupplyLeg};
use hauksbee_engine::rail_owner::RailDomains;
use hauksbee_engine::scheduler::Scheduler;
use hauksbee_engine::CustomRegistry;
use hauksbee_extract::{Component, ExtractedBoard, Pin};
use hauksbee_ir::{Device, SourceKind};
use hauksbee_models::{ModelEntry, ModelLibrary, SourceLayer};
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
    /// Whether the card records its sheet's package pinout
    /// (`[[models.source.pinout]]`), which the binder holds a footprint to.
    fn declares_package(&self) -> bool {
        let source = self.table.get("source").and_then(toml::Value::as_table);
        source
            .and_then(|s| s.get("pinout"))
            .and_then(toml::Value::as_array)
            .is_some_and(|p| !p.is_empty())
    }

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

    /// The charge current the card's PROG law gives at `ohms`, for a linear
    /// charger: a `vreg` tagged `regulated_current` whose current leaves at
    /// `out`. A power-path part's leaves at a charge port beside its system
    /// rail instead (see [`Card::charge_port`]).
    fn charge_current_at(&self, ohms: f64) -> Option<f64> {
        let program = self.entry()?.current_program?;
        let out = program.current_out_roles.iter().any(|r| r == "out");
        (out && self.regulated_current()).then_some(())?;
        program.operating_current_a(ohms)
    }

    /// For a power-path part (a regulated current out of a role other than
    /// `out`): the program pin, the port role, and the current at `ohms`.
    fn charge_port(&self, ohms: f64) -> Option<(String, String, f64)> {
        let program = self.entry()?.current_program?;
        self.regulated_current().then_some(())?;
        let [port] = program.current_out_roles.as_slice() else {
            return None;
        };
        (port != "out").then_some(())?;
        let amps = program.operating_current_a(ohms)?;
        Some((program.pin.clone(), port.clone(), amps))
    }

    fn regulated_current(&self) -> bool {
        let program = self.table.get("current_program").and_then(|p| p.as_table());
        self.kind == "vreg"
            && program.and_then(|p| text(p, "semantics")) == Some("regulated_current")
    }

    fn entry(&self) -> Option<ModelEntry> {
        self.table.clone().try_into().ok()
    }
}

/// Whether the card declares `open_collector = true`: its outputs sink only.
fn open_collector(card: &Card) -> bool {
    let flag = card
        .table
        .get("params")
        .and_then(|p| p.get("open_collector"));
    flag.and_then(toml::Value::as_bool) == Some(true)
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
        // A variant member's probe is a card of its own: its family's shared
        // match is replaced, so it is no longer one of a set.
        t.remove("variant_of");
        t.remove("variant");
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
    /// Resistors between two output-side nets (a divider's top leg): (from,
    /// to, ohms). No current of their own returns to ground, but what they
    /// dissipate is output power the part delivers.
    series: Vec<(String, String, f64)>,
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

    fn series(&mut self, reference: &str, a: &str, b: &str, ohms: f64) {
        self.two(reference, &format!("{ohms}R"), RES, a, b);
        self.series.push((a.into(), b.into(), ohms));
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

/// The rig's footprint for a card's part: a SOIC-8, unless the card numbers
/// more pads than that or records its own package, when it is left unnamed so
/// the package-fit check has no rig-chosen outline to hold the card against.
fn rig_footprint(card: &Card) -> &'static str {
    let pads = card
        .pins()
        .iter()
        .filter_map(|(p, _)| p.parse::<u32>().ok())
        .max();
    if card.declares_package() || pads.is_some_and(|n| n > 8) {
        ""
    } else {
        "Package_SO:SOIC-8"
    }
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
    if card.charge_current_at(CHARGER_PROG_OHMS).is_some() {
        return charger_plan(card, value, &rail(4.2), None);
    }
    let feedback = conv
        .and_then(|c| c.get("feedback"))
        .and_then(toml::Value::as_table);
    let vref = feedback
        .and_then(|f| float(f, "vref_v"))
        .or_else(|| conv.is_none().then(|| card.param("vref")).flatten());
    let fixed = conv.map_or_else(|| card.param("vout"), |c| float(c, "vout_setpoint"));
    // A fixed converter version (LM2596-12) names its output as the feedback
    // reference: FB sits on the output node, there is no divider to build, and
    // the rig runs it at its own setpoint rather than at the adjustable 5 V.
    let vref = vref.filter(|v| fixed.is_none_or(|f| (f - v).abs() > 1e-9));
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
        .push(component("U1", value, rig_footprint(card), pins));
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
        rig.series("R2", &outp, "FB", top);
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

/// The PROG resistor of every charger rig: 5k reads 0.2 A to 0.24 A on every
/// builtin charger law, inside each card's operating domain.
const CHARGER_PROG_OHMS: f64 = 5e3;

/// A linear charger on a +6V input, its CE and enables tied to the input and
/// a PROG resistor to ground, charging the net `output` through a resistive
/// load of `load_ohms`: `None` draws half the programmed current, `Some(0.0)`
/// leaves the output open for a declared battery.
fn charger_plan(card: &Card, value: &str, output: &str, load_ohms: Option<f64>) -> Option<Plan> {
    let iprog = card.charge_current_at(CHARGER_PROG_OHMS)?;
    let (input, output) = (rail(6.0), output.to_string());
    let mut rig = Rig::default();
    rig.net("GND");
    let pins = card
        .pins()
        .into_iter()
        .filter_map(|(pad, role)| {
            let net = match role.as_str() {
                "in" | "in2" | "ce" | "en" => input.as_str(),
                "out" | "out2" => output.as_str(),
                "gnd" => "GND",
                "prog" => "PROG",
                _ => return None,
            };
            Some(pin(&pad, rig.net(net)))
        })
        .collect();
    rig.parts
        .push(component("U1", value, "Package_SO:SOIC-8", pins));
    rig.two("R9", "5k", RES, "PROG", "GND");
    rig.two("C1", "10uF", CAP, &input, "GND");
    let ohms = load_ohms.unwrap_or(4.2 / (0.5 * iprog));
    if ohms > 0.0 {
        rig.load("R1", &output, ohms);
    }
    let iq = card.param("iq_a");
    Some(Plan {
        rig,
        input,
        output,
        target: 4.2,
        expect: Expect::Linear {
            iq: iq.unwrap_or(0.0),
        },
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
            // The second lane of a shared-select DPDT mux mirrors the first: the
            // binder stamps the two-lane arm only when every terminal is wired,
            // and otherwise falls back to bridging whichever two nets it sorts
            // first, which depends on the card's pad order, not on the part.
            "com_2" => input.clone(),
            "s0_2" => output.clone(),
            "s1" | "s1_1" | "s1_2" => {
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
    // A card that records its own package (a ten-pad mux, say) is held to its
    // own outline, not to the rig's SOT-23-6, which has no pads past six.
    let footprint = if card.declares_package() {
        ""
    } else {
        "Package_TO_SOT_SMD:SOT-23-6"
    };
    rig.parts.push(component("U1", value, footprint, pins));
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
    } else if named("Ichg_U1") {
        "charger"
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
        .sum::<f64>()
        + plan
            .rig
            .series
            .iter()
            .filter(|(_, _, r)| *r > 0.0)
            .map(|(a, b, r)| (v(a) - v(b)).powi(2) / r)
            .sum::<f64>();
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
    settle_with(rig, lib, edit, |_| {})
}

/// [`settle`] with `prep` run on the built scheduler before it steps (to
/// program a peripheral over its bus).
fn settle_with(
    rig: &Rig,
    lib: &ModelLibrary,
    edit: impl FnOnce(&mut hauksbee_engine::binder::BoundBoard),
    prep: impl FnOnce(&mut Scheduler),
) -> Option<Scheduler> {
    let mut bound = bind_board_with(&rig.board(), lib, &CustomRegistry::new());
    edit(&mut bound);
    let mut sched = Scheduler::new(bound, None, SolverOptions::default()).ok()?;
    prep(&mut sched);
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
    // A card that records its package binds only on a matching footprint; the
    // rig names none for it rather than a SOIC-14 the part never comes in.
    let footprint = if card.declares_package() {
        ""
    } else {
        "Package_SO:SOIC-14"
    };
    rig.parts.push(component("U1", value, footprint, pins));
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
            if let Device::OpAmp { supply, .. } | Device::Comparator { supply, .. } = d {
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

/// A card's own supply current with the outputs unloaded: an op-amp's `iq_a`,
/// a comparator's `supply_static_ua`.
fn card_iq(card: &Card) -> f64 {
    let static_ua = card.param("supply_static_ua").map(|ua| ua * 1e-6);
    card.param("iq_a").or(static_ua).unwrap_or(0.0)
}

/// Every builtin op-amp card and every push-pull comparator card (an
/// open-collector one has no high drive to draw): driven positive on a single
/// supply and, where the part has a `V-` pin, negative on split supplies. The
/// positive pin carries the sourced current, the negative pin the sunk current,
/// and a control with the routing stripped fails the same check.
#[test]
fn opamp_output_current_is_drawn_from_the_supply_pins() {
    let cards = cards_of(|k, t| {
        let open_collector = (t.get("params")).and_then(|p| p.get("open_collector"));
        k == "opamp"
            || (k == "comparator" && open_collector.and_then(toml::Value::as_bool) != Some(true))
    });
    assert!(
        cards.len() >= 33,
        "only {} op-amp/comparator cards",
        cards.len()
    );
    assert!(
        cards.iter().filter(|c| c.kind == "comparator").count() >= 3,
        "the push-pull comparators must be in the matrix"
    );
    let lib = probe_library(&cards, "opamps");
    let mut failures = Vec::new();
    let mut unexercised = Vec::new();
    for (i, card) in cards.iter().enumerate() {
        let value = format!("PROBE{i}");
        let iq = card_iq(card);
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
    // A card that records its package binds only on a matching footprint; the
    // rig names none for it rather than a SOIC-14 the part never comes in.
    let footprint = if card.declares_package() {
        ""
    } else {
        "Package_SO:SOIC-14"
    };
    rig.parts.push(component("U1", value, footprint, pins));
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
    "txb0101",
    "txb0102",
    "txb0104",
    "txb0108",
];

/// Every digital card with outputs: for each input pattern that drives an
/// output high into 1 kOhm, the +3.3V pin carries that current on top of what
/// the same board draws unloaded, within 1%. The control routes nothing and
/// must fail.
#[test]
fn digital_output_current_is_drawn_from_the_supply_pin() {
    let cards =
        cards_of(|k, t| matches!(k, "digital" | "shift_register") && t.contains_key("logic"));
    // A sink-only open-collector or open-drain card (no `voh`: ULN2003, or an
    // open-drain buffer such as the 74LVC1G07, which has a supply pin it never
    // sources through) has no high drive to check a sourced current against:
    // `open_collector_outputs_only_sink_into_the_ground_pin` covers it.
    let (sink_only, cards): (Vec<Card>, Vec<Card>) = cards
        .into_iter()
        .filter(|c| !c.logic("outputs").is_empty())
        // A `voh` stated as a row (min/typ/max with conditions) is still a `voh`.
        .partition(|c| c.table.get("params").and_then(|p| p.get("voh")).is_none());
    assert!(
        sink_only.iter().all(open_collector),
        "a card with no `voh` must say `open_collector = true`"
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

/// The outputs of an open-collector array on `+5V` pull-ups (or 1 k
/// pull-downs), the ground pin on net `E` through 1 ohm to ground, every input
/// at the level that sinks (`on`) or the one that leaves the outputs open.
/// Returns the rig and the output nets.
fn open_collector_rig(card: &Card, value: &str, on: bool, pull_up: bool) -> (Rig, Vec<String>) {
    let (inputs, outputs) = (card.logic("inputs"), card.logic("outputs"));
    let sink_when_high = card
        .table
        .get("logic")
        .is_some_and(|l| l.get("tristate").is_some());
    let mut rig = Rig::default();
    rig.net("GND");
    let mut pins = Vec::new();
    let mut outs = Vec::new();
    for (pad, role) in card.pins() {
        let net = match role.as_str() {
            "gnd" | "vss" => "E".to_string(),
            r if outputs.iter().any(|o| o == r) => {
                outs.push(format!("OUT_{r}"));
                format!("OUT_{r}")
            }
            r if inputs.iter().any(|i| i == r) => {
                // An array whose outputs are tri-stated by their inputs (ULN2003)
                // sinks on a high input; a buffer whose output follows its input
                // (74LVC1G07) sinks while its input is low.
                if on == sink_when_high {
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
        .push(component("U1", value, "Package_SO:SOIC-16", pins));
    rig.two("RE", "1R", RES, "E", "GND");
    for (i, net) in outs.iter().enumerate() {
        if pull_up {
            rig.two(&format!("RL{i}"), "1k", RES, net, &rail(5.0));
        } else {
            rig.load(&format!("RL{i}"), net, LOAD);
        }
    }
    (rig, outs)
}

/// What an open-collector board settled to: the current its outputs sank
/// (from the pull-ups), the current the ground pin carried, and the highest
/// output voltage.
fn open_collector_measure(
    card: &Card,
    lib: &ModelLibrary,
    value: &str,
    (on, pull_up, strip): (bool, bool, bool),
) -> Option<(f64, f64, f64)> {
    let (rig, outs) = open_collector_rig(card, value, on, pull_up);
    let edit = |bound: &mut hauksbee_engine::binder::BoundBoard| {
        for d in bound.digital.iter_mut().filter(|_| strip) {
            d.drivers.values_mut().for_each(|drv| drv.supply = None);
        }
    };
    let sched = settle(&rig, lib, edit)?;
    let v = |n: &str| sched.net_voltage(n).unwrap_or(f64::NAN);
    let sunk: f64 = outs.iter().map(|n| (5.0 - v(n)) / LOAD).sum();
    let top = outs.iter().map(|n| v(n)).fold(f64::MIN, f64::max);
    Some((sunk, v("E"), top))
}

/// An open-collector output sinks into the part's ground (`E`) pin and does
/// nothing else: on a pull-up the pin carries what the outputs sink (a control
/// with the routing stripped must fail); on a pull-down a driven output reads
/// 0 V and the pin carries nothing, because a Darlington has no high drive; and
/// with every input low the outputs float up to the pull-up and the pin carries
/// nothing.
#[test]
fn open_collector_outputs_only_sink_into_the_ground_pin() {
    let cards =
        cards_of(|k, t| matches!(k, "digital" | "shift_register") && t.contains_key("logic"));
    let cards: Vec<Card> = cards.into_iter().filter(open_collector).collect();
    assert!(!cards.is_empty(), "no open-collector digital card");
    let lib = probe_library(&cards, "open_collector");
    let mut failures = Vec::new();
    for (i, card) in cards.iter().enumerate() {
        let value = format!("PROBE{i}");
        let id = &card.id;
        let run =
            |on, pull_up, strip| open_collector_measure(card, &lib, &value, (on, pull_up, strip));
        let (Some((sunk, ie, _)), Some((_, off_ie, off_top)), Some((_, pd_ie, pd_top))) = (
            run(true, true, false),
            run(false, true, false),
            run(true, false, false),
        ) else {
            failures.push(format!("{id}: solve failed"));
            continue;
        };
        // The ground pin is 1 ohm to ground, so its volts are its amps.
        if sunk < MIN_IOUT || (ie / sunk - 1.0).abs() > TOL {
            failures.push(format!(
                "{id}: outputs sink {sunk:.4e} A, ground pin carries {ie:.4e} A"
            ));
        }
        if off_ie.abs() > 1e-6 || off_top < 4.9 {
            failures.push(format!(
                "{id}: inputs low: ground pin {off_ie:.2e} A, outputs {off_top} V"
            ));
        }
        if pd_top > 1e-3 || pd_ie.abs() > 1e-6 {
            failures.push(format!(
                "{id}: on a pull-down the outputs read {pd_top} V and the ground pin carries \
                 {pd_ie:.2e} A: an open collector cannot source"
            ));
        }
        match run(true, true, true) {
            Some((_, bare, _)) if (bare / sunk - 1.0).abs() > TOL => {}
            other => failures.push(format!("{id}: control with no routing: {other:?}")),
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Every DAC card on a supply-named rail, its outputs at full scale into 1 k to
/// ground: the supply pin carries what the outputs deliver, within 1%, and a
/// control with the routing stripped must fail.
#[test]
fn dac_output_current_is_drawn_from_the_supply_pin() {
    // The rig programs the card over I2C with a Fast Write, so it takes the DAC
    // cards that have an I2C address; the SPI and I2S ones (MCP4921/2, PCM5102A)
    // have no such register path to drive.
    let cards =
        cards_of(|k, t| k == "dac" && t.get("params").is_some_and(|p| p.get("i2c_addr").is_some()));
    assert!(!cards.is_empty(), "no DAC card");
    let lib = probe_library(&cards, "dacs");
    let mut failures = Vec::new();
    for (i, card) in cards.iter().enumerate() {
        let measure = |strip: bool| -> Option<(f64, f64)> {
            let mut rig = Rig::default();
            rig.net("GND");
            let (mut pins, mut outs) = (Vec::new(), Vec::new());
            for (pad, role) in card.pins() {
                let net = match role.as_str() {
                    "vdd" | "vcc" => rail(5.0),
                    "vss" | "gnd" => "GND".into(),
                    r if r.starts_with("vout") => {
                        outs.push(format!("OUT_{r}"));
                        format!("OUT_{r}")
                    }
                    _ => continue,
                };
                let id = rig.net(&net);
                pins.push(pin(&pad, id));
            }
            rig.parts.push(component(
                "U1",
                &format!("PROBE{i}"),
                "Package_SO:MSOP-10",
                pins,
            ));
            for (k, net) in outs.iter().enumerate() {
                rig.load(&format!("RL{k}"), net, LOAD);
            }
            let edit = |bound: &mut hauksbee_engine::binder::BoundBoard| {
                for dac in bound.dacs.iter_mut().filter(|_| strip) {
                    for drv in dac.vout_drivers.iter_mut().flatten() {
                        drv.supply = None;
                    }
                }
            };
            // Fast Write of full scale to every channel, as firmware would.
            let program = |sched: &mut Scheduler| {
                let bus = sched.i2c_buses()[0].clone();
                let mut bus = bus
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                bus.dispatch(hauksbee_mcu::I2cEvent::Start {
                    addr: 0x60,
                    read: false,
                });
                for _ in 0..4 * 2 {
                    bus.dispatch(hauksbee_mcu::I2cEvent::Write {
                        addr: 0x60,
                        data: 0x0F,
                    });
                }
                bus.dispatch(hauksbee_mcu::I2cEvent::Stop { addr: 0x60 });
            };
            let sched = settle_with(&rig, &lib, edit, program)?;
            let iout: f64 = (outs.iter())
                .map(|n| sched.net_voltage(n).unwrap_or(0.0) / LOAD)
                .sum();
            let supply = sched.supply_states().get(&rail(5.0)).map(|s| s.1)?;
            Some((iout, supply))
        };
        let Some((iout, supply)) = measure(false) else {
            failures.push(format!("{}: solve failed", card.id));
            continue;
        };
        if iout < MIN_IOUT || (supply / iout - 1.0).abs() > TOL {
            failures.push(format!(
                "{}: outputs source {iout:.4e} A, the supply pin carries {supply:.4e} A",
                card.id
            ));
        }
        let bare = measure(true).map_or(f64::NAN, |(_, s)| s);
        if (bare / iout - 1.0).abs() <= TOL {
            failures.push(format!("{}: control with no routing still passes", card.id));
        }
    }
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

/// A charger board settled for 2 ms, with an ideal 3.7 V cell declared on the
/// output when `cell` is set. The scheduler and the rail check on that net.
fn settle_charger(plan: &Plan, lib: &ModelLibrary, cell: bool) -> Scheduler {
    settle_charger_at(plan, lib, cell, 6.0)
}

/// [`settle_charger`] with the input rail at `input_volts`.
fn settle_charger_at(plan: &Plan, lib: &ModelLibrary, cell: bool, input_volts: f64) -> Scheduler {
    let mut bound = bind_board_with(&plan.rig.board(), lib, &CustomRegistry::new());
    let wired = |l: &&mut SupplyLeg| l.net_name == plan.input;
    let leg = bound
        .supplies
        .iter_mut()
        .find(wired)
        .expect("input rail leg");
    leg.reconfigure(
        &mut bound.circuit,
        PowerSupply::Ideal { volts: input_volts },
    );
    if cell {
        let node = bound.node(&plan.output).expect("output net is bound");
        let legs: Vec<_> = bound.supplies.iter().map(|l| l.vsource).collect();
        let domains = RailDomains::new(&bound.circuit, &bound.behavioral, &legs);
        assert_eq!(
            domains.owned_note(&bound.circuit, &plan.output, node, |_| None),
            None,
            "a charger's output is legal ground for a declared battery"
        );
        let supply = PowerSupply::Ideal { volts: 3.7 };
        let leg = SupplyLeg::stamp(&mut bound.circuit, node, &plan.output, supply);
        bound.supplies.push(leg);
    }
    let mut sched = Scheduler::new(bound, None, SolverOptions::default()).expect("scheduler");
    for _ in 0..20 {
        sched.step(1e-4);
    }
    assert!(sched.analog_valid(), "{:?}", sched.failed_windows());
    sched
}

/// Every builtin charger holds the PROG current into a short load, takes the
/// same current (plus its Iq) from the input, and charges a declared cell at
/// that current with the cell legal on the output, on a supply-named net and
/// on a plain one.
#[test]
fn a_linear_charger_limits_to_the_prog_current_and_charges_a_declared_cell() {
    let cards = power_cards();
    let lib = probe_library(&cards, "charger_cell");
    let mut seen = 0;
    for (i, card) in cards.iter().enumerate() {
        let Some(iprog) = card.charge_current_at(CHARGER_PROG_OHMS) else {
            continue;
        };
        seen += 1;
        let value = format!("PROBE{i}");
        let iq = card.param("iq_a").unwrap_or(0.0);
        let close = |got: f64, want: f64, what: &str| {
            assert!(
                (got / want - 1.0).abs() < 0.02,
                "{}: {what} {got:.5e} A, want {want:.5e} A",
                card.id
            );
        };
        // A 1 ohm load asks for more than the limit: the limit holds.
        let short = charger_plan(card, &value, "VBAT", Some(1.0)).unwrap();
        let sched = settle_charger(&short, &lib, false);
        let states = sched.supply_states();
        let v = |n: &str| sched.net_voltage(n).unwrap_or(f64::NAN);
        close(v("VBAT"), iprog, "short-load current");
        close(states[&short.input].1, iprog + iq, "input current");
        // A declared cell takes the charge: constant current, then the cell
        // sits above the float only when the limit gives way to it.
        for net in ["VBAT", "+4.2V"] {
            let cell = charger_plan(card, &value, net, Some(0.0)).unwrap();
            let sched = settle_charger(&cell, &lib, true);
            let states = sched.supply_states();
            close(-states[net].1, iprog, &format!("{net} charge current"));
            close(states[&cell.input].1, iprog + iq, &format!("{net} input"));
            // With the input unplugged the cell is not discharged into it.
            let dead = settle_charger_at(&cell, &lib, true, 0.0);
            let leak = dead.supply_states()[net].1;
            assert!(
                leak.abs() < 1e-3 * iprog,
                "{}: {net} leaks {leak:.3e} A with the input off",
                card.id
            );
        }
    }
    assert!(seen >= 5, "only {seen} charger cards found");
}

/// The refusal stays for a part that sets its rail's voltage: a declared
/// supply on a fixed regulator's output is the fight the rail check exists
/// to stop, and a charger's allowance does not loosen it.
#[test]
fn a_declared_supply_on_a_fixed_regulators_output_is_still_refused() {
    let cards = power_cards();
    let lib = probe_library(&cards, "charger_short");
    let (i, card) = cards
        .iter()
        .enumerate()
        .find(|(_, c)| {
            c.kind == "vreg" && c.param("dropout_v").is_some() && c.param("vout").is_some()
        })
        .filter(|(_, c)| c.charge_current_at(CHARGER_PROG_OHMS).is_none())
        .expect("a fixed linear regulator card");
    let plan = vreg_plan(card, &format!("PROBE{i}")).unwrap();
    let bound = bind_board_with(&plan.rig.board(), &lib, &CustomRegistry::new());
    let node = bound.node(&plan.output).unwrap();
    let domains = RailDomains::new(&bound.circuit, &bound.behavioral, &[]);
    assert!(
        domains
            .owned_note(&bound.circuit, &plan.output, node, |_| None)
            .is_some(),
        "{} drives {}",
        card.id,
        plan.output
    );
}

/// A power-path part's programmed current leaves at a `bat` port, not at the
/// `out` that is its system rail: that `out` must not be limited to the charge
/// current, so such a card keeps the plain regulator stamp.
#[test]
fn a_power_path_charger_keeps_its_system_rail_unlimited() {
    let mut card = power_cards()
        .into_iter()
        .find(|c| c.id == "tp4056")
        .unwrap();
    let program = card.table.get_mut("current_program").unwrap();
    let roles: Vec<toml::Value> = vec!["bat".into()];
    program
        .as_table_mut()
        .unwrap()
        .insert("current_out_roles".into(), roles.into());
    let cards = [card];
    let lib = probe_library(&cards, "charger_roles");
    let plan = vreg_plan(&cards[0], "PROBE0").unwrap();
    let bound = bind_board_with(&plan.rig.board(), &lib, &CustomRegistry::new());
    assert_eq!(path_of(&bound.circuit.devices), "fixed");
}

/// The first strap row of a card's resistor table that sets a charge float
/// and names no unmodelled mode: (strap role, ohms, float, input limit).
fn float_strap(card: &Card) -> Option<(String, f64, f64, Option<f64>)> {
    let entry = card.entry()?;
    entry.resistor_bins.iter().find_map(|table| {
        table.bins.iter().find_map(|bin| {
            let vfloat = *bin.params.get("vbat_reg_v")?;
            (bin.note.is_none()).then_some(())?;
            let ilim = bin.params.get("input_limit_a").copied();
            Some((table.pin.clone(), bin.ohms?, vfloat, ilim))
        })
    })
}

/// A power-path board: the input on a +5V rail, the system rail loaded to
/// about 100 mA, the program resistor, the charge port on `VBAT` (loaded by
/// `port_load` ohms when set), and the strap resistor when `strap` is set.
fn power_path_plan(card: &Card, value: &str, strap: bool, port_load: Option<f64>) -> Option<Plan> {
    let (prog_role, port_role, _) = card.charge_port(CHARGER_PROG_OHMS)?;
    let (strap_role, strap_ohms, ..) = float_strap(card)?;
    let vout = card.param("vout")?;
    let input = rail(5.0);
    let mut rig = Rig::default();
    rig.net("GND");
    let pins = card
        .pins()
        .into_iter()
        .filter_map(|(pad, role)| {
            let net = match role.as_str() {
                "in" => input.as_str(),
                "out" => "SYS",
                "gnd" => "GND",
                r if r == prog_role => "PROG",
                r if r == port_role => "VBAT",
                r if r == strap_role => "STRAP",
                _ => return None,
            };
            Some(pin(&pad, rig.net(net)))
        })
        .collect();
    rig.parts
        .push(component("U1", value, rig_footprint(card), pins));
    rig.two("R9", &format!("{CHARGER_PROG_OHMS}R"), RES, "PROG", "GND");
    if strap {
        rig.two("R8", &format!("{strap_ohms}R"), RES, "STRAP", "GND");
    }
    rig.two("C1", "10uF", CAP, &input, "GND");
    rig.two("C2", "10uF", CAP, "SYS", "GND");
    rig.load("R1", "SYS", vout / 0.1);
    if let Some(ohms) = port_load {
        rig.load("R2", "VBAT", ohms);
    }
    Some(Plan {
        rig,
        input,
        output: "VBAT".into(),
        target: vout,
        expect: Expect::Linear {
            iq: card.param("iq_a").unwrap_or(0.0),
        },
    })
}

/// Every builtin power-path card powers its system rail from the input,
/// charges a declared cell on its charge port at the programmed current
/// (capped by the strap's input limit) drawn from that same input, floats an
/// unloaded port at the strap's float voltage, and leaves the port unstamped,
/// saying so, when the strap cannot be read.
#[test]
fn a_power_path_card_charges_its_port_and_powers_its_rail_from_the_input() {
    let cards = power_cards();
    let lib = probe_library(&cards, "power_path");
    let mut seen = 0;
    for (i, card) in cards.iter().enumerate() {
        let Some((_, _, iprog)) = card.charge_port(CHARGER_PROG_OHMS) else {
            continue;
        };
        let Some((_, _, vfloat, ilim)) = float_strap(card) else {
            continue;
        };
        seen += 1;
        let value = format!("PROBE{i}");
        let ichg = ilim.map_or(iprog, |l| iprog.min(l));
        let vout = card.param("vout").unwrap();
        let iq = card.param("iq_a").unwrap_or(0.0);
        let near = |got: f64, want: f64, tol: f64, what: &str| {
            assert!(
                (got / want - 1.0).abs() < tol,
                "{}: {what} {got:.5e}, want {want:.5e}",
                card.id
            );
        };

        // A declared 3.7 V cell takes the programmed current; the system rail
        // holds; the input carries the rail's load, the charge and Iq.
        let plan = power_path_plan(card, &value, true, None).unwrap();
        let sched = settle_charger_at(&plan, &lib, true, 5.0);
        let states = sched.supply_states();
        let v = |n: &str| sched.net_voltage(n).unwrap_or(f64::NAN);
        near(v("SYS"), vout, 0.02, "system rail volts");
        near(-states["VBAT"].1, ichg, 0.02, "charge current");
        let isys = v("SYS") / (vout / 0.1);
        near(
            states[&plan.input].1,
            isys + ichg + iq,
            TOL,
            "input current",
        );

        // An unloaded port (a 100 k bleed only) floats at the strap's voltage.
        let plan = power_path_plan(card, &value, true, Some(1e5)).unwrap();
        let sched = settle_charger_at(&plan, &lib, false, 5.0);
        let vbat = sched.net_voltage("VBAT").unwrap_or(f64::NAN);
        near(vbat, vfloat, 0.01, "port float volts");

        // No strap: no float is selected, so no charge current is invented,
        // and the bind report says why.
        let plan = power_path_plan(card, &value, false, None).unwrap();
        let bound = bind_board_with(&plan.rig.board(), &lib, &CustomRegistry::new());
        let warning = bound.report.row("U1").and_then(|r| r.warning.clone());
        assert!(
            warning.as_deref().is_some_and(|w| w.contains("strap")),
            "{}: unread strap not reported: {warning:?}",
            card.id
        );
        let sched = settle_charger_at(&plan, &lib, true, 5.0);
        let leak = sched.supply_states()["VBAT"].1;
        assert!(
            leak.abs() < 1e-3 * ichg,
            "{}: VBAT carries {leak:.3e} A with no float selected",
            card.id
        );
    }
    assert!(seen >= 1, "no power-path charger cards found");
}
