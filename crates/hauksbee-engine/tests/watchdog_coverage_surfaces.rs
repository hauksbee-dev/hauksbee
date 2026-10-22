//! The watchdog coverage holes on every report surface (F6c), end to end
//! through the compiled binary.
//!
//! Two findings, mirroring `adc_dropped` exactly:
//!
//!   1. **A watchdog that cannot bite** (`Mcu::watchdog_limitation`). On the
//!      RP2040 the watchdog is unmeasured; on the ESP32 family the
//!      timer-group watchdogs are disabled at launch on purpose, because a
//!      paused guest would trip them. (The nRF52840 and STM32F103 watchdogs
//!      bite under Renode and carry no limitation.) Either way firmware that HANGS runs
//!      forever here, so every assertion about behaviour after a hang is
//!      fiction and a green run vouches for nothing on the recovery path.
//!
//!   2. **A watchdog that DID bite** (`Mcu::watchdog_resets`). Not an error, a
//!      finding: an assertion that passed across a reboot was measuring a
//!      rebooted core, not the run it claimed.
//!
//! Both must appear on all four surfaces (`--json` notes, `CosimJson`, the
//! default text summary, and `--plain` heads-ups), and a backend that claims
//! full fidelity must produce NOTHING on any of them: the silence is what makes
//! the warning mean something.
//!
//! Finding 2 is proven for real against simavr, which reboots at the right
//! virtual time (`testdata/firmware/avr_watchdog/wdt.elf` starves its watchdog;
//! `nowdt.elf` is the same firmware with the one arming line removed, and is the
//! silence control). Finding 1 needs a backend that DOES fall short, i.e. Renode
//! or the ESP32 QEMU, so it skips gracefully like every other `renode_*` /
//! `esp32_*` test when the emulator is not installed. The scheduler-level
//! accessors and the shared wording for finding 1 are covered in-crate by
//! `scheduler.rs`'s `a_backend_that_cannot_reboot_reports_its_watchdog_limitation_verbatim`.

#[cfg(any(feature = "avr", feature = "renode"))]
use crate::support::bin;
#[cfg(feature = "avr")]
use std::path::Path;
#[cfg(any(feature = "avr", feature = "renode"))]
use std::path::PathBuf;
#[cfg(any(feature = "avr", feature = "renode"))]
use std::process::Command;

#[cfg(any(feature = "avr", feature = "renode"))]
fn repo(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(rel)
}

/// An `ATmega328P` board; `wdt.elf` drives PB5, which is `D13` here.
#[cfg(feature = "avr")]
fn avr_board() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../hauksbee-ci/examples/boards/blinky.kicad_pcb")
}

/// 5 ms chunks over 200 ms of virtual time: the `WDTO_15MS` timeout bites in
/// chunk 3 or 4 of every reboot cycle, so this window holds about a dozen
/// reboots rather than resting on the first one.
#[cfg(feature = "avr")]
fn run_avr(firmware: &Path, extra: &[&str]) -> std::process::Output {
    let mut args = vec![
        "run".to_string(),
        avr_board().to_str().unwrap().to_string(),
        "--firmware".to_string(),
        firmware.to_str().unwrap().to_string(),
        "--headless".to_string(),
        "--seconds".to_string(),
        "0.2".to_string(),
        "--chunk-us".to_string(),
        "5000".to_string(),
    ];
    args.extend(extra.iter().map(ToString::to_string));
    Command::new(bin())
        .args(&args)
        .output()
        .expect("hauksbee binary runs")
}

/// The exact line every surface must carry, once the count is known. Written out
/// here rather than imported so a silent re-wording of the shared formatter
/// fails this test instead of following it.
#[cfg(feature = "avr")]
fn expected_reboot_line(resets: u64) -> String {
    let plural = if resets == 1 { "" } else { "s" };
    format!(
        "MCU U1: the watchdog rebooted the core {resets} time{plural} during this run; \
         behaviour observed after the first reboot belongs to a rebooted core"
    )
}

// Boots AVR firmware through the compiled binary, so it needs the GPL-gated
// `avr` feature (the GPL-free renode/qemu build refuses AVR firmware by design).
#[cfg(feature = "avr")]
#[test]
fn a_watchdog_that_rebooted_the_core_reaches_all_four_surfaces() {
    let fw = repo("testdata/firmware/avr_watchdog/wdt.elf");
    hauksbee_testkit::skip_if!(
        !fw.exists(),
        "wdt.elf not built (run make in testdata/firmware/avr_watchdog)"
    );

    // Surface 1 + 2: the `--json` coverage notes and the structured CosimJson
    // field, from one invocation so they cannot disagree about the count.
    let out = run_avr(&fw, &["--json"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("--json emits one object");

    let resets = v["cosim"]["watchdog_resets"]
        .as_array()
        .filter(|a| !a.is_empty())
        .expect("CosimJson.watchdog_resets must carry the reboots");
    assert_eq!(resets.len(), 1, "one MCU on this board: {resets:?}");
    assert_eq!(resets[0]["mcu_ref"], "U1");
    let n = resets[0]["resets"].as_u64().expect("a reboot count");
    assert!(
        n > 1,
        "a starved WDTO_15MS watchdog reboots repeatedly over 200 ms, got {n}"
    );

    let line = expected_reboot_line(n);
    let notes: Vec<&str> = v["notes"]
        .as_array()
        .map(|a| a.iter().filter_map(|n| n["message"].as_str()).collect())
        .unwrap_or_default();
    assert!(
        notes.contains(&line.as_str()),
        "a --json coverage note must carry the reboot line verbatim; got {notes:?}"
    );

    // simavr reboots the way silicon does, so it claims NO limitation: the
    // reboot finding must not drag a fidelity warning along with it.
    assert!(
        v["cosim"]["watchdog_limitations"].is_null(),
        "simavr claims full watchdog fidelity: {}",
        v["cosim"]
    );

    // Surface 3: the default text summary, next to the analog_valid and
    // dropped-ADC warnings, where a person actually reads it.
    let text = String::from_utf8_lossy(&run_avr(&fw, &[]).stdout).to_string()
        + &String::from_utf8_lossy(&run_avr(&fw, &[]).stderr);
    assert!(
        text.contains("WARNING: MCU U1: the watchdog rebooted the core"),
        "the default text summary must warn about the reboots; got:\n{text}"
    );

    // Surface 4: the --plain heads-ups, so the verdict reads "no failures, but
    // N worth a look" instead of a bare "Looks healthy".
    let plain = String::from_utf8_lossy(&run_avr(&fw, &["--plain"]).stdout).to_string();
    assert!(
        plain.contains(&line),
        "a --plain heads-up must carry the same line; got:\n{plain}"
    );
}

/// The silence that makes the warning mean something. `nowdt.elf` is `wdt.elf`
/// with the one arming line removed, on the one backend that claims full
/// watchdog fidelity: no surface may mention a watchdog at all. A run that
/// warned here would train users to ignore the warning that matters.
#[cfg(feature = "avr")]
#[test]
fn a_faithful_backend_with_no_reboots_says_nothing_on_any_surface() {
    let fw = repo("testdata/firmware/avr_watchdog/nowdt.elf");
    hauksbee_testkit::skip_if!(
        !fw.exists(),
        "nowdt.elf not built (run make in testdata/firmware/avr_watchdog)"
    );

    for extra in [vec![], vec!["--json"], vec!["--plain"]] {
        let out = run_avr(&fw, &extra);
        let both = String::from_utf8_lossy(&out.stdout).to_string()
            + &String::from_utf8_lossy(&out.stderr);
        // Evidence provenance echoes the firmware's own path, and this fixture
        // lives under `avr_watchdog/`; scrub that echo so the assertion tests
        // commentary, not the input's directory name.
        let scrubbed = both.replace(&fw.display().to_string(), "<fw>");
        assert!(
            !scrubbed.to_lowercase().contains("watchdog"),
            "simavr with a fed watchdog must be silent on {extra:?}; got:\n{both}"
        );
    }

    // And structurally: both fields are absent, not present-and-empty, so an
    // older consumer's shape is unchanged on a healthy run.
    let out = run_avr(&fw, &["--json"]);
    let v: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).expect("--json object");
    assert!(v["cosim"]["watchdog_limitations"].is_null());
    assert!(v["cosim"]["watchdog_resets"].is_null());
    // The cycle-exact backend also claims no timing limitation, and the claim
    // must be absence, not present-and-empty.
    assert!(v["cosim"]["timing_limitations"].is_null());
}

/// A live-backend limitation against `renode:stm32_bluepill_demo` (an F103),
/// proven byte-for-byte identical across all four surfaces (JSON note,
/// `CosimJson` field, the default text summary, and `--plain`); two surfaces
/// wording the same gap differently is the failure this mirroring exists to
/// avoid.
///
/// **A known timing bias** (`timing_limitations`): the F103's `TIMx` blocks run
/// at the post-PLL 72 MHz against the 8 MHz reset-default core.
///
/// The F103's independent watchdog used to be the other case here ("arms and
/// never fires"); it bites now (`renode_watchdog.rs`), so the same run must carry
/// NO watchdog limitation. A part that still cannot bite, such as the RP2040 or
/// the ESP32 family, reaches the same surfaces through the shared formatter,
/// which `scheduler.rs` covers in-crate.
///
/// Skips without Renode or the STM32 firmware, like every other `renode_*` test.
#[cfg(feature = "renode")]
#[test]
fn known_backend_limitations_reach_all_four_surfaces() {
    hauksbee_testkit::skip_if!(
        !hauksbee_mcu::renode::is_available(),
        "Renode not installed"
    );
    let board = repo("testdata/boards/stm32_bluepill_demo.kicad_pcb");
    let fw = repo("testdata/firmware/stm32_blinky/blinky.elf");
    hauksbee_testkit::skip_if!(!fw.exists(), "blinky.elf not built");

    let args = |extra: &[&str]| {
        let mut a = vec![
            "run",
            board.to_str().unwrap(),
            "--firmware",
            fw.to_str().unwrap(),
            "--headless",
            "--seconds",
            "0.05",
        ];
        a.extend_from_slice(extra);
        Command::new(bin()).args(&a).output().expect("binary runs")
    };

    // (field, "no limitation found" message, substring the sentence must carry)
    let cases = [(
        "timing_limitations",
        "the F103 declares a TIMx timing limitation, so it must be stated",
        "TIMx",
    )];

    let out = args(&["--json"]);
    let v: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).expect("--json object");
    assert!(
        v["cosim"]["watchdog_limitations"]
            .as_array()
            .is_none_or(Vec::is_empty),
        "the F103 watchdog bites, so no limitation may be claimed: {}",
        v["cosim"]["watchdog_limitations"]
    );

    for (field, missing_msg, needle) in cases {
        let out = args(&["--json"]);
        let v: serde_json::Value =
            serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).expect("--json object");
        let limits = v["cosim"][field]
            .as_array()
            .filter(|a| !a.is_empty())
            .unwrap_or_else(|| panic!("{field}: {missing_msg}"));
        assert_eq!(limits[0]["mcu_ref"], "U1", "{field}");
        let sentence = limits[0]["limitation"]
            .as_str()
            .unwrap_or_else(|| panic!("{field}: a whole sentence expected"))
            .to_string();
        assert!(
            sentence.contains(needle) && sentence.ends_with('.'),
            "{field}: the field carries the backend's whole sentence: {sentence}"
        );

        // The same sentence, unchanged, on the note channel and both text surfaces.
        let line = format!("MCU U1: {sentence}");
        let notes: Vec<&str> = v["notes"]
            .as_array()
            .map(|a| a.iter().filter_map(|n| n["message"].as_str()).collect())
            .unwrap_or_default();
        assert!(notes.contains(&line.as_str()), "{field}: {notes:?}");

        let text = String::from_utf8_lossy(&args(&[]).stdout).to_string();
        assert!(
            text.contains(&format!("WARNING: {line}")),
            "{field}: {text}"
        );
        let plain = String::from_utf8_lossy(&args(&["--plain"]).stdout).to_string();
        assert!(plain.contains(&line), "{field}: {plain}");
    }
}
