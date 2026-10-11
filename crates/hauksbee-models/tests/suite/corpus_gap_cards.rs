//! Corpus-gap cards: MMDL914T1G, MMBT3904, PMEG60T10ELR, 1N4007FL, VBUS05L1.
//!
//! Each of these parts was OPEN (or on a generic fallback) on real boards. The
//! tests pin the value strings the corpus carries, the datasheet pin map, and
//! that the fitted forward curve still reproduces the datasheet points it was
//! fitted to.

use super::{assert_pins, expect_model as resolve, param, pins, resolve_value, vf_at};

/// Thermal voltage at 300 K, the value the fits were made with.
const VT: f64 = 0.025_852;

#[test]
fn each_corpus_value_string_binds_its_own_card() {
    for (value, id) in [
        ("MMDL914T1G", "mmdl914"),
        ("SMMDL914T1G", "mmdl914"),
        ("MMDL914T3G", "mmdl914"),
        ("MMBT3904", "mmbt3904"),
        ("MMBT3904LT1G", "mmbt3904"),
        ("SMMBT3904LT1G", "mmbt3904"),
        ("PMEG60T10ELR", "pmeg60t10elr"),
        ("PMEG60T10ELRX", "pmeg60t10elr"),
        ("1N4007FL", "1n4007fl"),
        ("VBUS05L1", "vbus05l1_dd1"),
        ("VBUS05L1-DD1", "vbus05l1_dd1"),
    ] {
        assert_eq!(resolve(value).id, id, "{value}");
    }
}

#[test]
fn neighbouring_parts_keep_their_own_cards() {
    // The TO-92 and DO-41 parts have different ratings and datasheets.
    assert_eq!(resolve("2N3904").id, "2n3904");
    assert_eq!(resolve("1N4007").id, "1n4007");
    // A different Nexperia part must not be caught by the PMEG60T10ELR rule.
    assert_ne!(
        resolve_value("PMEG6010CEH,115")
            .model
            .map(|m| m.id)
            .as_deref(),
        Some("pmeg60t10elr")
    );
}

#[test]
fn package_specific_ratings_are_the_package_datasheets_own() {
    let sot23 = resolve("MMBT3904");
    let to92 = resolve("2N3904");
    // onsemi MMBT3904LT1/D Rev. 14: PD 225 mW and RthJA 556 C/W on FR-5.
    assert_eq!(sot23.ratings.max_power_w, Some(0.225));
    assert_eq!(sot23.ratings.theta_ja_c_per_w, Some(556.0));
    assert_eq!(to92.ratings.max_power_w, Some(0.625));
    let smc = resolve("1N4007FL");
    let do41 = resolve("1N4007");
    assert_eq!(smc.ratings.theta_ja_c_per_w, Some(75.0));
    assert_eq!(do41.ratings.theta_ja_c_per_w, Some(50.0));
}

#[test]
fn pad_maps_follow_the_datasheet_pinouts() {
    assert_pins(&resolve("MMDL914T1G"), &pins("1=cathode 2=anode"));
    assert_pins(&resolve("PMEG60T10ELR"), &pins("1=cathode 2=anode"));
    assert_pins(&resolve("MMBT3904"), &pins("1=base 2=emitter 3=collector"));
    assert_pins(&resolve("VBUS05L1"), &pins("1=p1 2=p2"));
}

#[test]
fn forward_fits_reproduce_the_datasheet_points() {
    // onsemi MMDL914T1/D Rev. 11, Fig. 2 at 25 C, read at three currents.
    let m = resolve("MMDL914T1G");
    for (i, want) in [(1e-3, 0.601), (10e-3, 0.726), (100e-3, 0.928)] {
        let got = vf_at(&m, i, VT);
        assert!(
            (got - want).abs() < 6e-3,
            "MMDL914 at {i} A: {got:.4} vs {want}"
        );
    }
    // Nexperia PMEG60T10ELR v.3, Table 7 typical VF at 25 C.
    let m = resolve("PMEG60T10ELR");
    for (i, want) in [(0.1, 0.410), (0.5, 0.480), (1.0, 0.525)] {
        let got = vf_at(&m, i, VT);
        assert!(
            (got - want).abs() < 2e-3,
            "PMEG60T10ELR at {i} A: {got:.4} vs {want}"
        );
    }
    // SMC N1646 Rev. A, Fig. 3 at 1 A stays under the printed VF max, 1.1 V.
    let m = resolve("1N4007FL");
    assert!(vf_at(&m, 1.0, VT) < 1.1);
}

#[test]
fn mmbt3904_vbe_sat_sits_inside_the_printed_interval() {
    // onsemi MMBT3904LT1/D: VBE(sat) 0.65 to 0.85 V at IC = 10 mA, IB = 1 mA.
    let m = resolve("MMBT3904");
    let vbe = VT * (10e-3 / param(&m, "is")).ln();
    assert!((0.65..=0.85).contains(&vbe), "VBE {vbe}");
    assert!((100.0..=300.0).contains(&param(&m, "bf")));
}

#[test]
fn vbus05l1_clamps_symmetrically_from_the_printed_clamp_points() {
    let m = resolve("VBUS05L1");
    let vbr = param(&m, "vbreak_v");
    let rd = param(&m, "rdyn_ohms");
    // Vishay doc. 81188 Rev. 1.8: VBR typ 8.4 V at 1 mA; VC 14 V typ at 2 A.
    assert!((vbr - 8.4).abs() < 1e-9, "vbreak_v {vbr}");
    let vc_2a = 2.0f64.mul_add(rd, vbr);
    assert!((vc_2a - 14.0).abs() < 0.1, "VC at 2 A {vc_2a}");
    assert!(vbr > 5.5, "must not conduct inside the 5.5 V working range");
    assert_eq!(m.behavioral.laws.len(), 1);
}
