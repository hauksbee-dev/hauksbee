//! Dump a deck's operating point for the named probes.
//!
//! `cargo run --release --example opdump -- <deck.cir> <probe> [probe ...]`

use hauksbee_ir::SpiceLoader;
use hauksbee_solve::{run_op, Probe, SolverOptions, SwitchModel};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let deck = std::fs::read_to_string(&args[0]).unwrap();
    let (circuit, directives) = SpiceLoader::load_with_directives(&deck).unwrap();
    let mut opts = SolverOptions::default();
    if let Some(r) = directives.reltol {
        opts.reltol = r;
    }
    if std::env::var("SMOOTH_SWITCH").is_ok() {
        opts.effects.switch_model = SwitchModel::Smooth;
    }
    let probes: Vec<Probe> = args[1..].iter().map(|s| Probe::parse(s).unwrap()).collect();
    match run_op(&circuit, &opts, &probes) {
        Ok(out) => {
            for p in &probes {
                println!("{} = {:.9e}", p.label(), out.column(&p.label()).unwrap()[0]);
            }
        }
        Err(e) => println!("REFUSED: {e}"),
    }
}
