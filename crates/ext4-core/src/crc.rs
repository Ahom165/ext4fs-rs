//! CRC32C (polynôme réfléchi 0x82F63B78, celui d'ext4/jbd2).
//!
//! Convention de flux : `crc32c_update` prend un état courant (initialiser
//! avec [`crc32c_init`]), `crc32c_finish` applique le complément final.
//! Les checksums ext4/jbd2 travaillent tous en "état courant" sans
//! complément intermédiaire ; les seeds (ex. `s_csum_seed`) sont des états
//! courants dérivés de l'UUID.

const POLY: u32 = 0x82F6_3B78;

const fn make_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { POLY ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
}

static TABLE: [u32; 256] = make_table();

/// État initial d'un flux CRC32C.
#[inline]
pub const fn crc32c_init() -> u32 {
    0xFFFF_FFFF
}

/// Avance le flux CRC32C sur `data`.
#[inline]
pub fn crc32c_update(state: u32, data: &[u8]) -> u32 {
    let mut c = state;
    for &b in data {
        c = TABLE[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
    }
    c
}

/// Complément final d'un flux CRC32C.
#[inline]
pub const fn crc32c_finish(state: u32) -> u32 {
    state ^ 0xFFFF_FFFF
}

/// CRC32C complet d'un buffer (init + update + finish).
#[inline]
pub fn crc32c(data: &[u8]) -> u32 {
    crc32c_finish(crc32c_update(crc32c_init(), data))
}

/// CRC16 (poly 0x1005 réfléchi 0xA001) — checksum legacy GDT_CSUM (non
/// utilisé avec metadata_csum mais conservé pour diagnostiquer d'anciens FS).
pub fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0;
    for &b in data {
        crc = (crc >> 8) | ((crc as u8 ^ b) as u16) << 8;
        let mut x = crc ^ (crc >> 8);
        x ^= x >> 4;
        crc ^= (x << 12) ^ (x << 5) ^ x;
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32c_known_vectors() {
        // Vecteurs standard CRC-32C (iSCSI)
        assert_eq!(crc32c(b""), 0x0000_0000);
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
        assert_eq!(crc32c(b"a"), 0xC1D0_4330);
        assert_eq!(crc32c(b"abc"), 0x364B_3FB7);
    }

    #[test]
    fn crc32c_stream_matches_oneshot() {
        let data = b"bonjour le monde ext4 metadata_csum";
        let mut st = crc32c_init();
        st = crc32c_update(st, &data[..10]);
        st = crc32c_update(st, &data[10..]);
        assert_eq!(crc32c_finish(st), crc32c(data));
    }
}
