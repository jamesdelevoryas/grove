//! The prompt editor: multi-line text with a cursor, readline-style keys, and a layout
//! that wraps by display width and reports where the terminal cursor goes.

use unicode_width::UnicodeWidthChar;

#[derive(Default)]
pub struct Editor {
    text: String,
    /// Byte offset, always on a char boundary.
    cursor: usize,
    kill: String,
    /// Column to aim for when moving up/down through lines of different lengths.
    goal: Option<usize>,
}

impl Editor {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn set(&mut self, text: &str) {
        self.text = text.to_string();
        self.cursor = self.text.len();
        self.goal = None;
    }

    pub fn take(&mut self) -> String {
        self.cursor = 0;
        self.goal = None;
        std::mem::take(&mut self.text)
    }

    pub fn clear(&mut self) {
        self.take();
    }

    pub fn insert(&mut self, s: &str) {
        // Pasted CRLF and lone CR become newlines.
        let s = s.replace("\r\n", "\n").replace('\r', "\n");
        self.text.insert_str(self.cursor, &s);
        self.cursor += s.len();
        self.goal = None;
    }

    pub fn insert_char(&mut self, c: char) {
        let mut buf = [0u8; 4];
        self.insert(c.encode_utf8(&mut buf));
    }

    fn prev_boundary(&self, i: usize) -> usize {
        let mut j = i.saturating_sub(1);
        while j > 0 && !self.text.is_char_boundary(j) {
            j -= 1;
        }
        j
    }

    fn next_boundary(&self, i: usize) -> usize {
        let mut j = (i + 1).min(self.text.len());
        while j < self.text.len() && !self.text.is_char_boundary(j) {
            j += 1;
        }
        j
    }

    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let start = self.prev_boundary(self.cursor);
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
        self.goal = None;
    }

    pub fn delete(&mut self) {
        if self.cursor >= self.text.len() {
            return;
        }
        let end = self.next_boundary(self.cursor);
        self.text.replace_range(self.cursor..end, "");
        self.goal = None;
    }

    pub fn left(&mut self) {
        self.cursor = self.prev_boundary(self.cursor);
        self.goal = None;
    }

    pub fn right(&mut self) {
        self.cursor = self.next_boundary(self.cursor);
        self.goal = None;
    }

    fn line_start(&self) -> usize {
        match self.text[..self.cursor].rfind('\n') {
            Some(i) => i + 1,
            None => 0,
        }
    }

    fn line_end(&self) -> usize {
        match self.text[self.cursor..].find('\n') {
            Some(i) => self.cursor + i,
            None => self.text.len(),
        }
    }

    pub fn home(&mut self) {
        self.cursor = self.line_start();
        self.goal = None;
    }

    pub fn end(&mut self) {
        self.cursor = self.line_end();
        self.goal = None;
    }

    pub fn word_left(&mut self) {
        let mut i = self.cursor;
        let chars: Vec<(usize, char)> = self.text[..i].char_indices().collect();
        let mut k = chars.len();
        while k > 0 && !chars[k - 1].1.is_alphanumeric() {
            k -= 1;
        }
        while k > 0 && chars[k - 1].1.is_alphanumeric() {
            k -= 1;
        }
        i = if k == 0 { 0 } else { chars[k].0 };
        self.cursor = i;
        self.goal = None;
    }

    pub fn word_right(&mut self) {
        let rest = &self.text[self.cursor..];
        let mut seen_word = false;
        let mut advance = rest.len();
        for (i, c) in rest.char_indices() {
            if c.is_alphanumeric() {
                seen_word = true;
            } else if seen_word {
                advance = i;
                break;
            }
        }
        self.cursor += advance;
        self.goal = None;
    }

    pub fn delete_word_back(&mut self) {
        let end = self.cursor;
        self.word_left();
        let killed = self.text[self.cursor..end].to_string();
        self.text.replace_range(self.cursor..end, "");
        self.kill = killed;
    }

    pub fn kill_to_end(&mut self) {
        let mut end = self.line_end();
        // At the end of a line, kill the newline instead.
        if end == self.cursor && end < self.text.len() {
            end += 1;
        }
        self.kill = self.text[self.cursor..end].to_string();
        self.text.replace_range(self.cursor..end, "");
        self.goal = None;
    }

    pub fn kill_to_start(&mut self) {
        let start = self.line_start();
        self.kill = self.text[start..self.cursor].to_string();
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
        self.goal = None;
    }

    pub fn yank(&mut self) {
        let k = self.kill.clone();
        self.insert(&k);
    }

    /// Moves up a logical line. Returns false when already on the first line (the caller
    /// then goes back in history).
    pub fn up(&mut self) -> bool {
        let start = self.line_start();
        if start == 0 {
            return false;
        }
        let col = self
            .goal
            .unwrap_or_else(|| self.text[start..self.cursor].chars().count());
        let prev_end = start - 1;
        let prev_start = match self.text[..prev_end].rfind('\n') {
            Some(i) => i + 1,
            None => 0,
        };
        self.cursor = advance_chars(&self.text, prev_start, prev_end, col);
        self.goal = Some(col);
        true
    }

    pub fn down(&mut self) -> bool {
        let end = self.line_end();
        if end >= self.text.len() {
            return false;
        }
        let col = self
            .goal
            .unwrap_or_else(|| self.text[self.line_start()..self.cursor].chars().count());
        let next_start = end + 1;
        let next_end = match self.text[next_start..].find('\n') {
            Some(i) => next_start + i,
            None => self.text.len(),
        };
        self.cursor = advance_chars(&self.text, next_start, next_end, col);
        self.goal = Some(col);
        true
    }

    /// Lays the text out in `width` cells after a `prefix_w`-wide prompt. Returns the
    /// visual rows (as plain strings) and the cursor's (row, column).
    pub fn layout(&self, width: usize, prefix_w: usize) -> (Vec<String>, (usize, usize)) {
        let width = width.max(prefix_w + 4);
        let mut rows = vec![String::new()];
        let mut col = prefix_w;
        let mut cursor = (0, prefix_w);
        for (i, c) in self.text.char_indices() {
            if i == self.cursor {
                cursor = (rows.len() - 1, col);
            }
            if c == '\n' {
                rows.push(String::new());
                col = prefix_w;
                continue;
            }
            let w = c.width().unwrap_or(0);
            if col + w > width {
                rows.push(String::new());
                col = prefix_w;
            }
            rows.last_mut().unwrap().push(c);
            col += w;
        }
        if self.cursor >= self.text.len() {
            if col >= width {
                rows.push(String::new());
                col = prefix_w;
            }
            cursor = (rows.len() - 1, col);
        }
        (rows, cursor)
    }
}

/// Byte offset `col` chars into [start, end), clamped to `end`.
fn advance_chars(text: &str, start: usize, end: usize, col: usize) -> usize {
    for (n, (off, _)) in text[start..end].char_indices().enumerate() {
        if n == col {
            return start + off;
        }
    }
    end
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_and_moves() {
        let mut e = Editor::default();
        e.insert("hello world");
        e.word_left();
        e.insert("big ");
        assert_eq!(e.text(), "hello big world");
        e.home();
        e.kill_to_end();
        assert_eq!(e.text(), "");
        e.yank();
        assert_eq!(e.text(), "hello big world");
    }

    #[test]
    fn moves_between_lines_keeping_column() {
        let mut e = Editor::default();
        e.insert("abcdef\nxy\nlonger line");
        assert!(e.up());
        assert!(e.up());
        e.insert("_");
        assert_eq!(e.text(), "abcdef_\nxy\nlonger line");
        assert!(!e.up());
    }

    #[test]
    fn layout_wraps_and_places_cursor() {
        let mut e = Editor::default();
        e.insert("abcdefghij");
        let (rows, cursor) = e.layout(8, 2);
        assert_eq!(rows, vec!["abcdef", "ghij"]);
        assert_eq!(cursor, (1, 6));
    }
}
