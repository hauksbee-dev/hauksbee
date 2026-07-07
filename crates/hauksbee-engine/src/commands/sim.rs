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
        default_probes, run_op, run_tran, Integration, Probe, SimOutput, SolverOptions, StepControl,
        DcInit,
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
    } else {
        Analysis::Op
    };

    // Refuse the unwired analyses loudly (exit 3): the netlist front-end does
    // not parse `.ac`/`.dc` directives or AC source magnitudes yet, so there is
    // nothing honest to compute. A loud refusal, never a silent no-op.
    match analysis {
        Analysis::Ac => {
            eprintln!(
                "error: --ac is recognized but not yet wired in `hauksbee sim`. The netlist \
                 front-end does not yet parse `.ac` directives or AC source magnitudes \
                 (SPICE-compat plan step 9). Refusing rather than emitting an empty or wrong \
                 result. Use --op or --tran."
            );
            std::process::exit(EXIT_INVALID_FOR_ANALYSIS);
        }
        Analysis::Dc => {
            eprintln!(
                "error: --dc (DC sweep) is recognized but not yet wired in `hauksbee sim`. The \
                 sweep driver is SPICE-compat plan step 9. Refusing rather than faking a result. \
                 Use --op for a single operating point or --tran."
            );
            std::process::exit(EXIT_INVALID_FOR_ANALYSIS);
        }
        _ => {}
    }

    // Probes: an explicit --print wins; otherwise every node voltage (and we say
    // so, on stderr, so the choice is never a silent surprise).
    let probes: Vec<Probe> = if print.is_empty() {
        eprintln!(
            "note: no --print given and the loader does not yet parse `.print`; \
             writing every node voltage."
        );
        default_probes(&circuit)
    } else {
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
        Analysis::Ac | Analysis::Dc => unreachable!("refused above"),
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
