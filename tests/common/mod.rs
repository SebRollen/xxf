use std::io::{Cursor, Write};

use zip::ZipWriter;
use zip::write::SimpleFileOptions;

const NS: &str = r#"xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships""#;

/// Build a minimal .docx whose body is `body` (raw WordprocessingML).
pub fn docx(body: &str) -> Vec<u8> {
    let mut z = ZipWriter::new(Cursor::new(Vec::new()));
    let opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    let files = [
        (
            "[Content_Types].xml",
            r#"<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>"#.to_string(),
        ),
        (
            "word/document.xml",
            format!(r#"<?xml version="1.0" encoding="UTF-8"?><w:document {NS}><w:body>{body}</w:body></w:document>"#),
        ),
    ];
    for (name, content) in files {
        z.start_file(name, opts).unwrap();
        z.write_all(content.as_bytes()).unwrap();
    }
    z.finish().unwrap().into_inner()
}

/// A paragraph with one plain run.
pub fn p(text: &str) -> String {
    format!(r#"<w:p><w:r><w:t xml:space="preserve">{text}</w:t></w:r></w:p>"#)
}

/// A one-row-per-slice table of plain paragraphs.
#[allow(dead_code)] // not every test binary uses it
pub fn table(rows: &[&[&str]]) -> String {
    let rows: String = rows
        .iter()
        .map(|cells| {
            let cells: String = cells.iter().map(|c| format!("<w:tc>{}</w:tc>", p(c))).collect();
            format!("<w:tr>{cells}</w:tr>")
        })
        .collect();
    format!("<w:tbl><w:tblPr/>{rows}</w:tbl>")
}
