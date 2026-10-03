//! `cogito test` (unit tests and property tests) and `cogito verify`
//! (contract checking by random testing).

use crate::ast::{BinOp, Expr, ExprKind, FnDef, Item, Program, UnOp};
use crate::diagnostic::{Colors, Diagnostic};
use crate::interp::{Ctrl, Env, Interp, Rng};
use crate::proptest::{shrink, Gen};
use crate::span::Span;
use crate::types::Ty;
use crate::value::{repr, Closure, Value};
use std::rc::Rc;
use std::time::Instant;

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
            let r = it.call_closure(c, args, vec![], Span::default());
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
    args: Vec<Value>,
    diag: Box<Diagnostic>,
    shrinks: u32,
    after: u32,
}

enum PropOutcome {
    Passed { cases: u32, discarded: u32 },
    GaveUp { cases: u32, discarded: u32 },
    Failed(Failure),
    CannotGenerate(String),
}

fn param_types(def: &FnDef) -> Result<Vec<Ty>, String> {
    let mut tys = Vec::new();
    for p in &def.params {
        match &p.ty {
            Some(t) => tys.push(t.ty.clone()),
            None => return Err(format!("parameter `{}` has no type annotation", p.name)),
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
    let tys = match param_types(def) {
        Ok(t) => t,
        Err(m) => return PropOutcome::CannotGenerate(m),
    };
    let bounds = int_bounds(def);
    let c = Rc::new(Closure { def: def.clone(), captures: vec![] });
    let mut rng = Rng::new(seed);
    let mut passed = 0u32;
    let mut discarded = 0u32;
    let was_silent = it.silent;
    it.silent = true;
    let result = loop {
        if passed >= cases {
            break PropOutcome::Passed { cases: passed, discarded };
        }
        if discarded > cases.max(10) * 20 {
            break PropOutcome::GaveUp { cases: passed, discarded };
        }
        let size = 2 + passed * 40 / cases.max(1);
        let mut args = Vec::with_capacity(tys.len());
        let mut gen = Gen { it, rng: &mut rng, extremes };
        let mut gen_err = None;
        for (i, t) in tys.iter().enumerate() {
            if let (Ty::Int, Some((lo, hi))) = (t, bounds.get(i).copied().flatten()) {
                args.push(gen.int_in(lo, hi, size));
                continue;
            }
            match gen.value(t, size, 0) {
                Ok(v) => args.push(v),
                Err(m) => {
                    gen_err = Some(m);
                    break;
                }
            }
        }
        if let Some(m) = gen_err {
            break PropOutcome::CannotGenerate(m);
        }
        match run_case(it, &c, args.clone()) {
            Outcome::Pass => passed += 1,
            Outcome::Discard => discarded += 1,
            Outcome::Fail(d) => {
                let (args, diag, shrinks) = shrink_failure(it, &c, args, d);
                break PropOutcome::Failed(Failure { args, diag, shrinks, after: passed + 1 });
            }
        }
    };
    it.silent = was_silent;
    result
}

/// Inclusive bounds on Int parameters that can be read off the `requires` /
/// `where` clauses: comparisons with literals, joined by `and`, and `in` ranges.
fn int_bounds(def: &FnDef) -> Vec<Option<(Option<i64>, Option<i64>)>> {
    let mut out: Vec<Option<(Option<i64>, Option<i64>)>> = vec![None; def.params.len()];
    fn lit(e: &Expr) -> Option<i64> {
        match &e.kind {
            ExprKind::Int(n) => Some(*n),
            ExprKind::Unary { op: UnOp::Neg, expr } => match &expr.kind {
                ExprKind::Int(n) => n.checked_neg(),
                _ => None,
            },
            _ => None,
        }
    }
    fn param_index(def: &FnDef, e: &Expr) -> Option<usize> {
        match &e.kind {
            ExprKind::Var(v) => def.params.iter().position(|p| p.name == v.name && p.pat.is_none()),
            _ => None,
        }
    }
    fn visit(def: &FnDef, e: &Expr, out: &mut Vec<Option<(Option<i64>, Option<i64>)>>) {
        let mut set = |i: usize, lo: Option<i64>, hi: Option<i64>| {
            let cur = out[i].get_or_insert((None, None));
            if let Some(l) = lo {
                cur.0 = Some(cur.0.map_or(l, |c: i64| c.max(l)));
            }
            if let Some(h) = hi {
                cur.1 = Some(cur.1.map_or(h, |c: i64| c.min(h)));
            }
        };
        match &e.kind {
            ExprKind::And(a, b) => {
                visit(def, a, out);
                visit(def, b, out);
            }
            ExprKind::Binary { op, lhs, rhs } => {
                if let (Some(i), Some(c)) = (param_index(def, lhs), lit(rhs)) {
                    match op {
                        BinOp::Ge => set(i, Some(c), None),
                        BinOp::Gt => set(i, c.checked_add(1), None),
                        BinOp::Le => set(i, None, Some(c)),
                        BinOp::Lt => set(i, None, c.checked_sub(1)),
                        BinOp::Eq => set(i, Some(c), Some(c)),
                        _ => {}
                    }
                } else if let (Some(c), Some(i)) = (lit(lhs), param_index(def, rhs)) {
                    match op {
                        BinOp::Le => set(i, Some(c), None),
                        BinOp::Lt => set(i, c.checked_add(1), None),
                        BinOp::Ge => set(i, None, Some(c)),
                        BinOp::Gt => set(i, None, c.checked_sub(1)),
                        BinOp::Eq => set(i, Some(c), Some(c)),
                        _ => {}
                    }
                } else if let (BinOp::In, Some(i), ExprKind::Range { start, end: Some(end), inclusive }) = (op, param_index(def, lhs), &rhs.kind) {
                    if let (Some(a), Some(b)) = (lit(start), lit(end)) {
                        set(i, Some(a), if *inclusive { Some(b) } else { b.checked_sub(1) });
                    }
                }
            }
            _ => {}
        }
    }
    for r in &def.requires {
        visit(def, r, &mut out);
    }
    out
}

fn shrink_failure(it: &mut Interp, c: &Rc<Closure>, mut cur: Vec<Value>, mut diag: Box<Diagnostic>) -> (Vec<Value>, Box<Diagnostic>, u32) {
    let code = diag.code;
    let mut steps = 0u32;
    let mut tries = 0u32;
    // Every attempt at shrinking a "took too long" failure runs to the full
    // budget, so only try a few.
    let max_tries = if code == "E0219" { 24 } else { 20_000 };
    'outer: while steps < 1000 && tries < max_tries {
        for i in 0..cur.len() {
            for cand in shrink(&cur[i]) {
                if tries >= max_tries {
                    break 'outer;
                }
                tries += 1;
                let mut trial = cur.clone();
                trial[i] = cand;
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
    out.push_str(&format!("      counterexample ({}{}):\n", what, shr));
    for (p, a) in def.params.iter().zip(&f.args) {
        out.push_str(&format!("        {}{}{} = {}\n", c.bold, p.name, c.reset, repr(a)));
    }
    out.push_str(&indent(&f.diag.render(&it.ctx.sm, !c.red.is_empty()), 6));
    out.push('\n');
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
                let start = Instant::now();
                let cl = Rc::new(Closure { def: t.func.clone(), captures: vec![] });
                let depth = it.stack.len();
                let saved_budget = it.budget;
                it.budget = Some(opts.budget);
                it.ticks = 0;
                let r = it.call_closure(&cl, vec![], vec![], Span::default());
                it.budget = saved_budget;
                it.stack.truncate(depth);
                let r = check_test_result(it, r);
                let ms = start.elapsed().as_secs_f64() * 1000.0;
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
                    PropOutcome::Passed { cases, discarded } => {
                        sum.passed += 1;
                        sum.cases += cases as u64;
                        let disc = if discarded > 0 { format!(", {} discarded", discarded) } else { String::new() };
                        out.push_str(&format!("  {}✓{} {} {}({} cases{}){}\n", c.green, c.reset, p.name, c.dim, cases, disc, c.reset));
                    }
                    PropOutcome::GaveUp { cases, discarded } => {
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
    it.flush();
    crate::out!("{}", out);
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
    let width = fns.iter().map(|d| d.display_name().len()).max().unwrap_or(0);
    let mut any = false;
    for def in fns {
        let name = def.display_name();
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
        let seed = opts.seed.unwrap_or_else(|| name_seed(&name));
        match quickcheck(it, &def, opts.cases, seed, opts.budget, true) {
            PropOutcome::Passed { cases, discarded } => {
                sum.passed += 1;
                sum.cases += cases as u64;
                let what = if def.has_contracts() { "contracts held" } else { "no errors" };
                let disc = if discarded > 0 { format!(", {} inputs rejected by `requires`", discarded) } else { String::new() };
                out.push_str(&format!("  {}✓{} {:w$}  {}{} cases, {}{}{}\n", c.green, c.reset, name, c.dim, cases, what, disc, c.reset, w = width));
            }
            PropOutcome::GaveUp { cases, discarded } => {
                sum.gave_up += 1;
                sum.cases += cases as u64;
                out.push_str(&format!(
                    "  {}?{} {:w$}  {}gave up: only {} of {} random inputs satisfied `requires`{}\n",
                    c.yellow,
                    c.reset,
                    name,
                    c.dim,
                    cases,
                    cases + discarded,
                    c.reset,
                    w = width
                ));
            }
            PropOutcome::Failed(f) => {
                sum.failed += 1;
                sum.cases += f.after as u64;
                let kind = match f.diag.code {
                    "E0302" => "postcondition violated",
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
    it.flush();
    crate::out!("{}", out);
    sum
}

fn flush_out(it: &mut Interp, out: &mut String) {
    use std::io::Write;
    it.flush();
    crate::out!("{}", out);
    let _ = std::io::stdout().flush();
    out.clear();
}
