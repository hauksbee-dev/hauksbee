//! Which model the extraction agent runs on, and that a pin map it is unsure
//! about cannot arrive looking certain.
//!
//! The codex path used to pass no `--model` at all, so the quality of a drafted
//! part depended on whatever the user's codex happened to default to, which
//! varies by plan and config. Reading a datasheet is not a cheap task: the
//! values are easy, and the pin map is where a weaker model fails, because
//! package drawings are rotated, mirrored, and labelled without numbers.

use hauksbee_models::datasheet::{codex_model, DEFAULT_CODEX_EFFORT, DEFAULT_CODEX_MODEL};

/// The env vars this module reads are process-global, so the tests that touch
/// them must not interleave.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn with_env<T>(vars: &[(&str, Option<&str>)], f: impl FnOnce() -> T) -> T {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let saved: Vec<(String, Option<String>)> = vars
        .iter()
        .map(|(k, _)| (k.to_string(), std::env::var(k).ok()))
        .collect();
    for (k, v) in vars {
        match v {
            Some(v) => unsafe { std::env::set_var(k, v) },
            None => unsafe { std::env::remove_var(k) },
        }
    }
    let out = f();
    for (k, v) in saved {
        match v {
            Some(v) => unsafe { std::env::set_var(&k, v) },
            None => unsafe { std::env::remove_var(&k) },
        }
    }
    out
}

#[test]
fn the_default_is_the_strong_model_at_high_effort() {
    with_env(
        &[
            ("HAUKSBEE_CODEX_MODEL", None),
            ("HAUKSBEE_CODEX_EFFORT", None),
        ],
        || {
            let (m, e) = codex_model(None);
            assert_eq!(m, DEFAULT_CODEX_MODEL);
            assert_eq!(e, DEFAULT_CODEX_EFFORT);
            assert_eq!(
                m, "gpt-5.6-sol",
                "the default must be pinned, not inherited"
            );
            assert_eq!(e, "high");
        },
    );
}

#[test]
fn an_explicit_choice_wins_over_everything() {
    with_env(&[("HAUKSBEE_CODEX_MODEL", Some("from-env"))], || {
        let (m, _) = codex_model(Some("from-flag"));
        assert_eq!(m, "from-flag", "--model must beat the environment");
    });
}

#[test]
fn the_environment_wins_over_the_default() {
    with_env(
        &[
            ("HAUKSBEE_CODEX_MODEL", Some("from-env")),
            ("HAUKSBEE_CODEX_EFFORT", Some("medium")),
        ],
        || {
            let (m, e) = codex_model(None);
            assert_eq!(m, "from-env");
            assert_eq!(e, "medium");
        },
    );
}

#[test]
fn an_empty_setting_is_not_a_choice() {
    // An unset variable and one set to "" both mean "I did not choose", and the
    // second is what a shell script that forgot to fill a value produces. It
    // must not send `--model ""` to codex.
    with_env(
        &[
            ("HAUKSBEE_CODEX_MODEL", Some("")),
            ("HAUKSBEE_CODEX_EFFORT", Some("   ")),
        ],
        || {
            let (m, e) = codex_model(None);
            assert_eq!(m, DEFAULT_CODEX_MODEL);
            assert_eq!(e, DEFAULT_CODEX_EFFORT);
        },
    );
    with_env(&[("HAUKSBEE_CODEX_MODEL", None)], || {
        assert_eq!(codex_model(Some("")).0, DEFAULT_CODEX_MODEL);
    });
}
