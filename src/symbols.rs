//! Where each variable and function of a file is declared and used, from
//! the resolved program: for the language server's go-to-definition, find
//! references and rename.

use crate::ast::*;
use crate::span::Span;
use crate::types::Name;
use std::collections::HashMap;
use std::rc::Rc;

/// What a name refers to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Sym {
    Global(u32),
    /// A slot in the frame of one function (identified by its address).
    Local(usize, u32),
}

#[derive(Clone, Debug)]
pub struct Occurrence {
    /// The name itself.
    pub span: Span,
    pub sym: Sym,
    /// Where the name is introduced (a `let`, a parameter, a pattern, a
    /// function's name).
    pub decl: bool,
    /// A record field written as its variable alone (`{ x }`): renaming the
    /// variable must keep the field's name (`{ x: y }`).
    pub field: Option<Name>,
}

struct Frame {
    id: usize,
    captures: Vec<Option<Sym>>,
    self_sym: Option<Sym>,
}

struct Walker<'a> {
    src: &'a str,
    file: u32,
    out: Vec<Occurrence>,
    frames: Vec<Frame>,
    /// Top-level functions by global slot (for named arguments).
    fns: HashMap<u32, Vec<Rc<FnDef>>>,
}

/// Every occurrence of a variable or function name in `prog` (whose source
/// is `src`), sorted by position.
pub fn occurrences(prog: &Program, src: &str) -> Vec<Occurrence> {
    let mut w =
        Walker { src, file: prog.file, out: Vec::new(), frames: vec![Frame { id: 0, captures: Vec::new(), self_sym: None }], fns: HashMap::new() };
    for item in &prog.items {
        if let Item::Fn(def) = item {
            if let Some(s) = def.global_slot {
                w.fns.entry(s).or_default().push(def.clone());
            }
        }
    }
    for item in &prog.items {
        match item {
            Item::Fn(def) => {
                let sym = def.global_slot.map(Sym::Global);
                if let Some(s) = sym {
                    w.push(def.name_span, s, true, None);
                }
                w.function(def, sym);
            }
            Item::Test(t) => w.function(&t.func, None),
            Item::Property(p) => w.function(&p.func, None),
            Item::Stmt(s) => w.stmt(s),
            Item::Type(_) | Item::Import(_) => {}
        }
    }
    let mut out = w.out;
    out.sort_by_key(|o| (o.span.start, o.span.end));
    out.dedup_by_key(|o| (o.span.start, o.span.end));
    out
}

impl Walker<'_> {
    fn frame(&self) -> &Frame {
        self.frames.last().unwrap()
    }

    fn sym(&self, res: VarRes) -> Option<Sym> {
        let f = self.frame();
        match res {
            VarRes::Local(s) => Some(Sym::Local(f.id, s)),
            VarRes::Capture(i) => f.captures.get(i as usize).copied().flatten(),
            VarRes::Global(s) => Some(Sym::Global(s)),
            VarRes::SelfFn => f.self_sym,
            VarRes::Unresolved => None,
        }
    }

    /// Record `span` if it holds exactly a name (not, say, `geo.origin`
    /// rewritten into a reference to the module's global).
    fn push(&mut self, span: Span, sym: Sym, decl: bool, field: Option<Name>) {
        if span.file != self.file {
            return;
        }
        let text = self.src.get(span.start as usize..span.end as usize).unwrap_or("");
        let is_name = !text.is_empty() && text.trim_end_matches('!').chars().all(|c| c.is_alphanumeric() || c == '_');
        if is_name {
            self.out.push(Occurrence { span, sym, decl, field });
        }
    }

    fn name_span(span: Span, name: &str) -> Span {
        Span { end: span.start + name.len() as u32, ..span }
    }

    /// Whether the record field `name` at `span` is written alone (`{ x }`),
    /// not as `x: value`.
    fn shorthand(&self, span: Span, name: &str) -> bool {
        let before = self.src[..span.start as usize].trim_end();
        self.src.get(span.start as usize..span.end as usize) == Some(name) && (before.ends_with('{') || before.ends_with(','))
    }

    fn function(&mut self, def: &FnDef, self_sym: Option<Sym>) {
        let parent = self.frame();
        let captures = def
            .captures
            .iter()
            .map(|c| match c {
                CaptureSrc::Local(s) => Some(Sym::Local(parent.id, *s)),
                CaptureSrc::Capture(i) => parent.captures.get(*i as usize).copied().flatten(),
                CaptureSrc::SelfFn => parent.self_sym,
            })
            .collect();
        let id = def as *const FnDef as usize;
        self.frames.push(Frame { id, captures, self_sym });
        for p in &def.params {
            match &p.pat {
                Some(pat) => self.pattern(pat),
                None => self.push(Self::name_span(p.span, &p.name), Sym::Local(id, p.slot), true, None),
            }
            if let Some(d) = &p.default {
                self.expr(d);
            }
        }
        for r in &def.requires {
            self.expr(r);
        }
        self.expr(&def.body);
        for e in &def.ensures {
            self.expr(e);
        }
        for (e, _) in &def.olds {
            self.expr(e);
        }
        self.frames.pop();
    }

    fn stmt(&mut self, s: &Stmt) {
        match &s.kind {
            StmtKind::Let { pat, value, .. } => {
                self.pattern(pat);
                // `let f = fn(n) => ... f(n - 1) ...`: the function's own name.
                match (&value.kind, &pat.kind) {
                    (ExprKind::Lambda(def), PatKind::Bind { res, .. }) => {
                        let me = self.sym(*res);
                        self.function(def, me);
                    }
                    _ => self.expr(value),
                }
            }
            StmtKind::Assign { target, value, .. } => {
                self.expr(target);
                self.expr(value);
            }
            StmtKind::Fn { def, res } => {
                let me = self.sym(*res);
                if let Some(s) = me {
                    self.push(def.name_span, s, true, None);
                }
                self.function(def, me);
            }
            StmtKind::Assert { cond, msg } => {
                self.expr(cond);
                if let Some(m) = msg {
                    self.expr(m);
                }
            }
            StmtKind::Expr(e) => self.expr(e),
        }
    }

    fn pattern(&mut self, p: &Pattern) {
        match &p.kind {
            PatKind::Bind { name, res, sub } => {
                if let Some(s) = self.sym(*res) {
                    self.push(Self::name_span(p.span, name), s, true, None);
                }
                if let Some(sub) = sub {
                    self.pattern(sub);
                }
            }
            PatKind::Tuple(ps) | PatKind::Or(ps) => ps.iter().for_each(|q| self.pattern(q)),
            PatKind::List { before, rest, after } => {
                before.iter().chain(after.iter()).for_each(|q| self.pattern(q));
                if let Some(Some(r)) = rest {
                    self.pattern(r);
                }
            }
            PatKind::Ctor { args, .. } => args.iter().for_each(|(_, q)| self.pattern(q)),
            PatKind::Record { fields, .. } => {
                for (field, q) in fields {
                    match &q.kind {
                        PatKind::Bind { name, res, sub: None } if name == field && self.shorthand(q.span, name) => {
                            if let Some(s) = self.sym(*res) {
                                self.push(q.span, s, true, Some(field.clone()));
                            }
                        }
                        _ => self.pattern(q),
                    }
                }
            }
            PatKind::Wild | PatKind::Lit(_) | PatKind::Range { .. } => {}
        }
    }

    /// Named arguments of a call to a top-level function name its
    /// parameters (`f(width: 3)`).
    fn named_args(&mut self, res: VarRes, args: &[Arg]) {
        let defs = match res {
            VarRes::Global(s) => self.fns.get(&s).cloned().unwrap_or_default(),
            _ => Vec::new(),
        };
        for a in args {
            if let (Some(name), [def]) = (&a.name, defs.as_slice()) {
                let before = self.src[..a.value.span.start as usize].trim_end();
                let Some(before) = before.strip_suffix(':').map(str::trim_end) else { continue };
                if let (Some(p), true) = (def.params.iter().find(|p| &p.name == name && p.pat.is_none()), before.ends_with(&**name)) {
                    let start = (before.len() - name.len()) as u32;
                    let span = Span { start, end: before.len() as u32, file: a.value.span.file };
                    self.push(span, Sym::Local(Rc::as_ptr(def) as usize, p.slot), false, None);
                }
            }
            self.expr(&a.value);
        }
    }

    fn expr(&mut self, e: &Expr) {
        match &e.kind {
            ExprKind::Var(v) => {
                if let Some(s) = self.sym(v.res) {
                    self.push(e.span, s, false, None);
                }
            }
            ExprKind::Call { callee, args } => {
                self.expr(callee);
                let res = match &callee.kind {
                    ExprKind::Var(v) => v.res,
                    _ => VarRes::Unresolved,
                };
                self.named_args(res, args);
            }
            ExprKind::MethodCall { receiver, method, method_span, args, .. } => {
                self.expr(receiver);
                if let Some(s) = self.sym(method.res) {
                    self.push(*method_span, s, false, None);
                }
                self.named_args(method.res, args);
            }
            ExprKind::Record { names, values, spread } => {
                for (name, v) in names.iter().zip(values) {
                    match &v.kind {
                        ExprKind::Var(var) if &var.name == name && self.shorthand(v.span, name) => {
                            if let Some(s) = self.sym(var.res) {
                                self.push(v.span, s, false, Some(name.clone()));
                            }
                        }
                        _ => self.expr(v),
                    }
                }
                if let Some(s) = spread {
                    self.expr(s);
                }
            }
            ExprKind::Lambda(def) => self.function(def, None),
            ExprKind::Match { scrutinee, arms } => {
                self.expr(scrutinee);
                for arm in arms {
                    self.pattern(&arm.pat);
                    if let Some(g) = &arm.guard {
                        self.expr(g);
                    }
                    self.expr(&arm.body);
                }
            }
            ExprKind::For { pat, iter, body } => {
                self.expr(iter);
                self.pattern(pat);
                self.expr(body);
            }
            ExprKind::Is { expr, pat } => {
                self.expr(expr);
                self.pattern(pat);
            }
            ExprKind::Comprehension { body, clauses } => {
                for c in clauses {
                    match c {
                        CompClause::For(pat, x) => {
                            self.expr(x);
                            self.pattern(pat);
                        }
                        CompClause::If(x) => self.expr(x),
                    }
                }
                self.expr(body);
            }
            ExprKind::Block(stmts) => stmts.iter().for_each(|s| self.stmt(s)),
            _ => for_each_child(e, &mut |c| self.expr(c)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index(src: &str) -> Vec<(String, Sym, bool)> {
        let mut it = crate::interp::Interp::new();
        let mut ns = Namespace::default();
        let file = it.ctx.sm.add("t.cog", src);
        let mut prog = crate::parser::parse_program(src, file).unwrap();
        crate::resolver::resolve_program(&mut it.ctx, &mut prog, &mut ns, std::path::Path::new("."), false);
        occurrences(&prog, src).into_iter().map(|o| (src[o.span.start as usize..o.span.end as usize].to_string(), o.sym, o.decl)).collect()
    }

    #[test]
    fn locals_captures_and_globals() {
        let occ = index("let total = 10\nfn main() {\n  var count = 0\n  let add = fn(n) => n + total + count\n  count += add(1)\n}\n");
        let syms = |name: &str| occ.iter().filter(|o| o.0 == name).map(|o| o.1).collect::<Vec<_>>();
        eprintln!("{:?}", occ);
        assert_eq!(syms("count").len(), 3);
        assert!(syms("count").windows(2).all(|w| w[0] == w[1]));
        assert_eq!(syms("total").len(), 2);
    }
}
