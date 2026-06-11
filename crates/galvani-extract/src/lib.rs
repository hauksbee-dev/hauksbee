//! Circuit extraction: turn a KiCad design into the connectivity graph the
//! simulator binds models onto.
//!
//! Two sources, one output shape:
//! - [`ExtractedBoard::from_kicad_pcb`] — layout only. Every pad in a
//!   `.kicad_pcb` carries its net, so the board file alone fully describes
//!   connectivity. This is the "hand us any PCB" path.
//! - [`ExtractedBoard::from_kicad_netlist`] — a `kicad-cli sch export
//!   netlist --format kicadsexpr` export. Richer (pin names/types), used
//!   when the schematic is available.

mod eagle;
mod ipc356;
mod netlist;
mod pcb;

use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum ExtractError {
    #[error("parse: {0}")]
    Parse(#[from] forge_sexpr::ParseError),
    #[error("xml: {0}")]
    Xml(String),
    #[error("not a {expected} file (root is {found:?})")]
    WrongRoot { expected: &'static str, found: Option<String> },
}

/// One electrical net. `id` is the KiCad net number (0 = the unconnected
/// net in PCB files); `name` like "GND", "/Debugger/nRF52_VDD".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Net {
    pub id: i64,
    pub name: String,
}

/// A component pin/pad connection point.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pin {
    /// Pad number / pin number as printed ("1", "A8", "EP").
    pub number: String,
    /// Net id this pin is on, if connected.
    pub net: Option<i64>,
    /// Pin name from the schematic ("VCC", "GPIO4"); empty for PCB-only.
    pub function: String,
    /// Electrical type from the schematic ("passive", "input", ...); empty
    /// for PCB-only extraction.
    pub kind: String,
    /// Absolute board position in mm, when extracted from a layout.
    pub position: Option<(f64, f64)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Component {
    /// Reference designator ("R1", "U101").
    pub reference: String,
    /// Value field ("10k", "BCM857BS").
    pub value: String,
    /// Symbol or footprint library id ("Device:R",
    /// "Resistor_SMD:R_0402_1005Metric").
    pub lib_id: String,
    /// Footprint name when known.
    pub footprint: String,
    /// Board position (x mm, y mm, rotation degrees) when from a layout.
    pub position: Option<(f64, f64, f64)>,
    /// Board side ("F.Cu"/"B.Cu") when from a layout.
    pub layer: String,
    /// Extra properties (part number, datasheet, ...).
    pub properties: Vec<(String, String)>,
    pub pins: Vec<Pin>,
}

/// The extraction result: everything the binder and renderer need to know
/// about what the board is, electrically.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractedBoard {
    pub name: String,
    pub nets: Vec<Net>,
    pub components: Vec<Component>,
}

impl ExtractedBoard {
    pub fn from_kicad_pcb(text: &str) -> Result<Self, ExtractError> {
        pcb::extract(text)
    }

    pub fn from_kicad_netlist(text: &str) -> Result<Self, ExtractError> {
        netlist::extract(text)
    }

    /// Eagle `.brd` (XML, Eagle 6+): Arduino, Adafruit, SparkFun designs.
    pub fn from_eagle_brd(text: &str) -> Result<Self, ExtractError> {
        eagle::extract(text)
    }

    /// IPC-D-356/356A fab netlist: the universal fallback any EDA exports.
    pub fn from_ipc_d356(text: &str) -> Result<Self, ExtractError> {
        ipc356::extract(text)
    }

    /// Sniff the format from content and extract accordingly.
    pub fn from_auto(text: &str) -> Result<Self, ExtractError> {
        let head: String = text.chars().take(512).collect();
        if head.contains("<eagle") {
            Self::from_eagle_brd(text)
        } else if head.trim_start().starts_with("(export") {
            Self::from_kicad_netlist(text)
        } else if head.contains("(kicad_pcb") {
            Self::from_kicad_pcb(text)
        } else {
            Self::from_ipc_d356(text)
        }
    }

    pub fn net(&self, id: i64) -> Option<&Net> {
        self.nets.iter().find(|n| n.id == id)
    }

    pub fn net_by_name(&self, name: &str) -> Option<&Net> {
        self.nets.iter().find(|n| n.name == name)
    }

    pub fn component(&self, reference: &str) -> Option<&Component> {
        self.components.iter().find(|c| c.reference == reference)
    }

    /// (component, pin) pairs attached to a net.
    pub fn net_members(&self, net_id: i64) -> Vec<(&Component, &Pin)> {
        let mut out = Vec::new();
        for c in &self.components {
            for p in &c.pins {
                if p.net == Some(net_id) {
                    out.push((c, p));
                }
            }
        }
        out
    }

    /// Consistency report: problems worth surfacing before simulation.
    pub fn lint(&self) -> Lint {
        let mut lint = Lint::default();
        let net_ids: std::collections::HashSet<i64> =
            self.nets.iter().map(|n| n.id).collect();
        let mut degree: std::collections::HashMap<i64, usize> =
            std::collections::HashMap::new();
        for c in &self.components {
            let mut connected = 0usize;
            for p in &c.pins {
                match p.net {
                    Some(id) => {
                        connected += 1;
                        if !net_ids.contains(&id) {
                            lint.undeclared_nets.push((
                                c.reference.clone(),
                                p.number.clone(),
                                id,
                            ));
                        }
                        *degree.entry(id).or_default() += 1;
                    }
                    None => lint
                        .unconnected_pins
                        .push((c.reference.clone(), p.number.clone())),
                }
            }
            if connected == 0 && !c.pins.is_empty() {
                lint.floating_components.push(c.reference.clone());
            }
        }
        for net in &self.nets {
            // Net 0 is KiCad's "no net" bucket; skip it.
            if net.id != 0 && degree.get(&net.id).copied().unwrap_or(0) == 1 {
                lint.single_pin_nets.push(net.name.clone());
            }
        }
        lint
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Lint {
    /// Pins whose net id has no declaration: (reference, pin, net id).
    pub undeclared_nets: Vec<(String, String, i64)>,
    /// Pins on no net at all: (reference, pin).
    pub unconnected_pins: Vec<(String, String)>,
    /// Components with pins but no connected pin.
    pub floating_components: Vec<String>,
    /// Named nets touching exactly one pin.
    pub single_pin_nets: Vec<String>,
}

impl Lint {
    pub fn is_clean(&self) -> bool {
        self.undeclared_nets.is_empty() && self.floating_components.is_empty()
    }
}
