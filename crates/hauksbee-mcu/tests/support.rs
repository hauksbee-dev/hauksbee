//! Fixture lookup shared by every integration-test module in this binary.
//!
//! The firmware images the emulator-backed tests boot are committed under
//! `testdata/firmware/` at the workspace root, but each one is BUILT by its own
//! `make`/`build.sh`, so a fresh checkout has the sources and not the images. A
//! test that cannot find its fixture must skip with a message naming what to
//! build, never fail, which is why every lookup returns `Option` rather than a
//! path: absence is a normal, reportable state here.

use std::path::PathBuf;

/// Absolute path to `testdata/firmware/<rel>`, or `None` when that fixture has
/// not been built. Canonicalized where possible so a backend that resolves
/// relative paths against its own working directory (Renode) still finds it.
pub fn firmware(rel: &str) -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/firmware")
        .join(rel);
    p.exists().then(|| p.canonicalize().unwrap_or(p))
}
