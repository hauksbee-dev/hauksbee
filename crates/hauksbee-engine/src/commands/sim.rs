//! `hauksbee sim <deck.cir>`: load a SPICE deck, run `.op`/`.tran`, write CSV.

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
    /// ngspice ASCII rawfile — not yet implemented (plan step 14).
    Raw,
    /// CSV and rawfile side by side — not yet implemented (plan step 14).
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
        default_probes, run_ac, run_dc, run_op, run_tran, DcInit, Integration, Probe, SimOutput,
        SolverOptions, StepControl,
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

    // Output format: only CSV is implemented. Rawfile refuses loudly (plan
    // step 14) rather than emitting nothing or the wrong thing.
    if matches!(format, SimFormat::Raw | SimFormat::Both) {
        eprintln!(
            "error: --format {} is not yet implemented (ngspice rawfile output is \
             SPICE-compat plan step 14). Use --format csv (the default).",
            match format {
                SimFormat::Raw => "raw",
                SimFormat::Both => "both",
                SimFormat::Csv => unreachable!(),
            }
        );
        std::process::exit(EXIT_INVALID_FOR_ANALYSIS);
    }

    // Choose the analysis: an explicit flag wins; otherwise a `.tran` card means
    // transient and anything else means the operating point.
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

    // Build solver options from the deck's tolerances.
    let mut opts = SolverOptions::default();
    if let Some(r) = directives.reltol {
        opts.reltol = r;
    }
    if let Some(a) = directives.abstol {
        opts.abstol = a;
    }
    if let Some(v) = directives.vntol {
        opts.vntol = v;
    }

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

    // Serialize to CSV and write to --out or stdout.
    let csv = sim_output_to_csv(&output);
    match out {
        Some(path) => {
            std::fs::write(path, csv)
                .map_err(|e| anyhow::anyhow!("writing '{}': {e}", path.display()))?;
            eprintln!(
                "wrote {} row(s) x {} column(s) to {}",
                output.rows.len(),
                output.columns.len(),
                path.display()
            );
        }
        None => print!("{csv}"),
    }
    Ok(())
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
    s.push_str(&header.join(","));
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
