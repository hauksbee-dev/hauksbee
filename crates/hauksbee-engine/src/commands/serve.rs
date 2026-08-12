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
pub(crate) fn open_browser(url: &str) {
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

/// Sweep stale hauksbee temp files from TMPDIR at server start.
///
/// The live-sim firmware file (`hauksbee-live-fw-*`) and the merged ESP flash
/// image (`hauksbee-esp-flash-*`) are deleted on Drop, but a process killed
/// mid-session (the app's quit SIGTERM being the normal case) never runs Drop,
/// so the file outlives it. The age threshold keeps this from racing another
/// hauksbee instance whose session is live right now.
fn sweep_stale_temp_files() {
    const STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with("hauksbee-live-fw-") && !name.starts_with("hauksbee-esp-flash-") {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > STALE_AFTER);
        if stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Exit when the parent process named in `HAUKSBEE_EXIT_WITH_PARENT` is gone.
///
/// The app launcher sets the variable to its own pid when it spawns this
/// server. Its normal Quit path SIGTERMs us, but a launcher killed by a raw
/// signal never runs its delegate, and the server would keep serving as an
/// orphan (observed in the cold-install audit). Opt-in via the env var only:
/// a terminal user's `nohup hauksbee serve &` legitimately outlives its shell
/// and must never be tied to a parent this way.
fn exit_when_parent_dies() {
    #[cfg(not(unix))]
    return;
    #[cfg(unix)]
    let Some(parent) = std::env::var("HAUKSBEE_EXIT_WITH_PARENT")
        .ok()
        .and_then(|v| v.parse::<i32>().ok())
    else {
        return;
    };
    #[cfg(unix)]
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_secs(2));
        // kill(pid, 0) probes existence without sending a signal; ESRCH means
        // the launcher is gone. EPERM would mean "exists but not ours", which
        // cannot happen for our own parent, and is treated as alive anyway.
        let gone = unsafe { libc::kill(parent as libc::pid_t, 0) } != 0
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
        if gone {
            // Normal shutdown path: the process exit drops nothing gracefully
            // here by design; emulator children die with the process group
            // (hauksbee_mcu::children), and the stale-temp sweep above covers
            // the files whose Drop this skips.
            std::process::exit(0);
        }
    });
}

/// `hauksbee serve [--port N] [--open|--no-open]`: the local web front door
/// (drop-a-board report).
pub fn run(port: u16, open: bool, no_open: bool) -> anyhow::Result<()> {
    use std::sync::Arc;
    sweep_stale_temp_files();
    exit_when_parent_dies();
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let addr = format!("127.0.0.1:{}", port);

        // Inject the engine's analysis as the server's analyzer callback, so the
        // server crate needs no dependency on the engine/extract crates. The
        // firmware-aware callback handles both the board-only path (firmware ==
        // None -> analyze_json) and the firmware co-sim path.
        let analyze = crate::commands::common::schematic_analyzer();
        // The web checks panel's backend: stage the uploads, inject the path
        // keys, and run the sibling hauksbee-ci binary (--json).
        let check: hauksbee_server::frontdoor::SchematicCheckRunner =
            Arc::new(|name, contents, fw, schematic, spec| {
                crate::webcheck::run_web_check_with_schematic(
                    name, contents, fw, schematic, spec,
                )
            });
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
        // launch by the desktop .app launcher (it sets HAUKSBEE_EXIT_WITH_PARENT),
        // where nobody can read the printed URL so opening the browser IS the
        // user interface. A bare non-TTY stdout is NOT a trigger any more: that
        // is every CI job and piped script, and popping a browser there was a
        // misfire. --no-open vetoes both. Only when the UI exists, though:
        // opening a browser onto the "build the frontend first" API-only
        // fallback would be worse than nothing. The listener is already bound
        // at this point (fallback port included), so the URL is live.
        {
            let launched_by_app = std::env::var_os("HAUKSBEE_EXIT_WITH_PARENT").is_some();
            if !no_open && (open || launched_by_app) && dir.is_some() {
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
            // Was "Nothing leaves your machine", which stopped being true when
            // this same server grew /api/models/extract. That path has its own
            // consent notice and never runs unasked, but a blanket promise in
            // the first thing a user reads is the wrong place to be approximate.
            println!(
                "  short co-sim. Your board never leaves this machine. The one thing that"
            );
            println!(
                "  can is a datasheet you explicitly send for model extraction, which asks"
            );
            println!("  first. Ctrl-C to stop.\n");
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
        hauksbee_server::serve_frontdoor_on_with_schematic(
            listener,
            dir.as_deref(),
            analyze,
            Some(check),
            Some(deps),
            Some(crate::commands::common::schematic_live_launcher()),
            startup_json,
        )
        .await
    })
}
