use std::path::{Path, PathBuf};

/// Encode a filesystem path as a complete TOML string value.
///
/// Windows paths contain backslashes (and canonical paths may carry the
/// `\\?\` prefix), so interpolating `Path::display()` between quotes creates
/// invalid TOML escape sequences. Let the TOML serializer own the quoting.
pub fn toml_path(path: &Path) -> String {
    toml::Value::String(path.to_string_lossy().into_owned()).to_string()
}

/// The compiled `hauksbee-ci` binary (Cargo sets this for the crate's tests).
#[allow(dead_code)]
pub fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_hauksbee-ci")
}

/// The `blinky` example board shipped with this crate.
#[allow(dead_code)]
pub fn blinky_board() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/boards/blinky.kicad_pcb")
}

/// The repo root, two levels up from this crate's manifest dir.
#[allow(dead_code)]
pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("repo root")
        .to_path_buf()
}

/// Decode a captured `Output`'s stderr as UTF-8.
#[allow(dead_code)]
pub fn stderr(o: &std::process::Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// Serializes every test that runs a co-sim whose SoC descriptor resolution
/// matters.
///
/// Descriptor overrides reach the runner through `HAUKSBEE_MCU_DIR`, which is
/// process-global: a test that publishes a stripped-down descriptor there is
/// visible to every other test running concurrently in this binary. Each such
/// test used to be alone in its own binary, so the isolation was structural;
/// now that they share one, it has to be taken explicitly.
///
/// Take it for the whole body of any test that either publishes a descriptor
/// dir or runs a spec expecting the stock descriptors.
#[allow(dead_code)]
pub static DESCRIPTOR_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Take [`DESCRIPTOR_ENV_LOCK`], ignoring poisoning from an unrelated test's
/// panic: the guarded resource is the environment, which a panicking test
/// leaves no worse than it found.
#[allow(dead_code)]
pub fn descriptor_env_lock() -> std::sync::MutexGuard<'static, ()> {
    DESCRIPTOR_ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
