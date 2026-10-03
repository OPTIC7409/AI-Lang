//! The abstract syntax tree. The parser builds it; the resolver fills in the
//! resolution fields (slots, captures, resolved types); the interpreter runs it.

use crate::span::Span;
use crate::types::{Name, Ty};
use crate::value::Text;
use std::cell::Cell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

/// Where a variable lives at runtime.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VarRes {
    Unresolved,
    /// A slot in the current function's frame.
    Local(u32),
    /// An index into the current closure's captured values.
    Capture(u32),
    /// A slot in the global table.
    Global(u32),
    /// The function currently executing (for recursion of local functions).
    SelfFn,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureSrc {
    Local(u32),
    Capture(u32),
    SelfFn,
}

#[derive(Debug)]
pub struct Var {
    pub name: Name,
    pub res: VarRes,
}

impl Var {
    pub fn new(name: Name) -> Var {
        Var { name, res: VarRes::Unresolved }
    }
}

#[derive(Debug)]
pub struct Program {
    pub items: Vec<Item>,
    pub file: u32,
    /// Number of local slots needed by the top-level frame.
    pub num_slots: u32,
}

#[derive(Debug)]
pub enum Item {
    Fn(Rc<FnDef>),
    Type(TypeDecl),
    Test(TestDecl),
    Property(PropDecl),
    Import(ImportDecl),
    Stmt(Stmt),
}

#[derive(Debug)]
pub struct Param {
    pub name: Name,
    pub span: Span,
    pub ty: Option<TypeExpr>,
    pub default: Option<Expr>,
    pub slot: u32,
    /// A destructuring pattern, as in `fn((key, value)) => ...`.
    pub pat: Option<Pattern>,
}

#[derive(Debug)]
pub struct FnDef {
    pub name: Option<Name>,
    pub name_span: Span,
    pub span: Span,
    pub generics: Vec<Name>,
    pub params: Vec<Param>,
    pub ret: Option<TypeExpr>,
    pub requires: Vec<Expr>,
    pub ensures: Vec<Expr>,
    pub body: Expr,
    /// The function's name ends in `!`: it mutates its first argument.
    pub mutating: bool,
    // ---- filled in by the resolver ----
    pub num_slots: u32,
    pub captures: Vec<CaptureSrc>,
    pub result_slot: u32,
    /// `old(expr)` inside `ensures`: each expression is evaluated on entry
    /// and stored in its slot (filled in by the resolver).
    pub olds: Vec<(Expr, u32)>,
    pub global_slot: Option<u32>,
    /// For a top-level function that shares its name with a built-in: the
    /// built-in's global slot, used as the last overload candidate.
    pub overload_fallback: Option<u32>,
}

impl FnDef {
    pub fn display_name(&self) -> Rc<str> {
        self.name.clone().unwrap_or_else(|| Rc::from("<anonymous fn>"))
    }

    pub fn required_params(&self) -> usize {
        self.params.iter().filter(|p| p.default.is_none()).count()
    }

    pub fn has_contracts(&self) -> bool {
        !self.requires.is_empty() || !self.ensures.is_empty()
    }
}

#[derive(Debug)]
pub struct TypeExpr {
    pub kind: TypeExprKind,
    pub span: Span,
    pub ty: Ty,
}

#[derive(Debug)]
pub enum TypeExprKind {
    Unit,
    Named(Name, Vec<TypeExpr>),
    Tuple(Vec<TypeExpr>),
    Record(Vec<(Name, TypeExpr)>),
    Fn(Vec<TypeExpr>, Box<TypeExpr>),
}

#[derive(Debug)]
pub struct FieldDecl {
    pub name: Option<Name>,
    pub ty: TypeExpr,
    pub span: Span,
}

#[derive(Debug)]
pub struct VariantDecl {
    pub name: Name,
    pub span: Span,
    pub fields: Vec<FieldDecl>,
    pub has_parens: bool,
    pub slot: u32,
}

#[derive(Debug)]
pub enum TypeBody {
    Record(Vec<FieldDecl>),
    Enum(Vec<VariantDecl>),
    /// `type Grid = List[List[Bool]]`
    Alias(TypeExpr),
}

#[derive(Debug)]
pub struct TypeDecl {
    pub name: Name,
    pub name_span: Span,
    pub span: Span,
    pub params: Vec<Name>,
    pub body: TypeBody,
    pub id: u32,
    /// Global slot of the record constructor (record types only).
    pub slot: u32,
}

#[derive(Debug)]
pub struct TestDecl {
    pub name: String,
    pub span: Span,
    pub func: Rc<FnDef>,
}

#[derive(Debug)]
pub struct PropDecl {
    pub name: String,
    pub span: Span,
    /// Parameters are the generated inputs; `where` clauses become `requires`.
    pub func: Rc<FnDef>,
}

#[derive(Debug, Clone)]
pub struct AliasDef {
    pub params: Vec<Name>,
    pub ty: Ty,
}

/// The names defined at the top level of a module (or the REPL, or the built-ins).
#[derive(Debug, Default, Clone)]
pub struct Namespace {
    /// Value names (functions, constructors, globals, modules) to global slots.
    pub values: HashMap<Name, u32>,
    /// Type names to type ids.
    pub types: HashMap<Name, u32>,
    /// Type aliases.
    pub aliases: HashMap<Name, AliasDef>,
}

#[derive(Debug)]
pub struct Module {
    pub name: Name,
    pub path: PathBuf,
    pub program: Program,
    pub ns: Namespace,
    pub executed: Cell<bool>,
}

#[derive(Debug)]
pub struct ImportDecl {
    pub path: String,
    pub path_span: Span,
    pub alias: Option<Name>,
    pub span: Span,
    pub module: Option<Rc<Module>>,
    pub slot: u32,
}

#[derive(Debug)]
pub struct Stmt {
    pub kind: StmtKind,
    pub span: Span,
}

#[derive(Debug)]
pub enum StmtKind {
    /// `let pattern = value` or (with `mutable`) `var pattern = value`.
    Let {
        pat: Pattern,
        ty: Option<TypeExpr>,
        value: Expr,
        mutable: bool,
    },
    /// `ty` is the declared type of the target's root variable (from the resolver).
    Assign {
        target: Expr,
        op: Option<BinOp>,
        value: Expr,
        ty: Option<Ty>,
    },
    Fn {
        def: Rc<FnDef>,
        res: VarRes,
    },
    Assert {
        cond: Expr,
        msg: Option<Expr>,
    },
    Expr(Expr),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    FloorDiv,
    Mod,
    Pow,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    In,
    NotIn,
}

impl BinOp {
    pub fn symbol(self) -> &'static str {
        match self {
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Div => "/",
            BinOp::FloorDiv => "//",
            BinOp::Mod => "%",
            BinOp::Pow => "**",
            BinOp::Eq => "==",
            BinOp::Ne => "!=",
            BinOp::Lt => "<",
            BinOp::Le => "<=",
            BinOp::Gt => ">",
            BinOp::Ge => ">=",
            BinOp::In => "in",
            BinOp::NotIn => "not in",
        }
    }

    pub fn is_comparison(self) -> bool {
        matches!(self, BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge | BinOp::In | BinOp::NotIn)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnOp {
    Neg,
    Not,
}

#[derive(Debug)]
pub struct Expr {
    pub kind: ExprKind,
    pub span: Span,
}

#[derive(Debug)]
pub struct Arg {
    pub name: Option<Name>,
    pub value: Expr,
}

#[derive(Debug, Clone, Default)]
pub struct FmtSpec {
    pub fill: char,
    pub align: Option<char>,
    pub plus: bool,
    pub zero: bool,
    pub width: usize,
    /// Group thousands with commas (`{n:,}`).
    pub group: bool,
    pub precision: Option<usize>,
    pub kind: Option<char>,
}

#[derive(Debug)]
pub enum InterpPart {
    Lit(Rc<Text>),
    Expr(Expr, Option<FmtSpec>),
}

#[derive(Debug)]
pub struct Arm {
    pub pat: Pattern,
    pub guard: Option<Expr>,
    pub body: Expr,
}

#[derive(Debug)]
pub enum CompClause {
    For(Pattern, Expr),
    If(Expr),
}

#[derive(Debug)]
pub struct ListItem {
    pub expr: Expr,
    pub spread: bool,
}

#[derive(Debug)]
pub enum ExprKind {
    Unit,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(Rc<Text>),
    Interp(Vec<InterpPart>),
    Var(Var),
    List(Vec<ListItem>),
    Comprehension {
        body: Box<Expr>,
        clauses: Vec<CompClause>,
    },
    Map(Vec<(Expr, Expr)>),
    Tuple(Vec<Expr>),
    Record {
        names: Rc<[Name]>,
        values: Vec<Expr>,
        spread: Option<Box<Expr>>,
    },
    Field {
        target: Box<Expr>,
        name: Name,
        name_span: Span,
    },
    Index {
        target: Box<Expr>,
        index: Box<Expr>,
    },
    Call {
        callee: Box<Expr>,
        args: Vec<Arg>,
    },
    /// For mutating calls, `root_ty` is the declared type of the receiver's root variable.
    MethodCall {
        receiver: Box<Expr>,
        method: Var,
        method_span: Span,
        args: Vec<Arg>,
        mutating: bool,
        root_ty: Option<Ty>,
    },
    Unary {
        op: UnOp,
        expr: Box<Expr>,
    },
    Binary {
        op: BinOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Range {
        start: Box<Expr>,
        end: Option<Box<Expr>>,
        inclusive: bool,
    },
    Try(Box<Expr>),
    If {
        cond: Box<Expr>,
        then: Box<Expr>,
        els: Option<Box<Expr>>,
    },
    Match {
        scrutinee: Box<Expr>,
        arms: Vec<Arm>,
    },
    Block(Vec<Stmt>),
    Lambda(Rc<FnDef>),
    While {
        cond: Box<Expr>,
        body: Box<Expr>,
    },
    For {
        pat: Pattern,
        iter: Box<Expr>,
        body: Box<Expr>,
    },
    Loop {
        body: Box<Expr>,
    },
    Break(Option<Box<Expr>>),
    Continue,
    Return(Option<Box<Expr>>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Lit {
    Unit,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(Rc<Text>),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CtorRef {
    pub type_id: u32,
    pub tag: u32,
    pub is_record: bool,
}

#[derive(Debug)]
pub struct Pattern {
    pub kind: PatKind,
    pub span: Span,
}

#[derive(Debug)]
pub enum PatKind {
    Wild,
    Bind {
        name: Name,
        res: VarRes,
        sub: Option<Box<Pattern>>,
    },
    Lit(Lit),
    Range {
        lo: Lit,
        hi: Lit,
        inclusive: bool,
    },
    Tuple(Vec<Pattern>),
    List {
        before: Vec<Pattern>,
        rest: Option<Option<Box<Pattern>>>,
        after: Vec<Pattern>,
    },
    /// `Circle(r)`, `Rect(w: w, ..)`, `None`, `Point(x, y)`.
    Ctor {
        name: Name,
        args: Vec<(Option<Name>, Pattern)>,
        rest: bool,
        ctor: CtorRef,
        field_idx: Vec<u32>,
    },
    Record {
        fields: Vec<(Name, Pattern)>,
        rest: bool,
    },
    Or(Vec<Pattern>),
}

/// Visit every direct sub-expression of `e` mutably (descending into lambdas).
pub fn for_each_child_mut(e: &mut Expr, f: &mut dyn FnMut(&mut Expr)) {
    match &mut e.kind {
        ExprKind::Unit | ExprKind::Bool(_) | ExprKind::Int(_) | ExprKind::Float(_) | ExprKind::Str(_) | ExprKind::Var(_) | ExprKind::Continue => {}
        ExprKind::Interp(parts) => {
            for p in parts.iter_mut() {
                if let InterpPart::Expr(x, _) = p {
                    f(x);
                }
            }
        }
        ExprKind::List(items) => items.iter_mut().for_each(|i| f(&mut i.expr)),
        ExprKind::Comprehension { body, clauses } => {
            for c in clauses.iter_mut() {
                match c {
                    CompClause::For(_, x) | CompClause::If(x) => f(x),
                }
            }
            f(body);
        }
        ExprKind::Map(es) => es.iter_mut().for_each(|(k, v)| {
            f(k);
            f(v);
        }),
        ExprKind::Tuple(items) => items.iter_mut().for_each(|x| f(x)),
        ExprKind::Record { values, spread, .. } => {
            values.iter_mut().for_each(|x| f(x));
            if let Some(s) = spread {
                f(s);
            }
        }
        ExprKind::Field { target, .. } => f(target),
        ExprKind::Index { target, index } => {
            f(target);
            f(index);
        }
        ExprKind::Call { callee, args } => {
            f(callee);
            args.iter_mut().for_each(|a| f(&mut a.value));
        }
        ExprKind::MethodCall { receiver, args, .. } => {
            f(receiver);
            args.iter_mut().for_each(|a| f(&mut a.value));
        }
        ExprKind::Unary { expr, .. } | ExprKind::Try(expr) => f(expr),
        ExprKind::Binary { lhs, rhs, .. } | ExprKind::And(lhs, rhs) | ExprKind::Or(lhs, rhs) => {
            f(lhs);
            f(rhs);
        }
        ExprKind::Range { start, end, .. } => {
            f(start);
            if let Some(x) = end {
                f(x);
            }
        }
        ExprKind::If { cond, then, els } => {
            f(cond);
            f(then);
            if let Some(x) = els {
                f(x);
            }
        }
        ExprKind::Match { scrutinee, arms } => {
            f(scrutinee);
            for a in arms.iter_mut() {
                if let Some(g) = &mut a.guard {
                    f(g);
                }
                f(&mut a.body);
            }
        }
        ExprKind::Block(stmts) => {
            for s in stmts.iter_mut() {
                match &mut s.kind {
                    StmtKind::Let { value, .. } => f(value),
                    StmtKind::Assign { target, value, .. } => {
                        f(target);
                        f(value);
                    }
                    StmtKind::Fn { .. } => {}
                    StmtKind::Assert { cond, msg } => {
                        f(cond);
                        if let Some(m) = msg {
                            f(m);
                        }
                    }
                    StmtKind::Expr(x) => f(x),
                }
            }
        }
        ExprKind::Lambda(def) => {
            if let Some(d) = Rc::get_mut(def) {
                f(&mut d.body);
            }
        }
        ExprKind::While { cond, body } => {
            f(cond);
            f(body);
        }
        ExprKind::For { iter, body, .. } => {
            f(iter);
            f(body);
        }
        ExprKind::Loop { body } => f(body),
        ExprKind::Break(v) | ExprKind::Return(v) => {
            if let Some(x) = v {
                f(x);
            }
        }
    }
}

impl Pattern {
    /// For exhaustiveness checking: does this pattern match every value of
    /// the shape it describes? (Tuples and records of catch-alls do.)
    pub fn covers(&self) -> bool {
        match &self.kind {
            PatKind::Wild => true,
            PatKind::Bind { sub, .. } => sub.as_ref().is_none_or(|s| s.covers()),
            PatKind::Or(alts) => alts.iter().any(|a| a.covers()),
            PatKind::Tuple(items) => items.iter().all(|p| p.covers()),
            PatKind::Record { fields, rest: true } => fields.iter().all(|(_, p)| p.covers()),
            PatKind::Ctor { ctor, args, .. } if ctor.is_record => args.iter().all(|(_, p)| p.covers()),
            _ => false,
        }
    }

    /// True if this pattern matches any value (never fails).
    pub fn is_irrefutable(&self) -> bool {
        match &self.kind {
            PatKind::Wild => true,
            PatKind::Bind { sub, .. } => sub.as_ref().is_none_or(|s| s.is_irrefutable()),
            PatKind::Or(alts) => alts.iter().any(|a| a.is_irrefutable()),
            _ => false,
        }
    }
}
