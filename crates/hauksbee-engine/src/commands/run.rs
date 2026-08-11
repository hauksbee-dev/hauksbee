//! `hauksbee run <board>`: the run orchestrator. Loads/binds the board, then
//! dispatches to the right report (reports/), the interactive TUI, a headless
//! co-sim, or the websocket server. Argument parsing lives in the binary; this
//! takes a plain [`RunConfig`].

use hauksbee_extract::ExtractedBoard;
use hauksbee_models::ModelLibrary;

use crate::binder::bind_board;
use crate::engine::HauksbeeEngine;
use crate::result::{
    strict_analog_exit_code, BindSummary, BootGateJson, JsonInputEvidence, JsonNote, JsonNoteKind,
    JsonReport, Refusal, EXIT_INVALID_FOR_ANALYSIS,
};

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

fn input_kind_name(kind: crate::board_input::InputKind) -> &'static str {
    use crate::board_input::InputKind;
    match kind {
        InputKind::Text => "layout_or_netlist",
        InputKind::Schematic => "kicad_schematic",
        InputKind::Altium => "altium_pcbdoc",
        InputKind::Gerber => "gerber_archive",
        InputKind::Odb => "odbpp",
        InputKind::Ipc2581 => "ipc2581",
        InputKind::BoardCode => "board_as_code",
    }
}

fn valid_digest(digest: &str) -> Option<String> {
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
    // Validate `--firmware` up front, before any heavy work or the TUI takes over
    // the terminal. The native emulator loaders segfault (exit 139) on a missing
    // file instead of erroring; this turns a one-character typo into a clean,
    // actionable message naming the absolute path that was tried.
    let firmware_source = cfg.firmware.clone();
    if let Some(fw) = &cfg.firmware {
        // A PlatformIO project directory, a built .pio tree, or a zip of either
        // resolves to its compiled image first; a bare .elf/.hex passes through.
        if let Some(resolved) = crate::firmware_input::resolve_firmware_cli(fw)? {
            if !quiet {
                eprintln!("  firmware: {}", resolved.note);
            }
            cfg.firmware = Some(resolved.path);
        }
        hauksbee_mcu::validate_firmware_path(cfg.firmware.as_deref().expect("just set"))?;
    }
    // Advisory: if this board sits among sibling .kicad_pcb files (a multi-board
    // product), say so, a clean verdict on one file is misleading if the user
    // meant the whole thing. Routed through `Notes` so it stays on stderr and is
    // silenced under --quiet / --json / a piped stdout (it is helpful exactly once
    // for an interactive user, and pure noise in report and pipeline output).
    let notes = Notes::new(quiet, cfg.json);
    warn_sibling_boards(&cfg.board, notes);
    // Every input format (gerber dir/zip, binary Altium, Board-as-Code, KiCad
    // schematic hierarchy, the text layout/netlist formats) routes through the
    // ONE normalizer shared with the web front door and hauksbee-ci, including
    // the file-stem name fallback for titleless boards. `raw` keeps the file's
    // exact bytes (the Altium DRC twin reads copper from them) and `text` is
    // the KiCad-parseable layout text (empty for Altium/gerber, which have
    // none; those checks get the bytes twin or an honest "not checked").
    let norm = crate::board_input::from_path(&cfg.board)?;
    let input_kind = norm.kind;
    let input_notes = norm.notes.clone();
    let is_altium = norm.is_binary();
    let is_board_code = norm.kind == crate::board_input::InputKind::BoardCode;
    let reader_notes = norm.notes;
    let raw = norm.raw;
    let text = norm.layout_text.unwrap_or_default();
    let mut board = norm.board;
    // Layered model library: builtin < ~/.hauksbee/models (datasheet-extracted)
    // < ~/.config/hauksbee/models (user) < --models-dir (highest). Identity
    // reconciliation needs the same library the subsequent bind uses, so build
    // it before BOM/PnP are applied rather than resolving them under a smaller
    // universe of models.
    let extra: Vec<&std::path::Path> = cfg.models_dir.as_deref().into_iter().collect();
    let lib = ModelLibrary::builtin_with_user_dirs(&extra);

    let mut inputs = vec![JsonInputEvidence {
        path: cfg.board.display().to_string(),
        kind: "board".to_string(),
        format: input_kind_name(input_kind).to_string(),
        sha256: None,
        contributed: vec![format!(
            "{} components and {} nets",
            board.components.len(),
            board.nets.len()
        )],
        ignored: input_notes,
        identity: Vec::new(),
    }];

    if let Some(path) = &cfg.bom {
        let mut overrides = hauksbee_extract::bom::ColumnOverrides::new();
        for pair in &cfg.bom_columns {
            let (role, header) = hauksbee_extract::bom::ColumnOverrides::parse_pair(pair)
                .map_err(anyhow::Error::msg)?;
            overrides.set(role, header);
        }
        let artifact = hauksbee_extract::bom::Bom::read_with(path, &overrides)?;
        let identity = crate::binder::apply_bom_identity(&mut board, &artifact, &lib)?;
        inputs.push(JsonInputEvidence {
            path: artifact.provenance.path.clone(),
            kind: "bom".to_string(),
            format: artifact.provenance.kind.clone(),
            sha256: valid_digest(&artifact.provenance.sha256),
            contributed: artifact
                .provenance
                .contributed
                .iter()
                .map(|item| format!("{}: {}", item.what, item.detail))
                .collect(),
            ignored: artifact
                .provenance
                .ignored
                .iter()
                .map(|item| format!("{}: {}", item.what, item.why))
                .collect(),
            identity: identity.lines(),
        });
    }
    if let Some(path) = &cfg.placement {
        let artifact = hauksbee_extract::placement::PlacementFile::read(path)?;
        let identity = crate::binder::apply_placement_identity(&mut board, &artifact, &lib)?;
        inputs.push(JsonInputEvidence {
            path: artifact.provenance.path.clone(),
            kind: "placement".to_string(),
            format: artifact.provenance.kind.clone(),
            sha256: valid_digest(&artifact.provenance.sha256),
            contributed: artifact
                .provenance
                .contributed
                .iter()
                .map(|item| format!("{}: {}", item.what, item.detail))
                .collect(),
            ignored: artifact
                .provenance
                .ignored
                .iter()
                .map(|item| format!("{}: {}", item.what, item.why))
                .collect(),
            identity: identity.lines(),
        });
    }
    if inputs.len() > 1 && !quiet && !cfg.json {
        eprintln!("Input inventory:");
        for input in &inputs {
            eprintln!("  {} ({}, {})", input.path, input.kind, input.format);
            for line in &input.contributed {
                eprintln!("    contributed: {line}");
            }
            for line in &input.ignored {
                eprintln!("    ignored: {line}");
            }
            for line in &input.identity {
                eprintln!("    identity: {}", line.trim());
            }
        }
    }
    // Do-not-populate policy, applied before binding because DNP decides
    // whether a part is stamped at all. The decision is printed rather than
    // assumed: a board where a part was quietly added or dropped is a board
    // whose numbers mean something other than the reader thinks.
    let dnp = board.apply_dnp_policy(cfg.dnp_policy, &cfg.fit, &cfg.no_fit)?;
    if !quiet {
        for line in dnp.lines() {
            eprintln!("{line}");
        }
    }
    let board = board;
    // A board with zero components can prove nothing ABOUT ITS PARTS: every
    // part-level check would pass vacuously and a "100% clean" verdict on an
    // empty board is exactly the false comfort this tool exists to prevent (M6).
    // Refuse as invalid for analysis, on every path (reports, TUI, co-sim,
    // serve).
    //
    // A copper-geometry question is not a question about parts, though. `--drc`
    // reads copper polygons and asks whether two nets touch; `--ampacity` reads
    // trace widths and asks how much current one can carry. Both are fully
    // answerable from a layout carrying no footprints at all, and the
    // capacity-only report exists precisely to answer the second one when no
    // part attributes a current. Refusing them here turned two legitimate
    // questions into an invalid input, and worse, turned a REAL SHORT on a
    // copper-only board into exit 3 instead of the gating exit 2: a build that
    // should have gone red went "cannot analyse" instead.
    let geometry_only_request = matches!(surface, SelectedSurface::Drc | SelectedSurface::Ampacity);
    if board.components.is_empty() && !geometry_only_request {
        let msg = format!(
            "this board has no components ('{}' parsed, but is empty); \
             nothing to check, so a pass would be meaningless",
            cfg.board.display()
        );
        let refusal = Refusal::new(
            "a part-level verdict for this board",
            msg.clone(),
            vec!["the input format was recognized and parsed"],
            "export a board revision that contains component placement, or ask only --drc/--ampacity for copper geometry",
        );
        if cfg.json {
            println!(
                "{}",
                // `ok` is true iff the verdict is `pass` (docs/analysis/JSON_OUTPUT.md
                // states the invariant, and every other rollup honours it). This
                // envelope read `ok:true` beside `verdict:"invalid"` and exit 3,
                // so a consumer gating on `ok` treated a refusal as a clean run.
                serde_json::json!({ "ok": false, "verdict": "invalid", "refusal": refusal.clone() })
            );
        } else {
            eprintln!("error: {msg}");
            eprintln!("{}", refusal.render_text());
        }
        crate::reports::ci_artifacts::exit_with_refusal(EXIT_INVALID_FOR_ANALYSIS, &refusal);
    }
    // --probe records live waveforms, which only exist during a co-sim; it is
    // meaningless for the static reports and the interactive server. Fail loudly
    // rather than silently ignore the flag.
    if !cfg.probe.is_empty() && surface != SelectedSurface::Headless {
        anyhow::bail!("--probe records co-sim waveforms and needs --headless");
    }

    // Flag-consistency contract (the --probe guard above is the model): a flag
    // that names an output artifact or an analysis input whose producing
    // analysis was not requested is an ERROR, because honouring it silently
    // means a file that never appears or a selector that selects nothing. A
    // flag that merely loses a rendering/serving preference to a
    // higher-precedence one WARNS on stderr and continues. ONE policy, no
    // third category: nothing the user asked for is ever dropped without a
    // word (M1: --list-nets and --tui used to lose to a report flag silently).
    if cfg.probe_csv.is_some() && cfg.probe.is_empty() {
        anyhow::bail!(
            "--probe-csv names an output file, but no --probe net was given; \
             add --probe <NET> (and --headless)"
        );
    }
    if cfg.ac_csv.is_some() && cfg.ac.is_none() {
        anyhow::bail!(
            "--ac-csv names an output file, but no --ac sweep was requested; \
             add --ac <FSTART:FSTOP:POINTS>"
        );
    }
    if (!cfg.ac_node.is_empty() || cfg.ac_loop.is_some()) && cfg.ac.is_none() {
        anyhow::bail!(
            "--ac-node/--ac-loop describe an --ac sweep, but no --ac was requested; \
             add --ac <FSTART:FSTOP:POINTS>"
        );
    }
    if cfg.ampacity && (cfg.json || cfg.plain) {
        anyhow::bail!(
            "--ampacity is a text-only report with no --json/--plain form yet; \
             refusing rather than silently ignoring the output flag"
        );
    }
    // --thermal has a --json form but no prose renderer yet; same refusal
    // policy as --ampacity rather than silently ignoring --plain (M2).
    if cfg.thermal && cfg.plain {
        anyhow::bail!(
            "--thermal has no --plain form yet (its table and --json only); \
             refusing rather than silently ignoring the output flag"
        );
    }
    // --list-nets prints its list and exits, so a report flag alongside it
    // never renders. Warn (a lost rendering preference, not an error).
    if cfg.list_nets && any_report_flag {
        eprintln!(
            "warning: --list-nets prints the net list and exits, so the report flag \
             is ignored here"
        );
    }
    // Same for --tui: an explicit report flag prints and exits, so the
    // dashboard never launches.
    if cfg.tui && any_report_flag {
        eprintln!("warning: a report flag prints and exits, so --tui is ignored here");
    }
    if cfg.plain && cfg.json {
        eprintln!(
            "warning: --plain and --json were both given; --json wins (machine output \
             has no prose form)"
        );
    }
    if cfg.serve
        && (cfg.report
            || cfg.drc
            || cfg.lint
            || cfg.si
            || cfg.resources
            || cfg.usb_c
            || cfg.check
            || cfg.ampacity
            || cfg.thermal)
    {
        eprintln!("warning: a report flag prints and exits, so --serve is ignored here");
    }
    if cfg.oracle && !cfg.drc {
        eprintln!("warning: --oracle only applies with --drc; ignored");
    }

    // --asbuilt describes the physical board, so it is validated and applied on
    // EVERY path, not only the simulating one (the static report branches used
    // to silently discard it). A bad path, a parse error, or an overlay that
    // does not describe this board is a hard error everywhere. The bound board
    // it produced is reused by the circuit-reading branches below (--ac,
    // --list-nets, co-sim, --serve).
    let mut prebound: Option<crate::binder::BoundBoard> = None;
    if let Some(asbuilt_path) = &cfg.asbuilt {
        let overlay = crate::asbuilt::AsBuiltOverlay::load(asbuilt_path)?;
        let mut b = bind_board(&board, &lib);
        let overlay_report = overlay.apply(&mut b)?;
        // Under --json stdout must stay one machine document, so the applied
        // narration is suppressed there (the validation above still ran).
        if !quiet && !cfg.json {
            println!("as-built overlay {} applied:", asbuilt_path.display());
            for line in &overlay_report.lines {
                println!("  {line}");
            }
            println!(
                "  note: the copper/netlist checks read the DESIGN files; the overlay is \
                 applied to the simulated circuit (co-sim, --ac, --serve)."
            );
        }
        prebound = Some(b);
    }

    // The companion Eagle `.sch`, if the user named one or one sits beside the
    // board. Resolved ONCE here, where both the explicit option and board path
    // are in hand, and handed to every report surface: a copper contact the
    // schematic declares must read the same way under `--check`, `--drc`,
    // `--json` and the CI artifacts, or the same board is a declared tie on one
    // surface and a serious short on another.
    // Same `<eagle>` head sniff the DRC dispatch uses, so "is this an Eagle board"
    // is decided identically in both places.
    let board_is_eagle = text
        .chars()
        .take(512)
        .collect::<String>()
        .contains("<eagle");
    let schematic_ties =
        crate::schematic_ties::resolve(&cfg.board, &board, schematic.as_deref(), board_is_eagle)?;
    if let Some(ties) = &schematic_ties {
        use sha2::{Digest, Sha256};

        let sha256 = Sha256::digest(&ties.raw)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        inputs.push(JsonInputEvidence {
            path: ties.path.display().to_string(),
            kind: "schematic".to_string(),
            format: "eagle_schematic".to_string(),
            sha256: Some(sha256),
            contributed: vec![format!(
                "{} declared net tie(s) supplied to copper-contact qualification",
                ties.ties.len()
            )],
            ignored: vec![
                "schematic connectivity is not rebound; the Eagle .brd remains the layout/netlist source"
                    .to_string(),
            ],
            identity: Vec::new(),
        });
        if !quiet && !cfg.json {
            let how = if ties.auto_discovered {
                "found beside the board"
            } else {
                "supplied"
            };
            eprintln!(
                "schematic {} ({how}): {} declared net tie(s)",
                ties.path.display(),
                ties.ties.len()
            );
        }
    }
    if let Some(path) = &cfg.emit_manifest {
        let manifest = capture_manifest(
            &cfg,
            firmware_source.as_deref(),
            schematic.as_deref(),
            schematic_ties.as_ref(),
        )?;
        manifest.write_new(path)?;
        eprintln!(
            "wrote immutable run manifest {} to {}",
            manifest.manifest_id,
            path.display()
        );
    }

    // --junit/--sarif: evaluate the selected surface with the same waiver and
    // gate policy that surface renders. Findings remain in the transaction
    // until the final outcome commits them; co-sim appends its dynamic findings.
    let mut ci_findings: Option<Vec<crate::result::JsonFinding>> = None;
    if cfg.junit.is_some() || cfg.sarif.is_some() {
        let mut findings = crate::reports::check::gather_findings(
            &cfg.board,
            &board,
            &text,
            &raw,
            is_altium,
            &lib,
            schematic_ties.as_ref(),
        )?;
        findings.retain(|finding| ci_check_selected(surface, finding));
        let bound = bind_board(&board, &lib);
        let evidence = crate::evidence::BoardEvidence::from_bound(
            &board,
            &bound.report,
            &reader_notes,
            hauksbee_ir::evidence::RunDate::from_system_clock(),
        )?
        .with_input_artifact(&cfg.board, &raw, input_kind)?;
        let mut maps = evidence.maps_for_findings(&findings)?;
        // The same run-level/finding-backed split the JSON verdict makes:
        // finding-backed maps become badges, never gate-grade JUnit failures,
        // and the run-level claims (input coverage, bind completeness) are
        // added so an invalid JSON verdict shows red here too instead of a
        // green test-report tab beside it.
        let finding_messages: std::collections::HashSet<String> =
            findings.iter().map(|f| f.message.clone()).collect();
        for (check, assertion) in [
            ("drc", "DRC input coverage"),
            ("si", "Signal-integrity input coverage"),
        ] {
            if !(findings.iter().any(|finding| finding.check == check)
                || (surface == SelectedSurface::Drc && check == "drc")
                || (surface == SelectedSurface::Si && check == "si"))
            {
                continue;
            }
            let coverage = evidence.check_coverage_map(check, assertion)?;
            if coverage.status() != hauksbee_ir::evidence::EvidenceStatus::Clean {
                maps.push(coverage);
            }
        }
        findings.extend(crate::reports::ci_artifacts::evidence_findings_with_gate(
            &maps,
            |m| !finding_messages.contains(m.assertion()),
        ));
        let blockers = crate::result::unmodelled_critical_refs(
            &crate::result::BindSummary::from_report(&bound.report),
        );
        let blockers = if surface == SelectedSurface::UsbC {
            crate::reports::usb_c::scoped_blockers(&board, &blockers)
        } else {
            blockers
        };
        if ci_surface_is_model_dependent(surface) && !blockers.is_empty() {
            findings.push(crate::result::JsonFinding {
                check: "evidence".into(),
                kind: "undermined".into(),
                severity: "serious".into(),
                nets: Vec::new(),
                location_mm: None,
                layer: None,
                refs: blockers.clone(),
                actionable: true,
                message: format!(
                    "INVALID evidence: {}",
                    crate::result::inconclusive_verdict(&blockers)
                ),
                plain: format!(
                    "INVALID evidence: {}",
                    crate::result::inconclusive_verdict(&blockers)
                ),
                fix: Some("supply device models or BOM identity for the named parts".into()),
            });
        }
        crate::reports::ci_artifacts::set_current_findings(findings.clone());
        ci_findings = Some(findings);
    }

    // --serial-attach bridges a host serial port to the firmware's UART, so
    // without firmware there is nothing on the far end to answer. Refuse here,
    // before any binding work, rather than open a port onto silence.
    if surface == SelectedSurface::Serial && cfg.firmware.is_none() {
        anyhow::bail!(
            "--serial-attach connects your own software to the emulated MCU's UART, so it \
             needs firmware to talk to: add --firmware <FILE>"
        );
    }

    // --list-nets: print the board's net names so the user can pick one for
    // --ac-node / --ac-loop without grepping the layout. One net per line on
    // stdout (pipeable); a JSON array under --json.
    if surface == SelectedSurface::Inventory {
        let bound = match prebound.take() {
            Some(b) => b,
            None => bind_board(&board, &lib),
        };
        let mut nets: Vec<String> = bound.net_names.clone();
        nets.sort();
        if cfg.json {
            println!("{}", list_nets_json(&nets));
        } else {
            eprintln!("{} net(s):", nets.len());
            for n in &nets {
                println!("{n}");
            }
        }
        return Ok(());
    }

    // --check / --all: the whole static suite (bind + DRC + lint + SI) in ONE
    // report, so a person (or an AI) gets everything in a single command instead
    // of running one flag at a time. Honours --plain / --json / --strict.
    if surface == SelectedSurface::Check {
        return crate::reports::check::emit(
            &cfg.board,
            &board,
            &text,
            &raw,
            input_kind,
            is_altium,
            &lib,
            &reader_notes,
            crate::reports::OutputMode::from_flags(cfg.json, cfg.plain),
            cfg.strict,
            cfg.verbose,
            &inputs,
            schematic_ties.as_ref(),
        );
    }

    if surface == SelectedSurface::Bind {
        return crate::reports::bind::emit(
            &board,
            &lib,
            &reader_notes,
            crate::reports::OutputMode::from_flags(cfg.json, cfg.plain),
            &inputs,
        );
    }

    // --drc: run geometric short / clearance detection, print, exit.
    if surface == SelectedSurface::Drc {
        return crate::reports::drc::emit(
            &cfg.board,
            &board,
            &text,
            &raw,
            input_kind,
            is_altium,
            &lib,
            &reader_notes,
            crate::reports::OutputMode::from_flags(cfg.json, cfg.plain),
            cfg.oracle,
            cfg.strict,
            cfg.verbose,
            &inputs,
            schematic_ties.as_ref(),
        );
    }

    // --ampacity: IPC-2221 capacity-only report. No current is fabricated here:
    // without a per-net current spec this tells the user the bottleneck capacity
    // and explicitly asks for a current before pass/fail.
    if surface == SelectedSurface::Ampacity {
        let bound = bind_board(&board, &lib);
        let evidence = crate::evidence::BoardEvidence::from_bound(
            &board,
            &bound.report,
            &reader_notes,
            hauksbee_ir::evidence::RunDate::from_system_clock(),
        )?
        .with_input_artifact(&cfg.board, &raw, input_kind)?;
        return crate::reports::ampacity::emit(&text, is_altium, &evidence);
    }

    // --lint: run the connectivity lint-class checks, the boot strap-pin lint
    // (which needs the model db's per-part strap tables), and the MCU internal
    // resource-conflict check (a lint-class structural check too), print, exit.
    if surface == SelectedSurface::Lint {
        return crate::reports::lint::emit(
            &cfg.board,
            &board,
            &raw,
            input_kind,
            &lib,
            &reader_notes,
            crate::reports::OutputMode::from_flags(cfg.json, cfg.plain),
            cfg.strict,
            &inputs,
        );
    }

    // --resources: run only the MCU internal resource-conflict check, print, exit.
    if surface == SelectedSurface::Resources {
        return crate::reports::lint::emit_resources(
            &cfg.board,
            &board,
            &raw,
            input_kind,
            &lib,
            &reader_notes,
            crate::reports::OutputMode::from_flags(cfg.json, cfg.plain),
            cfg.strict,
            &inputs,
        );
    }

    // --usb-c: run the USB-C CC attach classifier (the RPi 4 re-derivation) and
    // print the compliance report. The capability existed but was unreachable from
    // any user-facing surface; this is its CLI front door.
    if surface == SelectedSurface::UsbC {
        let bound = bind_board(&board, &lib);
        let evidence = crate::evidence::BoardEvidence::from_bound(
            &board,
            &bound.report,
            &reader_notes,
            hauksbee_ir::evidence::RunDate::from_system_clock(),
        )?
        .with_input_artifact(&cfg.board, &raw, input_kind)?;
        let blockers = crate::result::unmodelled_critical_refs(
            &crate::result::BindSummary::from_report(&bound.report),
        );
        return crate::reports::usb_c::emit(
            &board,
            &evidence,
            crate::reports::OutputMode::from_flags(cfg.json, cfg.plain),
            cfg.strict,
            &inputs,
            &blockers,
        );
    }

    // --si: run the signal-integrity / physics static checks, print, exit. The
    // geometry-bearing checks (antenna keepout, USB length skew) need the raw
    // KiCad layout text, so it is passed through.
    if surface == SelectedSurface::Si {
        return crate::reports::si::emit(
            &cfg.board,
            &board,
            &text,
            &raw,
            input_kind,
            is_altium,
            &lib,
            &reader_notes,
            crate::reports::OutputMode::from_flags(cfg.json, cfg.plain),
            cfg.strict,
            &inputs,
        );
    }

    // --ac: small-signal AC sweep on the bound circuit, print Bode + (optional)
    // loop-stability margins, then exit. Informational like the other reports.
    if surface == SelectedSurface::Ac {
        let ac_arg = cfg.ac.as_ref().expect("AC surface has an --ac value");
        // The overlay-applied bound board when --asbuilt was given, so the AC
        // sweep runs on the reworked circuit.
        let bound = match prebound.take() {
            Some(b) => b,
            None => bind_board(&board, &lib),
        };
        let evidence = crate::evidence::BoardEvidence::from_bound(
            &board,
            &bound.report,
            &reader_notes,
            hauksbee_ir::evidence::RunDate::from_system_clock(),
        )?
        .with_input_artifact(&cfg.board, &raw, input_kind)?;
        return crate::reports::ac::emit(
            &bound,
            &evidence,
            ac_arg,
            &cfg.ac_node,
            cfg.ac_csv.as_deref(),
            cfg.ac_loop.as_deref(),
            cfg.json,
            &inputs,
        );
    }

    // Bare `--json` with no specific report selector: emit a COMBINED machine
    // report (bind + DRC + lint/straps/resources + SI) and exit. Without this,
    // `--json` alone falls through to the TUI/websocket default below and hangs a
    // piped / CI / AI caller (the regression a bare `run <board> --json` hit).
    // `--json` is an explicit machine-intent flag, so it must never launch the TUI.
    // `--thermal`/`--headless` are selectors handled further down with their OWN
    // JSON emitters (thermal coverage, co-sim notes); they must fall THROUGH this
    // combined branch or those JSON paths become unreachable dead code.
    if surface == SelectedSurface::BareJson {
        return crate::reports::check::emit_combined_json(
            &cfg.board,
            &board,
            &text,
            &raw,
            input_kind,
            is_altium,
            &lib,
            &reader_notes,
            cfg.strict,
            &inputs,
            schematic_ties.as_ref(),
        );
    }

    // Default flow (no report/headless/ac flag). The interactive terminal UI is
    // the new human-facing default: bare `run <board>` on a TTY launches it. Any
    // explicit report flag was handled above, so reaching here means none was
    // given. `--serve` keeps the historical websocket frontend; a non-TTY stdout
    // (piped / CI) also keeps the websocket behaviour untouched, so existing
    // scripts and tests are unaffected.
    //
    // `--firmware`/`--apply-shorts` only matter for the simulating paths; the TUI
    // honours `--firmware` for its co-sim pane. We branch to the TUI before
    // building the websocket engine so we never spin up tokio for the TUI path.
    let stdout_is_tty = std::io::IsTerminal::is_terminal(&std::io::stdout());
    // Altium boards reach here with an empty `text` (binary parsed from bytes);
    // the TUI's text-based build path can't analyse those, so they keep the
    // websocket flow.
    // `--serial-attach` is a live co-sim session driven from another terminal, so
    // it must never be swallowed by the TTY-default dashboard: the whole point is
    // that this terminal narrates the serial endpoint while the user's tool talks
    // to it.
    let launch_tui = !cfg.serve
        && !cfg.headless
        && !cfg.serial_attach
        && !is_altium
        && (cfg.tui || stdout_is_tty);
    if launch_tui {
        // The TUI rebuilds its board from the layout text, so it cannot apply
        // the overlay; refuse rather than show pristine-design numbers under an
        // --asbuilt flag the user believes is in effect.
        if cfg.asbuilt.is_some() {
            anyhow::bail!(
                "the interactive dashboard does not apply --asbuilt; run a report \
                 (--check/--report) or a co-sim (--headless/--serve) instead"
            );
        }
        // Forcing the TUI without a terminal on the other end fails deep inside
        // the terminal setup with a bare OS error; say what is actually wrong.
        if cfg.tui && !stdout_is_tty && !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
            anyhow::bail!(
                "the interactive dashboard needs a terminal, and neither stdin nor stdout \
                 is one (output is piped or redirected). Drop --tui and use --check/--report \
                 for a report, --json for machine output, or --serve for the browser UI"
            );
        }
        return crate::tui::run(
            &cfg.board,
            &text,
            cfg.models_dir.as_deref(),
            cfg.firmware.clone(),
            schematic.as_deref(),
        );
    }

    // Bind with the layered library (so a --models-dir / user-dir custom part is
    // in scope), then build the engine from the bound board. When --asbuilt was
    // given, the overlay was already validated, applied and narrated up front;
    // reuse that bound board rather than re-binding and re-applying.
    let bound = match prebound.take() {
        Some(b) => b,
        None => bind_board(&board, &lib),
    };
    let mut board_evidence = crate::evidence::BoardEvidence::from_bound(
        &board,
        &bound.report,
        &reader_notes,
        hauksbee_ir::evidence::RunDate::from_system_clock(),
    )?
    .with_input_artifact(&cfg.board, &raw, input_kind)?;
    if let Some(firmware) = &cfg.firmware {
        board_evidence = board_evidence.with_firmware_artifact(
            firmware,
            &std::fs::read(firmware).map_err(|error| {
                anyhow::anyhow!(
                    "reading firmware evidence '{}': {error}",
                    firmware.display()
                )
            })?,
        )?;
    }
    // Net names captured before `bound` is consumed, for --probe validation.
    let probe_known_nets: Vec<String> = if cfg.probe.is_empty() {
        Vec::new()
    } else {
        bound.net_names.clone()
    };
    // Pre-flight: firmware on a `qemu:` backend needs the Espressif QEMU fork.
    // On an interactive terminal, offer to fetch it inline (official prebuilt,
    // into ~/.hauksbee-qemu-esp) so the run continues; declined or
    // non-interactive paths keep the loud install-guidance error the scheduler
    // raises. CLI-layer on purpose: the library/server must never prompt.
    if cfg.firmware.is_some() {
        // Firmware on a board with no processor cannot produce an answer: every
        // "the firmware must ..." assertion would pass because nothing ever ran.
        // That is invalid-for-analysis, not a warning, so it exits 3 like the
        // other unanswerable runs rather than reporting a vacuous success.
        if bound.mcus.is_empty() {
            let missing =
                crate::binder::no_processor_message(&bound.dnp_mcus, crate::binder::FitRemedy::Cli);
            let refusal = Refusal::new(
                "firmware behavior on this board",
                missing.clone(),
                vec!["board extraction, binding, and static copper checks remain available"],
                "fit/select a supported MCU on the board or remove --firmware and rerun the static analysis",
            );
            if cfg.json {
                let mut jr = JsonReport::new(&bound.name, BindSummary::from_report(&bound.report));
                jr.refusal = Some(refusal.clone());
                println!("{}", jr.to_json());
            } else {
                eprintln!("error: {missing}");
                eprintln!("{}", refusal.render_text());
            }
            crate::reports::ci_artifacts::exit_with_refusal(
                crate::result::EXIT_INVALID_FOR_ANALYSIS,
                &refusal,
            );
        }
        let backends: Vec<String> = bound.mcus.iter().map(|m| m.backend.clone()).collect();
        crate::commands::install::offer_esp_qemu_install(&backends)?;
    }
    let mut engine = HauksbeeEngine::from_bound(
        bound,
        cfg.firmware.as_deref(),
        &format!("/boards/{}", crate::commands::common::file_name(&cfg.board)),
    )?;
    board_evidence =
        board_evidence.with_scoped_substitutions(engine.scheduler().scoped_substitutions())?;

    // --apply-shorts: bridge every detected copper short before simulating.
    if cfg.apply_shorts {
        let report = if is_altium {
            ExtractedBoard::altium_drc(&raw)?
        } else {
            ExtractedBoard::drc_with_clearance_rules(
                &text,
                crate::reports::kicad_pro_clearance_rules(&cfg.board, &board),
            )?
        };
        let qualification = schematic_ties.as_ref().map(|ties| ties.qualify(&report));
        let applied = engine.apply_drc_shorts_with_qualification(&report, qualification.as_ref());
        eprintln!(
            "applied {applied} copper short(s) of {} detected ({} clearance violations)",
            report.short_count(),
            report.clearance_violations().count(),
        );
        // A served live sim must also DISCLOSE the outcome on the wire
        // BoardInfo, matching the report co-sim's "ran WITH the shorts
        // bridged" note.
        if cfg.serve && report.short_count() > 0 {
            engine.set_shorts_disclosure(hauksbee_server::protocol::ShortsDisclosure {
                detected: report.short_count(),
                bridged: applied,
                unapplied_reason: (applied == 0).then(|| {
                    "the shorted nets could not be bridged into the live circuit".to_string()
                }),
            });
        }
    }

    // --thermal: run a short co-sim, then print the steady-state junction
    // temperature per dissipating device and exit. Fix #1: a thermal table that
    // covers ~no dissipating devices because the power ICs are UNRESOLVED is a
    // meaningless result, not a "runs cool" pass, flag it invalid and exit 3.
    // Strict is the DEFAULT: a PARTIAL-coverage table escalates to exit 3
    // unless --no-strict-thermal opts out (--strict-thermal is accepted as a
    // quiet no-op so existing CI invocations keep working).
    if surface == SelectedSurface::Thermal {
        return crate::reports::thermal::emit(
            &mut engine,
            &board_evidence,
            cfg.ambient,
            cfg.seconds,
            cfg.json,
            !cfg.no_strict_thermal,
            &inputs,
        );
    }

    // --serial-attach: a live co-sim with a host-facing serial port. Placed before
    // the headless report path because it IS a co-sim run, just one whose stimulus
    // comes from the user's own software instead of a report flag; it prints its
    // own endpoint narration and session summary.
    if surface == SelectedSurface::Serial {
        let scfg = crate::commands::hostserial::SerialSessionConfig {
            transport: cfg.serial_transport,
            wait_secs: cfg.serial_wait,
            pace: !cfg.serial_no_pace,
            mcu: cfg.serial_mcu.clone(),
            chunk_us: cfg.chunk_us,
            ..Default::default()
        };
        let mut say = |line: &str| eprintln!("{line}");
        let summary =
            crate::commands::hostserial::run_session(&mut engine, cfg.seconds, &scfg, &mut say)?;
        for line in crate::commands::hostserial::summary_lines(&summary) {
            eprintln!("{line}");
        }
        return Ok(());
    }

    if surface == SelectedSurface::Headless {
        // --probe preconditions, checked before the run so a typo fails fast with
        // the same near-match style the rest of the net-facing CLI uses.
        let probes = crate::reports::cosim::dedup_probes(&cfg.probe);
        // `--probe ''` (an empty shell expansion, usually) would silently
        // record nothing: the dedup drops empties, so catch the case where
        // everything the user passed was empty (M8).
        if !cfg.probe.is_empty() && probes.is_empty() {
            anyhow::bail!(
                "--probe was given only empty net name(s); pass a real net \
                 (run --list-nets to see every net name)"
            );
        }
        crate::reports::cosim::validate_probes(
            &probes,
            cfg.probe_csv.as_deref(),
            &probe_known_nets,
        )?;

        let board_name = engine.report().board_name.clone();
        let summary = BindSummary::from_report(engine.report());
        // Captured before `summary` is consumed by the JSON branch; the
        // convergence-abort diagnosis below names this count.
        let unresolved_active_count = crate::result::coverage_open_active_refs(&summary).len();
        let mut uart_seen = false;
        let headless = crate::reports::cosim::run_headless(
            &mut engine,
            cfg.seconds,
            &mut uart_seen,
            cfg.json,
            cfg.strict,
            &probes,
            cfg.probe_csv.as_deref(),
            cfg.chunk_us,
        )?;
        let achieved_factor = headless.realtime_factor();
        let wall_s = headless.wall_s;
        let faults = headless.faults;

        // Co-sim honesty summary (Track B): total net toggles, UART activity, and
        // any chip substitution detected at build time. Built from the SAME run
        // stats the text table reads, so every surface agrees. The achieved
        // rate is stamped from the run's own wall-clock measurement so the
        // machine surface carries the delivered factor, not an assumed one.
        let cosim = crate::reports::cosim::build_cosim_json(&engine, uart_seen)?.map(|mut c| {
            c.wall_s = wall_s;
            c.realtime_factor = achieved_factor;
            c
        });
        let total_toggles = cosim.as_ref().map(|c| c.total_toggles).unwrap_or(0);
        // Analog-fidelity honesty (05 §3b): once any chunk's analog solve failed,
        // the run held stale voltages and cannot vouch for analog-derived findings
        // over the failed windows. `analog_abort` is the stricter condition: the
        // solve was stuck for a whole streak of chunks, so a strict run must
        // refuse (exit 3) rather than complete a fake-quiet run.
        let analog_valid = engine.scheduler().analog_valid();
        let failed_chunk_count = engine.scheduler().failed_chunk_count();
        // Chunks the primary solve failed but a fallback rung carried. These are
        // solved windows, so they are NOT failures, but the number in them came
        // from a more dissipative method and the reader is entitled to know
        // which. Disclosed independently of `analog_valid` for that reason.
        let fallback_chunk_count = engine.scheduler().fallback_chunk_count();
        let analog_abort = engine.scheduler().analog_abort_tripped();
        let timing_refusals = engine.scheduler().timing_refusals().to_vec();
        // A co-sim that drove no GPIO, produced no net toggles, AND emitted no
        // UART did not exercise the firmware. `any_gpio_driven()` is essential:
        // a firmware that drives a control line high and HOLDS it (boot-gate style)
        // has zero net toggles yet clearly ran, so a toggles-only test would cry
        // wolf on it. Determined BEFORE emitting so the refusal reaches every
        // surface, including --json when no MCU was instantiated (cosim is None).
        let zero_activity =
            total_toggles == 0 && !uart_seen && !engine.scheduler().any_gpio_driven();
        let strict_refusal = if cfg.strict && analog_abort {
            Some(Refusal::new(
                "a trustworthy strict co-sim verdict for the firmware and analog circuit",
                format!(
                    "the analog solver failed for {failed_chunk_count} chunk(s), including the consecutive-failure abort; affected windows held stale voltages"
                ),
                vec!["static board/copper findings and explicitly non-analog observations remain available"],
                "inspect the first analog non-convergence diagnosis below, fix its named net/device, then rerun the same strict command",
            ))
        } else if cfg.strict && zero_activity {
            Some(Refusal::new(
                "firmware behavior during this strict co-simulation",
                "the run observed no GPIO drive, net toggles, or UART output, so it cannot prove the firmware executed meaningfully",
                vec!["board extraction, binding, and static copper findings remain valid"],
                "verify the firmware image/MCU match and boot entry, then rerun with a probe or UART-producing fixture",
            ))
        } else {
            None
        };

        // Boot-safety advisory, derived once (so --json and --plain agree) from
        // the library so the TUI and web get the same advisory from the same call.
        // `held_high_control_nets` are the heads-up hazards (a control net driven/
        // pulled HIGH and held from reset that switches a transistor/relay and has
        // no bias resistor, a MOSFET gate / relay / igniter energised at power-up;
        // the switch requirement is the zero-FP guard). `gate_states` is the
        // informational per-gate panel, populated only when firmware actually ran
        // (with no --firmware or a stalled one, every gate would read "floating").
        let boot_advisory = crate::checks::boot::analyze(
            &board,
            &engine.scheduler().firmware_held_high_nets(),
            &engine.scheduler().firmware_output_configured_nets(),
            &engine.scheduler().firmware_driven_nets(),
            cfg.firmware.is_some() && !zero_activity,
        );
        let held_high_boot_nets = &boot_advisory.held_high_control_nets;
        let has_boot_advisory = !held_high_boot_nets.is_empty();
        let gate_rows = &boot_advisory.gate_states;

        let budget = engine.scheduler().error_budget()?;
        let fault_findings = crate::result::fault_findings_json(&faults);
        // The co-sim fault gate, asked of the findings the CI artifacts carry
        // rather than of the fault list beside them: `fault_findings_json`
        // emits one finding per fault, so this is the same set the JUnit
        // `<failure>` count and the SARIF error levels are built from, and the
        // exit code cannot grade the run differently from the archived file.
        let faults_gate = fault_findings.iter().any(|f| f.gates());
        let mut run_findings = fault_findings.clone();
        if cfg.strict_boot {
            run_findings.extend(held_high_boot_nets.iter().map(|net| crate::result::JsonFinding {
                check: "cosim".into(),
                kind: "boot_control_net".into(),
                severity: "warning".into(),
                nets: vec![net.clone()],
                location_mm: None,
                layer: None,
                refs: Vec::new(),
                actionable: true,
                message: format!(
                    "--strict-boot: control net '{net}' is driven HIGH and held from power-up with no bias resistor"
                ),
                plain: format!(
                    "--strict-boot: control net '{net}' can energise its load at reset"
                ),
                fix: Some("add a hardware bias that keeps the load off through reset, or confirm and document the polarity".into()),
            }));
        }
        let mut cosim_maps = Vec::new();
        for finding in &fault_findings {
            cosim_maps.push(board_evidence.simulation_map(
                finding.message.clone(),
                &finding.nets,
                &finding.refs,
                Some(budget.clone()),
            )?);
        }
        if let Some(summary) = &cosim {
            cosim_maps.push(board_evidence.simulation_map(
                format!(
                    "Firmware behaviour for {} executed on {}",
                    summary.mcu_ref, summary.requested_part
                ),
                &[],
                std::slice::from_ref(&summary.mcu_ref),
                Some(budget.clone()),
            )?);
            for activity in &summary.activity_summary {
                cosim_maps.push(board_evidence.simulation_map(
                    format!(
                        "{} toggled {} times between {:.6} V and {:.6} V",
                        activity.net, activity.toggles, activity.v_min, activity.v_max
                    ),
                    std::slice::from_ref(&activity.net),
                    &[],
                    Some(budget.clone()),
                )?);
            }
        }
        let run_evidence = board_evidence.clone().with_maps(cosim_maps);

        // Rewrite CI artifacts from the final co-sim evidence object before a
        // strict gate can exit. Invalid evidence is a failure in JUnit/SARIF and
        // a GitHub error annotation; qualified evidence remains visible.
        if let Some(base) = &ci_findings {
            let mut all = base.clone();
            all.extend(run_findings.clone());
            // The same run-level split as the static pass: an undermined map
            // backing one of the findings already in the file is that
            // finding's badge, not a second gate-grade failure. Co-sim
            // simulation maps and binding-completeness maps are run-level and
            // keep gating.
            let rewrite_messages: std::collections::HashSet<&str> = base
                .iter()
                .chain(run_findings.iter())
                .map(|f| f.message.as_str())
                .collect();
            all.extend(crate::reports::ci_artifacts::evidence_findings_with_gate(
                run_evidence.maps(),
                |m| !rewrite_messages.contains(m.assertion()),
            ));
            crate::reports::ci_artifacts::set_current_findings(all.clone());
            crate::reports::ci_artifacts::github_evidence_annotations_with_gate(
                run_evidence.maps(),
                |m| !rewrite_messages.contains(m.assertion()),
            );
        } else {
            let fault_messages: std::collections::HashSet<&str> =
                run_findings.iter().map(|f| f.message.as_str()).collect();
            crate::reports::ci_artifacts::github_evidence_annotations_with_gate(
                run_evidence.maps(),
                |m| !fault_messages.contains(m.assertion()),
            );
        }

        if cfg.json {
            // The co-sim machine report is a model-dependent-claim surface
            // like the static one: an unbound verdict-critical part gates its
            // verdict too, or a firmware run could read pass where the static
            // JSON for the same board says invalid.
            let mut jr = JsonReport::new(&board_name, summary)
                .with_bind_verdict_gate()
                // The co-sim exit gate fails on ANY raised fault, and the
                // plain-language classifier grades most of them `warning`, so
                // without this the co-sim document read `pass` beside its own
                // exit 2. `--strict-boot` is the same story one flag over: it
                // turns the boot advisory into an exit 2, so under that flag
                // the advisory is gate-grade for this document too.
                .with_surface_gate(faults_gate || (cfg.strict_boot && has_boot_advisory))
                .with_inputs(&inputs)
                .with_evidence(&run_evidence);
            // A substitution is an info-level note that must never be silently
            // absent (it changes how much the co-sim result can be trusted).
            for sub in engine.scheduler().substitutions() {
                jr.notes.push(JsonNote {
                    kind: JsonNoteKind::CosimSubstitution,
                    message: sub.message(),
                });
            }
            if zero_activity {
                jr.notes.push(JsonNote {
                    kind: JsonNoteKind::Coverage,
                    message: "co-sim saw zero net toggles and no UART output; the \
                              firmware was not exercised; this result cannot vouch \
                              for firmware behaviour"
                        .to_string(),
                });
            }
            // Independent of analog_valid: a run can be fully valid AND contain
            // windows a fallback rung produced. Silence here would let a
            // first-order, dissipative window read as a first-class one.
            if fallback_chunk_count > 0 {
                jr.notes.push(JsonNote {
                    kind: JsonNoteKind::Coverage,
                    message: format!(
                        "co-sim analog solve fell back on {fallback_chunk_count} chunk(s); \
                         those windows are converged but were produced by a fallback \
                         integration path (see fallback_windows in the co-sim JSON \
                         for the method, its known numerical trade-off, and the \
                         measured error_estimate_v per window)"
                    ),
                });
            }
            // A non-convergent chunk held stale voltages: a loud coverage note so
            // a CI consumer that filters notes (not just the CosimJson body) sees
            // the analog side is not trustworthy over the failed windows (05 §3b).
            if !analog_valid {
                jr.notes.push(JsonNote {
                    kind: JsonNoteKind::Coverage,
                    message: format!(
                        "co-sim analog solve failed on {failed_chunk_count} chunk(s) \
                         that no fallback integration could carry; those windows held \
                         stale node voltages and are reported as analog_valid:false; \
                         analog-derived findings over them are not trustworthy"
                    ),
                });
                // One note PER failed window naming the interval and the
                // solver's diagnosis, so a JSON consumer gets the offending net
                // and element without re-running the board (E29).
                for d in engine.scheduler().failed_window_diagnoses() {
                    jr.notes.push(JsonNote {
                        kind: JsonNoteKind::Coverage,
                        message: format!("analog non-convergence at {d}"),
                    });
                }
            }
            // A drive that lost to a co-located source, named on both sides
            // (E30). Never silent: a run that reports 3.300 V on a net the user
            // asked to force to 20 V has to say why.
            for msg in engine.scheduler().drive_conflicts() {
                jr.notes.push(JsonNote {
                    kind: JsonNoteKind::Coverage,
                    message: msg,
                });
            }
            // Co-sim coverage honesty (U3): dropped ADC injections and
            // never-exercised bus peripherals are silent-garbage modes; they
            // ride the same Coverage note channel analog_valid uses, in
            // addition to the structured CosimJson fields, so a consumer that
            // only filters notes still sees them.
            for d in engine.scheduler().adc_dropped() {
                jr.notes.push(JsonNote {
                    kind: JsonNoteKind::Coverage,
                    message: d.message(),
                });
            }
            for b in engine.scheduler().unexercised_buses() {
                jr.notes.push(JsonNote {
                    kind: JsonNoteKind::Coverage,
                    message: b.message(),
                });
            }
            // Watchdog coverage, same channel and the same reason: a backend
            // whose armed watchdog never fires lets hung firmware run forever,
            // so a consumer that only filters notes must still learn that this
            // run cannot vouch for the recovery path.
            for (mcu_ref, limitation) in engine.scheduler().watchdog_limitations() {
                jr.notes.push(JsonNote {
                    kind: JsonNoteKind::Coverage,
                    message: crate::scheduler::watchdog_limitation_message(&mcu_ref, &limitation),
                });
            }
            for (mcu_ref, resets) in engine.scheduler().watchdog_resets() {
                jr.notes.push(JsonNote {
                    kind: JsonNoteKind::Coverage,
                    message: crate::scheduler::watchdog_reset_message(&mcu_ref, resets),
                });
            }
            // Timing coverage: a known systematic time bias on a core makes a
            // time-based assertion there mean less than it looks, and a
            // consumer that only filters notes must still learn it.
            for (mcu_ref, limitation) in engine.scheduler().timing_limitations() {
                jr.notes.push(JsonNote {
                    kind: JsonNoteKind::Coverage,
                    message: crate::scheduler::timing_limitation_message(&mcu_ref, &limitation),
                });
            }
            for w in crate::reports::cosim::heuristic_framing_warnings(
                &engine.scheduler().spi_framing_modes(),
            ) {
                jr.notes.push(JsonNote {
                    kind: JsonNoteKind::Coverage,
                    message: format!("co-sim: {w}"),
                });
            }
            // Sub-chunk pulses invisible to tick-evaluated sequential parts
            // (friction 1.16) and runtime driver contention: same Coverage
            // note channel, in addition to the structured CosimJson fields,
            // so a consumer that only filters notes still sees them.
            for p in engine.scheduler().short_pulses() {
                jr.notes.push(JsonNote {
                    kind: JsonNoteKind::Coverage,
                    message: p.message(),
                });
            }
            for c in engine.scheduler().driver_contentions() {
                jr.notes.push(JsonNote {
                    kind: JsonNoteKind::Coverage,
                    message: c.message(),
                });
            }
            for net in held_high_boot_nets {
                jr.notes.push(JsonNote {
                    kind: JsonNoteKind::BootControlNet,
                    message: format!(
                        "control net '{net}' drives a transistor/relay, is driven HIGH and held \
                         from power-up, and has no resistor setting a safe default. If a HIGH on \
                         it turns the switched load ON when it must stay OFF until firmware \
                         enables it, it is energised at power-up; confirm the polarity and that \
                         this is intended."
                    ),
                });
            }
            if !gate_rows.is_empty() {
                jr.boot_gates = Some(
                    gate_rows
                        .iter()
                        .map(|(reference, net, state)| BootGateJson {
                            reference: reference.clone(),
                            net: net.clone(),
                            state: state.json().to_string(),
                        })
                        .collect(),
                );
            }
            // The electrical-stress faults must reach the machine surface too: the
            // --plain path renders them and --strict gates on them, but --json used
            // to omit them entirely, so a CI consumer parsing the JSON saw a clean
            // run over a board the co-sim flagged (a destroyed MOSFET, overcurrent…).
            if !run_findings.is_empty() {
                jr.findings = Some(run_findings.clone());
            }
            jr.cosim = cosim;
            jr.refusal = strict_refusal.clone();
            println!("{}", jr.to_json());
        } else if cfg.plain {
            // A co-sim with no stress faults is NOT plainly "healthy" if it ran on
            // a substitute chip or never exercised the firmware. Surface those as
            // heads-up notes so the verdict reads "no failures, but N worth a look"
            // (via PlainReport::verdict) instead of a bare "Looks healthy".
            let mut report = crate::plain_faults(&faults);
            for sub in engine.scheduler().substitutions() {
                report.heads_up.push(crate::plain::HeadsUp::note(format!(
                    "co-sim ran on a SUBSTITUTE chip: {}",
                    sub.message()
                )));
            }
            if zero_activity {
                report.heads_up.push(crate::plain::HeadsUp::note(
                    "co-sim saw zero net toggles and no UART output; the firmware was not \
                     exercised, so this result cannot vouch for firmware behaviour",
                ));
            }
            if fallback_chunk_count > 0 {
                report.heads_up.push(crate::plain::HeadsUp::note(format!(
                    "co-sim analog solve fell back on {fallback_chunk_count} chunk(s): \
                     those windows are converged, but a more robust and less accurate \
                     method produced them, so fast transients and ringing inside them are \
                     damped. Rerun with --json for the method and window of each"
                )));
            }
            if !analog_valid {
                report.heads_up.push(crate::plain::HeadsUp::note(format!(
                    "co-sim analog solve failed on {failed_chunk_count} chunk(s) that no \
                     fallback integration could carry; those windows held stale voltages \
                     and cannot be trusted (analog_valid is false)"
                )));
                // The interval AND the diagnosis, inline. "Rerun with --json to
                // see the windows" was the whole defect: the one surface a
                // person actually reads named nothing (E29).
                for d in engine.scheduler().failed_window_diagnoses() {
                    report.heads_up.push(crate::plain::HeadsUp::note(format!(
                        "analog non-convergence at {d}"
                    )));
                }
            }
            for msg in engine.scheduler().drive_conflicts() {
                report.heads_up.push(crate::plain::HeadsUp::note(msg));
            }
            // Co-sim coverage honesty (U3): the same dropped-ADC / unexercised-bus
            // / heuristic-framing warnings the JSON notes carry, as plain
            // heads-ups so the verdict reads "no failures, but N worth a look".
            for d in engine.scheduler().adc_dropped() {
                report
                    .heads_up
                    .push(crate::plain::HeadsUp::note(d.message()));
            }
            for b in engine.scheduler().unexercised_buses() {
                report
                    .heads_up
                    .push(crate::plain::HeadsUp::note(b.message()));
            }
            // Watchdog coverage, worded identically to the JSON notes and the
            // default text summary.
            for (mcu_ref, limitation) in engine.scheduler().watchdog_limitations() {
                report.heads_up.push(crate::plain::HeadsUp::note(
                    crate::scheduler::watchdog_limitation_message(&mcu_ref, &limitation),
                ));
            }
            for (mcu_ref, resets) in engine.scheduler().watchdog_resets() {
                report.heads_up.push(crate::plain::HeadsUp::note(
                    crate::scheduler::watchdog_reset_message(&mcu_ref, resets),
                ));
            }
            // Timing coverage, worded identically to the JSON notes and the
            // default text summary.
            for (mcu_ref, limitation) in engine.scheduler().timing_limitations() {
                report.heads_up.push(crate::plain::HeadsUp::note(
                    crate::scheduler::timing_limitation_message(&mcu_ref, &limitation),
                ));
            }
            for w in crate::reports::cosim::heuristic_framing_warnings(
                &engine.scheduler().spi_framing_modes(),
            ) {
                report.heads_up.push(crate::plain::HeadsUp::note(w));
            }
            // Sub-chunk pulse and driver-contention findings, same wording as
            // the JSON notes and the default text summary.
            for p in engine.scheduler().short_pulses() {
                report
                    .heads_up
                    .push(crate::plain::HeadsUp::note(p.message()));
            }
            for c in engine.scheduler().driver_contentions() {
                report
                    .heads_up
                    .push(crate::plain::HeadsUp::note(c.message()));
            }
            // Boot-safety heads-up: control nets the firmware switches ON and
            // holds from power-up, with no resistor setting a safe default. The
            // netlist alone cannot tell whether a power-up HIGH is intended;
            // running the firmware can. This is what surfaces, e.g., a MOSFET /
            // relay / igniter that energises at reset because firmware drove its
            // gate high (or enabled a pull-up on it) before anything else ran.
            for net in held_high_boot_nets {
                report.heads_up.push(crate::plain::HeadsUp::note(format!(
                    "control net '{net}' switches a transistor/relay and is driven HIGH and held \
                     from the moment the board powers up, with no resistor setting a safe default \
                     level. If a HIGH on this net turns the load ON when it must stay OFF until \
                     the firmware deliberately enables it (a MOSFET, relay, motor driver, or \
                     igniter), it is energised at power-up; confirm the polarity and that this \
                     is intended."
                )));
            }
            // Bind-coverage caveat: a co-sim over a board with unmodeled/open
            // active ICs cannot vouch for the firmware/analog behaviour on their
            // nets. --report and the web/JSON surfaces already say this; the
            // headless text/plain path must too, or a clean-looking co-sim silently
            // hides that half the board was never modelled.
            let open = crate::result::coverage_open_active_refs(&summary);
            if !open.is_empty() {
                report.heads_up.push(crate::plain::HeadsUp::note(format!(
                    "co-sim coverage: {} of {} critical parts modelled: {} active IC(s) are \
                     unresolved or open, so firmware/analog/thermal results on their nets are \
                     INCOMPLETE (the copper/DRC checks are unaffected). Scaffold a model for \
                     one:  hauksbee models new --board {} {}",
                    summary.critical_parts_bound_n,
                    summary.critical_parts_total,
                    open.len(),
                    cfg.board.display(),
                    open[0]
                )));
            }
            println!();
            print!("{}", report.render());
            if !gate_rows.is_empty() {
                print!("{}", crate::checks::boot::render_boot_gate_panel(gate_rows));
            }
        } else {
            // Default text headless mode: the co-sim activity table is printed
            // elsewhere, but the boot power-up hazard and the gate panel must
            // surface here too, otherwise the plainest persona is the ONLY one
            // that hides a switched load energised at reset (the --json/--plain/
            // web surfaces all carry it). Advisory-only (no exit-code change);
            // --strict-boot still escalates below.
            for net in held_high_boot_nets {
                println!(
                    "BOOT HAZARD: control net '{net}' switches a transistor/relay and is driven \
                     HIGH and held from the moment the board powers up, with no resistor setting \
                     a safe default level; if a HIGH turns the load ON when it must stay OFF \
                     until firmware enables it (a MOSFET, relay, motor driver, or igniter), it is \
                     energised at power-up. Confirm the polarity and that this is intended."
                );
            }
            if !gate_rows.is_empty() {
                print!("{}", crate::checks::boot::render_boot_gate_panel(gate_rows));
            }
            let open = crate::result::coverage_open_active_refs(&summary);
            if !open.is_empty() {
                println!(
                    "co-sim coverage: {} of {} critical parts modelled: {} active IC(s) \
                     unresolved/open, so firmware/analog results on their nets are INCOMPLETE \
                     (copper/DRC checks are unaffected). Scaffold a model for one:  \
                     hauksbee models new --board {} {}",
                    summary.critical_parts_bound_n,
                    summary.critical_parts_total,
                    open.len(),
                    cfg.board.display(),
                    open[0]
                );
            }
        }

        if !cfg.json {
            print!("{}", run_evidence.render_plain());
        }

        // 0-activity refusal (Track B): warn always; under --strict this is a hard
        // refusal (exit 3), not a clean pass. The UART-AND-toggles guard avoids
        // false positives on firmware that is busy on the bus but quiet on GPIO.
        if zero_activity {
            eprintln!(
                "WARNING: co-sim saw zero net toggles; cannot vouch for firmware \
                 behaviour (the MCU may have stalled at boot, run no I/O, or the \
                 firmware may not match this board)."
            );
            if cfg.strict && !analog_abort {
                if let Some(refusal) = &strict_refusal {
                    crate::reports::ci_artifacts::set_current_refusal(refusal.clone());
                }
                if !cfg.json {
                    if let Some(refusal) = &strict_refusal {
                        eprintln!("{}", refusal.render_text());
                    }
                }
                // `fail` outranks `invalid`, the same precedence the verdict
                // field applies: a run that raised real electrical faults CAN
                // be judged, so the fault gate below takes the exit (2) and
                // this refusal stays on stderr and in the artifacts. Exiting 3
                // here put "invalid for analysis" beside a document reading
                // `"verdict":"fail"`. A timing refusal, if this run also had
                // one, still exits 3 from between here and that gate: that
                // exception is documented in docs/ci/CI.md.
                if !faults_gate {
                    crate::reports::ci_artifacts::exit_with_refusal(
                        EXIT_INVALID_FOR_ANALYSIS,
                        strict_refusal.as_ref().expect("zero-activity refusal"),
                    );
                }
            }
        }

        // A waveform that exceeded a concrete replay capability is invalid
        // evidence. Always disclose it; strict mode fails closed with the same
        // INVALID code used for other evidence gaps.
        if !timing_refusals.is_empty() {
            for refusal in &timing_refusals {
                eprintln!("WARNING: timing evidence invalid: {refusal}");
            }
            if cfg.strict {
                let refusal = timing_refusal(&timing_refusals);
                crate::reports::ci_artifacts::exit_with_refusal(
                    EXIT_INVALID_FOR_ANALYSIS,
                    &refusal,
                );
            }
        }

        // Refuse-rather-than-fake (05 §3b): once the analog solve was stuck for a
        // whole streak of chunks, a strict run must abort with the invalid code
        // rather than complete a fake-quiet run. Warn always so the reason is never
        // silent; only --strict turns it into a failing exit.
        if analog_abort {
            eprintln!(
                "WARNING: co-sim analog solve failed to converge for {} chunks in a row \
                 ({} failed chunks total); no fallback integration could carry them, so \
                 the run held stale voltages and cannot vouch for the analog side. The usual cause is unresolved active parts leaving \
                 nodes floating: {} active IC(s) are unresolved/open here (hauksbee models \
                 --help). See {}.",
                crate::scheduler::STRICT_CONSECUTIVE_FAILED_ABORT,
                failed_chunk_count,
                unresolved_active_count,
                hauksbee_ir::docs_url("docs/about/LIMITATIONS.md"),
            );
            if let Some(code) = strict_analog_exit_code(cfg.strict && analog_abort) {
                if let Some(refusal) = &strict_refusal {
                    crate::reports::ci_artifacts::set_current_refusal(refusal.clone());
                }
                if !cfg.json {
                    if let Some(refusal) = &strict_refusal {
                        eprintln!("{}", refusal.render_text());
                    }
                }
                // NOT the zero-activity precedence: there the analog solve was
                // sound, so raised faults were a judgement the run could make.
                // Here the solve held stale voltages over the failed windows,
                // which is where these faults may come from, so the honest exit
                // is still invalid-for-analysis. The document's `fail` verdict
                // grades the faults as observed; this code says they could not
                // be trusted, and refusing outranks that.
                crate::reports::ci_artifacts::exit_with_refusal(
                    code,
                    strict_refusal.as_ref().expect("analog refusal"),
                );
            }
        }

        // Strict: any fault raised during the run fails the gate, and the last
        // line says so; a bare exit 2 reads as a tool crash, and --plain's
        // "worth a look" verdict used to contradict the failing code.
        if cfg.strict && faults_gate {
            let items: Vec<String> = faults
                .iter()
                .map(|f| format!("cosim-{} {}", f.kind.as_str(), f.component))
                .collect();
            crate::reports::strict_gate_exit(
                crate::reports::OutputMode::from_flags(cfg.json, cfg.plain),
                &items,
            );
        }
        // --strict-boot: opt-in escalation of the boot-safety advisory to a
        // failing gate (exit 2). The run was valid and these are real findings
        // about specific nets; default behaviour leaves them advisory-only. Print
        // the reason to stderr so the failure is never silent, including in the
        // default headless mode (neither --json nor --plain), where the advisory
        // text is not otherwise emitted.
        if cfg.strict_boot && has_boot_advisory {
            let mut items = Vec::new();
            for net in held_high_boot_nets {
                eprintln!(
                    "BOOT HAZARD (--strict-boot): control net '{net}' switches a transistor/relay \
                     and is driven HIGH and held from power-up with no bias resistor; the load is \
                     energised at reset."
                );
                items.push(format!("strict-boot {net}"));
            }
            crate::reports::strict_gate_exit(
                crate::reports::OutputMode::from_flags(cfg.json, cfg.plain),
                &items,
            );
        }
        // Mirror of the co-sim JSON verdict: the bind contract (unbound
        // verdict-critical parts) and undermined run-level simulation maps
        // exit 3; a per-fault map is that finding's badge and does not, the
        // same exemption the verdict field applies through the matching
        // fault-finding messages.
        let strict_blockers = crate::result::unmodelled_critical_refs(
            &crate::result::BindSummary::from_report(engine.report()),
        );
        let strict_invalid = !strict_blockers.is_empty()
            || crate::result::run_level_undermined(run_evidence.maps(), |a| {
                run_findings.iter().any(|f| f.message == a)
            });
        if cfg.strict && strict_invalid {
            crate::reports::exit_invalid_for_analysis(&strict_blockers);
        }
        return Ok(());
    }

    // Non-TTY invocation with no report flag and no explicit --serve: rather than
    // silently starting a websocket server a pipe / CI can't use (the "something
    // different" §7 warns about), print a two-line hint pointing at the report
    // surfaces and exit cleanly. A TTY would have launched the TUI above; an
    // explicit --serve keeps the historical websocket behaviour untouched.
    if !stdout_is_tty && !cfg.serve {
        eprintln!(
            "hauksbee run: stdout is not a terminal, so there is no interactive dashboard to show."
        );
        eprintln!(
            "  For a report add a flag: --check (all static checks) · --report (what was modelled) · --check --plain (prose) · --json (machine); or --serve for the browser UI."
        );
        return Ok(());
    }

    // The preloaded live session must run the board AS BUILT: run the same
    // geometric DRC the report page shows and bridge its validated shorts into
    // the live engine (KiCad-10 `version_warning` shorts refused, reason
    // disclosed), exactly like the web co-sim. Without this the live sim
    // silently streamed idealised rails right under a report whose co-sim
    // block ran WITH the shorts bridged and said so. Skipped when
    // `--apply-shorts` already bridged and disclosed them above.
    if !cfg.apply_shorts {
        let drc_report = if is_altium {
            ExtractedBoard::altium_drc(&raw).unwrap_or_default()
        } else {
            ExtractedBoard::drc_with_clearance_rules(
                &text,
                crate::reports::kicad_pro_clearance_rules(&cfg.board, &board),
            )
            .unwrap_or_default()
        };
        let qualification = schematic_ties
            .as_ref()
            .map(|ties| ties.qualify(&drc_report));
        engine
            .apply_and_disclose_drc_shorts_with_qualification(&drc_report, qualification.as_ref());
    }

    // Serve the loaded board's own file at the URL the frontend fetches it from
    // (`/boards/<name>`), so the 2D/3D viewer renders the real geometry for any
    // board, not just the demo boards baked into dist/.
    let file_name = crate::commands::common::file_name(&cfg.board);
    let board_url = format!("/boards/{file_name}");

    // `run --serve` preloads the board, so the React app lands on THIS
    // board's report (the same JSON the drop path produces) and "run it" expands
    // it into the live sim already running on `/ws`. Compute the report once here
    // and hand it to the app via `/api/startup`. Board-only unless firmware was
    // supplied (then include the in-process co-sim, matching the drop path).
    // The analyzers take the board as raw bytes (so binary formats survive)
    // and normalize by file name, exactly like the drop path. Binary (Altium)
    // and Board-as-Code inputs hand over the file's own bytes: a `.board`
    // name with the recompiled KiCad text would be re-"compiled" as DSL and
    // fail. Plain text boards hand over the layout text.
    let report_bytes: &[u8] = if is_altium || is_board_code {
        &raw
    } else {
        text.as_bytes()
    };
    let report_json = match &cfg.firmware {
        Some(fw) => {
            let fw_name = crate::commands::common::file_name(fw);
            match std::fs::read(fw) {
                Ok(bytes) => crate::frontdoor::analyze_with_firmware_json_with_ties(
                    &file_name,
                    report_bytes,
                    &fw_name,
                    &bytes,
                    schematic_ties.as_ref(),
                ),
                // Firmware was already path-validated above; a read error here is
                // unexpected, so fall back to the board-only report rather than fail.
                Err(_) => crate::frontdoor::analyze_json_with_ties(
                    &file_name,
                    report_bytes,
                    schematic_ties.as_ref(),
                ),
            }
        }
        // The preloaded browser report must read the same as `--drc` on the same
        // path, so it gets the companion schematic this run already resolved.
        None => crate::frontdoor::analyze_json_with_ties(
            &file_name,
            report_bytes,
            schematic_ties.as_ref(),
        ),
    };
    let report_val: serde_json::Value =
        serde_json::from_str(&report_json).unwrap_or(serde_json::Value::Null);
    let startup_json = serde_json::json!({
        "preloaded": true,
        "board_name": file_name,
        "report": report_val,
        // This server can also launch a live session for a NEWLY uploaded
        // board (replacing the preloaded one), same as `hauksbee serve`.
        "live": true,
        // Engine version, for the Environment page's "what am I running" card.
        "version": env!("CARGO_PKG_VERSION"),
    })
    .to_string();

    crate::commands::common::serve(
        engine,
        cfg.port,
        Some((board_url, text)),
        startup_json,
        cfg.open,
        cfg.no_open,
    )
}

fn capture_manifest(
    cfg: &RunConfig,
    firmware_source: Option<&std::path::Path>,
    explicit_schematic: Option<&std::path::Path>,
    schematic_ties: Option<&crate::schematic_ties::SchematicTies>,
) -> anyhow::Result<crate::run_manifest::RunManifest> {
    use std::collections::BTreeMap;

    use crate::run_manifest::{
        absolutize_argv_paths, board_sidecar_inputs, implicit_model_inputs, ManifestInput,
        ManifestRequest, ToolIdentity,
    };

    let mut inputs = Vec::new();
    if cfg.example.is_none() {
        inputs.push(ManifestInput::new("board", &cfg.board));
        inputs.extend(board_sidecar_inputs(&cfg.board, "board"));
    }
    for (role, path) in [
        ("bom", cfg.bom.as_deref()),
        ("placement", cfg.placement.as_deref()),
        ("asbuilt", cfg.asbuilt.as_deref()),
        ("models_dir", cfg.models_dir.as_deref()),
    ] {
        if let Some(path) = path {
            inputs.push(ManifestInput::new(role, path));
        }
    }
    // The RESOLVED schematic, not just the explicit CLI option: an auto-discovered
    // sibling contributes exactly as much as a named one (it can move a short
    // from serious to a declared tie and flip the strict exit), so a manifest
    // that omitted it would not replay the run it describes. This also keeps the
    // manifest agreeing with the evidence inventory, which hashes the same file.
    //
    if let Some(ties) = schematic_ties {
        inputs.push(ManifestInput::retained_file(
            "schematic",
            &ties.path,
            ties.raw.clone(),
        ));
    }
    if let Some(path) = firmware_source {
        inputs.push(ManifestInput::new("firmware_source", path));
    }
    if let Some(path) = cfg.firmware.as_deref() {
        if firmware_source != Some(path) {
            inputs.push(ManifestInput::new("firmware_resolved", path));
        }
    }
    inputs.extend(implicit_model_inputs());

    let dnp_policy = match cfg.dnp_policy {
        hauksbee_extract::dnp::DnpPolicy::FitExceptLinks => "fit-except-links",
        hauksbee_extract::dnp::DnpPolicy::FitAll => "fit-all",
        hauksbee_extract::dnp::DnpPolicy::Honour => "honour",
    };
    let options = BTreeMap::from([
        ("ac".into(), serde_json::json!(cfg.ac)),
        ("ac_csv".into(), serde_json::json!(cfg.ac_csv)),
        ("ac_loop".into(), serde_json::json!(cfg.ac_loop)),
        ("ac_node".into(), serde_json::json!(cfg.ac_node)),
        ("ambient_c".into(), serde_json::json!(cfg.ambient)),
        ("ampacity".into(), serde_json::json!(cfg.ampacity)),
        ("apply_shorts".into(), serde_json::json!(cfg.apply_shorts)),
        ("bom_columns".into(), serde_json::json!(cfg.bom_columns)),
        ("check".into(), serde_json::json!(cfg.check)),
        ("chunk_us".into(), serde_json::json!(cfg.chunk_us)),
        ("dnp_policy".into(), serde_json::json!(dnp_policy)),
        ("drc".into(), serde_json::json!(cfg.drc)),
        ("example".into(), serde_json::json!(cfg.example)),
        ("fit".into(), serde_json::json!(cfg.fit)),
        ("headless".into(), serde_json::json!(cfg.headless)),
        ("json".into(), serde_json::json!(cfg.json)),
        ("junit".into(), serde_json::json!(cfg.junit)),
        ("lint".into(), serde_json::json!(cfg.lint)),
        ("list_nets".into(), serde_json::json!(cfg.list_nets)),
        ("no_fit".into(), serde_json::json!(cfg.no_fit)),
        ("no_open".into(), serde_json::json!(cfg.no_open)),
        ("open".into(), serde_json::json!(cfg.open)),
        ("oracle".into(), serde_json::json!(cfg.oracle)),
        ("plain".into(), serde_json::json!(cfg.plain)),
        ("port".into(), serde_json::json!(cfg.port)),
        ("probe".into(), serde_json::json!(cfg.probe)),
        ("probe_csv".into(), serde_json::json!(cfg.probe_csv)),
        ("report".into(), serde_json::json!(cfg.report)),
        ("resources".into(), serde_json::json!(cfg.resources)),
        ("sarif".into(), serde_json::json!(cfg.sarif)),
        ("seconds".into(), serde_json::json!(cfg.seconds)),
        ("serial_attach".into(), serde_json::json!(cfg.serial_attach)),
        ("serial_mcu".into(), serde_json::json!(cfg.serial_mcu)),
        (
            "serial_no_pace".into(),
            serde_json::json!(cfg.serial_no_pace),
        ),
        (
            "serial_transport".into(),
            serde_json::json!(cfg.serial_transport.as_str()),
        ),
        ("serial_wait_s".into(), serde_json::json!(cfg.serial_wait)),
        ("serve".into(), serde_json::json!(cfg.serve)),
        ("si".into(), serde_json::json!(cfg.si)),
        ("strict".into(), serde_json::json!(cfg.strict)),
        ("strict_boot".into(), serde_json::json!(cfg.strict_boot)),
        (
            "strict_thermal".into(),
            serde_json::json!(cfg.strict_thermal),
        ),
        (
            "no_strict_thermal".into(),
            serde_json::json!(cfg.no_strict_thermal),
        ),
        ("thermal".into(), serde_json::json!(cfg.thermal)),
        ("tui".into(), serde_json::json!(cfg.tui)),
        ("usb_c".into(), serde_json::json!(cfg.usb_c)),
        ("verbose".into(), serde_json::json!(cfg.verbose)),
    ]);
    let mut features = Vec::new();
    if cfg!(feature = "avr") {
        features.push("avr".to_string());
    }
    if cfg!(feature = "embed-web") {
        features.push("embed-web".to_string());
    }
    if cfg!(feature = "qemu") {
        features.push("qemu".to_string());
    }
    if cfg!(feature = "renode") {
        features.push("renode".to_string());
    }
    let replay_paths = [
        cfg.example.is_none().then(|| cfg.board.clone()),
        cfg.bom.clone(),
        cfg.placement.clone(),
        explicit_schematic.map(std::path::Path::to_path_buf),
        firmware_source.map(std::path::Path::to_path_buf),
        cfg.firmware.clone(),
        cfg.asbuilt.clone(),
        cfg.junit.clone(),
        cfg.sarif.clone(),
        cfg.models_dir.clone(),
        cfg.ac_csv.clone(),
        cfg.probe_csv.clone(),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    let base = std::env::current_dir()?;
    crate::run_manifest::RunManifest::capture(ManifestRequest {
        tool: ToolIdentity::workspace("hauksbee"),
        command: absolutize_argv_paths(cfg.manifest_command.clone(), &base, &replay_paths),
        options,
        inputs,
        feature_flags: features,
    })
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
fn list_nets_json(nets: &[String]) -> String {
    serde_json::to_string(nets).unwrap_or_else(|_| "[]".into())
}

/// The single gate every chatty informational note routes through, so `--quiet`
/// (and JSON / non-TTY suppression) is honoured uniformly and future notes
/// inherit the behaviour by going through here instead of a bare `eprintln!`.
#[derive(Clone, Copy)]
struct Notes {
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

fn warn_sibling_boards(board: &std::path::Path, notes: Notes) {
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

fn timing_refusal(reasons: &[String]) -> Refusal {
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
