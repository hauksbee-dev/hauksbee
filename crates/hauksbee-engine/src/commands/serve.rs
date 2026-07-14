//! `hauksbee serve [--port N]`: the local web front door.
//!
//! W6 §1 (one web experience): this serves the SAME React bundle as
//! `hauksbee run --serve`, with no board preloaded. The app lands on the
//! drop-zone: drop a board (and optionally firmware) to get the plain-language
//! report from the analysis API, then press "run it" to bring a board to life.

use std::path::PathBuf;

/// `hauksbee serve [--port N]`: the local web front door (drop-a-board report).
pub fn run(port: u16) -> anyhow::Result<()> {
    use std::sync::Arc;
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let addr = format!("127.0.0.1:{}", port);

        // Inject the engine's analysis as the server's analyzer callback, so the
        // server crate needs no dependency on the engine/extract crates. The
        // firmware-aware callback handles both the board-only path (firmware ==
        // None -> analyze_json) and the firmware co-sim path.
        let analyze: hauksbee_server::frontdoor::FirmwareAnalyzer = Arc::new(
            |name: &str, contents: &str, fw: Option<(&str, &[u8])>| match fw {
                Some((fw_name, fw_bytes)) => {
                    crate::analyze_with_firmware_json(name, contents, fw_name, fw_bytes)
                }
                None => crate::analyze_json(name, contents),
            },
        );

        // The React bundle is the one web app. It is a build artifact (not
        // checked in), so a fresh clone has no dist/ yet: tell the user how to
        // build it rather than serving a blank 404.
        let static_dir =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../frontend/dist");
        let dir = static_dir.exists().then(|| static_dir.clone());

        // Bind FIRST, then print. The requested port may be busy, in which case
        // the bind falls back to another port; printing `addr` before binding
        // advertised a URL the server was not actually listening on.
        let (listener, bound) = hauksbee_server::bind_frontdoor(&addr).await?;
        if dir.is_some() {
            // dist/ is gitignored: a `git pull` updates frontend/src but not the
            // built bundle, so warn when what we serve predates the sources.
            crate::commands::common::warn_if_dist_stale(&static_dir);
            println!("\n  hauksbee is live. Open this in your browser:\n");
            println!("      http://{bound}\n");
            println!("  Drop a board file (.kicad_pcb / .kicad_sch / .brd / gerber zip) to get a");
            println!("  plain-language report. Optionally drop firmware (.elf / .hex) to run a");
            println!("  short co-sim. Nothing leaves your machine. Ctrl-C to stop.\n");
        } else {
            println!("\n  hauksbee serve needs the web app built once:\n");
            println!("      cd frontend && bun install && bun run build\n");
            println!("  then re-run `hauksbee serve`. The analysis API is live at");
            println!("  http://{bound}/api/analyze meanwhile. Ctrl-C to stop.\n");
        }

        // No board preloaded: the app lands on the drop zone.
        let startup_json = "{\"preloaded\":false}".to_string();
        hauksbee_server::serve_frontdoor_on(listener, dir.as_deref(), analyze, startup_json).await
    })
}
