//! Run xxf over a corpus of real documents and check every redline round trip.
//!
//!   cargo run --release --example corpus -- pairs A.docx B.docx C.docx ...   consecutive versions
//!   cargo run --release --example corpus -- groups DIR    files sharing a "XX123-" prefix, base vs each
//!   cargo run --release --example corpus -- sweep DIR     every file vs itself and vs the next file
//!
//! For each pair: parse, diff, write the redline, then check that it is
//! well-formed, that accepting all changes gives the new document, and that
//! rejecting all changes gives the old document's text. Panics are caught
//! and reported per pair.

use std::collections::BTreeMap;
use std::io::{Cursor, Read};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::time::Instant;

use quick_xml::Reader;
use quick_xml::events::Event;
use xxf::compare::{Stats, diff_blocks, stats};
use xxf::docx::{Source, load_bytes_with};
use xxf::model::Block;
use xxf::parse::Revisions;
use xxf::redline::{Options, write};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let pairs: Vec<(PathBuf, PathBuf)> = match args.first().map(String::as_str) {
        Some("pairs") => args[1..].windows(2).map(|w| (w[0].clone().into(), w[1].clone().into())).collect(),
        Some("groups") => groups(Path::new(&args[1])),
        Some("sweep") => {
            let files = docx_files(Path::new(&args[1]));
            let mut v: Vec<_> = files.iter().map(|f| (f.clone(), f.clone())).collect();
            v.extend(files.windows(2).map(|w| (w[0].clone(), w[1].clone())));
            v
        }
        _ => {
            eprintln!("usage: corpus (pairs FILES... | groups DIR | sweep DIR)");
            std::process::exit(2);
        }
    };
    let verbose = args[0] == "pairs";
    let mut failures: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let (mut ok, mut total) = (0, 0);
    let t = Instant::now();
    for (a, b) in &pairs {
        total += 1;
        let label = format!("{} → {}", name(a), name(b));
        match catch_unwind(AssertUnwindSafe(|| check(a, b))) {
            Ok(Ok(line)) => {
                ok += 1;
                if verbose {
                    println!("{label}\n  {line}");
                }
            }
            Ok(Err(e)) => failures.entry(category(&e)).or_default().push(format!("{label}: {e}")),
            Err(p) => {
                let msg = p.downcast_ref::<String>().cloned().or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()));
                failures.entry("panic".into()).or_default().push(format!("{label}: {}", msg.unwrap_or_default()));
            }
        }
    }
    println!("\n{ok}/{total} pairs passed in {:.1} s", t.elapsed().as_secs_f64());
    for (cat, list) in &failures {
        println!("\n## {cat}: {}", list.len());
        for f in list.iter().take(std::env::var("XXF_SHOW").ok().and_then(|v| v.parse().ok()).unwrap_or(12)) {
            println!("  {}", f.chars().take(300).collect::<String>());
        }
    }
}

fn name(p: &Path) -> String {
    p.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned())
}

fn category(e: &str) -> String {
    e.split(':').next().unwrap_or(e).to_string()
}

fn docx_files(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "docx"))
        .collect();
    v.sort();
    v
}

/// Group "WC001-Foo.docx", "WC001-Bar.docx" by prefix; compare the base
/// (Unmodified/Before/Original, else shortest name) against each other member.
fn groups(dir: &Path) -> Vec<(PathBuf, PathBuf)> {
    let mut g: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    for f in docx_files(dir) {
        let n = name(&f);
        g.entry(n.split('-').next().unwrap_or(&n).to_string()).or_default().push(f);
    }
    let mut out = Vec::new();
    for (_, mut files) in g {
        if files.len() < 2 {
            continue;
        }
        files.sort_by_key(|f| {
            let n = name(f);
            let base = ["Unmodified", "Before", "Original"].iter().any(|k| n.contains(k));
            (!base, n.len())
        });
        for other in &files[1..] {
            out.push((files[0].clone(), other.clone()));
        }
    }
    out
}

/// Paragraph texts in order (descending into tables), compared by text hash
/// so that normalizations the diff applies (e.g. hidden bookmark names) hold.
fn texts(blocks: &[Block], out: &mut Vec<(u64, String)>) {
    for b in blocks {
        match b {
            Block::Para(p) => out.push((p.text_hash, p.text.clone())),
            Block::Table(t) => t.rows.iter().flat_map(|r| &r.cells).for_each(|c| texts(&c.blocks, out)),
            Block::Opaque(_) => {}
        }
    }
}

fn all_texts(s: &Source) -> Vec<(u64, String)> {
    let mut v = Vec::new();
    texts(&s.doc.blocks, &mut v);
    v
}

fn part(bytes: &[u8], name: &str) -> Result<String, String> {
    let mut z = zip::ZipArchive::new(Cursor::new(bytes)).map_err(|e| e.to_string())?;
    let mut s = String::new();
    z.by_name(name).map_err(|e| format!("output: {name}: {e}"))?.read_to_string(&mut s).map_err(|e| e.to_string())?;
    Ok(s)
}

fn check(a: &Path, b: &Path) -> Result<String, String> {
    let (ab, bb) = (std::fs::read(a).map_err(|e| format!("read: {e}"))?, std::fs::read(b).map_err(|e| format!("read: {e}"))?);
    let t0 = Instant::now();
    let old = load_bytes_with(ab, Revisions::Accept).map_err(|e| format!("parse old: {e}"))?;
    let new = load_bytes_with(bb.clone(), Revisions::Accept).map_err(|e| format!("parse new: {e}"))?;
    let t1 = Instant::now();
    let items = diff_blocks(&old.doc.blocks, &new.doc.blocks);
    let t2 = Instant::now();
    let opts = Options { author: "xxf".into(), date: "2026-10-30T00:00:00Z".into() };
    let out = write(&old, &new, &items, &opts).map_err(|e| format!("write: {e}"))?;
    let t3 = Instant::now();

    let xml = part(&out, &new.main)?;
    let mut r = Reader::from_str(&xml);
    loop {
        match r.read_event() {
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(e) => return Err(format!("malformed: at {}: {e}", r.buffer_position())),
        }
    }

    let accepted = load_bytes_with(out.clone(), Revisions::Accept).map_err(|e| format!("parse accepted: {e}"))?;
    let again = diff_blocks(&new.doc.blocks, &accepted.doc.blocks);
    let s = stats(&new.doc.blocks, &accepted.doc.blocks, &again);
    if s != Stats::default() {
        return Err(format!("accept≠new: {s:?}"));
    }
    let new_has_revisions =
        ["<w:ins ", "<w:del ", "<w:moveFrom ", "<w:moveTo ", "PrChange "].iter().any(|t| new.xml.contains(t));
    if !new_has_revisions {
        let rejected = load_bytes_with(out.clone(), Revisions::Reject).map_err(|e| format!("parse rejected: {e}"))?;
        let (want, got) = (all_texts(&old), all_texts(&rejected));
        if want.iter().map(|t| t.0).ne(got.iter().map(|t| t.0)) {
            let i = want.iter().zip(&got).position(|(x, y)| x.0 != y.0).unwrap_or(want.len().min(got.len()));
            return Err(format!(
                "reject≠old: {} vs {} paragraphs; first difference at {i}: {:?} vs {:?}",
                want.len(),
                got.len(),
                want.get(i).map(|s| s.1.chars().take(80).collect::<String>()),
                got.get(i).map(|s| s.1.chars().take(80).collect::<String>())
            ));
        }
    }
    if let Ok(dir) = std::env::var("XXF_KEEP")
        && a != b
    {
        let _ = std::fs::write(Path::new(&dir).join(format!("{}__{}", name(a).trim_end_matches(".docx"), name(b))), &out);
    }
    let s = stats(&old.doc.blocks, &new.doc.blocks, &items);
    Ok(format!(
        "{:.1} MB xml, {} → {} blocks | parse {:.0} ms, diff {:.0} ms, redline {:.0} ms | {} changed, {} ins, {} del, {} moved{}",
        new.xml.len() as f64 / 1e6,
        old.doc.blocks.len(),
        new.doc.blocks.len(),
        (t1 - t0).as_secs_f64() * 1e3,
        (t2 - t1).as_secs_f64() * 1e3,
        (t3 - t2).as_secs_f64() * 1e3,
        s.changed,
        s.inserted,
        s.deleted,
        s.moved,
        if new_has_revisions { " (reject check skipped: new has revisions)" } else { "" }
    ))
}
