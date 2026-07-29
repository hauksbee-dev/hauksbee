//! Espressif QEMU process discovery and lifecycle management.
//!
//! Classic ESP32 (Xtensa LX6/LX7) and the RISC-V ESP32-C3 are not modelled by
//! mainline QEMU or by Renode (no `esp32.repl` ships in either). Espressif
//! maintains a QEMU fork with full ESP32 SoC peripheral models (GPIO matrix,
//! UART, SPI flash controller, timers) and native macOS-arm64 / Linux release
//! binaries. This module locates that fork's `qemu-system-xtensa` (ESP32 /
//! ESP32-S3) and `qemu-system-riscv32` (ESP32-C3) binaries and spawns them.
//!
//! Discovery order, per architecture, is:
//!   1. an explicit env override (`HAUKSBEE_QEMU_XTENSA` / `HAUKSBEE_QEMU_RISCV32`),
//!   2. a generic `HAUKSBEE_QEMU_DIR` pointing at the fork's `bin/`,
//!   3. the conventional unpacked location `~/.hauksbee-qemu-esp/qemu/bin/`
//!      (or the legacy `~/.galvani-qemu-esp/qemu/bin/`),
//!   4. the esp-idf tools install: `$IDF_TOOLS_PATH/tools/qemu-*/.../bin/`
//!      when set, else `~/.espressif/tools/qemu-*/.../bin/` (the idf_tools.py
//!      default on every OS), plus `C:\Espressif\tools\...` on Windows (the
//!      ESP-IDF Windows installer's default root),
//!   5. the binary on `PATH`.
//! On Windows the binary file names carry `.exe`.
//!
//! IMPORTANT: this must resolve the *Espressif* fork, not Homebrew's mainline
//! `qemu-system-xtensa` (which has only `lx60`/`kc705`/`sim` machines and cannot
//! boot an ESP32 image). [`is_esp_fork`] verifies the binary advertises an
//! `esp32` machine before it is accepted.
//!
//! Long-form how-and-why: docs/how-and-why/hauksbee-mcu/qemu.md.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

/// Which Espressif QEMU system binary an architecture needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum QemuArch {
    /// Xtensa LX6/LX7: ESP32 and ESP32-S3 (`qemu-system-xtensa`).
    Xtensa,
    /// RISC-V RV32IMC: ESP32-C3 (`qemu-system-riscv32`).
    Riscv32,
}

impl QemuArch {
    /// The QEMU system binary file name for this architecture.
    pub fn binary_name(self) -> &'static str {
        match self {
            QemuArch::Xtensa => "qemu-system-xtensa",
            QemuArch::Riscv32 => "qemu-system-riscv32",
        }
    }

    /// The per-arch env override variable name.
    fn env_override(self) -> &'static str {
        match self {
            QemuArch::Xtensa => "HAUKSBEE_QEMU_XTENSA",
            QemuArch::Riscv32 => "HAUKSBEE_QEMU_RISCV32",
        }
    }

    /// The binary file name on this platform: the Espressif Windows builds
    /// ship `qemu-system-*.exe`, everywhere else the bare name.
    fn file_name(self) -> String {
        if cfg!(windows) {
            format!("{}.exe", self.binary_name())
        } else {
            self.binary_name().to_string()
        }
    }
}

/// Locate an Espressif QEMU binary for `arch`, or describe how to install it.
pub fn find_qemu(arch: QemuArch) -> Result<PathBuf> {
    // 1. Explicit per-arch override (full path to the binary).
    if let Some(p) = std::env::var_os(arch.env_override()) {
        let p = PathBuf::from(p);
        if p.exists() {
            return Ok(p);
        }
        bail!(
            "{} is set to '{}' but it does not exist",
            arch.env_override(),
            p.display()
        );
    }

    let name = arch.binary_name();
    let file = arch.file_name();
    let mut candidates: Vec<PathBuf> = Vec::new();

    // 2. Generic dir override pointing at the fork's bin/.
    if let Some(dir) = std::env::var_os("HAUKSBEE_QEMU_DIR") {
        candidates.push(PathBuf::from(dir).join(&file));
    }

    if let Some(home) = home_dir() {
        // 3. Conventional unpacked location (what the docs tell you to use).
        //    `.hauksbee-qemu-esp` is the current name; `.galvani-qemu-esp` is
        //    kept as a fallback for installs predating the galvani->hauksbee
        //    rename, so an existing unpacked fork keeps resolving.
        candidates.extend(home_candidates(&home, &file));
    }
    // 4. esp-idf idf_tools installs, whichever roots this environment has.
    for root in idf_tools_roots() {
        candidates.extend(idf_tools_candidates(&root, &file));
    }

    for c in &candidates {
        if c.is_file() && is_esp_fork(c) {
            return Ok(c.clone());
        }
    }

    // 5. PATH, but only if it is the Espressif fork (mainline has no esp32).
    if let Ok(path) = which(name) {
        if is_esp_fork(&path) {
            return Ok(path);
        }
    }

    bail!(
        "Espressif QEMU ({name}) not found. One-click installs exist: run \
         `hauksbee install esp-qemu`, or in the app use Install on the \
         Environment page. Manual routes: unpack the fork's prebuilt binary \
         (https://github.com/espressif/qemu/releases) to \
         ~/.hauksbee-qemu-esp/qemu, set {} to the binary, or install it via \
         esp-idf `idf_tools.py install qemu-xtensa qemu-riscv32`. Homebrew's \
         mainline qemu-system-xtensa has no esp32 machine and will not work.",
        arch.env_override()
    )
}

/// The conventional unpacked locations for the fork under one home directory.
/// Takes the file name as a parameter (not `cfg!`-derived inside) so the unit
/// tests can exercise the Windows `.exe` shape on any OS.
fn home_candidates(home: &std::path::Path, file: &str) -> Vec<PathBuf> {
    vec![
        home.join(".hauksbee-qemu-esp/qemu/bin").join(file),
        home.join(".galvani-qemu-esp/qemu/bin").join(file),
    ]
}

/// The idf-tools roots this environment could have, in priority order:
/// `$IDF_TOOLS_PATH` (the esp-idf override, honoured on every OS), the
/// per-user default `~/.espressif`, and on Windows the ESP-IDF Windows
/// installer's default root `C:\Espressif`.
fn idf_tools_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(p) = std::env::var_os("IDF_TOOLS_PATH") {
        roots.push(PathBuf::from(p));
    }
    if let Some(home) = home_dir() {
        roots.push(home.join(".espressif"));
    }
    #[cfg(windows)]
    roots.push(PathBuf::from("C:\\Espressif"));
    roots
}

/// Candidate binaries named `file` under one idf-tools root:
/// `<root>/tools/qemu-*/<ver>/qemu/bin/<file>`. The tool directory carries a
/// version, so the `qemu-*` dirs are globbed. Platform-neutral so the unit
/// tests can build this tree (Windows file names included) in a temp dir.
fn idf_tools_candidates(root: &std::path::Path, file: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(root.join("tools")) {
        for e in entries.flatten() {
            let p = e.path();
            if p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("qemu-"))
                .unwrap_or(false)
            {
                // .../qemu-xtensa/<ver>/qemu/bin/<file>
                if let Ok(vers) = std::fs::read_dir(&p) {
                    for v in vers.flatten() {
                        out.push(v.path().join("qemu/bin").join(file));
                    }
                }
            }
        }
    }
    out
}

/// The user's home directory: `$HOME` first (Unix, and a deliberate override
/// wins on every OS), then `%USERPROFILE%` (the Windows convention, where HOME
/// is normally unset).
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// True if a usable Espressif QEMU for `arch` can be located. Used to skip
/// integration tests cleanly when the emulator is absent.
pub fn is_available(arch: QemuArch) -> bool {
    find_qemu(arch).is_ok()
}

/// Verify a candidate `qemu-system-*` is the Espressif fork by checking its
/// machine list advertises an `esp32`-family machine. This is what keeps a
/// Homebrew mainline binary on `PATH` from being mistaken for the fork.
/// `pub(crate)` so the installer (`qemu::install`) accepts a freshly unpacked
/// binary through the exact same check discovery uses.
pub(crate) fn is_esp_fork(bin: &std::path::Path) -> bool {
    let out = Command::new(bin)
        .arg("-machine")
        .arg("help")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    match out {
        Ok(o) => {
            let text = String::from_utf8_lossy(&o.stdout).to_lowercase();
            text.contains("esp32")
        }
        Err(_) => false,
    }
}

/// A spawned, headless Espressif QEMU instance with a QMP socket.
pub struct QemuProcess {
    child: Child,
    pub qmp_port: u16,
    /// QEMU's stderr, redirected to a temp file so that when the process dies
    /// (bad image, bad machine, bad drive size) the caller can surface QEMU's
    /// own words instead of a bare "exited" or a downstream socket error. A
    /// pipe would need a drain thread to avoid blocking a chatty process; a
    /// file needs nothing and is read only on failure.
    stderr_log: Option<tempfile::NamedTempFile>,
}

impl QemuProcess {
    /// Spawn QEMU headless for `arch`, booting `flash_image`.
    ///
    /// Wiring (all over TCP so nothing native is linked):
    ///   - `-machine <machine>`: the SoC model (esp32 / esp32s3 / esp32c3).
    ///   - `-drive file=<flash>,if=mtd,format=raw`: the merged 4 MB flash image
    ///     (2nd-stage bootloader + partition table + app). The 1st-stage ROM
    ///     bootloader is baked into the QEMU binary.
    ///   - `-qmp tcp:127.0.0.1:<qmp_port>,server,nowait`: the control channel for
    ///     memory reads/writes (GPIO mailbox) and run/stop stepping.
    ///   - `-serial tcp:127.0.0.1:<uart_port>,server,nowait`: UART0 as a raw
    ///     socket, bridged the same way the Renode backend bridges its UART.
    ///   - watchdogs disabled so a paused guest is not reset out from under us.
    ///
    /// NOTE: deliberately NO `-icount`. We measured that `-icount` (any shift,
    /// with or without `sleep=off`) prevents the Espressif esp32 / esp32s3
    /// Xtensa machines from booting at all (15 s wall: zero UART output, vs ~1 s
    /// to "hello" without icount). icount on these Xtensa machines is undocumented
    /// and, empirically, broken. So the lockstep uses QMP stop/cont over the
    /// free-running virtual clock instead (see the backend's lockstep notes). The
    /// `_icount_shift` argument is retained in the signature for forward
    /// compatibility but not passed to QEMU.
    pub fn spawn(
        arch: QemuArch,
        machine: &str,
        flash_image: &std::path::Path,
        _icount_shift: u8,
        qmp_port: u16,
        uart_port: u16,
    ) -> Result<Self> {
        let bin = find_qemu(arch)?;
        let flash = flash_image.to_str().context("non-UTF-8 flash image path")?;

        let mut cmd = Command::new(&bin);
        cmd.arg("-nographic")
            .arg("-machine")
            .arg(machine)
            .arg("-drive")
            .arg(format!("file={flash},if=mtd,format=raw"))
            .arg("-qmp")
            .arg(format!("tcp:127.0.0.1:{qmp_port},server,nowait"))
            .arg("-serial")
            .arg(format!("tcp:127.0.0.1:{uart_port},server,nowait"));
        // Disable the main timer-group watchdog so a paused guest is not reset
        // out from under us. The driver name is per-SoC (timer.<machine>.timg).
        cmd.arg("-global").arg(format!(
            "driver=timer.{machine}.timg,property=wdt_disable,value=true"
        ));
        // stderr goes to a temp file (not /dev/null) so a failed boot can be
        // explained with QEMU's actual complaint; see the struct field note.
        let stderr_log = tempfile::Builder::new()
            .prefix("hauksbee-qemu-stderr-")
            .suffix(".log")
            .tempfile()
            .ok();
        let stderr_sink = stderr_log
            .as_ref()
            .and_then(|t| t.reopen().ok())
            .map(Stdio::from)
            .unwrap_or_else(Stdio::null);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr_sink);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // Own process group: teardown kills the whole tree (QEMU plus
            // anything it forks) with one group kill, and the signal reaper
            // (crate::children) can do the same when the parent itself is
            // terminated. Without this, killing a serving hauksbee orphaned
            // its emulators; see children.rs.
            cmd.process_group(0);
        }

        let child = cmd
            .spawn()
            .with_context(|| format!("spawning Espressif QEMU from {}", bin.display()))?;
        crate::children::register(child.id());

        Ok(QemuProcess {
            child,
            qmp_port,
            stderr_log,
        })
    }

    /// What QEMU wrote to stderr so far, trimmed, capped to its last 2 KiB.
    /// Empty string when there is nothing (or the log could not be created).
    pub fn stderr_output(&self) -> String {
        let Some(log) = &self.stderr_log else {
            return String::new();
        };
        let Ok(bytes) = std::fs::read(log.path()) else {
            return String::new();
        };
        let tail = &bytes[bytes.len().saturating_sub(2048)..];
        String::from_utf8_lossy(tail).trim().to_string()
    }

    /// How long to wait for the QMP port to come up after spawn.
    pub fn startup_timeout() -> Duration {
        Duration::from_secs(20)
    }

    /// True if the child has already exited (QEMU rejected its arguments or the
    /// image), so the caller can fail fast instead of waiting for a QMP timeout.
    pub fn has_exited(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)))
    }

    /// The spawned QEMU's OS process id (diagnostics and the reaping tests).
    pub fn pid(&self) -> u32 {
        self.child.id()
    }
}

impl Drop for QemuProcess {
    fn drop(&mut self) {
        // Tree-kill first (the group on unix, taskkill /T on Windows), then
        // the direct kill/wait to reap the child handle. Also drops the
        // signal-reaper registration.
        crate::children::unregister(self.child.id());
        crate::children::kill_tree(self.child.id());
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Minimal `which`: search `PATH` for an executable named `name`. On Windows
/// executables carry an extension, so `<name>.exe` is tried first there (what
/// the Espressif builds ship); the bare name stays as a fallback for
/// MSYS2-style shims.
fn which(name: &str) -> Result<PathBuf> {
    let path = std::env::var_os("PATH").context("PATH not set")?;
    for dir in std::env::split_paths(&path) {
        if cfg!(windows) {
            let exe = dir.join(format!("{name}.exe"));
            if exe.is_file() {
                return Ok(exe);
            }
        }
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    bail!("{name} not found on PATH")
}

#[cfg(test)]
mod discovery_tests {
    use super::*;

    /// Create an empty file, parents included.
    fn touch(path: &std::path::Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"").unwrap();
    }

    /// The conventional home layouts produce the exact candidate paths, for
    /// both the Unix and the Windows (`.exe`) file names, on any OS.
    #[test]
    fn home_layouts_cover_current_legacy_and_windows_names() {
        let home = tempfile::tempdir().unwrap();
        for file in ["qemu-system-xtensa", "qemu-system-xtensa.exe"] {
            let cands = home_candidates(home.path(), file);
            assert_eq!(
                cands,
                vec![
                    home.path().join(".hauksbee-qemu-esp/qemu/bin").join(file),
                    home.path().join(".galvani-qemu-esp/qemu/bin").join(file),
                ],
                "current location first, legacy rename fallback second"
            );
        }
    }

    /// An idf-tools tree (`<root>/tools/qemu-*/<ver>/qemu/bin/<file>`) is
    /// globbed correctly: both arch tool dirs, any version, non-qemu tool dirs
    /// ignored. This is the layout idf_tools.py produces on every OS,
    /// including `%USERPROFILE%\.espressif` and `C:\Espressif` on Windows.
    #[test]
    fn idf_tools_tree_is_globbed() {
        let root = tempfile::tempdir().unwrap();
        let xtensa = root
            .path()
            .join("tools/qemu-xtensa/esp_develop_9.2.2/qemu/bin/qemu-system-xtensa.exe");
        let riscv = root
            .path()
            .join("tools/qemu-riscv32/esp_develop_9.2.2/qemu/bin/qemu-system-riscv32.exe");
        touch(&xtensa);
        touch(&riscv);
        // A non-qemu tool must not contribute candidates.
        touch(
            &root
                .path()
                .join("tools/xtensa-esp-elf/13.2.0/bin/xtensa-esp32-elf-gcc"),
        );

        let cands = idf_tools_candidates(root.path(), "qemu-system-xtensa.exe");
        assert_eq!(cands.len(), 2, "one per qemu-* tool dir: {cands:?}");
        assert!(cands.contains(&xtensa), "{cands:?}");
        assert!(
            cands
                .iter()
                .all(|c| !c.to_string_lossy().contains("xtensa-esp-elf")),
            "non-qemu tools ignored: {cands:?}"
        );

        let cands = idf_tools_candidates(root.path(), "qemu-system-riscv32.exe");
        assert!(cands.contains(&riscv), "{cands:?}");
    }

    /// A root with no tools/ directory yields no candidates (and no error).
    #[test]
    fn missing_idf_root_is_empty() {
        let root = tempfile::tempdir().unwrap();
        assert!(idf_tools_candidates(root.path(), "qemu-system-xtensa").is_empty());
    }
}
