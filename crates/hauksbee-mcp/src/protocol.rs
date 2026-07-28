//! The MCP protocol layer: JSON-RPC 2.0 over newline-delimited stdio, per the
//! Model Context Protocol stdio transport. Hand-rolled on serde_json rather
//! than an SDK because the subset a tools-only server needs (initialize /
//! initialized / ping / tools/list / tools/call) is a few hundred lines, and a
//! hand-rolled implementation keeps the dependency tree clean and auditable.
//! Every tool result carries BOTH a `content` text block (the serialized JSON,
//! for clients that only render text) and `structuredContent` (the object
//! itself, for clients that parse), so no client tier loses information.

use serde_json::{json, Value};

/// The protocol revision this server implements. Offered back verbatim when
/// the client requests a revision we know; otherwise this is the counter-offer
/// (the MCP version-negotiation rule: the server answers with the version it
/// is willing to speak, and the client disconnects if it cannot).
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// Revisions we can speak. The tools-only subset is identical across these,
/// so accepting a client's older revision is honest, not optimistic.
const SUPPORTED_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

/// One MCP session over stdio. The only state is whether `initialize` has
/// happened, used to give a precise error to a client that skips the
/// handshake rather than a confusing tool failure.
pub struct Server {
    initialized: bool,
}

impl Default for Server {
    fn default() -> Self {
        Self::new()
    }
}

impl Server {
    pub fn new() -> Self {
        Server { initialized: false }
    }

    /// Handle one line from stdin. Returns the response line to write, or
    /// None for notifications (which never get a response, per JSON-RPC).
    pub fn handle_line(&mut self, line: &str) -> Option<String> {
        let msg: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                return Some(
                    error_response(Value::Null, -32700, &format!("parse error: {e}")).to_string(),
                )
            }
        };
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let id = msg.get("id").cloned();
        let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));

        // No id = notification: act, but never respond.
        let id = match id {
            Some(id) if !id.is_null() => id,
            _ => {
                if method == "notifications/initialized" {
                    self.initialized = true;
                }
                return None;
            }
        };

        let response = match method {
            "initialize" => self.initialize(id, &params),
            "ping" => ok_response(id, json!({})),
            "tools/list" => ok_response(id, json!({ "tools": crate::tools::definitions() })),
            "tools/call" => self.tools_call(id, &params),
            other => error_response(id, -32601, &format!("method not found: {other}")),
        };
        Some(response.to_string())
    }

    /// The MCP handshake. Capabilities: tools only (no resources, prompts,
    /// sampling, or subscriptions), stated as exactly that so a client never
    /// probes for surfaces that do not exist.
    fn initialize(&mut self, id: Value, params: &Value) -> Value {
        let requested = params
            .get("protocolVersion")
            .and_then(Value::as_str)
            .unwrap_or(PROTOCOL_VERSION);
        let version = if SUPPORTED_VERSIONS.contains(&requested) {
            requested
        } else {
            PROTOCOL_VERSION
        };
        ok_response(
            id,
            json!({
                "protocolVersion": version,
                "capabilities": { "tools": {} },
                "serverInfo": {
                    "name": "hauksbee-mcp",
                    "title": "hauksbee: the verifier for AI-designed hardware",
                    "version": env!("CARGO_PKG_VERSION"),
                },
                "instructions": "hauksbee analyzes PCB design files with real device physics and co-simulates firmware on emulated MCUs. Honesty contract: a result with status \"invalid_for_analysis\" is a structured refusal, the run declined to vouch for itself; never read it as pass or fail, and never retry expecting a different answer. Coverage holes (substituted cores, dropped ADC channels, unexercised buses) arrive as data fields; surface them alongside any verdict.",
            }),
        )
    }

    /// `tools/call`: dispatch to the tool table. Tool-level failures are
    /// reported inside the result with `isError: true` (per MCP), reserving
    /// JSON-RPC errors for protocol misuse like calling before initialize.
    fn tools_call(&mut self, id: Value, params: &Value) -> Value {
        if !self.initialized {
            return error_response(
                id,
                -32002,
                "server not initialized: send initialize, then the notifications/initialized \
                 notification, before calling tools",
            );
        }
        let name = match params.get("name").and_then(Value::as_str) {
            Some(n) => n,
            None => return error_response(id, -32602, "tools/call requires a 'name' parameter"),
        };
        let args = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let outcome = crate::tools::call(name, &args);
        let text = serde_json::to_string(&outcome.value)
            .unwrap_or_else(|e| format!("{{\"error\":\"could not serialize result: {e}\"}}"));
        ok_response(
            id,
            json!({
                "content": [ { "type": "text", "text": text } ],
                "structuredContent": outcome.value,
                "isError": outcome.is_error,
            }),
        )
    }
}

/// A JSON-RPC 2.0 success envelope.
fn ok_response(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

/// A JSON-RPC 2.0 error envelope.
fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_negotiates_known_version_and_counter_offers_unknown() {
        let mut s = Server::new();
        let resp = s
            .handle_line(
                r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26"}}"#,
            )
            .unwrap();
        let v: Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(v["result"]["protocolVersion"], "2025-03-26");

        let resp = s
            .handle_line(
                r#"{"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":"1990-01-01"}}"#,
            )
            .unwrap();
        let v: Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(v["result"]["protocolVersion"], PROTOCOL_VERSION);
    }

    #[test]
    fn notifications_get_no_response_and_arm_initialized() {
        let mut s = Server::new();
        assert!(s
            .handle_line(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
            .is_none());
        assert!(s.initialized);
    }

    #[test]
    fn tools_call_before_initialized_is_a_protocol_error() {
        let mut s = Server::new();
        let resp = s
            .handle_line(
                r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"list_capabilities"}}"#,
            )
            .unwrap();
        let v: Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(v["error"]["code"], -32002);
    }

    #[test]
    fn unknown_method_is_minus_32601_and_parse_error_minus_32700() {
        let mut s = Server::new();
        let v: Value = serde_json::from_str(
            &s.handle_line(r#"{"jsonrpc":"2.0","id":1,"method":"nope"}"#)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(v["error"]["code"], -32601);
        let v: Value = serde_json::from_str(&s.handle_line("not json").unwrap()).unwrap();
        assert_eq!(v["error"]["code"], -32700);
    }
}
