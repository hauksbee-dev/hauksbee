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
#[derive(Debug, Clone)]
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
    /// Total simulated time (ms).
    pub sim_ms: f64,
    /// Boot-coverage tracking: first time (ms) each watched (net, level-bits)
    /// pair was seen at or above its level, i.e. when the firmware first drove
    /// the control net to its defined level. Absent = the net never reached the
    /// level (see `driven_nets` / `drive_direction_observable` to tell a driven-
    /// but-below-threshold net from a genuinely undriven one).
    pub first_reach_ms: HashMap<(String, u64), f64>,
    /// Nets the firmware drove to a *defined* level (HIGH or LOW) during the run,
    /// from the scheduler's `firmware_driven_nets`. Lets a below-threshold
    /// boot-coverage result say "driven but never exceeded X V" instead of
    /// falsely claiming the pin was left Hi-Z / undefined.
    pub driven_nets: std::collections::HashSet<String>,
    /// Whether this run's backend can report pin drive *direction*. True for the
    /// in-process AVR backend (reads DDR, so a held-LOW pin is known driven);
    /// false for the external Renode/QEMU backends (drive state comes only from
    /// observed edges, so absence from `driven_nets` is ambiguous, not proof of
    /// Hi-Z). Keeps the "undriven" diagnosis from over-claiming on those backends.
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
    /// assertions — recording every net's series unconditionally would cost
    /// memory on long runs for nothing.
    pub net_series: HashMap<String, Vec<(f64, f64)>>,
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

/// Run the spec and return one [`RunOutcome`] per seed (>=1).
pub fn run_spec(spec: &Spec) -> Result<Vec<RunOutcome>, SpecError> {
    run_spec_seeded(spec, None)
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
    // Read + extract the board once; clone per seed (binding mutates nothing on
    // the ExtractedBoard, but overrides do, so we re-derive per run).
    let board_path = spec.board_path();
    let base = load_board(&board_path)?;

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

    // Small-signal AC analysis linearises about the DC operating point. With no
    // tolerances that point is seed-independent, so compute the sweep once and
    // share it. Toleranced values move the bias point, so each member gets its
    // own sweep — sharing one would silently pin the AC results to nominal.
    let shared_ac = if spec.ac.is_some() && tols.is_empty() {
        Some(compute_ac(spec, &base, &[], lib)?)
    } else {
        None
    };

    let mut outcomes = Vec::with_capacity(plans.len());
    for plan in &plans {
        let mut outcome = run_one(spec, &base, &thresholds, plan, lib)?;
        outcome.ac = match &shared_ac {
            Some(ac) => Some(ac.clone()),
            None if spec.ac.is_some() => Some(compute_ac(spec, &base, &plan.values, lib)?),
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

/// Load and extract the board file the spec points at, dispatching on file
/// type. This is what makes a `board = "thing.kicad_sch"` spec just work.
///
/// A `.kicad_sch` is loaded *by path* (not from its text) so the sheet
/// hierarchy resolves: a schematic's sub-sheets live in sibling files, and only
/// the path-based entry point follows them. Loading the root text alone would
/// silently drop every component on a sub-sheet, producing a partial netlist
/// that passes vacuous checks. Everything else (`.kicad_pcb`, `.net`, Eagle
/// `.brd`, IPC-D-356) carries full connectivity in one file and is sniffed from
/// its content as before.
pub(crate) fn load_board(board_path: &Path) -> Result<ExtractedBoard, SpecError> {
    let ext = board_path.extension().and_then(|e| e.to_str());

    // Board-as-Code (`.board`) is a source format, not an extractable board:
    // the extractor only knows the compiled layout/netlist formats, so it would
    // otherwise fail with a cryptic "unrecognized board format" list that never
    // mentions `.board`. Both `hauksbee-ci run` and `hauksbee-ci init` reach
    // here, so catching it at this one seam gives both the exact recompile
    // command (in-process from-code recompilation is a place+route step owned by
    // the `hauksbee` binary, so we point at it rather than duplicating it).
    if ext.map(|e| e.eq_ignore_ascii_case("board")).unwrap_or(false) {
        let stem = board_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("board");
        return Err(SpecError::Invalid(format!(
            "'{}' is a Board-as-Code source file, which hauksbee-ci cannot load \
             directly. Compile it to a layout first, then point the spec/init at \
             that:\n    hauksbee from-code {} --out {stem}.kicad_pcb --route\n    \
             hauksbee-ci init {stem}.kicad_pcb",
            board_path.display(),
            board_path.display(),
        )));
    }

    let is_sch = ext
        .map(|e| e.eq_ignore_ascii_case("kicad_sch"))
        .unwrap_or(false);

    if is_sch {
        // Guard against pointing the spec at a sub-sheet rather than the
        // hierarchy root: a sub-sheet on its own is an incomplete board.
        if let Some(root) = parent_schematic_of(board_path) {
            return Err(SpecError::Invalid(format!(
                "board {} is a sub-sheet of {}. Point the spec at the hierarchy \
                 root ({}) so the whole design is loaded, not one page of it",
                board_path.display(),
                root.display(),
                root.display(),
            )));
        }
        ExtractedBoard::from_kicad_schematic_path(board_path)
            .map_err(|e| SpecError::Invalid(format!("extracting schematic: {e}")))
    } else {
        let text = std::fs::read_to_string(board_path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                SpecError::Io(format!(
                    "no board file at '{}' (resolved from the spec's `board` key). \
                     Check that path; it is taken relative to the spec file's directory",
                    board_path.display()
                ))
            } else {
                SpecError::Io(format!("reading board {}: {e}", board_path.display()))
            }
        })?;
        ExtractedBoard::from_auto(&text)
            .map_err(|e| SpecError::Invalid(format!("extracting board: {e}")))
    }
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

/// Validate every component reference the spec names against the board, with
/// near-match suggestions. Covers `[[override]]` refs and `max_current` assert
/// refs (overrides are also checked again in `apply_overrides`, but doing it
/// here means a typo'd `max_current` ref fails loudly instead of passing as an
/// untracked component).
fn check_component_refs(spec: &Spec, known_refs: &[String]) -> Result<(), SpecError> {
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
    for (reference, ctx) in named {
        if !set.contains(reference) {
            let near = crate::error::near_matches(reference, known_refs, 5);
            let hint = if near.is_empty() {
                String::new()
            } else {
                format!(" — did you mean: {}?", near.join(", "))
            };
            return Err(SpecError::Invalid(format!(
                "{ctx} references unknown component '{reference}'{hint}"
            )));
        }
    }
    Ok(())
}

/// Fail loud when a `max_current` / `max_temp` assertion names a component the
/// engine cannot actually measure. `check_component_refs` catches typos against
/// the board; this catches the subtler hole where the ref is a real component
/// but of a kind that is never tracked, so the guard would report a green pass
/// without ever being evaluated:
///
///   - peak current is recorded only for the device kinds
///     `update_peak_currents` walks (resistors and diodes, by exact device
///     name), so a `max_current` on a capacitor / IC / transistor package
///     would always take the "no current data" branch;
///   - junction temperature exists only for stress-monitored devices (the
///     binder's `DeviceMeta` list), so a `max_temp` on an unmonitored kind
///     (MCU, connector, anything the model library could not resolve) would
///     always take the "no dissipation measured" branch.
///
/// Runs post-bind (the bound circuit is what decides trackability), before the
/// engine is built, so the spec is rejected up front rather than reported green.
fn check_trackable_assert_refs(spec: &Spec, bound: &BoundBoard) -> Result<(), SpecError> {
    // Current tracking covers exactly what `update_peak_currents` records:
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
    // Thermal tracking covers every stress-monitored device. Multi-unit
    // packages stamp per-unit metas ("IC3906_q0"), so accept a ref whose units
    // are monitored too.
    let thermally_tracked = |r: &str| {
        bound.device_meta.iter().any(|m| {
            m.reference == r
                || m.reference
                    .strip_prefix(r)
                    .is_some_and(|s| s.starts_with("_q") || s.starts_with("_s"))
        })
    };
    for a in &spec.asserts {
        let Some(reference) = &a.reference else { continue };
        match a.kind.as_str() {
            "max_current" if !current_tracked.contains(reference.as_str()) => {
                return Err(SpecError::Invalid(format!(
                    "max_current assert references '{reference}', but peak current is only \
                     measured for resistors and diodes; this component binds as a kind whose \
                     through-current is never tracked, so the guard would report green without \
                     ever being evaluated — point the assert at a resistor/diode in the same \
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
            _ => {}
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
                let near = crate::error::near_matches(&ov.reference, &refs, 5);
                let hint = if near.is_empty() {
                    String::new()
                } else {
                    format!(" — did you mean: {}?", near.join(", "))
                };
                SpecError::Invalid(format!(
                    "override references unknown component '{}'{hint}",
                    ov.reference
                ))
            })?;
        comp.value = ov.value.clone();
    }
    Ok(board)
}

/// Write one ensemble member's sampled tolerance values onto the board (the
/// same pre-binding seam `apply_overrides` uses). Every reference was resolved
/// against the board in `tolerance::resolve`, so lookups here cannot miss.
/// Rust's `f64` Display never uses exponent notation, so the written value
/// round-trips through the ordinary component-value parser.
fn apply_sampled_values(board: &mut ExtractedBoard, sampled: &[crate::tolerance::SampledValue]) {
    for sv in sampled {
        if let Some(comp) = board
            .components
            .iter_mut()
            .find(|c| c.reference == sv.reference)
        {
            comp.value = format!("{}", sv.si);
        }
    }
}

fn run_one(
    spec: &Spec,
    base: &ExtractedBoard,
    thresholds: &[f64],
    plan: &crate::tolerance::SeedPlan,
    lib: &ModelLibrary,
) -> Result<RunOutcome, SpecError> {
    let seed = plan.seed;
    let mut board = apply_overrides(spec, base)?;
    apply_sampled_values(&mut board, &plan.values);
    let mut bound = bind_board(&board, lib);

    // The as-built overlay comes first: it is BOARD state (the physical rework
    // record — cuts, jumpers, fitted values), so it lands before any harness
    // attachment, at the same post-bind seam the engine CLI's --asbuilt uses.
    if let Some(asbuilt_path) = spec.asbuilt_path() {
        let overlay = hauksbee_engine::asbuilt::AsBuiltOverlay::load(&asbuilt_path)
            .map_err(|e| SpecError::Invalid(e.to_string()))?;
        overlay
            .apply(&mut bound)
            .map_err(|e| SpecError::Invalid(e.to_string()))?;
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
            // Existence is now guaranteed, so canonicalize for Renode (which
            // resolves relative paths against its own temp working directory).
            Some(p.canonicalize().unwrap_or(p))
        }
        None => None,
    };

    // Capture the QEMU backend strings before `bound` is consumed by
    // `from_bound`. We use these below to warn when bus-slave peripherals or
    // declarative sensors are attached to a QEMU-backed board: the QEMU I2C/SPI
    // bridge is deferred, so those slaves will silently not respond.
    // AVR (simavr) and Renode backends have a working bus bridge — no warning.
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
    let vcd_targets = attach_peripherals(spec, &board, &net_node, engine.scheduler_mut())?;

    // Attach declarative sensors (RegisterMapSensor) to their buses.
    attach_sensors(spec, engine.scheduler_mut())?;

    // Warn when bus-slave peripherals or declarative sensors are attached on a
    // QEMU backend. The QEMU I2C/SPI bus bridge is not yet implemented, so
    // these slaves are a no-op: the firmware's bus transactions will time-out or
    // receive garbage, potentially causing failures that look unrelated to the
    // missing sensor. Surface the mismatch now, before the co-sim starts, so the
    // user doesn't spend 45 minutes chasing an unrelated assertion failure.
    //
    // AVR (simavr) and Renode backends have a working bus bridge — no warning.
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

    let mut windows: HashMap<(String, u64), NetWindow> = HashMap::new();
    let mut uart: HashMap<String, String> = HashMap::new();
    let mut faults: Vec<RunFault> = Vec::new();
    let mut peak_current: HashMap<String, f64> = HashMap::new();
    let mut peak_temp_c: HashMap<String, f64> = HashMap::new();
    // Per-scenario rail-window timeseries and protection-trip tracking.
    let mut rail_windows: HashMap<(String, String), crate::scenarios::RailWindow> = HashMap::new();
    let mut protection_tripped: HashMap<String, bool> = HashMap::new();

    // Boot-coverage watch list: every (net, required-level) a boot-coverage
    // assertion names. We record the first frame each net reaches its level.
    let boot_watch: Vec<(String, f64)> = spec
        .asserts
        .iter()
        .filter(|a| a.kind == "boot-coverage")
        .filter_map(|a| Some((a.net.clone()?, a.min?)))
        .collect();
    let mut first_reach_ms: HashMap<(String, u64), f64> = HashMap::new();
    let mut first_fault_ms: Option<f64> = None;

    // Hardware-trace watch list: the nets any `hwtrace` assertion's trace.toml
    // probes. Loading the traces here (fail-loud, before the sim spends its
    // minutes) also validates them, so a malformed trace aborts the run with a
    // named error instead of failing every feature at evaluation time.
    let hwtrace_nets = crate::hwtrace::assert_nets(spec)?;
    let mut net_series: HashMap<String, Vec<(f64, f64)>> = HashMap::new();

    while t < total_s - 1e-12 {
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
        let analog_ok = !windows_overlap(engine.scheduler().failed_windows(), frame_start_s, frame.t);

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
        // Boot-coverage: track each watched net reaching AND HOLDING its
        // required driven level (the firmware actively driving the control net
        // and keeping it there). `boot_coverage_update` re-arms the moment the
        // net drops back below level, so a one-frame glitch above threshold that
        // then collapses cannot latch a pass — first_reach_ms ends up holding
        // the start of the run's final continuous hold, or nothing if the net is
        // not held to the end. Skipped on a held-stale frame so a stale voltage
        // cannot fake a reach.
        if analog_ok {
            for (net, level) in &boot_watch {
                let key = (net.clone(), level.to_bits());
                if let Some(&v) = frame.net_voltages.get(net) {
                    boot_coverage_update(&mut first_reach_ms, key, v, *level, t_ms);
                }
            }
            // Per-net windows for every threshold this frame has passed.
            for &thr in thresholds {
                if t_ms + 1e-9 >= thr {
                    for (name, &v) in &frame.net_voltages {
                        let key = (name.clone(), thr.to_bits());
                        windows.entry(key).or_insert_with(NetWindow::new).observe(v);
                    }
                }
            }
            // Hardware-trace waveforms: record the probed nets' voltages at the
            // frame cadence, for feature extraction against the captured trace.
            for net in &hwtrace_nets {
                if let Some(&v) = frame.net_voltages.get(net) {
                    net_series.entry(net.clone()).or_default().push((frame.t, v));
                }
            }

            // Peak current for monitored components.
            update_peak_currents(&engine, &net_node, &mut peak_current);

            // Peak steady-state junction temperature for dissipating components.
            for (reference, tj) in engine.scheduler().temp_states() {
                let e = peak_temp_c.entry(reference).or_insert(f64::NEG_INFINITY);
                if tj.is_finite() && tj > *e {
                    *e = tj;
                }
            }

            // Scenario rail windows: for each scenario window active at this time,
            // record the referenced rails' voltages into the window timeseries.
            for sw in &scenario_windows {
                if frame.t + 1e-12 >= sw.start_s {
                    for net in &sw.nets {
                        if let Some(&v) = frame.net_voltages.get(net) {
                            rail_windows
                                .entry((sw.id.clone(), net.clone()))
                                .or_default()
                                .observe(frame.t, v);
                        }
                    }
                }
            }
        }
        // Protection-trip tracking: read the sticky "ever tripped" flag so a
        // trip that occurs and re-arms within one coarse frame is still caught.
        for leg in &engine.scheduler().supplies {
            if leg.supply.protection_ever_tripped() {
                protection_tripped.insert(leg.net_name.clone(), true);
            } else {
                protection_tripped
                    .entry(leg.net_name.clone())
                    .or_insert(false);
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
    let toggles: HashMap<String, u64> = engine
        .scheduler()
        .stats
        .iter()
        .map(|(n, st)| (n.clone(), st.toggles))
        .collect();

    // Snapshot peripheral state for assertions, and dump any VCD sinks.
    let peripherals = snapshot_peripherals(spec, &engine, &vcd_targets);

    // Drive-state metadata for the boot-coverage diagnosis: which nets the firmware
    // actually drove to a defined level, and whether this backend can even report
    // drive direction (only the in-process AVR backend can; the external
    // Renode/QEMU backends see drive state only through observed edges).
    let driven_nets: std::collections::HashSet<String> =
        engine.scheduler().firmware_driven_nets().into_iter().collect();
    let drive_direction_observable = !engine.scheduler().has_external_backend();

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
        sim_ms: engine.scheduler().sim_time * 1000.0,
        first_reach_ms,
        driven_nets,
        drive_direction_observable,
        first_fault_ms,
        ac: None,
        analog_valid,
        failed_windows,
        analog_abort,
        sampled_values: plan.values.clone(),
        net_series,
    })
}

/// Does `[start_s, end_s)` overlap any failed-analog window in `windows`? Used to
/// gate a frame's aggregates and (via the outcome) to mark overlapping assertions
/// INVALID. Standard half-open interval overlap: `start < w.end && w.start < end`.
fn windows_overlap(windows: &[(f64, f64)], start_s: f64, end_s: f64) -> bool {
    windows
        .iter()
        .any(|&(ws, we)| start_s < we && ws < end_s)
}

/// Update the boot-coverage reach record for one watched net at one frame.
///
/// Records the start of the current continuous "at/above level" hold (once),
/// and RE-ARMS — forgets the reach — the instant the net drops back below
/// level. So after the loop, `first_reach_ms[key]` is the start of the run's
/// FINAL continuous hold (present only if the net is still held at the last
/// frame processed); a one-frame glitch above threshold that then collapses
/// leaves no record. This is what makes the assertion's documented "reach AND
/// hold" contract real — previously any single crossing latched a pass forever,
/// so firmware that pulsed the control net once and let it fall to 0 V still
/// passed.
fn boot_coverage_update(
    first_reach_ms: &mut HashMap<(String, u64), f64>,
    key: (String, u64),
    v: f64,
    level: f64,
    t_ms: f64,
) {
    if v >= level - 1e-6 {
        first_reach_ms.entry(key).or_insert(t_ms);
    } else {
        first_reach_ms.remove(&key);
    }
}

/// A scenario's measurement window: an id (empty for run-wide), the time it
/// begins, and the set of rails the spec's assertions reference for it.
struct ScenarioWindow {
    id: String,
    start_s: f64,
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

    // Build measurement windows from the rail_window assertions.
    let mut windows: Vec<ScenarioWindow> = Vec::new();
    for a in &spec.asserts {
        if a.kind != "rail_window" {
            continue;
        }
        let Some(net) = &a.net else { continue };
        let scope = a.scenario.clone().unwrap_or_default();
        // The window start is the scoped scenario's start_ms; a run-wide window
        // (no scope) starts at 0 and must not borrow the first scenario's start.
        let start_s = if scope.is_empty() {
            0.0
        } else {
            spec.scenarios
                .iter()
                .find(|s| s.id.as_deref() == Some(scope.as_str()))
                .map(|s| s.start_ms / 1000.0)
                .unwrap_or(0.0)
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
                nets: vec![net.clone()],
            }),
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

/// Resolve a SPI peripheral's `cs_net` to the MCU pin that drives it (05 §2.1),
/// so the co-sim frames transactions on the real chip-select edges. The net name
/// is looked up in the bound net map, then traced back to the GPIO driver pin via
/// the scheduler (the same net-to-driving-pin trace the 74HC595 chain wiring
/// uses). Returns `None` when `cs_net` is absent or does not resolve to a driven
/// MCU pin, in which case the bus falls back to the chunk-boundary heuristic and
/// the coverage reports `heuristic`.
fn resolve_cs_pin(
    p: &crate::spec::PeripheralSpec,
    net_node: &HashMap<String, NodeId>,
    sched: &hauksbee_engine::scheduler::Scheduler,
) -> Option<(char, u8)> {
    let net = p.cs_net.as_ref()?;
    let node = net_node.get(net).copied()?;
    sched.pin_driving_node(node)
}

/// Attach every peripheral in the spec to the scheduler. Returns the list of
/// (sink id, output path) for VCD sinks so they can be dumped after the run.
fn attach_peripherals(
    spec: &Spec,
    board: &ExtractedBoard,
    net_node: &HashMap<String, NodeId>,
    sched: &mut hauksbee_engine::scheduler::Scheduler,
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
                let cs_pin = resolve_cs_pin(p, net_node, sched);
                let bus = SpiBus::new(&p.id, Box::new(Spi25Eeprom::new(p.size.unwrap_or(256))));
                sched.attach_spi_bus(Arc::new(Mutex::new(bus)), cs_pin);
            }
            "spi_mcp3008" => {
                let cs_pin = resolve_cs_pin(p, net_node, sched);
                let bus = SpiBus::new(&p.id, Box::new(Mcp3008::new(p.vref.unwrap_or(5.0))));
                sched.attach_spi_bus(Arc::new(Mutex::new(bus)), cs_pin);
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
                let path = p.vcd_path.as_ref().map(|s| spec.base_dir.join(s));
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

    const BUS_SLAVE_KINDS: &[&str] = &[
        "i2c_eeprom", "i2c_lm75", "spi_eeprom", "spi_mcp3008",
    ];
    let backend_str = qemu_backends.join(", ");
    let mut warnings = Vec::new();

    for p in &spec.peripherals {
        if BUS_SLAVE_KINDS.contains(&p.kind.as_str()) {
            let bus_kind = if p.kind.starts_with("i2c") { "I2C" } else { "SPI" };
            warnings.push(format!(
                "WARNING: peripheral '{}' ({} {}) is a NO-OP on backend {} \
                 — I2C/SPI bus-slave co-sim is supported on AVR (simavr) and \
                 Renode backends only. The peripheral will not respond; firmware \
                 that depends on it may fail for that reason.",
                p.id, bus_kind, p.kind, backend_str
            ));
        }
    }

    for sa in &spec.sensors {
        warnings.push(format!(
            "WARNING: sensor '{}' (declarative bus sensor) is a NO-OP on backend {} \
             — I2C/SPI bus-slave co-sim is supported on AVR (simavr) and \
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
                        let _ = sink.write_to(path);
                    }
                }
            }
            _ => {}
        }
    }

    out
}

/// Compute per-component peak through-current from the latest node voltages.
/// Best-effort: resistors (V/R) and diodes (Shockley). Other kinds are left to
/// the fault monitor's overcurrent flags.
fn update_peak_currents(
    engine: &HauksbeeEngine,
    _net_node: &HashMap<String, NodeId>,
    peak: &mut HashMap<String, f64>,
) {
    let sched = engine.scheduler();
    let volts = &sched.node_volts;
    let v = |n: NodeId| volts.get(n.0 as usize).copied().unwrap_or(0.0);
    for dev in &sched.circuit.devices {
        let (name, i) = match dev {
            Device::Resistor {
                name, a, b, ohms, ..
            } => {
                let i = if *ohms > 0.0 {
                    ((v(*a) - v(*b)) / *ohms).abs()
                } else {
                    0.0
                };
                (name.clone(), i)
            }
            Device::Diode { name, a, k, model } => {
                let vd = v(*a) - v(*k);
                let vt = hauksbee_ir::thermal_voltage_c(sched.circuit.temp_c) * model.n;
                let i = if vt > 0.0 {
                    (model.is * (((vd / vt).clamp(-100.0, 200.0)).exp() - 1.0)).abs()
                } else {
                    0.0
                };
                (name.clone(), i)
            }
            _ => continue,
        };
        let e = peak.entry(name).or_insert(0.0);
        if i.is_finite() && i > *e {
            *e = i;
        }
    }
}

/// Remove the binder's auto-rail on `net`: drop its [`SupplyLeg`] and replace
/// the leg's internal Vsource with an open (1 TΩ) so the net floats except for
/// whatever the board itself feeds it.
fn suppress_rail(bound: &mut BoundBoard, net: &str) {
    let Some(node) = bound.node(net) else { return };
    bound.supplies.retain(|s| s.net != node);
    // The leg's source is named "Vsupply_<net>"; turn it (and any "Vrail_")
    // source on this node into an open resistor.
    let target = format!("Vsupply_{net}");
    for dev in bound.circuit.devices.iter_mut() {
        if let Device::Vsource { name, p, .. } = dev {
            if *p == node && (name == &target || name.starts_with("Vrail")) {
                let (nm, a, b) = (name.clone(), *p, NodeId::GROUND);
                *dev = Device::Resistor {
                    name: nm,
                    a,
                    b,
                    ohms: 1e12,
                    tc1: None,
                };
            }
        }
    }
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
    let v = s.volts.unwrap_or(5.0);
    let supply = match s.kind.as_str() {
        "ideal" => PowerSupply::Ideal { volts: v },
        "bench" => PowerSupply::Bench {
            volts: v,
            current_limit_a: s.current_limit_a.unwrap_or(1.0),
        },
        "wall" => PowerSupply::Wall {
            volts: v,
            r_out_ohms: s.r_out_ohms.unwrap_or(0.5),
            ripple_vpp: s.ripple_vpp.unwrap_or(0.1),
            ripple_hz: s.ripple_hz.unwrap_or(100.0),
        },
        "usb" => PowerSupply::Usb {
            spec: match s.usb.as_deref().unwrap_or("5v0.5a") {
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
            chemistry: match s.chemistry.as_deref().unwrap_or("liion") {
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

    // Replay a voltage series through the per-frame boot-coverage update and
    // return the recorded reach time (start of the final continuous hold), or
    // None if the net is not held at the end.
    fn boot_reach(series: &[(f64, f64)], level: f64) -> Option<f64> {
        let key = ("CTRL".to_string(), level.to_bits());
        let mut first_reach_ms: HashMap<(String, u64), f64> = HashMap::new();
        for &(t_ms, v) in series {
            boot_coverage_update(&mut first_reach_ms, key.clone(), v, level, t_ms);
        }
        first_reach_ms.get(&key).copied()
    }

    #[test]
    fn boot_coverage_requires_reach_and_hold_not_just_a_glitch() {
        // Driven up promptly and held to the end: passes, reach time is the
        // first crossing.
        assert_eq!(
            boot_reach(&[(0.0, 0.0), (5.0, 5.0), (10.0, 5.0), (50.0, 5.0)], 3.0),
            Some(5.0)
        );
        // A one-frame glitch above threshold that then collapses to 0 V for the
        // rest of the run must NOT latch a pass (the old bug).
        assert_eq!(
            boot_reach(&[(0.0, 0.0), (5.0, 5.0), (10.0, 0.0), (50.0, 0.0)], 3.0),
            None
        );
        // Dropped then recovered and held to the end: the recorded reach is the
        // start of the FINAL hold (so a late recovery is judged against the
        // deadline, not the early glitch).
        assert_eq!(
            boot_reach(&[(5.0, 5.0), (10.0, 0.0), (30.0, 5.0), (50.0, 5.0)], 3.0),
            Some(30.0)
        );
        // Never reaches: None.
        assert_eq!(boot_reach(&[(0.0, 0.0), (50.0, 1.0)], 3.0), None);
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

    // ── qemu_bus_slave_warnings unit tests ───────────────────────────────────

    /// Helper: write a minimal board + spec to temp with a unique name, load
    /// and return the Spec. Tests run in parallel so each gets its own file.
    fn load_spec_str(test_name: &str, spec_toml: &str) -> Spec {
        let dir = std::env::temp_dir().join("hauksbee_ci_warn_tests");
        std::fs::create_dir_all(&dir).unwrap();

        // Minimal board: a single pull-down resistor. No MCU footprint here —
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
        let full_spec = format!(
            "board = \"{}\"\n{}",
            board_path.display(),
            spec_toml
        );
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
        let body = format!(r#"duration_ms = 10

[[peripheral]]
id = "BME280"
type = "i2c_lm75"
address = 0x76
{MINIMAL_ASSERT}"#);
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
        let body = format!(r#"duration_ms = 10

[[peripheral]]
id = "ADC1"
type = "spi_mcp3008"
vref = 3.3
{MINIMAL_ASSERT}"#);
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
        let body = format!(r#"duration_ms = 10

[[sensor]]
id = "U2_bme280"
spec = """
[sensor]
name = "BME280_stub"
bus  = "i2c"
i2c_address = 0x76
"""
{MINIMAL_ASSERT}"#);
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
        let body = format!(r#"duration_ms = 10

[[peripheral]]
id = "BTN1"
type = "pushbutton"
net  = "+3V3"
{MINIMAL_ASSERT}"#);
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
        let body = format!(r#"duration_ms = 10

[[peripheral]]
id = "EEPROM1"
type = "i2c_eeprom"
address = 0x50

[[peripheral]]
id = "ADC1"
type = "spi_mcp3008"
vref = 3.3
{MINIMAL_ASSERT}"#);
        let spec = load_spec_str("multi_slave_qemu", &body);
        let backends = vec!["qemu:esp32s3".to_string()];
        let warnings = qemu_bus_slave_warnings(&spec, &backends);
        assert_eq!(warnings.len(), 2, "one warning per bus slave, got: {warnings:?}");
        assert!(warnings.iter().any(|w| w.contains("EEPROM1")));
        assert!(warnings.iter().any(|w| w.contains("ADC1")));
    }

    /// Renode backend (not QEMU) with a bus slave -> NO warning.
    /// `qemu_bus_slave_warnings` takes the backends list as a parameter;
    /// an empty list means no QEMU backends (as would be the case for Renode
    /// or AVR), so no warnings should fire.
    #[test]
    fn renode_backend_no_warning() {
        let body = format!(r#"duration_ms = 10

[[peripheral]]
id = "TEMP_SENSOR"
type = "i2c_lm75"
address = 0x48
{MINIMAL_ASSERT}"#);
        let spec = load_spec_str("renode_no_warn", &body);
        // Empty list = no QEMU backends (Renode/AVR boards).
        let backends: Vec<String> = vec![];
        let warnings = qemu_bus_slave_warnings(&spec, &backends);
        assert!(
            warnings.is_empty(),
            "non-qemu backends must not warn: {warnings:?}"
        );
    }
}
