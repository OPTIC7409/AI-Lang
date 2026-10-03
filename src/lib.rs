//! Cogito: a programming language designed and implemented by an AI.
//!
//! The pipeline is: source text -> [`lexer`] -> [`parser`] (AST) ->
//! [`resolver`] (static checks, name resolution) -> [`interp`] (execution).

// Errors are cold paths; boxing every diagnostic is not worth the noise.
#![allow(clippy::result_large_err)]
#![allow(clippy::type_complexity)]

pub mod ast;
pub mod builtins;
pub mod ctx;
pub mod diagnostic;
pub mod interp;
pub mod lexer;
pub mod parser;
pub mod proptest;
pub mod repl;
pub mod resolver;
pub mod span;
pub mod testing;
pub mod types;
pub mod value;

use ast::{Namespace, Program};
use diagnostic::Diagnostic;
use interp::Interp;
use std::path::Path;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Parse and resolve source code. On success returns the program and any
/// warnings; on failure returns all diagnostics.
pub fn load_source(
    it: &mut Interp,
    name: &str,
    src: &str,
    dir: &Path,
    ns: &mut Namespace,
    repl: bool,
) -> Result<(Program, Vec<Diagnostic>), Vec<Diagnostic>> {
    let file = it.ctx.sm.add(name, src);
    let mut prog = parser::parse_program(src, file).map_err(|d| vec![d])?;
    let diags = resolver::resolve_program(&mut it.ctx, &mut prog, ns, dir, repl);
    if diags.iter().any(|d| d.is_error()) {
        Err(diags)
    } else {
        Ok((prog, diags))
    }
}

/// Read, parse and resolve a file.
pub fn load_file(it: &mut Interp, path: &Path, ns: &mut Namespace) -> Result<(Program, Vec<Diagnostic>), Vec<Diagnostic>> {
    let src = std::fs::read_to_string(path).map_err(|e| vec![Diagnostic::error("E0114", format!("cannot read `{}`: {}", path.display(), e))])?;
    let dir = path.parent().map(|p| if p.as_os_str().is_empty() { Path::new(".") } else { p }).unwrap_or(Path::new("."));
    load_source(it, &path.display().to_string(), &src, dir, ns, false)
}

/// Run a whole program: top-level statements, then `main()` if defined.
pub fn run(it: &mut Interp, prog: &Program, ns: &Namespace) -> Result<(), Diagnostic> {
    run_with_value(it, prog, ns).map(|_| ())
}

/// Like [`run`], but also returns the value of the program's last top-level
/// statement when it is an expression (as the REPL and `cogito eval` show).
pub fn run_with_value(it: &mut Interp, prog: &Program, ns: &Namespace) -> Result<Option<value::Value>, Diagnostic> {
    let r = (|| -> interp::R<Option<value::Value>> {
        it.install(prog)?;
        let mut env = interp::Env::new(prog.num_slots);
        let n = prog.items.len();
        let mut last = None;
        for (i, item) in prog.items.iter().enumerate() {
            if let ast::Item::Stmt(s) = item {
                if let (true, ast::StmtKind::Expr(e)) = (i + 1 == n, &s.kind) {
                    last = Some(it.eval(e, &mut env)?);
                    continue;
                }
                it.exec_stmt(s, &mut env)?;
            }
        }
        if let Some(f) = it.global_by_name(ns, "main") {
            if f.is_callable() {
                it.call(&f, vec![], span::Span::default())?;
            }
        }
        Ok(last)
    })();
    it.flush();
    match r {
        Ok(v) => Ok(v),
        Err(interp::Ctrl::Error(d)) => Err(*d),
        Err(_) => Ok(None),
    }
}
