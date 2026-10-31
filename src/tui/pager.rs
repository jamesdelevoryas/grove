//! The frozen view (Ctrl+O): a still snapshot of one agent's transcript, scrolled with
//! vi / Helix / tmux copy-mode keys, while agents keep working underneath. The mouse is
//! released, so the terminal's own drag-select and copy work on what's on screen; `v` and
//! `y` copy any range by keyboard.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::theme::Theme;

pub enum PagerAction {
    None,
    Close,
    Copy(String),
}

pub struct Pager {
    agent: String,
    lines: Vec<Line<'static>>,
    plain: Vec<String>,
    top: usize,
    cursor: usize,
    /// Anchor line of a `v` selection.
    visual: Option<usize>,
    /// The `/` query being typed.
    input: Option<String>,
    query: Option<String>,
    pending_g: bool,
    /// Content rows on screen, from the last draw.
    height: usize,
    message: Option<String>,
}

impl Pager {
    pub fn new(agent: &str, lines: Vec<Line<'static>>) -> Self {
        let mut plain = Vec::with_capacity(lines.len());
        for line in &lines {
            let mut s = String::new();
            for span in &line.spans {
                s.push_str(&span.content);
            }
            plain.push(s.trim_end().to_string());
        }
        let last = lines.len().saturating_sub(1);
        Self {
            agent: agent.to_string(),
            lines,
            plain,
            // Starts at the bottom, where you were reading; `draw` clamps this.
            top: usize::MAX,
            cursor: last,
            visual: None,
            input: None,
            query: None,
            pending_g: false,
            height: 20,
            message: None,
        }
    }

    fn last(&self) -> usize {
        self.lines.len().saturating_sub(1)
    }

    fn max_top(&self) -> usize {
        self.lines.len().saturating_sub(self.height)
    }

    fn move_cursor(&mut self, delta: isize) {
        let next = (self.cursor as isize + delta).clamp(0, self.last() as isize);
        self.cursor = next as usize;
        self.follow_cursor();
    }

    fn follow_cursor(&mut self) {
        if self.cursor < self.top {
            self.top = self.cursor;
        } else if self.cursor >= self.top + self.height {
            self.top = self.cursor + 1 - self.height;
        }
    }

    /// vi's ctrl+d / ctrl+u: the view and the cursor move together.
    fn scroll_both(&mut self, delta: isize) {
        let top = (self.top as isize + delta).clamp(0, self.max_top() as isize) as usize;
        let moved = top as isize - self.top as isize;
        self.top = top;
        if moved == 0 {
            self.move_cursor(delta);
        } else {
            self.move_cursor(moved);
        }
    }

    /// vi's ctrl+e / ctrl+y: the view moves, the cursor stays on screen.
    fn scroll_view(&mut self, delta: isize) {
        self.top = (self.top as isize + delta).clamp(0, self.max_top() as isize) as usize;
        if self.cursor < self.top {
            self.cursor = self.top;
        } else if self.cursor >= self.top + self.height {
            self.cursor = self.top + self.height - 1;
        }
    }

    fn find(&mut self, forward: bool) {
        let Some(query) = self.query.clone() else {
            return;
        };
        let q = query.to_lowercase();
        let n = self.plain.len();
        for step in 1..=n {
            let i = if forward {
                (self.cursor + step) % n
            } else {
                (self.cursor + n - step % n) % n
            };
            if self.plain[i].to_lowercase().contains(&q) {
                self.cursor = i;
                // Put the match a third of the way down, so there's context above it.
                self.top = i.saturating_sub(self.height / 3).min(self.max_top());
                self.message = None;
                return;
            }
        }
        self.message = Some(format!("not found: {query}"));
    }

    pub fn key(&mut self, key: KeyEvent) -> PagerAction {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if let Some(input) = &mut self.input {
            match key.code {
                KeyCode::Enter => {
                    let q = std::mem::take(input);
                    self.input = None;
                    if !q.is_empty() {
                        self.query = Some(q);
                    }
                    self.find(true);
                }
                KeyCode::Esc => self.input = None,
                KeyCode::Backspace => {
                    if input.pop().is_none() {
                        self.input = None;
                    }
                }
                KeyCode::Char(c) if !ctrl => input.push(c),
                _ => {}
            }
            return PagerAction::None;
        }
        let half = (self.height / 2).max(1) as isize;
        let page = self.height.saturating_sub(2).max(1) as isize;
        let g = std::mem::take(&mut self.pending_g);
        self.message = None;
        match key.code {
            KeyCode::Char('o') | KeyCode::Char('c') if ctrl => return PagerAction::Close,
            KeyCode::Char('d') if ctrl => self.scroll_both(half),
            KeyCode::Char('u') if ctrl => self.scroll_both(-half),
            KeyCode::Char('f') if ctrl => self.scroll_both(page),
            KeyCode::Char('b') if ctrl => self.scroll_both(-page),
            KeyCode::Char('e') if ctrl => self.scroll_view(1),
            KeyCode::Char('y') if ctrl => self.scroll_view(-1),
            KeyCode::Char('n') if ctrl => self.move_cursor(1),
            KeyCode::Char('p') if ctrl => self.move_cursor(-1),
            _ if ctrl => {}
            KeyCode::Char('j') | KeyCode::Down => self.move_cursor(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_cursor(-1),
            KeyCode::PageDown | KeyCode::Char(' ') => self.scroll_both(page),
            KeyCode::PageUp => self.scroll_both(-page),
            KeyCode::Char('g') if g => {
                self.cursor = 0;
                self.top = 0;
            }
            KeyCode::Char('g') => self.pending_g = true,
            KeyCode::Home => {
                self.cursor = 0;
                self.top = 0;
            }
            KeyCode::Char('G') | KeyCode::End => {
                self.cursor = self.last();
                self.top = self.max_top();
            }
            KeyCode::Char('H') => self.cursor = self.top,
            KeyCode::Char('L') => {
                self.cursor = (self.top + self.height).saturating_sub(1).min(self.last())
            }
            KeyCode::Char('M') => self.cursor = (self.top + self.height / 2).min(self.last()),
            KeyCode::Char('/') => self.input = Some(String::new()),
            KeyCode::Char('n') => self.find(true),
            KeyCode::Char('N') => self.find(false),
            KeyCode::Char('v') | KeyCode::Char('V') => {
                self.visual = match self.visual {
                    Some(_) => None,
                    None => Some(self.cursor),
                };
            }
            KeyCode::Char('y') | KeyCode::Enter => {
                let (a, b) = match self.visual.take() {
                    Some(anchor) => (anchor.min(self.cursor), anchor.max(self.cursor)),
                    None => (self.cursor, self.cursor),
                };
                let mut text = String::new();
                for (i, line) in self.plain[a..=b].iter().enumerate() {
                    if i > 0 {
                        text.push('\n');
                    }
                    text.push_str(line);
                }
                let n = b - a + 1;
                self.message = Some(format!("copied {n} line{}", if n == 1 { "" } else { "s" }));
                return PagerAction::Copy(text);
            }
            KeyCode::Esc if self.visual.is_some() => self.visual = None,
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('i') => return PagerAction::Close,
            _ => {}
        }
        PagerAction::None
    }

    pub fn draw(&mut self, buf: &mut Buffer, area: Rect, theme: &Theme) {
        if area.height < 2 {
            return;
        }
        self.height = (area.height - 1) as usize;
        self.top = self.top.min(self.max_top());
        self.follow_cursor();
        let selected = self
            .visual
            .map(|anchor| (anchor.min(self.cursor), anchor.max(self.cursor)));
        for row in 0..self.height {
            let i = self.top + row;
            let y = area.y + row as u16;
            let Some(line) = self.lines.get(i) else { break };
            let full = Rect {
                x: area.x,
                y,
                width: area.width,
                height: 1,
            };
            if line.style.bg.is_some() {
                buf.set_style(full, line.style);
            }
            buf.set_line(area.x + 1, y, line, area.width.saturating_sub(1));
            let in_visual = selected.is_some_and(|(a, b)| i >= a && i <= b);
            if in_visual || i == self.cursor {
                buf.set_style(full, Style::new().bg(theme.sel_bg));
            }
        }

        // Status bar.
        let y = area.y + area.height - 1;
        let bar = Rect {
            x: area.x,
            y,
            width: area.width,
            height: 1,
        };
        buf.set_style(bar, Style::new().bg(theme.panel_bg));
        let left = match (&self.input, &self.message) {
            (Some(q), _) => Line::from(vec![
                Span::styled(" /", theme.accent()),
                Span::raw(q.clone()),
            ]),
            (None, Some(m)) => Line::from(Span::styled(format!(" {m}"), theme.muted())),
            (None, None) => {
                let mode = if self.visual.is_some() {
                    " VISUAL "
                } else {
                    " FROZEN "
                };
                Line::from(vec![
                    Span::styled(
                        mode,
                        Style::new()
                            .fg(theme.accent)
                            .add_modifier(Modifier::BOLD | Modifier::REVERSED),
                    ),
                    Span::styled(format!(" {} ", self.agent), theme.bold()),
                    Span::styled(
                        format!("{}/{}", self.cursor + 1, self.lines.len()),
                        theme.dim(),
                    ),
                ])
            }
        };
        buf.set_line(area.x, y, &left, area.width);
        let hint = "j/k · ^d/^u · gg/G · / n N · v y copy · q back";
        let w = hint.len() as u16;
        if w + 30 < area.width {
            buf.set_string(area.x + area.width - w - 1, y, hint, theme.dim());
        }
        if self.input.is_some() {
            // Show where typing goes.
            let x = area.x + 2 + self.input.as_ref().map(|q| q.chars().count()).unwrap_or(0) as u16;
            if let Some(cell) = buf.cell_mut((x.min(area.x + area.width - 1), y)) {
                cell.set_style(Style::new().add_modifier(Modifier::REVERSED));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn pager(n: usize) -> Pager {
        let mut lines = Vec::new();
        for i in 0..n {
            lines.push(Line::from(format!("line {i}")));
        }
        let mut p = Pager::new("main", lines);
        p.height = 10;
        p.top = p.max_top();
        p
    }

    #[test]
    fn vi_motions() {
        let mut p = pager(100);
        assert_eq!(p.cursor, 99);
        p.key(key(KeyCode::Char('k')));
        assert_eq!(p.cursor, 98);
        p.key(ctrl('u'));
        assert_eq!((p.top, p.cursor), (85, 93));
        p.key(ctrl('d'));
        assert_eq!((p.top, p.cursor), (90, 98));
        p.key(key(KeyCode::Char('g')));
        p.key(key(KeyCode::Char('g')));
        assert_eq!((p.top, p.cursor), (0, 0));
        p.key(key(KeyCode::Char('G')));
        assert_eq!(p.cursor, 99);
    }

    #[test]
    fn search_and_copy_a_range() {
        let mut p = pager(100);
        p.key(key(KeyCode::Char('/')));
        for c in "line 42".chars() {
            p.key(key(KeyCode::Char(c)));
        }
        p.key(key(KeyCode::Enter));
        assert_eq!(p.cursor, 42);
        p.key(key(KeyCode::Char('v')));
        p.key(key(KeyCode::Char('j')));
        p.key(key(KeyCode::Char('j')));
        let PagerAction::Copy(text) = p.key(key(KeyCode::Char('y'))) else {
            panic!("no copy")
        };
        assert_eq!(text, "line 42\nline 43\nline 44");
        assert!(matches!(p.key(key(KeyCode::Char('q'))), PagerAction::Close));
    }
}
