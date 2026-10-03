//! Static program context shared by the resolver and the interpreter:
//! sources, global slots, type definitions, and loaded modules.

use crate::ast::{CtorRef, Module, Namespace};
use crate::span::{SourceMap, Span};
use crate::types::{builtin_types, Name, Ty, TypeDef, TypeKind};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;

#[derive(Clone, Debug)]
pub enum GlobalKind {
    /// Index into `builtins::BUILTINS`.
    Builtin(u16),
    /// A built-in constant such as `pi`.
    Const,
    Fn,
    Ctor(CtorRef),
    Let,
    Var,
    Module(Rc<Module>),
}

#[derive(Clone, Debug)]
pub struct GlobalInfo {
    pub name: Name,
    pub kind: GlobalKind,
    pub span: Span,
    /// False for a top-level `let`/`var` until its declaration has been resolved.
    pub declared: bool,
}

/// The static signature of one user-defined function (for arity checks and
/// overload bookkeeping).
#[derive(Clone, Debug)]
pub struct FnSig {
    pub params: Vec<(Name, bool, Option<Ty>)>,
    pub span: Span,
}

impl FnSig {
    pub fn same_types(&self, other: &FnSig) -> bool {
        self.params.len() == other.params.len()
            && self.params.iter().zip(&other.params).all(|(a, b)| a.2.clone().unwrap_or(Ty::Any) == b.2.clone().unwrap_or(Ty::Any))
    }
}

pub struct Ctx {
    pub sm: SourceMap,
    pub globals: Vec<GlobalInfo>,
    pub types: Vec<Rc<TypeDef>>,
    pub builtins: Namespace,
    pub modules: HashMap<PathBuf, Rc<Module>>,
    pub loading: Vec<PathBuf>,
    /// Every record field name seen so far: `x.name(...)` may call a field.
    pub known_fields: HashSet<Name>,
    /// Signatures of user functions, per global slot (several when overloaded).
    pub sigs: HashMap<u32, Vec<FnSig>>,
    /// For types declared in an imported module: that module. Method calls
    /// on values of these types also look for functions in the module.
    pub type_home: HashMap<u32, Rc<Module>>,
    /// Names of all functions defined by imported modules.
    pub module_fns: HashSet<Name>,
}

impl Ctx {
    pub fn new() -> Ctx {
        let mut ctx = Ctx {
            sm: SourceMap::new(),
            globals: Vec::new(),
            types: builtin_types(),
            builtins: Namespace::default(),
            modules: HashMap::new(),
            loading: Vec::new(),
            known_fields: HashSet::new(),
            sigs: HashMap::new(),
            type_home: HashMap::new(),
            module_fns: HashSet::new(),
        };
        let types = ctx.types.clone();
        for td in &types {
            ctx.builtins.types.insert(td.name.clone(), td.id);
            if let TypeKind::Enum { variants } = &td.kind {
                for (tag, v) in variants.iter().enumerate() {
                    let slot = ctx.add_global(v.name.clone(), GlobalKind::Ctor(CtorRef { type_id: td.id, tag: tag as u32, is_record: false }), Span::default());
                    ctx.builtins.values.insert(v.name.clone(), slot);
                }
            }
        }
        ctx
    }

    pub fn add_global(&mut self, name: Name, kind: GlobalKind, span: Span) -> u32 {
        self.globals.push(GlobalInfo { name, kind, span, declared: true });
        (self.globals.len() - 1) as u32
    }

    pub fn type_by_id(&self, id: u32) -> &Rc<TypeDef> {
        &self.types[id as usize]
    }
}

impl Default for Ctx {
    fn default() -> Self {
        Ctx::new()
    }
}
