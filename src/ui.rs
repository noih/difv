use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use unicode_width::UnicodeWidthChar;

use crate::app::{App, Pane};
use crate::diff::RowKind;
use crate::git::Status;
use crate::settings;

const BG_DELETED: Color = Color::Rgb(0x3d, 0x1e, 0x22);
const BG_ADDED: Color = Color::Rgb(0x1c, 0x33, 0x22);
const BG_PHANTOM: Color = Color::Rgb(0x1a, 0x1a, 0x1c);
const BG_SELECTION: Color = Color::Rgb(0x2d, 0x44, 0x6b);
const FG_GUTTER: Color = Color::Rgb(0x6b, 0x6b, 0x76);

/// Unsaved edits, and a file rewritten under unsaved edits. Both are ASCII on
/// purpose: a round dot reads better, but `●` and the rest of its neighbourhood
/// are East Asian Ambiguous, so a terminal with a CJK font draws them two cells
/// wide in a slot laid out for one and shunts the rest of the row sideways.
const DIRTY_MARK: &str = "*";
const STALE_MARK: &str = "!";

pub fn draw(frame: &mut Frame, app: &mut App) {
    let [body, footer] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).areas(frame.area());
    // Weights are cell widths, so a drag keeps its proportion across a resize.
    let [w_files, w_old, w_new] = app.weights;
    let (files, old, new) = if app.files_hidden {
        let [old, new] =
            Layout::horizontal([Constraint::Fill(w_old), Constraint::Fill(w_new)]).areas(body);
        (Rect::ZERO, old, new)
    } else {
        let [files, old, new] = Layout::horizontal([
            Constraint::Fill(w_files),
            Constraint::Fill(w_old),
            Constraint::Fill(w_new),
        ])
        .areas(body);
        (files, old, new)
    };

    app.body = body;
    app.panes = [files, old, new];
    app.viewport_height = old.height.saturating_sub(2) as usize;

    if !app.files_hidden {
        draw_files(frame, app, files);
    }
    draw_side(frame, app, old, Pane::Old);
    draw_side(frame, app, new, Pane::New);
    draw_cursor(frame, app, new);
    draw_settings(frame, app, body);
    draw_help(frame, app, body);
    draw_footer(frame, app, footer);
}

/// Every key difv answers to, except the two the footer already shows: this
/// list is what `?` opens, so telling the reader about `?` is telling them what
/// they just did.
const HELP: [(&str, &str); 13] = [
    ("↑ ↓", "Select file / scroll / move the cursor"),
    ("Shift+Tab", "Next pane, from anywhere"),
    ("Tab", "One indent, in the Current pane"),
    ("Alt+↑ ↓", "Previous / next change"),
    ("← →", "Scroll sideways / move the cursor"),
    ("PageUp PageDown Home End", "Scroll / move the cursor"),
    ("Ctrl+S", "Save the file"),
    ("Ctrl+Z Ctrl+Y", "Undo / redo"),
    ("Ctrl+C Ctrl+X Ctrl+V", "Copy / cut / paste"),
    ("Ctrl+B", "Hide / show the file list"),
    ("r", "Reload changes"),
    ("Esc", "Back to the file list"),
    ("q", "Quit"),
];

/// A panel floating over the diff, centred and clamped to what there is room
/// for. Both overlays are detours rather than places, so neither takes a pane.
fn overlay(body: Rect, width: u16, rows: u16) -> Rect {
    let width = width.min(body.width);
    let height = (rows + 2).min(body.height);
    Rect {
        x: body.x + (body.width - width) / 2,
        y: body.y + (body.height - height) / 2,
        width,
        height,
    }
}

fn draw_help(frame: &mut Frame, app: &mut App, body: Rect) {
    if !app.help {
        return;
    }
    // Columns, not bytes: the key column already holds arrows.
    let keys = HELP
        .iter()
        .map(|(keys, _)| keys.chars().count())
        .max()
        .unwrap_or(0);
    let widest = HELP
        .iter()
        .map(|(_, what)| what.chars().count())
        .max()
        .unwrap_or(0);
    let area = overlay(body, (keys + widest + 5) as u16, HELP.len() as u16);
    // A short terminal cannot show the whole list, so it scrolls rather than
    // silently dropping the rest.
    let shown = area.height.saturating_sub(2) as usize;
    app.help_scroll = app.help_scroll.min(HELP.len().saturating_sub(shown));
    let items: Vec<ListItem> = HELP
        .iter()
        .map(|(key, what)| {
            ListItem::new(Line::from(vec![
                Span::styled(format!(" {key:>keys$}  "), Style::new().fg(Color::Yellow)),
                Span::raw(*what),
            ]))
        })
        .collect();

    frame.render_widget(Clear, area);
    let more = HELP.len().saturating_sub(shown + app.help_scroll);
    let title = if more > 0 || app.help_scroll > 0 {
        format!("Keys — ↑↓ for {more} more")
    } else {
        "Keys".to_string()
    };
    let mut state = ListState::default().with_offset(app.help_scroll);
    frame.render_stateful_widget(
        List::new(items).block(block(&title, true)),
        area,
        &mut state,
    );
}

/// The settings panel floats over the diff: it is a detour, not a place, and the
/// diff underneath is what most of its values are about.
fn draw_settings(frame: &mut Frame, app: &mut App, body: Rect) {
    let Some(selected) = app.settings else { return };

    let names = settings::ALL
        .iter()
        .map(|setting| setting.name().chars().count())
        .max()
        .unwrap_or(0);
    // Wide enough for the longest name plus its value; that also gives the
    // explanation room to read as a sentence rather than three words a line.
    let width = (names + 24) as u16;
    let rows = settings::ALL.len() as u16;
    let area = overlay(body, width, rows + EXPLAIN_ROWS);

    let items: Vec<ListItem> = settings::ALL
        .iter()
        .enumerate()
        .map(|(index, setting)| {
            let value = format!("< {} >", setting.value(&app.config));
            let style = if index == selected {
                Style::new().bg(BG_SELECTION).add_modifier(Modifier::BOLD)
            } else {
                Style::new()
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!(" {:names$}  ", setting.name()), style),
                Span::styled(value, style.fg(Color::Yellow)),
            ]))
        })
        .collect();

    frame.render_widget(Clear, area);
    frame.render_widget(block("Settings", true), area);

    // The explanation sits inside the same block, below the list. On a terminal
    // too short for both, the list wins: the settings are what the panel is
    // for, and the explanation is only useful once the row it explains is on
    // screen.
    let inner = Rect {
        x: area.x + 1,
        y: area.y + 1,
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    };
    let help = EXPLAIN_ROWS.min(inner.height.saturating_sub(rows));

    // Stateful, so a terminal too short for the whole list still scrolls the
    // selected row into view rather than hiding it below the border.
    let mut state = ListState::default().with_selected(Some(selected));
    frame.render_stateful_widget(
        List::new(items),
        Rect {
            height: inner.height - help,
            ..inner
        },
        &mut state,
    );

    if help > 0 {
        let setting = settings::ALL[selected];
        let text = Text::from(vec![
            Line::from(Span::styled(setting.help(), Style::new().fg(FG_GUTTER))),
            Line::from(Span::styled(
                format!("{} in config.toml", setting.label()),
                Style::new().fg(FG_GUTTER).add_modifier(Modifier::DIM),
            )),
        ]);
        // Indented by insetting the area rather than by a leading space, so a
        // line that wraps keeps the indent instead of starting at the border.
        frame.render_widget(
            Paragraph::new(text).wrap(Wrap { trim: false }),
            Rect {
                x: inner.x + 1,
                y: inner.y + inner.height - help,
                width: inner.width.saturating_sub(2),
                height: help,
            },
        );
    }
}

/// Rows under the settings list for the selected row's explanation and its
/// `config.toml` key: one for the key, two for the explanation, since the
/// longest one wraps at the panel's width.
const EXPLAIN_ROWS: u16 = 3;

fn block(title: &str, focused: bool) -> Block<'_> {
    let border = if focused {
        Style::new().fg(Color::Yellow)
    } else {
        Style::new().fg(FG_GUTTER)
    };
    Block::default()
        .borders(Borders::ALL)
        .border_style(border)
        .title(Span::styled(
            format!(" {title} "),
            Style::new().add_modifier(Modifier::BOLD),
        ))
}

fn draw_files(frame: &mut Frame, app: &mut App, area: Rect) {
    if app.files.is_empty() {
        let empty = Paragraph::new("No changes")
            .style(Style::new().fg(FG_GUTTER))
            .block(block("Changes", app.focus == Pane::Files));
        frame.render_widget(empty, area);
        return;
    }

    let selected = app.selected_file().map(|f| f.path.clone());
    let items: Vec<ListItem> = app
        .files
        .iter()
        .map(|f| {
            let color = match f.status {
                Status::Modified => Color::Yellow,
                Status::Added => Color::Green,
                Status::Deleted => Color::Red,
                Status::Renamed => Color::Cyan,
            };
            // Unsaved work and an external rewrite both have to be visible
            // without selecting the file they belong to.
            let stale = app.stale && selected.as_ref() == Some(&f.path);
            let (mark, mark_color) = match (app.is_dirty(&f.path), stale) {
                (_, true) => (STALE_MARK, Color::Red),
                (true, false) => (DIRTY_MARK, Color::Yellow),
                (false, false) => (" ", Color::Reset),
            };
            ListItem::new(Line::from(vec![
                Span::styled(mark, Style::new().fg(mark_color)),
                Span::styled(format!("{} ", f.status.letter()), Style::new().fg(color)),
                Span::raw(f.path.display().to_string()),
            ]))
        })
        .collect();

    let title = format!("Changes ({})", app.files.len());
    let list = List::new(items)
        .block(block(&title, app.focus == Pane::Files))
        .highlight_style(
            Style::new()
                .bg(Color::Rgb(0x2c, 0x30, 0x3a))
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("");
    frame.render_stateful_widget(list, area, &mut app.file_state);
}

fn draw_side(frame: &mut Frame, app: &App, area: Rect, side: Pane) {
    let old_side = side == Pane::Old;
    let (title, lines_src) = if old_side {
        ("HEAD / Before".to_string(), &app.diff.old[..])
    } else {
        // The file list marks unsaved work too, but `Ctrl+B` hides it — and the
        // pane holding the edits is the last place that mark should disappear
        // from.
        let dirty = app
            .selected_file()
            .is_some_and(|file| app.is_dirty(&file.path));
        let mark = if dirty { DIRTY_MARK } else { "" };
        (
            format!("{mark} Working Tree / Current")
                .trim_start()
                .to_string(),
            app.new_lines(),
        )
    };

    let height = area.height.saturating_sub(2) as usize;
    let width = area.width.saturating_sub(2) as usize;
    let gutter = gutter_width(lines_src.len());
    let text_width = width.saturating_sub(gutter + 1);
    let tab = app.tab_width();
    let selection = (!old_side)
        .then(|| app.buffer().and_then(|b| b.selection()))
        .flatten();

    let mut lines: Vec<Line> = Vec::with_capacity(height);
    for row in app.diff.rows.iter().skip(app.scroll).take(height) {
        let index = if old_side { row.old_line } else { row.new_line };
        let Some(index) = index else {
            lines.push(Line::from(Span::styled(
                " ".repeat(width),
                Style::new().bg(BG_PHANTOM),
            )));
            continue;
        };

        let style = match (row.kind, old_side) {
            (RowKind::Equal, _) => Style::new(),
            (RowKind::Delete, _) | (RowKind::Replace, true) => Style::new().bg(BG_DELETED),
            (RowKind::Insert, _) | (RowKind::Replace, false) => Style::new().bg(BG_ADDED),
        };

        let raw = &lines_src[index];
        let mut spans = vec![Span::styled(
            format!("{:>gutter$} ", index + 1),
            Style::new().fg(FG_GUTTER),
        )];
        let selected = selection.and_then(|range| selected_columns(range, index, raw, tab));
        for (text, selected) in segments(raw, app.hscroll, text_width, tab, selected) {
            let style = if selected {
                style.bg(BG_SELECTION)
            } else {
                style
            };
            spans.push(Span::styled(text, style));
        }
        lines.push(Line::from(spans));
    }

    let widget = Paragraph::new(lines).block(block(&title, app.focus == side));
    frame.render_widget(widget, area);
}

/// Place the terminal cursor, which is what makes the Current pane read as an
/// editor rather than a viewer.
fn draw_cursor(frame: &mut Frame, app: &App, area: Rect) {
    if !app.editing() {
        return;
    }
    let Some(buffer) = app.buffer() else { return };
    let (line, col) = buffer.cursor();
    let Some(row) = app.diff.row_of_new_line(line) else {
        return;
    };
    if row < app.scroll || row >= app.scroll + app.viewport_height {
        return;
    }
    let gutter = gutter_width(app.new_lines().len()) + 1;
    let text = app.new_lines().get(line).map(String::as_str).unwrap_or("");
    let display = display_col(text, col, app.tab_width());
    let Some(x) = display.checked_sub(app.hscroll) else {
        return;
    };
    let x = area.x + 1 + (gutter + x) as u16;
    let y = area.y + 1 + (row - app.scroll) as u16;
    if x < area.right().saturating_sub(1) && y < area.bottom().saturating_sub(1) {
        frame.set_cursor_position((x, y));
    }
}

/// The two things the footer offers, in the order they are drawn. Clicking one
/// is the same as pressing its key, so the labels carry the keys with them.
pub const BUTTONS: [&str; 2] = ["[settings ,]", "[help ?]"];

fn draw_footer(frame: &mut Frame, app: &mut App, area: Rect) {
    app.buttons = [Rect::ZERO; 2];
    if let Some(prompt) = app.prompt {
        let span = Span::styled(
            format!(" {}", prompt.text()),
            Style::new().fg(Color::Yellow),
        );
        frame.render_widget(Paragraph::new(Line::from(span)), area);
        return;
    }

    let mut spans = Vec::new();
    let mut x = area.x + 1;
    for (index, label) in BUTTONS.iter().enumerate() {
        let open = (index == 0 && app.settings.is_some()) || (index == 1 && app.help);
        let style = if open {
            Style::new().fg(Color::Black).bg(Color::Yellow)
        } else {
            Style::new().fg(FG_GUTTER)
        };
        spans.push(Span::raw(" "));
        spans.push(Span::styled(*label, style));
        app.buttons[index] = Rect::new(x, area.y, label.chars().count() as u16, 1);
        x += label.chars().count() as u16 + 1;
    }
    let position = if app.diff.rows.is_empty() {
        String::new()
    } else {
        let last = (app.scroll + app.viewport_height).min(app.diff.rows.len());
        format!("{}-{}/{} ", app.scroll + 1, last, app.diff.rows.len())
    };

    // Whatever difv has to say goes after the buttons rather than replacing
    // them: the way out of a message is often one of these two panels. It is cut
    // to what is left, since the position is drawn over the same row.
    let room = (area.width as usize)
        .saturating_sub(x.saturating_sub(area.x) as usize + position.chars().count() + 2);
    let message = match (&app.error, &app.notice) {
        (Some(err), _) => Some((err.as_str(), Color::Red)),
        (None, Some(notice)) => Some((notice.as_str(), Color::Yellow)),
        (None, None) if app.stale => Some((
            "File changed on disk — `Esc` then `r` to reload",
            Color::Red,
        )),
        (None, None) => None,
    };
    if let Some((text, color)) = message {
        let text: String = text.chars().take(room).collect();
        spans.push(Span::styled(format!("  {text}"), Style::new().fg(color)));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);

    if !position.is_empty() {
        let widget = Paragraph::new(Line::from(Span::styled(
            position,
            Style::new().fg(FG_GUTTER),
        )))
        .right_aligned();
        frame.render_widget(widget, area);
    }
}

pub fn gutter_width(line_count: usize) -> usize {
    line_count.max(1).to_string().len()
}

/// Columns one character takes: a tab runs to the indent width, a CJK glyph or
/// a wide emoji takes two cells, and a combining or control character takes
/// none. Everything that positions text goes through this, so the cursor, the
/// selection and the scroll bound all agree about where a character sits.
pub fn char_width(ch: char, tab: usize) -> usize {
    if ch == '\t' {
        tab.max(1)
    } else {
        ch.width().unwrap_or(0)
    }
}

pub fn display_width(raw: &str, tab: usize) -> usize {
    raw.chars().map(|ch| char_width(ch, tab)).sum()
}

/// Display column of a character index, so the cursor lands where the text is
/// drawn rather than where it is stored.
pub fn display_col(raw: &str, char_index: usize, tab: usize) -> usize {
    raw.chars()
        .take(char_index)
        .map(|ch| char_width(ch, tab))
        .sum()
}

/// The inverse: which character a click at a display column landed on. Past the
/// end of the line it clamps to the end.
pub fn char_index_at(raw: &str, display: usize, tab: usize) -> usize {
    let mut at = 0;
    for (index, ch) in raw.chars().enumerate() {
        if at >= display {
            return index;
        }
        at += char_width(ch, tab);
    }
    raw.chars().count()
}

/// The visible part of a line, as runs of equal selectedness. One pass that
/// stops at the right edge, so a long line costs the width of the pane rather
/// than its own length. A tab, or a wide glyph straddling an edge, contributes
/// the spaces of it that are actually on screen — half a glyph cannot be drawn.
fn segments(
    raw: &str,
    hscroll: usize,
    width: usize,
    tab: usize,
    selection: Option<(usize, usize)>,
) -> Vec<(String, bool)> {
    let selected_at = |col: usize| selection.is_some_and(|(from, to)| col >= from && col < to);
    let mut runs: Vec<(String, bool)> = Vec::new();
    let push = |runs: &mut Vec<(String, bool)>, ch: char, selected: bool| match runs.last_mut() {
        Some((text, was)) if *was == selected => text.push(ch),
        _ => runs.push((ch.to_string(), selected)),
    };

    let end = hscroll + width;
    let mut col = 0;
    for ch in raw.chars() {
        if col >= end {
            break;
        }
        let cells = char_width(ch, tab);
        if cells == 0 {
            continue;
        }
        if col + cells <= hscroll {
            col += cells;
            continue;
        }
        if ch == '\t' || col < hscroll || col + cells > end {
            for cell in col.max(hscroll)..(col + cells).min(end) {
                push(&mut runs, ' ', selected_at(cell));
            }
        } else {
            push(&mut runs, ch, selected_at(col));
        }
        col += cells;
    }
    // A selection running through a line covers its newline too, so the rest of
    // the row reads as selected rather than stopping at the last character.
    for cell in col.max(hscroll)..end {
        if !selected_at(cell) {
            break;
        }
        push(&mut runs, ' ', true);
    }
    runs
}

/// The selected span of one line, in display columns, or `None` when the line
/// is outside the selection.
fn selected_columns(
    ((from_row, from_col), (to_row, to_col)): ((usize, usize), (usize, usize)),
    line: usize,
    raw: &str,
    tab: usize,
) -> Option<(usize, usize)> {
    if line < from_row || line > to_row {
        return None;
    }
    let from = if line == from_row {
        display_col(raw, from_col, tab)
    } else {
        0
    };
    let to = if line == to_row {
        display_col(raw, to_col, tab)
    } else {
        display_width(raw, tab) + 1
    };
    (to > from).then_some((from, to))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The panel is the one widget drawn over the others, so it is worth
    /// checking that it actually lands on the screen.
    #[test]
    fn the_settings_panel_renders_over_the_diff() {
        use crate::config::Config;
        use crate::git::Repo;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut app = App::with(Repo::at(".".into()), Vec::new(), Config::default());
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();

        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let plain = terminal.backend().to_string();
        assert!(!plain.contains("Settings"));

        app.settings = Some(1);
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let open = terminal.backend().to_string();
        assert!(open.contains("Settings"), "{open}");
        // A name and a value a reader can act on, not a struct definition...
        assert!(open.contains("Indent width"), "{open}");
        assert!(open.contains("< 2 spaces >"), "{open}");
        // ...with the key it writes still findable, for editing the file by hand.
        assert!(open.contains("indent_width in config.toml"), "{open}");
        assert!(
            open.contains("Spaces per indent"),
            "the selected row explains itself: {open}"
        );
        // The help is indented by its area, so it starts in the same column as
        // the setting names — and a wrapped line keeps that indent rather than
        // starting hard against the border.
        let col = |needle: &str| {
            let line = open.lines().find(|line| line.contains(needle)).unwrap();
            line.find(needle).unwrap()
        };
        assert_eq!(col("indent_width in config.toml"), col("Indent width"));

        app.settings = None;
        app.help = true;
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let open = terminal.backend().to_string();
        assert!(open.contains("Keys"), "{open}");
        assert!(open.contains("Hide / show the file list"), "{open}");
    }

    /// The file list carries an unsaved mark too, but `Ctrl+B` hides it, and the
    /// pane holding the edits is the last place that mark may vanish from.
    #[test]
    fn the_current_pane_is_marked_while_the_buffer_is_unsaved() {
        use crate::app::tests::Fixture;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let fixture = Fixture::new("dirty-title", "a\n", "b\n");
        let mut app = fixture.app();
        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();

        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert!(!terminal.backend().to_string().contains("* Working Tree"));

        app.files_hidden = true;
        app.on_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('!'),
            crossterm::event::KeyModifiers::NONE,
        ));
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let dirty = terminal.backend().to_string();
        assert!(dirty.contains("* Working Tree"), "{dirty}");

        app.on_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('s'),
            crossterm::event::KeyModifiers::CONTROL,
        ));
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert!(!terminal.backend().to_string().contains("* Working Tree"));
    }

    /// A terminal shorter than a panel must scroll it, not hide the rows below
    /// the fold while the keys still reach them.
    #[test]
    fn the_overlays_stay_usable_on_a_short_terminal() {
        use crate::config::Config;
        use crate::git::Repo;
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut app = App::with(Repo::at(".".into()), Vec::new(), Config::default());
        let mut terminal = Terminal::new(TestBackend::new(60, 7)).unwrap();

        // Four rows of list, no room left for the explanation: the last
        // setting is off-panel until it is selected, and then the list has to
        // have scrolled to show its name.
        app.settings = Some(settings::ALL.len() - 1);
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let short = terminal.backend().to_string();
        assert!(
            short.contains("Ignore scrolling after a keystroke"),
            "{short}"
        );
        assert!(
            !short.contains("config.toml"),
            "explanation yields to the list: {short}"
        );

        app.settings = None;
        app.help = true;
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let short = terminal.backend().to_string();
        assert!(
            short.contains("more"),
            "the title says there is more: {short}"
        );
        assert!(!short.contains("Quit"), "{short}");

        // The arrows reach the rest instead of closing it.
        for _ in 0..HELP.len() {
            app.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        }
        assert!(app.help, "arrows scroll rather than close");
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let short = terminal.backend().to_string();
        assert!(short.contains("Quit"), "{short}");

        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!app.help);
        assert_eq!(app.help_scroll, 0, "reopening starts at the top");
    }

    /// The footer's buttons are only clickable because it records where it drew
    /// them, which is the kind of thing that rots silently.
    #[test]
    fn the_footer_buttons_land_where_the_footer_says_they_are() {
        use crate::config::Config;
        use crate::git::Repo;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut app = App::with(Repo::at(".".into()), Vec::new(), Config::default());
        let mut terminal = Terminal::new(TestBackend::new(80, 10)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        // TestBackend quotes each rendered row, so the columns start one in.
        let footer = terminal
            .backend()
            .to_string()
            .lines()
            .last()
            .unwrap()
            .trim_matches('"')
            .to_string();
        assert!(footer.contains(BUTTONS[0]), "{footer}");
        assert!(footer.contains(BUTTONS[1]), "{footer}");
        for (index, label) in BUTTONS.iter().enumerate() {
            let rect = app.buttons[index];
            assert_eq!(rect.y, 9, "the footer is the last row");
            assert_eq!(rect.width as usize, label.chars().count());
            // The recorded start is where the label actually starts.
            assert_eq!(&footer[rect.x as usize..][..rect.width as usize], *label);
        }
    }

    #[test]
    fn expands_tabs_and_applies_horizontal_scroll() {
        let text = |raw, hscroll, width, tab| {
            segments(raw, hscroll, width, tab, None)
                .into_iter()
                .map(|(text, _)| text)
                .collect::<String>()
        };
        assert_eq!(text("\tfoo", 0, 10, 4), "    foo");
        assert_eq!(text("\tfoo", 4, 10, 4), "foo");
        assert_eq!(text("\tfoo", 0, 10, 2), "  foo");
        assert_eq!(text("abcdef", 1, 3, 4), "bcd");
        assert_eq!(text("ab", 9, 10, 4), "");
        // A tab scrolled halfway through shows only the columns still on screen.
        assert_eq!(text("\tfoo", 2, 10, 4), "  foo");
    }

    /// A CJK glyph is two cells wide, so every column it touches has to be
    /// counted twice or the cursor, the selection and the scroll bound drift
    /// apart from what is drawn.
    #[test]
    fn wide_characters_take_two_columns_everywhere() {
        assert_eq!(display_width("你好", 4), 4);
        assert_eq!(display_width("a你b", 4), 4);
        assert_eq!(display_col("你好", 1, 4), 2);
        // A click inside a wide glyph lands on it, and past it lands after it.
        assert_eq!(char_index_at("你好", 0, 4), 0);
        assert_eq!(char_index_at("你好", 2, 4), 1);
        assert_eq!(char_index_at("你好", 4, 4), 2);

        let text = |raw, hscroll, width| {
            segments(raw, hscroll, width, 4, None)
                .into_iter()
                .map(|(text, _)| text)
                .collect::<String>()
        };
        assert_eq!(text("你好世界", 0, 8), "你好世界");
        assert_eq!(text("你好世界", 2, 8), "好世界");
        // Half a glyph cannot be drawn, so the visible half is a space and the
        // row keeps its width instead of shifting everything right of it.
        assert_eq!(text("你好", 1, 8), " 好");
        assert_eq!(text("你好", 0, 3), "你 ");
    }

    #[test]
    fn a_selection_marks_the_columns_it_covers() {
        let runs = segments("hello", 0, 10, 4, Some((1, 3)));
        assert_eq!(
            runs,
            vec![
                ("h".to_string(), false),
                ("el".to_string(), true),
                ("lo".to_string(), false),
            ]
        );

        // Through-line selections cover the newline, so the row reads as one
        // block rather than stopping at the last character.
        let runs = segments("ab", 0, 6, 4, Some((0, 3)));
        assert_eq!(runs, vec![("ab ".to_string(), true)]);
    }

    #[test]
    fn gutter_grows_with_line_count() {
        assert_eq!(gutter_width(0), 1);
        assert_eq!(gutter_width(9), 1);
        assert_eq!(gutter_width(10), 2);
        assert_eq!(gutter_width(1234), 4);
    }

    #[test]
    fn display_columns_round_trip_through_tabs() {
        assert_eq!(display_col("\tfoo", 2, 4), 5);
        assert_eq!(char_index_at("\tfoo", 5, 4), 2);
        assert_eq!(char_index_at("\tfoo", 0, 4), 0);
        // Clicking inside an expanded tab lands after it, not inside it.
        assert_eq!(char_index_at("\tfoo", 2, 4), 1);
    }

    #[test]
    fn a_click_past_the_end_clamps_to_the_line_end() {
        assert_eq!(char_index_at("hello world!", 40, 4), 12);
        assert_eq!(char_index_at("", 40, 4), 0);
    }

    #[test]
    fn selection_covers_whole_lines_in_the_middle() {
        let range = ((0, 1), (2, 2));
        assert_eq!(selected_columns(range, 0, "one", 4), Some((1, 4)));
        assert_eq!(selected_columns(range, 1, "two", 4), Some((0, 4)));
        assert_eq!(selected_columns(range, 2, "three", 4), Some((0, 2)));
        assert_eq!(selected_columns(range, 3, "four", 4), None);
    }
}
