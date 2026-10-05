//! Power-up state fuzz with rail assertions, on the boards where boot state
//! could matter.
//!
//! The hunt: a board control net whose logic level is *undefined* at power-on
//! (an MCU GPIO that is Hi-Z at reset driving a gate/enable, a latch output with
//! no reset) that, in some boot state, collapses a rail or drives a part beyond
//! its rating. This is the Tarski-brownout class (`flagship_brownout.rs`): one
//! undefined boot bit, compound interactions, a rail down.
//!
//! Method, per board: identify the genuinely-undefined boot control nets
//! (chased to the schematic and recorded in `docs/evidence/FAMOUS_SWEEP.md`),
//! fuzz them across seeds via the `[fuzz]` machinery, solve the DC operating
//! point per seed, and assert the rails hold and no stress fault fires across
//! *every* seed.
//!
//! Honest scope (recorded, not hidden): on these boards every fuzzed boot net
//! was hand-verified to carry a pull resistor that defines its safe state, so
//! the expected and observed result is GREEN. The voltage assertion has teeth
//! only on rails the binder can hold directly (externally-supplied input rails:
//! `LumenPnP` +3.3V/VDC, Olimex +3.3V/+5V); on a regulator *output* rail the
//! schematic-stage solve cannot close the converter loop, so those rails are not
//! asserted (see the doc). The fuzz still exercises the full extract -> bind ->
//! solve path across every boot state, which is the point: the negative is run,
//! not assumed.
//!
//! Corpus-gated: skipped when the board-corpus symlink is absent.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use crate::support::{self, corpus_boards_root as corpus, require_corpus, run_body};

/// The corpus board at `rel`, or `None` when this machine has no corpus: the
/// suite is corpus-gated, and an absent board skips unless the run demands it.
fn corpus_board(rel: &str, what: &str) -> Option<PathBuf> {
    let board = corpus().join(rel);
    if board.exists() {
        return Some(board);
    }
    assert!(!require_corpus(), "corpus required but {what} missing");
    eprintln!("corpus {what} missing; skipping");
    None
}

/// A `voltage` assertion holding `net` inside `[min, max]` on every member.
fn rail_holds(net: &str, min: f64, max: f64) -> String {
    format!(
        "[[assert]]\nkind = \"voltage\"\nname = \"{net} holds across all boot states\"\n\
         net = \"{net}\"\nmin = {min}\nmax = {max}\n"
    )
}

/// One boot-state fuzz, the shape every case in this file has: hold `supplies`,
/// drive every undefined boot net in `nets` to its pulled level, fuzz them all
/// across the undefined range (0 / 3.3 V) over `seeds` members, and require
/// `asserts` plus `no_faults` to hold on EVERY member.
///
/// A fuzz over zero nets passes every assertion without exercising anything,
/// which is the vacuous green this suite exists to refuse, so an empty net list
/// fails rather than reporting a covered negative.
fn boot_state_fuzz(
    case: &str,
    board: &Path,
    supplies: &[(&str, f64)],
    nets: &[String],
    seeds: u32,
    asserts: &str,
) {
    hauksbee_testkit::scanned(&format!("{case} boot-state fuzz (nets found)"), nets.len());
    assert!(
        !nets.is_empty(),
        "{case}: a fuzz over no nets proves nothing"
    );
    let mut setup = String::new();
    for (net, v) in supplies {
        let _ = writeln!(
            setup,
            "[[supply]]\nnet = \"{net}\"\nkind = \"ideal\"\nvolts = {v}"
        );
    }
    for net in nets {
        let _ = writeln!(setup, "[[net_drive]]\nnet = \"{net}\"\nvolts = 0.0");
    }
    let fuzzed: Vec<String> = nets.iter().map(|net| format!("\"{net}\"")).collect();
    let body = format!(
        "name = \"{case} boot-state fuzz\"\nboard = {}\nduration_ms = 1\n{setup}\
         [fuzz]\nseeds = {seeds}\nnets = [{}]\nlevels = [0.0, 3.3]\n\
         {asserts}[[assert]]\nkind = \"no_faults\"\n",
        support::toml_path(board),
        fuzzed.join(", "),
    );
    let result = run_body(&format!("{case}.toml"), &body);
    assert!(
        result.passed(),
        "{case} boot-state fuzz must be GREEN across all seeds:\n{}",
        result.render_human()
    );
    assert_eq!(
        result.seeds, seeds,
        "{case}: must hold across every boot seed"
    );
}

/// `LumenPnP` motherboard: the low-side AO3400A MOSFET gates are driven by
/// STM32F407 GPIOs that are Hi-Z at reset; only a 10k-to-GND pull holds each
/// gate. We fuzz each gate node directly across its undefined boot level
/// (0 / 3.3 V), which is more conservative than fuzzing the GPIO behind the 1k
/// series, and require the externally-held rails stay up and no part faults on
/// any seed. `+3.3V` is the AP1117-3.3 (U4) output fed from `+5V`, so it is
/// not supplied here: an ideal source on a regulator's output would fight it,
/// and its hold assertion now checks the regulator itself.
///
/// The gate nets are read off the board rather than written down here. Naming
/// them meant naming one revision's reference designators, and the revision the
/// corpus fetches has three AO3400A gates at Q2, Q5 and Q6 where the spec said
/// Q1 through Q4, so every `net_drive` and fuzz entry addressed a net that does
/// not exist. The spec still refused rather than fuzzing nothing, which is the
/// behaviour that surfaced this.
#[test]
fn lumenpnp_motor_gate_boot_states_are_safe() {
    let Some(board) = corpus_board("lumenpnp/mobo/mobo.kicad_sch", "LumenPnP") else {
        return;
    };
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
    assert!(
        gates.len() >= 3,
        "expected the board's low-side AO3400A gates, found {gates:?} from FETs {fets:?}"
    );
    boot_state_fuzz(
        "lumenpnp_gates",
        &board,
        &[("VDC", 24.0), ("+5V", 5.0)],
        &gates,
        16,
        &(rail_holds("VDC", 23.0, 25.0) + &rail_holds("+3.3V", 3.2, 3.4)),
    );
}

/// Olimex ESP32-EVB (REV-L): GPIO32/REL1 and GPIO33/REL2 drive the BC817-40
/// relay-driver NPN bases through a 1k series; the bases carry a 10k-to-GND pull
/// (R2/R6, hand-verified to GND), and an NPN with a low base is off regardless.
/// The +3.3V/+5V rails here are externally-supplied (not regulator outputs), so
/// the voltage assertion has real teeth. Fuzz the GPIO drive and require the
/// rails hold and nothing faults on any boot state.
#[test]
fn olimex_evb_relay_boot_states_are_safe() {
    let Some(board) = corpus_board(
        "olimex_esp32/HARDWARE/REV-L/ESP32-EVB_Rev_L.kicad_sch",
        "Olimex EVB",
    ) else {
        return;
    };
    boot_state_fuzz(
        "olimex_relay",
        &board,
        // +3.3V comes from the board's TPS62A02 buck (U2), fed from +5V.
        &[("+5V", 5.0)],
        &["GPIO32/REL1".to_string(), "GPIO33/REL2".to_string()],
        8,
        &(rail_holds("+3.3V", 3.2, 3.4) + &rail_holds("+5V", 4.9, 5.1)),
    );
}

/// MNT Reform motherboard 3.0: the LPC11U24 power supervisor (U18) PIO pins are
/// Hi-Z at reset; the buck-regulator enables (`3V3_PWR_EN`, `5V_PWR_EN`,
/// `1V2_PWR_EN`, `PCIE1_PWR_EN`, `USB_PWR_EN`) are held only by 10k-to-GND pulls
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
    let Some(board) = corpus_board(
        "mnt_reform/reform2-motherboard30-pcb/reform2-motherboard30.kicad_sch",
        "MNT Reform",
    ) else {
        return;
    };
    let enables: Vec<String> = [
        "3V3_PWR_EN",
        "5V_PWR_EN",
        "1V2_PWR_EN",
        "PCIE1_PWR_EN",
        "USB_PWR_EN",
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect();
    boot_state_fuzz(
        "reform_supervisor",
        &board,
        &[("VIN", 12.0), ("+5V", 5.0), ("+3V3", 3.3)],
        &enables,
        24,
        "",
    );
}
