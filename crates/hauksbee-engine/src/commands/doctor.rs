//! The `hauksbee doctor [--backends] [--json]` subcommand: probe the external
//! programs a run can use.
//!
//! It checks availability (AVR, QEMU, Renode, ngspice, kicad-cli, freerouting,
//! datasheet extraction) and prints one machine-readable line per backend. It
//! calls the engine's OWN resolvers rather than re-implementing discovery, so
//! what `doctor` reports can never drift from what a real run would accept. The
//! run manifest records the same discovery ([`resolved_backends`]), so a
//! reproduced run can say which backend versions changed.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::run_manifest::BackendRevision;

/// A probed backend. `status` is a single token by contract (see [`run`]).
struct Backend {
    name: &'static str,
    status: &'static str,
    detail: String,
    summary: &'static str,
    /// The executable (or jar) that was found, when there is one.
    path: Option<PathBuf>,
    /// The backend's own version string, when it reports one.
    version: Option<String>,
}

impl Backend {
    fn new(
        name: &'static str,
        status: &'static str,
        detail: String,
        summary: &'static str,
    ) -> Self {
        Self {
            name,
            status,
            detail,
            summary,
            path: None,
            version: None,
        }
    }

    /// An installed backend at `path` whose version is asked for with `version_args`.
    fn found(
        name: &'static str,
        summary: &'static str,
        path: PathBuf,
        version_args: &[&str],
    ) -> Self {
        let version = version_of_tool(&path, version_args);
        Self {
            detail: path.display().to_string(),
            path: Some(path),
            version,
            ..Self::new(name, "ok", String::new(), summary)
        }
    }

    fn available(&self) -> bool {
        self.status == "ok" || self.status == "builtin"
    }
}

/// `hauksbee doctor --backends`: report backend availability using the
/// engine's OWN discovery, so no other availability surface can drift from
/// what a real run would accept.
///
/// For each backend this calls the exact resolver the engine uses
/// (`hauksbee_mcu::qemu::find_qemu`, `hauksbee_mcu::renode::find_renode`, the
/// ngspice, kicad-cli and freerouting finders), no re-implemented search logic.
/// `find_qemu` runs the Espressif-fork check (`is_esp_fork`), so a Homebrew
/// mainline `qemu-system-xtensa` on PATH is reported `absent` here just as the
/// co-sim rejects it, and a fork under `~/.hauksbee-qemu-esp` or a Renode under
/// `~/renode-portable` is reported present with its resolved path.
///
/// Piped stdout: one line per backend, `NAME<TAB>STATUS<TAB>DETAIL`, STATUS a
/// single lowercase token (`ok` / `warn` / `absent` / `builtin` / `disabled`); DETAIL
/// is the resolved path or a one-line fix and may contain spaces
/// (parsers should read field 3 to end-of-line). The human header goes to
/// stderr so the data stream stays clean. On a TTY there is no parser to
/// protect, so the view is one box-drawing table instead of TSV interleaved
/// with stderr framing.
///
/// # Errors
///
/// Never returns an error: a backend that fails discovery is reported as a row,
/// not propagated. The `Result` keeps the subcommand signatures uniform.
pub fn run(_backends: bool, json: bool) -> anyhow::Result<()> {
    // Nothing below says where the web app is; both framings say it once.
    const SERVE_LINE: &str =
        "The web app is `hauksbee serve`, which serves http://127.0.0.1:3001 by default.";
    if json {
        println!("{}", backends_json());
        return Ok(());
    }
    let backends = probe_backends();

    // A TTY gets ONE table a human can actually read, closed by one summary
    // line; nothing else (U10: an earlier shape interleaved two formats). A
    // pipe gets the TSV contract external tooling parses; its human framing
    // goes to stderr as one contiguous block BEFORE the data lines, never
    // alternating with them, so a `2>&1` merge stays two clean blocks.
    let ready = backends.iter().filter(|b| b.available()).count();
    let summary_line = format!(
        "{ready} of {} backends ready. All are optional: board checks work without them. \
         `hauksbee install --help` fetches the missing ones.",
        backends.len()
    );
    if std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        let rows: Vec<Vec<String>> = backends
            .iter()
            .map(|b| {
                vec![
                    b.name.to_string(),
                    b.status.to_string(),
                    b.version.clone().unwrap_or_default(),
                    b.summary.to_string(),
                    b.detail.clone(),
                ]
            })
            .collect();
        print!(
            "{}",
            super::models::box_table(
                &["Backend", "Status", "Version", "Used for", "Detail"],
                &rows
            )
        );
        println!("{summary_line}");
        println!("{SERVE_LINE}");
    } else {
        eprintln!("hauksbee backends (resolved by the engine's own discovery)");
        eprintln!("{SERVE_LINE}");
        for b in &backends {
            eprintln!("    {:<13} {}", b.name, b.summary);
        }
        for b in &backends {
            println!("{}\t{}\t{}", b.name, b.status, b.detail);
        }
    }
    Ok(())
}

/// `hauksbee doctor --json` as data.
///
/// Shape: `{"tool": {name, version, git_revision}, "backends": [{name, status,
/// available, version, detail, summary}]}`. `version` is the backend's own
/// version string or `null`. The CLI prints it and the MCP `doctor` tool returns
/// it, so the two cannot disagree about what is installed.
#[must_use]
pub fn backends_json() -> serde_json::Value {
    let tool = crate::run_manifest::ToolIdentity::workspace("hauksbee");
    let arr: Vec<serde_json::Value> = probe_backends()
        .iter()
        .map(|b| {
            serde_json::json!({
                "name": b.name,
                "status": b.status,
                "available": b.available(),
                "version": b.version,
                "detail": b.detail,
                "summary": b.summary,
            })
        })
        .collect();
    serde_json::json!({ "tool": tool, "backends": arr })
}

/// The installed backends a run can use, with version and binary hash, as the
/// run manifest records them. Same discovery as [`run`]; the extraction row is
/// left out (it names a service, not a tool whose bytes decide a result). The
/// built-in AVR backend is `simavr` at its source commit.
#[must_use]
pub fn resolved_backends() -> Vec<BackendRevision> {
    probe(false)
        .into_iter()
        .filter(Backend::available)
        .map(|b| BackendRevision {
            name: if b.name == "avr" { "simavr" } else { b.name }.to_string(),
            sha256_of_binary: b.path.as_deref().and_then(sha256_of_file),
            version: b.version,
        })
        .collect()
}

/// Probe every backend with the engine's own resolvers, in report order. The
/// probes are independent and some start a program, so they run side by side.
fn probe_backends() -> Vec<Backend> {
    probe(true)
}

fn probe(with_extract: bool) -> Vec<Backend> {
    let all: [fn() -> Backend; 8] = [
        probe_avr,
        || probe_qemu(QemuKind::Xtensa),
        || probe_qemu(QemuKind::Riscv32),
        probe_renode,
        probe_ngspice,
        probe_kicad_cli,
        probe_freerouting,
        extract_row,
    ];
    // `extract_row` is last: the manifest leaves it out.
    let probes = if with_extract { &all[..] } else { &all[..7] };
    std::thread::scope(|scope| {
        // Every probe starts before the first join, so the handles are
        // collected on purpose.
        #[allow(clippy::needless_collect)]
        let handles: Vec<_> = probes.iter().map(|probe| scope.spawn(probe)).collect();
        handles.into_iter().filter_map(|h| h.join().ok()).collect()
    })
}

const AVR_SUMMARY: &str = "ATmega / ATtiny firmware co-sim";

/// AVR: built into this binary via libsimavr (feature `avr`); there is no
/// external process to locate, so it is `builtin` when compiled in, and its
/// version is the simavr source commit.
#[cfg(feature = "avr")]
fn probe_avr() -> Backend {
    Backend {
        version: hauksbee_mcu::SIMAVR_BUILD_COMMIT.map(str::to_owned),
        ..Backend::new(
            "avr",
            "builtin",
            hauksbee_mcu::simavr_build_detail(),
            AVR_SUMMARY,
        )
    }
}

#[cfg(not(feature = "avr"))]
fn probe_avr() -> Backend {
    // Honest for BINARY users too: this build (the permissive download) can
    // never gain AVR at runtime; rebuilding from source with libsimavr is the
    // only way to get it.
    Backend::new(
        "avr",
        "disabled",
        "not in this build (the permissive, Apache-2.0 download drops \
         the GPL simavr backend). For AVR co-sim, build from source \
         with libsimavr (scripts/install-sims.sh --avr)"
            .to_string(),
        AVR_SUMMARY,
    )
}

#[derive(Clone, Copy)]
enum QemuKind {
    Xtensa,
    Riscv32,
}

/// Espressif QEMU (Xtensa ESP32 / ESP32-S3, RISC-V ESP32-C3). `find_qemu`
/// verifies the binary is the Espressif fork before accepting it.
fn probe_qemu(kind: QemuKind) -> Backend {
    let (name, summary) = match kind {
        QemuKind::Xtensa => (
            "qemu-xtensa",
            "ESP32 / ESP32-S3 firmware co-sim (Espressif QEMU fork)",
        ),
        QemuKind::Riscv32 => (
            "qemu-riscv32",
            "ESP32-C3 firmware co-sim (Espressif QEMU fork)",
        ),
    };
    #[cfg(feature = "qemu")]
    {
        use hauksbee_mcu::qemu::{find_qemu, QemuArch};
        let arch = match kind {
            QemuKind::Xtensa => QemuArch::Xtensa,
            QemuKind::Riscv32 => QemuArch::Riscv32,
        };
        match find_qemu(arch) {
            Ok(p) => Backend::found(name, summary, p, &["--version"]),
            Err(e) => Backend::new(
                name,
                "absent",
                absent_detail(&e, "ESP32 firmware co-sim", "hauksbee install esp-qemu"),
                summary,
            ),
        }
    }
    #[cfg(not(feature = "qemu"))]
    {
        Backend::new(
            name,
            "disabled",
            "built without the `qemu` feature".to_string(),
            summary,
        )
    }
}

/// Renode (STM32 / nRF52 / SiFive RISC-V, i.e. ARM Cortex-M and RISC-V).
fn probe_renode() -> Backend {
    const SUMMARY: &str = "STM32 / nRF52 / RISC-V firmware co-sim";
    #[cfg(feature = "renode")]
    {
        match hauksbee_mcu::renode::find_renode() {
            Ok(p) => Backend::found("renode", SUMMARY, p, &["--version"]),
            Err(e) => Backend::new(
                "renode",
                "absent",
                absent_detail(&e, SUMMARY, "hauksbee install renode"),
                SUMMARY,
            ),
        }
    }
    #[cfg(not(feature = "renode"))]
    {
        Backend::new(
            "renode",
            "disabled",
            "built without the `renode` feature".to_string(),
            SUMMARY,
        )
    }
}

/// ngspice, the SPICE oracle: `$NGSPICE`, then PATH, then the known install
/// locations (the same finder the dependency panel and the differential harness use).
fn probe_ngspice() -> Backend {
    const SUMMARY: &str = "SPICE oracle for cross-checking the analog solver";
    crate::deps::find_ngspice().map_or_else(
        || {
            let fix = match std::env::consts::OS {
                "macos" => "`brew install ngspice`",
                "linux" => "your package manager (`apt install ngspice`)",
                _ => "ngspice.sourceforge.io",
            };
            Backend::new(
                "ngspice",
                "absent",
                format!("not installed (optional, only for the SPICE cross-check). Fix: {fix}"),
                SUMMARY,
            )
        },
        |p| Backend::found("ngspice", SUMMARY, p, &["--version"]),
    )
}

/// kicad-cli, the DRC oracle, through the finder the DRC cross-check uses.
fn probe_kicad_cli() -> Backend {
    const SUMMARY: &str = "KiCad's own DRC, for cross-checking copper findings";
    crate::reports::drc::find_kicad_cli().map_or_else(
        || {
            Backend::new(
                "kicad-cli",
                "absent",
                "not installed (optional, only for the KiCad DRC cross-check). \
                 Fix: install KiCad from kicad.org"
                    .to_string(),
                SUMMARY,
            )
        },
        |(path, version)| Backend {
            detail: path.clone(),
            path: Some(PathBuf::from(path)),
            version: Some(version).filter(|v| !v.is_empty()),
            ..Backend::new("kicad-cli", "ok", String::new(), SUMMARY)
        },
    )
}

/// freerouting, the autorouter: a jar, found the way `--route` finds it, plus
/// a `java` to run it. The version is the jar's file stem
/// (`freerouting-1.9.0.jar` is 1.9.0); Java is not started here.
fn probe_freerouting() -> Backend {
    const SUMMARY: &str = "autorouter for `hauksbee boardcode --route`";
    let Some(jar) =
        forge_codegen::find_freerouting_jar(&forge_codegen::FreeroutingConfig::default())
    else {
        return Backend::new(
            "freerouting",
            "absent",
            "no freerouting jar found (optional, only for autorouting; the built-in grid \
             router is used instead). Fix: set FREEROUTING_JAR or pass --freerouting-jar"
                .to_string(),
            SUMMARY,
        );
    };
    let version = jar
        .file_stem()
        .and_then(|s| s.to_str())
        .and_then(parse_version);
    let java = crate::deps::which_on_path("java").is_some();
    Backend {
        detail: if java {
            jar.display().to_string()
        } else {
            format!("{} found, but `java` is not on PATH", jar.display())
        },
        path: Some(jar),
        version,
        ..Backend::new(
            "freerouting",
            if java { "ok" } else { "warn" },
            String::new(),
            SUMMARY,
        )
    }
}

/// `tool args...`'s version, asked once per binary until that file changes.
fn version_of_tool(path: &Path, args: &[&str]) -> Option<String> {
    static VERSIONS: crate::deps::StampCache<Option<String>> = crate::deps::StampCache::new();
    if let Some(cached) = VERSIONS.get(path) {
        return cached;
    }
    let output = crate::deps::output_within(Command::new(path).args(args), Duration::from_secs(5));
    let version = output.and_then(|out| {
        let text = format!(
            "{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        parse_version(&text)
    });
    VERSIONS.put(path, version.clone());
    version
}

/// The first version-looking word in `text`: `QEMU emulator version 8.2.0 (...)`
/// gives `8.2.0`, `Renode v1.15.0.1` gives `1.15.0.1`, `** ngspice-42 : ...`
/// gives `42`, `freerouting-1.9.0` gives `1.9.0`.
fn parse_version(text: &str) -> Option<String> {
    text.split_whitespace().find_map(|word| {
        let word = word.trim_matches(|c: char| !c.is_ascii_alphanumeric());
        let word = word
            .strip_prefix("ngspice-")
            .or_else(|| word.strip_prefix("freerouting-"))
            .unwrap_or(word);
        let word = word.strip_prefix('v').unwrap_or(word);
        word.starts_with(|c: char| c.is_ascii_digit())
            .then(|| word.to_string())
    })
}

/// Lowercase hex SHA-256 of a file, streamed and remembered until it changes.
fn sha256_of_file(path: &Path) -> Option<String> {
    use sha2::{Digest as _, Sha256};
    use std::io::Read as _;
    static HASHES: crate::deps::StampCache<String> = crate::deps::StampCache::new();
    if let Some(hash) = HASHES.get(path) {
        return Some(hash);
    }
    let mut file = std::fs::File::open(path).ok()?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = file.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let hash = crate::run_manifest::bytes_to_hex(&hasher.finalize());
    HASHES.put(path, hash.clone());
    Some(hash)
}

/// A resolver's "not found" is an optional tool, not a fault: say what it is
/// for and the one command that fixes it. Any other error (an override
/// variable naming a missing file) is the resolver's own words, unchanged.
#[cfg(any(feature = "qemu", feature = "renode"))]
fn absent_detail(err: &anyhow::Error, needed_for: &str, fix: &str) -> String {
    if err
        .downcast_ref::<hauksbee_mcu::BackendNotInstalled>()
        .is_some()
    {
        format!("not installed (optional, only for {needed_for}). Fix: `{fix}`")
    } else {
        crate::deps::first_line(&err.to_string())
    }
}

/// Whether one datasheet-extraction backend can run right now.
enum Readiness {
    Missing,
    SignedOut,
    /// Installed (and signed in where the CLI can tell); `true` when the
    /// sign-in could actually be checked.
    Ready(bool),
}

/// The datasheet-extraction row. Installed is not usable, so this asks each
/// agent CLI for its sign-in (its own read-only status command, cached) and,
/// when the selected backend cannot run but another can, says which one works
/// and the one command that switches to it.
fn extract_row() -> Backend {
    use crate::deps::{signin_state, SignIn};
    use hauksbee_models::datasheet::Backend as Extractor;
    use hauksbee_models::extract_config::{ProcessHost, Resolved};
    const SUMMARY: &str = "datasheet model extraction (`hauksbee models backend show`)";
    let row = |status, detail: String| {
        Backend::new("extract", status, detail.replace('\n', " "), SUMMARY)
    };
    let Ok((_, resolved)) = super::backend::load_and_resolve() else {
        return row(
            "absent",
            "extraction config unreadable; run `hauksbee models backend show`".to_string(),
        );
    };
    let readiness = |r: &Resolved, b: Extractor| {
        let st = r.status(&ProcessHost, b);
        if !st.available {
            return Readiness::Missing;
        }
        // A wrapper command owns its own sign-in; the API backend has a key.
        if b.tool().is_none() || r.custom_command(b).is_some() {
            return Readiness::Ready(false);
        }
        match signin_state(b.name(), std::path::Path::new(&st.detail), false) {
            SignIn::NotSignedIn => Readiness::SignedOut,
            SignIn::SignedIn(_) => Readiness::Ready(true),
            SignIn::NotChecked => Readiness::Ready(false),
        }
    };
    let chosen = resolved.backend;
    let label = chosen.label();
    let switch = Extractor::ALL
        .into_iter()
        .find(|&b| b != chosen && matches!(readiness(&resolved, b), Readiness::Ready(_)))
        .map(|b| {
            format!(
                " {} is ready instead: switch with `hauksbee models backend use {}`.",
                b.label(),
                b.name()
            )
        });
    match readiness(&resolved, chosen) {
        Readiness::Ready(checked) => {
            let (_, line) = super::backend::extract_status()
                .unwrap_or_else(|e| (false, format!("extraction config unreadable: {e:#}")));
            let note = if checked { "" } else { " (sign-in not checked)" };
            row("ok", format!("{line}{note}"))
        }
        Readiness::SignedOut => row(
            "warn",
            format!(
                "{label} installed but not signed in. Run `{}`.{}",
                crate::deps::login_command(chosen.name()),
                switch.unwrap_or_default()
            ),
        ),
        Readiness::Missing => switch.map_or_else(
            || {
                row(
                    "absent",
                    format!(
                        "not installed (optional, only for drafting part models from datasheets). Fix: `{}`",
                        chosen.install_hint()
                    ),
                )
            },
            |switch| {
                row(
                    "warn",
                    format!("{label} is selected but not installed (`{}`).{switch}", chosen.install_hint()),
                )
            },
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::parse_version;

    #[test]
    fn versions_are_read_from_each_tools_banner() {
        assert_eq!(
            parse_version("QEMU emulator version 8.2.0 (esp_develop_8.2.0_20240122)\nCopyright"),
            Some("8.2.0".to_string())
        );
        assert_eq!(
            parse_version("Renode v1.15.0.52"),
            Some("1.15.0.52".to_string())
        );
        assert_eq!(
            parse_version("******\n** ngspice-42 : Circuit level simulation program"),
            Some("42".to_string())
        );
        assert_eq!(
            parse_version("freerouting-1.9.0"),
            Some("1.9.0".to_string())
        );
        assert_eq!(parse_version("no numbers here"), None);
    }
}
