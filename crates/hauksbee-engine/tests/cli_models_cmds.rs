//! CLI contract tests for the `models` subcommand family: the help text
//! states all six resolution layers, `models extract` takes the backend flags
//! (and `-y`), and `models add` explains every accepted source form when
//! handed something it cannot install. All offline: every invocation fails or
//! returns before anything could reach an LLM backend.

use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_hauksbee")
}

fn run(args: &[&str]) -> std::process::Output {
    Command::new(bin())
        .args(args)
        // add/remove/list resolve ~/.hauksbee from HOME; point it somewhere
        // disposable so no test can touch the real store.
        .env(
            "HOME",
            std::env::temp_dir().join("hauksbee_cli_models_home"),
        )
        .output()
        .expect("hauksbee binary runs")
}

#[test]
fn models_help_states_all_six_layers() {
    let out = run(&["models", "--help"]);
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("user-config-dir=25"),
        "the layer list must include the user config dir (25):\n{text}"
    );
    for layer in [
        "builtin=0",
        "pack=10",
        "user-dir=20",
        "--models-dir=30",
        "spice=40",
    ] {
        assert!(text.contains(layer), "layer '{layer}' missing:\n{text}");
    }
}

#[test]
fn extract_accepts_short_y_for_yes() {
    // -y must parse (like the other confirm flags); the command then fails on
    // the missing PDF, proving it got past argument parsing.
    let out = run(&[
        "models",
        "extract",
        "--pdf",
        "/definitely/not/a/real.pdf",
        "--part",
        "X1",
        "-y",
    ]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("no datasheet at"),
        "-y must be accepted and the failure must be the missing PDF: {err}"
    );
}

#[test]
fn extract_backend_flag_parses_each_backend_and_rejects_garbage() {
    for backend in ["codex", "claude-code", "api"] {
        let out = run(&[
            "models",
            "extract",
            "--pdf",
            "/definitely/not/a/real.pdf",
            "--part",
            "X1",
            "--backend",
            backend,
            "-y",
        ]);
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            err.contains("no datasheet at"),
            "--backend {backend} must parse; got: {err}"
        );
    }
    let out = run(&[
        "models",
        "extract",
        "--pdf",
        "x.pdf",
        "--part",
        "X1",
        "--backend",
        "gemini",
    ]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("codex") && err.contains("claude-code") && err.contains("api"),
        "an unknown backend must list the valid ones: {err}"
    );
}

#[test]
fn extract_refuses_a_key_pasted_as_the_env_name() {
    let out = run(&[
        "models",
        "extract",
        "--pdf",
        "/definitely/not/a/real.pdf",
        "--part",
        "X1",
        "--backend",
        "api",
        "--api-key-env",
        "sk-notaname-12345",
        "-y",
    ]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("not the key itself"),
        "a pasted key must be refused before anything runs: {err}"
    );
}

#[test]
fn models_add_names_every_accepted_source_form() {
    // A path that does not exist.
    let out = run(&["models", "add", "/definitely/not/a/pack"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    for form in ["pack directory", ".tar.gz/.tgz/.tar", "git URL"] {
        assert!(err.contains(form), "'{form}' missing from: {err}");
    }

    // A file that exists but is not a tarball.
    let dir = tempfile::tempdir().unwrap();
    let stray = dir.path().join("model.toml");
    std::fs::write(&stray, "[[models]]\n").unwrap();
    let out = run(&["models", "add", stray.to_str().unwrap()]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    for form in ["pack directory", ".tar.gz/.tgz/.tar", "git URL"] {
        assert!(err.contains(form), "'{form}' missing from: {err}");
    }
}
