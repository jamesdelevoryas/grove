//! Word wrapping for styled lines, measured in terminal cells.

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Wraps `line` to `width` cells. Continuation lines start with `indent` spaces (so list
/// items and prefixed blocks hang nicely). The line's own style carries over to every
/// piece, which is how full-width backgrounds survive wrapping.
pub fn wrap(line: &Line<'static>, width: usize, indent: usize) -> Vec<Line<'static>> {
    let width = width.max(8);
    let indent = indent.min(width / 2);
    let mut out = Vec::new();
    let mut cur: Vec<Span<'static>> = Vec::new();
    let mut cur_w = 0usize;
    let mut line_start = 0usize; // width of the indent on the current line

    for span in &line.spans {
        for token in tokens(&span.content) {
            let tw = token.width();
            let blank = token.chars().all(char::is_whitespace);
            if cur_w + tw > width && cur_w > line_start {
                out.push(finish(&mut cur, line.style));
                if indent > 0 {
                    cur.push(Span::raw(" ".repeat(indent)));
                }
                cur_w = indent;
                line_start = indent;
                if blank {
                    continue;
                }
            }
            if cur_w + tw <= width {
                push(&mut cur, token, span.style);
                cur_w += tw;
                continue;
            }
            // A word longer than a whole line: break it anywhere.
            for ch in token.chars() {
                let cw = ch.width().unwrap_or(0);
                if cur_w + cw > width && cur_w > line_start {
                    out.push(finish(&mut cur, line.style));
                    if indent > 0 {
                        cur.push(Span::raw(" ".repeat(indent)));
                    }
                    cur_w = indent;
                    line_start = indent;
                }
                let mut buf = [0u8; 4];
                push(&mut cur, ch.encode_utf8(&mut buf), span.style);
                cur_w += cw;
            }
        }
    }
    if !cur.is_empty() || out.is_empty() {
        out.push(finish(&mut cur, line.style));
    }
    out
}

fn finish(cur: &mut Vec<Span<'static>>, style: Style) -> Line<'static> {
    let spans = std::mem::take(cur);
    Line::from(spans).style(style)
}

/// Appends text, merging into the previous span when the style matches.
fn push(cur: &mut Vec<Span<'static>>, text: &str, style: Style) {
    if let Some(last) = cur.last_mut()
        && last.style == style
    {
        last.content.to_mut().push_str(text);
        return;
    }
    cur.push(Span::styled(text.to_string(), style));
}

/// Splits into alternating runs of whitespace and non-whitespace.
fn tokens(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut prev_ws: Option<bool> = None;
    for (i, ch) in s.char_indices() {
        let ws = ch.is_whitespace();
        if let Some(p) = prev_ws
            && p != ws
        {
            out.push(&s[start..i]);
            start = i;
        }
        prev_ws = Some(ws);
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

pub fn line_width(line: &Line) -> usize {
    let mut w = 0;
    for span in &line.spans {
        w += span.content.width();
    }
    w
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: &[Line]) -> Vec<String> {
        let mut out = Vec::new();
        for l in lines {
            let mut s = String::new();
            for span in &l.spans {
                s.push_str(&span.content);
            }
            out.push(s);
        }
        out
    }

    #[test]
    fn wraps_words_with_hanging_indent() {
        let line = Line::from("- alpha beta gamma delta");
        let wrapped = wrap(&line, 12, 2);
        assert_eq!(text(&wrapped), vec!["- alpha beta", "  gamma ", "  delta"]);
        for l in &wrapped {
            assert!(line_width(l) <= 12);
        }
    }

    #[test]
    fn breaks_long_words() {
        let line = Line::from("abcdefghijklmnopqrstuvwxyz");
        let wrapped = wrap(&line, 10, 0);
        assert_eq!(text(&wrapped), vec!["abcdefghij", "klmnopqrst", "uvwxyz"]);
    }

    #[test]
    fn wide_chars_count_double() {
        let line = Line::from("日本語日本語");
        for l in wrap(&line, 8, 0) {
            assert!(line_width(&l) <= 8);
        }
    }
}
