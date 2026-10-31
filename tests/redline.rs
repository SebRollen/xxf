//! Redline round trips: accepting every change must give the new document,
//! rejecting every change must give the old document's text, and the output
//! must be well-formed.

mod common;

use std::io::{Cursor, Read};

use common::{docx, docx_with_parts, document_xml, p, table, zip_parts};
use quick_xml::Reader;
use quick_xml::events::Event;
use xxf::compare::{Stats, diff_blocks, stats};
use xxf::docx::{Source, load_bytes, load_bytes_with};
use xxf::model::Block;
use xxf::parse::Revisions;
use xxf::redline::{Options, write};

fn opts() -> Options {
    Options { author: "Tester & Co".into(), date: "2026-10-30T12:00:00Z".into() }
}

/// Paragraph texts in document order, descending into tables. The text hash
/// is what the diff compares (hidden bookmark names normalized).
fn texts(blocks: &[Block], out: &mut Vec<(u64, String)>) {
    for b in blocks {
        match b {
            Block::Para(p) => out.push((p.text_hash, p.text.clone())),
            Block::Table(t) => {
                for r in &t.rows {
                    for c in &r.cells {
                        texts(&c.blocks, out);
                    }
                }
            }
            Block::Opaque(_) => {}
        }
    }
}

fn all_texts(s: &Source) -> Vec<(u64, String)> {
    let mut v = Vec::new();
    texts(&s.doc.blocks, &mut v);
    v
}

fn part(bytes: &[u8], name: &str) -> String {
    let mut z = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
    let mut s = String::new();
    z.by_name(name).unwrap().read_to_string(&mut s).unwrap();
    s
}

fn assert_well_formed(xml: &str) {
    let mut r = Reader::from_str(xml);
    loop {
        match r.read_event() {
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(e) => panic!("malformed at {}: {e}\n{xml}", r.buffer_position()),
        }
    }
}

/// Write the redline and check both round trips. Returns its document.xml.
fn roundtrip_bytes(old_bytes: &[u8], new_bytes: &[u8]) -> (Vec<u8>, String) {
    roundtrip_checked(old_bytes, new_bytes, true)
}

/// `check_reject` is off when the new document already has tracked changes:
/// rejecting everything then also rejects those.
fn roundtrip_checked(old_bytes: &[u8], new_bytes: &[u8], check_reject: bool) -> (Vec<u8>, String) {
    roundtrip_inner(old_bytes, new_bytes, check_reject, "word/document.xml")
}

fn roundtrip_checked_part(old_bytes: &[u8], new_bytes: &[u8], main: &str) -> (Vec<u8>, String) {
    roundtrip_inner(old_bytes, new_bytes, true, main)
}

fn roundtrip_inner(old_bytes: &[u8], new_bytes: &[u8], check_reject: bool, main: &str) -> (Vec<u8>, String) {
    let old = load_bytes(old_bytes).unwrap();
    let new = load_bytes(new_bytes).unwrap();
    let items = diff_blocks(&old.doc.blocks, &new.doc.blocks);
    let out = write(&old, &new, &items, &opts()).unwrap();
    let xml = part(&out, main);
    assert_well_formed(&xml);

    let accepted = load_bytes_with(out.clone(), Revisions::Accept).unwrap();
    let again = diff_blocks(&new.doc.blocks, &accepted.doc.blocks);
    assert_eq!(stats(&new.doc.blocks, &accepted.doc.blocks, &again), Stats::default(), "accept ≠ new\n{xml}");

    if check_reject {
        let rejected = load_bytes_with(out.clone(), Revisions::Reject).unwrap();
        assert_eq!(all_texts(&rejected), all_texts(&old), "reject ≠ old\n{xml}");
    }
    (out, xml)
}

fn roundtrip(old_body: &str, new_body: &str) -> String {
    roundtrip_bytes(&docx(old_body), &docx(new_body)).1
}

#[test]
fn identical_documents_copy_through() {
    let body = [p("One."), table(&[&["a", "b"]]), p("Two.")].concat();
    let xml = roundtrip(&body, &body);
    assert!(!xml.contains("<w:ins") && !xml.contains("<w:del"));
}

#[test]
fn word_edit() {
    let xml = roundtrip(&p("The quick brown fox jumps."), &p("The quick red fox jumps."));
    assert!(xml.contains(r#"<w:delText xml:space="preserve">brown</w:delText>"#), "{xml}");
    assert!(xml.contains(r#"<w:t xml:space="preserve">red</w:t>"#), "{xml}");
    assert!(xml.contains(r#"w:author="Tester &amp; Co""#));
}

#[test]
fn edits_at_start_and_end_and_in_multi_run_text() {
    let old = r#"<w:p><w:r><w:t xml:space="preserve">Alpha beta </w:t></w:r><w:r><w:rPr><w:b/></w:rPr><w:t>gamma delta</w:t></w:r><w:r><w:t xml:space="preserve"> epsilon.</w:t></w:r></w:p>"#;
    let new = r#"<w:p><w:r><w:t xml:space="preserve">Zeta beta </w:t></w:r><w:r><w:rPr><w:b/></w:rPr><w:t>gamma theta</w:t></w:r><w:r><w:t xml:space="preserve"> epsilon! Extra.</w:t></w:r></w:p>"#;
    roundtrip(old, new);
}

#[test]
fn paragraph_inserted_deleted_and_emptied() {
    let old = [p("Alpha paragraph here."), p("Beta paragraph is going away."), p("Gamma closes it."), "<w:p/>".into()].concat();
    let new = [p("Alpha paragraph here."), p("Gamma closes it."), p("Delta is entirely new text."), p("")].concat();
    roundtrip(&old, &new);
}

#[test]
fn moved_paragraph_uses_move_markup() {
    let a = p("First paragraph stays put.");
    let b = p("Second paragraph also stays.");
    let m = p("This one will be moved later.");
    let xml = roundtrip(&[m.clone(), a.clone(), b.clone()].concat(), &[a, b, m].concat());
    assert!(xml.contains("<w:moveFrom ") && xml.contains("<w:moveTo "), "{xml}");
    assert!(xml.contains(r#"w:name="move1""#));
}

#[test]
fn reformat_gets_rpr_change_with_old_properties() {
    let old = r#"<w:p><w:r><w:rPr><w:i/></w:rPr><w:t>Make this bold please.</w:t></w:r></w:p>"#;
    let new = r#"<w:p><w:r><w:rPr><w:i/></w:rPr><w:t xml:space="preserve">Make this </w:t></w:r><w:r><w:rPr><w:b/></w:rPr><w:t>bold</w:t></w:r><w:r><w:rPr><w:i/></w:rPr><w:t xml:space="preserve"> please.</w:t></w:r></w:p>"#;
    let xml = roundtrip(old, new);
    assert!(xml.contains("<w:rPr><w:b/><w:rPrChange "), "{xml}");
    assert!(xml.contains("<w:rPr><w:i/></w:rPr></w:rPrChange>"), "{xml}");
}

#[test]
fn paragraph_property_change_gets_ppr_change() {
    let old = r#"<w:p><w:pPr><w:jc w:val="right"/><w:rPr><w:b/></w:rPr></w:pPr><w:r><w:t>A centered line</w:t></w:r></w:p>"#;
    let new = r#"<w:p><w:pPr><w:jc w:val="center"/></w:pPr><w:r><w:t>A centered line</w:t></w:r></w:p>"#;
    let xml = roundtrip(old, new);
    assert!(xml.contains(r#"<w:pPr><w:jc w:val="center"/><w:pPrChange "#), "{xml}");
    assert!(xml.contains(r#"<w:pPr><w:jc w:val="right"/></w:pPr></w:pPrChange>"#), "{xml}");
}

#[test]
fn table_edits() {
    let old = table(&[&["Name", "Qty"], &["Apples", "3"], &["Pears", "5"], &["Plums", "7"]]);
    let new = table(&[&["Name", "Qty"], &["Kiwis", "9"], &["Apples", "4"], &["Pears", "5"]]);
    let xml = roundtrip(&old, &new);
    assert!(xml.contains("<w:trPr><w:ins ") && xml.contains("<w:trPr><w:del "), "{xml}");
}

#[test]
fn table_inserted_and_deleted() {
    let t1 = table(&[&["Gone", "table"], &["with", "rows"]]);
    let t2 = table(&[&["New", "table"]]);
    roundtrip(&[p("Before."), t1, p("After.")].concat(), &[p("Before."), p("After."), t2].concat());
}

#[test]
fn unmodeled_markup_survives_in_place() {
    let old = p("See the docs for details.");
    let new = r#"<w:p><w:bookmarkStart w:id="0" w:name="here"/><w:r><w:t xml:space="preserve">See </w:t></w:r><w:hyperlink w:anchor="here"><w:r><w:t>our docs</w:t></w:r></w:hyperlink><w:r><w:t xml:space="preserve"> for details.</w:t></w:r><w:bookmarkEnd w:id="0"/></w:p>"#;
    let xml = roundtrip(&old, new);
    assert!(xml.contains(r#"<w:bookmarkStart w:id="0" w:name="here"/>"#) && xml.contains(r#"<w:hyperlink w:anchor="here">"#), "{xml}");
}

#[test]
fn tabs_breaks_and_fields_split_cleanly() {
    let old = r#"<w:p><w:r><w:t>Page</w:t><w:tab/><w:t xml:space="preserve">one </w:t></w:r><w:r><w:fldChar w:fldCharType="begin"/></w:r><w:r><w:instrText xml:space="preserve"> PAGE </w:instrText></w:r><w:r><w:fldChar w:fldCharType="separate"/></w:r><w:r><w:t>1</w:t></w:r><w:r><w:fldChar w:fldCharType="end"/></w:r></w:p>"#;
    let new = r#"<w:p><w:r><w:t>Page</w:t><w:br/><w:t xml:space="preserve">two </w:t></w:r><w:r><w:fldChar w:fldCharType="begin"/></w:r><w:r><w:instrText xml:space="preserve"> PAGE </w:instrText></w:r><w:r><w:fldChar w:fldCharType="separate"/></w:r><w:r><w:t>1</w:t></w:r><w:r><w:fldChar w:fldCharType="end"/></w:r></w:p>"#;
    roundtrip(old, new);
}

#[test]
fn existing_tracked_changes_in_new_are_kept() {
    let old = p("Final text here.");
    let new = r#"<w:p><w:r><w:t xml:space="preserve">Final </w:t></w:r><w:del w:id="1" w:author="x"><w:r><w:delText>draft </w:delText></w:r></w:del><w:r><w:t>text there.</w:t></w:r></w:p>"#;
    let xml = roundtrip_checked(&docx(&old), &docx(new), false).1;
    assert!(xml.contains(r#"<w:del w:id="1" w:author="x">"#), "{xml}");
}

#[test]
fn deleted_first_row_and_first_cell_paragraph_stay_inside_the_table() {
    let cell = |paras: &[&str]| -> String {
        let ps: String = paras.iter().map(|t| p(t)).collect();
        format!(r#"<w:tc><w:tcPr><w:tcW w:w="2000" w:type="dxa"/></w:tcPr>{ps}</w:tc>"#)
    };
    let row = |cells: &[String]| format!("<w:tr>{}</w:tr>", cells.concat());
    let tbl = |rows: &[String]| format!(r#"<w:tbl><w:tblPr><w:tblW w:w="0" w:type="auto"/></w:tblPr><w:tblGrid><w:gridCol/><w:gridCol/></w:tblGrid>{}</w:tbl>"#, rows.concat());
    let old = tbl(&[
        row(&[cell(&["Header row going away"]), cell(&["Header two"])]),
        row(&[cell(&["First paragraph to delete entirely", "Kept paragraph"]), cell(&["x"])]),
    ]);
    let new = tbl(&[row(&[cell(&["Kept paragraph"]), cell(&["x"])])]);
    let xml = roundtrip(&old, &new);
    let tbl_at = xml.find("<w:tbl>").unwrap();
    assert!(xml.find("<w:tr>").unwrap() > tbl_at, "{xml}");
    // Cell properties stay the first child of every cell.
    assert_eq!(xml.matches("<w:tc>").count(), xml.matches("<w:tc><w:tcPr>").count(), "{xml}");
}

#[test]
fn byte_order_mark_and_unconventional_main_part() {
    let rels = |main: &str| -> Vec<u8> {
        format!(r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="{main}"/></Relationships>"#).into_bytes()
    };
    let with_bom = |body: &str| -> Vec<u8> { [b"\xEF\xBB\xBF".as_slice(), document_xml(body).as_bytes()].concat() };
    let old = zip_parts(&[
        ("_rels/.rels", rels("word/trial.xml")),
        ("word/trial.xml", with_bom(&[p("Same first."), p("Old second.")].concat())),
    ]);
    let new = zip_parts(&[
        ("_rels/.rels", rels("word/trial.xml")),
        ("word/trial.xml", with_bom(&[p("Same first."), p("New second.")].concat())),
    ]);
    let (out, _) = roundtrip_checked_part(&old, &new, "word/trial.xml");
    assert!(part(&out, "word/trial.xml").contains("<w:delText"));
}

const FOOTNOTE_REL: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/footnotes";
const HYPERLINK_REL: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink";

fn rels_xml(entries: &[(&str, &str, &str, bool)]) -> Vec<u8> {
    let mut x = String::from(r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">"#);
    for (id, typ, target, external) in entries {
        let mode = if *external { r#" TargetMode="External""# } else { "" };
        x.push_str(&format!(r#"<Relationship Id="{id}" Type="{typ}" Target="{target}"{mode}/>"#));
    }
    x.push_str("</Relationships>");
    x.into_bytes()
}

fn footnotes_xml(notes: &str) -> Vec<u8> {
    format!(
        r#"<?xml version="1.0"?><w:footnotes xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><w:footnote w:type="separator" w:id="-1"><w:p><w:r><w:separator/></w:r></w:p></w:footnote>{notes}</w:footnotes>"#
    )
    .into_bytes()
}

#[test]
fn deleted_footnote_with_a_link_keeps_its_relationship() {
    let note = r#"<w:footnote w:id="1"><w:p><w:hyperlink r:id="rId9"><w:r><w:t>see example.com</w:t></w:r></w:hyperlink></w:p></w:footnote>"#;
    let old = docx_with_parts(
        r#"<w:p><w:r><w:t>Claim</w:t></w:r><w:r><w:footnoteReference w:id="1"/></w:r><w:r><w:t xml:space="preserve"> stays.</w:t></w:r></w:p>"#,
        None,
        &[
            ("word/_rels/document.xml.rels", &rels_xml(&[("rId3", FOOTNOTE_REL, "footnotes.xml", false)])),
            ("word/footnotes.xml", &footnotes_xml(note)),
            ("word/_rels/footnotes.xml.rels", &rels_xml(&[("rId9", HYPERLINK_REL, "https://example.com/", true)])),
        ],
    );
    let new = docx_with_parts(
        &p("Claim stays."),
        None,
        &[
            ("word/_rels/document.xml.rels", &rels_xml(&[("rId3", FOOTNOTE_REL, "footnotes.xml", false)])),
            ("word/footnotes.xml", &footnotes_xml("")),
        ],
    );
    let (out, xml) = roundtrip_bytes(&old, &new);
    assert!(xml.contains(r#"<w:footnoteReference w:id="1"/>"#), "{xml}");
    let notes = part(&out, "word/footnotes.xml");
    assert!(notes.contains(r#"<w:hyperlink r:id="rIdXxf1">"#), "{notes}");
    assert!(part(&out, "word/_rels/footnotes.xml.rels").contains("https://example.com/"));
}

#[test]
fn old_cell_and_row_revisions_are_resolved_not_copied() {
    // Old: one cell inserted (w:cellIns) and one row inserted (trPr/w:ins), both accepted.
    let cell = |marker: &str, text: &str| format!(r#"<w:tc><w:tcPr><w:tcW w:w="2000" w:type="dxa"/>{marker}</w:tcPr>{}</w:tc>"#, p(text));
    let old = format!(
        r#"<w:tbl><w:tblPr/><w:tblGrid><w:gridCol/><w:gridCol/></w:tblGrid><w:tr>{}{}</w:tr><w:tr><w:trPr><w:ins w:id="7" w:author="a"/></w:trPr>{}{}</w:tr></w:tbl>"#,
        cell("", "Alpha cell text"),
        cell(r#"<w:cellIns w:id="8" w:author="a"/>"#, "Inserted cell"),
        cell("", "Row two"),
        cell("", "more")
    );
    roundtrip(&old, &p("All replaced by one paragraph."));
}

#[test]
fn cell_ending_in_a_nested_table_stays_valid_when_deleted() {
    let inner = table(&[&["inner"]]);
    let old = format!(r#"<w:tbl><w:tblPr/><w:tblGrid><w:gridCol/></w:tblGrid><w:tr><w:tc><w:tcPr/>{inner}</w:tc></w:tr></w:tbl>"#);
    let xml = roundtrip(&old, &p("Gone."));
    assert!(xml.contains("<w:tc><w:tcPr/><w:tbl>"), "{xml}");
}

#[test]
fn internal_non_part_relationship_carries_over() {
    // A picture whose click jumps to a bookmark: an internal target that isn't a part.
    let link = |rid: &str, text: &str| {
        format!(
            r#"<w:p><w:r><w:t xml:space="preserve">{text} </w:t></w:r><w:r><w:drawing><wp:inline><wp:docPr id="1" name="x"><a:hlinkClick xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" r:id="{rid}"/></wp:docPr></wp:inline></w:drawing></w:r></w:p>"#
        )
    };
    let old = docx_with_parts(
        &[p("Keep."), link("rId4", "Old words before the link")].concat(),
        None,
        &[("word/_rels/document.xml.rels", &rels_xml(&[("rId4", HYPERLINK_REL, "#bookmark", false)]))],
    );
    let new = docx_with_parts(
        &p("Keep."),
        None,
        &[("word/_rels/document.xml.rels", &rels_xml(&[("rId4", HYPERLINK_REL, "https://other.example/", true)]))],
    );
    let (out, _) = roundtrip_bytes(&old, &new);
    assert!(part(&out, "word/_rels/document.xml.rels").contains(r##"Target="#bookmark""##));
}

#[test]
fn old_namespaces_and_styles_come_along() {
    let styles = |extra: &str| -> Vec<u8> {
        format!(
            r#"<?xml version="1.0"?><w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:style w:type="paragraph" w:default="1" w:styleId="Normal"><w:name w:val="Normal"/></w:style>{extra}</w:styles>"#
        )
        .into_bytes()
    };
    let styles_rel = rels_xml(&[("rId1", "http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles", "styles.xml", false)]);
    let quote = r#"<w:style w:type="paragraph" w:default="1" w:styleId="Quote"><w:basedOn w:val="Base"/></w:style><w:style w:type="paragraph" w:styleId="Base"><w:name w:val="Base"/></w:style>"#;
    // The old paragraph uses a w15 attribute and a style the new document lacks.
    let old_body = r#"<w:p xmlns:w15="http://schemas.microsoft.com/office/word/2012/wordml"><w:pPr><w:pStyle w:val="Quote"/></w:pPr><w:r><w:t>Quoted old text here.</w:t></w:r></w:p>"#;
    let old_doc = [
        br#"<?xml version="1.0"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:w15="http://schemas.microsoft.com/office/word/2012/wordml"><w:body>"#.as_slice(),
        old_body.replace(r#" xmlns:w15="http://schemas.microsoft.com/office/word/2012/wordml""#, "").replace("<w:r>", r#"<w:r w15:x="1">"#).as_bytes(),
        b"</w:body></w:document>",
    ]
    .concat();
    let old = zip_parts(&[
        ("word/document.xml", old_doc),
        ("word/_rels/document.xml.rels", styles_rel.clone()),
        ("word/styles.xml", styles(quote)),
    ]);
    let new = zip_parts(&[
        ("word/document.xml", document_xml(&p("Completely different replacement sentence goes here.")).into_bytes()),
        ("word/_rels/document.xml.rels", styles_rel),
        ("word/styles.xml", styles("")),
    ]);
    let (out, xml) = roundtrip_bytes(&old, &new);
    assert!(xml.contains(r#"xmlns:w15="http://schemas.microsoft.com/office/word/2012/wordml""#), "{xml}");
    let st = part(&out, "word/styles.xml");
    assert!(st.contains(r#"w:styleId="Quote""#) && st.contains(r#"w:styleId="Base""#), "{st}");
    assert_eq!(st.matches(r#"w:default="1""#).count(), 1, "{st}");
}

const PNG_OLD: &[u8] = b"\x89PNG old image bytes";
const PNG_NEW: &[u8] = b"\x89PNG new image bytes";

fn image_para(rid: &str, caption: &str) -> String {
    format!(
        r#"<w:p><w:r><w:t xml:space="preserve">{caption} </w:t></w:r><w:r><w:drawing><wp:inline><wp:docPr id="1" name="Picture 1"/><a:graphic xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><a:graphicData><pic:pic xmlns:pic="http://schemas.openxmlformats.org/drawingml/2006/picture"><pic:blipFill><a:blip r:embed="{rid}"/></pic:blipFill></pic:pic></a:graphicData></a:graphic></wp:inline></w:drawing></w:r></w:p>"#
    )
}

fn rels(entries: &[(&str, &str)]) -> Vec<u8> {
    let mut x = String::from(r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">"#);
    for (id, target) in entries {
        x.push_str(&format!(
            r#"<Relationship Id="{id}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="{target}"/>"#
        ));
    }
    x.push_str("</Relationships>");
    x.into_bytes()
}

#[test]
fn deleted_image_is_carried_into_the_package() {
    let old_rels = rels(&[("rId7", "media/image1.png")]);
    let new_rels = rels(&[("rId4", "media/image1.png")]);
    let old = docx_with_parts(
        &[p("Intro text stays."), image_para("rId7", "Old figure caption")].concat(),
        None,
        &[("word/_rels/document.xml.rels", &old_rels), ("word/media/image1.png", PNG_OLD)],
    );
    let new = docx_with_parts(
        &[p("Intro text stays."), image_para("rId4", "Fresh chart")].concat(),
        None,
        &[("word/_rels/document.xml.rels", &new_rels), ("word/media/image1.png", PNG_NEW)],
    );
    let (out, xml) = roundtrip_bytes(&old, &new);
    let out_rels = part(&out, "word/_rels/document.xml.rels");
    // The old image collides by name with the new one, so it is copied under a new name.
    assert!(out_rels.contains(r#"Id="rIdXxf1""#) && out_rels.contains("media/image1_xxf1.png"), "{out_rels}");
    assert!(xml.contains(r#"r:embed="rIdXxf1""#) && xml.contains(r#"r:embed="rId4""#), "{xml}");
    let mut z = zip::ZipArchive::new(Cursor::new(&out)).unwrap();
    let mut img = Vec::new();
    z.by_name("word/media/image1_xxf1.png").unwrap().read_to_end(&mut img).unwrap();
    assert_eq!(img, PNG_OLD);
    // A png type must be declared for the copied part.
    assert!(part(&out, "[Content_Types].xml").contains("image1_xxf1.png"));
}

#[test]
fn identical_image_reuses_the_new_relationship() {
    let old_rels = rels(&[("rId7", "media/a.png")]);
    let new_rels = rels(&[("rId4", "media/b.png")]);
    let old = docx_with_parts(
        &[image_para("rId7", "Same picture, caption gone entirely"), p("Tail.")].concat(),
        None,
        &[("word/_rels/document.xml.rels", &old_rels), ("word/media/a.png", PNG_OLD)],
    );
    let new = docx_with_parts(
        &[p("Tail."), image_para("rId4", "Unrelated caption words")].concat(),
        None,
        &[("word/_rels/document.xml.rels", &new_rels), ("word/media/b.png", PNG_OLD)],
    );
    let (out, xml) = roundtrip_bytes(&old, &new);
    assert!(!part(&out, "word/_rels/document.xml.rels").contains("rIdXxf"));
    assert!(!xml.contains("rId7"), "{xml}");
}

/// Round trip on real files: `XXF_OLD=a.docx XXF_NEW=b.docx cargo test --test redline -- --ignored`
#[test]
#[ignore]
fn real_files() {
    let (Ok(a), Ok(b)) = (std::env::var("XXF_OLD"), std::env::var("XXF_NEW")) else {
        panic!("set XXF_OLD and XXF_NEW");
    };
    let new_bytes = std::fs::read(&b).unwrap();
    let has_revisions = ["<w:ins ", "<w:del ", "<w:moveFrom ", "rPrChange", "pPrChange"]
        .iter()
        .any(|t| part(&new_bytes, "word/document.xml").contains(t));
    let (out, _) = roundtrip_checked(&std::fs::read(&a).unwrap(), &new_bytes, !has_revisions);
    if let Ok(path) = std::env::var("XXF_OUT") {
        std::fs::write(path, out).unwrap();
    }
}
