//! Normalized document model. Run boundaries, revision IDs and proofing marks
//! are gone; what remains is block structure, text, and formatting hashes.
//!
//! Every node also records where it came from in the source `document.xml`
//! (byte offsets), so a redline can be written by copying unchanged XML
//! verbatim and rewriting only what changed.

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

/// Byte range into the source `document.xml`.
pub type Src = Range<u32>;

#[derive(Debug, Default)]
pub struct Document {
    pub blocks: Vec<Block>,
    /// Content of `w:body`, between its start and end tags.
    pub body: Src,
}

/// Where a paragraph's pieces live in the source XML.
#[derive(Debug, Default, Clone)]
pub struct ParaSrc {
    /// The whole `w:p` element.
    pub span: Src,
    /// End of the `<w:p ...>` start tag.
    pub stag_end: u32,
    /// The whole `w:pPr` element, if any.
    pub ppr: Option<Src>,
    /// Written as `<w:p/>`.
    pub empty_elem: bool,
    pub runs: Vec<RunSrc>,
}

#[derive(Debug, Clone)]
pub struct RunSrc {
    /// The whole `w:r` element.
    pub span: Src,
    /// Content of `w:rPr`, if any.
    pub rpr: Option<Src>,
    pub children: Vec<RunChild>,
}

#[derive(Debug, Clone)]
pub struct RunChild {
    /// The whole child element.
    pub span: Src,
    /// What it contributed to `Para::text`.
    pub text: Range<u32>,
    pub kind: ChildKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildKind {
    /// `w:t` (or `w:delText` when rejecting revisions): splittable text.
    Text,
    /// `w:instrText`: splittable field instruction text.
    Instr,
    /// Indivisible content: tab, break, symbol, drawing, note reference.
    Raw,
    /// Contributes no text: field characters, rendering hints.
    Marker,
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
    pub src: ParaSrc,
}

#[derive(Debug)]
pub struct Table {
    pub span: Src,
    pub rows: Vec<Row>,
    pub props: u64,
    pub text_hash: u64,
    pub hash: u64,
    pub bag: Vec<u64>,
}

#[derive(Debug)]
pub struct Row {
    pub span: Src,
    /// Where a `w:trPr` would go: after the start tag and any `w:tblPrEx`.
    pub trpr_at: u32,
    /// The whole `w:trPr` element, if any.
    pub trpr: Option<Src>,
    pub cells: Vec<Cell>,
    pub props: u64,
    pub text_hash: u64,
    pub hash: u64,
    pub bag: Vec<u64>,
}

#[derive(Debug)]
pub struct Cell {
    pub span: Src,
    /// Between the start and end tags (includes `w:tcPr`).
    pub content: Src,
    /// The whole `w:tcPr` element, if any.
    pub tcpr: Option<Src>,
    pub blocks: Vec<Block>,
    pub props: u64,
    pub text_hash: u64,
    pub hash: u64,
    pub bag: Vec<u64>,
}

#[derive(Debug)]
pub struct Opaque {
    pub span: Src,
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
        Self { text, tokens, ppr, style, text_hash, hash: h.finish(), bag, src: ParaSrc::default() }
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
        Self { span: 0..0, content: 0..0, tcpr: None, blocks, props, text_hash, hash, bag }
    }
}

impl Row {
    pub fn new(cells: Vec<Cell>, props: u64) -> Self {
        let (text_hash, hash, bag) = combine(
            KIND_ROW,
            props,
            cells.iter().map(|c| (c.text_hash, c.hash, c.bag.as_slice())),
        );
        Self { span: 0..0, trpr_at: 0, trpr: None, cells, props, text_hash, hash, bag }
    }
}

impl Table {
    pub fn new(rows: Vec<Row>, props: u64) -> Self {
        let (text_hash, hash, bag) = combine(
            KIND_TABLE,
            props,
            rows.iter().map(|r| (r.text_hash, r.hash, r.bag.as_slice())),
        );
        Self { span: 0..0, rows, props, text_hash, hash, bag }
    }
}

impl Opaque {
    pub fn new(name: String, hash: u64) -> Self {
        let mut th = H::new();
        th.u64(KIND_OPAQUE);
        th.bytes(name.as_bytes());
        Self { span: 0..0, name, hash, text_hash: th.finish() }
    }
}

impl Block {
    /// The whole element in the source XML.
    pub fn span(&self) -> Src {
        match self {
            Block::Para(p) => p.src.span.clone(),
            Block::Table(t) => t.span.clone(),
            Block::Opaque(o) => o.span.clone(),
        }
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
