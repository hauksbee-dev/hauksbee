//! `hauksbee models backend`: view and change the persistent datasheet
//! extraction settings kept in `~/.config/hauksbee/extract.toml`. The
//! contract for every field lives in
//! `crates/hauksbee-models/src/extract_config.rs`; this module is a thin CLI
//! skin over it, plus one interactive wizard (`setup`).
//!
//! Dependency-free like the rest of the CLI: no dialoguer, just stdin
//! `read_line`, the same style as the consent prompt in
//! [`crate::commands::models::extract`].

use std::io::Write;

use anyhow::Context;
use hauksbee_models::datasheet::{self, Backend};
use hauksbee_models::extract_config::{self, Loaded, Preset, ProcessHost, Resolved};

/// Load the config and resolve it against the real process environment, with
/// no `--backend` flag override (the CLI surfaces always show the saved /
/// auto-detected choice, never a hypothetical one).
fn load_and_resolve() -> anyhow::Result<(Loaded, Resolved)> {
    let loaded = extract_config::load()?;
    let resolved = loaded.config.resolve(&ProcessHost, None);
    Ok((loaded, resolved))
}

fn print_backend_line(resolved: &Resolved) {
    println!(
        "Backend: {} ({})",
        resolved.summary(),
        resolved.backend_source.describe()
    );
}

// ── show ─────────────────────────────────────────────────────────────────────

/// `hauksbee models backend show [--json]`
pub fn show(json: bool) -> anyhow::Result<()> {
    let (loaded, resolved) = load_and_resolve()?;
    let backends = extract_config::availability(&ProcessHost, &resolved);

    if json {
        let out = serde_json::json!({
            "path": loaded.path,
            "exists": loaded.exists,
            "config": loaded.config,
            "resolved": resolved,
            "backends": backends,
            "presets": extract_config::presets(),
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    println!(
        "Config file: {}{}",
        loaded.path.display(),
        if loaded.exists {
            ""
        } else {
            " (not created yet; every setting is a default)"
        }
    );
    println!();
    print_backend_line(&resolved);
    println!("  model:    {}", resolved.model());
    if let Some(effort) = resolved.effort() {
        println!("  effort:   {effort}");
    }
    println!("  retries:  {}", resolved.retries);
    println!("  timeout:  {}s", resolved.timeout_secs);
    if resolved.backend == Backend::Api {
        let set = std::env::var(&resolved.api.api_key_env).is_ok_and(|v| !v.trim().is_empty());
        println!("  base URL: {}", resolved.api.base_url);
        println!(
            "  key env:  {} ({})",
            resolved.api.api_key_env,
            if set { "set" } else { "not set" }
        );
    }
    println!();
    if resolved.env_overrides.is_empty() {
        println!("No HAUKSBEE_* environment overrides are active.");
    } else {
        println!(
            "Active environment overrides (win over the file): {}",
            resolved.env_overrides.join(", ")
        );
    }
    println!();
    println!("Availability:");
    for b in &backends {
        let mark = if b.available { "\u{2713}" } else { "\u{2717}" };
        println!("  {mark} {:<22} {}", b.label, b.detail);
        if !b.available {
            println!("      install: {}", b.install);
        }
    }
    if loaded.exists {
        println!();
        println!("{}", loaded.config.to_toml());
    }
    Ok(())
}

// ── presets / keys ───────────────────────────────────────────────────────────

fn preset_summary(preset: &Preset, resolved: &Resolved) -> String {
    if resolved.custom_command(preset.backend).is_some() {
        format!(
            "Preserves custom command; {}. Check its authentication and charges.",
            resolved.sends_data_to(preset.backend)
        )
    } else {
        preset.summary.to_string()
    }
}

/// `hauksbee models backend presets`
pub fn presets() -> anyhow::Result<()> {
    let (_, resolved) = load_and_resolve()?;
    for p in extract_config::presets() {
        let tag = if p.recommended { "  (recommended)" } else { "" };
        let ready_tag = if resolved.status(&ProcessHost, p.backend).available {
            "installed"
        } else {
            "not installed"
        };
        println!("{:<12} {}{tag}", p.id, p.label);
        println!(
            "             {} [{ready_tag}]",
            preset_summary(p, &resolved)
        );
    }
    Ok(())
}

/// `hauksbee models backend keys`
pub fn keys() -> anyhow::Result<()> {
    println!("Settings for `models backend set KEY=VALUE` / `unset KEY`:");
    println!();
    for k in extract_config::ExtractConfig::keys() {
        let default = if k.default.is_empty() {
            "(none)"
        } else {
            k.default.as_str()
        };
        println!("  {:<28} {}", k.key, k.help);
        println!("  {:<28} default: {default}", "");
    }
    Ok(())
}

// ── use ──────────────────────────────────────────────────────────────────────

/// What `use <TARGET>` resolved its argument to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UseTarget {
    Preset(&'static str),
    Backend(Backend),
}

/// A preset id wins over a bare backend name (several backend names, e.g.
/// `codex`, are also preset ids: the preset is the richer choice, so it goes
/// first).
fn classify_use_target(target: &str) -> Option<UseTarget> {
    if let Some(p) = extract_config::presets().iter().find(|p| p.id == target) {
        return Some(UseTarget::Preset(p.id));
    }
    target.parse::<Backend>().ok().map(UseTarget::Backend)
}

/// `hauksbee models backend use <PRESET-OR-BACKEND>`
pub fn use_target(target: &str) -> anyhow::Result<()> {
    let mut loaded = extract_config::load()?;
    match classify_use_target(target) {
        Some(UseTarget::Preset(id)) => {
            loaded.config.apply_preset(id)?;
        }
        Some(UseTarget::Backend(backend)) => {
            loaded.config.backend = Some(backend);
        }
        None => {
            let preset_ids: Vec<&str> = extract_config::presets().iter().map(|p| p.id).collect();
            let backend_names: Vec<&str> = Backend::ALL.iter().map(|b| b.name()).collect();
            anyhow::bail!(
                "'{target}' is neither a preset nor a backend.\nPresets: {}\nBackends: {}",
                preset_ids.join(", "),
                backend_names.join(", ")
            );
        }
    }
    extract_config::save(&loaded.config, &loaded.path)?;
    let resolved = loaded.config.resolve(&ProcessHost, None);
    print_backend_line(&resolved);
    println!("Saved to {}", loaded.path.display());
    Ok(())
}

// ── set / unset ──────────────────────────────────────────────────────────────

/// `hauksbee models backend set KEY=VALUE...` (and `unset KEY`, which calls
/// this with `"KEY="`). Stops at the first bad pair and writes nothing.
pub fn set(pairs: &[String]) -> anyhow::Result<()> {
    let mut loaded = extract_config::load()?;
    for pair in pairs {
        loaded.config.set_pair(pair).with_context(|| {
            format!(
                "'{pair}' (see `hauksbee models backend keys` for the settings and their defaults)"
            )
        })?;
    }
    extract_config::save(&loaded.config, &loaded.path)?;
    let resolved = loaded.config.resolve(&ProcessHost, None);
    print_backend_line(&resolved);
    println!("Saved to {}", loaded.path.display());
    Ok(())
}

// ── reset ────────────────────────────────────────────────────────────────────

/// `hauksbee models backend reset [--yes]`
pub fn reset(yes: bool) -> anyhow::Result<()> {
    let path = extract_config::config_path()?;
    if !path.exists() {
        println!("No config file at {} (nothing to reset).", path.display());
        return Ok(());
    }
    if !yes
        && !datasheet::confirm(
            &mut std::io::stdout(),
            &format!("Delete {} and go back to every default?", path.display()),
            &format!(
                "refusing to delete {} without approval: stdin is not a terminal; pass --yes \
                 for an explicit scripted opt-in",
                path.display()
            ),
        )?
    {
        println!("Nothing was deleted.");
        return Ok(());
    }
    std::fs::remove_file(&path).with_context(|| format!("deleting {}", path.display()))?;
    println!("Deleted {}", path.display());
    Ok(())
}

// ── path ─────────────────────────────────────────────────────────────────────

/// `hauksbee models backend path`
pub fn path() -> anyhow::Result<()> {
    println!("{}", extract_config::config_path()?.display());
    Ok(())
}

// ── check ────────────────────────────────────────────────────────────────────

/// One line describing whether a backend is ready, for `check` and its tests.
fn readiness_line(label: &str, available: bool, detail: &str) -> String {
    let mark = if available { "ready" } else { "not ready" };
    format!("{label}: {mark} ({detail})")
}

fn check_status(
    resolved: &Resolved,
    host: &dyn extract_config::Host,
) -> extract_config::BackendStatus {
    let mut status = resolved.status(host, resolved.backend);
    // Only native Codex uses its login probe; wrappers own authentication.
    if resolved.backend == Backend::Codex && resolved.custom_command(resolved.backend).is_none() {
        let statuses = crate::deps::probe_extractors();
        (status.available, status.detail) = match statuses.iter().find(|s| s.id == "codex") {
            Some(s) => (s.present, s.detail.clone().unwrap_or_default()),
            None => (false, "codex status unavailable".to_string()),
        };
    }
    status
}

/// `hauksbee models backend check [--send]`
pub fn check(send: bool) -> anyhow::Result<()> {
    let (_, resolved) = load_and_resolve()?;
    print_backend_line(&resolved);
    let status = check_status(&resolved, &ProcessHost);
    println!(
        "  {}",
        readiness_line(status.label, status.available, &status.detail)
    );

    if !status.available {
        anyhow::bail!(
            "{} is not ready: {}\nFix it ({}), or switch backend: `hauksbee models \
             backend use <preset>` (see `models backend presets`).",
            status.label,
            status.detail,
            status.install
        );
    }
    if !send {
        return Ok(());
    }

    println!();
    println!("Sending a one-line connectivity check...");
    let mut progress = |line: &str| println!("  \u{2026} {line}");
    let reply = datasheet::smoke_test(&resolved, &mut progress)?;
    println!("Reply: {reply}");
    Ok(())
}

// ── setup ────────────────────────────────────────────────────────────────────

/// The preset `setup` offers as the default choice: the first *recommended*
/// preset whose backend is already installed, else the first recommended
/// preset, else the first preset of any kind (there is always at least one).
fn default_setup_choice(backends: &[extract_config::BackendStatus]) -> &'static Preset {
    let installed = |b: Backend| backends.iter().any(|s| s.backend == b && s.available);
    let all = extract_config::presets();
    all.iter()
        .find(|p| p.recommended && installed(p.backend))
        .or_else(|| all.iter().find(|p| p.recommended))
        .unwrap_or(&all[0])
}

fn model_key(b: Backend) -> String {
    format!("{}.model", b.name())
}

fn effort_key(b: Backend) -> Option<String> {
    b.tool().map(|_| format!("{}.effort", b.name()))
}

fn allowed_efforts(b: Backend) -> &'static [&'static str] {
    extract_config::agent_spec(b).map_or(&[], |a| a.efforts)
}

fn prompt_line(label: &str) -> anyhow::Result<String> {
    print!("{label}");
    std::io::stdout().flush().ok();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(line.trim().to_string())
}

/// `hauksbee models backend setup`: an interactive wizard. Every prompt shows
/// its default in brackets; pressing enter keeps it.
pub fn setup() -> anyhow::Result<()> {
    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        anyhow::bail!(
            "models backend setup is interactive and stdin is not a terminal. Use \
             `hauksbee models backend use <preset>` or `set KEY=VALUE` instead \
             (see `models backend keys` / `models backend presets`)."
        );
    }

    let mut loaded = extract_config::load()?;
    let resolved_now = loaded.config.resolve(&ProcessHost, None);
    let backends = extract_config::availability(&ProcessHost, &resolved_now);

    println!("Agent CLIs on this machine:");
    for b in &backends {
        if b.backend.tool().is_some() {
            let mark = if b.available { "\u{2713}" } else { "\u{2717}" };
            println!("  {mark} {}", b.label);
        }
    }
    println!();

    let all = extract_config::presets();
    let default = default_setup_choice(&backends);
    println!("Presets:");
    for (i, p) in all.iter().enumerate() {
        let status = resolved_now.status(&ProcessHost, p.backend);
        let ready = if status.available {
            "installed".to_string()
        } else {
            format!("not installed: {}", status.install)
        };
        println!(
            "  {}. {} \u{2014} {} ({ready})",
            i + 1,
            p.label,
            preset_summary(p, &resolved_now)
        );
    }
    println!();

    let choice = prompt_line(&format!("Choice [{}]: ", default.id))?;
    let chosen: &Preset = if choice.is_empty() {
        default
    } else if let Ok(n) = choice.parse::<usize>() {
        all.get(n.saturating_sub(1))
            .with_context(|| format!("no preset numbered {n}"))?
    } else {
        all.iter()
            .find(|p| p.id == choice)
            .with_context(|| format!("unknown preset '{choice}' (see `models backend presets`)"))?
    };

    loaded.config.apply_preset(chosen.id)?;

    let model = prompt_line(&format!("Model [{}]: ", chosen.model))?;
    if !model.is_empty() {
        loaded.config.set(&model_key(chosen.backend), &model)?;
    }

    if let Some(key) = effort_key(chosen.backend) {
        let allowed = allowed_efforts(chosen.backend).join(", ");
        let effort = prompt_line(&format!("Effort [{}] ({allowed}): ", chosen.effort))?;
        if !effort.is_empty() {
            loaded.config.set(&key, &effort)?;
        }
    } else {
        let base = prompt_line(&format!("Base URL [{}]: ", chosen.api_base))?;
        if !base.is_empty() {
            loaded.config.set("api.base_url", &base)?;
        }
        let key_env = prompt_line(&format!("Key env var NAME [{}]: ", chosen.api_key_env))?;
        if !key_env.is_empty() {
            loaded.config.set("api.api_key_env", &key_env)?;
        }
    }

    extract_config::save(&loaded.config, &loaded.path)?;
    let resolved = loaded.config.resolve(&ProcessHost, None);
    println!();
    println!("Saved to {}", loaded.path.display());
    print_backend_line(&resolved);
    println!(
        "Terminal equivalent: hauksbee models backend use {}",
        chosen.id
    );
    println!();

    let run_check = prompt_line("Run a connectivity check now? [y/N] ")?;
    if matches!(run_check.to_ascii_lowercase().as_str(), "y" | "yes") {
        let mut progress = |line: &str| println!("  \u{2026} {line}");
        match datasheet::smoke_test(&resolved, &mut progress) {
            Ok(reply) => println!("Reply: {reply}"),
            Err(e) => println!("Connectivity check failed: {e:#}"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use extract_config::FakeHost;

    #[test]
    fn use_targets_prefer_presets_then_bare_backends_and_reject_unknowns() {
        for (target, expected) in [
            ("claude-code", Some(UseTarget::Preset("claude-code"))),
            ("agy", Some(UseTarget::Preset("agy"))),
            ("codex", Some(UseTarget::Preset("codex"))),
            ("api", Some(UseTarget::Backend(Backend::Api))),
            ("nope", None),
            ("", None),
        ] {
            assert_eq!(classify_use_target(target), expected, "{target}");
        }
    }

    #[test]
    fn custom_presets_and_checks_preserve_wrapper_routing_without_native_fallback() {
        let mut host = FakeHost::default();
        let mut config = extract_config::ExtractConfig::default();
        for preset in extract_config::presets()
            .iter()
            .filter(|p| p.backend.tool().is_some())
        {
            let backend = preset.backend;
            let resolved = config.resolve(&host, Some(backend));
            assert_eq!(preset_summary(preset, &resolved), preset.summary);
            config
                .set(&format!("{}.command", backend.name()), "custom-wrapper")
                .unwrap();
            config.apply_preset(preset.id).unwrap();
            for available in [false, true] {
                let tool = if available {
                    "custom-wrapper"
                } else {
                    backend.tool().unwrap()
                };
                host.tools = [(tool.into(), "/tmp/agent".into())].into();
                let resolved = config.resolve(&host, Some(backend));
                let summary = preset_summary(preset, &resolved);
                assert_eq!(resolved.custom_command(backend), Some("custom-wrapper"));
                assert!(summary.contains(&resolved.sends_data_to(backend)));
                assert!(!summary.contains(preset.summary));
                let status = check_status(&resolved, &host);
                assert_eq!(status, resolved.status(&host, backend));
                assert_eq!(status.available, available);
            }
            config
                .set(&format!("{}.command", backend.name()), "")
                .unwrap();
        }
    }

    #[test]
    fn readiness_line_names_the_backend_and_the_detail() {
        let ok = readiness_line("Codex", true, "logged in");
        assert!(ok.contains("Codex"), "{ok}");
        assert!(ok.contains("ready"), "{ok}");
        assert!(ok.contains("logged in"), "{ok}");

        let bad = readiness_line("Codex", false, "codex not found on PATH");
        assert!(bad.contains("not ready"), "{bad}");
        assert!(bad.contains("codex not found on PATH"), "{bad}");
    }

    #[test]
    fn default_setup_choice_prefers_a_recommended_installed_preset() {
        // Only agy installed: agy (recommended) should win over claude-code
        // and codex, which are also recommended but not installed.
        let host = FakeHost::new(&[], &["agy"]);
        let cfg = extract_config::ExtractConfig::default();
        let resolved = cfg.resolve(&host, None);
        let backends = extract_config::availability(&host, &resolved);
        let chosen = default_setup_choice(&backends);
        assert_eq!(chosen.backend, Backend::Agy);
        assert!(chosen.recommended);
    }

    #[test]
    fn default_setup_choice_falls_back_when_nothing_is_installed() {
        let host = FakeHost::default();
        let cfg = extract_config::ExtractConfig::default();
        let resolved = cfg.resolve(&host, None);
        let backends = extract_config::availability(&host, &resolved);
        let chosen = default_setup_choice(&backends);
        assert!(
            chosen.recommended,
            "falls back to a recommended preset even if uninstalled"
        );
    }

    #[test]
    fn model_and_effort_keys_match_extract_configs_own_key_names() {
        let known: Vec<String> = extract_config::ExtractConfig::keys()
            .into_iter()
            .map(|k| k.key)
            .collect();
        for b in Backend::ALL {
            assert!(known.contains(&model_key(b)), "{}", model_key(b));
            if let Some(k) = effort_key(b) {
                assert!(known.contains(&k), "{k}");
            }
        }
    }
}
