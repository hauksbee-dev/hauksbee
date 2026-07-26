//! Migration regression for the declarative digital-logic evaluator.
//!
//! ## Provenance, how these goldens were proven
//!
//! Before the `DigitalKind` enum was deleted, this file drove BOTH the legacy
//! hardcoded implementations (`Hc595`, `Hc165`, `Buffer`, `NorLatch`) and the
//! `[models.logic]` spec-driven [`LogicComponent`] through IDENTICAL
//! single-edge sequences and asserted register state and every driven-output
//! voltage equal AT EVERY EDGE (89/54/24/16 edges respectively), plus a
//! 4-chip daisy chain compared per edge against [`Hc595Chain`] (99 edges × 4
//! chips). That old-vs-new proof is commit `ed80984` ("tests: byte-exact
//! regression, legacy DigitalKind vs spec-driven LogicComponent"), check it
//! out to re-run the two-sided comparison. Only after it passed was the enum
//! deleted.
//!
//! This post-deletion version pins the SAME sequences against goldens
//! captured from that proven run (the printed per-edge trajectories), and
//! keeps the one comparison that is still two-sided: the spec-driven chain
//! versus `Hc595Chain::replay`, which remains independent legacy Rust (the
//! MCU-facing chain fast-path).
//!
//! ## Deliberately excluded corners (documented, not hidden)
//!
//! * **SRCLK rising while SRCLR_n is HELD low (74HC595).** The legacy
//!   `tick_595` cleared the register and THEN shifted within the same
//!   sample; the TI SN74HC595 function table (SCLS041I, "SRCLR L, SRCLK X
//!   -> shift register clear") says the register stays cleared, which is
//!   what `Hc595Chain::replay` implemented and what the spec's
//!   reset-dominant semantics implement. A legacy artifact no firmware
//!   sequence exercises.
//! * **Simultaneous SET+RESET release on the NOR latch.** A silicon race;
//!   the legacy Jacobi loop silently exited on an oscillating non-fixpoint
//!   there. Sequences release the two lines on separate edges (defined, and
//!   proven identical).
//! * **74HC165 clocked-shift direction.** Legacy `tick_165` shifted AWAY
//!   from QH (QH read SER after one clock), inverted from TI SCLS052I
//!   ("data is shifted toward the QH output": QH shows H, then G, ...). The
//!   real read path (`Hc165Chain` + responder, untouched by the migration)
//!   always had the datasheet direction, and nothing in the corpus
//!   exercises `tick_165`'s clocked shifts. The byte-exact proof at
//!   `ed80984` pinned the legacy semantics with a `shift_right` spec; the
//!   SHIPPING spec (db/digital.toml) is the silicon-correct `shift_left`,
//!   verified by `hc165_silicon_direction_matches_datasheet` below and by
//!   the untouched `Hc165Chain` tests it must agree with.

use std::collections::HashMap;

use hauksbee_engine::digital::{DigitalComponent, Hc595Chain, LogicLevels};
use hauksbee_engine::logic::LogicComponent;
use hauksbee_ir::{Circuit, NodeId};
use hauksbee_models::logic_spec::Logic;
use hauksbee_models::{ComponentQuery, ModelLibrary};

fn builtin(id: &str) -> hauksbee_models::ModelEntry {
    let lib = ModelLibrary::builtin();
    let q = ComponentQuery::new(None, Some(id.to_string()), None);
    lib.resolve(&q)
        .model
        .unwrap_or_else(|| panic!("builtin {id} model"))
}

/// A spec part driven through per-edge ticks from a persistent pin-state map,
/// using the same threshold decision (`LogicLevels::decide`) the bound
/// component path applies.
struct Rig {
    lc: LogicComponent,
    levels: HashMap<String, bool>,
    ll: LogicLevels,
}

impl Rig {
    fn from_model(model: &hauksbee_models::ModelEntry) -> Self {
        let lc = LogicComponent::compile(&model.id, &model.logic).expect("db spec compiles");
        Rig {
            lc,
            levels: HashMap::new(),
            ll: LogicLevels::from_params(model),
        }
    }

    fn edge(&mut self, pin: &str, high: bool) {
        self.levels.insert(pin.to_string(), high);
        let levels = self.levels.clone();
        let ll = self.ll;
        self.lc.tick(&mut |name, prev| {
            levels
                .get(name)
                .map(|&h| ll.decide(if h { 5.0 } else { 0.0 }, prev))
        });
    }
}

/// shiftOut(MSBFIRST): per bit set SER, pulse SRCLK high/low.
fn seq_shift_out(seq: &mut Vec<(&'static str, bool)>, byte: u8) {
    for bit in (0..8).rev() {
        seq.push(("ser", (byte >> bit) & 1 == 1));
        seq.push(("srclk", true));
        seq.push(("srclk", false));
    }
}

/// The 89-edge 74HC595 sequence from the byte-exact proof, with the golden
/// (shift, store) trajectory checkpoints captured from that run.
#[test]
fn hc595_golden_trajectory_from_byte_exact_proof() {
    let model = builtin("74HC595");
    let mut rig = Rig::from_model(&model);

    let mut seq: Vec<(&str, bool)> = vec![
        ("srclr_n", true),
        ("oe_n", false),
        ("srclk", false),
        ("rclk", false),
    ];
    seq_shift_out(&mut seq, 0xA6);
    seq.push(("rclk", true));
    seq.push(("rclk", false));
    seq_shift_out(&mut seq, 0x5A);
    seq.push(("ser", true));
    seq.push(("ser", false));
    seq.push(("ser", true));
    seq.push(("srclr_n", false));
    seq.push(("srclr_n", true));
    seq.push(("rclk", true));
    seq.push(("rclk", false));
    seq_shift_out(&mut seq, 0x0F);
    seq.push(("rclk", true));
    seq.push(("rclk", false));
    seq.push(("rclk", true));
    seq.push(("rclk", false));
    assert_eq!(seq.len(), 89, "the proven 89-edge sequence");

    // Goldens captured from the two-sided byte-exact run (commit ed80984):
    // (edge index, shift, store).
    let goldens: &[(usize, u64, u64)] = &[
        (8, 0x02, 0x00),
        (16, 0x0a, 0x00),
        (24, 0x53, 0x00),
        (26, 0xa6, 0x00), // 0xA6 fully shifted (8th SRCLK rising edge)
        (28, 0xa6, 0xa6), // RCLK rising latched it
        (32, 0x4c, 0xa6),
        (40, 0x65, 0xa6),
        (48, 0x96, 0xa6),
        (56, 0x5a, 0xa6), // 0x5A shifted, store still holds 0xA6
        (64, 0x00, 0x00), // clear + latch-of-cleared
        (72, 0x00, 0x00),
        (80, 0x07, 0x00),
        (88, 0x0f, 0x0f), // final: 0x0F latched
    ];
    for (i, &(pin, high)) in seq.iter().enumerate() {
        rig.edge(pin, high);
        if let Some(&(_, shift, store)) = goldens.iter().find(|g| g.0 == i) {
            assert_eq!(
                rig.lc.register("shift"),
                Some(shift),
                "edge {i}: shift golden (captured from the byte-exact proof)"
            );
            assert_eq!(
                rig.lc.register("store"),
                Some(store),
                "edge {i}: store golden"
            );
        }
    }
    // Output mapping: qa..qh = store bits 0..7, qh_serial = shift[7].
    for (i, q) in ["qa", "qb", "qc", "qd", "qe", "qf", "qg", "qh"]
        .iter()
        .enumerate()
    {
        assert_eq!(rig.lc.output_level(q), Some((0x0Fu8 >> i) & 1 == 1), "{q}");
    }
    println!("[hc595] 89-edge golden trajectory holds (13 checkpoints)");
}

/// The NOR-latch truth-table walk from the byte-exact proof: the full q
/// trajectory (all 16 edges) is the golden.
#[test]
fn nor_latch_golden_trajectory_from_byte_exact_proof() {
    // The binder-synthesized latch spec, via a real bound component.
    let model = builtin("74HC595"); // levels source only
    let levels = LogicLevels::from_params(&model);
    let mut circuit = Circuit::new();
    let set_n = circuit.node("SPIKE1");
    let reset_n = circuit.node("RESET_SR");
    let q_n = circuit.node("L1");
    let mut roles = HashMap::new();
    roles.insert("set".to_string(), set_n);
    roles.insert("reset".to_string(), reset_n);
    roles.insert("q".to_string(), q_n);
    let mut latch = DigitalComponent::new_nor_latch("U_L1".into(), levels, roles, HashMap::new());

    // (pin, level, expected q after the edge), captured from the proof run.
    let seq: &[(&str, bool, bool)] = &[
        ("set", false, true),
        ("reset", false, true),
        ("reset", true, true),
        ("reset", false, true),
        ("set", true, false),
        ("set", false, false),
        ("set", true, false),
        ("set", false, false),
        ("reset", true, true),
        ("reset", false, true),
        ("set", true, false),
        ("reset", true, false),
        ("reset", false, false),
        ("set", false, false),
        ("reset", true, true),
        ("reset", false, true),
    ];
    let mut pins: HashMap<NodeId, f64> = HashMap::new();
    for (i, &(pin, high, want_q)) in seq.iter().enumerate() {
        let node = if pin == "set" { set_n } else { reset_n };
        pins.insert(node, if high { 4.5 } else { 0.0 });
        let v = pins.clone();
        latch.tick(&mut circuit, &move |n: NodeId| {
            v.get(&n).copied().unwrap_or(0.0)
        });
        assert_eq!(
            latch.output_level("q"),
            Some(want_q),
            "edge {i} ({pin}={high}): q golden from the byte-exact proof"
        );
    }
    println!("[nor_latch] 16-edge golden q trajectory holds");
}

/// The shipping 74HC165 spec follows the DATASHEET shift direction
/// (TI SCLS052I: shifted toward QH, QH emits H, G, F, ... after a load),
/// agreeing with the untouched `Hc165Chain` read path.
#[test]
fn hc165_silicon_direction_matches_datasheet() {
    let model = builtin("74HC165");
    let mut rig = Rig::from_model(&model);
    // Load 0b1010_0001 (a=1, f=1, h=1), emit-order expectation h,g,f,e,d,c,b,a.
    for (p, h) in [
        ("pl_n", true),
        ("clk", false),
        ("clk_inh", false),
        ("ser", false),
        ("a", true),
        ("f", true),
        ("h", true),
    ] {
        rig.edge(p, h);
    }
    rig.edge("pl_n", false);
    rig.edge("pl_n", true);
    let expected = [true, false, true, false, false, false, false, true];
    assert_eq!(
        rig.lc.output_level("qh"),
        Some(expected[0]),
        "QH shows H after load"
    );
    assert_eq!(
        rig.lc.output_level("qh_n"),
        Some(!expected[0]),
        "QH_n complement"
    );
    for want in &expected[1..] {
        rig.edge("clk", true);
        rig.edge("clk", false);
        assert_eq!(
            rig.lc.output_level("qh"),
            Some(*want),
            "datasheet emit order"
        );
    }
    println!("[hc165] datasheet shift direction (SCLS052I) verified: h,g,f,e,d,c,b,a");
}

/// The passthrough fallback (a part with no `[models.logic]`) mirrors wired
/// a*/y* pairs; the old `Buffer` behaviour, golden from the proof run.
#[test]
fn passthrough_fallback_mirrors_wired_pairs() {
    let mut model = builtin("74HC595");
    model.id = "test_buffer".into();
    model.logic = Default::default();
    let mut circuit = Circuit::new();
    let mut roles = HashMap::new();
    let mut nets = HashMap::new();
    for p in ["a1", "a2", "a3", "y1", "y2", "y3"] {
        let n = circuit.node(&p.to_uppercase());
        roles.insert(p.to_string(), n);
        nets.insert(p, n);
    }
    let mut comp = DigitalComponent::new("U_BUF".into(), &model, roles, HashMap::new())
        .expect("passthrough synthesizes");

    let mut pins: HashMap<NodeId, f64> = HashMap::new();
    for pat in [0b001u8, 0b011, 0b010, 0b110, 0b111, 0b101, 0b000, 0b100] {
        pins.insert(nets["a1"], if pat & 1 != 0 { 5.0 } else { 0.0 });
        pins.insert(nets["a2"], if pat & 2 != 0 { 5.0 } else { 0.0 });
        pins.insert(nets["a3"], if pat & 4 != 0 { 5.0 } else { 0.0 });
        let v = pins.clone();
        comp.tick(&mut circuit, &move |n: NodeId| {
            v.get(&n).copied().unwrap_or(0.0)
        });
        for (i, y) in ["y1", "y2", "y3"].iter().enumerate() {
            assert_eq!(
                comp.output_level(y),
                Some(pat & (1 << i) != 0),
                "passthrough mirrors a{} -> {y} for pattern {pat:03b}",
                i + 1
            );
        }
    }
    println!("[buffer] passthrough fallback mirrors all 8 patterns");
}

/// STILL TWO-SIDED: a 4-chip spec-driven daisy chain (qh_serial -> next ser
/// through emulated nets with simultaneous-clock overlay semantics) versus
/// the legacy [`Hc595Chain`] controller, independent Rust that survives the
/// migration as the MCU-facing fast path. Byte-exact per chip per edge.
#[test]
fn spec_chain_matches_hc595chain_reference() {
    const N: usize = 4;
    let model = builtin("74HC595");
    let levels = LogicLevels::from_params(&model);
    let weights: [u8; N] = [0x11, 0x22, 0x33, 0x44];

    let logic: Logic = model.logic.clone();
    let mut chips: Vec<LogicComponent> = (0..N)
        .map(|_| LogicComponent::compile("chain", &logic).expect("compiles"))
        .collect();
    let mut ctrl: HashMap<&str, bool> = HashMap::new();
    let mut ser_in = vec![false; N];

    // Legacy reference controller over the same edge log.
    let mut circuit = Circuit::new();
    let mut ref_chips: Vec<DigitalComponent> = Vec::new();
    let mut prev_qh: Option<NodeId> = None;
    let n_srclk = circuit.node("SRCLK");
    let n_rclk = circuit.node("RCLK");
    let n_srclr = circuit.node("SRCLR");
    let n_ser0 = circuit.node("SER0");
    for k in 0..N {
        let mut roles = HashMap::new();
        roles.insert("srclk".to_string(), n_srclk);
        roles.insert("rclk".to_string(), n_rclk);
        roles.insert("srclr_n".to_string(), n_srclr);
        roles.insert("ser".to_string(), prev_qh.unwrap_or(n_ser0));
        let qh = circuit.node(&format!("QHS{k}"));
        roles.insert("qh_serial".to_string(), qh);
        ref_chips.push(
            DigitalComponent::new(format!("U{k}"), &model, roles, HashMap::new())
                .expect("builtin 595 logic compiles"),
        );
        prev_qh = Some(qh);
    }
    let mut gpio: HashMap<i64, (char, u8)> = HashMap::new();
    gpio.insert(n_srclk.0 as i64, ('B', 5));
    gpio.insert(n_rclk.0 as i64, ('D', 6));
    gpio.insert(n_srclr.0 as i64, ('C', 3));
    gpio.insert(n_ser0.0 as i64, ('B', 3));
    let order = hauksbee_engine::digital::order_595_chain(&ref_chips);
    let mut reference = Hc595Chain::build(&ref_chips, order, &gpio).expect("reference chain binds");

    let mut log: Vec<(&str, bool)> = vec![("srclr_n", true)];
    for &b in &weights {
        for bit in (0..8).rev() {
            log.push(("ser", (b >> bit) & 1 == 1));
            log.push(("srclk", true));
            log.push(("srclk", false));
        }
    }
    log.push(("rclk", true));
    log.push(("rclk", false));

    for (i, &(pin, high)) in log.iter().enumerate() {
        let mcu_pin = match pin {
            "srclk" => ('B', 5),
            "rclk" => ('D', 6),
            "srclr_n" => ('C', 3),
            "ser" => ('B', 3),
            _ => unreachable!(),
        };
        reference.replay(&[(mcu_pin.0, mcu_pin.1, high)]);

        if pin == "ser" {
            ser_in[0] = high;
        } else {
            ctrl.insert(pin, high);
        }
        let pre_ser = ser_in.clone();
        for (k, chip) in chips.iter_mut().enumerate() {
            let ctrl_snapshot = ctrl.clone();
            let ser_level = pre_ser[k];
            chip.tick(&mut |name, prev| {
                let v = match name {
                    "ser" => ser_level,
                    "oe_n" => return None, // unwired, tied enabled
                    other => ctrl_snapshot.get(other).copied().unwrap_or(false),
                };
                Some(levels.decide(if v { 5.0 } else { 0.0 }, prev))
            });
        }
        for k in 0..N - 1 {
            ser_in[k + 1] = chips[k].output_level("qh_serial").expect("tap");
        }

        for k in 0..N {
            assert_eq!(
                chips[k].register("shift"),
                Some(reference.shift[k] as u64),
                "edge {i}: chip {k} shift register diverged from Hc595Chain"
            );
            assert_eq!(
                chips[k].register("store"),
                Some(reference.latched[k] as u64),
                "edge {i}: chip {k} storage register diverged from Hc595Chain"
            );
        }
    }

    for p in 0..N {
        assert_eq!(
            chips[p].register("store"),
            Some(weights[N - 1 - p] as u64),
            "chain position {p} latches weights[{}]",
            N - 1 - p
        );
    }
    println!(
        "[chain] {} edges x {N} chips compared byte-exact against Hc595Chain; \
         final latched: {:?}",
        log.len(),
        chips
            .iter()
            .map(|c| c.register("store").unwrap())
            .collect::<Vec<_>>()
    );
}
