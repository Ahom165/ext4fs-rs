//! Bitmaps de blocs et d'inodes (un bit par bloc/inode dans le groupe).

use crate::error::{Error, Result};

pub struct Bitmap {
    pub block_no: u64,
    pub data: alloc::vec::Vec<u8>,
}

impl Bitmap {
    pub fn load(block_no: u64, data: alloc::vec::Vec<u8>) -> Bitmap {
        Bitmap { block_no, data }
    }

    #[inline]
    pub fn test(&self, idx: usize) -> bool {
        (self.data[idx / 8] >> (idx % 8)) & 1 != 0
    }

    #[inline]
    pub fn set(&mut self, idx: usize) {
        self.data[idx / 8] |= 1 << (idx % 8);
    }

    #[inline]
    pub fn clear(&mut self, idx: usize) {
        self.data[idx / 8] &= !(1 << (idx % 8));
    }

    /// Cherche le premier bit à 0 à partir de `start` (borné à `limit`).
    pub fn find_clear(&self, start: usize, limit: usize) -> Option<usize> {
        let mut i = start;
        while i < limit {
            let byte = self.data[i / 8];
            if byte != 0xFF {
                let mut bit = i % 8;
                while bit < 8 {
                    let idx = (i / 8) * 8 + bit;
                    if idx >= limit {
                        return None;
                    }
                    if byte & (1 << bit) == 0 {
                        return Some(idx);
                    }
                    bit += 1;
                }
            }
            i = (i / 8 + 1) * 8;
        }
        None
    }

    /// Compte les bits à 1 (diagnostic).
    pub fn count_set(&self, limit: usize) -> usize {
        (0..limit).filter(|&i| self.test(i)).count()
    }

    pub fn check(&self, limit: usize) -> Result<()> {
        if self.data.len() < (limit + 7) / 8 {
            return Err(Error::Corrupt("bitmap trop court"));
        }
        Ok(())
    }
}
