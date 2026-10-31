//! Reading .docx packages.

use std::io::{Cursor, Read, Seek};
use std::path::Path;

use quick_xml::Reader;
use quick_xml::events::Event;
use zip::ZipArchive;

use crate::Result;
use crate::hash::hash_bytes;
use crate::model::Document;
use crate::parse::{Rels, Revisions, parse_document_with};
use crate::styles::Styles;

pub const MAIN_PART: &str = "word/document.xml";
pub const MAIN_RELS: &str = "word/_rels/document.xml.rels";
pub const CONTENT_TYPES: &str = "[Content_Types].xml";
const STYLES: &str = "word/styles.xml";
const THEME: &str = "word/theme/theme1.xml";
const NUMBERING: &str = "word/numbering.xml";

/// A loaded document plus what's needed to write a redline from it: the
/// package bytes and the raw `document.xml` that the model's offsets index.
pub struct Source {
    pub doc: Document,
    pub xml: String,
    pub styles: Styles,
    pub zip: Vec<u8>,
}

/// One package relationship of the main document part.
#[derive(Debug, Clone)]
pub struct Rel {
    pub id: String,
    pub typ: String,
    pub target: String,
    pub external: bool,
}

pub fn load(path: &Path) -> Result<Source> {
    load_bytes_with(std::fs::read(path)?, Revisions::Accept)
}

pub fn load_bytes(bytes: &[u8]) -> Result<Source> {
    load_bytes_with(bytes.to_vec(), Revisions::Accept)
}

pub fn load_bytes_with(bytes: Vec<u8>, mode: Revisions) -> Result<Source> {
    let mut zip = ZipArchive::new(Cursor::new(&bytes))?;
    let xml = read_string(&mut zip, MAIN_PART)?;
    let rels = match read_string(&mut zip, MAIN_RELS) {
        Ok(s) => hash_rels(&parse_rels(&s)?, &mut zip)?,
        Err(_) => Rels::default(),
    };
    let styles = match read_string(&mut zip, STYLES) {
        Ok(s) => {
            let theme = read_string(&mut zip, THEME).ok();
            let numbering = read_string(&mut zip, NUMBERING).ok();
            Styles::parse(&s, theme.as_deref(), numbering.as_deref())?
        }
        Err(_) => Styles::default(),
    };
    let doc = parse_document_with(&xml, &rels, &styles, mode)?;
    drop(zip);
    Ok(Source { doc, xml, styles, zip: bytes })
}

pub fn read_string<R: Read + Seek>(zip: &mut ZipArchive<R>, name: &str) -> Result<String> {
    let mut f = zip.by_name(name)?;
    let mut s = String::with_capacity(f.size() as usize);
    f.read_to_string(&mut s)?;
    Ok(s)
}

pub fn read_bytes<R: Read + Seek>(zip: &mut ZipArchive<R>, name: &str) -> Result<Vec<u8>> {
    let mut f = zip.by_name(name)?;
    let mut buf = Vec::with_capacity(f.size() as usize);
    f.read_to_end(&mut buf)?;
    Ok(buf)
}

pub fn parse_rels(xml: &str) -> Result<Vec<Rel>> {
    let mut rels = Vec::new();
    let mut r = Reader::from_str(xml);
    loop {
        match r.read_event()? {
            Event::Empty(e) | Event::Start(e) if e.name().as_ref() == b"Relationship" => {
                let mut rel = Rel { id: String::new(), typ: String::new(), target: String::new(), external: false };
                for a in e.attributes() {
                    let a = a?;
                    match a.key.as_ref() {
                        b"Id" => rel.id = a.unescape_value()?.into_owned(),
                        b"Type" => rel.typ = a.unescape_value()?.into_owned(),
                        b"Target" => rel.target = a.unescape_value()?.into_owned(),
                        b"TargetMode" => rel.external = a.value.as_ref() == b"External",
                        _ => {}
                    }
                }
                rels.push(rel);
            }
            Event::Eof => return Ok(rels),
            _ => {}
        }
    }
}

/// Is this internal part compared by content (media, embeddings, charts)?
pub fn is_content_part(path: &str) -> bool {
    ["word/media/", "word/embeddings/", "word/charts/"].iter().any(|d| path.starts_with(d))
}

/// Map relationship IDs to a hash of their target. Embedded media and charts
/// are hashed by content, so the same image matches across documents
/// regardless of its file name or ID; everything else by target path/URL.
fn hash_rels<R: Read + Seek>(rels: &[Rel], zip: &mut ZipArchive<R>) -> Result<Rels> {
    let mut out = Rels::default();
    for rel in rels {
        let hash = if rel.external {
            hash_bytes(rel.target.as_bytes())
        } else {
            let p = resolve(&rel.target);
            match is_content_part(&p).then(|| read_bytes(zip, &p).ok()).flatten() {
                Some(buf) => hash_bytes(&buf),
                None => hash_bytes(p.as_bytes()),
            }
        };
        out.insert(rel.id.clone(), hash);
    }
    Ok(out)
}

/// Resolve a relationship target relative to `word/`.
pub fn resolve(target: &str) -> String {
    if let Some(abs) = target.strip_prefix('/') {
        return abs.to_string();
    }
    let mut parts: Vec<&str> = vec!["word"];
    for seg in target.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}
