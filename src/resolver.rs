//! The resolver walks the AST before anything runs. It:
//!
//! * binds every name to a frame slot, a captured value, or a global slot;
//! * reports undefined names (with "did you mean" suggestions);
//! * enforces immutability of `let` bindings and captured values;
//! * checks `break`/`continue`/`return`/`?` placement;
//! * resolves type annotations and type declarations;
//! * checks call arity against known functions and constructors;
//! * checks that `match` over an enum covers every variant;
//! * loads imported modules;
//! * warns about unused variables.

use crate::ast::*;
use crate::ctx::{Ctx, FnSig, GlobalKind};
use crate::diagnostic::{suggest, Diagnostic};
use crate::parser::parse_program;
use crate::span::Span;
use crate::types::{Name, Ty, TypeDef, TypeKind, VariantDef};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FnKind {
    TopLevel,
    Function,
    Lambda,
    Test,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum LocalKind {
    Let,
    Var,
    Param,
    Fn,
    Result,
}

struct Local {
    name: Name,
    slot: u32,
    mutable: bool,
    used: bool,
    span: Span,
    kind: LocalKind,
}

struct Scope {
    locals: Vec<Local>,
    start_slot: u32,
}

struct Capture {
    name: Name,
    src: CaptureSrc,
    mutable: bool,
    span: Span,
}

struct FnCtx {
    kind: FnKind,
    scopes: Vec<Scope>,
    next_slot: u32,
    max_slot: u32,
    captures: Vec<Capture>,
    self_name: Option<Name>,
    parent_visible: bool,
    loop_depth: u32,
}

impl FnCtx {
    fn new(kind: FnKind, parent_visible: bool, self_name: Option<Name>) -> FnCtx {
        FnCtx {
            kind,
            scopes: vec![Scope { locals: vec![], start_slot: 0 }],
            next_slot: 0,
            max_slot: 0,
            captures: vec![],
            self_name,
            parent_visible,
            loop_depth: 0,
        }
    }
}

struct Found {
    res: VarRes,
    mutable: bool,
    span: Span,
    captured: bool,
    is_fn: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum BindMode {
    Local,
    Global,
}

pub struct Resolver<'a> {
    ctx: &'a mut Ctx,
    ns: Namespace,
    fns: Vec<FnCtx>,
    diags: Vec<Diagnostic>,
    repl: bool,
    dir: PathBuf,
    generics: Vec<Name>,
    pending_methods: Vec<(Name, Span)>,
    /// Bindings of the current or-pattern's first alternative.
    or_bindings: Option<Vec<(Name, VarRes)>>,
    pat_names: Vec<Name>,
}

/// Resolve a program in the given namespace. Returns all diagnostics
/// (errors and warnings).
pub fn resolve_program(ctx: &mut Ctx, prog: &mut Program, ns: &mut Namespace, dir: &Path, repl: bool) -> Vec<Diagnostic> {
    let mut r = Resolver {
        ctx,
        ns: std::mem::take(ns),
        fns: vec![],
        diags: vec![],
        repl,
        dir: dir.to_path_buf(),
        generics: vec![],
        pending_methods: vec![],
        or_bindings: None,
        pat_names: vec![],
    };
    r.program(prog);
    *ns = std::mem::take(&mut r.ns);
    r.diags
}

fn confusion_hint(name: &str) -> Option<&'static str> {
    Some(match name {
        "null" | "nil" | "undefined" | "none" | "NULL" => "Cogito has no null; use `None` (an Option) for a missing value",
        "self" | "this" => "Cogito has no implicit receiver; write ordinary functions whose first parameter is the value, then call them as `value.func()`",
        "True" | "False" | "TRUE" | "FALSE" => "booleans are lowercase: `true` and `false`",
        "println" | "puts" | "printf" | "echo" | "console" | "printLn" | "say" => "use `print(...)`",
        "elif" | "elsif" | "elseif" => "write `else if`",
        "function" | "def" | "func" | "fun" | "proc" => "functions are declared with `fn`",
        "const" | "final" | "val" => "use `let` for a binding that never changes (all `let` bindings are immutable)",
        "lambda" => "anonymous functions are written `fn(x) => x + 1`",
        "length" | "size" => "use `len(x)` or `x.len()`",
        "append" => "use `push` (returns a new list) or `push!` (changes a `var` in place)",
        "switch" | "case" | "when" => "use `match value { pattern => result }`",
        "nan" | "NaN" => "Float division by zero is an error in Cogito, so NaN rarely appears; use `nan()` if you really need one",
        "string" | "String" => "the string type is `Str`; to convert a value use `str(x)`",
        "integer" | "Integer" => "the integer type is `Int`; to convert a value use `int(x)`",
        "new" => "values are built by calling their type: `Point(x: 1, y: 2)`",
        "throw" | "raise" => "errors are values: return `Err(...)`, or call `panic(message)` for bugs",
        "try" | "catch" => "errors are values: use `match` on a Result, or the `?` operator to propagate `Err`",
        _ => return None,
    })
}

impl<'a> Resolver<'a> {
    // ------------------------------------------------------------ utilities

    fn error(&mut self, d: Diagnostic) {
        self.diags.push(d);
    }

    fn line_of(&self, span: Span) -> String {
        if (span.file as usize) < self.ctx.sm.files.len() && span != Span::default() {
            self.ctx.sm.location(span)
        } else {
            "the standard library".into()
        }
    }

    fn cur(&mut self) -> &mut FnCtx {
        self.fns.last_mut().unwrap()
    }

    fn push_scope(&mut self) {
        let f = self.cur();
        let start = f.next_slot;
        f.scopes.push(Scope { locals: vec![], start_slot: start });
    }

    fn pop_scope(&mut self) {
        let f = self.cur();
        let s = f.scopes.pop().unwrap();
        f.next_slot = s.start_slot;
        for l in s.locals {
            if !l.used && !l.name.starts_with('_') && matches!(l.kind, LocalKind::Let | LocalKind::Var | LocalKind::Fn) {
                self.diags.push(
                    Diagnostic::warning("W0001", format!("unused variable `{}`", l.name))
                        .at(l.span)
                        .help(format!("remove it, or rename it to `_{}` if this is intentional", l.name)),
                );
            }
        }
    }

    fn declare_local(&mut self, name: Name, span: Span, mutable: bool, kind: LocalKind) -> u32 {
        let f = self.cur();
        let slot = f.next_slot;
        f.next_slot += 1;
        f.max_slot = f.max_slot.max(f.next_slot);
        f.scopes.last_mut().unwrap().locals.push(Local { name, slot, mutable, used: false, span, kind });
        slot
    }

    fn at_global_scope(&self) -> bool {
        self.fns.len() == 1 && self.fns[0].kind == FnKind::TopLevel && self.fns[0].scopes.len() == 1
    }

    fn lookup_level(&mut self, level: usize, name: &str) -> Option<Found> {
        {
            let f = &mut self.fns[level];
            for scope in f.scopes.iter_mut().rev() {
                for local in scope.locals.iter_mut().rev() {
                    if &*local.name == name {
                        local.used = true;
                        return Some(Found {
                            res: VarRes::Local(local.slot),
                            mutable: local.mutable,
                            span: local.span,
                            captured: false,
                            is_fn: local.kind == LocalKind::Fn,
                        });
                    }
                }
            }
            if f.self_name.as_deref() == Some(name) {
                return Some(Found { res: VarRes::SelfFn, mutable: false, span: Span::default(), captured: false, is_fn: true });
            }
            if let Some(i) = f.captures.iter().position(|c| &*c.name == name) {
                let c = &f.captures[i];
                return Some(Found { res: VarRes::Capture(i as u32), mutable: c.mutable, span: c.span, captured: true, is_fn: false });
            }
            if !f.parent_visible || level == 0 {
                return None;
            }
        }
        let found = self.lookup_level(level - 1, name)?;
        let src = match found.res {
            VarRes::Local(s) => CaptureSrc::Local(s),
            VarRes::Capture(i) => CaptureSrc::Capture(i),
            VarRes::SelfFn => CaptureSrc::SelfFn,
            _ => return None,
        };
        let f = &mut self.fns[level];
        f.captures.push(Capture { name: Rc::from(name), src, mutable: found.mutable, span: found.span });
        Some(Found {
            res: VarRes::Capture((f.captures.len() - 1) as u32),
            mutable: found.mutable,
            span: found.span,
            captured: true,
            is_fn: found.is_fn,
        })
    }

    fn global_slot(&self, name: &str) -> Option<u32> {
        self.ns.values.get(name).or_else(|| self.ctx.builtins.values.get(name)).copied()
    }

    fn lookup(&mut self, name: &str) -> Option<Found> {
        let level = self.fns.len() - 1;
        if let Some(f) = self.lookup_level(level, name) {
            return Some(f);
        }
        let slot = self.global_slot(name)?;
        let info = &self.ctx.globals[slot as usize];
        Some(Found {
            res: VarRes::Global(slot),
            mutable: matches!(info.kind, GlobalKind::Var),
            span: info.span,
            captured: false,
            is_fn: matches!(info.kind, GlobalKind::Fn | GlobalKind::Builtin(_)),
        })
    }

    fn visible_names(&self, upper: bool) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for f in &self.fns {
            for s in &f.scopes {
                for l in &s.locals {
                    out.push(l.name.to_string());
                }
            }
            for c in &f.captures {
                out.push(c.name.to_string());
            }
        }
        out.extend(self.ns.values.keys().map(|k| k.to_string()));
        out.extend(self.ctx.builtins.values.keys().map(|k| k.to_string()));
        out.retain(|n| n.chars().next().map_or(false, |c| c.is_ascii_uppercase()) == upper);
        out.sort();
        out.dedup();
        out
    }

    fn undefined(&mut self, name: &str, span: Span, what: &str) {
        let upper = name.chars().next().map_or(false, |c| c.is_ascii_uppercase());
        let names = self.visible_names(upper);
        let mut d = Diagnostic::error("E0100", format!("undefined {} `{}`", what, name)).at(span).label("not found in this scope");
        if let Some(s) = suggest(name, names.iter().map(|s| s.as_str())) {
            d = d.help(format!("did you mean `{}`?", s));
        } else if let Some(h) = confusion_hint(name) {
            d = d.help(h);
        } else if upper && self.ns.types.contains_key(name) {
            d = d.help(format!("`{}` is a type; build values with one of its constructors", name));
        }
        self.error(d);
    }

    fn resolve_var(&mut self, v: &mut Var, span: Span) -> Option<Found> {
        match self.lookup(&v.name) {
            Some(found) => {
                if let VarRes::Global(slot) = found.res {
                    let info = &self.ctx.globals[slot as usize];
                    if !info.declared && self.fns.last().map_or(false, |f| f.kind == FnKind::TopLevel) {
                        let decl = info.span;
                        let d = Diagnostic::error("E0103", format!("`{}` is used before its declaration", v.name))
                            .at(span)
                            .note(format!("`{}` is declared at {}", v.name, self.line_of(decl)))
                            .help("top-level statements run in order; move the declaration above this line");
                        self.error(d);
                    }
                }
                v.res = found.res;
                Some(found)
            }
            None => {
                let what = if v.name.chars().next().map_or(false, |c| c.is_ascii_uppercase()) { "constructor" } else { "name" };
                let name = v.name.clone();
                self.undefined(&name, span, what);
                None
            }
        }
    }

    fn define_global(&mut self, name: Name, kind: GlobalKind, span: Span, declared: bool) -> u32 {
        if let Some(&old) = self.ns.values.get(&name) {
            if !self.repl {
                let prev = self.ctx.globals[old as usize].span;
                let d = Diagnostic::error("E0102", format!("`{}` is defined more than once", name))
                    .at(span)
                    .label("redefined here")
                    .note(format!("first defined at {}", self.line_of(prev)));
                let d = if matches!(kind, GlobalKind::Let) {
                    d.help("use `var` for a value that changes, or choose a different name")
                } else {
                    d
                };
                self.error(d);
                return old;
            }
        }
        let slot = self.ctx.add_global(name.clone(), kind, span);
        self.ctx.globals[slot as usize].declared = declared;
        self.ns.values.insert(name, slot);
        slot
    }

    fn bound_names(pat: &Pattern, out: &mut Vec<(Name, Span)>) {
        match &pat.kind {
            PatKind::Bind { name, sub, .. } => {
                out.push((name.clone(), pat.span));
                if let Some(s) = sub {
                    Self::bound_names(s, out);
                }
            }
            PatKind::Tuple(ps) => ps.iter().for_each(|p| Self::bound_names(p, out)),
            PatKind::List { before, rest, after } => {
                before.iter().for_each(|p| Self::bound_names(p, out));
                if let Some(Some(r)) = rest {
                    Self::bound_names(r, out);
                }
                after.iter().for_each(|p| Self::bound_names(p, out));
            }
            PatKind::Ctor { args, .. } => args.iter().for_each(|(_, p)| Self::bound_names(p, out)),
            PatKind::Record { fields, .. } => fields.iter().for_each(|(_, p)| Self::bound_names(p, out)),
            PatKind::Or(alts) => {
                if let Some(a) = alts.first() {
                    Self::bound_names(a, out)
                }
            }
            _ => {}
        }
    }

    // ------------------------------------------------------------ program

    fn program(&mut self, prog: &mut Program) {
        // Pass 1: register every top-level name, so that functions can refer
        // to each other (and to types) regardless of order.
        let first_new_type = self.ctx.types.len() as u32;
        let mut type_decl_count = 0u32;
        for item in prog.items.iter_mut() {
            match item {
                Item::Type(td) => {
                    let id = first_new_type + type_decl_count;
                    type_decl_count += 1;
                    td.id = id;
                    if let Some(&old) = self.ns.types.get(&td.name) {
                        if !self.repl {
                            let prev = self.ctx.types.get(old as usize).map(|t| t.span).unwrap_or_default();
                            let d = Diagnostic::error("E0102", format!("type `{}` is defined more than once", td.name))
                                .at(td.name_span)
                                .note(format!("first defined at {}", self.line_of(prev)));
                            self.error(d);
                        }
                    }
                    if self.ctx.builtins.types.contains_key(&td.name) || is_primitive_type(&td.name) {
                        let d = Diagnostic::error("E0102", format!("`{}` is a built-in type and cannot be redefined", td.name)).at(td.name_span);
                        self.error(d);
                    }
                    self.ns.types.insert(td.name.clone(), id);
                    match &mut td.body {
                        TypeBody::Record(_) => {
                            td.slot = self.define_global(
                                td.name.clone(),
                                GlobalKind::Ctor(CtorRef { type_id: id, tag: 0, is_record: true }),
                                td.name_span,
                                true,
                            );
                        }
                        TypeBody::Enum(variants) => {
                            for (tag, v) in variants.iter_mut().enumerate() {
                                if self.ctx.builtins.values.contains_key(&v.name) {
                                    let d = Diagnostic::error("E0102", format!("`{}` is a built-in constructor and cannot be redefined", v.name)).at(v.span);
                                    self.diags.push(d);
                                }
                                v.slot = self.define_global(
                                    v.name.clone(),
                                    GlobalKind::Ctor(CtorRef { type_id: id, tag: tag as u32, is_record: false }),
                                    v.span,
                                    true,
                                );
                            }
                        }
                    }
                }
                Item::Fn(def) => {
                    let def = Rc::get_mut(def).expect("unique fn");
                    let name = def.name.clone().unwrap();
                    let slot = match self.ns.values.get(&name).copied() {
                        Some(s) if matches!(self.ctx.globals[s as usize].kind, GlobalKind::Fn) => s,
                        _ => self.define_global(name.clone(), GlobalKind::Fn, def.name_span, true),
                    };
                    def.global_slot = Some(slot);
                    if let Some(&b) = self.ctx.builtins.values.get(&name) {
                        if matches!(self.ctx.globals[b as usize].kind, GlobalKind::Builtin(_)) {
                            def.overload_fallback = Some(b);
                        }
                    }
                }
                Item::Import(imp) => self.import(imp),
                Item::Stmt(Stmt { kind: StmtKind::Let { pat, .. }, .. }) => {
                    let mut names = vec![];
                    Self::bound_names(pat, &mut names);
                    for (n, sp) in names {
                        self.define_global(n, GlobalKind::Let, sp, false);
                    }
                }
                Item::Stmt(Stmt { kind: StmtKind::Var { name, name_span, .. }, .. }) => {
                    self.define_global(name.clone(), GlobalKind::Var, *name_span, false);
                }
                _ => {}
            }
        }

        // Pass 2: build type definitions.
        let mut new_types = Vec::new();
        for item in prog.items.iter_mut() {
            if let Item::Type(td) = item {
                new_types.push(self.type_decl(td));
            }
        }
        for td in new_types {
            for name in field_names(&td) {
                self.ctx.known_fields.insert(name);
            }
            self.ctx.types.push(Rc::new(td));
        }

        // Pass 3: function signatures.
        let mut seen_sigs: Vec<(u32, FnSig)> = Vec::new();
        for item in prog.items.iter_mut() {
            if let Item::Fn(def) = item {
                let def = Rc::get_mut(def).unwrap();
                self.generics = def.generics.clone();
                for p in def.params.iter_mut() {
                    if let Some(t) = &mut p.ty {
                        self.resolve_type(t, &[]);
                    }
                }
                if let Some(t) = &mut def.ret {
                    self.resolve_type(t, &[]);
                }
                self.generics.clear();
                let sig = FnSig {
                    params: def.params.iter().map(|p| (p.name.clone(), p.default.is_some(), p.ty.as_ref().map(|t| t.ty.clone()))).collect(),
                    span: def.name_span,
                };
                let slot = def.global_slot.unwrap();
                if let Some((_, prev)) = seen_sigs.iter().find(|(s, sg)| *s == slot && sg.same_types(&sig)) {
                    let d = Diagnostic::error("E0102", format!("function `{}` is defined more than once with the same parameter types", def.name.as_ref().unwrap()))
                        .at(def.name_span)
                        .note(format!("first defined at {}", self.line_of(prev.span)))
                        .help("overloads must differ in their parameter type annotations");
                    self.error(d);
                }
                seen_sigs.push((slot, sig.clone()));
                let entry = self.ctx.sigs.entry(slot).or_default();
                if let Some(i) = entry.iter().position(|s| s.same_types(&sig)) {
                    entry[i] = sig;
                } else {
                    entry.push(sig);
                }
            }
        }

        // Pass 4: bodies and top-level statements, in order.
        self.fns.push(FnCtx::new(FnKind::TopLevel, false, None));
        for item in prog.items.iter_mut() {
            match item {
                Item::Fn(def) => {
                    let def = Rc::get_mut(def).unwrap();
                    let name = def.name.clone();
                    self.resolve_fn(def, FnKind::Function, false, name);
                }
                Item::Test(t) => {
                    let def = Rc::get_mut(&mut t.func).unwrap();
                    self.resolve_fn(def, FnKind::Test, false, None);
                }
                Item::Property(p) => {
                    let def = Rc::get_mut(&mut p.func).unwrap();
                    for param in &def.params {
                        if param.ty.is_none() {
                            let d = Diagnostic::error("E0106", format!("property input `{}` needs a type annotation", param.name))
                                .at(param.span)
                                .help(format!("Cogito generates random inputs from the type: write `{}: Int`, `{}: List[Str]`, ...", param.name, param.name));
                            self.diags.push(d);
                        }
                    }
                    self.resolve_fn(def, FnKind::Test, false, None);
                }
                Item::Stmt(s) => self.stmt(s),
                Item::Type(_) | Item::Import(_) => {}
            }
        }
        let top = self.fns.pop().unwrap();
        prog.num_slots = prog.num_slots.max(top.max_slot);

        // Method calls on names that are not functions are only valid if some
        // record has a field with that name.
        let pending = std::mem::take(&mut self.pending_methods);
        for (name, span) in pending {
            if !self.ctx.known_fields.contains(&name) && self.global_slot(&name).is_none() {
                self.undefined(&name, span, "function");
                if let Some(d) = self.diags.last_mut() {
                    d.notes.push(format!("`value.{}(...)` calls the function `{}` with `value` as its first argument", name, name));
                }
            }
        }
    }

    fn import(&mut self, imp: &mut ImportDecl) {
        let path = self.dir.join(&imp.path);
        let path = if path.extension().is_none() { path.with_extension("cog") } else { path };
        let canon = match path.canonicalize() {
            Ok(p) => p,
            Err(e) => {
                let d = Diagnostic::error("E0114", format!("cannot import `{}`: {}", imp.path, e))
                    .at(imp.path_span)
                    .note(format!("looked for {}", path.display()))
                    .help("import paths are relative to the importing file");
                self.error(d);
                return;
            }
        };
        let alias: Name = match &imp.alias {
            Some(a) => a.clone(),
            None => {
                let stem = canon.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                let valid = stem.chars().next().map_or(false, |c| c.is_ascii_lowercase() || c == '_')
                    && stem.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
                if !valid {
                    let d = Diagnostic::error("E0114", format!("the file name `{}` is not a valid module name", stem))
                        .at(imp.span)
                        .help(format!("give it a name: `import \"{}\" as my_module`", imp.path));
                    self.error(d);
                    return;
                }
                Rc::from(stem.as_str())
            }
        };
        let module = if let Some(m) = self.ctx.modules.get(&canon) {
            m.clone()
        } else {
            if self.ctx.loading.contains(&canon) {
                let d = Diagnostic::error("E0114", format!("import cycle: `{}` imports itself (directly or indirectly)", imp.path))
                    .at(imp.path_span)
                    .help("move the shared definitions into a third module that both can import");
                self.error(d);
                return;
            }
            let src = match std::fs::read_to_string(&canon) {
                Ok(s) => s,
                Err(e) => {
                    let d = Diagnostic::error("E0114", format!("cannot read `{}`: {}", imp.path, e)).at(imp.path_span);
                    self.error(d);
                    return;
                }
            };
            let display = canon.display().to_string();
            let file = self.ctx.sm.add(display, src.clone());
            let mut prog = match parse_program(&src, file) {
                Ok(p) => p,
                Err(d) => {
                    self.error(d);
                    self.error(Diagnostic::error("E0114", format!("module `{}` has syntax errors", imp.path)).at(imp.path_span));
                    return;
                }
            };
            self.ctx.loading.push(canon.clone());
            let mut ns = Namespace::default();
            let dir = canon.parent().map(|p| p.to_path_buf()).unwrap_or_default();
            let diags = resolve_program(self.ctx, &mut prog, &mut ns, &dir, false);
            self.ctx.loading.pop();
            let has_errors = diags.iter().any(|d| d.is_error());
            for d in diags {
                if d.is_error() {
                    self.diags.push(d);
                }
            }
            if has_errors {
                self.error(Diagnostic::error("E0114", format!("module `{}` has errors", imp.path)).at(imp.path_span));
                return;
            }
            let m = Rc::new(Module { name: alias.clone(), path: canon.clone(), program: prog, ns, executed: std::cell::Cell::new(false) });
            self.ctx.modules.insert(canon, m.clone());
            m
        };
        imp.slot = self.define_global(alias.clone(), GlobalKind::Module(module.clone()), imp.span, true);
        imp.alias = Some(alias);
        imp.module = Some(module);
    }

    fn type_decl(&mut self, td: &mut TypeDecl) -> TypeDef {
        let params = td.params.clone();
        let kind = match &mut td.body {
            TypeBody::Record(fields) => {
                let mut names: Vec<Name> = Vec::new();
                let mut tys = Vec::new();
                for f in fields.iter_mut() {
                    let n = f.name.clone().unwrap();
                    if names.contains(&n) {
                        let d = Diagnostic::error("E0102", format!("field `{}` is declared twice", n)).at(f.span);
                        self.error(d);
                    }
                    names.push(n);
                    tys.push(self.resolve_type(&mut f.ty, &params));
                }
                TypeKind::Record { fields: names.into(), tys }
            }
            TypeBody::Enum(variants) => {
                let mut defs = Vec::new();
                for v in variants.iter_mut() {
                    let named = v.fields.iter().any(|f| f.name.is_some());
                    let mut names: Vec<Name> = Vec::new();
                    let mut tys = Vec::new();
                    for (i, f) in v.fields.iter_mut().enumerate() {
                        let n = f.name.clone().unwrap_or_else(|| Rc::from(i.to_string().as_str()));
                        if names.contains(&n) {
                            let d = Diagnostic::error("E0102", format!("field `{}` is declared twice", n)).at(f.span);
                            self.error(d);
                        }
                        names.push(n);
                        tys.push(self.resolve_type(&mut f.ty, &params));
                    }
                    if v.has_parens && v.fields.is_empty() {
                        let d = Diagnostic::error("E0010", format!("variant `{}` has empty parentheses", v.name))
                            .at(v.span)
                            .help(format!("write just `{}` for a variant without fields", v.name));
                        self.error(d);
                    }
                    defs.push(VariantDef { name: v.name.clone(), fields: names.into(), tys, named });
                }
                TypeKind::Enum { variants: defs }
            }
        };
        TypeDef { id: td.id, name: td.name.clone(), params, kind, span: td.name_span }
    }

    fn lookup_type(&self, name: &str) -> Option<u32> {
        if let Some((module, tname)) = name.split_once('.') {
            let slot = self.ns.values.get(module)?;
            if let GlobalKind::Module(m) = &self.ctx.globals[*slot as usize].kind {
                return m.ns.types.get(tname).copied();
            }
            return None;
        }
        self.ns.types.get(name).or_else(|| self.ctx.builtins.types.get(name)).copied()
    }

    fn resolve_type(&mut self, te: &mut TypeExpr, type_params: &[Name]) -> Ty {
        let ty = match &mut te.kind {
            TypeExprKind::Unit => Ty::Unit,
            TypeExprKind::Tuple(items) => Ty::Tuple(items.iter_mut().map(|t| self.resolve_type(t, type_params)).collect()),
            TypeExprKind::Record(fields) => {
                let mut out = Vec::new();
                for (n, t) in fields.iter_mut() {
                    out.push((n.clone(), self.resolve_type(t, type_params)));
                }
                Ty::Record(out)
            }
            TypeExprKind::Fn(params, ret) => {
                let ps = params.iter_mut().map(|t| self.resolve_type(t, type_params)).collect();
                let r = self.resolve_type(ret, type_params);
                Ty::Fn(ps, Box::new(r))
            }
            TypeExprKind::Named(name, args) => {
                let name = name.clone();
                let mut targs: Vec<Ty> = args.iter_mut().map(|t| self.resolve_type(t, type_params)).collect();
                let span = te.span;
                let arity_err = |me: &mut Self, want: usize| {
                    let d = Diagnostic::error("E0106", format!("`{}` takes {} type argument{}, but {} were given", name, want, if want == 1 { "" } else { "s" }, targs.len()))
                        .at(span);
                    me.error(d);
                };
                match &*name {
                    "Int" | "Float" | "Str" | "Bool" | "Unit" | "Any" | "Range" => {
                        if !targs.is_empty() {
                            arity_err(self, 0);
                        }
                        match &*name {
                            "Int" => Ty::Int,
                            "Float" => Ty::Float,
                            "Str" => Ty::Str,
                            "Bool" => Ty::Bool,
                            "Unit" => Ty::Unit,
                            "Range" => Ty::Range,
                            _ => Ty::Any,
                        }
                    }
                    "List" => {
                        if targs.len() > 1 {
                            arity_err(self, 1);
                        }
                        Ty::List(Box::new(targs.pop().unwrap_or(Ty::Any)))
                    }
                    "Map" => {
                        if targs.len() == 1 || targs.len() > 2 {
                            arity_err(self, 2);
                        }
                        let v = targs.pop().unwrap_or(Ty::Any);
                        let k = targs.pop().unwrap_or(Ty::Any);
                        Ty::Map(Box::new(k), Box::new(v))
                    }
                    "Fn" => Ty::Fn(vec![], Box::new(Ty::Any)),
                    _ => {
                        if let Some(i) = type_params.iter().position(|p| *p == name) {
                            Ty::Param(i as u32, name.clone())
                        } else if self.generics.contains(&name) {
                            Ty::Generic(name.clone())
                        } else if let Some(id) = self.lookup_type(&name) {
                            let nparams = if (id as usize) < self.ctx.types.len() {
                                self.ctx.types[id as usize].params.len()
                            } else {
                                // a type declared in this program (not built yet)
                                usize::MAX
                            };
                            if nparams != usize::MAX && !targs.is_empty() && targs.len() != nparams {
                                arity_err(self, nparams);
                            }
                            let short: Name = Rc::from(name.rsplit('.').next().unwrap());
                            Ty::Named { id, name: short, args: targs }
                        } else {
                            let mut cands: Vec<String> = vec!["Int", "Float", "Str", "Bool", "Unit", "Any", "List", "Map", "Range"]
                                .into_iter()
                                .map(String::from)
                                .collect();
                            cands.extend(self.ns.types.keys().map(|k| k.to_string()));
                            cands.extend(self.ctx.builtins.types.keys().map(|k| k.to_string()));
                            cands.extend(self.generics.iter().map(|k| k.to_string()));
                            let mut d = Diagnostic::error("E0106", format!("unknown type `{}`", name)).at(span);
                            if let Some(s) = suggest(&name, cands.iter().map(|s| s.as_str())) {
                                d = d.help(format!("did you mean `{}`?", s));
                            } else if name.len() == 1 {
                                d = d.help(format!("to use `{}` as a type parameter, declare it: `fn name[{}](...)`", name, name));
                            } else if let Some(h) = confusion_hint(&name) {
                                d = d.help(h);
                            }
                            self.error(d);
                            Ty::Any
                        }
                    }
                }
            }
        };
        te.ty = ty.clone();
        ty
    }

    // ------------------------------------------------------------ functions

    fn resolve_fn(&mut self, def: &mut FnDef, kind: FnKind, parent_visible: bool, self_name: Option<Name>) {
        let sig_done = kind == FnKind::Function && !parent_visible;
        let saved_generics = self.generics.clone();
        self.generics.extend(def.generics.iter().cloned());
        self.fns.push(FnCtx::new(kind, parent_visible, self_name));
        let mut seen: Vec<Name> = Vec::new();
        let mutating = def.mutating;
        if mutating && def.params.is_empty() {
            let d = Diagnostic::error("E0111", format!("mutating function `{}` must take the value it changes as its first parameter", def.display_name()))
                .at(def.name_span)
                .help(format!("write `fn {}(xs: List[Int], ...)`", def.display_name()));
            self.error(d);
        }
        for (i, p) in def.params.iter_mut().enumerate() {
            if seen.contains(&p.name) {
                let d = Diagnostic::error("E0102", format!("parameter `{}` is declared twice", p.name)).at(p.span);
                self.error(d);
            }
            seen.push(p.name.clone());
            if let Some(t) = &mut p.ty {
                if !sig_done {
                    self.resolve_type(t, &[]);
                }
            }
            if let Some(d) = &mut p.default {
                self.expr(d);
            }
            p.slot = self.declare_local(p.name.clone(), p.span, mutating && i == 0, LocalKind::Param);
        }
        if let Some(t) = &mut def.ret {
            if !sig_done {
                self.resolve_type(t, &[]);
            }
        }
        for r in def.requires.iter_mut() {
            self.expr(r);
        }
        self.expr(&mut def.body);
        if !def.ensures.is_empty() {
            self.push_scope();
            def.result_slot = self.declare_local(Rc::from("result"), def.name_span, false, LocalKind::Result);
            for e in def.ensures.iter_mut() {
                self.expr(e);
            }
            self.pop_scope();
        }
        let f = self.fns.pop().unwrap();
        def.num_slots = f.max_slot;
        def.captures = f.captures.iter().map(|c| c.src).collect();
        self.generics = saved_generics;
    }

    // ------------------------------------------------------------ statements

    fn stmt(&mut self, s: &mut Stmt) {
        match &mut s.kind {
            StmtKind::Let { pat, ty, value } => {
                self.expr(value);
                if let Some(t) = ty {
                    self.resolve_type(t, &[]);
                }
                let mode = if self.at_global_scope() { BindMode::Global } else { BindMode::Local };
                self.pattern(pat, mode);
            }
            StmtKind::Var { name, name_span, res, ty, value } => {
                self.expr(value);
                if let Some(t) = ty {
                    self.resolve_type(t, &[]);
                }
                if self.at_global_scope() {
                    let slot = self.ns.values[name];
                    self.ctx.globals[slot as usize].declared = true;
                    *res = VarRes::Global(slot);
                } else {
                    *res = VarRes::Local(self.declare_local(name.clone(), *name_span, true, LocalKind::Var));
                }
            }
            StmtKind::Assign { target, op: _, value } => {
                self.expr(value);
                self.place(target, "E0101");
            }
            StmtKind::Fn { def, res } => {
                let def = Rc::get_mut(def).unwrap();
                let name = def.name.clone().unwrap();
                if def.mutating {
                    let d = Diagnostic::error("E0116", "mutating functions (ending in `!`) must be declared at the top level").at(def.name_span);
                    self.error(d);
                }
                self.resolve_fn(def, FnKind::Function, true, Some(name.clone()));
                *res = VarRes::Local(self.declare_local(name, def.name_span, false, LocalKind::Fn));
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

    /// Resolve an assignment target (or the receiver of a mutating call) and
    /// check that its root is mutable.
    fn place(&mut self, e: &mut Expr, code: &'static str) {
        let span = e.span;
        match &mut e.kind {
            ExprKind::Var(v) => {
                let name = v.name.clone();
                let Some(found) = self.resolve_var(v, span) else { return };
                if found.mutable {
                    return;
                }
                let mut d;
                if found.captured {
                    d = Diagnostic::error("E0110", format!("cannot change `{}` inside a closure", name))
                        .at(span)
                        .label("captured variable")
                        .note("closures capture a snapshot of the values they use, not the variables themselves")
                        .help("return the new value from the closure instead, or use a loop");
                } else if let VarRes::Global(slot) = found.res {
                    let kind = self.ctx.globals[slot as usize].kind.clone();
                    d = Diagnostic::error(code, format!("cannot change `{}`", name)).at(span);
                    d = match kind {
                        GlobalKind::Let => d.label("immutable binding").help(format!("declare it with `var {} = ...` to allow changes", name)),
                        GlobalKind::Fn | GlobalKind::Builtin(_) => d.label("this is a function"),
                        GlobalKind::Ctor(_) => d.label("this is a constructor"),
                        GlobalKind::Module(_) => d.label("this is a module"),
                        _ => d.label("this is a constant"),
                    };
                } else if found.res == VarRes::SelfFn || found.is_fn {
                    d = Diagnostic::error(code, format!("cannot change function `{}`", name)).at(span);
                } else {
                    d = Diagnostic::error(code, format!("cannot change `{}`, because it was declared with `let`", name))
                        .at(span)
                        .label("immutable binding");
                    if found.span != Span::default() {
                        d = d.note(format!("`{}` is declared at {}", name, self.line_of(found.span)));
                    }
                    d = d.help(format!("declare it with `var {}` to allow changes", name));
                }
                if code == "E0111" {
                    d.message = format!("cannot call a mutating function on `{}`, because it is not a `var`", name);
                }
                self.error(d);
            }
            ExprKind::Field { target, .. } => self.place(target, code),
            ExprKind::Index { target, index } => {
                self.expr(index);
                self.place(target, code);
            }
            _ => {
                self.expr(e);
                let d = Diagnostic::error("E0111", "mutating functions need a variable to change")
                    .at(span)
                    .label("this is a temporary value")
                    .help("store the value in a `var` first, or use the non-mutating version (without `!`), which returns a new value");
                self.error(d);
            }
        }
    }

    // ------------------------------------------------------------ expressions

    fn block(&mut self, stmts: &mut [Stmt]) {
        self.push_scope();
        let n = stmts.len();
        for (i, s) in stmts.iter_mut().enumerate() {
            self.stmt(s);
            if i + 1 < n {
                if let StmtKind::Expr(Expr { kind: ExprKind::Return(_) | ExprKind::Break(_) | ExprKind::Continue, span }) = &s.kind {
                    let d = Diagnostic::warning("W0002", "unreachable code").at(*span).help("the statements after this line never run");
                    self.diags.push(d);
                }
            }
        }
        self.pop_scope();
    }

    fn module_member(&self, target: &Expr, member: &str) -> Option<Result<u32, (Rc<Module>, Name)>> {
        if let ExprKind::Var(v) = &target.kind {
            // A local or captured variable shadows a module of the same name.
            for f in self.fns.iter() {
                for s in &f.scopes {
                    if s.locals.iter().any(|l| l.name == v.name) {
                        return None;
                    }
                }
            }
            let slot = self.ns.values.get(&v.name)?;
            if let GlobalKind::Module(m) = &self.ctx.globals[*slot as usize].kind {
                return Some(match m.ns.values.get(member) {
                    Some(s) => Ok(*s),
                    None => Err((m.clone(), v.name.clone())),
                });
            }
        }
        None
    }

    fn unknown_member(&mut self, m: &Module, alias: &str, member: &str, span: Span) {
        let names: Vec<String> = m.ns.values.keys().map(|k| k.to_string()).collect();
        let mut d = Diagnostic::error("E0100", format!("module `{}` has no member `{}`", alias, member)).at(span);
        if let Some(s) = suggest(member, names.iter().map(|s| s.as_str())) {
            d = d.help(format!("did you mean `{}.{}`?", alias, s));
        }
        self.error(d);
    }

    fn args(&mut self, args: &mut [Arg]) {
        for a in args.iter_mut() {
            self.expr(&mut a.value);
        }
    }

    fn check_call(&mut self, slot: u32, extra: usize, args: &[Arg], span: Span) {
        let positional = args.iter().filter(|a| a.name.is_none()).count() + extra;
        let named: Vec<&Name> = args.iter().filter_map(|a| a.name.as_ref()).collect();
        let info = self.ctx.globals[slot as usize].clone();
        match &info.kind {
            GlobalKind::Builtin(idx) => {
                let b = &crate::builtins::BUILTINS[*idx as usize];
                if !named.is_empty() {
                    let d = Diagnostic::error("E0108", format!("built-in function `{}` does not take named arguments", b.name)).at(span);
                    self.error(d);
                    return;
                }
                if positional < b.min as usize || positional > b.max as usize {
                    let expect = if b.min == b.max {
                        format!("{}", b.min)
                    } else if b.max as usize >= crate::builtins::VARIADIC as usize {
                        format!("at least {}", b.min)
                    } else {
                        format!("{} to {}", b.min, b.max)
                    };
                    let d = Diagnostic::error("E0107", format!("`{}` takes {} argument{}, but {} were given", b.name, expect, if expect == "1" { "" } else { "s" }, positional))
                        .at(span)
                        .note(format!("usage: {}", b.doc.lines().next().unwrap_or("")));
                    self.error(d);
                }
            }
            GlobalKind::Fn => {
                let Some(sigs) = self.ctx.sigs.get(&slot) else { return };
                if sigs.len() != 1 {
                    return;
                }
                // A user function that shares a built-in's name may fall back to it.
                if self.ctx.builtins.values.contains_key(&info.name) {
                    return;
                }
                let sig = sigs[0].clone();
                let params: Vec<(String, bool)> = sig.params.iter().map(|(n, d, _)| (n.to_string(), *d)).collect();
                self.check_against(&info.name, "function", &params, positional, &named, span, sig.span);
            }
            GlobalKind::Ctor(c) => {
                let td = self.ctx.type_by_id(c.type_id).clone();
                let (fields, _, field_named) = td.fields_of(c.tag);
                if fields.is_empty() && !c.is_record {
                    let d = Diagnostic::error("E0202", format!("`{}` has no fields and is not called", info.name))
                        .at(span)
                        .help(format!("write just `{}`", info.name));
                    self.error(d);
                    return;
                }
                if !field_named && !named.is_empty() {
                    let d = Diagnostic::error("E0108", format!("`{}` has positional fields; it cannot be called with named arguments", info.name)).at(span);
                    self.error(d);
                    return;
                }
                let params: Vec<(String, bool)> = fields.iter().map(|n| (n.to_string(), false)).collect();
                self.check_against(&info.name, "constructor", &params, positional, &named, span, td.span);
            }
            _ => {}
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn check_against(&mut self, name: &str, what: &str, params: &[(String, bool)], positional: usize, named: &[&Name], span: Span, def_span: Span) {
        let total = params.len();
        let required = params.iter().filter(|p| !p.1).count();
        if positional > total {
            let d = Diagnostic::error("E0107", format!("{} `{}` takes {} argument{}, but {} were given", what, name, total, if total == 1 { "" } else { "s" }, positional))
                .at(span)
                .note(format!("`{}` is defined at {}", name, self.line_of(def_span)));
            self.error(d);
            return;
        }
        let mut filled: Vec<bool> = (0..total).map(|i| i < positional).collect();
        for n in named {
            match params.iter().position(|p| p.0 == ***n) {
                Some(i) => {
                    if filled[i] {
                        let d = Diagnostic::error("E0108", format!("argument `{}` is given twice", n)).at(span);
                        self.error(d);
                        return;
                    }
                    filled[i] = true;
                }
                None => {
                    let mut d = Diagnostic::error("E0108", format!("{} `{}` has no parameter named `{}`", what, name, n)).at(span);
                    if let Some(s) = suggest(n, params.iter().map(|p| p.0.as_str())) {
                        d = d.help(format!("did you mean `{}`?", s));
                    } else {
                        let list: Vec<&str> = params.iter().map(|p| p.0.as_str()).collect();
                        d = d.note(format!("parameters: {}", list.join(", ")));
                    }
                    self.error(d);
                    return;
                }
            }
        }
        let missing: Vec<&str> = params.iter().zip(&filled).filter(|(p, f)| !p.1 && !**f).map(|(p, _)| p.0.as_str()).collect();
        if !missing.is_empty() {
            let given = positional + named.len();
            let d = Diagnostic::error(
                "E0107",
                format!(
                    "{} `{}` takes {}{} argument{}, but {} {} given",
                    what,
                    name,
                    if required < total { "at least " } else { "" },
                    required,
                    if required == 1 { "" } else { "s" },
                    given,
                    if given == 1 { "was" } else { "were" }
                ),
            )
            .at(span)
            .label(format!("missing: {}", missing.iter().map(|m| format!("`{}`", m)).collect::<Vec<_>>().join(", ")))
            .note(format!("`{}` is defined at {}", name, self.line_of(def_span)));
            self.error(d);
        }
    }

    fn expr(&mut self, e: &mut Expr) {
        let span = e.span;
        // Module member access is rewritten into a direct global reference.
        let mut replacement: Option<ExprKind> = None;
        match &mut e.kind {
            ExprKind::Field { target, name, name_span } => {
                match self.module_member(target, name) {
                    Some(Ok(slot)) => replacement = Some(ExprKind::Var(Var { name: name.clone(), res: VarRes::Global(slot) })),
                    Some(Err((m, alias))) => {
                        let (n, s) = (name.clone(), *name_span);
                        self.unknown_member(&m, &alias, &n, s);
                        return;
                    }
                    None => {}
                }
            }
            ExprKind::MethodCall { receiver, method, method_span, args, mutating } => match self.module_member(receiver, &method.name) {
                Some(Ok(slot)) => {
                    let callee = Expr { kind: ExprKind::Var(Var { name: method.name.clone(), res: VarRes::Global(slot) }), span: *method_span };
                    let args = std::mem::take(args);
                    if *mutating {
                        let mut args = args;
                        if args.is_empty() {
                            let d = Diagnostic::error("E0111", format!("`{}` needs the value to change as its first argument", method.name)).at(span);
                            self.error(d);
                            return;
                        }
                        let recv = args.remove(0).value;
                        replacement = Some(ExprKind::MethodCall {
                            receiver: Box::new(recv),
                            method: Var { name: method.name.clone(), res: VarRes::Global(slot) },
                            method_span: *method_span,
                            args,
                            mutating: true,
                        });
                    } else {
                        replacement = Some(ExprKind::Call { callee: Box::new(callee), args });
                    }
                }
                Some(Err((m, alias))) => {
                    let (n, s) = (method.name.clone(), *method_span);
                    self.unknown_member(&m, &alias, &n, s);
                    return;
                }
                None => {}
            },
            _ => {}
        }
        if let Some(k) = replacement {
            e.kind = k;
            // Resolve the rewritten node (its children still need resolving).
            match &mut e.kind {
                ExprKind::Var(_) => return,
                ExprKind::Call { callee, args } => {
                    self.args(args);
                    if let ExprKind::Var(Var { res: VarRes::Global(slot), .. }) = callee.kind {
                        self.check_call(slot, 0, args, span);
                    }
                    return;
                }
                ExprKind::MethodCall { receiver, args, .. } => {
                    self.args(args);
                    self.place(receiver, "E0111");
                    return;
                }
                _ => {}
            }
        }

        match &mut e.kind {
            ExprKind::Unit | ExprKind::Bool(_) | ExprKind::Int(_) | ExprKind::Float(_) | ExprKind::Str(_) => {}
            ExprKind::Interp(parts) => {
                for p in parts.iter_mut() {
                    if let InterpPart::Expr(x, _) = p {
                        self.expr(x);
                    }
                }
            }
            ExprKind::Var(v) => {
                if v.name.ends_with('!') {
                    let d = Diagnostic::error("E0111", format!("mutating function `{}` can only be called, not used as a value", v.name)).at(span);
                    self.error(d);
                    return;
                }
                self.resolve_var(v, span);
            }
            ExprKind::List(items) => {
                for it in items.iter_mut() {
                    self.expr(&mut it.expr);
                }
            }
            ExprKind::Comprehension { body, clauses } => {
                self.push_scope();
                for c in clauses.iter_mut() {
                    match c {
                        CompClause::For(pat, iter) => {
                            self.expr(iter);
                            self.pattern(pat, BindMode::Local);
                        }
                        CompClause::If(cond) => self.expr(cond),
                    }
                }
                self.expr(body);
                self.pop_scope();
            }
            ExprKind::Map(entries) => {
                for (k, v) in entries.iter_mut() {
                    self.expr(k);
                    self.expr(v);
                }
            }
            ExprKind::Tuple(items) => items.iter_mut().for_each(|x| self.expr(x)),
            ExprKind::Record { names, values, spread } => {
                for n in names.iter() {
                    self.ctx.known_fields.insert(n.clone());
                }
                for v in values.iter_mut() {
                    self.expr(v);
                }
                if let Some(s) = spread {
                    self.expr(s);
                }
            }
            ExprKind::Field { target, .. } => self.expr(target),
            ExprKind::Index { target, index } => {
                self.expr(target);
                self.expr(index);
            }
            ExprKind::Call { callee, args } => {
                self.expr(callee);
                self.args(args);
                if let ExprKind::Var(Var { res: VarRes::Global(slot), .. }) = callee.kind {
                    self.check_call(slot, 0, args, span);
                }
            }
            ExprKind::MethodCall { receiver, method, method_span, args, mutating } => {
                self.args(args);
                if *mutating {
                    self.place(receiver, "E0111");
                    match self.lookup(&method.name) {
                        Some(f) => {
                            method.res = f.res;
                            if let VarRes::Global(slot) = f.res {
                                self.check_call(slot, 1, args, span);
                            }
                        }
                        None => {
                            let n = method.name.clone();
                            self.undefined(&n, *method_span, "function");
                        }
                    }
                } else {
                    self.expr(receiver);
                    match self.lookup(&method.name) {
                        Some(f) => {
                            method.res = f.res;
                            if let VarRes::Global(slot) = f.res {
                                if !self.ctx.known_fields.contains(&method.name) {
                                    self.check_call(slot, 1, args, span);
                                }
                            }
                        }
                        None => self.pending_methods.push((method.name.clone(), *method_span)),
                    }
                }
            }
            ExprKind::Unary { expr, .. } => self.expr(expr),
            ExprKind::Binary { lhs, rhs, .. } | ExprKind::And(lhs, rhs) | ExprKind::Or(lhs, rhs) => {
                self.expr(lhs);
                self.expr(rhs);
            }
            ExprKind::Range { start, end, .. } => {
                self.expr(start);
                if let Some(e) = end {
                    self.expr(e);
                }
            }
            ExprKind::Try(inner) => {
                self.expr(inner);
                if self.cur().kind == FnKind::TopLevel {
                    let d = Diagnostic::error("E0115", "`?` can only be used inside a function")
                        .at(span)
                        .help("at the top level, use `match` or `.unwrap()` to get the value out");
                    self.error(d);
                }
            }
            ExprKind::If { cond, then, els } => {
                self.expr(cond);
                self.expr(then);
                if let Some(e) = els {
                    self.expr(e);
                }
            }
            ExprKind::Match { scrutinee, arms } => {
                self.expr(scrutinee);
                for arm in arms.iter_mut() {
                    self.push_scope();
                    self.pattern(&mut arm.pat, BindMode::Local);
                    if let Some(g) = &mut arm.guard {
                        self.expr(g);
                    }
                    self.expr(&mut arm.body);
                    self.pop_scope();
                }
                self.check_exhaustive(arms, span);
            }
            ExprKind::Block(stmts) => self.block(stmts),
            ExprKind::Lambda(def) => {
                let def = Rc::get_mut(def).unwrap();
                self.resolve_fn(def, FnKind::Lambda, true, None);
            }
            ExprKind::While { cond, body } => {
                self.expr(cond);
                self.cur().loop_depth += 1;
                self.expr(body);
                self.cur().loop_depth -= 1;
            }
            ExprKind::For { pat, iter, body } => {
                self.expr(iter);
                self.push_scope();
                self.pattern(pat, BindMode::Local);
                self.cur().loop_depth += 1;
                self.expr(body);
                self.cur().loop_depth -= 1;
                self.pop_scope();
            }
            ExprKind::Loop { body } => {
                self.cur().loop_depth += 1;
                self.expr(body);
                self.cur().loop_depth -= 1;
            }
            ExprKind::Break(v) => {
                if let Some(v) = v {
                    self.expr(v);
                }
                if self.cur().loop_depth == 0 {
                    let d = Diagnostic::error("E0104", "`break` outside of a loop").at(span);
                    self.error(d);
                }
            }
            ExprKind::Continue => {
                if self.cur().loop_depth == 0 {
                    let d = Diagnostic::error("E0104", "`continue` outside of a loop").at(span);
                    self.error(d);
                }
            }
            ExprKind::Return(v) => {
                if let Some(v) = v {
                    self.expr(v);
                }
                if self.cur().kind == FnKind::TopLevel {
                    let d = Diagnostic::error("E0105", "`return` outside of a function")
                        .at(span)
                        .help("use `exit(code)` to stop a script early");
                    self.error(d);
                }
            }
        }
    }

    // ------------------------------------------------------------ patterns

    fn bind(&mut self, name: &Name, span: Span, mode: BindMode) -> VarRes {
        if self.pat_names.contains(name) {
            let d = Diagnostic::error("E0102", format!("`{}` is bound more than once in this pattern", name)).at(span);
            self.error(d);
        }
        self.pat_names.push(name.clone());
        if let Some(first) = &self.or_bindings {
            return match first.iter().find(|(n, _)| n == name) {
                Some((_, r)) => *r,
                None => {
                    let d = Diagnostic::error("E0010", format!("`{}` is not bound in the first alternative of this pattern", name))
                        .at(span)
                        .help("every alternative of an `|` pattern must bind the same names");
                    self.error(d);
                    VarRes::Unresolved
                }
            };
        }
        match mode {
            BindMode::Global => {
                let slot = self.ns.values[name];
                self.ctx.globals[slot as usize].declared = true;
                VarRes::Global(slot)
            }
            BindMode::Local => VarRes::Local(self.declare_local(name.clone(), span, false, LocalKind::Let)),
        }
    }

    fn pattern(&mut self, p: &mut Pattern, mode: BindMode) {
        self.pat_names.clear();
        self.pattern_inner(p, mode);
        self.pat_names.clear();
    }

    fn pattern_inner(&mut self, p: &mut Pattern, mode: BindMode) {
        let span = p.span;
        match &mut p.kind {
            PatKind::Wild | PatKind::Lit(_) => {}
            PatKind::Range { lo, hi, .. } => {
                let ok = matches!((&*lo, &*hi), (Lit::Int(_), Lit::Int(_)) | (Lit::Float(_), Lit::Float(_)) | (Lit::Str(_), Lit::Str(_)));
                if !ok {
                    let d = Diagnostic::error("E0010", "both ends of a range pattern must be numbers (or both strings)").at(span);
                    self.error(d);
                }
            }
            PatKind::Bind { name, res, sub } => {
                *res = self.bind(&name.clone(), span, mode);
                if let Some(s) = sub {
                    self.pattern_inner(s, mode);
                }
            }
            PatKind::Tuple(items) => items.iter_mut().for_each(|x| self.pattern_inner(x, mode)),
            PatKind::List { before, rest, after } => {
                before.iter_mut().for_each(|x| self.pattern_inner(x, mode));
                if let Some(Some(r)) = rest {
                    self.pattern_inner(r, mode);
                }
                after.iter_mut().for_each(|x| self.pattern_inner(x, mode));
            }
            PatKind::Record { fields, .. } => {
                for (n, fp) in fields.iter_mut() {
                    self.ctx.known_fields.insert(n.clone());
                    self.pattern_inner(fp, mode);
                }
            }
            PatKind::Or(alts) => {
                let outer = self.or_bindings.take();
                let names_before = self.pat_names.clone();
                let mut first_bindings: Vec<(Name, VarRes)> = Vec::new();
                for (i, alt) in alts.iter_mut().enumerate() {
                    self.pat_names = names_before.clone();
                    if i == 0 {
                        self.or_bindings = outer.clone();
                        self.pattern_inner(alt, mode);
                        let mut names = vec![];
                        Self::bound_names(alt, &mut names);
                        first_bindings = names
                            .iter()
                            .map(|(n, _)| (n.clone(), self.lookup_binding(n, mode)))
                            .collect();
                    } else {
                        self.or_bindings = Some(first_bindings.clone());
                        self.pattern_inner(alt, mode);
                        let mut names = vec![];
                        Self::bound_names(alt, &mut names);
                        for (n, _) in &first_bindings {
                            if !names.iter().any(|(m, _)| m == n) {
                                let d = Diagnostic::error("E0010", format!("`{}` is not bound in every alternative of this pattern", n))
                                    .at(alt.span)
                                    .help("every alternative of an `|` pattern must bind the same names");
                                self.error(d);
                            }
                        }
                    }
                }
                self.or_bindings = outer;
            }
            PatKind::Ctor { name, args, rest, ctor, field_idx } => {
                let Some(slot) = self.global_slot(name) else {
                    let n = name.clone();
                    self.undefined(&n, span, "constructor");
                    for (_, a) in args.iter_mut() {
                        self.pattern_inner(a, mode);
                    }
                    return;
                };
                let GlobalKind::Ctor(c) = self.ctx.globals[slot as usize].kind.clone() else {
                    let d = Diagnostic::error("E0100", format!("`{}` is not a constructor", name)).at(span);
                    self.error(d);
                    return;
                };
                *ctor = c;
                let td = self.ctx.type_by_id(c.type_id).clone();
                let (fields, _, named) = td.fields_of(c.tag);
                let positional = args.iter().filter(|(n, _)| n.is_none()).count();
                if positional > fields.len() || (!*rest && args.len() != fields.len()) {
                    let d = Diagnostic::error(
                        "E0112",
                        format!("`{}` has {} field{}, but the pattern lists {}", name, fields.len(), if fields.len() == 1 { "" } else { "s" }, args.len()),
                    )
                    .at(span)
                    .help(if fields.is_empty() {
                        format!("write just `{}`", name)
                    } else {
                        format!("add `..` to ignore the remaining fields: `{}(.., ..)`", name).replace("(.., ..)", "(..)")
                    });
                    self.error(d);
                }
                field_idx.clear();
                let mut used = HashSet::new();
                for (i, (n, _)) in args.iter().enumerate() {
                    let idx = match n {
                        None => i as u32,
                        Some(fname) => {
                            if !named {
                                let d = Diagnostic::error("E0108", format!("`{}` has positional fields; match them by position", name)).at(span);
                                self.error(d);
                                i as u32
                            } else {
                                match fields.iter().position(|f| f == fname) {
                                    Some(j) => j as u32,
                                    None => {
                                        let mut d = Diagnostic::error("E0108", format!("`{}` has no field `{}`", name, fname)).at(span);
                                        if let Some(s) = suggest(fname, fields.iter().map(|f| &**f)) {
                                            d = d.help(format!("did you mean `{}`?", s));
                                        }
                                        self.error(d);
                                        i as u32
                                    }
                                }
                            }
                        }
                    };
                    if !used.insert(idx) {
                        let d = Diagnostic::error("E0102", "the same field is matched twice in this pattern").at(span);
                        self.error(d);
                    }
                    field_idx.push(idx);
                }
                for (_, a) in args.iter_mut() {
                    self.pattern_inner(a, mode);
                }
            }
        }
    }

    fn lookup_binding(&self, name: &Name, mode: BindMode) -> VarRes {
        match mode {
            BindMode::Global => self.ns.values.get(name).map(|s| VarRes::Global(*s)).unwrap_or(VarRes::Unresolved),
            BindMode::Local => {
                let f = self.fns.last().unwrap();
                for s in f.scopes.iter().rev() {
                    for l in s.locals.iter().rev() {
                        if &l.name == name {
                            return VarRes::Local(l.slot);
                        }
                    }
                }
                VarRes::Unresolved
            }
        }
    }

    fn check_exhaustive(&mut self, arms: &[Arm], span: Span) {
        fn flatten<'p>(p: &'p Pattern, out: &mut Vec<&'p Pattern>) {
            match &p.kind {
                PatKind::Or(alts) => alts.iter().for_each(|a| flatten(a, out)),
                PatKind::Bind { sub: Some(s), .. } => flatten(s, out),
                _ => out.push(p),
            }
        }
        let mut enum_id: Option<u32> = None;
        let mut covered: HashSet<u32> = HashSet::new();
        let mut bools: HashSet<bool> = HashSet::new();
        let mut kind_bool = false;
        for arm in arms {
            if arm.guard.is_none() && arm.pat.is_irrefutable() {
                return;
            }
            let mut pats = vec![];
            flatten(&arm.pat, &mut pats);
            for p in pats {
                match &p.kind {
                    PatKind::Ctor { ctor, args, .. } if !ctor.is_record => {
                        if enum_id.is_some_and(|id| id != ctor.type_id) {
                            return;
                        }
                        enum_id = Some(ctor.type_id);
                        if arm.guard.is_none() && args.iter().all(|(_, a)| a.is_irrefutable()) {
                            covered.insert(ctor.tag);
                        }
                    }
                    PatKind::Lit(Lit::Bool(b)) => {
                        kind_bool = true;
                        if arm.guard.is_none() {
                            bools.insert(*b);
                        }
                    }
                    _ => return,
                }
            }
        }
        if kind_bool && enum_id.is_none() {
            if bools.len() < 2 {
                let missing = if bools.contains(&true) { "false" } else { "true" };
                let d = Diagnostic::error("E0109", format!("non-exhaustive match: `{}` is not handled", missing))
                    .at(span)
                    .help(format!("add an arm `{} => ...`", missing));
                self.error(d);
            }
            return;
        }
        let Some(id) = enum_id else { return };
        let td = self.ctx.type_by_id(id).clone();
        let TypeKind::Enum { variants } = &td.kind else { return };
        let missing: Vec<String> = variants
            .iter()
            .enumerate()
            .filter(|(i, _)| !covered.contains(&(*i as u32)))
            .map(|(_, v)| if v.fields.is_empty() { v.name.to_string() } else { format!("{}(..)", v.name) })
            .collect();
        if !missing.is_empty() {
            let list = missing.iter().map(|m| format!("`{}`", m)).collect::<Vec<_>>().join(", ");
            let d = Diagnostic::error("E0109", format!("non-exhaustive match on `{}`: {} not handled", td.name, list))
                .at(span)
                .help(format!("add {} for {}, or a catch-all arm `_ => ...`", if missing.len() == 1 { "an arm" } else { "arms" }, list));
            self.error(d);
        }
    }
}

fn is_primitive_type(name: &str) -> bool {
    matches!(name, "Int" | "Float" | "Str" | "Bool" | "Unit" | "Any" | "List" | "Map" | "Range" | "Fn")
}

fn field_names(td: &TypeDef) -> Vec<Name> {
    match &td.kind {
        TypeKind::Record { fields, .. } => fields.iter().cloned().collect(),
        TypeKind::Enum { variants } => variants.iter().filter(|v| v.named).flat_map(|v| v.fields.iter().cloned()).collect(),
    }
}
