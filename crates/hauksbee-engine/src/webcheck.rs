//! The `/api/check` backend: run the checks the web builder composed, using
//! the REAL `hauksbee-ci` binary.
//!
//! The web checks panel builds the body of a spec (supplies, assertions,
//! duration — everything except the file paths); this module stages the
//! uploaded board and firmware in a temp dir, injects the `board`/`firmware`
//! keys, and shells the installed `hauksbee-ci run <spec> --json`. Shelling
//! the sibling binary — not reimplementing the runner — is the point: the
//! result the browser shows is byte-for-byte what a pipeline would produce,
//! so the web panel can never drift from CI truth. Detect-don't-bundle,
//! exactly like the kicad-cli / ngspice oracles.

use std::path::PathBuf;

/// Reject path separators and other surprises in an uploaded file name; the
/// staged file keeps its extension (format sniffing needs it) but nothing else
/// exotic.
fn sanitize_name(name: &str, fallback: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(fallback);
    let clean: String = base
        .chars()
        .map(|c| if c.is_alphanumeric() || ".-_".contains(c) { c } else { '_' })
        .collect();
    if clean.is_empty() || clean.starts_with('.') {
        format!("{fallback}{clean}")
    } else {
        clean
    }
}

/// The `hauksbee-ci` binary to run: `HAUKSBEE_CI_BIN` override, the sibling of
/// the current executable (the install layout), then PATH.
fn ci_binary() -> PathBuf {
    if let Ok(p) = std::env::var("HAUKSBEE_CI_BIN") {
        return PathBuf::from(p);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let sibling = dir.join("hauksbee-ci");
            if sibling.is_file() {
                return sibling;
            }
        }
    }
    PathBuf::from("hauksbee-ci")
}

fn err_json(msg: &str) -> String {
    serde_json::to_string(&serde_json::json!({ "ok": false, "error": msg }))
        .unwrap_or_else(|_| "{\"ok\":false,\"error\":\"internal error\"}".to_string())
}

/// Hard ceiling on one web-triggered check run. The builder caps duration_ms
/// client-side, but a pathological spec (huge fuzz count, a hung external
/// emulator) must not pin the request forever.
const CHECK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// Stage the uploaded files, inject the path keys, run `hauksbee-ci --json`,
/// and relay its JSON. Every failure mode returns `{"ok":false,"error":...}`
/// so the browser always has something readable.
pub fn run_web_check(
    board_name: &str,
    board_bytes: &[u8],
    firmware: Option<(&str, &[u8])>,
    spec_fragment: &str,
) -> String {
    // The client composes everything EXCEPT the file paths; a fragment that
    // smuggles its own board/firmware would silently point the run somewhere
    // else, so refuse it loudly.
    for line in spec_fragment.lines() {
        let t = line.trim_start();
        if t.starts_with("board") || t.starts_with("firmware") {
            let key = t.split(['=', ' ', '\t']).next().unwrap_or("");
            if key == "board" || key == "firmware" {
                return err_json(
                    "the spec body must not set `board` or `firmware` — those keys are \
                     filled in from the uploaded files",
                );
            }
        }
    }

    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "hauksbee-web-check-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return err_json(&format!("could not create a working dir: {e}"));
    }

    let board_file = sanitize_name(board_name, "board.kicad_pcb");
    if let Err(e) = std::fs::write(dir.join(&board_file), board_bytes) {
        return err_json(&format!("could not stage the board: {e}"));
    }
    let mut spec = format!("board = \"{board_file}\"\n");
    if let Some((fw_name, fw_bytes)) = firmware {
        let fw_file = sanitize_name(fw_name, "firmware.elf");
        if let Err(e) = std::fs::write(dir.join(&fw_file), fw_bytes) {
            return err_json(&format!("could not stage the firmware: {e}"));
        }
        spec.push_str(&format!("firmware = \"{fw_file}\"\n"));
    }
    spec.push('\n');
    spec.push_str(spec_fragment);
    let spec_path = dir.join("spec.toml");
    if let Err(e) = std::fs::write(&spec_path, &spec) {
        return err_json(&format!("could not stage the spec: {e}"));
    }

    let bin = ci_binary();
    let child = std::process::Command::new(&bin)
        .arg("run")
        .arg(&spec_path)
        .arg("--json")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return err_json(&format!(
                "hauksbee-ci was not found (looked for '{}'). It installs alongside \
                 hauksbee — re-run scripts/install.sh, or set HAUKSBEE_CI_BIN.",
                bin.display()
            ));
        }
        Err(e) => return err_json(&format!("could not run hauksbee-ci: {e}")),
    };

    // Poll-wait with a deadline instead of output(): a hung run must produce
    // an error the browser can show, not an eternally pending request.
    let started = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if started.elapsed() > CHECK_TIMEOUT {
                    let _ = child.kill();
                    return err_json(&format!(
                        "the check run exceeded {}s and was stopped — shorten duration_ms \
                         or run it from the CLI: hauksbee-ci run <spec>",
                        CHECK_TIMEOUT.as_secs()
                    ));
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(e) => return err_json(&format!("waiting for hauksbee-ci: {e}")),
        }
    }
    let out = match child.wait_with_output() {
        Ok(o) => o,
        Err(e) => return err_json(&format!("reading hauksbee-ci output: {e}")),
    };

    // --json prints one JSON object on stdout for both green and red runs, and
    // {"ok":false,...} for a spec error. Anything else is relayed with the
    // stderr tail so the user sees the real failure.
    let stdout = String::from_utf8_lossy(&out.stdout);
    if let Some(json_line) = stdout.lines().find(|l| l.trim_start().starts_with('{')) {
        return json_line.to_string();
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    let tail: Vec<&str> = stderr.lines().rev().take(12).collect();
    let tail: Vec<&str> = tail.into_iter().rev().collect();
    err_json(&format!(
        "hauksbee-ci produced no result (exit {:?}). {}",
        out.status.code(),
        tail.join(" | ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOOT_GATE: &str =
        include_str!("../../hauksbee-ci/examples/boards/boot_gate.kicad_pcb");

    fn spec_fragment() -> &'static str {
        r#"name = "web check"
duration_ms = 10

[[supply]]
net = "+5V"
kind = "ideal"
volts = 5.0

[[assert]]
kind = "no_faults"
"#
    }

    /// End-to-end through the real sibling binary when it exists (a build tree
    /// or an installed layout); skips cleanly otherwise so a bare `cargo test`
    /// on a fresh clone stays green.
    #[test]
    fn web_check_runs_the_real_ci_binary() {
        // In a cargo test the current_exe is the test runner deep in target/,
        // so resolve the built binary explicitly.
        let ci = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/release/hauksbee-ci");
        if !ci.is_file() {
            eprintln!("skipping web_check e2e (no release hauksbee-ci built)");
            return;
        }
        std::env::set_var("HAUKSBEE_CI_BIN", &ci);
        let json = run_web_check("boot_gate.kicad_pcb", BOOT_GATE.as_bytes(), None, spec_fragment());
        std::env::remove_var("HAUKSBEE_CI_BIN");
        let v: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert_eq!(v["ok"], true, "run succeeds: {json}");
        assert_eq!(v["passed"], true, "no_faults holds on the demo board: {json}");
        assert!(v["results"].as_array().is_some_and(|r| !r.is_empty()));
    }

    #[test]
    fn spec_fragment_must_not_smuggle_paths() {
        let json = run_web_check(
            "b.kicad_pcb",
            b"(kicad_pcb)",
            None,
            "board = \"/etc/passwd\"\n[[assert]]\nkind = \"no_faults\"\n",
        );
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["ok"], false);
        assert!(v["error"].as_str().unwrap_or("").contains("must not set"));
    }
}
