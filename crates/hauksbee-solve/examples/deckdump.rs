//! Dump a deck's transient probe waveforms as CSV, for hand comparison against
//! ngspice's `.print tran` table.
//!
//! `cargo run --release --example deckdump -- <deck.cir> <probe> [probe ...]`

use hauksbee_ir::SpiceLoader;
use hauksbee_solve::{run_tran, DcInit, Integration, Probe, SolverOptions, StepControl};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let path = &args[0];
    let deck = std::fs::read_to_string(path).unwrap();
    let (circuit, directives) = SpiceLoader::load_with_directives(&deck).unwrap();

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
    let td = directives.tran.expect("tran deck");
    let dt_max = td.tmax.unwrap_or(td.tstep).max(1e-15);
    opts.integration = Integration::Trapezoidal;
    opts.step = StepControl::Adaptive {
        dt_initial: (td.tstep / 100.0).max(1e-15),
        dt_min: 1e-15,
        dt_max,
    };
    if directives.use_initial_conditions {
        opts.dc_init = DcInit::FromZero;
    }
    if std::env::var("SMOOTH_SWITCH").is_ok() {
        opts.effects.switch_model = hauksbee_solve::SwitchModel::Smooth;
    }
    if let Ok(f) = std::env::var("SWITCH_FRAC") {
        opts.effects.switch_transition_frac = f.parse().unwrap();
    }

    let probes: Vec<Probe> = args[1..].iter().map(|s| Probe::parse(s).unwrap()).collect();
    let out = run_tran(&circuit, &opts, td.tstop, &probes).unwrap();
    let t = out.time.clone().unwrap();
    let cols: Vec<Vec<f64>> = probes
        .iter()
        .map(|p| out.column(&p.label()).unwrap())
        .collect();
    print!("time");
    for p in &probes {
        print!(",{}", p.label());
    }
    println!();
    for (i, &ti) in t.iter().enumerate() {
        print!("{ti:.9e}");
        for c in &cols {
            print!(",{:.9e}", c[i]);
        }
        println!();
    }
}
