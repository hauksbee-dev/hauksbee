//! Immutable, content-addressed run manifests shared by the human CLI and CI.
//!
//! A manifest is intentionally a deterministic *reproduction identity*, not a
//! process dump. It records exact input bytes, ordered arguments, normalized
//! options, workspace component versions, compiled features, and only the
//! small allowlist of environment selectors that can change model/backend
//! selection. Environment values are hashed: replay can prove the selector is
//! the same without publishing a username-bearing path. Timestamps, cwd,
//! hostname, PATH, debug flags, and secrets never enter the document.
//!
//! Paths are portable. Inputs and path-valued arguments are stored relative to
//! the project root (the git checkout containing the working directory, else
//! the working directory itself), or `~/...` under the home directory, so a
//! manifest replays from a colleague's checkout. Absolute paths from older
//! manifests are still read as they are. Each resolved backend (simavr, QEMU,
//! Renode, ngspice, kicad-cli, freerouting) is recorded with its version and
//! the SHA-256 of its binary, found by the same discovery `hauksbee doctor` uses.
//!
//! A finished run can also leave an [`Outcome`] in its manifest (verdict, exit
//! code, gating findings). It is not part of the manifest id: the manifest stays
//! the reproduction identity, and the outcome only gives `reproduce` something
//! to compare with.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

/// Schema 2 adds `root_hint`, `backends` and `outcome`, and stores paths relative
/// to the project root. Schema 1 documents remain readable.
pub const MANIFEST_SCHEMA_VERSION: u32 = 2;
const OLDEST_READABLE_SCHEMA: u32 = 1;

/// When set, a finished run writes its [`Outcome`] as JSON to this path. It is
/// how `reproduce` reads the replayed run's result without parsing its report.
pub const OUTCOME_FILE_ENV: &str = "HAUKSBEE_OUTCOME_FILE";

/// Environment selectors which change input/model/backend resolution. Values
/// are never serialized directly; see [`EnvironmentContract`]. API credentials
/// and diagnostic-only switches are deliberately absent.
const REPRODUCIBILITY_ENV: &[&str] = &[
    "HAUKSBEE_MCU_DIR",
    "HAUKSBEE_PIO",
    "HAUKSBEE_PIO_TIMEOUT_SECS",
    "HAUKSBEE_QEMU_DIR",
    "HAUKSBEE_QEMU_RISCV32",
    "HAUKSBEE_QEMU_XTENSA",
    "HAUKSBEE_RENODE",
    "IDF_TOOLS_PATH",
    "NGSPICE",
];

#[derive(Debug, Clone)]
pub struct ManifestInput {
    role: String,
    path: PathBuf,
    retained_bytes: Option<Vec<u8>>,
}

impl ManifestInput {
    pub fn new(role: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        Self {
            role: role.into(),
            path: path.into(),
            retained_bytes: None,
        }
    }

    /// Capture a file from bytes the caller already read and validated. This
    /// prevents the manifest from hashing a different revision after analysis
    /// resolution but before manifest construction.
    pub fn retained_file(
        role: impl Into<String>,
        path: impl Into<PathBuf>,
        bytes: Vec<u8>,
    ) -> Self {
        Self {
            role: role.into(),
            path: path.into(),
            retained_bytes: Some(bytes),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ManifestRequest {
    pub tool: ToolIdentity,
    /// Exact argv, including `argv[0]`, but excluding `--emit-manifest` and its
    /// output path. Replay must not attempt to overwrite its source evidence.
    pub command: Vec<String>,
    pub options: BTreeMap<String, serde_json::Value>,
    pub inputs: Vec<ManifestInput>,
    pub feature_flags: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolIdentity {
    pub name: String,
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_revision: Option<String>,
}

impl ToolIdentity {
    pub fn new(
        name: impl Into<String>,
        version: impl Into<String>,
        git_revision: Option<impl Into<String>>,
    ) -> Self {
        Self {
            name: name.into(),
            version: version.into(),
            git_revision: git_revision.map(Into::into),
        }
    }

    /// Identity of a workspace binary built from this engine revision.
    pub fn workspace(name: impl Into<String>) -> Self {
        Self::new(
            name,
            env!("CARGO_PKG_VERSION"),
            option_env!("GIT_HASH").map(str::to_owned),
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Digest {
    pub algorithm: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HashedInput {
    pub role: String,
    pub path: String,
    pub kind: String,
    pub digest: Digest,
    pub size_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_count: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildContract {
    pub target_os: String,
    pub target_arch: String,
    pub features: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginRevision {
    pub kind: String,
    pub name: String,
    pub version: String,
    pub provenance: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentSelector {
    pub name: String,
    pub value_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentContract {
    /// Values are hashes, never raw environment content. Replay verifies the
    /// current value against this digest and names the selector on mismatch.
    pub value_policy: String,
    pub selectors: Vec<EnvironmentSelector>,
}

/// One external program a run may have used, as the engine resolved it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendRevision {
    pub name: String,
    /// The tool's own version string, when it reports one.
    pub version: Option<String>,
    /// Hex SHA-256 of the executable (or jar). `None` for a backend linked into
    /// this binary, whose version is its source commit.
    pub sha256_of_binary: Option<String>,
}

/// What a finished run concluded. Informational: it is excluded from the
/// manifest id and only used for comparison by `reproduce`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outcome {
    /// `pass`, `fail`, `invalid` or `error`.
    pub verdict: String,
    pub exit_code: i32,
    /// Hex SHA-256 of the run record this run wrote. `null` until the run
    /// record exists; [`note_record_sha256`] is the hook that fills it.
    pub record_sha256: Option<String>,
    /// Sorted, stable ids of the findings that gated the run (finding group
    /// fingerprints, or `spec[i]:<assertion>` for CI specs).
    #[serde(default)]
    pub gating_findings: Vec<String>,
}

/// What a replayed run hands back to `reproduce` through the outcome file: its
/// [`Outcome`] plus the error message when it ended in one. The message is for
/// the person reading the DIFF; it is never written into a manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ReplayOutcome {
    #[serde(flatten)]
    outcome: Outcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Invocation {
    pub argv: Vec<String>,
    pub options: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunManifest {
    pub schema_version: u32,
    pub manifest_id: String,
    pub tool: ToolIdentity,
    /// The workspace uses one release version for these crates. An exact git
    /// revision in `tool` pins their source when the binary was built in git.
    pub components: BTreeMap<String, String>,
    /// Installed model packs are the current plugin surface. Their source path
    /// is intentionally omitted; the hashed pack directory and record remain
    /// in `inputs`, while this gives a user-visible name/version inventory.
    pub plugins: Vec<PluginRevision>,
    pub build: BuildContract,
    /// Names the project root that `inputs` and `invocation` paths are relative
    /// to, without giving its location on the capturing machine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_hint: Option<String>,
    pub environment: EnvironmentContract,
    /// External programs available when the run was captured.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub backends: Vec<BackendRevision>,
    pub inputs: Vec<HashedInput>,
    pub invocation: Invocation,
    pub reproduce: String,
    /// Set by [`finish_outcome`] when the run ends; not part of the id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<Outcome>,
}

impl RunManifest {
    /// Hash every requested input and capture the tool, build and environment
    /// identity into a new manifest.
    ///
    /// # Errors
    ///
    /// Returns an error if the command is empty, if an input is a symlink or
    /// cannot be read or hashed, if two inputs share a role and path, or if the
    /// manifest cannot be serialized to compute its id.
    pub fn capture(request: ManifestRequest) -> Result<Self> {
        Self::capture_at(request, &project_root())
    }

    /// As [`Self::capture`], with paths made relative to `root` instead of the
    /// project root found from the working directory.
    ///
    /// # Errors
    ///
    /// Same as [`Self::capture`], and if `root` cannot be resolved.
    pub fn capture_at(mut request: ManifestRequest, root: &Path) -> Result<Self> {
        if request.command.is_empty() {
            bail!("manifest command must contain argv[0]");
        }
        let root = fs::canonicalize(root)
            .with_context(|| format!("resolving project root '{}'", root.display()))?;
        let root = simplify_verbatim(&root);
        request.feature_flags.sort();
        request.feature_flags.dedup();

        let mut inputs = request
            .inputs
            .iter()
            .map(hash_input)
            .collect::<Result<Vec<_>>>()?;
        for input in &mut inputs {
            input.path = portable_path(Path::new(&input.path), &root);
        }
        inputs.sort_by(|a, b| (&a.role, &a.path).cmp(&(&b.role, &b.path)));
        for pair in inputs.windows(2) {
            if pair[0].role == pair[1].role && pair[0].path == pair[1].path {
                bail!(
                    "duplicate manifest input '{}' at '{}'",
                    pair[0].role,
                    pair[0].path
                );
            }
        }

        let workspace_version = env!("CARGO_PKG_VERSION").to_string();
        let components = ["engine", "extract", "mcu", "models", "solver"]
            .into_iter()
            .map(|name| (name.to_string(), workspace_version.clone()))
            .collect();

        let mut manifest = Self {
            schema_version: MANIFEST_SCHEMA_VERSION,
            manifest_id: String::new(),
            tool: request.tool,
            components,
            plugins: capture_plugins(),
            build: BuildContract {
                target_os: std::env::consts::OS.to_string(),
                target_arch: std::env::consts::ARCH.to_string(),
                features: request.feature_flags,
            },
            root_hint: Some(root_hint(&root)),
            environment: capture_environment(),
            backends: crate::commands::doctor::resolved_backends(),
            inputs,
            invocation: Invocation {
                argv: request
                    .command
                    .iter()
                    .map(|arg| portable_argument(arg, &root))
                    .collect(),
                options: request
                    .options
                    .into_iter()
                    .map(|(key, value)| (key, portable_value(value, &root)))
                    .collect(),
            },
            reproduce: "hauksbee reproduce <manifest.json>".to_string(),
            outcome: None,
        };
        manifest.manifest_id = manifest.computed_id()?;
        Ok(manifest)
    }

    /// Stable pretty JSON with a single trailing newline. Struct field order,
    /// `BTreeMap` keys, input order, feature order, and environment order are all
    /// canonicalized before this point.
    ///
    /// # Errors
    ///
    /// Returns an error if the manifest cannot be serialized to JSON.
    pub fn canonical_json(&self) -> Result<String> {
        let mut json = serde_json::to_string_pretty(self).context("serializing run manifest")?;
        json.push('\n');
        Ok(json)
    }

    /// Persist without replacing an existing artifact. The complete bytes are
    /// staged beside the destination and installed with `persist_noclobber`, so
    /// interruption cannot expose a partially-written manifest.
    ///
    /// # Errors
    ///
    /// Returns an error if the output directory does not exist or cannot be
    /// resolved, if the output lies inside a hashed input directory, if staging,
    /// writing or syncing the temporary file fails, or if `path` already exists.
    pub fn write_new(&self, path: &Path) -> Result<()> {
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        if !parent.is_dir() {
            bail!(
                "manifest output directory '{}' does not exist",
                parent.display()
            );
        }
        let output_parent = fs::canonicalize(parent).with_context(|| {
            format!("resolving manifest output directory '{}'", parent.display())
        })?;
        let output = output_parent.join(path.file_name().unwrap_or_else(|| OsStr::new("")));
        let root = project_root();
        for input in &self.inputs {
            if input.kind != "directory" {
                continue;
            }
            let input_dir = fs::canonicalize(resolve_stored_path(&input.path, &root))
                .with_context(|| format!("resolving hashed input directory '{}'", input.path))?;
            if output.starts_with(&input_dir) {
                bail!(
                    "manifest output '{}' is inside hashed input directory '{}' (writing it would immediately invalidate the recorded directory digest)",
                    path.display(),
                    input.path
                );
            }
        }
        let mut temp = tempfile::NamedTempFile::new_in(parent)
            .with_context(|| format!("creating manifest beside '{}'", path.display()))?;
        temp.write_all(self.canonical_json()?.as_bytes())
            .with_context(|| format!("writing staged manifest for '{}'", path.display()))?;
        temp.as_file_mut()
            .sync_all()
            .with_context(|| format!("syncing staged manifest for '{}'", path.display()))?;
        temp.persist_noclobber(path).map_err(|e| {
            if e.error.kind() == std::io::ErrorKind::AlreadyExists {
                anyhow::anyhow!(
                    "manifest '{}' already exists; refusing to overwrite immutable evidence",
                    path.display()
                )
            } else {
                anyhow::anyhow!("persisting manifest '{}': {}", path.display(), e.error)
            }
        })?;
        // The run that wrote this manifest records its outcome here when it ends.
        lock_pending().manifest = Some((path.to_path_buf(), self.clone()));
        Ok(())
    }

    /// Read a manifest and check its schema version and self-describing id.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read or parsed, if its
    /// `schema_version` is unsupported, or if its `manifest_id` does not match
    /// the id computed from its content.
    pub fn read_verified(path: &Path) -> Result<Self> {
        let bytes =
            fs::read(path).with_context(|| format!("reading run manifest '{}'", path.display()))?;
        let manifest: Self = serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing run manifest '{}'", path.display()))?;
        if !(OLDEST_READABLE_SCHEMA..=MANIFEST_SCHEMA_VERSION).contains(&manifest.schema_version) {
            bail!(
                "unsupported manifest schema_version {} (this build supports {} to {})",
                manifest.schema_version,
                OLDEST_READABLE_SCHEMA,
                MANIFEST_SCHEMA_VERSION
            );
        }
        let expected = manifest.computed_id()?;
        if manifest.manifest_id != expected {
            bail!(
                "manifest_id mismatch: document says '{}', content computes '{}'",
                manifest.manifest_id,
                expected
            );
        }
        Ok(manifest)
    }

    /// Re-hash every recorded input and compare it with the manifest.
    ///
    /// # Errors
    ///
    /// Returns an error if an input can no longer be read or hashed, or if its
    /// kind, digest, size or file count differs from the recorded value.
    pub fn verify_inputs(&self) -> Result<()> {
        self.verify_inputs_at(&project_root())
    }

    /// As [`Self::verify_inputs`], resolving stored relative paths against `root`.
    ///
    /// # Errors
    ///
    /// Same as [`Self::verify_inputs`].
    pub fn verify_inputs_at(&self, root: &Path) -> Result<()> {
        match self.input_changes_at(root).into_iter().next() {
            Some(change) => bail!("{change}"),
            None => Ok(()),
        }
    }

    /// Every recorded input that no longer matches, one line each: unreadable,
    /// or a different kind, digest, size or file count. Empty means all match.
    #[must_use]
    pub fn input_changes_at(&self, root: &Path) -> Vec<String> {
        let mut changes = Vec::new();
        for expected in &self.inputs {
            let actual = match hash_input(&ManifestInput::new(
                &expected.role,
                resolve_stored_path(&expected.path, root),
            )) {
                Ok(actual) => actual,
                Err(e) => {
                    changes.push(format!(
                        "input '{}' cannot be read at '{}': {e:#}",
                        expected.role, expected.path
                    ));
                    continue;
                }
            };
            if actual.kind != expected.kind
                || actual.digest != expected.digest
                || actual.size_bytes != expected.size_bytes
                || actual.file_count != expected.file_count
            {
                changes.push(format!(
                    "input '{}' digest mismatch at '{}': expected {}, found {}",
                    expected.role, expected.path, expected.digest.value, actual.digest.value
                ));
            }
        }
        changes
    }

    /// Check that the reproducibility environment selectors match this process.
    ///
    /// # Errors
    ///
    /// Returns an error naming each selector whose hashed value (or presence)
    /// differs from the recorded environment; the values themselves are withheld.
    pub fn verify_environment(&self) -> Result<()> {
        let current = capture_environment();
        if current != self.environment {
            let expected: BTreeMap<_, _> = self
                .environment
                .selectors
                .iter()
                .map(|s| (&s.name, &s.value_sha256))
                .collect();
            let actual: BTreeMap<_, _> = current
                .selectors
                .iter()
                .map(|s| (&s.name, &s.value_sha256))
                .collect();
            let names = expected
                .keys()
                .chain(actual.keys())
                .filter(|name| expected.get(*name) != actual.get(*name))
                .map(|name| name.as_str())
                .collect::<Vec<_>>();
            bail!(
                "reproducibility environment mismatch for: {} (values are withheld; set the same selector values used by the original run)",
                names.join(", ")
            );
        }
        Ok(())
    }

    /// Check that the running build's tool identity matches the manifest.
    ///
    /// # Errors
    ///
    /// Returns an error if the tool name, version or git revision differs.
    pub fn verify_tool(&self, current: &ToolIdentity) -> Result<()> {
        if &self.tool != current {
            bail!(
                "tool identity mismatch: manifest needs {} {} {:?}, current build is {} {} {:?}",
                self.tool.name,
                self.tool.version,
                self.tool.git_revision,
                current.name,
                current.version,
                current.git_revision
            );
        }
        Ok(())
    }

    /// Backends recorded here whose version or binary differs on this machine.
    /// Drift is information for the reader, not a reason to refuse a replay: the
    /// outcome comparison is the real test.
    #[must_use]
    pub fn backend_drift(&self) -> Vec<String> {
        if self.backends.is_empty() {
            return Vec::new();
        }
        let current = crate::commands::doctor::resolved_backends();
        let describe = |b: &BackendRevision| {
            let version = b.version.as_deref().unwrap_or("unknown version");
            b.sha256_of_binary.as_deref().map_or_else(
                || version.to_string(),
                |sha| format!("{version}, sha256 {}", &sha[..sha.len().min(12)]),
            )
        };
        self.backends
            .iter()
            .filter_map(
                |recorded| match current.iter().find(|c| c.name == recorded.name) {
                    None => Some(format!(
                        "backend {} was {} when recorded; it is not installed here",
                        recorded.name,
                        describe(recorded)
                    )),
                    Some(now) if now != recorded => Some(format!(
                        "backend {} was {} when recorded; here it is {}",
                        recorded.name,
                        describe(recorded),
                        describe(now)
                    )),
                    Some(_) => None,
                },
            )
            .collect()
    }

    fn computed_id(&self) -> Result<String> {
        let mut unsigned = self.clone();
        unsigned.manifest_id.clear();
        unsigned.outcome = None;
        let bytes = serde_json::to_vec(&unsigned).context("serializing manifest identity")?;
        Ok(format!("sha256:{}", hex_digest(&bytes)))
    }
}

fn capture_plugins() -> Vec<PluginRevision> {
    let mut plugins = hauksbee_models::PackStore::default_location()
        .and_then(|store| store.list().ok())
        .unwrap_or_default()
        .into_iter()
        .map(|record| PluginRevision {
            kind: "model_pack".to_string(),
            name: record.name,
            version: record.version,
            provenance: record.provenance,
        })
        .collect::<Vec<_>>();
    plugins.sort_by(|a, b| (&a.name, &a.version).cmp(&(&b.name, &b.version)));
    plugins
}

/// Current process argv normalized for a portable replay.
///
/// The executable is the published tool name, not a checkout-specific `target/`
/// path, and the emission option is removed so replay cannot overwrite its
/// source manifest.
#[must_use]
pub fn replay_argv(tool_name: &str) -> Vec<String> {
    let mut source = std::env::args().skip(1);
    let mut argv = vec![tool_name.to_string()];
    while let Some(arg) = source.next() {
        if arg == "--emit-manifest" {
            let _ = source.next();
            continue;
        }
        if arg.starts_with("--emit-manifest=") {
            continue;
        }
        argv.push(arg);
    }
    argv
}

/// Replace path-valued argv tokens (including `--flag=PATH`) with absolute paths.
///
/// This removes the original process cwd from the replay contract without
/// serializing that cwd as a separate ambient/private field.
#[must_use]
pub fn absolutize_argv_paths(mut argv: Vec<String>, base: &Path, paths: &[PathBuf]) -> Vec<String> {
    let replacements = paths
        .iter()
        .map(|path| {
            let original = path.display().to_string();
            let absolute = if path.is_absolute() {
                path.clone()
            } else {
                base.join(path)
            };
            (original, absolute.display().to_string())
        })
        .collect::<Vec<_>>();
    for arg in &mut argv {
        for (original, absolute) in &replacements {
            if arg == original {
                arg.clone_from(absolute);
                break;
            }
            if let Some((flag, value)) = arg.split_once('=') {
                if value == original {
                    *arg = format!("{flag}={absolute}");
                    break;
                }
            }
        }
    }
    argv
}

/// Inputs in the implicit model search path. Paths are recorded only when they
/// exist; `$HOME` itself is never serialized as environment metadata.
pub fn implicit_model_inputs() -> Vec<ManifestInput> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Vec::new();
    };
    [
        (
            "installed_model_pack_record",
            home.join(".hauksbee/packs.toml"),
        ),
        ("installed_model_packs", home.join(".hauksbee/packs")),
        ("user_models", home.join(".hauksbee/models")),
        ("user_config_models", home.join(".config/hauksbee/models")),
    ]
    .into_iter()
    .filter(|(_, path)| path.exists())
    .map(|(role, path)| ManifestInput::new(role, path))
    .collect()
}

/// Auto-discovered files beside a board which change a run without appearing in argv.
///
/// These are `KiCad` project clearance rules and Hauksbee waivers, which are not
/// explicit CLI arguments. Missing sidecars are the ordinary case and are
/// omitted.
pub fn board_sidecar_inputs(board: &Path, role_prefix: &str) -> Vec<ManifestInput> {
    let mut inputs = Vec::new();
    if board
        .extension()
        .and_then(OsStr::to_str)
        .is_some_and(|ext| matches!(ext, "kicad_pcb" | "kicad_sch"))
    {
        let project = board.with_extension("kicad_pro");
        if project.is_file() {
            inputs.push(ManifestInput::new(
                format!("{role_prefix}.kicad_project"),
                project,
            ));
        }
    }
    let waiver = board
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(crate::waiver::DEFAULT_WAIVER_FILE);
    if waiver.is_file() {
        inputs.push(ManifestInput::new(format!("{role_prefix}.waivers"), waiver));
    }
    inputs
}

/// Exit code of `reproduce` when the replay does not match the recorded outcome.
pub const EXIT_REPRODUCE_DIFF: i32 = 1;

/// Verify, re-run and compare a manifest through one of the two fixed run
/// binaries.
///
/// The document cannot nominate an arbitrary program: only `hauksbee` and
/// `hauksbee-ci` are accepted, and `argv[0]` must agree with the signed tool
/// name. The run executes from the project root, so root-relative paths
/// resolve. When the manifest recorded an [`Outcome`], a `MATCH` or `DIFF`
/// verdict is printed to stderr (the replayed run's own report keeps stdout) and
/// the process exits 0 on a match and [`EXIT_REPRODUCE_DIFF`] on a difference.
/// Recorded input bytes that no longer match are a difference too, and in that
/// case the run is not repeated. A manifest with no recorded outcome exits with
/// the replayed run's own code. Timings and other report text are never compared.
///
/// # Errors
///
/// Returns an error if the manifest fails [`RunManifest::read_verified`], names
/// an unsupported tool or a mismatched `argv[0]`, fails the tool or environment
/// verification, or if the executable cannot be located or launched.
pub fn reproduce(path: &Path) -> Result<()> {
    let manifest = RunManifest::read_verified(path)?;
    if !matches!(manifest.tool.name.as_str(), "hauksbee" | "hauksbee-ci") {
        bail!("unsupported manifest tool '{}'", manifest.tool.name);
    }
    if manifest.invocation.argv.first().map(String::as_str) != Some(manifest.tool.name.as_str()) {
        bail!("manifest invocation argv[0] does not match its tool identity");
    }
    manifest.verify_tool(&ToolIdentity::workspace(&manifest.tool.name))?;
    manifest.verify_environment()?;
    let root = project_root();
    for note in manifest.backend_drift() {
        eprintln!("note: {note}");
    }
    let changed = manifest.input_changes_at(&root);
    if !changed.is_empty() {
        report_diff(&manifest, &changed);
        std::process::exit(EXIT_REPRODUCE_DIFF);
    }

    let current = std::env::current_exe().context("locating the hauksbee executable")?;
    let executable = if manifest.tool.name == "hauksbee" {
        current
    } else {
        current.with_file_name(if cfg!(windows) {
            "hauksbee-ci.exe"
        } else {
            "hauksbee-ci"
        })
    };
    eprintln!(
        "verified immutable run manifest {} ({} input{}); reproducing with {}",
        manifest.manifest_id,
        manifest.inputs.len(),
        if manifest.inputs.len() == 1 { "" } else { "s" },
        manifest.tool.name
    );
    let outcome_file = tempfile::NamedTempFile::new().context("creating the outcome file")?;
    let status = std::process::Command::new(&executable)
        .args(manifest.invocation.argv.iter().skip(1))
        .current_dir(&root)
        .env(OUTCOME_FILE_ENV, outcome_file.path())
        .status()
        .with_context(|| format!("launching '{}'", executable.display()))?;
    let code = status.code().unwrap_or(3);
    let Some(recorded) = &manifest.outcome else {
        eprintln!(
            "reproduced: exit {code}; this manifest recorded no outcome to compare with \
             (re-emit it with --emit-manifest to get MATCH or DIFF)"
        );
        std::process::exit(code);
    };
    // A replay that died before reaching its end-of-run hook left nothing in the
    // file; the exit code alone still compares.
    let ReplayOutcome {
        outcome: observed,
        error,
    } = fs::read(outcome_file.path())
        .ok()
        .and_then(|bytes| serde_json::from_slice::<ReplayOutcome>(&bytes).ok())
        .unwrap_or_else(|| ReplayOutcome {
            outcome: Outcome {
                verdict: outcome_verdict(&manifest.tool.name, code, false).to_string(),
                exit_code: code,
                record_sha256: None,
                gating_findings: Vec::new(),
            },
            error: None,
        });
    let differences = compare_outcomes(recorded, &observed);
    if differences.is_empty() {
        eprintln!(
            "MATCH  verdict {}, exit {}, {} gating finding{} (manifest {})",
            observed.verdict,
            observed.exit_code,
            observed.gating_findings.len(),
            if observed.gating_findings.len() == 1 {
                ""
            } else {
                "s"
            },
            manifest.manifest_id
        );
        std::process::exit(0);
    }
    report_diff(&manifest, &differences);
    if let Some(error) = error {
        eprintln!("  the replayed run failed with: {error}");
    } else if observed.verdict == "error" {
        eprintln!(
            "  the replayed run exited {code} without recording why; \
             run `{}` in {} to see its message",
            manifest.invocation.argv.join(" "),
            root.display()
        );
    }
    std::process::exit(EXIT_REPRODUCE_DIFF);
}

fn report_diff(manifest: &RunManifest, differences: &[String]) {
    eprintln!(
        "DIFF  this run does not reproduce manifest {}",
        manifest.manifest_id
    );
    for line in differences {
        eprintln!("  {line}");
    }
}

/// What differs between a recorded outcome and an observed one: verdict, exit
/// code, and gating findings by id. Empty means they match. Other fields, such
/// as `record_sha256`, are not compared.
#[must_use]
pub fn compare_outcomes(recorded: &Outcome, observed: &Outcome) -> Vec<String> {
    const SHOWN: usize = 10;
    let mut lines = Vec::new();
    if recorded.verdict != observed.verdict {
        lines.push(format!(
            "verdict: {} -> {}",
            recorded.verdict, observed.verdict
        ));
    }
    if recorded.exit_code != observed.exit_code {
        lines.push(format!(
            "exit code: {} -> {}",
            recorded.exit_code, observed.exit_code
        ));
    }
    for (label, from, to) in [
        (
            "new gating finding",
            &observed.gating_findings,
            &recorded.gating_findings,
        ),
        (
            "gating finding gone",
            &recorded.gating_findings,
            &observed.gating_findings,
        ),
    ] {
        let only: Vec<&String> = from.iter().filter(|id| !to.contains(id)).collect();
        lines.extend(only.iter().take(SHOWN).map(|id| format!("{label}: {id}")));
        if only.len() > SHOWN {
            lines.push(format!("{label}: and {} more", only.len() - SHOWN));
        }
    }
    lines
}

/// The verdict word for a finished run.
///
/// `hauksbee` exits 2 for findings and 1
/// for bad input; `hauksbee-ci` exits 1 for a red assertion and 2 for a spec
/// error; both use 3 for a run that cannot be trusted. Gating findings under a
/// non-strict exit 0 are still a failing verdict.
#[must_use]
pub fn outcome_verdict(tool: &str, exit_code: i32, has_gating: bool) -> &'static str {
    match (tool, exit_code) {
        (_, 3) => "invalid",
        ("hauksbee-ci", 1) | ("hauksbee", 2) => "fail",
        (_, 0) if has_gating => "fail",
        (_, 0) => "pass",
        _ => "error",
    }
}

/// The verdict a finished run records.
///
/// It is the run's own machine verdict (`noted`, what its `--json` prints)
/// when it computed one and exited 0, else [`outcome_verdict`] from the exit
/// code. A non-strict run exits 0 over a
/// partly judged result (a failed solve, an unmodelled part), so the exit code
/// alone would record that as a pass.
#[must_use]
pub fn recorded_verdict(
    tool: &str,
    exit_code: i32,
    has_gating: bool,
    noted: Option<&'static str>,
) -> &'static str {
    match noted {
        Some("pass") | None => outcome_verdict(tool, exit_code, has_gating),
        Some(verdict) if exit_code == 0 => verdict,
        Some(_) => outcome_verdict(tool, exit_code, has_gating),
    }
}

#[derive(Default)]
struct Pending {
    manifest: Option<(PathBuf, RunManifest)>,
    gating: BTreeMap<String, Vec<String>>,
    record_sha256: Option<String>,
    verdict: Option<&'static str>,
    error: Option<String>,
    finished: bool,
}

static PENDING: Mutex<Pending> = Mutex::new(Pending {
    manifest: None,
    gating: BTreeMap::new(),
    record_sha256: None,
    verdict: None,
    error: None,
    finished: false,
});

fn lock_pending() -> std::sync::MutexGuard<'static, Pending> {
    PENDING.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Whether this process will record an outcome when it ends: it wrote a
/// manifest with `--emit-manifest`, or `reproduce` asked for one. Callers use it
/// to gather findings they would otherwise skip.
#[must_use]
pub fn tracking_outcome() -> bool {
    lock_pending().manifest.is_some() || std::env::var_os(OUTCOME_FILE_ENV).is_some()
}

/// Replace the gating finding ids for `scope` (an empty scope for a single-run
/// tool, `spec[i]` for the i-th CI spec). Cheap and safe to call when nothing is
/// tracked.
pub fn note_gating_findings(scope: &str, ids: impl IntoIterator<Item = String>) {
    lock_pending()
        .gating
        .insert(scope.to_string(), ids.into_iter().collect());
}

/// Hook for the run's own machine verdict.
///
/// `pass`, `fail` or `invalid`, as its `--json` states it, so the recorded
/// outcome is that verdict rather than one re-derived from the exit code.
/// Cheap and safe when nothing is tracked.
pub fn note_run_verdict(verdict: &'static str) {
    lock_pending().verdict = Some(verdict);
}

/// Hook for the error that ended the run. `reproduce` shows it beside a DIFF so
/// an `error` verdict always says why; it is not stored in any manifest.
pub fn note_run_error(message: impl Into<String>) {
    lock_pending().error = Some(message.into());
}

/// Hook for the run record: once a run has written its record, pass the record's
/// hex SHA-256 here and the manifest's outcome carries it.
pub fn note_record_sha256(hex: impl Into<String>) {
    lock_pending().record_sha256 = Some(hex.into());
}

/// Record how the run ended, once, in the manifest it wrote (and in the file
/// `reproduce` named, if any). Failure to record never changes the run's exit
/// code; it is reported on stderr.
pub fn finish_outcome(tool: &str, exit_code: i32) {
    let outcome_file = std::env::var_os(OUTCOME_FILE_ENV).map(PathBuf::from);
    let (manifest, outcome, error) = {
        let mut pending = lock_pending();
        if pending.finished || (pending.manifest.is_none() && outcome_file.is_none()) {
            return;
        }
        pending.finished = true;
        let mut gating: Vec<String> = pending
            .gating
            .iter()
            .flat_map(|(scope, ids)| {
                ids.iter().map(move |id| {
                    if scope.is_empty() {
                        id.clone()
                    } else {
                        format!("{scope}:{id}")
                    }
                })
            })
            .collect();
        gating.sort();
        gating.dedup();
        let outcome = Outcome {
            verdict: recorded_verdict(tool, exit_code, !gating.is_empty(), pending.verdict)
                .to_string(),
            exit_code,
            record_sha256: pending.record_sha256.take(),
            gating_findings: gating,
        };
        (pending.manifest.take(), outcome, pending.error.take())
    };
    if let Some(file) = outcome_file {
        let replay = ReplayOutcome {
            outcome: outcome.clone(),
            error,
        };
        if let Err(e) = serde_json::to_vec(&replay)
            .map_err(anyhow::Error::from)
            .and_then(|bytes| fs::write(&file, bytes).map_err(anyhow::Error::from))
        {
            eprintln!(
                "warning: could not write the run outcome to '{}': {e}",
                file.display()
            );
        }
    }
    if let Some((path, mut manifest)) = manifest {
        manifest.outcome = Some(outcome);
        if let Err(e) = replace_with_outcome(&path, &manifest) {
            eprintln!(
                "warning: could not record the run outcome in manifest '{}': {e:#}",
                path.display()
            );
        }
    }
}

/// Atomically replace the manifest this run wrote with the same manifest plus
/// its outcome. It refuses if the file on disk is no longer exactly that
/// manifest, so nothing else's evidence is ever overwritten.
fn replace_with_outcome(path: &Path, manifest: &RunManifest) -> Result<()> {
    let on_disk = RunManifest::read_verified(path)?;
    if on_disk.manifest_id != manifest.manifest_id || on_disk.outcome.is_some() {
        bail!("the file is no longer the manifest this run wrote");
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(manifest.canonical_json()?.as_bytes())?;
    temp.as_file_mut().sync_all()?;
    temp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

/// The directory manifest paths are relative to: the git checkout containing the
/// working directory, else the working directory.
#[must_use]
pub fn project_root() -> PathBuf {
    let cwd = std::env::current_dir()
        .and_then(fs::canonicalize)
        .map_or_else(|_| PathBuf::from("."), |dir| simplify_verbatim(&dir));
    cwd.ancestors()
        .find(|dir| dir.join(".git").exists())
        .map_or_else(|| cwd.clone(), Path::to_path_buf)
}

/// `text` without a Windows verbatim prefix: `\\?\C:\x` is `C:\x` and
/// `\\?\UNC\host\share\x` is `\\host\share\x`. Anything else, including verbatim
/// forms with no ordinary spelling (`\\?\Volume{..}`), is returned unchanged.
/// Pure text, so it behaves the same on every platform.
fn strip_verbatim_prefix(text: &str) -> std::borrow::Cow<'_, str> {
    use std::borrow::Cow;
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return Cow::Owned(format!(r"\\{rest}"));
    }
    match text.strip_prefix(r"\\?\") {
        Some(rest) if matches!(rest.as_bytes(), [drive, b':', ..] if drive.is_ascii_alphabetic()) => {
            Cow::Borrowed(rest)
        }
        _ => Cow::Borrowed(text),
    }
}

/// `path` with `canonicalize`'s Windows `\\?\` spelling replaced by the ordinary
/// one, so it compares equal to `current_dir()` and to the paths a user types.
/// A no-op for any path that has no such prefix.
fn simplify_verbatim(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    match strip_verbatim_prefix(&text) {
        std::borrow::Cow::Borrowed(stripped) if stripped.len() == text.len() => path.to_path_buf(),
        stripped => PathBuf::from(stripped.into_owned()),
    }
}

/// The canonical, ordinary-spelling form of `path` for comparing it with the
/// project root: the path itself when it exists, else its parent plus the file
/// name (an output that is not written yet). `None` when neither resolves.
fn canonical_for_root(path: &Path) -> Option<PathBuf> {
    if let Ok(resolved) = fs::canonicalize(path) {
        return Some(simplify_verbatim(&resolved));
    }
    let name = path.file_name()?;
    let parent = fs::canonicalize(path.parent()?).ok()?;
    Some(simplify_verbatim(&parent).join(name))
}

fn root_hint(root: &Path) -> String {
    let name = root.file_name().map_or_else(
        || "project".to_string(),
        |n| n.to_string_lossy().into_owned(),
    );
    let kind = if root.join(".git").exists() {
        "git checkout root"
    } else {
        "working directory"
    };
    format!("{name} ({kind})")
}

fn slash_path(path: &Path) -> String {
    let joined = path
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    if joined.is_empty() {
        ".".to_string()
    } else {
        joined
    }
}

/// `path` as stored: relative to `root`, else `~/...` under the home
/// directory, else unchanged.
fn portable_path(path: &Path, root: &Path) -> String {
    if let Ok(relative) = path.strip_prefix(root) {
        return slash_path(relative);
    }
    let home = std::env::var_os("HOME")
        .and_then(|h| fs::canonicalize(h).ok())
        .map(|h| simplify_verbatim(&h));
    if let Some(relative) = home.as_deref().and_then(|h| path.strip_prefix(h).ok()) {
        return format!("~/{}", slash_path(relative));
    }
    path.display().to_string()
}

/// The inverse of [`portable_path`]. Absolute paths (older manifests) are kept.
fn resolve_stored_path(stored: &str, root: &Path) -> PathBuf {
    if let Some(rest) = stored.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    let path = Path::new(stored);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

fn relative_to_root(text: &str, root: &Path) -> String {
    let path = Path::new(text);
    if !path.is_absolute() {
        return text.to_string();
    }
    if let Ok(relative) = path.strip_prefix(root) {
        return slash_path(relative);
    }
    // The same place spelled differently (a `\\?\` prefix, a Windows 8.3 short
    // name or another letter case, `..`, a symlinked ancestor) must still come
    // out project-relative, or a replay elsewhere would chase the capturing
    // directory.
    canonical_for_root(path)
        .as_deref()
        .and_then(|resolved| resolved.strip_prefix(root).ok())
        .map_or_else(|| text.to_string(), slash_path)
}

/// An argv token with an absolute path under `root` made root-relative, also
/// inside `--flag=PATH`.
fn portable_argument(arg: &str, root: &Path) -> String {
    match arg.split_once('=') {
        Some((flag, value)) if flag.starts_with("--") => {
            format!("{flag}={}", relative_to_root(value, root))
        }
        _ => relative_to_root(arg, root),
    }
}

fn portable_value(value: serde_json::Value, root: &Path) -> serde_json::Value {
    use serde_json::Value;
    match value {
        Value::String(text) => Value::String(relative_to_root(&text, root)),
        Value::Array(items) => {
            Value::Array(items.into_iter().map(|v| portable_value(v, root)).collect())
        }
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(k, v)| (k, portable_value(v, root)))
                .collect(),
        ),
        other => other,
    }
}

fn capture_environment() -> EnvironmentContract {
    let selectors = REPRODUCIBILITY_ENV
        .iter()
        .filter_map(|name| {
            std::env::var_os(name).map(|value| EnvironmentSelector {
                name: (*name).to_string(),
                value_sha256: hex_digest(&os_bytes(&value)),
            })
        })
        .collect();
    EnvironmentContract {
        value_policy: "sha256_only".to_string(),
        selectors,
    }
}

#[cfg(unix)]
fn os_bytes(value: &OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    value.as_bytes().to_vec()
}

#[cfg(not(unix))]
fn os_bytes(value: &OsStr) -> Vec<u8> {
    value.to_string_lossy().as_bytes().to_vec()
}

fn hash_input(input: &ManifestInput) -> Result<HashedInput> {
    let metadata = fs::symlink_metadata(&input.path)
        .with_context(|| format!("reading manifest input '{}'", input.path.display()))?;
    if metadata.file_type().is_symlink() {
        bail!(
            "manifest input '{}' is a symlink; use the resolved file or directory so the hashed target is explicit",
            input.path.display()
        );
    }
    let resolved = fs::canonicalize(&input.path)
        .with_context(|| format!("resolving manifest input '{}'", input.path.display()))?;
    let (kind, value, size_bytes, file_count) = if metadata.is_file() {
        let bytes = match &input.retained_bytes {
            Some(bytes) => bytes.clone(),
            None => fs::read(&resolved)
                .with_context(|| format!("hashing manifest input '{}'", input.path.display()))?,
        };
        ("file", hex_digest(&bytes), bytes.len() as u64, None)
    } else if metadata.is_dir() {
        let files = directory_files(&resolved)?;
        let mut hasher = Sha256::new();
        hasher.update(b"hauksbee-directory-v1\0");
        let mut total = 0u64;
        for (relative, path) in &files {
            let bytes = fs::read(path)
                .with_context(|| format!("hashing manifest input '{}'", path.display()))?;
            total += bytes.len() as u64;
            let relative = relative.as_bytes();
            hasher.update((relative.len() as u64).to_le_bytes());
            hasher.update(relative);
            hasher.update((bytes.len() as u64).to_le_bytes());
            hasher.update(Sha256::digest(&bytes));
        }
        (
            "directory",
            bytes_to_hex(&hasher.finalize()),
            total,
            Some(files.len() as u64),
        )
    } else {
        bail!(
            "manifest input '{}' is neither a regular file nor a directory",
            input.path.display()
        );
    };
    Ok(HashedInput {
        role: input.role.clone(),
        path: simplify_verbatim(&resolved).display().to_string(),
        kind: kind.to_string(),
        digest: Digest {
            algorithm: "sha256".to_string(),
            value,
        },
        size_bytes,
        file_count,
    })
}

fn directory_files(root: &Path) -> Result<Vec<(String, PathBuf)>> {
    fn visit(root: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) -> Result<()> {
        for entry in fs::read_dir(dir)
            .with_context(|| format!("reading manifest input directory '{}'", dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path)?;
            if metadata.file_type().is_symlink() {
                bail!(
                    "manifest input directory '{}' contains symlink '{}'; resolved inputs must be explicit",
                    root.display(),
                    path.display()
                );
            }
            if metadata.is_dir() {
                visit(root, &path, out)?;
            } else if metadata.is_file() {
                let relative = path
                    .strip_prefix(root)
                    .expect("walk stays under root")
                    .to_string_lossy()
                    .replace(std::path::MAIN_SEPARATOR, "/");
                out.push((relative, path));
            } else {
                bail!(
                    "manifest input directory '{}' contains non-file entry '{}'",
                    root.display(),
                    path.display()
                );
            }
        }
        Ok(())
    }

    let mut files = Vec::new();
    visit(root, root, &mut files)?;
    files.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(files)
}

/// Lowercase hex SHA-256 of `bytes`.
pub(crate) fn hex_digest(bytes: &[u8]) -> String {
    bytes_to_hex(&Sha256::digest(bytes))
}

pub(crate) fn bytes_to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut out, "{byte:02x}").expect("writing to String cannot fail");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(board: &Path, argument: &str) -> ManifestRequest {
        ManifestRequest {
            tool: ToolIdentity::new("hauksbee", "0.1.0", Some("0123456789ab")),
            command: vec!["hauksbee".into(), "run".into(), argument.into()],
            options: BTreeMap::from([("board".into(), serde_json::json!(argument))]),
            inputs: vec![ManifestInput::new("board", board)],
            feature_flags: Vec::new(),
        }
    }

    #[test]
    fn paths_under_the_root_are_stored_relative_and_resolve_in_a_copy() {
        let first = tempfile::tempdir().unwrap();
        let board = first.path().join("sub/board.kicad_pcb");
        fs::create_dir(first.path().join("sub")).unwrap();
        fs::write(&board, "bytes\n").unwrap();
        let root = fs::canonicalize(first.path()).unwrap();
        let arg = root.join("sub/board.kicad_pcb").display().to_string();

        let manifest = RunManifest::capture_at(request(&board, &arg), first.path()).unwrap();
        assert_eq!(manifest.inputs[0].path, "sub/board.kicad_pcb");
        assert_eq!(manifest.invocation.argv[2], "sub/board.kicad_pcb");
        assert_eq!(
            manifest.invocation.options["board"],
            serde_json::json!("sub/board.kicad_pcb")
        );
        assert!(manifest.root_hint.is_some());
        assert!(!manifest
            .canonical_json()
            .unwrap()
            .contains(root.to_str().unwrap()));

        let second = tempfile::tempdir().unwrap();
        fs::create_dir(second.path().join("sub")).unwrap();
        fs::copy(&board, second.path().join("sub/board.kicad_pcb")).unwrap();
        manifest.verify_inputs_at(second.path()).unwrap();
        assert!(manifest.input_changes_at(second.path()).is_empty());

        fs::write(second.path().join("sub/board.kicad_pcb"), "bytez\n").unwrap();
        let changes = manifest.input_changes_at(second.path());
        assert_eq!(changes.len(), 1, "{changes:?}");
        assert!(changes[0].contains("digest mismatch"), "{changes:?}");
    }

    #[test]
    fn windows_verbatim_prefixes_are_stripped_and_nothing_else_is() {
        let strip = |text: &str| strip_verbatim_prefix(text).into_owned();
        // The faulty spellings `canonicalize` produces on Windows.
        assert_eq!(strip(r"\\?\C:\Users\me\proj"), r"C:\Users\me\proj");
        assert_eq!(strip(r"\\?\d:/proj"), "d:/proj");
        assert_eq!(strip(r"\\?\UNC\host\share\proj"), r"\\host\share\proj");
        // Controls: ordinary paths and verbatim forms with no plain spelling.
        assert_eq!(strip(r"C:\Users\me\proj"), r"C:\Users\me\proj");
        assert_eq!(strip("/home/me/proj"), "/home/me/proj");
        assert_eq!(strip("board.kicad_pcb"), "board.kicad_pcb");
        assert_eq!(strip(r"\\?\Volume{1234}\proj"), r"\\?\Volume{1234}\proj");
        assert_eq!(strip(r"\\?\1:\proj"), r"\\?\1:\proj");
        assert_eq!(strip(r"\\host\share\proj"), r"\\host\share\proj");
    }

    #[cfg(unix)]
    #[test]
    fn an_equivalent_spelling_of_a_path_under_the_root_is_stored_relative() {
        // The root reached by another spelling, as a Windows 8.3 name, another
        // letter case or a `\\?\` prefix would reach it: here a symlink alias.
        let dir = tempfile::tempdir().unwrap();
        let root = simplify_verbatim(&fs::canonicalize(dir.path()).unwrap());
        fs::write(root.join("board.kicad_pcb"), "x").unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let alias = elsewhere.path().join("alias");
        std::os::unix::fs::symlink(&root, &alias).unwrap();
        let spelled = alias.join("board.kicad_pcb");
        assert!(
            spelled.strip_prefix(&root).is_err(),
            "plain strip is fooled"
        );
        assert_eq!(
            relative_to_root(&spelled.display().to_string(), &root),
            "board.kicad_pcb"
        );
        // An output that is not written yet resolves through its parent.
        let pending = alias.join("out.json");
        assert_eq!(
            relative_to_root(&pending.display().to_string(), &root),
            "out.json"
        );
        // `--flag=PATH` takes the same route.
        assert_eq!(
            portable_argument(&format!("--junit={}", pending.display()), &root),
            "--junit=out.json"
        );
        // Controls: outside the root and relative tokens are left alone.
        let outside = fs::canonicalize(elsewhere.path())
            .unwrap()
            .display()
            .to_string();
        assert_eq!(relative_to_root(&outside, &root), outside);
        assert_eq!(
            relative_to_root("board.kicad_pcb", &root),
            "board.kicad_pcb"
        );
    }

    #[test]
    fn a_replayed_runs_error_travels_beside_its_outcome_but_never_into_it() {
        let outcome = outcome("error", 1, &[]);
        let sent = serde_json::to_vec(&ReplayOutcome {
            outcome: outcome.clone(),
            error: Some("reading 'board.kicad_pcb': not found".into()),
        })
        .unwrap();
        let got: ReplayOutcome = serde_json::from_slice(&sent).unwrap();
        assert_eq!(got.outcome, outcome);
        assert_eq!(
            got.error.as_deref(),
            Some("reading 'board.kicad_pcb': not found")
        );
        // The manifest's own outcome shape carries no error text, and a bare
        // outcome (an older replay) still reads, with no error.
        let plain = serde_json::to_string(&outcome).unwrap();
        assert!(!plain.contains("error\":"), "{plain}");
        let old: ReplayOutcome = serde_json::from_str(&plain).unwrap();
        assert_eq!((old.outcome, old.error), (outcome, None));
    }

    #[test]
    fn schema_one_manifests_with_absolute_paths_still_verify() {
        let dir = tempfile::tempdir().unwrap();
        let board = dir.path().join("board.kicad_pcb");
        fs::write(&board, "bytes\n").unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        // The root does not contain the board, so its path stays absolute, as in
        // every manifest written before schema 2.
        let mut old = RunManifest::capture_at(
            request(&board, &board.display().to_string()),
            elsewhere.path(),
        )
        .unwrap();
        old.schema_version = 1;
        old.root_hint = None;
        old.backends.clear();
        old.outcome = None;
        old.manifest_id = old.computed_id().unwrap();
        let json = old.canonical_json().unwrap();
        assert!(!json.contains("root_hint") && !json.contains("backends"));
        assert!(!json.contains("outcome"));

        let file = elsewhere.path().join("old.json");
        fs::write(&file, json).unwrap();
        let loaded = RunManifest::read_verified(&file).unwrap();
        assert!(Path::new(&loaded.inputs[0].path).is_absolute());
        loaded.verify_inputs_at(elsewhere.path()).unwrap();
        assert!(loaded.outcome.is_none());
    }

    #[test]
    fn the_outcome_is_recorded_once_without_changing_the_manifest_identity() {
        let dir = tempfile::tempdir().unwrap();
        let board = dir.path().join("board.kicad_pcb");
        fs::write(&board, "bytes\n").unwrap();
        let manifest =
            RunManifest::capture_at(request(&board, &board.display().to_string()), dir.path())
                .unwrap();
        let file = dir.path().join("m.json");
        manifest.write_new(&file).unwrap();
        assert!(tracking_outcome());

        note_gating_findings("", ["lint/unrouted_pad/serious/711e36fbcb84".to_string()]);
        note_gating_findings("spec[1]", ["bus voltage".to_string()]);
        finish_outcome("hauksbee", 2);
        finish_outcome("hauksbee", 0); // take-once: a later call changes nothing

        let on_disk = RunManifest::read_verified(&file).unwrap();
        assert_eq!(on_disk.manifest_id, manifest.manifest_id);
        let outcome = on_disk.outcome.expect("outcome recorded");
        assert_eq!(outcome.verdict, "fail");
        assert_eq!(outcome.exit_code, 2);
        assert_eq!(outcome.record_sha256, None);
        assert_eq!(
            outcome.gating_findings,
            [
                "lint/unrouted_pad/serious/711e36fbcb84",
                "spec[1]:bus voltage"
            ]
        );
        assert!(fs::read_to_string(&file)
            .unwrap()
            .contains("\"record_sha256\": null"));
    }

    fn outcome(verdict: &str, exit_code: i32, findings: &[&str]) -> Outcome {
        Outcome {
            verdict: verdict.into(),
            exit_code,
            record_sha256: None,
            gating_findings: findings.iter().map(|f| (*f).to_string()).collect(),
        }
    }

    #[test]
    fn outcomes_compare_on_verdict_exit_code_and_gating_findings_only() {
        let base = outcome("fail", 2, &["a", "b"]);
        let mut same = outcome("fail", 2, &["a", "b"]);
        same.record_sha256 = Some("ff".into());
        assert!(compare_outcomes(&base, &same).is_empty());

        let diff = compare_outcomes(&base, &outcome("pass", 0, &["b", "c"]));
        assert_eq!(
            diff,
            [
                "verdict: fail -> pass",
                "exit code: 2 -> 0",
                "new gating finding: c",
                "gating finding gone: a",
            ]
        );
    }

    #[test]
    fn verdict_words_follow_each_tools_exit_codes() {
        assert_eq!(outcome_verdict("hauksbee", 0, false), "pass");
        assert_eq!(outcome_verdict("hauksbee", 0, true), "fail");
        assert_eq!(outcome_verdict("hauksbee", 2, false), "fail");
        assert_eq!(outcome_verdict("hauksbee", 1, false), "error");
        assert_eq!(outcome_verdict("hauksbee-ci", 1, false), "fail");
        assert_eq!(outcome_verdict("hauksbee-ci", 2, false), "error");
        assert_eq!(outcome_verdict("hauksbee-ci", 3, false), "invalid");
    }

    /// A non-strict run whose solve failed exits 0 with a partly judged
    /// (`invalid`) machine verdict: the manifest records that verdict, never
    /// the pass the exit code alone implies. A converged run that passed
    /// still records a pass, and a non-zero exit keeps its own word.
    #[test]
    fn the_recorded_verdict_is_the_runs_own_never_a_pass_over_a_failed_solve() {
        assert_eq!(
            recorded_verdict("hauksbee", 0, false, Some("invalid")),
            "invalid"
        );
        assert_eq!(recorded_verdict("hauksbee", 0, false, Some("pass")), "pass");
        assert_eq!(recorded_verdict("hauksbee", 0, false, None), "pass");
        assert_eq!(recorded_verdict("hauksbee", 0, true, Some("pass")), "fail");
        assert_eq!(
            recorded_verdict("hauksbee", 3, false, Some("pass")),
            "invalid"
        );
        assert_eq!(
            recorded_verdict("hauksbee", 2, false, Some("invalid")),
            "fail"
        );
    }
}
