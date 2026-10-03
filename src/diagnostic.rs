//! Diagnostics: every error Cogito reports has a code, a source location,
//! a human-readable message, and (whenever possible) a hint for fixing it.

use crate::span::{SourceMap, Span};
use std::fmt::Write as _;
use std::rc::Rc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

#[derive(Clone, Debug)]
pub struct TraceFrame {
    pub name: Rc<str>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct Diagnostic {
    pub severity: Severity,
    pub code: &'static str,
    pub message: String,
    pub span: Option<Span>,
    pub label: Option<String>,
    pub notes: Vec<String>,
    pub help: Option<String>,
    pub trace: Vec<TraceFrame>,
}

impl Diagnostic {
    pub fn error(code: &'static str, message: impl Into<String>) -> Diagnostic {
        Diagnostic {
            severity: Severity::Error,
            code,
            message: message.into(),
            span: None,
            label: None,
            notes: Vec::new(),
            help: None,
            trace: Vec::new(),
        }
    }

    pub fn warning(code: &'static str, message: impl Into<String>) -> Diagnostic {
        let mut d = Diagnostic::error(code, message);
        d.severity = Severity::Warning;
        d
    }

    pub fn at(mut self, span: Span) -> Diagnostic {
        self.span = Some(span);
        self
    }

    pub fn label(mut self, label: impl Into<String>) -> Diagnostic {
        self.label = Some(label.into());
        self
    }

    pub fn note(mut self, note: impl Into<String>) -> Diagnostic {
        self.notes.push(note.into());
        self
    }

    pub fn help(mut self, help: impl Into<String>) -> Diagnostic {
        self.help = Some(help.into());
        self
    }

    pub fn maybe_help(mut self, help: Option<String>) -> Diagnostic {
        if help.is_some() {
            self.help = help;
        }
        self
    }

    pub fn is_error(&self) -> bool {
        self.severity == Severity::Error
    }

    /// Render the diagnostic the way a compiler would, with a source excerpt.
    pub fn render(&self, sm: &SourceMap, color: bool) -> String {
        let c = Colors::new(color);
        let mut out = String::new();
        let (kind, kc) = match self.severity {
            Severity::Error => ("error", c.red),
            Severity::Warning => ("warning", c.yellow),
        };
        let _ = writeln!(out, "{}{}{}[{}]{}: {}{}{}", c.bold, kc, kind, self.code, c.reset, c.bold, self.message, c.reset);
        if let Some(span) = self.span {
            if (span.file as usize) < sm.files.len() {
                render_excerpt(&mut out, sm, span, self.label.as_deref(), kc, &c);
            }
        }
        for note in &self.notes {
            for (i, line) in note.lines().enumerate() {
                if i == 0 {
                    let _ = writeln!(out, "  {}={} {}note:{} {}", c.blue, c.reset, c.bold, c.reset, line);
                } else {
                    let _ = writeln!(out, "          {}", line);
                }
            }
        }
        if let Some(help) = &self.help {
            for (i, line) in help.lines().enumerate() {
                if i == 0 {
                    let _ = writeln!(out, "  {}={} {}help:{} {}", c.blue, c.reset, c.bold, c.reset, line);
                } else {
                    let _ = writeln!(out, "          {}", line);
                }
            }
        }
        if self.trace.len() >= 2 {
            let _ = writeln!(out, "  {}stack trace (most recent call first):{}", c.dim, c.reset);
            let max = 12;
            let n = self.trace.len();
            for (i, frame) in self.trace.iter().rev().enumerate() {
                if n > max && i == max / 2 {
                    let _ = writeln!(out, "    {}... {} more frames ...{}", c.dim, n - max, c.reset);
                }
                if n > max && i >= max / 2 && i < n - max / 2 {
                    continue;
                }
                if frame.span == Span::default() {
                    let _ = writeln!(out, "    {}at{} {}", c.dim, c.reset, frame.name);
                    continue;
                }
                let loc = if (frame.span.file as usize) < sm.files.len() { sm.location(frame.span) } else { "?".into() };
                let _ = writeln!(out, "    {}at{} {} {}({}){}", c.dim, c.reset, frame.name, c.dim, loc, c.reset);
            }
        }
        if self.severity == Severity::Error && explain(self.code).is_some() {
            let _ = writeln!(out, "  {}(run `cogito explain {}` for more about this error){}", c.dim, self.code, c.reset);
        }
        out
    }
}

fn render_excerpt(out: &mut String, sm: &SourceMap, span: Span, label: Option<&str>, kc: &str, c: &Colors) {
    let file = sm.get(span.file);
    let (line, col) = file.line_col(span.start as usize);
    let (end_line, end_col) = file.line_col((span.end as usize).max(span.start as usize));
    let gutter = end_line.to_string().len().max(2);
    let _ = writeln!(out, "{:>w$}{}-->{} {}:{}:{}", "", c.blue, c.reset, file.name, line, col, w = gutter);
    let _ = writeln!(out, "{:>w$} {}|{}", "", c.blue, c.reset, w = gutter);
    // Multi-line spans are shown by their first line only.
    let text = file.line_text(line);
    let _ = writeln!(out, "{}{:>w$} |{} {}", c.blue, line, c.reset, text.replace('\t', "    "), w = gutter);
    let text_chars = text.chars().count();
    let to = if line == end_line {
        if end_col > col {
            end_col
        } else {
            col + 1
        }
    } else {
        text_chars + 1
    };
    let width = to.saturating_sub(col).max(1);
    let prefix: String = text.chars().take(col.saturating_sub(1)).map(|ch| if ch == '\t' { "    " } else { " " }).collect();
    let _ = writeln!(
        out,
        "{:>w$} {}|{} {}{}{}{} {}{}",
        "",
        c.blue,
        c.reset,
        prefix,
        c.bold,
        kc,
        "^".repeat(width),
        label.unwrap_or(""),
        c.reset,
        w = gutter
    );
}

pub struct Colors {
    pub red: &'static str,
    pub yellow: &'static str,
    pub green: &'static str,
    pub blue: &'static str,
    pub cyan: &'static str,
    pub bold: &'static str,
    pub dim: &'static str,
    pub reset: &'static str,
}

impl Colors {
    pub fn new(enabled: bool) -> Colors {
        if enabled {
            Colors {
                red: "\x1b[31m",
                yellow: "\x1b[33m",
                green: "\x1b[32m",
                blue: "\x1b[34m",
                cyan: "\x1b[36m",
                bold: "\x1b[1m",
                dim: "\x1b[2m",
                reset: "\x1b[0m",
            }
        } else {
            Colors { red: "", yellow: "", green: "", blue: "", cyan: "", bold: "", dim: "", reset: "" }
        }
    }
}

/// Levenshtein distance, used for "did you mean ...?" suggestions.
pub fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        for j in 1..=b.len() {
            let cost = if a[i - 1].eq_ignore_ascii_case(&b[j - 1]) { 0 } else { 1 };
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// Pick the closest candidate to `name`, if any is close enough.
pub fn suggest<'a, I: IntoIterator<Item = &'a str>>(name: &str, candidates: I) -> Option<String> {
    let mut best: Option<(usize, &str)> = None;
    for cand in candidates {
        if cand == name || cand.is_empty() {
            continue;
        }
        let d = edit_distance(name, cand);
        let limit = match name.chars().count() {
            0..=2 => 1,
            3..=5 => 2,
            _ => 3,
        };
        if d <= limit && best.is_none_or(|(bd, bc)| d < bd || (d == bd && cand < bc)) {
            best = Some((d, cand));
        }
    }
    best.map(|(_, c)| c.to_string())
}

/// The error catalog: (code, title, long explanation).
pub const CATALOG: &[(&str, &str, &str)] = &[
    ("E0001", "unexpected character", "The lexer found a character that cannot start any token.\n\nCogito source code is ASCII outside of string literals and comments.\nCheck for stray symbols, smart quotes pasted from a word processor, or a\nmissing quote that ended a string early."),
    ("E0002", "unterminated string", "A string literal was opened with `\"` but never closed.\n\n    let s = \"hello      # missing closing quote\n\nStrings may not span lines unless they use triple quotes:\n\n    let poem = \"\"\"\n      roses are red\n      \"\"\""),
    ("E0003", "invalid number literal", "A number literal is malformed or too large.\n\nIntegers are 64-bit and signed, so the largest is 9223372036854775807.\nUnderscores may be used as separators: `1_000_000`.\nHex, binary and octal use `0x`, `0b`, `0o` prefixes."),
    ("E0004", "invalid escape sequence", "Inside a string, a backslash starts an escape sequence. Valid escapes are:\n\n    \\n  newline        \\t  tab        \\r  carriage return\n    \\\\  backslash      \\\"  quote      \\0  NUL\n    \\{  literal brace  \\}  literal brace\n    \\u{1F600}  any Unicode scalar value, in hex"),
    ("E0005", "invalid string interpolation", "Strings interpolate expressions written inside braces: \"x is {x}\".\nThe braces must contain an expression. To write a literal brace, escape it: \"\\{\".\n\nA format spec may follow a colon: \"{price:.2}\", \"{name:>10}\"."),
    ("E0010", "syntax error", "The parser expected something different at this point.\n\nCommon causes:\n  * a missing closing bracket, brace or parenthesis\n  * two statements on one line without a `;` between them\n  * using `=` where `==` was meant (or vice versa)\n  * a keyword used as a variable name"),
    ("E0011", "chained comparison", "Comparison operators cannot be chained, because `a < b < c` means different\nthings in different languages. Write the intent out explicitly:\n\n    a < b and b < c"),
    ("E0012", "invalid assignment target", "Only variables, fields and indexes can be assigned to:\n\n    x = 1\n    point.x = 2\n    grid[row][col] = 3\n\nThe root of the target must be a variable declared with `var`."),
    ("E0013", "invalid name", "Cogito uses capitalization to tell names apart, so the grammar never has to\nguess:\n\n  * types and constructors start with an uppercase letter: `Point`, `Some`\n  * variables and functions start with a lowercase letter or `_`: `total`, `parse_line`\n\nThis lets a pattern like `Red` (a constructor) be distinguished from `red`\n(a new variable binding) at a glance."),
    ("E0100", "undefined name", "A name was used that is not defined in any enclosing scope.\n\nCheck the spelling, and make sure the definition comes before its use when\nboth are at the top level. Functions and types may be used before they are\ndefined; `let` and `var` bindings may not."),
    ("E0101", "assignment to immutable binding", "Bindings created with `let` cannot be reassigned or mutated. Declare the\nvariable with `var` if it needs to change:\n\n    var count = 0\n    count += 1\n\nCogito makes mutability explicit so that readers (human or AI) can see at\nthe declaration which values may change."),
    ("E0102", "duplicate definition", "The same name is defined twice in the same scope.\n\nTop-level functions may share a name only if their parameter type\nannotations differ; this is called overloading and Cogito picks the first\nmatching definition at call time:\n\n    fn area(c: Circle) -> Float => pi * c.r * c.r\n    fn area(r: Rect) -> Float => r.w * r.h"),
    ("E0103", "used before declaration", "At the top level, statements run in order, so a `let` or `var` binding\ncannot be read before the line that creates it. Move the declaration up."),
    ("E0104", "break or continue outside a loop", "`break` and `continue` only make sense inside `while`, `for` or `loop`."),
    ("E0105", "return outside a function", "`return` can only be used inside a function body. At the top level a script\nsimply ends; use `exit(code)` to stop early."),
    ("E0106", "unknown type", "A type annotation names a type that does not exist.\n\nBuilt-in types: Int, Float, Str, Bool, Unit, Any, List[T], Map[K, V],\nOption[T], Result[T, E], tuples like (Int, Str), record types like\n{ x: Int, y: Int }, and function types like fn(Int) -> Int.\nUser types are declared with `type`."),
    ("E0107", "wrong number of arguments", "A function or constructor was called with too many or too few arguments.\nParameters with default values may be omitted."),
    ("E0108", "unknown field or argument name", "A named argument or record field does not exist on the target.\nCheck the spelling against the type or function definition."),
    ("E0109", "non-exhaustive match", "A `match` over an enum type must handle every variant, or include a\ncatch-all arm (`_ => ...`). This guarantees that adding a new variant\nlater produces a compile-time error at every place that must handle it,\ninstead of a runtime surprise."),
    ("E0110", "assignment to captured variable", "Closures in Cogito capture values, not variables: a closure receives a\nsnapshot of the variables it uses at the moment it is created. Assigning to\na captured variable inside the closure would only change the snapshot, so it\nis forbidden. Return the new value from the closure instead:\n\n    var total = 0\n    for x in xs { total += x }     # fine: no closure involved\n    let total2 = xs.fold(0, fn(acc, x) => acc + x)"),
    ("E0111", "mutating call on immutable value", "Functions whose names end in `!` mutate their first argument in place, for\nexample `xs.push!(4)` or `xs.sort!()`. The first argument must therefore be\na mutable place: a `var` variable, or a field or index of one.\n\nUse the non-mutating version to get a new value instead: `let ys = xs.push(4)`."),
    ("E0112", "wrong number of pattern fields", "A constructor pattern lists a different number of fields than the\nconstructor has. Use `..` inside the parentheses to ignore the rest:\n\n    Rect(w, ..) => w"),
    ("E0114", "import failed", "An `import` could not be completed: the file was not found, contained\nerrors, or imports form a cycle. Paths are relative to the importing file."),
    ("E0115", "`?` outside a function", "The `?` operator returns early from the enclosing function when it meets\n`Err(..)` or `None`. At the top level there is no function to return from;\nuse `match` or `unwrap()` instead."),
    ("E0116", "invalid declaration", "This declaration is not allowed here. For example, `type`, `test` and\n`property` declarations must appear at the top level of a file."),
    ("E0117", "`?` mixes Option and Result", "The `?` operator returns early with the `None` or `Err(..)` it finds. In a\nfunction declared to return a Result, an early `None` would be the wrong\ntype (and vice versa). Convert first:\n\n    let n = parse_int(s).ok_or(\"not a number\")?   # Option -> Result\n    let v = read_file(p).ok()?                     # Result -> Option"),
    ("E0200", "type mismatch", "A value did not have the type that a type annotation requires.\n\nCogito checks type annotations at runtime, at every function boundary: when\nan annotated parameter receives an argument, when an annotated function\nreturns, and when a value is stored in a typed field. Int values are\naccepted (and converted) where Float is expected."),
    ("E0201", "wrong number of arguments", "A function was called with too many or too few arguments."),
    ("E0202", "not callable", "Only functions and constructors can be called with `(...)`."),
    ("E0203", "no such field", "The value does not have the requested field or method.\n\n`value.name(args)` calls the function `name` with `value` as its first\nargument (unless `value` is a record with a field called `name`)."),
    ("E0204", "index out of bounds", "A list or string was indexed outside its bounds. Valid indexes for a list of\nlength n are 0..n-1, and negative indexes count from the end (-1 is the\nlast element). Use `get(xs, i)` for an index that may be missing: it returns\nan Option."),
    ("E0205", "key not found", "A map was indexed with a key it does not contain. Use `get(m, key)`, which\nreturns an Option, or `get_or(m, key, default)`."),
    ("E0206", "division by zero", "Division or remainder by zero is an error for both Int and Float in Cogito\n(rather than silently producing infinity or NaN). Guard the division, or\nstate the assumption as a contract: `requires divisor != 0`."),
    ("E0207", "integer overflow", "Int arithmetic is checked: results outside the 64-bit signed range are an\nerror, never silently wrapped. Use Float for very large magnitudes."),
    ("E0208", "no match arm matched", "None of the arms of a `match` matched the value. Add a catch-all arm\n(`_ => ...`) or handle the missing case explicitly."),
    ("E0209", "condition is not a Bool", "Conditions in `if`, `while`, `and`, `or`, `not`, `requires` and `ensures`\nmust be Bool. Cogito has no implicit truthiness, because \"is 0 false? is an\nempty list false?\" has different answers in every language. Compare\nexplicitly: `if xs.len() > 0`, `if name != \"\"`."),
    ("E0210", "unwrap failed", "`unwrap` was called on `None` or `Err(...)`. Handle the missing case with\n`match`, `unwrap_or(default)`, or propagate it with `?`."),
    ("E0211", "unsupported operation", "An operator was applied to values of types it does not support, such as\n`\"a\" + 1`. Cogito never converts types implicitly (other than Int to\nFloat). Use string interpolation to build strings: \"a{1}\"."),
    ("E0212", "pattern did not match", "A `let` or `for` destructuring pattern did not match the value. Use `match`\nwhen the shape of a value is not certain."),
    ("E0213", "stack overflow", "The maximum call depth was exceeded, usually because of unbounded\nrecursion. Check that every recursive function has a base case that is\nalways reached."),
    ("E0214", "uninitialized global", "A function read a top-level `let` or `var` before the line that defines it\nhad run."),
    ("E0215", "no matching overload", "A function with several definitions (overloads) was called, but no\ndefinition's parameter types accept the given arguments."),
    ("E0216", "invalid argument", "A built-in function received an argument value it cannot handle, such as a\nnegative count for `repeat`."),
    ("E0217", "panic", "The program called `panic(message)`."),
    ("E0218", "not yet implemented", "The program reached a `todo()`."),
    ("E0219", "step budget exceeded", "While running a property test or `cogito verify`, a single test case ran\nfor more steps (function calls plus loop iterations) than its budget.\nThis usually means an infinite loop, or a generated input that is too\nlarge for the algorithm. Restrict the inputs with `where` (properties) or\n`requires` (contracts), or raise the limit with `--budget N`."),
    ("E0300", "assertion failed", "An `assert` statement's condition was false. For comparisons, Cogito shows\nthe value of each side."),
    ("E0301", "precondition violated", "A function was called with arguments that violate its `requires`\ncontract. This is a bug in the *caller*: the function documented an\nassumption, and the call broke it."),
    ("E0302", "postcondition violated", "A function returned a value that violates its own `ensures` contract.\nThis is a bug in the *function*: it promised something it did not deliver.\nInside `ensures`, the name `result` refers to the returned value."),
    ("W0001", "unused variable", "A variable was declared but never read. Remove it, or prefix its name with\nan underscore (`_unused`) to signal that this is intentional."),
    ("W0002", "unreachable code", "Code after `return`, `break` or `continue` in the same block can never run."),
];

pub fn explain(code: &str) -> Option<(&'static str, &'static str)> {
    let code = code.trim().to_ascii_uppercase();
    CATALOG.iter().find(|(c, _, _)| *c == code).map(|(_, t, e)| (*t, *e))
}
