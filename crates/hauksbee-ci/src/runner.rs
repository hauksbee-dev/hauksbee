//! The headless runner: turn a [`Spec`] into a bound board, apply its supplies,
//! net drives, rail suppressions and overrides, run the co-sim for the
//! requested duration across one or more fuzz seeds, and collect everything the
//! assertions need (per-net min/max/toggles after a time threshold, UART,
//! faults, per-component peak current).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use hauksbee_engine::power_supply::{Chemistry, PowerSupply, SupplyLeg, UsbSpec};
use hauksbee_engine::{bind_board, BoundBoard, HauksbeeEngine};
use hauksbee_extract::ExtractedBoard;
use hauksbee_ir::{Device, NodeId, SourceKind};
use hauksbee_models::ModelLibrary;
use hauksbee_server::engine::Engine;

use crate::error::SpecError;
use crate::spec::{Spec, SupplySpec};

/// Per-net statistics collected over a run, sampled only at/after each
/// assertion's time threshold (tracked per threshold so `after_ms` is honored).
#[derive(Debug, Clone, Default)]
pub struct NetWindow {
    pub min_v: f64,
    pub max_v: f64,
    /// Last sampled voltage (the settled value).
    pub last_v: f64,
    pub samples: u64,
}

impl NetWindow {
    fn new() -> Self {
        NetWindow {
            min_v: f64::INFINITY,
            max_v: f64::NEG_INFINITY,
            last_v: 0.0,
            samples: 0,
        }
    }
    fn observe(&mut self, v: f64) {
        self.min_v = self.min_v.min(v);
        self.max_v = self.max_v.max(v);
        self.last_v = v;
        self.samples += 1;
    }
    /// Widen the min/max window with an intra-frame extreme WITHOUT touching
    /// `last_v`. The settled report value must stay the last-chunk voltage
    /// (`observe`), not the peak excursion, folding the extremes through
    /// `observe` would leave `last_v` reporting the max of the final frame.
    fn fold(&mut self, v: f64) {
        self.min_v = self.min_v.min(v);
        self.max_v = self.max_v.max(v);
        self.samples += 1;
    }
}

/// One fault raised during a run.
#[derive(Debug, Clone)]
pub struct RunFault {
    pub component: String,
    pub kind: String,
    pub value: f64,
    pub limit: f64,
    pub t_ms: f64,
}

/// A peripheral's end-of-run state, for `peripheral` assertions.
#[derive(Debug, Clone, Default)]
pub struct PeripheralSnapshot {
    /// Numeric state fields (temp_c, transitions, position, ...).
    pub fields: HashMap<String, f64>,
    /// Raw memory bytes for an EEPROM (empty otherwise), for `bytes` checks.
    pub bytes: Vec<u8>,
}

/// Everything one seed's run produced, indexed for assertion evaluation.
///
/// `Default` yields an empty outcome (no UART, no faults, empty maps,
/// `analog_valid: false`); real runs fill every field. Tests build a minimal
/// outcome with `RunOutcome { <fields under test>, ..Default::default() }`.
#[derive(Debug, Clone, Default)]
pub struct RunOutcome {
    /// Seed index (0-based).
    pub seed: u32,
    /// Per-net window keyed by (net, after_ms-millis-as-bits). We bucket by the
    /// distinct `after_ms` thresholds present in the spec.
    pub windows: HashMap<(String, u64), NetWindow>,
    /// Per-MCU UART output (lossy UTF-8).
    pub uart: HashMap<String, String>,
    /// All faults raised this run.
    pub faults: Vec<RunFault>,
    /// Per-net toggle counts over the run.
    pub toggles: HashMap<String, u64>,
    /// Per-component peak through-current magnitude (A), best-effort.
    pub peak_current: HashMap<String, f64>,
    /// Per-component peak steady-state junction temperature (C) over the run.
    /// Only dissipating devices appear.
    pub peak_temp_c: HashMap<String, f64>,
    /// Per-peripheral end-of-run snapshot, keyed by peripheral id.
    pub peripherals: HashMap<String, PeripheralSnapshot>,
    /// Per-scenario, per-net rail-window timeseries, keyed by (scenario id, net).
    /// The scenario id is "" for the run-wide window when a scenario has no id.
    pub rail_windows: HashMap<(String, String), crate::scenarios::RailWindow>,
    /// Whether each supply net's battery protection latched at any point in the
    /// run, keyed by net name.
    pub protection_tripped: HashMap<String, bool>,
    /// Whether each supply net's protection latched *within* a given scenario
    /// window, keyed by (scenario id, net). A trip counts for a window only if
    /// it first latched at or after that window's start time, so a
    /// `protection_trip` assertion scoped to a scenario ignores trips that
    /// happened before the scenario began. The scenario id is "" for the
    /// run-wide (unnamed) window.
    pub protection_tripped_scoped: HashMap<(String, String), bool>,
    /// Configured ambient temperature (°C) for the run. A non-dissipating
    /// device's junction sits here, so a `max_temp` ceiling below ambient must
    /// fail even when no dissipation was measured.
    pub ambient_c: f64,
    /// Total simulated time (ms).
    pub sim_ms: f64,
    /// Boot-coverage tracking, first time (ms) each watched (net, level-bits)
    /// pair was seen at or above its level, i.e. when the firmware first drove
    /// the control net to its defined level. NEVER forgotten once set, so a later
    /// drop cannot erase the fact that the net was reached: this is what lets the
    /// assertion tell "never reached" (absent here) apart from "reached then
    /// dropped" (present here, with a `boot_drop_after_cross_ms` entry). Absent =
    /// the net never reached the level (see `driven_nets` /
    /// `drive_direction_observable` to tell a driven-but-below-threshold net from
    /// a genuinely undriven one).
    pub boot_first_cross_ms: HashMap<(String, u64), f64>,
    /// The first time (ms) each pair fell back BELOW its level AFTER first
    /// crossing it (absent = it never dropped after crossing). With the first
    /// cross this decides "reached by the deadline AND held continuously through
    /// the deadline" purely from the boot window, independent of end-of-run
    /// state, so a legitimate post-deadline release does not fail the check and
    /// a late analog-failed frame cannot flip the verdict.
    pub boot_drop_after_cross_ms: HashMap<(String, u64), f64>,
    /// Nets the firmware drove to a *defined* level (HIGH or LOW) during the run,
    /// from the scheduler's `firmware_driven_nets`. Lets a below-threshold
    /// boot-coverage result say "driven but never exceeded X V" instead of
    /// falsely claiming the pin was left Hi-Z / undefined.
    pub driven_nets: std::collections::HashSet<String>,
    /// Whether every MCU backend in this run can report pin drive *direction*.
    /// True for the in-process AVR backend (reads DDR) and for Renode parts
    /// whose SoC descriptor maps each port's direction register (STM32F103
    /// CRL/CRH, STM32F4 MODER, nRF52840 DIR), on those a held-LOW pin is known
    /// driven. False for QEMU and unmapped Renode parts (drive state comes only
    /// from observed edges, so absence from `driven_nets` is ambiguous, not
    /// proof of Hi-Z). Keeps the "undriven" diagnosis from over-claiming on
    /// direction-blind backends.
    pub drive_direction_observable: bool,
    /// Time (ms) of the first stress fault this run, if any.
    pub first_fault_ms: Option<f64>,
    /// Small-signal AC results (seed-independent; the same for every seed). The
    /// Bode `(freq, mag_db, phase_deg)` table per net, plus loop-stability
    /// margins per net. Empty when the spec has no `[ac]` block.
    pub ac: Option<AcOutcome>,
    /// False once any chunk's analog solve failed to converge this run: the co-sim
    /// held stale voltages over `failed_windows` (05 §3b). A clean run reports
    /// `true` with an empty `failed_windows`.
    pub analog_valid: bool,
    /// Sim-time windows `[start_s, end_s)` where the analog solve failed, merged
    /// where consecutive. Assertion evaluation marks any assertion whose window
    /// overlaps one of these INVALID rather than pass/fail.
    pub failed_windows: Vec<(f64, f64)>,
    /// Windows solved by a fallback integration rung, with its stable method
    /// name. These are valid numbers, but their error budget must say which
    /// lower-fidelity method produced each span.
    pub fallback_windows: Vec<(f64, f64, String)>,
    /// Numerical qualification measured by this seed's actual scheduler run.
    /// `None` exists for synthetic unit-test outcomes that did not execute a
    /// solver, not as a zero-error claim.
    pub error_budget: Option<hauksbee_ir::evidence::ErrorBudget>,
    /// True once the analog solve failed `STRICT_CONSECUTIVE_FAILED_ABORT` chunks
    /// in a row at any point (the strict/CI hard-refusal condition). Drives exit 3
    /// on its own, even if no single assertion's window happened to overlap.
    pub analog_abort: bool,
    /// The tolerance-ensemble values this member ran with (empty when the spec
    /// declares no tolerances). This is the actionable failure artifact: a
    /// failing seed's report names these exact values, and `--seed N`
    /// reproduces them byte-identically.
    pub sampled_values: Vec<crate::tolerance::SampledValue>,
    /// Full `(t_s, volts)` waveforms for the nets any `hwtrace` assertion
    /// probes, sampled at the frame cadence (held-stale frames excluded, like
    /// every other analog aggregate). Empty when the spec has no hwtrace
    /// assertions, recording every net's series unconditionally would cost
    /// memory on long runs for nothing.
    pub net_series: HashMap<String, Vec<(f64, f64)>>,
    /// One message per MCU whose requested part was co-simulated on a substitute
    /// core (e.g. an STM32F411 run on an STM32F407 model). The `run` binary
    /// surfaces this on every honesty surface; the CI report must too, or a GREEN
    /// verdict silently vouches for firmware behaviour on the wrong silicon.
    pub substitutions: Vec<String>,
    /// Co-sim coverage warnings (U3): one message per dropped-ADC channel (the
    /// platform had no injection map, so the firmware never received the
    /// solved voltage) and per never-exercised bus peripheral (no matching
    /// controller modeled). Same channel discipline as `substitutions`: the
    /// report surfaces every entry, or a GREEN silently vouches for co-sim
    /// paths that never ran.
    pub coverage_warnings: Vec<String>,
    /// Measured per-backend edge/pulse capability at the chunk used.
    pub timing_coverage: Vec<hauksbee_engine::scheduler::TimingCoverage>,
    /// Timing claims the run could not honor. Any entry invalidates the whole
    /// strict result; these are not advisory warnings.
    pub timing_refusals: Vec<String>,
    /// Nets that name themselves a supply, carry a rail's worth of parts, and
    /// nothing powers. See [`dead_rails`]: the operating point around one is
    /// fiction, so every analog number in this outcome has to be read knowing
    /// they were dead.
    pub dead_rails: Vec<String>,
    /// Ids of bus peripherals that were bound but NEVER exercised (their
    /// platform models no matching bus controller). A `peripheral` assertion
    /// against one of these ids FAILS loudly instead of green-passing on the
    /// slave's power-on default state.
    pub unexercised_bus_ids: std::collections::HashSet<String>,
    /// Per-SPI-bus transaction-framing tier, keyed by bus/peripheral id:
    /// "exact", "backend", or "heuristic". A `peripheral` assertion against a
    /// heuristic-framed bus is flagged in its result detail, because the
    /// heuristic tier is documented actively wrong (merges two transactions in
    /// one chunk; truncates a boundary-spanning one).
    pub spi_framing: HashMap<String, String>,
    /// How much of the board bound to a real device model, from the binder.
    ///
    /// Analogue accuracy is capped by model availability, and part of that cap
    /// sits outside the project: vendors encrypt SPICE and IBIS models. So the
    /// number a board reaches has to be visible and gateable rather than a line
    /// in a report, or coverage falls silently the day a new part lands.
    /// `None` only in test-constructed outcomes, never in a real run.
    pub bind: Option<hauksbee_engine::result::BindSummary>,
    /// The production board/bind/scheduler evidence collected for this member.
    /// `None` only in narrow assertion-unit fixtures; every real runner outcome
    /// carries it and the top-level `run` path fails closed if it is absent.
    pub evidence: Option<hauksbee_engine::BoardEvidence>,
}

fn assertion_timing_refusals(
    spec: &Spec,
    coverage: &[hauksbee_engine::scheduler::TimingCoverage],
) -> Vec<String> {
    let has_toggle_assertion = spec.asserts.iter().any(|a| a.kind == "toggle");
    let pulse_floor_declared = spec.timing.and_then(|t| t.min_pulse_us).is_some();
    if !has_toggle_assertion || pulse_floor_declared {
        return Vec::new();
    }

    let mut poll_backends: Vec<String> = coverage
        .iter()
        .filter(|c| !c.cycle_exact)
        .map(|c| format!("{} ({})", c.mcu_ref, c.backend))
        .collect();
    poll_backends.sort();
    poll_backends.dedup();
    if poll_backends.is_empty() {
        Vec::new()
    } else {
        vec![format!(
            "timing claim refused: toggle assertion(s) run on poll backend(s) {} but no timing.min_pulse_us was declared; a poll can miss a pulse that rises and falls between samples",
            poll_backends.join(", ")
        )]
    }
}

/// The shared AC analysis outcome attached to every seed's run.
#[derive(Debug, Clone, Default)]
pub struct AcOutcome {
    /// Bode `(freq_hz, mag_db, phase_deg)` table per net (the transfer function
    /// from the unit AC stimulus to that net).
    pub bode: HashMap<String, Vec<(f64, f64, f64)>>,
    /// Loop-stability margins per net (T = -V_net convention), for any net a
    /// `phase_margin` assertion references.
    pub margins: HashMap<String, hauksbee_solve::StabilityMargins>,
}

/// How many parts must hang off a supply-named net before its silence counts as
/// a dead rail rather than an oddly-named signal. An enable called `VCC_EN`
/// reaches a handful of pins; a rail reaches the board.
const DEAD_RAIL_MIN_PARTS: usize = 6;

/// Nets that name themselves a supply, carry a rail's worth of parts, and are
/// powered by nothing.
///
/// The binder refuses to guess a voltage for `ANALOG_VDD` or bare `VDD`, which
/// is right: inventing one would over- or under-drive every part on the net.
/// What it did not do is say so. The net then sits at 0 V, the operating point
/// around it is fiction, and the stress monitor reports on that fiction with
/// the same confidence it reports a real overload. On the flagship board that
/// produced a named accusation against a specific 0402 resistor, from a run
/// where five of six rails were dead.
///
/// A first-time user cannot tell that report from a true one. That is the
/// asymmetry worth spending code on: a missed fault costs them a bug, a
/// confident false one costs them the tool.
///
/// Returns the net names, ordered, deduped.
pub fn unpowered_supply_nets(
    board: &ExtractedBoard,
    bound: &BoundBoard,
    also_powered: &[&str],
) -> Vec<String> {
    let mut powered: std::collections::BTreeSet<&str> = bound
        .supplies
        .iter()
        .map(|leg| leg.net_name.as_str())
        .collect();
    powered.extend(also_powered.iter().copied());

    // Distinct parts per net id, so a two-pin device straddling the net counts
    // once and a bypass-capacitor farm counts as the many parts it is.
    let mut parts_on: HashMap<i64, std::collections::BTreeSet<&str>> = HashMap::new();
    for comp in &board.components {
        for pin in &comp.pins {
            if let Some(id) = pin.net {
                parts_on
                    .entry(id)
                    .or_default()
                    .insert(comp.reference.as_str());
            }
        }
    }

    let mut out: Vec<String> = board
        .nets
        .iter()
        .filter(|net| !powered.contains(net.name.as_str()))
        .filter(|net| hauksbee_engine::names_a_supply_of_unknown_voltage(&net.name))
        .filter(|net| {
            parts_on
                .get(&net.id)
                .is_some_and(|p| p.len() >= DEAD_RAIL_MIN_PARTS)
        })
        .map(|net| net.name.clone())
        .collect();
    out.sort();
    out.dedup();
    out
}

/// [`unpowered_supply_nets`] for a run: anything the spec powers or drives has
/// had its question answered, whatever the name says.
fn dead_rails(board: &ExtractedBoard, bound: &BoundBoard, spec: &Spec) -> Vec<String> {
    let answered: Vec<&str> = spec
        .supplies
        .iter()
        .map(|s| s.net.as_str())
        .chain(spec.net_drives.iter().map(|d| d.net.as_str()))
        .collect();
    unpowered_supply_nets(board, bound, &answered)
}

/// Run the spec and return one [`RunOutcome`] per seed (>=1).
pub fn run_spec(spec: &Spec) -> Result<Vec<RunOutcome>, SpecError> {
    run_spec_seeded(spec, None)
}

/// RAII guard for the spec's `[mcu] descriptor_dir`: sets `HAUKSBEE_MCU_DIR`
/// (the env var `hauksbee_mcu::SocConfig::resolve` consumes) to the spec's
/// resolved descriptor directory for the guard's lifetime, and restores the
/// previous state on drop so a later spec in the same invocation does not
/// inherit it.
///
/// Precedence: an explicitly set, non-empty `HAUKSBEE_MCU_DIR` WINS over the
/// spec field; the guard then changes nothing. (The env var is the operator's
/// override of last resort; a spec must not be able to silently defeat it.)
///
/// The env var is process-global, so concurrent `run_spec*` calls in one
/// process must be serialized by the caller when any spec carries a
/// `descriptor_dir` (the CLI runs specs sequentially; tests take a lock).
pub(crate) struct DescriptorDirGuard {
    /// `Some(previous)` when we set the var and must restore `previous` on
    /// drop; `None` when the guard changed nothing.
    restore: Option<Option<std::ffi::OsString>>,
}

impl DescriptorDirGuard {
    pub(crate) fn apply(spec: &Spec) -> Self {
        let none = DescriptorDirGuard { restore: None };
        let Some(dir) = spec.mcu_descriptor_dir() else {
            return none;
        };
        let prev = std::env::var_os("HAUKSBEE_MCU_DIR");
        // A set-but-empty var is treated as unset, matching the consumer
        // (SocConfig's override_dirs skips an empty value).
        if prev.as_deref().is_some_and(|v| !v.is_empty()) {
            return none;
        }
        std::env::set_var("HAUKSBEE_MCU_DIR", &dir);
        DescriptorDirGuard {
            restore: Some(prev),
        }
    }
}

impl Drop for DescriptorDirGuard {
    fn drop(&mut self) {
        if let Some(prev) = self.restore.take() {
            match prev {
                Some(v) => std::env::set_var("HAUKSBEE_MCU_DIR", v),
                None => std::env::remove_var("HAUKSBEE_MCU_DIR"),
            }
        }
    }
}

/// Run the spec, optionally restricted to one ensemble seed (`--seed N`, the
/// failing-seed isolation path). Tolerance sampling and net fuzz are both
/// keyed by the absolute seed number, so the isolated member reproduces the
/// full run's values exactly.
///
/// The model library is the same layered one `hauksbee run` uses (builtin →
/// packs → `~/.hauksbee/models` → `~/.config/hauksbee/models`), so a
/// `[[models]]` routing entry in a user model dir binds in CI exactly as it
/// does interactively. For an explicit extra layer (the `--models-dir` flag)
/// use [`run_spec_with_lib`].
pub fn run_spec_seeded(spec: &Spec, only_seed: Option<u32>) -> Result<Vec<RunOutcome>, SpecError> {
    let lib = ModelLibrary::builtin_with_user_dirs(&[]);
    run_spec_with_lib(spec, only_seed, &lib)
}

/// [`run_spec_seeded`] against an explicit, already-built model library (how
/// the CLI threads `--models-dir` through).
pub fn run_spec_with_lib(
    spec: &Spec,
    only_seed: Option<u32>,
    lib: &ModelLibrary,
) -> Result<Vec<RunOutcome>, SpecError> {
    // `[mcu] descriptor_dir`: publish the spec's SoC-descriptor override dir
    // through the same channel the engine already consumes ($HAUKSBEE_MCU_DIR,
    // read by hauksbee_mcu::SocConfig::resolve), scoped to this run by the
    // guard's Drop. An explicitly set env var wins over the spec field.
    let _descriptor_dir = DescriptorDirGuard::apply(spec);
    // Read + extract the board once; clone per seed (binding mutates nothing on
    // the ExtractedBoard, but overrides do, so we re-derive per run).
    let board_path = spec.board_path();
    crate::progress::say(&format!("  reading {}", board_path.display()));
    let normalized = load_normalized_board(&board_path)?;
    let input_kind = normalized.kind;
    let input_raw = normalized.raw;
    let reader_notes = normalized.notes;
    let base = normalized.board;
    crate::progress::say(&format!(
        "  read {} components across {} nets",
        base.components.len(),
        base.nets.len()
    ));

    // Validate referenced nets against the board's net names before running.
    let known: Vec<String> = base.nets.iter().map(|n| n.name.clone()).collect();
    spec.check_nets(&known)?;

    // Validate any component reference the spec names (overrides + max_current
    // assertions) against the board, so a typo'd ref is a loud error rather
    // than a silently-green protection check.
    let known_refs: Vec<String> = base
        .components
        .iter()
        .map(|c| c.reference.clone())
        .collect();
    check_component_refs(spec, &known_refs)?;

    // Distinct after_ms thresholds (plus 0) that windows are bucketed by.
    let mut thresholds: Vec<f64> = spec.asserts.iter().filter_map(|a| a.after_ms).collect();
    thresholds.push(0.0);
    thresholds.sort_by(|a, b| a.partial_cmp(b).unwrap());
    thresholds.dedup();

    // Resolve the tolerance ensemble (if any) and lay out one plan per member.
    // With no tolerances this degenerates to the plain fuzz seed list.
    let tols = crate::tolerance::resolve(spec, &base)?;
    let plans: Vec<crate::tolerance::SeedPlan> = if tols.is_empty() {
        let seeds = spec.fuzz.as_ref().map(|f| f.seeds).unwrap_or(1).max(1);
        (0..seeds)
            .map(|seed| crate::tolerance::SeedPlan {
                seed,
                values: Vec::new(),
            })
            .collect()
    } else {
        match spec.ensemble_mode()? {
            crate::tolerance::Mode::MonteCarlo => crate::tolerance::build_plans(
                crate::tolerance::Mode::MonteCarlo,
                spec.ensemble_seed_count(),
                &tols,
            )?,
            crate::tolerance::Mode::Corners => {
                crate::tolerance::build_plans(crate::tolerance::Mode::Corners, 0, &tols)?
            }
        }
    };

    // Failing-seed isolation: keep only the requested member.
    let total = plans.len();
    let plans: Vec<crate::tolerance::SeedPlan> = match only_seed {
        Some(k) => {
            let sel: Vec<_> = plans.into_iter().filter(|p| p.seed == k).collect();
            if sel.is_empty() {
                return Err(SpecError::Invalid(format!(
                    "--seed {k} is outside this spec's ensemble (members are 0..{total})"
                )));
            }
            sel
        }
        None => plans,
    };

    // Small-signal AC analysis linearises about the DC operating point. That
    // point is seed-independent only when NEITHER toleranced values NOR per-seed
    // fuzz straps move the bias, then the sweep can be computed once and shared.
    // Toleranced values move the bias (each member gets its own sweep); so do
    // fuzz straps, which strap nets high/low per seed. Sharing one AC across
    // seeds that strap different nets would silently pin every member's AC to the
    // unstrapped nominal point.
    let fuzz_straps_present = plans
        .iter()
        .any(|p| !fuzz_net_drives(spec, p.seed).is_empty());
    let shared_ac = if spec.ac.is_some() && tols.is_empty() && !fuzz_straps_present {
        Some(compute_ac(spec, &base, &[], &[], lib)?)
    } else {
        None
    };

    let mut outcomes = Vec::with_capacity(plans.len());
    let member_count = plans.len();
    for plan in &plans {
        let mut outcome = run_one(
            spec,
            &base,
            &thresholds,
            plan,
            lib,
            member_count,
            &board_path,
            input_kind,
            &input_raw,
            &reader_notes,
        )?;
        outcome.ac = match &shared_ac {
            Some(ac) => Some(ac.clone()),
            None if spec.ac.is_some() => {
                let fuzz_drives = fuzz_net_drives(spec, plan.seed);
                Some(compute_ac(spec, &base, &plan.values, &fuzz_drives, lib)?)
            }
            None => None,
        };
        outcomes.push(outcome);
    }
    Ok(outcomes)
}

/// Run the spec's `[ac]` sweep on the biased circuit: bind, apply overrides /
/// supplies / net-drives so the DC operating point matches the run, then run the
/// AC analysis and collect Bode tables and loop margins for the nets the AC
/// assertions reference.
fn compute_ac(
    spec: &Spec,
    base: &ExtractedBoard,
    sampled: &[crate::tolerance::SampledValue],
    fuzz_drives: &[(String, f64)],
    lib: &ModelLibrary,
) -> Result<AcOutcome, SpecError> {
    use hauksbee_solve::{AcAnalysis, AcSpec, LoopStability, SolverOptions, Sweep};

    let cfg = spec.ac.as_ref().expect("compute_ac called without [ac]");
    let mut board = apply_overrides(spec, base)?;
    apply_sampled_values(&mut board, sampled);
    let mut bound = bind_board(&board, lib);

    // Bias the operating point exactly as the transient run would: rail
    // suppression, supplies, and net drives all shift the DC bias the
    // small-signal model linearises about.
    for net in &spec.suppress_rail {
        suppress_rail(&mut bound, net);
    }
    for s in &spec.supplies {
        attach_supply(&mut bound, s)?;
    }
    for d in &spec.net_drives {
        drive_net(&mut bound, &d.net, d.volts);
    }
    // Per-seed fuzz straps also move the DC bias the small-signal model
    // linearises about, so they must be applied here too, otherwise every fuzz
    // seed's AC would linearise about the unstrapped nominal point. Applied after
    // the spec net-drives (fuzz wins on an overlapping net), matching run_one.
    for (net, volts) in fuzz_drives {
        drive_net(&mut bound, net, *volts);
    }

    let sweep = match cfg.sweep.as_str() {
        "lin" => Sweep::Linear,
        _ => Sweep::Decade,
    };
    let ac_spec = AcSpec {
        fstart: cfg.fstart,
        fstop: cfg.fstop,
        points: cfg.points,
        sweep,
    };
    let resp = AcAnalysis::new(SolverOptions::default())
        .run(&bound.circuit, &ac_spec)
        .map_err(|e| SpecError::Invalid(format!("AC analysis: {e}")))?;

    // Collect the Bode and loop margins for every net an AC assertion names.
    let mut out = AcOutcome::default();
    for a in &spec.asserts {
        let Some(net) = &a.net else { continue };
        match a.kind.as_str() {
            "ac_gain" => {
                out.bode
                    .entry(net.clone())
                    .or_insert_with(|| resp.bode(&bound.circuit, net));
            }
            "phase_margin" => {
                if let Ok(st) = LoopStability::from_response(&resp, &bound.circuit, net) {
                    out.margins.insert(net.clone(), st.margins());
                }
                out.bode
                    .entry(net.clone())
                    .or_insert_with(|| resp.bode(&bound.circuit, net));
            }
            _ => {}
        }
    }
    Ok(out)
}

/// Load and extract the board file the spec points at, via the shared
/// board-input normalizer (`hauksbee_engine::board_input`). This is what makes
/// a `board = "thing.kicad_sch"` spec just work, and it accepts exactly the
/// format set every other surface does: `.kicad_pcb`, `.kicad_sch`
/// hierarchies, `.net`, Eagle `.brd`, IPC-D-356, binary Altium `.PcbDoc`,
/// gerber directories/zips, and Board-as-Code `.board` (bare or zipped).
///
/// A `.kicad_sch` is loaded *by path* (not from its text) so the sheet
/// hierarchy resolves: a schematic's sub-sheets live in sibling files, and only
/// the path-based entry point follows them. Loading the root text alone would
/// silently drop every component on a sub-sheet, producing a partial netlist
/// that passes vacuous checks.
///
/// `.board` needs no pre-compiled layout and no `--route` step: the compiled
/// text places footprints with net-named pads (full connectivity, no copper
/// tracks), and everything hauksbee-ci checks is netlist-driven.
pub(crate) fn load_board(board_path: &Path) -> Result<ExtractedBoard, SpecError> {
    load_normalized_board(board_path).map(|normalized| normalized.board)
}

fn load_normalized_board(
    board_path: &Path,
) -> Result<hauksbee_engine::board_input::NormalizedBoard, SpecError> {
    // CI-specific guard the normalizer does not carry: a spec pointing at a
    // sub-sheet rather than the hierarchy root is an incomplete board.
    let is_sch = board_path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("kicad_sch"))
        .unwrap_or(false);
    if is_sch {
        if let Some(root) = parent_schematic_of(board_path) {
            return Err(SpecError::Invalid(format!(
                "board {} is a sub-sheet of {}. Point the spec at the hierarchy \
                 root ({}) so the whole design is loaded, not one page of it",
                board_path.display(),
                root.display(),
                root.display(),
            )));
        }
    }

    use hauksbee_engine::board_input::{self, BoardInputError};
    board_input::from_path(board_path).map_err(|e| match e {
        // Re-word the file-system failures with the spec-relative context
        // (the path came from the spec's `board` key, not argv).
        BoardInputError::NotFound { path } => SpecError::Io(format!(
            "no board file at '{path}' (resolved from the spec's `board` key). \
                 Check that path; it is taken relative to the spec file's directory"
        )),
        BoardInputError::Io { path, message } => {
            SpecError::Io(format!("reading board {path}: {message}"))
        }
        BoardInputError::Schematic(e) => SpecError::Invalid(format!("extracting schematic: {e}")),
        BoardInputError::Extract(e) => SpecError::Invalid(format!("extracting board: {e}")),
        // Zip / gerber / Board-as-Code failures already carry a
        // self-contained, file-naming message.
        other => SpecError::Invalid(other.to_string()),
    })
}

/// Best-effort: is `sch` a sub-sheet referenced by another `.kicad_sch`? If so,
/// return the referencing (parent) file. We scan candidate schematics for a
/// `(property "Sheetfile" "<value>")` whose value's file name matches this
/// file, comparing by *basename* so a project that organizes sub-sheets in a
/// sub-directory (`"sheets/power.kicad_sch"`) is still caught. Candidates are
/// the schematics in this file's directory and its parent directory, which
/// covers flat and one-level-nested hierarchies (the layouts KiCad projects use
/// in practice).
///
/// This is a heuristic, not a full hierarchy parse, which is exactly why
/// pointing at a sub-sheet is reported as a clear error: the alternative is a
/// silent partial board. A hierarchy nested more than one directory deep can
/// slip past detection; that is a documented limitation, not a correctness bug
/// in the extraction.
fn parent_schematic_of(sch: &Path) -> Option<PathBuf> {
    let file_name = sch.file_name()?.to_str()?.to_string();
    // Normalize so the self-skip below is robust to `./` and similar.
    let sch_norm = normalize_path(sch);

    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(d) = sch.parent() {
        dirs.push(d.to_path_buf());
        if let Some(up) = d.parent() {
            dirs.push(up.to_path_buf());
        }
    }

    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if normalize_path(&p) == sch_norm {
                continue;
            }
            let is_sch = p
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("kicad_sch"))
                .unwrap_or(false);
            if !is_sch {
                continue;
            }
            if let Ok(text) = std::fs::read_to_string(&p) {
                if references_sheetfile(&text, &file_name) {
                    return Some(p);
                }
            }
        }
    }
    None
}

/// Does `text` contain a `(property "Sheetfile" "...")` whose value's file name
/// equals `file_name`? Matches by basename so subdir-qualified Sheetfile values
/// still match.
fn references_sheetfile(text: &str, file_name: &str) -> bool {
    let mut rest = text;
    let marker = "\"Sheetfile\" \"";
    while let Some(i) = rest.find(marker) {
        let after = &rest[i + marker.len()..];
        if let Some(end) = after.find('"') {
            let value = &after[..end];
            let base = Path::new(value)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(value);
            if base == file_name {
                return true;
            }
            rest = &after[end..];
        } else {
            break;
        }
    }
    false
}

/// Lexically normalize a path (resolve `.` / `..` segments, no IO) so two
/// spellings of the same file compare equal.
fn normalize_path(p: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Does this firmware image contradict its own extension? `Some(message)` when
/// it does, `None` when it agrees or cannot be read (the missing-file case has
/// its own, better error, and an unreadable file is not this function's story).
///
/// Both formats are trivially identifiable from their first bytes: an ELF opens
/// with `\x7fELF`, an Intel HEX record with an ASCII colon. A mismatch is a
/// renamed or mis-copied build artifact, and it has to be said HERE, because the
/// native ELF reader's answer is an unprefixed line on stderr from inside C plus
/// `rc=-1`, which names neither the file nor what is wrong with it.
pub(crate) fn firmware_format_mismatch(path: &Path) -> Option<String> {
    use std::io::Read;
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)?;
    let mut head = [0u8; 4];
    let read = std::fs::File::open(path)
        .and_then(|mut f| f.read(&mut head))
        .ok()?;
    let is_elf = read >= 4 && head == [0x7f, b'E', b'L', b'F'];
    let is_hex = read >= 1 && head[0] == b':';
    match ext.as_str() {
        "elf" if is_hex => Some(format!(
            "{} is named .elf but its contents are an Intel HEX file (it starts with \
             a ':' record, not the ELF magic). Rename it to .hex, or point `firmware` \
             at the .elf your build actually produced",
            path.display()
        )),
        "hex" if is_elf => Some(format!(
            "{} is named .hex but its contents are an ELF binary (it starts with the \
             ELF magic, not a ':' record). Rename it to .elf, or point `firmware` at \
             the .hex your build actually produced",
            path.display()
        )),
        _ => None,
    }
}

/// Validate every component reference the spec names against the board, with
/// near-match suggestions. Covers `[[override]]` refs and `max_current` assert
/// refs (overrides are also checked again in `apply_overrides`, but doing it
/// here means a typo'd `max_current` ref fails loudly instead of passing as an
/// untracked component).
fn check_component_refs(spec: &Spec, known_refs: &[String]) -> Result<(), SpecError> {
    let mut errs = component_ref_errors(spec, known_refs);
    match errs.len() {
        0 => Ok(()),
        1 => Err(errs.remove(0)),
        _ => Err(SpecError::Many(errs)),
    }
}

/// The collecting core of [`check_component_refs`]: EVERY unknown reference is
/// reported (one error per bad ref), not just the first, so one invocation
/// surfaces one invocation's worth of typos. Also feeds `hauksbee-ci check`'s
/// per-diagnostic output.
pub(crate) fn component_ref_errors(spec: &Spec, known_refs: &[String]) -> Vec<SpecError> {
    let set: std::collections::HashSet<&str> = known_refs.iter().map(String::as_str).collect();
    let mut named: Vec<(&str, &str)> = Vec::new();
    for ov in &spec.overrides {
        named.push((ov.reference.as_str(), "override"));
    }
    for a in &spec.asserts {
        if a.kind == "max_current" {
            if let Some(r) = &a.reference {
                named.push((r.as_str(), "max_current assert"));
            }
        }
        if a.kind == "max_temp" {
            if let Some(r) = &a.reference {
                named.push((r.as_str(), "max_temp assert"));
            }
        }
    }
    // A `[[decoupling.override]]` keyed by a ref that names no board capacitor is
    // silently dropped in apply_decoupling (the per-cap lookup never matches), so
    // the parasitics the user opted into are never applied and a rail_window
    // check sees a cleaner-than-real rail, a false GREEN. Validate the ref like
    // every other, so a typo fails loud.
    if let Some(dec) = &spec.decoupling {
        for ov in &dec.overrides {
            named.push((ov.reference.as_str(), "decoupling override"));
        }
    }
    // A SPI slave's `ref` names the board component the peripheral IS, and the
    // chip-select route reads that component's model `cs` pin role when the spec
    // declares no `cs_net`. A typo there resolves no component, so no model, so no
    // CS net, and the bus silently drops to the chunk-boundary framing heuristic
    // with no error: the same silent-degradation hole that `cs_net` itself had
    // (see the cs_net note in `Spec::referenced_nets`). Validate it like every
    // other reference so the typo fails at load.
    for p in &spec.peripherals {
        if crate::spec::is_spi_slave_kind(&p.kind) {
            if let Some(r) = &p.reference {
                named.push((r.as_str(), "SPI peripheral `ref`"));
            }
        }
    }
    let mut errs = Vec::new();
    for (reference, ctx) in named {
        if !set.contains(reference) {
            let near = crate::error::near_refs(reference, known_refs, 3);
            let hint = crate::error::suggestion_clause(&near);
            errs.push(SpecError::Invalid(format!(
                "{ctx} references unknown component '{reference}'{hint}"
            )));
        }
    }
    errs
}

/// Fail loud when a `max_current` / `max_temp` assertion names a component the
/// engine cannot actually measure. `check_component_refs` catches typos against
/// the board; this catches the subtler hole where the ref is a real component
/// but of a kind that is never tracked, so the guard would report a green pass
/// without ever being evaluated:
///
///   - peak current is recorded only for the device kinds the scheduler's
///     `accumulate_frame_peaks` walks (resistors and diodes, by exact device
///     name), so a `max_current` on a capacitor / IC / transistor package
///     would always take the "no current data" branch;
///   - junction temperature exists only for stress-monitored devices (the
///     binder's `DeviceMeta` list), so a `max_temp` on an unmonitored kind
///     (MCU, connector, anything the model library could not resolve) would
///     always take the "no dissipation measured" branch.
///
/// Add each scenario-scoped `protection_trip` assertion's `supply_net` to that
/// scenario's window `nets`. The scoped-trip verdict map only carries
/// (scenario, net) keys for a window's `nets`, seeded from the scenario's own
/// supply and rail_window nets, but a protection_trip may name a BATTERY rail the
/// scenario's load pulls current from, which is neither. Without adding it,
/// scope_protection_trips has no key and check_protection_trip returns a false RED
/// ("<net> was not a supply net in scenario window ... (nothing to trip)")
/// regardless of whether the pack actually latched.
fn merge_protection_trip_nets(windows: &mut [ScenarioWindow], asserts: &[crate::spec::Assertion]) {
    for a in asserts {
        if a.kind != "protection_trip" {
            continue;
        }
        let (Some(net), Some(scope)) = (&a.supply_net, a.scenario.as_ref()) else {
            continue;
        };
        if let Some(w) = windows.iter_mut().find(|w| &w.id == scope) {
            if !w.nets.contains(net) {
                w.nets.push(net.clone());
            }
        }
    }
}

/// A bound device `name` belongs to the bare spec ref `r`: an exact match, or a
/// multi-unit array unit `r_q<n>` / `r_s<n>` / `r_e<n>` (transistor / switch /
/// passive arrays). Mirrors thermally_tracked so max_current and max_temp agree,
/// before this, a package-level max_current on a resistor array (units RN1_e*)
/// was rejected as untrackable while the identical max_temp was accepted.
fn ref_or_unit_matches(r: &str, device_name: &str) -> bool {
    device_name == r
        || device_name
            .strip_prefix(r)
            .is_some_and(|s| s.starts_with("_q") || s.starts_with("_s") || s.starts_with("_e"))
}

/// Runs post-bind (the bound circuit is what decides trackability), before the
/// engine is built, so the spec is rejected up front rather than reported green.
fn check_trackable_assert_refs(spec: &Spec, bound: &BoundBoard) -> Result<(), SpecError> {
    // Current tracking covers exactly what `accumulate_frame_peaks` records:
    // resistors and diodes, keyed by device name.
    let current_tracked: std::collections::HashSet<&str> = bound
        .circuit
        .devices
        .iter()
        .filter_map(|d| match d {
            Device::Resistor { name, .. } | Device::Diode { name, .. } => Some(name.as_str()),
            _ => None,
        })
        .collect();
    // A multi-unit resistor/passive array (ref RN1) stamps per-unit devices
    // RN1_e1..RN1_e4, so `current_tracked` holds the unit names but NOT the bare
    // "RN1". Accept a bare ref whose units are tracked, matching thermally_tracked
    // below, otherwise a package-level max_current on an array is wrongly rejected
    // as untrackable while the identical max_temp is accepted.
    let is_current_tracked = |r: &str| {
        current_tracked
            .iter()
            .any(|name| ref_or_unit_matches(r, name))
    };
    // Thermal tracking covers every stress-monitored device. Multi-unit
    // packages stamp per-unit metas ("IC3906_q0"), so accept a ref whose units
    // are monitored too.
    let thermally_tracked = |r: &str| {
        bound.device_meta.iter().any(|m| {
            m.reference == r
                || m.reference.strip_prefix(r).is_some_and(|s| {
                    // The binder stamps multi-unit packages with `_q`/`_s`/`_e`
                    // suffixes (transistor arrays, switch banks, RESISTOR/passive
                    // arrays respectively). `_e` was omitted, so a package-level
                    // max_temp/max_current on a resistor array's bare ref was
                    // wrongly rejected as "no thermal model".
                    s.starts_with("_q") || s.starts_with("_s") || s.starts_with("_e")
                })
        })
    };
    for a in &spec.asserts {
        let Some(reference) = &a.reference else {
            continue;
        };
        match a.kind.as_str() {
            "max_current" if !is_current_tracked(reference.as_str()) => {
                return Err(SpecError::Invalid(format!(
                    "max_current assert references '{reference}', but peak current is only \
                     measured for resistors and diodes; this component binds as a kind whose \
                     through-current is never tracked, so the guard would report green without \
                     ever being evaluated; point the assert at a resistor/diode in the same \
                     path, or drop it"
                )));
            }
            "max_temp" if !thermally_tracked(reference) => {
                return Err(SpecError::Invalid(format!(
                    "max_temp assert references '{reference}', but it has no thermal model \
                     (it is not a stress-monitored device kind, or the model library could \
                     not resolve it), so its junction temperature is never estimated and the \
                     guard would report green without ever being evaluated"
                )));
            }
            // The celsius-less form ("<= device max") is only falsifiable when
            // the model carries a REAL datasheet Tj(max). On a part bound to a
            // generic fallback, the per-package default ceiling sits so high
            // the overpower monitor always trips first, so the assertion can
            // never fail: a gate that reads green while checking nothing.
            // Refuse at load, the same discipline as the untracked-ref
            // refusals above.
            "max_temp" if a.celsius.is_none() => {
                let has_real_tj = bound.device_meta.iter().any(|m| {
                    ref_or_unit_matches(reference, &m.reference)
                        && m.ratings.max_junction_temp_c.is_some()
                });
                if !has_real_tj {
                    return Err(SpecError::Invalid(format!(
                        "max_temp on '{reference}' needs an explicit `celsius`: {reference} \
                         is bound to a generic fallback model whose device max is a \
                         per-package default, not a datasheet limit, so the celsius-less \
                         form can never fail; add `celsius = <your ceiling in C>`"
                    )));
                }
            }
            _ => {}
        }
    }
    // protection_trip can only observe a supply whose model carries a
    // protection latch: today that is a `battery` supply with an explicit
    // `protection_trip_a`. Every other leg (ideal / bench / wall / usb, or a
    // battery without the trip fields) reports `tripped = false` forever, so
    // "protection held" would pass green while the rail is overloaded 10x.
    // Same discipline as the refusals above: a guard that can never fire must
    // error at load, not pass.
    for a in &spec.asserts {
        if a.kind != "protection_trip" {
            continue;
        }
        let Some(net) = &a.supply_net else {
            continue; // Spec::load already rejected the missing-net form.
        };
        let supply = spec.supplies.iter().find(|s| &s.net == net);
        let protected =
            supply.is_some_and(|s| s.kind == "battery" && s.protection_trip_a.is_some());
        if !protected {
            let why = match supply {
                None => "no [[supply]] is configured on that net, so it is an ideal rail with \
                         no protection model"
                    .to_string(),
                Some(s) if s.kind == "battery" => {
                    "the battery supply on that net has no `protection_trip_a`, so it is an \
                     unprotected pack"
                        .to_string()
                }
                Some(s) => format!(
                    "the `{}` supply on that net models its current limit as voltage foldback, \
                     not a latching protection trip",
                    s.kind
                ),
            };
            return Err(SpecError::Invalid(format!(
                "protection_trip assert references supply net '{net}', but {why}; the trip \
                 state can never become true, so the guard would report green without ever \
                 being evaluated; add `protection_trip_a` (and `protection_delay_ms`) to a \
                 battery [[supply]] on '{net}', or drop the assert"
            )));
        }
    }
    Ok(())
}

/// Apply overrides for this run to a fresh copy of the extracted board.
fn apply_overrides(spec: &Spec, base: &ExtractedBoard) -> Result<ExtractedBoard, SpecError> {
    let mut board = base.clone();
    for ov in &spec.overrides {
        let comp = board
            .components
            .iter_mut()
            .find(|c| c.reference == ov.reference)
            .ok_or_else(|| {
                let refs: Vec<String> = base
                    .components
                    .iter()
                    .map(|c| c.reference.clone())
                    .collect();
                let near = crate::error::near_refs(&ov.reference, &refs, 3);
                let hint = crate::error::suggestion_clause(&near);
                SpecError::Invalid(format!(
                    "override references unknown component '{}'{hint}",
                    ov.reference
                ))
            })?;
        comp.value = ov.value.clone();
    }
    // The DNP policy is board state too, and it decides whether a part is
    // stamped at all, so it lands at this same pre-bind seam.
    board
        .apply_dnp_policy(spec.dnp.into(), &spec.fit, &spec.no_fit)
        .map_err(|e| SpecError::Invalid(format!("dnp: {e}")))?;
    Ok(board)
}

/// Write one ensemble member's sampled tolerance values onto the board (the
/// same pre-binding seam `apply_overrides` uses). Every reference was resolved
/// against the board in `tolerance::resolve`, so lookups here cannot miss.
///
/// The value is serialized with `{:?}` (not `{}`): `format!("{}", 1210.0)`
/// yields "1210", which the component-value parser reads as a 4-digit imperial
/// footprint SIZE CODE (1206, 1210, 2512, ...) and rejects, silently leaving
/// the nominal member at its original value. `{:?}` always emits a decimal
/// point ("1210.0"), so the size-code heuristic never fires and the value
/// round-trips. `{:?}` is round-trippable for f64 and its scientific form (for
/// very small values) is accepted by the same parser.
fn apply_sampled_values(board: &mut ExtractedBoard, sampled: &[crate::tolerance::SampledValue]) {
    for sv in sampled {
        if let Some(comp) = board
            .components
            .iter_mut()
            .find(|c| c.reference == sv.reference)
        {
            comp.value = format!("{:?}", sv.si);
        }
    }
}

fn run_one(
    spec: &Spec,
    base: &ExtractedBoard,
    thresholds: &[f64],
    plan: &crate::tolerance::SeedPlan,
    lib: &ModelLibrary,
    member_count: usize,
    board_path: &Path,
    input_kind: hauksbee_engine::board_input::InputKind,
    input_raw: &[u8],
    reader_notes: &[String],
) -> Result<RunOutcome, SpecError> {
    let seed = plan.seed;
    let mut board = apply_overrides(spec, base)?;
    apply_sampled_values(&mut board, &plan.values);
    let mut bound = bind_board(&board, lib);

    // The as-built overlay comes first: it is BOARD state (the physical rework
    // record, cuts, jumpers, fitted values), so it lands before any harness
    // attachment, at the same post-bind seam the engine CLI's --asbuilt uses.
    if let Some(asbuilt_path) = spec.asbuilt_path() {
        let overlay = hauksbee_engine::asbuilt::AsBuiltOverlay::load(&asbuilt_path)
            .map_err(|e| SpecError::Invalid(e.to_string()))?;
        overlay
            .apply(&mut bound)
            .map_err(|e| SpecError::Invalid(e.to_string()))?;
    }

    // A spec that names firmware on a board with no processor cannot be
    // evaluated: nothing executes, so every firmware assertion passes without
    // ever being tested. In a pipeline that is the most expensive failure the
    // tool can have, so it is a spec error, not a warning.
    if spec.firmware.is_some() && bound.mcus.is_empty() {
        return Err(SpecError::Invalid(
            hauksbee_engine::binder::no_processor_message(
                &bound.dnp_mcus,
                hauksbee_engine::binder::FitRemedy::Spec,
            ),
        ));
    }

    // Reject max_current/max_temp asserts on components the bound circuit
    // cannot measure (untracked kinds), before any simulation runs: an
    // unevaluated guard must be a loud error, never a green pass.
    check_trackable_assert_refs(spec, &bound)?;

    // 0. Apply opt-in capacitor parasitics (ESR/ESL) before anything else, so the
    //    decoupling is honest for the whole run. Off by default.
    apply_decoupling(spec, &board, &mut bound)?;

    // 1. Suppress auto-rails on requested nets: drop the supply leg and turn its
    //    internal Vsource into an open so the net is fed only via board parts.
    for net in &spec.suppress_rail {
        suppress_rail(&mut bound, net);
    }

    // 2. Attach / reconfigure power supplies.
    for s in &spec.supplies {
        attach_supply(&mut bound, s)?;
    }

    // Everything that can power a rail has now been applied, so anything still
    // unpowered is unpowered for the whole run. Any analog number below has to
    // be read knowing it.
    let dead_rails = dead_rails(&board, &bound, spec);

    // 3. Fuzz the per-seed initial states by choosing which fuzzed nets are
    //    strapped high vs low. We then express these as net drives.
    let fuzz_drives = fuzz_net_drives(spec, seed);

    // 4. Drive nets (explicit + fuzzed) to fixed voltages via ideal sources.
    for (net, volts) in spec
        .net_drives
        .iter()
        .map(|d| (d.net.clone(), d.volts))
        .chain(fuzz_drives)
    {
        drive_net(&mut bound, &net, volts);
    }

    // Hollow-gate honesty: a voltage/rail_window assertion reading a net that
    // an IDEAL source (a declared `kind = "ideal"` leg, a binder auto-rail, or
    // a [[net_drive]]) feeds directly cannot fail for a board reason: the
    // source holds the net at its programmed voltage regardless of what the
    // board does. Behavioral legs (bench/wall/usb/battery) are exempt: their
    // current limits and droop are exactly what such an assertion tests.
    let hollow_warnings = hollow_gate_warnings(spec, &bound);
    let hollow_assumptions = hollow_gate_assumptions(spec, &bound);

    // Map net name -> node for fast sampling, and remember which components are
    // monitorable for peak-current (resistors/diodes by name).
    let net_node: HashMap<String, NodeId> = bound.net_nodes.clone();

    // Resolve the firmware path to an absolute path before handing it to the
    // engine: the Renode backend passes the path verbatim to Renode's
    // `sysbus LoadELF @<path>`, and Renode resolves relative paths against
    // its own working directory (a temp dir), not the repo root. Canonicalize
    // here so ELF loading works regardless of where the CLI is invoked from.
    let firmware = match spec.firmware_path() {
        Some(p) => {
            // The spec's `firmware` may be a PlatformIO project directory, a
            // built .pio tree, or a zip of either, parity with `run
            // --firmware` and the web drop zone, so the same repo layout works
            // in a pipeline. Resolve it to the compiled image first; a bare
            // .elf/.hex passes through untouched (resolve returns None).
            let p = match hauksbee_engine::firmware_input::resolve_firmware_cli(&p) {
                Ok(Some(resolved)) => {
                    eprintln!("  firmware: {}", resolved.note);
                    resolved.path
                }
                Ok(None) => p,
                Err(e) => {
                    return Err(SpecError::Invalid(format!(
                        "resolving the spec's `firmware = \"{}\"`: {e}",
                        spec.firmware
                            .as_ref()
                            .map(|f| f.display().to_string())
                            .unwrap_or_default()
                    )))
                }
            };
            // Validate before the native loader sees it (a missing file segfaults
            // simavr/QEMU/Renode, exit 139). Name the spec field and what the path
            // was resolved relative to: the bundled blinky.toml's firmware is
            // spec-relative three levels up, so it breaks the moment the spec is
            // copied elsewhere, and the message must make that obvious.
            hauksbee_engine::validate_firmware_path(&p).map_err(|e| {
                let field = spec
                    .firmware
                    .as_ref()
                    .map(|f| f.display().to_string())
                    .unwrap_or_default();
                SpecError::Io(format!(
                    "{e}\n  (from the spec's `firmware = \"{field}\"`, resolved relative \
                     to the spec file at {})",
                    spec.base_dir.display()
                ))
            })?;
            // Existence is guaranteed; now check the file is the FORMAT its
            // extension claims, before the native loader is handed it. The C
            // ELF reader prints its own unprefixed line to stderr and returns a
            // bare `rc=-1`, so a .hex renamed .elf surfaced as "Unexpected ELF
            // file type" from nowhere followed by "elf_read_firmware failed
            // (rc=-1)" - two lines, neither of which names the actual problem.
            if let Some(msg) = firmware_format_mismatch(&p) {
                return Err(SpecError::Io(msg));
            }
            // Canonicalize for Renode (which resolves relative paths against
            // its own temp working directory).
            Some(p.canonicalize().unwrap_or(p))
        }
        None => None,
    };

    // Capture the QEMU backend strings before `bound` is consumed by
    // `from_bound`. We use these below to warn when bus-slave peripherals or
    // declarative sensors are attached to a QEMU-backed board: the QEMU I2C/SPI
    // bridge is deferred, so those slaves will silently not respond.
    // AVR (simavr) and Renode backends have a working bus bridge, no warning.
    let qemu_backends: Vec<String> = bound
        .mcus
        .iter()
        .filter(|m| m.backend.starts_with("qemu:"))
        .map(|m| m.backend.clone())
        .collect();

    let mut engine = HauksbeeEngine::from_bound(bound, firmware.as_deref(), "/ci")
        .map_err(|e| SpecError::Invalid(format!("building engine: {e}")))?;

    // Ambient for the steady-state junction-temperature estimate (max_temp).
    engine.scheduler_mut().set_ambient_c(spec.ambient_c);

    // Attach this spec's peripherals (controls, bus slaves, sinks) and their
    // timeline events to the engine's scheduler.
    // In a multi-member fuzz/tolerance ensemble, make each seed's VCD path
    // distinct so per-seed waveforms are not all overwritten to one fixed file.
    let vcd_seed = (member_count > 1).then_some(seed);
    let vcd_targets = attach_peripherals(
        spec,
        &board,
        &net_node,
        engine.scheduler_mut(),
        lib,
        vcd_seed,
    )?;

    // Attach declarative sensors (RegisterMapSensor) to their buses.
    attach_sensors(spec, engine.scheduler_mut())?;

    // Warn when bus-slave peripherals or declarative sensors are attached on a
    // QEMU backend. The QEMU I2C/SPI bus bridge is not yet implemented, so
    // these slaves are a no-op: the firmware's bus transactions will time-out or
    // receive garbage, potentially causing failures that look unrelated to the
    // missing sensor. Surface the mismatch now, before the co-sim starts, so the
    // user doesn't spend 45 minutes chasing an unrelated assertion failure.
    //
    // AVR (simavr) and Renode backends have a working bus bridge, no warning.
    for msg in qemu_bus_slave_warnings(spec, &qemu_backends) {
        eprintln!("{msg}");
    }

    // Attach this spec's transient scenarios: dynamic load profiles stamped as
    // current sinks on the parts' supply nets.
    let scenario_windows = attach_scenarios(spec, &board, &net_node, engine.scheduler_mut())?;

    // Run the co-sim, sampling each frame.
    let frame_dt = (spec.frame_ms / 1000.0).max(1e-6);
    let total_s = spec.duration_ms / 1000.0;
    let mut t = 0.0;

    // On an external, wall-bounded emulator backend (QEMU/Renode), the analog
    // chunk must be coarse or the run drowns in TCP round-trips: the 100 us
    // default that suits the in-process AVR core would sub-divide each frame
    // into dozens of QMP cont/stop pairs, each with a wall-time floor. Match the
    // chunk to the frame (a few ms) the way the proven QEMU co-sim tests do; the
    // analog LED/RC settling on these boards is sub-microsecond, so a frame-sized
    // chunk still resolves the operating point each sample. The in-process AVR
    // path keeps its fine default.
    if engine.scheduler().has_external_backend() {
        engine.scheduler_mut().chunk_s = frame_dt.clamp(1e-3, 10e-3);
    }
    // Negotiate only after the external-backend performance default is known,
    // so the timing contract refines the chunk rather than being overwritten.
    // Keep running on refusal to produce diagnostics, but carry the refusal as
    // an INVALID result: no assertion may green over an unmet timing contract.
    let timing_configuration_refusal = spec.timing.and_then(|timing| {
        engine
            .scheduler_mut()
            .configure_timing(timing.requirement())
            .err()
            .map(|e| e.to_string())
    });

    let mut windows: HashMap<(String, u64), NetWindow> = HashMap::new();
    let mut uart: HashMap<String, String> = HashMap::new();
    let mut faults: Vec<RunFault> = Vec::new();
    let mut peak_current: HashMap<String, f64> = HashMap::new();
    let mut peak_temp_c: HashMap<String, f64> = HashMap::new();
    // Per-scenario rail-window timeseries and protection-trip tracking.
    let mut rail_windows: HashMap<(String, String), crate::scenarios::RailWindow> = HashMap::new();
    let mut protection_tripped: HashMap<String, bool> = HashMap::new();
    // First simulated time (s) each supply net's protection latched, so a
    // scenario-scoped `protection_trip` assertion can tell a pre-scenario trip
    // apart from one that fires inside its window.
    // Every sim-time at which a supply's protection NEWLY latched, per net. A
    // battery BMS runs in hiccup mode; it trips, the load drops below reset so
    // it re-arms, and it can trip again, so a supply shared across sequential
    // scenarios may latch once per window. Recording only the first latch time
    // (an `or_insert` keyed by net) would attribute a re-trip to the earliest
    // window and lose it in every later one. We keep the full list of latch
    // instants and per-net edge state to detect each new latch.
    let mut protection_trip_t: HashMap<String, Vec<f64>> = HashMap::new();
    let mut prot_prev_tripped: HashMap<String, bool> = HashMap::new();
    let mut prot_prev_ever: HashMap<String, bool> = HashMap::new();

    // Boot-coverage watch list: every (net, required-level) a boot-coverage
    // assertion names. We record the first frame each net reaches its level.
    let boot_watch: Vec<(String, f64)> = spec
        .asserts
        .iter()
        .filter(|a| a.kind == "boot_coverage" || a.kind == "boot-coverage")
        .filter_map(|a| Some((a.net.clone()?, a.min?)))
        .collect();
    let mut boot_first_cross_ms: HashMap<(String, u64), f64> = HashMap::new();
    let mut boot_drop_after_cross_ms: HashMap<(String, u64), f64> = HashMap::new();
    let mut first_fault_ms: Option<f64> = None;

    // Hardware-trace watch list: the nets any `hwtrace` assertion's trace.toml
    // probes. Loading the traces here (fail-loud, before the sim spends its
    // minutes) also validates them, so a malformed trace aborts the run with a
    // named error instead of failing every feature at evaluation time.
    let hwtrace_nets = crate::hwtrace::assert_nets(spec)?;
    let mut net_series: HashMap<String, Vec<(f64, f64)>> = HashMap::new();

    // The transient is where the minutes go, so it is the phase worth reporting
    // on. Label it with the ensemble member when there is more than one, since
    // otherwise the bar appears to restart from zero for no visible reason.
    let mut tick = crate::progress::Ticker::new(if member_count > 1 {
        format!("simulating (member {} of {member_count})", plan.seed + 1)
    } else {
        "simulating".to_string()
    });

    while t < total_s - 1e-12 {
        tick.at(t / total_s);
        let frame = engine.step(frame_dt);
        let t_ms = frame.t * 1000.0;

        // Analog-validity gate for this frame (05 §3b). By the time `step`
        // returns, the scheduler has recorded any chunk in this frame whose
        // analog solve failed (its sim-time window is in `failed_windows()`). If
        // this frame's covered span `[frame.t - frame_dt, frame.t)` overlaps one,
        // its end-of-frame node voltages are held-stale, not a real solve. We must
        // NOT fold them into the analog aggregates (voltage/rail windows,
        // boot-coverage, peak current/temperature) or they would manufacture a
        // settled value / a boot-reach / a peak from a solve that never happened.
        //
        // Per-frame query (over reconciling at the end) is the cheaper honest
        // design: the aggregates are running reductions, so once a stale sample is
        // folded in it cannot be subtracted back out; the windows list is small
        // and merged, so the overlap test is O(1)-ish per frame. UART and faults
        // are NOT gated: UART is digital MCU output independent of the analog
        // solve, and faults already only arise on converged chunks (the scheduler
        // skips the stress monitor on a failed chunk).
        let frame_start_s = (frame.t - frame_dt).max(0.0);
        let analog_ok =
            !windows_overlap(engine.scheduler().failed_windows(), frame_start_s, frame.t);

        // UART accumulation.
        for (mcu, bytes) in &frame.uart {
            uart.entry(mcu.clone())
                .or_default()
                .push_str(&String::from_utf8_lossy(bytes));
        }
        // Faults.
        for f in &frame.faults {
            let ft_ms = f.t * 1000.0;
            first_fault_ms = Some(first_fault_ms.map_or(ft_ms, |e| e.min(ft_ms)));
            faults.push(RunFault {
                component: f.component.clone(),
                kind: f.kind.clone(),
                value: f.value,
                limit: f.limit,
                t_ms: ft_ms,
            });
        }
        // Boot-coverage: record each watched net's FIRST crossing of its level
        // and its first drop back below level AFTER that crossing. The assertion
        // decides "reached by the deadline and held continuously through it" from
        // those two times, a one-frame glitch that collapses leaves a drop
        // record, and a legitimate post-deadline release (drop after the
        // deadline) still passes. Skipped on a held-stale frame so a stale
        // voltage cannot fake a reach or a drop.
        if analog_ok {
            for (net, level) in &boot_watch {
                let key = (net.clone(), level.to_bits());
                if let Some(&v) = frame.net_voltages.get(net) {
                    boot_reach_update(
                        &mut boot_first_cross_ms,
                        &mut boot_drop_after_cross_ms,
                        key,
                        v,
                        *level,
                        t_ms,
                    );
                }
            }
            // Per-net windows for every threshold this frame has passed. The
            // frame's final-chunk voltage is one sample; fold in the scheduler's
            // per-frame min/max too, so an intra-frame excursion that has subsided
            // by the last chunk still widens the window (the final-chunk value is
            // itself within [min,max], so observing both extremes subsumes it).
            let vext = engine.scheduler().frame_v_extremes();
            for &thr in thresholds {
                if t_ms + 1e-9 >= thr {
                    for (name, &v) in &frame.net_voltages {
                        let key = (name.clone(), thr.to_bits());
                        let w = windows.entry(key).or_insert_with(NetWindow::new);
                        w.observe(v);
                        if let Some(&(mn, mx)) = vext.get(name) {
                            // Widen the window with the extremes but keep last_v
                            // as the settled final-chunk value (`observe` above).
                            if mn.is_finite() {
                                w.fold(mn);
                            }
                            if mx.is_finite() {
                                w.fold(mx);
                            }
                        }
                    }
                }
            }
            // Hardware-trace waveforms: record the probed nets' voltages at the
            // frame cadence, for feature extraction against the captured trace.
            for net in &hwtrace_nets {
                if let Some(&v) = frame.net_voltages.get(net) {
                    net_series
                        .entry(net.clone())
                        .or_default()
                        .push((frame.t, v));
                }
            }

            // Peak current for monitored components. Fold the scheduler's
            // per-frame peak (accumulated across every sub-chunk of this frame),
            // not just the frame's final-chunk operating point, an inrush surge
            // that peaks mid-frame and settles by the last chunk would otherwise
            // be invisible to the over-current check.
            for (name, &i) in engine.scheduler().frame_peak_current() {
                let e = peak_current.entry(name.clone()).or_insert(0.0);
                if i.is_finite() && i > *e {
                    *e = i;
                }
            }

            // Peak steady-state junction temperature for dissipating components.
            for (reference, tj) in engine.scheduler().temp_states() {
                let e = peak_temp_c.entry(reference).or_insert(f64::NEG_INFINITY);
                if tj.is_finite() && tj > *e {
                    *e = tj;
                }
            }

            // Scenario rail windows: for each scenario window active at this time,
            // record the referenced rails' voltages into the window timeseries.
            // Bounded [start_s, end_s): scenarios are sequential phases, so a
            // rail_window scoped to an earlier scenario must stop sampling when the
            // next scenario begins, otherwise a later phase's excursion bleeds
            // into this window's min/max/dip/recovery aggregates and produces a
            // false verdict (the same later-scenario bleed the scoped
            // protection_trip windows guard against). The run-wide window keeps
            // end_s = +∞.
            for sw in &scenario_windows {
                if time_in_window(frame.t, sw.start_s, sw.end_s) {
                    for net in &sw.nets {
                        if let Some(&v) = frame.net_voltages.get(net) {
                            let w = rail_windows
                                .entry((sw.id.clone(), net.clone()))
                                .or_default();
                            w.observe(frame.t, v);
                            // Fold the scheduler's per-frame intra-frame extremes
                            // into the min/max envelope, exactly as the plain
                            // `voltage` assertion path above does, otherwise a sag
                            // that recovers by the frame's last chunk is invisible
                            // and a brownout-floor rail_window assertion false-
                            // passes the fault it exists to catch.
                            if let Some(&(mn, mx)) = vext.get(net) {
                                if mn.is_finite() {
                                    w.fold(mn);
                                }
                                if mx.is_finite() {
                                    w.fold(mx);
                                }
                            }
                        }
                    }
                }
            }
        }
        // Protection-trip tracking. Record EACH new latch instant, not just the
        // first, so a re-armed supply's second trip in a later scenario window is
        // not lost. Two edges signal a new latch:
        //   * the non-sticky `protection_tripped()` rising false→true (a re-trip
        //     the sampler actually observed), and
        //   * the sticky `protection_ever_tripped()` rising for the first time,
        //     catching a trip+re-arm that happened entirely within one coarse
        //     frame, which the non-sticky sample would miss (the reason the sticky
        //     flag was read here originally).
        for leg in &engine.scheduler().supplies {
            let net = &leg.net_name;
            let ever = leg.supply.protection_ever_tripped();
            let now = leg.supply.protection_tripped();
            let prev_now = prot_prev_tripped.get(net).copied().unwrap_or(false);
            let prev_ever = prot_prev_ever.get(net).copied().unwrap_or(false);
            if (now && !prev_now) || (ever && !prev_ever) {
                protection_trip_t
                    .entry(net.clone())
                    .or_default()
                    .push(frame.t);
            }
            prot_prev_tripped.insert(net.clone(), now);
            prot_prev_ever.insert(net.clone(), ever);
            if ever {
                protection_tripped.insert(net.clone(), true);
            } else {
                protection_tripped.entry(net.clone()).or_insert(false);
            }
        }

        // Refuse rather than fake (05 §3b): hauksbee-ci is inherently strict, so a
        // co-sim whose analog solve has been stuck for a whole streak of chunks
        // must not be asserted on. Stop the loop the moment the abort trips; the
        // check after the loop turns it into an exit-3 refusal rather than a
        // fake-green run against held-stale voltages.
        if engine.scheduler().analog_abort_tripped() {
            break;
        }

        t += frame_dt;
    }
    tick.done();

    // Analog-validity outcome (05 §3b). We do NOT `process::exit` here anymore:
    // the refusal is carried in the outcome and resolved at the `CiResult` layer,
    // so both the intermittent case (some failed chunks, no consecutive abort ->
    // any overlapping assertion is INVALID) and the hard case (the abort tripped)
    // route to exit 3 through the same testable path, rather than one killing the
    // process mid-run and the other never being reached.
    let analog_valid = engine.scheduler().analog_valid();
    let failed_windows: Vec<(f64, f64)> = engine.scheduler().failed_windows().to_vec();
    let analog_abort = engine.scheduler().analog_abort_tripped();
    if analog_abort {
        // Informational: the strict streak tripped. Exit 3 is enforced by
        // `CiResult::exit_code`, this line just explains why on stderr.
        eprintln!(
            "hauksbee-ci: analog co-sim failed to converge for {} chunks in a row \
             ({} failed chunks total); the run held stale voltages and cannot be \
             asserted on. Reporting INVALID (exit 3, 05 §3b).",
            hauksbee_engine::scheduler::STRICT_CONSECUTIVE_FAILED_ABORT,
            engine.scheduler().failed_chunk_count(),
        );
    }

    // Toggle counts from the scheduler's running stats. The scheduler only folds a
    // converged chunk into its stats (05 §3b), so these already exclude the failed
    // windows without any work here.
    let toggles = engine.scheduler().toggle_counts();

    // Snapshot peripheral state for assertions, and dump any VCD sinks.
    let peripherals = snapshot_peripherals(spec, &engine, &vcd_targets);

    // Drive-state metadata for the boot-coverage diagnosis: which nets the firmware
    // actually drove to a defined level, and whether the backends can report drive
    // direction. The in-process AVR backend always can (DDR hooks); a Renode
    // backend can once its SoC descriptor maps every polled port's direction
    // register (MODER / CRL+CRH / DIR, see db/mcu/*.soc.toml); QEMU and
    // unmapped Renode parts cannot, and stay conservative.
    let driven_nets: std::collections::HashSet<String> = engine
        .scheduler()
        .firmware_driven_nets()
        .into_iter()
        .collect();
    let drive_direction_observable = engine.scheduler().drive_direction_observable();

    let protection_tripped_scoped = scope_protection_trips(&protection_trip_t, &scenario_windows);

    let fallback_windows: Vec<(f64, f64, String)> = engine
        .scheduler()
        .fallback_windows()
        .iter()
        .map(|&(start, end, method)| (start, end, method.as_str().to_string()))
        .collect();
    let error_budget = Some(
        engine
            .scheduler()
            .error_budget()
            .map_err(|error| SpecError::Invalid(format!("building run error budget: {error}")))?,
    );
    let mut production_assumptions = hollow_assumptions;
    for drop in engine.scheduler().adc_dropped() {
        let scope = hauksbee_ir::evidence::Scope::Nets(
            hauksbee_ir::evidence::NetScope::new([drop.net.as_str()], None)
                .map_err(|e| SpecError::Invalid(format!("building ADC evidence: {e}")))?,
        );
        production_assumptions.push(hauksbee_ir::evidence::Assumption::not_exercised(
            hauksbee_ir::evidence::AssumptionSource::Scheduler,
            hauksbee_ir::evidence::Subject::new(
                &format!("adc/{}/{}", drop.mcu_ref, drop.channel),
                &format!("{} ADC channel {} on net {}", drop.mcu_ref, drop.channel, drop.net),
            ),
            scope,
            "the MCU backend has no ADC injection map, so the firmware never received the solved voltage",
            "add the ADC injection recipe to the SoC descriptor, then re-run",
        ));
    }
    for bus in engine.scheduler().unexercised_buses() {
        for assertion in spec
            .asserts
            .iter()
            .filter(|assertion| assertion.id.as_deref() == Some(bus.id.as_str()))
        {
            production_assumptions.push(hauksbee_ir::evidence::Assumption::not_exercised(
                hauksbee_ir::evidence::AssumptionSource::Scheduler,
                hauksbee_ir::evidence::Subject::new(
                    &format!("{}/{}", bus.bus.to_ascii_lowercase(), bus.id),
                    &format!("{} peripheral {}", bus.bus, bus.id),
                ),
                hauksbee_ir::evidence::Scope::Check {
                    check: "ci".into(),
                    kind: Some(assertion.label()),
                },
                "this MCU platform models no matching controller, so firmware traffic never reached it",
                "add the controller to the SoC descriptor, then re-run",
            ));
        }
    }
    let all_nets: Vec<&str> = board
        .nets
        .iter()
        .filter(|net| !net.name.trim().is_empty())
        .map(|net| net.name.as_str())
        .collect();
    for (start, end, method) in &fallback_windows {
        let scope = hauksbee_ir::evidence::Scope::Nets(
            hauksbee_ir::evidence::NetScope::new(
                all_nets.iter().copied(),
                Some(
                    hauksbee_ir::evidence::TimeWindow::new(*start, *end).map_err(|e| {
                        SpecError::Invalid(format!("building fallback evidence: {e}"))
                    })?,
                ),
            )
            .map_err(|e| SpecError::Invalid(format!("building fallback evidence: {e}")))?,
        );
        production_assumptions.push(hauksbee_ir::evidence::Assumption::reduced_fidelity(
            hauksbee_ir::evidence::AssumptionSource::Solver,
            hauksbee_ir::evidence::Subject::new(
                &format!("fallback/{method}/{start:.9}-{end:.9}"),
                &format!("the analog solve over {start:.6}-{end:.6} s"),
            ),
            scope,
            &format!("the primary integration failed and the {method} fallback produced the accepted values"),
            "resolve the convergence cause so the primary trapezoidal march carries this window, then re-run",
        ));
    }
    let mut evidence = hauksbee_engine::BoardEvidence::from_bound(
        &board,
        engine.report(),
        reader_notes,
        hauksbee_ir::evidence::RunDate::from_system_clock(),
    )
    .and_then(|evidence| evidence.with_input_artifact(board_path, input_raw, input_kind))
    .and_then(|evidence| evidence.with_substitutions(engine.scheduler().substitutions()))
    .and_then(|evidence| evidence.with_assumptions(production_assumptions))
    .map_err(|e| SpecError::Invalid(format!("building run evidence: {e}")))?;
    if let Some(path) = firmware.as_deref() {
        let bytes = std::fs::read(path).map_err(|error| {
            SpecError::Io(format!(
                "reading firmware evidence '{}': {error}",
                path.display()
            ))
        })?;
        evidence = evidence
            .with_firmware_artifact(path, &bytes)
            .map_err(|e| SpecError::Invalid(format!("building firmware evidence: {e}")))?;
    }

    Ok(RunOutcome {
        seed,
        windows,
        uart,
        faults,
        toggles,
        peak_current,
        peak_temp_c,
        peripherals,
        rail_windows,
        protection_tripped,
        protection_tripped_scoped,
        ambient_c: spec.ambient_c,
        bind: Some(hauksbee_engine::result::BindSummary::from_report(
            engine.report(),
        )),
        evidence: Some(evidence),
        sim_ms: engine.scheduler().sim_time * 1000.0,
        boot_first_cross_ms,
        boot_drop_after_cross_ms,
        driven_nets,
        drive_direction_observable,
        first_fault_ms,
        ac: None,
        analog_valid,
        failed_windows,
        fallback_windows,
        error_budget,
        analog_abort,
        sampled_values: plan.values.clone(),
        net_series,
        substitutions: engine
            .scheduler()
            .substitutions()
            .iter()
            .map(|s| s.message())
            .collect(),
        // Co-sim coverage honesty (U3): the same canonical messages the run
        // binary's surfaces emit, so the CI report names identical facts.
        coverage_warnings: engine
            .scheduler()
            .adc_dropped()
            .iter()
            .map(|d| d.message())
            .chain(
                engine
                    .scheduler()
                    .unexercised_buses()
                    .iter()
                    .map(|b| b.message()),
            )
            // Watchdog coverage: a backend whose armed watchdog never fires
            // (renode:nrf52840, the ESP32 timer groups) lets firmware that HANGS
            // run forever, so a CI assertion about behaviour after a hang proves
            // nothing; and a reboot that DID happen means an assertion which
            // passed across it measured a rebooted core.
            .chain(engine.scheduler().watchdog_limitations().into_iter().map(
                |(mcu_ref, limitation)| {
                    hauksbee_engine::scheduler::watchdog_limitation_message(&mcu_ref, &limitation)
                },
            ))
            .chain(
                engine
                    .scheduler()
                    .watchdog_resets()
                    .into_iter()
                    .map(|(mcu_ref, resets)| {
                        hauksbee_engine::scheduler::watchdog_reset_message(&mcu_ref, resets)
                    }),
            )
            .chain(hollow_warnings.iter().cloned())
            .collect(),
        timing_coverage: engine.scheduler().timing_coverage(),
        timing_refusals: timing_configuration_refusal
            .into_iter()
            .chain(assertion_timing_refusals(
                spec,
                &engine.scheduler().timing_coverage(),
            ))
            .chain(engine.scheduler().short_pulses().iter().map(|p| {
                format!(
                    "timing claim refused because a pulse was missed by a tick-evaluated part: {}",
                    p.message()
                )
            }))
            .chain(engine.scheduler().timing_refusals().iter().cloned())
            .collect(),
        dead_rails,
        unexercised_bus_ids: engine
            .scheduler()
            .unexercised_buses()
            .iter()
            .map(|b| b.id.clone())
            .collect(),
        spi_framing: engine
            .scheduler()
            .spi_framing_modes()
            .into_iter()
            .map(|(bus, mode)| (bus, mode.as_str().to_string()))
            .collect(),
    })
}

/// Whether a protection trip at `trip_t` (if any) latched inside a scenario
/// window `[start_s, end_s)`. Half-open: a trip exactly at the next scenario's
/// start belongs to that later scenario, not this one. `end_s == +inf` means the
/// window runs to end-of-run (the last scenario and the run-wide window).
///
/// The runner itself uses `any_trip_in_window`, since a hiccup-mode supply
/// latches more than once. This single-latch form is what the tests pin the
/// half-open boundary against, so it is compiled only for them.
#[cfg(test)]
fn trip_in_window(trip_t: Option<f64>, start_s: f64, end_s: f64) -> bool {
    trip_t.is_some_and(|t| time_in_window(t, start_s, end_s))
}

/// True if ANY recorded latch instant for a net falls in `[start_s, end_s)`. A
/// re-armed (hiccup-mode) supply latches more than once, so a scenario-scoped
/// verdict must consider every latch, not just the first, otherwise a later
/// window's own re-trip is lost (it was attributed to the earliest window that
/// saw the first latch).
fn any_trip_in_window(trip_ts: Option<&Vec<f64>>, start_s: f64, end_s: f64) -> bool {
    trip_ts.is_some_and(|ts| ts.iter().any(|&t| time_in_window(t, start_s, end_s)))
}

/// Fold the per-net latch times into per-(scenario, net) trip verdicts: a trip
/// belongs to a window if ANY latch fell WITHIN it, at or after the window's
/// start AND before the next scenario begins (`end_s`). The lower bound lets a
/// scoped `protection_trip` ignore a trip from before the scenario began; the
/// upper bound stops a LATER scenario's trip from being attributed to this
/// (earlier-starting) one. Testing every latch (not just the first) lets a later
/// window recover its own genuine re-trip on a re-armed supply. Half-open
/// `[start, end)`.
fn scope_protection_trips(
    trip_t: &HashMap<String, Vec<f64>>,
    windows: &[ScenarioWindow],
) -> HashMap<(String, String), bool> {
    let mut scoped: HashMap<(String, String), bool> = HashMap::new();
    for sw in windows {
        for net in &sw.nets {
            let tripped = any_trip_in_window(trip_t.get(net), sw.start_s, sw.end_s);
            scoped.insert((sw.id.clone(), net.clone()), tripped);
        }
    }
    scoped
}

/// Whether time `t` falls in a scenario's half-open window `[start_s, end_s)`.
/// Lenient at the start (a sample essentially at the scenario's onset counts),
/// strict at the end (a sample at or after the next scenario's start belongs to
/// THAT later phase, which picks it up via its own lenient start). `end_s == +∞`
/// (the last scenario and the run-wide window) leaves the upper test always true.
/// Shared by protection-trip scoping and rail-window sampling so both agree on
/// exactly which phase owns a given instant.
fn time_in_window(t: f64, start_s: f64, end_s: f64) -> bool {
    t + 1e-12 >= start_s && t < end_s - 1e-12
}

/// Does `[start_s, end_s)` overlap any failed-analog window in `windows`? Used to
/// gate a frame's aggregates and (via the outcome) to mark overlapping assertions
/// INVALID. Standard half-open interval overlap: `start < w.end && w.start < end`.
fn windows_overlap(windows: &[(f64, f64)], start_s: f64, end_s: f64) -> bool {
    windows.iter().any(|&(ws, we)| start_s < we && ws < end_s)
}

/// Update the boot-coverage reach record for one watched net at one frame.
///
/// Records the FIRST time the net is at/above level (`first_cross`, never
/// forgotten) and the first time it falls back below level AFTER that crossing
/// (`drop_after_cross`). The assertion combines these with its deadline: reached
/// = `first_cross <= deadline`; held-through-deadline = no `drop_after_cross`
/// at/before the deadline. Never forgetting the crossing is what lets the
/// assertion distinguish "never reached" from "reached then dropped"; keying the
/// hold off the deadline (not end-of-run) is what makes a legitimate
/// post-deadline release pass and makes the verdict independent of late
/// analog-failed frames.
fn boot_reach_update(
    first_cross_ms: &mut HashMap<(String, u64), f64>,
    drop_after_cross_ms: &mut HashMap<(String, u64), f64>,
    key: (String, u64),
    v: f64,
    level: f64,
    t_ms: f64,
) {
    if v >= level - 1e-6 {
        first_cross_ms.entry(key).or_insert(t_ms);
    } else if first_cross_ms.contains_key(&key) {
        // A drop only counts once the net has crossed; keep the earliest.
        drop_after_cross_ms.entry(key).or_insert(t_ms);
    }
}

/// A scenario's measurement window: an id (empty for run-wide), the time it
/// begins, the time it ends, and the set of rails the spec's assertions
/// reference for it. `end_s` is the start of the next scenario on the timeline
/// (`+∞` for the last scenario and for the run-wide window), so a
/// scenario-scoped `protection_trip` verdict counts only trips that latch within
/// this scenario's phase, a later scenario's trip must not be attributed here.
struct ScenarioWindow {
    id: String,
    start_s: f64,
    end_s: f64,
    nets: Vec<String>,
}

/// Attach every `[[scenario]]` load to the engine and build the list of
/// measurement windows the frame loop will populate. The window's rails are the
/// nets named by any `rail_window` assertion scoped to this scenario (or run-wide
/// when a rail_window has no scenario scope).
fn attach_scenarios(
    spec: &Spec,
    board: &ExtractedBoard,
    net_node: &HashMap<String, NodeId>,
    sched: &mut hauksbee_engine::scheduler::Scheduler,
) -> Result<Vec<ScenarioWindow>, SpecError> {
    use hauksbee_engine::DynamicLoad;

    // Resolve the profile set: built-ins plus any inline spec-local profiles.
    let resolve_profile = |name: &str| -> Option<hauksbee_models::LoadProfile> {
        if let Some(ip) = spec.profiles.iter().find(|p| p.id == name) {
            return Some(ip.to_profile());
        }
        hauksbee_models::LoadProfile::by_id(name)
    };

    // Each scenario's own (id, supply_net, start), seeded into the window set
    // below so a scenario-scoped `protection_trip` assertion has a
    // (scenario, net) verdict to read even when no rail_window names that net.
    // Without this the scoped-trip map only ever covered rail_window nets, so a
    // protection_trip scoped to a scenario with no matching rail_window always
    // failed (in either polarity) regardless of the real trip.
    let mut scenario_supplies: Vec<(String, String, f64)> = Vec::new();

    for sc in &spec.scenarios {
        let profile = resolve_profile(&sc.profile).ok_or_else(|| {
            SpecError::Invalid(format!(
                "scenario on part '{}' references unknown profile '{}' (not a built-in or [[profile]])",
                sc.part, sc.profile
            ))
        })?;

        // Resolve the supply net: explicit `supply_net`, else inferred from the
        // part's power pins.
        let net_name = match &sc.supply_net {
            Some(n) => n.clone(),
            None => infer_supply_net(board, &sc.part).ok_or_else(|| {
                SpecError::Invalid(format!(
                    "scenario on part '{}': could not infer a supply net (no VDD/VCC-class power pin found); set `supply_net` explicitly",
                    sc.part
                ))
            })?,
        };
        let node = net_node.get(&net_name).copied().ok_or_else(|| {
            SpecError::Invalid(format!(
                "scenario on part '{}': supply net '{}' not found on the board",
                sc.part, net_name
            ))
        })?;

        let id = sc.id.clone().unwrap_or_else(|| sc.part.clone());
        scenario_supplies.push((id.clone(), net_name.clone(), sc.start_ms / 1000.0));
        let load = DynamicLoad::new(
            sched.circuit_mut(),
            &format!("load_{id}"),
            node,
            profile,
            sc.start_ms / 1000.0,
            sc.seed,
        );
        sched.attach_peripheral(Box::new(load));
    }

    // Seed the window set with each scenario's own supply net, so a
    // scenario-scoped protection_trip has a (scenario, net) verdict even with no
    // rail_window on that net. The rail_window loop below then merges its own
    // nets into the same by-id windows (dedup), so the two producers agree on
    // the (scenario, net) key set the protection_trip consumer reads.
    let mut windows: Vec<ScenarioWindow> = Vec::new();
    for (id, net, start_s) in scenario_supplies {
        match windows.iter_mut().find(|w| w.id == id) {
            Some(w) => {
                if !w.nets.contains(&net) {
                    w.nets.push(net);
                }
            }
            None => windows.push(ScenarioWindow {
                id,
                start_s,
                end_s: f64::INFINITY,
                nets: vec![net],
            }),
        }
    }

    // Merge in the rail_window assertions' nets.
    for a in &spec.asserts {
        if a.kind != "rail_window" {
            continue;
        }
        let Some(net) = &a.net else { continue };
        let scope = a.scenario.clone().unwrap_or_default();
        // The window start is the scoped scenario's start_ms; a run-wide window
        // (no scope) starts at 0 and must not borrow the first scenario's start.
        // An unknown scope is a hard error, never a silent whole-run window:
        // `Spec::validate` already rejects it at load, so this is belt-and-braces
        // for any future call path that builds a Spec without validating.
        let start_s = if scope.is_empty() {
            0.0
        } else {
            spec.scenarios
                .iter()
                .find(|s| s.id.as_deref() == Some(scope.as_str()))
                .map(|s| s.start_ms / 1000.0)
                .ok_or_else(|| {
                    SpecError::Invalid(format!(
                        "rail_window on '{net}' is scoped to scenario '{scope}', but no \
                         [[scenario]] declares that id; refusing to measure it over the \
                         whole run"
                    ))
                })?
        };
        match windows.iter_mut().find(|w| w.id == scope) {
            Some(w) => {
                if !w.nets.contains(net) {
                    w.nets.push(net.clone());
                }
            }
            None => windows.push(ScenarioWindow {
                id: scope,
                start_s,
                end_s: f64::INFINITY,
                nets: vec![net.clone()],
            }),
        }
    }

    // Merge in scenario-scoped `protection_trip` assertions' supply nets. The
    // scoped-trip verdict map only carries (scenario, net) keys for nets in a
    // window's `nets`, but a protection_trip's `supply_net` may be a BATTERY rail
    // the scenario's load pulls current from, not the scenario's own (possibly
    // downstream) supply net and not named by any rail_window. Without adding it,
    // scope_protection_trips has no key and check_protection_trip returns a false
    // RED ("<net> was not a supply net in scenario window ... (nothing to trip)")
    // regardless of whether the pack actually latched.
    merge_protection_trip_nets(&mut windows, &spec.asserts);

    // Bound each scenario-scoped window at the next scenario's start on the
    // timeline: scenarios are sequential phases, so a trip that latches during a
    // LATER scenario must not be attributed to an earlier-starting one sharing the
    // same supply net (protection_tripped_scoped keys by net, and an earlier
    // start <= a later start, so without an upper bound the later trip satisfies
    // every earlier window). The run-wide window (empty id) keeps +∞; it
    // deliberately spans the whole run.
    let mut starts: Vec<f64> = spec.scenarios.iter().map(|s| s.start_ms / 1000.0).collect();
    starts.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    for w in &mut windows {
        if !w.id.is_empty() {
            w.end_s = starts
                .iter()
                .copied()
                .find(|&st| st > w.start_s + 1e-12)
                .unwrap_or(f64::INFINITY);
        }
    }
    Ok(windows)
}

/// Infer a part's supply net from its power pins. Looks for a pin whose function
/// name is a supply name (VDD/VCC/VBAT/AVDD/3V3/5V class) and returns that pin's
/// net name. Returns the first such net found.
fn infer_supply_net(board: &ExtractedBoard, part_ref: &str) -> Option<String> {
    const SUPPLY_HINTS: &[&str] = &[
        "VDD", "VCC", "VBAT", "AVDD", "DVDD", "VDDA", "VDD3P3", "3V3", "3.3V", "VIN", "VBUS", "5V",
        "VDDIO", "VDD_SPI",
    ];
    let comp = board.components.iter().find(|c| c.reference == part_ref)?;
    for pin in &comp.pins {
        let f = pin.function.to_ascii_uppercase();
        if SUPPLY_HINTS.iter().any(|h| f.contains(h)) {
            if let Some(net_id) = pin.net {
                if let Some(net) = board.nets.iter().find(|n| n.id == net_id) {
                    return Some(net.name.clone());
                }
            }
        }
    }
    None
}

/// Apply opt-in capacitor parasitics (ESR/ESL) to the bound circuit. Default
/// (no `[decoupling]` block, or `parasitics = false`) leaves caps ideal. With
/// `parasitics = true`, every bound capacitor gets package/dielectric-default
/// ESR/ESL inferred from its footprint and value; per-ref overrides win.
fn apply_decoupling(
    spec: &Spec,
    board: &ExtractedBoard,
    bound: &mut BoundBoard,
) -> Result<(), SpecError> {
    use hauksbee_engine::{apply_parasitics, EsrEsl};
    use hauksbee_ir::Device;

    let Some(dec) = &spec.decoupling else {
        return Ok(());
    };

    // Footprint/value lookup by capacitor reference, for default inference.
    let cap_meta: HashMap<String, (String, f64)> = board
        .components
        .iter()
        .map(|c| {
            // Parse the value to farads best-effort (0 if unparseable).
            let f = hauksbee_models::value::parse_value(&c.value)
                .map(|p| p.si)
                .unwrap_or(0.0);
            (c.reference.clone(), (c.footprint.clone(), f))
        })
        .collect();

    // Collect the capacitor names actually in the bound circuit.
    let cap_names: Vec<String> = bound
        .circuit
        .devices
        .iter()
        .filter_map(|d| match d {
            Device::Capacitor { name, .. } => Some(name.clone()),
            _ => None,
        })
        .collect();

    // Per-ref override table.
    let overrides: HashMap<&str, &crate::scenarios::CapOverride> = dec
        .overrides
        .iter()
        .map(|o| (o.reference.as_str(), o))
        .collect();

    for name in cap_names {
        let p = if let Some(ov) = overrides.get(name.as_str()) {
            // Start from the footprint default, then apply whichever fields the
            // override specifies.
            let (fp, val) = cap_meta
                .get(&name)
                .cloned()
                .unwrap_or_else(|| (String::new(), 0.0));
            let mut base = EsrEsl::from_footprint(&fp, val);
            if let Some(r) = ov.esr_ohms {
                base.esr_ohms = r;
            }
            if let Some(l) = ov.esl_henries {
                base.esl_henries = l;
            }
            base
        } else if dec.parasitics {
            let (fp, val) = cap_meta
                .get(&name)
                .cloned()
                .unwrap_or_else(|| (String::new(), 0.0));
            EsrEsl::from_footprint(&fp, val)
        } else {
            // Parasitics off and no override for this cap: leave it ideal.
            continue;
        };
        apply_parasitics(&mut bound.circuit, &name, p);
    }
    Ok(())
}

/// Resolve a peripheral's attachment net by net name or connector ref+pin.
fn resolve_net(
    spec: &crate::spec::PeripheralSpec,
    board: &ExtractedBoard,
    net_node: &HashMap<String, NodeId>,
    which: Option<&str>,
) -> Option<NodeId> {
    // Explicit net name (or named alternate terminal) wins.
    if let Some(name) = which {
        if let Some(&n) = net_node.get(name) {
            return Some(n);
        }
    }
    if which.is_none() {
        if let Some(name) = &spec.net {
            if let Some(&n) = net_node.get(name) {
                return Some(n);
            }
        }
        // Connector ref+pin: find the component, then the pin's net name.
        if let (Some(reference), Some(pin)) = (&spec.reference, &spec.pin) {
            if let Some(comp) = board.components.iter().find(|c| &c.reference == reference) {
                if let Some(p) = comp.pins.iter().find(|p| &p.number == pin) {
                    if let Some(net_id) = p.net {
                        if let Some(net) = board
                            .nets
                            .iter()
                            .find(|n| n.id == net_id)
                            .map(|n| n.name.clone())
                        {
                            return net_node.get(&net).copied();
                        }
                    }
                }
            }
        }
    }
    None
}

use hauksbee_engine::{CsProvenance, ResolvedCs};

/// Which CS net a SPI peripheral gets, and who supplied it.
///
/// The spec's `cs_net` wins outright: a board whose model pad map is wrong, or
/// whose CS is buffered through something the model cannot see, must stay
/// overridable by hand. Only when the spec is silent does the bound model's `cs`
/// pin role get a say, via `ref` naming the slave's own board component.
///
/// `None` means neither route produced a net, so the bus stays on the
/// chunk-boundary heuristic and the coverage reports `heuristic` — the same
/// honest answer as before the model-role route existed.
fn cs_net_name(
    p: &crate::spec::PeripheralSpec,
    board: &ExtractedBoard,
    lib: &hauksbee_models::ModelLibrary,
) -> Result<Option<(String, CsProvenance)>, SpecError> {
    if let Some(net) = &p.cs_net {
        return Ok(Some((net.clone(), CsProvenance::SpecDeclared)));
    }
    let Some(reference) = p.reference.as_ref() else {
        return Ok(None);
    };
    let Some((net, model_id)) = hauksbee_engine::binder::model_role_cs_net(board, reference, lib)
    else {
        return Ok(None);
    };
    // The `ref` must name the part this peripheral actually IS. Pointing a
    // `spi_eeprom` at the board's MCP3008 resolves a real `cs` role on a real
    // assembled part, and taking it would frame the EEPROM's transactions off the
    // ADC's chip-select while reporting `exact`.
    //
    // This is a LOUD refusal, not a quiet drop to the heuristic. The spec says two
    // contradictory things about one component, and picking either reading for the
    // user would bury the contradiction under a tier they did not ask about.
    //
    // Only judged when the bound model is one the built-in DB claims for some SPI
    // kind. An unrecognised id is allowed through: a user model pack may
    // legitimately supply the part, and no list here can know its id.
    if crate::spec::is_builtin_spi_slave_model_id(&model_id)
        && crate::spec::builtin_model_id_for_spi_kind(&p.kind) != Some(model_id.as_str())
    {
        return Err(SpecError::Invalid(format!(
            "peripheral '{}' is type '{}' but its `ref` names component '{}', which binds \
             model '{}'. Taking that part's chip-select would frame this slave's \
             transactions on another device's CS edges and report them as exact. Point \
             `ref` at the component this peripheral models, or declare `cs_net` explicitly.",
            p.id, p.kind, reference, model_id,
        )));
    }
    Ok(Some((net, CsProvenance::ModelRoles)))
}

/// Resolve a SPI peripheral's chip-select net to the MCU pin that drives it
/// (05 §2.1), so the co-sim frames transactions on the real chip-select edges.
///
/// The net comes from [`cs_net_name`] (spec-declared, else the bound model's
/// `cs` pin role). It is then looked up in the bound net map and traced back to
/// the GPIO driver pin via the scheduler (the same net-to-driving-pin trace the
/// 74HC595 chain wiring uses).
///
/// Returns `None` when no CS net was found, or when the net does not resolve to
/// a driven MCU pin (an unrouted CS, or one driven by something that is not an
/// MCU GPIO): the bus falls back to the chunk-boundary heuristic and the
/// coverage reports `heuristic`. The [`CsProvenance`] rides along so the report
/// can say which route produced the exact tier, because the two fail
/// differently: a spec typo is caught by net validation, whereas a model pad map
/// is only as right as the model entry.
fn resolve_cs_pin(
    p: &crate::spec::PeripheralSpec,
    board: &ExtractedBoard,
    net_node: &HashMap<String, NodeId>,
    sched: &hauksbee_engine::scheduler::Scheduler,
    lib: &hauksbee_models::ModelLibrary,
) -> Result<Option<ResolvedCs<NodeId>>, SpecError> {
    let Some((net, provenance)) = cs_net_name(p, board, lib)? else {
        return Ok(None);
    };
    let Some(node) = net_node.get(&net).copied() else {
        return Ok(None);
    };
    // Carry the CS NET NODE alongside the pin so attach_spi_bus installs the CS
    // frame on the MCU that actually drives this net, not merely the first MCU
    // that owns the identical chip-local (port,bit) tuple.
    Ok(sched.pin_driving_node(node).map(|pin| ResolvedCs {
        pin,
        net: Some(node),
        provenance,
    }))
}

/// Attach every peripheral in the spec to the scheduler. Returns the list of
/// (sink id, output path) for VCD sinks so they can be dumped after the run.
fn attach_peripherals(
    spec: &Spec,
    board: &ExtractedBoard,
    net_node: &HashMap<String, NodeId>,
    sched: &mut hauksbee_engine::scheduler::Scheduler,
    // The model library, for the SPI chip-select route that reads a bound model's
    // `cs` pin role when the spec declares no `cs_net`.
    lib: &hauksbee_models::ModelLibrary,
    // `Some(seed)` in a multi-member ensemble: the vcd_sink path is made
    // per-seed so N runs do not overwrite one fixed file. `None` for a single run.
    vcd_seed: Option<u32>,
) -> Result<Vec<(String, std::path::PathBuf)>, SpecError> {
    use std::sync::{Arc, Mutex};

    use hauksbee_engine::peripherals::controls::{pwl as pwl_source, StimulusKind};
    use hauksbee_engine::{Eeprom24c, I2cBus, Lm75, Mcp3008, Spi25Eeprom, SpiBus};
    use hauksbee_engine::{Encoder, Potentiometer, Pushbutton, Stimulus, ToggleSwitch, VcdSink};
    use hauksbee_ir::{NodeId as N, SourceKind};

    let mut vcd_targets = Vec::new();

    for p in &spec.peripherals {
        let err = |m: String| SpecError::Invalid(format!("peripheral '{}': {m}", p.id));
        match p.kind.as_str() {
            "pushbutton" => {
                let net = resolve_net(p, board, net_node, None)
                    .ok_or_else(|| err("net not found".into()))?;
                let to =
                    p.to.as_ref()
                        .and_then(|t| net_node.get(t).copied())
                        .unwrap_or(N::GROUND);
                let b = Pushbutton::new(
                    sched.circuit_mut(),
                    &p.id,
                    net,
                    to,
                    p.bounce_ms.unwrap_or(0.0),
                );
                sched.attach_peripheral(Box::new(b));
            }
            "toggle" => {
                let net = resolve_net(p, board, net_node, None)
                    .ok_or_else(|| err("net not found".into()))?;
                let to =
                    p.to.as_ref()
                        .and_then(|t| net_node.get(t).copied())
                        .unwrap_or(N::GROUND);
                let t = ToggleSwitch::new(
                    sched.circuit_mut(),
                    &p.id,
                    net,
                    to,
                    p.initial.map(|v| v >= 0.5).unwrap_or(false),
                );
                sched.attach_peripheral(Box::new(t));
            }
            "potentiometer" => {
                let w = resolve_net(p, board, net_node, p.wiper.as_deref())
                    .or_else(|| resolve_net(p, board, net_node, None))
                    .ok_or_else(|| err("wiper net not found".into()))?;
                let a =
                    p.a.as_ref()
                        .and_then(|n| net_node.get(n).copied())
                        .ok_or_else(|| err("pot terminal `a` net not found".into()))?;
                let b =
                    p.b.as_ref()
                        .and_then(|n| net_node.get(n).copied())
                        .unwrap_or(N::GROUND);
                let pot = Potentiometer::new(
                    sched.circuit_mut(),
                    &p.id,
                    a,
                    w,
                    b,
                    p.r_total.unwrap_or(10_000.0),
                    p.initial.unwrap_or(0.5),
                );
                sched.attach_peripheral(Box::new(pot));
            }
            "encoder" => {
                let a = resolve_net(p, board, net_node, p.net_a.as_deref())
                    .ok_or_else(|| err("encoder net_a not found".into()))?;
                let b = resolve_net(p, board, net_node, p.net_b.as_deref())
                    .ok_or_else(|| err("encoder net_b not found".into()))?;
                let enc = Encoder::new(sched.circuit_mut(), &p.id, a, b, p.vhigh.unwrap_or(5.0));
                sched.attach_peripheral(Box::new(enc));
            }
            "stimulus" => {
                let net = resolve_net(p, board, net_node, None)
                    .ok_or_else(|| err("net not found".into()))?;
                let kind = match p.waveform.as_deref().unwrap_or("dc") {
                    "dc" => StimulusKind::Wave(SourceKind::Dc(p.offset.unwrap_or(0.0))),
                    "sine" => StimulusKind::Wave(SourceKind::Sin {
                        offset: p.offset.unwrap_or(0.0),
                        amplitude: p.amplitude.unwrap_or(1.0),
                        freq: p.freq_hz.unwrap_or(1000.0),
                        delay: 0.0,
                        theta: 0.0,
                        phase: 0.0,
                    }),
                    "pwl" => {
                        let pts = p
                            .pwl
                            .as_ref()
                            .map(|v| v.iter().map(|[t, val]| (t / 1000.0, *val)).collect())
                            .unwrap_or_default();
                        StimulusKind::Wave(pwl_source(pts))
                    }
                    "noise" => StimulusKind::Noise {
                        offset: p.offset.unwrap_or(0.0),
                        amplitude: p.amplitude.unwrap_or(0.1),
                        seed: 0xC0FFEE,
                    },
                    other => return Err(err(format!("unknown waveform '{other}'"))),
                };
                let s = Stimulus::voltage(sched.circuit_mut(), &p.id, net, kind);
                sched.attach_peripheral(Box::new(s));
            }
            "i2c_eeprom" => {
                let bus = I2cBus::new(&p.id).with_slave(Box::new(Eeprom24c::new(
                    p.address.unwrap_or(0x50),
                    p.size.unwrap_or(256),
                )));
                sched.attach_i2c_bus(Arc::new(Mutex::new(bus)));
            }
            "i2c_lm75" => {
                let bus = I2cBus::new(&p.id).with_slave(Box::new(Lm75::new(
                    p.address.unwrap_or(Lm75::DEFAULT_ADDR),
                    p.temp_c.unwrap_or(25.0),
                )));
                sched.attach_i2c_bus(Arc::new(Mutex::new(bus)));
            }
            "spi_eeprom" => {
                let cs = resolve_cs_pin(p, board, net_node, sched, lib)?;
                let bus = SpiBus::new(&p.id, Box::new(Spi25Eeprom::new(p.size.unwrap_or(256))));
                sched.attach_spi_bus(Arc::new(Mutex::new(bus)), cs);
            }
            "spi_mcp3008" => {
                let cs = resolve_cs_pin(p, board, net_node, sched, lib)?;
                let bus = SpiBus::new(&p.id, Box::new(Mcp3008::new(p.vref.unwrap_or(5.0))));
                sched.attach_spi_bus(Arc::new(Mutex::new(bus)), cs);
            }
            "vcd_sink" => {
                let names = p.nets.clone().unwrap_or_default();
                let mut logged = Vec::new();
                for name in &names {
                    if let Some(&n) = net_node.get(name) {
                        logged.push((name.clone(), n));
                    } else {
                        return Err(err(format!("vcd net '{name}' not found")));
                    }
                }
                let path = p.vcd_path.as_ref().map(|s| {
                    let base = spec.base_dir.join(s);
                    // Per-seed path in an ensemble ("wave.vcd" -> "wave.seed3.vcd")
                    // so each member's waveform is retained, not overwritten.
                    match vcd_seed {
                        Some(seed) => {
                            let stem = base.file_stem().and_then(|s| s.to_str()).unwrap_or("wave");
                            let ext = base.extension().and_then(|s| s.to_str()).unwrap_or("vcd");
                            base.with_file_name(format!("{stem}.seed{seed}.{ext}"))
                        }
                        None => base,
                    }
                });
                if let Some(path) = &path {
                    vcd_targets.push((p.id.clone(), path.clone()));
                }
                let sink = VcdSink::new(&p.id, logged, path);
                sched.attach_peripheral(Box::new(sink));
            }
            other => return Err(err(format!("unknown type '{other}'"))),
        }

        // Register the peripheral's timeline events.
        if !p.events.is_empty() {
            let events = p
                .events
                .iter()
                .map(|e| hauksbee_engine::TimelineEvent {
                    target: p.id.clone(),
                    t_s: e.t_ms / 1000.0,
                    value: e.value,
                })
                .collect();
            sched.add_timeline(events);
        }
    }

    Ok(vcd_targets)
}

/// Parse and attach every `[[sensor]]` entry from the spec to the scheduler's
/// bus system. Each sensor is parsed via `RegisterMapSensor::from_toml`, has
/// its declared inputs overridden per the spec, then is attached to an
/// `I2cBus` (for `bus = "i2c"`) or a `SpiBus` (for `bus = "spi"`).
///
/// The resulting buses are registered with the scheduler exactly the same way
/// the `i2c_lm75` / `spi_mcp3008` peripheral kinds do it in `attach_peripherals`,
/// and the declarative co-sim tests (`declarative_sensor_cosim.rs`) wire it.
/// The Renode / simavr I2C+SPI bridge picks them up automatically without any
/// further changes.
fn attach_sensors(
    spec: &Spec,
    sched: &mut hauksbee_engine::scheduler::Scheduler,
) -> Result<(), SpecError> {
    use std::sync::{Arc, Mutex};

    use hauksbee_engine::{I2cBus, RegisterMapSensor, SpiBus};
    use hauksbee_models::sensor_spec::Bus;

    for sa in &spec.sensors {
        let toml_src = sa.toml_source(&spec.base_dir)?;

        let mut sensor = RegisterMapSensor::from_toml(&toml_src).map_err(|e| {
            SpecError::Invalid(format!(
                "sensor '{}': failed to parse sensor spec: {e}",
                sa.id
            ))
        })?;

        // Apply per-run input overrides.
        for (name, &value) in &sa.inputs {
            sensor.set_input(name, value);
        }

        // Attach to the correct bus type (I2C or SPI) the same way the
        // hand-coded slaves are attached in `attach_peripherals`.
        match sensor.bus() {
            Bus::I2c => {
                let bus = I2cBus::new(&sa.id).with_slave(Box::new(sensor));
                sched.attach_i2c_bus(Arc::new(Mutex::new(bus)));
            }
            Bus::Spi => {
                let arc = Arc::new(Mutex::new(SpiBus::new(&sa.id, Box::new(sensor))));
                // Declarative sensors do not (yet) carry a CS-net field, so they
                // stay on the chunk-boundary heuristic (coverage reports
                // `heuristic`). A resolved CS pin can be threaded here the same way
                // `attach_peripherals` does once the sensor spec grows a cs_net.
                if let Some(controller) = &sa.controller {
                    sched.attach_spi_bus_on(controller, arc, None);
                } else {
                    sched.attach_spi_bus(arc, None);
                }
            }
        }
    }

    Ok(())
}

/// Return warning strings for any bus-slave peripheral or declarative sensor
/// that was attached to a QEMU-backed board.
///
/// The QEMU I2C/SPI bus bridge is deferred (not yet implemented): these slaves
/// are silently a no-op on `qemu:*` backends. The warnings are surfaced before
/// the co-sim starts so users understand why bus-dependent assertions fail.
///
/// AVR (simavr) and Renode backends have a working bus bridge and produce no
/// warnings here.
///
/// Extracted into its own function so the logic is unit-testable without
/// capturing `eprintln!` output.
pub(crate) fn qemu_bus_slave_warnings(spec: &Spec, qemu_backends: &[String]) -> Vec<String> {
    if qemu_backends.is_empty() {
        return Vec::new();
    }

    const BUS_SLAVE_KINDS: &[&str] = &["i2c_eeprom", "i2c_lm75", "spi_eeprom", "spi_mcp3008"];
    let backend_str = qemu_backends.join(", ");
    let mut warnings = Vec::new();

    for p in &spec.peripherals {
        if BUS_SLAVE_KINDS.contains(&p.kind.as_str()) {
            let bus_kind = if p.kind.starts_with("i2c") {
                "I2C"
            } else {
                "SPI"
            };
            warnings.push(format!(
                "WARNING: peripheral '{}' ({} {}) is a NO-OP on backend {}; \
                 I2C/SPI bus-slave co-sim is supported on AVR (simavr) and \
                 Renode backends only. The peripheral will not respond; firmware \
                 that depends on it may fail for that reason.",
                p.id, bus_kind, p.kind, backend_str
            ));
        }
    }

    for sa in &spec.sensors {
        warnings.push(format!(
            "WARNING: sensor '{}' (declarative bus sensor) is a NO-OP on backend {}; \
             I2C/SPI bus-slave co-sim is supported on AVR (simavr) and \
             Renode backends only. The sensor will not respond; firmware \
             that depends on it may fail for that reason.",
            sa.id, backend_str
        ));
    }

    warnings
}

/// Snapshot every peripheral's end-of-run state and dump VCD files.
fn snapshot_peripherals(
    spec: &Spec,
    engine: &HauksbeeEngine,
    vcd_targets: &[(String, std::path::PathBuf)],
) -> HashMap<String, PeripheralSnapshot> {
    use hauksbee_engine::{Eeprom24c, Spi25Eeprom, VcdSink};

    let sched = engine.scheduler();
    let mut out: HashMap<String, PeripheralSnapshot> = HashMap::new();

    // Numeric state for every peripheral and bus.
    for (id, fields) in sched.peripheral_states() {
        out.entry(id).or_default().fields = fields;
    }

    // EEPROM bytes (I2C 24Cxx and SPI 25xx), and VCD dumps.
    for p in &spec.peripherals {
        match p.kind.as_str() {
            "i2c_eeprom" => {
                for bus in sched.i2c_buses() {
                    let b = bus.lock().unwrap_or_else(|e| e.into_inner());
                    if hauksbee_engine::Peripheral::id(&*b) == p.id {
                        if let Some(ee) = b.slave::<Eeprom24c>(p.address.unwrap_or(0x50)) {
                            out.entry(p.id.clone()).or_default().bytes = ee.contents().to_vec();
                        }
                    }
                }
            }
            "spi_eeprom" => {
                for bus in sched.spi_buses() {
                    let b = bus.lock().unwrap_or_else(|e| e.into_inner());
                    if hauksbee_engine::Peripheral::id(&*b) == p.id {
                        if let Some(ee) = b.slave::<Spi25Eeprom>() {
                            out.entry(p.id.clone()).or_default().bytes = ee.contents().to_vec();
                        }
                    }
                }
            }
            "vcd_sink" => {
                if let Some((_, path)) = vcd_targets.iter().find(|(id, _)| id == &p.id) {
                    if let Some(sink) = sched.peripherals.get::<VcdSink>(&p.id) {
                        // Surface a write failure (e.g. the output dir doesn't
                        // exist), silently dropping it left the user with no
                        // VCD artifact and no diagnostic on a green run.
                        if let Err(e) = sink.write_to(path) {
                            eprintln!(
                                "hauksbee: VCD sink '{}' failed to write {}: {e}",
                                p.id,
                                path.display()
                            );
                        }
                    }
                }
            }
            _ => {}
        }
    }

    out
}

/// Remove the binder's auto-rail on `net`: drop its [`SupplyLeg`] and
/// disconnect the leg's stamped devices so the net floats except for whatever
/// the board itself feeds it.
///
/// The leg's topology matters here (round-2 fix): `SupplyLeg::stamp` places
/// `Vsupply_<net>` on a PRIVATE node (`__supply_<net>`) behind a series
/// `Rsupply_<net>` resistor, so matching "a Vsource whose positive node is
/// the rail" never found it and suppress_rail was a silent NO-OP: the ideal
/// source kept sourcing through its milliohm series resistor and every
/// "suppressed" rail still read nominal. The reliable cut is the series
/// resistor itself: open `Rsupply_<net>` (1 TΩ) and the source is isolated on
/// its private node no matter how the leg is modelled. The direct-Vsource
/// match is kept for any bare `Vrail_*` ideal source stamped straight onto
/// the net (the pre-leg binder shape).
fn suppress_rail(bound: &mut BoundBoard, net: &str) {
    let Some(node) = bound.node(net) else { return };
    bound.supplies.retain(|s| s.net != node);
    let series = format!("Rsupply_{net}");
    for dev in bound.circuit.devices.iter_mut() {
        match dev {
            // The leg's series resistor: opening it disconnects the private
            // drive node from the rail.
            Device::Resistor { name, ohms, .. } if name == &series => {
                *ohms = 1e12;
            }
            // A bare ideal source stamped directly on the rail node.
            Device::Vsource { name, p, .. }
                if *p == node
                    && (name == &format!("Vsupply_{net}") || name.starts_with("Vrail")) =>
            {
                let (nm, a, b) = (name.clone(), *p, NodeId::GROUND);
                *dev = Device::Resistor {
                    name: nm,
                    a,
                    b,
                    ohms: 1e12,
                    tc1: None,
                };
            }
            _ => {}
        }
    }
}

/// One warning per voltage/rail_window assertion whose net is fed DIRECTLY by
/// an ideal source: a declared `kind = "ideal"` supply leg, a binder
/// auto-rail (also stamped as an ideal leg), or a `[[net_drive]]`. Such an
/// assertion is a hollow gate: the source holds the net at its programmed
/// voltage, so the check cannot fail for a board reason and its green vouches
/// for nothing. Behavioral legs (bench / wall / usb / battery) are exempt on
/// purpose, their droop and current limits are exactly what a rail assertion
/// tests, and so is a net an ideal source feeds only *through* board parts.
fn hollow_gate_warnings(spec: &Spec, bound: &BoundBoard) -> Vec<String> {
    use hauksbee_engine::power_supply::PowerSupply;
    let ideal_fed: std::collections::HashSet<&str> = bound
        .supplies
        .iter()
        .filter(|leg| matches!(leg.supply, PowerSupply::Ideal { .. }))
        .map(|leg| leg.net_name.as_str())
        .chain(spec.net_drives.iter().map(|d| d.net.as_str()))
        .collect();
    let mut out = Vec::new();
    for a in &spec.asserts {
        if !matches!(a.kind.as_str(), "voltage" | "rail_window") {
            continue;
        }
        let Some(net) = a.net.as_deref() else {
            continue;
        };
        if ideal_fed.contains(net) {
            out.push(format!(
                "assertion '{}' reads net '{net}', which your own ideal source feeds \
                 directly; it cannot fail for a board reason (the source holds the net at \
                 its programmed voltage). Assert on a net the board derives from it, or \
                 model the real supply (bench/usb/battery) so droop is possible",
                a.label()
            ));
        }
    }
    out
}

fn hollow_gate_assumptions(
    spec: &Spec,
    bound: &BoundBoard,
) -> Vec<hauksbee_ir::evidence::Assumption> {
    use hauksbee_engine::power_supply::PowerSupply;
    let ideal_fed: std::collections::HashSet<&str> = bound
        .supplies
        .iter()
        .filter(|leg| matches!(leg.supply, PowerSupply::Ideal { .. }))
        .map(|leg| leg.net_name.as_str())
        .chain(spec.net_drives.iter().map(|drive| drive.net.as_str()))
        .collect();
    spec.asserts
        .iter()
        .filter(|assertion| matches!(assertion.kind.as_str(), "voltage" | "rail_window"))
        .filter_map(|assertion| assertion.net.as_deref())
        .filter(|net| ideal_fed.contains(net))
        .map(hauksbee_ir::evidence::Assumption::held_by_ideal_source)
        .collect()
}

/// Force `net` to a fixed DC voltage by stamping an ideal source (unless one is
/// already present on that node).
fn drive_net(bound: &mut BoundBoard, net: &str, volts: f64) {
    let Some(node) = bound.node(net) else { return };
    if node.is_ground() {
        return;
    }
    let already = bound
        .circuit
        .devices
        .iter()
        .any(|d| matches!(d, Device::Vsource { p, .. } if *p == node));
    if !already {
        bound.circuit.add(Device::Vsource {
            name: format!("Vci_drive_{net}"),
            p: node,
            n: NodeId::GROUND,
            kind: SourceKind::Dc(volts),
        });
    } else {
        // Retarget any existing CI drive source we own.
        for dev in bound.circuit.devices.iter_mut() {
            if let Device::Vsource { name, p, kind, .. } = dev {
                if *p == node && name == &format!("Vci_drive_{net}") {
                    *kind = SourceKind::Dc(volts);
                }
            }
        }
    }
}

/// Attach (or reconfigure) a power-supply leg on a supply net.
fn attach_supply(bound: &mut BoundBoard, s: &SupplySpec) -> Result<(), SpecError> {
    let supply = build_supply(s)?;
    let Some(node) = bound.node(&s.net) else {
        return Ok(()); // net validation already ran; node absence means ground
    };
    if node.is_ground() {
        return Ok(());
    }
    // Reconfigure if a leg already exists for this net; else stamp a new one.
    if let Some(leg) = bound.supplies.iter_mut().find(|l| l.net == node) {
        leg.reconfigure(&mut bound.circuit, supply);
    } else {
        let leg = SupplyLeg::stamp(&mut bound.circuit, node, &s.net, supply);
        bound.supplies.push(leg);
    }
    Ok(())
}

/// Map a [`SupplySpec`] to the engine's behavioral [`PowerSupply`].
fn build_supply(s: &SupplySpec) -> Result<PowerSupply, SpecError> {
    // SupplySpec::validate already rejected a missing volts / usb / chemistry
    // at load; the error paths here are defense in depth for callers that
    // construct a SupplySpec without going through Spec::load. No silent
    // default: a guessed 5.0 V on a 3.3 V board fabricates faults.
    let volts_required = || {
        SpecError::Invalid(format!(
            "supply on '{}': `{}` needs an explicit `volts`",
            s.net, s.kind
        ))
    };
    let supply = match s.kind.as_str() {
        "ideal" => PowerSupply::Ideal {
            volts: s.volts.ok_or_else(volts_required)?,
        },
        "bench" => PowerSupply::Bench {
            volts: s.volts.ok_or_else(volts_required)?,
            current_limit_a: s.current_limit_a.unwrap_or(1.0),
        },
        "wall" => PowerSupply::Wall {
            volts: s.volts.ok_or_else(volts_required)?,
            r_out_ohms: s.r_out_ohms.unwrap_or(0.5),
            ripple_vpp: s.ripple_vpp.unwrap_or(0.1),
            ripple_hz: s.ripple_hz.unwrap_or(100.0),
        },
        "usb" => PowerSupply::Usb {
            spec: match s.usb.as_deref().ok_or_else(|| {
                SpecError::Invalid(format!(
                    "supply on '{}': `usb` needs an explicit profile (5v0.5a|5v1.5a|5v3a)",
                    s.net
                ))
            })? {
                "5v0.5a" | "5v_0.5a" => UsbSpec::V5_0_5A,
                "5v1.5a" | "5v_1.5a" => UsbSpec::V5_1_5A,
                "5v3a" | "5v_3a" => UsbSpec::V5_3A,
                other => {
                    return Err(SpecError::Invalid(format!(
                        "supply on '{}': unknown usb profile '{}' (expected 5v0.5a|5v1.5a|5v3a)",
                        s.net, other
                    )))
                }
            },
        },
        "battery" => PowerSupply::Battery {
            chemistry: match s.chemistry.as_deref().ok_or_else(|| {
                SpecError::Invalid(format!(
                    "supply on '{}': `battery` needs an explicit `chemistry` \
                     (liion|alkaline|nimh|lifepo4)",
                    s.net
                ))
            })? {
                "liion" | "lipo" => Chemistry::LiIon,
                "alkaline" => Chemistry::Alkaline,
                "nimh" => Chemistry::NiMh,
                "lifepo4" | "lfp" => Chemistry::LiFePO4,
                other => {
                    return Err(SpecError::Invalid(format!(
                    "supply on '{}': unknown chemistry '{}' (expected liion|alkaline|nimh|lifepo4)",
                    s.net, other
                )))
                }
            },
            cells: s.cells.unwrap_or(1),
            capacity_mah: s.capacity_mah.unwrap_or(1000.0),
            soc: s.soc.unwrap_or(1.0),
            r_internal_ohms: s.r_internal_ohms.unwrap_or(0.1),
            protection: match (s.protection_trip_a, s.protection_delay_ms) {
                (Some(trip_a), delay_ms) => {
                    let mut p = hauksbee_engine::power_supply::BatteryProtection::new(
                        trip_a,
                        delay_ms.unwrap_or(0.0) / 1000.0,
                    );
                    if let Some(reset) = s.protection_reset_a {
                        p.reset_a = reset;
                    }
                    Some(p)
                }
                (None, _) => None,
            },
        },
        other => {
            return Err(SpecError::Invalid(format!(
                "supply on '{}': unknown kind '{other}'",
                s.net
            )))
        }
    };
    Ok(supply)
}

/// Derive the per-seed fuzz net drives. Each fuzzed net is strapped to one of
/// the two configured levels (default 0/5 V), chosen by a deterministic PRNG
/// seeded from (seed, net) so a run is reproducible and seed 0 is the
/// all-low baseline.
fn fuzz_net_drives(spec: &Spec, seed: u32) -> Vec<(String, f64)> {
    let Some(fuzz) = &spec.fuzz else {
        return Vec::new();
    };
    let nets: Vec<String> = if fuzz.nets.is_empty() {
        spec.net_drives.iter().map(|d| d.net.clone()).collect()
    } else {
        fuzz.nets.clone()
    };
    let [lo, hi] = fuzz.levels.unwrap_or([0.0, 5.0]);
    nets.into_iter()
        .map(|net| {
            // Seed 0 = baseline (all low). Other seeds pick per-net by a small
            // splitmix-style hash of (seed, net) so states are spread out.
            let v = if seed == 0 {
                lo
            } else {
                let h = hash2(seed as u64, &net);
                if h & 1 == 0 {
                    lo
                } else {
                    hi
                }
            };
            (net, v)
        })
        .collect()
}

/// A tiny deterministic hash of a u64 and a string (splitmix64 over the bytes).
fn hash2(seed: u64, s: &str) -> u64 {
    let mut x = seed
        .wrapping_mul(0x9E3779B97F4A7C15)
        .wrapping_add(0xD1B54A32D192ED03);
    for b in s.bytes() {
        x ^= b as u64;
        x = x.wrapping_mul(0xFF51AFD7ED558CCD);
        x ^= x >> 33;
    }
    x ^= x >> 30;
    x = x.wrapping_mul(0xBF58476D1CE4E5B9);
    x ^= x >> 27;
    x
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poll_backend_toggle_assertion_requires_a_declared_pulse_floor() {
        let spec: Spec = toml::from_str(
            r#"
board = "board.kicad_pcb"
[[assert]]
kind = "toggle"
net = "CLOCK"
min_toggles = 2
"#,
        )
        .expect("spec shape");
        let coverage = vec![hauksbee_engine::scheduler::TimingCoverage {
            mcu_ref: "U1".into(),
            backend: "qemu:test".into(),
            cycle_exact: false,
            timestamp_precision_s: 1e-3,
            minimum_guaranteed_pulse_s: 2e-3,
            chunk_s: 1e-3,
        }];

        let refusals = assertion_timing_refusals(&spec, &coverage);

        assert_eq!(refusals.len(), 1);
        assert!(refusals[0].contains("toggle"));
        assert!(refusals[0].contains("timing.min_pulse_us"));
    }

    /// Serializes the DescriptorDirGuard tests: they mutate the process-global
    /// `HAUKSBEE_MCU_DIR` env var, so parallel test threads must not interleave.
    static MCU_DIR_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn spec_with_descriptor_dir(dir: Option<&str>) -> Spec {
        let mcu = dir
            .map(|d| format!("[mcu]\ndescriptor_dir = \"{d}\"\n"))
            .unwrap_or_default();
        let src = format!(
            "board = \"b.kicad_pcb\"\nduration_ms = 10\n{mcu}\
             [[assert]]\nkind = \"voltage\"\nnet = \"VCC\"\nmin = 3.0\n"
        );
        let mut spec: Spec = toml::from_str(&src).expect("valid toml");
        spec.base_dir = std::path::PathBuf::from("/repo/ci");
        spec
    }

    #[test]
    fn descriptor_dir_guard_sets_the_env_for_its_lifetime_and_restores() {
        let _lock = MCU_DIR_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("HAUKSBEE_MCU_DIR");
        let spec = spec_with_descriptor_dir(Some("socs"));
        {
            let _guard = DescriptorDirGuard::apply(&spec);
            assert_eq!(
                std::env::var("HAUKSBEE_MCU_DIR").as_deref(),
                Ok("/repo/ci/socs"),
                "the spec's descriptor_dir (resolved against the spec dir) is published"
            );
        }
        assert!(
            std::env::var_os("HAUKSBEE_MCU_DIR").is_none(),
            "the guard restores the unset state on drop, so a later spec in the \
             same invocation does not inherit it"
        );
    }

    #[test]
    fn an_explicit_env_var_wins_over_the_spec_descriptor_dir() {
        let _lock = MCU_DIR_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("HAUKSBEE_MCU_DIR", "/operator/override");
        let spec = spec_with_descriptor_dir(Some("socs"));
        {
            let _guard = DescriptorDirGuard::apply(&spec);
            assert_eq!(
                std::env::var("HAUKSBEE_MCU_DIR").as_deref(),
                Ok("/operator/override"),
                "the operator's env var must win over the spec field"
            );
        }
        assert_eq!(
            std::env::var("HAUKSBEE_MCU_DIR").as_deref(),
            Ok("/operator/override"),
            "the guard leaves the operator's value untouched"
        );
        std::env::remove_var("HAUKSBEE_MCU_DIR");
    }

    #[test]
    fn a_spec_without_descriptor_dir_leaves_the_env_alone() {
        let _lock = MCU_DIR_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("HAUKSBEE_MCU_DIR");
        let spec = spec_with_descriptor_dir(None);
        {
            let _guard = DescriptorDirGuard::apply(&spec);
            assert!(std::env::var_os("HAUKSBEE_MCU_DIR").is_none());
        }
        assert!(std::env::var_os("HAUKSBEE_MCU_DIR").is_none());
    }

    const DSL: &[u8] = br#"# Board-as-Code (hauksbee board DSL v1)
board version 20241229

fn main {
    net "A"
    net "B"
    comp R1 lib "Resistor_SMD:R_0402_1005Metric" val "10k" layer "F.Cu" at 0 0 rot 0 {
        pad "1" smd rect at 0 0 size 1 1 layers [F.Cu] net "A"
        pad "2" smd rect at 1 0 size 1 1 layers [F.Cu] net "B"
    }
}
"#;

    #[test]
    fn load_board_accepts_board_as_code() {
        // B5: hauksbee-ci loads `.board` directly, with no "compile it yourself
        // with from-code --route first" detour. The compiled text carries full
        // net connectivity (net-named pads), and CI is entirely netlist-driven,
        // so no routing step is needed. Rejecting `.board` here would also
        // contradict the web checks panel, which tells .board uploaders the
        // downloaded spec will run.
        let dir = std::env::temp_dir().join(format!("hauksbee-ci-bac-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("cell.board");
        std::fs::write(&p, DSL).unwrap();
        let board = load_board(&p).expect(".board must load in CI now");
        assert_eq!(board.components.len(), 1, "R1 survives the compile");
        assert!(
            board.nets.iter().any(|n| n.name == "A") && board.nets.iter().any(|n| n.name == "B"),
            "net connectivity survives: {:?}",
            board.nets.iter().map(|n| &n.name).collect::<Vec<_>>()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_board_accepts_a_gerber_zip() {
        // B5: a spec may point straight at the fab archive. Corpus-gated like
        // the engine's gerber tests: skips when board-corpus is absent.
        let src = hauksbee_testkit::corpus_dir(env!("CARGO_MANIFEST_DIR"))
            .unwrap_or_default()
            .join("famous/uconsole_cm4_adapter_gerber");
        if !src.exists() {
            if std::env::var("HAUKSBEE_REQUIRE_CORPUS").is_ok() {
                panic!("corpus required but uconsole_cm4_adapter_gerber missing");
            }
            eprintln!("skipping CI gerber-zip test (corpus absent)");
            return;
        }
        use std::io::Write;
        let dir = std::env::temp_dir().join(format!("hauksbee-ci-gerb-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let zip_path = dir.join("fab.zip");
        let mut w = zip::ZipWriter::new(std::fs::File::create(&zip_path).unwrap());
        for entry in std::fs::read_dir(&src).unwrap() {
            let p = entry.unwrap().path();
            if p.is_file() {
                w.start_file(
                    format!("gerbers/{}", p.file_name().unwrap().to_str().unwrap()),
                    zip::write::SimpleFileOptions::default(),
                )
                .unwrap();
                w.write_all(&std::fs::read(&p).unwrap()).unwrap();
            }
        }
        w.finish().unwrap();
        let board = load_board(&zip_path).expect("a gerber fab zip must load in CI");
        assert!(!board.nets.is_empty(), "nets recovered from copper");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_board_missing_file_names_the_spec_key() {
        // The spec-relative wording must survive the normalizer delegation.
        let err = load_board(std::path::Path::new("/definitely/not/here.kicad_pcb"))
            .expect_err("missing board errors");
        let msg = err.to_string();
        assert!(
            msg.contains("resolved from the spec's `board` key"),
            "CI keeps its spec-relative error wording: {msg}"
        );
    }

    #[test]
    fn protection_trip_supply_net_is_added_to_its_scenario_window() {
        // R46: a scenario window's `nets` was seeded only from the scenario's own
        // supply and rail_window nets, so a protection_trip naming a BATTERY rail
        // (that the scenario's load pulls from) had no (scenario, net) verdict key
        // and check_protection_trip returned a false RED. The supply_net must be
        // merged into the matching scenario window.
        let mut windows = vec![ScenarioWindow {
            id: "load".into(),
            start_s: 0.1,
            end_s: f64::INFINITY,
            nets: vec!["VBUS".into()], // scenario's own downstream supply
        }];
        let assert: crate::spec::Assertion = toml::from_str(
            "kind = \"protection_trip\"\nsupply_net = \"BATT\"\nscenario = \"load\"\nexpect_trip = true\n",
        )
        .expect("assertion parses");
        merge_protection_trip_nets(&mut windows, std::slice::from_ref(&assert));
        assert!(
            windows[0].nets.contains(&"BATT".to_string()),
            "the protection_trip supply_net must be in the scenario window: {:?}",
            windows[0].nets
        );
        // Idempotent: a second merge does not duplicate.
        merge_protection_trip_nets(&mut windows, std::slice::from_ref(&assert));
        assert_eq!(windows[0].nets.iter().filter(|n| *n == "BATT").count(), 1);
        // An UNSCOPED protection_trip (no scenario) is not merged into any window.
        let unscoped: crate::spec::Assertion = toml::from_str(
            "kind = \"protection_trip\"\nsupply_net = \"OTHER\"\nexpect_trip = true\n",
        )
        .expect("parses");
        merge_protection_trip_nets(&mut windows, std::slice::from_ref(&unscoped));
        assert!(!windows[0].nets.contains(&"OTHER".to_string()));
    }

    #[test]
    fn max_current_accepts_a_resistor_array_bare_ref() {
        // R46: current_tracked holds a multi-unit array's per-unit device names
        // (RN1_e1..RN1_e4), not the bare "RN1", so a package-level max_current on
        // the array was rejected as untrackable while the identical max_temp was
        // accepted. ref_or_unit_matches accepts the bare ref whose units are tracked.
        assert!(ref_or_unit_matches("RN1", "RN1")); // exact
        assert!(ref_or_unit_matches("RN1", "RN1_e1")); // passive array unit
        assert!(ref_or_unit_matches("RN1", "RN1_e12"));
        assert!(ref_or_unit_matches("SW1", "SW1_s0")); // switch bank unit
        assert!(ref_or_unit_matches("Q3", "Q3_q2")); // transistor array unit
                                                     // Not a match: a different ref, or a non-unit suffix.
        assert!(!ref_or_unit_matches("RN1", "RN10_e1"));
        assert!(!ref_or_unit_matches("RN1", "RN1_heater"));
        assert!(!ref_or_unit_matches("RN1", "RN2_e1"));
    }

    #[test]
    fn scenario_scoped_trip_is_bounded_by_the_next_scenario_start() {
        // Round-28: a scenario-scoped protection_trip window had only a lower
        // bound, so a trip that latched during a LATER scenario on a shared supply
        // net was attributed to every earlier-starting scenario. With the phases
        // inrush=[0, 0.1) and steady=[0.1, +inf) sharing net BATT and the BMS
        // latching at 0.15 s (during steady): steady must see the trip, inrush
        // must NOT (no false RED blaming inrush; no false GREEN either).
        let trip = Some(0.15);
        assert!(
            !trip_in_window(trip, 0.0, 0.1),
            "inrush must not own steady's trip"
        );
        assert!(
            trip_in_window(trip, 0.1, f64::INFINITY),
            "steady owns its own trip"
        );
        // A trip before a scenario begins is excluded by the lower bound.
        assert!(
            !trip_in_window(Some(0.05), 0.1, f64::INFINITY),
            "pre-start trip excluded"
        );
        // A trip exactly at the next scenario's start belongs to the later phase
        // (half-open [start, end)).
        assert!(
            !trip_in_window(Some(0.1), 0.0, 0.1),
            "boundary trip is the later phase's"
        );
        assert!(trip_in_window(Some(0.1), 0.1, f64::INFINITY));
        // No trip at all is never in any window.
        assert!(!trip_in_window(None, 0.0, f64::INFINITY));
    }

    #[test]
    fn rearmed_supply_second_trip_is_owned_by_its_own_scenario_window() {
        // R37: a battery BMS runs in hiccup mode; it trips, the load drops below
        // reset so it re-arms, and it can trip again. A supply shared by inrush
        // [0, 0.1) and steady [0.1, +inf) that latches at 0.05 (inrush), re-arms,
        // then latches again at 0.15 (steady) must have BOTH windows own their own
        // trip. Recording only the FIRST latch (0.05) and folding it with
        // `trip_in_window` gives steady `trip_in_window(0.05, 0.1, inf) = false`,
        // a false GREEN over a window in which the pack demonstrably tripped.
        let mut trip_t: HashMap<String, Vec<f64>> = HashMap::new();
        trip_t.insert("BATT".into(), vec![0.05, 0.15]);
        let windows = vec![
            ScenarioWindow {
                id: "inrush".into(),
                start_s: 0.0,
                end_s: 0.1,
                nets: vec!["BATT".into()],
            },
            ScenarioWindow {
                id: "steady".into(),
                start_s: 0.1,
                end_s: f64::INFINITY,
                nets: vec!["BATT".into()],
            },
        ];
        let scoped = scope_protection_trips(&trip_t, &windows);
        assert_eq!(
            scoped.get(&("steady".into(), "BATT".into())),
            Some(&true),
            "steady must recover its own re-trip at 0.15 (was false under first-trip-only)"
        );
        assert_eq!(
            scoped.get(&("inrush".into(), "BATT".into())),
            Some(&true),
            "inrush still owns its first trip at 0.05"
        );
        // The first-latch-only view (a single f64 per net) loses the re-trip:
        assert!(
            !trip_in_window(trip_t["BATT"].first().copied(), 0.1, f64::INFINITY),
            "the first-trip-only fold was the bug: steady wrongly saw no trip"
        );
        // A single-latch supply is unchanged, and no-latch is never in any window.
        assert!(any_trip_in_window(Some(&vec![0.15]), 0.1, f64::INFINITY));
        assert!(!any_trip_in_window(Some(&vec![0.05]), 0.1, f64::INFINITY));
        assert!(!any_trip_in_window(None, 0.0, f64::INFINITY));
    }

    #[test]
    fn rail_window_sampling_stops_at_the_next_scenario_start() {
        // Round-29: rail_window sampling admitted frames with only the lower bound,
        // so a scenario-scoped window kept collecting min/max/dip/recovery to
        // end-of-run and a LATER phase's excursion bled into the earlier verdict.
        // With inrush=[0, 0.05) and steady=[0.05, +inf): a steady-phase sample at
        // 0.06 s must be sampled by steady, NOT by inrush.
        assert!(
            time_in_window(0.02, 0.0, 0.05),
            "inrush samples its own phase"
        );
        assert!(
            !time_in_window(0.06, 0.0, 0.05),
            "steady-phase sample excluded from inrush"
        );
        assert!(
            time_in_window(0.06, 0.05, f64::INFINITY),
            "steady samples its own phase"
        );
        // The boundary sample belongs to the later phase (half-open).
        assert!(!time_in_window(0.05, 0.0, 0.05));
        assert!(time_in_window(0.05, 0.05, f64::INFINITY));
        // The run-wide window (end +inf) spans everything.
        assert!(time_in_window(9.9, 0.0, f64::INFINITY));
    }

    #[test]
    fn net_window_last_v_is_the_settled_value_not_the_peak() {
        // R24: the settled report value is written by observe(); folding the
        // intra-frame extremes must widen min/max WITHOUT clobbering last_v.
        let mut w = NetWindow::new();
        w.observe(3.30); // settled final-chunk voltage
        w.fold(0.0); // an intra-frame dip
        w.fold(5.0); // an intra-frame peak
        assert_eq!(w.last_v, 3.30, "last_v must stay the settled value");
        assert_eq!(w.min_v, 0.0, "the dip still widens the window");
        assert_eq!(w.max_v, 5.0, "the peak still widens the window");
        assert_eq!(w.samples, 3);
    }

    // Replay a voltage series through the per-frame boot-coverage update and
    // return (first_cross_ms, first_drop_after_cross_ms).
    fn boot_track(series: &[(f64, f64)], level: f64) -> (Option<f64>, Option<f64>) {
        let key = ("CTRL".to_string(), level.to_bits());
        let mut cross: HashMap<(String, u64), f64> = HashMap::new();
        let mut drop: HashMap<(String, u64), f64> = HashMap::new();
        for &(t_ms, v) in series {
            boot_reach_update(&mut cross, &mut drop, key.clone(), v, level, t_ms);
        }
        (cross.get(&key).copied(), drop.get(&key).copied())
    }

    #[test]
    fn boot_reach_records_first_cross_and_first_drop() {
        // Driven up promptly and held to the end: first cross at 5, never drops.
        assert_eq!(
            boot_track(&[(0.0, 0.0), (5.0, 5.0), (10.0, 5.0), (50.0, 5.0)], 3.0),
            (Some(5.0), None)
        );
        // A one-frame glitch that then collapses: cross at 5, drop at 10; the
        // drop record is what lets the assertion refuse a glitch as a pass.
        assert_eq!(
            boot_track(&[(0.0, 0.0), (5.0, 5.0), (10.0, 0.0), (50.0, 0.0)], 3.0),
            (Some(5.0), Some(10.0))
        );
        // Dropped then recovered: cross at 5, FIRST drop at 10 (the recovery does
        // not erase that it fell, a deadline after 10 sees the break).
        assert_eq!(
            boot_track(&[(5.0, 5.0), (10.0, 0.0), (30.0, 5.0), (50.0, 5.0)], 3.0),
            (Some(5.0), Some(10.0))
        );
        // A late (post-deadline) release: cross at 5, drop at 50, a deadline of,
        // say, 10 ms is held through, so the assertion still passes.
        assert_eq!(
            boot_track(&[(5.0, 5.0), (10.0, 5.0), (50.0, 0.0)], 3.0),
            (Some(5.0), Some(50.0))
        );
        // Never reaches: no cross, no drop.
        assert_eq!(boot_track(&[(0.0, 0.0), (50.0, 1.0)], 3.0), (None, None));
    }

    #[test]
    fn references_sheetfile_matches_bare_and_subdir_values() {
        // Bare name in the same directory.
        let flat = r#"(property "Sheetfile" "pic_sockets.kicad_sch")"#;
        assert!(references_sheetfile(flat, "pic_sockets.kicad_sch"));
        assert!(!references_sheetfile(flat, "other.kicad_sch"));

        // Subdir-qualified value still matches by basename.
        let nested = r#"(property "Sheetfile" "sheets/power.kicad_sch")"#;
        assert!(references_sheetfile(nested, "power.kicad_sch"));

        // A different file that merely shares a prefix must not match.
        let near = r#"(property "Sheetfile" "power_supply.kicad_sch")"#;
        assert!(!references_sheetfile(near, "power.kicad_sch"));

        // Multiple Sheetfile entries: any one matching is enough.
        let many =
            r#"(property "Sheetfile" "a.kicad_sch") ... (property "Sheetfile" "b.kicad_sch")"#;
        assert!(references_sheetfile(many, "b.kicad_sch"));
        assert!(!references_sheetfile(many, "c.kicad_sch"));
    }

    #[test]
    fn normalize_path_resolves_dot_segments() {
        assert_eq!(
            normalize_path(Path::new("a/./b/../c")),
            PathBuf::from("a/c")
        );
    }

    #[test]
    fn windows_overlap_is_half_open_interval_test() {
        let w = [(0.001, 0.003)];
        // A frame fully inside a failed window overlaps.
        assert!(windows_overlap(&w, 0.0015, 0.0025));
        // A frame straddling the start overlaps.
        assert!(windows_overlap(&w, 0.0005, 0.0015));
        // Touching at the closed start counts (start < end and w.start < end).
        assert!(windows_overlap(&w, 0.0005, 0.0011));
        // Abutting exactly at the open end does NOT overlap ([start,end) is open).
        assert!(!windows_overlap(&w, 0.003, 0.004));
        // A frame entirely before the window does not overlap.
        assert!(!windows_overlap(&w, 0.0, 0.001));
        // No failed windows: never overlaps.
        assert!(!windows_overlap(&[], 0.0, 1.0));
    }

    #[test]
    fn decoupling_override_with_unknown_ref_fails_loud() {
        // R33: a `[[decoupling.override]]` keyed by a ref that names no board
        // capacitor was silently dropped (the per-cap lookup never matched), so
        // the parasitics the user opted into were never applied and a rail_window
        // check saw a cleaner-than-real rail, a false GREEN. The ref must be
        // validated like every other, so a typo fails loud.
        let spec: Spec = toml::from_str(
            r#"
name = "t"
board = "board.kicad_pcb"
duration_ms = 10

[decoupling]
parasitics = false

[[decoupling.override]]
ref = "C10"
esr_ohms = 0.5
"#,
        )
        .expect("valid toml");

        // The board has C110, not C10 (a typo): must be rejected.
        let err = check_component_refs(&spec, &["C110".to_string()])
            .expect_err("an unknown decoupling override ref must fail loud");
        assert!(
            matches!(&err, SpecError::Invalid(m) if m.contains("decoupling override") && m.contains("C10")),
            "expected a decoupling-override ref error naming C10, got {err:?}"
        );

        // With the correct ref on the board it validates.
        assert!(
            check_component_refs(&spec, &["C10".to_string()]).is_ok(),
            "a decoupling override matching a real cap ref must validate"
        );
    }

    // ── qemu_bus_slave_warnings unit tests ───────────────────────────────────

    /// Helper: write a minimal board + spec to temp with a unique name, load
    /// and return the Spec. Tests run in parallel so each gets its own file.
    fn load_spec_str(test_name: &str, spec_toml: &str) -> Spec {
        let dir = std::env::temp_dir().join("hauksbee_ci_warn_tests");
        std::fs::create_dir_all(&dir).unwrap();

        // Minimal board: a single pull-down resistor. No MCU footprint here,
        // `qemu_bus_slave_warnings` receives the backend list as an argument
        // rather than discovering it from the board, so the board content does
        // not need an MCU part.
        let board_content = r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "GND")
  (net 2 "+3V3")
  (module Resistor_THT:R_Axial_DIN0207_L6.3mm_D2.5mm (layer F.Cu)
    (at 100 100)
    (fp_text reference R1 (at 0 0) (layer F.SilkS))
    (fp_text value 10k (at 0 2) (layer F.Fab))
    (pad 1 thru_hole circle (at 0 0) (net 2 "+3V3"))
    (pad 2 thru_hole circle (at 2 0) (net 1 "GND"))
  )
)
"#;
        // Give each test its own board path so parallel runs don't collide.
        let board_path = dir.join(format!("{test_name}_board.kicad_pcb"));
        std::fs::write(&board_path, board_content).unwrap();

        // Build the spec TOML: board path first (required field), then the
        // caller-supplied body. Use a unique file name per test.
        let full_spec = format!("board = \"{}\"\n{}", board_path.display(), spec_toml);
        let spec_path = dir.join(format!("{test_name}.toml"));
        std::fs::write(&spec_path, &full_spec).unwrap();

        Spec::load(&spec_path).expect("spec should load")
    }

    // Minimal assert block required by Spec::validate (a spec with no asserts
    // is rejected as vacuously passing). Append this to every test spec body.
    const MINIMAL_ASSERT: &str = r#"
[[assert]]
kind = "voltage"
net = "+3V3"
min = 3.0
"#;

    /// No QEMU backends -> no warnings regardless of peripherals.
    #[test]
    fn no_qemu_backends_produces_no_warnings() {
        let body = format!("duration_ms = 10\n{MINIMAL_ASSERT}");
        let spec = load_spec_str("no_qemu_warn", &body);
        let warnings = qemu_bus_slave_warnings(&spec, &[]);
        assert!(
            warnings.is_empty(),
            "no qemu backends -> no warnings, got: {warnings:?}"
        );
    }

    /// QEMU backend + i2c_lm75 peripheral -> warning produced naming both.
    #[test]
    fn qemu_backend_with_i2c_lm75_warns() {
        let body = format!(
            r#"duration_ms = 10

[[peripheral]]
id = "BME280"
type = "i2c_lm75"
address = 0x76
{MINIMAL_ASSERT}"#
        );
        let spec = load_spec_str("i2c_lm75_qemu", &body);
        let backends = vec!["qemu:esp32c3".to_string()];
        let warnings = qemu_bus_slave_warnings(&spec, &backends);
        assert_eq!(warnings.len(), 1, "exactly one warning for one bus slave");
        let w = &warnings[0];
        assert!(w.contains("BME280"), "warning names the peripheral id: {w}");
        assert!(w.contains("i2c_lm75"), "warning names the kind: {w}");
        assert!(w.contains("qemu:esp32c3"), "warning names the backend: {w}");
        assert!(w.contains("NO-OP"), "warning says NO-OP: {w}");
        assert!(w.contains("I2C"), "warning names bus type: {w}");
    }

    /// QEMU backend + spi_mcp3008 peripheral -> warning produced.
    #[test]
    fn qemu_backend_with_spi_mcp3008_warns() {
        let body = format!(
            r#"duration_ms = 10

[[peripheral]]
id = "ADC1"
type = "spi_mcp3008"
vref = 3.3
{MINIMAL_ASSERT}"#
        );
        let spec = load_spec_str("spi_mcp3008_qemu", &body);
        let backends = vec!["qemu:esp32".to_string()];
        let warnings = qemu_bus_slave_warnings(&spec, &backends);
        assert_eq!(warnings.len(), 1);
        let w = &warnings[0];
        assert!(w.contains("ADC1"), "names peripheral: {w}");
        assert!(w.contains("spi_mcp3008"), "names kind: {w}");
        assert!(w.contains("SPI"), "names bus type: {w}");
        assert!(w.contains("qemu:esp32"), "names backend: {w}");
    }

    /// QEMU backend + declarative [[sensor]] -> warning produced.
    #[test]
    fn qemu_backend_with_declarative_sensor_warns() {
        let body = format!(
            r#"duration_ms = 10

[[sensor]]
id = "U2_bme280"
spec = """
[sensor]
name = "BME280_stub"
bus  = "i2c"
i2c_address = 0x76
"""
{MINIMAL_ASSERT}"#
        );
        let spec = load_spec_str("sensor_qemu", &body);
        let backends = vec!["qemu:esp32c3".to_string()];
        let warnings = qemu_bus_slave_warnings(&spec, &backends);
        assert_eq!(warnings.len(), 1, "one warning for the declarative sensor");
        let w = &warnings[0];
        assert!(w.contains("U2_bme280"), "names sensor id: {w}");
        assert!(w.contains("qemu:esp32c3"), "names backend: {w}");
        assert!(w.contains("NO-OP"), "says NO-OP: {w}");
    }

    /// Non-bus-slave peripheral kinds (pushbutton, stimulus, vcd_sink) on a
    /// QEMU backend must NOT produce warnings.
    #[test]
    fn qemu_backend_non_bus_slave_no_warning() {
        let body = format!(
            r#"duration_ms = 10

[[peripheral]]
id = "BTN1"
type = "pushbutton"
net  = "+3V3"
{MINIMAL_ASSERT}"#
        );
        let spec = load_spec_str("pushbutton_qemu", &body);
        let backends = vec!["qemu:esp32c3".to_string()];
        let warnings = qemu_bus_slave_warnings(&spec, &backends);
        assert!(
            warnings.is_empty(),
            "pushbutton on qemu should not warn: {warnings:?}"
        );
    }

    /// Multiple bus-slave items -> one warning per item.
    #[test]
    fn qemu_backend_multiple_slaves_warn_per_item() {
        let body = format!(
            r#"duration_ms = 10

[[peripheral]]
id = "EEPROM1"
type = "i2c_eeprom"
address = 0x50

[[peripheral]]
id = "ADC1"
type = "spi_mcp3008"
vref = 3.3
{MINIMAL_ASSERT}"#
        );
        let spec = load_spec_str("multi_slave_qemu", &body);
        let backends = vec!["qemu:esp32s3".to_string()];
        let warnings = qemu_bus_slave_warnings(&spec, &backends);
        assert_eq!(
            warnings.len(),
            2,
            "one warning per bus slave, got: {warnings:?}"
        );
        assert!(warnings.iter().any(|w| w.contains("EEPROM1")));
        assert!(warnings.iter().any(|w| w.contains("ADC1")));
    }

    /// Renode backend (not QEMU) with a bus slave -> NO warning.
    /// `qemu_bus_slave_warnings` takes the backends list as a parameter;
    /// an empty list means no QEMU backends (as would be the case for Renode
    /// or AVR), so no warnings should fire.
    #[test]
    fn renode_backend_no_warning() {
        let body = format!(
            r#"duration_ms = 10

[[peripheral]]
id = "TEMP_SENSOR"
type = "i2c_lm75"
address = 0x48
{MINIMAL_ASSERT}"#
        );
        let spec = load_spec_str("renode_no_warn", &body);
        // Empty list = no QEMU backends (Renode/AVR boards).
        let backends: Vec<String> = vec![];
        let warnings = qemu_bus_slave_warnings(&spec, &backends);
        assert!(
            warnings.is_empty(),
            "non-qemu backends must not warn: {warnings:?}"
        );
    }

    /// protection_trip on a supply leg with no protection model must REFUSE at
    /// load. Before the guard, "[PASS] +5V protection held" was reported while
    /// 5 A was drawn from a 500 mA USB profile: the USB/bench foldback never
    /// sets a trip latch, and an unprotected battery has nothing to latch, so
    /// the assertion was structurally green.
    #[test]
    fn protection_trip_on_unprotected_supply_refuses_at_load() {
        let dir =
            std::env::temp_dir().join(format!("hauksbee-ci-prot-guard-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let load = |name: &str, body: &str| -> Spec {
            let p = dir.join(name);
            std::fs::write(&p, body).unwrap();
            Spec::load(&p).expect("spec loads")
        };
        let bound = BoundBoard {
            name: "prot-guard".to_string(),
            circuit: hauksbee_ir::Circuit::new(),
            net_nodes: HashMap::new(),
            net_names: Vec::new(),
            digital: Vec::new(),
            mcus: Vec::new(),
            dnp_mcus: Vec::new(),
            component_kinds: HashMap::new(),
            input_sources: HashMap::new(),
            supplies: Vec::new(),
            behavioral: Vec::new(),
            device_meta: Vec::new(),
            dacs: Vec::new(),
            report: hauksbee_engine::report::BindReport::default(),
        };
        let assert_block = "[[assert]]\nkind = \"protection_trip\"\n\
             supply_net = \"+5V\"\nexpect_trip = false\n";

        // No [[supply]] on the net at all: an ideal rail, nothing to trip.
        let spec = load(
            "none.toml",
            &format!("board = \"b.kicad_pcb\"\nduration_ms = 1\n{assert_block}"),
        );
        let err = check_trackable_assert_refs(&spec, &bound)
            .expect_err("ideal auto-rail carries no protection model");
        assert!(
            err.to_string().contains("no [[supply]]"),
            "names the missing supply: {err}"
        );

        // A USB profile: current-limited by foldback, but no protection latch.
        let spec = load(
            "usb.toml",
            &format!(
                "board = \"b.kicad_pcb\"\nduration_ms = 1\n\
                 [[supply]]\nnet = \"+5V\"\nkind = \"usb\"\nusb = \"5v0.5a\"\n{assert_block}"
            ),
        );
        let err = check_trackable_assert_refs(&spec, &bound)
            .expect_err("usb foldback is not a protection latch");
        assert!(
            err.to_string().contains("voltage foldback"),
            "explains the usb refusal: {err}"
        );

        // A battery without protection_trip_a: an unprotected pack.
        let spec = load(
            "batt-unprot.toml",
            &format!(
                "board = \"b.kicad_pcb\"\nduration_ms = 1\n\
                 [[supply]]\nnet = \"+5V\"\nkind = \"battery\"\nchemistry = \"liion\"\n{assert_block}"
            ),
        );
        let err = check_trackable_assert_refs(&spec, &bound)
            .expect_err("an unprotected battery has nothing to latch");
        assert!(
            err.to_string().contains("protection_trip_a"),
            "points at the missing field: {err}"
        );

        // A protected battery: the guard accepts, the trip is observable.
        let spec = load(
            "batt-prot.toml",
            &format!(
                "board = \"b.kicad_pcb\"\nduration_ms = 1\n\
                 [[supply]]\nnet = \"+5V\"\nkind = \"battery\"\nchemistry = \"liion\"\n\
                 protection_trip_a = 1.0\nprotection_delay_ms = 2.0\n{assert_block}"
            ),
        );
        check_trackable_assert_refs(&spec, &bound)
            .expect("a protected battery pack is a checkable guard");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod spi_cs_source_tests {
    //! Which of the two exact-framing routes a SPI peripheral takes, and the
    //! precedence between them.
    //!
    //! The model-role route exists so a modeled part does not need `cs_net`
    //! written out by hand. It must never take that decision AWAY from the spec:
    //! a pad map can be wrong, and a chip select can be buffered through
    //! something the model cannot see, so a hand-declared `cs_net` has to win.

    use super::*;
    use hauksbee_extract::{Component, Net, Pin};

    fn peripheral(toml_src: &str) -> crate::spec::PeripheralSpec {
        toml::from_str(toml_src).expect("peripheral shape")
    }

    /// One assembled, CS-wired 25xx EEPROM at U5 (pad 1 = `cs` in the model DB),
    /// with a second net available so precedence is observable.
    fn board() -> ExtractedBoard {
        ExtractedBoard {
            name: "spi-cs-source".to_string(),
            nets: vec![
                Net {
                    id: 1,
                    name: "EE_CS".to_string(),
                },
                Net {
                    id: 2,
                    name: "OVERRIDE_CS".to_string(),
                },
            ],
            components: vec![Component {
                reference: "U5".to_string(),
                value: "25LC256-I/SN".to_string(),
                lib_id: String::new(),
                footprint: "Package_SO:SOIC-8_3.9x4.9mm_P1.27mm".to_string(),
                position: None,
                layer: String::new(),
                properties: Vec::new(),
                dnp: false,
                pins: vec![Pin {
                    number: "1".to_string(),
                    net: Some(1),
                    function: String::new(),
                    kind: String::new(),
                    position: None,
                }],
            }],
        }
    }

    #[test]
    fn a_ref_with_no_cs_net_takes_the_model_role_route() {
        let lib = hauksbee_models::ModelLibrary::builtin();
        let p = peripheral("id = \"U5\"\ntype = \"spi_eeprom\"\nref = \"U5\"\n");
        assert_eq!(
            cs_net_name(&p, &board(), &lib).expect("no mismatch"),
            Some(("EE_CS".to_string(), CsProvenance::ModelRoles)),
            "with no cs_net declared, the bound model's `cs` pad supplies the net"
        );
    }

    #[test]
    fn a_declared_cs_net_wins_over_the_model_role() {
        let lib = hauksbee_models::ModelLibrary::builtin();
        let p = peripheral(
            "id = \"U5\"\ntype = \"spi_eeprom\"\nref = \"U5\"\ncs_net = \"OVERRIDE_CS\"\n",
        );
        assert_eq!(
            cs_net_name(&p, &board(), &lib).expect("no mismatch"),
            Some(("OVERRIDE_CS".to_string(), CsProvenance::SpecDeclared)),
            "an explicit cs_net must override the model's pad map, which is how a wrong \
             or incomplete model entry stays correctable from the spec"
        );
    }

    #[test]
    fn a_declared_cs_net_needs_no_ref_at_all() {
        // The pre-existing route, unchanged: no `ref`, no model, just a net name.
        let lib = hauksbee_models::ModelLibrary::builtin();
        let p = peripheral("id = \"EE\"\ntype = \"spi_eeprom\"\ncs_net = \"EE_CS\"\n");
        assert_eq!(
            cs_net_name(&p, &board(), &lib).expect("no mismatch"),
            Some(("EE_CS".to_string(), CsProvenance::SpecDeclared))
        );
    }

    #[test]
    fn neither_route_leaves_the_bus_on_the_heuristic() {
        let lib = hauksbee_models::ModelLibrary::builtin();
        let p = peripheral("id = \"EE\"\ntype = \"spi_eeprom\"\n");
        assert_eq!(
            cs_net_name(&p, &board(), &lib).expect("no mismatch"),
            None,
            "no cs_net and no ref means no CS net; the bus reports heuristic framing"
        );
    }

    /// A `ref` that names a real, assembled, modelled part of the WRONG kind must
    /// not supply a CS net. Pointing a `spi_eeprom` at the board's MCP3008 finds a
    /// genuine `cs` role on a genuine part, and taking it would frame the EEPROM's
    /// transactions off the ADC's chip-select while reporting `exact`. Dropping to
    /// the heuristic is the conservative answer, and the heuristic disclosure then
    /// fires on every surface, so the bus is not quietly trusted.
    #[test]
    fn a_ref_naming_the_wrong_spi_part_supplies_no_cs_net() {
        let lib = hauksbee_models::ModelLibrary::builtin();
        let mut b = board();
        b.components[0].value = "MCP3008-I/SL".to_string();
        b.components[0].pins[0].number = "10".to_string(); // the MCP3008's cs pad

        // Sanity: as its OWN kind the part does resolve, so the refusal below is
        // about the mismatch and not about the fixture failing to bind.
        let matched = peripheral("id = \"U5\"\ntype = \"spi_mcp3008\"\nref = \"U5\"\n");
        assert_eq!(
            cs_net_name(&matched, &b, &lib).expect("the matching kind is not a mismatch"),
            Some(("EE_CS".to_string(), CsProvenance::ModelRoles)),
            "an MCP3008 under the spi_mcp3008 kind must still resolve its cs pad"
        );

        let mismatched = peripheral("id = \"U5\"\ntype = \"spi_eeprom\"\nref = \"U5\"\n");
        let err = cs_net_name(&mismatched, &b, &lib)
            .expect_err(
                "a spi_eeprom pointed at an MCP3008 must be refused, not quietly \
                         downgraded to the known-wrong heuristic",
            )
            .to_string();
        assert!(
            err.contains("spi_eeprom") && err.contains("mcp3008") && err.contains("U5"),
            "the error must name the kind, the model it actually bound, and the ref: {err}"
        );
    }

    /// The mismatch check must only fire when it can actually judge. A model id no
    /// built-in kind claims may come from a user model pack, and refusing it would
    /// break the very extensibility the pin-role route is built on.
    #[test]
    fn an_unrecognised_model_id_is_not_treated_as_a_mismatch() {
        assert!(
            !crate::spec::is_builtin_spi_slave_model_id("some_user_pack_flash"),
            "a user pack's id is not a built-in SPI slave id"
        );
        assert_eq!(
            crate::spec::builtin_model_id_for_spi_kind("spi_eeprom"),
            Some("eeprom_25xx_spi")
        );
        assert_eq!(
            crate::spec::builtin_model_id_for_spi_kind("pushbutton"),
            None,
            "a non-SPI kind claims no model"
        );
    }

    #[test]
    fn a_ref_naming_an_unknown_component_is_a_loud_error() {
        // The silent-degradation hole this closes: a typo'd `ref` resolves no
        // component, so no model, so no CS net, and the bus would quietly drop to
        // the framing heuristic with nothing said. It must fail at load instead,
        // exactly as a typo'd `cs_net` already does.
        let spec: Spec = toml::from_str(
            r#"
board = "board.kicad_pcb"
[[peripheral]]
id = "EE"
type = "spi_eeprom"
ref = "U55"
"#,
        )
        .expect("spec shape");
        let errs = component_ref_errors(&spec, &["U5".to_string()]);
        assert_eq!(errs.len(), 1, "{errs:?}");
        let msg = errs[0].to_string();
        assert!(
            msg.contains("U55") && msg.contains("SPI peripheral"),
            "the error must name the bad ref and say where it came from: {msg}"
        );
    }

    #[test]
    fn a_correct_ref_raises_no_error() {
        let spec: Spec = toml::from_str(
            r#"
board = "board.kicad_pcb"
[[peripheral]]
id = "EE"
type = "spi_eeprom"
ref = "U5"
"#,
        )
        .expect("spec shape");
        assert!(component_ref_errors(&spec, &["U5".to_string()]).is_empty());
    }
}
