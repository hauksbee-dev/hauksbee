//! Official discrete-active cards (db/discrete_active.toml): the 2N2222A / PN2222A,
//! BC337 and BC327 grades, MMBT3906, the TIP120-series Darlingtons, the S8550 in
//! TO-92 and SOT-23, and the IRLZ44N / IRFZ44N / IRF3205 power MOSFETs. Each card
//! must resolve by its ordering codes, carry the datasheet pin map and ratings, and
//! leave its sibling part numbers alone.

use hauksbee_models::{ComponentKind, ModelEntry};

const TO92: &str = "Package_TO_SOT_THT:TO-92_Inline";
const SOT23: &str = "Package_TO_SOT_SMD:SOT-23";

fn id_of(value: &str) -> Option<String> {
    super::query_id(Some(value), Some(value), None)
}

fn id_on(value: &str, footprint: &str) -> Option<String> {
    super::query_id(Some(value), Some(value), Some(footprint))
}

fn card(value: &str) -> ModelEntry {
    super::expect_query(Some(value), Some(value), None)
}

fn card_on(value: &str, footprint: &str) -> ModelEntry {
    super::expect_query(Some(value), Some(value), Some(footprint))
}

/// Every spelling resolves to `id`.
fn assert_resolves(id: &str, values: &[&str]) {
    for v in values {
        assert_eq!(id_of(v).as_deref(), Some(id), "{v} must resolve to {id}");
    }
}

/// None of the values resolves to `id`.
fn assert_not(id: &str, values: &[&str]) {
    for v in values {
        assert_ne!(
            id_of(v).as_deref(),
            Some(id),
            "{v} must not resolve to {id}"
        );
    }
}

fn bjt_checks(m: &ModelEntry, kind: ComponentKind, bf: f64, ic: f64, vce: f64) {
    assert_eq!(m.kind, kind, "{}: polarity", m.id);
    assert!((super::param(m, "bf") - bf).abs() < 1e-9, "{}: bf", m.id);
    assert_eq!(m.ratings.max_current_a, Some(ic), "{}: IC rating", m.id);
    assert_eq!(m.ratings.max_voltage_v, Some(vce), "{}: VCEO rating", m.id);
}

#[test]
fn n2222a_family_resolves_with_the_datasheet_pins() {
    assert_resolves(
        "pn2222a",
        &[
            "PN2222A",
            "PN2222ABU",
            "PN2222ATA",
            "PN2222ATF",
            "PN2222ATFR",
        ],
    );
    assert_resolves("2n2222a", &["2N2222A"]);
    bjt_checks(&card("PN2222A"), ComponentKind::BjtNpn, 200.0, 1.0, 40.0);
    bjt_checks(&card("2N2222A"), ComponentKind::BjtNpn, 200.0, 0.8, 40.0);
    // TO-92 E-B-C and TO-18 lead code E-B-C: pin 1 is the emitter, pin 3 the collector.
    super::assert_pin_map(
        &card("PN2222A"),
        "pn2222a",
        &super::pins("1=emitter 2=base 3=collector"),
    );
    super::assert_pin_map(
        &card("2N2222A"),
        "2n2222a",
        &super::pins("1=emitter 2=base 3=collector"),
    );
    assert_eq!(card("PN2222A").ratings.max_power_w, Some(0.625));
    assert_eq!(card("2N2222A").ratings.max_junction_temp_c, Some(200.0));
}

#[test]
fn n2222a_siblings_do_not_resolve_to_the_a_cards() {
    // The 30 V non-A parts, the other packages and the 2N2221A are other datasheets.
    assert_not(
        "pn2222a",
        &[
            "PN2222",
            "P2N2222A",
            "PN2222AX",
            "2N2222A",
            "MMBT2222A",
            "PN2907A",
        ],
    );
    assert_not(
        "2n2222a",
        &[
            "2N2222",
            "2N2221A",
            "2N2222AUB",
            "PN2222A",
            "2N22222A",
            "2N2907A",
        ],
    );
}

#[test]
fn bc337_grades_resolve_to_their_own_card() {
    assert_resolves("bc337", &["BC337", "BC337G", "BC337RL1G"]);
    assert_resolves(
        "bc337_25",
        &["BC337-25", "BC337-025G", "BC337-25RL1G", "BC337-25ZL1G"],
    );
    assert_resolves(
        "bc337_40",
        &["BC337-40", "BC337-040G", "BC337-40RL1G", "BC337-40ZL1G"],
    );
    // hFE midpoint of the printed band for each grade.
    bjt_checks(&card("BC337"), ComponentKind::BjtNpn, 365.0, 0.8, 45.0);
    bjt_checks(&card("BC337-25"), ComponentKind::BjtNpn, 280.0, 0.8, 45.0);
    bjt_checks(&card("BC337-40"), ComponentKind::BjtNpn, 440.0, 0.8, 45.0);
    // TO-92 CASE 29 Style 17 is C-B-E, the reverse of the 2N2222A.
    for v in ["BC337", "BC337-25", "BC337-40"] {
        let m = card(v);
        super::assert_pins(&m, &super::pins("1=collector 2=base 3=emitter"));
    }
}

#[test]
fn bc337_siblings_do_not_resolve_to_the_bc337_cards() {
    // The 30 V BC338, the PNP BC327, the -16 grade (not on the Rev. 8 sheet) and the NPN BC547.
    for id in ["bc337", "bc337_25", "bc337_40"] {
        assert_not(
            id,
            &[
                "BC338", "BC338-25", "BC327", "BC327-25", "BC547", "BC337-16", "BC33",
            ],
        );
    }
    assert_not("bc337", &["BC337-25", "BC337-40"]);
    assert_not("bc337_25", &["BC337", "BC337-40"]);
    assert_not("bc337_40", &["BC337", "BC337-25"]);
}

#[test]
fn bc327_grades_resolve_to_their_own_card_and_bare_bc327_keeps_the_generic() {
    assert_resolves("bc327", &["BC327BU"]);
    assert_resolves("bc327_16", &["BC327-16", "BC327-16BU", "BC327-16TA"]);
    assert_resolves("bc327_25", &["BC327-25", "BC327-25BU", "BC327-25TA"]);
    assert_resolves("bc327_40", &["BC327-40", "BC327-40BU", "BC327-40TA"]);
    // tests/suite/bjt_regex_polarity.rs pins a bare BC327 to the generic PNP card.
    assert_eq!(id_of("BC327").as_deref(), Some("bc307"));
    bjt_checks(&card("BC327-16"), ComponentKind::BjtPnp, 175.0, 0.8, 45.0);
    bjt_checks(&card("BC327-25"), ComponentKind::BjtPnp, 280.0, 0.8, 45.0);
    bjt_checks(&card("BC327-40"), ComponentKind::BjtPnp, 440.0, 0.8, 45.0);
    bjt_checks(&card("BC327BU"), ComponentKind::BjtPnp, 365.0, 0.8, 45.0);
    for v in ["BC327BU", "BC327-16", "BC327-25", "BC327-40"] {
        super::assert_pins(&card(v), &super::pins("1=collector 2=base 3=emitter"));
    }
}

#[test]
fn bc327_siblings_do_not_resolve_to_the_bc327_cards() {
    for id in ["bc327", "bc327_16", "bc327_25", "bc327_40"] {
        assert_not(
            id,
            &[
                "BC328", "BC328-25", "BC337", "BC337-25", "BC557", "BC32", "BC327-60",
            ],
        );
    }
    assert_not("bc327_16", &["BC327-25", "BC327-40", "BC327BU"]);
    assert_not("bc327_25", &["BC327-16", "BC327-40", "BC327BU"]);
    assert_not("bc327_40", &["BC327-16", "BC327-25", "BC327BU"]);
}

#[test]
fn mmbt3906_resolves_on_its_ordering_codes() {
    assert_resolves(
        "mmbt3906",
        &[
            "MMBT3906",
            "MMBT3906L",
            "MMBT3906LT1G",
            "MMBT3906LT3G",
            "SMMBT3906LT1G",
            "SMMBT3906LT3G",
        ],
    );
    let m = card("MMBT3906LT1G");
    bjt_checks(&m, ComponentKind::BjtPnp, 200.0, 0.2, 40.0);
    // SOT-23 Style 6: 1 base, 2 emitter, 3 collector.
    super::assert_pin_map(&m, "mmbt3906", &super::pins("1=base 2=emitter 3=collector"));
    // Thermal rows: the scalar is the FR-5 board, the least favourable.
    assert_eq!(m.ratings.max_power_w, Some(0.225));
    assert_eq!(m.ratings.theta_ja_c_per_w, Some(556.0));
}

#[test]
fn mmbt3906_siblings_do_not_resolve_to_it() {
    assert_not(
        "mmbt3906",
        &[
            "MMBT3904",
            "MMBT3906X",
            "2N3906",
            "MMBT390",
            "MMBT2907A",
            "SMMBT3904LT1G",
        ],
    );
}

#[test]
fn tip_darlingtons_resolve_one_card_per_type() {
    let table = [
        ("tip120", ComponentKind::BjtNpn, 60.0),
        ("tip121", ComponentKind::BjtNpn, 80.0),
        ("tip122", ComponentKind::BjtNpn, 100.0),
        ("tip125", ComponentKind::BjtPnp, 60.0),
        ("tip126", ComponentKind::BjtPnp, 80.0),
        ("tip127", ComponentKind::BjtPnp, 100.0),
    ];
    for (id, kind, vceo) in table {
        let upper = id.to_uppercase();
        assert_resolves(id, &[upper.as_str(), &format!("{upper}G")]);
        let m = card(&upper);
        bjt_checks(&m, kind, 1000.0, 5.0, vceo);
        // CASE 221A Style 1: 1 base, 2 collector, 3 emitter, 4 collector (tab).
        super::assert_pins(&m, &super::pins("1=base 2=collector 3=emitter"));
        assert_eq!(m.ratings.theta_jc_c_per_w, Some(1.92), "{id}: RθJC");
    }
}

#[test]
fn tip_siblings_stay_distinct() {
    for (id, others) in [
        (
            "tip120",
            ["TIP121", "TIP122", "TIP125", "TIP110", "TIP12"].as_slice(),
        ),
        (
            "tip121",
            ["TIP120", "TIP122", "TIP126", "TIP111", "TIP1211"].as_slice(),
        ),
        (
            "tip122",
            ["TIP120", "TIP121", "TIP127", "TIP112", "TIP1222"].as_slice(),
        ),
        (
            "tip125",
            ["TIP120", "TIP126", "TIP127", "TIP115", "TIP12"].as_slice(),
        ),
        (
            "tip126",
            ["TIP121", "TIP125", "TIP127", "TIP116", "TIP1266"].as_slice(),
        ),
        (
            "tip127",
            ["TIP122", "TIP125", "TIP126", "TIP117", "TIP1277"].as_slice(),
        ),
    ] {
        assert_not(id, others);
    }
}

#[test]
fn s8550_resolves_only_with_a_footprint_that_names_its_package() {
    // TO-92 E-B-C, SOT-23 B-E-C: the same value on the wrong land would swap pins.
    let to92 = card_on("S8550", TO92);
    assert_eq!(to92.id, "s8550_to92");
    super::assert_pin_map(
        &to92,
        "s8550_to92",
        &super::pins("1=emitter 2=base 3=collector"),
    );
    let sot = card_on("S8550", SOT23);
    assert_eq!(sot.id, "s8550_sot23");
    super::assert_pin_map(
        &sot,
        "s8550_sot23",
        &super::pins("1=base 2=emitter 3=collector"),
    );
    assert_eq!(to92.kind, ComponentKind::BjtPnp);
    assert_eq!(sot.kind, ComponentKind::BjtPnp);
    // No footprint, or one that is neither package: neither official card binds.
    for fp in [
        None,
        Some("Package_TO_SOT_THT:TO-220-3_Vertical"),
        Some("Package_TO_SOT_SMD:SOT-223"),
    ] {
        let id = super::query_id(Some("S8550"), Some("S8550"), fp);
        assert!(
            !matches!(id.as_deref(), Some(i) if i.starts_with("s8550_")),
            "S8550 on {fp:?} must not bind a package-specific card, got {id:?}"
        );
    }
    assert_eq!(id_on("S8550-TA", TO92).as_deref(), Some("s8550_to92"));
    assert_ne!(id_on("S8550-TA", SOT23).as_deref(), Some("s8550_to92"));
}

#[test]
fn s8550_ranks_resolve_per_package() {
    for (rank, to92_bf) in [("B", 122.5), ("C", 160.0), ("D", 230.0), ("D3", 350.0)] {
        let value = format!("S8550-{rank}");
        let m = card_on(&value, TO92);
        assert_eq!(
            m.id,
            format!("s8550_to92_{}", rank.to_lowercase()),
            "{value}"
        );
        assert!(
            (super::param(&m, "bf") - to92_bf).abs() < 1e-9,
            "{value}: bf"
        );
        assert_ne!(
            id_on(&value, SOT23).as_deref(),
            Some(m.id.as_str()),
            "{value} on SOT-23"
        );
    }
    for (rank, sot_bf) in [("L", 160.0), ("H", 275.0)] {
        let value = format!("S8550-{rank}");
        let m = card_on(&value, SOT23);
        assert_eq!(
            m.id,
            format!("s8550_sot23_{}", rank.to_lowercase()),
            "{value}"
        );
        assert!(
            (super::param(&m, "bf") - sot_bf).abs() < 1e-9,
            "{value}: bf"
        );
        assert_ne!(
            id_on(&value, TO92).as_deref(),
            Some(m.id.as_str()),
            "{value} on TO-92"
        );
    }
}

#[test]
fn s8550_siblings_do_not_resolve_to_the_s8550_cards() {
    for fp in [TO92, SOT23] {
        for v in [
            "S8050", "SS8550", "S8550X", "S9012", "MMBT8550", "S8550-E", "S8550-D4",
        ] {
            let id = id_on(v, fp);
            assert!(
                !matches!(id.as_deref(), Some(i) if i.starts_with("s8550_")),
                "{v} on {fp} must not bind an s8550 card, got {id:?}"
            );
        }
    }
}

#[test]
fn power_mosfets_resolve_with_pins_and_ratings() {
    for v in ["IRLZ44N", "IRLZ44NPBF", "irlz44npbf"] {
        assert_eq!(id_of(v).as_deref(), Some("irlz44n"), "{v}");
    }
    for v in ["IRFZ44N", "IRFZ44NPBF"] {
        assert_eq!(id_of(v).as_deref(), Some("irfz44n"), "{v}");
    }
    for v in ["IRF3205", "IRF3205PBF"] {
        assert_eq!(id_of(v).as_deref(), Some("irf3205"), "{v}");
    }
    for (value, ron_vgs, id100, pd, vgs_th_max) in [
        ("IRLZ44N", 4.0, 33.0, 110.0, 2.0),
        ("IRFZ44N", 10.0, 35.0, 94.0, 4.0),
        ("IRF3205", 10.0, 80.0, 200.0, 4.0),
    ] {
        let m = card(value);
        assert_eq!(m.kind, ComponentKind::Nmos, "{value}");
        assert_eq!(m.ratings.max_voltage_v, Some(55.0), "{value}: V(BR)DSS");
        assert_eq!(
            m.ratings.max_current_a,
            Some(id100),
            "{value}: ID at TC = 100 °C"
        );
        assert_eq!(m.ratings.max_power_w, Some(pd), "{value}: PD");
        assert_eq!(m.ratings.max_junction_temp_c, Some(175.0), "{value}: TJ");
        assert_eq!(m.ratings.rds_on_vgs, Some(ron_vgs), "{value}: RDS(on) VGS");
        assert_eq!(
            m.ratings.vgs_th_max,
            Some(vgs_th_max),
            "{value}: VGS(th) max"
        );
        super::assert_pins(
            &m,
            &super::pins("1=gate 2=drain 3=source G=gate D=drain S=source"),
        );
    }
    // Logic-level versus standard-level threshold: the part the gate-drive check relies on.
    assert!(super::param(&card("IRLZ44N"), "vto") < super::param(&card("IRFZ44N"), "vto"));
    // The datasheet caps IRF3205 continuous current at the package, not only the die.
    let warning = card("IRF3205").params.get_str("warning").map(str::to_owned);
    assert!(warning.is_some_and(|w| w.contains("75 A")));
}

#[test]
fn power_mosfet_siblings_do_not_resolve_to_the_cards() {
    assert_not(
        "irlz44n",
        &[
            "IRLZ44",
            "IRLZ44NS",
            "IRLZ44NL",
            "IRLZ44NSTRLPBF",
            "IRFZ44N",
            "IRLZ34N",
            "IRLZ44NPBFX",
        ],
    );
    assert_not(
        "irfz44n",
        &[
            "IRFZ44",
            "IRFZ44NS",
            "IRFZ44NL",
            "IRFZ44NSTRLPBF",
            "IRLZ44N",
            "IRF540N",
            "IRFZ44NPBFX",
        ],
    );
    assert_not(
        "irf3205",
        &[
            "IRF3205Z",
            "IRF3205S",
            "IRF3205L",
            "IRF3205ZPBF",
            "IRF3205STRLPBF",
            "IRF3705",
            "IRF3205PBFX",
        ],
    );
}
