//! Small helpers shared by more than one command handler: board-text loading,
//! the Board-as-Code header sniff, the served-file name, and the websocket server
//! launcher (`hauksbee serve` and `run --serve` both reach it).

use std::path::{Path, PathBuf};

use hauksbee_server::Server;

use crate::engine::HauksbeeEngine;

pub fn read_board_text(path: &Path) -> anyhow::Result<String> {
    std::fs::read_to_string(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            anyhow::anyhow!(
                "no board file at '{}'. Check the path, or try a bundled example:\n  \
                 hauksbee run crates/hauksbee-ci/examples/boards/blinky.kicad_pcb --report",
                path.display()
            )
        } else {
            anyhow::anyhow!("reading '{}': {e}", path.display())
        }
    })
}

/// True when the text is a Board-as-Code (`.board`) DSL source, recognised by
/// the header `program_from_extracted`/`to_code` emit. Lets a `.board` saved
/// without that extension still route through the recompile path.
pub fn is_board_code_header(text: &str) -> bool {
    let head: String = text.chars().take(256).collect();
    head.contains("Board-as-Code") || head.contains("board version ")
}

pub fn file_name(p: &Path) -> String {
    p.file_name().and_then(|s| s.to_str()).unwrap_or("board").to_string()
}

pub fn serve(
    engine: HauksbeeEngine,
    port: u16,
    board_file: Option<(String, String)>,
) -> anyhow::Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let server = Server::new(Box::new(engine));
        let static_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../frontend/dist");
        let dir = static_dir.exists().then(|| static_dir.clone());
        let addr = format!("127.0.0.1:{port}");

        if dir.is_some() {
            println!("\n  hauksbee is live. Open this in your browser:\n");
            println!("      http://{addr}\n");
            println!("  (2D/3D board view; Ctrl-C to stop.)\n");
        } else {
            // The frontend is a build artifact and is not checked in, so a fresh
            // clone has no dist/ yet. Serve the websocket regardless (so the API
            // and any external viewer still work) but tell the user exactly how
            // to get the live view rather than leaving them on a blank 404 page.
            println!("\n  hauksbee websocket server is live at ws://{addr}/ws  (Ctrl-C to stop)\n");
            println!("  The live 2D/3D viewer at http://{addr} needs the frontend built once:\n");
            println!("      cd frontend && bun install && bun run build\n");
            println!("  then re-run this command. For a quick non-visual check, try:\n");
            println!("      hauksbee run <board> --report      # bind table");
            println!("      hauksbee run <board> --drc          # copper shorts");
            println!("      hauksbee run <board> --headless     # co-sim summary\n");
        }
        server.serve_with_board(&addr, dir.as_deref(), board_file).await
    })
}
