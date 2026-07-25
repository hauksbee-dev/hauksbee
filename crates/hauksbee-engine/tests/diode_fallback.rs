//! Generic signal-diode fallback: value-"D" parts must bind to a CONDUCTING
//! 1N4148 model, not bind OPEN.
//!
//! The KiCad stock "Device:D" symbol carries value "D" with no MPN, so the
//! model db cannot resolve it. On the Tarski board the ~94 D_stretch/D_inject/
//! D_hyst pulse-stretcher diodes are exactly this case; leaving them OPEN
//! silently deletes the comparator->spike charge path. The binder now falls
//! back to a generic 1N4148 (datasheet-grounded params) for such parts, the
//! same way it falls back to generic R/C/L for unresolved passives.

mod common;

use hauksbee_engine::binder::bind_board;
use hauksbee_extract::ExtractedBoard;
use hauksbee_ir::Device;
use hauksbee_models::ModelLibrary;

/// Build the smallest possible KiCad netlist with one value-"D" diode wired
/// between two named nets, and assert it binds to a conducting diode with the
/// 1N4148 fallback parameters (not the open default, not a generic Is=1e-14).
#[test]
fn value_d_diode_binds_conducting_1n4148() {
    // Minimal KiCad netlist: a single Device:D between nets A and K, plus two
    // resistors so the nets exist and are non-trivial.
    let net = r#"(export (version "E")
  (components
    (comp (ref "D_stretch601")
      (value "D")
      (footprint "Diode_SMD:D_SOD-323")
      (libsource (lib "Device") (part "D") (description "Diode")))
    (comp (ref "R1")
      (value "10k")
      (footprint "Resistor_SMD:R_0402_1005Metric")))
  (nets
    (net (code "1") (name "ANODE_NET")
      (node (ref "D_stretch601") (pin "2") (pinfunction "A") (pintype "passive"))
      (node (ref "R1") (pin "1") (pintype "passive")))
    (net (code "2") (name "CATHODE_NET")
      (node (ref "D_stretch601") (pin "1") (pinfunction "K") (pintype "passive"))
      (node (ref "R1") (pin "2") (pintype "passive")))))
"#;
    let board = ExtractedBoard::from_auto(net).expect("parse netlist");
    let lib = ModelLibrary::builtin();
    let bound = bind_board(&board, &lib);

    let diode = bound
        .circuit
        .devices
        .iter()
        .find_map(|d| match d {
            Device::Diode { name, model, a, k } if name == "D_stretch601" => {
                Some((*model, *a, *k))
            }
            _ => None,
        })
        .expect("value-\"D\" diode must bind to a Device::Diode (not OPEN)");
    let (model, anode, cathode) = diode;

    // Conducting: a real saturation current in the 1N4148 range (nA), NOT the
    // bare DiodeModel::default() Is=1e-14 (which would be a different, far
    // stiffer junction) and NOT zero (open).
    assert!(
        model.is > 1e-9 && model.is < 1e-8,
        "Is should be the 1N4148 ~4.35nA fallback, got {:e}",
        model.is
    );
    assert!(
        (model.is - 4.352e-9).abs() < 1e-12,
        "Is = Philips/Vishay 1N4148 4.352nA, got {:e}",
        model.is
    );
    assert!(
        (model.n - 1.906).abs() < 1e-6,
        "N = 1N4148 emission 1.906, got {}",
        model.n
    );
    assert!(model.rs > 0.0, "1N4148 has a nonzero series Rs, got {}", model.rs);

    // Polarity: anode is the pinfunction-"A" net, cathode the "K" net.
    assert_eq!(
        bound.net_nodes.get("ANODE_NET").copied(),
        Some(anode),
        "anode bound to the A-pin net"
    );
    assert_eq!(
        bound.net_nodes.get("CATHODE_NET").copied(),
        Some(cathode),
        "cathode bound to the K-pin net"
    );
}

/// On the real Tarski netlist, the D_stretch/D_inject/D_hyst diodes must now
/// bind as conducting devices rather than landing in the OPEN/unresolved bucket.
#[test]
fn tarski_stretcher_diodes_bind_conducting() {
    let board = common::tarski_board();
    let lib = ModelLibrary::builtin();
    let bound = bind_board(&board, &lib);

    let d_diodes: Vec<_> = bound
        .circuit
        .devices
        .iter()
        .filter_map(|d| match d {
            Device::Diode { name, model, .. } if name.starts_with("D_") => {
                Some((name.clone(), *model))
            }
            _ => None,
        })
        .collect();

    // The board has many wired D_ diodes; every one that is wired at both ends
    // must now be a conducting 1N4148, not open.
    assert!(
        d_diodes.len() >= 30,
        "expected the wired D_ stretcher/inject diodes to bind, got {}",
        d_diodes.len()
    );
    for (name, model) in &d_diodes {
        assert!(
            (model.is - 4.352e-9).abs() < 1e-12,
            "{name}: should carry the 1N4148 fallback Is, got {:e}",
            model.is
        );
    }
}
