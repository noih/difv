//! The Commits pane: the shape the command line asked for, and the line of
//! history it names, one page at a time.

use anyhow::Result;
use std::ffi::{OsStr, OsString};

use crate::git::Repo;

/// One row of the pane: the id a pick hands to git, the id a reader sees, and
/// what the commit said it did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    pub hash: String,
    pub short: String,
    pub subject: String,
}

/// The Current pane's label when it is the working tree rather than a
/// revision.
pub const WORKING_TREE: &str = "Working Tree";

/// Which end of a two-revision comparison the user wrote, so a pick can put
/// the commit back in the same form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sep {
    /// `a..b`
    TwoDots,
    /// `a...b`
    ThreeDots,
    /// `a b`
    Space,
}

/// What the command line asked for, as one of the three forms `git diff`
/// offers. difv still never resolves a revision itself — this is a reading of
/// the text, and the text is what goes back to git — but it is the reading
/// that lets the pane title and a pick agree about what is being compared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Shape {
    /// `difv` and `difv <rev>`: one revision against the working tree, and
    /// `None` for the `HEAD` difv compares against when nothing was typed.
    /// Every syntax difv does not recognise lands here too, since a revision
    /// it cannot read is still one git can.
    Worktree { base: Option<String> },
    /// `difv <rev>^!`: what one commit changed.
    Own { rev: String },
    /// `difv a..b`, `difv a...b`, `difv a b`: two revisions.
    Range { base: String, tip: String, sep: Sep },
}

impl Shape {
    /// The revisions as typed, read as one of the three forms. The order
    /// matters: `...` is checked before `..`, which is a prefix of it.
    pub fn of(revs: &[OsString]) -> Self {
        let typed: Vec<String> = revs
            .iter()
            .map(|r| r.to_string_lossy().into_owned())
            .collect();
        match typed.as_slice() {
            [] => Self::Worktree { base: None },
            // Two is the most the command line lets through.
            [a, b, ..] => Self::Range {
                base: a.clone(),
                tip: b.clone(),
                sep: Sep::Space,
            },
            [one] => Self::one(one),
        }
    }

    fn one(rev: &str) -> Self {
        // An omitted end of a range is `HEAD`, as it is to git.
        let end = |text: &str| match text.is_empty() {
            true => "HEAD".to_string(),
            false => text.to_string(),
        };
        if let Some((a, b)) = rev.split_once("...") {
            return Self::Range {
                base: end(a),
                tip: end(b),
                sep: Sep::ThreeDots,
            };
        }
        if let Some((a, b)) = rev.split_once("..") {
            return Self::Range {
                base: end(a),
                tip: end(b),
                sep: Sep::TwoDots,
            };
        }
        if let Some(base) = rev.strip_suffix("^!") {
            return Self::Own {
                rev: base.to_string(),
            };
        }
        Self::Worktree {
            base: Some(rev.to_string()),
        }
    }

    /// The head of the line of history the pane lists. Chosen once, at
    /// launch: a pick walks along the line rather than re-rooting it.
    pub fn tip(&self) -> String {
        match self {
            Self::Worktree { base } => base.clone().unwrap_or_else(|| "HEAD".to_string()),
            Self::Own { rev } => rev.clone(),
            Self::Range { tip, .. } => tip.clone(),
        }
    }

    /// The revisions to compare after picking a commit, in the form the user
    /// asked for. A launch with no revision at all is the one shape that
    /// changes: a commit picked out of a log means that commit's own changes
    /// everywhere else, and it means that here.
    pub fn retarget(&self, commit: &str) -> Vec<OsString> {
        match self {
            Self::Worktree { base: None } | Self::Own { .. } => {
                vec![OsString::from(format!("{commit}^!"))]
            }
            Self::Worktree { base: Some(_) } => vec![OsString::from(commit)],
            Self::Range { base, sep, .. } => match sep {
                Sep::TwoDots => vec![OsString::from(format!("{base}..{commit}"))],
                Sep::ThreeDots => vec![OsString::from(format!("{base}...{commit}"))],
                Sep::Space => vec![OsString::from(base), OsString::from(commit)],
            },
        }
    }

    /// The revisions for the Working tree row: difv's own default, except
    /// under a range, where the base is what the working tree is worth
    /// comparing against.
    pub fn working_tree(&self) -> Vec<OsString> {
        match self {
            Self::Range { base, .. } => vec![OsString::from(base)],
            _ => Vec::new(),
        }
    }

    /// Whether picking this row would leave the working tree on the Current
    /// side, and so an editor where the unsaved edits are. A reading of the
    /// shape rather than git's answer, because it is needed before anything
    /// is thrown away; it errs only for a root commit's `^!`, where it asks a
    /// question it did not have to and the edits survive the answer anyway.
    pub fn pick_keeps_worktree(&self, commit: Option<&str>) -> bool {
        match (self, commit) {
            // The Working tree row always has one.
            (_, None) => true,
            (Self::Worktree { base: Some(_) }, _) => true,
            _ => false,
        }
    }

    /// What the two panes are looking at, for their titles. `worktree` is
    /// git's own answer to which side the working tree is on, which a reading
    /// of the syntax cannot give: a root commit's `^!` names one commit, and
    /// git compares one commit against the working tree.
    pub fn labels(&self, worktree: bool) -> (String, String) {
        let (old, new) = match self {
            Self::Worktree { base } => (
                base.clone()
                    .map(short)
                    .unwrap_or_else(|| "HEAD".to_string()),
                WORKING_TREE.to_string(),
            ),
            Self::Own { rev } => (format!("{}^", short(rev.clone())), short(rev.clone())),
            Self::Range { base, tip, .. } => (short(base.clone()), short(tip.clone())),
        };
        match worktree {
            true => (old, WORKING_TREE.to_string()),
            false => (old, new),
        }
    }
}

/// Rows at least this many, however short the pane: a page is a process
/// spawn, and a handful of rows per spawn would make holding the arrow key
/// cost one for every screen.
const MIN_PAGE: usize = 64;

/// The line of history the pane lists, as far down it as anyone has looked.
///
/// The history is not counted and not read whole: pages arrive from `git log`
/// as the cursor approaches the end of what is loaded, and the pane draws the
/// rows on screen rather than the rows it holds. A repository with fifty
/// thousand commits therefore costs the same as one with fifty until someone
/// scrolls, and the footer says `12/200+` rather than pretending to know.
pub struct Commits {
    tip: OsString,
    /// Rows per page, never fewer than this however short the pane.
    page_floor: usize,
    rows: Vec<Commit>,
    /// A page came back short, so there is nothing further down.
    exhausted: bool,
    /// Where the reader is, counting the Working tree row as 0.
    pub cursor: usize,
    /// The first row on screen, in the same counting.
    pub scroll: usize,
    /// The commit being compared, by hash — an index would drift as commits
    /// arrive above it. `None` is the Working tree row.
    target: Option<String>,
}

impl Commits {
    pub fn new(tip: OsString) -> Self {
        Self::with_page_floor(tip, MIN_PAGE)
    }

    /// The same, with the page floor said out loud — which is how the paging
    /// itself is tested without a repository of a hundred commits.
    pub fn with_page_floor(tip: OsString, page_floor: usize) -> Self {
        Self {
            tip,
            page_floor,
            rows: Vec::new(),
            exhausted: false,
            cursor: 0,
            scroll: 0,
            target: None,
        }
    }

    pub fn tip(&self) -> &OsStr {
        &self.tip
    }

    #[cfg(test)]
    pub fn rows(&self) -> &[Commit] {
        &self.rows
    }

    /// A history without a repository behind it, for the tests that are about
    /// what is drawn rather than what git said.
    #[cfg(test)]
    pub fn fill_for_test(&mut self, count: usize) {
        self.rows = (0..count)
            .map(|n| Commit {
                hash: format!("{n:040}"),
                short: format!("{n:07}"),
                subject: format!("commit {n}"),
            })
            .collect();
        self.exhausted = true;
    }

    /// Rows including the Working tree row above them.
    pub fn len(&self) -> usize {
        self.rows.len() + 1
    }

    /// The commit a row stands for, or `None` for the Working tree row.
    pub fn at(&self, row: usize) -> Option<&Commit> {
        self.rows.get(row.checked_sub(1)?)
    }

    /// Whether a row is the one being compared.
    pub fn is_target(&self, row: usize) -> bool {
        match (self.at(row), &self.target) {
            (None, None) => true,
            (Some(commit), Some(hash)) => commit.hash == *hash,
            _ => false,
        }
    }

    pub fn set_target(&mut self, hash: Option<String>) {
        self.target = hash;
    }

    /// `12/200+` while more may be waiting, `12/200` once the end is known.
    pub fn position(&self) -> String {
        let more = match self.exhausted {
            true => "",
            false => "+",
        };
        format!("{}/{}{more}", self.cursor + 1, self.len())
    }

    /// Fetch another page when the cursor or the view has come within a
    /// screen of the end of what is loaded. Called after every move and every
    /// scroll; it is a `git log` only when it actually needs one.
    pub fn ensure_loaded(&mut self, repo: &Repo, viewport: usize) -> Result<()> {
        let page = (viewport * 2).max(self.page_floor);
        while !self.exhausted && self.cursor.max(self.scroll) + viewport + 1 >= self.len() {
            let next = repo.log_page(&self.tip, self.rows.len(), page)?;
            self.exhausted = next.len() < page;
            if next.is_empty() {
                break;
            }
            self.rows.extend(next);
        }
        Ok(())
    }

    /// Start again from the first page, keeping the reader where they were:
    /// the cursor and the target are found by hash, so a commit made above
    /// them does not shift them. A cursor whose commit is gone goes to the
    /// Working tree row; a target whose commit is gone keeps the comparison
    /// it made, which is still on screen.
    pub fn reload(&mut self, repo: &Repo, viewport: usize) -> Result<()> {
        let was = self.at(self.cursor).map(|c| c.hash.clone());
        self.rows.clear();
        self.exhausted = false;
        self.cursor = 0;
        self.scroll = 0;
        self.ensure_loaded(repo, viewport)?;
        if let Some(hash) = was {
            // Only as far as has been loaded: a commit further down than the
            // reader had scrolled is not worth pages of `git log` to find.
            self.cursor = self
                .rows
                .iter()
                .position(|c| c.hash == hash)
                .map(|row| row + 1)
                .unwrap_or(0);
        }
        Ok(())
    }

    /// Move the cursor by rows, clamped to what exists, and load what the
    /// move brought into reach.
    pub fn move_cursor(&mut self, by: isize, repo: &Repo, viewport: usize) -> Result<()> {
        // Ask for what the move will need before clamping to what is loaded,
        // or a move into rows that have not arrived stops short of them.
        let wanted = self.cursor.saturating_add_signed(by);
        self.cursor = wanted;
        self.ensure_loaded(repo, viewport)?;
        self.cursor = wanted.min(self.len().saturating_sub(1));
        self.keep_cursor_visible(viewport);
        Ok(())
    }

    pub fn go_to(&mut self, row: usize, viewport: usize) {
        self.cursor = row.min(self.len().saturating_sub(1));
        self.keep_cursor_visible(viewport);
    }

    /// The view follows the cursor by the smallest move that shows it, so
    /// paging up and down does not jump the list around.
    pub fn keep_cursor_visible(&mut self, viewport: usize) {
        let viewport = viewport.max(1);
        self.scroll = self
            .scroll
            .min(self.cursor)
            .max((self.cursor + 1).saturating_sub(viewport))
            .min(self.len().saturating_sub(1));
    }

    /// Scroll without moving the cursor, which is what the wheel does.
    pub fn scroll_by(&mut self, by: isize, repo: &Repo, viewport: usize) -> Result<()> {
        let last = self.len().saturating_sub(1);
        self.scroll = self.scroll.saturating_add_signed(by).min(last);
        self.ensure_loaded(repo, viewport)
    }
}

/// A title says the name the user thinks in — except when that name is a
/// whole object id, which is what picking a commit hands to git and which
/// would eat the pane. Abbreviated only when it is unmistakably an id, so a
/// branch or tag is never touched.
fn short(rev: String) -> String {
    let is_id = rev.len() == 40 && rev.chars().all(|c| c.is_ascii_hexdigit());
    match is_id {
        true => rev[..7].to_string(),
        false => rev,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape(revs: &[&str]) -> Shape {
        Shape::of(&revs.iter().map(OsString::from).collect::<Vec<_>>())
    }

    fn retarget(revs: &[&str], commit: &str) -> Vec<String> {
        shape(revs)
            .retarget(commit)
            .iter()
            .map(|r| r.to_string_lossy().into_owned())
            .collect()
    }

    /// Every form the command line lets through, read back as the shape it is.
    #[test]
    fn the_typed_revisions_are_read_as_one_of_three_shapes() {
        assert_eq!(shape(&[]), Shape::Worktree { base: None });
        assert_eq!(
            shape(&["main"]),
            Shape::Worktree {
                base: Some("main".to_string())
            }
        );
        assert_eq!(
            shape(&["abc^!"]),
            Shape::Own {
                rev: "abc".to_string()
            }
        );
        assert_eq!(
            shape(&["a..b"]),
            Shape::Range {
                base: "a".to_string(),
                tip: "b".to_string(),
                sep: Sep::TwoDots
            }
        );
        assert_eq!(
            shape(&["a...b"]),
            Shape::Range {
                base: "a".to_string(),
                tip: "b".to_string(),
                sep: Sep::ThreeDots
            }
        );
        assert_eq!(
            shape(&["a", "b"]),
            Shape::Range {
                base: "a".to_string(),
                tip: "b".to_string(),
                sep: Sep::Space
            }
        );
        // An omitted end is `HEAD`, as it is to git.
        assert_eq!(
            shape(&["..b"]),
            Shape::Range {
                base: "HEAD".to_string(),
                tip: "b".to_string(),
                sep: Sep::TwoDots
            }
        );
        // A syntax difv does not read is still one git can, so it is a
        // revision against the working tree like any other.
        assert_eq!(
            shape(&["@{u}"]),
            Shape::Worktree {
                base: Some("@{u}".to_string())
            }
        );
    }

    /// A pick puts the commit back in the form the user asked for — and a
    /// launch with no revision at all becomes "that commit's own changes",
    /// which is what a commit picked out of a log means.
    #[test]
    fn a_pick_keeps_the_form_of_the_comparison() {
        assert_eq!(retarget(&[], "c1"), ["c1^!"]);
        assert_eq!(retarget(&["main"], "c1"), ["c1"]);
        assert_eq!(retarget(&["abc^!"], "c1"), ["c1^!"]);
        assert_eq!(retarget(&["a..b"], "c1"), ["a..c1"]);
        assert_eq!(retarget(&["a...b"], "c1"), ["a...c1"]);
        assert_eq!(retarget(&["a", "b"], "c1"), ["a", "c1"]);
        assert_eq!(retarget(&["@{u}"], "c1"), ["c1"]);

        // And the shape a pick produces is stable: picking again means the
        // same thing.
        assert_eq!(retarget(&["c1^!"], "c2"), ["c2^!"]);
        assert_eq!(retarget(&["a..c1"], "c2"), ["a..c2"]);
    }

    /// The Working tree row is the way back to what difv compares by default —
    /// except under a range, where the base is what it is worth comparing the
    /// working tree against.
    #[test]
    fn the_working_tree_row_returns_to_the_default() {
        let empty: [&str; 0] = [];
        let of = |revs: &[&str]| {
            shape(revs)
                .working_tree()
                .iter()
                .map(|r| r.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(of(&[]), empty);
        assert_eq!(of(&["main"]), empty);
        assert_eq!(of(&["abc^!"]), empty);
        assert_eq!(of(&["a..b"]), ["a"]);
        assert_eq!(of(&["a", "b"]), ["a"]);
    }

    /// The history the pane lists is the one the command line pointed at.
    #[test]
    fn the_list_starts_where_the_command_line_pointed() {
        assert_eq!(shape(&[]).tip(), "HEAD");
        assert_eq!(shape(&["main"]).tip(), "main");
        assert_eq!(shape(&["abc^!"]).tip(), "abc");
        assert_eq!(shape(&["a..b"]).tip(), "b");
        assert_eq!(shape(&["a", "b"]).tip(), "b");
    }

    /// A history arrives a page at a time, and the end is known only when a
    /// page comes back short.
    #[test]
    fn history_arrives_a_page_at_a_time() {
        let fixture = crate::app::tests::Fixture::new("commits-page", "one\n", "one\n");
        for n in 2..=5 {
            fixture.write(&format!("{n}\n"));
            fixture.git(&["commit", "-qam", &format!("commit {n}")]);
        }
        let repo = crate::git::Repo::discover(fixture.dir()).unwrap();
        // A viewport of one makes the page two rows, which is what MIN_PAGE
        // would otherwise hide.
        let mut commits = Commits::with_page_floor(OsString::from("HEAD"), 1);
        assert_eq!(commits.len(), 1, "the Working tree row is always there");

        commits.ensure_loaded(&repo, 1).unwrap();
        assert_eq!(commits.rows().len(), 2, "one page, not the history");
        assert!(commits.position().ends_with('+'), "more may be waiting");

        // Walking to the bottom loads the rest and settles on the total.
        for _ in 0..10 {
            commits.move_cursor(1, &repo, 1).unwrap();
        }
        assert_eq!(commits.rows().len(), 5, "one root plus four");
        assert_eq!(commits.position(), "6/6");
        assert_eq!(
            commits.at(1).map(|c| c.subject.as_str()),
            Some("commit 5"),
            "newest first"
        );
    }

    /// A subject is whatever the commit said; only NUL separates the fields.
    #[test]
    fn a_subject_survives_being_anything() {
        let odd = "修 `a..b`  and \"quoted\", 100% — ok";
        let fixture = crate::app::tests::Fixture::new("commits-subject", "one\n", "two\n");
        fixture.git(&["commit", "-qam", odd]);
        let repo = crate::git::Repo::discover(fixture.dir()).unwrap();

        let mut commits = Commits::new(OsString::from("HEAD"));
        commits.ensure_loaded(&repo, 4).unwrap();
        assert_eq!(commits.at(1).unwrap().subject, odd);
        assert_eq!(commits.at(1).unwrap().short.len(), 7, "the short hash");
    }

    /// A commit made while difv is open must not move the reader: the cursor
    /// is found again by hash, not by where it was in the list.
    #[test]
    fn a_reload_finds_the_cursor_by_hash() {
        let fixture = crate::app::tests::Fixture::new("commits-reload", "one\n", "two\n");
        fixture.git(&["commit", "-qam", "second"]);
        let repo = crate::git::Repo::discover(fixture.dir()).unwrap();

        let mut commits = Commits::new(OsString::from("HEAD"));
        commits.ensure_loaded(&repo, 4).unwrap();
        commits.move_cursor(2, &repo, 4).unwrap();
        let under = commits.at(commits.cursor).unwrap().hash.clone();
        assert_eq!(commits.cursor, 2);

        fixture.write("three\n");
        fixture.git(&["commit", "-qam", "third"]);
        commits.reload(&repo, 4).unwrap();
        assert_eq!(commits.cursor, 3, "one row further down, the same commit");
        assert_eq!(commits.at(commits.cursor).unwrap().hash, under);

        // A cursor whose commit is gone falls back to the Working tree row.
        let mut orphan = Commits::new(OsString::from("HEAD"));
        orphan.ensure_loaded(&repo, 4).unwrap();
        orphan.move_cursor(1, &repo, 4).unwrap();
        orphan.rows[0].hash = "0".repeat(40);
        orphan.reload(&repo, 4).unwrap();
        assert_eq!(orphan.cursor, 0);
    }

    /// The mark says which row is being compared, and it is a hash rather
    /// than a row so that a commit arriving above it does not move it.
    #[test]
    fn the_target_is_kept_by_hash() {
        let fixture = crate::app::tests::Fixture::new("commits-target", "one\n", "two\n");
        fixture.git(&["commit", "-qam", "second"]);
        let repo = crate::git::Repo::discover(fixture.dir()).unwrap();

        let mut commits = Commits::new(OsString::from("HEAD"));
        commits.ensure_loaded(&repo, 4).unwrap();
        assert!(
            commits.is_target(0),
            "the working tree, until something else"
        );

        let hash = commits.at(1).unwrap().hash.clone();
        commits.set_target(Some(hash));
        assert!(commits.is_target(1) && !commits.is_target(0));

        fixture.write("three\n");
        fixture.git(&["commit", "-qam", "third"]);
        commits.reload(&repo, 4).unwrap();
        assert!(commits.is_target(2), "same commit, one row down");
    }

    /// The view follows the cursor by the least it can, so a page up and a
    /// page down land back where they started.
    #[test]
    fn the_view_follows_the_cursor() {
        let mut commits = Commits::new(OsString::from("HEAD"));
        commits.rows = (0..20)
            .map(|n| Commit {
                hash: format!("{n:040}"),
                short: format!("{n:07}"),
                subject: format!("commit {n}"),
            })
            .collect();

        commits.go_to(15, 5);
        assert_eq!(commits.scroll, 11, "the cursor is the last visible row");
        commits.go_to(3, 5);
        assert_eq!(commits.scroll, 3, "and the first, coming back up");
        commits.go_to(5, 5);
        assert_eq!(commits.scroll, 3, "a cursor already visible moves nothing");
    }

    /// The pane titles say what was typed, with git's answer about the
    /// working tree deciding the Current side.
    #[test]
    fn the_labels_name_both_sides() {
        let cases = [
            (vec![], true, ("HEAD", WORKING_TREE)),
            (vec!["main"], true, ("main", WORKING_TREE)),
            (vec!["a..b"], false, ("a", "b")),
            (vec!["a...b"], false, ("a", "b")),
            (vec!["abc^!"], false, ("abc^", "abc")),
            (vec!["a", "b"], false, ("a", "b")),
            // One argument can still name one commit — a root commit's `^!`
            // does — and then the Current side is the working tree whatever
            // the syntax suggested.
            (vec!["abc^!"], true, ("abc^", WORKING_TREE)),
            // A whole object id is what a pick hands to git; a title says as
            // much of it as a reader needs.
            (
                vec!["0123456789abcdef0123456789abcdef01234567^!"],
                false,
                ("0123456^", "0123456"),
            ),
        ];
        for (revs, worktree, want) in cases {
            let (old, new) = shape(&revs).labels(worktree);
            assert_eq!((old.as_str(), new.as_str()), want, "{revs:?} {worktree}");
        }
    }
}
