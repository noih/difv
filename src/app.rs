use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Result, bail};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Position, Rect};
use ratatui::widgets::ListState;

use crate::clipboard;
use crate::commits::Commits;
use crate::config::{self, Config};
use crate::diff::{self, DiffModel};
use crate::editor::{self, EditorBuffer, Selection};
use crate::git::{ChangedFile, Repo};
use crate::settings;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    Files,
    Commits,
    Old,
    New,
}

/// A question in the footer, so the diff the answer depends on stays visible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prompt {
    Quit,
    Reload,
    /// A commit was picked whose side cannot hold an editor.
    Pick,
}

impl Prompt {
    pub fn text(self) -> &'static str {
        match self {
            Prompt::Quit => "Unsaved changes. Quit anyway? (y/n)",
            Prompt::Reload => "Unsaved changes. Discard and reload? (y/n)",
            Prompt::Pick => "Unsaved changes. Discard and compare that commit? (y/n)",
        }
    }
}

pub struct App {
    pub repo: Repo,
    /// The line of history beside the diff, and where the reader is in it.
    pub commits: Commits,
    pub config: Config,
    pub files: Vec<ChangedFile>,
    pub file_state: ListState,
    pub diff: DiffModel,
    /// Buffers for files the user has opened. Only the current file and files
    /// with unsaved edits are kept, so memory tracks the working set.
    buffers: HashMap<PathBuf, EditorBuffer>,
    /// The Current side when the file cannot be edited — deleted, binary, or
    /// not valid UTF-8.
    readonly: Vec<String>,
    pub scroll: usize,
    pub hscroll: usize,
    pub focus: Pane,
    pub quit: bool,
    pub error: Option<String>,
    pub notice: Option<String>,
    /// When `notice` clears itself. Unset, it stays until the next key.
    notice_until: Option<Instant>,
    pub prompt: Option<Prompt>,
    /// The selected file was rewritten by another process and its buffer has
    /// unsaved edits, so difv flagged it instead of reloading it.
    pub stale: bool,
    disk_stamp: Option<(SystemTime, u64)>,
    /// The diff pane a mouse selection is being dragged in, from press to
    /// release. Kept apart from `focus`, which a key or the wheel can move
    /// mid-drag.
    selecting: Option<Pane>,
    /// A mouse selection on the HEAD side, `(anchor, head)` in the order it was
    /// made. That side has no editor, so this is the whole of its state.
    old_selection: Option<Selection>,
    /// Where the pointer was on the last drag that reached past the edge of the
    /// pane being selected in, so the view keeps scrolling toward it while the
    /// button is held still — the terminal only reports a drag that moves.
    edge_drag: Option<Position>,
    /// Rects from the last render, kept so mouse clicks can be mapped back.
    pub panes: [Rect; 4],
    /// The area the three panes share, so a divider drag can be bounded.
    pub body: Rect,
    pub viewport_height: usize,
    /// Relative pane widths, in cells at the time they were last set.
    pub weights: [u16; 3],
    /// The Changes pane's share of the left column against the Commits
    /// pane's, in the same relative units as `weights`.
    pub split: [u16; 2],
    /// Rows of commits on screen, correct only after a draw, like
    /// `viewport_height`.
    pub commits_height: usize,
    pub files_hidden: bool,
    /// The divider being dragged: 0 between Files and Old, 1 between Old and New.
    drag: Option<Divider>,
    /// The change ruler is being dragged, so the view keeps following the
    /// pointer until the button comes up.
    ruler_drag: bool,
    /// The selected row of the settings panel, when it is open.
    pub settings: Option<usize>,
    /// Where the settings panel writes to. `None` when there is no home
    /// directory to write into, which leaves the panel usable for this run.
    pub config_path: Option<PathBuf>,
    /// When the buffer last changed, which is what `momentum_delay_ms` counts
    /// from.
    typed_at: Option<Instant>,
    /// The key list is open.
    pub help: bool,
    /// First row of the key list on screen, for terminals too short for it.
    pub help_scroll: usize,
    /// Where the footer drew its buttons, so clicking one works.
    pub buttons: [Rect; 2],
    /// Display columns of the longest line on either side. See `measure`.
    longest: usize,
    /// Display columns of the longest line on the Before side. Cached at
    /// `load_diff`: `diff.old` cannot change until the next one, so a
    /// keystroke never needs to rescan it.
    longest_old: usize,
}

/// A pane narrower than this is unreadable, so a drag stops there.
const MIN_PANE: u16 = 8;

/// Rows a pane in the left column never goes below: a border and one row of
/// its own.
const MIN_SPLIT: u16 = 3;

/// A border two panes share, and which way a drag on it moves them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Divider {
    /// Between two columns, by index into the three.
    Column(usize),
    /// Between the Changes pane and the Commits pane.
    Split,
}

/// Columns one `←`/`→` or one sideways trackpad notch moves.
const HSCROLL_STEP: isize = 4;

/// How long the user has to be idle before a coarse stretch under the viewport
/// is refined — after typing that outran the budget, or after scrolling into a
/// stretch that did. Only armed while there is one on screen.
const REFINE_PAUSE: Duration = Duration::from_millis(300);

/// How often the view moves while a drag is held past a pane edge.
const AUTOSCROLL_TICK: Duration = Duration::from_millis(50);

/// How long a self-clearing footer notice stays.
const NOTICE_TTL: Duration = Duration::from_secs(2);

impl App {
    pub fn new(config: Config, repo: Repo, select: Option<&Path>) -> Result<Self> {
        let files = repo.changed_files()?;
        let remember = config.remember_layout;
        let mut app = Self::with(repo, files, config);
        if remember && let Some(layout) = crate::config::load_layout() {
            app.weights = layout.weights;
            app.split = layout.split;
        }
        if let Some(path) = select {
            app.select_path(path)?;
        }
        Ok(app)
    }

    /// Start on the file the command line named. A file with no changes is not
    /// an error — the rest of the list is still what the user came to see — so
    /// it says so in the footer and leaves the selection where it was. A path
    /// that is neither a change nor on disk is a typo, and refusing it here is
    /// the only place that can tell it from a file git reports as deleted,
    /// which is not on disk either.
    pub fn select_path(&mut self, path: &Path) -> Result<()> {
        let index = self
            .repo
            .relative(path)
            .and_then(|rel| self.files.iter().position(|f| f.path == rel));
        match index {
            Some(index) => self.select(index),
            None if !path.exists() => bail!("no such path: {}", path.display()),
            None => {
                self.notice = Some(format!("{} has no changes", path.display()));
                self.notice_until = Some(Instant::now() + NOTICE_TTL);
            }
        }
        Ok(())
    }

    /// Key handling is a pure state machine, so tests build an `App` directly
    /// rather than through repository discovery.
    pub fn with(repo: Repo, files: Vec<ChangedFile>, config: Config) -> Self {
        let mut app = Self {
            commits: Commits::new(std::ffi::OsString::from(repo.shape().tip())),
            repo,
            config,
            files,
            file_state: ListState::default(),
            diff: DiffModel::empty(),
            buffers: HashMap::new(),
            readonly: Vec::new(),
            scroll: 0,
            hscroll: 0,
            focus: Pane::Files,
            quit: false,
            error: None,
            notice: None,
            notice_until: None,
            prompt: None,
            stale: false,
            disk_stamp: None,
            selecting: None,
            old_selection: None,
            edge_drag: None,
            panes: [Rect::ZERO; 4],
            body: Rect::ZERO,
            viewport_height: 1,
            weights: [26, 37, 37],
            split: crate::config::DEFAULT_SPLIT,
            commits_height: 0,
            files_hidden: false,
            drag: None,
            ruler_drag: false,
            settings: None,
            // Tests build `App` directly, and a settings keypress writes; the
            // real config is not something a test may reach by default.
            config_path: if cfg!(test) {
                None
            } else {
                config::config_path()
            },
            typed_at: None,
            help: false,
            help_scroll: 0,
            buttons: [Rect::ZERO; 2],
            longest: 0,
            longest_old: 0,
        };
        if !app.files.is_empty() {
            app.file_state.select(Some(0));
        }
        app.load_diff();
        app
    }

    pub fn selected_file(&self) -> Option<&ChangedFile> {
        self.file_state.selected().and_then(|i| self.files.get(i))
    }

    pub fn buffer(&self) -> Option<&EditorBuffer> {
        let path = &self.selected_file()?.path;
        self.buffers.get(path)
    }

    fn buffer_mut(&mut self) -> Option<&mut EditorBuffer> {
        let path = self.selected_file()?.path.clone();
        self.buffers.get_mut(&path)
    }

    /// The Current side, whether it is editable or not.
    pub fn new_lines(&self) -> &[String] {
        match self.buffer() {
            Some(buffer) => buffer.lines(),
            None => &self.readonly,
        }
    }

    pub fn editing(&self) -> bool {
        self.focus == Pane::New && self.buffer().is_some()
    }

    /// The HEAD-side selection, sorted, or none when it covers nothing.
    pub fn old_selection(&self) -> Option<Selection> {
        let (a, b) = self.old_selection?;
        (a != b).then(|| (a.min(b), a.max(b)))
    }

    pub fn old_selected_text(&self) -> Option<String> {
        Some(editor::slice_selection(
            &self.diff.old,
            self.old_selection()?,
        ))
    }

    pub fn dirty_files(&self) -> Vec<&PathBuf> {
        self.buffers
            .iter()
            .filter(|(_, buffer)| buffer.dirty())
            .map(|(path, _)| path)
            .collect()
    }

    pub fn is_dirty(&self, path: &PathBuf) -> bool {
        self.buffers.get(path).is_some_and(EditorBuffer::dirty)
    }

    pub fn tab_width(&self) -> usize {
        self.buffer()
            .map(EditorBuffer::tab_width)
            .unwrap_or(self.config.indent_width)
    }

    fn load_diff(&mut self) {
        let Some(file) = self.selected_file().cloned() else {
            self.diff = DiffModel::empty();
            self.readonly.clear();
            self.longest = 0;
            self.longest_old = 0;
            return;
        };
        let head: Vec<String> = self
            .repo
            .old_content(&file)
            .lines()
            .map(str::to_string)
            .collect();
        let content = self.repo.new_content(&file);
        self.disk_stamp = self.stamp(&file);
        self.stale = false;
        self.old_selection = None;

        self.readonly.clear();
        // A revision on the Current side is read-only for the same reason a
        // binary file is: there is nothing on disk that saving it would mean.
        match content.editable().filter(|_| self.repo.worktree()) {
            Some(text) => {
                self.buffers
                    .entry(file.path.clone())
                    .or_insert_with(|| EditorBuffer::new(text, &self.config));
            }
            None => {
                self.readonly = content.text().lines().map(str::to_string).collect();
            }
        }
        // Bounded like a keystroke: a file too big to diff in one go opens at
        // once, coarse, and the part on screen is refined on the next idle.
        self.diff = DiffModel::build_bounded(head, self.new_lines());
        self.measure();
        self.scroll = 0;
        self.hscroll = 0;
    }

    /// The longest line on either side, in display columns. Kept rather than
    /// derived on demand: horizontal scrolling asks for it on every event, and
    /// only an edit or a reload can change it. A full scan of both sides — used
    /// here and nowhere on the keystroke path, which is exactly the point: at
    /// 100k lines this alone measured ~15ms, more than `rebuild`'s whole 20ms
    /// budget.
    fn measure(&mut self) {
        self.longest_old = self.measure_old();
        self.longest = self.longest_old.max(self.measure_new());
    }

    fn measure_old(&self) -> usize {
        let tab = self.tab_width();
        self.diff
            .old
            .iter()
            .map(|line| crate::ui::display_width(line, tab))
            .max()
            .unwrap_or(0)
    }

    fn measure_new(&self) -> usize {
        let tab = self.tab_width();
        self.new_lines()
            .iter()
            .map(|line| crate::ui::display_width(line, tab))
            .max()
            .unwrap_or(0)
    }

    /// Rebuild the diff after an edit. Bounded in time: a coarse result keeps
    /// typing responsive and is refined on the next pause. Does not touch
    /// `longest` — callers widen it their own way, since how cheaply that can
    /// be done depends on how much of the buffer the edit could have touched.
    fn rebuild_diff(&mut self) {
        let head = std::mem::take(&mut self.diff.old);
        self.diff = DiffModel::build_bounded(head, self.new_lines());
        self.typed_at = Some(Instant::now());
        self.follow_cursor();
    }

    /// Rebuild after an edit confined to the line the cursor now sits on —
    /// typing, backspace, Enter, a plain cut. `longest` only ever grows here,
    /// from that one line: `diff.old` is cached in `longest_old` from the last
    /// `load_diff`, and the rest of the Current side is assumed unchanged. A
    /// line getting shorter, or disappearing, leaves `longest` slightly
    /// generous rather than too small — invisible to the user, and corrected
    /// by the next `load_diff`. Too small would clamp real text out of reach,
    /// which is the bug this whole path exists to avoid.
    fn rebuild(&mut self) {
        self.rebuild_diff();
        self.widen_for_cursor_line();
    }

    fn widen_for_cursor_line(&mut self) {
        let Some(line) = self.buffer().map(|buffer| buffer.cursor().0) else {
            return;
        };
        let tab = self.tab_width();
        if let Some(width) = self
            .new_lines()
            .get(line)
            .map(|text| crate::ui::display_width(text, tab))
        {
            self.longest = self.longest.max(width);
        }
    }

    /// Rebuild after undo or redo, which can restore any earlier version of
    /// the buffer — not just the line the cursor now sits on — so widening
    /// from one line is not enough to keep `longest` from clamping real text
    /// short. Undo and redo are discrete keystrokes rather than a typing
    /// stream, so a full look at the Current side here is not the cost the
    /// 20ms ceiling exists to avoid.
    fn rebuild_remeasuring(&mut self) {
        self.rebuild_diff();
        self.longest = self.longest_old.max(self.measure_new());
    }

    /// A trackpad keeps sending scroll events after the finger has left it, and
    /// those that arrive just after a keystroke would drag the view straight
    /// back off the line being edited. There is no way to tell such an event
    /// from a deliberate one, so this is a plain guard window rather than a
    /// guess: `momentum_delay_ms = 0` turns it off, which is what the system's
    /// own inertia setting makes right.
    fn within_momentum_guard(&self) -> bool {
        let guard = Duration::from_millis(self.config.momentum_delay_ms);
        !guard.is_zero() && self.editing() && self.typed_at.is_some_and(|at| at.elapsed() < guard)
    }

    /// Give the coarse stretches under the viewport a real diff, one window at
    /// a time, within one budget in total. The first real line at or below the
    /// top of the view is found again afterwards: refinement can add rows
    /// above it, and the view must not move under the reader. The top row
    /// itself may be a phantom — a deletion has no line on this side — so the
    /// anchor is resolved forward rather than read straight off that row.
    pub fn refine(&mut self) {
        let deadline = Instant::now() + diff::BUDGET;
        let anchor = self.diff.new_line_at_or_after(self.scroll);
        let mut model = std::mem::replace(&mut self.diff, DiffModel::empty());
        // Bounded by time, and by count in case a window makes no progress.
        for _ in 0..16 {
            let view = self.scroll..self.scroll + self.viewport_height;
            let Some(index) = model.coarse_at(view.clone()) else {
                break;
            };
            model.refine(index, view, self.new_lines(), deadline);
            if let Some(line) = anchor
                && let Some(row) = model.row_of_new_line(line)
            {
                self.scroll = row;
            }
            if Instant::now() >= deadline {
                break;
            }
        }
        self.diff = model;
        // Not `follow_cursor`: refinement can run while the cursor sits far
        // outside a viewport the reader scrolled to on its own, and pulling
        // the view back to the cursor is exactly the jump the anchor above
        // exists to prevent.
        self.scroll = self.scroll.min(self.max_scroll());
    }

    /// Whether the viewport shows a stretch the last build ran out of time on.
    pub fn wants_refine(&self) -> bool {
        self.diff
            .coarse_at(self.scroll..self.scroll + self.viewport_height)
            .is_some()
    }

    fn refresh(&mut self) {
        self.buffers.clear();
        let _ = self.commits.reload(&self.repo, self.commits_height);
        self.relist();
    }

    /// The changed files again, with the reader kept where they were: the
    /// same file if it is still there, the first otherwise. Shared by `r` and
    /// by picking a commit, which differ only in what they do to the buffers.
    fn relist(&mut self) {
        let current = self.selected_file().map(|f| f.path.clone());
        match self.repo.changed_files() {
            Ok(files) => {
                self.files = files;
                self.error = None;
            }
            Err(err) => {
                self.error = Some(err.to_string());
                return;
            }
        }
        let index = current
            .and_then(|path| self.files.iter().position(|f| f.path == path))
            .unwrap_or(0);
        self.file_state
            .select((!self.files.is_empty()).then_some(index.min(self.files.len() - 1)));
        self.load_diff();
    }

    /// Compare what the row under the Commits cursor stands for, in the form
    /// the command line asked for. A pick that leaves the working tree on the
    /// Current side keeps unsaved edits — only the Before side changed — and
    /// one that does not has nowhere to put them, so it asks first, the way
    /// `r` does.
    fn pick(&mut self) {
        let row = self.commits.cursor;
        let commit = self.commits.at(row).map(|c| c.hash.clone());
        let revs = match &commit {
            Some(hash) => self.repo.shape().retarget(hash),
            None => self.repo.shape().working_tree(),
        };
        // Whether an editor survives is git's answer about the new revisions,
        // so it has to be asked before anything is thrown away.
        if !self.dirty_files().is_empty()
            && !self.repo.shape().pick_keeps_worktree(commit.as_deref())
        {
            self.prompt = Some(Prompt::Pick);
            return;
        }
        self.apply_pick(revs, commit);
    }

    /// Move the Commits cursor, fetching another page if the move reached
    /// for one. A page that will not come is a notice, not a stall.
    fn move_commit(&mut self, by: isize) {
        if let Err(err) = self
            .commits
            .move_cursor(by, &self.repo, self.commits_height)
        {
            self.notice = Some(err.to_string());
            self.notice_until = Some(Instant::now() + NOTICE_TTL);
        }
    }

    fn apply_pick(&mut self, revs: Vec<OsString>, commit: Option<String>) {
        if let Err(err) = self.repo.retarget(revs) {
            self.notice = Some(err.to_string());
            self.notice_until = Some(Instant::now() + NOTICE_TTL);
            return;
        }
        if !self.repo.worktree() {
            self.buffers.clear();
        }
        self.commits.set_target(commit);
        self.relist();
    }

    fn select(&mut self, index: usize) {
        if self.files.is_empty() || self.file_state.selected() == Some(index) {
            return;
        }
        // Unsaved edits survive navigation; everything else is cheap to rebuild.
        self.buffers.retain(|_, buffer| buffer.dirty());
        self.file_state
            .select(Some(index.min(self.files.len() - 1)));
        self.load_diff();
    }

    fn move_selection(&mut self, delta: isize) {
        let Some(current) = self.file_state.selected() else {
            return;
        };
        let next = (current as isize + delta).clamp(0, self.files.len() as isize - 1);
        self.select(next as usize);
    }

    /// The pane focus falls back to when leaving the editor, which is the file
    /// list unless it is hidden.
    fn home_pane(&self) -> Pane {
        if self.files_hidden {
            Pane::Old
        } else {
            Pane::Files
        }
    }

    fn cycle_pane(&mut self) {
        self.focus = match next_pane(self.focus) {
            // Both left panes go with `Ctrl+B`, so both are skipped.
            Pane::Files | Pane::Commits if self.files_hidden => Pane::Old,
            pane => pane,
        };
    }

    fn toggle_files(&mut self) {
        self.files_hidden = !self.files_hidden;
        // Bringing the list back is a step towards picking a file, so focus
        // follows it. Hiding it only has to move focus off a pane that is no
        // longer drawn.
        if !self.files_hidden {
            self.focus = Pane::Files;
        } else if matches!(self.focus, Pane::Files | Pane::Commits) {
            self.focus = Pane::New;
        }
    }

    /// Move a divider to a column, taking width from the pane on the other side
    /// so the pair keeps the room it had.
    fn drag_divider(&mut self, index: usize, x: u16) {
        let column = [Pane::Files, Pane::Old, Pane::New];
        let (left, right) = (
            self.panes[column[index] as usize],
            self.panes[column[index + 1] as usize],
        );
        let total = left.width + right.width;
        if total < MIN_PANE * 2 {
            return;
        }
        let width = x.saturating_sub(left.x).clamp(MIN_PANE, total - MIN_PANE);
        // Restate every drawn weight in cells, or the untouched pane keeps a
        // weight in the old units and the ratio shifts under it. Restating
        // changes the scale, so a hidden pane — which has no width to read — is
        // scaled by the same factor or it comes back a different size.
        let columns = column.map(|pane| self.panes[pane as usize]);
        let (cells, before) = self
            .weights
            .iter()
            .zip(columns)
            .filter(|(_, pane)| pane.width > 0)
            .fold((0u32, 0u32), |(cells, before), (weight, pane)| {
                (cells + pane.width as u32, before + *weight as u32)
            });
        for (weight, pane) in self.weights.iter_mut().zip(columns) {
            *weight = if pane.width > 0 {
                pane.width
            } else {
                (*weight as u32 * cells)
                    .checked_div(before)
                    .map_or(*weight, |scaled| scaled.max(1) as u16)
            };
        }
        self.weights[index] = width;
        self.weights[index + 1] = total - width;
    }

    /// The divider under a column, if any. Both borders that meet there count,
    /// so the grab area is two cells wide rather than one.
    fn divider_at(&self, at: Position) -> Option<Divider> {
        if !self.body.contains(at) {
            return None;
        }
        let column = [Pane::Files, Pane::Old, Pane::New];
        if let Some(index) = (0..2).find(|&index| {
            let edge = self.panes[column[index + 1] as usize].x;
            self.panes[column[index] as usize].width > 0 && (at.x == edge || at.x + 1 == edge)
        }) {
            return Some(Divider::Column(index));
        }
        // The two rows where the Changes pane's bottom border meets the
        // Commits pane's top one, the same two-cell grab a column divider has.
        let changes = self.panes[Pane::Files as usize];
        let commits = self.panes[Pane::Commits as usize];
        let inside = commits.width > 0 && (changes.x..changes.right()).contains(&at.x);
        (inside && (at.y + 1 == commits.y || at.y == commits.y)).then_some(Divider::Split)
    }

    /// Move rows between the Changes pane and the Commits pane. The same idea
    /// as a column drag, in one dimension less: the two share a total, and
    /// neither goes below a border and a row.
    fn drag_split(&mut self, y: u16) {
        let (top, bottom) = (
            self.panes[Pane::Files as usize],
            self.panes[Pane::Commits as usize],
        );
        let total = top.height + bottom.height;
        if total < MIN_SPLIT * 2 {
            return;
        }
        let height = y.saturating_sub(top.y).clamp(MIN_SPLIT, total - MIN_SPLIT);
        self.split = [height, total - height];
    }

    /// Columns of text one pane shows. The gutter and the borders are not
    /// scrollable, so they do not count, and the narrower side decides, or its
    /// last column is unreachable.
    fn text_width(&self) -> usize {
        let gutter = crate::ui::gutter_width(self.diff.old.len())
            .max(crate::ui::gutter_width(self.new_lines().len()))
            + 1;
        let pane = self.panes[Pane::Old as usize]
            .width
            .min(self.panes[Pane::New as usize].width) as usize;
        pane.saturating_sub(2 + gutter)
    }

    /// The furthest right that still shows text. Both sides scroll together, so
    /// the longest line on either of them sets it.
    fn max_hscroll(&self) -> usize {
        self.longest.saturating_sub(self.text_width())
    }

    fn scroll_sideways(&mut self, columns: isize) {
        let next = (self.hscroll as isize + columns).max(0) as usize;
        self.hscroll = next.min(self.max_hscroll());
    }

    fn max_scroll(&self) -> usize {
        self.diff.rows.len().saturating_sub(self.viewport_height)
    }

    /// Re-bound `scroll` and `hscroll` to the current viewport, and report
    /// whether either moved. `viewport_height` and the pane widths only
    /// become correct partway through a draw, so this is only meaningful
    /// called after one — a resize changes both, and without this the view
    /// can sit past its new maxima until the next scroll event, which reads
    /// as a blank pane and a footer that names rows past the file. That has
    /// to hold while editing too, or a resize there leaves `hscroll` blank
    /// until the next keystroke, with nothing to correct it in between.
    ///
    /// `scroll` (vertical) is left purely clamped, deliberately never pulled
    /// to the cursor's row here: this runs after *every* drawn frame, and
    /// the cursor can be far outside a viewport the reader scrolled to on
    /// purpose — with the wheel, say, while still editing elsewhere. Calling
    /// the whole of `follow_cursor` from here would snap the view straight
    /// back on the very next frame, which is the momentum-guard problem this
    /// branch already solved once, re-opened at a second call site.
    /// `hscroll` gets the same clamp, then — while editing — is widened by
    /// exactly the column the cursor's own cell needs, the horizontal half
    /// of `follow_cursor` and nothing more: a plain clamp alone would sit
    /// one column short of the cursor at the end of the widest line, since
    /// `follow_cursor` is one column more generous than `max_hscroll` there.
    pub fn clamp_scroll(&mut self) -> bool {
        // The Commits pane's height is only correct after a draw, and its
        // first page is only worth asking for once it is: a page is sized by
        // the pane. Loaded here rather than at startup so the list is there
        // before the first key, not after it.
        let rows = self.commits.len();
        if let Err(err) = self.commits.ensure_loaded(&self.repo, self.commits_height) {
            self.error = Some(err.to_string());
        }
        let (scroll, hscroll) = (self.scroll, self.hscroll);
        self.scroll = self.scroll.min(self.max_scroll());
        self.hscroll = self.hscroll.min(self.max_hscroll());
        if self.editing() {
            self.widen_hscroll_for_cursor();
        }
        // Rows that arrived are rows the frame just drawn does not have.
        rows != self.commits.len() || scroll != self.scroll || hscroll != self.hscroll
    }

    fn scroll_by(&mut self, delta: isize) {
        let next = (self.scroll as isize + delta).max(0) as usize;
        self.scroll = next.min(self.max_scroll());
    }

    fn scroll_to(&mut self, row: usize) {
        // Keep the target a few rows in from the top so context stays visible.
        self.scroll = row.saturating_sub(3).min(self.max_scroll());
    }

    /// Keep the cursor on screen after it moves or after rows shift under it,
    /// sideways as well as down: typing past the right edge would otherwise put
    /// text somewhere the cursor cannot be drawn.
    fn follow_cursor(&mut self) {
        let Some(buffer) = self.buffer() else { return };
        let (line, _) = buffer.cursor();
        if let Some(column) = self.cursor_column()
            && column < self.hscroll
        {
            self.hscroll = column;
        } else {
            self.widen_hscroll_for_cursor();
        }

        let Some(row) = self.diff.row_of_new_line(line) else {
            return;
        };
        if row < self.scroll {
            self.scroll = row;
        } else if row >= self.scroll + self.viewport_height {
            self.scroll = row + 1 - self.viewport_height;
        }
        self.scroll = self.scroll.min(self.max_scroll());
    }

    /// The cursor's display column, in the buffer being edited.
    fn cursor_column(&self) -> Option<usize> {
        let buffer = self.buffer()?;
        let (line, col) = buffer.cursor();
        self.new_lines()
            .get(line)
            .map(|text| crate::ui::display_col(text, col, self.tab_width()))
    }

    /// Widen `hscroll`, never narrow it, just enough that the cursor's own
    /// cell is on screen — the horizontal half of `follow_cursor`, shared
    /// with `clamp_scroll`, which needs exactly this much and no snap to the
    /// cursor's row.
    fn widen_hscroll_for_cursor(&mut self) {
        let Some(column) = self.cursor_column() else {
            return;
        };
        let width = self.text_width();
        if width > 0 && column >= self.hscroll + width {
            self.hscroll = column + 1 - width;
        }
    }

    fn jump_hunk(&mut self, forward: bool) {
        let from = self.scroll;
        let row = if forward {
            self.diff.changed_row_after(from)
        } else {
            self.diff.changed_row_before(from)
        };
        let Some(row) = row else { return };
        self.scroll_to(row);
        // Leaving the cursor behind would only mean the next keystroke scrolls
        // back to it.
        if self.editing()
            && let Some(line) = self.diff.new_line_at_or_after(row)
            && let Some(buffer) = self.buffer_mut()
        {
            buffer.move_cursor_to(line, 0);
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) {
        self.notice = None;
        self.notice_until = None;
        if let Some(prompt) = self.prompt {
            self.answer(prompt, key);
            return;
        }
        if self.help {
            // The arrows reach the rows a short terminal cannot show; anything
            // else closes it, since it is a reminder rather than a mode.
            match key.code {
                KeyCode::Down => self.help_scroll += 1,
                KeyCode::Up => self.help_scroll = self.help_scroll.saturating_sub(1),
                _ => {
                    self.help = false;
                    self.help_scroll = 0;
                }
            }
            return;
        }
        if let Some(row) = self.settings {
            self.on_settings_key(row, key);
            return;
        }
        if self.editing() {
            self.on_edit_key(key);
        } else {
            self.on_view_key(key);
        }
    }

    fn answer(&mut self, prompt: Prompt, key: KeyEvent) {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                self.prompt = None;
                match prompt {
                    Prompt::Quit => self.quit = true,
                    Prompt::Reload => self.refresh(),
                    Prompt::Pick => {
                        let commit = self.commits.at(self.commits.cursor).map(|c| c.hash.clone());
                        let revs = match &commit {
                            Some(hash) => self.repo.shape().retarget(hash),
                            None => self.repo.shape().working_tree(),
                        };
                        self.apply_pick(revs, commit);
                    }
                }
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => self.prompt = None,
            _ => {}
        }
    }

    /// The settings panel takes every key while it is open, so nothing it does
    /// can leak into the diff underneath. A preference is not a document: a
    /// change applies and is kept as it is made, with no save step.
    fn on_settings_key(&mut self, row: usize, key: KeyEvent) {
        let last = settings::ALL.len() - 1;
        let step = match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.settings = None;
                return;
            }
            KeyCode::Down | KeyCode::Tab => {
                self.settings = Some((row + 1).min(last));
                return;
            }
            KeyCode::Up | KeyCode::BackTab => {
                self.settings = Some(row.saturating_sub(1));
                return;
            }
            KeyCode::Home => {
                self.settings = Some(0);
                return;
            }
            KeyCode::End => {
                self.settings = Some(last);
                return;
            }
            KeyCode::Right | KeyCode::Enter | KeyCode::Char(' ') => 1,
            KeyCode::Left => -1,
            _ => return,
        };

        let before = self.config.clone();
        settings::ALL[row].adjust(&mut self.config, step);
        // A setting at the end of its range is not a change, and rewriting the
        // file for one would be surprising for a key that did nothing.
        if self.config == before {
            return;
        }
        // indent_width is `tab_width()`'s fallback for a file with nothing to
        // detect an indent from, so it can move how wide every tab renders —
        // and with it `longest` — even though no line changed.
        if settings::ALL[row] == settings::Setting::IndentWidth {
            self.measure();
        }
        let Some(path) = self.config_path.clone() else {
            self.notice = Some("No config file to save to — changes last for this run".into());
            return;
        };
        if let Err(err) = config::save(&path, &self.config) {
            self.notice = Some(format!("Could not save the config: {err}"));
        }
    }

    fn request_quit(&mut self) {
        if self.dirty_files().is_empty() {
            self.quit = true;
        } else {
            self.prompt = Some(Prompt::Quit);
        }
    }

    fn request_reload(&mut self) {
        if self.dirty_files().is_empty() {
            self.refresh();
        } else {
            self.prompt = Some(Prompt::Reload);
        }
    }

    fn on_view_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let page = self.viewport_height.max(1) as isize;
        match key.code {
            KeyCode::Char('q') => self.request_quit(),
            KeyCode::Char('c') if ctrl => {
                if self.focus == Pane::Old
                    && let Some(text) = self.old_selected_text()
                {
                    clipboard::copy(&text);
                } else {
                    self.request_quit();
                }
            }
            KeyCode::Char('r') => self.request_reload(),
            KeyCode::Char(',') => self.settings = Some(0),
            KeyCode::Char('?') => self.help = true,
            KeyCode::Char('b') if ctrl => self.toggle_files(),
            // Saving is the editor's key, and stays the editor's key — except
            // where there can be no editor to press it in, which is exactly
            // where pressing it deserves an answer rather than silence.
            KeyCode::Char('s') if ctrl && !self.repo.worktree() => self.save(),
            KeyCode::Esc => self.focus = self.home_pane(),
            // `Tab` belongs to the editor. Letting it also cycle panes means it
            // silently changes meaning at the end of the cycle, which is worse
            // than one key having a single job.
            KeyCode::BackTab => self.cycle_pane(),

            KeyCode::Down if alt => self.jump_hunk(true),
            KeyCode::Up if alt => self.jump_hunk(false),

            // The Commits pane browses without asking git anything: moving
            // the cursor is what makes a page arrive, and `Enter` alone is
            // what makes a comparison.
            KeyCode::Enter if self.focus == Pane::Commits => self.pick(),
            KeyCode::Down if self.focus == Pane::Commits => self.move_commit(1),
            KeyCode::Up if self.focus == Pane::Commits => self.move_commit(-1),
            KeyCode::PageDown if self.focus == Pane::Commits => {
                self.move_commit(self.commits_height.max(1) as isize)
            }
            KeyCode::PageUp if self.focus == Pane::Commits => {
                self.move_commit(-(self.commits_height.max(1) as isize))
            }
            KeyCode::Home if self.focus == Pane::Commits => {
                self.commits.go_to(0, self.commits_height)
            }
            KeyCode::End if self.focus == Pane::Commits => {
                self.commits.go_to(usize::MAX, self.commits_height)
            }

            KeyCode::Down if self.focus == Pane::Files => self.move_selection(1),
            KeyCode::Up if self.focus == Pane::Files => self.move_selection(-1),
            KeyCode::Down => self.scroll_by(1),
            KeyCode::Up => self.scroll_by(-1),

            KeyCode::PageDown => self.scroll_by(page),
            KeyCode::PageUp => self.scroll_by(-page),
            KeyCode::Home => self.scroll = 0,
            KeyCode::End => self.scroll = self.max_scroll(),
            KeyCode::Right => self.scroll_sideways(HSCROLL_STEP),
            KeyCode::Left => self.scroll_sideways(-HSCROLL_STEP),
            _ => {}
        }
    }

    /// Focus is the only gate on editing, so single-key shortcuts belong to the
    /// buffer here. Only difv's own chords are taken first.
    fn on_edit_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Esc => {
                self.focus = self.home_pane();
                return;
            }
            KeyCode::BackTab => {
                self.cycle_pane();
                return;
            }
            KeyCode::Char('b') if ctrl => {
                self.toggle_files();
                return;
            }
            KeyCode::Up if alt => {
                self.jump_hunk(false);
                return;
            }
            KeyCode::Down if alt => {
                self.jump_hunk(true);
                return;
            }
            KeyCode::Char('s') if ctrl => {
                self.save();
                return;
            }
            KeyCode::Char('z') if ctrl => {
                if self.buffer_mut().is_some_and(EditorBuffer::undo) {
                    self.rebuild_remeasuring();
                }
                return;
            }
            KeyCode::Char('y') if ctrl => {
                if self.buffer_mut().is_some_and(EditorBuffer::redo) {
                    self.rebuild_remeasuring();
                }
                return;
            }
            KeyCode::Char('c') if ctrl => {
                if let Some(text) = self.buffer().and_then(EditorBuffer::selected_text) {
                    clipboard::copy(&text);
                }
                return;
            }
            KeyCode::Char('x') if ctrl => {
                if let Some(text) = self.buffer().and_then(EditorBuffer::selected_text) {
                    clipboard::copy(&text);
                    if self.buffer_mut().is_some_and(EditorBuffer::cut) {
                        self.rebuild();
                    }
                }
                return;
            }
            KeyCode::Char('v') if ctrl => {
                if let Some(text) = clipboard::paste() {
                    self.paste(&text);
                }
                return;
            }
            _ => {}
        }

        if self.buffer_mut().is_some_and(|buffer| buffer.input(key)) {
            self.rebuild();
        } else {
            // A cursor move snaps the view just like an edit does, so momentum
            // has to be held off after it too.
            self.typed_at = Some(Instant::now());
            self.follow_cursor();
        }
    }

    pub fn paste(&mut self, text: &str) {
        if !self.editing() {
            return;
        }
        // `insert_str` leaves the cursor on the last line the paste produced,
        // so `rebuild`'s cursor-line widen never sees the first one — the
        // prefix that was already there plus the pasted text's own first
        // line, which can be wider than either alone. That first line is not
        // always the cursor's line *before* the insert either: pasting over a
        // selection deletes it first, which moves the cursor to the
        // selection's start, but a selection made downward has its start
        // above the cursor that made it. `selection()` already reports the
        // sorted range, so its start — when there is one — is where the
        // paste actually lands.
        let start = self
            .buffer()
            .map(|buffer| {
                buffer
                    .selection()
                    .map_or(buffer.cursor().0, |(from, _)| from.0)
            })
            .unwrap_or(0);
        if self
            .buffer_mut()
            .is_some_and(|buffer| buffer.insert_str(text))
        {
            self.rebuild();
            let end = self
                .buffer()
                .map(|buffer| buffer.cursor().0)
                .unwrap_or(start);
            let tab = self.tab_width();
            // `start` is always at or before `end`: with no selection the
            // cursor only moves forward as it inserts, and with one,
            // `selection()` is already sorted. So this is exactly the rows
            // the paste touched — O(pasted lines), not O(file).
            let width = self
                .new_lines()
                .get(start..=end)
                .into_iter()
                .flatten()
                .map(|line| crate::ui::display_width(line, tab))
                .max()
                .unwrap_or(0);
            self.longest = self.longest.max(width);
        }
    }

    fn save(&mut self) {
        let Some(file) = self.selected_file().cloned() else {
            return;
        };
        // Saving a revision would have nowhere to write. `Ctrl+S` on a file
        // that simply has no buffer is already silent; a whole side that can
        // never have one deserves to say why.
        if !self.repo.worktree() {
            self.notice = Some(format!("{} is read-only", self.repo.labels().1));
            self.notice_until = Some(Instant::now() + NOTICE_TTL);
            return;
        }
        let Some(buffer) = self.buffers.get(&file.path) else {
            return;
        };
        // Compare content rather than timestamps: an mtime that has not moved,
        // or a same-size rewrite, would let us destroy another process's work.
        let on_disk = self.repo.new_content(&file);
        let matches = on_disk
            .editable()
            .is_some_and(|text| text.body == buffer.loaded_body());
        if !matches {
            self.notice =
                Some("File changed on disk since it was opened — reload with `r` to see it".into());
            return;
        }

        let lines = buffer.lines().to_vec();
        let (eol, trailing) = (buffer.eol(), buffer.trailing_newline());
        match self.repo.write_file(&file.path, &lines, eol, trailing) {
            Ok(()) => {
                if let Some(buffer) = self.buffers.get_mut(&file.path) {
                    buffer.mark_saved();
                }
                self.disk_stamp = self.stamp(&file);
                self.stale = false;
                self.notice = Some(format!("Saved {}", file.path.display()));
            }
            Err(err) => self.notice = Some(format!("Save failed: {err}")),
        }
    }

    /// What the disk poll compares. `None` when the Current side is a
    /// revision: the disk is not what is shown, so a write to it is not a
    /// change to anything difv is displaying.
    fn stamp(&self, file: &ChangedFile) -> Option<(SystemTime, u64)> {
        if !self.repo.worktree() {
            return None;
        }
        let meta = std::fs::metadata(self.repo.abs(&file.path)).ok()?;
        Some((meta.modified().ok()?, meta.len()))
    }

    /// The files are expected to move under difv — a formatter, a build, a
    /// rebase — so a concurrent write should surface as it happens rather than
    /// at save time.
    /// One `stat` of one file per event.
    pub fn poll_disk(&mut self) {
        let Some(file) = self.selected_file().cloned() else {
            return;
        };
        let stamp = self.stamp(&file);
        if stamp == self.disk_stamp {
            return;
        }
        self.disk_stamp = stamp;

        if self
            .buffers
            .get(&file.path)
            .is_some_and(EditorBuffer::dirty)
        {
            self.stale = true;
            return;
        }
        // The buffer holds nothing the user would lose, so let the change show.
        let scroll = self.scroll;
        let hscroll = self.hscroll;
        self.buffers.remove(&file.path);
        self.load_diff();
        self.scroll = scroll.min(self.max_scroll());
        self.hscroll = hscroll.min(self.max_hscroll());
    }

    pub fn on_mouse(&mut self, mouse: MouseEvent) {
        if self.prompt.is_some() {
            return;
        }
        if matches!(
            mouse.kind,
            MouseEventKind::ScrollUp
                | MouseEventKind::ScrollDown
                | MouseEventKind::ScrollLeft
                | MouseEventKind::ScrollRight
        ) && self.within_momentum_guard()
        {
            return;
        }
        let at = Position::new(mouse.column, mouse.row);

        // An overlay takes the mouse the way it takes the keyboard. Without
        // this, a click on the panel falls through to the pane drawn under it
        // and moves the edit cursor where nothing appeared to happen.
        if let MouseEventKind::Down(MouseButton::Left) = mouse.kind
            && let Some(index) = self.button_at(at)
        {
            if index == 0 {
                self.help = false;
                self.settings = self.settings.is_none().then_some(0);
            } else {
                self.settings = None;
                self.help = !self.help;
            }
            return;
        }
        if self.settings.is_some() || self.help {
            return;
        }

        let over = self.pane_at(at);
        // Most terminals never report a horizontal wheel, so a modifier held
        // during a normal scroll is the gesture that works everywhere. Two of
        // them, because terminals disagree about which ones they forward.
        let sideways = mouse
            .modifiers
            .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT);
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                // A release outside the terminal never arrives, so a stale drag
                // would turn the next press into a resize, or leave a selection
                // still growing under an autoscroll. Cleared before the two
                // handles below are tested, since a new press ends whatever the
                // last one left behind wherever it lands.
                self.drag = None;
                self.ruler_drag = false;
                self.edge_drag = None;
                self.selecting = None;
                // The ruler is a view control drawn on a pane's border, so it
                // is tested before the pane under it: a press there must not
                // also land in the editor as a cursor move.
                if let Some(cell) = self.ruler_cell_at(at) {
                    self.ruler_drag = true;
                    self.scroll_to_ruler(cell);
                    return;
                }
                // A border between two panes is a handle first and part of a
                // pane second, so it is tested before focus moves.
                if let Some(divider) = self.divider_at(at) {
                    self.drag = Some(divider);
                    return;
                }
                let Some(pane) = over else { return };
                self.focus = pane;
                match pane {
                    Pane::Files => {
                        // +1 for the block border above the first row.
                        let top = self.panes[Pane::Files as usize].y + 1;
                        let offset = mouse.row.saturating_sub(top) as usize;
                        let index = self.file_state.offset() + offset;
                        if index < self.files.len() {
                            self.select(index);
                        }
                    }
                    // Clicking a commit is picking it: the two-step of cursor
                    // then `Enter` is for the keyboard, which has no way to
                    // say "this one" in one gesture.
                    Pane::Commits => {
                        let top = self.panes[Pane::Commits as usize].y + 1;
                        let row = self.commits.scroll + mouse.row.saturating_sub(top) as usize;
                        if row < self.commits.len() {
                            self.commits.go_to(row, self.commits_height);
                            self.pick();
                        }
                    }
                    Pane::New => {
                        if let Some((row, col)) = self.buffer_position(at) {
                            self.selecting = Some(Pane::New);
                            if let Some(buffer) = self.buffer_mut() {
                                buffer.move_cursor_to(row, col);
                                buffer.start_selection();
                            }
                        }
                    }
                    Pane::Old => {
                        if let Some(at) = self.text_position(Pane::Old, at) {
                            self.selecting = Some(Pane::Old);
                            self.old_selection = Some((at, at));
                        }
                    }
                }
            }
            // The gesture is "take this", not "go here": the row under the
            // pointer is copied and the selection stays where it was.
            MouseEventKind::Down(MouseButton::Right) if over == Some(Pane::Files) => {
                let top = self.panes[Pane::Files as usize].y + 1;
                let offset = mouse.row.saturating_sub(top) as usize;
                if let Some(file) = self.files.get(self.file_state.offset() + offset) {
                    let path = file.path.display().to_string();
                    clipboard::copy(&path);
                    self.notice = Some(format!("Copied {path}"));
                    self.notice_until = Some(Instant::now() + NOTICE_TTL);
                }
            }
            MouseEventKind::Drag(MouseButton::Left) if self.ruler_drag => {
                // The pointer leaves the one-cell column almost immediately on
                // the way down it, so the row is what matters and the column is
                // not tested again until the button comes up.
                let pane = self.panes[Pane::New as usize];
                let height = pane.height.saturating_sub(2);
                if height > 0 {
                    let cell = at.y.saturating_sub(pane.y + 1).min(height - 1);
                    self.scroll_to_ruler(cell as usize);
                }
            }
            MouseEventKind::Drag(MouseButton::Left) if let Some(divider) = self.drag => {
                match divider {
                    Divider::Column(index) => self.drag_divider(index, at.x),
                    Divider::Split => self.drag_split(at.y),
                }
            }
            MouseEventKind::Drag(MouseButton::Left) if let Some(pane) = self.selecting => {
                self.extend_selection(pane, at);
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.drag = None;
                self.ruler_drag = false;
                self.edge_drag = None;
                if self.selecting == Some(Pane::New)
                    && self
                        .buffer()
                        .and_then(EditorBuffer::selected_text)
                        .is_none()
                    && let Some(buffer) = self.buffer_mut()
                {
                    buffer.cancel_selection();
                }
                self.selecting = None;
            }
            // The wheel carries a position like any other mouse event, so the
            // pane under the pointer is the one the user means — following it
            // saves reaching for the keyboard just to say so.
            // Holding Shift turns the wheel sideways, which is the gesture that
            // works in terminals that never report a horizontal wheel at all.
            MouseEventKind::ScrollDown if sideways => {
                self.focus_diff(over);
                self.scroll_sideways(HSCROLL_STEP);
            }
            MouseEventKind::ScrollUp if sideways => {
                self.focus_diff(over);
                self.scroll_sideways(-HSCROLL_STEP);
            }
            MouseEventKind::ScrollDown => self.wheel(over, 1),
            MouseEventKind::ScrollUp => self.wheel(over, -1),
            // A two-finger sideways swipe on a trackpad, where the terminal
            // reports one. Both diff sides scroll together, so the pane under
            // the pointer only decides focus.
            MouseEventKind::ScrollRight => {
                self.focus_diff(over);
                self.scroll_sideways(HSCROLL_STEP);
            }
            MouseEventKind::ScrollLeft => {
                self.focus_diff(over);
                self.scroll_sideways(-HSCROLL_STEP);
            }
            _ => {}
        }
    }

    /// Grow the selection to the pointer. Past an edge of the pane, the view
    /// scrolls a step toward the pointer as well and the selection reaches the
    /// text that just came into view — dragging to the border is how the rest
    /// of a long line is reached, as in any editor.
    fn extend_selection(&mut self, pane: Pane, at: Position) {
        let rect = self.panes[pane as usize];
        let lines = match pane {
            Pane::Old => self.diff.old.len(),
            _ => self.new_lines().len(),
        };
        let gutter = crate::ui::gutter_width(lines) as u16 + 1;
        let (left, right) = (rect.x + 1 + gutter, (rect.x + rect.width).saturating_sub(2));
        let (top, bottom) = (rect.y + 1, (rect.y + rect.height).saturating_sub(2));

        // Worth ticking again only while there is room in the direction asked
        // for. Judged before the step rather than by whether anything moved:
        // while editing, the frame's clamp widens `hscroll` by one column for
        // the cursor at the right limit, and the next step would "move" it
        // back — a change every tick, forever.
        let room = (at.x < left && self.hscroll > 0)
            || (at.x > right && self.hscroll < self.max_hscroll())
            || (at.y < top && self.scroll > 0)
            || (at.y > bottom && self.scroll < self.max_scroll());
        self.edge_drag = room.then_some(at);
        if at.x < left {
            self.scroll_sideways(-HSCROLL_STEP);
        } else if at.x > right {
            self.scroll_sideways(HSCROLL_STEP);
        }
        if at.y < top {
            self.scroll_by(-1);
        } else if at.y > bottom {
            self.scroll_by(1);
        }
        // Past the right edge the head goes one column beyond the last visible
        // one: that is what takes in the character under it, and the end of
        // the line once the view is scrolled all the way.
        let inside = Position::new(at.x.max(left).min(right + 1), at.y.max(top).min(bottom));
        let Some((row, col)) = self.text_position(pane, inside) else {
            return;
        };
        if pane == Pane::Old {
            if let Some((anchor, _)) = self.old_selection {
                self.old_selection = Some((anchor, (row, col)));
            }
        } else if let Some(buffer) = self.buffer_mut() {
            buffer.move_cursor_to_keeping_selection(row, col);
        }
    }

    /// A drag is being held past a pane edge and the view can still move.
    pub fn dragging_past_edge(&self) -> bool {
        self.selecting.is_some() && self.edge_drag.is_some()
    }

    /// One more step of the scroll a held drag asked for.
    fn autoscroll(&mut self) {
        if let (Some(pane), Some(at)) = (self.selecting, self.edge_drag) {
            self.extend_selection(pane, at);
        }
    }

    /// How long until something is due without any input, if anything is.
    /// `None` is the idle case: the event loop blocks.
    pub fn next_tick(&self) -> Option<Duration> {
        if self.dragging_past_edge() {
            return Some(AUTOSCROLL_TICK);
        }
        let notice = self
            .notice_until
            .map(|until| until.saturating_duration_since(Instant::now()));
        let refine = self.wants_refine().then_some(REFINE_PAUSE);
        [notice, refine].into_iter().flatten().min()
    }

    /// Service whatever `next_tick` was waiting on, soonest first. A wake-up
    /// for one of them restarts the wait for the others, which only matters to
    /// the refine pause — and that is a pause, not a deadline.
    pub fn tick(&mut self) {
        if self.dragging_past_edge() {
            self.autoscroll();
        } else if self
            .notice_until
            .is_some_and(|until| until <= Instant::now())
        {
            self.notice = None;
            self.notice_until = None;
        } else if self.wants_refine() {
            self.refine();
        }
    }

    /// One wheel notch, in the pane it happened over. The file list moves by one
    /// entry: a selection is a choice, not a distance.
    fn wheel(&mut self, over: Option<Pane>, direction: isize) {
        if over == Some(Pane::Files) {
            self.focus = Pane::Files;
            self.move_selection(direction);
            return;
        }
        // The wheel over the commits scrolls them and nothing else: it never
        // moves the cursor, so it never picks.
        if over == Some(Pane::Commits) {
            self.focus = Pane::Commits;
            let by = direction * self.config.scroll_lines as isize;
            let _ = self.commits.scroll_by(by, &self.repo, self.commits_height);
            return;
        }
        self.focus_diff(over);
        self.scroll_by(direction * self.config.scroll_lines as isize);
    }

    /// Follow the pointer, unless it is over the file list, which has nothing to
    /// scroll sideways and no reason to take focus from a diff gesture.
    fn focus_diff(&mut self, over: Option<Pane>) {
        if let Some(pane) = over.filter(|pane| !matches!(pane, Pane::Files | Pane::Commits)) {
            self.focus = pane;
        }
    }

    /// Terminal cell to buffer position, through the visual row. Clicking a
    /// phantom row lands on the next real line; clicking past the end of a line
    /// lands at its end.
    pub fn buffer_position(&self, at: Position) -> Option<(usize, usize)> {
        self.text_position(Pane::New, at)
    }

    fn text_position(&self, side: Pane, at: Position) -> Option<(usize, usize)> {
        let pane = self.panes[side as usize];
        let row = self.scroll + at.y.checked_sub(pane.y + 1)? as usize;
        let (line, lines) = if side == Pane::Old {
            (self.diff.old_line_at_or_after(row)?, &self.diff.old[..])
        } else {
            (self.diff.new_line_at_or_after(row)?, self.new_lines())
        };
        let text = lines.get(line)?;
        let gutter = crate::ui::gutter_width(lines.len()) + 1;
        let column = at.x.checked_sub(pane.x + 1)? as usize;
        let display = column.saturating_sub(gutter) + self.hscroll;
        Some((
            line,
            crate::ui::char_index_at(text, display, self.tab_width()),
        ))
    }

    fn button_at(&self, at: Position) -> Option<usize> {
        (0..self.buttons.len()).find(|index| self.buttons[*index].contains(at))
    }

    /// The ruler cell a position is on, if it is on the ruler at all — the
    /// Current pane's right border, between its corners.
    fn ruler_cell_at(&self, at: Position) -> Option<usize> {
        let pane = self.panes[Pane::New as usize];
        let height = pane.height.saturating_sub(2);
        if height == 0 || pane.width == 0 || at.x != pane.right().saturating_sub(1) {
            return None;
        }
        let cell = at.y.checked_sub(pane.y + 1)?;
        (cell < height).then_some(cell as usize)
    }

    /// Scroll so the rows a ruler cell stands for sit in the middle of the
    /// viewport. A press is a jump rather than a grab: one cell covers many
    /// rows, so carrying an offset would only be more precise in the case
    /// where scrolling is not needed at all.
    fn scroll_to_ruler(&mut self, cell: usize) {
        let height = self.panes[Pane::New as usize].height.saturating_sub(2) as usize;
        if height == 0 {
            return;
        }
        // Across cells, not through them: the last cell has to reach the last
        // row, or a drag down the whole ruler stops short of the end of the
        // file by half a viewport and there is no way to ask for the rest.
        let span = crate::ui::ruler_span(self.diff.rows.len(), height);
        let row = match height {
            1 => 0,
            _ => cell * (span - 1) / (height - 1),
        };
        self.scroll = row
            .saturating_sub(self.viewport_height / 2)
            .min(self.max_scroll());
    }

    fn pane_at(&self, at: Position) -> Option<Pane> {
        [Pane::Files, Pane::Commits, Pane::Old, Pane::New]
            .into_iter()
            .find(|pane| self.panes[*pane as usize].contains(at))
    }
}

fn next_pane(pane: Pane) -> Pane {
    match pane {
        Pane::Files => Pane::Commits,
        Pane::Commits => Pane::Old,
        Pane::Old => Pane::New,
        Pane::New => Pane::Files,
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::process::Command;

    /// A real repository with one committed file, modified in the working tree.
    pub struct Fixture {
        dir: PathBuf,
    }

    impl Fixture {
        pub fn new(name: &str, committed: &str, working: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("difv-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let git = |args: &[&str]| {
                Command::new("git")
                    .arg("-C")
                    .arg(&dir)
                    .args(args)
                    .output()
                    .unwrap()
            };
            git(&["init", "-q"]);
            git(&["config", "user.email", "t@difv"]);
            git(&["config", "user.name", "difv"]);
            std::fs::write(dir.join("file.txt"), committed).unwrap();
            git(&["add", "-A"]);
            git(&["commit", "-qm", "init"]);
            std::fs::write(dir.join("file.txt"), working).unwrap();
            Self { dir }
        }

        pub fn dir(&self) -> &std::path::Path {
            &self.dir
        }

        /// For the tests that need a second file, or more history than the one
        /// commit `new` builds.
        pub fn write_file(&self, name: &str, body: &str) {
            std::fs::write(self.dir.join(name), body).unwrap();
        }

        pub fn app(&self) -> App {
            let repo = Repo::at(self.dir.clone());
            let files = repo.changed_files().unwrap();
            assert!(!files.is_empty(), "fixture must leave the file modified");
            let mut app = App::with(repo, files, Config::default());
            app.viewport_height = 20;
            app.focus = Pane::New;
            app
        }

        /// The same, comparing whatever the command line would have asked for
        /// rather than `HEAD` against the working tree.
        pub fn app_with_revs(&self, revs: &[&str]) -> App {
            let repo = Repo::discover(&self.dir)
                .unwrap()
                .with_revs(revs.iter().map(std::ffi::OsString::from).collect())
                .unwrap();
            let files = repo.changed_files().unwrap();
            assert!(!files.is_empty(), "fixture must leave something to compare");
            let mut app = App::with(repo, files, Config::default());
            app.viewport_height = 20;
            app.focus = Pane::New;
            app
        }

        fn read(&self) -> String {
            std::fs::read_to_string(self.dir.join("file.txt")).unwrap()
        }

        pub fn write(&self, text: &str) {
            std::fs::write(self.dir.join("file.txt"), text).unwrap();
        }

        pub fn git(&self, args: &[&str]) -> String {
            let out = Command::new("git")
                .arg("-C")
                .arg(&self.dir)
                .args(args)
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).into_owned()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn alt(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::ALT)
    }

    fn type_str(app: &mut App, text: &str) {
        for ch in text.chars() {
            app.on_key(key(KeyCode::Char(ch)));
        }
    }

    #[test]
    fn view_shortcuts_are_text_while_the_current_pane_has_focus() {
        let fixture = Fixture::new("shortcuts", "a\n", "b\n");
        let mut app = fixture.app();
        app.buffer_mut().unwrap().move_cursor_to(0, 1);

        app.on_key(key(KeyCode::Char('q')));
        app.on_key(key(KeyCode::Char('r')));
        assert!(!app.quit);
        assert_eq!(app.new_lines(), ["bqr"]);

        app.on_key(key(KeyCode::Esc));
        assert_eq!(app.focus, Pane::Files);
        app.on_key(key(KeyCode::Char('q')));
        // The buffer is dirty, so quitting asks first rather than discarding.
        assert!(!app.quit);
        assert_eq!(app.prompt, Some(Prompt::Quit));
        app.on_key(key(KeyCode::Char('y')));
        assert!(app.quit);
    }

    #[test]
    fn tab_only_indents_and_shift_tab_only_cycles() {
        let fixture = Fixture::new("tab", "a\n", "b\n");
        let mut app = fixture.app();
        app.buffer_mut().unwrap().move_cursor_to(0, 1);
        app.on_key(key(KeyCode::Tab));
        // Nothing in the file to detect from, so one indent is the config's.
        assert_eq!(app.new_lines(), ["b  "]);
        assert_eq!(app.focus, Pane::New);

        // `Tab` never moves focus, so it cannot change meaning under the user.
        app.focus = Pane::Files;
        app.on_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Pane::Files);

        // `Shift+Tab` reaches every pane from every pane.
        for expected in [Pane::Commits, Pane::Old, Pane::New, Pane::Files] {
            app.on_key(key(KeyCode::BackTab));
            assert_eq!(app.focus, expected);
        }
    }

    #[test]
    fn the_wheel_focuses_the_pane_under_the_pointer() {
        let fixture = Fixture::new("wheel", "a\n", "one\ntwo\nthree\nfour\nfive\n");
        let mut app = fixture.app();
        app.viewport_height = 2;
        app.panes = [
            Rect::new(0, 0, 10, 5),
            Rect::new(0, 5, 10, 5),
            Rect::new(10, 0, 20, 10),
            Rect::new(30, 0, 20, 10),
        ];

        let wheel = |pane: Pane, kind: MouseEventKind| MouseEvent {
            kind,
            column: app_column(pane),
            row: 3,
            modifiers: KeyModifiers::NONE,
        };
        fn app_column(pane: Pane) -> u16 {
            match pane {
                Pane::Files | Pane::Commits => 2,
                Pane::Old => 12,
                Pane::New => 32,
            }
        }

        app.focus = Pane::Files;
        app.on_mouse(wheel(Pane::New, MouseEventKind::ScrollDown));
        assert_eq!(app.focus, Pane::New);
        assert_eq!(app.scroll, 3);

        app.on_mouse(wheel(Pane::Old, MouseEventKind::ScrollUp));
        assert_eq!(app.focus, Pane::Old);
        assert_eq!(app.scroll, 0);

        // Over the file list the wheel moves the selection, and focus follows
        // so the arrow keys keep doing the same thing.
        app.on_mouse(wheel(Pane::Files, MouseEventKind::ScrollDown));
        assert_eq!(app.focus, Pane::Files);
    }

    #[test]
    fn the_wheel_step_comes_from_the_config() {
        let fixture = Fixture::new("wheelstep", "a\n", "one\ntwo\nthree\nfour\nfive\nsix\n");
        let repo = Repo::at(fixture.dir.clone());
        let files = repo.changed_files().unwrap();
        let config = Config {
            scroll_lines: 1,
            ..Config::default()
        };
        let mut app = App::with(repo, files, config);
        app.viewport_height = 2;
        app.panes[Pane::New as usize] = Rect::new(30, 0, 20, 10);

        let notch = MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 32,
            row: 3,
            modifiers: KeyModifiers::NONE,
        };
        app.on_mouse(notch);
        assert_eq!(app.scroll, 1);
        app.on_mouse(notch);
        assert_eq!(app.scroll, 2);
    }

    #[test]
    fn hunk_navigation_moves_the_cursor_while_editing() {
        let fixture = Fixture::new("hunk", "a\nb\nc\nd\ne\n", "a\nX\nc\nd\nY\n");
        let mut app = fixture.app();
        app.viewport_height = 2;
        app.scroll = 0;

        app.on_key(alt(KeyCode::Down));
        let row = app.diff.changed_row_after(0).unwrap();
        assert_eq!(
            app.buffer().unwrap().cursor().0,
            app.diff.new_line_at_or_after(row).unwrap()
        );
    }

    #[test]
    fn editing_updates_the_diff_without_saving() {
        let fixture = Fixture::new("live", "timeout = 3000\n", "timeout = 5000\n");
        let mut app = fixture.app();
        let buffer = app.buffer_mut().unwrap();
        buffer.move_cursor_to(0, 11);
        app.on_key(key(KeyCode::Backspace));
        type_str(&mut app, "4");

        assert_eq!(app.new_lines(), ["timeout = 4000"]);
        assert_eq!(app.diff.old, ["timeout = 3000"]);
        assert_eq!(fixture.read(), "timeout = 5000\n");
    }

    #[test]
    fn editing_a_line_back_to_head_clears_the_change() {
        let fixture = Fixture::new("erase", "a\n", "b\n");
        let mut app = fixture.app();
        app.buffer_mut().unwrap().move_cursor_to(0, 1);
        app.on_key(key(KeyCode::Backspace));
        type_str(&mut app, "a");
        assert!(
            app.diff
                .rows
                .iter()
                .all(|r| r.kind == crate::diff::RowKind::Equal)
        );
    }

    #[test]
    fn inserting_a_line_adds_a_phantom_row_and_keeps_later_rows_paired() {
        let fixture = Fixture::new("insert", "a\nb\n", "a\nb\nc\n");
        let mut app = fixture.app();
        app.buffer_mut().unwrap().move_cursor_to(0, 1);
        app.on_key(key(KeyCode::Enter));
        type_str(&mut app, "X");

        assert_eq!(app.new_lines(), ["a", "X", "b", "c"]);
        let shape: Vec<_> = app
            .diff
            .rows
            .iter()
            .map(|r| (r.old_line, r.new_line))
            .collect();
        assert_eq!(
            shape,
            [
                (Some(0), Some(0)),
                (None, Some(1)),
                (Some(1), Some(2)),
                (None, Some(3)),
            ]
        );
    }

    #[test]
    fn the_cursor_keeps_its_line_across_a_rebuild() {
        let fixture = Fixture::new("anchor", "a\nb\nc\n", "a\nb\nc\nd\n");
        let mut app = fixture.app();
        let before = app.diff.row_of_new_line(3).unwrap();

        // Insert above, which shifts every row below it.
        app.buffer_mut().unwrap().move_cursor_to(0, 1);
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.new_lines().len(), 5);
        assert_eq!(app.buffer().unwrap().cursor(), (1, 0));
        // The line that was buffer line 3 is now line 4, one row further down,
        // and the mapping reflects the rebuild rather than the old model.
        assert_eq!(app.diff.row_of_new_line(4), Some(before + 1));
    }

    #[test]
    fn saving_writes_the_file_and_leaves_the_index_alone() {
        let fixture = Fixture::new("save", "a\n", "b\n");
        let mut app = fixture.app();
        app.buffer_mut().unwrap().move_cursor_to(0, 1);
        type_str(&mut app, "!");
        assert!(app.is_dirty(&PathBuf::from("file.txt")));

        app.on_key(ctrl('s'));
        assert_eq!(fixture.read(), "b!\n");
        assert!(!app.is_dirty(&PathBuf::from("file.txt")));
        assert!(fixture.git(&["status", "--porcelain"]).starts_with(" M"));
    }

    #[test]
    fn saving_preserves_line_endings_and_trailing_newline() {
        let fixture = Fixture::new("crlf", "a\r\nb\r\n", "a\r\nb\r\nc\r\n");
        let mut app = fixture.app();
        app.buffer_mut().unwrap().move_cursor_to(1, 1);
        type_str(&mut app, "!");
        app.on_key(ctrl('s'));
        // Only line 2 was touched; the rest come back byte for byte.
        assert_eq!(fixture.read(), "a\r\nb!\r\nc\r\n");

        let fixture = Fixture::new("nonewline", "a\nb", "a\nb\nc");
        let mut app = fixture.app();
        app.buffer_mut().unwrap().move_cursor_to(1, 1);
        type_str(&mut app, "!");
        app.on_key(ctrl('s'));
        assert_eq!(fixture.read(), "a\nb!\nc");
    }

    #[test]
    fn a_save_is_refused_after_the_file_changes_on_disk() {
        let fixture = Fixture::new("conflict", "a\n", "b\n");
        let mut app = fixture.app();
        app.buffer_mut().unwrap().move_cursor_to(0, 1);
        type_str(&mut app, "!");

        fixture.write("another process wrote this\n");
        app.on_key(ctrl('s'));

        assert_eq!(fixture.read(), "another process wrote this\n");
        assert_eq!(app.new_lines(), ["b!"]);
        assert!(app.notice.as_ref().unwrap().contains("changed on disk"));
    }

    #[test]
    fn an_external_write_is_picked_up_when_the_buffer_is_clean() {
        let fixture = Fixture::new("external", "a\n", "b\n");
        let mut app = fixture.app();
        assert_eq!(app.new_lines(), ["b"]);

        // Stamps have second granularity on some filesystems; the size differs
        // here, which is the other half of the check.
        fixture.write("another process wrote this\n");
        app.poll_disk();
        assert_eq!(app.new_lines(), ["another process wrote this"]);
        assert!(!app.stale);
    }

    #[test]
    fn an_external_write_only_flags_a_dirty_buffer() {
        let fixture = Fixture::new("external-dirty", "a\n", "b\n");
        let mut app = fixture.app();
        app.buffer_mut().unwrap().move_cursor_to(0, 1);
        type_str(&mut app, "!");

        fixture.write("another process wrote this\n");
        app.poll_disk();
        assert_eq!(app.new_lines(), ["b!"]);
        assert!(app.stale);
    }

    #[test]
    fn reloading_confirms_before_discarding_edits() {
        let fixture = Fixture::new("reload", "a\n", "b\n");
        let mut app = fixture.app();
        app.buffer_mut().unwrap().move_cursor_to(0, 1);
        type_str(&mut app, "!");
        app.on_key(key(KeyCode::Esc));

        app.on_key(key(KeyCode::Char('r')));
        assert_eq!(app.prompt, Some(Prompt::Reload));
        app.on_key(key(KeyCode::Char('n')));
        assert_eq!(app.new_lines(), ["b!"]);

        app.on_key(key(KeyCode::Char('r')));
        app.on_key(key(KeyCode::Char('y')));
        assert_eq!(app.new_lines(), ["b"]);
    }

    #[test]
    fn horizontal_scrolling_stops_at_the_longest_line() {
        let fixture = Fixture::new("hscroll", "short\n", "0123456789012345678901234\n");
        let mut app = fixture.app();
        app.focus = Pane::Files;
        // Two borders plus a one-digit gutter and its space leaves 16 columns of
        // text, so a 25-column line can scroll 9 before its end is on screen.
        app.panes[Pane::Old as usize] = Rect::new(0, 0, 20, 10);
        app.panes[Pane::New as usize] = Rect::new(20, 0, 20, 10);

        for _ in 0..20 {
            app.on_key(key(KeyCode::Right));
        }
        assert_eq!(app.hscroll, 9);

        app.on_key(key(KeyCode::Left));
        assert_eq!(app.hscroll, 5);

        // A sideways trackpad swipe is the same movement, from the mouse.
        let swipe = |app: &mut App, kind: MouseEventKind| {
            app.on_mouse(MouseEvent {
                kind,
                column: 25,
                row: 3,
                modifiers: KeyModifiers::NONE,
            })
        };
        swipe(&mut app, MouseEventKind::ScrollLeft);
        assert_eq!(app.hscroll, 1);
        swipe(&mut app, MouseEventKind::ScrollLeft);
        assert_eq!(app.hscroll, 0);
        for _ in 0..5 {
            swipe(&mut app, MouseEventKind::ScrollRight);
        }
        assert_eq!(app.hscroll, 9);
        // The gesture followed the pointer into the Current pane.
        assert_eq!(app.focus, Pane::New);

        // Shift turns a vertical wheel sideways, for terminals with no
        // horizontal wheel to report.
        let scroll = app.scroll;
        app.on_mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 25,
            row: 3,
            modifiers: KeyModifiers::SHIFT,
        });
        assert_eq!(app.hscroll, 5);
        assert_eq!(app.scroll, scroll);

        // Nothing wider than the pane means no horizontal scrolling at all.
        let fixture = Fixture::new("hscroll-short", "a\n", "b\n");
        let mut app = fixture.app();
        app.focus = Pane::Files;
        app.panes[Pane::Old as usize] = Rect::new(0, 0, 20, 10);
        app.panes[Pane::New as usize] = Rect::new(20, 0, 20, 10);
        app.on_key(key(KeyCode::Right));
        assert_eq!(app.hscroll, 0);
    }

    #[test]
    fn typing_extends_how_far_horizontal_scrolling_reaches() {
        // `longest` must grow with a line the user types past every other
        // line's length, not just with a full rescan the next time a file is
        // opened — that rescan is exactly what `rebuild` must not do on every
        // keystroke.
        let fixture = Fixture::new("hscroll-typed", "x\n", "short\n");
        let mut app = fixture.app();
        app.panes[Pane::Old as usize] = Rect::new(0, 0, 20, 10);
        app.panes[Pane::New as usize] = Rect::new(20, 0, 20, 10);

        app.buffer_mut().unwrap().move_cursor_to(0, 5);
        type_str(&mut app, "0123456789012345678901234");
        assert_eq!(app.new_lines(), ["short0123456789012345678901234"]);

        // Scrolling is a view-key command, not an edit-key one; see the
        // sibling test above for the 16-columns-of-text arithmetic. The line
        // is now 30 columns wide, so its end sits at column 30 - 16 = 14.
        app.focus = Pane::Files;
        for _ in 0..20 {
            app.on_key(key(KeyCode::Right));
        }
        assert_eq!(app.hscroll, 14);
    }

    #[test]
    fn clamp_scroll_bounds_both_scrolls_to_the_current_viewport() {
        let fixture = Fixture::new("clamp", "a\n", "one\ntwo\nthree\nfour\nfive\n");
        let mut app = fixture.app();
        // Not editing, so `follow_cursor` never runs: this covers the plain
        // clamp on its own.
        app.focus = Pane::Files;
        app.viewport_height = 2;
        app.panes[Pane::Old as usize] = Rect::new(0, 0, 20, 10);
        app.panes[Pane::New as usize] = Rect::new(20, 0, 20, 10);
        // As if a resize shrank the terminal after these were set against a
        // larger one.
        app.scroll = 100;
        app.hscroll = 100;

        assert!(app.clamp_scroll(), "both scrolls moved");
        assert_eq!(app.scroll, app.diff.rows.len() - app.viewport_height);
        assert_eq!(app.hscroll, 0, "no line here is wider than the pane");
        assert!(!app.clamp_scroll(), "already in bounds, nothing to do");
    }

    /// A resize while editing must still pull `hscroll` back into range —
    /// nothing else will, until the next keystroke — but not so far back
    /// that it clips the cursor's own cell, which `follow_cursor` normally
    /// keeps one column more generous than a plain clamp allows.
    #[test]
    fn clamp_scroll_reclaims_hscroll_for_the_cursor_while_editing() {
        let fixture = Fixture::new("clamp-edit", "x\n", "AAAAAAAAAAAAAAAAAAAA\n");
        let mut app = fixture.app();
        app.panes[Pane::Old as usize] = Rect::new(0, 0, 20, 10);
        app.panes[Pane::New as usize] = Rect::new(20, 0, 20, 10);

        app.buffer_mut().unwrap().move_cursor_to(0, 20);
        app.follow_cursor();
        assert_eq!(app.hscroll, 5, "the cursor's cell is one column past 4");

        // As if a resize (or a shrinking pane during a drag) left `hscroll`
        // stuck at a value from a wider viewport.
        app.hscroll = 100;
        assert!(app.clamp_scroll());
        assert_eq!(
            app.hscroll, 5,
            "clamped back to where the cursor is still visible, not one column short of it"
        );
    }

    /// `clamp_scroll` runs after every drawn frame, so calling the whole of
    /// `follow_cursor` from it — rather than only the horizontal widen —
    /// would drag the view back to the cursor's row on the very next frame,
    /// undoing a deliberate scroll away from it while still editing: the
    /// wheel over the Current pane, say. That is exactly what the
    /// momentum-guard tests elsewhere protect against a keystroke doing;
    /// `clamp_scroll` must not reopen it from the draw loop.
    #[test]
    fn clamp_scroll_does_not_snap_the_view_back_to_the_cursor_while_editing() {
        let body: String = (0..200).map(|i| format!("line {i}\n")).collect();
        let fixture = Fixture::new("clamp-no-snap", "x\n", &body);
        let mut app = fixture.app();
        app.viewport_height = 20;
        app.panes[Pane::Old as usize] = Rect::new(0, 0, 20, 10);
        app.panes[Pane::New as usize] = Rect::new(20, 0, 20, 10);

        // The cursor stays at line 0, but the reader scrolls the view away
        // from it with the wheel, still editing elsewhere.
        app.on_mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 30,
            row: 5,
            modifiers: KeyModifiers::NONE,
        });
        let scrolled = app.scroll;
        assert!(scrolled > 0, "the wheel moved the view");

        // The first call may report the commit list arriving; what matters
        // is that the view is not corrected, now or after.
        app.clamp_scroll();
        assert!(!app.clamp_scroll(), "already in bounds, nothing to correct");
        assert_eq!(
            app.scroll, scrolled,
            "the view must stay where it was scrolled to, not snap to the cursor"
        );
    }

    #[test]
    fn pasting_widens_longest_from_the_line_the_paste_starts_on() {
        // `insert_str` leaves the cursor on the paste's last line, so a
        // widen that only looks at the pasted text or the cursor's line
        // misses the first line the paste touched — prefix plus the pasted
        // text's own first line — which is the widest line in the file here.
        let fixture = Fixture::new("paste-wide", "x\n", "AAAAAAAAAAAAAAAAAAAA\n");
        let mut app = fixture.app();
        app.panes[Pane::Old as usize] = Rect::new(0, 0, 20, 10);
        app.panes[Pane::New as usize] = Rect::new(20, 0, 20, 10);

        app.buffer_mut().unwrap().move_cursor_to(0, 20);
        app.paste("BBBBBBBBBB\nC");
        assert_eq!(
            app.new_lines(),
            ["AAAAAAAAAAAAAAAAAAAABBBBBBBBBB", "C"],
            "the paste landed where it should"
        );

        // 16 columns of text (see the sibling tests for the arithmetic), and
        // the first line is now 30 columns wide, so its end sits at 30 - 16.
        app.focus = Pane::Files;
        for _ in 0..20 {
            app.on_key(key(KeyCode::Right));
        }
        assert_eq!(app.hscroll, 14);
    }

    /// Pasting over a selection made *downward* — shift-select or a
    /// top-to-bottom drag — is where the naive "cursor's row before the
    /// insert" reading of "where did this land" breaks: `insert_str` deletes
    /// the selection first, which moves the cursor to the selection's
    /// *start*, above where it was when the selection was made. The paste
    /// lands there, not at the old cursor row.
    #[test]
    fn pasting_over_a_downward_selection_widens_from_where_it_actually_landed() {
        let fixture = Fixture::new(
            "paste-downward",
            "x\n",
            "l0\nl1\nXXXXXXXXXXXXXXXXXXXX\nl3\nl4\nl5\n",
        );
        let mut app = fixture.app();
        app.panes[Pane::Old as usize] = Rect::new(0, 0, 20, 10);
        app.panes[Pane::New as usize] = Rect::new(20, 0, 20, 10);

        // Select from the end of the widest line down to the start of a
        // later one, then paste over it.
        {
            let buffer = app.buffer_mut().unwrap();
            buffer.move_cursor_to(2, 20);
            buffer.start_selection();
            buffer.move_cursor_to_keeping_selection(5, 0);
        }
        app.paste("YYYYYYYYYYYYYYYYYYYY\nZ");

        assert_eq!(
            app.new_lines(),
            [
                "l0",
                "l1",
                "XXXXXXXXXXXXXXXXXXXXYYYYYYYYYYYYYYYYYYYY",
                "Zl5",
            ],
            "the paste landed on the row the selection started on"
        );

        // The row the selection started on is now 40 columns wide; its end
        // sits at 40 - 16 columns of visible text.
        app.focus = Pane::Files;
        for _ in 0..20 {
            app.on_key(key(KeyCode::Right));
        }
        assert_eq!(app.hscroll, 24);
    }

    #[test]
    fn typing_past_the_right_edge_brings_the_view_with_it() {
        let fixture = Fixture::new("hfollow", "a\n", "short\n");
        let mut app = fixture.app();
        app.panes[Pane::Old as usize] = Rect::new(0, 0, 20, 10);
        app.panes[Pane::New as usize] = Rect::new(20, 0, 20, 10);

        // 16 columns of text, so the cursor leaves the pane on the 17th.
        app.buffer_mut().unwrap().move_cursor_to(0, 5);
        type_str(&mut app, "0123456789012345");
        assert!(app.hscroll > 0, "the view followed the cursor");
        let cursor = app.buffer().unwrap().cursor().1;
        assert!(cursor >= app.hscroll && cursor < app.hscroll + 16);

        // And back again when the cursor returns to the start of the line.
        app.on_key(key(KeyCode::Home));
        assert_eq!(app.hscroll, 0);
    }

    #[test]
    fn a_click_resolves_through_scroll_and_horizontal_scroll() {
        let fixture = Fixture::new("click", "a\n", "one\ntwo\nthree\nfour\n");
        let mut app = fixture.app();
        app.panes[Pane::New as usize] = Rect::new(10, 2, 40, 10);
        app.scroll = 1;
        app.hscroll = 1;

        // Gutter is one digit wide plus a space, so text starts two cells in.
        let at = Position::new(10 + 1 + 2 + 1, 2 + 1 + 1);
        assert_eq!(app.buffer_position(at), Some((2, 2)));
    }

    /// The HEAD side has no editor, but a reader still wants to lift text out
    /// of it: drag to select, `Ctrl+C` copies it — and only then; with nothing
    /// selected, `Ctrl+C` there still means quit.
    #[test]
    fn dragging_across_the_head_side_selects_and_copies_it() {
        let fixture = Fixture::new("old-select", "one\ntwo\nthree\n", "one\nTWO\nthree\n");
        let mut app = fixture.app();
        app.viewport_height = 10;
        app.panes[Pane::Old as usize] = Rect::new(0, 0, 20, 10);
        app.panes[Pane::New as usize] = Rect::new(20, 0, 20, 10);
        let mouse = |kind, column, row| MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        // Gutter is one digit plus a space, so text starts at column 3 inside
        // the border. Press on "n" of "one", drag upward-left is sorted too:
        // release on "w" of "two".
        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 4, 1));
        app.on_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 5, 2));
        app.on_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 5, 2));
        assert_eq!(app.focus, Pane::Old);
        assert_eq!(app.old_selection(), Some(((0, 1), (1, 2))));
        assert_eq!(app.old_selected_text().as_deref(), Some("ne\ntw"));

        app.on_key(ctrl('c'));
        assert!(
            app.prompt.is_none() && !app.quit,
            "Ctrl+C copied, did not quit"
        );

        // A click without a drag selects nothing, so Ctrl+C is quit again.
        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 4, 1));
        app.on_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 4, 1));
        assert_eq!(app.old_selection(), None);
        app.on_key(ctrl('c'));
        assert!(app.quit);
    }

    /// A line wider than the pane can only be selected to its end if dragging
    /// past the pane's edge scrolls the view, and keeps scrolling while the
    /// button is held there — the terminal reports no drag that stands still.
    #[test]
    fn a_drag_held_past_the_edge_scrolls_until_the_line_ends() {
        let long = "abcdefghijklmnopqrstuvwxyz0123456789ABCD\n";
        let fixture = Fixture::new("edge-scroll", long, "x\n");
        let mut app = fixture.app();
        app.viewport_height = 8;
        // Inner width 18, gutter "1 " — 16 text columns for a 40-char line.
        app.panes[Pane::Old as usize] = Rect::new(0, 0, 20, 10);
        app.panes[Pane::New as usize] = Rect::new(20, 0, 20, 10);
        let mouse = |kind, column, row| MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 3, 1));
        // Into the New pane, past the Old pane's right border.
        app.on_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 25, 1));
        assert_eq!(
            app.hscroll, HSCROLL_STEP as usize,
            "one step per drag event"
        );
        assert!(
            app.dragging_past_edge(),
            "held past the edge, so keep going"
        );
        // The head reaches just past the last visible column, not the line end.
        assert_eq!(
            app.old_selection().unwrap().1,
            (0, 16 + HSCROLL_STEP as usize)
        );

        let mut ticks = 0;
        while app.dragging_past_edge() {
            app.autoscroll();
            ticks += 1;
            assert!(ticks < 100, "must stop once the view cannot move");
        }
        assert_eq!(app.hscroll, 24, "40 columns of text, 16 visible");
        assert_eq!(app.old_selected_text().as_deref(), Some(&long[..40]));

        // Back inside the pane: no more ticking.
        app.on_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 10, 1));
        assert!(!app.dragging_past_edge());
        app.on_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 10, 1));
        assert_eq!(app.edge_drag, None);
    }

    /// The editor pane has a cursor, and the frame's clamp widens the view by
    /// one column for it at the right limit; the tick must not read that as
    /// room to keep going.
    #[test]
    fn a_held_drag_in_the_editor_stops_at_the_limit_despite_the_clamp() {
        let long = "abcdefghijklmnopqrstuvwxyz0123456789ABCD\n";
        let fixture = Fixture::new("edge-scroll-new", "x\n", long);
        let mut app = fixture.app();
        app.viewport_height = 8;
        app.panes[Pane::Old as usize] = Rect::new(0, 0, 20, 10);
        app.panes[Pane::New as usize] = Rect::new(20, 0, 20, 10);
        let mouse = |kind, column, row| MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 23, 1));
        app.on_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 39, 1));
        assert!(app.dragging_past_edge());
        let mut ticks = 0;
        while app.dragging_past_edge() {
            app.autoscroll();
            app.clamp_scroll();
            ticks += 1;
            assert!(ticks < 100, "must stop once the view cannot move");
        }
        assert_eq!(
            app.buffer().unwrap().selected_text().as_deref(),
            Some(&long[..40])
        );

        // Focus moving mid-drag does not move the selection to another pane.
        app.on_key(key(KeyCode::Esc));
        app.on_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 30, 1));
        assert_eq!(app.old_selection(), None);
        app.on_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 30, 1));
    }

    /// A file on the command line is "start here", so it has to reach past the
    /// first row. The relative form of a path is `Repo::relative`'s to get
    /// right, and is tested there.
    #[test]
    fn a_file_argument_starts_on_that_file() {
        let fixture = Fixture::new("open-file", "a\n", "b\n");
        std::fs::write(fixture.dir().join("zz.txt"), "new\n").unwrap();
        let mut app = fixture.app();
        assert_eq!(app.files.len(), 2, "file.txt and zz.txt");

        app.select_path(&fixture.dir().join("zz.txt")).unwrap();
        assert_eq!(
            app.selected_file().map(|f| f.path.display().to_string()),
            Some("zz.txt".to_string())
        );
        assert_eq!(app.notice, None);
    }

    /// The file the reader most wants to open by name is often one that is no
    /// longer on disk, so the path is resolved through its directory rather
    /// than through itself.
    #[test]
    fn a_deleted_file_can_be_named_on_the_command_line() {
        let fixture = Fixture::new("open-deleted", "a\n", "b\n");
        std::fs::write(fixture.dir().join("aaa.txt"), "new\n").unwrap();
        std::fs::remove_file(fixture.dir().join("file.txt")).unwrap();
        let mut app = fixture.app();

        app.select_path(&fixture.dir().join("file.txt")).unwrap();
        let selected = app.selected_file().expect("a file is selected");
        assert_eq!(selected.path.display().to_string(), "file.txt");
        assert_eq!(selected.status, crate::git::Status::Deleted);

        // A path that is neither a change nor on disk is a typo, and the only
        // thing that tells it from the deleted file above is the list.
        let Err(err) = app.select_path(&fixture.dir().join("sub/typo.txt")) else {
            panic!("a path that does not exist is refused");
        };
        assert!(err.to_string().contains("no such path"), "{err}");
        assert!(err.to_string().contains("typo.txt"), "{err}");
    }

    /// `git rm -r src` takes the directory too, so neither the file nor its
    /// parent is there to resolve against — which is no reason for the file to
    /// stop being openable by name.
    #[test]
    fn a_deleted_directory_does_not_hide_the_files_that_were_in_it() {
        let fixture = Fixture::new("open-deleted-dir", "a\n", "b\n");
        std::fs::create_dir_all(fixture.dir().join("src/deep")).unwrap();
        std::fs::write(fixture.dir().join("src/deep/auth.ts"), "old\n").unwrap();
        fixture.git(&["add", "src/deep/auth.ts"]);
        fixture.git(&["commit", "-qm", "add src"]);
        std::fs::remove_dir_all(fixture.dir().join("src")).unwrap();
        let mut app = fixture.app();

        app.select_path(&fixture.dir().join("src/deep/auth.ts"))
            .unwrap();
        let selected = app.selected_file().expect("a file is selected");
        assert_eq!(selected.path.display().to_string(), "src/deep/auth.ts");
        assert_eq!(selected.status, crate::git::Status::Deleted);
    }

    /// Naming a file with nothing to show is not worth refusing to start over:
    /// the rest of the list is still what the reader came for.
    #[test]
    fn a_file_with_no_changes_only_notices() {
        let fixture = Fixture::new("open-unchanged", "a\n", "b\n");
        let mut app = fixture.app();
        let selected = app.file_state.selected();

        let path = fixture.dir().join("untouched.txt");
        std::fs::write(&path, "same as ever\n").unwrap();
        // Untracked would make it a change; committing only it is what leaves a
        // file with nothing to show, next to a `file.txt` that still has one.
        fixture.git(&["add", "untouched.txt"]);
        fixture.git(&["commit", "-qm", "add untouched"]);
        app.refresh();
        assert!(
            !app.files.iter().any(|f| f.path.ends_with("untouched.txt")),
            "the file has no changes: {:?}",
            app.files
        );

        app.select_path(&path).unwrap();
        assert_eq!(
            app.notice.as_deref(),
            Some(format!("{} has no changes", path.display()).as_str())
        );
        assert_eq!(app.file_state.selected(), selected, "selection untouched");
        assert!(app.next_tick().is_some(), "the notice fades on its own");
    }

    /// The same path through a repository with nothing to list at all, where
    /// there is no row for the selection to stay on.
    #[test]
    fn a_clean_repository_notices_without_a_row_to_select() {
        let fixture = Fixture::new("open-clean", "a\n", "b\n");
        let repo = Repo::at(fixture.dir().to_path_buf());
        let mut app = App::with(repo, Vec::new(), Config::default());
        assert_eq!(app.file_state.selected(), None);

        app.select_path(&fixture.dir().join("file.txt")).unwrap();
        assert!(
            app.notice
                .as_deref()
                .is_some_and(|n| n.ends_with("has no changes")),
            "{:?}",
            app.notice
        );
        assert_eq!(app.file_state.selected(), None);
    }

    /// Right-click on the file list is "take this path": copied, confirmed in
    /// the footer, and the confirmation fades on its own — without moving the
    /// selection, which is what a left click is for.
    #[test]
    fn right_click_copies_a_listed_path_and_the_notice_fades() {
        let fixture = Fixture::new("copy-path", "a\n", "b\n");
        let mut app = fixture.app();
        app.panes[Pane::Files as usize] = Rect::new(0, 0, 20, 10);
        app.panes[Pane::Old as usize] = Rect::new(20, 0, 20, 10);
        app.panes[Pane::New as usize] = Rect::new(40, 0, 20, 10);
        let selected = app.file_state.selected();
        let path = app.files[0].path.display().to_string();

        app.on_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Right),
            column: 5,
            row: 1,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(
            app.notice.as_deref(),
            Some(format!("Copied {path}").as_str())
        );
        assert_eq!(app.file_state.selected(), selected, "selection untouched");
        let wait = app.next_tick().expect("a fade is scheduled");
        assert!(wait <= NOTICE_TTL && wait > NOTICE_TTL / 2);

        // Not due yet: a tick leaves it. Due: a tick clears it.
        app.tick();
        assert!(app.notice.is_some());
        app.notice_until = Some(Instant::now());
        app.tick();
        assert_eq!(app.notice, None);
        assert_eq!(app.next_tick(), None, "idle again");

        // Right-click on a diff pane copies nothing.
        app.on_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Right),
            column: 30,
            row: 1,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.notice, None);
    }

    /// The ruler is a view control drawn on the editor's border: it scrolls,
    /// and it touches nothing the editor owns.
    #[test]
    fn pressing_the_ruler_scrolls_and_leaves_the_editor_alone() {
        let long: String = (0..200).map(|i| format!("line {i}\n")).collect();
        let edited = long.replace("line 100\n", "line one hundred\n");
        let fixture = Fixture::new("ruler", &long, &edited);
        let mut app = fixture.app();
        app.panes = [
            Rect::new(0, 0, 26, 6),
            Rect::new(0, 6, 26, 6),
            Rect::new(26, 0, 32, 12),
            Rect::new(58, 0, 32, 12),
        ];
        // The ruler is the Current pane's right border, between its corners.
        let (ruler_x, top) = (58 + 32 - 1, 1);
        let height = 12 - 2;
        // Both come from the same layout in `draw`, so a test where they differ
        // is a geometry the app never reaches.
        app.viewport_height = height;
        assert!(app.diff.rows.len() > height, "the file must not fit");

        let press = |app: &mut App, row: u16, kind: MouseEventKind| {
            app.on_mouse(MouseEvent {
                kind,
                column: ruler_x,
                row,
                modifiers: KeyModifiers::NONE,
            })
        };

        // Focus and cursor start in the editor, with a selection under way.
        app.focus = Pane::New;
        app.on_key(ctrl('a'));
        let before = app.buffer().map(|b| (b.cursor(), b.selection()));
        let selected_file = app.file_state.selected();

        press(&mut app, top + 5, MouseEventKind::Down(MouseButton::Left));
        let span = app.diff.rows.len().max(height);
        let expected = (5 * (span - 1) / (height - 1)).saturating_sub(app.viewport_height / 2);
        assert_eq!(app.scroll, expected.min(app.max_scroll()));
        assert!(app.scroll > 0, "a press halfway down scrolls");
        // The rows that cell marks are the rows now on screen.
        let banded =
            crate::ui::ruler_band(app.diff.rows.len(), height, app.scroll, app.viewport_height);
        assert!(
            banded.contains(&5),
            "{banded:?} should hold the cell pressed"
        );

        assert_eq!(app.focus, Pane::New, "focus untouched");
        assert_eq!(app.buffer().map(|b| (b.cursor(), b.selection())), before);
        assert_eq!(app.file_state.selected(), selected_file);
        assert_eq!(app.old_selection(), None);
        // Nothing is left armed to turn the next drag into a text selection.
        assert_eq!(app.selecting, None);
        assert_eq!(app.edge_drag, None);

        // A drag follows the pointer and stops at the end of the file.
        press(
            &mut app,
            top + height as u16 - 1,
            MouseEventKind::Drag(MouseButton::Left),
        );
        assert_eq!(app.scroll, app.max_scroll());

        // Once the button is up the same motion is no longer a scroll.
        press(
            &mut app,
            top + height as u16 - 1,
            MouseEventKind::Up(MouseButton::Left),
        );
        app.scroll = 0;
        press(&mut app, top + 5, MouseEventKind::Drag(MouseButton::Left));
        assert_eq!(app.scroll, 0, "the drag was disarmed");
    }

    /// The border cell one column to the left is the pane, not the ruler, and a
    /// press there must still reach the editor.
    #[test]
    fn only_the_ruler_column_is_the_ruler() {
        let fixture = Fixture::new("ruler-column", "a\nb\nc\n", "a\nB\nc\n");
        let mut app = fixture.app();
        let pane = Rect::new(58, 0, 32, 12);
        app.panes = [
            Rect::new(0, 0, 26, 6),
            Rect::new(0, 6, 26, 6),
            Rect::new(26, 0, 32, 12),
            pane,
        ];

        assert_eq!(app.ruler_cell_at(Position::new(89, 1)), Some(0));
        assert_eq!(app.ruler_cell_at(Position::new(89, 10)), Some(9));
        assert_eq!(
            app.ruler_cell_at(Position::new(88, 5)),
            None,
            "inside the pane"
        );
        assert_eq!(app.ruler_cell_at(Position::new(89, 0)), None, "the corner");
        assert_eq!(app.ruler_cell_at(Position::new(89, 11)), None, "the corner");

        // A pane with no room between its corners has no ruler at all.
        app.panes[Pane::New as usize] = Rect::new(58, 0, 32, 2);
        assert_eq!(app.ruler_cell_at(Position::new(89, 1)), None);
    }

    #[test]
    fn dragging_a_divider_moves_width_between_two_panes() {
        let fixture = Fixture::new("resize", "a\n", "b\n");
        let mut app = fixture.app();
        app.body = Rect::new(0, 0, 90, 10);
        app.panes = [
            Rect::new(0, 0, 26, 5),
            Rect::new(0, 5, 26, 5),
            Rect::new(26, 0, 32, 10),
            Rect::new(58, 0, 32, 10),
        ];

        let drag = |app: &mut App, x: u16, kind: MouseEventKind| {
            app.on_mouse(MouseEvent {
                kind,
                column: x,
                row: 5,
                modifiers: KeyModifiers::NONE,
            })
        };

        // Grabbing the shared border and dragging right widens Files and
        // narrows Old; the third pane is untouched.
        drag(&mut app, 26, MouseEventKind::Down(MouseButton::Left));
        drag(&mut app, 40, MouseEventKind::Drag(MouseButton::Left));
        assert_eq!(app.weights, [40, 18, 32]);

        // A drag past the far pane stops at the minimum rather than erasing it.
        drag(&mut app, 90, MouseEventKind::Drag(MouseButton::Left));
        assert_eq!(app.weights, [26 + 32 - MIN_PANE, MIN_PANE, 32]);

        drag(&mut app, 90, MouseEventKind::Up(MouseButton::Left));
        // The button is up, so the same motion is no longer a resize.
        let weights = app.weights;
        drag(&mut app, 30, MouseEventKind::Drag(MouseButton::Left));
        assert_eq!(app.weights, weights);
    }

    #[test]
    fn hiding_the_file_list_keeps_focus_and_cycling_usable() {
        let fixture = Fixture::new("hide", "a\n", "b\n");
        let mut app = fixture.app();
        app.focus = Pane::Files;

        app.on_key(ctrl('b'));
        assert!(app.files_hidden);
        // Focus cannot stay on a pane that is no longer drawn.
        assert_eq!(app.focus, Pane::New);
        // Cycling and Esc skip the hidden pane.
        app.on_key(key(KeyCode::Esc));
        assert_eq!(app.focus, Pane::Old);
        app.on_key(key(KeyCode::BackTab));
        assert_eq!(app.focus, Pane::New);
        app.on_key(key(KeyCode::BackTab));
        assert_eq!(app.focus, Pane::Old);

        // Ctrl+B works from inside the editor too, and showing the list focuses
        // it, since that is what it is being shown for.
        app.focus = Pane::New;
        app.on_key(ctrl('b'));
        assert!(!app.files_hidden);
        assert_eq!(app.focus, Pane::Files);
    }

    #[test]
    fn the_settings_panel_edits_the_live_config_and_swallows_keys() {
        let fixture = Fixture::new("settings", "a\n", "b\n");
        let mut app = fixture.app();
        // Never the real config file, and near the fixture so it is cleaned up.
        let config_file = fixture.dir.join("config.toml");
        app.config_path = Some(config_file.clone());
        app.focus = Pane::Files;
        app.on_key(key(KeyCode::Char(',')));
        assert_eq!(app.settings, Some(0));

        // `q` would quit and `↓` would change file, but the panel has the keys.
        app.on_key(key(KeyCode::Down));
        assert_eq!(app.settings, Some(1));
        assert_eq!(app.file_state.selected(), Some(0));

        // Row 1 is indent_width. Every change is live and lands on disk with no
        // save step of its own.
        assert!(!config_file.exists());
        assert_eq!(app.config.indent_width, 2);
        app.on_key(key(KeyCode::Right));
        assert_eq!(app.config.indent_width, 3);
        assert!(
            std::fs::read_to_string(&config_file)
                .unwrap()
                .contains("indent_width = 3")
        );

        app.on_key(key(KeyCode::Left));
        app.on_key(key(KeyCode::Left));
        assert_eq!(app.config.indent_width, 1);
        assert!(
            std::fs::read_to_string(&config_file)
                .unwrap()
                .contains("indent_width = 1")
        );

        // At the bottom of its range the key changes nothing, so it writes
        // nothing either.
        let stamp = std::fs::metadata(&config_file).unwrap().modified().unwrap();
        app.on_key(key(KeyCode::Left));
        assert_eq!(
            std::fs::metadata(&config_file).unwrap().modified().unwrap(),
            stamp
        );
        assert!(app.notice.is_none());

        app.on_key(key(KeyCode::Esc));
        assert_eq!(app.settings, None);
        assert!(!app.quit);
        app.on_key(key(KeyCode::Char('q')));
        assert!(app.quit);
    }

    #[test]
    fn the_momentum_guard_holds_the_view_and_can_be_turned_off() {
        let body: String = (0..200).map(|i| format!("line {i}\n")).collect();
        let fixture = Fixture::new("momentum", &body, &body.replace("line 3\n", "LINE 3\n"));
        let mut app = fixture.app();
        app.viewport_height = 20;
        app.panes[Pane::New as usize] = Rect::new(76, 0, 44, 22);
        let wheel = |app: &mut App| {
            app.on_mouse(MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: 80,
                row: 5,
                modifiers: KeyModifiers::NONE,
            })
        };

        for _ in 0..30 {
            wheel(&mut app);
        }
        assert!(app.scroll > 0);
        type_str(&mut app, "X");
        assert_eq!(app.scroll, 0, "typing jumps back to the cursor");

        // Inside the window, the tail of the flick is ignored.
        wheel(&mut app);
        assert_eq!(app.scroll, 0);

        // Outside it, the wheel is the user's again.
        app.typed_at =
            Some(std::time::Instant::now() - Duration::from_millis(app.config.momentum_delay_ms));
        wheel(&mut app);
        assert_eq!(app.scroll, app.config.scroll_lines);

        // And with the guard off there is no window at all.
        app.config.momentum_delay_ms = 0;
        type_str(&mut app, "Y");
        assert_eq!(app.scroll, 0);
        wheel(&mut app);
        assert_eq!(app.scroll, app.config.scroll_lines);
    }

    /// An overlay owns the mouse the way it owns the keyboard. Without that, a
    /// click on the panel reaches the pane drawn underneath and moves the edit
    /// cursor somewhere the user never saw.
    #[test]
    fn an_open_overlay_swallows_the_mouse_but_not_its_own_button() {
        let fixture = Fixture::new("overlay-mouse", "a\n", "one\ntwo\nthree\nfour\n");
        let mut app = fixture.app();
        app.body = Rect::new(0, 0, 90, 12);
        app.panes = [
            Rect::new(0, 0, 26, 6),
            Rect::new(0, 6, 26, 6),
            Rect::new(26, 0, 32, 12),
            Rect::new(58, 0, 32, 12),
        ];
        app.buttons = [Rect::new(1, 12, 12, 1), Rect::new(14, 12, 8, 1)];
        app.buffer_mut().unwrap().move_cursor_to(3, 0);
        let before = app.buffer().unwrap().cursor();

        let click = |app: &mut App, x: u16, y: u16| {
            app.on_mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: x,
                row: y,
                modifiers: KeyModifiers::NONE,
            })
        };

        // Clicking the footer opens the panel, and clicking it again closes it.
        click(&mut app, 3, 12);
        assert_eq!(app.settings, Some(0));

        // A click over the Current pane, where the panel is drawn, changes
        // nothing at all: not the cursor, not the focus, not the scroll.
        let focus = app.focus;
        click(&mut app, 70, 4);
        assert_eq!(app.buffer().unwrap().cursor(), before);
        assert_eq!(app.focus, focus);

        // Nor does the wheel reach the diff underneath.
        app.on_mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 70,
            row: 4,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.scroll, 0);

        // The other button still swaps panels, and its own click closes it.
        click(&mut app, 16, 12);
        assert!(app.help);
        assert_eq!(app.settings, None);
        click(&mut app, 16, 12);
        assert!(!app.help);

        // With both closed the same click on the pane works normally again.
        click(&mut app, 70, 4);
        assert_eq!(app.focus, Pane::New);
        assert_ne!(app.buffer().unwrap().cursor(), before);
    }

    #[test]
    fn a_divider_can_be_grabbed_by_either_of_its_two_border_cells() {
        let fixture = Fixture::new("grab", "a\n", "b\n");
        let mut app = fixture.app();
        app.body = Rect::new(0, 0, 90, 10);
        app.panes = [
            Rect::new(0, 0, 26, 5),
            Rect::new(0, 5, 26, 5),
            Rect::new(26, 0, 32, 10),
            Rect::new(58, 0, 32, 10),
        ];

        // The Files pane's right border and the Old pane's left one are two
        // cells that mean the same divider.
        assert_eq!(
            app.divider_at(Position::new(25, 5)),
            Some(Divider::Column(0))
        );
        assert_eq!(
            app.divider_at(Position::new(26, 5)),
            Some(Divider::Column(0))
        );
        assert_eq!(
            app.divider_at(Position::new(57, 5)),
            Some(Divider::Column(1))
        );
        assert_eq!(app.divider_at(Position::new(40, 5)), None);
        // Outside the panes there is nothing to grab.
        assert_eq!(app.divider_at(Position::new(26, 11)), None);

        // A hidden file list leaves no divider where its border used to be.
        app.panes[Pane::Files as usize] = Rect::ZERO;
        app.panes[Pane::Old as usize] = Rect::new(0, 0, 45, 10);
        app.panes[Pane::New as usize] = Rect::new(45, 0, 45, 10);
        assert_eq!(app.divider_at(Position::new(0, 5)), None);
        assert_eq!(
            app.divider_at(Position::new(45, 5)),
            Some(Divider::Column(1))
        );
    }

    /// Refinement changes how many rows sit above the viewport, so the row at
    /// the top has to be found again by the line it shows, or the view jumps.
    #[test]
    fn refining_under_the_viewport_keeps_the_top_line_where_it_was() {
        let committed: String = (0..200).map(|i| format!("line {i}\n")).collect();
        let mut lines: Vec<String> = committed.lines().map(str::to_string).collect();
        lines.remove(20);
        lines.insert(100, "inserted".into());
        lines[189] = "changed".into();
        let fixture = Fixture::new("refine", &committed, &(lines.join("\n") + "\n"));
        let mut app = fixture.app();
        app.viewport_height = 20;

        // Stand in for a build that ran out of time on the stretch between
        // the edits.
        let head = app.diff.old.clone();
        let current = app.new_lines().to_vec();
        app.diff = DiffModel::build_coarse(head, &current);
        assert_eq!(app.diff.coarse.len(), 1);

        // Above the stretch there is nothing to refine.
        app.scroll = 0;
        assert!(!app.wants_refine());

        // Inside it there is. The line at the top of the view stays at the
        // top, even though a row was added above it.
        app.scroll = 150;
        assert!(app.wants_refine());
        let top = app.diff.rows[150].new_line;
        assert_eq!(top, Some(150));
        app.refine();
        assert!(!app.wants_refine());
        assert_eq!(app.diff.rows[app.scroll].new_line, top);
        assert_eq!(app.scroll, 151, "the delete and the insert each took a row");
        let changed = app
            .diff
            .rows
            .iter()
            .filter(|r| r.kind != crate::diff::RowKind::Equal)
            .count();
        assert_eq!(changed, 3);
    }

    /// A deletion has no row on the New side, so a viewport that happens to
    /// scroll to one at the top must still resolve to the real line just
    /// below it, or the anchor silently does nothing and the view jumps.
    #[test]
    fn refining_under_the_viewport_survives_a_phantom_top_row() {
        let committed: String = (0..200).map(|i| format!("line {i}\n")).collect();
        let mut lines: Vec<String> = committed.lines().map(str::to_string).collect();
        lines.remove(21);
        lines.remove(20);
        lines.insert(100, "inserted".into());
        let fixture = Fixture::new("phantom", &committed, &(lines.join("\n") + "\n"));
        let mut app = fixture.app();
        app.viewport_height = 20;

        // Two deletes and one insert don't cancel out, so the coarse stretch's
        // last row is a lone Delete: a phantom on the New side.
        let head = app.diff.old.clone();
        let current = app.new_lines().to_vec();
        app.diff = DiffModel::build_coarse(head, &current);
        app.scroll = 101;
        assert_eq!(
            app.diff.rows[app.scroll],
            crate::diff::DiffRow {
                old_line: Some(101),
                new_line: None,
                kind: crate::diff::RowKind::Delete,
            }
        );
        assert!(app.wants_refine());

        // The line actually shown at the top is the next real one, line 101.
        let top = app.diff.new_line_at_or_after(app.scroll);
        assert_eq!(top, Some(101));
        app.refine();
        assert!(!app.wants_refine());
        assert_eq!(app.diff.rows[app.scroll].new_line, top);
        assert_eq!(app.scroll, 103, "two deletes and an insert each took a row");
    }

    #[test]
    fn a_binary_file_is_not_editable() {
        let fixture = Fixture::new("binary", "a\n", "a\n");
        std::fs::write(fixture.dir.join("file.txt"), [0x00, 0x01, 0x02]).unwrap();
        let mut app = fixture.app();
        assert!(app.buffer().is_none());
        assert!(!app.editing());

        // Focus alone cannot make it editable, so keys stay view commands.
        app.on_key(key(KeyCode::Char('q')));
        assert!(app.quit);
    }

    /// Two revisions have no working tree to write to, so the Current pane is
    /// read-only the way a binary file is — and says so when asked to save,
    /// which a file that simply has no buffer never had to.
    #[test]
    fn a_revision_on_the_current_side_is_read_only() {
        let fixture = Fixture::new("revision-readonly", "one\n", "one\n");
        fixture.write("two\n");
        fixture.git(&["commit", "-qam", "second"]);
        let second = fixture.git(&["rev-parse", "HEAD"]).trim().to_string();
        fixture.write("three\n");

        let mut app = fixture.app_with_revs(&[&format!("{second}^!")]);
        assert_eq!(app.new_lines(), ["two"], "the commit's own content");
        assert!(app.buffer().is_none() && !app.editing());

        app.on_key(key(KeyCode::Char('x')));
        assert_eq!(app.new_lines(), ["two"], "typing changes nothing");
        assert!(!app.is_dirty(&PathBuf::from("file.txt")));

        app.on_key(ctrl('s'));
        let notice = app.notice.clone().unwrap_or_default();
        assert!(notice.contains("read-only"), "{notice}");
        assert!(
            notice.contains(&second[..7]),
            "the pane's own label: {notice} vs {second}"
        );
        assert_eq!(fixture.read(), "three\n", "nothing was written");

        // The disk is not what is shown, so a write to it is not a change to
        // anything on screen.
        fixture.write("four\n");
        app.poll_disk();
        assert!(!app.stale);
        assert_eq!(app.new_lines(), ["two"]);
    }

    /// One revision still compares against the working tree, so everything the
    /// editor does keeps working.
    #[test]
    fn one_revision_leaves_the_working_tree_editable() {
        let fixture = Fixture::new("revision-editable", "one\n", "two\n");
        let head = fixture.git(&["rev-parse", "HEAD"]).trim().to_string();

        let mut app = fixture.app_with_revs(&[&head]);
        assert!(app.editing());
        app.on_key(key(KeyCode::Char('x')));
        assert!(app.is_dirty(&PathBuf::from("file.txt")));
        app.on_key(ctrl('s'));
        assert_eq!(fixture.read(), "xtwo\n");
    }

    /// Picking a commit compares it in the form the command line asked for,
    /// and the pane it was picked in stays where the reader left it.
    #[test]
    fn picking_a_commit_keeps_the_form_of_the_comparison() {
        let fixture = Fixture::new("pick-shape", "one\n", "two\n");
        fixture.git(&["commit", "-qam", "second"]);
        let second = fixture.git(&["rev-parse", "HEAD"]).trim().to_string();
        fixture.write("three\n");

        // No revision: picking a commit shows that commit's own changes.
        let mut app = fixture.app_with_revs(&[]);
        app.focus = Pane::Commits;
        app.commits_height = 8;
        app.commits.ensure_loaded(&app.repo, 8).unwrap();
        app.on_key(key(KeyCode::Down));
        assert_eq!(app.commits.cursor, 1, "the cursor moved");
        assert!(app.repo.worktree(), "and nothing was compared yet");

        app.on_key(key(KeyCode::Enter));
        assert!(!app.repo.worktree(), "a commit's own changes is read-only");
        let short = second[..7].to_string();
        assert_eq!(app.repo.labels(), (format!("{short}^"), short));
        assert!(app.commits.is_target(1), "the mark is on the row picked");
        assert_eq!(app.commits.cursor, 1, "and the cursor has not moved");

        // The Working tree row is the way back.
        app.on_key(key(KeyCode::Up));
        app.on_key(key(KeyCode::Enter));
        assert!(app.repo.worktree());
        assert_eq!(app.repo.labels().0, "HEAD");
        assert!(app.commits.is_target(0));

        // A revision keeps the working tree on the right, editable.
        let mut app = fixture.app_with_revs(&[&second]);
        app.focus = Pane::Commits;
        app.commits_height = 8;
        app.commits.ensure_loaded(&app.repo, 8).unwrap();
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Enter));
        assert!(app.repo.worktree(), "still against the working tree");
        assert_eq!(app.repo.labels().1, crate::commits::WORKING_TREE);
    }

    /// Browsing the history is free: the cursor moves without difv asking git
    /// anything, which a root that is not a repository at all proves — every
    /// call there would fail.
    #[test]
    fn moving_the_commit_cursor_asks_git_nothing() {
        let fixture = Fixture::new("pick-free", "a\n", "b\n");
        let mut app = fixture.app();
        app.focus = Pane::Commits;
        app.commits_height = 4;
        app.commits.ensure_loaded(&app.repo, 4).unwrap();
        let loaded = app.commits.len();

        for _ in 0..10 {
            app.on_key(key(KeyCode::Down));
        }
        assert_eq!(app.commits.len(), loaded, "no page was fetched");
        assert!(app.notice.is_none(), "and nothing failed: {:?}", app.notice);
    }

    /// Unsaved edits have nowhere to live once the Current side is a commit,
    /// so a pick that makes it one asks first — and one that does not, does
    /// not.
    #[test]
    fn a_pick_asks_before_it_costs_unsaved_edits() {
        let fixture = Fixture::new("pick-dirty", "one\n", "two\n");
        fixture.git(&["commit", "-qam", "second"]);
        fixture.write("three\n");

        let mut app = fixture.app_with_revs(&[]);
        app.commits_height = 8;
        app.commits.ensure_loaded(&app.repo, 8).unwrap();
        app.focus = Pane::New;
        app.on_key(key(KeyCode::Char('x')));
        assert!(app.is_dirty(&PathBuf::from("file.txt")));

        app.focus = Pane::Commits;
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.prompt, Some(Prompt::Pick), "it asks");
        assert!(app.repo.worktree(), "and changes nothing until answered");

        app.on_key(key(KeyCode::Char('n')));
        assert_eq!(app.prompt, None);
        assert!(app.repo.worktree(), "`n` leaves the comparison alone");
        assert!(app.is_dirty(&PathBuf::from("file.txt")), "and the edits");

        app.on_key(key(KeyCode::Enter));
        app.on_key(key(KeyCode::Char('y')));
        assert!(!app.repo.worktree(), "`y` picks");
        assert!(!app.is_dirty(&PathBuf::from("file.txt")), "and drops them");
    }

    /// A pick that leaves the working tree on the right changes only the
    /// Before side, so the edits are still there to save.
    #[test]
    fn a_working_tree_pick_keeps_unsaved_edits() {
        let fixture = Fixture::new("pick-keeps", "one\n", "two\n");
        fixture.git(&["commit", "-qam", "second"]);
        let second = fixture.git(&["rev-parse", "HEAD"]).trim().to_string();
        fixture.write("three\n");

        let mut app = fixture.app_with_revs(&[&second]);
        app.commits_height = 8;
        app.commits.ensure_loaded(&app.repo, 8).unwrap();
        app.focus = Pane::New;
        app.on_key(key(KeyCode::Char('x')));

        app.focus = Pane::Commits;
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.prompt, None, "nothing to ask about");
        assert!(app.is_dirty(&PathBuf::from("file.txt")));
        assert_eq!(app.new_lines(), ["xthree"]);
    }

    /// The Commits pane is under the pointer like any other: a click on a
    /// row picks it in one step, and the wheel scrolls without picking.
    #[test]
    fn clicking_a_commit_picks_it() {
        let fixture = Fixture::new("pick-click", "one\n", "two\n");
        fixture.git(&["commit", "-qam", "second"]);
        fixture.write("three\n");
        let mut app = fixture.app_with_revs(&[]);
        app.body = Rect::new(0, 0, 90, 20);
        app.panes = [
            Rect::new(0, 0, 26, 10),
            Rect::new(0, 10, 26, 10),
            Rect::new(26, 0, 32, 20),
            Rect::new(58, 0, 32, 20),
        ];
        app.commits_height = 8;
        app.commits.ensure_loaded(&app.repo, 8).unwrap();

        // Row 11 on screen is the first row under the Commits border: the
        // Working tree row; row 12 is the newest commit.
        let click = |app: &mut App, row: u16| {
            for kind in [
                MouseEventKind::Down(MouseButton::Left),
                MouseEventKind::Up(MouseButton::Left),
            ] {
                app.on_mouse(MouseEvent {
                    kind,
                    column: 5,
                    row,
                    modifiers: KeyModifiers::NONE,
                });
            }
        };
        click(&mut app, 12);
        assert_eq!(app.focus, Pane::Commits);
        assert_eq!(app.commits.cursor, 1);
        assert!(app.commits.is_target(1), "one click, picked");
        assert!(!app.repo.worktree(), "a commit's own changes");

        // The wheel over the pane scrolls it and picks nothing: the cursor
        // and the comparison stay where they were.
        let labels = app.repo.labels();
        app.commits.fill_for_test(50);
        app.on_mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 5,
            row: 15,
            modifiers: KeyModifiers::NONE,
        });
        assert!(app.commits.scroll > 0, "the view moved");
        assert_eq!(app.commits.cursor, 1, "the cursor did not");
        assert_eq!(app.repo.labels(), labels, "nor the comparison");
    }

    /// The border between the Changes pane and the Commits pane is a handle
    /// like the ones between the columns, and moves rows rather than columns.
    #[test]
    fn dragging_the_split_moves_rows_between_the_left_panes() {
        let fixture = Fixture::new("split-drag", "a\n", "b\n");
        let mut app = fixture.app();
        app.body = Rect::new(0, 0, 90, 20);
        app.panes = [
            Rect::new(0, 0, 26, 10),
            Rect::new(0, 10, 26, 10),
            Rect::new(26, 0, 32, 20),
            Rect::new(58, 0, 32, 20),
        ];
        let weights = app.weights;

        // Either of the two border rows grabs it.
        assert_eq!(app.divider_at(Position::new(10, 9)), Some(Divider::Split));
        assert_eq!(app.divider_at(Position::new(10, 10)), Some(Divider::Split));
        // Outside the column it is not a handle.
        assert_eq!(app.divider_at(Position::new(40, 10)), None);

        let drag = |app: &mut App, y: u16, kind: MouseEventKind| {
            app.on_mouse(MouseEvent {
                kind,
                column: 10,
                row: y,
                modifiers: KeyModifiers::NONE,
            })
        };
        drag(&mut app, 10, MouseEventKind::Down(MouseButton::Left));
        drag(&mut app, 14, MouseEventKind::Drag(MouseButton::Left));
        assert_eq!(app.split, [14, 6], "the Changes pane took four rows");
        assert_eq!(app.weights, weights, "and the columns are untouched");

        // Neither pane collapses past a border and a row.
        drag(&mut app, 19, MouseEventKind::Drag(MouseButton::Left));
        assert_eq!(app.split, [17, 3]);
        drag(&mut app, 0, MouseEventKind::Drag(MouseButton::Left));
        assert_eq!(app.split, [3, 17]);
    }

    /// The file `-C` names is looked up in the list the revisions produce, so
    /// a file a commit touched can be opened by name whether or not it still
    /// differs from `HEAD`.
    #[test]
    fn a_file_given_to_dash_c_selects_from_the_compared_list() {
        let fixture = Fixture::new("revision-select", "one\n", "one\n");
        fixture.write_file("other.txt", "other\n");
        fixture.write("two\n");
        fixture.git(&["add", "-A"]);
        fixture.git(&["commit", "-qm", "second"]);
        let second = fixture.git(&["rev-parse", "HEAD"]).trim().to_string();

        let mut app = fixture.app_with_revs(&[&format!("{second}^!")]);
        app.select_path(&fixture.dir().join("other.txt")).unwrap();
        assert_eq!(
            app.selected_file().map(|f| f.path.clone()),
            Some(PathBuf::from("other.txt")),
        );
        assert!(app.notice.is_none(), "it is in the list, so no notice");
    }
}
