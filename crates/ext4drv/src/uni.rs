//! Conversions UTF-16 ↔ UTF-8 sans std (noms de fichiers Windows ↔ ext4).

use alloc::string::String;
use alloc::vec::Vec;

/// UTF-16 (little endian, sans terminateur) → UTF-8 (lossy : U+FFFD).
pub fn utf16_to_utf8(wide: &[u16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(wide.len());
    let mut i = 0usize;
    while i < wide.len() {
        let wc = wide[i];
        i += 1;
        if (0xD800..0xDC00).contains(&wc) {
            // paire de surrogates haute
            if i < wide.len() && (0xDC00..0xE000).contains(&wide[i]) {
                let lo = wide[i];
                i += 1;
                let cp = 0x1_0000u32 + (((wc as u32 - 0xD800) << 10) | (lo as u32 - 0xDC00));
                push_utf8(&mut out, cp);
            } else {
                push_utf8(&mut out, 0xFFFD);
            }
        } else if (0xDC00..0xE000).contains(&wc) {
            push_utf8(&mut out, 0xFFFD); // surrogate basse orpheline
        } else {
            push_utf8(&mut out, wc as u32);
        }
    }
    out
}

/// UTF-8 → UTF-16 (lossy : U+FFFD par octet invalide).
pub fn utf8_to_utf16(bytes: &[u8]) -> Vec<u16> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        let (cp, adv): (u32, usize) = if b < 0x80 {
            (b as u32, 1)
        } else if (0xC0..0xE0).contains(&b) && i + 1 < bytes.len() {
            ((((b as u32) & 0x1F) << 6) | ((bytes[i + 1] as u32) & 0x3F), 2)
        } else if (0xE0..0xF0).contains(&b) && i + 2 < bytes.len() {
            (
                (((b as u32) & 0x0F) << 12)
                    | (((bytes[i + 1] as u32) & 0x3F) << 6)
                    | ((bytes[i + 2] as u32) & 0x3F),
                3,
            )
        } else if (0xF0..0xF8).contains(&b) && i + 3 < bytes.len() {
            (
                (((b as u32) & 0x07) << 18)
                    | (((bytes[i + 1] as u32) & 0x3F) << 12)
                    | (((bytes[i + 2] as u32) & 0x3F) << 6)
                    | ((bytes[i + 3] as u32) & 0x3F),
                4,
            )
        } else {
            (0xFFFD, 1)
        };
        i += adv;
        match cp {
            0..=0xFFFF => out.push(cp as u16),
            _ => {
                let c = cp - 0x1_0000;
                out.push(0xD800 + (c >> 10) as u16);
                out.push(0xDC00 + (c & 0x3FF) as u16);
            }
        }
    }
    out
}

fn push_utf8(out: &mut Vec<u8>, cp: u32) {
    match cp {
        0..=0x7F => out.push(cp as u8),
        0x80..=0x7FF => {
            out.push(0xC0 | (cp >> 6) as u8);
            out.push(0x80 | (cp & 0x3F) as u8);
        }
        0x800..=0xFFFF => {
            out.push(0xE0 | (cp >> 12) as u8);
            out.push(0x80 | ((cp >> 6) & 0x3F) as u8);
            out.push(0x80 | (cp & 0x3F) as u8);
        }
        _ => {
            out.push(0xF0 | (cp >> 18) as u8);
            out.push(0x80 | ((cp >> 12) & 0x3F) as u8);
            out.push(0x80 | ((cp >> 6) & 0x3F) as u8);
            out.push(0x80 | (cp & 0x3F) as u8);
        }
    }
}

/// Aide de debug : UTF-16 lossy → String.
pub fn wide_to_string(wide: &[u16]) -> String {
    String::from_utf8_lossy(&utf16_to_utf8(wide)).into_owned()
}
