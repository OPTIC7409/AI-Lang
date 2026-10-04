//! Runtime types: the meaning of type annotations, and user type definitions.

use crate::span::Span;
use std::fmt;
use std::rc::Rc;

pub type Name = Rc<str>;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Ty {
    Any,
    Unit,
    Bool,
    Int,
    Float,
    Str,
    Range,
    List(Box<Ty>),
    Map(Box<Ty>, Box<Ty>),
    Set(Box<Ty>),
    Tuple(Vec<Ty>),
    Record(Vec<(Name, Ty)>),
    Fn(Vec<Ty>, Box<Ty>),
    /// A user-declared (or built-in Option/Result) type.
    Named {
        id: u32,
        name: Name,
        args: Vec<Ty>,
    },
    /// The i-th type parameter of the enclosing type declaration.
    Param(u32, Name),
    /// A function-level generic parameter: accepts anything.
    Generic(Name),
}

impl Ty {
    /// Substitute type-declaration parameters with concrete arguments.
    pub fn subst(&self, args: &[Ty]) -> Ty {
        match self {
            Ty::Param(i, _) => args.get(*i as usize).cloned().unwrap_or(Ty::Any),
            Ty::List(t) => Ty::List(Box::new(t.subst(args))),
            Ty::Map(k, v) => Ty::Map(Box::new(k.subst(args)), Box::new(v.subst(args))),
            Ty::Set(t) => Ty::Set(Box::new(t.subst(args))),
            Ty::Tuple(ts) => Ty::Tuple(ts.iter().map(|t| t.subst(args)).collect()),
            Ty::Record(fs) => Ty::Record(fs.iter().map(|(n, t)| (n.clone(), t.subst(args))).collect()),
            Ty::Fn(ps, r) => Ty::Fn(ps.iter().map(|t| t.subst(args)).collect(), Box::new(r.subst(args))),
            Ty::Named { id, name, args: a } => Ty::Named { id: *id, name: name.clone(), args: a.iter().map(|t| t.subst(args)).collect() },
            other => other.clone(),
        }
    }

    /// A non-zero fingerprint of this type, used to memoize annotation checks.
    /// A number that identifies the type exactly (never 0), used to
    /// remember that a collection was already checked against it. Types are
    /// interned, so two different types never share a number, as two
    /// hashes could. (The lookup is on every checked write, so it uses a
    /// fast hash rather than the default SipHash.)
    pub fn fingerprint(&self) -> u64 {
        INTERNED.with(|m| {
            let mut m = m.borrow_mut();
            if let Some(&id) = m.get(self) {
                return id;
            }
            let id = m.len() as u64 + 1;
            m.insert(self.clone(), id);
            id
        })
    }

    pub fn is_any(&self) -> bool {
        matches!(self, Ty::Any | Ty::Generic(_) | Ty::Param(..))
    }
}

impl fmt::Display for Ty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Ty::Any => write!(f, "Any"),
            Ty::Unit => write!(f, "Unit"),
            Ty::Bool => write!(f, "Bool"),
            Ty::Int => write!(f, "Int"),
            Ty::Float => write!(f, "Float"),
            Ty::Str => write!(f, "Str"),
            Ty::Range => write!(f, "Range"),
            Ty::List(t) => write!(f, "List[{}]", t),
            Ty::Map(k, v) => write!(f, "Map[{}, {}]", k, v),
            Ty::Set(t) => write!(f, "Set[{}]", t),
            Ty::Tuple(ts) => {
                write!(f, "(")?;
                for (i, t) in ts.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", t)?;
                }
                write!(f, ")")
            }
            Ty::Record(fs) => {
                write!(f, "{{ ")?;
                // (The type checker marks a record literal's exact type with
                // a field named "", which is not shown.)
                for (i, (n, t)) in fs.iter().filter(|(n, _)| !n.is_empty()).enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}: {}", n, t)?;
                }
                write!(f, " }}")
            }
            Ty::Fn(ps, r) => {
                write!(f, "fn(")?;
                for (i, t) in ps.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", t)?;
                }
                write!(f, ") -> {}", r)
            }
            Ty::Named { name, args, .. } => {
                write!(f, "{}", name)?;
                if !args.is_empty() {
                    write!(f, "[")?;
                    for (i, t) in args.iter().enumerate() {
                        if i > 0 {
                            write!(f, ", ")?;
                        }
                        write!(f, "{}", t)?;
                    }
                    write!(f, "]")?;
                }
                Ok(())
            }
            Ty::Param(_, n) | Ty::Generic(n) => write!(f, "{}", n),
        }
    }
}

#[derive(Debug)]
pub struct VariantDef {
    pub name: Name,
    pub fields: Rc<[Name]>,
    pub tys: Vec<Ty>,
    /// Whether the fields were declared with names (`Circle(radius: Float)`)
    /// or positionally (`Some(T)`).
    pub named: bool,
}

#[derive(Debug)]
pub enum TypeKind {
    Record { fields: Rc<[Name]>, tys: Vec<Ty> },
    Enum { variants: Vec<VariantDef> },
}

#[derive(Debug)]
pub struct TypeDef {
    pub id: u32,
    pub name: Name,
    pub params: Vec<Name>,
    pub kind: TypeKind,
    pub span: Span,
}

impl TypeDef {
    pub fn variant(&self, tag: u32) -> Option<&VariantDef> {
        match &self.kind {
            TypeKind::Enum { variants } => variants.get(tag as usize),
            _ => None,
        }
    }

    pub fn is_enum(&self) -> bool {
        matches!(self.kind, TypeKind::Enum { .. })
    }

    /// Field names and types for the record, or for the given variant.
    pub fn fields_of(&self, tag: u32) -> (&Rc<[Name]>, &[Ty], bool) {
        match &self.kind {
            TypeKind::Record { fields, tys } => (fields, tys, true),
            TypeKind::Enum { variants } => {
                let v = &variants[tag as usize];
                (&v.fields, &v.tys, v.named)
            }
        }
    }

    pub fn ctor_name(&self, tag: u32) -> Name {
        match &self.kind {
            TypeKind::Record { .. } => self.name.clone(),
            TypeKind::Enum { variants } => variants[tag as usize].name.clone(),
        }
    }
}

/// Ids of the built-in enum types.
pub const OPTION_ID: u32 = 0;
pub const RESULT_ID: u32 = 1;
pub const ORDERING_ID: u32 = 2;

/// Create the built-in type definitions that every program starts with.
pub fn builtin_types() -> Vec<Rc<TypeDef>> {
    let t: Name = Rc::from("T");
    let e: Name = Rc::from("E");
    let pos = |n: usize| -> Rc<[Name]> { (0..n).map(|i| Rc::from(i.to_string().as_str())).collect::<Vec<Name>>().into() };
    let option = TypeDef {
        id: OPTION_ID,
        name: Rc::from("Option"),
        params: vec![t.clone()],
        kind: TypeKind::Enum {
            variants: vec![
                VariantDef { name: Rc::from("Some"), fields: pos(1), tys: vec![Ty::Param(0, t.clone())], named: false },
                VariantDef { name: Rc::from("None"), fields: pos(0), tys: vec![], named: false },
            ],
        },
        span: Span::default(),
    };
    let result = TypeDef {
        id: RESULT_ID,
        name: Rc::from("Result"),
        params: vec![t.clone(), e.clone()],
        kind: TypeKind::Enum {
            variants: vec![
                VariantDef { name: Rc::from("Ok"), fields: pos(1), tys: vec![Ty::Param(0, t)], named: false },
                VariantDef { name: Rc::from("Err"), fields: pos(1), tys: vec![Ty::Param(1, e)], named: false },
            ],
        },
        span: Span::default(),
    };
    let ordering = TypeDef {
        id: ORDERING_ID,
        name: Rc::from("Ordering"),
        params: vec![],
        kind: TypeKind::Enum {
            variants: vec![
                VariantDef { name: Rc::from("Less"), fields: pos(0), tys: vec![], named: false },
                VariantDef { name: Rc::from("Equal"), fields: pos(0), tys: vec![], named: false },
                VariantDef { name: Rc::from("Greater"), fields: pos(0), tys: vec![], named: false },
            ],
        },
        span: Span::default(),
    };
    vec![Rc::new(option), Rc::new(result), Rc::new(ordering)]
}

thread_local! {
    static INTERNED: std::cell::RefCell<std::collections::HashMap<Ty, u64, std::hash::BuildHasherDefault<FastHasher>>> = Default::default();
}

/// A small, fast, non-cryptographic hasher (the multiply-rotate step of
/// FxHash, with a final mix so that the low bits depend on every input).
#[derive(Default)]
struct FastHasher(u64);

impl std::hash::Hasher for FastHasher {
    fn write(&mut self, bytes: &[u8]) {
        for chunk in bytes.chunks(8) {
            let mut w = [0u8; 8];
            w[..chunk.len()].copy_from_slice(chunk);
            self.write_u64(u64::from_le_bytes(w));
        }
    }

    fn write_u64(&mut self, n: u64) {
        self.0 = (self.0.rotate_left(5) ^ n).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
    }

    fn write_u32(&mut self, n: u32) {
        self.write_u64(n as u64);
    }

    fn write_u8(&mut self, n: u8) {
        self.write_u64(n as u64);
    }

    fn write_usize(&mut self, n: usize) {
        self.write_u64(n as u64);
    }

    fn finish(&self) -> u64 {
        let mut x = self.0;
        x ^= x >> 33;
        x = x.wrapping_mul(0xff51_afd7_ed55_8ccd);
        x ^= x >> 33;
        x
    }
}
