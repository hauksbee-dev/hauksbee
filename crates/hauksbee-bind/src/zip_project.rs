//! A `KiCad` project shipped inside a `.zip` archive.
//!
//! Public designs often publish their `KiCad` project only as a zip. This module
//! is the one place that knows how to read such an archive, so `hauksbee run`,
//! `hauksbee-ci`, `models resolve` and every other caller of
//! [`crate::board_input::from_path`] accept it the same way, as if the archive
//! had been unpacked into a directory.
//!
//! The project is found by the first rule that matches anything:
//!
//! 1. every `.kicad_pro` in the archive is one project;
//! 2. else every root `.kicad_sch` (one no other schematic names in a
//!    `Sheetfile` property) is one project;
//! 3. else every `.kicad_pcb` is one project.
//!
//! Exactly one project is read. Several are refused with their paths, because
//! picking one by archive order would check a design the user did not mean.
//! Once the project is chosen the archive is unpacked into a fresh directory
//! under the OS temp directory, so sub-sheets and project settings sit beside
//! the design exactly as on disk, and the directory is removed when the
//! [`Unpacked`] handle drops.
//!
//! The archive is hostile input. Member paths are checked component by
//! component (no absolute path, no `..`, no drive prefix, no NUL) and an unsafe
//! one refuses the whole archive; symlinks are never created; the member count
//! and the bytes actually written (not the sizes the archive declares) are
//! capped.

// Same rule as `board_input`: a panic on a user's archive is a denial of
// service for `hauksbee serve`, so every failure here is a typed error.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use std::io::{Read, Seek};
use std::path::{Path, PathBuf};

use crate::board_input::BoardInputError;

/// The most members an archive may hold before it is refused.
const MAX_MEMBERS: usize = 20_000;

/// The most an archive may inflate to, in total (512 MiB), whatever it declares.
const MAX_UNPACKED_BYTES: u64 = hauksbee_extract::MAX_INFLATED_BYTES;

/// A project unpacked from an archive. The directory goes when this drops.
#[derive(Debug)]
pub struct Unpacked {
    _dir: tempfile::TempDir,
    /// The design file to read, inside the unpacked directory.
    pub design: PathBuf,
    /// The design's path inside the archive, for the reader note.
    pub member: String,
}

/// One file in the archive, with its path normalised to forward slashes.
#[derive(Debug, Clone)]
struct Member {
    index: usize,
    name: String,
}

impl Member {
    fn dir(&self) -> &str {
        self.name.rfind('/').map_or("", |i| &self.name[..=i])
    }

    fn base(&self) -> &str {
        self.name.rsplit('/').next().unwrap_or(&self.name)
    }

    fn stem(&self) -> &str {
        let base = self.base();
        base.rfind('.').map_or(base, |i| &base[..i])
    }

    fn has_ext(&self, ext: &str) -> bool {
        self.base()
            .rfind('.')
            .is_some_and(|i| self.base()[i + 1..].eq_ignore_ascii_case(ext))
    }
}

/// Whether `path` names a zip by its extension.
pub fn is_zip_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("zip"))
}

/// Unpack the `KiCad` project in a zip, or `None` when the archive holds none
/// (or is not a readable archive; the caller's own zip handling then speaks).
///
/// # Errors
///
/// [`BoardInputError::Zip`] when the archive holds several projects, an unsafe
/// member path, too many members or too many bytes, or the project names no
/// design file.
pub fn unpack<R: Read + Seek>(
    reader: R,
    file_name: &str,
) -> Result<Option<Unpacked>, BoardInputError> {
    unpack_capped(reader, file_name, MAX_UNPACKED_BYTES)
}

/// [`unpack`] with the byte ceiling given, so a test can hit it cheaply.
fn unpack_capped<R: Read + Seek>(
    reader: R,
    file_name: &str,
    cap: u64,
) -> Result<Option<Unpacked>, BoardInputError> {
    let Ok(mut archive) = zip::ZipArchive::new(reader) else {
        return Ok(None);
    };
    let Some(design) = choose(&mut archive, file_name)? else {
        return Ok(None);
    };
    let dir = tempfile::Builder::new()
        .prefix("hauksbee-zip-")
        .tempdir()
        .map_err(|e| {
            zip_err(format!(
                "could not make a temporary directory for '{file_name}': {e}"
            ))
        })?;
    extract_all(&mut archive, dir.path(), file_name, cap)?;
    let mut path = dir.path().to_path_buf();
    path.extend(design.name.split('/').filter(|c| !c.is_empty()));
    Ok(Some(Unpacked {
        _dir: dir,
        design: path,
        member: design.name,
    }))
}

/// The text of a project file that sits beside the design inside a zip
/// (`<stem>.<ext>`, for example `.kicad_pro`), or `None` when `zip_path` is not
/// a zip with a `KiCad` project or the archive has no such file.
///
/// The sibling lookups that read `board.with_extension("kicad_pro")` from disk
/// use this for a zip, so a project's clearance rules and DRC exclusions are
/// read from the archive instead of silently defaulting.
pub fn sibling_text(zip_path: &Path, ext: &str) -> Option<std::io::Result<String>> {
    let file = std::fs::File::open(zip_path).ok()?;
    let mut archive = zip::ZipArchive::new(std::io::BufReader::new(file)).ok()?;
    let name = zip_path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    let design = choose(&mut archive, name).ok().flatten()?;
    let members = list(&mut archive).ok()?;
    let want = members
        .iter()
        .find(|m| m.dir() == design.dir() && m.stem() == design.stem() && m.has_ext(ext))?;
    Some(
        archive
            .by_index(want.index)
            .and_then(|entry| {
                hauksbee_extract::read_member_capped(entry).map_err(zip::result::ZipError::Io)
            })
            .map_err(|e| std::io::Error::other(e.to_string()))
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned()),
    )
}

const fn zip_err(message: String) -> BoardInputError {
    BoardInputError::Zip(message)
}

/// Whether a normalised member path is safe to create under a directory.
fn is_safe(name: &str) -> bool {
    !name.starts_with('/')
        && !name.contains('\0')
        && name
            .split('/')
            .all(|part| part != ".." && !part.contains(':'))
}

/// Every file member, normalised and filtered. Backup trees, autosaves and
/// macOS resource forks are not part of the design and are left out.
///
/// An unsafe path or an oversized archive is an error, not a skip: the archive
/// is refused whole.
fn list<R: Read + Seek>(archive: &mut zip::ZipArchive<R>) -> Result<Vec<Member>, BoardInputError> {
    if archive.len() > MAX_MEMBERS {
        return Err(zip_err(format!(
            "this zip holds {} members, more than the {MAX_MEMBERS} hauksbee will unpack for one design; \
             a real KiCad project is far smaller, so unpack it yourself and point hauksbee at the folder",
            archive.len()
        )));
    }
    let mut out = Vec::new();
    for index in 0..archive.len() {
        let Ok(entry) = archive.by_index_raw(index) else {
            continue;
        };
        if entry.is_dir() {
            continue;
        }
        let name = entry.name().replace('\\', "/");
        if !is_safe(&name) {
            return Err(zip_err(format!(
                "this zip holds a member whose path escapes the folder it would unpack to ('{}'); \
                 hauksbee will not read it",
                name.escape_debug()
            )));
        }
        let name = name
            .split('/')
            .filter(|c| !c.is_empty() && *c != ".")
            .collect::<Vec<_>>()
            .join("/");
        if name.is_empty() || ignored(&name) {
            continue;
        }
        out.push(Member { index, name });
    }
    Ok(out)
}

fn ignored(name: &str) -> bool {
    let mut parts = name.split('/').peekable();
    while let Some(part) = parts.next() {
        let last = parts.peek().is_none();
        let lower = part.to_ascii_lowercase();
        if part == "__MACOSX" || lower.ends_with("-backups") || lower == ".history" {
            return true;
        }
        if last && (part.starts_with('.') || lower.starts_with("_autosave-")) {
            return true;
        }
    }
    false
}

/// A project found in the archive: where it sits and which file marked it.
#[derive(Debug, Clone)]
struct Candidate {
    marker: Member,
}

impl Candidate {
    fn label(&self) -> &str {
        &self.marker.name
    }
}

/// Pick the single design file to read, or `None` when there is no project.
fn choose<R: Read + Seek>(
    archive: &mut zip::ZipArchive<R>,
    file_name: &str,
) -> Result<Option<Member>, BoardInputError> {
    let members = list(archive)?;
    let with_ext =
        |ext: &str| -> Vec<Member> { members.iter().filter(|m| m.has_ext(ext)).cloned().collect() };
    let mut candidates: Vec<Candidate> = with_ext("kicad_pro")
        .into_iter()
        .map(|marker| Candidate { marker })
        .collect();
    if candidates.is_empty() {
        candidates = root_schematics(archive, &with_ext("kicad_sch"))
            .into_iter()
            .map(|marker| Candidate { marker })
            .collect();
    }
    if candidates.is_empty() {
        candidates = with_ext("kicad_pcb")
            .into_iter()
            .map(|marker| Candidate { marker })
            .collect();
    }
    candidates.sort_by(|a, b| a.label().cmp(b.label()));
    let [one] = candidates.as_slice() else {
        if candidates.is_empty() {
            return Ok(None);
        }
        return Err(zip_err(format!(
            "'{file_name}' holds {} KiCad projects ({}); hauksbee reads one design at a time. \
             Unpack the zip and pass the .kicad_pro, .kicad_pcb or root .kicad_sch of the one you mean",
            candidates.len(),
            candidates
                .iter()
                .map(Candidate::label)
                .collect::<Vec<_>>()
                .join(", ")
        )));
    };
    design_of(one, &members, file_name).map(Some)
}

/// The design file of a project: the layout named like the project, else the
/// schematic named like it, else the only layout or the only root schematic in
/// its folder.
fn design_of(
    candidate: &Candidate,
    members: &[Member],
    file_name: &str,
) -> Result<Member, BoardInputError> {
    let marker = &candidate.marker;
    let here: Vec<&Member> = members.iter().filter(|m| m.dir() == marker.dir()).collect();
    let named = |ext: &str| {
        here.iter()
            .find(|m| m.stem() == marker.stem() && m.has_ext(ext))
            .map(|m| (*m).clone())
    };
    if let Some(found) = named("kicad_pcb").or_else(|| named("kicad_sch")) {
        return Ok(found);
    }
    let only = |ext: &str| {
        let mut all = here.iter().filter(|m| m.has_ext(ext));
        match (all.next(), all.next()) {
            (Some(one), None) => Some((*one).clone()),
            _ => None,
        }
    };
    only("kicad_pcb").or_else(|| only("kicad_sch")).ok_or_else(|| {
        zip_err(format!(
            "the KiCad project '{}' in '{file_name}' has no .kicad_pcb or .kicad_sch of its own name beside it; \
             a schematic or PCB source is needed",
            marker.name
        ))
    })
}

/// The schematics no other schematic in the archive names as a sub-sheet.
fn root_schematics<R: Read + Seek>(
    archive: &mut zip::ZipArchive<R>,
    schematics: &[Member],
) -> Vec<Member> {
    let mut budget = MAX_UNPACKED_BYTES;
    let mut referenced: Vec<(usize, String)> = Vec::new();
    for member in schematics {
        let Ok(entry) = archive.by_index(member.index) else {
            continue;
        };
        let Ok(bytes) = read_within(entry, &mut budget) else {
            break;
        };
        let text = String::from_utf8_lossy(&bytes);
        for base in sheet_files(&text) {
            referenced.push((member.index, base));
        }
    }
    let roots: Vec<Member> = schematics
        .iter()
        .filter(|m| {
            !referenced
                .iter()
                .any(|(from, base)| *from != m.index && base == m.base())
        })
        .cloned()
        .collect();
    if roots.is_empty() {
        schematics.to_vec()
    } else {
        roots
    }
}

/// The file names (no directory) in the `Sheetfile` properties of a schematic.
fn sheet_files(text: &str) -> Vec<String> {
    let marker = "\"Sheetfile\" \"";
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find(marker) {
        let after = &rest[at + marker.len()..];
        let Some(end) = after.find('"') else {
            break;
        };
        let value = after[..end].replace('\\', "/");
        out.push(value.rsplit('/').next().unwrap_or(&value).to_string());
        rest = &after[end..];
    }
    out
}

fn read_within(entry: impl Read, budget: &mut u64) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    entry.take(*budget + 1).read_to_end(&mut buf)?;
    let len = buf.len() as u64;
    if len > *budget {
        return Err(std::io::Error::other("archive expands past the limit"));
    }
    *budget -= len;
    Ok(buf)
}

fn limit_error(file_name: &str, cap: u64) -> BoardInputError {
    zip_err(format!(
        "'{file_name}' expands to more than {} MiB, which is past the limit hauksbee will unpack for one design. \
         That is far larger than any real KiCad project, so the archive is most likely malformed or hostile; \
         if it genuinely is that big, unpack it and point hauksbee at the folder instead",
        (cap / (1024 * 1024)).max(1)
    ))
}

/// Unpack every kept member under `root`, enforcing the byte cap on what is
/// actually written.
fn extract_all<R: Read + Seek>(
    archive: &mut zip::ZipArchive<R>,
    root: &Path,
    file_name: &str,
    cap: u64,
) -> Result<(), BoardInputError> {
    let members = list(archive)?;
    let declared: u64 = members
        .iter()
        .filter_map(|m| archive.by_index_raw(m.index).ok().map(|e| e.size()))
        .fold(0, u64::saturating_add);
    if declared > cap {
        return Err(limit_error(file_name, cap));
    }
    let mut budget = cap;
    for member in members {
        let mut entry = archive.by_index(member.index).map_err(|e| {
            zip_err(format!(
                "could not read '{}' from '{file_name}': {e}",
                member.name
            ))
        })?;
        if entry.is_symlink() {
            continue;
        }
        let mut target = root.to_path_buf();
        target.extend(member.name.split('/'));
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| zip_err(format!("could not unpack '{}': {e}", member.name)))?;
        }
        let mut out = std::fs::File::create(&target)
            .map_err(|e| zip_err(format!("could not unpack '{}': {e}", member.name)))?;
        let copied = std::io::copy(&mut (&mut entry).take(budget + 1), &mut out)
            .map_err(|e| zip_err(format!("could not unpack '{}': {e}", member.name)))?;
        if copied > budget {
            return Err(limit_error(file_name, cap));
        }
        budget -= copied;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};

    const PCB: &str = r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "A")
  (net 2 "B")
  (module Resistor_SMD:R_0402_1005Metric (layer F.Cu)
    (at 0 0)
    (fp_text reference R1 (at 0 0) (layer F.SilkS))
    (fp_text value 10k (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (size 1 1) (layers F.Cu) (net 1 "A"))
    (pad 2 smd rect (at 1 0) (size 1 1) (layers F.Cu) (net 2 "B"))
  )
)
"#;

    fn zip_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, bytes) in entries {
            w.start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            w.write_all(bytes).unwrap();
        }
        w.finish().unwrap().into_inner()
    }

    fn unpack_bytes(bytes: &[u8]) -> Result<Option<Unpacked>, BoardInputError> {
        unpack(Cursor::new(bytes), "d.zip")
    }

    #[test]
    fn a_project_is_found_by_its_kicad_pro_inside_a_wrapper_folder() {
        let zip = zip_of(&[
            ("proj-main/README.md", b"hi"),
            ("proj-main/hw/proj.kicad_pro", b"{}"),
            ("proj-main/hw/proj.kicad_pcb", PCB.as_bytes()),
            ("proj-main/hw/proj-backups/proj-2024.kicad_pro", b"{}"),
            ("__MACOSX/proj-main/hw/._proj.kicad_pro", b"x"),
        ]);
        let found = unpack_bytes(&zip).unwrap().unwrap();
        assert_eq!(found.member, "proj-main/hw/proj.kicad_pcb");
        assert!(found.design.is_file());
        assert!(found.design.with_extension("kicad_pro").is_file());
    }

    #[test]
    fn the_unpacked_directory_goes_when_the_handle_drops() {
        let zip = zip_of(&[("a.kicad_pro", b"{}"), ("a.kicad_pcb", PCB.as_bytes())]);
        let found = unpack_bytes(&zip).unwrap().unwrap();
        let dir = found.design.parent().unwrap().to_path_buf();
        assert!(dir.is_dir());
        drop(found);
        assert!(!dir.exists(), "temp dir must be removed");
    }

    #[test]
    fn without_a_project_file_the_root_schematic_is_the_project() {
        let zip = zip_of(&[
            (
                "top.kicad_sch",
                b"(kicad_sch (property \"Sheetfile\" \"power.kicad_sch\"))",
            ),
            ("power.kicad_sch", b"(kicad_sch)"),
        ]);
        let found = unpack_bytes(&zip).unwrap().unwrap();
        assert_eq!(found.member, "top.kicad_sch");
    }

    #[test]
    fn without_project_or_schematic_a_lone_layout_is_read() {
        let zip = zip_of(&[("deep/dir/x.kicad_pcb", PCB.as_bytes())]);
        assert_eq!(
            unpack_bytes(&zip).unwrap().unwrap().member,
            "deep/dir/x.kicad_pcb"
        );
    }

    #[test]
    fn several_projects_are_listed_not_chosen() {
        let zip = zip_of(&[
            ("a/one.kicad_pro", b"{}"),
            ("a/one.kicad_pcb", PCB.as_bytes()),
            ("b/two.kicad_pro", b"{}"),
            ("b/two.kicad_pcb", PCB.as_bytes()),
        ]);
        let err = unpack_bytes(&zip).unwrap_err().to_string();
        assert!(err.contains("2 KiCad projects"), "{err}");
        assert!(
            err.contains("a/one.kicad_pro") && err.contains("b/two.kicad_pro"),
            "{err}"
        );
    }

    #[test]
    fn a_zip_with_no_kicad_source_is_left_to_the_gerber_reader() {
        let zip = zip_of(&[("job.gtl", b"G04*\nM02*\n"), ("job.drl", b"M48\n")]);
        assert!(unpack_bytes(&zip).unwrap().is_none());
        assert!(unpack(Cursor::new(b"not a zip".as_slice()), "x.zip")
            .unwrap()
            .is_none());
    }

    #[test]
    fn a_project_with_no_design_beside_it_says_a_source_is_needed() {
        let zip = zip_of(&[
            ("p/p.kicad_pro", b"{}"),
            ("p/a.kicad_pcb", PCB.as_bytes()),
            ("p/b.kicad_pcb", PCB.as_bytes()),
        ]);
        let err = unpack_bytes(&zip).unwrap_err().to_string();
        assert!(err.contains("schematic or PCB source is needed"), "{err}");
    }

    #[test]
    fn a_member_path_that_escapes_refuses_the_archive() {
        for bad in [
            "../evil.kicad_pro",
            "a/../../evil.kicad_pro",
            "/abs.kicad_pro",
            "..\\evil.kicad_pro",
            "C:\\evil.kicad_pro",
        ] {
            let zip = zip_of(&[(bad, b"{}"), ("ok.kicad_pcb", PCB.as_bytes())]);
            let err = unpack_bytes(&zip).unwrap_err().to_string();
            assert!(err.contains("escapes"), "{bad}: {err}");
        }
    }

    #[test]
    fn backslash_separated_members_unpack_under_the_folder() {
        let zip = zip_of(&[
            ("win\\p.kicad_pro", b"{}"),
            ("win\\p.kicad_pcb", PCB.as_bytes()),
        ]);
        let found = unpack_bytes(&zip).unwrap().unwrap();
        assert_eq!(found.member, "win/p.kicad_pcb");
        assert!(found.design.is_file());
    }

    #[test]
    fn too_many_members_are_refused() {
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for i in 0..=MAX_MEMBERS {
            w.start_file(
                format!("f{i}.txt"),
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
        }
        let bytes = w.finish().unwrap().into_inner();
        let err = unpack_bytes(&bytes).unwrap_err().to_string();
        assert!(err.contains("members"), "{err}");
    }

    #[test]
    fn an_archive_past_the_byte_cap_is_refused() {
        let big = vec![0_u8; 3 << 20];
        let zip = zip_of(&[
            ("p.kicad_pro", b"{}"),
            ("p.kicad_pcb", PCB.as_bytes()),
            ("blob.bin", &big),
        ]);
        let err = unpack_capped(Cursor::new(zip.as_slice()), "d.zip", 2 << 20)
            .unwrap_err()
            .to_string();
        assert!(err.contains("expands to more than 2 MiB"), "{err}");
        // Under the cap the same archive unpacks.
        assert!(unpack_capped(Cursor::new(zip.as_slice()), "d.zip", 8 << 20)
            .unwrap()
            .is_some());
    }

    #[test]
    fn sibling_text_reads_the_project_file_from_the_archive() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.zip");
        std::fs::write(
            &path,
            zip_of(&[
                ("w/p.kicad_pro", b"{\"a\":1}"),
                ("w/p.kicad_pcb", PCB.as_bytes()),
            ]),
        )
        .unwrap();
        let text = sibling_text(&path, "kicad_pro").unwrap().unwrap();
        assert_eq!(text, "{\"a\":1}");
        assert!(sibling_text(&path, "kicad_dru").is_none());
    }
}
