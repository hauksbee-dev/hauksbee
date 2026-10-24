//! Junction completeness: the pn junctions every real part carries that a
//! behavioural stamp leaves out.
//!
//! Three are added here, each an ASSUMED value unless the card states it:
//!
//! - **Power-MOSFET body diodes.** Stamped by the binder itself on every
//!   discrete MOSFET (`bind_mosfet`), at the card's `is` or
//!   [`hauksbee_ir::GENERIC_BODY_DIODE_IS`]; the parts bound with the generic
//!   value are listed in [`crate::bind_report::BindReport::assumed_body_diodes`].
//! - **IC pin clamps.** A logic or MCU pin carries a protection diode to its own
//!   supply and one from its ground ([`GENERIC_CLAMP_IS`]), plus the supply
//!   clamp from ground to supply. A pin whose card states a supply-independent
//!   ceiling (`supply_range`/`input_tolerance`, a 5 V-tolerant pad) gets no
//!   diode to the supply: its card has said it is built for more.
//! - **IC quiescent draw** from the card's `iq_a` (or `icc_a`), as a resistive
//!   load sized to draw that current at the rail level the rest state solves, so
//!   an unpowered part draws nothing rather than pulling its dead rail negative.
//!
//! The binder records every candidate part as an [`IcSite`] while it binds;
//! [`stamp_ic_junctions`] stamps the clamps and draws onto a bound board once a
//! rest solve has said which supplies the circuit actually powers. A part whose
//! every supply the solve leaves dead is not stamped and is listed as
//! abstained: a clamp into a rail no model drives would invent a back-feed the
//! board does not have (the `control_inputs` lesson: a node no model sets is
//! never read as 0 V).
//!
//! The clamps and draws are stamped only where a caller asks for them (today
//! the shadow scenarios of `hauksbee-checks`' `sim_parity`), not by
//! [`crate::binder::bind_board`], so the verdict a board gets is unchanged until
//! the parity ledger says they are ready.

use hauksbee_extract::Component;
use hauksbee_ir::{Circuit, Device, DeviceId, DiodeModel, NodeId};
use hauksbee_models::pin_roles::{is_switch_node_role, power_pin, PowerKind};
use hauksbee_models::{declared_pin_ceiling_v, ComponentKind, ModelEntry};

use crate::binder::BoundBoard;

/// Saturation current (A, ideality 1) of an IC pin's protection clamp.
///
/// Used when the card states none: an ASSUMED value. Datasheets rate the clamp, not its
/// curve: the Nexperia `74HC1G125` states its inputs carry clamp diodes and
/// rates IIK at ±20 mA for VI below -0.5 V or above VCC + 0.5 V. At 1e-14 A the
/// diode passes 1 uA at 0.48 V and 1 mA at 0.66 V, so a pin within the
/// datasheet's 0.5 V of its rail conducts no more than leakage, and a pin
/// pulled well past it conducts as the datasheet's IIK row describes.
pub const GENERIC_CLAMP_IS: f64 = 1e-14;

/// A supply the rest solve holds below this is dead: the part's clamps and
/// draw are not stamped (V).
pub const LIVE_SUPPLY_V: f64 = 0.5;

/// One signal pin of an [`IcSite`].
#[derive(Debug, Clone)]
pub struct SignalPin {
    /// The pad number.
    pub pin: String,
    /// The card's role for the pad.
    pub role: String,
    /// The node the pad lands on.
    pub node: NodeId,
    /// The card's supply-independent ceiling for the pin, when it states one.
    pub ceiling_v: Option<f64>,
}

/// A logic or MCU part whose pins get clamps, as the binder saw it.
#[derive(Debug, Clone)]
pub struct IcSite {
    pub reference: String,
    pub kind: ComponentKind,
    /// Nodes of the card's supply pads (`vdd`, `vddio`, `vcc`...).
    pub supplies: Vec<NodeId>,
    /// Nodes of the card's return pads, true grounds first.
    pub returns: Vec<NodeId>,
    /// The pads clamped.
    pub signals: Vec<SignalPin>,
    /// The card's quiescent draw (A), when it states one and no behavioural
    /// supply law already draws it.
    pub quiescent_a: Option<f64>,
}

/// Roles that take a higher voltage than the logic supply by design: never
/// clamped to it.
fn is_high_side_role(role: &str) -> bool {
    matches!(
        role,
        "vin" | "vbus" | "vbat" | "vsys" | "raw" | "vmot" | "vm" | "vs" | "vref" | "nc"
    )
}

/// Roles that mark a part as a power or driver IC whose outputs swing past its
/// logic supply: such a part is not clamped at all.
fn is_driver_role(role: &str) -> bool {
    is_switch_node_role(role)
        || matches!(
            role,
            "vout" | "vmot" | "vm" | "pvdd" | "vcp" | "bst" | "boot" | "vboot"
        )
        || (role.starts_with("out") && !role.starts_with("output_enable"))
}

/// The kinds clamped: logic and processors.
const fn clamped_kind(kind: ComponentKind) -> bool {
    matches!(
        kind,
        ComponentKind::Mcu
            | ComponentKind::Digital
            | ComponentKind::ShiftRegister
            | ComponentKind::Adc
            | ComponentKind::Dac
    )
}

/// The [`IcSite`] for a bound part, or `None` when it is not a clamped kind,
/// has no supply or return pad on a net, or is a driver IC (a part with a
/// switch-node, motor-supply, boost or output role whose outputs swing past
/// its logic supply; a processor is never one).
#[must_use]
pub fn ic_site(
    comp: &Component,
    model: &ModelEntry,
    node_of: &dyn Fn(Option<i64>) -> Option<NodeId>,
) -> Option<IcSite> {
    if !clamped_kind(model.kind) {
        return None;
    }
    let mut site = IcSite {
        reference: comp.reference.clone(),
        kind: model.kind,
        supplies: Vec::new(),
        returns: Vec::new(),
        signals: Vec::new(),
        quiescent_a: None,
    };
    let mut grounds = Vec::new();
    for pin in &comp.pins {
        let Some(role) = model.pins.get(&pin.number) else {
            continue;
        };
        let role = role.trim().to_ascii_lowercase();
        if model.kind != ComponentKind::Mcu && is_driver_role(&role) {
            return None;
        }
        let Some(node) = node_of(pin.net) else {
            continue;
        };
        match power_pin(&role, model.kind) {
            Some(p) if p.kind == PowerKind::Supply => {
                if node != NodeId::GROUND && !site.supplies.contains(&node) {
                    site.supplies.push(node);
                }
            }
            Some(p) => {
                let list = if p.ground {
                    &mut grounds
                } else {
                    &mut site.returns
                };
                if !list.contains(&node) {
                    list.push(node);
                }
            }
            None if is_high_side_role(&role) || role.is_empty() => {}
            None => site.signals.push(SignalPin {
                pin: pin.number.clone(),
                ceiling_v: declared_pin_ceiling_v(&model.envelope, &role),
                role,
                node,
            }),
        }
    }
    grounds.append(&mut site.returns);
    site.returns = grounds;
    if site.supplies.is_empty() || site.returns.is_empty() {
        return None;
    }
    let behavioural_draw = model.params.get_f64("supply_current_a").is_some();
    site.quiescent_a = if behavioural_draw {
        None
    } else {
        (model.params.get_f64("iq_a"))
            .or_else(|| model.params.get_f64("icc_a"))
            .filter(|a| a.is_finite() && *a > 0.0)
    };
    Some(site)
}

/// One stamped pin clamp.
#[derive(Debug, Clone)]
pub struct PinClamp {
    pub reference: String,
    pub pin: String,
    pub role: String,
    /// The pin's node.
    pub node: NodeId,
    /// The supply the upper clamp returns to (the part's highest live supply).
    pub supply: NodeId,
    /// The return the lower clamp comes from.
    pub ret: NodeId,
    /// The pin-to-supply diode; `None` for a pin rated past its supply.
    pub upper: Option<DeviceId>,
    /// The return-to-pin diode.
    pub lower: DeviceId,
}

/// What [`stamp_ic_junctions`] added.
#[derive(Debug, Clone, Default)]
pub struct IcJunctions {
    pub clamps: Vec<PinClamp>,
    /// (reference, supply node, return node, ground-to-supply diode).
    pub supply_clamps: Vec<(String, NodeId, NodeId, DeviceId)>,
    /// (reference, supply node, the draw's resistor, amps at the rest level).
    pub quiescent: Vec<(String, NodeId, DeviceId, f64)>,
    /// Parts not stamped because the rest solve powers none of their supplies.
    pub abstained: Vec<String>,
}

fn clamp_diode(circuit: &mut Circuit, name: String, a: NodeId, k: NodeId) -> DeviceId {
    circuit.add(Device::Diode {
        name,
        a,
        k,
        model: DiodeModel {
            is: GENERIC_CLAMP_IS,
            ..DiodeModel::default()
        },
    })
}

/// Stamp the clamps and quiescent draws of every recorded [`IcSite`] whose
/// supply `level` (the rest solve) holds up. The circuit gains devices: build a
/// new solver workspace after this.
pub fn stamp_ic_junctions(bound: &mut BoundBoard, level: &dyn Fn(NodeId) -> f64) -> IcJunctions {
    let mut out = IcJunctions::default();
    let sites = bound.report.ic_sites.clone();
    for site in &sites {
        let Some((supply, v)) = (site.supplies.iter())
            .map(|&n| (n, level(n)))
            .max_by(|a, b| a.1.total_cmp(&b.1))
        else {
            continue;
        };
        let ret = site.returns[0];
        if v - level(ret) < LIVE_SUPPLY_V {
            out.abstained.push(site.reference.clone());
            continue;
        }
        let circuit = &mut bound.circuit;
        let r = &site.reference;
        let rail = clamp_diode(circuit, format!("Dsup_{r}"), ret, supply);
        out.supply_clamps.push((r.clone(), supply, ret, rail));
        for s in &site.signals {
            if s.node == supply || s.node == ret || site.returns.contains(&s.node) {
                continue;
            }
            let tolerant = s.ceiling_v.is_some_and(|c| c > v + 0.5);
            let upper = (!tolerant)
                .then(|| clamp_diode(circuit, format!("Dclh_{r}_{}", s.pin), s.node, supply));
            let lower = clamp_diode(circuit, format!("Dcll_{r}_{}", s.pin), ret, s.node);
            out.clamps.push(PinClamp {
                reference: r.clone(),
                pin: s.pin.clone(),
                role: s.role.clone(),
                node: s.node,
                supply,
                ret,
                upper,
                lower,
            });
        }
        if let Some(amps) = site.quiescent_a {
            let ohms = (v - level(ret)) / amps;
            let id = circuit.add(Device::resistor(format!("Riq_{r}"), supply, ret, ohms));
            out.quiescent.push((r.clone(), supply, id, amps));
        }
    }
    out
}

/// Solve the DC operating point of `circuit`, cold (or from `seed`), and when
/// that fails, once more through a junction homotopy.
///
/// The generic body diodes and pin clamps make a board stiffer than the
/// circuit it was before them. When the plain solve fails, the circuit is
/// solved with every MOSFET body diode and every stamped clamp switched off,
/// and the full circuit is then solved from that point: a continuation in the
/// junctions, like the solver's own gmin and source stepping. A board the
/// plain solve already takes is solved exactly as before. The refusal of the
/// first attempt is the one returned when the homotopy fails too.
///
/// # Errors
///
/// The plain solve's error when neither path reaches a root.
pub fn dc_with_junction_homotopy(
    ws: &mut hauksbee_solve::Workspace,
    circuit: &mut Circuit,
    opts: &hauksbee_solve::SolverOptions,
    seed: Option<&[f64]>,
) -> hauksbee_solve::SolveResult<()> {
    let seed = seed.filter(|s| s.len() == ws.x.len());
    let first = hauksbee_solve::dc_operating_point_seeded(ws, circuit, opts, seed);
    let Err(err) = first else {
        return Ok(());
    };
    let mut off: Vec<(usize, f64)> = Vec::new();
    for (i, dev) in circuit.devices.iter_mut().enumerate() {
        match dev {
            Device::Mosfet { model, .. } if model.body_is > 0.0 => {
                off.push((i, model.body_is));
                model.body_is = 0.0;
            }
            Device::Diode { name, model, .. }
                if model.is > 0.0 && (name.starts_with("Dcl") || name.starts_with("Dsup_")) =>
            {
                off.push((i, model.is));
                model.is = 0.0;
            }
            _ => {}
        }
    }
    if off.is_empty() {
        return Err(err);
    }
    let bare = hauksbee_solve::dc_operating_point_seeded(ws, circuit, opts, seed);
    for &(i, is) in &off {
        match circuit.devices.get_mut(i) {
            Some(Device::Mosfet { model, .. }) => model.body_is = is,
            Some(Device::Diode { model, .. }) => model.is = is,
            _ => {}
        }
    }
    if bare.is_err() {
        return Err(err);
    }
    let x = ws.x.clone();
    hauksbee_solve::dc_operating_point_seeded(ws, circuit, opts, Some(&x)).map_err(|_| err)
}

/// The current (A) a generic clamp carries forward at `v` volts across it.
#[must_use]
pub fn clamp_current(v: f64) -> f64 {
    junction_current(GENERIC_CLAMP_IS, v)
}

/// Shockley current (A) of a junction of saturation current `is` at `v`
/// volts forward, at 27 °C, ideality 1; the exponent is capped where the
/// solver's own stamp caps it.
#[must_use]
pub fn junction_current(is: f64, v: f64) -> f64 {
    let vt = hauksbee_ir::thermal_voltage_c(27.0);
    is * (v / vt).clamp(-40.0, 40.0).exp_m1()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_generic_clamp_is_leakage_inside_half_a_volt_and_conducts_past_it() {
        assert!(clamp_current(0.3) < 2e-9);
        assert!(clamp_current(0.48) < 2e-6);
        let ma = clamp_current(0.66);
        assert!((5e-4..2e-3).contains(&ma), "{ma}");
    }

    #[test]
    fn the_generic_body_diode_lands_on_the_datasheet_forward_voltage() {
        // Si7461DP: VSD 0.8 V typ at 4.5 A. Solve the generic junction for 4.5 A.
        let vt = hauksbee_ir::thermal_voltage_c(27.0);
        let v = vt * (4.5 / hauksbee_ir::GENERIC_BODY_DIODE_IS).ln();
        assert!((0.7..0.85).contains(&v), "{v}");
    }
}
