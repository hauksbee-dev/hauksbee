//! Small-signal AC analysis: SPICE `.AC`.
//!
//! The model is the textbook one. First find the DC operating point with the
//! existing real Newton solver ([`dc_operating_point`]). Then, at each frequency
//! in a sweep, build the *complex* MNA system `(G + jwC) x = b` by stamping the
//! small-signal (linearized) model of every device about that operating point:
//!
//! - resistor  -> real conductance `G = 1/R`
//! - capacitor -> `jwC`
//! - inductor  -> branch relation `v = jwL * i` (the MNA inductor row)
//! - independent source -> its AC drive amplitude/phase on the RHS. A circuit
//!   with a dedicated injection source named `VINJ`, `VLOOP`, `IINJ`, or `ILOOP`
//!   drives only that source and AC-grounds the DC bias rails. Otherwise every
//!   source is treated as a 1 + 0j unit AC source for backwards compatibility.
//! - diode / BJT / MOSFET / behavioral op-amp -> their linearized conductances
//!   and transconductances evaluated at the DC bias, i.e. `g = dI/dV` and
//!   `gm = dI/dV_control` at the operating point. These are the same tangents
//!   the transient Newton step uses, frozen at the OP.
//!
//! The complex system is solved with a dense LU ([`ComplexSystem`]). The result
//! is the complex node-voltage phasor at every node; magnitude (dB) and phase
//! (degrees) at any output node are read straight off it.
//!
//! Because AC is a *linearized* analysis, the input AC amplitude is irrelevant
//! to the transfer function (it scales out). The reported response is the ratio
//! of the output phasor to the swept stimulus, so it is dimensionless gain.
//!
//! Honest limits, documented in `docs/AC_ANALYSIS.md`: this linearizes about a
//! single DC operating point, so it sees the *averaged* small-signal behaviour,
//! not switching-level (cycle-by-cycle) dynamics, and it says nothing about
//! large-signal stability. Behavioral comparators and voltage switches have no
//! continuous small-signal model and contribute only their quiescent
//! conductance.

use num_complex::Complex64;

use crate::cmatrix::ComplexSystem;
use crate::newton::{dc_operating_point, Workspace};
use crate::options::SolverOptions;
use crate::system::Layout;
use hauksbee_ir::{BjtModel, Circuit, Device, MosLevel, MosfetModel, NodeId};

/// How to space the frequency sweep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sweep {
    /// `points` per decade (log spacing). The SPICE `dec` sweep.
    Decade,
    /// `points` total, linearly spaced between fstart and fstop.
    Linear,
}

/// A frequency sweep specification.
#[derive(Debug, Clone, Copy)]
pub struct AcSpec {
    pub fstart: f64,
    pub fstop: f64,
    /// Points per decade (Decade) or total points (Linear).
    pub points: usize,
    pub sweep: Sweep,
}

impl AcSpec {
    /// Parse a `<fstart>:<fstop>:<points>` triple (the CLI surface). An optional
    /// trailing `:lin` selects linear spacing; default is per-decade.
    pub fn parse(s: &str) -> Result<AcSpec, String> {
        let parts: Vec<&str> = s.split(':').collect();
        if parts.len() < 3 || parts.len() > 4 {
            return Err(format!(
                "expected <fstart>:<fstop>:<points>[:lin], got '{s}'"
            ));
        }
        let fstart: f64 = parts[0]
            .parse()
            .map_err(|_| format!("bad fstart '{}'", parts[0]))?;
        let fstop: f64 = parts[1]
            .parse()
            .map_err(|_| format!("bad fstop '{}'", parts[1]))?;
        let points: usize = parts[2]
            .parse()
            .map_err(|_| format!("bad points '{}'", parts[2]))?;
        let sweep = match parts.get(3).copied() {
            None | Some("dec") => Sweep::Decade,
            Some("lin") => Sweep::Linear,
            Some(other) => return Err(format!("unknown sweep mode '{other}' (dec|lin)")),
        };
        if fstart <= 0.0 || fstop <= fstart {
            return Err("need 0 < fstart < fstop".into());
        }
        if points == 0 {
            return Err("points must be >= 1".into());
        }
        Ok(AcSpec {
            fstart,
            fstop,
            points,
            sweep,
        })
    }

    /// The list of frequencies (Hz) this sweep visits.
    pub fn frequencies(&self) -> Vec<f64> {
        match self.sweep {
            Sweep::Linear => {
                if self.points == 1 {
                    return vec![self.fstart];
                }
                let step = (self.fstop - self.fstart) / (self.points - 1) as f64;
                (0..self.points)
                    .map(|i| self.fstart + step * i as f64)
                    .collect()
            }
            Sweep::Decade => {
                // Index-based geometric stepping avoids accumulated float drift,
                // and the final point is pinned to fstop so a non-integer number
                // of decades (e.g. 100 Hz .. 3 kHz) still includes the endpoint.
                let decades = (self.fstop / self.fstart).log10();
                let steps = (decades * self.points as f64).ceil() as usize;
                let ratio = 10f64.powf(1.0 / self.points as f64);
                let mut out = Vec::with_capacity(steps + 1);
                for i in 0..steps {
                    let f = self.fstart * ratio.powi(i as i32);
                    if f >= self.fstop * (1.0 - 1e-9) {
                        break;
                    }
                    out.push(f);
                }
                out.push(self.fstop);
                out
            }
        }
    }
}

/// One point of an AC sweep: frequency plus the complex phasor at every node.
#[derive(Debug, Clone)]
pub struct AcPoint {
    pub freq: f64,
    /// Complex node voltage indexed by `NodeId.0` (ground = 0 included as 0+0j).
    pub node_phasor: Vec<Complex64>,
}

impl AcPoint {
    /// The phasor at a named node, if present.
    pub fn node(&self, circuit: &Circuit, name: &str) -> Option<Complex64> {
        for id in 0..circuit.node_count() {
            if circuit.node_name(NodeId(id as u32)) == name {
                return self.node_phasor.get(id).copied();
            }
        }
        None
    }
}

/// The full AC sweep result.
#[derive(Debug, Clone)]
pub struct AcResponse {
    pub points: Vec<AcPoint>,
}

impl AcResponse {
    /// Magnitude (dB) and phase (degrees) at a named node across the sweep.
    /// Returns `(freq, mag_db, phase_deg)` triples. Magnitude is `20*log10|V|`.
    pub fn bode(&self, circuit: &Circuit, node: &str) -> Vec<(f64, f64, f64)> {
        let mut out = Vec::with_capacity(self.points.len());
        for p in &self.points {
            if let Some(v) = p.node(circuit, node) {
                let mag = v.norm();
                let db = 20.0 * mag.max(1e-300).log10();
                let phase = v.arg().to_degrees();
                out.push((p.freq, db, phase));
            }
        }
        out
    }
}

/// AC analysis engine.
pub struct AcAnalysis {
    opts: SolverOptions,
}

impl AcAnalysis {
    /// New analysis with the solver options used for the DC operating point.
    pub fn new(opts: SolverOptions) -> Self {
        AcAnalysis { opts }
    }

    /// Run the sweep. Computes the DC operating point once, then solves the
    /// complex system at every frequency. When a dedicated loop injection source
    /// is present, only that source is driven and other independent sources are
    /// AC-grounded. Otherwise every independent voltage / current source is
    /// driven with unit AC amplitude for backwards compatibility.
    pub fn run(&self, circuit: &Circuit, spec: &AcSpec) -> Result<AcResponse, String> {
        // 1. DC operating point (reusing the real solver verbatim).
        let mut ws = Workspace::new(circuit);
        dc_operating_point(&mut ws, circuit, &self.opts)?;
        let op = OperatingPoint::capture(&ws, circuit);

        // 2. Sweep.
        let freqs = spec.frequencies();
        let mut points = Vec::with_capacity(freqs.len());
        let n_nodes = circuit.node_count();
        for &f in &freqs {
            let w = std::f64::consts::TAU * f;
            let x = self.solve_at(circuit, &ws.layout, &op, w)?;
            // Map unknowns back to node phasors (ground = 0).
            let mut node_phasor = vec![Complex64::new(0.0, 0.0); n_nodes];
            for node in 1..n_nodes {
                if let Some(idx) = ws.layout.node(NodeId(node as u32)) {
                    node_phasor[node] = x[idx];
                }
            }
            points.push(AcPoint {
                freq: f,
                node_phasor,
            });
        }
        Ok(AcResponse { points })
    }

    /// Assemble and solve the complex system at angular frequency `w`.
    fn solve_at(
        &self,
        circuit: &Circuit,
        layout: &Layout,
        op: &OperatingPoint,
        w: f64,
    ) -> Result<Vec<Complex64>, String> {
        let mut sys = ComplexSystem::new(layout.size);
        let dedicated_ac_source = has_dedicated_ac_source(circuit);
        // gmin on every node diagonal, mirroring the real solver (keeps a
        // floating node solvable and matches the OP's conditioning).
        let gmin = self.opts.gmin;
        for i in 0..layout.n_nodes {
            sys.add(i, i, Complex64::new(gmin, 0.0));
        }
        for (id, dev) in circuit.iter() {
            stamp_ac(
                &mut sys,
                layout,
                op,
                dev,
                id,
                w,
                &self.opts,
                dedicated_ac_source,
            );
        }
        sys.solve().ok_or_else(|| {
            format!(
                "AC system singular at w={w:.4} rad/s (f={:.4} Hz)",
                w / std::f64::consts::TAU
            )
        })
    }
}

/// The frozen DC operating point: converged node voltages and branch currents,
/// indexed by `NodeId.0` / device, so the small-signal stamps can evaluate
/// device tangents at the bias.
struct OperatingPoint {
    /// Node voltage by `NodeId.0` (ground included as 0).
    node_v: Vec<f64>,
}

impl OperatingPoint {
    fn capture(ws: &Workspace, circuit: &Circuit) -> OperatingPoint {
        let n = circuit.node_count();
        let mut node_v = vec![0.0; n];
        for node in 1..n {
            if let Some(idx) = ws.layout.node(NodeId(node as u32)) {
                node_v[node] = ws.x[idx];
            }
        }
        OperatingPoint { node_v }
    }

    #[inline]
    fn v(&self, n: NodeId) -> f64 {
        self.node_v.get(n.0 as usize).copied().unwrap_or(0.0)
    }
}

/// Stamp one device's small-signal model into the complex system at `w`.
fn stamp_ac(
    sys: &mut ComplexSystem,
    layout: &Layout,
    op: &OperatingPoint,
    dev: &Device,
    id: hauksbee_ir::DeviceId,
    w: f64,
    opts: &SolverOptions,
    dedicated_ac_source: bool,
) {
    let n = |node: NodeId| layout.node(node);
    match dev {
        Device::Resistor {
            a, b, ohms, tc1, ..
        } => {
            let r = resistor_value(*ohms, *tc1, opts);
            if r > 0.0 {
                sys.stamp_admittance(n(*a), n(*b), Complex64::new(1.0 / r, 0.0));
            }
        }
        Device::Capacitor { a, b, farads, .. } => {
            // jwC.
            sys.stamp_admittance(n(*a), n(*b), Complex64::new(0.0, w * *farads));
        }
        Device::Inductor { a, b, henries, .. } => {
            // MNA branch row: v_a - v_b - jwL * i = 0.
            let br = layout.branch(id).expect("inductor has a branch unknown");
            let (ai, bi) = (n(*a), n(*b));
            if let Some(ai) = ai {
                sys.add(ai, br, Complex64::new(1.0, 0.0));
                sys.add(br, ai, Complex64::new(1.0, 0.0));
            }
            if let Some(bi) = bi {
                sys.add(bi, br, Complex64::new(-1.0, 0.0));
                sys.add(br, bi, Complex64::new(-1.0, 0.0));
            }
            sys.add(br, br, Complex64::new(0.0, -w * *henries));
        }
        Device::Vsource {
            name, p, n: neg, ..
        } => {
            // Ideal AC voltage source of unit amplitude: branch row sets
            // v_p - v_n = 1.
            let br = layout.branch(id).expect("vsource has a branch unknown");
            let (pi, ni) = (n(*p), n(*neg));
            if let Some(pi) = pi {
                sys.add(pi, br, Complex64::new(1.0, 0.0));
                sys.add(br, pi, Complex64::new(1.0, 0.0));
            }
            if let Some(ni) = ni {
                sys.add(ni, br, Complex64::new(-1.0, 0.0));
                sys.add(br, ni, Complex64::new(-1.0, 0.0));
            }
            sys.add_rhs(br, source_ac_drive(name, dedicated_ac_source));
        }
        Device::Isource {
            name, p, n: neg, ..
        } => {
            // Unit AC current injected p -> n.
            let drive = source_ac_drive(name, dedicated_ac_source);
            if let Some(pi) = n(*p) {
                sys.add_rhs(pi, -drive);
            }
            if let Some(ni) = n(*neg) {
                sys.add_rhs(ni, drive);
            }
        }
        Device::Diode { a, k, model, .. } => {
            // Small-signal tangent at the bias, through the SAME device eval
            // the transient stamp uses (so breakdown biases get the breakdown
            // conductance). A charge-storing diode (dev-plan 04 §3.1)
            // additionally contributes jw*C(vd): junction depletion plus
            // diffusion capacitance frozen at the operating point — an AC
            // answer without it would silently miss the pole the transient
            // model has. Charge-free models add exactly 0.0j: bit-identical.
            let vd = op.v(*a) - op.v(*k);
            let (idc, gd_raw) =
                crate::stamp::diode_eval(model, vd, opts.model_temp(), opts.effects.temperature);
            let gd = gd_raw.max(opts.gmin);
            let cap = if crate::stamp::diode_has_charge(model, &opts.effects) {
                crate::stamp::diode_charge(model, vd, idc, gd_raw).1
            } else {
                0.0
            };
            sys.stamp_admittance(n(*a), n(*k), Complex64::new(gd, w * cap));
        }
        Device::Bjt { c, b, e, model, .. } => {
            stamp_bjt_ac(sys, layout, op, *c, *b, *e, model, opts)
        }
        Device::Mosfet { d, g, s, model, .. } => {
            stamp_mosfet_ac(sys, layout, op, *d, *g, *s, model, opts)
        }
        Device::OpAmp {
            out,
            inp,
            inn,
            reference,
            gain,
            pole_hz,
            rail_lo,
            rail_hi,
            ..
        } => stamp_opamp_ac(
            sys, layout, op, *out, *inp, *inn, *reference, *gain, *pole_hz, *rail_lo, *rail_hi, w,
        ),
        // No continuous small-signal model: a switch sits at its quiescent
        // conductance; a comparator output is a fixed rail (open small-signal).
        Device::VSwitch {
            a,
            b,
            ctrl_p,
            ctrl_n,
            von,
            voff,
            ron,
            roff,
            ..
        } => {
            let g = vswitch_g(op.v(*ctrl_p) - op.v(*ctrl_n), *von, *voff, *ron, *roff);
            sys.stamp_admittance(n(*a), n(*b), Complex64::new(g, 0.0));
        }
        Device::Comparator { .. } => { /* digital output: no small-signal path */ }
        // Controlled sources are linear and frequency-independent: the AC
        // stamp is the transient stamp with real-valued entries.
        Device::Vcvs {
            p,
            n: neg,
            cp,
            cn,
            gain,
            ..
        } => {
            // Branch row `v_p - v_n - gain*(v_cp - v_cn) = 0`, RHS 0 (a
            // dependent source is never an AC drive).
            let br = layout.branch(id).expect("vcvs has a branch unknown");
            if let Some(pi) = n(*p) {
                sys.add(pi, br, Complex64::new(1.0, 0.0));
                sys.add(br, pi, Complex64::new(1.0, 0.0));
            }
            if let Some(ni) = n(*neg) {
                sys.add(ni, br, Complex64::new(-1.0, 0.0));
                sys.add(br, ni, Complex64::new(-1.0, 0.0));
            }
            if let Some(cpi) = n(*cp) {
                sys.add(br, cpi, Complex64::new(-gain, 0.0));
            }
            if let Some(cni) = n(*cn) {
                sys.add(br, cni, Complex64::new(*gain, 0.0));
            }
        }
        Device::Vccs {
            p,
            n: neg,
            cp,
            cn,
            gm,
            ..
        } => {
            // i(p->n) = gm * v(cp,cn): the four transconductance entries.
            let (pi, ni) = (n(*p), n(*neg));
            let (cpi, cni) = (n(*cp), n(*cn));
            let g = Complex64::new(*gm, 0.0);
            if let Some(pi) = pi {
                if let Some(cpi) = cpi {
                    sys.add(pi, cpi, g);
                }
                if let Some(cni) = cni {
                    sys.add(pi, cni, -g);
                }
            }
            if let Some(ni) = ni {
                if let Some(cpi) = cpi {
                    sys.add(ni, cpi, -g);
                }
                if let Some(cni) = cni {
                    sys.add(ni, cni, g);
                }
            }
        }
        // Current-controlled sources: linear and frequency-independent like
        // E/G — the AC stamp is the transient stamp with real entries. The
        // control branch index resolves through the SAME layout the transient
        // path froze, so the F/H coupling column is identical.
        Device::Cccs {
            p, n: neg, ctrl_src, gain, ..
        } => {
            let cbr = layout
                .branch(*ctrl_src)
                .expect("cccs control source owns a branch unknown");
            if let Some(pi) = n(*p) {
                sys.add(pi, cbr, Complex64::new(*gain, 0.0));
            }
            if let Some(ni) = n(*neg) {
                sys.add(ni, cbr, Complex64::new(-*gain, 0.0));
            }
        }
        Device::Ccvs {
            p, n: neg, ctrl_src, transres, ..
        } => {
            // Branch row `v_p - v_n - transres*i_ctrl = 0`, RHS 0 (a dependent
            // source is never an AC drive).
            let br = layout.branch(id).expect("ccvs has a branch unknown");
            let cbr = layout
                .branch(*ctrl_src)
                .expect("ccvs control source owns a branch unknown");
            if let Some(pi) = n(*p) {
                sys.add(pi, br, Complex64::new(1.0, 0.0));
                sys.add(br, pi, Complex64::new(1.0, 0.0));
            }
            if let Some(ni) = n(*neg) {
                sys.add(ni, br, Complex64::new(-1.0, 0.0));
                sys.add(br, ni, Complex64::new(-1.0, 0.0));
            }
            sys.add(br, cbr, Complex64::new(-*transres, 0.0));
        }
    }
}

fn has_dedicated_ac_source(circuit: &Circuit) -> bool {
    circuit.iter().any(|(_, dev)| match dev {
        Device::Vsource { name, .. } | Device::Isource { name, .. } => {
            is_dedicated_ac_source(name)
        }
        _ => false,
    })
}

fn source_ac_drive(name: &str, dedicated_ac_source: bool) -> Complex64 {
    if !dedicated_ac_source || is_dedicated_ac_source(name) {
        Complex64::new(1.0, 0.0)
    } else {
        Complex64::new(0.0, 0.0)
    }
}

fn is_dedicated_ac_source(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    matches!(
        upper.as_str(),
        "VINJ" | "VLOOP" | "VAC" | "IINJ" | "ILOOP" | "IAC"
    ) || upper.contains("_VINJ")
        || upper.contains("_VLOOP")
        || upper.contains("_IINJ")
        || upper.contains("_ILOOP")
}

// --- small-signal device tangents at the operating point --------------------

fn resistor_value(ohms: f64, tc1: Option<f64>, opts: &SolverOptions) -> f64 {
    match tc1 {
        Some(tc) if opts.effects.temperature => ohms * (1.0 + tc * (opts.model_temp() - 27.0)),
        _ => ohms,
    }
}

/// BJT small-signal model at the bias: input conductances gpi (b-e), gmu (b-c),
/// output go (c-e), and transconductance gm (ic vs vbe). This mirrors the
/// Gummel-Poon tangents the transient stamp computes, frozen at the OP.
#[allow(clippy::too_many_arguments)]
fn stamp_bjt_ac(
    sys: &mut ComplexSystem,
    layout: &Layout,
    op: &OperatingPoint,
    c: NodeId,
    b: NodeId,
    e: NodeId,
    model: &BjtModel,
    opts: &SolverOptions,
) {
    let sign = model.polarity.sign();
    let temp_on = opts.effects.temperature;
    let t_c = opts.model_temp();
    let is = if temp_on { model.is_at(t_c) } else { model.is };
    let vt = hauksbee_ir::thermal_voltage_c(if temp_on { t_c } else { 27.0 });

    let vbe = sign * (op.v(b) - op.v(e));
    let vbc = sign * (op.v(b) - op.v(c));
    let nvf = model.nf * vt;
    let nvr = model.nr * vt;
    let ef = (vbe / nvf).clamp(-40.0, 40.0).exp();
    let er = (vbc / nvr).clamp(-40.0, 40.0).exp();
    let early = if opts.effects.early_effect && model.vaf.is_finite() {
        1.0 - vbc / model.vaf
    } else {
        1.0
    };
    let inv_early = 1.0 / early.max(0.1);

    let gpi = (is * ef / (nvf * model.bf)).max(opts.gmin);
    let gmu = (is * er / (nvr * model.br)).max(opts.gmin);
    let gif = is * ef / nvf;
    let gir = is * er / nvr;
    let gm = (gif * inv_early).max(opts.gmin);
    let go = (gir * inv_early).max(opts.gmin);

    let (ci, bi, ei) = (layout.node(c), layout.node(b), layout.node(e));
    stamp_pair(sys, bi, ei, gpi);
    stamp_pair(sys, bi, ci, gmu);
    stamp_pair(sys, ci, ei, go);
    // Transconductance: ic depends on vbe = v(b) - v(e). Polarity sign cancels
    // because gm multiplies the (sign-folded) vbe and the current is folded back.
    stamp_transconductance(sys, ci, ei, bi, ei, gm);
}

#[allow(clippy::too_many_arguments)]
fn stamp_mosfet_ac(
    sys: &mut ComplexSystem,
    layout: &Layout,
    op: &OperatingPoint,
    d: NodeId,
    gnode: NodeId,
    s: NodeId,
    model: &MosfetModel,
    opts: &SolverOptions,
) {
    match model.level {
        MosLevel::Level1 => {}
    }
    let sign = model.polarity.sign();
    let beta = model.beta();
    let mut vd = sign * op.v(d);
    let vg = sign * op.v(gnode);
    let mut vs = sign * op.v(s);
    let swap = vd < vs;
    if swap {
        std::mem::swap(&mut vd, &mut vs);
    }
    let vgs = vg - vs;
    let vds = vd - vs;
    let vth = model.vto;
    let vt = hauksbee_ir::thermal_voltage_c(if opts.effects.temperature {
        opts.model_temp()
    } else {
        27.0
    });
    let nsub = model.n_sub.max(1.0);

    let (gm, gds) = if vgs - vth < -nsub * vt * 10.0 {
        (opts.gmin, opts.gmin)
    } else if vgs <= vth {
        let i0 = beta * (nsub * vt) * (nsub * vt) * std::f64::consts::E;
        let id = i0 * ((vgs - vth) / (nsub * vt)).exp();
        ((id / (nsub * vt)).max(opts.gmin), opts.gmin)
    } else {
        let lambda = if opts.effects.early_effect {
            model.lambda
        } else {
            0.0
        };
        let vov = vgs - vth;
        if vds < vov {
            let gm = beta * vds * (1.0 + lambda * vds);
            let gds = beta
                * ((vov - vds) * (1.0 + lambda * vds) + (vov * vds - 0.5 * vds * vds) * lambda);
            (gm.max(opts.gmin), gds.max(opts.gmin))
        } else {
            let gm = beta * vov * (1.0 + lambda * vds);
            let gds = 0.5 * beta * vov * vov * lambda;
            (gm.max(opts.gmin), gds.max(opts.gmin))
        }
    };

    let (di, gi, si) = (layout.node(d), layout.node(gnode), layout.node(s));
    let (dn, sn) = if swap { (si, di) } else { (di, si) };
    stamp_pair(sys, dn, sn, gds);
    stamp_transconductance(sys, dn, sn, gi, sn, gm);
}

/// Behavioral op-amp small-signal: within the rails, `out = gain*(vp - vn)`
/// through a strong output stage, exactly the transient tangent. Saturated
/// (rail-pinned) at the OP -> the gain path is open and the output is held.
#[allow(clippy::too_many_arguments)]
fn stamp_opamp_ac(
    sys: &mut ComplexSystem,
    layout: &Layout,
    op: &OperatingPoint,
    out: NodeId,
    inp: NodeId,
    inn: NodeId,
    reference: Option<NodeId>,
    gain: f64,
    pole_hz: Option<f64>,
    rail_lo: f64,
    rail_hi: f64,
    w: f64,
) {
    let gout = 1.0;
    let vref = reference.map(|n| op.v(n)).unwrap_or(0.0);
    let target = (vref + gain * (op.v(inp) - op.v(inn))).clamp(rail_lo, rail_hi);
    let in_rail = target > rail_lo && target < rail_hi;
    let oi = layout.node(out);
    if let Some(oi) = oi {
        sys.add(oi, oi, Complex64::new(gout, 0.0));
        if in_rail {
            if let Some(ri) = reference.and_then(|n| layout.node(n)) {
                sys.add(oi, ri, Complex64::new(-gout, 0.0));
            }
            let gain = opamp_ac_gain(gain, pole_hz, w);
            if let Some(pi) = layout.node(inp) {
                sys.add(oi, pi, -gain * gout);
            }
            if let Some(ni) = layout.node(inn) {
                sys.add(oi, ni, gain * gout);
            }
        }
    }
}

fn opamp_ac_gain(gain: f64, pole_hz: Option<f64>, w: f64) -> Complex64 {
    let a0 = Complex64::new(gain, 0.0);
    let Some(pole_hz) = pole_hz else {
        return a0;
    };
    if pole_hz <= 0.0 || !pole_hz.is_finite() {
        return a0;
    }
    let wp = std::f64::consts::TAU * pole_hz;
    a0 / Complex64::new(1.0, w / wp)
}

fn vswitch_g(vctrl: f64, von: f64, voff: f64, ron: f64, roff: f64) -> f64 {
    let vmid = 0.5 * (von + voff);
    let span = ((von - voff).abs()).max(1e-9);
    let gon = 1.0 / ron.max(1e-12);
    let goff = 1.0 / roff.max(1e-12);
    let s = 0.5 * (1.0 + (3.0 * (vctrl - vmid) / span).tanh());
    (goff.ln() + s * (gon.ln() - goff.ln())).exp()
}

#[inline]
fn stamp_pair(sys: &mut ComplexSystem, a: Option<usize>, b: Option<usize>, g: f64) {
    sys.stamp_admittance(a, b, Complex64::new(g, 0.0));
}

/// Stamp a real transconductance into the complex system: current into (ip, in_)
/// proportional to v(cp) - v(cn).
#[inline]
fn stamp_transconductance(
    sys: &mut ComplexSystem,
    ip: Option<usize>,
    in_: Option<usize>,
    cp: Option<usize>,
    cn: Option<usize>,
    gm: f64,
) {
    let gm = Complex64::new(gm, 0.0);
    if let Some(ip) = ip {
        if let Some(cp) = cp {
            sys.add(ip, cp, gm);
        }
        if let Some(cn) = cn {
            sys.add(ip, cn, -gm);
        }
    }
    if let Some(in_) = in_ {
        if let Some(cp) = cp {
            sys.add(in_, cp, -gm);
        }
        if let Some(cn) = cn {
            sys.add(in_, cn, gm);
        }
    }
}
