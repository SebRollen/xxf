use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use xxf::compare::diff_blocks;
use xxf::render::{Style, render};

const USAGE: &str = "usage: xxf [--time] [--no-color] OLD.docx NEW.docx";

fn main() -> ExitCode {
    let mut time = false;
    let mut color = std::io::stdout().is_terminal();
    let mut paths: Vec<PathBuf> = Vec::new();
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--time" => time = true,
            "--no-color" => color = false,
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
    let items = diff_blocks(&old.blocks, &new.blocks);
    let t2 = Instant::now();

    let style = if color { &Style::COLOR } else { &Style::PLAIN };
    print!("{}", render(&old, &new, &items, style));
    if time {
        eprintln!(
            "load+parse {:.2} ms, diff {:.2} ms ({} → {} blocks)",
            (t1 - t0).as_secs_f64() * 1e3,
            (t2 - t1).as_secs_f64() * 1e3,
            old.blocks.len(),
            new.blocks.len()
        );
    }
    let s = xxf::compare::stats(&old.blocks, &new.blocks, &items);
    if s == Default::default() { ExitCode::SUCCESS } else { ExitCode::from(1) }
}

fn fail(path: &std::path::Path, e: xxf::Error) -> ExitCode {
    eprintln!("xxf: {}: {e}", path.display());
    ExitCode::from(2)
}
