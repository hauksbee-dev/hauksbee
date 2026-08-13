//! The `hauksbee-mcp` binary: read newline-delimited JSON-RPC 2.0 from stdin,
//! write responses to stdout, log nothing else to stdout (the MCP stdio
//! transport owns that stream; human-facing notes go to stderr). The loop is
//! deliberately thin: all protocol handling lives in [`hauksbee_mcp::protocol`]
//! so the tests can drive the exact same dispatch through a spawned process.

use std::io::{BufRead, Write};

const HELP: &str = concat!(
    "hauksbee-mcp ",
    env!("CARGO_PKG_VERSION"),
    "\n",
    env!("CARGO_PKG_DESCRIPTION"),
    "\n\nUSAGE:\n",
    "    hauksbee-mcp\n\n",
    "Runs as an MCP stdio server: it reads newline-delimited JSON-RPC 2.0 \
     requests\non stdin and writes responses on stdout, so it is launched by \
     an MCP host\n(Claude Code, Cursor, ...) rather than used interactively.\n\n\
     OPTIONS:\n\
    \x20   -h, --help       Print this help and exit\n\
    \x20   -V, --version    Print the version and exit\n"
);

fn version_string() -> String {
    match option_env!("GIT_HASH") {
        Some(hash) => format!("{} (git {hash})", env!("CARGO_PKG_VERSION")),
        None => env!("CARGO_PKG_VERSION").to_string(),
    }
}

fn main() {
    // A flag answer must be a real answer: exiting 0 with nothing on stdout
    // (the old behaviour) told a packaging smoke test precisely nothing.
    if let Some(arg) = std::env::args().nth(1) {
        match arg.as_str() {
            "--version" | "-V" => {
                println!("hauksbee-mcp {}", version_string());
                return;
            }
            "--help" | "-h" => {
                println!("{HELP}");
                return;
            }
            other => {
                eprintln!("hauksbee-mcp: unknown argument '{other}' (try --help)");
                std::process::exit(2);
            }
        }
    }

    // An MCP host that kills this server mid-tool-call must not orphan any
    // co-sim emulator the tool spawned: reap every live Renode/QEMU child on
    // SIGTERM/SIGINT. See hauksbee_mcu::children.
    hauksbee_mcu::children::install_signal_reaper();
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
