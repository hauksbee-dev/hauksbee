//! A 555's interval belongs to the BOARD, not to the card: the part is stamped
//! as a composite of primitives the solver already has, so RA, RB and C set the
//! timing by the SOLVE. These runs are what says the composite is that part and
//! not a plausible oscillator. Each one BRACKETS the first threshold crossing
//! of a classic astable powered on from rest -- `t = ln(3)*(RA+RB)*C`, with the
//! discharge that follows lasting `ln(2)*RB*C` -- by asserting that OUT is
//! still high just before it and already low just after, for two timing
//! capacitors an octave apart. A board with no timing capacitor must not answer
//! at all, and RESET held low must hold OUT at VOL.
//!
//! The bracket pins the first interval; a `toggle` rate over some twenty
//! periods then pins the free-running `f = 1/(ln2*(RA+2RB)*C)` for both
//! capacitors. Cycle-level agreement with ngspice is pinned separately by
//! `hauksbee-solve/tests/decks/ne555_astable.{cir,expect.toml}`.

use crate::support::{self, comp, pad};

/// The classic astable: RA from VCC to DISCH, RB from DISCH to THRES, the
/// timing cap on THRES, TRIG shorted to THRES (pins 2 and 6 on one net, which
/// is what makes it astable), and a load on OUT. `c_nf = None` leaves the
/// timing cap off.
fn board(c_nf: Option<f64>, reset_net: &str) -> String {
    // DIP-8: 1 GND, 2 TRIG, 3 OUT, 4 RESET, 5 CTRL, 6 THRES, 7 DISCH, 8 VCC.
    let u1 = pad(1, "GND")
        + &pad(2, "THRES")
        + &pad(3, "OUT")
        + &pad(4, reset_net)
        + &pad(5, "CTRL")
        + &pad(6, "THRES")
        + &pad(7, "DISCH")
        + &pad(8, "VCC");
    let mut parts = comp("U1", "Package_DIP:DIP-8_W7.62mm", "NE555", 20, u1);
    for (i, (r, val, a, b)) in [
        ("RA", Some("10k".to_string()), "VCC", "DISCH"),
        ("RB", Some("10k".to_string()), "DISCH", "THRES"),
        ("CT", c_nf.map(|c| format!("{c}n")), "THRES", "GND"),
        ("RL", Some("100k".to_string()), "OUT", "GND"),
    ]
    .into_iter()
    .enumerate()
    {
        let Some(val) = val else { continue };
        let lib = if r.starts_with('C') {
            "Capacitor_SMD:C_0805_2012Metric"
        } else {
            "Resistor_SMD:R_0805_2012Metric"
        };
        parts += &comp(r, lib, &val, 40 + i * 8, pad(1, a) + &pad(2, b));
    }
    let nets = ["VCC", "GND", "OUT", "THRES", "DISCH", "CTRL"]
        .map(|n| format!("net \"{n}\"\n"))
        .concat();
    format!("board version 20171130\nfn main {{\n{nets}{parts}}}\n")
}

/// Run one spec that ends at `end_ms` with one assertion on OUT and report
/// whether it passed. Whatever it answers, a bound 555 is never reported as an
/// unmodelled or open part: the composite drives OUT, so nothing may claim it
/// does not.
fn passes(case: &str, src: &str, end_ms: f64, assertion: &str) -> bool {
    let board_path = support::write_tmp("t.board", src);
    let spec = support::write_tmp(
        "t.toml",
        &format!(
            "name = \"{case}\"\nboard = {}\nduration_ms = {end_ms}\nframe_ms = 0.005\n\
             [[supply]]\nnet = \"VCC\"\nkind = \"bench\"\nvolts = 5.0\n\
             current_limit_a = 0.5\nr_out_ohms = 0.1\n\
             [[assert]]\nname = \"out\"\nnet = \"OUT\"\n{assertion}",
            support::toml_path(&board_path),
        ),
    );
    let r = hauksbee_ci::run(&support::run_config(spec)).expect("run the spec");
    assert!(
        !r.coverage_warnings.iter().any(|w| w.contains("U1")),
        "{case}: {:?}",
        r.coverage_warnings
    );
    support::row(&r, "out").passed
}

/// OUT's level over `[after_ms, end_ms]`: at VOH when `high`, else at VOL.
fn out_is(case: &str, src: &str, after_ms: f64, end_ms: f64, high: bool) -> bool {
    let (min, max) = if high { (2.5, 5.5) } else { (-0.1, 0.5) };
    let level = format!("kind = \"voltage\"\nafter_ms = {after_ms}\nmin = {min}\nmax = {max}\n");
    passes(case, src, end_ms, &level)
}

#[test]
fn a_555_times_its_own_board_and_refuses_what_it_cannot_time() {
    // Independent runs (`write_tmp` never reuses a path): one thread per capacitor.
    std::thread::scope(|s| {
        for c_nf in [10.0_f64, 4.7] {
            s.spawn(move || {
                // Powered on from rest, THRES charges through RA + RB from 0 to 2/3 VCC
                // (hence ln 3, not the ln 2 of every later cycle) and the discharge that
                // follows runs through RB alone. Both in ms, as the spec wants them.
                let te = 3.0_f64.ln() * 20e3 * c_nf * 1e-9 * 1e3;
                let tl = 2.0_f64.ln() * 10e3 * c_nf * 1e-9 * 1e3;
                let b = board(Some(c_nf), "VCC");
                // 10%/12% either side of the crossing: wide enough to absorb the
                // comparator's smoothing and the power-on ramp, far too narrow for a
                // frequency that came from anywhere but RA, RB and C (a 2x error in
                // either bound fails one of the two).
                assert!(
                    out_is(&format!("{c_nf}n charging"), &b, 0.2 * te, 0.9 * te, true),
                    "{c_nf}n: OUT left VOH before ln(3)*(RA+RB)*C = {te} ms"
                );
                assert!(
                    out_is(
                        &format!("{c_nf}n discharging"),
                        &b,
                        1.12 * te,
                        0.7_f64.mul_add(tl, te),
                        false
                    ),
                    "{c_nf}n: OUT was not at VOL through the discharge after {te} ms"
                );
                // Free-running, every cycle is ln(2)*(RA+2RB)*C. 3.5% = the card's own
                // timing_error_frac interval (0-3%, SLFS022's 3% max initial error) plus
                // the 0.5% the edge-TIMED rate can be out over these 22 periods; nothing
                // wider is justified now the long first charge and the partial period at
                // the end of the run both fall outside the span the rate is timed over.
                let f = 1.0 / (2.0_f64.ln() * 30e3 * c_nf * 1e-9);
                let rate = format!("kind = \"toggle\"\nfreq_hz = {f}\ntolerance = 0.035\n");
                assert!(
                    passes(&format!("{c_nf}n rate"), &b, 24e3 / f, &rate),
                    "{c_nf}n: OUT did not run at 1/(ln(2)*(RA+2RB)*C) = {f} Hz"
                );
            });
        }
        // RESET at ground outranks both comparators: OUT sits at VOL, all run.
        assert!(out_is(
            "reset held low",
            &board(Some(10.0), "GND"),
            0.05,
            0.3,
            false
        ));
        // No timing capacitor: THRES has nothing to hold it, so there is no
        // interval to report. This case exists to catch a timing that comes from
        // the model rather than from the board -- it must not answer VOH on the
        // schedule the 10 nF board answered on.
        assert!(!out_is(
            "no timing cap",
            &board(None, "VCC"),
            0.05,
            0.2,
            true
        ));
    });
}

/// A width, not a rate: the astable's HIGH time is the datasheet's
/// `ln(2)*(RA+RB)*C`, and `pulse_width` must read it off the same interpolated
/// edges the frequency path uses. A board with no timing capacitor has no pulse
/// at all, and that must REFUSE rather than report the run's length as a width.
#[test]
fn a_555_reports_its_high_time_as_a_width_and_refuses_when_there_is_no_pulse() {
    let c_nf = 10.0_f64;
    // Skip the first charge (to ln 3, not ln 2) so the pulse timed is a
    // free-running one; +-10% absorbs the comparator smoothing, as elsewhere.
    let t_high = 2.0_f64.ln() * 20e3 * c_nf * 1e-9 * 1e3;
    let after = 1.5 * 3.0_f64.ln() * 20e3 * c_nf * 1e-9 * 1e3;
    let width = format!(
        "kind = \"pulse_width\"\nafter_ms = {after}\nmin = {}\nmax = {}\n",
        t_high * 0.9,
        t_high * 1.1
    );
    assert!(
        passes("10n high width", &board(Some(c_nf), "VCC"), 2.0, &width),
        "OUT's high time is not ln(2)*(RA+RB)*C = {t_high} ms"
    );
    // RESET at ground holds OUT at VOL for the whole run: no high pulse exists,
    // so the check refuses and a refusal never passes.
    assert!(!passes(
        "no pulse",
        &board(Some(c_nf), "GND"),
        2.0,
        "kind = \"pulse_width\"\n"
    ));
}
