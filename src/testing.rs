//! `cogito test` (unit tests and property tests) and `cogito verify`
//! (contract checking by random testing).

use crate::ast::{BinOp, Expr, ExprKind, FnDef, Item, Param, Program, UnOp, VarRes};
use crate::diagnostic::{Colors, Diagnostic};
use crate::interp::{Ctrl, Env, Interp, Rng};
use crate::proptest::{shrink, Bound, Gen, InvPlan};
use crate::span::Span;
use crate::types::Ty;
use crate::value::{repr, Closure, Value};
use std::collections::HashMap;
use std::rc::Rc;

pub struct Options {
    pub seed: Option<u64>,
    pub cases: u32,
    pub filter: Option<String>,
    pub color: bool,
    /// verify: also check functions that have no contracts.
    pub all: bool,
    /// Step budget per generated test case.
    pub budget: u64,
}

impl Default for Options {
    fn default() -> Self {
        Options { seed: None, cases: 100, filter: None, color: false, all: false, budget: 10_000_000 }
    }
}

#[derive(Default, Debug, Clone, Copy)]
pub struct Summary {
    pub passed: u32,
    pub failed: u32,
    /// Properties for which too few generated inputs satisfied `where`/`requires`.
    pub gave_up: u32,
    pub skipped: u32,
    pub cases: u64,
}

impl Summary {
    pub fn add(&mut self, o: Summary) {
        self.passed += o.passed;
        self.failed += o.failed;
        self.gave_up += o.gave_up;
        self.skipped += o.skipped;
        self.cases += o.cases;
    }
}

enum Outcome {
    Pass,
    Discard,
    Fail(Box<Diagnostic>),
}

fn name_seed(name: &str) -> u64 {
    // FNV-1a: a stable seed per test name, so runs are reproducible.
    let mut h: u64 = 0xcbf29ce484222325;
    for b in name.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn requires_hold(it: &mut Interp, c: &Rc<Closure>, args: &[Value]) -> Result<bool, Ctrl> {
    let def = &c.def;
    if def.requires.is_empty() {
        return Ok(true);
    }
    let mut env = Env::new(def.num_slots);
    env.closure = Some(c.clone());
    for (p, a) in def.params.iter().zip(args) {
        let v = match &p.ty {
            Some(t) => it.conform(a.clone(), &t.ty).unwrap_or_else(|_| a.clone()),
            None => a.clone(),
        };
        env.locals[p.slot as usize] = v;
    }
    for p in &def.params {
        if let Some(pat) = &p.pat {
            let v = env.locals[p.slot as usize].clone();
            if !it.match_pattern(pat, &v, &mut env) {
                return Ok(false);
            }
        }
    }
    for r in &def.requires {
        match it.eval(r, &mut env)? {
            Value::Bool(true) => {}
            _ => return Ok(false),
        }
    }
    Ok(true)
}

/// A test (or property) also fails if it returns `Err(..)`, or if a `?`
/// returned early from it.
fn check_test_result(it: &Interp, r: Result<Value, Ctrl>) -> Result<Value, Ctrl> {
    let v = r?;
    let via_try = it.last_try_return;
    if via_try.is_some() || v.is_result() && matches!(&v, Value::Variant(vv) if vv.tag == 1) {
        let how = if via_try.is_some() { "`?` returned early" } else { "the test returned an error" };
        let mut d = Diagnostic::error("E0300", format!("{}: {}", how, crate::value::short_repr(&v)));
        if let Some(sp) = via_try {
            d = d.at(sp).label("returned early here");
        }
        return Err(Ctrl::Error(Box::new(d)));
    }
    Ok(v)
}

fn run_case(it: &mut Interp, c: &Rc<Closure>, args: Vec<Value>) -> Outcome {
    let depth = it.stack.len();
    it.ticks = 0;
    let out = match requires_hold(it, c, &args) {
        Ok(false) => Outcome::Discard,
        Err(Ctrl::Error(d)) => Outcome::Fail(d),
        Err(_) => Outcome::Discard,
        Ok(true) => {
            it.catching += 1;
            let r = it.call_case(c, args);
            it.catching -= 1;
            // For properties, an early `?` or an Err result is a failure.
            let r = if c.def.ensures.is_empty() && c.def.global_slot.is_none() { check_test_result(it, r) } else { r };
            match r {
                Ok(_) => Outcome::Pass,
                Err(Ctrl::Error(d)) => Outcome::Fail(d),
                Err(_) => Outcome::Pass,
            }
        }
    };
    it.stack.truncate(depth);
    out
}

struct Failure {
    /// Empty when checking the invariant of `generated` (a value being
    /// generated for an argument) failed.
    args: Vec<Value>,
    generated: Option<Value>,
    diag: Box<Diagnostic>,
    shrinks: u32,
    after: u32,
}

enum PropOutcome {
    /// `missed`: attempts where no record satisfying its invariant was
    /// found (counted in `discarded`), with the clause broken most often.
    Passed {
        cases: u32,
        discarded: u32,
        missed: Option<(u32, String)>,
    },
    GaveUp {
        cases: u32,
        discarded: u32,
        missed: Option<(u32, String)>,
    },
    Failed(Failure),
    CannotGenerate(String),
}

/// How to show a parameter to the user: its name, or its pattern's text.
fn param_label(it: &Interp, p: &Param) -> String {
    match &p.pat {
        Some(pat) => it.ctx.sm.snippet(pat.span).to_string(),
        None => p.name.to_string(),
    }
}

fn param_types(it: &Interp, def: &FnDef) -> Result<Vec<Ty>, String> {
    let mut tys = Vec::new();
    for p in &def.params {
        match &p.ty {
            Some(t) => tys.push(t.ty.clone()),
            None => return Err(format!("parameter `{}` has no type annotation", param_label(it, p))),
        }
    }
    Ok(tys)
}

fn quickcheck(it: &mut Interp, def: &Rc<FnDef>, cases: u32, seed: u64, budget: u64, extremes: bool) -> PropOutcome {
    let saved_budget = it.budget;
    it.budget = Some(budget);
    let r = quickcheck_inner(it, def, cases, seed, extremes);
    it.budget = saved_budget;
    r
}

fn quickcheck_inner(it: &mut Interp, def: &Rc<FnDef>, cases: u32, seed: u64, extremes: bool) -> PropOutcome {
    let tys = match param_types(it, def) {
        Ok(t) => t,
        Err(m) => return PropOutcome::CannotGenerate(m),
    };
    let bounds = bounds(def);
    let plans = invariant_plans(it);
    let c = Rc::new(Closure { def: def.clone(), captures: vec![] });
    let mut rng = Rng::new(seed);
    let mut passed = 0u32;
    let mut discarded = 0u32;
    let mut missed: Option<(u32, String)> = None;
    let was_silent = it.silent;
    it.silent = true;
    let result = loop {
        if passed >= cases {
            break PropOutcome::Passed { cases: passed, discarded, missed };
        }
        if discarded > cases.max(10) * 20 {
            break PropOutcome::GaveUp { cases: passed, discarded, missed };
        }
        // Inputs grow during the run. Discarded attempts count too, so that a
        // filter such as `xs.len() >= 5` eventually sees inputs that pass it.
        let size = 2 + (passed + discarded).min(cases) * 40 / cases.max(1);
        let mut args = Vec::with_capacity(tys.len());
        let mut gen = Gen::new(it, &mut rng, extremes, plans.clone());
        let mut gen_err = None;
        for (t, b) in tys.iter().zip(&bounds) {
            let v = gen.bounded(t, b, size, 0);
            match v {
                Ok(v) => args.push(v),
                Err(m) => {
                    gen_err = Some(m);
                    break;
                }
            }
        }
        if let Some((diag, v)) = gen.crashed.take() {
            break PropOutcome::Failed(Failure { args: vec![], generated: Some(v), diag, shrinks: 0, after: passed + 1 });
        }
        if let Some(m) = gen_err {
            // A record whose invariant random values rarely satisfy: skip
            // this attempt, unless no attempt ever succeeds.
            match gen.missed.take() {
                Some((ty, clause)) => {
                    let n = missed.as_ref().map_or(0, |m| m.0) + 1;
                    if passed == 0 && n >= 5 {
                        break PropOutcome::CannotGenerate(m);
                    }
                    let what = if clause.is_empty() {
                        format!("`{}` satisfying its invariant", ty)
                    } else {
                        format!("`{}` satisfying `where {}`", ty, clause)
                    };
                    missed = Some((n, what));
                    discarded += 1;
                    continue;
                }
                None => break PropOutcome::CannotGenerate(m),
            }
        }
        match run_case(it, &c, args.clone()) {
            Outcome::Pass => passed += 1,
            Outcome::Discard => discarded += 1,
            Outcome::Fail(d) => {
                let (args, diag, shrinks) = shrink_failure(it, &c, args, d, &plans);
                break PropOutcome::Failed(Failure { args, generated: None, diag, shrinks, after: passed + 1 });
            }
        }
    };
    it.silent = was_silent;
    result
}

/// How to generate each record type that has an invariant: bounds on its
/// fields and the fields its clauses define (`size == items.len()`).
fn invariant_plans(it: &Interp) -> Rc<HashMap<u32, InvPlan>> {
    let mut out = HashMap::new();
    for (id, def) in &it.ctx.invariants {
        let mut derived = vec![None; def.params.len()];
        for (ci, c) in def.requires.iter().enumerate() {
            let ExprKind::Binary { op: BinOp::Eq, lhs, rhs } = &c.kind else { continue };
            for (side, other, left) in [(lhs, rhs, true), (rhs, lhs, false)] {
                if let Some(i) = def.params.iter().position(|p| matches!(&side.kind, ExprKind::Var(v) if v.res == VarRes::Local(p.slot))) {
                    if derived[i].is_none() && !uses_slot(other, def.params[i].slot) {
                        derived[i] = Some((ci, left));
                        break;
                    }
                }
            }
        }
        // A computed field's bounds apply to what it is computed from:
        // with `x == y where x > 1000`, generate `y > 1000`; with
        // `size == items.len() where size >= 3`, at least 3 items.
        let mut bs = bounds(def);
        for (i, d) in derived.iter().enumerate() {
            let Some((ci, left)) = *d else { continue };
            let ExprKind::Binary { lhs, rhs, .. } = &def.requires[ci].kind else { continue };
            let other = if left { rhs } else { lhs };
            let (int, float) = (bs[i].int, bs[i].float);
            match source_field(def, other) {
                Some((j, false)) if derived[j].is_none() => {
                    bs[j].int = meet(bs[j].int, int);
                    bs[j].float = meet_f(bs[j].float, float);
                }
                Some((j, true)) if derived[j].is_none() => bs[j].len = meet(bs[j].len, int),
                _ => {}
            }
        }
        out.insert(*id, InvPlan { def: def.clone(), bounds: bs, derived });
    }
    Rc::new(out)
}

/// The parameter that `e` is (`y`), or whose length it is (`items.len()`,
/// `len(items)`): its index, and whether it is the length.
fn source_field(def: &FnDef, e: &Expr) -> Option<(usize, bool)> {
    let param = |e: &Expr| def.params.iter().position(|p| matches!(&e.kind, ExprKind::Var(v) if v.res == VarRes::Local(p.slot)));
    match &e.kind {
        ExprKind::MethodCall { receiver, method, args, .. } if &*method.name == "len" && args.is_empty() => param(receiver).map(|j| (j, true)),
        ExprKind::Call { callee, args } if args.len() == 1 && matches!(&callee.kind, ExprKind::Var(v) if &*v.name == "len") => {
            param(&args[0].value).map(|j| (j, true))
        }
        _ => param(e).map(|j| (j, false)),
    }
}

/// The intersection of two ranges (each end inclusive, `None` for none).
fn meet(a: Option<(Option<i64>, Option<i64>)>, b: Option<(Option<i64>, Option<i64>)>) -> Option<(Option<i64>, Option<i64>)> {
    match (a, b) {
        (Some((alo, ahi)), Some((blo, bhi))) => Some((
            alo.max(blo).or(alo).or(blo),
            match (ahi, bhi) {
                (Some(x), Some(y)) => Some(x.min(y)),
                (x, y) => x.or(y),
            },
        )),
        (a, b) => a.or(b),
    }
}

fn meet_f(a: Option<(Option<f64>, Option<f64>)>, b: Option<(Option<f64>, Option<f64>)>) -> Option<(Option<f64>, Option<f64>)> {
    let pick = |x: Option<f64>, y: Option<f64>, f: fn(f64, f64) -> f64| match (x, y) {
        (Some(x), Some(y)) => Some(f(x, y)),
        (x, y) => x.or(y),
    };
    match (a, b) {
        (Some((alo, ahi)), Some((blo, bhi))) => Some((pick(alo, blo, f64::max), pick(ahi, bhi, f64::min))),
        (a, b) => a.or(b),
    }
}

/// Whether an expression reads the local in `slot`.
fn uses_slot(e: &Expr, slot: u32) -> bool {
    let mut found = matches!(&e.kind, ExprKind::Var(v) if v.res == VarRes::Local(slot));
    crate::ast::for_each_child(e, &mut |c| found |= uses_slot(c, slot));
    found
}

fn bounds(def: &FnDef) -> Vec<Bound> {
    enum Target {
        Value(usize),
        Len(usize),
    }
    enum Lit {
        Int(i64),
        Float(f64),
    }
    fn lit(e: &Expr) -> Option<Lit> {
        match &e.kind {
            ExprKind::Int(n) => Some(Lit::Int(*n)),
            ExprKind::Float(f) => Some(Lit::Float(*f)),
            ExprKind::Unary { op: UnOp::Neg, expr } => match &expr.kind {
                ExprKind::Int(n) => n.checked_neg().map(Lit::Int),
                ExprKind::Float(f) => Some(Lit::Float(-f)),
                _ => None,
            },
            _ => None,
        }
    }
    fn param(def: &FnDef, e: &Expr) -> Option<usize> {
        match &e.kind {
            ExprKind::Var(v) => def.params.iter().position(|p| p.name == v.name && p.pat.is_none()),
            _ => None,
        }
    }
    /// `x`, `x.len()` or `len(x)` for a parameter `x`.
    fn target(def: &FnDef, e: &Expr) -> Option<Target> {
        match &e.kind {
            ExprKind::MethodCall { receiver, method, args, .. } if &*method.name == "len" && args.is_empty() => param(def, receiver).map(Target::Len),
            ExprKind::Call { callee, args }
                if args.len() == 1 && args[0].name.is_none() && matches!(&callee.kind, ExprKind::Var(v) if &*v.name == "len") =>
            {
                param(def, &args[0].value).map(Target::Len)
            }
            _ => param(def, e).map(Target::Value),
        }
    }
    fn flip(op: BinOp) -> BinOp {
        match op {
            BinOp::Lt => BinOp::Gt,
            BinOp::Gt => BinOp::Lt,
            BinOp::Le => BinOp::Ge,
            BinOp::Ge => BinOp::Le,
            other => other,
        }
    }
    fn tighten<T: Copy + PartialOrd>(cur: &mut Option<(Option<T>, Option<T>)>, lo: Option<T>, hi: Option<T>) {
        let cur = cur.get_or_insert((None, None));
        if let Some(l) = lo {
            cur.0 = Some(match cur.0 {
                Some(c) if c > l => c,
                _ => l,
            });
        }
        if let Some(h) = hi {
            cur.1 = Some(match cur.1 {
                Some(c) if c < h => c,
                _ => h,
            });
        }
    }
    fn int_range(op: BinOp, c: i64) -> Option<(Option<i64>, Option<i64>)> {
        Some(match op {
            BinOp::Ge => (Some(c), None),
            BinOp::Gt => (Some(c.checked_add(1)?), None),
            BinOp::Le => (None, Some(c)),
            BinOp::Lt => (None, Some(c.checked_sub(1)?)),
            BinOp::Eq => (Some(c), Some(c)),
            _ => return None,
        })
    }
    fn visit(def: &FnDef, e: &Expr, out: &mut [Bound]) {
        match &e.kind {
            ExprKind::And(a, b) => {
                visit(def, a, out);
                visit(def, b, out);
            }
            ExprKind::Unary { op: UnOp::Not, expr } => {
                if let ExprKind::MethodCall { receiver, method, args, .. } = &expr.kind {
                    if &*method.name == "is_empty" && args.is_empty() {
                        if let Some(i) = param(def, receiver) {
                            tighten(&mut out[i].len, Some(1), None);
                        }
                    }
                }
            }
            ExprKind::Binary { op, lhs, rhs } => {
                if let (BinOp::In, Some(i), ExprKind::Range { start, end: Some(end), inclusive }) = (op, param(def, lhs), &rhs.kind) {
                    if let (Some(Lit::Int(a)), Some(Lit::Int(b))) = (lit(start), lit(end)) {
                        let hi = if *inclusive { Some(b) } else { b.checked_sub(1) };
                        tighten(&mut out[i].int, Some(a), hi);
                    }
                    return;
                }
                let (t, c, op) = match (target(def, lhs), lit(rhs)) {
                    (Some(t), Some(c)) => (t, c, *op),
                    _ => match (lit(lhs), target(def, rhs)) {
                        (Some(c), Some(t)) => (t, c, flip(*op)),
                        _ => return,
                    },
                };
                match (t, c) {
                    (Target::Value(i), Lit::Int(c)) => {
                        if let Some((lo, hi)) = int_range(op, c) {
                            tighten(&mut out[i].int, lo, hi);
                            tighten(&mut out[i].float, lo.map(|x| x as f64), hi.map(|x| x as f64));
                        }
                    }
                    (Target::Value(i), Lit::Float(c)) => match op {
                        BinOp::Ge | BinOp::Gt => tighten(&mut out[i].float, Some(c), None),
                        BinOp::Le | BinOp::Lt => tighten(&mut out[i].float, None, Some(c)),
                        _ => {}
                    },
                    (Target::Len(i), Lit::Int(c)) => {
                        if let (BinOp::Ne, 0) = (op, c) {
                            tighten(&mut out[i].len, Some(1), None);
                        } else if let Some((lo, hi)) = int_range(op, c) {
                            tighten(&mut out[i].len, lo, hi);
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    let mut out = vec![Bound::default(); def.params.len()];
    for r in &def.requires {
        visit(def, r, &mut out);
    }
    out
}

fn shrink_failure(
    it: &mut Interp,
    c: &Rc<Closure>,
    mut cur: Vec<Value>,
    mut diag: Box<Diagnostic>,
    plans: &HashMap<u32, InvPlan>,
) -> (Vec<Value>, Box<Diagnostic>, u32) {
    let code = diag.code;
    let mut steps = 0u32;
    let mut tries = 0u32;
    // Every attempt at shrinking a "took too long" failure runs to the full
    // budget, so only try a few, for a few seconds at most.
    let max_tries = if code == "E0219" { 24 } else { 20_000 };
    let deadline = crate::platform::monotonic_seconds() + if code == "E0219" { 3.0 } else { 30.0 };
    'outer: while steps < 1000 && tries < max_tries {
        for i in 0..cur.len() {
            for cand in shrink(&cur[i]) {
                if tries >= max_tries || crate::platform::monotonic_seconds() > deadline {
                    break 'outer;
                }
                tries += 1;
                let mut trial = cur.clone();
                trial[i] = cand;
                match crate::proptest::repair(it, plans, &trial[i]) {
                    // (Repair may give back the value being shrunk.)
                    Some(v) if !crate::value::values_equal(&v, &cur[i]) => trial[i] = v,
                    _ => continue,
                }
                if let Outcome::Fail(d) = run_case(it, c, trial.clone()) {
                    if d.code == code {
                        cur = trial;
                        diag = d;
                        steps += 1;
                        continue 'outer;
                    }
                }
            }
        }
        break;
    }
    (cur, diag, steps)
}

fn indent(s: &str, n: usize) -> String {
    let pad = " ".repeat(n);
    s.lines().map(|l| if l.is_empty() { String::new() } else { format!("{}{}", pad, l) }).collect::<Vec<_>>().join("\n")
}

fn show_failure(it: &Interp, def: &FnDef, f: &Failure, c: &Colors, out: &mut String) {
    let what = if f.after == 1 { "on the first case".to_string() } else { format!("after {} cases", f.after) };
    let shr = if f.shrinks > 0 { format!(", shrunk {} time{}", f.shrinks, if f.shrinks == 1 { "" } else { "s" }) } else { String::new() };
    if let Some(v) = &f.generated {
        out.push_str(&format!("      checking the invariant of a generated value failed ({}):\n        {}\n", what, repr(v)));
    } else {
        out.push_str(&format!("      counterexample ({}{}):\n", what, shr));
    }
    for (p, a) in def.params.iter().zip(&f.args) {
        out.push_str(&format!("        {}{}{} = {}\n", c.bold, param_label(it, p), c.reset, repr(a)));
    }
    out.push_str(&indent(&f.diag.render(&it.ctx.sm, !c.red.is_empty()), 6));
    out.push('\n');
    // Overflow with an extreme input: say how to rule such inputs out, in
    // case they cannot happen.
    if f.diag.code == "E0207" {
        let huge = def.params.iter().zip(&f.args).find(|(_, a)| matches!(a, Value::Int(n) if n.unsigned_abs() >= 1 << 31));
        let is_huge = |v: &Value| matches!(v, Value::Int(n) if n.unsigned_abs() >= 1 << 31);
        if let Some((p, a)) = huge {
            let name = param_label(it, p);
            let (size, bound) = match a {
                Value::Int(n) if *n < 0 => ("small", format!("{} >= -1_000_000_000", name)),
                _ => ("large", format!("{} <= 1_000_000_000", name)),
            };
            out.push_str(&format!(
                "      {}verify also tries extreme Ints such as min_int and max_int; if inputs this {} cannot happen, say so: `requires {}`{}\n",
                c.dim, size, bound, c.reset
            ));
        } else if let Some((ty, field)) = f.args.iter().find_map(|a| match a {
            Value::Record(r) => {
                r.ty.as_ref().and_then(|t| r.names.iter().zip(r.values.iter()).find(|(_, v)| is_huge(v)).map(|(n, _)| (t.name.clone(), n.clone())))
            }
            _ => None,
        }) {
            out.push_str(&format!(
                "      {}verify also tries extreme Ints such as max_int in record fields; if `{}` cannot be this large, say so in the type: `type {} = {{ ... }} where {} <= 1_000_000_000`{}\n",
                c.dim, field, ty, field, c.reset
            ));
        }
    }
}

/// Run all `test` and `property` declarations of a program. The program's
/// top-level statements must already have been executed.
pub fn run_tests(it: &mut Interp, prog: &Program, file: &str, opts: &Options) -> Summary {
    let c = Colors::new(opts.color);
    let mut sum = Summary::default();
    let mut out = String::new();
    out.push_str(&format!("{}{}{}\n", c.bold, file, c.reset));
    let mut any = false;
    for item in &prog.items {
        match item {
            Item::Test(t) => {
                if let Some(f) = &opts.filter {
                    if !t.name.contains(f.as_str()) {
                        continue;
                    }
                }
                any = true;
                let start = crate::platform::monotonic_seconds();
                let cl = Rc::new(Closure { def: t.func.clone(), captures: vec![] });
                let depth = it.stack.len();
                let saved_budget = it.budget;
                it.budget = Some(opts.budget);
                it.ticks = 0;
                it.catching += 1;
                let r = it.call_closure(&cl, vec![], vec![], Span::default());
                it.catching -= 1;
                it.budget = saved_budget;
                it.stack.truncate(depth);
                let r = check_test_result(it, r);
                let ms = (crate::platform::monotonic_seconds() - start) * 1000.0;
                let timing = if ms > 100.0 { format!(" {}({:.0} ms){}", c.dim, ms, c.reset) } else { String::new() };
                match r {
                    Ok(_) | Err(Ctrl::Return(_)) => {
                        sum.passed += 1;
                        out.push_str(&format!("  {}✓{} {}{}\n", c.green, c.reset, t.name, timing));
                    }
                    Err(Ctrl::Error(d)) => {
                        sum.failed += 1;
                        out.push_str(&format!("  {}✗ {}{}\n", c.red, t.name, c.reset));
                        out.push_str(&indent(&d.render(&it.ctx.sm, opts.color), 6));
                        out.push('\n');
                    }
                    Err(_) => sum.passed += 1,
                }
            }
            Item::Property(p) => {
                if let Some(f) = &opts.filter {
                    if !p.name.contains(f.as_str()) {
                        continue;
                    }
                }
                any = true;
                let seed = opts.seed.unwrap_or_else(|| name_seed(&p.name));
                match quickcheck(it, &p.func, opts.cases, seed, opts.budget, false) {
                    PropOutcome::Passed { cases, discarded, .. } => {
                        sum.passed += 1;
                        sum.cases += cases as u64;
                        let disc = if discarded > 0 { format!(", {} discarded", discarded) } else { String::new() };
                        out.push_str(&format!("  {}✓{} {} {}({} cases{}){}\n", c.green, c.reset, p.name, c.dim, cases, disc, c.reset));
                    }
                    PropOutcome::GaveUp { cases, discarded, .. } => {
                        sum.gave_up += 1;
                        sum.cases += cases as u64;
                        out.push_str(&format!(
                            "  {}?{} {} {}(gave up: only {} of {} generated inputs satisfied the `where` clause; narrow the input types or the clause){}\n",
                            c.yellow,
                            c.reset,
                            p.name,
                            c.dim,
                            cases,
                            cases + discarded,
                            c.reset
                        ));
                    }
                    PropOutcome::Failed(f) => {
                        sum.failed += 1;
                        sum.cases += f.after as u64;
                        out.push_str(&format!("  {}✗ {}{} {}(seed {}){}\n", c.red, p.name, c.reset, c.dim, seed, c.reset));
                        show_failure(it, &p.func, &f, &c, &mut out);
                    }
                    PropOutcome::CannotGenerate(m) => {
                        sum.failed += 1;
                        out.push_str(&format!("  {}✗ {}{}\n      cannot generate inputs: {}\n", c.red, p.name, c.reset, m));
                    }
                }
            }
            _ => {}
        }
        flush_out(it, &mut out);
    }
    if !any {
        out.push_str(&format!("  {}(no tests){}\n", c.dim, c.reset));
    }
    flush_out(it, &mut out);
    sum
}

/// Check every function's contracts against random inputs.
pub fn run_verify(it: &mut Interp, prog: &Program, file: &str, opts: &Options) -> Summary {
    let c = Colors::new(opts.color);
    let mut sum = Summary::default();
    let mut out = String::new();
    out.push_str(&format!("{}{}{}\n", c.bold, file, c.reset));
    let fns: Vec<Rc<FnDef>> = prog
        .items
        .iter()
        .filter_map(|i| match i {
            Item::Fn(d) => Some(d.clone()),
            _ => None,
        })
        .collect();
    // Overloads share a name, so they are labelled with their parameter types.
    let label = |d: &FnDef| -> String {
        let name = d.display_name();
        if fns.iter().filter(|o| o.display_name() == name).count() > 1 {
            let tys: Vec<String> = d.params.iter().map(|p| p.ty.as_ref().map_or("Any".to_string(), |t| t.ty.to_string())).collect();
            format!("{}({})", name, tys.join(", "))
        } else {
            name.to_string()
        }
    };
    let labels: Vec<String> = fns.iter().map(|d| label(d)).collect();
    let width = labels.iter().map(|l| l.chars().count()).max().unwrap_or(0);
    let mut any = false;
    for (def, name) in fns.iter().cloned().zip(labels) {
        if let Some(f) = &opts.filter {
            if !name.contains(f.as_str()) {
                continue;
            }
        }
        if !def.has_contracts() && !opts.all {
            continue;
        }
        any = true;
        if def.params.is_empty() {
            sum.skipped += 1;
            out.push_str(&format!("  {}-{} {:w$}  {}skipped: no inputs to generate{}\n", c.dim, c.reset, name, c.dim, c.reset, w = width));
            flush_out(it, &mut out);
            continue;
        }
        let seed = opts.seed.unwrap_or_else(|| name_seed(&def.display_name()));
        match quickcheck(it, &def, opts.cases, seed, opts.budget, true) {
            PropOutcome::Passed { cases, discarded, missed } => {
                sum.passed += 1;
                sum.cases += cases as u64;
                let what = if def.has_contracts() { "contracts held" } else { "no errors" };
                let rejected = discarded - missed.as_ref().map_or(0, |m| m.0);
                let mut disc = if rejected > 0 { format!(", {} inputs rejected by `requires`", rejected) } else { String::new() };
                if let Some((n, ty)) = &missed {
                    disc.push_str(&format!(", {} attempts found no {}", n, ty));
                }
                out.push_str(&format!("  {}✓{} {:w$}  {}{} cases, {}{}{}\n", c.green, c.reset, name, c.dim, cases, what, disc, c.reset, w = width));
            }
            PropOutcome::GaveUp { cases, discarded, missed } => {
                // Not a failure: the function may need inputs (a well-formed
                // tree, a consistent table) that random values rarely are.
                sum.skipped += 1;
                sum.cases += cases as u64;
                let what = match &missed {
                    Some((n, ty)) if *n * 2 > discarded => format!("contained {} {}", crate::diagnostic::a_an(ty), ty),
                    _ => "satisfied `requires`".to_string(),
                };
                out.push_str(&format!(
                    "  {}?{} {:w$}  {}not checked: only {} of {} random inputs {};\n      test it with `test` blocks, or a `property` that builds valid inputs{}\n",
                    c.yellow,
                    c.reset,
                    name,
                    c.dim,
                    cases,
                    cases + discarded,
                    what,
                    c.reset,
                    w = width
                ));
            }
            PropOutcome::Failed(f) => {
                sum.failed += 1;
                sum.cases += f.after as u64;
                let kind = match f.diag.code {
                    "E0302" => "postcondition violated",
                    "E0303" => "type invariant violated",
                    "E0301" => "a precondition of a called function was violated",
                    "E0200" => "type error",
                    "E0219" => "took too long",
                    _ => "runtime error",
                };
                out.push_str(&format!("  {}✗ {:w$}{}  {}{}{}\n", c.red, name, c.reset, c.bold, kind, c.reset, w = width));
                show_failure(it, &def, &f, &c, &mut out);
            }
            PropOutcome::CannotGenerate(m) => {
                sum.skipped += 1;
                out.push_str(&format!("  {}-{} {:w$}  {}skipped: {}{}\n", c.dim, c.reset, name, c.dim, m, c.reset, w = width));
            }
        }
        flush_out(it, &mut out);
    }
    if !any {
        out.push_str(&format!("  {}(no functions with contracts; use --all to check every annotated function){}\n", c.dim, c.reset));
    }
    flush_out(it, &mut out);
    sum
}

/// Print the report so far. Reports bypass `silent` (which only mutes the
/// program under test) but honour `capture`.
fn flush_out(it: &mut Interp, out: &mut String) {
    use std::io::Write;
    if let Some(buf) = &mut it.capture {
        buf.push_str(out);
    } else {
        it.flush();
        crate::out!("{}", out);
        let _ = std::io::stdout().flush();
    }
    out.clear();
}
