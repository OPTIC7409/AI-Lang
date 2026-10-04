//! Recursive-descent parser.
//!
//! Operator precedence, loosest to tightest:
//!
//! ```text
//!   or
//!   and
//!   not
//!   == != < <= > >= in, not in      (non-associative: no chaining)
//!   |>                               (right side is a call)
//!   .. ..=
//!   + -
//!   * / // %
//!   unary -
//!   **                               (right-associative)
//!   postfix: call () index [] field . try ?
//! ```

use crate::ast::*;
use crate::diagnostic::Diagnostic;
use crate::lexer::{lex, StrPart, Tok, Token};
use crate::span::Span;
use crate::types::{Name, Ty};
use crate::value::Text;
use std::rc::Rc;

const TERNARY_HELP: &str = "Cogito has no `c ? a : b`; write `if c { a } else { b }`";

type PResult<T> = Result<T, Diagnostic>;

pub struct Parser<'s> {
    src: &'s str,
    file: u32,
    toks: Vec<Token>,
    pos: usize,
    /// Current nesting depth of expressions, blocks, patterns and types.
    depth: u32,
    /// Where each block that is still open began.
    open_blocks: Vec<Span>,
}

/// Deeper nesting than this is rejected, rather than risking a native stack overflow.
const MAX_NESTING: u32 = 256;

pub fn parse_program(src: &str, file: u32) -> PResult<Program> {
    parse_program_all(src, file).map_err(|mut ds| ds.swap_remove(0))
}

/// Like [`parse_program`], but when the program does not lex, reports every
/// error the lexer found (several bad strings, say) rather than the first.
pub fn parse_program_all(src: &str, file: u32) -> Result<Program, Vec<Diagnostic>> {
    let (toks, mut errors) = crate::lexer::lex_all(src, file, 0, src.len())?;
    let mut p = Parser { src, file, toks, pos: 0, depth: 0, open_blocks: Vec::new() };
    match p.program() {
        Ok(prog) if errors.is_empty() => Ok(prog),
        Ok(_) => Err(errors),
        Err(mut more) => {
            errors.append(&mut more);
            errors.sort_by_key(|d| d.span.map_or(0, |s| s.start));
            Err(errors)
        }
    }
}

/// Parse an expression from a sub-range of the source (used for string interpolation).
pub fn parse_expr_range(src: &str, file: u32, start: usize, end: usize) -> PResult<Expr> {
    let toks = lex(src, file, start, end)?;
    let mut p = Parser { src, file, toks, pos: 0, depth: 0, open_blocks: Vec::new() };
    p.skip_newlines();
    let e = p.expr()?;
    p.skip_newlines();
    if !p.at(&Tok::Eof) {
        return Err(p.unexpected("end of the interpolated expression"));
    }
    Ok(e)
}

fn mk(kind: ExprKind, span: Span) -> Expr {
    Expr { kind, span }
}

fn new_fn(name: Option<Name>, name_span: Span, span: Span, params: Vec<Param>, body: Expr) -> FnDef {
    let mutating = name.as_ref().is_some_and(|n| n.ends_with('!'));
    FnDef {
        name,
        name_span,
        span,
        generics: vec![],
        params,
        ret: None,
        requires: vec![],
        ensures: vec![],
        body,
        mutating,
        num_slots: 0,
        captures: vec![],
        result_slot: 0,
        olds: vec![],
        global_slot: None,
        overload_fallback: None,
    }
}

pub fn type_name_hint(name: &str) -> Option<&'static str> {
    Some(match name {
        "int" | "i64" | "integer" | "i32" | "usize" => "Int",
        "float" | "f64" | "double" => "Float",
        "str" | "string" | "String" => "Str",
        "bool" | "boolean" => "Bool",
        "list" | "array" | "vec" | "Vec" | "Array" | "ArrayList" => "List",
        "map" | "dict" | "hashmap" | "HashMap" | "Dict" | "BTreeMap" => "Map",
        "set" | "HashSet" | "BTreeSet" => "Set",
        "Integer" | "Long" => "Int",
        "Double" => "Float",
        "Boolean" => "Bool",
        "any" => "Any",
        "unit" | "void" | "None" => "Unit",
        "option" | "Maybe" => "Option",
        "result" => "Result",
        _ => return None,
    })
}

impl<'s> Parser<'s> {
    // ------------------------------------------------------------ helpers

    fn peek(&self) -> &Tok {
        &self.toks[self.pos].tok
    }

    fn peek_at(&self, n: usize) -> &Tok {
        let i = (self.pos + n).min(self.toks.len() - 1);
        &self.toks[i].tok
    }

    fn span(&self) -> Span {
        self.toks[self.pos].span
    }

    fn prev_span(&self) -> Span {
        if self.pos == 0 {
            self.toks[0].span
        } else {
            self.toks[self.pos - 1].span
        }
    }

    fn at(&self, t: &Tok) -> bool {
        self.peek() == t
    }

    fn bump(&mut self) -> Token {
        let t = self.toks[self.pos].clone();
        if self.pos < self.toks.len() - 1 {
            self.pos += 1;
        }
        t
    }

    fn eat(&mut self, t: &Tok) -> bool {
        if self.at(t) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn skip_newlines(&mut self) {
        while self.at(&Tok::Newline) {
            self.bump();
        }
    }

    fn skip_terminators(&mut self) {
        while matches!(self.peek(), Tok::Newline | Tok::Semi) {
            self.bump();
        }
    }

    /// The next token that is not a newline.
    fn peek_past_newlines(&self) -> &Tok {
        let mut i = self.pos;
        while self.toks[i].tok == Tok::Newline && i < self.toks.len() - 1 {
            i += 1;
        }
        &self.toks[i].tok
    }

    fn unexpected(&self, expected: &str) -> Diagnostic {
        let found = self.peek().describe();
        let mut d =
            Diagnostic::error("E0010", format!("expected {}, found {}", expected, found)).at(self.span()).label(format!("expected {}", expected));
        if let Tok::Eof = self.peek() {
            d = d.label("the file ended here").help("check for a missing closing bracket or brace");
        } else if self.looks_like_ternary() {
            d = d.help(TERNARY_HELP);
        }
        d
    }

    /// After `c ?` with a `:` later on the line: `c ? a : b` from another
    /// language, whose `?` was read as the try operator.
    fn looks_like_ternary(&self) -> bool {
        self.pos > 0
            && self.toks[self.pos - 1].tok == Tok::Question
            && self.toks[self.pos..].iter().take_while(|t| !matches!(t.tok, Tok::Newline | Tok::Eof)).any(|t| t.tok == Tok::Colon)
    }

    fn expect(&mut self, t: &Tok, what: &str) -> PResult<Span> {
        if self.at(t) {
            Ok(self.bump().span)
        } else {
            Err(self.unexpected(what))
        }
    }

    fn expect_closing(&mut self, t: &Tok, open_span: Span, what: &str) -> PResult<Span> {
        if self.at(t) {
            Ok(self.bump().span)
        } else {
            let open = match t {
                Tok::RParen => "(",
                Tok::RBracket => "[",
                Tok::RBrace => "{",
                _ => "",
            };
            let before = &self.src[..open_span.start as usize];
            let line = before.matches('\n').count() + 1;
            let col = before.chars().rev().take_while(|c| *c != '\n').count() + 1;
            let d = self.unexpected(what).note(format!("the `{}` that needs closing is at line {}, column {}", open, line, col));
            Err(self.python_lambda(self.arrow_function(d)))
        }
    }

    /// At the `:` of a slice from Python (`xs[1:3]`, `xs[:2]`, `xs[1:]`):
    /// Cogito writes `xs[1..3]`, `xs[..2]`, `xs[1..]`. (Not with a step.)
    fn python_slice(&self) -> Diagnostic {
        let colon = self.span();
        let d = Diagnostic::error("E0010", "slices are written with `..`: `xs[1..3]`, `xs[..2]`, `xs[1..]`").at(colon).label("`:` is Python's slice");
        // The rest of the index, up to its `]`: no other `:` (a step).
        let mut depth = 0;
        let mut i = self.pos + 1;
        while i < self.toks.len() {
            match self.toks[i].tok {
                Tok::LBracket | Tok::LParen | Tok::LBrace => depth += 1,
                Tok::RBracket if depth == 0 => break,
                Tok::RBracket | Tok::RParen | Tok::RBrace => depth -= 1,
                Tok::Colon if depth == 0 => return d.help("a slice has no step in Cogito: use `xs.chunks(n).map(fn(c) => c[0])` or a comprehension"),
                Tok::Newline | Tok::Eof if depth == 0 => return d,
                _ => {}
            }
            i += 1;
        }
        let bare =
            self.pos > 0 && self.toks[self.pos - 1].tok == Tok::LBracket && self.toks.get(self.pos + 1).is_some_and(|t| t.tok == Tok::RBracket);
        if bare {
            // (`xs[:]` copies: in Cogito, `xs` itself is already a value.)
            return d.help("`xs[:]` copies the list in Python; in Cogito, use `xs` itself: values never change behind your back");
        }
        d.fix(colon, "..")
    }

    /// At the parameters after `lambda` (Python): `lambda x, y: x + y` is
    /// `fn(x, y) => x + y`.
    fn python_lambda(&self, d: Diagnostic) -> Diagnostic {
        let at = self.pos - 1;
        if !matches!(&self.toks[at].tok, Tok::Ident(w) if &**w == "lambda") {
            return d;
        }
        let starts = at > 0 && matches!(self.toks[at - 1].tok, Tok::Assign | Tok::LParen | Tok::Comma | Tok::LBracket | Tok::Colon | Tok::Return);
        // `lambda` then names separated by commas, then `:`.
        let mut i = self.pos;
        let mut names = Vec::new();
        while let Tok::Ident(n) = &self.toks[i].tok {
            names.push(n.to_string());
            i += 1;
            if self.toks[i].tok == Tok::Comma {
                i += 1;
            } else {
                break;
            }
        }
        let d = d.help("an anonymous function is written `fn(x) => x * 2`");
        if !starts || self.toks[i].tok != Tok::Colon {
            return d;
        }
        let span = self.toks[at].span.to(self.toks[i].span);
        d.fix(span, format!("fn({}) =>", names.join(", ")))
    }

    /// At a `=>` after `x` or `(a, b)` that starts an expression: an arrow
    /// function from JavaScript, C# or Scala.
    fn arrow_function(&self, d: Diagnostic) -> Diagnostic {
        if self.peek() != &Tok::FatArrow {
            return d;
        }
        // Where an expression may start (not a guard's `if`, not a call).
        let starts =
            |i: usize| i > 0 && matches!(self.toks[i - 1].tok, Tok::Assign | Tok::LParen | Tok::Comma | Tok::LBracket | Tok::Colon | Tok::Return);
        let prev = self.pos - 1;
        let fix = match &self.toks[prev].tok {
            Tok::Ident(x) if starts(prev) => Some((self.toks[prev].span, format!("fn({})", x))),
            Tok::RParen => {
                // `(a, b)` or `()`: names separated by commas.
                let mut i = prev;
                while i > 0 && matches!(self.toks[i - 1].tok, Tok::Ident(_) | Tok::Comma) {
                    i -= 1;
                }
                let open = i.checked_sub(1).filter(|&o| self.toks[o].tok == Tok::LParen);
                let names = &self.toks[i..prev];
                let alternating = names.iter().enumerate().all(|(k, t)| matches!((k % 2, &t.tok), (0, Tok::Ident(_)) | (1, Tok::Comma)));
                match open {
                    Some(o) if starts(o) && alternating && names.len() % 2 == names.len().min(1) => {
                        let at = self.toks[o].span;
                        Some((Span { end: at.start, ..at }, "fn".to_string()))
                    }
                    _ => None,
                }
            }
            _ => None,
        };
        match fix {
            Some((span, text)) => d.help("an anonymous function is written `fn(x) => x * 2`").fix(span, text),
            None => d,
        }
    }

    fn expect_terminator(&mut self) -> PResult<()> {
        match self.peek() {
            Tok::Newline | Tok::Semi => {
                self.bump();
                Ok(())
            }
            Tok::RBrace | Tok::Eof => Ok(()),
            _ => {
                let mut d = self.unexpected("end of statement");
                d.label = Some("expected a newline or `;` before this".into());
                let prev = &self.toks[self.pos - 1];
                let prev_word = if let Tok::Ident(w) = &prev.tok { Some(&**w) } else { None };
                d = match self.peek() {
                    Tok::Assign => d.help("only variables, fields and indexes can be assigned to"),
                    Tok::FatArrow => {
                        let d = self.arrow_function(d);
                        if d.fixes.is_empty() {
                            d.help("put each statement on its own line, or separate statements with `;`")
                        } else {
                            d
                        }
                    }
                    // `} elif x {`
                    Tok::Ident(w) if matches!(&**w, "elif" | "elsif" | "elseif") && prev.tok == Tok::RBrace => {
                        d.help("write `else if`").fix(self.span(), "else if")
                    }
                    _ if matches!(prev_word, Some("elif" | "elsif" | "elseif")) => d.help("write `else if`").fix(prev.span, "else if"),
                    Tok::Ident(_) if matches!(prev_word, Some("def" | "function" | "func" | "fun")) && self.peek_at(1) == &Tok::LParen => {
                        d.help("functions are declared with `fn`: `fn name(x: Int) -> Int { ... }`").fix(prev.span, "fn")
                    }
                    Tok::Ident(_) if matches!(prev_word, Some("const" | "val")) && self.peek_at(1) == &Tok::Assign => {
                        d.help("use `let` (a `let` never changes)").fix(prev.span, "let")
                    }
                    // (The name is fixed next, everywhere it is used.)
                    Tok::Upper(n) if matches!(prev_word, Some("const" | "val")) && self.peek_at(1) == &Tok::Assign => d
                        .help(format!(
                            "use `let` with a lowercase name: `let {} = ...` (a `let` never changes; uppercase names are for types and constructors)",
                            self.free_lowercase_name(n)
                        ))
                        .fix(prev.span, "let"),
                    Tok::Ident(_) | Tok::Colon if prev_word == Some("lambda") => self.python_lambda(d),
                    Tok::Ident(_) | Tok::Upper(_) if matches!(self.toks[self.pos - 1].tok, Tok::Ident(_) | Tok::Upper(_)) => {
                        d.help("two names in a row: is an operator or a comma missing?")
                    }
                    _ if self.looks_like_ternary() => d.help(TERNARY_HELP),
                    // `"[" + xs |> join(",") + "]"`
                    t if t.is_binary_operator()
                        && self.toks[..self.pos].iter().rev().take_while(|t| !matches!(t.tok, Tok::Newline | Tok::Semi | Tok::LBrace)).any(|t| t.tok == Tok::PipeGt) =>
                    {
                        d.help("`|>` takes everything on its left (`a + b |> f` is `f(a + b)`), and only another `|>` can follow the call: put the pipeline in parentheses: `a + (xs |> f) + b`")
                    }
                    _ => d.help("put each statement on its own line, or separate statements with `;`"),
                };
                Err(d)
            }
        }
    }

    /// The lowercase name for the uppercase variable `n` (`MAX_SIZE` is
    /// `max_size`): one that is neither a built-in (`MAX` would hide `max`)
    /// nor a name already in the file.
    fn free_lowercase_name(&self, n: &str) -> String {
        let base = snake_case(n);
        let taken = |name: &str| {
            crate::builtins::BUILTINS.iter().any(|b| b.name == name)
                || matches!(name, "pi" | "tau" | "e" | "inf" | "max_int" | "min_int")
                || self.toks.iter().any(|t| matches!(&t.tok, Tok::Ident(x) if &**x == name))
        };
        if !taken(&base) {
            return base;
        }
        let value = format!("{}_value", base.trim_end_matches('_'));
        if !taken(&value) {
            return value;
        }
        format!("my_{}", base.trim_end_matches('_'))
    }

    /// Every use of the uppercase variable `n` in the file (as a name, and
    /// inside interpolations), if it is certainly that variable: not a type
    /// or constructor (nothing declares it as one, nor calls it).
    fn uses_of_constant(&self, n: &str) -> Option<Vec<Span>> {
        let mut out = Vec::new();
        for (i, t) in self.toks.iter().enumerate() {
            match &t.tok {
                Tok::Upper(x) if &**x == n => {
                    let prev = i.checked_sub(1).map(|j| &self.toks[j].tok);
                    let next = self.toks.get(i + 1).map(|t| &t.tok);
                    // (A type's or variant's declaration, a constructor call,
                    // a qualified name, or a pattern `MAX => ...`, where a
                    // lowercase name would bind anything.)
                    if matches!(prev, Some(Tok::Type | Tok::Bar | Tok::Dot))
                        || matches!(next, Some(Tok::LParen | Tok::LBrace | Tok::Bar | Tok::FatArrow | Tok::If))
                    {
                        return None;
                    }
                    out.push(t.span);
                }
                Tok::Str(parts) => {
                    for p in parts {
                        if let StrPart::Expr { start, end, .. } = p {
                            let code = &self.src[*start as usize..*end as usize];
                            let is_word = |c: char| c.is_alphanumeric() || c == '_';
                            for (at, _) in code.match_indices(n) {
                                let before = code[..at].chars().next_back();
                                let after = code[at + n.len()..].chars().next();
                                if !before.is_some_and(is_word) && !after.is_some_and(is_word) && before != Some('.') {
                                    let s = *start as usize + at;
                                    out.push(Span::new(t.span.file, s, s + n.len()));
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        Some(out)
    }

    fn lower_ident(&mut self, what: &str) -> PResult<(Name, Span)> {
        match self.peek().clone() {
            Tok::Ident(n) => {
                let sp = self.bump().span;
                Ok((n, sp))
            }
            Tok::Upper(n) => {
                let sp = self.span();
                Err(Diagnostic::error("E0013", format!("{} `{}` must start with a lowercase letter", what, n))
                    .at(sp)
                    .label("uppercase names are reserved for types and constructors")
                    .help(format!("rename it to `{}`", snake_case(&n))))
            }
            t if crate::lexer::KEYWORDS.contains(&t.text()) => {
                Err(Diagnostic::error("E0010", format!("`{}` is a keyword and cannot be used as a {}", t.text(), what))
                    .at(self.span())
                    .help(format!("choose a different name, e.g. `{}_`", t.text())))
            }
            _ => Err(self.unexpected(what)),
        }
    }

    fn upper_ident(&mut self, what: &str) -> PResult<(Name, Span)> {
        match self.peek().clone() {
            Tok::Upper(n) => {
                let sp = self.bump().span;
                Ok((n, sp))
            }
            Tok::Ident(n) => {
                let sp = self.span();
                let mut c = n.chars();
                let upper = match c.next() {
                    Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                    None => String::new(),
                };
                Err(Diagnostic::error("E0013", format!("{} `{}` must start with an uppercase letter", what, n))
                    .at(sp)
                    .help(format!("rename it to `{}`", upper)))
            }
            _ => Err(self.unexpected(what)),
        }
    }

    // ------------------------------------------------------------ items

    /// The whole file. After a syntax error, parsing resumes at the next
    /// line that starts a top-level declaration or statement, so that one
    /// run reports every syntax error (up to 20).
    fn program(&mut self) -> Result<Program, Vec<Diagnostic>> {
        let mut items = Vec::new();
        let mut errors = Vec::new();
        loop {
            self.skip_terminators();
            if self.at(&Tok::Eof) {
                break;
            }
            match self.item().and_then(|i| self.expect_terminator().map(|_| i)) {
                Ok(i) => items.push(i),
                Err(d) => {
                    errors.push(d);
                    if errors.len() >= 20 {
                        break;
                    }
                    self.depth = 0;
                    self.open_blocks.clear();
                    self.recover();
                }
            }
        }
        if errors.is_empty() {
            Ok(Program { items, file: self.file, num_slots: 0 })
        } else {
            Err(errors)
        }
    }

    /// Skip to the next token that begins a line at column 0 and could start
    /// a top-level item (not a `}` or a continuation such as `|` or `.`).
    fn recover(&mut self) {
        loop {
            if self.at(&Tok::Eof) {
                return;
            }
            self.pos += 1;
            let t = &self.toks[self.pos];
            let start = t.span.start as usize;
            let line_start = start == 0 || self.src.get(..start).is_some_and(|b| b.ends_with('\n'));
            // (A `{` at the start of a line usually opens the body of what
            // came before it.)
            let continues = matches!(
                t.tok,
                Tok::RBrace
                    | Tok::LBrace
                    | Tok::RParen
                    | Tok::RBracket
                    | Tok::Else
                    | Tok::Newline
                    | Tok::Bar
                    | Tok::Dot
                    | Tok::PipeGt
                    | Tok::And
                    | Tok::Or
                    | Tok::FatArrow
                    | Tok::Requires
                    | Tok::Ensures
                    | Tok::Where
                    | Tok::Comma
            );
            if t.tok == Tok::Eof || (line_start && !continues) {
                return;
            }
        }
    }

    fn item(&mut self) -> PResult<Item> {
        match self.peek() {
            Tok::Fn if !matches!(self.peek_at(1), Tok::LParen) => Ok(Item::Fn(Rc::new(self.fn_decl()?))),
            Tok::Type => Ok(Item::Type(self.type_decl()?)),
            Tok::Test => self.test_decl(),
            Tok::Property => self.property_decl(),
            Tok::Import => self.import_decl(),
            _ => Ok(Item::Stmt(self.stmt()?)),
        }
    }

    fn fn_decl(&mut self) -> PResult<FnDef> {
        let start = self.expect(&Tok::Fn, "`fn`")?;
        let (name, name_span) = self.lower_ident("function name")?;
        let mut generics = Vec::new();
        if self.at(&Tok::LBracket) {
            self.bump();
            loop {
                if self.at(&Tok::RBracket) {
                    break;
                }
                let (g, _) = self.upper_ident("type parameter")?;
                generics.push(g);
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
            self.expect(&Tok::RBracket, "`]` after type parameters")?;
        }
        if !self.at(&Tok::LParen) {
            return Err(self.unexpected("`(` to start the parameter list").help(format!("write `fn {}() {{ ... }}`", name)));
        }
        let params = self.params()?;
        let ret = if self.eat(&Tok::Arrow) { Some(self.type_expr()?) } else { None };
        let mut requires = Vec::new();
        let mut ensures = Vec::new();
        loop {
            match self.peek_past_newlines() {
                Tok::Requires => {
                    self.skip_newlines();
                    self.bump();
                    requires.push(self.expr()?);
                }
                Tok::Ensures => {
                    self.skip_newlines();
                    self.bump();
                    ensures.push(self.expr()?);
                }
                _ => break,
            }
        }
        let body = self.fn_body(&name)?;
        let span = start.to(body.span);
        let mut def = new_fn(Some(name), name_span, span, params, body);
        def.generics = generics;
        def.ret = ret;
        def.requires = requires;
        def.ensures = ensures;
        Ok(def)
    }

    fn fn_body(&mut self, name: &str) -> PResult<Expr> {
        match self.peek_past_newlines() {
            Tok::LBrace => {
                self.skip_newlines();
                self.block()
            }
            Tok::FatArrow => {
                self.skip_newlines();
                self.bump();
                self.skip_newlines();
                // Like a match arm, the body may be one assignment.
                self.arm_body()
            }
            Tok::Assign => Err(self.unexpected("function body").help(format!("single-expression functions use `=>`: `fn {}(x) => x * 2`", name))),
            Tok::Colon if !matches!(self.peek_at(1), Tok::Upper(_) | Tok::LParen | Tok::Ident(_)) => Err(self
                .unexpected("function body `{ ... }` or `=> expression`")
                .help(format!("a function body goes in braces, not after a colon: `fn {}(x) {{ ... }}` (indentation has no meaning)", name))),
            Tok::Colon => {
                let sp = self.span();
                let spaced = sp.start > 0 && self.src.as_bytes()[sp.start as usize - 1] == b' ';
                Err(self
                    .unexpected("function body `{ ... }` or `=> expression`")
                    .help(format!("the return type follows `->`: `fn {}(x: Int) -> Int`", name))
                    .fix(sp, if spaced { "->" } else { " ->" }))
            }
            _ => Err(self.unexpected("function body `{ ... }` or `=> expression`")),
        }
    }

    fn params(&mut self) -> PResult<Vec<Param>> {
        let open = self.expect(&Tok::LParen, "`(`")?;
        let mut params = Vec::new();
        loop {
            if self.at(&Tok::RParen) {
                break;
            }
            // A parameter may be a destructuring pattern: `fn((k, v)) => ...`,
            // `fn norm(Point(x, y): Point)`.
            let ctor = (matches!(self.peek(), Tok::Upper(_)) && self.peek_at(1) == &Tok::LParen)
                || (matches!(self.peek(), Tok::Ident(_)) && self.peek_at(1) == &Tok::Dot && matches!(self.peek_at(2), Tok::Upper(_)));
            let (name, span, pat) = if ctor || matches!(self.peek(), Tok::LParen | Tok::LBracket | Tok::LBrace) {
                let pat = self.pattern_primary()?;
                (Rc::from(format!("__arg{}", params.len()).as_str()), pat.span, Some(pat))
            } else {
                let (name, span) = self.lower_ident("parameter name")?;
                if name.ends_with('!') {
                    return Err(Diagnostic::error("E0013", "parameter names cannot end with `!`").at(span));
                }
                (name, span, None)
            };
            let ty = if self.eat(&Tok::Colon) { Some(self.type_expr()?) } else { None };
            let default = if self.eat(&Tok::Assign) { Some(self.expr()?) } else { None };
            if default.is_none() && params.iter().any(|p: &Param| p.default.is_some()) {
                return Err(Diagnostic::error("E0010", format!("parameter `{}` needs a default value, because an earlier parameter has one", name))
                    .at(span)
                    .help("parameters with default values must come after all the others"));
            }
            params.push(Param { name, span, ty, default, slot: 0, pat });
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        self.expect_closing(&Tok::RParen, open, "`,` or `)` in parameter list")?;
        Ok(params)
    }

    fn type_decl(&mut self) -> PResult<TypeDecl> {
        let start = self.expect(&Tok::Type, "`type`")?;
        let (name, name_span) = self.upper_ident("type name")?;
        let mut params = Vec::new();
        if self.eat(&Tok::LBracket) {
            loop {
                if self.at(&Tok::RBracket) {
                    break;
                }
                let (p, _) = self.upper_ident("type parameter")?;
                params.push(p);
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
            self.expect(&Tok::RBracket, "`]`")?;
        }
        self.expect(&Tok::Assign, "`=` after the type name")?;
        self.skip_newlines();
        let is_alias = match self.peek() {
            Tok::LParen | Tok::Fn => true,
            Tok::Ident(_) => self.peek_at(1) == &Tok::Dot,
            Tok::Upper(n) => {
                matches!(&**n, "Int" | "Float" | "Str" | "Bool" | "Unit" | "Any" | "Range" | "List" | "Map" | "Option" | "Result" | "Ordering")
                    || self.peek_at(1) == &Tok::LBracket
            }
            _ => false,
        };
        let body = if is_alias {
            TypeBody::Alias(self.type_expr()?)
        } else if self.at(&Tok::LBrace) {
            let open = self.bump().span;
            let mut fields = Vec::new();
            loop {
                self.skip_newlines();
                if self.at(&Tok::RBrace) {
                    break;
                }
                let (fname, fspan) = self.lower_ident("field name")?;
                self.expect(&Tok::Colon, "`:` and a type after the field name")?;
                let ty = self.type_expr()?;
                fields.push(FieldDecl { name: Some(fname), span: fspan.to(ty.span), ty });
                self.skip_newlines();
                if !self.eat(&Tok::Comma) {
                    self.skip_newlines();
                    break;
                }
            }
            self.expect_closing(&Tok::RBrace, open, "`,` or `}` in record type")?;
            TypeBody::Record(fields)
        } else {
            self.eat(&Tok::Bar);
            self.skip_newlines();
            let mut variants = Vec::new();
            loop {
                let (vname, vspan) = self.upper_ident("variant name")?;
                let mut fields = Vec::new();
                let mut has_parens = false;
                if self.at(&Tok::LParen) {
                    has_parens = true;
                    let open = self.bump().span;
                    loop {
                        if self.at(&Tok::RParen) {
                            break;
                        }
                        if matches!(self.peek(), Tok::Ident(_)) && self.peek_at(1) == &Tok::Colon {
                            let (fname, fspan) = self.lower_ident("field name")?;
                            self.bump();
                            let ty = self.type_expr()?;
                            fields.push(FieldDecl { name: Some(fname), span: fspan.to(ty.span), ty });
                        } else {
                            let ty = self.type_expr()?;
                            fields.push(FieldDecl { name: None, span: ty.span, ty });
                        }
                        if !self.eat(&Tok::Comma) {
                            break;
                        }
                    }
                    self.expect_closing(&Tok::RParen, open, "`,` or `)` in variant fields")?;
                    let named = fields.iter().filter(|f| f.name.is_some()).count();
                    if named != 0 && named != fields.len() {
                        return Err(Diagnostic::error("E0010", "variant fields must be either all named or all positional")
                            .at(vspan)
                            .help("write `Rect(w: Float, h: Float)` or `Rect(Float, Float)`"));
                    }
                }
                variants.push(VariantDecl { name: vname, span: vspan, fields, has_parens, slot: 0 });
                if self.peek_past_newlines() == &Tok::Bar {
                    self.skip_newlines();
                    self.bump();
                    self.skip_newlines();
                } else {
                    break;
                }
            }
            TypeBody::Enum(variants)
        };
        // `where` clauses: the type's invariant.
        let mut invariants = Vec::new();
        while self.peek_past_newlines() == &Tok::Where {
            self.skip_newlines();
            let w = self.bump().span;
            if !matches!(body, TypeBody::Record(_)) {
                return Err(Diagnostic::error("E0116", "only record types can have an invariant (`where`)")
                    .at(w)
                    .help("give the condition to the functions that build the value, as `requires`/`ensures`"));
            }
            self.skip_newlines();
            invariants.push(self.expr()?);
        }
        let span = start.to(self.prev_span());
        Ok(TypeDecl { name, name_span, span, params, body, invariants, id: 0, slot: 0 })
    }

    fn plain_string(&mut self, what: &str) -> PResult<(String, Span)> {
        match self.peek().clone() {
            Tok::Str(parts) => {
                let sp = self.bump().span;
                let mut s = String::new();
                for p in parts {
                    match p {
                        StrPart::Lit(l) => s.push_str(&l),
                        StrPart::Expr { .. } => return Err(Diagnostic::error("E0010", format!("{} cannot contain interpolation", what)).at(sp)),
                    }
                }
                Ok((s, sp))
            }
            _ => Err(self.unexpected(what)),
        }
    }

    fn test_decl(&mut self) -> PResult<Item> {
        let start = self.expect(&Tok::Test, "`test`")?;
        let (name, _) = self.plain_string("test name (a string)")?;
        self.skip_newlines();
        if !self.at(&Tok::LBrace) {
            return Err(self.unexpected("`{` to start the test body"));
        }
        let body = self.block()?;
        let span = start.to(body.span);
        let func = new_fn(Some(Rc::from(format!("test \"{}\"", name).as_str())), start, span, vec![], body);
        Ok(Item::Test(TestDecl { name, span, func: Rc::new(func) }))
    }

    fn property_decl(&mut self) -> PResult<Item> {
        let start = self.expect(&Tok::Property, "`property`")?;
        let (name, _) = self.plain_string("property name (a string)")?;
        if !self.at(&Tok::LParen) {
            return Err(self.unexpected("`(` with the property's inputs").help("write `property \"name\" (x: Int, xs: List[Int]) { ... }`"));
        }
        let params = self.params()?;
        let mut requires = Vec::new();
        while self.peek_past_newlines() == &Tok::Where {
            self.skip_newlines();
            self.bump();
            requires.push(self.expr()?);
        }
        self.skip_newlines();
        if !self.at(&Tok::LBrace) {
            return Err(self.unexpected("`{` to start the property body"));
        }
        let body = self.block()?;
        let span = start.to(body.span);
        let mut func = new_fn(Some(Rc::from(format!("property \"{}\"", name).as_str())), start, span, params, body);
        func.requires = requires;
        Ok(Item::Property(PropDecl { name, span, func: Rc::new(func) }))
    }

    fn import_decl(&mut self) -> PResult<Item> {
        let start = self.expect(&Tok::Import, "`import`")?;
        let (path, path_span) = self.plain_string("module path (a string)")?;
        let alias = if self.eat(&Tok::As) { Some(self.lower_ident("module alias")?.0) } else { None };
        Ok(Item::Import(ImportDecl { path, path_span, alias, span: start.to(self.prev_span()), module: None, slot: 0 }))
    }

    // ------------------------------------------------------------ statements

    fn stmt(&mut self) -> PResult<Stmt> {
        let start = self.span();
        match self.peek() {
            Tok::Let | Tok::Var => {
                let mutable = self.bump().tok == Tok::Var;
                let kw = if mutable { "var" } else { "let" };
                if matches!(self.peek(), Tok::Ident(w) if &**w == "mut") && matches!(self.peek_at(1), Tok::Ident(_) | Tok::LParen) {
                    let mut_span = self.span();
                    let d = Diagnostic::error("E0010", format!("Cogito has no `{} mut`", kw))
                        .at(start.to(mut_span))
                        .help("declare a variable that changes with `var`: `var x = 1`");
                    let space = usize::from(self.src.as_bytes().get(mut_span.end as usize) == Some(&b' '));
                    return Err(if mutable {
                        d.fix(Span::new(mut_span.file, mut_span.start as usize, mut_span.end as usize + space), "")
                    } else {
                        d.fix(start.to(mut_span), "var")
                    });
                }
                if let Tok::Upper(n) = self.peek().clone() {
                    if !matches!(self.peek_at(1), Tok::LParen) {
                        let new_name = self.free_lowercase_name(&n);
                        let mut d = Diagnostic::error("E0013", format!("variable `{}` must start with a lowercase letter", n))
                            .at(self.span())
                            .label("uppercase names are reserved for types and constructors")
                            .help(format!("rename it to `{}` (everywhere it is used)", new_name));
                        if n.chars().all(|c| !c.is_lowercase()) {
                            d = d.note("Cogito has no separate constant syntax: a `let` never changes");
                        }
                        if let Some(spans) = self.uses_of_constant(&n) {
                            for sp in spans {
                                d = d.fix(sp, new_name.clone());
                            }
                        }
                        return Err(d);
                    }
                }
                let pat = self.pattern()?;
                let ty = if self.eat(&Tok::Colon) { Some(self.type_expr()?) } else { None };
                if !self.at(&Tok::Assign) {
                    return Err(self.unexpected(&format!("`=` in `{}` binding", kw)).help(format!("every binding needs a value: `{} x = 1`", kw)));
                }
                self.bump();
                self.skip_newlines();
                let value = self.expr()?;
                if self.empty_braces(&value) {
                    return Err(self.empty_braces_error(&value, ty.as_ref()));
                }
                if self.at(&Tok::Else) {
                    return Err(Diagnostic::error("E0001", format!("Cogito has no `{} ... else`", kw))
                        .at(self.span())
                        .help("use `match`: `let x = match value { Some(v) => v, _ => return ... }`"));
                }
                let span = start.to(value.span);
                Ok(Stmt { kind: StmtKind::Let { pat, ty, value, mutable }, span })
            }
            Tok::Fn if !matches!(self.peek_at(1), Tok::LParen) => {
                let def = self.fn_decl()?;
                let span = def.span;
                Ok(Stmt { kind: StmtKind::Fn { def: Rc::new(def), res: VarRes::Unresolved }, span })
            }
            Tok::Type | Tok::Test | Tok::Property | Tok::Import => {
                let mut d = Diagnostic::error("E0116", format!("`{}` declarations are only allowed at the top level of a file", self.peek().text()))
                    .at(self.span());
                // At the start of a line, it was probably meant to be at the
                // top level, after a block that was never closed.
                let sp = self.span().start as usize;
                let at_line_start = self.src.get(..sp).is_some_and(|b| b.ends_with('\n') || b.is_empty());
                if let (true, Some(open)) = (at_line_start, self.open_blocks.first()) {
                    let line = self.src.get(..open.start as usize).map_or(0, |b| b.matches('\n').count() + 1);
                    d = d.note(format!("the block opened by the `{{` on line {} is still open here", line)).help("is a `}` missing above?");
                }
                Err(d)
            }
            Tok::Assert => {
                self.bump();
                let cond = self.expr()?;
                let msg = if self.eat(&Tok::Comma) { Some(self.expr()?) } else { None };
                let span = start.to(self.prev_span());
                Ok(Stmt { kind: StmtKind::Assert { cond, msg }, span })
            }
            _ => {
                let e = self.expr()?;
                let op = match self.peek() {
                    Tok::Assign => Some(None),
                    Tok::PlusAssign => Some(Some(BinOp::Add)),
                    Tok::MinusAssign => Some(Some(BinOp::Sub)),
                    Tok::StarAssign => Some(Some(BinOp::Mul)),
                    Tok::SlashAssign => Some(Some(BinOp::Div)),
                    Tok::PercentAssign => Some(Some(BinOp::Mod)),
                    Tok::SlashSlashAssign => Some(Some(BinOp::FloorDiv)),
                    Tok::StarStarAssign => Some(Some(BinOp::Pow)),
                    _ => None,
                };
                if let Some(op) = op {
                    self.bump();
                    check_place(&e)?;
                    self.skip_newlines();
                    let value = self.value_expr()?;
                    let span = e.span.to(value.span);
                    return Ok(Stmt { kind: StmtKind::Assign { target: e, op, value, ty: None }, span });
                }
                let span = e.span;
                Ok(Stmt { kind: StmtKind::Expr(e), span })
            }
        }
    }

    fn block(&mut self) -> PResult<Expr> {
        self.enter()?;
        let r = self.block_inner();
        self.depth -= 1;
        r
    }

    fn block_inner(&mut self) -> PResult<Expr> {
        let open = self.expect(&Tok::LBrace, "`{`")?;
        self.open_blocks.push(open);
        let mut stmts = Vec::new();
        loop {
            self.skip_terminators();
            if self.at(&Tok::RBrace) {
                break;
            }
            if self.at(&Tok::Eof) {
                return Err(Diagnostic::error("E0010", "unclosed `{`").at(open).label("this block is never closed").help("add a matching `}`"));
            }
            stmts.push(self.stmt()?);
            // `{1, 2, 3}` from Python: a block cannot hold a list of values.
            if stmts.len() == 1 && self.at(&Tok::Comma) {
                return Err(Diagnostic::error("E0010", "`{` starts a block here, and a block cannot hold values separated by commas")
                    .at(open.to(self.span()))
                    .help("for a set write `to_set([1, 2, 3])`, for a list `[1, 2, 3]`, and for a record `{ name: value }`"));
            }
            self.expect_terminator()?;
        }
        let close = self.bump().span;
        self.open_blocks.pop();
        Ok(mk(ExprKind::Block(stmts), open.to(close)))
    }

    // ------------------------------------------------------------ expressions

    fn enter(&mut self) -> PResult<()> {
        self.depth += 1;
        if self.depth > MAX_NESTING {
            return Err(Diagnostic::error("E0010", format!("code is nested too deeply (more than {} levels)", MAX_NESTING))
                .at(self.span())
                .help("split deeply nested expressions into smaller pieces with `let`"));
        }
        Ok(())
    }

    pub fn expr(&mut self) -> PResult<Expr> {
        self.enter()?;
        let r = self.or_expr();
        self.depth -= 1;
        r
    }

    fn or_expr(&mut self) -> PResult<Expr> {
        let mut lhs = self.and_expr()?;
        while self.at(&Tok::Or) {
            self.bump();
            self.skip_newlines();
            let rhs = self.and_expr()?;
            let span = lhs.span.to(rhs.span);
            lhs = mk(ExprKind::Or(Box::new(lhs), Box::new(rhs)), span);
        }
        Ok(lhs)
    }

    fn and_expr(&mut self) -> PResult<Expr> {
        let mut lhs = self.not_expr()?;
        while self.at(&Tok::And) {
            self.bump();
            self.skip_newlines();
            let rhs = self.not_expr()?;
            let span = lhs.span.to(rhs.span);
            lhs = mk(ExprKind::And(Box::new(lhs), Box::new(rhs)), span);
        }
        Ok(lhs)
    }

    fn not_expr(&mut self) -> PResult<Expr> {
        if self.at(&Tok::Not) {
            let start = self.bump().span;
            self.enter()?;
            let e = self.not_expr();
            self.depth -= 1;
            let e = e?;
            let span = start.to(e.span);
            return Ok(mk(ExprKind::Unary { op: UnOp::Not, expr: Box::new(e) }, span));
        }
        self.cmp_expr()
    }

    fn cmp_op(&self) -> Option<(BinOp, usize)> {
        Some(match self.peek() {
            Tok::EqEq => (BinOp::Eq, 1),
            Tok::NotEq => (BinOp::Ne, 1),
            Tok::Lt => (BinOp::Lt, 1),
            Tok::Le => (BinOp::Le, 1),
            Tok::Gt => (BinOp::Gt, 1),
            Tok::Ge => (BinOp::Ge, 1),
            Tok::In => (BinOp::In, 1),
            Tok::Not if self.peek_at(1) == &Tok::In => (BinOp::NotIn, 2),
            _ => return None,
        })
    }

    fn cmp_expr(&mut self) -> PResult<Expr> {
        let lhs = self.pipe_expr()?;
        if self.at(&Tok::Is) {
            let is_span = self.bump().span;
            let pat = self.pattern()?;
            if self.cmp_op().is_some() || self.at(&Tok::Is) {
                return Err(Diagnostic::error("E0011", "comparison operators cannot be chained")
                    .at(is_span.to(self.span()))
                    .help("`is` is a comparison: put the test in parentheses, `(x is Some(_)) == flag`"));
            }
            let span = lhs.span.to(pat.span);
            return Ok(mk(ExprKind::Is { expr: Box::new(lhs), pat }, span));
        }
        let Some((op, n)) = self.cmp_op() else { return Ok(lhs) };
        let op_span = self.span();
        for _ in 0..n {
            self.bump();
        }
        self.skip_newlines();
        let rhs = self.pipe_expr()?;
        if self.at(&Tok::Is) {
            return Err(Diagnostic::error("E0011", "comparison operators cannot be chained")
                .at(op_span.to(self.span()))
                .help("`is` is a comparison: put the test in parentheses, `flag == (x is Some(_))`"));
        }
        if let Some((op2, _)) = self.cmp_op() {
            return Err(Diagnostic::error("E0011", "comparison operators cannot be chained").at(op_span.to(self.span())).help(format!(
                "write `a {} b and b {} c` instead",
                op.symbol(),
                op2.symbol()
            )));
        }
        let span = lhs.span.to(rhs.span);
        Ok(mk(ExprKind::Binary { op, lhs: Box::new(lhs), rhs: Box::new(rhs) }, span))
    }

    fn pipe_expr(&mut self) -> PResult<Expr> {
        let mut lhs = self.range_expr()?;
        while self.at(&Tok::PipeGt) {
            let op_span = self.bump().span;
            self.skip_newlines();
            let rhs = self.postfix_expr()?;
            let span = lhs.span.to(rhs.span);
            lhs = match rhs.kind {
                ExprKind::Call { callee, mut args } => {
                    args.insert(0, Arg { name: None, value: lhs });
                    mk(ExprKind::Call { callee, args }, span)
                }
                ExprKind::MethodCall { mutating: true, .. } => {
                    return Err(Diagnostic::error("E0111", "cannot pipe into a mutating function")
                        .at(rhs.span)
                        .help("mutating functions (ending in `!`) need a variable to change; call them directly"))
                }
                ExprKind::MethodCall { receiver, method, method_span, mut args, mutating, root_ty } => {
                    args.insert(0, Arg { name: None, value: lhs });
                    mk(ExprKind::MethodCall { receiver, method, method_span, args, mutating, root_ty }, span)
                }
                _ => {
                    let callee = rhs;
                    if let ExprKind::Var(v) = &callee.kind {
                        if v.name.ends_with('!') {
                            return Err(Diagnostic::error("E0111", "cannot pipe into a mutating function").at(op_span.to(callee.span)));
                        }
                    }
                    mk(ExprKind::Call { callee: Box::new(callee), args: vec![Arg { name: None, value: lhs }] }, span)
                }
            };
        }
        Ok(lhs)
    }

    fn can_start_expr(&self) -> bool {
        matches!(
            self.peek(),
            Tok::Int(_)
                | Tok::Float(_)
                | Tok::Str(_)
                | Tok::Ident(_)
                | Tok::Upper(_)
                | Tok::LParen
                | Tok::LBracket
                | Tok::LBrace
                | Tok::Minus
                | Tok::Not
                | Tok::If
                | Tok::Match
                | Tok::Fn
                | Tok::True
                | Tok::False
                | Tok::While
                | Tok::For
                | Tok::Loop
                | Tok::Return
                | Tok::Break
                | Tok::Continue
                | Tok::Assert
        )
    }

    fn range_expr(&mut self) -> PResult<Expr> {
        let lhs = self.add_expr()?;
        let inclusive = match self.peek() {
            Tok::DotDot => false,
            Tok::DotDotEq => true,
            _ => return Ok(lhs),
        };
        self.bump();
        let end = if self.can_start_expr() && !matches!(self.peek(), Tok::LBrace | Tok::If | Tok::For) {
            Some(Box::new(self.add_expr()?))
        } else if inclusive {
            return Err(self.unexpected("the end of the inclusive range"));
        } else {
            None
        };
        let span = match &end {
            Some(e) => lhs.span.to(e.span),
            None => lhs.span.to(self.prev_span()),
        };
        Ok(mk(ExprKind::Range { start: Box::new(lhs), end, inclusive }, span))
    }

    fn add_expr(&mut self) -> PResult<Expr> {
        let mut lhs = self.mul_expr()?;
        loop {
            let op = match self.peek() {
                Tok::Plus => BinOp::Add,
                Tok::Minus => BinOp::Sub,
                _ => break,
            };
            self.bump();
            self.skip_newlines();
            let rhs = self.mul_expr()?;
            let span = lhs.span.to(rhs.span);
            lhs = mk(ExprKind::Binary { op, lhs: Box::new(lhs), rhs: Box::new(rhs) }, span);
        }
        Ok(lhs)
    }

    fn mul_expr(&mut self) -> PResult<Expr> {
        let mut lhs = self.unary_expr()?;
        loop {
            let op = match self.peek() {
                Tok::Star => BinOp::Mul,
                Tok::Slash => BinOp::Div,
                Tok::SlashSlash => BinOp::FloorDiv,
                Tok::Percent => BinOp::Mod,
                _ => break,
            };
            self.bump();
            self.skip_newlines();
            let rhs = self.unary_expr()?;
            let span = lhs.span.to(rhs.span);
            lhs = mk(ExprKind::Binary { op, lhs: Box::new(lhs), rhs: Box::new(rhs) }, span);
        }
        Ok(lhs)
    }

    fn unary_expr(&mut self) -> PResult<Expr> {
        if self.at(&Tok::Minus) {
            let start = self.bump().span;
            self.enter()?;
            let e = self.unary_expr();
            self.depth -= 1;
            let e = e?;
            let span = start.to(e.span);
            return Ok(mk(ExprKind::Unary { op: UnOp::Neg, expr: Box::new(e) }, span));
        }
        if self.at(&Tok::Not) {
            return self.not_expr();
        }
        self.power_expr()
    }

    fn power_expr(&mut self) -> PResult<Expr> {
        let base = self.postfix_expr()?;
        if self.at(&Tok::StarStar) {
            self.bump();
            self.skip_newlines();
            let exp = self.unary_expr()?;
            let span = base.span.to(exp.span);
            return Ok(mk(ExprKind::Binary { op: BinOp::Pow, lhs: Box::new(base), rhs: Box::new(exp) }, span));
        }
        Ok(base)
    }

    fn args(&mut self) -> PResult<(Vec<Arg>, Span)> {
        let open_idx = self.pos;
        let open = self.expect(&Tok::LParen, "`(`")?;
        let mut args = Vec::new();
        loop {
            if self.at(&Tok::RParen) {
                break;
            }
            if matches!(self.peek(), Tok::Ident(_)) && self.peek_at(1) == &Tok::Assign {
                let eq = self.toks[self.pos + 1].span;
                let spaced = self.src.as_bytes().get(eq.end as usize) == Some(&b' ');
                return Err(Diagnostic::error("E0010", "named arguments are written `name: value`")
                    .at(eq)
                    .help("write `f(x: 1)`; `=` only assigns to variables")
                    .fix(eq, if spaced { ":" } else { ": " }));
            }
            let name = if matches!(self.peek(), Tok::Ident(_)) && self.peek_at(1) == &Tok::Colon {
                let (n, _) = self.lower_ident("argument name")?;
                self.bump();
                Some(n)
            } else {
                None
            };
            if self.at(&Tok::DotDot) && name.is_none() {
                let d = Diagnostic::error("E0010", "a spread `..xs` cannot be a function argument")
                    .at(self.span())
                    .help("spreads work in list literals, records and patterns: to pass the elements as one list, write `f([a, ..xs])`");
                // `max(...xs)` from JavaScript. (No fix: `max(xs)` gives an
                // Option, where JavaScript gives a number.)
                let callee = open_idx.checked_sub(1).map(|i| &self.toks[i].tok);
                if args.is_empty() && matches!(callee, Some(Tok::Ident(f)) if matches!(&**f, "max" | "min")) {
                    return Err(
                        d.help("`max(xs)` and `min(xs)` take the list itself, and return an Option (`None` for an empty list): `max(xs).unwrap()`")
                    );
                }
                return Err(d);
            }
            let value = self.value_expr()?;
            if name.is_none() && args.iter().any(|a: &Arg| a.name.is_some()) {
                return Err(Diagnostic::error("E0108", "positional arguments must come before named arguments")
                    .at(value.span)
                    .help("move this argument before the named ones, or name it too"));
            }
            args.push(Arg { name, value });
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        let close = self.expect_closing(&Tok::RParen, open, "`,` or `)` in argument list")?;
        Ok((args, open.to(close)))
    }

    fn postfix_expr(&mut self) -> PResult<Expr> {
        let mut e = self.primary()?;
        loop {
            match self.peek() {
                Tok::LParen => {
                    let (mut args, aspan) = self.args()?;
                    let span = e.span.to(aspan);
                    // `push!(xs, 1)` is sugar for `xs.push!(1)`.
                    if let ExprKind::Var(v) = &e.kind {
                        if v.name.ends_with('!') {
                            if args.is_empty() {
                                return Err(Diagnostic::error("E0111", format!("`{}` mutates its first argument, so it needs one", v.name))
                                    .at(span)
                                    .help(format!("call it as `variable.{}(...)`", v.name)));
                            }
                            if let Some(n) = &args[0].name {
                                return Err(Diagnostic::error(
                                    "E0111",
                                    format!("the variable that `{}` changes cannot be passed by name (`{}: ...`)", v.name, n),
                                )
                                .at(span)
                                .help(format!(
                                    "pass the variable first, without a name: `{}(variable, ...)` or `variable.{}(...)`",
                                    v.name, v.name
                                )));
                            }
                            let receiver = args.remove(0).value;
                            let method = Var::new(v.name.clone());
                            let method_span = e.span;
                            e = mk(
                                ExprKind::MethodCall { receiver: Box::new(receiver), method, method_span, args, mutating: true, root_ty: None },
                                span,
                            );
                            continue;
                        }
                    }
                    e = mk(ExprKind::Call { callee: Box::new(e), args }, span);
                }
                Tok::LBracket => {
                    let open = self.bump().span;
                    // `xs[:n]` from Python.
                    if self.at(&Tok::Colon) {
                        return Err(self.python_slice());
                    }
                    // `xs[..n]` and `xs[..=n]` slice from the start.
                    let index = if matches!(self.peek(), Tok::DotDot | Tok::DotDotEq) {
                        let inclusive = self.bump().tok == Tok::DotDotEq;
                        let end = self.add_expr()?;
                        let span = open.to(end.span);
                        mk(ExprKind::Range { start: Box::new(mk(ExprKind::Int(0), open)), end: Some(Box::new(end)), inclusive }, span)
                    } else {
                        self.expr()?
                    };
                    // `xs[a:b]` from Python.
                    if self.at(&Tok::Colon) {
                        return Err(self.python_slice());
                    }
                    let close = self.expect_closing(&Tok::RBracket, open, "`]` after index")?;
                    let span = e.span.to(close);
                    e = mk(ExprKind::Index { target: Box::new(e), index: Box::new(index) }, span);
                }
                Tok::Dot => {
                    self.bump();
                    match self.peek().clone() {
                        Tok::Ident(name) => {
                            let name_span = self.bump().span;
                            if self.at(&Tok::LParen) {
                                let (args, aspan) = self.args()?;
                                let span = e.span.to(aspan);
                                let mutating = name.ends_with('!');
                                e = mk(
                                    ExprKind::MethodCall {
                                        receiver: Box::new(e),
                                        method: Var::new(name),
                                        method_span: name_span,
                                        args,
                                        mutating,
                                        root_ty: None,
                                    },
                                    span,
                                );
                            } else if name.ends_with('!') {
                                let mut d = Diagnostic::error("E0111", format!("mutating function `{}` must be called", name))
                                    .at(name_span)
                                    .help(format!("write `.{}(...)`", name));
                                // (Certain only for built-ins that take nothing but the receiver.)
                                if crate::builtins::BUILTINS.iter().any(|b| b.name == &*name && b.max == 1) {
                                    d = d
                                        .help(format!("write `.{}()`", name))
                                        .fix(Span::new(name_span.file, name_span.end as usize, name_span.end as usize), "()");
                                }
                                return Err(d);
                            } else {
                                let span = e.span.to(name_span);
                                e = mk(ExprKind::Field { target: Box::new(e), name, name_span }, span);
                            }
                        }
                        Tok::Upper(name) => {
                            let name_span = self.bump().span;
                            let span = e.span.to(name_span);
                            e = mk(ExprKind::Field { target: Box::new(e), name, name_span }, span);
                        }
                        Tok::Int(n) => {
                            let name_span = self.bump().span;
                            let span = e.span.to(name_span);
                            e = mk(ExprKind::Field { target: Box::new(e), name: Rc::from(n.to_string().as_str()), name_span }, span);
                        }
                        _ => return Err(self.unexpected("a field or method name after `.`")),
                    }
                }
                Tok::Question => {
                    let q = self.bump().span;
                    let span = e.span.to(q);
                    e = mk(ExprKind::Try(Box::new(e)), span);
                }
                _ => break,
            }
        }
        Ok(e)
    }

    fn string_expr(&mut self, parts: Vec<StrPart>, span: Span) -> PResult<Expr> {
        if parts.len() == 1 {
            if let StrPart::Lit(s) = &parts[0] {
                return Ok(mk(ExprKind::Str(Rc::new(Text::new(s.clone()))), span));
            }
        }
        let mut out = Vec::new();
        for p in parts {
            match p {
                StrPart::Lit(s) => out.push(InterpPart::Lit(Rc::new(Text::new(s)))),
                StrPart::Expr { start, end, spec } => {
                    let e = parse_expr_range(self.src, self.file, start as usize, end as usize)?;
                    let spec = match spec {
                        Some(s) => Some(parse_fmt_spec(&s).map_err(|msg| {
                            Diagnostic::error("E0005", format!("invalid format spec `{}`: {}", s, msg))
                                .at(Span::new(self.file, end as usize, end as usize + s.len() + 1))
                                .help("to write a literal brace in a string, escape it: `\\{` (or use a raw string, r\"...\", which has no interpolation)\nformat specs look like `{x:.2}` (2 decimals), `{x:>8}` (right-align in 8 columns), `{n:05}` (zero-pad), `{n:x}` (hex)")
                        })?),
                        None => None,
                    };
                    out.push(InterpPart::Expr(e, spec));
                }
            }
        }
        Ok(mk(ExprKind::Interp(out), span))
    }

    fn primary(&mut self) -> PResult<Expr> {
        let start = self.span();
        match self.peek().clone() {
            Tok::Int(n) => {
                self.bump();
                Ok(mk(ExprKind::Int(n), start))
            }
            Tok::Float(f) => {
                self.bump();
                Ok(mk(ExprKind::Float(f), start))
            }
            Tok::True => {
                self.bump();
                Ok(mk(ExprKind::Bool(true), start))
            }
            Tok::False => {
                self.bump();
                Ok(mk(ExprKind::Bool(false), start))
            }
            Tok::Str(parts) => {
                self.bump();
                self.string_expr(parts, start)
            }
            Tok::Ident(name) => {
                self.bump();
                if &*name == "_" {
                    return Err(Diagnostic::error("E0010", "`_` can only be used in patterns")
                        .at(start)
                        .help("`_` means \"ignore this value\"; it cannot be read"));
                }
                Ok(mk(ExprKind::Var(Var::new(name)), start))
            }
            Tok::Upper(name) => {
                self.bump();
                Ok(mk(ExprKind::Var(Var::new(name)), start))
            }
            Tok::LParen => {
                self.bump();
                if self.at(&Tok::RParen) {
                    let end = self.bump().span;
                    return Ok(mk(ExprKind::Unit, start.to(end)));
                }
                let first = self.expr()?;
                if self.eat(&Tok::Comma) {
                    let mut items = vec![first];
                    loop {
                        if self.at(&Tok::RParen) {
                            break;
                        }
                        items.push(self.expr()?);
                        if !self.eat(&Tok::Comma) {
                            break;
                        }
                    }
                    let end = self.expect_closing(&Tok::RParen, start, "`,` or `)` in tuple")?;
                    return Ok(mk(ExprKind::Tuple(items), start.to(end)));
                }
                let end = self.expect_closing(&Tok::RParen, start, "`)`")?;
                let mut e = first;
                e.span = start.to(end);
                Ok(e)
            }
            Tok::LBracket => self.list_like(),
            Tok::LBrace => {
                if self.brace_is_record() {
                    self.record_literal()
                } else if let Some(d) = self.brace_map() {
                    Err(d)
                } else {
                    self.block()
                }
            }
            Tok::If => self.if_expr(),
            Tok::Match => self.match_expr(),
            Tok::Fn => {
                self.bump();
                if let Tok::Ident(n) = self.peek() {
                    return Err(Diagnostic::error("E0010", format!("named function `{}` cannot be used as an expression", n))
                        .at(start.to(self.span()))
                        .help("declare it as a statement on its own line, or use an anonymous function: `fn(x) => x + 1`"));
                }
                let params = self.params()?;
                let ret = if self.eat(&Tok::Arrow) { Some(self.type_expr()?) } else { None };
                let body = if self.at(&Tok::FatArrow) {
                    self.bump();
                    self.skip_newlines();
                    self.arm_body()?
                } else if self.at(&Tok::LBrace) {
                    self.block()?
                } else {
                    return Err(self.unexpected("`=>` or `{` for the function body"));
                };
                let span = start.to(body.span);
                let mut def = new_fn(None, start, span, params, body);
                def.ret = ret;
                Ok(mk(ExprKind::Lambda(Rc::new(def)), span))
            }
            Tok::While => {
                self.bump();
                if matches!(self.peek(), Tok::Let | Tok::Var) {
                    return Err(Diagnostic::error("E0001", "Cogito has no `while let`")
                        .at(start.to(self.span()))
                        .help("loop with `while true` and a `match` whose other arm is `break`:\n`while true { match xs.pop!() { Some(x) => ..., None => break } }`"));
                }
                let cond = self.expr()?;
                self.skip_newlines();
                if self.at(&Tok::Assign) {
                    return Err(self.unexpected("`{` after the `while` condition").help("did you mean `==` (comparison)?").fix(self.span(), "=="));
                }
                let body = self.block()?;
                let span = start.to(body.span);
                Ok(mk(ExprKind::While { cond: Box::new(cond), body: Box::new(body) }, span))
            }
            Tok::For => {
                self.bump();
                let pat = self.pattern()?;
                if !self.at(&Tok::In) {
                    return Err(self.unexpected("`in` after the loop variable").help("write `for x in items { ... }`"));
                }
                self.bump();
                let iter = self.expr()?;
                self.skip_newlines();
                let body = self.block()?;
                let span = start.to(body.span);
                Ok(mk(ExprKind::For { pat, iter: Box::new(iter), body: Box::new(body) }, span))
            }
            Tok::Loop => {
                self.bump();
                self.skip_newlines();
                let body = self.block()?;
                let span = start.to(body.span);
                Ok(mk(ExprKind::Loop { body: Box::new(body) }, span))
            }
            Tok::Return => {
                self.bump();
                let value = if self.can_start_expr() { Some(Box::new(self.expr()?)) } else { None };
                let span = match &value {
                    Some(v) => start.to(v.span),
                    None => start,
                };
                Ok(mk(ExprKind::Return(value), span))
            }
            Tok::Break => {
                self.bump();
                let value = if self.can_start_expr() { Some(Box::new(self.expr()?)) } else { None };
                let span = match &value {
                    Some(v) => start.to(v.span),
                    None => start,
                };
                Ok(mk(ExprKind::Break(value), span))
            }
            Tok::Continue => {
                self.bump();
                Ok(mk(ExprKind::Continue, start))
            }
            Tok::Assert => {
                // `assert` used as an expression (e.g. a match arm) evaluates to ().
                let stmt = self.stmt()?;
                let span = stmt.span;
                Ok(mk(ExprKind::Block(vec![stmt]), span))
            }
            Tok::Assign => Err(self.unexpected("an expression").help("did you mean `==` (comparison)?")),
            t => {
                let mut d = self.unexpected("an expression");
                if crate::lexer::KEYWORDS.contains(&t.text()) {
                    d = d.note(format!("`{}` is a keyword", t.text()));
                }
                // `+ b` at the start of a line, meant to continue the line above.
                let line_start = self.pos > 0 && self.toks[self.pos - 1].tok == Tok::Newline;
                if line_start && matches!(t.text(), "+" | "*" | "/" | "//" | "%" | "**" | "==" | "!=" | "<" | "<=" | ">" | ">=") {
                    d = d.help(format!(
                        "a newline ends the statement, so a line cannot start with `{}`; end the line above with the operator instead, or wrap the expression in parentheses",
                        t.text()
                    ));
                }
                Err(d)
            }
        }
    }

    /// `{"a": 1}` (a map, written as in JSON or Python): the error, with
    /// the braces turned into brackets.
    fn brace_map(&self) -> Option<Diagnostic> {
        let mut i = self.pos + 1;
        while self.toks.get(i).is_some_and(|t| t.tok == Tok::Newline) {
            i += 1;
        }
        let key = matches!(self.toks.get(i)?.tok, Tok::Str(_) | Tok::Int(_) | Tok::Float(_));
        if !key || self.toks.get(i + 1)?.tok != Tok::Colon {
            return None;
        }
        let open = self.span();
        let mut d = Diagnostic::error("E0010", "maps are written with square brackets")
            .at(open)
            .help("write a map as `[\"a\": 1, \"b\": 2]` (and an empty map as `[:]`); braces hold blocks and records");
        let mut depth = 0;
        for t in &self.toks[self.pos..] {
            match t.tok {
                Tok::LBrace => depth += 1,
                Tok::RBrace => {
                    depth -= 1;
                    if depth == 0 {
                        d = d.fix(open, "[").fix(t.span, "]");
                        break;
                    }
                }
                Tok::Eof => break,
                _ => {}
            }
        }
        Some(d)
    }

    /// An expression where a value is written (an element, an argument, an
    /// assignment): `{}` there is a map from another language.
    fn value_expr(&mut self) -> PResult<Expr> {
        let e = self.expr()?;
        if self.empty_braces(&e) {
            return Err(self.empty_braces_error(&e, None));
        }
        Ok(e)
    }

    /// Whether `e` is `{}` (an empty block, with nothing but spaces inside).
    fn empty_braces(&self, e: &Expr) -> bool {
        let t = &self.src[e.span.start as usize..e.span.end as usize];
        matches!(&e.kind, ExprKind::Block(b) if b.is_empty())
            && t.len() >= 2
            && t.starts_with('{')
            && t.ends_with('}')
            && t[1..t.len() - 1].trim().is_empty()
    }

    fn empty_braces_error(&self, e: &Expr, ty: Option<&TypeExpr>) -> Diagnostic {
        let declared = ty.map(|t| match &t.kind {
            TypeExprKind::Named(n, _) => n.rsplit('.').next().unwrap_or("").to_string(),
            _ => String::new(),
        });
        let (what, empty) = match declared.as_deref() {
            Some("Set") => ("an empty set", Some("to_set([])")),
            Some("List") => ("an empty list", Some("[]")),
            None | Some("Map") => ("an empty map", Some("[:]")),
            // (A record type, or an alias: what it should be is not known here.)
            Some(_) => ("an empty value", None),
        };
        let d = Diagnostic::error("E0010", format!("`{{}}` is an empty block, not {}", what)).at(e.span).help(
            "an empty map is `[:]` (with entries: `[\"a\": 1]`), an empty list `[]`, an empty set `to_set([])`; a record is built with its fields",
        );
        match empty {
            Some(text) => d.fix(e.span, text),
            None => d,
        }
    }

    fn brace_is_record(&self) -> bool {
        let mut i = self.pos + 1;
        while i < self.toks.len() && self.toks[i].tok == Tok::Newline {
            i += 1;
        }
        if i + 1 >= self.toks.len() {
            return false;
        }
        let keyword_field = crate::lexer::KEYWORDS.contains(&self.toks[i].tok.text()) && self.toks[i + 1].tok == Tok::Colon;
        keyword_field || matches!((&self.toks[i].tok, &self.toks[i + 1].tok), (Tok::Ident(_), Tok::Colon) | (Tok::DotDot, _))
    }

    fn record_literal(&mut self) -> PResult<Expr> {
        let open = self.expect(&Tok::LBrace, "`{`")?;
        let mut names: Vec<Name> = Vec::new();
        let mut values = Vec::new();
        let mut spread = None;
        loop {
            self.skip_newlines();
            if self.at(&Tok::RBrace) {
                break;
            }
            if self.at(&Tok::DotDot) {
                let sp = self.bump().span;
                if spread.is_some() {
                    return Err(Diagnostic::error("E0010", "a record literal can only have one `..` spread").at(sp));
                }
                spread = Some(Box::new(self.expr()?));
            } else {
                let (name, nspan) = self.lower_ident("field name")?;
                if names.contains(&name) {
                    return Err(Diagnostic::error("E0102", format!("field `{}` is given twice", name)).at(nspan));
                }
                self.expect(&Tok::Colon, "`:` after the field name")?;
                self.skip_newlines();
                let v = self.value_expr()?;
                names.push(name);
                values.push(v);
            }
            self.skip_newlines();
            if !self.eat(&Tok::Comma) {
                self.skip_newlines();
                break;
            }
        }
        let close = self.expect_closing(&Tok::RBrace, open, "`,` or `}` in record")?;
        Ok(mk(ExprKind::Record { names: names.into(), values, spread }, open.to(close)))
    }

    fn list_like(&mut self) -> PResult<Expr> {
        let open = self.expect(&Tok::LBracket, "`[`")?;
        if self.at(&Tok::RBracket) {
            let close = self.bump().span;
            return Ok(mk(ExprKind::List(vec![]), open.to(close)));
        }
        if self.at(&Tok::Colon) && self.peek_at(1) == &Tok::RBracket {
            self.bump();
            let close = self.bump().span;
            return Ok(mk(ExprKind::Map(vec![]), open.to(close)));
        }
        let first_spread = self.eat(&Tok::DotDot);
        let first = self.value_expr()?;
        if !first_spread && self.at(&Tok::Colon) {
            // map literal
            self.bump();
            let v = self.value_expr()?;
            let mut entries = vec![(first, v)];
            while self.eat(&Tok::Comma) {
                if self.at(&Tok::RBracket) {
                    break;
                }
                let k = self.value_expr()?;
                self.expect(&Tok::Colon, "`:` between key and value")?;
                let v = self.value_expr()?;
                entries.push((k, v));
            }
            let close = self.expect_closing(&Tok::RBracket, open, "`,` or `]` in map literal")?;
            return Ok(mk(ExprKind::Map(entries), open.to(close)));
        }
        if !first_spread && self.at(&Tok::For) {
            let mut clauses = Vec::new();
            loop {
                if self.at(&Tok::For) {
                    self.bump();
                    let pat = self.pattern()?;
                    self.expect(&Tok::In, "`in`")?;
                    let iter = self.expr()?;
                    clauses.push(CompClause::For(pat, iter));
                } else if self.at(&Tok::If) {
                    self.bump();
                    clauses.push(CompClause::If(self.expr()?));
                } else {
                    break;
                }
            }
            let close = self.expect_closing(&Tok::RBracket, open, "`for`, `if` or `]` in list comprehension")?;
            return Ok(mk(ExprKind::Comprehension { body: Box::new(first), clauses }, open.to(close)));
        }
        // `[for x in xs { x * x }]`: a loop, whose value is `()`.
        if let (false, ExprKind::For { pat, iter, body }) = (first_spread, &first.kind) {
            let mut d = Diagnostic::error("E0010", "a `for` loop inside `[...]` gives a list holding one `()`")
                .at(first.span)
                .help("a list comprehension puts the value first: `[x * x for x in xs]` (with an optional filter: `[x for x in xs if x > 0]`)");
            // The loop's body is one expression (not an `if` without `else`,
            // which is likely meant as a filter).
            if let ExprKind::Block(stmts) = &body.kind {
                if let [Stmt { kind: StmtKind::Expr(value), .. }] = stmts.as_slice() {
                    let filter_like = matches!(&value.kind, ExprKind::If { els: None, .. });
                    let loop_like = matches!(&value.kind, ExprKind::For { .. } | ExprKind::While { .. } | ExprKind::Loop { .. });
                    if !filter_like && !loop_like && self.at(&Tok::RBracket) {
                        let text = |sp: Span| self.src[sp.start as usize..sp.end as usize].to_string();
                        d = d.fix(first.span, format!("{} for {} in {}", text(value.span), text(pat.span), text(iter.span)));
                    }
                }
            }
            return Err(d);
        }
        let mut items = vec![ListItem { expr: first, spread: first_spread }];
        while self.eat(&Tok::Comma) {
            if self.at(&Tok::RBracket) {
                break;
            }
            let spread = self.eat(&Tok::DotDot);
            items.push(ListItem { expr: self.value_expr()?, spread });
        }
        let close = self.expect_closing(&Tok::RBracket, open, "`,` or `]` in list")?;
        Ok(mk(ExprKind::List(items), open.to(close)))
    }

    /// A match-arm body: an expression, or an assignment (`x += 1`), which
    /// is treated as a block containing that statement.
    fn arm_body(&mut self) -> PResult<Expr> {
        let e = self.expr()?;
        let op = match self.peek() {
            Tok::Assign => Some(None),
            Tok::PlusAssign => Some(Some(BinOp::Add)),
            Tok::MinusAssign => Some(Some(BinOp::Sub)),
            Tok::StarAssign => Some(Some(BinOp::Mul)),
            Tok::SlashAssign => Some(Some(BinOp::Div)),
            Tok::PercentAssign => Some(Some(BinOp::Mod)),
            Tok::SlashSlashAssign => Some(Some(BinOp::FloorDiv)),
            Tok::StarStarAssign => Some(Some(BinOp::Pow)),
            _ => None,
        };
        let Some(op) = op else { return Ok(e) };
        self.bump();
        check_place(&e)?;
        self.skip_newlines();
        let value = self.value_expr()?;
        let span = e.span.to(value.span);
        let stmt = Stmt { kind: StmtKind::Assign { target: e, op, value, ty: None }, span };
        Ok(mk(ExprKind::Block(vec![stmt]), span))
    }

    fn if_expr(&mut self) -> PResult<Expr> {
        let start = self.expect(&Tok::If, "`if`")?;
        if matches!(self.peek(), Tok::Let | Tok::Var) {
            return Err(Diagnostic::error("E0001", "Cogito has no `if let`")
                .at(start.to(self.span()))
                .help("use `match`: `match value { Some(x) => ..., _ => ... }`"));
        }
        let cond = self.expr()?;
        if !self.at(&Tok::LBrace) {
            let mut d = self.unexpected("`{` after the `if` condition");
            if let Tok::Ident(n) = self.peek() {
                if &**n == "then" {
                    d = d.help("Cogito does not use `then`; write `if cond { ... }`");
                }
            }
            if self.at(&Tok::Colon) {
                d = d.help("Cogito uses braces, not colons: `if cond { ... }`");
            }
            if self.at(&Tok::Assign) {
                d = d.help("did you mean `==` (comparison)?").fix(self.span(), "==");
            }
            return Err(d);
        }
        let then = self.block()?;
        let els = if self.peek_past_newlines() == &Tok::Else {
            self.skip_newlines();
            self.bump();
            if self.at(&Tok::If) {
                Some(Box::new(self.if_expr()?))
            } else {
                self.skip_newlines();
                if !self.at(&Tok::LBrace) {
                    return Err(self.unexpected("`{` or `if` after `else`"));
                }
                Some(Box::new(self.block()?))
            }
        } else {
            None
        };
        let span = start.to(els.as_ref().map_or(then.span, |e| e.span));
        Ok(mk(ExprKind::If { cond: Box::new(cond), then: Box::new(then), els }, span))
    }

    fn match_expr(&mut self) -> PResult<Expr> {
        let start = self.expect(&Tok::Match, "`match`")?;
        let scrutinee = self.expr()?;
        self.skip_newlines();
        let open = self.expect(&Tok::LBrace, "`{` to start the match arms")?;
        let mut arms = Vec::new();
        loop {
            while matches!(self.peek(), Tok::Newline | Tok::Comma | Tok::Semi) {
                self.bump();
            }
            if self.at(&Tok::RBrace) {
                break;
            }
            if self.at(&Tok::Eof) {
                return Err(Diagnostic::error("E0010", "unclosed `match`").at(open).label("this `{` is never closed"));
            }
            let pat = self.pattern()?;
            let guard = if self.eat(&Tok::If) { Some(self.expr()?) } else { None };
            if !self.at(&Tok::FatArrow) {
                let mut d = self.unexpected("`=>` after the pattern");
                if self.at(&Tok::Arrow) {
                    d = d.help("match arms use a fat arrow: `pattern => result`").fix(self.span(), "=>");
                } else if self.at(&Tok::Colon) {
                    d = d.help("match arms are written `pattern => result`");
                }
                return Err(d);
            }
            self.bump();
            self.skip_newlines();
            let body = self.arm_body()?;
            arms.push(Arm { pat, guard, body });
            if !matches!(self.peek(), Tok::Newline | Tok::Comma | Tok::Semi | Tok::RBrace) {
                return Err(self.unexpected("a newline or `,` after the match arm"));
            }
        }
        let close = self.bump().span;
        Ok(mk(ExprKind::Match { scrutinee: Box::new(scrutinee), arms }, start.to(close)))
    }

    // ------------------------------------------------------------ types

    /// `List<Int>`: the error, with the angle brackets made square.
    fn angle_type_args(&self) -> Diagnostic {
        let mut d = Diagnostic::error("E0010", "type arguments go in square brackets")
            .at(self.span())
            .help("write `List[Int]` or `Map[Str, List[Int]]`; `<` and `>` only compare");
        let mut depth = 0;
        let mut fixes = Vec::new();
        for t in &self.toks[self.pos..] {
            match t.tok {
                Tok::Lt => {
                    depth += 1;
                    fixes.push((t.span, "["));
                }
                Tok::Gt => {
                    depth -= 1;
                    fixes.push((t.span, "]"));
                    if depth == 0 {
                        for (sp, text) in fixes {
                            d = d.fix(sp, text);
                        }
                        break;
                    }
                }
                Tok::Upper(_) | Tok::Ident(_) | Tok::Comma | Tok::Dot | Tok::LParen | Tok::RParen => {}
                _ => break,
            }
        }
        d
    }

    pub fn type_expr(&mut self) -> PResult<TypeExpr> {
        self.enter()?;
        let r = self.type_expr_inner();
        self.depth -= 1;
        r
    }

    fn type_expr_inner(&mut self) -> PResult<TypeExpr> {
        let start = self.span();
        match self.peek().clone() {
            Tok::Upper(name) => {
                self.bump();
                let mut args = Vec::new();
                if self.at(&Tok::LBracket) {
                    let open = self.bump().span;
                    loop {
                        if self.at(&Tok::RBracket) {
                            break;
                        }
                        args.push(self.type_expr()?);
                        if !self.eat(&Tok::Comma) {
                            break;
                        }
                    }
                    self.expect_closing(&Tok::RBracket, open, "`,` or `]` in type arguments")?;
                } else if self.at(&Tok::Lt) {
                    return Err(self.angle_type_args());
                }
                Ok(TypeExpr { kind: TypeExprKind::Named(name, args), span: start.to(self.prev_span()), ty: Ty::Any })
            }
            Tok::LParen => {
                self.bump();
                if self.eat(&Tok::RParen) {
                    return Ok(TypeExpr { kind: TypeExprKind::Unit, span: start.to(self.prev_span()), ty: Ty::Unit });
                }
                let first = self.type_expr()?;
                if self.eat(&Tok::Comma) {
                    let mut items = vec![first];
                    loop {
                        if self.at(&Tok::RParen) {
                            break;
                        }
                        items.push(self.type_expr()?);
                        if !self.eat(&Tok::Comma) {
                            break;
                        }
                    }
                    self.expect_closing(&Tok::RParen, start, "`,` or `)` in tuple type")?;
                    return Ok(TypeExpr { kind: TypeExprKind::Tuple(items), span: start.to(self.prev_span()), ty: Ty::Any });
                }
                self.expect_closing(&Tok::RParen, start, "`)`")?;
                Ok(first)
            }
            Tok::LBrace => {
                self.bump();
                let mut fields = Vec::new();
                loop {
                    self.skip_newlines();
                    if self.at(&Tok::RBrace) {
                        break;
                    }
                    let (n, _) = self.lower_ident("field name")?;
                    self.expect(&Tok::Colon, "`:` after field name")?;
                    let t = self.type_expr()?;
                    fields.push((n, t));
                    self.skip_newlines();
                    if !self.eat(&Tok::Comma) {
                        self.skip_newlines();
                        break;
                    }
                }
                self.expect_closing(&Tok::RBrace, start, "`,` or `}` in record type")?;
                Ok(TypeExpr { kind: TypeExprKind::Record(fields), span: start.to(self.prev_span()), ty: Ty::Any })
            }
            Tok::Fn => {
                self.bump();
                let open = self.expect(&Tok::LParen, "`(` in function type")?;
                let mut params = Vec::new();
                loop {
                    if self.at(&Tok::RParen) {
                        break;
                    }
                    params.push(self.type_expr()?);
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                }
                self.expect_closing(&Tok::RParen, open, "`,` or `)` in function type")?;
                let ret = if self.eat(&Tok::Arrow) {
                    self.type_expr()?
                } else {
                    TypeExpr { kind: TypeExprKind::Unit, span: self.prev_span(), ty: Ty::Unit }
                };
                Ok(TypeExpr { kind: TypeExprKind::Fn(params, Box::new(ret)), span: start.to(self.prev_span()), ty: Ty::Any })
            }
            Tok::Ident(module) if self.peek_at(1) == &Tok::Dot && matches!(self.peek_at(2), Tok::Upper(_)) => {
                self.bump();
                self.bump();
                let Tok::Upper(name) = self.bump().tok else { unreachable!() };
                let qualified: Name = Rc::from(format!("{}.{}", module, name).as_str());
                let mut args = Vec::new();
                if self.at(&Tok::LBracket) {
                    let open = self.bump().span;
                    loop {
                        if self.at(&Tok::RBracket) {
                            break;
                        }
                        args.push(self.type_expr()?);
                        if !self.eat(&Tok::Comma) {
                            break;
                        }
                    }
                    self.expect_closing(&Tok::RBracket, open, "`,` or `]` in type arguments")?;
                }
                Ok(TypeExpr { kind: TypeExprKind::Named(qualified, args), span: start.to(self.prev_span()), ty: Ty::Any })
            }
            Tok::Ident(name) => {
                let mut d = Diagnostic::error("E0013", format!("type names start with an uppercase letter, found `{}`", name)).at(start);
                if let Some(h) = type_name_hint(&name) {
                    d = d.help(format!("did you mean `{}`?", h)).fix(start, h);
                } else if &*name == "number" {
                    d = d.help("use `Int` for whole numbers and `Float` for the others (an Int is accepted where a Float is expected)");
                }
                Err(d)
            }
            _ => Err(self.unexpected("a type")),
        }
    }

    // ------------------------------------------------------------ patterns

    pub fn pattern(&mut self) -> PResult<Pattern> {
        self.enter()?;
        let r = self.pattern_inner();
        self.depth -= 1;
        r
    }

    fn pattern_inner(&mut self) -> PResult<Pattern> {
        let first = self.pattern_primary()?;
        if !self.at(&Tok::Bar) {
            return Ok(first);
        }
        let mut alts = vec![first];
        while self.eat(&Tok::Bar) {
            self.skip_newlines();
            alts.push(self.pattern_primary()?);
        }
        let span = alts[0].span.to(alts.last().unwrap().span);
        Ok(Pattern { kind: PatKind::Or(alts), span })
    }

    fn literal_pattern(&mut self, neg: bool) -> PResult<Option<Lit>> {
        let lit = match self.peek().clone() {
            Tok::Int(n) => Lit::Int(if neg { -n } else { n }),
            Tok::Float(f) => Lit::Float(if neg { -f } else { f }),
            Tok::Str(parts) if !neg => {
                let mut s = String::new();
                for p in parts {
                    match p {
                        StrPart::Lit(l) => s.push_str(&l),
                        StrPart::Expr { .. } => {
                            return Err(Diagnostic::error("E0010", "string patterns cannot contain interpolation").at(self.span()))
                        }
                    }
                }
                Lit::Str(Rc::new(Text::new(s)))
            }
            Tok::True if !neg => Lit::Bool(true),
            Tok::False if !neg => Lit::Bool(false),
            _ => return Ok(None),
        };
        self.bump();
        Ok(Some(lit))
    }

    /// The rest of a constructor pattern, after its (possibly qualified) name.
    fn ctor_pattern(&mut self, name: Name, start: Span) -> PResult<Pattern> {
        self.bump();
        let mut args = Vec::new();
        let mut rest = false;
        if self.at(&Tok::LParen) {
            let open = self.bump().span;
            loop {
                if self.at(&Tok::RParen) {
                    break;
                }
                if self.at(&Tok::DotDot) {
                    self.bump();
                    rest = true;
                    break;
                }
                if matches!(self.peek(), Tok::Ident(_)) && self.peek_at(1) == &Tok::Colon {
                    let (n, _) = self.lower_ident("field name")?;
                    self.bump();
                    let p = self.pattern()?;
                    args.push((Some(n), p));
                } else {
                    args.push((None, self.pattern()?));
                }
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
            self.expect_closing(&Tok::RParen, open, "`,` or `)` in pattern")?;
        }
        Ok(Pattern { kind: PatKind::Ctor { name, args, rest, ctor: CtorRef::default(), field_idx: vec![] }, span: start.to(self.prev_span()) })
    }

    fn pattern_primary(&mut self) -> PResult<Pattern> {
        let start = self.span();
        let neg = self.at(&Tok::Minus);
        if neg {
            self.bump();
        }
        if let Some(lo) = self.literal_pattern(neg)? {
            if matches!(self.peek(), Tok::DotDot | Tok::DotDotEq) {
                let inclusive = self.bump().tok == Tok::DotDotEq;
                let neg2 = self.eat(&Tok::Minus);
                let Some(hi) = self.literal_pattern(neg2)? else {
                    return Err(self
                        .unexpected("a literal for the end of the range pattern")
                        .help("a range pattern needs both ends (`1..=9`); for an open one, use a guard: `n if n >= 1 => ...`"));
                };
                return Ok(Pattern { kind: PatKind::Range { lo, hi, inclusive }, span: start.to(self.prev_span()) });
            }
            return Ok(Pattern { kind: PatKind::Lit(lo), span: start.to(self.prev_span()) });
        }
        if neg {
            return Err(self.unexpected("a number after `-` in pattern"));
        }
        // `module.Ctor(...)`: a constructor from an imported module.
        if let (Tok::Ident(module), Tok::Dot, Tok::Upper(ctor)) = (self.peek().clone(), self.peek_at(1).clone(), self.peek_at(2).clone()) {
            self.bump();
            self.bump();
            let qualified: Name = Rc::from(format!("{}.{}", module, ctor).as_str());
            let _ = ctor;
            return self.ctor_pattern(qualified, start);
        }
        match self.peek().clone() {
            Tok::Ident(name) => {
                self.bump();
                if &*name == "_" {
                    return Ok(Pattern { kind: PatKind::Wild, span: start });
                }
                if name.ends_with('!') {
                    return Err(Diagnostic::error("E0013", "variable names cannot end with `!`").at(start));
                }
                let sub = if self.eat(&Tok::At) { Some(Box::new(self.pattern_primary()?)) } else { None };
                Ok(Pattern { kind: PatKind::Bind { name, res: VarRes::Unresolved, sub }, span: start.to(self.prev_span()) })
            }
            Tok::Upper(name) => self.ctor_pattern(name, start),
            Tok::LParen => {
                self.bump();
                if self.eat(&Tok::RParen) {
                    return Ok(Pattern { kind: PatKind::Lit(Lit::Unit), span: start.to(self.prev_span()) });
                }
                let first = self.pattern()?;
                if self.eat(&Tok::Comma) {
                    let mut items = vec![first];
                    loop {
                        if self.at(&Tok::RParen) {
                            break;
                        }
                        items.push(self.pattern()?);
                        if !self.eat(&Tok::Comma) {
                            break;
                        }
                    }
                    self.expect_closing(&Tok::RParen, start, "`,` or `)` in tuple pattern")?;
                    return Ok(Pattern { kind: PatKind::Tuple(items), span: start.to(self.prev_span()) });
                }
                self.expect_closing(&Tok::RParen, start, "`)`")?;
                Ok(first)
            }
            Tok::LBracket => {
                self.bump();
                let mut before = Vec::new();
                let mut after = Vec::new();
                let mut rest: Option<Option<Box<Pattern>>> = None;
                loop {
                    if self.at(&Tok::RBracket) {
                        break;
                    }
                    if self.at(&Tok::DotDot) {
                        let sp = self.bump().span;
                        if rest.is_some() {
                            return Err(Diagnostic::error("E0010", "only one `..` is allowed in a list pattern").at(sp));
                        }
                        if let Tok::Ident(n) = self.peek().clone() {
                            let s = self.bump().span;
                            let p = if &*n == "_" {
                                Pattern { kind: PatKind::Wild, span: s }
                            } else {
                                Pattern { kind: PatKind::Bind { name: n, res: VarRes::Unresolved, sub: None }, span: s }
                            };
                            rest = Some(Some(Box::new(p)));
                        } else {
                            rest = Some(None);
                        }
                    } else if rest.is_some() {
                        after.push(self.pattern()?);
                    } else {
                        before.push(self.pattern()?);
                    }
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                }
                self.expect_closing(&Tok::RBracket, start, "`,` or `]` in list pattern")?;
                Ok(Pattern { kind: PatKind::List { before, rest, after }, span: start.to(self.prev_span()) })
            }
            Tok::LBrace => {
                self.bump();
                let mut fields = Vec::new();
                let mut rest = false;
                loop {
                    self.skip_newlines();
                    if self.at(&Tok::RBrace) {
                        break;
                    }
                    if self.eat(&Tok::DotDot) {
                        rest = true;
                        if let Tok::Ident(name) = self.peek().clone() {
                            return Err(Diagnostic::error("E0010", format!("`..{}` cannot bind the other fields of a record", name))
                                .at(self.span())
                                .help(format!(
                                    "write `..` alone to allow other fields; to keep the whole record, bind it with `@`: `{} @ {{ x, .. }}`",
                                    name
                                )));
                        }
                        self.skip_newlines();
                        break;
                    }
                    let (n, nspan) = self.lower_ident("field name")?;
                    let p = if self.eat(&Tok::Colon) {
                        self.pattern()?
                    } else {
                        Pattern { kind: PatKind::Bind { name: n.clone(), res: VarRes::Unresolved, sub: None }, span: nspan }
                    };
                    fields.push((n, p));
                    self.skip_newlines();
                    if !self.eat(&Tok::Comma) {
                        self.skip_newlines();
                        break;
                    }
                }
                self.expect_closing(&Tok::RBrace, start, "`,` or `}` in record pattern")?;
                Ok(Pattern { kind: PatKind::Record { fields, rest }, span: start.to(self.prev_span()) })
            }
            _ => Err(self.unexpected("a pattern")),
        }
    }
}

fn check_place(e: &Expr) -> PResult<()> {
    match &e.kind {
        ExprKind::Var(_) => Ok(()),
        ExprKind::Field { target, .. } | ExprKind::Index { target, .. } => check_place(target),
        ExprKind::Tuple(_) => Err(Diagnostic::error("E0012", "a tuple cannot be assigned to")
            .at(e.span)
            .label("cannot assign to this")
            .help("assign each variable on its own line; to swap, use a temporary (`let t = a`, `a = b`, `b = t`), or bind new names with `let (x, y) = (b, a)`")),
        _ => Err(Diagnostic::error("E0012", "invalid assignment target")
            .at(e.span)
            .label("cannot assign to this")
            .help("only variables, fields and indexes can be assigned to; to compare, use `==`")),
    }
}

/// `[[fill]align][+][0][width][.precision][type]`
pub fn parse_fmt_spec(s: &str) -> Result<FmtSpec, String> {
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    let mut spec = FmtSpec { fill: ' ', ..FmtSpec::default() };
    let is_align = |c: char| matches!(c, '<' | '>' | '^');
    if chars.len() >= 2 && is_align(chars[1]) {
        spec.fill = chars[0];
        spec.align = Some(chars[1]);
        i = 2;
    } else if !chars.is_empty() && is_align(chars[0]) {
        spec.align = Some(chars[0]);
        i = 1;
    }
    if i < chars.len() && chars[i] == '+' {
        spec.plus = true;
        i += 1;
    }
    if i < chars.len() && chars[i] == '0' {
        spec.zero = true;
        i += 1;
    }
    let ws = i;
    while i < chars.len() && chars[i].is_ascii_digit() {
        i += 1;
    }
    if i > ws {
        spec.width = chars[ws..i].iter().collect::<String>().parse().map_err(|_| "width is too large")?;
        if spec.width > 1000 {
            return Err("width is too large (the maximum is 1000)".into());
        }
    }
    if i < chars.len() && chars[i] == ',' {
        spec.group = true;
        i += 1;
    }
    if i < chars.len() && chars[i] == '.' {
        i += 1;
        let ps = i;
        while i < chars.len() && chars[i].is_ascii_digit() {
            i += 1;
        }
        if i == ps {
            return Err("expected digits after `.`".into());
        }
        spec.precision = Some(chars[ps..i].iter().collect::<String>().parse().map_err(|_| "precision is too large")?);
        if spec.precision > Some(100) {
            return Err("precision is too large (the maximum is 100)".into());
        }
    }
    if i < chars.len() {
        match chars[i] {
            'x' | 'X' | 'b' | 'o' | 'e' | '%' | 'f' | 'd' | 's' => {
                spec.kind = Some(chars[i]);
                i += 1;
            }
            '{' => {
                return Err("format specs must be written literally; for a computed width use `pad_left(s, width)` or `pad_right(s, width)`".into())
            }
            c => return Err(format!("unknown format type `{}`", c)),
        }
    }
    if i != chars.len() {
        return Err("unexpected characters at the end".into());
    }
    Ok(spec)
}

/// A lowercase spelling of an uppercase name, for suggestions: `MAX_N` is
/// `max_n`, `MaxValue` is `max_value`, `HTTPCode` is `http_code`.
fn snake_case(n: &str) -> String {
    let cs: Vec<char> = n.chars().collect();
    let mut out = String::new();
    for (i, &c) in cs.iter().enumerate() {
        if c.is_uppercase() && i > 0 {
            let prev = cs[i - 1];
            let next_lower = cs.get(i + 1).is_some_and(|x| x.is_lowercase());
            if prev.is_lowercase() || prev.is_ascii_digit() || (prev.is_uppercase() && next_lower) {
                out.push('_');
            }
        }
        out.extend(c.to_lowercase());
    }
    if crate::lexer::KEYWORDS.contains(&out.as_str()) {
        out.push('_');
    }
    out
}
