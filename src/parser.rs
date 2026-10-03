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

type PResult<T> = Result<T, Diagnostic>;

pub struct Parser<'s> {
    src: &'s str,
    file: u32,
    toks: Vec<Token>,
    pos: usize,
}

pub fn parse_program(src: &str, file: u32) -> PResult<Program> {
    let toks = lex(src, file, 0, src.len())?;
    let mut p = Parser { src, file, toks, pos: 0 };
    p.program()
}

/// Parse an expression from a sub-range of the source (used for string interpolation).
pub fn parse_expr_range(src: &str, file: u32, start: usize, end: usize) -> PResult<Expr> {
    let toks = lex(src, file, start, end)?;
    let mut p = Parser { src, file, toks, pos: 0 };
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

fn type_name_hint(name: &str) -> Option<&'static str> {
    Some(match name {
        "int" | "i64" | "integer" | "i32" | "usize" => "Int",
        "float" | "f64" | "double" | "number" => "Float",
        "str" | "string" | "String" => "Str",
        "bool" | "boolean" => "Bool",
        "list" | "array" | "vec" | "Vec" | "Array" => "List",
        "map" | "dict" | "hashmap" | "HashMap" | "Dict" => "Map",
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
        }
        d
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
            Err(self.unexpected(what).note(format!("the `{}` that needs closing is at line {}, column {}", open, line, col)))
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
                d = match self.peek() {
                    Tok::Assign => d.help("only variables, fields and indexes can be assigned to"),
                    Tok::Ident(_) | Tok::Upper(_) if matches!(self.toks[self.pos - 1].tok, Tok::Ident(_) | Tok::Upper(_)) => {
                        d.help("two names in a row: is an operator or a comma missing?")
                    }
                    _ => d.help("put each statement on its own line, or separate statements with `;`"),
                };
                Err(d)
            }
        }
    }

    fn lower_ident(&mut self, what: &str) -> PResult<(Name, Span)> {
        match self.peek().clone() {
            Tok::Ident(n) => {
                let sp = self.bump().span;
                Ok((n, sp))
            }
            Tok::Upper(n) => {
                let sp = self.span();
                let lower: String = {
                    let mut c = n.chars();
                    match c.next() {
                        Some(f) => f.to_lowercase().collect::<String>() + c.as_str(),
                        None => String::new(),
                    }
                };
                Err(Diagnostic::error("E0013", format!("{} `{}` must start with a lowercase letter", what, n))
                    .at(sp)
                    .label("uppercase names are reserved for types and constructors")
                    .help(format!("rename it to `{}`", lower)))
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

    fn program(&mut self) -> PResult<Program> {
        let mut items = Vec::new();
        loop {
            self.skip_terminators();
            if self.at(&Tok::Eof) {
                break;
            }
            items.push(self.item()?);
            self.expect_terminator()?;
        }
        Ok(Program { items, file: self.file, num_slots: 0 })
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
                self.expr()
            }
            Tok::Assign => Err(self.unexpected("function body").help(format!("single-expression functions use `=>`: `fn {}(x) => x * 2`", name))),
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
            // A parameter may be a destructuring pattern: `fn((k, v)) => ...`.
            let (name, span, pat) = if matches!(self.peek(), Tok::LParen | Tok::LBracket | Tok::LBrace) {
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
        let span = start.to(self.prev_span());
        Ok(TypeDecl { name, name_span, span, params, body, id: 0, slot: 0 })
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
                if let Tok::Upper(n) = self.peek().clone() {
                    if !matches!(self.peek_at(1), Tok::LParen) {
                        let mut c = n.chars();
                        let lower = c.next().map(|f| f.to_lowercase().collect::<String>() + c.as_str()).unwrap_or_default();
                        return Err(Diagnostic::error("E0013", format!("variable `{}` must start with a lowercase letter", n))
                            .at(self.span())
                            .label("uppercase names are reserved for types and constructors")
                            .help(format!("rename it to `{}`", lower)));
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
                let span = start.to(value.span);
                Ok(Stmt { kind: StmtKind::Let { pat, ty, value, mutable }, span })
            }
            Tok::Fn if !matches!(self.peek_at(1), Tok::LParen) => {
                let def = self.fn_decl()?;
                let span = def.span;
                Ok(Stmt { kind: StmtKind::Fn { def: Rc::new(def), res: VarRes::Unresolved }, span })
            }
            Tok::Type | Tok::Test | Tok::Property | Tok::Import => {
                Err(Diagnostic::error("E0116", format!("`{}` declarations are only allowed at the top level of a file", self.peek().text()))
                    .at(self.span()))
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
                    _ => None,
                };
                if let Some(op) = op {
                    self.bump();
                    check_place(&e)?;
                    self.skip_newlines();
                    let value = self.expr()?;
                    let span = e.span.to(value.span);
                    return Ok(Stmt { kind: StmtKind::Assign { target: e, op, value, ty: None }, span });
                }
                let span = e.span;
                Ok(Stmt { kind: StmtKind::Expr(e), span })
            }
        }
    }

    fn block(&mut self) -> PResult<Expr> {
        let open = self.expect(&Tok::LBrace, "`{`")?;
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
            self.expect_terminator()?;
        }
        let close = self.bump().span;
        Ok(mk(ExprKind::Block(stmts), open.to(close)))
    }

    // ------------------------------------------------------------ expressions

    pub fn expr(&mut self) -> PResult<Expr> {
        self.or_expr()
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
            let e = self.not_expr()?;
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
        let Some((op, n)) = self.cmp_op() else { return Ok(lhs) };
        let op_span = self.span();
        for _ in 0..n {
            self.bump();
        }
        self.skip_newlines();
        let rhs = self.pipe_expr()?;
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
        let end = if self.can_start_expr() && !self.at(&Tok::LBrace) {
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
            let e = self.unary_expr()?;
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
        let open = self.expect(&Tok::LParen, "`(`")?;
        let mut args = Vec::new();
        loop {
            if self.at(&Tok::RParen) {
                break;
            }
            let name = if matches!(self.peek(), Tok::Ident(_)) && self.peek_at(1) == &Tok::Colon {
                let (n, _) = self.lower_ident("argument name")?;
                self.bump();
                Some(n)
            } else {
                None
            };
            let value = self.expr()?;
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
                            if args.is_empty() || args[0].name.is_some() {
                                return Err(Diagnostic::error("E0111", format!("`{}` mutates its first argument, so it needs one", v.name))
                                    .at(span)
                                    .help(format!("call it as `variable.{}(...)`", v.name)));
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
                    // `xs[..n]` and `xs[..=n]` slice from the start.
                    let index = if matches!(self.peek(), Tok::DotDot | Tok::DotDotEq) {
                        let inclusive = self.bump().tok == Tok::DotDotEq;
                        let end = self.add_expr()?;
                        let span = open.to(end.span);
                        mk(ExprKind::Range { start: Box::new(mk(ExprKind::Int(0), open)), end: Some(Box::new(end)), inclusive }, span)
                    } else {
                        self.expr()?
                    };
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
                                return Err(Diagnostic::error("E0111", format!("mutating function `{}` must be called", name))
                                    .at(name_span)
                                    .help(format!("write `.{}()`", name)));
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
                    self.expr()?
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
                let cond = self.expr()?;
                self.skip_newlines();
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
                Err(d)
            }
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
        matches!((&self.toks[i].tok, &self.toks[i + 1].tok), (Tok::Ident(_), Tok::Colon) | (Tok::DotDot, _))
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
                let v = self.expr()?;
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
        let first = self.expr()?;
        if !first_spread && self.at(&Tok::Colon) {
            // map literal
            self.bump();
            let v = self.expr()?;
            let mut entries = vec![(first, v)];
            while self.eat(&Tok::Comma) {
                if self.at(&Tok::RBracket) {
                    break;
                }
                let k = self.expr()?;
                self.expect(&Tok::Colon, "`:` between key and value")?;
                let v = self.expr()?;
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
        let mut items = vec![ListItem { expr: first, spread: first_spread }];
        while self.eat(&Tok::Comma) {
            if self.at(&Tok::RBracket) {
                break;
            }
            let spread = self.eat(&Tok::DotDot);
            items.push(ListItem { expr: self.expr()?, spread });
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
            _ => None,
        };
        let Some(op) = op else { return Ok(e) };
        self.bump();
        check_place(&e)?;
        self.skip_newlines();
        let value = self.expr()?;
        let span = e.span.to(value.span);
        let stmt = Stmt { kind: StmtKind::Assign { target: e, op, value, ty: None }, span };
        Ok(mk(ExprKind::Block(vec![stmt]), span))
    }

    fn if_expr(&mut self) -> PResult<Expr> {
        let start = self.expect(&Tok::If, "`if`")?;
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
                    d = d.help("match arms use a fat arrow: `pattern => result`");
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

    pub fn type_expr(&mut self) -> PResult<TypeExpr> {
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
                    d = d.help(format!("did you mean `{}`?", h));
                }
                Err(d)
            }
            _ => Err(self.unexpected("a type")),
        }
    }

    // ------------------------------------------------------------ patterns

    pub fn pattern(&mut self) -> PResult<Pattern> {
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
                    return Err(self.unexpected("a literal for the end of the range pattern"));
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
    }
    if i < chars.len() {
        match chars[i] {
            'x' | 'X' | 'b' | 'o' | 'e' | '%' => {
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
