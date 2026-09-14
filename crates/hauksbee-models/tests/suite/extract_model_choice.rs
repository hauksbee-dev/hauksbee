//! Which model the extraction agent runs on, and how that choice is made.
//!
//! The codex path used to pass no `--model` at all, so the quality of a drafted
//! part depended on whatever the user's codex happened to default to, which
//! varies by plan and config. Reading a datasheet is not a cheap task: the
//! values are easy, and the pin map is where a weak model fails, because
//! package drawings are rotated, mirrored, and labelled without numbers.
//!
//! Every backend now goes through one resolver (flag, then environment, then
//! the config file, then the default), so these tests drive that resolver
//! against a fake host rather than the process environment.

use std::path::PathBuf;

use hauksbee_models::datasheet::{
    resolve_with, Args, Backend, DEFAULT_AGY_EFFORT, DEFAULT_AGY_MODEL, DEFAULT_CLAUDE_EFFORT,
    DEFAULT_CLAUDE_MODEL, DEFAULT_CODEX_EFFORT, DEFAULT_CODEX_MODEL,
};
use hauksbee_models::extract_config::{load_from, ExtractConfig, FakeHost};

fn host(vars: &[(&str, &str)], tools: &[&str]) -> FakeHost {
    FakeHost {
        vars: vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        tools: tools
            .iter()
            .map(|t| (t.to_string(), PathBuf::from(format!("/bin/{t}"))))
            .collect(),
    }
}

fn args() -> Args {
    Args::new(PathBuf::from("x.pdf"), "PART".into(), String::new())
}

#[test]
fn every_backend_defaults_to_its_strong_model_at_high_effort() {
    let cfg = ExtractConfig::default();
    let h = host(&[], &[]);
    for (backend, model, effort) in [
        (Backend::Codex, DEFAULT_CODEX_MODEL, DEFAULT_CODEX_EFFORT),
        (
            Backend::ClaudeCode,
            DEFAULT_CLAUDE_MODEL,
            DEFAULT_CLAUDE_EFFORT,
        ),
        (Backend::Agy, DEFAULT_AGY_MODEL, DEFAULT_AGY_EFFORT),
    ] {
        let r = resolve_with(&cfg, &h, &args().backend(Some(backend)));
        assert_eq!(
            r.model(),
            model,
            "{backend}: the default must be pinned, not inherited"
        );
        assert_eq!(r.effort(), Some(effort), "{backend}");
        assert_eq!(effort, "high", "high, deliberately not max: {backend}");
    }
    assert_eq!(DEFAULT_CODEX_MODEL, "gpt-5.6-sol");
    assert_eq!(DEFAULT_CLAUDE_MODEL, "claude-opus-5");
    assert_eq!(DEFAULT_AGY_MODEL, "gemini-3.8-flash");
}

#[test]
fn an_explicit_choice_wins_over_everything() {
    let mut cfg = ExtractConfig::default();
    cfg.set("codex.model", "from-file").unwrap();
    let h = host(&[("HAUKSBEE_CODEX_MODEL", "from-env")], &[]);
    let a = args()
        .backend(Some(Backend::Codex))
        .model(Some("from-flag".into()))
        .effort(Some("low".into()));
    let r = resolve_with(&cfg, &h, &a);
    assert_eq!(
        r.model(),
        "from-flag",
        "--model must beat the environment and the file"
    );
    assert_eq!(r.effort(), Some("low"));
}

#[test]
fn the_environment_wins_over_the_file_which_wins_over_the_default() {
    let mut cfg = ExtractConfig::default();
    cfg.set("codex.model", "from-file").unwrap();
    cfg.set("codex.effort", "low").unwrap();
    cfg.set("claude-code.model", "claude-from-file").unwrap();

    let r = resolve_with(&cfg, &host(&[], &[]), &args().backend(Some(Backend::Codex)));
    assert_eq!((r.model(), r.effort()), ("from-file", Some("low")));

    let h = host(&[("HAUKSBEE_CODEX_MODEL", "from-env")], &[]);
    let r = resolve_with(&cfg, &h, &args().backend(Some(Backend::Codex)));
    assert_eq!(r.model(), "from-env", "the environment beats the file");
    assert_eq!(
        r.effort(),
        Some("low"),
        "a field the environment leaves alone still comes from the file"
    );

    let h = host(&[("HAUKSBEE_CLAUDE_EFFORT", "medium")], &[]);
    let r = resolve_with(&cfg, &h, &args().backend(Some(Backend::ClaudeCode)));
    assert_eq!(
        (r.model(), r.effort()),
        ("claude-from-file", Some("medium"))
    );
}

#[test]
fn an_empty_setting_is_not_a_choice() {
    // An unset variable and one set to "" both mean "I did not choose", and the
    // second is what a shell script that forgot to fill a value produces. It
    // must not send `--model ""` to the CLI.
    let cfg = ExtractConfig::default();
    let h = host(
        &[
            ("HAUKSBEE_CODEX_MODEL", ""),
            ("HAUKSBEE_CODEX_EFFORT", "   "),
        ],
        &[],
    );
    let r = resolve_with(&cfg, &h, &args().backend(Some(Backend::Codex)));
    assert_eq!(
        (r.model(), r.effort()),
        (DEFAULT_CODEX_MODEL, Some(DEFAULT_CODEX_EFFORT))
    );

    let a = args()
        .backend(Some(Backend::Codex))
        .model(Some("".into()))
        .effort(Some(" ".into()));
    let r = resolve_with(&cfg, &host(&[], &[]), &a);
    assert_eq!(
        (r.model(), r.effort()),
        (DEFAULT_CODEX_MODEL, Some(DEFAULT_CODEX_EFFORT))
    );
}

#[test]
fn the_backend_itself_follows_the_same_precedence() {
    let mut cfg = ExtractConfig::default();
    cfg.set("backend", "agy").unwrap();
    let everything = ["codex", "claude", "agy"];

    let r = resolve_with(&cfg, &host(&[], &everything), &args());
    assert_eq!(r.backend, Backend::Agy, "the file beats auto-detection");

    let h = host(&[("HAUKSBEE_EXTRACT_BACKEND", "claude-code")], &everything);
    let r = resolve_with(&cfg, &h, &args());
    assert_eq!(
        r.backend,
        Backend::ClaudeCode,
        "the environment beats the file"
    );

    let r = resolve_with(&cfg, &h, &args().backend(Some(Backend::Codex)));
    assert_eq!(r.backend, Backend::Codex, "--backend beats everything");

    let r = resolve_with(&ExtractConfig::default(), &host(&[], &["claude"]), &args());
    assert_eq!(
        r.backend,
        Backend::ClaudeCode,
        "nothing chosen: the installed CLI"
    );
}

#[test]
fn a_saved_file_is_read_back_into_the_same_resolution() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("extract.toml");
    std::fs::write(
        &path,
        "backend = \"claude-code\"\n\n[claude-code]\nmodel = \"claude-sonnet-5\"\n",
    )
    .unwrap();
    let loaded = load_from(&path).unwrap();
    let r = resolve_with(&loaded.config, &host(&[], &[]), &args());
    assert_eq!(r.backend, Backend::ClaudeCode);
    assert_eq!((r.model(), r.effort()), ("claude-sonnet-5", Some("high")));
    assert_eq!(r.summary(), "Claude Code (claude-sonnet-5, high effort)");
}
