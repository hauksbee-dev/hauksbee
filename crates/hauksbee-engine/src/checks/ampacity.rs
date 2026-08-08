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
//! current, and stays silent on the known-good corpus, exactly as the LumenPnP
//! sweep (`trace_current_corpus.rs`) pins.

use std::collections::{hash_map::Entry, BTreeSet, HashMap, HashSet, VecDeque};

use hauksbee_extract::{
    audit_trace_currents, net_copper_from_text, ExtractedBoard, SiCheck, SiFinding, SiReport,
    SiSeverity, TraceAudit,
};
use hauksbee_models::ModelLibrary;

use crate::{binder::resolve, component_evidence::role_net};
use hauksbee_extract::assembly::{AssemblyState, FittedComponent};

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
            citation.push_str(&format!(
                "; net-flow balance uses max({:.3} A load side, {:.3} A source side), not their double-counted sum",
                self.into_components_a, self.out_of_components_a
            ));
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

/// How a two-terminal part on the path from a programming pin to ground behaves.
enum Series {
    /// A resistor of this many ohms.
    Ohms(f64),
    /// A closed link (a `SJ_Closed` solder jumper, a `0R`): a short.
    Closed,
    /// An open link: the path does not conduct through this part.
    Open,
    /// Anything else. The walk stops rather than assume: a diode, a fuse, a cap
    /// or an unrecognised part in this position each mean something different,
    /// and guessing which would be guessing the current.
    Unknown,
}

/// The two distinct electrical nets of a two-terminal part.
///
/// Layout readers preserve physical pad placements, so one numbered terminal
/// may appear more than once (for example on multiple copper layers). Count
/// numbered electrical terminals rather than raw pad records. A missing net may
/// be enriched by another record for the same number; contradictory nets or a
/// non-two-terminal part are refused.
fn electrical_terminal_nets(component: &hauksbee_extract::Component) -> Option<(i64, i64)> {
    let (first, second) = electrical_terminal_net_options(component).ok()??;
    let first = first?;
    let second = second?;
    (first != second).then_some((first, second))
}

/// The two logical terminal nets, retaining a missing-net state so a topology
/// reader can distinguish "not a two-terminal part" from "a two-terminal part
/// whose connection is incomplete". Contradictory duplicate physical pad
/// records are an error; callers must not first-win them.
fn electrical_terminal_net_options(
    component: &hauksbee_extract::Component,
) -> Result<Option<(Option<i64>, Option<i64>)>, ()> {
    let mut terminals: Vec<(&str, Option<i64>)> = Vec::new();
    for pin in &component.pins {
        let number = pin.number.trim();
        if number.is_empty() {
            return Err(());
        }
        if let Some((_, known_net)) = terminals
            .iter_mut()
            .find(|(known_number, _)| *known_number == number)
        {
            match (*known_net, pin.net) {
                (Some(previous), Some(incoming)) if previous != incoming => return Err(()),
                (None, Some(incoming)) => *known_net = Some(incoming),
                _ => {}
            }
        } else {
            terminals.push((number, pin.net));
        }
    }
    if terminals.len() != 2 {
        return Ok(None);
    }
    Ok(Some((terminals[0].1, terminals[1].1)))
}

fn component_hint(component: &hauksbee_extract::Component) -> String {
    format!(
        "{} {} {}",
        component.reference, component.footprint, component.lib_id
    )
    .to_ascii_lowercase()
}

/// Conventional single-part designator grammar: one class letter followed by
/// a digit (or KiCad's unannotated `?`). This keeps R10/C10 useful without
/// misclassifying RLY1, RF1, CON1, or CR1 merely because they share a prefix.
fn conventional_designator(reference: &str, class: char) -> bool {
    let mut chars = reference.trim().chars();
    chars
        .next()
        .is_some_and(|first| first.eq_ignore_ascii_case(&class))
        && chars
            .next()
            .is_some_and(|next| next.is_ascii_digit() || next == '?')
}

fn metadata_may_classify(reference: &str) -> bool {
    let reference = reference.trim().to_ascii_uppercase();
    reference.is_empty()
        || reference.starts_with("UNK")
        || reference
            .chars()
            .next()
            .is_none_or(|first| !first.is_ascii_alphabetic())
}

fn is_resistor_component(component: &hauksbee_extract::Component) -> bool {
    let hint = component_hint(component);
    let reference = component.reference.trim().to_ascii_uppercase();
    if reference.starts_with("RT")
        || reference.starts_with("RV")
        || hint.contains("thermistor")
        || hint.contains("varistor")
    {
        return false;
    }
    let conventional_reference = conventional_designator(&reference, 'R');
    let metadata_resistor = hint.contains("resistor")
        || hint.contains("res_")
        || hint.contains("resc")
        || hint.contains(":r_")
        || hint.contains(" r_");
    conventional_reference || (metadata_may_classify(&reference) && metadata_resistor)
}

fn is_capacitor_component(component: &hauksbee_extract::Component) -> bool {
    let hint = component_hint(component);
    let reference = component.reference.trim().to_ascii_uppercase();
    let metadata_capacitor =
        hint.contains("capacitor") || hint.contains("cap_") || hint.contains("capc");
    conventional_designator(&reference, 'C')
        || (metadata_may_classify(&reference) && metadata_capacitor)
}

fn classify_series(comp: &hauksbee_extract::Component) -> Series {
    // An identity-refused record cannot be read as a resistor value: Unknown
    // stops the walk, so the current stays undetermined rather than guessed.
    if matches!(AssemblyState::of(comp), AssemblyState::IdentityUnknown(_)) {
        return Series::Unknown;
    }
    let v = comp.value.trim().to_ascii_lowercase();
    let fp = comp.footprint.to_ascii_lowercase();
    if v == "open" || fp.contains("sj_open") || fp.contains("jumper_open") {
        return Series::Open;
    }
    if matches!(v.as_str(), "closed" | "short" | "link" | "jumper")
        || fp.contains("sj_closed")
        || fp.contains("jumper_closed")
    {
        return Series::Closed;
    }
    let parsed = hauksbee_models::value::parse_value(&comp.value);
    if is_capacitor_component(comp)
        || parsed
            .as_ref()
            .is_some_and(|p| matches!(p.unit.as_deref(), Some("F")))
    {
        // A capacitor is an open circuit for the DC programming equation. The
        // reference/footprint gate matters because real BOMs commonly spell a
        // capacitor `100n` or `10u`, which carries no explicit unit token.
        return Series::Open;
    }
    if !is_resistor_component(comp) {
        return Series::Unknown;
    }
    match parsed {
        // A value with a farad/henry unit in this position is not a resistor, and
        // a capacitor to ground on a PROG pin is a filter, not the programming
        // element.
        Some(p) if matches!(p.unit.as_deref(), Some("F") | Some("H")) => Series::Unknown,
        Some(p) if p.si == 0.0 => Series::Closed,
        Some(p) if p.si > 0.0 => Series::Ohms(p.si),
        _ => Series::Unknown,
    }
}

#[derive(Clone)]
struct ResistanceEdge {
    first: i64,
    second: i64,
    ohms: f64,
    reference: String,
    closed: bool,
}

/// Solve a dense linear system with partial pivoting. Programming networks are
/// intentionally bounded to a handful of nodes, so a small auditable solver is
/// preferable to adding a heavyweight sparse dependency.
fn solve_linear(mut matrix: Vec<Vec<f64>>, mut rhs: Vec<f64>) -> Option<Vec<f64>> {
    let n = rhs.len();
    for column in 0..n {
        let pivot = (column..n).max_by(|&left, &right| {
            matrix[left][column]
                .abs()
                .total_cmp(&matrix[right][column].abs())
        })?;
        let magnitude = matrix[pivot][column].abs();
        if !magnitude.is_finite() || magnitude == 0.0 {
            return None;
        }
        matrix.swap(column, pivot);
        rhs.swap(column, pivot);
        for row in (column + 1)..n {
            let factor = matrix[row][column] / matrix[column][column];
            if !factor.is_finite() {
                return None;
            }
            matrix[row][column] = 0.0;
            for inner in (column + 1)..n {
                matrix[row][inner] -= factor * matrix[column][inner];
            }
            rhs[row] -= factor * rhs[column];
        }
    }
    let mut solution = vec![0.0; n];
    for row in (0..n).rev() {
        let known: f64 = ((row + 1)..n)
            .map(|column| matrix[row][column] * solution[column])
            .sum();
        solution[row] = (rhs[row] - known) / matrix[row][row];
        if !solution[row].is_finite() {
            return None;
        }
    }
    Some(solution)
}

/// The populated DC-equivalent resistance from `start` to ground, plus the
/// conductive components that support the solved network.
///
/// This is a bounded nodal-conductance solve, not a shortest-path heuristic:
/// simultaneously populated parallel/bridge branches all affect the result.
/// Known-open capacitors/jumpers are excluded. Any unreadable two-terminal part
/// touching the reachable network makes the result undetermined rather than
/// being silently assumed open or resistive. Positive-resistance branches with
/// zero solved test current are omitted from the citation. Closed ideal links
/// are retained because current division through a zero-ohm subnetwork is not
/// uniquely recoverable from this equivalent-resistance solve.
fn program_resistance_to_ground(
    board: &ExtractedBoard,
    start: i64,
    max_hops: usize,
    excluded_component: Option<usize>,
) -> Option<(f64, Vec<String>)> {
    const MAX_PROGRAM_NODES: usize = 32;
    const MAX_PROGRAM_EDGES: usize = 64;

    let start_net = board.net(start)?;
    if super::converter::is_ground_net(&start_net.name) {
        return None;
    }

    let mut queue = VecDeque::from([(start, 0usize)]);
    let mut depth_by_net = HashMap::from([(start, 0usize)]);
    let mut discovered_nets = HashSet::from([start]);
    let mut consumed_components = HashSet::new();
    let mut edges = Vec::new();
    let mut reached_ground = false;

    while let Some((net_id, depth)) = queue.pop_front() {
        for (component_index, component) in board.components.iter().enumerate() {
            // Only a DNP part is skipped as electrically absent; an
            // identity-refused record must instead poison the walk below
            // (classify_series says Unknown), never be silently dropped.
            if matches!(AssemblyState::of(component), AssemblyState::DnpAbsent(_))
                || Some(component_index) == excluded_component
            {
                continue;
            }
            let terminals = match electrical_terminal_net_options(component) {
                Ok(Some(terminals)) => terminals,
                Ok(None) => {
                    // A multi-/single-terminal device on the programming net
                    // can change its DC equivalent (a trim pot is the common
                    // case). Ignore only the programmed IC itself, passed above.
                    if component.pins.iter().any(|pin| pin.net == Some(net_id)) {
                        return None;
                    }
                    continue;
                }
                Err(()) => {
                    if component.pins.iter().any(|pin| pin.net == Some(net_id)) {
                        return None;
                    }
                    continue;
                }
            };
            let far = match terminals {
                (Some(first), Some(second)) if first == net_id && second != net_id => second,
                (Some(first), Some(second)) if second == net_id && first != net_id => first,
                (Some(first), None) | (None, Some(first)) if first == net_id => return None,
                _ => continue,
            };

            let (ohms, closed) = match classify_series(component) {
                Series::Ohms(ohms) if ohms.is_finite() && ohms > 0.0 => (ohms, false),
                Series::Closed => (0.0, true),
                Series::Open => continue,
                Series::Ohms(_) | Series::Unknown => return None,
            };
            if consumed_components.insert(component_index) {
                edges.push(ResistanceEdge {
                    first: net_id,
                    second: far,
                    ohms,
                    reference: component.reference.clone(),
                    closed,
                });
                if edges.len() > MAX_PROGRAM_EDGES {
                    return None;
                }
            }
            if discovered_nets.insert(far) && discovered_nets.len() > MAX_PROGRAM_NODES {
                return None;
            }

            let far_net = board.net(far)?;
            if super::converter::is_ground_net(&far_net.name) {
                reached_ground = true;
                continue;
            }
            if let Entry::Vacant(entry) = depth_by_net.entry(far) {
                if depth >= max_hops {
                    return None;
                }
                entry.insert(depth + 1);
                queue.push_back((far, depth + 1));
            }
        }
    }
    if !reached_ground {
        return None;
    }

    let mut node_ids: Vec<i64> = edges
        .iter()
        .flat_map(|edge| [edge.first, edge.second])
        .chain(std::iter::once(start))
        .collect();
    node_ids.sort_unstable();
    node_ids.dedup();
    let node_index: HashMap<i64, usize> = node_ids
        .iter()
        .enumerate()
        .map(|(index, id)| (*id, index))
        .collect();

    let mut parents: Vec<usize> = (0..node_ids.len()).collect();
    fn root(parents: &mut [usize], index: usize) -> usize {
        if parents[index] != index {
            parents[index] = root(parents, parents[index]);
        }
        parents[index]
    }
    fn unite(parents: &mut [usize], left: usize, right: usize) {
        let left = root(parents, left);
        let right = root(parents, right);
        if left != right {
            parents[right] = left;
        }
    }
    for edge in &edges {
        if edge.closed {
            unite(
                &mut parents,
                node_index[&edge.first],
                node_index[&edge.second],
            );
        }
    }

    let start_root = root(&mut parents, node_index[&start]);
    let mut ground_roots = HashSet::new();
    for net_id in &node_ids {
        if board
            .net(*net_id)
            .is_some_and(|net| super::converter::is_ground_net(&net.name))
        {
            let index = node_index[net_id];
            ground_roots.insert(root(&mut parents, index));
        }
    }
    if ground_roots.contains(&start_root) {
        return None;
    }

    let mut variable_roots = BTreeSet::new();
    for index in 0..node_ids.len() {
        let component_root = root(&mut parents, index);
        if !ground_roots.contains(&component_root) {
            variable_roots.insert(component_root);
        }
    }
    let variable_roots: Vec<usize> = variable_roots.into_iter().collect();
    let variable_index: HashMap<usize, usize> = variable_roots
        .iter()
        .enumerate()
        .map(|(index, component_root)| (*component_root, index))
        .collect();
    let mut conductance = vec![vec![0.0; variable_roots.len()]; variable_roots.len()];
    for edge in edges.iter().filter(|edge| !edge.closed) {
        let first = root(&mut parents, node_index[&edge.first]);
        let second = root(&mut parents, node_index[&edge.second]);
        if first == second {
            continue;
        }
        let g = 1.0 / edge.ohms;
        if !g.is_finite() || g <= 0.0 {
            return None;
        }
        if let Some(&index) = variable_index.get(&first) {
            conductance[index][index] += g;
        }
        if let Some(&index) = variable_index.get(&second) {
            conductance[index][index] += g;
        }
        if let (Some(&left), Some(&right)) =
            (variable_index.get(&first), variable_index.get(&second))
        {
            conductance[left][right] -= g;
            conductance[right][left] -= g;
        }
    }
    let mut injection = vec![0.0; variable_roots.len()];
    injection[*variable_index.get(&start_root)?] = 1.0;
    let voltages = solve_linear(conductance, injection)?;
    let equivalent_ohms = voltages[*variable_index.get(&start_root)?];
    if !equivalent_ohms.is_finite() || equivalent_ohms <= 0.0 {
        return None;
    }

    let voltage = |component_root: usize| -> f64 {
        variable_index
            .get(&component_root)
            .map_or(0.0, |&index| voltages[index])
    };
    let mut references = BTreeSet::new();
    for edge in &edges {
        if edge.closed {
            references.insert(edge.reference.clone());
            continue;
        }
        let first = root(&mut parents, node_index[&edge.first]);
        let second = root(&mut parents, node_index[&edge.second]);
        let branch_current = (voltage(first) - voltage(second)).abs() / edge.ohms;
        if branch_current > 1e-9 {
            references.insert(edge.reference.clone());
        }
    }
    Some((equivalent_ohms, references.into_iter().collect()))
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
        let terminals = match electrical_terminal_nets(component) {
            Some(terminals) => terminals,
            None => {
                if component.pins.iter().any(|pin| pin.net == Some(sense_net)) {
                    return None;
                }
                continue;
            }
        };
        let far_net = if terminals.0 == sense_net {
            terminals.1
        } else if terminals.1 == sense_net {
            terminals.0
        } else {
            continue;
        };
        match classify_series(component) {
            Series::Open => continue,
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

/// Attribute cited currents to nets from the bound DB models. Returns a map of
/// `net name -> (current, citation)` in the shape [`audit_trace_currents`] wants.
fn attribute_currents(board: &ExtractedBoard, lib: &ModelLibrary) -> Attributions {
    let mut out: HashMap<String, Attribution> = HashMap::new();
    let mut undetermined: Vec<Undetermined> = Vec::new();
    let mut skipped_identity: Vec<(String, String)> = Vec::new();

    let mut consider = |net_id: Option<i64>, current_a: f64, citation: String, flow: RailFlow| {
        let Some(id) = net_id else { return };
        if current_a <= 0.0 {
            return;
        }
        let Some(net) = board.net(id) else { return };
        out.entry(net.name.clone())
            .or_default()
            .add(current_a, citation, flow);
    };

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

        let is_generic = model.id.starts_with("generic");

        // A regulated operating current selected by the board's programming resistor. The
        // equation and its normal-operating limit live together; an absolute
        // rating is never substituted for either one. The block itself is enough
        // evidence that this part can carry a programmed rail current only when
        // the model explicitly distinguishes regulation from protection.
        if let Some(program) = &model.current_program {
            if !is_generic {
                if program.semantics
                    == hauksbee_models::schema::CurrentProgramSemantics::ProtectionLimit
                {
                    // An OCP/current-limit setting is a protection threshold,
                    // not evidence that the protected rail continuously draws
                    // that current. `current_program` currently records and
                    // validates that meaning; model-specific behavioural blocks
                    // implement any dynamic protection simulation separately.
                    continue;
                }
                let role = program.pin.clone();
                let programmed = pin_net_for_role(part, model, &program.pin).and_then(|net| {
                    let (ohms, path) =
                        program_resistance_to_ground(board, net, 4, Some(component_index))?;
                    match &program.equation {
                        hauksbee_models::schema::CurrentProgramEquation::SenseScaledResistance {
                            sense_roles,
                            sense_far_roles,
                            ..
                        } => {
                            let mut sense_shunts = Vec::new();
                            for (sense_role, far_role) in
                                sense_roles.iter().zip(sense_far_roles)
                            {
                                let sense_net = pin_net_for_role(part, model, sense_role)?;
                                let expected_far = if far_role.eq_ignore_ascii_case("ground") {
                                    ExpectedSenseFar::Ground
                                } else {
                                    ExpectedSenseFar::Net(pin_net_for_role(
                                        part, model, far_role,
                                    )?)
                                };
                                let (sense_ohms, reference) = adjacent_sense_resistor(
                                    board,
                                    sense_net,
                                    &expected_far,
                                    component_index,
                                )?;
                                sense_shunts.push((sense_ohms, reference));
                            }
                            let sense_ohms = sense_shunts.first()?.0;
                            if sense_shunts.iter().any(|(ohms, _)| {
                                (*ohms - sense_ohms).abs()
                                    > sense_ohms.abs().max(ohms.abs()) * 1e-6
                            }) {
                                return None;
                            }
                            let equation_current_a =
                                program.equation_current_with_sense_a(ohms, sense_ohms)?;
                            let operating_current_a =
                                program.operating_current_with_sense_a(ohms, sense_ohms)?;
                            Some((
                                equation_current_a,
                                operating_current_a,
                                ohms,
                                path,
                                programmed_power_rails(board, part, model, program)?,
                                Some((sense_ohms, sense_shunts)),
                            ))
                        }
                        _ => {
                            let equation_current_a = program.equation_current_a(ohms)?;
                            let operating_current_a = program.operating_current_a(ohms)?;
                            Some((
                                equation_current_a,
                                operating_current_a,
                                ohms,
                                path,
                                programmed_power_rails(board, part, model, program)?,
                                None,
                            ))
                        }
                    }
                });
                match programmed {
                    Some((equation_current_a, current_a, ohms, path, nets, sense)) => {
                        let limit_note = if equation_current_a > current_a {
                            format!(
                                "; equation requests {equation_current_a:.3} A, bounded by the \
                                 {current_a:.3} A normal-operating limit"
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
                            "{} ({}) regulated current {:.3} A programmed by {} on {} \
                             ({:.0} ohm){}{} [datasheet equation]",
                            comp.reference,
                            model.id,
                            current_a,
                            path.join("+"),
                            role,
                            ohms,
                            sense_note,
                            limit_note,
                        );
                        for (net_id, flow) in nets {
                            consider(Some(net_id), current_a, citation.clone(), flow);
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
            continue;
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

/// Run the trace-ampacity check and append its findings (and an info note) to an
/// SI report. `pcb_text` is the raw `.kicad_pcb` text (the copper geometry source);
/// when it is absent or not a KiCad layout, the check is inert.
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
        report.findings.push(SiFinding {
            check: SiCheck::TraceAmpacity,
            severity: SiSeverity::High,
            message: format!(
                "net '{}' narrowest routed trace {:.2} mm carries a cited {:.2} A but IPC-2221 \
                 rates that width at only {:.2} A ({}, {:.0} C rise); needs >= {:.2} mm. \
                 Attributed from: {}",
                f.net,
                f.min_width_mm,
                f.cited_current_a,
                f.ampacity_a,
                f.describe_copper(),
                audit.dt_c,
                f.required_width_mm,
                f.citation,
            ),
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
                format!(
                    "Its {amps:.2} A normal-operating ceiling is a capability, not a load, so it is not charged to the rail."
                )
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
             IPC-2221 width. Poured rails are exempt (their cross-section is the plane, not the \
             discrete stubs).",
            cited.len(),
            findings.len()
        ),
        refs: vec![],
        nets: vec![],
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    use hauksbee_extract::{Component, Net, Pin};

    fn pin(number: &str, net: i64) -> Pin {
        Pin {
            number: number.into(),
            net: Some(net),
            function: String::new(),
            kind: String::new(),
            position: None,
        }
    }

    fn model_from_toml(src: &str) -> hauksbee_models::ModelEntry {
        toml::from_str(src).expect("valid model toml")
    }

    /// A one-entry library holding `toml`, written to a temp dir keyed on `tag`.
    fn lib_from(tag: &str, toml_src: &str) -> ModelLibrary {
        let dir =
            std::env::temp_dir().join(format!("hauksbee_ampacity_{tag}_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
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

    fn comp(reference: &str, value: &str, footprint: &str, pins: Vec<Pin>) -> Component {
        Component {
            reference: reference.into(),
            value: value.into(),
            lib_id: String::new(),
            footprint: footprint.into(),
            position: None,
            layer: String::new(),
            properties: vec![],
            dnp: false,
            pins,
        }
    }

    fn net(id: i64, name: &str) -> Net {
        Net {
            id,
            name: name.into(),
        }
    }

    /// Nets: 1 = +5V (in), 2 = GND, 3 = PROG, 4 = VBAT (out), 5 = the node
    /// between the programming resistor and its solder link.
    fn charger_board(prog_network: Vec<Component>) -> ExtractedBoard {
        let mut components = vec![comp(
            "U1",
            "TESTCHARGER",
            String::new().as_str(),
            vec![pin("1", 1), pin("2", 2), pin("3", 3), pin("4", 4)],
        )];
        components.extend(prog_network);
        ExtractedBoard {
            name: "charger".into(),
            nets: vec![
                net(1, "+5V"),
                net(2, "GND"),
                net(3, "PROG"),
                net(4, "VBAT"),
                net(5, "PROG_LINK"),
            ],
            components,
        }
    }

    #[test]
    fn a_programmed_charger_is_attributed_its_programmed_current_not_its_ceiling() {
        // The Olimex ESP32-EVB topology exactly: PROG -> 4.99k -> closed solder
        // jumper -> GND. I = 1000 / 4990 = 200 mA, a fifth of the 1 A ceiling,
        // and the ratio is the difference between a clean board and two High
        // findings on correctly-sized rails.
        let board = charger_board(vec![
            comp(
                "R10",
                "4.99k/1%/R0603",
                "Resistor_SMD:R_0603",
                vec![pin("1", 3), pin("2", 5)],
            ),
            comp(
                "E1",
                "Closed",
                "OLIMEX_Jumpers-FP:SJ_Closed",
                vec![pin("1", 5), pin("2", 2)],
            ),
        ]);
        let lib = lib_from("programmed", CHARGER_TOML);
        let got = attribute_currents(&board, &lib);
        assert!(
            got.undetermined.is_empty(),
            "the resistor is readable, so nothing should be undetermined: {:?}",
            got.undetermined
                .iter()
                .map(|u| u.reference.clone())
                .collect::<Vec<_>>()
        );
        for rail in ["+5V", "VBAT"] {
            let (current, citation) = got
                .cited
                .get(rail)
                .unwrap_or_else(|| panic!("{rail} should carry the programmed current"));
            // 1000 V / 4990 ohm, not a rounded 200 mA: the equation is what is
            // being tested, so the expectation is the equation.
            assert!(
                (*current - 1000.0 / 4990.0).abs() < 1e-9,
                "{rail}: expected the programmed {} A, got {current}",
                1000.0 / 4990.0
            );
            assert!(
                citation.contains("R10") && citation.contains("4990"),
                "the citation must name the resistor a reader can check: {citation}"
            );
        }
        assert!(
            !got.cited.contains_key("PROG"),
            "the programming pin itself carries no rail current"
        );
    }

    /// Two-sided IdentityUnknown contract: the same programmed charger with a
    /// refused identity attributes nothing anywhere (an unknown part must not
    /// become a fitted 200 mA charger), and the skip is recorded by name so
    /// the report can say the rails went unexamined.
    #[test]
    fn an_identity_refused_part_is_skipped_and_named_not_attributed() {
        let mut board = charger_board(vec![
            comp(
                "R10",
                "4.99k/1%/R0603",
                "Resistor_SMD:R_0603",
                vec![pin("1", 3), pin("2", 5)],
            ),
            comp(
                "E1",
                "Closed",
                "OLIMEX_Jumpers-FP:SJ_Closed",
                vec![pin("1", 5), pin("2", 2)],
            ),
        ]);
        board.components[0].properties.push((
            hauksbee_extract::DUPLICATE_REFERENCE_CONFLICT_KEY.to_string(),
            "two populated records with different values".to_string(),
        ));
        let lib = lib_from("identity_refused", CHARGER_TOML);
        let got = attribute_currents(&board, &lib);
        assert!(
            got.cited.is_empty(),
            "a refused identity must attribute nothing: {:?}",
            got.cited.keys().collect::<Vec<_>>()
        );
        assert_eq!(
            got.skipped_identity.len(),
            1,
            "the skip must be on the record: {:?}",
            got.skipped_identity
        );
        let (reference, reason) = &got.skipped_identity[0];
        assert_eq!(reference, "U1");
        assert!(
            reason.contains("duplicate designator"),
            "the reason must be the refusal itself: {reason}"
        );
    }

    /// The same charger with its programming equation retagged as a protection
    /// threshold. The resistor is present and readable, so the only thing
    /// standing between the board and an invented 200 mA steady-state load is
    /// the `protection_limit` semantics gate.
    const PROTECTION_TOML: &str = r#"
[[models]]
id = "test_protected"
kind = "vreg"
description = "test part whose PROG pin sets an OCP threshold, not a load"

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
semantics = "protection_limit"
equation = "inverse_resistance"
k_volts = 1000.0
"#;

    #[test]
    fn a_protection_limit_program_contributes_no_ampacity_load() {
        // Fully populated, unambiguous programming network: PROG -> 4.99k ->
        // GND. Under `regulated_current` semantics this exact topology
        // attributes ~200 mA to both rails (the test above proves it). Under
        // `protection_limit` it must attribute nothing, and it must not appear
        // as an undetermined hole either: a trip threshold is a capability,
        // not a missing load measurement.
        let board = charger_board(vec![comp(
            "R10",
            "4.99k",
            "Resistor_SMD:R_0603",
            vec![pin("1", 3), pin("2", 2)],
        )]);
        let lib = lib_from("protection_limit", PROTECTION_TOML);
        let got = attribute_currents(&board, &lib);
        assert!(
            got.cited.is_empty(),
            "a protection threshold must never seed steady-state ampacity: {:?}",
            got.cited
        );
        assert!(
            got.undetermined.is_empty(),
            "a protection threshold is not an undetermined load either: {:?}",
            got.undetermined
                .iter()
                .map(|u| u.reference.clone())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn an_open_link_in_the_programming_path_attributes_nothing_and_names_the_gap() {
        // Same board with the jumper open: the charger is not programmed at all,
        // so there is no current to attribute. The ceiling must NOT step in as a
        // fallback, and the hole must be visible rather than silent.
        let board = charger_board(vec![
            comp(
                "R10",
                "4.99k",
                "Resistor_SMD:R_0603",
                vec![pin("1", 3), pin("2", 5)],
            ),
            comp(
                "E1",
                "Open",
                "OLIMEX_Jumpers-FP:SJ_Open",
                vec![pin("1", 5), pin("2", 2)],
            ),
        ]);
        let lib = lib_from("open_link", CHARGER_TOML);
        let got = attribute_currents(&board, &lib);
        assert!(
            got.cited.is_empty(),
            "an unprogrammed charger must not have its ceiling charged to a rail: {:?}",
            got.cited
        );
        assert_eq!(got.undetermined.len(), 1, "the gap must be recorded");
        let u = &got.undetermined[0];
        assert_eq!(u.reference, "U1");
        assert_eq!(u.role, "prog");
        assert_eq!(u.ceiling_a, Some(0.4));
    }

    #[test]
    fn converging_program_paths_use_the_populated_equivalent_resistance() {
        // Both populated branches conduct simultaneously. Their first two
        // series legs are 200 ohm and 2 ohm, in parallel, followed by 1 ohm:
        // R_eq = (200 || 2) + 1 = 2.980198... ohm. Selecting only the shortest
        // path would silently understate an inverse-law current.
        let branch = |slow_first: bool| {
            let mut first_hop = vec![
                comp(
                    "R1",
                    "100R",
                    "Resistor_SMD:R_0603",
                    vec![pin("1", 3), pin("2", 6)],
                ),
                comp(
                    "R2",
                    "1R",
                    "Resistor_SMD:R_0603",
                    vec![pin("1", 3), pin("2", 7)],
                ),
            ];
            if !slow_first {
                first_hop.reverse();
            }
            first_hop.extend([
                comp(
                    "R3",
                    "100R",
                    "Resistor_SMD:R_0603",
                    vec![pin("1", 6), pin("2", 8)],
                ),
                comp(
                    "R4",
                    "1R",
                    "Resistor_SMD:R_0603",
                    vec![pin("1", 7), pin("2", 8)],
                ),
                comp(
                    "R5",
                    "1R",
                    "Resistor_SMD:R_0603",
                    vec![pin("1", 8), pin("2", 2)],
                ),
            ]);
            let mut board = charger_board(first_hop);
            board.nets.extend([
                net(6, "SLOW_BRANCH"),
                net(7, "FAST_BRANCH"),
                net(8, "CONVERGED"),
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
            assert_eq!(
                path,
                ["R1", "R2", "R3", "R4", "R5"],
                "the citation must retain every populated contributing branch"
            );
        }
    }

    #[test]
    fn parallel_program_resistors_are_solved_as_parallel_not_one_path() {
        let board = charger_board(vec![
            comp(
                "R10A",
                "10k",
                "Resistor_SMD:R_0603",
                vec![pin("1", 3), pin("2", 2)],
            ),
            comp(
                "R10B",
                "10k",
                "Resistor_SMD:R_0603",
                vec![pin("1", 3), pin("2", 2)],
            ),
        ]);
        let (ohms, path) = program_resistance_to_ground(&board, 3, 4, Some(0)).unwrap();
        assert!((ohms - 5_000.0).abs() < 1e-9, "got {ohms} via {path:?}");
        assert_eq!(path, ["R10A", "R10B"]);
    }

    #[test]
    fn repeated_physical_pad_records_still_form_two_electrical_terminals() {
        // Layout readers preserve physical pad records: a through/multilayer
        // pad can therefore repeat one electrical pin. Electrical topology must
        // count the two unique numbered terminals rather than `pins.len()`.
        let board = charger_board(vec![comp(
            "R10",
            "10k",
            "Resistor_SMD:R_0603",
            vec![pin("1", 3), pin("1", 3), pin("2", 2), pin("2", 2)],
        )]);
        let got = attribute_currents(&board, &lib_from("physical_pads", CHARGER_TOML));
        let (current, citation) = got.cited.get("VBAT").expect("VBAT attributed");
        assert!((*current - 0.1).abs() < 1e-12, "{current}: {citation}");
        assert!(got.undetermined.is_empty());
    }

    #[test]
    fn an_ambiguous_duplicate_resistor_never_programs_a_precise_current() {
        let mut resistor = comp(
            "R10",
            "4.99k",
            "Resistor_SMD:R_0603",
            vec![pin("1", 3), pin("2", 2)],
        );
        resistor.properties.push((
            hauksbee_extract::DUPLICATE_REFERENCE_CONFLICT_KEY.into(),
            "records named 'R10' disagree on value".into(),
        ));
        let board = charger_board(vec![resistor]);
        let got = attribute_currents(&board, &lib_from("ambiguous_program", CHARGER_TOML));

        assert!(
            got.cited.is_empty(),
            "conflicting source identity cannot support a datasheet-precise current: {:?}",
            got.cited
        );
        assert_eq!(got.undetermined.len(), 1);
        assert_eq!(got.undetermined[0].reference, "U1");
    }

    #[test]
    fn an_inferred_ambiguous_resistor_never_programs_a_precise_current() {
        let mut resistor = comp(
            "R10",
            "4.99k",
            "Resistor_SMD:R_0603",
            vec![pin("1", 3), pin("2", 2)],
        );
        resistor.properties.push((
            hauksbee_extract::altium::REFERENCE_AMBIGUOUS_KEY.into(),
            "same hierarchy, no authoritative source UID".into(),
        ));
        let board = charger_board(vec![resistor]);
        let got = attribute_currents(&board, &lib_from("inferred_program", CHARGER_TOML));

        assert!(
            got.cited.is_empty(),
            "inferred physical identity cannot support a datasheet-precise current: {:?}",
            got.cited
        );
        assert_eq!(got.undetermined.len(), 1);
        assert_eq!(got.undetermined[0].reference, "U1");
    }

    #[test]
    fn a_capacitor_is_not_read_as_a_programming_resistor() {
        // A filter cap from PROG to ground is not the programming element, and
        // reading it as one would produce a current from a farad value.
        for (index, value) in ["100nF", "100n", "10u"].into_iter().enumerate() {
            let board = charger_board(vec![comp(
                "C7",
                value,
                "Capacitor_SMD:C_0402",
                vec![pin("1", 3), pin("2", 2)],
            )]);
            let lib = lib_from(&format!("cap_{index}"), CHARGER_TOML);
            let got = attribute_currents(&board, &lib);
            assert!(
                got.cited.is_empty(),
                "a {value} cap programs nothing: {:?}",
                got.cited
            );
            assert_eq!(got.undetermined.len(), 1);
        }
    }

    #[test]
    fn a_numeric_fuse_is_not_read_as_a_programming_resistor() {
        let board = charger_board(vec![comp(
            "F1",
            "1",
            // Wrong/misleading footprint metadata must not override an explicit
            // fuse designator and turn a 1 A fuse value into one ohm.
            "Resistor_SMD:R_0603",
            vec![pin("1", 3), pin("2", 2)],
        )]);
        let got = attribute_currents(&board, &lib_from("numeric_fuse", CHARGER_TOML));
        assert!(got.cited.is_empty());
        assert_eq!(got.undetermined.len(), 1);
    }

    #[test]
    fn non_passive_reference_prefixes_override_misleading_passive_footprints() {
        for (reference, footprint) in [
            ("RLY1", "Resistor_SMD:R_0603"),
            ("RF1", "Resistor_SMD:R_0603"),
            ("CON1", "Capacitor_SMD:C_0603"),
            ("CR1", "Capacitor_SMD:C_0603"),
        ] {
            let board = charger_board(vec![comp(
                reference,
                "1",
                footprint,
                vec![pin("1", 3), pin("2", 2)],
            )]);
            let got = attribute_currents(
                &board,
                &lib_from(&format!("reference_{reference}"), CHARGER_TOML),
            );
            assert!(
                got.cited.is_empty(),
                "{reference} must not become a resistor"
            );
            assert_eq!(got.undetermined.len(), 1, "{reference}");
        }
    }

    #[test]
    fn a_multi_terminal_part_touching_prog_refuses_the_network() {
        let board = charger_board(vec![
            comp(
                "R10",
                "10k",
                "Resistor_SMD:R_0603",
                vec![pin("1", 3), pin("2", 2)],
            ),
            // A three-terminal trim pot in parallel can halve the effective
            // resistance. Ignoring it would under-report regulated current.
            comp(
                "RV1",
                "10k",
                "Potentiometer_THT:Potentiometer",
                vec![pin("1", 3), pin("2", 2), pin("3", 2)],
            ),
        ]);
        let got = attribute_currents(&board, &lib_from("trim_pot", CHARGER_TOML));
        assert!(got.cited.is_empty());
        assert_eq!(got.undetermined.len(), 1);
    }

    #[test]
    fn an_overwide_parallel_program_network_refuses_before_dense_solving() {
        let branches = (0..65)
            .map(|index| {
                comp(
                    &format!("R{index}"),
                    "10k",
                    "Resistor_SMD:R_0603",
                    vec![pin("1", 3), pin("2", 2)],
                )
            })
            .collect();
        let got = attribute_currents(&charger_board(branches), &lib_from("wide", CHARGER_TOML));
        assert!(got.cited.is_empty(), "oversized topology must fail closed");
        assert_eq!(got.undetermined.len(), 1);
    }

    #[test]
    fn a_thermistor_in_a_program_network_is_not_assumed_to_be_fixed_resistance() {
        let board = charger_board(vec![comp(
            "RT1",
            "10k",
            "Resistor_SMD:R_0603",
            vec![pin("1", 3), pin("2", 2)],
        )]);
        let got = attribute_currents(&board, &lib_from("thermistor", CHARGER_TOML));
        assert!(got.cited.is_empty());
        assert_eq!(got.undetermined.len(), 1);
    }

    #[test]
    fn an_out_of_domain_program_resistor_is_undetermined_not_clamped() {
        // A 100 ohm resistor would put the equation at 10 A. Normal operation
        // is specified only to 400 mA, while 800 mA is an absolute stress limit;
        // the latter must never become a promised output current.
        let board = charger_board(vec![comp(
            "R10",
            "100R",
            "Resistor_SMD:R_0603",
            vec![pin("1", 3), pin("2", 2)],
        )]);
        let lib = lib_from("out_of_domain", CHARGER_TOML);
        let got = attribute_currents(&board, &lib);
        assert!(
            got.cited.is_empty(),
            "the normal-operating endpoint is a domain boundary, not evidence of saturation: {:?}",
            got.cited
        );
        assert_eq!(got.undetermined.len(), 1);
        assert_eq!(got.undetermined[0].reference, "U1");
    }

    #[test]
    fn simultaneous_regulated_chargers_sum_on_the_shared_input_rail() {
        let board = ExtractedBoard {
            name: "two_chargers".into(),
            nets: vec![
                net(1, "+5V"),
                net(2, "GND"),
                net(3, "PROG_A"),
                net(4, "BAT_A"),
                net(5, "PROG_B"),
                net(6, "BAT_B"),
            ],
            components: vec![
                comp(
                    "U1",
                    "TESTCHARGER",
                    "",
                    vec![pin("1", 1), pin("2", 2), pin("3", 3), pin("4", 4)],
                ),
                comp(
                    "R1",
                    "10k",
                    "Resistor_SMD:R_0603",
                    vec![pin("1", 3), pin("2", 2)],
                ),
                comp(
                    "U2",
                    "TESTCHARGER",
                    "",
                    vec![pin("1", 1), pin("2", 2), pin("3", 5), pin("4", 6)],
                ),
                comp(
                    "R2",
                    "10k",
                    "Resistor_SMD:R_0603",
                    vec![pin("1", 5), pin("2", 2)],
                ),
            ],
        };
        let got = attribute_currents(&board, &lib_from("two_chargers", CHARGER_TOML));
        let (input_current, citation) = got.cited.get("+5V").expect("shared input");
        assert!((*input_current - 0.2).abs() < 1e-12, "{input_current}");
        assert!(
            citation.contains("U1") && citation.contains("U2"),
            "{citation}"
        );
        assert!((got.cited["BAT_A"].0 - 0.1).abs() < 1e-12);
        assert!((got.cited["BAT_B"].0 - 0.1).abs() < 1e-12);
    }

    #[test]
    fn cascaded_regulated_stages_do_not_double_count_the_middle_rail() {
        let board = ExtractedBoard {
            name: "cascaded_chargers".into(),
            nets: vec![
                net(1, "SOURCE"),
                net(2, "GND"),
                net(3, "PROG_A"),
                net(4, "MIDDLE"),
                net(5, "PROG_B"),
                net(6, "SINK"),
            ],
            components: vec![
                comp(
                    "U1",
                    "TESTCHARGER",
                    "",
                    vec![pin("1", 1), pin("2", 2), pin("3", 3), pin("4", 4)],
                ),
                comp(
                    "R1",
                    "10k",
                    "Resistor_SMD:R_0603",
                    vec![pin("1", 3), pin("2", 2)],
                ),
                comp(
                    "U2",
                    "TESTCHARGER",
                    "",
                    vec![pin("1", 4), pin("2", 2), pin("3", 5), pin("4", 6)],
                ),
                comp(
                    "R2",
                    "10k",
                    "Resistor_SMD:R_0603",
                    vec![pin("1", 5), pin("2", 2)],
                ),
            ],
        };
        let got = attribute_currents(&board, &lib_from("cascaded", CHARGER_TOML));
        assert!((got.cited["SOURCE"].0 - 0.1).abs() < 1e-12);
        assert!((got.cited["SINK"].0 - 0.1).abs() < 1e-12);
        assert!(
            (got.cited["MIDDLE"].0 - 0.1).abs() < 1e-12,
            "one 0.1 A through-current must not be counted at both ends: {:?}",
            got.cited["MIDDLE"]
        );
        assert!(got.cited["MIDDLE"].1.contains("U1"));
        assert!(got.cited["MIDDLE"].1.contains("U2"));
    }

    #[test]
    fn a_programmed_load_switch_threshold_is_not_a_fictional_load() {
        // AP22615A: ILIM = 6800 / RLIM is an OCP threshold, not a commanded
        // steady current. A lightly loaded switched rail must not be charged
        // 1 A merely because its protection is set there.
        let board = ExtractedBoard {
            name: "load_switch".into(),
            nets: vec![
                net(1, "+5V"),
                net(2, "GND"),
                net(3, "ISET"),
                net(4, "SWITCHED_5V"),
                net(5, "ENABLE"),
                net(6, "FAULT_N"),
            ],
            components: vec![
                comp(
                    "U1",
                    "AP22615A",
                    "Package_TO_SOT_SMD:TSOT-26",
                    vec![
                        pin("1", 4),
                        pin("2", 2),
                        pin("3", 6),
                        pin("4", 5),
                        pin("5", 3),
                        pin("6", 1),
                    ],
                ),
                comp(
                    "RSET",
                    "6.8k",
                    "Resistor_SMD:R_0603",
                    vec![pin("1", 3), pin("2", 2)],
                ),
            ],
        };

        let got = attribute_currents(&board, &ModelLibrary::builtin());
        assert!(
            got.cited.is_empty(),
            "an OCP setting is not a load: {:?}",
            got.cited
        );
    }

    #[test]
    fn ap22615_minimum_rlim_still_does_not_create_a_steady_load() {
        let board = ExtractedBoard {
            name: "load_switch_max".into(),
            nets: vec![net(1, "+5V"), net(2, "GND"), net(3, "ISET"), net(4, "OUT")],
            components: vec![
                comp(
                    "U1",
                    "AP22615A",
                    "Package_TO_SOT_SMD:TSOT-26",
                    vec![pin("1", 4), pin("2", 2), pin("5", 3), pin("6", 1)],
                ),
                comp(
                    "RSET",
                    "1.94k",
                    "Resistor_SMD:R_0603",
                    vec![pin("1", 3), pin("2", 2)],
                ),
            ],
        };

        let got = attribute_currents(&board, &ModelLibrary::builtin());
        assert!(
            got.cited.is_empty(),
            "an OCP setting is not a load: {:?}",
            got.cited
        );
    }

    #[test]
    fn ltc4020_input_limit_is_not_charged_to_kelvin_or_power_nets() {
        let board = ExtractedBoard {
            name: "ltc4020_program".into(),
            nets: vec![
                net(1, "VIN"),
                net(2, "GND"),
                net(3, "SENSE_TOP"),
                net(4, "SENSE_BOT"),
                net(5, "ILIMIT"),
                net(6, "BAT"),
            ],
            components: vec![
                comp(
                    "U2",
                    "LTC4020",
                    "Package_DFN_QFN:QFN-38",
                    vec![
                        pin("5", 4),
                        pin("6", 3),
                        pin("20", 6),
                        pin("25", 5),
                        pin("36", 1),
                    ],
                ),
                comp(
                    "R8",
                    "7.15k",
                    "Resistor_SMD:R_0603",
                    vec![pin("1", 5), pin("2", 2)],
                ),
                comp(
                    "R49",
                    "0.01",
                    "Resistor_SMD:R_2512",
                    vec![pin("1", 3), pin("2", 1)],
                ),
                comp(
                    "R50",
                    "0.01",
                    "Resistor_SMD:R_2512",
                    vec![pin("1", 4), pin("2", 2)],
                ),
            ],
        };

        let got = attribute_currents(&board, &ModelLibrary::builtin());
        assert!(
            got.cited.is_empty(),
            "a current-limit setting proves neither load current nor Kelvin-stub current: {:?}",
            got.cited
        );
    }

    #[test]
    fn sense_scaled_regulation_requires_exact_equal_declared_shunt_paths() {
        let make_board = |second_shunt: &str, wrong_far: bool, extra_branch: bool| {
            let mut components = vec![
                comp(
                    "U1",
                    "TESTSENSE",
                    "",
                    vec![
                        pin("1", 1),
                        pin("2", 2),
                        pin("3", 3),
                        pin("4", 4),
                        pin("5", 5),
                        pin("6", 6),
                    ],
                ),
                comp(
                    "R1",
                    "10k",
                    "Resistor_SMD:R_0603",
                    vec![pin("1", 3), pin("2", 2)],
                ),
                comp(
                    "R2",
                    "0.01",
                    "Resistor_SMD:R_2512",
                    vec![pin("1", 5), pin("2", if wrong_far { 4 } else { 1 })],
                ),
                comp(
                    "R3",
                    second_shunt,
                    "Resistor_SMD:R_2512",
                    vec![pin("1", 6), pin("2", 2)],
                ),
            ];
            if extra_branch {
                components.push(comp(
                    "R4",
                    "0.001",
                    "Resistor_SMD:R_0603",
                    vec![pin("1", 5), pin("2", 7)],
                ));
            }
            ExtractedBoard {
                name: "sense_regulator".into(),
                nets: vec![
                    net(1, "VIN"),
                    net(2, "GND"),
                    net(3, "PROG"),
                    net(4, "VOUT"),
                    net(5, "KELVIN_A"),
                    net(6, "KELVIN_B"),
                    net(7, "FILTER_NODE"),
                ],
                components,
            }
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
            assert!(
                !valid.cited.contains_key(kelvin),
                "Kelvin/reference net {kelvin} does not carry the regulated branch current"
            );
        }

        for (label, board) in [
            ("mismatched shunts", make_board("0.02", false, false)),
            ("wrong far-side role", make_board("0.01", true, false)),
            ("extra resistive branch", make_board("0.01", false, true)),
        ] {
            let got = attribute_currents(&board, &lib);
            assert!(got.cited.is_empty(), "{label}: {:?}", got.cited);
            assert_eq!(got.undetermined.len(), 1, "{label}");
        }

        let mut multi_terminal_sense = make_board("0.01", false, false);
        multi_terminal_sense.components.push(comp(
            "RV1",
            "10k",
            "Potentiometer_THT:Potentiometer",
            vec![pin("1", 5), pin("2", 1), pin("3", 7)],
        ));
        let got = attribute_currents(&multi_terminal_sense, &lib);
        assert!(got.cited.is_empty());
        assert_eq!(
            got.undetermined.len(),
            1,
            "a multi-terminal device touching a Kelvin net must make the law undetermined"
        );
    }

    #[test]
    fn checked_in_watchy_programs_tp4054_to_one_hundred_milliamps() {
        // This is the actual drag-and-drop example, not a reduced fixture. U3's
        // PROG network contains R3 = 10 kOhm, so the current official TP4054
        // law must attribute 100 mA to both charger rails.
        let text = include_str!("../../../hauksbee-ci/examples/boards/watchy.kicad_pcb");
        let board = ExtractedBoard::from_kicad_pcb(text).expect("Watchy extracts");
        let got = attribute_currents(&board, &ModelLibrary::builtin());
        let charger_rails: Vec<_> = got
            .cited
            .iter()
            .filter(|(_, (_, citation))| citation.contains("U3 (tp4054)"))
            .collect();

        assert_eq!(
            charger_rails.len(),
            2,
            "the input and battery rails must both be attributable: {charger_rails:?}"
        );
        for (rail, (current, citation)) in charger_rails {
            assert!(
                (*current - 0.1).abs() < 1e-12,
                "{rail}: R3=10k should program 100 mA, got {current}"
            );
            assert!(citation.contains("R3") && citation.contains("10000"));
        }
    }

    #[test]
    fn a_regulator_rating_is_capability_not_proof_of_board_load() {
        // An LDO capable of 1 A can feed a 2 mA load. Without a load assertion,
        // its rating cannot become an actual-current citation for ampacity.
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
        let board = ExtractedBoard {
            name: "ldo".into(),
            nets: vec![net(1, "+5V"), net(2, "GND"), net(3, "+3V3")],
            components: vec![comp(
                "U2",
                "TESTLDO",
                String::new().as_str(),
                vec![pin("1", 1), pin("2", 2), pin("3", 3)],
            )],
        };
        let lib = lib_from("plain_ldo", toml_src);
        let got = attribute_currents(&board, &lib);
        assert!(got.undetermined.is_empty());
        assert!(
            got.cited.is_empty(),
            "a capability is not a load: {:?}",
            got.cited
        );
    }

    #[test]
    fn programmed_power_path_uses_explicit_roles_not_name_heuristics() {
        // A 5-pin LDO: source/sink roles are data in current_program. Control
        // roles cannot receive current just because their names look familiar.
        let model = model_from_toml(
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
        );
        let program = model.current_program.as_ref().unwrap();
        let comp = hauksbee_extract::Component {
            reference: "U1".into(),
            value: "LP2985-3.3".into(),
            lib_id: String::new(),
            footprint: String::new(),
            position: None,
            layer: String::new(),
            properties: vec![],
            dnp: false,
            pins: vec![
                pin("1", 10), // in  -> VIN
                pin("2", 11), // gnd -> GND
                pin("3", 12), // en  -> EN
                pin("4", 13), // noise_bypass -> BYP
                pin("5", 14), // out -> +3V3
            ],
        };
        let board = ExtractedBoard {
            name: "t".into(),
            nets: vec![
                Net {
                    id: 10,
                    name: "VIN".into(),
                },
                Net {
                    id: 11,
                    name: "GND".into(),
                },
                Net {
                    id: 12,
                    name: "EN".into(),
                },
                Net {
                    id: 13,
                    name: "BYP".into(),
                },
                Net {
                    id: 14,
                    name: "+3V3".into(),
                },
            ],
            components: vec![comp.clone()],
        };
        let nets = power_nets_of(&board, &comp, &model, program);
        assert_eq!(
            nets,
            vec![10, 14],
            "only explicitly declared rails carry current"
        );
        assert!(!nets.contains(&11), "GND excluded");
        assert!(
            !nets.contains(&12),
            "EN must not be charged the rail current"
        );
        assert!(
            !nets.contains(&13),
            "bypass must not be charged the rail current"
        );
    }

    #[test]
    fn programmed_power_path_refuses_a_missing_declared_role() {
        let component = comp(
            "U1",
            "TESTCHARGER",
            "",
            vec![pin("1", 1), pin("2", 2), pin("3", 3)],
        );
        let lib = lib_from("missing_declared_role", CHARGER_TOML);
        let part = AssemblyState::of(&component).fitted().unwrap();
        let model = resolve(&lib, part).model.unwrap();
        let program = model.current_program.as_ref().unwrap();
        let board = charger_board(Vec::new());
        assert!(
            programmed_power_rails(&board, part, &model, program).is_none(),
            "missing OUT must refuse the entire current path, not retain VIN"
        );
    }

    #[test]
    fn programmed_power_path_refuses_conflicting_repeated_role_pads() {
        let component = comp(
            "U1",
            "TESTCHARGER",
            "",
            vec![
                pin("1", 1),
                pin("1", 5),
                pin("2", 2),
                pin("3", 3),
                pin("4", 4),
            ],
        );
        let lib = lib_from("conflicting_role_pads", CHARGER_TOML);
        let part = AssemblyState::of(&component).fitted().unwrap();
        let model = resolve(&lib, part).model.unwrap();
        let program = model.current_program.as_ref().unwrap();
        let board = charger_board(Vec::new());
        assert!(
            programmed_power_rails(&board, part, &model, program).is_none(),
            "one logical input pad cannot be on two non-zero nets"
        );
    }
}
