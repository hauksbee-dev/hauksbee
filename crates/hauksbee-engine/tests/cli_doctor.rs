//! CLI smoke tests for `hauksbee doctor --backends`.
//!
//! This subcommand is what `scripts/doctor.sh` calls so the shell tool can never
//! disagree with the engine about which co-sim backends are usable. The contract
//! the shell parser relies on:
//!   - it runs and exits 0 (a backend being absent is information, not failure),
//!   - stdout is one `NAME<TAB>STATUS<TAB>DETAIL` line per known backend, each
//!     backend appearing exactly once, STATUS a single known token,
//!   - `--json` emits a well-formed object with a `backends` array.
//!
//! These drive the actual compiled binary so the output contract is tested end
//! to end. The tests assert on the SHAPE of the output, never on whether any
//! particular emulator is installed on the test host (so they pass in CI with no
//! QEMU/Renode present, and on a dev box with them present).

use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_hauksbee")
}

/// Every backend the doctor is expected to report, exactly once.
const KNOWN_BACKENDS: &[&str] = &["avr", "qemu-xtensa", "qemu-riscv32", "renode"];

/// Statuses the machine-readable table may carry (single lowercase tokens).
const KNOWN_STATUSES: &[&str] = &["ok", "absent", "builtin", "disabled"];

fn run(args: &[&str]) -> std::process::Output {
    Command::new(bin())
        .args(args)
        .output()
        .expect("hauksbee binary runs")
}

#[test]
fn doctor_backends_runs_exits_zero_and_lists_each_backend_once() {
    let out = run(&["doctor", "--backends"]);
    assert!(
        out.status.success(),
        "doctor --backends must exit 0 (a missing backend is information, not \
         failure); got {:?}, stderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8_lossy(&out.stdout);
    for name in KNOWN_BACKENDS {
        // Each backend heads exactly one stdout line: `name<TAB>...`.
        let count = stdout
            .lines()
            .filter(|l| l.split('\t').next() == Some(name))
            .count();
        assert_eq!(
            count, 1,
            "backend '{name}' must appear exactly once on stdout, saw {count}. \
             stdout:\n{stdout}"
        );
    }

    // Every data line is a well-formed 3-field record with a known status token,
    // and names one of the known backends (no stray lines on stdout).
    for line in stdout.lines().filter(|l| !l.is_empty()) {
        let fields: Vec<&str> = line.splitn(3, '\t').collect();
        assert_eq!(
            fields.len(),
            3,
            "each line is NAME<TAB>STATUS<TAB>DETAIL; offending line: {line:?}"
        );
        assert!(
            KNOWN_BACKENDS.contains(&fields[0]),
            "unexpected backend name on stdout: {:?}",
            fields[0]
        );
        assert!(
            KNOWN_STATUSES.contains(&fields[1]),
            "status must be a single known token, got {:?} on line {line:?}",
            fields[1]
        );
    }
}

#[test]
fn doctor_defaults_to_backends_without_the_flag() {
    // `hauksbee doctor` with no flag behaves like `--backends` (the only check
    // today), so the documented short form works.
    let out = run(&["doctor"]);
    assert!(out.status.success(), "bare `doctor` exits 0");
    let stdout = String::from_utf8_lossy(&out.stdout);
    for name in KNOWN_BACKENDS {
        assert!(
            stdout.lines().any(|l| l.split('\t').next() == Some(name)),
            "bare `doctor` still reports backend '{name}'. stdout:\n{stdout}"
        );
    }
}

#[test]
fn doctor_backends_json_is_well_formed() {
    let out = run(&["doctor", "--backends", "--json"]);
    assert!(out.status.success(), "doctor --json exits 0");
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("--json stdout is valid JSON");
    let arr = v
        .get("backends")
        .and_then(|b| b.as_array())
        .expect("`backends` is an array");

    let names: Vec<&str> = arr
        .iter()
        .map(|b| b["name"].as_str().expect("each backend has a name"))
        .collect();
    for name in KNOWN_BACKENDS {
        assert_eq!(
            names.iter().filter(|n| *n == name).count(),
            1,
            "JSON reports backend '{name}' exactly once; names: {names:?}"
        );
    }
    // `available` is a bool and agrees with the status token.
    for b in arr {
        let status = b["status"].as_str().expect("status is a string");
        let available = b["available"].as_bool().expect("available is a bool");
        assert_eq!(
            available,
            status == "ok" || status == "builtin",
            "available must track status; backend: {b}"
        );
    }
}
