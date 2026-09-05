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
//!   3. the exact-source patched location
//!      `~/.hauksbee-qemu-esp-patched/qemu/bin/`, then the conventional
//!      `~/.hauksbee-qemu-esp/qemu/bin/` (or legacy Galvani path),
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

use crate::children::{home_dir, which};
use crate::external::{env_override, first_accepted, EmulatorProcess};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
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
        let ext = if cfg!(windows) { ".exe" } else { "" };
        format!("{}{ext}", self.binary_name())
    }
}

/// Locate an Espressif QEMU binary for `arch`, or describe how to install it.
pub fn find_qemu(arch: QemuArch) -> Result<PathBuf> {
    if let Some(p) = env_override(arch.env_override())? {
        return Ok(p);
    }
    let file = arch.file_name();
    let mut candidates: Vec<PathBuf> = std::env::var_os("HAUKSBEE_QEMU_DIR")
        .map(|dir| PathBuf::from(dir).join(&file))
        .into_iter()
        .collect();
    if let Some(home) = home_dir() {
        candidates.extend(home_candidates(&home, &file));
    }
    for root in idf_tools_roots() {
        candidates.extend(idf_tools_candidates(&root, &file));
    }
    // PATH last, and only if it is the fork (mainline has no esp32 machine).
    candidates.extend(which(arch.binary_name()).ok());
    if let Some(found) = first_accepted(candidates, is_esp_fork) {
        return Ok(found);
    }
    bail!(
        "Espressif QEMU ({}) not found. One-click installs exist: run \
         `hauksbee install esp-qemu`, or in the app use Install on the \
         Environment page. Manual routes: unpack the fork's prebuilt binary \
         (https://github.com/espressif/qemu/releases) to \
         ~/.hauksbee-qemu-esp/qemu, build Hauksbee's reviewed GPIO-state patch \
         with `scripts/install-sims.sh --qemu-patched-source`, set {} to the \
         binary, or install it via \
         esp-idf `idf_tools.py install qemu-xtensa qemu-riscv32`. Homebrew's \
         mainline qemu-system-xtensa has no esp32 machine and will not work.",
        arch.binary_name(),
        arch.env_override()
    )
}

/// The conventional unpacked locations for the fork under one home directory:
/// Hauksbee's exact-source GPIO-patched build first (the backend still probes
/// the live QOM object; path priority alone never claims the patch is
/// present), then the current name, then the pre-rename `.galvani-qemu-esp`
/// so an existing unpacked fork keeps resolving. Takes the file name as a
/// parameter so the unit tests can exercise the Windows `.exe` shape anywhere.
fn home_candidates(home: &Path, file: &str) -> Vec<PathBuf> {
    [
        ".hauksbee-qemu-esp-patched/qemu/bin",
        ".hauksbee-qemu-esp/qemu/bin",
        ".galvani-qemu-esp/qemu/bin",
    ]
    .iter()
    .map(|dir| home.join(dir).join(file))
    .collect()
}

/// The idf-tools roots this environment could have, in priority order:
/// `$IDF_TOOLS_PATH` (the esp-idf override, honoured on every OS), the
/// per-user default `~/.espressif`, and on Windows the ESP-IDF Windows
/// installer's default root `C:\Espressif`.
fn idf_tools_roots() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = std::env::var_os("IDF_TOOLS_PATH")
        .map(PathBuf::from)
        .into_iter()
        .collect();
    roots.extend(home_dir().map(|h| h.join(".espressif")));
    #[cfg(windows)]
    roots.push(PathBuf::from("C:\\Espressif"));
    roots
}

/// Candidate binaries named `file` under one idf-tools root:
/// `<root>/tools/qemu-*/<ver>/qemu/bin/<file>`. The tool directory carries a
/// version, so the `qemu-*` dirs are globbed. Platform-neutral so the unit
/// tests can build this tree (Windows file names included) in a temp dir.
fn idf_tools_candidates(root: &Path, file: &str) -> Vec<PathBuf> {
    let Ok(tools) = std::fs::read_dir(root.join("tools")) else {
        return Vec::new();
    };
    tools
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("qemu-"))
        .filter_map(|e| std::fs::read_dir(e.path()).ok())
        .flat_map(|vers| vers.flatten().map(|v| v.path().join("qemu/bin").join(file)))
        .collect()
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
pub(crate) fn is_esp_fork(bin: &Path) -> bool {
    Command::new(bin)
        .args(["-machine", "help"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .is_ok_and(|o| {
            String::from_utf8_lossy(&o.stdout)
                .to_lowercase()
                .contains("esp32")
        })
}

/// A spawned, headless Espressif QEMU instance with a QMP socket.
pub struct QemuProcess {
    inner: EmulatorProcess,
    pub qmp_port: u16,
}

impl QemuProcess {
    /// Spawn QEMU headless for `arch`, booting `flash_image`.
    ///
    /// Wiring (all over TCP so nothing native is linked):
    ///   - `-machine <machine>`: the SoC model (esp32 / esp32s3 / esp32c3).
    ///   - `-drive file=<flash>,if=mtd,format=raw,snapshot=on`: the merged 4 MB
    ///     flash image (2nd-stage bootloader + partition table + app), with all
    ///     guest writes redirected to a private temporary snapshot. The source
    ///     may be a tracked artifact or shared by concurrent sessions; no QEMU
    ///     instance may mutate it or share writable flash state with another.
    ///     The 1st-stage ROM bootloader is baked into the QEMU binary.
    ///   - `-qmp tcp:127.0.0.1:<qmp_port>,server,nowait`: the control channel for
    ///     memory reads/writes (GPIO mailbox) and run/stop stepping.
    ///   - `-serial tcp:127.0.0.1:<uart_port>,server,nowait`: UART0 as a raw
    ///     socket, bridged the same way the Renode backend bridges its UART.
    ///   - the main timer-group watchdog disabled (`-global ... wdt_disable`) so
    ///     a paused guest is not reset out from under us.
    ///
    /// NOTE: deliberately NO `-icount`. Measured: `-icount` (any shift, with or
    /// without `sleep=off`) prevents the Espressif esp32 / esp32s3 Xtensa
    /// machines from booting at all (15 s wall: zero UART output, vs ~1 s to
    /// "hello" without it). The lockstep uses QMP stop/cont over the
    /// free-running virtual clock instead; `_icount_shift` is retained in the
    /// signature for forward compatibility but not passed to QEMU.
    pub fn spawn(
        arch: QemuArch,
        machine: &str,
        flash_image: &Path,
        _icount_shift: u8,
        qmp_port: u16,
        uart_port: u16,
    ) -> Result<Self> {
        let bin = find_qemu(arch)?;
        let flash = flash_image.to_str().context("non-UTF-8 flash image path")?;
        let mut cmd = Command::new(&bin);
        cmd.args(["-nographic", "-machine", machine, "-drive"])
            .arg(format!("file={flash},if=mtd,format=raw,snapshot=on"))
            .arg("-qmp")
            .arg(format!("tcp:127.0.0.1:{qmp_port},server,nowait"))
            .arg("-serial")
            .arg(format!("tcp:127.0.0.1:{uart_port},server,nowait"))
            .arg("-global")
            .arg(format!(
                "driver=timer.{machine}.timg,property=wdt_disable,value=true"
            ));
        // stderr captured (not /dev/null) so a failed boot can be explained
        // with QEMU's actual complaint (bad image, bad machine, bad drive size).
        Ok(QemuProcess {
            inner: EmulatorProcess::spawn("Espressif QEMU", &mut cmd, true)?,
            qmp_port,
        })
    }

    /// What QEMU wrote to stderr so far, trimmed, capped to its last 2 KiB.
    pub fn stderr_output(&self) -> String {
        self.inner.stderr_output()
    }

    /// How long to wait for the QMP port to come up after spawn.
    pub fn startup_timeout() -> Duration {
        Duration::from_secs(20)
    }

    /// True if the child has already exited (QEMU rejected its arguments or the
    /// image), so the caller can fail fast instead of waiting for a QMP timeout.
    pub fn has_exited(&mut self) -> bool {
        self.inner.exit_reason().is_some()
    }

    /// Refuse an operation once the emulator child has exited, retaining the
    /// exit status and QEMU's captured stderr; safe to call on every later
    /// chunk, so a terminal QEMU failure never degrades into bare QMP errors.
    pub fn ensure_running(&mut self, operation: &str) -> Result<()> {
        self.inner.ensure_running(operation)
    }

    /// The spawned QEMU's OS process id (diagnostics and the reaping tests).
    pub fn pid(&self) -> u32 {
        self.inner.pid()
    }
}

#[cfg(test)]
mod discovery_tests {
    use super::*;

    /// Create an empty file, parents included.
    fn touch(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"").unwrap();
    }

    /// The conventional home layouts produce the exact candidate paths, for
    /// both the Unix and the Windows (`.exe`) file names, on any OS.
    #[test]
    fn home_layouts_cover_patched_current_legacy_and_windows_names() {
        let home = tempfile::tempdir().unwrap();
        for file in ["qemu-system-xtensa", "qemu-system-xtensa.exe"] {
            let cands = home_candidates(home.path(), file);
            assert_eq!(
                cands,
                vec![
                    home.path()
                        .join(".hauksbee-qemu-esp-patched/qemu/bin")
                        .join(file),
                    home.path().join(".hauksbee-qemu-esp/qemu/bin").join(file),
                    home.path().join(".galvani-qemu-esp/qemu/bin").join(file),
                ],
                "reviewed patched build first, then current and legacy upstream installs"
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
