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
    startup_json: String,
) -> anyhow::Result<()> {
    use std::sync::Arc;
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let server = Server::new(Box::new(engine));
        let static_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../frontend/dist");
        let dir = static_dir.exists().then(|| static_dir.clone());
        let addr = format!("127.0.0.1:{port}");

        // The analysis API the React landing calls (W6 §1: the report and the
        // live sim are one app). Same callback `hauksbee serve` uses, so the two
        // commands converge on one server path with a preload difference only.
        let analyze: hauksbee_server::frontdoor::FirmwareAnalyzer = Arc::new(
            |name: &str, contents: &[u8], fw: Option<(&str, &[u8])>| match fw {
                Some((fw_name, fw_bytes)) => {
                    crate::analyze_with_firmware_json(name, contents, fw_name, fw_bytes)
                }
                None => crate::analyze_json(name, contents),
            },
        );

        // Bind FIRST, then print. The requested port may be busy, in which case
        // the bind falls back to another port; printing `addr` before binding
        // advertised a URL the server was not actually listening on.
        let (listener, bound) = hauksbee_server::bind_frontdoor(&addr).await?;
        if dir.is_some() {
            warn_if_dist_stale(&static_dir);
            println!("\n  hauksbee is live. Open this in your browser:\n");
            println!("      http://{bound}\n");
            println!("  Lands on this board's report; press \"run it\" for the live 2D/3D sim.");
            println!("  Ctrl-C to stop.\n");
        } else {
            // The frontend is a build artifact and is not checked in, so a fresh
            // clone has no dist/ yet. Serve the websocket + API regardless (so an
            // external viewer still works) but tell the user how to get the live
            // view rather than leaving them on a blank 404 page.
            println!("\n  hauksbee websocket server is live at ws://{bound}/ws  (Ctrl-C to stop)\n");
            println!("  The live viewer at http://{bound} needs the frontend built once:\n");
            println!("      cd frontend && bun install && bun run build\n");
            println!("  then re-run this command. For a quick non-visual check, try:\n");
            println!("      hauksbee run <board> --report      # bind table");
            println!("      hauksbee run <board> --drc          # copper shorts");
            println!("      hauksbee run <board> --headless     # co-sim summary\n");
        }
        server
            .serve_app_on(listener, dir.as_deref(), board_file, analyze, startup_json)
            .await
    })
}

/// Advisory staleness check for the served React bundle. `frontend/dist` is a
/// gitignored build artifact: `git pull` updates `frontend/src` but never
/// `dist/`, so `hauksbee serve` happily serves a stale bundle and the user
/// re-hits bugs that are already fixed in the sources. If any file under the
/// sibling `frontend/src` (or `frontend/index.html`) is newer than
/// `dist/index.html`, say so — loudly enough to act on, quietly enough not to
/// block anything.
pub fn warn_if_dist_stale(dist_dir: &Path) {
    let Some(frontend) = dist_dir.parent() else { return };
    if let Some(msg) = dist_stale_message(dist_dir, &[frontend.join("src"), frontend.join("index.html")]) {
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
        let Ok(meta) = std::fs::metadata(&p) else { continue };
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
    Some(format!(
        "  warning: the web app bundle looks STALE — frontend/dist was built before\n  \
         '{}' changed. You may be served old, already-fixed behaviour. Rebuild with:\n\n      \
         cd frontend && bun install && bun run build\n",
        path.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// The staleness advisory fires when a source file is newer than the built
    /// dist/index.html, and stays quiet when the build is up to date. This is
    /// the regression test for the "git pull updated src, `hauksbee serve`
    /// served the old bundle, user re-hit an already-fixed bug" trap.
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
        let msg = dist_stale_message(&dist, &[src.clone()])
            .expect("stale dist must produce a warning");
        assert!(msg.contains("STALE"), "message names the condition: {msg}");
        assert!(msg.contains("App.tsx"), "message names the newer file: {msg}");
        assert!(msg.contains("bun run build"), "message says how to fix: {msg}");

        // No dist at all (fresh clone): nothing to compare, no warning here
        // (the serve commands already print the build instructions).
        assert!(dist_stale_message(&root.join("missing"), &[src]).is_none());

        let _ = fs::remove_dir_all(&root);
    }
}
