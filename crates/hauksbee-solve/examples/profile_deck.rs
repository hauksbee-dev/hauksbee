//! Bounded solver-only profile for one SPICE transient deck.
//!
//! This intentionally measures the already-parsed solver separately from the
//! process-level benchmark.  It is useful when a CLI timing looks close to
//! ngspice: `hauksbee` launch/parse/output work is then not mistaken for
//! Newton or sparse-factor cost.  The mode is an explicit experiment only;
//! no production defaults are changed.
//!
//! ```text
//! cargo run -p hauksbee-solve --release --example profile_deck -- \
//!   path/to/deck.cir interpreted 5 mem0
//! cargo run -p hauksbee-solve --release --example profile_deck -- \
//!   path/to/deck.cir planned 5
//! cargo run -p hauksbee-solve --release --example profile_deck -- \
//!   path/to/deck.cir bypass 5
//! ```

use std::time::Instant;

use hauksbee_ir::SpiceLoader;
use hauksbee_solve::{
    AssemblyMode, DcInit, Integration, NewtonBypass, SolverOptions, StepControl, Transient,
};

fn fnv1a(values: &[f64]) -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    for value in values {
        for byte in value.to_le_bytes() {
            h ^= u64::from(byte);
            h = h.wrapping_mul(0x100000001b3);
        }
    }
    h
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    let i = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[i]
}

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args
        .next()
        .expect("usage: profile_deck <deck.cir> <interpreted|planned|bypass> [reps]");
    let mode = args.next().unwrap_or_else(|| "interpreted".to_string());
    let reps: usize = args
        .next()
        .as_deref()
        .unwrap_or("5")
        .parse()
        .expect("reps must be a positive integer");
    let node_name = args.next().unwrap_or_else(|| "mem0".to_string());
    assert!(reps > 0, "reps must be positive");
    let deck = std::fs::read_to_string(&path).expect("read deck");
    let parse_start = Instant::now();
    let (circuit, directives) = SpiceLoader::load_with_directives(&deck).expect("parse deck");
    let parse_s = parse_start.elapsed().as_secs_f64();
    let td = directives.tran.expect("deck needs .tran");

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
    opts.integration = Integration::Trapezoidal;
    opts.step = StepControl::Adaptive {
        dt_initial: (td.tstep / 100.0).max(1e-15),
        dt_min: 1e-15,
        dt_max: td.tmax.unwrap_or(td.tstep).max(1e-15),
    };
    if directives.use_initial_conditions {
        opts.dc_init = DcInit::FromZero;
    }
    match mode.as_str() {
        "interpreted" => {}
        "planned" => opts.assembly = AssemblyMode::Planned,
        "bypass" => opts.newton_bypass = NewtonBypass::On,
        other => panic!("unknown mode {other}; use interpreted, planned, or bypass"),
    }

    let node = circuit
        .find_node(&node_name)
        .unwrap_or_else(|| panic!("deck has no node {node_name}"))
        .0 as usize;
    let reference = if mode != "interpreted" {
        let mut reference_opts = opts;
        reference_opts.assembly = AssemblyMode::Interpreted;
        reference_opts.newton_bypass = NewtonBypass::Off;
        Some(
            Transient::new(reference_opts)
                .run(&circuit, td.tstop)
                .expect("interpreted reference solve")
                .node_voltages[node]
                .clone(),
        )
    } else {
        None
    };
    let mut times = Vec::with_capacity(reps);
    let mut samples = 0usize;
    let mut hash = 0u64;
    let mut max_abs = 0.0f64;
    let mut rms_sum = 0.0f64;
    let mut abs_errors = Vec::new();
    for _ in 0..reps {
        let start = Instant::now();
        let output = match Transient::new(opts).run(&circuit, td.tstop) {
            Ok(output) => output,
            Err(error) => {
                println!(
                    "{{\"deck\":{:?},\"node\":{:?},\"mode\":{:?},\"parse_s\":{parse_s:.9},\"status\":\"failed\",\"error\":{:?}}}",
                    path, node_name, mode, error
                );
                return;
            }
        };
        let elapsed = start.elapsed().as_secs_f64();
        samples = output.time.len();
        let waveform = &output.node_voltages[node];
        hash = fnv1a(waveform);
        if let Some(reference) = &reference {
            let errors: Vec<f64> = waveform
                .iter()
                .zip(reference)
                .map(|(actual, expected)| (actual - expected).abs())
                .collect();
            max_abs = max_abs.max(errors.iter().copied().fold(0.0, f64::max));
            rms_sum = errors.iter().map(|e| e * e).sum::<f64>() / errors.len() as f64;
            abs_errors.extend(errors);
        }
        times.push(elapsed);
    }
    times.sort_by(f64::total_cmp);
    abs_errors.sort_by(f64::total_cmp);
    let (error_max_abs, error_p95_abs, error_rms_abs) = if reference.is_some() {
        (max_abs, percentile(&abs_errors, 0.95), rms_sum.sqrt())
    } else {
        (0.0, 0.0, 0.0)
    };
    println!(
        "{{\"deck\":{:?},\"node\":{:?},\"mode\":{:?},\"parse_s\":{parse_s:.9},\"status\":\"measured\",\"solver_median_s\":{:.9},\"solver_p95_s\":{:.9},\"accepted_samples\":{samples},\"waveform_fnv1a\":\"0x{hash:016x}\",\"reference_error_max_abs\":{error_max_abs:.9e},\"reference_error_p95_abs\":{error_p95_abs:.9e},\"reference_error_rms_abs\":{error_rms_abs:.9e}}}",
        path,
        node_name,
        mode,
        times[times.len() / 2],
        percentile(&times, 0.95),
    );
}
