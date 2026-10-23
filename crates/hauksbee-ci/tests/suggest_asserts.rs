//! `hauksbee-ci init --firmware .. --suggest-asserts`: the assertions a firmware
//! run supports are proposed from the run, and what is proposed is TRUE of that
//! run and FALSE of a different firmware behaviour.
//!
//! Two properties make a suggestion worth having. Uncommented as written it
//! passes (it describes what the firmware did, not what a template hoped), and
//! when the number in it is edited to something the firmware does not do, it
//! fails. A suggestion that cannot fail is decoration.

use std::path::{Path, PathBuf};

use hauksbee_ci::init::{init_observing, Observe, SUGGESTED_HEADER};
use hauksbee_ci::run;
#[cfg(feature = "avr")]
use hauksbee_ci::Spec;

use crate::support::{blinky_board, repo, run_config};

/// A scratch directory holding a copy of `board`, so the spec lands there.
fn scratch(board: &Path, tag: &str) -> (PathBuf, PathBuf) {
    let dir =
        std::env::temp_dir().join(format!("hauksbee_ci_suggest_{}_{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let dst = dir.join(board.file_name().unwrap());
    std::fs::copy(board, &dst).unwrap();
    (dir, dst)
}

/// The spec with every suggested block uncommented: strip `# ` from each line
/// after the suggestions header. Notes are `## ` lines and stay comments.
fn uncomment_suggestions(text: &str) -> String {
    let (head, tail) = text
        .split_once(SUGGESTED_HEADER)
        .expect("the spec carries the suggestions header");
    let tail: String = tail
        .lines()
        .map(|l| l.strip_prefix("# ").unwrap_or(l))
        .collect::<Vec<_>>()
        .join("\n");
    format!("{head}{SUGGESTED_HEADER}{tail}\n")
}

fn verdict(spec: &Path) -> hauksbee_ci::CiResult {
    run(&run_config(spec.to_path_buf())).expect("the spec runs")
}

/// Scaffold + observe the AVR demo (a ~5 Hz blink on D13, a UART banner).
#[cfg(feature = "avr")]
fn suggest_blinky(tag: &str, duration_ms: Option<f64>) -> (PathBuf, hauksbee_ci::init::Suggested) {
    let (dir, board) = scratch(&blinky_board(), tag);
    let fw = repo("testdata/firmware/demo/demo.hex");
    let found = init_observing(
        &board,
        Some(&dir),
        Observe {
            firmware: &fw,
            duration_ms,
        },
    )
    .expect("init --suggest-asserts");
    (found.path.clone(), found)
}

#[test]
#[cfg(feature = "avr")]
fn the_suggestions_pass_as_written_and_fail_when_the_number_is_edited() {
    let (spec, found) = suggest_blinky("blinky", None);
    assert!(found.warning.is_none(), "{:?}", found.warning);
    assert!(
        found
            .proposed
            .iter()
            .any(|l| l.starts_with("toggle: D13 at 4.9") || l.starts_with("toggle: D13 at 5.")),
        "the 5 Hz blink on D13 is proposed: {:?}",
        found.proposed
    );
    assert!(
        found
            .proposed
            .iter()
            .any(|l| l.contains("hauksbee-demo v1")),
        "the banner is proposed: {:?}",
        found.proposed
    );

    // As scaffolded the suggestions are comments, so the starter is still a
    // spec that loads with only the live gate.
    let text = std::fs::read_to_string(&spec).unwrap();
    assert!(text.contains("firmware = "), "the firmware line is live");
    assert!(
        text.contains("Not observed") && text.contains("`bus`"),
        "{text}"
    );
    let starter = Spec::load(&spec).expect("the starter loads");
    assert_eq!(starter.asserts.len(), 1, "only no_faults is live");

    // Uncommented, it loads and PASSES: it is what the firmware did.
    std::fs::write(&spec, uncomment_suggestions(&text)).unwrap();
    let loaded = Spec::load(&spec).expect("the uncommented spec loads");
    let kinds: Vec<&str> = loaded.asserts.iter().map(|a| a.kind.as_str()).collect();
    assert!(
        kinds.contains(&"toggle") && kinds.contains(&"uart"),
        "{kinds:?}"
    );
    let green = verdict(&spec);
    assert!(
        green.passed(),
        "the observed assertions describe the run they came from:\n{}",
        green.render_human()
    );

    // Edit the observed rate to one the firmware does not run at: it FAILS.
    let freq = loaded
        .asserts
        .iter()
        .find(|a| a.kind == "toggle")
        .and_then(|a| a.freq_hz)
        .expect("a rate was proposed");
    let edited = uncomment_suggestions(&text).replace(
        &format!("freq_hz = {freq:?}"),
        &format!("freq_hz = {:?}", freq * 3.0),
    );
    assert_ne!(
        edited,
        uncomment_suggestions(&text),
        "the rate was in the text"
    );
    std::fs::write(&spec, edited).unwrap();
    let red = verdict(&spec);
    assert!(!red.passed(), "a wrong expected rate must fail");
    assert!(
        red.render_human().contains("D13"),
        "the failure names the net:\n{}",
        red.render_human()
    );
    let _ = std::fs::remove_dir_all(spec.parent().unwrap());
}

#[test]
#[cfg(feature = "avr")]
fn a_run_too_short_to_time_a_rate_counts_edges_and_says_so() {
    // Too few edges to time a rate: the toggle is proposed as a count, says why,
    // and that count is true of the run it came from. Which duration lands in
    // that window depends on the firmware's start-up, so look for it.
    let mut seen_count = false;
    for ms in [250.0, 350.0, 450.0, 550.0, 650.0] {
        let (spec, found) = suggest_blinky(&format!("short{ms}"), Some(ms));
        assert!(found.warning.is_none(), "{:?}", found.warning);
        let text = std::fs::read_to_string(&spec).unwrap();
        let (_, tail) = text.split_once(SUGGESTED_HEADER).expect("the header");
        if tail.contains("min_toggles") {
            assert!(tail.contains("no rate could be timed"), "{tail}");
            std::fs::write(&spec, uncomment_suggestions(&text)).unwrap();
            let green = verdict(&spec);
            assert!(green.passed(), "{}", green.render_human());
            seen_count = true;
        }
        let _ = std::fs::remove_dir_all(spec.parent().unwrap());
        if seen_count {
            break;
        }
    }
    assert!(
        seen_count,
        "some short run must see a few edges without enough to time a rate"
    );
}

#[test]
#[cfg(feature = "avr")]
fn a_firmware_that_will_not_load_still_writes_the_spec_and_says_why() {
    let (dir, board) = scratch(&blinky_board(), "badfw");
    let bad = dir.join("not-firmware.hex");
    std::fs::write(&bad, "this is not intel hex\n").unwrap();
    let found = init_observing(
        &board,
        Some(&dir),
        Observe {
            firmware: &bad,
            duration_ms: Some(100.0),
        },
    )
    .expect("the scaffold is still written");
    assert!(found.warning.is_some() && found.proposed.is_empty());
    let text = std::fs::read_to_string(&found.path).unwrap();
    assert!(text.contains("nothing is proposed"), "{text}");
    let _ = std::fs::remove_dir_all(dir);
}

/// The same property on the Renode STM32 blink (PC13 at ~5 Hz, "hello from stm32").
#[test]
#[cfg(feature = "renode")]
fn the_stm32_blink_is_proposed_and_holds() {
    hauksbee_testkit::skip_if!(
        !hauksbee_mcu::renode::is_available(),
        "Renode not installed"
    );
    let fw = repo("testdata/firmware/stm32_blinky/blinky.elf");
    hauksbee_testkit::skip_if!(!fw.exists(), "blinky.elf not built");
    let (dir, board) = scratch(
        &repo("testdata/boards/stm32_bluepill_demo.kicad_pcb"),
        "stm32",
    );
    let found = init_observing(
        &board,
        Some(&dir),
        Observe {
            firmware: &fw,
            duration_ms: Some(1000.0),
        },
    )
    .expect("init --suggest-asserts");
    assert!(found.warning.is_none(), "{:?}", found.warning);
    assert!(
        found
            .proposed
            .iter()
            .any(|l| l.contains("hello from stm32")),
        "{:?}",
        found.proposed
    );
    let text = std::fs::read_to_string(&found.path).unwrap();
    std::fs::write(&found.path, uncomment_suggestions(&text)).unwrap();
    let green = verdict(&found.path);
    // The demo board is deliberately unrouted, so the board's own static check
    // is red whatever the firmware does; every assertion the run proposed holds.
    let failing: Vec<_> = green
        .results
        .iter()
        .filter(|r| r.kind != "static_check" && !r.passed)
        .map(|r| r.label.as_str())
        .collect();
    assert!(failing.is_empty(), "{failing:?}\n{}", green.render_human());
    assert!(
        green.results.iter().any(|r| r.kind == "toggle" && r.passed),
        "the proposed blink rate was not judged"
    );
    let _ = std::fs::remove_dir_all(dir);
}
