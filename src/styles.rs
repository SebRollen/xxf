//! Style resolution: turn direct + inherited formatting into the effective
//! properties Word would render, so that the same look expressed two ways
//! (direct formatting vs. a named style) compares equal.
//!
//! Properties are flattened to `path[@attr] → value` maps, e.g.
//! `w:spacing@w:before → 100`, `w:b → 1`, `w:sz → 24`, and merged in Word's
//! order: document defaults → paragraph style chain → character style chain →
//! direct formatting.
//!
//! List numbering IDs (`w:numId`) are arbitrary per file, so they are replaced
//! by a hash of the list definition they point to.
//!
//! Not yet modeled: table styles, numbering-level properties, toggle-property
//! XOR between style levels, complex-script/East-Asian variants (ignored).

use std::collections::BTreeMap;

use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};
use rustc_hash::FxHashMap;

use crate::Result;
use crate::hash::{H, hash_bytes};

pub type Props = BTreeMap<String, String>;

/// Property elements that don't affect how Latin text renders, or are
/// revision/proofing bookkeeping.
const IGNORED_ELEMS: &[&[u8]] = &[
    b"w:rPrChange",
    b"w:pPrChange",
    b"w:sectPrChange",
    b"w:numberingChange",
    b"w:ins",
    b"w:del",
    b"w:moveFrom",
    b"w:moveTo",
    b"w:noProof",
    b"w:lang",
    b"w:bCs",
    b"w:iCs",
    b"w:szCs",
    b"w:cs",
    // Paragraph-mark run properties inside w:pPr.
    b"w:rPr",
];

const IGNORED_ATTRS: &[&[u8]] = &[
    b"w:eastAsia",
    b"w:eastAsiaTheme",
    b"w:cs",
    b"w:cstheme",
    b"w:hint",
    // Relationship IDs differ between files and aren't resolvable here.
    b"r:id",
];

/// On/off properties: absent, `w:val="0"`, `"false"` and `"off"` all mean off.
const ON_OFF: &[&str] = &[
    "w:b",
    "w:i",
    "w:caps",
    "w:smallCaps",
    "w:strike",
    "w:dstrike",
    "w:outline",
    "w:shadow",
    "w:emboss",
    "w:imprint",
    "w:vanish",
    "w:webHidden",
    "w:specVanish",
    "w:rtl",
    "w:keepNext",
    "w:keepLines",
    "w:pageBreakBefore",
    "w:widowControl",
    "w:contextualSpacing",
    "w:suppressAutoHyphens",
    "w:suppressLineNumbers",
    "w:mirrorIndents",
    "w:bidi",
];

/// Values equal to the spec default once everything is merged, so explicit
/// and implicit defaults compare equal.
const SPEC_DEFAULTS: &[(&str, &str)] = &[
    ("w:sz", "20"),
    ("w:color", "auto"),
    ("w:u", "none"),
    ("w:vertAlign", "baseline"),
    ("w:highlight", "none"),
    ("w:jc", "left"),
    ("w:jc", "start"),
    ("w:outlineLvl", "9"),
    ("w:spacing@w:before", "0"),
    ("w:spacing@w:after", "0"),
    ("w:spacing@w:beforeAutospacing", "0"),
    ("w:spacing@w:afterAutospacing", "0"),
    ("w:ind@w:left", "0"),
    ("w:ind@w:start", "0"),
    ("w:ind@w:right", "0"),
    ("w:ind@w:end", "0"),
    ("w:ind@w:hanging", "0"),
    ("w:ind@w:firstLine", "0"),
];

#[derive(Debug, Default)]
struct StyleDef {
    based_on: Option<String>,
    ppr: Props,
    rpr: Props,
}

#[derive(Debug, Default)]
pub struct Styles {
    defs: FxHashMap<String, StyleDef>,
    default_para: Option<String>,
    doc_ppr: Props,
    doc_rpr: Props,
    major_font: Option<String>,
    minor_font: Option<String>,
    /// `w:numId` → hash of its list definition, as a string property value.
    numbering: FxHashMap<String, String>,
}

impl Styles {
    pub fn parse(styles_xml: &str, theme_xml: Option<&str>, numbering_xml: Option<&str>) -> Result<Self> {
        let mut s = Styles::default();
        if let Some(t) = theme_xml {
            (s.major_font, s.minor_font) = theme_fonts(t)?;
        }
        if let Some(n) = numbering_xml {
            s.numbering = numbering(n)?;
        }
        let mut r = Reader::from_str(styles_xml);
        let mut in_defaults = false;
        loop {
            match r.read_event()? {
                Event::Start(e) => match e.name().as_ref() {
                    b"w:docDefaults" => in_defaults = true,
                    b"w:rPr" if in_defaults => {
                        let span = r.read_to_end(e.name())?;
                        flatten(&styles_xml[span.start as usize..span.end as usize], &mut s.doc_rpr)?;
                    }
                    b"w:pPr" if in_defaults => {
                        let span = r.read_to_end(e.name())?;
                        flatten(&styles_xml[span.start as usize..span.end as usize], &mut s.doc_ppr)?;
                    }
                    b"w:style" => {
                        let id = attr(&e, b"w:styleId").unwrap_or_default();
                        let is_default = matches!(attr(&e, b"w:default").as_deref(), Some("1" | "true" | "on"));
                        if is_default && attr(&e, b"w:type").as_deref() == Some("paragraph") {
                            s.default_para = Some(id.clone());
                        }
                        let def = style_def(&mut r, styles_xml)?;
                        s.defs.insert(id, def);
                    }
                    _ => {}
                },
                Event::End(e) if e.name().as_ref() == b"w:docDefaults" => in_defaults = false,
                Event::Eof => return Ok(s),
                _ => {}
            }
        }
    }

    /// Merge a style and its `basedOn` ancestors into `ppr`/`rpr`, root first.
    fn apply_chain(&self, id: &str, ppr: &mut Props, rpr: &mut Props) {
        let mut chain = Vec::new();
        let mut cur = Some(id);
        // Bounded walk: malformed files can contain basedOn cycles.
        while let Some(c) = cur
            && chain.len() < 32
        {
            let Some(def) = self.defs.get(c) else { break };
            chain.push(def);
            cur = def.based_on.as_deref();
        }
        for def in chain.iter().rev() {
            merge(ppr, &def.ppr);
            merge(rpr, &def.rpr);
        }
    }

    fn theme_font(&self, name: &str) -> Option<&str> {
        if name.starts_with("major") {
            self.major_font.as_deref()
        } else if name.starts_with("minor") {
            self.minor_font.as_deref()
        } else {
            None
        }
    }

    /// Canonicalize merged properties and hash them (0 = all defaults).
    fn finish(&self, mut p: Props) -> u64 {
        if let Some(id) = p.get_mut("w:numPr/w:numId")
            && let Some(def) = self.numbering.get(id.as_str())
        {
            *id = def.clone();
        }
        for (theme, plain) in [("w:rFonts@w:asciiTheme", "w:rFonts@w:ascii"), ("w:rFonts@w:hAnsiTheme", "w:rFonts@w:hAnsi")] {
            if let Some(t) = p.get(theme)
                && let Some(font) = self.theme_font(t)
            {
                let font = font.to_string();
                p.remove(theme);
                p.insert(plain.to_string(), font);
            }
        }
        if p.get("w:spacing@w:beforeAutospacing").is_some_and(|v| is_on(v)) {
            p.remove("w:spacing@w:before");
        }
        if p.get("w:spacing@w:afterAutospacing").is_some_and(|v| is_on(v)) {
            p.remove("w:spacing@w:after");
        }
        let auto_rule = p.get("w:spacing@w:lineRule").is_none_or(|v| v == "auto");
        if auto_rule && p.get("w:spacing@w:line").is_none_or(|v| v == "240") {
            p.remove("w:spacing@w:line");
            p.remove("w:spacing@w:lineRule");
        }
        for k in ON_OFF {
            if let Some(v) = p.get_mut(*k) {
                *v = if is_on(v) { "1" } else { "0" }.to_string();
            }
        }
        p.retain(|k, v| {
            let off = v == "0" && ON_OFF.contains(&k.as_str());
            let spec_default = SPEC_DEFAULTS.contains(&(k.as_str(), v.as_str()));
            let cleared_tab = v == "clear" && k.contains("w:tab[");
            !(off || spec_default || cleared_tab)
        });
        if p.is_empty() {
            return 0;
        }
        let mut h = H::new();
        for (k, v) in &p {
            h.bytes(k.as_bytes());
            h.bytes(v.as_bytes());
        }
        h.finish()
    }
}

/// Resolves effective paragraph and run properties, caching by the raw XML of
/// the direct formatting: real documents repeat the same few `w:rPr`/`w:pPr`
/// blocks thousands of times.
pub struct Resolver<'s> {
    styles: &'s Styles,
    ppr_cache: FxHashMap<u64, (u64, Option<String>)>,
    rpr_cache: FxHashMap<(u64, u64), u64>,
}

impl<'s> Resolver<'s> {
    pub fn new(styles: &'s Styles) -> Self {
        Self { styles, ppr_cache: FxHashMap::default(), rpr_cache: FxHashMap::default() }
    }

    /// Effective paragraph properties for a raw `w:pPr` body, plus the
    /// paragraph style ID that runs inherit from.
    pub fn ppr(&mut self, raw: &str) -> Result<(u64, Option<String>)> {
        let key = hash_bytes(raw.as_bytes());
        if let Some(v) = self.ppr_cache.get(&key) {
            return Ok(v.clone());
        }
        let mut direct = Props::new();
        flatten(raw, &mut direct)?;
        let style = direct.remove("w:pStyle").or_else(|| self.styles.default_para.clone());
        let mut ppr = self.styles.doc_ppr.clone();
        let mut unused = Props::new();
        if let Some(id) = &style {
            self.styles.apply_chain(id, &mut ppr, &mut unused);
        }
        merge(&mut ppr, &direct);
        let v = (self.styles.finish(ppr), style);
        self.ppr_cache.insert(key, v.clone());
        Ok(v)
    }

    /// Effective run properties for a raw `w:rPr` body in a paragraph of `style`.
    pub fn rpr(&mut self, style: Option<&str>, raw: &str) -> Result<u64> {
        let key = (style.map_or(0, |s| hash_bytes(s.as_bytes())), hash_bytes(raw.as_bytes()));
        if let Some(&v) = self.rpr_cache.get(&key) {
            return Ok(v);
        }
        let mut direct = Props::new();
        flatten(raw, &mut direct)?;
        let mut rpr = self.styles.doc_rpr.clone();
        let mut unused = Props::new();
        if let Some(id) = style {
            self.styles.apply_chain(id, &mut unused, &mut rpr);
        }
        if let Some(id) = direct.remove("w:rStyle") {
            self.styles.apply_chain(&id, &mut unused, &mut rpr);
        }
        merge(&mut rpr, &direct);
        let v = self.styles.finish(rpr);
        self.rpr_cache.insert(key, v);
        Ok(v)
    }
}

fn is_on(v: &str) -> bool {
    matches!(v, "" | "1" | "true" | "on")
}

/// Layer `top` over `base`. A font set explicitly replaces an inherited theme
/// font for the same script, and vice versa.
fn merge(base: &mut Props, top: &Props) {
    for (k, v) in top {
        for (a, b) in [("@w:ascii", "@w:asciiTheme"), ("@w:hAnsi", "@w:hAnsiTheme")] {
            if let Some(stem) = k.strip_suffix(a) {
                base.remove(&format!("{stem}{b}"));
            } else if let Some(stem) = k.strip_suffix(b) {
                base.remove(&format!("{stem}{a}"));
            }
        }
        base.insert(k.clone(), v.clone());
    }
}

fn attr(e: &BytesStart, key: &[u8]) -> Option<String> {
    e.try_get_attribute(key).ok().flatten().and_then(|a| a.unescape_value().ok().map(|v| v.into_owned()))
}

/// Flatten the body of a property element (`w:rPr`, `w:pPr`) into `out`.
pub fn flatten(xml: &str, out: &mut Props) -> Result<()> {
    let mut r = Reader::from_str(xml);
    let mut path: Vec<String> = Vec::new();
    loop {
        match r.read_event()? {
            Event::Start(e) => {
                if IGNORED_ELEMS.contains(&e.name().as_ref()) {
                    r.read_to_end(e.name())?;
                    continue;
                }
                let p = child_path(path.last(), &e);
                add_attrs(&p, &e, out);
                path.push(p);
            }
            Event::Empty(e) => {
                if !IGNORED_ELEMS.contains(&e.name().as_ref()) {
                    let p = child_path(path.last(), &e);
                    add_attrs(&p, &e, out);
                }
            }
            Event::End(_) => {
                path.pop();
            }
            Event::Eof => return Ok(()),
            _ => {}
        }
    }
}

fn child_path(parent: Option<&String>, e: &BytesStart) -> String {
    let name = String::from_utf8_lossy(e.name().as_ref()).into_owned();
    let mut p = match parent {
        Some(pp) => format!("{pp}/{name}"),
        None => name,
    };
    // Tab stops are keyed by position, list levels by level.
    if let Some(key) = attr(e, b"w:pos").or_else(|| attr(e, b"w:ilvl")) {
        p = format!("{p}[{key}]");
    }
    p
}

fn add_attrs(path: &str, e: &BytesStart, out: &mut Props) {
    let mut any = false;
    for a in e.attributes().filter_map(|a| a.ok()) {
        any = true;
        let k = a.key.as_ref();
        if IGNORED_ATTRS.contains(&k)
            || k.starts_with(b"w:theme")
            || k.starts_with(b"w:rsid")
            || k.starts_with(b"w14:")
            || k.starts_with(b"w15:")
            || matches!(k, b"w:pos" | b"w:ilvl" | b"w:tplc")
        {
            continue;
        }
        let v = a.unescape_value().map(|v| v.into_owned()).unwrap_or_default();
        let key = if k == b"w:val" { path.to_string() } else { format!("{path}@{}", String::from_utf8_lossy(k)) };
        out.insert(key, v);
    }
    // A bare element (`<w:b/>`) is meaningful by presence; one whose
    // attributes were all ignored is not.
    if !any {
        out.entry(path.to_string()).or_default();
    }
}

fn style_def(r: &mut Reader<&[u8]>, xml: &str) -> Result<StyleDef> {
    let mut def = StyleDef::default();
    loop {
        match r.read_event()? {
            Event::Empty(e) if e.name().as_ref() == b"w:basedOn" => def.based_on = attr(&e, b"w:val"),
            Event::Start(e) => match e.name().as_ref() {
                b"w:basedOn" => {
                    def.based_on = attr(&e, b"w:val");
                    r.read_to_end(e.name())?;
                }
                n @ (b"w:pPr" | b"w:rPr") => {
                    let span = r.read_to_end(e.name())?;
                    let target = if n == b"w:pPr" { &mut def.ppr } else { &mut def.rpr };
                    flatten(&xml[span.start as usize..span.end as usize], target)?;
                }
                _ => {
                    r.read_to_end(e.name())?;
                }
            },
            Event::End(e) if e.name().as_ref() == b"w:style" => return Ok(def),
            Event::Eof => return Err("unexpected end of styles.xml".into()),
            _ => {}
        }
    }
}

/// (major, minor) Latin typefaces from a theme part.
fn theme_fonts(xml: &str) -> Result<(Option<String>, Option<String>)> {
    let mut r = Reader::from_str(xml);
    let (mut major, mut minor) = (None, None);
    let mut section: Option<bool> = None; // Some(true) = major
    loop {
        match r.read_event()? {
            Event::Start(e) => match e.name().as_ref() {
                b"a:majorFont" => section = Some(true),
                b"a:minorFont" => section = Some(false),
                _ => {}
            },
            Event::Empty(e) if e.name().as_ref() == b"a:latin" => match section {
                Some(true) if major.is_none() => major = attr(&e, b"typeface"),
                Some(false) if minor.is_none() => minor = attr(&e, b"typeface"),
                _ => {}
            },
            Event::End(e) if matches!(e.name().as_ref(), b"a:majorFont" | b"a:minorFont") => section = None,
            Event::Eof => return Ok((major, minor)),
            _ => {}
        }
    }
}

/// Map each `w:num` to a hash of its abstract list definition plus overrides.
fn numbering(xml: &str) -> Result<FxHashMap<String, String>> {
    let mut abstracts: FxHashMap<String, u64> = FxHashMap::default();
    let mut nums: Vec<(String, String, u64)> = Vec::new(); // (numId, abstractNumId, overrides)
    let mut r = Reader::from_str(xml);
    let props_hash = |raw: &str| -> Result<u64> {
        let mut p = Props::new();
        flatten(raw, &mut p)?;
        // Identity/template bookkeeping that doesn't change how a list looks.
        p.retain(|k, _| !matches!(k.as_str(), "w:nsid" | "w:tmpl" | "w:name" | "w:styleLink" | "w:numStyleLink" | "w:abstractNumId"));
        let mut h = H::new();
        for (k, v) in &p {
            h.bytes(k.as_bytes());
            h.bytes(v.as_bytes());
        }
        Ok(h.finish())
    };
    loop {
        match r.read_event()? {
            Event::Start(e) if e.name().as_ref() == b"w:abstractNum" => {
                let id = attr(&e, b"w:abstractNumId").unwrap_or_default();
                let span = r.read_to_end(e.name())?;
                abstracts.insert(id, props_hash(&xml[span.start as usize..span.end as usize])?);
            }
            Event::Start(e) if e.name().as_ref() == b"w:num" => {
                let id = attr(&e, b"w:numId").unwrap_or_default();
                let span = r.read_to_end(e.name())?;
                let raw = &xml[span.start as usize..span.end as usize];
                let mut p = Props::new();
                flatten(raw, &mut p)?;
                let abs = p.remove("w:abstractNumId").unwrap_or_default();
                nums.push((id, abs, props_hash(raw)?));
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(nums
        .into_iter()
        .map(|(id, abs, overrides)| {
            let mut h = H::new();
            h.u64(abstracts.get(&abs).copied().unwrap_or_else(|| hash_bytes(abs.as_bytes())));
            h.u64(overrides);
            (id, format!("list:{:016x}", h.finish()))
        })
        .collect())
}
