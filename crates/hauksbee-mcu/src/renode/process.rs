//! Renode process discovery and lifecycle management.
//!
//! Renode ships as a self-contained binary (the macOS arm64 `.dmg`, the Linux
//! portable tarball, or a system package). We locate it by, in order:
//!   1. the `HAUKSBEE_RENODE` environment variable (full path to the binary),
//!   2. a `renode` on `PATH`,
//!   3. the conventional macOS app-bundle install under `~/renode-portable`.
//!
//! A spawned instance is launched headless (`--disable-xwt --hide-log -p`) with
//! a Monitor TCP port, and torn down on drop.

use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

/// Locate the Renode executable, or return an error describing what to install.
pub fn find_renode() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("HAUKSBEE_RENODE") {
        let p = PathBuf::from(p);
        if p.exists() {
            return Ok(p);
        }
        bail!("HAUKSBEE_RENODE is set to '{}' but it does not exist", p.display());
    }

    // `renode` on PATH.
    if let Ok(path) = which("renode") {
        return Ok(path);
    }

    // Conventional portable install (macOS app bundle / extracted tarball).
    if let Some(home) = std::env::var_os("HOME") {
        let candidates = [
            PathBuf::from(&home)
                .join("renode-portable/Renode.app/Contents/MacOS/renode"),
            PathBuf::from(&home).join("renode-portable/renode"),
            PathBuf::from(&home).join("renode_portable/renode"),
        ];
        for c in candidates {
            if c.exists() {
                return Ok(c);
            }
        }
    }

    bail!(
        "Renode not found. Install it (https://renode.io) and either put `renode` \
         on PATH, set HAUKSBEE_RENODE to the binary, or extract the portable build \
         to ~/renode-portable."
    )
}

/// True if a usable Renode install can be located. Used to skip tests cleanly.
pub fn is_available() -> bool {
    find_renode().is_ok()
}

/// A spawned, headless Renode instance with a Monitor TCP port.
pub struct RenodeProcess {
    child: Child,
    pub monitor_port: u16,
}

impl RenodeProcess {
    /// Spawn Renode headless, listening for Monitor commands on `monitor_port`.
    ///
    /// The working directory is set to the binary's directory so that
    /// `@platforms/...` and `@scripts/...` relative paths resolve against the
    /// install tree (Renode resolves `@` paths relative to its base directory).
    pub fn spawn(monitor_port: u16) -> Result<Self> {
        let bin = find_renode()?;
        let workdir = bin
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));

        let child = Command::new(&bin)
            .current_dir(&workdir)
            .arg("--disable-xwt")
            .arg("--hide-log")
            .arg("-p") // plain output: strip ANSI colour codes
            .arg("-P")
            .arg(monitor_port.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("spawning Renode from {}", bin.display()))?;

        Ok(RenodeProcess {
            child,
            monitor_port,
        })
    }

    /// How long to wait for the Monitor port to come up after spawn.
    pub fn startup_timeout() -> Duration {
        Duration::from_secs(30)
    }
}

impl Drop for RenodeProcess {
    fn drop(&mut self) {
        // Best-effort terminate; Renode has no clean SIGTERM handler we rely on,
        // so kill and reap to avoid zombies.
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
