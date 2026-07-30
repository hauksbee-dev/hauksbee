//! The `hauksbee models` subcommand family: `lint` (standalone validation of
//! model / sensor TOML), `add`/`remove`/`list` (installed model packs,
//! 06-extensibility-sdk §3), and `resolve` (the per-component
//! which-entry-won-from-which-layer report, the pack author's debugging
//! surface).

use std::path::{Path, PathBuf};

use hauksbee_models::{ModelLibrary, Pack, PackStore};

/// `hauksbee models lint <file>`: standalone validation of model TOML.
///
/// Dispatches on the file's root shape: a `[sensor]` table lints as a
/// register-map sensor spec (`SensorSpec::from_toml`, the validation the
/// engine interpreter applies); anything with `[[models]]` entries lints each
/// entry's kind-specific params (`hauksbee_models::validate`) and, when a
/// `[models.logic]` block is present, COMPILES it through the same
/// `LogicComponent::compile` path binding uses, schema validation, expression
/// lowering, and the exhaustive comb-cycle convergence check, so "lint said
/// ok" and "the board binds it" can never disagree.
pub fn lint(file: &Path) -> anyhow::Result<()> {
    let text = std::fs::read_to_string(file)
        .map_err(|e| anyhow::anyhow!("reading '{}': {e}", file.display()))?;
    let root: toml::Value = toml::from_str(&text)
        .map_err(|e| anyhow::anyhow!("'{}' is not TOML: {e}", file.display()))?;

    let mut findings = 0usize;
    let mut checked = 0usize;

    if root.get("sensor").is_some() {
        checked += 1;
        match hauksbee_models::SensorSpec::from_toml(&text) {
            Ok(spec) => println!("sensor '{}': ok", spec.sensor().name),
            Err(e) => {
                findings += 1;
                println!("sensor spec: ERROR: {e}");
            }
        }
    } else if root.get("models").is_some() {
        let db: hauksbee_models::schema::DbFile = toml::from_str(&text)
            .map_err(|e| anyhow::anyhow!("'{}': [[models]] parse error: {e}", file.display()))?;
        for entry in &db.models {
            checked += 1;
            let mut entry_findings = 0usize;
            if let Err(errors) = hauksbee_models::validation::validate(entry) {
                for err in errors {
                    entry_findings += 1;
                    println!("model '{}': ERROR: {err}", entry.id);
                }
            }
            if !entry.logic.is_empty() {
                match crate::logic::LogicComponent::compile(&entry.id, &entry.logic) {
                    Ok(compiled) => {
                        for w in &compiled.warnings {
                            println!("model '{}' [models.logic]: warning: {w}", entry.id);
                        }
                    }
                    Err(e) => {
                        entry_findings += 1;
                        println!("model '{}' [models.logic]: ERROR: {e}", entry.id);
                    }
                }
            }
            // The behavioural block's own validation (converter/pin/FSM finiteness
            // gates) must run here too, or `hauksbee models lint` prints "ok" for a
            // model that panics the solver at sim time (e.g. vout_setpoint = nan).
            if !entry.behavioral.is_empty() {
                for err in hauksbee_models::behavioral::validate_behavioral(&entry.behavioral) {
                    entry_findings += 1;
                    println!("model '{}' [models.behavioral]: ERROR: {err}", entry.id);
                }
            }
            if entry_findings == 0 {
                println!("model '{}': ok", entry.id);
            }
            findings += entry_findings;
        }
    } else {
        anyhow::bail!(
            "'{}' has neither a [sensor] table nor [[models]] entries; nothing to lint",
            file.display()
        );
    }

    println!(
        "{checked} item(s) checked, {findings} finding(s){}",
        if findings == 0 { ": clean" } else { "" }
    );
    if findings > 0 {
        std::process::exit(2);
    }
    Ok(())
}

// ── Model packs (06-extensibility-sdk §3) ─────────────────────────────────────

fn default_store() -> anyhow::Result<PackStore> {
    PackStore::default_location()
        .ok_or_else(|| anyhow::anyhow!("HOME is not set; cannot locate ~/.hauksbee/packs"))
}

/// `hauksbee models add <path|url>`: validate a pack and install it into
/// `~/.hauksbee/packs/<name>@<version>/`, recording it in
/// `~/.hauksbee/packs.toml`.
///
/// Accepted sources:
///   - a local pack directory;
///   - a local `.tar.gz`/`.tgz`/`.tar` archive (unpacked with the system
///     `tar`, then the directory containing `pack.toml` is installed);
///   - a git URL (`git@…`, `git://…`, `ssh://…`, `…​.git`, or any `https://…`),
///     shallow-cloned with the system `git`.
/// Plain `http://` URLs are refused: no HTTP client ships in hauksbee, and an
/// unencrypted model source is a bad default anyway, clone or download it
/// yourself and pass the path.
pub fn add(source: &str) -> anyhow::Result<()> {
    let store = default_store()?;

    // Keep any temp dir alive until the install (which copies) finishes.
    let _staging: Option<tempfile::TempDir>;
    let src_dir: PathBuf = if source.starts_with("http://") {
        anyhow::bail!(
            "plain-http URLs are not supported: pass an https git URL, or download \
             the tarball yourself and `hauksbee models add <path>`"
        );
    } else if source.contains("://") || source.starts_with("git@") {
        let tmp = tempfile::tempdir()?;
        let dest = tmp.path().join("pack");
        run_tool(
            "git",
            &["clone", "--depth", "1", source, dest.to_str().unwrap()],
            "cloning the pack repo",
        )?;
        let dir = find_pack_root(&dest)?;
        _staging = Some(tmp);
        dir
    } else {
        let path = PathBuf::from(source);
        let is_tar = ["tar.gz", "tgz", "tar"]
            .iter()
            .any(|ext| source.ends_with(&format!(".{ext}")));
        if path.is_file() && is_tar {
            let tmp = tempfile::tempdir()?;
            run_tool(
                "tar",
                &[
                    "-xf",
                    path.to_str().unwrap(),
                    "-C",
                    tmp.path().to_str().unwrap(),
                ],
                "unpacking the pack tarball",
            )?;
            let dir = find_pack_root(tmp.path())?;
            _staging = Some(tmp);
            dir
        } else {
            _staging = None;
            path
        }
    };

    // Engine-level lint on top of the pack's own validation: compile every
    // [models.logic] block through the same path binding uses, so `models add`
    // and `models lint` can never disagree about a pack file.
    let pack = Pack::load(&src_dir)?;
    for file in &pack.model_files {
        let text = std::fs::read_to_string(file)?;
        let db: hauksbee_models::schema::DbFile = toml::from_str(&text)?;
        for entry in &db.models {
            if !entry.logic.is_empty() {
                crate::logic::LogicComponent::compile(&entry.id, &entry.logic).map_err(|e| {
                    anyhow::anyhow!(
                        "pack model file '{}', model '{}' [models.logic]: {e}",
                        file.display(),
                        entry.id
                    )
                })?;
            }
        }
    }

    let record = store.install(&src_dir, source)?;
    println!(
        "installed pack '{}@{}' ({} model file(s), provenance: {}) into {}",
        record.name,
        record.version,
        pack.model_files.len(),
        record.provenance,
        store.pack_dir(&record).display(),
    );
    println!("recorded in {}", store.record_path().display());
    Ok(())
}

/// `hauksbee models remove <name>`: delete an installed pack and its record.
pub fn remove(name: &str) -> anyhow::Result<()> {
    let store = default_store()?;
    let record = store.remove(name)?;
    println!("removed pack '{}@{}'", record.name, record.version);
    Ok(())
}

/// `hauksbee models list`: the installed packs, from `packs.toml`. With
/// `--builtin`, first the embedded MCU SoC descriptors; the `backend:part`
/// specs a board's `renode:<part>` / `qemu:<part>` backend string resolves to
/// when no override-dir descriptor shadows them.
pub fn list(builtin: bool) -> anyhow::Result<()> {
    if builtin {
        // Say up front that this list is NOT the set of co-simulatable MCUs.
        // Descriptors cover the external-emulator backends only; AVR parts
        // reach the in-process simavr core through routing entries and appear
        // nowhere here, so a bare descriptor list reads as "no AVR support",
        // the exact opposite of the truth.
        println!("built-in MCU SoC descriptors (backend:part):");
        for spec in hauksbee_mcu::SocConfig::builtin_specs() {
            println!("  {spec}");
        }
        println!(
            "\n  These are the descriptors for the EXTERNAL emulator backends (Renode,\n  \
             Espressif QEMU). They are not the whole list of co-simulatable parts:\n  \
             AVR (ATmega328P and the Arduino boards built on it) runs on the\n  \
             in-process simavr core, whose parts come from simavr's own database\n  \
             rather than a descriptor, so no AVR entry appears above. Run\n  \
             `hauksbee doctor` to see which backends this binary actually has, and\n  \
             `hauksbee models resolve <board>` to see what a given board binds to."
        );
        println!(
            "\n  To add a new MCU (so a board's part co-simulates exactly instead of on a\n  \
             substitute core): drop a <part>.soc.toml in $HAUKSBEE_MCU_DIR or\n  \
             ~/.config/hauksbee/mcu (it overrides the built-in of the same part), plus a\n  \
             [[models]] kind=\"mcu\" routing entry mapping your board's part value to it: two\n  \
             TOML files, no recompile. Copy the closest built-in above as a template; the\n  \
             full recipe is docs/extending/add-an-mcu-variant.md.\n  \
             For the model db as it applies to a board, use `hauksbee models resolve <board>`."
        );
        println!();
    }
    let store = default_store()?;
    let records = store.list()?;
    if records.is_empty() {
        println!("no packs installed (add one with `hauksbee models add <path|url>`)");
        return Ok(());
    }
    println!("installed packs ({}):", store.root().display());
    for r in &records {
        println!(
            "  {}@{}  license={}  provenance={}  source={}",
            r.name, r.version, r.license, r.provenance, r.source
        );
    }
    Ok(())
}

/// `hauksbee models resolve <board> [--json]`: per component, which model entry
/// won and from which priority layer: the pack author's debugging surface
/// (the layer-annotated extension of `run --report`'s bind table).
pub fn resolve(board_path: &Path, models_dir: Option<&Path>, json: bool) -> anyhow::Result<()> {
    // The shared board-input normalizer. A private mini board-code compile +
    // schematic dispatch here would leave `models resolve` accepting a
    // different format set than `run` (no Altium, no gerber, no zipped
    // .board). One normalizer, no drift.
    let board = crate::board_input::from_path(board_path)?.board;
    let extra: Vec<&Path> = models_dir.into_iter().collect();
    let lib = ModelLibrary::builtin_with_user_dirs(&extra);
    if json {
        println!("{}", resolve_report_json(&lib, &board));
    } else {
        print!("{}", resolve_report(&lib, &board));
    }
    Ok(())
}

/// One resolved (or unresolved) component row: the single source both the text
/// table and the JSON object render from, so they can never disagree.
struct ResolveRow {
    reference: String,
    value: String,
    model: String,
    layer: String,
    origin: String,
    resolved: bool,
}

fn resolve_rows(lib: &ModelLibrary, board: &hauksbee_extract::ExtractedBoard) -> Vec<ResolveRow> {
    board
        .components
        .iter()
        .map(|comp| {
            let res = crate::binder::resolve(lib, comp);
            let (model, layer, origin, resolved) = match (&res.model, res.layer) {
                (Some(m), Some(l)) => (
                    m.id.clone(),
                    l.to_string(),
                    res.origin.clone().unwrap_or_default(),
                    true,
                ),
                (Some(m), None) => (
                    m.id.clone(),
                    "engine-fallback".to_string(),
                    res.origin.clone().unwrap_or_default(),
                    true,
                ),
                _ => (
                    "UNRESOLVED".to_string(),
                    "-".to_string(),
                    "-".to_string(),
                    false,
                ),
            };
            ResolveRow {
                reference: comp.reference.clone(),
                value: comp.value.clone(),
                model,
                layer,
                origin,
                resolved,
            }
        })
        .collect()
}

/// The `models resolve` table, separated from I/O so tests can assert on it.
pub fn resolve_report(lib: &ModelLibrary, board: &hauksbee_extract::ExtractedBoard) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "layer priority: builtin(0) < pack(10) < user-dir(20) < models-dir(30) < spice(40); \
         specificity breaks ties within a layer"
    );
    let _ = writeln!(
        out,
        "{:<10} {:<24} {:<28} {:<16} {}",
        "Ref", "Value", "Model", "Layer", "Origin"
    );
    for row in resolve_rows(lib, board) {
        let _ = writeln!(
            out,
            "{:<10} {:<24} {:<28} {:<16} {}",
            row.reference, row.value, row.model, row.layer, row.origin
        );
    }
    out
}

/// The `models resolve --json` object: the same rows as the text table, plus a
/// resolved/unresolved rollup so a consumer needs no counting pass.
pub fn resolve_report_json(lib: &ModelLibrary, board: &hauksbee_extract::ExtractedBoard) -> String {
    let rows = resolve_rows(lib, board);
    let unresolved = rows.iter().filter(|r| !r.resolved).count();
    serde_json::json!({
        "ok": true,
        "components": rows
            .iter()
            .map(|r| {
                serde_json::json!({
                    "ref": r.reference,
                    "value": r.value,
                    "model": r.model,
                    "layer": r.layer,
                    "origin": r.origin,
                    "resolved": r.resolved,
                })
            })
            .collect::<Vec<_>>(),
        "total": rows.len(),
        "unresolved": unresolved,
    })
    .to_string()
}

/// Locate the directory holding `pack.toml` inside an unpacked tree: the root
/// itself, or exactly one direct child (the `tar xf` / `git clone` shape).
fn find_pack_root(dir: &Path) -> anyhow::Result<PathBuf> {
    if dir.join("pack.toml").is_file() {
        return Ok(dir.to_path_buf());
    }
    let candidates: Vec<PathBuf> = std::fs::read_dir(dir)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir() && p.join("pack.toml").is_file())
        .collect();
    match candidates.as_slice() {
        [one] => Ok(one.clone()),
        [] => anyhow::bail!(
            "no pack.toml found in '{}' (or one level below)",
            dir.display()
        ),
        many => anyhow::bail!(
            "'{}' contains {} directories with a pack.toml; pass the pack directory itself",
            dir.display(),
            many.len()
        ),
    }
}

/// Run an external tool (git / tar), failing loud with its stderr.
fn run_tool(tool: &str, args: &[&str], doing: &str) -> anyhow::Result<()> {
    let out = std::process::Command::new(tool)
        .args(args)
        .output()
        .map_err(|e| anyhow::anyhow!("{doing}: cannot run `{tool}`: {e}"))?;
    if !out.status.success() {
        anyhow::bail!(
            "{doing}: `{tool} {}` failed:\n{}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}

/// `hauksbee models extract`: draft a device model from a PDF datasheet.
///
/// The consent gate is the point of this wrapper. The extractor sends the
/// datasheet's text to an LLM backend, so the command states that and stops
/// unless the caller has said yes, either interactively or with `--yes` for a
/// script. Running the extraction and mentioning the fact afterwards would be
/// the wrong order: the user cannot unsend it.
pub fn extract(
    pdf: &std::path::Path,
    part: &str,
    kind: &str,
    out_dir: Option<&std::path::Path>,
    assume_yes: bool,
) -> anyhow::Result<()> {
    use hauksbee_models::datasheet;

    if !pdf.is_file() {
        anyhow::bail!("no datasheet at '{}'", pdf.display());
    }

    println!("Extract a model for {part} ({kind}) from {}", pdf.display());
    println!();
    println!("{}", datasheet::CONSENT_NOTICE);
    println!();

    if !assume_yes {
        // A tty can answer. A pipe cannot, and guessing "yes" on its behalf
        // would send someone's datasheet because they ran the wrong command.
        if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
            anyhow::bail!(
                "refusing to send anything without consent. This is not a terminal, so \
                 there is nobody to ask: pass --yes if you meant it."
            );
        }
        print!("Send the datasheet and draft a model? [y/N] ");
        use std::io::Write;
        std::io::stdout().flush().ok();
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            println!("Nothing was sent.");
            return Ok(());
        }
    }

    let args = datasheet::Args::new(pdf.to_path_buf(), part.to_string(), kind.to_string())
        .out_dir(out_dir.map(std::path::Path::to_path_buf));
    datasheet::run(args)?;

    println!();
    println!(
        "This model is a draft with provenance \"datasheet-extracted\". Read it before you \
         trust a result that depends on it, and check any value the datasheet did not state \
         outright."
    );
    Ok(())
}
