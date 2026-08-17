use std::ops::Range;
use std::time::{Duration, Instant};

use similar::{Algorithm, DiffOp, DiffTag, capture_diff_slices_deadline};

/// A rebuild runs on every keystroke, so it is bounded rather than debounced:
/// hitting the bound coarsens the change markers, where a debounce would leave
/// the model describing a buffer that no longer exists. Refining a window of a
/// coarse stretch later runs under the same bound.
pub const BUDGET: Duration = Duration::from_millis(20);

/// Rows on each side of the viewport refined along with it, so a few scrolls'
/// worth is ready before it is seen. A window this size diffs in well under a
/// millisecond, so it never comes near the budget on its own.
pub const REFINE_MARGIN: usize = 200;

/// A Replace at least this long, in a build that ran out of time, is taken to
/// be a stretch Myers gave up on rather than a real rewrite. A real one is
/// re-diffed once, confirmed, and forgotten; anything shorter is cheap either
/// way, so it is never worth tracking.
const COARSE_MIN: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    Equal,
    Delete,
    Insert,
    Replace,
}

/// One visual row of the side-by-side view. A `None` on either side is a
/// phantom row: the pane renders filler there so both sides stay aligned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiffRow {
    pub old_line: Option<usize>,
    pub new_line: Option<usize>,
    pub kind: RowKind,
}

/// A stretch the last build ran out of time on. Myers is divide and conquer, so
/// a subproblem that hit the deadline is emitted as one Replace and the splits
/// on either side of it stand: inside, both sides are paired positionally;
/// outside, the alignment is right. That is what makes it safe to refine one
/// stretch on its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoarseSpan {
    pub old: Range<usize>,
    pub new: Range<usize>,
}

/// The Before side plus the row alignment. The Current side is not stored: it
/// lives in the editor buffer, which is the only copy of that text.
pub struct DiffModel {
    pub old: Vec<String>,
    pub rows: Vec<DiffRow>,
    /// Buffer line to visual row, so the cursor can be placed in O(1) and
    /// cannot be resolved against a mapping the last rebuild left behind.
    new_line_to_row: Vec<Option<usize>>,
    /// Stretches still waiting for a real diff, in row order.
    pub coarse: Vec<CoarseSpan>,
}

impl DiffModel {
    /// Unbounded. Every call site outside tests now has a deadline, so this
    /// only exists for fixtures that want a real diff without one.
    #[cfg(test)]
    pub fn build(old: Vec<String>, new: &[String]) -> Self {
        Self::build_within(old, new, None)
    }

    pub fn build_bounded(old: Vec<String>, new: &[String]) -> Self {
        Self::build_within(old, new, Some(BUDGET))
    }

    /// A build whose deadline passed a second ago, so every subproblem Myers
    /// would have solved is left coarse. Deterministic, which a real deadline
    /// on a small input is not — and clearly in the past, since `similar`
    /// compares with `>` and a deadline of "now" could still be met.
    #[cfg(test)]
    pub fn build_coarse(old: Vec<String>, new: &[String]) -> Self {
        Self::build_at(old, new, Some(past()))
    }

    fn build_within(old: Vec<String>, new: &[String], budget: Option<Duration>) -> Self {
        Self::build_at(old, new, budget.map(|budget| Instant::now() + budget))
    }

    fn build_at(old: Vec<String>, new: &[String], deadline: Option<Instant>) -> Self {
        let ops = capture_diff_slices_deadline(Algorithm::Myers, &old, new, deadline);
        // The same comparison `similar` makes, so "ran out" means it did bail.
        let ran_out = deadline.is_some_and(|deadline| Instant::now() > deadline);
        let coarse = coarse_spans(&ops, ran_out, 0, 0);
        let rows = rows_from_ops(&ops, &old, new, 0, 0);
        let mut model = Self {
            old,
            rows,
            new_line_to_row: Vec::new(),
            coarse,
        };
        model.index(new.len());
        model
    }

    pub fn empty() -> Self {
        Self {
            old: Vec::new(),
            rows: Vec::new(),
            new_line_to_row: Vec::new(),
            coarse: Vec::new(),
        }
    }

    /// Rebuild the line-to-row map after `rows` changed.
    fn index(&mut self, new_len: usize) {
        self.new_line_to_row.clear();
        self.new_line_to_row.resize(new_len, None);
        for (index, row) in self.rows.iter().enumerate() {
            if let Some(line) = row.new_line {
                self.new_line_to_row[line] = Some(index);
            }
        }
    }

    pub fn row_of_new_line(&self, line: usize) -> Option<usize> {
        self.new_line_to_row.get(line).copied().flatten()
    }

    /// The visual rows a coarse stretch occupies. Its first row pairs the first
    /// line of each side, so the map finds it. A miss here means a span outlived
    /// the rows it was built from — an invariant break rather than something a
    /// caller can hit honestly — so it is loud in debug and falls back to row 0
    /// in release rather than panicking mid-refine.
    fn span_rows(&self, span: &CoarseSpan) -> Range<usize> {
        let start = self.row_of_new_line(span.new.start);
        debug_assert!(
            start.is_some(),
            "coarse span's first new line {} has no row",
            span.new.start
        );
        let start = start.unwrap_or(0);
        start..start + span.old.len().max(span.new.len())
    }

    /// The first coarse stretch any of whose rows are among `rows`.
    pub fn coarse_at(&self, rows: Range<usize>) -> Option<usize> {
        self.coarse.iter().position(|span| {
            let mine = self.span_rows(span);
            mine.start < rows.end && rows.start < mine.end
        })
    }

    /// Give a window of one coarse stretch a real diff: the visual rows
    /// `focus`, widened by `REFINE_MARGIN` on each side and clipped to the
    /// stretch, within `deadline`. Rows outside the window keep the alignment
    /// they had. The stretch shrinks to what is left on either side of the
    /// window, plus whatever the window's own diff ran out of time on — so a
    /// stretch that cannot be finished in one go is finished across several,
    /// and one that turns out to be a real rewrite is confirmed and dropped.
    /// `focus` need not overlap the stretch's rows, or even be in range of the
    /// model at all: it is clamped to the stretch first, so a stale index or a
    /// focus from before an edit or scroll still refines the end of the
    /// stretch nearest to it rather than panicking.
    pub fn refine(&mut self, index: usize, focus: Range<usize>, new: &[String], deadline: Instant) {
        let span = self.coarse.remove(index);
        let rows = self.span_rows(&span);
        let focus = focus.start.clamp(rows.start, rows.end)..focus.end.clamp(rows.start, rows.end);
        let window = focus.start.saturating_sub(REFINE_MARGIN).max(rows.start)
            ..(focus.end + REFINE_MARGIN).min(rows.end);
        // Row i of the stretch is line `start + i` on each side, until that
        // side runs out.
        let side = |range: &Range<usize>| {
            (range.start + (window.start - rows.start)).min(range.end)
                ..(range.start + (window.end - rows.start)).min(range.end)
        };
        let (wo, wn) = (side(&span.old), side(&span.new));

        let ops = capture_diff_slices_deadline(
            Algorithm::Myers,
            &self.old[wo.clone()],
            &new[wn.clone()],
            Some(deadline),
        );
        let ran_out = Instant::now() > deadline;
        let mut fine = rows_from_ops(&ops, &self.old, new, wo.start, wn.start);

        let before = CoarseSpan {
            old: span.old.start..wo.start,
            new: span.new.start..wn.start,
        };
        let after = CoarseSpan {
            old: wo.end..span.old.end,
            new: wn.end..span.new.end,
        };
        let mut replacement = paired_rows(before.old.clone(), before.new.clone(), &self.old, new);
        replacement.append(&mut fine);
        replacement.extend(paired_rows(
            after.old.clone(),
            after.new.clone(),
            &self.old,
            new,
        ));
        self.rows.splice(rows, replacement);

        // A piece with one side empty is a plain insert or delete, which is
        // already right; only a two-sided piece is still a guess.
        let mut spans = Vec::new();
        if !before.old.is_empty() && !before.new.is_empty() {
            spans.push(before);
        }
        spans.extend(coarse_spans(&ops, ran_out, wo.start, wn.start));
        if !after.old.is_empty() && !after.new.is_empty() {
            spans.push(after);
        }
        self.coarse.splice(index..index, spans);
        self.index(new.len());
    }

    /// The buffer line a visual row stands for. A phantom row belongs to no
    /// line, so it resolves to the next one that follows, or the last line.
    pub fn new_line_at_or_after(&self, row: usize) -> Option<usize> {
        self.line_at_or_after(row, |row| row.new_line)
    }

    pub fn old_line_at_or_after(&self, row: usize) -> Option<usize> {
        self.line_at_or_after(row, |row| row.old_line)
    }

    fn line_at_or_after(&self, row: usize, side: fn(&DiffRow) -> Option<usize>) -> Option<usize> {
        self.rows
            .get(row..)?
            .iter()
            .find_map(side)
            .or_else(|| self.rows.iter().rev().find_map(side))
    }

    pub fn changed_row_after(&self, from: usize) -> Option<usize> {
        self.hunk_starts().into_iter().find(|r| *r > from)
    }

    pub fn changed_row_before(&self, from: usize) -> Option<usize> {
        self.hunk_starts().into_iter().rev().find(|r| *r < from)
    }

    fn hunk_starts(&self) -> Vec<usize> {
        let mut starts = Vec::new();
        let mut prev_changed = false;
        for (i, row) in self.rows.iter().enumerate() {
            let changed = row.kind != RowKind::Equal;
            if changed && !prev_changed {
                starts.push(i);
            }
            prev_changed = changed;
        }
        starts
    }
}

/// An instant safely in the past, for builds and refinements that must bail.
#[cfg(test)]
pub fn past() -> Instant {
    Instant::now()
        .checked_sub(Duration::from_secs(1))
        .unwrap_or_else(Instant::now)
}

/// Op ranges are relative to the slices that were diffed; the offsets put them
/// back on whole-file line numbers, so a diff of one window lands in place.
fn rows_from_ops(
    ops: &[DiffOp],
    old: &[String],
    new: &[String],
    old_offset: usize,
    new_offset: usize,
) -> Vec<DiffRow> {
    let mut rows = Vec::new();
    for op in ops {
        let o = op.old_range().start + old_offset..op.old_range().end + old_offset;
        let n = op.new_range().start + new_offset..op.new_range().end + new_offset;
        match op.tag() {
            DiffTag::Equal => rows.extend(o.zip(n).map(|(o, n)| DiffRow {
                old_line: Some(o),
                new_line: Some(n),
                kind: RowKind::Equal,
            })),
            DiffTag::Delete => rows.extend(o.map(|o| DiffRow {
                old_line: Some(o),
                new_line: None,
                kind: RowKind::Delete,
            })),
            DiffTag::Insert => rows.extend(n.map(|n| DiffRow {
                old_line: None,
                new_line: Some(n),
                kind: RowKind::Insert,
            })),
            DiffTag::Replace => rows.extend(paired_rows(o, n, old, new)),
        }
    }
    rows
}

/// Replaced lines paired up so edits sit opposite each other, then whichever
/// side is longer spills into phantom-backed rows. A pair that happens to be
/// identical is drawn as equal: this is also how a coarse stretch is drawn, and
/// most of one is unchanged lines Myers had no time to confirm.
fn paired_rows(o: Range<usize>, n: Range<usize>, old: &[String], new: &[String]) -> Vec<DiffRow> {
    let mut rows = Vec::with_capacity(o.len().max(n.len()));
    let (mut o, mut n) = (o, n);
    loop {
        match (o.next(), n.next()) {
            (Some(o), Some(n)) => rows.push(DiffRow {
                old_line: Some(o),
                new_line: Some(n),
                kind: if old[o] == new[n] {
                    RowKind::Equal
                } else {
                    RowKind::Replace
                },
            }),
            (Some(o), None) => rows.push(DiffRow {
                old_line: Some(o),
                new_line: None,
                kind: RowKind::Delete,
            }),
            (None, Some(n)) => rows.push(DiffRow {
                old_line: None,
                new_line: Some(n),
                kind: RowKind::Insert,
            }),
            (None, None) => break,
        }
    }
    rows
}

/// The stretches a build gave up on. Only a build that actually ran out of time
/// has any: a big Replace from a build that finished is a real rewrite.
fn coarse_spans(
    ops: &[DiffOp],
    ran_out: bool,
    old_offset: usize,
    new_offset: usize,
) -> Vec<CoarseSpan> {
    if !ran_out {
        return Vec::new();
    }
    ops.iter()
        .filter(|op| op.tag() == DiffTag::Replace)
        .filter(|op| op.old_range().len() + op.new_range().len() >= COARSE_MIN)
        .map(|op| CoarseSpan {
            old: op.old_range().start + old_offset..op.old_range().end + old_offset,
            new: op.new_range().start + new_offset..op.new_range().end + new_offset,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape(m: &DiffModel) -> Vec<(Option<usize>, Option<usize>, RowKind)> {
        m.rows
            .iter()
            .map(|r| (r.old_line, r.new_line, r.kind))
            .collect()
    }

    fn lines(text: &str) -> Vec<String> {
        text.lines().map(str::to_string).collect()
    }

    fn build(old: &str, new: &str) -> DiffModel {
        DiffModel::build(lines(old), &lines(new))
    }

    fn numbered(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("line {i}")).collect()
    }

    /// 200 numbered lines with a deletion, an insertion and a change spread
    /// out — the shape a positional pairing gets wrong for the whole stretch
    /// between the first two, and right again after the insertion.
    fn edited() -> (Vec<String>, Vec<String>) {
        let old = numbered(200);
        let mut new = old.clone();
        new.remove(20);
        new.insert(100, "inserted".into());
        new[189] = "changed".into();
        (old, new)
    }

    #[test]
    fn a_build_that_ran_out_of_time_records_the_stretch_it_gave_up_on() {
        let (old, new) = edited();
        let m = DiffModel::build_coarse(old, &new);

        // The equal prefix and suffix are still found; the middle is one
        // positional stretch.
        assert_eq!(
            m.coarse,
            vec![CoarseSpan {
                old: 20..190,
                new: 20..190
            }]
        );
        assert_eq!(m.rows.len(), 200);
        assert_eq!(m.rows[9].kind, RowKind::Equal);
        // Drifted by the deletion, so wrong...
        assert_eq!(m.rows[50].kind, RowKind::Replace);
        // ...until the insertion cancels the drift, where equal pairs are
        // shown as equal even though Myers never confirmed them.
        assert_eq!(m.rows[150].kind, RowKind::Equal);
        for line in 0..new.len() {
            assert!(m.row_of_new_line(line).is_some(), "line {line} has no row");
        }

        // With time, the same input is fine and records nothing.
        let (old, new) = edited();
        let m = DiffModel::build(old, &new);
        assert!(m.coarse.is_empty());
        let changed = m.rows.iter().filter(|r| r.kind != RowKind::Equal).count();
        assert_eq!(changed, 3);
    }

    #[test]
    fn insert_creates_phantom_on_the_old_side() {
        let m = build("foo\nbar\nbaz\n", "foo\nbar\nhello\nbaz\n");
        assert_eq!(
            shape(&m),
            vec![
                (Some(0), Some(0), RowKind::Equal),
                (Some(1), Some(1), RowKind::Equal),
                (None, Some(2), RowKind::Insert),
                (Some(2), Some(3), RowKind::Equal),
            ]
        );
    }

    #[test]
    fn replace_pairs_lines_then_spills() {
        let m = build("a\n", "x\ny\n");
        assert_eq!(
            shape(&m),
            vec![
                (Some(0), Some(0), RowKind::Replace),
                (None, Some(1), RowKind::Insert),
            ]
        );
    }

    #[test]
    fn both_sides_stay_aligned_row_for_row() {
        let new = lines("a\nX\nd\ne\n");
        let m = DiffModel::build(lines("a\nb\nc\nd\n"), &new);
        // Every row must be renderable on both sides without drifting.
        for row in &m.rows {
            assert!(row.old_line.is_some() || row.new_line.is_some());
            assert!(row.old_line.is_none_or(|i| i < m.old.len()));
            assert!(row.new_line.is_none_or(|i| i < new.len()));
        }
    }

    #[test]
    fn hunk_navigation_walks_change_blocks() {
        let m = build("a\nb\nc\nd\ne\n", "a\nX\nc\nd\nY\n");
        assert_eq!(m.changed_row_after(0), Some(1));
        assert_eq!(m.changed_row_after(1), Some(4));
        assert_eq!(m.changed_row_after(4), None);
        assert_eq!(m.changed_row_before(4), Some(1));
    }

    #[test]
    fn added_file_is_all_inserts() {
        let m = build("", "one\ntwo\n");
        assert_eq!(m.rows.len(), 2);
        assert!(m.rows.iter().all(|r| r.kind == RowKind::Insert));
    }

    #[test]
    fn mapping_round_trips_a_buffer_line() {
        let m = build("foo\nbar\nbaz\n", "foo\nbar\nhello\nbaz\n");
        for line in 0..4 {
            let row = m.row_of_new_line(line).unwrap();
            assert_eq!(m.new_line_at_or_after(row), Some(line));
        }
    }

    #[test]
    fn a_phantom_row_resolves_forward_and_then_to_the_last_line() {
        // The deleted `b` leaves a phantom row on the new side at row 1.
        let m = build("a\nb\nc\n", "a\nc\n");
        assert_eq!(m.rows[1].new_line, None);
        assert_eq!(m.new_line_at_or_after(1), Some(1));

        // Trailing deletions leave no following line at all.
        let m = build("a\nb\nc\n", "a\n");
        assert_eq!(m.new_line_at_or_after(m.rows.len() - 1), Some(0));
    }

    #[test]
    fn mapping_follows_rows_inserted_above() {
        let before = build("a\nb\n", "a\nb\n");
        assert_eq!(before.row_of_new_line(1), Some(1));
        let after = build("a\nb\n", "a\nX\nY\nb\n");
        assert_eq!(after.row_of_new_line(3), Some(3));
        assert_eq!(after.new_line_at_or_after(3), Some(3));
    }

    fn far() -> Instant {
        Instant::now() + Duration::from_secs(60)
    }

    #[test]
    fn refining_a_stretch_gives_it_the_real_diff_and_leaves_the_rest_alone() {
        let (old, new) = edited();
        let mut m = DiffModel::build_coarse(old, &new);
        let before = m.rows.clone();
        assert_eq!(m.coarse_at(100..120), Some(0));
        assert_eq!(m.coarse_at(0..20), None);
        assert_eq!(m.coarse_at(190..200), None);

        m.refine(0, 100..120, &new, far());

        // The window covered the whole stretch, so nothing is left coarse.
        assert!(m.coarse.is_empty());
        assert_eq!(m.coarse_at(0..200), None);
        // Delete, insert, replace: the same three changes the fine build finds.
        let changed = m.rows.iter().filter(|r| r.kind != RowKind::Equal).count();
        assert_eq!(changed, 3);
        // One row more than before, since the delete and the insert no longer
        // share a row with anything.
        assert_eq!(m.rows.len(), 201);
        // Rows outside the stretch are exactly what they were, shifted by that
        // one row after it.
        assert_eq!(&m.rows[..20], &before[..20]);
        assert_eq!(&m.rows[191..], &before[190..]);
        // And the map still round-trips every line.
        for line in 0..new.len() {
            let row = m.row_of_new_line(line).unwrap();
            assert_eq!(m.rows[row].new_line, Some(line));
        }
    }

    #[test]
    fn refining_only_touches_the_window_around_the_focus() {
        // 800 changed lines in a row, far more than one window covers.
        let old = numbered(1000);
        let new: Vec<String> = old
            .iter()
            .enumerate()
            .map(|(i, l)| {
                if (100..900).contains(&i) {
                    format!("{l} x")
                } else {
                    l.clone()
                }
            })
            .collect();
        let mut m = DiffModel::build_coarse(old, &new);
        assert_eq!(
            m.coarse,
            vec![CoarseSpan {
                old: 100..900,
                new: 100..900
            }]
        );

        m.refine(0, 480..500, &new, far());

        // Rows 280..700 were re-diffed; what is left on either side is still
        // coarse, and the focus is no longer in a coarse stretch.
        assert_eq!(
            m.coarse,
            vec![
                CoarseSpan {
                    old: 100..280,
                    new: 100..280
                },
                CoarseSpan {
                    old: 700..900,
                    new: 700..900
                },
            ]
        );
        assert_eq!(m.rows.len(), 1000);
        assert_eq!(m.coarse_at(480..500), None);
        assert_eq!(m.coarse_at(150..170), Some(0));
        assert_eq!(m.coarse_at(890..950), Some(1));

        // A second refinement from the top of the first piece finishes it.
        m.refine(0, 100..120, &new, far());
        assert_eq!(
            m.coarse,
            vec![CoarseSpan {
                old: 700..900,
                new: 700..900
            }]
        );
    }

    #[test]
    fn a_window_that_runs_out_of_time_stays_coarse_and_loses_nothing() {
        let (old, new) = edited();
        let mut m = DiffModel::build_coarse(old, &new);
        let before = m.rows.clone();
        let spans = m.coarse.clone();

        // A deadline that has already passed: the window's own diff bails and
        // comes back as the same coarse stretch.
        m.refine(0, 100..120, &new, past());

        assert_eq!(m.coarse, spans);
        assert_eq!(m.rows, before);
        for line in 0..new.len() {
            let row = m.row_of_new_line(line).unwrap();
            assert_eq!(m.rows[row].new_line, Some(line));
        }
    }

    #[test]
    fn refining_from_a_focus_outside_the_stretch_does_not_panic() {
        let (old, new) = edited();
        let mut m = DiffModel::build_coarse(old, &new);

        // A stale focus, far past the stretch (rows 20..190) and past the end
        // of the file — the kind of mismatch a scroll or an edit between
        // `coarse_at` and `refine` could produce. Clamped to the stretch, this
        // is wide enough to cover it whole.
        m.refine(0, 100_000..100_010, &new, far());

        assert!(m.coarse.is_empty());
        // Every line still resolves to exactly one row.
        for line in 0..new.len() {
            let row = m.row_of_new_line(line).unwrap();
            assert_eq!(m.rows[row].new_line, Some(line));
        }
    }

    #[test]
    fn a_bounded_rebuild_still_covers_every_line() {
        // A file that differs almost everywhere is where the deadline bites.
        let old: Vec<String> = (0..4000).map(|i| format!("old line {i}")).collect();
        let new: Vec<String> = (0..4000).map(|i| format!("new line {i}")).collect();
        let m = DiffModel::build_bounded(old.clone(), &new);

        let mut seen_old: Vec<usize> = m.rows.iter().filter_map(|r| r.old_line).collect();
        let mut seen_new: Vec<usize> = m.rows.iter().filter_map(|r| r.new_line).collect();
        seen_old.dedup();
        seen_new.dedup();
        assert_eq!(seen_old, (0..old.len()).collect::<Vec<_>>());
        assert_eq!(seen_new, (0..new.len()).collect::<Vec<_>>());
        assert_eq!(m.new_line_to_row.len(), new.len());
        assert!(m.new_line_to_row.iter().all(Option::is_some));
    }
}
