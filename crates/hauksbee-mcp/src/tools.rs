//! The MCP tool surface: definitions (name, agent-facing description, JSON
//! Schema) and implementations for `analyze_board`, `run_checks`,
//! `list_capabilities`, `board_to_code`, and `run_script`. Everything routes
//! through the engine's own library front door (`analyze_json`,
//! `hauksbee_ci::run`, the doctor's backend resolvers), never a re-implemented
//! sniff or probe, so what the MCP server reports can never drift from what
//! the CLI would say for the same inputs. The honesty contract is enforced
//! here: an unanswerable run (the CLI's exit-3 territory) becomes a structured
//! `{status: "invalid_for_analysis", reason}` refusal, never a fabricated
//! result and never a generic error.

use serde_json::{json, Value};

/// One tool call's outcome. `is_error` maps to the MCP `isError` flag: true
/// only for genuine input/execution errors (unreadable file, bad TOML, a
/// crashed script). An honest refusal is NOT an error: it is a structured
/// result the agent must read, so it travels with `is_error == false`.
pub struct ToolResult {
    pub value: Value,
    pub is_error: bool,
}

impl ToolResult {
    fn ok(value: Value) -> Self {
        ToolResult {
            value,
            is_error: false,
        }
    }
    fn err(message: impl Into<String>) -> Self {
        ToolResult {
            value: json!({ "error": message.into() }),
            is_error: true,
        }
    }
}

/// The refusal status token. One spelling, shared by every tool and the
/// script sandbox, so an agent can match on it without per-tool special cases.
pub const INVALID_FOR_ANALYSIS: &str = "invalid_for_analysis";

/// The `tools/list` payload: every tool with its input schema. Descriptions
/// are written for an agent caller: they state the contract (what comes back,
/// what a refusal looks like) rather than marketing the feature.
pub fn definitions() -> Value {
    json!([
        {
            "name": "analyze_board",
            "description": "Full physics-grounded analysis of a PCB design file (KiCad .kicad_pcb/.kicad_sch, Eagle .brd, Altium .PcbDoc, IPC-D-356 .d356, gerber zip, or Board-as-Code .board). Returns the front-door report JSON: overall headline, serious/total finding counts, per-section findings (DRC, connectivity, signal integrity), bind coverage (which components resolved to models), nets, detected supplies, and honesty notes. With firmware_path (.elf/.hex, a PlatformIO project, or a zip of either) it also runs a short firmware co-sim and attaches a `cosim` section. HONESTY CONTRACT: if the firmware question cannot be answered (no MCU on the board, external-only backend, firmware failed to load, or the analog solve aborted) the result is {\"status\":\"invalid_for_analysis\",\"reason\":...,\"report\":...} with the static report riding along. Never treat a refusal as pass or fail; it means the run declined to vouch for itself. Coverage degradations arrive as data fields, not prose; surface them. The front-door report carries the substituted-MCU-core caveat, driver contentions and short pulses. It does NOT carry dropped ADC channels or unexercised buses: those two reach `hauksbee run --json` and the `run_checks` tool's `coverage_warnings`, so use `run_checks` when the board has an unmapped ADC channel or a bus slave on a controller-less platform.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "board_path": {
                        "type": "string",
                        "description": "Path to the board design file on this machine."
                    },
                    "firmware_path": {
                        "type": "string",
                        "description": "Optional path to compiled firmware (.elf/.hex), a PlatformIO project directory, or a zip containing either."
                    }
                },
                "required": ["board_path"]
            }
        },
        {
            "name": "run_checks",
            "description": "Run a hauksbee-ci check spec against a board: boots optional firmware on the emulated MCU, runs the analog co-sim, and evaluates the spec's assertions (voltage, rail_window, uart, toggle, no_faults, max_current, max_temp, boot-coverage, peripheral, hwtrace; plus tolerance ensembles and transient scenarios). `spec_toml` is the spec BODY as TOML text WITHOUT `board`/`firmware` keys; those are injected from board_path/firmware_path. Returns {passed, assertions_passed, run_valid, exit_code, analog_abort, seeds, coverage, substitutions, coverage_warnings, results[]} where each result is {label, kind, passed, invalid, detail, failing_seed, failing_seeds, seeds_total}. Exit-code semantics: 0 all green, 1 an assertion failed (results say which and on which seed), 3 invalid-for-analysis. HONESTY CONTRACT: exit 3 comes back as {\"status\":\"invalid_for_analysis\",\"reason\":...,\"result\":...}; never average it into a pass rate. `passed` is the process verdict (false on exit 3); read it only alongside `run_valid`.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "board_path": {
                        "type": "string",
                        "description": "Path to the board design file. Injected into the spec as its `board` key."
                    },
                    "spec_toml": {
                        "type": "string",
                        "description": format!("The hauksbee-ci spec as TOML text, WITHOUT `board` or `firmware` keys (they are injected from the path arguments). Format: {}. Minimal example: duration_ms = 10 plus one [[assert]] block.", hauksbee_ir::docs_url("docs/ci/CI.md"))
                    },
                    "firmware_path": {
                        "type": "string",
                        "description": "Optional firmware path, injected as the spec's `firmware` key."
                    }
                },
                "required": ["board_path", "spec_toml"]
            }
        },
        {
            "name": "list_capabilities",
            "description": "The scope table as data: which analysis checks and spec assertion kinds exist, which MCU co-sim backends are available ON THIS MACHINE RIGHT NOW (probed with the engine's own backend discovery, the same resolvers a real co-sim uses, so this cannot drift from what a run would accept), and which board/firmware formats are accepted. Call this before promising a user a co-sim: a backend with available=false means firmware for that MCU family will refuse or substitute, and that substitution shows up in run results as data.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "board_to_code",
            "description": "Decompile a text-format board file (KiCad .kicad_pcb or Eagle .brd XML) into Board-as-Code: the editable text form a coding agent can diff, modify, and feed back to analyze_board as a .board file. Binary formats (Altium .PcbDoc) and gerber archives have no text form and return an error. Result: {board, code}.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "board_path": {
                        "type": "string",
                        "description": "Path to the board design file (text formats only)."
                    }
                },
                "required": ["board_path"]
            }
        },
        {
            "name": "run_script",
            "description": "Code mode: run a JavaScript program server-side against the hauksbee API and return the composed result in ONE call, instead of many tool round-trips. The sandbox (embedded QuickJS) exposes exactly one capability, the global `hauksbee` object: analyzeBoard(path, firmwarePath?), runChecks(path, specToml, firmwarePath?), listCapabilities(), boardToCode(path). Each returns the same JSON object the corresponding tool returns. console.log(...) is captured into the response. The JS environment itself has no filesystem, no network, no imports and no other globals beyond the JS builtins. Note what that does NOT mean: `hauksbee.analyzeBoard(path)` reads any path on the machine and `hauksbee.runChecks(...)` can build a firmware project, which runs that project's build scripts. Treat those two as the capabilities they are, and do not pass a path or a spec that came from content you do not trust. The script runs as a function body: use `return` for its result, which must be JSON-serializable. HONESTY CONTRACT inside the sandbox: a refusal is THROWN as a structured error object with .status === \"invalid_for_analysis\" so a script cannot accidentally treat it as data; catch it if you want to handle it. Tool input errors are thrown as {error: message}. Response: {result, logs}; an uncaught throw comes back as an error with {thrown, logs}. Scripts are killed after 120 seconds.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "source": {
                        "type": "string",
                        "description": "JavaScript source. Runs as a function body: use `return` for the final value. Example: const r = hauksbee.analyzeBoard(\"b.kicad_pcb\"); return {serious: r.serious};"
                    }
                },
                "required": ["source"]
            }
        }
    ])
}

/// Dispatch one tool call by name. Unknown names are an error result (MCP
/// wants tool-level failures inside the result, not a protocol error).
pub fn call(name: &str, args: &Value) -> ToolResult {
    match name {
        "analyze_board" => analyze_board(args),
        "run_checks" => run_checks(args),
        "list_capabilities" => list_capabilities(),
        "board_to_code" => board_to_code(args),
        "run_script" => run_script(args),
        other => ToolResult::err(format!(
            "unknown tool '{other}'; call tools/list for the available tools"
        )),
    }
}

/// Dispatch for the script sandbox: the same tools, minus `run_script` itself
/// (a script spawning nested sandboxes gains nothing and could recurse).
pub fn call_from_script(name: &str, args: &Value) -> ToolResult {
    match name {
        "run_script" => ToolResult::err("run_script is not callable from inside a script"),
        other => call(other, args),
    }
}

/// Required string argument, or a message naming exactly what is missing so an
/// agent can repair the call without guessing.
fn req_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("missing required string argument '{key}'"))
}

/// Optional string argument (absent, null, or a string; anything else is the
/// caller's bug and reads as absent rather than silently stringified).
fn opt_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

/// The display name the front door sees: the file name (it drives format
/// sniffing for `.board` and gerber zips), never the whole path.
fn display_name(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string())
}

/// `analyze_board`: read the board (and optional firmware) bytes and run the
/// engine's front-door analysis, the exact code path `hauksbee serve` injects.
/// The refusal logic lives here and nowhere else: a firmware run whose co-sim
/// could not run or whose analog solve failed is unanswerable, and comes back
/// as a structured refusal with the static report attached as data.
fn analyze_board(args: &Value) -> ToolResult {
    let board_path = match req_str(args, "board_path") {
        Ok(p) => p,
        Err(e) => return ToolResult::err(e),
    };
    let bytes = match std::fs::read(board_path) {
        Ok(b) => b,
        Err(e) => return ToolResult::err(format!("could not read board file '{board_path}': {e}")),
    };
    let name = display_name(board_path);
    let fw = opt_str(args, "firmware_path");
    let report_json = match fw {
        Some(fw_path) => {
            let fw_bytes = match std::fs::read(fw_path) {
                Ok(b) => b,
                Err(e) => {
                    return ToolResult::err(format!(
                        "could not read firmware file '{fw_path}': {e}"
                    ))
                }
            };
            hauksbee_engine::analyze_with_firmware_json(
                &name,
                &bytes,
                &display_name(fw_path),
                &fw_bytes,
            )
        }
        None => hauksbee_engine::analyze_json(&name, &bytes),
    };
    let report: Value = match serde_json::from_str(&report_json) {
        Ok(v) => v,
        Err(e) => return ToolResult::err(format!("engine returned unparseable JSON: {e}")),
    };
    // An unreadable/unparseable board is an INPUT error (the CLI's exit-2
    // class), not a refusal: the caller sent something that is not a board.
    if report.get("ok").and_then(Value::as_bool) == Some(false) {
        let msg = report
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("the board file could not be read as any supported format");
        return ToolResult::err(format!("input error: {msg}"));
    }
    if fw.is_some() {
        if let Some(refusal) = firmware_refusal(&report) {
            return ToolResult::ok(refusal);
        }
    }
    ToolResult::ok(report)
}

/// Decide whether a firmware-carrying analysis is answerable. Two unanswerable
/// shapes, both the CLI's exit-3 territory: the co-sim never ran (no bound MCU,
/// external-only backend, firmware failed to load), or it ran and the analog
/// solve aborted. Returns the structured refusal carrying the report, so the
/// static findings, and the coverage holes the front door carries (substituted
/// core, driver contentions, short pulses), still reach the caller as data.
/// Dropped ADC channels and unexercised buses do not: see `analyze_board`'s
/// description.
fn firmware_refusal(report: &Value) -> Option<Value> {
    // The engine owns the typed contract. MCP is a transport: passing the same
    // object through prevents a second renderer from drifting on claim scope,
    // surviving conclusions, or remediation.
    let refusal = report.get("refusal")?.clone();
    let reason = refusal
        .get("missing_prerequisite")
        .and_then(Value::as_str)
        .unwrap_or("the requested firmware co-simulation could not produce a trustworthy answer");
    Some(json!({
        "status": INVALID_FOR_ANALYSIS,
        "reason": reason,
        "refusal": refusal,
        "report": report,
    }))
}

/// `run_checks`: stage the caller's spec body in a temp dir with the board
/// (and firmware) paths injected as real TOML, then run it through
/// `hauksbee_ci::run`, the same library entry the `hauksbee-ci` binary uses.
/// Exit 3 becomes the structured refusal; 0/1 return the full verdict JSON
/// with per-assertion results and the exit-code semantics as data.
fn run_checks(args: &Value) -> ToolResult {
    let board_path = match req_str(args, "board_path") {
        Ok(p) => p,
        Err(e) => return ToolResult::err(e),
    };
    let spec_body = match req_str(args, "spec_toml") {
        Ok(s) => s,
        Err(e) => return ToolResult::err(e),
    };
    // Canonicalize now: the staged spec lives in a temp dir, so a relative
    // board path would resolve against the wrong base. This also front-loads
    // the missing-file error with the caller's own path in it.
    let board_abs = match std::fs::canonicalize(board_path) {
        Ok(p) => p,
        Err(e) => return ToolResult::err(format!("board file '{board_path}': {e}")),
    };
    let firmware_abs = match opt_str(args, "firmware_path") {
        Some(f) => match std::fs::canonicalize(f) {
            Ok(p) => Some(p),
            Err(e) => return ToolResult::err(format!("firmware file '{f}': {e}")),
        },
        None => None,
    };
    // Parse the body first: a TOML syntax error should name the caller's spec,
    // not the temp file; and a `board`/`firmware` key in the body would fight
    // the injected one, so reject it with instructions instead of letting a
    // duplicate-key parse error confuse the caller.
    let parsed: toml::Value = match spec_body.parse() {
        Ok(v) => v,
        Err(e) => return ToolResult::err(format!("spec_toml is not valid TOML: {e}")),
    };
    if let Some(table) = parsed.as_table() {
        for key in ["board", "firmware"] {
            if table.contains_key(key) {
                return ToolResult::err(format!(
                    "spec_toml must not contain a '{key}' key; it is injected from the \
                     '{key}_path' argument"
                ));
            }
        }
    }
    // Serialize the injected paths as real TOML (quoting and escaping handled
    // by the toml crate), then append the caller's body verbatim.
    let mut header = toml::value::Table::new();
    header.insert(
        "board".to_string(),
        toml::Value::String(board_abs.display().to_string()),
    );
    if let Some(fw) = &firmware_abs {
        header.insert(
            "firmware".to_string(),
            toml::Value::String(fw.display().to_string()),
        );
    }
    let header_text = match toml::to_string(&toml::Value::Table(header)) {
        Ok(t) => t,
        Err(e) => return ToolResult::err(format!("could not serialize injected paths: {e}")),
    };
    let dir = match tempfile::tempdir() {
        Ok(d) => d,
        Err(e) => return ToolResult::err(format!("could not create a temp dir: {e}")),
    };
    let spec_path = dir.path().join("spec.toml");
    if let Err(e) = std::fs::write(&spec_path, format!("{header_text}\n{spec_body}")) {
        return ToolResult::err(format!("could not stage the spec: {e}"));
    }
    let cfg = hauksbee_ci::RunConfig {
        spec: spec_path,
        seed: None,
        models_dir: None,
    };
    let result = match hauksbee_ci::run(&cfg) {
        Ok(r) => r,
        Err(e) => return ToolResult::err(format!("spec error: {e}")),
    };
    // render_json is the canonical machine shape ({passed, assertions_passed,
    // run_valid, exit_code, results[], substitutions, coverage_warnings});
    // reusing it means the MCP surface can never tell a cleaner story than the
    // CLI --json surface does for the same run.
    let value: Value = match serde_json::from_str(&result.render_json()) {
        Ok(v) => v,
        Err(e) => return ToolResult::err(format!("could not parse the run result: {e}")),
    };
    if result.exit_code() == hauksbee_engine::result::EXIT_INVALID_FOR_ANALYSIS {
        let refusal = result.refusal().expect("exit 3 has a structured refusal");
        return ToolResult::ok(json!({
            "status": INVALID_FOR_ANALYSIS,
            "reason": refusal.missing_prerequisite.clone(),
            "refusal": refusal,
            "result": value,
        }));
    }
    ToolResult::ok(value)
}

/// `list_capabilities`: the scope table as data. Backend availability uses the
/// engine's OWN resolvers (the exact functions the scheduler calls), mirroring
/// the `hauksbee doctor` subcommand; the check/assertion/format lists are the
/// contract stated in agents/AGENTS.md, kept here as data an agent can branch
/// on instead of parsing prose.
fn list_capabilities() -> ToolResult {
    ToolResult::ok(json!({
        "reports": [
            {"name": "drc", "what": "copper clearance/short detection from the layout geometry"},
            {"name": "lint", "what": "connectivity, strap, and MCU resource-conflict checks"},
            {"name": "si", "what": "signal-integrity heuristics (trace ampacity, ripple, stubs)"},
            {"name": "thermal", "what": "junction temperature estimates from dissipation"},
            {"name": "ac", "what": "small-signal AC sweeps on selected nets"},
            {"name": "usb_c", "what": "USB-C CC termination classification"},
            {"name": "cosim", "what": "firmware co-simulation with analog coupling and stress faults"},
        ],
        "assertions": [
            "voltage", "rail_window", "uart", "toggle", "no_faults", "max_current",
            "max_temp", "boot-coverage", "peripheral", "hwtrace",
        ],
        "spec_features": ["tolerance ensembles (monte-carlo, corners)", "transient scenarios",
                           "supplies (ideal/bench/wall/usb/battery)", "as-built overlays",
                           "peripherals (buttons, pots, encoders, i2c/spi sensors)"],
        "backends": backend_probes(),
        "formats": {
            "board": [".kicad_pcb", ".kicad_sch", ".brd (Eagle)", ".PcbDoc (Altium)",
                       ".d356 (IPC-D-356)", "gerber folder or zip", ".board (Board-as-Code, bare or zipped)"],
            "firmware": [".elf", ".hex", "PlatformIO project directory", "zip of either"],
        },
    }))
}

/// Probe each MCU backend with the engine's own discovery, exactly like
/// `hauksbee doctor --backends --json`. `status` tokens match doctor's:
/// `builtin` (linked into this binary), `ok` (external tool resolved),
/// `absent` (feature compiled in, tool not found), `disabled` (compiled out).
fn backend_probes() -> Vec<Value> {
    let mut out = Vec::new();

    #[cfg(feature = "avr")]
    out.push(json!({
        "name": "avr", "status": "builtin", "available": true,
        "detail": "simavr linked into this binary",
        "summary": "ATmega / ATtiny firmware co-sim",
    }));
    #[cfg(not(feature = "avr"))]
    out.push(json!({
        "name": "avr", "status": "disabled", "available": false,
        "detail": "compiled out; rebuild with the avr feature (GPL-3.0 libsimavr)",
        "summary": "ATmega / ATtiny firmware co-sim",
    }));

    #[cfg(feature = "qemu")]
    {
        use hauksbee_mcu::qemu::{find_qemu, QemuArch};
        for (name, arch, summary) in [
            (
                "qemu-xtensa",
                QemuArch::Xtensa,
                "ESP32 / ESP32-S3 firmware co-sim (Espressif QEMU fork)",
            ),
            (
                "qemu-riscv32",
                QemuArch::Riscv32,
                "ESP32-C3 firmware co-sim (Espressif QEMU fork)",
            ),
        ] {
            match find_qemu(arch) {
                Ok(p) => out.push(json!({
                    "name": name, "status": "ok", "available": true,
                    "detail": p.display().to_string(), "summary": summary,
                })),
                Err(e) => out.push(json!({
                    "name": name, "status": "absent", "available": false,
                    "detail": first_line(&e.to_string()), "summary": summary,
                })),
            }
        }
    }
    #[cfg(not(feature = "qemu"))]
    for (name, summary) in [
        ("qemu-xtensa", "ESP32 / ESP32-S3 firmware co-sim"),
        ("qemu-riscv32", "ESP32-C3 firmware co-sim"),
    ] {
        out.push(json!({
            "name": name, "status": "disabled", "available": false,
            "detail": "built without the `qemu` feature", "summary": summary,
        }));
    }

    #[cfg(feature = "renode")]
    match hauksbee_mcu::renode::find_renode() {
        Ok(p) => out.push(json!({
            "name": "renode", "status": "ok", "available": true,
            "detail": p.display().to_string(),
            "summary": "STM32 / nRF52 / RISC-V firmware co-sim",
        })),
        Err(e) => out.push(json!({
            "name": "renode", "status": "absent", "available": false,
            "detail": first_line(&e.to_string()),
            "summary": "STM32 / nRF52 / RISC-V firmware co-sim",
        })),
    }
    #[cfg(not(feature = "renode"))]
    out.push(json!({
        "name": "renode", "status": "disabled", "available": false,
        "detail": "built without the `renode` feature",
        "summary": "STM32 / nRF52 / RISC-V firmware co-sim",
    }));

    out
}

/// Collapse a possibly multi-line resolver error to its first line, keeping
/// the capability table one row per backend (same rationale as doctor's).
#[cfg(any(feature = "qemu", feature = "renode"))]
fn first_line(msg: &str) -> String {
    msg.lines().next().unwrap_or("").to_string()
}

/// `board_to_code`: the editable Board-as-Code text form, via the engine's
/// decompiler. Text formats only; a binary board has no text to decompile and
/// gets a plain error saying so instead of a mangled lossy rendering.
fn board_to_code(args: &Value) -> ToolResult {
    let board_path = match req_str(args, "board_path") {
        Ok(p) => p,
        Err(e) => return ToolResult::err(e),
    };
    let bytes = match std::fs::read(board_path) {
        Ok(b) => b,
        Err(e) => return ToolResult::err(format!("could not read board file '{board_path}': {e}")),
    };
    // A format hauksbee recognises and deliberately does not read gets named,
    // with the action that unlocks it. The catch-all below cannot do that: it
    // guessed "e.g. Altium .PcbDoc" at every binary file, which is the wrong
    // vendor for a pre-Eagle-6 board and gives its owner nothing to act on.
    if let Some(message) =
        hauksbee_engine::board_input::unsupported_format_refusal(&display_name(board_path), &bytes)
    {
        return ToolResult::err(message);
    }
    let text = match String::from_utf8(bytes) {
        Ok(t) => t,
        Err(_) => {
            return ToolResult::err(
                "this board file is binary (e.g. Altium .PcbDoc); board_to_code needs a \
                 text format (.kicad_pcb or Eagle .brd XML)",
            )
        }
    };
    match hauksbee_engine::decompile_any_to_code(&text) {
        Ok(code) => ToolResult::ok(json!({
            "board": display_name(board_path),
            "code": code,
        })),
        Err(e) => ToolResult::err(format!("could not decompile the board: {e}")),
    }
}

/// `run_script`: hand the source to the QuickJS sandbox. Failures come back
/// with the captured logs attached, because a half-run script's logs are the
/// only forensic trail the caller has.
fn run_script(args: &Value) -> ToolResult {
    let source = match req_str(args, "source") {
        Ok(s) => s,
        Err(e) => return ToolResult::err(e),
    };
    match crate::script::run(source, std::time::Duration::from_secs(120)) {
        Ok(v) => ToolResult::ok(v),
        Err(v) => ToolResult {
            value: v,
            is_error: true,
        },
    }
}
