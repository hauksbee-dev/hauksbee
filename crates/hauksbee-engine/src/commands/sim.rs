//! The `hauksbee sim <deck.cir>` subcommand: load a SPICE `.cir` deck, run the
//! requested analysis (`.op` / `.tran` / `.ac` / `.dc`), and write the results as
//! CSV, an ngspice ASCII rawfile, or both. A malformed deck exits 2; a well-formed
//! deck the solver cannot honestly answer exits 3 rather than faking a result.

use crate::result::EXIT_INVALID_FOR_ANALYSIS;

/// `hauksbee run <board> --ac <fstart>:<fstop>:<points> [--ac-node NET ...]
/// [--ac-csv FILE] [--ac-loop NET]`
///
/// Runs the small-signal AC analysis on the bound circuit and prints a Bode
/// table (magnitude in dB, phase in degrees) for the requested net(s). With
/// `--ac-loop`, also reports gain crossover and phase margin for that net.
/// Exit code for a malformed deck (the loader rejected it). Distinct from the
/// exit-3 "cannot honestly answer" (a well-formed deck we refuse to fake).
pub const EXIT_MALFORMED_DECK: i32 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum SimFormat {
    /// One column per probe, one row per timepoint (or one row for `.op`).
    Csv,
    /// ngspice ASCII rawfile — the format `ngnutmeg`/`gaw`/`spicelib` read.
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
    let text = std::fs::read_to_string(file).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            anyhow::anyhow!("no deck at '{}'. Check the path.", file.display())
        } else {
            anyhow::anyhow!("reading '{}': {e}", file.display())
        }
    })?;

    // Parse. A SpiceError already carries its line number; print it verbatim and
    // exit 2 (malformed deck) — never fall through to a wrong parse.
    let (circuit, directives) = match SpiceLoader::load_with_directives(&text) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(EXIT_MALFORMED_DECK);
        }
    };

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
        // treated as `.print` (we emit CSV, never an ASCII plot) — noted once.
        let mut deck_vars: Vec<String> = Vec::new();
        for pr in &directives.prints {
            if pr.analysis == analysis_tag {
                deck_vars.extend(pr.vars.iter().cloned());
            }
        }
        if directives.saw_plot {
            eprintln!("note: `.plot` cards are treated as `.print` (CSV output; no ASCII plot).");
        }
        if deck_vars.is_empty() {
            eprintln!(
                "note: no --print and no matching `.print {analysis_tag}` card; \
                 writing every node voltage."
            );
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

    // Build solver options from the deck's tolerances and `.temp`.
    let mut opts = solver_opts_from_deck(&circuit, &directives);

    let output: SimOutput = match analysis {
        Analysis::Op => match run_op(&circuit, &opts, &probes) {
            Ok(o) => o,
            Err(msg) => {
                eprintln!(
                    "error: DC operating point did not converge (or a probe was invalid): {msg}"
                );
                std::process::exit(EXIT_INVALID_FOR_ANALYSIS);
            }
        },
        Analysis::Tran => {
            let Some(td) = directives.tran else {
                eprintln!(
                    "error: --tran requested but the deck has no `.tran` card, so there is no \
                     stop time or step to run. Add `.tran <tstep> <tstop>` or use --op."
                );
                std::process::exit(EXIT_INVALID_FOR_ANALYSIS);
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
                    eprintln!("error: transient solve failed: {msg}");
                    std::process::exit(EXIT_INVALID_FOR_ANALYSIS);
                }
            }
        }
        Analysis::Dc => {
            let Some(dc_card) = &directives.dc else {
                eprintln!(
                    "error: --dc requested but the deck has no `.dc` card, so there is no \
                     source or range to sweep. Add `.dc <src> <start> <stop> <step>` or use --op."
                );
                std::process::exit(EXIT_INVALID_FOR_ANALYSIS);
            };
            match run_dc(&circuit, &opts, dc_card, &probes) {
                Ok(o) => o,
                Err(msg) => {
                    eprintln!("error: DC sweep failed (a point did not converge or a probe was invalid): {msg}");
                    std::process::exit(EXIT_INVALID_FOR_ANALYSIS);
                }
            }
        }
        Analysis::Ac => {
            let Some(ac_card) = &directives.ac else {
                eprintln!(
                    "error: --ac requested but the deck has no `.ac` card, so there is no \
                     frequency sweep to run. Add `.ac <dec|oct|lin> <n> <fstart> <fstop>`."
                );
                std::process::exit(EXIT_INVALID_FOR_ANALYSIS);
            };
            match run_ac(&circuit, &opts, ac_card, &probes) {
                Ok(o) => o,
                Err(msg) => {
                    eprintln!("error: AC analysis refused: {msg}");
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
    s.push_str(&header.iter().map(|h| csv_escape(h)).collect::<Vec<_>>().join(","));
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
/// `opts.temperature_c`, which defaults to 27 °C — so `.temp` must be copied
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
    use super::{csv_escape, sim_output_to_csv, solver_opts_from_deck};
    use hauksbee_ir::SpiceLoader;

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
        assert!((opts.temperature_c - 100.0).abs() < 1e-9, "got {}", opts.temperature_c);

        // No `.temp` card → the solver default (27 °C) is preserved.
        let (c0, d0) =
            SpiceLoader::load_with_directives("t\nV1 in 0 1\nR1 in 0 1k\n.op\n.end\n").unwrap();
        let opts0 = solver_opts_from_deck(&c0, &d0);
        assert!((opts0.temperature_c - 27.0).abs() < 1e-9, "got {}", opts0.temperature_c);
    }
}
