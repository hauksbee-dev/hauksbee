//! Typed failures exposed by the solver's public APIs.
//!
//! The taxonomy separates invalid input, deliberate soundness refusals,
//! numerical non-convergence, singular structure, behavioral-expression
//! faults, and internal invariant failures. Each variant retains its historical
//! display message while structured fields let callers select recovery,
//! diagnostics, and exit behavior without parsing unstable text.

/// The part of the solve in which a numerical or behavioral failure occurred.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SolvePhase {
    /// DC operating-point search.
    Dc,
    /// Time-domain integration.
    Transient,
    /// Small-signal AC analysis.
    Ac,
    /// Partitioned-island execution.
    Partitioned,
    /// Torn-rail balance.
    RailBalance,
}

/// A typed solver failure.
///
/// `message` is deliberately retained on every variant. It is the historical
/// public text of the failure and therefore remains byte-for-byte stable while
/// the other fields give callers a non-textual classification surface.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum SolveError {
    /// Newton, transient integration, or a partition balance exhausted its
    /// convergence budget.
    #[error("{message}")]
    NonConvergence {
        /// Historical public error text.
        message: String,
        /// Solver phase that did not converge.
        phase: SolvePhase,
        /// Simulation time, when the failure occurred during a march.
        time: Option<f64>,
        /// Minimum or attempted step size, when relevant.
        dt: Option<f64>,
        /// Iteration/pass count, when reported by the old message.
        iterations: Option<usize>,
        /// Existing device/net blame clause, without presentation brackets.
        blame: Option<String>,
    },

    /// A matrix could not be factored, or a topology/ownership structure was
    /// inconsistent with the solver's assumptions.
    #[error("{message}")]
    Singular {
        /// Historical public error text.
        message: String,
        /// Offending matrix unknown, when known.
        unknown: Option<usize>,
        /// Offending net/node, when known.
        net: Option<String>,
    },

    /// A deck, analysis specification, probe, or value was invalid.
    #[error("{message}")]
    InvalidInput {
        /// Historical public error text.
        message: String,
    },

    /// The solver deliberately declined an analysis whose soundness it could
    /// not establish. This is an honesty outcome, not a numerical crash.
    #[error("{message}")]
    Refused {
        /// Historical public error text.
        message: String,
    },

    /// A named behavioral source expression errored or became non-finite.
    #[error("{message}")]
    Behavioral {
        /// Historical public error text.
        message: String,
        /// Name of the behavioral device that faulted.
        device: String,
        /// Solver phase in which the expression fault surfaced.
        phase: SolvePhase,
    },

    /// A solver invariant or result-evidence construction failed.
    #[error("{message}")]
    Internal {
        /// Historical public error text.
        message: String,
    },
}

impl SolveError {
    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self::InvalidInput {
            message: message.into(),
        }
    }

    pub(crate) fn refused(message: impl Into<String>) -> Self {
        Self::Refused {
            message: message.into(),
        }
    }

    pub(crate) fn internal(message: impl Into<String>) -> Self {
        Self::Internal {
            message: message.into(),
        }
    }

    pub(crate) fn singular(message: impl Into<String>) -> Self {
        Self::Singular {
            message: message.into(),
            unknown: None,
            net: None,
        }
    }

    pub(crate) fn behavioral(
        message: impl Into<String>,
        device: impl Into<String>,
        phase: SolvePhase,
    ) -> Self {
        Self::Behavioral {
            message: message.into(),
            device: device.into(),
            phase,
        }
    }

    /// Replace the displayed text while retaining the typed class and all
    /// structured fields. Orchestration layers use this to add group/capture
    /// context without collapsing a refusal or non-convergence into a string.
    pub(crate) fn with_message(self, message: impl Into<String>) -> Self {
        let message = message.into();
        match self {
            Self::NonConvergence {
                phase,
                time,
                dt,
                iterations,
                blame,
                ..
            } => Self::NonConvergence {
                message,
                phase,
                time,
                dt,
                iterations,
                blame,
            },
            Self::Singular { unknown, net, .. } => Self::Singular {
                message,
                unknown,
                net,
            },
            Self::InvalidInput { .. } => Self::InvalidInput { message },
            Self::Refused { .. } => Self::Refused { message },
            Self::Behavioral { device, phase, .. } => Self::Behavioral {
                message,
                device,
                phase,
            },
            Self::Internal { .. } => Self::Internal { message },
        }
    }

    /// True for a failure that says the DC operating point itself was not
    /// reachable. Power-ramp retry logic uses this instead of parsing `Display`.
    pub(crate) fn is_dc_failure(&self) -> bool {
        matches!(
            self,
            Self::NonConvergence {
                phase: SolvePhase::Dc,
                ..
            } | Self::Behavioral {
                phase: SolvePhase::Dc,
                ..
            }
        )
    }
}

/// Result type returned by solver APIs.
pub type SolveResult<T> = Result<T, SolveError>;

pub(crate) fn behavioral_device(fault: &str) -> String {
    fault
        .strip_prefix("behavioral source `")
        .and_then(|rest| rest.split_once("`: ").map(|(device, _)| device))
        .unwrap_or("<unknown>")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_convergence_display_is_verbatim() {
        let error = SolveError::NonConvergence {
            message: "Newton failed at t=1 even at dt_min=0.1 [node out]".into(),
            phase: SolvePhase::Transient,
            time: Some(1.0),
            dt: Some(0.1),
            iterations: None,
            blame: Some("node out".into()),
        };
        assert_eq!(
            error.to_string(),
            "Newton failed at t=1 even at dt_min=0.1 [node out]"
        );
    }

    #[test]
    fn singular_display_is_verbatim() {
        let error = SolveError::Singular {
            message: "AC system singular at w=6.2832 rad/s (f=1.0000 Hz)".into(),
            unknown: None,
            net: None,
        };
        assert_eq!(
            error.to_string(),
            "AC system singular at w=6.2832 rad/s (f=1.0000 Hz)"
        );
    }

    #[test]
    fn invalid_input_display_is_verbatim() {
        let error = SolveError::InvalidInput {
            message: "points must be >= 1".into(),
        };
        assert_eq!(error.to_string(), "points must be >= 1");
    }

    #[test]
    fn refused_display_is_verbatim() {
        let error = SolveError::Refused {
            message: "staged execution refused: the decomposition is unsound".into(),
        };
        assert_eq!(
            error.to_string(),
            "staged execution refused: the decomposition is unsound"
        );
    }

    #[test]
    fn behavioral_display_is_verbatim() {
        let error = SolveError::Behavioral {
            message: "AC linearization refused: behavioral source `B1`: ln domain error".into(),
            device: "B1".into(),
            phase: SolvePhase::Ac,
        };
        assert_eq!(
            error.to_string(),
            "AC linearization refused: behavioral source `B1`: ln domain error"
        );
    }

    #[test]
    fn internal_display_is_verbatim() {
        let error = SolveError::Internal {
            message: "invalid transient result window: end precedes start".into(),
        };
        assert_eq!(
            error.to_string(),
            "invalid transient result window: end precedes start"
        );
    }
}
