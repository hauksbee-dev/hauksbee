//! The behavioural power-IC models resolve and carry their behavioural blocks.

use hauksbee_models::{schema::CurrentProgramSemantics, ComponentQuery, ModelLibrary};

#[test]
fn power_ics_resolve_and_carry_behavioral() {
    let lib = ModelLibrary::builtin();
    for (val, id) in [
        ("LTC4020EUHFPBF", "ltc4020"),
        ("LTC6803-4", "ltc6803_4"),
        ("nPM1300-QEXX", "npm1300"),
    ] {
        let q = ComponentQuery {
            value: Some(val.into()),
            ..Default::default()
        };
        let r = lib.resolve(&q);
        let m = r.model.unwrap_or_else(|| panic!("{val} did not resolve"));
        assert_eq!(m.id, id, "{val} resolved to wrong id");
        assert!(
            !m.behavioral.is_empty(),
            "{val} must carry a behavioural block"
        );
    }

    // The dynamic LTC4020 converter reads R8 (ILIMIT) and R49 off the board.
    // Its declarative protection law records both exact Kelvin pairs but is not
    // promoted to a steady-state ampacity load.
    let q = ComponentQuery {
        value: Some("LTC4020EUHFPBF".into()),
        ..Default::default()
    };
    let m = lib.resolve(&q).model.unwrap();
    let conv = m.behavioral.converter.as_ref().expect("converter");
    let sp = conv.iin_program.as_ref().expect("iin_program");
    assert_eq!(sp.prog_ref.as_deref(), Some("R8"));
    assert_eq!(sp.rsense_refs, ["R49", "R50"]);
    let program = m.current_program.as_ref().expect("protection law");
    match &program.equation {
        hauksbee_models::schema::CurrentProgramEquation::SenseScaledResistance {
            sense_roles,
            sense_far_roles,
            ..
        } => {
            assert_eq!(sense_roles, &["senstop", "sensbot"]);
            assert_eq!(sense_far_roles, &["sensvin", "sensgnd"]);
        }
        other => panic!("unexpected LTC4020 law: {other:?}"),
    }
    assert_eq!(m.behavioral.fsm.as_ref().unwrap().states.len(), 2);

    // nPM1300 SHPHLD pulls to vsys.
    let q = ComponentQuery {
        value: Some("nPM1300-QEXX".into()),
        ..Default::default()
    };
    let m = lib.resolve(&q).model.unwrap();
    let pin = m.behavioral.pins.get("shphld").expect("shphld pin");
    assert_eq!(pin.pull_to.as_deref(), Some("vsys"));

    // LTC6803 leak law reads the tie resistor by ref.
    let q = ComponentQuery {
        value: Some("LTC6803-4".into()),
        ..Default::default()
    };
    let m = lib.resolve(&q).model.unwrap();
    assert_eq!(m.behavioral.laws.len(), 1);
    assert_eq!(m.behavioral.laws[0].name, "absent_cell_leak");
    assert_eq!(m.params.get_str("tie_ohms_from_ref"), Some("R52"));
}

#[test]
fn tp4054_uses_the_current_datasheets_piecewise_programming_law() {
    let lib = ModelLibrary::builtin();
    let model = lib
        .resolve(&ComponentQuery {
            value: Some("TP4054".into()),
            ..Default::default()
        })
        .model
        .expect("TP4054 resolves");
    let program = model
        .current_program
        .as_ref()
        .expect("TP4054 has a board-programmed charge-current law");

    assert_eq!(model.id, "tp4054");
    assert_eq!(program.pin, "prog");
    assert_eq!(program.semantics, CurrentProgramSemantics::RegulatedCurrent);
    assert_eq!(program.max_operating_current_a, Some(0.401));
    assert_eq!(
        model.ratings.max_current_a,
        Some(0.8),
        "the BAT-pin absolute maximum remains a stress rating, not an operating clamp"
    );

    // The checked-in Watchy board fits R3 = 10 kOhm, which lies in the simple
    // <=150 mA branch of Top Power's current Rev 2.1 equation.
    let watchy = program
        .equation_current_a(10_000.0)
        .expect("a positive resistor is evaluable");
    assert!((watchy - 0.1).abs() < 1e-12, "Watchy: got {watchy} A");

    // 5.1 kOhm is above the branch point. Treating every value as 1000/R gives
    // 196 mA; the published high-current equation gives about 186.53 mA.
    let above_transition = program
        .equation_current_a(5_100.0)
        .expect("a positive resistor is evaluable");
    let published = 1.2 / (5.1 + 4.0 / 3.0);
    assert!(
        (above_transition - published).abs() < 1e-12,
        "5.1 kOhm must use the high-current branch: expected {published}, got {above_transition}"
    );

    // The datasheet's 1.66 kOhm application circuit is the nominal 400 mA
    // endpoint (component tolerances explain the rounded printed resistor).
    let nominal_max = program
        .operating_current_a(1_660.0)
        .expect("a positive resistor is evaluable");
    let nominal_equation = 1.2 / (1.66 + 4.0 / 3.0);
    assert!((nominal_max - nominal_equation).abs() < 1e-12);

    assert_eq!(
        program.operating_current_a(100.0),
        None,
        "an undersized RPROG is outside the sourced equation domain; the model must not invent exact 400 mA saturation"
    );
}

#[test]
fn bl4054b_keeps_its_single_inverse_resistance_law() {
    let lib = ModelLibrary::builtin();
    let model = lib
        .resolve(&ComponentQuery {
            value: Some("BL4054B-42TPRN".into()),
            ..Default::default()
        })
        .model
        .expect("BL4054B resolves");
    let program = model.current_program.as_ref().expect("BL4054B law");

    assert_eq!(model.id, "bl4054b_42");
    assert_eq!(program.pin, "prog");
    assert_eq!(program.semantics, CurrentProgramSemantics::RegulatedCurrent);
    assert_eq!(program.max_operating_current_a, Some(0.8));
    assert_eq!(program.equation_current_a(2_000.0), Some(0.5));
    assert_eq!(program.operating_current_a(1_250.0), Some(0.8));
}

#[test]
fn ap22615_exposes_its_ocp_threshold_without_recasting_it_as_load_current() {
    let lib = ModelLibrary::builtin();
    let model = lib
        .resolve(&ComponentQuery {
            value: Some("AP22615A".into()),
            ..Default::default()
        })
        .model
        .expect("AP22615A resolves");
    let program = model.current_program.as_ref().expect("RLIM law");

    assert_eq!(program.equation_current_a(6_800.0), Some(1.0));
    assert_eq!(program.operating_current_a(6_800.0), Some(1.0));
    assert!((program.operating_current_a(1_940.0).unwrap() - 6800.0 / 1940.0).abs() < 1e-12);
    assert_eq!(program.max_operating_current_a, None);
    assert_eq!(program.semantics, CurrentProgramSemantics::ProtectionLimit);
    assert_eq!(
        model.ratings.max_pin_current_a, None,
        "the absolute-maximum table says load current is internally limited; it does not specify 2 A"
    );
}

#[test]
fn ltc4020_exposes_its_two_resistor_protection_limit_to_simulation() {
    let lib = ModelLibrary::builtin();
    let model = lib
        .resolve(&ComponentQuery {
            value: Some("LTC4020EUHFPBF".into()),
            ..Default::default()
        })
        .model
        .expect("LTC4020 resolves");

    let rail_program = model
        .current_program
        .as_ref()
        .expect("the two-resistor law must be available to protection analysis");
    assert_eq!(rail_program.pin, "ilimit");
    assert_eq!(
        rail_program.semantics,
        CurrentProgramSemantics::ProtectionLimit
    );
    assert!(
        (rail_program
            .equation_current_with_sense_a(7_150.0, 0.01)
            .unwrap()
            - 1.7875)
            .abs()
            < 1e-12
    );
    assert_eq!(
        rail_program.equation_current_with_sense_a(100_000.0, 0.01),
        Some(5.0)
    );
    let input_limit = model
        .behavioral
        .converter
        .as_ref()
        .and_then(|converter| converter.iin_program.as_ref())
        .expect("LTC4020 must retain the board-resolved ILIMIT/RSENSE law");
    assert_eq!(input_limit.prog_ref.as_deref(), Some("R8"));
    assert_eq!(input_limit.rsense_refs, ["R49", "R50"]);

    // Datasheet transfer: VILIMIT = 50 uA * RILIMIT, effective over 0..1 V;
    // that voltage scales the 50 mV full-scale sense threshold. Thus 20 kOhm
    // reaches full scale, 7.15 kOhm programs 17.875 mV, and 100 kOhm saturates
    // at 50 mV. These are datasheet-derived endpoints, not fault-calibration
    // constants chosen to straddle the Reform adapter budget.
    let programmed_limit = |program_ohms: f64, sense_ohms: f64| {
        (input_limit.vprog_ref * program_ohms / input_limit.prog_ref_ohms)
            .min(input_limit.v_sense_full)
            / sense_ohms
    };
    assert!((programmed_limit(7_150.0, 0.01) - 1.7875).abs() < 1e-12);
    assert!((programmed_limit(100_000.0, 0.01) - 5.0).abs() < 1e-12);
}

/// The ST SR2 Smart Reset (DocID026047 Rev 2): one entry covers the six
/// ordering options Table 7 lists, because they differ only in the tSRC delay
/// and the tREC option, neither of which this model carries. What it does carry
/// is the UDFN6 pin map and the fixed-volt input thresholds, and both are
/// asserted here because they are what makes a Watchy's U1 bind at all.
#[test]
fn the_sr2_family_binds_every_ordering_option_to_one_udfn6_map() {
    let lib = ModelLibrary::builtin();
    for value in [
        "SR2HARU", "SR2LABU", "SR2LARU", "SR2PARU", "SR2UABU", "SR2UARU",
    ] {
        let q = ComponentQuery {
            value: Some(value.into()),
            ..Default::default()
        };
        let m = lib
            .resolve(&q)
            .model
            .unwrap_or_else(|| panic!("{value} did not resolve"));
        assert_eq!(m.id, "sr2", "{value} resolved to the wrong entry");
    }

    // Neither the 4-pin SR1 nor an ordering code ST does not build may borrow
    // this map: the SR1 has a different package and pinout entirely.
    for value in ["SR1HARU", "SR2HABU", "SR2PABU", "SR2"] {
        let q = ComponentQuery {
            value: Some(value.into()),
            ..Default::default()
        };
        assert!(
            lib.resolve(&q).model.is_none_or(|m| m.id != "sr2"),
            "{value} must not resolve to the SR2 entry"
        );
    }

    let q = ComponentQuery {
        value: Some("SR2HARU".into()),
        ..Default::default()
    };
    let m = lib.resolve(&q).model.expect("SR2HARU resolves");
    // Table 1 Signal names. Pad 3 is the reset OUTPUT and pads 2/5 are the two
    // push-button inputs; swapping them would make the model read the ESP32's
    // EN net as a button press.
    for (pad, role) in [
        ("1", "vss"),
        ("2", "sr1_n"),
        ("3", "rst_n"),
        ("4", "nc"),
        ("5", "sr0_n"),
        ("6", "vcc"),
    ] {
        assert_eq!(
            m.pins.get(pad).map(String::as_str),
            Some(role),
            "SR2 pad {pad} must be {role}"
        );
    }
    assert_eq!(m.pins.len(), 6, "UDFN6 has six pads");

    // Table 4. The SR inputs have FIXED thresholds, not VCC fractions: 0.85 V
    // VIH min and 0.3 V VIL max regardless of a 1.65 to 5.5 V rail. Rounding
    // these up to a CMOS 0.3/0.7 x VCC pair would misjudge every press.
    assert_eq!(m.params.get_f64("vih"), Some(0.85));
    assert_eq!(m.params.get_f64("vil"), Some(0.3));
    assert_eq!(m.params.get_f64("vol"), Some(0.3));
    assert_eq!(
        m.params.get_f64("voh"),
        None,
        "these options are open-drain with no internal pull-up, and the datasheet's \
         VOH row is push-pull only; a voh here would invent a high-side driver"
    );
    // ICC typ at VCC = 3.0 V, a microamp-class supervisor.
    assert_eq!(m.params.get_f64("supply_static_ua"), Some(1.1));
    assert_eq!(m.params.get_str("supply_pin"), Some("6"));
    assert_eq!(m.params.get_str("gnd_pin"), Some("1"));
    // Table 2 abs max VCC.
    assert_eq!(m.ratings.max_voltage_v, Some(7.0));
}
