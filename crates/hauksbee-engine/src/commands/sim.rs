//! `hauksbee sim <deck.cir>`: load a SPICE deck, run `.op`/`.tran`/`.ac`/`.dc`,
//! and write CSV, an ngspice ASCII rawfile, or both. A malformed deck exits 2;
//! a well-formed deck the solver cannot honestly answer exits 3.

use crate::result::{Refusal, EXIT_INVALID_FOR_ANALYSIS};
use hauksbee_ir::{Directives, SpiceLoader};
use hauksbee_solve::{
    default_probes, run_ac, run_dc, run_op, run_tran, write_ascii_rawfile, DcInit, Integration,
    Probe, RawPlot, SimOutput, SolverOptions, StepControl,
};
use std::path::{Path, PathBuf};

/// Exit code for a malformed deck (the loader rejected it).
pub const EXIT_MALFORMED_DECK: i32 = 2;

fn malformed(msg: impl std::fmt::Display) -> ! {
    eprintln!("error: {msg}");
    std::process::exit(EXIT_MALFORMED_DECK);
}

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
        malformed(format!(
            "'{}' is not a text file ({why}); hauksbee sim expects a text SPICE deck (.cir)",
            file.display()
        ))
    });
    let (circuit, directives) =
        SpiceLoader::load_file_with_directives(file).unwrap_or_else(|e| malformed(e));
    if circuit.devices.is_empty() {
        malformed(format!(
            "deck has no circuit elements: '{}' parses but contains no devices \
             (R/C/L/V/I/D/Q/M...), so there is nothing to simulate.",
            file.display()
        ));
    }
    if matches!(format, SimFormat::Both) && out.is_none() {
        malformed(
            "--format both writes a CSV and a rawfile side by side, so it needs \
             --out <FILE> to name them (e.g. --out results.csv writes results.csv and \
             results.raw). Use --format csv or --format raw to print a single format to stdout.",
        );
    }
    // The deck's title line (SPICE convention: line 1) becomes the rawfile `Title:`.
    let title = text
        .lines()
        .next()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("hauksbee sim");

    // An explicit flag wins; otherwise the deck's analysis card, else `.op`.
    let analysis = match (op, tran, ac, dc) {
        (true, ..) => Analysis::Op,
        (_, true, ..) => Analysis::Tran,
        (_, _, true, _) => Analysis::Ac,
        (.., true) => Analysis::Dc,
        _ if directives.tran.is_some() => Analysis::Tran,
        _ if directives.dc.is_some() => Analysis::Dc,
        _ if directives.ac.is_some() => Analysis::Ac,
        _ => Analysis::Op,
    };
    let tag = match analysis {
        Analysis::Op => "op",
        Analysis::Tran => "tran",
        Analysis::Ac => "ac",
        Analysis::Dc => "dc",
    };

    // Probes: `--print` wins; else the deck's `.print`/`.plot` cards for this
    // analysis; else every node voltage. Notes print only once every fatal
    // pre-solve check has passed.
    let mut notes: Vec<String> = Vec::new();
    let parse_probes = |toks: &[String], what: &str| -> Vec<Probe> {
        toks.iter()
            .map(|t| Probe::parse(t).unwrap_or_else(|e| malformed(format!("{what}: {e}"))))
            .collect()
    };
    let probes: Vec<Probe> = if !print.is_empty() {
        parse_probes(print, "--print")
    } else {
        let deck_vars: Vec<String> = directives
            .prints
            .iter()
            .filter(|pr| pr.analysis == tag)
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
            default_probes(&circuit)
        } else {
            parse_probes(&deck_vars, &format!("`.print {tag}` output variable"))
        }
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
            malformed(format!(
                "invalid probe: {why} (known nodes: {})",
                known.join(", ")
            ));
        }
    }
    for note in &notes {
        eprintln!("{note}");
    }

    let mut opts = solver_opts_from_deck(&circuit, &directives);
    let output: SimOutput = match analysis {
        Analysis::Op => run_op(&circuit, &opts, &probes).unwrap_or_else(|msg| {
            refuse_sim(
                "DC operating-point analysis",
                format!("the DC operating point did not converge: {msg}"),
                "inspect the named non-convergent node/device, correct its model or bias path, then rerun --op",
            )
        }),
        Analysis::Tran => {
            let Some(td) = directives.tran else {
                refuse_sim(
                    "transient analysis",
                    "the deck has no `.tran` card, so there is no stop time or step to run",
                    "add `.tran <tstep> <tstop>` to the deck, then rerun --tran (or use --op)",
                );
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
            run_tran(&circuit, &opts, td.tstop, &probes).unwrap_or_else(|msg| {
                refuse_sim(
                    "transient analysis",
                    format!("the transient solve failed: {msg}"),
                    "inspect the named failed timestep/node, correct its model or timestep constraints, then rerun --tran",
                )
            })
        }
        Analysis::Dc => {
            let Some(card) = &directives.dc else {
                refuse_sim(
                    "DC sweep",
                    "the deck has no `.dc` card, so there is no source or range to sweep",
                    "add `.dc <src> <start> <stop> <step>` to the deck, then rerun --dc (or use --op)",
                );
            };
            run_dc(&circuit, &opts, card, &probes).unwrap_or_else(|msg| {
                refuse_sim(
                    "DC sweep",
                    format!("a DC sweep point did not converge: {msg}"),
                    "inspect the named failed sweep point/device, correct its model or range, then rerun --dc",
                )
            })
        }
        Analysis::Ac => {
            let Some(card) = &directives.ac else {
                refuse_sim(
                    "AC analysis",
                    "the deck has no `.ac` card, so there is no frequency sweep to run",
                    "add `.ac <dec|oct|lin> <n> <fstart> <fstop>` to the deck, then rerun --ac",
                );
            };
            run_ac(&circuit, &opts, card, &probes).unwrap_or_else(|msg| {
                refuse_sim(
                    "AC analysis",
                    msg.to_string(),
                    "Add `AC 1` to the driving source, then rerun the same --ac command",
                )
            })
        }
    };

    let plot = match analysis {
        Analysis::Op => RawPlot::OperatingPoint,
        Analysis::Tran => RawPlot::Transient,
        Analysis::Dc => RawPlot::Dc,
        Analysis::Ac => RawPlot::Ac,
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
            output.columns.len(),
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
pub(crate) fn csv_escape(field: &str) -> std::borrow::Cow<'_, str> {
    if field.contains([',', '"', '\n', '\r']) {
        std::borrow::Cow::Owned(format!("\"{}\"", field.replace('"', "\"\"")))
    } else {
        std::borrow::Cow::Borrowed(field)
    }
}

/// Render a [`SimOutput`] as CSV; a transient prepends a `time_s` column.
fn sim_output_to_csv(o: &SimOutput) -> String {
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
