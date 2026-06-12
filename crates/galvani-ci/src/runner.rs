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

    // Run the co-sim, sampling each frame.
    let frame_dt = (spec.frame_ms / 1000.0).max(1e-6);
    let total_s = spec.duration_ms / 1000.0;
    let mut t = 0.0;

    let mut windows: HashMap<(String, u64), NetWindow> = HashMap::new();
    let mut uart: HashMap<String, String> = HashMap::new();
    let mut faults: Vec<RunFault> = Vec::new();
    let mut peak_current: HashMap<String, f64> = HashMap::new();

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

        t += frame_dt;
    }

    // Toggle counts from the scheduler's running stats.
    let toggles: HashMap<String, u64> = engine
        .scheduler()
        .stats
        .iter()
        .map(|(n, st)| (n.clone(), st.toggles))
        .collect();

    Ok(RunOutcome {
        seed,
        windows,
        uart,
        faults,
        toggles,
        peak_current,
        sim_ms: engine.scheduler().sim_time * 1000.0,
    })
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
