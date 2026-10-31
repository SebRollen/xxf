//! Streaming parser from `word/document.xml` to the normalized model.
//!
//! Normalization, applied while parsing:
//! - existing tracked changes are accepted (`w:del`/`w:moveFrom` dropped,
//!   `w:ins`/`w:moveTo` unwrapped);
//! - run boundaries disappear: text is concatenated per paragraph and
//!   formatting is tracked per character range;
//! - revision IDs (`w:rsid*`), proofing marks, bookmarks, comment anchors and
//!   other bookkeeping are ignored;
//! - relationship IDs (`r:embed`, `r:id`, ...) are replaced by a hash of what
//!   they point to, since the IDs differ between files.
//!
//! Element names are matched by qualified name with Word's conventional `w:`
//! prefix.

use std::borrow::Cow;

use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};
use rustc_hash::FxHashMap;

use crate::Result;
use crate::hash::{H, hash_bytes};
use crate::model::*;

/// Relationship ID → hash of the target's content (or URL for external targets).
pub type Rels = FxHashMap<String, u64>;

/// Bookkeeping elements that never carry content.
const IGNORED: &[&[u8]] = &[
    b"w:proofErr",
    b"w:lastRenderedPageBreak",
    b"w:bookmarkStart",
    b"w:bookmarkEnd",
    b"w:commentRangeStart",
    b"w:commentRangeEnd",
    b"w:commentReference",
    b"w:permStart",
    b"w:permEnd",
    b"w:moveFromRangeStart",
    b"w:moveFromRangeEnd",
    b"w:moveToRangeStart",
    b"w:moveToRangeEnd",
    b"w:customXmlInsRangeStart",
    b"w:customXmlInsRangeEnd",
    b"w:customXmlDelRangeStart",
    b"w:customXmlDelRangeEnd",
];

/// Containers whose children are parsed as if the wrapper weren't there.
const TRANSPARENT: &[&[u8]] = &[
    b"w:ins",
    b"w:moveTo",
    b"w:sdt",
    b"w:sdtContent",
    b"w:customXml",
    b"w:smartTag",
    b"w:hyperlink",
    b"w:fldSimple",
    b"w:dir",
    b"w:bdo",
];

/// Subtrees dropped entirely: deleted content and wrapper metadata.
const SKIPPED: &[&[u8]] = &[
    b"w:del",
    b"w:moveFrom",
    b"w:sdtPr",
    b"w:sdtEndPr",
    b"w:customXmlPr",
    b"w:smartTagPr",
];

/// Inside property elements: revision history and spell-check state, which
/// don't change how the document looks.
const PROP_NOISE: &[&[u8]] = &[
    b"w:rPrChange",
    b"w:pPrChange",
    b"w:sectPrChange",
    b"w:tblPrChange",
    b"w:tblPrExChange",
    b"w:trPrChange",
    b"w:tcPrChange",
    b"w:tblGridChange",
    b"w:numberingChange",
    b"w:ins",
    b"w:del",
    b"w:moveFrom",
    b"w:moveTo",
    b"w:noProof",
    b"w:lang",
];

const REL_ATTRS: &[&[u8]] = &[b"r:id", b"r:embed", b"r:link", b"r:pict", b"r:dm", b"r:lo", b"r:qs", b"r:cs"];

/// Marks field instruction text so it never matches body text.
const FIELD_CODE: u64 = 0x5eed_f1e1_d00c_0de5;

pub fn parse_document(xml: &str, rels: &Rels) -> Result<Document> {
    let mut p = Parser { r: Reader::from_str(xml), rels };
    loop {
        match p.r.read_event()? {
            Event::Start(e) if e.name().as_ref() == b"w:body" => {
                let blocks = p.blocks(b"w:body", &mut 0)?;
                return Ok(Document { blocks });
            }
            Event::Empty(e) if e.name().as_ref() == b"w:body" => return Ok(Document::default()),
            Event::Eof => return Err("document.xml has no w:body".into()),
            _ => {}
        }
    }
}

struct Parser<'a> {
    r: Reader<&'a [u8]>,
    rels: &'a Rels,
}

fn is(name: &[u8], set: &[&[u8]]) -> bool {
    set.contains(&name)
}

fn eof(ctx: &str) -> crate::Error {
    format!("unexpected end of document.xml inside {ctx}").into()
}

impl<'a> Parser<'a> {
    /// Block-level content until `</end>`. `props` receives a `w:tcPr` hash.
    fn blocks(&mut self, end: &[u8], props: &mut u64) -> Result<Vec<Block>> {
        let mut out = Vec::new();
        loop {
            match self.r.read_event()? {
                Event::Start(e) => match e.name().as_ref() {
                    b"w:p" => out.push(Block::Para(self.para()?)),
                    b"w:tbl" => out.push(Block::Table(self.table()?)),
                    b"w:tcPr" => *props = self.hash_elem(&e, false, true)?,
                    n if is(n, TRANSPARENT) => {}
                    n if is(n, SKIPPED) || is(n, IGNORED) => {
                        self.r.read_to_end(e.name())?;
                    }
                    n => {
                        let name = String::from_utf8_lossy(n).into_owned();
                        let props = n.ends_with(b"Pr");
                        let h = self.hash_elem(&e, false, props)?;
                        out.push(Block::Opaque(Opaque::new(name, h)));
                    }
                },
                Event::Empty(e) if e.name().as_ref() == b"w:p" => {
                    out.push(Block::Para(Para::new(String::new(), Vec::new(), 0)))
                }
                Event::End(e) if e.name().as_ref() == end => return Ok(out),
                Event::Eof => return Err(eof("block content")),
                _ => {}
            }
        }
    }

    fn table(&mut self) -> Result<Table> {
        let mut props = H::new();
        let mut rows = Vec::new();
        loop {
            match self.r.read_event()? {
                Event::Start(e) => match e.name().as_ref() {
                    b"w:tblPr" | b"w:tblGrid" => props.u64(self.hash_elem(&e, false, true)?),
                    b"w:tr" => rows.push(self.row()?),
                    n if is(n, TRANSPARENT) => {}
                    _ => {
                        self.r.read_to_end(e.name())?;
                    }
                },
                Event::End(e) if e.name().as_ref() == b"w:tbl" => return Ok(Table::new(rows, props.finish())),
                Event::Eof => return Err(eof("w:tbl")),
                _ => {}
            }
        }
    }

    fn row(&mut self) -> Result<Row> {
        let mut props = H::new();
        let mut cells = Vec::new();
        loop {
            match self.r.read_event()? {
                Event::Start(e) => match e.name().as_ref() {
                    b"w:trPr" | b"w:tblPrEx" => props.u64(self.hash_elem(&e, false, true)?),
                    b"w:tc" => {
                        let mut cell_props = 0;
                        let blocks = self.blocks(b"w:tc", &mut cell_props)?;
                        cells.push(Cell::new(blocks, cell_props));
                    }
                    n if is(n, TRANSPARENT) => {}
                    _ => {
                        self.r.read_to_end(e.name())?;
                    }
                },
                Event::End(e) if e.name().as_ref() == b"w:tr" => return Ok(Row::new(cells, props.finish())),
                Event::Eof => return Err(eof("w:tr")),
                _ => {}
            }
        }
    }

    fn para(&mut self) -> Result<Para> {
        let mut b = ParaBuilder::default();
        loop {
            match self.r.read_event()? {
                Event::Start(e) => match e.name().as_ref() {
                    b"w:pPr" => b.ppr = self.hash_elem(&e, false, true)?,
                    b"w:r" => self.run(&mut b)?,
                    n if is(n, TRANSPARENT) => {}
                    n if is(n, SKIPPED) || is(n, IGNORED) => {
                        self.r.read_to_end(e.name())?;
                    }
                    // e.g. m:oMath: treat the whole subtree as one inline object.
                    _ => {
                        let h = self.hash_elem(&e, false, false)?;
                        b.object(h, 0);
                    }
                },
                Event::End(e) if e.name().as_ref() == b"w:p" => return Ok(b.finish()),
                Event::Eof => return Err(eof("w:p")),
                _ => {}
            }
        }
    }

    fn run(&mut self, b: &mut ParaBuilder) -> Result<()> {
        let mut fmt = 0;
        loop {
            match self.r.read_event()? {
                Event::Start(e) => match e.name().as_ref() {
                    b"w:rPr" => fmt = self.hash_elem(&e, false, true)?,
                    b"w:t" => {
                        let s = self.text(b"w:t")?;
                        b.text(&s, fmt);
                    }
                    b"w:instrText" => {
                        let s = self.text(b"w:instrText")?;
                        b.text(&s, fmt ^ FIELD_CODE);
                    }
                    n if is(n, IGNORED) || n == b"w:delText" || n == b"w:delInstrText" => {
                        self.r.read_to_end(e.name())?;
                    }
                    // Drawings, VML, embedded objects, and anything unrecognized.
                    _ => {
                        let h = self.hash_elem(&e, false, false)?;
                        b.object(h, fmt);
                    }
                },
                Event::Empty(e) => match e.name().as_ref() {
                    b"w:rPr" => fmt = 0,
                    b"w:tab" | b"w:ptab" => b.text("\t", fmt),
                    b"w:br" | b"w:cr" if e.attributes().next().is_none() => b.text("\n", fmt),
                    // Page/column breaks, symbols.
                    b"w:br" | b"w:sym" => {
                        let h = self.hash_elem(&e, true, false)?;
                        b.object(h, fmt);
                    }
                    b"w:noBreakHyphen" => b.text("\u{2011}", fmt),
                    b"w:softHyphen" => b.text("\u{00AD}", fmt),
                    // Note IDs differ between files; identify by kind only for now.
                    n @ (b"w:footnoteReference" | b"w:endnoteReference") => b.object(hash_bytes(n), fmt),
                    _ => {}
                },
                Event::End(e) if e.name().as_ref() == b"w:r" => return Ok(()),
                Event::Eof => return Err(eof("w:r")),
                _ => {}
            }
        }
    }

    fn text(&mut self, end: &[u8]) -> Result<String> {
        let mut s = String::new();
        loop {
            match self.r.read_event()? {
                Event::Text(t) => s.push_str(&t.unescape()?),
                Event::CData(t) => s.push_str(&String::from_utf8_lossy(&t)),
                Event::End(e) if e.name().as_ref() == end => return Ok(s),
                Event::Eof => return Err(eof("text")),
                _ => {}
            }
        }
    }

    /// Hash an element's normalized subtree. In `props` mode, revision history
    /// and proofing noise are excluded, and an element with no remaining
    /// content hashes to 0 so that `<w:rPr/>` equals no `w:rPr` at all.
    fn hash_elem(&mut self, start: &BytesStart, empty: bool, props: bool) -> Result<u64> {
        let mut h = H::new();
        let mut items = self.hash_attrs(&mut h, start);
        if !empty {
            let mut depth = 0usize;
            loop {
                match self.r.read_event()? {
                    Event::Start(e) => {
                        let name = e.name();
                        if is(name.as_ref(), IGNORED) || (props && is(name.as_ref(), PROP_NOISE)) {
                            self.r.read_to_end(name)?;
                            continue;
                        }
                        depth += 1;
                        items += 1;
                        h.u64(1);
                        h.bytes(name.as_ref());
                        items += self.hash_attrs(&mut h, &e);
                    }
                    Event::Empty(e) => {
                        let name = e.name();
                        if is(name.as_ref(), IGNORED) || (props && is(name.as_ref(), PROP_NOISE)) {
                            continue;
                        }
                        items += 1;
                        h.u64(1);
                        h.bytes(name.as_ref());
                        items += self.hash_attrs(&mut h, &e);
                        h.u64(2);
                    }
                    Event::End(_) => {
                        if depth == 0 {
                            break;
                        }
                        depth -= 1;
                        h.u64(2);
                    }
                    Event::Text(t) => {
                        let s = t.unescape()?;
                        if !s.trim().is_empty() {
                            items += 1;
                            h.u64(3);
                            h.bytes(s.as_bytes());
                        }
                    }
                    Event::CData(t) => {
                        items += 1;
                        h.u64(3);
                        h.bytes(&t);
                    }
                    Event::Eof => return Err(eof("element")),
                    _ => {}
                }
            }
        }
        Ok(if props && items == 0 { 0 } else { h.finish() })
    }

    /// Hash attributes in sorted order, skipping volatile IDs. Returns how many were hashed.
    fn hash_attrs(&self, h: &mut H, e: &BytesStart) -> usize {
        let elem = e.name();
        // Drawing object IDs and auto-generated names ("Picture 3") vary between saves.
        let volatile_id = matches!(elem.as_ref(), b"wp:docPr" | b"pic:cNvPr" | b"wps:cNvPr" | b"wpg:cNvPr");
        let mut attrs: Vec<(Vec<u8>, Cow<[u8]>)> = e
            .attributes()
            .filter_map(|a| a.ok())
            .filter(|a| {
                let k = a.key.as_ref();
                !(k.starts_with(b"w:rsid")
                    || k.ends_with(b":paraId")
                    || k.ends_with(b":textId")
                    || k.ends_with(b":anchorId")
                    || k.ends_with(b":editId")
                    || k == b"w:hint"
                    || (volatile_id && (k == b"id" || k == b"name")))
            })
            .map(|a| (a.key.as_ref().to_vec(), a.value))
            .collect();
        attrs.sort_unstable_by(|x, y| x.0.cmp(&y.0));
        for (k, v) in &attrs {
            h.bytes(k);
            if is(k, REL_ATTRS) {
                let target = std::str::from_utf8(v).ok().and_then(|id| self.rels.get(id));
                h.u64(target.copied().unwrap_or_else(|| hash_bytes(v)));
            } else {
                h.bytes(v);
            }
        }
        attrs.len()
    }
}

#[derive(Default)]
struct ParaBuilder {
    text: String,
    /// (byte offset where a formatting run starts, fmt hash)
    runs: Vec<(u32, u64)>,
    /// Content hash of each OBJ placeholder, in order.
    objects: Vec<u64>,
    ppr: u64,
}

impl ParaBuilder {
    fn set_fmt(&mut self, fmt: u64) {
        let at = self.text.len() as u32;
        match self.runs.last_mut() {
            Some(r) if r.1 == fmt => {}
            Some(r) if r.0 == at => r.1 = fmt,
            _ => self.runs.push((at, fmt)),
        }
    }

    fn text(&mut self, s: &str, fmt: u64) {
        if !s.is_empty() {
            self.set_fmt(fmt);
            self.text.push_str(s);
        }
    }

    fn object(&mut self, hash: u64, fmt: u64) {
        self.set_fmt(fmt);
        self.text.push(OBJ);
        self.objects.push(hash);
    }

    fn finish(self) -> Para {
        let text = self.text;
        let mut tokens = Vec::new();
        let mut objects = self.objects.into_iter();
        let mut chars = text.char_indices().peekable();
        while let Some((s, c)) = chars.next() {
            let kind = if c == OBJ {
                TokenKind::Object
            } else if c.is_whitespace() {
                while chars.next_if(|&(_, c)| c.is_whitespace()).is_some() {}
                TokenKind::Space
            } else if is_cjk(c) {
                TokenKind::Word
            } else if is_word(c) {
                while chars.next_if(|&(_, c)| is_word(c) && !is_cjk(c)).is_some() {}
                TokenKind::Word
            } else {
                TokenKind::Punct
            };
            let e = chars.peek().map_or(text.len(), |&(i, _)| i);
            let text_hash = match kind {
                TokenKind::Object => objects.next().unwrap_or(0),
                _ => hash_bytes(&text.as_bytes()[s..e]),
            };
            tokens.push(Token { start: s as u32, end: e as u32, kind, text_hash, fmt: 0 });
        }
        assign_fmt(&mut tokens, &self.runs);
        Para::new(text, tokens, self.ppr)
    }
}

/// A token's fmt is its run's fmt, or, when formatting changes mid-token, a
/// hash of the run layout within the token.
fn assign_fmt(tokens: &mut [Token], runs: &[(u32, u64)]) {
    let mut ri = 0;
    for t in tokens {
        while ri + 1 < runs.len() && runs[ri + 1].0 <= t.start {
            ri += 1;
        }
        if ri + 1 < runs.len() && runs[ri + 1].0 < t.end {
            let mut h = H::new();
            h.u64(runs[ri].1);
            let mut k = ri + 1;
            while k < runs.len() && runs[k].0 < t.end {
                h.u64((runs[k].0 - t.start) as u64);
                h.u64(runs[k].1);
                k += 1;
            }
            t.fmt = h.finish();
        } else {
            t.fmt = runs[ri].1;
        }
    }
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x3040..=0x30FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xAC00..=0xD7AF | 0xF900..=0xFAFF)
}
