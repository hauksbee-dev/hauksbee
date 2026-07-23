//! Resolve a firmware *input* — whatever the user handed us — into a bootable
//! image the MCU backends can load.
//!
//! The co-sim loaders want one compiled `.elf`/`.hex`. Users have a PlatformIO
//! project, an ESP-IDF build tree, or a zip of either, and should not need to
//! know that the artifact lives at `.pio/build/<env>/firmware.elf`. This module
//! closes that gap in three tiers:
//!
//! 1. **Pass-through**: anything that is not a zip archive is handed to the
//!    loaders untouched (they sniff ELF/hex themselves).
//! 2. **Archive search**: a `.zip` is searched for built images. A
//!    `.pio/build/<env>/firmware.elf` beats a stray `.elf`, which beats a
//!    `.hex`; ties go to the newest entry. The choice is reported so the user
//!    can see which image ran.
//! 3. **Project build**: a zip (or, on the CLI, a directory) that carries a
//!    `platformio.ini` but no built image is built with the user's own `pio`
//!    (detect-don't-bundle, exactly like the Renode / ngspice / kicad-cli
//!    oracles). A missing `pio` or a failing build is a loud, actionable error
//!    — never a silent fallback.
//!
//! Asymmetry, stated on purpose: a CLI **directory** with a `platformio.ini`
//! is always (re)built — it is the user's live project and `pio run` is
//! incremental — while a **zip** prefers an image it already contains, because
//! an upload should not kick off a multi-minute toolchain download when the
//! snapshot already carries the artifact.
//!
//! Building a project executes its build scripts (`extra_scripts` in
//! `platformio.ini` is arbitrary code). That is the nature of building
//! software and this server is localhost-only, but it is why the web tier
//! never builds anything the user did not explicitly upload.

use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};

/// A firmware input resolved to a bootable image (web / bytes tier).
#[derive(Debug)]
pub struct ResolvedFirmware {
    /// File name of the resolved image (drives the loaders' format sniff).
    pub name: String,
    pub bytes: Vec<u8>,
    /// Provenance when the input was not already a bare image ("picked
    /// `.pio/build/uno/firmware.elf` from the archive", "built with `pio run`
    /// (env uno)"). `None` for a pass-through.
    pub note: Option<String>,
}

/// A firmware input resolved to a file on disk (CLI tier).
pub struct ResolvedFirmwareFile {
    pub path: PathBuf,
    pub note: String,
}

const ZIP_MAGIC: &[u8] = b"PK\x03\x04";

/// The `pio` binary to invoke. `HAUKSBEE_PIO` overrides the PATH lookup (used
/// by tests to prove the missing-pio error path; handy if pio lives off PATH).
fn pio_bin() -> String {
    std::env::var("HAUKSBEE_PIO").unwrap_or_else(|_| "pio".to_string())
}

/// Resolve uploaded firmware bytes (the web drop zone's firmware part).
///
/// Non-zip inputs pass through untouched. Zips resolve to the best built image
/// inside, or — if the archive is a PlatformIO project with no built image —
/// to a `pio run` build of it. Errors are user-facing strings (they land in
/// the co-sim card verbatim).
pub fn resolve_firmware_bytes(fw_name: &str, fw_bytes: &[u8]) -> Result<ResolvedFirmware, String> {
    if !fw_bytes.starts_with(ZIP_MAGIC) {
        return Ok(ResolvedFirmware {
            name: fw_name.to_string(),
            bytes: fw_bytes.to_vec(),
            note: None,
        });
    }

    let mut archive = zip::ZipArchive::new(Cursor::new(fw_bytes))
        .map_err(|e| format!("could not open '{fw_name}' as a zip archive: {e}"))?;

    // Tier 2: an already-built image inside the archive.
    if let Some(best) = best_image_entry(&mut archive) {
        let mut file = archive
            .by_index(best.index)
            .map_err(|e| format!("could not read '{}' from the archive: {e}", best.entry_name))?;
        let mut bytes = Vec::with_capacity(file.size() as usize);
        file.read_to_end(&mut bytes)
            .map_err(|e| format!("could not read '{}' from the archive: {e}", best.entry_name))?;
        let name = Path::new(&best.entry_name)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("firmware.elf")
            .to_string();
        return Ok(ResolvedFirmware {
            name,
            bytes,
            note: Some(format!(
                "Ran the built image '{}' found inside the uploaded archive{}.",
                best.entry_name,
                if best.had_rivals {
                    " (it outranked the other .elf/.hex entries)"
                } else {
                    ""
                }
            )),
        });
    }

    // Tier 3: a PlatformIO project snapshot — extract and build it.
    if archive_has_platformio_ini(&mut archive) {
        let dir = extract_zip_to_temp(&mut archive, fw_name)?;
        let project = find_platformio_project(&dir).ok_or_else(|| {
            "the archive contains a platformio.ini but it could not be located after extraction"
                .to_string()
        })?;
        let (artifact, note) = pio_build(&project)?;
        let bytes = std::fs::read(&artifact)
            .map_err(|e| format!("could not read the built image '{}': {e}", artifact.display()))?;
        let name = artifact
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("firmware.elf")
            .to_string();
        return Ok(ResolvedFirmware { name, bytes, note: Some(note) });
    }

    Err(format!(
        "'{fw_name}' is a zip but contains no built firmware (.elf / .hex) and no \
         platformio.ini to build one from. Upload the compiled image directly — for \
         PlatformIO that is .pio/build/<env>/firmware.elf inside your project."
    ))
}

/// Resolve a CLI `--firmware` path that is a directory or a zip. Returns
/// `Ok(None)` for a plain file that is not a zip — the caller keeps its
/// existing path untouched.
pub fn resolve_firmware_cli(path: &Path) -> anyhow::Result<Option<ResolvedFirmwareFile>> {
    if path.is_dir() {
        // A live project directory: build it (pio run is incremental), or fall
        // back to an existing .pio/build artifact when there is no ini at all.
        if find_platformio_project(path).is_some() {
            let project = find_platformio_project(path).unwrap();
            let (artifact, note) = pio_build(&project).map_err(anyhow::Error::msg)?;
            return Ok(Some(ResolvedFirmwareFile { path: artifact, note }));
        }
        if let Some((artifact, env)) = newest_pio_artifact(path) {
            return Ok(Some(ResolvedFirmwareFile {
                note: format!(
                    "using the already-built image {} (env {env})",
                    artifact.display()
                ),
                path: artifact,
            }));
        }
        anyhow::bail!(
            "--firmware points at the directory '{}', but it has no platformio.ini to \
             build and no .pio/build/<env>/firmware.elf|.hex already built. Point at the \
             compiled image, or at a PlatformIO project.",
            path.display()
        );
    }
    let is_zip = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("zip"));
    if is_zip {
        let bytes = std::fs::read(path)?;
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("firmware.zip");
        let resolved = resolve_firmware_bytes(name, &bytes).map_err(anyhow::Error::msg)?;
        // The loaders want a file: park the resolved image next to the other
        // hauksbee temp artifacts.
        let dir = std::env::temp_dir().join(format!("hauksbee-fw-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        let out = dir.join(&resolved.name);
        std::fs::write(&out, &resolved.bytes)?;
        return Ok(Some(ResolvedFirmwareFile {
            path: out,
            note: resolved.note.unwrap_or_else(|| "resolved from the zip".to_string()),
        }));
    }
    Ok(None)
}

struct ImageEntry {
    index: usize,
    entry_name: String,
    had_rivals: bool,
}

/// Rank the archive's built images: `.pio/build/**/firmware.elf` (rank 0) >
/// any other `.elf` (1) > `.pio/build/**/firmware.hex` (2) > any `.hex` (3).
/// Ties go to the newest entry timestamp. macOS resource-fork noise
/// (`__MACOSX/`, dotfiles) is ignored.
fn best_image_entry<R: Read + std::io::Seek>(archive: &mut zip::ZipArchive<R>) -> Option<ImageEntry> {
    let mut candidates: Vec<(u8, i64, usize, String)> = Vec::new();
    for i in 0..archive.len() {
        let entry = match archive.by_index(i) {
            Ok(e) => e,
            Err(_) => continue,
        };
        if entry.is_dir() {
            continue;
        }
        let name = entry.name().to_string();
        if name.starts_with("__MACOSX/") || name.rsplit('/').next().is_some_and(|f| f.starts_with('.')) {
            continue;
        }
        let lower = name.to_ascii_lowercase();
        let in_pio_build = lower.contains(".pio/build/");
        let rank = if lower.ends_with(".elf") {
            if in_pio_build { 0 } else { 1 }
        } else if lower.ends_with(".hex") {
            if in_pio_build { 2 } else { 3 }
        } else {
            continue;
        };
        // Newest wins within a rank; the zip DateTime collapses to a sortable
        // scalar (seconds precision is plenty for "which build is fresher").
        let stamp = entry
            .last_modified()
            .map(|dt| {
                (dt.year() as i64) * 33177600
                    + (dt.month() as i64) * 2764800
                    + (dt.day() as i64) * 86400
                    + (dt.hour() as i64) * 3600
                    + (dt.minute() as i64) * 60
                    + dt.second() as i64
            })
            .unwrap_or(0);
        candidates.push((rank, -stamp, i, name));
    }
    let total = candidates.len();
    candidates.sort();
    candidates.into_iter().next().map(|(_, _, index, entry_name)| ImageEntry {
        index,
        entry_name,
        had_rivals: total > 1,
    })
}

fn archive_has_platformio_ini<R: Read + std::io::Seek>(archive: &mut zip::ZipArchive<R>) -> bool {
    (0..archive.len()).any(|i| {
        archive
            .by_index(i)
            .map(|e| {
                !e.name().starts_with("__MACOSX/")
                    && e.name().rsplit('/').next() == Some("platformio.ini")
            })
            .unwrap_or(false)
    })
}

/// Extract the whole archive under a fresh temp dir. Entry paths go through
/// `enclosed_name` so a hostile archive cannot write outside it (zip-slip).
fn extract_zip_to_temp<R: Read + std::io::Seek>(
    archive: &mut zip::ZipArchive<R>,
    fw_name: &str,
) -> Result<PathBuf, String> {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "hauksbee-pio-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).map_err(|e| format!("could not create a temp dir: {e}"))?;
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| format!("could not read entry {i} of '{fw_name}': {e}"))?;
        let Some(rel) = entry.enclosed_name() else {
            continue; // absolute or ..-escaping path: refuse silently, never write it
        };
        let out = dir.join(rel);
        if entry.is_dir() {
            std::fs::create_dir_all(&out).map_err(|e| format!("extract failed: {e}"))?;
            continue;
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("extract failed: {e}"))?;
        }
        let mut f = std::fs::File::create(&out)
            .map_err(|e| format!("extract of '{}' failed: {e}", out.display()))?;
        std::io::copy(&mut entry, &mut f)
            .map_err(|e| format!("extract of '{}' failed: {e}", out.display()))?;
    }
    Ok(dir)
}

/// Find the directory holding `platformio.ini`: the given dir, or the
/// shallowest child within 3 levels (zips usually wrap the project in one
/// top-level folder).
fn find_platformio_project(root: &Path) -> Option<PathBuf> {
    fn walk(dir: &Path, depth: usize) -> Option<PathBuf> {
        if dir.join("platformio.ini").is_file() {
            return Some(dir.to_path_buf());
        }
        if depth == 0 {
            return None;
        }
        let mut subdirs: Vec<PathBuf> = std::fs::read_dir(dir)
            .ok()?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.is_dir() && p.file_name().is_some_and(|n| n != ".pio" && n != "__MACOSX"))
            .collect();
        subdirs.sort();
        subdirs.iter().find_map(|d| walk(d, depth - 1))
    }
    walk(root, 3)
}

/// Build a PlatformIO project with the user's own toolchain and return the
/// built image. Detect-don't-bundle: a missing `pio` and a failing build are
/// both loud errors that say exactly what to do.
fn pio_build(project: &Path) -> Result<(PathBuf, String), String> {
    let bin = pio_bin();
    let out = std::process::Command::new(&bin)
        .arg("run")
        .arg("-d")
        .arg(project)
        .output();
    let out = match out {
        Ok(o) => o,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(format!(
                "this looks like a PlatformIO project, but PlatformIO is not installed \
                 (no `{bin}` on PATH). Install it (`brew install platformio` or \
                 `uv tool install platformio`) and retry — or hand over the compiled \
                 image directly: .pio/build/<env>/firmware.elf inside your project."
            ));
        }
        Err(e) => return Err(format!("could not run `{bin} run`: {e}")),
    };
    if !out.status.success() {
        let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&out.stderr));
        let tail: Vec<&str> = text.lines().rev().take(25).collect();
        let tail: Vec<&str> = tail.into_iter().rev().collect();
        return Err(format!(
            "`pio run` failed for the uploaded project. Last lines of the build output:\n{}",
            tail.join("\n")
        ));
    }
    let (artifact, env) = newest_pio_artifact(project).ok_or_else(|| {
        "`pio run` succeeded but produced no .pio/build/<env>/firmware.elf|.hex".to_string()
    })?;
    // Prefer the env the project itself declares, if it built an artifact too.
    let chosen = default_env(project)
        .and_then(|denv| {
            let p = project.join(".pio/build").join(&denv);
            ["firmware.elf", "firmware.hex"]
                .iter()
                .map(|f| p.join(f))
                .find(|p| p.is_file())
                .map(|p| (p, denv))
        })
        .unwrap_or((artifact, env));
    let note = format!("Built with `pio run` (env {}).", chosen.1);
    Ok((chosen.0, note))
}

/// The newest `firmware.elf|.hex` under `<project>/.pio/build/<env>/`, with its
/// env name. `.elf` beats `.hex` within an env.
fn newest_pio_artifact(project: &Path) -> Option<(PathBuf, String)> {
    let build = project.join(".pio").join("build");
    let mut best: Option<(std::time::SystemTime, PathBuf, String)> = None;
    for entry in std::fs::read_dir(&build).ok()? {
        let env_dir = entry.ok()?.path();
        if !env_dir.is_dir() {
            continue;
        }
        let env = env_dir.file_name()?.to_str()?.to_string();
        for name in ["firmware.elf", "firmware.hex"] {
            let p = env_dir.join(name);
            if let Ok(meta) = p.metadata() {
                let t = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
                if best.as_ref().is_none_or(|(bt, _, _)| t > *bt) {
                    best = Some((t, p, env.clone()));
                }
                break; // .elf found: don't let the same env's .hex shadow it
            }
        }
    }
    best.map(|(_, p, e)| (p, e))
}

/// `default_envs` from `platformio.ini` (first entry when comma-separated).
fn default_env(project: &Path) -> Option<String> {
    let ini = std::fs::read_to_string(project.join("platformio.ini")).ok()?;
    for line in ini.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("default_envs") {
            let value = rest.trim_start_matches([' ', '\t']).strip_prefix('=')?;
            let first = value.split(',').next()?.trim();
            if !first.is_empty() {
                return Some(first.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    fn zip_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, bytes) in entries {
            w.start_file(*name, SimpleFileOptions::default()).unwrap();
            w.write_all(bytes).unwrap();
        }
        w.finish().unwrap().into_inner()
    }

    #[test]
    fn non_zip_passes_through_untouched() {
        let elf = b"\x7fELF fake image";
        let r = resolve_firmware_bytes("app.elf", elf).unwrap();
        assert_eq!(r.name, "app.elf");
        assert_eq!(r.bytes, elf);
        assert!(r.note.is_none(), "pass-through carries no provenance note");
    }

    #[test]
    fn zip_resolves_the_pio_build_image_over_a_stray_elf() {
        let z = zip_of(&[
            ("project/src/main.cpp", b"int main(){}"),
            ("project/stray/other.elf", b"\x7fELF stray"),
            ("project/.pio/build/uno/firmware.elf", b"\x7fELF built"),
        ]);
        let r = resolve_firmware_bytes("project.zip", &z).unwrap();
        assert_eq!(r.name, "firmware.elf");
        assert_eq!(r.bytes, b"\x7fELF built");
        let note = r.note.expect("archive resolution is reported");
        assert!(note.contains(".pio/build/uno/firmware.elf"), "note names the entry: {note}");
    }

    #[test]
    fn zip_falls_back_to_hex_and_skips_macos_noise() {
        let z = zip_of(&[
            ("__MACOSX/._firmware.elf", b"resource fork junk"),
            ("build/blink.hex", b":00000001FF\n"),
        ]);
        let r = resolve_firmware_bytes("fw.zip", &z).unwrap();
        assert_eq!(r.name, "blink.hex");
        assert_eq!(r.bytes, b":00000001FF\n");
    }

    #[test]
    fn zip_with_nothing_useful_is_a_clear_error() {
        let z = zip_of(&[("README.md", b"# not firmware")]);
        let err = resolve_firmware_bytes("fw.zip", &z).unwrap_err();
        assert!(err.contains(".elf"), "error names what was looked for: {err}");
        assert!(err.contains("platformio.ini"), "and the build fallback: {err}");
    }

    #[test]
    fn pio_project_zip_without_pio_installed_says_how_to_get_it() {
        let z = zip_of(&[
            ("blink/platformio.ini", b"[env:uno]\nplatform = atmelavr\nboard = uno\n"),
            ("blink/src/main.cpp", b"int main(){}"),
        ]);
        std::env::set_var("HAUKSBEE_PIO", "/definitely/not/a/real/pio");
        let err = resolve_firmware_bytes("blink.zip", &z).unwrap_err();
        std::env::remove_var("HAUKSBEE_PIO");
        assert!(err.contains("PlatformIO"), "names the missing tool: {err}");
        assert!(err.contains("firmware.elf"), "offers the manual path: {err}");
    }

    /// Real end-to-end `pio run` build. Ignored by default: it needs pio on
    /// PATH and the atmelavr toolchain installed (first run downloads it).
    /// Run with: cargo test -p hauksbee-engine --lib firmware_input -- --ignored
    #[test]
    #[ignore]
    fn real_pio_project_builds_and_resolves() {
        let dir = std::env::temp_dir().join(format!("hauksbee-pio-e2e-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("platformio.ini"),
            "[env:uno]\nplatform = atmelavr\nboard = uno\n",
        )
        .unwrap();
        std::fs::write(dir.join("src/main.c"), "int main(void){for(;;){}}\n").unwrap();
        let r = resolve_firmware_cli(&dir).unwrap().expect("a project dir resolves");
        assert!(r.path.is_file(), "built artifact exists: {}", r.path.display());
        assert!(r.note.contains("pio run"), "note says it built: {}", r.note);
        let bytes = std::fs::read(&r.path).unwrap();
        assert!(bytes.starts_with(b"\x7fELF") || r.path.extension().is_some_and(|e| e == "hex"));
    }
}
