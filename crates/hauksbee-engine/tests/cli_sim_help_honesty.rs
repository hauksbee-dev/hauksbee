//! Drift guard for the `sim` command's help text: the "Honesty" paragraph must
//! not claim a capability refuses when it actually works (`--ac`, `--dc`, and
//! `--format raw`/`both` all landed). Plus an end-to-end run of a deck whose
//! model comes in through a deck-relative `.include`.

use std::path::PathBuf;
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_hauksbee")
}

fn sim_help() -> String {
    let out = Command::new(bin())
        .args(["sim", "--help"])
        .output()
        .expect("hauksbee sim --help runs");
    assert!(out.status.success(), "sim --help should exit 0");
    let raw = String::from_utf8(out.stdout).expect("help is utf-8");
    // clap re-wraps the doc comment to the terminal width, so a phrase can be
    // split across lines. Collapse every whitespace run to one space; the token
    // order is preserved, so contiguous phrases still match.
    raw.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The help must not claim these working features refuse. Each phrase is a
/// literal fragment of the OLD, stale "Honesty" paragraph.
#[test]
fn sim_help_does_not_claim_working_features_refuse() {
    let help = sim_help();
    let stale = [
        "the front-end cannot yet feed",
        "cannot yet be fed",
        "not yet built",
        "refuse loudly until",
        "the rawfile writer lands",
    ];
    for phrase in stale {
        assert!(
            !help.contains(phrase),
            "sim --help still carries the stale refusal claim {phrase:?}; \
             --ac/--dc/--format raw all work now.\n---help---\n{help}"
        );
    }
}

/// The help must positively describe the analyses and rawfile that work, and
/// carry the promised `--ac` worked example (top-3 panel ask).
#[test]
fn sim_help_states_the_working_capabilities() {
    let help = sim_help();
    // All four analyses named as running.
    assert!(
        help.contains("All four analyses run"),
        "help should say all four analyses run"
    );
    // The AC worked example the panel asked for.
    assert!(
        help.contains("--ac --print V(out)"),
        "help should carry an `--ac` worked example"
    );
    // The rawfile is described as working output, not a refusal.
    assert!(
        help.contains("ngspice ASCII rawfile"),
        "help should describe the working ngspice ASCII rawfile output"
    );
    // Cross-link to the drift-tested compatibility statement, in the URL form
    // an installed user can actually open (round 3 moved every help doc
    // pointer off repo-relative paths).
    assert!(
        help.contains("docs.hauksbee.dev/docs/spice-compat/compatibility"),
        "help should cross-link the compatibility statement URL"
    );
}

/// `.include diode.lib` resolves against the deck's own directory, and the
/// `.print tran` card selects the CSV columns.
#[test]
fn sim_runs_a_deck_with_a_relative_include() {
    let deck = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/sim_decks/half_wave.cir");
    let out = Command::new(bin())
        .args(["sim", deck.to_str().unwrap(), "--tran"])
        .output()
        .expect("hauksbee sim runs");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "sim must succeed:\n{stderr}");
    assert!(stdout.starts_with("time_s,V(out)\n"), "{stdout}");
    let last: Vec<f64> = stdout
        .lines()
        .last()
        .unwrap()
        .split(',')
        .map(|v| v.parse().unwrap())
        .collect();
    assert!(
        last[0] > 1.9e-3 && last[1] > 3.0 && last[1] < 5.0,
        "{last:?}"
    );
}
