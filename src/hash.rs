use std::hash::Hasher;

use rustc_hash::FxHasher;

/// Content hasher. Byte strings are terminated with 0xff (never valid UTF-8),
/// so concatenations of different splits hash differently.
#[derive(Default)]
pub struct H(FxHasher);

impl H {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn bytes(&mut self, b: &[u8]) {
        self.0.write(b);
        self.0.write_u8(0xff);
    }

    pub fn u64(&mut self, v: u64) {
        self.0.write_u64(v);
    }

    pub fn finish(&self) -> u64 {
        self.0.finish()
    }
}

pub fn hash_bytes(b: &[u8]) -> u64 {
    let mut h = H::new();
    h.bytes(b);
    h.finish()
}
