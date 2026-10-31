//! Reading .docx packages.

use std::fs::File;
use std::io::{BufReader, Cursor, Read, Seek};
use std::path::Path;

use quick_xml::Reader;
use quick_xml::events::Event;
use zip::ZipArchive;

use crate::Result;
use crate::hash::hash_bytes;
use crate::model::Document;
use crate::parse::{Rels, parse_document};
use crate::styles::Styles;

const MAIN_PART: &str = "word/document.xml";
const MAIN_RELS: &str = "word/_rels/document.xml.rels";
const STYLES: &str = "word/styles.xml";
const THEME: &str = "word/theme/theme1.xml";
const NUMBERING: &str = "word/numbering.xml";

pub fn load(path: &Path) -> Result<Document> {
    load_from(BufReader::new(File::open(path)?))
}

pub fn load_bytes(bytes: &[u8]) -> Result<Document> {
    load_from(Cursor::new(bytes))
}

fn load_from<R: Read + Seek>(r: R) -> Result<Document> {
    let mut zip = ZipArchive::new(r)?;
    let xml = read_string(&mut zip, MAIN_PART)?;
    let rels = match read_string(&mut zip, MAIN_RELS) {
        Ok(s) => parse_rels(&s, &mut zip)?,
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
    parse_document(&xml, &rels, &styles)
}

fn read_string<R: Read + Seek>(zip: &mut ZipArchive<R>, name: &str) -> Result<String> {
    let mut f = zip.by_name(name)?;
    let mut s = String::with_capacity(f.size() as usize);
    f.read_to_string(&mut s)?;
    Ok(s)
}

/// Map relationship IDs to a hash of their target. Embedded media and charts
/// are hashed by content, so the same image matches across documents
/// regardless of its file name or ID; everything else by target path/URL.
fn parse_rels<R: Read + Seek>(xml: &str, zip: &mut ZipArchive<R>) -> Result<Rels> {
    let mut rels = Rels::default();
    let mut r = Reader::from_str(xml);
    loop {
        match r.read_event()? {
            Event::Empty(e) | Event::Start(e) if e.name().as_ref() == b"Relationship" => {
                let (mut id, mut target, mut external) = (None, None, false);
                for a in e.attributes() {
                    let a = a?;
                    match a.key.as_ref() {
                        b"Id" => id = Some(a.unescape_value()?.into_owned()),
                        b"Target" => target = Some(a.unescape_value()?.into_owned()),
                        b"TargetMode" => external = a.value.as_ref() == b"External",
                        _ => {}
                    }
                }
                let (Some(id), Some(target)) = (id, target) else { continue };
                let path = (!external).then(|| resolve(&target));
                let hash = match path {
                    Some(p) if ["word/media/", "word/embeddings/", "word/charts/"].iter().any(|d| p.starts_with(d)) => {
                        let mut buf = Vec::new();
                        match zip.by_name(&p) {
                            Ok(mut f) => {
                                f.read_to_end(&mut buf)?;
                                hash_bytes(&buf)
                            }
                            Err(_) => hash_bytes(p.as_bytes()),
                        }
                    }
                    Some(p) => hash_bytes(p.as_bytes()),
                    None => hash_bytes(target.as_bytes()),
                };
                rels.insert(id, hash);
            }
            Event::Eof => return Ok(rels),
            _ => {}
        }
    }
}

/// Resolve a relationship target relative to `word/`.
fn resolve(target: &str) -> String {
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
