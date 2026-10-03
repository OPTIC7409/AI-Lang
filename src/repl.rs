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
    println!("{}Cogito {}{} — a programming language designed by an AI", c.bold, crate::VERSION, c.reset);
    println!("{}Type an expression or statement. :help for help, :quit to exit.{}", c.dim, c.reset);
    let stdin = std::io::stdin();
    let interactive = stdin.is_terminal();
    let mut ns = Namespace::default();
    let mut input_no = 0;
    loop {
        let mut src = String::new();
        let mut first = true;
        loop {
            if interactive || first {
                print!("{}", if first { ">>> " } else { "... " });
                let _ = std::io::stdout().flush();
            }
            let mut line = String::new();
            match stdin.lock().read_line(&mut line) {
                Ok(0) | Err(_) => {
                    if src.trim().is_empty() {
                        println!();
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
                    println!("  :help           show this help");
                    println!("  :quit           leave the REPL");
                    println!("  :doc NAME       documentation for a built-in function");
                    println!("  :builtins       list all built-in functions");
                    println!("  :explain CODE   explain an error code, e.g. :explain E0101");
                    println!("Statements and declarations (let, var, fn, type) persist between inputs.");
                    println!("Multi-line input continues while brackets are open.");
                }
                ("doc", Some(name)) => match BUILTINS.iter().find(|b| b.name == name) {
                    Some(b) => println!("{}", b.doc),
                    None => println!("no built-in named `{}`", name),
                },
                ("builtins", _) => {
                    let names: Vec<&str> = BUILTINS.iter().map(|b| b.name).collect();
                    println!("{}", names.join(" "));
                }
                ("explain", Some(code)) => match crate::diagnostic::explain(code) {
                    Some((t, e)) => println!("{}: {}\n\n{}", code.to_uppercase(), t, e),
                    None => println!("unknown error code `{}`", code),
                },
                _ => println!("unknown command `:{}` (try :help)", cmd),
            }
            continue;
        }
        input_no += 1;
        let name = format!("<repl:{}>", input_no);
        let (prog, warnings) = match crate::load_source(it, &name, &src, Path::new("."), &mut ns, true) {
            Ok(p) => p,
            Err(diags) => {
                for d in diags {
                    eprint!("{}", d.render(&it.ctx.sm, color));
                }
                continue;
            }
        };
        for w in warnings {
            if w.code != "W0001" {
                eprint!("{}", w.render(&it.ctx.sm, color));
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
                    println!("{}", repr(&v));
                }
            }
            Ok(None) => {}
            Err(Ctrl::Error(d)) => eprint!("{}", d.render(&it.ctx.sm, color)),
            Err(_) => {}
        }
    }
}
