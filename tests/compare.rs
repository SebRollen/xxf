mod common;

use common::{docx, p, table};
use xxf::align::Step;
use xxf::compare::{BlockItem, Detail, RowDetail, SpanKind, Stats, diff_blocks, stats};
use xxf::docx::load_bytes;
use xxf::model::Document;
use xxf::render::{Style, render};

fn compare(old: &str, new: &str) -> (Document, Document, Vec<BlockItem>) {
    let a = load_bytes(&docx(old)).unwrap();
    let b = load_bytes(&docx(new)).unwrap();
    let items = diff_blocks(&a.blocks, &b.blocks);
    (a, b, items)
}

fn report(old: &str, new: &str) -> String {
    let (a, b, items) = compare(old, new);
    render(&a, &b, &items, &Style::PLAIN)
}

fn changed(n: usize) -> Stats {
    Stats { changed: n, ..Stats::default() }
}

#[test]
fn run_splits_and_revision_noise_are_invisible() {
    let old = p("The quick brown fox.");
    let new = r#"<w:p w:rsidR="00AB12" w14:paraId="1A2B3C4D"><w:proofErr w:type="spellStart"/><w:r w:rsidR="0011"><w:t>The qu</w:t></w:r><w:bookmarkStart w:id="0" w:name="_GoBack"/><w:r w:rsidRPr="0022"><w:rPr><w:lang w:val="en-GB"/></w:rPr><w:t xml:space="preserve">ick brown</w:t></w:r><w:proofErr w:type="spellEnd"/><w:r><w:lastRenderedPageBreak/><w:t xml:space="preserve"> fox.</w:t></w:r><w:bookmarkEnd w:id="0"/></w:p>"#;
    let (_, _, items) = compare(&old, new);
    assert_eq!(stats(&items), Stats::default());
}

#[test]
fn word_edit() {
    let out = report(&p("The quick brown fox jumps."), &p("The quick red fox jumps."));
    assert_eq!(out.lines().next().unwrap(), "~ ¶1→¶1: The quick [-brown-]{+red+} fox jumps.");
}

#[test]
fn adjacent_word_edits_merge_across_spaces() {
    let out = report(&p("one two three four"), &p("one 2 3 four"));
    assert!(out.contains("one [-two three-]{+2 3+} four"), "{out}");
}

#[test]
fn bold_is_a_format_change() {
    let old = p("Make this bold please.");
    let new = r#"<w:p><w:r><w:t xml:space="preserve">Make this </w:t></w:r><w:r><w:rPr><w:b/></w:rPr><w:t>bold</w:t></w:r><w:r><w:t xml:space="preserve"> please.</w:t></w:r></w:p>"#;
    let (a, b, items) = compare(&old, new);
    assert_eq!(stats(&items), changed(1));
    let Some(Detail::Para(d)) = &items[0].detail else { panic!() };
    let kinds: Vec<_> = d.spans.iter().map(|s| s.kind).collect();
    assert_eq!(kinds, [SpanKind::Same, SpanKind::Reformat, SpanKind::Same]);
    assert!(render(&a, &b, &items, &Style::PLAIN).contains("Make this {~bold~} please."));
}

#[test]
fn paragraph_properties_change() {
    let old = p("A heading of sorts");
    let new = r#"<w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:r><w:t>A heading of sorts</w:t></w:r></w:p>"#;
    let (_, _, items) = compare(&old, new);
    let Some(Detail::Para(d)) = &items[0].detail else { panic!() };
    assert!(d.ppr_changed);
    assert!(d.spans.iter().all(|s| s.kind == SpanKind::Same));
}

#[test]
fn insert_and_delete_paragraphs() {
    let old = [p("Alpha paragraph here."), p("Beta paragraph is going away."), p("Gamma closes it.")].concat();
    let new = [p("Alpha paragraph here."), p("Gamma closes it."), p("Delta is entirely new text.")].concat();
    let (_, _, items) = compare(&old, &new);
    let steps: Vec<Step> = items.iter().map(|i| i.step).collect();
    assert_eq!(steps, [Step::Equal(0, 0), Step::Deleted(1), Step::Equal(2, 1), Step::Inserted(2)]);
}

#[test]
fn edited_paragraph_pairs_instead_of_delete_insert() {
    let old = [p("Intro."), p("The committee will meet on Tuesday to review the budget."), p("End.")].concat();
    let new = [p("Intro."), p("The committee will meet on Thursday to review the final budget."), p("End.")].concat();
    let (_, _, items) = compare(&old, &new);
    assert_eq!(stats(&items), changed(1));
    assert_eq!(items[1].step, Step::Changed(1, 1));
}

#[test]
fn moved_paragraph() {
    let a = p("First paragraph stays put.");
    let b = p("Second paragraph also stays.");
    let m = p("This one will be moved later.");
    let (_, _, items) = compare(&[m.clone(), a.clone(), b.clone()].concat(), &[a, b, m].concat());
    let steps: Vec<Step> = items.iter().map(|i| i.step).collect();
    assert_eq!(steps, [Step::MovedOut(0, 2), Step::Equal(1, 0), Step::Equal(2, 1), Step::MovedIn(0, 2)]);
}

#[test]
fn moved_and_reformatted_paragraph_has_detail() {
    let a = p("Anchor paragraph number one.");
    let m = p("Moving words go here.");
    let m2 = r#"<w:p><w:r><w:rPr><w:i/></w:rPr><w:t>Moving words go here.</w:t></w:r></w:p>"#;
    let (_, _, items) = compare(&[m, a.clone()].concat(), &[a, m2.to_string()].concat());
    let moved = items.iter().find(|i| matches!(i.step, Step::MovedIn(..))).unwrap();
    assert!(matches!(moved.detail, Some(Detail::Para(_))));
}

#[test]
fn table_cell_edit() {
    let old = table(&[&["Name", "Qty"], &["Apples", "3"], &["Pears", "5"]]);
    let new = table(&[&["Name", "Qty"], &["Apples", "4"], &["Pears", "5"]]);
    let (a, b, items) = compare(&old, &new);
    let Some(Detail::Table(t)) = &items[0].detail else { panic!("{items:?}") };
    let row = t.rows.iter().find(|r| r.step == Step::Changed(1, 1)).unwrap();
    let Some(RowDetail::Cells { cells, .. }) = &row.detail else { panic!() };
    assert!(cells[0].is_none() && cells[1].is_some());
    let out = render(&a, &b, &items, &Style::PLAIN);
    assert!(out.contains("~ T1 r2 c2 ¶1→¶1: [-3-]{+4+}"), "{out}");
}

#[test]
fn table_row_inserted() {
    let old = table(&[&["Name", "Qty"], &["Apples", "3"]]);
    let new = table(&[&["Name", "Qty"], &["Kiwis", "9"], &["Apples", "3"]]);
    let out = report(&old, &new);
    assert!(out.contains("+ T1 row 2: Kiwis | 9"), "{out}");
}

#[test]
fn existing_tracked_changes_are_accepted() {
    let old = p("Final text here.");
    let new = r#"<w:p><w:r><w:t xml:space="preserve">Final </w:t></w:r><w:del w:id="1" w:author="x"><w:r><w:delText>draft </w:delText></w:r></w:del><w:ins w:id="2" w:author="x"><w:r><w:t>text</w:t></w:r></w:ins><w:r><w:t xml:space="preserve"> here.</w:t></w:r></w:p>"#;
    let (_, _, items) = compare(&old, new);
    assert_eq!(stats(&items), Stats::default());
}

#[test]
fn hyperlinks_and_content_controls_are_transparent() {
    let old = p("See the docs for details.");
    let new = r#"<w:p><w:r><w:t xml:space="preserve">See </w:t></w:r><w:hyperlink r:id="rId9"><w:r><w:t>the docs</w:t></w:r></w:hyperlink><w:sdt><w:sdtPr><w:alias w:val="x"/></w:sdtPr><w:sdtContent><w:r><w:t xml:space="preserve"> for details.</w:t></w:r></w:sdtContent></w:sdt></w:p>"#;
    let (_, _, items) = compare(&old, new);
    assert_eq!(stats(&items), Stats::default());
}

#[test]
fn identical_documents() {
    let body = [p("One."), table(&[&["a", "b"]]), p("Two.")].concat();
    assert_eq!(report(&body, &body), "No differences.\n");
}

#[test]
fn reformat_plus_trailing_delete_at_end_of_document() {
    let old = [p("Same words here."), p("Deleted at the very end.")].concat();
    let new = r#"<w:p><w:r><w:rPr><w:b/></w:rPr><w:t>Same words here.</w:t></w:r></w:p>"#;
    let (_, _, items) = compare(&old, new);
    let steps: Vec<Step> = items.iter().map(|i| i.step).collect();
    assert_eq!(steps, [Step::Changed(0, 0), Step::Deleted(1)]);
}
