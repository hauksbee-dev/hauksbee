//! `galvani` CLI: bind a board and bring it to life.
//!
//! ```text
//! galvani run <board-file> [--firmware <hex>] [--seconds N] [--headless]
//!                          [--port 3001] [--report]
//! ```
//!
//! - `--report`   : print the bind report table and exit.
//! - `--headless` : run the co-sim for `--seconds` and print summary stats.
//! - default      : serve the live websocket (frontend/dist if present).

use std::path::{Path, PathBuf};

use galvani_engine::binder::bind_board;
use galvani_engine::GalvaniEngine;
use galvani_extract::ExtractedBoard;
use galvani_models::ModelLibrary;
use galvani_server::Server;

struct Args {
    board: PathBuf,
    firmware: Option<PathBuf>,
    seconds: f64,
    headless: bool,
    report_only: bool,
    port: u16,
}

fn parse_args() -> Result<Args, String> {
    let mut it = std::env::args().skip(1);
    let cmd = it.next().ok_or_else(usage)?;
    if cmd != "run" {
        return Err(format!("unknown command '{cmd}'\n{}", usage()));
    }
    let board = it.next().map(PathBuf::from).ok_or_else(usage)?;
    let mut args = Args {
        board,
        firmware: None,
        seconds: 1.0,
        headless: false,
        report_only: false,
        port: 3001,
    };
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--firmware" => {
                args.firmware = Some(PathBuf::from(it.next().ok_or("--firmware needs a path")?))
            }
            "--seconds" => {
                args.seconds = it
                    .next()
                    .ok_or("--seconds needs a number")?
                    .parse()
                    .map_err(|_| "bad --seconds")?
            }
            "--headless" => args.headless = true,
            "--report" => args.report_only = true,
            "--port" => {
                args.port = it
                    .next()
                    .ok_or("--port needs a number")?
                    .parse()
                    .map_err(|_| "bad --port")?
            }
            other => return Err(format!("unknown flag '{other}'\n{}", usage())),
        }
    }
    Ok(args)
}

fn usage() -> String {
    "usage: galvani run <board-file> [--firmware <hex>] [--seconds N] \
     [--headless] [--port 3001] [--report]"
        .to_string()
}

fn main() -> anyhow::Result<()> {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };

    let text = std::fs::read_to_string(&args.board)
        .map_err(|e| anyhow::anyhow!("reading {}: {e}", args.board.display()))?;
    // A `.kicad_sch` may reference sub-sheets that live in sibling files, so
    // it must be loaded by path to recurse the hierarchy; everything else is
    // self-contained and sniffed from its content.
    let board = if args.board.extension().and_then(|e| e.to_str()) == Some("kicad_sch") {
        ExtractedBoard::from_kicad_schematic_path(&args.board)?
    } else {
        ExtractedBoard::from_auto(&text)?
    };
    let lib = ModelLibrary::builtin();

    // --report: bind, print the table, exit.
    if args.report_only {
        let bound = bind_board(&board, &lib);
        print!("{}", bound.report.render_table());
        return Ok(());
    }

    let mut engine = GalvaniEngine::from_board_file(
        &text,
        args.firmware.as_deref(),
        &format!("/boards/{}", file_name(&args.board)),
    )?;

    if args.headless {
        run_headless(&mut engine, args.seconds);
        return Ok(());
    }

    // Default: serve the websocket.
    serve(engine, args.port)
}

fn run_headless(engine: &mut GalvaniEngine, seconds: f64) {
    use galvani_server::engine::Engine;
    eprintln!("co-sim: {seconds:.2}s headless...");
    let frame_dt = 1.0 / 1000.0; // 1 kHz frame cadence
    let mut t = 0.0;
    let mut last_uart: Vec<u8> = Vec::new();
    while t < seconds {
        let frame = engine.step(frame_dt);
        for bytes in frame.uart.values() {
            last_uart.extend_from_slice(bytes);
        }
        t += frame_dt;
    }

    let sched = engine.scheduler();
    println!("\nsimulated {:.3}s over {} nets", sched.sim_time, sched.stats.len());
    // Sort nets by activity (toggle count then range).
    let mut rows: Vec<_> = sched.stats.iter().collect();
    rows.sort_by(|a, b| {
        b.1.toggles
            .cmp(&a.1.toggles)
            .then((b.1.max_v - b.1.min_v).partial_cmp(&(a.1.max_v - a.1.min_v)).unwrap())
    });
    println!("\nmost active nets:");
    println!(
        "┌────────────────────────────┬──────────┬──────────┬──────────┐\n\
         │ Net                        │ min (V)  │ max (V)  │ toggles  │\n\
         ├────────────────────────────┼──────────┼──────────┼──────────┤"
    );
    for (name, st) in rows.iter().take(15) {
        let min_v = if st.min_v.is_finite() { st.min_v } else { 0.0 };
        let max_v = if st.max_v.is_finite() { st.max_v } else { 0.0 };
        println!(
            "│ {:<26} │ {:>8.3} │ {:>8.3} │ {:>8} │",
            truncate(name, 26),
            min_v,
            max_v,
            st.toggles
        );
    }
    println!("└────────────────────────────┴──────────┴──────────┴──────────┘");

    if !last_uart.is_empty() {
        let s = String::from_utf8_lossy(&last_uart);
        println!("\nUART output ({} bytes):\n{}", last_uart.len(), s.trim_end());
    }
}

fn serve(engine: GalvaniEngine, port: u16) -> anyhow::Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let server = Server::new(Box::new(engine));
        let static_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../frontend/dist");
        let dir = static_dir.exists().then_some(static_dir);
        let addr = format!("127.0.0.1:{port}");
        server.serve(&addr, dir.as_deref()).await
    })
}

fn file_name(p: &Path) -> String {
    p.file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("board")
        .to_string()
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}
