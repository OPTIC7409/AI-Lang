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
    cogito check [--json] FILE.cog... report errors and warnings without running (--json: one JSON object per line)
    cogito fmt [--check] [PATH...]  format files in the canonical layout (--check: only report; - for stdin)
    cogito lsp                      run the language server (for editors) on stdin/stdout
    cogito eval \"CODE\"              run a snippet of code
    cogito explain CODE             explain an error code (e.g. E0101)
    cogito doc [NAME | FILE.cog]    documentation for built-in functions, or a Markdown reference for a file
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
    --no-contracts   run/eval: skip `requires`, `ensures` and type invariants (`where`), for speed
",
        cogito::VERSION
    )
}

/// False after `--no-contracts`: `run` and `eval` then skip `requires`,
/// `ensures` and type invariants (`test` and `verify` always check them).
fn contracts_on() -> bool {
    std::env::var_os("COGITO_NO_CONTRACTS").is_none()
}

fn color_enabled() -> bool {
    std::env::var_os("NO_COLOR").is_none() && std::io::stderr().is_terminal()
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
                // Unused variables are noise while running; the other warnings
                // (an ignored result, `?` in a lambda, unreachable code) usually
                // explain a wrong answer.
                let likely_bugs: Vec<Diagnostic> = warnings.into_iter().filter(|w| w.code != "W0001").collect();
                print_diags(it, &likely_bugs, color);
            }
            Some((prog, ns))
        }
        Err(diags) => {
            // With errors to fix, unused variables are noise too.
            let shown: Vec<Diagnostic> = diags.iter().filter(|d| d.code != "W0001").cloned().collect();
            print_diags(it, &shown, color);
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
    it.contracts = contracts_on();
    it.args = prog_args;
    let Some((prog, ns)) = load(&mut it, path, color, true) else { return ExitCode::from(2) };
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
    it.contracts = contracts_on();
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
        let Some((prog, _ns)) = load(&mut it, f, opts.color, true) else {
            load_errors += 1;
            continue;
        };
        // Run top-level statements (but not `main`), with output suppressed
        // and a step budget, so that a runaway loop is reported.
        it.silent = true;
        it.budget = Some(opts.budget.saturating_mul(10));
        it.ticks = 0;
        let r = it.run_program(&prog);
        it.budget = None;
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

fn cmd_fmt(paths: &[String], check: bool, color: bool) -> ExitCode {
    // `cogito fmt -` formats standard input to standard output (for editors).
    if paths == ["-"] {
        let mut src = String::new();
        if std::io::Read::read_to_string(&mut std::io::stdin(), &mut src).is_err() {
            cogito::err_outln!("error: standard input is not valid UTF-8");
            return ExitCode::from(2);
        }
        return match cogito::format::format_source(&src) {
            Ok(out) => {
                cogito::out!("{}", out);
                ExitCode::SUCCESS
            }
            Err(d) => {
                let mut it = Interp::new();
                it.ctx.sm.add("<stdin>", src);
                cogito::err_out!("{}", d.render(&it.ctx.sm, color));
                ExitCode::from(2)
            }
        };
    }
    let files = collect_files(paths);
    let (mut changed, mut failed) = (0, 0);
    for f in &files {
        let src = match std::fs::read_to_string(f) {
            Ok(s) => s,
            Err(e) => {
                cogito::err_outln!("error: cannot read `{}`: {}", f.display(), e);
                failed += 1;
                continue;
            }
        };
        match cogito::format::format_source(&src) {
            Ok(out) if out == src => {}
            Ok(out) => {
                changed += 1;
                if check {
                    cogito::outln!("{} would be reformatted", f.display());
                } else if let Err(e) = std::fs::write(f, out) {
                    cogito::err_outln!("error: cannot write `{}`: {}", f.display(), e);
                    failed += 1;
                } else {
                    cogito::outln!("formatted {}", f.display());
                }
            }
            Err(d) => {
                let mut it = Interp::new();
                it.ctx.sm.add(f.display().to_string(), src);
                cogito::err_out!("{}", d.render(&it.ctx.sm, color));
                failed += 1;
            }
        }
    }
    if failed > 0 {
        ExitCode::from(2)
    } else if check && changed > 0 {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

fn cmd_check(paths: &[String], color: bool, json: bool) -> ExitCode {
    let files = collect_files(paths);
    let mut errors = 0;
    let mut warnings = 0;
    for f in &files {
        let mut it = Interp::new();
        let mut ns = Namespace::default();
        let ds = match cogito::load_file(&mut it, f, &mut ns) {
            Ok((_, ws)) => ws,
            Err(ds) => ds,
        };
        errors += ds.iter().filter(|d| d.is_error()).count();
        warnings += ds.iter().filter(|d| !d.is_error()).count();
        if json {
            // One JSON object per line, on stdout.
            for d in &ds {
                cogito::outln!("{}", d.to_json(&it.ctx.sm));
            }
        } else {
            print_diags(&it, &ds, color);
        }
    }
    if json {
        return if errors == 0 { ExitCode::SUCCESS } else { ExitCode::from(1) };
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
    // `cogito doc FILE.cog`: a reference for the file's own declarations.
    if let Some(path) = name.filter(|n| n.ends_with(".cog") || Path::new(n).is_file()) {
        let src = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => {
                cogito::err_outln!("error: cannot read `{}`: {}", path, e);
                return ExitCode::from(2);
            }
        };
        let title = Path::new(path).file_name().map_or(path.to_string(), |f| f.to_string_lossy().to_string());
        return match cogito::docgen::document(&src, &title) {
            Ok(md) => {
                cogito::out!("{}", md);
                ExitCode::SUCCESS
            }
            Err(d) => {
                let mut it = Interp::new();
                it.ctx.sm.add(path, src);
                cogito::err_out!("{}", d.render(&it.ctx.sm, false));
                ExitCode::from(2)
            }
        };
    }
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
    let raw: Vec<String> = std::env::args().skip(1).collect();
    // Global options (--max-depth N, --no-color) are read up to the program
    // being run: everything after `cogito FILE.cog` or `cogito run FILE.cog`
    // (or the code of `cogito eval`) belongs to the program's `args()`.
    let mut args = Vec::with_capacity(raw.len());
    let mut no_color = false;
    let mut positionals = 0;
    let mut want = usize::MAX;
    let mut i = 0;
    while i < raw.len() {
        let a = &raw[i];
        if positionals >= want {
            args.extend(raw[i..].iter().cloned());
            break;
        }
        if a == "--max-depth" || a.starts_with("--max-depth=") {
            let v = match a.strip_prefix("--max-depth=") {
                Some(v) => Some(v.to_string()),
                None => {
                    i += 1;
                    raw.get(i).cloned()
                }
            };
            match v.and_then(|n| n.parse::<usize>().ok()) {
                Some(n) => std::env::set_var("COGITO_MAX_DEPTH", n.to_string()),
                None => {
                    cogito::err_outln!("error: --max-depth needs a number");
                    return ExitCode::from(2);
                }
            }
        } else if a == "--no-color" {
            no_color = true;
        } else if a == "--no-contracts" {
            std::env::set_var("COGITO_NO_CONTRACTS", "1");
        } else {
            if !a.starts_with('-') || a == "-e" {
                positionals += 1;
                if positionals == 1 {
                    want = match a.as_str() {
                        "run" | "eval" | "-e" => 2,
                        f if f.ends_with(".cog") || Path::new(f).is_file() => 1,
                        _ => usize::MAX,
                    };
                }
            }
            args.push(a.clone());
        }
        i += 1;
    }
    let color = !no_color && color_enabled();
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
        "lsp" => ExitCode::from(cogito::lsp::run_stdio() as u8),
        "fmt" => {
            let check = args[1..].iter().any(|a| a == "--check");
            let paths: Vec<String> = args[1..].iter().filter(|a| !a.starts_with("--")).cloned().collect();
            cmd_fmt(&paths, check, color)
        }
        "check" => {
            let json = args[1..].iter().any(|a| a == "--json");
            let paths: Vec<String> = args[1..].iter().filter(|a| !a.starts_with("--")).cloned().collect();
            cmd_check(&paths, color, json)
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

/// The soft limit on this process's address space (`ulimit -v`), if any.
fn address_space_limit() -> Option<u64> {
    let limits = std::fs::read_to_string("/proc/self/limits").ok()?;
    let line = limits.lines().find(|l| l.starts_with("Max address space"))?;
    line.split_whitespace().nth(3)?.parse().ok()
}

fn main() -> ExitCode {
    // Run on a thread with a large stack so that deeply recursive Cogito
    // programs hit Cogito's own (friendly) recursion limit first. The stack
    // is reserved, not committed, so a large size costs nothing until it is
    // used, unless the address space is limited: then take a quarter of the
    // limit, and allow proportionally fewer nested calls.
    const FULL: u64 = 4 << 30;
    let mut stack = FULL;
    if let Some(limit) = address_space_limit() {
        stack = stack.min(limit / 4);
    }
    let user_depth = std::env::var_os("COGITO_MAX_DEPTH").is_some();
    loop {
        let size = usize::try_from(stack).unwrap_or(512 << 20);
        if stack < FULL && !user_depth {
            // About 40 KB of stack per nested call (100,000 calls in 4 GB).
            std::env::set_var("COGITO_MAX_DEPTH", (stack / (FULL / 100_000)).max(100).to_string());
        }
        match std::thread::Builder::new().stack_size(size).spawn(real_main) {
            Ok(child) => return child.join().unwrap_or(ExitCode::from(101)),
            Err(_) if stack > (16 << 20) => stack /= 4,
            Err(e) => {
                cogito::err_outln!("error: cannot start the interpreter: {}", e);
                return ExitCode::from(101);
            }
        }
    }
}
