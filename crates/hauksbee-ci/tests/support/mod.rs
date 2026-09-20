use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Write `body` to a fresh temp file named `name` and return its path.
///
/// Every integration module compiles into the same `it` binary and cargo
/// runs its tests concurrently, so the destination directory is unique per
/// call (process id + a monotonic counter) rather than shared by name: two
/// tests writing the same `name` (even across different modules) must never
/// collide on one path.
#[allow(dead_code)]
pub fn write_tmp(name: &str, body: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("hauksbee_ci_tests_{}_{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join(name);
    std::fs::write(&p, body).unwrap();
    p
}

/// One board-DSL pad: number `n` on net `net`, placed at x = `n` so that no two
/// lands of a part coincide.
pub fn pad(n: u32, net: &str) -> String {
    format!("pad \"{n}\" smd rect at {n} 0 size 2 1.5 layers [F.Cu F.Mask] net \"{net}\"\n")
}

/// One board-DSL component: reference `r`, footprint `lib`, value `val`, placed
/// at x = `x`, carrying `pads` (built from [`pad`]).
pub fn comp(r: &str, lib: &str, val: &str, x: usize, pads: String) -> String {
    format!("comp {r} lib \"{lib}\" val \"{val}\" layer \"F.Cu\" at {x} 20 rot 0 {{\n{pads}}}\n")
}

/// Write `body` to a spec file named `name` and run it with default config.
#[allow(dead_code)]
pub fn run_body(name: &str, body: &str) -> hauksbee_ci::CiResult {
    hauksbee_ci::run(&run_config(write_tmp(name, body))).expect("spec runs")
}

/// A [`hauksbee_ci::CiResult`] carrying `results` and nothing else: spec "t" on
/// board "b.kicad_pcb", one seed, no elapsed time, and every honesty list empty.
///
/// The report surfaces are judged from synthesized results in several modules,
/// and each one needs the same otherwise-empty carrier around them.
pub fn ci_result(
    results: Vec<hauksbee_ci::assertions::AssertResult>,
    analog_abort: bool,
) -> hauksbee_ci::CiResult {
    hauksbee_ci::CiResult {
        spec_name: "t".to_string(),
        board: "b.kicad_pcb".to_string(),
        results,
        seeds: 1,
        elapsed: std::time::Duration::from_secs(0),
        analog_abort,
        coverage: None,
        substitutions: Vec::new(),
        coverage_warnings: Vec::new(),
        timing_coverage: Vec::new(),
        timing_refusals: Vec::new(),
        dead_rails: Vec::new(),
        waiver_notes: Vec::new(),
        inventory: Vec::new(),
        assumptions: Vec::new(),
        evidence: Vec::new(),
    }
}

/// A committed example board/spec under this crate's `examples/` directory.
#[allow(dead_code)]
pub fn example(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("examples")
        .join(name)
}

/// A path under the repository root, [`repo_root`] plus `rel`.
#[allow(dead_code)]
pub fn repo(rel: &str) -> PathBuf {
    repo_root().join(rel)
}

/// The directory the board corpus sits directly under, whichever layout this
/// machine has (the hand-built `famous/<id>` tree or the flat `<id>` layout
/// `scripts/fetch-corpus.sh` produces), or an empty default if the corpus is
/// absent.
#[allow(dead_code)]
pub fn corpus_boards_root() -> PathBuf {
    hauksbee_testkit::corpus_boards_root(env!("CARGO_MANIFEST_DIR")).unwrap_or_default()
}

/// Whether an absent corpus board must hard-fail rather than skip.
#[allow(dead_code)]
pub fn require_corpus() -> bool {
    std::env::var("HAUKSBEE_REQUIRE_CORPUS").is_ok()
}

/// Encode a filesystem path as a complete TOML string value.
///
/// Windows paths contain backslashes (and canonical paths may carry the
/// `\\?\` prefix), so interpolating `Path::display()` between quotes creates
/// invalid TOML escape sequences. Let the TOML serializer own the quoting.
pub fn toml_path(path: &Path) -> String {
    toml::Value::String(path.to_string_lossy().into_owned()).to_string()
}

/// A run configuration with only the spec path overridden.
// Some integration modules compile this shared support file without using every helper.
#[allow(dead_code)]
pub fn run_config(spec: PathBuf) -> hauksbee_ci::RunConfig {
    hauksbee_ci::RunConfig {
        spec,
        ..Default::default()
    }
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
