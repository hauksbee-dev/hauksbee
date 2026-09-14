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

/// `hauksbee models backend presets`
pub fn presets() -> anyhow::Result<()> {
    let (_, resolved) = load_and_resolve()?;
    let backends = extract_config::availability(&ProcessHost, &resolved);
    let ready = |b: Backend| backends.iter().any(|s| s.backend == b && s.available);

    for p in extract_config::presets() {
        let tag = if p.recommended { "  (recommended)" } else { "" };
        let ready_tag = if ready(p.backend) {
            "installed"
        } else {
            "not installed"
        };
        println!("{:<12} {}{tag}", p.id, p.label);
        println!("             {} [{ready_tag}]", p.summary);
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
    if !yes {
        if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
            anyhow::bail!(
                "refusing to delete {} without approval: stdin is not a terminal; pass --yes \
                 for an explicit scripted opt-in",
                path.display()
            );
        }
        print!(
            "Delete {} and go back to every default? [y/N] ",
            path.display()
        );
        std::io::stdout().flush().ok();
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            println!("Nothing was deleted.");
            return Ok(());
        }
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

/// `hauksbee models backend check [--send]`
pub fn check(send: bool) -> anyhow::Result<()> {
    let (_, resolved) = load_and_resolve()?;
    print_backend_line(&resolved);

    // codex gets the login-aware detail `probe_extractors` already computes
    // for the web/CLI readiness gate; every other backend's PATH/key-env
    // check comes straight from `availability`, which is the same check
    // `resolve_settings` + `require_tool` apply before a real extraction.
    let (available, detail) = if resolved.backend == Backend::Codex {
        let statuses = crate::deps::probe_extractors();
        match statuses.iter().find(|s| s.id == "codex") {
            Some(s) => (s.present, s.detail.clone().unwrap_or_default()),
            None => (false, "codex status unavailable".to_string()),
        }
    } else {
        let rows = extract_config::availability(&ProcessHost, &resolved);
        match rows.iter().find(|s| s.backend == resolved.backend) {
            Some(s) => (s.available, s.detail.clone()),
            None => (false, "unknown backend".to_string()),
        }
    };
    println!(
        "  {}",
        readiness_line(resolved.backend.label(), available, &detail)
    );

    if !available {
        anyhow::bail!(
            "{} is not ready: {detail}\nFix it ({}), or switch backend: `hauksbee models \
             backend use <preset>` (see `models backend presets`).",
            resolved.backend.label(),
            resolved.backend.install_hint()
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

fn model_key(b: Backend) -> &'static str {
    match b {
        Backend::ClaudeCode => "claude-code.model",
        Backend::Agy => "agy.model",
        Backend::Codex => "codex.model",
        Backend::Api => "api.model",
    }
}

fn effort_key(b: Backend) -> Option<&'static str> {
    match b {
        Backend::ClaudeCode => Some("claude-code.effort"),
        Backend::Agy => Some("agy.effort"),
        Backend::Codex => Some("codex.effort"),
        Backend::Api => None,
    }
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
        let installed = backends
            .iter()
            .any(|s| s.backend == p.backend && s.available);
        let status = if installed {
            "installed".to_string()
        } else {
            format!("not installed: {}", p.backend.install_hint())
        };
        println!("  {}. {} \u{2014} {} ({status})", i + 1, p.label, p.summary);
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
        loaded.config.set(model_key(chosen.backend), &model)?;
    }

    if let Some(key) = effort_key(chosen.backend) {
        let allowed = allowed_efforts(chosen.backend).join(", ");
        let effort = prompt_line(&format!("Effort [{}] ({allowed}): ", chosen.effort))?;
        if !effort.is_empty() {
            loaded.config.set(key, &effort)?;
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
    fn a_preset_id_wins_over_a_same_named_backend() {
        // "claude-code", "agy" and "codex" are all preset ids AND backend
        // names; the preset (which also seeds model/effort) must win.
        assert_eq!(
            classify_use_target("claude-code"),
            Some(UseTarget::Preset("claude-code"))
        );
        assert_eq!(classify_use_target("agy"), Some(UseTarget::Preset("agy")));
        assert_eq!(
            classify_use_target("codex"),
            Some(UseTarget::Preset("codex"))
        );
    }

    #[test]
    fn a_bare_backend_name_with_no_matching_preset_falls_through() {
        // "api" is a backend name with no preset of the same id.
        assert_eq!(
            classify_use_target("api"),
            Some(UseTarget::Backend(Backend::Api))
        );
    }

    #[test]
    fn nonsense_targets_are_rejected() {
        assert_eq!(classify_use_target("nope"), None);
        assert_eq!(classify_use_target(""), None);
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
        let host = FakeHost {
            vars: Default::default(),
            tools: [("agy".to_string(), "/usr/local/bin/agy".into())]
                .into_iter()
                .collect(),
        };
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
            assert!(known.iter().any(|k| k == model_key(b)), "{}", model_key(b));
            if let Some(k) = effort_key(b) {
                assert!(known.iter().any(|kk| kk == k), "{k}");
            }
        }
    }
}
