//! Fixture lookup shared by every integration-test module in this binary.
//!
//! The firmware images the emulator-backed tests boot are committed under
//! `testdata/firmware/` at the workspace root, but each one is BUILT by its own
//! `make`/`build.sh`, so a fresh checkout has the sources and not the images. A
//! test that cannot find its fixture must skip with a message naming what to
//! build, never fail, which is why every lookup returns `Option` rather than a
//! path: absence is a normal, reportable state here.

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, OnceLock};

/// Serialize every test that touches Espressif QEMU: the ones that boot an
/// instance, and the ones that point the locator at a fake binary through
/// `HAUKSBEE_QEMU_XTENSA`. Since this binary links every module, an override
/// set by one test is visible to a booting test on another thread, which then
/// finds a "QEMU" that cannot boot and fails instead of skipping. One lock
/// across both kinds closes that, and keeps the real boots back-to-back so
/// their wall-clock behaviour stays predictable.
pub fn qemu_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Absolute path to `testdata/firmware/<rel>`, or `None` when that fixture has
/// not been built. Canonicalized where possible so a backend that resolves
/// relative paths against its own working directory (Renode) still finds it.
pub fn firmware(rel: &str) -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/firmware")
        .join(rel);
    p.exists().then(|| p.canonicalize().unwrap_or(p))
}
