//! `hauksbee serve [--port N]`: the local web front door.
//!
//! One web experience: this serves the SAME React bundle as
//! `hauksbee run --serve`, with no board preloaded. The app lands on the
//! drop-zone: drop a board (and optionally firmware) to get the plain-language
//! report from the analysis API, then press "run it" to bring a board to life.

/// Open the system browser at `url`, detached, best-effort.
///
/// Used for `--open` and for the no-TTY launch path (double-click / launchd /
/// Finder), where nobody can read a printed URL. Failure is silent by design: a
/// headless box has no browser, and the server must keep serving regardless.
fn open_browser(url: &str) {
    use std::process::{Command, Stdio};
    #[cfg(target_os = "macos")]
    let mut cmd = Command::new("open");
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut cmd = Command::new("xdg-open");
    // `start` is a cmd.exe builtin, not an executable; the empty string is
    // start's window-title slot, which would otherwise swallow the URL.
    #[cfg(windows)]
    let mut cmd = {
        let mut c = Command::new("cmd");
        c.args(["/C", "start", ""]);
        c
    };
    #[cfg(not(any(unix, windows)))]
    return;
    #[cfg(any(unix, windows))]
    {
        let _ = cmd
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
}

/// `hauksbee serve [--port N] [--open]`: the local web front door
/// (drop-a-board report).
pub fn run(port: u16, open: bool) -> anyhow::Result<()> {
    use std::sync::Arc;
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let addr = format!("127.0.0.1:{}", port);

        // Inject the engine's analysis as the server's analyzer callback, so the
        // server crate needs no dependency on the engine/extract crates. The
        // firmware-aware callback handles both the board-only path (firmware ==
        // None -> analyze_json) and the firmware co-sim path.
        let analyze: hauksbee_server::frontdoor::FirmwareAnalyzer = Arc::new(
            |name: &str, contents: &[u8], fw: Option<(&str, &[u8])>| match fw {
                Some((fw_name, fw_bytes)) => {
                    crate::analyze_with_firmware_json(name, contents, fw_name, fw_bytes)
                }
                None => crate::analyze_json(name, contents),
            },
        );
        // The web checks panel's backend: stage the uploads, inject the path
        // keys, and run the sibling hauksbee-ci binary (--json).
        let check: hauksbee_server::frontdoor::CheckRunner =
            Arc::new(|name, contents, fw, spec| crate::webcheck::run_web_check(name, contents, fw, spec));
        // The dependency panel's backend: status from the engine's own
        // discovery, installs through the engine's streaming installer.
        let deps = crate::commands::common::deps_hooks();

        // The React bundle is the one web app. Resolve it via the ladder
        // (HAUKSBEE_WEB_DIST override -> checkout dist -> embedded copy), so an
        // installed release binary serves the UI too, not just a source
        // checkout. If nothing resolves, tell the user how to build it rather
        // than serving a blank 404.
        let dir = crate::web_dist::resolve_web_dist();

        // Bind FIRST, then print. The requested port may be busy, in which case
        // the bind falls back to another port; printing `addr` before binding
        // advertised a URL the server was not actually listening on.
        let (listener, bound) = hauksbee_server::bind_frontdoor(&addr).await?;

        // Browser auto-open. Two triggers: the explicit --open flag, and a
        // non-TTY stdout, which means we were launched by something that is not
        // a terminal (double-click, launchd, Finder, the .app launcher). In
        // that situation nobody can read the printed URL, so opening the
        // browser IS the user interface. Only when the UI exists, though:
        // opening a browser onto the "build the frontend first" API-only
        // fallback would be worse than nothing. The listener is already bound
        // at this point (fallback port included), so the URL is live.
        {
            use std::io::IsTerminal;
            let launched_headless = !std::io::stdout().is_terminal();
            if (open || launched_headless) && dir.is_some() {
                open_browser(&format!("http://{bound}"));
            }
        }

        if let Some(static_dir) = dir.as_ref() {
            // dist/ is gitignored: a `git pull` updates frontend/src but not the
            // built bundle, so warn when what we serve predates the sources.
            // No-ops for the embedded cache (no sibling src to compare against).
            crate::commands::common::warn_if_dist_stale(static_dir);
            println!("\n  hauksbee is live. Open this in your browser:\n");
            println!("      http://{bound}\n");
            println!("  Drop a board file (.kicad_pcb / .kicad_sch / .brd / .board / gerber zip) to get a");
            println!("  plain-language report. Optionally drop firmware (.elf / .hex) to run a");
            println!("  short co-sim. Nothing leaves your machine. Ctrl-C to stop.\n");
        } else {
            println!("\n  hauksbee serve needs the web app built once:\n");
            println!("      cd frontend && bun install && bun run build\n");
            println!("  then re-run `hauksbee serve`. The analysis API is live at");
            println!("  http://{bound}/api/analyze meanwhile. Ctrl-C to stop.\n");
        }

        // No board preloaded: the app lands on the drop zone. `live: true`
        // tells it this server can launch a live sim for an uploaded board
        // (`/api/live/launch`), so the report offers the launch button instead
        // of the CLI-hint fallback.
        let startup_json = serde_json::json!({
            "preloaded": false,
            "live": true,
            // Engine version, for the Environment page's "what am I running"
            // card.
            "version": env!("CARGO_PKG_VERSION"),
        })
        .to_string();
        hauksbee_server::serve_frontdoor_on(
            listener,
            dir.as_deref(),
            analyze,
            Some(check),
            Some(deps),
            Some(crate::commands::common::live_launcher()),
            startup_json,
        )
        .await
    })
}
