//! Random value generation and shrinking, driven by type annotations.
//! Used by `property` blocks and by `cogito verify` (contract fuzzing).

use crate::ast::{ExprKind, FnDef};
use crate::diagnostic::Diagnostic;
use crate::interp::Ctrl;
use crate::interp::{Env, Interp, Rng};
use crate::span::Span;
use crate::types::{Ty, TypeDef, TypeKind};
use crate::value::*;
use std::collections::HashMap;
use std::rc::Rc;

const UNICODE: &[&str] = &["é", "ß", "中", "😀", "ñ", "Ω", "ü", "й"];

/// What `requires`/`where` clauses (or a type's invariant) say about one
/// input or field, read off comparisons with literals joined by `and`:
/// inclusive bounds on its value (Int or Float) and on its length
/// (`xs.len() >= 3`, `not s.is_empty()`).
#[derive(Clone, Default, Debug)]
pub struct Bound {
    pub int: Option<(Option<i64>, Option<i64>)>,
    pub float: Option<(Option<f64>, Option<f64>)>,
    pub len: Option<(Option<i64>, Option<i64>)>,
}

/// How to generate the values of a record type that has an invariant.
pub struct InvPlan {
    /// The invariant: its parameters are the fields, its `requires` the clauses.
    pub def: Rc<FnDef>,
    /// Bounds on each field, read off the clauses.
    pub bounds: Vec<Bound>,
    /// For a field that a clause `field == expr` defines in terms of the other
    /// fields (`size == items.len()`): that clause, and whether the field is
    /// on its left. Such a field is computed rather than generated.
    pub derived: Vec<Option<(usize, bool)>>,
}

pub struct Gen<'a> {
    pub it: &'a mut Interp,
    pub rng: &'a mut Rng,
    /// Occasionally produce extreme Ints (such as max_int) for top-level parameters.
    pub extremes: bool,
    /// The Ints, Floats and Strs generated so far for this case. A later
    /// parameter sometimes reuses one, so that a generated key is often in a
    /// generated map, and an element in a generated list.
    pub pool: Vec<Value>,
    /// Record types with invariants, by type id.
    pub plans: Rc<HashMap<u32, InvPlan>>,
    /// Set when no value of a record type that satisfies its invariant was
    /// found: the type and the clause broken most often. Unlike other
    /// generation errors this one may not happen again for the next case.
    pub missed: Option<(Rc<str>, String)>,
    /// Set when checking the invariant of a generated value failed with an
    /// error (a bug in the invariant): the error and the value.
    pub crashed: Option<(Box<Diagnostic>, Value)>,
    /// How many more candidate records with invariants this case may try:
    /// for recursive types, retries at each level would multiply.
    pub inv_tries: u32,
}

/// Extreme Float values, likely to expose overflow to infinity (`a + b`,
/// `x * x`) and underflow to zero.
const EXTREME_FLOATS: [f64; 10] = [1e308, -1e308, f64::MAX, f64::MIN, 1e154, -1e154, 1e-308, 5e-324, -5e-324, 1e-160];

/// Extreme Int values, likely to expose overflow.
const EXTREME_INTS: [i64; 9] = [i64::MAX, i64::MIN, i64::MAX - 1, i64::MIN + 1, 1 << 31, -(1 << 31), 1 << 32, 1 << 53, -(1 << 53)];

fn same_kind(ty: &Ty, v: &Value) -> bool {
    matches!((ty, v), (Ty::Int, Value::Int(_)) | (Ty::Float, Value::Float(_)) | (Ty::Str, Value::Str(_)))
}

fn mentions(ty: &Ty, id: u32) -> bool {
    match ty {
        Ty::Named { id: i, args, .. } => *i == id || args.iter().any(|a| mentions(a, id)),
        Ty::List(t) => mentions(t, id),
        Ty::Map(k, v) => mentions(k, id) || mentions(v, id),
        Ty::Tuple(ts) => ts.iter().any(|t| mentions(t, id)),
        Ty::Record(fs) => fs.iter().any(|(_, t)| mentions(t, id)),
        _ => false,
    }
}

impl<'a> Gen<'a> {
    pub fn new(it: &'a mut Interp, rng: &'a mut Rng, extremes: bool, plans: Rc<HashMap<u32, InvPlan>>) -> Gen<'a> {
        Gen { it, rng, extremes, pool: Vec::new(), plans, missed: None, crashed: None, inv_tries: 1000 }
    }

    /// A value of type `t` within the bounds `b`.
    pub fn bounded(&mut self, t: &Ty, b: &Bound, size: u32, depth: u32) -> Result<Value, String> {
        match (t, b) {
            (Ty::Int, Bound { int: Some((lo, hi)), .. }) => {
                let v = self.int_in(*lo, *hi, size);
                if self.pool.len() < 256 {
                    self.pool.push(v.clone());
                }
                Ok(v)
            }
            (Ty::Float, Bound { float: Some((lo, hi)), .. }) => Ok(self.float_in(*lo, *hi, size)),
            (Ty::Str | Ty::List(_) | Ty::Map(..) | Ty::Set(_), Bound { len: Some((lo, hi)), .. }) => self.sized(t, size, *lo, *hi),
            _ => self.value(t, size, depth),
        }
    }

    /// An extreme Int, `min_int` or `max_int` half of the time (so that 200
    /// cases almost surely try both).
    fn extreme(&mut self) -> i64 {
        if self.rng.below(2) == 0 {
            [i64::MAX, i64::MIN][self.rng.below(2)]
        } else {
            EXTREME_INTS[self.rng.below(EXTREME_INTS.len())]
        }
    }

    /// A field of a record at `depth`. An unbounded Int field of a parameter
    /// (or of a record in one) is now and then extreme, as parameters are:
    /// an account's balance near max_int finds overflow in a deposit.
    fn field(&mut self, t: &Ty, b: &Bound, size: u32, depth: u32) -> Result<Value, String> {
        if self.extremes && depth <= 1 && self.rng.below(100) < 3 {
            match t {
                Ty::Int if b.int.is_none() => return Ok(Value::Int(self.extreme())),
                Ty::Float if b.float.is_none() => return Ok(Value::Float(EXTREME_FLOATS[self.rng.below(EXTREME_FLOATS.len())])),
                _ => {}
            }
        }
        self.bounded(t, b, size, depth + 1)
    }

    /// A valid value obtained by shrinking `v` (which breaks an invariant):
    /// at each step, the largest shrunk candidate that is valid, or else a
    /// step down to the smallest candidate.
    fn shrink_to_valid(&mut self, mut v: Value) -> Option<Value> {
        let plans = self.plans.clone();
        for _ in 0..20 {
            let cands: Vec<Value> = shrink(&v).into_iter().take(64).collect();
            for c in cands.iter().rev() {
                if self.inv_tries == 0 {
                    return None;
                }
                self.inv_tries -= 1;
                self.it.ticks = 0;
                let depth = self.it.stack.len();
                let r = repair(self.it, &plans, c);
                self.it.stack.truncate(depth);
                if r.is_some() {
                    return r;
                }
            }
            v = cands.into_iter().next()?;
        }
        None
    }

    /// A record of a type with an invariant: generated within the bounds the
    /// invariant states, with fields it defines computed, and retried until
    /// the invariant holds.
    fn valid_record(&mut self, td: &Rc<TypeDef>, plan: &InvPlan, tys: &[Ty], size: u32, depth: u32) -> Result<Value, String> {
        let TypeKind::Record { fields, .. } = &td.kind else { unreachable!() };
        let mut broken: Vec<(String, u32)> = Vec::new();
        // A record inside another gets fewer tries: the outer one retries too.
        let attempts = if depth == 0 { 100 } else { 10 };
        // A recursive type (a trie, a tree) nested in itself gets much
        // smaller, or it would hold thousands of nodes.
        let size = if depth > 0 && tys.iter().any(|t| mentions(t, td.id)) { (size / 4).max(1) } else { size };
        'attempt: for attempt in 0..attempts {
            // (Only failed tries count against the budget.)
            if self.inv_tries == 0 {
                break;
            }
            // Smaller values satisfy more invariants (an empty edge list is
            // always in range), so retries shrink: steadily at the top, by
            // half each time inside another record (which is retried too).
            let size = if depth == 0 { (size * (attempts - attempt) / attempts).max(1) } else { (size >> attempt.min(31)).max(1) };
            // Checking a candidate gets the step budget of a whole case.
            self.it.ticks = 0;
            let mut vals = Vec::with_capacity(tys.len());
            for (i, t) in tys.iter().enumerate() {
                vals.push(match plan.derived[i] {
                    Some(_) => Value::Unit,
                    None => match self.field(t, &plan.bounds[i], size, depth) {
                        Ok(v) => v,
                        // No valid record nested in this one: try again.
                        Err(_) if self.missed.is_some() && self.crashed.is_none() => {
                            self.missed = None;
                            self.inv_tries = self.inv_tries.saturating_sub(1);
                            continue 'attempt;
                        }
                        Err(e) => return Err(e),
                    },
                });
            }
            if !compute_derived(self.it, plan, tys, &mut vals) {
                self.inv_tries = self.inv_tries.saturating_sub(1);
                continue;
            }
            let v = Value::Record(Rc::new(RecordVal { ty: Some(td.clone()), names: fields.clone(), values: vals }));
            let depth_before = self.it.stack.len();
            let r = self.it.broken_invariant(&v, Span::default());
            self.it.stack.truncate(depth_before);
            match r {
                Ok(None) => return Ok(v),
                Ok(Some(b)) => {
                    self.inv_tries = self.inv_tries.saturating_sub(1);
                    match broken.iter_mut().find(|(c, _)| *c == b.clause) {
                        Some((_, n)) => *n += 1,
                        None => broken.push((b.clause, 1)),
                    }
                    // As a last resort, look for a valid value among the
                    // smaller versions of this one (a map without its bad
                    // entries, a shorter list).
                    if attempt + 1 == attempts {
                        if let Some(ok) = self.shrink_to_valid(v) {
                            return Ok(ok);
                        }
                    }
                }
                Err(Ctrl::Error(d)) => {
                    self.crashed = Some((d, v));
                    return Err(format!("checking the invariant of `{}` failed", td.name));
                }
                Err(_) => {}
            }
        }
        let clause = broken.iter().max_by_key(|(_, n)| *n).map(|(c, _)| c.clone()).unwrap_or_default();
        let msg = if clause.is_empty() {
            format!(
                "cannot generate {} `{}` that satisfies its invariant (`where`): checking it failed with an error",
                crate::diagnostic::a_an(&td.name),
                td.name
            )
        } else {
            format!(
                "cannot generate {} `{}` that satisfies its invariant: random values rarely satisfy `where {}`;\n      \
                 test functions on it with a `property` that builds valid values",
                crate::diagnostic::a_an(&td.name),
                td.name,
                clause
            )
        };
        self.missed = Some((td.name.clone(), clause));
        Err(msg)
    }

    pub fn value(&mut self, ty: &Ty, size: u32, depth: u32) -> Result<Value, String> {
        let scalar = matches!(ty, Ty::Int | Ty::Float | Ty::Str);
        if scalar && depth == 0 {
            if let Some(v) = self.reuse(|v| same_kind(ty, v)) {
                return Ok(v);
            }
        }
        let v = self.fresh(ty, size, depth)?;
        if scalar && self.pool.len() < 256 {
            self.pool.push(v.clone());
        }
        Ok(v)
    }

    /// Sometimes (30% of the time) a value from the pool that `ok` accepts.
    fn reuse(&mut self, ok: impl Fn(&Value) -> bool) -> Option<Value> {
        if self.pool.is_empty() || self.rng.below(100) >= 30 {
            return None;
        }
        // A few random picks rather than a scan of the whole pool.
        for _ in 0..8 {
            let v = &self.pool[self.rng.below(self.pool.len())];
            if ok(v) {
                return Some(v.clone());
            }
        }
        None
    }

    fn fresh(&mut self, ty: &Ty, size: u32, depth: u32) -> Result<Value, String> {
        if depth > 48 {
            return Err(format!("cannot generate a finite value of type `{}`: it contains itself with no non-recursive alternative", ty));
        }
        // Collections deep inside a value are kept empty, so that recursive
        // types such as `{ kids: List[Tree] }` stay finite.
        let size = if depth >= 6 { 0 } else { size.max(1) as i64 };
        Ok(match ty {
            Ty::Unit => Value::Unit,
            Ty::Bool => Value::Bool(self.rng.next_u64() & 1 == 1),
            Ty::Int => {
                let r = self.rng.below(100);
                if self.extremes && depth == 0 && r < 6 {
                    Value::Int(self.extreme())
                } else if r < 15 {
                    Value::Int([0, 1, -1, 2, -2, 3, 10, -10][self.rng.below(8)])
                } else if r < 85 {
                    Value::Int(self.rng.range(-size, size))
                } else if r < 97 {
                    Value::Int(self.rng.range(-1000, 1000))
                } else {
                    Value::Int(self.rng.range(-100_000, 100_000))
                }
            }
            Ty::Float => {
                let r = self.rng.below(100);
                if self.extremes && depth == 0 && r < 6 {
                    Value::Float(EXTREME_FLOATS[self.rng.below(EXTREME_FLOATS.len())])
                } else if r < 20 {
                    const SPECIAL: [f64; 12] = [0.0, 1.0, -1.0, 0.5, 0.1, 0.2, 0.3, -0.1, 1e-9, 1e9, 2.5, 1.0 / 3.0];
                    Value::Float(SPECIAL[self.rng.below(SPECIAL.len())])
                } else if r < 85 {
                    let whole = self.rng.range(-size, size) as f64;
                    let frac = (self.rng.below(1000) as f64) / 1000.0;
                    Value::Float(whole + frac)
                } else {
                    Value::Float((self.rng.float() - 0.5) * 2.0e6)
                }
            }
            Ty::Str => {
                let n = self.rng.below(size as usize + 1);
                self.string(n)
            }
            Ty::Range => {
                let start = self.rng.range(-size, size);
                let len = self.rng.range(0, size);
                Value::Range(Rc::new(RangeVal { start, end: Some((start + len) as i128) }))
            }
            Ty::List(t) => {
                let n = self.rng.below(size as usize + 1);
                self.list(t, n, size as u32, depth)?
            }
            Ty::Map(k, v) => {
                let n = self.rng.below(size as usize + 1);
                self.map(k, v, n, size as u32, depth)?
            }
            Ty::Set(t) => {
                let n = self.rng.below(size as usize + 1);
                match self.map(t, &Ty::Unit, n, size as u32, depth)? {
                    Value::Map(m) => Value::Set(m),
                    v => v,
                }
            }
            Ty::Tuple(ts) => {
                let mut xs = Vec::new();
                for t in ts {
                    xs.push(self.value(t, size as u32, depth + 1)?);
                }
                Value::tuple(xs)
            }
            Ty::Record(fs) => {
                let mut names = Vec::new();
                let mut vals = Vec::new();
                for (n, t) in fs {
                    names.push(n.clone());
                    vals.push(self.value(t, size as u32, depth + 1)?);
                }
                Value::Record(Rc::new(RecordVal { ty: None, names: names.into(), values: vals }))
            }
            Ty::Named { id, args, .. } => {
                let td = self.it.ctx.types[*id as usize].clone();
                self.named(&td, args, size as u32, depth)?
            }
            Ty::Fn(..) => return Err(format!("cannot generate random functions (type `{}`)", ty)),
            // Generic type parameters are instantiated with Int.
            Ty::Generic(_) | Ty::Param(..) => return self.value(&Ty::Int, size as u32, depth),
            Ty::Any => return Err(format!("cannot generate values of type `{}`; give the input a concrete type such as Int or List[Str]", ty)),
        })
    }

    fn string(&mut self, n: usize) -> Value {
        let mut s = String::new();
        for _ in 0..n {
            let r = self.rng.below(100);
            if r < 62 {
                s.push((b'a' + self.rng.below(26) as u8) as char);
            } else if r < 74 {
                // Whitespace, including line breaks, finds bugs in text code.
                s.push([' ', ' ', ' ', '\n', '\t'][self.rng.below(5)]);
            } else if r < 88 {
                s.push((b' ' + self.rng.below(95) as u8) as char);
            } else {
                s.push_str(UNICODE[self.rng.below(UNICODE.len())]);
            }
        }
        Value::str(s)
    }

    fn list(&mut self, t: &Ty, n: usize, size: u32, depth: u32) -> Result<Value, String> {
        let inner = (size / 2).max(2);
        let mut xs = Vec::with_capacity(n);
        // Bugs cluster around duplicates, so sometimes draw the elements
        // from a small pool of values.
        let mode = self.rng.below(100);
        if mode < 25 && n > 1 {
            let pool_size = if mode < 10 { 1 } else { 2 + self.rng.below(2) };
            let mut pool = Vec::with_capacity(pool_size);
            for _ in 0..pool_size {
                pool.push(self.value(t, inner, depth + 1)?);
            }
            for _ in 0..n {
                xs.push(pool[self.rng.below(pool.len())].clone());
            }
        } else {
            for _ in 0..n {
                xs.push(self.value(t, inner, depth + 1)?);
            }
        }
        Ok(Value::list(xs))
    }

    fn map(&mut self, k: &Ty, v: &Ty, n: usize, size: u32, depth: u32) -> Result<Value, String> {
        let inner = (size / 2).max(2);
        let mut m = MapVal::new();
        // Keys may collide; keep drawing (a bounded number of times) to reach `n`.
        let mut tries = 0;
        while m.len() < n && tries < n * 4 + 8 {
            tries += 1;
            let key = self.value(k, inner, depth + 1)?;
            let val = self.value(v, inner, depth + 1)?;
            m.insert(key, val);
        }
        Ok(Value::Map(Rc::new(m)))
    }

    /// A Str, List or Map whose length lies within inclusive bounds read off
    /// a `where`/`requires` clause such as `xs.len() >= 3`.
    pub fn sized(&mut self, ty: &Ty, size: u32, lo: Option<i64>, hi: Option<i64>) -> Result<Value, String> {
        let lo = lo.unwrap_or(0).clamp(0, 10_000) as usize;
        let hi = hi.map_or(lo + size as usize, |h| h.clamp(0, 10_000) as usize);
        if hi < lo {
            return self.value(ty, size, 0);
        }
        let n = lo + self.rng.below((hi - lo).min(size as usize) + 1);
        match ty {
            Ty::Str => Ok(self.string(n)),
            Ty::List(t) => self.list(t, n, size.max(2), 0),
            Ty::Map(k, v) => self.map(k, v, n, size.max(2), 0),
            Ty::Set(t) => match self.map(t, &Ty::Unit, n, size.max(2), 0)? {
                Value::Map(m) => Ok(Value::Set(m)),
                v => Ok(v),
            },
            _ => self.value(ty, size, 0),
        }
    }

    /// A Float within optional bounds.
    pub fn float_in(&mut self, lo: Option<f64>, hi: Option<f64>, size: u32) -> Value {
        let natural = match self.value(&Ty::Float, size, 1) {
            Ok(Value::Float(f)) => f,
            _ => 0.0,
        };
        let r = self.rng.below(100);
        if self.extremes && r < 4 {
            let ok: Vec<f64> = EXTREME_FLOATS.iter().copied().filter(|x| lo.is_none_or(|l| *x >= l) && hi.is_none_or(|h| *x <= h)).collect();
            if !ok.is_empty() {
                return Value::Float(ok[self.rng.below(ok.len())]);
            }
        }
        let f = match (lo, hi) {
            (Some(l), Some(h)) if l <= h => {
                if r < 10 {
                    if r < 5 {
                        l
                    } else {
                        h
                    }
                } else {
                    l + self.rng.float() * (h - l)
                }
            }
            (Some(l), None) => {
                if r < 10 {
                    l
                } else {
                    l + natural.abs()
                }
            }
            (None, Some(h)) => {
                if r < 10 {
                    h
                } else {
                    h - natural.abs()
                }
            }
            _ => natural,
        };
        Value::Float(f)
    }

    /// An Int within optional inclusive bounds, favouring the boundaries.
    pub fn int_in(&mut self, lo: Option<i64>, hi: Option<i64>, size: u32) -> Value {
        let fits = |x: i64| lo.is_none_or(|l| x >= l) && hi.is_none_or(|h| x <= h);
        if let Some(v) = self.reuse(|v| matches!(v, Value::Int(x) if fits(*x))) {
            return v;
        }
        let natural = match self.value(&Ty::Int, size, 1) {
            Ok(Value::Int(n)) => n,
            _ => 0,
        };
        let r = self.rng.below(100);
        if self.extremes && r < 4 {
            let ok: Vec<i64> = EXTREME_INTS.iter().copied().filter(|x| lo.is_none_or(|l| *x >= l) && hi.is_none_or(|h| *x <= h)).collect();
            if !ok.is_empty() {
                return Value::Int(ok[self.rng.below(ok.len())]);
            }
        }
        let v = match (lo, hi) {
            (Some(l), Some(h)) if l <= h => {
                // A wide range (`n <= 1_000_000`, a safety bound) is not worth
                // its upper edge: values that large mostly make cases slow.
                let wide = h.saturating_sub(l) > 100_000;
                if r < 10 {
                    let edges = if wide {
                        [l, l.saturating_add(1), l, l.saturating_add(2)]
                    } else {
                        [l, l.saturating_add(1).min(h), h.saturating_sub(1).max(l), h]
                    };
                    edges[self.rng.below(4)]
                } else if r < 25 {
                    self.rng.range(l, if wide { l.saturating_add(10_000) } else { h })
                } else {
                    // Mostly values of a size that grows during the run, measured
                    // from whichever bound is closer to zero.
                    let span = natural.unsigned_abs().min(i64::MAX as u64) as i64;
                    if l >= 0 || (h >= 0 && l.unsigned_abs() > h.unsigned_abs()) {
                        if l >= 0 {
                            l.saturating_add(span).min(h)
                        } else {
                            natural.clamp(l, h)
                        }
                    } else {
                        h.saturating_sub(span).max(l)
                    }
                }
            }
            (Some(l), None) => {
                if r < 20 {
                    l
                } else {
                    l.saturating_add(natural.unsigned_abs().min(i64::MAX as u64) as i64)
                }
            }
            (None, Some(h)) => {
                if r < 20 {
                    h
                } else {
                    h.saturating_sub(natural.unsigned_abs().min(i64::MAX as u64) as i64)
                }
            }
            _ => natural,
        };
        Value::Int(v)
    }

    fn named(&mut self, td: &Rc<TypeDef>, args: &[Ty], size: u32, depth: u32) -> Result<Value, String> {
        let args: Vec<Ty> = if args.is_empty() { td.params.iter().map(|_| Ty::Int).collect() } else { args.to_vec() };
        match &td.kind {
            TypeKind::Record { fields, tys } => {
                let tys: Vec<Ty> = tys.iter().map(|t| t.subst(&args)).collect();
                let plans = self.plans.clone();
                if let Some(plan) = plans.get(&td.id) {
                    return self.valid_record(td, plan, &tys, size, depth);
                }
                let size = if depth > 0 && tys.iter().any(|t| mentions(t, td.id)) { (size / 4).max(1) } else { size };
                let mut vals = Vec::new();
                for t in &tys {
                    vals.push(self.field(t, &Bound::default(), size, depth)?);
                }
                Ok(Value::Record(Rc::new(RecordVal { ty: Some(td.clone()), names: fields.clone(), values: vals })))
            }
            TypeKind::Enum { variants } => {
                let simple: Vec<usize> = (0..variants.len()).filter(|i| !variants[*i].tys.iter().any(|t| mentions(t, td.id))).collect();
                let tag = if (depth >= 4 || size <= 1) && !simple.is_empty() {
                    simple[self.rng.below(simple.len())]
                } else {
                    self.rng.below(variants.len())
                };
                let v = &variants[tag];
                let mut vals = Vec::new();
                for t in &v.tys {
                    vals.push(self.value(&t.subst(&args), (size / 2).max(1), depth + 1)?);
                }
                Ok(Value::Variant(Rc::new(VariantVal { ty: td.clone(), tag: tag as u32, values: vals })))
            }
        }
    }
}

/// Fill in the fields of a record that its invariant's `field == expr`
/// clauses define, from the other fields.
pub fn compute_derived(it: &mut Interp, plan: &InvPlan, tys: &[Ty], vals: &mut [Value]) -> bool {
    if plan.derived.iter().all(|d| d.is_none()) {
        return true;
    }
    let def = plan.def.clone();
    let mut env = Env::new(def.num_slots);
    for (p, v) in def.params.iter().zip(vals.iter()) {
        env.locals[p.slot as usize] = v.clone();
    }
    let depth_before = it.stack.len();
    let mut ok = true;
    for (i, d) in plan.derived.iter().enumerate() {
        let Some((ci, left)) = *d else { continue };
        let ExprKind::Binary { lhs, rhs, .. } = &def.requires[ci].kind else { continue };
        let e = if left { rhs } else { lhs };
        let v = match it.eval(e, &mut env).ok().and_then(|v| it.conform(v, &tys[i]).ok()) {
            Some(v) => v,
            None => {
                ok = false;
                break;
            }
        };
        env.locals[def.params[i].slot as usize] = v.clone();
        vals[i] = v;
    }
    it.stack.truncate(depth_before);
    ok
}

/// A shrunk value made valid again: the fields that invariants define are
/// recomputed (a shorter `adj` gets the matching `n`); `None` if some
/// record still breaks its invariant.
pub fn repair(it: &mut Interp, plans: &HashMap<u32, InvPlan>, v: &Value) -> Option<Value> {
    if plans.is_empty() {
        return Some(v.clone());
    }
    Some(match v {
        Value::Record(r) => {
            let mut values = Vec::with_capacity(r.values.len());
            for x in &r.values {
                values.push(repair(it, plans, x)?);
            }
            if let Some(plan) = r.ty.as_ref().and_then(|td| plans.get(&td.id)) {
                let td = r.ty.clone().unwrap();
                let TypeKind::Record { tys, .. } = &td.kind else { return None };
                let tys: Vec<Ty> = tys.iter().map(|t| t.subst(&[])).collect();
                it.ticks = 0;
                if !compute_derived(it, plan, &tys, &mut values) {
                    return None;
                }
            }
            let nv = Value::Record(Rc::new(RecordVal { ty: r.ty.clone(), names: r.names.clone(), values }));
            let depth = it.stack.len();
            it.ticks = 0;
            let ok = matches!(it.broken_invariant(&nv, Span::default()), Ok(None));
            it.stack.truncate(depth);
            if !ok {
                return None;
            }
            nv
        }
        Value::List(xs) => {
            let mut out = Vec::with_capacity(xs.len());
            for x in xs.iter() {
                out.push(repair(it, plans, x)?);
            }
            Value::list(out)
        }
        Value::Tuple(xs) => {
            let mut out = Vec::with_capacity(xs.len());
            for x in xs.iter() {
                out.push(repair(it, plans, x)?);
            }
            Value::tuple(out)
        }
        Value::Variant(vv) => {
            let mut values = Vec::with_capacity(vv.values.len());
            for x in &vv.values {
                values.push(repair(it, plans, x)?);
            }
            Value::Variant(Rc::new(VariantVal { ty: vv.ty.clone(), tag: vv.tag, values }))
        }
        Value::Map(m) | Value::Set(m) => {
            if m.entries.iter().any(|(k, x)| repair(it, plans, k).is_none() || repair(it, plans, x).is_none()) {
                return None;
            }
            v.clone()
        }
        _ => v.clone(),
    })
}

/// Candidate "simpler" values, most aggressive first.
pub fn shrink(v: &Value) -> Vec<Value> {
    let mut out = Vec::new();
    match v {
        Value::Int(n) => {
            let n = *n;
            if n != 0 {
                out.push(Value::Int(0));
                if n < 0 && n != i64::MIN {
                    out.push(Value::Int(-n));
                }
                // Move toward zero by half the distance, then a quarter, and
                // so on down to a single step: shrinking takes logarithmic
                // rather than linear time to find the boundary.
                let mut d = n / 2;
                while d != 0 {
                    out.push(Value::Int(n - d));
                    d /= 2;
                }
            }
        }
        Value::Float(f) => {
            let f = *f;
            if f != 0.0 {
                out.push(Value::Float(0.0));
                for nice in [1.0, 0.5, 0.1] {
                    if nice < f.abs() {
                        out.push(Value::Float(nice * f.signum()));
                    }
                }
                if f < 0.0 {
                    out.push(Value::Float(-f));
                }
                if f.trunc() != f {
                    out.push(Value::Float(f.trunc()));
                }
                if f.abs() > 1.0 {
                    out.push(Value::Float((f / 2.0).trunc()));
                }
            }
        }
        Value::Bool(true) => out.push(Value::Bool(false)),
        Value::Str(s) => {
            let cs: Vec<char> = s.chars().collect();
            if !cs.is_empty() {
                out.push(Value::str(""));
                if cs.len() > 1 {
                    out.push(Value::str(cs[..cs.len() / 2].iter().collect::<String>()));
                    out.push(Value::str(cs[cs.len() / 2..].iter().collect::<String>()));
                }
                for i in 0..cs.len().min(32) {
                    let mut c = cs.clone();
                    c.remove(i);
                    out.push(Value::str(c.into_iter().collect::<String>()));
                }
                for i in 0..cs.len().min(16) {
                    if cs[i] != 'a' {
                        let mut c = cs.clone();
                        c[i] = 'a';
                        out.push(Value::str(c.into_iter().collect::<String>()));
                    }
                }
            }
        }
        Value::List(xs) => {
            if !xs.is_empty() {
                out.push(Value::list(vec![]));
                if xs.len() > 1 {
                    out.push(Value::list(xs[..xs.len() / 2].to_vec()));
                    out.push(Value::list(xs[xs.len() / 2..].to_vec()));
                }
                for i in 0..xs.len().min(32) {
                    let mut c = xs.to_vec();
                    c.remove(i);
                    out.push(Value::list(c));
                }
                for i in 0..xs.len().min(16) {
                    for s in shrink(&xs[i]).into_iter().take(4) {
                        let mut c = xs.to_vec();
                        c[i] = s;
                        out.push(Value::list(c));
                    }
                }
            }
        }
        Value::Tuple(xs) => {
            for i in 0..xs.len() {
                for s in shrink(&xs[i]).into_iter().take(6) {
                    let mut c = xs.to_vec();
                    c[i] = s;
                    out.push(Value::tuple(c));
                }
            }
        }
        Value::Set(m) => {
            if !m.is_empty() {
                out.push(Value::Set(Rc::new(MapVal::new())));
                for (k, _) in m.entries.iter().take(16) {
                    let mut c = (**m).clone();
                    c.remove(k);
                    out.push(Value::Set(Rc::new(c)));
                }
            }
        }
        Value::Map(m) => {
            if !m.is_empty() {
                out.push(Value::Map(Rc::new(MapVal::new())));
                for (k, _) in m.entries.iter().take(16) {
                    let mut c = (**m).clone();
                    c.remove(k);
                    out.push(Value::Map(Rc::new(c)));
                }
                for (k, x) in m.entries.iter().take(8) {
                    for s in shrink(x).into_iter().take(3) {
                        let mut c = (**m).clone();
                        c.insert(k.clone(), s);
                        out.push(Value::Map(Rc::new(c)));
                    }
                }
            }
        }
        Value::Record(r) => {
            for i in 0..r.values.len() {
                for s in shrink(&r.values[i]).into_iter().take(16) {
                    let mut c = (**r).clone();
                    c.values[i] = s;
                    out.push(Value::Record(Rc::new(c)));
                }
            }
        }
        Value::Variant(vv) => {
            // Simpler variants: nullary ones declared earlier, then sub-terms of the same type.
            if let TypeKind::Enum { variants } = &vv.ty.kind {
                for (tag, var) in variants.iter().enumerate() {
                    if var.tys.is_empty() && (tag as u32) != vv.tag {
                        out.push(Value::Variant(Rc::new(VariantVal { ty: vv.ty.clone(), tag: tag as u32, values: vec![] })));
                    }
                }
            }
            for x in &vv.values {
                if let Value::Variant(sub) = x {
                    if sub.ty.id == vv.ty.id {
                        out.push(x.clone());
                    }
                }
            }
            for i in 0..vv.values.len() {
                for s in shrink(&vv.values[i]).into_iter().take(6) {
                    let mut c = (**vv).clone();
                    c.values[i] = s;
                    out.push(Value::Variant(Rc::new(c)));
                }
            }
        }
        Value::Range(r) => {
            if let Some(e) = r.end {
                if e > r.start as i128 {
                    out.push(Value::Range(Rc::new(RangeVal { start: r.start, end: Some(r.start as i128) })));
                    out.push(Value::Range(Rc::new(RangeVal { start: r.start, end: Some(e - 1) })));
                }
            }
            if r.start != 0 {
                let len = r.end.map(|e| e - r.start as i128);
                out.push(Value::Range(Rc::new(RangeVal { start: 0, end: len })));
            }
        }
        _ => {}
    }
    out
}
