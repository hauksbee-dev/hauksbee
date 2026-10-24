//! The DC resistance a programming pin sees to ground, read off the board.
//!
//! A linear charger's PROG resistor (or any resistor-programmed limit) is a
//! network, not always one part: parallel trims, a series jumper, a solder
//! link. [`program_resistance_to_ground`] walks the populated two-terminal parts
//! out from the pin and solves the nodal conductance system, and returns `None`
//! rather than guess when it meets a part it cannot read. The static ampacity
//! check and the charger's simulated current limit both call it, so the current
//! the check cites and the current the solve delivers come from one reading.

use hauksbee_extract::assembly::AssemblyState;
use hauksbee_extract::Component;
use std::collections::{hash_map::Entry, BTreeSet, HashMap, HashSet, VecDeque};

pub use hauksbee_models::netclass::is_ground as is_ground_net;

/// How a two-terminal part on the path from a programming pin to ground behaves.
pub enum Series {
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
#[must_use]
pub fn electrical_terminal_nets(component: &Component) -> Option<(i64, i64)> {
    let (first, second) = electrical_terminal_net_options(component).ok()??;
    let first = first?;
    let second = second?;
    (first != second).then_some((first, second))
}

/// Whether `component` is an inert marker on a programming net.
///
/// A test point, a single-pad part or any other non-resistor with at most one
/// pad on a net joins the net to nothing, so no programming network reads
/// through it. A resistor-like part with at most one connected pad (a dangling
/// leg, whether or not its second pad is listed) is NOT such a marker: the
/// programming resistor was drawn but not connected, and the walk must say so
/// rather than read the pin as floating.
#[must_use]
pub fn is_inert_marker(component: &Component) -> bool {
    let connected = component.pins.iter().filter(|pin| pin.net.is_some());
    let connected_pads = if component
        .pins
        .iter()
        .any(|pin| pin.number.trim().is_empty())
    {
        // Unnumbered pads cannot be told apart from one pad on two layers.
        connected.count()
    } else {
        connected
            .map(|pin| pin.number.trim())
            .collect::<BTreeSet<_>>()
            .len()
    };
    if connected_pads > 1 {
        return false;
    }
    hauksbee_extract::is_test_point_or_hole(component) || !is_resistor_component(component)
}

/// A two-terminal part's terminal nets; `None` marks an unconnected terminal.
type TerminalNets = (Option<i64>, Option<i64>);

/// The two logical terminal nets, retaining a missing-net state so a topology
/// reader can distinguish "not a two-terminal part" from "a two-terminal part
/// whose connection is incomplete". Contradictory duplicate physical pad
/// records are an error; callers must not first-win them.
fn electrical_terminal_net_options(component: &Component) -> Result<Option<TerminalNets>, ()> {
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

fn component_hint(component: &Component) -> String {
    format!(
        "{} {} {}",
        component.reference, component.footprint, component.lib_id
    )
    .to_ascii_lowercase()
}

/// Conventional single-part designator grammar: one class letter followed by
/// a digit (or `KiCad`'s unannotated `?`). This keeps R10/C10 useful without
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

fn is_resistor_component(component: &Component) -> bool {
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

fn is_capacitor_component(component: &Component) -> bool {
    let hint = component_hint(component);
    let reference = component.reference.trim().to_ascii_uppercase();
    let metadata_capacitor =
        hint.contains("capacitor") || hint.contains("cap_") || hint.contains("capc");
    conventional_designator(&reference, 'C')
        || (metadata_may_classify(&reference) && metadata_capacitor)
}

#[must_use]
pub fn classify_series(comp: &Component) -> Series {
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
        Some(p) if matches!(p.unit.as_deref(), Some("F" | "H")) => Series::Unknown,
        Some(p) if p.si == 0.0 => Series::Closed,
        Some(p) if p.si > 0.0 => Series::Ohms(p.si),
        _ => Series::Unknown,
    }
}

/// Why a programming network could not be read off the board.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProgramUnread {
    /// A part on the network cannot be read as a plain resistor or link: its
    /// reference and the plain reason (a refused duplicate designator, a
    /// reference and footprint that do not say "resistor", a stray pin).
    Part { reference: String, why: String },
    /// No populated resistor path reaches ground from the programming pin.
    NoPath,
    /// The network is larger than the bounded reader solves, or its nodal
    /// system has no unique answer.
    TooComplex,
    /// The programming pin's net is not a net of this board.
    UnknownNet,
}

impl ProgramUnread {
    /// A part whose value is not a readable resistance: the one wording every
    /// check uses when a programming resistor's value does not parse.
    #[must_use]
    pub fn unreadable_value(reference: &str, value: &str) -> Self {
        Self::Part {
            reference: reference.to_string(),
            why: format!(
                "has the value '{value}', which is not a readable resistance (a number with an \
                 optional k or M multiplier); state it as a single number such as 10k"
            ),
        }
    }

    /// One plain clause for a report: what stopped the reading.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Part { reference, why } => format!("{reference} {why}"),
            Self::NoPath => "no populated resistor path to ground".to_string(),
            Self::TooComplex => {
                "the network is too large or ambiguous to solve for one resistance".to_string()
            }
            Self::UnknownNet => "the programming pin's net is not on the board".to_string(),
        }
    }
}

/// What a programming pin's network to ground reads as.
#[derive(Debug, Clone, PartialEq)]
pub enum ProgramReading {
    /// An equivalent resistance in ohms, and the parts that carry it.
    Ohms(f64, Vec<String>),
    /// The pin is joined to ground by closed links alone (a `0R`, a closed
    /// solder jumper) or sits on a ground net itself: zero ohms, the extreme
    /// of a too-small programming resistor. The closed links are listed.
    Shorted(Vec<String>),
    /// The resistance cannot be read; a check that rests on it must say so.
    Unread(ProgramUnread),
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
// Elimination reads the pivot row while writing another row of the same matrix.
#[allow(clippy::needless_range_loop)]
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

/// Union-find root of `index`, compressing the path as it goes.
fn root(parents: &mut [usize], index: usize) -> usize {
    if parents[index] != index {
        parents[index] = root(parents, parents[index]);
    }
    parents[index]
}

/// Union-find merge of the sets holding `left` and `right`.
fn unite(parents: &mut [usize], left: usize, right: usize) {
    let left = root(parents, left);
    let right = root(parents, right);
    if left != right {
        parents[right] = left;
    }
}

/// The resistive/closed edges of the programming network around `start`.
///
/// The walk goes at most `max_hops` nets deep. `Err` says why it met an
/// unreadable part, exceeded its size bounds, or never reached ground.
fn program_network_edges(
    components: &[Component],
    is_ground: &dyn Fn(i64) -> Option<bool>,
    start: i64,
    max_hops: usize,
    excluded_component: Option<usize>,
) -> Result<Vec<ResistanceEdge>, ProgramUnread> {
    const MAX_PROGRAM_NODES: usize = 32;
    const MAX_PROGRAM_EDGES: usize = 64;

    let mut queue = VecDeque::from([(start, 0usize)]);
    let mut depth_by_net = HashMap::from([(start, 0usize)]);
    let mut discovered_nets = HashSet::from([start]);
    let mut consumed_components = HashSet::new();
    let mut edges = Vec::new();
    let mut reached_ground = false;

    while let Some((net_id, depth)) = queue.pop_front() {
        for (component_index, component) in components.iter().enumerate() {
            // Only a DNP part is skipped as electrically absent; an
            // identity-refused record must instead poison the walk below
            // (classify_series says Unknown), never be silently dropped.
            if matches!(AssemblyState::of(component), AssemblyState::DnpAbsent(_))
                || Some(component_index) == excluded_component
            {
                continue;
            }
            if is_inert_marker(component) {
                // A test point or single-pad marker on the net joins it to
                // nothing, so it cannot change the equivalent. A resistor with
                // a dangling leg is not one: it falls through to the abstention.
                continue;
            }
            let Ok(Some(terminals)) = electrical_terminal_net_options(component) else {
                // A multi-/single-terminal (or contradictory) device on the
                // programming net can change its DC equivalent (a trim pot is the
                // common case). Ignore only the programmed IC itself, passed above.
                if component.pins.iter().any(|pin| pin.net == Some(net_id)) {
                    return Err(unread_part(
                        component,
                        if is_resistor_component(component) && component.pins.len() <= 1 {
                            "has an unconnected second terminal"
                        } else {
                            "is not a two-terminal part (or its pads disagree on their nets)"
                        },
                    ));
                }
                continue;
            };
            let far = match terminals {
                (Some(first), Some(second)) if first == net_id && second != net_id => second,
                (Some(first), Some(second)) if second == net_id && first != net_id => first,
                (Some(first), None) | (None, Some(first)) if first == net_id => {
                    return Err(unread_part(component, "has an unconnected second terminal"));
                }
                _ => continue,
            };

            let (ohms, closed) = match classify_series(component) {
                Series::Ohms(ohms) if ohms.is_finite() && ohms > 0.0 => (ohms, false),
                Series::Closed => (0.0, true),
                Series::Open => continue,
                Series::Ohms(_) | Series::Unknown => return Err(unread_series(component)),
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
                    return Err(ProgramUnread::TooComplex);
                }
            }
            if discovered_nets.insert(far) && discovered_nets.len() > MAX_PROGRAM_NODES {
                return Err(ProgramUnread::TooComplex);
            }

            if is_ground(far).ok_or(ProgramUnread::UnknownNet)? {
                reached_ground = true;
                continue;
            }
            if let Entry::Vacant(entry) = depth_by_net.entry(far) {
                if depth >= max_hops {
                    return Err(ProgramUnread::TooComplex);
                }
                entry.insert(depth + 1);
                queue.push_back((far, depth + 1));
            }
        }
    }
    if reached_ground {
        Ok(edges)
    } else {
        Err(ProgramUnread::NoPath)
    }
}

/// A part the walk cannot read, with the plain reason.
fn unread_part(component: &Component, why: &str) -> ProgramUnread {
    ProgramUnread::Part {
        reference: component.reference.clone(),
        why: why.to_string(),
    }
}

/// Why [`classify_series`] could not read a part on the network: a refused
/// identity (a duplicate designator the board cannot resolve) first, else a
/// reference, value and footprint that do not say "resistor".
fn unread_series(component: &Component) -> ProgramUnread {
    let why = match AssemblyState::of(component) {
        AssemblyState::IdentityUnknown(refusal) => format!(
            "cannot be trusted as the programming resistor ({}); rename the duplicate so it is \
             one part",
            refusal.reason()
        ),
        _ if hauksbee_models::value::parse_value(&component.value).is_none() => {
            return ProgramUnread::unreadable_value(&component.reference, &component.value);
        }
        _ => format!(
            "(value '{}', footprint '{}') is not identified as a plain resistor by its \
             reference designator, so its value is not read as the programming resistance; use \
             a conventional designator such as R1",
            component.value, component.footprint
        ),
    };
    ProgramUnread::Part {
        reference: component.reference.clone(),
        why,
    }
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
pub fn program_resistance_to_ground(
    components: &[Component],
    is_ground: &dyn Fn(i64) -> Option<bool>,
    start: i64,
    max_hops: usize,
    excluded_component: Option<usize>,
) -> Option<(f64, Vec<String>)> {
    match program_network_reading(components, is_ground, start, max_hops, excluded_component) {
        ProgramReading::Ohms(ohms, path) => Some((ohms, path)),
        ProgramReading::Shorted(_) | ProgramReading::Unread(_) => None,
    }
}

/// [`program_resistance_to_ground`], keeping the reason it has no resistance.
///
/// A pin shorted to ground reads as zero ohms ([`ProgramReading::Shorted`]),
/// the extreme of a too-small programming resistor, and anything the walk
/// could not read says which part or limit stopped it, so a check that rests
/// on the resistance can abstain by name instead of going silent.
#[must_use]
pub fn program_network_reading(
    components: &[Component],
    is_ground: &dyn Fn(i64) -> Option<bool>,
    start: i64,
    max_hops: usize,
    excluded_component: Option<usize>,
) -> ProgramReading {
    let Some(start_is_ground) = is_ground(start) else {
        return ProgramReading::Unread(ProgramUnread::UnknownNet);
    };
    if start_is_ground {
        return ProgramReading::Shorted(Vec::new());
    }

    let edges =
        match program_network_edges(components, is_ground, start, max_hops, excluded_component) {
            Ok(edges) => edges,
            Err(unread) => return ProgramReading::Unread(unread),
        };
    solve_program_network(&edges, is_ground, start)
}

fn closed_link_references(edges: &[ResistanceEdge]) -> Vec<String> {
    let links: BTreeSet<String> = edges
        .iter()
        .filter(|edge| edge.closed)
        .map(|edge| edge.reference.clone())
        .collect();
    links.into_iter().collect()
}

fn solve_program_network(
    edges: &[ResistanceEdge],
    is_ground: &dyn Fn(i64) -> Option<bool>,
    start: i64,
) -> ProgramReading {
    let unsolved = || ProgramReading::Unread(ProgramUnread::TooComplex);

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
    for edge in edges {
        if edge.closed {
            unite(
                &mut parents,
                node_index[&edge.first],
                node_index[&edge.second],
            );
        }
    }

    let start_root = root(&mut parents, node_index[&start]);
    let ground_roots: HashSet<usize> = node_ids
        .iter()
        .enumerate()
        .filter(|(_, id)| is_ground(**id) == Some(true))
        .map(|(index, _)| root(&mut parents, index))
        .collect();
    if ground_roots.contains(&start_root) {
        // Only closed links join the pin to ground: zero ohms.
        return ProgramReading::Shorted(closed_link_references(edges));
    }

    let variable_roots: BTreeSet<usize> = (0..node_ids.len())
        .map(|index| root(&mut parents, index))
        .filter(|component_root| !ground_roots.contains(component_root))
        .collect();
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
            return unsolved();
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
    let Some(&start_index) = variable_index.get(&start_root) else {
        return unsolved();
    };
    injection[start_index] = 1.0;
    let Some(voltages) = solve_linear(conductance, injection) else {
        return unsolved();
    };
    let equivalent_ohms = voltages[start_index];
    if !equivalent_ohms.is_finite() || equivalent_ohms <= 0.0 {
        return unsolved();
    }

    let voltage = |component_root: usize| -> f64 {
        variable_index
            .get(&component_root)
            .map_or(0.0, |&index| voltages[index])
    };
    let mut references = BTreeSet::new();
    for edge in edges {
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
    ProgramReading::Ohms(equivalent_ohms, references.into_iter().collect())
}
