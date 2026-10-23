//! Compiled behavioral-source expressions.
//! Long-form how-and-why: docs/how-and-why/hauksbee-ir/bexpr.md.
//!
//! A B-source card (`Bxxx n+ n- V={expr}` / `I={expr}`) carries an arithmetic
//! expression over node voltages, branch currents, `time`, and `.param`
//! values. The loader (spice.rs) rewrites the raw text into a *canonical*
//! form before it reaches this type:
//!
//! * every `V(node)` / `V(a,b)` / `I(vname)` reference becomes a synthetic
//!   dependency variable `__d{k}` (a differential `V(a,b)` becomes
//!   `(__d{i} - __d{j})`), with the meaning of slot `k` recorded in the
//!   device's `deps: Vec<BDep>`, positionally aligned with the `__d{k}`
//!   names;
//! * `.param` names are folded to numeric literals (round-trip-exact `{:?}`
//!   formatting), so a [`CompiledExpr`] never needs the parameter
//!   environment again;
//! * supported function names are mapped onto their `evalexpr` builtins
//!   (`exp` -> `math::exp`, ...), and everything is lowercased.
//!
//! So the ONLY identifiers a canonical expression may contain are `__d{k}`
//! and `time`; [`CompiledExpr::compile`] enforces that, which is what makes
//! the serde story sound: the expression **serializes as its canonical
//! source text and recompiles on deserialize** (the enforcement test
//! serde-round-trips every `Device` variant, and a pre-parsed tree is not a
//! serializable thing). `Debug`/`PartialEq` are likewise defined on the
//! source text, so a recompiled expression is indistinguishable from the
//! original.
//!
//! # The exact expression subset shipped
//!
//! Operators: `+ - * / % ^` (`^` is exponentiation; the loader also rewrites
//! `**` to `^`), comparisons `== != < <= > >=` and boolean `&& ||` (useful
//! inside `if`), numeric literals with optional exponent (`1e-3`; SPICE
//! engineering suffixes are NOT valid inside `{...}`; the suffix rule of
//!
//! Functions (mapped onto evalexpr builtins): `ln`, `log10`, `log2`, `exp`,
//! `pow(x,y)`, `sqrt`, `cbrt`, `abs`, `sin`, `cos`, `tan`, `asin`, `acos`,
//! `atan`, `atan2(y,x)`, `sinh`, `cosh`, `tanh`, `asinh`, `acosh`, `atanh`,
//! `hypot(x,y)`, `floor`, `ceil`, `round`, `min(...)`, `max(...)`, and
//! `if(cond, then, else)` (evalexpr's ternary builtin; this is the one
//! conditional form that ships).
//!
//! Refused loudly, with line numbers, by the loader: bare `log` (ambiguous
//! between ln and log10 across SPICE dialects, write `ln` or `log10`),
//! `POLY(...)`, `TABLE`, `VALUE`, any unknown function or identifier, and
//! un-braced expressions.

use evalexpr::{
    build_operator_tree, ContextWithMutableVariables, HashMapContext, Node as EvalNode,
    Operator as Op, Value,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::HashMap;
use std::ops::{Add, Div, Mul, Rem, Sub};
use Num::{B, F, I};

/// A pre-parsed behavioral expression in canonical form (see the module doc).
///
/// Cheap to evaluate repeatedly (parsed once, lowered to straight-line
/// steps); the finite-difference Jacobian in `hauksbee-solve` calls
/// [`CompiledExpr::eval`] `1 + n_deps` times per stamp per B-source, and the
/// line search stamps several times per Newton iteration. Evaluation lives
/// HERE, on the IR type, so `hauksbee-solve` needs no `evalexpr` dependency
/// edge of its own, a deliberate Cargo decision.
#[derive(Clone)]
pub struct CompiledExpr {
    /// Canonical source text (what serializes; see the module doc).
    src: String,
    /// The pre-parsed operator tree, rebuilt from `src` on deserialize.
    tree: EvalNode,
    /// `tree` lowered to straight-line [`Step`]s, or `None` when it reaches
    /// outside the subset [`Lower`] reproduces exactly.
    prog: Option<Box<[Step]>>,
    /// Dependency slot count: the highest `__d` index in the tree plus one; the
    /// loader constructs slots densely so this equals the device's `deps.len()`.
    n_slots: usize,
}

impl CompiledExpr {
    /// Parse a CANONICAL expression (identifiers restricted to `__d{k}` and
    /// `time`).
    ///
    /// # Errors
    ///
    /// A syntax error, or any identifier other than a `__d<k>` slot or `time`,
    /// as an `Err(String)`; the loader wraps it with a line number, serde with
    /// a deserialization error.
    pub fn compile(src: &str) -> Result<Self, String> {
        let tree = build_operator_tree(src)
            .map_err(|e| format!("malformed behavioral expression `{src}`: {e}"))?;
        let mut n_slots = 0usize;
        for ident in tree.iter_variable_identifiers() {
            if let Some(k) = ident
                .strip_prefix("__d")
                .and_then(|d| d.parse::<usize>().ok())
            {
                n_slots = n_slots.max(k + 1);
            } else if ident != "time" {
                return Err(format!(
                    "behavioral expression `{src}` contains non-canonical identifier \
                     `{ident}` (only `__d<k>` dependency slots and `time` survive \
                     loading; params fold to constants)"
                ));
            }
        }
        let mut lower = Lower::default();
        let prog = lower.emit(&tree).map(|_| lower.steps.into());
        Ok(Self {
            src: src.to_string(),
            tree,
            prog,
            n_slots,
        })
    }

    /// The canonical source text.
    #[must_use]
    pub fn src(&self) -> &str {
        &self.src
    }

    /// Number of dependency slots (`__d0 .. __d{n-1}`) the expression reads.
    #[must_use]
    pub const fn n_slots(&self) -> usize {
        self.n_slots
    }

    /// Evaluate at the given dependency-slot values and time. `deps.len()`
    /// must be at least [`CompiledExpr::n_slots`] (the device's `deps` vec is
    /// exactly that long by construction).
    ///
    /// NOTE: IEEE float semantics apply inside, `1/0` is `inf`, `ln(-1)` is
    /// NaN, not an error, so callers must ALSO guard the returned value with
    /// `is_finite()`; this function reports only structural faults.
    ///
    /// # Errors
    ///
    /// Fewer `deps` than [`CompiledExpr::n_slots`], an evalexpr evaluation
    /// fault, or a result that is neither a number nor a boolean. The caller
    /// decides what a fault means (the stamp turns it into a device-named
    /// solver refusal).
    pub fn eval(&self, deps: &[f64], time: f64) -> Result<f64, String> {
        if deps.len() < self.n_slots {
            return Err(format!(
                "behavioral expression `{}` needs {} dependency values, got {}",
                self.src,
                self.n_slots,
                deps.len()
            ));
        }
        // The lowered program answers every point it models; a fault goes back
        // to evalexpr so the reported error is evalexpr's own.
        let fast = self.prog.as_deref().and_then(|p| run(p, deps, time));
        fast.map_or_else(|| self.eval_tree(deps, time), |v| Ok(v.f64()))
    }

    /// Evaluate with evalexpr's interpreter, the reference [`Self::eval`] keeps
    /// (and, now only faults and unlowered forms reach it, a plain map context;
    /// binding `time` for a tree that never reads it changes nothing).
    fn eval_tree(&self, deps: &[f64], time: f64) -> Result<f64, String> {
        let mut ctx = HashMapContext::new();
        let names = (0..deps.len())
            .map(|k| format!("__d{k}"))
            .chain(["time".into()]);
        for (id, &v) in names.zip(deps.iter().chain([&time])) {
            ctx.set_value(id, v.into()).map_err(|e| e.to_string())?;
        }
        match self.tree.eval_with_context(&ctx) {
            Ok(Value::Boolean(b)) => Ok(if b { 1.0 } else { 0.0 }),
            Ok(other) => other.as_number().map_err(|_| {
                format!(
                    "behavioral expression `{}` evaluated to a non-number ({other:?})",
                    self.src
                )
            }),
            Err(e) => Err(format!(
                "behavioral expression `{}` failed to evaluate: {e}",
                self.src
            )),
        }
    }
}

/// An evalexpr value in the subset lowering keeps: `Copy`, so a step never
/// allocates. The derived `==` is evalexpr's `Value` equality (`1 != 1.0`).
#[derive(Clone, Copy, PartialEq)]
enum Num {
    F(f64),
    I(i64),
    B(bool),
}

impl Num {
    /// What [`CompiledExpr::eval`] reports: evalexpr's `as_number`, a boolean as 1 or 0.
    // The int-to-float `as` is evalexpr's own conversion, kept bit for bit.
    #[allow(clippy::cast_precision_loss)]
    fn f64(self) -> f64 {
        match self {
            F(x) => x,
            I(x) => x as f64,
            B(x) => f64::from(u8::from(x)),
        }
    }

    /// evalexpr's `as_number`.
    fn num(self) -> Option<f64> {
        (!matches!(self, B(_))).then(|| self.f64())
    }

    /// evalexpr's `as_boolean`.
    const fn bool(self) -> Option<bool> {
        if let B(x) = self {
            Some(x)
        } else {
            None
        }
    }
}

/// One step's operation over the registers its argument indices name; `None`
/// wherever evalexpr 11.3.1 errors (a type mismatch, an integer overflow or
/// division by zero).
type Apply = fn(&[Num], &[usize]) -> Option<Num>;

/// A straight-line step; step `k` writes register `k`, the last is the result.
#[derive(Clone)]
enum Step {
    Num(Num),
    Dep(usize),
    Time,
    Apply(Apply, Box<[usize]>),
}

/// Lowers an evalexpr tree to [`Step`]s, one per distinct subtree: evaluation
/// is pure and eager (evalexpr evaluates every operand, even an untaken `if`
/// arm), so a shared register holds the same bits the tree would recompute.
#[derive(Default)]
struct Lower {
    steps: Vec<Step>,
    seen: HashMap<(String, Vec<usize>), usize>,
}

impl Lower {
    /// The register holding `node`'s value, or `None` outside the lowered subset.
    fn emit(&mut self, node: &EvalNode) -> Option<usize> {
        let (op, kids) = (node.operator(), node.children());
        let (step, args, tuple) = match (op, kids) {
            (Op::RootNode, [one]) => return self.emit(one),
            (Op::Const { value }, []) => (Step::Num(num(value)?), vec![], false),
            (Op::VariableIdentifierRead { identifier }, []) => {
                let slot = identifier.strip_prefix("__d").map(str::parse);
                let step = slot.map_or(Some(Step::Time), |k| k.ok().map(Step::Dep))?;
                (step, vec![], false)
            }
            (Op::FunctionIdentifier { identifier }, [arg]) => {
                let mut arg = arg;
                while let (Op::RootNode, [inner]) = (arg.operator(), arg.children()) {
                    arg = inner;
                }
                let tuple = matches!(arg.operator(), Op::Tuple);
                let args = self.emit_all(if tuple {
                    arg.children()
                } else {
                    std::slice::from_ref(arg)
                })?;
                let apply = builtin(identifier, tuple, args.len())?;
                (Step::Apply(apply, args.clone().into()), args, tuple)
            }
            _ => {
                let (apply, args) = (operator(op, kids.len())?, self.emit_all(kids)?);
                (Step::Apply(apply, args.clone().into()), args, false)
            }
        };
        let next = self.steps.len();
        let reg = *self
            .seen
            .entry((format!("{op:?}{tuple}"), args))
            .or_insert(next);
        if reg == next {
            self.steps.push(step);
        }
        Some(reg)
    }

    fn emit_all(&mut self, nodes: &[EvalNode]) -> Option<Vec<usize>> {
        nodes.iter().map(|n| self.emit(n)).collect()
    }
}

/// A literal in the lowered subset.
const fn num(value: &Value) -> Option<Num> {
    match value {
        Value::Float(x) => Some(F(*x)),
        Value::Int(x) => Some(I(*x)),
        Value::Boolean(x) => Some(B(*x)),
        _ => None,
    }
}

/// Run a lowered program: its result, or `None` where evalexpr would fault.
fn run(prog: &[Step], deps: &[f64], time: f64) -> Option<Num> {
    // The solver evaluates every B-source `1 + n_deps` times per stamp, so a
    // heap register file per call was a measurable share of an MCU co-sim's
    // wall time; typical programs fit this stack file, longer ones allocate.
    const STACK_REGS: usize = 48;
    if prog.len() <= STACK_REGS {
        let mut regs = [F(0.0); STACK_REGS];
        run_into(prog, deps, time, &mut regs[..prog.len()])
    } else {
        run_into(prog, deps, time, &mut vec![F(0.0); prog.len()])
    }
}

/// [`run`] over a register file exactly `prog.len()` long.
fn run_into(prog: &[Step], deps: &[f64], time: f64, regs: &mut [Num]) -> Option<Num> {
    for (k, step) in prog.iter().enumerate() {
        regs[k] = match step {
            Step::Num(v) => *v,
            Step::Dep(d) => F(*deps.get(*d)?),
            Step::Time => F(time),
            Step::Apply(f, args) => f(&regs[..k], args)?,
        };
    }
    regs.last().copied()
}

/// evalexpr's `+ - * / %`: checked on two ints, float otherwise.
fn arith(
    x: Num,
    y: Num,
    int: fn(i64, i64) -> Option<i64>,
    float: fn(f64, f64) -> f64,
) -> Option<Num> {
    match (x, y) {
        (I(x), I(y)) => int(x, y).map(I),
        _ => Some(F(float(x.num()?, y.num()?))),
    }
}

/// evalexpr's `< <= > >=`: integer order on two ints, float order otherwise.
fn order(
    x: Num,
    y: Num,
    int: fn(&i64, &i64) -> bool,
    float: fn(&f64, &f64) -> bool,
) -> Option<Num> {
    Some(B(match (x, y) {
        (I(x), I(y)) => int(&x, &y),
        _ => float(&x.num()?, &y.num()?),
    }))
}

/// evalexpr's `min` (`max` if not `min`): separate int and float folds, the int
/// winning only when strictly beyond the float (so `min(x, 1)` can be the int `1`).
fn extreme(r: &[Num], a: &[usize], min: bool) -> Option<Num> {
    let (mut int, mut float) = if min {
        (i64::MAX, f64::INFINITY)
    } else {
        (i64::MIN, f64::NEG_INFINITY)
    };
    for &k in a {
        match (r[k], min) {
            (I(x), true) => int = int.min(x),
            (I(x), false) => int = int.max(x),
            (F(x), true) => float = float.min(x),
            (F(x), false) => float = float.max(x),
            (B(_), _) => return None,
        }
    }
    let beyond = if min {
        I(int).f64() < float
    } else {
        I(int).f64() > float
    };
    Some(if beyond { I(int) } else { F(float) })
}

/// evalexpr 11.3.1's `Operator::eval` on `n` operands, for the operators lowered.
fn operator(op: &Op, n: usize) -> Option<Apply> {
    let f: Apply = match (op, n) {
        (Op::Neg, 1) => |r, a| match r[a[0]] {
            I(x) => x.checked_neg().map(I),
            x => Some(F(-x.num()?)),
        },
        (Op::Not, 1) => |r, a| Some(B(!r[a[0]].bool()?)),
        (Op::Add, 2) => |r, a| arith(r[a[0]], r[a[1]], i64::checked_add, f64::add),
        (Op::Sub, 2) => |r, a| arith(r[a[0]], r[a[1]], i64::checked_sub, f64::sub),
        (Op::Mul, 2) => |r, a| arith(r[a[0]], r[a[1]], i64::checked_mul, f64::mul),
        (Op::Div, 2) => |r, a| arith(r[a[0]], r[a[1]], i64::checked_div, f64::div),
        (Op::Mod, 2) => |r, a| arith(r[a[0]], r[a[1]], i64::checked_rem, f64::rem),
        (Op::Exp, 2) => |r, a| Some(F(r[a[0]].num()?.powf(r[a[1]].num()?))),
        (Op::Eq, 2) => |r, a| Some(B(r[a[0]] == r[a[1]])),
        (Op::Neq, 2) => |r, a| Some(B(r[a[0]] != r[a[1]])),
        (Op::Gt, 2) => |r, a| order(r[a[0]], r[a[1]], i64::gt, f64::gt),
        (Op::Lt, 2) => |r, a| order(r[a[0]], r[a[1]], i64::lt, f64::lt),
        (Op::Geq, 2) => |r, a| order(r[a[0]], r[a[1]], i64::ge, f64::ge),
        (Op::Leq, 2) => |r, a| order(r[a[0]], r[a[1]], i64::le, f64::le),
        // Both operands are checked before combining: evalexpr never short-circuits.
        (Op::And, 2) => |r, a| Some(B(r[a[0]].bool()? & r[a[1]].bool()?)),
        (Op::Or, 2) => |r, a| Some(B(r[a[0]].bool()? | r[a[1]].bool()?)),
        _ => return None,
    };
    Some(f)
}

/// Float math on the argument registers, as evalexpr's `simple_math!` builtins.
macro_rules! math {
    ($f:path, $($k:literal),+) => {
        |r, a| Some(F($f($(r[a[$k]].num()?),+)))
    };
}

/// evalexpr 11.3.1's builtin `id` applied to one value, or to an `n`-tuple.
fn builtin(id: &str, tuple: bool, n: usize) -> Option<Apply> {
    let f: Apply = match (id, tuple, n) {
        ("min", true, _) => |r, a| extreme(r, a, true),
        ("max", true, _) => |r, a| extreme(r, a, false),
        ("if", true, 3) => |r, a| Some(if r[a[0]].bool()? { r[a[1]] } else { r[a[2]] }),
        ("math::abs", false, _) => |r, a| match r[a[0]] {
            F(x) => Some(F(x.abs())),
            I(x) => x.checked_abs().map(I),
            B(_) => None,
        },
        ("math::pow", true, 2) => math!(f64::powf, 0, 1),
        ("math::atan2", true, 2) => math!(f64::atan2, 0, 1),
        ("math::hypot", true, 2) => math!(f64::hypot, 0, 1),
        ("math::ln", false, _) => math!(f64::ln, 0),
        ("math::log2", false, _) => math!(f64::log2, 0),
        ("math::log10", false, _) => math!(f64::log10, 0),
        ("math::exp", false, _) => math!(f64::exp, 0),
        ("math::cos", false, _) => math!(f64::cos, 0),
        ("math::acos", false, _) => math!(f64::acos, 0),
        ("math::cosh", false, _) => math!(f64::cosh, 0),
        ("math::acosh", false, _) => math!(f64::acosh, 0),
        ("math::sin", false, _) => math!(f64::sin, 0),
        ("math::asin", false, _) => math!(f64::asin, 0),
        ("math::sinh", false, _) => math!(f64::sinh, 0),
        ("math::asinh", false, _) => math!(f64::asinh, 0),
        ("math::tan", false, _) => math!(f64::tan, 0),
        ("math::atan", false, _) => math!(f64::atan, 0),
        ("math::tanh", false, _) => math!(f64::tanh, 0),
        ("math::atanh", false, _) => math!(f64::atanh, 0),
        ("math::sqrt", false, _) => math!(f64::sqrt, 0),
        ("math::cbrt", false, _) => math!(f64::cbrt, 0),
        ("floor", false, _) => math!(f64::floor, 0),
        ("round", false, _) => math!(f64::round, 0),
        ("ceil", false, _) => math!(f64::ceil, 0),
        _ => return None,
    };
    Some(f)
}

/// Source-text identity: a recompiled expression equals the original.
impl PartialEq for CompiledExpr {
    fn eq(&self, other: &Self) -> bool {
        self.src == other.src
    }
}

/// Debug shows the source text only, so the serde round-trip enforcement
/// test's `format!("{dev:?}")` comparison is stable across recompiles.
impl std::fmt::Debug for CompiledExpr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CompiledExpr({:?})", self.src)
    }
}

/// Serializes as the canonical source string (the
/// expression serializes as text and recompiles on deserialize).
impl Serialize for CompiledExpr {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.src)
    }
}

impl<'de> Deserialize<'de> for CompiledExpr {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let src = String::deserialize(deserializer)?;
        Self::compile(&src).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compile_eval_roundtrip() {
        let e = CompiledExpr::compile("2.0*__d0 + math::tanh(__d1) + 0.5*time").unwrap();
        assert_eq!(e.n_slots(), 2);
        let v = e.eval(&[1.0, 0.0], 2.0).unwrap();
        assert!((v - 3.0).abs() < 1e-12, "got {v}");
        let json = serde_json::to_string(&e).unwrap();
        let back: CompiledExpr = serde_json::from_str(&json).unwrap();
        assert_eq!(e, back);
        assert_eq!(format!("{e:?}"), format!("{back:?}"));
        assert_eq!(back.eval(&[1.0, 0.0], 2.0).unwrap().to_bits(), v.to_bits());
    }

    /// The fixture corpus (loop, deck and binder expressions, then semantic
    /// probes), split before its evaluation-fault section.
    fn corpus() -> [impl Iterator<Item = CompiledExpr>; 2] {
        let text = include_str!("../tests/fixtures/bexpr_corpus.txt");
        let parts: [&str; 2] = text.split_once("# Evaluation faults").unwrap().into();
        parts.map(|part| {
            part.lines()
                .filter(|l| !l.starts_with('#'))
                .map(|l| CompiledExpr::compile(l).unwrap_or_else(|e| panic!("{e}")))
        })
    }

    /// Differential test of the lowered program against evalexpr's interpreter
    /// over the whole corpus at edge values (signed zeros, a subnormal, huge,
    /// infinities, NaN) mixed with pseudo-random signed magnitudes: a value
    /// matches bit for bit (NaN payloads included), a fault word for word.
    #[test]
    fn lowered_eval_matches_evalexpr() {
        const EDGES: [f64; 12] = [
            0.0,
            -0.0,
            1.0,
            -1.0,
            0.5,
            2.0,
            5e-324,
            1e300,
            -1e300,
            f64::INFINITY,
            -f64::INFINITY,
            f64::NAN,
        ];
        let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
        let mut draw = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let unit = f64::from(u32::try_from(seed >> 32).unwrap()) / f64::from(u32::MAX);
            let sign = if seed & 1 == 0 { 1.0 } else { -1.0 };
            let edge = EDGES[usize::try_from(seed >> 40).unwrap() % EDGES.len()];
            if seed.is_multiple_of(3) {
                edge
            } else {
                sign * 10f64.powf(unit.mul_add(24.0, -12.0))
            }
        };
        let [lowered, faults] = corpus();
        for (must_lower, e) in lowered.map(|e| (true, e)).chain(faults.map(|e| (false, e))) {
            assert!(
                !must_lower || e.prog.is_some(),
                "`{}` did not lower",
                e.src()
            );
            for _ in 0..500 {
                let (deps, time) = ([(); 7].map(|()| draw()), draw());
                let (new, old) = (e.eval(&deps, time), e.eval_tree(&deps, time));
                let same = match (&new, &old) {
                    (Ok(a), Ok(b)) => a.to_bits() == b.to_bits(),
                    _ => new == old,
                };
                assert!(
                    same,
                    "`{}` at {deps:?}, {time}: {new:?} vs {old:?}",
                    e.src()
                );
            }
        }
    }

    /// Mean cost of one eval, lowered and interpreted, per non-fault corpus
    /// expression (the six converter loop expressions first). A report, not a
    /// gate: `cargo nextest run -p hauksbee-ir --run-ignored all eval_timing --no-capture`.
    #[test]
    #[ignore = "timing report, run on demand"]
    fn eval_timing() {
        let [lowered, _] = corpus();
        for e in lowered {
            let ns = |f: &dyn Fn(&[f64], f64) -> Option<f64>| {
                let start = std::time::Instant::now();
                for i in 0..2000u32 {
                    let x = f64::from(i % 7) * 0.3;
                    std::hint::black_box(f(&[x, 0.1, -0.2, x, 5.0, 0.6, 0.62], x));
                }
                start.elapsed().as_nanos() / 2000
            };
            let (new, old) = (
                ns(&|d, t| e.eval(d, t).ok()),
                ns(&|d, t| e.eval_tree(d, t).ok()),
            );
            println!("{new:>6} ns lowered {old:>8} ns evalexpr  {:.60}", e.src());
        }
    }

    #[test]
    fn non_canonical_identifier_refuses() {
        let err = CompiledExpr::compile("2*vout").unwrap_err();
        assert!(err.contains("non-canonical"), "{err}");
        // And a corrupt serialized form refuses on deserialize.
        assert!(serde_json::from_str::<CompiledExpr>("\"2*vout\"").is_err());
    }

    #[test]
    fn ieee_semantics_are_reported_by_value_not_error() {
        // Structural contract: ln(-1) is a NaN VALUE, not an Err; the stamp
        // guards finiteness itself. Pin that so the guard's placement is a
        // documented behavior, not an accident.
        let e = CompiledExpr::compile("math::ln(__d0)").unwrap();
        assert!(e.eval(&[-1.0], 0.0).unwrap().is_nan());
        let d = CompiledExpr::compile("1.0/__d0").unwrap();
        assert!(d.eval(&[0.0], 0.0).unwrap().is_infinite());
    }
}
