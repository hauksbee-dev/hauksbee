//! The corpus batch: the parts added to close the gap the board corpus showed.
//!
//! These tests are deliberately about two different things, and the split
//! matters. The `binds_*` tests pin the MATCH RULES, because a regex is the
//! easiest thing in this database to get subtly wrong and the most expensive:
//! an over-strict rule silently resolves nothing and an over-loose one silently
//! binds the wrong die. The `*_matches_datasheet` tests pin the NUMBERS against
//! the published figures they were derived from, so a later edit cannot quietly
//! move a parameter away from its citation.
//!
//! One rule in particular is worth a test of its own and gets one below.
//! `mpn_re` is AND-ed with every other rule, and a component with no MPN property
//! is compared against it as the empty string, so adding an `mpn_re` beside a
//! `value_re` cannot widen an entry: it makes the entry bind NOTHING on any
//! layout-only board. Every entry in this batch was first written with both, and
//! every one of them resolved nothing at all until that was caught. The test
//! below is what keeps it caught.

use super::{assert_pins, expect_model as resolve, expect_query, param as p, vf_at};
use hauksbee_models::{ComponentKind, ComponentQuery, ModelEntry, ModelLibrary};

/// Resolve by value alone, the way a layout-only board queries the library.
fn by_value(value: &str) -> Option<ModelEntry> {
    super::resolve_value(value).model
}

// ─── match rules ─────────────────────────────────────────────────────────────

#[test]
fn value_strings_from_the_corpus_bind_to_the_right_entry() {
    // Left column: the value string as it appears on a real corpus board, warts
    // and all. Right column: the entry that must win. Every one of these was an
    // UNRESOLVED row in a bind report before the entry existed.
    for (value, want) in [
        // Olimex ESP32-EVB / RP2040-PICO-PC
        ("BC817-40(SOT23)", "bc817_40"),
        ("WPM2015-3/TR", "wpm2015"),
        ("1N5822/SS34/SMA", "1n5822"),
        ("1N5819(S4 SOD-123)", "1n5819hw"),
        ("MCP73833(MSOP10)", "mcp73833"),
        ("BL4054B-42TPRN", "bl4054b_42"),
        ("FB0805/600R/2A", "ferrite_bead_600r_2a"),
        ("DG306-5.0-3P", "dg306_terminal_block"),
        ("TB3 DG306-5.0-3P", "dg306_terminal_block"),
        ("BH10S", "bh10s_idc_header"),
        ("BH10S(B-D-02x5-LF)", "bh10s_idc_header"),
        ("PWRJ-2mm(YDJ-1134)", "dc_barrel_jack_2mm"),
        ("TFC-WXCP11-08-LF", "microsd_socket"),
        // open-book
        ("MEM2075", "gct_mem2075"),
        ("MEM2075-00-140-01-A", "gct_mem2075"),
        ("DMG3415", "dmg3415u"),
        // pvrouter mk2Wifi-C6: the symbol names the MINI-1/-1U pair at once
        ("ESP32-C6-MINI-1/U", "esp32c6_mini1"),
        ("ESP32-C6-MINI-1U-H4", "esp32c6_mini1"),
        // MNT Reform
        ("PMV50ENEAR", "pmv50enea"),
        ("PMV50EPEAR", "pmv50epea"),
        ("PMZ550UNEYL", "pmz550une"),
        ("100@100MHz 3A", "ferrite_bead_100r_3a"),
        ("220@100MHz 1.4A", "ferrite_bead_220r_1a4"),
        ("BLM18PG221SH1D", "ferrite_bead_220r_1a4"),
        ("IMC1210ER100K", "inductor_imc1210"),
        ("0ZCJ0075AF2E", "ptc_0zcj0075"),
        // Adafruit / SparkFun / Arduino
        ("AP2112K-3.3", "ap2112k_3v3"),
        ("MIC5205-3.3", "mic5205_3v3"),
        ("MCP1700-3302E", "mcp1700_3v3"),
        ("NCP1117ST50T3G", "ncp1117_5v"),
        ("MCP73831", "mcp73831"),
        ("6V 0.5A", "polyfuse_0805l050"),
        // C0 spellings: Adafruit, RPi minimal, Olimex, Duet, Lily58, a bare RP2040 value
        ("RT9080-3.3", "rt9080_3v3"),
        ("NCP1117ST33T3G", "ncp1117_3v3_sot223"),
        ("W25Q128JVSIQ", "w25q128jv_soic8"),
        ("W25Q128JVPIQ", "w25q128jv_soic8"),
        ("MCP73831-2-OT", "mcp73831"),
        ("74HCT02", "74hct02"),
        ("74HCT08", "74hct08"),
        ("RP2040_QFN56", "rp2040"),
        ("6V/0.5A", "polyfuse_0805l050"),
        // KiCad demos / Hunt / LumenPnP / ZSWatch
        ("FDV301N", "fdv301n"),
        ("DMG1012T-7", "dmg1012t"),
        ("CJ3134K", "cj3134k"),
        ("AO3422", "ao3422"),
        ("IRLML6402", "irlml6402"),
        ("BAV70", "bav70"),
        ("TLV75533PDBV", "tlv75533p"),
        ("AP22815", "ap22815"),
        ("AP22615A", "ap22615a"),
        ("PCA9306JK", "pca9306"),
        ("TXS0108EPW", "txs0108e"),
        ("0157004.DR", "fuse_nano2_157"),
    ] {
        assert_eq!(
            resolve(value).id,
            want,
            "value {value:?} bound to the wrong entry"
        );
    }
}

#[test]
fn an_mpn_rule_never_makes_its_own_entry_unbindable() {
    // The bug this exists to prevent, and it is a total one rather than a
    // narrowing: every value below resolves by value alone, i.e. with `mpn: None`
    // in the query, which is how a layout-only board asks. An entry carrying any
    // `mpn_re` at all fails that query, because None is compared as "".
    for value in [
        "BC817-40(SOT23)",
        "BC807-40",
        "MCP73831",
        "MCP73833(MSOP10)",
        "BL4054B-42TPRN",
        "AP2112K-3.3",
        "MIC5205-3.3",
        "MCP1700-3302E",
        "NCP1117ST50T3G",
        "TLV75533PDBV",
        "1N5822/SS34/SMA",
        "1N5819(S4 SOD-123)",
        "BAV70",
        "AP22815",
        "PCA9306JK",
        "TXS0108EPW",
    ] {
        assert!(
            by_value(value).is_some(),
            "{value:?} resolves to nothing when queried by value alone. The usual \
             cause is an `mpn_re` on its entry: that rule is ANDed and an absent \
             MPN is compared as \"\", so it makes the entry unbindable on every \
             layout-only board."
        );
    }
}

#[test]
fn polarity_and_die_boundaries_are_not_crossed() {
    // A match rule that reaches one part number too far is worse than one that
    // reaches too little, because it binds silently. These are the neighbours
    // each new rule sits next to.
    assert_eq!(resolve("PMV50ENEAR").kind, ComponentKind::Nmos);
    assert_eq!(resolve("PMV50EPEAR").kind, ComponentKind::Pmos); // complement, one letter apart
    assert_eq!(resolve("BC817-40").kind, ComponentKind::BjtNpn);
    assert_eq!(resolve("BC807-40").kind, ComponentKind::BjtPnp); // ditto
    assert_eq!(resolve("AO3422").kind, ComponentKind::Nmos);

    // The hFE bin IS the model. Nexperia ships one SPICE card per bin, these
    // entries are the -40 cards, and the other bins must not bind to them.
    for other_bin in ["BC817", "BC817-16", "BC817-25", "BC807-16", "BC807-25"] {
        assert!(
            by_value(other_bin).is_none_or(|m| m.id != "bc817_40" && m.id != "bc807_40"),
            "{other_bin} must not bind to a -40 card: different bin, different BF"
        );
    }

    // The C0 widenings stop at the datasheet's package, voltage and die edges.
    for (value, not) in [
        ("NCP1117LPST33T3G", "ncp1117_3v3_sot223"),
        ("W25Q128JVFIQ", "w25q128jv_soic8"),
        ("RT9080-0.8", "rt9080_3v3"),
        ("MCP73831-3-OT", "mcp73831"),
        ("74HCT02", "74hc02"),
    ] {
        assert!(
            by_value(value).is_none_or(|m| m.id != not),
            "{value} bound {not}"
        );
    }

    // A regulator's voltage code is not optional. An adjustable part or a bare
    // family name says nothing about its output and must stay unresolved rather
    // than inherit a fixed one.
    // A bare family name also leaves the package, and so the pin map, open.
    for indeterminate in ["MCP1700", "TLV62568"] {
        assert!(
            by_value(indeterminate).is_none_or(|m| m.kind != ComponentKind::Vreg),
            "{indeterminate} has no determinable output voltage and must not \
             bind to a fixed-output vreg entry"
        );
    }
    // The bare MIC5205 and LM1117 numbers are the families' ADJUSTABLE parts, so
    // they bind only to cards whose rail is the board's own divider against a
    // feedback reference, and those carry no output voltage of their own.
    for (value, card) in [("MIC5205", "mic5205_adj"), ("LM1117", "lm1117_adj")] {
        let m = by_value(value).unwrap_or_else(|| panic!("{value} binds its adjustable card"));
        assert_eq!(m.id, card);
        assert!(
            m.params.get_f64("vout").is_none(),
            "{value}: an adjustable card must not carry a fixed output voltage"
        );
    }
    // An adjustable buck may bind, but only to a board-feedback card whose rail
    // comes from the fitted divider on FB, never to a fixed output.
    let adjustable = by_value("SY8089AAAC").expect("SY8089AAAC binds its feedback card");
    assert_eq!(adjustable.id, "sy8089_sot23_5");
}

#[test]
fn not_assembled_and_mechanical_parts_are_ignored_not_invented() {
    // Olimex marks a no-fit position by prefixing the value with NA, which the
    // BOM shipped with the board confirms. An empty position is an open circuit,
    // so `ignore` is the physically correct answer here rather than a shortcut.
    for value in [
        "NA/R0603",
        "NA(1k/R0603)",
        "NA(TB2 DG306-5.0-2P)",
        "NA(NCP303LSN27T1G(SOT-23-5))",
        "DNP",
        "NC",
    ] {
        let m = resolve(value);
        assert_eq!(
            m.kind,
            ComponentKind::Ignore,
            "{value:?} should be an ignored no-fit"
        );
    }

    // Switches, across the six spellings the corpus uses.
    for value in [
        "Choc",
        "SW_Push",
        "SW_PUSH",
        "RESET",
        "ML",
        "T1107A",
        "SW_DIP_x01",
    ] {
        assert_eq!(
            resolve(value).kind,
            ComponentKind::Ignore,
            "{value:?} is a switch"
        );
    }
}

#[test]
fn a_net_tie_is_a_short_and_not_an_open() {
    // The distinction the entry exists for. Ignoring a net tie splits a ground
    // plane in two in simulation and then reports the consequences as faults on
    // a board that is electrically fine.
    let m = expect_query(Some("NetTie_2"), None, Some("NetTie:NetTie-2_SMD_Pad0.5mm"));
    assert_eq!(
        m.kind,
        ComponentKind::Passive,
        "a net tie is a resistor, not an ignore"
    );
    let ohms = p(&m, "ohms");
    assert!(
        ohms > 0.0 && ohms <= 0.005,
        "a net tie is a few square mm of copper: expected a milliohm, got {ohms}"
    );
}

// ─── numbers against their citations ─────────────────────────────────────────

#[test]
fn the_nexperia_bjt_cards_are_transcribed_exactly() {
    // These are not a fit. Nexperia publishes BC817-40.txt and BC807-40.txt and
    // these are those files' `.MODEL Transistor` parameters, so the test is a
    // string-for-number comparison against the vendor card and any drift is a
    // transcription error rather than a modelling disagreement.
    let npn = resolve("BC817-40");
    for (k, want) in [
        ("is", 5.244e-14),
        ("bf", 440.0),
        ("nf", 0.9753),
        ("vaf", 48.46),
        ("br", 50.35),
        ("ikf", 0.5843),
        ("ise", 1.62e-15),
        ("ne", 1.301),
        ("rb", 43.0),
        ("rc", 0.2327),
        ("re", 0.1312),
        ("tf", 6.08e-10),
    ] {
        let got = p(&npn, k);
        assert!(
            (got - want).abs() <= want.abs() * 1e-9,
            "BC817-40 {k}: card says {want:e}, entry says {got:e}"
        );
    }

    let pnp = resolve("BC807-40");
    for (k, want) in [
        ("is", 1.132e-13),
        ("bf", 535.0),
        ("vaf", 20.46),
        ("ikf", 0.2819),
        ("rb", 30.0),
    ] {
        let got = p(&pnp, k);
        assert!(
            (got - want).abs() <= want.abs() * 1e-9,
            "BC807-40 {k}: card says {want:e}, entry says {got:e}"
        );
    }
}

/// Level-1 on-resistance the entry's own parameters imply at `vgs`.
///
/// R(VGS) = 1/(kp*(VGS-vto)) + rd + rs, the same relation the fits in
/// db/mosfet.toml are derived from. Magnitudes, so P-channel works too.
fn rds_on(m: &ModelEntry, vgs: f64) -> f64 {
    let vto = p(m, "vto").abs();
    let kp = p(m, "kp");
    let rd = p(m, "rd");
    let rs = p(m, "rs");
    1.0 / (kp * (vgs.abs() - vto)) + rd + rs
}

#[test]
fn every_mosfet_reproduces_the_rows_its_fit_was_anchored_on() {
    // The two rows each fit consumed, from the datasheet named in the entry.
    // Tolerance 3%, not zero, and the reason is worth knowing: most of these fits
    // reproduce their anchors exactly, but PMV50ENEAR and PMV50EPEAR deliberately
    // take a kp BETWEEN the transconductance route and the on-resistance route
    // because the two agreed closely enough to make picking a winner arbitrary.
    // That compromise costs those two a couple of percent on their own anchors
    // (+2.1% and -1.4%, which is what their entries record). 3% holds every part
    // to its derivation while leaving that trade intact.
    for (part, vgs_lo, r_lo, vgs_hi, r_hi) in [
        ("FDV301N", 2.7, 3.8, 4.5, 3.1),
        ("DMG1012T-7", 1.8, 0.5, 4.5, 0.3),
        ("PMV50ENEAR", 4.5, 0.039, 10.0, 0.030),
        ("PMV50EPEAR", 4.5, 0.049, 10.0, 0.035),
        ("WPM2015-3/TR", 2.5, 0.103, 4.5, 0.081),
        ("AO3422", 2.5, 0.157, 4.5, 0.125),
        ("PMZ550UNEYL", 1.5, 0.890, 4.5, 0.550),
    ] {
        let m = resolve(part);
        for (vgs, want) in [(vgs_lo, r_lo), (vgs_hi, r_hi)] {
            let got = rds_on(&m, vgs);
            let err = (got - want).abs() / want;
            assert!(
                err < 0.03,
                "{part} Rds(on) at Vgs={vgs}: datasheet {want}, model {got:.6} ({:.1}%)",
                err * 100.0
            );
        }
    }
}

#[test]
fn the_out_of_sample_mosfet_rows_are_within_their_recorded_error() {
    // Three parts publish a THIRD 25 C on-resistance row that its fit did not
    // use, which is the only genuine out-of-sample check available. The bounds
    // here are the residuals db/mosfet.toml records, so this test is what stops
    // those recorded numbers becoming stale claims.
    for (part, vgs, want, tol) in [
        // fitted on 1.8 V and 4.5 V, checked at 2.5 V: entry records -2.8%
        ("DMG1012T-7", 2.5, 0.4, 0.05),
        // fitted on 1.5 V and 4.5 V, checked at 1.8 V (+0.3%) and 2.5 V (-1.4%)
        ("PMZ550UNEYL", 1.8, 0.770, 0.03),
        ("PMZ550UNEYL", 2.5, 0.660, 0.03),
        // CJ3134K's three max-only rows cannot be fitted by any single square
        // law; the entry records +20.2% at 2.5 V and this bound holds it there
        // rather than pretending it is small.
        ("CJ3134K", 2.5, 0.450, 0.25),
    ] {
        let m = resolve(part);
        let got = rds_on(&m, vgs);
        let err = (got - want).abs() / want;
        assert!(
            err < tol,
            "{part} out-of-sample Rds(on) at Vgs={vgs}: datasheet {want}, model \
             {got:.6}, error {:.1}% exceeds the recorded {:.0}%",
            err * 100.0,
            tol * 100.0
        );
    }
}

#[test]
fn the_irlml6402_carries_infineons_own_level_1_card() {
    // The one part in the batch where the vendor ships a genuinely LEVEL=1 SPICE
    // model, so the entry uses it rather than a derivation. It is also the only
    // part with a published lambda, which is how you can tell the card is being
    // used and not quietly replaced by a datasheet fit.
    let m = resolve("IRLML6402");
    assert_eq!(m.kind, ComponentKind::Pmos);
    for (k, want) in [
        ("vto", -1.0),
        ("kp", 12.788),
        ("lambda", 0.011_135_8),
        ("rs", 0.024_670_4),
    ] {
        let got = p(&m, k);
        assert!(
            (got - want).abs() <= want.abs() * 1e-9,
            "IRLML6402 {k}: Infineon's card says {want}, entry says {got}"
        );
    }
}

/// Thermal voltage for the Shockley forward-voltage checks below.
const VT: f64 = 0.025_852;

#[test]
fn the_new_diodes_sit_on_their_published_forward_points() {
    // Anchors first, at 2 mV: these are the points each fit consumed.
    for (part, i, want) in [
        ("1N5822", 3.0, 0.525), // Vishay 88526, 25 C
        ("1N5822", 9.4, 0.950),
        ("1N5819HW", 0.1, 0.320), // Diodes DS30217, 25 C
        ("1N5819HW", 3.0, 0.750),
        ("BAV70", 0.001, 0.715), // Nexperia BAV70 v.9, Table 7 maxima
        ("BAV70", 0.150, 1.250),
    ] {
        let m = resolve(part);
        let got = vf_at(&m, i, VT);
        assert!(
            (got - want).abs() < 2e-3,
            "{part} at {i} A: datasheet {want} V, model {got:.4} V"
        );
    }
}

#[test]
fn the_diode_out_of_sample_points_land_under_the_published_maximum() {
    // Rows no fit used. These datasheets publish MAXIMA, so a model fitted to
    // the maximum envelope must predict at or below it: a model that read HIGH
    // against a guaranteed limit would be wrong twice, once on the number and
    // once on the direction.
    for (part, i, limit) in [
        ("1N5822", 1.0, 0.390),  // onsemi 1N5820/D third point; entry records -5%
        ("BAV70", 0.010, 0.855), // entry records -6.5%
        ("BAV70", 0.050, 1.000), // entry records -5.1%
    ] {
        let m = resolve(part);
        let got = vf_at(&m, i, VT);
        assert!(
            got <= limit,
            "{part} at {i} A: model says {got:.4} V, above the datasheet MAXIMUM \
             of {limit} V"
        );
        assert!(
            got > limit * 0.85,
            "{part} at {i} A: model says {got:.4} V against a {limit} V maximum, \
             which is further below it than the entry's recorded residual"
        );
    }

    // BAV70's ideality factor is pinned at 1.0 on purpose: a free-n fit to the
    // max envelope returns 2.35, which is not physics for a silicon switching
    // junction. This is what stops it drifting back.
    let n = p(&resolve("BAV70"), "n");
    assert!(
        (n - 1.0).abs() < 1e-9,
        "BAV70's n must stay pinned at 1.0, got {n}"
    );
}

#[test]
fn the_ldo_outputs_and_dropouts_match_their_datasheets() {
    // `dropout_v` is the guaranteed maximum at the card's rated load and stated
    // temperature range, not the typical column: AP2112 DS39724 Rev. 2-2 p.7
    // (600 mA, TA = 25 °C); MIC5205 DS20005785A table 1-1 (150 mA,
    // TJ = -40..125 °C); MCP1700 DS20001826F table 1-1 (250 mA);
    // NCP1117/D Rev.31 electrical table (800 mA, applicable operating ambient
    // range); TLV755P SBVS320D §5.5 (3.3 V non-DYD, 500 mA, TJ = -40..125 °C).
    for (part, vout, dropout, iq) in [
        ("AP2112K-3.3", 3.3, 0.4, 55.0e-6),
        ("MIC5205-3.3", 3.3, 0.35, 80.0e-6),
        ("MCP1700-3302E", 3.3, 0.35, 1.6e-6),
        ("NCP1117ST50T3G", 5.0, 1.2, 6.0e-3),
        ("TLV75533PDBV", 3.3, 0.238, 25.0e-6),
    ] {
        let m = resolve(part);
        assert_eq!(m.kind, ComponentKind::Vreg, "{part}");
        for (k, want) in [("vout", vout), ("dropout_v", dropout), ("iq_a", iq)] {
            let got = p(&m, k);
            assert!(
                (got - want).abs() <= want * 1e-6,
                "{part} {k}: datasheet {want}, entry {got}"
            );
        }
    }
}

#[test]
fn the_chargers_float_at_the_cell_voltage_their_code_selects() {
    // A charger's float voltage is the rail every other part on the board sees,
    // so it is the one number that must not be approximate. 4.2 V is the
    // single-cell Li-Ion standard; the 4.35/4.40/4.50 V order codes are
    // different parts and do not bind to these entries.
    for part in ["MCP73831", "MCP73833(MSOP10)", "BL4054B-42TPRN"] {
        let m = resolve(part);
        assert_eq!(m.kind, ComponentKind::Vreg, "{part}");
        let vout = p(&m, "vout");
        assert!(
            (vout - 4.2).abs() < 1e-9,
            "{part} should float at 4.2 V, got {vout}"
        );
    }
}

#[test]
fn the_traced_bead_and_fuse_resistances_are_their_own_parts() {
    // The point of tracing each rating string to a real part: these differ by
    // two orders of magnitude, and a single representative value for all of them
    // would be wrong by that much. In particular a 0.5 A polyfuse is 0.15 ohm,
    // not the 0.01 ohm a generic fuse entry carries, which is 75 mV of rail drop
    // at its own hold current.
    for (value, want) in [
        ("220@100MHz 1.4A", 0.10), // Murata BLM18PG221SH1D, DCR max initial
        ("100@100MHz 3A", 0.022),  // Murata BLM18SP101SH1D
        ("FB0805/600R/2A", 0.060), // Murata BLM21SP601SH1D
        ("IMC1210ER100K", 2.1),    // Vishay IMC-1210, a wirewound, not a bead
        ("6V 0.5A", 0.150),        // Littelfuse 0805L050, Rmin initial
        ("0ZCJ0075AF2E", 0.090),   // Bel Fuse 0ZCJ, Rmin initial
        ("0157004.DR", 0.0163),    // Littelfuse 157 nominal cold resistance
    ] {
        let got = p(&resolve(value), "ohms");
        assert!(
            (got - want).abs() <= want * 1e-6,
            "{value:?}: datasheet {want} ohm, entry {got} ohm"
        );
    }
}

#[test]
fn the_pass_gate_translators_are_switches_and_not_logic() {
    // TI's and Nexperia's own datasheets call these pass gates with edge-rate
    // accelerators. A part with no boolean function of its input pins cannot be
    // a `digital` entry with a logic block, and writing one would be a
    // fabrication that happens to lint clean.
    for part in ["PCA9306JK", "TXS0108EPW"] {
        let m = resolve(part);
        assert_eq!(m.kind, ComponentKind::AnalogSwitch, "{part} is a pass gate");
        let ron = p(&m, "ron");
        let roff = p(&m, "roff");
        assert!(
            ron > 0.0 && ron < roff,
            "{part}: ron {ron} must be below roff {roff}"
        );
    }

    // The NTS0104 belongs in the same group but has no switch entry: its
    // datasheet publishes no on-resistance at all, and `ron` is the one number
    // the kind cannot do without. It is named by an IDENTITY-ONLY card, so it
    // binds unresolved; this asserts the gap stays honest rather than being
    // filled with a guess.
    let nts = resolve("NTS0104GU12");
    assert!(
        nts.kind != ComponentKind::AnalogSwitch
            && nts.logic.is_empty()
            && nts.params.get_bool("identity_only") == Some(true),
        "NTS0104GU12 must stay identity-only: no vendor on-resistance exists to model"
    );
}

#[test]
fn the_addressable_leds_stay_unresolved() {
    // The largest deliberate gap in the corpus, ~250 instances. Every datasheet
    // in the family was read and none publishes VOH or VOL, which the `digital`
    // kind requires; and DOUT is DIN with this pixel's own 24 bits consumed, not
    // DIN delayed, so there is no combinational relation between the pins to
    // write down. `dout = din` would lint clean and be false.
    for part in [
        "YS-SK6812MINI-E",
        "YS-SK6812MINI",
        "SK6805-EC15",
        "WS2812-2020",
        "WS2812B",
    ] {
        assert!(
            by_value(part).is_none(),
            "{part} must stay unresolved: its cascade is stateful and no \
             datasheet in the family publishes an output level"
        );
    }
}

#[test]
fn c0_cards_carry_their_datasheet_pins_and_levels() {
    // nPM1300 Table 35: VOUT1 = 1, PVDD = 4. NCP1117/D page 1, SOT-223: 1 gnd, 2 out, 3 in.
    assert_pins(&resolve("nPM1300-QEXX"), &[("1", "out"), ("4", "in")]);
    assert_pins(
        &resolve("NCP1117ST33T3G"),
        &[("1", "gnd"), ("2", "out"), ("3", "in")],
    );
    // SN74HCT02/08 tables: the HC pad maps with TTL input thresholds.
    for (value, pad, role) in [("74HCT02", "10", "g3y"), ("74HCT08", "11", "y4")] {
        let m = resolve(value);
        assert_pins(&m, &[(pad, role)]);
        assert_eq!((p(&m, "vih"), p(&m, "vil")), (2.0, 0.8));
    }
}

#[test]
fn the_dmg3415_base_number_does_not_take_the_dfn_variant() {
    // DS31735 is the SOT23 part; the DFN2015 DMG3415UFY has another pad map.
    assert!(by_value("DMG3415UFY").is_none_or(|e| e.id != "dmg3415u"));
}

#[test]
fn the_c6_module_card_does_not_take_the_bare_chip() {
    // The MINI-1 pad map is the module's; the bare QFN-40 chip numbers differently.
    for value in ["ESP32-C6", "ESP32-C6FH4", "ESP32-C6-MINI-1/X"] {
        assert!(
            by_value(value).is_none_or(|e| e.id != "esp32c6_mini1"),
            "{value} must not bind the module card"
        );
    }
}

#[test]
fn the_mem2075_socket_carries_the_gct_drawing_pad_map() {
    // GCT MEM2075 rev A1 sheet 1: P1 DAT2 .. P8 DAT1, P9 card detect, SH1-4 shell.
    let m = resolve("MEM2075-00-140-01-A");
    assert_eq!(m.kind, ComponentKind::Connector);
    assert_pins(
        &m,
        &[
            ("P2", "cs"),
            ("P3", "mosi"),
            ("P4", "vdd"),
            ("P5", "sck"),
            ("P6", "gnd"),
            ("P7", "miso"),
            ("P9", "cd"),
            ("SH4", "shell4"),
        ],
    );
}

#[test]
fn footprint_only_cards_never_speak_for_a_u_or_ic_reference() {
    // MNT Reform U7: a switching regulator in a power package is not a MOSFET.
    let id = |reference: &str| {
        let q = ComponentQuery {
            reference: Some(reference.into()),
            value: Some("Si7850DP".into()),
            footprint: Some("Package_SO:PowerPAK_SO-8_Single".into()),
            ..Default::default()
        };
        ModelLibrary::builtin_shared()
            .resolve(&q)
            .model
            .map(|m| m.id)
    };
    assert_eq!((id("U7"), id("IC3")), (None, None));
    assert_eq!(id("Q1").as_deref(), Some("generic_nmos_power_pkg"));
}

/// Resolve by value and footprint, the way a layout board with a package queries.
fn by_value_on(value: &str, footprint: &str) -> Option<ModelEntry> {
    let q = ComponentQuery {
        reference: Some("U1".into()),
        value: Some(value.into()),
        footprint: Some(footprint.into()),
        ..Default::default()
    };
    ModelLibrary::builtin_shared().resolve(&q).model
}

#[test]
fn fixed_ldos_and_shunt_references_bind_only_their_stated_package() {
    let id = |value: &str, footprint: &str| by_value_on(value, footprint).map(|m| m.id);
    // AP1117: E is SOT223, and a letter-less spelling is held to SOT-223 too.
    for value in ["AP1117-33", "AP1117E33G-13", "AP1117IE33G-13"] {
        assert_eq!(
            id(value, "Package_TO_SOT_SMD:SOT-223-3_TabPin2").as_deref(),
            Some("ap1117_33_sot223"),
            "{value}"
        );
    }
    assert_ne!(
        id("AP1117-33", "Package_TO_SOT_SMD:TO-252-2").as_deref(),
        Some("ap1117_33_sot223")
    );
    // The adjustable AP1117 has no output voltage.
    assert!(by_value("AP1117").is_none_or(|m| m.kind != ComponentKind::Vreg));
    // RT9013: only B (SOT-23-5); SC-82 (Y) and WDFN (QW) maps differ.
    assert_eq!(
        id("RT9013-33GB", "Package_TO_SOT_SMD:SOT-23-5").as_deref(),
        Some("rt9013_33_sot23_5")
    );
    for other in ["RT9013-33GY", "RT9013-33GQW", "RT9013-18GB"] {
        assert_ne!(
            id(other, "").as_deref(),
            Some("rt9013_33_sot23_5"),
            "{other}"
        );
    }
    // TPS709: plain TPS709 DBV only; TPS709A/B DBV put OUT elsewhere.
    assert_eq!(
        id("TPS70918DBVR", "Package_TO_SOT_SMD:SOT-23-5").as_deref(),
        Some("tps70918_dbv")
    );
    assert_ne!(id("TPS709A18DBVR", "").as_deref(), Some("tps70918_dbv"));
    // LM4040 3.0 V in DBZ: the three-pad outline only.
    for value in ["LM4040DBZ-3", "LM4040DBZ-3 VREF"] {
        assert_eq!(
            id(value, "Package_TO_SOT_SMD:SOT-23").as_deref(),
            Some("lm4040_30_dbz"),
            "{value}"
        );
    }
    assert_ne!(
        id("LM4040DBZ-3", "Package_TO_SOT_SMD:SOT-23-5").as_deref(),
        Some("lm4040_30_dbz")
    );
}

#[test]
fn the_new_fixed_ldo_and_reference_numbers_match_their_datasheets() {
    let on = |value: &str, footprint: &str| by_value_on(value, footprint).expect(value);
    // (value, footprint, vout, dropout_v, iq_a): DS31009 Rev. 24-3, DS9013-10, SBVS186H.
    for (value, footprint, vout, dropout, iq) in [
        ("AP1117E33G-13", "SOT223", 3.3, 1.3, 5.0e-3),
        ("RT9013-33GB", "SOT-23-5", 3.3, 0.4, 25.0e-6),
        ("TPS70918DBVR", "SOT-23-5", 1.8, 0.9, 1.3e-6),
    ] {
        let m = on(value, footprint);
        assert_eq!(
            (p(&m, "vout"), p(&m, "dropout_v"), p(&m, "iq_a")),
            (vout, dropout, iq),
            "{value}"
        );
    }
    assert_pins(
        &on("AP1117-33", "SOT-223"),
        &[("1", "gnd"), ("2", "out"), ("3", "in"), ("4", "out")],
    );
    for value in ["RT9013-33GB", "TPS70918DBVR"] {
        assert_pins(
            &on(value, "SOT-23-5"),
            &[("1", "in"), ("2", "gnd"), ("3", "en"), ("5", "out")],
        );
    }
    let r = on("LM4040DBZ-3", "SOT-23");
    assert_eq!(
        (r.kind, p(&r, "bv"), p(&r, "ibv")),
        (ComponentKind::Diode, 3.0, 1.0e-4)
    );
    assert_pins(&r, &[("1", "cathode"), ("2", "anode")]);
}
