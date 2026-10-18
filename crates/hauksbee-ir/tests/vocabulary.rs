//! The shared vocabulary: every entry complete, wire words matching serde,
//! the documented verdict table, and the old spellings mapped exactly once.

use std::collections::HashSet;

use hauksbee_ir::evidence::EvidenceStatus;
use hauksbee_ir::vocabulary::{
    self, Check, EvidenceGrade, ExitClass, Severity, VerdictState, EXIT_CONTRACT,
};

/// The verdict table as the formal-use audit (F02, F03) specifies it.
#[test]
fn verdict_table_covers_every_state() {
    let rows = vocabulary::verdicts();
    let got: Vec<(&str, &str, Option<&str>, u8)> = rows
        .iter()
        .map(|r| (r.entry.id, r.entry.label, r.ci_alias, r.exit_code))
        .collect();
    assert_eq!(
        got,
        [
            ("pass", "pass", Some("GREEN"), 0),
            (
                "pass_conditional",
                "pass, conditionally",
                Some("GREEN, conditionally"),
                0
            ),
            ("fail", "fail", Some("RED"), 1),
            ("partly_judged", "partly judged, not a pass", None, 3),
            ("invalid_run", "inconclusive, not a pass", None, 3),
        ]
    );
    for state in VerdictState::ALL {
        assert_eq!(
            serde_json::to_value(state).unwrap(),
            state.as_str(),
            "{state:?}"
        );
        assert!(state.meaning().ends_with('.'), "{state:?}");
        assert_eq!(state.is_pass(), state.exit_class() == ExitClass::Pass);
    }
}

/// A state that is neither a pass nor a fail reads "<word>, not a pass": the
/// app shows the word as its headline and the rest as its qualifier, and the
/// CLI opens a partly judged verdict with the same word, so the two never say
/// different things (the app once said INVALID where the CLI said PARTLY
/// JUDGED). An inconclusive run's word is the CLI's closing tag.
#[test]
fn verdict_headline_words_are_the_cli_words() {
    let head = |s: VerdictState| {
        s.label()
            .strip_suffix(", not a pass")
            .unwrap_or_else(|| panic!("{s:?}: {}", s.label()))
    };
    for s in VerdictState::ALL {
        assert_eq!(s.label().ends_with(", not a pass"), !s.is_pass() && s != VerdictState::Fail, "{s:?}");
    }
    assert_eq!(
        vocabulary::PARTLY_JUDGED_LEAD,
        format!("{} (not a pass)", head(VerdictState::PartlyJudged).to_uppercase())
    );
    assert_eq!(head(VerdictState::InvalidRun), "inconclusive");
}

#[test]
fn exit_table_is_the_shared_contract() {
    let codes: Vec<(u8, &str)> = vocabulary::exit_codes()
        .iter()
        .map(|r| (r.code, r.entry.id))
        .collect();
    assert_eq!(
        codes,
        [
            (0, "pass"),
            (1, "fail"),
            (2, "usage"),
            (3, "invalid_run"),
            (4, "tool_fault")
        ]
    );
    assert_eq!(EXIT_CONTRACT, "hauksbee/1");
}

#[test]
fn v3_verdicts_map_onto_states() {
    use VerdictState as S;
    assert_eq!(S::from_v3("pass", false, false), Some(S::Pass));
    assert_eq!(S::from_v3("pass", false, true), Some(S::PassConditional));
    assert_eq!(S::from_v3("fail", true, true), Some(S::Fail));
    assert_eq!(S::from_v3("invalid", true, false), Some(S::PartlyJudged));
    assert_eq!(S::from_v3("invalid", false, false), Some(S::InvalidRun));
    assert_eq!(S::from_v3("clean", false, false), None);
}

#[test]
fn severity_scale_and_mappings() {
    let ids: Vec<&str> = vocabulary::severities()
        .iter()
        .map(|r| r.entry.id)
        .collect();
    assert_eq!(ids, ["serious", "warning", "note", "info"]);
    assert!(Severity::Serious > Severity::Warning && Severity::Note > Severity::Info);
    assert_eq!(Severity::from_legacy("High"), Some(Severity::Serious));
    assert_eq!(Severity::from_legacy("medium"), Some(Severity::Warning));
    assert_eq!(vocabulary::sarif_level(Severity::Warning, true), "error");
    assert_eq!(vocabulary::sarif_level(Severity::Info, false), "note");
    assert_eq!(
        vocabulary::kicad_severity(Severity::Serious, true, true),
        "exclusion"
    );
    assert_eq!(
        vocabulary::kicad_severity(Severity::Warning, false, false),
        "warning"
    );
    assert_eq!(vocabulary::BLOCKING.id, "blocking");
}

#[test]
fn evidence_grade_renames_clean() {
    assert_eq!(
        EvidenceGrade::from(EvidenceStatus::Clean),
        EvidenceGrade::Full
    );
    assert_eq!(serde_json::to_value(EvidenceGrade::Full).unwrap(), "full");
    // The grade no longer shares a word with any verdict.
    let verdict_words: HashSet<&str> = VerdictState::ALL
        .iter()
        .flat_map(|s| [s.as_str(), s.label()])
        .collect();
    for g in EvidenceGrade::ALL {
        assert!(!verdict_words.contains(g.as_str()), "{g:?}");
    }
}

/// Every entry is filled in, wire ids match serde, and no alias is claimed by
/// two entries of the same domain (a migrating renderer must map each old
/// spelling to exactly one new word).
#[test]
fn every_entry_is_complete_and_aliases_are_unambiguous() {
    let v = vocabulary::vocabulary();
    let domains: Vec<(&str, Vec<&vocabulary::Entry>)> = vec![
        ("verdicts", v.verdicts.iter().map(|e| &e.entry).collect()),
        ("evidence_grades", v.evidence_grades.iter().collect()),
        (
            "severities",
            v.severities.iter().map(|e| &e.entry).collect(),
        ),
        (
            "exit_codes",
            v.exit_codes.iter().map(|e| &e.entry).collect(),
        ),
        ("checks", v.checks.iter().collect()),
    ];
    for (domain, entries) in domains {
        let mut ids = HashSet::new();
        let mut aliases = HashSet::new();
        for e in entries {
            for text in [e.id, e.label, e.meaning] {
                assert!(!text.trim().is_empty(), "{domain}/{}", e.id);
                assert!(!text.contains("  "), "{domain}/{}: {text:?}", e.id);
            }
            assert!(ids.insert(e.id), "{domain}: duplicate id {}", e.id);
            for a in e.aliases {
                assert!(aliases.insert(*a), "{domain}: alias {a:?} claimed twice");
            }
        }
    }
    for c in Check::ALL {
        assert_eq!(serde_json::to_value(c).unwrap(), c.as_str());
    }
    for g in EvidenceGrade::ALL {
        assert_eq!(serde_json::to_value(g).unwrap(), g.as_str());
    }
    let json = serde_json::to_value(&v).unwrap();
    assert_eq!(json["schema"], "hauksbee.vocabulary/1");
    assert_eq!(json["verdicts"][3]["label"], "partly judged, not a pass");
    assert_eq!(
        json["glossary"].as_array().unwrap().len(),
        hauksbee_ir::glossary::all().len()
    );
    assert!(vocabulary::labels().contains(&"pass, conditionally"));
}
