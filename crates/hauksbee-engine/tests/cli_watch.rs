//! End-to-end test for `hauksbee watch`: it must re-run the check when a watched
//! file changes. Detection + verdict + watch-set logic are unit-tested in the
//! library (`commands::watch`); this exercises the compiled binary's live loop.
//!
//! The production watcher polls a small, non-recursive dependency set, so this
//! test makes one real content change and requires one observed re-run. It is a
//! release contract, not an ignorable timing probe.

use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_hauksbee")
}

/// A small, real board fixture shipped with the CI examples.
fn blinky() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../hauksbee-ci/examples/boards/blinky.kicad_pcb")
}

#[test]
fn watch_reruns_on_change() {
    // Copy the fixture into a tempdir so touching it never dirties the repo.
    let dir = tempfile::tempdir().unwrap();
    let board = dir.path().join("blinky.kicad_pcb");
    let bytes = std::fs::read(blinky()).expect("read fixture board");
    std::fs::write(&board, &bytes).unwrap();

    let mut child = Command::new(bin())
        .arg("watch")
        .arg(&board)
        .arg("--plain")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn hauksbee watch");

    // Drain stdout on a thread into a shared buffer we can poll.
    let buf = Arc::new(Mutex::new(String::new()));
    let mut out = child.stdout.take().unwrap();
    let buf_thread = Arc::clone(&buf);
    let reader = std::thread::spawn(move || {
        let mut chunk = [0u8; 4096];
        loop {
            match out.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => buf_thread
                    .lock()
                    .unwrap()
                    .push_str(&String::from_utf8_lossy(&chunk[..n])),
            }
        }
    });

    let contains = |needle: &str| buf.lock().unwrap().contains(needle);
    let wait_for = |needle: &str, secs: u64| {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < deadline {
            if contains(needle) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        false
    };

    // Run #1 fires immediately at startup.
    assert!(
        wait_for("run #1", 30),
        "startup run did not appear:\n{}",
        buf.lock().unwrap()
    );

    // A genuine content change. Rewriting the exact same bytes is not a change
    // and native event backends are permitted to coalesce it away.
    let mut changed = bytes.clone();
    changed.extend_from_slice(b"\n# watch integration test\n");
    std::fs::write(&board, changed).unwrap();
    let saw_run2 = wait_for_short(&buf, "run #2", 30_000);

    let _ = child.kill();
    let _ = child.wait();
    let _ = reader.join();

    let captured = buf.lock().unwrap().clone();
    assert!(
        saw_run2,
        "watch did not re-run after the file changed:\n{captured}"
    );
    // The stream shows a separator and per-run headers for both runs.
    assert!(
        captured.contains("run #1"),
        "missing run #1 header:\n{captured}"
    );
    assert!(
        captured.contains("run #2"),
        "missing run #2 header:\n{captured}"
    );
    assert!(
        captured.contains("changed: blinky.kicad_pcb"),
        "missing change note:\n{captured}"
    );
}

/// Poll a shared buffer for `needle` for up to `ms` milliseconds.
fn wait_for_short(buf: &Arc<Mutex<String>>, needle: &str, ms: u64) -> bool {
    let deadline = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < deadline {
        if buf.lock().unwrap().contains(needle) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

#[test]
fn watch_once_runs_a_single_check_and_exits() {
    // `--once` is the plumbing check: it runs one check and exits with that run's
    // code, without entering the watch loop.
    let out = Command::new(bin())
        .arg("watch")
        .arg(blinky())
        .arg("--once")
        .output()
        .expect("run hauksbee watch --once");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("run #1"),
        "once mode should print the single run:\n{stdout}"
    );
    // It exits with the check's own code (0 or 2 for a board), never hanging.
    let code = out.status.code().unwrap_or(-1);
    assert!(
        code == 0 || code == 2,
        "unexpected exit code {code}:\n{stdout}"
    );
}

#[test]
fn watch_refuses_unknown_target() {
    let dir = tempfile::tempdir().unwrap();
    let txt = dir.path().join("notes.txt");
    std::fs::write(&txt, "hello").unwrap();
    let out = Command::new(bin())
        .arg("watch")
        .arg(&txt)
        .output()
        .expect("run hauksbee watch on a .txt");
    assert!(!out.status.success(), "watching a .txt must fail");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("don't know how to watch"),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains(".board"),
        "accepted targets listed: {stderr}"
    );
    assert!(
        stderr.contains(".toml"),
        "accepted targets listed: {stderr}"
    );
}
