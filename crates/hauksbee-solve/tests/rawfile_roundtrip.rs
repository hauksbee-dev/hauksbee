//! Round-trip proof: hauksbee's ASCII rawfiles are read back by a real
//! third-party reader (Python `spicelib`, the SPICE-compat plan's own named
//! example) and the variable names, point counts, and spot values match the
//! [`SimOutput`] we wrote (SPICE-compat plan step 14).
//!
//! This mirrors the ngspice harness discipline: it SKIPS (never fails) when the
//! external tool is absent, so a contributor without `uv`/`spicelib` is not
//! blocked. When `uv` is present it shells out to a one-shot Python script that
//! parses each rawfile with `spicelib.RawRead` and asserts the values, exiting
//! nonzero on any mismatch — a failure here is a real format bug.

use hauksbee_solve::{write_ascii_rawfile, RawPlot, SimOutput};
use std::process::Command;

/// Locate `uv` on PATH (the project's Python runner). `None` => skip.
fn find_uv() -> Option<std::path::PathBuf> {
    let exe = if cfg!(windows) { "uv.exe" } else { "uv" };
    let path = std::env::var("PATH").ok()?;
    for dir in std::env::split_paths(&path) {
        let cand = dir.join(exe);
        if cand.is_file() {
            return Some(cand);
        }
    }
    for p in ["/opt/homebrew/bin/uv", "/usr/local/bin/uv", "/usr/bin/uv"] {
        let pb = std::path::PathBuf::from(p);
        if pb.is_file() {
            return Some(pb);
        }
    }
    None
}

fn out(columns: &[&str], time: Option<Vec<f64>>, rows: Vec<Vec<f64>>) -> SimOutput {
    SimOutput {
        columns: columns.iter().map(|s| s.to_string()).collect(),
        time,
        rows,
    }
}

/// Write `raw` to a temp file, run the verifier `py` under `uv run --with
/// spicelib`, and assert it exits 0. `py` receives the rawfile path as argv[1].
fn spicelib_verify(uv: &std::path::Path, name: &str, raw: &str, py: &str) {
    let dir = std::env::temp_dir().join(format!("hauksbee_raw_{name}_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let raw_path = dir.join(format!("{name}.raw"));
    std::fs::write(&raw_path, raw).unwrap();

    let output = Command::new(uv)
        .args(["run", "--with", "spicelib", "python", "-c", py])
        .arg(&raw_path)
        .output()
        .expect("failed to launch uv");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "spicelib round-trip [{name}] failed:\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn spicelib_reads_all_four_analysis_types() {
    let Some(uv) = find_uv() else {
        eprintln!("SKIP: `uv` not found on PATH; spicelib round-trip not exercised.");
        return;
    };

    // --- Operating point: no scale variable, one point ----------------------
    let op = out(&["V(in)", "V(out)"], None, vec![vec![5.0, 3.75]]);
    let raw = write_ascii_rawfile(&op, RawPlot::OperatingPoint, "divider op");
    spicelib_verify(
        &uv,
        "op",
        &raw,
        r#"
import sys
from spicelib import RawRead
r = RawRead(sys.argv[1])
names = r.get_trace_names()
assert set(names) >= {"V(in)", "V(out)"}, names
assert r.get_trace("V(in)").get_wave().size == 1
assert abs(r.get_trace("V(in)").get_wave()[0] - 5.0) < 1e-12
assert abs(r.get_trace("V(out)").get_wave()[0] - 3.75) < 1e-12
"#,
    );

    // --- Transient: time scale ---------------------------------------------
    let tran = out(
        &["V(out)"],
        Some(vec![0.0, 1e-6, 2e-6]),
        vec![vec![0.0], vec![2.5], vec![5.0]],
    );
    let raw = write_ascii_rawfile(&tran, RawPlot::Transient, "rc tran");
    spicelib_verify(
        &uv,
        "tran",
        &raw,
        r#"
import sys
from spicelib import RawRead
r = RawRead(sys.argv[1])
t = r.get_axis()
v = r.get_trace("V(out)").get_wave()
assert len(t) == 3, len(t)
assert abs(t[0] - 0.0) < 1e-18 and abs(t[-1] - 2e-6) < 1e-15, (t[0], t[-1])
assert abs(v[0] - 0.0) < 1e-12 and abs(v[-1] - 5.0) < 1e-12, (v[0], v[-1])
"#,
    );

    // --- DC sweep: v-sweep scale -------------------------------------------
    let dc = out(
        &["Vin", "V(d)"],
        None,
        vec![vec![0.0, 0.0], vec![0.1, 0.1], vec![0.2, 0.2]],
    );
    let raw = write_ascii_rawfile(&dc, RawPlot::Dc, "diode dc");
    spicelib_verify(
        &uv,
        "dc",
        &raw,
        r#"
import sys
from spicelib import RawRead
r = RawRead(sys.argv[1])
x = r.get_axis()
vd = r.get_trace("V(d)").get_wave()
assert "V(d)" in r.get_trace_names()
assert len(x) == 3, len(x)
assert abs(x[0] - 0.0) < 1e-12 and abs(x[-1] - 0.2) < 1e-12, (x[0], x[-1])
assert abs(float(vd[-1].real if hasattr(vd[-1],"real") else vd[-1]) - 0.2) < 1e-12
"#,
    );

    // --- AC: complex, frequency scale, mag/phase folded to re/im ------------
    // mag 1 @ phase -90deg -> (0, -1); mag 2 @ phase 0 -> (2, 0).
    let ac = out(
        &["frequency", "V(out)", "V(out):phase_deg"],
        None,
        vec![vec![100.0, 1.0, -90.0], vec![1000.0, 2.0, 0.0]],
    );
    let raw = write_ascii_rawfile(&ac, RawPlot::Ac, "rc ac");
    spicelib_verify(
        &uv,
        "ac",
        &raw,
        r#"
import sys, math, cmath
from spicelib import RawRead
r = RawRead(sys.argv[1])
f = r.get_axis()
v = r.get_trace("V(out)").get_wave()
assert len(f) == 2, len(f)
assert abs(abs(f[0]) - 100.0) < 1e-6 and abs(abs(f[-1]) - 1000.0) < 1e-6
# point 0: magnitude 1, phase -90 deg
assert abs(abs(v[0]) - 1.0) < 1e-9, abs(v[0])
assert abs(math.degrees(cmath.phase(v[0])) - (-90.0)) < 1e-6, math.degrees(cmath.phase(v[0]))
# point 1: magnitude 2, phase 0
assert abs(abs(v[1]) - 2.0) < 1e-9, abs(v[1])
assert abs(math.degrees(cmath.phase(v[1])) - 0.0) < 1e-6
"#,
    );

    eprintln!("spicelib round-trip: op / tran / dc / ac all verified.");
}
