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
//! (chased to the schematic and recorded in `docs/evidence/CORPUS.md` Round 4),
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

/// The directory the corpus boards sit directly under, whichever layout this
/// machine has.
///
/// The `famous/` level exists only in the hand-built corpus, so joining it
/// unconditionally meant no board resolved on the corpus
/// `scripts/fetch-corpus.sh` produces, and every case below took its
/// board-missing branch.
fn corpus() -> PathBuf {
    hauksbee_testkit::corpus_boards_root(env!("CARGO_MANIFEST_DIR")).unwrap_or_default()
}

fn require_corpus() -> bool {
    std::env::var("HAUKSBEE_REQUIRE_CORPUS").is_ok()
}

/// Write a spec body to a temp file and run it.
fn run_body(name: &str, body: &str) -> hauksbee_ci::CiResult {
    let dir = std::env::temp_dir().join(format!("hauksbee_ci_powerup_fuzz_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join(name);
    std::fs::write(&p, body).unwrap();
    run(&RunConfig {
        spec: p,
        ..Default::default()
    })
    .expect("spec runs")
}

/// LumenPnP motherboard: the low-side AO3400A MOSFET gates are driven by
/// STM32F407 GPIOs that are Hi-Z at reset; only a 10k-to-GND pull holds each
/// gate. We fuzz each gate node directly across its undefined boot level
/// (0 / 3.3 V), which is more conservative than fuzzing the GPIO behind the 1k
/// series, and require the externally-held rails stay up and no part faults on
/// any seed.
///
/// The gate nets are read off the board rather than written down here. Naming
/// them meant naming one revision's reference designators, and the revision the
/// corpus fetches has three AO3400A gates at Q2, Q5 and Q6 where the spec said
/// Q1 through Q4, so every net_drive and fuzz entry addressed a net that does
/// not exist. The spec still refused rather than fuzzing nothing, which is the
/// behaviour that surfaced this.
#[test]
fn lumenpnp_motor_gate_boot_states_are_safe() {
    let board = corpus().join("lumenpnp/mobo/mobo.kicad_sch");
    if !board.exists() {
        assert!(!require_corpus(), "corpus required but LumenPnP missing");
        eprintln!("corpus LumenPnP missing; skipping");
        return;
    }
    // The same enumeration the boot-state panel uses, filtered to the low-side
    // switching FETs this case is about, so the fuzz set is whatever the fetched
    // revision actually carries.
    let extracted = hauksbee_extract::ExtractedBoard::from_kicad_schematic_path(&board)
        .expect("LumenPnP schematic reads");
    let fets: Vec<String> = extracted
        .components
        .iter()
        .filter(|c| c.value.to_ascii_uppercase().starts_with("AO3400"))
        .map(|c| c.reference.clone())
        .collect();
    let gates: Vec<String> = hauksbee_engine::checks::boot::transistor_gate_nets(&extracted)
        .into_iter()
        .filter(|(reference, _)| fets.contains(reference))
        .map(|(_, net)| net)
        .collect();
    // A fuzz over zero nets passes every assertion without exercising anything,
    // which is the vacuous green this suite exists to refuse. Say what was
    // covered, and fail on a discovery that found nothing.
    hauksbee_testkit::scanned("lumenpnp boot-gate fuzz (gate nets found)", gates.len());
    assert!(
        gates.len() >= 3,
        "expected the board's low-side AO3400A gates, found {gates:?} from FETs {fets:?}"
    );

    let drives: String = gates
        .iter()
        .map(|net| format!("[[net_drive]]\nnet = \"{net}\"\nvolts = 0.0\n"))
        .collect();
    let fuzz_nets: String = gates
        .iter()
        .map(|net| format!("\"{net}\""))
        .collect::<Vec<_>>()
        .join(", ");
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
{drives}[fuzz]
seeds = 16
nets = [{fuzz_nets}]
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
