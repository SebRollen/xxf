//! Streaming parser from `word/document.xml` to the normalized model.
//!
//! Normalization, applied while parsing:
//! - existing tracked changes are resolved: accepted by default
//!   (`w:del`/`w:moveFrom` dropped, `w:ins`/`w:moveTo` unwrapped), or rejected
//!   with `Revisions::Reject`;
//! - run boundaries disappear: text is concatenated per paragraph and
//!   formatting is tracked per character range;
//! - revision IDs (`w:rsid*`), proofing marks, bookmarks, comment anchors and
//!   other bookkeeping are ignored;
//! - run and paragraph formatting is resolved through styles to effective
//!   properties (see `styles`);
//! - relationship IDs (`r:embed`, `r:id`, ...) are replaced by a hash of what
//!   they point to, since the IDs differ between files.
//!
//! Element names are matched by qualified name with Word's conventional `w:`
//! prefix. Source byte offsets are recorded for the redline writer.

use std::borrow::Cow;

use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};
use rustc_hash::FxHashMap;

use crate::Result;
use crate::hash::{H, hash_bytes};
use crate::model::*;
use crate::styles::{Resolver, Styles};

/// Relationship ID → hash of the target's content (or URL for external targets).
pub type Rels = FxHashMap<String, u64>;

/// How to read tracked changes already present in a document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Revisions {
    /// The document as it reads with all changes accepted.
    Accept,
    /// The document as it reads with all changes rejected.
    Reject,
}

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
    b"w:sdt",
    b"w:sdtContent",
    b"w:customXml",
    b"w:smartTag",
    b"w:hyperlink",
    b"w:fldSimple",
    b"w:dir",
    b"w:bdo",
];

/// Subtrees dropped entirely: wrapper metadata.
const SKIPPED: &[&[u8]] = &[b"w:sdtPr", b"w:sdtEndPr", b"w:customXmlPr", b"w:smartTagPr"];

/// Revision wrappers kept (unwrapped) or dropped, by mode.
const INSERTED: &[&[u8]] = &[b"w:ins", b"w:moveTo"];
const DELETED: &[&[u8]] = &[b"w:del", b"w:moveFrom"];

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

pub fn parse_document(xml: &str, rels: &Rels, styles: &Styles) -> Result<Document> {
    parse_document_with(xml, rels, styles, Revisions::Accept)
}

pub fn parse_document_with(xml: &str, rels: &Rels, styles: &Styles, mode: Revisions) -> Result<Document> {
    let mut p = Parser { r: Reader::from_str(xml), xml, rels, res: Resolver::new(styles), mode };
    loop {
        match p.r.read_event()? {
            Event::Start(e) if e.name().as_ref() == b"w:body" => {
                let start = p.pos();
                let (blocks, end) = p.blocks(b"w:body", &mut None)?;
                return Ok(Document { blocks, body: start..end });
            }
            Event::Empty(e) if e.name().as_ref() == b"w:body" => return Ok(Document::default()),
            Event::Eof => return Err("document.xml has no w:body".into()),
            _ => {}
        }
    }
}

struct Parser<'a> {
    r: Reader<&'a [u8]>,
    xml: &'a str,
    rels: &'a Rels,
    res: Resolver<'a>,
    mode: Revisions,
}

fn is(name: &[u8], set: &[&[u8]]) -> bool {
    set.contains(&name)
}

fn eof(ctx: &str) -> crate::Error {
    format!("unexpected end of document.xml inside {ctx}").into()
}

/// The body of a `*Change` element's property child, e.g. the old run
/// properties inside `<w:rPrChange><w:rPr>...</w:rPr></w:rPrChange>`.
fn change_inner<'x>(raw: &'x str, change: &str, prop: &str) -> Option<&'x str> {
    let c = raw.find(&format!("<{change}"))?;
    let rest = &raw[c..];
    let open = format!("<{prop}>");
    match rest.find(&open) {
        Some(o) => {
            let body = &rest[o + open.len()..];
            Some(&body[..body.find(&format!("</{prop}>"))?])
        }
        None => Some(""),
    }
}

impl<'a> Parser<'a> {
    /// Current offset: the end of the last event read.
    fn pos(&self) -> u32 {
        self.r.buffer_position() as u32
    }

    /// Next event and the offset where it starts.
    fn next(&mut self) -> Result<(u32, Event<'a>)> {
        let start = self.pos();
        Ok((start, self.r.read_event()?))
    }

    /// Skip to the end of the element just opened, returning its raw inner XML.
    fn inner(&mut self, e: &BytesStart) -> Result<&'a str> {
        let span = self.r.read_to_end(e.name())?;
        Ok(&self.xml[span.start as usize..span.end as usize])
    }

    fn transparent(&self, n: &[u8]) -> bool {
        is(n, TRANSPARENT) || is(n, if self.mode == Revisions::Accept { INSERTED } else { DELETED })
    }

    fn skipped(&self, n: &[u8]) -> bool {
        is(n, SKIPPED) || is(n, IGNORED) || is(n, if self.mode == Revisions::Accept { DELETED } else { INSERTED })
    }

    /// Whether a paragraph-mark or row revision marker in `props` removes the
    /// paragraph/row in the current mode.
    fn removed_by_mark(&self, props: &str) -> bool {
        let tags: &[&str] = match self.mode {
            Revisions::Accept => &["<w:del ", "<w:moveFrom "],
            Revisions::Reject => &["<w:ins ", "<w:moveTo "],
        };
        tags.iter().any(|t| props.contains(t))
    }

    /// Block-level content until `</end>`; returns the blocks and the offset of
    /// the end tag. `tcpr` receives a `w:tcPr` (hash, span).
    fn blocks(&mut self, end: &[u8], tcpr: &mut Option<(u64, Src)>) -> Result<(Vec<Block>, u32)> {
        let mut out = Vec::new();
        loop {
            match self.next()? {
                (start, Event::Start(e)) => match e.name().as_ref() {
                    b"w:p" => {
                        if let Some(p) = self.para(start)? {
                            out.push(Block::Para(p));
                        }
                    }
                    b"w:tbl" => {
                        let t = self.table(start)?;
                        if !t.rows.is_empty() {
                            out.push(Block::Table(t));
                        }
                    }
                    b"w:tcPr" => {
                        let h = self.hash_elem(&e, false, true)?;
                        *tcpr = Some((h, start..self.pos()));
                    }
                    n if self.transparent(n) => {}
                    n if self.skipped(n) => {
                        self.r.read_to_end(e.name())?;
                    }
                    n => {
                        let name = String::from_utf8_lossy(n).into_owned();
                        let props = n.ends_with(b"Pr");
                        let h = self.hash_elem(&e, false, props)?;
                        let mut o = Opaque::new(name, h);
                        o.span = start..self.pos();
                        out.push(Block::Opaque(o));
                    }
                },
                (start, Event::Empty(e)) => match e.name().as_ref() {
                    b"w:p" => {
                        let (ppr, style) = self.res.ppr("")?;
                        let mut p = Para::new(String::new(), Vec::new(), ppr, style);
                        let end = self.pos();
                        p.src = ParaSrc { span: start..end, stag_end: end, empty_elem: true, ..Default::default() };
                        out.push(Block::Para(p));
                    }
                    b"w:tcPr" => *tcpr = Some((0, start..self.pos())),
                    _ => {}
                },
                (start, Event::End(e)) if e.name().as_ref() == end => return Ok((out, start)),
                (_, Event::Eof) => return Err(eof("block content")),
                _ => {}
            }
        }
    }

    fn table(&mut self, start: u32) -> Result<Table> {
        let mut props = H::new();
        let mut rows = Vec::new();
        loop {
            match self.next()? {
                (s, Event::Start(e)) => match e.name().as_ref() {
                    b"w:tblPr" | b"w:tblGrid" => props.u64(self.hash_elem(&e, false, true)?),
                    b"w:tr" => {
                        if let Some(r) = self.row(s)? {
                            rows.push(r);
                        }
                    }
                    n if is(n, TRANSPARENT) => {}
                    _ => {
                        self.r.read_to_end(e.name())?;
                    }
                },
                (_, Event::End(e)) if e.name().as_ref() == b"w:tbl" => {
                    let mut t = Table::new(rows, props.finish());
                    t.span = start..self.pos();
                    return Ok(t);
                }
                (_, Event::Eof) => return Err(eof("w:tbl")),
                _ => {}
            }
        }
    }

    /// A table row, or `None` if a row-level revision removes it in this mode.
    fn row(&mut self, start: u32) -> Result<Option<Row>> {
        let mut props = H::new();
        let mut cells = Vec::new();
        let mut trpr_at = self.pos();
        let mut trpr = None;
        loop {
            match self.next()? {
                (s, Event::Start(e)) => match e.name().as_ref() {
                    b"w:trPr" | b"w:tblPrEx" => {
                        props.u64(self.hash_elem(&e, false, true)?);
                        if e.name().as_ref() == b"w:trPr" {
                            trpr = Some(s..self.pos());
                        } else {
                            trpr_at = self.pos();
                        }
                    }
                    b"w:tc" => {
                        let stag_end = self.pos();
                        let mut tcpr = None;
                        let (blocks, end) = self.blocks(b"w:tc", &mut tcpr)?;
                        let mut c = Cell::new(blocks, tcpr.as_ref().map_or(0, |t| t.0));
                        c.span = s..self.pos();
                        c.content = stag_end..end;
                        c.tcpr = tcpr.map(|t| t.1);
                        cells.push(c);
                    }
                    n if is(n, TRANSPARENT) => {}
                    _ => {
                        self.r.read_to_end(e.name())?;
                    }
                },
                (s, Event::Empty(e)) if e.name().as_ref() == b"w:trPr" => trpr = Some(s..self.pos()),
                (_, Event::End(e)) if e.name().as_ref() == b"w:tr" => {
                    if let Some(t) = &trpr
                        && self.removed_by_mark(&self.xml[t.start as usize..t.end as usize])
                    {
                        return Ok(None);
                    }
                    let mut r = Row::new(cells, props.finish());
                    r.span = start..self.pos();
                    r.trpr_at = trpr_at;
                    r.trpr = trpr;
                    return Ok(Some(r));
                }
                (_, Event::Eof) => return Err(eof("w:tr")),
                _ => {}
            }
        }
    }

    /// A paragraph, or `None` if its mark is removed in this mode and nothing
    /// remains of it. (A removed mark on a non-empty paragraph would merge it
    /// into the next one; that is not modeled, the paragraph is kept.)
    fn para(&mut self, start: u32) -> Result<Option<Para>> {
        let mut b = ParaBuilder::default();
        (b.ppr, b.style) = self.res.ppr("")?;
        let stag_end = self.pos();
        let mut ppr_span = None;
        let mut mark_removed = false;
        loop {
            match self.next()? {
                (s, Event::Start(e)) => match e.name().as_ref() {
                    b"w:pPr" => {
                        let mut raw = self.inner(&e)?;
                        ppr_span = Some(s..self.pos());
                        mark_removed = self.removed_by_mark(raw);
                        if self.mode == Revisions::Reject
                            && let Some(old) = change_inner(raw, "w:pPrChange", "w:pPr")
                        {
                            raw = old;
                        }
                        (b.ppr, b.style) = self.res.ppr(raw)?;
                    }
                    b"w:r" => self.run(s, &mut b)?,
                    n if self.transparent(n) => {}
                    n if self.skipped(n) => {
                        self.r.read_to_end(e.name())?;
                    }
                    // e.g. m:oMath: treat the whole subtree as one inline object.
                    _ => {
                        let h = self.hash_elem(&e, false, false)?;
                        b.object(h, 0);
                    }
                },
                (s, Event::Empty(e)) if e.name().as_ref() == b"w:pPr" => ppr_span = Some(s..self.pos()),
                (_, Event::End(e)) if e.name().as_ref() == b"w:p" => {
                    if mark_removed && b.text.is_empty() {
                        return Ok(None);
                    }
                    let src = ParaSrc {
                        span: start..self.pos(),
                        stag_end,
                        ppr: ppr_span,
                        empty_elem: false,
                        runs: std::mem::take(&mut b.src_runs),
                    };
                    let mut p = b.finish();
                    p.src = src;
                    return Ok(Some(p));
                }
                (_, Event::Eof) => return Err(eof("w:p")),
                _ => {}
            }
        }
    }

    fn run(&mut self, start: u32, b: &mut ParaBuilder) -> Result<()> {
        let mut fmt = self.res.rpr(b.style.as_deref(), "")?;
        let mut src = RunSrc { span: start..start, rpr: None, children: Vec::new() };
        let reject = self.mode == Revisions::Reject;
        loop {
            let (s, ev) = self.next()?;
            let at = b.text.len() as u32;
            let kind = match ev {
                Event::Start(e) => match e.name().as_ref() {
                    b"w:rPr" => {
                        let span = self.r.read_to_end(e.name())?;
                        src.rpr = Some(span.start as u32..span.end as u32);
                        let mut raw = &self.xml[span.start as usize..span.end as usize];
                        if reject && let Some(old) = change_inner(raw, "w:rPrChange", "w:rPr") {
                            raw = old;
                        }
                        fmt = self.res.rpr(b.style.as_deref(), raw)?;
                        continue;
                    }
                    n @ (b"w:t" | b"w:delText") if n == b"w:t" || reject => {
                        let t = self.text(n)?;
                        b.text(&t, fmt);
                        ChildKind::Text
                    }
                    n @ (b"w:instrText" | b"w:delInstrText") if n == b"w:instrText" || reject => {
                        let t = self.text(n)?;
                        b.text(&t, fmt ^ FIELD_CODE);
                        ChildKind::Instr
                    }
                    n if is(n, IGNORED) => {
                        self.r.read_to_end(e.name())?;
                        ChildKind::Marker
                    }
                    b"w:t" | b"w:delText" | b"w:instrText" | b"w:delInstrText" => {
                        self.r.read_to_end(e.name())?;
                        continue;
                    }
                    // Drawings, VML, embedded objects, and anything unrecognized.
                    _ => {
                        let h = self.hash_elem(&e, false, false)?;
                        b.object(h, fmt);
                        ChildKind::Raw
                    }
                },
                Event::Empty(e) => match e.name().as_ref() {
                    b"w:rPr" => continue,
                    b"w:tab" | b"w:ptab" => {
                        b.text("\t", fmt);
                        ChildKind::Raw
                    }
                    b"w:br" | b"w:cr" if e.attributes().next().is_none() => {
                        b.text("\n", fmt);
                        ChildKind::Raw
                    }
                    // Page/column breaks, symbols.
                    b"w:br" | b"w:sym" => {
                        let h = self.hash_elem(&e, true, false)?;
                        b.object(h, fmt);
                        ChildKind::Raw
                    }
                    b"w:noBreakHyphen" => {
                        b.text("\u{2011}", fmt);
                        ChildKind::Raw
                    }
                    b"w:softHyphen" => {
                        b.text("\u{00AD}", fmt);
                        ChildKind::Raw
                    }
                    // Note IDs differ between files; identify by kind only for now.
                    n @ (b"w:footnoteReference" | b"w:endnoteReference") => {
                        b.object(hash_bytes(n), fmt);
                        ChildKind::Raw
                    }
                    b"w:t" | b"w:delText" => continue,
                    _ => ChildKind::Marker,
                },
                Event::End(e) if e.name().as_ref() == b"w:r" => {
                    src.span.end = self.pos();
                    b.src_runs.push(src);
                    return Ok(());
                }
                Event::Eof => return Err(eof("w:r")),
                _ => continue,
            };
            src.children.push(RunChild { span: s..self.pos(), text: at..b.text.len() as u32, kind });
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
    style: Option<String>,
    src_runs: Vec<RunSrc>,
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
        Para::new(text, tokens, self.ppr, self.style)
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
