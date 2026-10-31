use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use xxf::compare::{diff_blocks, stats};
use xxf::redline;
use xxf::render::{Style, render};

const USAGE: &str = "usage: xxf [--time] [--no-color] [-o REDLINE.docx [--author NAME]] OLD.docx NEW.docx";

fn main() -> ExitCode {
    let mut time = false;
    let mut color = std::io::stdout().is_terminal();
    let mut output: Option<PathBuf> = None;
    let mut author = String::from("xxf");
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--time" => time = true,
            "--no-color" => color = false,
            "-o" | "--output" | "--author" => {
                let Some(v) = args.next() else {
                    eprintln!("{arg} needs a value\n{USAGE}");
                    return ExitCode::from(2);
                };
                if arg == "--author" {
                    author = v;
                } else {
                    output = Some(v.into());
                }
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            _ => paths.push(arg.into()),
        }
    }
    let [old_path, new_path] = paths.as_slice() else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };

    let t0 = Instant::now();
    let (old, new) = std::thread::scope(|s| {
        let old = s.spawn(|| xxf::docx::load(old_path));
        let new = xxf::docx::load(new_path);
        (old.join().expect("loader thread panicked"), new)
    });
    let (old, new) = match (old, new) {
        (Ok(o), Ok(n)) => (o, n),
        (Err(e), _) => return fail(old_path, e),
        (_, Err(e)) => return fail(new_path, e),
    };
    let t1 = Instant::now();
    let items = diff_blocks(&old.doc.blocks, &new.doc.blocks);
    let t2 = Instant::now();

    let style = if color { &Style::COLOR } else { &Style::PLAIN };
    print!("{}", render(&old.doc, &new.doc, &items, style));

    if let Some(out) = &output {
        let opts = redline::Options { author, date: redline::now_iso() };
        let written = redline::write(&old, &new, &items, &opts).and_then(|bytes| Ok(std::fs::write(out, bytes)?));
        if let Err(e) = written {
            return fail(out, e);
        }
        eprintln!("wrote {}", out.display());
    }
    let t3 = Instant::now();

    if time {
        eprintln!(
            "load+parse {:.2} ms, diff {:.2} ms, redline {:.2} ms ({} → {} blocks)",
            (t1 - t0).as_secs_f64() * 1e3,
            (t2 - t1).as_secs_f64() * 1e3,
            if output.is_some() { (t3 - t2).as_secs_f64() * 1e3 } else { 0.0 },
            old.doc.blocks.len(),
            new.doc.blocks.len()
        );
    }
    let s = stats(&old.doc.blocks, &new.doc.blocks, &items);
    if s == Default::default() { ExitCode::SUCCESS } else { ExitCode::from(1) }
}

fn fail(path: &std::path::Path, e: xxf::Error) -> ExitCode {
    eprintln!("xxf: {}: {e}", path.display());
    ExitCode::from(2)
}
