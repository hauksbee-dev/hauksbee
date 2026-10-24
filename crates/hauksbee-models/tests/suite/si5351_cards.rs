//! The Si5351 clock-generator cards: one per package and variant, each bound
//! only where its footprint and ordering code agree, each carrying the crystal
//! oscillator's on-chip load capacitance.
//!
//! The pad maps are the data sheet's (Si5351A/B/C-B Rev. 1.3): 20-QFN Tables
//! 14 to 16 (pp.27-29), 16-QFN Tables 17 to 19 (pp.30-32), 10-MSOP Table 20
//! (p.33). A map applied to the wrong package is worse than no card: the 10-MSOP
//! puts VDD on pad 1, where both QFNs have the crystal's XA.

use super::{expect_query, query_id};

const QFN20: &str = "Package_DFN_QFN:QFN-20-1EP_4x4mm_P0.5mm_EP2.6x2.6mm";
const QFN16: &str = "Package_DFN_QFN:QFN-16-1EP_3x3mm_P0.5mm_EP1.7x1.7mm";
const MSOP10: &str = "Package_SO:MSOP-10_3x3mm_P0.5mm";

/// The id the value (as value and MPN, as the engine's query does) binds in `footprint`.
fn id(value: &str, footprint: &str) -> Option<String> {
    query_id(Some(value), Some(value), Some(footprint))
}

#[test]
fn each_qfn20_variant_has_its_own_pad_map() {
    let common = [
        ("1", "osci"),
        ("2", "osco"),
        ("4", "scl"),
        ("5", "sda"),
        ("8", "clk3"),
        ("9", "clk2"),
        ("10", "vddob"),
        ("11", "vddoa"),
        ("12", "clk1"),
        ("13", "clk0"),
        ("14", "vddod"),
        ("15", "clk7"),
        ("16", "clk6"),
        ("17", "clk5"),
        ("18", "vddoc"),
        ("19", "clk4"),
        ("20", "vdd"),
    ];
    // Pads 3 and 6 are what differ: A0 / VC / INTR, then SSEN / SSEN / CLKIN.
    for (value, card, p3, p6) in [
        ("Si5351A-B-GM", "si5351a_qfn20", "a0", "ssen"),
        ("Si5351B-B-GM", "si5351b_qfn20", "vc", "ssen"),
        ("Si5351C-B-GM", "si5351c_qfn20", "intr", "clkin"),
    ] {
        let mut expected = common.to_vec();
        expected.extend([("3", p3), ("6", p6), ("7", "oeb")]);
        let m = expect_query(Some(value), Some(value), Some(QFN20));
        super::assert_pin_map(&m, card, &expected);
    }
}

#[test]
fn each_qfn16_variant_has_its_own_pad_map() {
    let common = [
        ("1", "osci"),
        ("2", "osco"),
        ("4", "scl"),
        ("5", "sda"),
        ("7", "clk1"),
        ("8", "vddob"),
        ("9", "vddoa"),
        ("10", "clk0"),
        ("11", "vddod"),
        ("12", "clk3"),
        ("13", "clk2"),
        ("14", "vddoc"),
        ("15", "gnd"),
        ("16", "vdd"),
    ];
    // Pad 3 is A0 / VC / OEB; OEB is pad 6 on A and B but CLKIN is pad 6 on C.
    for (value, card, p3, p6) in [
        ("Si5351A-B-GM1", "si5351a_qfn16", "a0", "oeb"),
        ("Si5351B-B-GM1", "si5351b_qfn16", "vc", "oeb"),
        ("Si5351C-B-GM1", "si5351c_qfn16", "oeb", "clkin"),
    ] {
        let mut expected = common.to_vec();
        expected.extend([("3", p3), ("6", p6)]);
        let m = expect_query(Some(value), Some(value), Some(QFN16));
        super::assert_pin_map(&m, card, &expected);
    }
}

#[test]
fn the_msop10_has_vdd_on_pad_one_and_the_crystal_on_two_and_three() {
    let m = expect_query(Some("Si5351A-B-GT"), Some("Si5351A-B-GT"), Some(MSOP10));
    super::assert_pin_map(
        &m,
        "si5351a_msop10",
        &[
            ("1", "vdd"),
            ("2", "osci"),
            ("3", "osco"),
            ("4", "scl"),
            ("5", "sda"),
            ("6", "clk2"),
            ("7", "vddo"),
            ("8", "gnd"),
            ("9", "clk1"),
            ("10", "clk0"),
        ],
    );
    let caps = m.xtal_int_caps_on("2").expect("XA carries the oscillator");
    assert_eq!(caps.pads, ["2", "3"]);
    assert!(m.xtal_int_caps_on("1").is_none(), "pad 1 is VDD");
}

#[test]
fn a_bare_value_binds_by_its_footprint_and_never_by_a_guess() {
    for (value, footprint, want) in [
        ("SI5351A", QFN20, "si5351a_qfn20"),
        ("SI5351B", QFN20, "si5351b_qfn20"),
        ("SI5351C", "hackrf:GSG-QFN20-4", "si5351c_qfn20"),
        ("SI5351A", QFN16, "si5351a_qfn16"),
        ("SI5351C", QFN16, "si5351c_qfn16"),
        ("SI5351A", MSOP10, "si5351a_msop10"),
    ] {
        assert_eq!(
            id(value, footprint).as_deref(),
            Some(want),
            "{value} {footprint}"
        );
    }
    // A footprint that does not say the package, or none at all: no card, so no
    // pad map is applied that might be the wrong package's.
    for footprint in ["", "Package_SO:Generic", "Module:Adafruit_Si5351_Breakout"] {
        let bound = id("SI5351A", footprint);
        assert!(
            bound.as_deref().is_none_or(|b| !b.starts_with("si5351")),
            "{footprint:?} bound {bound:?}"
        );
    }
}

#[test]
fn an_ordering_code_that_names_another_package_binds_nothing() {
    for (value, footprint) in [
        // -GM is the 20-QFN, -GM1 the 16-QFN, -GT the 10-MSOP (Figure 24, p.34).
        ("Si5351A-B-GM", QFN16),
        ("Si5351A-B-GM", MSOP10),
        ("Si5351A-B-GM1", QFN20),
        ("Si5351A-B-GM1", MSOP10),
        ("Si5351A-B-GT", QFN20),
        ("Si5351A-B-GT", QFN16),
        // The MSOP is the A variant only (Figure 24 note).
        ("Si5351B", MSOP10),
        ("Si5351C-B-GT", MSOP10),
    ] {
        let bound = id(value, footprint);
        assert!(
            bound.as_deref().is_none_or(|b| !b.starts_with("si5351")),
            "{value} on {footprint} bound {bound:?}"
        );
    }
}

#[test]
fn custom_codes_and_tape_and_reel_keep_their_package() {
    // Bxxxxx is a five-character custom code (Figure 24), R means tape and reel.
    for (value, footprint, want) in [
        ("Si5351A-B04486-GM", QFN20, "si5351a_qfn20"),
        ("Si5351C-B-GMR", QFN20, "si5351c_qfn20"),
        ("Si5351B-B01234-GM1R", QFN16, "si5351b_qfn16"),
        ("Si5351A-B-GTR", MSOP10, "si5351a_msop10"),
    ] {
        assert_eq!(id(value, footprint).as_deref(), Some(want), "{value}");
    }
}

#[test]
fn every_package_states_the_same_programmable_crystal_load() {
    for (value, footprint, xa) in [
        ("Si5351A-B-GM", QFN20, "1"),
        ("Si5351B-B-GM", QFN20, "1"),
        ("Si5351C-B-GM", QFN20, "1"),
        ("Si5351A-B-GM1", QFN16, "1"),
        ("Si5351B-B-GM1", QFN16, "1"),
        ("Si5351C-B-GM1", QFN16, "1"),
        ("Si5351A-B-GT", MSOP10, "2"),
    ] {
        let m = expect_query(Some(value), Some(value), Some(footprint));
        let cap = m
            .xtal_int_caps_on(xa)
            .unwrap_or_else(|| panic!("{value}: no oscillator"));
        // Register 183 (AN619 p.60): 6, 8 or 10 pF across the pair, so 12 to 20 pF
        // on each pad; 00 (the 0 pF the data sheet's section 4.1.1 also lists) is reserved.
        assert_eq!(
            (cap.min_f, cap.max_f),
            (Some(12.0e-12), Some(20.0e-12)),
            "{value}"
        );
        assert!(cap.includes_pin_capacitance, "{value}");
        // Table 8 (p.11): crystal CL 6 to 12 pF; 4.1.1 (p.15) with 7.4 (p.25): 2 pF
        // of external capacitance across the pair (4 pF on each of XA and XB).
        assert_eq!(cap.max_crystal_cl_f, Some(12.0e-12), "{value}");
        assert_eq!(cap.max_external_load_f, Some(2.0e-12), "{value}");
        // The pin capacitance being inside CL is an inference from 4.1.1, said as one.
        assert!(
            cap.basis
                .contains("which implies the pin capacitance is inside CL"),
            "{value}: {}",
            cap.basis
        );
        for cited in ["4.1.1", "p.15", "AN619", "Register 183", "p.60", "Reserved"] {
            assert!(
                cap.basis.contains(cited),
                "{value}: {cited} missing from {}",
                cap.basis
            );
        }
    }
}
