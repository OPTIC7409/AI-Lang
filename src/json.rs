//! A small JSON value type with a parser and a printer, for the language
//! server protocol (the language's own `parse_json`/`to_json` work on
//! Cogito values instead).

use std::fmt::Write;

#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    pub fn get(&self, key: &str) -> &Json {
        match self {
            Json::Obj(fields) => fields.iter().find(|(k, _)| k == key).map_or(&Json::Null, |(_, v)| v),
            _ => &Json::Null,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Json::Num(n) if *n >= 0.0 && n.fract() == 0.0 => Some(*n as u64),
            _ => None,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Json::Null)
    }

    pub fn obj(fields: Vec<(&str, Json)>) -> Json {
        Json::Obj(fields.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
    }

    pub fn str(s: impl Into<String>) -> Json {
        Json::Str(s.into())
    }

    pub fn num(n: impl Into<f64>) -> Json {
        Json::Num(n.into())
    }

    pub fn parse(s: &str) -> Result<Json, String> {
        let mut p = Parser { b: s.as_bytes(), pos: 0, s };
        let v = p.value(0)?;
        p.ws();
        if p.pos != p.b.len() {
            return Err(format!("unexpected text at byte {}", p.pos));
        }
        Ok(v)
    }
}

impl std::fmt::Display for Json {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Json::Null => f.write_str("null"),
            Json::Bool(b) => write!(f, "{}", b),
            Json::Num(n) => {
                if n.fract() == 0.0 && n.abs() < 1e15 {
                    write!(f, "{}", *n as i64)
                } else {
                    write!(f, "{}", n)
                }
            }
            Json::Str(s) => {
                f.write_char('"')?;
                for c in s.chars() {
                    match c {
                        '"' => f.write_str("\\\"")?,
                        '\\' => f.write_str("\\\\")?,
                        '\n' => f.write_str("\\n")?,
                        '\r' => f.write_str("\\r")?,
                        '\t' => f.write_str("\\t")?,
                        c if (c as u32) < 0x20 => write!(f, "\\u{:04x}", c as u32)?,
                        c => f.write_char(c)?,
                    }
                }
                f.write_char('"')
            }
            Json::Arr(xs) => {
                f.write_char('[')?;
                for (i, x) in xs.iter().enumerate() {
                    if i > 0 {
                        f.write_char(',')?;
                    }
                    write!(f, "{}", x)?;
                }
                f.write_char(']')
            }
            Json::Obj(fields) => {
                f.write_char('{')?;
                for (i, (k, v)) in fields.iter().enumerate() {
                    if i > 0 {
                        f.write_char(',')?;
                    }
                    write!(f, "{}:{}", Json::Str(k.clone()), v)?;
                }
                f.write_char('}')
            }
        }
    }
}

struct Parser<'a> {
    b: &'a [u8],
    s: &'a str,
    pos: usize,
}

impl Parser<'_> {
    fn ws(&mut self) {
        while self.pos < self.b.len() && matches!(self.b[self.pos], b' ' | b'\t' | b'\n' | b'\r') {
            self.pos += 1;
        }
    }

    fn eat(&mut self, c: u8) -> bool {
        self.ws();
        if self.b.get(self.pos) == Some(&c) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn value(&mut self, depth: usize) -> Result<Json, String> {
        if depth > 256 {
            return Err("JSON nested too deeply".into());
        }
        self.ws();
        match self.b.get(self.pos) {
            Some(b'{') => {
                self.pos += 1;
                let mut fields = Vec::new();
                if self.eat(b'}') {
                    return Ok(Json::Obj(fields));
                }
                loop {
                    self.ws();
                    let k = match self.value(depth + 1)? {
                        Json::Str(k) => k,
                        _ => return Err("object keys must be strings".into()),
                    };
                    if !self.eat(b':') {
                        return Err("expected `:`".into());
                    }
                    let v = self.value(depth + 1)?;
                    fields.push((k, v));
                    if self.eat(b',') {
                        continue;
                    }
                    if self.eat(b'}') {
                        return Ok(Json::Obj(fields));
                    }
                    return Err("expected `,` or `}`".into());
                }
            }
            Some(b'[') => {
                self.pos += 1;
                let mut xs = Vec::new();
                if self.eat(b']') {
                    return Ok(Json::Arr(xs));
                }
                loop {
                    xs.push(self.value(depth + 1)?);
                    if self.eat(b',') {
                        continue;
                    }
                    if self.eat(b']') {
                        return Ok(Json::Arr(xs));
                    }
                    return Err("expected `,` or `]`".into());
                }
            }
            Some(b'"') => {
                self.pos += 1;
                let mut out = String::new();
                loop {
                    let start = self.pos;
                    while self.pos < self.b.len() && self.b[self.pos] != b'"' && self.b[self.pos] != b'\\' {
                        self.pos += 1;
                    }
                    out.push_str(&self.s[start..self.pos]);
                    match self.b.get(self.pos) {
                        Some(b'"') => {
                            self.pos += 1;
                            return Ok(Json::Str(out));
                        }
                        Some(b'\\') => {
                            let e = *self.b.get(self.pos + 1).ok_or("unterminated escape")?;
                            self.pos += 2;
                            match e {
                                b'n' => out.push('\n'),
                                b't' => out.push('\t'),
                                b'r' => out.push('\r'),
                                b'b' => out.push('\u{8}'),
                                b'f' => out.push('\u{c}'),
                                b'u' => {
                                    let mut code = self.hex4()?;
                                    // A surrogate pair encodes one character.
                                    if (0xD800..0xDC00).contains(&code) && self.b.get(self.pos..self.pos + 2) == Some(b"\\u") {
                                        self.pos += 2;
                                        let low = self.hex4()?;
                                        code = 0x10000 + ((code - 0xD800) << 10) + (low.wrapping_sub(0xDC00) & 0x3FF);
                                    }
                                    out.push(char::from_u32(code).unwrap_or('\u{FFFD}'));
                                }
                                c => out.push(c as char),
                            }
                        }
                        _ => return Err("unterminated string".into()),
                    }
                }
            }
            Some(b't') if self.b[self.pos..].starts_with(b"true") => {
                self.pos += 4;
                Ok(Json::Bool(true))
            }
            Some(b'f') if self.b[self.pos..].starts_with(b"false") => {
                self.pos += 5;
                Ok(Json::Bool(false))
            }
            Some(b'n') if self.b[self.pos..].starts_with(b"null") => {
                self.pos += 4;
                Ok(Json::Null)
            }
            Some(c) if *c == b'-' || c.is_ascii_digit() => {
                let start = self.pos;
                self.pos += 1;
                while self.pos < self.b.len() && matches!(self.b[self.pos], b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-') {
                    self.pos += 1;
                }
                self.s[start..self.pos].parse::<f64>().map(Json::Num).map_err(|_| "bad number".into())
            }
            _ => Err(format!("unexpected character at byte {}", self.pos)),
        }
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let h = self.s.get(self.pos..self.pos + 4).ok_or("bad \\u escape")?;
        self.pos += 4;
        u32::from_str_radix(h, 16).map_err(|_| "bad \\u escape".into())
    }
}

#[cfg(test)]
mod tests {
    use super::Json;

    #[test]
    fn round_trip() {
        let src = r#"{"a":[1,2.5,true,null],"b":"x\"y\né😀","c":{}}"#;
        let v = Json::parse(src).unwrap();
        assert_eq!(v.get("b").as_str(), Some("x\"y\né😀"));
        assert_eq!(Json::parse(&v.to_string()).unwrap(), v);
    }
}
