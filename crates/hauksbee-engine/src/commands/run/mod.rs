//! `hauksbee run <board>`: the run orchestrator. Loads/binds the board, then
//! dispatches to the right report (reports/), the interactive TUI, a headless
//! co-sim, or the websocket server. Argument parsing lives in the binary; this
//! takes a plain [`RunConfig`].

use crate::result::Refusal;

mod cosim;
mod cosim_gates;
mod manifest;
mod prepare;
mod serve_flow;
mod simulation;
mod static_surfaces;

/// Plain (non-clap) mirror of the binary's `RunArgs`, so the `run` orchestrator
/// lives in the library while argument parsing stays in `main.rs`. Field types
/// match `RunArgs` exactly; the binary builds one and hands it over.
pub struct RunConfig {
    pub board: std::path::PathBuf,
    /// Embedded example name when the board path was materialized from the
    /// binary. Its bytes are pinned by the tool revision, not a temp path.
    pub example: Option<String>,
    pub bom: Option<std::path::PathBuf>,
    pub bom_columns: Vec<String>,
    pub placement: Option<std::path::PathBuf>,
    pub firmware: Option<std::path::PathBuf>,
    pub seconds: f64,
    pub headless: bool,
    pub report: bool,
    pub drc: bool,
    pub ampacity: bool,
    pub lint: bool,
    pub si: bool,
    pub resources: bool,
    pub usb_c: bool,
    pub thermal: bool,
    pub ambient: f64,
    pub plain: bool,
    /// `--plain` DRC: print every clearance finding in full instead of
    /// condensing repeated near-identical ones past the first few.
    pub verbose: bool,
    pub json: bool,
    pub strict: bool,
    /// `--strict-thermal`: accepted for compatibility; partial-coverage
    /// escalation is the default, so this is a quiet no-op. Recorded in the
    /// invocation echo so a rerun reproduces the exact command.
    pub strict_thermal: bool,
    /// `--no-strict-thermal`: opt out of the default partial-coverage thermal
    /// escalation (exit stays 0; the INCONCLUSIVE caveat still prints).
    pub no_strict_thermal: bool,
    pub strict_boot: bool,
    pub list_nets: bool,
    pub check: bool,
    pub oracle: bool,
    pub apply_shorts: bool,
    pub serve: bool,
    /// `--open` under `--serve`: open the browser once the server is bound
    /// (same policy as the `serve` subcommand).
    pub open: bool,
    /// `--no-open` under `--serve`: never open a browser, even when launched
    /// by the desktop app.
    pub no_open: bool,
    pub tui: bool,
    pub port: u16,
    pub models_dir: Option<std::path::PathBuf>,
    pub ac: Option<String>,
    pub ac_node: Vec<String>,
    pub ac_csv: Option<std::path::PathBuf>,
    pub ac_loop: Option<String>,
    pub probe: Vec<String>,
    pub probe_csv: Option<std::path::PathBuf>,
    /// Solver chunk width in microseconds. Narrower chunks resolve firmware
    /// pulses the default width steps straight over, at proportional cost.
    pub chunk_us: Option<f64>,
    /// References of DNP parts to simulate as fitted, whatever the policy says.
    pub fit: Vec<String>,
    /// References of DNP parts to leave open, whatever the policy says.
    pub no_fit: Vec<String>,
    /// What to do with the DNP parts neither list names.
    pub dnp_policy: hauksbee_extract::dnp::DnpPolicy,
    /// Open a host-facing serial endpoint for the co-sim, so the user's own
    /// software can talk to the emulated MCU's UART (see
    /// `commands::hostserial`).
    pub serial_attach: bool,
    /// `pty` (default: unmodified serial software works unmodified) or `tcp`.
    pub serial_transport: hauksbee_mcu::hostserial::HostSerialTransport,
    /// Hold the co-sim at t=0 until a host tool attaches, at most this long.
    pub serial_wait: Option<f64>,
    /// Let the co-sim run as fast as it can while no host tool is attached.
    /// While a peer is attached, its bytes are delivered on a fixed
    /// compressed schedule (wall gaps scaled into sim time) rather than
    /// free-running; see `SerialSessionConfig::pace`.
    pub serial_no_pace: bool,
    /// Which MCU reference to bridge, when a board carries more than one.
    pub serial_mcu: Option<String>,
    /// `.asbuilt.toml` overlay: the declarative physical delta (cuts, jumpers,
    /// fitted values) between the design files and the real reworked board,
    /// applied to the bound board before the engine is built.
    pub asbuilt: Option<std::path::PathBuf>,
    /// Write this invocation's selected checks as JUnit XML (CI artifact).
    pub junit: Option<std::path::PathBuf>,
    /// Write this invocation's selected checks as SARIF 2.1.0 (CI artifact).
    pub sarif: Option<std::path::PathBuf>,
    /// Canonical immutable reproduction manifest requested by the CLI.
    pub emit_manifest: Option<std::path::PathBuf>,
    /// Normalized argv (tool name, then exact arguments) with
    /// `--emit-manifest` removed so replay cannot clobber its evidence.
    pub manifest_command: Vec<String>,
}

pub(crate) fn input_kind_name(kind: crate::board_input::InputKind) -> &'static str {
    use crate::board_input::InputKind;
    match kind {
        InputKind::Text => "layout_or_netlist",
        InputKind::Schematic => "kicad_schematic",
        InputKind::Altium => "altium_pcbdoc",
        InputKind::Gerber => "gerber_archive",
        InputKind::Ipc356Archive => "fab_ipc356",
        InputKind::Odb => "odbpp",
        InputKind::Ipc2581 => "ipc2581",
        InputKind::BoardCode => "board_as_code",
    }
}

/// The bytes served for a preloaded browser session must be the same primary
/// input bytes authenticated by the report. Board-as-Code uses compiled KiCad
/// text internally, but a saved `.board` session can only resume from its DSL.
pub(crate) fn preloaded_board_file(
    board_url: &str,
    raw: &[u8],
    input_kind: crate::board_input::InputKind,
    layout_text: &str,
) -> (String, String) {
    crate::commands::common::resumable_board_file(board_url, raw, input_kind)
        .unwrap_or_else(|| (board_url.to_string(), layout_text.to_string()))
}

pub(crate) fn valid_digest(digest: &str) -> Option<String> {
    (digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| digest.to_string())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SelectedSurface {
    Inventory,
    Check,
    Bind,
    Drc,
    Ampacity,
    Lint,
    Resources,
    UsbC,
    Si,
    Ac,
    Thermal,
    Serial,
    Headless,
    BareJson,
    Default,
}

/// Select once, in the same order the orchestrator executes. Every artifact,
/// evidence gate and dispatch branch reads this value rather than independently
/// reconstructing precedence from the raw flags.
fn selected_surface(cfg: &RunConfig) -> SelectedSurface {
    if cfg.list_nets {
        SelectedSurface::Inventory
    } else if cfg.check {
        SelectedSurface::Check
    } else if cfg.report {
        SelectedSurface::Bind
    } else if cfg.drc {
        SelectedSurface::Drc
    } else if cfg.ampacity {
        SelectedSurface::Ampacity
    } else if cfg.lint {
        SelectedSurface::Lint
    } else if cfg.resources {
        SelectedSurface::Resources
    } else if cfg.usb_c {
        SelectedSurface::UsbC
    } else if cfg.si {
        SelectedSurface::Si
    } else if cfg.ac.is_some() {
        SelectedSurface::Ac
    } else if cfg.thermal {
        SelectedSurface::Thermal
    } else if cfg.serial_attach {
        SelectedSurface::Serial
    } else if cfg.headless {
        SelectedSurface::Headless
    } else if cfg.json {
        SelectedSurface::BareJson
    } else {
        SelectedSurface::Default
    }
}

fn ci_check_selected(surface: SelectedSurface, finding: &crate::result::JsonFinding) -> bool {
    match surface {
        SelectedSurface::Check | SelectedSurface::BareJson => {
            matches!(finding.check.as_str(), "drc" | "lint" | "si" | "usb_c")
        }
        SelectedSurface::Drc => finding.check == "drc",
        SelectedSurface::Lint => finding.check == "lint",
        SelectedSurface::Resources => {
            finding.check == "lint" && finding.kind == "mcu_resource_conflict"
        }
        SelectedSurface::UsbC => finding.check == "usb_c",
        SelectedSurface::Si => finding.check == "si",
        _ => false,
    }
}

fn ci_surface_is_model_dependent(surface: SelectedSurface) -> bool {
    matches!(
        surface,
        SelectedSurface::Check
            | SelectedSurface::Lint
            | SelectedSurface::Resources
            | SelectedSurface::UsbC
            | SelectedSurface::Si
            | SelectedSurface::Headless
            | SelectedSurface::BareJson
    )
}

fn ci_selected_suites(surface: SelectedSurface) -> Vec<String> {
    let checks: &[&str] = match surface {
        SelectedSurface::Check | SelectedSurface::BareJson => &["drc", "lint", "si", "usb_c"],
        SelectedSurface::Bind => &["bind"],
        SelectedSurface::Drc => &["drc"],
        SelectedSurface::Ampacity => &["ampacity"],
        SelectedSurface::Lint | SelectedSurface::Resources => &["lint"],
        SelectedSurface::UsbC => &["usb_c"],
        SelectedSurface::Si => &["si"],
        SelectedSurface::Ac => &["ac"],
        SelectedSurface::Thermal => &["thermal"],
        SelectedSurface::Serial | SelectedSurface::Headless => &["cosim"],
        SelectedSurface::Inventory => &["inventory"],
        SelectedSurface::Default => &["run"],
    };
    checks.iter().map(|check| (*check).to_string()).collect()
}

fn begin_ci_artifact_run(cfg: &RunConfig, surface: SelectedSurface) -> anyhow::Result<()> {
    let protected = [
        Some(cfg.board.as_path()),
        cfg.bom.as_deref(),
        cfg.placement.as_deref(),
        cfg.firmware.as_deref(),
        cfg.asbuilt.as_deref(),
        cfg.emit_manifest.as_deref(),
        cfg.ac_csv.as_deref(),
        cfg.probe_csv.as_deref(),
    ];
    let aliased_input = |path: Option<&std::path::Path>| {
        path.and_then(|output| {
            protected
                .iter()
                .flatten()
                .find(|input| crate::reports::ci_artifacts::paths_alias(input, output))
                .copied()
        })
    };
    let junit_alias = aliased_input(cfg.junit.as_deref());
    let sarif_alias = aliased_input(cfg.sarif.as_deref());
    let same_output = cfg
        .junit
        .as_deref()
        .zip(cfg.sarif.as_deref())
        .is_some_and(|(junit, sarif)| crate::reports::ci_artifacts::paths_alias(junit, sarif));
    let mut errors = Vec::new();
    for (flag, input) in [("--junit", junit_alias), ("--sarif", sarif_alias)] {
        if let Some(input) = input {
            errors.push(format!(
                "{flag} output must not overwrite another run input/output '{}'; choose a different path",
                input.display()
            ));
        }
    }
    if same_output {
        errors.push("--junit and --sarif need different output paths".into());
    }
    if errors.is_empty() {
        return crate::reports::ci_artifacts::begin_run(
            &cfg.board,
            cfg.junit.as_deref(),
            cfg.sarif.as_deref(),
            ci_selected_suites(surface),
        );
    }

    // Even when one requested path is unsafe, invalidate every other safe
    // output before returning the validation error. Otherwise CI can archive a
    // prior green file merely because its sibling flag aliased an input.
    let safe_junit = (junit_alias.is_none() && !same_output)
        .then_some(cfg.junit.as_deref())
        .flatten();
    let safe_sarif = (sarif_alias.is_none() && !same_output)
        .then_some(cfg.sarif.as_deref())
        .flatten();
    let error = anyhow::anyhow!(errors.join("; "));
    if safe_junit.is_some() || safe_sarif.is_some() {
        crate::reports::ci_artifacts::begin_run(
            &cfg.board,
            safe_junit,
            safe_sarif,
            ci_selected_suites(surface),
        )?;
        crate::reports::ci_artifacts::finish_error(&error, 1);
    }
    Err(error)
}

fn run_error_exit_code(error: &anyhow::Error) -> i32 {
    if let Some(error) = error.downcast_ref::<hauksbee_extract::bom::BomError>() {
        return error.exit_code();
    }
    if let Some(error) = error.downcast_ref::<hauksbee_extract::placement::PlacementError>() {
        return error.exit_code();
    }
    if let Some(error) = error.downcast_ref::<crate::binder::IdentityRefusal>() {
        return error.exit_code();
    }
    1
}

pub fn run(cfg: RunConfig, quiet: bool) -> anyhow::Result<()> {
    run_with_schematic(cfg, quiet, None)
}

/// Run with an optional explicit Eagle companion schematic. Kept out of
/// [`RunConfig`] so adding the CLI input does not break downstream struct
/// literals of the established public configuration type.
pub fn run_with_schematic(
    mut cfg: RunConfig,
    quiet: bool,
    schematic: Option<std::path::PathBuf>,
) -> anyhow::Result<()> {
    // Bare --plain means the combined check surface. Normalize that convenience
    // before selecting or initializing artifacts, so every consumer sees the
    // same surface the execution branch will run.
    let any_report_flag = cfg.report
        || cfg.drc
        || cfg.ampacity
        || cfg.lint
        || cfg.si
        || cfg.resources
        || cfg.usb_c
        || cfg.thermal
        || cfg.check;
    if cfg.plain
        && !any_report_flag
        && !cfg.headless
        && !cfg.serve
        && !cfg.tui
        && !cfg.list_nets
        && !cfg.serial_attach
        && cfg.ac.is_none()
    {
        cfg.check = true;
    }
    let surface = selected_surface(&cfg);
    begin_ci_artifact_run(&cfg, surface)?;
    let result = (|| {
        if let Some(name) = cfg.example.as_deref() {
            cfg.board = crate::commands::examples::board(name)?;
            crate::reports::ci_artifacts::set_current_board_path(&cfg.board);
        }
        run_inner(cfg, quiet, surface, schematic)
    })();
    match &result {
        Ok(()) => crate::reports::ci_artifacts::finish_success()?,
        Err(error) => {
            crate::reports::ci_artifacts::finish_error(error, run_error_exit_code(error));
        }
    }
    result
}

fn run_inner(
    mut cfg: RunConfig,
    quiet: bool,
    surface: SelectedSurface,
    schematic: Option<std::path::PathBuf>,
) -> anyhow::Result<()> {
    let any_report_flag = cfg.report
        || cfg.drc
        || cfg.ampacity
        || cfg.lint
        || cfg.si
        || cfg.resources
        || cfg.usb_c
        || cfg.thermal
        || cfg.check;
    let (run_inputs, mut artifacts) = prepare::prepare_run_inputs(
        &mut cfg,
        quiet,
        surface,
        schematic.as_deref(),
        any_report_flag,
    )?;
    if static_surfaces::emit_selected(&cfg, surface, &run_inputs, &mut artifacts)? {
        return Ok(());
    }
    if simulation::launch_tui_if_selected(&cfg, &run_inputs, schematic.as_deref())? {
        return Ok(());
    }
    let prebound = artifacts.prebound.take();
    let mut sim = simulation::build_live_simulation(
        &cfg,
        &run_inputs,
        prebound,
        artifacts.schematic_ties.as_ref(),
    )?;
    if simulation::run_selected_simulation_surface(
        &cfg,
        surface,
        &run_inputs,
        &artifacts,
        &mut sim,
    )? {
        return Ok(());
    }
    let stdout_is_tty = std::io::IsTerminal::is_terminal(&std::io::stdout());
    serve_flow::emit_or_serve(&cfg, &run_inputs, &artifacts, sim, stdout_is_tty)
}

/// Warn (advisory, stderr) when the board sits among sibling `.kicad_pcb` files,
/// a multi-board product (e.g. a main board with a separate ESC/daughter board in
/// a sibling folder). A clean verdict on ONE file reads as "the product is fine"
/// when the user may have meant the whole thing. Best-effort; never fails the run.
/// Whether informational `note:` lines should be shown for this invocation.
/// They go to stderr and only for an interactive human: suppressed under
/// `--quiet`, under `--json` (machine output), and when stdout is piped or
/// redirected (not a TTY), so report and pipeline output stays clean while the
/// note stays discoverable for interactive users. Pure so it is unit-testable
/// without a real terminal.
fn notes_visible(quiet: bool, json: bool, stdout_is_tty: bool) -> bool {
    !quiet && !json && stdout_is_tty
}

/// The `--list-nets --json` array, serialized through serde_json rather than
/// Rust `Debug`. The two agree on the common escapes but diverge on control
/// characters, Debug emits variable-length brace-hex, whereas JSON mandates a
/// fixed four-hex-digit escape, so a net name carrying a control char from a
/// malformed/adversarial netlist would make the hand-`Debug`-assembled array a
/// document no JSON parser accepts. This is pure so it is unit-testable.
pub(crate) fn list_nets_json(nets: &[String]) -> String {
    serde_json::to_string(nets).unwrap_or_else(|_| "[]".into())
}

/// The single gate every chatty informational note routes through, so `--quiet`
/// (and JSON / non-TTY suppression) is honoured uniformly and future notes
/// inherit the behaviour by going through here instead of a bare `eprintln!`.
#[derive(Clone, Copy)]
pub(crate) struct Notes {
    enabled: bool,
}

impl Notes {
    fn new(quiet: bool, json: bool) -> Self {
        Notes {
            enabled: notes_visible(
                quiet,
                json,
                std::io::IsTerminal::is_terminal(&std::io::stdout()),
            ),
        }
    }

    /// Emit a single-line informational note (prefixed `note:`) on stderr, unless
    /// notes are suppressed for this invocation.
    fn say(&self, msg: impl std::fmt::Display) {
        if self.enabled {
            eprintln!("note: {msg}");
        }
    }
}

pub(crate) fn warn_sibling_boards(board: &std::path::Path, notes: Notes) {
    if !notes.enabled {
        return;
    }
    let Ok(abs) = std::fs::canonicalize(board) else {
        return;
    };
    let Some(dir) = abs.parent() else {
        return;
    };
    let found = sibling_board_names(&abs, dir);
    if found.is_empty() {
        return;
    }
    notes.say(format!(
        "{} other board file(s) in the same folder; this run only checks '{}':",
        found.len(),
        board.display()
    ));
    for name in found.iter().take(5) {
        eprintln!("  - {name}");
    }
    if found.len() > 5 {
        eprintln!("  ... and {} more", found.len() - 5);
    }
    eprintln!("  If they are part of the same product, check each one separately.");
}

/// The other `.kicad_pcb` FILE NAMES directly inside `dir` (the checked
/// board's own directory), excluding the board itself.
///
/// PRIVACY scope (U12): the user asked about ONE file, so the note may look
/// only at that file's OWN directory. No parent/child/sibling-directory walks
/// (an earlier version surfaced boards from unrelated neighbouring projects),
/// and only file NAMES are returned, never absolute paths of files the user
/// did not name. Pure-ish (reads one directory) so it is unit-testable.
fn sibling_board_names(board_abs: &std::path::Path, dir: &std::path::Path) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) == Some("kicad_pcb") && p != board_abs {
                if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                    found.push(name.to_string());
                }
            }
        }
    }
    found.sort();
    found.dedup();
    found
}

pub(crate) fn timing_refusal(reasons: &[String]) -> Refusal {
    Refusal::new(
        "trustworthy timing evidence from this co-simulation",
        format!("runtime timing evidence was refused: {}", reasons.join("; ")),
        vec!["static board and copper findings remain available"],
        "reduce the edge rate/transition count or use a backend that can replay the waveform exactly, then rerun",
    )
}

#[cfg(test)]
mod sibling_scope_tests {
    use super::sibling_board_names;

    /// U12: the nearby-boards note must NOT surface boards from parent,
    /// child, or sibling directories, and must return names, not paths.
    #[test]
    fn sibling_scan_stays_inside_the_boards_own_directory() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("proj");
        let sibling_dir = root.path().join("other-proj");
        let child_dir = dir.join("esc");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(&sibling_dir).unwrap();
        std::fs::create_dir_all(&child_dir).unwrap();
        let board = dir.join("main.kicad_pcb");
        std::fs::write(&board, "(kicad_pcb)").unwrap();
        std::fs::write(dir.join("rev_b.kicad_pcb"), "(kicad_pcb)").unwrap();
        std::fs::write(sibling_dir.join("private.kicad_pcb"), "(kicad_pcb)").unwrap();
        std::fs::write(child_dir.join("daughter.kicad_pcb"), "(kicad_pcb)").unwrap();
        std::fs::write(root.path().join("parent.kicad_pcb"), "(kicad_pcb)").unwrap();

        let names = sibling_board_names(&board, &dir);
        assert_eq!(names, vec!["rev_b.kicad_pcb".to_string()]);
        assert!(
            names.iter().all(|n| !n.contains('/')),
            "names only, no paths: {names:?}"
        );
    }
}

#[cfg(test)]
mod preloaded_board_file_tests {
    use super::preloaded_board_file;
    use crate::board_input::InputKind;

    #[test]
    fn board_code_route_serves_the_authenticated_dsl_not_compiled_layout() {
        let source = "board demo { outline rect(0mm, 0mm, 20mm, 10mm) }";
        let compiled = "(kicad_pcb (version 20240108))";
        let (url, served) = preloaded_board_file(
            "/boards/demo.board",
            source.as_bytes(),
            InputKind::BoardCode,
            compiled,
        );
        assert_eq!(url, "/boards/demo.board");
        assert_eq!(served, source);
        assert_ne!(served, compiled);
    }
}

#[cfg(test)]
mod terminal_refusal_tests {
    use super::timing_refusal;

    #[test]
    fn timing_refusal_preserves_every_backend_reason() {
        let refusal = timing_refusal(&[
            "GPIO4 exceeded the PWL transition budget".to_string(),
            "GPIO5 exceeded the PWL transition budget".to_string(),
        ]);
        let rendered = refusal.render_text();
        assert!(rendered.contains("GPIO4"), "{rendered}");
        assert!(rendered.contains("GPIO5"), "{rendered}");
        assert!(rendered.contains("timing evidence"), "{rendered}");
    }
}

#[cfg(test)]
mod list_nets_json_tests {
    use super::list_nets_json;

    #[test]
    fn control_char_net_names_emit_valid_json() {
        // R51: the array was hand-assembled with `format!("{n:?}")`, so a net name
        // carrying a control char (e.g. U+0007 from a malformed netlist) produced
        // `\u{7}`, valid Rust Debug but invalid JSON. serde_json must emit a
        // document every JSON parser accepts.
        let nets = vec![
            "GND".to_string(),
            "bell\u{7}x".to_string(),
            "esc\u{1b}end".to_string(),
            "nét".to_string(),
        ];
        let out = list_nets_json(&nets);
        // The output must round-trip through a strict JSON parser back to the
        // original names; the base bug's `\u{7}` form fails to parse here.
        let parsed: Vec<String> =
            serde_json::from_str(&out).expect("list-nets --json must be valid JSON");
        assert_eq!(parsed, nets);
    }
}

#[cfg(test)]
mod notes_gate_tests {
    use super::notes_visible;

    #[test]
    fn shown_only_for_interactive_non_json_non_quiet() {
        // Default interactive terminal: the note is discoverable.
        assert!(notes_visible(false, false, true));
    }

    #[test]
    fn quiet_suppresses_notes() {
        assert!(!notes_visible(true, false, true));
        assert!(!notes_visible(true, false, false));
        assert!(!notes_visible(true, true, true));
    }

    #[test]
    fn json_never_emits_notes() {
        // --json is machine output: no note regardless of TTY or quiet.
        assert!(!notes_visible(false, true, true));
        assert!(!notes_visible(false, true, false));
    }

    #[test]
    fn piped_stdout_suppresses_notes() {
        // Non-TTY stdout (piped / redirected / CI): keep report output clean.
        assert!(!notes_visible(false, false, false));
    }
}
