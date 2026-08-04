//! CLI tests for `hauksbee models lint`: every `[models.logic]` validation
//! category is proven against a deliberately-broken fixture, asserting the
//! NAMED error text (not just a non-zero exit), and the shipping builtin db
//! must lint clean through the same code path binding uses.

use std::path::PathBuf;
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_hauksbee")
}

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/logic_lint")
        .join(name)
}

fn lint(path: &std::path::Path) -> (i32, String) {
    let out = Command::new(bin())
        .args(["models", "lint"])
        .arg(path)
        .output()
        .expect("hauksbee binary runs");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    (out.status.code().unwrap_or(-1), stdout)
}

/// One broken fixture per validation category, with the named-error substring
/// the lint output must carry.
#[test]
fn lint_names_every_broken_category() {
    let cases: &[(&str, &str)] = &[
        (
            "undeclared_pin.toml",
            "references undeclared name 'phantom_pin'",
        ),
        (
            "width_mismatch.toml",
            "bit index 7 is out of range for register 'reg' (4 bits)",
        ),
        (
            "register_as_scalar.toml",
            "width mismatch: comb expression for 'y' uses register 'reg' (8 bits) as a 1-bit value",
        ),
        (
            "clock_also_comb.toml",
            "clock pin 'clk' of register 'ff' is also referenced combinationally",
        ),
        (
            "unreachable_output.toml",
            "output 'y_ghost' is declared but never assigned by comb (unreachable output)",
        ),
        (
            "bad_tristate.toml",
            "enable pin 'oe_missing' is not a declared input",
        ),
        (
            "non_converging.toml",
            "does not converge within 16 fixpoint sweeps",
        ),
    ];
    for (file, needle) in cases {
        let (code, out) = lint(&fixture(file));
        assert_eq!(
            code, 2,
            "{file}: lint must exit 2 on a finding; output:\n{out}"
        );
        assert!(
            out.contains(needle),
            "{file}: expected the named error {needle:?} in output:\n{out}"
        );
    }
}

/// The shipping builtin digital db must lint clean (same compile path as
/// binding, so lint-ok == bind-ok).
#[test]
fn builtin_digital_db_lints_clean() {
    let db = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../hauksbee-models/db/digital.toml");
    let (code, out) = lint(&db);
    assert_eq!(
        code, 0,
        "builtin digital.toml must lint clean; output:\n{out}"
    );
    for id in ["74hc595", "74hc165", "74hc125", "74hc27", "74hc02"] {
        assert!(
            out.contains(&format!("model '{id}': ok")),
            "expected '{id}' ok line in:\n{out}"
        );
    }
    assert!(out.contains(": clean"), "clean summary line:\n{out}");
}

/// A file with neither [sensor] nor [[models]] is a usage error (exit 1),
/// not a silent pass.
#[test]
fn lint_refuses_unrecognized_toml() {
    let dir = std::env::temp_dir().join("hauksbee_lint_test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("neither.toml");
    std::fs::write(&path, "[something_else]\nx = 1\n").unwrap();
    let (code, _out) = lint(&path);
    assert_eq!(code, 1, "unrecognized TOML shape must be a hard error");
}

/// The descriptor inspection must state, per part, what the co-sim will and will
/// not do about the two facts a wrong answer hides best (F6c):
///
/// - the CLOCK, which is cross-checked against the platform's own declarations at
///   load. A mismatch needs no advisory here because it is already a hard load
///   error, surfaced through the `soc descriptor '{}': ERROR: {e}` path.
/// - the WATCHDOG, either the part's own limitation sentence rendered verbatim,
///   or, when the descriptor claims none, that the part claims full fidelity.
///
/// Proven on a shipped descriptor (nRF52840, whose watchdog arms and never
/// fires) and on that same descriptor with the field removed, which is the only
/// way to reach the full-fidelity branch: every shipped Renode part currently
/// carries a limitation.
#[cfg(feature = "renode")]
#[test]
fn soc_inspection_states_the_clock_cross_check_and_the_watchdog_fidelity() {
    let stock_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../hauksbee-mcu/db/mcu/nrf52840.soc.toml");
    let (code, out) = lint(&stock_path);
    assert_eq!(code, 0, "the shipped descriptor must lint clean:\n{out}");
    assert!(
        out.contains(
            "clock: 64000000 Hz (cross-checked at load against the platform's own \
             `cpu PerformanceInMips` / `nvic systickFrequency` declarations)"
        ),
        "the clock line must say the number is checked, not decorative:\n{out}"
    );
    assert!(
        out.contains(
            "watchdog: The nRF52840 watchdog arms in this co-simulator (it reads back as \
             running, with a correct 32768 Hz reload) but never fires:"
        ),
        "the part's own sentence, verbatim:\n{out}"
    );

    // The other branch: a descriptor claiming no limitation says so as a claim,
    // rather than leaving the reader to infer it from a missing line. Drop the
    // `watchdog_limitation = """ … """` block, closing delimiter included.
    let stock = std::fs::read_to_string(&stock_path).expect("read stock descriptor");
    let mut silent = String::new();
    let mut skipping = false;
    for line in stock.lines() {
        if line.starts_with("watchdog_limitation") {
            skipping = true;
            continue;
        }
        if skipping {
            skipping = line.trim() != "\"\"\"";
            continue;
        }
        silent.push_str(line);
        silent.push('\n');
    }
    assert!(
        !silent.contains("watchdog_limitation") && silent.contains("[[soc.ports]]"),
        "the field must be gone and the rest of the descriptor intact:\n{silent}"
    );

    let dir = std::env::temp_dir().join(format!("hauksbee-soc-lint-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir");
    let path = dir.join("nrf52840.soc.toml");
    std::fs::write(&path, silent).expect("write descriptor");
    let (code, out) = lint(&path);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(code, 0, "still a valid descriptor:\n{out}");
    assert!(
        out.contains(
            "watchdog: this part claims full fidelity, an armed watchdog that is never fed \
             reboots the core the way silicon does"
        ),
        "an absent limitation is a CLAIM and must be printed as one:\n{out}"
    );
}
