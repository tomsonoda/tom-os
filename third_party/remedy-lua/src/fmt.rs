//! The formatter: re-indents source by its block structure, keeping every token and
//! comment as written. Lines inside long strings and long comments are left untouched.

#[allow(unused_imports)]
use crate::nostd::prelude::*;
use crate::Dialect;
use crate::diag::Diagnostic;
use crate::lexer::{Lexer, Tok, Token};

/// For each line: (starts inside a token or long comment, ends inside one).
///
/// From the tokens' spans (S5C-FRONT-4): scanning the text for quotes again missed a
/// backtick string's raw line breaks, so lines inside one were re-indented and the
/// string changed. Only the gaps between tokens are scanned, for long comments.
fn multiline_spans(src: &[u8], toks: &[Token]) -> Vec<(bool, bool)> {
    // Whether each line break lies inside a token or a long comment.
    let mut inside = Vec::new();
    let at = |i: usize| src.get(i).copied().unwrap_or(0);
    // Marks the line breaks in `a..b` as inside (`b` is past the last byte).
    let mark = |a: usize, b: usize, inside_now: bool, inside: &mut Vec<bool>| {
        for &c in src.get(a..b).unwrap_or_default() {
            if c == b'\n' {
                inside.push(inside_now);
            }
        }
    };
    let mut i = 0;
    for t in toks {
        let (a, b) = (t.span.0.max(i), t.span.1.max(t.span.0).max(i));
        // The gap before the token: blanks and comments.
        while i < a {
            if at(i) == b'-' && at(i + 1) == b'-' {
                let mut j = i + 2;
                if at(j) == b'[' {
                    let mut k = j + 1;
                    while at(k) == b'=' {
                        k += 1;
                    }
                    if at(k) == b'[' {
                        // A long comment: up to its closing bracket of the same level.
                        let level = k - j - 1;
                        let mut end = k + 1;
                        while end < a {
                            if at(end) == b']' {
                                let mut e = end + 1;
                                while at(e) == b'=' {
                                    e += 1;
                                }
                                if e - end - 1 == level && at(e) == b']' {
                                    end = e + 1;
                                    break;
                                }
                            }
                            end += 1;
                        }
                        let end = end.min(a);
                        mark(i, end, true, &mut inside);
                        i = end;
                        continue;
                    }
                }
                while j < a && at(j) != b'\n' {
                    j += 1;
                }
                i = j;
                continue;
            }
            if at(i) == b'\n' {
                inside.push(false);
            }
            i += 1;
        }
        mark(a, b, true, &mut inside);
        i = b;
    }
    mark(i, src.len(), false, &mut inside);
    let mut out = Vec::with_capacity(inside.len() + 1);
    let mut starts = false;
    for ends in inside.iter().copied().chain(crate::nostd::iter::once(false)) {
        out.push((starts, ends));
        starts = ends;
    }
    out
}

/// What Lua skips between tokens on a line.
const BLANKS: &[char] = &[' ', '\t', '\x0b', '\x0c'];

pub(crate) fn format(source: &str, name: &str, dialect: Dialect) -> Result<String, Diagnostic> {
    // It must parse: formatting broken code would only move the breakage.
    crate::parser::parse_within(source.as_bytes(), name, dialect, crate::parser::host_memory(source.len()))?;
    let toks = Lexer::new(source.as_bytes(), name, dialect).tokens()?;
    let spans = multiline_spans(source.as_bytes(), &toks);
    let lines: Vec<&str> = source.split('\n').collect();
    // Depth at the start of each line, and whether its first token closes a block.
    let mut depth_at: Vec<i64> = vec![-1; lines.len() + 1];
    let mut first_closes: Vec<bool> = vec![false; lines.len() + 1];
    let mut depth: i64 = 0;
    let mut last_line = 0u32;
    for t in &toks {
        let line = t.pos.line as usize;
        if t.pos.line != last_line {
            if let Some(d) = depth_at.get_mut(line) {
                *d = depth;
            }
            if let Some(f) = first_closes.get_mut(line) {
                *f = matches!(
                    t.tok,
                    Tok::End | Tok::Until | Tok::RBrace | Tok::RParen | Tok::RBracket | Tok::Else | Tok::Elseif
                );
            }
            last_line = t.pos.line;
        }
        match t.tok {
            Tok::Function | Tok::Do | Tok::Then | Tok::Repeat | Tok::LBrace | Tok::LParen | Tok::LBracket => depth += 1,
            Tok::End | Tok::Until | Tok::RBrace | Tok::RParen | Tok::RBracket | Tok::Elseif => depth -= 1,
            Tok::Else => {}
            _ => {}
        }
    }
    // Lines without tokens (comments) take the depth of the next line of code.
    let mut next_known = 0;
    for d in depth_at.iter_mut().rev() {
        if *d < 0 {
            *d = next_known;
        } else {
            next_known = *d;
        }
    }
    // The output is bounded like the parse (S6-FRONT-2): four spaces a level on every
    // line made `"do\n" * n .. "end\n" * n` (112 KB at n = 16,000) a 1 GB result.
    let room = crate::parser::host_memory(source.len());
    let mut out = String::with_capacity(source.len());
    for (i, line) in lines.iter().enumerate() {
        let lno = i + 1;
        let (starts_inside, ends_inside) = spans.get(i).copied().unwrap_or((false, false));
        // The `\r` of a `\r\n` goes, unless it is inside a token or another `\r` comes
        // before it (`\r\r\n` is two line breaks, `\r\n` one).
        let line = match line.strip_suffix('\r') {
            Some(l) if !ends_inside && !l.ends_with('\r') => l,
            _ => line,
        };
        if i > 0 {
            out.push('\n');
        }
        if starts_inside {
            out.push_str(line);
            continue;
        }
        // Only blanks (S5C-FRONT-11): a lone `\r` is a line break to Lua, which `str::trim`
        // removed, moving the lines after it up. Nor the blanks that keep a `\r` from the
        // `\n` beside it: `\n\r` and `\r\n` are one line break, not two.
        let mut trimmed = line.trim_start_matches(BLANKS);
        if trimmed.starts_with('\r') {
            trimmed = line;
        }
        if !ends_inside && !trimmed.trim_end_matches(BLANKS).ends_with('\r') {
            trimmed = trimmed.trim_end_matches(BLANKS);
        }
        if trimmed.is_empty() {
            continue;
        }
        let d = depth_at.get(lno).copied().unwrap_or(0);
        let d = if first_closes.get(lno).copied().unwrap_or(false) { d - 1 } else { d };
        let indent = usize::try_from(d.max(0)).unwrap_or(usize::MAX).saturating_mul(4);
        if out.len().saturating_add(indent).saturating_add(trimmed.len()) > room {
            let pos = crate::diag::Pos { line: u32::try_from(lno).unwrap_or(u32::MAX), col: 1 };
            return Err(Diagnostic::error(name, pos, "not enough memory"));
        }
        out.extend(crate::nostd::iter::repeat_n("    ", indent / 4));
        out.push_str(trimmed);
    }
    Ok(out)
}
