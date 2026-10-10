//! Répertoires ext4 : blocs linéaires, HTree (lecture universelle,
//! insertion via hash dx), checksums de blocs (tail metadata_csum).

use crate::error::{Error, Result};
use crate::gdt;
use crate::superblock::Superblock;
use crate::util::*;

pub const EXT4_INDEX_FL: u32 = 0x1000;
pub const FT_DIR_CSUM: u8 = 0xDE;
pub const DIRENT_TAIL_SIZE: usize = 12;
pub const DX_TAIL_SIZE: usize = 8;

/// Entrée de répertoire lue.
#[derive(Clone, Debug)]
pub struct DirEntry {
    pub inode: u32,
    pub file_type: u8,
    pub name: alloc::vec::Vec<u8>,
}

/// Seed par inode pour les checksums : crc32c(crc32c(s_csum_seed, ino), gen).
pub fn inode_csum_seed(sb: &Superblock, ino: u32, generation: u32) -> u32 {
    let c = crate::crc::crc32c_update(sb.csum_seed, &ino.to_le_bytes());
    crate::crc::crc32c_update(c, &generation.to_le_bytes())
}

/// Recalcule le tail de checksum d'un bloc de répertoires (12 derniers octets).
/// `dir_seed` = seed par inode.
pub fn dir_block_csum_set(block: &mut [u8], dir_seed: u32) {
    let bs = block.len();
    if bs < DIRENT_TAIL_SIZE + 8 {
        return;
    }
    let csum_area = &block[..bs - 4]; // checksum couvre le bloc moins les 4 o finaux
    let mut c = dir_seed;
    c = crate::crc::crc32c_update(c, &csum_area[..bs - DIRENT_TAIL_SIZE]);
    // convention kernel : le checksum du tail lui-même est exclu, calculé sur
    // [0, bs-tail) — voir ext4_dirblock_csum(size = (char*)t - b_data)
    let _ = csum_area;
    wr32_le(block, bs - 4, c);
}

/// Vérifie le checksum d'un bloc de répertoires.
pub fn dir_block_csum_verify(block: &[u8], dir_seed: u32) -> bool {
    let bs = block.len();
    if bs < DIRENT_TAIL_SIZE + 8 {
        return true;
    }
    let stored = rd32_le(block, bs - 4);
    let mut c = dir_seed;
    c = crate::crc::crc32c_update(c, &block[..bs - DIRENT_TAIL_SIZE]);
    stored == c
}

/// Pose le tail de checksum (fake dirent de 12 octets) si metadata_csum.
pub fn write_dirent_tail(block: &mut [u8], dir_seed: u32, metadata_csum: bool) {
    let bs = block.len();
    if !metadata_csum {
        return;
    }
    let t = bs - DIRENT_TAIL_SIZE;
    block[t..t + DIRENT_TAIL_SIZE].fill(0);
    wr32_le(block, t, 0); // det_reserved_zero1
    wr16_le(block, t + 4, DIRENT_TAIL_SIZE as u16); // det_rec_len
    block[t + 6] = 0; // det_reserved_zero2
    block[t + 7] = FT_DIR_CSUM; // det_reserved_ft
    let c = {
        let mut c = dir_seed;
        c = crate::crc::crc32c_update(c, &block[..t]);
        c
    };
    wr32_le(block, t + 8, c);
}

// ---------------------------------------------------------------------------
// Hash HTree (half_md4 / tea, signé et non signé) — portage de fs/ext4/hash.c
// ---------------------------------------------------------------------------

const DELTA: u32 = 0x9E37_79B9;

fn tea_transform(buf: &mut [u32; 4], input: &[u32; 4]) {
    let mut sum: u32 = 0;
    let mut b0 = buf[0];
    let mut b1 = buf[1];
    let (a, b, c, d) = (input[0], input[1], input[2], input[3]);
    let mut n = 16;
    loop {
        sum = sum.wrapping_add(DELTA);
        b0 = b0.wrapping_add(((b1 << 4).wrapping_add(a)) ^ b1.wrapping_add(sum) ^ ((b1 >> 5).wrapping_add(b)));
        b1 = b1.wrapping_add(((b0 << 4).wrapping_add(c)) ^ b0.wrapping_add(sum) ^ ((b0 >> 5).wrapping_add(d)));
        n -= 1;
        if n == 0 {
            break;
        }
    }
    buf[0] = buf[0].wrapping_add(b0);
    buf[1] = buf[1].wrapping_add(b1);
}

fn rol32(x: u32, n: u32) -> u32 {
    x.rotate_left(n)
}

fn half_md4_transform(buf: &mut [u32; 4], input: &[u32; 8]) -> u32 {
    let f = |x: u32, y: u32, z: u32| z ^ (x & (y ^ z));
    let g = |x: u32, y: u32, z: u32| (x & y).wrapping_add((x ^ y) & z);
    let h = |x: u32, y: u32, z: u32| x ^ y ^ z;
    let k2: u32 = 0x5A82_7999; // 013240474631 octal
    let k3: u32 = 0x6ED9_EBA1; // 015666365641 octal
    let (mut a, mut b, mut c, mut d) = (buf[0], buf[1], buf[2], buf[3]);
    macro_rules! round {
        ($f:ident, $a:ident, $b:ident, $c:ident, $d:ident, $x:expr, $s:expr) => {
            $a = $a
                .wrapping_add($f($b, $c, $d))
                .wrapping_add($x);
            $a = rol32($a, $s);
        };
    }
    // Round 1
    round!(f, a, b, c, d, input[0], 3);
    round!(f, d, a, b, c, input[1], 7);
    round!(f, c, d, a, b, input[2], 11);
    round!(f, b, c, d, a, input[3], 19);
    round!(f, a, b, c, d, input[4], 3);
    round!(f, d, a, b, c, input[5], 7);
    round!(f, c, d, a, b, input[6], 11);
    round!(f, b, c, d, a, input[7], 19);
    // Round 2
    round!(g, a, b, c, d, input[1].wrapping_add(k2), 3);
    round!(g, d, a, b, c, input[3].wrapping_add(k2), 5);
    round!(g, c, d, a, b, input[5].wrapping_add(k2), 9);
    round!(g, b, c, d, a, input[7].wrapping_add(k2), 13);
    round!(g, a, b, c, d, input[0].wrapping_add(k2), 3);
    round!(g, d, a, b, c, input[2].wrapping_add(k2), 5);
    round!(g, c, d, a, b, input[4].wrapping_add(k2), 9);
    round!(g, b, c, d, a, input[6].wrapping_add(k2), 13);
    // Round 3
    round!(h, a, b, c, d, input[3].wrapping_add(k3), 3);
    round!(h, d, a, b, c, input[7].wrapping_add(k3), 9);
    round!(h, c, d, a, b, input[2].wrapping_add(k3), 11);
    round!(h, b, c, d, a, input[6].wrapping_add(k3), 15);
    round!(h, a, b, c, d, input[1].wrapping_add(k3), 3);
    round!(h, d, a, b, c, input[5].wrapping_add(k3), 9);
    round!(h, c, d, a, b, input[0].wrapping_add(k3), 11);
    round!(h, b, c, d, a, input[4].wrapping_add(k3), 15);
    buf[0] = buf[0].wrapping_add(a);
    buf[1] = buf[1].wrapping_add(b);
    buf[2] = buf[2].wrapping_add(c);
    buf[3] = buf[3].wrapping_add(d);
    buf[1]
}

fn str2hashbuf(msg: &[u8], num: usize, unsigned: bool) -> alloc::vec::Vec<u32> {
    let mut out = alloc::vec![0u32; num];
    let len = msg.len();
    let pad: u32 = {
        let p = (len as u32) | ((len as u32) << 8);
        p | (p << 16)
    };
    let mut m = msg;
    if m.len() > num * 4 {
        m = &m[..num * 4];
    }
    let mut idx = 0usize;
    let mut buf_i = 0usize;
    while m.len() - idx >= 4 && buf_i < num {
        let v = if unsigned {
            u32::from_be_bytes([m[idx], m[idx + 1], m[idx + 2], m[idx + 3]])
        } else {
            let s = |b: u8| b as i8 as u32;
            ((s(m[idx]) << 24) as u32)
                .wrapping_add((s(m[idx + 1]) << 16) as u32)
                .wrapping_add((s(m[idx + 2]) << 8) as u32)
                .wrapping_add(s(m[idx + 3]) as u32)
        };
        out[buf_i] = v;
        buf_i += 1;
        idx += 4;
    }
    let rest = m.len() - idx;
    let mut val: u32 = pad;
    for i in 0..rest {
        let b = if unsigned {
            m[idx + i] as u32
        } else {
            m[idx + i] as i8 as u32
        };
        val = b.wrapping_add(val << 8);
    }
    if buf_i < num {
        out[buf_i] = val;
        buf_i += 1;
    }
    while buf_i < num {
        out[buf_i] = pad;
        buf_i += 1;
    }
    out
}

pub const DX_HASH_LEGACY: u8 = 0;
pub const DX_HASH_HALF_MD4: u8 = 1;
pub const DX_HASH_TEA: u8 = 2;
pub const DX_HASH_HALF_MD4_UNSIGNED: u8 = 4;
pub const DX_HASH_TEA_UNSIGNED: u8 = 5;
pub const HTREE_EOF_32BIT: u32 = 0x7FFF_FFFF;

/// Hash d'un nom pour l'arbre dx (seed = s_hash_seed).
pub fn dx_hash(hash_version: u8, name: &[u8], seed: &[u32; 4]) -> u32 {
    let mut buf: [u32; 4] = [0x6745_2301, 0xEFCD_AB89, 0x98BA_DCFE, 0x1032_5476];
    let nonzero_seed = seed.iter().any(|&s| s != 0);
    if nonzero_seed {
        buf.copy_from_slice(seed);
    }
    let mut hash;
    let mut minor = 0u32;
    match hash_version {
        DX_HASH_HALF_MD4 | DX_HASH_HALF_MD4_UNSIGNED => {
            let unsigned = hash_version == DX_HASH_HALF_MD4_UNSIGNED;
            let mut p = name;
            while !p.is_empty() {
                let input = str2hashbuf(p, 8, unsigned);
                minor = half_md4_transform(&mut buf, &input.as_slice().try_into().unwrap());
                p = if p.len() > 32 { &p[32..] } else { &[] };
            }
            hash = buf[1];
        }
        DX_HASH_TEA | DX_HASH_TEA_UNSIGNED => {
            let unsigned = hash_version == DX_HASH_TEA_UNSIGNED;
            let mut p = name;
            while !p.is_empty() {
                let input = str2hashbuf(p, 4, unsigned);
                tea_transform(&mut buf, &input.as_slice().try_into().unwrap());
                minor = buf[1];
                p = if p.len() > 16 { &p[16..] } else { &[] };
            }
            hash = buf[0];
        }
        DX_HASH_LEGACY | _ => {
            // legacy : non utilisé par mkfs modernes
            let mut hash0: u32 = 0x12A3_FE2D;
            let mut hash1: u32 = 0x37AB_E8F9;
            for &b in name {
                let v = if hash_version == DX_HASH_LEGACY {
                    (b as i8 as i32 as u32).wrapping_mul(7152_373)
                } else {
                    (b as u32).wrapping_mul(7152_373)
                };
                let mut hv = hash1 ^ (hash0 ^ v);
                if hv & 0x8000_0000 != 0 {
                    hv = hv.wrapping_sub(0x7FFF_FFFF);
                }
                hash1 = hash0;
                hash0 = hv;
            }
            hash = hash0 << 1;
            hash &= !1u32;
            return hash;
        }
    }
    hash &= !1u32;
    if hash == HTREE_EOF_32BIT << 1 {
        hash = (HTREE_EOF_32BIT - 1) << 1;
    }
    let _ = minor;
    hash
}

// ---------------------------------------------------------------------------
// Parse/écriture des blocs de répertoires
// ---------------------------------------------------------------------------

pub const DE_HEADER: usize = 8;

#[inline]
fn dirent_min_size(name_len: usize, has_filetype: bool) -> usize {
    let base = DE_HEADER + if has_filetype { name_len } else { name_len };
    let rounded = (base + 3) & !3;
    rounded
}

/// Itère les entrées réelles d'un bloc de répertoires (taille `usable`,
/// hors tail). Rappel entries avec (offset, inode, rec_len, name_len, ftype).
pub struct DirBlockIter<'a> {
    pub block: &'a [u8],
    pub offset: usize,
    pub usable: usize,
}

impl<'a> DirBlockIter<'a> {
    pub fn new(block: &'a [u8], metadata_csum: bool) -> DirBlockIter<'a> {
        let usable = if metadata_csum && block.len() >= DIRENT_TAIL_SIZE {
            block.len() - DIRENT_TAIL_SIZE
        } else {
            block.len()
        };
        DirBlockIter {
            block,
            offset: 0,
            usable,
        }
    }

    pub fn next_entry(&mut self) -> Option<Result<(usize, u32, u16, u8, u8)>> {
        if self.offset >= self.usable {
            return None;
        }
        let off = self.offset;
        let inode = rd32_le(self.block, off);
        let rec_len = rd16_le(self.block, off + 4) as usize;
        if rec_len < 4 || off + rec_len > self.usable {
            return Some(Err(Error::Corrupt("rec_len de dirent invalide")));
        }
        let name_len = self.block[off + 6] as usize;
        let ftype = self.block[off + 7];
        self.offset += rec_len;
        Some(Ok((off, inode, rec_len as u16, name_len as u8, ftype)))
    }
}

/// Insère une entrée dans un bloc de répertoires. Retourne Ok(true) si
/// inséré, Ok(false) si pas de place.
pub fn insert_entry(
    block: &mut [u8],
    name: &[u8],
    inode_no: u32,
    ftype: u8,
    has_filetype: bool,
    metadata_csum: bool,
) -> Result<bool> {
    let bs = block.len();
    let usable = if metadata_csum { bs - DIRENT_TAIL_SIZE } else { bs };
    let needed = dirent_min_size(name.len(), has_filetype);
    // Scan : dernière vraie entrée + éventuel trou (inode 0) réutilisable
    let mut off = 0usize;
    let mut last_off: Option<usize> = None;
    let mut last_end = 0usize;
    let mut hole: Option<(usize, usize)> = None; // (off, rec_len)
    while off < usable {
        let inode = rd32_le(block, off);
        let rec_len = rd16_le(block, off + 4) as usize;
        if rec_len < 4 || off + rec_len > usable {
            return Err(Error::Corrupt("rec_len de dirent invalide (insert)"));
        }
        let name_len = block[off + 6] as usize;
        if inode == 0 {
            let min_here = if has_filetype { 12 } else { 8 };
            let _ = name_len;
            let _ = min_here;
            if hole.is_none() {
                hole = Some((off, rec_len));
            }
        } else {
            last_off = Some(off);
            last_end = off + rec_len;
            hole = None;
        }
        off += rec_len;
    }
    // Réutilisation d'un trou
    if let (Some((h_off, h_len)), None) = (hole, last_off) {
        // bloc vide (que des trous) : insert au premier trou
        if h_len >= needed {
            let rest = h_len - needed;
            let name_len = name.len();
            wr32_le(block, h_off, inode_no);
            wr16_le(block, h_off + 4, needed as u16);
            block[h_off + 6] = name_len as u8;
            block[h_off + 7] = ftype;
            block[h_off + DE_HEADER..h_off + DE_HEADER + name_len].copy_from_slice(name);
            // reliquat : une entrée vide doit tenir ENTIÈREMENT avant le tail
            // (header 8 o) — sinon fit exact (rec_len = h_len)
            if rest >= 8 {
                wr32_le(block, h_off + needed, 0);
                wr16_le(block, h_off + needed + 4, rest as u16);
                block[h_off + needed + 6] = 0;
                block[h_off + needed + 7] = 0;
            }
            return Ok(true);
        }
        return Ok(false);
    }
    if let Some((h_off, h_len)) = hole {
        // trou entre deux vraies entrées
        if h_len >= needed + 8 {
            let rest = h_len - needed;
            let name_len = name.len();
            wr32_le(block, h_off, inode_no);
            wr16_le(block, h_off + 4, needed as u16);
            block[h_off + 6] = name_len as u8;
            block[h_off + 7] = ftype;
            block[h_off + DE_HEADER..h_off + DE_HEADER + name_len].copy_from_slice(name);
            wr32_le(block, h_off + needed, 0);
            wr16_le(block, h_off + needed + 4, rest as u16);
            block[h_off + needed + 6] = 0;
            block[h_off + needed + 7] = 0;
            return Ok(true);
        }
        if h_len >= needed {
            // fit exact : l'entrée avale tout le trou (pas de reliquat)
            let name_len = name.len();
            wr32_le(block, h_off, inode_no);
            wr16_le(block, h_off + 4, h_len as u16);
            block[h_off + 6] = name_len as u8;
            block[h_off + 7] = ftype;
            block[h_off + DE_HEADER..h_off + DE_HEADER + name_len].copy_from_slice(name);
            return Ok(true);
        }
        return Ok(false);
    }
    // Insertion en fin : shrink du rec_len de la dernière entrée vivante
    let last = last_off.ok_or(Error::Corrupt("bloc de répertoire vide sans trou"))?;
    let last_rec = rd16_le(block, last + 4) as usize;
    let last_min = {
        let nl = block[last + 6] as usize;
        dirent_min_size(nl, has_filetype)
    };
    if last_rec - last_min < needed {
        return Ok(false);
    }
    let name_len = name.len();
    let new_off = last + last_min;
    let _ = last_end;
    wr16_le(block, last + 4, last_min as u16);
    wr32_le(block, new_off, inode_no);
    wr16_le(block, new_off + 4, (last_rec - last_min) as u16);
    block[new_off + 6] = name_len as u8;
    block[new_off + 7] = ftype;
    block[new_off + DE_HEADER..new_off + DE_HEADER + name_len].copy_from_slice(name);
    Ok(true)
}

/// Supprime l'entrée à `off` en fusionnant son rec_len dans la précédente.
pub fn delete_entry(block: &mut [u8], off: usize, metadata_csum: bool) -> Result<()> {
    let bs = block.len();
    let usable = if metadata_csum { bs - DIRENT_TAIL_SIZE } else { bs };
    if off >= usable {
        return Err(Error::BadPath);
    }
    let rec_len = rd16_le(block, off + 4) as usize;
    if rec_len < 4 {
        return Err(Error::Corrupt("rec_len invalide (delete)"));
    }
    if off == 0 {
        // première entrée : inode 0, garde son rec_len
        wr32_le(block, 0, 0);
        return Ok(());
    }
    // trouve le précédent
    let mut prev = 0usize;
    let mut cur = 0usize;
    while cur < off {
        let rl = rd16_le(block, cur + 4) as usize;
        if rl < 4 || cur + rl > usable {
            return Err(Error::Corrupt("chaîne de dirent cassée (delete)"));
        }
        prev = cur;
        cur += rl;
    }
    if cur != off {
        return Err(Error::Corrupt("offset de dirent non aligné (delete)"));
    }
    let prev_rec = rd16_le(block, prev + 4) as usize;
    wr16_le(block, prev + 4, (prev_rec + rec_len) as u16);
    Ok(())
}

/// Décrit un répertoire indexé (dx_root parsé).
pub struct DxRoot {
    pub hash_version: u8,
    pub indirect_levels: u8,
    pub count: u16,
    pub limit: u16,
}

/// Parse le dx_root (premier bloc d'un répertoire indexé).
pub fn parse_dx_root(block: &[u8]) -> Result<DxRoot> {
    if block.len() < 36 {
        return Err(Error::Corrupt("dx_root trop court"));
    }
    let reserved_zero = rd32_le(block, 24);
    if reserved_zero != 0 {
        return Err(Error::Corrupt("dx_root reserved_zero != 0"));
    }
    let hash_version = block[28];
    let info_length = block[29];
    let indirect_levels = block[30];
    if info_length != 8 {
        return Err(Error::Corrupt("dx_root info_length != 8"));
    }
    let limit = rd16_le(block, 32);
    let count = rd16_le(block, 34);
    Ok(DxRoot {
        hash_version,
        indirect_levels,
        count,
        limit,
    })
}

/// Détecte un bloc d'index dx (nœud intermédiaire) : fake dirent
/// (inode 0, rec_len ≈ blocksize, name_len 0) suivi d'un countlimit plausible.
pub fn is_dx_node(block: &[u8]) -> bool {
    let bs = block.len();
    if bs < 12 {
        return false;
    }
    let inode = rd32_le(block, 0);
    let rec_len = rd16_le(block, 4) as usize;
    if inode != 0 || block[6] != 0 {
        return false;
    }
    if !(bs.saturating_sub(16)..=bs).contains(&rec_len) {
        return false;
    }
    let limit = rd16_le(block, 8) as usize;
    let count = rd16_le(block, 10) as usize;
    count <= limit && limit > 0 && 12 + limit * 8 <= bs
}

/// Structures dx pour insertion dans un répertoire indexé.
///
/// IMPORTANT : entries[0] est le countlimit en surimpression
/// (limit@base, count@base+2) et son champ `block` (@base+4) désigne le
/// premier enfant (hash implicite 0). Les enfants suivants sont
/// entries[i] = @base + i*8 (hash@.., block@..+4), i in 1..count.
pub struct DxWalker;

impl DxWalker {
    /// Trouve le bloc feuille où placer un nom (hash donné). Retourne (block).
    /// `read` lit un bloc du répertoire par numéro PHYSIQUE (dx stocke des
    /// blocs fs absolus, pas des lblocks logiques).
    pub fn find_leaf<F>(
        root_block: &[u8],
        hash: u32,
        read: &mut F,
    ) -> Result<(u64, DxRoot)>
    where
        F: FnMut(u64) -> Result<alloc::vec::Vec<u8>>,
    {
        let root = parse_dx_root(root_block)?;
        let count = root.count as usize;
        if count == 0 {
            return Err(Error::Corrupt("dx_root vide"));
        }
        // racine : base = 32
        let mut child = Self::descend(root_block, 32, count, hash)?;
        let mut levels = root.indirect_levels;
        while levels > 0 {
            let node = read(child)?;
            if !is_dx_node(&node) {
                return Err(Error::Corrupt("nœud dx invalide"));
            }
            let count = rd16_le(&node, 10) as usize;
            if count == 0 {
                return Err(Error::Corrupt("nœud dx vide"));
            }
            child = Self::descend(&node, 8, count, hash)?;
            levels -= 1;
        }
        Ok((child, root))
    }

    /// Descend dans un nœud : base = 32 (racine) ou 8 (nœud interne).
    /// Comme dx_probe : dernier entry avec hash <= cible, sinon entries[0].
    fn descend(block: &[u8], base: usize, count: usize, hash: u32) -> Result<u64> {
        // entries[0].block = premier enfant (hash implicite 0)
        let mut child = rd32_le(block, base + 4) as u64 & 0x0FFF_FFFF;
        for i in 1..count {
            let off = base + i * 8;
            let h = rd32_le(block, off);
            if h <= hash {
                child = rd32_le(block, off + 4) as u64 & 0x0FFF_FFFF;
            } else {
                break;
            }
        }
        Ok(child)
    }
}

/// Recalcule le checksum dx d'un bloc d'index (root ou nœud).
pub fn dx_csum_set(block: &mut [u8], count_offset: usize, dir_seed: u32) {
    let bs = block.len();
    if bs < count_offset + 8 + DX_TAIL_SIZE {
        return;
    }
    let count = rd16_le(block, count_offset + 2) as usize;
    let entries_end = count_offset + count * 8;
    let tail_off = bs - DX_TAIL_SIZE;
    let mut c = dir_seed;
    c = crate::crc::crc32c_update(c, &block[..entries_end]);
    c = crate::crc::crc32c_update(c, &block[tail_off..tail_off + 4]);
    c = crate::crc::crc32c_update(c, &[0u8; 4]);
    wr32_le(block, tail_off + 4, c);
}

/// Sum checksums bitmap (utilisé par fs.rs pour recalculer les checksums des
/// bitmaps dans les descripteurs).
pub use gdt::bitmap_csum;
