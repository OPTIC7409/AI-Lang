//! Function bodies compiled into closures.
//!
//! The interpreter walks the syntax tree: every expression goes through
//! `Interp::eval`, which matches on its kind and pays for a large stack
//! frame. On a function's first call its body is compiled once into a tree
//! of closures, one per expression, each already specialized for its kind
//! (`n < 2` on a local Int becomes a closure that compares two Ints). Only
//! the kinds that matter most in loops and calls are compiled; any other
//! expression becomes a closure that calls `eval` on it, so the two always
//! agree on what a program means. `COGITO_NO_COMPILE=1` turns compiling off
//! (to compare the two).

use crate::ast::*;
use crate::interp::{int_binop, norm_index, simple_index, Ctrl, Env, Interp, R};
use crate::span::Span;
use crate::value::Value;
use std::cell::OnceCell;
use std::rc::Rc;

/// A compiled expression.
pub type Code = Box<dyn Fn(&mut Interp, &mut Env) -> R>;
type CondCode = Box<dyn Fn(&mut Interp, &mut Env) -> R<bool>>;
/// A compiled statement.
pub type StmtCode = Box<dyn Fn(&mut Interp, &mut Env) -> R<()>>;

/// A function body's compiled code, made on first use.
#[derive(Default)]
pub struct Cache(OnceCell<Code>);

impl Cache {
    /// The code for `body`, the body of the function that holds this cache.
    pub fn get(&self, body: &Expr) -> Option<&Code> {
        Some(self.0.get_or_init(|| expr(body)))
    }
}

impl std::fmt::Debug for Cache {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(if self.0.get().is_some() { "Cache(compiled)" } else { "Cache" })
    }
}

/// Whether bodies are compiled (unless `COGITO_NO_COMPILE` is set).
pub fn enabled() -> bool {
    thread_local! {
        static ON: bool = std::env::var_os("COGITO_NO_COMPILE").is_none();
    }
    ON.with(|on| *on)
}

/// A pointer to a node of the syntax tree, for the closures that hand it
/// back to the interpreter.
///
/// SAFETY: compiled code is kept in the `FnDef` whose body holds the node,
/// and a body is never changed after the resolver is done with it (it is
/// shared through `Rc`), so the node outlives every closure that points to
/// it.
#[derive(Clone, Copy)]
struct Node<T>(*const T);

impl<T> Node<T> {
    fn new(x: &T) -> Node<T> {
        Node(x as *const T)
    }
    fn get(&self) -> &T {
        unsafe { &*self.0 }
    }
}

/// Evaluate `e` with the interpreter itself.
fn fallback(e: &Expr) -> Code {
    let e = Node::new(e);
    Box::new(move |it, env| it.eval(e.get(), env))
}

pub fn expr(e: &Expr) -> Code {
    let span = e.span;
    match &e.kind {
        ExprKind::Unit => Box::new(|_, _| Ok(Value::Unit)),
        ExprKind::Bool(b) => {
            let b = *b;
            Box::new(move |_, _| Ok(Value::Bool(b)))
        }
        ExprKind::Int(n) => {
            let n = *n;
            Box::new(move |_, _| Ok(Value::Int(n)))
        }
        ExprKind::Float(f) => {
            let f = *f;
            Box::new(move |_, _| Ok(Value::Float(f)))
        }
        ExprKind::Str(t) => {
            let t = t.clone();
            Box::new(move |_, _| Ok(Value::Str(t.clone())))
        }
        ExprKind::Var(Var { res: VarRes::Local(s), .. }) => {
            let s = *s as usize;
            Box::new(move |_, env| Ok(env.locals[s].clone()))
        }
        ExprKind::Var(v) => {
            let v = Node::new(v);
            Box::new(move |it, env| it.load(v.get(), span, env))
        }
        ExprKind::Field { target, name, name_span } => {
            let (target, name, name_span) = (expr(target), name.clone(), *name_span);
            Box::new(move |it, env| {
                let v = target(it, env)?;
                it.get_field(&v, &name, name_span)
            })
        }
        ExprKind::Binary { op, lhs, rhs } => binary(*op, lhs, rhs, span),
        ExprKind::Unary { op, expr: x } => {
            let (op, at) = (*op, x.span);
            let x = expr(x);
            Box::new(move |it, env| {
                let v = x(it, env)?;
                it.unary_value(op, v, span, at)
            })
        }
        ExprKind::And(a, b) => {
            let (a, b) = (cond(a, "the left side of `and`"), cond(b, "the right side of `and`"));
            Box::new(move |it, env| Ok(Value::Bool(a(it, env)? && b(it, env)?)))
        }
        ExprKind::Or(a, b) => {
            let (a, b) = (cond(a, "the left side of `or`"), cond(b, "the right side of `or`"));
            Box::new(move |it, env| Ok(Value::Bool(a(it, env)? || b(it, env)?)))
        }
        ExprKind::If { cond: c, then, els } => {
            let c = cond(c, "the `if` condition");
            let then = expr(then);
            match els {
                Some(x) => {
                    let els = expr(x);
                    Box::new(move |it, env| if c(it, env)? { then(it, env) } else { els(it, env) })
                }
                None => Box::new(move |it, env| if c(it, env)? { then(it, env) } else { Ok(Value::Unit) }),
            }
        }
        ExprKind::Block(stmts) => block(stmts),
        ExprKind::While { cond: c, body } => {
            let c = cond(c, "the `while` condition");
            let body = expr(body);
            Box::new(move |it, env| {
                while c(it, env)? {
                    it.tick(span)?;
                    match body(it, env) {
                        Ok(_) | Err(Ctrl::Continue) => {}
                        Err(Ctrl::Break(_)) => break,
                        Err(other) => return Err(other),
                    }
                }
                Ok(Value::Unit)
            })
        }
        ExprKind::For { pat, iter, body } => for_loop(pat, iter, body),
        ExprKind::Match { scrutinee, arms } => match_arms(scrutinee, arms),
        ExprKind::Index { target, index } => match &target.kind {
            ExprKind::Var(Var { res: VarRes::Local(s), .. }) => index_local(*s as usize, index, span),
            ExprKind::Index { target: t, index: outer } if simple_index(outer) && simple_index(index) => match t.kind {
                ExprKind::Var(Var { res: VarRes::Local(s), .. }) => index2_local(s as usize, outer, index, e),
                _ => fallback(e),
            },
            _ => fallback(e),
        },
        // `xs.push!(x)` and the like on a local.
        ExprKind::MethodCall { receiver, method: Var { res: VarRes::Global(g), .. }, args, mutating: true, root_ty, .. }
            if args.iter().all(|a| a.name.is_none()) =>
        {
            let ExprKind::Var(Var { res: VarRes::Local(slot), .. }) = receiver.kind else { return fallback(e) };
            let (g, slot, decl, receiver_span, node) = (*g, slot as usize, root_ty.clone(), receiver.span, Node::new(e));
            let codes: Vec<Code> = args.iter().map(|a| expr(&a.value)).collect();
            Box::new(move |it, env| {
                let arg = |it: &mut Interp, k: usize, env: &mut Env| codes[k](it, env);
                match it.mutating_local_with(slot, g, codes.len(), decl.as_ref(), receiver_span, span, env, arg) {
                    Some(r) => r,
                    None => it.eval(node.get(), env),
                }
            })
        }
        ExprKind::Call { callee, args } => call(e, callee, args),
        ExprKind::Return(v) => {
            let v = v.as_deref().map(expr);
            Box::new(move |it, env| {
                let v = match &v {
                    Some(x) => x(it, env)?,
                    None => Value::Unit,
                };
                it.try_span = None;
                Err(Ctrl::Return(v))
            })
        }
        _ => fallback(e),
    }
}

/// `a op b`, as `eval` computes it: Ints directly, anything else by `binop`.
fn binary(op: BinOp, lhs: &Expr, rhs: &Expr, span: Span) -> Code {
    match (&lhs.kind, &rhs.kind) {
        // `n - 1`, `i < n`: no closure for the operands.
        (ExprKind::Var(Var { res: VarRes::Local(a), .. }), ExprKind::Int(n)) => {
            let (a, n) = (*a as usize, *n);
            Box::new(move |it, env| {
                if let Value::Int(x) = env.locals[a] {
                    if let Some(v) = int_binop(op, x, n) {
                        return Ok(v);
                    }
                }
                let x = env.locals[a].clone();
                it.binop(op, x, Value::Int(n), span)
            })
        }
        (ExprKind::Var(Var { res: VarRes::Local(a), .. }), ExprKind::Var(Var { res: VarRes::Local(b), .. })) => {
            let (a, b) = (*a as usize, *b as usize);
            Box::new(move |it, env| {
                if let (Value::Int(x), Value::Int(y)) = (&env.locals[a], &env.locals[b]) {
                    if let Some(v) = int_binop(op, *x, *y) {
                        return Ok(v);
                    }
                }
                let (x, y) = (env.locals[a].clone(), env.locals[b].clone());
                it.binop(op, x, y, span)
            })
        }
        // `(i * 2) + 1`: no closure for the literal.
        (_, ExprKind::Int(n)) => {
            let (l, n) = (expr(lhs), *n);
            Box::new(move |it, env| match l(it, env)? {
                Value::Int(x) => match int_binop(op, x, n) {
                    Some(v) => Ok(v),
                    None => it.binop(op, Value::Int(x), Value::Int(n), span),
                },
                a => it.binop(op, a, Value::Int(n), span),
            })
        }
        _ => {
            let (l, r) = (expr(lhs), expr(rhs));
            Box::new(move |it, env| {
                let a = l(it, env)?;
                let b = r(it, env)?;
                if let (Value::Int(x), Value::Int(y)) = (&a, &b) {
                    if let Some(v) = int_binop(op, *x, *y) {
                        return Ok(v);
                    }
                }
                it.binop(op, a, b, span)
            })
        }
    }
}

/// `xs[i]` on a local, as `eval_index` computes it.
fn index_local(slot: usize, index: &Expr, span: Span) -> Code {
    let ix = expr(index);
    if simple_index(index) {
        Box::new(move |it, env| {
            let i = ix(it, env)?;
            if let (Value::List(xs), Value::Int(i)) = (&env.locals[slot], &i) {
                if let Some(j) = norm_index(*i, xs.len()) {
                    return Ok(xs[j].clone());
                }
            }
            let v = env.locals[slot].clone();
            it.index_value(v, i, span)
        })
    } else {
        // (The index might change the local: the list is the one before.)
        let index = Node::new(index);
        Box::new(move |it, env| {
            let v = env.locals[slot].clone();
            let i = ix(it, env)?;
            it.index_general(v, i, index.get(), span)
        })
    }
}

/// `match`, as `eval` runs it: the first arm whose pattern matches (and
/// whose guard holds; with a guard, each alternative of an or-pattern is
/// tried in turn).
fn match_arms(scrutinee: &Expr, arms: &[Arm]) -> Code {
    struct Compiled {
        alts: Vec<Node<Pattern>>,
        guard: Option<CondCode>,
        body: Code,
    }
    let span = scrutinee.span;
    let scrutinee = expr(scrutinee);
    let arms: Vec<Compiled> = arms
        .iter()
        .map(|arm| Compiled {
            alts: match (&arm.pat.kind, &arm.guard) {
                (PatKind::Or(alts), Some(_)) => alts.iter().map(Node::new).collect(),
                _ => vec![Node::new(&arm.pat)],
            },
            guard: arm.guard.as_ref().map(|g| cond(g, "the match guard")),
            body: expr(&arm.body),
        })
        .collect();
    Box::new(move |it, env| {
        let v = scrutinee(it, env)?;
        for arm in &arms {
            for alt in &arm.alts {
                if it.match_pattern(alt.get(), &v, env) {
                    match &arm.guard {
                        Some(g) if !g(it, env)? => {}
                        _ => return (arm.body)(it, env),
                    }
                }
            }
        }
        Err(it.no_arm(span, &v))
    })
}

/// `grid[i][j]` on a local list of lists, with indexes that have no
/// effects (so that `eval` can compute them again when the fast path does
/// not apply).
fn index2_local(slot: usize, outer: &Expr, inner: &Expr, e: &Expr) -> Code {
    let (a, b, node) = (expr(outer), expr(inner), Node::new(e));
    Box::new(move |it, env| {
        if let (Value::Int(i), Value::Int(j)) = (a(it, env)?, b(it, env)?) {
            if let Value::List(rows) = &env.locals[slot] {
                if let Some(Value::List(row)) = norm_index(i, rows.len()).map(|k| &rows[k]) {
                    if let Some(k) = norm_index(j, row.len()) {
                        return Ok(row[k].clone());
                    }
                }
            }
        }
        it.eval(node.get(), env)
    })
}

/// A condition (`what` names it in the error for a value that is not a
/// Bool), as `eval_cond` computes it.
fn cond(e: &Expr, what: &'static str) -> CondCode {
    let span = e.span;
    if let ExprKind::Binary { op: op @ (BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge | BinOp::Eq | BinOp::Ne), lhs, rhs } = &e.kind {
        let op = *op;
        let compare = move |x: i64, y: i64| match op {
            BinOp::Lt => x < y,
            BinOp::Le => x <= y,
            BinOp::Gt => x > y,
            BinOp::Ge => x >= y,
            BinOp::Eq => x == y,
            _ => x != y,
        };
        let general = move |it: &mut Interp, a: Value, b: Value| match it.binop(op, a, b, span)? {
            Value::Bool(b) => Ok(b),
            other => Err(it.fail(it.not_bool(span, what, &other))),
        };
        return match (&lhs.kind, &rhs.kind) {
            (ExprKind::Var(Var { res: VarRes::Local(a), .. }), ExprKind::Int(n)) => {
                let (a, n) = (*a as usize, *n);
                Box::new(move |it, env| match env.locals[a] {
                    Value::Int(x) => Ok(compare(x, n)),
                    _ => general(it, env.locals[a].clone(), Value::Int(n)),
                })
            }
            (ExprKind::Var(Var { res: VarRes::Local(a), .. }), ExprKind::Var(Var { res: VarRes::Local(b), .. })) => {
                let (a, b) = (*a as usize, *b as usize);
                Box::new(move |it, env| match (&env.locals[a], &env.locals[b]) {
                    (Value::Int(x), Value::Int(y)) => Ok(compare(*x, *y)),
                    _ => general(it, env.locals[a].clone(), env.locals[b].clone()),
                })
            }
            // `i % 3 == 0`: no closure for the literal.
            (_, ExprKind::Int(n)) => {
                let (l, n) = (expr(lhs), *n);
                Box::new(move |it, env| match l(it, env)? {
                    Value::Int(x) => Ok(compare(x, n)),
                    a => general(it, a, Value::Int(n)),
                })
            }
            _ => {
                let (l, r) = (expr(lhs), expr(rhs));
                Box::new(move |it, env| {
                    let a = l(it, env)?;
                    let b = r(it, env)?;
                    match (&a, &b) {
                        (Value::Int(x), Value::Int(y)) => Ok(compare(*x, *y)),
                        _ => general(it, a, b),
                    }
                })
            }
        };
    }
    let x = expr(e);
    Box::new(move |it, env| match x(it, env)? {
        Value::Bool(b) => Ok(b),
        other => Err(it.fail(it.not_bool(span, what, &other))),
    })
}

/// A block: its statements, then the value of the last one if it is an
/// expression (as `exec_block` does).
fn block(stmts: &[Stmt]) -> Code {
    let (init, last) = match stmts.split_last() {
        Some((Stmt { kind: StmtKind::Expr(e), .. }, init)) => (init, Some(expr(e))),
        _ => (stmts, None),
    };
    let codes: Vec<StmtCode> = init.iter().map(stmt).collect();
    match (codes.len(), last) {
        (0, Some(last)) => last,
        (_, last) => Box::new(move |it, env| {
            for c in &codes {
                c(it, env)?;
            }
            match &last {
                Some(l) => l(it, env),
                None => Ok(Value::Unit),
            }
        }),
    }
}

/// A statement. (The caller keeps `s` alive as long as the code: see
/// `Node`.)
pub fn stmt(s: &Stmt) -> StmtCode {
    match &s.kind {
        StmtKind::Expr(e) => {
            let x = expr(e);
            Box::new(move |it, env| x(it, env).map(|_| ()))
        }
        // `let x = v` into a local, without a type to check.
        StmtKind::Let { pat: Pattern { kind: PatKind::Bind { res: VarRes::Local(slot), sub: None, .. }, .. }, ty: None, value, .. } => {
            let (slot, x) = (*slot as usize, expr(value));
            Box::new(move |it, env| {
                let v = x(it, env)?;
                env.locals[slot] = v;
                Ok(())
            })
        }
        // `x = v` or `x += v` on a variable without a declared type. (Not a
        // list literal or method call, which `assign` may turn into an
        // append in place.)
        StmtKind::Assign { target, op, value, ty: None }
            if matches!(target.kind, ExprKind::Var(Var { res: VarRes::Local(_) | VarRes::Global(_), .. }))
                && !matches!(value.kind, ExprKind::List(_) | ExprKind::MethodCall { .. }) =>
        {
            let (op, x, target) = (*op, expr(value), Node::new(target));
            Box::new(move |it, env| {
                let rhs = x(it, env)?;
                it.assign_value(target.get(), op, rhs, None, env)
            })
        }
        // `xs[i] = v` or `xs[i] += v` on a local without a declared type:
        // the value, then the index, as `assign` computes them.
        StmtKind::Assign { target: Expr { kind: ExprKind::Index { target: t, index }, span, .. }, op, value, ty: None }
            if matches!(t.kind, ExprKind::Var(Var { res: VarRes::Local(_), .. })) =>
        {
            let ExprKind::Var(Var { res: VarRes::Local(slot), .. }) = t.kind else { unreachable!() };
            let (op, span, x, ix) = (*op, *span, expr(value), expr(index));
            Box::new(move |it, env| {
                let rhs = x(it, env)?;
                let idx = ix(it, env)?;
                if let (None, Value::List(xs), Value::Int(i)) = (op, &mut env.locals[slot as usize], &idx) {
                    if let Some(j) = norm_index(*i, xs.len()) {
                        Rc::make_mut(xs)[j] = rhs;
                        return Ok(());
                    }
                }
                it.assign_index_local(slot, idx, op, rhs, span, env)
            })
        }
        _ => {
            let s = Node::new(s);
            Box::new(move |it, env| it.exec_stmt(s.get(), env))
        }
    }
}

/// `for x in xs { ... }`, as `exec_for` runs it: over a range or a list
/// with the body compiled, over anything else by `exec_for` itself.
fn for_loop(pat: &Pattern, iter: &Expr, body: &Expr) -> Code {
    let span = iter.span;
    let (pat_node, body_node) = (Node::new(pat), Node::new(body));
    let simple = match &pat.kind {
        PatKind::Bind { res: VarRes::Local(s), sub: None, .. } => Some(*s as usize),
        _ => None,
    };
    let (iter, code) = (expr(iter), expr(body));
    Box::new(move |it, env| {
        let bind = |it: &mut Interp, v: Value, env: &mut Env| match simple {
            Some(s) => {
                env.locals[s] = v;
                Ok(())
            }
            None => it.bind_loop(pat_node.get(), v, env),
        };
        macro_rules! run_body {
            () => {
                it.tick(span)?;
                match code(it, env) {
                    Ok(_) | Err(Ctrl::Continue) => {}
                    Err(Ctrl::Break(_)) => break,
                    Err(other) => return Err(other),
                }
            };
        }
        match iter(it, env)? {
            Value::Range(r) => {
                let mut i = r.start;
                loop {
                    if let Some(end) = r.end {
                        if i as i128 >= end {
                            break;
                        }
                    }
                    bind(it, Value::Int(i), env)?;
                    run_body!();
                    match i.checked_add(1) {
                        Some(n) => i = n,
                        None => break,
                    }
                }
                Ok(Value::Unit)
            }
            Value::List(xs) => {
                for x in xs.iter() {
                    bind(it, x.clone(), env)?;
                    run_body!();
                }
                Ok(Value::Unit)
            }
            other => it.exec_for(pat_node.get(), other, body_node.get(), env, span),
        }
    })
}

/// `f(a, b)` of a top-level function: the arguments compiled, and the
/// function's own body run compiled by `run_body`.
fn call(e: &Expr, callee: &Expr, args: &[Arg]) -> Code {
    let ExprKind::Var(v @ Var { res: VarRes::Global(_), .. }) = &callee.kind else { return fallback(e) };
    if args.iter().any(|a| a.name.is_some()) {
        return fallback(e);
    }
    let (span, callee_span) = (e.span, callee.span);
    let (var, node) = (Node::new(v), Node::new(e));
    let codes: Vec<Code> = args.iter().map(|a| expr(&a.value)).collect();
    Box::new(move |it, env| {
        let f = it.load(var.get(), callee_span, env)?;
        if let Value::Func(c) = &f {
            let d = &c.def;
            if codes.len() == d.params.len()
                && !d.mutating
                && d.requires.is_empty()
                && d.ensures.is_empty()
                && d.params.iter().all(|p| p.pat.is_none())
            {
                return it.call_simple_with(c, env, span, |it, i, env| codes[i](it, env));
            }
        }
        match &node.get().kind {
            ExprKind::Call { args, .. } => it.call_general(&f, args, env, span),
            _ => unreachable!(),
        }
    })
}
