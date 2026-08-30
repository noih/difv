use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{App, Pane};
use crate::diff::{DiffRow, RowKind};
use crate::git::Status;
use crate::settings;

const BG_DELETED: Color = Color::Rgb(0x3d, 0x1e, 0x22);
const BG_ADDED: Color = Color::Rgb(0x1c, 0x33, 0x22);
const BG_PHANTOM: Color = Color::Rgb(0x1a, 0x1a, 0x1c);
const BG_SELECTION: Color = Color::Rgb(0x2d, 0x44, 0x6b);
const FG_GUTTER: Color = Color::Rgb(0x6b, 0x6b, 0x76);
/// The band behind the ruler marks, saying which rows are on screen. Bright
/// enough to find against a near-black terminal without a track behind it to
/// be read against, dark enough not to be mistaken for a mark: the marks are
/// coloured, this is only lighter.
const BG_RULER_VIEW: Color = Color::Rgb(0x4d, 0x56, 0x70);

/// Cells the viewport band never goes below, however small a share of the file
/// is on screen.
const MIN_THUMB: usize = 2;

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
    let (left, old, new) = if app.files_hidden {
        let [old, new] =
            Layout::horizontal([Constraint::Fill(w_old), Constraint::Fill(w_new)]).areas(body);
        (Rect::ZERO, old, new)
    } else {
        let [left, old, new] = Layout::horizontal([
            Constraint::Fill(w_files),
            Constraint::Fill(w_old),
            Constraint::Fill(w_new),
        ])
        .areas(body);
        (left, old, new)
    };
    // The left column is two panes stacked, in the same relative units.
    let (files, commits) = match app.files_hidden {
        true => (Rect::ZERO, Rect::ZERO),
        false => {
            let [files, commits] = Layout::vertical([
                Constraint::Fill(app.split[0]),
                Constraint::Fill(app.split[1]),
            ])
            .areas(left);
            (files, commits)
        }
    };

    app.body = body;
    app.panes = [files, commits, old, new];
    app.viewport_height = old.height.saturating_sub(2) as usize;
    app.commits_height = commits.height.saturating_sub(2) as usize;

    if !app.files_hidden {
        draw_files(frame, app, files);
        draw_commits(frame, app, commits);
    }
    let labels = app.repo.labels();
    draw_side(frame, app, old, Pane::Old, &labels);
    draw_side(frame, app, new, Pane::New, &labels);
    // One ruler, not two: the panes share a scroll position, so a second would
    // only repeat the first.
    draw_ruler(frame, app, new);
    draw_cursor(frame, app, new);
    draw_settings(frame, app, body);
    draw_help(frame, app, body);
    draw_footer(frame, app, footer);
    refresh_after_wide(frame.buffer_mut());
}

/// Ratatui only writes the cells that changed between two frames, and one
/// cell it can get wrong is the one after a wide glyph's own right half. A CJK
/// line moving one cell left puts each glyph over the cell its right half was
/// in; the terminal drops the old glyph and blanks the half that is left over
/// — one cell further on — but keeps that cell's background. Ratatui never
/// writes it: in both of its buffers that cell is the blank after a wide
/// glyph, so nothing has changed. What stays on screen is a block of the
/// line's colour, one per cell the pane moved, which a divider drag, a
/// `Ctrl+B` or a scroll scatters across a CJK diff.
///
/// The cell is asked for on every frame instead. Only the blank cell two on
/// from a wide glyph, so a CJK-heavy pane costs a few dozen cells a frame, and
/// a frame is drawn on input alone.
fn refresh_after_wide(buf: &mut ratatui::buffer::Buffer) {
    use ratatui::buffer::{Cell, CellDiffOption};
    let area = buf.area;
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right().saturating_sub(2) {
            if buf[(x, y)].symbol().width() > 1 && buf[(x + 2, y)] == Cell::EMPTY {
                buf[(x + 2, y)].set_diff_option(CellDiffOption::AlwaysUpdate);
            }
        }
    }
}

/// Every key difv answers to, except the two the footer already shows: this
/// list is what `?` opens, so telling the reader about `?` is telling them what
/// they just did.
const HELP: [(&str, &str); 14] = [
    ("↑ ↓", "Select file / scroll / move the cursor"),
    ("Enter", "Compare the commit under the cursor"),
    ("Shift+Tab", "Next pane, from anywhere"),
    ("Tab", "One indent, in the Current pane"),
    ("Alt+↑ ↓", "Previous / next change"),
    ("← →", "Scroll sideways / move the cursor"),
    ("PageUp PageDown Home End", "Scroll / move the cursor"),
    ("Ctrl+S", "Save the file"),
    ("Ctrl+Z Ctrl+Y", "Undo / redo"),
    ("Ctrl+C Ctrl+X Ctrl+V", "Copy / cut / paste"),
    ("Ctrl+B", "Hide / show the file and commit lists"),
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

/// The commit being compared. ASCII for the same reason `DIRTY_MARK` is — a
/// round dot is drawn two cells wide under a CJK font and shunts the row — and
/// not `*`, which already means an unsaved buffer one pane above.
const TARGET_MARK: &str = ">";

/// One line of history, as far down it as anyone has looked. Only the rows on
/// screen are laid out — the list behind them may hold tens of thousands —
/// and the cursor and the picked commit are drawn differently, because they
/// are two facts: where the reader is, and what is being compared.
fn draw_commits(frame: &mut Frame, app: &mut App, area: Rect) {
    let height = area.height.saturating_sub(2) as usize;
    let width = area.width.saturating_sub(2) as usize;
    let focused = app.focus == Pane::Commits;

    let mut rows: Vec<ListItem> = Vec::with_capacity(height);
    for row in app.commits.scroll..(app.commits.scroll + height).min(app.commits.len()) {
        let target = app.commits.is_target(row);
        let mark = match target {
            true => TARGET_MARK,
            false => " ",
        };
        let (id, subject) = match app.commits.at(row) {
            Some(commit) => (commit.short.clone(), commit.subject.clone()),
            None => (String::new(), "Working tree".to_string()),
        };
        let hash = match target {
            true => Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            false => Style::new().fg(FG_GUTTER),
        };
        let mut line = vec![Span::styled(mark, Style::new().fg(Color::Yellow))];
        if !id.is_empty() {
            line.push(Span::styled(format!("{id} "), hash));
        }
        // The subject is whatever the commit said, so it is cut in display
        // columns rather than bytes — a CJK subject is half as many characters
        // as it is cells wide.
        let room = width.saturating_sub(1 + id.width() + usize::from(!id.is_empty()));
        line.push(Span::raw(cut(&subject, room)));
        let style = match row == app.commits.cursor && focused {
            true => Style::new()
                .bg(Color::Rgb(0x2c, 0x30, 0x3a))
                .add_modifier(Modifier::BOLD),
            false if row == app.commits.cursor => Style::new().bg(Color::Rgb(0x23, 0x26, 0x2e)),
            false => Style::new(),
        };
        rows.push(ListItem::new(Line::from(line).style(style)));
    }

    let tip = app.commits.tip().to_string_lossy();
    let title = match tip.as_ref() {
        "HEAD" => "Commits".to_string(),
        other => format!("Commits ({other})"),
    };
    frame.render_widget(List::new(rows).block(block(&title, focused)), area);
}

/// A string cut to a number of display columns, so a wide glyph is never
/// halved by the pane's edge. A character that takes no column is dropped,
/// as `segments` drops it from a diff line: a commit subject is text anyone
/// could have written, and a control character in it would otherwise reach
/// the terminal as the escape sequence it is.
fn cut(text: &str, room: usize) -> String {
    let mut out = String::new();
    let mut col = 0;
    for ch in text.chars() {
        let cells = ch.width().unwrap_or(0);
        if cells == 0 {
            continue;
        }
        if col + cells > room {
            break;
        }
        out.push(ch);
        col += cells;
    }
    out
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

fn draw_side(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    side: Pane,
    (old_label, new_label): &(String, String),
) {
    let old_side = side == Pane::Old;
    let (title, lines_src) = if old_side {
        (format!("{old_label} / Before"), &app.diff.old[..])
    } else {
        // The file list marks unsaved work too, but `Ctrl+B` hides it — and the
        // pane holding the edits is the last place that mark should disappear
        // from.
        let dirty = app
            .selected_file()
            .is_some_and(|file| app.is_dirty(&file.path));
        let mark = if dirty { DIRTY_MARK } else { "" };
        (
            format!("{mark} {new_label} / Current")
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
    let selection = if old_side {
        app.old_selection()
    } else {
        app.buffer().and_then(|b| b.selection())
    };

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

/// What one ruler cell says about the rows it stands for. The three are the
/// Changes pane's three statuses, because they are the same three facts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mark {
    Deleted,
    Added,
    Modified,
}

impl Mark {
    /// Bars, not text, so these are backgrounds — muted enough to sit next to
    /// the panes without shouting, saturated enough to be findable at a
    /// glance. The named `Color::Red` and friends the file list uses are for
    /// glyphs, and read as warnings at full width.
    fn color(self) -> Color {
        match self {
            Mark::Deleted => Color::Rgb(0x8f, 0x3d, 0x43),
            Mark::Added => Color::Rgb(0x3f, 0x7d, 0x52),
            Mark::Modified => Color::Rgb(0x93, 0x78, 0x30),
        }
    }

    /// The same mark, inside the band. A mark is a background and so is the
    /// band, so they cannot both be drawn in that cell — instead the band
    /// brightens what it covers. One rule for the whole ruler: what is on
    /// screen is lighter. The hue is untouched, so a mark is still red, green
    /// or yellow first and on-screen second.
    fn lit(self) -> Color {
        match self {
            Mark::Deleted => Color::Rgb(0xb5, 0x60, 0x66),
            Mark::Added => Color::Rgb(0x6b, 0xa0, 0x7a),
            Mark::Modified => Color::Rgb(0xc4, 0xa2, 0x4a),
        }
    }
}

/// Rows per ruler cell, as a denominator. A diff shorter than the ruler is
/// laid out one row to one cell rather than stretched over the whole column,
/// so a mark sits beside the row it stands for while there is room for that to
/// be true.
pub fn ruler_span(rows: usize, height: usize) -> usize {
    rows.max(height)
}

/// The whole diff, one entry per cell of the ruler. A cell takes the union of
/// the rows it covers rather than the first of them: a lone deletion inside a
/// long insertion is exactly what a reader wants an overview to have kept, and
/// a cell holding both kinds cannot honestly be either.
fn ruler_marks(rows: &[DiffRow], height: usize) -> Vec<Option<Mark>> {
    let mut marks = vec![None; height];
    if height == 0 {
        return marks;
    }
    let span = ruler_span(rows.len(), height);
    for (index, row) in rows.iter().enumerate() {
        let mark = match row.kind {
            RowKind::Equal => continue,
            RowKind::Delete => Mark::Deleted,
            RowKind::Insert => Mark::Added,
            RowKind::Replace => Mark::Modified,
        };
        let cell = (index * height / span).min(height - 1);
        marks[cell] = Some(match marks[cell] {
            Some(seen) if seen != mark => Mark::Modified,
            Some(seen) => seen,
            None => mark,
        });
    }
    marks
}

/// The cells the rows on screen fall in, which is the ruler's other job: difv
/// has no scrollbar, so this band is the only thing that says how far through
/// a file the view is.
pub fn ruler_band(
    rows: usize,
    height: usize,
    scroll: usize,
    viewport: usize,
) -> std::ops::Range<usize> {
    if height == 0 || rows == 0 {
        return 0..0;
    }
    let span = ruler_span(rows, height);
    // The part of the column that stands for anything: with a diff shorter than
    // the ruler, the cells past the last row are not a place the view can be.
    let used = (rows * height).div_ceil(span).clamp(1, height);
    // A fixed length, positioned by the scroll — not each end rounded to a cell
    // on its own, which is how a thumb ends up growing and shrinking by a cell
    // as it travels: the two roundings drift in and out of phase.
    //
    // A floor under it, because proportion alone shrinks toward nothing on a
    // long enough file: a ten-thousand-line diff on a forty-row pane is a
    // single cell, which reads as a stray mark rather than as where you are.
    // Pressing the ruler jumps to the pointer rather than grabbing the thumb,
    // so this is about seeing it, not hitting it — and on a ruler with fewer
    // cells than the floor, the floor is the ruler.
    let thumb = (viewport * height)
        .div_ceil(span)
        .clamp(MIN_THUMB.min(used), used);
    // Positioned by the row it starts on, the way the marks are, and not by the
    // fraction of the scroll travelled: those are two coordinate systems, and a
    // thumb placed by the second one drifts off the cells the first one marks —
    // a change on screen drawn dim while the cell above it is lit. Clamping is
    // what still puts the end of the file at the end of the ruler.
    let start = (scroll * height / span).min(used - thumb);
    start..start + thumb
}

/// The whole diff on the Current pane's right border: marks over a band saying
/// which rows are on screen. Drawn after the pane, into the border cells it
/// already had, so the text area keeps every column it had before.
fn draw_ruler(frame: &mut Frame, app: &App, area: Rect) {
    let height = area.height.saturating_sub(2) as usize;
    if height == 0 || area.width == 0 {
        return;
    }
    let marks = ruler_marks(&app.diff.rows, height);
    let band = ruler_band(app.diff.rows.len(), height, app.scroll, app.viewport_height);
    let x = area.x + area.width - 1;
    let buffer = frame.buffer_mut();
    for (index, mark) in marks.iter().enumerate() {
        let cell = &mut buffer[(x, area.y + 1 + index as u16)];
        let banded = band.contains(&index);
        match mark {
            // A space rather than a block glyph: every block element is East
            // Asian Ambiguous, so a CJK font would draw it two cells wide in a
            // one-cell border and shunt the pane sideways.
            Some(mark) if banded => cell.set_symbol(" ").set_bg(mark.lit()),
            Some(mark) => cell.set_symbol(" ").set_bg(mark.color()),
            // The band keeps the border's own glyph where it covers no change,
            // so an unchanged stretch of the file still reads as the edge of a
            // pane. Only a mark is worth breaking the line for.
            None if banded => cell.set_bg(BG_RULER_VIEW),
            None => cell,
        };
    }
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
    let position = if app.focus == Pane::Commits {
        format!("{} ", app.commits.position())
    } else if app.diff.rows.is_empty() {
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
        assert!(
            open.contains("Hide / show the file and commit lists"),
            "{open}"
        );
    }

    /// What a terminal shows after ratatui's diff is applied to it. Ratatui's
    /// `TestBackend` sets cells one by one; a terminal does not. A wide glyph
    /// there covers two cells, and a write over either half blanks the other
    /// while keeping its background — which is what a diff that never revisits
    /// that half leaves on screen as a stray block. The model is that rule and
    /// nothing else.
    struct Terminal {
        width: usize,
        /// Per cell: the background, and which cell's glyph covers it.
        cells: Vec<(ratatui::style::Color, Option<usize>)>,
    }

    impl Terminal {
        fn new(width: usize, height: usize) -> Self {
            Self {
                width,
                cells: vec![(ratatui::style::Color::Reset, None); width * height],
            }
        }

        fn apply(&mut self, updates: &[(u16, u16, &ratatui::buffer::Cell)]) {
            for (x, y, cell) in updates {
                let at = *y as usize * self.width + *x as usize;
                let wide = cell.symbol().width() > 1;
                for i in at..=at + wide as usize {
                    // Whatever glyph was covering this cell loses its other half.
                    if let Some(other) = self.cells[i].1 {
                        self.cells[other].1 = None;
                    }
                    self.cells[i].1 = None;
                }
                self.cells[at] = (cell.bg, None);
                if wide {
                    self.cells[at + 1] = (cell.bg, Some(at));
                    self.cells[at].1 = Some(at + 1);
                }
            }
        }

        /// The cells whose background is not what `wanted` would show.
        fn wrong(&self, wanted: &ratatui::buffer::Buffer) -> Vec<(usize, usize)> {
            let mut wrong = Vec::new();
            let mut covered = None;
            for (i, cell) in wanted.content.iter().enumerate() {
                let want = match covered.take() {
                    Some(bg) => bg,
                    None => {
                        if cell.symbol().width() > 1 {
                            covered = Some(cell.bg);
                        }
                        cell.bg
                    }
                };
                if self.cells[i].0 != want {
                    wrong.push((i % self.width, i / self.width));
                }
            }
            wrong
        }
    }

    /// A pane moving one cell to the left slides every wide glyph on it over
    /// the cell the glyph's own right half used to be in. The terminal blanks
    /// what is left of the old glyph — its right half, one cell further on —
    /// but keeps that cell's background, and ratatui's diff never writes that
    /// cell: in both of its buffers it is the blank that follows a wide glyph.
    /// The block that leaves behind is what a divider drag, a `Ctrl+B` or a
    /// scroll scatters over a CJK diff.
    #[test]
    fn a_pane_moving_a_cell_leaves_no_stray_background_behind() {
        use crate::app::tests::Fixture;
        use ratatui::backend::TestBackend;

        let fixture = Fixture::new("wide-shift", "a\n", "加入的一行。\n");
        let mut app = fixture.app();
        let (width, height) = (140, 8);
        let mut terminal = ratatui::Terminal::new(TestBackend::new(width, height)).unwrap();
        let mut render = |app: &mut App| {
            let mut copy = None;
            terminal
                .draw(|frame| {
                    draw(frame, app);
                    copy = Some(frame.buffer_mut().clone());
                })
                .unwrap();
            copy.unwrap()
        };

        // Weights that sum to the width are cell widths; the second layout
        // moves the Before and Current panes one cell left each.
        app.weights = [30, 50, 60];
        let before = render(&mut app);
        app.weights = [29, 50, 61];
        let after = render(&mut app);

        let mut screen = Terminal::new(width as usize, height as usize);
        screen.apply(&ratatui::buffer::Buffer::empty(before.area).diff(&before));
        assert!(screen.wrong(&before).is_empty(), "the first frame is whole");
        screen.apply(&before.diff(&after));
        let wrong = screen.wrong(&after);
        assert!(wrong.is_empty(), "stray background at (x, y): {wrong:?}");
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

    /// The titles say what is being compared, so a revision comparison is not
    /// mistaken for the working tree.
    #[test]
    fn the_pane_titles_name_what_they_compare() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let fixture = crate::app::tests::Fixture::new("titles", "one\n", "one\n");
        fixture.write("two\n");
        fixture.git(&["commit", "-qam", "second"]);
        let second = fixture
            .git(&["rev-parse", "--short", "HEAD"])
            .trim()
            .to_string();
        fixture.write("three\n");

        let mut terminal = Terminal::new(TestBackend::new(100, 12)).unwrap();
        let mut shown = |app: &mut App| {
            terminal.draw(|frame| draw(frame, app)).unwrap();
            terminal.backend().to_string()
        };

        let plain = shown(&mut fixture.app_with_revs(&[]));
        assert!(plain.contains("HEAD / Before"), "{plain}");
        assert!(plain.contains("Working Tree / Current"), "{plain}");

        let one = shown(&mut fixture.app_with_revs(&[&second]));
        assert!(one.contains(&format!("{second} / Before")), "{one}");
        assert!(one.contains("Working Tree / Current"), "{one}");

        let range = shown(&mut fixture.app_with_revs(&[&format!("HEAD~1..{second}")]));
        assert!(range.contains("HEAD~1 / Before"), "{range}");
        assert!(range.contains(&format!("{second} / Current")), "{range}");
        // A revision on the right is read-only, so it never carries the mark
        // an unsaved buffer would.
        assert!(!range.contains(DIRTY_MARK), "{range}");
    }

    /// The Commits pane sits under the Changes list, with the Working tree
    /// row above the history, and says which row the reader is on and which
    /// commit is being compared as two different things.
    #[test]
    fn the_commits_pane_shows_the_cursor_and_the_target_apart() {
        use crate::app::tests::Fixture;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let fixture = Fixture::new("commits-pane", "one\n", "two\n");
        fixture.git(&["commit", "-qam", "second commit"]);
        fixture.write("three\n");
        let mut app = fixture.app();
        app.focus = Pane::Commits;

        let mut terminal = Terminal::new(TestBackend::new(100, 16)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let shown = terminal.backend().to_string();
        assert!(shown.contains("Commits"), "{shown}");
        assert!(shown.contains("Working tree"), "{shown}");

        // The Commits pane is under the Changes pane, in the same column.
        let changes = app.panes[Pane::Files as usize];
        let commits = app.panes[Pane::Commits as usize];
        assert_eq!(changes.x, commits.x);
        assert_eq!(changes.bottom(), commits.y);

        // Load the history, put the cursor on a commit and the target on the
        // working tree: two rows, two marks.
        app.commits_height = commits.height.saturating_sub(2) as usize;
        app.commits
            .ensure_loaded(&app.repo, app.commits_height)
            .unwrap();
        app.commits.go_to(1, app.commits_height);
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let shown = terminal.backend().to_string();
        assert!(shown.contains("second commit"), "{shown}");
        let row = |needle: &str| shown.lines().find(|l| l.contains(needle)).unwrap();
        assert!(
            row("Working tree").contains(TARGET_MARK),
            "the target is the working tree: {shown}"
        );
        assert!(
            !row("second commit").contains(TARGET_MARK),
            "which is not where the cursor is: {shown}"
        );
    }

    /// A subject is text anyone could have written. What reaches the terminal
    /// is its printable columns and nothing else — never an escape sequence.
    #[test]
    fn a_commit_subject_cannot_write_to_the_terminal() {
        // The escape byte is what makes a sequence; without it `[2J` is text.
        assert_eq!(cut("ok\x1b[2Jnot", 10), "ok[2Jnot");
        assert_eq!(cut("中文字", 4), "中文", "a wide glyph is never halved");
        assert_eq!(cut("a\u{200b}b", 5), "ab", "nor a zero-width one kept");
    }

    /// A history of thousands is drawn a screen at a time: the rows behind
    /// the ones on show are never laid out.
    #[test]
    fn only_the_visible_commits_are_drawn() {
        use crate::app::tests::Fixture;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let fixture = Fixture::new("commits-window", "a\n", "b\n");
        let mut app = fixture.app();
        app.commits.fill_for_test(1000);

        let mut terminal = Terminal::new(TestBackend::new(100, 16)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let shown = terminal.backend().to_string();
        let height = app.panes[Pane::Commits as usize].height.saturating_sub(2) as usize;
        assert!(height < 20, "the pane is a fraction of the terminal");
        assert!(shown.contains("commit 0"), "the first row: {shown}");
        assert!(
            !shown.contains(&format!("commit {height}")),
            "and nothing past the pane: {shown}"
        );
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

    /// The whole pass, on a real buffer: the marks land on the Current pane's
    /// right border, in the right cells, and the band says where the view is.
    #[test]
    fn the_ruler_marks_the_border_of_the_current_pane() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let committed: Vec<String> = (0..100).map(|i| format!("line {i}\n")).collect();
        let mut working = committed.clone();
        working[4] = "changed\n".to_string();
        working.insert(90, "added\n".to_string());
        let fixture =
            crate::app::tests::Fixture::new("ruler-render", &committed.concat(), &working.concat());

        let mut app = fixture.app();
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        // The rightmost column of the body, and the rows between the corners.
        let buffer = terminal.backend().buffer().clone();
        let x = 79;
        let ruler: Vec<Color> = (1..18).map(|y| buffer[(x, y)].bg).collect();

        // The change near the top of the file is near the top of the ruler and
        // the one near the end near the end. The view is at the top, so the
        // first is inside the band and lit, the second outside it and not.
        let at = |color: Color| ruler.iter().position(|bg| *bg == color);
        assert_eq!(at(Mark::Modified.lit()), Some(0), "{ruler:?}");
        assert_eq!(at(Mark::Modified.color()), None, "{ruler:?}");
        assert_eq!(at(Mark::Added.color()), Some(15), "{ruler:?}");
        // A banded cell with no mark is the band's own colour, and the bottom
        // of the ruler is bare border.
        assert_eq!(ruler[1], BG_RULER_VIEW, "{ruler:?}");
        assert_eq!(*ruler.last().unwrap(), Color::Reset, "{ruler:?}");
        assert_eq!(ruler.len(), 17, "one cell per row between the corners");
        // The pane's own border is still a border everywhere else.
        assert_eq!(buffer[(x, 0)].symbol(), "┐");
        assert_eq!(buffer[(x, 18)].symbol(), "┘");
        // Nothing was taken from the text: the pane to the left of the ruler
        // still holds the diff.
        assert!(terminal.backend().to_string().contains("changed"));
    }

    fn rows(kinds: &[RowKind]) -> Vec<DiffRow> {
        kinds
            .iter()
            .map(|&kind| DiffRow {
                old_line: None,
                new_line: None,
                kind,
            })
            .collect()
    }

    /// A cell stands for every row it covers, not the first of them: the whole
    /// point of an overview is that a one-line deletion in the middle of a
    /// rewrite is still on it.
    #[test]
    fn a_ruler_cell_takes_the_union_of_what_it_covers() {
        let mut kinds = vec![RowKind::Equal; 100];
        kinds[10] = RowKind::Delete;
        for kind in &mut kinds[11..20] {
            *kind = RowKind::Insert;
        }
        let marks = ruler_marks(&rows(&kinds), 10);

        assert_eq!(marks[1], Some(Mark::Modified), "both kinds in one cell");
        assert_eq!(marks[0], None);
        assert_eq!(marks[2], None);

        // Far enough apart to land in different cells, they keep their colours.
        let mut kinds = vec![RowKind::Equal; 100];
        kinds[5] = RowKind::Delete;
        kinds[95] = RowKind::Insert;
        let marks = ruler_marks(&rows(&kinds), 10);
        assert_eq!(marks[0], Some(Mark::Deleted));
        assert_eq!(marks[9], Some(Mark::Added));
        assert!(marks[1..9].iter().all(Option::is_none), "{marks:?}");
    }

    #[test]
    fn a_replaced_row_is_a_modification() {
        let marks = ruler_marks(&rows(&[RowKind::Equal, RowKind::Replace]), 4);
        assert_eq!(marks, vec![None, Some(Mark::Modified), None, None]);
    }

    /// A diff shorter than the ruler is laid out one row to one cell, so a mark
    /// sits beside the row it stands for while there is room for that.
    #[test]
    fn a_short_diff_is_not_stretched_down_the_column() {
        let kinds = [
            RowKind::Equal,
            RowKind::Insert,
            RowKind::Equal,
            RowKind::Delete,
            RowKind::Equal,
        ];
        let marks = ruler_marks(&rows(&kinds), 40);

        assert_eq!(marks[1], Some(Mark::Added));
        assert_eq!(marks[3], Some(Mark::Deleted));
        assert_eq!(marks.iter().filter(|m| m.is_some()).count(), 2);
        assert!(marks[4..].iter().all(Option::is_none));
    }

    #[test]
    fn an_unchanged_diff_leaves_the_border_alone() {
        assert!(
            ruler_marks(&rows(&[RowKind::Equal; 50]), 10)
                .iter()
                .all(Option::is_none)
        );
        assert!(ruler_marks(&rows(&[]), 4).iter().all(Option::is_none));
        assert!(ruler_marks(&rows(&[RowKind::Insert]), 0).is_empty());
    }

    /// The band is difv's only answer to "how far through am I", so it has to
    /// track the view and stop where the view stops.
    #[test]
    fn the_band_covers_the_rows_on_screen() {
        assert_eq!(ruler_band(100, 10, 0, 20), 0..2);
        assert_eq!(ruler_band(100, 10, 50, 20), 5..7);
        // The end of the file puts the thumb against the end of the ruler.
        assert_eq!(ruler_band(100, 10, 80, 20), 8..10);
        // A diff shorter than the ruler is laid out one row to one cell, so the
        // band covers the rows that exist rather than the whole column.
        assert_eq!(ruler_band(5, 40, 0, 40), 0..5);
        assert_eq!(ruler_band(5, 40, 1, 3), 1..4);
        assert_eq!(ruler_band(0, 10, 0, 20), 0..0);
        assert_eq!(ruler_band(100, 0, 0, 20), 0..0);
    }

    /// Proportion alone shrinks the band toward nothing on a long file, and a
    /// one-cell band reads as a stray mark rather than as where the view is.
    #[test]
    fn the_band_has_a_floor_under_it() {
        // Forty rows of ten thousand is a fifth of a cell, proportionally.
        assert_eq!(ruler_band(10_000, 38, 0, 40).len(), MIN_THUMB);
        assert_eq!(
            ruler_band(10_000, 38, 9_960, 40),
            36..38,
            "still reaches the end"
        );

        // Every position on the way keeps the floor and stays inside the ruler.
        for scroll in (0..=9_960).step_by(37) {
            let band = ruler_band(10_000, 38, scroll, 40);
            assert_eq!(band.len(), MIN_THUMB, "{scroll}");
            assert!(band.end <= 38, "{scroll}: {band:?}");
        }

        // A ruler with fewer cells than the floor is all band, not more.
        assert_eq!(ruler_band(500, 1, 0, 40), 0..1);
    }

    /// The band and the marks have to agree about where a row is, or a change
    /// on screen is drawn dim while the cell above it is lit. They only do if
    /// the band is positioned by the row it starts on, like the marks, rather
    /// than by the fraction of the scroll travelled.
    #[test]
    fn the_band_holds_the_cells_the_marks_put_on_screen() {
        // The case the two coordinate systems used to disagree on: the view is
        // at row 600 of 1000, which is cell 6, and the band used to be 4..6.
        assert!(ruler_band(1000, 10, 600, 10).contains(&6));

        // A viewport can straddle one cell more than a fixed-length band can
        // cover, so the two can be one cell apart — but never further, and the
        // cell the first row on screen falls in is always banded.
        for (rows, height, viewport) in [(1000, 10, 10), (208, 22, 22), (57, 17, 9), (41, 40, 12)] {
            let span = ruler_span(rows, height);
            for scroll in 0..=rows - viewport {
                let band = ruler_band(rows, height, scroll, viewport);
                let first = scroll * height / span;
                let last = (scroll + viewport - 1) * height / span;
                assert!(band.contains(&first), "{rows}/{height}/{scroll}: {band:?}");
                assert!(
                    last < band.end + 1,
                    "{rows}/{height}/{scroll}: {band:?} misses {last} by more than a cell"
                );
            }
        }
    }

    /// A thumb that grows and shrinks as it travels is the classic symptom of
    /// rounding each of its ends separately. Its length is a property of how
    /// much of the file is on screen, and that does not change while scrolling.
    #[test]
    fn the_thumb_keeps_its_length_wherever_it_is() {
        for (rows, height, viewport) in [(208, 22, 22), (1000, 30, 25), (57, 17, 9), (41, 40, 12)] {
            let lengths: Vec<usize> = (0..=rows - viewport)
                .map(|scroll| ruler_band(rows, height, scroll, viewport).len())
                .collect();
            let first = lengths[0];
            assert!(
                lengths.iter().all(|len| *len == first),
                "{rows}/{height}/{viewport}: {lengths:?}"
            );
            assert!(first >= MIN_THUMB);

            // And it reaches both ends: the top at the top, the bottom at the
            // bottom, with nothing left over.
            let used = ruler_band(rows, height, 0, viewport);
            let end = ruler_band(rows, height, rows - viewport, viewport);
            assert_eq!(used.start, 0);
            assert_eq!(end.end, (rows * height).div_ceil(ruler_span(rows, height)));
        }
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
