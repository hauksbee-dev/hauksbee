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
//!   3. the conventional unpacked location `~/.galvani-qemu-esp/qemu/bin/`
//!      (or the legacy `~/.hauksbee-qemu-esp/qemu/bin/`),
//!   4. the esp-idf tools install (`~/.espressif/tools/qemu-*/.../bin/`),
//!   5. the binary on `PATH`.
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
    let mut candidates: Vec<PathBuf> = Vec::new();

    // 2. Generic dir override pointing at the fork's bin/.
    if let Some(dir) = std::env::var_os("HAUKSBEE_QEMU_DIR") {
        candidates.push(PathBuf::from(dir).join(name));
    }

    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(&home);
        // 3. Conventional unpacked location (what the docs tell you to use).
        //    `.galvani-qemu-esp` is the current name; `.hauksbee-qemu-esp` is
        //    kept as a fallback for installs predating the hauksbee->galvani
        //    rename, so an existing unpacked fork keeps resolving.
        candidates.push(home.join(".galvani-qemu-esp/qemu/bin").join(name));
        candidates.push(home.join(".hauksbee-qemu-esp/qemu/bin").join(name));
        // 4. esp-idf idf_tools install. The directory carries a version, so
        //    glob the qemu-* tool dirs.
        if let Ok(entries) = std::fs::read_dir(home.join(".espressif/tools")) {
            for e in entries.flatten() {
                let p = e.path();
                if p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with("qemu-"))
                    .unwrap_or(false)
                {
                    // .../qemu-xtensa/<ver>/qemu/bin/<name>
                    if let Ok(vers) = std::fs::read_dir(&p) {
                        for v in vers.flatten() {
                            candidates.push(v.path().join("qemu/bin").join(name));
                        }
                    }
                }
            }
        }
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
        "Espressif QEMU ({name}) not found. Install the fork's prebuilt binary \
         (https://github.com/espressif/qemu/releases) and unpack it to \
         ~/.galvani-qemu-esp/qemu, set {} to the binary, or install it via \
         esp-idf `idf_tools.py install qemu-xtensa qemu-riscv32`. Homebrew's \
         mainline qemu-system-xtensa has no esp32 machine and will not work.",
        arch.env_override()
    )
}

/// True if a usable Espressif QEMU for `arch` can be located. Used to skip
/// integration tests cleanly when the emulator is absent.
pub fn is_available(arch: QemuArch) -> bool {
    find_qemu(arch).is_ok()
}

/// Verify a candidate `qemu-system-*` is the Espressif fork by checking its
/// machine list advertises an `esp32`-family machine. This is what keeps a
/// Homebrew mainline binary on `PATH` from being mistaken for the fork.
fn is_esp_fork(bin: &std::path::Path) -> bool {
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
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        let child = cmd
            .spawn()
            .with_context(|| format!("spawning Espressif QEMU from {}", bin.display()))?;

        Ok(QemuProcess { child, qmp_port })
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
}

impl Drop for QemuProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Minimal `which`: search `PATH` for an executable named `name`.
fn which(name: &str) -> Result<PathBuf> {
    let path = std::env::var_os("PATH").context("PATH not set")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    bail!("{name} not found on PATH")
}
