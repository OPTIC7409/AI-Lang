//! Random value generation and shrinking, driven by type annotations.
//! Used by `property` blocks and by `cogito verify` (contract fuzzing).

use crate::interp::{Interp, Rng};
use crate::types::{Ty, TypeDef, TypeKind};
use crate::value::*;
use std::rc::Rc;

const UNICODE: &[&str] = &["é", "ß", "中", "😀", "ñ", "Ω", "ü", "й"];

pub struct Gen<'a> {
    pub it: &'a Interp,
    pub rng: &'a mut Rng,
}

fn mentions(ty: &Ty, id: u32) -> bool {
    match ty {
        Ty::Named { id: i, args, .. } => *i == id || args.iter().any(|a| mentions(a, id)),
        Ty::List(t) => mentions(t, id),
        Ty::Map(k, v) => mentions(k, id) || mentions(v, id),
        Ty::Tuple(ts) => ts.iter().any(|t| mentions(t, id)),
        Ty::Record(fs) => fs.iter().any(|(_, t)| mentions(t, id)),
        _ => false,
    }
}

impl<'a> Gen<'a> {
    pub fn value(&mut self, ty: &Ty, size: u32, depth: u32) -> Result<Value, String> {
        let size = size.max(1) as i64;
        Ok(match ty {
            Ty::Unit => Value::Unit,
            Ty::Bool => Value::Bool(self.rng.next_u64() & 1 == 1),
            Ty::Int => {
                let r = self.rng.below(100);
                if r < 15 {
                    Value::Int([0, 1, -1, 2, -2, 3, 10, -10][self.rng.below(8)])
                } else if r < 85 {
                    Value::Int(self.rng.range(-size, size))
                } else if r < 97 {
                    Value::Int(self.rng.range(-1000, 1000))
                } else {
                    Value::Int(self.rng.range(-100_000, 100_000))
                }
            }
            Ty::Float => {
                let r = self.rng.below(100);
                if r < 20 {
                    const SPECIAL: [f64; 12] = [0.0, 1.0, -1.0, 0.5, 0.1, 0.2, 0.3, -0.1, 1e-9, 1e9, 2.5, 1.0 / 3.0];
                    Value::Float(SPECIAL[self.rng.below(SPECIAL.len())])
                } else if r < 85 {
                    let whole = self.rng.range(-size, size) as f64;
                    let frac = (self.rng.below(1000) as f64) / 1000.0;
                    Value::Float(whole + frac)
                } else {
                    Value::Float((self.rng.float() - 0.5) * 2.0e6)
                }
            }
            Ty::Str => {
                let n = self.rng.below(size as usize + 1);
                let mut s = String::new();
                for _ in 0..n {
                    let r = self.rng.below(100);
                    if r < 70 {
                        s.push((b'a' + self.rng.below(26) as u8) as char);
                    } else if r < 88 {
                        s.push((b' ' + self.rng.below(95) as u8) as char);
                    } else {
                        s.push_str(UNICODE[self.rng.below(UNICODE.len())]);
                    }
                }
                Value::str(s)
            }
            Ty::Range => {
                let start = self.rng.range(-size, size);
                let len = self.rng.range(0, size);
                Value::Range(Rc::new(RangeVal { start, end: Some(start + len) }))
            }
            Ty::List(t) => {
                let n = self.rng.below(size as usize + 1);
                let inner = (size as u32 / 2).max(2);
                let mut xs = Vec::with_capacity(n);
                // Bugs cluster around duplicates, so sometimes draw the
                // elements from a small pool of values.
                let mode = self.rng.below(100);
                if mode < 25 && n > 1 {
                    let pool_size = if mode < 10 { 1 } else { 2 + self.rng.below(2) };
                    let mut pool = Vec::with_capacity(pool_size);
                    for _ in 0..pool_size {
                        pool.push(self.value(t, inner, depth + 1)?);
                    }
                    for _ in 0..n {
                        xs.push(pool[self.rng.below(pool.len())].clone());
                    }
                } else {
                    for _ in 0..n {
                        xs.push(self.value(t, inner, depth + 1)?);
                    }
                }
                Value::list(xs)
            }
            Ty::Map(k, v) => {
                let n = self.rng.below(size as usize + 1);
                let inner = (size as u32 / 2).max(2);
                let mut m = MapVal::new();
                for _ in 0..n {
                    let key = self.value(k, inner, depth + 1)?;
                    let val = self.value(v, inner, depth + 1)?;
                    m.insert(key, val);
                }
                Value::Map(Rc::new(m))
            }
            Ty::Tuple(ts) => {
                let mut xs = Vec::new();
                for t in ts {
                    xs.push(self.value(t, size as u32, depth + 1)?);
                }
                Value::tuple(xs)
            }
            Ty::Record(fs) => {
                let mut names = Vec::new();
                let mut vals = Vec::new();
                for (n, t) in fs {
                    names.push(n.clone());
                    vals.push(self.value(t, size as u32, depth + 1)?);
                }
                Value::Record(Rc::new(RecordVal { ty: None, names: names.into(), values: vals }))
            }
            Ty::Named { id, args, .. } => {
                let td = self.it.ctx.types[*id as usize].clone();
                self.named(&td, args, size as u32, depth)?
            }
            Ty::Fn(..) => return Err(format!("cannot generate random functions (type `{}`)", ty)),
            Ty::Any | Ty::Generic(_) | Ty::Param(..) => {
                return Err(format!("cannot generate values of type `{}`; give the input a concrete type such as Int or List[Str]", ty))
            }
        })
    }

    fn named(&mut self, td: &Rc<TypeDef>, args: &[Ty], size: u32, depth: u32) -> Result<Value, String> {
        let args: Vec<Ty> = if args.is_empty() { td.params.iter().map(|_| Ty::Int).collect() } else { args.to_vec() };
        match &td.kind {
            TypeKind::Record { fields, tys } => {
                let mut vals = Vec::new();
                for t in tys {
                    vals.push(self.value(&t.subst(&args), size, depth + 1)?);
                }
                Ok(Value::Record(Rc::new(RecordVal { ty: Some(td.clone()), names: fields.clone(), values: vals })))
            }
            TypeKind::Enum { variants } => {
                let simple: Vec<usize> = (0..variants.len()).filter(|i| !variants[*i].tys.iter().any(|t| mentions(t, td.id))).collect();
                let tag = if (depth >= 4 || size <= 1) && !simple.is_empty() {
                    simple[self.rng.below(simple.len())]
                } else {
                    self.rng.below(variants.len())
                };
                let v = &variants[tag];
                let mut vals = Vec::new();
                for t in &v.tys {
                    vals.push(self.value(&t.subst(&args), (size / 2).max(1), depth + 1)?);
                }
                Ok(Value::Variant(Rc::new(VariantVal { ty: td.clone(), tag: tag as u32, values: vals })))
            }
        }
    }
}

/// Candidate "simpler" values, most aggressive first.
pub fn shrink(v: &Value) -> Vec<Value> {
    let mut out = Vec::new();
    match v {
        Value::Int(n) => {
            let n = *n;
            if n != 0 {
                out.push(Value::Int(0));
                if n < 0 && n != i64::MIN {
                    out.push(Value::Int(-n));
                }
                if n / 2 != 0 {
                    out.push(Value::Int(n / 2));
                }
                let toward = n - n.signum();
                if toward != 0 && toward != n / 2 {
                    out.push(Value::Int(toward));
                }
            }
        }
        Value::Float(f) => {
            let f = *f;
            if f != 0.0 {
                out.push(Value::Float(0.0));
                for nice in [1.0, 0.5, 0.1] {
                    if nice < f.abs() {
                        out.push(Value::Float(nice * f.signum()));
                    }
                }
                if f < 0.0 {
                    out.push(Value::Float(-f));
                }
                if f.trunc() != f {
                    out.push(Value::Float(f.trunc()));
                }
                if f.abs() > 1.0 {
                    out.push(Value::Float((f / 2.0).trunc()));
                }
            }
        }
        Value::Bool(true) => out.push(Value::Bool(false)),
        Value::Str(s) => {
            let cs: Vec<char> = s.chars().collect();
            if !cs.is_empty() {
                out.push(Value::str(""));
                if cs.len() > 1 {
                    out.push(Value::str(cs[..cs.len() / 2].iter().collect::<String>()));
                    out.push(Value::str(cs[cs.len() / 2..].iter().collect::<String>()));
                }
                for i in 0..cs.len().min(32) {
                    let mut c = cs.clone();
                    c.remove(i);
                    out.push(Value::str(c.into_iter().collect::<String>()));
                }
                for i in 0..cs.len().min(16) {
                    if cs[i] != 'a' {
                        let mut c = cs.clone();
                        c[i] = 'a';
                        out.push(Value::str(c.into_iter().collect::<String>()));
                    }
                }
            }
        }
        Value::List(xs) => {
            if !xs.is_empty() {
                out.push(Value::list(vec![]));
                if xs.len() > 1 {
                    out.push(Value::list(xs[..xs.len() / 2].to_vec()));
                    out.push(Value::list(xs[xs.len() / 2..].to_vec()));
                }
                for i in 0..xs.len().min(32) {
                    let mut c = (**xs).clone();
                    c.remove(i);
                    out.push(Value::list(c));
                }
                for i in 0..xs.len().min(16) {
                    for s in shrink(&xs[i]).into_iter().take(4) {
                        let mut c = (**xs).clone();
                        c[i] = s;
                        out.push(Value::list(c));
                    }
                }
            }
        }
        Value::Tuple(xs) => {
            for i in 0..xs.len() {
                for s in shrink(&xs[i]).into_iter().take(6) {
                    let mut c = (**xs).clone();
                    c[i] = s;
                    out.push(Value::tuple(c));
                }
            }
        }
        Value::Map(m) => {
            if !m.is_empty() {
                out.push(Value::Map(Rc::new(MapVal::new())));
                for (k, _) in m.entries.iter().take(16) {
                    let mut c = (**m).clone();
                    c.remove(k);
                    out.push(Value::Map(Rc::new(c)));
                }
                for (k, x) in m.entries.iter().take(8) {
                    for s in shrink(x).into_iter().take(3) {
                        let mut c = (**m).clone();
                        c.insert(k.clone(), s);
                        out.push(Value::Map(Rc::new(c)));
                    }
                }
            }
        }
        Value::Record(r) => {
            for i in 0..r.values.len() {
                for s in shrink(&r.values[i]).into_iter().take(6) {
                    let mut c = (**r).clone();
                    c.values[i] = s;
                    out.push(Value::Record(Rc::new(c)));
                }
            }
        }
        Value::Variant(vv) => {
            // Simpler variants: nullary ones declared earlier, then sub-terms of the same type.
            if let TypeKind::Enum { variants } = &vv.ty.kind {
                for (tag, var) in variants.iter().enumerate() {
                    if var.tys.is_empty() && (tag as u32) != vv.tag {
                        out.push(Value::Variant(Rc::new(VariantVal { ty: vv.ty.clone(), tag: tag as u32, values: vec![] })));
                    }
                }
            }
            for x in &vv.values {
                if let Value::Variant(sub) = x {
                    if sub.ty.id == vv.ty.id {
                        out.push(x.clone());
                    }
                }
            }
            for i in 0..vv.values.len() {
                for s in shrink(&vv.values[i]).into_iter().take(6) {
                    let mut c = (**vv).clone();
                    c.values[i] = s;
                    out.push(Value::Variant(Rc::new(c)));
                }
            }
        }
        Value::Range(r) => {
            if let Some(e) = r.end {
                if e > r.start {
                    out.push(Value::Range(Rc::new(RangeVal { start: r.start, end: Some(r.start) })));
                    out.push(Value::Range(Rc::new(RangeVal { start: r.start, end: Some(e - 1) })));
                }
            }
            if r.start != 0 {
                let len = r.end.map(|e| e - r.start);
                out.push(Value::Range(Rc::new(RangeVal { start: 0, end: len })));
            }
        }
        _ => {}
    }
    out
}
