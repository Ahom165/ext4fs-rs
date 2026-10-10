//! Lecteurs/écrivains little-endian et big-endian sur buffers bruts.
//!
//! ext4 est petit-boutiste, jbd2 est gros-boutiste ; tous les accès se font
//! via ces helpers sur `&[u8]` (aucun cast non aligné).

// ---- Little-endian -------------------------------------------------------
#[inline]
pub fn rd16_le(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}
#[inline]
pub fn rd32_le(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}
#[inline]
pub fn rd64_le(b: &[u8], off: usize) -> u64 {
    u64::from_le_bytes([
        b[off],
        b[off + 1],
        b[off + 2],
        b[off + 3],
        b[off + 4],
        b[off + 5],
        b[off + 6],
        b[off + 7],
    ])
}
#[inline]
pub fn wr16_le(b: &mut [u8], off: usize, v: u16) {
    b[off..off + 2].copy_from_slice(&v.to_le_bytes());
}
#[inline]
pub fn wr32_le(b: &mut [u8], off: usize, v: u32) {
    b[off..off + 4].copy_from_slice(&v.to_le_bytes());
}
#[inline]
pub fn wr64_le(b: &mut [u8], off: usize, v: u64) {
    b[off..off + 8].copy_from_slice(&v.to_le_bytes());
}

// ---- Big-endian (jbd2) ---------------------------------------------------
#[inline]
pub fn rd32_be(b: &[u8], off: usize) -> u32 {
    u32::from_be_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}
#[inline]
pub fn rd16_be(b: &[u8], off: usize) -> u16 {
    u16::from_be_bytes([b[off], b[off + 1]])
}
#[inline]
pub fn wr16_be(b: &mut [u8], off: usize, v: u16) {
    b[off..off + 2].copy_from_slice(&v.to_be_bytes());
}
#[inline]
pub fn rd64_be(b: &[u8], off: usize) -> u64 {
    u64::from_be_bytes([
        b[off],
        b[off + 1],
        b[off + 2],
        b[off + 3],
        b[off + 4],
        b[off + 5],
        b[off + 6],
        b[off + 7],
    ])
}
#[inline]
pub fn wr32_be(b: &mut [u8], off: usize, v: u32) {
    b[off..off + 4].copy_from_slice(&v.to_be_bytes());
}
#[inline]
pub fn wr64_be(b: &mut [u8], off: usize, v: u64) {
    b[off..off + 8].copy_from_slice(&v.to_be_bytes());
}

/// UUID v4 lisible (format 8-4-4-4-12).
pub fn uuid_to_str(u: &[u8; 16]) -> alloc::string::String {
    let hex = |b: u8| -> [char; 2] {
        const D: &[u8; 16] = b"0123456789abcdef";
        [D[(b >> 4) as usize] as char, D[(b & 0xF) as usize] as char]
    };
    let mut s = alloc::string::String::with_capacity(36);
    let push = |s: &mut alloc::string::String, b: u8| {
        let h = hex(b);
        s.push(h[0]);
        s.push(h[1]);
    };
    for (i, &b) in u.iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            s.push('-');
        }
        push(&mut s, b);
    }
    s
}

/// Heure Unix (secondes, nanosecondes) fournie par l'hôte (no_std : le
/// consommateur — driver Windows ou CLI — implémente [`TimeSource`]).
pub trait TimeSource {
    fn now(&self) -> (u32, u32);
}

/// Source de temps figée (tests / lecture seule).
pub struct FixedTime(pub u32);
impl TimeSource for FixedTime {
    fn now(&self) -> (u32, u32) {
        (self.0, 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endian_roundtrip() {
        let mut b = [0u8; 16];
        wr32_le(&mut b, 0, 0xEF53_1234);
        assert_eq!(rd32_le(&b, 0), 0xEF53_1234);
        wr64_le(&mut b, 4, 0x1122_3344_5566_7788);
        assert_eq!(rd64_le(&b, 4), 0x1122_3344_5566_7788);
        wr32_be(&mut b, 8, 0xC03B_3998);
        assert_eq!(rd32_be(&b, 8), 0xC03B_3998);
    }

    #[test]
    fn uuid_format() {
        let u = [
            0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF, 0x01, 0x23, 0x45, 0x67, 0x89, 0xAB,
            0xCD, 0xEF,
        ];
        assert_eq!(uuid_to_str(&u), "01234567-89ab-cdef-0123-456789abcdef");
    }
}
