//! Loads and normalizes board inputs before surface dispatch. This phase applies
//! manufacturing identity and DNP policy, validates flag combinations and
//! overlays, resolves companion schematics, emits manifests, and prepares the
//! static evidence findings shared by later report and simulation paths.

use hauksbee_ir::{eprintln, println};
use std::path::Path;

use hauksbee_extract::ExtractedBoard;
use hauksbee_models::ModelLibrary;

use crate::binder::{bind_board, BoundBoard};
use crate::board_input::InputKind;
use crate::result::{JsonFinding, JsonInputEvidence, Refusal, EXIT_INVALID_FOR_ANALYSIS};
use crate::schematic_ties::SchematicTies;

use super::manifest::capture_manifest;
use super::{
    ci_check_selected, input_kind_name, valid_digest, warn_sibling_boards, Notes, RunConfig,
    SelectedSurface,
};

pub struct RunInputs {
    pub(crate) board: ExtractedBoard,
    pub(crate) raw: Vec<u8>,
    pub(crate) text: String,
    pub(crate) input_kind: InputKind,
    pub(crate) is_altium: bool,
    pub(crate) reader_notes: Vec<String>,
    pub(crate) lib: ModelLibrary,
    pub(crate) inputs: Vec<JsonInputEvidence>,
}

pub struct RunArtifacts {
    pub(crate) prebound: Option<BoundBoard>,
    /// The design board with the as-built overlay's static-representable
    /// entries applied, for the static checks; `None` without `--asbuilt`.
    pub(crate) asbuilt_board: Option<ExtractedBoard>,
    pub(crate) schematic_ties: Option<SchematicTies>,
    pub(crate) ci_findings: Option<Vec<JsonFinding>>,
    /// The static check a combined run printed ahead of its co-sim, whose
    /// closing line and `--strict` exits are left for the co-sim to close.
    pub(crate) static_outcome: Option<crate::reports::check::CheckOutcome>,
}

/// One manufacturing-input row of the run's input inventory: what the artifact
/// contributed, what it could not, and the identity lines it settled.
fn artifact_evidence(
    kind: &str,
    path: &str,
    format: &str,
    sha256: &str,
    contributed: &[hauksbee_extract::bom::Contribution],
    ignored: &[hauksbee_extract::bom::IgnoredInput],
    identity: Vec<String>,
) -> JsonInputEvidence {
    JsonInputEvidence {
        path: path.to_string(),
        kind: kind.to_string(),
        format: format.to_string(),
        sha256: valid_digest(sha256),
        contributed: contributed
            .iter()
            .map(|item| format!("{}: {}", item.what, item.detail))
            .collect(),
        ignored: ignored
            .iter()
            .map(|item| format!("{}: {}", item.what, item.why))
            .collect(),
        identity,
    }
}

/// The flag-consistency contract: a flag that names an output artifact, or an
/// analysis input whose producing analysis was not requested, is an ERROR, because honouring it silently means
/// a file that never appears or a selector that selects nothing. A flag that
/// merely loses a rendering/serving preference to a higher-precedence one WARNS
/// on stderr and continues. One policy, no third category: nothing the user
/// asked for is ever dropped without a word.
fn validate_flag_consistency(cfg: &RunConfig, any_report_flag: bool) -> anyhow::Result<()> {
    if cfg.probe_csv.is_some() && cfg.probe.is_empty() {
        anyhow::bail!(
            "--probe-csv names an output file, but no --probe net was given; \
             add --probe <NET> (and --headless)"
        );
    }
    if let Some(vcd) = cfg.vcd.as_deref() {
        let inputs = [
            ("the board file", Some(cfg.board.as_path())),
            ("--bom", cfg.bom.as_deref()),
            ("--placement", cfg.placement.as_deref()),
            ("--firmware", cfg.firmware.as_deref()),
            ("--asbuilt", cfg.asbuilt.as_deref()),
        ];
        for (what, input) in inputs {
            if input.is_some_and(|input| crate::reports::ci_artifacts::paths_alias(input, vcd)) {
                anyhow::bail!(
                    "--vcd output must not overwrite {what} '{}'; choose a different path",
                    vcd.display()
                );
            }
        }
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
    if cfg.ampacity && cfg.json {
        anyhow::bail!(
            "--ampacity has no --json form yet; \
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
    // Same for --emit-netlist. With --list-nets alongside it, the net list is
    // what prints: it is checked first, and saying so names which of the two
    // the user is about to get.
    if cfg.emit_netlist && cfg.list_nets {
        eprintln!("warning: --list-nets prints first, so --emit-netlist is ignored here");
    } else if cfg.emit_netlist && any_report_flag {
        eprintln!(
            "warning: --emit-netlist prints the netlist and exits, so the report flag \
             is ignored here"
        );
    }
    // Same for --tui: an explicit report flag prints and exits, so the
    // dashboard never launches.
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
    Ok(())
}

/// Evaluate the selected surface for the `--junit` / `--sarif` artifacts, with
/// the same waiver and gate policy that surface renders. Findings stay in the
/// transaction until the final outcome commits them; co-sim appends its dynamic
/// findings later.
#[allow(clippy::too_many_arguments)]
fn gather_ci_artifact_findings(
    cfg: &RunConfig,
    surface: SelectedSurface,
    board: &ExtractedBoard,
    text: &str,
    raw: &[u8],
    input_kind: InputKind,
    is_altium: bool,
    lib: &ModelLibrary,
    reader_notes: &[String],
    schematic_ties: Option<&SchematicTies>,
) -> anyhow::Result<Vec<JsonFinding>> {
    let mut findings = crate::reports::check::gather_findings_with_schematic(
        &cfg.board,
        board,
        text,
        raw,
        is_altium,
        lib,
        schematic_ties,
        cfg.firmware.as_deref(),
    )?;
    findings.retain(|finding| ci_check_selected(surface, finding));
    let bound = bind_board(board, lib);
    let evidence = crate::evidence::board_evidence(
        board,
        &bound.report,
        reader_notes,
        &cfg.board,
        raw,
        input_kind,
    )?;
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
    // A verdict that is not a pass (unbound key parts, a partly judged board) is
    // added at the artifact writers from the run's machine report, which is the
    // one place that words it (see `ci_artifacts::note_verdict`).
    crate::reports::ci_artifacts::set_current_findings(findings.clone());
    Ok(findings)
}

#[allow(clippy::too_many_lines, reason = "linear input-preparation pipeline")]
pub fn prepare_run_inputs(
    cfg: &mut RunConfig,
    quiet: bool,
    surface: SelectedSurface,
    schematic: Option<&Path>,
    any_report_flag: bool,
) -> anyhow::Result<(RunInputs, RunArtifacts)> {
    // Validate `--firmware` up front, before any heavy work or the TUI takes over
    // the terminal. The native emulator loaders segfault (exit 139) on a missing
    // file instead of erroring; this turns a one-character typo into a clean,
    // actionable message naming the absolute path that was tried.
    //
    // Captured before that resolution rewrites it, because the manifest records
    // the firmware the run was ASKED for: a PlatformIO project directory that
    // resolves to a built .elf must replay as the project, not as one artifact
    // of a build that may no longer exist.
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
        let fw = cfg.firmware.as_deref().expect("just set");
        hauksbee_mcu::validate_firmware_path(fw)?;
        // Before the board analysis, which can take tens of seconds: a format
        // no backend loads must not wait for the bind to be refused.
        hauksbee_mcu::validate_firmware_format(fw)?;
    }
    // Advisory: if this board sits among sibling .kicad_pcb files (a multi-board
    // product), say so, a clean verdict on one file is misleading if the user
    // meant the whole thing. Routed through `Notes` so it stays on stderr and is
    // silenced under --quiet / --json / a piped stdout (it is helpful exactly once
    // for an interactive user, and pure noise in report and pipeline output).
    let notes = Notes::new(quiet, cfg.json);
    warn_sibling_boards(&cfg.board, notes);
    // Every input format (gerber dir/zip, binary Altium, KiCad
    // schematic hierarchy, the text layout/netlist formats) routes through the
    // ONE normalizer shared with the web front door and hauksbee-ci, including
    // the file-stem name fallback for titleless boards. `raw` keeps the file's
    // exact bytes (the Altium DRC twin reads copper from them) and `text` is
    // the KiCad-parseable layout text (empty for Altium/gerber, which have
    // none; those checks get the bytes twin or an honest "not checked").
    let norm = crate::board_input::from_path(&cfg.board)?;
    hauksbee_ir::progress::learned(norm.board.components.len(), norm.board.nets.len());
    hauksbee_ir::progress::learned_layout(norm.layout_text.as_deref());
    hauksbee_ir::progress::begin(hauksbee_ir::progress::Stage::Models, 0);
    let input_kind = norm.kind;
    let input_notes = norm.notes.clone();
    let is_altium = norm.is_binary();
    let reader_notes = norm.notes;
    let raw = norm.raw;
    let text = norm.layout_text.unwrap_or_default();
    let mut board = norm.board;
    // Companion identity belongs to the design files, before BOM/PnP
    // enrichment fills or normalizes values for binding. Keep that immutable
    // identity snapshot so a correct .brd/.sch pair cannot be rejected merely
    // because a later manufacturing input supplied a missing value.
    let design_identity_board = board.clone();
    // Layered model library: builtin < ~/.hauksbee/models (datasheet-extracted)
    // < ~/.config/hauksbee/models (user) < --models-dir (highest). Identity
    // reconciliation needs the same library the subsequent bind uses, so build
    // it before BOM/PnP are applied rather than resolving them under a smaller
    // universe of models.
    let extra: Vec<&std::path::Path> = cfg.models_dir.as_deref().into_iter().collect();
    let mut lib = ModelLibrary::builtin_with_user_dirs(&extra);
    // --spice-models sits above every directory layer: a vendor's own `.model`
    // card is the most authoritative statement about a part there is, which is
    // what the spice layer has always meant. It was previously reachable only
    // from the library API, so a vendor `.lib` beside a board could not become
    // a binding no matter where it was put.
    for path in &cfg.spice_models {
        lib.load_spice_models(path)?;
    }
    let lib = lib;

    let mut inputs = vec![JsonInputEvidence {
        path: cfg.board.display().to_string(),
        kind: "board".to_string(),
        format: input_kind_name(input_kind).to_string(),
        // The same digest the evidence inventory records for these bytes, so
        // the two provenance blocks describe the file identically.
        sha256: (!raw.is_empty()).then(|| crate::run_manifest::hex_digest(&raw)),
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
        let p = &artifact.provenance;
        inputs.push(artifact_evidence(
            "bom",
            &p.path,
            &p.kind,
            &p.sha256,
            &p.contributed,
            &p.ignored,
            identity.lines(),
        ));
    }
    if let Some(path) = &cfg.placement {
        let artifact = hauksbee_extract::placement::PlacementFile::read(path)?;
        let identity = crate::binder::apply_placement_identity(&mut board, &artifact, &lib)?;
        let p = &artifact.provenance;
        inputs.push(artifact_evidence(
            "placement",
            &p.path,
            &p.kind,
            &p.sha256,
            &p.contributed,
            &p.ignored,
            identity.lines(),
        ));
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
    // empty board is exactly the false comfort this tool exists to prevent.
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
                serde_json::json!({ "ok": false, "verdict": "invalid", "refusal": refusal })
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
    if !cfg.probe.is_empty()
        && !matches!(
            surface,
            SelectedSurface::Headless | SelectedSurface::CheckThenCosim
        )
    {
        anyhow::bail!("--probe records co-sim waveforms and needs --headless");
    }

    validate_flag_consistency(cfg, any_report_flag)?;
    super::ignored_flags::warn(cfg, surface);

    // --asbuilt describes the physical board, so it is validated and applied on
    // EVERY path, not only the simulating one (the static report branches used
    // to silently discard it). A bad path, a parse error, or an overlay that
    // does not describe this board is a hard error everywhere. The bound board
    // it produced is reused by the circuit-reading branches below (--ac,
    // --list-nets, co-sim, --serve).
    let (prebound, asbuilt_board) = if let Some(asbuilt_path) = &cfg.asbuilt {
        let overlay = crate::asbuilt::AsBuiltOverlay::load(asbuilt_path)?;
        let mut b = bind_board(&board, &lib);
        let overlay_report = overlay.apply(&mut b)?;
        // The same overlay on the extraction, so the static checks (lint,
        // envelope, bind, SI...) judge the reworked board too.
        let mut static_board = board.clone();
        let static_report = overlay.apply_to_board(&mut static_board, &lib)?;
        // Under --json stdout must stay one machine document, so the applied
        // narration is suppressed there (the validation above still ran).
        if !quiet && !cfg.json {
            println!("as-built overlay {} applied:", asbuilt_path.display());
            for line in &overlay_report.lines {
                println!("  {line}");
            }
            println!("static checks read the as-built board:");
            for line in &static_report.lines {
                println!("  {line}");
            }
            println!(
                "  note: the copper (DRC) checks read the DESIGN files; the overlay is \
                 applied to the simulated circuit (co-sim, --ac, --serve) and to the \
                 connectivity/model checks."
            );
        }
        (Some(b), Some(static_board))
    } else {
        (None, None)
    };

    // The companion Eagle `.sch`, if the user named one or one sits beside the
    // board. Resolved ONCE here, where both the explicit option and board path
    // are in hand, and handed to every report surface: a copper contact the
    // schematic declares must read the same way under `--check`, `--drc`,
    // `--json` and the CI artifacts, or the same schematic context differs on one
    // surface and a serious short on another.
    // Same `<eagle>` head sniff the DRC dispatch uses, so "is this an Eagle board"
    // is decided identically in both places.
    let board_is_eagle = text
        .chars()
        .take(512)
        .collect::<String>()
        .contains("<eagle");
    let schematic_ties = crate::schematic_ties::resolve(
        &cfg.board,
        &design_identity_board,
        schematic,
        board_is_eagle,
    )?;
    if let Some(ties) = &schematic_ties {
        let sha256 = crate::run_manifest::hex_digest(&ties.raw);
        inputs.push(JsonInputEvidence {
            path: ties.path.display().to_string(),
            kind: "schematic".to_string(),
            format: "eagle_schematic".to_string(),
            sha256: Some(sha256),
            contributed: vec![format!(
                "{} declared net tie(s) supplied as copper-contact context",
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
            cfg,
            firmware_source.as_deref(),
            schematic,
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
    // A run that records its outcome (--emit-manifest, or a `reproduce` replay)
    // needs the same gating findings to name what failed; a failure to gather
    // them there leaves the outcome without finding ids rather than failing the run.
    let wants_artifacts = cfg.junit.is_some() || cfg.sarif.is_some();
    let ci_findings = if wants_artifacts || crate::run_manifest::tracking_outcome() {
        let gathered = gather_ci_artifact_findings(
            cfg,
            surface,
            &board,
            &text,
            &raw,
            input_kind,
            is_altium,
            &lib,
            &reader_notes,
            schematic_ties.as_ref(),
        );
        if wants_artifacts {
            Some(gathered?)
        } else {
            gathered.ok()
        }
    } else {
        None
    };

    // --serial-attach bridges a host serial port to the firmware's UART, so
    // without firmware there is nothing on the far end to answer. Refuse here,
    // before any binding work, rather than open a port onto silence.
    if surface == SelectedSurface::Serial && cfg.firmware.is_none() {
        anyhow::bail!(
            "--serial-attach connects your own software to the emulated MCU's UART, so it \
             needs firmware to talk to: add --firmware <FILE>"
        );
    }

    Ok((
        RunInputs {
            board,
            raw,
            text,
            input_kind,
            is_altium,
            reader_notes,
            lib,
            inputs,
        },
        RunArtifacts {
            prebound,
            asbuilt_board,
            schematic_ties,
            ci_findings,
            static_outcome: None,
        },
    ))
}
