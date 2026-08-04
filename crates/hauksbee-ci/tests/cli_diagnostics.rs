//! The small print of the CLI's error surface, proven against the real binary.
//!
//! Each of these was a message that was technically true and practically
//! useless: an empty directory where a path should be, "no spec file at
//! 'ci/*.toml'" for a path that is fine, a required argument the help calls
//! optional, a C library's opinion leaking through unattributed, findings in
//! phase order rather than file order, and a frequency computed from one edge.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_hauksbee-ci")
}

fn blinky_board() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/boards/blinky.kicad_pcb")
}

/// A temp dir with the blinky board in it, and a spec written as a BARE
/// filename run from inside that dir: the case where `Path::parent()` is `""`.
fn dir_with_board() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::copy(blinky_board(), dir.path().join("blinky.kicad_pcb")).unwrap();
    dir
}

fn run_in(dir: &Path, args: &[&str]) -> Output {
    Command::new(bin())
        .args(args)
        .current_dir(dir)
        .output()
        .expect("binary runs")
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

// L1: `Path::parent()` on a bare filename is `Some("")`, not `None`, so the
// `unwrap_or(".")` never fired and the message ended "resolved relative to the
// spec file at )".
#[test]
fn a_bare_filename_spec_names_a_real_directory_in_its_firmware_error() {
    let dir = dir_with_board();
    std::fs::write(
        dir.path().join("s.toml"),
        "board = \"blinky.kicad_pcb\"\nduration_ms = 5\nfirmware = \"missing.elf\"\n\
         mcu = \"atmega328p\"\n\n[[assert]]\nkind = \"voltage\"\nnet = \"+5V\"\nmin = 1.0\n",
    )
    .unwrap();
    let err = stderr(&run_in(dir.path(), &["run", "s.toml"]));
    assert!(err.contains("no firmware file at"), "{err}");
    assert!(
        err.contains("resolved relative to the spec file at ."),
        "the directory must be named, not left blank: {err}"
    );
    assert!(
        !err.contains("spec file at )"),
        "the empty-directory bug is back: {err}"
    );
}

// L2: the docs teach `hauksbee-ci run ci/*.toml`, which invites quoting it. A
// quoted glob reached the loader as a literal path that of course does not
// exist, and the error sent the user off to check a path that is fine.
#[test]
fn a_quoted_glob_is_diagnosed_as_a_quoted_glob() {
    let dir = dir_with_board();
    let out = run_in(dir.path(), &["run", "ci/*.toml"]);
    assert_eq!(out.status.code(), Some(2));
    let err = stderr(&out);
    assert!(err.contains("looks like a glob"), "{err}");
    assert!(err.contains("your shell does"), "{err}");
    assert!(err.contains("Drop the quotes"), "{err}");
}

#[test]
fn an_ordinary_missing_spec_does_not_get_the_glob_advice() {
    let dir = dir_with_board();
    let err = stderr(&run_in(dir.path(), &["run", "ci/power-up.toml"]));
    assert!(err.contains("no spec file at"), "{err}");
    assert!(!err.contains("glob"), "{err}");
}

// L4: the native ELF reader prints its own unattributed line to stderr and
// returns `rc=-1`. A .hex renamed .elf therefore produced "Unexpected ELF file
// type" from nowhere, followed by an error naming neither the format nor the fix.
#[test]
fn a_hex_renamed_elf_is_named_as_intel_hex_by_both_run_and_check() {
    let dir = dir_with_board();
    // A minimal but real Intel HEX file: one data record, one EOF record.
    std::fs::write(
        dir.path().join("app.elf"),
        ":10010000214601360121470136007EFE09D2190140\n:00000001FF\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("s.toml"),
        "board = \"blinky.kicad_pcb\"\nduration_ms = 5\nfirmware = \"app.elf\"\n\
         mcu = \"atmega328p\"\n\n[[assert]]\nkind = \"voltage\"\nnet = \"+5V\"\nmin = 1.0\n",
    )
    .unwrap();

    let out = run_in(dir.path(), &["run", "s.toml"]);
    assert_eq!(out.status.code(), Some(2));
    let err = stderr(&out);
    assert!(err.contains("Intel HEX"), "{err}");
    assert!(err.contains("Rename it to .hex"), "{err}");
    assert!(
        !err.contains("Unexpected ELF file type"),
        "the C library's unattributed line must never be reached: {err}"
    );
    assert!(!err.contains("rc=-1"), "{err}");

    // And `check` must agree: it used to print OK on the same file.
    let out = run_in(dir.path(), &["check", "s.toml"]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "check must not pass a spec run refuses: {}\n{}",
        stdout(&out),
        stderr(&out)
    );
    assert!(stderr(&out).contains("firmware-format"), "{}", stderr(&out));
}

#[test]
fn an_elf_renamed_hex_is_caught_the_same_way() {
    let dir = dir_with_board();
    std::fs::write(dir.path().join("app.hex"), b"\x7fELF\x02\x01\x01\x00").unwrap();
    std::fs::write(
        dir.path().join("s.toml"),
        "board = \"blinky.kicad_pcb\"\nduration_ms = 5\nfirmware = \"app.hex\"\n\
         mcu = \"atmega328p\"\n\n[[assert]]\nkind = \"voltage\"\nnet = \"+5V\"\nmin = 1.0\n",
    )
    .unwrap();
    let err = stderr(&run_in(dir.path(), &["run", "s.toml"]));
    assert!(err.contains("ELF binary"), "{err}");
    assert!(err.contains("Rename it to .elf"), "{err}");
}

// L5: clap's required-unless message said `<SPEC>...` was required while the
// help it points at calls it `[SPEC]...`, and neither mentioned `--example`.
#[test]
fn run_with_nothing_to_run_names_every_way_to_give_it_something() {
    let dir = dir_with_board();
    let out = run_in(dir.path(), &["run"]);
    assert_eq!(out.status.code(), Some(2));
    let err = stderr(&out);
    assert!(err.contains("needs a spec to run"), "{err}");
    assert!(err.contains("--example blinky"), "{err}");
    assert!(err.contains("hauksbee-ci init"), "{err}");
    assert!(
        !err.contains("required arguments were not provided"),
        "clap's contradictory message must be gone: {err}"
    );
}

#[test]
fn example_alone_is_still_a_complete_invocation() {
    // The whole reason SPEC is optional. If this breaks, the message above is
    // lying.
    let dir = dir_with_board();
    let out = run_in(dir.path(), &["run", "--example", "blinky", "--quiet"]);
    assert!(
        matches!(out.status.code(), Some(0) | Some(1)),
        "--example must run: {}\n{}",
        stdout(&out),
        stderr(&out)
    );
}

// L6: the phases run structural-then-board, so findings came out in phase order
// and a reader working down their spec had to jump backwards.
#[test]
fn check_reports_findings_in_file_order() {
    let dir = dir_with_board();
    std::fs::write(
        dir.path().join("s.toml"),
        "board = \"blinky.kicad_pcb\"\nduration_ms = 5\n\n\
         [[assert]]\nkind = \"voltage\"\nnet = \"NOPE_NET\"\nmin = 1.0\n\n\
         [[assert]]\nkind = \"voltage\"\nnet = \"D13\"\nafter_ms = 500\nmin = 1.0\n",
    )
    .unwrap();
    let out = run_in(dir.path(), &["check", "s.toml", "--json"]);
    assert_eq!(out.status.code(), Some(2));
    let diags: Vec<serde_json::Value> =
        serde_json::from_str(stdout(&out).trim()).expect("one JSON array");
    let lines: Vec<u64> = diags
        .iter()
        .map(|d| d["line"].as_u64().unwrap_or(u64::MAX))
        .collect();
    assert!(lines.len() >= 2, "{diags:?}");
    assert!(
        lines.windows(2).all(|w| w[0] <= w[1]),
        "diagnostics must be in file order, got lines {lines:?}: {diags:?}"
    );
}

// L7: `check --help` promised every independent error in one invocation, but a
// TOML-level error stops deserialization and hides the rest.
#[test]
fn check_help_admits_that_a_toml_level_error_reports_alone() {
    let out = Command::new(bin())
        .args(["check", "--help"])
        .output()
        .expect("binary runs");
    let help = stdout(&out);
    assert!(
        help.contains("reported alone"),
        "the help must not over-promise: {help}"
    );
    assert!(help.contains("in file order"), "{help}");
}

// L10: two toggles is one period and one is no period at all, but the
// arithmetic happily divided anyway and reported "~0.50 Hz from 1 toggles".
#[test]
fn a_frequency_is_refused_when_there_are_too_few_toggles_to_measure_one() {
    let dir = dir_with_board();
    // No firmware, so nothing drives D13 and the net never toggles.
    std::fs::write(
        dir.path().join("s.toml"),
        "board = \"blinky.kicad_pcb\"\nduration_ms = 5\n\n\
         [[supply]]\nnet = \"+5V\"\nkind = \"bench\"\nvolts = 5.0\n\n\
         [[assert]]\nkind = \"toggle\"\nnet = \"D13\"\nfreq_hz = 5.0\n",
    )
    .unwrap();
    let out = run_in(dir.path(), &["run", "s.toml"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let report = stdout(&out);
    assert!(
        report.contains("too few to measure a frequency from"),
        "a rate must be refused, not invented: {report}"
    );
    assert!(
        !report.contains("~0.00 Hz from 0 toggles"),
        "the fabricated measurement must be gone: {report}"
    );
    assert!(
        report.contains("duration_ms"),
        "and it must name the knob: {report}"
    );
}
