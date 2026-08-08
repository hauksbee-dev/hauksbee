//! The watchdog coverage holes on every report surface (F6c), end to end
//! through the compiled binary.
//!
//! Two findings, mirroring `adc_dropped` exactly:
//!
//!   1. **A watchdog that cannot bite** (`Mcu::watchdog_limitation`). On
//!      `renode:nrf52840` the watchdog arms, reads back as running with a
//!      correct 32768 Hz reload, and never fires; on the ESP32 family the
//!      timer-group watchdogs are disabled at launch on purpose, because a
//!      paused guest would trip them. Either way firmware that HANGS runs
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

#[cfg(feature = "avr")]
use std::path::Path;
#[cfg(any(feature = "avr", feature = "renode"))]
use std::path::PathBuf;
#[cfg(any(feature = "avr", feature = "renode"))]
use std::process::Command;

/// The compiled `hauksbee` binary (Cargo sets this for the engine crate's tests).
#[cfg(any(feature = "avr", feature = "renode"))]
fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_hauksbee")
}

#[cfg(any(feature = "avr", feature = "renode"))]
fn repo(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(rel)
}

/// An ATmega328P board; `wdt.elf` drives PB5, which is `D13` here.
#[cfg(feature = "avr")]
fn avr_board() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../hauksbee-ci/examples/boards/blinky.kicad_pcb")
}

/// 5 ms chunks over 200 ms of virtual time: the WDTO_15MS timeout bites in
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
    args.extend(extra.iter().map(|s| s.to_string()));
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
    if !fw.exists() {
        eprintln!("SKIP: wdt.elf not built (run make in testdata/firmware/avr_watchdog)");
        return;
    }

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
    if !fw.exists() {
        eprintln!("SKIP: nowdt.elf not built (run make in testdata/firmware/avr_watchdog)");
        return;
    }

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

/// Finding 1 against a live backend that really does fall short:
/// `renode:nrf52840`, whose watchdog arms and never fires. The backend's own
/// sentence must reach the JSON note, the CosimJson field, and the text
/// surfaces, byte-for-byte identical on each; two surfaces wording the same gap
/// differently is the failure this mirroring exists to avoid.
///
/// Skips without Renode or the STM32 firmware, like every other `renode_*` test.
#[cfg(feature = "renode")]
#[test]
fn a_watchdog_that_cannot_bite_reaches_all_four_surfaces() {
    if !hauksbee_mcu::renode::is_available() {
        eprintln!("SKIP: Renode not installed");
        return;
    }
    let board = repo("testdata/boards/stm32_bluepill_demo.kicad_pcb");
    let fw = repo("testdata/firmware/stm32_blinky/blinky.elf");
    if !fw.exists() {
        eprintln!("SKIP: blinky.elf not built");
        return;
    }

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

    let out = args(&["--json"]);
    let v: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).expect("--json object");
    let limits = v["cosim"]["watchdog_limitations"]
        .as_array()
        .filter(|a| !a.is_empty())
        .expect("this backend's watchdog cannot bite, so it must state the limitation");
    assert_eq!(limits[0]["mcu_ref"], "U1");
    let sentence = limits[0]["limitation"]
        .as_str()
        .expect("a whole sentence")
        .to_string();
    assert!(
        sentence.contains("watchdog") && sentence.ends_with('.'),
        "the field carries the backend's whole sentence: {sentence}"
    );

    // The same sentence, unchanged, on the note channel and both text surfaces.
    let line = format!("MCU U1: {sentence}");
    let notes: Vec<&str> = v["notes"]
        .as_array()
        .map(|a| a.iter().filter_map(|n| n["message"].as_str()).collect())
        .unwrap_or_default();
    assert!(notes.contains(&line.as_str()), "{notes:?}");

    let text = String::from_utf8_lossy(&args(&[]).stdout).to_string();
    assert!(text.contains(&format!("WARNING: {line}")), "{text}");
    let plain = String::from_utf8_lossy(&args(&["--plain"]).stdout).to_string();
    assert!(plain.contains(&line), "{plain}");
}

/// The timing twin of the test above, against the same live backend: the
/// F103's descriptor declares a `timing_limitation` (its TIMx blocks run at
/// the post-PLL 72 MHz against the 8 MHz reset-default core), and that
/// sentence must reach the JSON note, the `CosimJson.timing_limitations`
/// field, and both text surfaces byte-for-byte identically. Same
/// skip-without-Renode behaviour as its sibling.
#[cfg(feature = "renode")]
#[test]
fn a_known_timing_bias_reaches_all_four_surfaces() {
    if !hauksbee_mcu::renode::is_available() {
        eprintln!("SKIP: Renode not installed");
        return;
    }
    let board = repo("testdata/boards/stm32_bluepill_demo.kicad_pcb");
    let fw = repo("testdata/firmware/stm32_blinky/blinky.elf");
    if !fw.exists() {
        eprintln!("SKIP: blinky.elf not built");
        return;
    }

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

    let out = args(&["--json"]);
    let v: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).expect("--json object");
    let limits = v["cosim"]["timing_limitations"]
        .as_array()
        .filter(|a| !a.is_empty())
        .expect("the F103 declares a TIMx timing limitation, so it must be stated");
    assert_eq!(limits[0]["mcu_ref"], "U1");
    let sentence = limits[0]["limitation"]
        .as_str()
        .expect("a whole sentence")
        .to_string();
    assert!(
        sentence.contains("TIMx") && sentence.ends_with('.'),
        "the field carries the descriptor's whole sentence: {sentence}"
    );

    // The same sentence, unchanged, on the note channel and both text surfaces.
    let line = format!("MCU U1: {sentence}");
    let notes: Vec<&str> = v["notes"]
        .as_array()
        .map(|a| a.iter().filter_map(|n| n["message"].as_str()).collect())
        .unwrap_or_default();
    assert!(notes.contains(&line.as_str()), "{notes:?}");

    let text = String::from_utf8_lossy(&args(&[]).stdout).to_string();
    assert!(text.contains(&format!("WARNING: {line}")), "{text}");
    let plain = String::from_utf8_lossy(&args(&["--plain"]).stdout).to_string();
    assert!(plain.contains(&line), "{plain}");
}
