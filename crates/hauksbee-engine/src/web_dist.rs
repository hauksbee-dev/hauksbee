//! Resolve where the built web app (`frontend/dist`) lives at runtime, so both
//! `hauksbee serve` and `hauksbee run --serve` can hand the static files to the
//! server no matter how the binary got onto the machine.
//!
//! The compile-time path `CARGO_MANIFEST_DIR/../../frontend/dist` only exists on
//! the build machine's checkout. A user who installs a release binary (or moves
//! it) has no such directory, so no web UI to serve. The `embed-web` cargo
//! feature fixes that: release bundles compile the built dist into the binary
//! and this resolver extracts it to a cache dir on first use.
//!
//! Precedence ladder (first hit wins):
//!   a. `HAUKSBEE_WEB_DIST` env var, if set and the dir exists. An explicit
//!      override, handy for packaging and tests.
//!   b. The checkout path `CARGO_MANIFEST_DIR/../../frontend/dist`, if it exists.
//!      Dev builds serve the LIVE dist, which is fresher than any embed.
//!   c. Only when built with `--features embed-web`: the embedded copy, extracted
//!      once to a versioned cache dir, and that dir returned.
//!
//! When `embed-web` is OFF, step (c) compiles to nothing (no `rust_embed`
//! reference at all), so a `--no-default-features` build without the feature
//! still compiles and behaves exactly like the checkout-only original.

use std::path::PathBuf;

/// The build-machine checkout path. This is the same expression the serve
/// handlers used inline before the resolver existed, kept here so the two call
/// sites converge on one definition.
fn checkout_dist() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../frontend/dist")
}

/// Locate the directory of built web assets to serve, or `None` if there is
/// none to serve (the serve handlers then print their build-the-frontend hint).
pub fn resolve_web_dist() -> Option<PathBuf> {
    let mut embedded_only = false;
    // (a) Explicit override always wins, if it points at a real directory.
    if let Some(raw) = std::env::var_os("HAUKSBEE_WEB_DIST") {
        if raw == ":embedded:" {
            // Release clean-room tests need to prove the binary's payload even
            // while the build checkout (and its live dist) still exists.
            embedded_only = true;
        } else {
            let p = PathBuf::from(raw);
            if p.is_dir() {
                return Some(p);
            }
        }
    }

    // (b) A source checkout serves its live dist directly (fresher than embed).
    let checkout = checkout_dist();
    if !embedded_only && checkout.is_dir() {
        return Some(checkout);
    }

    // (c) A bare installed binary: fall back to the embedded copy, if compiled in.
    #[cfg(feature = "embed-web")]
    {
        if let Some(dir) = embedded::extract() {
            return Some(dir);
        }
    }

    None
}

/// The embedded-assets path. Entirely behind `cfg(feature = "embed-web")` so a
/// build without the feature never references `rust_embed`.
#[cfg(feature = "embed-web")]
mod embedded {
    use sha2::{Digest, Sha256};
    use std::path::{Path, PathBuf};

    /// The built web app, compiled into the binary. `boards3d/` is EXCLUDED: it
    /// is ~14 MB of demo-only GLB 3D models, embedding them would bloat the
    /// binary massively, and uploaded user boards carry no GLB anyway. The 2D
    /// map and everything else stay, so a bare binary serves the full UI minus
    /// the pre-baked 3D demos.
    ///
    /// The folder is given relative to `CARGO_MANIFEST_DIR` (rust-embed resolves
    /// a relative `#[folder]` against it) rather than as `$CARGO_MANIFEST_DIR/..`
    /// so we do not also need rust-embed's `interpolate-folder-path` feature;
    /// `include-exclude` is the only sub-feature `embed-web` has to pull in.
    #[derive(rust_embed::RustEmbed)]
    #[folder = "../../frontend/dist"]
    #[exclude = "boards3d/*"]
    struct WebAssets;

    /// Root of the per-user cache. Mirrors the XDG cache convention without
    /// pulling in the `dirs` crate: `XDG_CACHE_HOME`, else `HOME/.cache`, else
    /// the system temp dir as a last resort.
    fn cache_root() -> PathBuf {
        if let Some(x) = std::env::var_os("XDG_CACHE_HOME") {
            if !x.is_empty() {
                return PathBuf::from(x);
            }
        }
        if let Some(home) = std::env::var_os("HOME") {
            if !home.is_empty() {
                return PathBuf::from(home).join(".cache");
            }
        }
        std::env::temp_dir()
    }

    /// True when `dir` exists and holds at least one entry.
    fn dir_non_empty(dir: &Path) -> bool {
        std::fs::read_dir(dir)
            .map(|mut entries| entries.next().is_some())
            .unwrap_or(false)
    }

    /// Stable identity of the actual embedded payload. Package versions are
    /// not unique build identities: a local rebuild or corrected private
    /// artifact can legitimately keep the same semver while changing JS.
    fn embedded_digest() -> Option<String> {
        let mut paths: Vec<_> = WebAssets::iter().map(|p| p.into_owned()).collect();
        paths.sort();
        let mut hash = Sha256::new();
        for path in paths {
            let file = WebAssets::get(&path)?;
            hash.update(path.as_bytes());
            hash.update([0]);
            hash.update(file.data.as_ref());
            hash.update([0]);
        }
        let digest = hash.finalize();
        Some(digest[..12].iter().map(|b| format!("{b:02x}")).collect())
    }

    /// Extract the embedded web app to a versioned cache dir and return it.
    ///
    /// The dir is keyed by package version AND embedded payload digest, so a
    /// same-version rebuild cannot accidentally serve an older cached UI.
    /// Extraction is idempotent and cheap on subsequent runs: an existing,
    /// non-empty versioned dir is returned as-is without touching the disk.
    ///
    /// To avoid a crash mid-extract leaving a half-populated cache that a later
    /// run would wrongly treat as complete, files land in a private temp dir
    /// first and are renamed into place atomically. On any error, returns
    /// `None` (the caller falls back to its build-the-frontend hint; it never
    /// panics).
    pub fn extract() -> Option<PathBuf> {
        let base = cache_root().join("hauksbee");
        let payload = embedded_digest()?;
        let cache_name = format!("web-{}-{payload}", env!("CARGO_PKG_VERSION"));
        let dir = base.join(&cache_name);

        // Fast path: a previous run already extracted this version.
        if dir_non_empty(&dir) {
            return Some(dir);
        }

        // Stage into a process-private temp dir, then rename atomically.
        let tmp = base.join(format!("{cache_name}.tmp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);

        for path in WebAssets::iter() {
            let file = WebAssets::get(path.as_ref())?;
            let dest = tmp.join(path.as_ref());
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent).ok()?;
            }
            std::fs::write(&dest, file.data.as_ref()).ok()?;
        }

        if !dir_non_empty(&tmp) {
            // Nothing was embedded (an empty dist at compile time). Give up.
            let _ = std::fs::remove_dir_all(&tmp);
            return None;
        }

        // Rename into place. If another process won the race and created `dir`
        // first, reuse theirs and drop our temp copy.
        match std::fs::rename(&tmp, &dir) {
            Ok(()) => Some(dir),
            Err(_) => {
                let _ = std::fs::remove_dir_all(&tmp);
                dir_non_empty(&dir).then_some(dir)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialize the env-var mutations: `resolve_web_dist` reads process-global
    /// state, so two tests poking `HAUKSBEE_WEB_DIST` at once would race.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// A set `HAUKSBEE_WEB_DIST` pointing at a real directory wins outright, and
    /// a stale/nonexistent override is ignored (falls through the ladder).
    #[test]
    fn env_override_takes_precedence_when_dir_exists() {
        let _guard = ENV_LOCK.lock().unwrap();
        let root =
            std::env::temp_dir().join(format!("hauksbee-webdist-env-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        // Existing dir -> returned verbatim.
        // SAFETY: guarded by ENV_LOCK; single-threaded within this test.
        unsafe { std::env::set_var("HAUKSBEE_WEB_DIST", &root) };
        assert_eq!(resolve_web_dist().as_deref(), Some(root.as_path()));

        // Nonexistent override -> ignored, does not short-circuit the ladder.
        let missing = root.join("does-not-exist");
        unsafe { std::env::set_var("HAUKSBEE_WEB_DIST", &missing) };
        assert_ne!(resolve_web_dist().as_deref(), Some(missing.as_path()));

        unsafe { std::env::remove_var("HAUKSBEE_WEB_DIST") };
        let _ = std::fs::remove_dir_all(&root);
    }

    /// With no override set, the resolver returns the checkout dist when it
    /// exists (the dev-build path), and its parent is the crate's frontend dir.
    #[test]
    fn checkout_branch_resolves_when_dist_present() {
        let _guard = ENV_LOCK.lock().unwrap();
        // SAFETY: guarded by ENV_LOCK; single-threaded within this test.
        unsafe { std::env::remove_var("HAUKSBEE_WEB_DIST") };

        let checkout = checkout_dist();
        let resolved = resolve_web_dist();

        if checkout.is_dir() {
            // Dev checkout with a built frontend: the checkout wins over embed.
            assert_eq!(resolved.as_deref(), Some(checkout.as_path()));
        } else {
            // No checkout dist (e.g. a CI run before the frontend is built):
            // without the embed feature there is nothing to serve. We only
            // assert the checkout branch does not fabricate a path.
            #[cfg(not(feature = "embed-web"))]
            assert!(resolved.is_none());
            let _ = resolved;
        }
    }
}
