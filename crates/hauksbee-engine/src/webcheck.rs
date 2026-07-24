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

/// Every spec key that names a file on disk. `board`/`firmware` are filled in
/// server-side from the uploads; the rest would let a raw fragment read
/// (`asbuilt`, an hwtrace `trace` and the data files it references, a sensor's
/// `spec_file`) or overwrite (a vcd_sink's `vcd_path` reaches `File::create`)
/// paths outside the staging dir, because the CI resolver accepts absolute
/// paths and `..` traversal.
const PATH_KEYS: [&str; 6] = ["board", "firmware", "asbuilt", "trace", "spec_file", "vcd_path"];

/// The first path-bearing key the fragment tries to set, if any.
///
/// Two passes. The line scan matches the key as the first token of a possibly
/// indented line, the way the builder (and a hand-written spec) emits it. The
/// TOML parse then catches the same keys smuggled where a line scan cannot see
/// them: inline tables (`peripheral = [{ vcd_path = "..." }]`), dotted keys
/// (`assert.trace = "..."`), and quoted keys. A fragment that fails to parse
/// cannot smuggle a path either: hauksbee-ci parses the identical text and
/// refuses it before resolving anything.
fn forbidden_path_key(spec_fragment: &str) -> Option<&'static str> {
    for line in spec_fragment.lines() {
        let key = line.trim_start().split(['=', ' ', '\t']).next().unwrap_or("");
        if let Some(hit) = PATH_KEYS.iter().copied().find(|k| *k == key) {
            return Some(hit);
        }
    }
    spec_fragment
        .parse::<toml::Value>()
        .ok()
        .as_ref()
        .and_then(toml_path_key)
}

fn toml_path_key(value: &toml::Value) -> Option<&'static str> {
    match value {
        toml::Value::Table(table) => table.iter().find_map(|(key, val)| {
            PATH_KEYS
                .iter()
                .copied()
                .find(|k| *k == key.as_str())
                .or_else(|| toml_path_key(val))
        }),
        toml::Value::Array(items) => items.iter().find_map(toml_path_key),
        _ => None,
    }
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
    // smuggles any path-bearing key would silently point the run at (or, for
    // `vcd_path`, write over) files outside the staging dir, so refuse it
    // loudly and name the key.
    if let Some(key) = forbidden_path_key(spec_fragment) {
        return err_json(&format!(
            "the spec body must not set `{key}`: path-bearing keys are filled in \
             from the uploaded files or are not available from the web panel"
        ));
    }

    // A fresh unpredictable staging dir (created 0700 with O_EXCL semantics),
    // not a guessable PID-plus-counter path under /tmp that a local attacker
    // could pre-create as a symlink and redirect the writes below. The guard
    // must stay alive until after the child exits: hauksbee-ci reads the
    // staged files while it runs. It is cleaned up when this function returns.
    let staging = match tempfile::TempDir::new() {
        Ok(d) => d,
        Err(e) => return err_json(&format!("could not create a working dir: {e}")),
    };
    let dir = staging.path();

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

    /// Every path-bearing spec key is refused, top-level and indented, and the
    /// rejection happens before anything touches the filesystem.
    #[test]
    fn every_path_bearing_key_is_rejected() {
        for key in PATH_KEYS {
            let frag = format!("{key} = \"/etc/passwd\"\n");
            assert_eq!(forbidden_path_key(&frag), Some(key), "top-level {key}");
            let indented = format!("  \t{key} = \"../../escape\"\n");
            assert_eq!(forbidden_path_key(&indented), Some(key), "indented {key}");
            let json = run_web_check("b.kicad_pcb", b"(kicad_pcb)", None, &frag);
            let v: serde_json::Value = serde_json::from_str(&json).unwrap();
            assert_eq!(v["ok"], false, "{key} fragment must be refused");
            assert!(
                v["error"].as_str().unwrap_or("").contains(key),
                "error names the offending key {key}: {json}"
            );
        }
    }

    /// Keys inside `[[table]]` blocks, inline tables, and dotted keys cannot
    /// sneak past the line scan: the TOML walk catches them.
    #[test]
    fn nested_and_inline_path_keys_are_rejected() {
        let block = "[[assert]]\nkind = \"hwtrace\"\ntrace = \"../secrets/trace.toml\"\n";
        assert_eq!(forbidden_path_key(block), Some("trace"));
        let inline = "peripheral = [{ id = \"scope\", type = \"vcd_sink\", net = \"CLK\", vcd_path = \"/tmp/pwn.vcd\" }]\n";
        assert_eq!(forbidden_path_key(inline), Some("vcd_path"));
        let dotted = "assert.trace = \"/etc/passwd\"\n";
        assert_eq!(forbidden_path_key(dotted), Some("trace"));
        let quoted = "\"vcd_path\" = \"/tmp/pwn.vcd\"\n";
        assert_eq!(forbidden_path_key(quoted), Some("vcd_path"));
        let sensor = "[[sensor]]\nid = \"t\"\nspec_file = \"/home/user/.ssh/id_rsa\"\n";
        assert_eq!(forbidden_path_key(sensor), Some("spec_file"));
    }

    /// A legitimate fragment, and identifiers that merely CONTAIN a forbidden
    /// key or mention one inside a string, pass the filter.
    #[test]
    fn benign_fragments_pass_the_path_filter() {
        assert_eq!(forbidden_path_key(spec_fragment()), None);
        let lookalikes = "name = \"trace of the board\"\nboard_rev = \"C\"\nfirmware_version = 2\n";
        assert_eq!(forbidden_path_key(lookalikes), None);
    }
}
