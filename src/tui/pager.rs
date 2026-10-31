//! The frozen view (Ctrl+O): a still snapshot of one agent's transcript that works like a
//! read-only Helix buffer, while agents keep working underneath. There's a cursor and a
//! selection (anchor + head, both inclusive), Helix motions and select mode, and `y`
//! copies the selection to the clipboard. The mouse is released too, so the terminal's
//! own drag-select works on what's on screen.
//!
//! Keys: h j k l · w b e (W B E) · x (line, again to extend) · v select mode · ; collapse ·
//! alt-; flip · f t F T <char> · gg ge gh gl gs · % all · / n N · ctrl-d ctrl-u ctrl-f
//! ctrl-b · ctrl-e ctrl-y · y or space y copy · q, ctrl-o, or esc (when nothing's
//! selected) to leave.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthChar;

use super::theme::Theme;

pub enum PagerAction {
    None,
    Close,
    Copy(String),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Class {
    Word,
    Punct,
    Space,
    Eol,
}

fn class(c: char, big: bool) -> Class {
    if c == '\n' {
        Class::Eol
    } else if c.is_whitespace() {
        Class::Space
    } else if big || c.is_alphanumeric() || c == '_' {
        Class::Word
    } else {
        Class::Punct
    }
}

/// A key that needs a second key: g…, f/t/F/T <char>, space y.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Pending {
    None,
    Goto,
    Find { forward: bool, till: bool },
    Space,
}

pub struct Pager {
    agent: String,
    lines: Vec<Line<'static>>,
    /// The text, every line ending in '\n'; positions are indexes into it.
    text: Vec<char>,
    /// Index of each line's first char.
    starts: Vec<usize>,
    anchor: usize,
    head: usize,
    /// Select mode (`v`): motions extend instead of replacing the selection.
    extend: bool,
    pending: Pending,
    /// The column j/k aim for.
    goal: Option<usize>,
    top: usize,
    /// Content rows on screen, from the last draw.
    height: usize,
    input: Option<String>,
    query: Option<String>,
    message: Option<String>,
}

impl Pager {
    pub fn new(agent: &str, lines: Vec<Line<'static>>) -> Self {
        let mut text = Vec::new();
        let mut starts = Vec::with_capacity(lines.len().max(1));
        for line in &lines {
            starts.push(text.len());
            let mut s = String::new();
            for span in &line.spans {
                s.push_str(&span.content);
            }
            for c in s.trim_end().chars() {
                text.push(c);
            }
            text.push('\n');
        }
        if starts.is_empty() {
            starts.push(0);
            text.push('\n');
        }
        // Start on the last line, where you were reading.
        let head = starts[starts.len() - 1];
        Self {
            agent: agent.to_string(),
            lines,
            text,
            starts,
            anchor: head,
            head,
            extend: false,
            pending: Pending::None,
            goal: None,
            top: usize::MAX,
            height: 20,
            input: None,
            query: None,
            message: None,
        }
    }

    // ---- positions ----

    fn line_of(&self, i: usize) -> usize {
        self.starts.partition_point(|&s| s <= i) - 1
    }

    /// Index of the '\n' that ends `line`.
    fn line_end(&self, line: usize) -> usize {
        match self.starts.get(line + 1) {
            Some(next) => next - 1,
            None => self.text.len() - 1,
        }
    }

    fn last_line(&self) -> usize {
        self.starts.len() - 1
    }

    fn col(&self, i: usize) -> usize {
        i - self.starts[self.line_of(i)]
    }

    fn cls(&self, i: usize, big: bool) -> Class {
        class(self.text[i], big)
    }

    fn selection(&self) -> (usize, usize) {
        (self.anchor.min(self.head), self.anchor.max(self.head))
    }

    /// Puts the head at `to`; outside select mode the selection collapses onto it.
    fn move_to(&mut self, to: usize) {
        self.head = to.min(self.text.len() - 1);
        if !self.extend {
            self.anchor = self.head;
        }
    }

    /// A motion that selects what it passes over (w, b, e, f, t): in normal mode the
    /// selection runs from `from` to `to`; in select mode only the head moves.
    fn select_span(&mut self, from: usize, to: usize) {
        if !self.extend {
            self.anchor = from;
        }
        self.head = to.min(self.text.len() - 1);
    }

    fn move_line(&mut self, delta: isize) {
        let line = self.line_of(self.head);
        let goal = self.goal.unwrap_or_else(|| self.col(self.head));
        let target = (line as isize + delta).clamp(0, self.last_line() as isize) as usize;
        let start = self.starts[target];
        let end = self.line_end(target);
        self.move_to((start + goal).min(end));
        self.goal = Some(goal);
    }

    // ---- word motions (Helix-like: w and e select to the word's edge) ----

    fn next_word_start(&self, big: bool) -> (usize, usize) {
        let len = self.text.len();
        let mut from = self.head;
        // Already at a word's last char: the motion starts with the next one.
        if from + 1 < len && self.cls(from, big) != self.cls(from + 1, big) {
            from += 1;
        }
        let mut j = from;
        let c = self.cls(j, big);
        if c != Class::Space && c != Class::Eol {
            while j < len && self.cls(j, big) == c {
                j += 1;
            }
        }
        while j < len && matches!(self.cls(j, big), Class::Space | Class::Eol) {
            j += 1;
        }
        (from, j.saturating_sub(1).max(from))
    }

    fn next_word_end(&self, big: bool) -> (usize, usize) {
        let len = self.text.len();
        let mut from = self.head;
        if from + 1 < len && self.cls(from, big) != self.cls(from + 1, big) {
            from += 1;
        }
        let mut j = from;
        while j < len && matches!(self.cls(j, big), Class::Space | Class::Eol) {
            j += 1;
        }
        if j >= len {
            return (from, len - 1);
        }
        let c = self.cls(j, big);
        while j < len && self.cls(j, big) == c {
            j += 1;
        }
        (from, j - 1)
    }

    fn prev_word_start(&self, big: bool) -> (usize, usize) {
        let mut from = self.head;
        if from > 0 && self.cls(from, big) != self.cls(from - 1, big) {
            from -= 1;
        }
        let mut j = from;
        while j > 0 && matches!(self.cls(j, big), Class::Space | Class::Eol) {
            j -= 1;
        }
        let c = self.cls(j, big);
        while j > 0 && self.cls(j - 1, big) == c {
            j -= 1;
        }
        (from, j)
    }

    fn find_char(&mut self, ch: char, forward: bool, till: bool) {
        let len = self.text.len();
        let mut found = None;
        if forward {
            let mut i = self.head + 1;
            // A repeated `t` would match the char right after the head: skip it.
            if till && i < len && self.text[i] == ch {
                i += 1;
            }
            while i < len {
                if self.text[i] == ch {
                    found = Some(if till { i - 1 } else { i });
                    break;
                }
                i += 1;
            }
        } else {
            let mut i = self.head;
            if till && i > 0 && self.text[i - 1] == ch {
                i -= 1;
            }
            while i > 0 {
                i -= 1;
                if self.text[i] == ch {
                    found = Some(if till { i + 1 } else { i });
                    break;
                }
            }
        }
        match found {
            Some(to) => {
                let from = self.head;
                self.select_span(from, to);
            }
            None => self.message = Some(format!("no '{ch}' found")),
        }
    }

    fn select_line(&mut self) {
        let (s, e) = self.selection();
        let first = self.line_of(s);
        let last = self.line_of(e);
        let whole = s == self.starts[first] && e == self.line_end(last);
        // `x` on whole lines takes the next one too.
        let last = if whole {
            (last + 1).min(self.last_line())
        } else {
            last
        };
        self.anchor = self.starts[first];
        self.head = self.line_end(last);
    }

    fn search(&mut self, forward: bool) {
        let Some(query) = self.query.clone() else {
            return;
        };
        let needle: Vec<char> = query.to_lowercase().chars().collect();
        if needle.is_empty() {
            return;
        }
        let hay: Vec<char> = self
            .text
            .iter()
            .map(|c| c.to_lowercase().next().unwrap_or(*c))
            .collect();
        let n = hay.len();
        let matches_at = |i: usize| i + needle.len() <= n && hay[i..i + needle.len()] == needle[..];
        let (s, _) = self.selection();
        for step in 1..=n {
            let i = if forward {
                (s + step) % n
            } else {
                (s + n - step % n) % n
            };
            if matches_at(i) {
                self.anchor = i;
                self.head = i + needle.len() - 1;
                self.message = None;
                return;
            }
        }
        self.message = Some(format!("not found: {query}"));
    }

    fn selected_text(&self) -> String {
        let (s, e) = self.selection();
        let mut out: String = self.text[s..=e].iter().collect();
        // A selection that ends on a line's newline copies the line without it.
        if out.ends_with('\n') {
            out.pop();
        }
        out
    }

    // ---- view ----

    fn max_top(&self) -> usize {
        self.starts.len().saturating_sub(self.height)
    }

    fn follow_head(&mut self) {
        let line = self.line_of(self.head);
        if line < self.top {
            self.top = line;
        } else if line >= self.top + self.height {
            self.top = line + 1 - self.height;
        }
    }

    /// ctrl-d / ctrl-u: view and cursor move together.
    fn scroll_both(&mut self, delta: isize) {
        self.top = (self.top as isize + delta).clamp(0, self.max_top() as isize) as usize;
        self.move_line(delta);
    }

    /// ctrl-e / ctrl-y: the view moves; the cursor only if it would leave the screen.
    fn scroll_view(&mut self, delta: isize) {
        self.top = (self.top as isize + delta).clamp(0, self.max_top() as isize) as usize;
        let line = self.line_of(self.head);
        if line < self.top {
            self.move_line((self.top - line) as isize);
        } else if line >= self.top + self.height {
            self.move_line(-((line + 1 - self.top - self.height) as isize));
        }
    }

    pub fn key(&mut self, key: KeyEvent) -> PagerAction {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        if let Some(input) = &mut self.input {
            match key.code {
                KeyCode::Enter => {
                    let q = std::mem::take(input);
                    self.input = None;
                    if !q.is_empty() {
                        self.query = Some(q);
                    }
                    self.search(true);
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
        let pending = std::mem::replace(&mut self.pending, Pending::None);
        self.message = None;
        let half = (self.height / 2).max(1) as isize;
        let page = self.height.saturating_sub(2).max(1) as isize;
        // Every motion but j/k forgets the goal column.
        let keeps_goal = matches!(
            key.code,
            KeyCode::Char('j' | 'k') | KeyCode::Up | KeyCode::Down
        ) && !ctrl;
        match pending {
            Pending::Goto => {
                match key.code {
                    KeyCode::Char('g') => self.move_to(0),
                    KeyCode::Char('e') => {
                        let last = self.starts[self.last_line()];
                        self.move_to(last);
                    }
                    KeyCode::Char('h') => self.move_to(self.starts[self.line_of(self.head)]),
                    KeyCode::Char('l') => {
                        let end = self.line_end(self.line_of(self.head));
                        self.move_to(
                            end.saturating_sub(1)
                                .max(self.starts[self.line_of(self.head)]),
                        );
                    }
                    KeyCode::Char('s') => {
                        let line = self.line_of(self.head);
                        let mut i = self.starts[line];
                        while i < self.line_end(line) && self.text[i].is_whitespace() {
                            i += 1;
                        }
                        self.move_to(i);
                    }
                    _ => {}
                }
                self.goal = None;
                return PagerAction::None;
            }
            Pending::Find { forward, till } => {
                if let KeyCode::Char(c) = key.code {
                    self.find_char(c, forward, till);
                }
                self.goal = None;
                return PagerAction::None;
            }
            Pending::Space => {
                if key.code == KeyCode::Char('y') {
                    return self.copy();
                }
                return PagerAction::None;
            }
            Pending::None => {}
        }
        match key.code {
            KeyCode::Char('o') | KeyCode::Char('c') if ctrl => return PagerAction::Close,
            KeyCode::Char('d') if ctrl => self.scroll_both(half),
            KeyCode::Char('u') if ctrl => self.scroll_both(-half),
            KeyCode::Char('f') if ctrl => self.scroll_both(page),
            KeyCode::Char('b') if ctrl => self.scroll_both(-page),
            KeyCode::Char('e') if ctrl => self.scroll_view(1),
            KeyCode::Char('y') if ctrl => self.scroll_view(-1),
            KeyCode::Char(';') if alt => std::mem::swap(&mut self.anchor, &mut self.head),
            _ if ctrl || alt => {}
            KeyCode::Char('h') | KeyCode::Left => {
                let start = self.starts[self.line_of(self.head)];
                if self.head > start {
                    self.move_to(self.head - 1);
                }
            }
            KeyCode::Char('l') | KeyCode::Right => {
                if self.head < self.line_end(self.line_of(self.head)) {
                    self.move_to(self.head + 1);
                }
            }
            KeyCode::Char('j') | KeyCode::Down => self.move_line(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_line(-1),
            KeyCode::PageDown => self.scroll_both(page),
            KeyCode::PageUp => self.scroll_both(-page),
            KeyCode::Home => self.move_to(self.starts[self.line_of(self.head)]),
            KeyCode::End => {
                let end = self.line_end(self.line_of(self.head));
                self.move_to(end);
            }
            KeyCode::Char(c @ ('w' | 'W')) => {
                let (from, to) = self.next_word_start(c == 'W');
                self.select_span(from, to);
            }
            KeyCode::Char(c @ ('e' | 'E')) => {
                let (from, to) = self.next_word_end(c == 'E');
                self.select_span(from, to);
            }
            KeyCode::Char(c @ ('b' | 'B')) => {
                let (from, to) = self.prev_word_start(c == 'B');
                self.select_span(from, to);
            }
            KeyCode::Char('x') => self.select_line(),
            KeyCode::Char('%') => {
                self.anchor = 0;
                self.head = self.text.len() - 1;
            }
            KeyCode::Char('v') => self.extend = !self.extend,
            KeyCode::Char(';') => self.anchor = self.head,
            KeyCode::Char('g') => self.pending = Pending::Goto,
            KeyCode::Char('G') => {
                let last = self.starts[self.last_line()];
                self.move_to(last);
            }
            KeyCode::Char('f') => {
                self.pending = Pending::Find {
                    forward: true,
                    till: false,
                }
            }
            KeyCode::Char('t') => {
                self.pending = Pending::Find {
                    forward: true,
                    till: true,
                }
            }
            KeyCode::Char('F') => {
                self.pending = Pending::Find {
                    forward: false,
                    till: false,
                }
            }
            KeyCode::Char('T') => {
                self.pending = Pending::Find {
                    forward: false,
                    till: true,
                }
            }
            KeyCode::Char(' ') => self.pending = Pending::Space,
            KeyCode::Char('/') => self.input = Some(String::new()),
            KeyCode::Char('n') => self.search(true),
            KeyCode::Char('N') => self.search(false),
            KeyCode::Char('y') | KeyCode::Enter => return self.copy(),
            KeyCode::Esc => {
                if self.extend {
                    self.extend = false;
                } else if self.anchor != self.head {
                    self.anchor = self.head;
                } else {
                    return PagerAction::Close;
                }
            }
            KeyCode::Char('q') => return PagerAction::Close,
            _ => {}
        }
        if !keeps_goal {
            self.goal = None;
        }
        PagerAction::None
    }

    fn copy(&mut self) -> PagerAction {
        let text = self.selected_text();
        let chars = text.chars().count();
        let lines = text.lines().count().max(1);
        self.message = Some(if lines > 1 {
            format!("yanked {lines} lines")
        } else {
            format!("yanked {chars} char{}", if chars == 1 { "" } else { "s" })
        });
        PagerAction::Copy(text)
    }

    pub fn draw(&mut self, buf: &mut Buffer, area: Rect, theme: &Theme) {
        if area.height < 2 {
            return;
        }
        self.height = (area.height - 1) as usize;
        self.top = self.top.min(self.max_top());
        self.follow_head();
        let (sel_start, sel_end) = self.selection();
        let sel_style = Style::new().bg(theme.sel_bg);
        let cursor_style = Style::new()
            .fg(theme.accent)
            .add_modifier(Modifier::REVERSED);
        let x0 = area.x + 1;
        let right = area.x + area.width;
        for row in 0..self.height {
            let line = self.top + row;
            let y = area.y + row as u16;
            let Some(styled) = self.lines.get(line) else {
                break;
            };
            if styled.style.bg.is_some() {
                buf.set_style(
                    Rect {
                        x: area.x,
                        y,
                        width: area.width,
                        height: 1,
                    },
                    styled.style,
                );
            }
            buf.set_line(x0, y, styled, area.width.saturating_sub(1));
            // Selection and cursor, cell by cell (the line's newline is one cell).
            let start = self.starts[line];
            let end = self.line_end(line);
            if sel_end < start || sel_start > end {
                continue;
            }
            let mut x = x0;
            for i in start..=end {
                let w = if self.text[i] == '\n' {
                    1
                } else {
                    self.text[i].width().unwrap_or(0).max(1)
                } as u16;
                if x >= right {
                    break;
                }
                if i >= sel_start && i <= sel_end {
                    let style = if i == self.head {
                        cursor_style
                    } else {
                        sel_style
                    };
                    for dx in 0..w.min(right - x) {
                        buf.set_style(
                            Rect {
                                x: x + dx,
                                y,
                                width: 1,
                                height: 1,
                            },
                            style,
                        );
                    }
                }
                x += w;
            }
        }

        // Status bar, Helix style.
        let y = area.y + area.height - 1;
        buf.set_style(
            Rect {
                x: area.x,
                y,
                width: area.width,
                height: 1,
            },
            Style::new().bg(theme.panel_bg),
        );
        let left = match (&self.input, &self.message) {
            (Some(q), _) => Line::from(vec![
                Span::styled(" /", theme.accent()),
                Span::raw(q.clone()),
            ]),
            (None, Some(m)) => Line::from(Span::styled(format!(" {m}"), theme.muted())),
            (None, None) => {
                let mode = if self.extend { " SEL " } else { " NOR " };
                let line = self.line_of(self.head);
                let (s, e) = self.selection();
                let mut spans = vec![
                    Span::styled(
                        mode,
                        Style::new()
                            .fg(theme.accent)
                            .add_modifier(Modifier::BOLD | Modifier::REVERSED),
                    ),
                    Span::styled(format!(" frozen · {} ", self.agent), theme.bold()),
                    Span::styled(
                        format!("{}:{}", line + 1, self.col(self.head) + 1),
                        theme.dim(),
                    ),
                ];
                if e > s {
                    spans.push(Span::styled(
                        format!("  {} selected", e - s + 1),
                        theme.dim(),
                    ));
                }
                Line::from(spans)
            }
        };
        buf.set_line(area.x, y, &left, area.width);
        let hint = "hjkl wbe x v ; f/t gg ge / n · y copy · q back";
        let w = hint.chars().count() as u16;
        if w + 40 < area.width {
            buf.set_string(area.x + area.width - w - 1, y, hint, theme.dim());
        }
        if self.input.is_some() {
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

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ch(c: char) -> KeyEvent {
        key(KeyCode::Char(c))
    }

    fn pager(text: &[&str]) -> Pager {
        let mut lines = Vec::new();
        for t in text {
            lines.push(Line::from(t.to_string()));
        }
        let mut p = Pager::new("main", lines);
        p.height = 10;
        p.top = 0;
        p
    }

    fn copy(p: &mut Pager) -> String {
        match p.key(ch('y')) {
            PagerAction::Copy(t) => t,
            _ => panic!("no copy"),
        }
    }

    #[test]
    fn starts_on_the_last_line() {
        let p = pager(&["one", "two", "three"]);
        assert_eq!(p.line_of(p.head), 2);
        assert_eq!(p.col(p.head), 0);
    }

    #[test]
    fn w_selects_a_word_and_y_copies_part_of_a_line() {
        let mut p = pager(&["hello brave new world"]);
        p.key(ch('w'));
        assert_eq!(copy(&mut p), "hello ");
        p.key(ch('w'));
        assert_eq!(copy(&mut p), "brave ");
        p.key(ch('e'));
        assert_eq!(copy(&mut p), "new");
        p.key(ch('b'));
        assert_eq!(copy(&mut p), "new");
    }

    #[test]
    fn select_mode_extends_across_lines() {
        let mut p = pager(&["first line here", "second line"]);
        p.key(ch('g'));
        p.key(ch('g'));
        p.key(ch('w')); // "first "
        p.key(ch(';'));
        p.key(ch('v'));
        p.key(ch('j'));
        p.key(ch('e'));
        // From the collapsed cursor (the space after "first") to the end of "line".
        assert_eq!(copy(&mut p), " line here\nsecond line");
        p.key(key(KeyCode::Esc)); // leave select mode
        p.key(ch('g'));
        p.key(ch('h'));
        p.key(ch('k'));
        p.key(ch('v'));
        p.key(ch('j'));
        p.key(ch('g'));
        p.key(ch('l'));
        assert_eq!(copy(&mut p), "first line here\nsecond line");
    }

    #[test]
    fn x_selects_lines_and_repeats() {
        let mut p = pager(&["a", "bb", "ccc"]);
        p.key(ch('g'));
        p.key(ch('g'));
        p.key(ch('x'));
        assert_eq!(copy(&mut p), "a");
        p.key(ch('x'));
        assert_eq!(copy(&mut p), "a\nbb");
    }

    #[test]
    fn find_char_and_search() {
        let mut p = pager(&["key = value; other"]);
        p.key(ch('g'));
        p.key(ch('h'));
        p.key(ch('t'));
        p.key(ch(';'));
        assert_eq!(copy(&mut p), "key = value");
        p.key(ch('/'));
        for c in "OTHER".chars() {
            p.key(ch(c));
        }
        p.key(key(KeyCode::Enter));
        assert_eq!(copy(&mut p), "other");
    }

    #[test]
    fn esc_collapses_then_closes() {
        let mut p = pager(&["some text"]);
        p.key(ch('w'));
        assert!(matches!(p.key(key(KeyCode::Esc)), PagerAction::None));
        assert!(matches!(p.key(key(KeyCode::Esc)), PagerAction::Close));
    }
}
