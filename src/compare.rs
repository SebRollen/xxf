//! Document comparison: block alignment plus per-block detail.

use std::ops::Range;

use crate::align::{Step, Unit, align};
use crate::diff::{self, Hunk};
use crate::model::{Block, Para, Table, TokenKind};

#[derive(Debug)]
pub struct Item<D> {
    pub step: Step,
    /// Present for `Changed`, and for `MovedIn` when the moved unit was also modified.
    pub detail: Option<D>,
}

pub type BlockItem = Item<Detail>;

#[derive(Debug)]
pub enum Detail {
    Para(ParaDiff),
    Table(TableDiff),
    /// An opaque block (e.g. section properties) changed.
    Opaque,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpanKind {
    Same,
    /// Same text, different run formatting.
    Reformat,
    /// Old tokens replaced by new tokens; either side may be empty.
    Replace,
}

/// A span of tokens: `old`/`new` index into each paragraph's tokens.
#[derive(Debug, Clone)]
pub struct Span {
    pub kind: SpanKind,
    pub old: Range<usize>,
    pub new: Range<usize>,
}

#[derive(Debug)]
pub struct ParaDiff {
    pub ppr_changed: bool,
    pub spans: Vec<Span>,
}

#[derive(Debug)]
pub struct TableDiff {
    pub props_changed: bool,
    pub rows: Vec<Item<RowDetail>>,
}

#[derive(Debug)]
pub enum RowDetail {
    /// Same cell count; one entry per cell, `None` if unchanged.
    Cells { props_changed: bool, cells: Vec<Option<CellDiff>> },
    /// Cell count changed (merge/split/add column); treated as replace.
    Restructured,
}

#[derive(Debug)]
pub struct CellDiff {
    pub props_changed: bool,
    pub blocks: Vec<BlockItem>,
}

pub fn diff_blocks(old: &[Block], new: &[Block]) -> Vec<BlockItem> {
    align(old, new, true)
        .into_iter()
        .map(|step| {
            let detail = match step {
                Step::Changed(i, j) => Some(diff_block(&old[i], &new[j])),
                Step::MovedIn(i, j) if old[i].hash() != new[j].hash() => Some(diff_block(&old[i], &new[j])),
                _ => None,
            };
            Item { step, detail }
        })
        .collect()
}

fn diff_block(a: &Block, b: &Block) -> Detail {
    match (a, b) {
        (Block::Para(a), Block::Para(b)) => Detail::Para(diff_para(a, b)),
        (Block::Table(a), Block::Table(b)) => Detail::Table(diff_table(a, b)),
        _ => Detail::Opaque,
    }
}

pub fn diff_para(a: &Para, b: &Para) -> ParaDiff {
    let ta: Vec<u64> = a.tokens.iter().map(|t| t.text_hash).collect();
    let tb: Vec<u64> = b.tokens.iter().map(|t| t.text_hash).collect();
    let mut spans: Vec<Span> = Vec::new();
    for h in diff::diff(&ta, &tb) {
        match h {
            Hunk::Equal { a: i, b: j, len } => {
                for k in 0..len {
                    let kind = if a.tokens[i + k].fmt == b.tokens[j + k].fmt {
                        SpanKind::Same
                    } else {
                        SpanKind::Reformat
                    };
                    match spans.last_mut() {
                        Some(s) if s.kind == kind => {
                            s.old.end += 1;
                            s.new.end += 1;
                        }
                        _ => spans.push(Span { kind, old: i + k..i + k + 1, new: j + k..j + k + 1 }),
                    }
                }
            }
            Hunk::Change { a: old, b: new } => spans.push(Span { kind: SpanKind::Replace, old, new }),
        }
    }
    ParaDiff { ppr_changed: a.ppr != b.ppr, spans: cleanup(spans, a, b) }
}

/// Semantic cleanup, after diff-match-patch's `cleanupSemantic`: an unchanged
/// stretch between two replacements is folded into them when it is no longer
/// than the edits on either side, or is only whitespace. This turns
/// `[-and-]{+signed+} [-the-]{+a+}` chaff in rewritten sentences into one
/// readable replacement, while keeping substantial shared text visible.
fn cleanup(mut spans: Vec<Span>, a: &Para, b: &Para) -> Vec<Span> {
    let chars = |p: &Para, r: &Range<usize>| p.slice(r.clone()).chars().count();
    let edit_len = |s: &Span| chars(a, &s.old).max(chars(b, &s.new));
    let mut i = 1;
    while i + 1 < spans.len() {
        let (prev, eq, next) = (&spans[i - 1], &spans[i], &spans[i + 1]);
        let fold = eq.kind != SpanKind::Replace
            && prev.kind == SpanKind::Replace
            && next.kind == SpanKind::Replace
            && (a.tokens[eq.old.clone()].iter().all(|t| t.kind == TokenKind::Space)
                || chars(a, &eq.old) <= edit_len(prev).min(edit_len(next)));
        if fold {
            let (old_end, new_end) = (next.old.end, next.new.end);
            spans.drain(i..i + 2);
            spans[i - 1].old.end = old_end;
            spans[i - 1].new.end = new_end;
            // The grown replacement may now swallow the equality before it.
            i = if i >= 3 { i - 2 } else { 1 };
        } else {
            i += 1;
        }
    }
    spans
}

pub fn diff_table(a: &Table, b: &Table) -> TableDiff {
    let rows = align(&a.rows, &b.rows, true)
        .into_iter()
        .map(|step| {
            let detail = match step {
                Step::Changed(i, j) => Some(diff_row(&a.rows[i], &b.rows[j])),
                Step::MovedIn(i, j) if a.rows[i].hash != b.rows[j].hash => Some(diff_row(&a.rows[i], &b.rows[j])),
                _ => None,
            };
            Item { step, detail }
        })
        .collect();
    TableDiff { props_changed: a.props != b.props, rows }
}

fn diff_row(a: &crate::model::Row, b: &crate::model::Row) -> RowDetail {
    if a.cells.len() != b.cells.len() {
        return RowDetail::Restructured;
    }
    let cells = a
        .cells
        .iter()
        .zip(&b.cells)
        .map(|(x, y)| {
            (x.hash != y.hash).then(|| CellDiff {
                props_changed: x.props != y.props,
                blocks: diff_blocks(&x.blocks, &y.blocks),
            })
        })
        .collect();
    RowDetail::Cells { props_changed: a.props != b.props, cells }
}

/// Counts of top-level changes, for summaries.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub changed: usize,
    pub inserted: usize,
    pub deleted: usize,
    pub moved: usize,
    /// Empty paragraphs inserted or deleted (layout spacing, usually).
    pub blank: usize,
}

pub fn stats(old: &[Block], new: &[Block], items: &[BlockItem]) -> Stats {
    let mut s = Stats::default();
    for it in items {
        match it.step {
            Step::Equal(..) | Step::MovedOut(..) => {}
            Step::Changed(..) => s.changed += 1,
            Step::Inserted(j) if is_blank(&new[j]) => s.blank += 1,
            Step::Deleted(i) if is_blank(&old[i]) => s.blank += 1,
            Step::Inserted(_) => s.inserted += 1,
            Step::Deleted(_) => s.deleted += 1,
            Step::MovedIn(..) => s.moved += 1,
        }
    }
    s
}

pub fn is_blank(b: &Block) -> bool {
    matches!(b, Block::Para(p) if p.tokens.is_empty())
}
