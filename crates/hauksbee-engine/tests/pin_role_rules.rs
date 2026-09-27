//! Configurable pin-role inference (issue #64).
//!
//! A layout-only source (a `.kicad_pcb`, or Board-as-Code decompiled from one)
//! gives a diode pads `1`/`2` with no electrode role, so the role-dependent
//! diode binder cannot tell anode from cathode and the part fails to bind. The
//! pin-rule table recovers the role from the footprint + kind + pad count, and
//! every inferred role is surfaced as a GUESS warning. These tests pin the three
//! behaviours that matter:
//!
//! 1. A layout-only 2-pin diode (pads 1/2, no roles, value 1N4148) BINDS via the
//!    rule and EMITS a guess-warning naming the pad, role, and rule.
//! 2. A diode WITH explicit pinfunction A/K binds with NO guess-warning (the
//!    explicit role wins; the rule never fires).
//! 3. A user-supplied rule overrides the built-in (different pad→role map), and
//!    its id appears in the guess-warning.

use crate::support::{combined, run};
use hauksbee_engine::binder::bind_board_with;
use hauksbee_engine::{bind_board, BindOutcome, BoardEvidence};
use hauksbee_extract::ExtractedBoard;
use hauksbee_ir::evidence::{Assumption, EvidenceStatus, RunDate};
use hauksbee_ir::Device;
use hauksbee_models::ModelLibrary;

/// A `KiCad` PCB with one 2-pin diode (value 1N4148, SOD-323 body) wired between
/// two nets, pads numbered 1/2 with NO pinfunction; the layout-only case.
const LAYOUT_DIODE_PCB: &str = r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "CATHODE_NET")
  (net 2 "ANODE_NET")
  (module Diode_SMD:D_SOD-323 (layer F.Cu)
    (fp_text reference D1 (at 0 0) (layer F.SilkS))
    (fp_text value 1N4148 (at 0 0) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (size 1 1) (layers F.Cu) (net 1 "CATHODE_NET"))
    (pad 2 smd rect (at 1 0) (size 1 1) (layers F.Cu) (net 2 "ANODE_NET")))
  (module Resistor_SMD:R_0402_1005Metric (layer F.Cu)
    (fp_text reference R1 (at 0 0) (layer F.SilkS))
    (fp_text value 10k (at 0 0) (layer F.Fab))
    (pad 1 smd rect (at 5 0) (size 1 1) (layers F.Cu) (net 2 "ANODE_NET"))
    (pad 2 smd rect (at 6 0) (size 1 1) (layers F.Cu) (net 1 "CATHODE_NET"))))
"#;

/// The same diode as a `KiCad` netlist, carrying explicit `pinfunction "A"/"K"`.
const NETLIST_DIODE_WITH_PINFUNC: &str = r#"(export (version "E")
  (components
    (comp (ref "D1")
      (value "1N4148")
      (footprint "Diode_SMD:D_SOD-323")
      (libsource (lib "Device") (part "D") (description "Diode")))
    (comp (ref "R1")
      (value "10k")
      (footprint "Resistor_SMD:R_0402_1005Metric")))
  (nets
    (net (code "1") (name "ANODE_NET")
      (node (ref "D1") (pin "2") (pinfunction "A") (pintype "passive"))
      (node (ref "R1") (pin "1") (pintype "passive")))
    (net (code "2") (name "CATHODE_NET")
      (node (ref "D1") (pin "1") (pinfunction "K") (pintype "passive"))
      (node (ref "R1") (pin "2") (pintype "passive")))))
"#;

fn diode_device(
    bound: &hauksbee_engine::BoundBoard,
) -> Option<(hauksbee_ir::NodeId, hauksbee_ir::NodeId)> {
    bound.circuit.devices.iter().find_map(|d| match d {
        Device::Diode { name, a, k, .. } if name == "D1" => Some((*a, *k)),
        _ => None,
    })
}

#[test]
fn layout_only_diode_binds_via_rule_with_guess_warning() {
    let board = ExtractedBoard::from_auto(LAYOUT_DIODE_PCB).expect("parse pcb");
    let lib = ModelLibrary::builtin();
    let bound = bind_board(&board, &lib);

    // It BINDS (a real Device::Diode stamped, not left open).
    let (anode, cathode) = diode_device(&bound)
        .expect("layout-only diode must bind via the pin-rule table, not land open");

    // Polarity follows the rule: pad1=cathode (CATHODE_NET), pad2=anode (ANODE_NET).
    assert_eq!(bound.net_nodes.get("ANODE_NET").copied(), Some(anode));
    assert_eq!(bound.net_nodes.get("CATHODE_NET").copied(), Some(cathode));

    // A guess-warning fired, naming the component, a pad, the guessed role, and
    // the rule id.
    let guesses: Vec<_> = bound
        .report
        .guess_warnings()
        .filter(|(r, _)| *r == "D1")
        .collect();
    assert!(
        guesses.len() == 2,
        "expected anode+cathode guess-warnings for D1, got {guesses:?}"
    );
    let joined = guesses
        .iter()
        .map(|(_, g)| *g)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(joined.contains("anode"), "names the anode role: {joined}");
    assert!(
        joined.contains("cathode"),
        "names the cathode role: {joined}"
    );
    assert!(
        joined.contains("diode_2pin_k1_a2"),
        "names the matched rule: {joined}"
    );
    assert!(joined.contains("pad"), "names a pad: {joined}");

    // Each guess is a disclosed pin-role assumption: a verdict on D1 is Qualified.
    let evidence = BoardEvidence::from_bound(&board, &bound.report, &[], RunDate::unknown());
    let evidence = evidence.unwrap();
    let said = |a: &Assumption| a.statement() == "D1 pin 2 is taken to be its anode pin.";
    assert!(evidence.assumptions().iter().any(said));
    let map = evidence.ci_assertion_map("d1", &["ANODE_NET".into()], &[], None, None);
    assert_eq!(map.unwrap().status(), EvidenceStatus::Qualified);
}

#[test]
fn explicit_pinfunction_diode_binds_with_no_guess() {
    let board = ExtractedBoard::from_auto(NETLIST_DIODE_WITH_PINFUNC).expect("parse netlist");
    let lib = ModelLibrary::builtin();
    let bound = bind_board(&board, &lib);

    // Binds, polarity from the explicit pinfunction.
    let (anode, cathode) = diode_device(&bound).expect("diode with explicit A/K must bind");
    assert_eq!(bound.net_nodes.get("ANODE_NET").copied(), Some(anode));
    assert_eq!(bound.net_nodes.get("CATHODE_NET").copied(), Some(cathode));

    // No guess-warning: the role came from the explicit pin-function, not a rule.
    let d1_guesses: Vec<_> = bound
        .report
        .guess_warnings()
        .filter(|(r, _)| *r == "D1")
        .collect();
    assert!(
        d1_guesses.is_empty(),
        "explicit-pinfunction diode must emit NO guess, got {d1_guesses:?}"
    );
}

#[test]
fn user_rule_overrides_builtin_and_is_named_in_guess() {
    // A user pin_rules.toml dropped into a model dir, FLIPPING the diode
    // convention (pad1=anode, pad2=cathode) for the SOD family. The user rule is
    // prepended, so it wins over the built-in diode_2pin_k1_a2.
    let tmp = std::env::temp_dir().join(format!("hauksbee_pinrule_{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    std::fs::write(
        tmp.join("pin_rules.toml"),
        r#"
        [[pin_rules]]
        id = "house_diode_flip"
        description = "House footprint: pad1=anode, pad2=cathode"
        footprint_re = "SOD-"
        kind = "diode"
        pad_count = 2
        roles = { "1" = "anode", "2" = "cathode" }
        "#,
    )
    .unwrap();

    let lib = ModelLibrary::builtin_with_user_dirs(&[tmp.as_path()]);
    let custom = hauksbee_engine::CustomRegistry::default();
    let board = ExtractedBoard::from_auto(LAYOUT_DIODE_PCB).expect("parse pcb");
    let bound = bind_board_with(&board, &lib, &custom);

    let (anode, cathode) = diode_device(&bound).expect("diode binds under the user rule");
    // Flipped: pad1 (CATHODE_NET) is now the ANODE, pad2 (ANODE_NET) the CATHODE.
    assert_eq!(
        bound.net_nodes.get("CATHODE_NET").copied(),
        Some(anode),
        "user rule flips pad1 to anode"
    );
    assert_eq!(
        bound.net_nodes.get("ANODE_NET").copied(),
        Some(cathode),
        "user rule flips pad2 to cathode"
    );

    let joined = bound
        .report
        .guess_warnings()
        .filter(|(r, _)| *r == "D1")
        .map(|(_, g)| g.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        joined.contains("house_diode_flip"),
        "guess must name the user rule, got: {joined}"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

/// A layout-only SOT-23 N-MOSFET (value TESTFET1): pads 1/2/3 on N1/N2/N3.
const LAYOUT_FET_PCB: &str = r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0) (net 0 "") (net 1 "N1") (net 2 "N2") (net 3 "N3")
  (module Package_TO_SOT_SMD:SOT-23 (layer F.Cu)
    (fp_text reference Q1 (at 0 0) (layer F.SilkS)) (fp_text value TESTFET1 (at 0 0) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (size 1 1) (layers F.Cu) (net 1 "N1"))
    (pad 2 smd rect (at 1 0) (size 1 1) (layers F.Cu) (net 2 "N2"))
    (pad 3 smd rect (at 2 0) (size 1 1) (layers F.Cu) (net 3 "N3"))))
"#;

/// Its card: gate and source printed at unnumbered leads, numbered `@standard`.
const FET_CARD: &str = r#"[[models]]
id = "testfet1"
kind = "nmos"
description = "Test N-MOSFET, SOT-23 package"
match = { value_re = "(?i)^TESTFET1$" }
params = { vto = 1.5, kp = 0.2 }
pins = { @pins, "3" = "drain" }
[models.source]
tier = "datasheet-derived"
[[models.source.pinout]]
package = "SOT-23"
@standard
pins = { @pins, "3" = "drain" }
"#;

/// Standard-numbered pads bind as assumptions on the one evidence path CI, the app
/// and CLI share; a disagreeing SOT-23 pin rule refuses them, never a printed table.
#[test]
fn standard_numbered_pads_are_assumptions_and_a_pin_rule_clash_refuses_them() {
    let jedec = "JEDEC TO-236 top view: 1 lower-left, 2 lower-right, 3 top";
    let (gs, sg) = (
        r#""1" = "gate", "2" = "source""#,
        r#""1" = "source", "2" = "gate""#,
    );
    let line = format!("standard = \"{jedec}\"");
    // Case: numbered from the standard, pads 1 and 2 | Q1 binds, pads guessed.
    for (standard, pins, binds, guessed) in [
        (line.as_str(), gs, true, 3),
        (&line, sg, false, 0),
        ("", sg, true, 0),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let card = dir.path().join("testfet1.toml");
        let text = FET_CARD.replace("@standard", standard);
        std::fs::write(&card, text.replace("@pins", pins)).unwrap();
        let lib = ModelLibrary::builtin_with_user_dirs(&[dir.path()]);
        let board = ExtractedBoard::from_auto(LAYOUT_FET_PCB).expect("parse pcb");
        let bound = bind_board_with(&board, &lib, &hauksbee_engine::CustomRegistry::default());
        let row = bound.report.row("Q1").unwrap();
        let warning = row.warning.as_deref().unwrap_or_default();
        let analog = matches!(row.outcome, BindOutcome::Analog { .. });
        let clash = warning.contains("as pin rule 'nmos_sot23_g1_s2_d3' reads it");
        let assumed = row.guesses.iter().filter(|g| g.contains(jedec)).count();
        let got = (analog, clash, assumed);
        assert_eq!(got, (binds, !binds, guessed), "{row:?}");
        if guessed > 0 {
            let today = RunDate::unknown();
            let evidence = BoardEvidence::from_bound(&board, &bound.report, &[], today).unwrap();
            let pad1 = |a: &Assumption| a.statement() == "Q1 pin 1 is taken to be its gate pin.";
            let said = |a: &Assumption| pad1(a) && a.because().contains("the package standard");
            assert!(evidence.assumptions().iter().any(said), "{row:?}");
            let map = evidence.ci_assertion_map("q1", &["N1".into()], &[], None, None);
            assert_eq!(map.unwrap().status(), EvidenceStatus::Qualified);
            // `models lint` tells an extraction agent which pads it numbered.
            let checked = combined(&run(&["models", "lint", card.to_str().unwrap()]));
            assert!(
                checked.contains("pads are numbered from package"),
                "{checked}"
            );
        }
    }
}
