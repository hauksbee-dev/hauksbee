//! Generic signal-diode fallback: value-"D" parts must bind to a CONDUCTING
//! 1N4148 model, not bind OPEN.
//!
//! The KiCad stock "Device:D" symbol carries value "D" with no MPN, so the
//! model db cannot resolve it. Leaving such a part OPEN silently deletes its
//! conduction path. The binder falls back to a generic 1N4148
//! (datasheet-grounded params), the same way it falls back to generic R/C/L
//! for unresolved passives.

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
            Device::Diode { name, model, a, k } if name == "D_stretch601" => Some((*model, *a, *k)),
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
    assert!(
        model.rs > 0.0,
        "1N4148 has a nonzero series Rs, got {}",
        model.rs
    );

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

/// A bank of generic diodes must all bind as conducting devices rather than
/// landing in the OPEN/unresolved bucket. The fixture is generated here so the
/// release test remains self-contained.
#[test]
fn many_generic_diodes_bind_conducting() {
    let mut components = String::new();
    let mut nets = String::new();
    for i in 0..32 {
        let reference = format!("D_BANK{i}");
        components.push_str(&format!(
            r#"(comp (ref "{reference}")
      (value "D")
      (footprint "Diode_SMD:D_SOD-323")
      (libsource (lib "Device") (part "D") (description "Diode")))"#
        ));
        nets.push_str(&format!(
            r#"(net (code "{}") (name "A{i}")
      (node (ref "{reference}") (pin "2") (pinfunction "A") (pintype "passive")))
    (net (code "{}") (name "K{i}")
      (node (ref "{reference}") (pin "1") (pinfunction "K") (pintype "passive")))"#,
            i * 2 + 1,
            i * 2 + 2
        ));
    }
    let netlist = format!(
        r#"(export (version "E")
  (components {components})
  (nets {nets}))"#
    );
    let board = ExtractedBoard::from_auto(&netlist).expect("parse generated diode bank");
    let lib = ModelLibrary::builtin();
    let bound = bind_board(&board, &lib);

    let d_diodes: Vec<_> = bound
        .circuit
        .devices
        .iter()
        .filter_map(|d| match d {
            Device::Diode { name, model, .. } if name.starts_with("D_BANK") => {
                Some((name.clone(), *model))
            }
            _ => None,
        })
        .collect();

    assert_eq!(d_diodes.len(), 32, "every wired generic diode binds");
    for (name, model) in &d_diodes {
        assert!(
            (model.is - 4.352e-9).abs() < 1e-12,
            "{name}: should carry the 1N4148 fallback Is, got {:e}",
            model.is
        );
    }
}
