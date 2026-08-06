use super::*;

/// 2026-08-01, a fixed run date so no test depends on the wall clock.
fn today() -> RunDate {
    RunDate::from_epoch_days(20_666)
}

fn all_kinds() -> Vec<Assumption> {
    vec![
        Assumption::open_part(
            "R7",
            "10k",
            "no model matched it, and its pins sit on connected nets",
        ),
        Assumption::substitute_model(
            AssumptionSource::Scheduler,
            "U1",
            "ATmega328PB",
            "atmega328p",
        ),
        Assumption::inferred_pin_role("U2", "3", "output"),
        Assumption::default_parameter("U2", "vout", "3.3 V"),
        Assumption::fitted_by_default(
            AssumptionSource::Reader,
            Subject::new("odbpp", "the ODB++ archive"),
            part_scope("R7"),
        ),
        Assumption::not_checked(
            AssumptionSource::Reader,
            "drc",
            None,
            "this input class carries no copper geometry",
            "supply a layout so the check has copper to read",
        ),
        Assumption::not_exercised(
            AssumptionSource::Scheduler,
            Subject::new("i2c0", "the i2c0 bus"),
            Scope::Nets(NetScope::new(["SDA", "SCL"], None).unwrap()),
            "the MCU backend models no I2C controller on this platform",
            "run on a platform whose backend models the controller",
        ),
        Assumption::reduced_fidelity(
            AssumptionSource::Scheduler,
            Subject::new("spi0/framing", "SPI transaction framing on spi0"),
            Scope::Nets(NetScope::new(["SCK"], None).unwrap()),
            "the chunk-boundary heuristic",
            "expose the chip-select GPIO so framing reads real edges",
        ),
        Assumption::parser_limitation(
            AssumptionSource::Reader,
            Subject::new("drc/short", "shorts on this board"),
            Scope::Check {
                check: "drc".into(),
                kind: Some("short".into()),
            },
            "the file was written by a newer KiCad than this reader models",
            "re-export the board from the KiCad version this build supports",
        ),
        Assumption::waived(
            "si",
            "controlled_impedance",
            "DDR_CLK",
            "the fab confirmed the stackup by email",
            "2027-06-01",
            today(),
        )
        .unwrap(),
    ]
}

// ── ids and sentences ────────────────────────────────────────────────

#[test]
fn ids_are_deterministic_and_match_the_documented_shapes() {
    assert_eq!(
        Assumption::open_part("R7", "10k", "").id.as_str(),
        "open-part:R7"
    );
    assert_eq!(
        Assumption::not_checked(
            AssumptionSource::Reader,
            "drc",
            None,
            "no copper",
            "add a layout"
        )
        .id
        .as_str(),
        "not-checked:drc"
    );
    // A check that could not run ONE of its rules names the rule, so the
    // traversal can scope it to the assertions that rely on that rule.
    assert_eq!(
        Assumption::not_checked(
            AssumptionSource::Reader,
            "drc",
            Some("short"),
            "the reader models a different file version",
            "re-export the board"
        )
        .id
        .as_str(),
        "not-checked:drc/short"
    );
    assert_eq!(
        Assumption::not_exercised(
            AssumptionSource::Scheduler,
            Subject::same("i2c0/U4"),
            Scope::Board,
            "never addressed",
            "exercise it"
        )
        .id
        .as_str(),
        "not-exercised:i2c0/U4"
    );
    assert_eq!(
        Assumption::waived(
            "si",
            "controlled_impedance",
            "DDR_CLK",
            "why",
            "2027-06-01",
            today(),
        )
        .unwrap()
        .id
        .as_str(),
        "waived:si/controlled_impedance/DDR_CLK"
    );
    // Same inputs, same id, twice: what makes an acknowledgment file and a
    // cross-run diff able to name one assumption.
    assert_eq!(
        Assumption::open_part("R7", "10k", "no model matched").id,
        Assumption::open_part("R7", "10k", "no model matched").id
    );
    // Whitespace and colons fold so the kind slug is always everything
    // before the first colon.
    assert_eq!(
        AssumptionId::new(AssumptionKind::NotChecked, "spec check: rails").as_str(),
        "not-checked:spec%20check%3A%20rails"
    );
}

#[test]
fn every_constructor_composes_four_sentences_and_validates() {
    for a in all_kinds() {
        println!(
            "[{}] {}\n  because: {}\n  consequence: {}\n  fix: {}\n",
            a.id, a.statement, a.because, a.consequence, a.replacement
        );
        a.validate().unwrap_or_else(|e| panic!("{e}"));
        for (name, text) in [
            ("statement", &a.statement),
            ("because", &a.because),
            ("consequence", &a.consequence),
            ("replacement", &a.replacement),
        ] {
            assert!(
                text.ends_with('.'),
                "{}: {name} is not a sentence: {text:?}",
                a.id
            );
            assert!(
                !text.contains(".."),
                "{}: {name} double stop: {text:?}",
                a.id
            );
            // A sentence opens with a capital, unless it opens with a
            // case-sensitive identifier a producer handed over, which must
            // be spelled the way the board spells it.
            assert!(
                !text.starts_with(|c: char| c.is_ascii_lowercase())
                    || !text
                        .split_whitespace()
                        .next()
                        .unwrap()
                        .chars()
                        .all(|c| c.is_ascii_lowercase()),
                "{}: {name} does not open a sentence: {text:?}",
                a.id
            );
            assert!(
                !text.to_lowercase().contains("n/a"),
                "{}: {name} is a placeholder, not an answer",
                a.id
            );
        }
    }
}

#[test]
fn only_waived_carries_an_expiry() {
    for a in all_kinds() {
        match a.kind {
            AssumptionKind::Waived => assert_eq!(a.expires.as_deref(), Some("2027-06-01")),
            _ => assert!(a.expires.is_none(), "{} carries an expiry", a.id),
        }
    }
    // The two malformed shapes are construction errors, not warnings.
    let mut open = Assumption::open_part("R7", "10k", "");
    open.expires = Some("2027-06-01".into());
    assert!(open.validate().is_err());
    let mut waived = Assumption::waived("si", "k", "N1", "why", "2027-06-01", today()).unwrap();
    waived.expires = None;
    assert!(waived.validate().is_err());
}

#[test]
fn case_sensitive_subjects_are_not_recapitalized() {
    // A bus called spi0 is not called Spi0, and the Altium property is
    // `value_unresolved`, not `Value_unresolved`. Spelling the same subject
    // two ways is the exact drift this module exists to stop, so it must not
    // do it inside one assumption.
    let a = Assumption::reduced_fidelity(
        AssumptionSource::Scheduler,
        Subject::same("spi0 framing"),
        Scope::Board,
        "the chunk-boundary heuristic",
        "expose the chip-select GPIO",
    );
    assert!(a.statement.contains("spi0 framing"), "{}", a.statement);
    assert!(!a.statement.contains("Spi0"), "{}", a.statement);
    let b = Assumption::open_part("R7", "10k", "value_unresolved: no value in the source");
    assert!(b.because.starts_with("value_unresolved:"), "{}", b.because);
}

/// An id with no subject names nothing: an acknowledgment file could not name
/// it and a diff could not track it. No constructor can produce one, because
/// a board legitimately carries an unnamed net or a blank designator and a
/// gap on one of those is still a gap: it gets named as unnamed rather than
/// crashing a reader or shipping an unciteable id.
#[test]
fn no_constructor_can_produce_an_unciteable_id() {
    let from_nothing = [
        Assumption::open_part("", "", ""),
        Assumption::substitute_model(AssumptionSource::Binder, "", "", ""),
        Assumption::inferred_pin_role("", "", ""),
        Assumption::default_parameter("", "", ""),
        Assumption::not_checked(AssumptionSource::Reader, "", None, "", ""),
        Assumption::held_by_ideal_source(""),
        Assumption::fitted_by_default(AssumptionSource::Reader, Subject::same(""), Scope::Board),
        Assumption::not_exercised(
            AssumptionSource::Scheduler,
            Subject::same(""),
            Scope::Board,
            "",
            "",
        ),
        Assumption::waived("", "", "", "", "2027-06-01", today()).unwrap(),
    ];
    for a in from_nothing {
        a.validate().unwrap_or_else(|e| panic!("{e}"));
        let subject = a.id().as_str().split_once(':').unwrap().1;
        assert!(!subject.is_empty(), "{} names no subject", a.id());
    }
}

/// Missing DATA is a different thing from a producer bug, and must not be
/// either a panic or a hole. Reasons, bus names and part numbers are lifted
/// out of real files; real files sometimes do not carry them. Every sentence
/// still has to be a sentence, in release as much as in debug, because in
/// release `build`'s assertion is compiled out and nothing else is looking.
#[test]
fn a_missing_datum_falls_back_rather_than_leaving_a_hole() {
    let thin = [
        Assumption::open_part("R7", "", ""),
        Assumption::substitute_model(AssumptionSource::Binder, "U1", "", ""),
        Assumption::inferred_pin_role("U2", "", ""),
        Assumption::default_parameter("U2", "", ""),
        Assumption::fitted_by_default(
            AssumptionSource::Reader,
            Subject::new("odbpp", ""),
            Scope::Board,
        ),
        Assumption::not_checked(AssumptionSource::Reader, "drc", None, "", ""),
        Assumption::not_exercised(
            AssumptionSource::Scheduler,
            Subject::new("i2c0", ""),
            Scope::Board,
            "",
            "",
        ),
        Assumption::reduced_fidelity(
            AssumptionSource::Solver,
            Subject::new("spi0", ""),
            Scope::Board,
            "",
            "",
        ),
        Assumption::parser_limitation(
            AssumptionSource::Reader,
            Subject::new("drc/short", ""),
            Scope::Board,
            "",
            "",
        ),
        Assumption::waived("si", "k", "DDR_CLK", "", "2027-06-01", today()).unwrap(),
    ];
    for a in thin {
        a.validate().unwrap_or_else(|e| panic!("{e}"));
        for (name, text) in [
            ("statement", a.statement()),
            ("because", a.because()),
            ("consequence", a.consequence()),
            ("replacement", a.replacement()),
        ] {
            assert!(text.len() > 10, "{}: {name} is a stub: {text:?}", a.id());
        }
    }
    // And whitespace inside a datum is tidied rather than refused: a reason
    // lifted from a file arrives with whatever spacing the file had.
    let a = Assumption::open_part("R7", "10k", "no  model   matched");
    assert_eq!(a.because(), "No model matched.");
}

#[test]
fn validate_catches_a_mismatched_id_slug() {
    // The id and the kind must name the same thing, or the status rule and
    // the rendered id describe different gaps. Reachable only by hand here,
    // since the constructors compose the id from the kind.
    let mut a = Assumption::open_part("R7", "10k", "no model matched");
    a.id = AssumptionId(format!("{}:R7", AssumptionKind::ReducedFidelity.slug()));
    assert!(a.validate().is_err());
    // And an id naming no subject: an acknowledgment file could not name it
    // and a diff could not track it.
    let mut a = Assumption::open_part("R7", "10k", "no model matched");
    a.id = AssumptionId(format!("{}:", AssumptionKind::OpenPart.slug()));
    assert!(a.validate().is_err());
}

#[test]
fn an_absurd_expiry_is_refused_rather_than_overflowing() {
    // `until` is user input, and the civil-date arithmetic multiplies by
    // 146_097, so an absurd year has to be refused before it is computed.
    assert_eq!(parse_ymd_epoch_days("9223372036854775807-03-01"), None);
    assert_eq!(parse_ymd_epoch_days("99999999999999-12-31"), None);
    assert_eq!(parse_ymd_epoch_days("2026-02-30"), None);
    assert_eq!(parse_ymd_epoch_days("2026-13-01"), None);
    assert_eq!(parse_ymd_epoch_days("2027-06-01"), Some(20_970));
}

// ── the status rule table ────────────────────────────────────────────

/// Every declared kind has a constructor and a row in the test table, driven
/// off the enum itself so a new variant fails mechanically rather than
/// waiting for someone to remember.
#[test]
fn every_kind_has_a_constructor() {
    use strum::IntoEnumIterator;
    let built: Vec<AssumptionKind> = all_kinds().iter().map(|a| a.kind).collect();
    for kind in AssumptionKind::iter() {
        assert!(
            built.contains(&kind),
            "{kind:?} has no constructor exercised in all_kinds()"
        );
    }
    assert_eq!(built.len(), AssumptionKind::iter().count());
}

#[test]
fn duplicate_on_path_assumptions_are_listed_once() {
    // A traversal walking several nets that share a part hands the same
    // assumption over twice; rendering it twice is noise.
    let a = Assumption::open_part("R7", "10k", "no model matched");
    let map = EvidenceMap::new("A", &[a.clone(), a], today());
    assert_eq!(map.assumptions().len(), 1);
    assert_eq!(map.status(), EvidenceStatus::Undermined);
}

/// One row per line of the table in [`EvidenceMap::derive_status`], because
/// the table IS the policy: nothing else in the tree decides whether a
/// conclusion is entitled to a verdict.
#[test]
fn status_rule_covers_every_kind() {
    let expected = [
        // In `all_kinds()` order, which is the order the kinds are
        // declared, so a new variant with no row here fails the length
        // assertion below rather than passing silently.
        (AssumptionKind::OpenPart, EvidenceStatus::Undermined),
        (AssumptionKind::SubstituteModel, EvidenceStatus::Undermined),
        (AssumptionKind::InferredPinRole, EvidenceStatus::Qualified),
        (AssumptionKind::DefaultParameter, EvidenceStatus::Qualified),
        (AssumptionKind::FittedByDefault, EvidenceStatus::Undermined),
        (AssumptionKind::NotChecked, EvidenceStatus::Undermined),
        (AssumptionKind::NotExercised, EvidenceStatus::Undermined),
        (AssumptionKind::ReducedFidelity, EvidenceStatus::Qualified),
        (AssumptionKind::ParserLimitation, EvidenceStatus::Qualified),
        (AssumptionKind::Waived, EvidenceStatus::Qualified),
    ];
    let built = all_kinds();
    assert_eq!(built.len(), expected.len(), "a kind lost its constructor");
    for (a, (kind, want)) in built.iter().zip(expected) {
        assert_eq!(a.kind, kind, "constructor order drifted from the table");
        let map = EvidenceMap::new("A", std::slice::from_ref(a), today());
        assert_eq!(map.status(), want, "{} should be {want:?} on its own", a.id);
    }
}

#[test]
fn two_gaps_on_unnameable_subjects_stay_two_gaps() {
    // Two footprints with blank designators are two gaps. Giving them one id
    // would be worse than giving them an ugly one, because the evidence map
    // dedupes by id and the second gap would vanish from the report rather
    // than appear twice. The statement is the only thing that tells them
    // apart, so it disambiguates the id, deterministically.
    let a = Assumption::open_part("", "10k", "no model matched");
    let b = Assumption::open_part("", "47k", "no model matched");
    assert_ne!(a.id(), b.id(), "{} == {}", a.id(), b.id());
    assert!(a.id().as_str().starts_with("open-part:unnamed-"));
    // Deterministic: the same board yields the same id next run, which is
    // what makes an id citeable at all.
    assert_eq!(
        a.id(),
        Assumption::open_part("", "10k", "no model matched").id()
    );
    let map = EvidenceMap::new("A", &[a, b], today());
    assert_eq!(map.assumptions().len(), 2, "a real gap went missing");
    // And a NAMED subject never carries prose: an id is a contract, and
    // "open-part:an_unnamed_part" would be neither citeable nor unique.
    assert_eq!(
        Assumption::open_part("R7", "10k", "").id().as_str(),
        "open-part:R7"
    );
    // EVERY constructor, because one that substitutes prose or a bare
    // sentinel before the id is composed is one that collides two gaps onto
    // one entry, and the dedupe then eats one. That is the failure this whole
    // scheme exists to prevent, so the coverage is exhaustive rather than
    // representative.
    let nameless: Vec<(Assumption, Assumption)> = vec![
        (
            Assumption::open_part("", "10k", "no model matched"),
            Assumption::open_part("", "47k", "no model matched"),
        ),
        (
            Assumption::substitute_model(AssumptionSource::Binder, "", "a", "b"),
            Assumption::substitute_model(AssumptionSource::Binder, "", "c", "d"),
        ),
        (
            Assumption::inferred_pin_role("", "", "output"),
            Assumption::inferred_pin_role("", "", "input"),
        ),
        (
            Assumption::default_parameter("", "", "3.3 V"),
            Assumption::default_parameter("", "", "5 V"),
        ),
        (
            Assumption::fitted_by_default(
                AssumptionSource::Reader,
                Subject::same(""),
                Scope::Board,
            ),
            Assumption::fitted_by_default(
                AssumptionSource::Reader,
                Subject::new("", "the second archive"),
                Scope::Board,
            ),
        ),
        (
            Assumption::not_checked(AssumptionSource::Reader, "", None, "no copper", "add one"),
            Assumption::not_checked(
                AssumptionSource::Reader,
                "",
                None,
                "no firmware",
                "supply one",
            ),
        ),
        (
            Assumption::not_exercised(
                AssumptionSource::Scheduler,
                Subject::same(""),
                Scope::Board,
                "a",
                "b",
            ),
            Assumption::not_exercised(
                AssumptionSource::Scheduler,
                Subject::new("", "the second bus"),
                Scope::Board,
                "c",
                "d",
            ),
        ),
        (
            Assumption::reduced_fidelity(
                AssumptionSource::Solver,
                Subject::same(""),
                Scope::Board,
                "a",
                "b",
            ),
            Assumption::reduced_fidelity(
                AssumptionSource::Solver,
                Subject::new("", "the second span"),
                Scope::Board,
                "c",
                "d",
            ),
        ),
        (
            Assumption::held_by_ideal_source(""),
            Assumption::held_by_ideal_source("   "),
        ),
        (
            Assumption::parser_limitation(
                AssumptionSource::Reader,
                Subject::same(""),
                Scope::Board,
                "a",
                "b",
            ),
            Assumption::parser_limitation(
                AssumptionSource::Reader,
                Subject::new("", "the second finding"),
                Scope::Board,
                "c",
                "d",
            ),
        ),
        (
            Assumption::waived("", "", "", "why", "2027-06-01", today()).unwrap(),
            Assumption::waived("", "", "", "another why", "2027-06-01", today()).unwrap(),
        ),
    ];
    for (first, second) in nameless {
        let subject = first.id().as_str().split_once(':').unwrap().1;
        assert!(
            subject.starts_with("unnamed-"),
            "{} does not go through the disambiguator",
            first.id()
        );
        // Distinct statements, distinct ids, both surviving the map.
        if first.statement() != second.statement() {
            assert_ne!(
                first.id(),
                second.id(),
                "two gaps collided on {}",
                first.id()
            );
            let map = EvidenceMap::new("A", &[first, second], today());
            assert_eq!(map.assumptions().len(), 2, "a real gap went missing");
        }
    }
}

#[test]
fn a_waiver_with_an_invalid_expiry_is_refused() {
    assert!(matches!(
        Assumption::waived("si", "controlled_impedance", "DDR_CLK", "why", "", today(),),
        Err(EvidenceError::InvalidDate { .. })
    ));
}

#[test]
fn a_scope_window_that_is_not_numbers_is_refused() {
    assert!(matches!(
        TimeWindow::new(0.0, f64::NAN),
        Err(EvidenceError::NonFinite { .. })
    ));
    let ok = Assumption::not_exercised(
        AssumptionSource::Solver,
        Subject::same("the settling window"),
        Scope::Nets(NetScope::new(["3V3"], Some(TimeWindow::new(0.0, 0.5).unwrap())).unwrap()),
        "the solve never reached it",
        "extend the run",
    );
    assert!(ok.validate().is_ok());
}

#[test]
fn the_wire_vocabulary_is_spelled_in_exactly_one_place() {
    // One surface downstream hand-serializes its JSON instead of going
    // through serde, so the strings have to be reachable as strings, and they
    // have to be the same strings serde writes.
    for status in [
        EvidenceStatus::Clean,
        EvidenceStatus::Qualified,
        EvidenceStatus::Undermined,
    ] {
        assert_eq!(
            serde_json::to_value(status).unwrap(),
            serde_json::Value::String(status.as_str().to_string())
        );
        assert_eq!(status.to_string(), status.as_str());
    }
    use strum::IntoEnumIterator;
    for kind in AssumptionKind::iter() {
        assert_eq!(
            serde_json::to_value(kind).unwrap(),
            serde_json::Value::String(kind.as_str().to_string())
        );
    }
}

#[test]
fn no_assumptions_is_clean() {
    let map = EvidenceMap::new("A", &[], today());
    assert_eq!(map.status(), EvidenceStatus::Clean);
    assert!(map.assumptions().is_empty());
    assert!(!map.is_undermined());
}

#[test]
fn one_undermining_assumption_beats_any_number_of_qualifiers() {
    let mut set: Vec<Assumption> = all_kinds()
        .into_iter()
        .filter(|a| {
            EvidenceMap::derive_status(std::slice::from_ref(a), today())
                == EvidenceStatus::Qualified
        })
        .collect();
    assert_eq!(
        EvidenceMap::derive_status(&set, today()),
        EvidenceStatus::Qualified
    );
    set.push(Assumption::open_part("R7", "10k", ""));
    assert_eq!(
        EvidenceMap::derive_status(&set, today()),
        EvidenceStatus::Undermined,
        "a pile of caveats must not dilute one undermining assumption"
    );
}

#[test]
fn a_lapsed_waiver_stops_covering() {
    let w =
        Assumption::waived("si", "k", "DDR_CLK", "fab confirmed", "2027-06-01", today()).unwrap();
    let expiry = parse_ymd_epoch_days("2027-06-01").unwrap();
    // In force up to and including the expiry date (end-of-day expiry, the
    // reading the waiver gate already uses).
    assert_eq!(
        EvidenceMap::derive_status(std::slice::from_ref(&w), RunDate::from_epoch_days(expiry)),
        EvidenceStatus::Qualified
    );
    assert_eq!(
        EvidenceMap::derive_status(
            std::slice::from_ref(&w),
            RunDate::from_epoch_days(expiry + 1)
        ),
        EvidenceStatus::Undermined
    );
    // An unreadable or absent expiry fails CLOSED: a date that cannot be
    // read cannot vouch for a finding.
    let mut broken = w.clone();
    broken.expires = Some("next Friday".into());
    assert_eq!(
        EvidenceMap::derive_status(std::slice::from_ref(&broken), today()),
        EvidenceStatus::Undermined
    );
    broken.expires = None;
    assert_eq!(
        EvidenceMap::derive_status(std::slice::from_ref(&broken), today()),
        EvidenceStatus::Undermined
    );
    // A caller with no date at all gets the same fail-closed reading.
    assert_eq!(
        EvidenceMap::derive_status(std::slice::from_ref(&w), RunDate::unknown()),
        EvidenceStatus::Undermined
    );
}

#[test]
fn a_clock_reading_from_before_this_build_is_not_believed() {
    // The asymmetry that makes RunDate a type: a date read LATE only expires
    // waivers early, but a date read early re-arms every lapsed one, which is
    // the direction expiry must never fail in. A zero-initialised field, a
    // container with no RTC, a dead clock battery: a bare number makes all of
    // them look like a valid Thursday in 1970.
    let lapsed =
        Assumption::waived("si", "k", "DDR_CLK", "fab confirmed", "2001-01-01", today()).unwrap();
    for broken in [0, 1, RunDate::EARLIEST_CREDIBLE_DAY - 1, i64::MIN] {
        assert_eq!(
            RunDate::from_epoch_days(broken).epoch_days(),
            None,
            "{broken} is not a credible run date"
        );
        assert_eq!(
            EvidenceMap::derive_status(
                std::slice::from_ref(&lapsed),
                RunDate::from_epoch_days(broken)
            ),
            EvidenceStatus::Undermined,
            "a broken clock must not resurrect a waiver that lapsed in 2001"
        );
    }
    // A credible reading is believed, and the floor itself is credible.
    assert_eq!(
        RunDate::from_epoch_days(RunDate::EARLIEST_CREDIBLE_DAY).epoch_days(),
        Some(RunDate::EARLIEST_CREDIBLE_DAY)
    );
    assert_eq!(
        parse_ymd_epoch_days("2026-07-29"),
        Some(RunDate::EARLIEST_CREDIBLE_DAY),
        "the floor is the date its doc comment claims"
    );
}

// ── forgery ─────────────────────────────────────────────────────────

/// A judgement is produced, never parsed. `Assumption` and `EvidenceMap` are
/// serialize-only for exactly this reason: a `Deserialize` on either is a
/// minting route that needs no constructor. Eight lines of JSON would
/// otherwise buy an assumption with any kind and any wording, and a map with
/// its `assumptions` key deleted would read back `Clean` over a real gap,
/// which is the hiding the whole spine exists to prevent.
///
/// This test is the guard on that decision. `serde_json::from_value` for
/// either type does not compile today, and if someone adds the derive to make
/// a consumer's life easier, this stops passing.
#[test]
fn the_judgement_types_are_serialize_only() {
    // Provenance serializes for reports but invariant-bearing references cannot
    // be minted by deserializing arbitrary JSON.
    let parameter = ParameterProvenance {
        parameter: "R7.resistance".into(),
        value: "10k".into(),
        origin: ValueOrigin::Artifact {
            index: ArtifactId(0),
            field: "value".into(),
        },
    };
    assert!(serde_json::to_value(parameter).is_ok());
    // The judgements do not. Written as a source-text assertion because a
    // negative trait bound is not expressible: the derives are in this file,
    // so the file can check itself.
    let src = include_str!("../evidence.rs");
    for judgement in ["pub struct Assumption {", "pub struct EvidenceMap {"] {
        let at = src.find(judgement).expect("the type is declared here");
        // The whole attribute block, however long it grows: everything from
        // the last blank line before the declaration. A fixed byte window
        // would slide off the `#[derive(...)]` line the first time someone
        // lengthens a doc comment, and the guard would go quiet in the same
        // edit that made it necessary.
        let block: String = src[..at]
            .rsplit_once("\n\n")
            .map(|(_, tail)| tail)
            .unwrap_or(&src[..at])
            .lines()
            // Attribute lines only: the doc comment above these types
            // discusses `Deserialize` in prose, and a guard that reads the
            // prose is a guard that fires on its own explanation.
            .filter(|l| l.trim_start().starts_with("#["))
            .collect();
        assert!(
            block.contains("#[derive("),
            "{judgement}: the attribute block was not found, so this guard is not \
                 guarding anything"
        );
        assert!(
            !block.contains("Deserialize"),
            "{judgement} gained a Deserialize derive, which is a minting route"
        );
    }
    // A hand-written impl is the other way someone would satisfy a
    // consumer's inconvenience, and it would not touch the derive line. The
    // needle is assembled at runtime so this assertion does not trip over
    // its own source text.
    for ty in ["Assumption", "EvidenceMap"] {
        let needle = format!("Deserialize<'de> for {ty}");
        assert!(
            !src.contains(&needle),
            "a hand-written `impl {needle}` is the same minting route"
        );
    }
}

#[test]
fn a_consumer_settles_a_status_by_re_deriving_it_from_the_registry() {
    // The only honest way to check a status: hand the kinds back to the rule.
    // A reader with the run's registry can always do this; a reader with a
    // map alone is holding the producer's word, which is why the map is not
    // parseable on its own.
    let registry = [Assumption::open_part("U2", "XC6206", "no model matched")];
    let map = EvidenceMap::new("3V3 stays above 3.1 V", &registry, today());
    let on_path: Vec<Assumption> = registry
        .iter()
        .filter(|a| map.assumptions().contains(a.id()))
        .cloned()
        .collect();
    assert_eq!(
        EvidenceMap::derive_status(&on_path, today()),
        map.status(),
        "re-deriving from the registry must reproduce the recorded status"
    );
}

// ── serde shape ─────────────────────────────────────────────────────

#[test]
fn assumption_json_shape_is_the_published_one() {
    let a = Assumption::open_part("R7", "10k", "no model matched");
    let v = serde_json::to_value(&a).unwrap();
    assert_eq!(v["id"], "open-part:R7");
    assert_eq!(v["kind"], "open_part");
    assert_eq!(v["source"], "binder");
    assert_eq!(v["scope"]["type"], "subjects");
    assert_eq!(v["scope"]["value"][0]["kind"], "part");
    assert_eq!(v["scope"]["value"][0]["id"], "R7");
    // No expiry on a run-derived assumption, and the field is absent
    // rather than null: the common shape stays small.
    assert!(v.get("expires").is_none());
    assert_eq!(
        v.as_object().unwrap().keys().count(),
        8,
        "the assumption shape gained or lost a field; the report schema \
             bumps exactly once, in the rendering phase"
    );
}

#[test]
fn evidence_map_json_shape_is_the_published_one() {
    let open = Assumption::open_part("U2", "XC6206", "no model matched");
    let mut registry = EvidenceRegistry::new(vec![open.clone()]).unwrap();
    let artifacts: Vec<ArtifactId> = (0..3)
        .map(|index| {
            registry
                .add_artifact(
                    ArtifactProvenance::new(
                        format!("board-{index}.kicad_pcb"),
                        ArtifactKind::KiCadPcb,
                        ArtifactRole::Layout,
                        String::new(),
                        Vec::new(),
                    )
                    .unwrap(),
                )
                .unwrap()
        })
        .collect();
    let map = EvidenceMap::new("3V3 stays above 3.1 V", &[open], today())
        .with_artifacts(&registry, [artifacts[0], artifacts[2]])
        .unwrap()
        .with_models(vec![ModelOnPath::new(
            "U2",
            "xc6206",
            ModelLayer::Pack,
            MatchConfidence::High,
        )
        .unwrap()])
        .with_error_budget(ErrorBudget {
            methods: vec![WindowMethod {
                window: TimeWindow {
                    start_s: 0.0,
                    end_s: 0.05,
                },
                method: IntegrationMethod::Trapezoidal,
                accuracy_cost: 0.0,
            }],
            ..ErrorBudget::new(IntegrationTolerance {
                reltol: 1e-3,
                abstol: 1e-12,
                chgtol: 1e-14,
            })
        });
    let v = serde_json::to_value(&map).unwrap();
    assert_eq!(v["assertion"], "3V3 stays above 3.1 V");
    assert_eq!(v["artifacts"][1], 2);
    assert_eq!(v["assumptions"][0], "open-part:U2");
    assert_eq!(v["status"], "undermined");
    assert_eq!(v["error_budget"]["tolerance"]["reltol"], 1e-3);
    assert_eq!(v["error_budget"]["methods"][0]["method"], "trapezoidal");
    assert!(v["error_budget"].get("residual").is_none());
    assert!(v.get("parameters").is_none());
    assert!(v.get("coverage").is_none());
}

#[test]
fn provenance_and_origin_shapes() {
    let p = ParameterProvenance {
        parameter: "U2.vout".into(),
        value: "3.3 V".into(),
        origin: ValueOrigin::Model {
            model_id: "xc6206".into(),
            layer: ModelLayer::Pack,
            confidence: MatchConfidence::Exact,
        },
    };
    let v = serde_json::to_value(&p).unwrap();
    assert_eq!(v["origin"]["type"], "model");
    assert_eq!(v["origin"]["layer"], "pack");
    let art = ArtifactProvenance {
        path: "boards/blinky.kicad_pcb".into(),
        kind: ArtifactKind::KiCadPcb,
        role: ArtifactRole::Layout,
        sha256: String::new(),
        contributed: vec![Contribution {
            what: "connectivity".into(),
            detail: "nets read from the file's net table".into(),
        }],
        ignored: vec![IgnoredInput {
            what: "F.SilkS".into(),
            why: "board artwork rather than parts".into(),
        }],
        cross_checks: vec![CrossCheck {
            what: "netlist against copper".into(),
            agreed: true,
            detail: "both reported 41 nets".into(),
        }],
        assumptions: vec![AssumptionId::new(
            AssumptionKind::FittedByDefault,
            "kicad_pcb",
        )],
    };
    let v = serde_json::to_value(&art).unwrap();
    assert_eq!(v["role"], "layout");
    assert!(v.get("sha256").is_none(), "an empty hash is omitted");
    assert_eq!(v["assumptions"][0], "fitted-by-default:kicad_pcb");
}

#[test]
fn schemas_generate_with_the_status_vocabulary() {
    let schema = schemars::schema_for!(EvidenceMap);
    let text = serde_json::to_string(&schema).unwrap();
    for want in ["clean", "qualified", "undermined", "assumptions", "status"] {
        assert!(text.contains(want), "schema is missing {want}");
    }
    // Every type that reaches a JSON surface must have a schema, because
    // the report schema has a drift test.
    serde_json::to_string(&schemars::schema_for!(Assumption)).unwrap();
    serde_json::to_string(&schemars::schema_for!(ArtifactProvenance)).unwrap();
    serde_json::to_string(&schemars::schema_for!(ParameterProvenance)).unwrap();
    serde_json::to_string(&schemars::schema_for!(ErrorBudget)).unwrap();
}

#[test]
fn the_published_schema_does_not_permit_a_null_the_writers_never_write() {
    // An optional field here is an ABSENT KEY, never `null`: the whole
    // non-finite discipline exists because a `null` cannot be read back
    // against a `number`. A schema that permits a third encoding is a schema
    // this module's own writers would fail, and narrowing it after
    // publication breaks every validating consumer, so it is pinned now.
    for (name, schema) in [
        ("EvidenceMap", schemars::schema_for!(EvidenceMap)),
        ("ErrorBudget", schemars::schema_for!(ErrorBudget)),
        ("Assumption", schemars::schema_for!(Assumption)),
        ("Scope", schemars::schema_for!(Scope)),
    ] {
        let text = serde_json::to_string(&schema).unwrap();
        assert!(
            !text.contains("\"null\""),
            "{name}'s schema permits null: {text}"
        );
    }
    // Optional fields stay optional, though: absence is the encoding.
    let map = serde_json::to_value(schemars::schema_for!(EvidenceMap)).unwrap();
    let required: Vec<&str> = map["required"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(required, ["assertion", "status"]);
}

// ── the discrimination fixture ──────────────────────────────────────

/// THE CONTRACT FOR THE CAUSAL-PATH TRAVERSAL, written before any
/// traversal or renderer exists.
///
/// The failure mode this exists to catch is the evidence map degenerating,
/// in either of two mirror-image ways, both of which pass every
/// type-level test in this file while reopening the honesty gap the spine
/// was built to close:
///
/// - **Saturated.** Every assumption gets attached to every assertion,
///   because "attach the whole registry" is the path of least resistance
///   once recording real net-part incidence gets awkward. Every assertion
///   then renders undermined, users learn to scroll past the block, and
///   the signal dies in noise.
/// - **Vacuous.** The traversal quietly returns nothing, every assertion
///   renders `Clean`, and the new vocabulary certifies the very silence it
///   was built to end, now with more authority.
///
/// The incidence below is built BY HAND, and `on_path_for` is a stand-in for
/// a traversal that does not exist yet, because the real one needs the
/// binder's net-part incidence and the binder lives in another crate that
/// this one cannot depend on. So this test cannot itself fail when the real
/// traversal degenerates. What it is instead is the SPECIFICATION of the test
/// that must exist beside that traversal, in the engine, asserting the same
/// two halves against real incidence: an unresolved part on assertion A's net
/// undermines A and names the part, and assertion B on an unreachable net
/// stays clean and empty. A vacuous traversal fails the first half; a
/// saturated one fails the second. Whoever writes the traversal owes that
/// test, and this one says exactly what it has to assert.
#[test]
fn an_unresolved_part_undermines_its_own_net_and_only_its_own_net() {
    // The fixture board, as the binder will eventually report it: net ->
    // the parts incident on it. X is the regulator feeding 3V3 and did not
    // resolve; the parts on VBUS are all bound.
    let incidence: &[(&str, &[&str])] =
        &[("3V3", &["X", "C1", "U5"]), ("VBUS", &["J1", "C9", "D2"])];
    let registry = vec![Assumption::open_part("X", "XC6206", "no model matched")];

    // Stand-in for the phase-4 traversal: an assumption is on-path when its
    // scope names a part incident on the assertion's subject nets.
    fn on_path_for(
        subject_nets: &[&str],
        incidence: &[(&str, &[&str])],
        registry: &[Assumption],
    ) -> Vec<Assumption> {
        let reachable: Vec<&str> = incidence
            .iter()
            .filter(|(net, _)| subject_nets.contains(net))
            .flat_map(|(_, refs)| refs.iter().copied())
            .collect();
        registry
            .iter()
            .filter(|a| match &a.scope {
                Scope::Subjects(subjects) => subjects.as_slice().iter().any(|entity| {
                    entity.kind() == EntityKind::Part && reachable.contains(&entity.id())
                }),
                Scope::Parameter(parameter) => {
                    parameter.subject().kind() == EntityKind::Part
                        && reachable.contains(&parameter.subject().id())
                }
                Scope::Nets(nets) => nets
                    .nets()
                    .iter()
                    .any(|n| subject_nets.contains(&n.as_str())),
                // A board-wide gap is on every assertion's path by
                // definition, which is why a constructor that hardcodes
                // Scope::Board for an undermining kind saturates a run.
                Scope::Board => true,
                // Named explicitly rather than swept into a `_` arm: these
                // two are NOT membership-by-electrical-reachability, and the
                // real traversal owes each its own rule and its own test. A
                // NotChecked assumption is on-path for every assertion that
                // relies on that check (§2.4's "covering this assertion's
                // check"), and a TimeWindow one for every assertion whose
                // observation window overlaps it. Dropping them here, in the
                // one worked example, is how they would go missing there:
                // silent, and for the kinds where silence is hardest to
                // notice.
                Scope::Check { .. } => false,
            })
            .cloned()
            .collect()
    }

    let a = EvidenceMap::new(
        "3V3 stays above 3.1 V",
        &on_path_for(&["3V3"], incidence, &registry),
        today(),
    );
    let b = EvidenceMap::new(
        "VBUS stays below 5.5 V",
        &on_path_for(&["VBUS"], incidence, &registry),
        today(),
    );

    // Half one: the gap cannot hide behind a board-wide percentage. A is
    // undermined, and it NAMES the part, so the report can say which.
    assert_eq!(a.status(), EvidenceStatus::Undermined);
    assert!(a.is_undermined());
    assert_eq!(a.assumptions(), [AssumptionId("open-part:X".into())]);

    // Half two: and it does not smear over the rest of the board.
    assert_eq!(b.status(), EvidenceStatus::Clean);
    assert!(b.assumptions().is_empty());

    // Half three, the one a caller controls rather than the traversal: a
    // board-scoped gap of an undermining kind is on every assertion's path by
    // definition, so scoping one that way makes a whole run invalid. That is
    // a real answer for a board with no BOM at all, and the wrong answer for
    // a reader that knows which parts are in question. Pinned here because it
    // is the saturated mode arriving as a scope choice rather than as a
    // traversal bug.
    let board_wide = vec![Assumption::fitted_by_default(
        AssumptionSource::Reader,
        Subject::new("odbpp", "the ODB++ archive"),
        Scope::Board,
    )];
    for nets in [&["3V3"], &["VBUS"]] {
        let map = EvidenceMap::new(
            "any assertion",
            &on_path_for(nets, incidence, &board_wide),
            today(),
        );
        assert_eq!(map.status(), EvidenceStatus::Undermined);
        assert_eq!(
            map.assumptions(),
            [AssumptionId("fitted-by-default:odbpp".into())]
        );
    }
    // Scoped to the parts actually in question, it touches only those.
    let scoped = vec![Assumption::fitted_by_default(
        AssumptionSource::Reader,
        Subject::new("odbpp", "the ODB++ archive"),
        part_scope("C1"),
    )];
    assert_eq!(
        EvidenceMap::new("A", &on_path_for(&["3V3"], incidence, &scoped), today()).status(),
        EvidenceStatus::Undermined
    );
    assert_eq!(
        EvidenceMap::new("B", &on_path_for(&["VBUS"], incidence, &scoped), today()).status(),
        EvidenceStatus::Clean
    );
}

/// The two degenerate traversals, each shown FAILING the discrimination
/// test's assertions, so that test is demonstrably load-bearing rather than
/// merely present.
///
/// This is the part a fixture of this shape usually skips: asserting that a
/// correct traversal passes says nothing about whether the assertions would
/// catch a wrong one. Here a saturating traversal (everything on every path)
/// and a vacuous one (nothing on any path) are both run against the same two
/// halves, and each fails its own half.
#[test]
fn a_saturating_traversal_and_a_vacuous_one_each_fail_a_half() {
    let registry = vec![Assumption::open_part("X", "XC6206", "no model matched")];

    // Saturating: attach the whole registry to every assertion. Half one
    // (A undermined, naming X) passes, and half two (B clean and empty)
    // fails, which is the failure that keeps the spine meaningful rather
    // than merely loud.
    let a = EvidenceMap::new("3V3 stays above 3.1 V", &registry, today());
    let b = EvidenceMap::new("VBUS stays below 5.5 V", &registry, today());
    assert_eq!(a.status(), EvidenceStatus::Undermined);
    assert_eq!(a.assumptions(), [AssumptionId("open-part:X".into())]);
    assert_ne!(
        b.status(),
        EvidenceStatus::Clean,
        "a saturating traversal must fail the discrimination test's second half"
    );
    assert!(!b.assumptions().is_empty());

    // Vacuous: attach nothing to anything. Half two passes, half one fails,
    // and the failure is the new vocabulary certifying the silence it was
    // built to end.
    let a = EvidenceMap::new("3V3 stays above 3.1 V", &[], today());
    let b = EvidenceMap::new("VBUS stays below 5.5 V", &[], today());
    assert_ne!(
        a.status(),
        EvidenceStatus::Undermined,
        "a vacuous traversal must fail the discrimination test's first half"
    );
    assert!(a.assumptions().is_empty());
    assert_eq!(b.status(), EvidenceStatus::Clean);
}

#[test]
fn the_ideal_source_wording_is_composed_here_too() {
    // `held_by_ideal_source` is the second `ReducedFidelity` constructor, so
    // the kind table above exercises the generic one and this covers the
    // named one. Its wording is load-bearing: it is the difference between
    // "this rail check passed" and "this rail check could not have failed".
    let a = Assumption::held_by_ideal_source("3V3");
    a.validate().unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(a.id().as_str(), "reduced-fidelity:3V3");
    assert_eq!(a.statement(), "Net 3V3 is held by an ideal source.");
    assert!(a.consequence().contains("vouches for nothing"));
    assert_eq!(a.scope(), &net_scope("3V3"));
    assert_eq!(
        EvidenceMap::derive_status(std::slice::from_ref(&a), today()),
        EvidenceStatus::Qualified
    );
}
