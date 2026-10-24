//! The parity ledger's writer: every finding `engine_lint` raises on every
//! board under a set of roots, one JSON line per finding.
//!
//! A change to how a check judges (the supply envelope moving from rail-name
//! tables to the solved DC levels, say) is compared by running this at the
//! commit before and the commit after over the same boards, then diffing the
//! two files by (board, check, parts, nets): findings only before, only after,
//! and in both. It is not a gate: it asserts nothing about the findings, so it
//! is ignored unless asked for.
//!
//! ```text
//! HAUKSBEE_LINT_DUMP=/tmp/after.jsonl \
//! HAUKSBEE_LINT_DUMP_ROOTS=/path/to/board-corpus:/path/to/fixtures \
//!   cargo nextest run -p hauksbee-engine --run-ignored only lint_parity_dump
//! ```
//!
//! Each line: `{"board": <path under its root>, "check", "severity", "refs",
//! "nets", "message"}`, plus one `{"board", "error"}` line for a board the
//! extractor could not read and one `{"board", "panicked"}` for a lint that
//! panicked, so a board that stops loading is not read as one that went quiet.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use hauksbee_engine::checks::engine_lint;
use hauksbee_extract::ExtractedBoard;
use hauksbee_models::ModelLibrary;

fn load(p: &Path) -> Result<ExtractedBoard, String> {
    if p.extension().and_then(|e| e.to_str()) == Some("kicad_sch") {
        return ExtractedBoard::from_kicad_schematic_path(p).map_err(|e| e.to_string());
    }
    let text = std::fs::read_to_string(p).map_err(|e| e.to_string())?;
    ExtractedBoard::from_auto(&text).map_err(|e| e.to_string())
}

#[test]
#[ignore = "writes a parity ledger; set HAUKSBEE_LINT_DUMP and HAUKSBEE_LINT_DUMP_ROOTS"]
fn lint_parity_dump() {
    let (Some(out), Some(roots)) = (
        std::env::var_os("HAUKSBEE_LINT_DUMP"),
        std::env::var_os("HAUKSBEE_LINT_DUMP_ROOTS"),
    ) else {
        eprintln!("HAUKSBEE_LINT_DUMP / HAUKSBEE_LINT_DUMP_ROOTS not set; nothing to dump");
        return;
    };
    let mut boards: Vec<(PathBuf, PathBuf)> = Vec::new();
    for root in std::env::split_paths(&roots) {
        let found =
            hauksbee_testkit::walk_files_with_ext(&root, &["kicad_pcb", "kicad_sch", "net", "brd"]);
        boards.extend(found.into_iter().map(|p| (root.clone(), p)));
    }
    boards.sort();
    let lines: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let next = std::sync::atomic::AtomicUsize::new(0);
    let workers = std::env::var("HAUKSBEE_LINT_DUMP_JOBS")
        .ok()
        .and_then(|j| j.parse::<usize>().ok())
        .unwrap_or(3);
    let lib = ModelLibrary::builtin();
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| loop {
                let i = next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let Some((root, path)) = boards.get(i) else {
                    break;
                };
                let name = path
                    .strip_prefix(root)
                    .unwrap_or(path)
                    .to_string_lossy()
                    .into_owned();
                let name = format!(
                    "{}/{name}",
                    root.file_name().unwrap_or_default().to_string_lossy()
                );
                let mut mine = Vec::new();
                match load(path) {
                    Err(error) => {
                        mine.push(serde_json::json!({"board": name, "error": error}).to_string());
                    }
                    Ok(board) => {
                        let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            engine_lint(&board, &lib)
                        }));
                        match ran {
                            Err(_) => mine.push(
                                serde_json::json!({"board": name, "panicked": true}).to_string(),
                            ),
                            Ok(report) => {
                                for f in report.findings {
                                    mine.push(
                                        serde_json::json!({
                                            "board": name,
                                            "check": format!("{:?}", f.check),
                                            "severity": format!("{:?}", f.severity),
                                            "refs": f.refs,
                                            "nets": f.nets,
                                            "message": f.message,
                                        })
                                        .to_string(),
                                    );
                                }
                            }
                        }
                    }
                }
                lines.lock().unwrap().extend(mine);
            });
        }
    });
    let mut lines = lines.into_inner().unwrap();
    lines.sort();
    let mut file = std::fs::File::create(out).unwrap();
    for line in &lines {
        writeln!(file, "{line}").unwrap();
    }
    eprintln!(
        "lint_parity_dump: {} boards, {} lines",
        boards.len(),
        lines.len()
    );
}
