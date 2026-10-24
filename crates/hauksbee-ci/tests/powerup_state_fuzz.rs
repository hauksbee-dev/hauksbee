//! Power-up state fuzz with rail assertions, on the boards where boot state
//! could matter.
//!
//! The hunt: a board control net whose logic level is *undefined* at power-on
//! (an MCU GPIO that is Hi-Z at reset driving a gate/enable, a latch output with
//! no reset) that, in some boot state, collapses a rail or drives a part beyond
//! its rating. This is the brownout class seen on the large internal test board: one
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
/// `open_gap` names the unmodelled parts that today make the run INVALID rather
/// than green (empty: the run must be GREEN); see
/// [`support::assert_invalid_by_open_parts`].
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
    open_gap: &[&str],
    static_flaws: &[StaticFlaw],
) -> hauksbee_ci::CiResult {
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
    if !static_flaws.is_empty() {
        assert_red_on_static_flaws(&result, static_flaws, !open_gap.is_empty(), case);
    } else if open_gap.is_empty() {
        support::assert_green(
            &result,
            &format!("{case} boot-state fuzz must be GREEN across all seeds"),
        );
    } else {
        support::assert_invalid_by_open_parts(&result, open_gap);
    }
    // Every boot seed runs once per member of the board's open variant and
    // corner sets (an LED bound to the assumed forward-voltage corners runs at
    // each corner), and every one of those runs must hold: a boot state is safe
    // only if it is safe whichever part is fitted. Past the run cap the sets
    // get the linear cover instead of the product, and the pass says so.
    let (expected, conditions) = expected_members(board, seeds);
    assert_eq!(
        result.seeds, expected,
        "{case}: must hold across every boot seed ({seeds}) at each open-set member"
    );
    // The sweep's own conditions first, exactly; after them only what the
    // assertions' evidence rests on (parts with no model, modelling
    // assumptions), never another sweep claim.
    let stated = &result.verdict_conditions;
    assert_eq!(
        stated[..conditions.len().min(stated.len())],
        conditions[..],
        "{case}: a pass over a partly swept open set must say so"
    );
    assert!(
        stated[conditions.len()..]
            .iter()
            .all(|c| support::is_evidence_condition(c)),
        "{case}: {stated:?}"
    );
    assert_eq!(
        result.render_json().contains("\"verdict_conditions\""),
        !stated.is_empty(),
        "{case}: the JSON document carries the conditions exactly when there are some"
    );
    result
}

/// One real flaw the board's static check reports, which makes a fuzz over it
/// RED. Every failing static row must be one of these and every one of these
/// must have a failing row: a flaw is listed explicitly or the test fails.
#[derive(Clone, Copy)]
enum StaticFlaw {
    /// A row naming this board net (a floating control net).
    Net(&'static str),
    /// A `part_field_mismatch` row on this part whose ordering code orders
    /// `orders`, a value its Value field does not state.
    PartField {
        reference: &'static str,
        orders: &'static str,
    },
}

impl StaticFlaw {
    /// Whether the failing static row (`label`, `detail`) is this flaw.
    fn is_row(self, label: &str, detail: &str) -> bool {
        use hauksbee_ir::vocabulary::STATIC_CHECK_KIND;
        match self {
            Self::Net(net) => format!("{label} {detail}").contains(&format!("'{net}'")),
            Self::PartField { reference, orders } => {
                label == format!("{STATIC_CHECK_KIND}: lint part_field_mismatch on {reference}")
                    && detail.contains(&format!("orders {orders}"))
            }
        }
    }

    fn name(self) -> String {
        match self {
            Self::Net(net) => format!("'{net}'"),
            Self::PartField { reference, orders } => format!("{reference} (orders {orders})"),
        }
    }
}

/// Assert a fuzz whose own numeric judgement holds is RED because the board's
/// static check fails it on real flaws, each failing static row being one of
/// the `flaws` and every flaw having a failing row. When the board also has
/// parts with no model (`open_parts`), the incomplete evidence outranks the
/// failure in the exit code (INVALID, 3), as it does for any CI run.
fn assert_red_on_static_flaws(
    result: &hauksbee_ci::CiResult,
    flaws: &[StaticFlaw],
    open_parts: bool,
    case: &str,
) {
    use hauksbee_ir::vocabulary::STATIC_CHECK_KIND;
    let report = result.render_human();
    let expected = if open_parts {
        hauksbee_engine::result::EXIT_INVALID_FOR_ANALYSIS
    } else {
        1
    };
    assert_eq!(
        result.exit_code(),
        expected,
        "{case}: expected RED (INVALID with open parts)\n{report}"
    );
    assert!(
        !result.passed(),
        "{case}: the static flaws must fail the run\n{report}"
    );
    assert!(
        result
            .results
            .iter()
            .filter(|r| r.kind != STATIC_CHECK_KIND)
            .all(|r| r.passed),
        "{case}: numeric judgement\n{report}"
    );
    let failing: Vec<(&str, &str)> = result
        .results
        .iter()
        .filter(|r| r.kind == STATIC_CHECK_KIND && !r.passed && r.waived.is_none())
        .map(|r| (r.label.as_str(), r.detail.as_str()))
        .collect();
    for (label, detail) in &failing {
        assert!(
            flaws.iter().any(|f| f.is_row(label, detail)),
            "{case}: unexpected static row {label} {detail}"
        );
    }
    for flaw in flaws {
        assert!(
            failing
                .iter()
                .any(|(label, detail)| flaw.is_row(label, detail)),
            "{case}: no failing static row for {} in {failing:?}",
            flaw.name()
        );
    }
}

/// How many members a `seeds`-seed fuzz runs on `board`, and the conditions
/// its pass rests on: each seed crossed with the board's open same-pinout
/// variant and corner sets, as `hauksbee-ci` plans them
/// ([`hauksbee_ci::tolerance::sweep_open_sets`]). Counted off the board
/// independently of the run, so a run that skipped a seed or a corner cannot
/// match it.
fn expected_members(board: &Path, seeds: u32) -> (u32, Vec<String>) {
    let mut extracted = hauksbee_extract::ExtractedBoard::from_kicad_schematic_path(board)
        .expect("corpus schematic reads");
    // The run reads the board through the loader, which puts the parts in
    // canonical (natural designator) order; the open sets and the conditions
    // that name them follow that order, so the expectation must too.
    extracted.canonicalize();
    let lib = hauksbee_models::ModelLibrary::builtin();
    let open = hauksbee_engine::variant_sweep::open_sets(&extracted, &lib);
    let plans = vec![hauksbee_ci::tolerance::SeedPlan::default(); seeds as usize];
    let swept = hauksbee_ci::tolerance::sweep_open_sets(plans, &open);
    let members = u32::try_from(swept.plans.len()).expect("capped run count fits u32");
    (members, swept.coverage.conditions())
}

const fn part_field(reference: &'static str, orders: &'static str) -> StaticFlaw {
    StaticFlaw::PartField { reference, orders }
}

/// Every real flaw the static check reports on the `LumenPnP` motherboard.
///
/// The buzzer and fan FET gates (behind 1k series resistors) and two
/// light-buffer inputs are driven only by STM32F407 pads, which are floating
/// inputs in reset (RM0090 8.3.1) with nothing on those nets pulling them.
///
/// Sixteen resistors carry a Mouser ordering code that orders another value
/// than their Value field and their LCSC code state: the sheets were copied
/// from a neighbour and kept its Mouser and Digi-Key codes. R8, R54, R55 and
/// R56 are `0R`, R53 and R71 are `1k`, R9 is `4.7k` and R10 is `1k`, all with
/// the 10 k Vishay CRCW0805 code `71-CRCW080510K0FKEAC`; R41 to R48 (`1k` on
/// the odd designators, `10k` on the even) carry the 36 k code
/// `71-CRCW080536K0FKEA`. Read off the schematic sheets, not off the check.
const LUMENPNP_STATIC_FLAWS: [StaticFlaw; 20] = [
    StaticFlaw::Net("Net-(Q5-Pad1)"),
    StaticFlaw::Net("Net-(Q6-Pad1)"),
    StaticFlaw::Net("BTM_LIGHT"),
    StaticFlaw::Net("TOP_LIGHT"),
    part_field("R8", "10 kΩ"),
    part_field("R9", "10 kΩ"),
    part_field("R10", "10 kΩ"),
    part_field("R41", "36 kΩ"),
    part_field("R42", "36 kΩ"),
    part_field("R43", "36 kΩ"),
    part_field("R44", "36 kΩ"),
    part_field("R45", "36 kΩ"),
    part_field("R46", "36 kΩ"),
    part_field("R47", "36 kΩ"),
    part_field("R48", "36 kΩ"),
    part_field("R53", "10 kΩ"),
    part_field("R54", "10 kΩ"),
    part_field("R55", "10 kΩ"),
    part_field("R56", "10 kΩ"),
    part_field("R71", "10 kΩ"),
];

/// `LumenPnP` motherboard: the low-side AO3400A MOSFET gates are driven by
/// STM32F407 GPIOs that are Hi-Z at reset, behind a 1k series; the buzzer
/// and fan gates have no pull at all, which the static check reports. We fuzz each gate node directly across its undefined boot level
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
        // Model gap: the TMC2226 stepper drivers and the TPS54560 buck hang off
        // these rails with no model, and stay critical as active parts. The
        // open TVS/ESD parts beside them (PESD24BS3, SMF28A, SM712, SRV05-4)
        // stay critical too: no card states an open part's standoff or
        // polarity, so nothing bounds what it clamps. GREEN once all are modelled.
        &["U6", "U7", "U8", "U9", "U10", "U11", "U16"],
        // Real flaws the static check reports (see `LUMENPNP_STATIC_FLAWS`).
        &LUMENPNP_STATIC_FLAWS,
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
        // Every part binds (the Olimex peripheral cards), so the run is GREEN.
        &[],
        &[],
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
    let Some(result) = reform_supervisor_fuzz("reform_supervisor", 24) else {
        return;
    };
    // Six class-only LEDs (D8 D9 D11 D12 D22 D23), two assumed forward-voltage
    // corners each: 24 seeds x 2^6 = 1536 runs is over the 1024-run cap. That
    // used to run only the first-listed (high-Vf, least-current) stand-ins and
    // say nothing. Now each seed runs the linear cover - all weak, all strong,
    // each part flipped alone against the rest: 2 + 6 x 2 = 14 members - and
    // the pass says it was not the full product.
    assert_eq!(
        result.seeds,
        24 * 14,
        "24 seeds x the 14-member linear cover"
    );
    // One sweep condition; the rest say what the assertions' evidence rests
    // on (the parts with no model, the modelling assumptions).
    let [condition, evidence @ ..] = result.verdict_conditions.as_slice() else {
        panic!("a sweep condition: {:?}", result.verdict_conditions);
    };
    assert!(
        evidence.iter().all(|c| support::is_evidence_condition(c)),
        "one sweep condition: {:?}",
        result.verdict_conditions
    );
    for needle in [
        "6 parts the board names only by class",
        "all weak, all strong, and each alone against the rest",
        "every combination would be 1536 runs, over the 1024-run cap",
        "D8",
        "D23",
    ] {
        assert!(
            condition.contains(needle),
            "{needle:?} missing: {condition}"
        );
    }
    // Every surface carries it: the human log and the JSON document.
    assert!(
        result.render_human().contains("CONDITION: "),
        "the human log must name the condition"
    );
    assert!(result.render_json().contains("\"verdict_conditions\""));
}

/// The MNT Reform supervisor fuzz over `seeds` seeds, or `None` without the
/// corpus. Every boot state must solve: the run is INVALID only by the open
/// parts named below, never by an analog abort.
fn reform_supervisor_fuzz(case: &str, seeds: u32) -> Option<hauksbee_ci::CiResult> {
    let board = corpus_board(
        "mnt_reform/reform2-motherboard30-pcb/reform2-motherboard30.kicad_sch",
        "MNT Reform",
    )?;
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
    Some(boot_state_fuzz(
        case,
        &board,
        &[("VIN", 12.0), ("+5V", 5.0), ("+3V3", 3.3)],
        &enables,
        seeds,
        "",
        // Model gap: `no_faults` spans the whole board and ~30 parts have no
        // model, among them the supervisor U18 and the regulators U7, U9, U12
        // and U14, which stay critical as active parts. The open ESD arrays
        // (SP0503BAHT, TPD6E05U06) stay critical too: no card states an open
        // part's standoff or polarity. GREEN once all of them are modelled.
        &["U18", "U7", "U9", "U12", "U14"],
        // Real flaws the static check reports: C146 and C180 are `1uF` on a
        // 0402 pad with LCSC C1589 (a 1 uF Samsung part), but their
        // Manufacturer_No is CL05A104KA5NNNC, the 100 nF part, so the BOM
        // carries two numbers for two different capacitors.
        &[part_field("C146", "100 nF"), part_field("C180", "100 nF")],
    ))
}

/// Seed 0 alone, every regulator enable low: the boot state whose every chunk
/// failed on macOS (all its LED-corner members aborted INVALID) while Linux
/// passed. The disabled TLV62568's behavioral current node sat nanovolts from
/// its root, inside Newton's own step tolerance, and the transient false-root
/// gate read that as relative residual 1.0 or a pass depending on which side
/// of a kink round-off left the last iterate. Pinned alone so the case runs in
/// seconds rather than as one seed of 24.
#[test]
fn mnt_reform_all_enables_low_boot_state_solves() {
    let Some(result) = reform_supervisor_fuzz("reform_supervisor_seed0", 1) else {
        return;
    };
    assert!(!result.analog_abort, "{}", result.render_human());
}
