// The timing evidence a co-simulated report carries when one solver chunk
// refused a PWL replay, shared by the export and the interactive-coverage tests.

export const TIMING_DIAGNOSIS = 'PWL replay refused on net /CLK: transition budget exceeded'

/** One cycle-exact MCU, the refusal, and the fallback window it forced. */
export const timingEvidence = {
  timing_coverage: [{
    mcu_ref: 'U1', backend: 'simavr:atmega328p', cycle_exact: true,
    timestamp_precision_s: 0.000001, minimum_guaranteed_pulse_s: 0.000002,
    chunk_s: 0.001,
  }],
  timing_refusals: [TIMING_DIAGNOSIS],
  fallback_windows: [{
    start_s: 0.001, end_s: 0.002, method: 'backward-euler' as const,
    fidelity_note: 'first-order and numerically dissipative', error_estimate_v: 0.012,
  }],
}

/** The typed refusal that names the same diagnosis. */
export const timingRefusal = {
  claim: 'timing-sensitive firmware and electrical conclusions',
  missing_prerequisite: TIMING_DIAGNOSIS,
  valid_partial_conclusions: ['Static board analysis remains valid.'],
  next_action: 'reduce transitions per solver chunk, then rerun',
}
