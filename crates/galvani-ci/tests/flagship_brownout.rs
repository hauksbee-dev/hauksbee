//! Flagship regression: the Tarski power-up brownout, driven entirely through
//! the `galvani-ci` runner and its TOML specs.
//!
//! The board is the real brownout cell, extracted verbatim from the
//! 3,442-component Tarski input system and checked in as a standalone netlist.
//! The as-designed spec FAILS (one fuzzed power-up register state collapses the
//! shunted rail from ~4.97 V to ~0.76 V); the repaired spec, with the milliohm
//! sense shunt swapped in, PASSES across every seed.
//!
//! This is the proof of "CI for hardware": the bug that cost weeks on the bench
//! becomes a one-line regression that fails on the broken layout forever.

use std::path::PathBuf;

use galvani_ci::{run, RunConfig};

fn example(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("examples")
        .join(name)
}

#[test]
fn as_designed_brownout_fails_the_rail_assertion() {
    let cfg = RunConfig {
        spec: example("tarski_brownout.toml"),
    };
    let result = run(&cfg).expect("spec runs");
    // The whole point: as designed, the check is RED.
    assert!(
        !result.passed(),
        "as-designed brownout spec must FAIL the rail assertion"
    );
    assert_eq!(result.exit_code(), 1);

    // The failure must be the rail collapsing well below the brownout threshold
    // on at least one fuzzed seed.
    let r = &result.results[0];
    assert!(!r.passed, "the voltage assertion must be the failure");
    assert!(
        r.detail.contains("0.7") || r.detail.contains("0.8"),
        "expected the collapsed rail (~0.76 V) in the detail, got: {}",
        r.detail
    );
    assert!(
        r.failing_seed.is_some(),
        "a specific fuzz seed must be implicated"
    );
}

#[test]
fn repaired_brownout_passes_across_all_seeds() {
    let cfg = RunConfig {
        spec: example("tarski_brownout_repaired.toml"),
    };
    let result = run(&cfg).expect("spec runs");
    assert!(
        result.passed(),
        "repaired brownout spec must PASS: {}",
        result.render_human()
    );
    assert_eq!(result.exit_code(), 0);
    assert_eq!(result.seeds, 8, "the repair must hold across all 8 seeds");

    // The settled rail must be up near 5 V, not collapsed.
    let r = &result.results[0];
    assert!(r.passed);
    assert!(
        r.detail.contains("4.9") || r.detail.contains("5.0"),
        "expected the healthy rail (~4.97 V) in the detail, got: {}",
        r.detail
    );
}

#[test]
fn the_repair_is_what_flips_red_to_green() {
    // Same board, same fuzz, same assertion — only the shunt value differs.
    // That single override is the difference between RED and GREEN, which is
    // exactly the regression a hardware CI would have caught.
    let broken = run(&RunConfig {
        spec: example("tarski_brownout.toml"),
    })
    .unwrap();
    let fixed = run(&RunConfig {
        spec: example("tarski_brownout_repaired.toml"),
    })
    .unwrap();
    assert!(!broken.passed() && fixed.passed());
}
