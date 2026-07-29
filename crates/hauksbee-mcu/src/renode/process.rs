//! Renode process discovery and lifecycle management.
//!
//! Renode ships as a self-contained binary (the macOS arm64 `.dmg`, the Linux
//! portable tarball, the Windows portable `.zip` / `.msi`, or a system
//! package). We locate it by, in order:
//!   1. the `HAUKSBEE_RENODE` environment variable (full path to the binary),
//!   2. a `renode` on `PATH` (`renode.exe` on Windows),
//!   3. the conventional portable install under `~/renode-portable` (macOS app
//!      bundle, Linux tarball, or Windows portable zip layout),
//!   4. on Windows only: the installer trees under `%ProgramFiles%` and
//!      `%LOCALAPPDATA%\Programs`.
//!
//! A spawned instance is launched headless (`--disable-xwt --hide-log -p`) with
//! a Monitor TCP port, and torn down on drop.
//!
//! Long-form how-and-why: docs/how-and-why/hauksbee-mcu/renode.md.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

/// Locate the Renode executable, or return an error describing what to install.
pub fn find_renode() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("HAUKSBEE_RENODE") {
        let p = PathBuf::from(p);
        if p.exists() {
            return Ok(p);
        }
        bail!(
            "HAUKSBEE_RENODE is set to '{}' but it does not exist",
            p.display()
        );
    }

    // `renode` on PATH (`renode.exe` on Windows; `which` tries the extension).
    if let Ok(path) = which("renode") {
        return Ok(path);
    }

    // Conventional install locations.
    if let Some(found) = first_existing(&conventional_candidates()) {
        return Ok(found);
    }

    bail!(
        "Renode not found. One-click installs exist: run `hauksbee install \
         renode`, or in the app use Install on the Environment page. Manual \
         routes: install it (https://renode.io) and either put `renode` on \
         PATH, set HAUKSBEE_RENODE to the binary, or extract the portable \
         build to ~/renode-portable."
    )
}

/// All conventional install locations for the current environment: the
/// portable layouts under the home directory plus, on Windows, the installer
/// trees. Only the environment lookups are cfg-gated; the layout helpers below
/// stay platform-neutral so the unit tests exercise every layout on any OS.
fn conventional_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(home) = home_dir() {
        out.extend(home_candidates(&home));
    }
    #[cfg(windows)]
    {
        // The .msi installer defaults to `%ProgramFiles%\Renode`; per-user
        // tools conventionally land under `%LOCALAPPDATA%\Programs`. The env
        // variable names exist only on Windows, hence the cfg gate.
        for var in ["ProgramFiles", "ProgramFiles(x86)"] {
            if let Some(pf) = std::env::var_os(var) {
                out.extend(windows_install_candidates(Path::new(&pf)));
            }
        }
        if let Some(lad) = std::env::var_os("LOCALAPPDATA") {
            out.extend(windows_install_candidates(
                &Path::new(&lad).join("Programs"),
            ));
        }
    }
    out
}

/// Candidate Renode binaries under one home directory: the `~/renode-portable`
/// layouts our docs and installer produce on each OS.
fn home_candidates(home: &Path) -> Vec<PathBuf> {
    [
        // macOS: the app bundle copied out of the portable .dmg.
        "renode-portable/Renode.app/Contents/MacOS/renode",
        // Linux: the portable tarball (plus the underscore spelling the
        // upstream tarball itself unpacks to).
        "renode-portable/renode",
        "renode_portable/renode",
        // Windows: the portable zip extracted into ~\renode-portable puts
        // Renode.exe at the top; a copied installer tree carries bin\.
        // NTFS is case-insensitive, so the one capitalised spelling matches
        // however the file is cased on disk.
        "renode-portable/Renode.exe",
        "renode-portable/bin/Renode.exe",
        "renode_portable/Renode.exe",
        "renode_portable/bin/Renode.exe",
    ]
    .iter()
    .map(|rel| home.join(rel))
    .collect()
}

/// Candidate Renode binaries under one Windows install root (`%ProgramFiles%`,
/// `%LOCALAPPDATA%\Programs`): the .msi tree (`Renode\bin\Renode.exe`) and a
/// zip extracted straight into the root (`Renode\Renode.exe`).
/// Only Windows discovery calls this at runtime, but it is compiled (and
/// unit-tested) on every OS so a layout regression shows up in the native
/// suite, not just on a Windows machine.
#[cfg_attr(not(windows), allow(dead_code))]
fn windows_install_candidates(root: &Path) -> Vec<PathBuf> {
    ["Renode/bin/Renode.exe", "Renode/Renode.exe"]
        .iter()
        .map(|rel| root.join(rel))
        .collect()
}

/// First candidate that exists as a file, in priority order.
fn first_existing(candidates: &[PathBuf]) -> Option<PathBuf> {
    candidates.iter().find(|c| c.is_file()).cloned()
}

/// The user's home directory: `$HOME` first (Unix, and any shell that sets it
/// deliberately wins on every OS), then `%USERPROFILE%` (the Windows
/// convention, where HOME is normally unset).
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
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

        let mut cmd = Command::new(&bin);
        cmd.current_dir(&workdir)
            .arg("--disable-xwt")
            .arg("--hide-log")
            .arg("-p") // plain output: strip ANSI colour codes
            .arg("-P")
            .arg(monitor_port.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // Own process group: teardown kills the whole tree (the .NET
            // Renode host plus anything it forks) with one group kill, and
            // the signal reaper (crate::children) can do the same when the
            // parent itself is terminated. See children.rs for the emulator
            // leak this prevents.
            cmd.process_group(0);
        }
        let child = cmd
            .spawn()
            .with_context(|| format!("spawning Renode from {}", bin.display()))?;
        crate::children::register(child.id());

        Ok(RenodeProcess {
            child,
            monitor_port,
        })
    }

    /// How long to wait for the Monitor port to come up after spawn.
    pub fn startup_timeout() -> Duration {
        Duration::from_secs(30)
    }

    /// The spawned Renode's OS process id (diagnostics and the reaping tests).
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// `Some(reason)` once the process has exited, `None` while it still runs.
    ///
    /// The startup wait polls this so a Renode that failed to bind its monitor
    /// port reports that fact immediately instead of after the full timeout.
    pub fn exit_reason(&mut self) -> Option<String> {
        match self.child.try_wait() {
            Ok(Some(status)) => Some(format!("exit status {status}")),
            Ok(None) => None,
            // A child we can no longer wait on is gone as far as we are
            // concerned; treating it as alive would hang the caller.
            Err(e) => Some(format!("wait failed: {e}")),
        }
    }
}

impl Drop for RenodeProcess {
    fn drop(&mut self) {
        // Best-effort terminate; Renode has no clean SIGTERM handler we rely
        // on, so kill and reap to avoid zombies. Tree-kill first (the group
        // on unix, taskkill /T on Windows) so nothing Renode forked survives,
        // then the direct kill/wait to reap the child handle. Also drops the
        // signal-reaper registration.
        crate::children::unregister(self.child.id());
        crate::children::kill_tree(self.child.id());
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Minimal `which`: search `PATH` for an executable named `name`. On Windows
/// executables carry an extension, so `<name>.exe` is tried first there (that
/// is what Renode ships); the bare name stays as a fallback for MSYS2-style
/// shims.
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
    fn touch(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"").unwrap();
    }

    /// Every documented `~/renode-portable` layout resolves, on any OS: the
    /// probe roots are parameterised, so the Windows zip layout is exercised
    /// by the native suite too.
    #[test]
    fn home_layouts_all_resolve() {
        for layout in [
            "renode-portable/Renode.app/Contents/MacOS/renode",
            "renode-portable/renode",
            "renode_portable/renode",
            "renode-portable/Renode.exe",
            "renode-portable/bin/Renode.exe",
            "renode_portable/Renode.exe",
            "renode_portable/bin/Renode.exe",
        ] {
            let home = tempfile::tempdir().unwrap();
            let bin = home.path().join(layout);
            touch(&bin);
            let found = first_existing(&home_candidates(home.path()));
            assert_eq!(found.as_deref(), Some(bin.as_path()), "layout {layout}");
        }
    }

    /// The Windows installer trees resolve, .msi shape first.
    #[test]
    fn windows_install_layouts_resolve() {
        for layout in ["Renode/bin/Renode.exe", "Renode/Renode.exe"] {
            let root = tempfile::tempdir().unwrap();
            let bin = root.path().join(layout);
            touch(&bin);
            let found = first_existing(&windows_install_candidates(root.path()));
            assert_eq!(found.as_deref(), Some(bin.as_path()), "layout {layout}");
        }
    }

    /// An empty home yields no candidates hit, and a directory (not a file)
    /// at a candidate path is not accepted.
    #[test]
    fn misses_and_directories_are_rejected() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(first_existing(&home_candidates(home.path())), None);
        std::fs::create_dir_all(home.path().join("renode-portable/renode")).unwrap();
        assert_eq!(
            first_existing(&home_candidates(home.path())),
            None,
            "a directory named like the binary must not be picked up"
        );
    }
}
