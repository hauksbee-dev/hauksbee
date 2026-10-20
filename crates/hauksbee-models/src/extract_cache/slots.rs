//! Deterministic per-slot sandbox names.
//!
//! A fresh random sandbox path (`hauksbee-extract-XXXXXX`) reaches the model: Claude Code
//! announces the working directory at the start of the conversation and codex puts it in
//! its environment context, in front of our prompt. So every part's call started with a
//! different prefix and a cache hit stopped there. A slot is a number handed out for the
//! life of one sandbox and given back when it is dropped, so the path is
//! `<temp>/hauksbee-extract-p<pid>-s<N>`: the same for every part that runs in slot N of
//! this process, and unique across processes. The directory is created fresh (mode 0700)
//! and removed on drop, so it is empty between parts, and confinement is unchanged: the
//! agent may reach this directory and the paths it always could, and nothing else.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use anyhow::{Context, Result};

static IN_USE: Mutex<BTreeSet<usize>> = Mutex::new(BTreeSet::new());

/// An extraction sandbox directory, removed when dropped.
#[derive(Debug)]
pub struct Sandbox {
    path: PathBuf,
    slot: Option<usize>,
    /// Held for its drop (removal) when the slot name could not be used.
    _fallback: Option<tempfile::TempDir>,
}

impl Sandbox {
    /// A fresh empty sandbox under `root` (canonicalised), in the lowest free slot.
    ///
    /// # Errors
    ///
    /// Fails when `root` cannot be resolved or no directory can be created in it.
    pub fn create(root: &Path, purpose: &str) -> Result<Self> {
        let root = root
            .canonicalize()
            .with_context(|| format!("canonicalizing temporary root {}", root.display()))?;
        let slot = {
            let mut used = IN_USE.lock().unwrap_or_else(PoisonError::into_inner);
            let free = (0..).find(|n| !used.contains(n)).unwrap_or(0);
            used.insert(free);
            free
        };
        let path = root.join(format!("hauksbee-extract-p{}-s{slot}", std::process::id()));
        if make_private_dir(&path).is_ok() {
            return Ok(Self {
                path,
                slot: Some(slot),
                _fallback: None,
            });
        }
        // The slot's name is taken by something that is not ours to remove: a random name.
        release(slot);
        let dir = tempfile::Builder::new()
            .prefix("hauksbee-extract-")
            .tempdir_in(&root)
            .with_context(|| purpose.to_string())?;
        Ok(Self {
            path: dir.path().to_path_buf(),
            slot: None,
            _fallback: Some(dir),
        })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The slot number, `None` for a sandbox that fell back to a random name.
    #[must_use]
    pub const fn slot(&self) -> Option<usize> {
        self.slot
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        if let Some(slot) = self.slot {
            let _ = std::fs::remove_dir_all(&self.path);
            release(slot);
        }
    }
}

fn release(slot: usize) {
    IN_USE
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .remove(&slot);
}

/// Create `path` empty with mode 0700, replacing a directory a crashed run of this
/// pid left; refuses anything else (a symlink, a file) so the name cannot be planted.
fn make_private_dir(path: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(path)?,
        Ok(_) => return Err(std::io::ErrorKind::AlreadyExists.into()),
        Err(_) => {}
    }
    std::fs::create_dir(path)?;
    #[cfg(unix)]
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_slot_keeps_its_path_for_the_next_sandbox_and_starts_empty() {
        let root = tempfile::tempdir().unwrap();
        let a = Sandbox::create(root.path(), "t").unwrap();
        let b = Sandbox::create(root.path(), "t").unwrap();
        assert_ne!(a.path(), b.path(), "two live sandboxes never share a path");
        std::fs::write(a.path().join("datasheet.pdf"), b"part one").unwrap();
        let first = a.path().to_path_buf();
        let slot = a.slot();
        drop(a);
        assert!(!first.exists(), "dropping removes the sandbox");
        let c = Sandbox::create(root.path(), "t").unwrap();
        assert_eq!(c.path(), first, "the freed slot's path is reused");
        assert_eq!(c.slot(), slot);
        assert_eq!(std::fs::read_dir(c.path()).unwrap().count(), 0);
        assert!(c
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with(&format!("hauksbee-extract-p{}-s", std::process::id())));
    }

    #[cfg(unix)]
    #[test]
    fn a_planted_symlink_is_never_followed_and_the_sandbox_gets_a_random_name() {
        let root = tempfile::tempdir().unwrap();
        let victim = tempfile::tempdir().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        // Occupy every slot name this process could take next.
        let probe = Sandbox::create(&root_path, "t").unwrap();
        let name = probe.path().to_path_buf();
        drop(probe);
        std::os::unix::fs::symlink(victim.path(), &name).unwrap();
        std::fs::write(victim.path().join("keep"), b"x").unwrap();
        let s = Sandbox::create(&root_path, "t").unwrap();
        assert_eq!(s.slot(), None);
        assert_ne!(s.path(), name);
        drop(s);
        assert!(victim.path().join("keep").is_file());
    }
}
