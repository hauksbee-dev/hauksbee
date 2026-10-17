//! A CI run and `hauksbee check` judge the same board, so they must not
//! contradict each other: a run never reads GREEN over a board whose static
//! check fails on a gating finding, and both treat an unmodelled part by the
//! same rule (an open active IC or transistor refuses the verdict, an open
//! connector or passive is a stated condition).

use crate::support;
use hauksbee_ir::vocabulary::STATIC_CHECK_KIND;
use hauksbee_models::ModelLibrary;

fn fixture(name: &str) -> std::path::PathBuf {
    support::repo(&format!("crates/hauksbee-ci/tests/fixtures/{name}"))
}

/// A no-faults spec on `board`, with `supply` declared when given.
fn no_faults_spec(board: &std::path::Path, supply: Option<(&str, f64)>) -> String {
    let supply = supply.map_or_else(String::new, |(net, volts)| {
        format!("[[supply]]\nnet = \"{net}\"\nkind = \"bench\"\nvolts = {volts}\n")
    });
    format!(
        "name = \"agree\"\nboard = {}\nduration_ms = 2\n{supply}[[assert]]\nkind = \"no_faults\"\n",
        support::toml_path(board)
    )
}

/// `boot_gate` carries two real copper shorts, so `hauksbee check` fails it.
/// A spec that only asserts `no_faults` used to say nothing about them; the
/// run now fails on each, naming the short in every surface it writes.
#[test]
fn a_board_its_static_check_fails_never_runs_green() {
    let board = support::example("boards/boot_gate.kicad_pcb");
    let lib = ModelLibrary::builtin_with_user_dirs(&[]);
    let gating = hauksbee_engine::reports::check::static_gating_findings(&board, &lib)
        .expect("the static suite runs");
    assert!(
        !gating.is_empty(),
        "the fixture's check fails on its shorts"
    );

    let r = support::run_body("static fail.toml", &no_faults_spec(&board, None));
    let human = r.render_human();
    assert_ne!(
        r.exit_code(),
        0,
        "never GREEN over a failing check:\n{human}"
    );
    let rows: Vec<_> = r
        .results
        .iter()
        .filter(|row| row.kind == STATIC_CHECK_KIND)
        .collect();
    assert_eq!(
        rows.len(),
        gating.len(),
        "one row per gating finding:\n{human}"
    );
    for (row, finding) in rows.iter().zip(&gating) {
        assert!(
            !row.passed && !row.invalid && row.waived.is_none(),
            "{row:?}"
        );
        for subject in finding.refs.iter().chain(&finding.nets) {
            assert!(
                row.label.contains(subject.as_str()),
                "{} names {subject}",
                row.label
            );
        }
        assert!(
            human.contains(&row.label),
            "human report names it:\n{human}"
        );
    }
    let junit = r.render_junit();
    assert!(
        junit.contains(&format!("classname=\"{STATIC_CHECK_KIND}\"")) && junit.contains("<failure"),
        "{junit}"
    );
    let json: serde_json::Value = serde_json::from_str(&r.render_json()).unwrap();
    assert!(
        json["results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["kind"] == STATIC_CHECK_KIND && row["passed"] == false),
        "{json}"
    );
}

/// Legal control: a board the static check passes gets no static row, so its
/// run's verdict is exactly what its own assertions say.
#[test]
fn a_board_its_static_check_passes_adds_no_row() {
    let board = fixture("regulator.board");
    let lib = ModelLibrary::builtin_with_user_dirs(&[]);
    let gating = hauksbee_engine::reports::check::static_gating_findings(&board, &lib)
        .expect("the static suite runs");
    assert!(gating.is_empty(), "the control is check-clean: {gating:?}");
    let r = support::run_body(
        "static pass.toml",
        &no_faults_spec(&board, Some(("VIN", 5.0))),
    );
    assert!(
        r.results.iter().all(|row| row.kind != STATIC_CHECK_KIND),
        "{:?}",
        r.results
    );
}

/// The one shared rule for open parts: an open connector on the board leaves a
/// clean no-faults run GREEN with the part named as a condition, the way the
/// check states it; the same open part as an IC (designator U1) refuses it.
#[test]
fn an_open_connector_is_a_condition_and_an_open_ic_refuses_the_verdict() {
    let board = fixture("open_connector.board");
    let r = support::run_body(
        "open conn.toml",
        &no_faults_spec(&board, Some(("+5V", 5.0))),
    );
    let human = r.render_human();
    let row = support::row(&r, "kind:no_faults");
    assert!(
        row.passed && !row.invalid,
        "the open connector does not refuse it:\n{human}"
    );
    assert!(
        r.verdict_conditions.iter().any(|c| c.contains("J1")),
        "the open connector is a stated condition: {:?}\n{human}",
        r.verdict_conditions
    );

    let text = std::fs::read_to_string(&board).unwrap();
    let ic = std::env::temp_dir().join(format!("open_ic_{}.board", std::process::id()));
    std::fs::write(&ic, text.replace("comp J1 ", "comp U1 ")).unwrap();
    let r = support::run_body("open ic.toml", &no_faults_spec(&ic, Some(("+5V", 5.0))));
    let human = r.render_human();
    assert!(
        support::row(&r, "kind:no_faults").invalid,
        "an open IC refuses it:\n{human}"
    );
    assert_eq!(r.exit_code(), 3, "{human}");
}
