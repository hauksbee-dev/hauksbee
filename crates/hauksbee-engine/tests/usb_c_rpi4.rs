//! The flagship derivation: re-derive the Raspberry Pi 4 USB-C shared-CC-pulldown
//! fault cold, from the reconstructed schematic and the USB Type-C spec
//! thresholds alone.
//!
//! Two layers of assertion:
//!   1. `classify_attach` over explicit `SinkTermination`s, the pure physics:
//!      always runs, carries the tight numeric bounds. This is the core
//!      derivation and does not depend on the board-corpus symlink.
//!   2. End-to-end over the reconstructed `.kicad_sch` files in
//!      `board-corpus/famous/rpi4_usbc_reconstruction/`: parses the schematic,
//!      extracts the CC termination, and asserts the same 2x2 matrix. Skipped
//!      with a message when the corpus is not checked out (matching the
//!      `drc_corpus` pattern), so `cargo test --workspace` stays green either way.
//!
//! The 2x2 matrix is (as-designed shared R79 / repaired independent Rd) x
//! (passive cable / e-marked cable), all at the Default USB Rp (80 µA).

use std::path::PathBuf;

use hauksbee_engine::checks::usb_c::{
    classify_attach, classify_board, extract_sink_termination, Attach, Cable, PinState, Rp,
    SinkTermination,
};
use hauksbee_extract::ExtractedBoard;

/// Shared 5.1k (the RPi 4 rev 1.0/1.1 defect): CC1 and CC2 are one net.
fn as_designed() -> SinkTermination {
    SinkTermination {
        cc1_rd_ohms: Some(5100.0),
        cc2_rd_ohms: Some(5100.0),
        shared_net: true,
    }
}

/// Independent 5.1k per CC pin (the rev 1.2 repair).
fn repaired() -> SinkTermination {
    SinkTermination {
        cc1_rd_ohms: Some(5100.0),
        cc2_rd_ohms: Some(5100.0),
        shared_net: false,
    }
}

/// Assert a value is within `tol` of `want`.
fn near(got: f64, want: f64, tol: f64, what: &str) {
    assert!(
        (got - want).abs() <= tol,
        "{what}: got {got:.5}, want {want:.5} (tol {tol})"
    );
}

// ---------------------------------------------------------------------------
// Layer 1: the pure-physics 2x2 matrix (always runs)
// ---------------------------------------------------------------------------

#[test]
fn as_designed_passive_cable_powers() {
    // A dumb cable wires only one CC through, so the lone shared 5.1k reads as a
    // normal Rd. This is why the bug shipped: it works with dumb cables.
    let r = classify_attach(as_designed(), Rp::Default, Cable::Passive);
    // 80 µA into 5.1k = 0.408 V, in the vRd window.
    near(r.cc1_v, 0.408, 0.002, "as-designed/passive CC1");
    assert_eq!(r.cc1_state, PinState::Rd);
    assert_eq!(r.cc2_state, PinState::Open);
    assert_eq!(r.attach, Attach::SinkAttached);
    assert!(r.powers(), "must power with a passive cable");
}

#[test]
fn as_designed_emarked_cable_is_audio_accessory_no_power() {
    // THE FAMOUS FAULT. An e-marked cable presents Ra on the VCONN CC line.
    // Because R79 shorts CC1 to CC2, BOTH source pins drive one node sitting at
    // 5.1k || 1k, dragging both CC voltages into the Ra band. Two Ra
    // terminations => Audio Adapter Accessory => the source withholds VBUS.
    let r = classify_attach(as_designed(), Rp::Default, Cable::emarked());
    // Solver: two 80 µA sources into 5.1k||1k = 836.07 Ohm => 0.13377 V.
    near(r.cc1_v, 0.13377, 0.001, "as-designed/e-marked CC1");
    near(r.cc2_v, 0.13377, 0.001, "as-designed/e-marked CC2");
    // Both below the 0.20 V vRa threshold (Table 4-28).
    assert!(r.cc1_v < r.thresholds.vra_max);
    assert!(r.cc2_v < r.thresholds.vra_max);
    assert_eq!(r.cc1_state, PinState::Ra);
    assert_eq!(r.cc2_state, PinState::Ra);
    assert_eq!(r.attach, Attach::AudioAccessory);
    assert!(!r.powers(), "the board appears dead: VBUS withheld");
}

#[test]
fn repaired_passive_cable_powers() {
    let r = classify_attach(repaired(), Rp::Default, Cable::Passive);
    near(r.cc1_v, 0.408, 0.002, "repaired/passive CC1");
    assert_eq!(r.cc1_state, PinState::Rd);
    assert_eq!(r.cc2_state, PinState::Open);
    assert_eq!(r.attach, Attach::SinkAttached);
    assert!(r.powers());
}

#[test]
fn repaired_emarked_cable_powers() {
    // With independent Rd's the cable's Ra only loads CC2. CC1 still reads a
    // clean 0.408 V Rd; CC2 reads Ra. Rd/Ra => powered cable with sink => VBUS.
    let r = classify_attach(repaired(), Rp::Default, Cable::emarked());
    near(r.cc1_v, 0.408, 0.002, "repaired/e-marked CC1 (Rd)");
    // 80 µA into 5.1k || 1k = 836.07 Ohm => 0.06689 V.
    near(r.cc2_v, 0.06689, 0.001, "repaired/e-marked CC2 (Ra)");
    assert_eq!(r.cc1_state, PinState::Rd);
    assert_eq!(r.cc2_state, PinState::Ra);
    assert_eq!(r.attach, Attach::PoweredCableWithSink);
    assert!(r.powers(), "repaired board powers with BOTH cable types");
}

#[test]
fn the_repair_is_what_distinguishes_them() {
    // The single difference between fault and fix is shared_net, and it flips
    // exactly the e-marked-cable outcome. Same parts, same values, same Rp.
    let fault = classify_attach(as_designed(), Rp::Default, Cable::emarked());
    let fixed = classify_attach(repaired(), Rp::Default, Cable::emarked());
    assert!(!fault.powers());
    assert!(fixed.powers());
    // Both pass the passive cable.
    assert!(classify_attach(as_designed(), Rp::Default, Cable::Passive).powers());
    assert!(classify_attach(repaired(), Rp::Default, Cable::Passive).powers());
}

// ---------------------------------------------------------------------------
// Layer 2: end-to-end from the reconstructed .kicad_sch (skips if no corpus)
// ---------------------------------------------------------------------------

fn reconstruction_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../board-corpus/famous/rpi4_usbc_reconstruction")
}

fn load_board(file: &str) -> Option<ExtractedBoard> {
    let path = reconstruction_dir().join(file);
    let text = std::fs::read_to_string(&path).ok()?;
    Some(ExtractedBoard::from_kicad_schematic(&text).expect("schematic parses"))
}

#[test]
fn end_to_end_as_designed_schematic() {
    let Some(board) = load_board("rpi4_usbc_as_designed.kicad_sch") else {
        eprintln!("board-corpus not present; skipping end-to-end as-designed test");
        return;
    };

    // The defect is visible in the topology alone: CC1 and CC2 are one net.
    let term = extract_sink_termination(&board).expect("CC termination found");
    assert!(term.shared_net, "as-designed must show CC1==CC2 (shared R79)");
    near(term.cc1_rd_ohms.unwrap(), 5100.0, 1.0, "R79 ohms");

    // Passive cable: powers.
    let p = classify_board(&board, Rp::Default, Cable::Passive).unwrap();
    assert_eq!(p.attach, Attach::SinkAttached);
    assert!(p.powers());

    // E-marked cable: the famous failure, derived end-to-end from the schematic.
    let e = classify_board(&board, Rp::Default, Cable::emarked()).unwrap();
    near(e.cc1_v, 0.13377, 0.001, "e2e as-designed/e-marked CC1");
    near(e.cc2_v, 0.13377, 0.001, "e2e as-designed/e-marked CC2");
    assert_eq!(e.attach, Attach::AudioAccessory);
    assert!(!e.powers());
}

#[test]
fn end_to_end_repaired_schematic() {
    let Some(board) = load_board("rpi4_usbc_repaired.kicad_sch") else {
        eprintln!("board-corpus not present; skipping end-to-end repaired test");
        return;
    };

    let term = extract_sink_termination(&board).expect("CC termination found");
    assert!(!term.shared_net, "repaired must show independent CC1/CC2 nets");
    near(term.cc1_rd_ohms.unwrap(), 5100.0, 1.0, "R1 ohms");
    near(term.cc2_rd_ohms.unwrap(), 5100.0, 1.0, "R2 ohms");

    // Both cable types power.
    let p = classify_board(&board, Rp::Default, Cable::Passive).unwrap();
    assert_eq!(p.attach, Attach::SinkAttached);
    assert!(p.powers());

    let e = classify_board(&board, Rp::Default, Cable::emarked()).unwrap();
    near(e.cc1_v, 0.408, 0.002, "e2e repaired/e-marked CC1");
    assert_eq!(e.attach, Attach::PoweredCableWithSink);
    assert!(e.powers());
}
