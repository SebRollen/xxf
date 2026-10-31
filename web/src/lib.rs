//! WebAssembly entry points for the xxf web page (`web/index.html`).
//!
//! Plain `extern "C"` exports rather than wasm-bindgen, so building needs
//! nothing but cargo. The page:
//! 1. copies each document into wasm memory with `alloc` (ownership passes to
//!    `compare`, which frees them);
//! 2. calls `compare`;
//! 3. reads each result with `result_ptr` / `result_len`.

use std::cell::RefCell;

use xxf::compare::{diff_blocks, stats};
use xxf::docx::load_bytes_with;
use xxf::parse::Revisions;
use xxf::redline::{Options, write};
use xxf::render::{Style, render};

/// Result slots, by index.
const REPORT_HTML: usize = 0;
const REDLINE_DOCX: usize = 1;
const STATS_JSON: usize = 2;
const ERROR: usize = 3;
const SLOTS: usize = 4;

thread_local! {
    static RESULTS: RefCell<Vec<Vec<u8>>> = RefCell::new(vec![Vec::new(); SLOTS]);
}

/// Allocate `len` bytes for the page to fill.
#[unsafe(no_mangle)]
pub extern "C" fn alloc(len: usize) -> *mut u8 {
    let mut v = Vec::<u8>::with_capacity(len);
    let p = v.as_mut_ptr();
    std::mem::forget(v);
    p
}

/// Free a buffer from `alloc` that was not passed to `compare`.
///
/// # Safety
/// `ptr` and `len` must come from one `alloc` call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dealloc(ptr: *mut u8, len: usize) {
    drop(unsafe { Vec::from_raw_parts(ptr, 0, len) });
}

/// Compare two .docx files. Takes ownership of all four buffers (each from
/// `alloc`, filled to its length). Returns 1 on success; on failure 0, with
/// the message in result slot `ERROR`. The redline is written only if
/// `want_redline` is non-zero.
///
/// # Safety
/// Each pointer/length pair must come from one `alloc` call, fully written.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn compare(
    old_ptr: *mut u8,
    old_len: usize,
    new_ptr: *mut u8,
    new_len: usize,
    author_ptr: *mut u8,
    author_len: usize,
    date_ptr: *mut u8,
    date_len: usize,
    want_redline: u32,
) -> u32 {
    let take = |p: *mut u8, n: usize| unsafe { Vec::from_raw_parts(p, n, n) };
    let (old, new) = (take(old_ptr, old_len), take(new_ptr, new_len));
    let author = String::from_utf8_lossy(&take(author_ptr, author_len)).into_owned();
    let date = String::from_utf8_lossy(&take(date_ptr, date_len)).into_owned();
    let (results, ok) = match run(old, new, author, date, want_redline != 0) {
        Ok(r) => (r, 1),
        Err(e) => {
            let mut r = vec![Vec::new(); SLOTS];
            r[ERROR] = e.to_string().into_bytes();
            (r, 0)
        }
    };
    RESULTS.with(|r| *r.borrow_mut() = results);
    ok
}

fn run(old: Vec<u8>, new: Vec<u8>, author: String, date: String, want_redline: bool) -> xxf::Result<Vec<Vec<u8>>> {
    let old = load_bytes_with(old, Revisions::Accept).map_err(|e| format!("old document: {e}"))?;
    let new = load_bytes_with(new, Revisions::Accept).map_err(|e| format!("new document: {e}"))?;
    let items = diff_blocks(&old.doc.blocks, &new.doc.blocks);
    let mut results = vec![Vec::new(); SLOTS];
    results[REPORT_HTML] = render(&old.doc, &new.doc, &items, &Style::HTML).into_bytes();
    let s = stats(&old.doc.blocks, &new.doc.blocks, &items);
    results[STATS_JSON] = format!(
        r#"{{"changed":{},"inserted":{},"deleted":{},"moved":{},"blank":{},"oldBlocks":{},"newBlocks":{}}}"#,
        s.changed,
        s.inserted,
        s.deleted,
        s.moved,
        s.blank,
        old.doc.blocks.len(),
        new.doc.blocks.len()
    )
    .into_bytes();
    if want_redline {
        results[REDLINE_DOCX] = write(&old, &new, &items, &Options { author, date })?;
    }
    Ok(results)
}

/// Address of result slot `i` (valid until the next `compare`).
#[unsafe(no_mangle)]
pub extern "C" fn result_ptr(i: usize) -> *const u8 {
    RESULTS.with(|r| r.borrow().get(i).map_or(std::ptr::null(), |v| v.as_ptr()))
}

#[unsafe(no_mangle)]
pub extern "C" fn result_len(i: usize) -> usize {
    RESULTS.with(|r| r.borrow().get(i).map_or(0, Vec::len))
}
