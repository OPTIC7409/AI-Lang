//! A gradual static type checker.
//!
//! Annotations are always checked when the program runs. This pass checks
//! them earlier where it can: it infers a type for each expression from
//! literals, annotations and the signatures of functions and built-ins, and
//! reports an error only when a value's type is known *and* cannot possibly
//! satisfy what is expected. Anything it cannot work out is `Any`, which is
//! never an error, so a correct program is never rejected.
//!
//! It finds, before the program runs:
//! * arguments that do not fit a function's or constructor's parameter types,
//! * returned values that do not fit the declared return type,
//! * `let`/`var` values and assignments that do not fit their annotation,
//! * conditions that are not Bools,
//! * operators applied to the wrong kinds of values (`"a" + 1`),
//! * fields that a record type does not have.

use crate::ast::*;
use crate::ctx::{Ctx, GlobalKind};
use crate::diagnostic::Diagnostic;
use crate::span::Span;
use crate::types::{Name, Ty, TypeDef, TypeKind, OPTION_ID, RESULT_ID};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

/// Check a resolved program; returns the errors found.
pub fn check_program(ctx: &Ctx, prog: &Program) -> Vec<Diagnostic> {
    let mut reassigned = HashSet::new();
    // Locals of the top-level code (inside a top-level `for` or `if`).
    let mut top_locals = HashSet::new();
    for item in &prog.items {
        match item {
            Item::Stmt(s) => {
                scan_stmt(s, &mut reassigned);
                scan_stmt(s, &mut top_locals);
            }
            Item::Fn(def) => scan(&def.body, &mut reassigned),
            Item::Test(t) => scan(&t.func.body, &mut reassigned),
            Item::Property(p) => scan(&p.func.body, &mut reassigned),
            _ => {}
        }
    }
    let reassigned_globals = reassigned.into_iter().filter_map(|r| if let VarRes::Global(s) = r { Some(s) } else { None }).collect();
    let top = Frame {
        reassigned: top_locals.into_iter().filter_map(|r| if let VarRes::Local(s) = r { Some(s) } else { None }).collect(),
        ..Frame::default()
    };
    let mut c = Checker { ctx, diags: Vec::new(), globals: HashMap::new(), frames: vec![top], reassigned_globals, divisions: HashSet::new() };
    // Top-level statements first (in order), so that functions see the
    // types of the globals they use.
    for item in &prog.items {
        if let Item::Stmt(s) = item {
            c.stmt(s);
        }
    }
    for item in &prog.items {
        match item {
            Item::Fn(def) => c.function(def, &[]),
            Item::Test(t) => c.function(&t.func, &[]),
            Item::Property(p) => c.function(&p.func, &[]),
            Item::Type(td) => c.invariant(td.id),
            _ => {}
        }
    }
    c.diags
}

#[derive(Default)]
struct Frame {
    locals: HashMap<u32, Ty>,
    /// Local slots assigned as a whole somewhere in the function (`x = ...`).
    reassigned: HashSet<u32>,
    captures: Vec<Ty>,
    /// The declared return type of the function being checked.
    ret: Option<Ty>,
    name: Option<Name>,
}

struct Checker<'a> {
    ctx: &'a Ctx,
    diags: Vec<Diagnostic>,
    /// Types of top-level variables (from annotations, or inferred for `let`).
    globals: HashMap<u32, Ty>,
    frames: Vec<Frame>,
    /// Globals assigned as a whole somewhere in the program.
    reassigned_globals: HashSet<u32>,
    /// Where `/` was used (its result is a Float even for Ints).
    divisions: HashSet<Span>,
}

/// Record the variables that are assigned as a whole (`x = ...`), or passed
/// to a mutating function, anywhere inside `e`.
fn scan(e: &Expr, out: &mut HashSet<VarRes>) {
    match &e.kind {
        ExprKind::Block(stmts) => {
            for s in stmts {
                scan_target(s, out);
                // (`for_each_child` does not enter local functions.)
                if let StmtKind::Fn { def, .. } = &s.kind {
                    scan(&def.body, out);
                }
            }
        }
        ExprKind::MethodCall { receiver, mutating: true, .. } => {
            if let ExprKind::Var(v) = &receiver.kind {
                out.insert(v.res);
            }
        }
        _ => {}
    }
    for_each_child(e, &mut |c| scan(c, out));
}

/// Whether a statement never finishes normally.
fn diverges(s: &Stmt) -> bool {
    match &s.kind {
        StmtKind::Assert { cond: Expr { kind: ExprKind::Bool(false), .. }, .. } => true,
        StmtKind::Expr(x) => match &x.kind {
            ExprKind::Return(_) | ExprKind::Break(_) | ExprKind::Continue => true,
            ExprKind::Call { callee, .. } => matches!(&callee.kind, ExprKind::Var(v) if matches!(&*v.name, "panic" | "todo" | "exit")),
            _ => false,
        },
        _ => false,
    }
}

fn scan_target(s: &Stmt, out: &mut HashSet<VarRes>) {
    if let StmtKind::Assign { target: Expr { kind: ExprKind::Var(v), .. }, .. } = &s.kind {
        out.insert(v.res);
    }
}

fn scan_stmt(s: &Stmt, out: &mut HashSet<VarRes>) {
    scan_target(s, out);
    match &s.kind {
        StmtKind::Let { value, .. } => scan(value, out),
        StmtKind::Assign { target, value, .. } => {
            scan(target, out);
            scan(value, out);
        }
        StmtKind::Assert { cond, msg } => {
            scan(cond, out);
            if let Some(m) = msg {
                scan(m, out);
            }
        }
        StmtKind::Expr(e) => scan(e, out),
        StmtKind::Fn { def, .. } => scan(&def.body, out),
    }
}

/// Whether a value of type `a` might be accepted where `e` is expected.
/// `false` means that no value of type `a` can be.
pub fn compatible(ctx: &Ctx, a: &Ty, e: &Ty) -> bool {
    if a.is_any() || e.is_any() || a == e {
        return true;
    }
    match (a, e) {
        (Ty::Int, Ty::Float) => true,
        (Ty::List(x), Ty::List(y)) => compatible(ctx, x, y),
        (Ty::Map(k1, v1), Ty::Map(k2, v2)) => compatible(ctx, k1, k2) && compatible(ctx, v1, v2),
        (Ty::Set(x), Ty::Set(y)) => compatible(ctx, x, y),
        (Ty::Tuple(xs), Ty::Tuple(ys)) => xs.len() == ys.len() && xs.iter().zip(ys).all(|(x, y)| compatible(ctx, x, y)),
        (Ty::Named { id: i1, args: a1, .. }, Ty::Named { id: i2, args: a2, .. }) => {
            i1 == i2 && (a1.is_empty() || a2.is_empty() || (a1.len() == a2.len() && a1.iter().zip(a2).all(|(x, y)| compatible(ctx, x, y))))
        }
        // An anonymous record is converted to a declared record type with
        // exactly the same fields.
        (Ty::Record(fs), Ty::Named { id, args, .. }) => match ctx.types.get(*id as usize).map(|t| &t.kind) {
            Some(TypeKind::Record { fields, tys }) => {
                let fs: Vec<&(Name, Ty)> = fs.iter().filter(|(n, _)| !n.is_empty()).collect();
                fields.len() == fs.len()
                    && fs.iter().all(|(n, t)| fields.iter().position(|f| f == n).is_some_and(|i| compatible(ctx, t, &tys[i].subst(args))))
            }
            _ => false,
        },
        (Ty::Record(f1), Ty::Record(f2)) => {
            f2.iter().filter(|(n, _)| !n.is_empty()).all(|(n, t)| f1.iter().find(|(m, _)| m == n).is_some_and(|(_, u)| compatible(ctx, u, t)))
        }
        // A declared record is checked field by field against a structural annotation.
        (Ty::Named { .. }, Ty::Record(_)) => true,
        (Ty::Fn(..), Ty::Fn(..)) => true,
        _ => false,
    }
}

/// The type that results from joining two branches.
fn join(a: Ty, b: Ty) -> Ty {
    if a == b {
        a
    } else {
        Ty::Any
    }
}

fn option(t: Ty) -> Ty {
    Ty::Named { id: OPTION_ID, name: Rc::from("Option"), args: vec![t] }
}

fn result(t: Ty, e: Ty) -> Ty {
    Ty::Named { id: RESULT_ID, name: Rc::from("Result"), args: vec![t, e] }
}

fn list(t: Ty) -> Ty {
    Ty::List(Box::new(t))
}

/// The element type of a collection, for `for` loops and comprehensions.
fn element(t: &Ty) -> Ty {
    match t {
        Ty::List(e) => (**e).clone(),
        Ty::Range => Ty::Int,
        Ty::Str => Ty::Str,
        Ty::Map(k, v) => Ty::Tuple(vec![(**k).clone(), (**v).clone()]),
        Ty::Set(e) => (**e).clone(),
        _ => Ty::Any,
    }
}

/// A short description of a type for messages: "a Str", "an Int".
fn a(t: &Ty) -> String {
    let s = shown(t);
    let vowel = s.starts_with(['A', 'E', 'I', 'O']) || (s.starts_with('U') && !s.starts_with("Uni"));
    format!("{} {}", if vowel { "an" } else { "a" }, s)
}

/// A type as messages show it: unknown parts are left out (`List`, not
/// `List[Any]`) or written `_` (`Map[Str, _]`).
fn shown(t: &Ty) -> String {
    t.to_string().replace("[Any, Any]", "").replace("[Any]", "").replace("Any", "_")
}

impl<'a> Checker<'a> {
    fn frame(&mut self) -> &mut Frame {
        self.frames.last_mut().unwrap()
    }

    fn error(&mut self, span: Span, msg: String, label: &str) -> &mut Diagnostic {
        let d = Diagnostic::error("E0121", msg).at(span).label(label);
        self.diags.push(d);
        self.diags.last_mut().unwrap()
    }

    fn compatible(&self, a: &Ty, e: &Ty) -> bool {
        compatible(self.ctx, a, e)
    }

    /// An error for a value of the wrong type where `expected` was needed,
    /// with a hint when the value is a `/` and an Int was expected.
    fn mismatch(&mut self, span: Span, msg: String, label: &str, expected: &Ty) -> &mut Diagnostic {
        let hint = matches!(expected, Ty::Int) && self.divisions.contains(&span);
        let d = self.error(span, msg, label);
        if hint {
            d.help = Some("`/` always gives a Float; for an Int, use floor division `//`".into());
        }
        d
    }

    /// How to show an actual and an expected type in a message; when two
    /// different types print the same (a `Dir` here and one in a module),
    /// say where each is declared.
    fn pair(&self, actual: &Ty, expected: &Ty) -> (String, String) {
        let (x, y) = (a(actual), expected.to_string());
        if actual.to_string() != y {
            return (x, y);
        }
        let place = |t: &Ty| match t {
            Ty::Named { id, .. } => {
                self.type_def(*id).filter(|d| (d.span.file as usize) < self.ctx.sm.files.len()).map(|d| self.ctx.sm.location(d.span))
            }
            _ => None,
        };
        match (place(actual), place(expected)) {
            (Some(p), Some(q)) => (format!("{} (declared at {})", x, p), format!("{} (declared at {})", y, q)),
            _ => (x, y),
        }
    }

    fn type_def(&self, id: u32) -> Option<&Rc<TypeDef>> {
        self.ctx.types.get(id as usize)
    }

    // ------------------------------------------------------------ functions

    /// A record type's `where` clauses: Bool conditions on its fields.
    fn invariant(&mut self, id: u32) {
        let Some(def) = self.ctx.invariants.get(&id).cloned() else { return };
        let Some(TypeKind::Record { tys, .. }) = self.type_def(id).map(|t| &t.kind) else { return };
        let mut frame = Frame { name: def.name.clone(), ..Frame::default() };
        for (p, t) in def.params.iter().zip(tys) {
            frame.locals.insert(p.slot, erase_params(t));
        }
        self.frames.push(frame);
        for c in &def.requires {
            self.condition(c, "a type invariant (`where`)");
        }
        self.frames.pop();
    }

    fn function(&mut self, def: &FnDef, captures: &[Ty]) {
        let mut reassigned = HashSet::new();
        scan(&def.body, &mut reassigned);
        let mut frame = Frame {
            reassigned: reassigned.into_iter().filter_map(|r| if let VarRes::Local(s) = r { Some(s) } else { None }).collect(),
            captures: captures.to_vec(),
            ret: def.ret.as_ref().map(|t| t.ty.clone()).filter(|t| !t.is_any()),
            name: def.name.clone(),
            ..Frame::default()
        };
        for p in &def.params {
            let t = p.ty.as_ref().map_or(Ty::Any, |t| t.ty.clone());
            frame.locals.insert(p.slot, t);
        }
        self.frames.push(frame);
        for p in &def.params {
            if let Some(d) = &p.default {
                let t = self.expr(d);
                if let Some(ann) = &p.ty {
                    if !self.compatible(&t, &ann.ty) {
                        self.error(
                            d.span,
                            format!("the default value of `{}` is {}, but the parameter is declared as {}", p.name, a(&t), ann.ty),
                            "wrong type",
                        );
                    }
                }
            }
            if let Some(pat) = &p.pat {
                let t = p.ty.as_ref().map_or(Ty::Any, |t| t.ty.clone());
                self.bind(pat, &t, false);
            }
        }
        for r in &def.requires {
            self.condition(r, "a `requires` clause");
        }
        let body = self.expr(&def.body);
        if let Some(ret) = def.ret.as_ref().map(|t| t.ty.clone()) {
            if !self.compatible(&body, &ret) && !matches!(ret, Ty::Unit) {
                let span = tail_span(&def.body);
                self.mismatch(
                    span,
                    format!("`{}` is declared to return {}, but this is {}", def.display_name(), ret, a(&body)),
                    "returned here",
                    &ret,
                );
            }
            self.frame().locals.insert(def.result_slot, ret);
        }
        for (e, slot) in &def.olds {
            let t = self.expr(e);
            self.frame().locals.insert(*slot, t);
        }
        for e in &def.ensures {
            self.condition(e, "an `ensures` clause");
        }
        self.frames.pop();
    }

    fn fn_type(&self, def: &FnDef) -> Ty {
        let ps = def.params.iter().map(|p| p.ty.as_ref().map_or(Ty::Any, |t| t.ty.clone())).collect();
        Ty::Fn(ps, Box::new(def.ret.as_ref().map_or(Ty::Any, |t| t.ty.clone())))
    }

    fn capture_types(&self, def: &FnDef) -> Vec<Ty> {
        let f = self.frames.last().unwrap();
        def.captures
            .iter()
            .map(|c| match c {
                CaptureSrc::Local(s) => f.locals.get(s).cloned().unwrap_or(Ty::Any),
                CaptureSrc::Capture(i) => f.captures.get(*i as usize).cloned().unwrap_or(Ty::Any),
                CaptureSrc::SelfFn => Ty::Any,
            })
            .collect()
    }

    // ------------------------------------------------------------ statements

    fn stmt(&mut self, s: &Stmt) {
        match &s.kind {
            StmtKind::Let { pat, ty, value, mutable } => {
                let t = self.expr(value);
                let declared = ty.as_ref().map(|t| t.ty.clone()).filter(|t| !t.is_any());
                if let Some(d) = &declared {
                    if !self.compatible(&t, d) {
                        let what = match &pat.kind {
                            PatKind::Bind { name, .. } => format!("`{}`", name),
                            _ => "this pattern".into(),
                        };
                        let (shown_t, shown_d) = self.pair(&t, d);
                        let diag =
                            self.mismatch(value.span, format!("{} is declared as {}, but the value is {}", what, shown_d, shown_t), "wrong type", d);
                        if matches!(&value.kind, ExprKind::Block(stmts) if stmts.is_empty()) {
                            diag.help = Some(match d {
                                Ty::Map(..) => "`{}` is an empty block, not a map; an empty map is `[:]`".into(),
                                Ty::Set(_) => "`{}` is an empty block, not a set; an empty set is `to_set([])`".into(),
                                _ => "`{}` is an empty block, whose value is `()`".into(),
                            });
                        }
                    }
                }
                // A `var` without an annotation may later hold anything, unless
                // it holds a value of a declared type and is never assigned as
                // a whole (writes to its fields keep the type).
                let stable = match &pat.kind {
                    PatKind::Bind { res: VarRes::Local(s), sub: None, .. } => !self.frames.last().unwrap().reassigned.contains(s),
                    PatKind::Bind { res: VarRes::Global(s), sub: None, .. } => !self.reassigned_globals.contains(s),
                    _ => false,
                };
                let bound = match declared {
                    Some(d) => d,
                    // (Without its inferred type arguments: `var b = Box(1)`
                    // may later get `b.v = 2.5`, which is not checked.)
                    None if *mutable && stable && matches!(t, Ty::Named { .. }) => match t {
                        Ty::Named { id, name, .. } => Ty::Named { id, name, args: vec![] },
                        t => t,
                    },
                    None if *mutable => Ty::Any,
                    None => t,
                };
                self.bind(pat, &bound, false);
            }
            StmtKind::Assign { target, op, value, ty } => {
                let vt = self.expr(value);
                let tt = self.expr(target);
                if op.is_none() {
                    match (&target.kind, ty) {
                        (ExprKind::Var(v), Some(decl)) => {
                            if !self.compatible(&vt, decl) {
                                self.mismatch(
                                    value.span,
                                    format!("`{}` is declared as {}, but the value is {}", v.name, decl, a(&vt)),
                                    "wrong type",
                                    decl,
                                );
                            }
                        }
                        // A field or element: its type is known from the record
                        // type or the declared collection type.
                        (ExprKind::Field { .. } | ExprKind::Index { .. }, _) if !self.compatible(&vt, &tt) => {
                            let place = self.ctx.sm.snippet(target.span).to_string();
                            let (shown_v, shown_t) = self.pair(&vt, &tt);
                            self.mismatch(value.span, format!("`{}` is {}, but the value is {}", place, shown_t, shown_v), "wrong type", &tt);
                        }
                        _ => {}
                    }
                } else if let Some(op) = op {
                    self.binary(*op, &tt, &vt, s.span);
                }
            }
            StmtKind::Fn { def, res } => {
                let t = self.fn_type(def);
                if let VarRes::Local(slot) = res {
                    self.frame().locals.insert(*slot, t);
                }
                let caps = self.capture_types(def);
                self.function(def, &caps);
            }
            StmtKind::Assert { cond, msg } => {
                self.condition(cond, "`assert`");
                if let Some(m) = msg {
                    self.expr(m);
                }
            }
            StmtKind::Expr(e) => {
                self.expr(e);
            }
        }
    }

    /// Record the types of the names a pattern binds.
    fn bind(&mut self, pat: &Pattern, t: &Ty, unknown: bool) {
        let t = if unknown { Ty::Any } else { t.clone() };
        match &pat.kind {
            PatKind::Bind { res, sub, .. } => {
                match res {
                    VarRes::Local(slot) => {
                        self.frame().locals.insert(*slot, t.clone());
                    }
                    VarRes::Global(slot) => {
                        self.globals.insert(*slot, t.clone());
                    }
                    _ => {}
                }
                if let Some(p) = sub {
                    self.bind(p, &t, false);
                }
            }
            PatKind::Tuple(ps) => {
                for (i, p) in ps.iter().enumerate() {
                    let et = match &t {
                        Ty::Tuple(ts) if ts.len() == ps.len() => ts[i].clone(),
                        _ => Ty::Any,
                    };
                    self.bind(p, &et, false);
                }
            }
            PatKind::List { before, rest, after } => {
                let et = match &t {
                    Ty::List(e) => (**e).clone(),
                    Ty::Str => Ty::Str,
                    _ => Ty::Any,
                };
                for p in before.iter().chain(after.iter()) {
                    self.bind(p, &et, false);
                }
                if let Some(Some(r)) = rest {
                    let rt = match &t {
                        Ty::List(_) | Ty::Str => t.clone(),
                        _ => Ty::Any,
                    };
                    self.bind(r, &rt, false);
                }
            }
            PatKind::Ctor { ctor, args, field_idx, .. } => {
                let tys = self.field_types(ctor, &t);
                for ((_, p), idx) in args.iter().zip(field_idx) {
                    let ft = tys.as_ref().and_then(|v| v.get(*idx as usize).cloned()).unwrap_or(Ty::Any);
                    self.bind(p, &ft, false);
                }
            }
            PatKind::Record { fields, .. } => {
                for (n, p) in fields {
                    let ft = self.field_type(&t, n).unwrap_or(Ty::Any);
                    self.bind(p, &ft, false);
                }
            }
            PatKind::Or(alts) => {
                for p in alts {
                    self.bind(p, &t, false);
                }
            }
            PatKind::Wild | PatKind::Lit(_) | PatKind::Range { .. } => {}
        }
    }

    /// The declared field types of a constructor, with the type's arguments
    /// filled in from the scrutinee type when known.
    fn field_types(&self, ctor: &CtorRef, scrutinee: &Ty) -> Option<Vec<Ty>> {
        let td = self.type_def(ctor.type_id)?;
        let (_, tys, _) = if ctor.is_record { td.fields_of(0) } else { td.fields_of(ctor.tag) };
        let args: &[Ty] = match scrutinee {
            Ty::Named { id, args, .. } if *id == ctor.type_id => args,
            _ => &[],
        };
        Some(tys.iter().map(|t| if args.is_empty() { erase_params(t) } else { t.subst(args) }).collect())
    }

    /// The type of a field of a value of type `t`, if known.
    fn field_type(&self, t: &Ty, name: &str) -> Option<Ty> {
        match t {
            Ty::Record(fs) => fs.iter().find(|(n, _)| !n.is_empty() && &**n == name).map(|(_, t)| t.clone()),
            Ty::Named { id, args, .. } => {
                let td = self.type_def(*id)?;
                match &td.kind {
                    TypeKind::Record { fields, tys } => {
                        let i = fields.iter().position(|f| &**f == name)?;
                        Some(if args.is_empty() { erase_params(&tys[i]) } else { tys[i].subst(args) })
                    }
                    TypeKind::Enum { .. } => None,
                }
            }
            Ty::Tuple(ts) => ts.get(name.parse::<usize>().ok()?).cloned(),
            _ => None,
        }
    }

    // ------------------------------------------------------------ expressions

    fn condition(&mut self, e: &Expr, what: &str) {
        let t = self.expr(e);
        if !self.compatible(&t, &Ty::Bool) {
            self.error(e.span, format!("{} needs a Bool, but this is {}", what, a(&t)), "not a Bool").help =
                Some("Cogito has no truthiness: compare explicitly, e.g. `n != 0` or `not xs.is_empty()`".into());
        }
    }

    fn expr(&mut self, e: &Expr) -> Ty {
        match &e.kind {
            ExprKind::Unit => Ty::Unit,
            ExprKind::Bool(_) => Ty::Bool,
            ExprKind::Int(_) => Ty::Int,
            ExprKind::Float(_) => Ty::Float,
            ExprKind::Str(_) => Ty::Str,
            ExprKind::Interp(parts) => {
                for p in parts {
                    if let InterpPart::Expr(x, _) = p {
                        self.expr(x);
                    }
                }
                Ty::Str
            }
            ExprKind::Var(v) => self.var(v),
            ExprKind::List(items) => {
                let mut t: Option<Ty> = None;
                for it in items {
                    let x = self.expr(&it.expr);
                    let x = if it.spread { element(&x) } else { x };
                    t = Some(match t {
                        None => x,
                        Some(p) => join(p, x),
                    });
                }
                list(t.unwrap_or(Ty::Any))
            }
            ExprKind::Comprehension { body, clauses } => {
                for c in clauses {
                    match c {
                        CompClause::For(p, it) => {
                            let t = self.expr(it);
                            self.bind(p, &element(&t), false);
                        }
                        CompClause::If(cond) => self.condition(cond, "a comprehension's `if`"),
                    }
                }
                list(self.expr(body))
            }
            ExprKind::Map(entries) => {
                let (mut kt, mut vt): (Option<Ty>, Option<Ty>) = (None, None);
                for (k, v) in entries {
                    let a = self.expr(k);
                    let b = self.expr(v);
                    kt = Some(kt.map_or(a.clone(), |p| join(p, a)));
                    vt = Some(vt.map_or(b.clone(), |p| join(p, b)));
                }
                Ty::Map(Box::new(kt.unwrap_or(Ty::Any)), Box::new(vt.unwrap_or(Ty::Any)))
            }
            ExprKind::Tuple(items) => Ty::Tuple(items.iter().map(|x| self.expr(x)).collect()),
            ExprKind::Record { names, values, spread } => {
                let tys: Vec<Ty> = values.iter().map(|v| self.expr(v)).collect();
                match spread {
                    Some(s) => {
                        let st = self.expr(s);
                        match &st {
                            // `{ ..p, y: 5 }` keeps p's declared type, but not its
                            // type arguments (`{ ..b, v: "x" }` on a Box[Int]).
                            Ty::Named { id, name, .. } => {
                                let st = Ty::Named { id: *id, name: name.clone(), args: vec![] };
                                for ((n, t), v) in names.iter().zip(&tys).zip(values) {
                                    match self.field_type(&st, n) {
                                        Some(ft) if !self.compatible(t, &ft) => {
                                            self.error(v.span, format!("field `{}` of {} is {}, but this is {}", n, st, ft, a(t)), "wrong type");
                                        }
                                        None if self.is_record_type(&st) => {
                                            self.error(v.span, format!("{} has no field `{}`", st, n), "unknown field");
                                        }
                                        _ => {}
                                    }
                                }
                                st
                            }
                            _ => Ty::Any,
                        }
                    }
                    // A literal's fields are exactly these (marked by a field
                    // named ""); an annotation `{ a: Int }` accepts records
                    // with more fields.
                    None => Ty::Record(names.iter().cloned().zip(tys).chain(std::iter::once((Name::from(""), Ty::Unit))).collect()),
                }
            }
            ExprKind::Field { target, name, name_span } => {
                let t = self.expr(target);
                match self.field_type(&t, name) {
                    Some(ft) => ft,
                    None => {
                        let missing = match &t {
                            Ty::Record(fs) => fs.iter().any(|(n, _)| n.is_empty()),
                            Ty::Named { .. } => self.is_record_type(&t),
                            _ => false,
                        };
                        if missing {
                            let fields = self.field_names(&t);
                            let d = self.error(*name_span, format!("{} has no field `{}`", t, name), "unknown field");
                            if !fields.is_empty() {
                                d.notes.push(format!("its fields are: {}", fields.join(", ")));
                            }
                        }
                        Ty::Any
                    }
                }
            }
            ExprKind::Index { target, index } => {
                let t = self.expr(target);
                let i = self.expr(index);
                match (&t, &i) {
                    (Ty::List(_) | Ty::Str, Ty::Range) => t,
                    (Ty::List(e), Ty::Int) => (**e).clone(),
                    (Ty::Str, Ty::Int) => Ty::Str,
                    (Ty::Map(_, v), _) => (**v).clone(),
                    (Ty::Tuple(ts), _) => match &index.kind {
                        ExprKind::Int(n) => usize::try_from(*n).ok().and_then(|n| ts.get(n).cloned()).unwrap_or(Ty::Any),
                        _ => Ty::Any,
                    },
                    _ => Ty::Any,
                }
            }
            ExprKind::Call { callee, args } => {
                let arg_tys: Vec<(Option<Name>, Ty, Span)> = args.iter().map(|a| (a.name.clone(), self.expr(&a.value), a.value.span)).collect();
                match &callee.kind {
                    ExprKind::Var(v) => self.call_named(v, None, &arg_tys, e.span),
                    // A function value's result is not checked against a
                    // `fn(A) -> B` annotation when the program runs, so it is
                    // not assumed here either.
                    _ => {
                        self.expr(callee);
                        Ty::Any
                    }
                }
            }
            ExprKind::MethodCall { receiver, method, args, mutating, .. } => {
                let rt = self.expr(receiver);
                let arg_tys: Vec<(Option<Name>, Ty, Span)> = args.iter().map(|a| (a.name.clone(), self.expr(&a.value), a.value.span)).collect();
                // A record's own field, or a function from the module that
                // declared the receiver's type, may take precedence.
                if self.field_type(&rt, &method.name).is_some()
                    || self.has_home(&rt)
                    || *mutating
                    || (matches!(rt, Ty::Any) && (self.ctx.module_fns.contains(&method.name) || self.ctx.known_fields.contains(&method.name)))
                {
                    return Ty::Any;
                }
                self.call_named(method, Some((rt, receiver.span)), &arg_tys, e.span)
            }
            ExprKind::Unary { op, expr } => {
                let t = self.expr(expr);
                match op {
                    UnOp::Not => {
                        if !self.compatible(&t, &Ty::Bool) {
                            self.error(expr.span, format!("`not` needs a Bool, but this is {}", a(&t)), "not a Bool");
                        }
                        Ty::Bool
                    }
                    UnOp::Neg => match t {
                        Ty::Int | Ty::Float => t,
                        Ty::Any | Ty::Generic(_) | Ty::Param(..) => Ty::Any,
                        other => {
                            self.error(expr.span, format!("cannot negate {}", a(&other)), "not a number");
                            Ty::Any
                        }
                    },
                }
            }
            ExprKind::Binary { op, lhs, rhs } => {
                let l = self.expr(lhs);
                let r = self.expr(rhs);
                self.binary(*op, &l, &r, e.span)
            }
            ExprKind::And(x, y) | ExprKind::Or(x, y) => {
                let what = if matches!(e.kind, ExprKind::And(..)) { "`and`" } else { "`or`" };
                self.condition(x, what);
                self.condition(y, what);
                Ty::Bool
            }
            ExprKind::Range { start, end, .. } => {
                for x in std::iter::once(&**start).chain(end.as_deref()) {
                    let t = self.expr(x);
                    if !self.compatible(&t, &Ty::Int) {
                        self.error(x.span, format!("range bounds must be Ints, but this is {}", a(&t)), "not an Int");
                    }
                }
                Ty::Range
            }
            ExprKind::Try(inner) => match self.expr(inner) {
                Ty::Named { id, args, .. } if (id == OPTION_ID || id == RESULT_ID) && !args.is_empty() => args[0].clone(),
                _ => Ty::Any,
            },
            ExprKind::Is { expr, .. } => {
                self.expr(expr);
                Ty::Bool
            }
            ExprKind::If { cond, then, els } => {
                self.condition(cond, "an `if` condition");
                let a = self.expr(then);
                match els {
                    Some(x) => {
                        let b = self.expr(x);
                        join(a, b)
                    }
                    None => Ty::Any,
                }
            }
            ExprKind::Match { scrutinee, arms } => {
                let st = self.expr(scrutinee);
                let mut t: Option<Ty> = None;
                for arm in arms {
                    self.bind(&arm.pat, &st, false);
                    if let Some(g) = &arm.guard {
                        self.condition(g, "a match guard");
                    }
                    let at = self.expr(&arm.body);
                    t = Some(t.map_or(at.clone(), |p| join(p, at)));
                }
                t.unwrap_or(Ty::Any)
            }
            ExprKind::Block(stmts) => {
                let mut last = Ty::Unit;
                for (i, s) in stmts.iter().enumerate() {
                    match &s.kind {
                        StmtKind::Expr(x) if i + 1 == stmts.len() => last = self.expr(x),
                        _ => self.stmt(s),
                    }
                }
                // A block that stops partway (`return`, `assert false`,
                // `panic(...)`) never produces its last value.
                if stmts.iter().any(diverges) {
                    Ty::Any
                } else {
                    last
                }
            }
            ExprKind::Lambda(def) => {
                let caps = self.capture_types(def);
                self.function(def, &caps);
                self.fn_type(def)
            }
            // A loop's value is `()`, but a `return` inside it may be the
            // function's real result, so it is not treated as known.
            ExprKind::While { cond, body } => {
                self.condition(cond, "a `while` condition");
                self.expr(body);
                Ty::Any
            }
            ExprKind::For { pat, iter, body } => {
                let t = self.expr(iter);
                self.bind(pat, &element(&t), false);
                self.expr(body);
                Ty::Any
            }
            ExprKind::Loop { body } => {
                self.expr(body);
                Ty::Any
            }
            ExprKind::Break(v) => {
                if let Some(v) = v {
                    self.expr(v);
                }
                Ty::Any
            }
            ExprKind::Continue => Ty::Any,
            ExprKind::Return(v) => {
                let t = v.as_ref().map_or(Ty::Unit, |v| self.expr(v));
                let (ret, name) = {
                    let f = self.frames.last().unwrap();
                    (f.ret.clone(), f.name.clone())
                };
                if let (Some(ret), Some(v)) = (ret, v) {
                    if !self.compatible(&t, &ret) {
                        let name = name.map_or("this function".to_string(), |n| format!("`{}`", n));
                        self.mismatch(v.span, format!("{} is declared to return {}, but this is {}", name, ret, a(&t)), "returned here", &ret);
                    }
                }
                Ty::Any
            }
        }
    }

    fn var(&mut self, v: &Var) -> Ty {
        match v.res {
            VarRes::Local(s) => self.frames.last().unwrap().locals.get(&s).cloned().unwrap_or(Ty::Any),
            VarRes::Capture(i) => self.frames.last().unwrap().captures.get(i as usize).cloned().unwrap_or(Ty::Any),
            VarRes::Global(s) => {
                let info = &self.ctx.globals[s as usize];
                match &info.kind {
                    GlobalKind::Const => match &*info.name {
                        "max_int" | "min_int" => Ty::Int,
                        _ => Ty::Float,
                    },
                    GlobalKind::Let | GlobalKind::Var => info.ty.clone().or_else(|| self.globals.get(&s).cloned()).unwrap_or(Ty::Any),
                    GlobalKind::Fn => match self.ctx.sigs.get(&s).map(|v| v.as_slice()) {
                        Some([sig]) => {
                            Ty::Fn(sig.params.iter().map(|p| p.2.clone().unwrap_or(Ty::Any)).collect(), Box::new(sig.ret.clone().unwrap_or(Ty::Any)))
                        }
                        _ => Ty::Any,
                    },
                    // A constructor with no fields is a value of its type.
                    GlobalKind::Ctor(c) => match self.type_def(c.type_id) {
                        Some(td) if !c.is_record && td.fields_of(c.tag).1.is_empty() => {
                            Ty::Named { id: c.type_id, name: td.name.clone(), args: if c.type_id == OPTION_ID { vec![Ty::Any] } else { vec![] } }
                        }
                        _ => Ty::Any,
                    },
                    _ => Ty::Any,
                }
            }
            _ => Ty::Any,
        }
    }

    fn has_home(&self, t: &Ty) -> bool {
        matches!(t, Ty::Named { id, .. } if self.ctx.type_home.contains_key(id))
    }

    fn is_record_type(&self, t: &Ty) -> bool {
        matches!(t, Ty::Named { id, .. } if matches!(self.type_def(*id).map(|d| &d.kind), Some(TypeKind::Record { .. })))
    }

    fn field_names(&self, t: &Ty) -> Vec<String> {
        match t {
            Ty::Record(fs) => fs.iter().filter(|(n, _)| !n.is_empty()).map(|(n, _)| n.to_string()).collect(),
            Ty::Named { id, .. } => match self.type_def(*id).map(|d| &d.kind) {
                Some(TypeKind::Record { fields, .. }) => fields.iter().map(|f| f.to_string()).collect(),
                _ => vec![],
            },
            _ => vec![],
        }
    }

    /// A call of a named function (with the receiver, for method syntax).
    fn call_named(&mut self, v: &Var, receiver: Option<(Ty, Span)>, args: &[(Option<Name>, Ty, Span)], span: Span) -> Ty {
        let mut all: Vec<(Option<Name>, Ty, Span)> = Vec::new();
        let receiver_given = receiver.is_some();
        if let Some((t, sp)) = receiver {
            all.push((None, t, sp));
        }
        all.extend(args.iter().cloned());
        match v.res {
            VarRes::Global(s) => {
                let kind = self.ctx.globals[s as usize].kind.clone();
                match kind {
                    GlobalKind::Fn => self.call_user(s, &v.name, &all),
                    GlobalKind::Ctor(c) => self.call_ctor(c, &all),
                    GlobalKind::Builtin(i) => {
                        let name = crate::builtins::BUILTINS[i as usize].name;
                        self.check_builtin_args(name, receiver_given, &all);
                        builtin_result(name, &all)
                    }
                    _ => Ty::Any,
                }
            }
            // (See `ExprKind::Call`: a function value's result is unknown.)
            VarRes::Local(_) | VarRes::Capture(_) => {
                let _ = span;
                Ty::Any
            }
            _ => Ty::Any,
        }
    }

    /// Report arguments of a built-in that can never have the kind it needs
    /// (`"a,b".split(1)`), as the built-in would when the program runs.
    fn check_builtin_args(&mut self, name: &str, method: bool, args: &[(Option<Name>, Ty, Span)]) {
        let kinds = builtin_kinds(name);
        for (i, (n, t, span)) in args.iter().enumerate() {
            if n.is_some() {
                break;
            }
            let Some(k) = kinds.get(i) else { break };
            if k.excludes(t) {
                let which = if method && i == 0 { format!("the receiver of `.{}()`", name) } else { format!("argument {} of `{}`", i + 1, name) };
                self.error(*span, format!("{} must be {}, but this is {}", which, k.describe(), a(t)), &format!("not {}", k.describe()));
            }
        }
    }

    fn call_user(&mut self, slot: u32, name: &str, args: &[(Option<Name>, Ty, Span)]) -> Ty {
        let Some(sigs) = self.ctx.sigs.get(&slot) else { return Ty::Any };
        // Overloads, and functions that fall back to a built-in, are left to
        // the runtime's dispatch.
        if sigs.len() != 1 || self.ctx.builtins.values.contains_key(name) {
            return Ty::Any;
        }
        let sig = sigs[0].clone();
        let mut pos = 0;
        for (n, t, sp) in args {
            let idx = match n {
                Some(n) => sig.params.iter().position(|p| &p.0 == n),
                None => {
                    let i = pos;
                    pos += 1;
                    Some(i)
                }
            };
            let Some((pname, _, Some(pty))) = idx.and_then(|i| sig.params.get(i)) else { continue };
            if !self.compatible(t, pty) {
                let (shown_t, shown_p) = self.pair(t, pty);
                let shown = if pname.starts_with("__arg") { "this parameter".to_string() } else { format!("`{}`", pname) };
                let at = if (sig.span.file as usize) < self.ctx.sm.files.len() { Some(self.ctx.sm.location(sig.span)) } else { None };
                let d = self.mismatch(
                    *sp,
                    format!("`{}` expects {} to be {}, but this argument is {}", name, shown, shown_p, shown_t),
                    "wrong type",
                    pty,
                );
                if let Some(at) = at {
                    d.notes.push(format!("`{}` is declared at {}", name, at));
                }
            }
        }
        sig.ret.clone().unwrap_or(Ty::Any)
    }

    fn call_ctor(&mut self, c: CtorRef, args: &[(Option<Name>, Ty, Span)]) -> Ty {
        let Some(td) = self.type_def(c.type_id).cloned() else { return Ty::Any };
        let (fields, tys, _) = if c.is_record { td.fields_of(0) } else { td.fields_of(c.tag) };
        let generic = !td.params.is_empty();
        let mut pos = 0;
        let mut inferred: Vec<Option<Ty>> = vec![None; td.params.len()];
        for (n, t, sp) in args {
            let idx = match n {
                Some(n) => fields.iter().position(|f| f == n),
                None => {
                    let i = pos;
                    pos += 1;
                    Some(i)
                }
            };
            let Some(i) = idx.filter(|i| *i < tys.len()) else { continue };
            let ft = &tys[i];
            if let Ty::Param(p, _) = ft {
                if let Some(slot) = inferred.get_mut(*p as usize) {
                    *slot = Some(match slot.take() {
                        None => t.clone(),
                        Some(prev) => join(prev, t.clone()),
                    });
                }
                continue;
            }
            let ft = erase_params(ft);
            if !self.compatible(t, &ft) {
                let ctor = td.ctor_name(if c.is_record { 0 } else { c.tag });
                let field = if fields[i].parse::<usize>().is_ok() { format!("field {}", i + 1) } else { format!("field `{}`", fields[i]) };
                self.mismatch(*sp, format!("{} of `{}` is {}, but this is {}", field, ctor, ft, a(t)), "wrong type", &ft);
            }
        }
        let args = if generic && inferred.iter().all(|t| t.is_some()) {
            inferred.into_iter().map(|t| t.unwrap()).collect()
        } else if generic {
            inferred.into_iter().map(|t| t.unwrap_or(Ty::Any)).collect()
        } else {
            vec![]
        };
        Ty::Named { id: c.type_id, name: td.name.clone(), args }
    }

    fn binary(&mut self, op: BinOp, l: &Ty, r: &Ty, span: Span) -> Ty {
        use BinOp::*;
        if op == Div {
            self.divisions.insert(span);
        }
        let num = |t: &Ty| matches!(t, Ty::Int | Ty::Float);
        let unknown = |t: &Ty| t.is_any();
        let bad = |me: &mut Self, what: String| {
            me.error(span, what, "unsupported operands");
            Ty::Any
        };
        match op {
            Eq | Ne => Ty::Bool,
            Lt | Le | Gt | Ge => {
                let comparable = unknown(l)
                    || unknown(r)
                    || (num(l) && num(r))
                    // Ranges, maps and functions have no order.
                    || (l == r && !matches!(l, Ty::Range | Ty::Map(..) | Ty::Set(..) | Ty::Fn(..)))
                    || matches!(
                        (l, r),
                        (Ty::List(_), Ty::List(_))
                            | (Ty::Tuple(_), Ty::Tuple(_))
                            | (Ty::Named { .. }, Ty::Named { .. })
                            | (Ty::Record(_), Ty::Record(_))
                    );
                if !comparable {
                    bad(self, format!("cannot compare {} with {}", a(l), a(r)));
                }
                Ty::Bool
            }
            In | NotIn => Ty::Bool,
            Add => match (l, r) {
                _ if unknown(l) || unknown(r) => Ty::Any,
                (Ty::Int, Ty::Int) => Ty::Int,
                _ if num(l) && num(r) => Ty::Float,
                (Ty::Str, Ty::Str) => Ty::Str,
                (Ty::List(x), Ty::List(y)) => list(join((**x).clone(), (**y).clone())),
                (Ty::Str, _) | (_, Ty::Str) => {
                    let d = self.error(span, format!("cannot add {} and {}", a(l), a(r)), "unsupported operands");
                    d.help = Some("there are no implicit conversions: use interpolation, `\"{x}\"`, or `str(x)`".into());
                    Ty::Any
                }
                _ => bad(self, format!("cannot add {} and {}", a(l), a(r))),
            },
            Sub | Div | FloorDiv | Mod | Pow => match (l, r) {
                _ if unknown(l) || unknown(r) => Ty::Any,
                _ if num(l) && num(r) => match op {
                    Div => Ty::Float,
                    Pow => Ty::Any,
                    _ if matches!((l, r), (Ty::Int, Ty::Int)) => Ty::Int,
                    _ => Ty::Float,
                },
                _ => bad(self, format!("`{}` needs numbers, but got {} and {}", op.symbol(), a(l), a(r))),
            },
            Mul => match (l, r) {
                _ if unknown(l) || unknown(r) => Ty::Any,
                (Ty::Int, Ty::Int) => Ty::Int,
                _ if num(l) && num(r) => Ty::Float,
                (Ty::Str, Ty::Int) | (Ty::Int, Ty::Str) => Ty::Str,
                (Ty::List(_), Ty::Int) => l.clone(),
                (Ty::Int, Ty::List(_)) => r.clone(),
                _ => bad(self, format!("cannot multiply {} by {}", a(l), a(r))),
            },
        }
    }
}

/// Replace a type declaration's own parameters with `Any`.
fn erase_params(t: &Ty) -> Ty {
    match t {
        Ty::Param(..) => Ty::Any,
        Ty::List(x) => list(erase_params(x)),
        Ty::Map(k, v) => Ty::Map(Box::new(erase_params(k)), Box::new(erase_params(v))),
        Ty::Set(x) => Ty::Set(Box::new(erase_params(x))),
        Ty::Tuple(ts) => Ty::Tuple(ts.iter().map(erase_params).collect()),
        Ty::Record(fs) => Ty::Record(fs.iter().map(|(n, t)| (n.clone(), erase_params(t))).collect()),
        Ty::Fn(ps, r) => Ty::Fn(ps.iter().map(erase_params).collect(), Box::new(erase_params(r))),
        Ty::Named { id, name, args } => Ty::Named { id: *id, name: name.clone(), args: args.iter().map(erase_params).collect() },
        other => other.clone(),
    }
}

/// The span of the expression that gives a function body its value.
fn tail_span(e: &Expr) -> Span {
    match &e.kind {
        ExprKind::Block(stmts) => match stmts.last() {
            Some(Stmt { kind: StmtKind::Expr(x), .. }) => tail_span(x),
            Some(s) => s.span,
            None => e.span,
        },
        _ => e.span,
    }
}

/// What a built-in's positional parameter must be, where that is certain
/// (the built-in fails at run time otherwise).
#[derive(Clone, Copy, PartialEq)]
enum Kind {
    /// Anything (not checked).
    Any,
    Str,
    Int,
    /// Int or Float.
    Num,
    Fn,
    Set,
    /// A List, Str, Map, Set, Tuple or Range.
    Sized,
}

fn builtin_kinds(name: &str) -> &'static [Kind] {
    use Kind::*;
    match name {
        "lines" | "words" | "chars" | "trim" | "trim_start" | "trim_end" | "upper" | "lower" | "capitalize" | "is_digit" | "is_alpha"
        | "is_alnum" | "is_space" | "is_upper" | "is_lower" | "parse_float" | "ord" | "read_file" | "file_exists" | "list_dir" | "env"
        | "parse_json" => &[Str],
        "split_once" | "strip_prefix" | "strip_suffix" | "starts_with" | "ends_with" | "write_file" | "append_file" => &[Str, Str],
        "split" => &[Str, Str, Int],
        "replace" => &[Str, Str, Str],
        "parse_int" => &[Str, Int],
        "chr" | "bit_not" | "seed" | "exit" => &[Int],
        "gcd" | "lcm" | "wrapping_add" | "wrapping_sub" | "wrapping_mul" | "bit_and" | "bit_or" | "bit_xor" | "shl" | "shr" | "random_int" => {
            &[Int, Int]
        }
        "abs" | "sqrt" | "exp" | "ln" | "log2" | "log10" | "sin" | "cos" | "tan" | "asin" | "acos" | "atan" | "floor" | "ceil" | "trunc" | "sign"
        | "is_nan" | "sleep" => &[Num],
        "pow" | "log" | "atan2" | "hypot" => &[Num, Num],
        "round" | "fixed" => &[Num, Int],
        "len" => &[Sized],
        "map" | "filter" | "each" | "find" | "find_index" | "take_while" | "drop_while" | "flat_map" | "min_by" | "max_by" | "sort_by"
        | "sort_with" | "group_by" | "partition" | "map_values" | "map_err" | "and_then" | "unwrap_or_else" | "reduce" | "any" | "all" => &[Any, Fn],
        "fold" => &[Any, Any, Fn],
        "update" => &[Any, Any, Any, Fn],
        "catch" => &[Fn],
        "take" | "drop" | "chunks" | "windows" | "repeat" => &[Any, Int],
        "pad_left" | "pad_right" => &[Any, Int, Str],
        "join" => &[Any, Str],
        "union" | "intersection" | "difference" | "is_subset" => &[Set, Set],
        _ => &[],
    }
}

impl Kind {
    /// Whether a value of type `t` can never be of this kind.
    fn excludes(self, t: &Ty) -> bool {
        let known = !matches!(t, Ty::Any | Ty::Generic(_) | Ty::Param(..));
        known
            && !match self {
                Kind::Any => true,
                Kind::Str => matches!(t, Ty::Str),
                Kind::Int => matches!(t, Ty::Int),
                Kind::Num => matches!(t, Ty::Int | Ty::Float),
                Kind::Fn => matches!(t, Ty::Fn(..)),
                Kind::Set => matches!(t, Ty::Set(_)),
                Kind::Sized => matches!(t, Ty::List(_) | Ty::Str | Ty::Map(..) | Ty::Set(_) | Ty::Tuple(_) | Ty::Range),
            }
    }

    fn describe(self) -> &'static str {
        match self {
            Kind::Any => "anything",
            Kind::Str => "a Str",
            Kind::Int => "an Int",
            Kind::Num => "a number",
            Kind::Fn => "a function",
            Kind::Set => "a Set",
            Kind::Sized => "a collection or string",
        }
    }
}

/// Result types of built-ins whose result type is certain.
fn builtin_result(name: &str, args: &[(Option<Name>, Ty, Span)]) -> Ty {
    let first = args.first().map(|a| a.1.clone()).unwrap_or(Ty::Any);
    let elem = element(&first);
    match name {
        "len" | "count" | "ord" | "gcd" | "lcm" | "bit_and" | "bit_or" | "bit_xor" | "bit_not" | "shl" | "shr" | "wrapping_add" | "wrapping_sub"
        | "wrapping_mul" | "hash" | "random_int" | "int" | "floor" | "ceil" | "trunc" | "sign" => Ty::Int,
        "round" if args.len() == 1 => Ty::Int,
        "round" => Ty::Float,
        "str" | "repr" | "upper" | "lower" | "trim" | "trim_start" | "trim_end" | "capitalize" | "replace" | "pad_left" | "pad_right" | "join"
        | "chr" | "fixed" | "to_json" | "type_of" | "read_stdin" | "input" => Ty::Str,
        "sqrt" | "exp" | "ln" | "log" | "sin" | "cos" | "tan" | "asin" | "acos" | "atan" | "hypot" | "random" | "float" | "time" | "clock" => {
            Ty::Float
        }
        "is_empty" | "contains" | "starts_with" | "ends_with" | "is_digit" | "is_alpha" | "is_alnum" | "is_space" | "is_upper" | "is_lower"
        | "is_some" | "is_none" | "is_ok" | "is_err" | "has" | "any" | "all" | "file_exists" | "is_nan" => Ty::Bool,
        "split" | "lines" | "words" | "chars" | "args" => list(Ty::Str),
        "parse_int" | "index_of" | "find_index" => option(Ty::Int),
        "parse_float" => option(Ty::Float),
        "read_line" | "env" => option(Ty::Str),
        "read_file" => result(Ty::Str, Ty::Str),
        "keys" => match &first {
            Ty::Map(k, _) => list((**k).clone()),
            _ => Ty::Any,
        },
        "values" => match &first {
            Ty::Map(_, v) => list((**v).clone()),
            _ => Ty::Any,
        },
        "entries" => match &first {
            Ty::Map(k, v) => list(Ty::Tuple(vec![(**k).clone(), (**v).clone()])),
            _ => Ty::Any,
        },
        "sort" | "reverse" | "unique" | "filter" | "take" | "drop" | "slice" | "take_while" | "drop_while" => match &first {
            Ty::List(_) | Ty::Str => first.clone(),
            _ => Ty::Any,
        },
        "to_set" => match &first {
            Ty::Set(_) => first.clone(),
            Ty::List(_) | Ty::Str | Ty::Range => Ty::Set(Box::new(elem)),
            _ if args.is_empty() => Ty::Set(Box::new(Ty::Any)),
            _ => Ty::Any,
        },
        "union" | "intersection" | "difference" => match &first {
            Ty::Set(_) => first.clone(),
            _ => Ty::Any,
        },
        "is_subset" => Ty::Bool,
        "shuffle" => match &first {
            Ty::List(_) => first.clone(),
            Ty::Str => list(Ty::Str),
            _ => Ty::Any,
        },
        "first" | "last" | "get" => match &first {
            Ty::List(_) | Ty::Str | Ty::Range => option(elem),
            Ty::Map(_, v) if name == "get" => option((**v).clone()),
            _ => Ty::Any,
        },
        "enumerate" => match &first {
            Ty::List(_) | Ty::Str | Ty::Range => list(Ty::Tuple(vec![Ty::Int, elem])),
            _ => Ty::Any,
        },
        "to_list" => match &first {
            Ty::List(_) | Ty::Str | Ty::Range => list(elem),
            _ => Ty::Any,
        },
        "range" => Ty::Range,
        "unwrap" | "expect" | "unwrap_or" | "unwrap_or_else" => match &first {
            Ty::Named { id, args: targs, .. } if (*id == OPTION_ID || *id == RESULT_ID) && !targs.is_empty() => {
                // The default may be of another type (`env("PORT").unwrap_or(8080)`).
                let default = match (name, args.get(1).map(|a| &a.1)) {
                    ("unwrap_or", Some(t)) => Some(t.clone()),
                    ("unwrap_or_else", Some(Ty::Fn(_, r))) => Some((**r).clone()),
                    ("unwrap_or" | "unwrap_or_else", _) => Some(Ty::Any),
                    _ => None,
                };
                match default {
                    Some(d) => join(targs[0].clone(), d),
                    None => targs[0].clone(),
                }
            }
            _ => Ty::Any,
        },
        _ => Ty::Any,
    }
}
