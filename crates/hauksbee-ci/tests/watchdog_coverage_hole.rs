//! The watchdog coverage holes on the hauksbee-ci surface (F6c), in the same
//! place a dropped ADC injection and an unexercised bus already appear.
//!
//! Two findings ride `CiResult::coverage_warnings` and therefore the human,
//! JUnit, and GitHub-annotation formats:
//!
//!   1. an armed watchdog that CANNOT reboot the core the way silicon does
//!      (`renode:nrf52840`, the ESP32 timer groups), which means firmware that
//!      HANGS runs forever here and every assertion about behaviour after a hang
//!      is fiction, and
//!   2. reboots that DID happen, which means an assertion that passed across one
//!      was measuring a rebooted core, not the run it claimed.
//!
//! Finding 2 is proven for real on simavr, whose watchdog reboots at the right
//! virtual time: `testdata/firmware/avr_watchdog/wdt.elf` starves its watchdog
//! and `nowdt.elf` is the same firmware with the one arming line removed. That
//! pair also proves the SILENCE: simavr claims full watchdog fidelity, so a fed
//! watchdog must produce nothing at all, which is what makes finding 1 mean
//! something when it does fire. Finding 1 itself needs a backend that falls
//! short, so it is covered by the engine's
//! `watchdog_coverage_surfaces.rs::a_watchdog_that_cannot_bite_reaches_all_four_surfaces`
//! (Renode-gated) and by the scheduler's in-crate mock-core tests.

#![cfg(feature = "avr")]

use std::path::PathBuf;

use hauksbee_ci::{run, RunConfig};

fn repo(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(rel)
}

/// A spec that runs `firmware` on the ATmega blinky board for 200 ms in 5 ms
/// frames. The WDTO_15MS timeout bites in frame 3 or 4 of every reboot cycle, so
/// the window holds about a dozen reboots rather than resting on the first.
///
/// The assertion is deliberately unrelated to the watchdog: a green verdict here
/// would otherwise silently vouch for a recovery path nothing measured.
fn spec_for(firmware: &str, name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "hauksbee-ci-wdcoverage-{}-{name}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("mkdir spec dir");
    let body = format!(
        r#"
name        = "reboot coverage ({name})"
board       = "{}"
firmware    = "{}"
duration_ms = 200
frame_ms    = 5.0

[[supply]]
net   = "+5V"
kind  = "ideal"
volts = 5.0

[[assert]]
kind = "no_faults"
"#,
        repo("crates/hauksbee-ci/examples/boards/blinky.kicad_pcb")
            .canonicalize()
            .unwrap()
            .display(),
        repo(&format!("testdata/firmware/avr_watchdog/{firmware}"))
            .canonicalize()
            .unwrap()
            .display(),
    );
    let p = dir.join("watchdog.toml");
    std::fs::write(&p, body).expect("write spec");
    p
}

#[test]
fn watchdog_reboots_reach_the_ci_coverage_hole_path_in_every_format() {
    if !repo("testdata/firmware/avr_watchdog/wdt.elf").exists() {
        eprintln!("SKIP: wdt.elf not built (run make in testdata/firmware/avr_watchdog)");
        return;
    }
    let spec = spec_for("wdt.elf", "starved");
    let result = run(&RunConfig {
        spec,
        ..Default::default()
    })
    .expect("ci run");

    let hit = result
        .coverage_warnings
        .iter()
        .find(|w| w.contains("the watchdog rebooted the core"))
        .unwrap_or_else(|| {
            panic!(
                "the reboots must reach CiResult::coverage_warnings: {:?}",
                result.coverage_warnings
            )
        })
        .clone();
    assert!(
        hit.starts_with("MCU U1: the watchdog rebooted the core ")
            && hit.ends_with(
                "during this run; behaviour observed after the first reboot belongs to a \
                 rebooted core"
            ),
        "the canonical wording, shared with the run binary's surfaces: {hit}"
    );

    let human = result.render_human();
    assert!(
        human.contains(&format!("co-sim COVERAGE HOLE: {hit}")),
        "the human report must carry it in the coverage-hole slot: {human}"
    );
    let junit = result.render_junit();
    assert!(
        junit.contains("COVERAGE HOLE") && junit.contains("the watchdog rebooted the core"),
        "junit must carry it: {junit}"
    );
    let gh = result.render_github_annotations();
    assert!(
        gh.contains("COSIM COVERAGE HOLE") && gh.contains("the watchdog rebooted the core"),
        "github annotations must carry it: {gh}"
    );
}

/// The silence control. simavr's watchdog reboots the way silicon does, so a
/// firmware that never arms one is a run with NO watchdog finding: not an empty
/// limitation, not a zero count, nothing. A warning here would train a pipeline
/// to filter out the warning that matters.
#[test]
fn a_fed_watchdog_on_a_faithful_backend_adds_no_coverage_warning() {
    if !repo("testdata/firmware/avr_watchdog/nowdt.elf").exists() {
        eprintln!("SKIP: nowdt.elf not built (run make in testdata/firmware/avr_watchdog)");
        return;
    }
    let spec = spec_for("nowdt.elf", "fed");
    let result = run(&RunConfig {
        spec,
        ..Default::default()
    })
    .expect("ci run");

    assert!(
        !result
            .coverage_warnings
            .iter()
            .any(|w| w.to_lowercase().contains("watchdog")),
        "a faithful backend with no reboots must add nothing: {:?}",
        result.coverage_warnings
    );
    assert!(!result.render_human().to_lowercase().contains("watchdog"));
    assert!(!result.render_junit().to_lowercase().contains("watchdog"));
    assert!(!result
        .render_github_annotations()
        .to_lowercase()
        .contains("watchdog"));
}
