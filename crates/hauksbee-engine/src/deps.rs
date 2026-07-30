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
//! The install side shells KILLABLE children and streams their output:
//! `hauksbee install esp-qemu --yes` (this very binary's own Rust installer,
//! checksum-verified, see `hauksbee_mcu::qemu::install`) for the Espressif
//! QEMU fork, and `scripts/install-sims.sh --renode-only` for Renode (the
//! script is shipped in release bundles next to the binary). One install runs
//! at a time (RAII slot, same pattern as `webcheck::WebCheckSlot`), a hard
//! timeout kills the whole child process group, and a failure surfaces the
//! child's actual output tail, never a bare exit code.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// One probed dependency, serialized verbatim as the `/api/deps` JSON.
#[derive(serde::Serialize)]
pub struct DepStatus {
    /// Stable machine id (`renode`, `esp-qemu`, `ngspice`, `kicad-cli`, `avr`).
    pub id: &'static str,
    /// Human display name.
    pub name: &'static str,
    /// True only when the engine's own resolver accepted it.
    pub present: bool,
    /// Resolved binary path(s) when present.
    pub path: Option<String>,
    /// Version string when present and cheap to read (qemu / ngspice /
    /// kicad-cli answer `--version` instantly; Renode's .NET startup takes
    /// seconds, so its version is deliberately not probed here).
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
/// this is a plain `which` that could disagree with a real run.
pub fn probe_all() -> Vec<DepStatus> {
    vec![
        probe_renode(),
        probe_esp_qemu(),
        probe_ngspice(),
        probe_kicad_cli(),
        probe_avr(),
        probe_codex(),
    ]
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
    let cost = "an OpenAI account and the codex CLI".to_string();
    let manual = "npm install -g @openai/codex   # then: codex login".to_string();
    let privacy = "Using this sends the datasheet's text to OpenAI. Nothing is sent unless \
                   you ask for an extraction, and hauksbee never runs it on its own.";
    match which_codex() {
        Some(p) => DepStatus {
            id: "codex",
            name: "Codex (datasheet extraction)",
            present: true,
            version: codex_version(&p),
            path: Some(p.display().to_string()),
            unlocks,
            // Deliberately never auto-installable: an account and a login are
            // the user's to give, and a one-click button for a service that
            // takes their data would be the wrong shape whatever it said.
            installable: false,
            cost,
            manual,
            detail: None,
            sends_data_offhost: Some(privacy),
        },
        None => DepStatus {
            id: "codex",
            name: "Codex (datasheet extraction)",
            present: false,
            path: None,
            version: None,
            unlocks,
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
        },
    }
}

fn which_codex() -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        if cfg!(windows) {
            for ext in ["codex.exe", "codex.cmd"] {
                let c = dir.join(ext);
                if c.is_file() {
                    return Some(c);
                }
            }
        }
        let c = dir.join("codex");
        if c.is_file() {
            return Some(c);
        }
    }
    None
}

fn codex_version(bin: &std::path::Path) -> Option<String> {
    let out = std::process::Command::new(bin)
        .arg("--version")
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    s.lines()
        .next()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
}

/// The `/api/deps` response body: `{"deps":[...]}`.
pub fn deps_json() -> String {
    serde_json::to_string(&serde_json::json!({ "deps": probe_all() }))
        .unwrap_or_else(|_| "{\"deps\":[]}".to_string())
}

// ── individual probes ────────────────────────────────────────────────────────

fn host_is_installable_os() -> bool {
    matches!(std::env::consts::OS, "macos" | "linux")
}

fn probe_renode() -> DepStatus {
    let unlocks = "STM32, nRF52 and RISC-V firmware co-simulation";
    // Real numbers from the renode/renode release assets (v1.16.x): the
    // portable download is 75-90 MB, and the unpacked install is a few
    // hundred MB on disk.
    let cost = if host_is_installable_os() {
        "about an 80 MB download, a few hundred MB unpacked (macOS and Linux; \
         Windows needs a manual install)"
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
    let cost = match std::env::consts::OS {
        "macos" => "two small downloads, about 8 MB total (checksum-verified)".to_string(),
        "linux" => "two downloads, about 35 MB total (checksum-verified)".to_string(),
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
                version: qemu_version(x),
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
                    installable: hauksbee_mcu::qemu::install::host_asset_triple().is_ok(),
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
            version: ngspice_version(&p),
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

/// The `ngspice-NN` token from `ngspice --version` output, if it answers.
fn ngspice_version(bin: &PathBuf) -> Option<String> {
    let out = Command::new(bin).arg("--version").output().ok()?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    for line in text.lines() {
        if let Some(pos) = line.find("ngspice-") {
            let tok: String = line[pos..]
                .chars()
                .take_while(|c| !c.is_whitespace() && *c != ':')
                .collect();
            return Some(tok);
        }
    }
    None
}

/// First line of `<qemu binary> --version`, e.g.
/// `QEMU emulator version 9.2.2 ...`.
#[cfg(feature = "qemu")]
fn qemu_version(bin: &std::path::Path) -> Option<String> {
    let out = Command::new(bin).arg("--version").output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines().next().map(|l| l.trim().to_string())
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
const INSTALL_SIMS_SH: &str = include_str!("../../../scripts/install-sims.sh");
const COMMON_SH: &str = include_str!("../../../scripts/common.sh");

/// A runnable `install-sims.sh` path: the on-disk copy when one exists, else
/// the embedded copy (plus the `common.sh` it sources) written to a temp dir.
fn materialize_install_sims_script() -> Result<PathBuf, String> {
    if let Some(p) = find_install_sims_script() {
        return Ok(p);
    }
    let dir = std::env::temp_dir().join(format!("hauksbee-install-sims-{}", std::process::id()));
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("could not stage the bundled installer script: {e}"))?;
    let script = dir.join("install-sims.sh");
    std::fs::write(&script, INSTALL_SIMS_SH)
        .and_then(|_| std::fs::write(dir.join("common.sh"), COMMON_SH))
        .map_err(|e| format!("could not write the bundled installer script: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for f in [&script, &dir.join("common.sh")] {
            let _ = std::fs::set_permissions(f, std::fs::Permissions::from_mode(0o755));
        }
    }
    Ok(script)
}

/// Locate `scripts/install-sims.sh`: env override first (tests), then walking
/// up from the executable (release bundles ship `scripts/` next to `bin/`),
/// then from the current directory and the build-time checkout (source runs).
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
    if let Ok(cwd) = std::env::current_dir() {
        roots.push(cwd);
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

    match id {
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
    }
}

/// Espressif QEMU: shell this very binary's `install esp-qemu --yes`. That
/// reuses the checksum-verifying Rust installer end-to-end AND gives us a
/// killable child (the in-process call could not be interrupted on timeout).
fn install_esp_qemu(progress: &mut dyn FnMut(&str)) -> Result<(), String> {
    #[cfg(not(feature = "qemu"))]
    {
        let _ = progress;
        return Err(
            "this build of hauksbee was compiled without the `qemu` feature; rebuild \
             with it before installing the Espressif QEMU fork"
                .to_string(),
        );
    }
    #[cfg(feature = "qemu")]
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

/// Renode: shell `install-sims.sh --renode-only` (the per-backend installer
/// this repo already maintains). The script comes from disk when present,
/// else from the copy embedded in this binary, so the shipped .app (which
/// carries no scripts/) installs Renode exactly like a checkout does. There
/// is no Rust-side Renode installer to prefer.
pub(crate) fn install_renode(progress: &mut dyn FnMut(&str)) -> Result<(), String> {
    if !host_is_installable_os() {
        return Err(format!(
            "Renode auto-install supports macOS and Linux only (this is {}); install \
             it manually from github.com/renode/renode/releases",
            std::env::consts::OS
        ));
    }
    let script = materialize_install_sims_script()?;
    progress("installing Renode (about an 80 MB download) ...");
    let mut cmd = Command::new("bash");
    cmd.arg(&script).arg("--renode-only");
    run_streaming(cmd, progress, INSTALL_TIMEOUT)
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
    let mut child = cmd
        .spawn()
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
            kill_process_group(&mut child);
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
/// on Windows `taskkill /T` walks the tree, since a timed-out installer's
/// grandchildren would otherwise outlive it.
fn kill_process_group(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        let _ = Command::new("kill")
            .args(["-9", "--", &format!("-{}", child.id())])
            .output();
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/T", "/F", "/PID", &child.id().to_string()])
            .output();
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

    #[test]
    fn deps_json_reports_every_dep_with_the_contract_fields() {
        let json = deps_json();
        let v: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        let deps = v["deps"].as_array().expect("deps array");
        let ids: Vec<&str> = deps.iter().filter_map(|d| d["id"].as_str()).collect();
        assert_eq!(
            ids,
            ["renode", "esp-qemu", "ngspice", "kicad-cli", "avr", "codex"],
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
            } else {
                assert!(
                    offhost.is_none(),
                    "a local binary must not claim to send anything: {d}"
                );
            }
        }
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
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo starting; echo 'the disk is full' >&2; exit 3"]);
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
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo begun; sleep 30; echo never"]);
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

    #[test]
    fn success_streams_and_returns_ok() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo one; echo two >&2; exit 0"]);
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
        let script = dir.join("install-sims.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\necho fake renode install ran \"$@\"\nexit 0\n",
        )
        .unwrap();
        std::env::set_var("HAUKSBEE_INSTALL_SIMS", &script);
        let mut lines = Vec::new();
        let res = install_dep("renode", &mut |l| lines.push(l.to_string()));
        std::env::remove_var("HAUKSBEE_INSTALL_SIMS");
        let _ = std::fs::remove_dir_all(&dir);
        if std::env::consts::OS == "macos" || std::env::consts::OS == "linux" {
            res.expect("fake installer exits 0");
            assert!(
                lines
                    .iter()
                    .any(|l| l.contains("fake renode install ran --renode-only")),
                "the script ran with --renode-only: {lines:?}"
            );
        }
    }
}
