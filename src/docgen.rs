//! Documentation from source: `cogito doc FILE.cog` writes a Markdown
//! reference of a file's types and functions, each with its signature
//! (including contracts) and the comment lines directly above it. The
//! language server shows the same text on hover.

use crate::ast::{Item, Program};
use crate::diagnostic::Diagnostic;

pub struct Decl {
    pub kind: &'static str,
    pub name: String,
    /// The declaration's source up to its body: for a function, the
    /// signature with its `requires`/`ensures` lines.
    pub signature: String,
    pub comment: String,
}

/// The comment lines directly above byte offset `start`.
fn comment_above(text: &str, start: usize) -> String {
    let line_start = text[..start].rfind('\n').map_or(0, |i| i + 1);
    let mut lines = Vec::new();
    for l in text[..line_start].lines().rev() {
        match l.trim().strip_prefix('#') {
            Some(c) => lines.push(c.strip_prefix(' ').unwrap_or(c).trim_end().to_string()),
            None => break,
        }
    }
    lines.reverse();
    lines.join("\n")
}

/// The top-level declarations of a parsed program.
pub fn declarations(text: &str, prog: &Program) -> Vec<Decl> {
    let mut out = Vec::new();
    for item in &prog.items {
        match item {
            Item::Fn(def) => {
                let start = def.span.start as usize;
                let end = (def.body.span.start as usize).clamp(start, text.len());
                let mut sig = text[start..end].trim_end().to_string();
                for suffix in ["=>", "{"] {
                    if let Some(s) = sig.strip_suffix(suffix) {
                        sig = s.trim_end().to_string();
                    }
                }
                out.push(Decl { kind: "function", name: def.display_name().to_string(), signature: sig, comment: comment_above(text, start) });
            }
            Item::Type(td) => {
                let (s, e) = (td.span.start as usize, (td.span.end as usize).min(text.len()));
                out.push(Decl {
                    kind: "type",
                    name: td.name.to_string(),
                    signature: text[s..e].trim_end().to_string(),
                    comment: comment_above(text, s),
                });
            }
            _ => {}
        }
    }
    out
}

/// Markdown for one declaration (used for hover).
pub fn markdown(d: &Decl) -> String {
    let mut s = format!("```cogito\n{}\n```", d.signature);
    if !d.comment.is_empty() {
        s.push('\n');
        s.push_str(&d.comment);
    }
    s
}

/// A Markdown reference for a whole file.
pub fn document(text: &str, title: &str) -> Result<String, Diagnostic> {
    let prog = crate::parser::parse_program(text, 0)?;
    let decls = declarations(text, &prog);
    let mut out = format!("# {}\n", title);
    // The file's opening comment, if any, introduces it.
    let intro: Vec<&str> = text.lines().take_while(|l| l.trim_start().starts_with('#')).map(|l| l.trim_start()[1..].trim()).collect();
    if !intro.is_empty() {
        out.push('\n');
        out.push_str(&intro.join("\n"));
        out.push('\n');
    }
    for (heading, kind) in [("Types", "type"), ("Functions", "function")] {
        let these: Vec<&Decl> = decls.iter().filter(|d| d.kind == kind).collect();
        if these.is_empty() {
            continue;
        }
        out.push_str(&format!("\n## {}\n", heading));
        for d in these {
            out.push_str(&format!("\n### `{}`\n\n{}\n", d.name, markdown(d)));
        }
    }
    let tests: Vec<String> = prog
        .items
        .iter()
        .filter_map(|i| match i {
            Item::Test(t) => Some(format!("- test \"{}\"", t.name)),
            Item::Property(p) => Some(format!("- property \"{}\"", p.name)),
            _ => None,
        })
        .collect();
    if !tests.is_empty() {
        out.push_str(&format!("\n## Tests\n\n{}\n", tests.join("\n")));
    }
    Ok(out)
}
