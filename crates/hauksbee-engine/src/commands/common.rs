//! Small helpers shared by more than one command handler: board-text loading,
//! the Board-as-Code header sniff, the served-file name, and the websocket server
//! launcher (`hauksbee serve` and `run --serve` both reach it).

use std::path::{Path, PathBuf};

use hauksbee_server::Server;

use crate::engine::HauksbeeEngine;

pub fn read_board_text(path: &Path) -> anyhow::Result<String> {
    // A directory here would surface as the raw OS errno ("Is a directory");
    // say what is wrong and note the asymmetry with from-code, which accepts
    // a directory holding one .board file.
    if path.is_dir() {
        anyhow::bail!(
            "'{}' is a directory; to-code reads a single board file, so pass the \
             .kicad_pcb inside it (from-code does accept a directory containing one \
             .board file; to-code does not).",
            path.display()
        );
    }
    std::fs::read_to_string(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            // The suggestion matches the INVOKING surface (this reader serves
            // to-code): a `hauksbee run` command here answered the wrong
            // question. Never suggest an unrunnable command: the checkout path
            // only exists inside a hauksbee source tree.
            let checkout = Path::new("crates/hauksbee-ci/examples/boards/blinky.kicad_pcb");
            let suggestion = if checkout.exists() {
                "try:  hauksbee to-code crates/hauksbee-ci/examples/boards/blinky.kicad_pcb"
                    .to_string()
            } else {
                "to-code decompiles an existing board file; any .kicad_pcb / .brd works".to_string()
            };
            anyhow::anyhow!(
                "no board file at '{}'. Check the path ({suggestion}).",
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

/// Width-cap every line of an error message that quotes file content (a TOML
/// parser's caret-annotated snippet). A machine-written input can be one
/// enormous line, and quoting it whole buries the actual error; anything past
/// the cap is elided with a marker. Lines within the cap pass through intact.
pub fn cap_context_width(msg: &str) -> String {
    const MAX: usize = 200;
    msg.lines()
        .map(|line| {
            if line.chars().count() <= MAX {
                line.to_string()
            } else {
                let head: String = line.chars().take(MAX).collect();
                format!("{head} ...(line truncated)")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn file_name(p: &Path) -> String {
    p.file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("board")
        .to_string()
}

/// The browser tool panels' backend hooks, shared by `hauksbee serve` and
/// `run --serve`: dependency status is the engine's own discovery
/// (`deps::deps_json`), installs go through the engine's streaming installer
/// (`deps::install_dep`, which enforces its own one-at-a-time slot, timeout, and
/// output cap), and datasheet extraction goes through `webextract` (which holds
/// the same consent contract `hauksbee models extract` holds).
pub fn deps_hooks() -> hauksbee_server::frontdoor::ToolHooks {
    use std::sync::Arc;
    hauksbee_server::frontdoor::ToolHooks {
        deps_status: Arc::new(crate::deps::deps_json),
        install: Arc::new(|id, progress| crate::deps::install_dep(id, progress)),
        datasheet: crate::webextract::hooks(),
    }
}

/// The web live-launch callback: turn an uploaded board (and optional
/// firmware) into a running [`HauksbeeEngine`] the server hub can install
/// behind `/ws`. Shared by `hauksbee serve` and `run --serve` so an uploaded
/// board can go live from either front door.
///
/// Refusals mirror the CLI: firmware on a board that bound zero processors is
/// unanswerable (the CLI exits 3 with `no_processor_message`), so the same
/// message comes back as the launch error instead of a silent board-only
/// downgrade. Engine-build failures (unloadable firmware, missing emulator)
/// surface verbatim too; the frontend keeps the report up and shows them.
pub fn live_launcher() -> hauksbee_server::frontdoor::LiveLauncher {
    use hauksbee_server::frontdoor::LiveLaunch;
    use std::io::Write as _;
    use std::sync::Arc;
    Arc::new(
        |name: &str, contents: &[u8], fw: Option<(&str, &[u8])>| -> Result<LiveLaunch, String> {
            let norm =
                crate::board_input::from_bytes(name, contents).map_err(|e| e.web_message())?;
            // Geometric DRC on the same input the web co-sim runs it on (the
            // bytes twin for a binary board, the layout text otherwise; a
            // gerber archive has neither). Computed BEFORE `norm.board` is
            // moved out, and applied to the live engine below: without this
            // the live sim silently simulated the un-shorted board while the
            // report's co-sim block (for the same board) ran with the shorts
            // bridged and said so.
            use hauksbee_extract::ExtractedBoard;
            let drc = if norm.is_binary() {
                ExtractedBoard::altium_drc(&norm.raw).unwrap_or_default()
            } else if norm.is_gerber() {
                Default::default()
            } else {
                ExtractedBoard::drc(norm.layout_text.as_deref().unwrap_or_default())
                    .unwrap_or_default()
            };
            let mut board = norm.board;
            // Same DNP default as bare `hauksbee run` (no --fit/--no-fit here).
            board
                .apply_dnp_policy(Default::default(), &[], &[])
                .map_err(|e| e.to_string())?;
            let lib = hauksbee_models::ModelLibrary::builtin_with_user_dirs(&[]);
            let bound = crate::binder::bind_board(&board, &lib);

            // Stage the firmware (resolving a zip/project upload to its image
            // first). The temp file must outlive this call: the emulated MCU
            // reloads from the path on reset, so it rides along as the
            // session's keepalive.
            let mut keepalive: Option<Box<dyn std::any::Any + Send>> = None;
            let mut fw_path: Option<std::path::PathBuf> = None;
            if let Some((fw_name, fw_bytes)) = fw {
                if bound.mcus.is_empty() {
                    return Err(crate::binder::no_processor_message(
                        &bound.dnp_mcus,
                        crate::binder::FitRemedy::Cli,
                    ));
                }
                let resolved = crate::firmware_input::resolve_firmware_bytes(fw_name, fw_bytes)?;
                let suffix = if resolved.name.to_ascii_lowercase().ends_with(".hex") {
                    ".hex"
                } else {
                    ".elf"
                };
                let mut tmp = tempfile::Builder::new()
                    .prefix("hauksbee-live-fw-")
                    .suffix(suffix)
                    .tempfile()
                    .map_err(|e| format!("could not stage the firmware: {e}"))?;
                tmp.write_all(&resolved.bytes)
                    .and_then(|_| tmp.flush())
                    .map_err(|e| format!("could not write the firmware: {e}"))?;
                fw_path = Some(tmp.path().to_path_buf());
                keepalive = Some(Box::new(tmp));
            }

            let board_url = format!("/boards/{name}");
            let mut engine = HauksbeeEngine::from_bound(bound, fw_path.as_deref(), &board_url)
                .map_err(|e| e.to_string())?;
            // This engine is a live web session: end a dead solve's step at
            // the abort streak (see `arm_live_abort`).
            engine.arm_live_abort();
            // Same bridge-or-refuse policy as the web co-sim (validated shorts
            // bridged, KiCad-10 `version_warning` shorts left alone), recorded
            // on the engine so the sim view can disclose it.
            engine.apply_and_disclose_drc_shorts(&drc);
            Ok(LiveLaunch {
                engine: Box::new(engine),
                board_name: name.to_string(),
                board_file: norm.layout_text.map(|t| (name.to_string(), t)),
                keepalive,
            })
        },
    )
}

pub fn serve(
    mut engine: HauksbeeEngine,
    port: u16,
    board_file: Option<(String, String)>,
    startup_json: String,
    open: bool,
    no_open: bool,
) -> anyhow::Result<()> {
    use std::sync::Arc;
    // The preloaded engine is about to become the live session behind /ws:
    // same live-only dead-solve behaviour as an uploaded board's engine.
    engine.arm_live_abort();
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        // Name the preloaded session by its board FILE name (the board_file
        // route is "/boards/<file>"): layout files often carry no board name,
        // and an unnamed session breaks the web app's session-identity
        // surfaces (`/api/live/status`, the wrong-board banner).
        let session_name = board_file
            .as_ref()
            .and_then(|(url, _)| url.rsplit('/').next())
            .map(|s| s.to_string());
        let server = Server::new_named(Box::new(engine), session_name);
        // Resolve the web app via the ladder (HAUKSBEE_WEB_DIST override ->
        // checkout dist -> embedded copy), so an installed release binary shows
        // the live viewer too, not just a source checkout.
        let dir = crate::web_dist::resolve_web_dist();
        let addr = format!("127.0.0.1:{port}");

        // The analysis API the React landing calls (the report and the live sim
        // are one app). Same callback `hauksbee serve` uses, so the two
        // commands converge on one server path with a preload difference only.
        let analyze: hauksbee_server::frontdoor::FirmwareAnalyzer = Arc::new(
            |name: &str, contents: &[u8], fw: Option<(&str, &[u8])>| match fw {
                Some((fw_name, fw_bytes)) => {
                    crate::analyze_with_firmware_json(name, contents, fw_name, fw_bytes)
                }
                None => crate::analyze_json(name, contents),
            },
        );
        // Same checks backend as `hauksbee serve`, so the web checks panel works
        // in the preloaded (`run --serve`) flow too.
        let check: hauksbee_server::frontdoor::CheckRunner =
            Arc::new(|name, contents, fw, spec| {
                crate::webcheck::run_web_check(name, contents, fw, spec)
            });

        // Bind FIRST, then print. The requested port may be busy, in which case
        // the bind falls back to another port; printing `addr` before binding
        // advertised a URL the server was not actually listening on.
        let (listener, bound) = hauksbee_server::bind_frontdoor(&addr).await?;
        // Browser auto-open: the SAME policy as the `serve` subcommand (one
        // rule, two front doors): the explicit --open flag or a desktop .app
        // launch triggers it, --no-open vetoes both, and it only fires when
        // the web UI actually resolved.
        {
            let launched_by_app = std::env::var_os("HAUKSBEE_EXIT_WITH_PARENT").is_some();
            if !no_open && (open || launched_by_app) && dir.is_some() {
                crate::commands::serve::open_browser(&format!("http://{bound}"));
            }
        }
        if let Some(static_dir) = dir.as_ref() {
            warn_if_dist_stale(static_dir);
            println!("\n  hauksbee is live. Open this in your browser:\n");
            println!("      http://{bound}\n");
            println!("  Lands on this board's report; press \"run it\" for the live 2D/3D sim.");
            println!("  Ctrl-C to stop.\n");
        } else {
            // The frontend is a build artifact and is not checked in, so a fresh
            // clone has no dist/ yet. Serve the websocket + API regardless (so an
            // external viewer still works) but tell the user how to get the live
            // view rather than leaving them on a blank 404 page.
            println!(
                "\n  hauksbee websocket server is live at ws://{bound}/ws  (Ctrl-C to stop)\n"
            );
            println!("  The live viewer at http://{bound} needs the frontend built once:\n");
            println!("      cd frontend && bun install && bun run build\n");
            println!("  then re-run this command. For a quick non-visual check, try:\n");
            println!("      hauksbee run <board> --report      # bind table");
            println!("      hauksbee run <board> --drc          # copper shorts");
            println!("      hauksbee run <board> --headless     # co-sim summary\n");
        }
        server
            .serve_app_on(
                listener,
                dir.as_deref(),
                board_file,
                analyze,
                Some(check),
                Some(deps_hooks()),
                Some(live_launcher()),
                startup_json,
            )
            .await
    })
}

/// Advisory staleness check for the served React bundle. `frontend/dist` is a
/// gitignored build artifact: `git pull` updates `frontend/src` but never
/// `dist/`, so `hauksbee serve` happily serves a stale bundle and the user
/// re-hits bugs that are already fixed in the sources. If any file under the
/// sibling `frontend/src` (or `frontend/index.html`) is newer than
/// `dist/index.html`, say so, loudly enough to act on, quietly enough not to
/// block anything.
pub fn warn_if_dist_stale(dist_dir: &Path) {
    let Some(frontend) = dist_dir.parent() else {
        return;
    };
    if let Some(msg) = dist_stale_message(
        dist_dir,
        &[frontend.join("src"), frontend.join("index.html")],
    ) {
        eprintln!("{msg}");
    }
}

/// Testable core of [`warn_if_dist_stale`]: returns the warning to print when
/// `dist_dir/index.html` is older than any file under `source_paths`.
pub fn dist_stale_message(dist_dir: &Path, source_paths: &[PathBuf]) -> Option<String> {
    let built = std::fs::metadata(dist_dir.join("index.html"))
        .and_then(|m| m.modified())
        .ok()?;
    let mut newest: Option<(PathBuf, std::time::SystemTime)> = None;
    let mut stack: Vec<PathBuf> = source_paths.to_vec();
    while let Some(p) = stack.pop() {
        let Ok(meta) = std::fs::metadata(&p) else {
            continue;
        };
        if meta.is_dir() {
            if let Ok(entries) = std::fs::read_dir(&p) {
                stack.extend(entries.flatten().map(|e| e.path()));
            }
        } else if let Ok(m) = meta.modified() {
            if m > built && newest.as_ref().is_none_or(|(_, t)| m > *t) {
                newest = Some((p, m));
            }
        }
    }
    let (path, _) = newest?;
    // The path mixes a compile-time CARGO_MANIFEST_DIR prefix with runtime
    // separators, which prints as 'a/b\c' on Windows. One separator style,
    // and the repo-relative tail is the readable part anyway.
    let shown = path.display().to_string().replace('\\', "/");
    let shown = shown
        .rsplit_once("/frontend/")
        .map(|(_, tail)| format!("frontend/{tail}"))
        .unwrap_or(shown);
    Some(format!(
        "  warning: the web app bundle looks STALE: frontend/dist was built before\n  \
         '{shown}' changed. You may be served old, already-fixed behaviour. Rebuild with:\n\n      \
         cd frontend && bun install && bun run build\n"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// The staleness advisory fires when a source file is newer than the built
    /// dist/index.html, and stays quiet when the build is up to date. This is
    /// the regression test for the "git pull updates src, `hauksbee serve`
    /// serves the stale bundle, user re-hits an already-fixed bug" trap.
    #[test]
    fn dist_stale_message_fires_only_when_sources_are_newer() {
        let root = std::env::temp_dir().join(format!("hauksbee-stale-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let dist = root.join("dist");
        let src = root.join("src");
        fs::create_dir_all(&dist).unwrap();
        fs::create_dir_all(src.join("components")).unwrap();

        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        let write_with_mtime = |p: &Path, t: std::time::SystemTime| {
            fs::write(p, "x").unwrap();
            let f = fs::File::options().write(true).open(p).unwrap();
            f.set_modified(t).unwrap();
        };

        // Fresh build: sources older than dist -> no warning.
        write_with_mtime(&src.join("components/App.tsx"), old);
        write_with_mtime(&dist.join("index.html"), std::time::SystemTime::now());
        assert!(
            dist_stale_message(&dist, &[src.clone()]).is_none(),
            "up-to-date dist must not warn"
        );

        // A pull touched a nested source file after the build -> warn, naming it.
        write_with_mtime(&dist.join("index.html"), old);
        write_with_mtime(
            &src.join("components/App.tsx"),
            std::time::SystemTime::now(),
        );
        let msg =
            dist_stale_message(&dist, &[src.clone()]).expect("stale dist must produce a warning");
        assert!(msg.contains("STALE"), "message names the condition: {msg}");
        assert!(
            msg.contains("App.tsx"),
            "message names the newer file: {msg}"
        );
        assert!(
            msg.contains("bun run build"),
            "message says how to fix: {msg}"
        );

        // No dist at all (fresh clone): nothing to compare, no warning here
        // (the serve commands already print the build instructions).
        assert!(dist_stale_message(&root.join("missing"), &[src]).is_none());

        let _ = fs::remove_dir_all(&root);
    }
}
