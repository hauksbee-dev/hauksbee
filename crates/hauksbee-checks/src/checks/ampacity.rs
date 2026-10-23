//! Trace-ampacity check wired into `--si`.
//! Long-form how-and-why: docs/how-and-why/hauksbee-engine/checks.md.
//!
//! The physics (IPC-2221 width -> current, the Poured-net exemption, the
//! "never invent a current" rule) all live in
//! [`hauksbee_extract::trace_current`] and are unit-tested there. What was
//! missing, and what this module supplies, is the *attribution* layer that the
//! `--si` surface needs: deciding which net carries how much current, from the
//! bound DB models, so the existing [`audit_trace_currents`] engine can be run
//! automatically instead of by hand.
//!
//! ## Attribution discipline (this is the zero-false-positive boundary)
//!
//! A current is only ever attributed to a net from an *explicit, citeable*
//! source. This module never guesses a load. Today the sole automatic source is
//! a DB current-program equation explicitly tagged `regulated_current` (for
//! example, a linear charger's constant-current phase), evaluated from the
//! resistor network actually fitted. Its datasheet equation and the populated
//! conductive programming topology travel into the finding.
//!
//! Converter current limits, connector/regulator ratings, FET ratings, and
//! `protection_limit` programming equations are capabilities or trip
//! thresholds—not evidence of board draw—so none of them seeds steady-state
//! ampacity. An absolute stress rating is never substituted for a load.
//!
//! Everything else (signal nets, parts with no current rating, poured rails) is
//! left un-attributed, and [`audit_trace_currents`] then skips it. The result is
//! a check that fires on a genuinely under-width *routed* trace carrying a cited
//! current, and stays silent on the known-good corpus, exactly as the `LumenPnP`
//! sweep (`trace_current_corpus.rs`) pins.

use std::collections::{BTreeSet, HashMap};

use hauksbee_extract::{
    audit_trace_currents, net_copper_from_text, ExtractedBoard, SiCheck, SiFinding, SiReport,
    SiSeverity, TraceAudit, TraceCurrentFinding,
};
use hauksbee_models::ModelLibrary;

use hauksbee_bind::program_resistor::{
    classify_series, electrical_terminal_nets, program_network_reading as read_program_network,
    program_resistance_to_ground as read_program_ohms, ProgramReading, Series,
};
use hauksbee_bind::{binder::resolve, component_evidence::role_net};
use hauksbee_extract::assembly::{AssemblyState, FittedComponent};
use hauksbee_extract::drc::connectivity::{path_neck_from_text, PathNeck};
use hauksbee_extract::trace_current::Bottleneck;
use hauksbee_extract::Component;
use hauksbee_ir::units::{format_amps, format_celsius, format_percent, format_volts};

/// Direction of a regulated through-current at one of a component's rail pins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RailFlow {
    /// Current leaves the net through an input/supply pin.
    IntoComponent,
    /// Current enters the net through a regulated output/battery pin.
    OutOfComponent,
}

/// Per-net directional totals. A current passing through two cascaded stages
/// appears once as a source and once as a load on the middle net; using the
/// larger side avoids counting the same through-current twice. Parallel loads
/// (or parallel sources) remain additive on their respective side.
#[derive(Default)]
struct Attribution {
    into_components_a: f64,
    out_of_components_a: f64,
    citations: BTreeSet<String>,
}

impl Attribution {
    fn add(&mut self, current_a: f64, citation: String, flow: RailFlow) {
        match flow {
            RailFlow::IntoComponent => self.into_components_a += current_a,
            RailFlow::OutOfComponent => self.out_of_components_a += current_a,
        }
        self.citations.insert(citation);
    }

    fn finish(self) -> (f64, String) {
        let current_a = self.into_components_a.max(self.out_of_components_a);
        let mut citation = self
            .citations
            .into_iter()
            .collect::<Vec<_>>()
            .join("; plus ");
        if self.into_components_a > 0.0 && self.out_of_components_a > 0.0 {
            citation = format!( "{citation}; net-flow balance uses max({} load side, {} source side), not their double-counted sum", format_amps(self.into_components_a), format_amps(self.out_of_components_a));
        }
        (current_a, citation)
    }
}

/// A part whose rail current the board programs, where the programming resistor
/// could not be read off the layout. Recorded so the hole is visible: the
/// alternative is to charge the part's ceiling to the rail, which invents a load.
struct Undetermined {
    reference: String,
    model_id: String,
    role: String,
    ceiling_a: Option<f64>,
    reason: &'static str,
}

/// What [`attribute_currents`] found: the cited currents, and the programmable
/// parts whose current it declined to guess.
struct Attributions {
    cited: HashMap<String, (f64, String)>,
    undetermined: Vec<Undetermined>,
    /// Parts whose identity is refused, as `(reference, reason)`. They carry
    /// no attributed current, and the report says so instead of letting the
    /// skip look like a clean pass.
    skipped_identity: Vec<(String, String)>,
}

/// The populated DC resistance from `start` to ground and the parts that carry
/// it ([`read_program_ohms`] over this board's nets).
pub(super) fn program_resistance_to_ground(
    board: &ExtractedBoard,
    start: i64,
    max_hops: usize,
    excluded_component: Option<usize>,
) -> Option<(f64, Vec<String>)> {
    let is_ground = |id| {
        board
            .net(id)
            .map(|net| super::converter::is_ground_net(&net.name))
    };
    read_program_ohms(
        &board.components,
        &is_ground,
        start,
        max_hops,
        excluded_component,
    )
}

/// [`program_resistance_to_ground`], keeping why there is no resistance: a pin
/// shorted to ground, or the part that could not be read.
pub(super) fn program_reading(
    board: &ExtractedBoard,
    start: i64,
    max_hops: usize,
    excluded_component: Option<usize>,
) -> ProgramReading {
    let is_ground = |id| {
        board
            .net(id)
            .map(|net| super::converter::is_ground_net(&net.name))
    };
    read_program_network(
        &board.components,
        &is_ground,
        start,
        max_hops,
        excluded_component,
    )
}

enum ExpectedSenseFar {
    Ground,
    Net(i64),
}

impl ExpectedSenseFar {
    fn matches(&self, board: &ExtractedBoard, net_id: i64) -> bool {
        match self {
            Self::Ground => board
                .net(net_id)
                .is_some_and(|net| super::converter::is_ground_net(&net.name)),
            Self::Net(expected) => net_id == *expected,
        }
    }
}

/// Read exactly one positive resistor directly adjacent to a sense pin and
/// connected to the model-declared far side. Extra conductive branches,
/// unreadable/conflicting components, or a wrong reference net make the result
/// undetermined rather than allowing a smallest-resistor guess.
fn adjacent_sense_resistor(
    board: &ExtractedBoard,
    sense_net: i64,
    expected_far: &ExpectedSenseFar,
    excluded_component: usize,
) -> Option<(f64, String)> {
    let mut candidate = None;
    for (component_index, component) in board.components.iter().enumerate() {
        // Same rule as the programming walk: DNP means absent, while a refused
        // identity flows into classify_series and refuses the read.
        if matches!(AssemblyState::of(component), AssemblyState::DnpAbsent(_))
            || component_index == excluded_component
        {
            continue;
        }
        let Some(terminals) = electrical_terminal_nets(component) else {
            if component.pins.iter().any(|pin| pin.net == Some(sense_net)) {
                return None;
            }
            continue;
        };
        let far_net = if terminals.0 == sense_net {
            terminals.1
        } else if terminals.1 == sense_net {
            terminals.0
        } else {
            continue;
        };
        match classify_series(component) {
            Series::Open => {}
            Series::Ohms(ohms)
                if ohms.is_finite() && ohms > 0.0 && expected_far.matches(board, far_net) =>
            {
                if candidate.is_some() {
                    return None;
                }
                candidate = Some((ohms, component.reference.clone()));
            }
            _ => return None,
        }
    }
    candidate
}

/// Read the sense shunts a sense-scaled current program declares, one per
/// `(sense role, far-side role)` pair. `None` when any shunt cannot be read or
/// the shunts disagree; otherwise their shared value and the shunts themselves.
fn read_sense_shunts(
    board: &ExtractedBoard,
    part: FittedComponent<'_>,
    model: &hauksbee_models::ModelEntry,
    sense_roles: &[String],
    sense_far_roles: &[String],
    excluded_component: usize,
) -> Option<(f64, Vec<(f64, String)>)> {
    let sense_shunts = sense_roles
        .iter()
        .zip(sense_far_roles)
        .map(|(sense_role, far_role)| {
            let sense_net = pin_net_for_role(part, model, sense_role)?;
            let expected_far = if far_role.eq_ignore_ascii_case("ground") {
                ExpectedSenseFar::Ground
            } else {
                ExpectedSenseFar::Net(pin_net_for_role(part, model, far_role)?)
            };
            adjacent_sense_resistor(board, sense_net, &expected_far, excluded_component)
        })
        .collect::<Option<Vec<_>>>()?;
    let sense_ohms = sense_shunts.first()?.0;
    let disagree = sense_shunts
        .iter()
        .any(|(ohms, _)| (*ohms - sense_ohms).abs() > sense_ohms.abs().max(ohms.abs()) * 1e-6);
    (!disagree).then_some((sense_ohms, sense_shunts))
}

/// Attribute cited currents to nets from the bound DB models. Returns a map of
/// `net name -> (current, citation)` in the shape [`audit_trace_currents`] wants.
// One pass per part; the regulated-current read and its citation share much local state.
#[allow(clippy::too_many_lines)]
fn attribute_currents(board: &ExtractedBoard, lib: &ModelLibrary) -> Attributions {
    let mut out: HashMap<String, Attribution> = HashMap::new();
    let mut undetermined: Vec<Undetermined> = Vec::new();
    let mut skipped_identity: Vec<(String, String)> = Vec::new();

    for (component_index, comp) in board.components.iter().enumerate() {
        // The three-state contract, asked once: a DNP part is absent (its
        // absence is on the run report already), a refused identity is
        // recorded so the attribution names the hole, and only a present part
        // reaches its model.
        let part = match AssemblyState::of(comp) {
            AssemblyState::Present(part) => part,
            AssemblyState::DnpAbsent(_) => continue,
            AssemblyState::IdentityUnknown(refusal) => {
                skipped_identity.push((comp.reference.clone(), refusal.reason()));
                continue;
            }
        };
        let res = resolve(lib, part);
        let Some(model) = res.model.as_ref() else {
            continue;
        };

        // A regulated operating current selected by the board's programming resistor. The
        // equation and its normal-operating limit live together; an absolute
        // rating is never substituted for either one. The block itself is enough
        // evidence that this part can carry a programmed rail current only when
        // the model explicitly distinguishes regulation from protection.
        let Some(program) = model
            .current_program
            .as_ref()
            .filter(|_| !model.id.starts_with("generic"))
        else {
            continue;
        };
        if program.semantics == hauksbee_models::schema::CurrentProgramSemantics::ProtectionLimit {
            // An OCP/current-limit setting is a protection threshold,
            // not evidence that the protected rail continuously draws
            // that current. `current_program` currently records and
            // validates that meaning; model-specific behavioural blocks
            // implement any dynamic protection simulation separately.
            continue;
        }
        let role = program.pin.clone();
        let programmed = pin_net_for_role(part, model, &program.pin).and_then(|net| {
            let (ohms, path) = program_resistance_to_ground(board, net, 4, Some(component_index))?;
            let sense = match &program.equation {
                hauksbee_models::schema::CurrentProgramEquation::SenseScaledResistance {
                    sense_roles,
                    sense_far_roles,
                    ..
                } => Some(read_sense_shunts(
                    board,
                    part,
                    model,
                    sense_roles,
                    sense_far_roles,
                    component_index,
                )?),
                _ => None,
            };
            let (equation_current_a, operating_current_a) = match &sense {
                Some((sense_ohms, _)) => (
                    program.equation_current_with_sense_a(ohms, *sense_ohms)?,
                    program.operating_current_with_sense_a(ohms, *sense_ohms)?,
                ),
                None => (
                    program.equation_current_a(ohms)?,
                    program.operating_current_a(ohms)?,
                ),
            };
            Some((
                equation_current_a,
                operating_current_a,
                ohms,
                path,
                programmed_power_rails(board, part, model, program)?,
                sense,
            ))
        });
        match programmed {
            Some((equation_current_a, current_a, ohms, path, nets, sense)) => {
                let limit_note = if equation_current_a > current_a {
                    format!(
                        "; equation requests {}, bounded by the \
                         {} normal-operating limit",
                        format_amps(equation_current_a),
                        format_amps(current_a)
                    )
                } else {
                    String::new()
                };
                let sense_note = sense
                    .map(|(sense_ohms, shunts)| {
                        let references = shunts
                            .into_iter()
                            .map(|(_, reference)| reference)
                            .collect::<Vec<_>>()
                            .join("+");
                        format!(
                            "; equal sense shunt(s) {references} ({sense_ohms:.6} ohm) connect the model's declared sense paths"
                        )
                    })
                    .unwrap_or_default();
                let citation = format!(
                    "{} ({}) regulated current {} programmed by {} on {} \
                     ({:.0} ohm){}{} [datasheet equation]",
                    comp.reference,
                    model.id,
                    format_amps(current_a),
                    path.join("+"),
                    role,
                    ohms,
                    sense_note,
                    limit_note
                );
                if current_a <= 0.0 {
                    continue;
                }
                for (net_id, flow) in nets {
                    if let Some(net) = board.net(net_id) {
                        out.entry(net.name.clone()).or_default().add(
                            current_a,
                            citation.clone(),
                            flow,
                        );
                    }
                }
            }
            None => undetermined.push(Undetermined {
                reference: comp.reference.clone(),
                model_id: model.id.clone(),
                role,
                ceiling_a: program.max_operating_current_a,
                reason: "the programming or required sense-resistor path could not be read",
            }),
        }
    }

    Attributions {
        cited: out.into_iter().map(|(k, a)| (k, a.finish())).collect(),
        undetermined,
        skipped_identity,
    }
}

/// Decision-grade operating-current attributions shared with other static
/// checks (notably input-cap ripple). The map intentionally contains only
/// currents the board/model establishes as operating states; ratings and
/// protection limits never enter it.
pub(super) fn attributed_operating_currents(
    board: &ExtractedBoard,
    lib: &ModelLibrary,
) -> HashMap<String, (f64, String)> {
    attribute_currents(board, lib).cited
}

/// Resolve a pin *role* (from the model's behavioural converter) to the board
/// net id on the footprint instance.
fn pin_net_for_role(
    part: FittedComponent<'_>,
    model: &hauksbee_models::ModelEntry,
    role: &str,
) -> Option<i64> {
    role_net(part, model, role).ok()
}

/// The exact rail path declared by a regulated-current model. Explicit input and
/// output role lists avoid guessing direction from names and keep control,
/// enable, feedback, sense, and ground pins out of the attribution.
fn programmed_power_rails(
    board: &ExtractedBoard,
    part: FittedComponent<'_>,
    model: &hauksbee_models::ModelEntry,
    program: &hauksbee_models::schema::CurrentProgram,
) -> Option<Vec<(i64, RailFlow)>> {
    let collapse_roles = |roles: &[String]| -> Option<i64> {
        if roles.is_empty() {
            return None;
        }
        let mut collapsed = None;
        for role in roles {
            let id = role_net(part, model, role).ok()?;
            if id == 0 {
                return None;
            }
            let net = board.net(id)?;
            if super::converter::is_ground_net(&net.name) {
                return None;
            }
            match collapsed {
                None => collapsed = Some(id),
                Some(known) if known == id => {}
                Some(_) => return None,
            }
        }
        collapsed
    };

    let input = collapse_roles(&program.current_in_roles)?;
    let output = collapse_roles(&program.current_out_roles)?;
    if input == output {
        return None;
    }
    Some(vec![
        (input, RailFlow::IntoComponent),
        (output, RailFlow::OutOfComponent),
    ])
}

/// Compatibility/view helper for topology tests and callers that only need the
/// rail identities, not their source/load direction.
#[cfg(test)]
fn power_nets_of(
    board: &ExtractedBoard,
    comp: &hauksbee_extract::Component,
    model: &hauksbee_models::ModelEntry,
    program: &hauksbee_models::schema::CurrentProgram,
) -> Vec<i64> {
    let part = AssemblyState::of(comp)
        .fitted()
        .expect("test helper: only present parts have rails");
    programmed_power_rails(board, part, model, program)
        .unwrap_or_default()
        .into_iter()
        .map(|(id, _)| id)
        .collect()
}

/// The severity and message of one trace-ampacity finding: a verdict when the board
/// declared the copper weight behind the shortfall, a conditional note otherwise.
fn ampacity_message(f: &TraceCurrentFinding, dt_c: f64) -> (SiSeverity, String) {
    let Some(d) = f.declared_shortfall.as_ref() else {
        return (
            SiSeverity::Info,
            format!(
                "net '{}' carries a cited {}, and its lowest-rated routed trace is \
                 {:.2} mm, which IPC-2221 rates at only {} ({}, {} rise); that \
                 would need >= {:.2} mm. This is NOT a verdict: the copper weight was \
                 assumed, and the real board may be heavier copper that carries it. \
                 Declare the stackup (or upload a fab drawing) to turn this into a \
                 decision. Attributed from: {}",
                f.net,
                format_amps(f.cited_current_a),
                f.bottleneck_width_mm,
                format_amps(f.ampacity_a),
                f.describe_copper(),
                format_celsius(dt_c),
                f.required_width_mm,
                f.citation
            ),
        );
    };
    // Name the declared segment the conclusion rests on whenever it is not the
    // same one the reported bottleneck came from, so the evidence and the
    // width-to-fix are both visible.
    let evidence = if d.layer == f.layer {
        String::new()
    } else {
        format!(
            " Its lowest DECLARED-copper trace, {} at {:.2} mm, independently rates \
             only {}, which is what makes this a verdict rather than an \
             artefact of an assumed copper weight.",
            d.layer.as_deref().unwrap_or("(unnamed layer)"),
            d.width_mm,
            format_amps(d.ampacity_a)
        )
    };
    (
        SiSeverity::High,
        format!(
            "net '{}' carries a cited {}, but its lowest-rated routed trace is \
             {:.2} mm and IPC-2221 rates that at only {} ({}, {} rise); \
             needs >= {:.2} mm.{} Attributed from: {}",
            f.net,
            format_amps(f.cited_current_a),
            f.bottleneck_width_mm,
            format_amps(f.ampacity_a),
            f.describe_copper(),
            format_celsius(dt_c),
            f.required_width_mm,
            evidence,
            f.citation
        ),
    )
}

/// A converter pin role that names the switch node itself.
fn is_switch_role(role: &str) -> bool {
    hauksbee_models::pin_roles::is_switch_node_role(role)
}

/// Fitted inductors with a pin on the switch node `sw`.
fn switch_inductors(board: &ExtractedBoard, sw: i64) -> impl Iterator<Item = &Component> {
    board
        .components
        .iter()
        .filter(|c| AssemblyState::of(c).is_present() && hauksbee_extract::is_plain_inductor(c))
        .filter(move |c| c.pins.iter().any(|p| p.net == Some(sw)))
}

/// A switch-node track's copper weight, IPC-2221 rating and the basis to
/// print. With no declared weight the track is rated as 2 oz, so an assumed
/// weight can only suppress the finding, never create it.
fn switch_rating(audit: &TraceAudit, b: &Bottleneck) -> (f64, f64, String) {
    let (oz, basis) = if b.copper_source.is_assumed() {
        (2.0, "2 oz, the copper weight was not declared".to_string())
    } else {
        (b.oz, hauksbee_extract::trace_current::describe_oz(b.oz))
    };
    let ampacity = hauksbee_extract::ipc2221_ampacity(b.width_mm, oz, audit.dt_c, b.external);
    (oz, ampacity, basis)
}

/// The smallest inductor ripple a buck is designed with, as a fraction of its
/// load current. TI SLVA477B ("Basic Calculation of a Buck Converter's Power
/// Stage", section 3) puts the usual choice at 20 % to 40 % of the output
/// current; the low end gives the largest load a chosen inductance implies.
const MIN_RIPPLE_RATIO: f64 = 0.2;

/// The largest load current the switch node's inductor implies, with how it
/// was reached. `None` when the card states no switching frequency, the output
/// voltage is board-programmed (not the card's own), or an inductor's value
/// does not read as an inductance.
///
/// A buck's inductor is sized for its load: the ripple it allows is
/// `vout * (1 - vout / vin) / (L * f_sw)`, below `vout / (L * f_sw)` at any input,
/// and designers keep it at no less than [`MIN_RIPPLE_RATIO`] of the load. So
/// the load the design was built for is at most `vout / (0.2 * L * f_sw)`.
/// Every step leans high (the input-free ripple bound, the low ripple ratio),
/// so this ceiling can only relax the rated-current rule where the inductor
/// shows the design was never meant to run near the converter's rating.
fn inductor_implied_load(
    board: &ExtractedBoard,
    sw: i64,
    conv: &hauksbee_models::behavioral::Converter,
) -> Option<(f64, String)> {
    let fsw = conv.switching_hz.filter(|f| f.is_finite() && *f > 0.0)?;
    let fixed_output = conv
        .feedback
        .as_ref()
        .is_none_or(|fb| (fb.vref_v - conv.vout_setpoint).abs() < 1e-6);
    let vout = Some(conv.vout_setpoint).filter(|v| fixed_output && v.is_finite() && *v > 0.0)?;
    let henries: Vec<(String, f64)> = switch_inductors(board, sw)
        .map(|c| {
            hauksbee_models::value::parse_value(&c.value)
                .filter(|p| {
                    p.unit
                        .as_deref()
                        .is_none_or(|u| u.eq_ignore_ascii_case("H"))
                })
                .map(|p| (c.reference.clone(), p.si))
                .filter(|(_, l)| l.is_finite() && *l > 0.0)
        })
        .collect::<Option<_>>()?;
    let (inductor, l) = henries.into_iter().min_by(|a, b| a.1.total_cmp(&b.1))?;
    let load = vout / (MIN_RIPPLE_RATIO * l * fsw);
    Some((
        load,
        format!(
            "the {} its inductor {inductor} implies ({:.0} uH at {:.0} kHz and \
             {} out, with ripple at least {} of load)",
            format_amps(load),
            l * 1e6,
            fsw / 1e3,
            format_volts(vout),
            format_percent(MIN_RIPPLE_RATIO * 100.0, 0)
        ),
    ))
}

/// The output rail behind a switch node: the far net of a fitted inductor on
/// it, with its bottleneck width (infinite when poured). `None` when there is
/// no such inductor or the rail has no copper, so there is nothing to compare.
fn output_rail_width(
    board: &ExtractedBoard,
    sw: i64,
    by_name: &HashMap<&str, &hauksbee_extract::NetCopper>,
    audit: &TraceAudit,
) -> Option<(String, f64)> {
    switch_inductors(board, sw)
        .filter_map(|c| c.pins.iter().find_map(|p| p.net.filter(|&n| n != sw)))
        .filter_map(|id| board.net(id))
        .filter_map(|rail| {
            let nc = by_name.get(rail.name.as_str())?;
            let width = match nc.kind {
                hauksbee_extract::CopperKind::Poured => f64::INFINITY,
                hauksbee_extract::CopperKind::Traces => audit.bottleneck(nc)?.width_mm,
                hauksbee_extract::CopperKind::None => return None,
            };
            Some((rail.name.clone(), width))
        })
        .max_by(|a, b| a.1.total_cmp(&b.1))
}

/// How a rail's routed width reads in a finding: millimetres, or "a pour" when
/// the rail is a plane with no finite track width.
fn rail_width_text(width_mm: f64) -> String {
    if width_mm.is_finite() {
        format!("{width_mm:.2} mm")
    } else {
        "a pour".to_string()
    }
}

/// A buck converter's switch node routed on a trace rated for less than half
/// the converter's rated output current, and narrower than its own output
/// rail.
///
/// The switch node of a buck carries the full inductor current while the
/// switch is on, so at the converter's rated load it carries that load plus the
/// ripple peak. This is a design-rule finding, not an overstress claim: the
/// rated output current is what the converter is built to deliver, not proof of
/// what the board draws, so the trace is judged against half of it and the
/// finding is a warning. A poured switch node is out of reach and skipped. With
/// no declared copper weight the trace is rated as 2 oz, so an assumed weight
/// can only suppress the finding, never create it. A board that routes its
/// whole power path at one width (switch node and output rail alike) made a
/// board-wide sizing choice the rated current cannot prove wrong, so the
/// finding needs the switch node to be the odd one out: narrower than the
/// output rail on the far side of the inductor. Where the inductor's value and
/// the card's switching frequency bound the load the design was built for
/// ([`inductor_implied_load`]) below half the rating, that bound is the current
/// judged instead.
///
/// The width judged is the weakest track on the path between the converter's
/// switch pads and the inductor, not the narrowest segment anywhere on the
/// net: the bootstrap capacitor hangs off the same node on a thin stub that
/// carries gate-drive charge, not the inductor current.
fn switch_node_findings(
    board: &ExtractedBoard,
    lib: &ModelLibrary,
    text: &str,
    copper: &[hauksbee_extract::NetCopper],
    audit: &TraceAudit,
) -> Vec<SiFinding> {
    let by_name: HashMap<&str, &hauksbee_extract::NetCopper> =
        copper.iter().map(|nc| (nc.name.as_str(), nc)).collect();
    let mut out = Vec::new();
    for (comp, model) in super::resolved_components(board, lib) {
        let Some(conv) = model.behavioral.converter.as_ref() else {
            continue;
        };
        let rated = conv.iout_limit_a.or(model.ratings.max_current_a);
        let (Some(rated), true, true) = (
            rated.filter(|a| a.is_finite() && *a > 0.0),
            conv.topology == hauksbee_models::behavioral::Topology::Buck,
            is_switch_role(&conv.out_pin),
        ) else {
            continue;
        };
        let Some(part) = AssemblyState::of(comp).fitted() else {
            continue;
        };
        let Some(net) = pin_net_for_role(part, &model, &conv.out_pin).and_then(|id| board.net(id))
        else {
            continue;
        };
        let Some(nc) = by_name
            .get(net.name.as_str())
            .filter(|nc| nc.kind == hauksbee_extract::CopperKind::Traces)
        else {
            continue;
        };
        let Some(mut b) = audit.bottleneck(nc) else {
            continue;
        };
        // The current to size against: half the rating, unless the inductor
        // shows the design was built for less.
        let (size_a, size_basis) = match inductor_implied_load(board, net.id, conv) {
            Some((load, why)) if load < rated / 2.0 => (load, why),
            _ => (
                rated / 2.0,
                "half the converter's rated output current".to_string(),
            ),
        };
        let thin = |b: &Bottleneck| {
            let a = switch_rating(audit, b).1;
            a > 0.0 && a < size_a
        };
        if !thin(&b) {
            continue;
        }
        // The net-wide bottleneck can sit on a branch the inductor current
        // never enters (the bootstrap capacitor's stub). Judge the copper
        // between the converter's switch pads and the inductor instead, when
        // the layout lets that path be traced; otherwise keep the net-wide
        // figure, which can only be narrower.
        let ends = |c: &Component| -> Vec<(String, String)> {
            c.pins
                .iter()
                .filter(|p| p.net == Some(net.id))
                .map(|p| (c.reference.clone(), p.number.clone()))
                .collect()
        };
        let inductors: Vec<(String, String)> =
            switch_inductors(board, net.id).flat_map(&ends).collect();
        let rating = |layer: &str, w: f64| switch_rating(audit, &audit.rate(Some(layer), w)).1;
        match path_neck_from_text(text, &net.name, &ends(comp), &inductors, &rating) {
            Some(PathNeck::Untracked) => continue,
            Some(PathNeck::Track { layer, width_mm }) => {
                b = audit.rate(Some(&layer), width_mm);
                if !thin(&b) {
                    continue;
                }
            }
            None => {}
        }
        let (oz, ampacity, basis) = switch_rating(audit, &b);
        let Some(rail) = output_rail_width(board, net.id, &by_name, audit) else {
            continue;
        };
        if b.width_mm >= rail.1 {
            continue;
        }
        let rail_text = rail_width_text(rail.1);
        out.push(SiFinding {
            check: SiCheck::TraceAmpacity,
            severity: SiSeverity::Medium,
            message: format!(
                "switch node '{}' of {} ({}, a {} buck) is routed at {:.2} mm, which \
                 IPC-2221 rates at {} ({basis}, {} rise), under {size_basis}, \
                 while its output rail '{}' is routed at \
                 {rail_text}. The switch node carries the full inductor current while the \
                 switch is on, so it is the narrowest point of the power path; it needs >= \
                 {:.2} mm for {}.",
                net.name,
                comp.reference,
                model.id,
                format_amps(rated),
                b.width_mm,
                format_amps(ampacity),
                format_celsius(audit.dt_c),
                rail.0,
                hauksbee_extract::ipc2221_min_width_mm(rated, oz, audit.dt_c, b.external),
                format_amps(rated)
            ),
            refs: vec![comp.reference.clone()],
            nets: vec![net.name.clone()],
        });
    }
    out
}

/// Run the trace-ampacity check and append its findings (and an info note) to an
/// SI report.
///
/// `pcb_text` is the raw `.kicad_pcb` text (the copper geometry source);
/// when it is absent or not a `KiCad` layout, the check is inert.
pub fn append_ampacity(
    board: &ExtractedBoard,
    lib: &ModelLibrary,
    pcb_text: Option<&str>,
    report: &mut SiReport,
) {
    let Some(text) = pcb_text.filter(|t| t.contains("(kicad_pcb")) else {
        return;
    };
    let copper = net_copper_from_text(text);
    if copper.is_empty() {
        return;
    }
    report.findings.extend(switch_node_findings(
        board,
        lib,
        text,
        &copper,
        &TraceAudit::from_pcb_text(text),
    ));
    let Attributions {
        cited,
        undetermined,
        skipped_identity,
    } = attribute_currents(board, lib);
    if cited.is_empty() && undetermined.is_empty() && skipped_identity.is_empty() {
        return;
    }

    // Copper weight and layer side come from the board's own stackup when it
    // declares one; otherwise the 1 oz external default, marked ASSUMED per net.
    let audit = TraceAudit::from_pcb_text(text);
    let findings = audit_trace_currents(&copper, &cited, &audit);

    for f in &findings {
        // A verdict is only decision-grade when the copper weight behind it was
        // read off the board. Where it was assumed, the shortfall is real under
        // that assumption and false under another (a 0.25 mm trace rates 0.88 A
        // as 1 oz and 1.45 A as 2 oz), so it is reported as a conditional note
        // rather than a defect. This mirrors the controlled-impedance check,
        // which likewise only raises findings on a declared stackup and keeps a
        // defaulted estimate informational.
        // A verdict needs a segment whose copper weight the BOARD declared to fail
        // on its own. The reported bottleneck may be an assumed-weight layer that
        // rates lower; that is still worth reporting, but it is not a conviction.
        let (severity, message) = ampacity_message(f, audit.dt_c);
        report.findings.push(SiFinding {
            check: SiCheck::TraceAmpacity,
            severity,
            message,
            refs: vec![],
            nets: vec![f.net.clone()],
        });
    }

    // The coverage hole, named per part. A programmable part whose resistor could
    // not be read is a net this check did not examine, and the honest form of that
    // is to say which part and what would close it, not to fall back on the
    // ceiling and call the result a measurement.
    for u in &undetermined {
        let ceiling = u.ceiling_a.map_or_else(
            || "No normal-operating ceiling is declared; an absolute rating is not substituted."
                .to_string(),
            |amps| {
                format!( "Its {} normal-operating ceiling is a capability, not a load, so it is not charged to the rail.", format_amps(amps))
            },
        );
        report.findings.push(SiFinding {
            check: SiCheck::TraceAmpacity,
            severity: SiSeverity::Info,
            message: format!(
                "trace-ampacity: {} ({}) sets its current with an external resistor on {}, and \
                 {}, so its rails carry no attributed current here. {} To cover these rails, \
                 give the part's model a [models.current_program] equation and keep the \
                 programming resistor readable on the layout.",
                u.reference, u.model_id, u.role, u.reason, ceiling,
            ),
            refs: vec![u.reference.clone()],
            nets: vec![],
        });
    }

    // A part the attribution refused to read at all: identity unknown. Say so
    // per part, so the skip is a visible coverage hole, not a silent pass.
    for (reference, reason) in &skipped_identity {
        report.findings.push(SiFinding {
            check: SiCheck::TraceAmpacity,
            severity: SiSeverity::Info,
            message: format!(
                "trace-ampacity: {reference} was not attributed any current: {reason}. Its \
                 rails are unexamined by this check until the identity is resolved (a BOM \
                 or placement file with an authoritative reference fixes it).",
            ),
            refs: vec![reference.clone()],
            nets: vec![],
        });
    }

    // Auditable info note so the negative is on the record: how many nets carried
    // a cited current and how many were under width.
    report.findings.push(SiFinding {
        check: SiCheck::TraceAmpacity,
        severity: SiSeverity::Info,
        message: format!(
            "trace-ampacity: {} net(s) carried an attributed current; {} routed trace(s) under \
             IPC-2221 width. Poured nets are not rated: hauksbee does not rasterise a fill, so \
             the conductor's real cross-section is out of reach rather than known to be \
             adequate.",
            cited.len(),
            findings.len()
        ),
        refs: vec![],
        nets: vec![],
    });
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{part, pin};
    use super::*;
    use crate::test_support::board;

    use hauksbee_extract::{Component, Net};

    /// A one-entry library holding `toml`, written to a temp dir keyed on `tag`.
    fn lib_from(tag: &str, toml_src: &str) -> ModelLibrary {
        let dir = hauksbee_testkit::fresh_scratch_dir("ampacity", tag);
        std::fs::write(dir.join("m.toml"), toml_src).unwrap();
        let mut lib = ModelLibrary::empty();
        let errs = lib.load_user_dir(&dir);
        assert!(errs.is_empty(), "test model must load: {errs:?}");
        lib
    }

    /// A charger with a 400 mA normal operating limit, an independent 800 mA
    /// absolute BAT-pin maximum, and current set by a resistor on PROG.
    const CHARGER_TOML: &str = r#"
[[models]]
id = "test_charger"
kind = "vreg"
description = "test charger, current programmed on PROG"

[models.match]
value_re = "(?i)^TESTCHARGER$"

[models.params]
vout = 4.2
dropout_v = 0.3
iq_a = 0.001

[models.pins]
"1" = "in"
"2" = "gnd"
"3" = "prog"
"4" = "out"

[models.ratings]
max_current_a = 0.8

[models.current_program]
pin = "prog"
semantics = "regulated_current"
current_in_roles = ["in"]
current_out_roles = ["out"]
max_operating_current_a = 0.4
equation = "inverse_resistance"
k_volts = 1000.0
"#;

    /// The same charger with its programming equation retagged as a
    /// protection threshold.
    fn protection_toml() -> String {
        let retagged = CHARGER_TOML.replace(
            "semantics = \"regulated_current\"\ncurrent_in_roles = [\"in\"]\n\
             current_out_roles = [\"out\"]\nmax_operating_current_a = 0.4\n",
            "semantics = \"protection_limit\"\n",
        );
        assert_ne!(
            retagged, CHARGER_TOML,
            "the charger's program must be retagged"
        );
        retagged
    }

    const SENSE_REGULATOR_TOML: &str = r#"
[[models]]
id = "test_sense_regulator"
kind = "vreg"
description = "regulated current with two declared Kelvin shunts"
[models.match]
value_re = "^TESTSENSE$"
[models.params]
vout = 3.3
dropout_v = 0.2
iq_a = 0.001
[models.pins]
"1" = "in"
"2" = "gnd"
"3" = "prog"
"4" = "out"
"5" = "sense_a"
"6" = "sense_b"
[models.current_program]
pin = "prog"
semantics = "regulated_current"
current_in_roles = ["in"]
current_out_roles = ["out"]
max_operating_current_a = 3.0
equation = "sense_scaled_resistance"
sense_roles = ["sense_a", "sense_b"]
sense_far_roles = ["in", "ground"]
program_bias_a = 0.00005
program_full_scale_v = 1.0
sense_full_scale_v = 0.05
"#;

    /// A component on `footprint` whose pads are `(pad number, net id)`.
    fn comp(reference: &str, value: &str, footprint: &str, pads: &[(&str, i64)]) -> Component {
        Component {
            footprint: footprint.into(),
            ..part(reference, value, pads)
        }
    }

    /// A two-terminal part on an 0603 resistor footprint between nets `a` and `b`.
    fn two_pin(reference: &str, value: &str, a: i64, b: i64) -> Component {
        comp(
            reference,
            value,
            "Resistor_SMD:R_0603",
            &[("1", a), ("2", b)],
        )
    }

    /// A two-terminal current shunt on a 2512 footprint between nets `a` and `b`.
    fn shunt(reference: &str, value: &str, a: i64, b: i64) -> Component {
        comp(
            reference,
            value,
            "Resistor_SMD:R_2512",
            &[("1", a), ("2", b)],
        )
    }

    const POT: &str = "Potentiometer_THT:Potentiometer";

    /// A TESTCHARGER U-reference with pins in/gnd/prog/out on the given nets.
    fn charger(reference: &str, [vin, gnd, prog, out]: [i64; 4]) -> Component {
        let pads = [("1", vin), ("2", gnd), ("3", prog), ("4", out)];
        comp(reference, "TESTCHARGER", "", &pads)
    }

    /// Nets: 1 = +5V (in), 2 = GND, 3 = PROG, 4 = VBAT (out), 5 = the node
    /// between the programming resistor and its solder link.
    fn charger_board(prog_network: Vec<Component>) -> ExtractedBoard {
        let mut components = vec![charger("U1", [1, 2, 3, 4])];
        components.extend(prog_network);
        numbered_board(
            "charger",
            &["+5V", "GND", "PROG", "VBAT", "PROG_LINK"],
            components,
        )
    }

    /// A board named `name` whose nets are `names`, numbered from 1.
    fn numbered_board(name: &str, names: &[&str], components: Vec<Component>) -> ExtractedBoard {
        let nets: Vec<(i64, &str)> = (1..).zip(names.iter().copied()).collect();
        board(name, &nets, components)
    }

    fn prog_4k99_via_jumper(jumper: &str) -> Vec<Component> {
        let footprint = format!("OLIMEX_Jumpers-FP:SJ_{jumper}");
        vec![
            two_pin("R10", "4.99k/1%/R0603", 3, 5),
            comp("E1", jumper, &footprint, &[("1", 5), ("2", 2)]),
        ]
    }

    fn undetermined_refs(got: &Attributions) -> Vec<String> {
        got.undetermined
            .iter()
            .map(|u| u.reference.clone())
            .collect()
    }

    /// PROG -> 4.99k -> closed jumper -> GND programs 1000/4990 A on both
    /// rails (a fifth of the ceiling), never the programming pin itself.
    #[test]
    fn a_programmed_charger_is_attributed_its_programmed_current_not_its_ceiling() {
        let board = charger_board(prog_4k99_via_jumper("Closed"));
        let got = attribute_currents(&board, &lib_from("programmed", CHARGER_TOML));
        assert!(got.undetermined.is_empty(), "{:?}", undetermined_refs(&got));
        for rail in ["+5V", "VBAT"] {
            let (current, citation) = got.cited.get(rail).unwrap_or_else(|| panic!("{rail}"));
            assert!(
                (*current - 1000.0 / 4990.0).abs() < 1e-9,
                "{rail}: {current}"
            );
            assert!(
                citation.contains("R10") && citation.contains("4990"),
                "{citation}"
            );
        }
        assert!(!got.cited.contains_key("PROG"));
    }

    /// A refused identity attributes nothing and is recorded by name.
    #[test]
    fn an_identity_refused_part_is_skipped_and_named_not_attributed() {
        let mut board = charger_board(prog_4k99_via_jumper("Closed"));
        board.components[0].properties.push((
            hauksbee_extract::DUPLICATE_REFERENCE_CONFLICT_KEY.to_string(),
            "two populated records with different values".to_string(),
        ));
        let got = attribute_currents(&board, &lib_from("identity_refused", CHARGER_TOML));
        assert!(
            got.cited.is_empty(),
            "{:?}",
            got.cited.keys().collect::<Vec<_>>()
        );
        assert_eq!(got.skipped_identity.len(), 1, "{:?}", got.skipped_identity);
        let (reference, reason) = &got.skipped_identity[0];
        assert_eq!(reference, "U1");
        assert!(reason.contains("duplicate designator"), "{reason}");
    }

    /// Under `protection_limit` semantics the same readable network attributes
    /// nothing and is not an undetermined hole either.
    #[test]
    fn a_protection_limit_program_contributes_no_ampacity_load() {
        let board = charger_board(vec![two_pin("R10", "4.99k", 3, 2)]);
        let got = attribute_currents(&board, &lib_from("protection_limit", &protection_toml()));
        assert!(got.cited.is_empty(), "{:?}", got.cited);
        assert!(got.undetermined.is_empty(), "{:?}", undetermined_refs(&got));
    }

    /// With the jumper open the charger is unprogrammed: no ceiling fallback,
    /// and the gap is recorded with its role and ceiling.
    #[test]
    fn an_open_link_in_the_programming_path_attributes_nothing_and_names_the_gap() {
        let board = charger_board(prog_4k99_via_jumper("Open"));
        let got = attribute_currents(&board, &lib_from("open_link", CHARGER_TOML));
        assert!(got.cited.is_empty(), "{:?}", got.cited);
        assert_eq!(got.undetermined.len(), 1);
        let u = &got.undetermined[0];
        assert_eq!(
            (u.reference.as_str(), u.role.as_str(), u.ceiling_a),
            ("U1", "prog", Some(0.4))
        );
    }

    /// Both populated branches conduct: (200 || 2) + 1 ohm, whatever order
    /// the parts are listed in, with every contributing part cited.
    #[test]
    fn converging_program_paths_use_the_populated_equivalent_resistance() {
        let branch = |slow_first: bool| {
            let mut first_hop = vec![two_pin("R1", "100R", 3, 6), two_pin("R2", "1R", 3, 7)];
            if !slow_first {
                first_hop.reverse();
            }
            first_hop.extend([
                two_pin("R3", "100R", 6, 8),
                two_pin("R4", "1R", 7, 8),
                two_pin("R5", "1R", 8, 2),
            ]);
            let mut board = charger_board(first_hop);
            board.nets.extend([
                Net::new(6, "SLOW_BRANCH"),
                Net::new(7, "FAST_BRANCH"),
                Net::new(8, "CONVERGED"),
            ]);
            board
        };
        for slow_first in [true, false] {
            let (ohms, path) = program_resistance_to_ground(&branch(slow_first), 3, 4, Some(0))
                .expect("the programming path is readable");
            let expected = (200.0 * 2.0) / (200.0 + 2.0) + 1.0;
            assert!(
                (ohms - expected).abs() < 1e-9,
                "got {ohms} ohm via {path:?}"
            );
            assert_eq!(path, ["R1", "R2", "R3", "R4", "R5"]);
        }

        let board = charger_board(vec![
            two_pin("R10A", "10k", 3, 2),
            two_pin("R10B", "10k", 3, 2),
        ]);
        let (ohms, path) = program_resistance_to_ground(&board, 3, 4, Some(0)).unwrap();
        assert!((ohms - 5_000.0).abs() < 1e-9, "got {ohms} via {path:?}");
        assert_eq!(path, ["R10A", "R10B"]);
    }

    /// Repeated physical pad records are one electrical terminal each.
    #[test]
    fn repeated_physical_pad_records_still_form_two_electrical_terminals() {
        let pads = [("1", 3), ("1", 3), ("2", 2), ("2", 2)];
        let board = charger_board(vec![comp("R10", "10k", "Resistor_SMD:R_0603", &pads)]);
        let got = attribute_currents(&board, &lib_from("physical_pads", CHARGER_TOML));
        let (current, citation) = got.cited.get("VBAT").expect("VBAT attributed");
        assert!((*current - 0.1).abs() < 1e-12, "{current}: {citation}");
        assert!(got.undetermined.is_empty());
    }

    /// Every programming network the check must refuse rather than read as a
    /// precise resistance: ambiguous or inferred identities, capacitors,
    /// fuses, non-passive designators on passive footprints, thermistors, a
    /// multi-terminal part touching PROG, an over-wide parallel network, and
    /// an out-of-domain (10 A) resistor.
    #[test]
    fn unreadable_programming_networks_are_undetermined_not_guessed() {
        let mut ambiguous = two_pin("R10", "4.99k", 3, 2);
        ambiguous.properties.push((
            hauksbee_extract::DUPLICATE_REFERENCE_CONFLICT_KEY.into(),
            "records named 'R10' disagree on value".into(),
        ));
        let mut inferred = two_pin("R10", "4.99k", 3, 2);
        inferred.properties.push((
            hauksbee_extract::altium::REFERENCE_AMBIGUOUS_KEY.into(),
            "same hierarchy, no authoritative source UID".into(),
        ));
        let prog_gnd = &[("1", 3), ("2", 2)];
        let mut cases: Vec<(&str, Vec<Component>)> = vec![
            ("ambiguous", vec![ambiguous]),
            ("inferred", vec![inferred]),
            (
                "cap_nf",
                vec![comp("C7", "100nF", "Capacitor_SMD:C_0402", prog_gnd)],
            ),
            (
                "cap_u",
                vec![comp("C7", "10u", "Capacitor_SMD:C_0402", prog_gnd)],
            ),
            ("fuse", vec![two_pin("F1", "1", 3, 2)]),
            ("relay", vec![two_pin("RLY1", "1", 3, 2)]),
            ("rf", vec![two_pin("RF1", "1", 3, 2)]),
            (
                "con",
                vec![comp("CON1", "1", "Capacitor_SMD:C_0603", prog_gnd)],
            ),
            ("thermistor", vec![two_pin("RT1", "10k", 3, 2)]),
            (
                "trim_pot",
                vec![
                    two_pin("R10", "10k", 3, 2),
                    comp("RV1", "10k", POT, &[("1", 3), ("2", 2), ("3", 2)]),
                ],
            ),
            ("out_of_domain", vec![two_pin("R10", "100R", 3, 2)]),
        ];
        cases.push((
            "wide",
            (0..65)
                .map(|i| two_pin(&format!("R{i}"), "10k", 3, 2))
                .collect(),
        ));
        for (tag, network) in cases {
            let got = attribute_currents(&charger_board(network), &lib_from(tag, CHARGER_TOML));
            assert!(got.cited.is_empty(), "{tag}: {:?}", got.cited);
            assert_eq!(got.undetermined.len(), 1, "{tag}");
            assert_eq!(got.undetermined[0].reference, "U1", "{tag}");
        }
    }

    /// Two chargers on one input rail sum there; cascaded stages count the
    /// middle rail's through-current once.
    #[test]
    fn regulated_stages_sum_on_shared_rails_and_do_not_double_count_cascades() {
        let two = numbered_board(
            "two_chargers",
            &["+5V", "GND", "PROG_A", "BAT_A", "PROG_B", "BAT_B"],
            vec![
                charger("U1", [1, 2, 3, 4]),
                two_pin("R1", "10k", 3, 2),
                charger("U2", [1, 2, 5, 6]),
                two_pin("R2", "10k", 5, 2),
            ],
        );
        let got = attribute_currents(&two, &lib_from("two_chargers", CHARGER_TOML));
        let (input_current, citation) = got.cited.get("+5V").expect("shared input");
        assert!((*input_current - 0.2).abs() < 1e-12, "{input_current}");
        assert!(
            citation.contains("U1") && citation.contains("U2"),
            "{citation}"
        );
        assert!((got.cited["BAT_A"].0 - 0.1).abs() < 1e-12);
        assert!((got.cited["BAT_B"].0 - 0.1).abs() < 1e-12);

        let cascaded = numbered_board(
            "cascaded_chargers",
            &["SOURCE", "GND", "PROG_A", "MIDDLE", "PROG_B", "SINK"],
            vec![
                charger("U1", [1, 2, 3, 4]),
                two_pin("R1", "10k", 3, 2),
                charger("U2", [4, 2, 5, 6]),
                two_pin("R2", "10k", 5, 2),
            ],
        );
        let got = attribute_currents(&cascaded, &lib_from("cascaded", CHARGER_TOML));
        assert!((got.cited["SOURCE"].0 - 0.1).abs() < 1e-12);
        assert!((got.cited["SINK"].0 - 0.1).abs() < 1e-12);
        assert!(
            (got.cited["MIDDLE"].0 - 0.1).abs() < 1e-12,
            "{:?}",
            got.cited["MIDDLE"]
        );
        assert!(got.cited["MIDDLE"].1.contains("U1") && got.cited["MIDDLE"].1.contains("U2"));
    }

    /// Builtin OCP-threshold programs (AP22615A ILIM, LTC4020 input limit)
    /// are capabilities, not loads: nothing is charged to any rail.
    #[test]
    fn builtin_protection_thresholds_are_not_fictional_loads() {
        let load_switch = |rset: &str| {
            numbered_board(
                "load_switch",
                &["+5V", "GND", "ISET", "SWITCHED_5V", "ENABLE", "FAULT_N"],
                vec![
                    comp(
                        "U1",
                        "AP22615A",
                        "Package_TO_SOT_SMD:TSOT-26",
                        &[("1", 4), ("2", 2), ("3", 6), ("4", 5), ("5", 3), ("6", 1)],
                    ),
                    two_pin("RSET", rset, 3, 2),
                ],
            )
        };
        for rset in ["6.8k", "1.94k"] {
            let got = attribute_currents(&load_switch(rset), &ModelLibrary::builtin());
            assert!(got.cited.is_empty(), "RSET {rset}: {:?}", got.cited);
        }

        let ltc4020 = numbered_board(
            "ltc4020_program",
            &["VIN", "GND", "SENSE_TOP", "SENSE_BOT", "ILIMIT", "BAT"],
            vec![
                comp(
                    "U2",
                    "LTC4020",
                    "Package_DFN_QFN:QFN-38",
                    &[("5", 4), ("6", 3), ("20", 6), ("25", 5), ("36", 1)],
                ),
                two_pin("R8", "7.15k", 5, 2),
                shunt("R49", "0.01", 3, 1),
                shunt("R50", "0.01", 4, 2),
            ],
        );
        let got = attribute_currents(&ltc4020, &ModelLibrary::builtin());
        assert!(got.cited.is_empty(), "{:?}", got.cited);
    }

    #[test]
    fn sense_scaled_regulation_requires_exact_equal_declared_shunt_paths() {
        let make_board = |second_shunt: &str, wrong_far: bool, extra_branch: bool| {
            let pads = [("1", 1), ("2", 2), ("3", 3), ("4", 4), ("5", 5), ("6", 6)];
            let mut components = vec![
                comp("U1", "TESTSENSE", "", &pads),
                two_pin("R1", "10k", 3, 2),
                shunt("R2", "0.01", 5, if wrong_far { 4 } else { 1 }),
                shunt("R3", second_shunt, 6, 2),
            ];
            if extra_branch {
                components.push(two_pin("R4", "0.001", 5, 7));
            }
            numbered_board(
                "sense_regulator",
                &[
                    "VIN",
                    "GND",
                    "PROG",
                    "VOUT",
                    "KELVIN_A",
                    "KELVIN_B",
                    "FILTER_NODE",
                ],
                components,
            )
        };
        let lib = lib_from("sense_regulator", SENSE_REGULATOR_TOML);

        let valid = attribute_currents(&make_board("0.01", false, false), &lib);
        assert!(valid.undetermined.is_empty());
        for rail in ["VIN", "VOUT"] {
            let (current, citation) = &valid.cited[rail];
            assert!((*current - 2.5).abs() < 1e-12, "{rail}: {current}");
            assert!(citation.contains("R2+R3") && citation.contains("equal"));
        }
        for kelvin in ["KELVIN_A", "KELVIN_B", "GND"] {
            assert!(!valid.cited.contains_key(kelvin), "{kelvin}");
        }

        let mut multi_terminal_sense = make_board("0.01", false, false);
        let pot = comp("RV1", "10k", POT, &[("1", 5), ("2", 1), ("3", 7)]);
        multi_terminal_sense.components.push(pot);
        for (label, board) in [
            ("mismatched shunts", make_board("0.02", false, false)),
            ("wrong far-side role", make_board("0.01", true, false)),
            ("extra resistive branch", make_board("0.01", false, true)),
            (
                "multi-terminal device on a Kelvin net",
                multi_terminal_sense,
            ),
        ] {
            let got = attribute_currents(&board, &lib);
            assert!(got.cited.is_empty(), "{label}: {:?}", got.cited);
            assert_eq!(got.undetermined.len(), 1, "{label}");
        }
    }

    /// The checked-in Watchy board: U3's PROG network holds R3 = 10 kOhm, so
    /// the TP4054 law attributes 100 mA to both charger rails.
    #[test]
    fn checked_in_watchy_programs_tp4054_to_one_hundred_milliamps() {
        let text = include_str!("../../../hauksbee-ci/examples/boards/watchy.kicad_pcb");
        let board = ExtractedBoard::from_kicad_pcb(text).expect("Watchy extracts");
        let got = attribute_currents(&board, &ModelLibrary::builtin());
        let charger_rails: Vec<_> = got
            .cited
            .iter()
            .filter(|(_, (_, citation))| citation.contains("U3 (tp4054)"))
            .collect();
        assert_eq!(charger_rails.len(), 2, "{charger_rails:?}");
        for (rail, (current, citation)) in charger_rails {
            assert!((*current - 0.1).abs() < 1e-12, "{rail}: {current}");
            assert!(citation.contains("R3") && citation.contains("10000"));
        }
    }

    /// The switch-node check on a 3 A, 5 V test buck (U1, switch pin 2) whose
    /// inductor L1 (`henries`) joins the switch node SWN to +5V. `card_extra`
    /// is appended to the converter block.
    fn switch_node_run(card_extra: &str, henries: &str, text: &str) -> Vec<SiFinding> {
        // One model directory per call: tests share a process under `cargo test`.
        static CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let toml_src = format!(
            r#"
[[models]]
id = "test_buck"
kind = "vreg"
description = "test 3 A buck"

[models.match]
value_re = "(?i)^TESTBUCK$"

[models.pins]
"1" = "in"
"2" = "sw"
"3" = "gnd"
"4" = "fb"

[models.behavioral.converter]
topology = "buck"
out_pin = "sw"
in_pin = "in"
vout_setpoint = 5.0
iout_limit_a = 3.0
{card_extra}
"#
        );
        let call = CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let lib = lib_from(&format!("switch_node_{call}"), &toml_src);
        let board = board(
            "buck",
            &[(1, "+12V"), (2, "SWN"), (3, "GND"), (4, "+5V")],
            vec![
                comp(
                    "U1",
                    "TESTBUCK",
                    "",
                    &[("1", 1), ("2", 2), ("3", 3), ("4", 4)],
                ),
                comp(
                    "L1",
                    henries,
                    "Inductor_SMD:L_Bourns_SRR1260",
                    &[("1", 2), ("2", 4)],
                ),
            ],
        );
        let copper = net_copper_from_text(text);
        switch_node_findings(
            &board,
            &lib,
            text,
            &copper,
            &TraceAudit::from_pcb_text(text),
        )
    }

    fn segment(width: f64, net: &str) -> String {
        format!("(segment (start 0 0) (end 5 0) (width {width}) (layer \"F.Cu\") (net \"{net}\"))")
    }

    #[test]
    fn a_buck_switch_node_on_a_thin_trace_is_a_warning() {
        let run = |text: &str| switch_node_run("", "10uH", text);
        let pcb = |sw: f64, rail: f64| {
            format!(
                "(kicad_pcb {} {})",
                segment(sw, "SWN"),
                segment(rail, "+5V")
            )
        };
        // A name-only net reference (KiCad 10 has no net table) still resolves.
        let thin = run(&pcb(0.2, 1.0));
        assert_eq!(thin.len(), 1, "{thin:?}");
        assert_eq!(thin[0].severity, SiSeverity::Medium);
        assert!(thin[0].message.contains("2 oz"), "{}", thin[0].message);
        assert!(run(&pcb(2.0, 2.0)).is_empty(), "a wide switch node is fine");
        assert!(
            run(&pcb(0.2, 0.2)).is_empty(),
            "a power path routed at one width throughout is a board-wide choice"
        );
        assert!(
            run(&format!("(kicad_pcb {})", segment(0.2, "SWN"))).is_empty(),
            "an unrouted output rail gives nothing to compare against"
        );
        let poured = format!(
            "(kicad_pcb {} {} \
             (zone (net \"SWN\") (layers \"F.Cu\") (filled_polygon (layer \"F.Cu\") (pts (xy 0 0) (xy 1 0) (xy 1 1)))))",
            segment(0.2, "SWN"),
            segment(1.0, "+5V")
        );
        assert!(
            run(&poured).is_empty(),
            "a poured switch node is out of reach"
        );
    }

    /// With pads placed, the switch node is judged on the copper between U1's
    /// switch pad (0,0) and L1 (5,0), not on the bootstrap stub to C1 at (0,3),
    /// which carries gate-drive charge rather than the inductor current.
    #[test]
    fn a_bootstrap_stub_off_the_switch_path_is_not_judged() {
        let pad = |r: &str, n: &str, x: f64, y: f64| {
            format!(
                "(footprint \"X\" (layer \"F.Cu\") (at {x} {y}) (property \"Reference\" \"{r}\") \
                 (pad \"{n}\" smd rect (at 0 0) (size 1 1) (layers \"F.Cu\") (net \"SWN\")))"
            )
        };
        let routed = |path: f64, stub: f64| {
            format!(
                "(kicad_pcb {} {} {} {} (segment (start 0 0) (end 0 3) (width {stub}) \
                 (layer \"F.Cu\") (net \"SWN\")) {})",
                pad("U1", "2", 0.0, 0.0),
                pad("L1", "1", 5.0, 0.0),
                pad("C1", "1", 0.0, 3.0),
                segment(path, "SWN"),
                segment(1.0, "+5V")
            )
        };
        assert!(
            switch_node_run("", "10uH", &routed(0.8, 0.2)).is_empty(),
            "a thin bootstrap stub is off the inductor current's path"
        );
        let on_path = switch_node_run("", "10uH", &routed(0.2, 0.8));
        assert_eq!(on_path.len(), 1, "{on_path:?}");
        assert!(
            on_path[0].message.contains("0.20 mm"),
            "{}",
            on_path[0].message
        );
    }

    /// A large inductor shows the buck was built for far less than its rating,
    /// and the switch node is judged against that load instead.
    #[test]
    fn a_buck_inductor_bounds_the_switch_node_load() {
        let text = format!(
            "(kicad_pcb {} {})",
            segment(0.2, "SWN"),
            segment(1.0, "+5V")
        );
        let fsw = "switching_hz = 500e3";
        // 10 uH at 500 kHz allows up to 5 A, above half the rating: still flagged.
        assert_eq!(switch_node_run(fsw, "10uH", &text).len(), 1);
        // 220 uH at 500 kHz implies at most 0.23 A, which 0.2 mm carries.
        let relaxed = switch_node_run(fsw, "220uH", &text);
        assert!(relaxed.is_empty(), "{relaxed:?}");
        // Without a stated switching frequency the inductor proves nothing.
        assert_eq!(switch_node_run("", "220uH", &text).len(), 1);
    }

    /// A fixed LDO's current rating is a capability, never a cited load.
    #[test]
    fn a_regulator_rating_is_capability_not_proof_of_board_load() {
        let toml_src = r#"
[[models]]
id = "test_ldo"
kind = "vreg"
description = "test fixed LDO, no programming pin"

[models.match]
value_re = "(?i)^TESTLDO$"

[models.params]
vout = 3.3
dropout_v = 0.2
iq_a = 0.001

[models.pins]
"1" = "in"
"2" = "gnd"
"3" = "out"

[models.ratings]
max_current_a = 1.0
"#;
        let board = board(
            "ldo",
            &[(1, "+5V"), (2, "GND"), (3, "+3V3")],
            vec![comp("U2", "TESTLDO", "", &[("1", 1), ("2", 2), ("3", 3)])],
        );
        let got = attribute_currents(&board, &lib_from("plain_ldo", toml_src));
        assert!(got.undetermined.is_empty());
        assert!(got.cited.is_empty(), "{:?}", got.cited);
    }

    /// Source/sink roles are data in `current_program`; control pins with
    /// familiar names are never charged, a missing declared role refuses the
    /// whole path, and one logical pad on two nets refuses too.
    #[test]
    fn programmed_power_path_uses_explicit_roles_and_refuses_incomplete_pads() {
        let model: hauksbee_models::ModelEntry = toml::from_str(
            r#"
                id = "lp2985_3v3"
                kind = "vreg"
                [pins]
                "1" = "in"
                "2" = "gnd"
                "3" = "en"
                "4" = "noise_bypass"
                "5" = "out"
                [current_program]
                pin = "noise_bypass"
                semantics = "regulated_current"
                current_in_roles = ["in"]
                current_out_roles = ["out"]
                max_operating_current_a = 0.1
                equation = "inverse_resistance"
                k_volts = 1000.0
            "#,
        )
        .expect("valid model toml");
        let program = model.current_program.as_ref().unwrap();
        let ldo_pads = [("1", 10), ("2", 11), ("3", 12), ("4", 13), ("5", 14)];
        let ldo = comp("U1", "LP2985-3.3", "", &ldo_pads);
        let board = board(
            "t",
            &[
                (10, "VIN"),
                (11, "GND"),
                (12, "EN"),
                (13, "BYP"),
                (14, "+3V3"),
            ],
            vec![ldo.clone()],
        );
        let nets = power_nets_of(&board, &ldo, &model, program);
        assert_eq!(
            nets,
            vec![10, 14],
            "only explicitly declared rails carry current"
        );

        let lib = lib_from("incomplete_pads", CHARGER_TOML);
        let board = charger_board(Vec::new());
        let missing_out = comp("U1", "TESTCHARGER", "", &[("1", 1), ("2", 2), ("3", 3)]);
        let mut conflicting = charger("U1", [1, 2, 3, 4]);
        conflicting.pins.insert(1, pin("1", 5));
        for (label, component) in [
            ("missing OUT", missing_out),
            ("conflicting pads", conflicting),
        ] {
            let part = AssemblyState::of(&component).fitted().unwrap();
            let model = resolve(&lib, part).model.unwrap();
            let program = model.current_program.as_ref().unwrap();
            assert!(
                programmed_power_rails(&board, part, &model, program).is_none(),
                "{label}"
            );
        }
    }
}
