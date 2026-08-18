//! Loads and normalizes board inputs before surface dispatch. This phase applies
//! manufacturing identity and DNP policy, validates flag combinations and
//! overlays, resolves companion schematics, emits manifests, and prepares the
//! static evidence findings shared by later report and simulation paths.

use std::path::Path;

use hauksbee_extract::ExtractedBoard;
use hauksbee_models::ModelLibrary;

use crate::binder::{bind_board, BoundBoard};
use crate::board_input::InputKind;
use crate::result::{JsonFinding, JsonInputEvidence, Refusal, EXIT_INVALID_FOR_ANALYSIS};
use crate::schematic_ties::SchematicTies;

use super::{
    ci_check_selected, ci_surface_is_model_dependent, input_kind_name, manifest::capture_manifest,
    valid_digest, warn_sibling_boards, Notes, RunConfig, SelectedSurface,
};

pub(crate) struct RunInputs {
    pub(crate) board: ExtractedBoard,
    pub(crate) raw: Vec<u8>,
    pub(crate) text: String,
    pub(crate) input_kind: InputKind,
    pub(crate) is_altium: bool,
    pub(crate) is_board_code: bool,
    pub(crate) reader_notes: Vec<String>,
    pub(crate) lib: ModelLibrary,
    pub(crate) inputs: Vec<JsonInputEvidence>,
}

pub(crate) struct RunArtifacts {
    pub(crate) prebound: Option<BoundBoard>,
    pub(crate) schematic_ties: Option<SchematicTies>,
    pub(crate) ci_findings: Option<Vec<JsonFinding>>,
}

pub(crate) fn prepare_run_inputs(
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
            &cfg,
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
    let mut ci_findings: Option<Vec<crate::result::JsonFinding>> = None;
    if cfg.junit.is_some() || cfg.sarif.is_some() {
        let mut findings = crate::reports::check::gather_findings_with_schematic(
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

    Ok((
        RunInputs {
            board,
            raw,
            text,
            input_kind,
            is_altium,
            is_board_code,
            reader_notes,
            lib,
            inputs,
        },
        RunArtifacts {
            prebound,
            schematic_ties,
            ci_findings,
        },
    ))
}
