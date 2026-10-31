//! Writing a redline: the new document with tracked changes that, when all
//! rejected, give back the old document's text.
//!
//! Unchanged XML is copied verbatim from the new document, so everything the
//! diff doesn't model (bookmarks, fields, hyperlinks, content controls,
//! comment anchors) survives in place. Changed paragraphs are rebuilt run by
//! run: kept text keeps its new run properties, inserted text is wrapped in
//! `w:ins`, deleted text is rebuilt from the old document's runs inside
//! `w:del`, and formatting changes get a `w:rPrChange` with the old run
//! properties. Unmodified moved paragraphs become `w:moveFrom`/`w:moveTo`.
//!
//! Old content brought into the new package has its relationship IDs (images,
//! hyperlinks) and list IDs remapped; parts it needs are copied over.
//!
//! Not yet written as revisions: table/row/cell property changes, section
//! property changes, paragraph-level objects (math) inside edits.

use std::borrow::Cow;
use std::fmt::Write as _;
use std::io::{Cursor, Write};
use std::ops::Range;
use std::time::{SystemTime, UNIX_EPOCH};

use quick_xml::Reader;
use quick_xml::events::Event;
use rustc_hash::{FxHashMap, FxHashSet};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use crate::Result;
use crate::align::Step;
use crate::compare::{BlockItem, CellDiff, Detail, ParaDiff, RowDetail, SpanKind, TableDiff};
use crate::docx::{self, CONTENT_TYPES, Rel, Source};
use crate::hash::hash_bytes;
use crate::model::*;

pub struct Options {
    pub author: String,
    /// ISO 8601, e.g. `2026-10-30T21:00:00Z`.
    pub date: String,
}

/// Write a .docx: the new package, its `document.xml` replaced by a redline.
pub fn write(old: &Source, new: &Source, items: &[BlockItem], opts: &Options) -> Result<Vec<u8>> {
    let mut pkg = Package::new(old, new)?;
    let xml = {
        let mut e = Emitter::new(old, new, opts, &mut pkg);
        let body = new.doc.body.clone();
        e.out.push_str(&with_namespaces(&new.xml[..body.start as usize], &old.xml));
        e.blocks(&old.doc.blocks, &new.doc.blocks, items, body.clone());
        e.out.push_str(&new.xml[body.end as usize..]);
        e.out
    };
    pkg.write(&xml)
}

/// The new document's prolog (up to the body content) with any namespace
/// declarations from the old root element that it lacks: old content copied
/// into the redline may use prefixes the new document never declared.
fn with_namespaces(prolog: &str, old_xml: &str) -> String {
    let root = |x: &str| -> Option<(usize, usize)> {
        let start = x.find("<w:document")?;
        Some((start, start + x[start..].find('>')?))
    };
    let (Some((ns, ne)), Some((os, oe))) = (root(prolog), root(old_xml)) else { return prolog.to_string() };
    // Declarations on a root start tag, read with the XML parser so any
    // whitespace or quoting is handled.
    let declared = |tag: &str| -> Vec<(String, String)> {
        let mut r = Reader::from_str(tag);
        match r.read_event() {
            Ok(Event::Start(e) | Event::Empty(e)) => e
                .attributes()
                .filter_map(|a| a.ok())
                .filter_map(|a| {
                    let prefix = a.key.as_ref().strip_prefix(b"xmlns:")?;
                    Some((String::from_utf8_lossy(prefix).into_owned(), String::from_utf8_lossy(&a.value).into_owned()))
                })
                .collect(),
            _ => Vec::new(),
        }
    };
    let have: Vec<String> = declared(&prolog[ns..=ne]).into_iter().map(|d| d.0).collect();
    let missing: String = declared(&old_xml[os..=oe])
        .into_iter()
        .filter(|(p, _)| !have.contains(p))
        .map(|(p, uri)| format!(r#" xmlns:{p}="{uri}""#))
        .collect();
    if missing.is_empty() {
        return prolog.to_string();
    }
    // Insert before the end of the root start tag (before any "/" of "/>").
    let at = if prolog[..ne].ends_with('/') { ne - 1 } else { ne };
    format!("{}{missing}{}", &prolog[..at], &prolog[at..])
}

/// Check that `xml` is well-formed and namespace-correct: properly nested,
/// no duplicate attributes, every element and attribute prefix declared.
/// (The redline tests and corpus harness run every output through this.)
pub fn check_xml(xml: &str) -> Result<()> {
    use quick_xml::name::ResolveResult;
    let mut r = quick_xml::NsReader::from_str(xml);
    loop {
        let at = r.buffer_position();
        let (ns, ev) = r.read_resolved_event().map_err(|e| format!("at byte {at}: {e}"))?;
        match ev {
            Event::Start(e) | Event::Empty(e) => {
                if let ResolveResult::Unknown(p) = ns {
                    return Err(format!("at byte {at}: undeclared prefix {}", String::from_utf8_lossy(&p)).into());
                }
                for a in e.attributes() {
                    let a = a.map_err(|e| format!("at byte {at}: {e}"))?;
                    if let (ResolveResult::Unknown(p), _) = r.resolve_attribute(a.key) {
                        return Err(format!("at byte {at}: undeclared prefix {}", String::from_utf8_lossy(&p)).into());
                    }
                }
            }
            Event::Eof => return Ok(()),
            _ => {}
        }
    }
}

/// Current UTC time as ISO 8601.
pub fn now_iso() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let (days, rem) = ((secs / 86_400) as i64, secs % 86_400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// What happened to a stretch of the new paragraph's text.
#[derive(Debug, Clone, PartialEq)]
enum St {
    Keep,
    Ins,
    /// Same text, formatting changed; carries the old run's `w:rPr` content.
    Reformat(Option<Src>),
}

/// A stretch of new text (byte range into `Para::text`).
struct Seg {
    range: Range<u32>,
    st: St,
}

/// Old text deleted at a position in the new text.
struct Del {
    at: u32,
    old: Range<u32>,
}

/// Revision applied to a whole new paragraph's mark and text.
#[derive(Clone, Copy)]
enum Mark {
    Keep,
    Ins,
    MoveTo(u32),
}

struct Emitter<'a, 'p> {
    old: &'a Source,
    new: &'a Source,
    /// ` w:author=".." w:date=".."`
    rev: String,
    out: String,
    next_id: u32,
    next_move: u32,
    /// Old list ID → new list ID with the same definition.
    num_map: FxHashMap<String, String>,
    pkg: &'p mut Package<'a>,
}

/// Per-paragraph state while rebuilding runs.
struct Pieces<'s, 'a> {
    segs: &'s [Seg],
    dels: &'s [Del],
    di: usize,
    old: Option<&'a Para>,
    mark: Mark,
    /// The current run's `w:rPr` content and pending children.
    rpr: &'a str,
    group: Option<(St, String)>,
}

impl<'a, 'p> Emitter<'a, 'p> {
    fn new(old: &'a Source, new: &'a Source, opts: &Options, pkg: &'p mut Package<'a>) -> Self {
        let by_def: FxHashMap<&String, &String> = new.styles.numbering().iter().map(|(id, def)| (def, id)).collect();
        let num_map = old
            .styles
            .numbering()
            .iter()
            .filter_map(|(id, def)| by_def.get(def).map(|n| (id.clone(), (*n).clone())))
            .collect();
        Self {
            old,
            new,
            rev: format!(r#" w:author="{}" w:date="{}""#, esc_attr(&opts.author), esc_attr(&opts.date)),
            out: String::with_capacity(new.xml.len() + new.xml.len() / 4),
            // Revision IDs share a space with bookmarks and comments; start high.
            next_id: 900_000,
            next_move: 1,
            num_map,
            pkg,
        }
    }

    fn id(&mut self) -> u32 {
        self.next_id += 1;
        self.next_id
    }

    /// `w:id=".." w:author=".." w:date=".."` for a new revision.
    fn attrs(&mut self) -> String {
        let id = self.id();
        format!(r#"w:id="{id}"{}"#, self.rev)
    }

    fn copy(&mut self, r: Range<u32>) {
        let x: &'a str = &self.new.xml;
        self.out.push_str(&x[r.start as usize..r.end as usize]);
    }

    fn new_str(&self, r: &Src) -> &'a str {
        let x: &'a str = &self.new.xml;
        &x[r.start as usize..r.end as usize]
    }

    fn old_str(&self, r: &Src) -> &'a str {
        let x: &'a str = &self.old.xml;
        &x[r.start as usize..r.end as usize]
    }

    /// Emit a block list. `content` is the new XML range holding the new
    /// blocks; bytes between blocks (wrappers, bookmarks) are copied as is.
    fn blocks(&mut self, old: &'a [Block], new: &'a [Block], items: &[BlockItem], content: Src) {
        // Moves are written as moves only for unmodified paragraphs.
        let mut moves: FxHashMap<(usize, usize), u32> = FxHashMap::default();
        for it in items {
            if let Step::MovedIn(i, j) = it.step
                && it.detail.is_none()
                && matches!(old[i], Block::Para(_))
            {
                moves.insert((i, j), self.next_move);
                self.next_move += 1;
            }
        }
        let mut cur = content.start;
        for it in items {
            match it.step {
                Step::Equal(_, j) => {
                    let s = new[j].span();
                    self.copy(cur..s.end);
                    cur = s.end;
                }
                Step::Changed(_, j) | Step::Inserted(j) | Step::MovedIn(_, j) => {
                    let s = new[j].span();
                    self.copy(cur..s.start);
                    cur = s.end;
                    match it.step {
                        Step::Changed(i, _) => self.changed(&old[i], &new[j], it.detail.as_ref()),
                        Step::MovedIn(i, _) => self.inserted(&new[j], moves.get(&(i, j)).copied()),
                        _ => self.inserted(&new[j], None),
                    }
                }
                Step::Deleted(i) => self.deleted(&old[i], None),
                Step::MovedOut(i, j) => self.deleted(&old[i], moves.get(&(i, j)).copied()),
            }
        }
        self.copy(cur..content.end);
    }

    fn changed(&mut self, a: &'a Block, b: &'a Block, detail: Option<&Detail>) {
        match (a, b, detail) {
            (Block::Para(a), Block::Para(b), Some(Detail::Para(d))) => self.changed_para(a, b, d),
            (Block::Table(a), Block::Table(b), Some(Detail::Table(d))) => self.changed_table(a, b, d),
            _ => self.copy(b.span()),
        }
    }

    fn inserted(&mut self, b: &'a Block, mv: Option<u32>) {
        match b {
            Block::Para(p) => {
                let segs = [Seg { range: 0..p.text.len() as u32, st: St::Ins }];
                let mark = mv.map_or(Mark::Ins, Mark::MoveTo);
                self.para(p, &segs, &[], None, mark, None);
            }
            Block::Table(t) => self.inserted_table(t),
            Block::Opaque(o) => self.copy(o.span.clone()),
        }
    }

    fn deleted(&mut self, b: &'a Block, mv: Option<u32>) {
        match b {
            Block::Para(p) => self.deleted_para(p, mv),
            Block::Table(t) => self.deleted_table(t),
            // Can't be marked as deleted; it's simply absent from the redline.
            Block::Opaque(_) => {}
        }
    }

    fn inserted_blocks(&mut self, blocks: &'a [Block], content: Src) {
        let mut cur = content.start;
        for b in blocks {
            let s = b.span();
            self.copy(cur..s.start);
            self.inserted(b, None);
            cur = s.end;
        }
        self.copy(cur..content.end);
    }

    // ---- paragraphs ----

    fn changed_para(&mut self, a: &'a Para, b: &'a Para, d: &ParaDiff) {
        let mut segs = Vec::new();
        let mut dels = Vec::new();
        for sp in &d.spans {
            let nr = token_bytes(b, &sp.new);
            match sp.kind {
                SpanKind::Same => segs.push(Seg { range: nr, st: St::Keep }),
                SpanKind::Reformat => {
                    for (k, t) in b.tokens[sp.new.clone()].iter().enumerate() {
                        let old_at = a.tokens[sp.old.start + k].start;
                        segs.push(Seg { range: t.start..t.end, st: St::Reformat(rpr_at(a, old_at)) });
                    }
                }
                SpanKind::Replace => {
                    let or = token_bytes(a, &sp.old);
                    if !or.is_empty() {
                        dels.push(Del { at: nr.start, old: or });
                    }
                    if !nr.is_empty() {
                        segs.push(Seg { range: nr, st: St::Ins });
                    }
                }
            }
        }
        self.para(b, &segs, &dels, Some(a), Mark::Keep, d.ppr_changed.then_some(a));
    }

    /// Emit new paragraph `p` with its text classified by `segs`, old text
    /// from `old` deleted at `dels`, and optionally a paragraph-mark revision
    /// and the old paragraph properties.
    fn para(&mut self, p: &'a Para, segs: &[Seg], dels: &[Del], old: Option<&'a Para>, mark: Mark, ppr_old: Option<&'a Para>) {
        let s = &p.src;
        if s.empty_elem {
            let tag = self.new_str(&s.span);
            self.out.push_str(tag.trim_end_matches("/>").trim_end());
            self.out.push('>');
        } else {
            self.copy(s.span.start..s.stag_end);
        }

        let mark_xml = match mark {
            Mark::Keep => None,
            Mark::Ins => Some(format!("<w:ins {}/>", self.attrs())),
            Mark::MoveTo(_) => Some(format!("<w:moveTo {}/>", self.attrs())),
        };
        let change_xml = ppr_old.map(|o| {
            let inner = self.old_ppr(o);
            format!("<w:pPrChange {}><w:pPr>{inner}</w:pPr></w:pPrChange>", self.attrs())
        });
        let mut cur = s.stag_end;
        if let Some(r) = &s.ppr {
            cur = r.end;
        }
        if mark_xml.is_some() || change_xml.is_some() {
            let mut inner = s.ppr.as_ref().map_or("", |r| inner_of(self.new_str(r))).to_string();
            if let Some(m) = mark_xml {
                inner = add_para_mark(&inner, &m);
            }
            if let Some(c) = change_xml
                && !inner.contains("<w:pPrChange")
            {
                inner.push_str(&c);
            }
            let _ = write!(self.out, "<w:pPr>{inner}</w:pPr>");
        } else if let Some(r) = &s.ppr {
            self.copy(r.clone());
        }

        let move_range = match mark {
            Mark::MoveTo(n) => {
                let id = self.id();
                let _ = write!(self.out, r#"<w:moveToRangeStart w:id="{id}" w:name="move{n}"{}/>"#, self.rev);
                Some(id)
            }
            _ => None,
        };

        let mut pc = Pieces { segs, dels, di: 0, old, mark, rpr: "", group: None };
        for run in &s.runs {
            self.copy(cur..run.span.start);
            cur = run.span.end;
            self.run(p, run, &mut pc);
        }
        while pc.di < dels.len() {
            if let Some(o) = old {
                self.del_runs(o, dels[pc.di].old.clone(), false);
            }
            pc.di += 1;
        }
        if let Some(id) = move_range {
            let _ = write!(self.out, r#"<w:moveToRangeEnd w:id="{id}"/>"#);
        }
        if s.empty_elem {
            self.out.push_str("</w:p>");
        } else {
            self.copy(cur..s.span.end);
        }
    }

    /// Rebuild one new run, split where its text changes status.
    fn run(&mut self, p: &'a Para, run: &'a RunSrc, pc: &mut Pieces<'_, 'a>) {
        if run.bare {
            // A paragraph-level object can't go in a w:r or w:ins; an
            // inserted equation is marked inside its m:oMath instead.
            let ch = &run.children[0];
            self.flush(pc);
            self.due_deletions(pc, ch.text.start);
            let raw = self.new_str(&ch.span);
            if status_at(pc.segs, ch.text.start) == St::Ins {
                let tag = if matches!(pc.mark, Mark::MoveTo(_)) { "w:moveTo" } else { "w:ins" };
                let attrs = self.attrs();
                self.out.push_str(&wrap_math(raw, &format!("<{tag} {attrs}>"), &format!("</{tag}>")));
            } else {
                self.out.push_str(raw);
            }
            return;
        }
        pc.rpr = run.rpr.as_ref().map_or("", |r| self.new_str(r));
        for ch in &run.children {
            match ch.kind {
                ChildKind::Marker => {
                    let raw = self.new_str(&ch.span);
                    match &mut pc.group {
                        Some((_, buf)) => buf.push_str(raw),
                        None => pc.group = Some((St::Keep, raw.to_string())),
                    }
                }
                ChildKind::Raw => {
                    let st = status_at(pc.segs, ch.text.start);
                    self.piece(pc, st, self.new_str(&ch.span), ch.text.start);
                }
                ChildKind::Text | ChildKind::Instr => {
                    let tag = if ch.kind == ChildKind::Text { "w:t" } else { "w:instrText" };
                    let (mut a, end) = (ch.text.start, ch.text.end);
                    while a < end {
                        let si = pc.segs.partition_point(|s| s.range.end <= a);
                        let mut b = pc.segs.get(si).map_or(end, |s| s.range.end.min(end));
                        if let Some(d) = pc.dels[pc.di..].iter().find(|d| d.at > a) {
                            b = b.min(d.at);
                        }
                        let st = pc.segs.get(si).map_or(St::Keep, |s| s.st.clone());
                        let xml = format!(r#"<{tag} xml:space="preserve">{}</{tag}>"#, esc_text(&p.text[a as usize..b as usize]));
                        self.piece(pc, st, &xml, a);
                        a = b;
                    }
                }
            }
        }
        self.flush(pc);
    }

    /// Add content starting at text offset `at` with status `st` to the
    /// current output run, first emitting any deletions due at `at`.
    fn piece(&mut self, pc: &mut Pieces<'_, 'a>, st: St, xml: &str, at: u32) {
        if pc.di < pc.dels.len() && pc.dels[pc.di].at <= at {
            self.flush(pc);
            self.due_deletions(pc, at);
        }
        match &mut pc.group {
            Some((g, buf)) if *g == st => buf.push_str(xml),
            _ => {
                self.flush(pc);
                pc.group = Some((st, xml.to_string()));
            }
        }
    }

    /// Emit the deletions positioned at or before text offset `at`.
    fn due_deletions(&mut self, pc: &mut Pieces<'_, 'a>, at: u32) {
        while pc.di < pc.dels.len() && pc.dels[pc.di].at <= at {
            if let Some(o) = pc.old {
                self.del_runs(o, pc.dels[pc.di].old.clone(), false);
            }
            pc.di += 1;
        }
    }

    fn flush(&mut self, pc: &mut Pieces<'_, 'a>) {
        let Some((st, children)) = pc.group.take() else { return };
        let rpr = match &st {
            St::Reformat(old) if !pc.rpr.contains("<w:rPrChange") => {
                let old = old.as_ref().map(|r| self.old_rpr(r)).unwrap_or_default();
                format!("{}<w:rPrChange {}><w:rPr>{old}</w:rPr></w:rPrChange>", pc.rpr, self.attrs())
            }
            _ => pc.rpr.to_string(),
        };
        let run = if rpr.is_empty() {
            format!("<w:r>{children}</w:r>")
        } else {
            format!("<w:r><w:rPr>{rpr}</w:rPr>{children}</w:r>")
        };
        if st == St::Ins {
            let tag = if matches!(pc.mark, Mark::MoveTo(_)) { "w:moveTo" } else { "w:ins" };
            let attrs = self.attrs();
            let _ = write!(self.out, "<{tag} {attrs}>{run}</{tag}>");
        } else {
            self.out.push_str(&run);
        }
    }

    /// Old text `range` of paragraph `p` as deleted (or moved-from) runs.
    fn del_runs(&mut self, p: &'a Para, range: Range<u32>, moved: bool) {
        let (wrap, t, instr) =
            if moved { ("w:moveFrom", "w:t", "w:instrText") } else { ("w:del", "w:delText", "w:delInstrText") };
        let mut body = String::new();
        for run in &p.src.runs {
            if run.bare {
                // Equations are marked inside their m:oMath; other
                // paragraph-level objects can't be marked and are dropped.
                let ch = &run.children[0];
                let raw = self.old_str(&ch.span);
                if range.start <= ch.text.start && ch.text.end <= range.end && raw.contains("<m:oMath") {
                    self.close_wrapper(wrap, &mut body);
                    let attrs = self.attrs();
                    let raw = self.pkg.remap(&accept_revisions(raw));
                    self.out.push_str(&wrap_math(&raw, &format!("<{wrap} {attrs}>"), &format!("</{wrap}>")));
                }
                continue;
            }
            let mut children = String::new();
            for ch in &run.children {
                let (a, b) = (ch.text.start.max(range.start), ch.text.end.min(range.end));
                match ch.kind {
                    ChildKind::Text | ChildKind::Instr if a < b => {
                        let tag = if ch.kind == ChildKind::Text { t } else { instr };
                        let _ = write!(
                            children,
                            r#"<{tag} xml:space="preserve">{}</{tag}>"#,
                            esc_text(&p.text[a as usize..b as usize])
                        );
                    }
                    ChildKind::Raw if a < b && a == ch.text.start && b == ch.text.end => {
                        let raw = self.old_str(&ch.span);
                        if raw.starts_with("<w:footnoteReference") || raw.starts_with("<w:endnoteReference") {
                            // The note itself is carried into the new notes part;
                            // if it can't be, the reference is dropped.
                            if let Some(r) = self.pkg.note_ref(raw) {
                                children.push_str(&r);
                            }
                        } else {
                            children.push_str(&self.pkg.remap(&accept_revisions(raw)));
                        }
                    }
                    _ => {}
                }
            }
            if !children.is_empty() {
                let rpr = run.rpr.as_ref().map(|r| self.old_rpr(r)).unwrap_or_default();
                if rpr.is_empty() {
                    let _ = write!(body, "<w:r>{children}</w:r>");
                } else {
                    let _ = write!(body, "<w:r><w:rPr>{rpr}</w:rPr>{children}</w:r>");
                }
            }
        }
        self.close_wrapper(wrap, &mut body);
    }

    /// Emit pending deleted runs inside one revision wrapper.
    fn close_wrapper(&mut self, wrap: &str, body: &mut String) {
        if !body.is_empty() {
            let attrs = self.attrs();
            let _ = write!(self.out, "<{wrap} {attrs}>{body}</{wrap}>");
            body.clear();
        }
    }

    fn deleted_para(&mut self, p: &'a Para, mv: Option<u32>) {
        let ppr = self.old_ppr(p);
        let mark = match mv {
            Some(_) => format!("<w:moveFrom {}/>", self.attrs()),
            None => format!("<w:del {}/>", self.attrs()),
        };
        let _ = write!(self.out, "<w:p><w:pPr>{ppr}<w:rPr>{mark}</w:rPr></w:pPr>");
        let range_id = mv.map(|n| {
            let id = self.id();
            let _ = write!(self.out, r#"<w:moveFromRangeStart w:id="{id}" w:name="move{n}"{}/>"#, self.rev);
            id
        });
        self.del_runs(p, 0..p.text.len() as u32, mv.is_some());
        if let Some(id) = range_id {
            let _ = write!(self.out, r#"<w:moveFromRangeEnd w:id="{id}"/>"#);
        }
        self.out.push_str("</w:p>");
    }

    /// Old paragraph properties, made safe to embed in the new document:
    /// no mark run properties, section properties or revision history; list
    /// and relationship IDs remapped.
    fn old_ppr(&mut self, p: &Para) -> String {
        let Some(r) = &p.src.ppr else { return String::new() };
        let mut s = inner_of(self.old_str(r)).to_string();
        for name in ["w:pPrChange", "w:rPr", "w:sectPr"] {
            s = remove_elem(&s, name);
        }
        s = map_attr(&s, "<w:numId ", "w:val", |v| self.num_map.get(v).cloned());
        self.pkg.remap(&s)
    }

    fn old_rpr(&mut self, r: &Src) -> String {
        let s = remove_elem(self.old_str(r), "w:rPrChange");
        self.pkg.remap(&s)
    }

    // ---- tables ----

    fn changed_table(&mut self, a: &'a Table, b: &'a Table, d: &TableDiff) {
        // Table properties and grid first: deleted rows must come after them.
        let head_end = b.rows.first().map_or(b.span.start, |r| r.span.start);
        self.copy(b.span.start..head_end);
        let mut cur = head_end;
        for it in &d.rows {
            match it.step {
                Step::Equal(_, j) => {
                    let r = &b.rows[j];
                    self.copy(cur..r.span.end);
                    cur = r.span.end;
                }
                Step::Changed(_, j) | Step::Inserted(j) | Step::MovedIn(_, j) => {
                    let r = &b.rows[j];
                    self.copy(cur..r.span.start);
                    cur = r.span.end;
                    match (it.step, &it.detail) {
                        (Step::Changed(i, _), Some(RowDetail::Cells { cells, .. })) => self.changed_row(&a.rows[i], r, cells),
                        (Step::Changed(i, _), _) => {
                            self.deleted_row(&a.rows[i]);
                            self.inserted_row(r);
                        }
                        _ => self.inserted_row(r),
                    }
                }
                Step::Deleted(i) | Step::MovedOut(i, _) => self.deleted_row(&a.rows[i]),
            }
        }
        self.copy(cur..b.span.end);
    }

    fn changed_row(&mut self, a: &'a Row, b: &'a Row, cells: &[Option<CellDiff>]) {
        let mut cur = b.span.start;
        for (c, cd) in cells.iter().enumerate() {
            let Some(cd) = cd else { continue };
            let cell = &b.cells[c];
            self.copy(cur..cell.content.start);
            self.blocks(&a.cells[c].blocks, &cell.blocks, &cd.blocks, cell.content.clone());
            cur = cell.content.end;
        }
        self.copy(cur..b.span.end);
    }

    fn inserted_table(&mut self, t: &'a Table) {
        let mut cur = t.span.start;
        for r in &t.rows {
            self.copy(cur..r.span.start);
            self.inserted_row(r);
            cur = r.span.end;
        }
        self.copy(cur..t.span.end);
    }

    fn inserted_row(&mut self, r: &'a Row) {
        let mark = format!("<w:ins {}/>", self.attrs());
        let mut cur = r.span.start;
        match &r.trpr {
            Some(t) => {
                self.copy(cur..t.start);
                let inner = insert_before_first(inner_of(self.new_str(t)), &["<w:trPrChange"], &mark);
                let _ = write!(self.out, "<w:trPr>{inner}</w:trPr>");
                cur = t.end;
            }
            None => {
                self.copy(cur..r.trpr_at);
                let _ = write!(self.out, "<w:trPr>{mark}</w:trPr>");
                cur = r.trpr_at;
            }
        }
        for cell in &r.cells {
            self.copy(cur..cell.content.start);
            self.inserted_blocks(&cell.blocks, cell.content.clone());
            cur = cell.content.end;
        }
        self.copy(cur..r.span.end);
    }

    fn deleted_table(&mut self, t: &'a Table) {
        let x: &'a str = &self.old.xml;
        let stag_end = t.span.start as usize + x[t.span.start as usize..].find('>').map_or(0, |g| g + 1);
        let head_end = t.rows.first().map_or(t.span.end as usize - "</w:tbl>".len(), |r| r.span.start as usize);
        self.out.push_str("<w:tbl>");
        let head = self.pkg.remap(&strip(&x[stag_end..head_end], &["w:tblPrChange", "w:tblGridChange"]));
        self.out.push_str(&head);
        for r in &t.rows {
            self.deleted_row(r);
        }
        self.out.push_str("</w:tbl>");
    }

    fn deleted_row(&mut self, r: &'a Row) {
        let x: &'a str = &self.old.xml;
        let mark = format!("<w:del {}/>", self.attrs());
        let stag_end = r.span.start as usize + x[r.span.start as usize..].find('>').map_or(0, |g| g + 1);
        self.out.push_str("<w:tr>");
        if r.trpr_at as usize > stag_end {
            let ex = self.pkg.remap(&strip(&x[stag_end..r.trpr_at as usize], &["w:tblPrExChange"]));
            self.out.push_str(&ex);
        }
        // The old row's own revision marks were resolved when it was read.
        let trpr = r.trpr.as_ref().map_or("", |t| inner_of(self.old_str(t)));
        let trpr = strip(trpr, &["w:ins", "w:del", "w:trPrChange"]) + &mark;
        let _ = write!(self.out, "<w:trPr>{trpr}</w:trPr>");
        for c in &r.cells {
            self.out.push_str("<w:tc>");
            if let Some(t) = &c.tcpr {
                let tcpr = self.pkg.remap(&strip(self.old_str(t), &["w:cellIns", "w:cellDel", "w:cellMerge", "w:tcPrChange"]));
                self.out.push_str(&tcpr);
            }
            for b in &c.blocks {
                self.deleted(b, None);
            }
            // A cell must end with a paragraph. If the old cell didn't, add one
            // marked as inserted, so rejecting the redline removes it again.
            if !matches!(c.blocks.last(), Some(Block::Para(_))) {
                let attrs = self.attrs();
                let _ = write!(self.out, "<w:p><w:pPr><w:rPr><w:ins {attrs}/></w:rPr></w:pPr></w:p>");
            }
            self.out.push_str("</w:tc>");
        }
        self.out.push_str("</w:tr>");
    }
}

/// Byte range of a token range; an empty range maps to its insertion point.
fn token_bytes(p: &Para, r: &Range<usize>) -> Range<u32> {
    if r.is_empty() {
        let at = p.tokens.get(r.start).map_or(p.text.len() as u32, |t| t.start);
        at..at
    } else {
        p.tokens[r.start].start..p.tokens[r.end - 1].end
    }
}

/// `w:rPr` content of the run holding text offset `at`.
fn rpr_at(p: &Para, at: u32) -> Option<Src> {
    p.src
        .runs
        .iter()
        .find(|r| r.children.iter().any(|c| c.text.start <= at && at < c.text.end))
        .and_then(|r| r.rpr.clone())
}

fn status_at(segs: &[Seg], at: u32) -> St {
    let i = segs.partition_point(|s| s.range.end <= at);
    segs.get(i).map_or(St::Keep, |s| s.st.clone())
}

// ---- XML string helpers ----

fn esc_text(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            c => o.push(c),
        }
    }
    o
}

fn esc_attr(s: &str) -> String {
    esc_text(s).replace('"', "&quot;")
}

/// Content of an element given its outer XML (`<a ...>content</a>` or `<a/>`).
fn inner_of(outer: &str) -> &str {
    if outer.ends_with("/>") {
        return "";
    }
    let start = outer.find('>').map_or(0, |i| i + 1);
    let end = outer.rfind("</").unwrap_or(outer.len()).max(start);
    &outer[start..end]
}

/// Position of a start tag `<name` (exact name, not a prefix of a longer one).
fn find_tag(s: &str, open: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(i) = s[from..].find(open) {
        let i = from + i;
        match s.as_bytes().get(i + open.len()) {
            Some(b' ' | b'>' | b'/' | b'\t' | b'\n' | b'\r') => return Some(i),
            _ => from = i + open.len(),
        }
    }
    None
}

/// Remove every `name` element (with its subtree) from a fragment.
fn remove_elem(s: &str, name: &str) -> String {
    let open = format!("<{name}");
    let close = format!("</{name}>");
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = find_tag(rest, &open) {
        out.push_str(&rest[..i]);
        let after = &rest[i..];
        let gt = after.find('>').map_or(after.len(), |g| g + 1);
        if after[..gt].ends_with("/>") {
            rest = &after[gt..];
            continue;
        }
        let (mut depth, mut j) = (1, gt);
        while depth > 0 {
            let next_close = after[j..].find(&close).map(|k| k + j);
            let next_open = find_tag(&after[j..], &open).map(|k| k + j);
            match (next_open, next_close) {
                (Some(o), Some(c)) if o < c => {
                    let g = after[o..].find('>').map_or(after.len(), |g| o + g + 1);
                    if !after[o..g].ends_with("/>") {
                        depth += 1;
                    }
                    j = g;
                }
                (_, Some(c)) => {
                    depth -= 1;
                    j = c + close.len();
                }
                (_, None) => {
                    j = after.len();
                    break;
                }
            }
        }
        rest = &after[j..];
    }
    out.push_str(rest);
    out
}

/// `s` without any of the `names` elements.
fn strip(s: &str, names: &[&str]) -> String {
    names.iter().fold(s.to_string(), |acc, n| remove_elem(&acc, n))
}

/// An old object (drawing, text box, equation) as it reads with its own
/// tracked changes accepted, which is how it was compared. Copied into the
/// redline unresolved, rejecting the redline would also reject those.
fn accept_revisions(raw: &str) -> Cow<'_, str> {
    const MARKERS: &[&str] = &["<w:ins ", "<w:del ", "<w:moveFrom ", "<w:moveTo ", "PrChange "];
    if !MARKERS.iter().any(|m| raw.contains(m)) {
        return Cow::Borrowed(raw);
    }
    let mut s = raw.to_string();
    for name in ["w:del", "w:moveFrom", "w:rPrChange", "w:pPrChange", "w:sectPrChange", "w:tblPrChange", "w:trPrChange", "w:tcPrChange"] {
        s = remove_elem(&s, name);
    }
    for name in ["w:ins", "w:moveTo"] {
        s = unwrap_elem(&s, name);
    }
    Cow::Owned(s)
}

/// Remove `name` start/end tags but keep their content; drop empty ones.
fn unwrap_elem(s: &str, name: &str) -> String {
    let (open, close) = (format!("<{name}"), format!("</{name}>"));
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = find_tag(rest, &open) {
        out.push_str(&rest[..i]);
        let gt = i + rest[i..].find('>').map_or(rest.len() - i, |g| g + 1);
        rest = &rest[gt..];
    }
    out.push_str(rest);
    out.replace(&close, "")
}

/// Wrap the content of every `m:oMath` in `raw` in a revision element
/// (`w:ins`/`w:del` may appear inside an equation, not around it).
fn wrap_math(raw: &str, open: &str, close: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 64);
    let mut rest = raw;
    while let Some(i) = find_tag(rest, "<m:oMath") {
        let gt = i + rest[i..].find('>').map_or(0, |g| g + 1);
        if rest[i..gt].ends_with("/>") {
            out.push_str(&rest[..gt]);
            rest = &rest[gt..];
            continue;
        }
        let end = rest[gt..].find("</m:oMath>").map_or(rest.len(), |e| gt + e);
        let _ = write!(out, "{}{open}{}{close}", &rest[..gt], &rest[gt..end]);
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

/// Insert `ins` before the first of `tags` found in `s`, or at the end.
fn insert_before_first(s: &str, tags: &[&str], ins: &str) -> String {
    let at = tags.iter().filter_map(|t| find_tag(s, t)).min().unwrap_or(s.len());
    format!("{}{ins}{}", &s[..at], &s[at..])
}

/// Add a revision marker to the paragraph-mark run properties inside `w:pPr`
/// content (`w:ins`/`w:del` must be the first children of that `w:rPr`).
fn add_para_mark(ppr: &str, mark: &str) -> String {
    match find_tag(ppr, "<w:rPr") {
        Some(i) => {
            let gt = i + ppr[i..].find('>').map_or(0, |g| g + 1);
            if ppr[i..gt].ends_with("/>") {
                format!("{}<w:rPr>{mark}</w:rPr>{}", &ppr[..i], &ppr[gt..])
            } else {
                format!("{}{mark}{}", &ppr[..gt], &ppr[gt..])
            }
        }
        None => insert_before_first(ppr, &["<w:sectPr", "<w:pPrChange"], &format!("<w:rPr>{mark}</w:rPr>")),
    }
}

/// Rewrite attribute `attr` on every tag starting with `tag_open`.
fn map_attr(s: &str, tag_open: &str, attr: &str, mut f: impl FnMut(&str) -> Option<String>) -> String {
    let needle = format!(r#"{attr}=""#);
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find(tag_open) {
        let tag_end = rest[i..].find('>').map_or(rest.len(), |g| i + g);
        match rest[i..tag_end].find(&needle) {
            Some(a) => {
                let vs = i + a + needle.len();
                let ve = vs + rest[vs..].find('"').unwrap_or(0);
                out.push_str(&rest[..vs]);
                out.push_str(&f(&rest[vs..ve]).unwrap_or_else(|| rest[vs..ve].to_string()));
                rest = &rest[ve..];
            }
            None => {
                out.push_str(&rest[..tag_end]);
                rest = &rest[tag_end..];
            }
        }
    }
    out.push_str(rest);
    out
}

// ---- package ----

/// The output package: the new .docx plus whatever old content needs.
struct Package<'a> {
    new: &'a Source,
    old_zip: ZipArchive<Cursor<&'a [u8]>>,
    new_zip: ZipArchive<Cursor<&'a [u8]>>,
    /// Relationships of the main document part.
    main_rels: RelSet,
    /// Content hashes of internal parts in the new package, by part name.
    new_hashes: FxHashMap<String, Option<u64>>,
    /// (old part, output part) to copy.
    copies: Vec<(String, String)>,
    old_types: ContentTypes,
    new_types: ContentTypes,
    new_types_xml: String,
    added_types: Vec<(String, String)>,
    next_docpr: u32,
    /// Footnotes and endnotes carried over for deleted references, by kind.
    notes: FxHashMap<&'static str, Notes>,
    /// Style IDs referenced by old content brought into the redline.
    old_styles: FxHashSet<String>,
}

/// The relationships of one output part (the main part, or a notes part), and
/// how the old document's corresponding part's relationship IDs map onto them.
#[derive(Default)]
struct RelSet {
    /// Directories that the old and new parts' relative targets resolve from.
    old_dir: String,
    new_dir: String,
    old: FxHashMap<String, Rel>,
    new: Vec<Rel>,
    /// The new part's relationships part, and its XML if it exists.
    part: String,
    xml: Option<String>,
    added: Vec<Rel>,
    /// Old relationship ID → ID in the output (None: unresolvable).
    map: FxHashMap<String, Option<String>>,
}

impl RelSet {
    fn load(
        old_zip: &mut ZipArchive<Cursor<&[u8]>>,
        new_zip: &mut ZipArchive<Cursor<&[u8]>>,
        old_part: &str,
        new_part: &str,
    ) -> Result<Self> {
        let old = match docx::read_string(old_zip, &docx::rels_part(old_part)) {
            Ok(x) => docx::parse_rels(&x)?.into_iter().map(|r| (r.id.clone(), r)).collect(),
            Err(_) => FxHashMap::default(),
        };
        let part = docx::rels_part(new_part);
        let xml = docx::read_string(new_zip, &part).ok();
        let new = match &xml {
            Some(x) => docx::parse_rels(x)?,
            None => Vec::new(),
        };
        let (old_dir, new_dir) = (docx::dir_of(old_part).to_string(), docx::dir_of(new_part).to_string());
        Ok(Self { old_dir, new_dir, old, new, part, xml, added: Vec::new(), map: FxHashMap::default() })
    }

    fn add(&mut self, typ: String, target: String, external: bool) -> String {
        let id = format!("rIdXxf{}", self.added.len() + 1);
        self.added.push(Rel { id: id.clone(), typ, target, external });
        id
    }

    /// Target of `part` relative to the new part's directory.
    fn target(&self, part: &str) -> String {
        let dir = format!("{}/", self.new_dir);
        match part.strip_prefix(dir.as_str()) {
            Some(rel) if !self.new_dir.is_empty() => rel.to_string(),
            _ => format!("/{part}"),
        }
    }

    /// The relationships part XML with additions, if anything was added.
    fn output(&self) -> Option<String> {
        if self.added.is_empty() {
            return None;
        }
        let mut add = String::new();
        for r in &self.added {
            let mode = if r.external { r#" TargetMode="External""# } else { "" };
            let _ = write!(add, r#"<Relationship Id="{}" Type="{}" Target="{}"{mode}/>"#, r.id, esc_attr(&r.typ), esc_attr(&r.target));
        }
        Some(match &self.xml {
            Some(x) => insert_at_close(x, "</Relationships>", &add),
            None => format!(
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">{add}</Relationships>"#
            ),
        })
    }
}

/// A notes part (footnotes or endnotes) receiving notes from the old document.
struct Notes {
    /// The output notes part, once known.
    part: Option<String>,
    /// The part doesn't exist in the new package; it is written fresh.
    created: bool,
    old_xml: String,
    new_xml: String,
    /// Notes to append.
    added: String,
    next_id: i64,
    /// Old note ID → new note ID.
    map: FxHashMap<String, String>,
    rels: RelSet,
    /// The new package has no notes part: (old part, relationship type) to
    /// copy whole, keeping note IDs, once a reference needs it.
    copy_old: Option<(String, String)>,
}

#[derive(Default)]
struct ContentTypes {
    defaults: FxHashMap<String, String>,
    overrides: FxHashMap<String, String>,
}

impl ContentTypes {
    fn parse(xml: &str) -> Result<Self> {
        let mut ct = ContentTypes::default();
        let mut r = Reader::from_str(xml);
        loop {
            match r.read_event()? {
                Event::Empty(e) | Event::Start(e) => {
                    let get = |k: &[u8]| {
                        e.try_get_attribute(k).ok().flatten().and_then(|a| a.unescape_value().ok().map(|v| v.into_owned()))
                    };
                    match e.name().as_ref() {
                        b"Default" => {
                            if let (Some(x), Some(t)) = (get(b"Extension"), get(b"ContentType")) {
                                ct.defaults.insert(x.to_ascii_lowercase(), t);
                            }
                        }
                        b"Override" => {
                            if let (Some(p), Some(t)) = (get(b"PartName"), get(b"ContentType")) {
                                ct.overrides.insert(p.trim_start_matches('/').to_string(), t);
                            }
                        }
                        _ => {}
                    }
                }
                Event::Eof => return Ok(ct),
                _ => {}
            }
        }
    }

    fn type_of(&self, part: &str) -> Option<&String> {
        self.overrides.get(part).or_else(|| self.defaults.get(&ext(part)))
    }
}

/// Content type by extension, for packages that don't declare one.
fn guess_type(part: &str) -> &'static str {
    match ext(part).as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "bmp" => "image/bmp",
        "tif" | "tiff" => "image/tiff",
        "svg" => "image/svg+xml",
        "emf" => "image/x-emf",
        "wmf" => "image/x-wmf",
        _ => "application/octet-stream",
    }
}

fn ext(part: &str) -> String {
    part.rsplit_once('.').map_or(String::new(), |(_, e)| e.to_ascii_lowercase())
}

impl<'a> Package<'a> {
    fn new(old: &'a Source, new: &'a Source) -> Result<Self> {
        let mut old_zip = ZipArchive::new(Cursor::new(old.zip.as_slice()))?;
        let mut new_zip = ZipArchive::new(Cursor::new(new.zip.as_slice()))?;
        let main_rels = RelSet::load(&mut old_zip, &mut new_zip, &old.main, &new.main)?;
        let old_types = ContentTypes::parse(&docx::read_string(&mut old_zip, CONTENT_TYPES).unwrap_or_default())?;
        let new_types_xml = docx::read_string(&mut new_zip, CONTENT_TYPES).unwrap_or_default();
        let new_types = ContentTypes::parse(&new_types_xml)?;
        Ok(Self {
            new,
            old_zip,
            new_zip,
            main_rels,
            new_hashes: FxHashMap::default(),
            copies: Vec::new(),
            old_types,
            new_types,
            new_types_xml,
            added_types: Vec::new(),
            next_docpr: 1_000_000,
            notes: FxHashMap::default(),
            old_styles: FxHashSet::default(),
        })
    }

    /// Rewrite an old main-part XML fragment for the new package.
    fn remap(&mut self, frag: &str) -> String {
        let mut set = std::mem::take(&mut self.main_rels);
        let out = self.remap_in(&mut set, frag);
        self.main_rels = set;
        out
    }

    /// Rewrite an old XML fragment for the new package: relationship IDs
    /// (`r:*` attributes, via `set`) and drawing object IDs.
    fn remap_in(&mut self, set: &mut RelSet, frag: &str) -> String {
        for tag in ["<w:pStyle ", "<w:rStyle ", "<w:tblStyle "] {
            let _ = map_attr(frag, tag, "w:val", |id| {
                self.old_styles.insert(id.to_string());
                None
            });
        }
        if !frag.contains(" r:") && !frag.contains("<wp:docPr") {
            return frag.to_string();
        }
        let mut out = String::with_capacity(frag.len());
        let mut rest = frag;
        while let Some(i) = rest.find(" r:") {
            let after = &rest[i + 3..];
            let name_len = after.find(|c: char| !c.is_ascii_alphabetic()).unwrap_or(after.len());
            if name_len > 0 && after[name_len..].starts_with("=\"") {
                let vs = i + 3 + name_len + 2;
                let ve = vs + rest[vs..].find('"').unwrap_or(0);
                out.push_str(&rest[..vs]);
                let id = &rest[vs..ve];
                out.push_str(self.map_id(set, id).as_deref().unwrap_or(id));
                rest = &rest[ve..];
            } else {
                out.push_str(&rest[..i + 3]);
                rest = &rest[i + 3..];
            }
        }
        out.push_str(rest);
        // Drawing object IDs must be unique within the document.
        map_attr(&out, "<wp:docPr ", "id", |_| {
            self.next_docpr += 1;
            Some(self.next_docpr.to_string())
        })
    }

    /// Rewrite an old footnote/endnote reference for the new package, carrying
    /// the note it cites into the output. None if there is no note to carry.
    fn note_ref(&mut self, raw: &str) -> Option<String> {
        let kind = if raw.starts_with("<w:footnoteReference") { "footnote" } else { "endnote" };
        let id = attr_value(raw, "w:id")?;
        if !self.notes.contains_key(kind) {
            let n = self.load_notes(kind)?;
            self.notes.insert(kind, n);
        }
        let mut n = self.notes.remove(kind)?;
        let out = self.carry_note(&mut n, kind, raw, id);
        self.notes.insert(kind, n);
        out
    }

    fn carry_note(&mut self, n: &mut Notes, kind: &str, raw: &str, id: String) -> Option<String> {
        if let Some((old_part, typ)) = n.copy_old.take() {
            // No notes part in the new package: bring the old one whole.
            let new_part = self.unique_part(&old_part);
            n.rels = RelSet::load(&mut self.old_zip, &mut self.new_zip, &old_part, &new_part).ok()?;
            let old_xml = std::mem::take(&mut n.old_xml);
            n.new_xml = self.remap_in(&mut n.rels, &old_xml);
            n.part = Some(new_part.clone());
            n.created = true;
            let t = self.old_types.type_of(&old_part).cloned().unwrap_or_else(|| guess_type(&old_part).to_string());
            self.added_types.push((new_part.clone(), t));
            let target = self.main_rels.target(&new_part);
            self.main_rels.add(typ, target, false);
        }
        if n.created {
            return Some(raw.to_string());
        }
        let new_id = match n.map.get(&id) {
            Some(m) => m.clone(),
            None => {
                let inner = inner_of(find_note(&n.old_xml, kind, &id)?).to_string();
                let inner = self.remap_in(&mut n.rels, &inner);
                let nid = n.next_id.to_string();
                n.next_id += 1;
                let _ = write!(n.added, r#"<w:{kind} w:id="{nid}">{inner}</w:{kind}>"#);
                n.map.insert(id, nid.clone());
                nid
            }
        };
        Some(map_attr(raw, &format!("<w:{kind}Reference "), "w:id", |_| Some(new_id.clone())))
    }

    fn load_notes(&mut self, kind: &str) -> Option<Notes> {
        let suffix = format!("/{kind}s");
        let find = |rels: Vec<&Rel>, dir: &str| {
            rels.into_iter()
                .find(|r| !r.external && r.typ.ends_with(&suffix))
                .map(|r| (docx::resolve(dir, &r.target), r.typ.clone()))
        };
        let (old_part, typ) = find(self.main_rels.old.values().collect(), &self.main_rels.old_dir)?;
        let new_part = find(self.main_rels.new.iter().collect(), &self.main_rels.new_dir).map(|p| p.0);
        let old_xml = docx::read_string(&mut self.old_zip, &old_part).ok()?;
        let new_xml = new_part.as_ref().and_then(|p| docx::read_string(&mut self.new_zip, p).ok());
        let mut max_id = 0i64;
        if let Some(x) = &new_xml {
            let _ = map_attr(x, &format!("<w:{kind} "), "w:id", |v| {
                max_id = max_id.max(v.parse().unwrap_or(0));
                None
            });
        }
        let (part, rels, copy_old) = match (new_part, &new_xml) {
            (Some(p), Some(_)) => {
                let rels = RelSet::load(&mut self.old_zip, &mut self.new_zip, &old_part, &p).ok()?;
                (Some(p), rels, None)
            }
            _ => (None, RelSet::default(), Some((old_part, typ))),
        };
        Some(Notes {
            part,
            created: false,
            old_xml,
            new_xml: new_xml.unwrap_or_default(),
            added: String::new(),
            next_id: max_id + 1,
            map: FxHashMap::default(),
            rels,
            copy_old,
        })
    }

    fn map_id(&mut self, set: &mut RelSet, id: &str) -> Option<String> {
        if let Some(v) = set.map.get(id) {
            return v.clone();
        }
        let v = self.resolve_id(set, id);
        set.map.insert(id.to_string(), v.clone());
        v
    }

    fn resolve_id(&mut self, set: &mut RelSet, id: &str) -> Option<String> {
        let rel = set.old.get(id)?.clone();
        let old_part = docx::resolve(&set.old_dir, &rel.target);
        // External targets, and internal ones that aren't parts (a hyperlink
        // to "#bookmark"), carry over as they are.
        if rel.external || self.old_zip.index_for_name(&old_part).is_none() {
            if let Some(n) = set.new.iter().chain(&set.added).find(|n| n.external == rel.external && n.typ == rel.typ && n.target == rel.target) {
                return Some(n.id.clone());
            }
            return Some(set.add(rel.typ, rel.target, rel.external));
        }
        let bytes = docx::read_bytes(&mut self.old_zip, &old_part).ok()?;
        let hash = hash_bytes(&bytes);
        // Reuse an identical part already in the new package.
        let candidates: Vec<Rel> = set.new.iter().filter(|n| !n.external && n.typ == rel.typ).cloned().collect();
        for n in candidates {
            let part = docx::resolve(&set.new_dir, &n.target);
            let h = match self.new_hashes.get(&part) {
                Some(h) => *h,
                None => {
                    let h = docx::read_bytes(&mut self.new_zip, &part).ok().map(|b| hash_bytes(&b));
                    self.new_hashes.insert(part, h);
                    h
                }
            };
            if h == Some(hash) {
                return Some(n.id);
            }
        }
        // The same old part may already have been copied for another rel set.
        let new_part = match self.copies.iter().find(|(o, _)| *o == old_part) {
            Some((_, n)) => n.clone(),
            None => {
                let new_part = self.unique_part(&old_part);
                if !self.new_types.overrides.contains_key(&new_part) && !self.new_types.defaults.contains_key(&ext(&new_part)) {
                    let typ = self.old_types.type_of(&old_part).cloned().unwrap_or_else(|| guess_type(&old_part).to_string());
                    self.added_types.push((new_part.clone(), typ));
                }
                self.copies.push((old_part, new_part.clone()));
                new_part
            }
        };
        let target = set.target(&new_part);
        Some(set.add(rel.typ, target, false))
    }

    fn unique_part(&self, want: &str) -> String {
        let taken = |p: &str| {
            self.new_zip.index_for_name(p).is_some()
                || self.copies.iter().any(|(_, n)| n == p)
                || self.notes.values().any(|n| n.part.as_deref() == Some(p))
        };
        if !taken(want) {
            return want.to_string();
        }
        let (stem, ext) = want.rsplit_once('.').unwrap_or((want, ""));
        (1..).map(|n| format!("{stem}_xxf{n}.{ext}")).find(|p| !taken(p)).unwrap_or_default()
    }

    fn types_xml(&self) -> Option<String> {
        if self.added_types.is_empty() {
            return None;
        }
        let mut add = String::new();
        for (part, typ) in &self.added_types {
            let _ = write!(add, r#"<Override PartName="/{}" ContentType="{}"/>"#, esc_attr(part), esc_attr(typ));
        }
        Some(insert_at_close(&self.new_types_xml, "</Types>", &add))
    }

    /// The new styles part with definitions added for styles that old content
    /// uses but the new document lacks (with their `basedOn` ancestors), so
    /// deleted text keeps its look. None if nothing is missing.
    fn styles_xml(&mut self) -> Option<(String, String)> {
        if self.old_styles.is_empty() {
            return None;
        }
        let styles_part = |mut rels: std::slice::Iter<'_, &Rel>, dir: &str| {
            rels.find(|r| !r.external && r.typ.ends_with("/styles")).map(|r| docx::resolve(dir, &r.target))
        };
        let rels = &self.main_rels;
        let new_part = styles_part(rels.new.iter().collect::<Vec<_>>().iter(), &rels.new_dir)?;
        let old_part = styles_part(rels.old.values().collect::<Vec<_>>().iter(), &rels.old_dir)?;
        let new_xml = docx::read_string(&mut self.new_zip, &new_part).ok()?;
        let old_xml = docx::read_string(&mut self.old_zip, &old_part).ok()?;
        let defined = |xml: &str, id: &str| xml.contains(&format!(r#"w:styleId="{id}""#));
        // A style the new document cites without defining must stay undefined,
        // or the new content would change look.
        let mut cited_by_new = FxHashSet::default();
        for (xml, tag) in [
            (&self.new.xml, "<w:pStyle "),
            (&self.new.xml, "<w:rStyle "),
            (&self.new.xml, "<w:tblStyle "),
            (&new_xml, "<w:basedOn "),
        ] {
            let _ = map_attr(xml, tag, "w:val", |id| {
                cited_by_new.insert(id.to_string());
                None
            });
        }
        let mut queue: Vec<String> = self.old_styles.iter().cloned().collect();
        queue.sort();
        let (mut add, mut done) = (String::new(), FxHashSet::default());
        while let Some(id) = queue.pop() {
            if !done.insert(id.clone()) || defined(&new_xml, &id) || cited_by_new.contains(&id) {
                continue;
            }
            let Some(def) = find_style(&old_xml, &id) else { continue };
            let _ = map_attr(def, "<w:basedOn ", "w:val", |b| {
                queue.push(b.to_string());
                None
            });
            // Never let a carried style become the new document's default.
            let tag_end = def.find('>').unwrap_or(0);
            let tag = def[..tag_end].replace(r#" w:default="1""#, "").replace(r#" w:default="true""#, "");
            add.push_str(&tag);
            add.push_str(&def[tag_end..]);
        }
        (!add.is_empty()).then(|| (new_part, insert_at_close(&new_xml, "</w:styles>", &add)))
    }

    fn write(&mut self, document_xml: &str) -> Result<Vec<u8>> {
        // Parts to write in place of (or in addition to) the new package's.
        let mut replace: FxHashMap<String, String> = FxHashMap::default();
        if let Some((part, xml)) = self.styles_xml() {
            replace.insert(part, xml);
        }
        replace.insert(self.new.main.clone(), document_xml.to_string());
        if let Some(r) = self.main_rels.output() {
            replace.insert(self.main_rels.part.clone(), r);
        }
        if let Some(t) = self.types_xml() {
            replace.insert(CONTENT_TYPES.to_string(), t);
        }
        for (kind, n) in &self.notes {
            let Some(part) = &n.part else { continue };
            if n.created || !n.added.is_empty() {
                replace.insert(part.clone(), insert_at_close(&n.new_xml, &format!("</w:{kind}s>"), &n.added));
            }
            if let Some(r) = n.rels.output() {
                replace.insert(n.rels.part.clone(), r);
            }
        }
        // Fast compression: the output is a working file, and level 1 is several
        // times faster than the default for a few percent in size.
        let deflate = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated).compression_level(Some(1));
        let mut w = ZipWriter::new(Cursor::new(Vec::new()));
        for i in 0..self.new_zip.len() {
            let f = self.new_zip.by_index_raw(i)?;
            let name = f.name().to_string();
            match replace.remove(&name) {
                Some(content) => {
                    drop(f);
                    w.start_file(name, deflate)?;
                    w.write_all(content.as_bytes())?;
                }
                None => w.raw_copy_file(f)?,
            }
        }
        // New parts, in a stable order.
        let mut fresh: Vec<_> = replace.into_iter().collect();
        fresh.sort();
        for (name, content) in fresh {
            w.start_file(name, deflate)?;
            w.write_all(content.as_bytes())?;
        }
        for (old_part, new_part) in &self.copies {
            if let Some(i) = self.old_zip.index_for_name(old_part) {
                let f = self.old_zip.by_index_raw(i)?;
                w.raw_copy_file_rename(f, new_part)?;
            }
        }
        Ok(w.finish()?.into_inner())
    }
}

/// Value of attribute `name` in a start tag.
fn attr_value(tag: &str, name: &str) -> Option<String> {
    let needle = format!(r#" {name}=""#);
    let i = tag.find(&needle)? + needle.len();
    Some(tag[i..i + tag[i..].find('"')?].to_string())
}

/// The whole `w:style` element with the given style ID.
fn find_style<'x>(xml: &'x str, id: &str) -> Option<&'x str> {
    let start = xml.find(&format!(r#"w:styleId="{id}""#))?;
    let start = xml[..start].rfind("<w:style ")?;
    let end = start + xml[start..].find("</w:style>")? + "</w:style>".len();
    Some(&xml[start..end])
}

/// The whole `w:footnote`/`w:endnote` element with the given ID.
fn find_note<'x>(xml: &'x str, kind: &str, id: &str) -> Option<&'x str> {
    let (open, close) = (format!("<w:{kind}"), format!("</w:{kind}>"));
    let mut from = 0;
    while let Some(i) = find_tag(&xml[from..], &open) {
        let start = from + i;
        let tag_end = start + xml[start..].find('>')? + 1;
        if attr_value(&xml[start..tag_end], "w:id").as_deref() == Some(id) {
            if xml[start..tag_end].ends_with("/>") {
                return Some(&xml[start..tag_end]);
            }
            let end = tag_end + xml[tag_end..].find(&close)? + close.len();
            return Some(&xml[start..end]);
        }
        from = tag_end;
    }
    None
}

fn insert_at_close(xml: &str, close: &str, add: &str) -> String {
    match xml.rfind(close) {
        Some(i) => format!("{}{add}{}", &xml[..i], &xml[i..]),
        None => format!("{xml}{add}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remove_elem_handles_nesting_and_empty_tags() {
        let s = r#"<w:jc w:val="left"/><w:rPr><w:b/></w:rPr><w:rPrX/><w:rPr/><w:ind/>"#;
        assert_eq!(remove_elem(s, "w:rPr"), r#"<w:jc w:val="left"/><w:rPrX/><w:ind/>"#);
        let nested = "<a><x><x></x></x><y/></a>";
        assert_eq!(remove_elem(nested, "x"), "<a><y/></a>");
    }

    #[test]
    fn para_mark_goes_first_in_mark_run_properties() {
        let m = "<w:ins/>";
        assert_eq!(add_para_mark("<w:jc/><w:rPr><w:b/></w:rPr>", m), "<w:jc/><w:rPr><w:ins/><w:b/></w:rPr>");
        assert_eq!(add_para_mark("<w:jc/><w:rPr/>", m), "<w:jc/><w:rPr><w:ins/></w:rPr>");
        assert_eq!(add_para_mark("<w:jc/><w:sectPr/>", m), "<w:jc/><w:rPr><w:ins/></w:rPr><w:sectPr/>");
        assert_eq!(add_para_mark("", m), "<w:rPr><w:ins/></w:rPr>");
    }

    #[test]
    fn map_attr_rewrites_only_matching_tags() {
        let s = r#"<w:numId w:val="3"/><w:ilvl w:val="3"/>"#;
        assert_eq!(map_attr(s, "<w:numId ", "w:val", |_| Some("9".into())), r#"<w:numId w:val="9"/><w:ilvl w:val="3"/>"#);
    }

    #[test]
    fn iso_date() {
        let d = now_iso();
        assert_eq!(d.len(), 20);
        assert!(d.starts_with("20") && d.ends_with('Z'));
    }
}
