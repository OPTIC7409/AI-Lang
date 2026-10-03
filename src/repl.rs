//! The interactive read-eval-print loop.

use crate::ast::{Item, Namespace, StmtKind};
use crate::builtins::BUILTINS;
use crate::diagnostic::Colors;
use crate::interp::{Ctrl, Env, Interp};
use crate::value::{repr, Value};
use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;

/// Whether the input so far leaves a bracket or a triple-quoted string open.
pub fn needs_more(src: &str) -> bool {
    let mut depth: i64 = 0;
    let b = src.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'#' => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'"' => {
                if b[i..].starts_with(b"\"\"\"") {
                    match src[i + 3..].find("\"\"\"") {
                        Some(j) => i += j + 6,
                        None => return true,
                    }
                    continue;
                }
                i += 1;
                while i < b.len() && b[i] != b'"' && b[i] != b'\n' {
                    if b[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
            }
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            _ => {}
        }
        i += 1;
    }
    if depth > 0 {
        return true;
    }
    let t = src.trim_end();
    t.ends_with(['+', '-', '*', '/', '%', '=', ',', '.']) || t.ends_with("|>") || t.ends_with(" and") || t.ends_with(" or")
}

pub fn run(it: &mut Interp, color: bool) {
    let c = Colors::new(color);
    let stdin = std::io::stdin();
    let interactive = stdin.is_terminal();
    if interactive {
        crate::outln!("{}Cogito {}{} — a programming language designed by an AI", c.bold, crate::VERSION, c.reset);
        crate::outln!("{}Type an expression or statement. :help for help, :quit to exit.{}", c.dim, c.reset);
    }
    let mut ns = Namespace::default();
    let mut input_no = 0;
    loop {
        let mut src = String::new();
        let mut first = true;
        loop {
            if interactive {
                crate::out!("{}", if first { ">>> " } else { "... " });
                let _ = std::io::stdout().flush();
            }
            let mut line = String::new();
            match stdin.lock().read_line(&mut line) {
                Ok(0) | Err(_) => {
                    if src.trim().is_empty() {
                        if interactive {
                            crate::outln!();
                        }
                        return;
                    }
                    break;
                }
                Ok(_) => {}
            }
            src.push_str(&line);
            first = false;
            if !needs_more(&src) {
                break;
            }
        }
        let trimmed = src.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(cmd) = trimmed.strip_prefix(':') {
            let mut parts = cmd.splitn(2, ' ');
            match (parts.next().unwrap_or(""), parts.next().map(str::trim)) {
                ("q" | "quit" | "exit", _) => return,
                ("help" | "h", _) => {
                    crate::outln!("  :help           show this help");
                    crate::outln!("  :quit           leave the REPL");
                    crate::outln!("  :doc NAME       documentation for a built-in function");
                    crate::outln!("  :builtins       list all built-in functions");
                    crate::outln!("  :explain CODE   explain an error code, e.g. :explain E0101");
                    crate::outln!("Statements and declarations (let, var, fn, type) persist between inputs.");
                    crate::outln!("Multi-line input continues while brackets are open.");
                }
                ("doc", Some(name)) => match BUILTINS.iter().find(|b| b.name == name) {
                    Some(b) => crate::outln!("{}", b.doc),
                    None => crate::outln!("no built-in named `{}`", name),
                },
                ("builtins", _) => {
                    let names: Vec<&str> = BUILTINS.iter().map(|b| b.name).collect();
                    crate::outln!("{}", names.join(" "));
                }
                ("explain", Some(code)) => match crate::diagnostic::explain(code) {
                    Some((t, e)) => crate::outln!("{}: {}\n\n{}", code.to_uppercase(), t, e),
                    None => crate::outln!("unknown error code `{}`", code),
                },
                _ => crate::outln!("unknown command `:{}` (try :help)", cmd),
            }
            continue;
        }
        input_no += 1;
        let name = format!("<repl:{}>", input_no);
        // If the input has errors, forget any names it introduced.
        let saved_ns = ns.clone();
        let (prog, warnings) = match crate::load_source(it, &name, &src, Path::new("."), &mut ns, true) {
            Ok(p) => p,
            Err(diags) => {
                ns = saved_ns;
                for d in diags {
                    crate::err_out!("{}", d.render(&it.ctx.sm, color));
                }
                continue;
            }
        };
        for w in warnings {
            if w.code != "W0001" {
                crate::err_out!("{}", w.render(&it.ctx.sm, color));
            }
        }
        let result = (|| -> Result<Option<Value>, Ctrl> {
            it.install(&prog)?;
            let mut env = Env::new(prog.num_slots);
            let n = prog.items.len();
            let mut last = None;
            for (i, item) in prog.items.iter().enumerate() {
                if let Item::Stmt(s) = item {
                    if i + 1 == n {
                        if let StmtKind::Expr(e) = &s.kind {
                            last = Some(it.eval(e, &mut env)?);
                            continue;
                        }
                    }
                    it.exec_stmt(s, &mut env)?;
                }
            }
            Ok(last)
        })();
        it.flush();
        it.stack.clear();
        match result {
            Ok(Some(v)) => {
                if !matches!(v, Value::Unit) {
                    crate::outln!("{}", repr(&v));
                }
            }
            Ok(None) => {}
            Err(Ctrl::Error(d)) => crate::err_out!("{}", d.render(&it.ctx.sm, color)),
            Err(_) => {}
        }
    }
}
