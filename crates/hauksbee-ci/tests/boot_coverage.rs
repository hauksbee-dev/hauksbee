//! Boot-coverage assertion: the formerly-rejected "Hi-Z control input" class,
//! made decidable by running the firmware.
//!
//! `docs/evidence/CORPUS.md` records two faults (Watchy e-paper RES#,
//! `ZSWatch` DISPLAY-EN) as honest misses: a control net is driven only by an MCU
//! GPIO that goes Hi-Z at reset, so its power-up default is undefined, and the
//! netlist *cannot encode the load's intended default* (a display that must be
//! on by default looks byte-identical to a haptic motor that must be off). A
//! static check firing there would be a confident false positive on a shipped
//! board.
//!
//! The `boot-coverage` assertion turns "we cannot know the intended default"
//! into "watch what the firmware actually does": run the co-sim from reset and
//! require the MCU to actively drive the control net to a defined level within a
//! deadline, with no stress fault during the boot window before it.
//!
//! Two proofs run here. First, a constructed two-sided proof on a synthetic
//! board with two real AVR firmware variants: variant A drives a floating
//! MOSFET gate promptly (PASS), variant B never touches it (FAIL, naming the
//! net). Second, the real Watchy v1.5 board under the Espressif QEMU ESP32
//! backend (the e-paper RES# net, GPIO9), also two-sided. The mechanism is
//! backend-agnostic across simavr (AVR), Renode (STM32/nRF/RISC-V) and the
//! Espressif QEMU fork (ESP32/ESP32-C3); see docs/cosim/MCU.md for the matrix. The
//! ESP32 tests skip cleanly when the deterministic fork is absent.

#[cfg(feature = "qemu")]
use std::{fmt::Write as _, path::PathBuf};

use hauksbee_ci::run;

use crate::support::{self, example};

/// The firmware hex files are committed, but rebuild them from source if a
/// toolchain is present so the test tracks the .c, not a stale artifact. Absence
/// of avr-gcc is fine - the committed hex is used.
#[cfg(feature = "avr")]
fn ensure_firmware(variant: &str) {
    let dir = support::repo(&format!("testdata/firmware/boot_gate_{variant}"));
    let hex = dir.join("boot_gate.hex");
    if hex.exists() {
        // Try to refresh if avr-gcc is available; ignore failure.
        if which_avr_gcc() {
            let _ = std::process::Command::new("make")
                .current_dir(&dir)
                .output();
        }
        return;
    }
    assert!(
        which_avr_gcc(),
        "firmware hex missing and avr-gcc not on PATH: cannot build {variant}"
    );
    let out = std::process::Command::new("make")
        .current_dir(&dir)
        .output()
        .expect("run make");
    assert!(
        out.status.success(),
        "building firmware {variant} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[cfg(feature = "avr")]
fn which_avr_gcc() -> bool {
    std::process::Command::new("avr-gcc")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Variant A drives the gate promptly: the boot-coverage assertion PASSES, and
/// so does the `no_faults` assertion (boot window clean).
// Runs AVR .hex firmware on the ATmega boot_gate board, so it needs the
// GPL-gated `avr` feature (the GPL-free renode/qemu build refuses AVR
// firmware by design). The Watchy QEMU rows below cover the external-backend
// half of the mechanism.
#[cfg(feature = "avr")]
#[test]
fn gate_driven_promptly_passes() {
    ensure_firmware("a");
    let result = run(&support::run_config(example("boot_gate_pass.toml"))).expect("PASS spec runs");
    support::assert_green(
        &result,
        "variant A drives the gate within the deadline, so all assertions pass",
    );
    // The boot-coverage assertion is present and green.
    let bc = support::row(&result, "kind:boot_coverage");
    support::assert_row_green(bc, "boot-coverage must pass");
    assert!(
        bc.detail.contains("GATE_CTRL"),
        "names the control net: {}",
        bc.detail
    );
    // simavr counts cycles, so the deadline is judged hard: the detail states
    // the bound it met, with no host-paced condition.
    assert!(
        bc.detail.contains("(<= 20 ms)") && !bc.detail.contains("not judged"),
        "a cycle-exact backend judges the deadline: {}",
        bc.detail
    );
    assert!(bc.host_paced.is_none(), "simavr is not host-paced");
}

/// The same prompt drive against a deadline it misses is a hard FAIL on simavr:
/// cycle-counted time is the same on every host, so the timing verdict stays a
/// verdict (the ESP32 QEMU rows below are the host-paced contrast).
#[cfg(feature = "avr")]
#[test]
fn simavr_keeps_hard_boot_deadline_verdicts() {
    ensure_firmware("a");
    let src = example("boot_gate_pass.toml");
    let examples_dir = src.parent().expect("examples dir");
    let text = std::fs::read_to_string(&src).expect("example spec reads");
    let absolute = |rel: &str| {
        let path = examples_dir.join(rel.trim_end_matches('"'));
        path.canonicalize()
            .expect("spec path resolves")
            .display()
            .to_string()
    };
    let mut spec = String::with_capacity(text.len());
    for line in text.lines() {
        let board = line.strip_prefix("board = \"");
        let firmware = line.strip_prefix("firmware = \"");
        let rewritten = match (board, firmware) {
            (Some(rel), _) => format!("board = '{}'", absolute(rel)),
            (None, Some(rel)) => format!("firmware = '{}'", absolute(rel)),
            (None, None) if line.starts_with("deadline_ms = ") => {
                format!("deadline_ms = {TOO_SOON_MS}")
            }
            (None, None) => line.to_string(),
        };
        spec.push_str(&rewritten);
        spec.push('\n');
    }
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("boot_gate_too_soon.toml");
    std::fs::write(&path, spec).expect("spec writes");
    let result = run(&support::run_config(path)).expect("spec runs");
    let bc = support::row(&result, "kind:boot_coverage");
    // Deliberate negative: a late drive is a numeric FAIL, hard on simavr.
    assert!(
        !bc.passed && bc.detail.contains("boot deadline") && !bc.detail.contains("not judged"),
        "a drive later than the deadline is a hard FAIL on simavr: {}",
        bc.detail
    );
    assert!(bc.host_paced.is_none(), "simavr is not host-paced");
}

/// A deadline variant A's drive misses: the gate is first seen driven at the
/// 1 ms frame (docs/ci/CI.md), later than this.
#[cfg(feature = "avr")]
const TOO_SOON_MS: f64 = 0.5;

/// Variant B never drives the gate: the boot-coverage assertion FAILS, naming
/// the control net that was left Hi-Z. This is the discriminating half - the
/// check has teeth only because this case goes RED.
#[cfg(feature = "avr")]
#[test]
fn gate_left_floating_fails_naming_the_net() {
    ensure_firmware("b");
    let result = run(&support::run_config(example("boot_gate_fail.toml"))).expect("FAIL spec runs");
    assert!(
        !result.passed(),
        "variant B never drives the gate, so boot-coverage must FAIL:\n{}",
        result.render_human()
    );
    let bc = support::row(&result, "kind:boot_coverage");
    // Deliberate negative: the gate is never driven.
    assert!(
        !bc.passed,
        "boot-coverage must fail for the never-driven gate"
    );
    assert!(
        bc.detail.contains("GATE_CTRL") && bc.detail.to_uppercase().contains("NEVER"),
        "the failure names the undriven control net: {}",
        bc.detail
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Watchy v1.5 e-paper RES# boot-coverage on the REAL board, ESP32 QEMU backend.
//
// This executes the formerly-MISSED Watchy display-RES# validation row
// (docs/evidence/CORPUS.md) end-to-end: the unmodified corpus Watchy
// v1.5 layout, its RES net (no pull in v1.5, U1 ESP32-PICO-D4 pad 28 = GPIO9),
// the Espressif QEMU esp32 machine booting a reduced display-init firmware that
// drives GPIO9 HIGH. Two-sided: the display-init firmware PASSES (drives RES in
// time), the esp32_blinky firmware (drives GPIO2/4, never GPIO9) FAILS.
//
// Gated twice: skip when the corpus is absent (hard-fail under
// HAUKSBEE_REQUIRE_CORPUS=1), and skip when the Espressif QEMU binary is not
// installed (QEMU is an environmental dependency, not part of the corpus, so its
// absence is always a skip - the proven backend tests gate the same way).

#[cfg(feature = "qemu")]
fn watchy_v15_board() -> Option<PathBuf> {
    // corpus_board, not a hand-joined path: the fetch layout has no `famous/`
    // level, and the hand-rolled join made the nightly gate's REQUIRE_CORPUS
    // panic fire on every runner that ran the documented fetch.
    hauksbee_testkit::corpus_board(
        env!("CARGO_MANIFEST_DIR"),
        "famous/watchy_history/v1.5/Watchy.kicad_pcb",
    )
}

#[cfg(feature = "qemu")]
fn qemu_xtensa_available() -> bool {
    // Reuse the backend's real discovery so this gate agrees exactly with what
    // the co-sim will actually do. is_available runs the candidate's
    // `-machine help` (is_esp_fork) and only returns true for the Espressif
    // fork that advertises an esp32 machine. A bare path check is not enough:
    // a stock Homebrew qemu-system-xtensa exists on PATH and at an env-pointed
    // path but cannot boot an ESP32 image, so a path check would let the heavy
    // test run and FAIL instead of skipping. This makes the skip decision
    // identical to the backend's accept/reject decision.
    hauksbee_mcu::qemu::is_available(hauksbee_mcu::qemu::QemuArch::Xtensa)
}

/// The Watchy example specs name their board with a repo-relative path into
/// the hand-built corpus layout; that is right for a user running
/// `hauksbee-ci run examples/...` from a hand-built checkout, but wrong for the
/// fetch layout (no `famous/` level) and for `HAUKSBEE_CORPUS_DIR` pointing
/// anywhere else. Run a temp copy of the spec instead, with the `board` key
/// rewritten to wherever `corpus_board` actually resolved it and the `firmware`
/// key absolutized (spec paths resolve relative to the spec file, and the temp
/// copy does not live in examples/). TOML literal strings keep Windows paths
/// free of escape mangling.
#[cfg(feature = "qemu")]
fn resolved_spec(name: &str, board: &std::path::Path) -> (tempfile::TempDir, PathBuf) {
    let src = example(name);
    let text = std::fs::read_to_string(&src).expect("example spec reads");
    let examples_dir = src.parent().expect("examples dir");
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        if line.starts_with("board = ") {
            let _ = writeln!(out, "board = '{}'", board.display());
        } else if let Some(rest) = line.strip_prefix("firmware = \"") {
            let rel = rest.trim_end().trim_end_matches('"');
            let abs = examples_dir
                .join(rel)
                .canonicalize()
                .expect("firmware path resolves");
            let _ = writeln!(out, "firmware = '{}'", abs.display());
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    let dir = tempfile::tempdir().expect("temp dir for rewritten spec");
    let spec = dir.path().join(name);
    std::fs::write(&spec, out).expect("rewritten spec writes");
    (dir, spec)
}

#[cfg(feature = "qemu")]
fn corpus_or_skip(what: &str) -> Option<PathBuf> {
    let board = watchy_v15_board();
    if board.is_none() {
        assert!(
            !support::require_corpus(),
            "corpus required but Watchy v1.5 board missing ({what})"
        );
        eprintln!("skipping {what} (corpus absent)");
    }
    board
}

/// One Watchy v1.5 boot-coverage run under QEMU: resolve the corpus board,
/// rewrite `what`'s spec onto it, run it, and time the run. `None` is a clean
/// skip (no corpus, or no Espressif QEMU).
///
/// The wall-clock ceiling guards the external-backend chunk coarsening
/// (`Scheduler::has_external_backend` -> `chunk_s` about the frame size): without
/// it this co-sim sub-divides into ~80 QMP cont/stop pairs per frame and takes
/// ten MINUTES instead of ~3.5 s, a ~200x regression a 120 s ceiling catches
/// while still tolerating a slow CI machine.
#[cfg(feature = "qemu")]
fn watchy_run(what: &str, spec: &str) -> Option<hauksbee_ci::CiResult> {
    let board = corpus_or_skip(what)?;
    if !qemu_xtensa_available() {
        eprintln!("skipping {what} (Espressif QEMU not installed)");
        return None;
    }
    let (_spec_dir, spec) = resolved_spec(spec, &board);
    let t0 = std::time::Instant::now();
    let result = run(&support::run_config(spec)).expect("the Watchy spec runs");
    let wall = t0.elapsed();
    assert!(
        wall.as_secs() < 120,
        "Watchy QEMU co-sim took {wall:?}; the external-backend chunk \
         coarsening in hauksbee-ci's runner has likely regressed"
    );
    Some(result)
}

/// PASS: the reduced display-init firmware drives the Watchy e-paper RES# net
/// (GPIO9) HIGH within the boot window on the real v1.5 board under QEMU.
#[cfg(feature = "qemu")]
#[test]
fn watchy_v15_display_res_driven_passes() {
    let Some(result) = watchy_run("watchy_v15_display_res PASS", "watchy_v15_display_res.toml")
    else {
        return;
    };
    let bc = support::row(&result, "kind:boot_coverage");
    support::assert_row_green(
        bc,
        &format!(
            "display-init firmware must drive RES in time on the real Watchy board:\n{}",
            result.render_human()
        ),
    );
    assert!(
        bc.detail.contains("RES"),
        "names the RES net: {}",
        bc.detail
    );
    // QEMU credits ESP32 time from host wall-clock spans, so the 700 ms deadline
    // is a time a host's speed flips (measured 384-496 ms on one machine,
    // 704-760 ms on another). The verdict judges only that RES was driven; the
    // deadline is reported unjudged, and the pass is conditional on evidence.
    assert!(
        bc.detail.contains("(approximate)")
            && bc
                .detail
                .contains("the 700 ms boot deadline and the hold through it not judged")
            && bc.detail.contains("MCU U1 runs on a host-paced clock"),
        "the deadline is reported unjudged, with the measured time approximate: {}",
        bc.detail
    );
    let paced = bc
        .host_paced
        .as_ref()
        .expect("a host-paced bound is recorded");
    assert_eq!(paced.mcus, ["U1"]);
    let human = result.render_human();
    assert!(
        human.contains("evidence: qualified")
            && human.contains("the time measured against the 700 ms boot deadline")
            && human.contains("host-paced clock of MCU U1"),
        "the condition reaches the evidence every surface renders:\n{human}"
    );
}

/// FAIL (negative control): firmware that drives GPIO2/4 but never GPIO9 leaves
/// the RES net Hi-Z on the same board, so boot-coverage goes RED. This is the
/// teeth - the check passes above only because it genuinely watches the firmware
/// drive the net.
#[cfg(feature = "qemu")]
#[test]
fn watchy_v15_display_res_undriven_fails() {
    let Some(result) = watchy_run(
        "watchy_v15_display_res FAIL",
        "watchy_v15_display_res_undriven.toml",
    ) else {
        return;
    };
    let bc = support::row(&result, "kind:boot_coverage");
    // Deliberate negative: RES is never driven.
    assert!(
        !bc.passed,
        "a firmware that never drives RES must make boot-coverage FAIL:\n{}",
        result.render_human()
    );
    assert!(
        bc.detail.contains("RES") && bc.detail.to_uppercase().contains("NEVER"),
        "the failure names the undriven RES net: {}",
        bc.detail
    );
    // Never driven is host-independent: a hard FAIL even on a host-paced clock.
    assert!(
        !bc.detail.contains("not judged"),
        "undriven is judged, not left conditional: {}",
        bc.detail
    );
}
