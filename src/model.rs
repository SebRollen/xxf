//! Normalized document model. Run boundaries, revision IDs and proofing marks
//! are gone; what remains is block structure, text, and formatting hashes.

use std::ops::Range;

use crate::align::Unit;
use crate::hash::H;

/// Placeholder character standing in for an inline object (image, math, etc.).
pub const OBJ: char = '\u{FFFC}';

const KIND_PARA: u64 = 1;
const KIND_TABLE: u64 = 2;
const KIND_ROW: u64 = 3;
const KIND_CELL: u64 = 4;
const KIND_OPAQUE: u64 = 5;

#[derive(Debug, Default)]
pub struct Document {
    pub blocks: Vec<Block>,
}

#[derive(Debug)]
pub enum Block {
    Para(Para),
    Table(Table),
    /// A block-level element we don't model, e.g. `w:sectPr`; compared by hash only.
    Opaque(Opaque),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    Word,
    Space,
    Punct,
    Object,
}

#[derive(Debug, Clone)]
pub struct Token {
    /// Byte range into `Para::text`.
    pub start: u32,
    pub end: u32,
    pub kind: TokenKind,
    pub text_hash: u64,
    /// Hash of the effective run properties covering this token (0 = all defaults).
    pub fmt: u64,
}

#[derive(Debug)]
pub struct Para {
    pub text: String,
    pub tokens: Vec<Token>,
    /// Hash of effective paragraph properties (0 = all defaults).
    pub ppr: u64,
    /// Paragraph style ID, for reporting only; comparison uses `ppr`.
    pub style: Option<String>,
    pub text_hash: u64,
    /// Text + formatting + paragraph properties.
    pub hash: u64,
    /// Sorted hashes of word/object tokens, for similarity.
    pub bag: Vec<u64>,
}

#[derive(Debug)]
pub struct Table {
    pub rows: Vec<Row>,
    pub props: u64,
    pub text_hash: u64,
    pub hash: u64,
    pub bag: Vec<u64>,
}

#[derive(Debug)]
pub struct Row {
    pub cells: Vec<Cell>,
    pub props: u64,
    pub text_hash: u64,
    pub hash: u64,
    pub bag: Vec<u64>,
}

#[derive(Debug)]
pub struct Cell {
    pub blocks: Vec<Block>,
    pub props: u64,
    pub text_hash: u64,
    pub hash: u64,
    pub bag: Vec<u64>,
}

#[derive(Debug)]
pub struct Opaque {
    pub name: String,
    pub hash: u64,
    pub text_hash: u64,
}

impl Para {
    pub fn new(text: String, tokens: Vec<Token>, ppr: u64, style: Option<String>) -> Self {
        let mut th = H::new();
        th.u64(KIND_PARA);
        let mut h = H::new();
        h.u64(ppr);
        for t in &tokens {
            th.u64(t.text_hash);
            h.u64(t.fmt);
        }
        let text_hash = th.finish();
        h.u64(text_hash);
        let mut bag: Vec<u64> = tokens
            .iter()
            .filter(|t| matches!(t.kind, TokenKind::Word | TokenKind::Object))
            .map(|t| t.text_hash)
            .collect();
        bag.sort_unstable();
        Self { text, tokens, ppr, style, text_hash, hash: h.finish(), bag }
    }

    /// Text covered by a range of tokens.
    pub fn slice(&self, r: Range<usize>) -> &str {
        if r.is_empty() {
            return "";
        }
        &self.text[self.tokens[r.start].start as usize..self.tokens[r.end - 1].end as usize]
    }
}

/// Combine children into (text_hash, hash, bag) for a container node.
fn combine<'a>(
    kind: u64,
    props: u64,
    children: impl Iterator<Item = (u64, u64, &'a [u64])>,
) -> (u64, u64, Vec<u64>) {
    let mut th = H::new();
    th.u64(kind);
    let mut h = H::new();
    h.u64(props);
    let mut bag = Vec::new();
    for (t, x, b) in children {
        th.u64(t);
        h.u64(x);
        bag.extend_from_slice(b);
    }
    let text_hash = th.finish();
    h.u64(text_hash);
    bag.sort_unstable();
    (text_hash, h.finish(), bag)
}

impl Cell {
    pub fn new(blocks: Vec<Block>, props: u64) -> Self {
        let (text_hash, hash, bag) = combine(
            KIND_CELL,
            props,
            blocks.iter().map(|b| (b.text_hash(), b.hash(), b.bag())),
        );
        Self { blocks, props, text_hash, hash, bag }
    }
}

impl Row {
    pub fn new(cells: Vec<Cell>, props: u64) -> Self {
        let (text_hash, hash, bag) = combine(
            KIND_ROW,
            props,
            cells.iter().map(|c| (c.text_hash, c.hash, c.bag.as_slice())),
        );
        Self { cells, props, text_hash, hash, bag }
    }
}

impl Table {
    pub fn new(rows: Vec<Row>, props: u64) -> Self {
        let (text_hash, hash, bag) = combine(
            KIND_TABLE,
            props,
            rows.iter().map(|r| (r.text_hash, r.hash, r.bag.as_slice())),
        );
        Self { rows, props, text_hash, hash, bag }
    }
}

impl Opaque {
    pub fn new(name: String, hash: u64) -> Self {
        let mut th = H::new();
        th.u64(KIND_OPAQUE);
        th.bytes(name.as_bytes());
        Self { name, hash, text_hash: th.finish() }
    }
}

impl Unit for Block {
    fn hash(&self) -> u64 {
        match self {
            Block::Para(p) => p.hash,
            Block::Table(t) => t.hash,
            Block::Opaque(o) => o.hash,
        }
    }

    fn text_hash(&self) -> u64 {
        match self {
            Block::Para(p) => p.text_hash,
            Block::Table(t) => t.text_hash,
            Block::Opaque(o) => o.text_hash,
        }
    }

    fn bag(&self) -> &[u64] {
        match self {
            Block::Para(p) => &p.bag,
            Block::Table(t) => &t.bag,
            Block::Opaque(_) => &[],
        }
    }

    fn kind(&self) -> u8 {
        match self {
            Block::Para(_) => 0,
            Block::Table(_) => 1,
            Block::Opaque(_) => 2,
        }
    }
}

impl Unit for Row {
    fn hash(&self) -> u64 {
        self.hash
    }

    fn text_hash(&self) -> u64 {
        self.text_hash
    }

    fn bag(&self) -> &[u64] {
        &self.bag
    }

    fn kind(&self) -> u8 {
        0
    }
}
