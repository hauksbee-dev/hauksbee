//! `hauksbee sim <deck.cir>`: load a SPICE deck, run `.op`/`.tran`/`.ac`/`.dc`,
//! and write CSV, an ngspice ASCII rawfile, or both. A malformed deck exits 2;
//! a well-formed deck the solver cannot honestly answer exits 3.

use crate::result::{Refusal, EXIT_INVALID_FOR_ANALYSIS};
use hauksbee_ir::{AcDirective, AcSweep, Circuit, DcSweep, Directives, SpiceLoader};
use hauksbee_solve::{
    default_probes, run_ac, run_dc, run_op, run_tran, write_ascii_rawfile, DcInit, Integration,
    Probe, RawPlot, SimOutput, SolverOptions, StepControl,
};
use std::path::{Path, PathBuf};

/// Exit code for a malformed deck (the loader rejected it).
pub const EXIT_MALFORMED_DECK: i32 = 2;

/// Why a deck could not be simulated. The CLI renders these as its two exit
/// codes; a caller with no terminal (the browser deck panel) reads
/// [`SimError::message`] instead, so neither front end invents its own wording.
enum SimError {
    /// The deck is malformed or the request misuses it (exit 2).
    Malformed(String),
    /// Well-formed, but the solver cannot answer it honestly (exit 3).
    Refused {
        claim: &'static str,
        missing: String,
        next_action: &'static str,
    },
}

impl SimError {
    fn malformed(msg: impl std::fmt::Display) -> SimError {
        SimError::Malformed(msg.to_string())
    }

    fn refused(claim: &'static str, missing: impl Into<String>, next_action: &'static str) -> Self {
        SimError::Refused {
            claim,
            missing: missing.into(),
            next_action,
        }
    }

    /// One sentence, for a surface that cannot print a multi-line refusal.
    fn message(self) -> String {
        match self {
            SimError::Malformed(msg) => msg,
            SimError::Refused {
                claim,
                missing,
                next_action,
            } => format!("{claim} refused: {missing}. {next_action}"),
        }
    }

    /// Print the CLI's diagnostic and exit with the code it earns.
    fn exit(self) -> ! {
        let code = match self {
            SimError::Malformed(msg) => {
                eprintln!("error: {msg}");
                EXIT_MALFORMED_DECK
            }
            SimError::Refused {
                claim,
                missing,
                next_action,
            } => {
                let refusal = Refusal::new(
                    claim,
                    missing,
                    vec!["the deck parsed and the circuit was assembled"],
                    next_action,
                );
                eprintln!("error: {claim} refused: {}", refusal.missing_prerequisite);
                eprintln!("{}", refusal.render_text());
                EXIT_INVALID_FOR_ANALYSIS
            }
        };
        std::process::exit(code);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum SimFormat {
    /// One column per probe, one row per timepoint (or one row for `.op`).
    Csv,
    /// ngspice ASCII rawfile; the format `ngnutmeg`/`gaw`/`spicelib` read.
    Raw,
    /// CSV and rawfile side by side (needs `--out` so the two files have names).
    Both,
}

#[derive(Clone, Copy)]
enum Analysis {
    Op,
    Tran,
    Ac,
    Dc,
}

/// `hauksbee sim`: load a `.cir`, run the chosen analysis, write the results.
#[allow(clippy::too_many_arguments)]
pub fn run(
    file: &Path,
    out: Option<&Path>,
    format: SimFormat,
    op: bool,
    tran: bool,
    ac: bool,
    dc: bool,
    print: &[String],
) -> anyhow::Result<()> {
    let bytes = std::fs::read(file).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            // Only suggest a command that can run from where the user is.
            let suggestion = if Path::new("examples/decks/rlc_ringdown.cir").exists() {
                "hauksbee sim examples/decks/rlc_ringdown.cir --tran --print V(out)"
            } else {
                "hauksbee sim --example rlc_ringdown --tran --print V(out)"
            };
            anyhow::anyhow!(
                "no deck at '{}'. Check the path, or try a bundled example:\n  {suggestion}",
                file.display()
            )
        } else {
            anyhow::anyhow!("reading '{}': {e}", file.display())
        }
    })?;
    let text = decode_deck_text(bytes).unwrap_or_else(|why| {
        SimError::malformed(format!(
            "'{}' is not a text file ({why}); hauksbee sim expects a text SPICE deck (.cir)",
            file.display()
        ))
        .exit()
    });
    let (circuit, directives) = SpiceLoader::load_file_with_directives(file)
        .unwrap_or_else(|e| SimError::malformed(e).exit());
    if circuit.devices.is_empty() {
        SimError::malformed(format!(
            "deck has no circuit elements: '{}' parses but contains no devices \
             (R/C/L/V/I/D/Q/M...), so there is nothing to simulate.",
            file.display()
        ))
        .exit();
    }
    if matches!(format, SimFormat::Both) && out.is_none() {
        SimError::malformed(
            "--format both writes a CSV and a rawfile side by side, so it needs \
             --out <FILE> to name them (e.g. --out results.csv writes results.csv and \
             results.raw). Use --format csv or --format raw to print a single format to stdout.",
        )
        .exit();
    }
    // The deck's title line (SPICE convention: line 1) becomes the rawfile `Title:`.
    let title = text
        .lines()
        .next()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("hauksbee sim");

    // An explicit flag wins; otherwise `simulate` reads the deck's own card.
    let forced = match (op, tran, ac, dc) {
        (true, ..) => Some(Analysis::Op),
        (_, true, ..) => Some(Analysis::Tran),
        (_, _, true, _) => Some(Analysis::Ac),
        (.., true) => Some(Analysis::Dc),
        _ => None,
    };
    let mut notes: Vec<String> = directives.notes.clone();
    let solved = simulate(&circuit, &directives, print, forced, &mut notes);
    // Notes print only once every fatal pre-solve check has passed; a refusal
    // is past them, a malformed deck or probe never got there.
    if !matches!(solved, Err(SimError::Malformed(_))) {
        for note in &notes {
            eprintln!("{note}");
        }
    }
    let (tag, output) = solved.unwrap_or_else(|e| e.exit());

    let plot = match tag {
        "tran" => RawPlot::Transient,
        "dc" => RawPlot::Dc,
        "ac" => RawPlot::Ac,
        _ => RawPlot::OperatingPoint,
    };
    // `--out X`: raw goes to X if it ends `.raw`, else X-with-`.raw`; `both`
    // additionally writes the CSV to X (or X-with-`.csv` if X ends `.raw`).
    let with_ext = |base: &Path, ext: &str| -> PathBuf {
        if base.extension().and_then(|e| e.to_str()) == Some(ext) {
            base.to_path_buf()
        } else {
            base.with_extension(ext)
        }
    };
    let write = |path: &Path, body: String| -> anyhow::Result<()> {
        std::fs::write(path, body)
            .map_err(|e| anyhow::anyhow!("writing '{}': {e}", path.display()))?;
        eprintln!(
            "wrote {} row(s) x {} column(s) to {}",
            output.rows.len(),
            output.columns.len() + usize::from(output.time.is_some()),
            path.display()
        );
        Ok(())
    };
    match (format, out) {
        (SimFormat::Csv, None) => print!("{}", sim_output_to_csv(&output)),
        (SimFormat::Csv, Some(path)) => write(path, sim_output_to_csv(&output))?,
        (SimFormat::Raw, None) => print!("{}", write_ascii_rawfile(&output, plot, title)),
        (SimFormat::Raw, Some(base)) => write(
            &with_ext(base, "raw"),
            write_ascii_rawfile(&output, plot, title),
        )?,
        (SimFormat::Both, base) => {
            let base = base.expect("--format both requires --out (refused above)");
            write(&with_ext(base, "csv"), sim_output_to_csv(&output))?;
            write(
                &with_ext(base, "raw"),
                write_ascii_rawfile(&output, plot, title),
            )?;
        }
    }
    Ok(())
}

/// One deck simulated: what ran, the numbers, and the caveats `hauksbee sim`
/// prints on stderr. The browser deck panel shows the same three, so a warning
/// cannot reach one front end and not the other.
#[derive(Debug)]
pub struct DeckRun {
    /// `"op"`, `"tran"`, `"dc"` or `"ac"`: the analysis the deck asked for.
    pub analysis: &'static str,
    pub output: SimOutput,
    pub notes: Vec<String>,
}

/// Simulate a deck supplied as text: the browser upload path, and the pure
/// core `hauksbee sim` runs after it has read the file. No process exit, no
/// stderr, no file IO, so the two front ends cannot drift.
pub fn simulate_deck(text: &str, print: &[String]) -> Result<DeckRun, String> {
    // `.include`/`.lib` resolve against the deck's own directory, which does
    // not exist on this side of an upload. Refusing is the honest answer; a
    // silent partial parse would simulate a different circuit.
    if let Some(card) = text.lines().map(str::trim).find(|line| {
        let head = line.split_whitespace().next().unwrap_or("");
        [".include", ".inc", ".lib"]
            .iter()
            .any(|d| head.eq_ignore_ascii_case(d))
    }) {
        return Err(format!(
            "this deck pulls in another file (`{card}`), which exists only on the machine that \
             wrote it. Upload a self-contained deck, or run `hauksbee sim <deck.cir>` locally \
             where the included file is."
        ));
    }
    let (circuit, directives) =
        SpiceLoader::load_with_directives(text).map_err(|e| e.to_string())?;
    if circuit.devices.is_empty() {
        return Err(
            "deck has no circuit elements: it parses but contains no devices \
                    (R/C/L/V/I/D/Q/M...), so there is nothing to simulate."
                .to_string(),
        );
    }
    within_point_budget(&directives)?;
    let mut notes = directives.notes.clone();
    let (analysis, output) =
        simulate(&circuit, &directives, print, None, &mut notes).map_err(SimError::message)?;
    Ok(DeckRun {
        analysis,
        output,
        notes,
    })
}

/// Most solved points one uploaded deck may ask for. It bounds how many steps
/// a deck asks for, never what one costs; [`DECK_TIME_LIMIT`] bounds the rest.
const MAX_DECK_POINTS: f64 = 200_000.0;

/// Longest a deck uploaded to the web app may spend solving; the CLI is uncapped.
pub const DECK_TIME_LIMIT: std::time::Duration = std::time::Duration::from_secs(60);

/// [`simulate_deck`] under a wall-clock limit, for the browser deck panel. The
/// solver has no cancel hook, so an over-running solve is left on its own
/// thread to finish (the point budget bounds it); what this bounds is how long
/// a request may wait, which is what ties the server up.
pub fn simulate_deck_within(
    text: &str,
    print: &[String],
    limit: std::time::Duration,
) -> Result<DeckRun, String> {
    let (text, print) = (text.to_string(), print.to_vec());
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || tx.send(simulate_deck(&text, &print)));
    rx.recv_timeout(limit).unwrap_or_else(|_| {
        Err(format!(
            "this deck was still solving after {limit:?}, which is longer than a deck run in \
             the browser is given. Shorten the stop time or widen the step, or run it locally \
             with `hauksbee sim <deck.cir>`."
        ))
    })
}

/// Refuse, before solving, a deck whose cards imply more than [`MAX_DECK_POINTS`].
fn within_point_budget(d: &Directives) -> Result<(), String> {
    let sweep_points = |s: &DcSweep| ((s.stop - s.start) / s.step).abs() + 1.0;
    let points = match (&d.tran, &d.ac, &d.dc) {
        (Some(t), ..) if t.tstep > 0.0 => (t.tstop - t.tstart) / t.tstep,
        (_, Some(a), _) => ac_points(a),
        (_, _, Some(c)) => sweep_points(&c.inner) * c.outer.as_ref().map_or(1.0, sweep_points),
        _ => 0.0,
    };
    if points.is_nan() || points > MAX_DECK_POINTS {
        return Err(format!(
            "this deck asks for about {points:.0} solved points; a deck run in the browser is \
             capped at {MAX_DECK_POINTS:.0}. Widen the step (or narrow the sweep), or run it \
             locally with `hauksbee sim <deck.cir>`."
        ));
    }
    Ok(())
}

/// Frequency points a `.ac` card asks for: `points` is per decade/octave for a
/// log sweep, and the whole sweep for a linear one.
fn ac_points(a: &AcDirective) -> f64 {
    let decades = (a.fstop / a.fstart).log10();
    #[allow(clippy::cast_precision_loss)] // a point count never reaches 2^53.
    let per = a.points as f64;
    match a.sweep {
        AcSweep::Decade => per * decades,
        AcSweep::Octave => per * decades / 2f64.log10(),
        AcSweep::Linear => per,
    }
}

/// Run one already-loaded deck: pick the analysis (`forced` is the CLI's
/// explicit `--op`/`--tran`/`--ac`/`--dc`, else the deck's own card), resolve
/// and validate the probes, solve. Caveats land in `notes`, which the caller
/// shows however its surface shows them.
fn simulate(
    circuit: &Circuit,
    directives: &Directives,
    print: &[String],
    forced: Option<Analysis>,
    notes: &mut Vec<String>,
) -> Result<(&'static str, SimOutput), SimError> {
    // Four printed digits read as a measurement unless the run says what they
    // rest on, so every deck run states its basis whether or not it has caveats.
    notes.push(
        "note: results come solely from the deck's own model cards at nominal values; no \
         tolerance, temperature or parasitic spread is applied, so they carry no uncertainty \
         interval."
            .into(),
    );
    // The pole sets the small-signal rate; only `slew` bounds a large step. A
    // card with a pole and no slew therefore edges faster than any real part.
    if circuit.devices.iter().any(|d| {
        matches!(d, hauksbee_ir::Device::OpAmp { pole_hz: Some(p), slew, .. }
            if p.is_finite() && *p > 0.0 && !slew.is_some_and(|s| s > 0.0))
    }) {
        notes.push(
            "note: an op-amp card sets `pole_hz` but no `slew` (V/us), so large steps are not \
             slew-limited and edge faster than the real part; small-signal results are unaffected."
                .into(),
        );
    }
    let analysis = forced.unwrap_or(if directives.tran.is_some() {
        Analysis::Tran
    } else if directives.dc.is_some() {
        Analysis::Dc
    } else if directives.ac.is_some() {
        Analysis::Ac
    } else {
        Analysis::Op
    });
    let tag = match analysis {
        Analysis::Op => "op",
        Analysis::Tran => "tran",
        Analysis::Ac => "ac",
        Analysis::Dc => "dc",
    };

    // Probes: `--print` wins; else the deck's `.print`/`.plot` cards for this
    // analysis; else every node voltage.
    let parse_probes = |toks: &[String], what: &str| -> Result<Vec<Probe>, SimError> {
        toks.iter()
            .map(|t| Probe::parse(t).map_err(|e| SimError::malformed(format!("{what}: {e}"))))
            .collect()
    };
    let probes: Vec<Probe> = if print.is_empty() {
        let deck_vars: Vec<String> = directives
            .prints
            .iter()
            .filter(|pr| pr.analysis.is_empty() || pr.analysis == tag)
            .flat_map(|pr| pr.vars.iter().cloned())
            .collect();
        if directives.saw_plot {
            notes.push(
                "note: `.plot` cards are treated as `.print` (CSV output; no ASCII plot).".into(),
            );
        }
        if deck_vars.is_empty() {
            notes.push(format!(
                "note: no --print and no matching `.print {tag}` card; writing every node voltage."
            ));
            default_probes(circuit)
        } else {
            parse_probes(&deck_vars, &format!("`.print {tag}` output variable"))?
        }
    } else {
        parse_probes(print, "--print")?
    };
    // A mistyped probe is user misuse (exit 2), never a solver "non-convergence".
    for p in &probes {
        let missing_node = |n: &String| circuit.find_node(n).is_none();
        let bad = match p {
            Probe::NodeVoltage(a) if missing_node(a) => {
                Some(format!("V({a}): no node named '{a}'"))
            }
            Probe::NodeDiff(a, b) => [a, b]
                .into_iter()
                .find(|n| missing_node(n))
                .map(|n| format!("V({a},{b}): no node named '{n}'")),
            Probe::BranchCurrent(d)
                if !circuit
                    .devices
                    .iter()
                    .any(|dev| dev.name().eq_ignore_ascii_case(d)) =>
            {
                Some(format!("I({d}): no element named '{d}'"))
            }
            _ => None,
        };
        if let Some(why) = bad {
            let known: Vec<&str> = circuit.node_names().collect();
            return Err(SimError::malformed(format!(
                "invalid probe: {why} (known nodes: {})",
                known.join(", ")
            )));
        }
    }

    let mut opts = solver_opts_from_deck(circuit, directives);
    let output: SimOutput = match analysis {
        Analysis::Op => run_op(circuit, &opts, &probes).map_err(|msg| {
            SimError::refused(
                "DC operating-point analysis",
                format!("the DC operating point did not converge: {msg}"),
                "inspect the named non-convergent node/device, correct its model or bias path, then run it again",
            )
        })?,
        Analysis::Tran => {
            let Some(td) = directives.tran else {
                return Err(SimError::refused(
                    "transient analysis",
                    "the deck has no `.tran` card, so there is no stop time or step to run",
                    "add `.tran <tstep> <tstop>` to the deck, then run it again",
                ));
            };
            opts.integration = Integration::Trapezoidal;
            opts.step = StepControl::Adaptive {
                dt_initial: (td.tstep / 100.0).max(1e-15),
                dt_min: 1e-15,
                dt_max: td.tmax.unwrap_or(td.tstep).max(1e-15),
            };
            if directives.use_initial_conditions {
                opts.dc_init = DcInit::FromZero;
            }
            run_tran(circuit, &opts, td.tstop, &probes).map_err(|msg| {
                SimError::refused(
                    "transient analysis",
                    format!("the transient solve failed: {msg}"),
                    "inspect the named failed timestep/node, correct its model or timestep constraints, then run it again",
                )
            })?
        }
        Analysis::Dc => {
            let Some(card) = &directives.dc else {
                return Err(SimError::refused(
                    "DC sweep",
                    "the deck has no `.dc` card, so there is no source or range to sweep",
                    "add `.dc <src> <start> <stop> <step>` to the deck, then run it again",
                ));
            };
            run_dc(circuit, &opts, card, &probes).map_err(|msg| {
                SimError::refused(
                    "DC sweep",
                    format!("a DC sweep point did not converge: {msg}"),
                    "inspect the named failed sweep point/device, correct its model or range, then run it again",
                )
            })?
        }
        Analysis::Ac => {
            let Some(card) = &directives.ac else {
                return Err(SimError::refused(
                    "AC analysis",
                    "the deck has no `.ac` card, so there is no frequency sweep to run",
                    "add `.ac <dec|oct|lin> <n> <fstart> <fstop>` to the deck, then run it again",
                ));
            };
            run_ac(circuit, &opts, card, &probes).map_err(|msg| {
                SimError::refused(
                    "AC analysis",
                    msg.to_string(),
                    "add `AC 1` to the driving source in the deck, then run it again",
                )
            })?
        }
    };
    Ok((tag, output))
}

/// Decode deck bytes as text, refusing binary input (invalid UTF-8, or NUL
/// bytes, which are valid UTF-8 but never a text netlist).
fn decode_deck_text(bytes: Vec<u8>) -> Result<String, &'static str> {
    let text = String::from_utf8(bytes).map_err(|_| "invalid UTF-8 bytes")?;
    if text.contains('\0') {
        return Err("contains NUL bytes");
    }
    Ok(text)
}

/// RFC-4180 escape a CSV field (a differential probe label `V(out,ref)`
/// carries a comma).
pub fn csv_escape(field: &str) -> std::borrow::Cow<'_, str> {
    if field.contains([',', '"', '\n', '\r']) {
        std::borrow::Cow::Owned(format!("\"{}\"", field.replace('"', "\"\"")))
    } else {
        std::borrow::Cow::Borrowed(field)
    }
}

/// Render a [`SimOutput`] as CSV; a transient prepends a `time_s` column.
/// The browser deck panel downloads exactly these bytes, so there is one CSV
/// writer and no second implementation to drift from it.
pub fn sim_output_to_csv(o: &SimOutput) -> String {
    let mut header: Vec<String> = Vec::new();
    if o.time.is_some() {
        header.push("time_s".to_string());
    }
    header.extend(o.columns.iter().cloned());
    let mut s = header
        .iter()
        .map(|h| csv_escape(h))
        .collect::<Vec<_>>()
        .join(",");
    s.push('\n');
    for (i, row) in o.rows.iter().enumerate() {
        let mut cells: Vec<String> = Vec::with_capacity(row.len() + 1);
        if let Some(t) = &o.time {
            cells.push(format!("{:.10e}", t[i]));
        }
        cells.extend(row.iter().map(|v| format!("{v:.10e}")));
        s.push_str(&cells.join(","));
        s.push('\n');
    }
    s
}

/// Solver options from the deck: `.options` tolerances and the `.temp` card
/// (the solver reads every temperature-dependent quantity through
/// `opts.temperature_c`, so the deck temperature must be copied here).
fn solver_opts_from_deck(circuit: &hauksbee_ir::Circuit, directives: &Directives) -> SolverOptions {
    let mut opts = SolverOptions::default();
    opts.reltol = directives.reltol.unwrap_or(opts.reltol);
    opts.abstol = directives.abstol.unwrap_or(opts.abstol);
    opts.vntol = directives.vntol.unwrap_or(opts.vntol);
    opts.temperature_c = circuit.temp_c;
    opts
}

#[cfg(test)]
mod tests {
    use super::{csv_escape, decode_deck_text, sim_output_to_csv, solver_opts_from_deck};
    use hauksbee_ir::SpiceLoader;

    #[test]
    fn binary_deck_bytes_are_refused_with_a_reason() {
        assert_eq!(
            decode_deck_text(b"divider\nV1 in 0 5\0\0".to_vec()),
            Err("contains NUL bytes")
        );
        assert_eq!(
            decode_deck_text(vec![0x7F, 0x45, 0x4C, 0x46, 0xFF, 0xFE]),
            Err("invalid UTF-8 bytes")
        );
        let deck = "divider\nV1 in 0 5\nR1 in out 1k\nR2 out 0 1k\n.end\n";
        assert_eq!(
            decode_deck_text(deck.as_bytes().to_vec()).as_deref(),
            Ok(deck)
        );
    }

    #[test]
    fn csv_escape_quotes_fields_with_commas() {
        assert_eq!(csv_escape("V(out,ref)"), "\"V(out,ref)\"");
        assert_eq!(csv_escape("time_s"), "time_s");
        assert_eq!(csv_escape("say \"hi\""), "\"say \"\"hi\"\"\"");
    }

    #[test]
    fn differential_probe_csv_keeps_columns_aligned() {
        let out = hauksbee_solve::SimOutput {
            columns: vec!["V(out,ref)".to_string(), "I(V1)".to_string()],
            time: Some(vec![1.0e-3]),
            rows: vec![vec![2.5, 1.0e-4]],
            error_budget: hauksbee_ir::evidence::ErrorBudget::new(
                hauksbee_ir::evidence::IntegrationTolerance::new(1e-3, 1e-6, 1e-12, 1e-14).unwrap(),
            ),
        };
        let csv = sim_output_to_csv(&out);
        let mut lines = csv.lines();
        assert_eq!(lines.next().unwrap(), "time_s,\"V(out,ref)\",I(V1)");
        assert_eq!(lines.next().unwrap().split(',').count(), 3);
    }

    #[test]
    fn deck_temp_card_reaches_solver_options() {
        let (c, d) =
            SpiceLoader::load_with_directives("t\nV1 in 0 1\nR1 in 0 1k\n.temp 100\n.op\n.end\n")
                .unwrap();
        let opts = solver_opts_from_deck(&c, &d);
        assert!(
            (opts.temperature_c - 100.0).abs() < 1e-9,
            "got {}",
            opts.temperature_c
        );
        let (c0, d0) =
            SpiceLoader::load_with_directives("t\nV1 in 0 1\nR1 in 0 1k\n.op\n.end\n").unwrap();
        assert!((solver_opts_from_deck(&c0, &d0).temperature_c - 27.0).abs() < 1e-9);
    }
}
