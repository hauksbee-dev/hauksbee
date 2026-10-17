//! An unmodelled part is an open circuit: a fair stand-in for a bystander, a false
//! one for the part that DRIVES the judged net. A rail whose regulator had no model
//! read `RED: VOUT settled 0.000 V`, and the regulator was never named. A powered
//! open part that can drive the net (directly, or through modelled series
//! conduction) makes a FAILURE there INVALID in the scheduler's one wording, since the
//! number is the board without it; an IN-BAND reading there is no more a pass than a
//! failure is a failure, and a part with nothing to drive WITH is only a caveat. The headless `hauksbee run` half
//! lives in `hauksbee-engine/tests/cli_strict_plain.rs`, against the same string.

use crate::support;
use hauksbee_ci::report::CiResult;
use hauksbee_engine::scheduler::open_driver_reason;
use std::fmt::Write as _;

/// Run `fixtures/open_parts.kicad_pcb` under voltage assertions `(net, min, max)`.
fn run(name: &str, asserts: &[(&str, f64, f64)]) -> CiResult {
    let board = support::repo_root().join("crates/hauksbee-ci/tests/fixtures/open_parts.kicad_pcb");
    let mut spec = format!(
        "name = \"open driver\"\nboard = {}\nduration_ms = 3\n",
        support::toml_path(&board)
    );
    for (i, (net, min, max)) in asserts.iter().enumerate() {
        let _ = writeln!(
            spec,
            "[[assert]]\nkind = \"voltage\"\nname = \"a{i}\"\nnet = \"{net}\"\nmin = {min}\n\
             max = {max}\nafter_ms = 1.0"
        );
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join(name);
    std::fs::write(&path, spec).expect("write spec");
    hauksbee_ci::run(&support::run_config(path)).expect("run the spec")
}

#[test]
fn an_open_converter_behind_an_inductor_refuses_the_rail_with_the_part_named() {
    // (a) The voltage assertion ALONE refuses, naming a part one inductor away.
    let r = run("alone.toml", &[("VOUT", 3.2, 3.4)]);
    let nets = ["SW".to_string(), "VOUT".to_string()];
    let reason = open_driver_reason("U1", "TPS54331D", "+12V", &nets);
    assert!(reason.contains("not a simulated result") && reason.contains("hauksbee models new"));
    let a = &r.results[0];
    assert!(a.invalid && !a.passed, "{a:?}");
    assert!(a.detail.contains(&reason), "{a:?}");
    assert!(r.coverage_warnings.contains(&reason));
    let json = serde_json::to_string(&r.json_report()).expect("json");
    assert!(json.contains(&reason), "{json}");
    let next = r.refusal().expect("an invalid run refuses").next_action;
    assert!(next.contains("hauksbee models new"), "{next}");
    // The evidence must agree with the verdict: U1 is on this assertion's
    // causal path, so its map is undermined, never merely qualified.
    let map = r
        .evidence
        .iter()
        .find(|m| m.assertion() == a.label)
        .expect("evidence for the refused assertion");
    assert!(map.is_undermined(), "{:?}", map.status());
    assert_eq!(r.exit_code(), 3);
}

#[test]
fn only_what_an_open_part_can_drive_is_refused() {
    let asserts = [
        ("VADJ", 4.8, 5.2), // (b) a modelled divider's 5.0 V, in band: U3 can still drive it
        ("VADJ", 8.0, 9.0), // ... and out of band: no failure without U3, so INVALID
        ("RAIL", 11.8, 12.0), // (c) U2 only loads RAIL: GREEN, with the caveat
        ("FLOAT", -0.1, 0.3), // (d) floating, no open part near: stays RED
        ("MID", 5.9, 6.1),  // (e) unrelated PASS (header J1 is a port) ...
        ("MID", 7.0, 8.0),  // ... and unrelated FAIL keep their verdicts
    ];
    let r = run("all.toml", &asserts);
    // The fixture is placement-only: its waived not-routed row stays listed
    // after the assertions' rows, and is not one of them.
    support::assert_placement_waived(&r);
    let flags: Vec<(bool, bool)> = r
        .results
        .iter()
        .filter(|a| a.kind != hauksbee_ir::vocabulary::STATIC_CHECK_KIND)
        .map(|a| (a.passed, a.invalid))
        .collect();
    let (pass, fail, invalid) = ((true, false), (false, false), (false, true));
    let unproven = (true, true); // in band, but the band is not evidence
    assert_eq!(
        flags,
        [unproven, invalid, pass, fail, pass, fail],
        "{:?}",
        r.results
    );
    // An IN-BAND reading on a net an open ACTIVE part can drive is no more a
    // pass than an out-of-band one is a failure: it names U3 and exits 3, with
    // nobody asking for coverage. U2, which can only LOAD RAIL, stays a caveat.
    let detail = |i: usize| r.results[i].detail.as_str();
    let caveat = "unmodelled part on the net drives or loads it";
    let named = |i: usize| detail(i).contains("U3 (LM337) has no model");
    assert!(
        !detail(0).contains(caveat) && detail(2).contains(caveat),
        "{}",
        detail(0)
    );
    assert!(named(0) && named(1) && r.exit_code() == 3, "{}", detail(0));
    assert!(detail(3).contains("floating"), "{}", detail(3));
    let blamed = |part: &str| r.coverage_warnings.iter().any(|w| w.contains(part));
    assert!(
        blamed("U3") && !blamed("U2") && !blamed("J1"),
        "{:?}",
        r.coverage_warnings
    );
}
