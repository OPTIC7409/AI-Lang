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
    ty: Option<Ty>,
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
    /// Enclosing loops, innermost last: true for `loop`, false for `while`/`for`.
    loops: Vec<bool>,
    /// The declared return type, if any (used to check `?`).
    ret: Option<Ty>,
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
            loops: Vec::new(),
            ret: None,
        }
    }
}

struct Found {
    res: VarRes,
    mutable: bool,
    span: Span,
    captured: bool,
    is_fn: bool,
    ty: Option<Ty>,
}

#[derive(Clone, Copy, PartialEq)]
enum BindMode {
    Local,
    LocalMut,
    Global,
}

pub struct Resolver<'a> {
    ctx: &'a mut Ctx,
    /// Names of built-in types a declaration tried to redefine (E0102):
    /// patterns written for that declaration are not reported again.
    clashing_types: HashSet<Name>,
    /// The type alias whose definition is being resolved.
    resolving_alias: Option<Name>,
    /// Steps left for the current exhaustiveness check (which is exponential
    /// in the worst case); when they run out, the match is checked at runtime.
    exhaust_steps: std::cell::Cell<u32>,
    /// Set when the exhaustiveness search ran out of steps or depth.
    exhaust_gave_up: std::cell::Cell<bool>,
    /// The fields of the matched value's declared record type, if known.
    scrutinee_fields: Option<Rc<[Name]>>,
    /// Declared types for the parameters of the next function resolved
    /// (an invariant's parameters are the record's fields).
    pending_param_tys: Vec<Ty>,
    ns: Namespace,
    fns: Vec<FnCtx>,
    diags: Vec<Diagnostic>,
    repl: bool,
    dir: PathBuf,
    generics: Vec<Name>,
    pending_methods: Vec<(Name, Span)>,
    /// An import failed: later "undefined name" errors are probably caused by it.
    import_failed: bool,
    /// Bindings of the current or-pattern's first alternative.
    or_bindings: Option<Vec<(Name, VarRes)>>,
    pat_names: Vec<Name>,
}

/// Resolve a program in the given namespace. Returns all diagnostics
/// (errors and warnings).
pub fn resolve_program(ctx: &mut Ctx, prog: &mut Program, ns: &mut Namespace, dir: &Path, repl: bool) -> Vec<Diagnostic> {
    let mut r = Resolver {
        clashing_types: HashSet::new(),
        ctx,
        ns: std::mem::take(ns),
        fns: vec![],
        diags: vec![],
        repl,
        exhaust_steps: std::cell::Cell::new(0),
        exhaust_gave_up: std::cell::Cell::new(false),
        scrutinee_fields: None,
        pending_param_tys: vec![],
        resolving_alias: None,
        dir: dir.to_path_buf(),
        generics: vec![],
        pending_methods: vec![],
        import_failed: false,
        or_bindings: None,
        pat_names: vec![],
    };
    // Every record field name in the file, wherever it appears: a method
    // call `r.count()` may call a field, so its arity cannot be checked.
    let mut fields = HashSet::new();
    for item in &prog.items {
        match item {
            Item::Stmt(s) => stmt_record_fields(s, &mut fields),
            Item::Fn(def) => fn_record_fields(def, &mut fields),
            Item::Test(t) => fn_record_fields(&t.func, &mut fields),
            Item::Property(p) => fn_record_fields(&p.func, &mut fields),
            _ => {}
        }
    }
    r.ctx.known_fields.extend(fields);
    r.program(prog);
    *ns = std::mem::take(&mut r.ns);
    // After a failed import, this file's other errors are mostly about the
    // names the module would have defined: report only the import's.
    if r.import_failed {
        r.diags.retain(|d| !d.is_error() || d.code == "E0114" || d.span.is_some_and(|s| s.file != prog.file));
    }
    if !repl && !r.diags.iter().any(|d| d.is_error()) {
        let mut out = Vec::new();
        for item in &prog.items {
            match item {
                Item::Stmt(s) => unused_in_stmt(s, false, &mut out),
                Item::Fn(def) => unused_values(&def.body, true, false, &mut out),
                // (A test's last line is not its result: a condition there
                // needs `assert`.)
                Item::Test(t) => unused_values(&t.func.body, false, true, &mut out),
                Item::Property(p) => unused_values(&p.func.body, false, true, &mut out),
                _ => {}
            }
        }
        r.diags.extend(out);
    }
    r.diags
}

/// W0008: an expression without calls whose value is computed and then
/// dropped (`- pad` on a line of its own, `y == x + 1` meant as `y = x +
/// 1`, `if c { j + 1 }`). `used` says whether `e`'s value is used.
fn unused_values(e: &Expr, used: bool, in_test: bool, out: &mut Vec<Diagnostic>) {
    match &e.kind {
        ExprKind::Block(stmts) => {
            let n = stmts.len();
            for (i, s) in stmts.iter().enumerate() {
                unused_in_stmt_with(s, used && i + 1 == n, in_test, out);
            }
            return;
        }
        ExprKind::If { cond, then, els } => {
            unused_values(cond, true, in_test, out);
            // (Without `else`, the branch's value is never used.)
            unused_values(then, used && els.is_some(), in_test, out);
            if let Some(x) = els {
                unused_values(x, used, in_test, out);
            }
            return;
        }
        ExprKind::Match { scrutinee, arms } => {
            unused_values(scrutinee, true, in_test, out);
            for a in arms.iter() {
                if let Some(g) = &a.guard {
                    unused_values(g, true, in_test, out);
                }
                unused_values(&a.body, used, in_test, out);
            }
            return;
        }
        ExprKind::While { cond, body } => {
            unused_values(cond, true, in_test, out);
            unused_values(body, false, in_test, out);
            return;
        }
        ExprKind::For { iter, body, .. } => {
            unused_values(iter, true, in_test, out);
            unused_values(body, false, in_test, out);
            return;
        }
        ExprKind::Loop { body } => {
            unused_values(body, false, in_test, out);
            return;
        }
        ExprKind::Lambda(def) => {
            unused_values(&def.body, true, false, out);
            return;
        }
        _ => {}
    }
    // (Only for a value that is not used: `has_calls` walks the subtree.)
    let pure = !used
        && matches!(
            e.kind,
            ExprKind::Int(_)
                | ExprKind::Float(_)
                | ExprKind::Str(_)
                | ExprKind::Bool(_)
                | ExprKind::Var(_)
                | ExprKind::Field { .. }
                | ExprKind::Index { .. }
                | ExprKind::Binary { .. }
                | ExprKind::Unary { .. }
                | ExprKind::And(..)
                | ExprKind::Or(..)
                | ExprKind::Tuple(_)
                | ExprKind::List(_)
        )
        && !has_calls(e);
    if !used && pure {
        let comparison = matches!(&e.kind, ExprKind::Binary { op: BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge, .. });
        let d = match &e.kind {
            ExprKind::Unary { op: UnOp::Neg, .. } => {
                Diagnostic::warning("W0008", "this line's value is not used: a line that starts with `-` is a statement of its own")
                    .at(e.span)
                    .help("to continue the expression above, end that line with the operator (`a -`), or wrap the whole expression in parentheses")
            }
            _ if comparison && in_test => Diagnostic::warning("W0008", "the result of this comparison is not used, so it checks nothing")
                .at(e.span)
                .help("to check it, write `assert` before it"),
            _ if comparison => Diagnostic::warning("W0008", "the result of this comparison is not used")
                .at(e.span)
                .help("to assign, write `=`; to check it, write `assert` before it"),
            _ => Diagnostic::warning("W0008", "the value of this expression is not used")
                .at(e.span)
                .help("to change a variable, assign to it (`x = x + 1` or `x += 1`); otherwise use the value or remove it"),
        };
        out.push(d);
        return;
    }
    for_each_child(e, &mut |c| unused_values(c, true, in_test, out));
}

fn unused_in_stmt(s: &Stmt, used: bool, out: &mut Vec<Diagnostic>) {
    unused_in_stmt_with(s, used, false, out)
}

fn unused_in_stmt_with(s: &Stmt, used: bool, in_test: bool, out: &mut Vec<Diagnostic>) {
    match &s.kind {
        StmtKind::Expr(x) => unused_values(x, used, in_test, out),
        StmtKind::Let { value, .. } => unused_values(value, true, in_test, out),
        StmtKind::Assign { value, .. } => unused_values(value, true, in_test, out),
        StmtKind::Assert { cond, msg } => {
            unused_values(cond, true, in_test, out);
            if let Some(m) = msg {
                unused_values(m, true, in_test, out);
            }
        }
        StmtKind::Fn { def, .. } => unused_values(&def.body, true, false, out),
    }
}

fn fn_record_fields(def: &FnDef, out: &mut HashSet<Name>) {
    record_fields(&def.body, out);
    def.requires.iter().chain(&def.ensures).for_each(|e| record_fields(e, out));
}

fn stmt_record_fields(s: &Stmt, out: &mut HashSet<Name>) {
    match &s.kind {
        StmtKind::Let { value, .. } => record_fields(value, out),
        StmtKind::Assign { target, value, .. } => {
            record_fields(target, out);
            record_fields(value, out);
        }
        StmtKind::Fn { def, .. } => fn_record_fields(def, out),
        StmtKind::Assert { cond, msg } => {
            record_fields(cond, out);
            if let Some(m) = msg {
                record_fields(m, out);
            }
        }
        StmtKind::Expr(e) => record_fields(e, out),
    }
}

fn record_fields(e: &Expr, out: &mut HashSet<Name>) {
    match &e.kind {
        ExprKind::Record { names, .. } => out.extend(names.iter().cloned()),
        // (`for_each_child` does not enter local functions.)
        ExprKind::Block(stmts) => {
            for s in stmts {
                if let StmtKind::Fn { def, .. } = &s.kind {
                    fn_record_fields(def, out);
                }
            }
        }
        _ => {}
    }
    for_each_child(e, &mut |c| record_fields(c, out));
}

fn confusion_hint(name: &str) -> Option<&'static str> {
    Some(match name {
        "null" | "nil" | "undefined" | "none" | "NULL" => "Cogito has no null; use `None` (an Option) for a missing value",
        "self" | "this" => {
            "Cogito has no implicit receiver; write ordinary functions whose first parameter is the value, then call them as `value.func()`"
        }
        "True" | "False" | "TRUE" | "FALSE" => "booleans are lowercase: `true` and `false`",
        "println" | "puts" | "printf" | "echo" | "console" | "printLn" | "say" => "use `print(...)`",
        "elif" | "elsif" | "elseif" => "write `else if`",
        "function" | "def" | "func" | "fun" | "proc" => "functions are declared with `fn`",
        "const" | "final" | "val" => "use `let` for a binding that never changes (all `let` bindings are immutable)",
        "lambda" => "anonymous functions are written `fn(x) => x + 1`",
        "length" | "size" => "use `len(x)` or `x.len()`",
        "append" => "use `push` (returns a new list) or `push!` (changes a `var` in place)",
        "switch" | "case" | "when" => "use `match value { pattern => result }`",
        "nan" | "NaN" => "Cogito has no NaN constant: Float division by zero is an error, so NaN only comes from operations like `inf - inf`; test for it with `is_nan(x)`",
        "string" | "String" => "the string type is `Str`; to convert a value use `str(x)`",
        "integer" | "Integer" => "the integer type is `Int`; to convert a value use `int(x)`",
        "new" => "values are built by calling their type: `Point(x: 1, y: 2)`",
        "throw" | "raise" => "errors are values: return `Err(...)`, or call `panic(message)` for bugs",
        "try" | "catch" => "errors are values: use `match` on a Result, or the `?` operator to propagate `Err`",
        "to_upper" | "toUpperCase" | "uppercase" | "to_uppercase" | "upcase" => "use `upper`: `s.upper()`",
        "to_lower" | "toLowerCase" | "lowercase" | "to_lowercase" | "downcase" => "use `lower`: `s.lower()`",
        "trim_left" | "ltrim" | "lstrip" | "trimStart" | "trim_start_matches" => "use `trim_start` (or `strip_prefix` to remove a given prefix)",
        "trim_right" | "rtrim" | "rstrip" | "trimEnd" | "trim_end_matches" => "use `trim_end` (or `strip_suffix` to remove a given suffix)",
        "strip" => "use `trim` (whitespace), or `strip_prefix` / `strip_suffix`",
        "substring" | "substr" | "subList" | "sublist" => "use `slice(start, end)` or indexing with a range: `s[1..4]`",
        "indexOf" | "find_str" | "search" => "use `index_of(x)`, which returns an Option",
        "includes" | "contains_key" | "has_key" | "containsKey" => "use `contains` (lists, strings) or `has` (map keys)",
        "startswith" | "startsWith" => "use `starts_with`",
        "endswith" | "endsWith" => "use `ends_with`",
        "to_string" | "toString" | "to_str" | "as_str" | "String.valueOf" => "use `str(x)` (or interpolation: `\"{x}\"`)",
        "parse" | "to_int" | "atoi" | "parseInt" | "Int.parse" => "use `parse_int(s)`, which returns an Option",
        "to_float" | "parseFloat" | "atof" => "use `parse_float(s)` (returns an Option) or `float(n)` to convert a number",
        "split_whitespace" => "use `words`",
        "foreach" | "for_each" | "forEach" | "iter" => "use a `for` loop, or `each(f)` to call a function on every element",
        "reversed" => "use `reverse` (a new value) or `reverse!` (in place)",
        "sorted" => "use `sort` (a new value) or `sort!` (in place)",
        "items" | "iteritems" | "pairs" => "use `entries` (a list of `(key, value)` tuples), or `for (k, v) in m`",
        "extend" | "concat" | "append_all" => "use `xs + ys` (a new list) or `extend!` (in place)",
        "pop" | "pop_back" => "use `pop!`, which removes the last element of a `var` and returns it as an Option",
        "del" | "delete" | "erase" | "discard" => "use `remove(key_or_index)` (a new value) or `remove!` (in place)",
        "assert_eq" | "assertEqual" | "assert_equal" | "expect_eq" => "use `assert a == b`; a failure shows both sides",
        "isEmpty" | "empty" => "use `is_empty`",
        "drop_last" | "but_last" | "init" | "dropLast" => "slice off the end: `xs[..-1]` is all but the last element (`xs[..-n]` all but the last n)",
        "take_last" | "takeLast" => "slice from the end: `xs[-n..]` is the last n elements",
        "charAt" | "char_at" | "nth" => "index with `s[i]` (or `xs.get(i)`, which returns an Option)",
        "format" | "sprintf" | "fmt" => "use string interpolation with a format spec: `\"{x:.2} {name:>10}\"`",
        "filter_map" | "filterMap" | "compact_map" => "use a comprehension: `[f(x) for x in xs if keep(x)]`, or `collect_some`",
        "Vec" | "vec" | "array" | "Array" | "list" => "lists are written `[1, 2, 3]`; the type is `List[Int]`",
        "HashSet" | "Set" | "TreeSet" | "frozenset" | "set_new" => "a set is built with `to_set(xs)` (empty: `to_set()`); the type is `Set[Int]`",
        "HashMap" | "dict" | "Dict" | "hashmap" | "map_new" => "maps are written `[\"a\": 1]` (empty: `[:]`); the type is `Map[Str, Int]`",
        "set!" | "put!" | "update!" | "replace!" => "assign directly: `xs[i] = v` or `m[k] = v` (the non-mutating `set(xs, i, v)` returns a copy)",
        "mod" | "rem" => "use the `%` operator (the result takes the sign of the divisor)",
        "div" | "floor_div" | "idiv" => "use the `//` operator (floor division); `/` always gives a Float",
        _ => return None,
    })
}

/// The exact replacement for a name from another language, when there is
/// one: `null` is `None`. Names of functions (`length` is `len`) are only
/// replaced where they are called (`call`).
fn confusion_fix(name: &str, call: bool) -> Option<&'static str> {
    let value = match name {
        "null" | "nil" | "undefined" | "none" | "NULL" => Some("None"),
        "True" | "TRUE" => Some("true"),
        "False" | "FALSE" => Some("false"),
        _ => None,
    };
    if value.is_some() || !call {
        return value;
    }
    Some(match name {
        "println" | "puts" | "printLn" | "say" => "print",
        "length" | "size" => "len",
        "to_upper" | "toUpperCase" | "uppercase" | "to_uppercase" | "upcase" => "upper",
        "to_lower" | "toLowerCase" | "lowercase" | "to_lowercase" | "downcase" => "lower",
        "trim_left" | "ltrim" | "lstrip" | "trimStart" => "trim_start",
        "trim_right" | "rtrim" | "rstrip" | "trimEnd" => "trim_end",
        "startswith" | "startsWith" => "starts_with",
        "endswith" | "endsWith" => "ends_with",
        "indexOf" => "index_of",
        "includes" => "contains",
        "contains_key" | "has_key" | "containsKey" => "has",
        "to_string" | "toString" | "to_str" => "str",
        "split_whitespace" => "words",
        "reversed" => "reverse",
        "sorted" => "sort",
        "items" | "iteritems" => "entries",
        "isEmpty" | "empty" => "is_empty",
        "parseInt" | "atoi" => "parse_int",
        "parseFloat" | "atof" => "parse_float",
        _ => return None,
    })
}

impl<'a> Resolver<'a> {
    // ------------------------------------------------------------ utilities

    /// Whether `v` names a variable declared with `var` (and not captured).
    fn is_var(&self, v: &Var) -> bool {
        match v.res {
            VarRes::Local(s) => {
                self.fns.last().is_some_and(|f| f.scopes.iter().flat_map(|sc| sc.locals.iter()).any(|l| l.slot == s && l.name == v.name && l.mutable))
            }
            VarRes::Global(s) => matches!(self.ctx.globals[s as usize].kind, GlobalKind::Var),
            _ => false,
        }
    }

    /// The span of the whole line holding `span` (with its newline), when
    /// nothing else is on it; otherwise `span` itself.
    fn whole_line(&self, span: Span) -> Span {
        let src = &self.ctx.sm.get(span.file).src;
        let (s, e) = (span.start as usize, span.end as usize);
        let start = src[..s].rfind('\n').map_or(0, |i| i + 1);
        let end = src[e..].find('\n').map_or(src.len(), |i| e + i + 1);
        if src[start..s].trim().is_empty() && src[e..end].trim().is_empty() {
            Span::new(span.file, start, end)
        } else {
            span
        }
    }

    fn error(&mut self, d: Diagnostic) {
        self.diags.push(d);
    }

    /// Resolve a contract clause. For one already reported as changing
    /// something (E0118), errors about what it changes would only repeat it.
    fn contract_expr(&mut self, e: &mut Expr, effectful: bool) {
        let n = self.diags.len();
        self.expr(e);
        if effectful {
            let mut k = n;
            while k < self.diags.len() {
                if matches!(self.diags[k].code, "E0110" | "E0111") {
                    self.diags.remove(k);
                } else {
                    k += 1;
                }
            }
        }
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
        f.scopes.last_mut().unwrap().locals.push(Local { name, slot, mutable, used: false, span, kind, ty: None });
        slot
    }

    fn at_global_scope(&self) -> bool {
        self.fns.len() == 1 && self.fns[0].kind == FnKind::TopLevel && self.fns[0].scopes.len() == 1
    }

    /// Whether `name` is a parameter of the current function (and not
    /// shadowed by a local).
    fn is_param(&self, name: &str) -> bool {
        let f = self.fns.last().unwrap();
        let local = f.scopes.iter().rev().flat_map(|s| s.locals.iter().rev()).find(|l| &*l.name == name);
        local.is_some_and(|l| l.kind == LocalKind::Param)
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
                            ty: local.ty.clone(),
                        });
                    }
                }
            }
            if f.self_name.as_deref() == Some(name) {
                return Some(Found { res: VarRes::SelfFn, mutable: false, span: Span::default(), captured: false, is_fn: true, ty: None });
            }
            if let Some(i) = f.captures.iter().position(|c| &*c.name == name) {
                let c = &f.captures[i];
                return Some(Found { res: VarRes::Capture(i as u32), mutable: c.mutable, span: c.span, captured: true, is_fn: false, ty: None });
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
            ty: None,
        })
    }

    /// Which of Option/Result a call returns, when that is known statically.
    fn static_result_kind(&self, e: &Expr) -> Option<u32> {
        let slot = match &e.kind {
            ExprKind::Call { callee, .. } => match &callee.kind {
                ExprKind::Var(Var { res: VarRes::Global(s), .. }) => *s,
                _ => return None,
            },
            // A function of the same name in an imported module may answer.
            ExprKind::MethodCall { method, .. } if self.ctx.module_fns.contains(&method.name) => return None,
            ExprKind::MethodCall { method: Var { res: VarRes::Global(s), .. }, .. } => *s,
            _ => return None,
        };
        let kind_of = |t: &Ty| match t {
            Ty::Named { id, .. } if *id == crate::types::OPTION_ID || *id == crate::types::RESULT_ID => Some(*id),
            _ => None,
        };
        let builtin_kind = |i: u16| {
            let sig = crate::builtins::BUILTINS[i as usize].doc.lines().next().unwrap_or("");
            match (sig.contains("-> Option"), sig.contains("-> Result")) {
                (true, false) => Some(crate::types::OPTION_ID),
                (false, true) => Some(crate::types::RESULT_ID),
                _ => None,
            }
        };
        let info = &self.ctx.globals[slot as usize];
        match &info.kind {
            GlobalKind::Builtin(i) => builtin_kind(*i),
            GlobalKind::Fn => {
                let sigs = self.ctx.sigs.get(&slot)?;
                let mut kinds: Vec<Option<u32>> = sigs.iter().map(|s| s.ret.as_ref().and_then(kind_of)).collect();
                // A user function named like a built-in falls back to it.
                if let Some(&b) = self.ctx.builtins.values.get(&info.name) {
                    if let GlobalKind::Builtin(i) = self.ctx.globals[b as usize].kind {
                        kinds.push(builtin_kind(i));
                    }
                }
                if kinds.iter().all(|k| *k == kinds[0]) {
                    kinds[0]
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// `?` on an Option inside a function returning a Result (or vice versa)
    /// cannot work; report it before the program runs.
    fn check_try_kinds(&mut self, inner: &Expr, span: Span) {
        let fn_kind = match &self.cur().ret {
            Some(Ty::Named { id, .. }) if *id == crate::types::OPTION_ID || *id == crate::types::RESULT_ID => *id,
            _ => return,
        };
        let Some(inner_kind) = self.static_result_kind(inner) else { return };
        if inner_kind == fn_kind {
            return;
        }
        let d = if inner_kind == crate::types::OPTION_ID {
            Diagnostic::error("E0117", "`?` on an Option inside a function that returns a Result")
                .at(span)
                .label("this would return `None`, which is not a Result")
                .help("convert the Option first: `.ok_or(\"what went wrong\")?`")
        } else {
            Diagnostic::error("E0117", "`?` on a Result inside a function that returns an Option")
                .at(span)
                .label("this would return `Err(..)`, which is not an Option")
                .help("convert the Result first: `.ok()?`")
        };
        self.error(d);
    }

    /// The global slot of a function (or constructor) with this name, if any.
    fn function_slot(&self, name: &str) -> Option<u32> {
        let is_fn = |slot: u32| matches!(self.ctx.globals[slot as usize].kind, GlobalKind::Fn | GlobalKind::Builtin(_) | GlobalKind::Ctor(_));
        if let Some(&s) = self.ns.values.get(name) {
            if is_fn(s) {
                return Some(s);
            }
        }
        self.ctx.builtins.values.get(name).copied().filter(|s| is_fn(*s))
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
            ty: info.ty.clone(),
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
        out.retain(|n| n.chars().next().is_some_and(|c| c.is_ascii_uppercase()) == upper);
        out.sort();
        out.dedup();
        out
    }

    fn undefined(&mut self, name: &str, span: Span, what: &str) {
        if self.import_failed {
            return;
        }
        let upper = name.chars().next().is_some_and(|c| c.is_ascii_uppercase());
        let names = self.visible_names(upper);
        let mut d = Diagnostic::error("E0100", format!("undefined {} `{}`", what, name)).at(span).label("not found in this scope");
        if name == "result" {
            d = Diagnostic::error("E0100", "`result` is only defined in `ensures` clauses")
                .at(span)
                .help("in a postcondition, `result` is the value the function returns: `ensures result >= 0`");
        } else if name == "old" {
            d = Diagnostic::error("E0100", "`old(...)` can only be used in `ensures` clauses")
                .at(span)
                .help("in a postcondition, `old(e)` is the value `e` had when the function was called");
        } else if let Some((alias, member)) = name.split_once('.') {
            // `shapes.Circel`: the names the module does define.
            let m = self.ns.values.get(alias).and_then(|s| match &self.ctx.globals[*s as usize].kind {
                GlobalKind::Module(m) => Some(m.clone()),
                _ => None,
            });
            if let Some(m) = m {
                let mut names: Vec<&str> =
                    m.ns.values
                        .keys()
                        .map(|k| &**k)
                        .filter(|k| k.starts_with(char::is_uppercase) == member.starts_with(char::is_uppercase))
                        .collect();
                names.sort();
                if let Some(s) = suggest(member, names) {
                    let full = format!("{}.{}", alias, s);
                    d = d.help(format!("did you mean `{}`?", full));
                }
            }
        } else if let Some(alias) = self.module_with(name) {
            d = d.help(format!("`{}` is defined in the imported module `{}`: write `{}.{}`", name, alias, alias, name));
            if self.modules_with(name) == 1 {
                d = d.fix(span, format!("{}.{}", alias, name));
            }
        } else if let Some(h) = confusion_hint(name) {
            d = d.help(h);
            if let Some(r) = confusion_fix(name, what == "function") {
                d = d.fix(span, r);
            }
        } else if let Some(s) = suggest(name, names.iter().map(|s| s.as_str())) {
            d = d.help(format!("did you mean `{}`?", s));
        } else if upper && self.ns.types.contains_key(name) {
            d = d.help(format!("`{}` is a type; build values with one of its constructors", name));
        }
        self.error(d);
    }

    /// How many imported modules define `name`.
    fn modules_with(&self, name: &str) -> usize {
        self.ns
            .values
            .values()
            .filter(|&&slot| matches!(&self.ctx.globals[slot as usize].kind, GlobalKind::Module(m) if m.ns.values.contains_key(name) || m.ns.types.contains_key(name)))
            .count()
    }

    /// The alias of an imported module that defines `name` (a value or type).
    fn module_with(&self, name: &str) -> Option<Name> {
        let mut aliases: Vec<&Name> = self.ns.values.keys().collect();
        aliases.sort();
        aliases.into_iter().find_map(|alias| match &self.ctx.globals[self.ns.values[alias] as usize].kind {
            GlobalKind::Module(m) if m.ns.values.contains_key(name) || m.ns.types.contains_key(name) => Some(alias.clone()),
            _ => None,
        })
    }

    fn resolve_var(&mut self, v: &mut Var, span: Span) -> Option<Found> {
        match self.lookup(&v.name) {
            Some(found) => {
                if let VarRes::Global(slot) = found.res {
                    let info = &self.ctx.globals[slot as usize];
                    if !info.declared && self.fns.last().is_some_and(|f| f.kind == FnKind::TopLevel) {
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
                let what = if v.name.chars().next().is_some_and(|c| c.is_ascii_uppercase()) { "constructor" } else { "name" };
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
                let d = if matches!(kind, GlobalKind::Let) { d.help("use `var` for a value that changes, or choose a different name") } else { d };
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
        // Pass 0: load imported modules first; they add their own types to the
        // registry, so this program's type ids must be assigned afterwards.
        for item in prog.items.iter_mut() {
            if let Item::Import(imp) = item {
                self.import(imp);
            }
        }
        // Pass 1: register every top-level name, so that functions can refer
        // to each other (and to types) regardless of order.
        let first_new_type = self.ctx.types.len() as u32;
        let mut type_decl_count = 0u32;
        // `type A = B` with a single bare name that is already a type is an alias.
        let program_types: HashSet<Name> = prog
            .items
            .iter()
            .filter_map(|i| match i {
                Item::Type(td) => Some(td.name.clone()),
                _ => None,
            })
            .collect();
        for item in prog.items.iter_mut() {
            if let Item::Type(td) = item {
                if let TypeBody::Enum(vs) = &td.body {
                    if vs.len() == 1 && !vs[0].has_parens {
                        let n = vs[0].name.clone();
                        if program_types.contains(&n) || self.ns.types.contains_key(&n) || self.ns.aliases.contains_key(&n) {
                            let span = vs[0].span;
                            td.body = TypeBody::Alias(TypeExpr { kind: TypeExprKind::Named(n, vec![]), span, ty: Ty::Any });
                        }
                    }
                }
            }
        }
        let mut type_spans: std::collections::HashMap<Name, Span> = std::collections::HashMap::new();
        for item in prog.items.iter_mut() {
            match item {
                Item::Type(td) if matches!(td.body, TypeBody::Alias(_)) => {
                    if (self.ns.types.contains_key(&td.name) || self.ns.aliases.contains_key(&td.name)) && !self.repl {
                        let d = Diagnostic::error("E0102", format!("type `{}` is defined more than once", td.name)).at(td.name_span);
                        self.error(d);
                    }
                    if self.ctx.builtins.types.contains_key(&td.name) || is_primitive_type(&td.name) {
                        let d = Diagnostic::error("E0102", format!("`{}` is a built-in type and cannot be redefined", td.name)).at(td.name_span);
                        self.clashing_types.insert(td.name.clone());
                        self.error(d);
                    }
                    self.ns.types.remove(&td.name);
                }
                Item::Type(td) => {
                    let id = first_new_type + type_decl_count;
                    type_decl_count += 1;
                    td.id = id;
                    if let Some(&old) = self.ns.types.get(&td.name) {
                        if !self.repl {
                            let prev = type_spans
                                .get(&td.name)
                                .copied()
                                .unwrap_or_else(|| self.ctx.types.get(old as usize).map(|t| t.span).unwrap_or_default());
                            let d = Diagnostic::error("E0102", format!("type `{}` is defined more than once", td.name))
                                .at(td.name_span)
                                .note(format!("first defined at {}", self.line_of(prev)));
                            self.error(d);
                        }
                    }
                    if self.ctx.builtins.types.contains_key(&td.name) || is_primitive_type(&td.name) {
                        let d = Diagnostic::error("E0102", format!("`{}` is a built-in type and cannot be redefined", td.name)).at(td.name_span);
                        self.clashing_types.insert(td.name.clone());
                        self.error(d);
                    }
                    self.ns.types.insert(td.name.clone(), id);
                    self.ns.aliases.remove(&td.name);
                    type_spans.entry(td.name.clone()).or_insert(td.name_span);
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
                                    let d = Diagnostic::error("E0102", format!("`{}` is a built-in constructor and cannot be redefined", v.name))
                                        .at(v.span);
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
                        TypeBody::Alias(_) => unreachable!(),
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
                Item::Stmt(Stmt { kind: StmtKind::Let { pat, mutable, .. }, .. }) => {
                    let mut names = vec![];
                    Self::bound_names(pat, &mut names);
                    let kind = if *mutable { GlobalKind::Var } else { GlobalKind::Let };
                    for (n, sp) in names {
                        self.define_global(n, kind.clone(), sp, false);
                    }
                }
                _ => {}
            }
        }

        // Pass 2: resolve aliases (in order), then build type definitions.
        for item in prog.items.iter_mut() {
            if let Item::Type(td) = item {
                if let TypeBody::Alias(te) = &mut td.body {
                    let params = td.params.clone();
                    self.resolving_alias = Some(td.name.clone());
                    let ty = self.resolve_type(te, &params);
                    self.resolving_alias = None;
                    self.ns.aliases.insert(td.name.clone(), AliasDef { params, ty });
                }
            }
        }
        let mut new_types = Vec::new();
        for item in prog.items.iter_mut() {
            if let Item::Type(td) = item {
                if !matches!(td.body, TypeBody::Alias(_)) {
                    new_types.push(self.type_decl(td));
                }
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
                    ret: def.ret.as_ref().map(|t| t.ty.clone()),
                    span: def.name_span,
                };
                let slot = def.global_slot.unwrap();
                if let Some((_, prev)) = seen_sigs.iter().find(|(s, sg)| *s == slot && sg.same_types(&sig)) {
                    let d = Diagnostic::error(
                        "E0102",
                        format!("function `{}` is defined more than once with the same parameter types", def.name.as_ref().unwrap()),
                    )
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
        let n_items = prog.items.len();
        for (idx, item) in prog.items.iter_mut().enumerate() {
            let last_item = idx + 1 == n_items;
            match item {
                Item::Fn(def) => {
                    // Top-level functions refer to themselves through their
                    // global slot, so that recursion respects overloading.
                    let def = Rc::get_mut(def).unwrap();
                    self.resolve_fn(def, FnKind::Function, false, None);
                    self.check_hidden_builtin(def);
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
                                .help(format!(
                                    "Cogito generates random inputs from the type: write `{}: Int`, `{}: List[Str]`, ...",
                                    param.name, param.name
                                ));
                            self.diags.push(d);
                        }
                    }
                    self.resolve_fn(def, FnKind::Test, false, None);
                }
                Item::Stmt(s) => {
                    self.stmt(s);
                    if let (false, StmtKind::Expr(e)) = (self.repl || last_item, &s.kind) {
                        self.check_discarded(e);
                    }
                    // `main()` at the bottom of the file, as in Python: `main`
                    // already runs after the top-level statements.
                    if let StmtKind::Expr(Expr { kind: ExprKind::Call { callee, args }, span }) = &s.kind {
                        let main_fn = self.ns.values.get("main").is_some_and(|&slot| matches!(self.ctx.globals[slot as usize].kind, GlobalKind::Fn));
                        if !self.repl && args.is_empty() && main_fn && matches!(&callee.kind, ExprKind::Var(v) if &*v.name == "main") {
                            let d = Diagnostic::warning("W0007", "`main` runs twice: here, and again after the top-level statements")
                                .at(*span)
                                .help("`fn main()` is called automatically; remove this call")
                                .fix(self.whole_line(*span), "");
                            self.diags.push(d);
                        }
                    }
                }
                Item::Type(td) if !td.invariants.is_empty() => self.invariant(td),
                Item::Type(_) | Item::Import(_) => {}
            }
        }
        let top = self.fns.pop().unwrap();
        prog.num_slots = prog.num_slots.max(top.max_slot);

        // Method calls on names that are not functions are only valid if some
        // record has a field with that name.
        let pending = std::mem::take(&mut self.pending_methods);
        for (name, span) in pending {
            if !self.ctx.known_fields.contains(&name) && !self.ctx.module_fns.contains(&name) && self.global_slot(&name).is_none() {
                let before = self.diags.len();
                self.undefined(&name, span, "function");
                if self.diags.len() > before {
                    if let Some(d) = self.diags.last_mut() {
                        d.notes.push(format!("`value.{}(...)` calls the function `{}` with `value` as its first argument", name, name));
                    }
                }
            }
        }
    }

    fn import(&mut self, imp: &mut ImportDecl) {
        let errors_before = self.diags.iter().filter(|d| d.is_error()).count();
        self.import_inner(imp);
        if self.diags.iter().filter(|d| d.is_error()).count() > errors_before {
            self.import_failed = true;
        }
    }

    fn import_inner(&mut self, imp: &mut ImportDecl) {
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
                let valid = stem.chars().next().is_some_and(|c| c.is_ascii_lowercase() || c == '_')
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
        } else if self.ctx.failed_modules.contains(&canon) {
            // Its errors were reported where it was first imported.
            self.error(Diagnostic::error("E0114", format!("module `{}` has errors (shown above)", imp.path)).at(imp.path_span));
            return;
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
            // Shown relative to the current directory when it is inside it.
            let cwd = std::env::current_dir().ok().and_then(|d| d.canonicalize().ok());
            let display =
                cwd.and_then(|d| canon.strip_prefix(d).ok().map(|p| p.display().to_string())).unwrap_or_else(|| canon.display().to_string());
            let file = self.ctx.sm.add(display, src.clone());
            let mut prog = match parse_program(&src, file) {
                Ok(p) => p,
                Err(d) => {
                    self.error(d);
                    self.error(Diagnostic::error("E0114", format!("module `{}` has syntax errors", imp.path)).at(imp.path_span));
                    self.ctx.failed_modules.insert(canon);
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
            // The module's own code is type-checked too.
            let type_errors: Vec<Diagnostic> = if has_errors { vec![] } else { crate::typecheck::check_program(self.ctx, &prog) };
            let has_errors = has_errors || !type_errors.is_empty();
            self.diags.extend(type_errors);
            if has_errors {
                self.error(Diagnostic::error("E0114", format!("module `{}` has errors", imp.path)).at(imp.path_span));
                self.ctx.failed_modules.insert(canon);
                return;
            }
            let m = Rc::new(Module { name: alias.clone(), path: canon.clone(), program: prog, ns, executed: std::cell::Cell::new(false) });
            for id in m.ns.types.values() {
                self.ctx.type_home.insert(*id, m.clone());
            }
            for (name, slot) in m.ns.values.iter() {
                if matches!(self.ctx.globals[*slot as usize].kind, GlobalKind::Fn) {
                    self.ctx.module_fns.insert(name.clone());
                }
            }
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
            TypeBody::Alias(_) => unreachable!(),
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

    fn lookup_alias(&self, name: &str) -> Option<AliasDef> {
        if let Some((module, tname)) = name.split_once('.') {
            let slot = self.ns.values.get(module)?;
            if let GlobalKind::Module(m) = &self.ctx.globals[*slot as usize].kind {
                return m.ns.aliases.get(tname).cloned();
            }
            return None;
        }
        self.ns.aliases.get(name).cloned()
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
                    let d = Diagnostic::error(
                        "E0106",
                        format!("`{}` takes {} type argument{}, but {} were given", name, want, if want == 1 { "" } else { "s" }, targs.len()),
                    )
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
                    "Set" => {
                        if targs.len() > 1 {
                            arity_err(self, 1);
                        }
                        Ty::Set(Box::new(targs.pop().unwrap_or(Ty::Any)))
                    }
                    "Fn" => Ty::Fn(vec![], Box::new(Ty::Any)),
                    _ => {
                        if let Some(i) = type_params.iter().position(|p| *p == name) {
                            Ty::Param(i as u32, name.clone())
                        } else if self.generics.contains(&name) {
                            Ty::Generic(name.clone())
                        } else if let Some(alias) = self.lookup_alias(&name) {
                            if !targs.is_empty() && targs.len() != alias.params.len() {
                                arity_err(self, alias.params.len());
                            }
                            alias.ty.subst(&targs)
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
                            let mut cands: Vec<String> = vec!["Int", "Float", "Str", "Bool", "Unit", "Any", "List", "Map", "Set", "Range"]
                                .into_iter()
                                .map(String::from)
                                .collect();
                            cands.extend(self.ns.types.keys().map(|k| k.to_string()));
                            cands.extend(self.ns.aliases.keys().map(|k| k.to_string()));
                            cands.extend(self.ctx.builtins.types.keys().map(|k| k.to_string()));
                            cands.extend(self.generics.iter().map(|k| k.to_string()));
                            let mut d = Diagnostic::error("E0106", format!("unknown type `{}`", name)).at(span);
                            if self.resolving_alias.as_deref() == Some(&*name) {
                                d = Diagnostic::error("E0106", format!("the type alias `{}` refers to itself", name))
                                    .at(span)
                                    .help(format!("an alias is only another name for an existing type; for a recursive type, declare an enum or record: `type {} = | Leaf | Node(List[{}])`", name, name));
                            } else if let Some(h) = crate::parser::type_name_hint(&name) {
                                // (The span covers type arguments too: `HashMap[Str, Int]`.)
                                let name_span = Span::new(span.file, span.start as usize, span.start as usize + name.len());
                                d = d.help(format!("did you mean `{}`?", h));
                                if self.ctx.sm.snippet(name_span) == &*name {
                                    d = d.fix(name_span, h);
                                }
                            } else if let Some(s) = suggest(&name, cands.iter().map(|s| s.as_str())) {
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

    /// A record type's `where` clauses become a function of its fields (with
    /// the clauses as its preconditions), which the interpreter calls to
    /// check every value of the type.
    fn invariant(&mut self, td: &mut TypeDecl) {
        let TypeBody::Record(fields) = &td.body else { return };
        let span = td.span;
        let params =
            fields.iter().map(|f| Param { name: f.name.clone().unwrap(), span: f.span, ty: None, default: None, slot: 0, pat: None }).collect();
        let field_tys: Vec<Ty> = match self.ctx.types.get(td.id as usize).map(|t| &t.kind) {
            Some(TypeKind::Record { tys, .. }) => tys.clone(),
            _ => vec![],
        };
        let mut def = FnDef {
            name: Some(Rc::from(format!("invariant of {}", td.name).as_str())),
            name_span: td.name_span,
            span,
            generics: td.params.clone(),
            params,
            ret: None,
            requires: std::mem::take(&mut td.invariants),
            ensures: vec![],
            body: Expr { kind: ExprKind::Unit, span },
            mutating: false,
            num_slots: 0,
            captures: vec![],
            result_slot: 0,
            olds: vec![],
            global_slot: None,
            overload_fallback: None,
        };
        // `self.lo` is a natural guess from other languages.
        for c in &def.requires {
            if let Some(sp) = find_var(c, &["self", "this"]) {
                let d = Diagnostic::error("E0100", "a `where` clause refers to the fields by name")
                    .at(sp)
                    .help(format!("write the condition on the fields directly, e.g. `where {} >= 0`", def.params.first().map_or("x", |p| &*p.name)));
                self.error(d);
                return;
            }
        }
        // (The fields' declared types, for checks such as E0119.)
        let field_tys = if td.params.is_empty() { field_tys } else { vec![] };
        self.resolve_fn_with(&mut def, FnKind::Function, &field_tys);
        self.ctx.invariants.insert(td.id, Rc::new(def));
    }

    fn resolve_fn_with(&mut self, def: &mut FnDef, kind: FnKind, param_tys: &[Ty]) {
        self.pending_param_tys = param_tys.to_vec();
        self.resolve_fn(def, kind, false, None);
        self.pending_param_tys.clear();
    }

    fn resolve_fn(&mut self, def: &mut FnDef, kind: FnKind, parent_visible: bool, self_name: Option<Name>) {
        let sig_done = kind == FnKind::Function && !parent_visible;
        let saved_generics = self.generics.clone();
        self.generics.extend(def.generics.iter().cloned());
        self.fns.push(FnCtx::new(kind, parent_visible, self_name));
        self.cur().ret = def.ret.as_ref().map(|t| t.ty.clone());
        let mut seen: Vec<Name> = Vec::new();
        let mutating = def.mutating;
        if def.params.len() > 64 {
            let d = Diagnostic::error("E0116", format!("`{}` has {} parameters; the maximum is 64", def.display_name(), def.params.len()))
                .at(def.name_span)
                .help("group related parameters into a record");
            self.error(d);
        }
        if mutating && def.params.is_empty() {
            let d = Diagnostic::error(
                "E0111",
                format!("mutating function `{}` must take the value it changes as its first parameter", def.display_name()),
            )
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
            if let Some(pat) = &mut p.pat {
                if mutating && i == 0 {
                    let d = Diagnostic::error("E0111", "the first parameter of a mutating function must be a plain name").at(pat.span);
                    self.diags.push(d);
                }
                self.pattern(pat, BindMode::Local);
            }
            if let Some(t) = &p.ty {
                if !t.ty.is_any() {
                    let ty = t.ty.clone();
                    self.set_declared_type(VarRes::Local(p.slot), ty);
                }
            } else if let Some(ty) = self.pending_param_tys.get(i).filter(|t| !t.is_any()).cloned() {
                self.set_declared_type(VarRes::Local(p.slot), ty);
            }
        }
        self.pending_param_tys.clear();
        if let Some(t) = &mut def.ret {
            if !sig_done {
                self.resolve_type(t, &[]);
            }
        }
        let mut effectful = Vec::new();
        for (i, c) in def.requires.iter_mut().chain(def.ensures.iter_mut()).enumerate() {
            if let Some((span, what)) = contract_effect(c) {
                let d = Diagnostic::error("E0118", format!("contracts must not change anything, but this {}", what))
                    .at(span)
                    .help("a contract only states a condition; move the change into the function body");
                self.error(d);
                effectful.push(i);
            } else if let Some((span, what)) = contract_escape(c) {
                let d = Diagnostic::error("E0122", format!("{} cannot be used in a contract", what))
                    .at(span)
                    .label("this would leave the function the contract is checked in")
                    .help("a contract is a condition, not a statement: write it as one expression, using `match` or `if` instead of `?`, and `or`/`and` instead of `return`");
                self.error(d);
            }
        }
        let n_requires = def.requires.len();
        for (i, r) in def.requires.iter_mut().enumerate() {
            self.contract_expr(r, effectful.contains(&i));
        }
        // `old(expr)` in postconditions: evaluated on entry, into dedicated slots.
        let mut olds: Vec<Expr> = Vec::new();
        for en in def.ensures.iter_mut() {
            extract_olds(en, &mut olds);
        }
        def.olds.clear();
        for (i, mut e) in olds.into_iter().enumerate() {
            self.expr(&mut e);
            let slot = self.declare_local(Rc::from(format!("old#{}", i).as_str()), e.span, false, LocalKind::Param);
            def.olds.push((e, slot));
        }
        self.expr(&mut def.body);
        if !def.ensures.is_empty() {
            self.push_scope();
            def.result_slot = self.declare_local(Rc::from("result"), def.name_span, false, LocalKind::Result);
            for (i, e) in def.ensures.iter_mut().enumerate() {
                self.contract_expr(e, effectful.contains(&(n_requires + i)));
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
            StmtKind::Let { pat, ty, value, mutable } => {
                // `let fact = fn(n) => ... fact(n - 1) ...` may refer to itself.
                match (&pat.kind, &mut value.kind) {
                    (PatKind::Bind { name, sub: None, .. }, ExprKind::Lambda(def)) if !self.at_global_scope() => {
                        let name = name.clone();
                        let def = Rc::get_mut(def).unwrap();
                        def.name = Some(name.clone());
                        self.resolve_fn(def, FnKind::Lambda, true, Some(name));
                    }
                    _ => self.expr(value),
                }
                if let Some(t) = ty {
                    self.resolve_type(t, &[]);
                }
                let mode = if self.at_global_scope() {
                    BindMode::Global
                } else if *mutable {
                    BindMode::LocalMut
                } else {
                    BindMode::Local
                };
                self.pattern(pat, mode);
                // Remember the annotation, so that later writes are checked too.
                if let Some(t) = ty.as_ref() {
                    let t = t.ty.clone();
                    self.declare_pattern_types(pat, &t);
                }
            }
            StmtKind::Assign { target, op, value, ty } => {
                self.expr(value);
                *ty = self.place(target, "E0101");
                // `xs = xs + [x]` and `xs = [..xs, x]` append in place, as
                // `xs += [x]` does, instead of copying `xs` each time.
                if op.is_none() {
                    if let Some(rest) = appended_part(target, value) {
                        *op = Some(BinOp::Add);
                        *value = rest;
                    }
                }
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

    /// W0005: a `!` function named like a built-in, with untyped
    /// parameters, takes every call to its name that fits its parameters, so
    /// a call on a field (`s.items.push!(x)`) runs it again instead of the
    /// built-in.
    fn check_hidden_builtin(&mut self, def: &FnDef) {
        let (Some(name), Some(slot)) = (&def.name, def.global_slot) else { return };
        if !def.mutating || def.overload_fallback.is_none() || def.params.iter().any(|p| p.ty.is_some() || p.pat.is_some()) {
            return;
        }
        let fits = |n: usize| n >= def.required_params() && n <= def.params.len();
        let Some(at) = find_field_call(&def.body, name, &|res, n| res == VarRes::Global(slot) && fits(n)) else { return };
        let p = def.params[0].name.clone();
        let d = Diagnostic::warning("W0005", format!("this calls your `{}`, not the built-in `{}`", name, name))
            .at(at)
            .label("calls this function again")
            .note(format!("`{}` has untyped parameters, so it takes every call to `{}` with this many arguments, whatever the values", name, name))
            .help(format!("give `{}` the type of the values this function is for; then calls on other values go to the built-in", p));
        self.diags.push(d);
    }

    /// Resolve an assignment target (or the receiver of a mutating call) and
    /// check that its root is mutable.
    fn place(&mut self, e: &mut Expr, code: &'static str) -> Option<Ty> {
        let span = e.span;
        match &mut e.kind {
            ExprKind::Var(v) => {
                let name = v.name.clone();
                let found = self.resolve_var(v, span)?;
                if found.mutable && !found.captured {
                    return found.ty;
                }
                let mut d;
                if found.captured {
                    d = Diagnostic::error("E0110", format!("cannot change `{}` inside a closure", name))
                        .at(span)
                        .label("captured variable")
                        .note("closures capture a snapshot of the values they use, not the variables themselves")
                        .help("return the new value from the closure instead, or use a loop; to keep state between calls (a memo table, a counter), pass it to a `!` function as its first argument");
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
                    d = Diagnostic::error(code, format!("cannot change `{}`, because it is not a `var`", name))
                        .at(span)
                        .label("immutable binding (a `let`, a parameter, a loop variable or a pattern binding)");
                    if found.span != Span::default() {
                        d = d.note(format!("`{}` is declared at {}", name, self.line_of(found.span)));
                    }
                    d = if self.is_param(&name) {
                        d.help(format!(
                            "parameters cannot change: copy it into a variable first (`var {0} = {0}`), or, to change the caller's variable, make `{0}` the first parameter of a `!` function",
                            name
                        ))
                    } else {
                        d.help(format!("declare it with `var {}` to allow changes", name))
                    };
                }
                if code == "E0111" {
                    d.message = if found.captured {
                        format!("cannot call a mutating function on `{}`, because closures cannot change the variables they capture", name)
                    } else {
                        format!("cannot call a mutating function on `{}`, because it is not a `var`", name)
                    };
                }
                self.error(d);
                None
            }
            ExprKind::Field { target, .. } => self.place(target, code),
            ExprKind::Index { target, index } => {
                self.expr(index);
                self.place(target, code)
            }
            _ => {
                self.expr(e);
                let d = Diagnostic::error("E0111", "mutating functions need a variable to change")
                    .at(span)
                    .label("this is a temporary value")
                    .help("store the value in a `var` first, or use the non-mutating version (without `!`), which returns a new value");
                self.error(d);
                None
            }
        }
    }

    /// The declared type of a variable, if the expression is one.
    fn declared_type_of(&self, e: &Expr) -> Option<Ty> {
        let ExprKind::Var(v) = &e.kind else { return None };
        let t = match v.res {
            VarRes::Local(slot) => {
                let f = self.fns.last()?;
                f.scopes.iter().rev().find_map(|s| s.locals.iter().rev().find(|l| l.slot == slot)).and_then(|l| l.ty.clone())
            }
            VarRes::Global(slot) => self.ctx.globals.get(slot as usize).and_then(|g| g.ty.clone()),
            _ => None,
        }?;
        if t.is_any() {
            None
        } else {
            Some(t)
        }
    }

    fn snippet_text(&self, span: Span) -> String {
        if (span.file as usize) < self.ctx.sm.files.len() {
            self.ctx.sm.snippet(span).to_string()
        } else {
            "the value".into()
        }
    }

    /// Give each name bound by a pattern the part of the annotation that
    /// describes it: `var (a, b): (Int, Str)` declares `a: Int`, `b: Str`.
    fn declare_pattern_types(&mut self, pat: &Pattern, ty: &Ty) {
        if ty.is_any() {
            return;
        }
        match (&pat.kind, ty) {
            (PatKind::Bind { res, sub, .. }, _) => {
                self.set_declared_type(*res, ty.clone());
                if let Some(p) = sub {
                    self.declare_pattern_types(p, ty);
                }
            }
            (PatKind::Tuple(ps), Ty::Tuple(ts)) if ps.len() == ts.len() => {
                for (p, t) in ps.iter().zip(ts) {
                    self.declare_pattern_types(p, t);
                }
            }
            (PatKind::List { before, rest, after }, Ty::List(et)) => {
                for p in before.iter().chain(after.iter()) {
                    self.declare_pattern_types(p, et);
                }
                if let Some(Some(p)) = rest {
                    self.declare_pattern_types(p, ty);
                }
            }
            _ => {}
        }
    }

    fn set_declared_type(&mut self, res: VarRes, ty: Ty) {
        match res {
            VarRes::Local(slot) => {
                let f = self.cur();
                for scope in f.scopes.iter_mut().rev() {
                    if let Some(l) = scope.locals.iter_mut().rev().find(|l| l.slot == slot) {
                        l.ty = Some(ty);
                        return;
                    }
                }
            }
            VarRes::Global(slot) => self.ctx.globals[slot as usize].ty = Some(ty),
            _ => {}
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
                if let StmtKind::Expr(e) = &s.kind {
                    self.check_discarded(e);
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
            if let ExprKind::Lambda(def) = &mut a.value.kind {
                if def.ret.is_none() {
                    if let Some(def) = Rc::get_mut(def) {
                        if let Some((span, is_try)) = lambda_escape(&mut def.body) {
                            let what = if is_try { "`?`" } else { "`return`" };
                            let d = Diagnostic::warning("W0004", format!("{} inside an anonymous function returns from that function only", what))
                                .at(span)
                                .label("leaves the `fn(...) => ...`, not the enclosing function")
                                .help(if is_try {
                                    "to stop at the first error, use a `for` loop; to turn a list of Results into a Result of a list, use `.collect_ok()` (or `.collect_some()` for Options);\nif this is intended, give the anonymous function a return type: `fn(x) -> Result[Int, Str] => ...`"
                                } else {
                                    "to leave the enclosing function early, use a `for` loop instead of a function such as `each`;\nif this is intended, give the anonymous function a return type: `fn(x) -> Int { ... }`"
                                });
                            self.diags.push(d);
                        }
                    }
                }
            }
        }
    }

    /// Warn when an expression statement throws away the result of a
    /// built-in that has no side effects (`xs.sort()` instead of `xs.sort!()`).
    fn check_discarded(&mut self, e: &Expr) {
        let (name, res, mutating) = match &e.kind {
            ExprKind::MethodCall { method, mutating, .. } => (&method.name, method.res, *mutating),
            ExprKind::Call { callee, .. } => match &callee.kind {
                ExprKind::Var(v) => (&v.name, v.res, v.name.ends_with('!')),
                _ => return,
            },
            _ => return,
        };
        let VarRes::Global(slot) = res else { return };
        // A user function that returns a Result: dropping it drops the error.
        // (Not when the call may go to a built-in of the same name, or to a
        // record's field: `logger.save(x)`.)
        if matches!(self.ctx.globals[slot as usize].kind, GlobalKind::Fn) {
            let other = self.ctx.builtins.values.contains_key(&**name) || self.ctx.known_fields.contains(&**name);
            let returns_result = !other
                && self.ctx.sigs.get(&slot).is_some_and(|sigs| {
                    !sigs.is_empty() && sigs.iter().all(|s| matches!(&s.ret, Some(Ty::Named { name, .. }) if &**name == "Result"))
                });
            if returns_result {
                let d = Diagnostic::warning("W0006", format!("the Result of `{}` is ignored", name))
                    .at(e.span)
                    .label("an error here would go unnoticed")
                    .help("handle it with `match` or `?`, stop on an error with `.unwrap()`, or say it may be ignored with `let _ = ...`");
                self.diags.push(d);
            }
            return;
        }
        if mutating {
            return;
        }
        let GlobalKind::Builtin(idx) = self.ctx.globals[slot as usize].kind else { return };
        let b = &crate::builtins::BUILTINS[idx as usize];
        const EFFECTS: &[&str] = &[
            "print",
            "write",
            "eprint",
            "input",
            "read_line",
            "read_stdin",
            "read_file",
            "write_file",
            "append_file",
            "list_dir",
            "exit",
            "sleep",
            "panic",
            "todo",
            "dbg",
            "catch",
            "seed",
            "random",
            "random_int",
            "shuffle",
            "choice",
            "each",
            "flush",
            // These stop the program when the value is not Ok/Some, so a
            // dropped result still checks something.
            "unwrap",
            "expect",
            "unwrap_err",
        ];
        if b.name.ends_with('!') || EFFECTS.contains(&b.name) {
            return;
        }
        // A callback with effects (`xs.any(fn(x) { print(x); ... })`) makes
        // the call do something even when its result is dropped (but `map`
        // used that way is still better written with `each`).
        let args: Vec<&Expr> = match &e.kind {
            ExprKind::MethodCall { args, .. } | ExprKind::Call { args, .. } => args.iter().map(|a| &a.value).collect(),
            _ => vec![],
        };
        if &**name != "map" && args.iter().any(|a| has_effects(a, EFFECTS)) {
            return;
        }
        let twin = format!("{}!", name);
        let help = if self.ctx.builtins.values.contains_key(twin.as_str()) {
            format!("`{}` returns a new value and leaves its argument unchanged; to change a `var` in place, call `{}`", name, twin)
        } else if &**name == "map" {
            "`map` builds a new list; to run a function for its side effects, use `each` or a `for` loop".to_string()
        } else {
            format!("`{}` returns a new value and does not change its arguments; store the result, e.g. `let y = ...`", name)
        };
        let mut d = Diagnostic::warning("W0003", format!("the result of `{}` is unused", name)).at(e.span).help(help);
        // `xs.push(x)` on a `var`: the call was meant to change it.
        if self.ctx.builtins.values.contains_key(twin.as_str()) {
            let target = match &e.kind {
                ExprKind::MethodCall { receiver, method_span, .. } => Some((&**receiver, *method_span)),
                ExprKind::Call { callee, args } => args.first().filter(|a| a.name.is_none()).map(|a| (&a.value, callee.span)),
                _ => None,
            };
            if let Some((Expr { kind: ExprKind::Var(v), .. }, name_span)) = target {
                if self.is_var(v) {
                    d = d.fix(Span::new(name_span.file, name_span.end as usize, name_span.end as usize), "!");
                }
            }
        }
        self.diags.push(d);
    }

    fn check_call(&mut self, slot: u32, extra: usize, args: &[Arg], span: Span) {
        let positional = args.iter().filter(|a| a.name.is_none()).count() + extra;
        let named: Vec<&Name> = args.iter().filter_map(|a| a.name.as_ref()).collect();
        let info = self.ctx.globals[slot as usize].clone();
        match &info.kind {
            GlobalKind::Builtin(idx) => {
                let b = &crate::builtins::BUILTINS[*idx as usize];
                if !named.is_empty() {
                    let names = crate::builtins::param_names(*idx);
                    if names.is_empty() {
                        let d = Diagnostic::error("E0108", format!("built-in function `{}` does not take named arguments", b.name)).at(span);
                        self.error(d);
                        return;
                    }
                    for (k, n) in named.iter().enumerate() {
                        if named[..k].contains(n) {
                            let d = Diagnostic::error("E0108", format!("argument `{}` is given twice", n)).at(span);
                            self.error(d);
                            return;
                        }
                    }
                    let forms = crate::builtins::param_forms(*idx);
                    for n in &named {
                        if !forms.iter().any(|f| f.iter().any(|x| **x == ***n)) {
                            let mut d = Diagnostic::error("E0108", format!("`{}` has no parameter named `{}`", b.name, n))
                                .at(span)
                                .note(format!("its parameters are: {}", names.join(", ")));
                            if let Some(s) = suggest(n, names.iter().map(|s| s.as_str())) {
                                d = d.help(format!("did you mean `{}`?", s));
                            }
                            self.error(d);
                            return;
                        }
                    }
                }
                let positional = positional + named.len();
                if positional < b.min as usize || positional > b.max as usize {
                    let expect = if b.min == b.max {
                        format!("{}", b.min)
                    } else if b.max == crate::builtins::VARIADIC {
                        format!("at least {}", b.min)
                    } else {
                        format!("{} to {}", b.min, b.max)
                    };
                    let d = Diagnostic::error(
                        "E0107",
                        format!("`{}` takes {} argument{}, but {} were given", b.name, expect, if expect == "1" { "" } else { "s" }, positional),
                    )
                    .at(span)
                    .note(format!("usage: {}", b.doc.lines().next().unwrap_or("")));
                    // `set([1, 2])` from Python: a set of values is `to_set`.
                    let d = if b.name == "set" && positional <= 1 { d.help("for a set of values, use `to_set(xs)`") } else { d };
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
                    let d = Diagnostic::error("E0108", format!("`{}` has positional fields; it cannot be called with named arguments", info.name))
                        .at(span);
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
            let d = Diagnostic::error(
                "E0107",
                format!("{} `{}` takes {} argument{}, but {} were given", what, name, total, if total == 1 { "" } else { "s" }, positional),
            )
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
            ExprKind::Field { target, name, name_span } => match self.module_member(target, name) {
                Some(Ok(slot)) => replacement = Some(ExprKind::Var(Var { name: name.clone(), res: VarRes::Global(slot) })),
                Some(Err((m, alias))) => {
                    let (n, s) = (name.clone(), *name_span);
                    self.unknown_member(&m, &alias, &n, s);
                    return;
                }
                None => {}
            },
            ExprKind::MethodCall { receiver, method, method_span, args, mutating, .. } => match self.module_member(receiver, &method.name) {
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
                            root_ty: None,
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
                ExprKind::MethodCall { receiver, args, root_ty, .. } => {
                    self.args(args);
                    *root_ty = self.place(receiver, "E0111");
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
                let n = self.diags.len();
                self.expr(callee);
                // `length(xs)`: the name of a function from another language.
                if let (ExprKind::Var(v), Some(d)) = (&callee.kind, self.diags.get_mut(n)) {
                    if d.code == "E0100" && d.span == Some(callee.span) && d.fixes.is_empty() {
                        if let Some(r) = confusion_fix(&v.name, true) {
                            d.fixes.push(crate::diagnostic::Fix { span: callee.span, text: r.to_string() });
                        }
                    }
                }
                self.args(args);
                if let ExprKind::Var(Var { res: VarRes::Global(slot), .. }) = callee.kind {
                    self.check_call(slot, 0, args, span);
                }
            }
            ExprKind::MethodCall { receiver, method, method_span, args, mutating, root_ty } => {
                self.args(args);
                if *mutating {
                    *root_ty = self.place(receiver, "E0111");
                    match self.lookup(&method.name) {
                        Some(f) => {
                            method.res = f.res;
                            // An imported module's function of the same name
                            // may answer instead (by the receiver's type), so
                            // its arity cannot be checked here.
                            if let VarRes::Global(slot) = f.res {
                                if !self.ctx.module_fns.contains(&method.name) {
                                    self.check_call(slot, 1, args, span);
                                }
                            }
                        }
                        None => {
                            // It may be defined in the module of the receiver's type.
                            if !self.ctx.module_fns.contains(&method.name) {
                                let n = method.name.clone();
                                self.undefined(&n, *method_span, "function");
                            }
                        }
                    }
                } else {
                    self.expr(receiver);
                    match self.lookup(&method.name) {
                        Some(f) => {
                            // A variable named like a function does not hide the
                            // function from method-call syntax (`lines.len()` with a
                            // variable called `len` still calls the built-in).
                            let shadowing_var = match f.res {
                                // Even a local function: in `let get = fn(k) => m.get(k)`
                                // the body's `m.get` is the built-in, as at top level.
                                VarRes::Local(_) | VarRes::Capture(_) | VarRes::SelfFn => true,
                                VarRes::Global(slot) => {
                                    matches!(self.ctx.globals[slot as usize].kind, GlobalKind::Let | GlobalKind::Var | GlobalKind::Const)
                                }
                                _ => false,
                            };
                            method.res = match (shadowing_var, self.function_slot(&method.name)) {
                                (true, Some(g)) => VarRes::Global(g),
                                _ => f.res,
                            };
                            let f = Found { res: method.res, ..f };
                            if let VarRes::Global(slot) = f.res {
                                if !self.ctx.known_fields.contains(&method.name) && !self.ctx.module_fns.contains(&method.name) {
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
            ExprKind::Is { expr, pat } => {
                self.expr(expr);
                if let Some(b) = first_binding(pat) {
                    let d = Diagnostic::error("E0120", "an `is` pattern cannot bind names")
                        .at(b)
                        .label("this name would be bound")
                        .help("`is` only tests the shape of a value: use `_` here, or use `match` to take the value apart");
                    self.error(d);
                }
                self.push_scope();
                self.pattern(pat, BindMode::Local);
                // (Already an error above; not also an unused variable.)
                for l in self.cur().scopes.last_mut().unwrap().locals.iter_mut() {
                    l.used = true;
                }
                self.pop_scope();
            }
            ExprKind::Try(inner) => {
                self.expr(inner);
                if self.cur().kind == FnKind::TopLevel {
                    let d = Diagnostic::error("E0115", "`?` can only be used inside a function")
                        .at(span)
                        .help("at the top level, use `match` or `.unwrap()` to get the value out");
                    self.error(d);
                } else {
                    self.check_try_kinds(inner, span);
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
                let errors_before = self.diags.iter().filter(|d| d.is_error()).count();
                for arm in arms.iter_mut() {
                    self.push_scope();
                    self.pattern(&mut arm.pat, BindMode::Local);
                    if let Some(g) = &mut arm.guard {
                        self.expr(g);
                    }
                    self.expr(&mut arm.body);
                    self.pop_scope();
                }
                let head = Span { end: scrutinee.span.end, ..span };
                if let Some(t) = self.declared_type_of(scrutinee).filter(|t| !self.clashing_types.contains(t.to_string().as_str())) {
                    let what = self.snippet_text(scrutinee.span);
                    for arm in arms.iter() {
                        if let Some(bad) = pattern_mismatch(&arm.pat, &t) {
                            // (Not again for a constructor reported as undefined.)
                            if self.diags.iter().any(|d| d.code == "E0100" && d.span == Some(bad.span)) {
                                continue;
                            }
                            let d = Diagnostic::error("E0119", format!("this pattern can never match: `{}` is declared as `{}`", what, t))
                                .at(bad.span)
                                .label("a pattern for a different type");
                            self.error(d);
                        }
                    }
                }
                if self.diags.iter().filter(|d| d.is_error()).count() == errors_before {
                    // A declared record type says which fields the value has.
                    self.scrutinee_fields = match self.declared_type_of(scrutinee) {
                        Some(Ty::Named { id, .. }) => match self.ctx.types.get(id as usize).map(|t| &t.kind) {
                            Some(TypeKind::Record { fields, .. }) => Some(fields.clone()),
                            _ => None,
                        },
                        _ => None,
                    };
                    self.check_exhaustive(arms, head);
                    self.scrutinee_fields = None;
                }
            }
            ExprKind::Block(stmts) => self.block(stmts),
            ExprKind::Lambda(def) => {
                let def = Rc::get_mut(def).unwrap();
                self.resolve_fn(def, FnKind::Lambda, true, None);
            }
            ExprKind::While { cond, body } => {
                self.expr(cond);
                self.cur().loops.push(false);
                self.expr(body);
                self.cur().loops.pop();
            }
            ExprKind::For { pat, iter, body } => {
                self.expr(iter);
                self.push_scope();
                self.pattern(pat, BindMode::Local);
                self.cur().loops.push(false);
                self.expr(body);
                self.cur().loops.pop();
                self.pop_scope();
            }
            ExprKind::Loop { body } => {
                self.cur().loops.push(true);
                self.expr(body);
                self.cur().loops.pop();
            }
            ExprKind::Break(v) => {
                let has_value = v.is_some();
                if let Some(v) = v {
                    self.expr(v);
                }
                match self.cur().loops.last() {
                    None => {
                        let d = Diagnostic::error("E0104", "`break` outside of a loop").at(span);
                        self.error(d);
                    }
                    Some(false) if has_value => {
                        let d = Diagnostic::error("E0104", "`break` with a value is only allowed inside `loop`")
                            .at(span)
                            .help("`while` and `for` loops always produce `()`; use `loop { ... break value ... }` to compute a value");
                        self.error(d);
                    }
                    _ => {}
                }
            }
            ExprKind::Continue => {
                if self.cur().loops.is_empty() {
                    let d = Diagnostic::error("E0104", "`continue` outside of a loop").at(span);
                    self.error(d);
                }
            }
            ExprKind::Return(v) => {
                if let Some(v) = v {
                    self.expr(v);
                }
                if self.cur().kind == FnKind::TopLevel {
                    let d = Diagnostic::error("E0105", "`return` outside of a function").at(span).help("use `exit(code)` to stop a script early");
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
            BindMode::LocalMut => VarRes::Local(self.declare_local(name.clone(), span, true, LocalKind::Var)),
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
                        first_bindings = names.iter().map(|(n, _)| (n.clone(), self.lookup_binding(n, mode))).collect();
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
                let slot = match name.split_once('.') {
                    Some((module, member)) => self.ns.values.get(module).and_then(|m| match &self.ctx.globals[*m as usize].kind {
                        GlobalKind::Module(m) => m.ns.values.get(member).copied(),
                        _ => None,
                    }),
                    None => self.global_slot(name),
                };
                let Some(slot) = slot else {
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
                        format!(
                            "`{}` has {} field{}, but the pattern lists {}",
                            name,
                            fields.len(),
                            if fields.len() == 1 { "" } else { "s" },
                            args.len()
                        ),
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
            BindMode::Local | BindMode::LocalMut => {
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
        // Matches are checked statically: enum variants, Bools, tuples,
        // records, list lengths, and literals (which never cover a whole type).
        let analyzable = arms.iter().any(|a| {
            let mut ps = vec![];
            flatten_alts(&a.pat, &mut ps);
            ps.iter().any(|p| !matches!(p.kind, PatKind::Wild | PatKind::Bind { sub: None, .. }))
        });
        if !analyzable {
            return;
        }
        let rows: Vec<Vec<Option<&Pattern>>> = arms.iter().filter(|a| a.guard.is_none()).map(|a| vec![Some(&a.pat)]).collect();
        self.exhaust_steps.set(20_000);
        self.exhaust_gave_up.set(false);
        if let Some(w) = self.missing(rows, 1, 0) {
            let witness = w.into_iter().next().unwrap_or_else(|| "_".into());
            let mut d = Diagnostic::error("E0109", format!("non-exhaustive match: `{}` is not handled", witness))
                .at(span)
                .help(format!("add an arm for `{}`, or a catch-all arm `_ => ...`", witness));
            if witness.contains("..") && arms.iter().any(|a| has_exact_record(&a.pat)) {
                d = d.note("a record pattern without `..` matches only records with exactly its fields; write `{ x, y, .. }` to allow others");
            }
            self.error(d);
        } else if self.exhaust_gave_up.get() {
            // Never accept a match as exhaustive without having shown it.
            let d = Diagnostic::error("E0109", "this match has too many combinations to check that it is exhaustive")
                .at(span)
                .help("add a catch-all arm `_ => ...`");
            self.error(d);
        }
    }

    /// Returns a value (as pattern text, one per column) that no row matches,
    /// or None if the rows are exhaustive (or cannot be analyzed).
    fn missing<'p>(&self, rows: Vec<Vec<Option<&'p Pattern>>>, n: usize, depth: usize) -> Option<Vec<String>> {
        let steps = self.exhaust_steps.get();
        if depth > 64 || steps == 0 {
            self.exhaust_gave_up.set(true);
            return None;
        }
        self.exhaust_steps.set(steps - 1);
        if n == 0 {
            return if rows.is_empty() { Some(vec![]) } else { None };
        }
        // A row of catch-alls matches whatever the other rows miss.
        if rows.iter().any(|r| r.iter().all(|c| c.is_none_or(|p| p.covers()))) {
            return None;
        }
        // Normalize the first column: bindings and catch-alls become wildcards,
        // `x @ p` becomes p, and or-patterns become several rows.
        let mut norm: Vec<Vec<Option<&'p Pattern>>> = Vec::new();
        for row in rows {
            let mut heads = vec![];
            match row[0] {
                None => heads.push(None),
                Some(p) => {
                    let mut ps = vec![];
                    flatten_alts(p, &mut ps);
                    for p in ps {
                        heads.push(if p.covers() && !matches!(p.kind, PatKind::Tuple(_) | PatKind::Ctor { .. }) { None } else { Some(p) });
                    }
                }
            }
            for h in heads {
                let mut r = row.clone();
                r[0] = h;
                norm.push(r);
            }
        }
        let rows = norm;
        // A constructor pattern says what type the column has (a record
        // pattern in the same column is then read against that type).
        let first = rows.iter().find_map(|r| r[0].filter(|p| matches!(p.kind, PatKind::Ctor { .. }))).or_else(|| rows.iter().find_map(|r| r[0]));
        let Some(first) = first else {
            // Only wildcards in this column.
            let rest: Vec<_> = rows.into_iter().map(|r| r[1..].to_vec()).collect();
            return self.missing(rest, n - 1, depth + 1).map(|mut w| {
                w.insert(0, "_".into());
                w
            });
        };
        // The constructors that can appear in this column: (label, arity, matcher).
        type Spec<'p> = Box<dyn Fn(&'p Pattern, usize) -> Option<Vec<Option<&'p Pattern>>> + 'p>;
        let mut ctors: Vec<(String, usize, Spec<'p>)> = Vec::new();
        match &first.kind {
            PatKind::Ctor { ctor, .. } => {
                let td = self.ctx.types.get(ctor.type_id as usize)?.clone();
                let type_id = ctor.type_id;
                match &td.kind {
                    TypeKind::Enum { variants } if !ctor.is_record => {
                        for (tag, v) in variants.iter().enumerate() {
                            let tag = tag as u32;
                            let arity = v.fields.len();
                            ctors.push((
                                v.name.to_string(),
                                arity,
                                Box::new(move |p: &'p Pattern, arity: usize| match &p.kind {
                                    PatKind::Ctor { ctor, args, field_idx, .. } if ctor.type_id == type_id && ctor.tag == tag && !ctor.is_record => {
                                        let mut sub = vec![None; arity];
                                        for ((_, a), idx) in args.iter().zip(field_idx) {
                                            if (*idx as usize) < arity {
                                                sub[*idx as usize] = Some(a);
                                            }
                                        }
                                        Some(sub)
                                    }
                                    _ => None,
                                }),
                            ));
                        }
                    }
                    TypeKind::Record { fields, .. } if ctor.is_record => {
                        let arity = fields.len();
                        let names = fields.clone();
                        ctors.push((
                            td.name.to_string(),
                            arity,
                            Box::new(move |p: &'p Pattern, arity: usize| match &p.kind {
                                PatKind::Ctor { ctor, args, field_idx, .. } if ctor.type_id == type_id && ctor.is_record => {
                                    let mut sub = vec![None; arity];
                                    for ((_, a), idx) in args.iter().zip(field_idx) {
                                        if (*idx as usize) < arity {
                                            sub[*idx as usize] = Some(a);
                                        }
                                    }
                                    Some(sub)
                                }
                                // `{ a: true, .. }` on this record type.
                                PatKind::Record { fields, rest } => {
                                    if !*rest && fields.len() != names.len() {
                                        return None;
                                    }
                                    let mut sub = vec![None; arity];
                                    for (n, a) in fields {
                                        let i = names.iter().position(|x| x == n)?;
                                        sub[i] = Some(a);
                                    }
                                    Some(sub)
                                }
                                _ => None,
                            }),
                        ));
                    }
                    _ => return None,
                }
            }
            PatKind::Lit(Lit::Bool(_)) => {
                for b in [true, false] {
                    ctors.push((
                        b.to_string(),
                        0,
                        Box::new(move |p: &'p Pattern, _| match &p.kind {
                            PatKind::Lit(Lit::Bool(x)) if *x == b => Some(vec![]),
                            _ => None,
                        }),
                    ));
                }
            }
            PatKind::Tuple(items) => {
                let k = items.len();
                ctors.push((
                    String::new(),
                    k,
                    Box::new(move |p: &'p Pattern, _| match &p.kind {
                        PatKind::Tuple(ps) if ps.len() == k => Some(ps.iter().map(Some).collect()),
                        _ => None,
                    }),
                ));
            }
            // Numbers and strings have too many values to list: a literal or
            // range covers only part of the column, so only the rows with a
            // wildcard here can cover the rest.
            PatKind::Lit(Lit::Int(_) | Lit::Float(_) | Lit::Str(_)) | PatKind::Range { .. } => {
                let rest: Vec<_> = rows.into_iter().filter(|r| r[0].is_none()).map(|r| r[1..].to_vec()).collect();
                return self.missing(rest, n - 1, depth + 1).map(|mut w| {
                    w.insert(0, "_".into());
                    w
                });
            }
            // Lists: a pattern without `..` matches one length, one with `..`
            // every length from its minimum up. Once a list is longer than
            // every fixed-length pattern and than the longest prefix plus the
            // longest suffix (so that no `[a, ..]` and `[.., z]` overlap), all
            // lengths behave alike: lengths 0..=that+1 cover every case.
            PatKind::List { .. } => {
                let (mut fixed, mut prefix, mut suffix) = (0, 0, 0);
                for r in &rows {
                    match r[0].map(|p| &p.kind) {
                        Some(PatKind::List { before, rest: None, after }) => fixed = fixed.max(before.len() + after.len()),
                        Some(PatKind::List { before, rest: Some(_), after }) => {
                            prefix = prefix.max(before.len());
                            suffix = suffix.max(after.len());
                        }
                        _ => {}
                    }
                }
                let longest = fixed.max(prefix + suffix);
                for len in 0..=longest + 1 {
                    let open = len == longest + 1;
                    ctors.push((
                        if open { "[..]".to_string() } else { "[]".to_string() },
                        len,
                        Box::new(move |p: &'p Pattern, len: usize| match &p.kind {
                            PatKind::List { before, rest: None, after } if before.len() + after.len() == len => {
                                Some(before.iter().chain(after.iter()).map(Some).collect())
                            }
                            PatKind::List { before, rest: Some(_), after } if before.len() + after.len() <= len => {
                                let mut v: Vec<Option<&'p Pattern>> = before.iter().map(Some).collect();
                                v.extend(std::iter::repeat_n(None, len - before.len() - after.len()));
                                v.extend(after.iter().map(Some));
                                Some(v)
                            }
                            _ => None,
                        }),
                    ));
                }
            }
            // Anonymous records: one constructor whose fields are all the
            // names mentioned in this column.
            PatKind::Record { .. } => {
                let mut names: Vec<Name> = Vec::new();
                for r in &rows {
                    if let Some(PatKind::Record { fields, .. }) = r[0].map(|p| &p.kind) {
                        for (n, _) in fields {
                            if !names.contains(n) {
                                names.push(n.clone());
                            }
                        }
                    }
                }
                names.sort();
                let label = format!("{{{}", names.iter().map(|n| n.to_string()).collect::<Vec<_>>().join("\u{0}"));
                let names2 = names.clone();
                ctors.push((
                    label.clone(),
                    names.len(),
                    Box::new(move |p: &'p Pattern, _| match &p.kind {
                        PatKind::Record { fields, .. } => Some(names2.iter().map(|n| fields.iter().find(|(f, _)| f == n).map(|(_, p)| p)).collect()),
                        _ => None,
                    }),
                ));
                // A pattern without `..` matches only records with exactly its
                // fields, so a record with one more field must be matched by
                // a pattern with `..` (or a wildcard), unless the value's
                // declared type has exactly the pattern's fields.
                let all_fields = if depth == 0 { self.scrutinee_fields.clone() } else { None };
                let complete = move |fields: &[(Name, Pattern)]| {
                    all_fields.as_ref().is_some_and(|all| all.len() == fields.len() && fields.iter().all(|(f, _)| all.contains(f)))
                };
                let exact = rows.iter().any(|r| matches!(r[0].map(|p| &p.kind), Some(PatKind::Record { rest: false, fields }) if !complete(fields)));
                if exact {
                    let names3 = names.clone();
                    ctors.push((
                        label,
                        names.len(),
                        Box::new(move |p: &'p Pattern, _| match &p.kind {
                            PatKind::Record { fields, rest } if *rest || complete(fields) => {
                                Some(names3.iter().map(|n| fields.iter().find(|(f, _)| f == n).map(|(_, p)| p)).collect())
                            }
                            _ => None,
                        }),
                    ));
                }
            }
            _ => return None,
        }
        for (label, arity, spec) in &ctors {
            let mut sub_rows = Vec::new();
            for r in &rows {
                match r[0] {
                    None => {
                        let mut nr = vec![None; *arity];
                        nr.extend_from_slice(&r[1..]);
                        sub_rows.push(nr);
                    }
                    Some(p) => {
                        if let Some(mut nr) = spec(p, *arity) {
                            nr.extend_from_slice(&r[1..]);
                            sub_rows.push(nr);
                        }
                    }
                }
            }
            if let Some(w) = self.missing(sub_rows, arity + n - 1, depth + 1) {
                let (args, rest) = w.split_at(*arity);
                let text = if label.is_empty() {
                    format!("({})", args.join(", "))
                } else if label == "[]" {
                    format!("[{}]", args.join(", "))
                } else if label == "[..]" {
                    format!("[{}..]", args.iter().map(|a| format!("{}, ", a)).collect::<String>())
                } else if let Some(names) = label.strip_prefix('{') {
                    let fields: Vec<String> =
                        names.split('\u{0}').zip(args).filter(|(_, a)| *a != "_").map(|(n, a)| format!("{}: {}", n, a)).collect();
                    if fields.is_empty() {
                        "{ .. }".to_string()
                    } else {
                        format!("{{ {}, .. }}", fields.join(", "))
                    }
                } else if *arity == 0 {
                    label.clone()
                } else if args.iter().all(|a| a == "_") {
                    format!("{}(..)", label)
                } else {
                    format!("{}({})", label, args.join(", "))
                };
                let mut out = vec![text];
                out.extend_from_slice(rest);
                return Some(out);
            }
        }
        None
    }
}

/// Replace each `old(x)` in an expression with a reference to `old#i`,
/// collecting the `x`s.
/// The first `?` or `return` in a lambda body that belongs to the lambda
/// itself (not to a lambda nested inside it). `true` means `?`.
fn lambda_escape(e: &mut Expr) -> Option<(Span, bool)> {
    fn go(e: &mut Expr, found: &mut Option<(Span, bool)>) {
        if found.is_some() {
            return;
        }
        match &e.kind {
            ExprKind::Lambda(_) => return,
            ExprKind::Try(_) => {
                *found = Some((e.span, true));
                return;
            }
            ExprKind::Return(_) => {
                *found = Some((e.span, false));
                return;
            }
            _ => {}
        }
        for_each_child_mut(e, &mut |c| go(c, found));
    }
    let mut found = None;
    go(e, &mut found);
    found
}

/// The first assignment or mutating call in a contract, if any.
fn contract_effect(e: &mut Expr) -> Option<(Span, &'static str)> {
    fn go(e: &mut Expr, found: &mut Option<(Span, &'static str)>) {
        if found.is_some() {
            return;
        }
        match &e.kind {
            ExprKind::MethodCall { mutating: true, .. } => {
                *found = Some((e.span, "calls a mutating function"));
                return;
            }
            ExprKind::Call { callee, .. } if matches!(&callee.kind, ExprKind::Var(v) if v.name.ends_with('!')) => {
                *found = Some((e.span, "calls a mutating function"));
                return;
            }
            ExprKind::Block(stmts) => {
                if let Some(s) = stmts.iter().find(|s| matches!(s.kind, StmtKind::Assign { .. })) {
                    *found = Some((s.span, "assigns to a variable"));
                    return;
                }
            }
            _ => {}
        }
        for_each_child_mut(e, &mut |c| go(c, found));
    }
    let mut found = None;
    go(e, &mut found);
    found
}

/// For `x = x + e` with `x` a variable: `e`, when computing it cannot
/// change `x`, so that the assignment can be done as `x += e`. (`x = [..x,
/// a]` is handled when it runs: it appends only if `x` holds a list.)
fn appended_part(target: &Expr, value: &mut Expr) -> Option<Expr> {
    let ExprKind::Var(Var { res: res @ (VarRes::Local(_) | VarRes::Global(_)), .. }) = &target.kind else { return None };
    let is_target = |e: &Expr| matches!(&e.kind, ExprKind::Var(v) if v.res == *res);
    // A call could change a global while `e` is computed.
    let safe = |e: &Expr| !may_change_locals(e) && !(matches!(res, VarRes::Global(_)) && has_calls(e));
    let span = value.span;
    match &mut value.kind {
        ExprKind::Binary { op: BinOp::Add, lhs, rhs } if is_target(lhs) && safe(rhs) => {
            Some(std::mem::replace(&mut **rhs, Expr { kind: ExprKind::Unit, span }))
        }
        _ => None,
    }
}

/// A `return` or `?` in a contract, outside any anonymous function: it
/// would leave the function the contract is checked in (the caller, for a
/// `requires` or a type's `where` clause).
fn contract_escape(e: &Expr) -> Option<(Span, &'static str)> {
    match &e.kind {
        ExprKind::Lambda(_) => return None,
        ExprKind::Return(_) => return Some((e.span, "`return`")),
        ExprKind::Try(_) => return Some((e.span, "`?`")),
        _ => {}
    }
    let mut found = None;
    for_each_child(e, &mut |c| {
        if found.is_none() {
            found = contract_escape(c);
        }
    });
    found
}

fn extract_olds(e: &mut Expr, out: &mut Vec<Expr>) {
    if let ExprKind::Call { callee, args } = &mut e.kind {
        if matches!(&callee.kind, ExprKind::Var(v) if &*v.name == "old") && args.len() == 1 && args[0].name.is_none() {
            let inner = std::mem::replace(&mut args[0].value, Expr { kind: ExprKind::Unit, span: e.span });
            let name: Name = Rc::from(format!("old#{}", out.len()).as_str());
            out.push(inner);
            e.kind = ExprKind::Var(Var::new(name));
            return;
        }
    }
    for_each_child_mut(e, &mut |c| extract_olds(c, out));
}

/// The first name a pattern binds, if any (`_` binds nothing).
fn first_binding(p: &Pattern) -> Option<Span> {
    match &p.kind {
        PatKind::Bind { .. } => Some(p.span),
        PatKind::Wild | PatKind::Lit(_) | PatKind::Range { .. } => None,
        PatKind::Tuple(ps) | PatKind::Or(ps) => ps.iter().find_map(first_binding),
        PatKind::List { before, rest, after } => before
            .iter()
            .chain(after.iter())
            .find_map(first_binding)
            .or_else(|| rest.as_ref().and_then(|r| r.as_ref()).and_then(|r| first_binding(r))),
        PatKind::Ctor { args, .. } => args.iter().find_map(|(_, p)| first_binding(p)),
        PatKind::Record { fields, .. } => fields.iter().find_map(|(_, p)| first_binding(p)),
    }
}

/// The part of a pattern that can never match a value of type `ty`, if any.
fn pattern_mismatch<'p>(p: &'p Pattern, ty: &Ty) -> Option<&'p Pattern> {
    if ty.is_any() {
        return None;
    }
    let ok = match (&p.kind, ty) {
        (PatKind::Wild, _) => true,
        (PatKind::Bind { sub, .. }, _) => return sub.as_ref().and_then(|s| pattern_mismatch(s, ty)),
        (PatKind::Or(alts), _) => return alts.iter().find_map(|a| pattern_mismatch(a, ty)),
        // (Sub-patterns of a constructor are not checked here.)
        (PatKind::Ctor { ctor, .. }, Ty::Named { id, .. }) => ctor.type_id == *id,
        // A structural annotation `{ x: Float }` also accepts declared records.
        (PatKind::Ctor { ctor, .. }, Ty::Record(_)) => ctor.is_record,
        (PatKind::Ctor { .. }, _) => false,
        (PatKind::Lit(Lit::Bool(_)), t) => matches!(t, Ty::Bool),
        (PatKind::Lit(Lit::Int(_) | Lit::Float(_)), t) => matches!(t, Ty::Int | Ty::Float),
        (PatKind::Lit(Lit::Str(_)), t) => matches!(t, Ty::Str),
        (PatKind::Lit(Lit::Unit), t) => matches!(t, Ty::Unit),
        (PatKind::Range { .. }, t) => matches!(t, Ty::Int | Ty::Float | Ty::Str),
        (PatKind::Tuple(ps), Ty::Tuple(ts)) => {
            if ps.len() != ts.len() {
                false
            } else {
                return ps.iter().zip(ts).find_map(|(p, t)| pattern_mismatch(p, t));
            }
        }
        (PatKind::Tuple(_), _) => false,
        (PatKind::List { before, after, .. }, Ty::List(et)) => return before.iter().chain(after.iter()).find_map(|p| pattern_mismatch(p, et)),
        (PatKind::List { .. }, _) => false,
        (PatKind::Record { .. }, Ty::Record(_) | Ty::Named { .. }) => true,
        (PatKind::Record { .. }, _) => false,
    };
    if ok {
        None
    } else {
        Some(p)
    }
}

/// Whether an anonymous function might do something besides compute a
/// value: print, call a `!` function or another effectful built-in, or
/// assign to a variable.
fn has_effects(e: &Expr, effects: &[&str]) -> bool {
    let ExprKind::Lambda(def) = &e.kind else { return false };
    fn walk(e: &Expr, effects: &[&str], found: &mut bool) {
        match &e.kind {
            ExprKind::MethodCall { mutating: true, .. } => *found = true,
            ExprKind::MethodCall { method, .. } if effects.contains(&&*method.name) => *found = true,
            ExprKind::Call { callee, .. } => {
                if let ExprKind::Var(v) = &callee.kind {
                    if v.name.ends_with('!') || effects.contains(&&*v.name) {
                        *found = true;
                    }
                }
            }
            ExprKind::Block(stmts) if stmts.iter().any(|s| matches!(s.kind, StmtKind::Assign { .. })) => *found = true,
            _ => {}
        }
        for_each_child(e, &mut |c| walk(c, effects, found));
    }
    let mut found = false;
    walk(&def.body, effects, &mut found);
    found
}

/// The first use of a variable with one of these names in an expression.
/// A call `x.f.name(...)` or `name(x.f, ...)` (on a field) inside `e` for
/// which `calls(resolution, argument count)` holds.
fn find_field_call(e: &Expr, name: &str, calls: &dyn Fn(VarRes, usize) -> bool) -> Option<Span> {
    match &e.kind {
        ExprKind::MethodCall { receiver, method, method_span, args, .. }
            if &*method.name == name && matches!(receiver.kind, ExprKind::Field { .. }) && calls(method.res, args.len() + 1) =>
        {
            return Some(*method_span)
        }
        ExprKind::Call { callee, args }
            if matches!(&callee.kind, ExprKind::Var(v) if &*v.name == name && calls(v.res, args.len()))
                && args.first().is_some_and(|a| a.name.is_none() && matches!(a.value.kind, ExprKind::Field { .. })) =>
        {
            return Some(callee.span)
        }
        _ => {}
    }
    let mut found = None;
    for_each_child(e, &mut |c| {
        if found.is_none() {
            found = find_field_call(c, name, calls);
        }
    });
    found
}

fn find_var(e: &Expr, names: &[&str]) -> Option<Span> {
    if let ExprKind::Var(v) = &e.kind {
        if names.contains(&&*v.name) {
            return Some(e.span);
        }
    }
    let mut found = None;
    for_each_child(e, &mut |c| {
        if found.is_none() {
            found = find_var(c, names);
        }
    });
    found
}

/// Whether a pattern contains a record pattern without `..`.
fn has_exact_record(p: &Pattern) -> bool {
    let mut ps = vec![];
    flatten_alts(p, &mut ps);
    ps.iter().any(|p| match &p.kind {
        PatKind::Record { rest: false, .. } => true,
        PatKind::Record { fields, .. } => fields.iter().any(|(_, f)| has_exact_record(f)),
        PatKind::Tuple(items) => items.iter().any(has_exact_record),
        PatKind::List { before, after, .. } => before.iter().chain(after.iter()).any(has_exact_record),
        PatKind::Ctor { args, .. } => args.iter().any(|(_, a)| has_exact_record(a)),
        _ => false,
    })
}

fn flatten_alts<'p>(p: &'p Pattern, out: &mut Vec<&'p Pattern>) {
    match &p.kind {
        PatKind::Or(alts) => alts.iter().for_each(|a| flatten_alts(a, out)),
        PatKind::Bind { sub: Some(s), .. } => flatten_alts(s, out),
        _ => out.push(p),
    }
}

fn is_primitive_type(name: &str) -> bool {
    matches!(name, "Int" | "Float" | "Str" | "Bool" | "Unit" | "Any" | "List" | "Map" | "Set" | "Range" | "Fn")
}

fn field_names(td: &TypeDef) -> Vec<Name> {
    match &td.kind {
        TypeKind::Record { fields, .. } => fields.iter().cloned().collect(),
        TypeKind::Enum { variants } => variants.iter().filter(|v| v.named).flat_map(|v| v.fields.iter().cloned()).collect(),
    }
}
