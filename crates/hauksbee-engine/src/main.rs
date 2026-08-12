//! `hauksbee` CLI: argument parsing (clap derive) + dispatch + process-level
//! concerns (exit codes, the `--json` error envelope). Every command's logic
//! lives in the library: the report families in [`hauksbee_engine::reports`], the
//! subcommand handlers in [`hauksbee_engine::commands`]. The `--help` text (per
//! command, usage-on-error, did-you-mean) is generated from the clap structs
//! below, so this file is intentionally thin.
//!
//! Subcommands: `run` (bind a board, then report / TUI / co-sim / serve), `serve`
//! (web front door), `to-code` / `from-code` / `check-code` (Board-as-Code), `sim`
//! (SPICE deck), `doctor` (backend availability), `models lint`. See each
//! `Command` variant (or `--help`) for the full flag surface.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// CI for hardware: hand it a PCB and it tells you what blows up before you fab.
///
/// Point `hauksbee` at any board file (KiCad, Eagle, IPC-D-356, or gerbers) and
/// it extracts the circuit, binds device models, runs the static checks, and can
/// co-simulate the firmware on an emulated MCU. Start with `run --report`.
/// The `--version` string: crate version plus the git hash this binary was
/// built from (`build.rs` sets `GIT_HASH`; absent outside a git checkout, e.g.
/// a source tarball, in which case the bare crate version is all we honestly
/// have). The hash is what lets an operator tie behaviour to an exact build.
/// Returned as `&'static str` because that is what clap's `version` wants; the
/// one-time `OnceLock` init is the cheapest way to a static composed string.
fn version_string() -> &'static str {
    static V: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    V.get_or_init(|| match option_env!("GIT_HASH") {
        Some(hash) => format!("{} (git {hash})", env!("CARGO_PKG_VERSION")),
        None => env!("CARGO_PKG_VERSION").to_string(),
    })
}

#[derive(Parser)]
#[command(
    name = "hauksbee",
    version = version_string(),
    about = "CI for hardware: hand it a PCB; it tells you what blows up before you order boards.",
    long_about = None,
    // No propagate_version: with it, `hauksbee run --version` reported itself
    // as "hauksbee-run", a binary that does not exist. `hauksbee --version` is
    // the one version surface.
    // No infer_subcommands: inference made `hauksbee check board.kicad_pcb`
    // silently resolve to check-code (a nonsense DSL parse error on a board
    // file) and `hauksbee se` start a blocking server. Clap's did-you-mean
    // suggestion on the full names covers the convenience without the traps.
    arg_required_else_help = true,
    after_help = "try it now: hauksbee run --example blinky --check --plain"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,

    /// Suppress informational `note:` lines on stderr (global; errors, warnings
    /// and report output are unaffected; `--json`/piped output already implies it).
    #[arg(long, global = true)]
    quiet: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Extract + bind a board, then check it or bring it to life.
    ///
    /// Accepts any supported board input (see the BOARD argument below), all
    /// analysed identically.
    ///
    /// With no flag on a terminal, bare `run` opens the interactive full-screen
    /// dashboard (TUI); piped/redirected (CI) it prints a hint. Pass `--serve` (or
    /// use the `serve` subcommand) for the live 2D/3D websocket frontend. The
    /// `--report`/`--drc`/`--lint`/`--si`/`--resources` flags each print one
    /// static report and exit; `--headless --firmware <hex>` runs the firmware
    /// co-sim for `--seconds`.
    ///
    /// These reports are informational and exit 0 by default, even when they
    /// list findings. Add `--strict` to FAIL (exit 2) on a real defect, or
    /// `--plain` for a verdict a non-engineer can read. For the full assertion /
    /// fault flow, gate on `hauksbee-ci` or `hauksbee check-code`.
    ///
    /// Exit contract: 0 = clean or report-only, 1 = input error (a missing or
    /// unreadable file), 2 = findings under --strict, or a usage error such as
    /// an unknown flag, 3 = invalid for analysis (the result cannot be trusted,
    /// so the run refuses to pretend).
    ///
    /// Sibling contract: `hauksbee-ci run` numbers 1 and 2 differently there
    /// (1 = a RED assertion, 2 = spec/board error); each binary's --help states
    /// its own contract.
    ///
    /// Example:
    ///   hauksbee run board.kicad_pcb --report --plain
    ///   hauksbee run board.kicad_pcb --drc --plain --strict
    ///   hauksbee run board.kicad_pcb --firmware blink.hex --headless --seconds 2   # firmware co-sim
    #[command(verbatim_doc_comment)]
    Run(RunArgs),

    /// Verify an immutable run manifest and execute its recorded command.
    ///
    /// Reproduction fails before launching if the manifest was edited, any
    /// input bytes changed, a behavior-changing environment selector differs,
    /// or this binary is not the recorded tool revision. The manifest may only
    /// invoke `hauksbee` or its sibling `hauksbee-ci`; it cannot name an
    /// arbitrary executable.
    ///
    /// Example:
    ///   hauksbee reproduce run.manifest.json
    Reproduce(ReproduceArgs),

    /// Decompile a board into editable Board-as-Code text.
    ///
    /// Example:
    ///   hauksbee to-code my_board.kicad_pcb --out my_board.board
    #[command(verbatim_doc_comment)]
    ToCode(ToCodeArgs),

    /// Recompile Board-as-Code back into a `.kicad_pcb`.
    ///
    /// Optionally re-place the parts (`--relayout`/`--incremental`) and route
    /// them (`--route` via freerouting, or `--route-grid` for the in-tree A*).
    ///
    /// Example:
    ///   hauksbee from-code my_board.board --out my_board.kicad_pcb --route
    #[command(verbatim_doc_comment)]
    FromCode(FromCodeArgs),

    /// Merge an externally routed Specctra SES file back onto a board.
    ///
    /// The return half of `from-code --route-dsn`: export the DSN, route it
    /// with any Specctra-capable router on your own clock (the freerouting
    /// GUI, a long headless run, a different autorouter entirely), then merge
    /// the SES it produced. The board is recompiled from the same `.board`
    /// source (placement is deterministic, so pads land where the DSN said),
    /// the SES copper is merged on, and the SAME post-merge audit the built-in
    /// `--route` path runs judges the result: real routed connections, the
    /// internal DRC, and the endpoint-net assertion. `--route-strict` gates on
    /// that audit exactly as `from-code --route-strict` does. This also makes
    /// a route a cacheable, diffable artifact: keep the SES, re-merge at will.
    ///
    /// Example:
    ///   hauksbee from-code my.board --out my.kicad_pcb --route-dsn my.dsn
    ///   java -jar freerouting.jar -de my.dsn -do my.ses   # any router, any time
    ///   hauksbee merge-ses my.board my.ses --out my.kicad_pcb --route-strict
    #[command(verbatim_doc_comment)]
    MergeSes(MergeSesArgs),

    /// Recompile, bind, co-sim with the stress monitor, print a fault report.
    ///
    /// This is the edit -> simulate loop: exits non-zero if a fault is raised,
    /// so it drops straight into a script or pre-commit hook.
    ///
    /// Example:
    ///   hauksbee check-code my_board.board --seconds 0.2
    #[command(verbatim_doc_comment)]
    CheckCode(CheckCodeArgs),

    /// Simulate a SPICE deck (`.cir`) and write the results as CSV.
    ///
    /// Loads the netlist through the same loader the rest of the tool uses,
    /// runs the analysis the deck asks for (or the one a flag forces), and
    /// writes one column per probe. With no analysis flag: a `.tran` card runs
    /// a transient, otherwise the DC operating point (`.op`).
    ///
    /// All four analyses run: operating point (`--op`), transient (`--tran`),
    /// AC small-signal sweep (`--ac`), and DC sweep (`--dc`), force one with
    /// its flag, or let the deck's own directives choose. Output is CSV by
    /// default; `--format raw` writes an ngspice ASCII rawfile (the format
    /// `ngnutmeg`/`gaw`/`spicelib` read) and `--format both` writes CSV and
    /// rawfile side by side.
    ///
    /// Honesty: the tool refuses loudly rather than fake a number. `--format
    /// both` needs `--out` to name its two files and refuses (exit 2) without
    /// it. A well-formed deck we cannot honestly answer, e.g. `.ac` with no AC
    /// source, exits 3 explaining why. A malformed deck prints the loader's
    /// line-numbered error and exits 2. Only the ngspice *ASCII* rawfile is
    /// emitted (the binary rawfile variant is not written). The exact
    /// supported/refused card list is the drift-tested compatibility
    /// statement (URL below).
    ///
    /// Example (self-contained decks bundled under examples/learn/):
    ///   hauksbee sim examples/decks/divider.cir --op --print V(out)
    ///   hauksbee sim examples/decks/rlc_ringdown.cir --tran --print V(out)
    ///   hauksbee sim examples/decks/rc_lowpass_ac.cir --ac --print V(out)
    #[command(verbatim_doc_comment, after_help = sim_after_help())]
    Sim(SimArgs),

    /// Start the local web front door: a "drop your board, get a report" page.
    ///
    /// Opens a local web server with no board pre-loaded. Point a browser at the
    /// printed URL, drop a .kicad_pcb / .kicad_sch / .brd / gerber zip on the
    /// page, and get the plain-language verdict, the full report, and a 2D map
    /// of the parts, no terminal needed beyond this command. Nothing is
    /// uploaded off the machine; the analysis runs in this process.
    ///
    /// Example:
    ///   hauksbee serve --port 3001
    #[command(verbatim_doc_comment)]
    Serve(ServeArgs),

    /// Report which co-simulation backends this build can actually locate.
    ///
    /// Runs the ENGINE's own backend discovery; the same `find_qemu` /
    /// `find_renode` a co-sim would use, and prints, for each backend, the
    /// resolved binary path or that it is absent. This is the authoritative
    /// probe: any other surface that reports backend availability calls this
    /// same discovery, so nothing can disagree with the engine: a Homebrew
    /// mainline `qemu-system-xtensa` that has no
    /// `esp32` machine is reported absent here exactly as the co-sim rejects it,
    /// and a `~/renode-portable` install the co-sim finds is reported present.
    ///
    /// On a TTY the report is one table. When stdout is piped it is one
    /// machine-parseable line per backend
    /// (`NAME<TAB>STATUS<TAB>PATH-OR-HINT`, STATUS a single lowercase token);
    /// the human header goes to stderr. `--json` emits a JSON object instead.
    ///
    /// Example:
    ///   hauksbee doctor --backends
    #[command(verbatim_doc_comment)]
    Doctor(DoctorArgs),

    /// Model-library tooling: validate model TOML, manage installed model
    /// packs, and debug which entry wins resolution.
    ///
    /// `models lint <file>` checks a `[[models]]` db file (params per kind,
    /// plus each entry's `[models.logic]` block: schema validation, expression
    /// compilation, and the exhaustive combinational-cycle convergence check),
    /// a `[sensor]` register-map spec, or a `[soc]` MCU descriptor (the loader's
    /// own validation plus the checks that catch a descriptor which would run
    /// and observe the wrong register, plus an inspection of what it configures).
    /// Every failure is a NAMED error tied to its entry. It is the same
    /// validation binding runs, available standalone so a spec author (or an LLM
    /// extraction pipeline) fails fast.
    ///
    /// `models add <path|url>` installs a model pack (a directory with a
    /// pack.toml manifest and models/*.toml) into ~/.hauksbee/packs;
    /// `models remove <name>` uninstalls it; `models list` shows what is
    /// installed. `models resolve <board>` prints, per component, which model
    /// entry won and from which of the six priority layers (builtin=0 <
    /// pack=10 < user-dir=20 < user-config-dir=25 < --models-dir=30 <
    /// spice=40).
    ///
    /// Examples:
    ///   hauksbee models lint my_part.toml
    ///   hauksbee models add ./acme-sensors
    ///   hauksbee models resolve my_board.kicad_pcb
    #[command(verbatim_doc_comment)]
    Models(ModelsArgs),

    /// Watch a target and re-run the right check on every file change: a board
    /// runs `run --check`, a `.board` runs `check-code`, a `.toml` runs the spec
    /// through `hauksbee-ci`. Ctrl-C exits with the last run's code.
    ///
    /// Example:
    ///   hauksbee watch my_board.kicad_pcb --plain
    #[command(verbatim_doc_comment)]
    Watch(WatchArgs),

    /// Install an external co-sim dependency.
    ///
    /// `install esp-qemu` downloads Espressif's official prebuilt QEMU fork
    /// (qemu-system-xtensa for ESP32/ESP32-S3, qemu-system-riscv32 for
    /// ESP32-C3) from github.com/espressif/qemu releases into
    /// `~/.hauksbee-qemu-esp/`, verifies the sha256 against the release's
    /// checksum manifest, and accepts each binary only after the same
    /// esp32-machine check the co-sim itself applies. Nothing is bundled:
    /// the fork is a separate GPL program hauksbee talks to over sockets.
    ///
    /// Example:
    ///   hauksbee install esp-qemu --yes
    #[command(verbatim_doc_comment)]
    Install(InstallArgs),
}

#[derive(Parser)]
struct InstallArgs {
    #[command(subcommand)]
    command: InstallCommand,
}

#[derive(Parser)]
struct ReproduceArgs {
    /// Manifest emitted by `hauksbee run --emit-manifest` or
    /// `hauksbee-ci run --emit-manifest`.
    #[arg(value_name = "MANIFEST.JSON")]
    manifest: PathBuf,
}

#[derive(Subcommand)]
enum InstallCommand {
    /// Fetch the Espressif QEMU fork (ESP32 / ESP32-S3 / ESP32-C3 co-sim).
    EspQemu {
        /// Skip the confirmation prompt (for CI / scripts).
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Fetch the Renode portable build (STM32 / nRF52 / RISC-V co-sim).
    Renode {
        /// Skip the confirmation prompt (for CI / scripts).
        #[arg(long, short = 'y')]
        yes: bool,
    },
}

#[derive(Parser)]
struct WatchArgs {
    /// Board, `.board`, or hauksbee-ci spec (`.toml`) to watch.
    #[arg(value_name = "TARGET")]
    target: PathBuf,
    /// Stream plain-language reports (default: the expert report).
    #[arg(long, visible_alias = "explain")]
    plain: bool,
    /// Run the check once and exit (test the plumbing without watching).
    #[arg(long)]
    once: bool,
}

#[derive(Parser)]
struct ModelsArgs {
    #[command(subcommand)]
    command: ModelsCommand,
}

#[derive(Subcommand)]
enum ModelsCommand {
    /// Validate a model / sensor spec TOML file; exit 2 on any finding.
    Lint(ModelsLintArgs),
    /// Install a model pack from a directory, tarball, or git URL.
    Add(ModelsAddArgs),
    /// Uninstall a model pack by name.
    Remove(ModelsRemoveArgs),
    /// List installed model packs (and, with --builtin, the embedded MCU SoC descriptors).
    List(ModelsListArgs),
    /// Show, per board component, which model entry won and from which layer.
    Resolve(ModelsResolveArgs),
    /// Draft a device model from a PDF datasheet. Asks first: this sends the
    /// datasheet's text to an LLM backend.
    Extract(ModelsExtractArgs),
    /// Scaffold a model entry for one board component, pre-seeded from the
    /// board's own context (value, guessed kind), plus the lint/resolve
    /// commands to finish the job. Nothing is sent anywhere.
    New(ModelsNewArgs),
}

#[derive(Parser)]
struct ModelsNewArgs {
    /// The component's reference designator on the board (e.g. U3, R7).
    #[arg(value_name = "REF")]
    reference: String,
    /// The board the component sits on (any format `run` accepts).
    #[arg(long, value_name = "BOARD")]
    board: PathBuf,
    /// Where to write the scaffold. Default: ./<id>.toml in the current
    /// directory. Refuses to overwrite.
    #[arg(long, value_name = "FILE")]
    out: Option<PathBuf>,
}

#[derive(Parser)]
struct ModelsExtractArgs {
    /// The datasheet PDF to read.
    #[arg(long)]
    pdf: PathBuf,
    /// The part number the model is for (e.g. BC847B).
    #[arg(long)]
    part: String,
    /// What kind of device it is (bjt_npn, vreg, opamp, i2c_sensor, ...).
    /// Omit it and the model works it out from the datasheet, which is usually
    /// what you want: it is about to read the page that says so.
    #[arg(long, default_value = "")]
    kind: String,
    /// Where to write the model card. Defaults to the user model directory
    /// (~/.hauksbee/models), which every run already reads.
    #[arg(long)]
    out_dir: Option<PathBuf>,
    /// Skip the prompt. For scripts that have already got the user's consent.
    #[arg(long, short = 'y')]
    yes: bool,
    /// Which LLM backend drafts the model. Default: codex (or api when
    /// HAUKSBEE_LLM_API_KEY is set, matching the pre-flag behaviour).
    #[arg(long, value_enum)]
    backend: Option<ExtractBackendArg>,
    /// Model the extraction runs on (a codex/claude model name, or the api
    /// backend's model id). Default: gpt-5.6-sol for codex, the CLI's own
    /// default for claude-code.
    #[arg(long)]
    model: Option<String>,
    /// Base URL for --backend api (an OpenAI-compatible endpoint).
    /// Default: HAUKSBEE_LLM_BASE_URL, then https://api.openai.com/v1.
    #[arg(long, value_name = "URL")]
    api_base: Option<String>,
    /// NAME of the environment variable holding the api key (default:
    /// OPENAI_API_KEY). The name, never the key itself: the key is read from
    /// the environment at call time and is never stored.
    #[arg(long, value_name = "NAME")]
    api_key_env: Option<String>,
}

/// CLI spelling of the extraction backend. Separate from
/// `hauksbee_models::datasheet::Backend` because the models crate carries no
/// clap dependency; the `From` impl below is the whole binding.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum ExtractBackendArg {
    /// `codex exec` (needs `codex` in PATH).
    Codex,
    /// Headless `claude -p` (needs `claude` in PATH).
    ClaudeCode,
    /// An OpenAI-compatible chat-completions endpoint.
    Api,
}

impl From<ExtractBackendArg> for hauksbee_models::datasheet::Backend {
    fn from(b: ExtractBackendArg) -> Self {
        match b {
            ExtractBackendArg::Codex => Self::Codex,
            ExtractBackendArg::ClaudeCode => Self::ClaudeCode,
            ExtractBackendArg::Api => Self::Api,
        }
    }
}

#[derive(Parser)]
struct ModelsListArgs {
    /// Also list the embedded MCU SoC descriptors (the `backend:part` specs a
    /// board's `renode:<part>` / `qemu:<part>` backend string resolves to when
    /// no override-dir descriptor shadows them).
    #[arg(long)]
    builtin: bool,
}

#[derive(Parser)]
struct ModelsLintArgs {
    /// A `[[models]]` db TOML file or a `[sensor]` spec TOML file.
    file: PathBuf,
}

#[derive(Parser)]
struct ModelsAddArgs {
    /// A pack directory, a .tar.gz/.tgz/.tar archive, or a git URL.
    source: String,
}

#[derive(Parser)]
struct ModelsRemoveArgs {
    /// The pack name (as shown by `models list`).
    name: String,
}

#[derive(Parser)]
struct ModelsResolveArgs {
    /// Board file to resolve (.kicad_pcb, .kicad_sch, .brd, .d356, gerbers…).
    board: PathBuf,
    /// Extra model directory, loaded at the --models-dir layer (priority 30).
    #[arg(long, value_name = "DIR")]
    models_dir: Option<PathBuf>,
    /// Refuse (exit 3) if any component resolves below this semantic source
    /// tier. Storage layer is not accuracy: use names such as
    /// `datasheet-derived`, `curated-library`, or `vendor-spice`.
    #[arg(long, value_name = "TIER")]
    min_model_tier: Option<hauksbee_ir::evidence::ModelSourceTier>,
    /// Refuse (exit 3) if any selected model has passed less validation than
    /// requested (`physical-bounds-only`, `datasheet-curves`, or
    /// `vendor-qualified`).
    #[arg(long, value_name = "LEVEL")]
    min_model_validation: Option<hauksbee_ir::evidence::ModelValidation>,
    /// Refuse (exit 3) unless every selected model publishes validated finite
    /// uncertainty intervals. Unknown accuracy is never converted to a guess.
    #[arg(long)]
    require_model_intervals: bool,
    /// Emit the resolution as one JSON object ({"components":[{"ref","value",
    /// "model","layer","origin"}]}) instead of the text table. A component the
    /// binder could not resolve carries model "UNRESOLVED".
    #[arg(long)]
    json: bool,
}

/// CLI spelling of the host-serial transport. Separate from
/// `hauksbee_mcu::hostserial::HostSerialTransport` because the mcu crate carries
/// no clap dependency; the conversion below is the whole binding.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum SerialTransportArg {
    /// A pseudo-terminal with a real device path: unmodified serial software
    /// works unmodified.
    Pty,
    /// A loopback TCP port, for a tool that speaks sockets.
    Tcp,
}

impl From<SerialTransportArg> for hauksbee_mcu::hostserial::HostSerialTransport {
    fn from(a: SerialTransportArg) -> Self {
        match a {
            SerialTransportArg::Pty => Self::Pty,
            SerialTransportArg::Tcp => Self::Tcp,
        }
    }
}

/// `run`'s after-help paragraph; a function because the docs pointer renders
/// through [`hauksbee_ir::docs_url`] at runtime.
fn run_after_help() -> String {
    format!(
        "TRANSIENT / BROWNOUT analysis (a rail sagging under a load step, WiFi burst, or \
         inrush) is not a `run` flag: it is a dynamic scenario judged by an assertion. \
         Scaffold one with `hauksbee-ci init <board>` (it emits a [[scenario]] + \
         rail_window stub when a supply is detected) and see {}. For \
         scriptable waveforms from a headless run, use --probe/--probe-csv.",
        hauksbee_ir::docs_url("docs/checks/TRANSIENTS.md")
    )
}

/// `sim`'s after-help line; a function because the compatibility-statement
/// pointer renders through [`hauksbee_ir::docs_url`] at runtime.
fn sim_after_help() -> String {
    format!(
        "The drift-tested SPICE compatibility statement (every supported and refused card): {}",
        hauksbee_ir::docs_url("docs/spice-compat/compatibility.md")
    )
}

/// `--json`'s long help; a function because the schema docs pointer renders
/// through [`hauksbee_ir::docs_url`] at runtime.
fn json_flag_long_help() -> String {
    format!(
        "Emit machine-readable JSON instead of the box-drawing tables, for any of \
         --report/--drc/--lint/--si/--resources/--usb-c/--thermal/--ac. Implies \
         non-interactive, stable output; every field is documented at {}. \
         `valid:false` + `reason` is set on AC/thermal results that are \
         meaningless; the bind section reports critical_parts_bound + \
         active_path_unresolved by role.",
        hauksbee_ir::docs_url("docs/analysis/JSON_OUTPUT.md")
    )
}

/// `--oracle`'s long help; runtime docs URL, same reason as above.
fn oracle_flag_long_help() -> String {
    format!(
        "Cross-check the geometric DRC against KiCad's own `kicad-cli pcb drc` (the \
         oracle) and print whether they agree, so a copper finding is self-confirming \
         without running a second tool by hand. Uses a `kicad-cli` found on PATH or \
         in a standard install location (newest version preferred); KiCad is NOT \
         bundled (see {}). No-op unless paired with `--drc`.",
        hauksbee_ir::docs_url("docs/cosim/ORACLES.md")
    )
}

/// `--ac-loop`'s long help; runtime docs URL, same reason as above.
fn ac_loop_flag_long_help() -> String {
    format!(
        "Measure loop stability at this break/output net: report gain crossover \
         and phase margin. Use with `--ac`. The net is the far side of a loop \
         broken by an injection `Vsource` (see {}).",
        hauksbee_ir::docs_url("docs/analysis/AC_ANALYSIS.md")
    )
}

/// Echo a user-typed numeric argument back in an error message without letting
/// it explode the line: `1e308` parses to a float whose `Display` form is 309
/// digits. The RAW input string is what the user typed, capped defensively.
fn echo_arg(s: &str) -> String {
    const MAX: usize = 32;
    if s.chars().count() <= MAX {
        s.to_string()
    } else {
        let head: String = s.chars().take(MAX).collect();
        format!("{head}...")
    }
}

/// `--ambient` bound check: a physical temperature. Out of range is a clap
/// usage error (exit 2) naming the bound, instead of a garbage simulation.
fn parse_ambient(s: &str) -> Result<f64, String> {
    let v: f64 = s
        .parse()
        .map_err(|_| format!("'{}' is not a number", echo_arg(s)))?;
    if !v.is_finite() || !(-273.15..=1000.0).contains(&v) {
        return Err(format!(
            "ambient temperature must be within [-273.15, 1000] C, got {}",
            echo_arg(s)
        ));
    }
    Ok(v)
}

/// `--seconds` bound check: simulated time must be positive and sane. Zero
/// seconds simulates nothing (every assertion would vacuously hold) and
/// beyond 1e6 s (~11.6 days of simulated time) is a typo, not a run.
fn parse_seconds(s: &str) -> Result<f64, String> {
    let v: f64 = s
        .parse()
        .map_err(|_| format!("'{}' is not a number", echo_arg(s)))?;
    if !v.is_finite() || v <= 0.0 || v > 1e6 {
        return Err(format!(
            "seconds must be within (0, 1e6], got {}",
            echo_arg(s)
        ));
    }
    Ok(v)
}

#[derive(Parser)]
// These flags each select one analysis surface. Mixing a static report, the
// aggregate check, AC, thermal or headless co-sim would otherwise let execution,
// artifacts and annotations silently choose different precedence.
#[command(group(
    clap::ArgGroup::new("report_mode")
        .args([
            "report", "drc", "ampacity", "lint", "si", "resources", "thermal", "usb_c",
            "check", "headless", "ac"
        ])
        .multiple(false)
))]
#[command(after_help = run_after_help())]
struct RunArgs {
    /// Board input: KiCad (.kicad_pcb / .kicad_sch / .net), Eagle (.brd),
    /// Altium (.PcbDoc), IPC-D-356 (.d356), a gerber folder or zip, or
    /// Board-as-Code (.board). The one format list; every other surface
    /// accepts the same set.
    #[arg(value_name = "BOARD", required_unless_present = "example")]
    board: Option<PathBuf>,

    /// Bill of materials to reconcile with the board before binding. Common
    /// KiCad, Altium, LCSC/JLCPCB and hand-maintained CSV/TSV exports are
    /// detected from their headers.
    #[arg(long, value_name = "FILE", help_heading = "Identity inputs")]
    bom: Option<PathBuf>,

    /// Confirm an ambiguous BOM column as ROLE=HEADER. Repeat for multiple
    /// roles; accepted roles are reference, value, mpn, manufacturer,
    /// quantity, footprint, populate, and distributor_part.
    #[arg(
        long = "bom-column",
        value_name = "ROLE=HEADER",
        requires = "bom",
        help_heading = "Identity inputs"
    )]
    bom_columns: Vec<String>,

    /// Pick-and-place/CPL file to reconcile with layout positions, rotations
    /// and sides before its values or packages are used.
    #[arg(long, value_name = "FILE", help_heading = "Identity inputs")]
    placement: Option<PathBuf>,

    /// Eagle `.sch` companion to an Eagle `.brd`, read for declared net-pair
    /// context. A schematic has no board coordinates, so it never authorizes a
    /// physical join or downgrades a short; board-local layout evidence must do
    /// that. A same-named valid sibling is used automatically. KiCad and Altium
    /// layouts declare physical ties in the layout file itself.
    #[arg(long, value_name = "FILE", help_heading = "Identity inputs")]
    schematic: Option<PathBuf>,

    /// Run an embedded example board instead of a file (try `blinky`). The
    /// board is compiled into the binary and materialized under the temp
    /// directory, so this works with no checkout on disk.
    #[arg(
        long,
        value_name = "NAME",
        conflicts_with = "board",
        help_heading = "Start here"
    )]
    example: Option<String>,

    /// Firmware to co-simulate on the board's MCU: a compiled .elf/.hex, a
    /// PlatformIO project directory (built with your own `pio run`), or a zip
    /// of either (the built image inside is found automatically).
    #[arg(long, value_name = "FIRMWARE", help_heading = "Co-simulation")]
    firmware: Option<PathBuf>,

    /// As-built overlay (.asbuilt.toml): the declarative physical delta between
    /// the design files and the real reworked board (cut traces, jumper wires,
    /// lifted pins, fitted component values), applied before simulating.
    #[arg(long, value_name = "FILE", help_heading = "Co-simulation")]
    asbuilt: Option<PathBuf>,

    /// Write this invocation's selected checks as JUnit XML. Gate-grade findings
    /// become failures; whole-run refusals become errors.
    #[arg(long, value_name = "FILE", help_heading = "CI output")]
    junit: Option<PathBuf>,

    /// Write this invocation's selected checks as SARIF 2.1.0. Gate-grade
    /// findings and invalid/refused outcomes become `error` results.
    #[arg(long, value_name = "FILE", help_heading = "CI output")]
    sarif: Option<PathBuf>,

    /// Write a canonical, immutable JSON reproduction manifest. It hashes all
    /// run inputs and model sources, records exact options/tool/solver versions
    /// and safe environment selectors, and refuses to overwrite an existing
    /// file. Replay it with `hauksbee reproduce <FILE>`.
    #[arg(long, value_name = "FILE", help_heading = "CI output")]
    emit_manifest: Option<PathBuf>,

    /// Seconds of simulated time to run under --headless.
    #[arg(
        long,
        default_value_t = 1.0,
        value_name = "N",
        value_parser = parse_seconds,
        allow_negative_numbers = true,
        help_heading = "Co-simulation"
    )]
    seconds: f64,

    /// Run the co-sim headless for --seconds and print summary stats (no server).
    #[arg(long, help_heading = "Co-simulation")]
    headless: bool,

    /// Print the bind report table (every component -> device model) and exit.
    #[arg(long, group = "report_mode", help_heading = "Reports")]
    report: bool,

    /// Print the geometric copper short / clearance (DRC) report and exit.
    #[arg(long, group = "report_mode", help_heading = "Reports")]
    drc: bool,

    /// Print IPC-2221 trace-current capacity for power-like routed nets and exit.
    /// This is capacity-only unless a future spec supplies per-net current.
    #[arg(long, group = "report_mode", help_heading = "Reports")]
    ampacity: bool,

    /// Print the connectivity lint + strap-pin + resource-conflict report and exit.
    #[arg(long, group = "report_mode", help_heading = "Reports")]
    lint: bool,

    /// Print the signal-integrity / physics static-check report and exit.
    #[arg(long, group = "report_mode", help_heading = "Reports")]
    si: bool,

    /// Print only the MCU internal resource-conflict report and exit.
    #[arg(long, group = "report_mode", help_heading = "Reports")]
    resources: bool,

    /// Print the USB-C CC compliance report (the attach a compliant source sees
    /// from the receptacle's CC termination, and whether it applies VBUS) and
    /// exit. Flags the Raspberry-Pi-4-class shared-CC-pulldown fault.
    #[arg(long = "usb-c", group = "report_mode", help_heading = "Reports")]
    usb_c: bool,

    /// Run a short headless co-sim and print the steady-state junction-temperature
    /// estimate per dissipating device (Tj = Tambient + P * theta_JA), then exit.
    #[arg(long, group = "report_mode", help_heading = "Reports")]
    thermal: bool,

    /// Ambient temperature (C) for the --thermal estimate. Default 25 C.
    #[arg(
        long,
        default_value_t = 25.0,
        value_name = "C",
        value_parser = parse_ambient,
        allow_negative_numbers = true,
        help_heading = "Advanced / analyses"
    )]
    ambient: f64,

    /// Translate the report into plain language for a non-engineer: a one-line
    /// verdict, then each finding as what it is, why it matters, and what to do.
    /// Applies to --drc/--lint/--si/--resources/--usb-c and to --headless faults.
    #[arg(long, visible_alias = "explain", help_heading = "Start here")]
    plain: bool,

    /// With --plain --drc (or --check): print every clearance finding in full.
    /// By default, repeated near-identical clearance warnings condense to one
    /// aggregated line per rule and layer after the first few.
    #[arg(long, help_heading = "Reports")]
    verbose: bool,

    /// Emit machine-readable JSON instead of the box-drawing tables, for any of
    /// --report/--drc/--lint/--si/--resources/--usb-c/--thermal/--ac. Implies
    /// non-interactive, stable output. `valid:false` +
    /// `reason` is set on AC/thermal results that are meaningless; the bind
    /// section reports critical_parts_bound + active_path_unresolved by role.
    #[arg(long, help_heading = "Start here", long_help = json_flag_long_help())]
    json: bool,

    /// Exit non-zero if a report (--check/--drc/--lint/--si/--resources/--usb-c) finds problems,
    /// so it can gate a CI pipeline directly. Default stays exit 0 (scripts that
    /// only read the text are unaffected). Exit 2 counts shorts, serious/medium
    /// lint findings, every real SI finding and a serious USB-C CC verdict; DRC
    /// clearance notes and the lint low-severity notes do not fail the gate.
    /// Exit 3 is the other half: a board whose findings do not gate (or has
    /// none at all) still fails the gate when the run could not be judged (an
    /// unbound current-carrying / active part on a model-dependent surface,
    /// undermined run-level evidence), which is what that surface's
    /// `verdict: "invalid"` says in --json.
    #[arg(long, visible_alias = "fail-on-findings")]
    strict: bool,

    /// Accepted for compatibility; this is now the DEFAULT. A PARTIAL-coverage
    /// thermal result (rows exist while an active power IC on the live circuit
    /// is open/unresolved) escalates to exit 3 (invalid for analysis) whether
    /// or not this flag is passed, so existing CI invocations that passed it
    /// keep their behaviour. Use --no-strict-thermal to opt out.
    #[arg(
        long,
        help_heading = "Advanced / analyses",
        conflicts_with = "no_strict_thermal"
    )]
    strict_thermal: bool,

    /// Opt out of the default strict thermal gate, restoring the old
    /// non-strict behaviour: a PARTIAL-coverage thermal result exits 0 instead
    /// of 3, and undermined thermal evidence no longer escalates either (the
    /// two exits --strict-thermal used to opt in to). The INCONCLUSIVE
    /// coverage caveat (text stderr + JSON note) still prints; only the exit
    /// code changes. An empty thermal table over open power ICs stays invalid
    /// (exit 3) regardless.
    #[arg(long, help_heading = "Advanced / analyses")]
    no_strict_thermal: bool,

    /// Opt-in: escalate the co-sim boot-safety advisories to exit 2. By default,
    /// heads-up notes about MCU control nets driven HIGH at boot, or left
    /// floating the whole run, with no bias resistor are advisory only and do
    /// not affect the exit code. Pass --strict-boot to fail CI on any such note.
    #[arg(long, help_heading = "Advanced / analyses")]
    strict_boot: bool,

    /// List the board's net names (sorted) and exit. Use it to find the exact net
    /// to pass to `--ac-node` / `--ac-loop` without grepping the layout file.
    #[arg(long, help_heading = "Reports")]
    list_nets: bool,

    /// Run ALL the static checks at once in one report, instead of one flag at a
    /// time: bind coverage, DRC (shorts + clearance), the connectivity lint,
    /// signal integrity, USB-C CC compliance, and the MCU resource-conflict,
    /// boot strap-pin, config-pin decode and output-contention checks that ride
    /// with the lint. Honours --plain / --json / --strict.
    #[arg(long, visible_alias = "all", help_heading = "Start here")]
    check: bool,

    /// Cross-check the geometric DRC against KiCad's own `kicad-cli pcb drc` (the
    /// oracle) and print whether they agree, so a copper finding is self-confirming
    /// without running a second tool by hand. Uses a `kicad-cli` found on PATH or
    /// in a standard install location (newest version preferred); KiCad is NOT
    /// bundled. No-op unless paired with `--drc`.
    #[arg(
        long,
        help_heading = "Advanced / analyses",
        long_help = oracle_flag_long_help()
    )]
    oracle: bool,

    /// Bridge every detected copper short before simulating (show the consequences).
    #[arg(long, help_heading = "Advanced / analyses")]
    apply_shorts: bool,

    /// Serve the live 2D/3D websocket frontend (the historical bare-`run`
    /// behaviour). With no report/serve flag on a TTY, `run` now launches the
    /// interactive terminal UI instead; pass `--serve` to keep the web frontend,
    /// or use the `hauksbee serve` subcommand.
    #[arg(long, help_heading = "Advanced / analyses")]
    serve: bool,

    /// With --serve: open the browser on the served URL once the port is
    /// bound (the same policy as the `serve` subcommand). A piped/CI launch
    /// never auto-opens.
    #[arg(long, help_heading = "Advanced / analyses")]
    open: bool,

    /// With --serve: never open a browser, even when launched by the desktop
    /// app (mirrors `serve --no-open`).
    #[arg(long, help_heading = "Advanced / analyses", conflicts_with = "open")]
    no_open: bool,

    /// Force the interactive terminal UI even when stdout is not a TTY (mainly
    /// for testing under a PTY). Normally the TUI is the auto-default for bare
    /// `run` on a TTY; this never triggers when a report flag is given.
    #[arg(long, help_heading = "Advanced / analyses")]
    tui: bool,

    /// Port for the live frontend websocket server (`--serve`).
    #[arg(
        long,
        default_value_t = 3001,
        value_name = "PORT",
        help_heading = "Advanced / analyses"
    )]
    port: u16,

    /// Extra model directory (highest priority), layered over the built-in DB.
    #[arg(long, value_name = "DIR", help_heading = "Advanced / analyses")]
    models_dir: Option<PathBuf>,

    /// Small-signal AC sweep: `<fstart>:<fstop>:<points>[:dec|:oct|:lin]` (Hz;
    /// points per decade by default, per octave with `:oct`, total with `:lin`). Linearises about the DC operating point and prints
    /// a Bode (magnitude dB + phase) table for `--ac-node`, then exits. Drive is
    /// a unit AC source on every independent source in the circuit.
    ///
    /// Example: hauksbee run board.kicad_pcb --ac 10:1e6:20 --ac-node OUT
    #[arg(
        long,
        value_name = "FSTART:FSTOP:POINTS",
        help_heading = "Advanced / analyses"
    )]
    ac: Option<String>,

    /// Output net(s) to report for `--ac` (repeatable). Defaults to every net.
    #[arg(
        long = "ac-node",
        value_name = "NET",
        help_heading = "Advanced / analyses"
    )]
    ac_node: Vec<String>,

    /// Write the full AC sweep (all reported nets) to this CSV file.
    #[arg(long, value_name = "FILE", help_heading = "Advanced / analyses")]
    ac_csv: Option<PathBuf>,

    /// Measure loop stability at this break/output net: report gain crossover
    /// and phase margin. Use with `--ac`. The net is the far side of a loop
    /// broken by an injection `Vsource`.
    #[arg(
        long = "ac-loop",
        value_name = "NET",
        help_heading = "Advanced / analyses",
        long_help = ac_loop_flag_long_help()
    )]
    ac_loop: Option<String>,

    /// Record these nets' node voltages each chunk of a `--headless` run and write
    /// them to `--probe-csv`, so waveforms are scriptable with no UI. Comma-
    /// separated and/or repeatable: `--probe +5V,GATE --probe D13`. An unknown net
    /// is a loud error (with near-matches) before the run starts.
    #[arg(
        long,
        value_name = "NET[,NET...]",
        value_delimiter = ',',
        help_heading = "Advanced / analyses"
    )]
    probe: Vec<String>,

    /// CSV path for `--probe`: header is `time_s` then one column per probed net,
    /// one row per co-sim chunk.
    #[arg(long, value_name = "FILE", help_heading = "Advanced / analyses")]
    probe_csv: Option<PathBuf>,

    /// Solver chunk width for a `--headless` co-sim, in microseconds. The analog
    /// solve advances one chunk at a time and tick-evaluated parts are sampled
    /// once per chunk, so a firmware pulse narrower than a chunk can rise and
    /// fall unseen. Halve the chunk below the narrowest pulse you care about
    /// (`--chunk-us 1` for a 2 us strobe); runtime scales inversely.
    #[arg(long, value_name = "US", help_heading = "Advanced / analyses")]
    chunk_us: Option<f64>,

    /// Open a host serial port onto the emulated MCU's UART and run a live
    /// co-sim, so your own software talks to the simulated board the way it
    /// talks to real hardware. Prints a device path (e.g. /dev/ttys006) to
    /// paste into another terminal, and reports when your tool attaches and
    /// detaches. Needs --firmware. Sim time is paced to wall-clock time so a
    /// script's timeouts and sleeps mean what they say.
    #[arg(long, help_heading = "Host serial")]
    serial_attach: bool,

    /// How the host attaches under --serial-attach. `pty` gives a real device
    /// path, so unmodified serial software (pyserial, minicom, a vendor tool)
    /// works unmodified; `tcp` gives a loopback port for a tool that speaks
    /// sockets, or for a platform with no pty.
    #[arg(
        long,
        value_name = "KIND",
        default_value = "pty",
        help_heading = "Host serial"
    )]
    serial_transport: SerialTransportArg,

    /// Hold the co-sim at t=0 until your software opens the port, waiting at
    /// most N seconds (then failing loudly rather than running a session with
    /// nobody on the far end). Without this the run starts immediately and any
    /// output before you attach is held and flushed on attach.
    #[arg(long, value_name = "SECS", help_heading = "Host serial")]
    serial_wait: Option<f64>,

    /// Let the co-sim run as fast as it can under --serial-attach until your
    /// software opens the port. While a peer is attached, its bytes are
    /// delivered on a fixed compressed schedule (its own wall-clock gaps,
    /// scaled into sim time) rather than free-running: byte-arrival timing
    /// against a real process must not depend on machine load, or timing
    /// verdicts change with load.
    #[arg(long, help_heading = "Host serial")]
    serial_no_pace: bool,

    /// Which MCU's UART to bridge when the board carries more than one
    /// (reference designator, e.g. U1). Default: every MCU on the board.
    #[arg(long, value_name = "REF", help_heading = "Host serial")]
    serial_mcu: Option<String>,

    /// Simulate these Do-Not-Populate parts as fitted, whatever the DNP policy
    /// says. Comma-separated and/or repeatable: `--fit A101,R7`. An unknown
    /// reference is a loud error.
    #[arg(
        long,
        value_name = "REF[,REF...]",
        value_delimiter = ',',
        help_heading = "Do-not-populate"
    )]
    fit: Vec<String>,

    /// Leave these Do-Not-Populate parts open, whatever the DNP policy says.
    /// The inverse of `--fit`, for a footprint you know will stay empty.
    #[arg(
        long,
        value_name = "REF[,REF...]",
        value_delimiter = ',',
        help_heading = "Do-not-populate"
    )]
    no_fit: Vec<String>,

    /// Leave every DNP part out of the simulation, matching what a fab house
    /// would build from the board file. The default instead simulates DNP
    /// parts as fitted, because most DNP footprints get placed eventually,
    /// while keeping near-zero-ohm links (0R bridges, solder jumpers, ferrite
    /// beads) open, since fitting one of those merges the nets it bridges.
    /// Every run prints which parts were fitted and which were left open.
    #[arg(long, conflicts_with = "fit_all_dnp", help_heading = "Do-not-populate")]
    honour_dnp: bool,

    /// Simulate every DNP part as fitted, including near-zero-ohm links.
    #[arg(long, help_heading = "Do-not-populate")]
    fit_all_dnp: bool,
}

#[derive(Parser)]
struct ToCodeArgs {
    /// Board file to decompile.
    #[arg(value_name = "BOARD")]
    board: PathBuf,

    /// Write the Board-as-Code to this file (default: print to stdout).
    #[arg(long, value_name = "FILE")]
    out: Option<PathBuf>,
}

#[derive(Parser)]
struct FromCodeArgs {
    /// Board-as-Code file (or directory) to recompile.
    #[arg(value_name = "CODE")]
    code: PathBuf,

    /// Write the `.kicad_pcb` here (default: print to stdout).
    #[arg(long, value_name = "FILE")]
    out: Option<PathBuf>,

    /// Re-place every part from scratch before emitting.
    #[arg(long, conflicts_with = "incremental")]
    relayout: bool,

    /// Re-place only parts that moved, keeping the rest pinned.
    #[arg(long)]
    incremental: bool,

    /// Autoroute via freerouting (falls back to the grid A* if absent).
    #[arg(long, conflicts_with = "route_grid")]
    route: bool,

    /// Force the in-tree grid A* router instead of freerouting.
    #[arg(long)]
    route_grid: bool,

    /// Fail (exit non-zero) when the route leaves open connections, a serious
    /// (short) DRC finding, or a wrong-net endpoint. Only affects a routing run.
    #[arg(long)]
    route_strict: bool,

    /// Wall-clock budget in seconds for the freerouting run before it is
    /// killed (and the grid A* fallback takes over). The default 180 s only
    /// covers small boards: a real 137-part board legitimately needs 12-15
    /// minutes, so raise this (e.g. --route-timeout 1200) rather than let the
    /// fallback silently take over.
    #[arg(long, value_name = "SECS", default_value_t = 180)]
    route_timeout: u64,

    /// Maximum freerouting optimisation passes (its `-mp`). Fewer passes
    /// finish sooner at some routing-quality cost.
    #[arg(long, value_name = "N", default_value_t = 10)]
    route_passes: u32,

    /// Explicit freerouting jar to run, overriding $FREEROUTING_JAR and the
    /// conventional-location search (tools/ up the tree, ~/.local/share/freerouting).
    #[arg(long, value_name = "JAR")]
    freerouting_jar: Option<PathBuf>,

    /// Write the Specctra routing DSN to this file and STOP before routing:
    /// route it with any Specctra-capable router, for as long as you like,
    /// then merge the SES back with `hauksbee merge-ses`. The unrouted board
    /// is still emitted to --out/stdout as usual.
    #[arg(long, value_name = "FILE", conflicts_with_all = ["route", "route_grid"])]
    route_dsn: Option<PathBuf>,

    /// Emit one machine-readable JSON object for the routing run on stdout
    /// instead of prose: {"nets_total","connections_routed","unrouted",
    /// "segments","vias","seconds","engine","drc_serious",
    /// "endpoint_net_violations"} (with --route-dsn: {"ok","dsn"}). Requires
    /// --out so the board text does not share stdout, and a routing flag
    /// (--route/--route-grid/--route-dsn) to describe.
    #[arg(long, requires = "out")]
    json: bool,
}

#[derive(Parser)]
struct MergeSesArgs {
    /// Board-as-Code file (or directory) the DSN was exported from.
    #[arg(value_name = "CODE")]
    code: PathBuf,

    /// The routed Specctra SES file to merge (coordinate scale auto-detected).
    #[arg(value_name = "SES")]
    ses: PathBuf,

    /// Write the routed `.kicad_pcb` here (default: print to stdout).
    #[arg(long, value_name = "FILE")]
    out: Option<PathBuf>,

    /// Fail (exit non-zero) when the merged route leaves open connections, a
    /// serious (short) DRC finding, or a wrong-net endpoint.
    #[arg(long)]
    route_strict: bool,

    /// Emit one machine-readable JSON object for the merge on stdout (same
    /// shape as `from-code --route --json`, engine "merged-ses"). Requires
    /// --out so the board text does not share stdout.
    #[arg(long, requires = "out")]
    json: bool,
}

#[derive(Parser)]
struct ServeArgs {
    /// Port for the local web front door. If busy, the next free port is
    /// bound instead and the printed URL reflects the real one.
    #[arg(long, default_value_t = 3001, value_name = "PORT")]
    port: u16,

    /// Open the system browser at the served URL once the listener is up.
    ///
    /// Also happens automatically (without this flag) when serve was launched
    /// by the desktop app, where nobody can read the printed URL. Opening is
    /// best-effort: on a headless machine with no browser it silently does
    /// nothing. A piped/CI launch never auto-opens.
    #[arg(long)]
    open: bool,

    /// Never open a browser, even when launched by the desktop app.
    #[arg(long, conflicts_with = "open")]
    no_open: bool,
}

#[derive(Parser)]
// At most one analysis may be forced; with none, the deck's directives decide.
#[command(group(
    clap::ArgGroup::new("analysis")
        .args(["op", "tran", "ac", "dc"])
        .multiple(false)
))]
struct SimArgs {
    /// SPICE deck to simulate (`.cir`).
    #[arg(value_name = "DECK", required_unless_present = "example")]
    file: Option<PathBuf>,

    /// Simulate an embedded example deck instead of a file (try
    /// `rlc_ringdown`). Materialized under the temp directory, so this works
    /// with no checkout on disk.
    #[arg(long, value_name = "NAME", conflicts_with = "file")]
    example: Option<String>,

    /// Write the CSV here (default: print to stdout).
    #[arg(long, value_name = "FILE")]
    out: Option<PathBuf>,

    /// Output format. `csv` (default) writes one column per probe; `raw` writes
    /// an ngspice ASCII rawfile (to `--out` if given, else stdout); `both` writes
    /// a CSV and a rawfile side by side and requires `--out` to name them.
    #[arg(long, value_enum, default_value_t = hauksbee_engine::commands::sim::SimFormat::Csv)]
    format: hauksbee_engine::commands::sim::SimFormat,

    /// Force the DC operating point analysis.
    #[arg(long, group = "analysis")]
    op: bool,

    /// Force a transient run (needs a `.tran` card for the window).
    #[arg(long, group = "analysis")]
    tran: bool,

    /// Run the deck's `.ac` sweep (small-signal). Needs an `.ac` card and at
    /// least one source with an `AC <mag> [phase]` spec.
    #[arg(long, group = "analysis")]
    ac: bool,

    /// Run the deck's `.dc` sweep. Needs a `.dc <src> <start> <stop> <step>` card.
    #[arg(long, group = "analysis")]
    dc: bool,

    /// Probe expressions to output: `V(a)`, `V(a,b)`, `I(V1)`. Overrides the
    /// deck's `.print`. Space-separated. Default: every node voltage.
    #[arg(long, value_name = "PROBE", num_args = 1..)]
    print: Vec<String>,
}

#[derive(Parser)]
struct DoctorArgs {
    /// Report co-sim backend availability (the default and, for now, only
    /// check). Accepted explicitly so the documented `hauksbee doctor
    /// --backends` invocation is stable if future checks are added.
    #[arg(long)]
    backends: bool,

    /// Emit machine-readable JSON (`{"backends":[{name,status,available,...}]}`)
    /// instead of the tab-separated text table.
    #[arg(long)]
    json: bool,
}

#[derive(Parser)]
struct CheckCodeArgs {
    /// Board-as-Code file (or directory) to check.
    #[arg(value_name = "CODE")]
    code: PathBuf,

    /// Seconds of simulated time to run the stress monitor for.
    #[arg(
        long,
        default_value_t = 0.2,
        value_name = "N",
        value_parser = parse_seconds,
        allow_negative_numbers = true
    )]
    seconds: f64,

    /// Run the stress monitor in destructive mode (parts can be destroyed).
    #[arg(long)]
    destructive: bool,

    /// Ambient temperature (C) for the steady-state junction-temperature
    /// estimate (Tj = Tambient + P * theta_JA). Default 25 C.
    #[arg(
        long,
        default_value_t = 25.0,
        value_name = "C",
        value_parser = parse_ambient,
        allow_negative_numbers = true,
        help_heading = "Advanced / analyses"
    )]
    ambient: f64,

    /// Emit the check as one JSON object ({"ok","board","components","nets",
    /// "resolved_fraction","unresolved","simulated_seconds","active_nets",
    /// "faults"}) instead of the human table. The exit code is unchanged
    /// (1 when a part is destroyed).
    #[arg(long)]
    json: bool,
}

fn artifact_flag_value(args: &[String], flag: &str) -> Option<PathBuf> {
    args.iter()
        .take_while(|arg| arg.as_str() != "--")
        .enumerate()
        .find_map(|(index, arg)| {
            if arg == flag {
                args.get(index + 1)
                    .filter(|value| !value.starts_with('-'))
                    .map(PathBuf::from)
            } else {
                arg.strip_prefix(&format!("{flag}="))
                    .filter(|value| !value.is_empty())
                    .map(PathBuf::from)
            }
        })
}

fn invalidate_run_artifacts_after_parse_error(args: &[String], error: &clap::Error) {
    // The only global option accepted before a top-level subcommand is the
    // value-less --quiet flag. Do not scan arbitrary positional values for the
    // word `run`: e.g. in `to-code run --junit FILE`, `run` is a board name and
    // grants no permission to mutate FILE.
    let mut run_index = 1;
    while args.get(run_index).is_some_and(|arg| arg == "--quiet") {
        run_index += 1;
    }
    if args.get(run_index).map(String::as_str) != Some("run") {
        return;
    }
    let protected: Vec<PathBuf> = args
        .iter()
        .take_while(|arg| arg.as_str() != "--")
        .enumerate()
        .skip(run_index + 1)
        .filter(|(index, arg)| {
            !arg.starts_with('-')
                && !matches!(
                    args.get(index.saturating_sub(1)).map(String::as_str),
                    Some("--junit" | "--sarif")
                )
        })
        .map(|(_, arg)| PathBuf::from(arg))
        .chain(
            [
                "--bom",
                "--placement",
                "--firmware",
                "--asbuilt",
                "--emit-manifest",
                "--ac-csv",
                "--probe-csv",
            ]
            .into_iter()
            .filter_map(|flag| artifact_flag_value(args, flag)),
        )
        .collect();
    let board = protected
        .first()
        .cloned()
        .unwrap_or_else(|| PathBuf::from("unparsed-run-input"));
    let mut junit = artifact_flag_value(args, "--junit");
    let mut sarif = artifact_flag_value(args, "--sarif");
    if junit.as_ref().is_some_and(|path| {
        protected
            .iter()
            .any(|input| hauksbee_engine::reports::ci_artifacts::paths_alias(path, input))
    }) {
        junit = None;
    }
    if sarif.as_ref().is_some_and(|path| {
        protected
            .iter()
            .any(|input| hauksbee_engine::reports::ci_artifacts::paths_alias(path, input))
    }) {
        sarif = None;
    }
    if junit
        .as_ref()
        .zip(sarif.as_ref())
        .is_some_and(|(junit, sarif)| {
            hauksbee_engine::reports::ci_artifacts::paths_alias(junit, sarif)
        })
    {
        // Neither name is safe: writing one mutates the other. A usage-error
        // invalidator cannot report into an output pair it cannot distinguish.
        junit = None;
        sarif = None;
    }
    if junit.is_none() && sarif.is_none() {
        return;
    }
    if let Err(write_error) = hauksbee_engine::reports::ci_artifacts::begin_run(
        &board,
        junit.as_deref(),
        sarif.as_deref(),
        vec!["run".into()],
    ) {
        eprintln!("error: could not invalidate requested CI artifact: {write_error}");
        return;
    }
    hauksbee_engine::reports::ci_artifacts::finish_error(
        &anyhow::anyhow!(error.to_string()),
        error.exit_code(),
    );
}

fn main() -> anyhow::Result<()> {
    // Before anything can spawn a co-sim emulator: make sure a SIGTERM/SIGINT
    // to this process (e.g. killing a long-lived `hauksbee serve`) reaps every
    // live Renode/QEMU child instead of orphaning it. See
    // hauksbee_mcu::children for the leak this closes.
    hauksbee_mcu::children::install_signal_reaper();
    // `hauksbee <board.kicad_pcb>` (no subcommand) is the most natural first
    // thing to type; clap would answer "unrecognized subcommand". Catch a
    // first argument that is a board file and say the actual fix.
    if let Some(first) = std::env::args().nth(1) {
        if !first.starts_with('-') && looks_like_board_input(std::path::Path::new(&first)) {
            eprintln!("error: '{first}' looks like a board file, and hauksbee needs a subcommand.");
            eprintln!("  Try: hauksbee run {first} --check");
            std::process::exit(2);
        }
    }
    let raw_args: Vec<String> = std::env::args().collect();
    let cli = match Cli::try_parse_from(&raw_args) {
        Ok(cli) => cli,
        Err(error) => {
            if !matches!(
                error.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            ) {
                invalidate_run_artifacts_after_parse_error(&raw_args, &error);
            }
            error.exit()
        }
    };
    // Fix #3 (LOW): under `--json`, an AI/CI consumer expects parseable output on
    // EVERY path, including a hard error. Emit `{"ok": false, "error": "..."}`
    // instead of the plaintext `error:` line so the failure is still valid JSON.
    // Applies to every subcommand that has a `--json` flag.
    let json = match &cli.command {
        Command::Run(args) => args.json,
        Command::FromCode(args) => args.json,
        Command::MergeSes(args) => args.json,
        Command::CheckCode(args) => args.json,
        Command::Models(args) => {
            matches!(&args.command, ModelsCommand::Resolve(r) if r.json)
        }
        _ => false,
    };
    let quiet = cli.quiet;
    let result = match cli.command {
        // The run orchestrator owns embedded-example lookup so it can open the
        // requested artifact transaction first. Its Result still flows through
        // the shared JSON/text error envelope below.
        Command::Run(mut args) => {
            let schematic = args.schematic.take();
            hauksbee_engine::commands::run::run_with_schematic(run_config(args), quiet, schematic)
        }
        Command::Reproduce(args) => hauksbee_engine::run_manifest::reproduce(&args.manifest),
        Command::ToCode(args) => {
            hauksbee_engine::commands::boardcode::to_code(&args.board, args.out.as_deref())
        }
        Command::FromCode(args) => hauksbee_engine::commands::boardcode::from_code(
            &args.code,
            &hauksbee_engine::commands::boardcode::FromCodeOpts {
                out: args.out,
                relayout: args.relayout,
                incremental: args.incremental,
                route: args.route,
                route_grid: args.route_grid,
                route_strict: args.route_strict,
                route_timeout_secs: args.route_timeout,
                route_passes: args.route_passes,
                freerouting_jar: args.freerouting_jar,
                route_dsn: args.route_dsn,
                json: args.json,
            },
        ),
        Command::MergeSes(args) => hauksbee_engine::commands::boardcode::merge_ses(
            &args.code,
            &args.ses,
            args.out.as_deref(),
            args.route_strict,
            args.json,
        ),
        Command::CheckCode(args) => {
            // A board file handed to check-code used to fall into the DSL
            // parser and emit a nonsense parse error; name the actual fix.
            if board_extension(&args.code) {
                eprintln!(
                    "error: check-code reads Board-as-Code (.board) files; for board checks run: \
                     hauksbee run {} --check",
                    args.code.display()
                );
                std::process::exit(2);
            }
            hauksbee_engine::commands::boardcode::check(
                &args.code,
                args.seconds,
                args.destructive,
                args.ambient,
                args.json,
            )
        }
        Command::Serve(args) => {
            hauksbee_engine::commands::serve::run(args.port, args.open, args.no_open)
        }
        Command::Doctor(args) => hauksbee_engine::commands::doctor::run(args.backends, args.json),
        // Same no-`?` rule as Run above: an unknown `--example` name must
        // reach the shared error handler, not escape through `main`'s return.
        Command::Sim(mut args) => (|| {
            if let Some(name) = &args.example {
                args.file = Some(hauksbee_engine::commands::examples::deck(name)?);
            }
            hauksbee_engine::commands::sim::run(
                &args.file.expect("DECK or --example (enforced by clap)"),
                args.out.as_deref(),
                args.format,
                args.op,
                args.tran,
                args.ac,
                args.dc,
                &args.print,
            )
        })(),
        Command::Models(args) => match args.command {
            ModelsCommand::Lint(args) => hauksbee_engine::commands::models::lint(&args.file),
            ModelsCommand::Add(args) => hauksbee_engine::commands::models::add(&args.source),
            ModelsCommand::Remove(args) => hauksbee_engine::commands::models::remove(&args.name),
            ModelsCommand::List(args) => hauksbee_engine::commands::models::list(args.builtin),
            ModelsCommand::Resolve(args) => hauksbee_engine::commands::models::resolve_checked(
                &args.board,
                args.models_dir.as_deref(),
                args.json,
                hauksbee_engine::commands::models::ModelRequirement {
                    minimum_tier: args.min_model_tier,
                    minimum_validation: args.min_model_validation,
                    require_intervals: args.require_model_intervals,
                },
            ),
            ModelsCommand::New(args) => hauksbee_engine::commands::models::new(
                &args.reference,
                &args.board,
                args.out.as_deref(),
            ),
            ModelsCommand::Extract(args) => hauksbee_engine::commands::models::extract(
                &args.pdf,
                &args.part,
                &args.kind,
                args.out_dir.as_deref(),
                args.yes,
                args.backend.map(Into::into),
                args.model,
                args.api_base,
                args.api_key_env,
            ),
        },
        Command::Watch(args) => {
            hauksbee_engine::commands::watch::run(args.target, args.plain, args.once)
        }
        Command::Install(args) => match args.command {
            InstallCommand::EspQemu { yes } => hauksbee_engine::commands::install::esp_qemu(yes),
            InstallCommand::Renode { yes } => hauksbee_engine::commands::install::renode(yes),
        },
    };
    if let Err(e) = &result {
        if json {
            // serde_json escapes the message so the object is always well-formed.
            let obj = serde_json::json!({ "ok": false, "error": e.to_string() });
            println!("{obj}");
        } else {
            eprintln!("error: {e}");
        }
        std::process::exit(error_exit_code(e));
    }
    result
}

/// Preserve the typed invalid-for-analysis contract through anyhow's shared
/// CLI error envelope. Parser and reconciliation errors are not failed checks:
/// there was no trustworthy board state to check, so they exit 3 in text and
/// JSON modes alike.
fn error_exit_code(error: &anyhow::Error) -> i32 {
    if let Some(error) = error.downcast_ref::<hauksbee_extract::bom::BomError>() {
        return error.exit_code();
    }
    if let Some(error) = error.downcast_ref::<hauksbee_extract::placement::PlacementError>() {
        return error.exit_code();
    }
    if let Some(error) = error.downcast_ref::<hauksbee_engine::binder::IdentityRefusal>() {
        return error.exit_code();
    }
    1
}

/// Whether a path carries an extension of a BOARD design format (the inputs
/// `run` reads). Deliberately does NOT include `.board`: Board-as-Code is the
/// one format `check-code` legitimately reads.
fn board_extension(p: &std::path::Path) -> bool {
    let ext = p
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    matches!(
        ext.as_str(),
        "kicad_pcb" | "kicad_sch" | "brd" | "pcbdoc" | "d356" | "net"
    )
}

/// Whether a bare first argument looks like a board input rather than a
/// subcommand: a board-format extension, a `.board` file, or an existing
/// gerber directory/zip. Used only for the no-subcommand hint, so it must
/// never match a real subcommand name (none contain a dot or a path
/// separator that resolves to a file).
fn looks_like_board_input(p: &std::path::Path) -> bool {
    if board_extension(p) {
        return true;
    }
    let ext = p
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    if ext == "board" || ext == "zip" {
        return true;
    }
    p.is_dir() && p.exists() && p.to_str().is_some_and(|s| s.contains('/'))
}

/// Deconstruct the parsed `RunArgs` (clap) into the library's plain
/// [`hauksbee_engine::commands::run::RunConfig`],
/// so the run orchestrator lives in `hauksbee_engine::commands::run` while
/// argument parsing stays here.
fn run_config(a: RunArgs) -> hauksbee_engine::commands::run::RunConfig {
    hauksbee_engine::commands::run::RunConfig {
        // Present by construction unless --example. The run orchestrator
        // resolves embedded examples only after it has opened the requested
        // fail-closed artifact transaction.
        board: a.board.unwrap_or_else(|| {
            PathBuf::from(format!(
                "embedded-example-{}",
                a.example
                    .as_deref()
                    .expect("BOARD or --example (enforced by clap)")
            ))
        }),
        example: a.example,
        bom: a.bom,
        bom_columns: a.bom_columns,
        placement: a.placement,
        firmware: a.firmware,
        asbuilt: a.asbuilt,
        junit: a.junit,
        sarif: a.sarif,
        emit_manifest: a.emit_manifest,
        manifest_command: hauksbee_engine::run_manifest::replay_argv("hauksbee"),
        seconds: a.seconds,
        headless: a.headless,
        report: a.report,
        drc: a.drc,
        ampacity: a.ampacity,
        lint: a.lint,
        si: a.si,
        resources: a.resources,
        usb_c: a.usb_c,
        thermal: a.thermal,
        ambient: a.ambient,
        plain: a.plain,
        verbose: a.verbose,
        json: a.json,
        strict: a.strict,
        strict_thermal: a.strict_thermal,
        no_strict_thermal: a.no_strict_thermal,
        strict_boot: a.strict_boot,
        list_nets: a.list_nets,
        check: a.check,
        oracle: a.oracle,
        apply_shorts: a.apply_shorts,
        serve: a.serve,
        open: a.open,
        no_open: a.no_open,
        tui: a.tui,
        port: a.port,
        models_dir: a.models_dir,
        ac: a.ac,
        ac_node: a.ac_node,
        ac_csv: a.ac_csv,
        ac_loop: a.ac_loop,
        probe: a.probe,
        probe_csv: a.probe_csv,
        chunk_us: a.chunk_us,
        serial_attach: a.serial_attach,
        serial_transport: a.serial_transport.into(),
        serial_wait: a.serial_wait,
        serial_no_pace: a.serial_no_pace,
        serial_mcu: a.serial_mcu,
        fit: a.fit,
        no_fit: a.no_fit,
        dnp_policy: if a.honour_dnp {
            hauksbee_extract::dnp::DnpPolicy::Honour
        } else if a.fit_all_dnp {
            hauksbee_extract::dnp::DnpPolicy::FitAll
        } else {
            hauksbee_extract::dnp::DnpPolicy::FitExceptLinks
        },
    }
}
