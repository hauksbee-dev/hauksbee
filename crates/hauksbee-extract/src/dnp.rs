//! What to do about Do-Not-Populate parts, decided explicitly and reported.
//!
//! DNP means the assembler does not place the part. In practice it gets used
//! for two opposite things:
//!
//! 1. "Not on the assembly BOM, but it will be there." A socketed module (an
//!    Arduino Nano, an ESP32 carrier) bought separately and plugged into
//!    headers; a footprint stuffed by hand later; a part fitted at rework.
//!    Analysing the board without it answers a question nobody asked.
//! 2. "This link is deliberately open." A 0R bridge between two ground
//!    planes, a solder jumper selecting a mode, a config strap. Fitting one of
//!    these merges nets that the designer split on purpose, and the tool would
//!    then report one ground plane on a board that has two.
//!
//! Because most DNP footprints eventually get placed, the default is to fit
//! them, with case 2 carved out: a near-zero-ohm link is a topology decision,
//! so it stays open unless asked for by name. Every run reports exactly which
//! parts were fitted and which were left open, so the choice is never silent.

use crate::{Component, ExtractedBoard};

/// Resistance at or below which a two-terminal part counts as a link rather
/// than a component: fitting it merges the nets on either side instead of
/// putting a meaningful impedance between them.
const LINK_OHMS: f64 = 0.5;

/// How to treat DNP parts that were not named individually.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DnpPolicy {
    /// Fit DNP parts, except near-zero-ohm links. The default: most DNP
    /// footprints are placed eventually, and the exception is the one case
    /// where fitting silently rewrites the board's topology.
    #[default]
    FitExceptLinks,
    /// Fit every DNP part, links included.
    FitAll,
    /// Leave every DNP part open, matching what the board file says a fab
    /// house would build.
    Honour,
}

impl DnpPolicy {
    /// The one-line description a report prints so the default is never a
    /// surprise.
    pub fn describe(self) -> &'static str {
        match self {
            DnpPolicy::FitExceptLinks => {
                "DNP parts are simulated as fitted (they are usually placed eventually), \
                 except near-zero-ohm links, which stay open because fitting one merges \
                 the nets it bridges"
            }
            DnpPolicy::FitAll => "every DNP part is simulated as fitted, including links",
            DnpPolicy::Honour => {
                "DNP parts are left out of the simulation, matching the board file"
            }
        }
    }
}

/// Why one DNP part ended up fitted or open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DnpReason {
    /// The policy fits DNP parts and this one is not a link.
    Policy,
    /// Named by the caller (`--fit R7`, `fit = ["R7"]`).
    NamedFit,
    /// Named by the caller (`--no-fit R7`, `no_fit = ["R7"]`).
    NamedOpen,
    /// A near-zero-ohm link under the default policy: fitting it would merge
    /// the nets on either side, which is a topology decision, not an omission.
    ZeroOhmLink,
    /// The policy honours DNP.
    HonouredPolicy,
}

impl DnpReason {
    pub fn describe(&self) -> &'static str {
        match self {
            DnpReason::Policy => "DNP, fitted by default",
            DnpReason::NamedFit => "DNP, fitted because you asked for it",
            DnpReason::NamedOpen => "DNP, left open because you asked for it",
            DnpReason::ZeroOhmLink => "DNP link (near 0 ohm), left open: fitting it merges nets",
            DnpReason::HonouredPolicy => "DNP, left open",
        }
    }
}

/// One DNP part and what was decided about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DnpPart {
    pub reference: String,
    pub value: String,
    pub reason: DnpReason,
}

/// What a policy did to a board's DNP parts, for reporting.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DnpDecision {
    pub policy_line: String,
    pub fitted: Vec<DnpPart>,
    pub left_open: Vec<DnpPart>,
}

impl DnpDecision {
    /// True when the board had no DNP parts at all, so there is nothing to say.
    pub fn is_empty(&self) -> bool {
        self.fitted.is_empty() && self.left_open.is_empty()
    }

    /// The lines a human-facing report prints. Empty when the board has no DNP
    /// parts, so a board without any never mentions the subject.
    pub fn lines(&self) -> Vec<String> {
        if self.is_empty() {
            return Vec::new();
        }
        let mut out = vec![format!("do-not-populate: {}", self.policy_line)];
        for p in &self.fitted {
            out.push(format!(
                "  fitted:    {} ({}), {}",
                p.reference,
                p.value,
                p.reason.describe()
            ));
        }
        for p in &self.left_open {
            out.push(format!(
                "  left open: {} ({}), {}",
                p.reference,
                p.value,
                p.reason.describe()
            ));
        }
        out
    }
}

/// A two-terminal part whose value is a near-zero resistance, or a footprint
/// that only exists to bridge copper. Fitting one merges the nets it sits
/// between, so under the default policy it stays open.
pub fn is_zero_ohm_link(comp: &Component) -> bool {
    if comp.pins.len() > 2 {
        return false;
    }
    let value = comp.value.trim();
    let lower = value.to_ascii_lowercase();
    // Named links carry no numeric value: "JUMPER", "SOLDER_BRIDGE", "NET_TIE".
    if lower.contains("jumper")
        || lower.contains("solderbridge")
        || lower.contains("solder_bridge")
        || lower.contains("net_tie")
        || lower.contains("nettie")
    {
        return true;
    }
    // Ferrite beads are a few milliohms at DC and are used as rail splits for
    // exactly the same reason a 0R is.
    let footprint = comp.footprint.to_ascii_lowercase();
    if lower.starts_with("fb") || footprint.contains("ferrite") || lower.contains("ferrite") {
        return true;
    }
    // A resistance at or under the link threshold: "0", "0R", "0.0", "R010".
    match hauksbee_models::value::parse_value(value) {
        Some(v) => {
            let is_ohms = v.unit.as_deref().is_none_or(|u| {
                let u = u.to_ascii_lowercase();
                u == "r" || u == "ohm" || u == "ohms" || u == "\u{3a9}"
            });
            is_ohms && v.si <= LINK_OHMS
        }
        None => false,
    }
}

impl ExtractedBoard {
    /// Decide which DNP parts to simulate, clear the flag on those, and return
    /// the record of what was done so every surface can report it.
    ///
    /// `fit` and `no_fit` name individual references and always win over the
    /// policy. An unknown reference in either list is an error: a typo must
    /// fail loudly rather than quietly change nothing.
    pub fn apply_dnp_policy(
        &mut self,
        policy: DnpPolicy,
        fit: &[String],
        no_fit: &[String],
    ) -> Result<DnpDecision, crate::ExtractError> {
        let unknown: Vec<&str> = fit
            .iter()
            .chain(no_fit.iter())
            .filter(|r| !self.components.iter().any(|c| &c.reference == *r))
            .map(|r| r.as_str())
            .collect();
        if !unknown.is_empty() {
            return Err(crate::ExtractError::UnknownReference(format!(
                "unknown reference(s): {}. Check the reference designators against the board.",
                unknown.join(", ")
            )));
        }
        if let Some(clash) = fit.iter().find(|r| no_fit.contains(r)) {
            return Err(crate::ExtractError::UnknownReference(format!(
                "{clash} is named as both fitted and left open; pick one"
            )));
        }

        let mut decision = DnpDecision {
            policy_line: policy.describe().to_string(),
            ..Default::default()
        };
        for comp in &mut self.components {
            if !comp.dnp {
                continue;
            }
            let reason = if fit.contains(&comp.reference) {
                DnpReason::NamedFit
            } else if no_fit.contains(&comp.reference) {
                DnpReason::NamedOpen
            } else {
                match policy {
                    DnpPolicy::Honour => DnpReason::HonouredPolicy,
                    DnpPolicy::FitAll => DnpReason::Policy,
                    DnpPolicy::FitExceptLinks => {
                        if is_zero_ohm_link(comp) {
                            DnpReason::ZeroOhmLink
                        } else {
                            DnpReason::Policy
                        }
                    }
                }
            };
            let part = DnpPart {
                reference: comp.reference.clone(),
                value: comp.value.clone(),
                reason: reason.clone(),
            };
            match reason {
                DnpReason::Policy | DnpReason::NamedFit => {
                    comp.dnp = false;
                    decision.fitted.push(part);
                }
                _ => decision.left_open.push(part),
            }
        }
        Ok(decision)
    }
}
