//! ngspice-compatible ASCII rawfile writer, the format `ngnutmeg`, `gaw` and
//! `spicelib` read. It reshapes a [`SimOutput`]; it adds no physics.
//!
//! Layout (verified against ngspice-46 `write` output): `Title:`, `Date:`,
//! `Command:`, `Plotname:`, `Flags: real|complex`, `No. Variables:`,
//! `No. Points:`, a `Variables:` block of `\t<i>\t<name>\t<type>` lines, then a
//! `Values:` block where each point is ` <i>\t<v0>` followed by `\t<vk>` per
//! variable and a blank line. Numbers are `%.15e` with a signed two-digit
//! exponent; complex values are `re,im`. `Date:` is a fixed epoch placeholder
//! so two runs of one deck produce identical bytes.

use crate::SimOutput;

const RAW_DATE: &str = "Thu Jan  1 00:00:00  1970";
/// Carries the `ngspice` token so readers auto-detect the dialect.
const RAW_COMMAND: &str = "hauksbee sim (ngspice-compatible rawfile)";

/// Which ngspice plot a [`SimOutput`] represents; the caller chose the analysis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawPlot {
    /// `.op`: a single point, no scale variable.
    OperatingPoint,
    /// `.tran`: `time` scale.
    Transient,
    /// `.dc`: the swept source is the scale (`v-sweep`/`i-sweep`).
    Dc,
    /// `.ac`: complex values, `frequency` scale.
    Ac,
}

impl RawPlot {
    fn plotname(self) -> &'static str {
        match self {
            RawPlot::OperatingPoint => "Operating Point",
            RawPlot::Transient => "Transient Analysis",
            RawPlot::Dc => "DC transfer characteristic",
            RawPlot::Ac => "AC Analysis",
        }
    }
}

/// `I(...)` is a current; everything else is a voltage.
fn var_type(label: &str) -> &'static str {
    match label.trim_start().chars().next() {
        Some('i') | Some('I') => "current",
        _ => "voltage",
    }
}

/// `%.15e` with a signed, two-digit-minimum exponent (`1.000000000000000e+02`).
fn fmt_e(x: f64) -> String {
    let raw = format!("{x:.15e}");
    match raw.split_once('e') {
        Some((mant, exp)) => {
            let e: i32 = exp.parse().unwrap_or(0);
            let sign = if e < 0 { '-' } else { '+' };
            format!("{mant}e{sign}{:02}", e.abs())
        }
        None => raw,
    }
}

/// Ordered `(name, type)` variables (scale first when the plot has one) and the
/// `values[point][var] = (re, im)` matrix.
struct RawTable {
    complex: bool,
    vars: Vec<(String, &'static str)>,
    values: Vec<Vec<(f64, f64)>>,
}

fn build_table(out: &SimOutput, plot: RawPlot) -> RawTable {
    let real_rows = |skip_scale: bool| -> Vec<Vec<(f64, f64)>> {
        out.rows
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let mut row = Vec::with_capacity(r.len() + 1);
                if skip_scale {
                    row.push((
                        out.time.as_ref().expect("transient has a time axis")[i],
                        0.0,
                    ));
                }
                row.extend(r.iter().map(|&v| (v, 0.0)));
                row
            })
            .collect()
    };
    let probe_vars = |cols: &[String]| -> Vec<(String, &'static str)> {
        cols.iter().map(|c| (c.clone(), var_type(c))).collect()
    };
    match plot {
        RawPlot::OperatingPoint => RawTable {
            complex: false,
            vars: probe_vars(&out.columns),
            values: real_rows(false),
        },
        RawPlot::Transient => {
            let mut vars = vec![("time".to_string(), "time")];
            vars.extend(probe_vars(&out.columns));
            RawTable {
                complex: false,
                vars,
                values: real_rows(true),
            }
        }
        RawPlot::Dc => {
            // Column 0 is the swept source; ngspice names the scale by its kind.
            let scale = match out.columns.first().and_then(|c| c.chars().next()) {
                Some('i') | Some('I') => ("i-sweep", "current"),
                _ => ("v-sweep", "voltage"),
            };
            let mut vars = vec![(scale.0.to_string(), scale.1)];
            vars.extend(probe_vars(&out.columns[1.min(out.columns.len())..]));
            RawTable {
                complex: false,
                vars,
                values: real_rows(false),
            }
        }
        RawPlot::Ac => {
            // Columns are frequency then (mag, phase_deg) pairs; fold each pair
            // back into a complex phasor.
            let mut vars = vec![("frequency".to_string(), "frequency")];
            let mag_cols: Vec<usize> = (1..out.columns.len()).step_by(2).collect();
            vars.extend(
                mag_cols
                    .iter()
                    .map(|&j| (out.columns[j].clone(), var_type(&out.columns[j]))),
            );
            let values = out
                .rows
                .iter()
                .map(|r| {
                    let mut row = vec![(r[0], 0.0)];
                    for &j in &mag_cols {
                        let (mag, ph) = (r[j], r[j + 1].to_radians());
                        row.push((mag * ph.cos(), mag * ph.sin()));
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

/// Serialize a [`SimOutput`] as an ngspice ASCII rawfile titled `title` (the
/// deck's first line). Deterministic: same input, same bytes.
pub fn write_ascii_rawfile(out: &SimOutput, plot: RawPlot, title: &str) -> String {
    let table = build_table(out, plot);
    let flags = if table.complex { "complex" } else { "real" };
    let mut s = format!(
        "Title: {title}\nDate: {RAW_DATE}\nCommand: {RAW_COMMAND}\nPlotname: {}\nFlags: {flags}\n\
         No. Variables: {}\nNo. Points: {}\nVariables:\n",
        plot.plotname(),
        table.vars.len(),
        table.values.len()
    );
    for (i, (name, ty)) in table.vars.iter().enumerate() {
        s.push_str(&format!("\t{i}\t{name}\t{ty}\n"));
    }
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
            error_budget: crate::sim::error_budget(&crate::SolverOptions::default()).unwrap(),
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
        assert!(
            raw.contains("Command: hauksbee sim (ngspice-compatible rawfile)\n"),
            "{raw}"
        );
        assert!(raw.contains("Plotname: Transient Analysis\n"), "{raw}");
        assert!(raw.contains("Flags: real\n"));
        assert!(raw.contains("No. Variables: 2\n"));
        assert!(raw.contains("No. Points: 3\n"));
        assert!(raw.contains("\t0\ttime\ttime\n"));
        assert!(raw.contains("\t1\tV(out)\tvoltage\n"));
        assert!(raw.contains(" 0\t0.000000000000000e+00\n\t0.000000000000000e+00\n"));
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
        // mag 1, phase -90 deg -> (0, -1).
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
        assert!(
            raw.contains(" 0\t1.000000000000000e+03,0.000000000000000e+00\n"),
            "{raw}"
        );
        assert!(
            raw.contains(",-1.000000000000000e+00\n"),
            "expected im=-1 phasor line in:\n{raw}"
        );
    }
}
