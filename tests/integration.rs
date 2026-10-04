//! End-to-end tests that drive the `cogito` binary.
//!
//! * `tests/lang/*.cog`   — the language's own test suite, written in Cogito.
//! * `tests/errors/*.cog` — programs that must fail; the first line says which
//!   error code (`# expect: E0101`), or the exact message (`# expect: error: ...`).
//! * `tests/warnings/*.cog` — programs that `check` accepts with exactly the
//!   warning on the first line (`# expect: W0003`, or `# expect: nothing`).
//! * `tests/verify/*.cog` — programs that `verify` must reject, with a line
//!   `# expect: text` giving part of the report.
//! * `examples/*.cog`     — example programs; when `examples/NAME.out` exists,
//!   the program's output must match it exactly.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn cogito(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_cogito"))
        .args(args)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env("NO_COLOR", "1")
        .output()
        .expect("failed to run cogito")
}

fn files(dir: &str, ext: &str) -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join(dir);
    let mut out: Vec<PathBuf> = std::fs::read_dir(&root)
        .unwrap_or_else(|e| panic!("cannot read {}: {}", root.display(), e))
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == ext))
        .collect();
    out.sort();
    out
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).to_string()
}

#[test]
fn language_test_suite() {
    let out = cogito(&["test", "tests/lang"]);
    assert!(out.status.success(), "the Cogito test suite failed:\n{}{}", text(&out.stdout), text(&out.stderr));
}

#[test]
fn error_codes() {
    let mut failures = Vec::new();
    for f in files("tests/errors", "cog") {
        let src = std::fs::read_to_string(&f).unwrap();
        let expect =
            src.lines().next().and_then(|l| l.strip_prefix("# expect: ")).unwrap_or_else(|| panic!("{} has no `# expect:` line", f.display()));
        let out = cogito(&["run", f.to_str().unwrap()]);
        let stderr = text(&out.stderr);
        // `# expect: error: text` is an exact message (as `main` returning
        // `Err` prints); otherwise the line names an error code.
        let wanted = if expect.starts_with("error: ") { expect.to_string() } else { format!("error[{}]", expect.trim()) };
        if out.status.success() || !stderr.contains(&wanted) {
            failures.push(format!("{}: expected error {}, got:\n{}", f.display(), expect, stderr));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

#[test]
fn examples_produce_expected_output() {
    let mut failures = Vec::new();
    for f in files("examples", "cog") {
        let expected_path = f.with_extension("out");
        let Ok(expected) = std::fs::read_to_string(&expected_path) else { continue };
        let out = cogito(&["run", f.to_str().unwrap()]);
        let stdout = text(&out.stdout);
        if !out.status.success() {
            failures.push(format!("{} failed:\n{}", f.display(), text(&out.stderr)));
        } else if stdout != expected {
            failures.push(format!("{}: output differs.\n--- expected\n{}\n--- actual\n{}", f.display(), expected, stdout));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

#[test]
fn check_json_output() {
    let out = cogito(&["check", "--json", "tests/errors/builtin_arg_kind.cog", "tests/warnings/ignored_result.cog"]);
    assert_eq!(out.status.code(), Some(1));
    let stdout = text(&out.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 2, "{}", stdout);
    assert!(lines[0].starts_with('{') && lines[0].contains("\"code\":\"E0121\"") && lines[0].contains("\"line\":3,\"column\":15"), "{}", lines[0]);
    assert!(lines[1].contains("\"severity\":\"warning\"") && lines[1].contains("\"code\":\"W0006\""), "{}", lines[1]);
}

#[test]
fn test_and_verify_json_output() {
    let out = cogito(&["test", "--json", "tests/lang/numbers.cog"]);
    assert!(out.status.success());
    let stdout = text(&out.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    assert!(lines.iter().take(lines.len() - 1).all(|l| l.contains("\"kind\":\"test\"") && l.contains("\"status\":\"passed\"")), "{}", stdout);
    assert!(lines.last().unwrap().contains("\"summary\":true") && lines.last().unwrap().contains("\"ok\":true"), "{}", stdout);
    let out = cogito(&["verify", "--json", "tests/verify/bang_invariant.cog"]);
    assert_eq!(out.status.code(), Some(1));
    let stdout = text(&out.stdout);
    assert!(
        stdout.contains("\"status\":\"failed\"") && stdout.contains("\"counterexample\":{\"s\":") && stdout.contains("\"code\":\"E0303\""),
        "{}",
        stdout
    );
}

#[test]
fn warning_codes() {
    let mut failures = Vec::new();
    for f in files("tests/warnings", "cog") {
        let src = std::fs::read_to_string(&f).unwrap();
        let expect =
            src.lines().next().and_then(|l| l.strip_prefix("# expect: ")).unwrap_or_else(|| panic!("{} has no `# expect:` line", f.display()));
        let out = cogito(&["check", f.to_str().unwrap()]);
        let stderr = text(&out.stderr);
        let codes: Vec<&str> = stderr.lines().filter_map(|l| l.strip_prefix("warning[")).filter_map(|l| l.split(']').next()).collect();
        let wanted: Vec<&str> = if expect.trim() == "nothing" { vec![] } else { vec![expect.trim()] };
        if !out.status.success() || codes != wanted {
            failures.push(format!("{}: expected warnings {:?}, got:\n{}", f.display(), wanted, stderr));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

#[test]
fn verify_failures() {
    let mut failures = Vec::new();
    for f in files("tests/verify", "cog") {
        let src = std::fs::read_to_string(&f).unwrap();
        let expect = src.lines().find_map(|l| l.strip_prefix("# expect: ")).unwrap_or_else(|| panic!("{} has no `# expect:` line", f.display()));
        let out = cogito(&["verify", f.to_str().unwrap()]);
        let stdout = text(&out.stdout);
        if out.status.success() || !stdout.contains(expect) {
            failures.push(format!("{}: expected a failure containing {:?}, got:\n{}{}", f.display(), expect, stdout, text(&out.stderr)));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

#[test]
fn examples_tests_and_contracts_pass() {
    let out = cogito(&["test", "examples"]);
    assert!(out.status.success(), "example tests failed:\n{}{}", text(&out.stdout), text(&out.stderr));
    for f in files("examples", "cog") {
        let src = std::fs::read_to_string(&f).unwrap();
        if src.contains("ensures") || src.contains("requires") {
            let out = cogito(&["verify", f.to_str().unwrap()]);
            assert!(out.status.success(), "verify {} failed:\n{}{}", f.display(), text(&out.stdout), text(&out.stderr));
        }
    }
}

#[test]
fn check_reports_no_errors_for_examples() {
    let out = cogito(&["check", "examples"]);
    assert!(out.status.success(), "check failed:\n{}", text(&out.stderr));
}

#[test]
fn eval_and_exit_codes() {
    let out = cogito(&["eval", "print(6 * 7)"]);
    assert_eq!(text(&out.stdout), "42\n");
    assert!(out.status.success());
    let out = cogito(&["eval", "print(nope)"]);
    assert_eq!(out.status.code(), Some(2), "static errors exit with 2");
    let out = cogito(&["eval", "print(1 // 0)"]);
    assert_eq!(out.status.code(), Some(1), "runtime errors exit with 1");
}

#[test]
fn explain_knows_every_code_used() {
    for f in files("tests/errors", "cog") {
        let src = std::fs::read_to_string(&f).unwrap();
        let code = src.lines().next().unwrap().trim_start_matches("# expect: ").trim().to_string();
        if code.starts_with("error: ") {
            continue;
        }
        let out = cogito(&["explain", &code]);
        assert!(out.status.success(), "`cogito explain {}` failed", code);
    }
}

#[test]
fn repl_evaluates_piped_input() {
    use std::io::Write;
    let mut child = Command::new(env!("CARGO_BIN_EXE_cogito"))
        .env("NO_COLOR", "1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.as_mut().unwrap().write_all(b"let x = 20\nfn twice(n) => n * 2\ntwice(x) + 2\nvar xs = [\n1,\n2]\nxs.push!(3)\nxs\n").unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(text(&out.stdout), "42\n[1, 2, 3]\n", "stderr: {}", text(&out.stderr));
}

#[test]
fn exhaustiveness_never_gives_up_silently() {
    // 70 columns is deeper than the exhaustiveness search goes: rather than
    // accept the match unchecked, `check` asks for a catch-all arm.
    let n = 70;
    let mut src = "fn f(t) -> Int => match t {\n".to_string();
    for i in 0..n {
        let row: Vec<&str> = (0..n).map(|j| if i == j { "true" } else { "_" }).collect();
        src += &format!("  ({}) => {}\n", row.join(", "), i);
    }
    src += &format!("  ({}) => -1\n}}\n", vec!["false"; n].join(", "));
    let out = cogito(&["eval", &src]);
    assert!(text(&out.stderr).contains("too many combinations"), "stderr: {}", text(&out.stderr));
}

#[test]
fn every_syntax_error_is_reported() {
    let out = cogito(&["check", "tests/errors/several_syntax.cog"]);
    let stderr = text(&out.stderr);
    assert_eq!(stderr.matches("error[E00").count(), 3, "{}", stderr);
}

#[test]
fn spec_is_embedded() {
    let out = cogito(&["spec"]);
    assert!(out.status.success());
    assert!(text(&out.stdout).contains("Cogito"));
}

#[test]
fn language_server_reports_diagnostics() {
    use std::io::{BufRead, BufReader, Read, Write};
    let mut child = Command::new(env!("CARGO_BIN_EXE_cogito"))
        .arg("lsp")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut send = |body: &str| {
        write!(stdin, "Content-Length: {}\r\n\r\n{}", body.len(), body).unwrap();
        stdin.flush().unwrap();
    };
    let mut recv = || {
        let mut len = 0;
        loop {
            let mut line = String::new();
            stdout.read_line(&mut line).unwrap();
            if line.trim().is_empty() {
                break;
            }
            if let Some(v) = line.strip_prefix("Content-Length:") {
                len = v.trim().parse().unwrap();
            }
        }
        let mut buf = vec![0; len];
        stdout.read_exact(&mut buf).unwrap();
        String::from_utf8(buf).unwrap()
    };
    send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#);
    assert!(recv().contains("documentFormattingProvider"));
    send(
        r#"{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":"file:///tmp/x.cog","languageId":"cogito","version":1,"text":"fn f(n: Int) -> Int => n\nprint(f(\"a\"))\n"}}}"#,
    );
    let diags = recv();
    assert!(diags.contains("publishDiagnostics") && diags.contains("E0121"), "{}", diags);
    // Completion: names in scope after a prefix, functions and fields after `.`.
    let src = "type Point = { x: Int, y: Int }\nfn norm(p: Point) -> Int => p.x + p.y\nlet pt = Point(1, 2)\nprint(pt.no";
    send(&format!(
        r#"{{"jsonrpc":"2.0","method":"textDocument/didChange","params":{{"textDocument":{{"uri":"file:///tmp/x.cog","version":2}},"contentChanges":[{{"text":{}}}]}}}}"#,
        cogito::json::Json::str(src)
    ));
    recv();
    send(
        r#"{"jsonrpc":"2.0","id":3,"method":"textDocument/completion","params":{"textDocument":{"uri":"file:///tmp/x.cog"},"position":{"line":3,"character":11}}}"#,
    );
    let items = recv();
    assert!(items.contains("\"label\":\"norm\"") && !items.contains("\"label\":\"print\""), "{}", items);
    send(
        r#"{"jsonrpc":"2.0","id":4,"method":"textDocument/completion","params":{"textDocument":{"uri":"file:///tmp/x.cog"},"position":{"line":3,"character":1}}}"#,
    );
    let items = recv();
    assert!(items.contains("\"label\":\"print\"") && items.contains("\"label\":\"pt\""), "{}", items);
    // Hover: the type the checker infers for a variable.
    let src = "let words = [\"a\"].map(fn(w) => w.upper())\nvar n = 0\nn += words.len()\nprint(words, n)\n";
    send(&format!(
        r#"{{"jsonrpc":"2.0","method":"textDocument/didChange","params":{{"textDocument":{{"uri":"file:///tmp/x.cog","version":3}},"contentChanges":[{{"text":{}}}]}}}}"#,
        cogito::json::Json::str(src)
    ));
    recv();
    send(
        r#"{"jsonrpc":"2.0","id":5,"method":"textDocument/hover","params":{"textDocument":{"uri":"file:///tmp/x.cog"},"position":{"line":3,"character":8}}}"#,
    );
    let hover = recv();
    assert!(hover.contains("words: List[Str]"), "{}", hover);
    send(
        r#"{"jsonrpc":"2.0","id":6,"method":"textDocument/hover","params":{"textDocument":{"uri":"file:///tmp/x.cog"},"position":{"line":3,"character":14}}}"#,
    );
    let hover = recv();
    assert!(hover.contains("n: Int"), "{}", hover);
    send(r#"{"jsonrpc":"2.0","id":2,"method":"shutdown"}"#);
    assert!(recv().contains("\"id\":2"));
    send(r#"{"jsonrpc":"2.0","method":"exit"}"#);
    assert!(child.wait().unwrap().success());
}
