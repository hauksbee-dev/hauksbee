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

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Reject path separators and other surprises in an uploaded file name; the
/// staged file keeps its extension (format sniffing needs it) but nothing else
/// exotic.
fn sanitize_name(name: &str, fallback: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(fallback);
    let clean: String = base
        .chars()
        .map(|c| if c.is_alphanumeric() || ".-_".contains(c) { c } else { '_' })
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
/// (`asbuilt`, an hwtrace `trace` and the data files it references, a sensor's
/// `spec_file`) or overwrite (a vcd_sink's `vcd_path` reaches `File::create`)
/// paths outside the staging dir, because the CI resolver accepts absolute
/// paths and `..` traversal.
const PATH_KEYS: [&str; 6] = ["board", "firmware", "asbuilt", "trace", "spec_file", "vcd_path"];

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
        let key = line.trim_start().split(['=', ' ', '\t']).next().unwrap_or("");
        if let Some(hit) = PATH_KEYS.iter().copied().find(|k| *k == key) {
            return Some(hit);
        }
    }
    spec_fragment
        .parse::<toml::Value>()
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

/// Hard ceiling on one web-triggered check run. The builder caps duration_ms
/// client-side, but the web UI also lets a user edit the spec TOML directly, so
/// nothing client-side is trustworthy. A pathological spec (huge fuzz count, a
/// hung external emulator) must not pin the request forever, and the
/// `validate_web_limits` ceilings below stop it from getting that far in the
/// first place.
const CHECK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

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
/// `frame_ms`: net-sampling cadence. Reject absurdly fine cadence (which
/// multiplies the frame count and the wall time) by requiring at least this.
const MIN_WEB_FRAME_MS: f64 = 0.05;
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
    fn acquire() -> Option<WebCheckSlot> {
        // Optimistically claim a slot, then roll back if that put us over the
        // cap. The transient over-count is bounded by the number of racing
        // callers and is undone immediately, so no run ever proceeds above the
        // ceiling.
        let prev = ACTIVE_WEB_CHECKS.fetch_add(1, Ordering::AcqRel);
        if prev >= MAX_CONCURRENT_WEB_CHECKS {
            ACTIVE_WEB_CHECKS.fetch_sub(1, Ordering::AcqRel);
            None
        } else {
            Some(WebCheckSlot)
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
    let value = match spec_fragment.parse::<toml::Value>() {
        Ok(v) => v,
        Err(_) => return Ok(()),
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

    // frame_ms: sampling cadence. Present-only (default is fine); reject cadence
    // finer than the floor.
    if let Some(ms) = table.get("frame_ms").and_then(as_number) {
        if ms < MIN_WEB_FRAME_MS {
            return Err(format!(
                "frame_ms = {ms} is finer than the web check limit of {MIN_WEB_FRAME_MS} ms. \
                 A finer cadence multiplies the frame count; run it from the CLI for full density."
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
    if let Some(len) = table.get("tolerance").and_then(toml::Value::as_array).map(|a| a.len()) {
        if len > MAX_WEB_TOLERANCE_RULES {
            return Err(format!(
                "the spec declares {len} [[tolerance]] rules, over the web check limit of \
                 {MAX_WEB_TOLERANCE_RULES}. Run wide tolerance sweeps from the CLI."
            ));
        }
    }

    // [[assert]] count.
    if let Some(len) = table.get("assert").and_then(toml::Value::as_array).map(|a| a.len()) {
        if len > MAX_WEB_ASSERTS {
            return Err(format!(
                "the spec declares {len} [[assert]] blocks, over the web check limit of \
                 {MAX_WEB_ASSERTS}. Split the check or run it from the CLI."
            ));
        }
    }

    Ok(())
}

/// Stage the uploaded files, inject the path keys, run `hauksbee-ci --json`,
/// and relay its JSON. Every failure mode returns `{"ok":false,"error":...}`
/// so the browser always has something readable.
pub fn run_web_check(
    board_name: &str,
    board_bytes: &[u8],
    firmware: Option<(&str, &[u8])>,
    spec_fragment: &str,
) -> String {
    // Reserve a concurrency slot on entry. At the cap, refuse fast rather than
    // pile another emulator-spawning child onto a loaded box. `_slot` lives to
    // the end of the function, and its `Drop` releases the slot on every exit
    // path below (each early return, the normal return, or a panic).
    let _slot = match WebCheckSlot::acquire() {
        Some(s) => s,
        None => {
            return err_json(
                "server busy: too many checks are running right now, try again in a moment",
            )
        }
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

    let board_file = sanitize_name(board_name, "board.kicad_pcb");
    if let Err(e) = std::fs::write(dir.join(&board_file), board_bytes) {
        return err_json(&format!("could not stage the board: {e}"));
    }
    let mut spec = format!("board = \"{board_file}\"\n");
    if let Some((fw_name, fw_bytes)) = firmware {
        let fw_file = sanitize_name(fw_name, "firmware.elf");
        if let Err(e) = std::fs::write(dir.join(&fw_file), fw_bytes) {
            return err_json(&format!("could not stage the firmware: {e}"));
        }
        spec.push_str(&format!("firmware = \"{fw_file}\"\n"));
    }
    spec.push('\n');
    spec.push_str(spec_fragment);
    let spec_path = dir.join("spec.toml");
    if let Err(e) = std::fs::write(&spec_path, &spec) {
        return err_json(&format!("could not stage the spec: {e}"));
    }

    let bin = ci_binary();
    let child = std::process::Command::new(&bin)
        .arg("run")
        .arg(&spec_path)
        .arg("--json")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return err_json(&format!(
                "hauksbee-ci was not found (looked for '{}'). It installs alongside \
                 hauksbee — re-run scripts/install.sh, or set HAUKSBEE_CI_BIN.",
                bin.display()
            ));
        }
        Err(e) => return err_json(&format!("could not run hauksbee-ci: {e}")),
    };

    // Poll-wait with a deadline instead of output(): a hung run must produce
    // an error the browser can show, not an eternally pending request.
    let started = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if started.elapsed() > CHECK_TIMEOUT {
                    let _ = child.kill();
                    return err_json(&format!(
                        "the check run exceeded {}s and was stopped — shorten duration_ms \
                         or run it from the CLI: hauksbee-ci run <spec>",
                        CHECK_TIMEOUT.as_secs()
                    ));
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(e) => return err_json(&format!("waiting for hauksbee-ci: {e}")),
        }
    }
    let out = match child.wait_with_output() {
        Ok(o) => o,
        Err(e) => return err_json(&format!("reading hauksbee-ci output: {e}")),
    };

    // --json prints one JSON object on stdout for both green and red runs, and
    // {"ok":false,...} for a spec error. Anything else is relayed with the
    // stderr tail so the user sees the real failure.
    let stdout = String::from_utf8_lossy(&out.stdout);
    if let Some(json_line) = stdout.lines().find(|l| l.trim_start().starts_with('{')) {
        return json_line.to_string();
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    let tail: Vec<&str> = stderr.lines().rev().take(12).collect();
    let tail: Vec<&str> = tail.into_iter().rev().collect();
    err_json(&format!(
        "hauksbee-ci produced no result (exit {:?}). {}",
        out.status.code(),
        tail.join(" | ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes the tests that touch the global `ACTIVE_WEB_CHECKS` counter
    /// (every `run_web_check` call acquires a slot, and the concurrency test
    /// asserts on the exact count). Cargo runs tests in parallel, so without this
    /// two counter-touching tests could interleave and consume each other's
    /// slots. Recovered from a poisoned lock so one failing test does not cascade.
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn serial_guard() -> std::sync::MutexGuard<'static, ()> {
        SERIAL.lock().unwrap_or_else(|e| e.into_inner())
    }

    const BOOT_GATE: &str =
        include_str!("../../hauksbee-ci/examples/boards/boot_gate.kicad_pcb");

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
    #[test]
    fn web_check_runs_the_real_ci_binary() {
        let _serial = serial_guard();
        // In a cargo test the current_exe is the test runner deep in target/,
        // so resolve the built binary explicitly.
        let ci = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/release/hauksbee-ci");
        if !ci.is_file() {
            eprintln!("skipping web_check e2e (no release hauksbee-ci built)");
            return;
        }
        std::env::set_var("HAUKSBEE_CI_BIN", &ci);
        let json = run_web_check("boot_gate.kicad_pcb", BOOT_GATE.as_bytes(), None, spec_fragment());
        std::env::remove_var("HAUKSBEE_CI_BIN");
        let v: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert_eq!(v["ok"], true, "run succeeds: {json}");
        assert_eq!(v["passed"], true, "no_faults holds on the demo board: {json}");
        assert!(v["results"].as_array().is_some_and(|r| !r.is_empty()));
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
        let tolerances = "[[tolerance]]\nref = \"R*\"\npercent = 10.0\n".repeat(MAX_WEB_TOLERANCE_RULES);
        let spec = format!(
            "duration_ms = {MAX_WEB_DURATION_MS}\nframe_ms = {MIN_WEB_FRAME_MS}\n\
             [fuzz]\nseeds = {MAX_WEB_FUZZ_SEEDS}\n\
             [ensemble]\nseeds = {MAX_WEB_ENSEMBLE_SEEDS}\n\
             {tolerances}\n{asserts}"
        );
        assert_eq!(validate_web_limits(&spec), Ok(()), "spec at the caps: {spec}");
    }

    /// An integer `duration_ms` at the cap is accepted (the field is f64 in the
    /// spec, but a hand author writes `2000`, not `2000.0`).
    #[test]
    fn integer_duration_at_cap_is_accepted() {
        assert_eq!(validate_web_limits("duration_ms = 2000\n[[assert]]\nkind = \"no_faults\"\n"), Ok(()));
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
            .expect_err("0.001 ms is finer than the 0.05 ms floor");
        assert!(err.contains("frame_ms"), "names the key: {err}");
    }

    #[test]
    fn fuzz_seeds_over_cap_is_rejected() {
        let err = validate_web_limits("[fuzz]\nseeds = 64\n[[assert]]\nkind = \"no_faults\"\n")
            .expect_err("64 seeds exceeds the fuzz cap of 8");
        assert!(err.contains("fuzz") && err.contains("seeds"), "names the field: {err}");
    }

    #[test]
    fn ensemble_seeds_over_cap_is_rejected() {
        let err = validate_web_limits("[ensemble]\nseeds = 128\n[[assert]]\nkind = \"no_faults\"\n")
            .expect_err("128 seeds exceeds the ensemble cap of 16");
        assert!(err.contains("ensemble") && err.contains("seeds"), "names the field: {err}");
    }

    #[test]
    fn too_many_tolerance_rules_rejected() {
        let toosmany = "[[tolerance]]\nref = \"R*\"\npercent = 10.0\n".repeat(MAX_WEB_TOLERANCE_RULES + 1);
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

    /// Acquire the full budget of slots, confirm the next acquire is refused,
    /// then drop one and confirm a slot frees up. Serialized so no other
    /// counter-touching test perturbs the count mid-assertion.
    #[test]
    fn concurrency_budget_is_enforced() {
        let _serial = serial_guard();
        // The counter should be at rest since we hold the serial lock.
        assert_eq!(ACTIVE_WEB_CHECKS.load(Ordering::Acquire), 0, "counter starts clean");

        let mut slots: Vec<WebCheckSlot> = Vec::new();
        for i in 0..MAX_CONCURRENT_WEB_CHECKS {
            slots.push(WebCheckSlot::acquire().unwrap_or_else(|| panic!("slot {i} within budget")));
        }
        // At the cap: the next acquire must fail, and the counter must not have
        // crept past the ceiling (the failed acquire rolled its increment back).
        assert!(WebCheckSlot::acquire().is_none(), "acquire past the cap is refused");
        assert_eq!(ACTIVE_WEB_CHECKS.load(Ordering::Acquire), MAX_CONCURRENT_WEB_CHECKS);

        // Freeing one slot lets exactly one more through.
        drop(slots.pop());
        let regained = WebCheckSlot::acquire().expect("a freed slot is reusable");
        assert!(WebCheckSlot::acquire().is_none(), "still capped after regaining one");

        drop(regained);
        drop(slots);
        assert_eq!(ACTIVE_WEB_CHECKS.load(Ordering::Acquire), 0, "all slots released on drop");
    }
}
