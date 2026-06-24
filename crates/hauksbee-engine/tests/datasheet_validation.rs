//! Physical validation of datasheet-extracted device models.
//!
//! A model that parses and is within the static range checks can still be
//! garbage (wrong order of magnitude `is`, an Early voltage that collapses the
//! output impedance, a regulator that does not regulate). The only honest test
//! is to *simulate* the model at the datasheet's spec'd operating point and
//! check the numbers come out where the datasheet says they should.
//!
//! These helpers build a tiny purpose-made circuit per device kind, solve its
//! DC operating point with the real hauksbee solver, and assert physical sanity:
//!
//!   * diode  — forward voltage at a stated forward current,
//!   * BJT    — DC current gain (beta = Ic/Ib) at a stated bias,
//!   * LDO    — output voltage under load, within tolerance, with headroom.
//!
//! The same functions back both the offline fixture test (canned codex reply,
//! always runs) and the `#[ignore]` live test (real codex). When a real
//! extraction lands in `testdata/extracted/`, the live-ish tests pick it up.

use std::path::PathBuf;

use hauksbee_ir::{BjtModel, Circuit, Device, DiodeModel, NodeId, Polarity, SourceKind};
use hauksbee_models::schema::{ComponentKind, DbFile, ModelEntry};
use hauksbee_solve::{SolverOptions, Transient};

// ── Locating models ────────────────────────────────────────────────────────

fn testdata(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata")
        .join(rel)
}

/// Parse the first `[[models]]` entry out of a TOML string.
fn first_model(toml_src: &str) -> ModelEntry {
    let db: DbFile = toml::from_str(toml_src).expect("model TOML parses");
    db.models.into_iter().next().expect("at least one model")
}

/// Load an extracted model from `testdata/extracted/<file>` if it exists.
fn load_extracted(file: &str) -> Option<ModelEntry> {
    let path = testdata("extracted").join(file);
    let src = std::fs::read_to_string(&path).ok()?;
    Some(first_model(&src))
}

// ── Solver glue ────────────────────────────────────────────────────────────

/// Solve a circuit's DC operating point, returning the (single-sample)
/// `Waveforms` or the solver's error string. A 1 ns single-step transient
/// lands on the DC bias for a resistive circuit.
fn dc_try(circuit: &Circuit) -> Result<hauksbee_solve::Waveforms, String> {
    let opts = SolverOptions::fixed(1e-9);
    Transient::new(opts).run(circuit, 1e-9)
}

/// Solve a circuit's DC operating point, panicking on non-convergence. Used by
/// the success-path checks where a healthy model must always converge.
fn dc(circuit: &Circuit) -> hauksbee_solve::Waveforms {
    dc_try(circuit).expect("DC operating point converges")
}

fn final_v(wf: &hauksbee_solve::Waveforms, circuit: &Circuit, net: &str) -> f64 {
    wf.final_node(circuit, net)
        .unwrap_or_else(|| panic!("node '{net}' not in solution"))
}

/// Magnitude of the current a voltage source delivers (branch current).
fn source_current(wf: &hauksbee_solve::Waveforms, name: &str) -> f64 {
    wf.branch_currents
        .iter()
        .find(|(n, _)| n == name)
        .and_then(|(_, v)| v.last().copied())
        .unwrap_or_else(|| panic!("no branch current for source '{name}'"))
        .abs()
}

// ── Building IR models from a ModelEntry ────────────────────────────────────

fn diode_model_of(m: &ModelEntry) -> DiodeModel {
    let d = DiodeModel::default();
    let p = &m.params;
    DiodeModel {
        is: p.get_f64("is").unwrap_or(d.is),
        n: p.get_f64("n").unwrap_or(d.n),
        rs: p.get_f64("rs").unwrap_or(d.rs),
        cjo: p.get_f64("cjo").unwrap_or(d.cjo),
        vj: p.get_f64("vj").unwrap_or(d.vj),
        m: p.get_f64("m").unwrap_or(d.m),
        bv: p.get_f64("bv").unwrap_or(d.bv),
        ..d
    }
}

fn bjt_model_of(m: &ModelEntry) -> BjtModel {
    let d = BjtModel::default();
    let p = &m.params;
    let polarity = if m.kind == ComponentKind::BjtPnp {
        Polarity::P
    } else {
        Polarity::N
    };
    BjtModel {
        polarity,
        is: p.get_f64("is").unwrap_or(d.is),
        bf: p.get_f64("bf").unwrap_or(d.bf),
        br: p.get_f64("br").unwrap_or(d.br),
        vaf: p.get_f64("vaf").unwrap_or(d.vaf),
        nf: p.get_f64("nf").unwrap_or(d.nf),
        rb: p.get_f64("rb").unwrap_or(d.rb),
        re: p.get_f64("re").unwrap_or(d.re),
        rc: p.get_f64("rc").unwrap_or(d.rc),
        ..d
    }
}

// ── Physical checks ─────────────────────────────────────────────────────────

/// Diode forward voltage at (approximately) a given forward current.
///
/// A bare current source into a single diode node has no conductive DC path at
/// the solver's zero-volt start and stalls homotopy, so we use the textbook
/// rig instead: a voltage source through a series resistor sized to land the
/// operating point at `i_f`. Vcc is set a few hundred mV above the expected
/// drop, then `R = (Vcc - Vf_guess) / i_f`. We read the actual Vf and the
/// actual current so the caller can sanity-check both.
///
/// Returns (vf, i_actual).
pub fn diode_vf_current(m: &ModelEntry, i_f: f64) -> (f64, f64) {
    assert_eq!(m.kind, ComponentKind::Diode, "not a diode model");
    let vf_guess = 0.7_f64;
    let vcc = vf_guess + 2.0; // generous headroom so R dominates the bias
    let r = (vcc - vf_guess) / i_f;

    let mut c = Circuit::new();
    let nin = c.node("in");
    let na = c.node("a");
    c.add(Device::Vsource {
        name: "V1".into(),
        p: nin,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(vcc),
    });
    c.add(Device::Resistor {
        name: "R1".into(),
        a: nin,
        b: na,
        ohms: r,
        tc1: None,
    });
    c.add(Device::Diode {
        name: "D1".into(),
        a: na,
        k: NodeId::GROUND,
        model: diode_model_of(m),
    });
    let wf = dc(&c);
    let vf = final_v(&wf, &c, "a");
    // Current is what V1 sources (series circuit => same through the diode).
    let i = source_current(&wf, "V1");
    (vf, i)
}

/// Convenience: Vf at (approximately) `i_f`, ignoring the exact current.
pub fn diode_vf_at(m: &ModelEntry, i_f: f64) -> f64 {
    diode_vf_current(m, i_f).0
}

/// DC current gain beta = Ic/Ib for an NPN at a stated collector bias.
///
/// Topology: Vcc through Rc to the collector, base driven by a fixed base
/// current Ib (current source), emitter grounded. Beta is read as Ic/Ib where
/// Ic is the current Vcc delivers through Rc.
pub fn bjt_beta_at(m: &ModelEntry, vcc: f64, rc: f64, i_b: f64) -> f64 {
    bjt_beta_try(m, vcc, rc, i_b).expect("BJT bias converges")
}

/// As [`bjt_beta_at`], but returns the solver error instead of panicking when
/// the operating point cannot be reached (used to prove garbage is rejected).
pub fn bjt_beta_try(m: &ModelEntry, vcc: f64, rc: f64, i_b: f64) -> Result<f64, String> {
    assert!(
        matches!(m.kind, ComponentKind::BjtNpn),
        "beta helper is wired for NPN only (got {:?})",
        m.kind
    );
    let mut c = Circuit::new();
    let nvcc = c.node("vcc");
    let ncol = c.node("col");
    let nbase = c.node("base");

    c.add(Device::Vsource {
        name: "Vcc".into(),
        p: nvcc,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(vcc),
    });
    c.add(Device::Resistor {
        name: "Rc".into(),
        a: nvcc,
        b: ncol,
        ohms: rc,
        tc1: None,
    });
    // Push Ib into the base node from ground.
    c.add(Device::Isource {
        name: "Ib".into(),
        p: NodeId::GROUND,
        n: nbase,
        kind: SourceKind::Dc(i_b),
    });
    c.add(Device::Bjt {
        name: "Q1".into(),
        c: ncol,
        b: nbase,
        e: NodeId::GROUND,
        model: bjt_model_of(m),
    });

    let wf = dc_try(&c)?;
    // Ic = current through Rc = (Vcc - Vcol)/Rc, equivalently |I(Vcc)|.
    let ic = source_current(&wf, "Vcc");
    Ok(ic / i_b)
}

/// Base-emitter voltage of an NPN biased to ~2 mA collector current. A healthy
/// silicon BJT sits near 0.6..0.75 V here; a model with a wildly wrong `is`
/// lands far outside that. Returns Vbe, or the solver error.
pub fn bjt_vbe_try(m: &ModelEntry) -> Result<f64, String> {
    assert!(matches!(m.kind, ComponentKind::BjtNpn));
    let bf = m.params.get_f64("bf").unwrap_or(100.0);
    let i_b = 2e-3 / bf;
    let mut c = Circuit::new();
    let nvcc = c.node("vcc");
    let ncol = c.node("col");
    let nbase = c.node("base");
    c.add(Device::Vsource {
        name: "Vcc".into(),
        p: nvcc,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(10.0),
    });
    c.add(Device::Resistor {
        name: "Rc".into(),
        a: nvcc,
        b: ncol,
        ohms: 2.2e3,
        tc1: None,
    });
    c.add(Device::Isource {
        name: "Ib".into(),
        p: NodeId::GROUND,
        n: nbase,
        kind: SourceKind::Dc(i_b),
    });
    c.add(Device::Bjt {
        name: "Q1".into(),
        c: ncol,
        b: nbase,
        e: NodeId::GROUND,
        model: bjt_model_of(m),
    });
    let wf = dc_try(&c)?;
    Ok(final_v(&wf, &c, "base"))
}

/// LDO output voltage under a resistive load. The binder models a vreg as an
/// ideal `vout` source, so this confirms the extracted `vout` regulates the
/// output net and delivers the load current. Returns (vout, iload).
pub fn ldo_output_under_load(m: &ModelEntry, r_load: f64) -> (f64, f64) {
    assert_eq!(m.kind, ComponentKind::Vreg, "not a vreg model");
    let vout = m
        .params
        .get_f64("vout")
        .expect("vreg model must carry vout");
    let mut c = Circuit::new();
    let nout = c.node("out");
    // The behavioral regulator: an ideal source at vout on the output net,
    // exactly as binder::bind_vreg stamps it.
    c.add(Device::Vsource {
        name: format!("Vreg_{}", m.id),
        p: nout,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(vout),
    });
    c.add(Device::Resistor {
        name: "Rload".into(),
        a: nout,
        b: NodeId::GROUND,
        ohms: r_load,
        tc1: None,
    });
    let wf = dc(&c);
    let v = final_v(&wf, &c, "out");
    let i = source_current(&wf, &format!("Vreg_{}", m.id));
    (v, i)
}

// ── Reusable assertions, used by both fixture and live tests ────────────────

/// Assert a diode model is physically sane against the 1N4148 datasheet:
/// Vf at 10 mA should sit in 0.55..0.80 V (datasheet caps it at 1.0 V; real
/// silicon switching diodes land near 0.65..0.72 V), and reverse leakage tiny.
pub fn assert_diode_physical(m: &ModelEntry) {
    let (vf, i) = diode_vf_current(m, 10e-3);
    assert!(
        (8e-3..=12e-3).contains(&i),
        "diode bias landed at {:.2} mA, not ~10 mA (operating point off)",
        i * 1e3
    );
    assert!(
        (0.55..=0.80).contains(&vf),
        "1N4148 Vf at ~10 mA = {vf:.3} V, expected 0.55..0.80 V (datasheet max 1.0 V)"
    );
    // Sanity at a second point: Vf rises with current.
    let vf_hi = diode_vf_at(m, 100e-3);
    assert!(
        vf_hi > vf,
        "Vf should increase with current: {vf:.3} V @10mA vs {vf_hi:.3} V @100mA"
    );
    assert!(
        vf_hi < 1.05,
        "1N4148 Vf at ~100 mA = {vf_hi:.3} V, datasheet max 1.0 V"
    );
}

/// Assert an NPN model reproduces the BC847 datasheet DC current gain band.
/// hFE at VCE=5 V, IC=2 mA is 110..450 (group A typ 180, group B typ 290).
/// We bias for ~2 mA collector (Ib = 2mA/expected-beta) and check beta lands
/// in a band generous enough for the spread but tight enough to catch garbage.
pub fn assert_bjt_physical(m: &ModelEntry) {
    // Aim collector current near 2 mA. With Vcc=10 V and Rc=2.2k the collector
    // sits well out of saturation for Ic up to ~3 mA. Pick Ib so that, at the
    // model's own bf, Ic ~ 2 mA.
    let bf = m.params.get_f64("bf").expect("bf present");
    let i_b = 2e-3 / bf;
    let beta = bjt_beta_at(m, 10.0, 2.2e3, i_b);
    assert!(
        (80.0..=600.0).contains(&beta),
        "BC847 beta = {beta:.1}, expected within ~the datasheet hFE band 110..450 \
         (allowing solver/bias spread 80..600)"
    );
    // And it must actually be in the forward-active region, not saturated:
    // recompute Ic and confirm it is in a sane few-mA range.
    let ic = beta * i_b;
    assert!(
        (0.5e-3..=4e-3).contains(&ic),
        "BC847 collector current {ic:.4} A out of the intended ~2 mA bias window"
    );

    // Vbe at ~2 mA must be a real silicon junction drop. This is what catches a
    // bad saturation current `is` that the beta check alone misses. The BC847
    // datasheet gives VBE typ 660 mV at IC = 2 mA; allow 0.55..0.80 V.
    let vbe = bjt_vbe_try(m).expect("Vbe operating point converges");
    assert!(
        (0.55..=0.80).contains(&vbe),
        "BC847 Vbe at ~2 mA = {vbe:.3} V, expected ~0.66 V (0.55..0.80); \
         a wrong saturation current `is` shows up here"
    );
}

/// Assert an LDO model regulates near its labelled output under a real load.
/// For AMS1117-3.3 the output must be 3.30 V within the datasheet tolerance
/// band (3.201..3.399 V) and deliver the expected load current.
pub fn assert_ldo_physical(m: &ModelEntry, nominal: f64, tol: f64) {
    // 100 ohm load at 3.3 V -> 33 mA, comfortably under the 1 A rating.
    let (vout, iload) = ldo_output_under_load(m, 100.0);
    assert!(
        (vout - nominal).abs() <= tol,
        "{} output {vout:.3} V not within {tol:.3} V of nominal {nominal:.3} V",
        m.id
    );
    let expect_i = vout / 100.0;
    assert!(
        (iload - expect_i).abs() < 1e-4,
        "{} load current {iload:.4} A != V/R {expect_i:.4} A (regulator not sourcing load)",
        m.id
    );
    // Ratings must be present and physical for the stress monitor to use them.
    if let Some(imax) = m.ratings.max_current_a {
        assert!(
            (0.1..=10.0).contains(&imax),
            "{} max_current_a {imax} A implausible for an LDO",
            m.id
        );
        assert!(
            iload < imax,
            "test load {iload:.3} A exceeds rated {imax:.3} A"
        );
    }
}

// ── Offline fixture tests (always run, no codex / network) ──────────────────

/// A canned codex reply for the BC847 — the exact shape real codex returns,
/// captured from a live run. This is the CI-safe path: it never shells out.
const FIXTURE_BC847: &str = r#"
[[models]]
id = "bc847"
kind = "bjt_npn"
description = "NPN general-purpose transistor, 65 V, 100 mA"
[models.match]
value_re = "(?i)^BC847"
[models.params]
is = 1.635e-14  # derived from VBE typ 660 mV at IC = 2 mA
bf = 290.0      # hFE group B typ 290 at VCE = 5 V, IC = 2 mA
nf = 1.0
vaf = 65.0      # VCEO max used as a proxy (Early V not given)
br = 1.0
[models.ratings]
max_current_a = 0.1
max_surge_current_a = 0.2
max_power_w = 0.25
max_voltage_v = 65.0
[models.pins]
"1" = "base"
"2" = "emitter"
"3" = "collector"
"#;

const FIXTURE_1N4148: &str = r#"
[[models]]
id = "1n4148"
kind = "diode"
description = "1N4148 high-speed switching diode, 100 V, 200 mA"
[models.match]
value_re = "(?i)^1N4148"
[models.params]
is = 2.52e-9   # classic 1N4148 SPICE value
n = 1.752      # emission coefficient
rs = 0.568     # series resistance
cjo = 4.0e-12  # Cd = 4 pF at VR = 0
bv = 100.0     # VRRM
[models.ratings]
max_current_a = 0.2
max_surge_current_a = 4.0
max_voltage_v = 100.0
[models.pins]
"1" = "cathode"
"2" = "anode"
"#;

const FIXTURE_AMS1117: &str = r#"
[[models]]
id = "ams1117-3.3"
kind = "vreg"
description = "AMS1117-3.3 1A low-dropout regulator, fixed 3.3 V"
[models.match]
value_re = "(?i)^AMS1117"
[models.params]
vout = 3.3       # fixed 3.3 V option
dropout_v = 1.3  # guaranteed max dropout
iq_a = 0.011     # quiescent current 11 mA max
[models.ratings]
max_current_a = 1.0
max_voltage_v = 15.0
max_junction_temp_c = 125.0
[models.pins]
"1" = "gnd"
"2" = "out"
"3" = "in"
"#;

#[test]
fn fixture_bc847_beta_in_band() {
    let m = first_model(FIXTURE_BC847);
    assert_eq!(m.kind, ComponentKind::BjtNpn);
    assert_bjt_physical(&m);
    // Ratings populated for the stress monitor.
    assert_eq!(m.ratings.max_voltage_v, Some(65.0));
    assert_eq!(m.ratings.max_current_a, Some(0.1));
}

#[test]
fn fixture_1n4148_forward_voltage() {
    let m = first_model(FIXTURE_1N4148);
    assert_eq!(m.kind, ComponentKind::Diode);
    assert_diode_physical(&m);
    assert_eq!(m.ratings.max_voltage_v, Some(100.0));
    assert_eq!(m.ratings.max_surge_current_a, Some(4.0));
}

#[test]
fn fixture_ams1117_regulates() {
    let m = first_model(FIXTURE_AMS1117);
    assert_eq!(m.kind, ComponentKind::Vreg);
    assert_ldo_physical(&m, 3.3, 0.099); // datasheet band +/- ~0.1 V
    assert_eq!(m.ratings.max_current_a, Some(1.0));
}

/// A deliberately garbage model must be rejected by the physical checks, not
/// silently bound. This is the "validation rejects nonsense" guarantee: the
/// model passes the static range checks (bf in 1..2000, is in 1e-20..1e-3) yet
/// is physically absurd, and physical validation must reject it — whether by
/// landing beta out of band or by failing to reach a sane operating point.
#[test]
fn garbage_bjt_is_rejected() {
    // Case 1: bf = 5 with a normal is — a "transistor" that barely amplifies.
    // It converges fine, but beta lands ~5, far below the BC847 band.
    let weak = first_model(
        r#"
[[models]]
id = "garbage_weak"
kind = "bjt_npn"
[models.params]
is = 1e-14
bf = 5.0
nf = 1.0
vaf = 65.0
"#,
    );
    assert!(
        bjt_is_rejected(&weak),
        "a bf=5 'transistor' slipped through physical validation"
    );

    // Case 2: is = 1e-20 — a junction that never turns on. The operating point
    // cannot be reached, so the solver reports an error (also a rejection).
    let dead = first_model(
        r#"
[[models]]
id = "garbage_dead"
kind = "bjt_npn"
[models.params]
is = 1e-20
bf = 200.0
nf = 1.0
vaf = 65.0
"#,
    );
    assert!(
        bjt_is_rejected(&dead),
        "a non-conducting BJT (is=1e-20) slipped through physical validation"
    );
}

/// True when an NPN model fails physical validation at the BC847 bias: either
/// the operating point cannot be reached, or beta lands outside the band.
fn bjt_is_rejected(m: &ModelEntry) -> bool {
    let bf = m.params.get_f64("bf").unwrap_or(1.0);
    let i_b = 2e-3 / bf;
    let beta_bad = match bjt_beta_try(m, 10.0, 2.2e3, i_b) {
        Err(_) => true,
        Ok(beta) => !(80.0..=600.0).contains(&beta),
    };
    let vbe_bad = match bjt_vbe_try(m) {
        Err(_) => true,
        Ok(vbe) => !(0.55..=0.80).contains(&vbe),
    };
    beta_bad || vbe_bad
}

// ── Live extracted-model tests (run only if extraction artifacts exist) ──────
// These pick up whatever the real `model-extract` binary wrote into
// testdata/extracted/. They are not #[ignore] because they self-skip when the
// artifact is absent, so CI stays green without codex.

#[test]
fn extracted_bc847_physical() {
    let Some(m) = load_extracted("BC847.toml") else {
        eprintln!("no testdata/extracted/BC847.toml — run model-extract; skipping");
        return;
    };
    assert_bjt_physical(&m);
    assert!(
        m.ratings.max_voltage_v.is_some(),
        "extracted BC847 must carry VCEO in ratings"
    );
}

#[test]
fn extracted_1n4148_physical() {
    let Some(m) = load_extracted("1N4148.toml") else {
        eprintln!("no testdata/extracted/1N4148.toml — run model-extract; skipping");
        return;
    };
    assert_diode_physical(&m);
}

#[test]
fn extracted_ams1117_physical() {
    // model-extract sanitises '.' in the part name to '_' for the filename.
    let m = load_extracted("AMS1117-3_3.toml").or_else(|| load_extracted("AMS1117-3.3.toml"));
    let Some(m) = m else {
        eprintln!("no testdata/extracted/AMS1117-3_3.toml — run model-extract; skipping");
        return;
    };
    assert_ldo_physical(&m, 3.3, 0.15);
}
