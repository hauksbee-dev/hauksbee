//! Rail ownership: which supply nets may take an ideal source, decided from
//! the bound circuit rather than from part kinds.
//! Long-form how-and-why: docs/how-and-why/hauksbee-engine/binder.md.
//!
//! Nets that a series conductor joins are one rail at DC: a resistor of at
//! most [`SERIES_OHMS`] (a sense resistor, a ferrite's DCR, a 0 Ω jumper), an
//! inductor or ferrite, a 0 V ammeter. Each such group is a *domain*. A domain
//! already has a supply when an element in it sets its voltage: a regulator's
//! output source, a controlled source or op-amp output, or a node a
//! behavioural model drives (a converter's output, a voltage law). An ideal
//! source stamped there as well would fight that element and carry its load,
//! hiding the load from the element's input: a buck's +5V past its inductor
//! read −1.8 A of input current instead of +70 mA. So a driven domain takes no
//! ideal rail, and an undriven one takes exactly one.
//!
//! Two drivers in one domain have no single meaning (two regulators tied, a
//! declared supply on a regulator's output); the binder and the CI runner
//! report them instead of letting the sources fight.
//!
//! A current-limited source (a linear charger's constant-current output, a
//! `Behavioral` source named with [`LIMITED_SOURCE_PREFIX`]) is the
//! exception: it forces a current, not a voltage, so a battery or supply
//! beside it is the charger's load, not a rival. Such a source still counts as
//! feeding its net (an undriven rail's name-derived ideal source gives way to
//! it), but it never makes two drivers fight and never refuses a declared
//! supply.
//!
//! A shunt regulator (TL431 class) drives its cathode too: its sink holds the
//! node in closed loop against whatever feeds it, so an ideal source there
//! would take the feed current the shunt should sink.
//!
//! Switches (load switches, analog switches, relay contacts) do not make one
//! domain: whether one side feeds the other depends on state and on a
//! direction no card states, and power muxes and USB OTG paths run both ways.
//! They do make a *fed* group ([`RailDomains::join_switches`]): a supply net
//! behind a switch from a driven rail, or from a rail already supplied at the
//! same voltage, is that rail switched, not a second supply. Stamping one
//! there would hold it up while the switch is open and carry its load while
//! the switch is closed, so the upstream rail would never see it.

use crate::behavioral::BehavioralDevice;
use hauksbee_ir::{BOutput, Circuit, Device, DeviceId, NodeId, SourceKind};

/// The largest resistance treated as a series conductor joining two nets
/// into one rail.
///
/// Sense resistors, ferrite DCRs and jumpers sit below it; an
/// RC filter's 10 Ω, which drops a visible voltage, does not.
pub const SERIES_OHMS: f64 = 1.0;

/// The name prefix of a shunt regulator's cathode sink, the `Vccs` whose `p`
/// it regulates.
pub const SHUNT_SINK_PREFIX: &str = "Gka_";

/// The name prefix of a current-limited source: a `Behavioral` source whose `n`
/// node it feeds (a linear charger's constant-current output stage).
pub const LIMITED_SOURCE_PREFIX: &str = "Ichg_";

/// An element that sets a node's voltage, or feeds it a limited current.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Driver {
    /// The node it drives.
    pub node: NodeId,
    /// The part (reference designator) or device that drives it.
    pub by: String,
    /// True for a current-limited source: it feeds the node without setting
    /// its voltage, so it never fights another source.
    pub limited: bool,
}

/// Where a driving part takes its power, for pointing a refused supply at it.
#[derive(Debug, Clone, PartialEq)]
pub struct Feed {
    /// The part's input node.
    pub input: NodeId,
    /// The least voltage the part needs there, when its card says.
    pub min_v: Option<f64>,
    /// The input pin's number, so a spec can name an unnamed net by pin.
    pub pin: Option<String>,
}

/// The board's nets grouped into rail domains, with the drivers in each.
#[derive(Debug, Clone, Default)]
pub struct RailDomains {
    root: Vec<usize>,
    /// `root`, further joined across switches between board nets.
    fed: Vec<usize>,
    drivers: Vec<Driver>,
}

impl RailDomains {
    /// Group `circuit`'s nodes and collect the drivers: every voltage-setting
    /// device not in `skip`, plus each behavioural device's driven nodes.
    #[must_use]
    pub fn new(circuit: &Circuit, behavioral: &[BehavioralDevice], skip: &[DeviceId]) -> Self {
        let mut root: Vec<usize> = (0..circuit.node_count()).collect();
        for (_, d) in circuit.iter() {
            if let Some([a, b]) = series_ends(d).filter(|e| !e.iter().any(|n| n.is_ground())) {
                let (ra, rb) = (find(&mut root, a.0 as usize), find(&mut root, b.0 as usize));
                root[ra.max(rb)] = ra.min(rb);
            }
        }
        for i in 0..root.len() {
            root[i] = find(&mut root, i);
        }
        let generic = circuit.iter().filter(|(id, _)| !skip.contains(id));
        let mut drivers: Vec<Driver> = generic
            .filter_map(|(_, d)| {
                let node = sets_voltage_of(d).filter(|n| !n.is_ground())?;
                Some(Driver {
                    node,
                    by: part_of(d.name()).to_string(),
                    limited: false,
                })
            })
            .collect();
        drivers.extend(
            circuit
                .iter()
                .filter(|(id, _)| !skip.contains(id))
                .filter_map(|(_, d)| match d {
                    Device::Behavioral { name, n, .. }
                        if name.starts_with(LIMITED_SOURCE_PREFIX) && !n.is_ground() =>
                    {
                        Some(Driver {
                            node: *n,
                            by: part_of(name).to_string(),
                            limited: true,
                        })
                    }
                    _ => None,
                }),
        );
        for b in behavioral {
            drivers.extend(b.driven_nodes().into_iter().map(|node| Driver {
                node,
                by: b.reference.clone(),
                limited: false,
            }));
        }
        let fed = root.clone();
        Self { root, fed, drivers }
    }

    /// Join the nets each switch in `circuit` connects, where both ends pass
    /// `board_net` (a part's internal switch between private nodes feeds
    /// nothing a supply could land on), into fed groups.
    pub fn join_switches(&mut self, circuit: &Circuit, board_net: impl Fn(NodeId) -> bool) {
        for (_, d) in circuit.iter() {
            let Device::VSwitch { a, b, .. } = d else {
                continue;
            };
            if [a, b].iter().all(|n| !n.is_ground() && board_net(**n)) {
                let fed = &mut self.fed;
                let (ra, rb) = (find(fed, a.0 as usize), find(fed, b.0 as usize));
                fed[ra.max(rb)] = ra.min(rb);
            }
        }
        for i in 0..self.fed.len() {
            self.fed[i] = find(&mut self.fed, i);
        }
    }

    /// True when `a` and `b` are one rail, or one rail and its switched
    /// branches.
    #[must_use]
    pub fn fed_joined(&self, a: NodeId, b: NodeId) -> bool {
        self.fed.get(a.0 as usize) == self.fed.get(b.0 as usize)
    }

    /// True when a driver sits on `node`'s fed group: the net is a driven
    /// rail or a switched branch of one.
    #[must_use]
    pub fn fed_by_driver(&self, node: NodeId) -> bool {
        self.drivers.iter().any(|d| self.fed_joined(d.node, node))
    }

    /// True when `a` and `b` are one rail at DC.
    #[must_use]
    pub fn joined(&self, a: NodeId, b: NodeId) -> bool {
        self.root.get(a.0 as usize) == self.root.get(b.0 as usize)
    }

    /// The drivers of `node`'s domain, one per driving part: the voltage
    /// setters and the current-limited feeds both.
    #[must_use]
    pub fn drivers_of(&self, node: NodeId) -> Vec<&Driver> {
        self.collect(node, true)
    }

    /// The parts that set `node`'s domain voltage: [`Self::drivers_of`] less
    /// the current-limited feeds, which a source beside them cannot fight.
    #[must_use]
    pub fn setters_of(&self, node: NodeId) -> Vec<&Driver> {
        self.collect(node, false)
    }

    fn collect(&self, node: NodeId, limited: bool) -> Vec<&Driver> {
        let mut out: Vec<&Driver> = Vec::new();
        let mine = |d: &&Driver| self.joined(d.node, node) && (limited || !d.limited);
        for d in self.drivers.iter().filter(mine) {
            if !out.iter().any(|o| o.by == d.by) {
                out.push(d);
            }
        }
        out
    }

    /// Why `net` (on `node`) cannot take a declared supply: the parts already
    /// setting its domain's voltage, and the net to declare it on instead.
    ///
    /// `feed_of` gives a driving part's [`Feed`]. The note follows that chain upstream past
    /// every input another part drives, so it names the one net where the
    /// board takes power and the user fixes the spec in one edit.
    #[must_use]
    pub fn owned_note(
        &self,
        circuit: &Circuit,
        net: &str,
        node: NodeId,
        feed_of: impl Fn(&str) -> Option<Feed>,
    ) -> Option<String> {
        let drivers = self.setters_of(node);
        let first = drivers.first()?;
        let by = drivers.iter().map(|d| d.by.as_str()).collect::<Vec<_>>();
        let via = if first.node == node {
            String::new()
        } else {
            format!(
                " (through a series conductor from '{}')",
                circuit.node_name(first.node)
            )
        };
        let instead = self.upstream_feed(first, &feed_of).map_or_else(
            || {
                " Declare the supply where the board takes power instead: the driving part's \
                 input, a battery or a connector."
                    .to_string()
            },
            |(chain, feed)| {
                let top = &chain[0];
                let feeds = if chain.len() > 1 {
                    format!(" ({})", chain.join(" feeds "))
                } else {
                    String::new()
                };
                let by_pin = feed.pin.map_or_else(String::new, |p| {
                    format!(" (or ref = \"{top}\", pin = \"{p}\")")
                });
                let need = feed
                    .min_v
                    .map_or_else(String::new, |v| format!(", at least {v:.2} V"));
                format!(
                    " Declare the supply on {top}'s input instead{feeds}: net = \"{}\"{by_pin}{need}.",
                    circuit.node_name(feed.input),
                )
            },
        );
        Some(format!(
            "'{net}' is already driven by {}{via}, so a supply there would fight it and hide \
             its current draw.{instead}",
            by.join(" and ")
        ))
    }

    /// The undriven feed at the top of `from`'s chain, with the parts on the
    /// way (topmost first). `None` when a part's input is unknown or the chain
    /// loops, so the note never names a net the runner would refuse in turn.
    fn upstream_feed(
        &self,
        from: &Driver,
        feed_of: &impl Fn(&str) -> Option<Feed>,
    ) -> Option<(Vec<String>, Feed)> {
        let mut chain = vec![from.by.clone()];
        let mut feed = feed_of(&from.by)?;
        while let Some(next) = self.setters_of(feed.input).first() {
            let Some(up) = feed_of(&next.by).filter(|_| !chain.contains(&next.by)) else {
                break;
            };
            chain.insert(0, next.by.clone());
            feed = up;
        }
        self.setters_of(feed.input)
            .is_empty()
            .then_some((chain, feed))
    }

    /// Every domain two or more parts drive, naming the parts and the nets
    /// they meet on.
    #[must_use]
    pub fn conflicts(&self, circuit: &Circuit) -> Vec<RailConflict> {
        let mut seen: Vec<usize> = Vec::new();
        let mut out = Vec::new();
        for d in self.drivers.iter().filter(|d| !d.limited) {
            let r = self.root[d.node.0 as usize];
            if seen.contains(&r) {
                continue;
            }
            seen.push(r);
            let parts = self.setters_of(d.node);
            if parts.len() < 2 {
                continue;
            }
            let net = |p: &&Driver| circuit.node_name(p.node).to_string();
            let at = parts
                .iter()
                .map(|p| format!("{} on '{}'", p.by, net(p)))
                .collect::<Vec<_>>();
            out.push(RailConflict {
                parts: parts.iter().map(|p| p.by.clone()).collect(),
                nets: parts.iter().map(net).collect(),
                message: format!(
                    "{} drive one rail: each sets its voltage, so they fight and the rail's \
                     current splits between them by solver stiffness, not by the board; the \
                     rail's voltage and every input current upstream of it are unreliable",
                    at.join(" and ")
                ),
            });
        }
        out
    }
}

/// A rail with more than one source setting its voltage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RailConflict {
    /// The parts that drive it (empty when two named rails were joined).
    pub parts: Vec<String>,
    /// The nets they meet on.
    pub nets: Vec<String>,
    /// What fights and what it does to the result.
    pub message: String,
}

fn find(root: &mut [usize], mut i: usize) -> usize {
    while root[i] != i {
        root[i] = root[root[i]];
        i = root[i];
    }
    i
}

/// The two nets a series conductor joins.
fn series_ends(d: &Device) -> Option<[NodeId; 2]> {
    match d {
        Device::Resistor { a, b, ohms, .. } if *ohms <= SERIES_OHMS => Some([*a, *b]),
        Device::Inductor { a, b, .. } => Some([*a, *b]),
        Device::Vsource {
            p,
            n,
            kind: SourceKind::Dc(v),
            ..
        } if *v == 0.0 => Some([*p, *n]),
        _ => None,
    }
}

/// The node a device holds at a voltage of its own, if any. A 0 V source is
/// an ammeter, a conductor; any other source drives its positive terminal.
fn sets_voltage_of(d: &Device) -> Option<NodeId> {
    match d {
        Device::Vsource {
            kind: SourceKind::Dc(v),
            ..
        } if *v == 0.0 => None,
        Device::Vsource { p, .. }
        | Device::Vcvs { p, .. }
        | Device::Ccvs { p, .. }
        | Device::Behavioral {
            p,
            output: BOutput::Voltage,
            ..
        } => Some(*p),
        Device::Vccs { name, p, .. } if name.starts_with(SHUNT_SINK_PREFIX) => Some(*p),
        Device::OpAmp { out, .. } => Some(*out),
        _ => None,
    }
}

/// The reference designator inside a stamped device name (`Vreg_U3` -> `U3`,
/// an op-amp's `U4` unchanged).
fn part_of(name: &str) -> &str {
    let rest = name.split_once('_').map_or(name, |(_, r)| r);
    rest.split('_').next().unwrap_or(rest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hauksbee_ir::CompiledExpr;

    fn circuit(build: impl FnOnce(&mut Circuit, [NodeId; 4])) -> Circuit {
        let mut c = Circuit::new();
        let nodes = ["+12V", "SW", "+5V", "+5V_A"].map(|n| c.node(n));
        build(&mut c, nodes);
        c
    }

    #[test]
    fn a_rail_past_an_inductor_and_a_ferrite_is_owned_by_the_driver() {
        let c = circuit(|c, [_, sw, out, ana]| {
            c.add(Device::vsource(
                "Vreg_U1",
                sw,
                NodeId::GROUND,
                SourceKind::Dc(5.0),
            ));
            c.add(Device::Inductor {
                name: "L1".into(),
                a: sw,
                b: out,
                henries: 1e-5,
                ic: None,
            });
            c.add(Device::resistor("FB1", out, ana, 0.05));
        });
        let d = RailDomains::new(&c, &[], &[]);
        let [vin, _, out, ana] = ["+12V", "SW", "+5V", "+5V_A"].map(|n| c.find_node(n).unwrap());
        assert_eq!(d.drivers_of(ana).len(), 1);
        assert_eq!(d.drivers_of(out)[0].by, "U1");
        assert!(d.drivers_of(vin).is_empty());
        assert!(d.conflicts(&c).is_empty());
        let feed = Feed {
            input: vin,
            min_v: Some(5.6),
            pin: Some("4".into()),
        };
        let note = d
            .owned_note(&c, "+5V_A", ana, |p| (p == "U1").then(|| feed.clone()))
            .unwrap();
        assert!(note.contains("'SW'"), "{note}");
        let fix = "Declare the supply on U1's input instead: net = \"+12V\" \
                   (or ref = \"U1\", pin = \"4\"), at least 5.60 V.";
        assert!(note.contains(fix), "{note}");
        assert!(d
            .owned_note(&c, "+5V_A", ana, |_| None)
            .unwrap()
            .contains("a battery or a connector"));
    }

    #[test]
    fn the_note_follows_a_regulator_chain_up_to_the_undriven_input() {
        let mut c = Circuit::new();
        let [vin, mid, out] = ["VBAT", "+5V", "+3V3"].map(|n| c.node(n));
        c.add(Device::vsource(
            "Vreg_U1",
            mid,
            NodeId::GROUND,
            SourceKind::Dc(5.0),
        ));
        c.add(Device::vsource(
            "Vreg_U2",
            out,
            NodeId::GROUND,
            SourceKind::Dc(3.3),
        ));
        let d = RailDomains::new(&c, &[], &[]);
        let feed = |input, min_v| Feed {
            input,
            min_v,
            pin: None,
        };
        let feeds = |p: &str| match p {
            "U1" => Some(feed(vin, Some(5.3))),
            "U2" => Some(feed(mid, Some(3.4))),
            _ => None,
        };
        let note = d.owned_note(&c, "+3V3", out, feeds).unwrap();
        assert!(
            note.contains("on U1's input instead (U1 feeds U2): net = \"VBAT\", at least 5.30 V."),
            "{note}"
        );
        // A loop, or a top part with no known input, never names a driven net.
        let note = d
            .owned_note(&c, "+3V3", out, |p| (p == "U2").then(|| feed(mid, None)))
            .unwrap();
        assert!(
            !note.contains("'+5V'") && note.contains("a battery or a connector"),
            "{note}"
        );
    }

    /// A charger's limited output (`Ichg_` into a node, a 0 V ammeter to the
    /// board net) feeds the rail without setting its voltage.
    fn charger_circuit(c: &mut Circuit, [_, sw, out, _]: [NodeId; 4]) {
        c.add(Device::Behavioral {
            name: "Ichg_U1".into(),
            p: NodeId::GROUND,
            n: sw,
            output: BOutput::Current,
            expr: CompiledExpr::compile("0.1").unwrap(),
            deps: Vec::new(),
        });
        c.add(Device::vsource("Vreg_U1", out, sw, SourceKind::Dc(0.0)));
    }

    #[test]
    fn a_limited_source_feeds_a_rail_but_never_owns_it() {
        let c = circuit(charger_circuit);
        let d = RailDomains::new(&c, &[], &[]);
        let out = c.find_node("+5V").unwrap();
        assert_eq!(d.drivers_of(out).len(), 1);
        assert!(d.setters_of(out).is_empty());
        assert!(d.fed_by_driver(out));
        assert_eq!(d.owned_note(&c, "+5V", out, |_| None), None);
        assert!(d.conflicts(&c).is_empty());
    }

    #[test]
    fn a_hard_source_beside_a_limited_one_is_the_only_setter() {
        let c = circuit(|c, nodes| {
            charger_circuit(c, nodes);
            c.add(Device::vsource(
                "Vreg_U2",
                nodes[2],
                NodeId::GROUND,
                SourceKind::Dc(5.0),
            ));
        });
        let d = RailDomains::new(&c, &[], &[]);
        let out = c.find_node("+5V").unwrap();
        assert_eq!(d.drivers_of(out).len(), 2);
        assert_eq!(d.setters_of(out).len(), 1);
        assert_eq!(d.setters_of(out)[0].by, "U2");
        assert!(d.conflicts(&c).is_empty());
        assert!(d.owned_note(&c, "+5V", out, |_| None).is_some());
    }

    #[test]
    fn two_regulators_on_one_rail_conflict_but_a_filter_resistor_does_not_join() {
        let c = circuit(|c, [vin, _, out, ana]| {
            c.add(Device::vsource(
                "Vreg_U1",
                out,
                NodeId::GROUND,
                SourceKind::Dc(5.0),
            ));
            c.add(Device::vsource(
                "Vreg_U2",
                ana,
                NodeId::GROUND,
                SourceKind::Dc(5.0),
            ));
            c.add(Device::resistor("R1", vin, out, 10.0));
        });
        let d = RailDomains::new(&c, &[], &[]);
        assert!(d.conflicts(&c).is_empty());
        let c = circuit(|c, [_, _, out, ana]| {
            c.add(Device::vsource(
                "Vreg_U1",
                out,
                NodeId::GROUND,
                SourceKind::Dc(5.0),
            ));
            c.add(Device::vsource(
                "Vreg_U2",
                ana,
                NodeId::GROUND,
                SourceKind::Dc(5.0),
            ));
            c.add(Device::resistor("R0", out, ana, 0.0));
        });
        let conflicts = RailDomains::new(&c, &[], &[]).conflicts(&c);
        assert_eq!(conflicts.len(), 1, "{conflicts:?}");
        assert!(conflicts[0]
            .message
            .contains("U1 on '+5V' and U2 on '+5V_A'"));
        assert_eq!(conflicts[0].nets, ["+5V", "+5V_A"]);
    }

    #[test]
    fn a_switch_feeds_a_branch_without_joining_it_and_a_shunt_drives_its_cathode() {
        let c = circuit(|c, [vin, sw, out, ana]| {
            c.add(Device::vsource(
                "Vreg_U1",
                out,
                NodeId::GROUND,
                SourceKind::Dc(5.0),
            ));
            c.add(Device::VSwitch {
                name: "U2".into(),
                a: out,
                b: ana,
                ctrl_p: vin,
                ctrl_n: NodeId::GROUND,
                von: 1.0,
                voff: 0.5,
                ron: 0.05,
                roff: 1e9,
            });
            c.add(Device::Vccs {
                name: format!("{SHUNT_SINK_PREFIX}U3"),
                p: sw,
                n: NodeId::GROUND,
                cp: vin,
                cn: NodeId::GROUND,
                gm: 0.1,
            });
        });
        let [vin, sw, out, ana] = ["+12V", "SW", "+5V", "+5V_A"].map(|n| c.find_node(n).unwrap());
        let mut d = RailDomains::new(&c, &[], &[]);
        assert!(!d.joined(out, ana) && !d.fed_by_driver(ana));
        d.join_switches(&c, |n| n != ana);
        assert!(
            !d.fed_by_driver(ana),
            "a switch to a private node feeds nothing"
        );
        d.join_switches(&c, |_| true);
        assert!(d.fed_joined(out, ana) && d.fed_by_driver(ana));
        assert!(d.drivers_of(ana).is_empty(), "switched, not one rail");
        assert_eq!(d.drivers_of(sw)[0].by, "U3");
        assert!(!d.fed_by_driver(vin));
    }
}
