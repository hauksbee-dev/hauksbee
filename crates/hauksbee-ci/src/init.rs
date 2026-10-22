//! `hauksbee-ci init <board>`: scaffold a starter spec from a real board, so a
//! user's first CI spec is an edit and not a blank page.
//!
//! It loads and binds the board through the same extract+bind path the runner
//! uses, then reads the detected supplies (the binder's supply legs), the
//! detected MCU, and the board's rail-looking nets straight off the bound board.
//! Every generated line carries a short comment naming what it does, cribbed
//! from docs/ci/CI.md, so the file teaches its own format as the user edits it.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use hauksbee_engine::{bind_board, is_ground, power_rail_voltage, BoundBoard};
use hauksbee_extract::ExtractedBoard;
use hauksbee_models::ModelLibrary;

use crate::error::SpecError;
use crate::runner;

/// Scaffold `<board-stem>.toml` into the CURRENT DIRECTORY and return its path.
///
/// The old default (beside the board) put the spec where no CI tooling looks;
/// writing where the user is standing means `hauksbee-ci init
/// hardware/board.kicad_pcb` run from `ci/` lands the spec in `ci/`, which is
/// exactly where the pre-commit hook and the action's auto-detect search. The
/// generated `board = "..."` path is computed relative to the output directory,
/// so the spec is valid wherever it lands. Refuses to overwrite an existing
/// file: the point is a starting point, not a clobber of hand-written work.
///
/// # Errors
///
/// As [`init_to`].
pub fn init(board: &Path) -> Result<PathBuf, SpecError> {
    init_to(board, None)
}

/// [`init`] with an explicit destination (`--out`).
///
/// A path ending in `.toml` is the spec file itself, and anything else is a
/// directory that gets `<board-stem>.toml` inside it (created if it does not
/// exist yet). `None` means the current directory.
///
/// The suffix decides, not the filesystem. `--out ci` on a repo that has no
/// `ci/` yet is the common first command, and the guidance printed right after
/// says specs are discovered in `ci/`; resolving that to a FILE named `ci`
/// would scaffold a spec no tool ever finds, which is a gate that silently
/// checks nothing.
///
/// # Errors
///
/// [`SpecError::Io`] when the current directory or a destination directory
/// cannot be read or created, or the spec cannot be written;
/// [`SpecError::Invalid`] when the spec file already exists; and any error
/// from loading the board while rendering it.
pub fn init_to(board: &Path, out: Option<&Path>) -> Result<PathBuf, SpecError> {
    let (out, spec_dir, run_hint) = plan_destination(board, out)?;
    let spec = render_spec_at_with_run_hint(board, &spec_dir, &run_hint)?;
    std::fs::write(&out, spec)
        .map_err(|e| SpecError::Io(format!("writing {}: {e}", out.display())))?;
    Ok(out)
}

/// How long `init --suggest-asserts` runs the firmware when `--duration-ms` is
/// not given: long enough to see several periods of a blink, which the starter
/// spec's 200 ms power-up window is not.
pub const OBSERVE_DEFAULT_MS: f64 = 1000.0;

/// The most toggling nets proposed from one run; a board with a bus busy on
/// dozens of pins would otherwise bury the three assertions that matter.
const MAX_SUGGESTED_NETS: usize = 8;

/// First line of the section `init --suggest-asserts` appends. The tests (and
/// anyone scripting around it) find the suggestions by this text.
pub const SUGGESTED_HEADER: &str = "## ---- asserts suggested from one run of";

/// Say one assertion as `[[assert]]` TOML: the single writer behind both
/// `init --suggest-asserts` (commented, for the user to opt into) and
/// `hauksbee-ci add` (live, appended to a spec).
///
/// `fields` are `(key, value)` in the order they should read. `notes` are
/// prose lines above the block: `## ` lines when commented (so uncommenting a
/// block by stripping `# ` leaves them alone), plain `# ` comments when live.
#[must_use]
pub fn render_assert_block(
    kind: &str,
    fields: &[(String, toml::Value)],
    notes: &[String],
    commented: bool,
) -> String {
    let (mark, note_mark) = if commented { ("# ", "## ") } else { ("", "# ") };
    let mut out = String::new();
    for note in notes {
        let _ = writeln!(out, "{note_mark}{note}");
    }
    let _ = writeln!(out, "{mark}[[assert]]");
    let _ = writeln!(
        out,
        "{mark}kind = {}",
        toml::Value::String(kind.to_string())
    );
    for (key, value) in fields {
        let _ = writeln!(out, "{mark}{key} = {value}");
    }
    out
}

/// What `init --firmware .. --suggest-asserts` should look at.
#[derive(Debug, Clone, Copy)]
pub struct Observe<'a> {
    /// The firmware image to boot.
    pub firmware: &'a Path,
    /// How long to run it; [`OBSERVE_DEFAULT_MS`] when `None`.
    pub duration_ms: Option<f64>,
}

/// What [`init_observing`] wrote and what it learned.
#[derive(Debug, Clone)]
pub struct Suggested {
    /// The spec file.
    pub path: PathBuf,
    /// One line per assertion proposed (`toggle PC13: 5.0 Hz`).
    pub proposed: Vec<String>,
    /// Why the run could not be observed, when it could not; the spec is
    /// still written, with the firmware line live and nothing proposed.
    pub warning: Option<String>,
}

/// One toggling net the run saw.
struct ToggleSeen {
    net: String,
    edges: u64,
    /// The edge-timed rate, or why none could be timed.
    rate: Result<f64, String>,
}

/// Everything a run showed that an assertion can say.
struct Seen {
    toggles: Vec<ToggleSeen>,
    uart_line: Option<String>,
    entries: Vec<String>,
    /// The pulse width (microseconds) a polling MCU backend guarantees to see,
    /// when the firmware ran on one. A `toggle` on such a backend is refused
    /// unless the spec declares `timing.min_pulse_us`.
    poll_pulse_floor_us: Option<f64>,
}

/// [`init_to`], then boot `observe.firmware` once and append the assertions
/// that run supports as commented blocks the user can opt into.
///
/// The firmware line is live and the starter's other blocks are unchanged, so
/// uncommenting a suggestion yields a spec that passes on the run it came from
/// and fails when the number in it is edited to something the firmware does
/// not do.
///
/// # Errors
///
/// As [`init_to`]. A firmware run that fails is not an error: the spec is
/// written and [`Suggested::warning`] says why nothing was proposed.
pub fn init_observing(
    board: &Path,
    out: Option<&Path>,
    observe: Observe<'_>,
) -> Result<Suggested, SpecError> {
    let (path, spec_dir, run_hint) = plan_destination(board, out)?;
    let mut text = render_spec_at_with_run_hint(board, &spec_dir, &run_hint)?;
    let duration_ms = observe.duration_ms.unwrap_or(OBSERVE_DEFAULT_MS);
    let firmware_ref = toml_path(board_reference(observe.firmware, &spec_dir));
    text = replace_line(
        &text,
        "# firmware = ",
        &format!("firmware = {firmware_ref}   # the image the suggestions below were observed on"),
    )?;
    text = replace_line(
        &text,
        "duration_ms = ",
        &format!("duration_ms = {duration_ms}               # simulated time to run"),
    )?;
    std::fs::write(&path, &text)
        .map_err(|e| SpecError::Io(format!("writing {}: {e}", path.display())))?;

    let name = observe.firmware.file_name().map_or_else(
        || "the firmware".into(),
        |n| n.to_string_lossy().into_owned(),
    );
    let (section, proposed, warning) = match observe_firmware(&path) {
        Ok(seen) => {
            let (blocks, proposed) = suggestion_blocks(&seen);
            text.push_str(&timing_declaration(&seen));
            (blocks, proposed, None)
        }
        Err(e) => (String::new(), Vec::new(), Some(e.to_string())),
    };
    // Prose is `## `, code is `# `: stripping `# ` from every line that has it
    // uncomments the blocks and leaves every note a comment.
    let mut tail = format!(
        "\n{SUGGESTED_HEADER} {name} ({duration_ms} ms) ----\n\
         ## Each block is something the firmware DID in that run, said as an assertion.\n\
         ## Uncomment the ones you want to hold it to. Edit a number to say what you\n\
         ## EXPECT instead; `hauksbee-ci explain <kind>` lists a kind's fields.\n"
    );
    match &warning {
        Some(why) => {
            let _ = writeln!(
                tail,
                "## The run did not finish, so nothing is proposed: {}",
                why.lines().next().unwrap_or("")
            );
        }
        None if proposed.is_empty() => {
            tail.push_str(
                "## The run showed nothing to assert on: no net toggled, no UART text,\n\
                 ## and the core's entry point was not reported.\n",
            );
        }
        None => tail.push_str(&section),
    }
    let _ = writeln!(
        tail,
        "##\n## Not observed (a run does not record the bench view yet, so these cannot be\n\
         ## proposed from it): bus traffic `bus`, reset count `resets`, UART baud\n\
         ## `uart_baud`. Add one by hand once you know what to expect:\n\
         ##   hauksbee-ci add {run_hint} bus protocol=i2c addr=0x48"
    );
    text.push_str(&tail);
    std::fs::write(&path, text)
        .map_err(|e| SpecError::Io(format!("writing {}: {e}", path.display())))?;
    Ok(Suggested {
        path,
        proposed,
        warning,
    })
}

/// `text` with the first line that starts with `prefix` replaced by `line`.
fn replace_line(text: &str, prefix: &str, line: &str) -> Result<String, SpecError> {
    let mut found = false;
    let mut out = String::with_capacity(text.len() + line.len());
    for l in text.lines() {
        if !found && l.starts_with(prefix) {
            found = true;
            out.push_str(line);
        } else {
            out.push_str(l);
        }
        out.push('\n');
    }
    if found {
        Ok(out)
    } else {
        Err(SpecError::Invalid(format!(
            "the starter spec has no `{}` line to point at the firmware",
            prefix.trim()
        )))
    }
}

/// Boot the firmware in the spec at `path` and read off what it did.
///
/// Two runs, because a net's waveform is only recorded for a net some
/// assertion times: the first finds the nets that moved (and the UART text
/// and the entry symbol), the second times each mover's edges.
fn observe_firmware(path: &Path) -> Result<Seen, SpecError> {
    use crate::spec::{Assertion, Spec};
    let spec = Spec::load(path)?;
    let first = runner::run_spec(&spec)?;
    let out = first
        .first()
        .ok_or_else(|| SpecError::Invalid("the run produced no outcome".into()))?;

    let mut movers: Vec<(String, u64)> = out
        .toggles
        .iter()
        .filter(|&(net, &n)| {
            n >= 2
                && !is_ground(net)
                && power_rail_voltage(net).is_none()
                && !looks_like_power_name(net)
        })
        .map(|(net, &n)| (net.clone(), n))
        .collect();
    movers.sort();
    movers.truncate(MAX_SUGGESTED_NETS);

    let mut timed = spec.clone();
    timed.asserts = movers
        .iter()
        .map(|(net, _)| Assertion {
            kind: "toggle".into(),
            net: Some(net.clone()),
            ..Default::default()
        })
        .collect();
    let dt_s = spec.frame_ms.max(1e-3) / 1000.0;
    let series = if movers.is_empty() {
        Vec::new()
    } else {
        runner::run_spec(&timed)?
    };
    let toggles = movers
        .into_iter()
        .map(|(net, edges)| {
            let rate = series
                .first()
                .and_then(|o| o.net_series.get(&net))
                .map_or_else(
                    || Err("the run recorded no waveform for it".to_string()),
                    |s| crate::assertions::edge_rate(s, dt_s).map(|(hz, _)| hz),
                )
                .and_then(|hz| {
                    // The same refusal the toggle assertion makes: under ten
                    // samples a period the grid is reporting itself, not a rate.
                    if 1.0 / (hz * dt_s) < 10.0 {
                        Err(format!(
                            "{hz:.1} Hz is under-sampled at frame_ms = {}; lower frame_ms to time it",
                            spec.frame_ms
                        ))
                    } else {
                        Ok(hz)
                    }
                });
            ToggleSeen { net, edges, rate }
        })
        .collect();

    let uart_line = {
        let mut texts: Vec<(&String, &String)> = out.uart.iter().collect();
        texts.sort();
        texts.into_iter().find_map(|(_, text)| {
            text.lines()
                .map(str::trim)
                .find(|l| !l.is_empty())
                .map(|l| l.chars().take(80).collect::<String>())
        })
    };
    let mut entries: Vec<String> = out
        .execution
        .iter()
        .filter(|e| e.reached_entry == hauksbee_engine::execution::Observed::Yes)
        .filter_map(|e| e.entry_symbol.clone())
        .collect();
    entries.sort();
    entries.dedup();
    let poll_pulse_floor_us = out
        .timing_coverage
        .iter()
        .filter(|c| !c.cycle_exact && c.backend != hauksbee_engine::scheduler::CHUNK_SAMPLED_LOGIC)
        .map(|c| c.minimum_guaranteed_pulse_s * 1e6)
        .reduce(f64::max);
    Ok(Seen {
        toggles,
        uart_line,
        entries,
        poll_pulse_floor_us,
    })
}

/// The `[timing]` table a `toggle` suggestion needs on a polling backend, as
/// live TOML with its reason, or nothing when no toggle is proposed or the
/// backend sees every edge.
///
/// A poll cannot prove a pulse did not rise and fall between two samples, so
/// the engine refuses a `toggle` there unless the spec says how narrow a pulse
/// it needs seen. The value is the width the backend already guarantees,
/// lowered to a quarter of the fastest timed period so the check never claims
/// a pulse the polling cannot resolve.
fn timing_declaration(seen: &Seen) -> String {
    let Some(floor_us) = seen.poll_pulse_floor_us else {
        return String::new();
    };
    if seen.toggles.is_empty() {
        return String::new();
    }
    let quarter_period_us = seen
        .toggles
        .iter()
        .filter_map(|t| t.rate.as_ref().ok())
        .map(|hz| 0.25e6 / hz)
        .fold(f64::INFINITY, f64::min);
    let min_pulse_us = floor_us.min(quarter_period_us).max(1.0).floor();
    format!(
        "\n# The firmware runs on a polling backend, which cannot prove a pulse did not rise\n\
         # and fall between two samples, so a `toggle` check must say how narrow a pulse\n\
         # it needs seen. This is the width the backend already guarantees.\n\
         [timing]\n\
         min_pulse_us = {min_pulse_us}\n"
    )
}

/// The commented blocks for what the run saw, and one summary line per block.
fn suggestion_blocks(seen: &Seen) -> (String, Vec<String>) {
    let (mut s, mut lines) = (String::new(), Vec::new());
    let mut push = |kind: &str, fields: Vec<(String, toml::Value)>, note: String, line: String| {
        s.push_str(&render_assert_block(kind, &fields, &[note], true));
        lines.push(line);
    };
    if let Some(line) = &seen.uart_line {
        push(
            "uart",
            vec![("contains".into(), toml::Value::String(line.clone()))],
            format!("observed: the firmware printed {line:?}"),
            format!("uart: prints {line:?}"),
        );
    }
    for symbol in &seen.entries {
        push(
            "reaches",
            vec![("symbol".into(), toml::Value::String(symbol.clone()))],
            format!("observed: the core reached `{symbol}`"),
            format!("reaches: {symbol}"),
        );
    }
    for t in &seen.toggles {
        let net = toml::Value::String(t.net.clone());
        match &t.rate {
            Ok(hz) => {
                let mut fields = vec![("net".to_string(), net)];
                fields.extend(
                    crate::catalog::measured_fields("toggle", *hz)
                        .into_iter()
                        .map(|(k, v)| (k.to_string(), v)),
                );
                push(
                    "toggle",
                    fields,
                    format!("observed: {} toggled {} times, {hz:.2} Hz by edge timing", t.net, t.edges),
                    format!("toggle: {} at {hz:.2} Hz", t.net),
                );
            }
            Err(why) => push(
                "toggle",
                vec![
                    ("net".to_string(), net),
                    (
                        "min_toggles".to_string(),
                        toml::Value::Integer(i64::try_from((t.edges * 4 / 5).max(1)).unwrap_or(1)),
                    ),
                ],
                format!(
                    "observed: {} toggled {} times; no rate could be timed ({why}), so this counts instead",
                    t.net, t.edges
                ),
                format!("toggle: {} at least {} times", t.net, (t.edges * 4 / 5).max(1)),
            ),
        }
    }
    (s, lines)
}

/// Where a scaffold lands: `(spec file, its directory, the `run` hint the spec
/// prints for itself)`. Creates the directory and refuses an existing file, so
/// every way of scaffolding shares one rule about not clobbering hand-written work.
fn plan_destination(
    board: &Path,
    out: Option<&Path>,
) -> Result<(PathBuf, PathBuf, String), SpecError> {
    let out = match out {
        None => {
            let cwd = std::env::current_dir()
                .map_err(|e| SpecError::Io(format!("reading the current directory: {e}")))?;
            cwd.join(format!("{}.toml", board_stem(board)))
        }
        Some(p) => {
            // A `.toml` suffix names the spec file; every other spelling is a
            // directory to write `<board-stem>.toml` into, whether or not it
            // exists yet.
            if p.is_dir() || !names_a_spec_file(p) {
                std::fs::create_dir_all(p)
                    .map_err(|e| SpecError::Io(format!("creating {}: {e}", p.display())))?;
                p.join(format!("{}.toml", board_stem(board)))
            } else {
                if let Some(parent) = p.parent().filter(|d| !d.as_os_str().is_empty()) {
                    std::fs::create_dir_all(parent).map_err(|e| {
                        SpecError::Io(format!("creating {}: {e}", parent.display()))
                    })?;
                }
                p.to_path_buf()
            }
        }
    };
    if out.exists() {
        return Err(SpecError::Invalid(format!(
            "{} already exists; refusing to overwrite it. Move it aside (or delete it) to regenerate the starter spec.",
            out.display()
        )));
    }
    let spec_dir = out
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    let run_hint = if out.is_relative() {
        out.clone()
    } else {
        std::env::current_dir()
            .ok()
            .and_then(|cwd| relative_path(&cwd, &out))
            .unwrap_or_else(|| out.clone())
    };
    Ok((out, spec_dir, run_hint.display().to_string()))
}

/// The USB profile scaffolded for a detected USB rail: a BC1.2 / USB-C 1.5 A
/// port, the commonest thing a hobby board is plugged into. Generous enough
/// that the starter spec stays green on a board that behaves, tight enough that
/// a real inrush shows as droop.
const USB_PROFILE: &str = "5v1.5a";

/// The bench-PSU current limit scaffolded for a detected rail with no better
/// story. A bench supply is the honest default for "something outside the board
/// feeds this net": it holds `volts` until the board asks for more than this,
/// then folds back, which is what makes a rail assertion on the net falsifiable.
const BENCH_LIMIT_A: f64 = 2.0;

/// Does this detected rail read as a USB port, so the scaffold can model the
/// actual source instead of a generic bench supply? Both halves must agree: the
/// name says USB and the detected voltage is the 5 V a USB port supplies. A
/// `VBUS_3V3` level-shifter net is not a port.
fn is_usb_port_rail(net: &str, volts: f64) -> bool {
    let n = net.to_ascii_uppercase();
    (n.contains("VBUS") || n.contains("USB")) && (volts - 5.0).abs() < 0.1
}

/// Is this `--out` value the spec file itself rather than a directory to put
/// the spec in? A `.toml` extension (any case) says file; everything else,
/// including a bare name like `ci` and an explicit `ci/`, says directory.
fn names_a_spec_file(p: &Path) -> bool {
    p.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("toml"))
}

/// Does a net NAME read as a power net even though the rail detector could
/// not price it (so `power_rail_voltage` returned `None`)? Used only to keep
/// the scaffold's boot/toggle examples off nets like `VSUP_UNLIMITED` or
/// `PWR_EN_RAIL`; a false positive here just means the example falls back to
/// a placeholder, so the heuristic can afford to be broad.
fn looks_like_power_name(net: &str) -> bool {
    let n = net.to_ascii_uppercase();
    [
        "VSUP", "VDD", "VCC", "VBUS", "VBAT", "BAT+", "PWR", "POWER", "SUPPLY", "VIN", "RAIL",
    ]
    .iter()
    .any(|tag| n.contains(tag))
}

/// Does a supply net's name say a cell feeds it (`VBAT`, `BAT+`, `VCELL`,
/// `BATT`)? Such a rail is scaffolded as a `battery` source, since the spec
/// refuses an honest `kind = "battery"` without a chemistry and the user
/// should not have to discover the required fields one rerun at a time.
fn looks_like_battery_net(net: &str) -> bool {
    let n = net.to_ascii_uppercase();
    ["VBAT", "BAT+", "BATT", "VCELL", "CELL+"]
        .iter()
        .any(|tag| n.contains(tag))
}

fn board_stem(board: &Path) -> String {
    board
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("board")
        .to_string()
}

/// One `[[supply]]` the starter spec declares, as data: the CLI scaffold writes
/// it as TOML and the web builder (`hauksbee-ci init --json`) seeds its rows
/// from the same list, so both start from one spec.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ScaffoldSupply {
    pub net: String,
    /// `usb` or `bench`; the scaffold never declares an `ideal` source.
    pub kind: &'static str,
    pub volts: f64,
    /// The negotiated profile of a `usb` source.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usb: Option<&'static str>,
    /// The fold-back limit of a `bench` source.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_limit_a: Option<f64>,
    /// Why it is declared: `rail` (the board names it) or `regulator_input`
    /// (a regulator's input has no other DC support).
    pub origin: &'static str,
}

impl ScaffoldSupply {
    fn rail(net: &str, volts: f64) -> Self {
        let usb = is_usb_port_rail(net, volts);
        Self {
            net: net.to_string(),
            kind: if usb { "usb" } else { "bench" },
            volts,
            usb: usb.then_some(USB_PROFILE),
            current_limit_a: (!usb).then_some(BENCH_LIMIT_A),
            origin: "rail",
        }
    }

    const fn regulator_input(net: String, volts: f64) -> Self {
        Self {
            net,
            kind: "bench",
            volts,
            usb: None,
            current_limit_a: Some(BENCH_LIMIT_A),
            origin: "regulator_input",
        }
    }

    /// The `[[supply]]` block; `annotated` adds the teaching comments.
    fn toml_block(&self, annotated: bool) -> String {
        let note = |c: &'static str| if annotated { c } else { "" };
        let net = &self.net;
        let mut out = if self.origin == "regulator_input" {
            format!(
                "[[supply]]\nnet = \"{net}\"{}\n",
                note("                   # detected regulator input; set your adapter voltage")
            )
        } else {
            format!(
                "[[supply]]\nnet = \"{net}\"{}\n{}",
                note("                   # detected supply rail"),
                note(
                    "# cold power-on: set ramp_ms (in ms) to bring this rail up from 0 V \
                     instead of starting it settled, so power-on RC timing is simulated\n"
                ),
            )
        };
        if let Some(profile) = self.usb {
            let _ = writeln!(
                out,
                "kind = \"usb\"{}\nusb = \"{profile}\"{}",
                note("                     # the name and the 5 V say this rail is a USB port"),
                note("               # what the port negotiates: 5v0.5a | 5v1.5a | 5v3a"),
            );
        } else {
            let limit = self.current_limit_a.unwrap_or(BENCH_LIMIT_A);
            let _ = writeln!(
                out,
                "kind = \"bench\"\nvolts = {}\ncurrent_limit_a = {limit:.1}{}",
                fmt1(self.volts),
                note("            # what the source delivers before it folds back; set yours"),
            );
        }
        out
    }
}

/// What `hauksbee-ci init` detects on a board, before it is written as TOML.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Scaffold {
    pub supplies: Vec<ScaffoldSupply>,
}

/// Detect the starter spec's supplies for `board` without writing anything.
///
/// The rails the board names, then any regulator input those leave without DC
/// support, each seeded as a bench source for the user to adjust.
///
/// # Errors
///
/// Any error from loading `board` (`runner::load_board`).
pub fn scaffold(board: &Path) -> Result<Scaffold, SpecError> {
    let extracted = runner::load_board(board)?;
    hauksbee_ir::progress::learned(extracted.components.len(), extracted.nets.len());
    let lib = ModelLibrary::builtin_with_user_dirs(&[]);
    hauksbee_ir::progress::begin_once(
        hauksbee_ir::progress::Stage::Bind,
        extracted.components.len() as u64 * hauksbee_ir::progress::BIND_STEPS,
    );
    let bound = bind_board(&extracted, &lib);
    let rails: Vec<(String, f64)> = bound
        .supplies
        .iter()
        .map(|leg| (leg.net_name.clone(), leg.supply.nominal_volts()))
        .collect();
    Ok(Scaffold {
        supplies: scaffold_supplies(board, &extracted, &bound, rails),
    })
}

/// The detected rails, sorted, then the regulator inputs they leave unpowered.
/// Seeding one input can expose the regulator behind it, so the probe repeats
/// until the list stops growing (bounded: a cascade is a few stages deep).
///
/// Every supply passes the run's own refusal rule ([`runner::supply_refusal`])
/// before it is written: one on a rail a board part already drives moves to
/// the undriven input the run names instead, and one with no such place is
/// left out, so the scaffold never emits a spec its own run rejects.
fn scaffold_supplies(
    board: &Path,
    extracted: &ExtractedBoard,
    bound: &BoundBoard,
    mut rails: Vec<(String, f64)>,
) -> Vec<ScaffoldSupply> {
    rails.sort_by(|a, b| a.0.cmp(&b.0));
    let mut supplies: Vec<ScaffoldSupply> = Vec::new();
    for (net, v) in &rails {
        if placement(extracted, bound, &supplies, net.clone(), None).is_some_and(|(n, _)| &n == net)
        {
            supplies.push(ScaffoldSupply::rail(net, *v));
        }
    }
    for _ in 0..4 {
        // The probe is a run of the whole board, and only a regulator can ask for a supply:
        // a board the binder found none on has nothing for it to find.
        if bound.report.regulators.is_empty() {
            break;
        }
        // One input per pass, the one that wants the most volts: a cascade's
        // first stage (12 V into a 5 V regulator feeding a 3.3 V one) is the
        // highest, and powering it powers the stages behind it, which must not
        // also get a source of their own on the regulator's output.
        let Some(found) = unsupported_regulator_inputs(board, &supplies) else {
            // The run refused the draft, so it is never handed to the user:
            // the last regulator input added goes (the rails passed the same
            // rule above, and the first draft has no input to drop).
            if supplies
                .last()
                .is_some_and(|s| s.origin == "regulator_input")
            {
                supplies.pop();
            }
            break;
        };
        let Some((net, hint)) = found
            .into_iter()
            .filter_map(|(net, hint)| placement(extracted, bound, &supplies, net, hint))
            .max_by(|a, b| seeded_input_volts(a.1).total_cmp(&seeded_input_volts(b.1)))
        else {
            break;
        };
        supplies.push(ScaffoldSupply::regulator_input(
            net,
            seeded_input_volts(hint),
        ));
    }
    supplies
}

/// The voltage seeded on a regulator input whose regulators need at least
/// `min_v`: the next whole volt strictly above it, so a draft never sits on
/// the boundary where the run reports the regulator's input as too low to
/// judge. With no figure, a 9 V adapter is the usual guess.
fn seeded_input_volts(min_v: Option<f64>) -> f64 {
    min_v.map_or(9.0, |v| v.floor() + 1.0)
}

/// Where the run accepts a supply proposed on `net` after the draft
/// `supplies`: `net` itself, or, on a rail a board part already drives, the
/// undriven input at the top of that part's chain (with the least voltage its
/// card asks there, which replaces the downstream `hint`). `None` when the run
/// would refuse it and names nowhere else, or the place is already declared.
fn placement(
    extracted: &ExtractedBoard,
    bound: &BoundBoard,
    supplies: &[ScaffoldSupply],
    net: String,
    hint: Option<f64>,
) -> Option<(String, Option<f64>)> {
    let earlier: Vec<&str> = supplies.iter().map(|s| s.net.as_str()).collect();
    let (net, hint) = match runner::supply_refusal(bound, extracted, &net, &earlier) {
        None => (net, hint),
        Some(refusal) => {
            let (instead, min_v) = refusal.instead?;
            if runner::supply_refusal(bound, extracted, &instead, &earlier).is_some() {
                return None;
            }
            (instead, min_v)
        }
    };
    (!earlier.contains(&net.as_str())).then_some((net, hint))
}

/// Regulator inputs the draft `supplies` leave without DC support, with the
/// most input voltage any regulator on that net needs. A 1 ms run of the draft
/// reports them as `AddSupply` remedies, so this is the verdict a real run
/// would reach. `None` when the run refuses the draft.
fn unsupported_regulator_inputs(
    board: &Path,
    supplies: &[ScaffoldSupply],
) -> Option<Vec<(String, Option<f64>)>> {
    use hauksbee_ir::evidence::Remedy;
    let abs = std::fs::canonicalize(board).unwrap_or_else(|_| board.to_path_buf());
    let mut text = format!(
        "name = \"scaffold probe\"\nboard = {}\nduration_ms = 1\n",
        toml_path(abs.display().to_string())
    );
    for s in supplies {
        text.push('\n');
        text.push_str(&s.toml_block(false));
    }
    text.push_str("\n[[assert]]\nkind = \"no_faults\"\n");
    let mut spec = toml::from_str::<crate::spec::Spec>(&text).ok()?;
    spec.normalize();
    let outcomes = spec
        .validate()
        .and_then(|()| runner::run_spec(&spec))
        .ok()?;
    let mut found: Vec<(String, Option<f64>)> = Vec::new();
    for a in outcomes
        .iter()
        .filter_map(|o| o.evidence.as_ref())
        .flat_map(hauksbee_engine::BoardEvidence::assumptions)
    {
        let Remedy::AddSupply { net, volts_hint } = a.remedy() else {
            continue;
        };
        let volts_hint = *volts_hint;
        match found.iter_mut().find(|(n, _)| n == net) {
            Some((_, hint)) => {
                *hint = match (*hint, volts_hint) {
                    (Some(a), Some(b)) => Some(a.max(b)),
                    (a, b) => a.or(b),
                };
            }
            None => found.push((net.clone(), volts_hint)),
        }
    }
    Some(found)
}

/// Render the starter spec's TOML text for `board`.
///
/// The `board = "..."` reference is written relative to the board's own
/// directory (the historical beside-the-board shape). Kept for callers that
/// place the file themselves.
///
/// # Errors
///
/// As [`render_spec_at`].
pub fn render_spec(board: &Path) -> Result<String, SpecError> {
    let dir = board
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    render_spec_at(board, &dir)
}

/// The path string the generated spec's `board = "..."` line carries: relative
/// from `spec_dir` to the board when both resolve, absolute otherwise. A spec
/// whose board reference is wrong on arrival would fail its very first run,
/// the opposite of "your first spec is an edit".
fn board_reference(board: &Path, spec_dir: &Path) -> String {
    let board_abs = std::fs::canonicalize(board).unwrap_or_else(|_| board.to_path_buf());
    let dir_abs = std::fs::canonicalize(spec_dir).unwrap_or_else(|_| spec_dir.to_path_buf());
    relative_path(&dir_abs, &board_abs).map_or_else(
        || board_abs.display().to_string(),
        |rel| rel.display().to_string(),
    )
}

/// Encode a path as a TOML string. Hand-written basic-string quoting turns a
/// native Windows path such as `..\hardware` into an invalid escape (`\h`).
fn toml_path(path: String) -> String {
    toml::Value::String(path).to_string()
}

/// `to` expressed relative to the directory `from` (both absolute), walking up
/// with `..` where needed. `None` when they share nothing below the root: a
/// different drive, or two trees that only meet at `/`. A board in a separate
/// checkout spelled `../../../../../../../Users/...` is correct but unreadable,
/// and it breaks as soon as the spec moves one level, so the absolute path is
/// the honest reference there.
fn relative_path(from: &Path, to: &Path) -> Option<PathBuf> {
    use std::path::Component;
    let from: Vec<Component> = from.components().collect();
    let to: Vec<Component> = to.components().collect();
    let common = from
        .iter()
        .zip(to.iter())
        .take_while(|(a, b)| a == b)
        .count();
    if from[..common]
        .iter()
        .all(|c| matches!(c, Component::Prefix(_) | Component::RootDir))
    {
        return None;
    }
    let mut rel = PathBuf::new();
    for _ in common..from.len() {
        rel.push("..");
    }
    for c in &to[common..] {
        rel.push(c.as_os_str());
    }
    if rel.as_os_str().is_empty() {
        rel.push(".");
    }
    Some(rel)
}

/// Render the starter spec's TOML text for `board` as it should read when the
/// file lives in `spec_dir`. Split out from [`init_to`] so it can be exercised
/// without touching disk.
///
/// # Errors
///
/// Any error from loading `board` (`runner::load_board`).
pub fn render_spec_at(board: &Path, spec_dir: &Path) -> Result<String, SpecError> {
    render_spec_at_with_run_hint(board, spec_dir, &format!("{}.toml", board_stem(board)))
}

// One linear template: each block writes the next section of the starter spec.
#[allow(clippy::too_many_lines)]
fn render_spec_at_with_run_hint(
    board: &Path,
    spec_dir: &Path,
    run_hint: &str,
) -> Result<String, SpecError> {
    let extracted = runner::load_board(board)?;
    // Same layered library as a run (builtin → packs → user model dirs), so
    // the scaffold detects the MCU/supplies a run would actually bind.
    let lib = ModelLibrary::builtin_with_user_dirs(&[]);
    let bound = bind_board(&extracted, &lib);

    // The board's own detected supplies: the binder stamps one supply leg per
    // rail it found, at that rail's nominal voltage. Reuse them verbatim so the
    // scaffold powers exactly what a run would.
    let scaffolded = scaffold_supplies(
        board,
        &extracted,
        &bound,
        bound
            .supplies
            .iter()
            .map(|leg| (leg.net_name.clone(), leg.supply.nominal_volts()))
            .collect(),
    );
    let supplies: Vec<(String, f64)> = scaffolded
        .iter()
        .filter(|s| s.origin == "rail")
        .map(|s| (s.net.clone(), s.volts))
        .collect();

    // Rail-looking nets, from the shared rail-name helper rather than a fresh
    // heuristic: every non-ground net the binder recognises as a supply rail,
    // deduped and ordered. These become commented voltage assertions.
    let mut rails: Vec<(String, f64)> = extracted
        .nets
        .iter()
        .filter(|n| !is_ground(&n.name))
        .filter_map(|n| Some((n.name.clone(), power_rail_voltage(&n.name)?)))
        .collect();
    rails.sort_by(|a, b| a.0.cmp(&b.0));
    rails.dedup_by(|a, b| a.0 == b.0);

    // Rails the binder found by name but could not put a voltage to. Same
    // detector the run uses, so the scaffold asks about exactly the nets the
    // report would later warn about.
    // A rail a board resistor feeds from a powered node is the board's own
    // answer, not a gap: the run solves it as wired, so no [[supply]] is
    // proposed there (the same rule the run's dead-rail census applies).
    let (unpowered, resistor_fed) = crate::rail_feed::split_dead_and_fed(
        &bound,
        runner::unpowered_supply_nets(&extracted, &bound, &[]),
        &[],
    );

    // Reference rail voltage the boot-coverage "driven high" threshold keys off:
    // the highest detected supply or rail, falling back to 3.3 V.
    let vref = supplies
        .iter()
        .chain(rails.iter())
        .map(|(_, v)| *v)
        .fold(0.0_f64, f64::max);
    let vref = if vref > 0.0 { vref } else { 3.3 };
    let boot_level = round1(vref * 0.7); // a logic-high threshold, one decimal

    // A concrete control net for the boot-coverage assertion: the first
    // non-rail, non-ground signal net (what the firmware is most likely to
    // drive). Power-ish NAMES the rail detector does not price (VSUP_*,
    // VBAT_*, PWR_*) are excluded too: a scaffolded boot/toggle example on a
    // power net reads as nonsense on arrival and teaches the wrong shape.
    // None such -> a named placeholder the user replaces.
    let boot_net = extracted
        .nets
        .iter()
        .map(|n| n.name.as_str())
        .find(|n| {
            !n.is_empty()
                && !is_ground(n)
                && power_rail_voltage(n).is_none()
                && !looks_like_power_name(n)
        })
        .map(str::to_string);

    // Detected MCU (first, if any). The binder's backend string is
    // "<backend>:<kind>"; the spec's `mcu` hint wants just the kind.
    let mcu_backend = bound.mcus.first().map(|m| m.backend.clone());
    let mcu_kind = mcu_backend
        .as_deref()
        .map(|b| b.rsplit(':').next().unwrap_or(b).to_string());

    // Can the detected MCU's backend actually satisfy a boot-coverage assertion?
    // The gate is pin drive DIRECTION: a backend that reports it can tell a
    // held-LOW control net from an undriven one, so its boot-coverage diagnosis
    // is trustworthy. The in-process AVR backend always reports it (DDR hooks),
    // and a `renode:` part does once its SoC descriptor maps every GPIO port's
    // direction register (stm32f103 CRL/CRH, stm32f4 MODER, nrf52840 DIR, see
    // db/mcu/*.soc.toml). The `qemu:` ESP32 family and unmapped Renode parts
    // cannot; on those a scaffolded boot-coverage assertion can go RED with a
    // misleading diagnosis on a net the firmware actually drives LOW (or via an
    // unmodelled peripheral bus), so the assertion is emitted commented-out with
    // an honest note naming the gap and the user opts in deliberately.
    let boot_coverage_supported = mcu_backend
        .as_deref()
        .is_none_or(hauksbee_engine::scheduler::backend_reports_drive_direction);

    let stem = board_stem(board);
    // TOML-encoded, not hand-quoted: a Windows relative path is full of
    // backslashes, and `"..\hardware\b.kicad_pcb"` is not the string it looks
    // like.
    let board_file = toml_path(board_reference(board, spec_dir));
    let docs = hauksbee_ir::docs_url("docs/ci/CI.md");

    let mut s = format!(
        "# hauksbee-ci starter spec, generated by `hauksbee-ci init`.\n\
         # Every line is commented with what it does. Uncomment and tune, then run:\n\
         #   hauksbee-ci run {run_hint}\n\
         # The board, MCU, supplies and rails below were detected from the board.\n\
         # Full assertion catalog ({docs}): voltage, uart, toggle, no_faults,\n\
         #   max_current, max_temp, rail_window, protection_trip, boot_coverage,\n\
         #   phase_margin, ac_gain, peripheral, hwtrace, model_coverage.\n\
         \n\
         name = \"{stem} power-up\"        # label shown in reports\n\
         board = {board_file}          # the design file this spec checks\n\
         # bom = \"hardware/bom.csv\"        # exact purchased-part identity (optional)\n\
         # bom_columns = [\"reference=Customer Reference\"] # confirm an ambiguous header\n\
         # placement = \"hardware/positions.csv\" # assembly position/side cross-check (optional)\n\
         # variant = \"hardware/prototype.variant.toml\" # explicit fit/no-fit assembly (optional)\n"
    );

    // MCU + firmware placeholder. The informational value is what the BOARD
    // says (the part value), not the coarse backend family it collapses to:
    // printing "atmega328p" for an RP2040 board because of a family fallback
    // would teach the user a falsehood on line three of their first spec.
    // When the modelled core differs from the requested part, both are named.
    let requested = bound
        .mcus
        .first()
        .map(|m| m.requested_part.as_str())
        .filter(|r| !r.is_empty());
    match (&mcu_kind, requested) {
        (Some(kind), Some(requested)) if !requested.eq_ignore_ascii_case(kind) => {
            let _ = writeln!(s, "mcu = \"{requested}\"                # board's MCU (informational); co-simmed on the {kind} core");
        }
        (Some(kind), _) => {
            let _ = writeln!(s, "mcu = \"{kind}\"                # detected MCU (informational; the binder auto-detects)");
        }
        (None, _) => {
            s.push_str("# mcu = \"atmega328p\"          # no MCU detected (informational note only; the binder detects the MCU from the board)\n");
        }
    }
    s.push_str(
        "# firmware = \"firmware/build/app.elf\"   # ELF/hex to boot on the MCU (co-sim your firmware)\n\
         duration_ms = 200               # simulated time to run\n\
         \n\
         # Supplies: power the rails the board expects. kind is one of\n\
         # ideal | bench | wall | usb | battery.\n\
         # Each detected rail is scaffolded as a behavioral source (a bench PSU with a\n\
         # current limit, or a USB port with its droop), never as kind = \"ideal\", so\n\
         # that the rail assertions further down are gates rather than decoration: an\n\
         # ideal source pins its net to `volts` whatever the board draws, so a voltage\n\
         # or rail_window check on that net cannot fail for a board reason and its green\n\
         # vouches for nothing. A bench PSU folds back at its limit and a USB port sags\n\
         # across its cable, so the rail can move and the check can fire.\n",
    );
    // Supplies (enabled): one leg per detected rail, modelled BEHAVIORALLY.
    if scaffolded.is_empty() {
        s.push_str(
            "# No supply rail was detected; add one the board is fed from:\n\
             # [[supply]]\n# net = \"+5V\"\n# kind = \"bench\"\n# volts = 5.0\n# current_limit_a = 2.0\n",
        );
    }
    for supply in &scaffolded {
        s.push_str(&supply.toml_block(true));
    }
    // Rails whose name says "supply" and nothing else. Nobody can read a voltage
    // off VDD_A or bare VDD, so the binder refuses to invent one and the net
    // sits at 0 V. Leaving that for the user to discover means their first run
    // solves a board with a rail dead and reports whatever that implies. Only the
    // person who drew the schematic knows the number, so the scaffold asks rather
    // than guessing, and puts the question where they are already editing.
    if !unpowered.is_empty() {
        s.push_str(
            "\n# These nets name a supply but not a voltage, so nothing can work out\n\
             # what to feed them and they will sit at 0 V. Fill in each one and\n\
             # uncomment it, or every analog result is solved around a dead rail.\n",
        );
        for net in &unpowered {
            if looks_like_battery_net(net) {
                // A cell holder is a source with a chemistry, not an ideal
                // number: the pack sags under load and walks down with charge,
                // which is what a battery-powered board's boot assertions test.
                let _ = writeln!(
                    s,
                    "# [[supply]]\n# net = \"{net}\"\n# kind = \"battery\"              # the name says a cell feeds this net\n\
                     # chemistry = \"liion\"         # liion | alkaline | nimh | lifepo4\n\
                     # cells = 1                    # cells in series\n\
                     # capacity_mah = 2000"
                );
            } else {
                let _ = writeln!(
                    s,
                    "# [[supply]]\n# net = \"{net}\"\n# kind = \"bench\"\n\
                     # volts =                     # what does this rail run at?\n\
                     # current_limit_a = {BENCH_LIMIT_A:.1}"
                );
            }
        }
    }

    // Say why a supply-named rail got no question, so its absence is not a gap.
    if !resistor_fed.is_empty() {
        s.push_str(
            "\n# These supply-named nets are fed by the board itself, so no supply is proposed:\n",
        );
        for fed in &resistor_fed {
            let _ = writeln!(s, "# {}", fed.reason());
        }
    }

    // Assertions. no_faults is the one live gate. boot_coverage is always
    // scaffolded COMMENTED-OUT: it asserts on what the *firmware* does, and
    // `firmware = ...` is itself commented above, so the starter spec has no
    // image to boot. Left live it would go RED out of the box on every board
    // (the control net is never driven / the MCU never runs), handing the user
    // a false red on their very first run. So the starter is GREEN on
    // no_faults alone; the user opts into boot-coverage deliberately, after
    // wiring up their firmware.
    s.push_str(
        "\n# Assertions: at least one must hold for the build to go green.\n\
         \n\
         # no_faults: the stress monitor raised no over-current / over-voltage /\n\
         # over-power / reverse-bias / over-temperature fault across the run.\n\
         [[assert]]\nkind = \"no_faults\"\n\
         \n\
         # boot_coverage: a control net (a gate / enable / reset / chip-select) the\n\
         # firmware must actively drive to a defined level within a deadline of reset,\n\
         # with no stress fault during the boot window before it does.\n\
         # NOTE: left commented-out. It boots your firmware and checks the control net,\n\
         #   so it only means something once `firmware = ...` above points at a real\n\
         #   ELF/hex. Uncomment both the firmware line and this block together.\n",
    );
    if !boot_coverage_supported {
        let _ = writeln!(
            s,
            "#   Also note this board's MCU runs on the `{}` backend. It co-sims\n\
             #   GPIO and UART ({}), but its platform has no verified direction-\n\
             #   register map, so it cannot report pin drive DIRECTION and cannot\n\
             #   distinguish a held-LOW pin from an undriven one. On it, watch only a net\n\
             #   driven by plain GPIO to a defined HIGH level. AVR boards and the\n\
             #   direction-mapped Renode parts (STM32F103/F4, nRF52840) can watch a\n\
             #   held-LOW net too.",
            mcu_backend.as_deref().unwrap_or(""),
            hauksbee_ir::docs_url("docs/cosim/MCU.md")
        );
    }
    // Two over-long literal arms read better as a match than as map_or_else closures.
    #[allow(clippy::option_if_let_else)]
    let boot_net_line = match &boot_net {
        Some(net) => format!("# net = \"{net}\"                  # control net to watch (edit to your gate/enable/reset/CS)"),
        None => "# net = \"CONTROL_NET\"            # no signal net detected; set this to a real control net".to_string(),
    };
    let _ = writeln!(
        s,
        "# [[assert]]\n# kind = \"boot_coverage\"\n{boot_net_line}\n\
         # min = {}                    # driven level (V) the firmware must reach\n\
         # deadline_ms = 100.0             # by this long after reset\n",
        fmt1(boot_level)
    );

    // Commented voltage assertions on the rails. A rail the BOARD derives (a
    // regulator output, whatever hangs off the input rail) is the strongest gate
    // the scaffold can offer, because every part between the source and that net
    // is under test, so those come first and are named as such. The rails fed
    // straight from a declared supply leg come after; they gate too, but only
    // because the supply above is behavioral.
    let (derived, source_fed): (Vec<_>, Vec<_>) = rails
        .iter()
        .partition(|(net, _)| !scaffolded.iter().any(|s| &s.net == net));
    s.push_str(
        "# voltage: a rail stays within bounds (min = worst dip, max = worst rise).\n\
         # Uncomment the rails you want gated and tune the tolerance.\n",
    );
    if rails.is_empty() {
        s.push_str(
            "# [[assert]]\n# kind = \"voltage\"\n# net = \"+5V\"\n# min = 4.75\n# max = 5.25\n",
        );
    }
    if !derived.is_empty() {
        s.push_str(
            "# These rails the board makes for itself, so a check on them measures the\n\
             # regulator, the filtering and the load between the input and the net:\n",
        );
    }
    let noted = (derived.iter().map(|r| (r, "derived by the board")))
        .chain(source_fed.iter().map(|r| (r, "fed by the supply above")));
    for ((net, v), note) in noted {
        let _ = writeln!(
            s,
            "# [[assert]]\n# kind = \"voltage\"\n\
             # net = \"{net}\"                  # rail detected at ~{} V, {note}\n\
             # min = {}\n# max = {}\n\
             # after_ms = 50                  # only sample once it has settled",
            fmt1(*v),
            fmt2(v * 0.95),
            fmt2(v * 1.05)
        );
    }

    // Transient / brownout on-ramp: scaffold a [[profile]] + [[scenario]] +
    // rail_window whenever a supply rail was detected. This is the ONLY place a
    // user discovers dynamic analysis from `hauksbee run`; there is no `run
    // --transient` flag; brownout/inrush lives here in the spec. Commented so
    // the starter stays GREEN on no_faults alone; the user opts in and tunes the
    // load profile to their board. See docs/checks/TRANSIENTS.md. `part` is
    // required by the scenario loader (the load attaches to a component); it
    // is scaffolded with the detected MCU so uncommenting the block as written
    // always parses. The floor is derived from THIS rail's own detected
    // voltage (-5%), never from the board-wide reference: a 4.5 V floor
    // scaffolded onto a 1.1 V core rail is wrong on arrival.
    if let Some((rail, rail_v)) = supplies.first() {
        let scenario_part = bound.mcus.first().map_or("U1", |m| m.reference.as_str());
        let _ = writeln!(
            s,
            "\n# rail_window: does the rail stay up under a dynamic load step (a WiFi\n\
             # burst, a motor kick, an inrush)? This is the transient/brownout check:\n\
             # there is NO `run --transient` flag; it lives here. A [[profile]] shapes\n\
             # the load current, a [[scenario]] attaches it to a supply net, and the\n\
             # rail_window assert bounds the rail while it runs. See {}.\n\
             # [[profile]]\n# id = \"load_step\"\n# [[profile.segment]]\n\
             # level_a = 0.05                 # baseline current (A)\n\
             # rise_s = 0.001\n# duration_s = 0.0\n# [[profile.segment]]\n\
             # level_a = 0.5                  # the step / burst current (A); tune to your load\n\
             # rise_s = 0.0005\n# duration_s = 0.010\n# period_s = 0.100\n# idle_a = 0.05\n#\n\
             # [[scenario]]\n# id = \"step\"\n\
             # part = \"{scenario_part}\"                   # the component drawing the load\n\
             # profile = \"load_step\"\n\
             # supply_net = \"{rail}\"          # detected rail the load hangs off\n\
             # start_ms = 1.0\n#\n\
             # [[assert]]\n# kind = \"rail_window\"\n# scenario = \"step\"\n# net = \"{rail}\"\n\
             # min = {}                    # 5% below the rail's detected {} V; tune to your budget",
            hauksbee_ir::docs_url("docs/checks/TRANSIENTS.md"),
            fmt2(rail_v * 0.95),
            fmt1(*rail_v)
        );
    }

    // Firmware-behaviour assertions; the tool's headline pitch ("assert the UART
    // says hello, assert the LED blinks"). Only meaningful once `firmware = ...`
    // above points at a real image, so scaffold them COMMENTED and only when an
    // MCU was detected; uncomment them together with the firmware line.
    if mcu_backend.is_some() {
        let _ = writeln!(
            s,
            "\n# uart: the firmware's serial output contains a string or matches a regex.\n\
             #   (needs `firmware = ...` above; the tool boots the image and reads the UART.)\n\
             # [[assert]]\n# kind = \"uart\"\n\
             # contains = \"hello\"             # a boot banner / heartbeat your firmware prints\n\
             \n\
             # toggle: a net toggles at an expected rate, a blink / clock / PWM check.\n\
             # [[assert]]\n# kind = \"toggle\"\n\
             # net = \"{}\"                  # the blinking / clocked net (edit to yours)\n\
             # freq_hz = 1.0                  # expected toggle rate (Hz)\n\
             # tolerance = 0.2                # +/-20%",
            boot_net.as_deref().unwrap_or("LED")
        );
    }

    write_indicator_outcomes(
        &mut s,
        &hauksbee_engine::checks::indicators::indicator_traces(&extracted, &lib),
        vref,
    );
    Ok(s)
}

/// One commented outcome per logic-driven indicator. Its meaning (which level
/// of the source signal should light it) is the designer's to state: no check
/// can tell an inverted indicator from an intended one, so the scaffold asks,
/// with the polarity left for the user to confirm.
fn write_indicator_outcomes(
    s: &mut String,
    traces: &[hauksbee_engine::checks::indicators::IndicatorTrace],
    vref: f64,
) {
    if traces.is_empty() {
        return;
    }
    s.push_str(
        "\n# Indicator outcomes: what each LED MEANS is not checked unless you state it.\n\
         # Each LED below is lit from a logic signal; an active-low signal through a\n\
         # buffer (or an active-high one through an inverter) lights it on the wrong\n\
         # state while every voltage stays in range. Sweep the source net and say\n\
         # which way the LED's own net must move. Uncomment the [search] line once.\n\
         # [search]\n",
    );
    let mut swept = std::collections::BTreeSet::new();
    for t in traces {
        let source = &t.source_net;
        let lit_high = !t.sinks.unwrap_or(false);
        // Asserted low and lit high: the LED net falls as the source rises.
        let kind = if t.source_active_low == lit_high {
            "decreases"
        } else {
            "increases"
        };
        let other = if kind == "decreases" {
            "increases"
        } else {
            "decreases"
        };
        let polarity = if t.source_active_low {
            format!("the name suggests {source} is active-low (asserted at 0 V)")
        } else {
            format!("the name gives no polarity; this assumes {source} is active-high")
        };
        let path = if t.stages.is_empty() {
            format!("driven by {}", t.driver)
        } else {
            let stages: Vec<String> = t
                .stages
                .iter()
                .rev()
                .map(|st| {
                    format!(
                        "{} {}",
                        st.reference,
                        if st.inverting { "inverter" } else { "buffer" }
                    )
                })
                .collect();
            format!("via {}", stages.join(" -> "))
        };
        let _ = writeln!(
            s,
            "#\n# LED {led} should be lit when {source} is asserted; confirm whether {source} is active-low.\n\
             #   {led} hangs off {net} ({path}); {polarity}.\n\
             #   If it is not, change kind to \"{other}\".",
            led = t.indicator,
            net = t.indicator_net,
        );
        if swept.insert(source.clone()) {
            let _ = writeln!(
                s,
                "# [[search.sweep]]\n# net = {}\n# volts = [0.0, {}]",
                toml_path(source.clone()),
                fmt1(vref)
            );
        }
        let _ = writeln!(
            s,
            "# [[assert]]\n# name = {}\n# kind = \"{kind}\"\n# net = {}\n# along = {}",
            toml_path(format!("{} lit when {source} is asserted", t.indicator)),
            toml_path(t.indicator_net.clone()),
            toml_path(source.clone()),
        );
    }
}

/// Round to one decimal place.
fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

/// Format a voltage with one decimal place ("5.0", "3.3"), so TOML always reads
/// as a float rather than an int the user might mistake for a count.
fn fmt1(v: f64) -> String {
    format!("{v:.1}")
}

/// Format a voltage bound with two decimals ("4.75", "5.25").
fn fmt2(v: f64) -> String {
    format!("{v:.2}")
}

#[cfg(test)]
mod seeded_volts_tests {
    use super::seeded_input_volts;

    #[test]
    fn a_whole_volt_minimum_is_seeded_above_it_not_on_it() {
        // Faulty case: a 7 V minimum seeded at exactly 7 V leaves the run
        // unable to judge the regulator, so the draft gets 8 V.
        assert!((seeded_input_volts(Some(7.0)) - 8.0).abs() < f64::EPSILON);
        // Legal controls: a fractional minimum rounds up to the next volt,
        // and no figure falls back to a 9 V adapter.
        assert!((seeded_input_volts(Some(6.3)) - 7.0).abs() < f64::EPSILON);
        assert!((seeded_input_volts(None) - 9.0).abs() < f64::EPSILON);
    }
}

#[cfg(test)]
mod relative_path_tests {
    use super::{relative_path, toml_path};
    use std::path::{Path, PathBuf};

    #[test]
    fn sibling_parent_and_nested_shapes_all_resolve() {
        let rel = |from: &str, to: &str| relative_path(Path::new(from), Path::new(to));
        assert_eq!(
            rel("/repo/ci", "/repo/hardware/board.kicad_pcb"),
            Some(PathBuf::from("../hardware/board.kicad_pcb"))
        );
        assert_eq!(
            rel("/repo", "/repo/hardware/board.kicad_pcb"),
            Some(PathBuf::from("hardware/board.kicad_pcb"))
        );
        assert_eq!(
            rel("/repo/a/b", "/repo/board.kicad_pcb"),
            Some(PathBuf::from("../../board.kicad_pcb"))
        );
        // Same directory: the bare file name.
        assert_eq!(
            rel("/repo", "/repo/board.kicad_pcb"),
            Some(PathBuf::from("board.kicad_pcb"))
        );
    }

    #[test]
    fn trees_that_only_meet_at_the_root_stay_absolute() {
        let rel = |from: &str, to: &str| relative_path(Path::new(from), Path::new(to));
        assert_eq!(rel("/tmp/specs", "/Users/me/boards/board.brd"), None);
        assert_eq!(rel("/", "/board.brd"), None);
    }

    #[test]
    fn windows_separators_are_escaped_for_toml() {
        let encoded = toml_path(r"..\hardware\blinky.kicad_pcb".to_string());
        let parsed: toml::Value = toml::from_str(&format!("board = {encoded}")).unwrap();
        assert_eq!(
            parsed["board"].as_str(),
            Some(r"..\hardware\blinky.kicad_pcb")
        );
    }
}
