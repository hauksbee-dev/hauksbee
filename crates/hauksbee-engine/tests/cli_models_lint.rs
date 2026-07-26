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
