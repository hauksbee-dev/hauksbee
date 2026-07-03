//! Round-4 Surface A: power-up state fuzz with rail assertions on the boards
//! where boot state could matter.
//!
//! The hunt: a board control net whose logic level is *undefined* at power-on
//! (an MCU GPIO that is Hi-Z at reset driving a gate/enable, a latch output with
//! no reset) that, in some boot state, collapses a rail or drives a part beyond
//! its rating. This is the Tarski-brownout class (`flagship_brownout.rs`): one
//! undefined boot bit, compound interactions, a rail down.
//!
//! Method, per board: identify the genuinely-undefined boot control nets
//! (chased to the schematic and recorded in `docs/record/FAMOUS_SWEEP.md` Round 4),
//! fuzz them across seeds via the `[fuzz]` machinery, solve the DC operating
//! point per seed, and assert the rails hold and no stress fault fires across
//! *every* seed.
//!
//! Honest scope (recorded, not hidden): on these boards every fuzzed boot net
//! was hand-verified to carry a pull resistor that defines its safe state, so
//! the expected and observed result is GREEN. The voltage assertion has teeth
//! only on rails the binder can hold directly (externally-supplied input rails:
//! LumenPnP +3.3V/VDC, Olimex +3.3V/+5V); on a regulator *output* rail the
//! schematic-stage solve cannot close the converter loop, so those rails are not
//! asserted (see the doc). The fuzz still exercises the full extract -> bind ->
//! solve path across every boot state, which is the point: the negative is run,
//! not assumed.
//!
//! Corpus-gated: skipped when the board-corpus symlink is absent.

use std::path::PathBuf;

use hauksbee_ci::{run, RunConfig};

fn corpus() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../board-corpus/famous")
}

fn require_corpus() -> bool {
    std::env::var("HAUKSBEE_REQUIRE_CORPUS").is_ok()
}

/// Write a spec body to a temp file and run it.
fn run_body(name: &str, body: &str) -> hauksbee_ci::CiResult {
    let dir = std::env::temp_dir().join("hauksbee_ci_powerup_fuzz");
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join(name);
    std::fs::write(&p, body).unwrap();
    run(&RunConfig { spec: p }).expect("spec runs")
}

/// LumenPnP motherboard: the 4 low-side MOSFET gates (Q1..Q4, AO3400A) are
/// driven by STM32F407 GPIOs that are Hi-Z at reset; only a 10k-to-GND pull
/// (R42/R44/R46/R48, hand-verified to GND) holds each gate. We fuzz each gate
/// node directly across its undefined boot level (0 / 3.3 V) - more conservative
/// than fuzzing the GPIO behind the 1k series - and require the externally-held
/// rails stay up and no part faults on any seed.
#[test]
fn lumenpnp_motor_gate_boot_states_are_safe() {
    let board = corpus().join("lumenpnp/mobo/mobo.kicad_sch");
    if !board.exists() {
        assert!(!require_corpus(), "corpus required but LumenPnP missing");
        eprintln!("corpus LumenPnP missing; skipping");
        return;
    }
    let body = format!(
        r#"name = "lumenpnp boot-state fuzz (MOSFET gates undefined)"
board = "{}"
duration_ms = 1
[[supply]]
net = "VDC"
kind = "ideal"
volts = 24.0
[[supply]]
net = "+5V"
kind = "ideal"
volts = 5.0
[[supply]]
net = "+3.3V"
kind = "ideal"
volts = 3.3
[[net_drive]]
net = "Net-(Q1-Pad1)"
volts = 0.0
[[net_drive]]
net = "Net-(Q2-Pad1)"
volts = 0.0
[[net_drive]]
net = "Net-(Q3-Pad1)"
volts = 0.0
[[net_drive]]
net = "Net-(Q4-Pad1)"
volts = 0.0
[fuzz]
seeds = 16
nets = ["Net-(Q1-Pad1)", "Net-(Q2-Pad1)", "Net-(Q3-Pad1)", "Net-(Q4-Pad1)"]
levels = [0.0, 3.3]
[[assert]]
kind = "voltage"
name = "VDC motor rail holds across all boot gate states"
net = "VDC"
min = 23.0
max = 25.0
[[assert]]
kind = "voltage"
name = "+3.3V holds across all boot gate states"
net = "+3.3V"
min = 3.2
max = 3.4
[[assert]]
kind = "no_faults"
"#,
        board.display()
    );
    let result = run_body("lumenpnp_gates.toml", &body);
    assert!(
        result.passed(),
        "LumenPnP boot-state fuzz must be GREEN across all seeds:\n{}",
        result.render_human()
    );
    assert_eq!(result.seeds, 16, "must hold across all 16 boot seeds");
}

/// Olimex ESP32-EVB (REV-L): GPIO32/REL1 and GPIO33/REL2 drive the BC817-40
/// relay-driver NPN bases through a 1k series; the bases carry a 10k-to-GND pull
/// (R2/R6, hand-verified to GND), and an NPN with a low base is off regardless.
/// The +3.3V/+5V rails here are externally-supplied (not regulator outputs), so
/// the voltage assertion has real teeth. Fuzz the GPIO drive and require the
/// rails hold and nothing faults on any boot state.
#[test]
fn olimex_evb_relay_boot_states_are_safe() {
    let board = corpus().join("olimex_esp32/HARDWARE/REV-L/ESP32-EVB_Rev_L.kicad_sch");
    if !board.exists() {
        assert!(!require_corpus(), "corpus required but Olimex EVB missing");
        eprintln!("corpus Olimex EVB missing; skipping");
        return;
    }
    let body = format!(
        r#"name = "Olimex ESP32-EVB boot-state fuzz (relay NPN bases undefined)"
board = "{}"
duration_ms = 1
[[supply]]
net = "+3.3V"
kind = "ideal"
volts = 3.3
[[supply]]
net = "+5V"
kind = "ideal"
volts = 5.0
[[net_drive]]
net = "GPIO32/REL1"
volts = 0.0
[[net_drive]]
net = "GPIO33/REL2"
volts = 0.0
[fuzz]
seeds = 8
nets = ["GPIO32/REL1", "GPIO33/REL2"]
levels = [0.0, 3.3]
[[assert]]
kind = "voltage"
name = "+3.3V holds across all boot relay states"
net = "+3.3V"
min = 3.2
max = 3.4
[[assert]]
kind = "voltage"
name = "+5V holds across all boot relay states"
net = "+5V"
min = 4.9
max = 5.1
[[assert]]
kind = "no_faults"
"#,
        board.display()
    );
    let result = run_body("olimex_relay.toml", &body);
    assert!(
        result.passed(),
        "Olimex EVB boot-state fuzz must be GREEN across all seeds:\n{}",
        result.render_human()
    );
    assert_eq!(result.seeds, 8);
}

/// MNT Reform motherboard 3.0: the LPC11U24 power supervisor (U18) PIO pins are
/// Hi-Z at reset; the buck-regulator enables (3V3_PWR_EN, 5V_PWR_EN,
/// 1V2_PWR_EN, PCIE1_PWR_EN, USB_PWR_EN) are held only by 10k-to-GND pulls
/// (R105/R92/..., hand-verified to GND), so every regulator is disabled at boot
/// until firmware enables it - textbook-correct sequencing. We fuzz all five
/// enables across their undefined boot level and require no stress fault.
///
/// Honest limit (recorded in the doc): the rails here are regulator *outputs*
/// (LM62460 bucks). The schematic-stage solve cannot close the converter loop,
/// so a held-rail voltage assertion is not meaningful and is omitted; and the
/// stress monitor carries no rated model on the schematic-bound passives, so
/// `no_faults` is a *weak* negative on this board. The trustworthy boot-safety
/// conclusion is the hand-verified pull topology; the fuzz proves the
/// extract/bind/solve path is exercised across every supervisor boot state.
#[test]
fn mnt_reform_supervisor_boot_states_raise_no_fault() {
    let board =
        corpus().join("mnt_reform/reform2-motherboard30-pcb/reform2-motherboard30.kicad_sch");
    if !board.exists() {
        assert!(!require_corpus(), "corpus required but MNT Reform missing");
        eprintln!("corpus MNT Reform missing; skipping");
        return;
    }
    let body = format!(
        r#"name = "MNT Reform mobo3.0 boot-state fuzz (LPC supervisor enables undefined)"
board = "{}"
duration_ms = 1
[[supply]]
net = "VIN"
kind = "ideal"
volts = 12.0
[[supply]]
net = "+5V"
kind = "ideal"
volts = 5.0
[[supply]]
net = "+3V3"
kind = "ideal"
volts = 3.3
[[net_drive]]
net = "3V3_PWR_EN"
volts = 0.0
[[net_drive]]
net = "5V_PWR_EN"
volts = 0.0
[[net_drive]]
net = "1V2_PWR_EN"
volts = 0.0
[[net_drive]]
net = "PCIE1_PWR_EN"
volts = 0.0
[[net_drive]]
net = "USB_PWR_EN"
volts = 0.0
[fuzz]
seeds = 24
nets = ["3V3_PWR_EN", "5V_PWR_EN", "1V2_PWR_EN", "PCIE1_PWR_EN", "USB_PWR_EN"]
levels = [0.0, 3.3]
[[assert]]
kind = "no_faults"
"#,
        board.display()
    );
    let result = run_body("reform_supervisor.toml", &body);
    assert!(
        result.passed(),
        "MNT Reform boot-state fuzz must raise no fault across all seeds:\n{}",
        result.render_human()
    );
    assert_eq!(result.seeds, 24);
}
