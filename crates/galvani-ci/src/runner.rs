//! The headless runner: turn a [`Spec`] into a bound board, apply its supplies,
//! net drives, rail suppressions and overrides, run the co-sim for the
//! requested duration across one or more fuzz seeds, and collect everything the
//! assertions need (per-net min/max/toggles after a time threshold, UART,
//! faults, per-component peak current).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use galvani_engine::power_supply::{Chemistry, PowerSupply, SupplyLeg, UsbSpec};
use galvani_engine::{bind_board, BoundBoard, GalvaniEngine};
use galvani_extract::ExtractedBoard;
use galvani_ir::{Device, NodeId, SourceKind};
use galvani_models::ModelLibrary;
use galvani_server::engine::Engine;

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
}

/// Run the spec and return one [`RunOutcome`] per seed (>=1).
pub fn run_spec(spec: &Spec) -> Result<Vec<RunOutcome>, SpecError> {
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
    let known_refs: Vec<String> = base.components.iter().map(|c| c.reference.clone()).collect();
    check_component_refs(spec, &known_refs)?;

    // Distinct after_ms thresholds (plus 0) that windows are bucketed by.
    let mut thresholds: Vec<f64> = spec
        .asserts
        .iter()
        .filter_map(|a| a.after_ms)
        .collect();
    thresholds.push(0.0);
    thresholds.sort_by(|a, b| a.partial_cmp(b).unwrap());
    thresholds.dedup();

    let seeds = spec.fuzz.as_ref().map(|f| f.seeds).unwrap_or(1).max(1);
    let mut outcomes = Vec::with_capacity(seeds as usize);
    for seed in 0..seeds {
        let outcome = run_one(spec, &base, &thresholds, seed)?;
        outcomes.push(outcome);
    }
    Ok(outcomes)
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
fn load_board(board_path: &Path) -> Result<ExtractedBoard, SpecError> {
    let is_sch = board_path
        .extension()
        .and_then(|e| e.to_str())
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
        let text = std::fs::read_to_string(board_path)
            .map_err(|e| SpecError::Io(format!("reading board {}: {e}", board_path.display())))?;
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

/// Apply overrides for this run to a fresh copy of the extracted board.
fn apply_overrides(spec: &Spec, base: &ExtractedBoard) -> Result<ExtractedBoard, SpecError> {
    let mut board = base.clone();
    for ov in &spec.overrides {
        let comp = board
            .components
            .iter_mut()
            .find(|c| c.reference == ov.reference)
            .ok_or_else(|| {
                let refs: Vec<String> =
                    base.components.iter().map(|c| c.reference.clone()).collect();
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

fn run_one(
    spec: &Spec,
    base: &ExtractedBoard,
    thresholds: &[f64],
    seed: u32,
) -> Result<RunOutcome, SpecError> {
    let board = apply_overrides(spec, base)?;
    let lib = ModelLibrary::builtin();
    let mut bound = bind_board(&board, &lib);

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

    let firmware = spec.firmware_path();
    let mut engine = GalvaniEngine::from_bound(bound, firmware.as_deref(), "/ci")
        .map_err(|e| SpecError::Invalid(format!("building engine: {e}")))?;

    // Attach this spec's peripherals (controls, bus slaves, sinks) and their
    // timeline events to the engine's scheduler.
    let vcd_targets = attach_peripherals(spec, &board, &net_node, engine.scheduler_mut())?;

    // Attach this spec's transient scenarios: dynamic load profiles stamped as
    // current sinks on the parts' supply nets.
    let scenario_windows = attach_scenarios(spec, &board, &net_node, engine.scheduler_mut())?;

    // Run the co-sim, sampling each frame.
    let frame_dt = (spec.frame_ms / 1000.0).max(1e-6);
    let total_s = spec.duration_ms / 1000.0;
    let mut t = 0.0;

    let mut windows: HashMap<(String, u64), NetWindow> = HashMap::new();
    let mut uart: HashMap<String, String> = HashMap::new();
    let mut faults: Vec<RunFault> = Vec::new();
    let mut peak_current: HashMap<String, f64> = HashMap::new();
    // Per-scenario rail-window timeseries and protection-trip tracking.
    let mut rail_windows: HashMap<(String, String), crate::scenarios::RailWindow> = HashMap::new();
    let mut protection_tripped: HashMap<String, bool> = HashMap::new();

    while t < total_s - 1e-12 {
        let frame = engine.step(frame_dt);
        let t_ms = frame.t * 1000.0;

        // UART accumulation.
        for (mcu, bytes) in &frame.uart {
            uart.entry(mcu.clone())
                .or_default()
                .push_str(&String::from_utf8_lossy(bytes));
        }
        // Faults.
        for f in &frame.faults {
            faults.push(RunFault {
                component: f.component.clone(),
                kind: f.kind.clone(),
                value: f.value,
                limit: f.limit,
                t_ms: f.t * 1000.0,
            });
        }
        // Per-net windows for every threshold this frame has passed.
        for &thr in thresholds {
            if t_ms + 1e-9 >= thr {
                for (name, &v) in &frame.net_voltages {
                    let key = (name.clone(), thr.to_bits());
                    windows
                        .entry(key)
                        .or_insert_with(NetWindow::new)
                        .observe(v);
                }
            }
        }
        // Peak current for monitored components.
        update_peak_currents(&engine, &net_node, &mut peak_current);

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
        // Protection-trip tracking: latch true if any supply's protection has
        // tripped this frame (it stays recorded even if it later re-arms).
        for leg in &engine.scheduler().supplies {
            if leg.supply.protection_tripped() {
                protection_tripped.insert(leg.net_name.clone(), true);
            } else {
                protection_tripped.entry(leg.net_name.clone()).or_insert(false);
            }
        }

        t += frame_dt;
    }

    // Toggle counts from the scheduler's running stats.
    let toggles: HashMap<String, u64> = engine
        .scheduler()
        .stats
        .iter()
        .map(|(n, st)| (n.clone(), st.toggles))
        .collect();

    // Snapshot peripheral state for assertions, and dump any VCD sinks.
    let peripherals = snapshot_peripherals(spec, &engine, &vcd_targets);

    Ok(RunOutcome {
        seed,
        windows,
        uart,
        faults,
        toggles,
        peak_current,
        peripherals,
        rail_windows,
        protection_tripped,
        sim_ms: engine.scheduler().sim_time * 1000.0,
    })
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
    sched: &mut galvani_engine::scheduler::Scheduler,
) -> Result<Vec<ScenarioWindow>, SpecError> {
    use galvani_engine::DynamicLoad;

    // Resolve the profile set: built-ins plus any inline spec-local profiles.
    let resolve_profile = |name: &str| -> Option<galvani_models::LoadProfile> {
        if let Some(ip) = spec.profiles.iter().find(|p| p.id == name) {
            return Some(ip.to_profile());
        }
        galvani_models::LoadProfile::by_id(name)
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
        // The window start is the scenario's start_ms (0 for run-wide).
        let start_s = spec
            .scenarios
            .iter()
            .find(|s| s.id.as_deref() == Some(scope.as_str()) || (scope.is_empty()))
            .map(|s| s.start_ms / 1000.0)
            .unwrap_or(0.0);
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
    use galvani_engine::{apply_parasitics, EsrEsl};
    use galvani_ir::Device;

    let Some(dec) = &spec.decoupling else {
        return Ok(());
    };

    // Footprint/value lookup by capacitor reference, for default inference.
    let cap_meta: HashMap<String, (String, f64)> = board
        .components
        .iter()
        .map(|c| {
            // Parse the value to farads best-effort (0 if unparseable).
            let f = galvani_models::value::parse_value(&c.value)
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
                        if let Some(net) =
                            board.nets.iter().find(|n| n.id == net_id).map(|n| n.name.clone())
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

/// Attach every peripheral in the spec to the scheduler. Returns the list of
/// (sink id, output path) for VCD sinks so they can be dumped after the run.
fn attach_peripherals(
    spec: &Spec,
    board: &ExtractedBoard,
    net_node: &HashMap<String, NodeId>,
    sched: &mut galvani_engine::scheduler::Scheduler,
) -> Result<Vec<(String, std::path::PathBuf)>, SpecError> {
    use std::sync::{Arc, Mutex};

    use galvani_engine::peripherals::controls::{pwl as pwl_source, StimulusKind};
    use galvani_engine::{
        Encoder, Potentiometer, Pushbutton, Stimulus, ToggleSwitch, VcdSink,
    };
    use galvani_engine::{Eeprom24c, I2cBus, Lm75, Mcp3008, Spi25Eeprom, SpiBus};
    use galvani_ir::{NodeId as N, SourceKind};

    let mut vcd_targets = Vec::new();

    for p in &spec.peripherals {
        let err = |m: String| SpecError::Invalid(format!("peripheral '{}': {m}", p.id));
        match p.kind.as_str() {
            "pushbutton" => {
                let net = resolve_net(p, board, net_node, None)
                    .ok_or_else(|| err("net not found".into()))?;
                let to = p
                    .to
                    .as_ref()
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
                let to = p
                    .to
                    .as_ref()
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
                let a = p
                    .a
                    .as_ref()
                    .and_then(|n| net_node.get(n).copied())
                    .ok_or_else(|| err("pot terminal `a` net not found".into()))?;
                let b = p
                    .b
                    .as_ref()
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
                let bus = SpiBus::new(&p.id, Box::new(Spi25Eeprom::new(p.size.unwrap_or(256))));
                sched.attach_spi_bus(Arc::new(Mutex::new(bus)));
            }
            "spi_mcp3008" => {
                let bus = SpiBus::new(&p.id, Box::new(Mcp3008::new(p.vref.unwrap_or(5.0))));
                sched.attach_spi_bus(Arc::new(Mutex::new(bus)));
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
                .map(|e| galvani_engine::TimelineEvent {
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

/// Snapshot every peripheral's end-of-run state and dump VCD files.
fn snapshot_peripherals(
    spec: &Spec,
    engine: &GalvaniEngine,
    vcd_targets: &[(String, std::path::PathBuf)],
) -> HashMap<String, PeripheralSnapshot> {
    use galvani_engine::{Eeprom24c, Spi25Eeprom, VcdSink};

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
                    if galvani_engine::Peripheral::id(&*b) == p.id {
                        if let Some(ee) = b.slave::<Eeprom24c>(p.address.unwrap_or(0x50)) {
                            out.entry(p.id.clone()).or_default().bytes = ee.contents().to_vec();
                        }
                    }
                }
            }
            "spi_eeprom" => {
                for bus in sched.spi_buses() {
                    let b = bus.lock().unwrap_or_else(|e| e.into_inner());
                    if galvani_engine::Peripheral::id(&*b) == p.id {
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
    engine: &GalvaniEngine,
    _net_node: &HashMap<String, NodeId>,
    peak: &mut HashMap<String, f64>,
) {
    let sched = engine.scheduler();
    let volts = &sched.node_volts;
    let v = |n: NodeId| volts.get(n.0 as usize).copied().unwrap_or(0.0);
    for dev in &sched.circuit.devices {
        let (name, i) = match dev {
            Device::Resistor { name, a, b, ohms, .. } => {
                let i = if *ohms > 0.0 {
                    ((v(*a) - v(*b)) / *ohms).abs()
                } else {
                    0.0
                };
                (name.clone(), i)
            }
            Device::Diode { name, a, k, model } => {
                let vd = v(*a) - v(*k);
                let vt = galvani_ir::thermal_voltage_c(sched.circuit.temp_c) * model.n;
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
                    let mut p =
                        galvani_engine::power_supply::BatteryProtection::new(
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
        let many = r#"(property "Sheetfile" "a.kicad_sch") ... (property "Sheetfile" "b.kicad_sch")"#;
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
}
