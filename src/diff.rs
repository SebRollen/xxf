//! Myers' O(ND) sequence diff, linear-space divide-and-conquer variant.

use std::hash::Hash;
use std::ops::{Index, IndexMut, Range};

use rustc_hash::FxHashMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hunk {
    /// `a[a..a + len] == b[b..b + len]`
    Equal { a: usize, b: usize, len: usize },
    /// `a[a]` was replaced by `b[b]`; either side may be empty.
    Change { a: Range<usize>, b: Range<usize> },
}

/// Inputs at least this long are first split at anchors.
const ANCHOR_MIN_LEN: usize = 4096;

/// Diff two sequences into alternating equal / change hunks.
///
/// Long inputs are first anchored on elements that occur exactly once in each
/// (patience diff): the longest increasing run of such matches splits the
/// problem into small independent gaps, each diffed with Myers. Short inputs
/// get a plain, minimal Myers diff.
pub fn diff<T: Eq + Hash>(a: &[T], b: &[T]) -> Vec<Hunk> {
    let max = max_d(a.len(), b.len());
    let mut vf = V::new(max);
    let mut vb = V::new(max);
    let mut out = Vec::new();
    let (mut pa, mut pb) = (0, 0);
    if a.len() + b.len() >= ANCHOR_MIN_LEN {
        for (ia, ib) in anchors(a, b) {
            conquer(a, pa..ia, b, pb..ib, &mut vf, &mut vb, &mut out);
            push_equal(&mut out, ia, ib, 1);
            (pa, pb) = (ia + 1, ib + 1);
        }
    }
    conquer(a, pa..a.len(), b, pb..b.len(), &mut vf, &mut vb, &mut out);
    out
}

/// Matches of elements unique in both sequences, longest increasing subset.
fn anchors<T: Eq + Hash>(a: &[T], b: &[T]) -> Vec<(usize, usize)> {
    // Per element: (count in a, count in b, index in a, index in b).
    let mut seen: FxHashMap<&T, (u32, u32, usize, usize)> = FxHashMap::default();
    for (i, x) in a.iter().enumerate() {
        let e = seen.entry(x).or_insert((0, 0, i, 0));
        e.0 += 1;
    }
    for (j, x) in b.iter().enumerate() {
        if let Some(e) = seen.get_mut(x) {
            e.1 += 1;
            e.3 = j;
        }
    }
    let mut pairs: Vec<(usize, usize)> =
        seen.into_values().filter(|e| e.0 == 1 && e.1 == 1).map(|e| (e.2, e.3)).collect();
    pairs.sort_unstable();
    // Longest increasing subsequence of the b indices (patience sorting).
    let mut tails: Vec<usize> = Vec::new(); // index into pairs ending a run of each length
    let mut prev: Vec<usize> = vec![usize::MAX; pairs.len()];
    for (i, &(_, j)) in pairs.iter().enumerate() {
        let len = tails.partition_point(|&t| pairs[t].1 < j);
        if len > 0 {
            prev[i] = tails[len - 1];
        }
        if len == tails.len() {
            tails.push(i);
        } else {
            tails[len] = i;
        }
    }
    let mut out = Vec::with_capacity(tails.len());
    let mut cur = tails.last().copied().unwrap_or(usize::MAX);
    while cur != usize::MAX {
        out.push(pairs[cur]);
        cur = prev[cur];
    }
    out.reverse();
    out
}

fn push_equal(out: &mut Vec<Hunk>, a: usize, b: usize, len: usize) {
    if len == 0 {
        return;
    }
    if let Some(Hunk::Equal { a: la, len: ll, .. }) = out.last_mut()
        && *la + *ll == a
    {
        *ll += len;
        return;
    }
    out.push(Hunk::Equal { a, b, len });
}

fn push_change(out: &mut Vec<Hunk>, a: Range<usize>, b: Range<usize>) {
    if a.is_empty() && b.is_empty() {
        return;
    }
    // Recursion leaves arrive in order, so consecutive changes are contiguous.
    if let Some(Hunk::Change { a: la, b: lb }) = out.last_mut() {
        la.end = a.end;
        lb.end = b.end;
        return;
    }
    out.push(Hunk::Change { a, b });
}

struct V {
    offset: isize,
    v: Vec<usize>,
}

impl V {
    fn new(max_d: usize) -> Self {
        Self { offset: max_d as isize, v: vec![0; 2 * max_d] }
    }
}

impl Index<isize> for V {
    type Output = usize;
    fn index(&self, k: isize) -> &usize {
        &self.v[(k + self.offset) as usize]
    }
}

impl IndexMut<isize> for V {
    fn index_mut(&mut self, k: isize) -> &mut usize {
        &mut self.v[(k + self.offset) as usize]
    }
}

fn max_d(n: usize, m: usize) -> usize {
    (n + m).div_ceil(2) + 1
}

fn common_prefix<T: Eq>(a: &[T], ar: Range<usize>, b: &[T], br: Range<usize>) -> usize {
    a[ar].iter().zip(&b[br]).take_while(|(x, y)| x == y).count()
}

fn common_suffix<T: Eq>(a: &[T], ar: Range<usize>, b: &[T], br: Range<usize>) -> usize {
    a[ar].iter().rev().zip(b[br].iter().rev()).take_while(|(x, y)| x == y).count()
}

/// Find a point on an optimal edit path roughly halfway through the edit
/// script, searching forward and backward simultaneously.
fn middle_snake<T: Eq>(
    a: &[T],
    ar: Range<usize>,
    b: &[T],
    br: Range<usize>,
    vf: &mut V,
    vb: &mut V,
) -> Option<(usize, usize)> {
    let n = ar.len();
    let m = br.len();
    let delta = n as isize - m as isize;
    let odd = delta & 1 == 1;
    vf[1] = 0;
    vb[1] = 0;
    for d in 0..max_d(n, m) as isize {
        if d >= MIN_COST_CAP && d as usize * (n + m) > COST_BUDGET {
            return None;
        }
        for k in (-d..=d).rev().step_by(2) {
            let mut x = if k == -d || (k != d && vf[k - 1] < vf[k + 1]) {
                vf[k + 1]
            } else {
                vf[k - 1] + 1
            };
            let y = (x as isize - k) as usize;
            let (x0, y0) = (x, y);
            if x < n && y < m {
                x += common_prefix(a, ar.start + x..ar.end, b, br.start + y..br.end);
            }
            vf[k] = x;
            if odd && (k - delta).abs() < d && vf[k] + vb[-(k - delta)] >= n {
                return Some((x0 + ar.start, y0 + br.start));
            }
        }
        for k in (-d..=d).rev().step_by(2) {
            let mut x = if k == -d || (k != d && vb[k - 1] < vb[k + 1]) {
                vb[k + 1]
            } else {
                vb[k - 1] + 1
            };
            let mut y = (x as isize - k) as usize;
            if x < n && y < m {
                let s = common_suffix(a, ar.start..ar.start + n - x, b, br.start..br.start + m - y);
                x += s;
                y += s;
            }
            vb[k] = x;
            if !odd && (k - delta).abs() <= d && vb[k] + vf[-(k - delta)] >= n {
                return Some((n - x + ar.start, m - y + br.start));
            }
        }
    }
    None
}

/// Past this much work (edit distance × range length) the search gives up
/// and reports the whole range as one change. With anchoring (below) such
/// ranges are the stretches between anchors, so this only bites on genuinely
/// unrelated content, which then costs near-linear time instead of quadratic.
const COST_BUDGET: usize = 50_000_000;
const MIN_COST_CAP: isize = 256;

fn conquer<T: Eq>(
    a: &[T],
    mut ar: Range<usize>,
    b: &[T],
    mut br: Range<usize>,
    vf: &mut V,
    vb: &mut V,
    out: &mut Vec<Hunk>,
) {
    let pre = common_prefix(a, ar.clone(), b, br.clone());
    push_equal(out, ar.start, br.start, pre);
    ar.start += pre;
    br.start += pre;
    let suf = common_suffix(a, ar.clone(), b, br.clone());
    ar.end -= suf;
    br.end -= suf;

    if ar.is_empty() || br.is_empty() {
        push_change(out, ar.clone(), br.clone());
    } else if let Some((x, y)) = middle_snake(a, ar.clone(), b, br.clone(), vf, vb) {
        conquer(a, ar.start..x, b, br.start..y, vf, vb, out);
        conquer(a, x..ar.end, b, y..br.end, vf, vb, out);
    } else {
        push_change(out, ar.clone(), br.clone());
    }

    push_equal(out, ar.end, br.end, suf);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lcs_len(a: &[u8], b: &[u8]) -> usize {
        let mut f = vec![vec![0usize; b.len() + 1]; a.len() + 1];
        for i in 1..=a.len() {
            for j in 1..=b.len() {
                f[i][j] = if a[i - 1] == b[j - 1] {
                    f[i - 1][j - 1] + 1
                } else {
                    f[i - 1][j].max(f[i][j - 1])
                };
            }
        }
        f[a.len()][b.len()]
    }

    /// Checks the hunks tile both sequences exactly and equal hunks really are equal.
    fn check(a: &[u8], b: &[u8]) -> usize {
        check_hunks(a, b, &diff(a, b))
    }

    fn check_hunks<T: Eq + std::fmt::Debug>(a: &[T], b: &[T], hunks: &[Hunk]) -> usize {
        let (mut i, mut j, mut same) = (0, 0, 0);
        for h in hunks.iter().cloned() {
            match h {
                Hunk::Equal { a: x, b: y, len } => {
                    assert_eq!((x, y), (i, j));
                    assert_eq!(a[x..x + len], b[y..y + len]);
                    i += len;
                    j += len;
                    same += len;
                }
                Hunk::Change { a: ra, b: rb } => {
                    assert_eq!((ra.start, rb.start), (i, j));
                    i = ra.end;
                    j = rb.end;
                }
            }
        }
        assert_eq!((i, j), (a.len(), b.len()));
        same
    }

    #[test]
    fn simple() {
        assert_eq!(check(b"abcabba", b"cbabac"), 4);
        assert_eq!(check(b"", b"abc"), 0);
        assert_eq!(check(b"abc", b""), 0);
        assert_eq!(check(b"abc", b"abc"), 3);
    }

    #[test]
    fn long_inputs_use_anchors_and_stay_valid() {
        // Unrelated halves around a shared unique middle: anchored, and the
        // unrelated parts exceed the cost budget and become single changes.
        let mut a: Vec<u32> = (0..20_000).collect();
        let mut b: Vec<u32> = (100_000..120_000).collect();
        a.extend(500_000..500_100);
        b.extend(500_000..500_100);
        a.extend(200_000..220_000);
        b.extend(300_000..320_000);
        let t = std::time::Instant::now();
        let hunks = diff(&a, &b);
        assert!(t.elapsed().as_secs() < 5, "too slow: {:?}", t.elapsed());
        let same: usize = hunks.iter().map(|h| if let Hunk::Equal { len, .. } = h { *len } else { 0 }).sum();
        assert_eq!(same, 100);
        check_hunks(&a, &b, &hunks);
    }

    #[test]
    fn matches_dp_lcs_on_random_inputs() {
        let mut s = 0x9e3779b97f4a7c15u64;
        let mut rnd = move || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s
        };
        for _ in 0..2000 {
            let alpha = 2 + rnd() % 4;
            let a: Vec<u8> = (0..rnd() % 40).map(|_| (rnd() % alpha) as u8).collect();
            let b: Vec<u8> = (0..rnd() % 40).map(|_| (rnd() % alpha) as u8).collect();
            assert_eq!(check(&a, &b), lcs_len(&a, &b), "{a:?} vs {b:?}");
        }
    }
}
