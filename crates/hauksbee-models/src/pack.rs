//! Model packs (06-extensibility-sdk §3): versioned, shareable bundles of
//! model data with an explicit place in the resolution priority order.
//!
//! # Pack format
//!
//! A pack is a directory:
//!
//! ```text
//! my-pack/
//!   pack.toml          # the manifest (required)
//!   models/            # one or more [[models]] db TOML files (required)
//!     sensors.toml
//!   firmware/          # optional fixtures for the pack's own tests
//! ```
//!
//! `pack.toml`:
//!
//! ```toml
//! [pack]
//! name = "acme-sensors"            # [a-z0-9._-], used as the install dir name
//! version = "1.2.0"                # x.y.z, digits only
//! license = "MIT"
//! min_hauksbee_version = "0.1.0"   # oldest hauksbee this pack works with
//! provenance = "hand-written"      # hand-written | datasheet-extracted | vendor
//! description = "ACME's sensor line"   # optional
//! ```
//!
//! Distribution is deliberately plain: a git repo or a tarball. No signing, no
//! registry. `hauksbee models add <path|url>` copies a validated pack into
//! `~/.hauksbee/packs/<name>@<version>/` and records it in
//! `~/.hauksbee/packs.toml` (the lockfile-ish record, sibling of the packs
//! dir). Validation is fail-loud: every failure category is a named
//! [`PackError`] variant, and every `models/*.toml` file must pass the same
//! per-entry validation `hauksbee models lint` applies, a pack that installs
//! is a pack that loads.
//!
//! # Where packs sit in resolution
//!
//! Installed packs load at [`crate::SourceLayer::Pack`] (priority 10): above
//! the builtin db, below the user model dirs. Same-layer conflicts *between*
//! packs (two packs shipping the same model id) are reported loudly at load,
//! naming both packs, never silently resolved. See [`crate::ModelLibrary`].
//!
//! Long-form how-and-why: docs/how-and-why/hauksbee-models/pack.md.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// A pack-format or pack-store failure. Every validation category is a named
/// variant (the `sensor_spec.rs` discipline): a bad pack fails loud and
/// specific, at `models add` time, before anything is copied.
#[derive(Debug, thiserror::Error)]
pub enum PackError {
    /// The pack directory does not exist or is not a directory.
    #[error("pack directory '{0}' does not exist (or is not a directory)")]
    NotADirectory(PathBuf),

    /// No `pack.toml` at the pack root.
    #[error("no pack.toml in '{0}': a pack is a directory with a pack.toml manifest")]
    MissingManifest(PathBuf),

    /// `pack.toml` is not valid TOML or lacks the `[pack]` table.
    #[error("pack.toml parse error: {0}")]
    ManifestParse(String),

    /// A required manifest field is absent.
    #[error("pack.toml is missing required field 'pack.{0}'")]
    MissingField(&'static str),

    /// `pack.name` is empty or contains characters unusable as a directory
    /// name (allowed: lowercase alphanumerics, `.`, `_`, `-`).
    #[error("invalid pack name {0:?}: must be non-empty, lowercase [a-z0-9._-]")]
    InvalidName(String),

    /// A version field is not plain `x.y.z` (digits only).
    #[error("invalid {field} {value:?}: expected x.y.z (digits only, e.g. \"1.2.0\")")]
    InvalidVersion { field: &'static str, value: String },

    /// Vendor material cannot enter the highest source tier without a stated
    /// redistribution/use licence.
    #[error("vendor pack has a blank license; vendor models require an explicit license before they can enter the vendor-spice tier")]
    InvalidLicense,

    /// `pack.provenance` is not one of the three declared origins.
    #[error("unknown provenance {0:?}: expected \"hand-written\", \"datasheet-extracted\", or \"vendor\"")]
    UnknownProvenance(String),

    /// The pack requires a newer hauksbee than this build.
    #[error("pack requires hauksbee >= {required} but this is {current}")]
    IncompatibleVersion { required: String, current: String },

    /// No `models/` directory, or it contains no `.toml` files: the pack
    /// carries no models, so installing it would be a silent no-op.
    #[error("pack '{0}' has no models/*.toml files: a pack must ship at least one model file")]
    NoModels(PathBuf),

    /// A `models/*.toml` file failed the same validation `models lint` runs.
    #[error("pack model file '{file}' failed validation: {message}")]
    ModelFileInvalid { file: String, message: String },

    /// Installing a name that is already installed.
    #[error("pack '{0}' is already installed; run `hauksbee models remove {1}` first")]
    AlreadyInstalled(String, String),

    /// Removing a pack that is not in the record.
    #[error("no installed pack named {0:?} (see `hauksbee models list`)")]
    NotInstalled(String),

    /// Filesystem trouble, wrapped with the operation that hit it.
    #[error("pack I/O error {context}: {error}")]
    Io {
        context: String,
        error: std::io::Error,
    },
}

fn io_err(context: impl Into<String>) -> impl FnOnce(std::io::Error) -> PackError {
    let context = context.into();
    move |error| PackError::Io { context, error }
}

// ── Manifest ──────────────────────────────────────────────────────────────────

/// Where a pack's model data came from. Declared, not inferred: the point is
/// that a reviewer of `models list` output can see at a glance whether numbers
/// were typed by a human, extracted by a model, or shipped by the part vendor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Provenance {
    #[serde(rename = "hand-written")]
    HandWritten,
    #[serde(rename = "datasheet-extracted")]
    DatasheetExtracted,
    #[serde(rename = "vendor")]
    Vendor,
}

impl std::fmt::Display for Provenance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Provenance::HandWritten => write!(f, "hand-written"),
            Provenance::DatasheetExtracted => write!(f, "datasheet-extracted"),
            Provenance::Vendor => write!(f, "vendor"),
        }
    }
}

/// The validated `[pack]` manifest.
#[derive(Debug, Clone, Serialize)]
pub struct PackManifest {
    pub name: String,
    pub version: String,
    pub license: String,
    pub min_hauksbee_version: String,
    pub provenance: Provenance,
    pub description: String,
}

/// Raw deserialisation target: every field optional so absence becomes a named
/// [`PackError::MissingField`], not a serde message.
#[derive(Deserialize)]
struct RawManifestFile {
    pack: Option<RawManifest>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    name: Option<String>,
    version: Option<String>,
    license: Option<String>,
    min_hauksbee_version: Option<String>,
    provenance: Option<String>,
    #[serde(default)]
    description: String,
}

/// Parse `x.y.z` (digits only) into a comparable triple.
fn parse_version(field: &'static str, v: &str) -> Result<(u64, u64, u64), PackError> {
    let bad = || PackError::InvalidVersion {
        field,
        value: v.to_string(),
    };
    let parts: Vec<&str> = v.split('.').collect();
    if parts.len() != 3 {
        return Err(bad());
    }
    let mut nums = [0u64; 3];
    for (i, p) in parts.iter().enumerate() {
        if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
            return Err(bad());
        }
        nums[i] = p.parse().map_err(|_| bad())?;
    }
    Ok((nums[0], nums[1], nums[2]))
}

fn valid_pack_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
}

impl PackManifest {
    /// Parse and validate a `pack.toml` source string. `hauksbee_version` is
    /// the running build's version (pass [`HAUKSBEE_VERSION`]), checked
    /// against `min_hauksbee_version`.
    pub fn from_toml(src: &str, hauksbee_version: &str) -> Result<PackManifest, PackError> {
        let raw: RawManifestFile =
            toml::from_str(src).map_err(|e| PackError::ManifestParse(e.to_string()))?;
        let raw = raw
            .pack
            .ok_or_else(|| PackError::ManifestParse("no [pack] table".to_string()))?;

        let name = raw.name.ok_or(PackError::MissingField("name"))?;
        if !valid_pack_name(&name) {
            return Err(PackError::InvalidName(name));
        }
        let version = raw.version.ok_or(PackError::MissingField("version"))?;
        parse_version("pack.version", &version)?;
        let license = raw.license.ok_or(PackError::MissingField("license"))?;
        let min = raw
            .min_hauksbee_version
            .ok_or(PackError::MissingField("min_hauksbee_version"))?;
        let required = parse_version("pack.min_hauksbee_version", &min)?;
        // Cargo exposes the full SemVer string here. Model-pack compatibility
        // intentionally uses the numeric compatibility line, so a prerelease
        // such as 0.1.0-beta.1 can load packs whose floor is 0.1.0.
        let current_core = hauksbee_version
            .split(['-', '+'])
            .next()
            .unwrap_or(hauksbee_version);
        let current = parse_version("hauksbee version", current_core)?;
        if required > current {
            return Err(PackError::IncompatibleVersion {
                required: min,
                current: hauksbee_version.to_string(),
            });
        }
        let provenance = match raw
            .provenance
            .ok_or(PackError::MissingField("provenance"))?
            .as_str()
        {
            "hand-written" => Provenance::HandWritten,
            "datasheet-extracted" => Provenance::DatasheetExtracted,
            "vendor" => Provenance::Vendor,
            other => return Err(PackError::UnknownProvenance(other.to_string())),
        };
        if provenance == Provenance::Vendor && license.trim().is_empty() {
            return Err(PackError::InvalidLicense);
        }
        Ok(PackManifest {
            name,
            version,
            license,
            min_hauksbee_version: min,
            provenance,
            description: raw.description,
        })
    }

    /// The install directory name: `<name>@<version>`.
    pub fn dir_name(&self) -> String {
        format!("{}@{}", self.name, self.version)
    }
}

/// This build's version, the value packs' `min_hauksbee_version` is checked
/// against.
pub const HAUKSBEE_VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod behavioral_gate_tests {
    use super::*;

    fn write_pack(dir: &Path, model_toml: &str) {
        std::fs::create_dir_all(dir.join("models")).unwrap();
        std::fs::write(
            dir.join("pack.toml"),
            "[pack]\nname = \"test-pack\"\nversion = \"0.1.0\"\nlicense = \"MIT\"\n\
             min_hauksbee_version = \"0.0.0\"\nprovenance = \"hand-written\"\n",
        )
        .unwrap();
        std::fs::write(dir.join("models").join("m.toml"), model_toml).unwrap();
    }

    // R52: validate_behavioral was never called from Pack::load, so a converter
    // model with vout_setpoint = nan installed clean and later panicked the solver
    // at `v_cmd.clamp(0.0, nan)`. Pack::load must now run the behavioural gate.
    #[test]
    fn pack_load_rejects_nonfinite_behavioral_converter() {
        let base = std::env::temp_dir().join(format!("hb_pack_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);

        let bad = "[[models]]\nid = \"badconv\"\nkind = \"digital\"\n\
            [models.pins]\n\"1\" = \"in\"\n\"2\" = \"out\"\n\
            [models.behavioral.converter]\ntopology = \"buck\"\nout_pin = \"out\"\n\
            in_pin = \"in\"\nvout_setpoint = nan\nefficiency = 0.9\n";
        let dir = base.join("bad");
        write_pack(&dir, bad);
        let err = Pack::load(&dir).expect_err("a nan vout_setpoint must be rejected");
        match err {
            PackError::ModelFileInvalid { message, .. } => {
                assert!(
                    message.contains("behavioral") && message.contains("vout_setpoint"),
                    "error must name the behavioural field: {message}"
                );
            }
            other => panic!("expected ModelFileInvalid, got {other:?}"),
        }

        // A well-formed converter loads fine.
        let good = "[[models]]\nid = \"okconv\"\nkind = \"digital\"\n\
            [models.pins]\n\"1\" = \"in\"\n\"2\" = \"out\"\n\
            [models.behavioral.converter]\ntopology = \"buck\"\nout_pin = \"out\"\n\
            in_pin = \"in\"\nvout_setpoint = 3.3\nefficiency = 0.9\n";
        let dir = base.join("good");
        write_pack(&dir, good);
        assert!(
            Pack::load(&dir).is_ok(),
            "a valid behavioural converter must load"
        );

        let _ = std::fs::remove_dir_all(&base);
    }
}

// ── Pack ──────────────────────────────────────────────────────────────────────

/// A validated pack: manifest plus the model files it ships.
#[derive(Debug)]
pub struct Pack {
    pub manifest: PackManifest,
    /// The pack root directory this was loaded from.
    pub dir: PathBuf,
    /// Absolute paths of the `models/*.toml` files, sorted by file name so
    /// load order is deterministic.
    pub model_files: Vec<PathBuf>,
}

impl Pack {
    /// Load and fully validate a pack directory. Everything `models add`
    /// checks happens here, so the library loader and the installer agree.
    pub fn load(dir: &Path) -> Result<Pack, PackError> {
        if !dir.is_dir() {
            return Err(PackError::NotADirectory(dir.to_path_buf()));
        }
        let manifest_path = dir.join("pack.toml");
        if !manifest_path.is_file() {
            return Err(PackError::MissingManifest(dir.to_path_buf()));
        }
        let src = std::fs::read_to_string(&manifest_path)
            .map_err(io_err(format!("reading '{}'", manifest_path.display())))?;
        let manifest = PackManifest::from_toml(&src, HAUKSBEE_VERSION)?;

        // models/*.toml: at least one, and every one passes entry validation.
        let models_dir = dir.join("models");
        let mut model_files: Vec<PathBuf> = Vec::new();
        if models_dir.is_dir() {
            for e in std::fs::read_dir(&models_dir)
                .map_err(io_err(format!("listing '{}'", models_dir.display())))?
                .flatten()
            {
                let p = e.path();
                if p.extension().and_then(|x| x.to_str()) == Some("toml") {
                    model_files.push(p);
                }
            }
        }
        if model_files.is_empty() {
            return Err(PackError::NoModels(dir.to_path_buf()));
        }
        model_files.sort();

        for file in &model_files {
            let fname = file
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("?")
                .to_string();
            let text = std::fs::read_to_string(file)
                .map_err(io_err(format!("reading '{}'", file.display())))?;
            let db: crate::schema::DbFile =
                toml::from_str(&text).map_err(|e| PackError::ModelFileInvalid {
                    file: fname.clone(),
                    message: e.to_string(),
                })?;
            if db.models.is_empty() {
                return Err(PackError::ModelFileInvalid {
                    file: fname,
                    message: "no [[models]] entries".to_string(),
                });
            }
            for entry in &db.models {
                if let Err(errors) = crate::validation::validate(entry) {
                    // Every error here belongs to this one entry, so the id
                    // prefixes the joined bare messages exactly once. Joining
                    // the errors' own Display (which carries the prefix too)
                    // rendered "model 's8050': model 's8050': ...".
                    let msgs: Vec<&str> = errors.iter().map(|e| e.message.as_str()).collect();
                    return Err(PackError::ModelFileInvalid {
                        file: fname,
                        message: format!("model '{}': {}", entry.id, msgs.join("; ")),
                    });
                }
                if !entry.logic.is_empty() {
                    if let Err(e) = entry.logic.validate() {
                        return Err(PackError::ModelFileInvalid {
                            file: fname,
                            message: format!("model '{}' [models.logic]: {e}", entry.id),
                        });
                    }
                }
                // The behavioural block carries its own finiteness/positivity gates
                // (converter setpoints & limits, pull/od/drive voltages & ohms,
                // programmed sense params, FSM dwell). Without this call those gates
                // never ran on a real installed pack, a `vout_setpoint = nan`
                // panics the solver at `v_cmd.clamp(0.0, nan)` on a model that
                // "validated clean". Gate it here like params and logic.
                if !entry.behavioral.is_empty() {
                    let errs = crate::behavioral::validate_behavioral(&entry.behavioral);
                    if !errs.is_empty() {
                        return Err(PackError::ModelFileInvalid {
                            file: fname,
                            message: format!(
                                "model '{}' [models.behavioral]: {}",
                                entry.id,
                                errs.join("; ")
                            ),
                        });
                    }
                }
            }
        }

        Ok(Pack {
            manifest,
            dir: dir.to_path_buf(),
            model_files,
        })
    }
}

// ── PackStore ─────────────────────────────────────────────────────────────────

/// One entry of the lockfile-ish `packs.toml` record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackRecord {
    pub name: String,
    pub version: String,
    pub license: String,
    pub provenance: String,
    /// Where the pack was installed from (a path or URL, verbatim).
    pub source: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct PacksFile {
    #[serde(default, rename = "pack")]
    packs: Vec<PackRecord>,
}

/// The installed-packs store: `<home>/.hauksbee/packs/<name>@<version>/`
/// directories plus the `<home>/.hauksbee/packs.toml` record alongside.
#[derive(Debug, Clone)]
pub struct PackStore {
    /// Directory holding the installed pack dirs.
    root: PathBuf,
    /// The `packs.toml` record file (sibling of `root`).
    record: PathBuf,
}

impl PackStore {
    /// The store under an explicit home directory (tests pass a temp dir; the
    /// CLI passes `$HOME`). Nothing is created until the first install.
    pub fn in_home(home: &Path) -> PackStore {
        let base = home.join(".hauksbee");
        PackStore {
            root: base.join("packs"),
            record: base.join("packs.toml"),
        }
    }

    /// The store for the current user (`$HOME`), or `None` when HOME is unset.
    pub fn default_location() -> Option<PackStore> {
        std::env::var_os("HOME").map(|h| PackStore::in_home(Path::new(&h)))
    }

    /// Where installed packs live.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The `packs.toml` record path.
    pub fn record_path(&self) -> &Path {
        &self.record
    }

    /// Validate the pack at `src`, copy it into the store, and record it.
    /// `source_label` is what `packs.toml` remembers as the origin (the path
    /// or URL the user typed). Fails loud before copying anything.
    pub fn install(&self, src: &Path, source_label: &str) -> Result<PackRecord, PackError> {
        let pack = Pack::load(src)?;
        let mut file = self.read_record()?;
        if let Some(existing) = file.packs.iter().find(|r| r.name == pack.manifest.name) {
            return Err(PackError::AlreadyInstalled(
                format!("{}@{}", existing.name, existing.version),
                existing.name.clone(),
            ));
        }

        let dest = self.root.join(pack.manifest.dir_name());
        if dest.exists() {
            // A dir without a record entry is a broken previous install; be
            // explicit rather than merging old and new files.
            std::fs::remove_dir_all(&dest)
                .map_err(io_err(format!("clearing stale '{}'", dest.display())))?;
        }
        copy_dir(src, &dest)?;

        let record = PackRecord {
            name: pack.manifest.name.clone(),
            version: pack.manifest.version.clone(),
            license: pack.manifest.license.clone(),
            provenance: pack.manifest.provenance.to_string(),
            source: source_label.to_string(),
        };
        file.packs.push(record.clone());
        file.packs.sort_by(|a, b| a.name.cmp(&b.name));
        self.write_record(&file)?;
        Ok(record)
    }

    /// Remove an installed pack by name: deletes its directory and drops it
    /// from `packs.toml`.
    pub fn remove(&self, name: &str) -> Result<PackRecord, PackError> {
        let mut file = self.read_record()?;
        let idx = file
            .packs
            .iter()
            .position(|r| r.name == name)
            .ok_or_else(|| PackError::NotInstalled(name.to_string()))?;
        let record = file.packs.remove(idx);
        let dir = self
            .root
            .join(format!("{}@{}", record.name, record.version));
        if dir.exists() {
            std::fs::remove_dir_all(&dir)
                .map_err(io_err(format!("removing '{}'", dir.display())))?;
        }
        self.write_record(&file)?;
        Ok(record)
    }

    /// The installed packs, from `packs.toml` (empty when none installed).
    pub fn list(&self) -> Result<Vec<PackRecord>, PackError> {
        Ok(self.read_record()?.packs)
    }

    /// The install directory of a recorded pack.
    pub fn pack_dir(&self, record: &PackRecord) -> PathBuf {
        self.root
            .join(format!("{}@{}", record.name, record.version))
    }

    fn read_record(&self) -> Result<PacksFile, PackError> {
        if !self.record.exists() {
            return Ok(PacksFile::default());
        }
        let text = std::fs::read_to_string(&self.record)
            .map_err(io_err(format!("reading '{}'", self.record.display())))?;
        toml::from_str(&text).map_err(|e| {
            PackError::ManifestParse(format!("'{}' is corrupt: {e}", self.record.display()))
        })
    }

    fn write_record(&self, file: &PacksFile) -> Result<(), PackError> {
        if let Some(parent) = self.record.parent() {
            std::fs::create_dir_all(parent)
                .map_err(io_err(format!("creating '{}'", parent.display())))?;
        }
        let text = toml::to_string_pretty(file)
            .map_err(|e| PackError::ManifestParse(format!("serialising packs.toml: {e}")))?;
        std::fs::write(&self.record, text)
            .map_err(io_err(format!("writing '{}'", self.record.display())))
    }
}

/// Recursive directory copy (no symlink following; a pack is plain files).
fn copy_dir(src: &Path, dest: &Path) -> Result<(), PackError> {
    std::fs::create_dir_all(dest).map_err(io_err(format!("creating '{}'", dest.display())))?;
    for e in std::fs::read_dir(src)
        .map_err(io_err(format!("listing '{}'", src.display())))?
        .flatten()
    {
        let from = e.path();
        let to = dest.join(e.file_name());
        let ty = e
            .file_type()
            .map_err(io_err(format!("stat '{}'", from.display())))?;
        if ty.is_dir() {
            copy_dir(&from, &to)?;
        } else if ty.is_file() {
            std::fs::copy(&from, &to).map_err(io_err(format!("copying '{}'", from.display())))?;
        }
        // Symlinks and specials are skipped: nothing in a pack needs them.
    }
    Ok(())
}
