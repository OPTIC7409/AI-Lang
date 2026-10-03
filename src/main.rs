#![allow(clippy::result_large_err)]

use cogito::ast::Namespace;
use cogito::diagnostic::{explain, Colors, Diagnostic, CATALOG};
use cogito::interp::Interp;
use cogito::testing::{self, Options, Summary};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const SPEC: &str = include_str!("../docs/llm-spec.md");

fn usage() -> String {
    format!(
        "Cogito {} — a programming language designed by an AI

USAGE:
    cogito                          start the interactive REPL
    cogito FILE.cog [ARGS...]       run a program
    cogito run FILE.cog [ARGS...]   run a program
    cogito test [PATH...]           run `test` and `property` blocks (files or directories)
    cogito verify FILE.cog          check function contracts against random inputs
    cogito check FILE.cog...        report errors and warnings without running
    cogito eval \"CODE\"              run a snippet of code
    cogito explain CODE             explain an error code (e.g. E0101)
    cogito doc [NAME]               documentation for built-in functions
    cogito spec                     print the compact language specification (for humans and LLMs)
    cogito version                  print the version

OPTIONS (test / verify):
    --cases N        number of random cases per property (default 100, verify: 200)
    --seed N         random seed (default: derived from each property's name)
    --filter TEXT    only run tests/functions whose name contains TEXT
    --all            verify: also check functions without contracts
    --budget N       maximum steps (calls + loop iterations) per generated case (default 10000000)

GLOBAL OPTIONS:
    --max-depth N    maximum number of nested calls before a stack-overflow error (default 100000)
    --no-color       disable colored output
",
        cogito::VERSION
    )
}

fn color_enabled(args: &[String]) -> bool {
    !args.iter().any(|a| a == "--no-color") && std::env::var_os("NO_COLOR").is_none() && std::io::stderr().is_terminal()
}

fn print_diags(it: &Interp, diags: &[Diagnostic], color: bool) {
    for d in diags {
        cogito::err_out!("{}", d.render(&it.ctx.sm, color));
    }
}

fn load(it: &mut Interp, path: &Path, color: bool, show_warnings: bool) -> Option<(cogito::ast::Program, Namespace)> {
    let mut ns = Namespace::default();
    match cogito::load_file(it, path, &mut ns) {
        Ok((prog, warnings)) => {
            if show_warnings {
                print_diags(it, &warnings, color);
            }
            Some((prog, ns))
        }
        Err(diags) => {
            print_diags(it, &diags, color);
            let n = diags.iter().filter(|d| d.is_error()).count();
            let c = Colors::new(color);
            cogito::err_outln!(
                "{}{}error{}: could not run `{}` due to {} error{}",
                c.bold,
                c.red,
                c.reset,
                path.display(),
                n,
                if n == 1 { "" } else { "s" }
            );
            None
        }
    }
}

fn cmd_run(path: &Path, prog_args: Vec<String>, color: bool) -> ExitCode {
    let mut it = Interp::new();
    it.args = prog_args;
    let Some((prog, ns)) = load(&mut it, path, color, false) else { return ExitCode::from(2) };
    match cogito::run(&mut it, &prog, &ns) {
        Ok(()) => ExitCode::SUCCESS,
        Err(d) => {
            cogito::err_out!("{}", d.render(&it.ctx.sm, color));
            ExitCode::from(1)
        }
    }
}

fn cmd_eval(code: &str, prog_args: Vec<String>, color: bool) -> ExitCode {
    let mut it = Interp::new();
    it.args = prog_args;
    let mut ns = Namespace::default();
    let (prog, _) = match cogito::load_source(&mut it, "<eval>", code, Path::new("."), &mut ns, false) {
        Ok(p) => p,
        Err(diags) => {
            print_diags(&it, &diags, color);
            return ExitCode::from(2);
        }
    };
    match cogito::run_with_value(&mut it, &prog, &ns) {
        Ok(v) => {
            if let Some(v) = v {
                if !matches!(v, cogito::value::Value::Unit) {
                    cogito::outln!("{}", cogito::value::repr(&v));
                }
            }
            ExitCode::SUCCESS
        }
        Err(d) => {
            cogito::err_out!("{}", d.render(&it.ctx.sm, color));
            ExitCode::from(1)
        }
    }
}

fn collect_files(paths: &[String]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let roots: Vec<PathBuf> = if paths.is_empty() { vec![PathBuf::from(".")] } else { paths.iter().map(PathBuf::from).collect() };
    for root in roots {
        if root.is_dir() {
            let mut stack = vec![root];
            while let Some(dir) = stack.pop() {
                let Ok(rd) = std::fs::read_dir(&dir) else { continue };
                let mut entries: Vec<PathBuf> = rd.filter_map(|e| e.ok()).map(|e| e.path()).collect();
                entries.sort();
                for p in entries {
                    let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                    if p.is_dir() {
                        if !name.starts_with('.') && name != "target" {
                            stack.push(p);
                        }
                    } else if p.extension().is_some_and(|e| e == "cog") {
                        out.push(p);
                    }
                }
            }
        } else {
            out.push(root);
        }
    }
    out.sort();
    out
}

fn has_tests(src: &str) -> bool {
    src.lines().any(|l| {
        let t = l.trim_start();
        t.starts_with("test \"") || t.starts_with("property \"")
    })
}

fn cmd_test(paths: &[String], opts: &Options, verify: bool) -> ExitCode {
    let start = std::time::Instant::now();
    let files = collect_files(paths);
    let mut total = Summary::default();
    let mut load_errors = 0;
    let c = Colors::new(opts.color);
    let mut ran = 0;
    for f in &files {
        if !verify && paths.iter().all(|p| Path::new(p).is_dir()) {
            // When scanning directories, skip files without tests.
            match std::fs::read_to_string(f) {
                Ok(src) if has_tests(&src) => {}
                _ => continue,
            }
        }
        ran += 1;
        let mut it = Interp::new();
        it.test_mode = true;
        let Some((prog, _ns)) = load(&mut it, f, opts.color, false) else {
            load_errors += 1;
            continue;
        };
        // Run top-level statements (but not `main`), with output suppressed.
        it.silent = true;
        let r = it.run_program(&prog);
        it.silent = false;
        if let Err(cogito::interp::Ctrl::Error(d)) = r {
            cogito::err_out!("{}", d.render(&it.ctx.sm, opts.color));
            cogito::err_outln!("{}error{}: the top-level code of `{}` failed, so its tests were not run", c.red, c.reset, f.display());
            load_errors += 1;
            continue;
        }
        let name = f.display().to_string();
        let s = if verify { testing::run_verify(&mut it, &prog, &name, opts) } else { testing::run_tests(&mut it, &prog, &name, opts) };
        total.add(s);
    }
    let secs = start.elapsed().as_secs_f64();
    let ok = total.failed == 0 && total.gave_up == 0 && load_errors == 0;
    let status = if ok { format!("{}ok{}", c.green, c.reset) } else { format!("{}FAILED{}", c.red, c.reset) };
    let mut parts = vec![format!("{} passed", total.passed), format!("{} failed", total.failed)];
    if total.gave_up > 0 {
        parts.push(format!("{} gave up", total.gave_up));
    }
    if total.skipped > 0 {
        parts.push(format!("{} skipped", total.skipped));
    }
    if load_errors > 0 {
        parts.push(format!("{} file{} with errors", load_errors, if load_errors == 1 { "" } else { "s" }));
    }
    if total.cases > 0 {
        parts.push(format!("{} generated cases", total.cases));
    }
    if ran == 0 {
        cogito::outln!("no test files found");
        return ExitCode::SUCCESS;
    }
    cogito::outln!("\n{}: {} {}({:.2}s){}", status, parts.join(", "), c.dim, secs, c.reset);
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

fn cmd_check(paths: &[String], color: bool) -> ExitCode {
    let files = collect_files(paths);
    let mut errors = 0;
    let mut warnings = 0;
    for f in &files {
        let mut it = Interp::new();
        let mut ns = Namespace::default();
        match cogito::load_file(&mut it, f, &mut ns) {
            Ok((_, ws)) => {
                warnings += ws.len();
                print_diags(&it, &ws, color);
            }
            Err(ds) => {
                errors += ds.iter().filter(|d| d.is_error()).count();
                warnings += ds.iter().filter(|d| !d.is_error()).count();
                print_diags(&it, &ds, color);
            }
        }
    }
    let c = Colors::new(color);
    if errors == 0 {
        cogito::err_outln!(
            "{}ok{}: {} file{} checked, {} warning{}",
            c.green,
            c.reset,
            files.len(),
            if files.len() == 1 { "" } else { "s" },
            warnings,
            if warnings == 1 { "" } else { "s" }
        );
        ExitCode::SUCCESS
    } else {
        cogito::err_outln!(
            "{}error{}: {} error{}, {} warning{}",
            c.red,
            c.reset,
            errors,
            if errors == 1 { "" } else { "s" },
            warnings,
            if warnings == 1 { "" } else { "s" }
        );
        ExitCode::from(1)
    }
}

fn cmd_doc(name: Option<&str>) -> ExitCode {
    use cogito::builtins::BUILTINS;
    match name {
        Some(n) => {
            let n = n.trim_end_matches("()");
            match BUILTINS.iter().find(|b| b.name == n) {
                Some(b) => {
                    cogito::outln!("{}", b.doc);
                    ExitCode::SUCCESS
                }
                None => {
                    let names: Vec<&str> = BUILTINS.iter().map(|b| b.name).collect();
                    match cogito::diagnostic::suggest(n, names) {
                        Some(s) => cogito::err_outln!("no built-in function `{}`; did you mean `{}`?", n, s),
                        None => cogito::err_outln!("no built-in function `{}`", n),
                    }
                    ExitCode::from(1)
                }
            }
        }
        None => {
            let mut cat = "";
            for b in BUILTINS {
                if b.category != cat {
                    cat = b.category;
                    cogito::outln!("\n## {}\n", cat);
                }
                let mut lines = b.doc.lines();
                let sig = lines.next().unwrap_or("");
                let desc = lines.next().unwrap_or("");
                cogito::outln!("  {:<58} {}", sig, desc);
            }
            ExitCode::SUCCESS
        }
    }
}

fn parse_opts(args: &[String], color: bool, default_cases: u32) -> Result<(Options, Vec<String>), String> {
    let mut opts = Options { color, cases: default_cases, ..Options::default() };
    let mut rest = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        let value = |i: &mut usize| -> Result<String, String> {
            *i += 1;
            args.get(*i).cloned().ok_or_else(|| format!("`{}` needs a value", a))
        };
        match a.as_str() {
            "--cases" => opts.cases = value(&mut i)?.parse().map_err(|_| "--cases needs a number".to_string())?,
            "--seed" => opts.seed = Some(value(&mut i)?.parse().map_err(|_| "--seed needs a number".to_string())?),
            "--filter" => opts.filter = Some(value(&mut i)?),
            "--all" => opts.all = true,
            "--budget" => opts.budget = value(&mut i)?.parse().map_err(|_| "--budget needs a number".to_string())?,
            "--no-color" => {}
            s if s.starts_with("--") => return Err(format!("unknown option `{}`", s)),
            _ => rest.push(a.clone()),
        }
        i += 1;
    }
    Ok((opts, rest))
}

fn real_main() -> ExitCode {
    cogito::builtins::start_clock();
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    // Global option: --max-depth N (maximum number of nested calls).
    if let Some(i) = args.iter().position(|a| a == "--max-depth") {
        match args.get(i + 1).and_then(|n| n.parse::<usize>().ok()) {
            Some(n) => {
                std::env::set_var("COGITO_MAX_DEPTH", n.to_string());
                args.drain(i..i + 2);
            }
            None => {
                cogito::err_outln!("error: --max-depth needs a number");
                return ExitCode::from(2);
            }
        }
    }
    let color = color_enabled(&args);
    let Some(cmd) = args.first() else {
        let mut it = Interp::new();
        cogito::repl::run(&mut it, std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none());
        return ExitCode::SUCCESS;
    };
    match cmd.as_str() {
        "help" | "--help" | "-h" => {
            cogito::out!("{}", usage());
            ExitCode::SUCCESS
        }
        "version" | "--version" | "-V" => {
            cogito::outln!("cogito {}", cogito::VERSION);
            ExitCode::SUCCESS
        }
        "repl" => {
            let mut it = Interp::new();
            cogito::repl::run(&mut it, color);
            ExitCode::SUCCESS
        }
        "run" => match args.get(1) {
            Some(f) => cmd_run(Path::new(f), args[2..].to_vec(), color),
            None => {
                cogito::err_outln!("usage: cogito run FILE.cog [ARGS...]");
                ExitCode::from(2)
            }
        },
        "eval" | "-e" => match args.get(1) {
            Some(code) => cmd_eval(code, args[2..].to_vec(), color),
            None => {
                cogito::err_outln!("usage: cogito eval \"CODE\"");
                ExitCode::from(2)
            }
        },
        "test" | "verify" => {
            let verify = cmd == "verify";
            match parse_opts(&args[1..], color, if verify { 200 } else { 100 }) {
                Ok((opts, paths)) => cmd_test(&paths, &opts, verify),
                Err(m) => {
                    cogito::err_outln!("error: {}", m);
                    ExitCode::from(2)
                }
            }
        }
        "check" => {
            let paths: Vec<String> = args[1..].iter().filter(|a| !a.starts_with("--")).cloned().collect();
            cmd_check(&paths, color)
        }
        "explain" => match args.get(1) {
            Some(code) => match explain(code) {
                Some((title, text)) => {
                    cogito::outln!("{}: {}\n\n{}", code.to_uppercase(), title, text);
                    ExitCode::SUCCESS
                }
                None => {
                    cogito::err_outln!("unknown error code `{}`", code);
                    ExitCode::from(1)
                }
            },
            None => {
                for (code, title, _) in CATALOG {
                    cogito::outln!("{}  {}", code, title);
                }
                ExitCode::SUCCESS
            }
        },
        "doc" => cmd_doc(args.get(1).map(|s| s.as_str())),
        "spec" => {
            cogito::out!("{}", SPEC);
            ExitCode::SUCCESS
        }
        f if f.ends_with(".cog") || Path::new(f).is_file() => cmd_run(Path::new(f), args[1..].to_vec(), color),
        other => {
            cogito::err_outln!("unknown command `{}`\n", other);
            cogito::err_out!("{}", usage());
            ExitCode::from(2)
        }
    }
}

fn main() -> ExitCode {
    // Run on a thread with a large stack so that deeply recursive Cogito
    // programs hit Cogito's own (friendly) recursion limit first.
    // The stack is reserved, not committed, so a large size costs nothing
    // until it is used.
    let stack = usize::try_from(4u64 << 30).unwrap_or(512 << 20);
    let child = std::thread::Builder::new().stack_size(stack).spawn(real_main).expect("failed to start interpreter thread");
    child.join().unwrap_or(ExitCode::from(101))
}
