//! End-to-end tests through the real binary: spawn `hauksbee-mcp`, speak the
//! MCP handshake over its stdio, and drive the tools against the repo's real
//! board and firmware fixtures. No mocks anywhere: analyze_board runs the real
//! engine, run_checks runs the real spec runner, and the refusal test feeds
//! real firmware to a genuinely processorless board.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

/// A live server process with line-oriented JSON-RPC helpers.
struct McpClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
}

impl McpClient {
    /// Spawn the binary and complete the initialize / initialized handshake,
    /// asserting on the negotiated version and declared capabilities so every
    /// test exercises the handshake, not only the one that checks it.
    fn start() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_hauksbee-mcp"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn hauksbee-mcp");
        let stdin = child.stdin.take().expect("stdin piped");
        let stdout = BufReader::new(child.stdout.take().expect("stdout piped"));
        let mut client = McpClient {
            child,
            stdin,
            stdout,
            next_id: 0,
        };
        let init = client.request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "mcp-test", "version": "0" },
            }),
        );
        assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
        assert!(
            init["result"]["capabilities"]["tools"].is_object(),
            "server must declare the tools capability: {init}"
        );
        client.notify("notifications/initialized");
        client
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let msg = json!({ "jsonrpc": "2.0", "id": self.next_id, "method": method,
                          "params": params });
        writeln!(self.stdin, "{msg}").expect("write request");
        self.stdin.flush().expect("flush");
        let mut line = String::new();
        self.stdout.read_line(&mut line).expect("read response");
        let v: Value = serde_json::from_str(&line).expect("response is JSON");
        assert_eq!(v["id"], self.next_id, "response id matches request");
        v
    }

    fn notify(&mut self, method: &str) {
        let msg = json!({ "jsonrpc": "2.0", "method": method });
        writeln!(self.stdin, "{msg}").expect("write notification");
        self.stdin.flush().expect("flush");
    }

    /// Call a tool and return (structuredContent, isError).
    fn call_tool(&mut self, name: &str, arguments: Value) -> (Value, bool) {
        let resp = self.request(
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        );
        let result = &resp["result"];
        assert!(
            result.is_object(),
            "tools/call must produce a result, got: {resp}"
        );
        // The text content and structuredContent must agree: parse the text
        // block and compare, so no client tier can see a different story.
        let text: Value = serde_json::from_str(result["content"][0]["text"].as_str().unwrap())
            .expect("content text is JSON");
        assert_eq!(
            text, result["structuredContent"],
            "content text and structuredContent must carry the same object"
        );
        (
            result["structuredContent"].clone(),
            result["isError"].as_bool().unwrap_or(false),
        )
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Absolute path to a repo fixture (tests run with the crate as cwd).
fn fixture(rel: &str) -> String {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("workspace root");
    root.join(rel).display().to_string()
}

#[test]
fn handshake_lists_all_five_tools_with_schemas() {
    let mut c = McpClient::start();
    let resp = c.request("tools/list", json!({}));
    let tools = resp["result"]["tools"].as_array().expect("tools array");
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    for expected in [
        "analyze_board",
        "run_checks",
        "list_capabilities",
        "board_to_code",
        "run_script",
    ] {
        assert!(names.contains(&expected), "missing tool {expected}");
    }
    for t in tools {
        assert_eq!(
            t["inputSchema"]["type"], "object",
            "schema for {}",
            t["name"]
        );
        assert!(
            t["description"].as_str().unwrap().len() > 40,
            "description for {} is too thin to guide an agent",
            t["name"]
        );
    }
}

#[test]
fn analyze_board_returns_the_front_door_report_structure() {
    let mut c = McpClient::start();
    let (report, is_error) = c.call_tool(
        "analyze_board",
        json!({ "board_path": fixture("testdata/boards/button_pullup.kicad_pcb") }),
    );
    assert!(
        !is_error,
        "analysis of a valid board is not an error: {report}"
    );
    assert_eq!(report["ok"], true);
    assert_eq!(report["file_name"], "button_pullup.kicad_pcb");
    assert!(report["headline"].as_str().unwrap().len() > 5);
    assert!(report["serious"].is_number() && report["total"].is_number());
    let sections = report["sections"].as_array().expect("sections");
    let titles: Vec<&str> = sections
        .iter()
        .map(|s| s["title"].as_str().unwrap())
        .collect();
    assert!(
        titles.iter().any(|t| t.contains("DRC")),
        "DRC section present"
    );
    // Bind coverage is part of the contract: the resolved/unresolved counts
    // must be present as data, not summarized away.
    assert!(report["bind"].is_object(), "bind summary present: {report}");
    assert!(
        report["nets"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n == "BTN"),
        "board nets are listed"
    );
}

#[test]
fn run_checks_returns_per_assertion_results_and_verdict() {
    let mut c = McpClient::start();
    let spec = r#"
name = "mcp inline spec"
duration_ms = 10

[[supply]]
net = "+5V"
kind = "ideal"
volts = 5.0

[[assert]]
kind = "voltage"
net = "+5V"
min = 4.5

[[assert]]
kind = "no_faults"
"#;
    let (verdict, is_error) = c.call_tool(
        "run_checks",
        json!({
            "board_path": fixture("testdata/boards/button_pullup.kicad_pcb"),
            "spec_toml": spec,
        }),
    );
    assert!(!is_error, "inline spec should run: {verdict}");
    // The full exit-code semantics ride along as data.
    assert_eq!(verdict["exit_code"], 0, "spec should be green: {verdict}");
    assert_eq!(verdict["passed"], true);
    assert_eq!(verdict["assertions_passed"], true);
    assert_eq!(verdict["run_valid"], true);
    let results = verdict["results"]
        .as_array()
        .expect("per-assertion results");
    assert_eq!(results.len(), 2);
    for r in results {
        assert!(r["label"].is_string() && r["kind"].is_string());
        assert_eq!(r["passed"], true);
        assert_eq!(r["invalid"], false);
    }
    assert!(
        verdict["results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["kind"] == "voltage"),
        "the voltage assertion is reported by kind"
    );
}

#[test]
fn run_checks_rejects_a_board_key_in_the_spec_body() {
    let mut c = McpClient::start();
    let (v, is_error) = c.call_tool(
        "run_checks",
        json!({
            "board_path": fixture("testdata/boards/button_pullup.kicad_pcb"),
            "spec_toml": "board = \"elsewhere.kicad_pcb\"\n",
        }),
    );
    assert!(is_error);
    assert!(v["error"].as_str().unwrap().contains("board"), "{v}");
}

/// The honesty contract, load-bearing: firmware handed to a board with no
/// processor is an unanswerable question. The result must be the structured
/// refusal, with the static report attached as data, never a fabricated
/// verdict and never a generic error.
#[test]
fn firmware_on_a_processorless_board_returns_the_refusal_shape() {
    let mut c = McpClient::start();
    let (v, is_error) = c.call_tool(
        "analyze_board",
        json!({
            "board_path": fixture("testdata/boards/button_pullup.kicad_pcb"),
            "firmware_path": fixture("testdata/firmware/demo/demo.hex"),
        }),
    );
    assert!(
        !is_error,
        "a refusal is a structured result, not an error: {v}"
    );
    assert_eq!(v["status"], "invalid_for_analysis", "refusal status: {v}");
    let reason = v["reason"].as_str().expect("refusal carries a reason");
    assert!(!reason.is_empty());
    for key in [
        "claim",
        "missing_prerequisite",
        "valid_partial_conclusions",
        "next_action",
    ] {
        assert!(
            v["refusal"].get(key).is_some(),
            "MCP refusal lost {key}: {v}"
        );
    }
    assert_eq!(
        v["refusal"], v["report"]["refusal"],
        "MCP must pass through the engine's one typed refusal contract"
    );
    // The static report still rides along: refusing the firmware question
    // must not withhold the answerable static findings.
    assert_eq!(v["report"]["ok"], true);
    assert_eq!(v["report"]["file_name"], "button_pullup.kicad_pcb");
}

#[test]
fn list_capabilities_reports_backends_probed_on_this_machine() {
    let mut c = McpClient::start();
    let (v, is_error) = c.call_tool("list_capabilities", json!({}));
    assert!(!is_error);
    let backends = v["backends"].as_array().expect("backends");
    let names: Vec<&str> = backends
        .iter()
        .map(|b| b["name"].as_str().unwrap())
        .collect();
    for expected in ["avr", "qemu-xtensa", "qemu-riscv32", "renode"] {
        assert!(names.contains(&expected), "backend row for {expected}");
    }
    for b in backends {
        assert!(b["available"].is_boolean());
        assert!(
            ["ok", "builtin", "absent", "disabled"].contains(&b["status"].as_str().unwrap()),
            "doctor-style status token: {b}"
        );
    }
    assert!(v["assertions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|a| a == "voltage"));
    assert!(v["formats"]["board"].as_array().unwrap().len() >= 5);
}

#[test]
fn board_to_code_returns_the_editable_text_form() {
    let mut c = McpClient::start();
    let (v, is_error) = c.call_tool(
        "board_to_code",
        json!({ "board_path": fixture("testdata/boards/button_pullup.kicad_pcb") }),
    );
    assert!(!is_error, "{v}");
    assert_eq!(v["board"], "button_pullup.kicad_pcb");
    let code = v["code"].as_str().expect("code text");
    assert!(
        code.contains("BTN"),
        "the code names the board's nets:\n{code}"
    );
}

#[test]
fn missing_board_file_is_a_tool_error_not_a_refusal() {
    let mut c = McpClient::start();
    let (v, is_error) = c.call_tool(
        "analyze_board",
        json!({ "board_path": "/nonexistent/nowhere.kicad_pcb" }),
    );
    assert!(is_error, "an unreadable path is an input error");
    assert!(v["error"].as_str().unwrap().contains("nowhere.kicad_pcb"));
    assert!(v.get("status").is_none(), "input errors are not refusals");
}

/// Code mode end to end: one tools/call runs a script that analyzes the
/// board, inspects the finding count, conditionally runs a check spec, and
/// returns a combined verdict, with console.log captured.
#[test]
fn run_script_composes_analyze_and_checks_in_one_call() {
    let mut c = McpClient::start();
    let board = fixture("testdata/boards/button_pullup.kicad_pcb");
    let source = format!(
        r#"
const board = {board:?};
const report = hauksbee.analyzeBoard(board);
console.log("findings:", report.total, "serious:", report.serious);
let checks = null;
if (report.serious === 0) {{
    checks = hauksbee.runChecks(board,
        'duration_ms = 10\n' +
        '[[supply]]\nnet = "+5V"\nkind = "ideal"\nvolts = 5.0\n' +
        '[[assert]]\nkind = "voltage"\nnet = "+5V"\nmin = 4.5\n');
}}
return {{
    board: report.file_name,
    findings: report.total,
    serious: report.serious,
    checksRan: checks !== null,
    checksPassed: checks === null ? null : checks.passed,
    exitCode: checks === null ? null : checks.exit_code,
}};
"#
    );
    let (v, is_error) = c.call_tool("run_script", json!({ "source": source }));
    assert!(!is_error, "script should complete: {v}");
    let result = &v["result"];
    assert_eq!(result["board"], "button_pullup.kicad_pcb");
    assert_eq!(
        result["checksRan"], true,
        "clean board should run checks: {v}"
    );
    assert_eq!(result["checksPassed"], true);
    assert_eq!(result["exitCode"], 0);
    let logs = v["logs"].as_array().expect("captured logs");
    assert!(
        logs.iter()
            .any(|l| l.as_str().unwrap().starts_with("findings:")),
        "console.log is captured: {logs:?}"
    );
}

/// Inside the sandbox the refusal is thrown, catchable, and structurally the
/// same object the plain tool returns; and the sandbox has no filesystem or
/// network globals to escape through.
#[test]
fn run_script_throws_refusals_and_has_no_ambient_capabilities() {
    let mut c = McpClient::start();
    let board = fixture("testdata/boards/button_pullup.kicad_pcb");
    let fw = fixture("testdata/firmware/demo/demo.hex");
    let source = format!(
        r#"
let caught = null;
try {{
    hauksbee.analyzeBoard({board:?}, {fw:?});
}} catch (e) {{
    caught = e;
}}
return {{
    status: caught === null ? "no-throw" : caught.status,
    hasReason: caught !== null && typeof caught.reason === "string",
    ambient: [typeof require, typeof process, typeof fetch,
              typeof XMLHttpRequest, typeof os, typeof std],
}};
"#
    );
    let (v, is_error) = c.call_tool("run_script", json!({ "source": source }));
    assert!(!is_error, "{v}");
    assert_eq!(v["result"]["status"], "invalid_for_analysis", "{v}");
    assert_eq!(v["result"]["hasReason"], true);
    for t in v["result"]["ambient"].as_array().unwrap() {
        assert_eq!(t, "undefined", "no ambient capability may exist: {v}");
    }
}

/// An uncaught structured throw surfaces as an error result carrying the
/// thrown object, so even a script that forgets try/catch cannot flatten a
/// refusal into a generic failure string.
#[test]
fn run_script_uncaught_refusal_carries_the_structured_throw() {
    let mut c = McpClient::start();
    let board = fixture("testdata/boards/button_pullup.kicad_pcb");
    let fw = fixture("testdata/firmware/demo/demo.hex");
    let source = format!("hauksbee.analyzeBoard({board:?}, {fw:?}); return 1;");
    let (v, is_error) = c.call_tool("run_script", json!({ "source": source }));
    assert!(is_error, "uncaught throw is an error result: {v}");
    assert_eq!(v["thrown"]["status"], "invalid_for_analysis", "{v}");
}

// ── CLI flag answers (round-9 #29) ──────────────────────────────────────────
//
// `--version` and `--help` used to exit 0 with NOTHING on stdout, which told a
// packaging smoke test (or a human checking what they installed) precisely
// nothing. A flag answer must be a real answer.

#[test]
fn version_flag_prints_the_version_and_exits_zero() {
    for flag in ["--version", "-V"] {
        let out = Command::new(env!("CARGO_BIN_EXE_hauksbee-mcp"))
            .arg(flag)
            .output()
            .expect("run hauksbee-mcp");
        assert!(out.status.success(), "{flag} must exit 0");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert_eq!(
            stdout.trim(),
            format!("hauksbee-mcp {}", env!("CARGO_PKG_VERSION")),
            "{flag} stdout: {stdout:?}"
        );
    }
}

#[test]
fn help_flag_prints_usage_and_exits_zero() {
    for flag in ["--help", "-h"] {
        let out = Command::new(env!("CARGO_BIN_EXE_hauksbee-mcp"))
            .arg(flag)
            .output()
            .expect("run hauksbee-mcp");
        assert!(out.status.success(), "{flag} must exit 0");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains("USAGE"), "{flag} stdout: {stdout}");
        assert!(
            stdout.contains("MCP stdio server"),
            "help must say what this binary is: {stdout}"
        );
        assert!(stdout.contains("--version"), "{flag} stdout: {stdout}");
    }
}

#[test]
fn unknown_flag_fails_loudly_instead_of_starting_the_server() {
    let out = Command::new(env!("CARGO_BIN_EXE_hauksbee-mcp"))
        .arg("--definitely-not-a-flag")
        .output()
        .expect("run hauksbee-mcp");
    assert!(!out.status.success(), "an unknown flag must not exit 0");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--definitely-not-a-flag") && stderr.contains("--help"),
        "stderr must name the argument and point at --help: {stderr}"
    );
}
