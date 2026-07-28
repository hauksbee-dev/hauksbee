//! The `run_script` code-mode sandbox: an embedded QuickJS (via rquickjs, MIT
//! licensed, bundled C, no system dependency) whose ONLY capability is the
//! `hauksbee` API. An agent submits one JavaScript program; it runs
//! server-side, composing analyzeBoard / runChecks / listCapabilities /
//! boardToCode calls and returning one result, instead of paying a round-trip
//! per tool call (the Cloudflare "code mode" pattern). The bridge is
//! deliberately narrow: one native function that takes a tool name and a JSON
//! args string and returns a JSON result string, one native log sink, and a
//! JS prelude that wraps them in the typed `hauksbee` object. Everything else
//! (no filesystem, no network, no timers, no imports) simply does not exist in
//! the QuickJS instance we build. The honesty contract holds inside the
//! sandbox: refusals are THROWN as structured errors so a script cannot
//! mistake them for data, and a script may catch and branch on them.

use rquickjs::{Context, Ctx, Function, Runtime, Value as JsValue};
use serde_json::{json, Value};
use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

/// Memory ceiling for a script run. Generous for JSON crunching, small enough
/// that a runaway script cannot take the server down with it.
const MEMORY_LIMIT_BYTES: usize = 256 * 1024 * 1024;

/// The JS prelude evaluated before the user script: builds `console.log` and
/// the `hauksbee` object over the two native bridge functions. Refusals
/// (status == invalid_for_analysis) and tool errors both THROW here, so the
/// user script sees them as exceptions, never as ordinary return values.
const PRELUDE: &str = r#"
globalThis.console = {
    log: (...args) => __hauksbee_log(args.map(a =>
        typeof a === "string" ? a : JSON.stringify(a)).join(" ")),
};
const __call = (name, args) => {
    const r = JSON.parse(__hauksbee_call(name, JSON.stringify(args)));
    if (!r.ok) throw { error: r.error };
    if (r.result && r.result.status === "invalid_for_analysis") throw r.result;
    return r.result;
};
globalThis.hauksbee = {
    analyzeBoard: (path, firmwarePath) =>
        __call("analyze_board", { board_path: path, firmware_path: firmwarePath }),
    runChecks: (path, specToml, firmwarePath) =>
        __call("run_checks", { board_path: path, spec_toml: specToml,
                               firmware_path: firmwarePath }),
    listCapabilities: () => __call("list_capabilities", {}),
    boardToCode: (path) => __call("board_to_code", { board_path: path }),
};
"#;

/// Run one script. Ok carries `{result, logs}`; Err carries `{error, thrown?,
/// logs}`. Logs ride along on BOTH paths: a failed script's console output is
/// the only forensic trail the caller has.
pub fn run(source: &str, timeout: Duration) -> Result<Value, Value> {
    let logs: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    let outcome = run_inner(source, timeout, logs.clone());
    let logs = json!(logs.borrow().clone());
    match outcome {
        Ok(result) => Ok(json!({ "result": result, "logs": logs })),
        Err(mut failure) => {
            // Attach the logs to the failure object built by run_inner.
            if let Some(obj) = failure.as_object_mut() {
                obj.insert("logs".to_string(), logs);
            }
            Err(failure)
        }
    }
}

/// The engine-room half of [`run`]: build the runtime, arm the wall-clock
/// interrupt and the memory ceiling, install the bridge, evaluate the prelude
/// and then the user source wrapped in a function body (so `return` works).
fn run_inner(
    source: &str,
    timeout: Duration,
    logs: Rc<RefCell<Vec<String>>>,
) -> Result<Value, Value> {
    let rt = Runtime::new()
        .map_err(|e| json!({ "error": format!("could not create the JS runtime: {e}") }))?;
    rt.set_memory_limit(MEMORY_LIMIT_BYTES);
    let deadline = Instant::now() + timeout;
    // The interrupt handler runs periodically inside the interpreter loop;
    // returning true aborts the script. This is the only reliable way to stop
    // a `while(true){}` submitted by a confused agent.
    rt.set_interrupt_handler(Some(Box::new(move || Instant::now() >= deadline)));

    let ctx = Context::full(&rt)
        .map_err(|e| json!({ "error": format!("could not create the JS context: {e}") }))?;

    ctx.with(|ctx| {
        install_bridge(&ctx, logs).map_err(|e| json!({ "error": format!("bridge setup: {e}") }))?;
        ctx.eval::<(), _>(PRELUDE)
            .map_err(|e| json!({ "error": format!("prelude failed: {e}") }))?;

        // Wrap the user source as a function body so `return` is legal, then
        // JSON.stringify the completion value INSIDE the sandbox: the result
        // crosses the bridge as one string, so no JS-to-Rust object
        // marshalling can quietly drop a field. `undefined` maps to null
        // (JSON has no undefined).
        let wrapped = format!(
            "(() => {{ const __v = (function() {{\n{source}\n}})();\n\
             return JSON.stringify(__v === undefined ? null : __v); }})()"
        );
        match ctx.eval::<rquickjs::String, _>(wrapped.as_str()) {
            Ok(s) => {
                let text = s
                    .to_string()
                    .map_err(|e| json!({ "error": format!("result was not a string: {e}") }))?;
                serde_json::from_str(&text).map_err(|e| {
                    json!({ "error": format!("script returned a non-JSON-serializable value: {e}") })
                })
            }
            Err(_) => Err(thrown_failure(&ctx)),
        }
    })
}

/// Install the two native bridge functions. Split out so the setup errors can
/// be reported distinctly from script errors.
fn install_bridge(ctx: &Ctx<'_>, logs: Rc<RefCell<Vec<String>>>) -> rquickjs::Result<()> {
    let globals = ctx.globals();
    globals.set(
        "__hauksbee_log",
        Function::new(ctx.clone(), move |line: String| {
            logs.borrow_mut().push(line);
        })?,
    )?;
    globals.set(
        "__hauksbee_call",
        Function::new(ctx.clone(), |name: String, args_json: String| -> String {
            let args: Value = serde_json::from_str(&args_json).unwrap_or(Value::Null);
            let outcome = crate::tools::call_from_script(&name, &args);
            // The envelope the prelude unwraps: ok=false throws {error}, a
            // refusal result throws itself, anything else returns as data.
            let envelope = if outcome.is_error {
                json!({ "ok": false, "error": outcome.value.get("error")
                    .and_then(Value::as_str).unwrap_or("tool call failed") })
            } else {
                json!({ "ok": true, "result": outcome.value })
            };
            envelope.to_string()
        })?,
    )?;
    Ok(())
}

/// Convert the pending exception into the structured failure object. A
/// JSON-serializable thrown value (the refusal contract) lands under `thrown`
/// verbatim; an Error object or unstringifiable value degrades to its message.
fn thrown_failure(ctx: &Ctx<'_>) -> Value {
    let caught: JsValue = ctx.catch();
    if let Some(text) = js_json_stringify(ctx, &caught) {
        if let Ok(v) = serde_json::from_str::<Value>(&text) {
            return json!({ "error": "script threw", "thrown": v });
        }
    }
    // Timeouts surface as an "interrupted" exception with no useful payload;
    // name them for what they are.
    let msg = exception_message(&caught);
    if msg.contains("interrupted") {
        json!({ "error": "script aborted: timeout or memory limit exceeded" })
    } else {
        json!({ "error": format!("script threw: {msg}") })
    }
}

/// Best-effort JSON.stringify of a JS value, done inside the sandbox so JS
/// semantics (toJSON, undefined-dropping) apply. None when it is not
/// stringifiable (cycles, or an Error whose own properties are non-enumerable).
fn js_json_stringify<'js>(ctx: &Ctx<'js>, v: &JsValue<'js>) -> Option<String> {
    let stringified = ctx.json_stringify(v.clone()).ok()??;
    stringified.to_string().ok()
}

/// A human-usable message for a thrown value that did not stringify: an Error
/// object's name/message pair, or the value's type as a last resort.
fn exception_message(v: &JsValue) -> String {
    if let Some(ex) = v.as_exception() {
        let name = ex
            .get::<_, String>("name")
            .unwrap_or_else(|_| "Error".to_string());
        let message = ex.message().unwrap_or_default();
        return format!("{name}: {message}");
    }
    format!("a non-Error value of type {:?}", v.type_of())
}
