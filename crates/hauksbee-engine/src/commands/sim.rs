//! The `hauksbee sim <deck.cir>` subcommand: load a SPICE `.cir` deck, run the
//! requested analysis (`.op` / `.tran` / `.ac` / `.dc`), and write the results as
//! CSV, an ngspice ASCII rawfile, or both. A malformed deck exits 2; a well-formed
//! deck the solver cannot honestly answer exits 3 rather than faking a result.

use crate::result::{Refusal, EXIT_INVALID_FOR_ANALYSIS};

/// `hauksbee run <board> --ac <fstart>:<fstop>:<points> [--ac-node NET ...]
/// [--ac-csv FILE] [--ac-loop NET]`
///
/// Runs the small-signal AC analysis on the bound circuit and prints a Bode
/// table (magnitude in dB, phase in degrees) for the requested net(s). With
/// `--ac-loop`, also reports gain crossover and phase margin for that net.
/// Exit code for a malformed deck (the loader rejected it). Distinct from the
/// exit-3 "cannot honestly answer" (a well-formed deck we refuse to fake).
pub const EXIT_MALFORMED_DECK: i32 = 2;

fn refuse_sim(claim: &str, missing: impl Into<String>, next_action: &str) -> ! {
    let refusal = Refusal::new(
        claim,
        missing,
        vec!["the deck parsed and the circuit was assembled"],
        next_action,
    );
    eprintln!("error: {claim} refused: {}", refusal.missing_prerequisite);
    eprintln!("{}", refusal.render_text());
    std::process::exit(EXIT_INVALID_FOR_ANALYSIS);
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

/// `hauksbee sim`: load a `.cir`, run `.op` or `.tran`, write CSV.
pub fn run(
    file: &std::path::Path,
    out: Option<&std::path::Path>,
    format: SimFormat,
    op: bool,
    tran: bool,
    ac: bool,
    dc: bool,
    print: &[String],
) -> anyhow::Result<()> {
    use hauksbee_ir::SpiceLoader;
    use hauksbee_solve::{
        default_probes, run_ac, run_dc, run_op, run_tran, write_ascii_rawfile, DcInit, Integration,
        Probe, RawPlot, SimOutput, StepControl,
    };

    // Read the deck. A missing file is an ordinary CLI error (exit 1) with an
    // actionable message, not a deck-malformed refusal.
    let bytes = std::fs::read(file).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            {
                // Never suggest an unrunnable command: the checkout path only
                // exists inside a hauksbee source tree; elsewhere, point at
                // the embedded example.
                let checkout = std::path::Path::new("examples/decks/rlc_ringdown.cir");
                let suggestion = if checkout.exists() {
                    "hauksbee sim examples/decks/rlc_ringdown.cir --tran --print V(out)"
                } else {
                    "hauksbee sim --example rlc_ringdown --tran --print V(out)"
                };
                anyhow::anyhow!(
                    "no deck at '{}'. Check the path, or try a bundled example:\n  \
                     {suggestion}",
                    file.display()
                )
            }
        } else {
            anyhow::anyhow!("reading '{}': {e}", file.display())
        }
    })?;

    // A binary file (invalid UTF-8, or NUL bytes, which ARE valid UTF-8) is
    // user misuse of the deck argument, not a malformed-but-text deck; say
    // what this command expected instead of a per-line parse error.
    let text = match decode_deck_text(bytes) {
        Ok(t) => t,
        Err(why) => {
            eprintln!(
                "error: '{}' is not a text file ({why}); hauksbee sim expects a text SPICE deck (.cir)",
                file.display()
            );
            std::process::exit(EXIT_MALFORMED_DECK);
        }
    };

    // Parse. A SpiceError already carries its line number; print it verbatim and
    // exit 2 (malformed deck), never fall through to a wrong parse.
    let (circuit, directives) = match SpiceLoader::load_with_directives(&text) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(EXIT_MALFORMED_DECK);
        }
    };

    // A deck with no circuit elements has nothing to simulate; the help
    // promises a loud refusal, and an exit-0 run with empty output is the
    // opposite. Comment/blank-only decks and stray directive-only files land
    // here.
    if circuit.devices.is_empty() {
        eprintln!(
            "error: deck has no circuit elements: '{}' parses but contains no devices \
             (R/C/L/V/I/D/Q/M...), so there is nothing to simulate.",
            file.display()
        );
        std::process::exit(EXIT_MALFORMED_DECK);
    }

    // `--format both` writes two files side by side, so it needs a base name to
    // derive them from. Refuse early (rather than dump a rawfile to a terminal)
    // if there is nowhere to put them.
    if matches!(format, SimFormat::Both) && out.is_none() {
        eprintln!(
            "error: --format both writes a CSV and a rawfile side by side, so it needs \
             --out <FILE> to name them (e.g. --out results.csv writes results.csv and \
             results.raw). Use --format csv or --format raw to print a single format to stdout."
        );
        std::process::exit(EXIT_MALFORMED_DECK);
    }

    // The deck's title line (SPICE convention: line 1) becomes the rawfile
    // `Title:`. Fall back to a stable placeholder when the deck omits it.
    let title = text
        .lines()
        .next()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("hauksbee sim")
        .to_string();

    // Choose the analysis: an explicit flag wins; otherwise a `.tran` card means
    // transient and anything else means the operating point.
    #[derive(Clone, Copy)]
    enum Analysis {
        Op,
        Tran,
        Ac,
        Dc,
    }
    let analysis = if op {
        Analysis::Op
    } else if tran {
        Analysis::Tran
    } else if ac {
        Analysis::Ac
    } else if dc {
        Analysis::Dc
    } else if directives.tran.is_some() {
        Analysis::Tran
    } else if directives.dc.is_some() {
        Analysis::Dc
    } else if directives.ac.is_some() {
        Analysis::Ac
    } else {
        Analysis::Op
    };

    // The `.print`/`.plot ANALYSIS` label that matches the chosen analysis.
    let analysis_tag = match analysis {
        Analysis::Op => "op",
        Analysis::Tran => "tran",
        Analysis::Ac => "ac",
        Analysis::Dc => "dc",
    };

    // Probes, in priority order:
    //   1. `--print` on the command line wins outright.
    //   2. else the deck's `.print`/`.plot` cards for this analysis.
    //   3. else every node voltage (announced, so it is never a silent surprise).
    // Informational notes are collected here and only printed once the probe
    // set is fully parsed and validated: a run that is about to die with a
    // fatal probe error must not chirp a note first.
    let mut notes: Vec<String> = Vec::new();
    let probes: Vec<Probe> = if !print.is_empty() {
        let mut ps = Vec::with_capacity(print.len());
        for tok in print {
            match Probe::parse(tok) {
                Ok(p) => ps.push(p),
                Err(msg) => {
                    eprintln!("error: --print: {msg}");
                    std::process::exit(EXIT_MALFORMED_DECK);
                }
            }
        }
        ps
    } else {
        // Collect the deck's output variables for this analysis. `.plot` is
        // treated as `.print` (we emit CSV, never an ASCII plot), noted once.
        let mut deck_vars: Vec<String> = Vec::new();
        for pr in &directives.prints {
            if pr.analysis == analysis_tag {
                deck_vars.extend(pr.vars.iter().cloned());
            }
        }
        if directives.saw_plot {
            notes.push(
                "note: `.plot` cards are treated as `.print` (CSV output; no ASCII plot)."
                    .to_string(),
            );
        }
        if deck_vars.is_empty() {
            notes.push(format!(
                "note: no --print and no matching `.print {analysis_tag}` card; \
                 writing every node voltage."
            ));
            default_probes(&circuit)
        } else {
            let mut ps = Vec::with_capacity(deck_vars.len());
            for tok in &deck_vars {
                match Probe::parse(tok) {
                    Ok(p) => ps.push(p),
                    Err(msg) => {
                        eprintln!("error: `.print {analysis_tag}` output variable: {msg}");
                        std::process::exit(EXIT_MALFORMED_DECK);
                    }
                }
            }
            ps
        }
    };

    // Validate every probe against the circuit BEFORE solving: a mistyped
    // probe is user misuse (exit 2), and the old path reported it through the
    // solver as "did not converge", blaming the circuit for a typo.
    for p in &probes {
        let bad: Option<String> = match p {
            Probe::NodeVoltage(a) => circuit
                .find_node(a)
                .is_none()
                .then(|| format!("V({a}): the deck has no node named '{a}'")),
            Probe::NodeDiff(a, b) => [a, b]
                .into_iter()
                .find(|n| circuit.find_node(n).is_none())
                .map(|n| format!("V({a},{b}): the deck has no node named '{n}'")),
            Probe::BranchCurrent(d) => circuit
                .devices
                .iter()
                .all(|dev| !dev.name().eq_ignore_ascii_case(d))
                .then(|| format!("I({d}): the deck has no element named '{d}'")),
        };
        if let Some(why) = bad {
            let known: Vec<&str> = circuit.node_names().collect();
            eprintln!(
                "error: invalid probe: {why} (known nodes: {})",
                known.join(", ")
            );
            std::process::exit(EXIT_MALFORMED_DECK);
        }
    }

    // Every fatal pre-solve exit is behind us; the deferred notes can print.
    for note in &notes {
        eprintln!("{note}");
    }

    // Build solver options from the deck's tolerances and `.temp`.
    let mut opts = solver_opts_from_deck(&circuit, &directives);

    let output: SimOutput = match analysis {
        Analysis::Op => match run_op(&circuit, &opts, &probes) {
            Ok(o) => o,
            Err(msg) => {
                refuse_sim(
                    "DC operating-point analysis",
                    format!("the DC operating point did not converge: {msg}"),
                    "inspect the named non-convergent node/device, correct its model or bias path, then rerun --op",
                );
            }
        },
        Analysis::Tran => {
            let Some(td) = directives.tran else {
                refuse_sim(
                    "transient analysis",
                    "the deck has no `.tran` card, so there is no stop time or step to run",
                    "add `.tran <tstep> <tstop>` to the deck, then rerun --tran (or use --op)",
                );
            };
            // Adaptive step bounded by the deck's requested step (its tmax if
            // given, else tstep), the same shape the existing cross-check uses.
            let dt_max = td.tmax.unwrap_or(td.tstep).max(1e-15);
            opts.integration = Integration::Trapezoidal;
            opts.step = StepControl::Adaptive {
                dt_initial: (td.tstep / 100.0).max(1e-15),
                dt_min: 1e-15,
                dt_max,
            };
            // `uic` means power-on start: skip the DC solve, march from rest.
            if directives.use_initial_conditions {
                opts.dc_init = DcInit::FromZero;
            }
            match run_tran(&circuit, &opts, td.tstop, &probes) {
                Ok(o) => o,
                Err(msg) => {
                    refuse_sim(
                        "transient analysis",
                        format!("the transient solve failed: {msg}"),
                        "inspect the named failed timestep/node, correct its model or timestep constraints, then rerun --tran",
                    );
                }
            }
        }
        Analysis::Dc => {
            let Some(dc_card) = &directives.dc else {
                refuse_sim(
                    "DC sweep",
                    "the deck has no `.dc` card, so there is no source or range to sweep",
                    "add `.dc <src> <start> <stop> <step>` to the deck, then rerun --dc (or use --op)",
                );
            };
            match run_dc(&circuit, &opts, dc_card, &probes) {
                Ok(o) => o,
                Err(msg) => {
                    refuse_sim(
                        "DC sweep",
                        format!("a DC sweep point did not converge: {msg}"),
                        "inspect the named failed sweep point/device, correct its model or range, then rerun --dc",
                    );
                }
            }
        }
        Analysis::Ac => {
            let Some(ac_card) = &directives.ac else {
                refuse_sim(
                    "AC analysis",
                    "the deck has no `.ac` card, so there is no frequency sweep to run",
                    "add `.ac <dec|oct|lin> <n> <fstart> <fstop>` to the deck, then rerun --ac",
                );
            };
            match run_ac(&circuit, &opts, ac_card, &probes) {
                Ok(o) => o,
                Err(msg) => {
                    let refusal = Refusal::new(
                        "AC analysis",
                        msg.to_string(),
                        vec!["the deck parsed and the circuit was assembled"],
                        "Add `AC 1` to the driving source, then rerun the same --ac command",
                    );
                    eprintln!(
                        "error: AC analysis refused: {}",
                        refusal.missing_prerequisite
                    );
                    eprintln!("{}", refusal.render_text());
                    std::process::exit(EXIT_INVALID_FOR_ANALYSIS);
                }
            }
        }
    };

    // The rawfile writer lives solve-side next to SimOutput; the CLI only needs
    // to tell it which plot this is (the SimOutput does not record its analysis).
    let plot = match analysis {
        Analysis::Op => RawPlot::OperatingPoint,
        Analysis::Tran => RawPlot::Transient,
        Analysis::Dc => RawPlot::Dc,
        Analysis::Ac => RawPlot::Ac,
    };

    // Path rules (documented in --help via SimFormat):
    //   raw  + --out X  -> write the rawfile to X if X ends `.raw`, else to
    //                      X-with-`.raw` (so `--out r.csv` -> `r.raw`).
    //   raw  + no --out -> print the rawfile to stdout.
    //   both + --out X  -> CSV to X (or X-with-`.csv` if X ends `.raw`) AND
    //                      rawfile to X-with-`.raw`, side by side.
    //   csv keeps its existing behavior (X or stdout).
    let raw_path = |base: &std::path::Path| -> std::path::PathBuf {
        if base.extension().and_then(|e| e.to_str()) == Some("raw") {
            base.to_path_buf()
        } else {
            base.with_extension("raw")
        }
    };
    let announce = |path: &std::path::Path| {
        eprintln!(
            "wrote {} row(s) x {} column(s) to {}",
            output.rows.len(),
            output.columns.len(),
            path.display()
        );
    };

    match format {
        SimFormat::Csv => {
            let csv = sim_output_to_csv(&output);
            match out {
                Some(path) => {
                    std::fs::write(path, csv)
                        .map_err(|e| anyhow::anyhow!("writing '{}': {e}", path.display()))?;
                    announce(path);
                }
                None => print!("{csv}"),
            }
        }
        SimFormat::Raw => {
            let raw = write_ascii_rawfile(&output, plot, &title);
            match out {
                Some(base) => {
                    let path = raw_path(base);
                    std::fs::write(&path, raw)
                        .map_err(|e| anyhow::anyhow!("writing '{}': {e}", path.display()))?;
                    announce(&path);
                }
                None => print!("{raw}"),
            }
        }
        SimFormat::Both => {
            // `out` is Some here (refused above when it is not).
            let base = out.expect("--format both requires --out (refused above)");
            let csv_path = if base.extension().and_then(|e| e.to_str()) == Some("raw") {
                base.with_extension("csv")
            } else {
                base.to_path_buf()
            };
            let raw = raw_path(base);
            let csv_text = sim_output_to_csv(&output);
            std::fs::write(&csv_path, csv_text)
                .map_err(|e| anyhow::anyhow!("writing '{}': {e}", csv_path.display()))?;
            announce(&csv_path);
            let raw_text = write_ascii_rawfile(&output, plot, &title);
            std::fs::write(&raw, raw_text)
                .map_err(|e| anyhow::anyhow!("writing '{}': {e}", raw.display()))?;
            announce(&raw);
        }
    }
    Ok(())
}

/// Decode raw deck bytes as text, refusing binary input. Returns the reason a
/// human can act on: invalid UTF-8, or embedded NUL bytes (which ARE valid
/// UTF-8, so `String::from_utf8` alone would let binary content through).
fn decode_deck_text(bytes: Vec<u8>) -> Result<String, &'static str> {
    let text = String::from_utf8(bytes).map_err(|_| "invalid UTF-8 bytes")?;
    if text.contains('\0') {
        return Err("contains NUL bytes");
    }
    Ok(text)
}

/// RFC-4180 escape a single CSV field: wrap in double quotes (doubling any
/// internal quote) when it contains a comma, quote, or newline. A differential
/// probe label like `V(out,ref)` carries a comma, so an unescaped header field
/// would split into two columns and misalign every data cell that follows.
pub(crate) fn csv_escape(field: &str) -> std::borrow::Cow<'_, str> {
    if field.contains([',', '"', '\n', '\r']) {
        std::borrow::Cow::Owned(format!("\"{}\"", field.replace('"', "\"\"")))
    } else {
        std::borrow::Cow::Borrowed(field)
    }
}

/// Render a [`hauksbee_solve::SimOutput`] as CSV. A transient prepends a
/// `time_s` column; an operating point is a bare header + one row.
fn sim_output_to_csv(o: &hauksbee_solve::SimOutput) -> String {
    let mut s = String::new();
    let mut header = Vec::new();
    if o.time.is_some() {
        header.push("time_s".to_string());
    }
    header.extend(o.columns.iter().cloned());
    s.push_str(
        &header
            .iter()
            .map(|h| csv_escape(h))
            .collect::<Vec<_>>()
            .join(","),
    );
    s.push('\n');
    for (i, row) in o.rows.iter().enumerate() {
        let mut cells = Vec::with_capacity(row.len() + 1);
        if let Some(t) = &o.time {
            cells.push(format!("{:.10e}", t[i]));
        }
        for v in row {
            cells.push(format!("{v:.10e}"));
        }
        s.push_str(&cells.join(","));
        s.push('\n');
    }
    s
}

/// Build [`SolverOptions`] from a deck's directives: its `.options` tolerances
/// and its `.temp` card. The loader records the deck temperature in
/// `circuit.temp_c`; the solver reads every temperature-dependent quantity
/// (diode/BJT saturation current, thermal voltage, resistor `tc1`) through
/// `opts.temperature_c`, which defaults to 27 °C, so `.temp` must be copied
/// here or every analysis silently runs at 27 °C regardless of the deck.
fn solver_opts_from_deck(
    circuit: &hauksbee_ir::Circuit,
    directives: &hauksbee_ir::Directives,
) -> hauksbee_solve::SolverOptions {
    let mut opts = hauksbee_solve::SolverOptions::default();
    if let Some(r) = directives.reltol {
        opts.reltol = r;
    }
    if let Some(a) = directives.abstol {
        opts.abstol = a;
    }
    if let Some(v) = directives.vntol {
        opts.vntol = v;
    }
    opts.temperature_c = circuit.temp_c;
    opts
}

#[cfg(test)]
mod tests {
    use super::{csv_escape, decode_deck_text, sim_output_to_csv, solver_opts_from_deck};
    use hauksbee_ir::SpiceLoader;

    #[test]
    fn binary_deck_bytes_are_refused_with_a_reason() {
        // NUL bytes are valid UTF-8, so they need their own check.
        assert_eq!(
            decode_deck_text(b"divider\nV1 in 0 5\0\0".to_vec()),
            Err("contains NUL bytes")
        );
        // Not UTF-8 at all (e.g. an ELF header or random binary).
        assert_eq!(
            decode_deck_text(vec![0x7F, 0x45, 0x4C, 0x46, 0xFF, 0xFE]),
            Err("invalid UTF-8 bytes")
        );
        // An ordinary text deck passes through untouched.
        let deck = "divider\nV1 in 0 5\nR1 in out 1k\nR2 out 0 1k\n.end\n";
        assert_eq!(
            decode_deck_text(deck.as_bytes().to_vec()).as_deref(),
            Ok(deck)
        );
    }

    #[test]
    fn csv_escape_quotes_fields_with_commas() {
        // R23 (CSV-DIFF-PROBE-COMMA): a differential-probe label like V(out,ref)
        // carries a comma; unquoted it would split into two columns.
        assert_eq!(csv_escape("V(out,ref)"), "\"V(out,ref)\"");
        assert_eq!(csv_escape("time_s"), "time_s"); // no comma → untouched
        assert_eq!(csv_escape("say \"hi\""), "\"say \"\"hi\"\"\""); // quotes doubled
    }

    #[test]
    fn differential_probe_csv_keeps_columns_aligned() {
        // The header and every data row must have the same field count. With a
        // differential probe V(out,ref) the unescaped header had 4 comma-fields
        // while the row had 3, misassigning every downstream column.
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
        let header = lines.next().unwrap();
        let row = lines.next().unwrap();
        // csv-field count: commas outside quotes + 1. Compare structurally.
        assert_eq!(
            header, "time_s,\"V(out,ref)\",I(V1)",
            "the diff-probe label must be quoted as one column"
        );
        assert_eq!(
            row.split(',').count(),
            3,
            "the data row has exactly time + 2 probe columns: {row}"
        );
    }

    #[test]
    fn deck_temp_card_reaches_solver_options() {
        // A `.temp 100` deck must run the solver at 100 °C, not the default 27.
        let (c, d) =
            SpiceLoader::load_with_directives("t\nV1 in 0 1\nR1 in 0 1k\n.temp 100\n.op\n.end\n")
                .unwrap();
        let opts = solver_opts_from_deck(&c, &d);
        assert!(
            (opts.temperature_c - 100.0).abs() < 1e-9,
            "got {}",
            opts.temperature_c
        );

        // No `.temp` card → the solver default (27 °C) is preserved.
        let (c0, d0) =
            SpiceLoader::load_with_directives("t\nV1 in 0 1\nR1 in 0 1k\n.op\n.end\n").unwrap();
        let opts0 = solver_opts_from_deck(&c0, &d0);
        assert!(
            (opts0.temperature_c - 27.0).abs() < 1e-9,
            "got {}",
            opts0.temperature_c
        );
    }
}
