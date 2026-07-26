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

use hauksbee_engine::bind_board;
use hauksbee_engine::binder::bind_board_with;
use hauksbee_extract::ExtractedBoard;
use hauksbee_ir::Device;
use hauksbee_models::ModelLibrary;

/// A KiCad PCB with one 2-pin diode (value 1N4148, SOD-323 body) wired between
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

/// The same diode as a KiCad netlist, carrying explicit `pinfunction "A"/"K"`.
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

fn diode_device(bound: &hauksbee_engine::BoundBoard) -> Option<(hauksbee_ir::NodeId, hauksbee_ir::NodeId)> {
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
    let joined = guesses.iter().map(|(_, g)| *g).collect::<Vec<_>>().join("\n");
    assert!(joined.contains("anode"), "names the anode role: {joined}");
    assert!(joined.contains("cathode"), "names the cathode role: {joined}");
    assert!(joined.contains("diode_2pin_k1_a2"), "names the matched rule: {joined}");
    assert!(joined.contains("pad"), "names a pad: {joined}");
}

#[test]
fn explicit_pinfunction_diode_binds_with_no_guess() {
    let board = ExtractedBoard::from_auto(NETLIST_DIODE_WITH_PINFUNC).expect("parse netlist");
    let lib = ModelLibrary::builtin();
    let bound = bind_board(&board, &lib);

    // Binds, polarity from the explicit pinfunction.
    let (anode, cathode) =
        diode_device(&bound).expect("diode with explicit A/K must bind");
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

    let (anode, cathode) =
        diode_device(&bound).expect("diode binds under the user rule");
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
