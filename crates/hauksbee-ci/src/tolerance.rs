//! Component-tolerance ensembles: Monte-Carlo sampling and deterministic
//! corner enumeration over the spec's `[[tolerance]]` rules.
//! Long-form how-and-why: docs/how-and-why/hauksbee-ci/tolerance.md.
//!
//! Real analog circuits are built from ±1% / ±5% / ±10% parts. A board that
//! only meets its assertions at *nominal* component values is a latent defect:
//! some fraction of assembled units will land outside the window. This module
//! turns that into a CI property: the same assertion set is replayed across an
//! *ensemble* of component-value samples, and passes only if it holds on every
//! member.
//!
//! Two honesty rules govern everything here:
//!
//! 1. **Sampled coverage is not proof.** A Monte-Carlo ensemble that passes
//!    24/24 seeds is statistical evidence, not a worst-case bound. The report
//!    wording says so. Corner mode (`mode = "corners"`) enumerates every
//!    all-min/all-max combination deterministically, which bounds the worst
//!    case *only for monotonic responses*, also stated in the report.
//! 2. **Reproducibility is doctrine.** Every sampled value is a pure function
//!    of `(spec, seed, component reference)`, nothing depends on iteration
//!    order or on how many other components are toleranced. A failing seed can
//!    therefore be re-run in isolation (`hauksbee-ci run spec.toml --seed N`)
//!    and produces byte-identical values. The tolerance stream is
//!    domain-separated (`"tol:" + ref`) from the net-fuzz stream, so adding a
//!    tolerance never changes which fuzz levels seed N straps.
//!
//! Seed 0 is always the nominal baseline (all components at nominal, matching
//! fuzz's all-low seed 0), so "nominal passes but the ensemble fails" is
//! visible inside a single run.

use crate::error::SpecError;
use crate::spec::Spec;
use hauksbee_extract::ExtractedBoard;

/// Full-factorial corner enumeration is capped at 2^CORNER_CAP runs. Above
/// this many toleranced components, corner mode refuses and points at
/// Monte-Carlo instead, silently truncating the corner set would fake the
/// bounded claim the mode exists to make.
pub const CORNER_CAP: usize = 10;

/// How many interior Latin-hypercube probes corner mode runs on top of its
/// `2^n` corners, for `n` toleranced components.
///
/// Corner mode's bounded-worst-case claim rests on the response being monotonic
/// in every toleranced value: only then does an extreme of the inputs produce an
/// extreme of the output. A resonance, a comparator threshold crossed mid-range,
/// or a regulator that drops out at an interior load all break that, and the
/// corners then bound nothing while still reporting green. Enumerating the
/// interior is not an option (it is continuous), so this samples it: a stratified
/// Latin-hypercube design, which by construction puts one probe in every
/// per-component stratum and is the cheapest design that cannot miss a whole
/// region of one component's range.
///
/// The count scales with the dimension and then flattens, so the check costs a
/// bounded handful of extra sims rather than a multiple of the corner set: 4
/// probes for one component, 6 for two, 8 from three up. It buys detection, never
/// proof, which is why a clean interior sweep narrows the disclosure instead of
/// removing it.
pub fn interior_probe_count(n: usize) -> usize {
    (2 * n + 2).min(8)
}

/// How a component's per-seed value deviation is distributed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Distribution {
    /// Uniform over [-tol, +tol]. The default: it assumes nothing about the
    /// vendor's binning and stresses the tolerance edges hardest.
    Uniform,
    /// Gaussian with sigma = tol/3, truncated (by rejection) at ±tol; the
    /// standard EDA convention: the datasheet tolerance is treated as a 3-sigma
    /// bound, and no sample may exceed it (a part outside its marked tolerance
    /// would have been binned out at the factory).
    Gaussian,
}

impl Distribution {
    pub fn parse(s: &str) -> Result<Self, SpecError> {
        match s {
            "uniform" => Ok(Distribution::Uniform),
            "gaussian" => Ok(Distribution::Gaussian),
            other => Err(SpecError::Invalid(format!(
                "unknown tolerance distribution '{other}'{} (expected uniform|gaussian)",
                crate::error::did_you_mean_hint(other, &["uniform", "gaussian"])
            ))),
        }
    }
}

/// One component with a resolved tolerance: the board reference, the parsed
/// nominal value (SI units), and the spread.
#[derive(Debug, Clone)]
pub struct ResolvedTolerance {
    pub reference: String,
    /// Nominal value in base SI units (Ω, F, H, ...), parsed from the
    /// `[[override]]` value (if the tolerance came from one) or the board's
    /// own component value.
    pub nominal_si: f64,
    /// Tolerance as a percentage of nominal (10.0 = ±10%).
    pub percent: f64,
    pub distribution: Distribution,
}

/// Which extreme a component sits at in a corner-mode run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Corner {
    Min,
    Max,
}

/// One component's concrete value for one ensemble member.
#[derive(Debug, Clone)]
pub struct SampledValue {
    pub reference: String,
    /// The sampled value in SI units; this exact number is written into the
    /// board before binding.
    pub si: f64,
    pub nominal_si: f64,
    /// `Some` in corner mode (which extreme), `None` for Monte-Carlo samples.
    pub corner: Option<Corner>,
}

impl SampledValue {
    /// Human form for reports: `R3=10.9k` / `C1=18.2n(min)`.
    pub fn describe(&self) -> String {
        let tag = match self.corner {
            Some(Corner::Min) => "(min)",
            Some(Corner::Max) => "(max)",
            None => "",
        };
        format!("{}={}{tag}", self.reference, format_engineering(self.si))
    }
}

/// One ensemble member: the seed index and every toleranced component's value
/// for that member. `values` is empty when the spec has no tolerances (the
/// plain fuzz/single-run path).
#[derive(Debug, Clone)]
pub struct SeedPlan {
    pub seed: u32,
    pub values: Vec<SampledValue>,
    /// True for a corner-mode INTERIOR probe: a member whose components all sit
    /// strictly inside their tolerance ranges rather than at an extreme.
    ///
    /// It is not part of the corner set and carries none of its bounded claim.
    /// Its whole job is to disprove monotonicity: a corner sweep that passes
    /// while an interior probe fails has shown that the extremes do not bound
    /// the response, and the assertion must report that rather than the corner
    /// pass. See [`interior_probe_count`].
    pub interior: bool,
}

/// The ensemble execution mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    MonteCarlo,
    Corners,
}

impl Mode {
    pub fn parse(s: &str) -> Result<Self, SpecError> {
        match s {
            "monte-carlo" => Ok(Mode::MonteCarlo),
            "corners" => Ok(Mode::Corners),
            other => Err(SpecError::Invalid(format!(
                "unknown [ensemble] mode '{other}'{} (expected monte-carlo|corners)",
                crate::error::did_you_mean_hint(other, &["monte-carlo", "corners"])
            ))),
        }
    }
}

/// Resolve the spec's tolerance declarations against the board: expand ref
/// patterns, pick each component's nominal, and validate that every rule
/// matches something and every nominal parses.
///
/// Rules apply in order and the **last matching rule wins** per component
/// (so a broad `ref = "R*"` can be followed by a tighter `ref = "R7"`).
/// `[[override]]` entries carrying a `tolerance` are applied after all
/// `[[tolerance]]` rules, with the override's `value` as the nominal.
/// The result is sorted by reference so corner bit-ordering and reports are
/// deterministic regardless of rule order.
pub fn resolve(spec: &Spec, board: &ExtractedBoard) -> Result<Vec<ResolvedTolerance>, SpecError> {
    use std::collections::BTreeMap;

    // reference -> (nominal source, percent, distribution). BTreeMap gives the
    // sorted-by-reference output directly.
    let mut by_ref: BTreeMap<String, ResolvedTolerance> = BTreeMap::new();

    // The value each component will have *after* overrides are applied, an
    // override without a tolerance still moves the nominal the tolerance rule
    // spreads around.
    let overridden_value = |reference: &str| -> Option<&str> {
        spec.overrides
            .iter()
            .rev()
            .find(|o| o.reference == reference)
            .map(|o| o.value.as_str())
    };

    for rule in &spec.tolerances {
        let dist = Distribution::parse(rule.distribution.as_deref().unwrap_or("uniform"))?;
        let mut matched = false;
        for comp in &board.components {
            if !glob_match(&rule.reference, &comp.reference) {
                continue;
            }
            matched = true;
            let value_str = overridden_value(&comp.reference).unwrap_or(&comp.value);
            let nominal = parse_nominal(&comp.reference, value_str)?;
            by_ref.insert(
                comp.reference.clone(),
                ResolvedTolerance {
                    reference: comp.reference.clone(),
                    nominal_si: nominal,
                    percent: rule.percent,
                    distribution: dist,
                },
            );
        }
        if !matched {
            let refs: Vec<String> = board
                .components
                .iter()
                .map(|c| c.reference.clone())
                .collect();
            let near = crate::error::near_matches(&rule.reference, &refs, 5);
            let hint = if near.is_empty() {
                String::new()
            } else {
                format!("; did you mean: {}?", near.join(", "))
            };
            return Err(SpecError::Invalid(format!(
                "[[tolerance]] ref '{}' matches no component on the board{hint}",
                rule.reference
            )));
        }
    }

    // Overrides with a tolerance: applied last, so they win over any [[tolerance]]
    // pattern covering the same ref. The nominal must be the ref's EFFECTIVE board
    // value; the LAST override on that ref (apply_overrides is last-wins), not
    // the value of whichever (possibly earlier) override carries the tolerance
    // field. Otherwise duplicate overrides on one ref spread the ensemble around a
    // stale nominal while the board runs the last override's value.
    let mut last_value: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
    for ov in &spec.overrides {
        last_value.insert(ov.reference.as_str(), ov.value.as_str());
    }
    for ov in &spec.overrides {
        let Some(percent) = ov.tolerance else {
            continue;
        };
        let dist = Distribution::parse(ov.distribution.as_deref().unwrap_or("uniform"))?;
        let eff_value = last_value
            .get(ov.reference.as_str())
            .copied()
            .unwrap_or(ov.value.as_str());
        let nominal = parse_nominal(&ov.reference, eff_value)?;
        by_ref.insert(
            ov.reference.clone(),
            ResolvedTolerance {
                reference: ov.reference.clone(),
                nominal_si: nominal,
                percent,
                distribution: dist,
            },
        );
    }

    Ok(by_ref.into_values().collect())
}

fn parse_nominal(reference: &str, value: &str) -> Result<f64, SpecError> {
    match hauksbee_models::value::parse_value(value) {
        Some(p) if p.si.is_finite() && p.si > 0.0 => Ok(p.si),
        _ => Err(SpecError::Invalid(format!(
            "tolerance on '{reference}': its value '{value}' does not parse as a \
             positive component value, so there is no nominal to spread around \
             (set an [[override]] with a numeric value, or fix the board value)"
        ))),
    }
}

/// Minimal glob: `*` matches any (possibly empty) run of characters; every
/// other character matches itself. Enough for `R*` / `R1?`-free use; we
/// deliberately support only `*` to keep the matching teachable.
pub fn glob_match(pattern: &str, s: &str) -> bool {
    fn inner(p: &[u8], s: &[u8]) -> bool {
        match p.split_first() {
            None => s.is_empty(),
            Some((b'*', rest)) => (0..=s.len()).any(|i| inner(rest, &s[i..])),
            Some((c, rest)) => s
                .split_first()
                .is_some_and(|(sc, sr)| sc == c && inner(rest, sr)),
        }
    }
    inner(pattern.as_bytes(), s.as_bytes())
}

/// Build the ensemble plan: one [`SeedPlan`] per run.
///
/// Monte-Carlo: `seed_count` members; seed 0 is the nominal baseline, every
/// other seed samples each component independently from its distribution.
/// Corners: `2^n` members (n = toleranced component count, capped at
/// [`CORNER_CAP`]); member k puts component i (in sorted-reference order) at
/// its min when bit i of k is 0 and its max when bit i is 1, so member 0 is
/// all-min and member 2^n - 1 is all-max.
pub fn build_plans(
    mode: Mode,
    seed_count: u32,
    tolerances: &[ResolvedTolerance],
) -> Result<Vec<SeedPlan>, SpecError> {
    match mode {
        Mode::MonteCarlo => Ok((0..seed_count)
            .map(|seed| SeedPlan {
                seed,
                values: tolerances.iter().map(|t| sample(seed, t)).collect(),
                interior: false,
            })
            .collect()),
        Mode::Corners => {
            let n = tolerances.len();
            if n == 0 {
                return Err(SpecError::Invalid(
                    "[ensemble] mode = \"corners\" needs at least one [[tolerance]]".into(),
                ));
            }
            if n > CORNER_CAP {
                // n can be arbitrarily large here (one tolerance per component),
                // so 1u64 << n overflows for n >= 64, a debug panic / wrong
                // number in the very message that reports the cap was blown.
                let runs = match u32::try_from(n).ok().and_then(|s| 1u64.checked_shl(s)) {
                    Some(v) => v.to_string(),
                    None => "more than 2^63".to_string(),
                };
                return Err(SpecError::Invalid(format!(
                    "corner mode enumerates 2^n combinations and {n} toleranced \
                     components would be {runs} runs (cap is 2^{CORNER_CAP} = {}); \
                     use mode = \"monte-carlo\" above {CORNER_CAP} components",
                    1u64 << CORNER_CAP,
                )));
            }
            let count = 1u32 << n;
            let mut plans: Vec<SeedPlan> = (0..count)
                .map(|k| SeedPlan {
                    seed: k,
                    interior: false,
                    values: tolerances
                        .iter()
                        .enumerate()
                        .map(|(i, t)| {
                            let corner = if (k >> i) & 1 == 0 {
                                Corner::Min
                            } else {
                                Corner::Max
                            };
                            let tol = t.percent / 100.0;
                            let si = match corner {
                                Corner::Min => t.nominal_si * (1.0 - tol),
                                Corner::Max => t.nominal_si * (1.0 + tol),
                            };
                            SampledValue {
                                reference: t.reference.clone(),
                                si,
                                nominal_si: t.nominal_si,
                                corner: Some(corner),
                            }
                        })
                        .collect(),
                })
                .collect();
            // The interior probes follow the corners, numbered on from the last
            // corner so a member index still names exactly one run and `--seed k`
            // isolates a probe the same way it isolates a corner.
            plans.extend(interior_plans(count, tolerances));
            Ok(plans)
        }
    }
}

/// The interior Latin-hypercube probes for a corner run, numbered from
/// `first_seed`.
///
/// One probe per stratum per component: each component's `[-tol, +tol]` range is
/// cut into `N` equal strata, each probe takes one stratum's midpoint, and the
/// stratum order is permuted INDEPENDENTLY per component. That independence is
/// the whole design: without it every probe would sit on the range's diagonal and
/// a response that only misbehaves off-diagonal would never be sampled.
///
/// Midpoints keep every probe strictly interior, so a probe can never coincide
/// with a corner and re-report it as new information. The permutation comes from
/// the same domain-tagged [`SplitMix`] the Monte-Carlo sampler uses, keyed on the
/// component reference, so the design is pure in the resolved tolerances and a
/// re-run reproduces it exactly.
///
/// The design is NOT stable across edits to the spec, and it does not claim to
/// be: `probes` itself depends on the component count, so adding the third
/// toleranced component to a two-component spec reshuffles every axis and moves
/// `first_seed`. An interior member index therefore identifies a point only
/// within one spec revision, which is all `--seed` replay needs. The corner
/// indices have the same property (a corner is a bit pattern over a
/// sorted-by-reference list) and this is the existing contract, not a new
/// weakening of it.
fn interior_plans(first_seed: u32, tolerances: &[ResolvedTolerance]) -> Vec<SeedPlan> {
    let probes = interior_probe_count(tolerances.len());

    // Per component, the stratum index each probe uses.
    let strata: Vec<Vec<usize>> = tolerances
        .iter()
        .map(|t| {
            let mut order: Vec<usize> = (0..probes).collect();
            // Fisher-Yates, walking down so the swap partner is always drawn from
            // the not-yet-placed prefix.
            let mut rng = SplitMix::for_component(u32::MAX, &format!("lhs:{}", t.reference));
            for i in (1..probes).rev() {
                order.swap(i, rng.below(i as u64 + 1) as usize);
            }
            order
        })
        .collect();

    (0..probes)
        .map(|p| SeedPlan {
            seed: first_seed + p as u32,
            interior: true,
            values: tolerances
                .iter()
                .enumerate()
                .map(|(i, t)| {
                    // Stratum midpoint mapped onto [-1, +1]: for probes = 4 the
                    // fractions are -0.75, -0.25, +0.25, +0.75, all strictly
                    // inside the range the corners already cover.
                    let frac = 2.0 * (strata[i][p] as f64 + 0.5) / probes as f64 - 1.0;
                    SampledValue {
                        reference: t.reference.clone(),
                        si: t.nominal_si * (1.0 + frac * t.percent / 100.0),
                        nominal_si: t.nominal_si,
                        corner: None,
                    }
                })
                .collect(),
        })
        .collect()
}

/// Sample one component's value for one seed. Pure in `(seed, reference,
/// rule)`: the PRNG stream is seeded from a domain-separated hash of the seed
/// and the reference, so the value does not depend on what other components
/// are toleranced or on evaluation order, that is what makes `--seed N`
/// re-runs byte-identical.
pub fn sample(seed: u32, t: &ResolvedTolerance) -> SampledValue {
    let tol = t.percent / 100.0;
    let frac = if seed == 0 {
        0.0 // seed 0 is the nominal baseline, mirroring fuzz's all-low seed 0.
    } else {
        let mut rng = SplitMix::for_component(seed, &t.reference);
        match t.distribution {
            // Uniform over [-1, 1).
            Distribution::Uniform => 2.0 * rng.next_f64() - 1.0,
            // Truncated gaussian: sigma = tol/3 => unit-sigma normal truncated
            // at |z| <= 3, by rejection (deterministic: the stream is private
            // to this (seed, ref) pair, so extra draws perturb nothing else).
            Distribution::Gaussian => loop {
                let z = rng.next_gaussian();
                if z.abs() <= 3.0 {
                    break z / 3.0;
                }
            },
        }
    };
    SampledValue {
        reference: t.reference.clone(),
        si: t.nominal_si * (1.0 + frac * tol),
        nominal_si: t.nominal_si,
        corner: None,
    }
}

/// A splitmix64 PRNG, tiny, solid, and stateless to seed. The same family the
/// fuzz path's `hash2` uses, kept separate and domain-tagged (`"tol:"`) so the
/// two streams can never collide.
struct SplitMix(u64);

impl SplitMix {
    fn for_component(seed: u32, reference: &str) -> Self {
        // Fold the domain tag + reference bytes into the state, then mix.
        let mut x = (seed as u64)
            .wrapping_mul(0x9E3779B97F4A7C15)
            .wrapping_add(0xD1B54A32D192ED03);
        for b in "tol:".bytes().chain(reference.bytes()) {
            x ^= b as u64;
            x = x.wrapping_mul(0xFF51AFD7ED558CCD);
            x ^= x >> 33;
        }
        SplitMix(x)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }

    /// Uniform in [0, 1) with 53 bits of precision.
    fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// A uniform integer in `[0, n)`, without the modulo bias of `next_u64() % n`.
    ///
    /// `n` never divides 2^64 for the probe counts the Latin-hypercube shuffle
    /// uses (3, 5, 6, 7), so plain `%` would favour the low residues. The bias is
    /// tiny, and the shuffle would still be a valid Latin hypercube, but a
    /// "uniform permutation" that is not uniform is the kind of quiet
    /// wrongness that later gets cited as a guarantee. Rejection sampling costs
    /// an occasional extra draw from a private stream and nothing else.
    fn below(&mut self, n: u64) -> u64 {
        debug_assert!(n > 0);
        // The largest multiple of n that fits in u64: draws at or above it are
        // the ones that would skew the residues, so redraw them.
        let limit = u64::MAX - (u64::MAX % n);
        loop {
            let v = self.next_u64();
            if v < limit {
                return v % n;
            }
        }
    }

    /// Standard normal via Box–Muller.
    fn next_gaussian(&mut self) -> f64 {
        // u1 in (0, 1] so ln never sees 0.
        let u1 = 1.0 - self.next_f64();
        let u2 = self.next_f64();
        (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()
    }
}

/// Format an SI value in engineering notation for reports: 10900 -> "10.9k",
/// 1.82e-8 -> "18.2n". Three significant digits; trailing zeros trimmed.
pub fn format_engineering(v: f64) -> String {
    if v == 0.0 || !v.is_finite() {
        return format!("{v}");
    }
    const SUFFIX: &[(f64, &str)] = &[
        (1e9, "G"),
        (1e6, "M"),
        (1e3, "k"),
        (1.0, ""),
        (1e-3, "m"),
        (1e-6, "u"),
        (1e-9, "n"),
        (1e-12, "p"),
    ];
    // Three significant digits on the scaled mantissa (which lives in [1, 1000)).
    let sig_digits = |m: f64| {
        if m >= 100.0 {
            0
        } else if m >= 10.0 {
            1
        } else {
            2
        }
    };
    let round_to = |m: f64, digits: usize| {
        let factor = 10f64.powi(digits as i32);
        (m * factor).round() / factor
    };

    let mag = v.abs();
    let mut idx = SUFFIX
        .iter()
        .position(|(s, _)| mag >= *s)
        .unwrap_or(SUFFIX.len() - 1);
    let mut mantissa = mag / SUFFIX[idx].0;
    let mut digits = sig_digits(mantissa);
    // Detect the carry: rounding the mantissa to `digits` decimals can push it up
    // to 1000 (e.g. 999999 -> "1000k", 999.6 -> "1000"). When that happens promote
    // to the next-larger suffix so the shown mantissa stays in [1, 1000), "1M"/"1k"
    // rather than "1000k"/"1000". At the top suffix (G) there is nothing larger, so
    // leave it (unreachable for realistic component tolerances).
    if round_to(mantissa, digits) >= 1000.0 && idx > 0 {
        idx -= 1;
        mantissa = mag / SUFFIX[idx].0;
        digits = sig_digits(mantissa);
    }
    let suffix = SUFFIX[idx].1;
    let s = format!("{mantissa:.digits$}");
    let s = if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        s
    };
    format!("{}{s}{suffix}", if v < 0.0 { "-" } else { "" })
}

/// One-line description of a plan's sampled set: `R1=10.9k, R2=9.4k`.
pub fn describe_values(values: &[SampledValue]) -> String {
    values
        .iter()
        .map(SampledValue::describe)
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(reference: &str, nominal: f64, percent: f64, dist: Distribution) -> ResolvedTolerance {
        ResolvedTolerance {
            reference: reference.into(),
            nominal_si: nominal,
            percent,
            distribution: dist,
        }
    }

    #[test]
    fn duplicate_override_spreads_around_the_last_value() {
        // R55: apply_overrides is last-wins (the board runs the LAST override's
        // value), but resolve() keyed the ensemble nominal off whichever override
        // carried the tolerance field. Two overrides on R1; the first with a
        // tolerance, the second setting the real value, must spread around the
        // LAST value, not the earlier one.
        let spec: Spec = toml::from_str(
            "board = \"b.kicad_pcb\"\nduration_ms = 10\n\
             [[override]]\nref = \"R1\"\nvalue = \"10k\"\ntolerance = 5\n\
             [[override]]\nref = \"R1\"\nvalue = \"12k\"\n\
             [[assert]]\nkind = \"voltage\"\nnet = \"VCC\"\nmin = 3.0\n",
        )
        .unwrap();
        let board = ExtractedBoard {
            name: "b".into(),
            nets: vec![],
            components: vec![],
        };
        let resolved = resolve(&spec, &board).unwrap();
        let r1 = resolved
            .iter()
            .find(|r| r.reference == "R1")
            .expect("R1 resolved");
        assert!(
            (r1.nominal_si - 12_000.0).abs() < 1.0,
            "must spread around the last override (12k), got {}",
            r1.nominal_si
        );
    }

    #[test]
    fn seed_zero_is_nominal() {
        let t = rule("R1", 10_000.0, 10.0, Distribution::Uniform);
        assert_eq!(sample(0, &t).si, 10_000.0);
        let g = rule("R1", 10_000.0, 10.0, Distribution::Gaussian);
        assert_eq!(sample(0, &g).si, 10_000.0);
    }

    #[test]
    fn samples_are_deterministic_and_order_independent() {
        let t = rule("R1", 10_000.0, 10.0, Distribution::Uniform);
        let a = sample(7, &t).si;
        let b = sample(7, &t).si;
        assert_eq!(a.to_bits(), b.to_bits(), "same (seed, ref) => same value");
        // A different reference gets a different stream.
        let u = rule("R2", 10_000.0, 10.0, Distribution::Uniform);
        assert_ne!(sample(7, &t).si.to_bits(), sample(7, &u).si.to_bits());
    }

    #[test]
    fn uniform_samples_stay_inside_the_tolerance_band() {
        let t = rule("R1", 10_000.0, 10.0, Distribution::Uniform);
        for seed in 1..500 {
            let v = sample(seed, &t).si;
            assert!((9_000.0..=11_000.0).contains(&v), "seed {seed}: {v}");
        }
    }

    #[test]
    fn gaussian_samples_are_truncated_at_the_tolerance_bound() {
        let t = rule("C1", 1e-7, 20.0, Distribution::Gaussian);
        let mut spread = 0.0f64;
        for seed in 1..2000 {
            let v = sample(seed, &t).si;
            assert!(
                v >= 0.8e-7 - 1e-20 && v <= 1.2e-7 + 1e-20,
                "seed {seed}: {v}"
            );
            spread = spread.max((v - 1e-7).abs());
        }
        // The distribution actually spreads (not all-nominal).
        assert!(
            spread > 0.05e-7,
            "gaussian never moved: max spread {spread}"
        );
    }

    #[test]
    fn corner_plans_enumerate_all_min_max_combinations() {
        let ts = vec![
            rule("R1", 10_000.0, 10.0, Distribution::Uniform),
            rule("R2", 10_000.0, 10.0, Distribution::Uniform),
        ];
        let plans = build_plans(Mode::Corners, 0, &ts).unwrap();
        let corners: Vec<&SeedPlan> = plans.iter().filter(|p| !p.interior).collect();
        assert_eq!(corners.len(), 4);
        // Member 0 = all-min; member 3 = all-max.
        assert!(corners[0].values.iter().all(|v| v.si == 9_000.0));
        assert!(corners[3].values.iter().all(|v| v.si == 11_000.0));
        // Member 1: bit0 set => R1 at max, R2 at min.
        assert_eq!(corners[1].values[0].si, 11_000.0);
        assert_eq!(corners[1].values[1].si, 9_000.0);
    }

    /// The interior probes must sit STRICTLY inside the corner box, or they
    /// re-report a corner as if it were new information about the interior.
    #[test]
    fn interior_probes_are_strictly_inside_the_corner_box() {
        let ts = vec![
            rule("R1", 10_000.0, 10.0, Distribution::Uniform),
            rule("R2", 100e-9, 20.0, Distribution::Uniform),
        ];
        let plans = build_plans(Mode::Corners, 0, &ts).unwrap();
        let interior: Vec<&SeedPlan> = plans.iter().filter(|p| p.interior).collect();
        assert_eq!(
            interior.len(),
            interior_probe_count(2),
            "two components get {} probes",
            interior_probe_count(2)
        );
        for p in &interior {
            for v in &p.values {
                assert!(v.corner.is_none(), "an interior value is at no corner");
                let tol = if v.reference == "R1" { 0.10 } else { 0.20 };
                let lo = v.nominal_si * (1.0 - tol);
                let hi = v.nominal_si * (1.0 + tol);
                assert!(
                    v.si > lo && v.si < hi,
                    "{} = {} must be strictly inside ({lo}, {hi})",
                    v.reference,
                    v.si
                );
            }
        }
        // Member indices continue on from the corners, so `--seed k` still names
        // exactly one run.
        let seeds: Vec<u32> = plans.iter().map(|p| p.seed).collect();
        let mut sorted = seeds.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), seeds.len(), "member indices are unique");
    }

    /// Latin hypercube, both halves: every stratum of every component is hit
    /// exactly once (the "Latin" part), and the axes are permuted independently
    /// so the probes are not all on the diagonal (the "hypercube" part). A
    /// diagonal-only design would miss any response that only misbehaves when
    /// two components move oppositely.
    #[test]
    fn interior_probes_cover_every_stratum_and_are_not_diagonal() {
        let ts = vec![
            rule("R1", 1_000.0, 10.0, Distribution::Uniform),
            rule("R2", 1_000.0, 10.0, Distribution::Uniform),
        ];
        let probes = interior_probe_count(2);
        let plans = build_plans(Mode::Corners, 0, &ts).unwrap();
        let interior: Vec<&SeedPlan> = plans.iter().filter(|p| p.interior).collect();

        // Which stratum each probe landed in, per component.
        let stratum = |v: &SampledValue| -> usize {
            let frac = v.si / v.nominal_si - 1.0; // in (-0.1, +0.1)
            let unit = frac / 0.10; // in (-1, +1)
            (((unit + 1.0) / 2.0) * probes as f64).floor() as usize
        };
        for i in 0..2 {
            let mut hit: Vec<usize> = interior.iter().map(|p| stratum(&p.values[i])).collect();
            hit.sort_unstable();
            assert_eq!(
                hit,
                (0..probes).collect::<Vec<_>>(),
                "component {i} must hit every stratum exactly once"
            );
        }
        let diagonal = interior
            .iter()
            .all(|p| stratum(&p.values[0]) == stratum(&p.values[1]));
        assert!(!diagonal, "the axes must be permuted independently");
    }

    /// The extra cost is bounded and small: the probe count flattens at 8 rather
    /// than scaling with the 2^n corner set, so the monotonicity check never
    /// becomes the dominant cost of a corner run.
    #[test]
    fn the_interior_probe_count_is_bounded() {
        assert_eq!(interior_probe_count(1), 4);
        assert_eq!(interior_probe_count(2), 6);
        for n in 3..=CORNER_CAP {
            assert_eq!(interior_probe_count(n), 8, "n = {n}");
        }
        // At the cap the corners dominate by two orders of magnitude, which is
        // the point: the check is an addend, not a multiplier.
        let corners = 1usize << CORNER_CAP;
        assert!(interior_probe_count(CORNER_CAP) * 100 < corners);
    }

    /// The design has to be reproducible, or `--seed k` cannot re-run a probe
    /// and a red build is not investigable.
    #[test]
    fn the_interior_design_is_deterministic() {
        let ts = vec![
            rule("R1", 4_700.0, 5.0, Distribution::Uniform),
            rule("C2", 22e-6, 20.0, Distribution::Gaussian),
            rule("R9", 100.0, 1.0, Distribution::Uniform),
        ];
        let a = build_plans(Mode::Corners, 0, &ts).unwrap();
        let b = build_plans(Mode::Corners, 0, &ts).unwrap();
        for (pa, pb) in a.iter().zip(b.iter()) {
            assert_eq!(pa.seed, pb.seed);
            assert_eq!(pa.interior, pb.interior);
            for (va, vb) in pa.values.iter().zip(pb.values.iter()) {
                assert_eq!(va.si.to_bits(), vb.si.to_bits(), "{} moved", va.reference);
            }
        }
    }

    /// `below` must be free of the modulo bias `next_u64() % n` carries, since
    /// none of the probe counts divide 2^64. A chi-square-free check is enough
    /// here: over many draws every residue must appear within a few percent of
    /// its expected share.
    #[test]
    fn below_is_uniform_over_its_range() {
        for n in [3u64, 5, 6, 7, 8] {
            let mut rng = SplitMix::for_component(1, "uniformity");
            let draws = 120_000usize;
            let mut counts = vec![0usize; n as usize];
            for _ in 0..draws {
                let v = rng.below(n);
                assert!(v < n, "below({n}) returned {v}");
                counts[v as usize] += 1;
            }
            let expected = draws as f64 / n as f64;
            for (r, c) in counts.iter().enumerate() {
                let dev = (*c as f64 - expected).abs() / expected;
                assert!(
                    dev < 0.05,
                    "n = {n}, residue {r}: {c} draws is {:.1}% off the expected {expected:.0}",
                    dev * 100.0
                );
            }
        }
    }

    /// Monte-Carlo is untouched: it never claimed a bound, so it has nothing to
    /// probe for and must not pay for one.
    #[test]
    fn monte_carlo_gets_no_interior_probes() {
        let ts = vec![rule("R1", 1_000.0, 10.0, Distribution::Uniform)];
        let plans = build_plans(Mode::MonteCarlo, 5, &ts).unwrap();
        assert_eq!(plans.len(), 5);
        assert!(plans.iter().all(|p| !p.interior));
    }

    #[test]
    fn corner_cap_refuses_and_names_monte_carlo() {
        let ts: Vec<ResolvedTolerance> = (0..CORNER_CAP + 1)
            .map(|i| rule(&format!("R{i}"), 1_000.0, 5.0, Distribution::Uniform))
            .collect();
        let err = build_plans(Mode::Corners, 0, &ts).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("monte-carlo"),
            "refusal points at monte-carlo: {msg}"
        );
    }

    #[test]
    fn corner_cap_refusal_does_not_overflow_at_64_tolerances() {
        // 64+ toleranced components blow the cap, and the refusal must come out
        // cleanly. Reporting the corner count as 1u64 << n overflows for n >= 64
        // (a debug panic, or a wrong number in release). (round-7 #15)
        let ts: Vec<ResolvedTolerance> = (0..64)
            .map(|i| rule(&format!("R{i}"), 1_000.0, 5.0, Distribution::Uniform))
            .collect();
        let err = build_plans(Mode::Corners, 0, &ts).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("monte-carlo"),
            "refusal points at monte-carlo: {msg}"
        );
        assert!(
            msg.contains("64"),
            "refusal names the component count: {msg}"
        );
    }

    #[test]
    fn glob_match_supports_star_only() {
        assert!(glob_match("R*", "R17"));
        assert!(glob_match("R*", "R"));
        assert!(!glob_match("R*", "C3"));
        assert!(glob_match("R_Shunt*", "R_Shunt15301"));
        assert!(glob_match("*", "anything"));
        assert!(glob_match("R1", "R1"));
        assert!(!glob_match("R1", "R12"));
    }

    #[test]
    fn engineering_format_reads_naturally() {
        assert_eq!(format_engineering(10_900.0), "10.9k");
        assert_eq!(format_engineering(1.82e-8), "18.2n");
        assert_eq!(format_engineering(0.05), "50m");
        assert_eq!(format_engineering(5.0), "5");
        assert_eq!(format_engineering(2_250_000.0), "2.25M");
    }

    #[test]
    fn engineering_format_carries_past_a_decade_edge() {
        // R23 (FMT-ENG-DECADE-ROUNDUP): rounding the mantissa up to 1000 must
        // carry into the next-larger suffix, never emit a wrong-decade label
        // like "1000k" or a bare "1000".
        assert_eq!(format_engineering(999_999.0), "1M");
        assert_eq!(format_engineering(999.6), "1k");
        assert_eq!(format_engineering(999_500.0), "1M");
        // A value already clear of the edge is unaffected.
        assert_eq!(format_engineering(10_900.0), "10.9k");
        // The carry composes with the sign.
        assert_eq!(format_engineering(-999_999.0), "-1M");
    }
}
