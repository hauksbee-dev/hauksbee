//! The `/api/deps` backend: probe the optional external dependencies with the
//! engine's OWN discovery, and run the one-click installs the web panel offers.
//!
//! The status side calls the exact resolvers a real run uses
//! (`hauksbee_mcu::renode::find_renode`, `hauksbee_mcu::qemu::find_qemu`), the
//! same posture as `hauksbee doctor --backends`: no re-implemented search
//! logic, so what the panel reports can never drift from what a co-sim would
//! actually accept. The ngspice probe follows the documented lookup the
//! ngspice differential harness uses (`$NGSPICE`, PATH, per-OS defaults); the
//! kicad-cli probe mirrors `reports::drc::find_kicad_cli` (private to a module
//! another lane owns, keep the two in lockstep if either changes).
//!
//! The install side shells KILLABLE children and streams their output.
//! `hauksbee install esp-qemu --yes` uses the checksum-pinned native PowerShell
//! installer on Windows and this binary's Rust manifest-verifying installer on
//! Unix. Renode likewise uses the checksum-pinned platform installer
//! (`install-sims.sh` on Unix, `install-sims-windows.ps1` on Windows; both are
//! embedded and shipped beside the binary). One install runs
//! at a time (RAII slot, same pattern as `webcheck::WebCheckSlot`), a hard
//! timeout kills the whole child process group, and a failure surfaces the
//! child's actual output tail, never a bare exit code.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// One probed dependency, serialized verbatim as the `/api/deps` JSON.
#[derive(serde::Serialize)]
pub struct DepStatus {
    /// Stable machine id (`renode`, `esp-qemu`, `ngspice`, `kicad-cli`, `avr`).
    pub id: &'static str,
    /// Human display name.
    pub name: &'static str,
    /// True when the engine's own resolver found the local dependency.
    ///
    /// For the optional LLM CLIs this is deliberately a discovery result, not
    /// an authentication claim. Their sign-in state is checked only when the
    /// user requests datasheet extraction (the Environment page must not run
    /// login/version commands just to paint a status row).
    pub present: bool,
    /// Resolved binary path(s) when present.
    pub path: Option<String>,
    /// A version string when the resolver already had one without an extra
    /// status-only subprocess. Most rows intentionally leave this empty: a
    /// dependency status page should answer "is it here?", not launch every
    /// simulator's version command.
    pub version: Option<String>,
    /// What having it unlocks, in plain language.
    pub unlocks: &'static str,
    /// True when POST `/api/deps/install/<id>` can install it on this host.
    pub installable: bool,
    /// Honest cost: real download size and OS support.
    pub cost: String,
    /// The manual terminal command, for a user who prefers the shell.
    pub manual: String,
    /// Why it is absent / partial (the resolver's own message), when absent.
    pub detail: Option<String>,
    /// Set only on a dependency that sends the user's data off this machine,
    /// stating plainly what leaves and where it goes.
    ///
    /// Every other dependency here is a local binary: installing it has no
    /// privacy consequence, so there is nothing to say. Codex is different in
    /// kind, not degree, and burying that difference in the `unlocks` prose
    /// would let a UI render it as just another install button. A separate
    /// field means a surface has to decide what to do with it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sends_data_offhost: Option<&'static str>,
}

/// Probe every dependency. Each probe runs the engine's own discovery; none of
/// this is a plain `which` that could disagree with a real run. Independent
/// resolvers run in parallel so a slow local tool cannot hold up all the other
/// rows. Version and authentication commands are intentionally deferred to the
/// operation that needs them.
pub fn probe_all() -> Vec<DepStatus> {
    std::thread::scope(|scope| {
        let renode = scope.spawn(probe_renode);
        let esp_qemu = scope.spawn(probe_esp_qemu);
        let ngspice = scope.spawn(probe_ngspice);
        let kicad_cli = scope.spawn(probe_kicad_cli);
        let avr = scope.spawn(probe_avr);
        let codex = scope.spawn(probe_codex);
        let claude_code = scope.spawn(probe_claude_code);
        vec![
            renode.join().expect("Renode dependency probe panicked"),
            esp_qemu
                .join()
                .expect("Espressif QEMU dependency probe panicked"),
            ngspice.join().expect("ngspice dependency probe panicked"),
            kicad_cli
                .join()
                .expect("kicad-cli dependency probe panicked"),
            avr.join().expect("AVR dependency probe panicked"),
            codex.join().expect("Codex dependency probe panicked"),
            claude_code
                .join()
                .expect("Claude Code dependency probe panicked"),
        ]
    })
}

/// Probe only the two extractors for `/api/models/extract/ready`.
///
/// The Environment page uses [`probe_all`] and intentionally does not run an
/// authentication command. Readiness is different: before offering consent,
/// it must fail closed if the Codex CLI is installed but not signed in. Keeping
/// this narrow avoids making a model-readiness request wait for KiCad/QEMU.
pub(crate) fn probe_extractors() -> Vec<DepStatus> {
    let mut codex = probe_codex();
    if let Some(path) = codex.path.clone() {
        match codex_login_state(std::path::Path::new(&path)) {
            CodexLogin::LoggedIn(how) => {
                codex.detail = how;
            }
            CodexLogin::NotLoggedIn => {
                codex.present = false;
                codex.manual = "codex login".to_string();
                codex.detail = Some(
                    "codex is installed but not signed in, so an extraction would fail once \
                     you had already asked for it. Run `codex login`. It signs in with a \
                     ChatGPT account, so if you have one there is nothing else to pay."
                        .to_string(),
                );
            }
        }
    }
    vec![codex, probe_claude_code()]
}

/// Claude Code, the second datasheet-to-model extractor backend. Same shape
/// and same privacy honesty as the codex probe: using it sends datasheet
/// text to Anthropic, nothing runs unless the user asks, and it is never
/// auto-installed. Presence means the `claude` CLI resolves on PATH. Its
/// version and sign-in state are deliberately deferred until extraction, so a
/// status page never starts the CLI just to display a row.
fn probe_claude_code() -> DepStatus {
    let unlocks = "datasheet-to-model extraction (`hauksbee models extract`, the web Extend flow) via the Claude Code CLI";
    let cost = "free if you already pay for Claude: the CLI signs in with that account".to_string();
    let manual = "npm install -g @anthropic-ai/claude-code   # then: claude login".to_string();
    let privacy = "Using this sends the datasheet's text to Anthropic. Nothing is sent unless                    you ask for an extraction, and hauksbee never runs it on its own.";
    let Some(bin) = which_on_path("claude") else {
        return DepStatus {
            id: "claude-code",
            name: "Claude Code (datasheet extraction)",
            present: false,
            path: None,
            version: None,
            unlocks,
            installable: false,
            cost,
            manual,
            detail: Some(
                "claude not found on PATH. This is optional: extraction also runs via                  codex or an API key, and a model can always be written by hand (one                  TOML file, see docs/extending/)."
                    .to_string(),
            ),
            sends_data_offhost: Some(privacy),
        };
    };
    DepStatus {
        id: "claude-code",
        name: "Claude Code (datasheet extraction)",
        present: true,
        version: None,
        path: Some(bin.display().to_string()),
        unlocks,
        installable: false,
        cost,
        manual,
        detail: Some(
            "claude CLI found; sign-in is checked only when datasheet extraction is requested."
                .to_string(),
        ),
        sends_data_offhost: Some(privacy),
    }
}

/// Codex, the optional datasheet-to-model extractor.
///
/// It is listed with the co-simulation backends because it is discovered the
/// same way and unlocks a capability the same way. It differs in one respect
/// that the UI must not smooth over: using it sends datasheet text to OpenAI.
/// So it is never auto-installed, never runs without being asked, and carries
/// `sends_data_offhost` so no surface can present it as just another local
/// tool.
fn probe_codex() -> DepStatus {
    let unlocks =
        "drafting a device model from a datasheet, for a part with no model (you review it \
         before it is saved)";
    // The cost line is the one most likely to change someone's mind, so it
    // leads with the fact rather than the price: codex signs in with a ChatGPT
    // account, and most people who would want datasheet extraction already pay
    // for one. For them this is free, and the only thing standing between them
    // and it is not knowing.
    let cost = "free if you already pay for ChatGPT: codex signs in with that account".to_string();
    let manual = "npm install -g @openai/codex   # then: codex login".to_string();
    let privacy = "Using this sends the datasheet's text to OpenAI. Nothing is sent unless \
                   you ask for an extraction, and hauksbee never runs it on its own.";

    let Some(bin) = which_codex() else {
        return DepStatus {
            id: "codex",
            name: "Codex (datasheet extraction)",
            present: false,
            path: None,
            version: None,
            unlocks,
            // Deliberately never auto-installable: an account and a login are
            // the user's to give, and a one-click button for a service that
            // takes their data would be the wrong shape whatever it said.
            installable: false,
            cost,
            manual,
            detail: Some(
                "codex not found on PATH. This is optional: every other part of hauksbee \
                 works without it, and a model can always be written by hand (one TOML \
                 file, see docs/extending/)."
                    .to_string(),
            ),
            sends_data_offhost: Some(privacy),
        };
    };

    DepStatus {
        id: "codex",
        name: "Codex (datasheet extraction)",
        // The status page answers the cheap, local question. The readiness
        // endpoint calls `probe_extractors` to turn this into a fail-closed
        // authenticated status before consent is offered.
        present: true,
        version: None,
        path: Some(bin.display().to_string()),
        unlocks,
        installable: false,
        cost,
        manual,
        detail: Some(
            "codex CLI found; login status is checked only when datasheet extraction is requested."
                .to_string(),
        ),
        sends_data_offhost: Some(privacy),
    }
}

/// Whether codex can actually run, and how it is authenticated.
enum CodexLogin {
    /// Signed in. Carries codex's own description, e.g. "Logged in using ChatGPT".
    LoggedIn(Option<String>),
    NotLoggedIn,
}

/// Ask codex itself. Its own answer cannot drift from what an extraction hits.
fn codex_login_state(bin: &std::path::Path) -> CodexLogin {
    let out = std::process::Command::new(bin)
        .args(["login", "status"])
        .output();
    match out {
        Ok(o) if o.status.success() => {
            // codex writes "Logged in using ChatGPT" to STDERR, not stdout.
            // Reading stdout alone lost the description while still getting
            // the state right from the exit code, so the row said "installed"
            // and could not say how it was authenticated.
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            );
            let line = text
                .lines()
                .map(str::trim)
                .find(|l| !l.is_empty())
                .map(str::to_string);
            CodexLogin::LoggedIn(line)
        }
        // A non-zero exit is codex saying no. An error running it at all means
        // we cannot tell, and the safe reading is the one that does not send
        // the user into a failing extraction.
        _ => CodexLogin::NotLoggedIn,
    }
}

fn which_codex() -> Option<std::path::PathBuf> {
    which_on_path("codex")
}

/// Resolve one binary name on PATH (with the Windows .exe/.cmd variants).
fn which_on_path(name: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        if cfg!(windows) {
            for ext in [format!("{name}.exe"), format!("{name}.cmd")] {
                let c = dir.join(&ext);
                if c.is_file() {
                    return Some(c);
                }
            }
        }
        let c = dir.join(name);
        if c.is_file() {
            return Some(c);
        }
    }
    None
}

/// Keep the status response briefly so a page mount, a retry, and a post-install
/// refresh do not all relaunch local resolver subprocesses. The cache is short
/// enough that a manual install becomes visible without restarting the server;
/// successful in-app installs invalidate it immediately.
const DEPS_CACHE_TTL: Duration = Duration::from_secs(2);
static DEPS_JSON_CACHE: OnceLock<Mutex<Option<(Instant, String)>>> = OnceLock::new();

fn deps_cache() -> &'static Mutex<Option<(Instant, String)>> {
    DEPS_JSON_CACHE.get_or_init(|| Mutex::new(None))
}

/// Discard the status snapshot after an installer changes the filesystem.
pub fn invalidate_deps_cache() {
    let mut cache = deps_cache().lock().unwrap_or_else(|e| e.into_inner());
    *cache = None;
}

/// The `/api/deps` response body: `{"deps":[...]}`.
pub fn deps_json() -> String {
    let mut cache = deps_cache().lock().unwrap_or_else(|e| e.into_inner());
    if let Some((at, json)) = cache.as_ref() {
        if at.elapsed() < DEPS_CACHE_TTL {
            return json.clone();
        }
    }
    let json = serde_json::to_string(&serde_json::json!({ "deps": probe_all() }))
        .unwrap_or_else(|_| "{\"deps\":[]}".to_string());
    *cache = Some((Instant::now(), json.clone()));
    json
}

// ── individual probes ────────────────────────────────────────────────────────

fn host_is_installable_os() -> bool {
    matches!(std::env::consts::OS, "macos" | "linux" | "windows")
}

fn probe_renode() -> DepStatus {
    let unlocks = "STM32, nRF52 and RISC-V firmware co-simulation";
    // Real numbers from the renode/renode release assets (v1.16.x): the
    // portable download is 75-90 MB, and the unpacked install is a few
    // hundred MB on disk.
    let cost = if host_is_installable_os() {
        "about a 120 MB download on Windows (about 80 MB on Unix), a few \
         hundred MB unpacked"
            .to_string()
    } else {
        "not auto-installable on this OS; install Renode manually (renode.io)".to_string()
    };
    // The one-click install works from ANY binary now (the installer script is
    // embedded; see `materialize_install_sims_script`), so the manual line can
    // always be the subcommand that runs the same flow.
    let manual = "hauksbee install renode".to_string();

    #[cfg(feature = "renode")]
    {
        match hauksbee_mcu::renode::find_renode() {
            Ok(p) => DepStatus {
                // A local binary: running it sends nothing anywhere.
                sends_data_offhost: None,
                id: "renode",
                name: "Renode",
                present: true,
                path: Some(p.display().to_string()),
                version: None,
                unlocks,
                installable: false,
                cost,
                manual,
                detail: None,
            },
            Err(e) => DepStatus {
                // A local binary: running it sends nothing anywhere.
                sends_data_offhost: None,
                id: "renode",
                name: "Renode",
                present: false,
                path: None,
                version: None,
                unlocks,
                installable: host_is_installable_os(),
                cost,
                manual,
                detail: Some(e.to_string()),
            },
        }
    }
    #[cfg(not(feature = "renode"))]
    {
        DepStatus {
            // A local binary: running it sends nothing anywhere.
            sends_data_offhost: None,
            id: "renode",
            name: "Renode",
            present: false,
            path: None,
            version: None,
            unlocks,
            installable: false,
            cost,
            manual,
            detail: Some(
                "this build of hauksbee was compiled without the `renode` feature, so it \
                 could not use Renode even if installed"
                    .to_string(),
            ),
        }
    }
}

fn probe_esp_qemu() -> DepStatus {
    let unlocks = "ESP32, ESP32-S3 and ESP32-C3 firmware co-simulation";
    // Real numbers from the espressif/qemu release assets (esp-develop-9.2.x):
    // the two per-arch tarballs total ~8 MB on macOS and ~35 MB on Linux.
    // The checksum wording is conditional on purpose. The installer verifies
    // against the release's own manifest when it can fetch it, and falls back
    // to TLS plus a post-install machine check when it cannot, so an
    // unconditional "checksum-verified" would promise a guarantee that any
    // single failed request downgrades.
    let cost = match std::env::consts::OS {
        "macos" => "two small downloads, about 8 MB total (checksum-verified when the \
                    release manifest is reachable)"
            .to_string(),
        "linux" => "two downloads, about 35 MB total (checksum-verified when the release \
                    manifest is reachable)"
            .to_string(),
        "windows" => "two checksum-pinned Windows archives, about 190 MB total".to_string(),
        _ => "not auto-installable on this OS; see github.com/espressif/qemu/releases".to_string(),
    };
    let manual = "hauksbee install esp-qemu".to_string();

    #[cfg(feature = "qemu")]
    {
        use hauksbee_mcu::qemu::{find_qemu, QemuArch};
        let xtensa = find_qemu(QemuArch::Xtensa);
        let riscv = find_qemu(QemuArch::Riscv32);
        match (&xtensa, &riscv) {
            (Ok(x), Ok(r)) => DepStatus {
                // A local binary: running it sends nothing anywhere.
                sends_data_offhost: None,
                id: "esp-qemu",
                name: "Espressif QEMU",
                present: true,
                path: Some(format!("{}; {}", x.display(), r.display())),
                // The machine-help invocation in the resolver is enough to
                // reject mainline QEMU. A second `--version` subprocess adds
                // no readiness information, so leave cosmetic version blank.
                version: None,
                unlocks,
                installable: false,
                cost,
                manual,
                detail: None,
            },
            _ => {
                // Name which half is missing rather than a blanket "absent":
                // a partial idf_tools install is a real state a user hits.
                let mut parts = Vec::new();
                match &xtensa {
                    Ok(p) => parts.push(format!("qemu-system-xtensa found at {}", p.display())),
                    Err(e) => parts.push(format!(
                        "qemu-system-xtensa: {}",
                        first_line(&e.to_string())
                    )),
                }
                match &riscv {
                    Ok(p) => parts.push(format!("qemu-system-riscv32 found at {}", p.display())),
                    Err(e) => parts.push(format!(
                        "qemu-system-riscv32: {}",
                        first_line(&e.to_string())
                    )),
                }
                DepStatus {
                    // A local binary: running it sends nothing anywhere.
                    sends_data_offhost: None,
                    id: "esp-qemu",
                    name: "Espressif QEMU",
                    present: false,
                    path: None,
                    version: None,
                    unlocks,
                    installable: cfg!(windows)
                        || hauksbee_mcu::qemu::install::host_asset_triple().is_ok(),
                    cost,
                    manual,
                    detail: Some(parts.join("; ")),
                }
            }
        }
    }
    #[cfg(not(feature = "qemu"))]
    {
        DepStatus {
            // A local binary: running it sends nothing anywhere.
            sends_data_offhost: None,
            id: "esp-qemu",
            name: "Espressif QEMU",
            present: false,
            path: None,
            version: None,
            unlocks,
            installable: false,
            cost,
            manual,
            detail: Some(
                "this build of hauksbee was compiled without the `qemu` feature, so it \
                 could not use the Espressif QEMU fork even if installed"
                    .to_string(),
            ),
        }
    }
}

/// Locate ngspice the way the differential harness documents its lookup:
/// `$NGSPICE`, then PATH, then the known per-OS install locations.
fn find_ngspice() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("NGSPICE") {
        let pb = PathBuf::from(&p);
        if pb.is_file() {
            return Some(pb);
        }
    }
    let exe = if cfg!(windows) {
        "ngspice.exe"
    } else {
        "ngspice"
    };
    if let Ok(path) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path) {
            let cand = dir.join(exe);
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    for p in [
        "/opt/homebrew/bin/ngspice",
        "/usr/local/bin/ngspice",
        "/usr/bin/ngspice",
        "/opt/local/bin/ngspice",
        "C:\\Program Files\\ngspice\\bin\\ngspice_con.exe",
        "C:\\Program Files\\ngspice\\bin\\ngspice.exe",
    ] {
        let pb = PathBuf::from(p);
        if pb.is_file() {
            return Some(pb);
        }
    }
    None
}

fn probe_ngspice() -> DepStatus {
    let unlocks = "cross-checking hauksbee's analog solver against ngspice, the SPICE oracle";
    let manual = match std::env::consts::OS {
        "macos" => "brew install ngspice",
        "linux" => "sudo apt install ngspice   # or your distro's package",
        _ => "install ngspice from ngspice.sourceforge.io",
    }
    .to_string();
    match find_ngspice() {
        Some(p) => DepStatus {
            // A local binary: running it sends nothing anywhere.
            sends_data_offhost: None,
            id: "ngspice",
            name: "ngspice",
            present: true,
            version: None,
            path: Some(p.display().to_string()),
            unlocks,
            installable: false,
            cost: "a package-manager install".to_string(),
            manual,
            detail: None,
        },
        None => DepStatus {
            // A local binary: running it sends nothing anywhere.
            sends_data_offhost: None,
            id: "ngspice",
            name: "ngspice",
            present: false,
            path: None,
            version: None,
            unlocks,
            installable: false,
            cost: "a package-manager install".to_string(),
            manual,
            detail: Some(
                "ngspice not found ($NGSPICE, PATH, or a standard install location). \
                 It comes from your system package manager, not from here."
                    .to_string(),
            ),
        },
    }
}

/// Locate a usable `kicad-cli`. Delegates to the DRC oracle's own finder so
/// the dependency panel and the check that consumes kicad-cli can never
/// disagree about whether it is installed or which one won.
fn find_kicad_cli() -> Option<(String, String)> {
    crate::reports::drc::find_kicad_cli()
}

fn probe_kicad_cli() -> DepStatus {
    let unlocks = "cross-checking copper DRC findings against KiCad's own DRC (the layout \
                   oracle), and SVG/Gerber export of re-laid-out boards";
    let manual = match std::env::consts::OS {
        "macos" => "brew install --cask kicad   # or download from kicad.org",
        "linux" => "sudo apt install kicad   # or download from kicad.org",
        _ => "download KiCad from kicad.org",
    }
    .to_string();
    let cost = "part of the full KiCad suite; the KiCad download alone is over 1 GB".to_string();
    match find_kicad_cli() {
        Some((path, ver)) => DepStatus {
            // A local binary: running it sends nothing anywhere.
            sends_data_offhost: None,
            id: "kicad-cli",
            name: "kicad-cli",
            present: true,
            path: Some(path),
            version: Some(ver),
            unlocks,
            installable: false,
            cost,
            manual,
            detail: None,
        },
        None => DepStatus {
            // A local binary: running it sends nothing anywhere.
            sends_data_offhost: None,
            id: "kicad-cli",
            name: "kicad-cli",
            present: false,
            path: None,
            version: None,
            unlocks,
            installable: false,
            cost,
            manual,
            detail: Some(
                "kicad-cli not found (PATH, /Applications, or a standard install \
                 location). KiCad is a desktop application install, not something this \
                 server should download for you."
                    .to_string(),
            ),
        },
    }
}

fn probe_avr() -> DepStatus {
    let unlocks = "ATmega and ATtiny firmware co-simulation";
    // libsimavr is linked INTO the binary at build time (GPL-3.0, so it is
    // system-linked, never vendored). There is nothing to install at runtime:
    // it is present-or-not per build, which is why this row never gets an
    // install button.
    #[cfg(feature = "avr")]
    {
        DepStatus {
            // A local binary: running it sends nothing anywhere.
            sends_data_offhost: None,
            id: "avr",
            name: "AVR (simavr)",
            present: true,
            path: None,
            version: None,
            unlocks,
            installable: false,
            cost: "linked into this binary at build time".to_string(),
            manual: String::new(),
            detail: Some("simavr is linked into this binary; no separate install".to_string()),
        }
    }
    #[cfg(not(feature = "avr"))]
    {
        DepStatus {
            // A local binary: running it sends nothing anywhere.
            sends_data_offhost: None,
            id: "avr",
            name: "AVR (simavr)",
            present: false,
            path: None,
            version: None,
            unlocks,
            installable: false,
            cost: "linked at build time only; a runtime install cannot add it".to_string(),
            manual: "scripts/install-sims.sh --avr, then rebuild hauksbee with the `avr` \
                     feature"
                .to_string(),
            detail: Some(
                "this build of hauksbee was compiled without libsimavr. It is linked \
                 in-process at build time, so installing it now cannot help this binary; \
                 rebuild after installing simavr."
                    .to_string(),
            ),
        }
    }
}

#[cfg(feature = "qemu")]
fn first_line(msg: &str) -> String {
    msg.lines().next().unwrap_or("").to_string()
}

// ── install side ─────────────────────────────────────────────────────────────

/// The installer script and the helper it sources, embedded at build time.
///
/// Why: the shipped .app carries no `scripts/` directory, so on a stranger's
/// machine `find_install_sims_script` finds nothing and the Environment page
/// could offer no Renode Install button at all (the cold-install audit's
/// defect 3). Embedding the script keeps ONE maintained installer
/// implementation while making it available from any binary, bundle or bare;
/// an on-disk copy still wins so a user-patched script is honored.
///
/// The include paths point INSIDE this crate (`assets/scripts/`) because
/// `cargo package` ships only files under the crate directory; a
/// `../../../scripts/` include compiles from a git checkout and from nowhere
/// else. The AUTHORITATIVE copies stay at repo-root `scripts/` (users and CI
/// run them from there); `tests/packaged_asset_sync.rs` fails the build when
/// the mirror drifts, and `scripts/sync-crate-assets.sh` refreshes it.
#[cfg(not(windows))]
const INSTALL_SIMS_SH: &str = include_str!("../assets/scripts/install-sims.sh");
#[cfg(not(windows))]
const COMMON_SH: &str = include_str!("../assets/scripts/common.sh");
#[cfg(not(windows))]
const REQUIRED_SIMULATOR_VERSIONS: &str =
    include_str!("../assets/scripts/required-simulator-versions.env");
#[cfg(not(windows))]
const RENODE_CHECKSUMS: &str = include_str!("../assets/scripts/renode-checksums.txt");
#[cfg(not(windows))]
const ESPRESSIF_QEMU_CHECKSUMS: &str =
    include_str!("../assets/scripts/espressif-qemu-checksums.txt");
#[cfg(not(windows))]
const SIMULATOR_PROVENANCE_PY: &str = include_str!("../assets/scripts/simulator-provenance.py");
#[cfg(not(windows))]
const SIMAVR_PAYLOAD_PROVENANCE_SH: &str =
    include_str!("../assets/scripts/simavr-payload-provenance.sh");
#[cfg(windows)]
const INSTALL_SIMS_WINDOWS_PS1: &str = include_str!("../assets/scripts/install-sims-windows.ps1");

struct MaterializedInstaller {
    path: PathBuf,
    _owned_dir: Option<tempfile::TempDir>,
}

impl MaterializedInstaller {
    fn path(&self) -> &std::path::Path {
        &self.path
    }
}

/// A runnable `install-sims.sh` path: the on-disk copy when one exists, else
/// the embedded copy (plus the `common.sh` it sources) written to a temp dir.
#[cfg(not(windows))]
fn materialize_install_sims_script() -> Result<MaterializedInstaller, String> {
    if let Some(p) = find_install_sims_script() {
        return Ok(MaterializedInstaller {
            path: p,
            _owned_dir: None,
        });
    }
    let owned_dir = tempfile::Builder::new()
        .prefix("hauksbee-install-sims-")
        .tempdir()
        .map_err(|e| format!("could not stage the bundled installer script: {e}"))?;
    let dir = owned_dir.path();
    let script = dir.join("install-sims.sh");
    let files = [
        ("install-sims.sh", INSTALL_SIMS_SH),
        ("common.sh", COMMON_SH),
        (
            "required-simulator-versions.env",
            REQUIRED_SIMULATOR_VERSIONS,
        ),
        ("renode-checksums.txt", RENODE_CHECKSUMS),
        ("espressif-qemu-checksums.txt", ESPRESSIF_QEMU_CHECKSUMS),
        ("simulator-provenance.py", SIMULATOR_PROVENANCE_PY),
        ("simavr-payload-provenance.sh", SIMAVR_PAYLOAD_PROVENANCE_SH),
    ];
    for (name, contents) in files {
        if let Err(error) = std::fs::write(dir.join(name), contents) {
            return Err(format!(
                "could not write bundled installer sidecar {name}: {error}"
            ));
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for f in [&script, &dir.join("common.sh")] {
            let _ = std::fs::set_permissions(f, std::fs::Permissions::from_mode(0o755));
        }
    }
    Ok(MaterializedInstaller {
        path: script,
        _owned_dir: Some(owned_dir),
    })
}

#[cfg(windows)]
fn materialize_install_sims_windows_script() -> Result<MaterializedInstaller, String> {
    if let Some(p) = find_named_installer("install-sims-windows.ps1") {
        return Ok(MaterializedInstaller {
            path: p,
            _owned_dir: None,
        });
    }
    let owned_dir = tempfile::Builder::new()
        .prefix("hauksbee-install-sims-windows-")
        .tempdir()
        .map_err(|e| format!("could not stage the bundled Windows installer: {e}"))?;
    let script = owned_dir.path().join("install-sims-windows.ps1");
    std::fs::write(&script, INSTALL_SIMS_WINDOWS_PS1)
        .map_err(|e| format!("could not write the bundled Windows installer: {e}"))?;
    Ok(MaterializedInstaller {
        path: script,
        _owned_dir: Some(owned_dir),
    })
}

/// Locate `scripts/install-sims.sh`: env override first (tests), then walking
/// up from the executable (release bundles ship `scripts/` next to `bin/`),
/// then from the build-time checkout (source runs). It deliberately does not
/// execute a same-named script merely because the process was launched from an
/// arbitrary consumer directory.
#[cfg(not(windows))]
fn find_install_sims_script() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("HAUKSBEE_INSTALL_SIMS") {
        let pb = PathBuf::from(p);
        return pb.is_file().then_some(pb);
    }
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            roots.push(dir.to_path_buf());
        }
    }
    // The build-machine checkout (same expression web_dist uses); the is_file
    // check below makes this a no-op on any other machine.
    roots.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."));
    for root in roots {
        let mut cur: Option<&std::path::Path> = Some(root.as_path());
        for _ in 0..6 {
            let Some(dir) = cur else { break };
            let cand = dir.join("scripts/install-sims.sh");
            if cand.is_file() {
                return Some(cand);
            }
            cur = dir.parent();
        }
    }
    None
}

#[cfg(windows)]
fn find_named_installer(name: &str) -> Option<PathBuf> {
    if let Ok(path) = std::env::var("HAUKSBEE_INSTALL_SIMS") {
        let path = PathBuf::from(path);
        return path.is_file().then_some(path);
    }
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            roots.push(dir.to_path_buf());
        }
    }
    roots.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."));
    for root in roots {
        let mut cur: Option<&std::path::Path> = Some(root.as_path());
        for _ in 0..6 {
            let Some(dir) = cur else { break };
            let candidate = dir.join("scripts").join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
            cur = dir.parent();
        }
    }
    None
}

/// Hard ceiling on one web-triggered install. Generous: the Renode download is
/// ~80 MB and a slow link is normal; nothing legitimate takes longer than this.
const INSTALL_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// Cap on the bytes of child output relayed to the browser; the tail is still
/// kept for the final error message after the cap trips.
const MAX_RELAY_BYTES: usize = 256 * 1024;

/// How many trailing output lines a failure message carries.
const ERROR_TAIL_LINES: usize = 12;

/// True while an install is running. One at a time: each install writes to
/// shared locations (`~/.hauksbee-qemu-esp`, `~/renode-portable`), so two
/// racing installers could interleave half-written trees.
static INSTALL_ACTIVE: AtomicBool = AtomicBool::new(false);

/// RAII slot for the one-install-at-a-time budget (the `WebCheckSlot` pattern):
/// `Drop` releases on every exit path, including panic unwinds.
struct InstallSlot;

impl InstallSlot {
    fn acquire() -> Option<InstallSlot> {
        if INSTALL_ACTIVE.swap(true, Ordering::AcqRel) {
            None
        } else {
            Some(InstallSlot)
        }
    }
}

impl Drop for InstallSlot {
    fn drop(&mut self) {
        INSTALL_ACTIVE.store(false, Ordering::Release);
    }
}

/// Run the install for one dependency id, streaming human-readable progress
/// lines through `progress`. Returns `Err(message)` with the child's real
/// output tail on any failure. Refuses (fast) when an install is already
/// running, when the id is unknown, and when the id is not installable from
/// here (the error then carries the manual command).
pub fn install_dep(id: &str, progress: &mut dyn FnMut(&str)) -> Result<(), String> {
    let _slot = InstallSlot::acquire().ok_or_else(|| {
        "another install is already running; wait for it to finish and try again".to_string()
    })?;

    let result = match id {
        "esp-qemu" => install_esp_qemu(progress),
        "renode" => install_renode(progress),
        "ngspice" => Err(format!(
            "ngspice comes from your system package manager, not from here. Run: {}",
            probe_ngspice().manual
        )),
        "kicad-cli" => Err(format!(
            "KiCad is a desktop application install (over 1 GB), not something this \
             server downloads for you. Run: {}",
            probe_kicad_cli().manual
        )),
        "avr" => Err(
            "libsimavr is linked into the hauksbee binary at build time; it cannot be \
             added to a running binary. Install it (scripts/install-sims.sh --avr) and \
             rebuild with the `avr` feature."
                .to_string(),
        ),
        other => Err(format!("unknown dependency id '{other}'")),
    };
    if result.is_ok() {
        invalidate_deps_cache();
    }
    result
}

/// Espressif QEMU: use the native checksum-pinned PowerShell installer on
/// Windows; elsewhere shell this binary's Rust installer. Both routes run as
/// structurally owned child trees so downloads cannot survive a timeout.
pub(crate) fn install_esp_qemu(progress: &mut dyn FnMut(&str)) -> Result<(), String> {
    #[cfg(not(feature = "qemu"))]
    {
        let _ = progress;
        Err(
            "this build of hauksbee was compiled without the `qemu` feature; rebuild \
             with it before installing the Espressif QEMU fork"
                .to_string(),
        )
    }
    #[cfg(feature = "qemu")]
    {
        #[cfg(windows)]
        {
            let script = materialize_install_sims_windows_script()?;
            progress("installing checksum-pinned Espressif QEMU for Windows (about 190 MB) ...");
            let mut cmd = Command::new("powershell.exe");
            cmd.args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
                .arg(script.path())
                .arg("-QemuOnly");
            return run_streaming(cmd, progress, INSTALL_TIMEOUT);
        }
        #[cfg(not(windows))]
        {
            if let Err(e) = hauksbee_mcu::qemu::install::host_asset_triple() {
                return Err(e.to_string());
            }
            let exe = std::env::current_exe()
                .map_err(|e| format!("could not locate the hauksbee binary: {e}"))?;
            progress("installing the Espressif QEMU fork (ESP32 family) ...");
            let mut cmd = Command::new(exe);
            cmd.args(["install", "esp-qemu", "--yes"]);
            run_streaming(cmd, progress, INSTALL_TIMEOUT)
        }
    }
}

/// Renode: shell the platform's checksum-pinned installer. The script comes
/// from disk when present, else from the copy embedded in this binary.
pub(crate) fn install_renode(progress: &mut dyn FnMut(&str)) -> Result<(), String> {
    #[cfg(windows)]
    {
        let script = materialize_install_sims_windows_script()?;
        progress("installing checksum-pinned Renode for Windows (about 120 MB) ...");
        let mut cmd = Command::new("powershell.exe");
        cmd.args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
            .arg(script.path())
            .arg("-RenodeOnly");
        run_streaming(cmd, progress, INSTALL_TIMEOUT)
    }
    #[cfg(not(windows))]
    {
        if !host_is_installable_os() {
            return Err(format!(
                "Renode auto-install is unavailable on {}; install it manually from \
                 github.com/renode/renode/releases",
                std::env::consts::OS
            ));
        }
        let script = materialize_install_sims_script()?;
        progress("installing Renode (about an 80 MB download) ...");
        let mut cmd = Command::new("bash");
        cmd.arg(script.path()).arg("--renode-only");
        run_streaming(cmd, progress, INSTALL_TIMEOUT)
    }
}

/// Spawn `cmd` in its own process group, relay its stdout+stderr line by line
/// into `progress` (capped), and enforce `timeout` by killing the whole group
/// (the child shells `curl` and friends; killing only the direct child would
/// orphan an in-flight download). On a non-zero exit or timeout the error
/// carries the last [`ERROR_TAIL_LINES`] lines of real output.
fn run_streaming(
    mut cmd: Command,
    progress: &mut dyn FnMut(&str),
    timeout: Duration,
) -> Result<(), String> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // A fresh process group so a timeout kill reaches grandchildren too.
        cmd.process_group(0);
    }
    let (mut child, tree_guard) = hauksbee_mcu::children::spawn_owned(&mut cmd)
        .map_err(|e| format!("could not start the installer: {e}"))?;

    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let mut readers = Vec::new();
    for pipe in [
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn std::io::Read + Send>),
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn std::io::Read + Send>),
    ]
    .into_iter()
    .flatten()
    {
        let tx = tx.clone();
        readers.push(std::thread::spawn(move || {
            use std::io::BufRead;
            let reader = std::io::BufReader::new(pipe);
            for line in reader.lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        }));
    }
    drop(tx); // the channel closes when both readers finish

    let started = Instant::now();
    let mut tail: VecDeque<String> = VecDeque::new();
    let mut relayed = 0usize;
    let mut capped = false;
    let mut disconnected = false;
    let mut status: Option<std::process::ExitStatus> = None;

    loop {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(line) => {
                tail.push_back(line.clone());
                if tail.len() > ERROR_TAIL_LINES {
                    tail.pop_front();
                }
                if relayed < MAX_RELAY_BYTES {
                    relayed += line.len();
                    progress(&line);
                } else if !capped {
                    capped = true;
                    progress("[output capped; the tail will be reported at the end]");
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => disconnected = true,
        }
        if status.is_none() {
            if let Ok(Some(st)) = child.try_wait() {
                status = Some(st);
            }
        }
        if disconnected && status.is_some() {
            break;
        }
        if started.elapsed() > timeout {
            kill_process_group(&mut child, &tree_guard);
            for r in readers {
                let _ = r.join();
            }
            return Err(format!(
                "the install exceeded {} minutes and was stopped. Last output:\n{}",
                timeout.as_secs() / 60,
                tail_text(&tail)
            ));
        }
    }
    for r in readers {
        let _ = r.join();
    }
    let status = status.expect("loop only exits with a status");
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "the installer exited with {}. Last output:\n{}",
            status
                .code()
                .map(|c| format!("code {c}"))
                .unwrap_or_else(|| "a signal".to_string()),
            tail_text(&tail)
        ))
    }
}

fn tail_text(tail: &VecDeque<String>) -> String {
    if tail.is_empty() {
        "(the installer produced no output)".to_string()
    } else {
        tail.iter().cloned().collect::<Vec<_>>().join("\n")
    }
}

/// Kill the child's whole process tree, falling back to the direct child.
/// On unix `kill -- -<pid>` addresses the group created by `process_group(0)`;
/// on Windows the retained Job Object terminates the exact owned tree, never a
/// potentially recycled numeric PID.
fn kill_process_group(
    child: &mut std::process::Child,
    _tree_guard: &hauksbee_mcu::children::ProcessTreeGuard,
) {
    #[cfg(unix)]
    {
        let _ = Command::new("kill")
            .args(["-9", "--", &format!("-{}", child.id())])
            .output();
    }
    #[cfg(windows)]
    {
        let _ = _tree_guard.terminate();
    }
    let _ = child.kill();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes tests that touch the global INSTALL_ACTIVE flag.
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn serial_guard() -> std::sync::MutexGuard<'static, ()> {
        SERIAL.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn shell_command(script: &str) -> Command {
        #[cfg(windows)]
        {
            let mut command = Command::new("powershell.exe");
            command.args(["-NoProfile", "-Command", script]);
            command
        }
        #[cfg(not(windows))]
        {
            let mut command = Command::new("sh");
            command.args(["-c", script]);
            command
        }
    }

    #[test]
    fn deps_json_reports_every_dep_with_the_contract_fields() {
        let json = deps_json();
        let v: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        let deps = v["deps"].as_array().expect("deps array");
        let ids: Vec<&str> = deps.iter().filter_map(|d| d["id"].as_str()).collect();
        assert_eq!(
            ids,
            [
                "renode",
                "esp-qemu",
                "ngspice",
                "kicad-cli",
                "avr",
                "codex",
                "claude-code"
            ],
            "the panel renders this list in order, so adding a probe is a \
             deliberate change to what a user sees, not an incidental one"
        );
        for d in deps {
            assert!(d["present"].is_boolean(), "present is a bool: {d}");
            assert!(d["installable"].is_boolean(), "installable is a bool: {d}");
            assert!(d["unlocks"].as_str().is_some_and(|s| !s.is_empty()));
            // Honesty: an absent dep explains itself; a present one has a
            // location (except AVR, which is linked into the binary).
            if d["present"] == false {
                assert!(
                    d["detail"].as_str().is_some_and(|s| !s.is_empty()),
                    "absent dep must say why: {d}"
                );
            } else if d["id"] != "avr" {
                assert!(
                    d["path"].as_str().is_some_and(|s| !s.is_empty()),
                    "present dep must show its resolved path: {d}"
                );
            }
            // Only a dep that sends data off the machine carries the notice,
            // and it must carry it whether or not it is installed: a user
            // deciding WHETHER to install needs to know before they do.
            let offhost = d["sends_data_offhost"].as_str();
            if d["id"] == "codex" {
                assert!(
                    offhost.is_some_and(|s| s.contains("OpenAI")),
                    "codex must state where the data goes: {d}"
                );
            } else if d["id"] == "claude-code" {
                assert!(
                    offhost.is_some_and(|s| s.contains("Anthropic")),
                    "claude-code must state where the data goes: {d}"
                );
            } else {
                assert!(
                    offhost.is_none(),
                    "a local binary must not claim to send anything: {d}"
                );
            }
        }
    }

    /// A shim standing in for a codex that answers a given way. Real codex
    /// cannot be logged out on the machine running this, and the logged-out
    /// branch is the one that matters, so it is the one worth a fixture.
    #[cfg(unix)]
    fn codex_shim(dir: &std::path::Path, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join("codex");
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    #[cfg(unix)]
    struct PathGuard(Option<std::ffi::OsString>);

    #[cfg(unix)]
    impl Drop for PathGuard {
        fn drop(&mut self) {
            if let Some(path) = self.0.take() {
                std::env::set_var("PATH", path);
            } else {
                std::env::remove_var("PATH");
            }
        }
    }

    /// The Environment page must not execute either LLM CLI. A version/login
    /// command is both slower and a surprising side effect for a read-only
    /// status request; readiness owns the one Codex auth check instead.
    #[cfg(unix)]
    #[test]
    fn fast_extractor_rows_do_not_spawn_cli_status_commands() {
        let _serial = serial_guard();
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("called");
        let marker_literal = marker.to_string_lossy().replace('\'', "'\\''");
        let script = format!("#!/bin/sh\ntouch '{marker_literal}'\n");
        use std::os::unix::fs::PermissionsExt;
        for name in ["codex", "claude"] {
            let path = dir.path().join(name);
            std::fs::write(&path, &script).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let _path_guard = PathGuard(std::env::var_os("PATH"));
        std::env::set_var("PATH", dir.path());
        let started = Instant::now();
        let codex = probe_codex();
        let claude = probe_claude_code();
        let elapsed = started.elapsed();

        assert!(codex.present);
        assert!(claude.present);
        assert!(codex.version.is_none());
        assert!(claude.version.is_none());
        assert!(codex
            .detail
            .as_deref()
            .is_some_and(|d| d.contains("only when")));
        assert!(claude
            .detail
            .as_deref()
            .is_some_and(|d| d.contains("only when")));
        assert!(
            !marker.exists(),
            "status probes must not execute either CLI"
        );
        assert!(
            elapsed < Duration::from_secs(1),
            "discovery-only LLM probes took {elapsed:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_unauthenticated_codex_is_not_reported_as_usable() {
        // Installed is not usable. Reporting presence here would send someone
        // into an extraction that fails after they had already read the privacy
        // notice and said yes, which is the worst moment to find out.
        let dir = tempfile::tempdir().unwrap();
        let shim = codex_shim(
            dir.path(),
            r#"if [ "$1" = "login" ]; then echo "Not logged in" >&2; exit 1; fi
echo "codex-cli 0.0.0""#,
        );
        assert!(
            matches!(codex_login_state(&shim), CodexLogin::NotLoggedIn),
            "a codex that refuses `login status` is not signed in"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_signed_in_codex_reports_how_it_is_authenticated() {
        // codex writes this to STDERR, which is why the probe reads both
        // streams: reading stdout alone got the state right from the exit code
        // and silently lost the description.
        let dir = tempfile::tempdir().unwrap();
        let shim = codex_shim(
            dir.path(),
            r#"if [ "$1" = "login" ]; then echo "Logged in using ChatGPT" >&2; exit 0; fi
echo "codex-cli 0.0.0""#,
        );
        match codex_login_state(&shim) {
            CodexLogin::LoggedIn(how) => assert_eq!(
                how.as_deref(),
                Some("Logged in using ChatGPT"),
                "the how must survive, and it arrives on stderr"
            ),
            CodexLogin::NotLoggedIn => panic!("a codex exiting 0 is signed in"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_codex_we_cannot_run_counts_as_not_signed_in() {
        // Failing closed: if we cannot tell, the reading that does not walk the
        // user into a broken extraction is the right one.
        assert!(matches!(
            codex_login_state(std::path::Path::new("/no/such/codex")),
            CodexLogin::NotLoggedIn
        ));
    }

    /// AVR is linked at build time: it must never be offered as installable,
    /// whatever the build shape.
    #[test]
    fn avr_is_never_installable() {
        let _serial = serial_guard();
        let avr = probe_all().into_iter().find(|d| d.id == "avr").unwrap();
        assert!(!avr.installable);
        let err = install_dep("avr", &mut |_| {}).expect_err("avr install must refuse");
        assert!(
            err.contains("build time"),
            "explains the build-time link: {err}"
        );
    }

    #[test]
    fn unknown_and_manual_only_ids_are_refused_with_guidance() {
        let _serial = serial_guard();
        let err = install_dep("nonsense", &mut |_| {}).expect_err("unknown id");
        assert!(err.contains("unknown dependency id"), "{err}");
        for id in ["ngspice", "kicad-cli"] {
            let err = install_dep(id, &mut |_| {}).expect_err("manual-only id");
            assert!(
                err.contains("brew") || err.contains("apt") || err.contains("kicad.org"),
                "{id} refusal must carry the manual command: {err}"
            );
        }
    }

    #[test]
    fn one_install_at_a_time() {
        let _serial = serial_guard();
        let slot = InstallSlot::acquire().expect("slot free at rest");
        let err = install_dep("esp-qemu", &mut |_| {}).expect_err("second install refused");
        assert!(err.contains("already running"), "{err}");
        drop(slot);
        assert!(InstallSlot::acquire().is_some(), "slot released on drop");
    }

    /// A failing child's real output reaches the error, not just an exit code
    /// (the cold-drive session that lost 15 minutes to a bare "exit status: 1"
    /// is the regression here).
    #[test]
    fn failure_carries_the_output_tail() {
        let cmd = shell_command("echo starting; echo 'the disk is full' >&2; exit 3");
        let mut lines = Vec::new();
        let err = run_streaming(
            cmd,
            &mut |l| lines.push(l.to_string()),
            Duration::from_secs(30),
        )
        .expect_err("exit 3 is a failure");
        assert!(err.contains("code 3"), "names the exit code: {err}");
        assert!(
            err.contains("the disk is full"),
            "carries the stderr tail: {err}"
        );
        assert!(
            lines.iter().any(|l| l.contains("starting")),
            "stdout was streamed live: {lines:?}"
        );
    }

    #[test]
    fn timeout_kills_the_child_and_says_so() {
        #[cfg(windows)]
        let script = "echo begun; Start-Sleep -Seconds 30; echo never";
        #[cfg(not(windows))]
        let script = "echo begun; sleep 30; echo never";
        let cmd = shell_command(script);
        let mut lines = Vec::new();
        let started = Instant::now();
        let err = run_streaming(
            cmd,
            &mut |l| lines.push(l.to_string()),
            Duration::from_secs(1),
        )
        .expect_err("must time out");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "did not wait for the sleep"
        );
        assert!(err.contains("was stopped"), "names the timeout: {err}");
        assert!(
            !lines.iter().any(|l| l.contains("never")),
            "child was killed"
        );
    }

    #[cfg(windows)]
    #[test]
    fn timeout_kills_a_real_installer_grandchild_through_the_owned_job() {
        use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };

        let scratch = tempfile::tempdir().expect("scratch directory");
        let marker = scratch.path().join("grandchild.pid");
        let escaped = marker.display().to_string().replace('\'', "''");
        let script = format!(
            "$p=Start-Process powershell -ArgumentList '-NoProfile','-Command','Start-Sleep 300' -PassThru; Set-Content -LiteralPath '{escaped}' -Value $p.Id; Wait-Process -Id $p.Id"
        );
        let err = run_streaming(shell_command(&script), &mut |_| {}, Duration::from_secs(1))
            .expect_err("owned installer tree times out");
        assert!(err.contains("was stopped"), "{err}");
        let pid: u32 = std::fs::read_to_string(&marker)
            .expect("grandchild marker")
            .trim()
            .parse()
            .expect("numeric grandchild pid");
        for _ in 0..100 {
            // SAFETY: the queried handle is checked and closed on every path.
            let live = unsafe {
                let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
                if handle.is_null() {
                    false
                } else {
                    let mut code = 0;
                    let live =
                        GetExitCodeProcess(handle, &mut code) != 0 && code == STILL_ACTIVE as u32;
                    CloseHandle(handle);
                    live
                }
            };
            if !live {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("installer grandchild {pid} survived Job-backed timeout");
    }

    #[test]
    fn success_streams_and_returns_ok() {
        let cmd = shell_command("echo one; echo two >&2; exit 0");
        let mut lines = Vec::new();
        run_streaming(
            cmd,
            &mut |l| lines.push(l.to_string()),
            Duration::from_secs(30),
        )
        .expect("exit 0 succeeds");
        assert!(
            lines.iter().any(|l| l == "one"),
            "stdout relayed: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l == "two"),
            "stderr relayed: {lines:?}"
        );
    }

    /// The renode installer resolves scripts/install-sims.sh through the env
    /// override, so a broken override must not silently fall through to a
    /// real installer.
    #[test]
    fn renode_install_respects_the_script_override() {
        let _serial = serial_guard();
        // A fake "installer" that succeeds instantly, proving the plumbing
        // (script resolution -> bash child -> streamed lines -> Ok).
        let dir = std::env::temp_dir().join(format!("hauksbee-deps-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        #[cfg(windows)]
        let script = dir.join("install-sims-windows.ps1");
        #[cfg(not(windows))]
        let script = dir.join("install-sims.sh");
        #[cfg(windows)]
        let script_body = "Write-Output \"fake renode install ran $args\"\nexit 0\n";
        #[cfg(not(windows))]
        let script_body = "#!/bin/sh\necho fake renode install ran \"$@\"\nexit 0\n";
        std::fs::write(&script, script_body).unwrap();
        std::env::set_var("HAUKSBEE_INSTALL_SIMS", &script);
        let mut lines = Vec::new();
        let res = install_dep("renode", &mut |l| lines.push(l.to_string()));
        std::env::remove_var("HAUKSBEE_INSTALL_SIMS");
        let _ = std::fs::remove_dir_all(&dir);
        if matches!(std::env::consts::OS, "macos" | "linux" | "windows") {
            res.expect("fake installer exits 0");
            #[cfg(windows)]
            let expected = "fake renode install ran -RenodeOnly";
            #[cfg(not(windows))]
            let expected = "fake renode install ran --renode-only";
            assert!(
                lines.iter().any(|line| line.contains(expected)),
                "the script ran with the platform-specific Renode-only flag: {lines:?}"
            );
        }
    }
}
