//! `cogito lsp`: a language server, so that editors show Cogito's errors
//! and warnings as you type, format on request, show documentation on hover,
//! list a file's declarations, and jump to definitions.
//!
//! It speaks the Language Server Protocol (JSON-RPC over standard input and
//! output) and supports: diagnostics on open and change, formatting
//! (`cogito fmt`), hover for built-ins, keywords and the file's own
//! functions and types, completion, document symbols, and go to definition.

use crate::ast::{Item, Namespace, PatKind, Program, StmtKind, TypeBody};
use crate::diagnostic::{Diagnostic, Severity};
use crate::interp::Interp;
use crate::json::Json;
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

/// Run the server on standard input and output until `exit`.
pub fn run_stdio() -> i32 {
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let stdout = std::io::stdout();
    let mut server = Server { docs: HashMap::new(), parsed: HashMap::new(), shutdown: false };
    loop {
        let Some(msg) = read_message(&mut input) else { return if server.shutdown { 0 } else { 1 } };
        let Ok(msg) = Json::parse(&msg) else { continue };
        let method = msg.get("method").as_str().unwrap_or("").to_string();
        let id = msg.get("id").clone();
        if method == "exit" {
            return if server.shutdown { 0 } else { 1 };
        }
        // Keep serving even if one request hits a bug.
        let replies = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| server.handle(&method, msg.get("params"), &id)))
            .unwrap_or_else(|_| if id.is_null() { vec![] } else { vec![error_reply(&id, -32603, "internal error in the Cogito language server")] });
        let mut out = stdout.lock();
        for r in replies {
            let body = r.to_string();
            let _ = write!(out, "Content-Length: {}\r\n\r\n{}", body.len(), body);
        }
        let _ = out.flush();
    }
}

fn read_message(input: &mut impl BufRead) -> Option<String> {
    let mut len = None;
    loop {
        let mut line = String::new();
        if input.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(v) = line.strip_prefix("Content-Length:") {
            len = v.trim().parse::<usize>().ok();
        }
    }
    let mut buf = vec![0; len?];
    input.read_exact(&mut buf).ok()?;
    String::from_utf8(buf).ok()
}

fn reply(id: &Json, result: Json) -> Json {
    Json::obj(vec![("jsonrpc", Json::str("2.0")), ("id", id.clone()), ("result", result)])
}

fn error_reply(id: &Json, code: i32, message: &str) -> Json {
    Json::obj(vec![
        ("jsonrpc", Json::str("2.0")),
        ("id", id.clone()),
        ("error", Json::obj(vec![("code", Json::num(code)), ("message", Json::str(message))])),
    ])
}

fn notification(method: &str, params: Json) -> Json {
    Json::obj(vec![("jsonrpc", Json::str("2.0")), ("method", Json::str(method)), ("params", params)])
}

struct Server {
    docs: HashMap<String, String>,
    /// For each document, the last version of its text that parsed: while
    /// a line is half typed, completion still knows the declarations.
    parsed: HashMap<String, String>,
    shutdown: bool,
}

impl Server {
    fn handle(&mut self, method: &str, params: &Json, id: &Json) -> Vec<Json> {
        let uri = params.get("textDocument").get("uri").as_str().unwrap_or("").to_string();
        match method {
            "initialize" => vec![reply(
                id,
                Json::obj(vec![
                    (
                        "capabilities",
                        Json::obj(vec![
                            ("textDocumentSync", Json::num(1)),
                            ("documentFormattingProvider", Json::Bool(true)),
                            ("hoverProvider", Json::Bool(true)),
                            ("documentSymbolProvider", Json::Bool(true)),
                            ("definitionProvider", Json::Bool(true)),
                            ("referencesProvider", Json::Bool(true)),
                            ("renameProvider", Json::obj(vec![("prepareProvider", Json::Bool(true))])),
                            ("completionProvider", Json::obj(vec![("triggerCharacters", Json::Arr(vec![Json::str(".")]))])),
                            (
                                "codeActionProvider",
                                Json::obj(vec![("codeActionKinds", Json::Arr(vec![Json::str("quickfix"), Json::str("source.fixAll")]))]),
                            ),
                        ]),
                    ),
                    ("serverInfo", Json::obj(vec![("name", Json::str("cogito")), ("version", Json::str(crate::VERSION))])),
                ]),
            )],
            "shutdown" => {
                self.shutdown = true;
                vec![reply(id, Json::Null)]
            }
            "textDocument/didOpen" => {
                let text = params.get("textDocument").get("text").as_str().unwrap_or("").to_string();
                self.docs.insert(uri.clone(), text);
                vec![self.diagnostics(&uri)]
            }
            "textDocument/didChange" => {
                // Full synchronization: the last change holds the whole text.
                if let Json::Arr(changes) = params.get("contentChanges") {
                    if let Some(text) = changes.last().and_then(|c| c.get("text").as_str()) {
                        self.docs.insert(uri.clone(), text.to_string());
                    }
                }
                vec![self.diagnostics(&uri)]
            }
            "textDocument/didSave" => vec![self.diagnostics(&uri)],
            "textDocument/didClose" => {
                self.docs.remove(&uri);
                vec![notification("textDocument/publishDiagnostics", Json::obj(vec![("uri", Json::str(uri)), ("diagnostics", Json::Arr(vec![]))]))]
            }
            "textDocument/formatting" => vec![reply(id, self.formatting(&uri))],
            "textDocument/hover" => vec![reply(id, self.hover(&uri, params.get("position")))],
            "textDocument/documentSymbol" => vec![reply(id, self.symbols(&uri))],
            "textDocument/definition" => vec![reply(id, self.definition(&uri, params.get("position")))],
            "textDocument/completion" => vec![reply(id, self.completion(&uri, params.get("position")))],
            "textDocument/codeAction" => vec![reply(id, self.code_actions(&uri, params.get("range")))],
            "textDocument/references" => {
                let with_decl = !matches!(params.get("context").get("includeDeclaration"), Json::Bool(false));
                vec![reply(id, self.references(&uri, params.get("position"), with_decl))]
            }
            "textDocument/prepareRename" => vec![reply(id, self.prepare_rename(&uri, params.get("position")))],
            "textDocument/rename" => {
                let new_name = params.get("newName").as_str().unwrap_or("").to_string();
                match self.rename(&uri, params.get("position"), &new_name) {
                    Ok(edit) => vec![reply(id, edit)],
                    Err(msg) => vec![error_reply(id, -32803, &msg)],
                }
            }
            _ if !id.is_null() => vec![error_reply(id, -32601, &format!("method not supported: {}", method))],
            _ => vec![],
        }
    }

    fn text(&self, uri: &str) -> &str {
        self.docs.get(uri).map_or("", |s| s.as_str())
    }

    /// The document's declarations: from its current text if that parses,
    /// else without the line being typed at `offset`, else from the last
    /// version that parsed.
    fn program(&mut self, uri: &str, offset: usize) -> Option<(String, Program)> {
        let text = self.text(uri).to_string();
        if let Ok(p) = crate::parser::parse_program(&text, 0) {
            self.parsed.insert(uri.to_string(), text.clone());
            return Some((text, p));
        }
        let line_start = text[..offset].rfind('\n').map_or(0, |i| i + 1);
        let line_end = text[offset..].find('\n').map_or(text.len(), |i| offset + i);
        let without = format!("{}{}", &text[..line_start], &text[line_end..]);
        if let Ok(p) = crate::parser::parse_program(&without, 0) {
            return Some((without, p));
        }
        let old = self.parsed.get(uri)?.clone();
        let p = crate::parser::parse_program(&old, 0).ok()?;
        Some((old, p))
    }

    fn completion(&mut self, uri: &str, pos: &Json) -> Json {
        let text = self.text(uri).to_string();
        let lines = LineIndex::new(&text);
        let Some(offset) = lines.offset(pos) else { return Json::Arr(vec![]) };
        let before = &text[..offset];
        let start = before.char_indices().rev().take_while(|(_, c)| c.is_alphanumeric() || *c == '_' || *c == '!').last().map_or(offset, |(i, _)| i);
        let prefix = &before[start..];
        let receiver = before[..start].strip_suffix('.').map(|r| {
            let rs = r.char_indices().rev().take_while(|(_, c)| c.is_alphanumeric() || *c == '_').last().map_or(r.len(), |(i, _)| i);
            &r[rs..]
        });
        let mut items = Completions::default();
        let prog = self.program(uri, offset);
        // `geometry.` on an imported module: its declarations.
        if let (Some(m), Some((_, p))) = (receiver, &prog) {
            if let Some(path) = import_path(p, m) {
                let file = uri_to_path(uri).parent().map(|d| d.join(&path)).unwrap_or_else(|| PathBuf::from(&path));
                if let Ok(src) = std::fs::read_to_string(&file) {
                    if let Ok(mp) = crate::parser::parse_program(&src, 0) {
                        items.declarations(&src, &mp, false);
                    }
                }
                return items.json(prefix);
            }
        }
        if let Some((t, p)) = &prog {
            items.declarations(t, p, receiver.is_some());
        }
        for b in crate::builtins::BUILTINS.iter() {
            let mut doc = b.doc.lines();
            let sig = doc.next().unwrap_or("").to_string();
            items.add(b.name, KIND_FUNCTION, sig, doc.collect::<Vec<_>>().join("\n"));
        }
        if receiver.is_none() {
            for k in crate::lexer::KEYWORDS {
                items.add(k, KIND_KEYWORD, String::new(), keyword_doc(k).unwrap_or("").to_string());
            }
            for c in ["pi", "tau", "e", "inf", "max_int", "min_int"] {
                items.add(c, KIND_CONSTANT, String::new(), String::new());
            }
            for t in ["Int", "Float", "Str", "Bool", "Unit", "List", "Map", "Set", "Option", "Result", "Range", "Any", "Some", "None", "Ok", "Err"] {
                items.add(t, KIND_STRUCT, String::new(), String::new());
            }
            // Any other name in the file (parameters, local variables).
            for w in words(&text) {
                items.add(&w, KIND_VARIABLE, String::new(), String::new());
            }
        }
        items.json(prefix)
    }

    /// Check the document (syntax, names, types).
    fn check(&self, uri: &str) -> (Interp, Vec<Diagnostic>) {
        let text = self.text(uri);
        let path = uri_to_path(uri);
        let dir = path.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
        let mut it = Interp::new();
        let mut ns = Namespace::default();
        let name = path.display().to_string();
        let diags = match crate::load_source(&mut it, &name, text, &dir, &mut ns, false) {
            Ok((_, warnings)) => warnings,
            Err(diags) => diags,
        };
        (it, diags)
    }

    /// The fixes for the problems in `range`, as quick fixes, and all the
    /// fixes in the document as one "fix all" action.
    fn code_actions(&self, uri: &str, range: &Json) -> Json {
        let text = self.text(uri);
        let (it, diags) = self.check(uri);
        let lines = LineIndex::new(text);
        let from = lines.offset(range.get("start")).unwrap_or(0);
        let to = lines.offset(range.get("end")).unwrap_or(text.len()).max(from);
        let edit = |fixes: Vec<&crate::diagnostic::Fix>| {
            let edits = fixes
                .iter()
                .map(|x| Json::obj(vec![("range", lines.range(x.span.start as usize, x.span.end as usize)), ("newText", Json::str(x.text.clone()))]));
            Json::obj(vec![("changes", Json::Obj(vec![(uri.to_string(), Json::Arr(edits.collect()))]))])
        };
        let in_doc = |d: &&Diagnostic| !d.fixes.is_empty() && d.fixes.iter().all(|x| x.span.file == 0);
        let mut actions = Vec::new();
        for d in diags.iter().filter(in_doc) {
            let Some(sp) = d.span.filter(|s| s.file == 0) else { continue };
            if (sp.end as usize) < from || (sp.start as usize) > to {
                continue;
            }
            actions.push(Json::obj(vec![
                ("title", Json::str(format!("Fix: {}", d.describe_fixes(&it.ctx.sm)))),
                ("kind", Json::str("quickfix")),
                ("diagnostics", Json::Arr(vec![diagnostic_json(d, &it, &lines)])),
                ("isPreferred", Json::Bool(true)),
                ("edit", edit(d.fixes.iter().collect())),
            ]));
        }
        let fixable: Vec<Diagnostic> = diags.iter().filter(in_doc).cloned().collect();
        let (_, used) = crate::diagnostic::apply_fixes(text, 0, &fixable);
        if !used.is_empty() {
            actions.push(Json::obj(vec![
                ("title", Json::str(format!("Fix all {} automatically fixable problem{}", used.len(), if used.len() == 1 { "" } else { "s" }))),
                ("kind", Json::str("source.fixAll")),
                ("edit", edit(used.iter().flat_map(|d| d.fixes.iter()).collect())),
            ]));
        }
        Json::Arr(actions)
    }

    /// Check the document and publish what was found.
    fn diagnostics(&self, uri: &str) -> Json {
        let text = self.text(uri);
        let (it, diags) = self.check(uri);
        let lines = LineIndex::new(text);
        let items: Vec<Json> = diags.iter().map(|d| diagnostic_json(d, &it, &lines)).collect();
        notification("textDocument/publishDiagnostics", Json::obj(vec![("uri", Json::str(uri)), ("diagnostics", Json::Arr(items))]))
    }

    fn formatting(&self, uri: &str) -> Json {
        let text = self.text(uri);
        match crate::format::format_source(text) {
            Ok(out) if out != text => {
                let lines = LineIndex::new(text);
                Json::Arr(vec![Json::obj(vec![("range", lines.range(0, text.len())), ("newText", Json::str(out))])])
            }
            _ => Json::Arr(vec![]),
        }
    }

    fn hover(&self, uri: &str, pos: &Json) -> Json {
        let text = self.text(uri);
        let lines = LineIndex::new(text);
        let Some(offset) = lines.offset(pos) else { return Json::Null };
        let Some((word, start, end)) = word_at(text, offset) else { return Json::Null };
        let markdown = if let Some(d) = self.declaration_doc(text, &word) {
            d
        } else if let Some(t) = self.name_type(uri, start) {
            format!("```cogito\n{}: {}\n```", word, t)
        } else if let Some(b) = crate::builtins::BUILTINS.iter().find(|b| b.name == word) {
            let mut doc = b.doc.lines();
            let sig = doc.next().unwrap_or("");
            format!("```cogito\n{}\n```\n{}", sig, doc.collect::<Vec<_>>().join("\n"))
        } else if let Some(k) = keyword_doc(&word) {
            k.to_string()
        } else {
            return Json::Null;
        };
        Json::obj(vec![
            ("contents", Json::obj(vec![("kind", Json::str("markdown")), ("value", Json::str(markdown))])),
            ("range", lines.range(start, end)),
        ])
    }

    /// The type the checker knows for the variable at `offset`, if any.
    fn name_type(&self, uri: &str, offset: usize) -> Option<String> {
        let text = self.text(uri);
        let path = uri_to_path(uri);
        let dir = path.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
        let mut it = Interp::new();
        let mut ns = Namespace::default();
        let file = it.ctx.sm.add(path.display().to_string(), text);
        let mut prog = crate::parser::parse_program(text, file).ok()?;
        let diags = crate::resolver::resolve_program(&mut it.ctx, &mut prog, &mut ns, &dir, false);
        if diags.iter().any(|d| d.is_error()) {
            return None;
        }
        let types = crate::typecheck::name_types(&it.ctx, &prog);
        let (_, t) = types.iter().find(|(sp, _)| sp.file == file && sp.start as usize == offset)?;
        (!t.is_any()).then(|| t.to_string())
    }

    /// The signature and comment of a top-level function or type declared
    /// in this file.
    fn declaration_doc(&self, text: &str, word: &str) -> Option<String> {
        let prog = crate::parser::parse_program(text, 0).ok()?;
        crate::docgen::declarations(text, &prog).iter().find(|d| d.name == word).map(crate::docgen::markdown)
    }

    fn symbols(&self, uri: &str) -> Json {
        let text = self.text(uri);
        let Ok(prog) = crate::parser::parse_program(text, 0) else { return Json::Arr(vec![]) };
        let lines = LineIndex::new(text);
        let sym = |name: String, kind: u32, span: crate::span::Span, sel: crate::span::Span| {
            Json::obj(vec![
                ("name", Json::str(name)),
                ("kind", Json::num(kind)),
                ("range", lines.range(span.start as usize, span.end as usize)),
                ("selectionRange", lines.range(sel.start as usize, sel.end as usize)),
            ])
        };
        let mut out = Vec::new();
        for item in &prog.items {
            match item {
                // LSP symbol kinds: 12 function, 23 struct, 10 enum, 13 variable, 6 method.
                Item::Fn(def) => out.push(sym(def.display_name().to_string(), 12, def.span, def.name_span)),
                Item::Type(td) => {
                    let kind = match &td.body {
                        TypeBody::Record(..) => 23,
                        _ => 10,
                    };
                    out.push(sym(td.name.to_string(), kind, td.span, td.name_span));
                }
                Item::Test(t) => out.push(sym(format!("test \"{}\"", t.name), 6, t.span, t.span)),
                Item::Property(p) => out.push(sym(format!("property \"{}\"", p.name), 6, p.span, p.span)),
                Item::Stmt(s) => {
                    if let StmtKind::Let { pat, .. } = &s.kind {
                        if let PatKind::Bind { name, .. } = &pat.kind {
                            out.push(sym(name.to_string(), 13, s.span, pat.span));
                        }
                    }
                }
                Item::Import(_) => {}
            }
        }
        Json::Arr(out)
    }

    /// The occurrences of every variable and function in the document (from
    /// its resolved program, even if it has errors elsewhere).
    fn index(&self, uri: &str) -> Option<Vec<crate::symbols::Occurrence>> {
        index_of(self.text(uri), &uri_to_path(uri))
    }

    /// The occurrences of the name at `pos` (all of the same symbol).
    fn same_symbol(&self, uri: &str, pos: &Json) -> Option<Vec<crate::symbols::Occurrence>> {
        let offset = LineIndex::new(self.text(uri)).offset(pos)?;
        let index = self.index(uri)?;
        let here = index.iter().find(|o| o.span.start as usize <= offset && offset <= o.span.end as usize)?.sym;
        Some(index.into_iter().filter(|o| o.sym == here).collect())
    }

    fn references(&self, uri: &str, pos: &Json, with_decl: bool) -> Json {
        let lines = LineIndex::new(self.text(uri));
        let Some(occs) = self.same_symbol(uri, pos) else { return Json::Null };
        let locations = occs
            .iter()
            .filter(|o| with_decl || !o.decl)
            .map(|o| Json::obj(vec![("uri", Json::str(uri)), ("range", lines.range(o.span.start as usize, o.span.end as usize))]))
            .collect();
        Json::Arr(locations)
    }

    /// The name at `pos`, if it can be renamed: a variable or function
    /// declared in this document.
    fn prepare_rename(&self, uri: &str, pos: &Json) -> Json {
        let text = self.text(uri);
        let lines = LineIndex::new(text);
        let Some(offset) = lines.offset(pos) else { return Json::Null };
        let Some(occs) = self.same_symbol(uri, pos) else { return Json::Null };
        if !occs.iter().any(|o| o.decl) {
            return Json::Null;
        }
        match occs.iter().find(|o| o.span.start as usize <= offset && offset <= o.span.end as usize) {
            Some(o) => Json::obj(vec![
                ("range", lines.range(o.span.start as usize, o.span.end as usize)),
                ("placeholder", Json::str(&text[o.span.start as usize..o.span.end as usize])),
            ]),
            None => Json::Null,
        }
    }

    /// Rename the variable or function at `pos` everywhere in the document,
    /// unless the new name would change what any name refers to.
    fn rename(&self, uri: &str, pos: &Json, new_name: &str) -> Result<Json, String> {
        let text = self.text(uri);
        let occs = self
            .same_symbol(uri, pos)
            .filter(|o| o.iter().any(|o| o.decl))
            .ok_or("only a variable or function declared in this file can be renamed")?;
        let old = &text[occs[0].span.start as usize..occs[0].span.end as usize];
        let base = new_name.strip_suffix('!').unwrap_or(new_name);
        let valid = base.starts_with(|c: char| c.is_lowercase() || c == '_')
            && base.chars().all(|c| c.is_alphanumeric() || c == '_')
            && new_name.ends_with('!') == old.ends_with('!')
            && !crate::lexer::KEYWORDS.contains(&new_name);
        if !valid {
            let bang = if old.ends_with('!') { ", ending in `!`" } else { ", without `!`" };
            return Err(format!("`{}` is not a valid name here: it must start with a lowercase letter or `_`{}", new_name, bang));
        }
        let edits: Vec<(usize, usize, String)> = occs
            .iter()
            .map(|o| {
                let replacement = match &o.field {
                    Some(f) => format!("{}: {}", f, new_name),
                    None => new_name.to_string(),
                };
                (o.span.start as usize, o.span.end as usize, replacement)
            })
            .collect();
        // Every other name must keep referring to what it did: compare which
        // occurrences share a symbol, before and after.
        let mut renamed = text.to_string();
        for (start, end, rep) in edits.iter().rev() {
            renamed.replace_range(*start..*end, rep);
        }
        let shift = |offset: usize| -> usize {
            let mut at = offset as isize;
            for (start, end, rep) in &edits {
                if *end <= offset {
                    at += rep.len() as isize - (end - start) as isize;
                }
            }
            at as usize
        };
        let groups = |occs: &[crate::symbols::Occurrence], map: &dyn Fn(usize) -> usize| {
            let mut by_sym: HashMap<crate::symbols::Sym, Vec<usize>> = HashMap::new();
            for o in occs {
                by_sym.entry(o.sym).or_default().push(map(o.span.start as usize));
            }
            let mut groups: Vec<Vec<usize>> = by_sym.into_values().collect();
            groups.sort();
            groups
        };
        let path = uri_to_path(uri);
        let before = index_of(text, &path).unwrap_or_default();
        let after = index_of(&renamed, &path).ok_or("the renamed program does not parse")?;
        // (A renamed shorthand field `{ x }` becomes `{ x: y }`: the name moves.)
        let moved = |start: usize| {
            let field = occs.iter().find(|o| o.span.start as usize == start).and_then(|o| o.field.as_ref());
            shift(start) + field.map_or(0, |f| f.len() + 2)
        };
        let before_groups = groups(&before, &moved);
        let after_groups = groups(&after, &|start| start);
        if before_groups != after_groups {
            return Err(format!("renaming `{}` to `{}` would change what other names refer to", old, new_name));
        }
        let lines = LineIndex::new(text);
        let changes: Vec<Json> =
            edits.iter().map(|(start, end, rep)| Json::obj(vec![("range", lines.range(*start, *end)), ("newText", Json::str(rep))])).collect();
        Ok(Json::obj(vec![("changes", Json::obj(vec![(uri, Json::Arr(changes))]))]))
    }

    fn definition(&self, uri: &str, pos: &Json) -> Json {
        let text = self.text(uri);
        let lines = LineIndex::new(text);
        let Some(offset) = lines.offset(pos) else { return Json::Null };
        // A variable or function: where it is declared in this document.
        if let Some(decl) = self.same_symbol(uri, pos).and_then(|occs| occs.into_iter().find(|o| o.decl)) {
            return Json::obj(vec![("uri", Json::str(uri)), ("range", lines.range(decl.span.start as usize, decl.span.end as usize))]);
        }
        let Some((word, _, _)) = word_at(text, offset) else { return Json::Null };
        let Ok(prog) = crate::parser::parse_program(text, 0) else { return Json::Null };
        match find_declaration(&prog, &word) {
            Some(span) => Json::obj(vec![("uri", Json::str(uri)), ("range", lines.range(span.start as usize, span.end as usize))]),
            None => Json::Null,
        }
    }
}

const KIND_FUNCTION: u64 = 3;
const KIND_FIELD: u64 = 5;
const KIND_VARIABLE: u64 = 6;
const KIND_MODULE: u64 = 9;
const KIND_KEYWORD: u64 = 14;
const KIND_ENUM_MEMBER: u64 = 20;
const KIND_CONSTANT: u64 = 21;
const KIND_STRUCT: u64 = 22;

/// Completion candidates: the first entry for a name wins.
#[derive(Default)]
struct Completions {
    items: Vec<(String, u64, String, String)>,
    seen: std::collections::HashSet<String>,
}

impl Completions {
    fn add(&mut self, name: &str, kind: u64, detail: String, doc: String) {
        if self.seen.insert(name.to_string()) {
            self.items.push((name.to_string(), kind, detail, doc));
        }
    }

    /// The top-level declarations of a program (after `x.`, only what
    /// method syntax can call or read: functions and fields).
    fn declarations(&mut self, text: &str, prog: &Program, after_dot: bool) {
        let docs = crate::docgen::declarations(text, prog);
        let doc_of = |name: &str| docs.iter().find(|d| d.name == name).map(|d| (d.signature.clone(), d.comment.clone())).unwrap_or_default();
        for item in &prog.items {
            match item {
                Item::Fn(def) => {
                    let name = def.display_name();
                    let (sig, comment) = doc_of(&name);
                    self.add(&name, KIND_FUNCTION, sig, comment);
                }
                Item::Type(td) => {
                    if let TypeBody::Record(fields) = &td.body {
                        for f in fields {
                            if let Some(n) = &f.name {
                                self.add(n, KIND_FIELD, format!("{}.{}", td.name, n), String::new());
                            }
                        }
                    }
                    if after_dot {
                        continue;
                    }
                    let (sig, comment) = doc_of(&td.name);
                    self.add(&td.name, KIND_STRUCT, sig, comment);
                    if let TypeBody::Enum(vs) = &td.body {
                        for v in vs {
                            self.add(&v.name, KIND_ENUM_MEMBER, format!("variant of {}", td.name), String::new());
                        }
                    }
                }
                Item::Import(imp) if !after_dot => self.add(&import_alias(imp), KIND_MODULE, imp.path.clone(), String::new()),
                Item::Stmt(s) if !after_dot => {
                    if let StmtKind::Let { pat, .. } = &s.kind {
                        if let PatKind::Bind { name, .. } = &pat.kind {
                            self.add(name, KIND_VARIABLE, String::new(), String::new());
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn json(self, prefix: &str) -> Json {
        Json::Arr(
            self.items
                .into_iter()
                .filter(|(n, ..)| n.starts_with(prefix) && n != prefix)
                .map(|(n, kind, detail, doc)| {
                    let mut fields = vec![("label", Json::str(n)), ("kind", Json::num(kind as f64))];
                    if !detail.is_empty() {
                        fields.push(("detail", Json::str(detail)));
                    }
                    if !doc.is_empty() {
                        fields.push(("documentation", Json::obj(vec![("kind", Json::str("markdown")), ("value", Json::str(doc))])));
                    }
                    Json::obj(fields)
                })
                .collect(),
        )
    }
}

/// The name an import is known by: its `as` alias, or the file's name.
fn import_alias(imp: &crate::ast::ImportDecl) -> String {
    match &imp.alias {
        Some(a) => a.to_string(),
        None => Path::new(&imp.path).file_stem().map_or(String::new(), |s| s.to_string_lossy().to_string()),
    }
}

/// The file imported under `alias`, if any.
fn import_path(prog: &Program, alias: &str) -> Option<String> {
    prog.items.iter().find_map(|i| match i {
        Item::Import(imp) if import_alias(imp) == alias => Some(imp.path.clone()),
        _ => None,
    })
}

/// The lowercase names used in a text (outside strings and comments, as
/// far as a quick scan can tell).
fn words(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let code = line.split('#').next().unwrap_or("");
        let mut in_str = false;
        let mut cur = String::new();
        for c in code.chars().chain(std::iter::once(' ')) {
            if c == '"' {
                in_str = !in_str;
            }
            if !in_str && (c.is_alphanumeric() || c == '_' || (c == '!' && !cur.is_empty())) {
                cur.push(c);
            } else {
                if cur.len() > 1 && cur.starts_with(|f: char| f.is_lowercase() || f == '_') && !crate::lexer::KEYWORDS.contains(&cur.as_str()) {
                    out.push(std::mem::take(&mut cur));
                }
                cur.clear();
            }
        }
    }
    out
}

fn find_declaration(prog: &Program, word: &str) -> Option<crate::span::Span> {
    prog.items.iter().find_map(|item| match item {
        Item::Fn(def) if def.name.as_deref() == Some(word) => Some(def.name_span),
        Item::Type(td) if &*td.name == word => Some(td.name_span),
        Item::Type(td) => match &td.body {
            // A variant name leads to its type.
            TypeBody::Enum(vs) if vs.iter().any(|v| &*v.name == word) => Some(td.name_span),
            _ => None,
        },
        Item::Stmt(s) => match &s.kind {
            StmtKind::Let { pat, .. } => match &pat.kind {
                PatKind::Bind { name, .. } if &**name == word => Some(pat.span),
                _ => None,
            },
            _ => None,
        },
        _ => None,
    })
}

fn diagnostic_json(d: &Diagnostic, it: &Interp, lines: &LineIndex) -> Json {
    let range = match d.span {
        Some(sp) if sp.file == 0 => lines.range(sp.start as usize, sp.end as usize),
        _ => lines.range(0, 0),
    };
    let mut message = d.message.clone();
    // Problems in an imported file are shown at the top, with their location.
    if let Some(sp) = d.span {
        if sp.file != 0 && (sp.file as usize) < it.ctx.sm.files.len() {
            message = format!("{} (at {})", message, it.ctx.sm.location(sp));
        }
    }
    for n in &d.notes {
        message.push_str("\nnote: ");
        message.push_str(n);
    }
    if let Some(h) = &d.help {
        message.push_str("\nhelp: ");
        message.push_str(h);
    }
    let severity = match d.severity {
        Severity::Error => 1,
        Severity::Warning => 2,
    };
    Json::obj(vec![
        ("range", range),
        ("severity", Json::num(severity)),
        ("code", Json::str(d.code)),
        ("source", Json::str("cogito")),
        ("message", Json::str(message)),
    ])
}

/// Converts between byte offsets and LSP positions (lines, and characters
/// counted in UTF-16 code units).
struct LineIndex<'a> {
    text: &'a str,
    starts: Vec<usize>,
}

impl<'a> LineIndex<'a> {
    fn new(text: &'a str) -> LineIndex<'a> {
        let mut starts = vec![0];
        starts.extend(text.match_indices('\n').map(|(i, _)| i + 1));
        LineIndex { text, starts }
    }

    fn position(&self, offset: usize) -> Json {
        let offset = offset.min(self.text.len());
        let line = match self.starts.binary_search(&offset) {
            Ok(i) => i,
            Err(i) => i - 1,
        };
        let start = self.starts[line];
        let col = self.text.get(start..offset).map_or(0, |s| s.encode_utf16().count());
        Json::obj(vec![("line", Json::num(line as u32)), ("character", Json::num(col as u32))])
    }

    fn range(&self, start: usize, end: usize) -> Json {
        Json::obj(vec![("start", self.position(start)), ("end", self.position(end.max(start)))])
    }

    fn offset(&self, pos: &Json) -> Option<usize> {
        let line = pos.get("line").as_u64()? as usize;
        let ch = pos.get("character").as_u64()? as usize;
        let start = *self.starts.get(line)?;
        let end = self.starts.get(line + 1).copied().unwrap_or(self.text.len());
        let mut units = 0;
        for (i, c) in self.text[start..end].char_indices() {
            if units >= ch {
                return Some(start + i);
            }
            units += c.len_utf16();
        }
        Some(end)
    }
}

/// The occurrences of names in `text` (a document at `path`), from its
/// resolved program.
fn index_of(text: &str, path: &Path) -> Option<Vec<crate::symbols::Occurrence>> {
    let dir = path.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
    let mut it = Interp::new();
    let mut ns = Namespace::default();
    let file = it.ctx.sm.add(path.display().to_string(), text);
    let mut prog = crate::parser::parse_program(text, file).ok()?;
    crate::resolver::resolve_program(&mut it.ctx, &mut prog, &mut ns, &dir, false);
    Some(crate::symbols::occurrences(&prog, text))
}

/// The identifier (with a trailing `!`) at or just before a byte offset.
fn word_at(text: &str, offset: usize) -> Option<(String, usize, usize)> {
    let is_word = |c: char| c.is_alphanumeric() || c == '_' || c == '!';
    let mut start = offset.min(text.len());
    while start > 0 && text[..start].chars().next_back().is_some_and(is_word) {
        start -= text[..start].chars().next_back().unwrap().len_utf8();
    }
    let mut end = offset.min(text.len());
    while end < text.len() && text[end..].chars().next().is_some_and(is_word) {
        end += text[end..].chars().next().unwrap().len_utf8();
    }
    let w = &text[start..end];
    // `!` belongs only at the end of a name.
    let w = match w.find('!') {
        Some(i) => &w[..=i],
        None => w,
    };
    if w.is_empty() || w.starts_with(|c: char| c.is_ascii_digit()) {
        None
    } else {
        Some((w.to_string(), start, start + w.len()))
    }
}

fn keyword_doc(word: &str) -> Option<&'static str> {
    Some(match word {
        "let" => "`let name = value` binds a name that never changes. Use `var` for one that does.",
        "var" => "`var name = value` binds a name that can be reassigned. An annotation (`var n: Int = 0`) is checked on every write.",
        "fn" => "`fn name(param: Type) -> Type { body }` declares a function; `fn(x) => x + 1` is an anonymous function. A name ending in `!` mutates its first argument.",
        "match" => "`match value { pattern => result ... }` takes a value apart. It must cover every case (or end with `_ => ...`).",
        "requires" => "A precondition: checked on entry; a violation is the caller's bug. `cogito verify` checks contracts with generated inputs.",
        "ensures" => "A postcondition: checked on exit. `result` is the returned value; `old(e)` is `e` on entry.",
        "test" => "`test \"name\" { assert ... }` is run by `cogito test`.",
        "property" => "`property \"name\" (x: Type) where cond { assert ... }` is run by `cogito test` with generated inputs, and failures are shrunk.",
        "is" => "`value is Pattern` is a Bool: whether the value matches the pattern. It cannot bind names; use `match` for that.",
        "where" => "In a `property`, `where` filters the generated inputs.",
        "assert" => "`assert cond` (or `assert cond, \"message\"`) fails with both sides shown when a comparison is false.",
        "import" => "`import \"path/file.cog\"` (or `as alias`) binds a module; use its members as `alias.name`.",
        "type" => "`type Name = { field: Type }` declares a record; `type Name = | A | B(Type)` an enum; `type Name = OtherType` an alias.",
        _ => return None,
    })
}

/// The path of a `file://` URI (percent-decoded).
fn uri_to_path(uri: &str) -> PathBuf {
    let raw = uri.strip_prefix("file://").unwrap_or(uri);
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() + 1 {
            if let Some(b) = raw.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    let s = String::from_utf8_lossy(&out).into_owned();
    // `file:///C:/x` on Windows.
    let s = if s.len() > 2 && s.starts_with('/') && s.as_bytes()[2] == b':' { s[1..].to_string() } else { s };
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positions_count_utf16() {
        let text = "let é = 1\nlet 😀x = 2\n";
        let li = LineIndex::new(text);
        let p = li.position(text.find('x').unwrap());
        assert_eq!(p.get("line").as_u64(), Some(1));
        assert_eq!(p.get("character").as_u64(), Some(6));
        let back = li.offset(&p).unwrap();
        assert_eq!(&text[back..back + 1], "x");
    }

    #[test]
    fn words() {
        assert_eq!(word_at("xs.push!(1)", 4).map(|w| w.0), Some("push!".into()));
        assert_eq!(word_at("len(x)", 3).map(|w| w.0), Some("len".into()));
        assert_eq!(uri_to_path("file:///a%20b/c.cog"), PathBuf::from("/a b/c.cog"));
    }
}
