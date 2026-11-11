//! The multi-line compose draft: a minimal local editor state (the
//! lines + the caret). The single-line boxes keep tui-input; this
//! one is local because the off-the-shelf multi-line editor widget
//! pins an older ratatui than this workspace builds against.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Debug, Default)]
pub struct Draft {
    lines: Vec<String>,
    /// (row, char-col) — the caret sits ON the characters (0..=len).
    cursor: (usize, usize),
}

impl Draft {
    pub fn new() -> Self {
        Self {
            lines: vec![String::new()],
            cursor: (0, 0),
        }
    }

    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    pub fn cursor(&self) -> (usize, usize) {
        self.cursor
    }

    /// The whole draft as one string (the send / save / copy face).
    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    pub fn clear(&mut self) {
        self.lines = vec![String::new()];
        self.cursor = (0, 0);
    }

    /// Insert a string at the caret; '\n' splits, '\r' is dropped.
    pub fn insert_str(&mut self, s: &str) {
        for c in s.chars() {
            match c {
                '\r' => {}
                '\n' => self.newline(),
                _ => self.char(c),
            }
        }
    }

    /// The byte offset of the char-col (the caret rides characters;
    /// String's edit methods take bytes — CJK input made this split
    /// load-bearing).
    fn byte_col(line: &str, col: usize) -> usize {
        line.char_indices()
            .nth(col)
            .map(|(b, _)| b)
            .unwrap_or(line.len())
    }

    fn char(&mut self, c: char) {
        let (row, col) = self.cursor;
        let byte = Self::byte_col(&self.lines[row], col);
        self.lines[row].insert(byte, c);
        self.cursor.1 += 1;
    }

    fn newline(&mut self) {
        let (row, col) = self.cursor;
        let byte = Self::byte_col(&self.lines[row], col);
        let rest = self.lines[row].split_off(byte);
        self.lines.insert(row + 1, rest);
        self.cursor = (row + 1, 0);
    }

    fn backspace(&mut self) {
        let (row, col) = self.cursor;
        if col > 0 {
            let byte = Self::byte_col(&self.lines[row], col - 1);
            self.lines[row].remove(byte);
            self.cursor.1 -= 1;
        } else if row > 0 {
            let cur = self.lines.remove(row);
            self.cursor.1 = self.lines[row - 1].chars().count();
            self.lines[row - 1].push_str(&cur);
            self.cursor.0 -= 1;
        }
    }

    /// Ctrl-W: delete the run of characters before the caret — the
    /// trailing whitespace first, then the word itself. At the line
    /// start it joins the previous row (the backspace behavior).
    fn delete_word_back(&mut self) {
        let (row, col) = self.cursor;
        let chars: Vec<char> = self.lines[row].chars().collect();
        let mut i = col;
        while i > 0 && chars[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !chars[i - 1].is_whitespace() {
            i -= 1;
        }
        if i < col {
            let line = &mut self.lines[row];
            let start = Self::byte_col(line, i);
            let end = Self::byte_col(line, col);
            line.replace_range(start..end, "");
            self.cursor.1 = i;
        } else if col == 0 && row > 0 {
            self.backspace();
        }
    }

    fn delete(&mut self) {
        let (row, col) = self.cursor;
        let len = self.lines[row].chars().count();
        if col < len {
            let byte = Self::byte_col(&self.lines[row], col);
            self.lines[row].remove(byte);
        } else if row + 1 < self.lines.len() {
            let next = self.lines.remove(row + 1);
            self.lines[row].push_str(&next);
        }
    }

    /// The editor's key face: characters, newline, backspace/delete,
    /// the arrow moves. Everything else returns false (the caller's
    /// channel keys stay theirs).
    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        let (row, col) = self.cursor;
        match (key.code, key.modifiers) {
            (KeyCode::Char(c), m) if !m.contains(KeyModifiers::CONTROL) => {
                self.char(c);
            }
            (KeyCode::Enter, _) => self.newline(),
            (KeyCode::Backspace, _) => self.backspace(),
            (KeyCode::Delete, _) => self.delete(),
            (KeyCode::Left, _) => {
                if col > 0 {
                    self.cursor.1 -= 1;
                } else if row > 0 {
                    self.cursor = (row - 1, self.lines[row - 1].chars().count());
                }
            }
            (KeyCode::Right, _) => {
                let len = self.lines[row].chars().count();
                if col < len {
                    self.cursor.1 += 1;
                } else if row + 1 < self.lines.len() {
                    self.cursor = (row + 1, 0);
                }
            }
            (KeyCode::Up, _) => {
                self.cursor.0 = row.saturating_sub(1);
                self.clamp_col();
            }
            (KeyCode::Down, _) => {
                self.cursor.0 = (row + 1).min(self.lines.len() - 1);
                self.clamp_col();
            }
            (KeyCode::Home, m) if m.contains(KeyModifiers::CONTROL) => {
                self.cursor = (0, 0);
            }
            (KeyCode::End, m) if m.contains(KeyModifiers::CONTROL) => {
                let last = self.lines.len() - 1;
                self.cursor = (last, self.lines[last].chars().count());
            }
            (KeyCode::Home, _) => self.cursor.1 = 0,
            (KeyCode::End, _) => self.cursor.1 = self.lines[row].chars().count(),
            (KeyCode::Char('w'), m) if m.contains(KeyModifiers::CONTROL) => {
                self.delete_word_back();
            }
            _ => return false,
        }
        true
    }

    /// Vertical moves keep the intended column when the target row is
    /// shorter (the caret rides the row's end, vim-style).
    fn clamp_col(&mut self) {
        let len = self.lines[self.cursor.0].chars().count();
        self.cursor.1 = self.cursor.1.min(len);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::from(code)
    }

    #[test]
    fn typing_and_newlines() {
        let mut d = Draft::new();
        d.insert_str("ab\ncd");
        assert_eq!(d.lines(), ["ab", "cd"]);
        assert_eq!(d.cursor(), (1, 2));
        assert_eq!(d.text(), "ab\ncd");
        d.handle_key(key(KeyCode::Enter));
        assert_eq!(d.lines(), ["ab", "cd", ""]);
    }

    #[test]
    fn backspace_joins_rows() {
        let mut d = Draft::new();
        d.insert_str("ab\ncd");
        d.handle_key(key(KeyCode::Backspace));
        assert_eq!(d.lines(), ["ab", "c"]);
        d.handle_key(key(KeyCode::Backspace));
        d.handle_key(key(KeyCode::Backspace));
        assert_eq!(d.lines(), ["ab"]);
    }

    #[test]
    fn clear_resets_everything() {
        let mut d = Draft::new();
        d.insert_str("x\ny\nz");
        d.clear();
        assert!(d.text().trim().is_empty());
        assert_eq!(d.cursor(), (0, 0));
    }

    #[test]
    fn wide_chars_edit_by_char_col() {
        let mut d = Draft::new();
        d.insert_str("你好");
        assert_eq!(d.cursor(), (0, 2));
        d.handle_key(key(KeyCode::Left));
        d.insert_str("世界");
        assert_eq!(d.lines(), ["你世界好"]);
        d.handle_key(key(KeyCode::Backspace));
        assert_eq!(d.lines(), ["你世好"]);
        d.handle_key(key(KeyCode::Backspace));
        assert_eq!(d.lines(), ["你好"]);
    }

    #[test]
    fn arrows_move_between_rows() {
        let mut d = Draft::new();
        d.insert_str("ab\nc");
        d.handle_key(key(KeyCode::Up));
        assert_eq!(d.cursor(), (0, 1)); // the col rides the shorter row
        d.handle_key(key(KeyCode::Down));
        assert_eq!(d.cursor(), (1, 1));
    }

    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    #[test]
    fn ctrl_w_deletes_the_last_word() {
        let mut d = Draft::new();
        d.insert_str("hello world  ");
        d.handle_key(ctrl(KeyCode::Char('w')));
        assert_eq!(d.lines(), ["hello "]);
        d.handle_key(ctrl(KeyCode::Char('w')));
        assert_eq!(d.lines(), [""]);
        // At the line start it joins the previous row.
        d.insert_str("a\nb");
        d.handle_key(key(KeyCode::Home));
        d.handle_key(ctrl(KeyCode::Char('w')));
        assert_eq!(d.lines(), ["a"]);
    }

    #[test]
    fn home_end_ride_the_line_ctrl_rides_the_document() {
        let mut d = Draft::new();
        d.insert_str("long line\nshort");
        d.handle_key(key(KeyCode::Home));
        assert_eq!(d.cursor(), (1, 0));
        d.handle_key(key(KeyCode::End));
        assert_eq!(d.cursor(), (1, 5));
        d.handle_key(ctrl(KeyCode::Home));
        assert_eq!(d.cursor(), (0, 0));
        d.handle_key(ctrl(KeyCode::End));
        assert_eq!(d.cursor(), (1, 5));
    }
}
