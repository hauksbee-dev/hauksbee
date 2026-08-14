//! Round-2 CI-surface contracts, the end-to-end half: the real binary, real
//! boards, real exit codes.
//!
//! * Multi-spec `hauksbee-ci run a.toml b.toml --junit out.xml`: one merged
//!   JUnit document, one aggregate summary, worst exit code of the set
//!   (severity order 3 > 2 > 1 > 0).
//! * `hauksbee-ci init` writes to the CURRENT DIRECTORY by default and
//!   honours `--out`, with the generated `board = "..."` path relative to
//!   where the spec lands.
//! * A board-side hauksbee-waivers.toml reaches `hauksbee-ci run`: a waived
//!   failure is visible but does not gate; an unparseable file gates.

use std::path::{Path, PathBuf};
use std::process::Command;

use hauksbee_ci::{run, RunConfig, Spec};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_hauksbee-ci")
}

fn blinky_board() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/boards/blinky.kicad_pcb")
}

/// A fresh dir holding a copy of the blinky board plus a spec asserting
/// `+5V >= min_v` with no firmware (pure analog, fast). `min_v = 1.0` passes
/// at the 5 V rail; `min_v = 100.0` cannot.
fn board_dir(tag: &str, min_v: f64) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let board = dir.path().join("blinky.kicad_pcb");
    std::fs::copy(blinky_board(), &board).unwrap();
    let spec = dir.path().join(format!("{tag}.toml"));
    std::fs::write(
        &spec,
        format!(
            "name = \"{tag}\"\nboard = \"blinky.kicad_pcb\"\nduration_ms = 20\n\n\
             [[supply]]\nnet = \"+5V\"\nkind = \"ideal\"\nvolts = 5.0\n\n\
             [[assert]]\nkind = \"voltage\"\nnet = \"+5V\"\nmin = {min_v}\n"
        ),
    )
    .unwrap();
    (dir, spec)
}

#[test]
fn multi_spec_run_merges_junit_and_exits_with_the_worst_code() {
    let (dir, green_spec) = board_dir("green", 1.0);
    let junit = dir.path().join("merged.xml");
    let missing = dir.path().join("nope.toml");

    let out = Command::new(bin())
        .arg("run")
        .arg(&green_spec)
        .arg(&missing) // exit-2 member: no such spec
        .arg("--junit")
        .arg(&junit)
        .output()
        .expect("binary runs");

    // Worst of {0, 2} is 2.
    assert_eq!(out.status.code(), Some(2), "worst exit code wins");

    // One merged document: single envelope, one suite per spec.
    let xml = std::fs::read_to_string(&junit).expect("merged junit written");
    assert_eq!(xml.matches("<?xml").count(), 1, "{xml}");
    assert_eq!(xml.matches("<testsuite ").count(), 2, "{xml}");
    assert!(xml.contains("errors=\"1\""), "the load error counts: {xml}");

    // The aggregate summary names each spec's verdict.
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("=== 2 specs ==="), "{stdout}");
    assert!(stdout.contains("[GREEN]"), "{stdout}");
    assert!(stdout.contains("[SPEC ERROR]"), "{stdout}");
    assert!(stdout.contains("worst exit code of the set: 2"), "{stdout}");
}

#[test]
fn a_red_member_makes_the_set_red_but_runs_every_member() {
    let (dir, green_spec) = board_dir("green", 1.0);
    let red_spec = {
        let spec = dir.path().join("red.toml");
        std::fs::write(
            &spec,
            "name = \"red\"\nboard = \"blinky.kicad_pcb\"\nduration_ms = 20\n\n\
             [[supply]]\nnet = \"+5V\"\nkind = \"ideal\"\nvolts = 5.0\n\n\
             [[assert]]\nkind = \"voltage\"\nnet = \"+5V\"\nmin = 100.0\n",
        )
        .unwrap();
        spec
    };
    let out = Command::new(bin())
        .arg("run")
        .arg(&red_spec)
        .arg(&green_spec)
        .output()
        .expect("binary runs");
    assert_eq!(out.status.code(), Some(1), "worst of {{1, 0}} is 1");
    let stdout = String::from_utf8_lossy(&out.stdout);
    // The green member still ran and is reported after the red one.
    assert!(
        stdout.contains("[RED]") && stdout.contains("[GREEN]"),
        "{stdout}"
    );
    // The failing bound is marked and the why names the observed shortfall.
    assert!(stdout.contains("<- FAILED HERE"), "{stdout}");
    assert!(stdout.contains("below your"), "{stdout}");
}

#[test]
fn init_defaults_to_the_current_directory_with_a_relative_board_path() {
    let dir = tempfile::tempdir().unwrap();
    let hw = dir.path().join("hardware");
    std::fs::create_dir_all(&hw).unwrap();
    let board = hw.join("blinky.kicad_pcb");
    std::fs::copy(blinky_board(), &board).unwrap();
    let cwd = dir.path().join("ci");
    std::fs::create_dir_all(&cwd).unwrap();

    let out = Command::new(bin())
        .arg("init")
        .arg("../hardware/blinky.kicad_pcb")
        .current_dir(&cwd)
        .output()
        .expect("binary runs");
    assert!(
        out.status.success(),
        "init succeeds: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // The spec landed where the user was standing, not beside the board.
    let spec_path = cwd.join("blinky.toml");
    assert!(spec_path.exists(), "spec lands in the current directory");
    assert!(
        !hw.join("blinky.toml").exists(),
        "nothing is written beside the board"
    );

    // The generated board reference resolves FROM the spec's directory.
    let spec = Spec::load(&spec_path).expect("generated spec loads");
    assert!(
        spec.board_path().exists(),
        "the relative board path resolves: {}",
        spec.board_path().display()
    );

    // The guidance names where the file went rather than telling the user to
    // move it.
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("wrote starter spec to"), "{stdout}");
    assert!(stdout.contains("HAUKSBEE_CI_SPECS"), "{stdout}");
}

#[test]
fn init_out_accepts_a_directory_or_a_file_path() {
    let dir = tempfile::tempdir().unwrap();
    let board = dir.path().join("blinky.kicad_pcb");
    std::fs::copy(blinky_board(), &board).unwrap();

    // --out <dir>: <stem>.toml inside it, created on demand.
    let out_dir = dir.path().join("ci");
    let out = Command::new(bin())
        .arg("init")
        .arg(&board)
        .arg("--out")
        .arg("ci/")
        .current_dir(dir.path())
        .output()
        .expect("binary runs");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let spec_path = out_dir.join("blinky.toml");
    assert!(spec_path.exists(), "--out dir/ places <stem>.toml inside");
    assert!(Spec::load(&spec_path).is_ok(), "and it loads");
    let generated = std::fs::read_to_string(&spec_path).expect("read generated spec");
    assert!(
        generated.contains("#   hauksbee-ci run ci/blinky.toml"),
        "the file's own copy-paste command names its actual path:\n{generated}"
    );

    // --out <dir> with NO trailing slash and no such directory yet: still a
    // directory. This is the first command a repo runs, the guidance printed
    // right after it says specs are discovered in `ci/`, and resolving it to a
    // FILE named `ci` would scaffold a spec nothing ever picks up.
    let bare_dir = dir.path().join("checks");
    let out = Command::new(bin())
        .arg("init")
        .arg(&board)
        .arg("--out")
        .arg(&bare_dir)
        .current_dir(dir.path())
        .output()
        .expect("binary runs");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        bare_dir.is_dir(),
        "--out checks makes a directory, not a file named `checks`"
    );
    let bare_spec = bare_dir.join("blinky.toml");
    assert!(
        bare_spec.exists(),
        "and <stem>.toml lands inside it, where the hook and the action look"
    );
    assert!(Spec::load(&bare_spec).is_ok(), "and it loads");
    // The printed path is the file, not the directory: a user who copies it
    // into `hauksbee-ci run <path>` must get a run.
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("checks/blinky.toml"),
        "the guidance names the file that was written, got: {stdout}"
    );

    // --out <file.toml>: exactly that file.
    let file_out = dir.path().join("power-up.toml");
    let out = Command::new(bin())
        .arg("init")
        .arg(&board)
        .arg("--out")
        .arg(&file_out)
        .current_dir(dir.path())
        .output()
        .expect("binary runs");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(file_out.exists(), "--out file.toml writes that file");
    assert!(Spec::load(&file_out).is_ok());

    // Overwrite refusal still holds on the explicit path.
    let out = Command::new(bin())
        .arg("init")
        .arg(&board)
        .arg("--out")
        .arg(&file_out)
        .current_dir(dir.path())
        .output()
        .expect("binary runs");
    assert_eq!(out.status.code(), Some(2), "refuses to overwrite");
}

// ── suppress_rail actually suppresses (round-2: it was a silent no-op) ──────

#[test]
fn a_suppressed_rail_with_no_declared_supply_reads_dead() {
    // The regression: SupplyLeg::stamp puts the ideal source on a private
    // node behind Rsupply_<net>, so the old device match never fired and a
    // "suppressed" rail still read nominal. Post-fix, suppressing the only
    // feed leaves the rail at ~0 V.
    let dir = tempfile::tempdir().unwrap();
    let board = dir.path().join("blinky.kicad_pcb");
    std::fs::copy(blinky_board(), &board).unwrap();
    let spec = dir.path().join("suppressed.toml");
    std::fs::write(
        &spec,
        "name = \"suppressed\"\nboard = \"blinky.kicad_pcb\"\nduration_ms = 20\n\
         suppress_rail = [\"+5V\"]\n\n\
         [[assert]]\nkind = \"voltage\"\nnet = \"+5V\"\nmax = 0.5\n",
    )
    .unwrap();
    let result = run(&RunConfig {
        spec,
        ..Default::default()
    })
    .expect("run completes");
    let human = result.render_human();
    assert_eq!(
        result.exit_code(),
        0,
        "a suppressed, otherwise-unfed rail must sit near 0 V: {human}"
    );
}

// ── hollow-gate honesty: asserting on your own ideal source ─────────────────

#[test]
fn asserting_on_an_ideal_fed_net_is_flagged_as_a_hollow_gate() {
    // The green spec asserts +5V >= 1.0 while its own `kind = "ideal"` leg
    // holds +5V at 5.0: that check cannot fail for a board reason, and the
    // report must say so.
    let (_dir, spec) = board_dir("hollow", 1.0);
    let result = run(&RunConfig {
        spec,
        ..Default::default()
    })
    .expect("run completes");
    assert_eq!(result.exit_code(), 0);
    assert!(
        result
            .coverage_warnings
            .iter()
            .any(|w| w.contains("cannot fail for a board reason")),
        "the hollow gate is named: {:?}",
        result.coverage_warnings
    );
    // A behavioral (usb) supply on the same net is exactly what the assertion
    // tests, so it must NOT be flagged.
    let dir = tempfile::tempdir().unwrap();
    let board = dir.path().join("blinky.kicad_pcb");
    std::fs::copy(blinky_board(), &board).unwrap();
    let spec = dir.path().join("behavioral.toml");
    std::fs::write(
        &spec,
        "name = \"behavioral\"\nboard = \"blinky.kicad_pcb\"\nduration_ms = 20\n\n\
         [[supply]]\nnet = \"+5V\"\nkind = \"usb\"\nusb = \"5v0.5a\"\n\n\
         [[assert]]\nkind = \"voltage\"\nnet = \"+5V\"\nmin = 4.5\n",
    )
    .unwrap();
    let result = run(&RunConfig {
        spec,
        ..Default::default()
    })
    .expect("run completes");
    assert!(
        !result
            .coverage_warnings
            .iter()
            .any(|w| w.contains("cannot fail for a board reason")),
        "a behavioral supply can droop, so the assertion is real: {:?}",
        result.coverage_warnings
    );
}

// ── max_temp: the celsius-less form is refused on fallback-bound parts ──────

#[test]
fn celsius_less_max_temp_on_a_fallback_bound_part_is_refused_at_load() {
    let dir = tempfile::tempdir().unwrap();
    let board = dir.path().join("blinky.kicad_pcb");
    std::fs::copy(blinky_board(), &board).unwrap();
    let spec = dir.path().join("maxtemp.toml");
    // R1 (the blinky LED resistor) binds to a generic fallback: no datasheet
    // Tj(max), so "<= device max" could never fail (the overpower monitor
    // front-runs the per-package default ceiling).
    std::fs::write(
        &spec,
        "name = \"maxtemp\"\nboard = \"blinky.kicad_pcb\"\nduration_ms = 20\n\n\
         [[supply]]\nnet = \"+5V\"\nkind = \"ideal\"\nvolts = 5.0\n\n\
         [[assert]]\nkind = \"max_temp\"\nref = \"R1\"\n",
    )
    .unwrap();
    let err = run(&RunConfig {
        spec: spec.clone(),
        ..Default::default()
    })
    .expect_err("the unfalsifiable form must be refused at load");
    let msg = err.to_string();
    assert!(
        msg.contains("explicit `celsius`") && msg.contains("R1"),
        "the refusal names the fix and the part: {msg}"
    );
    // With an explicit ceiling the same assert loads and runs.
    std::fs::write(
        &spec,
        "name = \"maxtemp\"\nboard = \"blinky.kicad_pcb\"\nduration_ms = 20\n\n\
         [[supply]]\nnet = \"+5V\"\nkind = \"ideal\"\nvolts = 5.0\n\n\
         [[assert]]\nkind = \"max_temp\"\nref = \"R1\"\ncelsius = 125\n",
    )
    .unwrap();
    assert!(
        run(&RunConfig {
            spec,
            ..Default::default()
        })
        .is_ok(),
        "the explicit-celsius form still runs"
    );
}

// ── waivers reach hauksbee-ci run (board-side hauksbee-waivers.toml) ────────

fn write_waivers(dir: &Path, body: &str) {
    std::fs::write(dir.join("hauksbee-waivers.toml"), body).unwrap();
}

#[test]
fn a_board_side_waiver_turns_a_red_run_green_but_stays_visible() {
    let (dir, spec) = board_dir("waived", 100.0);
    write_waivers(
        dir.path(),
        r#"
[[waive]]
check = "ci"
kind = "voltage"
nets = ["+5V"]
reason = "bench-verified; staged rollout of the new rail spec"
until = "2030-01-01"
"#,
    );
    let result = run(&RunConfig {
        spec,
        ..Default::default()
    })
    .expect("run completes");
    assert_eq!(result.exit_code(), 0, "the waived failure does not gate");
    let human = result.render_human();
    assert!(human.contains("[WAIVED]"), "still visible: {human}");
    assert!(human.contains("bench-verified"), "with the reason: {human}");
}

#[test]
fn an_expired_board_side_waiver_gates_again() {
    let (dir, spec) = board_dir("expired", 100.0);
    write_waivers(
        dir.path(),
        r#"
[[waive]]
check = "ci"
kind = "voltage"
nets = ["+5V"]
reason = "lapsed on purpose"
until = "2020-01-01"
"#,
    );
    let result = run(&RunConfig {
        spec,
        ..Default::default()
    })
    .expect("run completes");
    assert_eq!(result.exit_code(), 1, "the finding comes back");
    assert!(
        result.waiver_notes.iter().any(|n| n.contains("lapsed")),
        "and the red is explainable: {:?}",
        result.waiver_notes
    );
}

#[test]
fn an_unparseable_waiver_file_warns_and_fails_toward_gating() {
    let (dir, spec) = board_dir("garbage", 100.0);
    write_waivers(dir.path(), "this is not toml [[[");
    let result = run(&RunConfig {
        spec,
        ..Default::default()
    })
    .expect("run completes; a bad waiver file is a warning, not a crash");
    assert_eq!(result.exit_code(), 1, "fails closed: the finding gates");
    assert!(
        result
            .waiver_notes
            .iter()
            .any(|n| n.contains("ignoring the waiver file")),
        "the report says why: {:?}",
        result.waiver_notes
    );
}

#[test]
fn a_waiver_scoped_to_another_net_does_not_cover_the_failure() {
    let (dir, spec) = board_dir("scope", 100.0);
    write_waivers(
        dir.path(),
        r#"
[[waive]]
check = "ci"
kind = "voltage"
nets = ["3V3"]
reason = "a different rail entirely"
until = "2030-01-01"
"#,
    );
    let result = run(&RunConfig {
        spec,
        ..Default::default()
    })
    .expect("run completes");
    assert_eq!(result.exit_code(), 1, "scope is per-finding, not per-rule");
    assert!(
        result
            .waiver_notes
            .iter()
            .any(|n| n.contains("matched nothing")),
        "the unused waiver is called out as stale: {:?}",
        result.waiver_notes
    );
}
