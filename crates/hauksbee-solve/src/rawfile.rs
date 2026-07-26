//! ngspice-compatible ASCII rawfile writer (SPICE-compat plan step 14).
//!
//! A rawfile is the on-disk format ngspice's own tooling (`ngnutmeg`, `gaw`,
//! the Python `spicelib`) reads. Emitting one makes hauksbee a *drop-in*: a
//! downstream tool that reads a rawfile does not care which engine produced it.
//! This writer consumes the very same [`SimOutput`] the CSV writer does; it
//! adds no physics, it only reshapes.
//!
//! ## The format (verified against ngspice-46 `write` output)
//!
//! ```text
//! Title: <deck title>
//! Date: Thu Jan  1 00:00:00  1970
//! Command: hauksbee sim (ngspice-compatible rawfile)
//! Plotname: <Transient Analysis | AC Analysis | DC transfer characteristic | Operating Point>
//! Flags: real            (or `complex` for AC)
//! No. Variables: N
//! No. Points: M
//! Variables:
//! \t0\ttime\ttime
//! \t1\tv(out)\tvoltage
//! Values:
//!  0\t<v0>
//! \t<v1>
//!
//!  1\t<v0>
//! \t<v1>
//! ```
//!
//! The Values block is whitespace-exact to what ngspice writes: the point line
//! is `SPACE index TAB value`, every later variable is `TAB value`, and a blank
//! line separates point groups. Complex (AC) values are `re,im` (comma, no
//! space). Numbers are `%.15e` with a signed two-digit exponent (`e+02`,
//! `e-11`), matching ngspice byte-for-byte so a diff against a reference raw is
//! meaningful.
//!
//! ## Determinism (repo doctrine: reproducible bytes)
//!
//! ngspice stamps a wall-clock `Date:`. We do not, a rawfile that changes
//! every run is not diff-able. `Date:` is a **fixed epoch placeholder**
//! (`Thu Jan  1 00:00:00  1970`): date-shaped so any tool that parses the field
//! succeeds, constant so two runs of the same deck produce identical bytes.
//!
//! ## Plotname strings matter
//!
//! Tools branch on `Plotname`, so the four strings are exactly ngspice's:
//! `Transient Analysis`, `AC Analysis`, `DC transfer characteristic`,
//! `Operating Point`. The scale (independent) variable follows the plot: `time`
//! for transient, `frequency` for AC, the swept source (named `v-sweep` /
//! `i-sweep` as ngspice does) for DC, and *no* scale for the operating point
//! (a single point has nothing to sweep).

use crate::SimOutput;

/// Fixed, deterministic `Date:` placeholder, see the module doc-comment.
/// Date-shaped so tools parse it; constant so output is byte-reproducible.
const RAW_DATE: &str = "Thu Jan  1 00:00:00  1970";

/// The `Command:` line. ngspice-aware readers (e.g. `spicelib`) auto-detect the
/// rawfile *dialect* from this line: they look for the substring `ngspice`.
/// We name hauksbee honestly *and* carry that token, so a reader picks the
/// ngspice dialect (double-`time`, complex-AC layout, which is what we emit)
/// without the caller having to pass a dialect hint. Deterministic: no version
/// churn, so the bytes stay reproducible.
const RAW_COMMAND: &str = "hauksbee sim (ngspice-compatible rawfile)";

/// Which ngspice "plot" a [`SimOutput`] represents. Drives the `Plotname`
/// header, the scale variable, and real-vs-complex encoding. The [`SimOutput`]
/// itself does not record which analysis produced it, so the caller (who chose
/// the analysis) declares it here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawPlot {
    /// `.op`: a single point, no scale variable. Plotname `Operating Point`.
    OperatingPoint,
    /// `.tran`: `time` scale. Plotname `Transient Analysis`.
    Transient,
    /// `.dc`: the swept source is the scale. Plotname `DC transfer characteristic`.
    Dc,
    /// `.ac`: complex, `frequency` scale. Plotname `AC Analysis`.
    Ac,
}

impl RawPlot {
    /// The exact ngspice `Plotname:` string tools switch on.
    fn plotname(self) -> &'static str {
        match self {
            RawPlot::OperatingPoint => "Operating Point",
            RawPlot::Transient => "Transient Analysis",
            RawPlot::Dc => "DC transfer characteristic",
            RawPlot::Ac => "AC Analysis",
        }
    }
}

/// The ngspice variable *type* for a probe label: `I(...)` is a current,
/// everything else (`V(x)`, `V(a,b)`, a bare node) is a voltage.
fn var_type(label: &str) -> &'static str {
    match label.trim_start().chars().next() {
        Some('i') | Some('I') => "current",
        _ => "voltage",
    }
}

/// Format one real value as ngspice does: `%.15e` with a signed, two-digit-min
/// exponent (`1.000000000000000e+02`, `4.999999500000050e-09`). Rust's `{:e}`
/// writes the exponent as a bare `e2` / `e-9`, so we re-pad it.
fn fmt_e(x: f64) -> String {
    let raw = format!("{x:.15e}"); // e.g. "1.000000000000000e2" or "...e-9"
    match raw.split_once('e') {
        Some((mant, exp)) => {
            let e: i32 = exp.parse().unwrap_or(0);
            let sign = if e < 0 { '-' } else { '+' };
            format!("{mant}e{sign}{:02}", e.abs())
        }
        None => raw, // NaN / inf: leave as-is (deterministic, and a loud tell)
    }
}

/// A normalized view of one plot for serialization: the ordered variable
/// definitions (name, type), including the scale as index 0 when present, and
/// the per-point value matrix in the same column order. `im` is 0 for real
/// plots; `complex` decides whether it is written.
struct RawTable {
    complex: bool,
    /// `(name, type)` per variable, scale first when the plot has one.
    vars: Vec<(String, &'static str)>,
    /// `values[point][var]` = `(re, im)`.
    values: Vec<Vec<(f64, f64)>>,
}

/// Reshape a [`SimOutput`] into the flat variable list + value matrix the
/// rawfile serializer walks, per the analysis conventions.
fn build_table(out: &SimOutput, plot: RawPlot) -> RawTable {
    match plot {
        RawPlot::OperatingPoint => {
            // No scale: every column is a variable; exactly one point.
            let vars = out
                .columns
                .iter()
                .map(|c| (c.clone(), var_type(c)))
                .collect();
            let values = out
                .rows
                .iter()
                .map(|r| r.iter().map(|&v| (v, 0.0)).collect())
                .collect();
            RawTable {
                complex: false,
                vars,
                values,
            }
        }
        RawPlot::Transient => {
            // Scale is `time` (from SimOutput::time); columns are the probes.
            let mut vars = vec![("time".to_string(), "time" as &'static str)];
            vars.extend(out.columns.iter().map(|c| (c.clone(), var_type(c))));
            let time = out
                .time
                .as_ref()
                .expect("transient SimOutput carries a time axis");
            let values = out
                .rows
                .iter()
                .enumerate()
                .map(|(i, r)| {
                    let mut row = vec![(time[i], 0.0)];
                    row.extend(r.iter().map(|&v| (v, 0.0)));
                    row
                })
                .collect();
            RawTable {
                complex: false,
                vars,
                values,
            }
        }
        RawPlot::Dc => {
            // run_dc puts the swept-source value in column 0 (named after the
            // source); ngspice names this scale `v-sweep`/`i-sweep` by the
            // source's kind. The remaining columns are the probes.
            let sweep_name = out.columns.first().map(String::as_str).unwrap_or("v");
            let (scale_name, scale_type) = match sweep_name.chars().next() {
                Some('i') | Some('I') => ("i-sweep", "current"),
                _ => ("v-sweep", "voltage"),
            };
            let mut vars = vec![(scale_name.to_string(), scale_type)];
            vars.extend(out.columns.iter().skip(1).map(|c| (c.clone(), var_type(c))));
            let values = out
                .rows
                .iter()
                .map(|r| r.iter().map(|&v| (v, 0.0)).collect())
                .collect();
            RawTable {
                complex: false,
                vars,
                values,
            }
        }
        RawPlot::Ac => {
            // run_ac lays out columns as: frequency, then (mag, phase_deg) pairs
            // per probe. The rawfile is complex re/im, so we drop each phase
            // column and fold (mag, phase) back into a phasor.
            let mut vars = vec![("frequency".to_string(), "frequency" as &'static str)];
            // Probe base labels sit at odd indices 1,3,5,...; phase at 2,4,6,...
            let mut probe_cols = Vec::new();
            let mut j = 1;
            while j < out.columns.len() {
                vars.push((out.columns[j].clone(), var_type(&out.columns[j])));
                probe_cols.push(j);
                j += 2;
            }
            let values = out
                .rows
                .iter()
                .map(|r| {
                    // Scale (frequency) is real; write it with a zero imaginary.
                    let mut row = vec![(r[0], 0.0)];
                    for &mag_idx in &probe_cols {
                        let mag = r[mag_idx];
                        let phase_rad = r[mag_idx + 1].to_radians();
                        row.push((mag * phase_rad.cos(), mag * phase_rad.sin()));
                    }
                    row
                })
                .collect();
            RawTable {
                complex: true,
                vars,
                values,
            }
        }
    }
}

/// Serialize a [`SimOutput`] as an ngspice ASCII rawfile. `title` is the deck's
/// title line (SPICE convention: the first line of the deck). The bytes are
/// deterministic: the same `SimOutput` and title always produce the same file.
pub fn write_ascii_rawfile(out: &SimOutput, plot: RawPlot, title: &str) -> String {
    let table = build_table(out, plot);
    let n_vars = table.vars.len();
    let n_points = table.values.len();

    let mut s = String::new();
    // Header. `Title:` carries the deck's own title so the file is
    // self-identifying; a blank title still emits the field (tools expect it).
    s.push_str(&format!("Title: {title}\n"));
    s.push_str(&format!("Date: {RAW_DATE}\n"));
    s.push_str(&format!("Command: {RAW_COMMAND}\n"));
    s.push_str(&format!("Plotname: {}\n", plot.plotname()));
    s.push_str(if table.complex {
        "Flags: complex\n"
    } else {
        "Flags: real\n"
    });
    s.push_str(&format!("No. Variables: {n_vars}\n"));
    s.push_str(&format!("No. Points: {n_points}\n"));

    // Variables block: `\t<index>\t<name>\t<type>`.
    s.push_str("Variables:\n");
    for (i, (name, ty)) in table.vars.iter().enumerate() {
        s.push_str(&format!("\t{i}\t{name}\t{ty}\n"));
    }

    // Values block: `SPACE <point> TAB <v0>` then `\t<vk>` per later variable,
    // a blank line between point groups.
    s.push_str("Values:\n");
    for (pt, row) in table.values.iter().enumerate() {
        for (k, &(re, im)) in row.iter().enumerate() {
            if k == 0 {
                s.push_str(&format!(" {pt}\t"));
            } else {
                s.push('\t');
            }
            s.push_str(&fmt_e(re));
            if table.complex {
                s.push(',');
                s.push_str(&fmt_e(im));
            }
            s.push('\n');
        }
        s.push('\n');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(columns: &[&str], time: Option<Vec<f64>>, rows: Vec<Vec<f64>>) -> SimOutput {
        SimOutput {
            columns: columns.iter().map(|s| s.to_string()).collect(),
            time,
            rows,
        }
    }

    #[test]
    fn fmt_e_matches_ngspice_exponent_padding() {
        assert_eq!(fmt_e(100.0), "1.000000000000000e+02");
        assert_eq!(fmt_e(5.0), "5.000000000000000e+00");
        assert_eq!(fmt_e(-1.25e-3), "-1.250000000000000e-03");
        assert_eq!(fmt_e(0.0), "0.000000000000000e+00");
    }

    #[test]
    fn transient_golden_structure_and_endpoints() {
        let o = out(
            &["V(out)"],
            Some(vec![0.0, 1e-6, 2e-6]),
            vec![vec![0.0], vec![2.5], vec![5.0]],
        );
        let raw = write_ascii_rawfile(&o, RawPlot::Transient, "rc lowpass step");
        // The Command line carries the `ngspice` token so readers auto-detect
        // the dialect (see RAW_COMMAND), and it is deterministic.
        assert!(
            raw.contains("Command: hauksbee sim (ngspice-compatible rawfile)\n"),
            "{raw}"
        );
        assert!(raw.to_lowercase().contains("ngspice"));
        assert!(raw.contains("Plotname: Transient Analysis\n"), "{raw}");
        assert!(raw.contains("Flags: real\n"));
        assert!(raw.contains("No. Variables: 2\n"));
        assert!(raw.contains("No. Points: 3\n"));
        assert!(raw.contains("\t0\ttime\ttime\n"));
        assert!(raw.contains("\t1\tV(out)\tvoltage\n"));
        // First point: index 0, time 0, value 0.
        assert!(raw.contains(" 0\t0.000000000000000e+00\n\t0.000000000000000e+00\n"));
        // Last point: index 2, time 2e-6, value 5.
        assert!(raw.contains(" 2\t2.000000000000000e-06\n\t5.000000000000000e+00\n"));
    }

    #[test]
    fn operating_point_has_no_scale_variable() {
        let o = out(&["V(in)", "V(out)"], None, vec![vec![5.0, 3.75]]);
        let raw = write_ascii_rawfile(&o, RawPlot::OperatingPoint, "divider");
        assert!(raw.contains("Plotname: Operating Point\n"));
        assert!(raw.contains("No. Variables: 2\n"));
        assert!(raw.contains("No. Points: 1\n"));
        assert!(raw.contains("\t0\tV(in)\tvoltage\n"));
        assert!(raw.contains("\t1\tV(out)\tvoltage\n"));
        assert!(raw.contains(" 0\t5.000000000000000e+00\n\t3.750000000000000e+00\n"));
    }

    #[test]
    fn dc_sweep_scale_is_v_sweep() {
        // run_dc column 0 is the source name; DC scale becomes `v-sweep`.
        let o = out(
            &["Vin", "V(d)"],
            None,
            vec![vec![0.0, 0.0], vec![0.1, 0.1], vec![0.2, 0.2]],
        );
        let raw = write_ascii_rawfile(&o, RawPlot::Dc, "diode dc sweep");
        assert!(raw.contains("Plotname: DC transfer characteristic\n"));
        assert!(raw.contains("\t0\tv-sweep\tvoltage\n"), "{raw}");
        assert!(raw.contains("\t1\tV(d)\tvoltage\n"));
        assert!(raw.contains(" 0\t0.000000000000000e+00\n\t0.000000000000000e+00\n"));
        assert!(raw.contains(" 2\t2.000000000000000e-01\n\t2.000000000000000e-01\n"));
    }

    #[test]
    fn ac_is_complex_and_folds_mag_phase_to_re_im() {
        // One probe at one frequency: mag 1, phase -90 deg -> (0, -1).
        let o = out(
            &["frequency", "V(out)", "V(out):phase_deg"],
            None,
            vec![vec![1000.0, 1.0, -90.0]],
        );
        let raw = write_ascii_rawfile(&o, RawPlot::Ac, "rc ac bode");
        assert!(raw.contains("Plotname: AC Analysis\n"));
        assert!(raw.contains("Flags: complex\n"));
        assert!(raw.contains("No. Variables: 2\n"));
        assert!(raw.contains("\t0\tfrequency\tfrequency\n"));
        assert!(raw.contains("\t1\tV(out)\tvoltage\n"));
        // frequency written as re,im with a zero imaginary part.
        assert!(
            raw.contains(" 0\t1.000000000000000e+03,0.000000000000000e+00\n"),
            "{raw}"
        );
        // phasor: re ~ 0, im ~ -1. cos(-90 deg) ~ 6.1e-17, sin ~ -1.
        let has_im = raw.contains(",-1.000000000000000e+00\n");
        assert!(has_im, "expected im=-1 phasor line in:\n{raw}");
    }
}
