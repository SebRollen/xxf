//! Alignment of block-like sequences (paragraphs, table rows).
//!
//! 1. Myers diff on full content hashes gives exact matches.
//! 2. Inside each changed hunk, Myers on text-only hashes pairs units whose
//!    text is identical but formatting/properties differ.
//! 3. Remaining units in the hunk are paired by bag-of-words similarity with an
//!    order-preserving max-weight alignment.
//! 4. Unpaired deletions/insertions with identical text anywhere in the
//!    document become moves.

use std::collections::VecDeque;
use std::ops::Range;

use rustc_hash::FxHashMap;

use crate::diff::{self, Hunk};

pub trait Unit {
    /// Hash of everything that matters: text, formatting, properties.
    fn hash(&self) -> u64;
    /// Hash of text content only.
    fn text_hash(&self) -> u64;
    /// Sorted token hashes, for similarity.
    fn bag(&self) -> &[u64];
    /// Units of different kinds are never paired.
    fn kind(&self) -> u8;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Equal(usize, usize),
    /// Same unit, modified.
    Changed(usize, usize),
    Deleted(usize),
    Inserted(usize),
    /// Old position of a moved unit: (old, new).
    MovedOut(usize, usize),
    /// New position of a moved unit: (old, new). May also be modified.
    MovedIn(usize, usize),
}

/// Minimum Dice similarity for two units to be considered the same, edited.
const SIMILARITY: f64 = 0.5;
/// Hunks larger than this many old×new pairs skip similarity pairing.
const MAX_PAIR_CELLS: usize = 1 << 16;
/// Units with fewer words than this are too generic to call moves.
const MIN_MOVE_WORDS: usize = 3;

pub fn align<T: Unit>(old: &[T], new: &[T], moves: bool) -> Vec<Step> {
    let ha: Vec<u64> = old.iter().map(Unit::hash).collect();
    let hb: Vec<u64> = new.iter().map(Unit::hash).collect();
    let mut steps = Vec::with_capacity(old.len().max(new.len()));
    for h in diff::diff(&ha, &hb) {
        match h {
            Hunk::Equal { a, b, len } => steps.extend((0..len).map(|k| Step::Equal(a + k, b + k))),
            Hunk::Change { a, b } => pair_hunk(old, new, a, b, &mut steps),
        }
    }
    if moves {
        find_moves(old, new, &mut steps);
    }
    steps
}

fn pair_hunk<T: Unit>(old: &[T], new: &[T], or: Range<usize>, nr: Range<usize>, steps: &mut Vec<Step>) {
    if or.is_empty() || nr.is_empty() {
        steps.extend(or.map(Step::Deleted));
        steps.extend(nr.map(Step::Inserted));
        return;
    }
    let ta: Vec<u64> = old[or.clone()].iter().map(Unit::text_hash).collect();
    let tb: Vec<u64> = new[nr.clone()].iter().map(Unit::text_hash).collect();
    for h in diff::diff(&ta, &tb) {
        match h {
            Hunk::Equal { a, b, len } => {
                steps.extend((0..len).map(|k| Step::Changed(or.start + a + k, nr.start + b + k)))
            }
            Hunk::Change { a, b } => pair_similar(
                old,
                new,
                or.start + a.start..or.start + a.end,
                nr.start + b.start..nr.start + b.end,
                steps,
            ),
        }
    }
}

/// Dice coefficient of two sorted token multisets.
pub fn similarity<T: Unit>(a: &T, b: &T) -> f64 {
    if a.kind() != b.kind() {
        return 0.0;
    }
    let (x, y) = (a.bag(), b.bag());
    let total = (x.len() + y.len()) as f64;
    if x.is_empty() || y.is_empty() || 2.0 * x.len().min(y.len()) as f64 / total < SIMILARITY {
        return 0.0;
    }
    let (mut i, mut j, mut common) = (0, 0, 0);
    while i < x.len() && j < y.len() {
        match x[i].cmp(&y[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                common += 1;
                i += 1;
                j += 1;
            }
        }
    }
    2.0 * common as f64 / total
}

fn pair_similar<T: Unit>(old: &[T], new: &[T], or: Range<usize>, nr: Range<usize>, steps: &mut Vec<Step>) {
    let (n, m) = (or.len(), nr.len());
    let mut pairs = Vec::new();
    let lone_short_pair = n == 1 && m == 1 && {
        let (a, b) = (&old[or.start], &new[nr.start]);
        a.kind() == b.kind() && a.bag().len().min(b.bag().len()) < MIN_MOVE_WORDS
    };
    if lone_short_pair {
        // A lone short unit replaced in place ("3" → "4") is an edit; it is
        // too small to be a move candidate anyway.
        pairs.push((0, 0));
    } else if n > 0 && m > 0 && n * m <= MAX_PAIR_CELLS {
        // Order-preserving matching maximizing total similarity (LCS-style DP).
        let w = m + 1;
        let mut f = vec![0f64; (n + 1) * w];
        for i in 1..=n {
            for j in 1..=m {
                let mut best = f[(i - 1) * w + j].max(f[i * w + j - 1]);
                let s = similarity(&old[or.start + i - 1], &new[nr.start + j - 1]);
                if s >= SIMILARITY {
                    best = best.max(f[(i - 1) * w + j - 1] + s);
                }
                f[i * w + j] = best;
            }
        }
        let (mut i, mut j) = (n, m);
        while i > 0 && j > 0 {
            if f[i * w + j] == f[(i - 1) * w + j] {
                i -= 1;
            } else if f[i * w + j] == f[i * w + j - 1] {
                j -= 1;
            } else {
                pairs.push((i - 1, j - 1));
                i -= 1;
                j -= 1;
            }
        }
        pairs.reverse();
    }
    let (mut i, mut j) = (0, 0);
    for (pi, pj) in pairs.into_iter().chain(std::iter::once((n, m))) {
        steps.extend((i..pi).map(|k| Step::Deleted(or.start + k)));
        steps.extend((j..pj).map(|k| Step::Inserted(nr.start + k)));
        if pi < n {
            steps.push(Step::Changed(or.start + pi, nr.start + pj));
        }
        i = pi + 1;
        j = pj + 1;
    }
}

fn find_moves<T: Unit>(old: &[T], new: &[T], steps: &mut [Step]) {
    let mut inserted: FxHashMap<u64, VecDeque<usize>> = FxHashMap::default();
    for (k, s) in steps.iter().enumerate() {
        if let Step::Inserted(j) = *s
            && new[j].bag().len() >= MIN_MOVE_WORDS
        {
            inserted.entry(new[j].text_hash()).or_default().push_back(k);
        }
    }
    if inserted.is_empty() {
        return;
    }
    for k in 0..steps.len() {
        if let Step::Deleted(i) = steps[k]
            && old[i].bag().len() >= MIN_MOVE_WORDS
            && let Some(q) = inserted.get_mut(&old[i].text_hash())
            && let Some(ki) = q.pop_front()
        {
            let Step::Inserted(j) = steps[ki] else { unreachable!() };
            steps[k] = Step::MovedOut(i, j);
            steps[ki] = Step::MovedIn(i, j);
        }
    }
}
