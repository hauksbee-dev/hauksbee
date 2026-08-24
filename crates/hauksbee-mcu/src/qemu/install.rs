//! Fetch-and-install for the Espressif QEMU fork (`hauksbee install esp-qemu`).
//!
//! Downloads Espressif's OFFICIAL prebuilt `qemu-system-xtensa` /
//! `qemu-system-riscv32` release archives from `github.com/espressif/qemu`
//! and unpacks them into `~/.hauksbee-qemu-esp/`; the FIRST conventional
//! location [`find_qemu`](super::find_qemu) checks, then accepts each binary
//! only after it passes the same `is_esp_fork` machine-list check discovery
//! uses. Fetch, never bundle: the fork is GPL-2.0 and deliberately not
//! vendored into this Apache-2.0-licensed tree (the same posture as libsimavr and
//! Renode; see scripts/install-sims.sh, whose asset-resolution rules this
//! module mirrors).
//!
//! # Layout produced
//!
//! ```text
//! ~/.hauksbee-qemu-esp/qemu/bin/qemu-system-xtensa
//! ~/.hauksbee-qemu-esp/qemu/bin/qemu-system-riscv32
//! ~/.hauksbee-qemu-esp/qemu/share/qemu/...
//! ```
//!
//! Each release archive carries one top-level `qemu/` directory, so both
//! per-arch tarballs unpack into the same prefix and merge cleanly.
//!
//! # Release asset naming (verified against the live release)
//!
//! Tag `esp-develop-9.2.2-20260417` publishes assets named
//! `qemu-{xtensa|riscv32}-softmmu-esp_develop_9.2.2_20260417-<triple>.tar.xz`
//! (note: dashes in the tag become underscores in the asset's version part)
//! for the triples `aarch64-apple-darwin`, `x86_64-apple-darwin`,
//! `x86_64-linux-gnu`, `aarch64-linux-gnu`, `x86_64-w64-mingw32`, plus one
//! `qemu-<ver>-checksum.sha256` asset listing `<sha256hex> *<asset-name>`
//! lines. Upstream has changed both the separator convention and the
//! compression (`.tar.bz2` -> `.tar.xz`) across releases, so the installer
//! resolves the asset name by LISTING the release's published assets and only
//! falls back to the constructed modern form when the API is unreachable.
//!
//! # External tools
//!
//! Downloads shell out to `curl` and unpack with the system `tar` (which
//! sniffs xz/bz2/gz), matching the repo's no-HTTP-client posture
//! (`hauksbee models add` uses the same approach). Checksums are computed
//! with `sha256sum` or `shasum -a 256`, whichever exists.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use super::process::is_esp_fork;
use super::QemuArch;

/// The GitHub repository the fork's official prebuilt binaries come from.
pub const ESP_QEMU_REPO: &str = "espressif/qemu";

/// Pinned fallback release tag, used only when the GitHub API cannot be
/// reached to resolve `releases/latest`. Verified live on 2026-07-10.
pub const FALLBACK_TAG: &str = "esp-develop-9.2.2-20260417";

/// What Intel macOS installs instead of `BROKEN_INTEL_MAC_TAG` (a private
/// sibling constant naming the mislabeled release): the
/// previous release of the same QEMU 9.2.2 fork, whose `x86_64-apple-darwin`
/// archives contain genuine x86_64 binaries.
pub const INTEL_MAC_FALLBACK_TAG: &str = "esp-develop-9.2.2-20250817";

/// This release's `x86_64-apple-darwin` archives are mislabeled upstream:
/// both the xtensa and riscv32 tarballs contain arm64 binaries (verified
/// with `file(1)`; a real Intel Mac refuses to exec them with "Bad CPU type
/// in executable"). scripts/required-simulator-versions.env carries the same
/// override for the pinned-CI installer.
const BROKEN_INTEL_MAC_TAG: &str = "esp-develop-9.2.2-20260417";

/// Where the installer unpacks: `~/.hauksbee-qemu-esp`. The binaries land in
/// `<root>/qemu/bin/`, which is the first conventional location `find_qemu`
/// checks.
pub fn install_root() -> Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .ok_or_else(|| anyhow::anyhow!("HOME is not set; cannot pick an install directory"))?;
    Ok(PathBuf::from(home).join(".hauksbee-qemu-esp"))
}

/// The release-asset tool prefix for an architecture (`qemu-xtensa` /
/// `qemu-riscv32`), as used in the published asset names.
pub fn tool_name(arch: QemuArch) -> &'static str {
    match arch {
        QemuArch::Xtensa => "qemu-xtensa",
        QemuArch::Riscv32 => "qemu-riscv32",
    }
}

/// The `<os>-<arch>` suffix Espressif uses in its release asset names, for
/// this host. The four supported unix targets mirror
/// `scripts/install-sims.sh`. Windows publishes a `x86_64-w64-mingw32` asset
/// and discovery (`qemu::process`) does resolve a manually unpacked `.exe`
/// tree, but this installer shells `curl` and `tar`, which a stock Windows
/// box may not have in compatible form and which no native Windows runner has
/// exercised; auto-install is therefore refused there with manual guidance
/// rather than half-installed.
pub fn host_asset_triple() -> Result<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin"),
        ("linux", "x86_64") => Ok("x86_64-linux-gnu"),
        ("linux", "aarch64") => Ok("aarch64-linux-gnu"),
        (os, arch) => {
            // Name the path in the host's own convention: telling a Windows
            // user to create `~/...` sends them somewhere that does not exist.
            let unpack_hint = if cfg!(windows) {
                "%USERPROFILE%\\.hauksbee-qemu-esp\\qemu\\bin\\qemu-system-xtensa.exe"
            } else {
                "~/.hauksbee-qemu-esp/qemu/bin/qemu-system-xtensa"
            };
            bail!(
                "no Espressif QEMU prebuilt is auto-installable for {os}/{arch}. \
                 Download the right asset from \
                 https://github.com/{ESP_QEMU_REPO}/releases yourself and unpack \
                 it so {unpack_hint} exists ({}).",
                hauksbee_ir::docs_url("docs/cosim/SIMULATORS.md")
            )
        }
    }
}

/// The asset's version fragment for a release tag: dashes become underscores
/// (`esp-develop-9.2.2-20260417` -> `esp_develop_9.2.2_20260417`). Verified
/// against the live release's published names.
pub fn tag_to_asset_version(tag: &str) -> String {
    tag.replace('-', "_")
}

/// Pick the published asset for `arch` + `triple` out of a release's asset
/// list. Matches the shape `qemu-<tool>-softmmu-*-<triple>.tar.{xz,bz2,gz}`
/// so it survives upstream's historical renames (separator and compression
/// changes) without constructing a name that 404s.
pub fn pick_asset<'a, I>(names: I, arch: QemuArch, triple: &str) -> Option<String>
where
    I: IntoIterator<Item = &'a str>,
{
    let prefix = format!("{}-softmmu-", tool_name(arch));
    let suffix_mid = format!("-{triple}.tar.");
    names.into_iter().find_map(|n| {
        let compressed =
            n.ends_with(".tar.xz") || n.ends_with(".tar.bz2") || n.ends_with(".tar.gz");
        (n.starts_with(&prefix) && n.contains(&suffix_mid) && compressed).then(|| n.to_string())
    })
}

/// The constructed (fallback) asset name for when the release's asset list
/// cannot be fetched: the modern `.tar.xz` form with the underscored version.
pub fn constructed_asset_name(arch: QemuArch, tag: &str, triple: &str) -> String {
    format!(
        "{}-softmmu-{}-{}.tar.xz",
        tool_name(arch),
        tag_to_asset_version(tag),
        triple
    )
}

/// The checksum-manifest asset name for a release
/// (`qemu-<ver>-checksum.sha256`).
pub fn checksum_asset_name(tag: &str) -> String {
    format!("qemu-{}-checksum.sha256", tag_to_asset_version(tag))
}

/// Extract `asset`'s sha256 from the release's checksum manifest. Lines look
/// like `<hex> *<asset-name>` (a leading `*` marks binary mode); `#` lines
/// are comments.
pub fn checksum_for(manifest: &str, asset: &str) -> Option<String> {
    for line in manifest.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let (Some(hash), Some(name)) = (parts.next(), parts.next()) else {
            continue;
        };
        let name = name.trim_start_matches('*');
        if name == asset && hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()) {
            return Some(hash.to_ascii_lowercase());
        }
    }
    None
}

/// Pull every string value for `"key"` out of a JSON blob without a JSON
/// dependency (the same posture as the QMP client): scan for the quoted key,
/// then read the string that follows the colon. Good for the GitHub API's
/// `tag_name` / `name` fields; not a general JSON parser.
fn json_string_values(json: &str, key: &str) -> Vec<String> {
    let needle = format!("\"{key}\"");
    let mut out = Vec::new();
    let mut rest = json;
    while let Some(pos) = rest.find(&needle) {
        rest = &rest[pos + needle.len()..];
        let Some(colon) = rest.find(':') else { break };
        let after = rest[colon + 1..].trim_start();
        if let Some(stripped) = after.strip_prefix('"') {
            if let Some(end) = stripped.find('"') {
                // GitHub asset names / tags never contain escapes; a value
                // with a backslash is not one of ours, skip it.
                let val = &stripped[..end];
                if !val.contains('\\') {
                    out.push(val.to_string());
                }
            }
        }
    }
    out
}

/// One resolved download: the asset name and the URL it is fetched from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedAsset {
    pub arch: QemuArch,
    pub asset: String,
    pub url: String,
    /// Expected sha256 (lowercase hex) when the release's checksum manifest
    /// was fetched and lists this asset; `None` means "manifest unavailable",
    /// which the installer reports loudly (it does NOT silently skip a
    /// mismatch, a wrong hash always aborts).
    pub sha256: Option<String>,
}

/// What `plan()` resolved: the tag and the per-arch assets.
#[derive(Debug, Clone)]
pub struct InstallPlan {
    pub tag: String,
    pub assets: Vec<PlannedAsset>,
}

fn download_url(tag: &str, asset: &str) -> String {
    format!("https://github.com/{ESP_QEMU_REPO}/releases/download/{tag}/{asset}")
}

/// Run `curl` for a URL, returning stdout as bytes. `--fail` turns HTTP
/// errors into a nonzero exit so a 404 can never masquerade as an archive.
fn curl_bytes(url: &str) -> Result<Vec<u8>> {
    let out = Command::new("curl")
        .args(["--silent", "--show-error", "--fail", "--location", url])
        .output()
        .context("running curl (is curl installed?)")?;
    if !out.status.success() {
        bail!(
            "curl failed for {url}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

/// GET a GitHub API URL as text.
fn github_api(url: &str) -> Result<String> {
    let out = Command::new("curl")
        .args([
            "--silent",
            "--show-error",
            "--fail",
            "--location",
            "-H",
            "Accept: application/vnd.github+json",
            "-H",
            "X-GitHub-Api-Version: 2022-11-28",
            url,
        ])
        .output()
        .context("running curl (is curl installed?)")?;
    if !out.status.success() {
        bail!(
            "GitHub API request failed for {url}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Resolve the release tag and per-arch asset names/URLs/checksums for this
/// host. Network-touching (GitHub API + the checksum asset); everything it
/// returns is inspectable before any archive is downloaded.
pub fn plan(arches: &[QemuArch], progress: &mut dyn FnMut(&str)) -> Result<InstallPlan> {
    let triple = host_asset_triple()?;

    // Tag: releases/latest, with the pinned fallback when unreachable.
    let (tag, listed_names) = match github_api(&format!(
        "https://api.github.com/repos/{ESP_QEMU_REPO}/releases/latest"
    )) {
        Ok(json) => {
            let tag = json_string_values(&json, "tag_name")
                .into_iter()
                .next()
                .ok_or_else(|| anyhow::anyhow!("GitHub API response had no tag_name"))?;
            // Asset "name" fields; also matches unrelated "name" keys, which
            // pick_asset's shape filter then ignores.
            let names = json_string_values(&json, "name");
            (tag, Some(names))
        }
        Err(e) => {
            progress(&format!(
                "GitHub API unreachable ({e}); using pinned release {FALLBACK_TAG} \
                 with constructed asset names"
            ));
            (FALLBACK_TAG.to_string(), None)
        }
    };
    // Reroute Intel Macs off the release whose x86_64 archives are mislabeled
    // (see BROKEN_INTEL_MAC_TAG); asset names for the replacement release are
    // constructed, and its own published checksum manifest is fetched below.
    let (tag, listed_names) = if triple == "x86_64-apple-darwin" && tag == BROKEN_INTEL_MAC_TAG {
        progress(&format!(
            "release {tag} ships arm64 binaries in its x86_64-apple-darwin archives; \
             installing {INTEL_MAC_FALLBACK_TAG} instead"
        ));
        (INTEL_MAC_FALLBACK_TAG.to_string(), None)
    } else {
        (tag, listed_names)
    };
    progress(&format!("release: {tag}"));

    // Checksum manifest (best effort to FETCH; a fetched manifest is then
    // authoritative, a listed hash that mismatches always aborts).
    let manifest = match curl_bytes(&download_url(&tag, &checksum_asset_name(&tag))) {
        Ok(bytes) => Some(String::from_utf8_lossy(&bytes).into_owned()),
        Err(e) => {
            progress(&format!(
                "checksum manifest not fetched ({e}); will install WITHOUT hash \
                 verification (the esp32-machine check still applies)"
            ));
            None
        }
    };

    let mut assets = Vec::new();
    for &arch in arches {
        let asset = match &listed_names {
            Some(names) => {
                pick_asset(names.iter().map(String::as_str), arch, triple).ok_or_else(|| {
                    anyhow::anyhow!(
                        "release {tag} publishes no {} asset for {triple}; pick one \
                         manually from https://github.com/{ESP_QEMU_REPO}/releases/tag/{tag}",
                        tool_name(arch)
                    )
                })?
            }
            None => constructed_asset_name(arch, &tag, triple),
        };
        let sha256 = manifest.as_deref().and_then(|m| checksum_for(m, &asset));
        assets.push(PlannedAsset {
            arch,
            url: download_url(&tag, &asset),
            asset,
            sha256,
        });
    }
    Ok(InstallPlan { tag, assets })
}

/// sha256 of a file via the system tool (`sha256sum`, else `shasum -a 256`).
fn sha256_file(path: &Path) -> Result<String> {
    for (bin, args) in [("sha256sum", vec![]), ("shasum", vec!["-a", "256"])] {
        let out = Command::new(bin).args(&args).arg(path).output();
        if let Ok(o) = out {
            if o.status.success() {
                let text = String::from_utf8_lossy(&o.stdout);
                if let Some(hash) = text.split_whitespace().next() {
                    return Ok(hash.to_ascii_lowercase());
                }
            }
        }
    }
    bail!("neither sha256sum nor shasum is available to verify the download")
}

/// Unpack one downloaded release archive into `root`. The archive carries a
/// top-level `qemu/` directory, so this yields `<root>/qemu/bin/...`. The
/// system `tar` sniffs the compression (xz today, bz2 historically).
pub fn unpack_archive(archive: &Path, root: &Path) -> Result<()> {
    validate_archive_members(archive)?;
    std::fs::create_dir_all(root).with_context(|| format!("creating {}", root.display()))?;
    let out = Command::new("tar")
        .arg("xf")
        .arg(archive)
        .arg("-C")
        .arg(root)
        .output()
        .context("running tar (is tar installed?)")?;
    if !out.status.success() {
        bail!(
            "tar failed unpacking {}: {}",
            archive.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Reject traversal and link/device members before the system tar sees them.
/// The release archive is checksum-verified when upstream publishes a digest,
/// but a reviewed malformed archive must still not write outside staging.
fn validate_archive_members(archive: &Path) -> Result<()> {
    let names = Command::new("tar")
        .args(["tf"])
        .arg(archive)
        .output()
        .context("listing archive members with tar")?;
    if !names.status.success() {
        bail!(
            "tar could not list {}: {}",
            archive.display(),
            String::from_utf8_lossy(&names.stderr).trim()
        );
    }
    for raw in String::from_utf8_lossy(&names.stdout).lines() {
        let normalized = raw.trim_start_matches("./");
        let path = Path::new(normalized);
        if raw.starts_with('/')
            || raw.starts_with('\\')
            || path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            bail!("unsafe path in {}: {raw}", archive.display());
        }
    }

    let verbose = Command::new("tar")
        .args(["tvf"])
        .arg(archive)
        .output()
        .context("inspecting archive member types with tar")?;
    if !verbose.status.success() {
        bail!(
            "tar could not inspect member types in {}",
            archive.display()
        );
    }
    for line in String::from_utf8_lossy(&verbose.stdout).lines() {
        match line.as_bytes().first().copied() {
            Some(b'-' | b'd') => {}
            Some(kind) => bail!(
                "unsafe archive member type '{}' in {} (links and special files are refused)",
                kind as char,
                archive.display()
            ),
            None => {}
        }
    }
    Ok(())
}

/// Post-unpack acceptance for one arch under `root`: the binary must exist at
/// the discovery path AND pass the same Espressif-fork machine-list check
/// `find_qemu` applies. No half-installs: a failure names what to fix.
pub fn verify_installed(arch: QemuArch, root: &Path) -> Result<PathBuf> {
    let bin = root.join("qemu/bin").join(arch.binary_name());
    if !bin.is_file() {
        bail!(
            "{} missing after unpack (expected {}); the archive layout may have \
             changed upstream",
            arch.binary_name(),
            bin.display()
        );
    }
    // The archive should carry the executable bit; enforce it anyway.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perm = std::fs::metadata(&bin)?.permissions();
        perm.set_mode(perm.mode() | 0o755);
        std::fs::set_permissions(&bin, perm)?;
    }
    if !is_esp_fork(&bin) {
        bail!(
            "{} unpacked but its esp32 machine-list check failed. On a fresh \
             machine this is usually a missing shared library; run\n  {} -machine help\n\
             to see the loader error (on macOS: brew install the named library).",
            arch.binary_name(),
            bin.display()
        );
    }
    Ok(bin)
}

/// Download + verify + unpack one planned asset into `root`, returning the
/// accepted binary path.
fn install_one(
    planned: &PlannedAsset,
    root: &Path,
    progress: &mut dyn FnMut(&str),
) -> Result<PathBuf> {
    let staging = tempfile_dir(root)?;
    let archive = staging.join(&planned.asset);
    progress(&format!("downloading {} ...", planned.url));
    let bytes = curl_bytes(&planned.url)?;
    std::fs::write(&archive, &bytes).with_context(|| format!("writing {}", archive.display()))?;

    match &planned.sha256 {
        Some(expected) => {
            let got = sha256_file(&archive)?;
            if &got != expected {
                bail!(
                    "sha256 MISMATCH for {}: manifest says {expected}, downloaded \
                     file is {got}. Refusing to install it.",
                    planned.asset
                );
            }
            progress("sha256 verified against the release's checksum manifest");
        }
        None => progress(
            "no checksum available for this asset; relying on TLS + the \
             esp32-machine check",
        ),
    }

    unpack_archive(&archive, root)?;
    let bin = verify_installed(planned.arch, root)?;
    // Best-effort cleanup of the staging dir; the install already succeeded.
    let _ = std::fs::remove_dir_all(&staging);
    progress(&format!("installed {}", bin.display()));
    Ok(bin)
}

/// A private staging dir under the install root (same filesystem, so no
/// cross-device surprises; created fresh per run).
fn tempfile_dir(root: &Path) -> Result<PathBuf> {
    let dir = root.join(format!(".staging-{}", std::process::id()));
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating staging dir {}", dir.display()))?;
    Ok(dir)
}

struct InstallFsLock {
    dir: PathBuf,
    owner: String,
}

impl InstallFsLock {
    fn acquire(parent: &Path) -> Result<Self> {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating install parent {}", parent.display()))?;
        let dir = parent.join(".hauksbee-qemu-esp.install.lock");
        let owner = std::process::id().to_string();
        for _ in 0..2 {
            match std::fs::create_dir(&dir) {
                Ok(()) => {
                    if let Err(error) = std::fs::write(dir.join("pid"), &owner) {
                        let _ = std::fs::remove_dir_all(&dir);
                        return Err(error).context("writing QEMU install lock owner");
                    }
                    return Ok(Self { dir, owner });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let recorded = std::fs::read_to_string(dir.join("pid")).unwrap_or_default();
                    if recorded.trim().is_empty() {
                        let age = std::fs::metadata(&dir)
                            .and_then(|meta| meta.modified())
                            .ok()
                            .and_then(|modified| modified.elapsed().ok())
                            .unwrap_or_default();
                        if age < std::time::Duration::from_secs(30) {
                            bail!(
                                "another Hauksbee process is initializing the Espressif QEMU install lock; wait and retry"
                            );
                        }
                    }
                    if process_is_alive(recorded.trim()) {
                        bail!(
                            "another Hauksbee process ({}) is installing Espressif QEMU; wait for it to finish",
                            recorded.trim()
                        );
                    }
                    let stale = parent.join(format!(
                        ".hauksbee-qemu-esp.install.lock.stale-{}",
                        transaction_nonce()
                    ));
                    std::fs::rename(&dir, &stale).context("reclaiming stale QEMU install lock")?;
                    let _ = std::fs::remove_dir_all(stale);
                }
                Err(error) => return Err(error).context("acquiring QEMU install lock"),
            }
        }
        bail!("could not acquire the Espressif QEMU install lock")
    }
}

impl Drop for InstallFsLock {
    fn drop(&mut self) {
        let recorded = std::fs::read_to_string(self.dir.join("pid")).unwrap_or_default();
        if recorded.trim() == self.owner {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

#[cfg(unix)]
fn process_is_alive(pid: &str) -> bool {
    let Ok(pid) = pid.parse::<i32>() else {
        return false;
    };
    // SAFETY: kill(pid, 0) sends no signal; it is the POSIX existence probe.
    let result = unsafe { libc::kill(pid, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn process_is_alive(pid: &str) -> bool {
    !pid.is_empty()
}

fn transaction_nonce() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{}-{nanos}", std::process::id())
}

fn transaction_dirs(parent: &Path, prefix: &str) -> Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(parent)
        .with_context(|| format!("reading install parent {}", parent.display()))?
    {
        let entry = entry?;
        if entry.file_name().to_string_lossy().starts_with(prefix) {
            found.push(entry.path());
        }
    }
    found.sort();
    Ok(found)
}

fn recover_interrupted_install(
    parent: &Path,
    root: &Path,
    progress: &mut dyn FnMut(&str),
) -> Result<()> {
    for candidate in transaction_dirs(parent, ".hauksbee-qemu-esp.candidate-")? {
        std::fs::remove_dir_all(&candidate)
            .with_context(|| format!("removing stale candidate {}", candidate.display()))?;
    }
    let backups = transaction_dirs(parent, ".hauksbee-qemu-esp.backup-")?;
    if root.exists() {
        for backup in backups {
            std::fs::remove_dir_all(&backup)
                .with_context(|| format!("removing committed backup {}", backup.display()))?;
        }
    } else if backups.len() == 1 {
        progress("recovering the previous QEMU install after an interrupted swap");
        std::fs::rename(&backups[0], root).context("restoring interrupted QEMU install")?;
    } else if backups.len() > 1 {
        bail!(
            "multiple interrupted QEMU backups exist under {}; refusing to guess",
            parent.display()
        );
    }
    Ok(())
}

/// Install the Espressif QEMU fork for `arches` into `~/.hauksbee-qemu-esp/`.
///
/// Skips an arch whose binary discovery already accepts (idempotent). Returns
/// the accepted binary paths in `arches` order. On any failure the error
/// carries the manual-install pointer; nothing is left half-accepted (the
/// fork check gates every binary).
pub fn install_esp_qemu(
    arches: &[QemuArch],
    progress: &mut dyn FnMut(&str),
) -> Result<Vec<PathBuf>> {
    let root = install_root()?;
    let missing: Vec<QemuArch> = arches
        .iter()
        .copied()
        .filter(|&a| {
            if let Ok(existing) = super::find_qemu(a) {
                progress(&format!(
                    "{} already discoverable at {}; skipping",
                    a.binary_name(),
                    existing.display()
                ));
                false
            } else {
                true
            }
        })
        .collect();
    if missing.is_empty() {
        return arches.iter().map(|&a| super::find_qemu(a)).collect();
    }

    // If either requested architecture is missing, rebuild the complete
    // requested tree in a sibling candidate. Extracting into the live prefix
    // would let a failed second archive corrupt an otherwise working install.
    // The two archives merge into one `qemu/` tree. Rebuild both whenever the
    // tree is incomplete so swapping the candidate cannot discard a sibling
    // architecture that happened to be installed already.
    let complete_arches = [QemuArch::Xtensa, QemuArch::Riscv32];
    let plan = plan(&complete_arches, progress)?;
    let parent = root
        .parent()
        .ok_or_else(|| anyhow::anyhow!("install root has no parent: {}", root.display()))?;
    let _lock = InstallFsLock::acquire(parent)?;
    recover_interrupted_install(parent, &root, progress)?;
    let nonce = transaction_nonce();
    let candidate = parent.join(format!(".hauksbee-qemu-esp.candidate-{nonce}"));
    let backup = parent.join(format!(".hauksbee-qemu-esp.backup-{nonce}"));
    std::fs::create_dir_all(&candidate)
        .with_context(|| format!("creating install candidate {}", candidate.display()))?;
    for planned in &plan.assets {
        if let Err(error) = install_one(planned, &candidate, progress) {
            let _ = std::fs::remove_dir_all(&candidate);
            return Err(error);
        }
    }
    for &arch in &complete_arches {
        if let Err(error) = verify_installed(arch, &candidate) {
            let _ = std::fs::remove_dir_all(&candidate);
            return Err(error);
        }
    }

    if root.exists() {
        if let Err(error) = std::fs::rename(&root, &backup) {
            let _ = std::fs::remove_dir_all(&candidate);
            return Err(error).with_context(|| {
                format!(
                    "moving existing install {} to {}",
                    root.display(),
                    backup.display()
                )
            });
        }
    }
    if let Err(error) = std::fs::rename(&candidate, &root) {
        if backup.exists() {
            let _ = std::fs::rename(&backup, &root);
        }
        let _ = std::fs::remove_dir_all(&candidate);
        return Err(error).with_context(|| format!("committing install to {}", root.display()));
    }
    // Final acceptance through the discovery path itself, per requested arch.
    let accepted: Result<Vec<PathBuf>> = arches
        .iter()
        .map(|&a| {
            super::find_qemu(a).with_context(|| {
                format!("{} still not discoverable after install", a.binary_name())
            })
        })
        .collect();
    match accepted {
        Ok(paths) => {
            let _ = std::fs::remove_dir_all(&backup);
            Ok(paths)
        }
        Err(error) => {
            // Acceptance is part of the transaction. Keep the old tree until
            // the normal discovery path has accepted the committed candidate,
            // then restore it if that final probe rejects the new tree.
            let _ = std::fs::remove_dir_all(&root);
            if backup.exists() {
                std::fs::rename(&backup, &root).with_context(|| {
                    format!(
                        "restoring {} after final QEMU acceptance failed: {error:#}",
                        root.display()
                    )
                })?;
            }
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The exact asset list published by the live esp-develop-9.2.2-20260417
    // release (fetched 2026-07-10), so selection is pinned against reality.
    const LIVE_ASSETS: &[&str] = &[
        "qemu-esp_develop_9.2.2_20260417-checksum.sha256",
        "qemu-esp_develop_9.2.2_20260417-src.tar.xz",
        "qemu-riscv32-softmmu-esp_develop_9.2.2_20260417-aarch64-apple-darwin.tar.xz",
        "qemu-riscv32-softmmu-esp_develop_9.2.2_20260417-aarch64-linux-gnu.tar.xz",
        "qemu-riscv32-softmmu-esp_develop_9.2.2_20260417-x86_64-apple-darwin.tar.xz",
        "qemu-riscv32-softmmu-esp_develop_9.2.2_20260417-x86_64-linux-gnu.tar.xz",
        "qemu-riscv32-softmmu-esp_develop_9.2.2_20260417-x86_64-w64-mingw32.tar.xz",
        "qemu-xtensa-softmmu-esp_develop_9.2.2_20260417-aarch64-apple-darwin.tar.xz",
        "qemu-xtensa-softmmu-esp_develop_9.2.2_20260417-aarch64-linux-gnu.tar.xz",
        "qemu-xtensa-softmmu-esp_develop_9.2.2_20260417-x86_64-apple-darwin.tar.xz",
        "qemu-xtensa-softmmu-esp_develop_9.2.2_20260417-x86_64-linux-gnu.tar.xz",
        "qemu-xtensa-softmmu-esp_develop_9.2.2_20260417-x86_64-w64-mingw32.tar.xz",
    ];

    #[test]
    fn picks_the_right_live_asset_per_arch_and_triple() {
        for (arch, triple, want) in [
            (
                QemuArch::Xtensa,
                "aarch64-apple-darwin",
                "qemu-xtensa-softmmu-esp_develop_9.2.2_20260417-aarch64-apple-darwin.tar.xz",
            ),
            (
                QemuArch::Riscv32,
                "x86_64-linux-gnu",
                "qemu-riscv32-softmmu-esp_develop_9.2.2_20260417-x86_64-linux-gnu.tar.xz",
            ),
        ] {
            assert_eq!(
                pick_asset(LIVE_ASSETS.iter().copied(), arch, triple).as_deref(),
                Some(want)
            );
        }
        // The src tarball and checksum manifest must never be picked.
        assert_eq!(
            pick_asset(
                ["qemu-esp_develop_9.2.2_20260417-src.tar.xz"],
                QemuArch::Xtensa,
                "aarch64-apple-darwin"
            ),
            None
        );
    }

    #[test]
    fn pick_asset_survives_historical_bz2_naming() {
        // Older releases shipped .tar.bz2 with hyphenated versions; the shape
        // filter accepts both.
        let old = ["qemu-xtensa-softmmu-esp-develop-8.0.0-20230522-x86_64-linux-gnu.tar.bz2"];
        assert_eq!(
            pick_asset(old, QemuArch::Xtensa, "x86_64-linux-gnu").as_deref(),
            Some(old[0])
        );
    }

    #[test]
    fn constructed_fallback_matches_the_live_naming_convention() {
        assert_eq!(
            constructed_asset_name(
                QemuArch::Xtensa,
                "esp-develop-9.2.2-20260417",
                "aarch64-apple-darwin"
            ),
            "qemu-xtensa-softmmu-esp_develop_9.2.2_20260417-aarch64-apple-darwin.tar.xz"
        );
        assert_eq!(
            checksum_asset_name("esp-develop-9.2.2-20260417"),
            "qemu-esp_develop_9.2.2_20260417-checksum.sha256"
        );
    }

    #[test]
    fn checksum_manifest_parses_the_live_format() {
        // Verbatim lines from the live release's checksum asset.
        let manifest = "\
# qemu-xtensa-softmmu-esp_develop_9.2.2_20260417-aarch64-apple-darwin.tar.xz: 3867936 bytes
bb8c15810565d3df1665dc34962430885e11bc95575b228fb44698146be1e9d6 *qemu-xtensa-softmmu-esp_develop_9.2.2_20260417-aarch64-apple-darwin.tar.xz
# qemu-riscv32-softmmu-esp_develop_9.2.2_20260417-x86_64-linux-gnu.tar.xz: 16842920 bytes
547f03e04701a92cbb699f7f7d015adc1f5b5ef93cbb94c0dd9b7107e2d84e77 *qemu-riscv32-softmmu-esp_develop_9.2.2_20260417-x86_64-linux-gnu.tar.xz
";
        assert_eq!(
            checksum_for(
                manifest,
                "qemu-xtensa-softmmu-esp_develop_9.2.2_20260417-aarch64-apple-darwin.tar.xz"
            )
            .as_deref(),
            Some("bb8c15810565d3df1665dc34962430885e11bc95575b228fb44698146be1e9d6")
        );
        assert_eq!(checksum_for(manifest, "not-an-asset.tar.xz"), None);
    }

    #[test]
    fn json_scanner_reads_github_api_shapes() {
        // Both pretty-printed (real API) and compact forms.
        let pretty = "{\n  \"tag_name\": \"esp-develop-9.2.2-20260417\",\n  \"assets\": [\n    { \"name\": \"a.tar.xz\" },\n    { \"name\": \"b.tar.xz\" }\n  ]\n}";
        assert_eq!(
            json_string_values(pretty, "tag_name"),
            vec!["esp-develop-9.2.2-20260417"]
        );
        assert_eq!(
            json_string_values(pretty, "name"),
            vec!["a.tar.xz", "b.tar.xz"]
        );
        let compact = "{\"tag_name\":\"v1\",\"name\":\"x\"}";
        assert_eq!(json_string_values(compact, "tag_name"), vec!["v1"]);
    }
}
