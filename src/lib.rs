//! xxf: fast structural diff for Word (OOXML) documents.
//!
//! Pipeline: unzip → parse + normalize `word/document.xml` into a block model
//! (paragraphs, tables, opaque blocks) → hash-anchored alignment of blocks with
//! similarity pairing and move detection → word-level diff inside changed
//! paragraphs.

pub mod align;
pub mod compare;
pub mod diff;
pub mod docx;
pub mod hash;
pub mod model;
pub mod parse;
pub mod render;
pub mod styles;

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = std::result::Result<T, Error>;
