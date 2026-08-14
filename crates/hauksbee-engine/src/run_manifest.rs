//! Immutable, content-addressed run manifests shared by the human CLI and CI.
//!
//! A manifest is intentionally a deterministic *reproduction identity*, not a
//! process dump. It records exact input bytes, ordered arguments, normalized
//! options, workspace component versions, compiled features, and only the
//! small allowlist of environment selectors that can change model/backend
//! selection. Environment values are hashed: replay can prove the selector is
//! the same without publishing a username-bearing path. Timestamps, cwd,
//! hostname, PATH, debug flags, and secrets never enter the document.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

pub const MANIFEST_SCHEMA_VERSION: u32 = 1;

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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Invocation {
    pub argv: Vec<String>,
    pub options: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    pub environment: EnvironmentContract,
    pub inputs: Vec<HashedInput>,
    pub invocation: Invocation,
    pub reproduce: String,
}

impl RunManifest {
    pub fn capture(mut request: ManifestRequest) -> Result<Self> {
        if request.command.is_empty() {
            bail!("manifest command must contain argv[0]");
        }
        request.feature_flags.sort();
        request.feature_flags.dedup();

        let mut inputs = request
            .inputs
            .iter()
            .map(hash_input)
            .collect::<Result<Vec<_>>>()?;
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
            environment: capture_environment(),
            inputs,
            invocation: Invocation {
                argv: request.command,
                options: request.options,
            },
            reproduce: "hauksbee reproduce <manifest.json>".to_string(),
        };
        manifest.manifest_id = manifest.computed_id()?;
        Ok(manifest)
    }

    /// Stable pretty JSON with a single trailing newline. Struct field order,
    /// BTreeMap keys, input order, feature order, and environment order are all
    /// canonicalized before this point.
    pub fn canonical_json(&self) -> Result<String> {
        let mut json = serde_json::to_string_pretty(self).context("serializing run manifest")?;
        json.push('\n');
        Ok(json)
    }

    /// Persist without replacing an existing artifact. The complete bytes are
    /// staged beside the destination and installed with `persist_noclobber`, so
    /// interruption cannot expose a partially-written manifest.
    pub fn write_new(&self, path: &Path) -> Result<()> {
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
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
        for input in &self.inputs {
            if input.kind != "directory" {
                continue;
            }
            let input_dir = fs::canonicalize(&input.path)
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
        Ok(())
    }

    pub fn read_verified(path: &Path) -> Result<Self> {
        let bytes =
            fs::read(path).with_context(|| format!("reading run manifest '{}'", path.display()))?;
        let manifest: Self = serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing run manifest '{}'", path.display()))?;
        if manifest.schema_version != MANIFEST_SCHEMA_VERSION {
            bail!(
                "unsupported manifest schema_version {} (this build supports {})",
                manifest.schema_version,
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

    pub fn verify_inputs(&self) -> Result<()> {
        for expected in &self.inputs {
            let actual = hash_input(&ManifestInput::new(&expected.role, &expected.path))?;
            if actual.kind != expected.kind
                || actual.digest != expected.digest
                || actual.size_bytes != expected.size_bytes
                || actual.file_count != expected.file_count
            {
                bail!(
                    "input '{}' digest mismatch at '{}': expected {}, found {}",
                    expected.role,
                    expected.path,
                    expected.digest.value,
                    actual.digest.value
                );
            }
        }
        Ok(())
    }

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

    fn computed_id(&self) -> Result<String> {
        let mut unsigned = self.clone();
        unsigned.manifest_id.clear();
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

/// Current process argv normalized for a portable replay. The executable is
/// the published tool name, not a checkout-specific `target/` path, and the
/// emission option is removed so replay cannot overwrite its source manifest.
pub fn replay_argv(tool_name: &str) -> Vec<String> {
    let mut source = std::env::args().skip(1).peekable();
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

/// Replace path-valued argv tokens (including `--flag=PATH`) with absolute
/// paths. This removes the original process cwd from the replay contract
/// without serializing that cwd as a separate ambient/private field.
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
                *arg = absolute.clone();
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

/// Auto-discovered files beside a board which change a run without appearing
/// as explicit CLI arguments: KiCad project clearance rules and Hauksbee
/// waivers. Missing sidecars are the ordinary case and are omitted.
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

/// Verify and execute a manifest through one of the two fixed run binaries.
/// The document cannot nominate an arbitrary program: only `hauksbee` and
/// `hauksbee-ci` are accepted, and `argv[0]` must agree with the signed tool name.
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
    manifest.verify_inputs()?;

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
    let status = std::process::Command::new(&executable)
        .args(manifest.invocation.argv.iter().skip(1))
        .status()
        .with_context(|| format!("launching '{}'", executable.display()))?;
    std::process::exit(status.code().unwrap_or(3));
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
        path: resolved.display().to_string(),
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

fn hex_digest(bytes: &[u8]) -> String {
    bytes_to_hex(&Sha256::digest(bytes))
}

fn bytes_to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut out, "{byte:02x}").expect("writing to String cannot fail");
    }
    out
}
