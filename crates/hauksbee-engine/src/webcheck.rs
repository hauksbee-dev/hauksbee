//! The `/api/check` backend: run the checks the web builder composed, using
//! the REAL `hauksbee-ci` binary.
//!
//! The web checks panel builds the body of a spec (supplies, assertions,
//! duration, everything except the file paths); this module stages the
//! uploaded board and firmware in a temp dir, injects the `board`/`firmware`
//! keys, and shells the installed `hauksbee-ci run <spec> --json`. Shelling
//! the sibling binary, not reimplementing the runner, is the point: the
//! result the browser shows is byte-for-byte what a pipeline would produce,
//! so the web panel can never drift from CI truth. Detect-don't-bundle,
//! exactly like the kicad-cli / ngspice oracles.

// This module reads check specs composed in a browser the user did not write, and `hauksbee serve`
// exposes it to a browser, so a panic here is a denial of service rather than
// a crash in a CLI. Failures must be typed errors that the caller can report.
// Test code below is exempt: an unwrap in a test is an assertion.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Reject path separators and other surprises in an uploaded file name; the
/// staged file keeps its extension (format sniffing needs it) but nothing else
/// exotic.
fn sanitize_name(name: &str, fallback: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(fallback);
    let clean: String = base
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || ".-_".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    if clean.is_empty() || clean.starts_with('.') {
        format!("{fallback}{clean}")
    } else {
        clean
    }
}

/// The `hauksbee-ci` binary to run: `HAUKSBEE_CI_BIN` override, the sibling of
/// the current executable (the install layout), then PATH.
fn ci_binary() -> PathBuf {
    if let Ok(p) = std::env::var("HAUKSBEE_CI_BIN") {
        return PathBuf::from(p);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let sibling = dir.join("hauksbee-ci");
            if sibling.is_file() {
                return sibling;
            }
        }
    }
    PathBuf::from("hauksbee-ci")
}

fn err_json(msg: &str) -> String {
    serde_json::to_string(&serde_json::json!({ "ok": false, "error": msg }))
        .unwrap_or_else(|_| "{\"ok\":false,\"error\":\"internal error\"}".to_string())
}

/// Every spec key that names a file on disk. `board`/`firmware` are filled in
/// server-side from the uploads; the rest would let a raw fragment read
/// (`bom`, `placement`, `variant`, `models_dir`, `asbuilt`, an hwtrace `trace`
/// and the data files it references, a sensor's `spec_file`) or overwrite (a
/// `vcd_sink`'s `vcd_path` reaches `File::create`)
/// paths outside the staging dir, because the CI resolver accepts absolute
/// paths and `..` traversal.
const PATH_KEYS: [&str; 11] = [
    "board",
    "firmware",
    "schematic",
    "bom",
    "placement",
    "variant",
    "models_dir",
    "asbuilt",
    "trace",
    "spec_file",
    "vcd_path",
];

/// The first path-bearing key the fragment tries to set, if any.
///
/// Two passes. The line scan matches the key as the first token of a possibly
/// indented line, the way the builder (and a hand-written spec) emits it. The
/// TOML parse then catches the same keys smuggled where a line scan cannot see
/// them: inline tables (`peripheral = [{ vcd_path = "..." }]`), dotted keys
/// (`assert.trace = "..."`), and quoted keys. A fragment that fails to parse
/// cannot smuggle a path either: hauksbee-ci parses the identical text and
/// refuses it before resolving anything.
fn forbidden_path_key(spec_fragment: &str) -> Option<&'static str> {
    for line in spec_fragment.lines() {
        let key = line
            .trim_start()
            .split(['=', ' ', '\t'])
            .next()
            .unwrap_or("");
        if let Some(hit) = PATH_KEYS.iter().copied().find(|k| *k == key) {
            return Some(hit);
        }
    }
    toml::from_str::<toml::Value>(spec_fragment)
        .ok()
        .as_ref()
        .and_then(toml_path_key)
}

fn toml_path_key(value: &toml::Value) -> Option<&'static str> {
    match value {
        toml::Value::Table(table) => table.iter().find_map(|(key, val)| {
            PATH_KEYS
                .iter()
                .copied()
                .find(|k| *k == key.as_str())
                .or_else(|| toml_path_key(val))
        }),
        toml::Value::Array(items) => items.iter().find_map(toml_path_key),
        _ => None,
    }
}

/// The deadline of a short web request that has no board to scale it by (the
/// catalogue, the schema, a lint), and the least a board's run is given. The
/// builder caps `duration_ms` client-side, but the web UI also lets a user edit
/// the spec TOML directly, so nothing client-side is trustworthy. A pathological
/// spec (huge fuzz count, a hung external emulator) must not pin the request
/// forever, and the `validate_web_limits` ceilings below stop it from getting
/// that far in the first place.
const CHECK_TIMEOUT: std::time::Duration = hauksbee_ir::progress::RUN_CEILING_FLOOR;

// ── Web-only resource ceilings ────────────────────────────────────────────────
//
// These caps apply ONLY to the web `/api/check` path, where a browser (possibly
// hand-editing the raw TOML) drives an anonymous, shared-server run. The CLI
// (`hauksbee-ci run`) has NO such limits: a campaign on a workstation is meant to
// be as long, as finely sampled, and as heavily fuzzed as the author wants. The
// web path is a quick sanity check, not a campaign, so it gets a leash the CLI
// never sees. Each cap names the real spec.rs field it bounds.

/// `duration_ms`: simulated time. 2 s of sim time is plenty for a web quick
/// check; longer runs belong on the CLI.
const MAX_WEB_DURATION_MS: f64 = 2000.0;
/// Frames per run (`duration_ms / frame_ms`): the work a cadence costs. A short
/// window may sample as finely as the CLI allows; only the product is capped.
const MAX_WEB_FRAMES: f64 = 40_000.0;
/// `[fuzz]` seeds: initial-state fuzz members, each a full co-sim run.
const MAX_WEB_FUZZ_SEEDS: i64 = 8;
/// `[ensemble]` seeds: tolerance Monte-Carlo members, each a full co-sim run.
/// (Also the natural ceiling on the tolerance ensemble that `corners` mode would
/// otherwise blow up; see the note in `validate_web_limits`.)
const MAX_WEB_ENSEMBLE_SEEDS: i64 = 16;
/// `[[tolerance]]` rule count: bounds how many tolerance rules a web spec may
/// declare. A coarse proxy for corner-mode explosion (2^n over toleranced
/// components, which we cannot expand without binding the board here).
const MAX_WEB_TOLERANCE_RULES: usize = 16;
/// `[[assert]]` count: total assertions evaluated per run.
const MAX_WEB_ASSERTS: usize = 64;

/// Simultaneous web check runs. Each spawns a `hauksbee-ci` child that may in
/// turn spawn emulators, so unbounded concurrency lets a handful of requests
/// exhaust the box. The CLI is single-invocation-per-shell and needs no such
/// governor.
const MAX_CONCURRENT_WEB_CHECKS: usize = 4;

/// Count of web checks currently executing. `run_web_check` is a synchronous
/// blocking call from the async axum handler (it polls a child process, it does
/// not `.await`), so a plain atomic counter is the right primitive here: a tokio
/// `Semaphore` buys nothing when no future ever yields while a slot is held.
static ACTIVE_WEB_CHECKS: AtomicUsize = AtomicUsize::new(0);

/// RAII slot in the concurrency budget. `acquire` reserves one (or returns None
/// at the cap); `Drop` releases it on EVERY exit path from `run_web_check` (each
/// early `return`, the normal return, or a panic unwinding through the frame),
/// so a slot can never leak.
struct WebCheckSlot;

impl WebCheckSlot {
    fn acquire() -> Option<Self> {
        // Optimistically claim a slot, then roll back if that put us over the
        // cap. The transient over-count is bounded by the number of racing
        // callers and is undone immediately, so no run ever proceeds above the
        // ceiling.
        let prev = ACTIVE_WEB_CHECKS.fetch_add(1, Ordering::AcqRel);
        if prev >= MAX_CONCURRENT_WEB_CHECKS {
            ACTIVE_WEB_CHECKS.fetch_sub(1, Ordering::AcqRel);
            None
        } else {
            Some(Self)
        }
    }
}

impl Drop for WebCheckSlot {
    fn drop(&mut self) {
        ACTIVE_WEB_CHECKS.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Read a TOML scalar as f64, accepting both float (`2000.0`) and integer
/// (`2000`) literals, since a hand-written spec uses either for a numeric field.
#[allow(clippy::cast_precision_loss, reason = "an integer literal read as f64")]
fn as_number(v: &toml::Value) -> Option<f64> {
    v.as_float().or_else(|| v.as_integer().map(|i| i as f64))
}

/// Enforce the web-only ceilings on a composed spec fragment BEFORE it is handed
/// to `hauksbee-ci`. Returns `Err(message)` naming the offending key and its cap.
///
/// A fragment that does NOT parse as TOML is let through (Ok): `hauksbee-ci`
/// parses the identical text and emits the real, located parse error, so we do
/// not invent a worse one. The path-key checks in `forbidden_path_key` are a
/// separate gate and still apply regardless.
fn validate_web_limits(spec_fragment: &str) -> Result<(), String> {
    let Ok(value) = toml::from_str::<toml::Value>(spec_fragment) else {
        return Ok(());
    };
    let Some(table) = value.as_table() else {
        return Ok(());
    };

    // duration_ms: total simulated time.
    if let Some(ms) = table.get("duration_ms").and_then(as_number) {
        if ms > MAX_WEB_DURATION_MS {
            return Err(format!(
                "duration_ms = {ms} exceeds the web check limit of {MAX_WEB_DURATION_MS} ms \
                 (2 seconds of simulated time). Run longer campaigns from the CLI: \
                 hauksbee-ci run <spec>"
            ));
        }
    }

    // frame_ms: sampling cadence, judged with the window it samples (100 ms when
    // the spec leaves `duration_ms` to the runner's default).
    if let Some(ms) = table.get("frame_ms").and_then(as_number) {
        let window = table.get("duration_ms").and_then(as_number);
        let frames = window.unwrap_or(100.0) / ms;
        // A zero or negative cadence is the runner's error to word, not ours.
        if ms > 0.0 && frames > MAX_WEB_FRAMES {
            return Err(format!(
                "frame_ms = {ms} makes {frames:.0} frames, over the web check limit of \
                 {MAX_WEB_FRAMES}. Shorten duration_ms or coarsen frame_ms; run denser \
                 campaigns from the CLI."
            ));
        }
    }

    // [fuzz] seeds: initial-state fuzz members.
    if let Some(seeds) = table
        .get("fuzz")
        .and_then(|f| f.get("seeds"))
        .and_then(toml::Value::as_integer)
    {
        if seeds > MAX_WEB_FUZZ_SEEDS {
            return Err(format!(
                "[fuzz] seeds = {seeds} exceeds the web check limit of {MAX_WEB_FUZZ_SEEDS}. \
                 Each seed is a full co-sim run; run heavier fuzzing from the CLI."
            ));
        }
    }

    // [ensemble] seeds: tolerance Monte-Carlo members.
    if let Some(seeds) = table
        .get("ensemble")
        .and_then(|e| e.get("seeds"))
        .and_then(toml::Value::as_integer)
    {
        if seeds > MAX_WEB_ENSEMBLE_SEEDS {
            return Err(format!(
                "[ensemble] seeds = {seeds} exceeds the web check limit of {MAX_WEB_ENSEMBLE_SEEDS}. \
                 Each seed is a full co-sim run; run larger ensembles from the CLI."
            ));
        }
    }

    // [[tolerance]] rules: bounds the tolerance ensemble (and, coarsely, corner
    // enumeration, which is 2^n over the toleranced components a rule expands to
    // once the board is bound). We cannot expand patterns without a board here,
    // so we cap the rule count as the cheap, board-free proxy.
    if let Some(len) = table
        .get("tolerance")
        .and_then(toml::Value::as_array)
        .map(Vec::len)
    {
        if len > MAX_WEB_TOLERANCE_RULES {
            return Err(format!(
                "the spec declares {len} [[tolerance]] rules, over the web check limit of \
                 {MAX_WEB_TOLERANCE_RULES}. Run wide tolerance sweeps from the CLI."
            ));
        }
    }

    // [[assert]] count.
    if let Some(len) = table
        .get("assert")
        .and_then(toml::Value::as_array)
        .map(Vec::len)
    {
        if len > MAX_WEB_ASSERTS {
            return Err(format!(
                "the spec declares {len} [[assert]] blocks, over the web check limit of \
                 {MAX_WEB_ASSERTS}. Split the check or run it from the CLI."
            ));
        }
    }

    Ok(())
}

/// How long a SIGTERM'd hauksbee-ci child gets to run its emulator reaper and
/// exit before the SIGKILL fallback.
#[cfg(unix)]
const STOP_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// Stop a timed-out `hauksbee-ci` child WITHOUT orphaning its emulators.
///
/// A bare `child.kill()` is SIGKILL, which the child cannot
/// catch, so the QEMU / Renode emulators it spawned (each in its OWN process
/// group, deliberately, see `hauksbee_mcu::children`) are orphaned and keep
/// free-running at full CPU forever. Verified live: SIGKILL of a mid-co-sim
/// `hauksbee-ci run` leaves `qemu-system-xtensa` running; SIGTERM reaps it.
/// hauksbee-ci installs `hauksbee_mcu::children::install_signal_reaper`, so a
/// SIGTERM lets it group-kill every registered emulator before dying. Repeated
/// timed-out web checks each leaking one full-speed QEMU is exactly the slow
/// starvation that ends with the whole `hauksbee serve` box unresponsive or
/// the server killed, so: TERM first, grace period, then the KILL fallback for
/// a child too wedged to run its handler.
#[allow(clippy::cast_possible_wrap, reason = "OS pids fit in pid_t")]
fn stop_child(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        // Freshly-exited child: nothing to signal (kill(2) on a reaped pid
        // could hit a recycled process; on a zombie it is harmless but
        // pointless). try_wait also reaps, making the pid check meaningful.
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
        let term_sent = std::time::Instant::now();
        while term_sent.elapsed() < STOP_GRACE {
            if matches!(child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
    // Windows (no signals), or a unix child that ignored/never reached its
    // TERM handler inside the grace period.
    let _ = child.kill();
    let _ = child.wait();
}

/// Stage the uploaded files, inject the path keys, run `hauksbee-ci --json`,
/// and relay its JSON. Every failure mode returns `{"ok":false,"error":...}`
/// so the browser always has something readable.
#[must_use]
pub fn run_web_check(
    board_name: &str,
    board_bytes: &[u8],
    firmware: Option<(&str, &[u8])>,
    spec_fragment: &str,
) -> String {
    run_web_check_with_schematic(board_name, board_bytes, firmware, None, spec_fragment)
}

/// Schematic-aware counterpart used by the shipped browser checks route.
#[must_use]
pub fn run_web_check_with_schematic(
    board_name: &str,
    board_bytes: &[u8],
    firmware: Option<(&str, &[u8])>,
    schematic: Option<(&str, &[u8])>,
    spec_fragment: &str,
) -> String {
    use hauksbee_frontdoor_api::frontdoor::{DesignUpload, NamedUpload};
    let named = |(name, bytes): (&str, &[u8])| NamedUpload {
        name: name.to_string(),
        bytes: bytes.to_vec(),
    };
    run_web_check_design(
        DesignUpload {
            board: NamedUpload {
                name: board_name.to_string(),
                bytes: board_bytes.to_vec(),
            },
            firmware: firmware.map(named),
            schematic: schematic.map(named),
            bom: None,
            placement: None,
            variant: None,
            asbuilt: None,
            models: Vec::new(),
        },
        spec_fragment,
    )
}

/// Complete browser Checks path. The uploaded companions are staged inside the
/// same confined working directory and injected into the portable spec using
/// the canonical CLI keys.
#[must_use]
#[allow(clippy::needless_pass_by_value, reason = "the DesignCheckRunner type")]
#[allow(clippy::too_many_lines, reason = "stage every upload, then run once")]
pub fn run_web_check_design(
    upload: hauksbee_frontdoor_api::frontdoor::DesignUpload,
    spec_fragment: &str,
) -> String {
    // The catalogue needs no board and no staging: relay `hauksbee-ci explain --json`.
    if spec_fragment.trim() == hauksbee_frontdoor_api::frontdoor::CATALOG_SPEC {
        return run_ci_json(&["explain".as_ref(), "--json".as_ref()]);
    }
    // The editor's schema and lint need no board and no slot either.
    if spec_fragment.trim() == hauksbee_frontdoor_api::frontdoor::SCHEMA_SPEC {
        return run_ci_json(&["explain".as_ref(), "--schema".as_ref()]);
    }
    if let Some(body) = spec_fragment
        .trim_start()
        .strip_prefix(hauksbee_frontdoor_api::frontdoor::LINT_SPEC)
    {
        return lint_buffer(body.split_once('\n').map_or("", |(_, rest)| rest));
    }
    let measure = match measure_request(spec_fragment) {
        Ok(m) => m,
        Err(msg) => return err_json(&msg),
    };
    // Reserve a concurrency slot on entry. At the cap, refuse fast rather than
    // pile another emulator-spawning child onto a loaded box. `_slot` lives to
    // the end of the function, and its `Drop` releases the slot on every exit
    // path below (each early return, the normal return, or a panic).
    let Some(_slot) = WebCheckSlot::acquire() else {
        return err_json(
            "server busy: too many checks are running right now, try again in a moment",
        );
    };

    // The client composes everything EXCEPT the file paths; a fragment that
    // smuggles any path-bearing key would silently point the run at (or, for
    // `vcd_path`, write over) files outside the staging dir, so refuse it
    // loudly and name the key.
    if let Some(key) = forbidden_path_key(spec_fragment) {
        return err_json(&format!(
            "the spec body must not set `{key}`: path-bearing keys are filled in \
             from the uploaded files or are not available from the web panel"
        ));
    }

    // Enforce the web-only ceilings (duration, cadence, fuzz/ensemble seeds,
    // tolerance and assertion volume) before spending anything on staging or on
    // spawning hauksbee-ci. The CLI keeps full power; only this path is leashed.
    if let Err(msg) = validate_web_limits(spec_fragment) {
        return err_json(&msg);
    }

    // A reading already taken for these exact inputs is replayed, not re-run.
    let cache_key = measure.and_then(|index| measure_cache_key(&upload, spec_fragment, index));
    if let Some(hit) = cache_key.as_deref().and_then(measure_cache_get) {
        return hit;
    }

    // A fresh unpredictable staging dir (created 0700 with O_EXCL semantics),
    // not a guessable PID-plus-counter path under /tmp that a local attacker
    // could pre-create as a symlink and redirect the writes below. The guard
    // must stay alive until after the child exits: hauksbee-ci reads the
    // staged files while it runs. It is cleaned up when this function returns.
    let staging = match tempfile::TempDir::new() {
        Ok(d) => d,
        Err(e) => return err_json(&format!("could not create a working dir: {e}")),
    };
    let dir = staging.path();

    let board_file = sanitize_name(&upload.board.name, "board.kicad_pcb");
    if let Err(e) = std::fs::write(dir.join(&board_file), &upload.board.bytes) {
        return err_json(&format!("could not stage the board: {e}"));
    }
    let mut spec = format!("board = \"{board_file}\"\n");
    if let Some(firmware) = &upload.firmware {
        let fw_file = sanitize_name(&firmware.name, "firmware.elf");
        if let Err(e) = std::fs::write(dir.join(&fw_file), &firmware.bytes) {
            return err_json(&format!("could not stage the firmware: {e}"));
        }
        let _ = writeln!(spec, "firmware = \"{fw_file}\"");
    }
    if let Some(schematic) = &upload.schematic {
        let schematic_file = sanitize_name(&schematic.name, "board.sch");
        if let Err(e) = std::fs::write(dir.join(&schematic_file), &schematic.bytes) {
            return err_json(&format!("could not stage the schematic: {e}"));
        }
        let _ = writeln!(spec, "schematic = \"{schematic_file}\"");
    }
    for (key, fallback, file) in [
        ("bom", "bom.csv", upload.bom.as_ref()),
        ("placement", "placement.csv", upload.placement.as_ref()),
        ("variant", "variant.toml", upload.variant.as_ref()),
        ("asbuilt", "asbuilt.toml", upload.asbuilt.as_ref()),
    ] {
        if let Some(file) = file {
            let leaf = sanitize_name(&file.name, fallback);
            if let Err(error) = std::fs::write(dir.join(&leaf), &file.bytes) {
                return err_json(&format!("could not stage {key}: {error}"));
            }
            let _ = writeln!(spec, "{key} = \"{leaf}\"");
        }
    }
    if !upload.models.is_empty() {
        let model_dir = dir.join("models");
        if let Err(error) = std::fs::create_dir(&model_dir) {
            return err_json(&format!("could not stage model directory: {error}"));
        }
        let mut names = std::collections::BTreeSet::new();
        for (index, model) in upload.models.iter().enumerate() {
            let leaf = sanitize_name(&model.name, &format!("model-{index}.toml"));
            if !leaf.to_ascii_lowercase().ends_with(".toml") {
                return err_json(&format!("model file '{}' is not TOML", model.name));
            }
            if !names.insert(leaf.to_ascii_lowercase()) {
                return err_json(&format!("duplicate model filename '{leaf}'"));
            }
            if let Err(error) = std::fs::write(model_dir.join(&leaf), &model.bytes) {
                return err_json(&format!("could not stage model '{leaf}': {error}"));
            }
        }
        spec.push_str("models_dir = \"models\"\n");
    }
    spec.push('\n');
    spec.push_str(spec_fragment);
    let spec_path = dir.join("spec.toml");
    if let Err(e) = std::fs::write(&spec_path, &spec) {
        return err_json(&format!("could not stage the spec: {e}"));
    }

    // A spec body that is only the scaffold marker asks for the board's
    // starter supplies instead of a run.
    if spec_fragment.trim() == hauksbee_frontdoor_api::frontdoor::SCAFFOLD_SPEC {
        let board_path = dir.join(&board_file);
        return run_ci_json_watched(&["init".as_ref(), board_path.as_os_str(), "--json".as_ref()]);
    }
    if let Some(index) = measure {
        let index = index.to_string();
        let reading = run_ci_json_within(
            &[
                "measure".as_ref(),
                spec_path.as_os_str(),
                "--assertion".as_ref(),
                index.as_ref(),
            ],
            MEASURE_TIMEOUT,
        );
        if let Some(key) = cache_key {
            measure_cache_put(key, &reading);
        }
        return reading;
    }
    run_with_exports(dir, &spec_path)
}

/// The run, with the CLI also writing its `JUnit` and reproduction manifest; the
/// answer carries their bytes (and the JSON line itself) under `exports`, which
/// the server keeps for the export route. A run whose manifest cannot be
/// captured is run again without the extra files rather than lost.
fn run_with_exports(dir: &std::path::Path, spec_path: &std::path::Path) -> String {
    let junit = dir.join("results.xml");
    let manifest = dir.join("manifest.json");
    let line = run_ci_json_watched(&[
        "run".as_ref(),
        spec_path.as_os_str(),
        "--json".as_ref(),
        "--junit".as_ref(),
        junit.as_os_str(),
        "--emit-manifest".as_ref(),
        manifest.as_os_str(),
    ]);
    let Ok(serde_json::Value::Object(mut answer)) = serde_json::from_str(&line) else {
        return line;
    };
    let Ok(junit_xml) = std::fs::read_to_string(&junit) else {
        if line.contains("run manifest") {
            return run_ci_json_watched(&[
                "run".as_ref(),
                spec_path.as_os_str(),
                "--json".as_ref(),
            ]);
        }
        return line;
    };
    answer.insert(
        "exports".into(),
        serde_json::json!({
            "json": line,
            "junit": junit_xml,
            "manifest": std::fs::read_to_string(&manifest).ok(),
        }),
    );
    serde_json::Value::Object(answer).to_string()
}

/// How long a measurement may run before it is stopped: it is one nominal run, so a
/// minute of waiting means something is wrong.
const MEASURE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Lint the editor's buffer: write it to a private file and relay `hauksbee-ci lint`.
fn lint_buffer(toml: &str) -> String {
    let dir = match tempfile::TempDir::new() {
        Ok(d) => d,
        Err(e) => return err_json(&format!("could not create a working dir: {e}")),
    };
    let path = dir.path().join("buffer.toml");
    if let Err(e) = std::fs::write(&path, toml) {
        return err_json(&format!("could not stage the buffer: {e}"));
    }
    run_ci_json(&["lint".as_ref(), path.as_os_str()])
}

/// The assertion index a measurement is for: `# hauksbee:measure N` on the first line.
/// `Ok(None)` when the fragment is not a measurement request.
fn measure_request(spec_fragment: &str) -> Result<Option<usize>, String> {
    let Some(rest) = spec_fragment
        .trim_start()
        .strip_prefix(hauksbee_frontdoor_api::frontdoor::MEASURE_SPEC)
    else {
        return Ok(None);
    };
    let first = rest.lines().next().unwrap_or("").trim();
    first.parse().map(Some).map_err(|_| {
        format!("a measurement names the assertion to read, `# hauksbee:measure N`; got '{first}'")
    })
}

/// How many readings are kept; a full cache is emptied, not grown.
const MEASURE_CACHE_CAP: usize = 64;

type ReadingCache = std::sync::Mutex<std::collections::HashMap<String, String>>;

fn measure_cache() -> &'static ReadingCache {
    static CACHE: std::sync::OnceLock<ReadingCache> = std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// What a reading depends on, hashed: this build, every uploaded byte, the spec
/// without its assertions, and the one assertion without its bounds (a reading
/// never depends on a bound, so editing one must not re-run the simulation).
fn measure_cache_key(
    upload: &hauksbee_frontdoor_api::frontdoor::DesignUpload,
    spec_fragment: &str,
    index: usize,
) -> Option<String> {
    use sha2::{Digest, Sha256};
    let mut table = toml::from_str::<toml::Table>(spec_fragment).ok()?;
    let mut chosen = table
        .remove("assert")?
        .as_array()?
        .get(index)?
        .as_table()?
        .clone();
    for bound in ["min", "max", "amps", "freq_hz", "min_toggles", "tolerance"] {
        chosen.remove(bound);
    }
    let mut hash = Sha256::new();
    hash.update(env!("CARGO_PKG_VERSION").as_bytes());
    let files = std::iter::once(&upload.board)
        .chain(upload.firmware.iter())
        .chain(upload.schematic.iter())
        .chain(upload.bom.iter())
        .chain(upload.placement.iter())
        .chain(upload.variant.iter())
        .chain(upload.asbuilt.iter())
        .chain(upload.models.iter());
    for file in files {
        hash.update(b"\0file\0");
        hash.update(file.name.as_bytes());
        hash.update(b"\0");
        hash.update(&file.bytes);
    }
    hash.update(b"\0spec\0");
    hash.update(table.to_string().as_bytes());
    hash.update(b"\0assert\0");
    hash.update(chosen.to_string().as_bytes());
    Some(crate::run_manifest::bytes_to_hex(&hash.finalize()))
}

/// A cached reading, marked `cached` with the time the lookup took.
fn measure_cache_get(key: &str) -> Option<String> {
    let started = std::time::Instant::now();
    let stored = measure_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(key)
        .cloned()?;
    let mut reading: serde_json::Value = serde_json::from_str(&stored).ok()?;
    reading["cached"] = true.into();
    reading["t_ms"] = (started.elapsed().as_secs_f64() * 1000.0).into();
    Some(reading.to_string())
}

/// Keep a successful reading; a refusal is never cached (the inputs may be fixed
/// without changing a byte that the key sees, such as an emulator coming up).
fn measure_cache_put(key: String, reading: &str) {
    if !reading.contains("\"ok\":true") {
        return;
    }
    let mut cache = measure_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if cache.len() >= MEASURE_CACHE_CAP {
        cache.clear();
    }
    cache.insert(key, reading.to_string());
}

/// Spawn `hauksbee-ci <args>` (which prints one JSON object), drain it under
/// the check deadline, and relay that object, or an `{"ok":false,...}` that
/// carries the real failure.
fn run_ci_json(args: &[&std::ffi::OsStr]) -> String {
    run_ci_json_within(args, CHECK_TIMEOUT)
}

/// [`run_ci_json`] with its own deadline.
fn run_ci_json_within(args: &[&std::ffi::OsStr], timeout: std::time::Duration) -> String {
    run_ci(args, Deadline::Fixed(timeout))
}

/// A run of a board (the scaffold, the checks): the child reports how far it has
/// got, that is passed to whoever is watching this thread
/// ([`hauksbee_frontdoor_api::frontdoor::with_run_watch`]), and the run is
/// stopped at a ceiling scaled to the work it reports rather than at a fixed
/// time, or as soon as the watcher goes away.
fn run_ci_json_watched(args: &[&std::ffi::OsStr]) -> String {
    run_ci(args, Deadline::Scaled)
}

/// The environment variable an operator sets to cap a board's web run, in whole
/// seconds: a shared server that wants runs shorter than the scaled ceiling says
/// so here, and the stopped run's message still names where it got to.
const RUN_LIMIT_ENV: &str = "HAUKSBEE_CHECK_RUN_LIMIT_SECS";

fn operator_limit() -> Option<std::time::Duration> {
    std::env::var(RUN_LIMIT_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .map(std::time::Duration::from_secs)
}

/// How long a child may run.
#[derive(Clone, Copy)]
enum Deadline {
    /// This long from its start, whatever it says.
    Fixed(std::time::Duration),
    /// The ceiling its own progress reports give (`hauksbee_ir::progress::Heard::ceiling`),
    /// which is the floor until it has said how big its board is.
    Scaled,
}

/// What the stderr reader hands the waiting loop: one progress line as the
/// child wrote it, and as read.
type ProgressLine = (String, hauksbee_ir::progress::Heard);

/// Read the child's stderr to its end, passing each progress line on and keeping
/// every other byte (the failure tail is made from what remains).
fn drain_stderr(
    stream: Option<std::process::ChildStderr>,
    progress: Option<std::sync::mpsc::Sender<ProgressLine>>,
) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut kept = Vec::new();
        let Some(stream) = stream else {
            return kept;
        };
        let mut lines = std::io::BufReader::new(stream);
        let mut line = Vec::new();
        while matches!(std::io::BufRead::read_until(&mut lines, b'\n', &mut line), Ok(n) if n > 0) {
            let text = String::from_utf8_lossy(&line);
            match (&progress, hauksbee_ir::progress::Heard::from_json(&text)) {
                (Some(tx), Some(heard)) => {
                    let _ = tx.send((text.trim().to_string(), heard));
                }
                _ => kept.extend_from_slice(&line),
            }
            line.clear();
        }
        kept
    })
}

fn run_ci(args: &[&std::ffi::OsStr], deadline: Deadline) -> String {
    let bin = ci_binary();
    let watched = matches!(deadline, Deadline::Scaled);
    let mut command = std::process::Command::new(&bin);
    command
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if watched {
        command.env(hauksbee_ir::progress::STREAM_ENV, "1");
    }
    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return err_json(&format!(
                "hauksbee-ci was not found (looked for '{}'). It installs alongside \
                 hauksbee; re-run scripts/install.sh, or set HAUKSBEE_CI_BIN.",
                bin.display()
            ));
        }
        Err(e) => return err_json(&format!("could not run hauksbee-ci: {e}")),
    };

    // Drain both pipes WHILE the child runs, on their own threads. Piped
    // stdout/stderr hold 64 KiB each; a run report grew past that (the
    // evidence spine alone can be several hundred KiB of JSON), the child
    // blocked in write(), try_wait never saw an exit, and every web check
    // "timed out" against a child that was only waiting for someone to read.
    let stdout_thread = child.stdout.take().map(|mut s| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = std::io::Read::read_to_end(&mut s, &mut buf);
            buf
        })
    });
    let (tx, rx) = std::sync::mpsc::channel::<ProgressLine>();
    let stderr_thread = drain_stderr(child.stderr.take(), watched.then_some(tx));

    // Poll-wait with a deadline instead of output(): a hung run must produce
    // an error the browser can show, not an eternally pending request.
    let started = std::time::Instant::now();
    let operator_cap = operator_limit();
    let mut latest: Option<hauksbee_ir::progress::Heard> = None;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                while let Ok((raw, heard)) = rx.try_recv() {
                    hauksbee_frontdoor_api::frontdoor::report_progress(&raw);
                    latest = Some(heard);
                }
                if watched && hauksbee_frontdoor_api::frontdoor::run_cancelled() {
                    stop_child(&mut child);
                    return err_json("the check run was cancelled");
                }
                if started.elapsed() > limit_for(deadline, latest.as_ref(), operator_cap) {
                    stop_child(&mut child);
                    return err_json(&stopped_message(
                        deadline,
                        latest.as_ref(),
                        started.elapsed().as_secs(),
                    ));
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(e) => return err_json(&format!("waiting for hauksbee-ci: {e}")),
        }
    };
    // The child has exited, so both readers see EOF; the joins cannot block.
    let out_stdout = stdout_thread
        .and_then(|t| t.join().ok())
        .unwrap_or_default();
    let out_stderr = stderr_thread.join().unwrap_or_default();
    // The last words of the run, sent just before it exited.
    while let Ok((raw, _)) = rx.try_recv() {
        hauksbee_frontdoor_api::frontdoor::report_progress(&raw);
    }

    // --json prints one JSON object on stdout for both green and red runs, and
    // {"ok":false,...} for a spec error. Anything else is relayed with the
    // stderr tail so the user sees the real failure.
    let stdout = String::from_utf8_lossy(&out_stdout);
    if let Some(json_line) = stdout.lines().find(|l| l.trim_start().starts_with('{')) {
        return json_line.to_string();
    }
    let stderr = String::from_utf8_lossy(&out_stderr);
    let mut tail: Vec<&str> = stderr.lines().rev().take(12).collect();
    tail.reverse();
    err_json(&format!(
        "hauksbee-ci produced no result (exit {:?}). {}",
        status.code(),
        tail.join(" | ")
    ))
}

/// How long a child may run from its start, given what it has reported so far.
///
/// A fixed deadline is that. A board's run is scaled to the work its own
/// progress reports ([`hauksbee_ir::progress::Heard::ceiling`]), the floor until
/// it has reported any, and an operator's cap (when set) can only shorten it.
fn limit_for(
    deadline: Deadline,
    latest: Option<&hauksbee_ir::progress::Heard>,
    operator_cap: Option<std::time::Duration>,
) -> std::time::Duration {
    match deadline {
        Deadline::Fixed(limit) => limit,
        Deadline::Scaled => {
            let scaled = latest.map_or(
                hauksbee_ir::progress::RUN_CEILING_FLOOR,
                hauksbee_ir::progress::Heard::ceiling,
            );
            operator_cap.map_or(scaled, |cap| cap.min(scaled))
        }
    }
}

/// What the browser is told when a run is stopped at its ceiling.
fn stopped_message(
    deadline: Deadline,
    latest: Option<&hauksbee_ir::progress::Heard>,
    elapsed_s: u64,
) -> String {
    match deadline {
        Deadline::Scaled => hauksbee_ir::vocabulary::check_run_stopped(
            latest.map(|h| (h.label.as_str(), h.percent())),
            elapsed_s,
        ),
        Deadline::Fixed(limit) => format!(
            "the check run exceeded {}s and was stopped; shorten duration_ms \
             or run it from the CLI: {}",
            limit.as_secs(),
            hauksbee_ir::vocabulary::CHECK_RUN_CLI_COMMAND
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::PoisonError;

    /// Serializes the tests that touch the global `ACTIVE_WEB_CHECKS` counter
    /// (every `run_web_check` call acquires a slot, and the concurrency test
    /// asserts on the exact count). Cargo runs tests in parallel, so without this
    /// two counter-touching tests could interleave and consume each other's
    /// slots. Recovered from a poisoned lock so one failing test does not cascade.
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn serial_guard() -> std::sync::MutexGuard<'static, ()> {
        SERIAL.lock().unwrap_or_else(PoisonError::into_inner)
    }

    const BOOT_GATE: &str =
        include_str!("../../hauksbee-ci/examples/boards/boot_gate_clean.kicad_pcb");

    fn spec_fragment() -> &'static str {
        r#"name = "web check"
duration_ms = 10

[[supply]]
net = "+5V"
kind = "ideal"
volts = 5.0

[[assert]]
kind = "no_faults"
"#
    }

    /// End-to-end through the real sibling binary when it exists (a build tree
    /// or an installed layout); skips cleanly otherwise so a bare `cargo test`
    /// on a fresh clone stays green.
    /// The workspace's newest built `hauksbee-ci`. In a cargo test the
    /// `current_exe` is the test runner deep in target/, so resolve it
    /// explicitly, and take the newest so a stale release binary left from an
    /// old checkout never stands in for the one this tree just built.
    fn built_ci_binary() -> Option<std::path::PathBuf> {
        ["release", "debug"]
            .iter()
            .map(|p| {
                std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join(format!("../../target/{p}/hauksbee-ci"))
            })
            .filter_map(|p| Some((std::fs::metadata(&p).ok()?.modified().ok()?, p)))
            .max()
            .map(|(_, p)| p)
    }

    #[test]
    fn web_check_runs_the_real_ci_binary() {
        let _serial = serial_guard();
        let Some(ci) = built_ci_binary() else {
            eprintln!("skipping web_check e2e (no hauksbee-ci built)");
            return;
        };
        std::env::set_var("HAUKSBEE_CI_BIN", &ci);
        let json = run_web_check(
            "boot_gate.kicad_pcb",
            BOOT_GATE.as_bytes(),
            None,
            spec_fragment(),
        );
        std::env::remove_var("HAUKSBEE_CI_BIN");
        let v: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert_eq!(v["ok"], true, "run succeeds: {json}");
        assert_eq!(
            v["passed"], true,
            "no_faults holds on the demo board: {json}"
        );
        assert!(v["results"].as_array().is_some_and(|r| !r.is_empty()));
    }

    /// The scaffold marker asks the CI binary for `init --json` instead of a
    /// run, and the answer is the supply list the builder seeds from.
    #[test]
    fn web_scaffold_asks_the_ci_binary_for_its_init_supplies() {
        let _serial = serial_guard();
        let Some(ci) = built_ci_binary() else {
            eprintln!("skipping web_scaffold e2e (no hauksbee-ci built)");
            return;
        };
        std::env::set_var("HAUKSBEE_CI_BIN", &ci);
        let json = run_web_check(
            "boot_gate.kicad_pcb",
            BOOT_GATE.as_bytes(),
            None,
            hauksbee_frontdoor_api::frontdoor::SCAFFOLD_SPEC,
        );
        std::env::remove_var("HAUKSBEE_CI_BIN");
        let v: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert_eq!(v["ok"], true, "scaffold succeeds: {json}");
        assert!(
            v["supplies"]
                .as_array()
                .is_some_and(|s| s.iter().any(|x| x["net"] == "+5V")),
            "the +5V rail is among the supplies: {json}"
        );
    }

    #[test]
    fn spec_fragment_must_not_smuggle_paths() {
        let _serial = serial_guard();
        let json = run_web_check(
            "b.kicad_pcb",
            b"(kicad_pcb)",
            None,
            "board = \"/etc/passwd\"\n[[assert]]\nkind = \"no_faults\"\n",
        );
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["ok"], false);
        assert!(v["error"].as_str().unwrap_or("").contains("must not set"));
    }

    /// Every path-bearing spec key is refused, top-level and indented, and the
    /// rejection happens before anything touches the filesystem.
    #[test]
    fn every_path_bearing_key_is_rejected() {
        let _serial = serial_guard();
        for key in PATH_KEYS {
            let frag = format!("{key} = \"/etc/passwd\"\n");
            assert_eq!(forbidden_path_key(&frag), Some(key), "top-level {key}");
            let indented = format!("  \t{key} = \"../../escape\"\n");
            assert_eq!(forbidden_path_key(&indented), Some(key), "indented {key}");
            let json = run_web_check("b.kicad_pcb", b"(kicad_pcb)", None, &frag);
            let v: serde_json::Value = serde_json::from_str(&json).unwrap();
            assert_eq!(v["ok"], false, "{key} fragment must be refused");
            assert!(
                v["error"].as_str().unwrap_or("").contains(key),
                "error names the offending key {key}: {json}"
            );
        }
    }

    /// Keys inside `[[table]]` blocks, inline tables, and dotted keys cannot
    /// sneak past the line scan: the TOML walk catches them.
    #[test]
    fn nested_and_inline_path_keys_are_rejected() {
        let block = "[[assert]]\nkind = \"hwtrace\"\ntrace = \"../secrets/trace.toml\"\n";
        assert_eq!(forbidden_path_key(block), Some("trace"));
        let inline = "peripheral = [{ id = \"scope\", type = \"vcd_sink\", net = \"CLK\", vcd_path = \"/tmp/pwn.vcd\" }]\n";
        assert_eq!(forbidden_path_key(inline), Some("vcd_path"));
        let dotted = "assert.trace = \"/etc/passwd\"\n";
        assert_eq!(forbidden_path_key(dotted), Some("trace"));
        let quoted = "\"vcd_path\" = \"/tmp/pwn.vcd\"\n";
        assert_eq!(forbidden_path_key(quoted), Some("vcd_path"));
        let sensor = "[[sensor]]\nid = \"t\"\nspec_file = \"/home/user/.ssh/id_rsa\"\n";
        assert_eq!(forbidden_path_key(sensor), Some("spec_file"));
    }

    /// A legitimate fragment, and identifiers that merely CONTAIN a forbidden
    /// key or mention one inside a string, pass the filter.
    #[test]
    fn benign_fragments_pass_the_path_filter() {
        assert_eq!(forbidden_path_key(spec_fragment()), None);
        let lookalikes = "name = \"trace of the board\"\nboard_rev = \"C\"\nfirmware_version = 2\n";
        assert_eq!(forbidden_path_key(lookalikes), None);
    }

    // ── Web-only resource ceilings ────────────────────────────────────────────

    /// A spec sitting exactly at every cap passes validation. This is a pure
    /// function, so it needs neither the real binary nor a concurrency slot.
    #[test]
    fn spec_at_the_caps_is_accepted() {
        let asserts = "[[assert]]\nkind = \"no_faults\"\n".repeat(MAX_WEB_ASSERTS);
        let tolerances =
            "[[tolerance]]\nref = \"R*\"\npercent = 10.0\n".repeat(MAX_WEB_TOLERANCE_RULES);
        let spec = format!(
            "duration_ms = {MAX_WEB_DURATION_MS}\nframe_ms = 0.05\n\
             [fuzz]\nseeds = {MAX_WEB_FUZZ_SEEDS}\n\
             [ensemble]\nseeds = {MAX_WEB_ENSEMBLE_SEEDS}\n\
             {tolerances}\n{asserts}"
        );
        assert_eq!(
            validate_web_limits(&spec),
            Ok(()),
            "spec at the caps: {spec}"
        );
    }

    /// An integer `duration_ms` at the cap is accepted (the field is f64 in the
    /// spec, but a hand author writes `2000`, not `2000.0`).
    #[test]
    fn integer_duration_at_cap_is_accepted() {
        assert_eq!(
            validate_web_limits("duration_ms = 2000\n[[assert]]\nkind = \"no_faults\"\n"),
            Ok(())
        );
    }

    #[test]
    fn duration_over_cap_is_rejected() {
        let err = validate_web_limits("duration_ms = 2500\n[[assert]]\nkind = \"no_faults\"\n")
            .expect_err("2500 ms exceeds the 2000 ms cap");
        assert!(err.contains("duration_ms"), "names the key: {err}");
        assert!(err.contains("2000"), "names the cap: {err}");
    }

    #[test]
    fn frame_ms_too_fine_is_rejected() {
        let err = validate_web_limits("frame_ms = 0.001\n[[assert]]\nkind = \"no_faults\"\n")
            .expect_err("0.001 ms over the default 100 ms window is 100000 frames");
        assert!(err.contains("frame_ms"), "names the key: {err}");
        let short = "duration_ms = 30\nframe_ms = 0.01\n[[assert]]\nkind = \"no_faults\"\n";
        assert_eq!(
            validate_web_limits(short),
            Ok(()),
            "a short window may be fine"
        );
    }

    #[test]
    fn fuzz_seeds_over_cap_is_rejected() {
        let err = validate_web_limits("[fuzz]\nseeds = 64\n[[assert]]\nkind = \"no_faults\"\n")
            .expect_err("64 seeds exceeds the fuzz cap of 8");
        assert!(
            err.contains("fuzz") && err.contains("seeds"),
            "names the field: {err}"
        );
    }

    #[test]
    fn ensemble_seeds_over_cap_is_rejected() {
        let err =
            validate_web_limits("[ensemble]\nseeds = 128\n[[assert]]\nkind = \"no_faults\"\n")
                .expect_err("128 seeds exceeds the ensemble cap of 16");
        assert!(
            err.contains("ensemble") && err.contains("seeds"),
            "names the field: {err}"
        );
    }

    #[test]
    fn too_many_tolerance_rules_rejected() {
        let toosmany =
            "[[tolerance]]\nref = \"R*\"\npercent = 10.0\n".repeat(MAX_WEB_TOLERANCE_RULES + 1);
        let err = validate_web_limits(&format!("{toosmany}[[assert]]\nkind = \"no_faults\"\n"))
            .expect_err("over the tolerance-rule cap");
        assert!(err.contains("tolerance"), "names tolerance: {err}");
    }

    #[test]
    fn too_many_asserts_rejected() {
        let asserts = "[[assert]]\nkind = \"no_faults\"\n".repeat(MAX_WEB_ASSERTS + 1);
        let err = validate_web_limits(&asserts).expect_err("over the assert cap");
        assert!(err.contains("assert"), "names assert: {err}");
        assert!(err.contains("64"), "names the cap: {err}");
    }

    /// A fragment that is not valid TOML is let through so hauksbee-ci can emit
    /// the real, located parse error instead of an invented one.
    #[test]
    fn unparseable_fragment_is_passed_through() {
        assert_eq!(validate_web_limits("this = = not toml ]["), Ok(()));
    }

    /// Spawn a shell child that traps TERM with `on_term` and touches a flag
    /// file once the trap is installed; return it after the flag appears.
    #[cfg(unix)]
    fn ready_trap_child(name: &str, on_term: &str) -> (std::process::Child, std::path::PathBuf) {
        let pid = std::process::id();
        let flag = std::env::temp_dir().join(format!("hauksbee-stopchild-{name}-{pid}"));
        let _ = std::fs::remove_file(&flag);
        let child = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!(
                "trap '{on_term}' TERM; : > '{}'; while :; do sleep 0.05; done",
                flag.display()
            ))
            .spawn()
            .expect("spawn trap child");
        // Wait for the trap to be installed (the flag file appears after it).
        let t0 = std::time::Instant::now();
        while !flag.exists() {
            assert!(t0.elapsed() < CHECK_TIMEOUT, "child never became ready");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        (child, flag)
    }

    /// A timed-out child must get SIGTERM (so its signal
    /// reaper can kill the emulators it spawned), not a blind SIGKILL. A child
    /// that exits on TERM must NOT be `SIGKILLed`; verified by its exit status
    /// carrying the clean trap exit, not a signal death.
    #[cfg(unix)]
    #[test]
    fn stop_child_terms_gracefully_so_the_childs_reaper_can_run() {
        use std::os::unix::process::ExitStatusExt;
        let (mut child, flag) = ready_trap_child("graceful", "exit 0");
        stop_child(&mut child);
        let status = child.wait().expect("child reaped");
        assert_eq!(
            status.signal(),
            None,
            "a TERM-handling child must exit through its handler, not be SIGKILLed: {status}"
        );
        assert!(status.success(), "the trap exits 0: {status}");
        let _ = std::fs::remove_file(&flag);
    }

    /// The fallback: a child that ignores SIGTERM is still `SIGKILLed` after the
    /// grace period, so a wedged run cannot outlive its request forever.
    #[cfg(unix)]
    #[test]
    fn stop_child_kills_a_term_ignoring_child_after_the_grace() {
        use std::os::unix::process::ExitStatusExt;
        let (mut child, flag) = ready_trap_child("stubborn", "");
        stop_child(&mut child);
        let status = child.wait().expect("child reaped");
        assert_eq!(
            status.signal(),
            Some(libc::SIGKILL),
            "a TERM-ignoring child gets the KILL fallback: {status}"
        );
        let _ = std::fs::remove_file(&flag);
    }

    /// Acquire the full budget of slots, confirm the next acquire is refused,
    /// then drop one and confirm a slot frees up. Serialized so no other
    /// counter-touching test perturbs the count mid-assertion.
    #[test]
    fn concurrency_budget_is_enforced() {
        let _serial = serial_guard();
        // The counter should be at rest since we hold the serial lock.
        assert_eq!(
            ACTIVE_WEB_CHECKS.load(Ordering::Acquire),
            0,
            "counter starts clean"
        );

        let mut slots: Vec<WebCheckSlot> = Vec::new();
        for i in 0..MAX_CONCURRENT_WEB_CHECKS {
            slots.push(WebCheckSlot::acquire().unwrap_or_else(|| panic!("slot {i} within budget")));
        }
        // At the cap: the next acquire must fail, and the counter must not have
        // crept past the ceiling (the failed acquire rolled its increment back).
        assert!(
            WebCheckSlot::acquire().is_none(),
            "acquire past the cap is refused"
        );
        assert_eq!(
            ACTIVE_WEB_CHECKS.load(Ordering::Acquire),
            MAX_CONCURRENT_WEB_CHECKS
        );

        // Freeing one slot lets exactly one more through.
        drop(slots.pop());
        let regained = WebCheckSlot::acquire().expect("a freed slot is reusable");
        assert!(
            WebCheckSlot::acquire().is_none(),
            "still capped after regaining one"
        );

        drop(regained);
        drop(slots);
        assert_eq!(
            ACTIVE_WEB_CHECKS.load(Ordering::Acquire),
            0,
            "all slots released on drop"
        );
    }

    /// Regression: the runner must DRAIN the child's pipes while it runs. A
    /// stub "hauksbee-ci" floods both stdout and stderr with well over the
    /// 64 KiB pipe capacity before (stdout) and after (stderr) the JSON line;
    /// with the old read-after-exit pattern the child blocked in `write()`, the
    /// poll loop saw no exit, and a healthy run was reported as a timeout.
    /// Two-sided: the run returns the child's real JSON (not the timeout
    /// error), and it does so promptly.
    #[test]
    fn chatty_child_output_is_drained_not_deadlocked() {
        let _serial = serial_guard();
        let dir = tempfile::tempdir().expect("tempdir");
        // 4000 lines x 50 bytes ~ 200 KiB per stream, far past the pipe buffer.
        // A shell script on unix; a batch file on Windows, because
        // CreateProcess execs only real executables and cmd batch files are
        // the one script form std::process spawns directly.
        #[cfg(unix)]
        let stub = {
            use std::os::unix::fs::PermissionsExt;
            let stub = dir.path().join("hauksbee-ci");
            std::fs::write(
                &stub,
                "#!/bin/sh\n\
                 i=0; while [ $i -lt 4000 ]; do echo \"stderr filler line $i ................................\" >&2; i=$((i+1)); done\n\
                 i=0; while [ $i -lt 4000 ]; do echo \"stdout filler line $i ................................\"; i=$((i+1)); done\n\
                 echo '{\"ok\":true,\"passed\":true,\"results\":[{\"kind\":\"no_faults\"}]}'\n",
            )
            .expect("write stub");
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
            stub
        };
        #[cfg(windows)]
        let stub = {
            let stub = dir.path().join("hauksbee-ci.bat");
            std::fs::write(
                &stub,
                "@echo off\r\n\
                 for /L %%i in (1,1,4000) do echo stderr filler line %%i ................................ 1>&2\r\n\
                 for /L %%i in (1,1,4000) do echo stdout filler line %%i ................................\r\n\
                 echo {\"ok\":true,\"passed\":true,\"results\":[{\"kind\":\"no_faults\"}]}\r\n",
            )
            .expect("write stub");
            stub
        };
        std::env::set_var("HAUKSBEE_CI_BIN", &stub);
        let started = std::time::Instant::now();
        let json = run_web_check("b.kicad_pcb", b"(kicad_pcb)", None, spec_fragment());
        std::env::remove_var("HAUKSBEE_CI_BIN");
        let v: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert_eq!(
            v["ok"], true,
            "the flood run's real JSON came through: {json}"
        );
        assert!(
            started.elapsed() < CHECK_TIMEOUT / 2,
            "the run returned promptly instead of riding the timeout ({}s)",
            started.elapsed().as_secs()
        );
    }

    /// A stub `hauksbee-ci` for the watched-run tests: the shell `body`, run as
    /// the child.
    #[cfg(unix)]
    fn stub_ci(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let stub = dir.join("hauksbee-ci");
        std::fs::write(&stub, format!("#!/bin/sh\n{body}\n")).expect("write stub");
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        stub
    }

    /// The web run asks the child to report its progress and hands each report,
    /// as the child wrote it, to whoever watches the thread; the reports are not
    /// left in the failure tail, and the answer is still the child's JSON.
    #[cfg(unix)]
    #[test]
    fn a_watched_run_passes_the_childs_progress_to_the_watcher() {
        use hauksbee_frontdoor_api::frontdoor::{with_run_watch, RunWatch};
        let _serial = serial_guard();
        let dir = tempfile::tempdir().expect("tempdir");
        let stub = stub_ci(
            dir.path(),
            r#"if [ "$HAUKSBEE_PROGRESS_STREAM" = 1 ]; then
  echo '{"stage":"bind","label":"Matching parts to models","fraction":0.25,"elapsed_ms":900,"total_ms":4000,"eta_ms":null}' >&2
  echo 'ordinary diagnostic line' >&2
  echo '{"stage":"simulate","label":"Simulating","fraction":0.75,"elapsed_ms":2900,"total_ms":4000,"eta_ms":900}' >&2
fi
echo '{"ok":true,"passed":true,"results":[]}'"#,
        );
        std::env::set_var("HAUKSBEE_CI_BIN", &stub);
        let heard = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
        let sink = heard.clone();
        let json = with_run_watch(
            RunWatch {
                progress: Box::new(move |line| sink.borrow_mut().push(line.to_string())),
                cancelled: Box::new(|| false),
            },
            || run_web_check("b.kicad_pcb", b"(kicad_pcb)", None, spec_fragment()),
        );
        std::env::remove_var("HAUKSBEE_CI_BIN");
        let v: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert_eq!(v["ok"], true, "{json}");
        let heard = heard.borrow();
        assert_eq!(
            heard.len(),
            2,
            "both reports reached the watcher: {heard:?}"
        );
        assert!(heard[0].contains("Matching parts to models"), "{heard:?}");
        assert!(heard[1].contains("\"Simulating\""), "{heard:?}");
    }

    /// A page that has closed the stream is not waited for: the child is
    /// stopped, and promptly, instead of running on to the ceiling.
    #[cfg(unix)]
    #[test]
    fn a_watcher_that_has_gone_away_stops_the_child() {
        use hauksbee_frontdoor_api::frontdoor::{with_run_watch, RunWatch};
        let _serial = serial_guard();
        let dir = tempfile::tempdir().expect("tempdir");
        let stub = stub_ci(dir.path(), "exec sleep 120");
        std::env::set_var("HAUKSBEE_CI_BIN", &stub);
        let started = std::time::Instant::now();
        let json = with_run_watch(
            RunWatch {
                progress: Box::new(|_| {}),
                cancelled: Box::new(|| true),
            },
            || run_web_check("b.kicad_pcb", b"(kicad_pcb)", None, spec_fragment()),
        );
        std::env::remove_var("HAUKSBEE_CI_BIN");
        let v: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert_eq!(v["ok"], false, "{json}");
        assert!(
            v["error"].as_str().unwrap_or("").contains("cancelled"),
            "{json}"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(20),
            "the child was stopped, not waited for ({}s)",
            started.elapsed().as_secs()
        );
    }

    /// With nobody watching, the same run behaves as it always did (no progress
    /// is asked for beyond the environment flag, and nothing is cancelled).
    #[cfg(unix)]
    #[test]
    fn an_unwatched_run_is_unchanged() {
        let _serial = serial_guard();
        let dir = tempfile::tempdir().expect("tempdir");
        let stub = stub_ci(
            dir.path(),
            r#"echo '{"stage":"bind","label":"x","fraction":0.5,"elapsed_ms":1,"total_ms":1,"eta_ms":null}' >&2
echo '{"ok":true,"passed":true,"results":[]}'"#,
        );
        std::env::set_var("HAUKSBEE_CI_BIN", &stub);
        let json = run_web_check("b.kicad_pcb", b"(kicad_pcb)", None, spec_fragment());
        std::env::remove_var("HAUKSBEE_CI_BIN");
        assert!(json.contains("\"ok\":true"), "{json}");
    }

    /// What a stopped run says is built from the one vocabulary, names the stage
    /// it was in and how far it got, and gives the terminal command.
    #[test]
    fn a_boards_ceiling_scales_with_its_reported_work_and_an_operator_can_only_shorten_it() {
        use std::time::Duration;
        let floor = hauksbee_ir::progress::RUN_CEILING_FLOOR;
        // Nothing heard yet: the floor, then the work the run says it has.
        assert_eq!(limit_for(Deadline::Scaled, None, None), floor);
        let big = hauksbee_ir::progress::Heard {
            label: "Simulating".into(),
            fraction: 0.09,
            elapsed: Duration::from_secs(600),
            total_ms: 14_000.0,
        };
        let scaled = limit_for(Deadline::Scaled, Some(&big), None);
        assert!(scaled > Duration::from_secs(3600), "{scaled:?}");
        assert_eq!(
            limit_for(Deadline::Scaled, Some(&big), Some(Duration::from_secs(90))),
            Duration::from_secs(90)
        );
        // A cap longer than the scaled ceiling changes nothing.
        assert_eq!(
            limit_for(Deadline::Scaled, None, Some(Duration::from_secs(99_999))),
            floor
        );
        // A short request keeps its own fixed deadline, whatever the cap says.
        assert_eq!(
            limit_for(
                Deadline::Fixed(CHECK_TIMEOUT),
                None,
                Some(Duration::from_secs(1))
            ),
            CHECK_TIMEOUT
        );
    }

    #[test]
    fn a_stopped_board_run_names_what_was_running_and_the_next_step() {
        let heard = hauksbee_ir::progress::Heard {
            label: "Simulating".into(),
            fraction: 0.42,
            elapsed: std::time::Duration::from_secs(600),
            total_ms: 60_000.0,
        };
        let said = stopped_message(Deadline::Scaled, Some(&heard), 612);
        assert!(said.contains("10 minutes"), "{said}");
        assert!(said.contains("\"Simulating\" (42%"), "{said}");
        assert!(
            said.contains(hauksbee_ir::vocabulary::CHECK_RUN_CLI_COMMAND),
            "{said}"
        );
        let silent = stopped_message(Deadline::Scaled, None, 300);
        assert!(
            silent.contains("300 seconds") || silent.contains("5 minutes"),
            "{silent}"
        );
        let fixed = stopped_message(Deadline::Fixed(CHECK_TIMEOUT), None, 300);
        assert!(
            fixed.contains(hauksbee_ir::vocabulary::CHECK_RUN_CLI_COMMAND),
            "{fixed}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn complete_design_bundle_is_staged_into_the_checks_spec() {
        use hauksbee_frontdoor_api::frontdoor::{DesignUpload, NamedUpload};
        use std::os::unix::fs::PermissionsExt;

        let _serial = serial_guard();
        let dir = tempfile::tempdir().expect("tempdir");
        let stub = dir.path().join("hauksbee-ci");
        std::fs::write(
            &stub,
            "#!/bin/sh\nset -eu\nspec=$2\nbase=$(dirname \"$spec\")\n\
             grep -q '^bom = \"bom.csv\"' \"$spec\"\n\
             grep -q '^placement = \"board.pos\"' \"$spec\"\n\
             grep -q '^variant = \"production.variant.toml\"' \"$spec\"\n\
             grep -q '^asbuilt = \"unit.asbuilt.toml\"' \"$spec\"\n\
             grep -q '^models_dir = \"models\"' \"$spec\"\n\
             test -f \"$base/models/custom.toml\"\n\
             echo '{\"ok\":true}'\n",
        )
        .expect("write stub");
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::env::set_var("HAUKSBEE_CI_BIN", &stub);
        let named = |name: &str, bytes: &[u8]| NamedUpload {
            name: name.into(),
            bytes: bytes.to_vec(),
        };
        let json = run_web_check_design(
            DesignUpload {
                board: named("board.kicad_pcb", b"board"),
                firmware: None,
                schematic: None,
                bom: Some(named("bom.csv", b"bom")),
                placement: Some(named("board.pos", b"placement")),
                variant: Some(named("production.variant.toml", b"variant")),
                asbuilt: Some(named("unit.asbuilt.toml", b"overlay")),
                models: vec![named("custom.toml", b"model")],
            },
            spec_fragment(),
        );
        std::env::remove_var("HAUKSBEE_CI_BIN");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&json).unwrap()["ok"],
            true,
            "{json}"
        );
    }

    fn measure_upload() -> hauksbee_frontdoor_api::frontdoor::DesignUpload {
        hauksbee_frontdoor_api::frontdoor::DesignUpload {
            board: hauksbee_frontdoor_api::frontdoor::NamedUpload {
                name: "b.kicad_pcb".into(),
                bytes: b"board".to_vec(),
            },
            firmware: None,
            schematic: None,
            bom: None,
            placement: None,
            variant: None,
            asbuilt: None,
            models: Vec::new(),
        }
    }

    #[test]
    fn a_measure_request_names_its_assertion() {
        assert_eq!(measure_request("name = \"x\"\n"), Ok(None));
        assert_eq!(
            measure_request("# hauksbee:measure 3\n[[assert]]"),
            Ok(Some(3))
        );
        assert!(measure_request("# hauksbee:measure\n").is_err());
        assert!(measure_request("# hauksbee:measure -1\n").is_err());
    }

    /// A reading depends on everything but the bounds: editing a bound must hit the
    /// cache, editing the net, the circuit or an uploaded byte must not.
    #[test]
    fn the_measure_cache_key_ignores_bounds_and_nothing_else() {
        let base = "# hauksbee:measure 0\nduration_ms = 10\n\n[[assert]]\nkind = \"voltage\"\nnet = \"+5V\"\nmin = 4.0\n";
        let key = |upload: &hauksbee_frontdoor_api::frontdoor::DesignUpload, fragment: &str| {
            measure_cache_key(upload, fragment, 0)
        };
        let upload = measure_upload();
        let first = key(&upload, base).expect("a key");
        assert_eq!(
            Some(first.clone()),
            key(&upload, &base.replace("4.0", "4.5"))
        );
        assert_eq!(
            Some(first.clone()),
            key(&upload, &base.replace("min = 4.0", "min = 4.0\nmax = 5.5"))
        );
        assert_ne!(
            Some(first.clone()),
            key(&upload, &base.replace("+5V", "+3V3"))
        );
        assert_ne!(Some(first.clone()), key(&upload, &base.replace("10", "20")));
        let mut other = measure_upload();
        other.board.bytes = b"board2".to_vec();
        assert_ne!(Some(first), key(&other, base));
        assert_eq!(measure_cache_key(&upload, base, 5), None);
    }

    /// The second identical measurement is answered from the cache without spawning
    /// the CI binary, and says so; a refusal is never kept.
    #[cfg(unix)]
    #[test]
    fn a_repeated_measurement_is_replayed_from_the_cache() {
        use std::os::unix::fs::PermissionsExt;

        let _serial = serial_guard();
        let dir = tempfile::tempdir().expect("tempdir");
        let counter = dir.path().join("runs");
        let stub = dir.path().join("hauksbee-ci");
        std::fs::write(
            &stub,
            format!(
                "#!/bin/sh\necho x >> '{}'\n\
                 echo '{{\"ok\":true,\"value\":5.0,\"cached\":false,\"t_ms\":900.0}}'\n",
                counter.display()
            ),
        )
        .expect("write stub");
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::env::set_var("HAUKSBEE_CI_BIN", &stub);
        // A board name no other test uses, so the process-wide cache cannot already hold it.
        let mut upload = measure_upload();
        upload.board.name = "measure-cache-test.kicad_pcb".into();
        let fragment = "# hauksbee:measure 0\nduration_ms = 10\n\n[[assert]]\nkind = \"voltage\"\nnet = \"+5V\"\n";
        let first = run_web_check_design(upload.clone(), fragment);
        let again = run_web_check_design(upload, &format!("{fragment}min = 4.5\n"));
        std::env::remove_var("HAUKSBEE_CI_BIN");
        let first: serde_json::Value = serde_json::from_str(&first).unwrap();
        let again: serde_json::Value = serde_json::from_str(&again).unwrap();
        assert_eq!(first["cached"], false, "{first}");
        assert_eq!(again["cached"], true, "{again}");
        assert_eq!(again["value"], 5.0);
        assert!(again["t_ms"].as_f64().unwrap() < 50.0, "{again}");
        assert_eq!(std::fs::read_to_string(counter).unwrap().lines().count(), 1);

        measure_cache_put("refusal".into(), "{\"ok\":false,\"error\":\"no\"}");
        assert!(measure_cache_get("refusal").is_none());
    }
}
