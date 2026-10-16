//! Schottky rectifiers a board names by a bare part number: SS12-SS16, SB120-SB160,
//! 1N5820/1N5821 and MBR0520.
//!
//! Before these cards each resolved to the generic diode fallback, which has no
//! blocking rating, so an SS12 (VRRM 20 V) on a 24 V rail passed. The faulty
//! case is that fallback; the legal control is each part holding its own
//! datasheet VRRM, taken from the sheet's table (Vishay 88746, Vishay 88526,
//! Diodes DS23022, semiware MBR0520L Rev. A2.0).

use super::{expect_model as resolve, resolve_part, vf_at};

#[test]
fn each_rectifier_holds_its_own_vrrm_not_a_generic_one() {
    for (part, vrrm) in [
        ("SS12", 20.0),
        ("SS13", 30.0),
        ("SS14", 40.0),
        ("SS15", 50.0),
        ("SS16", 60.0),
        ("SB120", 20.0),
        ("SB130", 30.0),
        ("SB140", 40.0),
        ("SB150", 50.0),
        ("SB160", 60.0),
        ("1N5820", 20.0),
        ("1N5821", 30.0),
        ("MBR0520", 20.0),
        ("MBR0520L", 20.0),
    ] {
        let m = resolve(part);
        assert_eq!(m.id, part.trim_end_matches('L').to_lowercase(), "{part}");
        assert_eq!(m.ratings.max_voltage_v, Some(vrrm), "{part} rating");
        assert_eq!(m.params.get_f64("bv"), Some(vrrm), "{part} breakdown floor");
    }
}

#[test]
fn ordering_code_suffixes_resolve_to_the_same_card() {
    // Vishay SS1x suffixes: -E3/61T, HE3_A/H; Diodes SB1x0 -T; onsemi MBR0520LT1G.
    for (code, id) in [
        ("SS14-E3/61T", "ss14"),
        ("SS16HE3_A/H", "ss16"),
        ("SB140-T", "sb140"),
        ("1N5821-E3/54", "1n5821"),
        ("MBR0520LT1G", "mbr0520"),
    ] {
        assert_eq!(resolve(code).id, id, "{code}");
    }
}

#[test]
fn neighbouring_numbers_do_not_borrow_a_card() {
    // A one-digit slip must not inherit the wrong VRRM: SS17, SS110, SB170 and
    // 1N5823 have no card here and must not resolve to a lower-rated sibling.
    for part in ["SS17", "SS110", "SB170", "1N5823"] {
        if let Some(m) = resolve_part(part) {
            assert_ne!(m.id, "ss16", "{part}");
            assert_ne!(m.id, "sb160", "{part}");
            assert_ne!(m.id, "1n5821", "{part}");
        }
    }
}

#[test]
fn the_forward_fit_sits_on_the_printed_points() {
    // Vishay 88746: VF max 0.50 V (SS12-SS14) / 0.75 V (SS15, SS16) at 1.0 A.
    for (part, vf) in [("SS12", 0.50), ("SS14", 0.50), ("SS16", 0.75)] {
        let got = vf_at(&resolve(part), 1.0, 0.02585);
        assert!((got - vf).abs() < 2e-3, "{part}: {got:.4} V vs {vf} V");
    }
    // Vishay 88526: 1N5820 VF 0.475 V at 3.0 A and 0.850 V at 9.4 A.
    let m = resolve("1N5820");
    for (i, vf) in [(3.0, 0.475), (9.4, 0.850)] {
        let got = vf_at(&m, i, 0.02585);
        assert!((got - vf).abs() < 2e-3, "1N5820 at {i} A: {got:.4} V");
    }
    // semiware A2.0: MBR0520L VF 0.30 V at 0.1 A and 0.39 V at 0.5 A.
    let m = resolve("MBR0520");
    for (i, vf) in [(0.1, 0.30), (0.5, 0.39)] {
        let got = vf_at(&m, i, 0.02585);
        assert!((got - vf).abs() < 2e-3, "MBR0520 at {i} A: {got:.4} V");
    }
}
