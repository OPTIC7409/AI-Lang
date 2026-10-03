//! The tree-walking interpreter.

use crate::ast::*;
use crate::builtins::{BFn, BUILTINS};
use crate::ctx::{Ctx, GlobalKind};
use crate::diagnostic::{suggest, Diagnostic, TraceFrame};
use crate::span::Span;
use crate::types::{Name, Ty, TypeDef, TypeKind, OPTION_ID, ORDERING_ID, RESULT_ID};
use crate::value::*;
use std::cmp::Ordering;
use std::io::{IsTerminal, Write};
use std::rc::Rc;

pub enum Ctrl {
    Error(Box<Diagnostic>),
    Return(Value),
    Break(Value),
    Continue,
    /// `exit(code)` in an embedded interpreter, which must not end the host
    /// process: unwinds to the top level.
    Exit(i32),
}

pub type R<T = Value> = Result<T, Ctrl>;

pub struct Env {
    pub locals: Vec<Value>,
    pub closure: Option<Rc<Closure>>,
}

impl Env {
    pub fn new(slots: u32) -> Env {
        Env { locals: vec![Value::Unit; slots as usize], closure: None }
    }
}

pub struct Frame {
    pub name: Rc<str>,
    pub call_span: Span,
}

enum PlaceRoot {
    Local(u32),
    Global(u32),
}

enum Step {
    Field(Name),
    Index(Value),
}

/// A small, fast, seedable PRNG (SplitMix64).
#[derive(Clone)]
pub struct Rng(pub u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }

    pub fn float(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Uniform in [lo, hi] (inclusive).
    pub fn range(&mut self, lo: i64, hi: i64) -> i64 {
        if hi <= lo {
            return lo;
        }
        let span = (hi as i128 - lo as i128 + 1) as u128;
        (lo as i128 + (self.next_u64() as u128 % span) as i128) as i64
    }

    pub fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next_u64() % n as u64) as usize
        }
    }
}

pub struct Interp {
    pub ctx: Ctx,
    pub globals: Vec<Option<Value>>,
    pub stack: Vec<Frame>,
    pub max_depth: usize,
    pub rng: Rng,
    pub args: Vec<String>,
    /// Discard all program output (used while generating test inputs).
    pub silent: bool,
    /// Collect program output here instead of writing it to stdout.
    pub capture: Option<String>,
    pub contracts: bool,
    /// Maximum number of steps (calls plus loop iterations) before a
    /// `Budget` error; used to stop runaway test cases.
    pub budget: Option<u64>,
    pub ticks: u64,
    /// Where the most recent `?` returned early (for error messages).
    try_span: Option<Span>,
    /// For the most recently finished call: the `?` that made it return early.
    pub last_try_return: Option<Span>,
    /// The first argument of a mutating function that failed (so the
    /// caller's variable is not lost).
    salvaged: Option<Value>,
    /// Global variables currently being changed by a mutating call.
    busy_globals: Vec<(u32, Name)>,
    /// Set by `cogito test`: `exit()` becomes an error.
    pub test_mode: bool,
    /// Running inside a host (such as a web page) rather than as a process:
    /// `exit()` unwinds with [`Ctrl::Exit`] instead of ending the process.
    pub embedded: bool,
    /// When set, standard input is read from here instead of the process.
    pub input: Option<std::io::Cursor<Vec<u8>>>,
    /// What `clock()` measures from.
    pub clock_start: f64,
    /// The status passed to `exit()` in embedded mode.
    pub exit_code: Option<i32>,
    stdout: std::io::BufWriter<std::io::Stdout>,
    stdout_tty: bool,
    pub none: Value,
}

impl Default for Interp {
    fn default() -> Self {
        Interp::new()
    }
}

impl Interp {
    pub fn new() -> Interp {
        let mut ctx = Ctx::new();
        let mut globals: Vec<Option<Value>> = Vec::new();
        // Built-in constructors were registered by Ctx::new.
        for info in ctx.globals.iter() {
            match &info.kind {
                GlobalKind::Ctor(c) => {
                    let td = ctx.types[c.type_id as usize].clone();
                    let (fields, _, _) = td.fields_of(c.tag);
                    if fields.is_empty() {
                        globals.push(Some(Value::Variant(Rc::new(VariantVal { ty: td.clone(), tag: c.tag, values: vec![] }))));
                    } else {
                        globals.push(Some(Value::Ctor(td.clone(), c.tag)));
                    }
                }
                _ => globals.push(None),
            }
        }
        for (i, b) in BUILTINS.iter().enumerate() {
            let slot = ctx.add_global(Rc::from(b.name), GlobalKind::Builtin(i as u16), Span::default());
            ctx.builtins.values.insert(Rc::from(b.name), slot);
            globals.push(Some(Value::Builtin(i as u16)));
        }
        for (name, v) in [("pi", std::f64::consts::PI), ("tau", std::f64::consts::TAU), ("e", std::f64::consts::E), ("inf", f64::INFINITY)] {
            let slot = ctx.add_global(Rc::from(name), GlobalKind::Const, Span::default());
            ctx.builtins.values.insert(Rc::from(name), slot);
            globals.push(Some(Value::Float(v)));
        }
        for (name, v) in [("max_int", i64::MAX), ("min_int", i64::MIN)] {
            let slot = ctx.add_global(Rc::from(name), GlobalKind::Const, Span::default());
            ctx.builtins.values.insert(Rc::from(name), slot);
            globals.push(Some(Value::Int(v)));
        }
        let none = Value::Variant(Rc::new(VariantVal { ty: ctx.types[OPTION_ID as usize].clone(), tag: 1, values: vec![] }));
        let seed = crate::platform::seed();
        Interp {
            ctx,
            globals,
            stack: Vec::new(),
            max_depth: std::env::var("COGITO_MAX_DEPTH").ok().and_then(|s| s.parse().ok()).unwrap_or(100_000),
            rng: Rng::new(seed),
            args: Vec::new(),
            silent: false,
            capture: None,
            contracts: true,
            budget: None,
            ticks: 0,
            try_span: None,
            last_try_return: None,
            salvaged: None,
            busy_globals: Vec::new(),
            test_mode: false,
            embedded: false,
            input: None,
            clock_start: crate::platform::monotonic_seconds(),
            exit_code: None,
            stdout: std::io::BufWriter::with_capacity(1 << 16, std::io::stdout()),
            stdout_tty: std::io::stdout().is_terminal(),
            none,
        }
    }

    // ------------------------------------------------------------ output

    pub fn write_out(&mut self, s: &str) {
        if self.silent {
            return;
        }
        if let Some(buf) = &mut self.capture {
            buf.push_str(s);
            return;
        }
        let r = self.stdout.write_all(s.as_bytes()).and_then(|_| if self.stdout_tty { self.stdout.flush() } else { Ok(()) });
        if let Err(e) = r {
            if e.kind() == std::io::ErrorKind::BrokenPipe {
                // The reader went away (e.g. `cogito prog | head`): stop quietly.
                std::process::exit(0);
            }
        }
    }

    /// Write to standard error (or into the capture buffer, when capturing).
    pub fn write_err(&mut self, s: &str) {
        if self.silent {
            return;
        }
        if let Some(buf) = &mut self.capture {
            buf.push_str(s);
            return;
        }
        self.flush();
        crate::err_out!("{}", s);
    }

    pub fn flush(&mut self) {
        let _ = self.stdout.flush();
    }

    /// Read one line (including its newline) from standard input.
    pub fn read_input_line(&mut self, buf: &mut Vec<u8>) -> std::io::Result<usize> {
        use std::io::BufRead;
        self.flush();
        match &mut self.input {
            Some(c) => c.read_until(b'\n', buf),
            None => std::io::stdin().lock().read_until(b'\n', buf),
        }
    }

    /// Read the rest of standard input.
    pub fn read_input_all(&mut self, buf: &mut Vec<u8>) -> std::io::Result<usize> {
        use std::io::Read;
        self.flush();
        match &mut self.input {
            Some(c) => c.read_to_end(buf),
            None => std::io::stdin().read_to_end(buf),
        }
    }

    /// Count `n` steps at once (for bulk operations such as building a large list).
    pub fn tick_n(&mut self, n: u64, span: Span) -> R<()> {
        if self.budget.is_some() {
            self.ticks = self.ticks.saturating_add(n.saturating_sub(1));
            return self.tick(span);
        }
        Ok(())
    }

    #[inline]
    pub fn tick(&mut self, span: Span) -> R<()> {
        self.ticks += 1;
        if let Some(b) = self.budget {
            if self.ticks > b {
                let help = if self.embedded && !self.test_mode {
                    "this usually means an infinite loop; the playground stops programs after this many steps\nto keep the page responsive (the `cogito` command-line tool has no limit)"
                } else {
                    "this usually means an infinite loop, or an input too large for the algorithm;\nlimit the inputs with `where`/`requires`, or raise the limit with `--budget N`"
                };
                return Err(self.fail(self.diag(span, "E0219", format!("step budget exceeded: more than {} calls and loop iterations", b)).help(help)));
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------ values

    pub fn some(&self, v: Value) -> Value {
        Value::Variant(Rc::new(VariantVal { ty: self.ctx.types[OPTION_ID as usize].clone(), tag: 0, values: vec![v] }))
    }

    pub fn none(&self) -> Value {
        self.none.clone()
    }

    pub fn option(&self, v: Option<Value>) -> Value {
        match v {
            Some(v) => self.some(v),
            None => self.none(),
        }
    }

    pub fn ok(&self, v: Value) -> Value {
        Value::Variant(Rc::new(VariantVal { ty: self.ctx.types[RESULT_ID as usize].clone(), tag: 0, values: vec![v] }))
    }

    pub fn err_val(&self, v: Value) -> Value {
        Value::Variant(Rc::new(VariantVal { ty: self.ctx.types[RESULT_ID as usize].clone(), tag: 1, values: vec![v] }))
    }

    pub fn ordering(&self, o: Ordering) -> Value {
        let tag = match o {
            Ordering::Less => 0,
            Ordering::Equal => 1,
            Ordering::Greater => 2,
        };
        Value::Variant(Rc::new(VariantVal { ty: self.ctx.types[ORDERING_ID as usize].clone(), tag, values: vec![] }))
    }

    // ------------------------------------------------------------ errors

    fn trace(&self, span: Span, skip_top: bool) -> Vec<TraceFrame> {
        let frames = if skip_top && !self.stack.is_empty() { &self.stack[..self.stack.len() - 1] } else { &self.stack[..] };
        if frames.is_empty() {
            return vec![];
        }
        let mut out = Vec::new();
        let mut loc = span;
        for f in frames.iter().rev() {
            out.push(TraceFrame { name: f.name.clone(), span: loc });
            loc = f.call_span;
        }
        if loc != Span::default() {
            out.push(TraceFrame { name: Rc::from("<top level>"), span: loc });
        }
        out.reverse();
        out
    }

    pub fn diag(&self, span: Span, code: &'static str, msg: impl Into<String>) -> Diagnostic {
        let mut d = Diagnostic::error(code, msg).at(span);
        d.trace = self.trace(span, false);
        d
    }

    pub fn err(&self, span: Span, code: &'static str, msg: impl Into<String>) -> Ctrl {
        Ctrl::Error(Box::new(self.diag(span, code, msg)))
    }

    pub fn fail(&self, d: Diagnostic) -> Ctrl {
        Ctrl::Error(Box::new(d))
    }

    fn snippet(&self, span: Span) -> String {
        if (span.file as usize) < self.ctx.sm.files.len() {
            self.ctx.sm.snippet(span).to_string()
        } else {
            "?".into()
        }
    }

    fn location(&self, span: Span) -> String {
        if (span.file as usize) < self.ctx.sm.files.len() && span != Span::default() {
            self.ctx.sm.location(span)
        } else {
            "<builtin>".into()
        }
    }

    // ------------------------------------------------------------ program

    /// Install the hoisted definitions of a program (functions, constructors,
    /// imported modules), then run its top-level statements.
    pub fn run_program(&mut self, prog: &Program) -> R<()> {
        if self.globals.len() < self.ctx.globals.len() {
            self.globals.resize(self.ctx.globals.len(), None);
        }
        self.install(prog)?;
        let mut env = Env::new(prog.num_slots);
        for item in &prog.items {
            if let Item::Stmt(s) = item {
                self.exec_stmt(s, &mut env)?;
            }
        }
        Ok(())
    }

    pub fn install(&mut self, prog: &Program) -> R<()> {
        if self.globals.len() < self.ctx.globals.len() {
            self.globals.resize(self.ctx.globals.len(), None);
        }
        for item in &prog.items {
            match item {
                Item::Fn(def) => self.install_fn(def),
                Item::Type(td) if matches!(td.body, TypeBody::Alias(_)) => {}
                Item::Type(td) => {
                    let ty = self.ctx.types[td.id as usize].clone();
                    match &ty.kind {
                        TypeKind::Record { .. } => self.globals[td.slot as usize] = Some(Value::Ctor(ty.clone(), 0)),
                        TypeKind::Enum { variants } => {
                            if let TypeBody::Enum(decls) = &td.body {
                                for (tag, (v, d)) in variants.iter().zip(decls).enumerate() {
                                    let val = if v.fields.is_empty() {
                                        Value::Variant(Rc::new(VariantVal { ty: ty.clone(), tag: tag as u32, values: vec![] }))
                                    } else {
                                        Value::Ctor(ty.clone(), tag as u32)
                                    };
                                    self.globals[d.slot as usize] = Some(val);
                                }
                            }
                        }
                    }
                }
                Item::Import(imp) => {
                    if let Some(m) = &imp.module {
                        if !m.executed.get() {
                            m.executed.set(true);
                            self.run_program(&m.program)?;
                        }
                        self.globals[imp.slot as usize] = Some(Value::Module(m.clone()));
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn same_sig(a: &FnDef, b: &FnDef) -> bool {
        a.params.len() == b.params.len()
            && a.params
                .iter()
                .zip(&b.params)
                .all(|(x, y)| x.ty.as_ref().map(|t| &t.ty).unwrap_or(&Ty::Any) == y.ty.as_ref().map(|t| &t.ty).unwrap_or(&Ty::Any))
    }

    fn install_fn(&mut self, def: &Rc<FnDef>) {
        let slot = def.global_slot.expect("top-level fn has a slot") as usize;
        let new = Value::Func(Rc::new(Closure { def: def.clone(), captures: vec![] }));
        let fallback = def.overload_fallback.and_then(|s| self.globals[s as usize].clone());
        let mut cands: Vec<Value> = match self.globals[slot].take() {
            Some(Value::Func(old)) => vec![Value::Func(old)],
            Some(Value::Overload(list)) => list.iter().cloned().collect(),
            _ => vec![],
        };
        cands.retain(|c| !matches!(c, Value::Builtin(_)));
        if let Some(i) = cands.iter().position(|c| matches!(c, Value::Func(f) if Self::same_sig(&f.def, def))) {
            cands[i] = new;
        } else {
            cands.push(new);
        }
        if let Some(fb) = fallback {
            cands.push(fb);
        }
        self.globals[slot] = Some(if cands.len() == 1 { cands.pop().unwrap() } else { Value::Overload(Rc::new(cands)) });
    }

    /// For a value whose type was declared in an imported module, the
    /// function `name` defined in that module (if any).
    fn home_method(&self, recv: &Value, name: &str) -> Option<Value> {
        if self.ctx.type_home.is_empty() {
            return None;
        }
        let id = match recv {
            Value::Record(r) => r.ty.as_ref()?.id,
            Value::Variant(v) => v.ty.id,
            _ => return None,
        };
        let m = self.ctx.type_home.get(&id)?;
        let v = self.global_by_name(&m.ns, name)?;
        if v.is_callable() {
            Some(v)
        } else {
            None
        }
    }

    pub fn global_by_name(&self, ns: &Namespace, name: &str) -> Option<Value> {
        let slot = ns.values.get(name)?;
        self.globals.get(*slot as usize).cloned().flatten()
    }

    // ------------------------------------------------------------ statements

    pub fn exec_block(&mut self, stmts: &[Stmt], env: &mut Env) -> R {
        let n = stmts.len();
        for (i, s) in stmts.iter().enumerate() {
            if i + 1 == n {
                if let StmtKind::Expr(e) = &s.kind {
                    return self.eval(e, env);
                }
            }
            self.exec_stmt(s, env)?;
        }
        Ok(Value::Unit)
    }

    fn store(&mut self, res: VarRes, v: Value, env: &mut Env) {
        match res {
            VarRes::Local(s) => env.locals[s as usize] = v,
            VarRes::Global(s) => self.globals[s as usize] = Some(v),
            _ => {}
        }
    }

    pub fn exec_stmt(&mut self, s: &Stmt, env: &mut Env) -> R<()> {
        match &s.kind {
            StmtKind::Expr(e) => {
                self.eval(e, env)?;
            }
            StmtKind::Let { pat, ty, value, mutable } => {
                let mut v = self.eval(value, env)?;
                if let Some(t) = ty {
                    let kw = if *mutable { "var" } else { "let" };
                    v = self.conform(v, &t.ty).map_err(|m| {
                        self.fail(
                            self.diag(value.span, "E0200", format!("type mismatch in `{}`: {}", kw, m)).note(format!("the annotation is `{}`", t.ty)),
                        )
                    })?;
                }
                if let PatKind::Bind { res, sub: None, .. } = &pat.kind {
                    self.store(*res, v, env);
                } else if !self.match_pattern(pat, &v, env) {
                    return Err(self.fail(
                        self.diag(pat.span, "E0212", format!("the pattern `{}` does not match the value", self.snippet(pat.span)))
                            .note(format!("the value is {}", short_repr(&v)))
                            .help("use `match` to handle values of different shapes"),
                    ));
                }
            }
            StmtKind::Assign { target, op, value, ty } => self.assign(target, *op, value, ty.as_ref(), env)?,
            StmtKind::Fn { def, res } => {
                let c = self.make_closure(def, env);
                self.store(*res, c, env);
            }
            StmtKind::Assert { cond, msg } => self.exec_assert(cond, msg.as_ref(), env, s.span)?,
        }
        Ok(())
    }

    fn exec_assert(&mut self, cond: &Expr, msg: Option<&Expr>, env: &mut Env, span: Span) -> R<()> {
        let src = self.snippet(cond.span);
        let mut notes = Vec::new();
        let passed = match &cond.kind {
            ExprKind::Binary { op, lhs, rhs } if op.is_comparison() => {
                let a = self.eval(lhs, env)?;
                let b = self.eval(rhs, env)?;
                let r = self.binop(*op, a.clone(), b.clone(), cond.span)?;
                if !matches!(r, Value::Bool(true)) {
                    notes.push(format!("left:  {}", repr(&a)));
                    notes.push(format!("right: {}", repr(&b)));
                }
                matches!(r, Value::Bool(true))
            }
            _ => match self.eval(cond, env)? {
                Value::Bool(b) => {
                    if !b {
                        for w in self.where_values(cond, env) {
                            notes.push(w);
                        }
                    }
                    b
                }
                other => {
                    return Err(self.fail(
                        self.diag(cond.span, "E0209", format!("`assert` needs a Bool, got {}", describe(&other)))
                            .help("write a comparison, like `assert x == 3`"),
                    ))
                }
            },
        };
        if passed {
            return Ok(());
        }
        let mut message = format!("assertion failed: `{}`", src);
        if let Some(m) = msg {
            let mv = self.eval(m, env)?;
            message = format!("{} ({})", message, display(&mv));
        }
        let mut d = self.diag(span, "E0300", message);
        if !notes.is_empty() {
            d = d.note(notes.join("\n"));
        }
        Err(self.fail(d))
    }

    /// "name = value" for each variable mentioned in an expression.
    pub fn where_values(&mut self, e: &Expr, env: &Env) -> Vec<String> {
        let mut vars: Vec<(Name, VarRes)> = Vec::new();
        collect_vars(e, &mut vars);
        let mut out = Vec::new();
        for (name, res) in vars {
            let v = match res {
                VarRes::Local(s) => env.locals.get(s as usize).cloned(),
                VarRes::Capture(i) => env.closure.as_ref().and_then(|c| c.captures.get(i as usize).cloned()),
                VarRes::Global(s) => self.globals.get(s as usize).cloned().flatten(),
                _ => None,
            };
            if let Some(v) = v {
                if !v.is_callable() && !matches!(v, Value::Module(_)) {
                    out.push(format!("{} = {}", name, short_repr(&v)));
                }
            }
        }
        out
    }

    fn assign(&mut self, target: &Expr, op: Option<BinOp>, value: &Expr, decl: Option<&Ty>, env: &mut Env) -> R<()> {
        let rhs = self.eval(value, env)?;
        let span = target.span;
        // Fast paths for plain variables without a declared type.
        if let (ExprKind::Var(v), None) = (&target.kind, decl) {
            match v.res {
                VarRes::Local(s) => {
                    let s = s as usize;
                    match op {
                        None => env.locals[s] = rhs,
                        Some(op) => {
                            if !append_in_place(op, &mut env.locals[s], &rhs) {
                                let cur = env.locals[s].clone();
                                env.locals[s] = self.binop(op, cur, rhs, span)?;
                            }
                        }
                    }
                    return Ok(());
                }
                VarRes::Global(s) => {
                    let s = s as usize;
                    match op {
                        None => self.globals[s] = Some(rhs),
                        Some(op) => {
                            let Some(cur) = self.globals[s].as_mut() else {
                                return Err(self.err(span, "E0214", format!("`{}` is used before it is initialized", v.name)));
                            };
                            if !append_in_place(op, cur, &rhs) {
                                let cur = cur.clone();
                                let nv = self.binop(op, cur, rhs, span)?;
                                self.globals[s] = Some(nv);
                            }
                        }
                    }
                    return Ok(());
                }
                _ => {}
            }
        }
        let (root, steps) = self.eval_place(target, env)?;
        let expected = self.expected_type(&root, &steps, decl, env);
        let mismatch = |me: &Self, m: String| {
            me.fail(
                me.diag(span, "E0200", format!("type mismatch in assignment to `{}`: {}", me.snippet(span), m))
                    .help("the variable (or field) was declared with a type, and every write must respect it"),
            )
        };
        match op {
            None => {
                let v = match &expected {
                    Some(t) => self.conform(rhs, t).map_err(|m| mismatch(self, m))?,
                    None => rhs,
                };
                self.with_place(&root, &steps, true, env, span, |p| *p = v)?;
            }
            Some(op) => {
                // `+=` on strings and lists appends in place; only the new
                // elements need checking against the declared element type.
                let fast = op == BinOp::Add
                    && match (&expected, &rhs) {
                        (None, _) => true,
                        (Some(Ty::Str), Value::Str(_)) => true,
                        (Some(Ty::List(et)), Value::List(ys)) => ys.iter().all(|y| self.has_type(y, et, false)),
                        _ => false,
                    };
                if fast {
                    let fp = expected.as_ref().map(|t| t.fingerprint());
                    let rhs_ref = &rhs;
                    let done = self.with_place(&root, &steps, false, env, span, |p| {
                        let valid = matches!((&*p, fp), (Value::List(xs), Some(f)) if xs.checked() == f);
                        let ok = append_in_place(op, p, rhs_ref);
                        if let (true, true, Value::List(xs), Some(f)) = (ok, valid, &*p, fp) {
                            xs.set_checked(f);
                        }
                        ok
                    })?;
                    if done {
                        return Ok(());
                    }
                }
                let cur = self.with_place(&root, &steps, false, env, span, |p| p.clone())?;
                let mut nv = self.binop(op, cur, rhs, span)?;
                if let Some(t) = &expected {
                    nv = self.conform(nv, t).map_err(|m| mismatch(self, m))?;
                }
                self.with_place(&root, &steps, false, env, span, |p| *p = nv)?;
            }
        }
        Ok(())
    }

    /// The type that a write to this place must have: the declared type of
    /// the root variable walked along the path, or the declared type of a
    /// field of a nominal record or variant.
    fn expected_type(&self, root: &PlaceRoot, steps: &[Step], decl: Option<&Ty>, env: &Env) -> Option<Ty> {
        if let Some(t) = decl {
            if let Some(w) = self.walk_ty(t, steps) {
                return Some(w);
            }
        }
        let last = steps.last()?;
        if !matches!(last, Step::Field(_)) {
            return None;
        }
        let root_val = self.root_value(root, env)?;
        let parent = peek_place(root_val, &steps[..steps.len() - 1])?;
        nominal_field_ty(parent, last)
    }

    fn root_value<'a>(&'a self, root: &PlaceRoot, env: &'a Env) -> Option<&'a Value> {
        match root {
            PlaceRoot::Local(s) => env.locals.get(*s as usize),
            PlaceRoot::Global(s) => self.globals.get(*s as usize)?.as_ref(),
        }
    }

    fn walk_ty(&self, ty: &Ty, steps: &[Step]) -> Option<Ty> {
        let mut cur = ty.clone();
        for st in steps {
            cur = match (&cur, st) {
                (Ty::List(t), Step::Index(_)) => (**t).clone(),
                (Ty::Map(_, v), Step::Index(_)) => (**v).clone(),
                (Ty::Tuple(ts), Step::Field(n)) => ts.get(n.parse::<usize>().ok()?)?.clone(),
                (Ty::Tuple(ts), Step::Index(Value::Int(i))) => ts.get(norm_index(*i, ts.len())?)?.clone(),
                (Ty::Record(fs), Step::Field(n)) => fs.iter().find(|(f, _)| f == n)?.1.clone(),
                (Ty::Named { id, args, .. }, Step::Field(n)) => {
                    let td = self.ctx.types.get(*id as usize)?;
                    match &td.kind {
                        TypeKind::Record { fields, tys } => tys[fields.iter().position(|f| f == n)?].subst(args),
                        _ => return None,
                    }
                }
                _ => return None,
            };
            if cur.is_any() {
                return None;
            }
        }
        if cur.is_any() {
            None
        } else {
            Some(cur)
        }
    }

    fn eval_place(&mut self, e: &Expr, env: &mut Env) -> R<(PlaceRoot, Vec<Step>)> {
        match &e.kind {
            ExprKind::Var(v) => match v.res {
                VarRes::Local(s) => Ok((PlaceRoot::Local(s), vec![])),
                VarRes::Global(s) => Ok((PlaceRoot::Global(s), vec![])),
                _ => Err(self.err(e.span, "E0012", format!("`{}` cannot be changed here", v.name))),
            },
            ExprKind::Field { target, name, .. } => {
                let (root, mut steps) = self.eval_place(target, env)?;
                steps.push(Step::Field(name.clone()));
                Ok((root, steps))
            }
            ExprKind::Index { target, index } => {
                let (root, mut steps) = self.eval_place(target, env)?;
                let i = self.eval(index, env)?;
                steps.push(Step::Index(i));
                Ok((root, steps))
            }
            _ => Err(self.err(e.span, "E0012", "invalid assignment target")),
        }
    }

    fn with_place<T>(
        &mut self,
        root: &PlaceRoot,
        steps: &[Step],
        insert_last: bool,
        env: &mut Env,
        span: Span,
        f: impl FnOnce(&mut Value) -> T,
    ) -> R<T> {
        let res = {
            let root_val: Option<&mut Value> = match root {
                PlaceRoot::Local(s) => Some(&mut env.locals[*s as usize]),
                PlaceRoot::Global(s) => self.globals[*s as usize].as_mut(),
            };
            match root_val {
                None => Err(("E0214", "variable used before it is initialized".to_string())),
                Some(cur) => walk_place(cur, steps, insert_last).map(f),
            }
        };
        res.map_err(|(code, msg)| self.err(span, code, msg))
    }

    // ------------------------------------------------------------ expressions

    #[inline]
    fn load(&self, v: &Var, span: Span, env: &Env) -> R {
        match v.res {
            VarRes::Local(s) => Ok(env.locals[s as usize].clone()),
            VarRes::Capture(i) => Ok(env.closure.as_ref().expect("closure").captures[i as usize].clone()),
            VarRes::Global(s) => match &self.globals[s as usize] {
                Some(v) => Ok(v.clone()),
                None => {
                    if let Some((_, f)) = self.busy_globals.iter().find(|(b, _)| *b == s) {
                        return Err(self.err(span, "E0214", format!("`{}` cannot be read while `{}` is changing it", v.name, f)));
                    }
                    Err(self.err(span, "E0214", format!("`{}` is used before it is initialized", v.name)))
                }
            },
            VarRes::SelfFn => Ok(Value::Func(env.closure.clone().expect("self fn"))),
            VarRes::Unresolved => Err(self.err(span, "E0100", format!("undefined name `{}`", v.name))),
        }
    }

    pub fn make_closure(&self, def: &Rc<FnDef>, env: &Env) -> Value {
        let captures = def
            .captures
            .iter()
            .map(|c| match c {
                CaptureSrc::Local(s) => env.locals[*s as usize].clone(),
                CaptureSrc::Capture(i) => env.closure.as_ref().expect("closure").captures[*i as usize].clone(),
                CaptureSrc::SelfFn => Value::Func(env.closure.clone().expect("self fn")),
            })
            .collect();
        Value::Func(Rc::new(Closure { def: def.clone(), captures }))
    }

    fn eval_cond(&mut self, e: &Expr, env: &mut Env, what: &str) -> R<bool> {
        match self.eval(e, env)? {
            Value::Bool(b) => Ok(b),
            other => Err(self.fail(self.not_bool(e.span, what, &other))),
        }
    }

    fn not_bool(&self, span: Span, what: &str, v: &Value) -> Diagnostic {
        let help = match v {
            Value::Int(_) | Value::Float(_) => "compare explicitly, e.g. `x != 0`",
            Value::Str(_) => "compare explicitly, e.g. `s != \"\"`",
            Value::List(_) | Value::Map(_) => "check the length explicitly, e.g. `not xs.is_empty()`",
            Value::Variant(vv) if vv.ty.id == OPTION_ID => "use `x.is_some()`, or `match` on the Option",
            Value::Variant(vv) if vv.ty.id == RESULT_ID => "use `x.is_ok()`, or `match` on the Result",
            _ => "conditions must be `true` or `false`",
        };
        self.diag(span, "E0209", format!("{} must be a Bool, but this is {}", what, describe(v))).help(help)
    }

    pub fn eval_args(&mut self, args: &[Arg], env: &mut Env, first: Option<Value>) -> R<(Vec<Value>, Vec<(Name, Value)>)> {
        let mut pos = Vec::with_capacity(args.len() + 1);
        if let Some(f) = first {
            pos.push(f);
        }
        let mut named = Vec::new();
        for a in args {
            let v = self.eval(&a.value, env)?;
            match &a.name {
                None => {
                    if !named.is_empty() {
                        return Err(self.err(a.value.span, "E0108", "positional arguments must come before named arguments"));
                    }
                    pos.push(v)
                }
                Some(n) => named.push((n.clone(), v)),
            }
        }
        Ok((pos, named))
    }

    pub fn eval(&mut self, e: &Expr, env: &mut Env) -> R {
        match &e.kind {
            ExprKind::Unit => Ok(Value::Unit),
            ExprKind::Bool(b) => Ok(Value::Bool(*b)),
            ExprKind::Int(n) => Ok(Value::Int(*n)),
            ExprKind::Float(f) => Ok(Value::Float(*f)),
            ExprKind::Str(s) => Ok(Value::Str(s.clone())),
            ExprKind::Var(v) => self.load(v, e.span, env),
            ExprKind::Interp(parts) => {
                let mut s = String::new();
                for p in parts {
                    match p {
                        InterpPart::Lit(l) => s.push_str(l),
                        InterpPart::Expr(x, spec) => {
                            let v = self.eval(x, env)?;
                            match spec {
                                None => write_value(&mut s, &v, false),
                                Some(sp) => {
                                    let f = self.format_spec(&v, sp, x.span)?;
                                    s.push_str(&f);
                                }
                            }
                        }
                    }
                }
                Ok(Value::str(s))
            }
            ExprKind::List(items) => {
                let mut out = Vec::with_capacity(items.len());
                for it in items {
                    let v = self.eval(&it.expr, env)?;
                    if it.spread {
                        let xs = self.iter_values(v, it.expr.span)?;
                        out.extend(xs);
                    } else {
                        out.push(v);
                    }
                }
                Ok(Value::list(out))
            }
            ExprKind::Comprehension { body, clauses } => {
                let mut out = Vec::new();
                self.comprehension(clauses, 0, body, env, &mut out)?;
                Ok(Value::list(out))
            }
            ExprKind::Map(entries) => {
                let mut m = MapVal::with_capacity(entries.len());
                for (k, v) in entries {
                    let k = self.eval(k, env)?;
                    let v = self.eval(v, env)?;
                    m.insert(k, v);
                }
                Ok(Value::Map(Rc::new(m)))
            }
            ExprKind::Tuple(items) => {
                let mut out = Vec::with_capacity(items.len());
                for it in items {
                    out.push(self.eval(it, env)?);
                }
                Ok(Value::tuple(out))
            }
            ExprKind::Record { names, values, spread } => {
                let mut vals = Vec::with_capacity(values.len());
                for v in values {
                    vals.push(self.eval(v, env)?);
                }
                match spread {
                    None => Ok(Value::Record(Rc::new(RecordVal { ty: None, names: names.clone(), values: vals }))),
                    Some(base) => {
                        let b = self.eval(base, env)?;
                        let Value::Record(mut r) = b else {
                            return Err(self.err(base.span, "E0211", format!("`..` in a record literal needs a record, got {}", describe(&b))));
                        };
                        let rec = Rc::make_mut(&mut r);
                        let mut new_names: Option<Vec<Name>> = None;
                        for (n, v) in names.iter().zip(vals) {
                            match rec.names.iter().position(|x| x == n) {
                                Some(i) => {
                                    let v = match &rec.ty {
                                        Some(td) => {
                                            let td = td.clone();
                                            let (_, tys, _) = td.fields_of(0);
                                            self.conform(v, &tys[i]).map_err(|m| {
                                                self.err(e.span, "E0200", format!("type mismatch for field `{}` of `{}`: {}", n, td.name, m))
                                            })?
                                        }
                                        None => v,
                                    };
                                    rec.values[i] = v;
                                }
                                None => {
                                    if let Some(td) = &rec.ty {
                                        return Err(self.err(e.span, "E0203", format!("`{}` has no field `{}`", td.name, n)));
                                    }
                                    new_names.get_or_insert_with(|| rec.names.to_vec()).push(n.clone());
                                    rec.values.push(v);
                                }
                            }
                        }
                        if let Some(nn) = new_names {
                            rec.names = nn.into();
                        }
                        Ok(Value::Record(r))
                    }
                }
            }
            ExprKind::Field { target, name, name_span } => {
                let v = self.eval(target, env)?;
                self.get_field(&v, name, *name_span)
            }
            ExprKind::Index { target, index } => {
                let v = self.eval(target, env)?;
                let i = self.eval(index, env)?;
                self.index_value(v, i, e.span)
            }
            ExprKind::Call { callee, args } => {
                let f = self.eval(callee, env)?;
                let (pos, named) = self.eval_args(args, env, None)?;
                self.call_value(&f, pos, named, e.span)
            }
            ExprKind::MethodCall { receiver, method, method_span, args, mutating, root_ty } => {
                if *mutating {
                    return self.mutating_call(receiver, method, *method_span, args, e.span, root_ty.as_ref(), env);
                }
                let recv = self.eval(receiver, env)?;
                match &recv {
                    Value::Record(r) => {
                        if let Some(f) = r.get(&method.name) {
                            let f = f.clone();
                            let (pos, named) = self.eval_args(args, env, None)?;
                            return self.call_value(&f, pos, named, e.span);
                        }
                    }
                    Value::Module(m) => {
                        let m = m.clone();
                        let Some(f) = self.global_by_name(&m.ns, &method.name) else {
                            return Err(self.err(*method_span, "E0203", format!("module `{}` has no member `{}`", m.name, method.name)));
                        };
                        let (pos, named) = self.eval_args(args, env, None)?;
                        return self.call_value(&f, pos, named, e.span);
                    }
                    _ => {}
                }
                let home_fn = self.home_method(&recv, &method.name);
                let f = match home_fn {
                    Some(f) => f,
                    None => {
                        if method.res == VarRes::Unresolved {
                            return Err(self.fail(self.no_member(&recv, &method.name, *method_span)));
                        }
                        self.load(method, *method_span, env)?
                    }
                };
                let (pos, named) = self.eval_args(args, env, Some(recv))?;
                self.call_value(&f, pos, named, e.span)
            }
            ExprKind::Unary { op, expr } => {
                let v = self.eval(expr, env)?;
                match (op, v) {
                    (UnOp::Neg, Value::Int(i)) => {
                        i.checked_neg().map(Value::Int).ok_or_else(|| self.err(e.span, "E0207", "integer overflow in negation"))
                    }
                    (UnOp::Neg, Value::Float(f)) => Ok(Value::Float(-f)),
                    (UnOp::Not, Value::Bool(b)) => Ok(Value::Bool(!b)),
                    (UnOp::Not, other) => Err(self.fail(self.not_bool(expr.span, "the operand of `not`", &other))),
                    (UnOp::Neg, other) => Err(self.err(e.span, "E0211", format!("cannot negate {}", describe(&other)))),
                }
            }
            ExprKind::Binary { op, lhs, rhs } => {
                let a = self.eval(lhs, env)?;
                let b = self.eval(rhs, env)?;
                self.binop(*op, a, b, e.span)
            }
            ExprKind::And(a, b) => {
                if !self.eval_cond(a, env, "the left side of `and`")? {
                    return Ok(Value::Bool(false));
                }
                Ok(Value::Bool(self.eval_cond(b, env, "the right side of `and`")?))
            }
            ExprKind::Or(a, b) => {
                if self.eval_cond(a, env, "the left side of `or`")? {
                    return Ok(Value::Bool(true));
                }
                Ok(Value::Bool(self.eval_cond(b, env, "the right side of `or`")?))
            }
            ExprKind::Range { start, end, inclusive } => {
                let s = self.eval(start, env)?;
                let Value::Int(s) = s else {
                    return Err(self.err(start.span, "E0211", format!("range bounds must be Int, got {}", describe(&s))));
                };
                let end = match end {
                    None => None,
                    Some(x) => {
                        let v = self.eval(x, env)?;
                        let Value::Int(n) = v else {
                            return Err(self.err(x.span, "E0211", format!("range bounds must be Int, got {}", describe(&v))));
                        };
                        if *inclusive {
                            Some(n as i128 + 1)
                        } else {
                            Some(n as i128)
                        }
                    }
                };
                Ok(Value::Range(Rc::new(RangeVal { start: s, end })))
            }
            ExprKind::Try(inner) => {
                let v = self.eval(inner, env)?;
                match &v {
                    Value::Variant(vv) if vv.ty.id == RESULT_ID || vv.ty.id == OPTION_ID => {
                        if vv.tag == 0 {
                            Ok(vv.values[0].clone())
                        } else {
                            self.try_span = Some(e.span);
                            Err(Ctrl::Return(v))
                        }
                    }
                    _ => Err(self.fail(
                        self.diag(e.span, "E0211", format!("`?` needs a Result or Option, got {}", describe(&v)))
                            .help("`?` unwraps `Ok(x)`/`Some(x)` and returns early on `Err`/`None`"),
                    )),
                }
            }
            ExprKind::If { cond, then, els } => {
                if self.eval_cond(cond, env, "the `if` condition")? {
                    self.eval(then, env)
                } else if let Some(x) = els {
                    self.eval(x, env)
                } else {
                    Ok(Value::Unit)
                }
            }
            ExprKind::Match { scrutinee, arms } => {
                let v = self.eval(scrutinee, env)?;
                for arm in arms {
                    // With a guard, each alternative of an or-pattern gets its own chance.
                    if let (PatKind::Or(alts), Some(g)) = (&arm.pat.kind, &arm.guard) {
                        for alt in alts {
                            if self.match_pattern(alt, &v, env) && self.eval_cond(g, env, "the match guard")? {
                                return self.eval(&arm.body, env);
                            }
                        }
                        continue;
                    }
                    if self.match_pattern(&arm.pat, &v, env) {
                        if let Some(g) = &arm.guard {
                            if !self.eval_cond(g, env, "the match guard")? {
                                continue;
                            }
                        }
                        return self.eval(&arm.body, env);
                    }
                }
                Err(self.fail(
                    self.diag(scrutinee.span, "E0208", format!("no match arm matched the value {}", short_repr(&v)))
                        .help("add an arm for this value, or a catch-all arm `_ => ...`"),
                ))
            }
            ExprKind::Block(stmts) => self.exec_block(stmts, env),
            ExprKind::Lambda(def) => Ok(self.make_closure(def, env)),
            ExprKind::While { cond, body } => {
                while self.eval_cond(cond, env, "the `while` condition")? {
                    self.tick(e.span)?;
                    match self.eval(body, env) {
                        Ok(_) | Err(Ctrl::Continue) => {}
                        Err(Ctrl::Break(_)) => break,
                        Err(other) => return Err(other),
                    }
                }
                Ok(Value::Unit)
            }
            ExprKind::Loop { body } => loop {
                self.tick(e.span)?;
                match self.eval(body, env) {
                    Ok(_) | Err(Ctrl::Continue) => {}
                    Err(Ctrl::Break(v)) => return Ok(v),
                    Err(other) => return Err(other),
                }
            },
            ExprKind::For { pat, iter, body } => {
                let it = self.eval(iter, env)?;
                self.exec_for(pat, it, body, env, iter.span)
            }
            ExprKind::Break(v) => {
                let v = match v {
                    Some(x) => self.eval(x, env)?,
                    None => Value::Unit,
                };
                Err(Ctrl::Break(v))
            }
            ExprKind::Continue => Err(Ctrl::Continue),
            ExprKind::Return(v) => {
                let v = match v {
                    Some(x) => self.eval(x, env)?,
                    None => Value::Unit,
                };
                self.try_span = None;
                Err(Ctrl::Return(v))
            }
        }
    }

    fn comprehension(&mut self, clauses: &[CompClause], i: usize, body: &Expr, env: &mut Env, out: &mut Vec<Value>) -> R<()> {
        if i == clauses.len() {
            out.push(self.eval(body, env)?);
            return Ok(());
        }
        match &clauses[i] {
            CompClause::If(c) => {
                if self.eval_cond(c, env, "the comprehension condition")? {
                    self.comprehension(clauses, i + 1, body, env, out)?;
                }
            }
            CompClause::For(pat, iter) => {
                let it = self.eval(iter, env)?;
                let items = self.iter_values(it, iter.span)?;
                for item in items {
                    self.tick(iter.span)?;
                    self.bind_loop(pat, item, env)?;
                    self.comprehension(clauses, i + 1, body, env, out)?;
                }
            }
        }
        Ok(())
    }

    fn bind_loop(&mut self, pat: &Pattern, v: Value, env: &mut Env) -> R<()> {
        if let PatKind::Bind { res: VarRes::Local(s), sub: None, .. } = &pat.kind {
            env.locals[*s as usize] = v;
            return Ok(());
        }
        if !self.match_pattern(pat, &v, env) {
            return Err(self.fail(
                self.diag(pat.span, "E0212", format!("the loop pattern `{}` does not match the element", self.snippet(pat.span)))
                    .note(format!("the element is {}", short_repr(&v))),
            ));
        }
        Ok(())
    }

    fn exec_for(&mut self, pat: &Pattern, it: Value, body: &Expr, env: &mut Env, span: Span) -> R {
        macro_rules! run_body {
            () => {
                self.tick(span)?;
                match self.eval(body, env) {
                    Ok(_) | Err(Ctrl::Continue) => {}
                    Err(Ctrl::Break(_)) => break,
                    Err(other) => return Err(other),
                }
            };
        }
        match it {
            Value::Range(r) => {
                let mut i = r.start;
                loop {
                    if let Some(end) = r.end {
                        if i as i128 >= end {
                            break;
                        }
                    }
                    self.bind_loop(pat, Value::Int(i), env)?;
                    run_body!();
                    match i.checked_add(1) {
                        Some(n) => i = n,
                        None => break,
                    }
                }
            }
            Value::List(xs) => {
                for x in xs.iter() {
                    self.bind_loop(pat, x.clone(), env)?;
                    run_body!();
                }
            }
            Value::Str(s) => {
                let mut buf = [0u8; 4];
                for c in s.chars() {
                    self.bind_loop(pat, Value::str(&*c.encode_utf8(&mut buf)), env)?;
                    run_body!();
                }
            }
            Value::Map(m) => {
                for (k, v) in m.entries.iter() {
                    self.bind_loop(pat, Value::tuple(vec![k.clone(), v.clone()]), env)?;
                    run_body!();
                }
            }
            other => return Err(self.fail(self.not_iterable(&other, span))),
        }
        Ok(Value::Unit)
    }

    fn not_iterable(&self, v: &Value, span: Span) -> Diagnostic {
        let mut d = self.diag(span, "E0211", format!("cannot iterate over {}", describe(v)));
        d = match v {
            Value::Int(_) => d.help("to count, iterate over a range: `for i in 0..n`"),
            Value::Variant(vv) if vv.ty.id == OPTION_ID => d.help("match on the Option instead, or use `.unwrap_or(...)`"),
            _ => d.help("lists, ranges, strings and maps can be iterated"),
        };
        d
    }

    /// Materialize an iterable value into a vector of its elements.
    pub fn iter_values(&mut self, v: Value, span: Span) -> R<Vec<Value>> {
        match v {
            Value::List(xs) => Ok(list_into_vec(xs)),
            Value::Tuple(xs) => Ok(list_into_vec(xs)),
            Value::Range(r) => match r.end {
                Some(end) => {
                    let n = r.len().unwrap_or(0);
                    if n > 100_000_000 {
                        return Err(self.err(span, "E0216", format!("range {}..{} is too large to collect into a list", r.start, end)));
                    }
                    self.tick_n(n as u64, span)?;
                    Ok((r.start as i128..end).map(|i| Value::Int(i as i64)).collect())
                }
                None => Err(self.err(span, "E0216", "cannot collect an unbounded range").map_help("give the range an end: `0..n`")),
            },
            Value::Str(s) => Ok(s.chars().map(|c| Value::str(c.to_string())).collect()),
            Value::Map(m) => Ok(m.entries.iter().map(|(k, v)| Value::tuple(vec![k.clone(), v.clone()])).collect()),
            other => Err(self.fail(self.not_iterable(&other, span))),
        }
    }

    // ------------------------------------------------------------ fields and indexes

    fn no_member(&self, v: &Value, name: &str, span: Span) -> Diagnostic {
        let mut d = self.diag(span, "E0203", format!("{} has no field or method `{}`", describe(v), name));
        if let Value::Record(r) = v {
            if let Some(s) = suggest(name, r.names.iter().map(|n| &**n)) {
                d = d.help(format!("did you mean `{}`?", s));
            } else {
                d = d.note(format!("its fields are: {}", r.names.iter().map(|n| n.to_string()).collect::<Vec<_>>().join(", ")));
            }
        } else {
            let names: Vec<&str> = BUILTINS.iter().map(|b| b.name).collect();
            if let Some(s) = suggest(name, names) {
                d = d.help(format!("did you mean `{}`?", s));
            }
        }
        d
    }

    pub fn get_field(&mut self, v: &Value, name: &str, span: Span) -> R {
        match v {
            Value::Record(r) => match r.get(name) {
                Some(x) => Ok(x.clone()),
                None => Err(self.fail(self.no_member(v, name, span))),
            },
            Value::Variant(vv) => {
                let (fields, _, _) = vv.ty.fields_of(vv.tag);
                match fields.iter().position(|f| &**f == name) {
                    Some(i) => Ok(vv.values[i].clone()),
                    None => {
                        let mut d = self.diag(span, "E0203", format!("`{}` has no field `{}`", vv.name(), name));
                        if vv.ty.is_enum() {
                            d = d.help(format!("`{}` is a variant of `{}`; use `match` to handle each variant", vv.name(), vv.ty.name));
                        }
                        Err(self.fail(d))
                    }
                }
            }
            Value::Tuple(t) => match name.parse::<usize>() {
                Ok(i) if i < t.len() => Ok(t[i].clone()),
                Ok(i) => Err(self.err(span, "E0204", format!("tuple index {} is out of bounds for a tuple of {} elements", i, t.len()))),
                Err(_) => Err(self.fail(self.no_member(v, name, span))),
            },
            Value::Module(m) => match self.global_by_name(&m.ns, name) {
                Some(x) => Ok(x),
                None => Err(self.err(span, "E0203", format!("module `{}` has no member `{}`", m.name, name))),
            },
            _ => {
                let mut d = self.diag(span, "E0203", format!("{} has no field `{}`", describe(v), name));
                if self.ctx.builtins.values.contains_key(name) {
                    d = d.help(format!("to call the function `{}`, write `.{}()`", name, name));
                }
                Err(self.fail(d))
            }
        }
    }

    pub fn index_value(&mut self, v: Value, idx: Value, span: Span) -> R {
        match (&v, &idx) {
            (Value::List(xs), Value::Int(i)) => match norm_index(*i, xs.len()) {
                Some(i) => Ok(xs[i].clone()),
                None => Err(self.fail(
                    self.diag(span, "E0204", format!("index {} is out of bounds for a list of length {}", i, xs.len()))
                        .label(if xs.is_empty() {
                            "the list is empty".to_string()
                        } else {
                            format!("valid indexes are 0..{} (or -{}..-1)", xs.len() - 1, xs.len())
                        })
                        .help("use `xs.get(i)`, which returns an Option, if the index may be missing"),
                )),
            },
            (Value::Tuple(xs), Value::Int(i)) => match norm_index(*i, xs.len()) {
                Some(i) => Ok(xs[i].clone()),
                None => Err(self.err(span, "E0204", format!("index {} is out of bounds for a tuple of {} elements", i, xs.len()))),
            },
            (Value::List(xs), Value::Range(r)) => {
                let (a, b) = slice_bounds(r, xs.len());
                Ok(Value::list(xs[a..b].to_vec()))
            }
            (Value::Str(s), Value::Int(i)) => {
                let n = s.char_len();
                match norm_index(*i, n) {
                    Some(i) => Ok(Value::str(s.char_at(i).unwrap_or(""))),
                    None => Err(self.err(span, "E0204", format!("index {} is out of bounds for a string of length {}", i, n))),
                }
            }
            (Value::Str(s), Value::Range(r)) => {
                let n = s.char_len();
                let (a, b) = slice_bounds(r, n);
                Ok(Value::str(s.slice_chars(a, b)))
            }
            (Value::Map(m), k) => match m.get(k) {
                Some(x) => Ok(x.clone()),
                None => Err(self.fail(
                    self.diag(span, "E0205", format!("key {} not found in map", short_repr(k)))
                        .help("use `m.get(key)` (returns an Option) or `m.get_or(key, default)`"),
                )),
            },
            (Value::Range(r), Value::Int(i)) => {
                let len = r.len_u128();
                let ok = *i >= 0 && len.is_none_or(|n| (*i as u128) < n);
                if ok {
                    Ok(Value::Int((r.start as i128 + *i as i128) as i64))
                } else {
                    Err(self.err(span, "E0204", format!("index {} is out of bounds for the range", i)))
                }
            }
            _ => Err(self.err(span, "E0211", format!("cannot index {} with {}", describe(&v), describe(&idx)))),
        }
    }

    // ------------------------------------------------------------ operators

    fn bad_binop(&self, op: BinOp, a: &Value, b: &Value, span: Span) -> Ctrl {
        let mut d = self.diag(span, "E0211", format!("cannot apply `{}` to {} and {}", op.symbol(), describe(a), describe(b)));
        let (ta, tb) = (type_name(a), type_name(b));
        let help = match (op, a, b) {
            (BinOp::Add, Value::Str(_), _) | (BinOp::Add, _, Value::Str(_)) => {
                Some("convert with `str(x)`, or use interpolation: \"text {x}\"".to_string())
            }
            (BinOp::Add, Value::List(_), _) => Some("to add one element, use `xs.push(x)` or `xs + [x]`".to_string()),
            (_, Value::Variant(v), _) | (_, _, Value::Variant(v)) if v.ty.id == OPTION_ID => {
                Some("this is an Option; get the value out first with `match`, `?`, or `.unwrap_or(default)`".to_string())
            }
            (_, Value::Variant(v), _) | (_, _, Value::Variant(v)) if v.ty.id == RESULT_ID => {
                Some("this is a Result; get the value out first with `match`, `?`, or `.unwrap_or(default)`".to_string())
            }
            (_, Value::Str(_), Value::Int(_) | Value::Float(_)) | (_, Value::Int(_) | Value::Float(_), Value::Str(_)) => {
                Some("convert between numbers and strings explicitly with `int(s)`, `float(s)` or `str(x)`".to_string())
            }
            _ if ta == tb => None,
            _ => Some("Cogito never converts types implicitly (except Int to Float)".to_string()),
        };
        if let Some(h) = help {
            d = d.help(h);
        }
        self.fail(d)
    }

    fn div_zero(&self, span: Span) -> Ctrl {
        self.fail(self.diag(span, "E0206", "division by zero").help("check the divisor first, or state it as a contract: `requires d != 0`"))
    }

    fn overflow(&self, span: Span, op: BinOp, a: i64, b: i64) -> Ctrl {
        self.fail(
            self.diag(span, "E0207", format!("integer overflow: {} {} {}", a, op.symbol(), b))
                .help("Int is 64-bit; use Float (e.g. `float(x)`) for larger magnitudes"),
        )
    }

    pub fn binop(&mut self, op: BinOp, a: Value, b: Value, span: Span) -> R {
        use Value::*;
        match op {
            BinOp::Add => match (a, b) {
                (Int(x), Int(y)) => x.checked_add(y).map(Int).ok_or_else(|| self.overflow(span, op, x, y)),
                (Float(x), Float(y)) => Ok(Float(x + y)),
                (Int(x), Float(y)) => Ok(Float(x as f64 + y)),
                (Float(x), Int(y)) => Ok(Float(x + y as f64)),
                (Str(mut x), Str(y)) => {
                    Rc::make_mut(&mut x).push_str(&y);
                    Ok(Str(x))
                }
                (List(mut x), List(y)) => {
                    Rc::make_mut(&mut x).extend(y.iter().cloned());
                    Ok(List(x))
                }
                (a, b) => Err(self.bad_binop(op, &a, &b, span)),
            },
            BinOp::Sub => match (a, b) {
                (Int(x), Int(y)) => x.checked_sub(y).map(Int).ok_or_else(|| self.overflow(span, op, x, y)),
                (Float(x), Float(y)) => Ok(Float(x - y)),
                (Int(x), Float(y)) => Ok(Float(x as f64 - y)),
                (Float(x), Int(y)) => Ok(Float(x - y as f64)),
                (a, b) => Err(self.bad_binop(op, &a, &b, span)),
            },
            BinOp::Mul => match (a, b) {
                (Int(x), Int(y)) => x.checked_mul(y).map(Int).ok_or_else(|| self.overflow(span, op, x, y)),
                (Float(x), Float(y)) => Ok(Float(x * y)),
                (Int(x), Float(y)) => Ok(Float(x as f64 * y)),
                (Float(x), Int(y)) => Ok(Float(x * y as f64)),
                (Str(s), Int(n)) | (Int(n), Str(s)) => {
                    if n < 0 {
                        return Err(self.err(span, "E0216", format!("cannot repeat a string {} times", n)));
                    }
                    if (s.len() as u128) * (n as u128) > 1 << 31 {
                        return Err(self.err(span, "E0216", "repeated string would be too large"));
                    }
                    self.tick_n((s.len() as u64 * n as u64) / 64, span)?;
                    Ok(Value::str(s.repeat(n as usize)))
                }
                (List(xs), Int(n)) | (Int(n), List(xs)) => {
                    if n < 0 {
                        return Err(self.err(span, "E0216", format!("cannot repeat a list {} times", n)));
                    }
                    if xs.is_empty() {
                        return Ok(Value::list(vec![]));
                    }
                    if (xs.len() as u128) * (n as u128) > 1 << 28 {
                        return Err(self.err(span, "E0216", "repeated list would be too large"));
                    }
                    self.tick_n(xs.len() as u64 * n as u64, span)?;
                    let mut out = Vec::with_capacity(xs.len() * n as usize);
                    for _ in 0..n {
                        out.extend(xs.iter().cloned());
                    }
                    Ok(Value::list(out))
                }
                (a, b) => Err(self.bad_binop(op, &a, &b, span)),
            },
            BinOp::Div => {
                let (Some(x), Some(y)) = (a.as_f64(), b.as_f64()) else {
                    return Err(self.bad_binop(op, &a, &b, span));
                };
                if y == 0.0 {
                    return Err(self.div_zero(span));
                }
                Ok(Float(x / y))
            }
            BinOp::FloorDiv => match (a, b) {
                (Int(x), Int(y)) => {
                    if y == 0 {
                        return Err(self.div_zero(span));
                    }
                    let q = x.checked_div(y).ok_or_else(|| self.overflow(span, op, x, y))?;
                    Ok(Int(if (x % y != 0) && ((x < 0) != (y < 0)) { q - 1 } else { q }))
                }
                (a, b) => {
                    let (Some(x), Some(y)) = (a.as_f64(), b.as_f64()) else {
                        return Err(self.bad_binop(op, &a, &b, span));
                    };
                    if y == 0.0 {
                        return Err(self.div_zero(span));
                    }
                    Ok(Float(float_divmod(x, y).0))
                }
            },
            BinOp::Mod => match (a, b) {
                (Int(x), Int(y)) => {
                    if y == 0 {
                        return Err(self.div_zero(span));
                    }
                    let r = x.checked_rem(y).unwrap_or(0);
                    Ok(Int(if r != 0 && ((r < 0) != (y < 0)) { r + y } else { r }))
                }
                (a, b) => {
                    let (Some(x), Some(y)) = (a.as_f64(), b.as_f64()) else {
                        return Err(self.bad_binop(op, &a, &b, span));
                    };
                    if y == 0.0 {
                        return Err(self.div_zero(span));
                    }
                    Ok(Float(float_divmod(x, y).1))
                }
            },
            BinOp::Pow => match (a, b) {
                (Int(x), Int(y)) => {
                    if y < 0 {
                        return Err(self.fail(
                            self.diag(span, "E0216", format!("negative exponent {} for an Int base", y))
                                .help("use a Float base for fractional results: `2.0 ** -1`"),
                        ));
                    }
                    if y > u32::MAX as i64 {
                        return Err(self.overflow(span, op, x, y));
                    }
                    x.checked_pow(y as u32).map(Int).ok_or_else(|| self.overflow(span, op, x, y))
                }
                (a, b) => {
                    let (Some(x), Some(y)) = (a.as_f64(), b.as_f64()) else {
                        return Err(self.bad_binop(op, &a, &b, span));
                    };
                    Ok(Float(x.powf(y)))
                }
            },
            BinOp::Eq => Ok(Bool(values_equal(&a, &b))),
            BinOp::Ne => Ok(Bool(!values_equal(&a, &b))),
            BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => match compare(&a, &b) {
                Some(o) => Ok(Bool(match op {
                    BinOp::Lt => o == Ordering::Less,
                    BinOp::Le => o != Ordering::Greater,
                    BinOp::Gt => o == Ordering::Greater,
                    _ => o != Ordering::Less,
                })),
                None => {
                    if let (Float(x), _) | (_, Float(x)) = (&a, &b) {
                        if x.is_nan() {
                            return Ok(Bool(false));
                        }
                    }
                    Err(self.bad_binop(op, &a, &b, span))
                }
            },
            BinOp::In | BinOp::NotIn => {
                let r = self.contains(&b, &a, span)?;
                Ok(Bool(if op == BinOp::In { r } else { !r }))
            }
        }
    }

    pub fn contains(&self, container: &Value, x: &Value, span: Span) -> R<bool> {
        match container {
            Value::List(xs) | Value::Tuple(xs) => Ok(xs.iter().any(|y| values_equal(x, y))),
            Value::Str(s) => match x {
                Value::Str(sub) => Ok(s.contains(sub.as_str())),
                _ => Err(self.err(span, "E0211", format!("`in` on a string needs a Str to search for, got {}", describe(x)))),
            },
            Value::Map(m) => Ok(m.contains(x)),
            Value::Range(r) => match x {
                Value::Int(n) => Ok(r.contains(*n)),
                _ => Ok(false),
            },
            _ => Err(self.err(span, "E0211", format!("cannot check membership in {}", describe(container)))),
        }
    }

    // ------------------------------------------------------------ formatting

    pub fn format_spec(&mut self, v: &Value, spec: &FmtSpec, span: Span) -> R<String> {
        let numeric = matches!(v, Value::Int(_) | Value::Float(_));
        let mut body = match (spec.kind, v) {
            (Some(k @ ('x' | 'X' | 'b' | 'o')), Value::Int(n)) => {
                let a = n.unsigned_abs();
                let digits = match k {
                    'x' => format!("{:x}", a),
                    'X' => format!("{:X}", a),
                    'b' => format!("{:b}", a),
                    _ => format!("{:o}", a),
                };
                if *n < 0 {
                    format!("-{}", digits)
                } else {
                    digits
                }
            }
            (Some('e'), x) if numeric => {
                // Python style: 1.234500e+03
                let f = x.as_f64().unwrap();
                let s = format!("{:.*e}", spec.precision.unwrap_or(6), f);
                match s.split_once('e') {
                    Some((m, e)) => {
                        let n: i32 = e.parse().unwrap_or(0);
                        format!("{}e{}{:02}", m, if n < 0 { '-' } else { '+' }, n.abs())
                    }
                    None => s,
                }
            }
            (Some('%'), x) if numeric => {
                let f = x.as_f64().unwrap() * 100.0;
                format!("{:.*}%", spec.precision.unwrap_or(0), f)
            }
            (Some(k), _) => {
                return Err(self.err(span, "E0216", format!("format type `{}` cannot be used with {}", k, describe(v))));
            }
            (None, Value::Float(f)) if spec.precision.is_some() && !f.is_finite() => format_float(*f),
            (None, Value::Float(f)) if spec.precision.is_some() => format!("{:.*}", spec.precision.unwrap(), f),
            // Ints are formatted exactly (not through a Float).
            (None, Value::Int(n)) if spec.precision.is_some() => {
                let p = spec.precision.unwrap();
                if p == 0 {
                    n.to_string()
                } else {
                    format!("{}.{}", n, "0".repeat(p))
                }
            }
            (None, Value::Str(s)) if spec.precision.is_some() => s.chars().take(spec.precision.unwrap()).collect(),
            _ => display(v),
        };
        if spec.group && numeric && spec.kind.is_none() {
            body = group_thousands(&body);
        }
        if spec.plus && numeric && !body.starts_with('-') {
            body.insert(0, '+');
        }
        let len = body.chars().count();
        if spec.width > len {
            let pad = spec.width - len;
            if spec.zero && numeric && spec.align.is_none() {
                let (sign, rest) = if body.starts_with('-') || body.starts_with('+') { body.split_at(1) } else { ("", body.as_str()) };
                body = format!("{}{}{}", sign, "0".repeat(pad), rest);
            } else {
                let fill = spec.fill.to_string();
                let align = spec.align.unwrap_or(if numeric { '>' } else { '<' });
                body = match align {
                    '>' => format!("{}{}", fill.repeat(pad), body),
                    '^' => format!("{}{}{}", fill.repeat(pad / 2), body, fill.repeat(pad - pad / 2)),
                    _ => format!("{}{}", body, fill.repeat(pad)),
                };
            }
        }
        Ok(body)
    }

    // ------------------------------------------------------------ patterns

    pub fn match_pattern(&mut self, p: &Pattern, v: &Value, env: &mut Env) -> bool {
        match &p.kind {
            PatKind::Wild => true,
            PatKind::Bind { res, sub, .. } => {
                if let Some(s) = sub {
                    if !self.match_pattern(s, v, env) {
                        return false;
                    }
                }
                self.store(*res, v.clone(), env);
                true
            }
            PatKind::Lit(l) => lit_matches(l, v),
            PatKind::Range { lo, hi, inclusive } => {
                let (lo, hi) = (lit_value(lo), lit_value(hi));
                let (Some(a), Some(b)) = (compare(v, &lo), compare(v, &hi)) else { return false };
                a != Ordering::Less && (b == Ordering::Less || (*inclusive && b == Ordering::Equal))
            }
            PatKind::Tuple(ps) => match v {
                Value::Tuple(xs) if xs.len() == ps.len() => {
                    let xs = xs.clone();
                    ps.iter().zip(xs.iter()).all(|(p, x)| self.match_pattern(p, x, env))
                }
                _ => false,
            },
            PatKind::List { before, rest, after } => {
                let Value::List(xs) = v else { return false };
                let xs = xs.clone();
                let n = xs.len();
                let fixed = before.len() + after.len();
                if rest.is_none() && n != fixed || n < fixed {
                    return false;
                }
                for (p, x) in before.iter().zip(xs.iter()) {
                    if !self.match_pattern(p, x, env) {
                        return false;
                    }
                }
                for (p, x) in after.iter().zip(xs[n - after.len()..].iter()) {
                    if !self.match_pattern(p, x, env) {
                        return false;
                    }
                }
                if let Some(Some(rp)) = rest {
                    let mid = Value::list(xs[before.len()..n - after.len()].to_vec());
                    if !self.match_pattern(rp, &mid, env) {
                        return false;
                    }
                }
                true
            }
            PatKind::Ctor { ctor, args, field_idx, .. } => {
                let values: &Vec<Value> = match v {
                    Value::Variant(vv) if !ctor.is_record && vv.ty.id == ctor.type_id && vv.tag == ctor.tag => &vv.values,
                    Value::Record(r) if ctor.is_record && r.ty.as_ref().is_some_and(|t| t.id == ctor.type_id) => &r.values,
                    _ => return false,
                };
                let values = values.clone();
                for ((_, ap), idx) in args.iter().zip(field_idx) {
                    match values.get(*idx as usize) {
                        Some(x) => {
                            if !self.match_pattern(ap, x, env) {
                                return false;
                            }
                        }
                        None => return false,
                    }
                }
                true
            }
            PatKind::Record { fields, rest } => {
                let Value::Record(r) = v else { return false };
                let r = r.clone();
                if !*rest && r.names.len() != fields.len() {
                    return false;
                }
                for (n, fp) in fields {
                    match r.get(n) {
                        Some(x) => {
                            if !self.match_pattern(fp, x, env) {
                                return false;
                            }
                        }
                        None => return false,
                    }
                }
                true
            }
            PatKind::Or(alts) => alts.iter().any(|a| self.match_pattern(a, v, env)),
        }
    }

    // ------------------------------------------------------------ types

    /// Does `v` have type `ty`? With `coerce`, Int is accepted for Float.
    pub fn has_type(&self, v: &Value, ty: &Ty, coerce: bool) -> bool {
        match (ty, v) {
            (Ty::Any | Ty::Generic(_) | Ty::Param(..), _) => true,
            (Ty::Int, Value::Int(_)) | (Ty::Float, Value::Float(_)) | (Ty::Str, Value::Str(_)) | (Ty::Bool, Value::Bool(_)) => true,
            (Ty::Float, Value::Int(_)) => coerce,
            (Ty::Unit, Value::Unit) | (Ty::Range, Value::Range(_)) => true,
            (Ty::List(t), Value::List(xs)) => {
                if t.is_any() {
                    return true;
                }
                // Memoize successful exact checks: an unchanged list is not re-scanned.
                let fp = if coerce { 0 } else { ty.fingerprint() };
                if fp != 0 && xs.checked() == fp {
                    return true;
                }
                let ok = xs.iter().all(|x| self.has_type(x, t, coerce));
                if ok && fp != 0 {
                    xs.set_checked(fp);
                }
                ok
            }
            (Ty::Map(k, t), Value::Map(m)) => {
                if k.is_any() && t.is_any() {
                    return true;
                }
                let fp = if coerce { 0 } else { ty.fingerprint() };
                if fp != 0 && m.checked() == fp {
                    return true;
                }
                let ok = m.entries.iter().all(|(a, b)| self.has_type(a, k, coerce) && self.has_type(b, t, coerce));
                if ok && fp != 0 {
                    m.set_checked(fp);
                }
                ok
            }
            (Ty::Tuple(ts), Value::Tuple(xs)) => ts.len() == xs.len() && ts.iter().zip(xs.iter()).all(|(t, x)| self.has_type(x, t, coerce)),
            (Ty::Record(fs), Value::Record(r)) => fs.iter().all(|(n, t)| r.get(n).is_some_and(|x| self.has_type(x, t, coerce))),
            (Ty::Fn(..), v) => v.is_callable(),
            (Ty::Named { id, args, .. }, Value::Variant(vv)) => {
                vv.ty.id == *id && {
                    let (_, tys, _) = vv.ty.fields_of(vv.tag);
                    args.is_empty() || vv.values.iter().zip(tys).all(|(x, t)| self.has_type(x, &t.subst(args), coerce))
                }
            }
            (Ty::Named { id, args, .. }, Value::Record(r)) if r.ty.is_none() => {
                // An anonymous record is accepted (and converted) where a record
                // type with exactly the same fields is expected.
                coerce && {
                    let td = &self.ctx.types[*id as usize];
                    match &td.kind {
                        TypeKind::Record { fields, tys } => {
                            fields.len() == r.names.len()
                                && fields.iter().zip(tys).all(|(f, t)| r.get(f).is_some_and(|x| self.has_type(x, &t.subst(args), true)))
                        }
                        _ => false,
                    }
                }
            }
            (Ty::Named { id, args, .. }, Value::Record(r)) => {
                r.ty.as_ref().is_some_and(|t| t.id == *id) && {
                    let td = r.ty.as_ref().unwrap();
                    let (_, tys, _) = td.fields_of(0);
                    args.is_empty() || r.values.iter().zip(tys).all(|(x, t)| self.has_type(x, &t.subst(args), coerce))
                }
            }
            _ => false,
        }
    }

    /// Check `v` against `ty`, converting Int to Float where a Float is
    /// expected. On mismatch, returns a description of the problem.
    pub fn conform(&self, v: Value, ty: &Ty) -> Result<Value, String> {
        if self.has_type(&v, ty, false) {
            return Ok(v);
        }
        match (ty, &v) {
            (Ty::Float, Value::Int(i)) => return Ok(Value::Float(*i as f64)),
            (Ty::List(t), Value::List(xs)) => {
                let mut out = Vec::with_capacity(xs.len());
                for (i, x) in xs.iter().enumerate() {
                    match self.conform(x.clone(), t) {
                        Ok(y) => out.push(y),
                        Err(m) => return Err(format!("expected {}, but element {} is wrong: {}", ty, i, m)),
                    }
                }
                return Ok(Value::list(out));
            }
            (Ty::Map(kt, vt), Value::Map(m)) => {
                let mut out = MapVal::with_capacity(m.len());
                for (k, x) in m.entries.iter() {
                    let k2 = self.conform(k.clone(), kt).map_err(|e| format!("expected {}, but a key is wrong: {}", ty, e))?;
                    let x2 = self
                        .conform(x.clone(), vt)
                        .map_err(|e| format!("expected {}, but the value for key {} is wrong: {}", ty, short_repr(k), e))?;
                    out.insert(k2, x2);
                }
                return Ok(Value::Map(Rc::new(out)));
            }
            (Ty::Tuple(ts), Value::Tuple(xs)) if ts.len() == xs.len() => {
                let mut out = Vec::with_capacity(xs.len());
                for (i, (t, x)) in ts.iter().zip(xs.iter()).enumerate() {
                    out.push(self.conform(x.clone(), t).map_err(|e| format!("expected {}, but element {} is wrong: {}", ty, i, e))?);
                }
                return Ok(Value::tuple(out));
            }
            (Ty::Record(fs), Value::Record(r)) => {
                let mut r2 = (**r).clone();
                for (n, t) in fs {
                    match r.names.iter().position(|x| x == n) {
                        Some(i) => {
                            r2.values[i] =
                                self.conform(r.values[i].clone(), t).map_err(|e| format!("expected {}, but field `{}` is wrong: {}", ty, n, e))?;
                        }
                        None => return Err(format!("expected {}, but the record has no field `{}`", ty, n)),
                    }
                }
                return Ok(Value::Record(Rc::new(r2)));
            }
            (Ty::Named { id, args, .. }, Value::Variant(vv)) if vv.ty.id == *id => {
                let (fields, tys, _) = vv.ty.fields_of(vv.tag);
                let mut vv2 = (**vv).clone();
                for (i, t) in tys.iter().enumerate() {
                    vv2.values[i] = self
                        .conform(vv.values[i].clone(), &t.subst(args))
                        .map_err(|e| format!("expected {}, but field `{}` of {} is wrong: {}", ty, fields[i], vv.name(), e))?;
                }
                return Ok(Value::Variant(Rc::new(vv2)));
            }
            (Ty::Named { id, args, .. }, Value::Record(r)) if r.ty.is_none() => {
                let td = self.ctx.types[*id as usize].clone();
                if let TypeKind::Record { fields, tys } = &td.kind {
                    let extra: Vec<&Name> = r.names.iter().filter(|n| !fields.contains(n)).collect();
                    let missing: Vec<&Name> = fields.iter().filter(|n| r.get(n).is_none()).collect();
                    if extra.is_empty() && missing.is_empty() {
                        let mut values = Vec::with_capacity(fields.len());
                        for (f, t) in fields.iter().zip(tys) {
                            let v = r.get(f).cloned().unwrap_or_default();
                            values.push(self.conform(v, &t.subst(args)).map_err(|e| format!("expected {}, but field `{}` is wrong: {}", ty, f, e))?);
                        }
                        return Ok(Value::Record(Rc::new(RecordVal { ty: Some(td.clone()), names: fields.clone(), values })));
                    }
                    let mut why = Vec::new();
                    if !missing.is_empty() {
                        why.push(format!("missing {}", missing.iter().map(|n| format!("`{}`", n)).collect::<Vec<_>>().join(", ")));
                    }
                    if !extra.is_empty() {
                        why.push(format!("unexpected {}", extra.iter().map(|n| format!("`{}`", n)).collect::<Vec<_>>().join(", ")));
                    }
                    return Err(format!("expected {}, got a record with different fields ({})", ty, why.join("; ")));
                }
            }
            (Ty::Named { id, args, .. }, Value::Record(r)) if r.ty.as_ref().is_some_and(|t| t.id == *id) => {
                let td = r.ty.clone().unwrap();
                let (fields, tys, _) = td.fields_of(0);
                let mut r2 = (**r).clone();
                for (i, t) in tys.iter().enumerate() {
                    r2.values[i] = self
                        .conform(r.values[i].clone(), &t.subst(args))
                        .map_err(|e| format!("expected {}, but field `{}` is wrong: {}", ty, fields[i], e))?;
                }
                return Ok(Value::Record(Rc::new(r2)));
            }
            _ => {}
        }
        Err(format!("expected {}, got {}", ty, describe(&v)))
    }

    // ------------------------------------------------------------ calls

    pub fn call_value(&mut self, f: &Value, args: Vec<Value>, named: Vec<(Name, Value)>, span: Span) -> R {
        match f {
            Value::Func(c) => self.call_closure(c, args, named, span),
            Value::Builtin(idx) => {
                let b = &BUILTINS[*idx as usize];
                let args = if named.is_empty() {
                    args
                } else {
                    crate::builtins::arrange_named(*idx, args, named).map_err(|m| self.err(span, "E0108", m))?
                };
                self.check_builtin_arity(*idx, args.len(), span)?;
                match b.f {
                    BFn::Pure(fp) => fp(self, args, span),
                    BFn::Mut(_) => Err(self.err(
                        span,
                        "E0111",
                        format!("`{}` changes its first argument, so it must be called on a variable: `x.{}(...)`", b.name, b.name),
                    )),
                }
            }
            Value::Overload(cands) => {
                let cands = cands.clone();
                for c in cands.iter() {
                    if self.accepts(c, &args, &named) {
                        return self.call_value(c, args, named, span);
                    }
                }
                Err(self.fail(self.no_overload(&cands, &args, &named, span)))
            }
            Value::Ctor(td, tag) => self.construct(td, *tag, args, named, span),
            Value::Variant(vv) => Err(self.fail(
                self.diag(span, "E0202", format!("`{}` is a value, not a function", vv.name()))
                    .help(format!("write just `{}` without parentheses", vv.name())),
            )),
            other => Err(self.err(span, "E0202", format!("{} is not callable", describe(other)))),
        }
    }

    fn no_overload(&self, cands: &[Value], args: &[Value], named: &[(Name, Value)], span: Span) -> Diagnostic {
        let mut parts: Vec<String> = args.iter().map(type_name).collect();
        parts.extend(named.iter().map(|(n, v)| format!("{}: {}", n, type_name(v))));
        let got = parts.join(", ");
        let name = match cands.first() {
            Some(Value::Func(c)) => c.def.display_name().to_string(),
            Some(Value::Builtin(i)) => BUILTINS[*i as usize].name.to_string(),
            _ => "function".into(),
        };
        let mut lines = Vec::new();
        for c in cands {
            match c {
                Value::Func(c) => {
                    let ps = c
                        .def
                        .params
                        .iter()
                        .map(|p| match &p.ty {
                            Some(t) => format!("{}: {}", p.name, t.ty),
                            None => p.name.to_string(),
                        })
                        .collect::<Vec<_>>()
                        .join(", ");
                    lines.push(format!("{}({})  at {}", name, ps, self.location(c.def.name_span)));
                }
                Value::Builtin(i) => lines.push(format!("{} (built-in)", BUILTINS[*i as usize].doc.lines().next().unwrap_or(""))),
                _ => {}
            }
        }
        self.diag(span, "E0215", format!("no definition of `{}` accepts arguments of types ({})", name, got))
            .note(format!("candidates are:\n{}", lines.join("\n")))
    }

    fn check_builtin_arity(&self, idx: u16, n: usize, span: Span) -> R<()> {
        let b = &BUILTINS[idx as usize];
        if n < b.min as usize || n > b.max as usize {
            let expect = if b.min == b.max {
                format!("{}", b.min)
            } else if b.max == crate::builtins::VARIADIC {
                format!("at least {}", b.min)
            } else {
                format!("{} to {}", b.min, b.max)
            };
            return Err(self.fail(
                self.diag(
                    span,
                    "E0201",
                    format!("`{}` takes {} argument{}, but {} were given", b.name, expect, if expect == "1" { "" } else { "s" }, n),
                )
                .note(format!("usage: {}", b.doc.lines().next().unwrap_or(""))),
            ));
        }
        Ok(())
    }

    fn accepts(&self, c: &Value, args: &[Value], named: &[(Name, Value)]) -> bool {
        match c {
            Value::Func(f) => {
                let d = &f.def;
                let n = args.len() + named.len();
                if args.len() > d.params.len() || n < d.required_params() || n > d.params.len() {
                    return false;
                }
                for (a, p) in args.iter().zip(&d.params) {
                    if let Some(t) = &p.ty {
                        if !self.has_type(a, &t.ty, true) {
                            return false;
                        }
                    }
                }
                for (nm, a) in named {
                    match d.params.iter().find(|p| &p.name == nm) {
                        Some(p) => {
                            if let Some(t) = &p.ty {
                                if !self.has_type(a, &t.ty, true) {
                                    return false;
                                }
                            }
                        }
                        None => return false,
                    }
                }
                true
            }
            Value::Builtin(i) => {
                let b = &BUILTINS[*i as usize];
                let n = args.len() + named.len();
                (named.is_empty() || !crate::builtins::param_names(*i).is_empty()) && n >= b.min as usize && n <= b.max as usize
            }
            _ => true,
        }
    }

    pub fn construct(&mut self, td: &Rc<TypeDef>, tag: u32, args: Vec<Value>, named: Vec<(Name, Value)>, span: Span) -> R {
        let (fields, tys, field_named) = td.fields_of(tag);
        let cname = td.ctor_name(tag);
        let n = fields.len();
        if args.len() > n {
            return Err(self.err(
                span,
                "E0201",
                format!("`{}` has {} field{}, but {} arguments were given", cname, n, if n == 1 { "" } else { "s" }, args.len()),
            ));
        }
        let mut vals: Vec<Option<Value>> = vec![None; n];
        for (i, a) in args.into_iter().enumerate() {
            vals[i] = Some(a);
        }
        for (nm, v) in named {
            if !field_named {
                return Err(self.err(span, "E0108", format!("`{}` has positional fields and cannot take named arguments", cname)));
            }
            match fields.iter().position(|f| *f == nm) {
                Some(i) => {
                    if vals[i].is_some() {
                        return Err(self.err(span, "E0108", format!("field `{}` is given twice", nm)));
                    }
                    vals[i] = Some(v);
                }
                None => {
                    let mut d = self.diag(span, "E0108", format!("`{}` has no field `{}`", cname, nm));
                    if let Some(s) = suggest(&nm, fields.iter().map(|f| &**f)) {
                        d = d.help(format!("did you mean `{}`?", s));
                    }
                    return Err(self.fail(d));
                }
            }
        }
        let mut values = Vec::with_capacity(n);
        for (i, v) in vals.into_iter().enumerate() {
            match v {
                None => return Err(self.err(span, "E0201", format!("missing field `{}` when building `{}`", fields[i], cname))),
                Some(v) => {
                    let v = self
                        .conform(v, &tys[i])
                        .map_err(|m| self.fail(self.diag(span, "E0200", format!("type mismatch for field `{}` of `{}`: {}", fields[i], cname, m))))?;
                    values.push(v);
                }
            }
        }
        if td.is_enum() {
            Ok(Value::Variant(Rc::new(VariantVal { ty: td.clone(), tag, values })))
        } else {
            Ok(Value::Record(Rc::new(RecordVal { ty: Some(td.clone()), names: fields.clone(), values })))
        }
    }

    pub fn call_closure(&mut self, c: &Rc<Closure>, args: Vec<Value>, named: Vec<(Name, Value)>, span: Span) -> R {
        Ok(self.call_closure_full(c, args, named, span, false)?.0)
    }

    /// Call a closure. When `want_first` is set, also return the final value
    /// of the first parameter (used by mutating `!` functions).
    pub fn call_closure_full(
        &mut self,
        c: &Rc<Closure>,
        args: Vec<Value>,
        named: Vec<(Name, Value)>,
        span: Span,
        want_first: bool,
    ) -> R<(Value, Value)> {
        let def = c.def.clone();
        self.tick(span)?;
        if self.stack.len() >= self.max_depth {
            return Err(self.fail(
                self.diag(span, "E0213", format!("stack overflow: more than {} nested calls (in `{}`)", self.max_depth, def.display_name()))
                    .help(if self.embedded {
                        "check that the recursion has a base case that is always reached;\nthe browser's stack is small, so the playground allows fewer nested calls than the `cogito` tool"
                    } else {
                        "check that the recursion has a base case that is always reached;\nfor legitimately deep recursion, raise the limit with `cogito --max-depth N ...`"
                    }),
            ));
        }
        let nparams = def.params.len();
        if args.len() > nparams {
            return Err(self.fail(
                self.diag(
                    span,
                    "E0201",
                    format!(
                        "`{}` takes {} argument{}, but {} were given",
                        def.display_name(),
                        nparams,
                        if nparams == 1 { "" } else { "s" },
                        args.len()
                    ),
                )
                .note(format!("`{}` is defined at {}", def.display_name(), self.location(def.name_span))),
            ));
        }
        let mut env = Env { locals: vec![Value::Unit; def.num_slots as usize], closure: Some(c.clone()) };
        let mut filled = 0u64;
        let nargs = args.len();
        for (i, a) in args.into_iter().enumerate() {
            env.locals[def.params[i].slot as usize] = a;
        }
        if nparams <= 64 {
            filled = if nargs >= 64 { u64::MAX } else { (1u64 << nargs) - 1 };
        }
        for (n, v) in named {
            match def.params.iter().position(|p| p.name == n) {
                Some(i) => {
                    if filled & (1 << i) != 0 {
                        return Err(self.err(span, "E0108", format!("argument `{}` is given twice", n)));
                    }
                    filled |= 1 << i;
                    env.locals[def.params[i].slot as usize] = v;
                }
                None => {
                    let mut d = self.diag(span, "E0108", format!("`{}` has no parameter named `{}`", def.display_name(), n));
                    if let Some(s) = suggest(&n, def.params.iter().map(|p| &*p.name)) {
                        d = d.help(format!("did you mean `{}`?", s));
                    }
                    return Err(self.fail(d));
                }
            }
        }
        self.stack.push(Frame { name: def.display_name(), call_span: span });
        let r = self.call_body(&def, &mut env, filled, span);
        self.stack.pop();
        if r.is_err() && want_first && nparams > 0 {
            self.salvaged = Some(std::mem::take(&mut env.locals[def.params[0].slot as usize]));
        }
        let result = r?;
        let first = if want_first && nparams > 0 { std::mem::take(&mut env.locals[def.params[0].slot as usize]) } else { Value::Unit };
        Ok((result, first))
    }

    fn call_body(&mut self, def: &Rc<FnDef>, env: &mut Env, filled: u64, span: Span) -> R {
        for (i, p) in def.params.iter().enumerate() {
            if filled & (1 << i) == 0 {
                match &p.default {
                    Some(d) => {
                        let v = self.eval(d, env)?;
                        env.locals[p.slot as usize] = v;
                    }
                    None => {
                        let missing: Vec<String> = def
                            .params
                            .iter()
                            .enumerate()
                            .filter(|(j, q)| filled & (1 << j) == 0 && q.default.is_none())
                            .map(|(_, q)| format!("`{}`", q.name))
                            .collect();
                        return Err(self.fail({
                            let mut d = Diagnostic::error(
                                "E0201",
                                format!(
                                    "`{}` is missing argument{} {}",
                                    def.display_name(),
                                    if missing.len() == 1 { "" } else { "s" },
                                    missing.join(", ")
                                ),
                            )
                            .at(span)
                            .note(format!("`{}` is defined at {}", def.display_name(), self.location(def.name_span)));
                            d.trace = self.trace(span, true);
                            d
                        }));
                    }
                }
            }
            if let Some(t) = &p.ty {
                if !self.has_type(&env.locals[p.slot as usize], &t.ty, false) {
                    let v = std::mem::take(&mut env.locals[p.slot as usize]);
                    match self.conform(v, &t.ty) {
                        Ok(v) => env.locals[p.slot as usize] = v,
                        Err(m) => {
                            let pname = match &p.pat {
                                Some(pat) => self.snippet(pat.span),
                                None => p.name.to_string(),
                            };
                            let mut d =
                                Diagnostic::error("E0200", format!("type mismatch for parameter `{}` of `{}`: {}", pname, def.display_name(), m))
                                    .at(span)
                                    .label(format!("`{}` expects {} here", def.display_name(), t.ty))
                                    .note(format!("`{}` is declared as `{}: {}` at {}", pname, pname, t.ty, self.location(p.span)));
                            d.trace = self.trace(span, true);
                            return Err(self.fail(d));
                        }
                    }
                }
            }
        }
        for p in def.params.iter() {
            if let Some(pat) = &p.pat {
                let v = env.locals[p.slot as usize].clone();
                if !self.match_pattern(pat, &v, env) {
                    let mut d = Diagnostic::error(
                        "E0212",
                        format!("argument of `{}` does not match the parameter pattern `{}`", def.display_name(), self.snippet(pat.span)),
                    )
                    .at(span)
                    .note(format!("the argument is {}", short_repr(&v)));
                    d.trace = self.trace(span, true);
                    return Err(self.fail(d));
                }
            }
        }
        if self.contracts {
            for r in &def.requires {
                match self.eval(r, env)? {
                    Value::Bool(true) => {}
                    Value::Bool(false) => {
                        let wh = self.where_values(r, env);
                        let mut d =
                            Diagnostic::error("E0301", format!("precondition of `{}` violated: `{}`", def.display_name(), self.snippet(r.span)))
                                .at(span)
                                .label(format!("this call breaks a precondition of `{}`", def.display_name()))
                                .note(format!("`{}` requires `{}` (at {})", def.display_name(), self.snippet(r.span), self.location(r.span)));
                        if !wh.is_empty() {
                            d = d.note(format!("where {}", wh.join(", ")));
                        }
                        d = d.help("this is a bug in the caller: make sure the arguments satisfy the precondition");
                        d.trace = self.trace(span, true);
                        return Err(self.fail(d));
                    }
                    other => return Err(self.fail(self.not_bool(r.span, "a `requires` clause", &other))),
                }
            }
        }
        if self.contracts && !def.ensures.is_empty() {
            for (e, slot) in &def.olds {
                let v = self.eval(e, env)?;
                env.locals[*slot as usize] = v;
            }
        }
        let mut from_try = None;
        self.last_try_return = None;
        let mut result = match self.eval(&def.body, env) {
            Ok(v) => v,
            Err(Ctrl::Return(v)) => {
                from_try = self.try_span.take();
                v
            }
            Err(Ctrl::Break(_)) | Err(Ctrl::Continue) => Value::Unit,
            Err(e) => return Err(e),
        };
        self.last_try_return = from_try;
        if let Some(t) = &def.ret {
            if let (Some(tsp), false) = (from_try, self.has_type(&result, &t.ty, false)) {
                let (what, help) = if result.is_option() {
                    ("`None`", "convert the Option first: `.ok_or(\"what went wrong\")?`")
                } else {
                    ("an `Err(..)`", "convert the Result first: `.ok()?`")
                };
                return Err(self.fail(
                    self.diag(
                        tsp,
                        "E0117",
                        format!("`?` returned {} early from `{}`, which is declared to return {}", what, def.display_name(), t.ty),
                    )
                    .label("returns early here")
                    .help(help),
                ));
            }
            if !self.has_type(&result, &t.ty, false) {
                result = self.conform(result, &t.ty).map_err(|m| {
                    self.fail(
                        self.diag(t.span, "E0200", format!("`{}` returned the wrong type: {}", def.display_name(), m))
                            .label("declared return type")
                            .help("this is a bug in the function: it returned a value that does not match its declared return type"),
                    )
                })?;
            }
        }
        if self.contracts && !def.ensures.is_empty() {
            env.locals[def.result_slot as usize] = result.clone();
            for en in &def.ensures {
                match self.eval(en, env)? {
                    Value::Bool(true) => {}
                    Value::Bool(false) => {
                        let wh = self.where_values(en, env);
                        let mut d = self
                            .diag(en.span, "E0302", format!("postcondition of `{}` violated: `{}`", def.display_name(), self.snippet(en.span)))
                            .label("this promise was not kept");
                        if !wh.is_empty() {
                            d = d.note(format!("where {}", wh.join(", ")));
                        }
                        let args: Vec<String> = def
                            .params
                            .iter()
                            .filter(|p| !wh.iter().any(|w| w.starts_with(&format!("{} = ", p.name))))
                            .map(|p| format!("{} = {}", p.name, short_repr(&env.locals[p.slot as usize])))
                            .collect();
                        if !args.is_empty() {
                            d = d.note(format!("called with {}", args.join(", ")));
                        }
                        d = d.help(format!("this is a bug in `{}` (or in its contract)", def.display_name()));
                        return Err(self.fail(d));
                    }
                    other => return Err(self.fail(self.not_bool(en.span, "an `ensures` clause", &other))),
                }
            }
        }
        Ok(result)
    }

    #[allow(clippy::too_many_arguments)]
    fn mutating_call(&mut self, receiver: &Expr, method: &Var, method_span: Span, args: &[Arg], span: Span, decl: Option<&Ty>, env: &mut Env) -> R {
        let (pos, named) = self.eval_args(args, env, None)?;
        let (root, steps) = self.eval_place(receiver, env)?;
        let f = if method.res == VarRes::Unresolved {
            // A mutating function from the module that declared the receiver's type.
            let found = self.root_value(&root, env).and_then(|v| peek_place(v, &steps)).and_then(|v| self.home_method(v, &method.name));
            match found {
                Some(f) => f,
                None => return Err(self.err(method_span, "E0100", format!("undefined function `{}`", method.name))),
            }
        } else {
            self.load(method, span, env)?
        };
        let expected = self.expected_type(&root, &steps, decl, env);
        let mut target = self.with_place(&root, &steps, false, env, receiver.span, std::mem::take)?;
        // While the call runs, the global is unavailable (it has been moved
        // into the call), so reading it gives a clear error instead of `()`.
        let busy = if let PlaceRoot::Global(s) = root {
            let saved = self.globals[s as usize].take();
            self.busy_globals.push((s, method.name.clone()));
            Some((s, saved))
        } else {
            None
        };
        let pre = match (&expected, &target) {
            (Some(t @ Ty::List(_)), Value::List(xs)) => Some((xs.checked() == t.fingerprint(), xs.len())),
            _ => None,
        };
        let result = self.call_mutating(&f, &mut target, pos, named, span);
        let mut type_error = None;
        if let (Ok(_), Some(t)) = (&result, &expected) {
            // push!/extend! only append: if the list was known to match before,
            // only the new elements need checking.
            let appended_ok = match (&pre, &target, t, &f) {
                (Some((true, pre_len)), Value::List(xs), Ty::List(et), Value::Builtin(i))
                    if matches!(BUILTINS[*i as usize].name, "push!" | "extend!") && xs.len() >= *pre_len =>
                {
                    let ok = xs[*pre_len..].iter().all(|x| self.has_type(x, et, false));
                    if ok {
                        xs.set_checked(t.fingerprint());
                    }
                    ok
                }
                _ => false,
            };
            if !appended_ok && !self.has_type(&target, t, false) {
                match self.conform(target.clone(), t) {
                    Ok(v) => target = v,
                    Err(m) => {
                        type_error = Some(
                            self.fail(
                                self.diag(
                                    span,
                                    "E0200",
                                    format!("`{}` broke the declared type of `{}`: {}", method.name, self.snippet(receiver.span), m),
                                )
                                .help("the variable (or field) was declared with a type, and every change must respect it"),
                            ),
                        )
                    }
                }
            }
        }
        if let Some((s, saved)) = busy {
            self.globals[s as usize] = saved;
            self.busy_globals.pop();
        }
        self.with_place(&root, &steps, false, env, receiver.span, |p| *p = target)?;
        if let Some(e) = type_error {
            return Err(e);
        }
        result
    }

    pub fn call_mutating(&mut self, f: &Value, target: &mut Value, args: Vec<Value>, named: Vec<(Name, Value)>, span: Span) -> R {
        match f {
            Value::Builtin(idx) => {
                let b = &BUILTINS[*idx as usize];
                match b.f {
                    BFn::Mut(fp) => {
                        if !named.is_empty() {
                            return Err(self.err(span, "E0108", format!("built-in function `{}` does not take named arguments", b.name)));
                        }
                        self.check_builtin_arity(*idx, args.len() + 1, span)?;
                        fp(self, target, args, span)
                    }
                    BFn::Pure(_) => Err(self.err(span, "E0111", format!("`{}` is not a mutating function", b.name))),
                }
            }
            Value::Func(c) if c.def.mutating => {
                let mut full = Vec::with_capacity(args.len() + 1);
                full.push(std::mem::take(target));
                full.extend(args);
                match self.call_closure_full(c, full, named, span, true) {
                    Ok((r, first)) => {
                        *target = first;
                        Ok(r)
                    }
                    Err(e) => {
                        // Keep whatever the function had done so far, rather than losing the value.
                        if let Some(v) = self.salvaged.take() {
                            *target = v;
                        }
                        Err(e)
                    }
                }
            }
            Value::Overload(cands) => {
                let cands = cands.clone();
                let mut probe = Vec::with_capacity(args.len() + 1);
                probe.push(target.clone());
                probe.extend(args.iter().cloned());
                for c in cands.iter() {
                    let ok = match c {
                        Value::Builtin(i) => matches!(BUILTINS[*i as usize].f, BFn::Mut(_)) && self.accepts(c, &probe, &named),
                        Value::Func(f) => f.def.mutating && self.accepts(c, &probe, &named),
                        _ => false,
                    };
                    if ok {
                        drop(probe);
                        return self.call_mutating(c, target, args, named, span);
                    }
                }
                Err(self.fail(self.no_overload(&cands, &probe, &named, span)))
            }
            other => Err(self.err(span, "E0111", format!("{} is not a mutating function", describe(other)))),
        }
    }

    // ------------------------------------------------------------ entry points

    /// Call a function value with positional arguments (for builtins and tools).
    pub fn call(&mut self, f: &Value, args: Vec<Value>, span: Span) -> R {
        self.call_value(f, args, vec![], span)
    }

    pub fn call_bool(&mut self, f: &Value, args: Vec<Value>, span: Span, what: &str) -> R<bool> {
        match self.call(f, args, span)? {
            Value::Bool(b) => Ok(b),
            other => Err(self.err(span, "E0209", format!("{} must return a Bool, but it returned {}", what, describe(&other)))),
        }
    }
}

/// `+=` on strings and lists appends in place when the value is not shared.
fn append_in_place(op: BinOp, cur: &mut Value, rhs: &Value) -> bool {
    if op != BinOp::Add {
        return false;
    }
    match (cur, rhs) {
        (Value::Str(s), Value::Str(r)) => {
            Rc::make_mut(s).push_str(r);
            true
        }
        (Value::List(xs), Value::List(ys)) => {
            Rc::make_mut(xs).extend(ys.iter().cloned());
            true
        }
        _ => false,
    }
}

/// Follow a path without modifying anything.
fn peek_place<'v>(mut v: &'v Value, steps: &[Step]) -> Option<&'v Value> {
    for st in steps {
        v = match (v, st) {
            (Value::Record(r), Step::Field(n)) => r.get(n)?,
            (Value::Variant(vv), Step::Field(n)) => {
                let (fields, _, _) = vv.ty.fields_of(vv.tag);
                vv.values.get(fields.iter().position(|f| f == n)?)?
            }
            (Value::Tuple(t), Step::Field(n)) => t.get(n.parse::<usize>().ok()?)?,
            (Value::List(xs) | Value::Tuple(xs), Step::Index(Value::Int(i))) => xs.get(norm_index(*i, xs.len())?)?,
            (Value::Map(m), Step::Index(k)) => m.get(k)?,
            _ => return None,
        };
    }
    Some(v)
}

/// The declared type of a field of a nominal record or variant.
fn nominal_field_ty(parent: &Value, step: &Step) -> Option<Ty> {
    let Step::Field(n) = step else { return None };
    let (td, tag) = match parent {
        Value::Record(r) => (r.ty.as_ref()?, 0),
        Value::Variant(v) => (&v.ty, v.tag),
        _ => return None,
    };
    let (fields, tys, _) = td.fields_of(tag);
    let t = tys.get(fields.iter().position(|f| f == n)?)?;
    if t.is_any() || contains_param(t) {
        None
    } else {
        Some(t.clone())
    }
}

fn contains_param(t: &Ty) -> bool {
    match t {
        Ty::Param(..) | Ty::Generic(_) => true,
        Ty::List(x) => contains_param(x),
        Ty::Map(k, v) => contains_param(k) || contains_param(v),
        Ty::Tuple(ts) => ts.iter().any(contains_param),
        Ty::Record(fs) => fs.iter().any(|(_, t)| contains_param(t)),
        Ty::Fn(ps, r) => ps.iter().any(contains_param) || contains_param(r),
        Ty::Named { args, .. } => args.iter().any(contains_param),
        _ => false,
    }
}

fn walk_place<'v>(mut cur: &'v mut Value, steps: &[Step], insert_last: bool) -> Result<&'v mut Value, (&'static str, String)> {
    let n = steps.len();
    for (i, step) in steps.iter().enumerate() {
        cur = step_mut(cur, step, insert_last && i + 1 == n)?;
    }
    Ok(cur)
}

fn step_mut<'v>(v: &'v mut Value, step: &Step, insert: bool) -> Result<&'v mut Value, (&'static str, String)> {
    match step {
        Step::Field(name) => match v {
            Value::Record(r) => {
                let r = Rc::make_mut(r);
                match r.names.iter().position(|n| n == name) {
                    Some(i) => Ok(&mut r.values[i]),
                    None => Err(("E0203", format!("the record has no field `{}`", name))),
                }
            }
            Value::Variant(vv) => {
                let vv = Rc::make_mut(vv);
                let (fields, _, _) = vv.ty.fields_of(vv.tag);
                match fields.iter().position(|n| n == name) {
                    Some(i) => Ok(&mut vv.values[i]),
                    None => Err(("E0203", format!("`{}` has no field `{}`", vv.ty.ctor_name(vv.tag), name))),
                }
            }
            Value::Tuple(t) => {
                let t = Rc::make_mut(t);
                let n = t.len();
                match name.parse::<usize>() {
                    Ok(i) if i < n => Ok(&mut t[i]),
                    _ => Err(("E0204", format!("tuple has no element `{}`", name))),
                }
            }
            other => Err(("E0203", format!("cannot set field `{}` on {}", name, describe(other)))),
        },
        Step::Index(idx) => match v {
            Value::List(xs) => {
                let len = xs.len();
                let Value::Int(i) = idx else {
                    return Err(("E0211", format!("list indexes must be Int, got {}", describe(idx))));
                };
                match norm_index(*i, len) {
                    Some(j) => Ok(&mut Rc::make_mut(xs)[j]),
                    None => Err(("E0204", format!("index {} is out of bounds for a list of length {}", i, len))),
                }
            }
            Value::Map(m) => {
                let m = Rc::make_mut(m);
                if insert && !m.contains(idx) {
                    m.insert(idx.clone(), Value::Unit);
                }
                match m.get_mut(idx) {
                    Some(x) => Ok(x),
                    None => Err(("E0205", format!("key {} not found in map", short_repr(idx)))),
                }
            }
            Value::Tuple(t) => {
                let len = t.len();
                let Value::Int(i) = idx else {
                    return Err(("E0211", format!("tuple indexes must be Int, got {}", describe(idx))));
                };
                match norm_index(*i, len) {
                    Some(j) => Ok(&mut Rc::make_mut(t)[j]),
                    None => Err(("E0204", format!("index {} is out of bounds for a tuple of {} elements", i, len))),
                }
            }
            Value::Str(_) => Err(("E0211", "strings cannot be changed in place; build a new string instead".to_string())),
            other => Err(("E0211", format!("cannot index into {}", describe(other)))),
        },
    }
}

pub fn norm_index(i: i64, len: usize) -> Option<usize> {
    let len = len as i64;
    let j = if i < 0 { i + len } else { i };
    if j >= 0 && j < len {
        Some(j as usize)
    } else {
        None
    }
}

/// Clamp a range to valid slice bounds (negative numbers count from the end).
pub fn slice_bounds(r: &RangeVal, len: usize) -> (usize, usize) {
    let n = len as i64;
    let fix = |x: i64| -> usize {
        let x = if x < 0 { x + n } else { x };
        x.clamp(0, n) as usize
    };
    let a = fix(r.start);
    let b = r.end.map(|e| fix(e.clamp(i64::MIN as i128, i64::MAX as i128) as i64)).unwrap_or(len);
    if b < a {
        (a, a)
    } else {
        (a, b)
    }
}

/// Floor division and modulo for floats, as Python defines them (the
/// remainder has the sign of the divisor, and infinities behave sensibly).
fn float_divmod(x: f64, y: f64) -> (f64, f64) {
    let mut m = x % y;
    let mut d = (x - m) / y;
    if m != 0.0 && ((y < 0.0) != (m < 0.0)) {
        m += y;
        d -= 1.0;
    }
    if m == 0.0 {
        m = 0.0f64.copysign(y);
    }
    let fd = if d != 0.0 {
        let f = d.floor();
        if d - f > 0.5 {
            f + 1.0
        } else {
            f
        }
    } else {
        0.0f64.copysign(x / y)
    };
    (fd, m)
}

/// "1234567.5" -> "1,234,567.5"
fn group_thousands(s: &str) -> String {
    let (sign, rest) = if let Some(r) = s.strip_prefix('-') { ("-", r) } else { ("", s) };
    let (int_part, frac) = match rest.find('.') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let mut out = String::new();
    for (i, c) in int_part.chars().enumerate() {
        if i > 0 && (int_part.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    format!("{}{}{}", sign, out, frac)
}

fn lit_value(l: &Lit) -> Value {
    match l {
        Lit::Unit => Value::Unit,
        Lit::Bool(b) => Value::Bool(*b),
        Lit::Int(i) => Value::Int(*i),
        Lit::Float(f) => Value::Float(*f),
        Lit::Str(s) => Value::Str(s.clone()),
    }
}

fn lit_matches(l: &Lit, v: &Value) -> bool {
    match (l, v) {
        (Lit::Unit, Value::Unit) => true,
        (Lit::Bool(a), Value::Bool(b)) => a == b,
        (Lit::Int(a), Value::Int(b)) => a == b,
        (Lit::Int(a), Value::Float(b)) => (*a as f64) == *b,
        (Lit::Float(a), Value::Float(b)) => a == b,
        (Lit::Float(a), Value::Int(b)) => *a == (*b as f64),
        (Lit::Str(a), Value::Str(b)) => a.as_str() == b.as_str(),
        _ => false,
    }
}

fn collect_vars(e: &Expr, out: &mut Vec<(Name, VarRes)>) {
    let mut push = |v: &Var| {
        if !out.iter().any(|(n, _)| *n == v.name) && v.res != VarRes::Unresolved {
            out.push((v.name.clone(), v.res));
        }
    };
    match &e.kind {
        ExprKind::Var(v) => push(v),
        ExprKind::Interp(parts) => {
            for p in parts {
                if let InterpPart::Expr(x, _) = p {
                    collect_vars(x, out);
                }
            }
        }
        ExprKind::List(items) => items.iter().for_each(|i| collect_vars(&i.expr, out)),
        ExprKind::Tuple(items) => items.iter().for_each(|i| collect_vars(i, out)),
        ExprKind::Map(es) => es.iter().for_each(|(k, v)| {
            collect_vars(k, out);
            collect_vars(v, out)
        }),
        ExprKind::Record { values, .. } => values.iter().for_each(|i| collect_vars(i, out)),
        ExprKind::Field { target, .. } => collect_vars(target, out),
        ExprKind::Index { target, index } => {
            collect_vars(target, out);
            collect_vars(index, out);
        }
        ExprKind::Call { callee, args } => {
            collect_vars(callee, out);
            args.iter().for_each(|a| collect_vars(&a.value, out));
        }
        ExprKind::MethodCall { receiver, args, .. } => {
            collect_vars(receiver, out);
            args.iter().for_each(|a| collect_vars(&a.value, out));
        }
        ExprKind::Unary { expr, .. } | ExprKind::Try(expr) => collect_vars(expr, out),
        ExprKind::Binary { lhs, rhs, .. } | ExprKind::And(lhs, rhs) | ExprKind::Or(lhs, rhs) => {
            collect_vars(lhs, out);
            collect_vars(rhs, out);
        }
        ExprKind::Range { start, end, .. } => {
            collect_vars(start, out);
            if let Some(e) = end {
                collect_vars(e, out);
            }
        }
        ExprKind::If { cond, .. } => collect_vars(cond, out),
        _ => {}
    }
}

trait MapHelp {
    fn map_help(self, help: &str) -> Self;
}

impl MapHelp for Ctrl {
    fn map_help(self, help: &str) -> Self {
        match self {
            Ctrl::Error(mut d) => {
                d.help = Some(help.to_string());
                Ctrl::Error(d)
            }
            other => other,
        }
    }
}
