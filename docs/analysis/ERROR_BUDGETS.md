# Numerical error budgets

Hauksbee qualifies every production numeric result with a machine-readable
`error_budget`. It reports what the solver used and what it measured. It does
not convert an algorithm's formal order into a guessed percentage accuracy.

The contract follows the verification discipline in
[NASA-STD-7009B](https://standards.nasa.gov/standard/nasa/nasa-std-7009):
record numerical-error sources and verification status, identify unverified or
unvalidated aspects, and make a failed analysis unmistakable rather than
presenting an unqualified definitive number.

## Fields and units

- `tolerance` contains the actual solver settings: relative tolerance,
  absolute voltage tolerance (`vntol`, V), absolute current tolerance
  (`abstol`, A), and charge tolerance (`chgtol`, C). These are acceptance
  settings, not a bound on output error.
- `methods` partitions each solved transient interval by the integration path
  that produced it. Primary, fallback, and subdivided windows do not overlap.
  The method name is provenance; it is not an empirical accuracy claim.
- `residual.max_abs` is the largest measured final node-KCL residual among the
  reported solves (A), with the node or unknown in `at`. It is a convergence
  diagnostic, not voltage error and not physical-model uncertainty. If the
  active path cannot measure it, the field is absent and the UI says
  **unmeasured**. Absence never means zero.
  A post-solve forced-voltage override also suppresses the residual because it
  changes reported values after the equation system was solved.
- `failed_windows` are half-open intervals `[start_s, end_s)` with no valid
  transient solution. Values held across those spans are recovery state, not
  measurements. Consumers must refuse claims that depend on them.
- `event_time_error_s` is a worst-case timestamp quantization bound. In co-sim
  it is the chunk duration: an event observed at a chunk boundary can be late
  by no more than that value. It says nothing about amplitude accuracy.
- `model_uncertainty` contains only intervals a model producer can support with
  a named basis. An empty list means no bounded physical-model uncertainty was
  established; it does not mean the model is exact.

## Where it appears

Solver `SimOutput` carries the budget for operating-point, transient, DC-sweep,
and AC runs. Engine AC and thermal evidence, firmware co-sim JSON, the web
co-sim section, and numeric CI assertions propagate the same typed object.
Human, JUnit, and GitHub CI output render its settings, measured residual, and
invalid-window count from that object rather than paraphrasing a second record.

## Refusal boundary

A tolerance setting alone never proves physical accuracy. A result is usable
only over solved windows, and only for claims supported by its models and input
artifacts. Hauksbee therefore refuses or marks invalid any assertion whose
observation interval intersects a failed solve window or whose evidence is
otherwise undermined. Missing residuals and missing model intervals remain
visible unknowns; they are never filled with zero, `null`, or an estimated
percentage.
