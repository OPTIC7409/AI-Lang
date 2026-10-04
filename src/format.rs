//! `cogito fmt`: the canonical layout of Cogito source code.
//!
//! The formatter changes only whitespace. It keeps every token's original
//! text and every comment, recomputes indentation (two spaces per open
//! bracket, plus one level for continuation lines), normalizes the spacing
//! between tokens on a line, and allows at most one blank line in a row.
//! Line breaks stay where the author put them, so statement boundaries never
//! move. As a safety net, the result is lexed again and must produce exactly
//! the same tokens as the input.

use crate::diagnostic::Diagnostic;
use crate::lexer::{lex, Tok, Token};
use std::mem::discriminant;

const INDENT: &str = "  ";

/// One piece of the source: a token, or a comment (which the lexer skips).
struct Item<'a> {
    tok: Option<&'a Tok>,
    start: usize,
    end: usize,
}

/// Format a whole source file.
pub fn format_source(src: &str) -> Result<String, Diagnostic> {
    let toks = lex(src, 0, 0, src.len())?;
    let items = items(src, &toks);
    let out = layout(src, &items);
    // Safety net: the same tokens, with the same text, in the same order.
    let again = lex(&out, 0, 0, out.len()).map_err(|_| internal("the formatted code no longer lexes"))?;
    if signature(src, &toks) != signature(&out, &again) {
        return Err(internal("formatting would change the meaning of the code"));
    }
    Ok(out)
}

fn internal(why: &str) -> Diagnostic {
    Diagnostic::error("E0001", format!("cannot format this file: {}", why)).help("this is a bug in `cogito fmt`; the file was left unchanged")
}

fn signature<'a>(src: &'a str, toks: &[Token]) -> Vec<(std::mem::Discriminant<Tok>, &'a str)> {
    toks.iter()
        .filter(|t| !matches!(t.tok, Tok::Newline | Tok::Eof))
        .map(|t| (discriminant(&t.tok), &src[t.span.start as usize..t.span.end as usize]))
        .collect()
}

/// Tokens and comments, in source order.
fn items<'a>(src: &str, toks: &'a [Token]) -> Vec<Item<'a>> {
    let mut out = Vec::new();
    let mut pos = 0;
    let add_comments = |from: usize, to: usize, out: &mut Vec<Item<'a>>| {
        // Comments are the only non-whitespace text between tokens.
        let gap = &src[from..to];
        let mut i = 0;
        while let Some(rel) = gap[i..].find('#') {
            let start = i + rel;
            let end = gap[start..].find('\n').map_or(gap.len(), |e| start + e);
            out.push(Item { tok: None, start: from + start, end: from + end });
            i = end;
        }
    };
    for t in toks {
        if matches!(t.tok, Tok::Newline | Tok::Eof) {
            continue;
        }
        let (s, e) = (t.span.start as usize, t.span.end as usize);
        if s < pos {
            continue;
        }
        add_comments(pos, s, &mut out);
        out.push(Item { tok: Some(&t.tok), start: s, end: e });
        pos = e;
    }
    add_comments(pos, src.len(), &mut out);
    out
}

fn is_opener(t: &Tok) -> bool {
    matches!(t, Tok::LParen | Tok::LBracket | Tok::LBrace)
}

fn is_closer(t: &Tok) -> bool {
    matches!(t, Tok::RParen | Tok::RBracket | Tok::RBrace)
}

/// Tokens that, at the start of a line, continue the previous line.
fn continues_line(t: &Tok) -> bool {
    matches!(t, Tok::Dot | Tok::PipeGt | Tok::And | Tok::Or | Tok::Requires | Tok::Ensures | Tok::Bar | Tok::Where)
}

/// Tokens that, at the end of a line, make the next line a continuation.
fn wants_continuation(t: &Tok) -> bool {
    matches!(
        t,
        Tok::Assign
            | Tok::PlusAssign
            | Tok::MinusAssign
            | Tok::StarAssign
            | Tok::SlashAssign
            | Tok::PercentAssign
            | Tok::FatArrow
            | Tok::Arrow
            | Tok::Plus
            | Tok::Minus
            | Tok::Star
            | Tok::StarStar
            | Tok::Slash
            | Tok::SlashSlash
            | Tok::Percent
            | Tok::PipeGt
            | Tok::And
            | Tok::Or
            | Tok::EqEq
            | Tok::NotEq
            | Tok::Lt
            | Tok::Le
            | Tok::Gt
            | Tok::Ge
    )
}

/// Can this token end an operand (so that a following `-` is binary)?
fn ends_operand(t: &Tok) -> bool {
    matches!(
        t,
        Tok::Int(_)
            | Tok::Float(_)
            | Tok::Str(_)
            | Tok::Ident(_)
            | Tok::Upper(_)
            | Tok::True
            | Tok::False
            | Tok::RParen
            | Tok::RBracket
            | Tok::RBrace
            | Tok::Question
    )
}

/// Whether to put a space between two tokens on the same line. `before` is
/// the token before `prev`, used to tell unary from binary minus.
fn space_between(before: Option<&Tok>, prev: &Tok, next: &Tok) -> bool {
    use Tok::*;
    match (prev, next) {
        // Closing and separating punctuation hugs what precedes it.
        (_, RParen | RBracket | Comma | Semi | Question | Colon) => false,
        (LBrace, RBrace) => false,
        (_, RBrace) => true,
        (LParen | LBracket, _) => false,
        (LBrace, _) => true,
        // Member access and ranges.
        (Dot, _) | (_, Dot) => false,
        (DotDot | DotDotEq, LBrace) => true,
        (DotDot | DotDotEq, _) => false,
        (_, DotDot | DotDotEq) => !matches!(prev, Int(_) | Ident(_) | Upper(_) | RParen | RBracket | Float(_)),
        // Calls, generic arguments and indexing.
        (Ident(_) | Upper(_) | RParen | RBracket | Fn | Question, LParen) => false,
        (Ident(_) | Upper(_) | RParen | RBracket | Str(_), LBracket) => false,
        // Unary minus: no space between it and its operand.
        (Minus, _) => before.is_some_and(ends_operand),
        _ => true,
    }
}

fn layout(src: &str, items: &[Item]) -> String {
    let mut out = String::with_capacity(src.len() + 64);
    // For each open bracket, the indentation level of the line it opened on.
    let mut stack: Vec<usize> = Vec::new();
    let mut level = 0;
    let mut prev: Option<&Item> = None;
    let mut before_prev: Option<&Tok> = None;
    // Whether `prev` was the first item on its line.
    let mut prev_first = false;
    // The last token of the code so far (not a comment).
    let mut last_code: Option<&Tok> = None;
    for it in items {
        let gap = match prev {
            Some(p) => &src[p.end..it.start],
            None => &src[..it.start],
        };
        let newlines = gap.matches('\n').count();
        let first = prev.is_none() || newlines > 0;
        if first {
            if prev.is_some() {
                out.push('\n');
                if newlines > 1 {
                    out.push('\n');
                }
            }
            // A line that starts by closing brackets lines up with the line
            // that opened the first of them; any other line is indented one
            // level more than the line that opened the innermost open bracket,
            // plus one for a continuation line.
            level = match (it.tok, stack.last()) {
                (Some(t), Some(l)) if is_closer(t) => *l,
                (_, Some(l)) => l + 1,
                (_, None) => 0,
            };
            let continuation = match it.tok {
                Some(t) if is_closer(t) => false,
                Some(t) if continues_line(t) => true,
                _ => last_code.is_some_and(wants_continuation) && !matches!(last_code, Some(t) if is_opener(t)),
            };
            if continuation {
                level += 1;
            }
            for _ in 0..level {
                out.push_str(INDENT);
            }
        } else if let Some(p) = prev {
            match (p.tok, it.tok) {
                // A trailing comment keeps its column (for aligned comments),
                // with at least one space.
                (_, None) => {
                    if gap.is_empty() {
                        out.push(' ');
                    } else {
                        out.push_str(gap);
                    }
                }
                (Some(a), Some(b)) => {
                    let before = if prev_first { None } else { before_prev };
                    if space_between(before, a, b) {
                        out.push(' ');
                    }
                }
                (None, Some(_)) => out.push(' '),
            }
        }
        out.push_str(&src[it.start..it.end]);
        if let Some(t) = it.tok {
            if is_opener(t) {
                stack.push(level);
            } else if is_closer(t) {
                stack.pop();
            }
            last_code = Some(t);
            before_prev = prev.and_then(|p| p.tok);
        }
        prev_first = first;
        prev = Some(it);
    }
    while out.ends_with([' ', '\n']) {
        out.pop();
    }
    if !out.is_empty() {
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::format_source;

    fn fmt(s: &str) -> String {
        format_source(s).unwrap()
    }

    #[test]
    fn spacing_and_indentation() {
        assert_eq!(fmt("let x=1+2*3\n"), "let x = 1 + 2 * 3\n");
        assert_eq!(fmt("fn f(a:Int,b:Int)->Int{\na+b\n}\n"), "fn f(a: Int, b: Int) -> Int {\n  a + b\n}\n");
        assert_eq!(fmt("print( xs[ 0 ] , -x , a - b )\n"), "print(xs[0], -x, a - b)\n");
        assert_eq!(fmt("let r = { a : 1 , ..s }\nlet m = [:]\n"), "let r = { a: 1, ..s }\nlet m = [:]\n");
        assert_eq!(fmt("for i in 0..n {\nprint(i)\n}\n"), "for i in 0..n {\n  print(i)\n}\n");
    }

    #[test]
    fn continuation_lines() {
        let src = "let t = (1..=10)\n|> map(fn(n) => n * 2)\n|> sum\n";
        assert_eq!(fmt(src), "let t = (1..=10)\n  |> map(fn(n) => n * 2)\n  |> sum\n");
        let src = "type Shape =\n| Circle(r: Float)\n| Sq(s: Float)\n";
        assert_eq!(fmt(src), "type Shape =\n  | Circle(r: Float)\n  | Sq(s: Float)\n");
        let src = "fn f(x: Int) -> Int\nrequires x > 0\n{\nx\n}\n";
        assert_eq!(fmt(src), "fn f(x: Int) -> Int\n  requires x > 0\n{\n  x\n}\n");
    }

    #[test]
    fn comments_strings_and_blank_lines() {
        let src = "# top\n\n\n\nlet s = \"a  b {x+1}\"   # keep\nlet t = \"\"\"\n    keep   this\n  \"\"\"\n";
        assert_eq!(fmt(src), "# top\n\nlet s = \"a  b {x+1}\"   # keep\nlet t = \"\"\"\n    keep   this\n  \"\"\"\n");
    }

    #[test]
    fn contracts_and_unary_minus() {
        let src = "fn f(x: Int) -> Int\n  ensures match result {\n    0 => true\n    _ => false\n  }\n=> x\nfn g() -> Int {\n  -1\n}\nfor _ in 0.. {\n  break\n}\n";
        assert_eq!(fmt(src), src);
        assert_eq!(fmt("let x = a-b\nlet y = -b\nlet z = f(-1, - 2)\n"), "let x = a - b\nlet y = -b\nlet z = f(-1, -2)\n");
    }

    #[test]
    fn idempotent() {
        let src = "fn g(xs: List[Int]) -> Int {\n  match xs {\n    [] => 0\n    [x, ..rest] => x + g(rest)\n  }\n}\n";
        assert_eq!(fmt(src), src);
        assert_eq!(fmt(&fmt(src)), fmt(src));
    }
}
