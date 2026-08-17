use std::time::{Duration, Instant};

use crossterm::event::KeyEvent;
use ratatui_textarea::{CursorMove, Input, Key, TextArea, WrapMode};

use crate::config::{Config, Eol, Indent};
use crate::git::TextFile;

/// Consecutive edits closer together than this, of the same kind and at
/// adjacent positions, undo as one step.
const GROUP_PAUSE: Duration = Duration::from_millis(500);
const MAX_SNAPSHOTS: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditKind {
    Insert,
    Delete,
    Other,
}

#[derive(Debug, Clone)]
struct Snapshot {
    lines: Vec<String>,
    cursor: (usize, usize),
}

struct LastEdit {
    kind: EditKind,
    at: (usize, usize),
    when: Instant,
}

/// The text a sorted `(from, to)` selection covers, `(row, col)` in chars, `to`
/// exclusive. Shared by the editor and the read-only HEAD side.
pub fn slice_selection(lines: &[String], range: Selection) -> String {
    let ((from_row, from_col), (to_row, to_col)) = range;
    if from_row == to_row {
        return slice_chars(&lines[from_row], from_col, to_col);
    }
    let mut out = slice_chars(&lines[from_row], from_col, usize::MAX);
    for line in &lines[from_row + 1..to_row] {
        out.push('\n');
        out.push_str(line);
    }
    out.push('\n');
    out.push_str(&slice_chars(&lines[to_row], 0, to_col));
    out
}

pub type Selection = ((usize, usize), (usize, usize));

/// The Current pane's text. Wraps `ratatui-textarea` for cursor, selection, and
/// unicode handling, but keeps its own undo stack: the crate records one entry
/// per edit with no merging, and a single input can push more than one, so
/// "undo a typed word in one step" cannot be expressed by counting its undos.
pub struct EditorBuffer {
    area: TextArea<'static>,
    loaded: Vec<String>,
    loaded_body: String,
    eol: Eol,
    trailing_newline: bool,
    indent: Indent,
    snapshots: Vec<Snapshot>,
    index: usize,
    last_edit: Option<LastEdit>,
}

impl EditorBuffer {
    pub fn new(file: &TextFile, config: &Config) -> Self {
        let lines: Vec<String> = split_lines(&file.body);
        let indent = config.indent_for(&lines);
        let mut area = TextArea::new(lines.clone());
        area.set_max_histories(0);
        area.set_wrap_mode(WrapMode::None);
        area.set_hard_tab_indent(indent.tabs);
        area.set_tab_length(indent.width.min(u8::MAX as usize) as u8);

        let snapshot = Snapshot {
            lines: lines.clone(),
            cursor: (0, 0),
        };
        Self {
            area,
            loaded: lines,
            loaded_body: file.body.clone(),
            eol: config.eol_for(file.eol),
            trailing_newline: file.trailing_newline,
            indent,
            snapshots: vec![snapshot],
            index: 0,
            last_edit: None,
        }
    }

    pub fn lines(&self) -> &[String] {
        self.area.lines()
    }

    pub fn cursor(&self) -> (usize, usize) {
        let cursor = self.area.cursor();
        (cursor.0, cursor.1)
    }

    pub fn selection(&self) -> Option<Selection> {
        self.area.selection_range()
    }

    pub fn selected_text(&self) -> Option<String> {
        Some(slice_selection(self.lines(), self.selection()?))
    }

    pub fn dirty(&self) -> bool {
        self.area.lines() != self.loaded.as_slice()
    }

    pub fn eol(&self) -> Eol {
        self.eol
    }

    pub fn tab_width(&self) -> usize {
        self.indent.width
    }

    pub fn trailing_newline(&self) -> bool {
        self.trailing_newline
    }

    /// The content this buffer was loaded from, for detecting that another
    /// process rewrote the file underneath us.
    pub fn loaded_body(&self) -> &str {
        &self.loaded_body
    }

    pub fn mark_saved(&mut self) {
        self.loaded = self.area.lines().to_vec();
        self.loaded_body = self.body();
    }

    pub fn body(&self) -> String {
        let mut body = self.area.lines().join(self.eol.as_str());
        if self.trailing_newline {
            body.push_str(self.eol.as_str());
        }
        body
    }

    /// A key that is not one of difv's own. Returns whether the buffer changed.
    pub fn input(&mut self, key: KeyEvent) -> bool {
        let input = Input::from(key);
        let kind = match input.key {
            Key::Backspace | Key::Delete => EditKind::Delete,
            Key::Char(_) | Key::Tab | Key::Enter => EditKind::Insert,
            _ => EditKind::Other,
        };
        if input.key == Key::Tab && !self.indent.tabs {
            let unit = self.indent.unit();
            return self.edit(EditKind::Insert, move |area| area.insert_str(unit));
        }
        self.edit(kind, move |area| area.input(input))
    }

    pub fn insert_str(&mut self, text: &str) -> bool {
        self.edit(EditKind::Other, |area| area.insert_str(text))
    }

    pub fn move_cursor_to(&mut self, row: usize, col: usize) {
        self.area.cancel_selection();
        self.move_cursor_to_keeping_selection(row, col);
    }

    /// Used by drag, and by undo when restoring a snapshot's cursor.
    pub fn move_cursor_to_keeping_selection(&mut self, row: usize, col: usize) {
        self.area.move_cursor(CursorMove::Jump(
            row.min(u16::MAX as usize) as u16,
            col.min(u16::MAX as usize) as u16,
        ));
    }

    pub fn start_selection(&mut self) {
        self.area.start_selection();
    }

    pub fn cancel_selection(&mut self) {
        self.area.cancel_selection();
    }

    pub fn cut(&mut self) -> bool {
        self.edit(EditKind::Other, |area| area.cut())
    }

    pub fn undo(&mut self) -> bool {
        if self.index == 0 {
            return false;
        }
        self.index -= 1;
        self.restore();
        true
    }

    pub fn redo(&mut self) -> bool {
        if self.index + 1 >= self.snapshots.len() {
            return false;
        }
        self.index += 1;
        self.restore();
        true
    }

    fn edit(&mut self, kind: EditKind, apply: impl FnOnce(&mut TextArea<'static>) -> bool) -> bool {
        if !apply(&mut self.area) {
            return false;
        }
        self.record(kind);
        true
    }

    fn record(&mut self, kind: EditKind) {
        let cursor = self.cursor();
        let now = Instant::now();
        let continues = self.last_edit.as_ref().is_some_and(|last| {
            last.kind == kind
                && kind != EditKind::Other
                && last.at.0 == cursor.0
                && last.at.1.abs_diff(cursor.1) <= 1
                && now.duration_since(last.when) < GROUP_PAUSE
        });

        let snapshot = Snapshot {
            lines: self.area.lines().to_vec(),
            cursor,
        };
        if continues {
            self.snapshots[self.index] = snapshot;
        } else {
            self.snapshots.truncate(self.index + 1);
            self.snapshots.push(snapshot);
            self.index += 1;
            if self.snapshots.len() > MAX_SNAPSHOTS {
                self.snapshots.remove(0);
                self.index -= 1;
            }
        }
        self.last_edit = Some(LastEdit {
            kind,
            at: cursor,
            when: now,
        });
    }

    fn restore(&mut self) {
        let snapshot = self.snapshots[self.index].clone();
        let mut area = TextArea::new(snapshot.lines);
        area.set_max_histories(0);
        area.set_wrap_mode(WrapMode::None);
        area.set_hard_tab_indent(self.indent.tabs);
        area.set_tab_length(self.indent.width.min(u8::MAX as usize) as u8);
        self.area = area;
        self.move_cursor_to(snapshot.cursor.0, snapshot.cursor.1);
        // An undo is never a continuation of the edit it reverses.
        self.last_edit = None;
    }
}

/// `str::lines` drops the distinction between "ends with a newline" and "has an
/// empty last line"; the trailing-newline flag carries that instead.
fn split_lines(body: &str) -> Vec<String> {
    let body = body.strip_suffix('\n').unwrap_or(body);
    let body = body.strip_suffix('\r').unwrap_or(body);
    body.split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line).to_string())
        .collect()
}

fn slice_chars(line: &str, from: usize, to: usize) -> String {
    line.chars()
        .skip(from)
        .take(to.saturating_sub(from))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyModifiers};

    fn buffer(body: &str) -> EditorBuffer {
        let file = TextFile {
            body: body.to_string(),
            eol: Some(Eol::Lf),
            trailing_newline: body.ends_with('\n'),
        };
        EditorBuffer::new(&file, &Config::default())
    }

    fn press(buffer: &mut EditorBuffer, code: KeyCode) {
        buffer.input(KeyEvent::new(code, KeyModifiers::NONE));
    }

    fn type_str(buffer: &mut EditorBuffer, text: &str) {
        for ch in text.chars() {
            press(buffer, KeyCode::Char(ch));
        }
    }

    #[test]
    fn splitting_keeps_the_line_count_the_file_has() {
        assert_eq!(split_lines("a\nb\n"), vec!["a", "b"]);
        assert_eq!(split_lines("a\nb"), vec!["a", "b"]);
        assert_eq!(split_lines("a\r\nb\r\n"), vec!["a", "b"]);
        assert_eq!(split_lines(""), vec![""]);
    }

    #[test]
    fn dirty_is_set_on_edit_and_cleared_by_undoing_back() {
        let mut buffer = buffer("hello\n");
        assert!(!buffer.dirty());
        type_str(&mut buffer, "x");
        assert!(buffer.dirty());
        buffer.undo();
        assert!(!buffer.dirty());
    }

    #[test]
    fn a_typed_word_undoes_as_one_step() {
        let mut buffer = buffer("\n");
        type_str(&mut buffer, "hello");
        assert_eq!(buffer.lines(), ["hello"]);
        buffer.undo();
        assert_eq!(buffer.lines(), [""]);
    }

    #[test]
    fn a_different_kind_of_edit_starts_a_new_step() {
        let mut buffer = buffer("abc\n");
        buffer.move_cursor_to(0, 3);
        type_str(&mut buffer, "de");
        press(&mut buffer, KeyCode::Backspace);
        assert_eq!(buffer.lines(), ["abcd"]);
        buffer.undo();
        assert_eq!(buffer.lines(), ["abcde"]);
        buffer.undo();
        assert_eq!(buffer.lines(), ["abc"]);
    }

    #[test]
    fn a_new_edit_clears_the_redo_stack() {
        let mut buffer = buffer("\n");
        type_str(&mut buffer, "hello");
        buffer.undo();
        type_str(&mut buffer, "x");
        assert!(!buffer.redo());
        assert_eq!(buffer.lines(), ["x"]);
    }

    #[test]
    fn redo_restores_the_undone_edit() {
        let mut buffer = buffer("\n");
        type_str(&mut buffer, "hi");
        buffer.undo();
        assert_eq!(buffer.lines(), [""]);
        assert!(buffer.redo());
        assert_eq!(buffer.lines(), ["hi"]);
    }

    #[test]
    fn replacing_a_selection_undoes_as_one_step() {
        let mut buffer = buffer("hello world\n");
        buffer.move_cursor_to(0, 0);
        buffer.start_selection();
        buffer.move_cursor_to_keeping_selection(0, 5);
        type_str(&mut buffer, "X");
        assert_eq!(buffer.lines(), ["X world"]);
        buffer.undo();
        assert_eq!(buffer.lines(), ["hello world"]);
    }

    #[test]
    fn tab_inserts_the_detected_indent() {
        let mut buffer = buffer("fn a() {\n  b();\n}\n");
        buffer.move_cursor_to(2, 1);
        press(&mut buffer, KeyCode::Tab);
        assert_eq!(buffer.lines()[2], "}  ");
    }

    #[test]
    fn body_restores_the_files_line_ending_and_trailing_newline() {
        let file = TextFile {
            body: "a\r\nb\r\n".to_string(),
            eol: Some(Eol::Crlf),
            trailing_newline: true,
        };
        let buffer = EditorBuffer::new(&file, &Config::default());
        assert_eq!(buffer.body(), "a\r\nb\r\n");

        let file = TextFile {
            body: "a\nb".to_string(),
            eol: Some(Eol::Lf),
            trailing_newline: false,
        };
        let buffer = EditorBuffer::new(&file, &Config::default());
        assert_eq!(buffer.body(), "a\nb");
    }

    #[test]
    fn cut_removes_the_selection_as_one_step() {
        let mut buffer = buffer("hello world\n");
        buffer.move_cursor_to(0, 0);
        buffer.start_selection();
        buffer.move_cursor_to_keeping_selection(0, 6);
        // What copy would place on the clipboard, and what cut removes.
        assert_eq!(buffer.selected_text().unwrap(), "hello ");
        assert!(buffer.cut());
        assert_eq!(buffer.lines(), ["world"]);
        buffer.undo();
        assert_eq!(buffer.lines(), ["hello world"]);
    }

    #[test]
    fn selected_text_spans_lines() {
        let mut buffer = buffer("one\ntwo\nthree\n");
        buffer.move_cursor_to(0, 1);
        buffer.start_selection();
        buffer.move_cursor_to_keeping_selection(2, 2);
        assert_eq!(buffer.selected_text().unwrap(), "ne\ntwo\nth");
    }

    #[test]
    fn undo_stack_stays_bounded() {
        let mut buffer = buffer("\n");
        for i in 0..(MAX_SNAPSHOTS + 20) {
            buffer.move_cursor_to(0, 0);
            type_str(&mut buffer, if i % 2 == 0 { "a" } else { "b" });
        }
        assert!(buffer.snapshots.len() <= MAX_SNAPSHOTS);
    }
}
