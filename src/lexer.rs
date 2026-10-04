//! The lexer turns source text into tokens.
//!
//! Newlines are significant: they end statements. To keep this predictable,
//! the lexer only emits a newline token when the innermost open bracket is a
//! brace (or there is none). Inside `(...)` and `[...]` newlines are ignored.
//! A newline is also dropped when the next line starts with `.`, `|>`, `and`
//! or `or`, so that method chains and pipelines can span lines.

use crate::diagnostic::Diagnostic;
use crate::span::Span;
use std::rc::Rc;

#[derive(Clone, Debug, PartialEq)]
pub enum Tok {
    Int(i64),
    Float(f64),
    Str(Vec<StrPart>),
    /// Starts with a lowercase letter or `_`; may end with `!`.
    Ident(Rc<str>),
    /// Starts with an uppercase letter.
    Upper(Rc<str>),
    // keywords
    Let,
    Var,
    Fn,
    Return,
    If,
    Else,
    While,
    For,
    In,
    Is,
    Loop,
    Break,
    Continue,
    Match,
    Type,
    Test,
    Property,
    Requires,
    Ensures,
    And,
    Or,
    Not,
    True,
    False,
    Import,
    As,
    Assert,
    Where,
    // punctuation
    LParen,
    RParen,
    LBracket,
    RBracket,
    LBrace,
    RBrace,
    Comma,
    Colon,
    Semi,
    Dot,
    DotDot,
    DotDotEq,
    Question,
    Arrow,
    FatArrow,
    Assign,
    PlusAssign,
    MinusAssign,
    StarAssign,
    SlashAssign,
    PercentAssign,
    SlashSlashAssign,
    StarStarAssign,
    EqEq,
    NotEq,
    Lt,
    Le,
    Gt,
    Ge,
    Plus,
    Minus,
    Star,
    StarStar,
    Slash,
    SlashSlash,
    Percent,
    PipeGt,
    Bar,
    At,
    Newline,
    Eof,
}

#[derive(Clone, Debug, PartialEq)]
pub enum StrPart {
    Lit(String),
    /// An interpolated expression: byte range in the source, plus an optional format spec.
    Expr {
        start: u32,
        end: u32,
        spec: Option<String>,
    },
}

#[derive(Clone, Debug)]
pub struct Token {
    pub tok: Tok,
    pub span: Span,
}

impl Tok {
    /// Whether the token is a binary arithmetic or comparison operator.
    pub fn is_binary_operator(&self) -> bool {
        matches!(
            self,
            Tok::Plus
                | Tok::Minus
                | Tok::Star
                | Tok::StarStar
                | Tok::Slash
                | Tok::SlashSlash
                | Tok::Percent
                | Tok::EqEq
                | Tok::NotEq
                | Tok::Lt
                | Tok::Le
                | Tok::Gt
                | Tok::Ge
        )
    }
}

pub fn keyword(s: &str) -> Option<Tok> {
    Some(match s {
        "let" => Tok::Let,
        "var" => Tok::Var,
        "fn" => Tok::Fn,
        "return" => Tok::Return,
        "if" => Tok::If,
        "else" => Tok::Else,
        "while" => Tok::While,
        "for" => Tok::For,
        "in" => Tok::In,
        "is" => Tok::Is,
        "loop" => Tok::Loop,
        "break" => Tok::Break,
        "continue" => Tok::Continue,
        "match" => Tok::Match,
        "type" => Tok::Type,
        "test" => Tok::Test,
        "property" => Tok::Property,
        "requires" => Tok::Requires,
        "ensures" => Tok::Ensures,
        "and" => Tok::And,
        "or" => Tok::Or,
        "not" => Tok::Not,
        "true" => Tok::True,
        "false" => Tok::False,
        "import" => Tok::Import,
        "as" => Tok::As,
        "assert" => Tok::Assert,
        "where" => Tok::Where,
        _ => return None,
    })
}

pub const KEYWORDS: &[&str] = &[
    "let", "var", "fn", "return", "if", "else", "while", "for", "in", "loop", "break", "continue", "match", "type", "test", "property", "requires",
    "ensures", "and", "or", "not", "true", "false", "import", "as", "assert", "where", "is",
];

impl Tok {
    pub fn describe(&self) -> String {
        match self {
            Tok::Int(n) => format!("number `{}`", n),
            Tok::Float(f) => format!("number `{}`", f),
            Tok::Str(_) => "a string".into(),
            Tok::Ident(s) | Tok::Upper(s) => format!("`{}`", s),
            Tok::Newline => "end of line".into(),
            Tok::Eof => "end of file".into(),
            other => format!("`{}`", other.text()),
        }
    }

    pub fn text(&self) -> &'static str {
        match self {
            Tok::Let => "let",
            Tok::Var => "var",
            Tok::Fn => "fn",
            Tok::Return => "return",
            Tok::If => "if",
            Tok::Else => "else",
            Tok::While => "while",
            Tok::For => "for",
            Tok::In => "in",
            Tok::Is => "is",
            Tok::Loop => "loop",
            Tok::Break => "break",
            Tok::Continue => "continue",
            Tok::Match => "match",
            Tok::Type => "type",
            Tok::Test => "test",
            Tok::Property => "property",
            Tok::Requires => "requires",
            Tok::Ensures => "ensures",
            Tok::And => "and",
            Tok::Or => "or",
            Tok::Not => "not",
            Tok::True => "true",
            Tok::False => "false",
            Tok::Import => "import",
            Tok::As => "as",
            Tok::Assert => "assert",
            Tok::Where => "where",
            Tok::LParen => "(",
            Tok::RParen => ")",
            Tok::LBracket => "[",
            Tok::RBracket => "]",
            Tok::LBrace => "{",
            Tok::RBrace => "}",
            Tok::Comma => ",",
            Tok::Colon => ":",
            Tok::Semi => ";",
            Tok::Dot => ".",
            Tok::DotDot => "..",
            Tok::DotDotEq => "..=",
            Tok::Question => "?",
            Tok::Arrow => "->",
            Tok::FatArrow => "=>",
            Tok::Assign => "=",
            Tok::PlusAssign => "+=",
            Tok::MinusAssign => "-=",
            Tok::StarAssign => "*=",
            Tok::SlashAssign => "/=",
            Tok::PercentAssign => "%=",
            Tok::SlashSlashAssign => "//=",
            Tok::StarStarAssign => "**=",
            Tok::EqEq => "==",
            Tok::NotEq => "!=",
            Tok::Lt => "<",
            Tok::Le => "<=",
            Tok::Gt => ">",
            Tok::Ge => ">=",
            Tok::Plus => "+",
            Tok::Minus => "-",
            Tok::Star => "*",
            Tok::StarStar => "**",
            Tok::Slash => "/",
            Tok::SlashSlash => "//",
            Tok::Percent => "%",
            Tok::PipeGt => "|>",
            Tok::Bar => "|",
            Tok::At => "@",
            Tok::Newline => "newline",
            Tok::Eof => "end of file",
            Tok::Int(_) | Tok::Float(_) | Tok::Str(_) | Tok::Ident(_) | Tok::Upper(_) => "?",
        }
    }
}

pub struct Lexer<'a> {
    src: &'a str,
    b: &'a [u8],
    pos: usize,
    end: usize,
    file: u32,
    toks: Vec<Token>,
    delims: Vec<u8>,
    nesting: u32,
    /// Errors inside one-line strings: lexing continues on the next line,
    /// so that one run reports all of them.
    soft: Vec<Diagnostic>,
    /// Errors for which a token was substituted (`&&` for `and`): the token
    /// stream is still sound, so the parser can look for more errors.
    substituted: Vec<Diagnostic>,
}

/// Lex `src[start..end]`. Spans are absolute offsets into `src`.
pub fn lex(src: &str, file: u32, start: usize, end: usize) -> Result<Vec<Token>, Diagnostic> {
    match lex_all(src, file, start, end) {
        Ok((toks, subs)) if subs.is_empty() => Ok(toks),
        Ok((_, mut ds)) | Err(mut ds) => Err(ds.swap_remove(0)),
    }
}

/// Like [`lex`], but reports every error it can. `Ok` holds the tokens with
/// the errors for which a token was substituted (`&&` read as `and`), so
/// that the parser can still look for errors; `Err` means the tokens are
/// not usable (a bad string skips the rest of its line).
pub fn lex_all(src: &str, file: u32, start: usize, end: usize) -> Result<(Vec<Token>, Vec<Diagnostic>), Vec<Diagnostic>> {
    // A UTF-8 byte-order mark at the start of a file is ignored.
    let start = if start == 0 && src.starts_with('\u{feff}') { 3 } else { start };
    let mut lx = Lexer {
        src,
        b: src.as_bytes(),
        pos: start,
        end,
        file,
        toks: Vec::new(),
        delims: Vec::new(),
        nesting: 0,
        soft: Vec::new(),
        substituted: Vec::new(),
    };
    let r = lx.run();
    let mut errors = std::mem::take(&mut lx.soft);
    if let Err(d) = r {
        errors.push(d);
    }
    if errors.is_empty() {
        return Ok((lx.toks, lx.substituted));
    }
    errors.append(&mut lx.substituted);
    errors.sort_by_key(|d| d.span.map_or(0, |s| s.start));
    Err(errors)
}

fn is_ident_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_'
}

fn is_ident_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

impl<'a> Lexer<'a> {
    fn peek(&self) -> u8 {
        if self.pos < self.end {
            self.b[self.pos]
        } else {
            0
        }
    }

    fn peek_at(&self, n: usize) -> u8 {
        if self.pos + n < self.end {
            self.b[self.pos + n]
        } else {
            0
        }
    }

    fn span(&self, start: usize) -> Span {
        Span::new(self.file, start, self.pos)
    }

    fn push(&mut self, tok: Tok, start: usize) {
        let span = self.span(start);
        self.toks.push(Token { tok, span });
    }

    fn err(&self, code: &'static str, msg: impl Into<String>, start: usize, end: usize) -> Diagnostic {
        Diagnostic::error(code, msg).at(Span::new(self.file, start, end.max(start + 1).min(self.src.len().max(start))))
    }

    /// Whether the last token ends an operand, so that a binary operator
    /// may follow.
    fn after_operand(&self) -> bool {
        matches!(
            self.toks.last().map(|t| &t.tok),
            Some(
                Tok::Int(_)
                    | Tok::Float(_)
                    | Tok::Str(_)
                    | Tok::Ident(_)
                    | Tok::Upper(_)
                    | Tok::True
                    | Tok::False
                    | Tok::RParen
                    | Tok::RBracket
                    | Tok::Question
            )
        )
    }

    /// `word` as the replacement for the operator at `start..end`, with
    /// spaces where the operator touches its operands (`a&&b`).
    fn spaced(&self, start: usize, end: usize, word: &str) -> String {
        let tight = |c: Option<&u8>| c.is_some_and(|c| !matches!(c, b' ' | b'\t' | b'\n' | b'\r'));
        let before = if tight(start.checked_sub(1).and_then(|i| self.b.get(i))) { " " } else { "" };
        let after = if tight(self.b[..self.end].get(end)) { " " } else { "" };
        format!("{}{}{}", before, word, after)
    }

    /// Whether the tokens since the start of the statement are one place
    /// (`i`, `p.count`, `xs[0]`): `x++` is then a whole statement, not part
    /// of an expression (`let y = i++`).
    fn operand_starts_statement(&self) -> bool {
        let mut k = self.toks.len();
        let mut depth = 0i32;
        while k > 0 {
            match &self.toks[k - 1].tok {
                Tok::RBracket => depth += 1,
                Tok::LBracket => depth -= 1,
                Tok::Ident(_) | Tok::Upper(_) | Tok::Dot | Tok::Int(_) => {}
                _ if depth > 0 => {}
                _ => break,
            }
            k -= 1;
        }
        k == 0 || matches!(self.toks[k - 1].tok, Tok::Newline | Tok::LBrace | Tok::Semi)
    }

    /// Whether the operand of a `!` that starts at `pos` is followed by
    /// the end of a condition (`&&`, `||`, `)`, `{`, the end of the line,
    /// ...) rather than by an operator that binds tighter than `not`.
    fn not_operand_ends_clause(&self, pos: usize) -> bool {
        let b = &self.b[..self.end];
        let mut i = pos;
        while i < b.len() && b[i] == b'!' {
            i += 1;
        }
        // The operand: a name or a parenthesized expression, then calls,
        // indexes and fields.
        let skip_group = |mut i: usize| -> Option<usize> {
            let (open, close) = (b[i], if b[i] == b'(' { b')' } else { b']' });
            let mut depth = 0;
            while i < b.len() {
                match b[i] {
                    c if c == open => depth += 1,
                    c if c == close => {
                        depth -= 1;
                        if depth == 0 {
                            return Some(i + 1);
                        }
                    }
                    b'"' | b'\n' => return None,
                    _ => {}
                }
                i += 1;
            }
            None
        };
        if i < b.len() && matches!(b[i], b'(' | b'[') {
            match skip_group(i) {
                Some(j) => i = j,
                None => return false,
            }
        } else {
            let s = i;
            while i < b.len() && (is_ident_char(b[i])) {
                i += 1;
            }
            if i == s {
                return false;
            }
        }
        loop {
            if i < b.len() && b[i] == b'!' && b.get(i + 1) != Some(&b'=') {
                i += 1;
            } else if i < b.len() && matches!(b[i], b'(' | b'[') {
                match skip_group(i) {
                    Some(j) => i = j,
                    None => return false,
                }
            } else if i + 1 < b.len() && b[i] == b'.' && is_ident_start(b[i + 1]) {
                i += 1;
                while i < b.len() && is_ident_char(b[i]) {
                    i += 1;
                }
            } else {
                break;
            }
        }
        while i < b.len() && matches!(b[i], b' ' | b'\t') {
            i += 1;
        }
        let rest = &b[i..];
        let word = |w: &[u8]| rest.starts_with(w) && rest.get(w.len()).is_none_or(|c| !is_ident_char(*c));
        rest.is_empty()
            || matches!(rest[0], b')' | b']' | b'}' | b',' | b'{' | b';' | b'\n' | b'\r' | b'#')
            || rest.starts_with(b"&&")
            || rest.starts_with(b"||")
            || word(b"and")
            || word(b"or")
    }

    /// Whether the statement ends at `pos` (only spaces or a comment
    /// follow on the line, or a `;` or `}`).
    fn statement_ends_at(&self, pos: usize) -> bool {
        let rest = &self.b[pos.min(self.end)..self.end];
        let n = rest.iter().take_while(|&&c| matches!(c, b' ' | b'\t' | b'\r')).count();
        matches!(rest.get(n), None | Some(b'\n' | b'#' | b';' | b'}'))
    }

    /// A string literal from another language starting at `start` (a single
    /// quote, possibly after an `f`/`r`/`u` prefix token): the span to
    /// replace and the Cogito string, when the translation is certain.
    fn single_quoted(&self, start: usize) -> Option<(usize, String, usize)> {
        if self.src[start..self.end].starts_with("\'\'\'") {
            // (Python's triple quotes keep indentation and the first line
            // break, which Cogito's `"""` strings do not.)
            return None;
        }
        let prefix = match self.toks.last() {
            Some(Token { tok: Tok::Ident(p), span }) if span.end as usize == start => Some((p.clone(), span.start as usize)),
            _ => None,
        };
        let raw = prefix.as_ref().is_some_and(|(p, _)| &**p == "r");
        let close = closing_quote(&self.src[start + 1..self.end], '\'', raw)?;
        let body = &self.src[start + 1..start + 1 + close];
        let end = start + 1 + close + 1;
        let (from, text) = match prefix.as_ref().map(|(p, at)| (&**p, *at)) {
            None => (start, format!("\"{}\"", convert_body(body, false)?)),
            Some(("u", at)) => (at, format!("\"{}\"", convert_body(body, false)?)),
            Some(("f", at)) => (at, format!("\"{}\"", convert_body(body, true)?)),
            Some(("r", at)) if !body.contains('"') => (at, format!("r\"{}\"", body)),
            Some(("r", at)) if !body.contains("\"#") => (at, format!("r#\"{}\"#", body)),
            Some((p, _)) if p.len() > 2 || !p.chars().all(|c| matches!(c, 'f' | 'r' | 'b' | 'u')) => {
                (start, format!("\"{}\"", convert_body(body, false)?))
            }
            _ => return None,
        };
        Some((from, text, end))
    }

    /// A JavaScript template literal `` `a ${b}` `` closed on the same line,
    /// as a Cogito string: the replacement and the end of the literal.
    fn template_literal(&self, start: usize) -> Option<(String, usize)> {
        let rest = &self.src[start + 1..self.end];
        let mut out = String::from("\"");
        let mut chars = rest.char_indices().peekable();
        let mut depth = 0;
        while let Some((i, c)) = chars.next() {
            match c {
                '\n' => return None,
                '`' if depth == 0 => {
                    out.push('"');
                    return Some((out, start + 1 + i + 1));
                }
                // A template inside an interpolation.
                '`' => return None,
                '$' if depth == 0 && chars.peek().map(|x| x.1) == Some('{') => {
                    chars.next();
                    depth = 1;
                    out.push('{');
                }
                '{' if depth > 0 => {
                    depth += 1;
                    out.push('{');
                }
                '}' if depth > 0 => {
                    depth -= 1;
                    out.push('}');
                }
                _ if depth > 0 => out.push(c),
                '{' | '}' => {
                    out.push('\\');
                    out.push(c);
                }
                '"' => out.push_str("\\\""),
                '\\' => {
                    let (_, e) = chars.next()?;
                    match e {
                        '$' => out.push('$'),
                        '`' => out.push('`'),
                        _ => push_escape(&mut out, e, &mut chars.by_ref().map(|x| x.1))?,
                    }
                }
                c => out.push(c),
            }
        }
        None
    }

    fn newline_significant(&self) -> bool {
        matches!(self.delims.last(), None | Some(b'{'))
    }

    /// Skip spaces, tabs, carriage returns and comments (but not newlines).
    fn skip_inline_ws(&mut self) {
        loop {
            match self.peek() {
                b' ' | b'\t' | b'\r' => self.pos += 1,
                b'#' => {
                    while self.pos < self.end && self.b[self.pos] != b'\n' {
                        self.pos += 1;
                    }
                }
                // A comment in the style of C, Java, JavaScript, Rust, Go...
                b'/' if self.slash_comment() => {
                    let start = self.pos;
                    // (`///` too.)
                    let slashes = self.b[start..self.end].iter().take_while(|&&c| c == b'/').count();
                    let d = self
                        .err("E0010", "comments start with `#`, not `//`", start, start + 2)
                        .help("`//` is floor division in Cogito: write the comment as `# ...`")
                        .fix(Span::new(self.file, start, start + slashes), "#");
                    self.substituted.push(d);
                    while self.pos < self.end && self.b[self.pos] != b'\n' {
                        self.pos += 1;
                    }
                }
                _ => break,
            }
        }
    }

    /// Whether the `//` here can only be meant as a comment: it starts a
    /// line (where a newline ends the statement), or prose follows it (two
    /// words in a row, which floor division cannot be followed by).
    fn slash_comment(&self) -> bool {
        let rest = &self.b[self.pos..self.end];
        if !rest.starts_with(b"//") || rest.starts_with(b"//=") {
            return false;
        }
        let before = &self.b[..self.pos];
        let line_start = before.iter().rev().take_while(|&&c| c != b'\n').all(|&c| c == b' ' || c == b'\t');
        if line_start && self.newline_significant() {
            return true;
        }
        let line = rest[2..].split(|&c| c == b'\n').next().unwrap_or(&[]);
        let line = std::str::from_utf8(line).unwrap_or("");
        let mut words = line.split([' ', '\t']).filter(|w| !w.is_empty());
        let word = |w: Option<&str>| w.filter(|w| w.chars().all(|c| c.is_ascii_alphabetic()) && !KEYWORDS.contains(w)).is_some();
        line.starts_with([' ', '\t']) && word(words.next()) && word(words.next())
    }

    fn continuation_ahead(&self) -> bool {
        let rest = &self.b[self.pos..self.end];
        if rest.starts_with(b"|>") {
            return true;
        }
        if rest.starts_with(b".") && !rest.starts_with(b"..") {
            return true;
        }
        if rest.starts_with(b"&&") || rest.starts_with(b"||") {
            return true;
        }
        for kw in [&b"and"[..], &b"or"[..]] {
            if rest.starts_with(kw) && rest.get(kw.len()).is_none_or(|c| !is_ident_char(*c)) {
                return true;
            }
        }
        false
    }

    fn run(&mut self) -> Result<(), Diagnostic> {
        loop {
            self.skip_inline_ws();
            if self.pos >= self.end {
                let p = self.pos;
                if !matches!(self.toks.last(), None | Some(Token { tok: Tok::Newline, .. })) {
                    self.toks.push(Token { tok: Tok::Newline, span: Span::new(self.file, p, p) });
                }
                self.toks.push(Token { tok: Tok::Eof, span: Span::new(self.file, p, p) });
                return Ok(());
            }
            let start = self.pos;
            let c = self.peek();
            if c == b'\n' {
                self.pos += 1;
                if self.newline_significant() {
                    // Collapse blank lines and comments, then decide whether the
                    // statement continues on the next line.
                    loop {
                        self.skip_inline_ws();
                        if self.peek() == b'\n' {
                            self.pos += 1;
                        } else {
                            break;
                        }
                    }
                    if !self.continuation_ahead() && !matches!(self.toks.last(), None | Some(Token { tok: Tok::Newline, .. })) {
                        self.toks.push(Token { tok: Tok::Newline, span: Span::new(self.file, start, start + 1) });
                    }
                }
                continue;
            }
            if c.is_ascii_digit() {
                self.number()?;
                continue;
            }
            if c == b'r' && self.peek_at(1) == b'"' {
                self.raw_string()?;
                continue;
            }
            if c == b'r' && self.peek_at(1) == b'#' {
                let hashes = self.b[self.pos + 1..self.end].iter().take_while(|&&h| h == b'#').count();
                if self.peek_at(1 + hashes) == b'"' {
                    self.hashed_raw_string(hashes)?;
                    continue;
                }
            }
            // `f"..."` from Python: every Cogito string interpolates.
            if c == b'f' && self.peek_at(1) == b'"' && !self.b[self.pos..self.end].starts_with(b"f\"\"\"") {
                let line = &self.src[start..self.end];
                let line = &line[..line.find('\n').unwrap_or(line.len())];
                let mut d = self
                    .err("E0001", "strings need no `f` prefix", start, start + 1)
                    .help("every string interpolates: \"total: {n}\" (write `\\{` for a literal brace)");
                if !line.contains("{{") && !line.contains("}}") {
                    d = d.fix(Span::new(self.file, start, start + 1), "");
                }
                self.substituted.push(d);
                self.pos += 1;
                continue;
            }
            if is_ident_start(c) {
                self.ident();
                continue;
            }
            if c == b'"' {
                let one_line = !self.b[self.pos..self.end].starts_with(b"\"\"\"");
                if let Err(d) = self.string() {
                    if !one_line || self.soft.len() >= 20 {
                        return Err(d);
                    }
                    // Skip the rest of the line (a one-line string ends on it)
                    // and look for more errors.
                    self.soft.push(d);
                    self.nesting = 0;
                    while self.pos < self.end && self.b[self.pos] != b'\n' {
                        self.pos += 1;
                    }
                }
                continue;
            }
            self.punct()?;
        }
    }

    fn number(&mut self) -> Result<(), Diagnostic> {
        let start = self.pos;
        let after_dot = matches!(self.toks.last(), Some(Token { tok: Tok::Dot, .. }));
        if self.peek() == b'0' && matches!(self.peek_at(1), b'x' | b'b' | b'o') {
            let radix = match self.peek_at(1) {
                b'x' => 16,
                b'b' => 2,
                _ => 8,
            };
            self.pos += 2;
            let ds = self.pos;
            while self.peek().is_ascii_alphanumeric() || self.peek() == b'_' {
                self.pos += 1;
            }
            let digits: String = self.src[ds..self.pos].chars().filter(|c| *c != '_').collect();
            return match i64::from_str_radix(&digits, radix) {
                Ok(n) => {
                    self.push(Tok::Int(n), start);
                    Ok(())
                }
                Err(e) if matches!(e.kind(), std::num::IntErrorKind::PosOverflow) => Err(self
                    .err("E0003", format!("integer literal `{}` is too large", &self.src[start..self.pos]), start, self.pos)
                    .help("Int is a signed 64-bit integer (max 0x7fffffffffffffff)")),
                Err(_) => Err(self.err("E0003", format!("invalid base-{} literal `{}`", radix, &self.src[start..self.pos]), start, self.pos)),
            };
        }
        while self.peek().is_ascii_digit() || self.peek() == b'_' {
            self.pos += 1;
        }
        let mut is_float = false;
        if !after_dot && self.peek() == b'.' && self.peek_at(1).is_ascii_digit() {
            is_float = true;
            self.pos += 1;
            while self.peek().is_ascii_digit() || self.peek() == b'_' {
                self.pos += 1;
            }
        }
        if !after_dot
            && matches!(self.peek(), b'e' | b'E')
            && (self.peek_at(1).is_ascii_digit() || (matches!(self.peek_at(1), b'+' | b'-') && self.peek_at(2).is_ascii_digit()))
        {
            is_float = true;
            self.pos += 2;
            while self.peek().is_ascii_digit() {
                self.pos += 1;
            }
        }
        if is_ident_start(self.peek()) {
            let s = self.pos;
            while is_ident_char(self.peek()) {
                self.pos += 1;
            }
            return Err(self
                .err("E0003", format!("invalid number literal `{}`", &self.src[start..self.pos]), start, self.pos)
                .label(format!("unexpected `{}` after the digits", &self.src[s..self.pos])));
        }
        let text: String = self.src[start..self.pos].chars().filter(|c| *c != '_').collect();
        if is_float {
            match text.parse::<f64>() {
                Ok(f) => self.push(Tok::Float(f), start),
                Err(_) => return Err(self.err("E0003", format!("invalid float literal `{}`", text), start, self.pos)),
            }
        } else {
            match text.parse::<i64>() {
                Ok(n) => self.push(Tok::Int(n), start),
                Err(_) => {
                    return Err(self
                        .err("E0003", format!("integer literal `{}` is too large", text), start, self.pos)
                        .help("Int is a signed 64-bit integer (max 9223372036854775807); use a Float literal like `1.0e20` for larger magnitudes"))
                }
            }
        }
        Ok(())
    }

    fn ident(&mut self) {
        let start = self.pos;
        while is_ident_char(self.peek()) {
            self.pos += 1;
        }
        let first = self.b[start];
        // Mutating function names end with `!` (but `a!=b` is `a != b`).
        if !first.is_ascii_uppercase() && self.peek() == b'!' && self.peek_at(1) != b'=' {
            self.pos += 1;
        }
        let text = &self.src[start..self.pos];
        if let Some(kw) = keyword(text) {
            self.push(kw, start);
        } else if first.is_ascii_uppercase() {
            self.push(Tok::Upper(Rc::from(text)), start);
        } else {
            self.push(Tok::Ident(Rc::from(text)), start);
        }
    }

    fn punct(&mut self) -> Result<(), Diagnostic> {
        let start = self.pos;
        let c = self.peek();
        let c1 = self.peek_at(1);
        let c2 = self.peek_at(2);
        let (tok, len) = match c {
            b'(' => (Tok::LParen, 1),
            b')' => (Tok::RParen, 1),
            b'[' => (Tok::LBracket, 1),
            b']' => (Tok::RBracket, 1),
            b'{' => (Tok::LBrace, 1),
            b'}' => (Tok::RBrace, 1),
            b',' => (Tok::Comma, 1),
            b':' => (Tok::Colon, 1),
            b';' => (Tok::Semi, 1),
            b'?' => (Tok::Question, 1),
            b'@' => (Tok::At, 1),
            b'.' => {
                if c1 == b'.' && c2 == b'=' {
                    (Tok::DotDotEq, 3)
                } else if c1 == b'.' && c2 == b'.' {
                    let mut d = self.err("E0010", "`...` is not an operator", start, start + 3).help("use `..` for ranges and rest patterns");
                    // A spread, as in `[...xs]` or `f(...args)`.
                    if matches!(self.toks.last().map(|t| &t.tok), Some(Tok::LBracket | Tok::LParen | Tok::LBrace | Tok::Comma)) {
                        d = d.fix(Span::new(self.file, start, start + 3), "..");
                    }
                    return Err(d);
                } else if c1 == b'.' {
                    (Tok::DotDot, 2)
                } else {
                    (Tok::Dot, 1)
                }
            }
            b'-' => match c1 {
                b'>' => (Tok::Arrow, 2),
                b'=' => (Tok::MinusAssign, 2),
                b'-' if self.after_operand() && self.statement_ends_at(start + 2) && self.operand_starts_statement() => {
                    return Err(self
                        .err("E0010", "`--` is not an operator", start, start + 2)
                        .help("use `x -= 1` to decrement")
                        .fix(Span::new(self.file, start, start + 2), self.spaced(start, start, "-= 1").trim_end().to_string()))
                }
                _ => (Tok::Minus, 1),
            },
            b'=' => match c1 {
                b'>' => (Tok::FatArrow, 2),
                b'=' if c2 == b'=' => {
                    return Err(self
                        .err("E0010", "`===` is not an operator", start, start + 3)
                        .help("use `==`, which compares values (of any type)")
                        .fix(Span::new(self.file, start, start + 3), "=="))
                }
                b'=' => (Tok::EqEq, 2),
                _ => (Tok::Assign, 1),
            },
            b'+' => match c1 {
                b'=' => (Tok::PlusAssign, 2),
                b'+' => {
                    let mut d =
                        self.err("E0010", "`++` is not an operator", start, start + 2).help("use `x += 1` to increment, or `+` to concatenate");
                    if self.after_operand() && self.statement_ends_at(start + 2) && self.operand_starts_statement() {
                        d = d.fix(Span::new(self.file, start, start + 2), self.spaced(start, start, "+= 1").trim_end().to_string());
                    }
                    return Err(d);
                }
                _ => (Tok::Plus, 1),
            },
            b'*' => match c1 {
                b'*' if c2 == b'=' => (Tok::StarStarAssign, 3),
                b'*' => (Tok::StarStar, 2),
                b'=' => (Tok::StarAssign, 2),
                _ => (Tok::Star, 1),
            },
            b'/' => match c1 {
                b'/' if c2 == b'=' => (Tok::SlashSlashAssign, 3),
                b'/' => (Tok::SlashSlash, 2),
                b'=' => (Tok::SlashAssign, 2),
                _ => (Tok::Slash, 1),
            },
            b'%' => match c1 {
                b'=' => (Tok::PercentAssign, 2),
                _ => (Tok::Percent, 1),
            },
            b'!' => match c1 {
                b'=' if c2 == b'=' => {
                    return Err(self
                        .err("E0010", "`!==` is not an operator", start, start + 3)
                        .help("use `!=`, which compares values (of any type)")
                        .fix(Span::new(self.file, start, start + 3), "!="))
                }
                b'=' => (Tok::NotEq, 2),
                _ => {
                    let d = self
                        .err("E0001", "unexpected character `!`", start, start + 1)
                        .help("use the keyword `not` for boolean negation, and `!=` for inequality");
                    // `!x` in front of an operand reads as `not x` (a certain
                    // fix only when nothing that binds tighter than `not`,
                    // such as `<` in `!p < q`, follows the operand).
                    if !self.after_operand() && (is_ident_start(c1) || matches!(c1, b'(' | b'[' | b'!')) {
                        let d = if self.not_operand_ends_clause(start + 1) { d.fix(Span::new(self.file, start, start + 1), "not ") } else { d };
                        self.substituted.push(d);
                        (Tok::Not, 1)
                    } else {
                        return Err(d);
                    }
                }
            },
            b'<' => match c1 {
                b'=' => (Tok::Le, 2),
                _ => (Tok::Lt, 1),
            },
            b'>' => match c1 {
                b'=' => (Tok::Ge, 2),
                _ => (Tok::Gt, 1),
            },
            b'|' => match c1 {
                b'>' => (Tok::PipeGt, 2),
                b'|' => {
                    let d = self.err("E0001", "unexpected `||`", start, start + 2);
                    if !self.after_operand() {
                        return Err(d.help("anonymous functions are written `fn() => value` (with parameters: `fn(x) => x + 1`)"));
                    }
                    let fix = self.spaced(start, start + 2, "or");
                    self.substituted.push(d.help("use the keyword `or` for boolean disjunction").fix(Span::new(self.file, start, start + 2), fix));
                    (Tok::Or, 2)
                }
                _ => (Tok::Bar, 1),
            },
            b'&' => {
                let len = if c1 == b'&' { 2 } else { 1 };
                let mut d = self
                    .err("E0001", format!("unexpected `{}`", &self.src[start..start + len]), start, start + len)
                    .help("use the keyword `and` for boolean conjunction");
                if !self.after_operand() {
                    return Err(d.help("Cogito has no references: pass the value itself (values are never changed through another name)"));
                }
                // (A single `&` may be a bitwise and, which is `bit_and(a, b)`.)
                if len == 2 {
                    d = d.fix(Span::new(self.file, start, start + 2), self.spaced(start, start + 2, "and"));
                } else {
                    d = d.help("use the keyword `and` for boolean conjunction, or `bit_and(a, b)` for the bitwise and of two Ints");
                }
                self.substituted.push(d);
                (Tok::And, len)
            }
            b'`' => {
                let mut d = self
                    .err("E0001", "unexpected character `` ` ``", start, start + 1)
                    .help("strings use double quotes, and interpolate with braces: \"total: {n}\"");
                if let Some((text, end)) = self.template_literal(start) {
                    d = d.fix(Span::new(self.file, start, end), text);
                }
                return Err(d);
            }
            b'\'' => {
                let mut d = self
                    .err("E0001", "unexpected character `'`", start, start + 1)
                    .help("strings use double quotes: \"text\" (there is no separate character type)");
                if let Some((from, text, end)) = self.single_quoted(start) {
                    d = d.fix(Span::new(self.file, from, end), text);
                }
                return Err(d);
            }
            _ => {
                let ch = self.src[start..].chars().next().unwrap_or('?');
                let shown = if ch.is_control() || matches!(ch, '\u{200b}'..='\u{200f}' | '\u{feff}' | '\u{2028}' | '\u{2029}') {
                    format!("{} (U+{:04X})", ch.escape_unicode(), ch as u32)
                } else {
                    format!("`{}`", ch)
                };
                let mut d = self.err("E0001", format!("unexpected character {}", shown), start, start + ch.len_utf8());
                if matches!(ch, '“' | '”' | '‘' | '’') {
                    d = d.help("this is a typographic quote; use a plain `\"`");
                    // The whole quoted text, if it is closed on the same line.
                    let from = start + ch.len_utf8();
                    let line = &self.src[from..self.end];
                    let line = &line[..line.find('\n').unwrap_or(line.len())];
                    let pair: [char; 2] = if matches!(ch, '“' | '”') { ['“', '”'] } else { ['‘', '’'] };
                    if let Some(i) = line.find(pair) {
                        let q = line[i..].chars().next().unwrap_or('"');
                        if let Some(body) = convert_body(&line[..i], false) {
                            d = d.fix(Span::new(self.file, start, from + i + q.len_utf8()), format!("\"{}\"", body));
                        }
                    }
                } else if ch == ';' {
                } else if !ch.is_ascii() {
                    d = d.help("identifiers must be ASCII; non-ASCII text is allowed inside strings and comments");
                }
                return Err(d);
            }
        };
        self.pos += len;
        match tok {
            Tok::LParen => self.delims.push(b'('),
            Tok::LBracket => self.delims.push(b'['),
            Tok::LBrace => self.delims.push(b'{'),
            Tok::RParen | Tok::RBracket | Tok::RBrace => {
                self.delims.pop();
            }
            _ => {}
        }
        self.push(tok, start);
        Ok(())
    }

    fn string(&mut self) -> Result<(), Diagnostic> {
        let start = self.pos;
        if self.b[self.pos..self.end].starts_with(b"\"\"\"") {
            return self.triple_string();
        }
        self.pos += 1;
        let mut parts = Vec::new();
        let mut lit = String::new();
        loop {
            if self.pos >= self.end || self.peek() == b'\n' {
                return Err(self
                    .err("E0002", "unterminated string", start, self.pos)
                    .label("string starts here")
                    .help("close the string with `\"`, or use triple quotes \"\"\" for multi-line strings"));
            }
            let c = self.peek();
            match c {
                b'"' => {
                    self.pos += 1;
                    break;
                }
                b'\\' => self.escape(&mut lit)?,
                b'{' => {
                    if !lit.is_empty() {
                        parts.push(StrPart::Lit(std::mem::take(&mut lit)));
                    }
                    parts.push(self.interpolation()?);
                }
                // A lone `}` is just a character (only `{` starts an interpolation).
                _ => {
                    let ch = self.src[self.pos..].chars().next().unwrap();
                    lit.push(ch);
                    self.pos += ch.len_utf8();
                }
            }
        }
        if !lit.is_empty() || parts.is_empty() {
            parts.push(StrPart::Lit(lit));
        }
        self.push(Tok::Str(parts), start);
        Ok(())
    }

    fn escape(&mut self, lit: &mut String) -> Result<(), Diagnostic> {
        let s = self.pos;
        self.pos += 1;
        let c = self.peek();
        self.pos += 1;
        match c {
            b'n' => lit.push('\n'),
            b't' => lit.push('\t'),
            b'r' => lit.push('\r'),
            b'0' => lit.push('\0'),
            b'\\' => lit.push('\\'),
            b'"' => lit.push('"'),
            b'{' => lit.push('{'),
            b'}' => lit.push('}'),
            b'u' => {
                if self.peek() != b'{' {
                    return Err(self.err("E0004", "invalid unicode escape", s, self.pos).help("write unicode escapes as \\u{1F600}"));
                }
                self.pos += 1;
                let hs = self.pos;
                while self.peek().is_ascii_hexdigit() {
                    self.pos += 1;
                }
                let hex = &self.src[hs..self.pos];
                if self.peek() != b'}' {
                    return Err(self.err("E0004", "invalid unicode escape", s, self.pos).help("write unicode escapes as \\u{1F600}"));
                }
                self.pos += 1;
                match u32::from_str_radix(hex, 16).ok().and_then(char::from_u32) {
                    Some(ch) => lit.push(ch),
                    None => return Err(self.err("E0004", format!("`{}` is not a valid unicode scalar value", hex), s, self.pos)),
                }
            }
            0 => return Err(self.err("E0002", "unterminated string", s, s + 1)),
            _ => {
                let ch = self.src[self.pos - 1..].chars().next().unwrap_or('?');
                return Err(self
                    .err("E0004", format!("invalid escape sequence `\\{}`", ch), s, self.pos)
                    .help("valid escapes: \\n \\t \\r \\0 \\\\ \\\" \\{ \\} \\u{...}"));
            }
        }
        Ok(())
    }

    /// Called with `pos` at `{`. Scans to the matching `}`.
    fn interpolation(&mut self) -> Result<StrPart, Diagnostic> {
        self.nesting += 1;
        let r = self.interpolation_inner();
        self.nesting -= 1;
        r
    }

    fn interpolation_inner(&mut self) -> Result<StrPart, Diagnostic> {
        let open = self.pos;
        if self.nesting > 64 {
            return Err(self.err("E0005", "string interpolations are nested too deeply", open, open + 1));
        }
        self.pos += 1;
        if self.peek() == b'{' {
            return Err(self
                .err("E0005", "`{{` is not an escape sequence in Cogito strings", open, open + 2)
                .help("write `\\{` for a literal `{` and `\\}` for `}`, or use a raw string r\"...\" (no interpolation)"));
        }
        let expr_start = self.pos;
        let mut depth: i32 = 0;
        let mut spec_start: Option<usize> = None;
        loop {
            if self.pos >= self.end || self.peek() == b'\n' {
                return Err(self
                    .err("E0005", "unterminated interpolation in string", open, open + 1)
                    .label("this `{` is never closed")
                    .help("close the interpolation with `}`, or write `\\{` for a literal brace"));
            }
            let c = self.peek();
            match c {
                b'"' => {
                    if self.skip_nested_string().is_err() {
                        return Err(self
                            .err("E0005", "unclosed `{` in string", open, open + 1)
                            .label("this `{` starts an interpolation that is never closed")
                            .help("write `\\{` for a literal brace, or use a raw string r\"...\""));
                    }
                    continue;
                }
                // A raw string (`r"..."`, `r#"..."#`): its braces and quotes are text.
                b'r' if !is_ident_char(self.b[self.pos - 1]) && self.raw_end(self.pos).is_some() => {
                    self.pos = self.raw_end(self.pos).unwrap_or(self.pos + 1);
                    continue;
                }
                b'(' | b'[' | b'{' => depth += 1,
                b')' | b']' => depth -= 1,
                b'}' => {
                    if depth == 0 {
                        break;
                    }
                    depth -= 1;
                }
                b':' if depth == 0 && spec_start.is_none() => {
                    spec_start = Some(self.pos);
                }
                _ => {}
            }
            self.pos += 1;
        }
        let close = self.pos;
        self.pos += 1;
        let expr_end = spec_start.unwrap_or(close);
        if self.src[expr_start..expr_end].trim().is_empty() {
            return Err(self
                .err("E0005", "empty interpolation `{}` in string", open, close + 1)
                .help("put an expression inside the braces, or write `\\{` for a literal brace"));
        }
        let spec = spec_start.map(|s| self.src[s + 1..close].to_string());
        Ok(StrPart::Expr { start: expr_start as u32, end: expr_end as u32, spec })
    }

    /// The end of a one-line raw string starting at `at` (`r"..."` or
    /// `r#"..."#`), if there is one.
    fn raw_end(&self, at: usize) -> Option<usize> {
        let hashes = self.b[at + 1..self.end].iter().take_while(|&&h| h == b'#').count();
        let open = at + 1 + hashes;
        if self.b.get(open) != Some(&b'"') || open >= self.end {
            return None;
        }
        let close: Vec<u8> = std::iter::once(b'"').chain(std::iter::repeat_n(b'#', hashes)).collect();
        let body = &self.b[open + 1..self.end];
        let line = &body[..body.iter().position(|&c| c == b'\n').unwrap_or(body.len())];
        line.windows(close.len()).position(|w| w == close.as_slice()).map(|i| open + 1 + i + close.len())
    }

    /// Skip over a string literal nested inside an interpolation.
    fn skip_nested_string(&mut self) -> Result<(), Diagnostic> {
        let start = self.pos;
        self.pos += 1;
        loop {
            if self.pos >= self.end || self.peek() == b'\n' {
                return Err(self.err("E0002", "unterminated string", start, start + 1));
            }
            match self.peek() {
                b'"' => {
                    self.pos += 1;
                    return Ok(());
                }
                b'\\' => self.pos += 2,
                b'{' => {
                    self.interpolation()?;
                }
                _ => self.pos += 1,
            }
        }
    }

    /// Raw strings: `r"..."` or `r"""..."""`. No escapes, no interpolation;
    /// ideal for JSON, regular expressions and Windows paths. Triple-quoted
    /// raw strings are dedented like ordinary triple-quoted strings.
    fn raw_string(&mut self) -> Result<(), Diagnostic> {
        let start = self.pos;
        self.pos += 1;
        if self.b[self.pos..self.end].starts_with(b"\"\"\"") {
            let body_start = self.pos + 3;
            let Some(rel) = self.src[body_start..self.end].find("\"\"\"") else {
                return Err(self.err("E0002", "unterminated raw string", start, start + 4).label("raw string starts here"));
            };
            let raw = &self.src[body_start..body_start + rel];
            self.pos = body_start + rel + 3;
            let text = dedent(raw);
            self.push(Tok::Str(vec![StrPart::Lit(text)]), start);
            return Ok(());
        }
        let body_start = self.pos + 1;
        let mut p = body_start;
        while p < self.end && self.b[p] != b'"' {
            if self.b[p] == b'\n' {
                return Err(self
                    .err("E0002", "unterminated raw string", start, p)
                    .help("raw strings cannot span lines unless they use triple quotes: r\"\"\"...\"\"\""));
            }
            p += 1;
        }
        if p >= self.end {
            return Err(self.err("E0002", "unterminated raw string", start, p));
        }
        let text = self.src[body_start..p].to_string();
        self.pos = p + 1;
        self.push(Tok::Str(vec![StrPart::Lit(text)]), start);
        Ok(())
    }

    /// `r#"..."#` (any number of `#`): taken exactly as written, quotes and
    /// newlines included, up to a `"` followed by as many `#`.
    fn hashed_raw_string(&mut self, hashes: usize) -> Result<(), Diagnostic> {
        let start = self.pos;
        let body_start = self.pos + 2 + hashes;
        let mut close = String::from("\"");
        close.push_str(&"#".repeat(hashes));
        let Some(rel) = self.src[body_start..self.end].find(&close) else {
            return Err(self
                .err("E0002", "unterminated raw string", start, body_start)
                .label("raw string starts here")
                .help(format!("close it with `{}`", close)));
        };
        let text = self.src[body_start..body_start + rel].to_string();
        self.pos = body_start + rel + close.len();
        self.push(Tok::Str(vec![StrPart::Lit(text)]), start);
        Ok(())
    }

    /// Triple-quoted strings may span lines. A newline directly after the
    /// opening quotes is dropped, as is the final line if it holds only
    /// whitespace before the closing quotes. The common indentation of all
    /// non-blank lines is removed.
    fn triple_string(&mut self) -> Result<(), Diagnostic> {
        let start = self.pos;
        self.pos += 3;
        // Find the closing quotes first (to compute indentation).
        let body_start = self.pos;
        let mut p = self.pos;
        let close;
        loop {
            if p + 3 > self.end {
                return Err(self.err("E0002", "unterminated triple-quoted string", start, start + 3).label("string starts here"));
            }
            if self.b[p] == b'\\' {
                p += 2;
                continue;
            }
            if &self.b[p..p + 3] == b"\"\"\"" {
                close = p;
                break;
            }
            p += 1;
        }
        let raw = &self.src[body_start..close];
        let single_line = !raw.contains('\n');
        let mut lines: Vec<&str> = raw.split('\n').collect();
        let skip_first = lines.len() > 1 && lines[0].trim().is_empty();
        let drop_last = lines.len() > 1 && lines.last().is_some_and(|l| l.trim().is_empty());
        if skip_first {
            lines.remove(0);
        }
        if drop_last {
            lines.pop();
        }
        let indent = if single_line {
            0
        } else {
            lines.iter().filter(|l| !l.trim().is_empty()).map(|l| l.len() - l.trim_start_matches([' ', '\t']).len()).min().unwrap_or(0)
        };
        // Now scan for real.
        self.pos = body_start;
        if skip_first {
            while self.b[self.pos] != b'\n' {
                self.pos += 1;
            }
            self.pos += 1;
        }
        let content_end = if drop_last { raw.rfind('\n').map(|i| body_start + i).unwrap_or(close) } else { close };
        let mut parts = Vec::new();
        let mut lit = String::new();
        let mut at_line_start = true;
        while self.pos < content_end {
            if at_line_start {
                let mut n = 0;
                while n < indent && self.pos < content_end && matches!(self.b[self.pos], b' ' | b'\t') {
                    self.pos += 1;
                    n += 1;
                }
                at_line_start = false;
                continue;
            }
            let c = self.b[self.pos];
            match c {
                b'\\' => self.escape(&mut lit)?,
                b'{' => {
                    if !lit.is_empty() {
                        parts.push(StrPart::Lit(std::mem::take(&mut lit)));
                    }
                    parts.push(self.interpolation()?);
                }
                b'\n' => {
                    lit.push('\n');
                    self.pos += 1;
                    at_line_start = true;
                }
                b'\r' => self.pos += 1,
                _ => {
                    let ch = self.src[self.pos..].chars().next().unwrap();
                    lit.push(ch);
                    self.pos += ch.len_utf8();
                }
            }
        }
        self.pos = close + 3;
        if !lit.is_empty() || parts.is_empty() {
            parts.push(StrPart::Lit(lit));
        }
        self.push(Tok::Str(parts), start);
        Ok(())
    }
}

/// Remove a leading blank line, a trailing whitespace-only line, and the
/// common indentation of the remaining non-blank lines.
pub fn dedent(raw: &str) -> String {
    let raw = raw.replace("\r\n", "\n");
    let mut lines: Vec<&str> = raw.split('\n').collect();
    if lines.len() > 1 && lines[0].trim().is_empty() {
        lines.remove(0);
    }
    if lines.len() > 1 && lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    if !raw.contains('\n') {
        return raw.to_string();
    }
    let indent = lines.iter().filter(|l| !l.trim().is_empty()).map(|l| l.len() - l.trim_start_matches([' ', '\t']).len()).min().unwrap_or(0);
    lines.iter().map(|l| if l.len() >= indent { &l[indent..] } else { l.trim_start() }).collect::<Vec<_>>().join("\n")
}

/// Where a string body ends: the index of the unescaped `quote` in `s`, on
/// the same line.
fn closing_quote(s: &str, quote: char, raw: bool) -> Option<usize> {
    let mut chars = s.char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            '\n' => return None,
            '\\' => {
                chars.next();
            }
            c if c == quote => return Some(i),
            _ => {}
        }
    }
    let _ = raw;
    None
}

/// The body of a string from another language (a single-quoted or
/// typographically quoted string; `interp` for a Python f-string) as the
/// body of a Cogito string; `None` when an escape has no certain
/// equivalent.
fn convert_body(body: &str, interp: bool) -> Option<String> {
    let mut out = String::new();
    let mut chars = body.chars().peekable();
    let mut depth = 0;
    while let Some(c) = chars.next() {
        match c {
            // Inside `{...}` of an f-string, the expression is kept as it is.
            '{' if interp && depth == 0 && chars.peek() == Some(&'{') => {
                chars.next();
                out.push_str("\\{");
            }
            '}' if interp && depth == 0 && chars.peek() == Some(&'}') => {
                chars.next();
                out.push_str("\\}");
            }
            '{' if interp => {
                depth += 1;
                out.push('{');
            }
            '}' if interp && depth > 0 => {
                depth -= 1;
                out.push('}');
            }
            _ if depth > 0 => out.push(c),
            '{' | '}' => {
                out.push('\\');
                out.push(c);
            }
            '"' => out.push_str("\\\""),
            '\\' => {
                let e = chars.next()?;
                match e {
                    '\'' => out.push('\''),
                    '"' => out.push_str("\\\""),
                    _ => push_escape(&mut out, e, &mut chars)?,
                }
            }
            c => out.push(c),
        }
    }
    (depth == 0).then_some(out)
}

/// The Cogito form of the escape `\e` (with what follows in `rest`):
/// the escapes Cogito shares, and `\uXXXX`/`\xHH` as `\u{...}`.
fn push_escape(out: &mut String, e: char, rest: &mut dyn Iterator<Item = char>) -> Option<()> {
    match e {
        'n' | 't' | 'r' | '0' | '\\' => {
            out.push('\\');
            out.push(e);
        }
        'u' => {
            let first = rest.next()?;
            out.push_str("\\u{");
            if first == '{' {
                loop {
                    let c = rest.next()?;
                    if c == '}' {
                        break;
                    }
                    c.is_ascii_hexdigit().then_some(())?;
                    out.push(c);
                }
            } else {
                first.is_ascii_hexdigit().then_some(())?;
                out.push(first);
                for _ in 0..3 {
                    let c = rest.next()?;
                    c.is_ascii_hexdigit().then_some(())?;
                    out.push(c);
                }
            }
            out.push('}');
        }
        'x' => {
            out.push_str("\\u{");
            for _ in 0..2 {
                let c = rest.next()?;
                c.is_ascii_hexdigit().then_some(())?;
                out.push(c);
            }
            out.push('}');
        }
        _ => return None,
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(src: &str) -> Vec<Tok> {
        lex(src, 0, 0, src.len()).unwrap().into_iter().map(|t| t.tok).collect()
    }

    #[test]
    fn basic() {
        assert_eq!(
            toks("let x = 1 + 2.5"),
            vec![Tok::Let, Tok::Ident("x".into()), Tok::Assign, Tok::Int(1), Tok::Plus, Tok::Float(2.5), Tok::Newline, Tok::Eof]
        );
    }

    #[test]
    fn ranges_and_tuple_index() {
        assert_eq!(toks("1..5"), vec![Tok::Int(1), Tok::DotDot, Tok::Int(5), Tok::Newline, Tok::Eof]);
        assert_eq!(toks("t.0.1"), vec![Tok::Ident("t".into()), Tok::Dot, Tok::Int(0), Tok::Dot, Tok::Int(1), Tok::Newline, Tok::Eof]);
    }

    #[test]
    fn newlines_in_parens_ignored() {
        assert_eq!(toks("f(\n1,\n2)\n"), toks("f(1, 2)"));
    }

    #[test]
    fn continuation_lines() {
        assert_eq!(toks("xs\n  |> f\n  .g()"), toks("xs |> f .g()"));
    }

    #[test]
    fn mutating_ident() {
        assert_eq!(toks("push!(x)")[0], Tok::Ident("push!".into()));
        assert_eq!(toks("a!=b")[1], Tok::NotEq);
    }

    #[test]
    fn interpolation() {
        let src = "\"a{x + 1}b{y:.2}\"";
        let t = toks(src);
        match &t[0] {
            Tok::Str(parts) => {
                assert_eq!(parts.len(), 4);
                assert_eq!(parts[0], StrPart::Lit("a".into()));
                assert!(matches!(&parts[3], StrPart::Expr { spec: Some(s), .. } if s == ".2"));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn triple() {
        let src = "\"\"\"\n    hello\n      world\n    \"\"\"";
        match &toks(src)[0] {
            Tok::Str(parts) => assert_eq!(parts[0], StrPart::Lit("hello\n  world".into())),
            _ => panic!(),
        }
    }
}
