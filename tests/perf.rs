//! Timing on a large synthetic document: `cargo test --release --test perf -- --ignored --nocapture`

mod common;

use std::time::Instant;

use common::{docx, p};
use xxf::compare::{diff_blocks, stats};
use xxf::docx::load_bytes;

const WORDS: &[&str] = &[
    "the", "agreement", "party", "shall", "notice", "within", "days", "of", "receipt", "and", "any",
    "obligation", "hereunder", "provided", "that", "such", "term", "may", "be", "extended", "by",
    "written", "consent", "licensor", "licensee", "territory", "fee", "payment", "invoice", "breach",
];

#[test]
#[ignore]
fn large_document() {
    let mut s = 0x2545f4914f6cdd1du64;
    let mut rnd = move |n: u64| {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s % n
    };
    let n = 20_000;
    let paras: Vec<String> = (0..n)
        .map(|i| {
            let len = 15 + rnd(30);
            let words: Vec<&str> = (0..len).map(|_| WORDS[rnd(WORDS.len() as u64) as usize]).collect();
            format!("{i}. {}.", words.join(" "))
        })
        .collect();
    // ~1% edited, 0.5% deleted, 0.5% inserted, 20 moved.
    let mut new = paras.clone();
    for _ in 0..n / 100 {
        let i = rnd(new.len() as u64) as usize;
        new[i] = new[i].replacen(" the ", " a ", 1).replacen(" shall ", " must ", 1);
    }
    for _ in 0..n / 200 {
        new.remove(rnd(new.len() as u64) as usize);
    }
    for k in 0..n / 200 {
        new.insert(rnd(new.len() as u64) as usize, format!("Inserted clause {k} about payment terms."));
    }
    for _ in 0..20 {
        let p = new.remove(rnd(new.len() as u64) as usize);
        new.insert(rnd(new.len() as u64) as usize, p);
    }
    let a_bytes = docx(&paras.iter().map(|t| p(t)).collect::<String>());
    let b_bytes = docx(&new.iter().map(|t| p(t)).collect::<String>());

    let t = Instant::now();
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(&a_bytes)).unwrap();
    let mut xml = String::new();
    std::io::Read::read_to_string(&mut zip.by_name("word/document.xml").unwrap(), &mut xml).unwrap();
    let unzip = t.elapsed();
    let t = Instant::now();
    xxf::parse::parse_document(&xml, &Default::default(), &Default::default()).unwrap();
    println!(
        "one doc: {:.1} MB xml, unzip {:.1} ms, parse {:.1} ms",
        xml.len() as f64 / 1e6,
        unzip.as_secs_f64() * 1e3,
        t.elapsed().as_secs_f64() * 1e3
    );

    let t0 = Instant::now();
    let a = load_bytes(&a_bytes).unwrap().doc;
    let b = load_bytes(&b_bytes).unwrap().doc;
    let t1 = Instant::now();
    let items = diff_blocks(&a.blocks, &b.blocks);
    let t2 = Instant::now();
    let old_src = load_bytes(&a_bytes).unwrap();
    let new_src = load_bytes(&b_bytes).unwrap();
    let t = Instant::now();
    let opts = xxf::redline::Options { author: "perf".into(), date: "2026-01-01T00:00:00Z".into() };
    let out = xxf::redline::write(&old_src, &new_src, &items, &opts).unwrap();
    println!("redline {:.1} ms ({:.1} MB zipped)", t.elapsed().as_secs_f64() * 1e3, out.len() as f64 / 1e6);
    println!(
        "{} paras, {:.1} MB zipped each: load {:.1} ms (both), diff {:.1} ms; {:?}",
        n,
        a_bytes.len() as f64 / 1e6,
        (t1 - t0).as_secs_f64() * 1e3,
        (t2 - t1).as_secs_f64() * 1e3,
        stats(&a.blocks, &b.blocks, &items)
    );
}
