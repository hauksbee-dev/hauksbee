//! Which open parts may stop undermining an assertion, and which never do.
//!
//! An unresolved part is simulated as an open circuit, and by default it is
//! critical to every assertion it touches: it "might carry current". The one
//! relaxation (`open_part_bound` in hauksbee-engine's evidence) is a part with a
//! single terminal on ground on a geometric board: it closes no loop whatever it
//! is, and anything it reaches off the board lands on the reference node, so the
//! open circuit the run simulated is exact. It stays disclosed as a
//! reduced-fidelity qualification.
//!
//! Each control below is a legal board that goes green with the part disclosed;
//! each fault is an input that must stay INVALID or fail. The clamp faults pin
//! the rules this crate deliberately does NOT have: an open part carries no
//! card, so no standoff or polarity bounds a TVS, and it stays critical.

use crate::support;
use hauksbee_ci::report::CiResult;
use hauksbee_ir::evidence::AssumptionKind;

/// A 10k/10k divider from +5V, plus one extra footprint `part` (a full
/// `(module ...)` form) on the same nets.
fn board(part: &str) -> String {
    let resistor = |reference: &str, a: (u8, &str), b: (u8, &str), y: u32| {
        format!(
            "  (module Resistor_SMD:R_0603_1608Metric (layer F.Cu) (at 100 {y})\n\
             \x20   (fp_text reference {reference} (at 0 0) (layer F.SilkS))\n\
             \x20   (fp_text value 10k (at 0 2) (layer F.Fab))\n\
             \x20   (pad 1 smd rect (at 0 0) (size 1 1) (net {} \"{}\"))\n\
             \x20   (pad 2 smd rect (at 2 0) (size 1 1) (net {} \"{}\")))\n",
            a.0, a.1, b.0, b.1
        )
    };
    format!(
        "(kicad_pcb (version 20171130) (host pcbnew 5.1.0)\n\
         \x20 (net 0 \"\")\n  (net 1 \"GND\")\n  (net 2 \"+5V\")\n  (net 3 \"VOUT\")\n{}{}{part})\n",
        resistor("R1", (2, "+5V"), (3, "VOUT"), 100),
        resistor("R2", (3, "VOUT"), (1, "GND"), 110),
    )
}

/// One footprint `reference`/`value` whose pads are `(number, net id, net name)`.
fn part(reference: &str, value: &str, pads: &[(&str, u8, &str)]) -> String {
    let pads: Vec<String> = pads
        .iter()
        .enumerate()
        .map(|(i, (number, id, name))| {
            format!(
                "    (pad {number} smd rect (at {} 0) (size 1 1) (net {id} \"{name}\"))\n",
                i * 2
            )
        })
        .collect();
    let pads = pads.concat();
    format!(
        "  (module Custom:Mystery (layer F.Cu) (at 120 100)\n\
         \x20   (fp_text reference {reference} (at 0 0) (layer F.SilkS))\n\
         \x20   (fp_text value {value} (at 0 2) (layer F.Fab))\n{pads}  )\n"
    )
}

fn run(name: &str, extra: &str) -> CiResult {
    let board = support::write_tmp(&format!("{name}.kicad_pcb"), &board(extra));
    support::run_body(
        &format!("{name}.toml"),
        &format!(
            "name = \"{name}\"\nboard = {}\nduration_ms = 1\n\
             [[supply]]\nnet = \"+5V\"\nkind = \"ideal\"\nvolts = 5.0\n\
             [[assert]]\nkind = \"voltage\"\nnet = \"VOUT\"\nmin = 2.4\nmax = 2.6\n\
             [[assert]]\nkind = \"no_faults\"\n",
            support::toml_path(&board)
        ),
    )
}

/// Control: an unidentified single-pad part on ground (a shield tab, say). It
/// closes no current path, so the board is green, and the part is still disclosed.
#[test]
fn a_single_ground_terminal_open_part_is_disclosed_not_critical() {
    let result = run("single_pad", &part("U9", "MYSTERY-PAD", &[("1", 1, "GND")]));
    support::assert_green(&result, "a one-terminal open part cannot change any result");
    let u9 = result
        .assumptions
        .iter()
        .find(|a| a.statement().contains("U9"))
        .expect("U9 stays disclosed");
    assert_eq!(u9.kind(), AssumptionKind::ReducedFidelity, "{u9:?}");
    assert!(u9.consequence().contains("single terminal"), "{u9:?}");
    assert!(
        result.render_human().contains("U9"),
        "every surface names it"
    );
}

/// Fault: one pad on a signal net. It may be a contact or wire pad carrying
/// current from off the board, so one terminal alone bounds nothing there.
#[test]
fn a_single_terminal_off_ground_stays_critical() {
    let result = run(
        "signal_pad",
        &part("U9", "MYSTERY-PAD", &[("1", 3, "VOUT")]),
    );
    support::assert_invalid_by_open_parts(&result, &["U9"]);
}

/// Fault: the same unknown part with a second terminal to ground. It might be
/// a clamp, a load or a short, so it undermines both assertions.
#[test]
fn an_unknown_two_terminal_part_stays_critical() {
    let pads = [("1", 3, "VOUT"), ("2", 1, "GND")];
    let result = run("unknown_clamp", &part("U9", "MYSTERY-CLAMP", &pads));
    support::assert_invalid_by_open_parts(&result, &["U9"]);
}

/// Fault: two pads on one net (ground, the net the rule otherwise accepts). A charged part shorted by its own copper is
/// stressed where no check sees it, so this is not a single terminal.
#[test]
fn several_pads_on_one_net_stay_critical() {
    let pads = [("1", 1, "GND"), ("2", 1, "GND")];
    let result = run("shorted_pads", &part("U9", "MYSTERY-CELL", &pads));
    support::assert_invalid_by_open_parts(&result, &["U9"]);
}

/// Faults for the TVS rule this crate does not have: an unidentified clamp
/// across the 5 V rail, either way round, with a standoff above or below it.
/// Nothing on an open part states its standoff or polarity, so none of them
/// may come back green.
#[test]
fn open_clamps_across_a_rail_never_go_green() {
    for (name, value, cathode, anode) in [
        ("tvs_ok", "MYSTERY-TVS-6V", (2, "+5V"), (1, "GND")),
        ("tvs_reversed", "MYSTERY-TVS-6V", (1, "GND"), (2, "+5V")),
        (
            "tvs_low_standoff",
            "MYSTERY-TVS-3V3",
            (2, "+5V"),
            (1, "GND"),
        ),
    ] {
        let pads = [("1", cathode.0, cathode.1), ("2", anode.0, anode.1)];
        let result = run(name, &part("U9", value, &pads));
        support::assert_invalid_by_open_parts(&result, &["U9"]);
    }
}
