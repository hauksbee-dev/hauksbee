//! USB Type-C CC attach classifier (generic, reusable infrastructure).
//!
//! A USB Type-C *source* (charger / DFP) decides whether to apply VBUS by
//! asserting a pull-up termination (Rp, modelled per spec as a current source)
//! on each of the two CC pins and reading the resulting CC voltage. The voltage
//! on each pin lands in one of three windows, which the spec calls vRa
//! (a powered-cable/VCONN marker, Ra), vRd (a sink's Rd pull-down is present),
//! or vOPEN (nothing there). The *pair* of pin states then maps to a port
//! state: Sink attached, powered cable, audio adapter accessory, debug
//! accessory, or nothing (USB Type-C spec R1.3, Table 4-10 "Source
//! Perspective").
//!
//! This module is deliberately independent of any one board. Given a sink's CC
//! termination (the resistance each CC pin presents to GND, and whether the two
//! pins are actually the *same* net), plus a chosen source Rp level and a cable
//! model, it builds the source+cable+sink resistor network as a [`Circuit`],
//! solves the DC operating point with the production solver, reads the two CC
//! voltages, and classifies them against the spec thresholds.
//!
//! The numbers it compares against are normative, taken straight from the USB
//! Type-C Cable and Connector Specification Release 1.3 (July 2017):
//!   - Table 4-20: Source Rp current-source values (Default 80 µA, 1.5 A
//!     180 µA, 3.0 A 330 µA).
//!   - Table 4-21: Sink Rd = 5.1 kΩ.
//!   - Table 4-22: powered-cable Ra = 800 Ω to 1.2 kΩ.
//!   - Tables 4-28/4-29/4-30: the source-side CC voltage windows (vRa / vRd /
//!     vOPEN) per Rp level.
//!
//! The RPi 4 reconstruction in `board-corpus/famous/rpi4_usbc_reconstruction/`
//! is the flagship caller: its as-designed CC subcircuit ties CC1 and CC2 to a
//! single shared 5.1 kΩ (R79), and this classifier re-derives, cold, the famous
//! "e-marked charger refuses to power the Pi" fault from the topology and these
//! thresholds alone.

use hauksbee_extract::ExtractedBoard;
use hauksbee_ir::{Circuit, Device, NodeId, SourceKind};
use hauksbee_models::value::parse_value;
use hauksbee_solve::{dc_operating_point, SolverOptions, Workspace};

/// The Rp termination a source advertises, per USB Type-C spec Table 4-20.
/// Modelled as a current source into the CC pin (the spec's "Current
/// Source/Pull-Down CC Model", Figure 4-6), which is the model the source-side
/// voltage windows in Tables 4-28/4-29/4-30 are defined against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rp {
    /// Default USB Power: 80 µA, source-side thresholds vRa<0.20 V, vRd 0.20..1.60 V.
    Default,
    /// 1.5 A @ 5 V: 180 µA, vRa<0.40 V, vRd 0.40..1.60 V.
    Med1A5,
    /// 3.0 A @ 5 V: 330 µA, vRa<0.80 V, vRd 0.80..2.60 V.
    High3A,
}

impl Rp {
    /// Rp modelled as a current source: the spec current value (A), Table 4-20.
    pub fn current_a(self) -> f64 {
        match self {
            Rp::Default => 80e-6,
            Rp::Med1A5 => 180e-6,
            Rp::High3A => 330e-6,
        }
    }

    /// Rp modelled as a resistor pull-up to 4.75..5.5 V (Table 4-20, middle
    /// column). Provided for the alternative resistive-Rp model; the classifier
    /// defaults to the current-source model.
    pub fn pullup_ohms(self) -> f64 {
        match self {
            Rp::Default => 56_000.0,
            Rp::Med1A5 => 22_000.0,
            Rp::High3A => 10_000.0,
        }
    }

    /// Source-side CC thresholds for this Rp (volts), USB Type-C spec
    /// Tables 4-28 (Default), 4-29 (1.5 A), 4-30 (3.0 A).
    ///
    /// Returns `(vra_threshold, vrd_max_threshold, vopen)`:
    ///   - a CC pin **below** `vra_threshold` reads as Ra (powered cable);
    ///   - **between** `vra_threshold` and `vrd_max_threshold` reads as Rd (a
    ///     sink's pull-down is attached);
    ///   - **at or above** `vopen` reads as open (nothing attached).
    pub fn thresholds(self) -> CcThresholds {
        match self {
            // Table 4-28: vRa max 0.20 V, vRd 0.25..1.50 V (threshold 1.60 V), vOPEN 1.65 V.
            Rp::Default => CcThresholds { vra_max: 0.20, vrd_max: 1.60, vopen: 1.65 },
            // Table 4-29: vRa max 0.40 V, vRd 0.45..1.50 V (threshold 1.60 V), vOPEN 1.65 V.
            Rp::Med1A5 => CcThresholds { vra_max: 0.40, vrd_max: 1.60, vopen: 1.65 },
            // Table 4-30: vRa max 0.80 V, vRd 0.85..2.45 V (threshold 2.60 V), vOPEN 2.75 V.
            Rp::High3A => CcThresholds { vra_max: 0.80, vrd_max: 2.60, vopen: 2.75 },
        }
    }
}

/// The source-side CC voltage thresholds for one Rp advertisement (volts).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CcThresholds {
    /// A CC voltage strictly below this reads as Ra (powered cable / VCONN).
    pub vra_max: f64,
    /// At/above `vra_max` and below this reads as Rd (sink attached).
    pub vrd_max: f64,
    /// At/above this reads as open (no connect).
    pub vopen: f64,
}

/// The per-pin termination the source resolves a CC voltage into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinState {
    /// Below vRa: a powered cable's Ra (or anything pulling the pin near GND).
    Ra,
    /// In the vRd window: a sink's Rd pull-down.
    Rd,
    /// At/above vOPEN: nothing attached.
    Open,
}

impl PinState {
    /// Classify a solved CC voltage against the source thresholds for an Rp.
    pub fn classify(v: f64, t: CcThresholds) -> PinState {
        if v >= t.vopen {
            PinState::Open
        } else if v < t.vra_max {
            PinState::Ra
        } else {
            // Between vra_max and vopen. The Rd window's upper threshold sits
            // below vOPEN; a voltage above vrd_max but below vOPEN is an
            // out-of-spec grey zone the source does not credit as a sink. We
            // fold it to Open (undetected) rather than invent a fourth state.
            if v < t.vrd_max {
                PinState::Rd
            } else {
                PinState::Open
            }
        }
    }
}

/// The port state a source declares from the (CC1, CC2) pin-state pair, per
/// USB Type-C spec Table 4-10 "Source Perspective".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attach {
    /// Open/Open: nothing attached. No VBUS.
    Nothing,
    /// Rd/Open or Open/Rd: a sink is attached. **VBUS applied.**
    SinkAttached,
    /// Rd/Ra or Ra/Rd: a powered (e-marked) cable with a sink behind it.
    /// **VBUS applied** (the sink is still seen).
    PoweredCableWithSink,
    /// Ra/Open or Open/Ra: a powered cable with no sink. No VBUS.
    PoweredCableNoSink,
    /// Rd/Rd: Debug Accessory Mode. No normal VBUS.
    DebugAccessory,
    /// Ra/Ra: Audio Adapter Accessory Mode. **No VBUS**; this is the RPi 4 fault.
    AudioAccessory,
}

impl Attach {
    /// Whether a compliant source applies VBUS power in this state.
    pub fn powers(self) -> bool {
        matches!(self, Attach::SinkAttached | Attach::PoweredCableWithSink)
    }

    /// Map a (CC1, CC2) pin-state pair to the port state (Table 4-10).
    pub fn from_pins(cc1: PinState, cc2: PinState) -> Attach {
        use PinState::{Open, Ra, Rd};
        match (cc1, cc2) {
            (Open, Open) => Attach::Nothing,
            (Rd, Open) | (Open, Rd) => Attach::SinkAttached,
            (Rd, Ra) | (Ra, Rd) => Attach::PoweredCableWithSink,
            (Ra, Open) | (Open, Ra) => Attach::PoweredCableNoSink,
            (Rd, Rd) => Attach::DebugAccessory,
            (Ra, Ra) => Attach::AudioAccessory,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Attach::Nothing => "Nothing",
            Attach::SinkAttached => "SinkAttached",
            Attach::PoweredCableWithSink => "PoweredCableWithSink",
            Attach::PoweredCableNoSink => "PoweredCableNoSink",
            Attach::DebugAccessory => "DebugAccessory",
            Attach::AudioAccessory => "AudioAccessory",
        }
    }
}

/// A cable between the source and the sink.
///
/// Passive cables wire only one CC line through (the other position in the plug
/// is VCONN, which a passive cable leaves unconnected). An electronically
/// marked ("e-marked") cable powers an e-marker chip from VCONN, presenting Ra
/// (800 Ω to 1.2 kΩ) on the VCONN CC line while passing the other CC through.
#[derive(Debug, Clone, Copy)]
pub enum Cable {
    /// Passive cable: CC1 connects through, CC2 (the VCONN position) is open at
    /// the sink. No Ra anywhere.
    Passive,
    /// E-marked cable: both CC lines connect through, and the e-marker presents
    /// `ra_ohms` from the VCONN CC line (here CC2) to GND. Per Table 4-22 the
    /// nominal Ra is 800 Ω to 1.2 kΩ; pass `1000.0` for the canonical 1 kΩ.
    EMarked { ra_ohms: f64 },
}

impl Cable {
    /// Canonical e-marked cable with a 1.0 kΩ Ra (mid of the 800 Ω..1.2 kΩ band).
    pub fn emarked() -> Cable {
        Cable::EMarked { ra_ohms: 1000.0 }
    }
}

/// The sink's CC termination as seen at the receptacle: the resistance each CC
/// pin presents to GND, and whether the two CC pins are physically the *same*
/// net (the RPi 4 defect).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SinkTermination {
    /// Ohms from CC1 to GND (the parallel combination of every resistor found
    /// on the CC1 net to GND). `None` if CC1 has no resistive path to GND.
    pub cc1_rd_ohms: Option<f64>,
    /// Ohms from CC2 to GND. `None` if CC2 has no path to GND.
    pub cc2_rd_ohms: Option<f64>,
    /// True when CC1 and CC2 resolve to the *same* electrical net (so a single
    /// shared pulldown terminates both). This is the RPi 4 rev 1.0/1.1 bug.
    pub shared_net: bool,
}

/// The full result of attaching a source + cable to a sink and classifying.
#[derive(Debug, Clone)]
pub struct CcResult {
    /// Solved CC1 voltage at the receptacle (V).
    pub cc1_v: f64,
    /// Solved CC2 voltage at the receptacle (V).
    pub cc2_v: f64,
    pub cc1_state: PinState,
    pub cc2_state: PinState,
    pub attach: Attach,
    /// The Rp advertisement used.
    pub rp: Rp,
    /// The thresholds applied (echoed for reporting).
    pub thresholds: CcThresholds,
}

impl CcResult {
    /// Whether the source applies VBUS.
    pub fn powers(&self) -> bool {
        self.attach.powers()
    }
}

/// Attach a source (asserting `rp`) through `cable` to a sink with the given
/// `term`, solve the CC network, and classify.
///
/// The network solved (current-source Rp model):
///   - node CC1: Rp current source `rp.current_a()` injected; if the cable
///     wires CC1 through, it reaches the sink's CC1 termination to GND.
///   - node CC2: same, gated by whether the cable wires CC2 through; plus the
///     cable's Ra to GND on CC2 when e-marked.
///   - the sink's Rd(s) to GND, honouring `shared_net` (one node) or two
///     independent nodes.
///
/// When `term.shared_net` is true the two source CC pins drive a single node,
/// which is exactly why the RPi 4 reads two Ra terminations under an e-marked
/// cable.
pub fn classify_attach(term: SinkTermination, rp: Rp, cable: Cable) -> CcResult {
    let mut c = Circuit::new();

    // The sink CC node(s). Shared => one node both pins land on.
    let cc1_sink = c.node("CC1");
    let cc2_sink = if term.shared_net { cc1_sink } else { c.node("CC2") };

    // Sink Rd terminations to GND (the board's pulldowns).
    if let Some(r) = term.cc1_rd_ohms {
        c.add(Device::Resistor {
            name: "Rd1".into(),
            a: cc1_sink,
            b: NodeId::GROUND,
            ohms: r,
            tc1: None,
        });
    }
    // When shared, cc2's resistor is the same physical R79 already added via
    // cc1; adding it again would halve the resistance. Only add cc2's Rd when
    // it is an independent net.
    if !term.shared_net {
        if let Some(r) = term.cc2_rd_ohms {
            c.add(Device::Resistor {
                name: "Rd2".into(),
                a: cc2_sink,
                b: NodeId::GROUND,
                ohms: r,
                tc1: None,
            });
        }
    }

    // Source Rp on CC1: a current source from GND into the CC1 node (current
    // flows p->n internally, so p=GND, n=CC1 injects current *into* CC1).
    // The cable wires CC1 through in both passive and e-marked cases.
    c.add(Device::Isource {
        name: "Rp1".into(),
        p: NodeId::GROUND,
        n: cc1_sink,
        kind: SourceKind::Dc(rp.current_a()),
    });

    // CC2 path depends on the cable.
    match cable {
        Cable::Passive => {
            // A passive cable does not connect the second CC line (that plug
            // position carries VCONN, which a passive cable leaves open). So the
            // source's CC2 pin reaches nothing through the cable, even on the
            // shared-net board where the *sink* ties CC1 and CC2 together: the
            // source still only sees the one CC line the cable wired. We stamp
            // nothing for CC2 here and report it as Open (at vOPEN) below.
        }
        Cable::EMarked { ra_ohms } => {
            // E-marked: CC2 wires through, source asserts Rp on it, and the
            // e-marker presents Ra from CC2 to GND.
            c.add(Device::Isource {
                name: "Rp2".into(),
                p: NodeId::GROUND,
                n: cc2_sink,
                kind: SourceKind::Dc(rp.current_a()),
            });
            c.add(Device::Resistor {
                name: "Ra".into(),
                a: cc2_sink,
                b: NodeId::GROUND,
                ohms: ra_ohms,
                tc1: None,
            });
        }
    }

    // Solve the DC operating point.
    let opts = SolverOptions::default();
    let mut ws = Workspace::new(&c);
    let solved = dc_operating_point(&mut ws, &c, &opts).is_ok();

    let read = |node: NodeId| -> f64 {
        if node.is_ground() {
            0.0
        } else {
            ws.layout
                .node(node)
                .and_then(|i| ws.x.get(i).copied())
                .unwrap_or(0.0)
        }
    };

    let th = rp.thresholds();

    let cc1_v = if solved { read(cc1_sink) } else { 0.0 };
    // For a passive cable the source CC2 pin is open at the sink: report vOPEN
    // and Open state directly (it is not part of the solved network).
    let (cc2_v, cc2_state) = match cable {
        Cable::Passive => (th.vopen, PinState::Open),
        Cable::EMarked { .. } => {
            let v = if solved { read(cc2_sink) } else { 0.0 };
            (v, PinState::classify(v, th))
        }
    };
    let cc1_state = PinState::classify(cc1_v, th);
    let attach = Attach::from_pins(cc1_state, cc2_state);

    CcResult {
        cc1_v,
        cc2_v,
        cc1_state,
        cc2_state,
        attach,
        rp,
        thresholds: th,
    }
}

// ---------------------------------------------------------------------------
// Extracting the sink termination from a parsed board
// ---------------------------------------------------------------------------

/// Find the USB-C receptacle's CC termination on a parsed board.
///
/// Locates the receptacle (a component whose pins carry CC1/CC2 functions, or
/// failing that the conventional A5/B5 pad numbers), identifies the CC1 and CC2
/// nets and the GND net, then for each CC net computes the parallel resistance
/// of every resistor that bridges it to GND. Detects the shared-net defect when
/// CC1 and CC2 resolve to one net.
///
/// Returns `None` if no receptacle with identifiable CC pins is found.
pub fn extract_sink_termination(board: &ExtractedBoard) -> Option<SinkTermination> {
    let gnd_net = gnd_net_id(board);
    let (cc1_net, cc2_net) = receptacle_cc_nets(board)?;
    let shared_net = cc1_net == cc2_net;

    let rd = |cc_net: i64| -> Option<f64> { net_resistance_to(board, cc_net, gnd_net) };

    Some(SinkTermination {
        cc1_rd_ohms: rd(cc1_net),
        cc2_rd_ohms: if shared_net { rd(cc1_net) } else { rd(cc2_net) },
        shared_net,
    })
}

/// The primary GND net id (the net named GND / GNDPWR), if any.
///
/// Kept for the single-ground classifier path. The CC Rd audit uses
/// [`ground_net_ids`] instead, because a USB-C Rd can legitimately return to a
/// secondary ground (GNDA / DGND / a split shield ground) rather than the one
/// net literally named "GND".
fn gnd_net_id(board: &ExtractedBoard) -> Option<i64> {
    board
        .nets
        .iter()
        .find(|n| n.name.eq_ignore_ascii_case("GND") || n.name.eq_ignore_ascii_case("GNDPWR"))
        .map(|n| n.id)
}

/// Whether a net name denotes a ground reference. Recognises the common
/// ground-family names a USB-C Rd can return to: GND, the analog/digital splits
/// (GNDA/AGND, GNDD/DGND), power/earth grounds (GNDPWR/PGND/EGND), shield/USB
/// grounds, and numbered grounds (GND1, GND2, ...). Deliberately conservative:
/// it matches only names whose alphabetic-only core is exactly a ground token,
/// so a signal like "GND_SENSE" or "GNDLED" (an LED return that is *not* the
/// system ground) does not masquerade as ground and credit a phantom Rd.
fn is_ground_name(name: &str) -> bool {
    // Drop digits and separators so "GND1", "GNDA_2", "GND-3" all reduce to
    // their alphabetic core ("GND", "GNDA", "GND").
    let core: String = name
        .to_ascii_uppercase()
        .chars()
        .filter(|c| c.is_ascii_alphabetic())
        .collect();
    matches!(
        core.as_str(),
        "GND"
            | "GNDA"
            | "AGND"
            | "GNDD"
            | "DGND"
            | "GNDPWR"
            | "PGND"
            | "EGND"
            | "GNDREF"
            | "SGND"
            | "GNDUSB"
            | "USBGND"
            | "VSS"
            | "VSSA"
    )
}

/// All net ids that are ground references on the board (see [`is_ground_name`]).
fn ground_net_ids(board: &ExtractedBoard) -> std::collections::HashSet<i64> {
    board
        .nets
        .iter()
        .filter(|n| is_ground_name(&n.name))
        .map(|n| n.id)
        .collect()
}

/// Resolve the receptacle's CC1 and CC2 net ids, preferring the USB-C connector
/// over any downstream CC controller / PMIC that also carries CC pin functions.
fn receptacle_cc_nets(board: &ExtractedBoard) -> Option<(i64, i64)> {
    // The receptacle's CC1/CC2 nets. A CC pin function (or the A5/B5 pad numbers)
    // can appear on *several* components: the USB-C connector itself, AND any CC
    // controller / PMIC the CC lines route into (e.g. the nPM1300 on ZSWatch,
    // whose pads 23/24 carry CC1/CC2 functions on a *different* net than the
    // receptacle). The sink termination we want to classify is the one at the
    // *receptacle*, where the discrete Rd pulldowns live; reading the PMIC-side
    // net instead misses those Rds and manufactures a false "no termination"
    // (PoweredCableNoSink / Nothing) result on a correctly-designed board.
    //
    // So we score each component by how strongly it looks like the USB-C
    // receptacle and prefer the best one. The A5/B5 pad-number fallback is
    // *gated* on the component being a receptacle, because grid-array packages
    // (the nRF5340 module's BGA) legitimately number pads "A5"/"B5" with
    // unrelated functions.
    let mut best_score = i32::MIN;
    let mut cc1_net = None;
    let mut cc2_net = None;
    for comp in &board.components {
        let score = receptacle_score(comp);
        let mut c1 = None;
        let mut c2 = None;
        for pin in &comp.pins {
            let f = pin.function.to_ascii_uppercase();
            let n = pin.number.to_ascii_uppercase();
            if f == "CC1" || f == "CC" || (score > 0 && n == "A5") {
                c1 = c1.or(pin.net);
            }
            if f == "CC2" || (score > 0 && n == "B5") {
                c2 = c2.or(pin.net);
            }
        }
        // Only consider this component if it actually carries a CC1 (or shared CC)
        // pin. Prefer the highest receptacle score; ties keep the first found.
        if c1.is_some() && c2.is_some() && score > best_score {
            best_score = score;
            cc1_net = c1;
            cc2_net = c2;
        }
    }
    Some((cc1_net?, cc2_net?))
}

/// One USB-C receptacle's CC nets, with the component reference that carries
/// them.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ReceptacleNets {
    reference: String,
    cc1_net: i64,
    cc2_net: i64,
}

/// Every distinct USB-C *receptacle* on the board, each with its own CC1/CC2
/// nets. A board with two USB-C halves (e.g. a split keyboard's two MCU
/// receptacles) has two independent receptacles, each with its own discrete Rd;
/// the single-best [`receptacle_cc_nets`] credits only one, so the audit walks
/// all of them.
///
/// "Receptacle" here is gated on a positive [`receptacle_score`]: a downstream
/// CC controller / PMIC that merely carries CC pin functions (score 0) is
/// excluded, exactly as in `receptacle_cc_nets`, so we never read the PMIC-side
/// net as if it were the connector.
fn all_receptacle_cc_nets(board: &ExtractedBoard) -> Vec<ReceptacleNets> {
    let mut out: Vec<ReceptacleNets> = Vec::new();
    for comp in &board.components {
        let score = receptacle_score(comp);
        if score <= 0 {
            continue;
        }
        let mut c1 = None;
        let mut c2 = None;
        for pin in &comp.pins {
            let f = pin.function.to_ascii_uppercase();
            let n = pin.number.to_ascii_uppercase();
            if f == "CC1" || f == "CC" || n == "A5" {
                c1 = c1.or(pin.net);
            }
            if f == "CC2" || n == "B5" {
                c2 = c2.or(pin.net);
            }
        }
        if let (Some(cc1_net), Some(cc2_net)) = (c1, c2) {
            let rec = ReceptacleNets { reference: comp.reference.clone(), cc1_net, cc2_net };
            // De-dupe by the (cc1, cc2) net pair: two receptacles wired to the
            // *same* CC nets (a mirrored footprint of one logical port) count
            // once, so we do not double-report.
            if !out.iter().any(|r| r.cc1_net == rec.cc1_net && r.cc2_net == rec.cc2_net) {
                out.push(rec);
            }
        }
    }
    out
}

/// Parallel resistance (ohms) of every two-pin resistor that connects `from_net`
/// to `gnd_net`. `None` if no such resistor exists (or GND is unknown).
fn net_resistance_to(board: &ExtractedBoard, from_net: i64, gnd_net: Option<i64>) -> Option<f64> {
    let gnd = gnd_net?;
    let mut set = std::collections::HashSet::new();
    set.insert(gnd);
    net_resistance_to_grounds(board, from_net, &set)
}

/// Parallel resistance (ohms) of every two-pin resistor that bridges `from_net`
/// to *any* recognised ground net (see [`ground_net_ids`]). `None` if no such
/// resistor exists or the ground set is empty. This is the audit's Rd reader: a
/// CC Rd that returns to a secondary ground (GNDA, DGND, shield ground) is still
/// a valid sink termination and must be credited.
fn net_resistance_to_grounds(
    board: &ExtractedBoard,
    from_net: i64,
    grounds: &std::collections::HashSet<i64>,
) -> Option<f64> {
    if grounds.is_empty() {
        return None;
    }
    let mut inv_sum = 0.0f64;
    let mut found = false;
    for comp in &board.components {
        // A Do-Not-Populate resistor is on the layout but not assembled, so it
        // presents no resistance: skip it (counting it manufactures a phantom
        // termination, the very false-positive shape this audit must avoid).
        if comp.dnp {
            continue;
        }
        // Only resistors (Device:R or a value that parses to ohms with a
        // resistor-ish reference).
        if !is_resistor(comp) {
            continue;
        }
        let mut on_from = false;
        let mut on_gnd = false;
        for pin in &comp.pins {
            match pin.net {
                Some(id) if id == from_net => on_from = true,
                Some(id) if grounds.contains(&id) => on_gnd = true,
                _ => {}
            }
        }
        if on_from && on_gnd {
            if let Some(v) = parse_value(&comp.value) {
                if v.si > 0.0 {
                    inv_sum += 1.0 / v.si;
                    found = true;
                }
            }
        }
    }
    if found {
        Some(1.0 / inv_sum)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// CC double-termination audit
// ---------------------------------------------------------------------------

/// What a single CC pin presents at the receptacle.
#[derive(Debug, Clone, PartialEq)]
pub struct CcPinTermination {
    /// The discrete external Rd-to-GND resistance found on the receptacle CC net
    /// (parallel of every resistor that bridges the net to GND). `None` if the
    /// CC pin has no external resistor to GND.
    pub external_rd_ohms: Option<f64>,
    /// True when the CC net reaches (directly, or through a 0 Ω / ferrite bridge)
    /// a controller / PMIC that provides its own *internal* Rd of `internal_rd_ohms`.
    pub internal_rd_ohms: Option<f64>,
    /// The reference designator of that internal-Rd controller, when found.
    pub controller_ref: Option<String>,
}

impl CcPinTermination {
    /// The effective Rd a source sees: external Rd in parallel with the
    /// controller's internal Rd (whichever are present).
    pub fn effective_rd_ohms(&self) -> Option<f64> {
        match (self.external_rd_ohms, self.internal_rd_ohms) {
            (Some(e), Some(i)) => Some(1.0 / (1.0 / e + 1.0 / i)),
            (Some(e), None) => Some(e),
            (None, Some(i)) => Some(i),
            (None, None) => None,
        }
    }

    /// True when BOTH an external Rd and a controller's internal Rd terminate the
    /// same CC line: the double-termination defect. The effective Rd is then well
    /// below the nominal 5.1 kΩ a source expects, so the source under-reads the
    /// CC voltage and mis-detects the advertised current.
    pub fn is_double_terminated(&self) -> bool {
        self.external_rd_ohms.is_some() && self.internal_rd_ohms.is_some()
    }
}

/// One USB-C receptacle's CC-termination audit, tagged with its reference.
#[derive(Debug, Clone)]
pub struct ReceptacleCc {
    /// The receptacle's reference designator (e.g. "J1").
    pub reference: String,
    pub cc1: CcPinTermination,
    pub cc2: CcPinTermination,
}

impl ReceptacleCc {
    /// True when either CC pin of this receptacle is double-terminated.
    pub fn has_double_termination(&self) -> bool {
        self.cc1.is_double_terminated() || self.cc2.is_double_terminated()
    }
}

/// The CC-termination audit for a board's USB-C receptacle(s).
///
/// `cc1`/`cc2` are the *primary* receptacle (the highest-scoring one, first on
/// ties) and are retained for single-receptacle callers. `receptacles` lists
/// every distinct USB-C receptacle on the board, each with its own CC
/// terminations: on a dual-receptacle board (e.g. a split keyboard's two
/// halves) this is where the second receptacle's independent Rd is credited,
/// which the single `cc1`/`cc2` pair cannot represent.
#[derive(Debug, Clone)]
pub struct CcTerminationAudit {
    pub cc1: CcPinTermination,
    pub cc2: CcPinTermination,
    /// Every distinct receptacle audited (at least one; the first is the
    /// primary, mirrored into `cc1`/`cc2`).
    pub receptacles: Vec<ReceptacleCc>,
}

impl CcTerminationAudit {
    /// True when *any* receptacle's CC pin carries both an external and an
    /// internal Rd (the double-termination defect, on any half of the board).
    pub fn has_double_termination(&self) -> bool {
        self.receptacles.iter().any(|r| r.has_double_termination())
    }

    /// True when *every* receptacle presents a credited Rd on at least one CC
    /// pin (external or internal). A receptacle with no Rd at all is either a
    /// power-only / debug receptacle or an under-read; this lets a caller
    /// distinguish "all halves terminated" from "primary terminated only".
    pub fn all_receptacles_terminated(&self) -> bool {
        !self.receptacles.is_empty()
            && self.receptacles.iter().all(|r| {
                r.cc1.effective_rd_ohms().is_some() || r.cc2.effective_rd_ohms().is_some()
            })
    }
}

/// Audit the receptacle's CC1/CC2 terminations for the double-Rd defect: a
/// discrete external 5.1 kΩ Rd on a CC line that *also* routes into a PMIC /
/// controller which already provides an internal Rd of its own (e.g. the Nordic
/// nPM1300, whose datasheet states its CC pins "have internal pull-downs with
/// resistance equal to Rd" = 5.1 kΩ). Both in parallel halve the effective Rd to
/// ~2.55 kΩ, dragging the CC voltage out of spec.
///
/// Returns `None` if the board has no identifiable USB-C receptacle CC nets.
pub fn audit_cc_termination(board: &ExtractedBoard) -> Option<CcTerminationAudit> {
    let grounds = ground_net_ids(board);

    // Audit every distinct receptacle. Fall back to the single-best resolver
    // when the multi-receptacle walk finds none (it gates strictly on a
    // positive receptacle score; the single resolver is more permissive about
    // shared-CC parts, which preserves the RPi 4 schematic-reconstruction case
    // whose CC pins live on a symbol that scores 0).
    let mut receptacles: Vec<ReceptacleCc> = all_receptacle_cc_nets(board)
        .into_iter()
        .map(|r| ReceptacleCc {
            reference: r.reference,
            cc1: audit_one_cc(board, r.cc1_net, &grounds),
            cc2: audit_one_cc(board, r.cc2_net, &grounds),
        })
        .collect();

    if receptacles.is_empty() {
        let (cc1_net, cc2_net) = receptacle_cc_nets(board)?;
        receptacles.push(ReceptacleCc {
            reference: String::new(),
            cc1: audit_one_cc(board, cc1_net, &grounds),
            cc2: audit_one_cc(board, cc2_net, &grounds),
        });
    }

    let primary = receptacles[0].clone();
    Some(CcTerminationAudit { cc1: primary.cc1, cc2: primary.cc2, receptacles })
}

fn audit_one_cc(
    board: &ExtractedBoard,
    cc_net: i64,
    grounds: &std::collections::HashSet<i64>,
) -> CcPinTermination {
    let external_rd_ohms = net_resistance_to_grounds(board, cc_net, grounds);
    // Walk the CC net plus anything bridged to it by a 0 Ω resistor / ferrite,
    // and see if it reaches a known internal-Rd controller's CC pin.
    let reachable = nets_bridged_to(board, cc_net);
    let mut internal_rd_ohms = None;
    let mut controller_ref = None;
    for comp in &board.components {
        if comp.dnp {
            continue;
        }
        if let Some(internal) = internal_cc_rd_ohms(&comp.value) {
            // Does this controller land a CC pin on one of the reachable nets?
            let on_cc = comp.pins.iter().any(|p| {
                let f = p.function.to_ascii_uppercase();
                (f == "CC1" || f == "CC2" || f == "CC")
                    && p.net.map(|n| reachable.contains(&n)).unwrap_or(false)
            });
            if on_cc {
                internal_rd_ohms = Some(internal);
                controller_ref = Some(comp.reference.clone());
                break;
            }
        }
    }
    CcPinTermination { external_rd_ohms, internal_rd_ohms, controller_ref }
}

/// The set of net ids electrically continuous with `start` through 0 Ω resistors
/// / ferrite beads (DC shorts). A CC line often reaches the controller through a
/// 0 Ω "bridge" resistor placed on its own net, so the controller's CC pin is a
/// different net id than the receptacle CC pin; this union re-joins them.
fn nets_bridged_to(board: &ExtractedBoard, start: i64) -> std::collections::HashSet<i64> {
    use std::collections::HashSet;
    let mut set = HashSet::new();
    set.insert(start);
    // Iterate to a fixed point (bridges can chain).
    loop {
        let mut grew = false;
        for comp in &board.components {
            if comp.dnp || !is_dc_bridge(comp) {
                continue;
            }
            let nets: Vec<i64> = comp.pins.iter().filter_map(|p| p.net).collect();
            // A 0 Ω / ferrite ties all its pins' nets together. If any is in the
            // set, add the rest.
            if nets.iter().any(|n| set.contains(n)) {
                for n in nets {
                    if set.insert(n) {
                        grew = true;
                    }
                }
            }
        }
        if !grew {
            break;
        }
    }
    set
}

/// A component that is a DC short between its two terminals: a 0 Ω resistor or a
/// ferrite bead / inductor (which is ~0 Ω at DC). Used to bridge CC nets.
fn is_dc_bridge(comp: &hauksbee_extract::Component) -> bool {
    let r = comp.reference.to_ascii_uppercase();
    let v = comp.value.to_ascii_uppercase();
    // 0 Ω resistor.
    if is_resistor(comp) {
        let v0 = v.trim_end_matches('R').trim_end_matches("OHM").trim();
        if v0 == "0" || v == "0R" || v == "0" || v == "0OHM" || v == "0.0" {
            return true;
        }
    }
    // Ferrite bead (FB/L prefix) - a DC short.
    r.starts_with("FB") || (r.starts_with('L') && r.len() <= 4)
}

/// The internal CC Rd (ohms) a controller provides, keyed on its value/part
/// number. Only parts whose datasheet *states* an integrated Rd belong here; the
/// discriminating citation is recorded in the doc. Returns `None` for parts that
/// present no internal CC termination (so an external Rd on their CC line is
/// correct, not doubled).
fn internal_cc_rd_ohms(value: &str) -> Option<f64> {
    let v = value.to_ascii_uppercase();
    // Nordic nPM1300: "These pins [CC1, CC2] have internal pull-downs with
    // resistance equal to Rd" and the electrical table gives Rd = 5.1 kΩ
    // (nPM1300 Product Specification v1.1, sections 6.1.3 and the Rd parameter).
    if v.contains("NPM1300") {
        return Some(5100.0);
    }
    None
}

/// Score how strongly a component looks like the USB-C *receptacle* (the
/// connector), so the CC termination is read at the receptacle and not at a
/// downstream CC controller / PMIC that also carries CC1/CC2 pin functions.
///
/// Higher is more receptacle-like. A score > 0 also gates the A5/B5 pad-number
/// fallback (grid-array packages number pads "A5"/"B5" with unrelated
/// functions, so the fallback must only fire on something that really is a
/// connector).
fn receptacle_score(comp: &hauksbee_extract::Component) -> i32 {
    let fp = comp.footprint.to_ascii_lowercase();
    let lib = comp.lib_id.to_ascii_lowercase();
    let val = comp.value.to_ascii_lowercase();
    let r = comp.reference.to_ascii_uppercase();
    let mut s = 0;
    // Strongest signal: a footprint that names a USB-C / Type-C receptacle.
    if fp.contains("usb_c") || fp.contains("type_c") || fp.contains("type-c") || fp.contains("usb-c")
    {
        s += 100;
    }
    if fp.contains("receptacle") {
        s += 40;
    }
    // A connector library or value.
    if lib.contains("connector") || lib.contains("conn:") || val.contains("usb") {
        s += 10;
    }
    // A connector reference prefix (J / CN / CON / X / P). These are the
    // designators boards give the receptacle; a PMIC/controller is U/IC.
    if r.starts_with('J') || r.starts_with("CN") || r.starts_with("CON") || r.starts_with('X') {
        s += 5;
    }
    s
}

/// Heuristic: is this component a resistor?
fn is_resistor(comp: &hauksbee_extract::Component) -> bool {
    let lib = comp.lib_id.to_ascii_uppercase();
    lib == "DEVICE:R"
        || lib.ends_with(":R")
        || lib.contains(":R_")
        || comp.reference.starts_with('R')
}

/// Convenience: parse a board, extract its sink CC termination, attach the given
/// source + cable, and classify. Returns `None` if the board carries no
/// identifiable USB-C receptacle CC termination.
pub fn classify_board(board: &ExtractedBoard, rp: Rp, cable: Cable) -> Option<CcResult> {
    let term = extract_sink_termination(board)?;
    Some(classify_attach(term, rp, cable))
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Spec constants (Tables 4-20, 4-21, 4-22) ---------------------------

    #[test]
    fn rp_current_values_match_spec_table_4_20() {
        assert_eq!(Rp::Default.current_a(), 80e-6);
        assert_eq!(Rp::Med1A5.current_a(), 180e-6);
        assert_eq!(Rp::High3A.current_a(), 330e-6);
    }

    #[test]
    fn rp_resistor_pullups_match_spec_table_4_20() {
        assert_eq!(Rp::Default.pullup_ohms(), 56_000.0);
        assert_eq!(Rp::Med1A5.pullup_ohms(), 22_000.0);
        assert_eq!(Rp::High3A.pullup_ohms(), 10_000.0);
    }

    #[test]
    fn emarked_cable_ra_in_spec_band() {
        // Table 4-22: Ra is 800 Ohm to 1.2 kOhm. The canonical helper uses 1 kOhm.
        if let Cable::EMarked { ra_ohms } = Cable::emarked() {
            assert!((800.0..=1200.0).contains(&ra_ohms), "Ra {ra_ohms} out of 800..1200");
        } else {
            panic!("emarked() must be an e-marked cable");
        }
    }

    // --- Source-side thresholds (Tables 4-28/29/30) -------------------------

    #[test]
    fn default_rp_thresholds_match_table_4_28() {
        let t = Rp::Default.thresholds();
        assert_eq!(t.vra_max, 0.20);
        assert_eq!(t.vrd_max, 1.60);
        assert_eq!(t.vopen, 1.65);
    }

    #[test]
    fn med_and_high_rp_thresholds_match_tables_4_29_4_30() {
        let m = Rp::Med1A5.thresholds();
        assert_eq!((m.vra_max, m.vrd_max, m.vopen), (0.40, 1.60, 1.65));
        let h = Rp::High3A.thresholds();
        assert_eq!((h.vra_max, h.vrd_max, h.vopen), (0.80, 2.60, 2.75));
    }

    #[test]
    fn pin_state_windows_for_default_rp() {
        let t = Rp::Default.thresholds();
        // Below 0.20 V => Ra.
        assert_eq!(PinState::classify(0.0, t), PinState::Ra);
        assert_eq!(PinState::classify(0.1338, t), PinState::Ra);
        assert_eq!(PinState::classify(0.199, t), PinState::Ra);
        // 0.20..1.60 => Rd.
        assert_eq!(PinState::classify(0.20, t), PinState::Rd);
        assert_eq!(PinState::classify(0.408, t), PinState::Rd);
        assert_eq!(PinState::classify(1.5, t), PinState::Rd);
        // 1.65 and up => Open.
        assert_eq!(PinState::classify(1.65, t), PinState::Open);
        assert_eq!(PinState::classify(3.3, t), PinState::Open);
    }

    #[test]
    fn ra_threshold_scales_with_rp() {
        // At High Rp (330 µA) the same 0.30 V that would be Rd at Default is
        // still Ra, because the vRa window widens to 0.80 V (Table 4-30).
        assert_eq!(PinState::classify(0.30, Rp::Default.thresholds()), PinState::Rd);
        assert_eq!(PinState::classify(0.30, Rp::High3A.thresholds()), PinState::Ra);
    }

    // --- Table 4-10 Source Perspective pair mapping -------------------------

    #[test]
    fn attach_pairs_match_table_4_10() {
        use Attach::*;
        use PinState::{Open, Ra, Rd};
        assert_eq!(Attach::from_pins(Open, Open), Nothing);
        assert_eq!(Attach::from_pins(Rd, Open), SinkAttached);
        assert_eq!(Attach::from_pins(Open, Rd), SinkAttached);
        assert_eq!(Attach::from_pins(Rd, Ra), PoweredCableWithSink);
        assert_eq!(Attach::from_pins(Ra, Rd), PoweredCableWithSink);
        assert_eq!(Attach::from_pins(Ra, Open), PoweredCableNoSink);
        assert_eq!(Attach::from_pins(Open, Ra), PoweredCableNoSink);
        assert_eq!(Attach::from_pins(Rd, Rd), DebugAccessory);
        assert_eq!(Attach::from_pins(Ra, Ra), AudioAccessory);
    }

    #[test]
    fn only_sink_states_power() {
        assert!(Attach::SinkAttached.powers());
        assert!(Attach::PoweredCableWithSink.powers());
        assert!(!Attach::Nothing.powers());
        assert!(!Attach::PoweredCableNoSink.powers());
        assert!(!Attach::DebugAccessory.powers());
        assert!(!Attach::AudioAccessory.powers());
    }

    // --- Solver cross-checks against hand arithmetic ------------------------

    #[test]
    fn single_rd_divider_is_ohms_law() {
        // One CC pin, 80 µA into a lone 5.1k Rd => 0.408 V exactly.
        let term = SinkTermination {
            cc1_rd_ohms: Some(5100.0),
            cc2_rd_ohms: None,
            shared_net: false,
        };
        let r = classify_attach(term, Rp::Default, Cable::Passive);
        let want = 80e-6 * 5100.0; // 0.408 V
        assert!((r.cc1_v - want).abs() < 1e-4, "got {} want {}", r.cc1_v, want);
    }

    // --- CC double-termination audit (pure-logic) --------------------------

    #[test]
    fn effective_rd_is_parallel_when_doubled() {
        let t = CcPinTermination {
            external_rd_ohms: Some(5100.0),
            internal_rd_ohms: Some(5100.0),
            controller_ref: Some("IC401".into()),
        };
        assert!(t.is_double_terminated());
        // 5.1k || 5.1k = 2.55k exactly.
        assert!((t.effective_rd_ohms().unwrap() - 2550.0).abs() < 1e-6);
    }

    #[test]
    fn external_only_or_internal_only_is_not_doubled() {
        let ext = CcPinTermination {
            external_rd_ohms: Some(5100.0),
            internal_rd_ohms: None,
            controller_ref: None,
        };
        assert!(!ext.is_double_terminated());
        assert_eq!(ext.effective_rd_ohms(), Some(5100.0));

        let int = CcPinTermination {
            external_rd_ohms: None,
            internal_rd_ohms: Some(5100.0),
            controller_ref: Some("IC401".into()),
        };
        assert!(!int.is_double_terminated());
        assert_eq!(int.effective_rd_ohms(), Some(5100.0));
    }

    #[test]
    fn internal_cc_rd_table_only_knows_cited_parts() {
        // nPM1300: datasheet states internal Rd = 5.1k on CC pins.
        assert_eq!(internal_cc_rd_ohms("nPM1300-QEXX"), Some(5100.0));
        assert_eq!(internal_cc_rd_ohms("NPM1300"), Some(5100.0));
        // Anything not on the cited list must return None, so an external Rd on
        // its CC line is treated as the *only* termination, not a double.
        assert_eq!(internal_cc_rd_ohms("TUSB320"), None);
        assert_eq!(internal_cc_rd_ohms("STUSB4500"), None);
        assert_eq!(internal_cc_rd_ohms("5k1"), None);
    }

    #[test]
    fn ground_names_recognise_the_gnd_family_only() {
        // The system grounds a USB-C Rd can legitimately return to.
        for g in ["GND", "GNDA", "AGND", "GNDD", "DGND", "GNDPWR", "PGND", "VSS", "VSSA"] {
            assert!(is_ground_name(g), "{g} should be a ground");
        }
        // Numeric / separated suffixes still reduce to a ground core.
        for g in ["GND1", "GND2", "GNDA_2", "GND-3", "gnda"] {
            assert!(is_ground_name(g), "{g} should be a ground");
        }
        // Things that merely *contain* "GND" but are not the system ground must
        // not be credited as a Rd return (the false-positive shape to avoid).
        for n in ["GND_SENSE", "GNDLED", "VBUS", "CC1", "USB_DP", "EARTH", "GNDSW"] {
            assert!(!is_ground_name(n), "{n} must not be treated as ground");
        }
    }

    #[test]
    fn shared_node_parallel_resistance_is_exact() {
        // As-designed + e-marked: two 80 µA sources into 5.1k || 1k.
        let term = SinkTermination {
            cc1_rd_ohms: Some(5100.0),
            cc2_rd_ohms: Some(5100.0),
            shared_net: true,
        };
        let r = classify_attach(term, Rp::Default, Cable::emarked());
        let rpar = 1.0 / (1.0 / 5100.0 + 1.0 / 1000.0); // 836.066 Ohm
        let want = (80e-6 + 80e-6) * rpar; // 0.133771 V
        assert!((r.cc1_v - want).abs() < 1e-4, "got {} want {}", r.cc1_v, want);
        assert!((r.cc2_v - want).abs() < 1e-4, "got {} want {}", r.cc2_v, want);
    }
}
