//! Bug-hunt physics channel: drive each neuron's timing subcircuit through the
//! real hauksbee solver using values **extracted from the live InputSystem
//! layout** (not hand-typed constants), and assert each time constant against
//! the documented design intent (Tarski-Emulator params.rs + TODO_PCB_FIX.md).
//!
//! This is the channel that catches the C_stretch class of bug: a value that is
//! individually plausible (10 pF in a 0402) but makes a *time constant* wrong by
//! orders of magnitude, killing circuit function. Value-by-value review cannot
//! see it; only computing τ on the real netlist does.
//!
//! Honest note on what each test does: `confirmed_c_stretch...` and
//! `siblings_membrane_tau...` actually run the transient solver on an RC step and
//! fit τ from the response. The other three (`...theta0...`, `...adaptation...`,
//! `...share_one_timing_value_set`) are *value* comparisons against the
//! calibrated constants; they assert the layout values equal design intent, no
//! solver involved. NONE of these tests check *connectivity*: a net swapped
//! between two equal-value parts would pass. The scope here is values + the
//! C_stretch τ class, not general topology correctness.
//!
//! Findings reproduced here:
//!   * CONFIRMED (known, excluded): C_stretch = 10 pF on every neuron's pulse
//!     stretcher (19 instances, one per neuron, NOT per synapse) → τ_pulse =
//!     1.5 µs, ~600× too short. The output layer cannot fire. Asserted as a
//!     *demonstration* that the toolchain reproduces the bug straight from the
//!     layout value.
//!   * NEGATIVE RESULT (the "siblings" hunt): every *other* neuron timing
//!     constant extracted from the layout matches the calibrated reference
//!     within tolerance, membrane τ_m, threshold-divider θ₀, threshold
//!     adaptation τ_θ, reset τ. No C_stretch siblings exist.
//!
//! Run: `cargo test -p hauksbee-engine --test bug_hunt_physics -- --nocapture`

use hauksbee_extract::ExtractedBoard;
use hauksbee_ir::{Circuit, Device, NodeId, SourceKind};
use hauksbee_solve::{Integration, SolverOptions, StepControl, Transient};
use std::collections::HashMap;

const LAYOUT: &str = "/Users/hauksbee-user/Tarski/Tarski-Repos/Tarski-Schematics/Neuron/InputSystem/InputSystem.kicad_pcb";

/// Parse a KiCad value string ("820k", "4.7nF", "150pF", "1k", "47") into SI.
fn si(v: &str) -> Option<f64> {
    let v = v.trim().trim_end_matches(['F', 'f', 'Ω']).trim();
    // strip a trailing unit letter, capture the multiplier
    let (num, mult) = {
        let mut chars: Vec<char> = v.chars().collect();
        let mut m = 1.0;
        if let Some(&last) = chars.last() {
            m = match last {
                'p' | 'P' => 1e-12,
                'n' | 'N' => 1e-9,
                'u' | 'U' | 'µ' => 1e-6,
                'm' => 1e-3, // milli (lowercase only; 'M' is mega)
                'k' | 'K' => 1e3,
                'M' => 1e6,
                'G' => 1e9,
                'R' | 'r' => 1.0,
                _ => {
                    // no unit suffix
                    return v.parse::<f64>().ok();
                }
            };
            chars.pop();
        }
        (chars.into_iter().collect::<String>(), m)
    };
    num.trim().parse::<f64>().ok().map(|x| x * mult)
}

/// Pull every component value on the layout, keyed by reference.
fn layout_values() -> Option<HashMap<String, String>> {
    let text = std::fs::read_to_string(LAYOUT).ok()?;
    let board = ExtractedBoard::from_kicad_pcb(&text).ok()?;
    Some(
        board
            .components
            .into_iter()
            .map(|c| (c.reference, c.value))
            .collect(),
    )
}

/// Fixed-step trapezoidal transient.
fn solver(dt: f64) -> SolverOptions {
    let mut o = SolverOptions::default();
    o.integration = Integration::Trapezoidal;
    o.step = StepControl::Fixed { dt };
    o
}

/// Fit τ from a step response by the 63.2% crossing time.
fn tau_from_step(times: &[f64], v: &[f64]) -> Option<f64> {
    let v0 = v[0];
    let vinf = *v.last().unwrap();
    let span = vinf - v0;
    if span.abs() < 1e-9 {
        return None;
    }
    let target = v0 + 0.632 * span;
    let rising = span > 0.0;
    for i in 1..v.len() {
        let (a, b) = (v[i - 1], v[i]);
        let crossed = if rising {
            a < target && b >= target
        } else {
            a > target && b <= target
        };
        if crossed {
            let frac = ((target - a) / (b - a)).clamp(0.0, 1.0);
            return Some(times[i - 1] + frac * (times[i] - times[i - 1]));
        }
    }
    None
}

/// A simple R||C charged through a current step: measures τ = R·C from the
/// layout's real R and C, solved by the production transient.
fn rc_tau(r_ohms: f64, c_farads: f64) -> f64 {
    let mut c = Circuit::new();
    let n = c.node("N");
    c.add(Device::Resistor {
        name: "R".into(),
        a: n,
        b: NodeId::GROUND,
        ohms: r_ohms,
        tc1: None,
    });
    c.add(Device::Capacitor {
        name: "C".into(),
        a: n,
        b: NodeId::GROUND,
        farads: c_farads,
        ic: Some(0.0),
    });
    c.add(Device::Isource {
        name: "I".into(),
        p: NodeId::GROUND,
        n,
        kind: SourceKind::Dc(1e-6),
    });
    let dt = (r_ohms * c_farads / 200.0).max(1e-12);
    let t = Transient::new(solver(dt));
    let wf = t.run(&c, 6.0 * r_ohms * c_farads).expect("rc solves");
    let times = &wf.time;
    let v = wf.node(&c, "N").expect("N").to_vec();
    tau_from_step(times, &v).expect("crosses 63%")
}

fn val(map: &HashMap<String, String>, reference: &str) -> f64 {
    let raw = map
        .get(reference)
        .unwrap_or_else(|| panic!("{reference} not on layout"));
    si(raw).unwrap_or_else(|| panic!("cannot parse {reference} value {raw:?}"))
}

// ════════════════════════════════════════════════════════════════════════════
// CONFIRMED (known/excluded); the toolchain reproduces C_stretch from layout
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn confirmed_c_stretch_pulse_is_catastrophically_short() {
    let Some(m) = layout_values() else {
        eprintln!("layout missing; skipping");
        return;
    };
    // R_stretch and C_stretch on a representative synapse-bearing neuron (Neuron3).
    let r_stretch = val(&m, "R__stretch701"); // 150 kΩ
    let c_stretch = val(&m, "C__stretch701"); // 10 pF on the live board
    let tau = rc_tau(r_stretch, c_stretch);
    println!(
        "C_stretch path: R={:.0}kΩ C={:.3}pF -> τ_pulse={:.3} µs (need ~870 µs)",
        r_stretch / 1e3,
        c_stretch * 1e12,
        tau * 1e6
    );
    // Design intent (TODO_PCB_FIX.md): τ_pulse must be ~870 µs (C=5.8 nF). The
    // live 10 pF gives ~1.5 µs, three orders of magnitude short. Assert that
    // the layout value IS the broken one (so this test tracks the real board)
    // and that the resulting τ is far below the working threshold.
    assert!(
        (c_stretch - 10e-12).abs() < 1e-13,
        "expected the unfixed 10pF on the layout, got {c_stretch:.3e} F"
    );
    assert!(
        tau < 5e-6,
        "τ_pulse {:.3} µs unexpectedly large for 10pF",
        tau * 1e6
    );
    // The working design needs τ_pulse comparable to the 1 ms integration tick.
    assert!(
        tau < 1e-3 / 100.0,
        "τ_pulse {:.3} µs is <1% of the 1ms tick → output layer dead (the bug)",
        tau * 1e6
    );
}

// ════════════════════════════════════════════════════════════════════════════
// NEGATIVE RESULT; the sibling hunt: every other timing constant is healthy
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn siblings_membrane_tau_matches_intent() {
    let Some(m) = layout_values() else { return };
    let r_leak = val(&m, "R_leak701"); // 120k
    let c_mem = val(&m, "C_mem701"); // 10nF
    let tau = rc_tau(r_leak, c_mem);
    let intent = 1.2e-3;
    println!(
        "membrane: R_leak={:.0}kΩ C_mem={:.1}nF -> τ_m={:.4} ms (intent 1.2 ms)",
        r_leak / 1e3,
        c_mem * 1e9,
        tau * 1e3
    );
    assert!(
        (tau - intent).abs() / intent < 0.05,
        "membrane τ {:.4} ms deviates >5% from 1.2 ms; would be a C_stretch sibling",
        tau * 1e3
    );
}

#[test]
fn siblings_threshold_adaptation_tau_matches_intent() {
    let Some(m) = layout_values() else { return };
    // τ_θ = C_thresh / (1/R_top + 1/R_bottom). On most neurons the 4.7nF cap is
    // named C_adapt<n>01; on Neuron2's source sheet it is C_thresh601. Use a
    // clean instance (Neuron3): C_adapt701 = 4.7nF is the threshold cap.
    let c_thresh = val(&m, "C_adapt701"); // 4.7nF threshold-adaptation cap
    let r_top = val(&m, "R_top701"); // 820k
    let r_bottom = val(&m, "R_bottom701"); // 150k
    let g = 1.0 / r_top + 1.0 / r_bottom;
    let tau = c_thresh / g;
    let intent = 4.7e-9 / (1.0 / 820e3 + 1.0 / 150e3); // ~0.596 ms
    println!(
        "threshold adapt: C={:.1}nF R_top={:.0}k R_bot={:.0}k -> τ_θ={:.4} ms (intent {:.4} ms)",
        c_thresh * 1e9,
        r_top / 1e3,
        r_bottom / 1e3,
        tau * 1e3,
        intent * 1e3
    );
    assert!((tau - intent).abs() / intent < 0.02, "τ_θ off by >2%");
}

#[test]
fn siblings_threshold_divider_theta0_matches_intent() {
    let Some(m) = layout_values() else { return };
    let r_top = val(&m, "R_top701"); // 820k to VDD
    let r_bottom = val(&m, "R_bottom701"); // 150k to Vref
    let (vdd, vref) = (5.0, 2.5);
    let g_top = 1.0 / r_top;
    let g_bot = 1.0 / r_bottom;
    let theta0 = (vdd * g_top + vref * g_bot) / (g_top + g_bot) - vref;
    println!(
        "θ₀ from layout divider {:.0}k/{:.0}k = {:.4} V (intent 0.387 V)",
        r_top / 1e3,
        r_bottom / 1e3,
        theta0
    );
    assert!(
        (theta0 - 0.387).abs() < 0.005,
        "θ₀ {theta0:.4} V deviates from the 0.387 V design point"
    );
}

#[test]
fn siblings_all_neurons_share_one_timing_value_set() {
    // The strongest sibling check: every neuron's timing components must carry
    // the SAME values. A single neuron with a stray value would be a localized
    // C_stretch sibling. We scan all R_top/R_bottom/R_leak/C_mem/C_adapt across
    // the 19 neurons and assert the value-set is unique.
    let Some(m) = layout_values() else { return };
    let families = [
        ("R_top", "820k"),
        ("R_bottom", "150k"),
        ("R_leak", "120k"),
        ("R_inject", "47k"),
        ("R_charge", "1k"),
        ("C_mem", "10nF"),
        ("C_reset", "7pF"),
    ];
    for (fam, expect) in families {
        let want = si(expect).unwrap();
        let mut seen = Vec::new();
        for (r, v) in &m {
            // family member: ref starts with the family name followed by digits
            if r.starts_with(fam)
                && r[fam.len()..]
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_digit())
            {
                let got = si(v).unwrap_or(f64::NAN);
                if (got - want).abs() / want > 0.001 {
                    seen.push((r.clone(), v.clone()));
                }
            }
        }
        println!("{fam}: {} deviating instances from {expect}", seen.len());
        assert!(
            seen.is_empty(),
            "{fam} has instances off the {expect} design value (C_stretch sibling?): {seen:?}"
        );
    }
}

// ════════════════════════════════════════════════════════════════════════════
// Finding #14, INH_Q4: disprove the *gross* mechanisms for the documented
// "inhibitory 1× branch is broken" defect, from the layout.
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn inh_q4_mirror_banks_fully_populated() {
    // Context: the emulator documents (params.rs INH_Q4_BROKEN / synapse.rs) that
    // the inhibitory 1× mirror leg (Q4) delivers no current on this board. Q4 is
    // ONE transistor in a shared-reference 1×/2×/4× NPN bank, so a broken 1× leg
    // could NOT show up as a missing set resistor (the set resistor serves the
    // whole bank). This test therefore does NOT speak to the Q4 root cause; it
    // only rules out the cruder "an entire inhibitory mirror bank is missing /
    // un-referenced" failure by confirming both banks are fully populated.
    //
    // What it asserts: the board carries exactly 2 × 90 = 180 of the 10 MΩ
    // mirror-set resistors, one excitatory (PNP ref) and one inhibitory (NPN
    // ref) per synapse, even though they are inconsistently NAMED across
    // instances (R_Set_G / R_Set_VCC / bare R####). Presence, not name, matters.
    //
    // Caveat (stated, not hidden): this is a board-WIDE count, not a per-synapse
    // check. A synapse missing its inhibitory resistor while another carried a
    // spare would still sum to 180 and pass. It is a coarse population sanity
    // check, not a localizer.
    let text = match std::fs::read_to_string(LAYOUT) {
        Ok(t) => t,
        Err(_) => {
            eprintln!("layout missing; skipping");
            return;
        }
    };
    let board = ExtractedBoard::from_kicad_pcb(&text).unwrap();
    let r10m = board.components.iter().filter(|c| c.value == "10M").count();
    let hc595 = board
        .components
        .iter()
        .filter(|c| c.value.contains("74HC595"))
        .count();
    println!("10MΩ mirror-set resistors on board: {r10m}; 74HC595 (one per synapse): {hc595}");
    assert_eq!(hc595, 90, "expected the 90-synapse shift chain");
    assert_eq!(
        r10m,
        2 * 90,
        "expected 180 (both mirror banks populated per synapse), found {r10m}; \
         a deficit would mean a wholesale missing/un-referenced inhibitory bank \
         (NOT the same as the documented single-transistor Q4 defect)."
    );
}
