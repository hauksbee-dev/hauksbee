//! The `hauksbee-mcp` binary: read newline-delimited JSON-RPC 2.0 from stdin,
//! write responses to stdout, log nothing else to stdout (the MCP stdio
//! transport owns that stream; human-facing notes go to stderr). The loop is
//! deliberately thin: all protocol handling lives in [`hauksbee_mcp::protocol`]
//! so the tests can drive the exact same dispatch through a spawned process.

use std::io::{BufRead, Write};

fn main() {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut server = hauksbee_mcp::protocol::Server::new();
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            // A read error means the client side is gone; exit quietly.
            Err(_) => break,
        };
        if line.trim().is_empty() {
            continue;
        }
        if let Some(response) = server.handle_line(&line) {
            let mut out = stdout.lock();
            // A broken pipe here also means the client is gone.
            if writeln!(out, "{response}")
                .and_then(|_| out.flush())
                .is_err()
            {
                break;
            }
        }
    }
}
