//! A simple voltage regulator is stamped as an IDEAL DC source that ignores its
//! input, so its setpoint is a RESULT only when the input has independent
//! headroom. On the AMS1117-3.3 diagnostic board at VIN = 2.5 V, half a volt
//! BELOW the part's own output band, every surface said `[PASS] VOUT after 1ms:
//! min=3.300V (>= 3.2V) ... evidence: clean`, exit 0: a board that cannot
//! regulate, certified as one that does. 4.4 V is the discriminating row,
//! clearing 3.3 V but not the 3.399 V endpoint plus 1.1 V of dropout.
//!
//! What the fix owes, across every surface at once: the run REFUSES (INVALID,
//! exit 3) instead of passing or failing; the refusal is ONE string, byte-identical
//! in the coverage warnings, in each assertion detail and in the headless `run`
//! notes; and a genuine failure on an untainted net stays a failure (FAIL > INVALID).

mod support;

use hauksbee_ci::report::CiResult;
use hauksbee_ci::runner::RunOutcome;
use hauksbee_ci::RunConfig;
use hauksbee_engine::scheduler::RegulationTaint;
use std::path::{Path, PathBuf};

fn fixture_board() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/regulator.board")
}

/// A spec whose only variables are the bench supply on VIN and an optional `vin
/// high` floor. `volts = None` declares no supply at all, the condition the headless
/// `hauksbee run` reaches on its own, so the two surfaces are comparable word for
/// word. The floor is a GENUINE red on a net OUTSIDE the taint (VIN is a supply
/// rail, where the taint search stops).
fn spec_for(dir: &Path, volts: Option<f64>, floor: Option<f64>) -> PathBuf {
    let spec = dir.join(format!("reg-{volts:?}-{floor:?}.toml"));
    let supply = volts.map_or(String::new(), |v| {
        format!("[[supply]]\nnet = \"VIN\"\nkind = \"bench\"\nvolts = {v}\ncurrent_limit_a = 0.1\nr_out_ohms = 0.1\n")
    });
    let red = floor.map_or(String::new(), |v| {
        format!("[[assert]]\nkind = \"voltage\"\nname = \"vin high\"\nnet = \"VIN\"\nmin = {v}\n")
    });
    let body = format!(
        "name = \"regulator validity\"\nboard = {}\nduration_ms = 3\n{supply}\
         [[assert]]\nkind = \"voltage\"\nname = \"vout regulates\"\nnet = \"VOUT\"\n\
         min = 3.2\nmax = 3.4\nafter_ms = 1.0\n\
         [[assert]]\nkind = \"no_faults\"\nname = \"no faults\"\n{red}",
        support::toml_path(&fixture_board())
    );
    std::fs::write(&spec, body).expect("write spec");
    spec
}

fn run(spec: &Path) -> CiResult {
    hauksbee_ci::run(&RunConfig {
        spec: spec.to_path_buf(),
        ..RunConfig::default()
    })
    .expect("run the spec")
}

/// The one refusal string, as the coverage-warning channel carries it.
fn reason(r: &CiResult) -> String {
    r.coverage_warnings
        .iter()
        .find(|w| w.contains("regulation of"))
        .unwrap_or_else(|| panic!("no regulation warning in {:?}", r.coverage_warnings))
        .clone()
}

fn result<'a>(r: &'a CiResult, name: &str) -> &'a hauksbee_ci::assertions::AssertResult {
    r.results
        .iter()
        .find(|x| x.label == name)
        .unwrap_or_else(|| panic!("no assertion '{name}' in {:?}", r.results))
}

#[test]
fn regulator_headroom_decides_whether_the_setpoint_is_a_result() {
    let dir = tempfile::tempdir().expect("tempdir");
    // 5.0 V clears 3.399 + 1.1; 4.4 V clears 3.3 but NOT the real endpoint;
    // 2.5 V is below the output itself.
    for (volts, supported) in [(5.0, true), (4.4, false), (2.5, false)] {
        let r = run(&spec_for(
            dir.path(),
            Some(volts),
            (!supported).then_some(volts + 1.0),
        ));
        let vout = result(&r, "vout regulates");
        if supported {
            assert_eq!(r.exit_code(), 0, "{volts} V: {:?}", r.results);
            assert!(vout.passed && !vout.invalid, "{volts} V: {vout:?}");
            // Supported is not CLEAN: an ideal source held at its setpoint,
            // with only a typical-dropout headroom check, is QUALIFIED.
            let map = r
                .evidence
                .iter()
                .find(|m| m.assertion() == "vout regulates")
                .expect("evidence for the VOUT assertion");
            assert_eq!(format!("{:?}", map.status()).to_lowercase(), "qualified");
            continue;
        }

        let reason = reason(&r);
        assert!(reason.contains("U1") && reason.contains("VOUT"), "{reason}");

        for name in ["vout regulates", "no faults"] {
            let a = result(&r, name);
            assert!(a.invalid, "{volts} V: {name} should be INVALID: {a:?}");
            // Byte-identical, not merely similar: one wording per gap.
            assert!(
                a.detail.contains(&reason),
                "{volts} V: {name} detail does not carry the shared reason\n  detail: {}\n  reason: {reason}",
                a.detail
            );
        }
        // FAIL > INVALID: VIN is the supply rail the taint search stops at, so
        // a real failure there survives the refusal beside it.
        let vin = result(&r, "vin high");
        assert!(!vin.passed && !vin.invalid, "{volts} V: {vin:?}");

        let next = r.refusal().expect("an invalid run refuses").next_action;
        assert!(next.contains("regulator's input net"), "{volts} V: {next}");
        assert!(!r.json_report().run_valid, "{volts} V: run_valid");
        assert_eq!(r.exit_code(), 3, "{volts} V");
    }
}

#[test]
fn a_tainted_member_never_outranks_a_real_failure_in_an_ensemble() {
    // Member 0 is tainted, so its no_faults claim cannot be judged. Member 1 is
    // trustworthy and DID fault. The ensemble verdict is that failure.
    let spec: hauksbee_ci::spec::Spec = toml::from_str(
        "name = \"e\"\nboard = \"b\"\n[[assert]]\nkind = \"no_faults\"\nname = \"no faults\"\n",
    )
    .expect("spec");
    let taint = RegulationTaint {
        reason: "U1 regulation of VOUT is unsupported: ...".into(),
        nets: vec!["VOUT".into()],
        refs: vec!["U1".into()],
        since_s: 0.0,
    };
    let outcomes = [
        RunOutcome {
            seed: 0,
            regulation: vec![taint],
            ..RunOutcome::default()
        },
        RunOutcome {
            seed: 1,
            faults: vec![hauksbee_ci::runner::RunFault {
                component: "R1".into(),
                kind: "overpower".into(),
                value: 0.3,
                limit: 0.1,
                t_ms: 0.4,
            }],
            ..RunOutcome::default()
        },
    ];
    let results = hauksbee_ci::assertions::evaluate(&spec, &outcomes);
    let a = &results[0];
    assert!(!a.passed && !a.invalid, "{a:?}");
    assert_eq!(a.failing_seeds, vec![1]);
}

// The headless `hauksbee run` half (same board, no declared supply, `--strict`
// exit 3 with this reason) lives in hauksbee-engine/tests/cli_strict_plain.rs:
// this crate's tests may only spawn `hauksbee-ci`, the one binary CI builds for them.
