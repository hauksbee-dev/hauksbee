//! Tear certificates: the honesty object of the decomposition engine.
//!
//! A decomposed solve is only as trustworthy as the reasons its tears are
//! exact. Those reasons differ in kind and in strength: an ideal source is
//! pinned by definition; a balance tear keeps its KCL books exactly; a free
//! tear is zero-current-exact in the continuous sense but transmits a sampled
//! waveform; a stiff tear is exact only within a measured sag. The certificate
//! records, per torn node, which claim is being made and what tolerance it
//! carries, so that (a) reports and `--json` can show a user why their board
//! was torn and how much to trust the result, (b) the validation gates can
//! assert each tear against its own bar instead of a blanket number
//! (`docs/dev-plans/08-validation-and-test-campaign.md` section 3), and (c)
//! analyses that a torn model cannot honestly answer are refused instead of
//! silently answered wrong. The canonical example of (c) is lore #12: a rail
//! pinned by a stiff or balance tear can no longer sag, so any
//! supply-integrity question (brownout, inrush, droop) evaluated on the torn
//! model would report calm where the real board browns out. Refuse, don't
//! fake.
//!
//! The certificate is assembled structurally here. The *numeric* halves of the
//! gates (tear-vs-monolith agreement on convergent fixtures, capture-grid
//! tolerances once an executor picks a grid) belong to the orchestrator layer,
//! which fills [`ToleranceClaim::CaptureGrid`] and runs the
//! `tear_matches_monolith_*` suite; this module owns what can be proven from
//! topology and stamps alone.
//!
//! Long-form how-and-why (motivation, theory, rejected alternatives, the
//! buried bodies): docs/how-and-why/hauksbee-solve/decompose.md

use hauksbee_ir::{Circuit, NodeId};

use super::conduction::ConductionGraph;
use super::drivers::{driver_assignments, DriverAssignment, DriverPolicy};
use super::feedforward::StageDag;
use super::rails::{detect_balance_tears, BalanceTearCandidate, RailPolicy, TearMotive};

/// What kind of tear a node carries. The kinds are ordered by the strength of
/// their exactness claim; see the vocabulary in
/// `docs/dev-plans/02-tearing-architecture.md` section 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TearKind {
    /// One scalar KCL equation reconciles the rail per step: exact.
    Balance,
    /// The node is pinned at a settled voltage with no coupling equation:
    /// exact only within the measured stiffness tolerance.
    Stiff,
    /// A sense-only, proven one-directional boundary: zero-current exact,
    /// capture-grid limited when replayed from samples.
    Free,
}

/// Why the tear is believed exact. Every variant names a proof that exists in
/// code (a test, an SCC argument, a measurement), not an aspiration.
#[derive(Debug, Clone, PartialEq)]
pub enum Evidence {
    /// The node is driven by an ideal source; pinning it changes nothing.
    IdealSource,
    /// The scalar rail-balance loop keeps the node's KCL exact per step.
    BalanceEquation,
    /// Every device on the boundary only senses the node (their stamps write
    /// nothing into its KCL row: the property `declared_sense_rows_receive_no_
    /// current` and its staged/BBM companion enforce against the stamps), and
    /// the stage DAG's SCC condensation proves no reverse path exists.
    ZeroCurrentSenseOneDirectional,
    /// The node's voltage was measured (or provisionally verified) to move
    /// less than `tol_v` across the operating envelope; `sag_v` is the
    /// measured movement.
    MeasuredStiffness { sag_v: f64, tol_v: f64 },
}

/// The tolerance the tear's exactness claim carries. Gates assert against
/// this, never against a blanket number.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ToleranceClaim {
    /// Agreement with the monolith to numerical round-off.
    RoundOff,
    /// Limited by the replay sampling grid; `dt` is filled by the executor
    /// when it picks the grid (None until then, meaning "claim pending").
    CaptureGrid { dt: Option<f64> },
    /// Bounded by the certified stiffness sag.
    Stiffness { sag_v: f64 },
}

/// One torn node and the full story of why tearing it is legitimate.
#[derive(Debug, Clone)]
pub struct TearRecord {
    pub node: NodeId,
    pub kind: TearKind,
    pub evidence: Evidence,
    pub tolerance: ToleranceClaim,
    /// For free tears: the solve groups on each side (indices into the
    /// [`StageDag::groups`] of the decomposition this record belongs to).
    pub upstream: Option<usize>,
    pub downstream: Option<usize>,
}

/// Analyses a decomposed model must refuse. Extend as tears gain new
/// blind spots; every variant must state its reason in `reason()` so the
/// refusal is self-explanatory at every surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefusedAnalysis {
    /// Brownout / inrush / droop on a rail that a tear has pinned. The tear
    /// removed exactly the physics the question asks about (lore #12).
    SupplyIntegrityOnTornRail,
}

impl RefusedAnalysis {
    /// The plain-language reason surfaced with every refusal.
    pub fn reason(&self) -> &'static str {
        match self {
            RefusedAnalysis::SupplyIntegrityOnTornRail => {
                "a torn rail is pinned at its settled voltage, so supply sag, \
                 brownout, and inrush physics are absent from the decomposed \
                 model by construction; run this analysis on the monolithic \
                 (untorn) solve"
            }
        }
    }
}

/// The assembled honesty object for one decomposition.
#[derive(Debug, Clone)]
pub struct TearCertificate {
    /// One record per torn node.
    pub records: Vec<TearRecord>,
    /// Analyses this decomposition must refuse, with the nodes that cause
    /// each refusal.
    pub refusals: Vec<(RefusedAnalysis, Vec<NodeId>)>,
    /// True when a monolithic oracle exists (the fused solve converges), so
    /// the numeric tear-vs-monolith gate can actually run. False means the
    /// certificate rests on the structural proofs alone, which is the honest
    /// statement for boards like the flagship whose monolith never converges.
    /// Set by the orchestrator once it has tried; None until then.
    pub monolith_checkable: Option<bool>,
    /// Sense boundaries that could NOT be certified (a sensed node owned by
    /// no group, absorbed by no driver assignment, and torn by no rail).
    /// Non-empty means the decomposition is incomplete and the caller must
    /// fall back monolithic rather than trust it.
    pub uncertified_boundaries: Vec<NodeId>,
}

impl TearCertificate {
    /// Whether the certificate licenses running the decomposition at all.
    pub fn sound(&self) -> bool {
        self.uncertified_boundaries.is_empty()
    }

    /// Check whether an analysis may run on the decomposed model. `Err`
    /// carries the reason and the offending nodes; surfaces must present it
    /// as a refusal (exit 3 discipline), never downgrade it to a warning.
    pub fn permits(&self, analysis: RefusedAnalysis) -> Result<(), (RefusedAnalysis, Vec<NodeId>)> {
        for (refused, nodes) in &self.refusals {
            if *refused == analysis {
                return Err((*refused, nodes.clone()));
            }
        }
        Ok(())
    }

    /// One line per record, for reports and diagnostics.
    pub fn summary(&self, circuit: &Circuit) -> String {
        use std::fmt::Write;
        let mut out = String::new();
        for r in &self.records {
            let kind = match r.kind {
                TearKind::Balance => "balance",
                TearKind::Stiff => "stiff",
                TearKind::Free => "free",
            };
            let tol = match r.tolerance {
                ToleranceClaim::RoundOff => "round-off".to_string(),
                ToleranceClaim::CaptureGrid { dt: Some(dt) } => {
                    format!("capture grid dt={dt:.3e}s")
                }
                ToleranceClaim::CaptureGrid { dt: None } => "capture grid (pending)".to_string(),
                ToleranceClaim::Stiffness { sag_v } => format!("stiffness sag {sag_v:.3e}V"),
            };
            let _ = writeln!(
                out,
                "{kind} tear at {} ({tol})",
                circuit.node_name(r.node)
            );
        }
        for (refused, nodes) in &self.refusals {
            let names: Vec<_> = nodes.iter().map(|n| circuit.node_name(*n)).collect();
            let _ = writeln!(out, "refuses {refused:?} on [{}]", names.join(", "));
        }
        if !self.uncertified_boundaries.is_empty() {
            let names: Vec<_> = self
                .uncertified_boundaries
                .iter()
                .map(|n| circuit.node_name(*n))
                .collect();
            let _ = writeln!(
                out,
                "UNSOUND: uncertified sense boundaries [{}] (fall back monolithic)",
                names.join(", ")
            );
        }
        out
    }
}

/// The full decomposition analysis: the four passes plus their certificate.
/// This is the object the orchestrator consumes; `Partition::analyze` remains
/// the fast path for circuits none of this machinery helps.
#[derive(Debug)]
pub struct Decomposition {
    pub graph: ConductionGraph,
    pub dag: StageDag,
    pub balance_tears: Vec<BalanceTearCandidate>,
    pub drivers: Vec<DriverAssignment>,
    pub certificate: TearCertificate,
}

impl Decomposition {
    /// Run the four analysis passes and assemble the certificate.
    pub fn analyze(circuit: &Circuit, motive: TearMotive) -> Decomposition {
        Self::analyze_with(circuit, motive, RailPolicy::default(), DriverPolicy::default())
    }

    /// [`Decomposition::analyze`] with explicit policies.
    pub fn analyze_with(
        circuit: &Circuit,
        motive: TearMotive,
        rails: RailPolicy,
        drv: DriverPolicy,
    ) -> Decomposition {
        let graph = ConductionGraph::analyze(circuit);
        let dag = StageDag::build(circuit, &graph);
        let balance = detect_balance_tears(circuit, &graph, motive, &rails);
        let drivers = driver_assignments(circuit, &graph, &dag, &drv);

        let mut records = Vec::new();
        let mut supply_nodes = Vec::new();

        // Balance tears: exact by the scalar KCL reconciliation, but the rail
        // is still effectively pinned between outer iterations, so it shares
        // the supply-integrity refusal with stiff tears. (The balance loop
        // reconciles current books; it does not model the feed's transient
        // droop physics any better than a pin does.)
        for cand in balance.iter().filter(|c| c.torn()) {
            records.push(TearRecord {
                node: cand.rail,
                kind: TearKind::Balance,
                evidence: Evidence::BalanceEquation,
                tolerance: ToleranceClaim::RoundOff,
                upstream: None,
                downstream: None,
            });
            supply_nodes.push(cand.rail);
        }

        // Free tears: certified by the zero-current sense property (enforced
        // against the stamps by the conduction cross-checks) plus the SCC
        // one-directionality proof that StageDag::build already ran (an edge
        // only reaches free_tears if its endpoints landed in different
        // condensation groups, i.e. no reverse path exists).
        for ft in &dag.free_tears {
            records.push(TearRecord {
                node: ft.node,
                kind: TearKind::Free,
                evidence: Evidence::ZeroCurrentSenseOneDirectional,
                tolerance: ToleranceClaim::CaptureGrid { dt: None },
                upstream: Some(ft.upstream),
                downstream: Some(ft.downstream),
            });
        }

        // Boundary completeness: every sensed node must be accounted for by
        // exactly one certified mechanism, or the decomposition is unsound.
        // A sensed node is certified when it is (a) conducted by some island
        // (the free-tear path covers inter-group edges; intra-group sensing
        // needs no tear), (b) absorbed into the senser by the driver pass, or
        // (c) a torn balance rail. Anything else is a floating sense net: the
        // exact failure shape of the STEP-1 dead-membrane bug, and the reason
        // this check exists.
        // "Conducted" covers more than it may look like: an ideal source is a
        // device with conduction terminals, so a source-pinned node is owned
        // by the source's island; and driver absorption operates on tears
        // whose nodes are conducted by the driver group, so it cannot rescue
        // an unconducted node. What remains uncertifiable is a sensed node no
        // device conducts and no rail tear owns: a genuinely floating sense
        // net, which is the STEP-1 dead-membrane failure shape.
        let torn_rails: std::collections::BTreeSet<NodeId> =
            supply_nodes.iter().copied().collect();
        let mut uncertified = Vec::new();
        for e in &graph.sense_edges {
            let conducted = graph
                .node_island
                .get(e.node.0 as usize)
                .copied()
                .flatten()
                .is_some();
            if conducted || torn_rails.contains(&e.node) {
                continue;
            }
            uncertified.push(e.node);
        }
        uncertified.sort_unstable();
        uncertified.dedup();

        let refusals = if supply_nodes.is_empty() {
            Vec::new()
        } else {
            supply_nodes.sort_unstable();
            supply_nodes.dedup();
            vec![(RefusedAnalysis::SupplyIntegrityOnTornRail, supply_nodes)]
        };

        let certificate = TearCertificate {
            records,
            refusals,
            monolith_checkable: None,
            uncertified_boundaries: uncertified,
        };

        Decomposition {
            graph,
            dag,
            balance_tears: balance,
            drivers,
            certificate,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hauksbee_ir::{BjtModel, Device, Polarity, SourceKind};

    /// The rail_tear.rs shape in miniature: +5V -> shunt -> rail -> n PNP
    /// mirror blocks. Wide enough that the cost model tears it.
    fn shunt_array(n_blocks: usize) -> (Circuit, NodeId) {
        let mut c = Circuit::new();
        let p5 = c.node("+5V");
        c.add(Device::Vsource {
            name: "V5".into(),
            p: p5,
            n: NodeId::GROUND,
            kind: SourceKind::Dc(5.0),
        });
        let rail = c.node("ANALOG_VDD");
        c.add(Device::Resistor {
            name: "R_shunt".into(),
            a: p5,
            b: rail,
            ohms: 1e3,
            tc1: None,
        });
        let model = BjtModel {
            polarity: Polarity::P,
            ..BjtModel::default()
        };
        for k in 0..n_blocks {
            let base = c.node(&format!("b{k}"));
            let col = c.node(&format!("c{k}"));
            c.add(Device::Bjt {
                name: format!("Q{k}"),
                c: col,
                b: base,
                e: rail,
                model: model.clone(),
            });
            c.add(Device::Resistor {
                name: format!("Rb{k}"),
                a: base,
                b: NodeId::GROUND,
                ohms: 100e3,
                tc1: None,
            });
            c.add(Device::Resistor {
                name: format!("Rc{k}"),
                a: col,
                b: NodeId::GROUND,
                ohms: 10e3,
                tc1: None,
            });
        }
        (c, rail)
    }

    #[test]
    fn balance_tear_is_certified_and_refuses_supply_integrity() {
        let (c, rail) = shunt_array(24);
        let d = Decomposition::analyze(&c, TearMotive::ConvergenceEscalation);
        assert!(d.certificate.sound(), "{}", d.certificate.summary(&c));
        let rec = d
            .certificate
            .records
            .iter()
            .find(|r| r.node == rail)
            .expect("rail has a tear record");
        assert_eq!(rec.kind, TearKind::Balance);
        assert_eq!(rec.evidence, Evidence::BalanceEquation);
        assert_eq!(rec.tolerance, ToleranceClaim::RoundOff);
        // The refusal is the point: the torn model must not answer brownout
        // questions about the rail it pinned (lore #12).
        let err = d
            .certificate
            .permits(RefusedAnalysis::SupplyIntegrityOnTornRail)
            .expect_err("torn rail must refuse supply-integrity analyses");
        assert!(err.1.contains(&rail));
        assert!(err.0.reason().contains("monolithic"));
    }

    #[test]
    fn free_tear_is_certified_with_pending_capture_grid() {
        // Stage 0: an RC island whose output a comparator conducts.
        // Stage 1: a switch island whose select SENSES the comparator output.
        let mut c = Circuit::new();
        let vin = c.node("vin");
        c.add(Device::Vsource {
            name: "V1".into(),
            p: vin,
            n: NodeId::GROUND,
            kind: SourceKind::Dc(3.0),
        });
        let cmp_out = c.node("cmp_out");
        c.add(Device::Comparator {
            name: "K1".into(),
            out: cmp_out,
            inp: vin,
            inn: NodeId::GROUND,
            out_lo: 0.0,
            out_hi: 5.0,
            hysteresis: 1e-3,
        });
        let a = c.node("a");
        let b = c.node("b");
        c.add(Device::Vsource {
            name: "V2".into(),
            p: a,
            n: NodeId::GROUND,
            kind: SourceKind::Dc(1.0),
        });
        c.add(Device::VSwitch {
            name: "S1".into(),
            a,
            b,
            ctrl_p: cmp_out,
            ctrl_n: NodeId::GROUND,
            von: 2.0,
            voff: 1.0,
            ron: 10.0,
            roff: 1e9,
        });
        c.add(Device::Resistor {
            name: "Rload".into(),
            a: b,
            b: NodeId::GROUND,
            ohms: 1e3,
            tc1: None,
        });

        let d = Decomposition::analyze(&c, TearMotive::Profit);
        assert!(d.certificate.sound(), "{}", d.certificate.summary(&c));
        let rec = d
            .certificate
            .records
            .iter()
            .find(|r| r.node == cmp_out)
            .expect("the sensed comparator output is a certified free tear");
        assert_eq!(rec.kind, TearKind::Free);
        assert_eq!(rec.evidence, Evidence::ZeroCurrentSenseOneDirectional);
        assert_eq!(rec.tolerance, ToleranceClaim::CaptureGrid { dt: None });
        assert!(rec.upstream.is_some() && rec.downstream.is_some());
        // No rail was torn, so nothing is refused.
        assert!(d
            .certificate
            .permits(RefusedAnalysis::SupplyIntegrityOnTornRail)
            .is_ok());
    }

    #[test]
    fn floating_sense_net_makes_the_certificate_unsound() {
        // A switch whose select node no device conducts: the STEP-1
        // dead-membrane shape. The certificate must flag it rather than let
        // the executor float the select and silently turn the switch off.
        let mut c = Circuit::new();
        let a = c.node("a");
        let b = c.node("b");
        c.add(Device::Vsource {
            name: "V1".into(),
            p: a,
            n: NodeId::GROUND,
            kind: SourceKind::Dc(1.0),
        });
        let sel = c.node("sel_floating");
        c.add(Device::VSwitch {
            name: "S1".into(),
            a,
            b,
            ctrl_p: sel,
            ctrl_n: NodeId::GROUND,
            von: 2.0,
            voff: 1.0,
            ron: 10.0,
            roff: 1e9,
        });
        c.add(Device::Resistor {
            name: "Rload".into(),
            a: b,
            b: NodeId::GROUND,
            ohms: 1e3,
            tc1: None,
        });

        let d = Decomposition::analyze(&c, TearMotive::Profit);
        assert!(!d.certificate.sound());
        assert_eq!(d.certificate.uncertified_boundaries, vec![sel]);
        let summary = d.certificate.summary(&c);
        assert!(summary.contains("UNSOUND"), "{summary}");
        assert!(summary.contains("sel_floating"), "{summary}");
    }
}
