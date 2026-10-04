//! The Cogito interpreter compiled to WebAssembly.
//!
//! A tiny C-style interface, so the page needs no generated glue code:
//!
//! 1. `cogito_alloc(n)` returns a buffer; the page copies UTF-8 into it.
//! 2. `cogito_run(src, src_len, input, input_len, mode)` runs the program
//!    and returns a pointer to a little-endian `u32` length followed by a
//!    UTF-8 JSON object: `{"out": "...", "diag": "...", "status": N}`.
//!    `out` is the program's output (or the test report), `diag` holds
//!    diagnostics with ANSI colors, and `status` is the exit status the
//!    command-line tool would give.
//! 3. If the module traps, `cogito_panic_ptr`/`cogito_panic_len` describe
//!    the panic message, and the page must instantiate a fresh module.
//!
//! The page provides `env.cogito_host_now_ms` (`Date.now()`) and
//! `env.cogito_host_clock_ms` (`performance.now()`).

use cogito::ast::Namespace;
use cogito::diagnostic::{Colors, Diagnostic};
use cogito::interp::{Ctrl, Interp};
use cogito::testing::{self, Options, Summary};
use cogito::value::{repr, Value};
use std::cell::RefCell;
use std::path::Path;

const FILE: &str = "playground.cog";
/// Steps (calls plus loop iterations) a program may take before it is
/// stopped: a few seconds of work.
const RUN_BUDGET: u64 = 40_000_000;
/// Nested calls allowed. The binding limit is not the 32 MiB WebAssembly
/// stack but the browser's own native stack (about 1 MB), which deep
/// recursion through higher-order built-ins exhausts at roughly 340 levels.
const MAX_DEPTH: usize = 300;

const MODE_RUN: u32 = 0;
const MODE_TEST: u32 = 1;
const MODE_VERIFY: u32 = 2;
const MODE_CHECK: u32 = 3;
const MODE_VERIFY_ALL: u32 = 4;
const MODE_FORMAT: u32 = 5;

thread_local! {
    static RESULT: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

static mut PANIC_BUF: [u8; 2048] = [0; 2048];
static mut PANIC_LEN: usize = 0;

#[no_mangle]
pub extern "C" fn cogito_alloc(len: usize) -> *mut u8 {
    let mut v = Vec::<u8>::with_capacity(len.max(1));
    let p = v.as_mut_ptr();
    std::mem::forget(v);
    p
}

/// # Safety
/// `ptr` must come from `cogito_alloc(len)` and not have been freed.
#[no_mangle]
pub unsafe extern "C" fn cogito_free(ptr: *mut u8, len: usize) {
    drop(Vec::from_raw_parts(ptr, 0, len.max(1)));
}

#[no_mangle]
pub extern "C" fn cogito_panic_ptr() -> *const u8 {
    std::ptr::addr_of!(PANIC_BUF) as *const u8
}

#[no_mangle]
pub extern "C" fn cogito_panic_len() -> usize {
    unsafe { PANIC_LEN }
}

fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let msg = info.to_string();
        let bytes = msg.as_bytes();
        let n = bytes.len().min(2048);
        unsafe {
            let buf = &mut *std::ptr::addr_of_mut!(PANIC_BUF);
            buf[..n].copy_from_slice(&bytes[..n]);
            PANIC_LEN = n;
        }
    }));
}

/// # Safety
/// `src` and `input` must point to `src_len` and `input_len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn cogito_run(src: *const u8, src_len: usize, input: *const u8, input_len: usize, mode: u32) -> *const u8 {
    install_panic_hook();
    let src = String::from_utf8_lossy(std::slice::from_raw_parts(src, src_len)).into_owned();
    let input = std::slice::from_raw_parts(input, input_len).to_vec();
    let out = run(&src, input, mode);
    let json = format!("{{\"out\":{},\"diag\":{},\"status\":{}}}", json_str(&out.out), json_str(&out.diag), out.status);
    RESULT.with(|r| {
        let mut r = r.borrow_mut();
        r.clear();
        r.extend_from_slice(&(json.len() as u32).to_le_bytes());
        r.extend_from_slice(json.as_bytes());
        r.as_ptr()
    })
}

struct Outcome {
    out: String,
    diag: String,
    status: i32,
}

fn new_interp(input: Vec<u8>) -> Interp {
    let mut it = Interp::new();
    it.embedded = true;
    it.capture = Some(String::new());
    it.input = Some(std::io::Cursor::new(input));
    it.max_depth = MAX_DEPTH;
    it
}

fn render(it: &Interp, diags: &[Diagnostic]) -> String {
    diags.iter().map(|d| d.render(&it.ctx.sm, true)).collect()
}

fn run(src: &str, input: Vec<u8>, mode: u32) -> Outcome {
    if mode == MODE_FORMAT {
        // `out` is the formatted source.
        return match cogito::format::format_source(src) {
            Ok(code) => Outcome { out: code, diag: String::new(), status: 0 },
            Err(d) => {
                let mut it = new_interp(Vec::new());
                it.ctx.sm.add(FILE, src);
                Outcome { out: String::new(), diag: d.render(&it.ctx.sm, true), status: 2 }
            }
        };
    }
    let mut it = new_interp(input);
    if mode == MODE_TEST || mode == MODE_VERIFY || mode == MODE_VERIFY_ALL {
        it.test_mode = true;
    }
    let mut ns = Namespace::default();
    let loaded = cogito::load_source(&mut it, FILE, src, Path::new("."), &mut ns, false);
    let c = Colors::new(true);
    let (prog, warnings) = match loaded {
        Ok(p) => p,
        Err(diags) => {
            let n = diags.iter().filter(|d| d.is_error()).count();
            let mut diag = render(&it, &diags);
            diag.push_str(&format!("{}{}error{}: could not run the program due to {} error{}\n", c.bold, c.red, c.reset, n, plural(n)));
            return Outcome { out: String::new(), diag, status: 2 };
        }
    };
    // As on the command line, `check` shows every warning; running shows only
    // those that usually explain a wrong answer.
    let shown: Vec<Diagnostic> = warnings.iter().filter(|w| mode == MODE_CHECK || w.code != "W0001").cloned().collect();
    let mut diag = render(&it, &shown);
    match mode {
        MODE_CHECK => {
            diag.push_str(&format!("{}ok{}: no errors, {} warning{}\n", c.green, c.reset, warnings.len(), plural(warnings.len())));
            Outcome { out: String::new(), diag, status: 0 }
        }
        MODE_TEST | MODE_VERIFY | MODE_VERIFY_ALL => {
            // Run the top-level statements (but not `main`) with output muted.
            it.silent = true;
            it.budget = Some(RUN_BUDGET);
            let r = it.run_program(&prog);
            it.budget = None;
            it.silent = false;
            if let Err(Ctrl::Error(d)) = r {
                diag.push_str(&d.render(&it.ctx.sm, true));
                diag.push_str(&format!("{}error{}: the top-level code failed, so the tests were not run\n", c.red, c.reset));
                return Outcome { out: take(&mut it), diag, status: 1 };
            }
            let verify = mode != MODE_TEST;
            let opts = Options { color: true, all: mode == MODE_VERIFY_ALL, cases: if verify { 200 } else { 100 }, ..Options::default() };
            let start = cogito::platform::monotonic_seconds();
            let s = if verify { testing::run_verify(&mut it, &prog, FILE, &opts) } else { testing::run_tests(&mut it, &prog, FILE, &opts) };
            let secs = cogito::platform::monotonic_seconds() - start;
            let mut out = take(&mut it);
            out.push_str(&summary_line(&s, secs, &c));
            Outcome { out, diag, status: if s.failed == 0 && s.gave_up == 0 { 0 } else { 1 } }
        }
        MODE_RUN => {
            it.budget = Some(RUN_BUDGET);
            let r = cogito::run_with_value(&mut it, &prog, &ns);
            let mut status = 0;
            match r {
                Ok(Some(v)) if !matches!(v, Value::Unit) && it.exit_code.is_none() => {
                    let mut out = take(&mut it);
                    out.push_str(&format!("{}=> {}{}\n", c.dim, repr(&v), c.reset));
                    return Outcome { out, diag, status };
                }
                Ok(_) => {}
                Err(d) => {
                    diag.push_str(&d.render(&it.ctx.sm, true));
                    status = 1;
                }
            }
            if let Some(code) = it.exit_code {
                status = code;
            }
            Outcome { out: take(&mut it), diag, status }
        }
        _ => Outcome { out: String::new(), diag: format!("unknown mode {}\n", mode), status: 2 },
    }
}

fn take(it: &mut Interp) -> String {
    it.capture.take().unwrap_or_default()
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

fn summary_line(s: &Summary, secs: f64, c: &Colors) -> String {
    let ok = s.failed == 0 && s.gave_up == 0;
    let status = if ok { format!("{}ok{}", c.green, c.reset) } else { format!("{}FAILED{}", c.red, c.reset) };
    let mut parts = vec![format!("{} passed", s.passed), format!("{} failed", s.failed)];
    if s.gave_up > 0 {
        parts.push(format!("{} gave up", s.gave_up));
    }
    if s.skipped > 0 {
        parts.push(format!("{} skipped", s.skipped));
    }
    if s.cases > 0 {
        parts.push(format!("{} generated cases", s.cases));
    }
    format!("\n{}: {} {}({:.2}s){}\n", status, parts.join(", "), c.dim, secs, c.reset)
}

fn json_str(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for ch in s.chars() {
        match ch {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 || c == '\u{2028}' || c == '\u{2029}' => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}
