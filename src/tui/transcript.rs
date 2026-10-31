//! An agent's transcript as the UI sees it: entries built from live events (or a restored
//! conversation), each rendered to wrapped lines that are cached until the entry or the
//! width changes. Streaming only touches the last entry, so a frame re-wraps almost
//! nothing.

use std::collections::HashMap;

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;

use super::markdown;
use super::theme::Theme;
use super::wrap::wrap;
use crate::protocol::{EventKind, Level};

const TOOL_PREVIEW_LINES: usize = 3;
const DIFF_PREVIEW_LINES: usize = 10;
const MESSAGE_PREVIEW_LINES: usize = 4;
const EXPANDED_MAX_LINES: usize = 500;

pub enum Kind {
    User(String),
    Assistant(String),
    Progress(String),
    Tool {
        name: String,
        input: Value,
        result: Option<String>,
        is_error: bool,
        running: bool,
    },
    Injected {
        from: String,
        text: String,
        directive: bool,
    },
    Feed(String),
    Notice {
        level: Level,
        text: String,
    },
}

pub struct Entry {
    pub kind: Kind,
    pub rev: u64,
    pub expanded: bool,
}

struct Cached {
    rev: u64,
    width: usize,
    expanded: bool,
    lines: Vec<Line<'static>>,
}

#[derive(Default)]
pub struct Transcript {
    pub entries: Vec<Entry>,
    cache: Vec<Option<Cached>>,
    open_text: Option<usize>,
    open_progress: Option<usize>,
    tools: HashMap<String, usize>,
    next_rev: u64,
}

impl Transcript {
    fn rev(&mut self) -> u64 {
        self.next_rev += 1;
        self.next_rev
    }

    pub fn push(&mut self, kind: Kind) -> usize {
        let rev = self.rev();
        self.entries.push(Entry {
            kind,
            rev,
            expanded: false,
        });
        self.cache.push(None);
        self.entries.len() - 1
    }

    pub fn touch(&mut self, i: usize) {
        let rev = self.rev();
        if let Some(e) = self.entries.get_mut(i) {
            e.rev = rev;
        }
    }

    pub fn toggle(&mut self, i: usize) {
        if let Some(e) = self.entries.get_mut(i) {
            e.expanded = !e.expanded;
        }
    }

    fn close_open(&mut self) {
        self.open_text = None;
        self.open_progress = None;
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.cache.clear();
        self.tools.clear();
        self.close_open();
    }

    /// Applies a live event. Returns true if anything visible changed.
    pub fn apply(&mut self, kind: &EventKind) -> bool {
        match kind {
            EventKind::UserText(text) => {
                self.close_open();
                self.push(Kind::User(text.clone()));
            }
            EventKind::Injected {
                from,
                text,
                directive,
            } => {
                self.push(Kind::Injected {
                    from: from.clone(),
                    text: text.clone(),
                    directive: *directive,
                });
            }
            EventKind::Feed(text) => {
                self.push(Kind::Feed(text.clone()));
            }
            EventKind::TextDelta(delta) => {
                self.open_progress = None;
                match self.open_text {
                    Some(i) => {
                        if let Kind::Assistant(text) = &mut self.entries[i].kind {
                            text.push_str(delta);
                        }
                        self.touch(i);
                    }
                    None => {
                        let i = self.push(Kind::Assistant(delta.clone()));
                        self.open_text = Some(i);
                    }
                }
            }
            EventKind::ProgressDelta(delta) => {
                self.open_text = None;
                match self.open_progress {
                    Some(i) => {
                        if let Kind::Progress(text) = &mut self.entries[i].kind {
                            text.push_str(delta);
                        }
                        self.touch(i);
                    }
                    None => {
                        if delta.trim().is_empty() {
                            return false;
                        }
                        let i = self.push(Kind::Progress(delta.clone()));
                        self.open_progress = Some(i);
                    }
                }
            }
            EventKind::BlockDone(block) => match block["type"].as_str() {
                Some("text") => {
                    if self.open_text.is_none() {
                        let text = block["text"].as_str().unwrap_or("").to_string();
                        if !text.is_empty() {
                            self.push(Kind::Assistant(text));
                        }
                    }
                    self.open_text = None;
                }
                Some("thinking") => self.open_progress = None,
                Some("tool_use") => {
                    self.close_open();
                    self.push_tool(block);
                }
                _ => return false,
            },
            EventKind::ToolStart { id, .. } => {
                if let Some(&i) = self.tools.get(id) {
                    if let Kind::Tool { running, .. } = &mut self.entries[i].kind {
                        *running = true;
                    }
                    self.touch(i);
                }
            }
            EventKind::ToolDone {
                id,
                content,
                is_error,
            } => {
                if let Some(&i) = self.tools.get(id) {
                    if let Kind::Tool {
                        result,
                        running,
                        is_error: err,
                        ..
                    } = &mut self.entries[i].kind
                    {
                        *result = Some(content.clone());
                        *running = false;
                        *err = *is_error;
                    }
                    self.touch(i);
                }
            }
            EventKind::ResponseDiscarded => {
                self.close_open();
                self.push(Kind::Notice {
                    level: Level::Warn,
                    text: "interrupted".into(),
                });
            }
            EventKind::TurnEnd => {
                self.close_open();
                return false;
            }
            EventKind::Cleared => {
                self.clear();
                self.push(Kind::Notice {
                    level: Level::Info,
                    text: "conversation cleared".into(),
                });
            }
            EventKind::Notice { level, text } => {
                self.push(Kind::Notice {
                    level: *level,
                    text: text.clone(),
                });
            }
            EventKind::History(messages) => self.load_history(messages),
            EventKind::Pending(blocks) => {
                for block in blocks {
                    self.push_user_block(block);
                }
            }
            _ => return false,
        }
        true
    }

    fn push_tool(&mut self, block: &Value) {
        let id = block["id"].as_str().unwrap_or("").to_string();
        let i = self.push(Kind::Tool {
            name: block["name"].as_str().unwrap_or("?").to_string(),
            input: block["input"].clone(),
            result: None,
            is_error: false,
            running: false,
        });
        self.tools.insert(id, i);
    }

    fn push_user_block(&mut self, block: &Value) {
        match block["type"].as_str() {
            Some("text") => {
                let text = block["text"].as_str().unwrap_or("");
                if text == "(continue)" {
                    return;
                }
                if let Some((from, body)) = parse_message(text) {
                    self.push(Kind::Injected {
                        from,
                        text: body,
                        directive: false,
                    });
                } else if text.starts_with("[activity]") {
                    self.push(Kind::Feed(text.to_string()));
                } else {
                    self.push(Kind::User(text.to_string()));
                }
            }
            Some("tool_result") => {
                let id = block["tool_use_id"].as_str().unwrap_or("");
                let content = match &block["content"] {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                let is_error = block["is_error"].as_bool().unwrap_or(false);
                self.apply(&EventKind::ToolDone {
                    id: id.to_string(),
                    content,
                    is_error,
                });
            }
            _ => {}
        }
    }

    /// Rebuilds entries from a saved conversation.
    pub fn load_history(&mut self, messages: &[Value]) {
        for m in messages {
            match m["role"].as_str() {
                Some("user") => {
                    if let Some(blocks) = m["content"].as_array() {
                        for b in blocks {
                            self.push_user_block(b);
                        }
                    }
                }
                Some("system") => {
                    let text = m["content"].as_str().unwrap_or("");
                    for note in text.split("\n\n[") {
                        let note = note.trim_start_matches('[');
                        let (from, body) = match note.split_once("] ") {
                            Some((f, b)) => (f.to_string(), b.to_string()),
                            None => ("grove".to_string(), note.to_string()),
                        };
                        self.push(Kind::Injected {
                            from,
                            text: body,
                            directive: true,
                        });
                    }
                }
                Some("assistant") => {
                    if let Some(blocks) = m["content"].as_array() {
                        for b in blocks {
                            match b["type"].as_str() {
                                Some("text") => {
                                    let t = b["text"].as_str().unwrap_or("");
                                    if !t.is_empty() {
                                        self.push(Kind::Assistant(t.to_string()));
                                    }
                                }
                                Some("thinking") => {
                                    let t = b["thinking"].as_str().unwrap_or("");
                                    if !t.trim().is_empty() {
                                        self.push(Kind::Progress(t.to_string()));
                                    }
                                }
                                Some("tool_use") => self.push_tool(b),
                                _ => {}
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        self.close_open();
    }

    /// Makes sure every entry's lines are cached for `width`; returns each entry's height
    /// (including the blank line after it).
    pub fn layout(&mut self, width: usize, theme: &Theme) -> Vec<usize> {
        let mut heights = Vec::with_capacity(self.entries.len());
        for i in 0..self.entries.len() {
            let entry = &self.entries[i];
            let fresh = match &self.cache[i] {
                Some(c) => c.rev == entry.rev && c.width == width && c.expanded == entry.expanded,
                None => false,
            };
            if !fresh {
                let lines = render_entry(entry, width, theme);
                self.cache[i] = Some(Cached {
                    rev: entry.rev,
                    width,
                    expanded: entry.expanded,
                    lines,
                });
            }
            let n = self.cache[i].as_ref().map(|c| c.lines.len()).unwrap_or(0);
            heights.push(n + 1);
        }
        heights
    }

    /// Cached lines of an entry (after `layout`).
    pub fn lines(&self, i: usize) -> &[Line<'static>] {
        match self.cache.get(i) {
            Some(Some(c)) => &c.lines,
            _ => &[],
        }
    }

    /// Plain text of an entry, for copying.
    pub fn plain(&self, i: usize) -> String {
        let Some(entry) = self.entries.get(i) else {
            return String::new();
        };
        match &entry.kind {
            Kind::User(t) | Kind::Assistant(t) | Kind::Progress(t) | Kind::Feed(t) => t.clone(),
            Kind::Injected { text, .. } | Kind::Notice { text, .. } => text.clone(),
            Kind::Tool {
                name,
                input,
                result,
                ..
            } => {
                let mut s = format!("{name}({})", tool_summary(name, input));
                if let Some(r) = result {
                    s.push('\n');
                    s.push_str(r);
                }
                s
            }
        }
    }
}

fn parse_message(text: &str) -> Option<(String, String)> {
    let rest = text.strip_prefix("<message from=\"")?;
    let (from, rest) = rest.split_once("\">")?;
    let body = rest
        .trim_start_matches('\n')
        .trim_end_matches("</message>")
        .trim_end();
    Some((from.to_string(), body.to_string()))
}

fn clip(s: &str, max: usize) -> String {
    let first = s.lines().next().unwrap_or("");
    let mut out: String = first.chars().take(max).collect();
    if first.chars().count() > max || s.lines().nth(1).is_some() {
        out.push('…');
    }
    out
}

pub fn tool_summary(name: &str, input: &Value) -> String {
    let s = |k: &str| input[k].as_str().unwrap_or("").to_string();
    match name {
        "bash" => clip(&s("command"), 90),
        "read_file" | "write_file" | "edit_file" | "list_dir" => s("path"),
        "send_message" => format!("→ {}: {}", s("to"), clip(&s("text"), 60)),
        "spawn_agent" => format!("{}: {}", s("name"), clip(&s("task"), 60)),
        "interrupt_agent" | "stop_agent" => format!("{}: {}", s("agent"), clip(&s("reason"), 60)),
        "notify_user" => clip(&s("text"), 80),
        "request_upgrade" => clip(&s("summary"), 80),
        _ => {
            let mut j = input.to_string();
            if j == "{}" {
                j.clear();
            }
            clip(&j, 80)
        }
    }
}

fn display_name(name: &str) -> String {
    match name {
        "bash" => "Bash".into(),
        "read_file" => "Read".into(),
        "write_file" => "Write".into(),
        "edit_file" => "Edit".into(),
        "list_dir" => "List".into(),
        "spawn_agent" => "Spawn".into(),
        "send_message" => "Message".into(),
        "list_agents" => "Agents".into(),
        other => other.to_string(),
    }
}

/// Wraps text lines with a first-line prefix and a hanging indent of the same width.
fn block(
    out: &mut Vec<Line<'static>>,
    text: &str,
    first: Vec<Span<'static>>,
    style: Style,
    row: Style,
    width: usize,
    max_lines: usize,
) -> usize {
    let mut prefix_w = 0;
    for s in &first {
        prefix_w += unicode_width::UnicodeWidthStr::width(s.content.as_ref());
    }
    let mut shown = 0;
    let mut total = 0;
    let mut first = Some(first);
    for raw in text.lines() {
        total += 1;
        if shown >= max_lines {
            continue;
        }
        let mut spans = match first.take() {
            Some(f) => f,
            None => vec![Span::raw(" ".repeat(prefix_w))],
        };
        spans.push(Span::styled(raw.replace('\t', "    "), style));
        for l in wrap(&Line::from(spans).style(row), width, prefix_w) {
            if shown < max_lines {
                out.push(l);
                shown += 1;
            }
        }
    }
    if total == 0
        && let Some(f) = first
    {
        out.push(Line::from(f).style(row));
    }
    total
}

fn more_hint(out: &mut Vec<Line<'static>>, hidden: usize, theme: &Theme) {
    if hidden > 0 {
        out.push(Line::from(Span::styled(
            format!("     … +{hidden} lines (enter to expand)"),
            theme.dim(),
        )));
    }
}

fn render_entry(entry: &Entry, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let width = width.max(10);
    match &entry.kind {
        Kind::User(text) => {
            let row = Style::new().bg(theme.user_bg);
            let first = vec![Span::styled(
                "❯ ",
                theme.accent().add_modifier(Modifier::BOLD),
            )];
            block(&mut out, text, first, Style::new(), row, width, usize::MAX);
        }
        Kind::Assistant(text) => {
            let md = markdown::render(text, theme);
            let mut first = true;
            for m in md {
                let mut spans = vec![if first {
                    Span::styled("● ", Style::new().fg(theme.fg))
                } else {
                    Span::raw("  ")
                }];
                first = false;
                let row = m.line.style;
                spans.extend(m.line.spans);
                for l in wrap(&Line::from(spans).style(row), width, 2 + m.indent) {
                    out.push(l);
                }
            }
        }
        Kind::Progress(text) => {
            let style = theme.dim().add_modifier(Modifier::ITALIC);
            let first = vec![Span::styled("∴ ", theme.dim())];
            let max = if entry.expanded {
                EXPANDED_MAX_LINES
            } else {
                6
            };
            let total = block(
                &mut out,
                text.trim(),
                first,
                style,
                Style::new(),
                width,
                max,
            );
            more_hint(&mut out, total.saturating_sub(max), theme);
        }
        Kind::Tool {
            name,
            input,
            result,
            is_error,
            running,
            ..
        } => {
            let dot_style = match (result.is_some(), *is_error, *running) {
                (true, true, _) => Style::new().fg(theme.red),
                (true, false, _) => Style::new().fg(theme.green),
                (false, _, true) => Style::new().fg(theme.accent),
                _ => theme.dim(),
            };
            let header = vec![
                Span::styled("● ", dot_style),
                Span::styled(display_name(name), theme.bold()),
                Span::styled(format!("({})", tool_summary(name, input)), theme.muted()),
            ];
            for l in wrap(&Line::from(header), width, 2) {
                out.push(l);
            }
            render_tool_body(
                &mut out,
                entry,
                name,
                input,
                result.as_deref(),
                *is_error,
                *running,
                width,
                theme,
            );
        }
        Kind::Injected {
            from,
            text,
            directive,
        } => {
            let (glyph, bg, color) = if *directive {
                ("⚑ ", theme.warn_bg, theme.yellow)
            } else if from == "memory" {
                ("✦ ", theme.memory_bg, theme.cyan)
            } else {
                ("✉ ", theme.message_bg, theme.magenta)
            };
            let row = Style::new().bg(bg);
            let label = if *directive {
                format!("{from} (operator) ")
            } else {
                format!("{from} ")
            };
            let first = vec![
                Span::styled(glyph, Style::new().fg(color)),
                Span::styled(label, Style::new().fg(color).add_modifier(Modifier::BOLD)),
            ];
            let max = if entry.expanded {
                EXPANDED_MAX_LINES
            } else {
                MESSAGE_PREVIEW_LINES
            };
            let total = block(&mut out, text, first, Style::new(), row, width, max);
            more_hint(&mut out, total.saturating_sub(max), theme);
        }
        Kind::Feed(text) => {
            let n = text.lines().count().saturating_sub(1);
            out.push(Line::from(vec![
                Span::styled("↳ ", theme.dim()),
                Span::styled(
                    format!("activity · {n} event{}", if n == 1 { "" } else { "s" }),
                    theme.dim(),
                ),
            ]));
            if entry.expanded {
                for l in text.lines().skip(1) {
                    for w in wrap(
                        &Line::from(Span::styled(format!("  {l}"), theme.dim())),
                        width,
                        4,
                    ) {
                        out.push(w);
                    }
                }
            }
        }
        Kind::Notice { level, text } => {
            let (glyph, style, row) = match level {
                Level::Alert => (
                    "▲ ",
                    Style::new().fg(theme.red).add_modifier(Modifier::BOLD),
                    Style::new().bg(theme.alert_bg),
                ),
                Level::Warn => ("! ", Style::new().fg(theme.yellow), Style::new()),
                Level::Info => ("· ", theme.dim(), Style::new()),
            };
            let max = if entry.expanded {
                EXPANDED_MAX_LINES
            } else {
                8
            };
            let total = block(
                &mut out,
                text,
                vec![Span::styled(glyph, style)],
                style,
                row,
                width,
                max,
            );
            more_hint(&mut out, total.saturating_sub(max), theme);
        }
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn render_tool_body(
    out: &mut Vec<Line<'static>>,
    entry: &Entry,
    name: &str,
    input: &Value,
    result: Option<&str>,
    is_error: bool,
    running: bool,
    width: usize,
    theme: &Theme,
) {
    let elbow = || vec![Span::styled("  ⎿  ", theme.dim())];
    // Edits show as a diff, whatever the result says.
    if name == "edit_file" && !is_error {
        let old = input["old_string"].as_str().unwrap_or("");
        let new = input["new_string"].as_str().unwrap_or("");
        let max = if entry.expanded {
            EXPANDED_MAX_LINES
        } else {
            DIFF_PREVIEW_LINES
        };
        let mut shown = 0;
        let mut total: usize = 0;
        for (sign, text, bg, fg) in [
            ("-", old, theme.del_bg, theme.red),
            ("+", new, theme.add_bg, theme.green),
        ] {
            for l in text.lines() {
                total += 1;
                if shown >= max {
                    continue;
                }
                let row = Style::new().bg(bg);
                let spans = vec![
                    Span::raw("     "),
                    Span::styled(format!("{sign} "), Style::new().fg(fg)),
                    Span::raw(l.replace('\t', "    ")),
                ];
                for w in wrap(&Line::from(spans).style(row), width, 7) {
                    out.push(w);
                }
                shown += 1;
            }
        }
        more_hint(out, total.saturating_sub(max), theme);
        return;
    }
    let Some(result) = result else {
        let text = if running { "running…" } else { "waiting…" };
        out.push(Line::from(vec![
            elbow()[0].clone(),
            Span::styled(text, theme.dim()),
        ]));
        return;
    };
    let style = if is_error {
        Style::new().fg(theme.red)
    } else {
        theme.muted()
    };
    let body = match name {
        "write_file" if !is_error => {
            let content = input["content"].as_str().unwrap_or("");
            let mut s = result.to_string();
            if entry.expanded {
                s.push('\n');
                s.push_str(content);
            }
            s
        }
        "bash" if !is_error => bash_output(result),
        _ => result.trim_end().to_string(),
    };
    let max = if entry.expanded {
        EXPANDED_MAX_LINES
    } else {
        TOOL_PREVIEW_LINES
    };
    let total = block(out, &body, elbow(), style, Style::new(), width, max);
    let hidden = total.saturating_sub(max);
    more_hint(out, hidden, theme);
}

/// A successful command's output without the "exit code: 0" and section headers the
/// model gets: just stdout, then stderr.
fn bash_output(result: &str) -> String {
    let mut out = String::new();
    for line in result.lines() {
        if line == "exit code: 0" || line == "stdout:" || line == "stderr:" {
            continue;
        }
        if out.is_empty() && line.is_empty() {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    let out = out.trim_end().to_string();
    if out.is_empty() {
        "(no output)".to_string()
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn bash_output_drops_boilerplate() {
        assert_eq!(bash_output("exit code: 0\n\nstdout:\nhi\n"), "hi");
        assert_eq!(bash_output("exit code: 0"), "(no output)");
    }

    #[test]
    fn rebuilds_from_history() {
        let mut t = Transcript::default();
        let messages = vec![
            json!({"role":"user","content":[{"type":"text","text":"hello"}]}),
            json!({"role":"assistant","content":[{"type":"text","text":"hi"},{"type":"tool_use","id":"t1","name":"bash","input":{"command":"ls"}}]}),
            json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"a\nb"},{"type":"text","text":"<message from=\"memory\">\nlikes rust\n</message>"}]}),
            json!({"role":"system","content":"[supervisor] careful"}),
        ];
        t.load_history(&messages);
        assert_eq!(t.entries.len(), 5);
        assert!(matches!(&t.entries[0].kind, Kind::User(s) if s == "hello"));
        assert!(matches!(&t.entries[2].kind, Kind::Tool { result: Some(r), .. } if r == "a\nb"));
        assert!(matches!(&t.entries[3].kind, Kind::Injected { from, .. } if from == "memory"));
        assert!(
            matches!(&t.entries[4].kind, Kind::Injected { from, directive: true, .. } if from == "supervisor")
        );
    }

    #[test]
    fn streaming_reuses_the_open_entry() {
        let mut t = Transcript::default();
        t.apply(&EventKind::TextDelta("Hel".into()));
        t.apply(&EventKind::TextDelta("lo".into()));
        t.apply(&EventKind::BlockDone(json!({"type":"text","text":"Hello"})));
        assert_eq!(t.entries.len(), 1);
        assert!(matches!(&t.entries[0].kind, Kind::Assistant(s) if s == "Hello"));
        let heights = t.layout(40, &Theme::dark());
        assert_eq!(heights, vec![2]);
    }
}
