//! The `hauksbee models` subcommand family: `lint` (standalone validation of
//! model / sensor TOML), `add`/`remove`/`list` (installed model packs,
//! 06-extensibility-sdk §3), and `resolve` (the per-component
//! which-entry-won-from-which-layer report, the pack author's debugging
//! surface).

use std::path::{Path, PathBuf};

use hauksbee_ir::evidence::{
    ModelLayer, ModelSource, ModelSourceTier, ModelUncertainty, ModelValidation,
};
use hauksbee_models::{ModelLibrary, Pack, PackStore};
use sha2::{Digest, Sha256};

/// `hauksbee models lint <file>`: standalone validation of model TOML.
///
/// Dispatches on the file's root shape: a `[sensor]` table lints as a
/// register-map sensor spec (`SensorSpec::from_toml`, the validation the
/// engine interpreter applies); anything with `[[models]]` entries lints each
/// entry's kind-specific params (`hauksbee_models::validate`) and, when a
/// `[models.logic]` block is present, COMPILES it through the same
/// `LogicComponent::compile` path binding uses, schema validation, expression
/// lowering, and the exhaustive comb-cycle convergence check, so "lint said
/// ok" and "the board binds it" can never disagree; a `[soc]` table lints as an
/// MCU descriptor through the same loader a co-simulation uses, plus the checks
/// the loader leaves to author intent, plus an inspection of what the descriptor
/// will actually do (see `lint_soc`).
pub fn lint(file: &Path) -> anyhow::Result<()> {
    // A board file handed to `models lint` used to fall into the TOML parser,
    // which dumped the whole one-line board file as error context. Detect it
    // (extension first: a binary .PcbDoc fails read_to_string with a UTF-8
    // error that hides the actual mistake) and name the command they meant.
    let ext = file
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    let board_ext = matches!(
        ext.as_str(),
        "kicad_pcb" | "kicad_sch" | "brd" | "pcbdoc" | "d356" | "net" | "board" | "zip"
    );
    if board_ext {
        anyhow::bail!(
            "'{}' is a board, not a model spec: to see which model entry each \
             board part resolves to, run  hauksbee models resolve {}",
            file.display(),
            file.display()
        );
    }
    let text = std::fs::read_to_string(file)
        .map_err(|e| anyhow::anyhow!("reading '{}': {e}", file.display()))?;
    let head = text.trim_start();
    if head.starts_with("(kicad_pcb")
        || head.starts_with("(kicad_sch")
        || head.starts_with("(export")
    {
        anyhow::bail!(
            "'{}' is a board, not a model spec: to see which model entry each \
             board part resolves to, run  hauksbee models resolve {}",
            file.display(),
            file.display()
        );
    }
    let root: toml::Value = toml::from_str(&text).map_err(|e| {
        anyhow::anyhow!(
            "'{}' is not TOML: {e}",
            file.display(),
            e = crate::commands::common::cap_context_width(&e.to_string())
        )
    })?;

    let mut findings = 0usize;
    let mut checked = 0usize;
    // Warnings are the suspected-mistake tier: they never gate the exit code,
    // but they must be counted. Printing a warning and then "ok" for the same
    // entry and ": clean" for the file makes the warning look like decoration,
    // which is how a lint gets ignored. (The `[soc]` `note:` advisories are a
    // different tier: they report a capability the descriptor leaves absent,
    // which is usually correct, and deliberately do not count.)
    let mut warnings = 0usize;

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
        let db: hauksbee_models::schema::DbFile = toml::from_str(&text).map_err(|e| {
            // An unknown `kind` is the classic first-model mistake; serde's
            // unknown-variant wording lists the vocabulary but a synonym
            // deserves its direct answer (ldo -> vreg).
            match hauksbee_models::validation::kind_error_note(&e.to_string()) {
                Some(note) => anyhow::anyhow!(
                    "'{}': [[models]] parse error: {e}\n  {note}",
                    file.display()
                ),
                None => anyhow::anyhow!("'{}': [[models]] parse error: {e}", file.display()),
            }
        })?;
        for entry in &db.models {
            checked += 1;
            let mut entry_findings = 0usize;
            let mut entry_warnings = 0usize;
            if let Err(errors) = hauksbee_models::validation::validate(entry) {
                for err in errors {
                    entry_findings += 1;
                    println!("model '{}': ERROR: {err}", entry.id);
                }
            }
            if entry.r#match.is_empty() {
                entry_findings += 1;
                println!(
                    "model '{}': ERROR: entry has no match rules (lib_id / value_re / \
                     footprint_re / mpn_re all absent); it cannot claim a board component",
                    entry.id
                );
            }
            // Parameter-name vocabulary (warning tier, no exit-code effect). The
            // params bag is free-form on purpose, so a misspelled key is not an
            // error, it is a key nothing reads: the entry validates and then
            // silently runs on the default. This is the only place that key is
            // ever mentioned, so it says so, and adds the nearest real name when
            // one is close enough to be worth naming. Unknown names can also be
            // genuine extensions, which is why it cannot gate.
            for u in hauksbee_models::param_names::unknown_params(entry) {
                entry_warnings += 1;
                println!(
                    "model '{}' [models.params]: warning: {}",
                    entry.id,
                    u.message()
                );
            }
            if !entry.logic.is_empty() {
                match crate::logic::LogicComponent::compile(&entry.id, &entry.logic) {
                    Ok(compiled) => {
                        for w in &compiled.warnings {
                            entry_warnings += 1;
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
                match entry_warnings {
                    0 => println!("model '{}': ok", entry.id),
                    n => println!("model '{}': ok, {n} warning(s) above", entry.id),
                }
            }
            findings += entry_findings;
            warnings += entry_warnings;
        }
    } else if root.get("soc").is_some() {
        checked += 1;
        findings += lint_soc(file, &text, &root);
    } else {
        anyhow::bail!(
            "'{}' has no [sensor] table, no [[models]] entries and no [soc] table; \
             nothing to lint",
            file.display()
        );
    }

    let warning_note = if warnings > 0 {
        format!(", {warnings} warning(s)")
    } else {
        String::new()
    };
    println!(
        "{checked} item(s) checked, {findings} finding(s){warning_note}{}",
        if findings == 0 && warnings == 0 {
            ": clean"
        } else {
            ""
        }
    );
    if findings > 0 {
        std::process::exit(2);
    }
    Ok(())
}

/// Validate and inspect a `[soc]` MCU descriptor, returning the finding count.
///
/// Two layers. First the REAL loader ([`hauksbee_mcu::SocConfig::from_soc_toml`]),
/// so a descriptor that lints clean is a descriptor a co-sim will accept: the
/// same discipline that makes `models lint` and board binding agree about a
/// `[models.logic]` block. The loader refuses everything that has no correct
/// execution (a zero clock, an unsubstitutable `{support}` token, a direction
/// encoding narrower than its port, a shadowed ADC channel, a scale factor that
/// cannot scale), so those arrive here as layer-one errors and are not re-checked.
///
/// Layer two is what the loader deliberately leaves alone: a descriptor that
/// executes, where whether it is right depends on what the author meant. Those
/// print as `ERROR` and count towards the exit code.
///
/// Then the inspection: one line per capability the resolved descriptor
/// configures, so an author reads their intent back instead of inferring it from
/// a co-sim result, and a set of `note:` advisories for the capabilities the
/// descriptor leaves absent. Advisories do NOT affect the exit code. A descriptor
/// with no ADC map or no bus controllers is usually exactly right, and warning
/// about it would train people to ignore the output.
///
/// A wrong `odr_offset` cannot be caught from a file at all. That is why the
/// walkthrough reads every offset off a running machine.
fn lint_soc(file: &Path, text: &str, root: &toml::Value) -> usize {
    let (lines, findings) = soc_lint_report(file, text, root);
    for line in lines {
        println!("{line}");
    }
    findings
}

/// The pure core of [`lint_soc`]: the lines it would print, and the finding
/// count. Separated from the I/O so the tests below can lint every embedded
/// descriptor and assert on the exact output instead of on a process exit code.
fn soc_lint_report(file: &Path, text: &str, root: &toml::Value) -> (Vec<String>, usize) {
    let mut out = Vec::new();
    // Layer 1: the loader, verbatim. Its errors already name the field.
    let config = match hauksbee_mcu::SocConfig::from_soc_toml(text) {
        Ok(c) => c,
        Err(e) => {
            out.push(format!("soc descriptor '{}': ERROR: {e}", file.display()));
            return (out, 1);
        }
    };
    let Some(soc) = root.get("soc").and_then(|v| v.as_table()) else {
        // `from_soc_toml` succeeded, so `[soc]` is a table; reported rather than
        // unwrapped so a future schema change cannot panic the linter.
        out.push(format!(
            "soc descriptor '{}': ERROR: [soc] is not a table",
            file.display()
        ));
        return (out, 1);
    };

    let mut findings = 0usize;

    // Layer 2, check one: a blank `mcu_label`. It reaches reports and
    // arch-mismatch errors rather than the emulator, so the loader runs it
    // correctly and the cost is a report that names nothing.
    let label_blank = soc
        .get("mcu_label")
        .and_then(|v| v.as_str())
        .is_some_and(|s| s.trim().is_empty());
    if label_blank {
        out.push(
            "soc descriptor: ERROR: `mcu_label` is empty; it is the part name every \
             report and arch-mismatch error prints"
                .to_string(),
        );
        findings += 1;
    }

    // Layer 2, check two: an ADC `monitor_command` with no substitution token.
    // It runs, and feeds the same constant every chunk, so injection appears to
    // work and never changes. The loader accepts it because a self-contained
    // trigger command is a thing an author can mean.
    if let Some(adc) = soc.get("adc").and_then(|v| v.as_array()) {
        for entry in adc {
            let Some(entry) = entry.as_table() else {
                continue;
            };
            let channel = entry
                .get("channel")
                .and_then(|v| v.as_integer())
                .unwrap_or(-1);
            let Some(cmd) = entry.get("monitor_command").and_then(|v| v.as_str()) else {
                continue;
            };
            if !["{count}", "{millivolts}", "{volts}"]
                .iter()
                .any(|t| cmd.contains(t))
            {
                out.push(format!(
                    "soc descriptor: ERROR: ADC channel {channel} monitor_command contains \
                     none of {{count}}, {{millivolts}} or {{volts}}, so it would feed the \
                     same value every chunk: {cmd:?}"
                ));
                findings += 1;
            }
        }
    }

    if findings == 0 {
        let part = file
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .trim_end_matches(".soc.toml");
        out.push(format!(
            "soc descriptor '{}': ok ({}:{part})",
            file.display(),
            config.backend().name()
        ));
        out.extend(
            soc_inspection(&config)
                .into_iter()
                .map(|l| format!("  {l}")),
        );
        out.extend(
            soc_advisories(&config)
                .into_iter()
                .map(|l| format!("  note: {l}")),
        );
    }
    (out, findings)
}

/// One line per capability a resolved descriptor configures: the read-back an
/// author uses to check the file says what they meant.
fn soc_inspection(config: &hauksbee_mcu::SocConfig) -> Vec<String> {
    let mut out = Vec::new();
    match config {
        #[cfg(feature = "renode")]
        hauksbee_mcu::SocConfig::Renode(c) => {
            out.push(format!("part: {} on machine {:?}", c.mcu_label, c.machine));
            out.push(if c.platform.contains('\n') {
                format!(
                    "platform: {} line(s) of inline .repl source",
                    c.platform.lines().count()
                )
            } else {
                format!("platform: {}", c.platform)
            });
            if let Some(b) = &c.support_bundle {
                out.push(format!(
                    "support bundle: {b} (unpacked before the platform loads)"
                ));
            }
            out.push(format!(
                "cpu: {}   clock: {} Hz (cross-checked at load against the platform's own \
                 `cpu PerformanceInMips` / `nvic systickFrequency` declarations)",
                c.cpu, c.frequency_hz
            ));
            out.push(match &c.watchdog_limitation {
                Some(l) => format!("watchdog: {l}"),
                None => "watchdog: this part claims full fidelity, an armed watchdog that is \
                         never fed reboots the core the way silicon does"
                    .to_string(),
            });
            out.push(match &c.timing_limitation {
                Some(l) => format!("timing: {l}"),
                None => "timing: this part claims a firmware delay costs the virtual time \
                         it costs on silicon (measured by the clock-truth gate)"
                    .to_string(),
            });
            out.push(match &c.uart {
                Some(u) => format!("uart bridge: {u}"),
                None => "uart bridge: none".to_string(),
            });
            for p in &c.ports {
                out.push(format!(
                    "gpio port {}: {} pins on sysbus.{}, output state at +{:#x}, direction {}",
                    p.letter,
                    p.width,
                    p.peripheral,
                    p.odr_offset,
                    match &p.dir {
                        Some(d) => format!("at +{:#x} ({:?})", d.offset, d.encoding),
                        None => "not observable".to_string(),
                    }
                ));
            }
            out.push(format!(
                "i2c controllers: {}",
                name_list(&c.i2c_controllers)
            ));
            out.push(format!(
                "spi controllers: {}",
                name_list(&c.spi_controllers)
            ));
            for a in &c.adc_channels {
                out.push(format!(
                    "adc channel {}: 0..{} counts over 0..{} V via {}",
                    a.channel,
                    a.max_count,
                    a.full_scale_volts,
                    match &a.inject {
                        hauksbee_mcu::AdcInject::MonitorCommand(cmd) => format!("monitor {cmd:?}"),
                        hauksbee_mcu::AdcInject::MemoryWord(addr) =>
                            format!("a write to {addr:#010x}"),
                    }
                ));
            }
            out.push(format!(
                "setup commands: {} before the firmware loads, {} after",
                c.extra_setup.len(),
                c.post_load_setup.len()
            ));
        }
        #[cfg(feature = "qemu")]
        hauksbee_mcu::SocConfig::Qemu(c) => {
            out.push(format!("part: {} on machine {:?}", c.mcu_label, c.machine));
            out.push(format!(
                "arch: {:?}   clock: {} Hz   icount_shift: {}",
                c.arch, c.frequency_hz, c.icount_shift
            ));
            for b in &c.banks {
                out.push(format!(
                    "gpio bank {}: {} pins, peripheral out {:#010x} + enable {:#010x} when capability-probed; firmware-mailbox fallback {:#010x}; input mailbox {:#010x}",
                    b.letter,
                    b.width,
                    b.peripheral_out_reg,
                    b.peripheral_enable_reg,
                    b.out_reg,
                    b.in_reg
                ));
            }
            out.push(format!(
                "gpio capability probe: {} properties gpio-out + gpio-enable",
                c.gpio_qom_path
            ));
            out.push(format!("i2c buses: {}", name_list(&c.i2c_buses)));
            // Unlike the Renode branch, the watchdog statement here is a
            // property of how the backend LAUNCHES QEMU (`wdt_disable=true` for
            // the timer groups) rather than of this file, so the descriptor has
            // no field to read back. Quote the backend's own constant rather
            // than restating it: this is the sentence a run reports verbatim on
            // its four batch surfaces, and a second copy would be a second
            // wording.
            out.push(format!(
                "watchdog: {} (stated per-backend rather than per-descriptor for this family)",
                hauksbee_mcu::qemu::WATCHDOG_LIMITATION
            ));
            // Same per-backend reasoning for timing: wall-clock pacing is how
            // the backend drives QEMU (no icount), not anything this file
            // declares, so quote the constant a run reports verbatim.
            out.push(format!(
                "timing: {} (stated per-backend rather than per-descriptor for this family)",
                hauksbee_mcu::qemu::TIMING_LIMITATION
            ));
        }
        #[cfg(not(any(feature = "renode", feature = "qemu")))]
        _ => out.push("this build carries no emulator backend".to_string()),
    }
    out
}

/// A comma-separated controller list, or the word `none`, so an empty list reads
/// as a deliberate answer rather than a truncated line.
#[cfg(any(feature = "qemu", feature = "renode"))]
fn name_list(names: &[String]) -> String {
    if names.is_empty() {
        "none".to_string()
    } else {
        names.join(", ")
    }
}

/// What a valid descriptor will NOT do. Each of these is a legitimate choice and
/// a common accident, so they are notes and not findings: the point is that the
/// author sees the consequence before a co-sim run reports it as a coverage hole.
fn soc_advisories(config: &hauksbee_mcu::SocConfig) -> Vec<String> {
    #[cfg(any(feature = "renode", feature = "qemu"))]
    let mut out = Vec::new();
    #[cfg(not(any(feature = "renode", feature = "qemu")))]
    let out = Vec::new();
    match config {
        #[cfg(feature = "renode")]
        hauksbee_mcu::SocConfig::Renode(c) => {
            if c.ports.is_empty() {
                out.push(
                    "no [[soc.ports]]: no GPIO is observable, so every net this MCU \
                     drives reports as never driven"
                        .to_string(),
                );
            }
            if c.uart.is_none() {
                out.push(
                    "no `uart`: firmware output and --serial-attach have nowhere to go".to_string(),
                );
            }
            if c.ports.iter().any(|p| p.dir.is_none()) {
                out.push(
                    "at least one port has no `dir` map, so drive direction is not \
                     observable there and every output-state change reports as a drive. \
                     That is the conservative answer and the right default; a WRONG dir \
                     map is worse than none"
                        .to_string(),
                );
            }
            if c.adc_channels.is_empty() {
                out.push(
                    "no [[soc.adc]]: analog injections into this MCU are DROPPED and \
                     reported as a coverage hole by `hauksbee run` (text, --plain, \
                     --json) and hauksbee-ci, never silently"
                        .to_string(),
                );
            }
            if c.i2c_controllers.is_empty() && c.spi_controllers.is_empty() {
                out.push(
                    "no i2c or spi controllers: a bound sensor is recorded UNEXERCISED \
                     and a CI peripheral assertion against it fails"
                        .to_string(),
                );
            }
        }
        #[cfg(feature = "qemu")]
        hauksbee_mcu::SocConfig::Qemu(c) => {
            if c.banks.is_empty() {
                out.push("no [[soc.banks]]: no GPIO is observable".to_string());
            }
            if c.i2c_buses.is_empty() {
                out.push(
                    "no [soc.i2c].buses: a bound sensor is recorded UNEXERCISED and a CI \
                     peripheral assertion against it fails"
                        .to_string(),
                );
            }
        }
        #[cfg(not(any(feature = "renode", feature = "qemu")))]
        _ => {}
    }
    out
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
        if !path.exists() {
            anyhow::bail!(
                "no pack at '{source}': pass a pack directory, a .tar.gz/.tgz/.tar \
                 tarball, or a git URL (https://…, git@…, ssh://…)"
            );
        }
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
        } else if path.is_file() {
            anyhow::bail!(
                "'{source}' is a file but not a tarball hauksbee can unpack: pass a \
                 pack directory, a .tar.gz/.tgz/.tar tarball, or a git URL \
                 (https://…, git@…, ssh://…)"
            );
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
             full recipe is {}.\n  \
             For the model db as it applies to a board, use `hauksbee models resolve <board>`.",
            hauksbee_ir::docs_url("docs/extending/add-an-mcu-variant.md")
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
    resolve_checked(board_path, models_dir, json, ModelRequirement::default())
}

/// Resolve and optionally refuse when selected sources do not meet an explicit
/// accuracy policy. A refusal is exit 3: the input parsed, but the requested
/// analysis accuracy cannot be vouched for.
pub fn resolve_checked(
    board_path: &Path,
    models_dir: Option<&Path>,
    json: bool,
    requirement: ModelRequirement,
) -> anyhow::Result<()> {
    // The shared board-input normalizer. A private mini board-code compile +
    // schematic dispatch here would leave `models resolve` accepting a
    // different format set than `run` (no Altium, no gerber, no zipped
    // .board). One normalizer, no drift.
    let board = crate::board_input::from_path(board_path)?.board;
    let extra: Vec<&Path> = models_dir.into_iter().collect();
    let lib = ModelLibrary::builtin_with_user_dirs(&extra);
    let refusals = model_requirement_refusals(&lib, &board, requirement);
    if json {
        let mut value: serde_json::Value =
            serde_json::from_str(&resolve_report_json(&lib, &board))?;
        if !refusals.is_empty() {
            value["ok"] = serde_json::Value::Bool(false);
            value["status"] = serde_json::Value::String("invalid_for_analysis".to_string());
            value["reason"] = serde_json::Value::String("model_accuracy_insufficient".to_string());
            value["refusals"] = serde_json::to_value(&refusals)?;
        }
        println!("{value}");
    } else {
        print!("{}", resolve_report(&lib, &board));
        for refusal in &refusals {
            eprintln!(
                "REFUSED {} ({}): {}",
                refusal.reference, refusal.model, refusal.reason
            );
        }
    }
    if !refusals.is_empty() {
        std::process::exit(crate::result::EXIT_INVALID_FOR_ANALYSIS);
    }
    Ok(())
}

/// Optional fail-closed policy for the model-resolution validation surface.
#[derive(Debug, Clone, Copy, Default)]
pub struct ModelRequirement {
    pub minimum_tier: Option<ModelSourceTier>,
    pub minimum_validation: Option<ModelValidation>,
    pub require_intervals: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ModelRequirementRefusal {
    pub reference: String,
    pub model: String,
    pub reason: String,
}

pub fn model_requirement_refusals(
    lib: &ModelLibrary,
    board: &hauksbee_extract::ExtractedBoard,
    requirement: ModelRequirement,
) -> Vec<ModelRequirementRefusal> {
    if requirement.minimum_tier.is_none()
        && requirement.minimum_validation.is_none()
        && !requirement.require_intervals
    {
        return Vec::new();
    }

    resolve_rows(lib, board)
        .into_iter()
        .filter_map(|row| {
            let reason = if !row.resolved {
                Some("component is explicitly open because no model resolved".to_string())
            } else if requirement
                .minimum_tier
                .is_some_and(|minimum| row.source.tier().priority() < minimum.priority())
            {
                Some(format!(
                    "source tier {} is below required {}",
                    row.source.tier(),
                    requirement.minimum_tier.expect("checked above")
                ))
            } else if requirement
                .minimum_validation
                .is_some_and(|minimum| row.source.validation().priority() < minimum.priority())
            {
                Some(format!(
                    "validation {} is below required {}",
                    row.source.validation(),
                    requirement.minimum_validation.expect("checked above")
                ))
            } else if requirement.require_intervals
                && row
                    .source
                    .uncertainty()
                    .iter()
                    .any(|value| !value.is_strict_bound())
            {
                Some(
                    "model has no validated two-sided specification or empirical error interval"
                        .to_string(),
                )
            } else {
                None
            }?;
            Some(ModelRequirementRefusal {
                reference: row.reference,
                model: row.model,
                reason,
            })
        })
        .collect()
}

/// One resolved (or unresolved) component row: the single source both the text
/// table and the JSON object render from, so they can never disagree.
struct ResolveRow {
    reference: String,
    value: String,
    model: String,
    layer: String,
    origin: String,
    source: ModelSource,
    resolved: bool,
}

fn open_source(reference: &str) -> ModelSource {
    ModelSource::new(
        ModelSourceTier::Open,
        ModelLayer::Unspecified,
        "unresolved",
        ModelValidation::Unvalidated,
        vec![ModelUncertainty::unknown(
            format!("{reference}.model"),
            "no model resolved; the component is explicitly open",
        )
        .expect("static uncertainty is valid")],
    )
    .expect("static open source is valid")
}

fn uncertainty_label(source: &ModelSource) -> String {
    if source
        .uncertainty()
        .iter()
        .any(|value| matches!(value, ModelUncertainty::Unknown { .. }))
    {
        "unknown".to_string()
    } else {
        source
            .uncertainty()
            .iter()
            .filter_map(ModelUncertainty::interval_kind)
            .map(|kind| kind.to_string())
            .collect::<Vec<_>>()
            .join(",")
    }
}

/// Group rank for the resolve report's reading order: what needs attention
/// first. UNRESOLVED leads, the engine's last-ditch fallback next, then the
/// real layers from most-user-supplied (spice, --models-dir) down to builtin.
fn layer_rank(layer: &str, resolved: bool) -> u8 {
    if !resolved {
        return 0;
    }
    match layer {
        "engine-fallback" => 1,
        "spice" => 2,
        "models-dir" => 3,
        "user-config-dir" => 4,
        "user-dir" => 5,
        "pack" => 6,
        "builtin" => 7,
        _ => 8,
    }
}

/// Natural sort key for a reference designator: alpha prefix, then the
/// trailing integer numerically (R2 before R10), then the raw string.
fn natural_ref_key(reference: &str) -> (String, u64, String) {
    let split = reference
        .find(|c: char| c.is_ascii_digit())
        .unwrap_or(reference.len());
    let (prefix, digits) = reference.split_at(split);
    (
        prefix.to_ascii_uppercase(),
        digits
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>()
            .parse()
            .unwrap_or(u64::MAX),
        reference.to_string(),
    )
}

fn resolve_rows(lib: &ModelLibrary, board: &hauksbee_extract::ExtractedBoard) -> Vec<ResolveRow> {
    let mut rows: Vec<ResolveRow> = board
        .components
        .iter()
        .map(|comp| {
            // The pack author's question is "which model entry would win for
            // this record", so this deliberately uses the library view: a DNP
            // part still shows the model it would get when fitted, and a
            // refused identity still shows UNRESOLVED. Nothing here binds.
            let res = crate::binder::library_resolution(lib, comp);
            let source = res
                .provenance
                .clone()
                .unwrap_or_else(|| open_source(&comp.reference));
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
                source,
                resolved,
            }
        })
        .collect();
    // Reading order, not board order: UNRESOLVED rows first (they are why
    // someone runs this command), then by layer group, natural reference
    // order within each group.
    rows.sort_by(|a, b| {
        layer_rank(&a.layer, a.resolved)
            .cmp(&layer_rank(&b.layer, b.resolved))
            .then_with(|| natural_ref_key(&a.reference).cmp(&natural_ref_key(&b.reference)))
    });
    rows
}

/// Render a Unicode box-drawing table from a header row and data rows, the
/// same style as the bind table in `report.rs`. Column widths fit the widest
/// cell; a row shorter than the header renders its missing cells empty.
pub fn box_table(headers: &[&str], rows: &[Vec<String>]) -> String {
    let cols = headers.len();
    let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();
    for row in rows {
        for (i, cell) in row.iter().take(cols).enumerate() {
            widths[i] = widths[i].max(cell.len());
        }
    }

    let rule = |l: &str, m: &str, r: &str| {
        let mut s = String::from(l);
        for (i, w) in widths.iter().enumerate() {
            s.push_str(&"─".repeat(w + 2));
            s.push_str(if i + 1 == cols { r } else { m });
        }
        s.push('\n');
        s
    };
    let line = |cells: &[String]| {
        let mut s = String::from("│");
        for (i, w) in widths.iter().enumerate() {
            let cell = cells.get(i).map(String::as_str).unwrap_or("");
            s.push_str(&format!(" {cell:<w$} │", w = w));
        }
        s.push('\n');
        s
    };

    let mut out = String::new();
    out.push_str(&rule("┌", "┬", "┐"));
    out.push_str(&line(
        &headers.iter().map(|h| h.to_string()).collect::<Vec<_>>(),
    ));
    out.push_str(&rule("├", "┼", "┤"));
    for row in rows {
        out.push_str(&line(row));
    }
    out.push_str(&rule("└", "┴", "┘"));
    out
}

/// The `models resolve` table, separated from I/O so tests can assert on it.
pub fn resolve_report(lib: &ModelLibrary, board: &hauksbee_extract::ExtractedBoard) -> String {
    let mut out = String::from(
        "layer priority: builtin(0) < pack(10) < user-dir(20) < user-config-dir(25) < \
         models-dir(30) < spice(40); specificity breaks ties within a layer\n",
    );
    let rows: Vec<Vec<String>> = resolve_rows(lib, board)
        .into_iter()
        .map(|r| {
            vec![
                r.reference,
                r.value,
                r.model,
                r.layer,
                r.origin,
                r.source.tier().to_string(),
                r.source.validation().to_string(),
                uncertainty_label(&r.source),
            ]
        })
        .collect();
    out.push_str(&box_table(
        &[
            "Ref",
            "Value",
            "Model",
            "Layer",
            "Origin",
            "Tier",
            "Validation",
            "Uncertainty",
        ],
        &rows,
    ));
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
                    "source": r.source,
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
    backend: Option<hauksbee_models::datasheet::Backend>,
    model: Option<String>,
    api_base: Option<String>,
    api_key_env: Option<String>,
) -> anyhow::Result<()> {
    use hauksbee_models::datasheet;

    // Flag sanity before file checks: a pasted key must be refused before
    // anything else happens, whatever else is wrong with the invocation.
    if let Some(name) = &api_key_env {
        datasheet::validate_api_key_env_name(name)?;
    }
    if !pdf.is_file() {
        // Dead end -> next move: where to find the datasheet, and the flow
        // that fetches it for you.
        let query: String = part
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                    c.to_string()
                } else {
                    format!("%{:02X}", c as u32)
                }
            })
            .collect();
        anyhow::bail!(
            "no datasheet at '{}'. Search for one:\n  \
             https://duckduckgo.com/?q={query}+datasheet+pdf\n\
             or use the guided flow: `hauksbee serve` fetches the datasheet and \
             extracts the model for you (Extract a part).",
            pdf.display()
        );
    }

    // An empty --kind means the extractor identifies the part from the
    // datasheet itself; never print it as bare empty parens.
    if kind.trim().is_empty() {
        println!(
            "Extract a model for {part} (kind read from the datasheet) from {}",
            pdf.display()
        );
    } else {
        println!("Extract a model for {part} ({kind}) from {}", pdf.display());
    }
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
        .out_dir(out_dir.map(std::path::Path::to_path_buf))
        .model(model)
        .backend(backend)
        .api_base(api_base)
        .api_key_env(api_key_env);
    let written = datasheet::run(args)?;

    println!();
    println!(
        "This model is a draft with provenance \"datasheet-extracted\". Read it before you \
         trust a result that depends on it, and check any value the datasheet did not state \
         outright."
    );
    // End with the consuming command: the model exists to be used in a run.
    println!();
    match out_dir {
        Some(dir) => println!(
            "use it:  hauksbee run <your-board> --models-dir {}\n\
             check the binding:  hauksbee models resolve <your-board> --models-dir {}",
            dir.display(),
            dir.display()
        ),
        None => println!(
            "It landed in the standing user model directory ({}), which every run \
             already reads: re-run your board and it binds.\n\
             check the binding:  hauksbee models resolve <your-board>",
            written
                .parent()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "~/.hauksbee/models".to_string())
        ),
    }
    Ok(())
}

/// `hauksbee models new <REF> --board <board>`: scaffold a model entry for
/// one board component, pre-seeded only from the board's own identity (value
/// as the match regex). It never invents datasheet parameters or guesses the
/// device kind: until the author chooses a kind and supplies evidence,
/// `models lint` must refuse it. `--pack-dir` additionally writes a pack
/// manifest and the model under `models/`.
pub fn new(reference: &str, board_path: &Path, out: Option<&Path>) -> anyhow::Result<()> {
    new_with_kind(reference, board_path, out, None, None)
}

/// Build one unresolved model card without touching disk. Both `models new` and
/// `models prepare` use this exact renderer so bulk preparation cannot drift
/// from the single-part workflow.
fn scaffold_template(
    board_path: &Path,
    comp: &hauksbee_extract::Component,
    kind_override: Option<&str>,
) -> anyhow::Result<String> {
    let kind_override = kind_override.map(str::trim).filter(|kind| !kind.is_empty());
    if let Some(kind) = kind_override {
        if !hauksbee_models::validation::KIND_NAMES.contains(&kind) {
            anyhow::bail!(
                "unknown model kind '{kind}'. Pick one of: {}",
                hauksbee_models::validation::KIND_NAMES.join(", ")
            );
        }
    }
    // A reference designator is board context, not a datasheet. Even an `R`
    // or `D` prefix can be used for a custom symbol, and `Q` covers both BJTs
    // and MOSFETs in real projects. Never turn that convention into a physical
    // model claim. `choose_kind` is deliberately not a schema value: TOML
    // parses, but the shared linter refuses it until the author decides.
    let kind = kind_override.unwrap_or("choose_kind");
    let kind_note = if kind_override.is_some() {
        "# Selected explicitly with --kind; still verify it against the part documentation."
    } else {
        "# TODO: choose the device kind from the part documentation.\n\
         # `choose_kind` is an unresolved placeholder; `models lint` must refuse\n\
         # this file until you replace it with a supported kind."
    };
    let value = comp.value.trim();
    let id = sanitise_id(&format!("{}_{}", comp.reference, value));
    let value_re = format!("^{}$", escape_regex(value));
    // Basic TOML strings must escape both quotes and backslashes. Board values
    // routinely contain regex punctuation (`LM358(A)+`) and a raw interpolation
    // would emit invalid TOML (`\(` is not a TOML escape). Keep the generated
    // file syntactically valid without pretending the value itself is evidence.
    let value_re_toml = toml_quote(&value_re);
    let property_rules = scaffold_property_match_rules(&comp.properties);
    let board_display = board_path.display().to_string();
    let description_toml = toml_quote(&format!(
        "{} ({} on {}): describe the part",
        value,
        comp.reference,
        board_path.display()
    ));
    let template = format!(
        "# Model scaffold for {ref_display} ({value_display}) on {board}, generated by:\n\
         #   hauksbee models new {ref_display} --board {board}\n\
         # Provenance: unresolved authoring scaffold from board context. No device\n\
         # facts are asserted; verify every value against a source before use.\n\
         \n\
         [[models]]\n\
         id = \"{id}\"\n\
         {kind_note}\n\
         # Valid kinds: {kinds}.\n\
         kind = \"{kind}\"\n\
         description = {description}\n\
         \n\
         # Which board parts this entry claims: regexes (case-insensitive), ANDed.\n\
         # value_re matches the Value field; add mpn_re when the board carries a\n\
         # manufacturer part-number property, or footprint_re / lib_id for tighter\n\
         # aim.\n\
         [models.match]\n\
         value_re = {value_re}\n\
         # mpn_re = {value_re}\n\
         {property_rules}\
         \n\
         # Kind-specific parameters (what the solver simulates). `hauksbee models\n\
         # lint` names anything missing for the kind you picked.\n\
         # [models.params]\n\
         \n\
         # Absolute maximum ratings for the stress monitor (add what you know).\n\
         # [models.ratings]\n",
        ref_display = comp.reference,
        value_display = value,
        board = board_display,
        description = description_toml,
        id = id,
        kind = kind,
        kind_note = kind_note,
        kinds = hauksbee_models::validation::KIND_NAMES.join(", "),
        value_re = value_re_toml,
        property_rules = property_rules,
    );
    Ok(format!(
        "{template}\n\
         # No datasheet values are guessed here. The unknown interval is data;\n\
         # it is not permission to invent a tolerance.\n\
         [models.source]\n\
         tier = \"user-model\"\n\
         validation = \"unvalidated\"\n\
         # [[models.source.references]]\n\
         # url = \"https://vendor.example/device.pdf\"\n\
         # title = \"TODO: exact datasheet title and revision\"\n\
         # locator = \"TODO: table/section/page\"\n\
         # sha256 = \"TODO: optional exact source-byte SHA-256\"\n\
         [[models.source.uncertainty]]\n\
         status = \"unknown\"\n\
         parameter = \"model\"\n\
         reason = \"TODO: cite a datasheet, vendor model, or measured fit; this scaffold contains no device facts\"\n\
         \n\
         # Executable behavior is not limited to a nominal SPICE number. Use the\n\
         # smallest source-bound combination that answers the intended question:\n\
         # - [models.params] for R/C/L/diode/BJT/MOSFET/op-amp/vreg primitives;\n\
         # - [models.logic] for combinational/sequential digital behavior;\n\
         # - [models.behavioral] pins/FSM/converter/laws for rails, power states,\n\
         #   protection, source/load currents and time-dependent functional laws;\n\
         # - [models.peripheral] for firmware-visible I2C EEPROM or SPI NOR;\n\
         # - [models.current_program] for board-resistor-programmed current/limits.\n\
         # Declare [models.coverage].implements and .missing so `models coverage\n\
         # --require REF:CAPABILITY` can gate the exact question without claiming\n\
         # transistor-perfect behavior for unrelated functions.\n\
         \n\
         # TODO before trusting a result:\n\
         # - replace choose_kind and add the required parameters;\n\
         # - map every modeled pad in [models.pins];\n\
         # - cite each number beside the source table or measurement;\n\
         # - run `hauksbee models lint` then `hauksbee models resolve`.\n"
    ))
}

/// Retain exact board-provided identity evidence in a scaffold without
/// mistaking it for device behavior. These keys commonly carry MPNs or exact
/// distributor/library order codes. Every emitted rule is ANDed with the value
/// rule, so the draft cannot accidentally generalise from one sourced board to
/// every component with a similar display value.
fn scaffold_property_match_rules(properties: &[(String, String)]) -> String {
    let mut rows = properties
        .iter()
        .filter(|(key, value)| {
            if value.trim().is_empty() {
                return false;
            }
            let key = key.trim().to_ascii_lowercase().replace([' ', '-'], "_");
            key.contains("mpn")
                || key.contains("manufacturer_part")
                || key == "part_number"
                || key == "mfr_part"
                || key == "kilib_generator"
                || key.contains("digikey")
                || key.contains("mouser")
                || key.contains("farnell")
                || key.contains("lcsc")
        })
        .map(|(key, value)| {
            let regex = format!("^{}$", escape_regex(value.trim()));
            format!("{} = {}", toml_quote(key), toml_quote(&regex))
        })
        .collect::<Vec<_>>();
    rows.sort();
    rows.dedup();
    if rows.is_empty() {
        String::new()
    } else {
        format!(
            "# Exact identity evidence retained from the board; this targets the\n\
             # source record but does not itself assert electrical behavior.\n\
             [models.match.properties]\n\
             {}\n",
            rows.join("\n")
        )
    }
}

/// The pack manifest is deliberately boring: it identifies a local hand-written
/// pack and leaves licensing/evidence decisions to its author. Keep this helper
/// shared by the single-part and bulk scaffolds so the two workflows cannot
/// silently drift.
fn pack_manifest(pack_dir: &Path) -> (PathBuf, String) {
    let pack_name = pack_dir
        .file_name()
        .and_then(|n| n.to_str())
        .map(sanitise_id)
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "model-pack".to_string());
    let manifest_path = pack_dir.join("pack.toml");
    let manifest = format!(
        "# Hauksbee model-pack scaffold for {pack_name}.\n\
         # TODO: set a real SPDX license before sharing or installing.\n\
         # TODO: review the version and description before publishing.\n\
         [pack]\n\
         name = \"{pack_name}\"\n\
         version = \"0.1.0\"\n\
         min_hauksbee_version = \"0.1.0\"\n\
         provenance = \"hand-written\"\n\
         description = \"TODO: describe the evidence-backed models in this pack\"\n\
         # license = \"MIT\"\n"
    );
    (manifest_path, manifest)
}

fn model_id(comp: &hauksbee_extract::Component) -> String {
    sanitise_id(&format!("{}_{}", comp.reference, comp.value.trim()))
}

/// Variant used by the CLI when the author supplies an explicit device kind.
/// The compatibility wrapper above keeps the public Rust API additive.
pub fn new_with_kind(
    reference: &str,
    board_path: &Path,
    out: Option<&Path>,
    kind_override: Option<&str>,
    pack_dir: Option<&Path>,
) -> anyhow::Result<()> {
    let board = crate::board_input::from_path(board_path)?.board;
    let Some(comp) = board
        .components
        .iter()
        .find(|c| c.reference.eq_ignore_ascii_case(reference))
    else {
        let known: Vec<String> = board
            .components
            .iter()
            .map(|c| c.reference.clone())
            .collect();
        let near = crate::reports::cosim::nearest_nets(reference, &known, 3);
        let hint = if near.is_empty() {
            String::new()
        } else {
            format!(" Did you mean {}?", near.join(", "))
        };
        anyhow::bail!(
            "no component '{reference}' on {}.{hint} (`hauksbee models resolve {}` \
             lists every reference.)",
            board_path.display(),
            board_path.display()
        );
    };

    let template = scaffold_template(board_path, comp, kind_override)?;
    let id = model_id(comp);

    let (out_path, pack_manifest): (PathBuf, Option<(PathBuf, String)>) =
        if let Some(pack_dir) = pack_dir {
            let (manifest_path, manifest) = pack_manifest(pack_dir);
            let model_path = pack_dir.join("models").join(format!("{id}.toml"));
            (model_path, Some((manifest_path, manifest)))
        } else {
            (
                out.map(Path::to_path_buf)
                    .unwrap_or_else(|| PathBuf::from(format!("{id}.toml"))),
                None,
            )
        };
    if out_path.exists() {
        anyhow::bail!(
            "'{}' already exists; refusing to overwrite (pass --out for another path)",
            out_path.display()
        );
    }
    if let Some((manifest_path, manifest)) = &pack_manifest {
        if manifest_path.exists() {
            anyhow::bail!(
                "'{}' already exists; refusing to overwrite the pack manifest",
                manifest_path.display()
            );
        }
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                anyhow::anyhow!("creating model-pack directory '{}': {e}", parent.display())
            })?;
        }
        std::fs::write(manifest_path, manifest)
            .map_err(|e| anyhow::anyhow!("writing '{}': {e}", manifest_path.display()))?;
    }
    std::fs::write(&out_path, template)
        .map_err(|e| anyhow::anyhow!("writing '{}': {e}", out_path.display()))?;
    let dir = out_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| ".".to_string());
    println!("wrote {}", out_path.display());
    if let Some((manifest_path, _)) = &pack_manifest {
        println!("wrote {}", manifest_path.display());
    }
    println!();
    println!(
        "edit it, then:\n  \
         hauksbee models lint {out}\n  \
         hauksbee models resolve {board} --models-dir {dir}   # {ref_} should bind to '{id}'",
        out = out_path.display(),
        board = board_path.display(),
        dir = dir,
        ref_ = comp.reference,
        id = id,
    );
    Ok(())
}

#[derive(Debug)]
struct PlannedFile {
    path: PathBuf,
    contents: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CoverageStage {
    Unresolved,
    IdentityOnly,
    ExecutableScopeUnspecified,
    ExecutablePartial,
    ExecutableDeclared,
    Ignored,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct CoveragePin {
    pub number: String,
    pub function: String,
    pub kind: String,
    pub net: Option<String>,
    pub position_mm: Option<(f64, f64)>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct CoverageProperty {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct CoverageComponent {
    pub reference: String,
    pub value: String,
    pub lib_id: String,
    pub footprint: String,
    pub stage: CoverageStage,
    pub actionable_behavior_gap: bool,
    pub model_id: Option<String>,
    pub model_kind: Option<String>,
    pub layer: String,
    pub origin: String,
    pub source: ModelSource,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub references: Vec<hauksbee_models::schema::ModelSourceReference>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub properties: Vec<CoverageProperty>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub static_contracts: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub implements: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub missing: Vec<String>,
    pub pins: Vec<CoveragePin>,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct CoverageSummary {
    pub active_connected: usize,
    /// Resolved to at least an identity card (active_connected - unresolved).
    pub identified: usize,
    /// Has an executable model, whether its scope is unspecified, partial, or
    /// complete for every capability the card declares.
    pub executable_available: usize,
    pub unresolved: usize,
    pub identity_only: usize,
    pub executable_scope_unspecified: usize,
    pub executable_partial: usize,
    pub executable_declared: usize,
    pub ignored: usize,
    pub actionable_behavior_gaps: usize,
    /// Connected unresolved/identity-only components for which `models
    /// prepare` can draft an authoring card. This is deliberately separate
    /// from `active_connected`: the latter is the stable U/IC/MCU behavioural
    /// denominator, while this count also includes load-bearing discrete and
    /// module parts such as Q/F/L/CM.
    pub authoring_targets: usize,
    pub requirements_total: usize,
    pub requirements_met: usize,
    pub requirements_unmet: usize,
}

/// Model identity and executable-behaviour coverage for one extracted board.
///
/// This is deliberately a library surface rather than CLI-only output. The
/// browser and MCP server consume these exact rows, so a component click can
/// never call an identity card "fully modelled" while `models coverage` says
/// it is partial. No files are written and no network or LLM is used.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ModelCoverageSnapshot {
    pub schema_version: u32,
    pub summary: CoverageSummary,
    pub components: Vec<CoverageComponent>,
    /// Connected unresolved/identity-only devices that `models prepare` can
    /// scaffold after the user approves the destination and write.
    pub authoring_targets: Vec<CoverageComponent>,
}

#[derive(Debug, Clone, serde::Serialize)]
struct CoverageBoard {
    path: String,
    name: String,
    input_kind: String,
    sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sha256_unavailable_reason: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
struct CoverageReport {
    schema_version: u32,
    board: CoverageBoard,
    summary: CoverageSummary,
    components: Vec<CoverageComponent>,
    /// Every connected component that is unresolved or identity-only, not
    /// merely U/IC/MCU references. These are the exact rows `models prepare`
    /// offers to scaffold after approval.
    authoring_targets: Vec<CoverageComponent>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    requirements: Vec<CoverageRequirementResult>,
}

#[derive(Debug, Clone)]
struct CoverageRequirement {
    reference: String,
    capability: String,
}

#[derive(Debug, Clone, serde::Serialize)]
struct CoverageRequirementResult {
    reference: String,
    capability: String,
    met: bool,
    model_id: Option<String>,
    stage: CoverageStage,
    reason: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    references: Vec<hauksbee_models::schema::ModelSourceReference>,
}

fn model_kind_name(kind: hauksbee_models::ComponentKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| format!("{kind:?}").to_ascii_lowercase())
}

fn input_sha256(path: &Path) -> (Option<String>, Option<String>) {
    match std::fs::read(path) {
        Ok(bytes) => {
            let digest = Sha256::digest(bytes);
            let hex = digest.iter().map(|byte| format!("{byte:02x}")).collect();
            (Some(hex), None)
        }
        Err(_error) if path.is_dir() => (
            None,
            Some("input is a directory; no single-file SHA-256 exists".to_string()),
        ),
        Err(error) => (None, Some(format!("could not hash input: {error}"))),
    }
}

fn active_connected_component(comp: &hauksbee_extract::Component) -> bool {
    !comp.dnp
        && is_active_model_reference(&comp.reference)
        && comp
            .pins
            .iter()
            .any(|pin| pin.net.is_some_and(|net| net != 0))
}

fn connected_component(comp: &hauksbee_extract::Component) -> bool {
    !comp.dnp
        && comp
            .pins
            .iter()
            .any(|pin| pin.net.is_some_and(|net| net != 0))
}

fn coverage_component(
    comp: &hauksbee_extract::Component,
    net_names: &std::collections::HashMap<i64, String>,
    lib: &ModelLibrary,
) -> CoverageComponent {
    let resolution = crate::binder::library_resolution(lib, comp);
    let source = resolution
        .provenance
        .clone()
        .unwrap_or_else(|| open_source(&comp.reference));
    let model = resolution.model.as_ref();
    let identity_only = model
        .and_then(|entry| entry.params.get_bool("identity_only"))
        .unwrap_or(false);
    let stage = match model {
        None => CoverageStage::Unresolved,
        Some(entry) if entry.kind == hauksbee_models::ComponentKind::Ignore => {
            CoverageStage::Ignored
        }
        Some(_) if identity_only => CoverageStage::IdentityOnly,
        Some(entry) if entry.coverage.is_empty() => CoverageStage::ExecutableScopeUnspecified,
        Some(entry) if !entry.coverage.missing.is_empty() => CoverageStage::ExecutablePartial,
        Some(_) => CoverageStage::ExecutableDeclared,
    };
    let actionable_behavior_gap = matches!(
        stage,
        CoverageStage::Unresolved
            | CoverageStage::IdentityOnly
            | CoverageStage::ExecutableScopeUnspecified
            | CoverageStage::ExecutablePartial
    );
    let mut static_contracts = Vec::new();
    if let Some(entry) = model {
        if let Some(roles) = entry.params.get_str("must_not_float_roles") {
            static_contracts.extend(
                roles
                    .split(',')
                    .map(str::trim)
                    .filter(|role| !role.is_empty())
                    .map(|role| format!("must_not_float:{role}")),
            );
        }
        static_contracts.extend(
            entry
                .straps
                .iter()
                .map(|strap| format!("strap:{}", strap.role)),
        );
    }
    let pins = comp
        .pins
        .iter()
        .map(|pin| CoveragePin {
            number: pin.number.clone(),
            function: pin.function.clone(),
            kind: pin.kind.clone(),
            net: pin.net.and_then(|id| net_names.get(&id).cloned()),
            position_mm: pin.position,
        })
        .collect();
    CoverageComponent {
        reference: comp.reference.clone(),
        value: comp.value.clone(),
        lib_id: comp.lib_id.clone(),
        footprint: comp.footprint.clone(),
        stage,
        actionable_behavior_gap,
        model_id: model.map(|entry| entry.id.clone()),
        model_kind: model.map(|entry| model_kind_name(entry.kind)),
        layer: resolution
            .layer
            .map(|layer| layer.to_string())
            .unwrap_or_else(|| {
                if model.is_some() {
                    "engine-fallback".to_string()
                } else {
                    "-".to_string()
                }
            }),
        origin: resolution.origin.unwrap_or_else(|| "-".to_string()),
        source,
        references: resolution.references.clone(),
        properties: comp
            .properties
            .iter()
            .map(|(name, value)| CoverageProperty {
                name: name.clone(),
                value: value.clone(),
            })
            .collect(),
        static_contracts,
        implements: model
            .map(|entry| entry.coverage.implements.clone())
            .unwrap_or_default(),
        missing: model
            .map(|entry| entry.coverage.missing.clone())
            .unwrap_or_default(),
        pins,
    }
}

fn coverage_components(
    board: &hauksbee_extract::ExtractedBoard,
    lib: &ModelLibrary,
) -> Vec<CoverageComponent> {
    let net_names: std::collections::HashMap<i64, String> = board
        .nets
        .iter()
        .map(|net| (net.id, net.name.clone()))
        .collect();
    let mut rows = board
        .components
        .iter()
        .filter(|comp| active_connected_component(comp))
        .map(|comp| coverage_component(comp, &net_names, lib))
        .collect::<Vec<_>>();
    rows.sort_by_key(|row| natural_ref_key(&row.reference));
    rows
}

fn coverage_summary(components: &[CoverageComponent]) -> CoverageSummary {
    let mut summary = CoverageSummary {
        active_connected: components.len(),
        ..CoverageSummary::default()
    };
    for component in components {
        match component.stage {
            CoverageStage::Unresolved => summary.unresolved += 1,
            CoverageStage::IdentityOnly => summary.identity_only += 1,
            CoverageStage::ExecutableScopeUnspecified => summary.executable_scope_unspecified += 1,
            CoverageStage::ExecutablePartial => summary.executable_partial += 1,
            CoverageStage::ExecutableDeclared => summary.executable_declared += 1,
            CoverageStage::Ignored => summary.ignored += 1,
        }
        if component.actionable_behavior_gap {
            summary.actionable_behavior_gaps += 1;
        }
    }
    summary.identified = summary.active_connected - summary.unresolved;
    summary.executable_available = summary.executable_scope_unspecified
        + summary.executable_partial
        + summary.executable_declared;
    summary
}

/// Compute the shared, read-only model coverage payload used by the CLI,
/// browser report and MCP tool. `models prepare` is a separate explicit write
/// action; merely asking what is missing never mutates the user's model store.
pub fn coverage_snapshot(
    board: &hauksbee_extract::ExtractedBoard,
    lib: &ModelLibrary,
) -> ModelCoverageSnapshot {
    let components = coverage_components(board, lib);
    let net_names: std::collections::HashMap<i64, String> = board
        .nets
        .iter()
        .map(|net| (net.id, net.name.clone()))
        .collect();
    let mut authoring_targets = board
        .components
        .iter()
        .filter(|comp| connected_component(comp))
        .map(|comp| coverage_component(comp, &net_names, lib))
        .filter(|row| {
            matches!(
                row.stage,
                CoverageStage::Unresolved | CoverageStage::IdentityOnly
            )
        })
        .collect::<Vec<_>>();
    authoring_targets.sort_by_key(|row| natural_ref_key(&row.reference));
    let mut summary = coverage_summary(&components);
    summary.authoring_targets = authoring_targets.len();
    ModelCoverageSnapshot {
        schema_version: 1,
        summary,
        components,
        authoring_targets,
    }
}

/// Read a board from disk and return model coverage without printing or
/// writing. This is the programmatic entry used by MCP and other automation;
/// callers that want to create scaffolds must invoke the separately
/// approval-gated [`prepare`] workflow.
pub fn coverage_snapshot_for_path(
    board_path: &Path,
    models_dir: Option<&Path>,
) -> anyhow::Result<ModelCoverageSnapshot> {
    let normalized = crate::board_input::from_path(board_path)?;
    let extra: Vec<&Path> = models_dir.into_iter().collect();
    let lib = ModelLibrary::builtin_with_user_dirs(&extra);
    Ok(coverage_snapshot(&normalized.board, &lib))
}

fn coverage_report(
    board_path: &Path,
    normalized: &crate::board_input::NormalizedBoard,
    lib: &ModelLibrary,
    requirements: &[CoverageRequirement],
) -> CoverageReport {
    let snapshot = coverage_snapshot(&normalized.board, lib);
    let components = snapshot.components;
    let net_names: std::collections::HashMap<i64, String> = normalized
        .board
        .nets
        .iter()
        .map(|net| (net.id, net.name.clone()))
        .collect();
    let authoring_targets = snapshot.authoring_targets;
    let requirement_results = requirements
        .iter()
        .map(|requirement| {
            let row = components
                .iter()
                .chain(authoring_targets.iter())
                .find(|row| row.reference.eq_ignore_ascii_case(&requirement.reference))
                .cloned()
                .or_else(|| {
                    normalized
                        .board
                        .components
                        .iter()
                        .find(|comp| {
                            connected_component(comp)
                                && comp.reference.eq_ignore_ascii_case(&requirement.reference)
                        })
                        .map(|comp| coverage_component(comp, &net_names, lib))
                });
            match row.as_ref() {
                Some(row) => {
                    let met = row
                        .implements
                        .iter()
                        .any(|capability| capability == &requirement.capability);
                    let reason = if met {
                        format!(
                            "model '{}' declares this capability executable",
                            row.model_id.as_deref().unwrap_or("(unnamed)")
                        )
                    } else if row
                        .missing
                        .iter()
                        .any(|capability| capability == &requirement.capability)
                    {
                        format!(
                            "model '{}' explicitly declares this capability missing",
                            row.model_id.as_deref().unwrap_or("(unnamed)")
                        )
                    } else {
                        format!(
                            "model '{}' does not declare this capability implemented; fail-closed scope is {:?}",
                            row.model_id.as_deref().unwrap_or("UNRESOLVED"),
                            row.stage
                        )
                    };
                    CoverageRequirementResult {
                        reference: row.reference.clone(),
                        capability: requirement.capability.clone(),
                        met,
                        model_id: row.model_id.clone(),
                        stage: row.stage,
                        reason,
                        references: row.references.clone(),
                    }
                }
                None => CoverageRequirementResult {
                    reference: requirement.reference.clone(),
                    capability: requirement.capability.clone(),
                    met: false,
                    model_id: None,
                    stage: CoverageStage::Unresolved,
                    reason: "reference is not a connected device on this board".to_string(),
                    references: Vec::new(),
                },
            }
        })
        .collect::<Vec<_>>();
    let mut summary = coverage_summary(&components);
    summary.authoring_targets = authoring_targets.len();
    summary.requirements_total = requirement_results.len();
    summary.requirements_met = requirement_results.iter().filter(|row| row.met).count();
    summary.requirements_unmet = summary.requirements_total - summary.requirements_met;
    let (sha256, sha256_unavailable_reason) = input_sha256(board_path);
    CoverageReport {
        schema_version: 4,
        board: CoverageBoard {
            path: board_path.display().to_string(),
            name: normalized.board.name.clone(),
            input_kind: format!("{:?}", normalized.kind).to_ascii_lowercase(),
            sha256,
            sha256_unavailable_reason,
        },
        summary,
        components,
        authoring_targets,
        requirements: requirement_results,
    }
}

fn parse_coverage_requirements(values: &[String]) -> anyhow::Result<Vec<CoverageRequirement>> {
    values
        .iter()
        .map(|value| {
            let Some((reference, capability)) = value.split_once(':') else {
                anyhow::bail!(
                    "invalid model requirement '{value}': expected REF:CAPABILITY (for example U6:dc_output_regulation)"
                );
            };
            let reference = reference.trim();
            let capability = capability.trim();
            if reference.is_empty() || capability.is_empty() {
                anyhow::bail!(
                    "invalid model requirement '{value}': both REF and CAPABILITY must be non-empty"
                );
            }
            Ok(CoverageRequirement {
                reference: reference.to_string(),
                capability: capability.to_string(),
            })
        })
        .collect()
}

/// `hauksbee models coverage <BOARD>`: report what was extracted, what model
/// identity won, what static and executable scope is present, and what remains.
/// This is deliberately read-only; `models prepare` is the separate,
/// approval-gated transition from inventory to files.
pub fn coverage(
    board_path: &Path,
    models_dir: Option<&Path>,
    json: bool,
    required: &[String],
) -> anyhow::Result<()> {
    let normalized = crate::board_input::from_path(board_path)?;
    let extra: Vec<&Path> = models_dir.into_iter().collect();
    let lib = ModelLibrary::builtin_with_user_dirs(&extra);
    let requirements = parse_coverage_requirements(required)?;
    let report = coverage_report(board_path, &normalized, &lib, &requirements);
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        if report.summary.requirements_unmet > 0 {
            anyhow::bail!(
                "{} of {} requested model capabilities are unmet",
                report.summary.requirements_unmet,
                report.summary.requirements_total
            );
        }
        return Ok(());
    }
    println!(
        "Model coverage for {}: {}/{} connected active devices identified; {}/{} have executable behavior; {} unresolved; {} identity-only; {} executable with unspecified scope; {} executable partial; {} executable with no declared gaps.",
        board_path.display(),
        report.summary.identified,
        report.summary.active_connected,
        report.summary.executable_available,
        report.summary.active_connected,
        report.summary.unresolved,
        report.summary.identity_only,
        report.summary.executable_scope_unspecified,
        report.summary.executable_partial,
        report.summary.executable_declared,
    );
    if !report.authoring_targets.is_empty() {
        println!(
            "Authoring queue: {} connected unresolved/identity-only component(s), including load-bearing discretes and module boundaries.",
            report.authoring_targets.len()
        );
        let targets = report
            .authoring_targets
            .iter()
            .map(|component| {
                vec![
                    component.reference.clone(),
                    component.value.clone(),
                    serde_json::to_value(component.stage)
                        .ok()
                        .and_then(|value| value.as_str().map(str::to_owned))
                        .unwrap_or_else(|| "unknown".to_string()),
                    component
                        .model_id
                        .clone()
                        .unwrap_or_else(|| "UNRESOLVED".to_string()),
                ]
            })
            .collect::<Vec<_>>();
        print!(
            "{}",
            box_table(&["Ref", "Value", "Stage", "Current model"], &targets)
        );
    }
    let rows = report
        .components
        .iter()
        .map(|component| {
            vec![
                component.reference.clone(),
                component.value.clone(),
                serde_json::to_value(component.stage)
                    .ok()
                    .and_then(|value| value.as_str().map(str::to_owned))
                    .unwrap_or_else(|| "unknown".to_string()),
                component
                    .model_id
                    .clone()
                    .unwrap_or_else(|| "UNRESOLVED".to_string()),
                if component.implements.is_empty() {
                    "-".to_string()
                } else {
                    component.implements.join(",")
                },
                if component.missing.is_empty() {
                    "-".to_string()
                } else {
                    component.missing.join(",")
                },
            ]
        })
        .collect::<Vec<_>>();
    print!(
        "{}",
        box_table(
            &["Ref", "Value", "Stage", "Model", "Implements", "Missing"],
            &rows,
        )
    );
    if !report.requirements.is_empty() {
        let requirements = report
            .requirements
            .iter()
            .map(|row| {
                vec![
                    row.reference.clone(),
                    row.capability.clone(),
                    if row.met { "MET" } else { "UNMET" }.to_string(),
                    row.reason.clone(),
                ]
            })
            .collect::<Vec<_>>();
        print!(
            "{}",
            box_table(
                &["Ref", "Required capability", "Status", "Evidence"],
                &requirements,
            )
        );
    }
    println!(
        "Read-only. To draft unresolved identities and extend partial executable models, run: hauksbee models prepare {} --pack-dir <DIR>",
        board_path.display()
    );
    if report.summary.requirements_unmet > 0 {
        anyhow::bail!(
            "{} of {} requested model capabilities are unmet",
            report.summary.requirements_unmet,
            report.summary.requirements_total
        );
    }
    Ok(())
}

/// `hauksbee models prepare <BOARD> --pack-dir <DIR>`: enumerate connected
/// identities that remain unresolved and copy partial executable cards into
/// safe board-local extensions, then ask before writing any file. This command is
/// intentionally local and deterministic: it does not fetch datasheets,
/// invoke an LLM, install a pack, or mutate the model library.
pub fn prepare(
    board_path: &Path,
    pack_dir: &Path,
    models_dir: Option<&Path>,
    assume_yes: bool,
) -> anyhow::Result<()> {
    let normalized = crate::board_input::from_path(board_path)?;
    let extra: Vec<&Path> = models_dir.into_iter().collect();
    let lib = ModelLibrary::builtin_with_user_dirs(&extra);
    let report = coverage_report(board_path, &normalized, &lib, &[]);
    let components = coverage_authoring_components(&normalized.board, &report);

    println!(
        "Inspecting {} for unresolved identities and declared behavior gaps (installed/user library{}).",
        board_path.display(),
        models_dir
            .map(|path| format!(" plus {}", path.display()))
            .unwrap_or_default()
    );
    if components.is_empty() {
        println!(
            "No connected unresolved identities or declared behavior gaps found; nothing will be written."
        );
        return Ok(());
    }

    #[derive(serde::Serialize)]
    struct WorkItem {
        reference: String,
        value: String,
        stage_before: CoverageStage,
        base_model_id: Option<String>,
        prepared_file: String,
        implements_before: Vec<String>,
        missing_before: Vec<String>,
        required_actions: Vec<String>,
    }
    #[derive(serde::Serialize)]
    struct WorkPlan {
        schema_version: u32,
        board: String,
        inventory: String,
        approval: String,
        items: Vec<WorkItem>,
        validation_commands: Vec<String>,
    }

    let mut plans = Vec::with_capacity(components.len() + 3);
    let mut work_items = Vec::with_capacity(components.len());
    let (manifest_path, manifest) = pack_manifest(pack_dir);
    plans.push(PlannedFile {
        path: manifest_path,
        contents: manifest,
    });
    plans.push(PlannedFile {
        path: pack_dir.join("inventory.json"),
        // Retain the whole read-only report: the active-device denominator and
        // the broader authoring queue are both evidence the author needs.
        contents: format!("{}\n", serde_json::to_string_pretty(&report)?),
    });
    for comp in &components {
        let id = model_id(comp);
        let existing = report
            .components
            .iter()
            .chain(report.authoring_targets.iter())
            .find(|row| row.reference == comp.reference);
        let resolution = crate::binder::library_resolution(&lib, comp);
        let executable_upgrade = existing.is_some_and(|row| {
            matches!(
                row.stage,
                CoverageStage::ExecutableScopeUnspecified | CoverageStage::ExecutablePartial
            )
        });
        let contents = if executable_upgrade {
            editable_model_upgrade(board_path, comp, &resolution)?
        } else {
            let context = existing
                .and_then(|row| row.model_id.as_deref().map(|model| (row, model)))
                .map(|(row, model)| {
                    let missing = if row.missing.is_empty() {
                        "not declared by the identity card".to_string()
                    } else {
                        row.missing.join(", ")
                    };
                    let unlocked_by = resolution
                        .model
                        .as_ref()
                        .and_then(|entry| entry.params.get_str("unlocked_by"))
                        .unwrap_or("a source-bound executable model for the behavior this analysis requires")
                        .to_string();
                    format!(
                        "# Upgrade target: source-bound identity model '{model}' already resolves.\n\
                         # Declared missing behavior: {missing}.\n\
                         # Existing card says behavior is unlocked by: {unlocked_by}.\n\
                         # Board-observed pins/nets and provenance are retained in ../inventory.json.\n\
                         # This draft deliberately remains lint-invalid until executable behavior is supplied.\n"
                    )
                })
                .unwrap_or_default();
            format!("{context}{}", scaffold_template(board_path, comp, None)?)
        };
        plans.push(PlannedFile {
            path: pack_dir.join("models").join(format!("{id}.toml")),
            contents,
        });
        let row = existing.expect("authoring components originate in the coverage report");
        work_items.push(WorkItem {
            reference: comp.reference.clone(),
            value: comp.value.clone(),
            stage_before: row.stage,
            base_model_id: row.model_id.clone(),
            prepared_file: format!("models/{id}.toml"),
            implements_before: row.implements.clone(),
            missing_before: row.missing.clone(),
            required_actions: vec![
                "bind manufacturer source and exact pin/function identity".to_string(),
                "implement only source-supported reusable behavior".to_string(),
                "replace unknown uncertainty and unvalidated provenance only after evidence"
                    .to_string(),
                "add exact positive and hostile near-name tests".to_string(),
            ],
        });
    }
    // Keep every handoff command directly runnable. `models lint` accepts one
    // file, so a shell glob would expand to an invalid multi-argument command
    // as soon as the pack contains more than one card. Exact per-file commands
    // are also easier for humans and agents to retry selectively.
    let mut validation_commands = work_items
        .iter()
        .map(|item| {
            format!(
                "hauksbee models lint {}",
                shell_quote(&pack_dir.join(&item.prepared_file).display().to_string())
            )
        })
        .collect::<Vec<_>>();
    validation_commands.push(format!(
        "hauksbee models coverage {} --models-dir {} --json",
        shell_quote(&board_path.display().to_string()),
        shell_quote(&pack_dir.join("models").display().to_string())
    ));
    validation_commands.push(format!(
        "hauksbee run {} --check --json --strict --models-dir {}",
        shell_quote(&board_path.display().to_string()),
        shell_quote(&pack_dir.join("models").display().to_string())
    ));
    let work_plan = WorkPlan {
        schema_version: 1,
        board: board_path.display().to_string(),
        inventory: "inventory.json".to_string(),
        approval: "these files were written only after the interactive prompt or explicit --yes"
            .to_string(),
        items: work_items,
        validation_commands,
    };
    plans.insert(
        2,
        PlannedFile {
            path: pack_dir.join("workplan.json"),
            contents: format!("{}\n", serde_json::to_string_pretty(&work_plan)?),
        },
    );

    println!(
        "This will write exactly {} file(s) under {} (no network, LLM, installer, or pack registration):",
        plans.len(),
        pack_dir.display()
    );
    for plan in &plans {
        let comp = components.iter().find(|c| {
            plan.path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .is_some_and(|stem| stem == model_id(c))
        });
        if let Some(comp) = comp {
            println!(
                "  {}  <- {} ({})",
                plan.path.display(),
                comp.reference,
                comp.value
            );
        } else {
            println!("  {}", plan.path.display());
        }
    }
    for plan in &plans {
        if plan.path.exists() {
            anyhow::bail!(
                "'{}' already exists; refusing to overwrite (nothing was written)",
                plan.path.display()
            );
        }
    }

    if !request_prepare_approval(assume_yes)? {
        println!("Nothing was written.");
        return Ok(());
    }

    std::fs::create_dir_all(pack_dir.join("models")).map_err(|e| {
        anyhow::anyhow!(
            "creating model-pack directory '{}': {e}",
            pack_dir.join("models").display()
        )
    })?;
    for plan in &plans {
        std::fs::write(&plan.path, &plan.contents)
            .map_err(|e| anyhow::anyhow!("writing '{}': {e}", plan.path.display()))?;
        println!("wrote {}", plan.path.display());
    }
    Ok(())
}

/// Select every exact row the coverage surface calls actionable, including a
/// model that executes only part of its declared behavior. Preparation is an
/// authoring inventory, not an analysis, so it still avoids circuit binding
/// and solving.
fn coverage_authoring_components<'a>(
    board: &'a hauksbee_extract::ExtractedBoard,
    report: &CoverageReport,
) -> Vec<&'a hauksbee_extract::Component> {
    let references = report
        .components
        .iter()
        .chain(report.authoring_targets.iter())
        .filter(|row| row.actionable_behavior_gap)
        .map(|row| row.reference.as_str())
        .collect::<std::collections::HashSet<_>>();
    let mut components: Vec<_> = board
        .components
        .iter()
        .filter(|comp| connected_component(comp))
        .filter(|comp| references.contains(comp.reference.as_str()))
        .collect();
    components.sort_by_key(|comp| natural_ref_key(&comp.reference));
    components
}

/// Start a partial-model upgrade from the exact winning executable card rather
/// than an empty shell. Its match is narrowed to this board-observed identity,
/// and its provenance is demoted to user-model/unvalidated: copied behavior
/// keeps working, while any edits must earn validation before an accuracy gate
/// can promote them.
fn editable_model_upgrade(
    board_path: &Path,
    comp: &hauksbee_extract::Component,
    resolution: &hauksbee_models::Resolution,
) -> anyhow::Result<String> {
    editable_model_upgrade_fields(
        &board_path.display().to_string(),
        &comp.reference,
        &comp.value,
        &comp.lib_id,
        &comp.footprint,
        resolution,
    )
}

/// Build the same conservative partial-model extension used by `models
/// prepare`, but from a browser-selected component. This function is
/// read-only: it returns reviewable TOML and never writes or fetches anything.
/// The expected winning id prevents a stale browser report from silently
/// drafting against a different installed model library.
pub fn draft_model_upgrade(
    board_label: &str,
    reference: &str,
    value: &str,
    lib_id: &str,
    footprint: &str,
    properties: &[(String, String)],
    expected_model_id: &str,
) -> anyhow::Result<String> {
    let lib = ModelLibrary::builtin_with_user_dirs(&[]);
    let component = hauksbee_extract::Component {
        reference: reference.to_string(),
        value: value.to_string(),
        lib_id: lib_id.to_string(),
        footprint: footprint.to_string(),
        position: None,
        layer: String::new(),
        properties: properties.to_vec(),
        dnp: false,
        pins: Vec::new(),
    };
    let resolution = crate::binder::library_resolution(&lib, &component);
    let actual = resolution.model.as_ref().map(|model| model.id.as_str());
    if actual != Some(expected_model_id) {
        anyhow::bail!(
            "the model library changed since this report (expected '{}', now resolved '{}'); re-analyze the board before drafting",
            expected_model_id,
            actual.unwrap_or("nothing")
        );
    }
    editable_model_upgrade_fields(
        board_label,
        reference,
        value,
        lib_id,
        footprint,
        &resolution,
    )
}

/// Build the same unresolved, evidence-first scaffold used by `models new` and
/// `models prepare`, but from a browser-selected component. This is deliberately
/// read-only: it returns bytes for review and never creates a pack, saves a
/// model, fetches a datasheet, or invokes an LLM.
pub fn draft_model_scaffold(
    board_label: &str,
    reference: &str,
    value: &str,
    lib_id: &str,
    footprint: &str,
    properties: &[(String, String)],
) -> anyhow::Result<String> {
    if reference.trim().is_empty() {
        anyhow::bail!("the model scaffold needs a component reference");
    }
    let component = hauksbee_extract::Component {
        reference: reference.to_string(),
        value: value.to_string(),
        lib_id: lib_id.to_string(),
        footprint: footprint.to_string(),
        position: None,
        layer: String::new(),
        properties: properties.to_vec(),
        dnp: false,
        pins: Vec::new(),
    };
    scaffold_template(Path::new(board_label), &component, None)
}

fn editable_model_upgrade_fields(
    board_label: &str,
    reference: &str,
    value: &str,
    lib_id: &str,
    footprint: &str,
    resolution: &hauksbee_models::Resolution,
) -> anyhow::Result<String> {
    let mut model = resolution.model.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "{} was marked as an executable upgrade but no model resolved",
            reference
        )
    })?;
    let base_id = model.id.clone();
    model.id = format!(
        "{}_board_extension",
        sanitise_id(&format!("{}_{}", reference, value.trim()))
    );
    model.r#match = hauksbee_models::schema::MatchRules {
        lib_id: (!lib_id.trim().is_empty()).then(|| lib_id.to_string()),
        value_re: (!value.trim().is_empty())
            .then(|| format!("(?i)^{}$", escape_regex(value.trim()))),
        footprint_re: (!footprint.trim().is_empty())
            .then(|| format!("^{}$", escape_regex(footprint.trim()))),
        mpn_re: None,
        properties: Default::default(),
    };

    let source = hauksbee_models::schema::ModelSourceSpec {
        tier: ModelSourceTier::UserModel,
        validation: ModelValidation::Unvalidated,
        references: resolution.references.clone(),
        uncertainty: vec![ModelUncertainty::unknown(
            format!("{}.board_extension", model.id),
            "copied from a resolved model for local extension; edits have not yet been validated",
        )?],
    };
    let missing = model.coverage.missing.join(", ");
    // Do not flatten ModelEntry into an auxiliary wrapper here. The TOML
    // serializer accepts the scalar fields and the first nested table from a
    // flattened value but can silently omit the later sibling tables. For an
    // executable partial model that discarded pins, params, peripheral and
    // coverage while still returning syntactically valid TOML—the exact
    // opposite of an extension. Build the one `[[models]]` value explicitly,
    // insert provenance as an ordinary sibling table, then serialize the whole
    // tree. The loader parses this same shape back into ModelEntry + source.
    let mut model_value = toml::Value::try_from(&model)?;
    let model_table = model_value
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("model '{}' did not serialize as a TOML table", model.id))?;
    model_table.insert("source".to_string(), toml::Value::try_from(&source)?);
    let mut root_table = toml::map::Map::new();
    root_table.insert("models".to_string(), toml::Value::Array(vec![model_value]));
    let root = toml::Value::Table(root_table);
    let toml = toml::to_string_pretty(&root)?;
    Ok(format!(
        "# Board-local extension for {} ({}) on {}.\n\
         # Copied from winning model '{base_id}' so existing behavior is preserved.\n\
         # Close only source-supported gaps, then update [models.source] validation/uncertainty.\n\
         # Declared gaps at preparation time: {}.\n\
         # The exact board inventory and source hashes are retained in ../inventory.json.\n\n{}",
        reference,
        value,
        board_label,
        if missing.is_empty() {
            "scope was unspecified"
        } else {
            &missing
        },
        toml
    ))
}

fn is_active_model_reference(reference: &str) -> bool {
    let prefix: String = reference
        .chars()
        .take_while(|c| !c.is_ascii_digit())
        .collect::<String>()
        .to_ascii_uppercase();
    matches!(prefix.as_str(), "U" | "IC" | "MCU")
}

fn request_prepare_approval(assume_yes: bool) -> anyhow::Result<bool> {
    let stdin_is_tty = std::io::IsTerminal::is_terminal(&std::io::stdin());
    if assume_yes {
        return prepare_approval(true, stdin_is_tty, None);
    }
    prepare_approval(false, stdin_is_tty, None)?;
    print!("Write exactly these files? [y/N] ");
    use std::io::Write;
    std::io::stdout().flush().ok();
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    prepare_approval(false, true, Some(&answer))
}

fn prepare_approval(
    assume_yes: bool,
    stdin_is_tty: bool,
    answer: Option<&str>,
) -> anyhow::Result<bool> {
    if assume_yes {
        return Ok(true);
    }
    if !stdin_is_tty {
        anyhow::bail!(
            "refusing to write without approval: stdin is not a terminal; pass --yes for an explicit scripted opt-in"
        );
    }
    Ok(matches!(
        answer
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "y" | "yes"
    ))
}

/// A model id from free text: lowercase alphanumerics and underscores.
fn sanitise_id(text: &str) -> String {
    let mut id: String = text
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    while id.contains("__") {
        id = id.replace("__", "_");
    }
    id.trim_matches('_').to_string()
}

/// Escape regex metacharacters so a literal part value matches itself.
fn escape_regex(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if "\\.^$|?*+()[]{}".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Render a string as a TOML basic string, including the surrounding quotes.
/// Model scaffolds are generated from board-provided text, so hand-quoting is
/// not safe: values such as `LM358(A)+` contain regex backslashes and values
/// copied from a BOM may contain quotes or control characters.
fn toml_quote(text: &str) -> String {
    toml::Value::String(text.to_string()).to_string()
}

/// POSIX-shell display quoting for generated, user-visible replay commands.
/// The command list is documentation rather than an executed shell script,
/// but it still must remain copy/paste safe for spaces, quotes and `$()` in a
/// board or pack path.
fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\"'\"'"))
}

#[cfg(test)]
mod tests {
    use super::box_table;
    use super::{
        draft_model_upgrade, escape_regex, prepare_approval, sanitise_id,
        scaffold_property_match_rules, toml_quote,
    };

    #[test]
    fn scaffold_keeps_exact_identity_properties_but_not_decorative_metadata() {
        let rules = scaffold_property_match_rules(&[
            (
                "KiLib Generator".into(),
                "https://supplier.example/1812L150-24MR".into(),
            ),
            ("Description".into(), "resettable fuse".into()),
            ("Manufacturer Part Number".into(), "1812L150/24MR".into()),
        ]);
        assert!(rules.contains("[models.match.properties]"), "{rules}");
        assert!(rules.contains("KiLib Generator"), "{rules}");
        assert!(rules.contains("Manufacturer Part Number"), "{rules}");
        assert!(rules.contains("1812L150/24MR"), "{rules}");
        assert!(!rules.contains("Description"), "{rules}");
    }

    #[test]
    fn prepare_decline_is_a_successful_noop_decision() {
        assert!(!prepare_approval(false, true, Some("n\n")).unwrap());
        assert!(!prepare_approval(false, true, Some("")).unwrap());
        assert!(prepare_approval(false, true, Some("yes\n")).unwrap());
    }

    #[test]
    fn prepare_requires_explicit_yes_for_non_tty() {
        let err = prepare_approval(false, false, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("stdin is not a terminal"), "{err}");
        assert!(prepare_approval(true, false, None).unwrap());
    }

    #[test]
    fn browser_extension_preserves_every_executable_bma423_table() {
        let draft = draft_model_upgrade(
            "watchy.kicad_pcb",
            "U6",
            "BMA423",
            "Package_LGA:LGA-12_2x2mm_P0.5mm",
            "Package_LGA:LGA-12_2x2mm_P0.5mm",
            &[],
            "bma423",
        )
        .unwrap();
        let parsed: hauksbee_models::schema::DbFile = toml::from_str(&draft).unwrap();
        let model = &parsed.models[0];
        assert_eq!(model.pins.get("10").map(String::as_str), Some("csb"));
        assert!(matches!(
            model.peripheral,
            Some(hauksbee_models::schema::PeripheralSpec::RegisterMap { .. })
        ));
        assert!(model.peripheral_power.is_some());
        assert!(model
            .coverage
            .implements
            .iter()
            .any(|value| value == "chip_id_read"));
        assert!(draft.contains("[models.source]"), "{draft}");
        assert!(draft.contains("required_high_roles = [\"csb\"]"), "{draft}");
        assert!(draft.contains("address_select_role = \"sdo\""), "{draft}");
    }

    /// The shared table renderer: box-drawing frame, padded columns, one line
    /// per row. Both `models resolve` and the doctor's TTY view sit on this.
    #[test]
    fn box_table_renders_a_padded_box_drawing_table() {
        let out = box_table(
            &["Name", "Status"],
            &[
                vec!["avr".to_string(), "builtin".to_string()],
                vec!["renode".to_string(), "ok".to_string()],
            ],
        );
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 6, "frame + header + rule + 2 rows:\n{out}");
        assert!(lines[0].starts_with('┌') && lines[0].ends_with('┐'));
        assert_eq!(lines[1], "│ Name   │ Status  │");
        assert!(lines[2].starts_with('├') && lines[2].contains('┼'));
        assert_eq!(lines[3], "│ avr    │ builtin │");
        assert_eq!(lines[4], "│ renode │ ok      │");
        assert!(lines[5].starts_with('└') && lines[5].ends_with('┘'));
        // Every line is equally wide, so the frame is actually a box.
        let width = lines[0].chars().count();
        assert!(lines.iter().all(|l| l.chars().count() == width), "{out}");
    }

    #[test]
    fn resolve_report_reading_order() {
        use super::{layer_rank, natural_ref_key};
        // UNRESOLVED leads, engine-fallback next, then most-user-supplied
        // layer first, builtin last.
        assert!(layer_rank("-", false) < layer_rank("engine-fallback", true));
        assert!(layer_rank("engine-fallback", true) < layer_rank("models-dir", true));
        assert!(layer_rank("models-dir", true) < layer_rank("user-dir", true));
        assert!(layer_rank("user-dir", true) < layer_rank("pack", true));
        assert!(layer_rank("pack", true) < layer_rank("builtin", true));
        // Natural reference order: R2 before R10, groups by prefix.
        assert!(natural_ref_key("R2") < natural_ref_key("R10"));
        assert!(natural_ref_key("C1") < natural_ref_key("R1"));
    }

    /// Lint a descriptor source the way `models lint` does, from the parse
    /// onwards, and hand back the printed lines plus the finding count.
    #[cfg(feature = "renode")]
    fn lint_descriptor(name: &str, src: &str) -> (Vec<String>, usize) {
        let root: toml::Value = toml::from_str(src).expect("the fixture must be TOML");
        super::soc_lint_report(std::path::Path::new(name), src, &root)
    }

    /// Every descriptor hauksbee ships must lint clean, so a future one cannot
    /// arrive broken: the sweep is the gate, not a spot check of the newest file.
    #[cfg(all(feature = "renode", feature = "qemu"))]
    #[test]
    fn every_shipped_soc_descriptor_lints_clean() {
        let mut swept = 0usize;
        for spec in hauksbee_mcu::SocConfig::builtin_specs() {
            let src = hauksbee_mcu::SocConfig::builtin_source(spec)
                .unwrap_or_else(|| panic!("{spec} is advertised but carries no source"));
            let part = spec.split_once(':').map(|(_, p)| p).unwrap_or(spec);
            let (lines, findings) = lint_descriptor(&format!("{part}.soc.toml"), src);
            assert_eq!(findings, 0, "{spec} must lint clean:\n{}", lines.join("\n"));
            // A clean descriptor prints its inspection, not just a verdict: an
            // "ok" with nothing under it would mean the read-back went silent.
            assert!(
                lines.len() > 2,
                "{spec} must print an inspection:\n{}",
                lines.join("\n")
            );
            swept += 1;
        }
        assert!(
            swept >= 9,
            "the sweep must cover every descriptor, saw {swept}"
        );
    }

    /// A minimal Renode descriptor with `{extra}` splicing in the case under
    /// test. The platform is INLINE and declares the core clock in both places
    /// the loader checks, because a `@platform` reference that declares neither
    /// is now a hard load error and every case here needs a descriptor that gets
    /// as far as the lint.
    #[cfg(feature = "renode")]
    fn soc_fixture(extra: &str) -> String {
        format!(
            r#"
[soc]
backend = "renode"
machine = "m"
platform_repl = """
using "platforms/cpus/stm32f072.repl"

nvic:
    systickFrequency: 8000000

cpu:
    PerformanceInMips: 8
"""
cpu_path = "sysbus.cpu"
frequency_hz = 8_000_000
expected_e_machine = "EM_ARM"
mcu_label = "test part"
{extra}
"#
        )
    }

    /// The two-sided pair the linter exists for. A 32-bit port with `moder`
    /// decodes only its low 16 pins, so pins 16 and up read as inputs and their
    /// edges vanish: strictly worse than no direction map at all, which at least
    /// reports every output-state change. It must be a finding.
    #[cfg(feature = "renode")]
    #[test]
    fn a_too_narrow_dir_encoding_is_a_finding_and_a_valid_descriptor_is_not() {
        let (lines, findings) = lint_descriptor(
            "wide.soc.toml",
            &soc_fixture(
                "[[soc.ports]]\nletter = \"0\"\nperipheral = \"sio\"\nodr_offset = 0x10\n\
                 width = 32\ndir = { offset = 0x20, encoding = \"moder\" }",
            ),
        );
        assert_eq!(findings, 1, "output was:\n{}", lines.join("\n"));
        let text = lines.join("\n");
        assert!(text.contains("ERROR"), "{text}");
        assert!(
            text.contains("decodes only 16 pins") && text.contains("dropped silently"),
            "the finding must say what goes wrong, not just that something did: {text}"
        );

        // The same port with an encoding that reaches 32 pins is valid, prints
        // its inspection, and produces nothing.
        let (lines, findings) = lint_descriptor(
            "wide.soc.toml",
            &soc_fixture(
                "[[soc.ports]]\nletter = \"0\"\nperipheral = \"sio\"\nodr_offset = 0x10\n\
                 width = 32\ndir = { offset = 0x20, encoding = \"dir_bits\" }",
            ),
        );
        assert_eq!(findings, 0, "output was:\n{}", lines.join("\n"));
        let text = lines.join("\n");
        assert!(text.contains("wide.soc.toml': ok (renode:wide)"), "{text}");
        assert!(
            text.contains("gpio port 0: 32 pins on sysbus.sio, output state at +0x10"),
            "the inspection must read the port back: {text}"
        );
        assert!(!text.contains("ERROR"), "{text}");
    }

    /// The linter's own two checks, the ones the loader leaves to author intent.
    /// Both are findings, because both mean the descriptor runs and reports the
    /// wrong thing.
    #[cfg(feature = "renode")]
    #[test]
    fn intent_findings_are_errors_and_absent_capabilities_are_only_notes() {
        let blank_label = soc_fixture("").replace("mcu_label = \"test part\"", "mcu_label = \"\"");
        let (lines, findings) = lint_descriptor("m.soc.toml", &blank_label);
        assert_eq!(findings, 1, "{}", lines.join("\n"));
        assert!(
            lines.join("\n").contains("`mcu_label` is empty"),
            "{lines:?}"
        );

        let (lines, findings) = lint_descriptor(
            "m.soc.toml",
            &soc_fixture(
                "[[soc.adc]]\nchannel = 2\n\
                 monitor_command = \"sysbus.adc SetDefaultValue 1650\"\n\
                 full_scale_volts = 3.3\nmax_count = 4095",
            ),
        );
        assert_eq!(findings, 1, "{}", lines.join("\n"));
        assert!(
            lines.join("\n").contains("ADC channel 2 monitor_command"),
            "{lines:?}"
        );

        // And the distinction that keeps the output worth reading: a descriptor
        // with no ADC map, no buses and no ports is USUALLY right, so it is all
        // notes and no findings.
        let (lines, findings) = lint_descriptor("m.soc.toml", &soc_fixture(""));
        assert_eq!(findings, 0, "{}", lines.join("\n"));
        let notes = lines.iter().filter(|l| l.contains("note:")).count();
        assert!(notes >= 3, "absent capabilities must be noted: {lines:?}");
        assert!(!lines.join("\n").contains("ERROR"), "{lines:?}");
    }

    /// A descriptor the loader refuses lints as exactly one finding carrying the
    /// loader's own named error, so `models lint` and a co-sim cannot disagree
    /// about whether a file is valid.
    #[cfg(feature = "renode")]
    #[test]
    fn a_loader_refusal_becomes_one_finding_naming_the_loaders_error() {
        let zero_clock = soc_fixture("").replace("frequency_hz = 8_000_000", "frequency_hz = 0");
        let (lines, findings) = lint_descriptor("m.soc.toml", &zero_clock);
        assert_eq!(findings, 1, "{}", lines.join("\n"));
        assert_eq!(
            lines.len(),
            1,
            "a refusal prints the error and nothing else"
        );
        assert!(
            lines[0].contains("ERROR") && lines[0].contains("soc.frequency_hz must not be 0"),
            "{lines:?}"
        );
    }

    #[test]
    fn scaffold_helpers_never_invent_model_facts() {
        assert_eq!(sanitise_id("R7_SR2HARU (rev.b)"), "r7_sr2haru_rev_b");
        assert_eq!(escape_regex("LM358(A)+"), "LM358\\(A\\)\\+");
    }

    #[test]
    fn scaffold_toml_quotes_keep_regex_backslashes_and_quotes_valid() {
        for value in ["LM358(A)+", "vendor\\\"part", "line\npart"] {
            let encoded = toml_quote(value);
            let parsed: toml::Value = format!("value = {encoded}")
                .parse()
                .expect("quoted string is TOML");
            assert_eq!(parsed["value"].as_str(), Some(value));
        }
    }
}
