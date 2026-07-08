//! Drift guard for the `sim` command's help text (persona-panel finding, analog
//! engineer): the "Honesty" paragraph must not claim a capability refuses when
//! it actually works. `--ac`, `--dc`, and `--format raw`/`both` all landed (plan
//! steps 9/14), so the help must describe them as working, and must NOT repeat
//! the stale claim that they REFUSE / cannot be fed / are not yet built.
//!
//! The SPICE *loader* claims are gated by the compat-drift test in
//! `hauksbee-ir` against `docs/spice-compat/compatibility.md`. That test lives
//! IR-side and cannot see the engine's clap strings, so this focused CLI test
//! owns the help-text surface: it exercises the real compiled binary's
//! `sim --help` output, the same string a user reads.

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
    assert!(help.contains("All four analyses run"), "help should say all four analyses run");
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
    // Cross-link to the drift-tested compatibility statement.
    assert!(
        help.contains("docs/spice-compat/compatibility.md"),
        "help should cross-link the compatibility statement"
    );
}
