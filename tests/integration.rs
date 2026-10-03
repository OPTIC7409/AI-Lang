//! End-to-end tests that drive the `cogito` binary.
//!
//! * `tests/lang/*.cog`   — the language's own test suite, written in Cogito.
//! * `tests/errors/*.cog` — programs that must fail; the first line says which
//!   error code (`# expect: E0101`).
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
        if out.status.success() || !stderr.contains(&format!("error[{}]", expect.trim())) {
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
fn spec_is_embedded() {
    let out = cogito(&["spec"]);
    assert!(out.status.success());
    assert!(text(&out.stdout).contains("Cogito"));
}
