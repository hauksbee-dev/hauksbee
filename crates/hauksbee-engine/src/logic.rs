//! Generic evaluator for declarative digital logic (`[models.logic]`).
//! Long-form how-and-why: docs/how-and-why/hauksbee-engine/digital.md
//! (the digital-domain essay) and docs/how-and-why/hauksbee-models/logic_spec.md
//! (the format and the byte-exact migration record).
//!
//! One [`LogicComponent`] replaces the old `DigitalKind` enum: a digital
//! part's behaviour arrives as data (a validated
//! [`hauksbee_models::logic_spec::Logic`] block) and is COMPILED once at bind
//! time into pin index tables, `u64` register state, and evalexpr operator
//! trees, then evaluated per tick with no parsing and no per-tick
//! allocation of program structure. Ticks happen on every simulated edge in
//! the replay paths, so bind-time compilation is a real performance
//! requirement, not a nicety.
//!
//! ## Expression compilation
//!
//! The spec's boolean grammar (digit-led pin names like `1a`, `!`/`&`/`^`/`|`,
//! `reg[bit]` references) is not evalexpr syntax, so the validated
//! [`LogicExpr`] AST is lowered into a fully parenthesized evalexpr source
//! string over sanitized boolean variables (`v_<name>` for pins/outputs,
//! `b_<reg>_<bit>` for register bits; full parenthesization makes operator
//! precedence a non-issue) and compiled with `build_operator_tree`; the same
//! pattern `register_map.rs` uses for sensor value expressions.
//!
//! ## Evaluation order and combinational cycles
//!
//! Outputs are evaluated in dependency (topological) order. Outputs forming a
//! strongly connected component, a genuine combinational cycle, e.g. the
//! cross-coupled NOR SR latch, are resolved by Gauss–Seidel fixpoint
//! iteration in `outputs`-declaration order (the spec's declared resolution
//! order), seeded from the previous stable levels (`init` at power-on).
//!
//! The iteration bound is [`COMB_FIXPOINT_BOUND`] sweeps. The bound is not a
//! correctness knob: at compile time every SCC is checked EXHAUSTIVELY, all
//! `2^m` assignments of its external inputs crossed with all `2^k` seed
//! states of its members, and a spec whose cycle fails to settle within the
//! bound for any combination is REFUSED with a named error and a concrete
//! witness (`y = !y` is the canonical refusal). A cycle too wide to enumerate
//! (`m + k >` [`CONVERGENCE_ENUM_CAP`]) is likewise refused rather than
//! shipped unverified. Runtime non-convergence is therefore unreachable for a
//! compiled spec; the runtime bound remains as defense in depth and screams
//! on stderr if it ever trips.
//!
//! Gauss–Seidel (not Jacobi) is deliberate: sequential sweeps in declared
//! order settle every physical latch deterministically from any seed, whereas
//! simultaneous updates oscillate forever from the symmetric seeds (the
//! classic SR metastability). The declared order IS the tie-break the plan
//! calls "a declared resolution order".
//!
//! ## Register semantics
//!
//! See the `logic_spec` module docs for the full contract. In short: per tick
//! every register computes its next value from the PRE-tick state with
//! priority reset > async load > clock edge (matching the 74HC595 SRCLR and
//! 74HC165 PL datasheet rows), then all registers commit simultaneously,
//! tied shift+latch clocks behave like real silicon (store one clock behind
//! shift).
//!
//! ## Unwired pins
//!
//! An unwired input reads LOW in expressions and as data/clock. An unwired
//! CONTROL pin reads the level that keeps the part operating normally:
//! resets and loads released, clocks enabled, tri-states driving. (A board
//! that ties these does so in copper; a spec input left unwired must not
//! freeze the part.) This matches the old hardcoded parts' behaviour, an
//! unwired SRCLR_n read released, an unwired OE_n stayed enabled, with one
//! documented exception: the old `tick_165` treated an unwired PL_n as LOW
//! (load permanently transparent), which was a modeling artifact, not
//! silicon; here an unwired PL_n reads released like every other active-low
//! control.

use std::collections::HashMap;

use evalexpr::{
    build_operator_tree, ContextWithMutableVariables, DefaultNumericTypes, HashMapContext,
    Node as EvalNode, Value,
};
use hauksbee_models::logic_spec::{
    Edge, Level, Logic, LogicExpr, LogicSpecError, RegisterOp,
};

/// Fixpoint sweep bound for combinational cycles. Part of the semantics
/// contract: the compile-time exhaustive check verifies every SCC settles
/// within this many Gauss–Seidel sweeps for every input/seed combination.
/// Physical latch structures settle in ≤ 2 sweeps; 16 leaves generous slack
/// for legitimate wider cycles without letting a pathological spec spin.
pub const COMB_FIXPOINT_BOUND: usize = 16;

/// Maximum `external inputs + cycle members` the exhaustive convergence check
/// will enumerate (2^cap evaluations). A cycle wider than this is refused
/// rather than shipped unverified, refuse-rather-than-fake.
pub const CONVERGENCE_ENUM_CAP: u32 = 12;

/// A named logic-compilation failure (everything `logic_spec` validation
/// cannot know statically, plus the pass-through of validation itself).
#[derive(Debug, thiserror::Error)]
pub enum LogicCompileError {
    #[error("logic spec: {0}")]
    Spec(#[from] LogicSpecError),

    #[error(
        "combinational cycle through {outputs:?} does not converge within \
         {bound} fixpoint sweeps for {witness}; a comb network that cannot \
         settle is refused, not approximated"
    )]
    NonConvergent {
        outputs: Vec<String>,
        bound: usize,
        witness: String,
    },

    #[error(
        "combinational cycle through {outputs:?} has {external} external \
         input(s) and {members} member(s): {total} > {cap} enumeration bits, \
         too wide to verify convergence exhaustively — refused rather than \
         shipped unverified"
    )]
    TooWideToVerify {
        outputs: Vec<String>,
        external: u32,
        members: u32,
        total: u32,
        cap: u32,
    },

    /// The AST-to-evalexpr lowering produced something evalexpr rejected.
    /// Structurally unreachable (the lowering emits only boolean operators
    /// over identifiers); kept loud instead of unwrapped.
    #[error("internal: lowered expression for '{output}' failed to compile: {message}")]
    Lowering { output: String, message: String },
}

/// Where a register's serial/parallel data comes from (resolved indices).
#[derive(Debug, Clone, Copy)]
enum DataSrc {
    Pin(usize),
    Reg(usize),
}

/// One compiled register.
#[derive(Debug, Clone)]
struct CompReg {
    name: String,
    bits: u32,
    mask: u64,
    state: u64,
    /// (input pin index, edge), plus the previous clock level for edge detect.
    clock: Option<(usize, Edge)>,
    prev_clock: bool,
    /// (input pin index, active level, reset value), first active wins.
    resets: Vec<(usize, Level, u64)>,
    op: Option<RegisterOp>,
    data_in: Option<DataSrc>,
    /// (input pin index, active level, data pin index per bit).
    load: Option<(usize, Level, Vec<usize>)>,
    clock_enable: Option<(usize, Level)>,
}

/// One compiled combinational output.
#[derive(Debug)]
struct CompComb {
    /// Index into `outputs`.
    out: usize,
    node: EvalNode<DefaultNumericTypes>,
}

/// One evaluation-plan step: a single acyclic output, or a strongly connected
/// component resolved by fixpoint iteration (members in declaration order).
#[derive(Debug)]
enum PlanStep {
    Single(usize),
    Scc(Vec<usize>),
}

/// A compiled tri-state group.
#[derive(Debug, Clone)]
struct CompTristate {
    /// Output indices gated by this group.
    outputs: Vec<usize>,
    /// Input pin index of the enable.
    enable: usize,
    active: Level,
}

/// One bound, compiled `[models.logic]` part: pin tables, register state,
/// compiled expressions. Pure logic; it computes LEVELS; the caller owns
/// voltage thresholds, net wiring, and drivers.
#[derive(Debug)]
pub struct LogicComponent {
    /// The model id / spec name, for diagnostics.
    spec_id: String,
    /// Declared input pin names (index = pin id used everywhere below).
    input_names: Vec<String>,
    /// Declared output names in declaration order.
    output_names: Vec<String>,
    /// Last decided level per input (hysteresis memory for the caller's
    /// threshold decision, and the value expressions read).
    input_levels: Vec<bool>,
    /// The level an UNWIRED instance of each input reads (see module docs).
    input_defaults: Vec<bool>,
    /// Current output levels.
    output_levels: Vec<bool>,
    /// Current output drive enables (false = tri-stated).
    output_enabled: Vec<bool>,
    registers: Vec<CompReg>,
    comb: Vec<CompComb>,
    plan: Vec<PlanStep>,
    tristates: Vec<CompTristate>,
    /// evalexpr variable names, precomputed: `v_<input>` / `v_<output>`.
    input_vars: Vec<String>,
    output_vars: Vec<String>,
    /// Register bits referenced by any expression: (reg index, bit, var name).
    reg_bit_vars: Vec<(usize, u32, String)>,
    /// Reused evaluation context (keys are stable; values overwritten per tick).
    ctx: HashMapContext<DefaultNumericTypes>,
    /// Cycle warnings raised at validation (surfaced by lint / bind).
    pub warnings: Vec<String>,
}

/// Lower a validated AST into fully parenthesized evalexpr source over
/// sanitized boolean variables.
fn lower_expr(e: &LogicExpr, out: &mut String) {
    match e {
        LogicExpr::Const(true) => out.push_str("true"),
        LogicExpr::Const(false) => out.push_str("false"),
        LogicExpr::Name(n) => {
            out.push_str("v_");
            out.push_str(n);
        }
        LogicExpr::Bit(n, i) => {
            out.push_str(&format!("b_{n}_{i}"));
        }
        LogicExpr::Not(a) => {
            out.push_str("(!");
            lower_expr(a, out);
            out.push(')');
        }
        LogicExpr::And(a, b) => {
            out.push('(');
            lower_expr(a, out);
            out.push_str(" && ");
            lower_expr(b, out);
            out.push(')');
        }
        LogicExpr::Or(a, b) => {
            out.push('(');
            lower_expr(a, out);
            out.push_str(" || ");
            lower_expr(b, out);
            out.push(')');
        }
        // Boolean XOR: evalexpr has no `^`, but `!=` on booleans is exactly it.
        LogicExpr::Xor(a, b) => {
            out.push('(');
            lower_expr(a, out);
            out.push_str(" != ");
            lower_expr(b, out);
            out.push(')');
        }
    }
}

/// Tarjan strongly-connected components over the comb dependency graph
/// (`deps[i]` = output indices output `i` references). Returns SCCs in
/// TOPOLOGICAL order (dependencies before dependents).
fn tarjan_sccs(deps: &[Vec<usize>]) -> Vec<Vec<usize>> {
    let n = deps.len();
    let mut index = vec![usize::MAX; n];
    let mut low = vec![0usize; n];
    let mut on_stack = vec![false; n];
    let mut stack: Vec<usize> = Vec::new();
    let mut next_index = 0usize;
    let mut sccs: Vec<Vec<usize>> = Vec::new();

    // Iterative Tarjan (explicit frame stack: node, child cursor).
    for start in 0..n {
        if index[start] != usize::MAX {
            continue;
        }
        let mut frames: Vec<(usize, usize)> = vec![(start, 0)];
        while let Some(&mut (v, ref mut ci)) = frames.last_mut() {
            if *ci == 0 {
                index[v] = next_index;
                low[v] = next_index;
                next_index += 1;
                stack.push(v);
                on_stack[v] = true;
            }
            if *ci < deps[v].len() {
                let w = deps[v][*ci];
                *ci += 1;
                if index[w] == usize::MAX {
                    frames.push((w, 0));
                } else if on_stack[w] {
                    low[v] = low[v].min(index[w]);
                }
            } else {
                frames.pop();
                if let Some(&mut (p, _)) = frames.last_mut() {
                    low[p] = low[p].min(low[v]);
                }
                if low[v] == index[v] {
                    let mut scc = Vec::new();
                    while let Some(w) = stack.pop() {
                        on_stack[w] = false;
                        scc.push(w);
                        if w == v {
                            break;
                        }
                    }
                    sccs.push(scc);
                }
            }
        }
    }
    // Tarjan emits SCCs in reverse topological order.
    sccs.reverse();
    sccs
}

impl LogicComponent {
    /// Compile a validated `[models.logic]` block. Runs `Logic::validate`,
    /// lowers and compiles every expression, builds the evaluation plan, and
    /// exhaustively verifies fixpoint convergence for every combinational
    /// cycle. All failures are named.
    pub fn compile(spec_id: &str, logic: &Logic) -> Result<Self, LogicCompileError> {
        let validated = logic.validate()?;

        let input_names: Vec<String> = logic.inputs.clone();
        let output_names: Vec<String> = logic.outputs.clone();
        let input_idx: HashMap<&str, usize> = input_names
            .iter()
            .enumerate()
            .map(|(i, n)| (n.as_str(), i))
            .collect();
        let output_idx: HashMap<&str, usize> = output_names
            .iter()
            .enumerate()
            .map(|(i, n)| (n.as_str(), i))
            .collect();
        let reg_idx: HashMap<&str, usize> = logic
            .registers
            .iter()
            .enumerate()
            .map(|(i, r)| (r.name.as_str(), i))
            .collect();

        // ── Registers ──
        let registers: Vec<CompReg> = logic
            .registers
            .iter()
            .map(|r| {
                let mask = if r.bits == 64 { u64::MAX } else { (1u64 << r.bits) - 1 };
                CompReg {
                    name: r.name.clone(),
                    bits: r.bits,
                    mask,
                    state: r.init & mask,
                    clock: r.clock.as_ref().map(|c| (input_idx[c.pin.as_str()], c.edge)),
                    prev_clock: false,
                    resets: r
                        .resets
                        .iter()
                        .map(|rst| (input_idx[rst.pin.as_str()], rst.active, rst.value))
                        .collect(),
                    op: r.op,
                    data_in: r.data_in.as_ref().map(|d| {
                        if let Some(&ri) = reg_idx.get(d.as_str()) {
                            DataSrc::Reg(ri)
                        } else {
                            DataSrc::Pin(input_idx[d.as_str()])
                        }
                    }),
                    load: r.load.as_ref().map(|l| {
                        (
                            input_idx[l.pin.as_str()],
                            l.active,
                            l.data.iter().map(|p| input_idx[p.as_str()]).collect(),
                        )
                    }),
                    clock_enable: r
                        .clock_enable
                        .as_ref()
                        .map(|e| (input_idx[e.pin.as_str()], e.active)),
                }
            })
            .collect();

        // ── Comb compilation ──
        let mut comb: Vec<CompComb> = Vec::with_capacity(validated.comb.len());
        let mut deps: Vec<Vec<usize>> = vec![Vec::new(); output_names.len()];
        let mut reg_bit_vars: Vec<(usize, u32, String)> = Vec::new();
        let mut seen_bits: std::collections::HashSet<(usize, u32)> = std::collections::HashSet::new();
        for (out_name, ast) in &validated.comb {
            let out = output_idx[out_name.as_str()];
            let mut src = String::new();
            lower_expr(ast, &mut src);
            let node = build_operator_tree::<DefaultNumericTypes>(&src).map_err(|e| {
                LogicCompileError::Lowering {
                    output: out_name.clone(),
                    message: e.to_string(),
                }
            })?;
            let mut names = Vec::new();
            let mut bits = Vec::new();
            ast.collect_refs(&mut names, &mut bits);
            for n in names {
                if let Some(&oi) = output_idx.get(n) {
                    deps[out].push(oi);
                }
            }
            for (n, i) in bits {
                let ri = reg_idx[n];
                if seen_bits.insert((ri, i)) {
                    reg_bit_vars.push((ri, i, format!("b_{n}_{i}")));
                }
            }
            comb.push(CompComb { out, node });
        }
        // comb is in outputs-declaration order (validate() guarantees it), so
        // comb[i].out == i; keep the assumption checked.
        debug_assert!(comb.iter().enumerate().all(|(i, c)| c.out == i));

        // ── Evaluation plan: SCCs in topological order ──
        let plan: Vec<PlanStep> = tarjan_sccs(&deps)
            .into_iter()
            .map(|mut scc| {
                if scc.len() == 1 && !deps[scc[0]].contains(&scc[0]) {
                    PlanStep::Single(scc[0])
                } else {
                    // Declaration order within the SCC = resolution order.
                    scc.sort_unstable();
                    PlanStep::Scc(scc)
                }
            })
            .collect();

        // ── Tri-state groups ──
        let mut tristates: Vec<CompTristate> = Vec::new();
        for (group, ts) in &logic.tristate {
            let outs = logic.expand_tristate_group(group)?;
            tristates.push(CompTristate {
                outputs: outs.iter().map(|o| output_idx[o.as_str()]).collect(),
                enable: input_idx[ts.enable.as_str()],
                active: ts.active,
            });
        }

        // ── Unwired-pin defaults (see module docs) ──
        let mut input_defaults = vec![false; input_names.len()];
        for r in &registers {
            for &(pin, active, _) in &r.resets {
                // Released = inactive.
                input_defaults[pin] = match active {
                    Level::Low => true,
                    Level::High => false,
                };
            }
            if let Some((pin, active, _)) = &r.load {
                input_defaults[*pin] = match active {
                    Level::Low => true,
                    Level::High => false,
                };
            }
            if let Some((pin, active)) = r.clock_enable {
                // Clock enabled = active.
                input_defaults[pin] = match active {
                    Level::Low => false,
                    Level::High => true,
                };
            }
        }
        for ts in &tristates {
            // Outputs driven = active.
            input_defaults[ts.enable] = match ts.active {
                Level::Low => false,
                Level::High => true,
            };
        }

        // ── Initial output levels (power-on) ──
        let mut output_levels = vec![false; output_names.len()];
        for (name, &v) in &logic.init {
            output_levels[output_idx[name.as_str()]] = v != 0;
        }

        let input_vars: Vec<String> = input_names.iter().map(|n| format!("v_{n}")).collect();
        let output_vars: Vec<String> = output_names.iter().map(|n| format!("v_{n}")).collect();

        let mut lc = LogicComponent {
            spec_id: spec_id.to_string(),
            input_levels: input_defaults.clone(),
            input_defaults,
            output_enabled: vec![true; output_names.len()],
            input_names,
            output_names,
            output_levels,
            registers,
            comb,
            plan,
            tristates,
            input_vars,
            output_vars,
            reg_bit_vars,
            ctx: HashMapContext::new(),
            warnings: validated.warnings,
        };
        lc.check_convergence()?;
        Ok(lc)
    }

    /// Exhaustively verify that every combinational cycle settles within
    /// [`COMB_FIXPOINT_BOUND`] Gauss–Seidel sweeps for EVERY assignment of
    /// its external inputs and EVERY seed state of its members. Refuses
    /// (named error) when it cannot verify.
    fn check_convergence(&mut self) -> Result<(), LogicCompileError> {
        // Collect the plan's SCC steps first (borrow gymnastics: we mutate ctx
        // during evaluation).
        let scc_steps: Vec<Vec<usize>> = self
            .plan
            .iter()
            .filter_map(|s| match s {
                PlanStep::Scc(m) => Some(m.clone()),
                PlanStep::Single(_) => None,
            })
            .collect();

        for members in scc_steps {
            // External variables: everything the member expressions read that
            // is not itself a member. Enumerating non-member OUTPUTS as free
            // bits over-approximates reachable states, safe: convergence
            // for a superset of states implies it for the reachable set.
            let member_set: std::collections::HashSet<usize> = members.iter().copied().collect();
            let mut externals: Vec<String> = Vec::new();
            let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
            for &mi in &members {
                let node = &self.comb[mi].node;
                for var in node.iter_variable_identifiers() {
                    let is_member = self
                        .output_vars
                        .iter()
                        .enumerate()
                        .any(|(oi, ov)| ov == var && member_set.contains(&oi));
                    if !is_member && seen.insert(var.to_string()) {
                        externals.push(var.to_string());
                    }
                }
            }
            let m = externals.len() as u32;
            let k = members.len() as u32;
            let names: Vec<String> = members
                .iter()
                .map(|&i| self.output_names[i].clone())
                .collect();
            if m + k > CONVERGENCE_ENUM_CAP {
                return Err(LogicCompileError::TooWideToVerify {
                    outputs: names,
                    external: m,
                    members: k,
                    total: m + k,
                    cap: CONVERGENCE_ENUM_CAP,
                });
            }

            for ext_bits in 0u64..(1u64 << m) {
                for (i, var) in externals.iter().enumerate() {
                    let v = (ext_bits >> i) & 1 == 1;
                    let _ = self.ctx.set_value(var.clone(), Value::Boolean(v));
                }
                for seed_bits in 0u64..(1u64 << k) {
                    let mut levels: Vec<bool> = (0..k as usize)
                        .map(|i| (seed_bits >> i) & 1 == 1)
                        .collect();
                    for (i, &mi) in members.iter().enumerate() {
                        let _ = self
                            .ctx
                            .set_value(self.output_vars[mi].clone(), Value::Boolean(levels[i]));
                    }
                    let mut settled = false;
                    for _ in 0..COMB_FIXPOINT_BOUND {
                        let mut changed = false;
                        for (i, &mi) in members.iter().enumerate() {
                            let v = self.comb[mi]
                                .node
                                .eval_boolean_with_context(&self.ctx)
                                .unwrap_or(false);
                            if v != levels[i] {
                                levels[i] = v;
                                changed = true;
                                let _ = self.ctx.set_value(
                                    self.output_vars[mi].clone(),
                                    Value::Boolean(v),
                                );
                            }
                        }
                        if !changed {
                            settled = true;
                            break;
                        }
                    }
                    if !settled {
                        let ext_desc: Vec<String> = externals
                            .iter()
                            .enumerate()
                            .map(|(i, v)| format!("{v}={}", (ext_bits >> i) & 1))
                            .collect();
                        return Err(LogicCompileError::NonConvergent {
                            outputs: names,
                            bound: COMB_FIXPOINT_BOUND,
                            witness: format!(
                                "externals [{}], seed {:#b}",
                                ext_desc.join(", "),
                                seed_bits
                            ),
                        });
                    }
                }
            }
        }
        Ok(())
    }

    /// The model id / spec name this component was compiled from.
    pub fn spec_id(&self) -> &str {
        &self.spec_id
    }

    /// Declared input pin names.
    pub fn input_names(&self) -> &[String] {
        &self.input_names
    }

    /// Declared output names, in declaration order.
    pub fn output_names(&self) -> &[String] {
        &self.output_names
    }

    /// Does the spec declare this input pin?
    pub fn has_input(&self, name: &str) -> bool {
        self.input_names.iter().any(|n| n == name)
    }

    /// Does the spec declare this output?
    pub fn has_output(&self, name: &str) -> bool {
        self.output_names.iter().any(|n| n == name)
    }

    /// True when the part has at least one clocked register; the parts whose
    /// sub-chunk pulse trains demand edge-granular replay.
    pub fn is_sequential(&self) -> bool {
        self.registers.iter().any(|r| r.clock.is_some())
    }

    /// Input pins that participate in sequential behaviour (register clocks,
    /// resets, loads + load data, enables, serial data): the pins whose edge
    /// timing matters. Used by the scheduler to decide edge-replay membership.
    pub fn sequential_pins(&self) -> Vec<&str> {
        let mut pins: Vec<usize> = Vec::new();
        for r in &self.registers {
            if let Some((p, _)) = r.clock {
                pins.push(p);
            }
            for &(p, _, _) in &r.resets {
                pins.push(p);
            }
            if let Some((p, _, data)) = &r.load {
                pins.push(*p);
                pins.extend(data.iter().copied());
            }
            if let Some((p, _)) = r.clock_enable {
                pins.push(p);
            }
            if let Some(DataSrc::Pin(p)) = r.data_in {
                pins.push(p);
            }
        }
        pins.sort_unstable();
        pins.dedup();
        pins.into_iter()
            .map(|p| self.input_names[p].as_str())
            .collect()
    }

    /// Current level of an input pin (the last decided sample).
    pub fn input_level(&self, name: &str) -> Option<bool> {
        let i = self.input_names.iter().position(|n| n == name)?;
        Some(self.input_levels[i])
    }

    /// Current logic level of an output.
    pub fn output_level(&self, name: &str) -> Option<bool> {
        let i = self.output_names.iter().position(|n| n == name)?;
        Some(self.output_levels[i])
    }

    /// Is an output currently driving (not tri-stated)?
    pub fn output_enabled(&self, name: &str) -> Option<bool> {
        let i = self.output_names.iter().position(|n| n == name)?;
        Some(self.output_enabled[i])
    }

    /// All outputs: `(name, level, enabled)`.
    pub fn outputs(&self) -> impl Iterator<Item = (&str, bool, bool)> {
        self.output_names
            .iter()
            .enumerate()
            .map(|(i, n)| (n.as_str(), self.output_levels[i], self.output_enabled[i]))
    }

    /// Current value of a register.
    pub fn register(&self, name: &str) -> Option<u64> {
        self.registers
            .iter()
            .find(|r| r.name == name)
            .map(|r| r.state)
    }

    /// All registers: `(name, value)`.
    pub fn registers(&self) -> impl Iterator<Item = (&str, u64)> {
        self.registers.iter().map(|r| (r.name.as_str(), r.state))
    }

    /// Declared width of a register, if the spec has one by this name.
    pub fn register_bits(&self, name: &str) -> Option<u32> {
        self.registers
            .iter()
            .find(|r| r.name == name)
            .map(|r| r.bits)
    }

    /// Overwrite a register's value (the chain-controller mirror / test-latch
    /// path). Returns false when the spec has no such register. Callers that
    /// need the outputs to reflect the new state must call
    /// [`LogicComponent::refresh_outputs`] afterwards.
    pub fn set_register(&mut self, name: &str, value: u64) -> bool {
        match self.registers.iter_mut().find(|r| r.name == name) {
            Some(r) => {
                r.state = value & r.mask;
                true
            }
            None => false,
        }
    }

    /// One evaluation step. `sample(name, prev)` returns the decided level of
    /// a WIRED input pin given its previous level (the caller applies its
    /// voltage thresholds + hysteresis), or `None` for an unwired pin (which
    /// then reads its default, see module docs).
    pub fn tick(&mut self, sample: &mut dyn FnMut(&str, bool) -> Option<bool>) {
        // 1. Sample every input.
        for i in 0..self.input_names.len() {
            let prev = self.input_levels[i];
            self.input_levels[i] = match sample(&self.input_names[i], prev) {
                Some(level) => level,
                None => self.input_defaults[i],
            };
        }

        // 2. Registers: next values from the PRE-tick state, then commit
        //    simultaneously (tied-clock silicon semantics).
        let old_state: Vec<u64> = self.registers.iter().map(|r| r.state).collect();
        for ri in 0..self.registers.len() {
            let (cur_clock, next) = {
                let r = &self.registers[ri];
                let cur_clock = r
                    .clock
                    .map(|(p, _)| self.input_levels[p])
                    .unwrap_or(false);
                let edge_fired = match r.clock {
                    Some((_, Edge::Rising)) => cur_clock && !r.prev_clock,
                    Some((_, Edge::Falling)) => !cur_clock && r.prev_clock,
                    None => false,
                };
                let enabled = match r.clock_enable {
                    Some((p, active)) => active.is_active(self.input_levels[p]),
                    None => true,
                };
                // Priority: reset > load > clock edge > hold.
                let mut next = r.state;
                let active_reset = r
                    .resets
                    .iter()
                    .find(|&&(p, active, _)| active.is_active(self.input_levels[p]));
                if let Some(&(_, _, value)) = active_reset {
                    next = value & r.mask;
                } else if let Some((p, active, data)) = &r.load {
                    if active.is_active(self.input_levels[*p]) {
                        let mut v = 0u64;
                        for (bit, &pin) in data.iter().enumerate() {
                            if self.input_levels[pin] {
                                v |= 1u64 << bit;
                            }
                        }
                        next = v;
                    } else if edge_fired && enabled {
                        next = self.apply_op(r, &old_state);
                    }
                } else if edge_fired && enabled {
                    next = self.apply_op(r, &old_state);
                }
                (cur_clock, next)
            };
            let r = &mut self.registers[ri];
            r.prev_clock = cur_clock;
            r.state = next & r.mask;
        }

        // 3+4. Comb evaluation + tri-state.
        self.refresh_outputs();
    }

    /// Apply a register's clocked op to the PRE-tick state snapshot.
    fn apply_op(&self, r: &CompReg, old_state: &[u64]) -> u64 {
        let din_bit = |src: &DataSrc| -> u64 {
            match src {
                DataSrc::Pin(p) => u64::from(self.input_levels[*p]),
                // Validation restricts shift data to pins; unreachable, kept total.
                DataSrc::Reg(ri) => old_state[*ri] & 1,
            }
        };
        match r.op {
            Some(RegisterOp::ShiftLeft) => {
                let d = r.data_in.as_ref().map(din_bit).unwrap_or(0);
                (r.state.wrapping_shl(1) | d) & r.mask
            }
            Some(RegisterOp::ShiftRight) => {
                let d = r.data_in.as_ref().map(din_bit).unwrap_or(0);
                (r.state >> 1) | (d.wrapping_shl(r.bits - 1))
            }
            Some(RegisterOp::Load) => match r.data_in {
                Some(DataSrc::Reg(ri)) => old_state[ri] & r.mask,
                Some(DataSrc::Pin(p)) => u64::from(self.input_levels[p]),
                None => r.state,
            },
            Some(RegisterOp::CountUp) => r.state.wrapping_add(1) & r.mask,
            Some(RegisterOp::CountDown) => r.state.wrapping_sub(1) & r.mask,
            None => r.state,
        }
    }

    /// Re-evaluate the combinational outputs and tri-state enables from the
    /// CURRENT input levels and register state (also the entry point after an
    /// external register overwrite via [`LogicComponent::set_register`]).
    pub fn refresh_outputs(&mut self) {
        // Bind every variable the expressions read.
        for i in 0..self.input_names.len() {
            let _ = self
                .ctx
                .set_value(self.input_vars[i].clone(), Value::Boolean(self.input_levels[i]));
        }
        for i in 0..self.output_names.len() {
            let _ = self
                .ctx
                .set_value(self.output_vars[i].clone(), Value::Boolean(self.output_levels[i]));
        }
        for (ri, bit, var) in &self.reg_bit_vars {
            let v = (self.registers[*ri].state >> bit) & 1 == 1;
            let _ = self.ctx.set_value(var.clone(), Value::Boolean(v));
        }

        for step in &self.plan {
            match step {
                PlanStep::Single(i) => {
                    let v = self.comb[*i]
                        .node
                        .eval_boolean_with_context(&self.ctx)
                        .unwrap_or(false);
                    self.output_levels[*i] = v;
                    let _ = self
                        .ctx
                        .set_value(self.output_vars[*i].clone(), Value::Boolean(v));
                }
                PlanStep::Scc(members) => {
                    // Gauss–Seidel sweeps in declaration order, seeded from
                    // the previous stable levels. Convergence within the
                    // bound was verified exhaustively at compile time; the
                    // check below is defense in depth only.
                    let mut settled = false;
                    for _ in 0..COMB_FIXPOINT_BOUND {
                        let mut changed = false;
                        for &mi in members {
                            let v = self.comb[mi]
                                .node
                                .eval_boolean_with_context(&self.ctx)
                                .unwrap_or(false);
                            if v != self.output_levels[mi] {
                                self.output_levels[mi] = v;
                                changed = true;
                                let _ = self
                                    .ctx
                                    .set_value(self.output_vars[mi].clone(), Value::Boolean(v));
                            }
                        }
                        if !changed {
                            settled = true;
                            break;
                        }
                    }
                    if !settled {
                        // Unreachable for a compiled spec (see check_convergence);
                        // scream rather than silently hold a wrong level.
                        eprintln!(
                            "ERROR: logic '{}': comb cycle failed to settle within {} sweeps \
                             at runtime — this should have been refused at compile time",
                            self.spec_id, COMB_FIXPOINT_BOUND
                        );
                    }
                }
            }
        }

        // Tri-state enables.
        for i in 0..self.output_enabled.len() {
            self.output_enabled[i] = true;
        }
        for ts in &self.tristates {
            let en = ts.active.is_active(self.input_levels[ts.enable]);
            if !en {
                for &oi in &ts.outputs {
                    self.output_enabled[oi] = false;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compile(toml_src: &str) -> LogicComponent {
        let logic: Logic = toml::from_str(toml_src).expect("logic TOML parses");
        LogicComponent::compile("test", &logic).expect("compiles")
    }

    /// Drive a tick from a plain name->level map (unlisted pins = unwired).
    fn tick_with(lc: &mut LogicComponent, levels: &[(&str, bool)]) {
        let map: HashMap<&str, bool> = levels.iter().copied().collect();
        lc.tick(&mut |name, _prev| map.get(name).copied());
    }

    const HC595: &str = r#"
inputs  = ["ser", "srclk", "rclk", "srclr_n", "oe_n"]
outputs = ["qa", "qb", "qc", "qd", "qe", "qf", "qg", "qh", "qh_serial"]

[[register]]
name = "shift"
bits = 8
clock = { pin = "srclk", edge = "rising" }
reset = { pin = "srclr_n", active = "low", value = 0 }
op = "shift_left"
data_in = "ser"

[[register]]
name = "store"
bits = 8
clock = { pin = "rclk", edge = "rising" }
op = "load"
data_in = "shift"

[comb]
"qa" = "store[0]"
"qb" = "store[1]"
"qc" = "store[2]"
"qd" = "store[3]"
"qe" = "store[4]"
"qf" = "store[5]"
"qg" = "store[6]"
"qh" = "store[7]"
"qh_serial" = "shift[7]"

[tristate]
"qa..qh" = { enable = "oe_n", active = "low" }
"#;

    /// shiftOut(MSBFIRST) of `byte`: per bit, set SER then pulse SRCLK.
    fn shift_out_msb(lc: &mut LogicComponent, byte: u8, srclr: bool, oe: bool) {
        for bit in (0..8).rev() {
            let b = (byte >> bit) & 1 == 1;
            tick_with(lc, &[("ser", b), ("srclk", false), ("rclk", false), ("srclr_n", srclr), ("oe_n", oe)]);
            tick_with(lc, &[("ser", b), ("srclk", true), ("rclk", false), ("srclr_n", srclr), ("oe_n", oe)]);
            tick_with(lc, &[("ser", b), ("srclk", false), ("rclk", false), ("srclr_n", srclr), ("oe_n", oe)]);
        }
    }

    #[test]
    fn hc595_shifts_and_latches_msb_first() {
        let mut lc = compile(HC595);
        shift_out_msb(&mut lc, 0xA6, true, false);
        assert_eq!(lc.register("shift"), Some(0xA6), "shift register after 8 clocks");
        assert_eq!(lc.register("store"), Some(0x00), "store unlatched until RCLK");
        // qh_serial tracks the shift register's top bit before the latch.
        assert_eq!(lc.output_level("qh_serial"), Some(true), "0xA6 bit7 at the tap");
        // RCLK pulse latches.
        tick_with(&mut lc, &[("srclk", false), ("rclk", true), ("srclr_n", true), ("oe_n", false)]);
        assert_eq!(lc.register("store"), Some(0xA6), "RCLK rising latched shift->store");
        // qa..qh mirror store bits 0..7.
        let byte = 0xA6u8;
        for (i, q) in ["qa", "qb", "qc", "qd", "qe", "qf", "qg", "qh"].iter().enumerate() {
            assert_eq!(
                lc.output_level(q),
                Some((byte >> i) & 1 == 1),
                "{q} = store[{i}]"
            );
        }
    }

    #[test]
    fn hc595_srclr_is_dominant_over_clock() {
        let mut lc = compile(HC595);
        shift_out_msb(&mut lc, 0xFF, true, false);
        assert_eq!(lc.register("shift"), Some(0xFF));
        // Assert clear, then clock while held: the register stays cleared
        // (TI SN74HC595 function table: SRCLR L + SRCLK X -> cleared).
        tick_with(&mut lc, &[("srclk", false), ("srclr_n", false), ("oe_n", false)]);
        assert_eq!(lc.register("shift"), Some(0x00), "clear wipes the shift register");
        tick_with(&mut lc, &[("ser", true), ("srclk", true), ("srclr_n", false), ("oe_n", false)]);
        assert_eq!(
            lc.register("shift"),
            Some(0x00),
            "a clock edge while SRCLR is held low does not shift"
        );
    }

    #[test]
    fn hc595_tied_clocks_latch_one_step_behind() {
        // SRCLK and RCLK rising on the SAME tick: store captures the
        // PRE-shift value (TI datasheet: with tied clocks the shift register
        // is one clock pulse ahead of the storage register).
        let mut lc = compile(HC595);
        shift_out_msb(&mut lc, 0x01, true, false);
        assert_eq!(lc.register("shift"), Some(0x01));
        tick_with(&mut lc, &[("ser", true), ("srclk", true), ("rclk", true), ("srclr_n", true), ("oe_n", false)]);
        assert_eq!(lc.register("shift"), Some(0x03), "shift took the new bit");
        assert_eq!(lc.register("store"), Some(0x01), "store captured the pre-shift value");
    }

    #[test]
    fn hc595_oe_high_tristates_parallel_outputs_only() {
        let mut lc = compile(HC595);
        tick_with(&mut lc, &[("oe_n", true), ("srclr_n", true)]);
        for q in ["qa", "qb", "qc", "qd", "qe", "qf", "qg", "qh"] {
            assert_eq!(lc.output_enabled(q), Some(false), "{q} tri-stated while OE_n high");
        }
        assert_eq!(
            lc.output_enabled("qh_serial"),
            Some(true),
            "the serial tap is not OE-gated on a 74HC595"
        );
        tick_with(&mut lc, &[("oe_n", false), ("srclr_n", true)]);
        assert_eq!(lc.output_enabled("qa"), Some(true), "OE_n low re-enables");
    }

    /// The silicon-correct 74HC165 (shift toward QH: QH shows H, then G, ...).
    const HC165: &str = r#"
inputs  = ["pl_n", "clk", "clk_inh", "ser", "a", "b", "c", "d", "e", "f", "g", "h"]
outputs = ["qh", "qh_n"]

[[register]]
name = "reg"
bits = 8
clock = { pin = "clk", edge = "rising" }
clock_enable = { pin = "clk_inh", active = "low" }
op = "shift_left"
data_in = "ser"
load = { pin = "pl_n", active = "low", data = ["a", "b", "c", "d", "e", "f", "g", "h"] }

[comb]
"qh" = "reg[7]"
"qh_n" = "!reg[7]"
"#;

    #[test]
    fn hc165_loads_and_emits_h_first() {
        let mut lc = compile(HC165);
        // Parallel-load 0b1010_0001 (a=1, f=1, h=1).
        let hi = [("a", true), ("f", true), ("h", true)];
        let mut base: Vec<(&str, bool)> = vec![("pl_n", false), ("clk", false), ("clk_inh", false), ("ser", false)];
        base.extend_from_slice(&hi);
        tick_with(&mut lc, &base);
        assert_eq!(lc.register("reg"), Some(0b1010_0001));
        assert_eq!(lc.output_level("qh"), Some(true), "QH shows H right after load");
        assert_eq!(lc.output_level("qh_n"), Some(false), "QH_n is the complement");

        // Release PL, clock: QH walks H, G, F, ... (silicon direction).
        let expected = [true, false, true, false, false, false, false, true]; // h,g,f,e,d,c,b,a
        assert_eq!(lc.output_level("qh"), Some(expected[0]));
        for want in &expected[1..] {
            tick_with(&mut lc, &[("pl_n", true), ("clk", true), ("clk_inh", false), ("ser", false)]);
            tick_with(&mut lc, &[("pl_n", true), ("clk", false), ("clk_inh", false), ("ser", false)]);
            assert_eq!(lc.output_level("qh"), Some(*want));
        }
    }

    #[test]
    fn hc165_clock_inhibit_blocks_shifts() {
        let mut lc = compile(HC165);
        tick_with(&mut lc, &[("pl_n", false), ("clk", false), ("clk_inh", false), ("h", true)]);
        assert_eq!(lc.output_level("qh"), Some(true));
        // CLK_INH high: rising clock does nothing.
        tick_with(&mut lc, &[("pl_n", true), ("clk", true), ("clk_inh", true)]);
        assert_eq!(lc.register("reg"), Some(0x80), "inhibited clock held the register");
    }

    const NOR_LATCH: &str = r#"
inputs  = ["set", "reset"]
outputs = ["q", "qb"]

[comb]
"q"  = "!(set | qb)"
"qb" = "!(reset | q)"

[init]
"q" = 1
"qb" = 0
"#;

    #[test]
    fn nor_latch_matches_spike_recorder_truth_table() {
        let mut lc = compile(NOR_LATCH);
        assert_eq!(lc.warnings.len(), 1, "cycle warning surfaced");
        // Power-on idle: Q HIGH (init).
        assert_eq!(lc.output_level("q"), Some(true), "power-on idle Q HIGH");
        // RESET pulse with no spike: Q stays HIGH.
        tick_with(&mut lc, &[("set", false), ("reset", true)]);
        assert_eq!(lc.output_level("q"), Some(true), "RESET holds idle HIGH");
        // Release: HOLD.
        tick_with(&mut lc, &[("set", false), ("reset", false)]);
        assert_eq!(lc.output_level("q"), Some(true), "hold");
        // Spike (SET pulse): Q LOW.
        tick_with(&mut lc, &[("set", true), ("reset", false)]);
        assert_eq!(lc.output_level("q"), Some(false), "SET drives Q LOW");
        // Spike clears: HELD LOW (the latch memory).
        tick_with(&mut lc, &[("set", false), ("reset", false)]);
        assert_eq!(lc.output_level("q"), Some(false), "held LOW after the pulse");
        // RESET: back to idle HIGH.
        tick_with(&mut lc, &[("set", false), ("reset", true)]);
        assert_eq!(lc.output_level("q"), Some(true), "RESET returns idle HIGH");
    }

    #[test]
    fn non_convergent_cycle_is_refused_at_compile() {
        let logic: Logic = toml::from_str(
            r#"
inputs  = ["x"]
outputs = ["y"]
[comb]
"y" = "!y & !x | x & y & 0 | !y"
"#,
        )
        .unwrap();
        // y = !y (obfuscated enough to have an input): never settles.
        let e = LogicComponent::compile("osc", &logic).unwrap_err();
        assert!(
            matches!(e, LogicCompileError::NonConvergent { .. }),
            "got: {e}"
        );
        let msg = e.to_string();
        assert!(msg.contains("does not converge"), "named error text: {msg}");
    }

    #[test]
    fn too_wide_cycle_is_refused_not_shipped_unverified() {
        // A 13-output ring: converges trivially, but too wide to verify
        // exhaustively under the cap, refused with the named error.
        let outputs: Vec<String> = (0..13).map(|i| format!("y{i}")).collect();
        let mut comb = std::collections::BTreeMap::new();
        for i in 0..13usize {
            comb.insert(format!("y{i}"), format!("y{}", (i + 1) % 13));
        }
        let logic = Logic {
            inputs: vec![],
            outputs,
            comb,
            registers: vec![],
            tristate: Default::default(),
            init: Default::default(),
        };
        let e = LogicComponent::compile("ring", &logic).unwrap_err();
        assert!(
            matches!(e, LogicCompileError::TooWideToVerify { members: 13, .. }),
            "got: {e}"
        );
    }

    #[test]
    fn dff_with_preset_and_clear() {
        // The 74HC74 single-flop shape: load-from-pin on rising clock, dual
        // async controls.
        let mut lc = compile(
            r#"
inputs  = ["d", "clk", "pre_n", "clr_n"]
outputs = ["q", "q_n"]
[[register]]
name = "ff"
bits = 1
clock = { pin = "clk", edge = "rising" }
reset = [
  { pin = "clr_n", active = "low", value = 0 },
  { pin = "pre_n", active = "low", value = 1 },
]
op = "load"
data_in = "d"
[comb]
"q" = "ff[0]"
"q_n" = "!ff[0]"
"#,
        );
        // Clock a 1 through D.
        tick_with(&mut lc, &[("d", true), ("clk", false), ("pre_n", true), ("clr_n", true)]);
        tick_with(&mut lc, &[("d", true), ("clk", true), ("pre_n", true), ("clr_n", true)]);
        assert_eq!(lc.output_level("q"), Some(true), "D captured on rising edge");
        assert_eq!(lc.output_level("q_n"), Some(false));
        // D changes while clock low: no effect.
        tick_with(&mut lc, &[("d", false), ("clk", false), ("pre_n", true), ("clr_n", true)]);
        assert_eq!(lc.output_level("q"), Some(true), "level-insensitive between edges");
        // Async clear dominates the clock.
        tick_with(&mut lc, &[("d", true), ("clk", true), ("pre_n", true), ("clr_n", false)]);
        assert_eq!(lc.output_level("q"), Some(false), "CLR_n forces 0");
        // Async preset.
        tick_with(&mut lc, &[("d", false), ("clk", false), ("pre_n", false), ("clr_n", true)]);
        assert_eq!(lc.output_level("q"), Some(true), "PRE_n forces 1");
    }

    #[test]
    fn counter_ops_wrap_at_width() {
        let mut lc = compile(
            r#"
inputs  = ["clk", "rst"]
outputs = ["q0", "q1"]
[[register]]
name = "cnt"
bits = 2
clock = { pin = "clk", edge = "rising" }
reset = { pin = "rst", active = "high", value = 0 }
op = "count_up"
[comb]
"q0" = "cnt[0]"
"q1" = "cnt[1]"
"#,
        );
        for want in [1u64, 2, 3, 0, 1] {
            tick_with(&mut lc, &[("clk", true), ("rst", false)]);
            tick_with(&mut lc, &[("clk", false), ("rst", false)]);
            assert_eq!(lc.register("cnt"), Some(want), "2-bit counter wraps");
        }
    }

    #[test]
    fn unwired_controls_read_released() {
        // Drive only SER/SRCLK; SRCLR_n and OE_n unwired must read released /
        // enabled, so the part shifts and drives normally.
        let mut lc = compile(HC595);
        let seq = [("ser", true), ("srclk", false)];
        tick_with(&mut lc, &seq);
        tick_with(&mut lc, &[("ser", true), ("srclk", true)]);
        assert_eq!(lc.register("shift"), Some(0x01), "unwired SRCLR_n reads released");
        assert_eq!(lc.output_enabled("qa"), Some(true), "unwired OE_n stays enabled");
    }

    #[test]
    fn falling_edge_clock_is_honored() {
        let mut lc = compile(
            r#"
inputs  = ["d", "clkn"]
outputs = ["q"]
[[register]]
name = "ff"
bits = 1
clock = { pin = "clkn", edge = "falling" }
op = "load"
data_in = "d"
[comb]
"q" = "ff[0]"
"#,
        );
        tick_with(&mut lc, &[("d", true), ("clkn", true)]);
        assert_eq!(lc.output_level("q"), Some(false), "rising edge does nothing");
        tick_with(&mut lc, &[("d", true), ("clkn", false)]);
        assert_eq!(lc.output_level("q"), Some(true), "falling edge captures D");
    }

    #[test]
    fn set_register_plus_refresh_drives_outputs() {
        // The chain-mirror / latch_byte path: overwrite store, refresh, read qa..qh.
        let mut lc = compile(HC595);
        assert!(lc.set_register("store", 0x5A));
        assert!(!lc.set_register("nonexistent", 1), "unknown register refused");
        lc.refresh_outputs();
        for (i, q) in ["qa", "qb", "qc", "qd", "qe", "qf", "qg", "qh"].iter().enumerate() {
            assert_eq!(lc.output_level(q), Some((0x5Au8 >> i) & 1 == 1));
        }
    }

    #[test]
    fn gate_expressions_evaluate_truth_tables() {
        // One 74HC00 NAND gate + one XOR (the §1.3 shapes) in a single block.
        let mut lc = compile(
            r#"
inputs  = ["1a", "1b"]
outputs = ["nand_y", "xor_y"]
[comb]
"nand_y" = "!(1a & 1b)"
"xor_y" = "1a ^ 1b"
"#,
        );
        for (a, b, nand, xor) in [
            (false, false, true, false),
            (false, true, true, true),
            (true, false, true, true),
            (true, true, false, false),
        ] {
            tick_with(&mut lc, &[("1a", a), ("1b", b)]);
            assert_eq!(lc.output_level("nand_y"), Some(nand), "NAND({a},{b})");
            assert_eq!(lc.output_level("xor_y"), Some(xor), "XOR({a},{b})");
        }
    }

    #[test]
    fn sequential_pins_cover_clock_and_data() {
        let lc = compile(HC595);
        let pins = lc.sequential_pins();
        for p in ["ser", "srclk", "rclk", "srclr_n"] {
            assert!(pins.contains(&p), "{p} is a sequential pin");
        }
        assert!(!pins.contains(&"oe_n"), "OE is not sequential");
        assert!(lc.is_sequential());
    }
}
