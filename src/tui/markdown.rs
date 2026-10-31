//! Markdown to styled lines (before wrapping). Small on purpose: headings, emphasis,
//! inline code, fenced code with a tinted background, lists, quotes, rules, links.

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::theme::Theme;

/// A rendered line and the indent its wrapped continuations should hang at.
pub struct MdLine {
    pub line: Line<'static>,
    pub indent: usize,
}

struct State<'t> {
    theme: &'t Theme,
    out: Vec<MdLine>,
    cur: Vec<Span<'static>>,
    styles: Vec<Style>,
    /// One entry per open list: the next number for ordered lists.
    lists: Vec<Option<u64>>,
    quote: usize,
    in_code: bool,
    item_indent: usize,
    /// The current line begins a list item, so it carries its own indent and bullet.
    item_start: bool,
}

impl State<'_> {
    fn style(&self) -> Style {
        let mut s = Style::new();
        for st in &self.styles {
            s = s.patch(*st);
        }
        s
    }

    fn prefix(&self) -> Vec<Span<'static>> {
        let mut spans = Vec::new();
        if self.item_indent > 0 && !self.item_start {
            spans.push(Span::raw(" ".repeat(self.item_indent)));
        }
        for _ in 0..self.quote {
            spans.push(Span::styled("│ ", Style::new().fg(self.theme.dim)));
        }
        spans
    }

    fn flush(&mut self) {
        if self.cur.is_empty() {
            return;
        }
        let mut spans = self.prefix();
        self.item_start = false;
        spans.append(&mut self.cur);
        let indent = self.item_indent + 2 * self.quote;
        self.out.push(MdLine {
            line: Line::from(spans),
            indent,
        });
    }

    fn blank(&mut self) {
        if let Some(last) = self.out.last()
            && last.line.spans.is_empty()
        {
            return;
        }
        if self.out.is_empty() {
            return;
        }
        self.out.push(MdLine {
            line: Line::default(),
            indent: 0,
        });
    }

    fn text(&mut self, text: &str) {
        let style = self.style();
        self.cur.push(Span::styled(text.to_string(), style));
    }
}

pub fn render(src: &str, theme: &Theme) -> Vec<MdLine> {
    let mut st = State {
        theme,
        out: Vec::new(),
        cur: Vec::new(),
        styles: Vec::new(),
        lists: Vec::new(),
        quote: 0,
        in_code: false,
        item_indent: 0,
        item_start: false,
    };
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    opts.insert(Options::ENABLE_TABLES);
    for event in Parser::new_ext(src, opts) {
        match event {
            Event::Start(tag) => match tag {
                Tag::Paragraph => st.flush(),
                Tag::Heading { level, .. } => {
                    st.flush();
                    let mut style = Style::new().add_modifier(Modifier::BOLD);
                    if level == HeadingLevel::H1 || level == HeadingLevel::H2 {
                        style = style.fg(theme.accent);
                    }
                    st.styles.push(style);
                }
                Tag::BlockQuote(_) => {
                    st.flush();
                    st.quote += 1;
                }
                Tag::CodeBlock(kind) => {
                    st.flush();
                    st.in_code = true;
                    if let CodeBlockKind::Fenced(lang) = kind
                        && !lang.is_empty()
                    {
                        let label = Span::styled(
                            format!(" {lang} "),
                            Style::new().fg(theme.dim).bg(theme.code_bg),
                        );
                        st.out.push(MdLine {
                            line: Line::from(vec![label]).style(Style::new().bg(theme.code_bg)),
                            indent: 0,
                        });
                    }
                }
                Tag::List(start) => {
                    st.flush();
                    st.lists.push(start);
                }
                Tag::Item => {
                    st.flush();
                    let depth = st.lists.len().saturating_sub(1);
                    let bullet = match st.lists.last_mut() {
                        Some(Some(n)) => {
                            let b = format!("{n}. ");
                            *n += 1;
                            b
                        }
                        _ => "• ".to_string(),
                    };
                    let pad = " ".repeat(depth * 2);
                    st.item_indent = depth * 2 + bullet.chars().count();
                    st.item_start = true;
                    if !pad.is_empty() {
                        st.cur.push(Span::raw(pad));
                    }
                    st.cur
                        .push(Span::styled(bullet, Style::new().fg(theme.dim)));
                }
                Tag::Emphasis => st.styles.push(Style::new().add_modifier(Modifier::ITALIC)),
                Tag::Strong => st.styles.push(Style::new().add_modifier(Modifier::BOLD)),
                Tag::Strikethrough => st
                    .styles
                    .push(Style::new().add_modifier(Modifier::CROSSED_OUT)),
                Tag::Link { .. } => st.styles.push(
                    Style::new()
                        .fg(theme.blue)
                        .add_modifier(Modifier::UNDERLINED),
                ),
                Tag::TableRow | Tag::TableHead => st.flush(),
                Tag::TableCell if !st.cur.is_empty() => {
                    st.cur.push(Span::styled(" │ ", Style::new().fg(theme.dim)));
                }
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph => {
                    st.flush();
                    if st.lists.is_empty() {
                        st.blank();
                    }
                }
                TagEnd::Heading(_) => {
                    st.styles.pop();
                    st.flush();
                    st.blank();
                }
                TagEnd::BlockQuote(_) => {
                    st.flush();
                    st.quote = st.quote.saturating_sub(1);
                    st.blank();
                }
                TagEnd::CodeBlock => {
                    st.in_code = false;
                    st.blank();
                }
                TagEnd::List(_) => {
                    st.flush();
                    st.lists.pop();
                    if st.lists.is_empty() {
                        st.item_indent = 0;
                        st.blank();
                    } else {
                        st.item_indent = (st.lists.len() - 1) * 2 + 2;
                    }
                }
                TagEnd::Item => st.flush(),
                TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough | TagEnd::Link => {
                    st.styles.pop();
                }
                TagEnd::TableHead | TagEnd::TableRow => st.flush(),
                TagEnd::Table => st.blank(),
                _ => {}
            },
            Event::Text(text) => {
                if st.in_code {
                    let code = Style::new().bg(theme.code_bg);
                    for line in text.lines() {
                        let expanded = line.replace('\t', "    ");
                        st.out.push(MdLine {
                            line: Line::from(vec![Span::styled(format!(" {expanded}"), code)])
                                .style(code),
                            indent: 1,
                        });
                    }
                } else {
                    st.text(&text);
                }
            }
            Event::Code(code) => {
                let style = st.style().fg(theme.inline_code);
                st.cur.push(Span::styled(code.to_string(), style));
            }
            Event::SoftBreak => st.text(" "),
            Event::HardBreak => st.flush(),
            Event::Rule => {
                st.flush();
                st.out.push(MdLine {
                    line: Line::from(Span::styled("─".repeat(32), Style::new().fg(theme.dim))),
                    indent: 0,
                });
                st.blank();
            }
            Event::TaskListMarker(done) => {
                st.text(if done { "[x] " } else { "[ ] " });
            }
            Event::Html(html) | Event::InlineHtml(html) => st.text(&html),
            Event::InlineMath(m) | Event::DisplayMath(m) => st.text(&m),
            Event::FootnoteReference(r) => st.text(&r),
        }
    }
    st.flush();
    while let Some(last) = st.out.last() {
        if last.line.spans.is_empty() {
            st.out.pop();
        } else {
            break;
        }
    }
    st.out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(lines: &[MdLine]) -> Vec<String> {
        let mut out = Vec::new();
        for l in lines {
            let mut s = String::new();
            for span in &l.line.spans {
                s.push_str(&span.content);
            }
            out.push(s);
        }
        out
    }

    #[test]
    fn renders_basic_blocks() {
        let theme = Theme::dark();
        let md =
            "# Title\n\nSome *text* and `code`.\n\n- one\n- two\n\n```rust\nfn main() {}\n```\n";
        let lines = plain(&render(md, &theme));
        assert_eq!(lines[0], "Title");
        assert_eq!(lines[1], "");
        assert_eq!(lines[2], "Some text and code.");
        assert!(lines.contains(&"• one".to_string()), "{lines:?}");
        assert!(lines.contains(&" fn main() {}".to_string()), "{lines:?}");
    }
}
