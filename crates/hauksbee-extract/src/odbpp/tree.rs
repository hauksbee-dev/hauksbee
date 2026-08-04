//! The ODB++ job as a flat, case-folded file map.
//!
//! ODB++ is a *directory tree*, and the three shapes it arrives in (a folder, a
//! `.tgz`, a `.zip`) differ only in how you enumerate that tree. Parsing
//! against `std::fs` would mean either a second archive-shaped parser or
//! unpacking to a temp directory on the web path, so every shape is normalized
//! into one [`OdbTree`] first and the record parsers only ever see it.
//!
//! Two details the shapes share and that a naive walk gets wrong:
//!
//! * **Case.** The matrix names layers in upper case (`NAME=COMP_+_TOP`) while
//!   the directories on disk are lower case (`comp_+_top`), and different tools
//!   disagree about which case the job's own directories use. Keys are
//!   lower-cased, so a lookup never depends on a producer's habit.
//! * **Per-file gzip.** ODB++ permits any file to be stored gzipped with a `.Z`
//!   (or `.gz`) suffix, and Altium/Cadence archives routinely do it for the
//!   large `features` files. The suffix is stripped and the payload inflated on
//!   the way in. A payload that will not inflate is *recorded*
//!   ([`OdbTree::undecompressed`]) rather than dropped, so a Unix-`compress`
//!   `.Z` from an exotic tool surfaces as a named problem instead of a layer
//!   that silently reads as empty.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;

use crate::ExtractError;

/// The marker that identifies an ODB++ job root: the matrix file, which every
/// job has exactly one of and which no other format ships.
pub(crate) const MATRIX_PATH: &str = "matrix/matrix";

/// A whole ODB++ job in memory: lower-cased `/`-separated path relative to the
/// job root → file content.
pub(crate) struct OdbTree {
    files: BTreeMap<String, Vec<u8>>,
    /// Paths whose `.Z` / `.gz` payload would not inflate, so their content is
    /// the raw compressed bytes and will not parse. Reported, never hidden.
    pub(crate) undecompressed: Vec<String>,
    /// How much more the job may inflate to before [`MAX_INFLATED_BYTES`] is
    /// reached. Held on the tree rather than passed around so the ceiling covers
    /// the WHOLE job: a bomb split across a thousand members must not slip past
    /// a per-member limit.
    budget: u64,
}

impl OdbTree {
    fn new() -> Self {
        OdbTree {
            files: BTreeMap::new(),
            undecompressed: Vec::new(),
            budget: MAX_INFLATED_BYTES,
        }
    }

    /// The error for a job that exceeds the inflation ceiling. Names the limit,
    /// so a legitimately enormous job reads as a limit to raise rather than as a
    /// corrupt file.
    fn too_large(&self) -> ExtractError {
        ExtractError::Odb(format!(
            "this ODB++ job expands to more than {} MiB, which is past the limit \
             hauksbee will hold in memory for one job. That is far larger than any \
             real fab job, so the archive is most likely malformed or hostile; if \
             it genuinely is that big, unpack it and point hauksbee at the \
             directory instead",
            MAX_INFLATED_BYTES / (1024 * 1024)
        ))
    }

    /// Insert one member, inflating and un-suffixing a `.Z`/`.gz` payload.
    fn insert(&mut self, path: &str, bytes: Vec<u8>) -> Result<(), ExtractError> {
        let key = path.replace('\\', "/").to_ascii_lowercase();
        let (key, compressed) = match key.strip_suffix(".z").or_else(|| key.strip_suffix(".gz")) {
            Some(stem) => (stem.to_string(), true),
            None => (key, false),
        };
        if !compressed {
            // Plain members still count: an uncompressed 2 GiB `features` inside
            // a tar is the same problem without the compression ratio.
            let len = bytes.len() as u64;
            if len > self.budget {
                return Err(self.too_large());
            }
            self.budget -= len;
            self.files.insert(key, bytes);
            return Ok(());
        }
        match gunzip_within(&bytes, &mut self.budget) {
            Some(plain) => {
                self.files.insert(key, plain);
            }
            None => {
                // Either not gzip at all, or past the ceiling. The two are told
                // apart by the magic, so a bomb is not mislabelled as an exotic
                // compression the reader merely does not know.
                if is_gzip(&bytes) {
                    return Err(self.too_large());
                }
                let len = bytes.len() as u64;
                if len > self.budget {
                    return Err(self.too_large());
                }
                self.budget -= len;
                self.undecompressed.push(key.clone());
                self.files.insert(key, bytes);
            }
        }
        Ok(())
    }

    /// Drop the leading job-directory component(s) so every key is relative to
    /// the root that holds `matrix/`. Archives normally wrap the job in a
    /// directory (`myjob/matrix/matrix`), and some wrap it twice.
    fn rebase_on_matrix(&mut self) {
        let Some(prefix) = self
            .files
            .keys()
            .filter_map(|k| k.strip_suffix(MATRIX_PATH))
            .min_by_key(|p| p.len())
            .map(str::to_string)
        else {
            return;
        };
        if prefix.is_empty() {
            return;
        }
        self.files = std::mem::take(&mut self.files)
            .into_iter()
            .filter_map(|(k, v)| k.strip_prefix(&prefix).map(|s| (s.to_string(), v)))
            .collect();
        for p in &mut self.undecompressed {
            if let Some(s) = p.strip_prefix(&prefix) {
                *p = s.to_string();
            }
        }
    }

    /// True when this looks like an ODB++ job at all: it has a matrix file.
    pub(crate) fn has_matrix(&self) -> bool {
        self.files.contains_key(MATRIX_PATH)
    }

    pub(crate) fn get(&self, path: &str) -> Option<&[u8]> {
        self.files.get(&path.to_ascii_lowercase()).map(Vec::as_slice)
    }

    /// A file decoded as (lossy) text. ODB++ files are ASCII by spec; lossy
    /// decoding keeps a stray high byte in a free-text property from failing
    /// the whole job.
    pub(crate) fn text(&self, path: &str) -> Option<String> {
        self.get(path).map(|b| String::from_utf8_lossy(b).into_owned())
    }

    /// Every key under `prefix` (which must end in `/`), in sorted order.
    pub(crate) fn paths_under(&self, prefix: &str) -> Vec<&str> {
        let prefix = prefix.to_ascii_lowercase();
        self.files
            .keys()
            .filter(|k| k.starts_with(&prefix))
            .map(String::as_str)
            .collect()
    }

    /// The immediate sub-directory names under `prefix` (which must end in
    /// `/`): `steps/` → the step names, `steps/pcb/layers/` → the layer names.
    pub(crate) fn dirs_under(&self, prefix: &str) -> Vec<String> {
        let prefix = prefix.to_ascii_lowercase();
        let mut out: Vec<String> = self
            .files
            .keys()
            .filter_map(|k| k.strip_prefix(&prefix))
            .filter_map(|rest| rest.split_once('/').map(|(head, _)| head.to_string()))
            .collect();
        out.sort_unstable();
        out.dedup();
        out
    }

    /// Build from an unpacked job directory. `dir` may be the job root itself
    /// or any ancestor of it (a downloads folder holding one job).
    pub(crate) fn from_dir(dir: &Path) -> Result<Self, ExtractError> {
        let mut tree = OdbTree::new();
        let mut files = Vec::new();
        collect(dir, &mut files);
        for path in files {
            let Ok(rel) = path.strip_prefix(dir) else {
                continue;
            };
            let Some(rel) = rel.to_str() else { continue };
            // Read failures (a permission-denied or vanished file) are skipped
            // rather than fatal: the missing member then shows up as the
            // specific "no eda/data" style refusal, which names what is absent.
            if let Ok(bytes) = std::fs::read(&path) {
                tree.insert(rel, bytes)?;
            }
        }
        tree.rebase_on_matrix();
        Ok(tree)
    }

    /// Build from a `.zip` archive's bytes.
    pub(crate) fn from_zip(bytes: &[u8]) -> Result<Self, ExtractError> {
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes))
            .map_err(|e| ExtractError::Odb(format!("read ODB++ zip: {e}")))?;
        let mut tree = OdbTree::new();
        for i in 0..zip.len() {
            let mut f = zip
                .by_index(i)
                .map_err(|e| ExtractError::Odb(format!("read ODB++ zip member {i}: {e}")))?;
            if f.is_dir() {
                continue;
            }
            let name = f.name().to_string();
            let mut buf = Vec::new();
            f.read_to_end(&mut buf)
                .map_err(|e| ExtractError::Odb(format!("read ODB++ zip member {name}: {e}")))?;
            tree.insert(&name, buf)?;
        }
        tree.rebase_on_matrix();
        Ok(tree)
    }

    /// Build from a gzipped tar (`.tgz` / `.tar.gz`), the form the ODB++ spec
    /// names and the one Altium and Cadence write by default.
    pub(crate) fn from_tgz(bytes: &[u8]) -> Result<Self, ExtractError> {
        // The tar itself counts against the ceiling before a single member is
        // read: a bomb in the OUTER gzip never reaches the per-member budget.
        let mut budget = MAX_INFLATED_BYTES;
        let plain = match gunzip_within(bytes, &mut budget) {
            Some(p) => p,
            None if is_gzip(bytes) && bytes.len() as u64 <= MAX_INFLATED_BYTES => {
                return Err(OdbTree::new().too_large())
            }
            None => {
                return Err(ExtractError::Odb(
                    "ODB++ .tgz: the gzip stream is corrupt".into(),
                ))
            }
        };
        Self::from_tar(&plain)
    }

    /// Build from an uncompressed tar.
    pub(crate) fn from_tar(bytes: &[u8]) -> Result<Self, ExtractError> {
        let mut ar = tar::Archive::new(std::io::Cursor::new(bytes));
        let mut tree = OdbTree::new();
        let entries = ar
            .entries()
            .map_err(|e| ExtractError::Odb(format!("read ODB++ tar: {e}")))?;
        for entry in entries {
            let mut entry = entry.map_err(|e| ExtractError::Odb(format!("read ODB++ tar: {e}")))?;
            if !entry.header().entry_type().is_file() {
                continue;
            }
            let name = match entry.path() {
                Ok(p) => p.to_string_lossy().into_owned(),
                Err(_) => continue,
            };
            let mut buf = Vec::new();
            entry
                .read_to_end(&mut buf)
                .map_err(|e| ExtractError::Odb(format!("read ODB++ tar member {name}: {e}")))?;
            tree.insert(&name, buf)?;
        }
        tree.rebase_on_matrix();
        Ok(tree)
    }

    /// Build from archive bytes, dispatching on the container magic.
    pub(crate) fn from_archive(bytes: &[u8]) -> Result<Self, ExtractError> {
        if is_zip(bytes) {
            Self::from_zip(bytes)
        } else if is_gzip(bytes) {
            Self::from_tgz(bytes)
        } else if looks_like_tar(bytes) {
            Self::from_tar(bytes)
        } else {
            Err(ExtractError::Odb(
                "not an ODB++ archive: expected a zip, a gzipped tar (.tgz) or a tar".into(),
            ))
        }
    }
}

fn collect(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect(&p, out);
        } else if p.is_file() {
            out.push(p);
        }
    }
}

pub(crate) fn is_zip(bytes: &[u8]) -> bool {
    bytes.starts_with(b"PK\x03\x04")
}

pub(crate) fn is_gzip(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0x1f, 0x8b])
}

/// A bare (uncompressed) tar: the `ustar` magic sits at offset 257 of the first
/// 512-byte header block. Checking the magic rather than the extension keeps a
/// mis-named archive readable and keeps a random binary out.
pub(crate) fn looks_like_tar(bytes: &[u8]) -> bool {
    bytes.len() >= 265 && (&bytes[257..262] == b"ustar")
}

/// The ceiling on what one job may inflate to, in total.
///
/// A `.tgz` is a compressed container of compressible text, so an ODB++ upload
/// is a natural decompression bomb: 400 KiB of gzip inflates to 400 MiB at a
/// ratio real fab jobs never come close to (the largest job tested, a 2 MB Valor
/// archive, inflates to 24 MB). The web front door accepts uploads up to 256 MiB
/// and this module reads members whole, so without a ceiling a single upload can
/// ask for hundreds of gigabytes of memory.
///
/// 512 MiB is roughly 20× the largest real job seen and leaves the honest
/// professional board — an 8-layer job with tens of megabytes of `features` — far
/// inside it. Hitting the ceiling is an error naming the limit, not a truncated
/// read: a job silently missing its last layers is exactly the half-board this
/// crate refuses to produce.
pub(crate) const MAX_INFLATED_BYTES: u64 = 512 * 1024 * 1024;

/// Inflate a gzip member, or `None` if it is not gzip / is corrupt / would
/// exceed `budget` (which is decremented by what was produced).
fn gunzip_within(bytes: &[u8], budget: &mut u64) -> Option<Vec<u8>> {
    if !is_gzip(bytes) {
        return None;
    }
    let mut out = Vec::new();
    // `take(budget + 1)` so an input that exactly fills the budget succeeds and
    // one that exceeds it is detected rather than silently truncated.
    let mut dec = flate2::read::GzDecoder::new(bytes).take(*budget + 1);
    dec.read_to_end(&mut out).ok()?;
    if out.len() as u64 > *budget {
        return None;
    }
    *budget -= out.len() as u64;
    Some(out)
}

/// Cheap content sniff for the archive forms: does this archive contain a
/// `matrix/matrix` member? Reads only what it must — a zip's central directory
/// for the zip form, and at most [`SNIFF_BYTES`] of inflated tar for the tgz
/// form, so detection never inflates a 200 MB fab archive.
pub(crate) fn archive_has_matrix(bytes: &[u8]) -> bool {
    if is_zip(bytes) {
        let Ok(zip) = zip::ZipArchive::new(std::io::Cursor::new(bytes)) else {
            return false;
        };
        return zip.file_names().any(name_is_matrix);
    }
    if is_gzip(bytes) {
        let mut head = Vec::new();
        let mut dec = flate2::read::GzDecoder::new(bytes).take(SNIFF_BYTES);
        if dec.read_to_end(&mut head).is_err() && head.is_empty() {
            return false;
        }
        return tar_names(&head).iter().any(|n| name_is_matrix(n));
    }
    if looks_like_tar(bytes) {
        return tar_names(bytes).iter().any(|n| name_is_matrix(n));
    }
    false
}

/// How much of a gzipped tar to inflate when sniffing. A tar stores its
/// directory inline, so the matrix header may sit anywhere; 8 MiB covers the
/// `matrix/` entry of every real archive tested (tools write `matrix` early)
/// while bounding the work a hostile upload can force.
const SNIFF_BYTES: u64 = 8 * 1024 * 1024;

fn name_is_matrix(name: &str) -> bool {
    let n = name.replace('\\', "/").to_ascii_lowercase();
    let n = n.strip_suffix(".z").or_else(|| n.strip_suffix(".gz")).unwrap_or(&n).to_string();
    n == MATRIX_PATH || n.ends_with(&format!("/{MATRIX_PATH}"))
}

/// The member names in a (possibly truncated) tar, read straight from the
/// 512-byte headers. `tar::Archive` gives up on a truncated stream mid-member;
/// sniffing must tolerate one, so this walks the headers itself.
fn tar_names(bytes: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut off = 0usize;
    while off + 512 <= bytes.len() {
        let block = &bytes[off..off + 512];
        if block.iter().all(|&b| b == 0) {
            break;
        }
        if &block[257..262] != b"ustar" {
            break;
        }
        let name_end = block[..100].iter().position(|&b| b == 0).unwrap_or(100);
        let name = String::from_utf8_lossy(&block[..name_end]).into_owned();
        // The size field is 11 octal digits (offset 124), NUL/space terminated.
        let size_field = String::from_utf8_lossy(&block[124..136]);
        let size = u64::from_str_radix(size_field.trim().trim_end_matches('\0').trim(), 8)
            .unwrap_or(0) as usize;
        if !name.is_empty() {
            out.push(name);
        }
        off += 512 + size.div_ceil(512) * 512;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_case_folded_and_rebased_on_the_matrix() {
        let mut tree = OdbTree::new();
        tree.insert("MyJob/MATRIX/Matrix", b"STEP {\n}\n".to_vec()).expect("insert");
        tree.insert("MyJob/steps/PCB/eda/data", b"UNITS=MM\n".to_vec()).expect("insert");
        tree.rebase_on_matrix();
        assert!(tree.has_matrix(), "matrix found after case folding + rebase");
        assert!(tree.get("steps/pcb/eda/data").is_some());
        assert_eq!(tree.dirs_under("steps/"), vec!["pcb".to_string()]);
    }

    #[test]
    fn a_gzipped_member_is_inflated_and_unsuffixed() {
        use std::io::Write;
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        enc.write_all(b"UNITS=MM\nF 3\n").expect("gz write");
        let gz = enc.finish().expect("gz finish");
        let mut tree = OdbTree::new();
        tree.insert("steps/pcb/layers/f.cu/features.Z", gz).expect("insert");
        assert_eq!(
            tree.text("steps/pcb/layers/f.cu/features").as_deref(),
            Some("UNITS=MM\nF 3\n"),
            "a .Z member is inflated and loses the suffix"
        );
        assert!(tree.undecompressed.is_empty());
    }

    #[test]
    fn an_uninflatable_member_is_recorded_not_hidden() {
        let mut tree = OdbTree::new();
        // A Unix-`compress` .Z (magic 1f 9d), which is NOT gzip.
        tree.insert("steps/pcb/layers/f.cu/features.Z", vec![0x1f, 0x9d, 0x90, 0x01]).expect("insert");
        assert_eq!(
            tree.undecompressed,
            vec!["steps/pcb/layers/f.cu/features".to_string()],
            "a member that will not inflate must be named, not silently empty"
        );
    }

    #[test]
    fn container_magics_are_distinguished() {
        assert!(is_zip(b"PK\x03\x04rest"));
        assert!(is_gzip(&[0x1f, 0x8b, 0x08, 0x00]));
        assert!(!looks_like_tar(b"short"));
        let mut fake_tar = vec![0u8; 512];
        fake_tar[257..262].copy_from_slice(b"ustar");
        assert!(looks_like_tar(&fake_tar));
    }
}
