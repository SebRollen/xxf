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

pub const CONTENT_TYPES: &str = "[Content_Types].xml";
const PACKAGE_RELS: &str = "_rels/.rels";
/// Conventional part names, used when the package's relationships don't say.
const MAIN_PART: &str = "word/document.xml";
const STYLES: &str = "word/styles.xml";
const THEME: &str = "word/theme/theme1.xml";
const NUMBERING: &str = "word/numbering.xml";

/// A loaded document plus what's needed to write a redline from it: the
/// package bytes and the raw main part XML that the model's offsets index.
pub struct Source {
    pub doc: Document,
    pub xml: String,
    pub styles: Styles,
    pub zip: Vec<u8>,
    /// Name of the main document part, usually `word/document.xml`.
    pub main: String,
    /// Name of the main part's relationships part.
    pub main_rels: String,
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
    // Encrypted .docx files and legacy .doc files are OLE compound files.
    if bytes.starts_with(&[0xD0, 0xCF, 0x11, 0xE0]) {
        return Err("not a .docx package (encrypted document, or a legacy .doc file)".into());
    }
    let mut zip = ZipArchive::new(Cursor::new(&bytes))?;
    let main = main_part(&mut zip);
    let main_rels = rels_part(&main);
    let base = dir_of(&main).to_string();
    let xml = read_string(&mut zip, &main).map_err(|e| -> crate::Error {
        if zip.index_for_name("mimetype").is_some() && zip.index_for_name("content.xml").is_some() {
            "not a .docx package (an OpenDocument file)".into()
        } else {
            format!("no main document part {main}: {e}").into()
        }
    })?;
    let rel_list = match read_string(&mut zip, &main_rels) {
        Ok(s) => parse_rels(&s)?,
        Err(_) => Vec::new(),
    };
    let rels = hash_rels(&rel_list, &base, &mut zip)?;
    let part = |suffix: &str, default: &str| {
        rel_list
            .iter()
            .find(|r| !r.external && r.typ.ends_with(suffix))
            .map_or_else(|| default.to_string(), |r| resolve(&base, &r.target))
    };
    let styles = match read_string(&mut zip, &part("/styles", STYLES)) {
        Ok(s) => {
            let theme = read_string(&mut zip, &part("/theme", THEME)).ok();
            let numbering = read_string(&mut zip, &part("/numbering", NUMBERING)).ok();
            Styles::parse(&s, theme.as_deref(), numbering.as_deref())?
        }
        Err(_) => Styles::default(),
    };
    let doc = parse_document_with(&xml, &rels, &styles, mode)?;
    drop(zip);
    Ok(Source { doc, xml, styles, zip: bytes, main, main_rels })
}

/// The main document part, from the package relationships.
fn main_part<R: Read + Seek>(zip: &mut ZipArchive<R>) -> String {
    read_string(zip, PACKAGE_RELS)
        .ok()
        .and_then(|s| parse_rels(&s).ok())
        .and_then(|rels| rels.into_iter().find(|r| r.typ.ends_with("/officeDocument") && !r.external))
        .map_or_else(|| MAIN_PART.to_string(), |r| resolve("", &r.target))
}

/// `word/document.xml` → `word/_rels/document.xml.rels`
pub fn rels_part(part: &str) -> String {
    match part.rsplit_once('/') {
        Some((dir, name)) => format!("{dir}/_rels/{name}.rels"),
        None => format!("_rels/{part}.rels"),
    }
}

/// Directory of a part name: `word/document.xml` → `word`.
pub fn dir_of(part: &str) -> &str {
    part.rsplit_once('/').map_or("", |(d, _)| d)
}

/// A part's text, without a leading byte-order mark (which the XML reader
/// would skip, shifting every recorded offset).
pub fn read_string<R: Read + Seek>(zip: &mut ZipArchive<R>, name: &str) -> Result<String> {
    let mut f = zip.by_name(name)?;
    let mut s = String::with_capacity(f.size() as usize);
    f.read_to_string(&mut s)?;
    if s.starts_with('\u{FEFF}') {
        s.drain(..3);
    }
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

/// Is this internal part compared by content? Parts that body content
/// points at (images, charts, SmartArt, embeddings) are, so they match across
/// documents whatever their names. Document-wide parts (styles, settings,
/// notes, headers) are compared by name: they are large, and their changes
/// are not body changes.
pub fn is_content_part(path: &str) -> bool {
    const DOCUMENT_WIDE: &[&str] = &[
        "styles", "stylesWithEffects", "settings", "webSettings", "fontTable", "numbering", "footnotes", "endnotes",
        "comments", "people", "header", "footer", "theme/", "glossary/", "customXml",
    ];
    let name = path.rsplit_once("word/").map_or(path, |(_, n)| n);
    path.contains("word/") && !DOCUMENT_WIDE.iter().any(|p| name.starts_with(p))
}

/// Map relationship IDs to a hash of their target. Embedded media and charts
/// are hashed by content, so the same image matches across documents
/// regardless of its file name or ID; everything else by target path/URL.
fn hash_rels<R: Read + Seek>(rels: &[Rel], base: &str, zip: &mut ZipArchive<R>) -> Result<Rels> {
    let mut out = Rels::default();
    for rel in rels {
        let hash = if rel.external {
            hash_bytes(rel.target.as_bytes())
        } else {
            let p = resolve(base, &rel.target);
            match is_content_part(&p).then(|| read_bytes(zip, &p).ok()).flatten() {
                Some(buf) => hash_bytes(&buf),
                None => hash_bytes(p.as_bytes()),
            }
        };
        out.insert(rel.id.clone(), hash);
    }
    Ok(out)
}

/// Resolve a relationship target relative to the source part's directory.
pub fn resolve(base: &str, target: &str) -> String {
    if let Some(abs) = target.strip_prefix('/') {
        return abs.to_string();
    }
    let mut parts: Vec<&str> = base.split('/').filter(|s| !s.is_empty()).collect();
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
