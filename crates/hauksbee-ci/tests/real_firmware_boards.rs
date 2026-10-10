//! Real public boards running their real public firmware, end to end.
//!
//! Each case is a corpus board (`corpus.toml`) with firmware built from its
//! upstream source, run through `hauksbee-ci run` exactly as a user would, and
//! judged per assertion: what the firmware visibly did must PASS, and what the
//! run cannot honestly vouch for must stay INVALID with the part that makes it
//! so named. A case that starts passing an INVALID row has gained a model and
//! should be tightened; one that loses a PASS row is a regression.
//!
//! The firmware is not committed. Build it with the recipes recorded in the
//! run notes (pico-examples `sdk-2.2.0`, emonTx3 `411de961`, Arduino-ESP32
//! 1.0.6) and point `HAUKSBEE_REALFW_DIR` at a directory holding
//! `emontx34_ds_jeelib_classic.hex`, `thingplus_blink.elf`,
//! `thingplus_hello.elf` and `esp32evb_getchipid.elf`. Each run takes one to
//! several minutes of emulation, so the cases are `#[ignore]`d: run them with
//! `--run-ignored only`.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::support::{bin, require_corpus, stdout, toml_path};

/// The corpus board at `rel`, or `None` with a skip note.
fn board(rel: &str) -> Option<PathBuf> {
    let found = hauksbee_testkit::corpus_board(env!("CARGO_MANIFEST_DIR"), rel);
    if found.is_none() {
        assert!(!require_corpus(), "corpus required but {rel} is missing");
        eprintln!("SKIP: corpus board {rel} absent");
    }
    found
}

/// The built firmware `name` under `HAUKSBEE_REALFW_DIR`, or `None` with a
/// skip note naming what is missing.
fn firmware(name: &str) -> Option<PathBuf> {
    let Some(dir) = std::env::var_os("HAUKSBEE_REALFW_DIR") else {
        eprintln!(
            "SKIP: HAUKSBEE_REALFW_DIR unset (firmware {name} is built from source, not committed)"
        );
        return None;
    };
    let path = Path::new(&dir).join(name);
    if !path.is_file() {
        eprintln!("SKIP: {} not built", path.display());
        return None;
    }
    Some(path)
}

/// Run `spec_body` (everything after `board`/`firmware`) and return the exit
/// code and the human report.
fn run(name: &str, board: &Path, firmware: &Path, spec_body: &str) -> (i32, String) {
    let dir = tempfile::tempdir().expect("temp dir");
    let spec = dir.path().join(format!("{name}.toml"));
    let text = format!(
        "name = \"{name}\"\nboard = {}\nfirmware = {}\n{spec_body}",
        toml_path(board),
        toml_path(firmware)
    );
    std::fs::write(&spec, text).expect("spec writes");
    let out = Command::new(bin())
        .arg("run")
        .arg(&spec)
        .output()
        .expect("hauksbee-ci runs");
    (out.status.code().unwrap_or(-1), stdout(&out))
}

/// The verdict tag (`PASS`, `FAIL`, `INVALID`) the report printed for the
/// assertion whose label starts with `label`, and the lines of its block.
fn verdict<'a>(report: &'a str, label: &str) -> (&'a str, Vec<&'a str>) {
    let mut lines = report.lines();
    while let Some(line) = lines.next() {
        let t = line.trim_start();
        let Some(rest) = t.strip_prefix('[') else {
            continue;
        };
        let Some((tag, text)) = rest.split_once("] ") else {
            continue;
        };
        if text.starts_with(label) {
            // The block is every following line indented deeper than the
            // header.
            let indent = line.len() - t.len();
            let block = lines
                .by_ref()
                .take_while(|l| l.len() - l.trim_start().len() > indent)
                .collect();
            return (tag, block);
        }
    }
    panic!("no assertion labelled {label:?} in:\n{report}");
}

fn assert_verdict(report: &str, label: &str, want: &str, named: &[&str]) {
    let (tag, block) = verdict(report, label);
    assert_eq!(tag, want, "{label}:\n{report}");
    let block = block.join("\n");
    for part in named {
        assert!(block.contains(part), "{label} must name {part}:\n{block}");
    }
}

/// A `uart` assertion's block shows the needle matched the firmware's output,
/// whatever the verdict the evidence layer then gave it.
fn assert_printed(report: &str, label: &str) {
    let block = verdict(report, label).1.join("\n");
    assert!(
        block.contains("UART=\"") && !block.contains("does NOT"),
        "{label}: the firmware did not print it:\n{block}"
    );
}

/// The run's `EXECUTION EVIDENCE` line: the firmware ran, without a fault.
fn assert_executed(report: &str, mcu: &str) {
    let line = report
        .lines()
        .find(|l| l.contains("EXECUTION EVIDENCE") && l.contains(mcu))
        .unwrap_or_else(|| panic!("no execution evidence for {mcu}:\n{report}"));
    assert!(
        line.contains(": executed;") && line.contains("faults: none"),
        "{line}"
    );
}

/// The emonTx V3.4.5 (ATmega328P-AU, TQFP-32) running the upstream
/// `emonTx34_DS_jeelib_classic` sketch prints its banner and its RFM69CW node
/// line, and drives the LED on D6 within 20 ms. The UART claims stay INVALID
/// because the RFM12B/RFM69 module (U$14) on the SPI pins, the ISP header and
/// the RJ45 one-wire jack have no models; the 3.3 V claim for the same reason
/// (they hang off the rail the MCP1702 makes). The POST prompt does not come
/// within the run: the sketch polls a radio that is not simulated, so that
/// claim is INVALID on U$14, not a FAIL. The claim lists only the nets the
/// RFM12B symbol can drive (SDO on D12/MISO, the open-collector IRQ on D2);
/// its SEL (D10), SCK (D13) and SDI (D11) pins are inputs.
#[cfg(feature = "avr")]
#[test]
#[ignore = "minutes of simavr emulation; needs the corpus and HAUKSBEE_REALFW_DIR"]
fn emontx_v345_boots_its_upstream_firmware() {
    let Some(board) = board("emontx3/hardware/V3.4.5/emonTx V3.4.5.brd") else {
        return;
    };
    let Some(fw) = firmware("emontx34_ds_jeelib_classic.hex") else {
        return;
    };
    let (code, report) = run(
        "emontx",
        &board,
        &fw,
        r#"duration_ms = 1500
# IC2 is the alternate through-hole footprint for the 3.3 V LDO U$6.
no_fit = ["IC2"]

[[supply]]
net = "+5V"
kind = "usb"
usb = "5v0.5a"

[[assert]]
kind = "no_faults"

[[assert]]
kind = "voltage"
net = "3.3V"
min = 3.13
max = 3.46
after_ms = 50

[[assert]]
kind = "uart"
contains = "emonTx V3.4 Discrete Sampling V3.40"

[[assert]]
kind = "uart"
contains = "RFM69CW Node: 8 Freq: 433Mhz Group: 210"

[[assert]]
kind = "boot_coverage"
net = "D6"
min = 3.0
deadline_ms = 20.0

[[assert]]
kind = "uart"
contains = "POST.....wait 10s"
"#,
    );
    assert_eq!(code, 3, "{report}");
    assert_executed(&report, "U1_TQFP");
    assert_verdict(&report, "D6 driven", "PASS", &[]);
    assert_verdict(&report, "no stress faults", "INVALID", &["U$14", "U$13"]);
    assert_verdict(&report, "3.3V in", "INVALID", &["U$14", "ISP1", "U$13"]);
    for line in [
        "emonTx V3.4 Discrete Sampling V3.40",
        "RFM69CW Node: 8 Freq: 433Mhz Group: 210",
    ] {
        let label = format!("UART contains \"{line}\"");
        assert_printed(&report, &label);
        assert_verdict(&report, &label, "INVALID", &["U$14", "ISP1", "S1", "S3"]);
    }
    assert_verdict(
        &report,
        "UART contains \"POST",
        "INVALID",
        &["U$14 (RFM12B) has no model", "D12/MISO", "D2"],
    );
}

/// The spec body shared by the SparkFun Thing Plus RP2040 cases.
#[cfg(feature = "renode")]
const THING_PLUS_SUPPLY: &str = r#"
# U7 carries the value "3.3V"; the schematic part is AP2112K-3.3.
[[override]]
ref = "U7"
value = "AP2112K-3.3"

[[supply]]
net = "V_USB"
kind = "usb"
usb = "5v0.5a"
"#;

#[cfg(feature = "renode")]
fn thing_plus() -> Option<PathBuf> {
    if !hauksbee_mcu::renode::is_available() {
        eprintln!("SKIP: Renode not found");
        return None;
    }
    board("sparkfun_thingplus_rp2040/Hardware/RP2040_Thing_Plus.brd")
}

/// pico-examples `blink` toggles `LED_BUILTIN` at 2 Hz on the Thing Plus. The
/// rail and fault claims stay INVALID: the W25Q128 flash (U4) and the WS2812
/// (U6) sit on 3.3V with no model.
#[cfg(feature = "renode")]
#[test]
#[ignore = "minutes of Renode emulation; needs the corpus and HAUKSBEE_REALFW_DIR"]
fn thing_plus_rp2040_blinks_with_pico_examples_blink() {
    let Some(board) = thing_plus() else { return };
    let Some(fw) = firmware("thingplus_blink.elf") else {
        return;
    };
    let body = format!(
        r#"duration_ms = 1200

# 250 ms on / 250 ms off; a 100 ms pulse floor keeps the poll at 50 ms or finer.
[timing]
min_pulse_us = 100000.0
{THING_PLUS_SUPPLY}
[[assert]]
kind = "no_faults"

[[assert]]
kind = "voltage"
net = "3.3V"
min = 3.13
max = 3.46
after_ms = 50

[[assert]]
kind = "toggle"
net = "LED_BUILTIN"
freq_hz = 2.0
tolerance = 0.1
"#
    );
    let (code, report) = run("thingplus_blink", &board, &fw, &body);
    assert_eq!(code, 3, "{report}");
    assert_executed(&report, "U5");
    assert_verdict(&report, "LED_BUILTIN toggles", "PASS", &[]);
    assert_verdict(&report, "no stress faults", "INVALID", &["U4", "U6"]);
    assert_verdict(&report, "3.3V in", "INVALID", &["U4", "U6"]);
}

/// pico-examples `hello_serial` prints "Hello, world!" (CRLF, once a second)
/// on the Thing Plus's UART0. The claim stays INVALID: the QSPI flash (U4),
/// the MAX17048 fuel gauge on I2C (U2) and the WS2812 (U6) are on the RP2040's
/// own nets with no model.
#[cfg(feature = "renode")]
#[test]
#[ignore = "minutes of Renode emulation; needs the corpus and HAUKSBEE_REALFW_DIR"]
fn thing_plus_rp2040_prints_with_pico_examples_hello_serial() {
    let Some(board) = thing_plus() else { return };
    let Some(fw) = firmware("thingplus_hello.elf") else {
        return;
    };
    let body = format!(
        r#"duration_ms = 1300
{THING_PLUS_SUPPLY}
[[assert]]
kind = "uart"
matches = "(Hello, world!\\r\\n){{2}}"
"#
    );
    let (code, report) = run("thingplus_hello", &board, &fw, &body);
    assert_eq!(code, 3, "{report}");
    assert_executed(&report, "U5");
    let label = "UART matches";
    assert_printed(&report, label);
    assert_verdict(&report, label, "INVALID", &["U4", "U2", "U6"]);
}

/// Arduino-ESP32 `GetChipID` on the Olimex ESP32-EVB rev L prints the chip
/// model and core count after the ROM banner, on +3.3V from the TPS62A02
/// buck. The UART claims stay INVALID: the LAN8710A PHY (U4) on the EMAC pins,
/// the CAN transceiver (U6), the BAT54 auto-reset network (U9, U10, D10) on
/// EN/GPIO0 and the IR receiver (U7) have no models.
#[cfg(feature = "qemu")]
#[test]
#[ignore = "about a minute of QEMU emulation; needs the corpus and HAUKSBEE_REALFW_DIR"]
fn esp32_evb_prints_its_chip_id() {
    if !hauksbee_mcu::qemu::is_available(hauksbee_mcu::qemu::QemuArch::Xtensa) {
        eprintln!("SKIP: no Espressif QEMU fork");
        return;
    }
    let Some(board) = board("olimex_esp32/HARDWARE/REV-L/ESP32-EVB_Rev_L.kicad_pcb") else {
        return;
    };
    let Some(fw) = firmware("esp32evb_getchipid.elf") else {
        return;
    };
    let (code, report) = run(
        "esp32evb",
        &board,
        &fw,
        r#"duration_ms = 1000

[[supply]]
net = "/+5V_USB"
kind = "usb"
usb = "5v0.5a"

[[assert]]
kind = "no_faults"

[[assert]]
kind = "voltage"
net = "+3.3V"
min = 3.13
max = 3.46
after_ms = 50

[[assert]]
kind = "uart"
contains = "ESP32 Chip model = "

[[assert]]
kind = "uart"
contains = "This chip has 2 cores"
"#,
    );
    assert_eq!(code, 3, "{report}");
    assert_executed(&report, "U3");
    // The buck's input sits at the USB rail, not lifted by a back-driven
    // output: no operating limit is violated anywhere on the board.
    assert!(!report.contains("operating-limit-violated"), "{report}");
    assert_verdict(&report, "no stress faults", "INVALID", &["U5", "U4"]);
    assert_verdict(&report, "+3.3V in", "INVALID", &["D10", "U7"]);
    for needle in ["ESP32 Chip model = ", "This chip has 2 cores"] {
        let label = format!("UART contains \"{needle}\"");
        assert_printed(&report, &label);
        assert_verdict(
            &report,
            &label,
            "INVALID",
            &["U4", "U6", "U9", "U10", "D10"],
        );
    }
}
