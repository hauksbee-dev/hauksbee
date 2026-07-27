//! Boot-safety advisory: the business logic behind the co-sim's power-up
//! findings, kept in the library so the CLI, the TUI and the web front door all
//! derive the same advisory from the same call rather than each re-deriving it.
//! Long-form how-and-why: docs/how-and-why/hauksbee-engine/checks.md.
//!
//! Two things are computed from a finished co-sim's firmware drive sets and the
//! board's topology:
//!
//! 1. **Held-high control-net hazards** ([`BootAdvisory::held_high_control_nets`]):
//!    a control net the firmware drives (or pulls) HIGH and holds from reset, that
//!    switches a transistor/relay and has no bias resistor setting a safe default,
//!    a MOSFET gate / relay / motor enable / igniter energised at power-up. The
//!    switch requirement is the zero-false-positive guard.
//! 2. **Per-gate power-up state** ([`BootAdvisory::gate_states`]): what the firmware
//!    does to each transistor gate at reset (driven HIGH / pulled HIGH / driven
//!    LOW / floating), reported factually rather than judged.
//!
//! The predicates and the rendering both live here, so every surface reads the
//! same advisory; the CLI is a thin caller.

use std::collections::HashSet;

use hauksbee_extract::{Component, ExtractedBoard};

/// The structured boot-safety advisory for a finished co-sim run. Built by
/// [`analyze`]; rendered by the CLI (`--plain`/`--json`), the TUI and the web.
#[derive(Debug, Clone, Default)]
pub struct BootAdvisory {
    /// Control nets that switch a transistor/relay, are held HIGH from power-up,
    /// and have no bias resistor setting a safe default; the heads-up hazards.
    pub held_high_control_nets: Vec<String>,
    /// Per-transistor-gate power-up state rows for the informational panel. Empty
    /// when the firmware did not run (nothing drove the pins, so every gate would
    /// read "floating", which says nothing about the design).
    pub gate_states: Vec<(String, String, BootGateState)>,
}

/// Derive the boot advisory from a finished co-sim's firmware drive sets.
///
/// `firmware_held_high` is the UNFILTERED set of nets the firmware held high (a
/// factual level); `output_configured` is the set it drove as outputs (splits a
/// strong HIGH from a weak pull-up); `driven` is the union of output-configured
/// and written nets. `firmware_ran` is `true` only when firmware was supplied and
/// actually exercised the board; the gate-state panel is suppressed otherwise.
pub fn analyze(
    board: &ExtractedBoard,
    firmware_held_high: &[String],
    output_configured: &[String],
    driven: &[String],
    firmware_ran: bool,
) -> BootAdvisory {
    // The heads-up hazards: held-high nets that switch a load and have no bias.
    let held_high_control_nets: Vec<String> = firmware_held_high
        .iter()
        .filter(|net| net_drives_a_switch(board, net) && net_has_no_bias_resistor(board, net))
        .cloned()
        .collect();

    // The informational panel, only when the firmware actually ran.
    let gate_states = if firmware_ran {
        let gates = transistor_gate_nets(board);
        let held_high: HashSet<String> = firmware_held_high.iter().cloned().collect();
        let configured: HashSet<String> = output_configured.iter().cloned().collect();
        let driven: HashSet<String> = driven.iter().cloned().collect();
        let mut rows = boot_gate_states(&gates, &held_high, &configured, &driven);
        // Downgrade the false "floating" rows: a gate the firmware never drives
        // but a bias resistor holds at a rail is NOT floating, it is resistively
        // defined (the reverse-polarity P-FET with a 100k gate pulldown). Replace
        // the warning-level Floating row with a factual, non-warning bias note
        // naming the resistor. A gate with NO bias network stays Floating and
        // fires exactly as before.
        for (_, net, state) in rows.iter_mut() {
            if *state == BootGateState::Floating {
                if let Some(bias) = gate_bias(board, net) {
                    *state = BootGateState::HeldByBias(bias);
                }
            }
        }
        rows
    } else {
        Vec::new()
    };

    BootAdvisory {
        held_high_control_nets,
        gate_states,
    }
}

/// True when a net has no *bias* resistor, no resistor tying it toward a power
/// rail or ground, so nothing on the board fixes its power-up level (it is set
/// entirely by firmware). Used to sharpen the boot-control-net heads-up to nets
/// with no hardware fail-safe. A resistor whose other terminal is NOT a rail or
/// ground is a series element (e.g. GPIO -> R -> MOSFET gate), which sets no
/// default level, so it does NOT count as a bias and the net is still flagged.
/// An unknown/unresolvable net name returns false (assume biased; stay silent).
///
/// "Resistor" is the strict shared predicate (`super::straps::is_assembled_resistor`):
/// plain R refs only. An RV varistor / RT thermistor / RN network is `R…`-prefixed
/// but sets no DC level (a varistor is high-impedance below its clamp), so crediting
/// one as a bias would silently suppress a real boot hazard.
pub fn net_has_no_bias_resistor(board: &ExtractedBoard, net_name: &str) -> bool {
    let Some(net) = board.nets.iter().find(|n| n.name == net_name) else {
        return false;
    };
    for (comp, _) in board.net_members(net.id) {
        // A DNP (not-assembled) resistor is electrically absent, and a
        // varistor/thermistor/network/ferrite is not a bias-setting resistor:
        // neither may be credited (that would suppress a real hazard).
        if !super::straps::is_assembled_resistor(comp) {
            continue;
        }
        for p in &comp.pins {
            if let Some(other) = p.net {
                if other != net.id && is_power_or_ground_net(board, other) {
                    return false; // a pull-up/down to a rail/ground: a hardware default exists
                }
            }
        }
    }
    true
}

/// The bias resistor that fixes a gate net's power-up level, if one exists: a
/// resistor tying the net to a rail/ground, directly (1 hop) or through one
/// series resistor (2 hops, a plain pull chain or divider). Returns the resistor
/// *on the gate net* (its reference + value) and the rail it reaches, so the
/// boot panel can report "held low by R1 (100k to GND)" instead of "floating".
///
/// This is the read-out twin of [`net_has_no_bias_resistor`]: that predicate
/// answers the yes/no the hazard filter needs (1 hop only, kept deliberately
/// tight there), while this returns the *details* the informational panel needs,
/// and reaches one series resistor further so a gate biased through a divider is
/// still recognised as defined rather than mis-reported as undefined. It never
/// invents a bias: it fires only when a real DC resistive path to a rail exists,
/// so it can never suppress a genuinely floating gate.
///
/// "Resistor" is the same strict predicate the hazard filter uses
/// (`super::straps::is_assembled_resistor`), plain assembled R refs only, so a
/// varistor / thermistor / DNP part is never miscredited as a bias.
pub fn gate_bias(board: &ExtractedBoard, net_name: &str) -> Option<GateBias> {
    let net = board.nets.iter().find(|n| n.name == net_name)?;
    let bias_of = |comp: &Component, rail_id: i64| -> Option<GateBias> {
        let (rail, level) = rail_name_and_level(board, rail_id)?;
        Some(GateBias {
            reference: comp.reference.clone(),
            value: comp.value.clone(),
            rail,
            level,
        })
    };

    // 1 hop: a resistor on the gate net whose other terminal is a rail/ground.
    for (comp, _) in board.net_members(net.id) {
        if !super::straps::is_assembled_resistor(comp) {
            continue;
        }
        for p in &comp.pins {
            if let Some(other) = p.net {
                if other != net.id {
                    if let Some(b) = bias_of(comp, other) {
                        return Some(b);
                    }
                }
            }
        }
    }

    // 2 hops: gate -> R1 -> mid -> R2 -> rail. Name R1 (the resistor sitting on
    // the gate net) and the rail R2 reaches. `mid` must not itself be a rail,
    // that is the 1-hop case, already handled, so this only extends the reach,
    // never double-counts.
    for (comp, _) in board.net_members(net.id) {
        if !super::straps::is_assembled_resistor(comp) {
            continue;
        }
        for p in &comp.pins {
            let Some(mid) = p.net else { continue };
            if mid == net.id || rail_name_and_level(board, mid).is_some() {
                continue;
            }
            for (c2, _) in board.net_members(mid) {
                if c2.reference == comp.reference || !super::straps::is_assembled_resistor(c2) {
                    continue;
                }
                for p2 in &c2.pins {
                    if let Some(other2) = p2.net {
                        if other2 != mid {
                            if let Some(b) = bias_of(comp, other2) {
                                return Some(b);
                            }
                        }
                    }
                }
            }
        }
    }
    None
}

/// A net's name and whether a bias toward it holds a gate LOW (ground family) or
/// HIGH (a supply rail). `None` when the net is neither a rail nor ground.
fn rail_name_and_level(board: &ExtractedBoard, net_id: i64) -> Option<(String, BiasLevel)> {
    if !is_power_or_ground_net(board, net_id) {
        return None;
    }
    let net = board.nets.iter().find(|n| n.id == net_id)?;
    let n = net.name.to_ascii_uppercase();
    let is_ground = n.starts_with("GND") || n.ends_with("GND") || n.starts_with("VSS");
    let level = if is_ground {
        BiasLevel::Low
    } else {
        BiasLevel::High
    };
    Some((net.name.clone(), level))
}

/// True when a net connects to a transistor or relay, a switch whose control
/// input (a MOSFET/BJT gate-base, a relay coil) at the wrong level at power-up
/// switches a load. This is the load-bearing zero-FP guard for the boot
/// advisory: it separates a genuine load-control net (e.g. an igniter gate fed
/// by a mis-mapped pull-up) from an ordinary `INPUT_PULLUP` button input, both
/// read HIGH at boot, but only the former switches anything. Reference prefix
/// 'Q' = transistor, 'K' = relay (standard KiCad designators). DNP (not
/// assembled) switches don't count. (Pin-function data that would let us require
/// the *control* terminal specifically is absent in PCB-only extraction, so any
/// terminal of a populated Q/K qualifies, a deliberate, conservative breadth.)
pub fn net_drives_a_switch(board: &ExtractedBoard, net_name: &str) -> bool {
    let Some(net) = board.nets.iter().find(|n| n.name == net_name) else {
        return false;
    };
    board.net_members(net.id).iter().any(|(c, _)| {
        !c.dnp
            && matches!(
                c.reference.chars().next().map(|ch| ch.to_ascii_uppercase()),
                Some('Q') | Some('K')
            )
    })
}

/// Whether a net id names a power rail or ground. Grounds: the GND/AGND/VSS
/// family. Rails: a leading '+', a `V…`/`…V` name (VCC/VDD/VBAT/VMOT/VSYS/VIN
/// and bare voltages like 12V/3V3/5V/1V8). The broad `V`-name rule is
/// deliberately inclusive, a missed rail would mis-read a real pull as "no
/// bias" and over-flag, so on the zero-FP surface we err toward recognising
/// rails (a false rail only *suppresses* an advisory, the safe direction here is
/// the opposite, hence breadth).
fn is_power_or_ground_net(board: &ExtractedBoard, net_id: i64) -> bool {
    let Some(net) = board.nets.iter().find(|n| n.id == net_id) else {
        return false;
    };
    let n = net.name.to_ascii_uppercase();
    // Ground family.
    if n.starts_with("GND") || n.ends_with("GND") || n.starts_with("VSS") {
        return true;
    }
    // Explicit '+' rail (e.g. "+3V3", "+5V", "+12V").
    if n.starts_with('+') {
        return true;
    }
    // V-prefixed rails (VCC/VDD/VBAT/VMOT/VSYS/VIN/VIO/VREF…) and bare voltage
    // names with a digit and a 'V' (e.g. "12V", "3V3", "5V0", "1V8", "9V").
    let v_named = n.starts_with('V') && n.len() >= 2;
    let voltage_named = n.contains('V')
        && n.chars().next().is_some_and(|c| c.is_ascii_digit())
        && n.chars().any(|c| c.is_ascii_digit());
    v_named || voltage_named
}

/// The pad number of a transistor's *control* terminal (a MOSFET gate / BJT
/// base), inferred from the footprint by package convention. Conservative:
/// returns `None` for any package whose control-pad position isn't reliable
/// (e.g. TO-92, whose lead order varies by part), so the boot-state panel simply
/// omits that device rather than mislabelling a row.
fn switch_control_pad(footprint: &str) -> Option<&'static str> {
    let f = footprint.to_ascii_uppercase();
    // 8-lead SINGLE power MOSFET (Power-SO-8 family): gate on pad 4, source on
    // 1-3, drain on 5-8. Checked before the 3-lead group ("SOT-23-8" also
    // contains "SOT-23"). SOT-23-8 is unambiguously a single power FET; a bare
    // SO-8/SOIC-8 is more often a DUAL FET or a gate-driver IC (gates on other
    // pads), so only treat those as pad-4 when the footprint says "power".
    let eight_lead_single = f.contains("SOT-23-8")
        || f.contains("SOT23-8")
        || ((f.contains("SO-8") || f.contains("SOIC-8") || f.contains("SO8"))
            && (f.contains("POWER") || f.contains("PWR")));
    if eight_lead_single {
        return Some("4");
    }
    // 3-lead discrete packages where the control terminal is pad 1 (MOSFET gate
    // G-D-S, BJT base B-C-E/B-E-C, pad 1 is the control either way).
    const THREE_LEAD: [&str; 12] = [
        "SOT-23", "SOT23", "SOT-323", "SOT323", "SC-70", "SC70", "TO-252", "DPAK", "TO-263",
        "D2PAK", "TO-220", "TO-247",
    ];
    if THREE_LEAD.iter().any(|p| is_three_lead_variant(&f, p)) {
        return Some("1");
    }
    None
}

/// Whether footprint `f` contains 3-lead package name `p` AND really is the
/// 3-lead variant. "SOT-23-5" / "SOT-23-6" / "SC-70-5" still contain the bare
/// package name, but a trailing `-N` lead count other than 3 is a different
/// pinout where pad 1 is not the control terminal, so the match must not fire
/// (this mirrors the explicit SOT-23-8 handling in the caller). A bare name
/// with no lead-count suffix ("SOT-23") is the 3-lead package.
fn is_three_lead_variant(f: &str, p: &str) -> bool {
    let Some(idx) = f.find(p) else {
        return false;
    };
    let after = &f[idx + p.len()..];
    if let Some(rest) = after.strip_prefix('-') {
        let count: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if !count.is_empty() {
            return count == "3";
        }
    }
    true
}

/// A pad named as a MOSFET *gate* (`G`/`GATE`).
fn is_gate_pad_name(s: &str) -> bool {
    matches!(s.trim().to_ascii_uppercase().as_str(), "G" | "GATE")
}

/// A pad named as a BJT *base* (`B`/`BASE`). Kept separate from the gate name so
/// a 4-terminal MOSFET with an explicit bulk/body pad labelled `B` never has its
/// bulk picked over the real gate, gate names are tried first.
fn is_base_pad_name(s: &str) -> bool {
    matches!(s.trim().to_ascii_uppercase().as_str(), "B" | "BASE")
}

/// Every transistor (`Q…`) whose control terminal can be identified, paired with
/// the net on that terminal; the rows of the boot-state panel. The control pad
/// is found first by an explicit `G`/`GATE` pad name (then `B`/`BASE`), else by
/// footprint convention. DNP transistors and unidentifiable parts are skipped
/// (the panel omits a device rather than mislabel it).
fn transistor_gate_nets(board: &ExtractedBoard) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for c in &board.components {
        if c.dnp || c.reference.chars().next().map(|ch| ch.to_ascii_uppercase()) != Some('Q') {
            continue;
        }
        let named = |is: fn(&str) -> bool| {
            c.pins
                .iter()
                .find(move |p| is(&p.number) || is(&p.function))
        };
        // 1. An explicit GATE pad, 2. an explicit BASE pad (gate wins over a
        // bulk pad also labelled `B`), 3. else the footprint's control pad.
        let pin = named(is_gate_pad_name)
            .or_else(|| named(is_base_pad_name))
            .or_else(|| {
                switch_control_pad(&c.footprint)
                    .and_then(|pad| c.pins.iter().find(|p| p.number == pad))
            });
        let Some(net_id) = pin.and_then(|p| p.net) else {
            continue;
        };
        if let Some(net) = board.nets.iter().find(|n| n.id == net_id) {
            if !net.name.is_empty() {
                out.push((c.reference.clone(), net.name.clone()));
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// What the firmware does to a gate net at power-up. Reported factually (no
/// channel-type safety claim, a HIGH gate is "on" for a low-side N-MOSFET but
/// "off" for a high-side P-MOSFET, which the netlist can't disambiguate).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootGateState {
    /// Strong push-pull HIGH (the pin is configured as an output and held high).
    DrivenHigh,
    /// HIGH via a weak internal pull-up (the firmware left the pin an input but
    /// enabled its pull-up), e.g. a serial RX pin mis-mapped onto a gate. The
    /// gate still goes high, but by accident rather than an intended drive.
    PulledHigh,
    DrivenLow,
    /// The firmware never drives the gate AND no bias resistor fixes its level:
    /// genuinely undefined at reset. This is the only state that carries the
    /// "undefined until firmware drives it" warning.
    Floating,
    /// The firmware never drives the gate, but a bias resistor ties its net to a
    /// rail, so the level IS defined by hardware; the reverse-polarity P-FET
    /// with a gate pulldown case. Reported as an informational, non-warning row
    /// naming the resistor, so a correctly-biased gate is never mis-flagged as
    /// floating (the zero-false-positive fix for the boot-gate panel).
    HeldByBias(GateBias),
}

/// The bias resistor that fixes a not-firmware-driven gate's power-up level, and
/// the rail it ties to. Populated by [`gate_bias`] so the panel can name the
/// resistor, its value, and the rail instead of crying "floating".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateBias {
    /// The resistor directly on the gate net (e.g. "R1").
    pub reference: String,
    /// Its value string (e.g. "100k"); may be empty on a value-less extraction.
    pub value: String,
    /// The rail/ground the bias ties toward (e.g. "GND", "+3V3").
    pub rail: String,
    /// Which way the gate is held.
    pub level: BiasLevel,
}

/// Which level a bias resistor holds a gate at, a pull-down to ground holds it
/// LOW, a pull-up to a supply holds it HIGH.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BiasLevel {
    Low,
    High,
}

impl BiasLevel {
    fn word(self) -> &'static str {
        match self {
            BiasLevel::Low => "low",
            BiasLevel::High => "high",
        }
    }
}

impl BootGateState {
    pub fn label(&self) -> String {
        match self {
            BootGateState::DrivenHigh => "driven HIGH and held".to_string(),
            BootGateState::PulledHigh => "pulled HIGH (weak internal pull-up)".to_string(),
            BootGateState::DrivenLow => "driven LOW and held".to_string(),
            BootGateState::Floating => "never driven (floating)".to_string(),
            BootGateState::HeldByBias(b) => {
                let paren = if b.value.is_empty() {
                    format!("to {}", b.rail)
                } else {
                    format!("{} to {}", b.value, b.rail)
                };
                format!(
                    "not driven by firmware; held {} by {} ({paren})",
                    b.level.word(),
                    b.reference,
                )
            }
        }
    }
    /// A short marker for the states worth a look (active or undefined at reset);
    /// LOW and a hardware-biased gate are reported without a marker (both are the
    /// common held-at-a-defined-level case, nothing for a reader to chase).
    pub fn marker(&self) -> &'static str {
        match self {
            BootGateState::DrivenHigh | BootGateState::PulledHigh => "  <- switched at power-up",
            BootGateState::Floating => "  <- undefined until firmware drives it",
            BootGateState::DrivenLow | BootGateState::HeldByBias(_) => "",
        }
    }
    pub fn json(&self) -> &'static str {
        match self {
            BootGateState::DrivenHigh => "driven_high",
            BootGateState::PulledHigh => "pulled_high",
            BootGateState::DrivenLow => "driven_low",
            BootGateState::Floating => "floating",
            BootGateState::HeldByBias(b) => match b.level {
                BiasLevel::Low => "held_low_by_bias",
                BiasLevel::High => "held_high_by_bias",
            },
        }
    }
}

/// Classify each transistor gate's power-up state from the co-sim drive sets.
/// `held_high` is the UNFILTERED set of nets held high (a factual level, so it
/// must not be the safety-filtered advisory list); `configured` is the set the
/// firmware drove as outputs (used to split a strong HIGH from a pull-up);
/// `driven` is the union (output-configured ∪ written), a net in neither is
/// floating. A `pinMode(OUTPUT)`-with-no-write pin appears in `driven` and
/// reports "driven LOW"; note the analog solve leaves it tri-stated (it only
/// enables a Thevenin leg on a PORT edge), so panel and solver intentionally
/// disagree there; the panel is the more faithful account of the real pin.
fn boot_gate_states(
    gates: &[(String, String)],
    held_high: &HashSet<String>,
    configured: &HashSet<String>,
    driven: &HashSet<String>,
) -> Vec<(String, String, BootGateState)> {
    gates
        .iter()
        .map(|(reference, net)| {
            let state = if held_high.contains(net) {
                if configured.contains(net) {
                    BootGateState::DrivenHigh
                } else {
                    BootGateState::PulledHigh
                }
            } else if driven.contains(net) {
                BootGateState::DrivenLow
            } else {
                BootGateState::Floating
            };
            (reference.clone(), net.clone(), state)
        })
        .collect()
}

/// Render the informational boot-state panel for the `--plain` surface: aligned
/// plain-language lines, one per transistor gate, reporting (not judging) what
/// the firmware does to it at power-up. The arrows flag the active / undefined
/// cases for a non-engineer to verify; LOW is reported without a flag.
pub fn render_boot_gate_panel(rows: &[(String, String, BootGateState)]) -> String {
    let ref_w = rows
        .iter()
        .map(|(r, _, _)| r.len())
        .max()
        .unwrap_or(3)
        .max(2);
    let net_w = rows
        .iter()
        .map(|(_, n, _)| n.len())
        .max()
        .unwrap_or(8)
        .max(8);
    let mut s = String::from(
        "\nPower-up state of MOSFET / transistor gates: what the firmware does to each\n\
         switch the moment the board powers up. Verify each is the level you intend\n\
         (a HIGH or floating gate can switch a load on before the firmware means to):\n",
    );
    for (reference, net, state) in rows {
        s.push_str(&format!(
            "  {reference:<ref_w$}  {net:<net_w$}  {}{}\n",
            state.label(),
            state.marker(),
        ));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::{
        analyze, boot_gate_states, gate_bias, is_base_pad_name, is_gate_pad_name,
        is_power_or_ground_net, net_drives_a_switch, net_has_no_bias_resistor, switch_control_pad,
        transistor_gate_nets, BiasLevel, BootGateState,
    };
    use hauksbee_extract::{Component, ExtractedBoard, Net, Pin};

    fn pin(net: Option<i64>) -> Pin {
        Pin {
            number: "1".into(),
            net,
            function: String::new(),
            kind: String::new(),
            position: None,
        }
    }
    fn resistor(reference: &str, a: i64, b: i64) -> Component {
        Component {
            reference: reference.into(),
            value: "10k".into(),
            lib_id: String::new(),
            footprint: String::new(),
            position: None,
            layer: String::new(),
            properties: vec![],
            dnp: false,
            pins: vec![pin(Some(a)), pin(Some(b))],
        }
    }
    fn board(nets: &[(i64, &str)], comps: Vec<Component>) -> ExtractedBoard {
        ExtractedBoard {
            name: "t".into(),
            nets: nets
                .iter()
                .map(|(id, n)| Net {
                    id: *id,
                    name: (*n).into(),
                })
                .collect(),
            components: comps,
        }
    }

    #[test]
    fn rails_and_grounds_recognised_signals_not() {
        let b = board(
            &[
                (1, "GND"),
                (2, "+3V3"),
                (3, "VCC"),
                (4, "GNDA"),
                (5, "5V"),
                (6, "VMOT"),
                (7, "VSYS"),
                (8, "VIN"),
                (9, "12V"),
                (10, "1V8"),
                (11, "SIG"),
                (12, "DATA0"),
            ],
            vec![],
        );
        for id in [1, 2, 3, 4, 5, 6, 7, 8, 9, 10] {
            assert!(
                is_power_or_ground_net(&b, id),
                "net {id} should be rail/ground"
            );
        }
        assert!(
            !is_power_or_ground_net(&b, 11),
            "SIG must not read as rail/ground"
        );
        assert!(
            !is_power_or_ground_net(&b, 12),
            "DATA0 must not read as rail/ground"
        );
    }

    #[test]
    fn gate_net_with_no_resistor_has_no_bias() {
        let b = board(&[(1, "GATE")], vec![]);
        assert!(net_has_no_bias_resistor(&b, "GATE"));
    }

    #[test]
    fn pulldown_to_ground_counts_as_bias() {
        let b = board(&[(1, "GATE"), (2, "GND")], vec![resistor("R1", 1, 2)]);
        assert!(!net_has_no_bias_resistor(&b, "GATE"));
    }

    #[test]
    fn varistor_or_thermistor_to_ground_is_not_a_bias() {
        // An RV varistor is high-impedance below its clamp and sets NO DC
        // level; an RT thermistor likewise is not a bias resistor. Crediting
        // either would silently suppress a real held-high boot hazard (#8).
        let b = board(&[(1, "GATE"), (2, "GND")], vec![resistor("RV1", 1, 2)]);
        assert!(
            net_has_no_bias_resistor(&b, "GATE"),
            "RV to GND must not count as a bias"
        );
        let b = board(&[(1, "GATE"), (2, "GND")], vec![resistor("RT1", 1, 2)]);
        assert!(
            net_has_no_bias_resistor(&b, "GATE"),
            "RT to GND must not count as a bias"
        );
        // A plain R with the same wiring IS a bias (the strict predicate keeps it).
        let b = board(&[(1, "GATE"), (2, "GND")], vec![resistor("R1", 1, 2)]);
        assert!(!net_has_no_bias_resistor(&b, "GATE"));
    }

    #[test]
    fn dnp_resistor_to_ground_is_not_a_bias() {
        let mut r = resistor("R1", 1, 2);
        r.dnp = true;
        let b = board(&[(1, "GATE"), (2, "GND")], vec![r]);
        assert!(
            net_has_no_bias_resistor(&b, "GATE"),
            "a DNP resistor is electrically absent"
        );
    }

    #[test]
    fn series_resistor_to_a_signal_is_not_a_bias() {
        let b = board(&[(1, "GPIO"), (2, "GATE")], vec![resistor("R1", 1, 2)]);
        assert!(net_has_no_bias_resistor(&b, "GPIO"));
    }

    #[test]
    fn unknown_net_name_is_treated_as_biased() {
        let b = board(&[(1, "GATE")], vec![]);
        assert!(!net_has_no_bias_resistor(&b, "does-not-exist"));
    }

    fn part(reference: &str, net: i64) -> Component {
        part_dnp(reference, net, false)
    }
    fn part_dnp(reference: &str, net: i64, dnp: bool) -> Component {
        Component {
            reference: reference.into(),
            value: String::new(),
            lib_id: String::new(),
            footprint: String::new(),
            position: None,
            layer: String::new(),
            properties: vec![],
            dnp,
            pins: vec![pin(Some(net))],
        }
    }

    #[test]
    fn net_to_transistor_or_relay_drives_a_switch() {
        let b = board(
            &[(1, "GATE"), (2, "COIL"), (3, "HDR"), (4, "DNPGATE")],
            vec![
                part("Q1", 1),
                part("K1", 2),
                part("J3", 3),
                part("U7", 3),
                part_dnp("Q9", 4, true),
            ],
        );
        assert!(net_drives_a_switch(&b, "GATE"));
        assert!(net_drives_a_switch(&b, "COIL"));
        assert!(!net_drives_a_switch(&b, "HDR"));
        assert!(
            !net_drives_a_switch(&b, "DNPGATE"),
            "a DNP transistor must not count"
        );
        assert!(!net_drives_a_switch(&b, "missing"));
    }

    #[test]
    fn switch_control_pad_by_footprint() {
        assert_eq!(switch_control_pad("Package_TO_SOT_SMD:SOT-23"), Some("1"));
        assert_eq!(switch_control_pad("SOT-23-3"), Some("1"));
        assert_eq!(
            switch_control_pad("Package_TO_SOT_SMD:TO-252-3_DPAK"),
            Some("1")
        );
        // 5/6-pin SOT-23 variants contain "SOT-23" but pad 1 is NOT the
        // control terminal there (#9): they must not match the 3-lead rule.
        assert_eq!(switch_control_pad("Package_TO_SOT_SMD:SOT-23-5"), None);
        assert_eq!(switch_control_pad("Package_TO_SOT_SMD:SOT-23-6"), None);
        assert_eq!(switch_control_pad("SOT23-5"), None);
        assert_eq!(switch_control_pad("SOT23-6_HandSoldering"), None);
        assert_eq!(switch_control_pad("SOT-23-8_Handsoldering"), Some("4"));
        assert_eq!(switch_control_pad("Package_SO:SO-8_Power"), Some("4"));
        assert_eq!(switch_control_pad("Package_SO:SO-8"), None);
        assert_eq!(switch_control_pad("Package_SO:SOIC-8"), None);
        assert_eq!(switch_control_pad("Package_TO_SOT_THT:TO-92"), None);
        assert_eq!(switch_control_pad("Resistor_SMD:R_0402"), None);
    }

    #[test]
    fn control_pad_names_recognised() {
        // The control-terminal predicate is gate-name OR base-name.
        let is_control = |s: &str| is_gate_pad_name(s) || is_base_pad_name(s);
        for s in ["G", "GATE", "g", "Base", "B"] {
            assert!(is_control(s), "{s} should be a control pad name");
        }
        for s in ["D", "S", "1", "drain", ""] {
            assert!(!is_control(s), "{s} must not be a control pad name");
        }
    }

    fn transistor(
        reference: &str,
        footprint: &str,
        pads: &[(&str, &str, i64)],
        dnp: bool,
    ) -> Component {
        Component {
            reference: reference.into(),
            value: String::new(),
            lib_id: String::new(),
            footprint: footprint.into(),
            position: None,
            layer: String::new(),
            properties: vec![],
            dnp,
            pins: pads
                .iter()
                .map(|(num, func, net)| Pin {
                    number: (*num).into(),
                    net: Some(*net),
                    function: (*func).into(),
                    kind: String::new(),
                    position: None,
                })
                .collect(),
        }
    }

    #[test]
    fn transistor_gate_nets_prefers_named_pad_then_footprint() {
        let b = board(
            &[
                (1, "GATE_A"),
                (2, "DRN"),
                (3, "SRC"),
                (4, "GATE_B"),
                (5, "X"),
                (6, "BULK"),
            ],
            vec![
                transistor(
                    "Q1",
                    "whatever",
                    &[("G", "", 1), ("D", "", 2), ("S", "", 3)],
                    false,
                ),
                transistor(
                    "Q2",
                    "SOT-23",
                    &[("1", "", 4), ("2", "", 5), ("3", "", 2)],
                    false,
                ),
                transistor("Q3", "SOT-23", &[("1", "", 1)], true),
                transistor("Q5", "TO-92", &[("1", "", 5)], false),
                transistor(
                    "Q4",
                    "SOT-23",
                    &[("G", "", 1), ("S", "", 3), ("D", "", 2), ("B", "", 6)],
                    false,
                ),
            ],
        );
        let gates = transistor_gate_nets(&b);
        assert_eq!(
            gates,
            vec![
                ("Q1".to_string(), "GATE_A".to_string()),
                ("Q2".to_string(), "GATE_B".to_string()),
                ("Q4".to_string(), "GATE_A".to_string()),
            ]
        );
    }

    #[test]
    fn gate_bias_direct_pulldown_and_pullup() {
        // Pull-down to GND: held low, names R1 + value + rail (the reported case).
        let mut r = resistor("R1", 1, 2);
        r.value = "100k".into();
        let b = board(&[(1, "Q1_G"), (2, "GND")], vec![r]);
        let bias = gate_bias(&b, "Q1_G").expect("a pull-down to ground is a bias");
        assert_eq!(bias.reference, "R1");
        assert_eq!(bias.value, "100k");
        assert_eq!(bias.rail, "GND");
        assert_eq!(bias.level, BiasLevel::Low);

        // Pull-up to a supply rail: held high.
        let b = board(&[(1, "Q1_G"), (2, "+3V3")], vec![resistor("R2", 1, 2)]);
        let bias = gate_bias(&b, "Q1_G").expect("a pull-up to a rail is a bias");
        assert_eq!(bias.reference, "R2");
        assert_eq!(bias.rail, "+3V3");
        assert_eq!(bias.level, BiasLevel::High);
    }

    #[test]
    fn gate_bias_two_hop_chain_reaches_rail() {
        // gate -> R1 -> MID -> R2 -> GND. R1 (the resistor on the gate net) is
        // named; the rail two hops away is still recognised, so a gate biased
        // through a chain is not mis-reported as floating.
        let b = board(
            &[(1, "Q1_G"), (2, "MID"), (3, "GND")],
            vec![resistor("R1", 1, 2), resistor("R2", 2, 3)],
        );
        let bias = gate_bias(&b, "Q1_G").expect("a 2-hop chain to a rail is a bias");
        assert_eq!(
            bias.reference, "R1",
            "the resistor on the gate net is named"
        );
        assert_eq!(bias.rail, "GND");
        assert_eq!(bias.level, BiasLevel::Low);
    }

    #[test]
    fn gate_bias_absent_when_genuinely_floating_or_series_to_signal() {
        // No resistor at all: genuinely floating, no bias.
        let b = board(&[(1, "Q1_G")], vec![]);
        assert!(gate_bias(&b, "Q1_G").is_none());
        // A series resistor to a plain signal net (GPIO -> R -> gate) sets no
        // default level: not a bias.
        let b = board(&[(1, "Q1_G"), (2, "GPIO7")], vec![resistor("R1", 1, 2)]);
        assert!(gate_bias(&b, "Q1_G").is_none());
        // A DNP resistor to ground is electrically absent: not a bias.
        let mut r = resistor("R1", 1, 2);
        r.dnp = true;
        let b = board(&[(1, "Q1_G"), (2, "GND")], vec![r]);
        assert!(gate_bias(&b, "Q1_G").is_none());
    }

    #[test]
    fn floating_gate_with_pulldown_is_downgraded_bare_gate_stays_floating() {
        // Q1: a gate the firmware never drives, held low by a 100k pull-down ->
        // reported as a factual bias note with NO warning marker. Q2: a gate with
        // no bias network at all -> still floating, still warned (unchanged).
        let mut r = resistor("R1", 1, 2);
        r.value = "100k".into();
        let b = board(
            &[(1, "Q1_G"), (2, "GND"), (3, "Q2_G")],
            vec![
                transistor("Q1", "SOT-23", &[("1", "", 1)], false),
                transistor("Q2", "SOT-23", &[("1", "", 3)], false),
                r,
            ],
        );
        let adv = analyze(&b, &[], &[], &[], true);
        let q1 = adv.gate_states.iter().find(|(r, _, _)| r == "Q1").unwrap();
        match &q1.2 {
            BootGateState::HeldByBias(bias) => {
                assert_eq!(bias.reference, "R1");
                assert_eq!(bias.level, BiasLevel::Low);
            }
            other => panic!("Q1 should be HeldByBias, got {other:?}"),
        }
        assert_eq!(
            q1.2.marker(),
            "",
            "a correctly-biased gate carries no warning marker"
        );
        assert_eq!(
            q1.2.label(),
            "not driven by firmware; held low by R1 (100k to GND)"
        );
        let q2 = adv.gate_states.iter().find(|(r, _, _)| r == "Q2").unwrap();
        assert_eq!(
            q2.2,
            BootGateState::Floating,
            "a bare gate is still floating"
        );
        assert!(q2.2.marker().contains("undefined"));
    }

    #[test]
    fn boot_gate_states_classifies_high_low_floating() {
        let gates = vec![
            ("Q1".to_string(), "DrivenHi".to_string()),
            ("Q2".to_string(), "PulledHi".to_string()),
            ("Q3".to_string(), "LoNet".to_string()),
            ("Q4".to_string(), "FloatNet".to_string()),
        ];
        let set = |xs: &[&str]| -> std::collections::HashSet<String> {
            xs.iter().map(|s| s.to_string()).collect()
        };
        let held_high = set(&["DrivenHi", "PulledHi"]);
        let configured = set(&["DrivenHi", "LoNet"]);
        let driven = set(&["DrivenHi", "PulledHi", "LoNet"]);
        let rows = boot_gate_states(&gates, &held_high, &configured, &driven);
        assert_eq!(rows[0].2, BootGateState::DrivenHigh);
        assert_eq!(rows[1].2, BootGateState::PulledHigh);
        assert_eq!(rows[2].2, BootGateState::DrivenLow);
        assert_eq!(rows[3].2, BootGateState::Floating);
    }
}
