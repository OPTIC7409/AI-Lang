//! Runtime values.
//!
//! Every Cogito value has *value semantics*: assigning a list to a new
//! variable and changing one of them never affects the other. This is
//! implemented with reference counting plus copy-on-write, so passing large
//! values around is cheap and mutation of an unshared value happens in place.

use crate::ast::{FnDef, Module};
use crate::types::{Name, TypeDef, OPTION_ID, RESULT_ID};
use std::cell::Cell;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::hash::{Hash, Hasher};
use std::ops::{Deref, DerefMut};
use std::rc::Rc;

#[derive(Clone, Default)]
pub enum Value {
    #[default]
    Unit,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(Rc<Text>),
    List(Rc<List>),
    Tuple(Rc<List>),
    Map(Rc<MapVal>),
    Record(Rc<RecordVal>),
    Variant(Rc<VariantVal>),
    Range(Rc<RangeVal>),
    Func(Rc<Closure>),
    /// Index into `builtins::BUILTINS`.
    Builtin(u16),
    /// Several functions sharing one name; the first whose parameter types
    /// accept the arguments is called.
    Overload(Rc<Vec<Value>>),
    /// A record constructor (tag 0) or an enum variant constructor.
    Ctor(Rc<TypeDef>, u32),
    Module(Rc<Module>),
}

/// The storage of a string. It derefs to `String`, and caches its length in
/// characters so that `len` and indexing are O(1) for ASCII text. Any
/// mutable access clears the cache.
pub struct Text {
    s: String,
    /// Character count plus one; 0 means "not yet computed".
    chars: Cell<usize>,
}

impl Text {
    pub fn new(s: String) -> Text {
        Text { s, chars: Cell::new(0) }
    }

    pub fn as_str(&self) -> &str {
        &self.s
    }

    pub fn char_len(&self) -> usize {
        let c = self.chars.get();
        if c != 0 {
            return c - 1;
        }
        let n = if self.s.is_ascii() { self.s.len() } else { self.s.chars().count() };
        self.chars.set(n + 1);
        n
    }

    pub fn is_ascii_text(&self) -> bool {
        self.char_len() == self.s.len()
    }

    /// The character at a character index.
    pub fn char_at(&self, i: usize) -> Option<&str> {
        if self.is_ascii_text() {
            return self.s.get(i..i + 1);
        }
        let (b, c) = self.s.char_indices().nth(i)?;
        Some(&self.s[b..b + c.len_utf8()])
    }

    /// The substring between two character indexes (already clamped).
    pub fn slice_chars(&self, a: usize, b: usize) -> &str {
        if self.is_ascii_text() {
            return &self.s[a..b];
        }
        let mut it = self.s.char_indices().map(|(i, _)| i).chain(std::iter::once(self.s.len()));
        let start = it.nth(a).unwrap_or(self.s.len());
        let end = if b > a { it.nth(b - a - 1).unwrap_or(self.s.len()) } else { start };
        &self.s[start..end]
    }
}

impl Deref for Text {
    type Target = String;
    fn deref(&self) -> &String {
        &self.s
    }
}

impl DerefMut for Text {
    fn deref_mut(&mut self) -> &mut String {
        self.chars.set(0);
        &mut self.s
    }
}

impl Clone for Text {
    fn clone(&self) -> Text {
        Text { s: self.s.clone(), chars: Cell::new(self.chars.get()) }
    }
}

impl PartialEq for Text {
    fn eq(&self, other: &Text) -> bool {
        self.s == other.s
    }
}

impl Eq for Text {}

impl Hash for Text {
    fn hash<H: Hasher>(&self, h: &mut H) {
        self.s.hash(h)
    }
}

impl std::fmt::Debug for Text {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.s.fmt(f)
    }
}

impl std::fmt::Display for Text {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.s.fmt(f)
    }
}

/// The storage of a list or tuple. It derefs to `Vec<Value>`; it also
/// remembers which type annotation its elements were last checked against,
/// so that passing the same unchanged list through many annotated calls costs
/// O(1) per call. Any mutable access clears that memo.
pub struct List {
    items: Vec<Value>,
    checked: Cell<u64>,
}

impl List {
    pub fn new(items: Vec<Value>) -> List {
        List { items, checked: Cell::new(0) }
    }

    pub fn into_vec(self) -> Vec<Value> {
        self.items
    }

    pub fn checked(&self) -> u64 {
        self.checked.get()
    }

    pub fn set_checked(&self, h: u64) {
        self.checked.set(h)
    }
}

impl Deref for List {
    type Target = Vec<Value>;
    fn deref(&self) -> &Vec<Value> {
        &self.items
    }
}

impl DerefMut for List {
    fn deref_mut(&mut self) -> &mut Vec<Value> {
        self.checked.set(0);
        &mut self.items
    }
}

impl Clone for List {
    fn clone(&self) -> List {
        List { items: self.items.clone(), checked: Cell::new(self.checked.get()) }
    }
}

/// Take the elements out of a shared list, copying only if it is shared.
pub fn list_into_vec(rc: Rc<List>) -> Vec<Value> {
    match Rc::try_unwrap(rc) {
        Ok(l) => l.items,
        Err(rc) => rc.items.clone(),
    }
}

pub struct Closure {
    pub def: Rc<FnDef>,
    pub captures: Vec<Value>,
}

#[derive(Clone)]
pub struct RecordVal {
    /// `None` for anonymous records like `{ x: 1 }`.
    pub ty: Option<Rc<TypeDef>>,
    pub names: Rc<[Name]>,
    pub values: Vec<Value>,
}

impl RecordVal {
    pub fn get(&self, name: &str) -> Option<&Value> {
        self.names.iter().position(|n| &**n == name).map(|i| &self.values[i])
    }
}

#[derive(Clone)]
pub struct VariantVal {
    pub ty: Rc<TypeDef>,
    pub tag: u32,
    pub values: Vec<Value>,
}

impl VariantVal {
    pub fn name(&self) -> Name {
        self.ty.ctor_name(self.tag)
    }
}

#[derive(Clone, Debug)]
pub struct RangeVal {
    pub start: i64,
    /// Exclusive end (an i128, so that `..=max_int` is representable);
    /// `None` for an unbounded range like `0..`.
    pub end: Option<i128>,
}

impl RangeVal {
    pub fn len(&self) -> Option<usize> {
        self.len_u128().map(|n| n.min(usize::MAX as u128) as usize)
    }

    pub fn len_u128(&self) -> Option<u128> {
        self.end.map(|e| if e > self.start as i128 { (e - self.start as i128) as u128 } else { 0 })
    }

    pub fn is_empty(&self) -> bool {
        self.len() == Some(0)
    }

    pub fn contains(&self, n: i64) -> bool {
        n >= self.start && self.end.is_none_or(|e| (n as i128) < e)
    }
}

/// Hashable wrapper used for map keys.
#[derive(Clone)]
pub struct HKey(pub Value);

impl PartialEq for HKey {
    fn eq(&self, other: &Self) -> bool {
        // As map keys, NaN equals NaN (otherwise a NaN key could never be found).
        match (&self.0, &other.0) {
            (Value::Float(a), Value::Float(b)) if a.is_nan() && b.is_nan() => true,
            (a, b) => values_equal(a, b),
        }
    }
}

/// Exact comparison of an Int with a Float (no rounding through f64).
pub fn cmp_int_float(x: i64, y: f64) -> Option<Ordering> {
    if y.is_nan() {
        return None;
    }
    if y >= i64::MAX as f64 {
        return Some(Ordering::Less);
    }
    if y < i64::MIN as f64 {
        return Some(Ordering::Greater);
    }
    let t = y.trunc();
    match x.cmp(&(t as i64)) {
        Ordering::Equal => Some(if y > t {
            Ordering::Less
        } else if y < t {
            Ordering::Greater
        } else {
            Ordering::Equal
        }),
        o => Some(o),
    }
}

impl Eq for HKey {}

impl Hash for HKey {
    fn hash<H: Hasher>(&self, h: &mut H) {
        hash_value(&self.0, h)
    }
}

fn hash_value<H: Hasher>(v: &Value, h: &mut H) {
    match v {
        Value::Unit => 0u8.hash(h),
        Value::Bool(b) => {
            1u8.hash(h);
            b.hash(h)
        }
        Value::Int(i) => {
            2u8.hash(h);
            i.hash(h)
        }
        Value::Float(f) => {
            if f.fract() == 0.0 && f.abs() < 9.0e18 {
                2u8.hash(h);
                (*f as i64).hash(h)
            } else {
                3u8.hash(h);
                f.to_bits().hash(h)
            }
        }
        Value::Str(s) => {
            4u8.hash(h);
            s.hash(h)
        }
        Value::List(xs) | Value::Tuple(xs) => {
            5u8.hash(h);
            xs.len().hash(h);
            for x in xs.iter() {
                hash_value(x, h);
            }
        }
        Value::Map(m) => {
            6u8.hash(h);
            m.len().hash(h);
        }
        Value::Record(r) => {
            7u8.hash(h);
            r.values.len().hash(h);
        }
        Value::Variant(v) => {
            8u8.hash(h);
            v.ty.id.hash(h);
            v.tag.hash(h);
            for x in &v.values {
                hash_value(x, h);
            }
        }
        Value::Range(r) => {
            9u8.hash(h);
            r.start.hash(h);
            r.end.hash(h);
        }
        Value::Func(f) => {
            10u8.hash(h);
            (Rc::as_ptr(f) as usize).hash(h)
        }
        Value::Builtin(i) => {
            11u8.hash(h);
            i.hash(h)
        }
        _ => 12u8.hash(h),
    }
}

/// An insertion-ordered hash map.
#[derive(Clone, Default)]
pub struct MapVal {
    pub entries: Vec<(Value, Value)>,
    index: HashMap<HKey, usize>,
    /// Memo of the last type annotation the map was checked against (see `List`).
    checked: Cell<u64>,
}

impl MapVal {
    pub fn new() -> MapVal {
        MapVal::default()
    }

    pub fn with_capacity(n: usize) -> MapVal {
        MapVal { entries: Vec::with_capacity(n), index: HashMap::with_capacity(n), checked: Cell::new(0) }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn get(&self, k: &Value) -> Option<&Value> {
        self.index.get(&HKey(k.clone())).map(|&i| &self.entries[i].1)
    }

    pub fn checked(&self) -> u64 {
        self.checked.get()
    }

    pub fn set_checked(&self, h: u64) {
        self.checked.set(h)
    }

    pub fn get_mut(&mut self, k: &Value) -> Option<&mut Value> {
        self.checked.set(0);
        match self.index.get(&HKey(k.clone())) {
            Some(&i) => Some(&mut self.entries[i].1),
            None => None,
        }
    }

    pub fn contains(&self, k: &Value) -> bool {
        self.index.contains_key(&HKey(k.clone()))
    }

    pub fn insert(&mut self, k: Value, v: Value) -> Option<Value> {
        self.checked.set(0);
        match self.index.get(&HKey(k.clone())) {
            Some(&i) => Some(std::mem::replace(&mut self.entries[i].1, v)),
            None => {
                self.index.insert(HKey(k.clone()), self.entries.len());
                self.entries.push((k, v));
                None
            }
        }
    }

    pub fn remove(&mut self, k: &Value) -> Option<Value> {
        self.checked.set(0);
        let i = self.index.remove(&HKey(k.clone()))?;
        let (_, v) = self.entries.remove(i);
        for idx in self.index.values_mut() {
            if *idx > i {
                *idx -= 1;
            }
        }
        Some(v)
    }

    pub fn clear(&mut self) {
        self.checked.set(0);
        self.entries.clear();
        self.index.clear();
    }
}

impl Value {
    pub fn str(s: impl Into<String>) -> Value {
        Value::Str(Rc::new(Text::new(s.into())))
    }

    pub fn list(v: Vec<Value>) -> Value {
        Value::List(Rc::new(List::new(v)))
    }

    pub fn tuple(v: Vec<Value>) -> Value {
        Value::Tuple(Rc::new(List::new(v)))
    }

    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Int(i) => Some(*i as f64),
            Value::Float(f) => Some(*f),
            _ => None,
        }
    }

    pub fn is_callable(&self) -> bool {
        matches!(self, Value::Func(_) | Value::Builtin(_) | Value::Overload(_) | Value::Ctor(..))
    }

    /// For Option/Result values: (type id, tag, payload).
    pub fn as_variant(&self, type_id: u32) -> Option<(u32, Option<&Value>)> {
        match self {
            Value::Variant(v) if v.ty.id == type_id => Some((v.tag, v.values.first())),
            _ => None,
        }
    }

    pub fn is_option(&self) -> bool {
        matches!(self, Value::Variant(v) if v.ty.id == OPTION_ID)
    }

    pub fn is_result(&self) -> bool {
        matches!(self, Value::Variant(v) if v.ty.id == RESULT_ID)
    }
}

pub fn type_name(v: &Value) -> String {
    match v {
        Value::Unit => "Unit".into(),
        Value::Bool(_) => "Bool".into(),
        Value::Int(_) => "Int".into(),
        Value::Float(_) => "Float".into(),
        Value::Str(_) => "Str".into(),
        Value::List(_) => "List".into(),
        Value::Tuple(_) => "Tuple".into(),
        Value::Map(_) => "Map".into(),
        Value::Record(r) => match &r.ty {
            Some(t) => t.name.to_string(),
            None => "Record".into(),
        },
        Value::Variant(v) => v.ty.name.to_string(),
        Value::Range(_) => "Range".into(),
        Value::Func(_) | Value::Builtin(_) | Value::Overload(_) => "Fn".into(),
        Value::Ctor(..) => "Fn".into(),
        Value::Module(_) => "Module".into(),
    }
}

pub fn values_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Unit, Value::Unit) => true,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Int(x), Value::Int(y)) => x == y,
        (Value::Float(x), Value::Float(y)) => x == y,
        (Value::Int(x), Value::Float(y)) | (Value::Float(y), Value::Int(x)) => cmp_int_float(*x, *y) == Some(Ordering::Equal),
        (Value::Str(x), Value::Str(y)) => Rc::ptr_eq(x, y) || x == y,
        (Value::List(x), Value::List(y)) | (Value::Tuple(x), Value::Tuple(y)) => {
            Rc::ptr_eq(x, y) || (x.len() == y.len() && x.iter().zip(y.iter()).all(|(a, b)| values_equal(a, b)))
        }
        (Value::Map(x), Value::Map(y)) => {
            Rc::ptr_eq(x, y) || (x.len() == y.len() && x.entries.iter().all(|(k, v)| y.get(k).is_some_and(|w| values_equal(v, w))))
        }
        (Value::Record(x), Value::Record(y)) => {
            let same_ty = match (&x.ty, &y.ty) {
                (Some(a), Some(b)) => a.id == b.id,
                (None, None) => true,
                _ => false,
            };
            same_ty && x.values.len() == y.values.len() && x.names.iter().zip(&x.values).all(|(n, v)| y.get(n).is_some_and(|w| values_equal(v, w)))
        }
        (Value::Variant(x), Value::Variant(y)) => {
            x.ty.id == y.ty.id && x.tag == y.tag && x.values.iter().zip(&y.values).all(|(a, b)| values_equal(a, b))
        }
        (Value::Range(x), Value::Range(y)) => x.start == y.start && x.end == y.end,
        (Value::Func(x), Value::Func(y)) => Rc::ptr_eq(x, y),
        (Value::Builtin(x), Value::Builtin(y)) => x == y,
        (Value::Ctor(t1, a), Value::Ctor(t2, b)) => t1.id == t2.id && a == b,
        (Value::Module(x), Value::Module(y)) => Rc::ptr_eq(x, y),
        _ => false,
    }
}

/// Ordering between two values, or `None` if they cannot be compared.
pub fn compare(a: &Value, b: &Value) -> Option<Ordering> {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => Some(x.cmp(y)),
        (Value::Float(x), Value::Float(y)) => x.partial_cmp(y),
        (Value::Int(x), Value::Float(y)) => cmp_int_float(*x, *y),
        (Value::Float(x), Value::Int(y)) => cmp_int_float(*y, *x).map(Ordering::reverse),
        (Value::Str(x), Value::Str(y)) => Some(x.as_str().cmp(y.as_str())),
        (Value::Bool(x), Value::Bool(y)) => Some(x.cmp(y)),
        (Value::Unit, Value::Unit) => Some(Ordering::Equal),
        (Value::List(x), Value::List(y)) | (Value::Tuple(x), Value::Tuple(y)) => {
            for (a, b) in x.iter().zip(y.iter()) {
                match compare(a, b)? {
                    Ordering::Equal => continue,
                    o => return Some(o),
                }
            }
            Some(x.len().cmp(&y.len()))
        }
        (Value::Variant(x), Value::Variant(y)) if x.ty.id == y.ty.id => {
            if x.tag != y.tag {
                return Some(x.tag.cmp(&y.tag));
            }
            for (a, b) in x.values.iter().zip(&y.values) {
                match compare(a, b)? {
                    Ordering::Equal => continue,
                    o => return Some(o),
                }
            }
            Some(Ordering::Equal)
        }
        (Value::Record(x), Value::Record(y)) if x.ty.as_ref().map(|t| t.id) == y.ty.as_ref().map(|t| t.id) && x.names == y.names => {
            for (a, b) in x.values.iter().zip(&y.values) {
                match compare(a, b)? {
                    Ordering::Equal => continue,
                    o => return Some(o),
                }
            }
            Some(Ordering::Equal)
        }
        _ => None,
    }
}

pub fn format_float(f: f64) -> String {
    if f.is_nan() {
        return "nan".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "inf".into() } else { "-inf".into() };
    }
    let a = f.abs();
    if a != 0.0 && !(1e-5..1e16).contains(&a) {
        return format!("{:e}", f);
    }
    if f.fract() == 0.0 {
        return format!("{:.1}", f);
    }
    format!("{}", f)
}

pub fn escape_str(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            '\0' => out.push_str("\\0"),
            '{' => out.push_str("\\{"),
            '}' => out.push_str("\\}"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{{{:x}}}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Append the textual form of a value. Top-level strings are written raw
/// when `quote` is false; nested strings are always quoted.
pub fn write_value(out: &mut String, v: &Value, quote: bool) {
    match v {
        Value::Unit => out.push_str("()"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Int(i) => {
            let _ = write!(out, "{}", i);
        }
        Value::Float(f) => out.push_str(&format_float(*f)),
        Value::Str(s) => {
            if quote {
                escape_str(s, out)
            } else {
                out.push_str(s)
            }
        }
        Value::List(xs) => {
            out.push('[');
            for (i, x) in xs.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_value(out, x, true);
            }
            out.push(']');
        }
        Value::Tuple(xs) => {
            out.push('(');
            for (i, x) in xs.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_value(out, x, true);
            }
            if xs.len() == 1 {
                out.push(',');
            }
            out.push(')');
        }
        Value::Map(m) => {
            if m.is_empty() {
                out.push_str("[:]");
                return;
            }
            out.push('[');
            for (i, (k, v)) in m.entries.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_value(out, k, true);
                out.push_str(": ");
                write_value(out, v, true);
            }
            out.push(']');
        }
        Value::Record(r) => {
            match &r.ty {
                Some(t) => {
                    out.push_str(&t.name);
                    out.push('(');
                }
                None => out.push_str("{ "),
            }
            for (i, (n, v)) in r.names.iter().zip(&r.values).enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(n);
                out.push_str(": ");
                write_value(out, v, true);
            }
            match &r.ty {
                Some(_) => out.push(')'),
                None => out.push_str(if r.names.is_empty() { "}" } else { " }" }),
            }
        }
        Value::Variant(v) => {
            let td = &v.ty;
            let (fields, _, named) = td.fields_of(v.tag);
            out.push_str(&v.name());
            if !v.values.is_empty() {
                out.push('(');
                for (i, x) in v.values.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    if named {
                        out.push_str(&fields[i]);
                        out.push_str(": ");
                    }
                    write_value(out, x, true);
                }
                out.push(')');
            }
        }
        Value::Range(r) => match r.end {
            Some(e) if e > i64::MAX as i128 => {
                let _ = write!(out, "{}..={}", r.start, e - 1);
            }
            Some(e) => {
                let _ = write!(out, "{}..{}", r.start, e);
            }
            None => {
                let _ = write!(out, "{}..", r.start);
            }
        },
        Value::Func(c) => {
            let _ = write!(out, "<fn {}>", c.def.display_name());
        }
        Value::Builtin(i) => {
            let _ = write!(out, "<fn {}>", crate::builtins::BUILTINS[*i as usize].name);
        }
        Value::Overload(fs) => {
            let name = fs.first().map(display).unwrap_or_default();
            let _ = write!(out, "{} (+{} overloads)", name, fs.len().saturating_sub(1));
        }
        Value::Ctor(td, tag) => {
            let _ = write!(out, "<constructor {}>", td.ctor_name(*tag));
        }
        Value::Module(m) => {
            let _ = write!(out, "<module {}>", m.name);
        }
    }
}

/// How a value prints: strings without quotes.
pub fn display(v: &Value) -> String {
    let mut s = String::new();
    write_value(&mut s, v, false);
    s
}

/// The source-like form of a value: strings quoted and escaped.
pub fn repr(v: &Value) -> String {
    let mut s = String::new();
    write_value(&mut s, v, true);
    s
}

/// A shortened repr for error messages.
pub fn short_repr(v: &Value) -> String {
    let r = repr(v);
    if r.chars().count() > 80 {
        let cut: String = r.chars().take(77).collect();
        format!("{}...", cut)
    } else {
        r
    }
}

/// "Int 5", "Str \"hi\"", ...
pub fn describe(v: &Value) -> String {
    match v {
        Value::Unit => "()".into(),
        Value::Func(_) | Value::Builtin(_) | Value::Overload(_) | Value::Ctor(..) | Value::Module(_) => short_repr(v),
        Value::Variant(_) | Value::Record(_) => short_repr(v),
        _ => format!("{} {}", type_name(v), short_repr(v)),
    }
}
