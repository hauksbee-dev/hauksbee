//! Compiled behavioral-source expressions (dev-plan 04 §2.5).
//!
//! A B-source card (`Bxxx n+ n- V={expr}` / `I={expr}`) carries an arithmetic
//! expression over node voltages, branch currents, `time`, and `.param`
//! values. The loader (spice.rs) rewrites the raw text into a *canonical*
//! form before it reaches this type:
//!
//! * every `V(node)` / `V(a,b)` / `I(vname)` reference becomes a synthetic
//!   dependency variable `__d{k}` (a differential `V(a,b)` becomes
//!   `(__d{i} - __d{j})`), with the meaning of slot `k` recorded in the
//!   device's `deps: Vec<BDep>` — positionally aligned with the `__d{k}`
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
//! source text and recompiles on deserialize** (the 06 §5 enforcement test
//! serde-round-trips every `Device` variant, and a pre-parsed tree is not a
//! serializable thing). `Debug`/`PartialEq` are likewise defined on the
//! source text, so a recompiled expression is indistinguishable from the
//! original.
//!
//! # The exact expression subset shipped (be honest, §4.3)
//!
//! Operators: `+ - * / % ^` (`^` is exponentiation; the loader also rewrites
//! `**` to `^`), comparisons `== != < <= > >=` and boolean `&& ||` (useful
//! inside `if`), numeric literals with optional exponent (`1e-3`; SPICE
//! engineering suffixes are NOT valid inside `{...}` — the suffix rule of
//! §4.2 applies).
//!
//! Functions (mapped onto evalexpr builtins): `ln`, `log10`, `log2`, `exp`,
//! `pow(x,y)`, `sqrt`, `cbrt`, `abs`, `sin`, `cos`, `tan`, `asin`, `acos`,
//! `atan`, `atan2(y,x)`, `sinh`, `cosh`, `tanh`, `asinh`, `acosh`, `atanh`,
//! `hypot(x,y)`, `floor`, `ceil`, `round`, `min(...)`, `max(...)`, and
//! `if(cond, then, else)` (evalexpr's ternary builtin — this is the one
//! conditional form that ships).
//!
//! Refused loudly, with line numbers, by the loader: bare `log` (ambiguous
//! between ln and log10 across SPICE dialects — write `ln` or `log10`),
//! `POLY(...)`, `TABLE`, `VALUE`, any unknown function or identifier, and
//! un-braced expressions.

use evalexpr::{
    build_operator_tree, ContextWithMutableVariables, DefaultNumericTypes, HashMapContext,
    Node as EvalNode, Value,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A pre-parsed behavioral expression in canonical form (see the module doc).
///
/// Cheap to evaluate repeatedly (the tree is parsed once); the finite-
/// difference Jacobian in `hauksbee-solve` calls [`CompiledExpr::eval`]
/// `1 + n_deps` times per Newton iteration per B-source. Evaluation lives
/// HERE, on the IR type, so `hauksbee-solve` needs no `evalexpr` dependency
/// edge of its own — a deliberate Cargo decision (04 §2.5 flags the edge).
#[derive(Clone)]
pub struct CompiledExpr {
    /// Canonical source text (what serializes; see the module doc).
    src: String,
    /// The pre-parsed operator tree, rebuilt from `src` on deserialize.
    tree: EvalNode<DefaultNumericTypes>,
    /// Cached `__d{k}` variable names, index = dependency slot. Slot count is
    /// the highest `__d` index in the tree plus one; the loader constructs
    /// slots densely so this equals the device's `deps.len()`.
    dep_names: Vec<String>,
    /// Whether the tree reads `time`.
    uses_time: bool,
}

impl CompiledExpr {
    /// Parse a CANONICAL expression (identifiers restricted to `__d{k}` and
    /// `time`). Anything else — a syntax error, an unknown identifier — is an
    /// `Err(String)`; the loader wraps it with a line number, serde with a
    /// deserialization error.
    pub fn compile(src: &str) -> Result<CompiledExpr, String> {
        let tree = build_operator_tree::<DefaultNumericTypes>(src)
            .map_err(|e| format!("malformed behavioral expression `{src}`: {e}"))?;
        let mut n_slots = 0usize;
        let mut uses_time = false;
        for ident in tree.iter_variable_identifiers() {
            if ident == "time" {
                uses_time = true;
            } else if let Some(k) = ident
                .strip_prefix("__d")
                .and_then(|d| d.parse::<usize>().ok())
            {
                n_slots = n_slots.max(k + 1);
            } else {
                return Err(format!(
                    "behavioral expression `{src}` contains non-canonical identifier \
                     `{ident}` (only `__d<k>` dependency slots and `time` survive \
                     loading; params fold to constants)"
                ));
            }
        }
        let dep_names = (0..n_slots).map(|k| format!("__d{k}")).collect();
        Ok(CompiledExpr {
            src: src.to_string(),
            tree,
            dep_names,
            uses_time,
        })
    }

    /// The canonical source text.
    pub fn src(&self) -> &str {
        &self.src
    }

    /// Number of dependency slots (`__d0 .. __d{n-1}`) the expression reads.
    pub fn n_slots(&self) -> usize {
        self.dep_names.len()
    }

    /// Evaluate at the given dependency-slot values and time. `deps.len()`
    /// must be at least [`CompiledExpr::n_slots`] (the device's `deps` vec is
    /// exactly that long by construction). Returns `Err` on an evaluation
    /// fault (evalexpr error, or a non-float result); the caller decides what
    /// a fault means (the stamp turns it into a device-named solver refusal).
    /// NOTE: IEEE float semantics apply inside — `1/0` is `inf`, `ln(-1)` is
    /// NaN, not an error — so callers must ALSO guard the returned value with
    /// `is_finite()`; this function reports only structural faults.
    pub fn eval(&self, deps: &[f64], time: f64) -> Result<f64, String> {
        if deps.len() < self.dep_names.len() {
            return Err(format!(
                "behavioral expression `{}` needs {} dependency values, got {}",
                self.src,
                self.dep_names.len(),
                deps.len()
            ));
        }
        let mut ctx = HashMapContext::<DefaultNumericTypes>::new();
        for (name, v) in self.dep_names.iter().zip(deps) {
            ctx.set_value(name.clone(), Value::from_float(*v))
                .map_err(|e| format!("behavioral eval context: {e}"))?;
        }
        if self.uses_time {
            ctx.set_value("time".to_string(), Value::from_float(time))
                .map_err(|e| format!("behavioral eval context: {e}"))?;
        }
        match self.tree.eval_with_context(&ctx) {
            Ok(Value::Float(f)) => Ok(f),
            Ok(Value::Int(i)) => Ok(i as f64),
            Ok(Value::Boolean(b)) => Ok(if b { 1.0 } else { 0.0 }),
            Ok(other) => Err(format!(
                "behavioral expression `{}` evaluated to a non-number ({other:?})",
                self.src
            )),
            Err(e) => Err(format!(
                "behavioral expression `{}` failed to evaluate: {e}",
                self.src
            )),
        }
    }
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

/// Serializes as the canonical source string (the plan is explicit: the
/// expression serializes as text and recompiles on deserialize).
impl Serialize for CompiledExpr {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.src)
    }
}

impl<'de> Deserialize<'de> for CompiledExpr {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let src = String::deserialize(deserializer)?;
        CompiledExpr::compile(&src).map_err(serde::de::Error::custom)
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
        assert_eq!(back.eval(&[1.0, 0.0], 2.0).unwrap(), v);
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
        // Structural contract: ln(-1) is a NaN VALUE, not an Err — the stamp
        // guards finiteness itself. Pin that so the guard's placement is a
        // documented behavior, not an accident.
        let e = CompiledExpr::compile("math::ln(__d0)").unwrap();
        assert!(e.eval(&[-1.0], 0.0).unwrap().is_nan());
        let d = CompiledExpr::compile("1.0/__d0").unwrap();
        assert!(d.eval(&[0.0], 0.0).unwrap().is_infinite());
    }
}
