//! Component-tolerance ensembles: Monte-Carlo sampling and deterministic
//! corner enumeration over the spec's `[[tolerance]]` rules.
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
//!    case *only for monotonic responses*, also stated in the report. Rather
//!    than leave that condition as a caveat with nothing behind it, every corner
//!    run also carries a small stratified Latin-hypercube sample of the
//!    interior, and an interior point that FAILS an assertion every corner
//!    passed fails the assertion: the corners demonstrably did not bound it.
//!    Note what that is and is not. It searches the interior for a point that
//!    breaks an assertion; it does not compare interior measurements against the
//!    corner extrema, so a response worse than every corner still passes while
//!    it stays inside the assertion's window. A clean probe set is evidence for
//!    the monotonicity the bound needs, never proof of it, and the report says
//!    which of the two it has.
//! 2. **Reproducibility is doctrine.** Re-running a member reproduces it
//!    byte-identically (`hauksbee-ci run spec.toml --seed N`), which is the
//!    property that makes a red build investigable. Every Monte-Carlo sampled
//!    value is a pure function of `(seed, component reference, rule)`, so it
//!    depends on neither iteration order nor how many other components are
//!    toleranced, and the tolerance stream is domain-separated (`"tol:" + ref`)
//!    from the net-fuzz stream, so adding a tolerance never changes which fuzz
//!    levels seed N straps.
//!
//!    The corner and interior members are reproducible for a given spec but are
//!    NOT stable across edits to it, and do not claim to be: a corner index is a
//!    bit pattern over the sorted-by-reference tolerance list, and the interior
//!    probe count and starting index both depend on how many components are
//!    toleranced. Adding a rule renumbers both. A member index identifies a
//!    point within one spec revision, which is what replay needs.
//!
//! In Monte-Carlo mode seed 0 is always the nominal baseline (all components at
//! nominal, matching fuzz's all-low seed 0), so "nominal passes but the ensemble
//! fails" is visible inside a single run. Corner mode has no nominal member:
//! member 0 is the all-min corner, since a nominal run would tell it nothing it
//! is looking for.

use crate::error::SpecError;
use crate::spec::Spec;
use hauksbee_engine::variant_sweep::OpenSet;
use hauksbee_extract::ExtractedBoard;
use hauksbee_ir::units::format_engineering;

/// Full-factorial corner enumeration is capped at `2^CORNER_CAP` runs.
///
/// Above this many toleranced components, corner mode refuses and points at
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
/// probes for one component, 6 for two, 8 from three up.
///
/// What it buys is detection of a non-monotonicity severe enough to break an
/// assertion, which is the case that matters and the only one observable from a
/// pass/fail verdict. It is not a monotonicity test: each member is judged
/// against the assertion's own window, so a response worse than every corner but
/// still in band passes. That is why a clean sweep narrows the disclosure rather
/// than removing it.
#[must_use]
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
    /// Parse a spec's `distribution` token.
    ///
    /// # Errors
    ///
    /// [`SpecError::Invalid`] for anything but `uniform` or `gaussian`.
    pub fn parse(s: &str) -> Result<Self, SpecError> {
        match s {
            "uniform" => Ok(Self::Uniform),
            "gaussian" => Ok(Self::Gaussian),
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
    #[must_use]
    pub fn describe(&self) -> String {
        let tag = match self.corner {
            Some(Corner::Min) => "(min)",
            Some(Corner::Max) => "(max)",
            None => "",
        };
        format!(
            "{}={}{tag}",
            self.reference,
            format_engineering(self.si, "")
        )
    }
}

/// One ensemble member: the seed index and every toleranced component's value
/// for that member. `values` is empty when the spec has no tolerances (the
/// plain fuzz/single-run path).
#[derive(Debug, Clone, Default)]
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
    /// The variant member this run simulates for each undetermined same-pinout
    /// part, `(reference, member id)`: the member axis of [`with_variants`].
    pub variants: Vec<(String, String)>,
    /// The `[search]` operating point this member runs at: one forced net
    /// voltage or injected current per swept net (see [`crate::search`]).
    pub conditions: Vec<crate::search::Condition>,
    /// Post-bind nudges, `(reference, factor)`, that scale a switch's
    /// thresholds: the culprit-ranking reruns of [`crate::search`]. Passive
    /// values are nudged through `values` instead, before binding.
    pub perturb: Vec<(String, f64)>,
}

/// Cross every plan with every combination of the undetermined variant sets.
///
/// The sets are same-pinout `(reference, member ids)`, so the ensemble passes
/// only if every member does. `None` above 2^[`CORNER_CAP`] runs: the caller
/// decides what covers the sets then ([`sweep_open_sets`]); it must never just
/// keep the flagged stand-ins without saying so.
#[must_use]
pub fn with_variants(
    mut runs: Vec<SeedPlan>,
    axes: &[(String, Vec<String>)],
) -> Option<Vec<SeedPlan>> {
    for (reference, ids) in axes {
        let member = |plan: &SeedPlan, id: &String| {
            let mut plan = plan.clone();
            plan.variants.push((reference.clone(), id.clone()));
            plan
        };
        let cross = |p: &SeedPlan| ids.iter().map(|id| member(p, id)).collect::<Vec<_>>();
        runs = runs.iter().flat_map(cross).collect();
        if runs.len() > 1 << CORNER_CAP {
            return None;
        }
    }
    Some(runs)
}

/// One member pin list: `(reference, member id)` per open set.
type Pins = Vec<(String, String)>;

/// How far a run got through its board's open variant and corner sets.
///
/// Everything here other than [`Self::is_full`] is coverage the run did NOT
/// reach, kept so the report can say what a pass rests on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VariantCoverage {
    /// How the assumed-corner sets were covered.
    pub corners: CornerCover,
    /// The assumed-corner sets' references.
    pub corner_refs: Vec<String>,
    /// The member each assumed-corner set binds when nothing pins it.
    pub corner_stand_ins: Vec<String>,
    /// Runs the full product of the corner sets would have been (saturating).
    pub corner_product: u64,
    /// Orderable-variant sets whose product exceeded the cap, so they ran only
    /// as their stand-in variants.
    pub variant_refs: Vec<String>,
    /// Those sets' stand-in members.
    pub variant_stand_ins: Vec<String>,
    /// Runs the full product of the variant sets would have been (saturating).
    pub variant_product: u64,
}

/// How the assumed-corner sets were covered.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CornerCover {
    /// Every combination ran (or there were no corner sets).
    #[default]
    Full,
    /// Past the cap: all weak, all strong, and each set alone against the rest
    /// (see `boundary_patterns` for what that does and does not bound).
    Boundary,
    /// Past the cap even for [`Self::Boundary`]: every set at corner k together.
    Joint,
    /// Nothing fit: every corner set ran only as its stand-in member.
    StandIn,
}

impl VariantCoverage {
    /// True when every combination of every open set ran, so the pass needs no
    /// condition beyond what the checks themselves state.
    #[must_use]
    pub fn is_full(&self) -> bool {
        self.corners == CornerCover::Full && self.variant_refs.is_empty()
    }

    /// What a pass rests on beyond its assertions: one sentence per group of
    /// open sets the sweep did not cross in full, empty when it did.
    #[must_use]
    pub fn conditions(&self) -> Vec<String> {
        let mut out = Vec::new();
        let cap = 1u64 << CORNER_CAP;
        let (refs, n) = (list(&self.corner_refs), self.corner_refs.len());
        let fix = "put the manufacturer part number (or the colour) on each part to remove this assumption";
        let over = format!(
            "every combination would be {} runs, over the {cap}-run cap",
            self.corner_product
        );
        match self.corners {
            CornerCover::Full => {}
            CornerCover::Boundary => out.push(format!(
                "{refs} ({n} parts the board names only by class) passed at their assumed corners all weak, all strong, and each alone against the rest, not in every combination ({over}); a mix that is none of those is not judged; {fix}."
            )),
            CornerCover::Joint => out.push(format!(
                "{refs} ({n} parts the board names only by class) passed only with every part at one assumed corner together (all weak, then all strong), not in every combination ({over}); {fix}."
            )),
            CornerCover::StandIn => out.push(format!(
                "{refs} ({n} parts the board names only by class) were not swept across their assumed corners ({over}): each ran only as its stand-in ({}), the first corner the card lists, which bounds an assertion in one direction only, so a pass holds at that corner and not at the others; {fix}.",
                list(&self.corner_stand_ins)
            )),
        }
        if !self.variant_refs.is_empty() {
            out.push(format!(
                "{} have orderable variants the board does not pick, and crossing them would be {} runs, over the {cap}-run cap, so each ran only as its stand-in variant ({}); a pass holds for those variants and not the others; add `[[override]] ref=\"<ref>\" select=\"<variant>\"` to the spec, or put the orderable code in the BOM.",
                list(&self.variant_refs),
                self.variant_product,
                list(&self.variant_stand_ins)
            ));
        }
        out
    }
}

/// A list for a sentence: the first few distinct items, then a count.
fn list(items: &[String]) -> String {
    const SHOWN: usize = 6;
    let mut seen: Vec<&str> = Vec::new();
    for item in items {
        if !seen.contains(&item.as_str()) {
            seen.push(item);
        }
    }
    if seen.len() > SHOWN {
        format!(
            "{} and {} more",
            seen[..SHOWN].join(", "),
            seen.len() - SHOWN
        )
    } else {
        seen.join(", ")
    }
}

/// The plans crossed with the open sets, and how completely.
#[derive(Debug, Clone)]
pub struct VariantSweep {
    pub plans: Vec<SeedPlan>,
    pub coverage: VariantCoverage,
}

/// Cross every plan with the board's open variant and corner sets as far as
/// the 2^[`CORNER_CAP`] run budget allows, and say how far that was.
///
/// Within the budget every combination runs and the coverage is full. Past it
/// nothing is dropped silently:
///
/// * the orderable-variant sets (`corners == false`) are unordered, so no
///   subset of them stands for the rest: past the cap they keep their flagged
///   stand-in and the coverage names them;
/// * the assumed-corner sets (`corners == true`, an LED whose colour the board
///   does not name, say) are ordered by construction: each member is an
///   assumed bound of one class, listed weak to strong, and the same index
///   is the same side across sets. They fall back to a linear cover instead
///   of the product, and then only if that fits: `boundary_patterns`, then
///   `joint_patterns` (all sets at one corner together), then the stand-in
///   alone. Each fallback is reported by [`VariantCoverage::conditions`].
#[must_use]
pub fn sweep_open_sets(runs: Vec<SeedPlan>, sets: &[OpenSet]) -> VariantSweep {
    let cap = 1usize << CORNER_CAP;
    let (corners, variants): (Vec<&OpenSet>, Vec<&OpenSet>) = sets.iter().partition(|s| s.corners);
    let axes = |sets: &[&OpenSet]| -> Vec<(String, Vec<String>)> {
        (sets.iter())
            .map(|s| (s.reference.clone(), s.ids.clone()))
            .collect()
    };
    let product = |base: usize, sets: &[&OpenSet]| -> u64 {
        (sets.iter()).fold(base as u64, |n, s| n.saturating_mul(s.ids.len() as u64))
    };
    let stand_ins = |sets: &[&OpenSet]| -> Vec<String> {
        (sets.iter())
            .filter_map(|s| s.ids.first().cloned())
            .collect()
    };
    let refs =
        |sets: &[&OpenSet]| -> Vec<String> { sets.iter().map(|s| s.reference.clone()).collect() };
    let mut coverage = VariantCoverage::default();
    let mut runs = runs;
    if !variants.is_empty() {
        if let Some(crossed) = with_variants(runs.clone(), &axes(&variants)) {
            runs = crossed;
        } else {
            coverage.variant_refs = refs(&variants);
            coverage.variant_stand_ins = stand_ins(&variants);
            coverage.variant_product = product(runs.len(), &variants);
        }
    }
    if corners.is_empty() {
        return VariantSweep {
            plans: runs,
            coverage,
        };
    }
    if let Some(crossed) = with_variants(runs.clone(), &axes(&corners)) {
        return VariantSweep {
            plans: crossed,
            coverage,
        };
    }
    coverage.corner_refs = refs(&corners);
    coverage.corner_stand_ins = stand_ins(&corners);
    coverage.corner_product = product(runs.len(), &corners);
    let owned: Vec<OpenSet> = corners.iter().map(|s| (*s).clone()).collect();
    let tiers = [
        (boundary_patterns(&owned), CornerCover::Boundary),
        (joint_patterns(&owned), CornerCover::Joint),
    ];
    let fits = |patterns: &Vec<Pins>| runs.len().saturating_mul(patterns.len()) <= cap;
    let (plans, cover) = match tiers.iter().find(|(patterns, _)| fits(patterns)) {
        Some((patterns, cover)) => (cross_patterns(&runs, patterns), *cover),
        None => (runs, CornerCover::StandIn),
    };
    coverage.corners = cover;
    VariantSweep { plans, coverage }
}

/// Every plan once per pin pattern, one seed's members adjacent.
fn cross_patterns(runs: &[SeedPlan], patterns: &[Pins]) -> Vec<SeedPlan> {
    let pinned = |plan: &SeedPlan, pins: &Pins| {
        let mut plan = plan.clone();
        plan.variants.extend(pins.iter().cloned());
        plan
    };
    runs.iter()
        .flat_map(|plan| patterns.iter().map(|pins| pinned(plan, pins)))
        .collect()
}

/// Every set at its k-th member together, for each k up to the widest set (a
/// narrower set holds its last member): all weak, then all strong.
fn joint_patterns(sets: &[OpenSet]) -> Vec<Pins> {
    let widest = sets.iter().map(|s| s.ids.len()).max().unwrap_or(0);
    let at = |k: usize| -> Pins {
        (sets.iter())
            .filter_map(|s| {
                Some((
                    s.reference.clone(),
                    s.ids.get(k).or_else(|| s.ids.last())?.clone(),
                ))
            })
            .collect()
    };
    let mut out: Vec<Pins> = Vec::new();
    for pins in (0..widest).map(at) {
        if !out.contains(&pins) {
            out.push(pins);
        }
    }
    out
}

/// The linear cover of the assumed-corner sets: [`joint_patterns`] plus, for
/// each set and each of its members, that member with every other set held
/// all-first and then all-last. That is `widest + 2 * (total members)` runs
/// instead of the product.
///
/// Why this bounds what CI asserts on. An assumed corner set is one class's
/// assumed bounds (a generic LED's forward voltage, a generic FET's
/// threshold): each member shifts one monotone characteristic of a part that
/// sits on its own branch. What a CI assertion reads off such a board is
/// either
///
/// * a quantity of one part's own branch (its current, the node it drives, its
///   dissipation), which that part's corner moves one way and every OTHER
///   part's corner moves the other way (a lighter or heavier load on the shared
///   supply) or not at all: the worst case is that part at one member with the
///   rest all at the opposite member, which is a `(set, member, rest
///   all-first | all-last)` pattern; or
/// * a shared quantity (the rail sagging under the combined load, the total
///   current), which every part moves the same way: the worst case is all the
///   parts at the same extreme, which is a joint pattern.
///
/// Where the premise fails this is a sample, not a bound: a quantity that is
/// a DIFFERENCE of two parts' responses, or parts coupled through something
/// other than a shared supply, can have its worst case at a mix that is in
/// none of these patterns. So a pass swept this way is never reported clean:
/// [`VariantCoverage::conditions`] says it was not every combination.
fn boundary_patterns(sets: &[OpenSet]) -> Vec<Pins> {
    type Rest = fn(&OpenSet) -> Option<&String>;
    let mut out = joint_patterns(sets);
    let rests: [Rest; 2] = [|t| t.ids.first(), |t| t.ids.last()];
    for set in sets {
        for id in &set.ids {
            for rest in rests {
                let pins: Pins = (sets.iter())
                    .filter_map(|t| {
                        let pick = if t.reference == set.reference {
                            id
                        } else {
                            rest(t)?
                        };
                        Some((t.reference.clone(), pick.clone()))
                    })
                    .collect();
                if !out.contains(&pins) {
                    out.push(pins);
                }
            }
        }
    }
    out
}

/// The ensemble execution mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    MonteCarlo,
    Corners,
}

impl Mode {
    /// Parse a spec's `[ensemble] mode` token.
    ///
    /// # Errors
    ///
    /// [`SpecError::Invalid`] for anything but `monte-carlo` or `corners`.
    pub fn parse(s: &str) -> Result<Self, SpecError> {
        match s {
            "monte-carlo" => Ok(Self::MonteCarlo),
            "corners" => Ok(Self::Corners),
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
///
/// # Errors
///
/// [`SpecError::Invalid`] if a distribution token is unknown, a
/// `[[tolerance]]` ref pattern matches no component, or a matched component's
/// effective value does not parse as a positive number.
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
            .find(|o| o.reference == reference && o.value.is_some())
            .and_then(|o| o.value.as_deref())
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
            return Err(SpecError::Invalid(format!(
                "[[tolerance]] ref '{}' matches no component on the board{}",
                rule.reference,
                crate::error::suggestion_clause(&near)
            )));
        }
    }

    // Overrides with a tolerance: applied last, so they win over any [[tolerance]]
    // pattern covering the same ref. The nominal must be the ref's EFFECTIVE board
    // value; the LAST override on that ref (apply_overrides is last-wins), not
    // the value of whichever (possibly earlier) override carries the tolerance
    // field. Otherwise duplicate overrides on one ref spread the ensemble around a
    // stale nominal while the board runs the last override's value.
    for ov in &spec.overrides {
        let Some(percent) = ov.tolerance else {
            continue;
        };
        let dist = Distribution::parse(ov.distribution.as_deref().unwrap_or("uniform"))?;
        let eff_value = overridden_value(&ov.reference).unwrap_or_default();
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

/// Minimal glob: `*` matches any (possibly empty) run of characters.
///
/// Every other character matches itself. Enough for `R*` / `R1?`-free use; we
/// deliberately support only `*` to keep the matching teachable.
#[must_use]
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
///
/// # Errors
///
/// [`SpecError::Invalid`] in corner mode when there are no tolerances, or
/// more than [`CORNER_CAP`] of them.
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
                ..SeedPlan::default()
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
                let runs = u32::try_from(n)
                    .ok()
                    .and_then(|s| 1u64.checked_shl(s))
                    .map_or_else(|| "more than 2^63".to_string(), |v| v.to_string());
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
                    values: tolerances
                        .iter()
                        .enumerate()
                        .map(|(i, t)| {
                            let (corner, sign) = if (k >> i) & 1 == 0 {
                                (Corner::Min, -1.0)
                            } else {
                                (Corner::Max, 1.0)
                            };
                            SampledValue {
                                reference: t.reference.clone(),
                                si: t.nominal_si * (1.0 + sign * t.percent / 100.0),
                                nominal_si: t.nominal_si,
                                corner: Some(corner),
                            }
                        })
                        .collect(),
                    ..SeedPlan::default()
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
/// the whole design: with a shared order every probe would sit on the range's
/// diagonal, and a response that only misbehaves when two components move
/// oppositely would never be sampled. Independence makes an all-diagonal design
/// vanishingly unlikely rather than impossible, and nothing rejects one; a
/// coincidence there costs this run's off-diagonal coverage, not correctness of
/// anything the run reports.
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
// `probes` is at most 8, so every index and count cast here is exact.
#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
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
            ..SeedPlan::default()
        })
        .collect()
}

/// Sample one component's value for one seed.
///
/// Pure in `(seed, reference, rule)`: the PRNG stream is seeded from a domain-separated hash of the seed
/// and the reference, so the value does not depend on what other components
/// are toleranced or on evaluation order, that is what makes `--seed N`
/// re-runs byte-identical.
#[must_use]
pub fn sample(seed: u32, t: &ResolvedTolerance) -> SampledValue {
    let tol = t.percent / 100.0;
    let frac = if seed == 0 {
        0.0 // seed 0 is the nominal baseline, mirroring fuzz's all-low seed 0.
    } else {
        let mut rng = SplitMix::for_component(seed, &t.reference);
        match t.distribution {
            // Uniform over [-1, 1).
            Distribution::Uniform => rng.next_f64().mul_add(2.0, -1.0),
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
        let mut x = u64::from(seed)
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            .wrapping_add(0xD1B5_4A32_D192_ED03);
        for b in "tol:".bytes().chain(reference.bytes()) {
            x ^= u64::from(b);
            x = x.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
            x ^= x >> 33;
        }
        Self(x)
    }

    const fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in [0, 1) with 53 bits of precision.
    // A 53-bit integer and 2^53 are both exactly representable in f64.
    #[allow(clippy::cast_precision_loss)]
    fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// A uniform integer in `[0, n)`, without the modulo bias of `next_u64() % n`.
    ///
    /// The Latin-hypercube shuffle calls this with every `n` in `2..=probes`,
    /// and `probes` is 4, 6 or 8, so it hits 3, 5, 6 and 7: none of them divide
    /// 2^64, and plain `%` would favour the low residues. The bias is tiny, and
    /// the shuffle would still be a valid Latin hypercube, but a "uniform
    /// permutation" that is not uniform is the kind of quiet wrongness that
    /// later gets cited as a guarantee. Rejection sampling costs an occasional
    /// extra draw from a private stream and nothing else.
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
        // apply_overrides is last-wins, so the ensemble nominal must be the
        // last override's value even when an earlier one carried the tolerance.
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
            "got {}",
            r1.nominal_si
        );
    }

    #[test]
    fn samples_are_nominal_at_seed_zero_deterministic_and_inside_the_band() {
        let t = rule("R1", 10_000.0, 10.0, Distribution::Uniform);
        let g = rule("C1", 1e-7, 20.0, Distribution::Gaussian);
        assert_eq!(sample(0, &t).si.to_bits(), 10_000.0_f64.to_bits());
        assert_eq!(sample(0, &g).si.to_bits(), 1e-7_f64.to_bits());
        // Same (seed, ref) => same value; a different ref gets its own stream.
        assert_eq!(sample(7, &t).si.to_bits(), sample(7, &t).si.to_bits());
        let u = rule("R2", 10_000.0, 10.0, Distribution::Uniform);
        assert_ne!(sample(7, &t).si.to_bits(), sample(7, &u).si.to_bits());
        // Uniform stays inside the band; gaussian is truncated at it and
        // actually spreads.
        for seed in 1..500 {
            let v = sample(seed, &t).si;
            assert!((9_000.0..=11_000.0).contains(&v), "seed {seed}: {v}");
        }
        let mut spread = 0.0f64;
        for seed in 1..2000 {
            let v = sample(seed, &g).si;
            assert!(
                (0.8e-7 - 1e-20..=1.2e-7 + 1e-20).contains(&v),
                "seed {seed}: {v}"
            );
            spread = spread.max((v - 1e-7).abs());
        }
        assert!(
            spread > 0.05e-7,
            "gaussian never moved: max spread {spread}"
        );
    }

    /// Corners enumerate every min/max combination (bit i of the member index
    /// is component i), the interior probes continue the member numbering so
    /// `--seed k` names exactly one run, and Monte-Carlo (which never claimed
    /// a bound) gets no probes.
    #[test]
    fn corner_plans_enumerate_all_min_max_combinations_then_the_probes() {
        let ts = vec![
            rule("R1", 10_000.0, 10.0, Distribution::Uniform),
            rule("R2", 10_000.0, 10.0, Distribution::Uniform),
        ];
        let plans = build_plans(Mode::Corners, 0, &ts).unwrap();
        let corners: Vec<&SeedPlan> = plans.iter().filter(|p| !p.interior).collect();
        assert_eq!(corners.len(), 4);
        let all_at = |p: &SeedPlan, si: f64| p.values.iter().all(|v| (v.si - si).abs() < 1e-9);
        assert!(all_at(corners[0], 9_000.0) && all_at(corners[3], 11_000.0));
        assert_eq!(corners[1].values[0].si.to_bits(), 11_000.0_f64.to_bits());
        assert_eq!(corners[1].values[1].si.to_bits(), 9_000.0_f64.to_bits());
        assert_eq!(plans.len() - corners.len(), interior_probe_count(2));
        let mut seeds: Vec<u32> = plans.iter().map(|p| p.seed).collect();
        seeds.sort_unstable();
        seeds.dedup();
        assert_eq!(seeds.len(), plans.len(), "member indices are unique");

        let mc = build_plans(Mode::MonteCarlo, 5, &ts).unwrap();
        assert_eq!(mc.len(), 5);
        assert!(mc.iter().all(|p| !p.interior));
    }

    /// Latin hypercube, both halves: every stratum of every component is hit
    /// exactly once, and the axes are permuted independently rather than
    /// sharing one order. Independent permutations can coincide by chance;
    /// the off-diagonal check is a property of THIS fixture, asserted because
    /// a regression collapsing the axes onto one order shows up as a
    /// permanent diagonal.
    #[test]
    // A unit fraction in (0, 1) times six probes: the stratum index is small and non-negative.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn interior_probes_cover_every_stratum_and_are_not_diagonal() {
        let ts = vec![
            rule("R1", 1_000.0, 10.0, Distribution::Uniform),
            rule("R2", 1_000.0, 10.0, Distribution::Uniform),
        ];
        let probes = interior_probe_count(2);
        let plans = build_plans(Mode::Corners, 0, &ts).unwrap();
        let interior: Vec<&SeedPlan> = plans.iter().filter(|p| p.interior).collect();
        let stratum = |v: &SampledValue| -> usize {
            let unit = (v.si / v.nominal_si - 1.0) / 0.10; // in (-1, +1)
            (f64::midpoint(unit, 1.0) * f64::from(u8::try_from(probes).unwrap())).floor() as usize
        };
        for i in 0..2 {
            let mut hit: Vec<usize> = interior.iter().map(|p| stratum(&p.values[i])).collect();
            hit.sort_unstable();
            assert_eq!(hit, (0..probes).collect::<Vec<_>>(), "component {i}");
        }
        assert!(
            !interior
                .iter()
                .all(|p| stratum(&p.values[0]) == stratum(&p.values[1])),
            "the axes must be permuted independently"
        );
    }

    /// The probe count flattens at 8 rather than scaling with the 2^n corner
    /// set: the monotonicity check is an addend, not a multiplier.
    #[test]
    fn the_interior_probe_count_is_bounded() {
        assert_eq!(interior_probe_count(1), 4);
        assert_eq!(interior_probe_count(2), 6);
        for n in 3..=CORNER_CAP {
            assert_eq!(interior_probe_count(n), 8, "n = {n}");
        }
        assert!(interior_probe_count(CORNER_CAP) * 100 < 1usize << CORNER_CAP);
    }

    /// Every seed runs every variant combination, and a sweep past the cap is refused.
    #[test]
    fn variant_axes_cross_every_plan_within_the_cap() {
        use std::collections::BTreeSet;
        let mk = |r: &str, ids: &str| (r.to_string(), ids.split(' ').map(String::from).collect());
        let axes: [(String, Vec<String>); 2] = [mk("U3", "a b"), mk("U4", "x y z")];
        let plans = build_plans(Mode::MonteCarlo, 2, &[]).unwrap();
        let swept = with_variants(plans.clone(), &axes).unwrap();
        let runs: BTreeSet<_> = swept.iter().map(|p| (p.seed, &p.variants)).collect();
        assert_eq!((swept.len(), runs.len()), (12, 12), "{runs:?}");
        let fifth = format!("{:?}", swept[4].variants);
        assert_eq!(fifth, r#"[("U3", "b"), ("U4", "y")]"#);
        let wide = [("U3".to_string(), (0..1025).map(|i| i.to_string()).collect())];
        assert!(with_variants(plans, &wide).is_none());
    }

    fn open(reference: &str, ids: &[&str], corners: bool) -> OpenSet {
        OpenSet {
            reference: reference.into(),
            ids: ids.iter().map(ToString::to_string).collect(),
            corners,
        }
    }

    /// `n` LED-like corner sets (high-Vf stand-in first), refs D1..Dn.
    fn leds(n: usize) -> Vec<OpenSet> {
        (1..=n)
            .map(|i| open(&format!("D{i}"), &["led_high_vf", "led_low_vf"], true))
            .collect()
    }

    /// Under the cap every combination of every set runs and nothing is
    /// disclosed: the sweep claims what it did, so it needs no condition.
    #[test]
    fn open_sets_within_the_cap_are_crossed_in_full_with_no_condition() {
        let plans = build_plans(Mode::MonteCarlo, 4, &[]).unwrap();
        let mut sets = leds(3);
        sets.push(open("U3", &["a", "b", "c"], false));
        let swept = sweep_open_sets(plans, &sets);
        assert_eq!(swept.plans.len(), 4 * 8 * 3);
        assert!(swept.coverage.is_full(), "{:?}", swept.coverage);
        assert!(swept.coverage.conditions().is_empty());
        let distinct: std::collections::BTreeSet<_> =
            swept.plans.iter().map(|p| (p.seed, &p.variants)).collect();
        assert_eq!(distinct.len(), swept.plans.len());
    }

    /// Six LED sets and 24 seeds is 1536 members, over the cap: the corner
    /// sets get the linear cover, which has the all-weak and all-strong
    /// members, every part alone at each corner against the rest at either
    /// extreme, and the run says it was not every combination.
    #[test]
    fn corner_sets_past_the_cap_get_the_linear_cover_and_say_so() {
        fn pins(p: &SeedPlan) -> Vec<&str> {
            p.variants.iter().map(|(_, id)| id.as_str()).collect()
        }
        let plans = build_plans(Mode::MonteCarlo, 24, &[]).unwrap();
        let swept = sweep_open_sets(plans, &leds(6));
        // 2 uniform members, then per set one corner alone against the rest at
        // each extreme (the other two combinations are the uniform members).
        let per_seed = 2 + 2 * 6;
        assert_eq!(swept.plans.len(), 24 * per_seed);
        assert_eq!(swept.coverage.corners, CornerCover::Boundary);
        let seed0: Vec<Vec<&str>> = swept
            .plans
            .iter()
            .filter(|p| p.seed == 0)
            .map(pins)
            .collect();
        let uniform = |id: &'static str| vec![id; 6];
        assert!(seed0.contains(&uniform("led_high_vf")), "stand-in member");
        assert!(seed0.contains(&uniform("led_low_vf")), "all strong");
        // D3 alone low against every other part high, and alone high against
        // every other part low.
        let mut low_alone = uniform("led_high_vf");
        low_alone[2] = "led_low_vf";
        let mut high_alone = uniform("led_low_vf");
        high_alone[2] = "led_high_vf";
        assert!(seed0.contains(&low_alone) && seed0.contains(&high_alone));
        let said = swept.coverage.conditions();
        assert_eq!(said.len(), 1, "{said:?}");
        for needle in [
            "D1, D2, D3, D4, D5, D6",
            "all weak, all strong",
            "not in every combination",
            "1536 runs",
        ] {
            assert!(said[0].contains(needle), "{needle}: {}", said[0]);
        }
    }

    /// When even the linear cover does not fit the budget the sets are swept
    /// jointly; when nothing fits they keep the stand-in and the condition
    /// says that, naming it, instead of passing as if they had been swept.
    #[test]
    fn corner_sets_degrade_to_joint_then_to_the_named_stand_in() {
        let plans = build_plans(Mode::MonteCarlo, 24, &[]).unwrap();
        let joint = sweep_open_sets(plans, &leds(30));
        assert_eq!(joint.coverage.corners, CornerCover::Joint);
        assert_eq!(joint.plans.len(), 24 * 2);
        assert!(
            joint.coverage.conditions()[0].contains("every part at one assumed corner together")
        );
        let plans = build_plans(Mode::MonteCarlo, 600, &[]).unwrap();
        let stand_in = sweep_open_sets(plans, &leds(2));
        assert_eq!(stand_in.coverage.corners, CornerCover::StandIn);
        assert_eq!(
            stand_in.plans.len(),
            600,
            "nothing pinned: the stand-in runs"
        );
        assert!(stand_in.plans.iter().all(|p| p.variants.is_empty()));
        let said = stand_in.coverage.conditions().join(" ");
        assert!(
            said.contains("not swept") && said.contains("led_high_vf"),
            "{said}"
        );
    }

    /// An orderable-variant set is unordered, so no subset stands for the
    /// rest: past the cap it keeps its stand-in and is named, while the corner
    /// sets still get their cover.
    #[test]
    fn variant_sets_past_the_cap_are_named_not_sampled() {
        let plans = build_plans(Mode::MonteCarlo, 2, &[]).unwrap();
        let wide: Vec<String> = (0..600).map(|i| format!("v{i}")).collect();
        let wide: Vec<&str> = wide.iter().map(String::as_str).collect();
        let mut sets = vec![open("U1", &wide, false)];
        sets.extend(leds(2));
        let swept = sweep_open_sets(plans, &sets);
        assert_eq!(swept.coverage.variant_refs, ["U1"]);
        assert_eq!(swept.coverage.corners, CornerCover::Full);
        assert_eq!(swept.plans.len(), 2 * 4);
        assert!(swept
            .plans
            .iter()
            .all(|p| p.variants.iter().all(|(r, _)| r != "U1")));
        let said = swept.coverage.conditions();
        assert_eq!(said.len(), 1);
        assert!(
            said[0].contains("U1") && said[0].contains("stand-in variant (v0)"),
            "{said:?}"
        );
    }

    /// `below` must be free of the modulo bias `next_u64() % n` carries: over
    /// many draws every residue appears within a few percent of its share.
    #[test]
    fn below_is_uniform_over_its_range() {
        for n in [3u32, 5, 6, 7, 8] {
            let mut rng = SplitMix::for_component(1, "uniformity");
            let draws = 120_000u32;
            let mut counts = vec![0u32; n as usize];
            for _ in 0..draws {
                let v = rng.below(u64::from(n));
                assert!(v < u64::from(n), "below({n}) returned {v}");
                counts[usize::try_from(v).unwrap()] += 1;
            }
            let expected = f64::from(draws) / f64::from(n);
            for (r, c) in counts.iter().enumerate() {
                let dev = (f64::from(*c) - expected).abs() / expected;
                assert!(
                    dev < 0.05,
                    "n = {n}, residue {r}: {c} draws, {:.1}% off",
                    dev * 100.0
                );
            }
        }
    }

    #[test]
    fn corner_cap_refuses_and_names_monte_carlo_without_overflowing() {
        // 64 tolerances would overflow a `1u64 << n` corner count; the
        // refusal must still come out cleanly with the component count.
        for n in [CORNER_CAP + 1, 64] {
            let ts: Vec<ResolvedTolerance> = (0..n)
                .map(|i| rule(&format!("R{i}"), 1_000.0, 5.0, Distribution::Uniform))
                .collect();
            let msg = build_plans(Mode::Corners, 0, &ts).unwrap_err().to_string();
            assert!(
                msg.contains("monte-carlo") && msg.contains(&n.to_string()),
                "{msg}"
            );
        }
    }

    #[test]
    fn glob_match_supports_star_only() {
        assert!(glob_match("R*", "R17") && glob_match("R*", "R") && !glob_match("R*", "C3"));
        assert!(glob_match("*", "anything"));
        assert!(glob_match("R1", "R1") && !glob_match("R1", "R12"));
    }
}
