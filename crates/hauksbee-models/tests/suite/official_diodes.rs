//! Official-parts batch diodes: resolution and behaviour tests for the cards added in that batch.
//!
//! The cards live in `db/discrete_diodes.toml`: BZT52C and BZX84-C Zeners, MBR0530/MBR0540/
//! `MBR120xSF` and B5817W-B5819W Schottkys, SMAJ/SMBJ/SMCJ TVS diodes, ESD5Z and PESD5V0S1BL ESD
//! diodes. A resolved card drops its cited rows, so the row-level checks read the file as written.

use super::{expect_model, param, resolve_part};
use hauksbee_models::schema::{Characteristic, DbFile, ModelEntry};
use hauksbee_models::ComponentKind;

const VT: f64 = 8.617_333_262e-5 * 300.15;

fn written() -> Vec<ModelEntry> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("db/discrete_diodes.toml");
    let text = std::fs::read_to_string(path).expect("discrete_diodes.toml readable");
    toml::from_str::<DbFile>(&text)
        .expect("discrete_diodes.toml parses")
        .models
}

fn rows(m: &ModelEntry) -> &[Characteristic] {
    &m.source.as_ref().expect("source block").characteristic
}

fn row<'a>(m: &'a ModelEntry, parameter: &str, conditions: &str) -> &'a Characteristic {
    rows(m)
        .iter()
        .find(|c| c.parameter.starts_with(parameter) && c.conditions.starts_with(conditions))
        .unwrap_or_else(|| panic!("{}: no row {parameter} / {conditions}", m.id))
}

/// The part number a card is ordered as, from its id.
fn ordering_name(id: &str) -> String {
    // TVS and ESD ids spell a decimal point as an underscore (`smaj5_0a` is SMAJ5.0A).
    id.strip_prefix("bzx84_c").map_or_else(
        || id.replace('_', ".").to_uppercase(),
        |code| format!("BZX84-C{}", code.to_uppercase()),
    )
}

#[test]
fn every_card_resolves_from_its_own_ordering_name_and_is_cited() {
    let cards = written();
    assert!(
        cards.len() > 300,
        "expected the whole batch, got {}",
        cards.len()
    );
    let mut ids: Vec<&str> = cards.iter().map(|c| c.id.as_str()).collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), cards.len(), "duplicate card ids");
    for c in &cards {
        let name = ordering_name(&c.id);
        let got = resolve_part(&name).unwrap_or_else(|| panic!("{name} did not resolve"));
        assert_eq!(got.id, c.id, "{name} resolved to {}", got.id);
        let src = c.source.as_ref().expect("source block");
        assert!(!src.references.is_empty(), "{}: no reference", c.id);
        for r in &src.references {
            assert_eq!(
                r.sha256.as_deref().map(str::len),
                Some(64),
                "{}: sha256 of the sheet",
                c.id
            );
        }
        assert!(!rows(c).is_empty(), "{}: no cited rows", c.id);
    }
}

#[test]
fn bzt52c_codes_clamp_at_their_own_voltage_and_stay_apart() {
    for (code, vz, izt) in [
        ("BZT52C2V0", 2.0, 5.0),
        ("BZT52C3V3", 3.3, 5.0),
        ("BZT52C5V1", 5.1, 5.0),
        ("BZT52C6V8", 6.8, 5.0),
        ("BZT52C12", 12.0, 5.0),
        ("BZT52C51", 51.0, 5.0),
    ] {
        let m = expect_model(code);
        assert_eq!(m.id, code.to_lowercase());
        assert!((param(&m, "bv") - vz).abs() < 1e-9, "{code} bv");
        assert!((param(&m, "ibv") - izt / 1000.0).abs() < 1e-9, "{code} ibv");
    }
    // Packaging suffixes are the same silicon.
    assert_eq!(expect_model("BZT52C5V1-7-F").id, "bzt52c5v1");
    // The family stops at 51 V: a neighbour that the sheet does not print gets no card.
    assert!(resolve_part("BZT52C56").is_none_or(|m| !m.id.starts_with("bzt52c")));
    // A longer code is not swallowed by a shorter one.
    assert_ne!(expect_model("BZT52C20").id, expect_model("BZT52C2V0").id);
}

#[test]
fn zener_voltage_lies_inside_the_printed_window() {
    for c in written()
        .iter()
        .filter(|c| c.id.starts_with("bzt52c") || c.id.starts_with("bzx84_c"))
    {
        let r = rows(c)
            .iter()
            .find(|r| r.parameter.starts_with("VZ"))
            .unwrap_or_else(|| panic!("{}: no VZ row", c.id));
        let bv = c.params.get_f64("bv").expect("bv");
        let (lo, hi) = (r.min.expect("VZ min"), r.max.expect("VZ max"));
        assert!(lo <= bv && bv <= hi, "{}: bv {bv} outside {lo}..{hi}", c.id);
        // The knee current is the one the voltage was printed at.
        let izt: f64 = r
            .conditions
            .split(|ch: char| !(ch.is_ascii_digit() || ch == '.'))
            .find_map(|t| t.parse().ok())
            .expect("test current in conditions");
        let ibv = c.params.get_f64("ibv").expect("ibv");
        assert!(
            (ibv - izt / 1000.0).abs() < 1e-12,
            "{}: ibv {ibv} vs {izt} mA",
            c.id
        );
    }
}

#[test]
fn bzx84_c_types_resolve_with_and_without_the_dash() {
    for (v, id, bv) in [
        ("BZX84-C5V1", "bzx84_c5v1", 5.1),
        ("BZX84C5V1", "bzx84_c5v1", 5.1),
        ("BZX84-C12", "bzx84_c12", 12.05),
        ("BZX84-C75", "bzx84_c75", 74.5),
    ] {
        let m = expect_model(v);
        assert_eq!(m.id, id, "{v}");
        assert!(
            (param(&m, "bv") - bv).abs() < 0.2,
            "{v} bv {}",
            param(&m, "bv")
        );
    }
    // Tolerance classes A and B are different silicon windows and have no card here.
    assert!(resolve_part("BZX84-B5V1").is_none_or(|m| !m.id.starts_with("bzx84_c")));
}

#[test]
fn schottky_forward_laws_stay_within_ten_percent_of_every_25c_row() {
    let mut checked = 0;
    for c in written().iter().filter(|c| {
        c.id.starts_with("mbr")
            || (c.id.starts_with('b') && c.id.ends_with('w') && c.id.starts_with("b58"))
    }) {
        let (n, is, rs) = (
            c.params.get_f64("n").unwrap(),
            c.params.get_f64("is").unwrap(),
            c.params.get_f64("rs").unwrap(),
        );
        for r in rows(c)
            .iter()
            .filter(|r| r.parameter.starts_with("VF") && r.conditions.contains("25 "))
        {
            let i: f64 = r
                .conditions
                .trim_start_matches("IF = ")
                .split(' ')
                .next()
                .unwrap()
                .parse()
                .unwrap();
            let vf = (n * VT).mul_add((i / is).ln_1p(), i * rs);
            let printed = r.max.expect("VF max");
            assert!(
                ((vf - printed) / printed).abs() < 0.10,
                "{}: VF({i} A) = {vf:.3} V against the printed {printed} V",
                c.id
            );
            // Guaranteed maxima: the law must not sit far below them either.
            checked += 1;
        }
    }
    assert!(checked >= 19, "only {checked} VF rows checked");
}

#[test]
fn schottky_ratings_and_breakdown_follow_the_sheets() {
    for (v, id, vrrm) in [
        ("MBR0530", "mbr0530", 30.0),
        ("MBR0530T1G", "mbr0530", 30.0),
        ("MBR0540", "mbr0540", 40.0),
        ("NRVB0540T1G", "mbr0540", 40.0),
        ("MBR120LSFT1G", "mbr120lsf", 20.0),
        ("MBR120VLSFT1G", "mbr120vlsf", 20.0),
        ("MBR120ESFT1G", "mbr120esf", 20.0),
        ("B5817W", "b5817w", 20.0),
        ("B5818W", "b5818w", 30.0),
        ("B5819W", "b5819w", 40.0),
    ] {
        let m = expect_model(v);
        assert_eq!(m.id, id, "{v}");
        assert!((param(&m, "bv") - vrrm).abs() < 1e-9, "{v} bv");
        let max_v = m.ratings.max_voltage_v.expect("max_voltage_v");
        assert!((max_v - vrrm).abs() < 1e-9, "{v} rating");
        assert!(
            m.params.get_f64("cjo").is_some() || !v.starts_with('B'),
            "{v}"
        );
    }
    // A bare MBR120 has no standalone sheet, so it does not borrow a sibling's card.
    assert!(resolve_part("MBR120").is_none_or(|m| !m.id.starts_with("mbr120")));
    // Neighbouring Schottky numbers are not swallowed by the MBR05xx cards.
    assert!(resolve_part("MBR0520").is_none_or(|m| m.id != "mbr0530" && m.id != "mbr0540"));
}

#[test]
fn tvs_clamp_lands_on_the_printed_clamping_voltage() {
    let mut seen = 0;
    for c in written().iter().filter(|c| c.id.starts_with("sm")) {
        let vbreak = c.params.get_f64("vbreak_v").expect("vbreak_v");
        let bidir = c.id.ends_with("ca");
        let rdyn = c.params.get_f64("rdyn_rev_ohms").expect("rdyn_rev_ohms");
        let vbr = row(c, "VBR", "IT");
        let (lo, hi) = (vbr.min.unwrap(), vbr.max.unwrap());
        assert!(
            (vbreak - f64::midpoint(lo, hi)).abs() < 1e-5,
            "{}: vbreak_v",
            c.id
        );
        let vc = row(c, "VC", "IPP").max.unwrap();
        let ipp = row(c, "IPP", "10/1000").max.unwrap();
        assert!(
            (rdyn.mul_add(ipp, vbreak) - vc).abs() < 1e-3 * vc,
            "{}: clamp misses VC",
            c.id
        );
        let vr = c.ratings.max_voltage_v.unwrap();
        assert!(
            vr < lo,
            "{}: stand-off {vr} V must be below VBR min {lo} V",
            c.id
        );
        assert_eq!(
            c.params.get_f64("vf_v").is_none(),
            bidir,
            "{}: forward knee only on unidirectional",
            c.id
        );
        assert_eq!(c.kind, ComponentKind::Digital);
        seen += 1;
    }
    assert!(seen > 250, "only {seen} TVS cards");
}

#[test]
fn tvs_orderings_resolve_to_the_exact_voltage_and_polarity() {
    for (v, id) in [
        ("SMAJ5.0A", "smaj5_0a"),
        ("SMAJ5.0CA", "smaj5_0ca"),
        ("SMAJ12A", "smaj12a"),
        ("SMAJ12CA", "smaj12ca"),
        ("SMBJ5.0A", "smbj5_0a"),
        ("SMBJ12CA", "smbj12ca"),
        ("SMBJ33A", "smbj33a"),
        ("SMCJ5.0A", "smcj5_0a"),
        ("SMCJ15CA", "smcj15ca"),
        ("SMCJ12A", "smcj12a"),
    ] {
        let m = expect_model(v);
        assert_eq!(m.id, id, "{v}");
    }
    // Voltages never collapse: 12 is not 120 or 1.2, and A is not CA.
    let a12 = expect_model("SMAJ12A");
    let a120 = expect_model("SMAJ120A");
    assert_ne!(a12.id, a120.id);
    assert!(param(&a12, "vbreak_v") < 20.0 && param(&a120, "vbreak_v") > 100.0);
    let (uni, bi) = (expect_model("SMBJ12A"), expect_model("SMBJ12CA"));
    assert!(uni.params.get_f64("vf_v").is_some() && bi.params.get_f64("vf_v").is_none());
    // The 5% (non-A) 10% parts are different silicon; the sheet's non-A rows are not carded here.
    assert!(resolve_part("SMAJ12").is_none_or(|m| m.id != "smaj12a"));
}

#[test]
fn tvs_values_match_spot_checked_sheet_rows() {
    // SMAJ12A: VBR 13.3-14.7 V at 1 mA, VC 19.9 V at 20.1 A (Littelfuse SMAJ table).
    let m = expect_model("SMAJ12A");
    assert!((param(&m, "vbreak_v") - 14.0).abs() < 1e-9);
    let cards = written();
    let c = cards.iter().find(|c| c.id == "smaj12a").unwrap();
    assert_eq!(row(c, "VC", "IPP").max, Some(19.9));
    assert_eq!(row(c, "IPP", "10/1000").max, Some(20.1));
    assert_eq!(row(c, "PPPM", "10/1000").max, Some(400.0));
}

#[test]
fn esd5z_diodes_clamp_on_vc_and_keep_their_sheet_capacitance() {
    for (v, id, vrwm, vbr, vc, ipp, cd) in [
        ("ESD5Z3.3T1G", "esd5z3_3", 3.3, 5.0, 14.1, 11.2, 105.0),
        ("ESD5Z5.0T1G", "esd5z5_0", 5.0, 6.2, 18.6, 9.4, 80.0),
        ("ESD5Z12T1G", "esd5z12", 12.0, 14.1, 25.0, 9.6, 55.0),
    ] {
        let m = expect_model(v);
        assert_eq!(m.id, id, "{v}");
        assert!((param(&m, "bv") - vbr).abs() < 1e-9);
        assert!(f64::mul_add(cd, -1e-12, param(&m, "cjo")).abs() < 1e-18);
        assert_eq!(m.ratings.max_voltage_v, Some(vrwm));
        // Reverse current at IPP: VBR + rs * IPP + n Vt ln(IPP / IBV) is the printed VC max.
        let clamp = VT.mul_add(
            (ipp / param(&m, "ibv")).ln(),
            param(&m, "rs").mul_add(ipp, vbr),
        );
        assert!(
            (clamp - vc).abs() < 0.02,
            "{v}: clamp {clamp:.2} V against VC {vc} V"
        );
    }
    // ESD5Z3.3 is not ESD5Z3: the other voltages in the series have no card here.
    assert!(resolve_part("ESD5Z2.5T1G").is_none_or(|m| !m.id.starts_with("esd5z")));
}

#[test]
fn pesd5v0s1bl_is_a_symmetric_clamp() {
    let m = expect_model("PESD5V0S1BL");
    assert_eq!(m.id, "pesd5v0s1bl");
    assert_eq!(m.kind, ComponentKind::Digital);
    let vb = param(&m, "vbreak_v");
    let rd = param(&m, "rdyn_ohms");
    assert!((5.5..=9.5).contains(&vb));
    // 12 A clamps on the printed 14 V maximum, and 1 A sits under the 10 V maximum.
    assert!((rd.mul_add(12.0, vb) - 14.0).abs() < 1e-4);
    assert!(vb + rd < 10.0);
}
