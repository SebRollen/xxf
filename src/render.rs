//! Human-readable report of a comparison: plain or colored text for the
//! terminal, or an HTML fragment (for the web page) from the same logic.

use std::fmt::Write;

use crate::align::Step;
use crate::compare::{BlockItem, Detail, Item, ParaDiff, RowDetail, SpanKind, TableDiff, is_blank, stats};
use crate::model::{Block, Document, OBJ, Para, Row, Table};

pub struct Style {
    pub del: &'static str,
    pub ins: &'static str,
    pub fmt: &'static str,
    pub mv: &'static str,
    pub dim: &'static str,
    /// Ends any of the above.
    pub reset: &'static str,
    /// Text markers around deleted, inserted and reformatted text:
    /// [del open, del close, ins open, ins close, fmt open, fmt close].
    pub marks: [&'static str; 6],
    /// Escape document text for HTML.
    pub html: bool,
}

const TEXT_MARKS: [&str; 6] = ["[-", "-]", "{+", "+}", "{~", "~}"];

impl Style {
    pub const PLAIN: Style =
        Style { del: "", ins: "", fmt: "", mv: "", dim: "", reset: "", marks: TEXT_MARKS, html: false };
    pub const COLOR: Style = Style {
        del: "\x1b[31m",
        ins: "\x1b[32m",
        fmt: "\x1b[33m",
        mv: "\x1b[36m",
        dim: "\x1b[2m",
        reset: "\x1b[0m",
        marks: TEXT_MARKS,
        html: false,
    };
    /// An HTML fragment meant for a `white-space: pre-wrap` container, with
    /// `del`, `ins`, `fmt`, `mv` and `dim` classes to style.
    pub const HTML: Style = Style {
        del: r#"<span class="del">"#,
        ins: r#"<span class="ins">"#,
        fmt: r#"<span class="fmt">"#,
        mv: r#"<span class="mv">"#,
        dim: r#"<span class="dim">"#,
        reset: "</span>",
        marks: ["", "", "", "", "", ""],
        html: true,
    };
}

/// Characters of unchanged context to show around edits.
const CONTEXT: usize = 40;
/// Maximum characters shown for a whole inserted/deleted/moved block.
const SUMMARY: usize = 160;

pub fn render(old: &Document, new: &Document, items: &[BlockItem], st: &Style) -> String {
    let mut r = Renderer { out: String::new(), st };
    r.blocks(&old.blocks, &new.blocks, items, "", 0);
    let s = stats(&old.blocks, &new.blocks, items);
    if s == Default::default() {
        r.out.push_str("No differences.\n");
    } else {
        let _ = write!(
            r.out,
            "\n{} changed, {} inserted, {} deleted, {} moved",
            s.changed,
            s.inserted,
            s.deleted,
            s.moved
        );
        if s.blank > 0 {
            let _ = write!(r.out, " ({} empty paragraphs added/removed, not shown)", s.blank);
        }
        r.out.push('\n');
    }
    r.out
}

struct Renderer<'s> {
    out: String,
    st: &'s Style,
}

impl Renderer<'_> {
    fn line(&mut self, indent: usize, color: &str, text: std::fmt::Arguments) {
        let reset = if color.is_empty() { "" } else { self.st.reset };
        let _ = writeln!(self.out, "{:w$}{color}{text}{reset}", "", w = indent * 2);
    }

    /// Document-derived text, escaped for the output format.
    fn esc(&self, s: String) -> String {
        if self.st.html { escape_html(&s) } else { s }
    }

    fn label(&self, blocks: &[Block], i: usize) -> String {
        self.esc(label(blocks, i))
    }

    fn summary(&self, b: &Block) -> String {
        self.esc(summary(b))
    }

    fn row_text(&self, r: &Row) -> String {
        self.esc(row_text(r))
    }

    fn blocks(&mut self, old: &[Block], new: &[Block], items: &[BlockItem], prefix: &str, indent: usize) {
        let st = self.st;
        for it in items {
            match it.step {
                Step::Equal(..) => {}
                // Empty spacer paragraphs are counted in the summary, not listed.
                Step::Deleted(i) if is_blank(&old[i]) => {}
                Step::Inserted(j) if is_blank(&new[j]) => {}
                Step::Deleted(i) => {
                    self.line(indent, st.del, format_args!("- {prefix}{}: {}", self.label(old, i), self.summary(&old[i])))
                }
                Step::Inserted(j) => {
                    self.line(indent, st.ins, format_args!("+ {prefix}{}: {}", self.label(new, j), self.summary(&new[j])))
                }
                Step::MovedOut(i, j) => self.line(
                    indent,
                    st.mv,
                    format_args!("» {prefix}{} moved to {}: {}", self.label(old, i), self.label(new, j), self.summary(&old[i])),
                ),
                Step::MovedIn(i, j) => {
                    let head = format!("« {prefix}{} moved from {}", self.label(new, j), self.label(old, i));
                    match &it.detail {
                        Some(d) => self.detail(&old[i], &new[j], d, &head, prefix, indent),
                        None => self.line(indent, st.mv, format_args!("{head}")),
                    }
                }
                Step::Changed(i, j) => {
                    let head = format!("~ {prefix}{}→{}", self.label(old, i), self.label(new, j));
                    if let Some(d) = &it.detail {
                        self.detail(&old[i], &new[j], d, &head, prefix, indent);
                    }
                }
            }
        }
    }

    fn detail(&mut self, a: &Block, b: &Block, d: &Detail, head: &str, prefix: &str, indent: usize) {
        match (a, b, d) {
            (Block::Para(a), Block::Para(b), Detail::Para(d)) => {
                let (body, all_reformatted) = self.inline(a, b, d);
                let mut notes = Vec::new();
                if all_reformatted {
                    notes.push("text formatting changed".to_string());
                }
                if d.ppr_changed {
                    notes.push(match (&a.style, &b.style) {
                        (x, y) if x != y => self.esc(format!(
                            "paragraph style {} → {}",
                            x.as_deref().unwrap_or("none"),
                            y.as_deref().unwrap_or("none")
                        )),
                        _ => "paragraph formatting changed".to_string(),
                    });
                }
                let note = if notes.is_empty() { String::new() } else { format!(" ({})", notes.join("; ")) };
                let (dim, reset) = (self.st.dim, self.st.reset);
                self.line(indent, "", format_args!("{head}: {body}{dim}{note}{reset}"));
            }
            (Block::Table(a), Block::Table(b), Detail::Table(d)) => {
                let note = if d.props_changed { " (table properties changed)" } else { "" };
                self.line(indent, "", format_args!("{head}{note}"));
                let tprefix = format!("{prefix}{} ", head.rsplit('→').next().unwrap_or(""));
                self.rows(a, b, d, &tprefix, indent + 1);
            }
            _ => self.line(indent, "", format_args!("{head}: changed")),
        }
    }

    fn rows(&mut self, a: &Table, b: &Table, d: &TableDiff, prefix: &str, indent: usize) {
        let st = self.st;
        for Item { step, detail } in &d.rows {
            match *step {
                Step::Equal(..) => {}
                Step::Deleted(i) => self.line(indent, st.del, format_args!("- {prefix}row {}: {}", i + 1, self.row_text(&a.rows[i]))),
                Step::Inserted(j) => self.line(indent, st.ins, format_args!("+ {prefix}row {}: {}", j + 1, self.row_text(&b.rows[j]))),
                Step::MovedOut(i, j) => self.line(
                    indent,
                    st.mv,
                    format_args!("» {prefix}row {} moved to row {}: {}", i + 1, j + 1, self.row_text(&a.rows[i])),
                ),
                Step::MovedIn(i, j) | Step::Changed(i, j) => {
                    if matches!(step, Step::MovedIn(..)) {
                        self.line(indent, st.mv, format_args!("« {prefix}row {} moved from row {}", j + 1, i + 1));
                    }
                    match detail {
                        Some(RowDetail::Restructured) => {
                            self.line(indent, st.del, format_args!("- {prefix}row {}: {}", i + 1, self.row_text(&a.rows[i])));
                            self.line(indent, st.ins, format_args!("+ {prefix}row {}: {}", j + 1, self.row_text(&b.rows[j])));
                        }
                        Some(RowDetail::Cells { props_changed, cells }) => {
                            if *props_changed {
                                self.line(indent, st.dim, format_args!("~ {prefix}row {}: row properties changed", j + 1));
                            }
                            for (c, cell) in cells.iter().enumerate() {
                                let Some(cell) = cell else { continue };
                                let cprefix = format!("{prefix}r{} c{} ", j + 1, c + 1);
                                if cell.props_changed {
                                    self.line(indent, st.dim, format_args!("~ {cprefix}cell properties changed"));
                                }
                                let (x, y) = (&a.rows[i].cells[c], &b.rows[j].cells[c]);
                                self.blocks(&x.blocks, &y.blocks, &cell.blocks, &cprefix, indent);
                            }
                        }
                        None => {}
                    }
                }
            }
        }
    }

    /// Inline markup for a paragraph diff. When every unchanged-text span was
    /// reformatted (a whole-paragraph format change), formatting markers would
    /// just wrap everything, so the text is shown plainly and the second
    /// return value is true for the caller to note instead.
    fn inline(&self, a: &Para, b: &Para, d: &ParaDiff) -> (String, bool) {
        let st = self.st;
        let all_reformatted = d.spans.iter().any(|s| s.kind == SpanKind::Reformat)
            && !d.spans.iter().any(|s| s.kind == SpanKind::Same);
        let [d0, d1, i0, i1, f0, f1] = st.marks;
        let text = |p: &Para, r: &std::ops::Range<usize>| self.esc(show(p.slice(r.clone())));
        let mut s = String::new();
        let n = d.spans.len();
        for (k, sp) in d.spans.iter().enumerate() {
            match sp.kind {
                SpanKind::Same => {
                    let t = show(a.slice(sp.old.clone()));
                    s.push_str(&self.esc(elide(&t, k > 0, k + 1 < n)));
                }
                SpanKind::Reformat if all_reformatted => {
                    let t = show(b.slice(sp.new.clone()));
                    s.push_str(&self.esc(elide(&t, k > 0, k + 1 < n)));
                }
                SpanKind::Reformat => {
                    let _ = write!(s, "{}{f0}{}{f1}{}", st.fmt, text(b, &sp.new), st.reset);
                }
                SpanKind::Replace => {
                    if !sp.old.is_empty() {
                        let _ = write!(s, "{}{d0}{}{d1}{}", st.del, text(a, &sp.old), st.reset);
                    }
                    if !sp.new.is_empty() {
                        let _ = write!(s, "{}{i0}{}{i1}{}", st.ins, text(b, &sp.new), st.reset);
                    }
                }
            }
        }
        (s, all_reformatted)
    }
}

fn label(blocks: &[Block], i: usize) -> String {
    match &blocks[i] {
        Block::Para(_) => format!("¶{}", i + 1),
        Block::Table(_) => format!("T{}", i + 1),
        Block::Opaque(o) => format!("<{}>#{}", o.name, i + 1),
    }
}

fn summary(b: &Block) -> String {
    match b {
        Block::Para(p) => truncate(&show(&p.text), SUMMARY),
        Block::Table(t) => {
            let first = t.rows.first().map(row_text).unwrap_or_default();
            format!("table, {} rows: {}", t.rows.len(), truncate(&first, SUMMARY))
        }
        Block::Opaque(o) => o.name.clone(),
    }
}

fn row_text(r: &Row) -> String {
    let cells: Vec<String> = r
        .cells
        .iter()
        .map(|c| {
            c.blocks
                .iter()
                .filter_map(|b| match b {
                    Block::Para(p) => Some(show(&p.text)),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();
    truncate(&cells.join(" | "), SUMMARY)
}

fn escape_html(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            c => o.push(c),
        }
    }
    o
}

/// Make invisible characters visible.
fn show(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            OBJ => '◆',
            '\t' => '⇥',
            '\n' => '↵',
            c => c,
        })
        .collect()
}

fn truncate(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

/// Shorten unchanged context, keeping the ends that touch an edit.
fn elide(s: &str, keep_head: bool, keep_tail: bool) -> String {
    if !keep_head && !keep_tail {
        // Nothing to give context to: the span is the whole paragraph.
        return truncate(s, SUMMARY);
    }
    let chars: Vec<char> = s.chars().collect();
    let budget = CONTEXT * (keep_head as usize + keep_tail as usize);
    if chars.len() <= budget + 1 {
        return s.to_string();
    }
    let head: String = if keep_head { chars[..CONTEXT].iter().collect() } else { String::new() };
    let tail: String = if keep_tail { chars[chars.len() - CONTEXT..].iter().collect() } else { String::new() };
    format!("{head}…{tail}")
}
