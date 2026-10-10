//! Attributs étendus (xattrs) ext4 : bloc dédié (i_file_acl) + corps
//! d'inode (ibody). Lecture et écriture, avec hash d'entrée et checksum
//! de bloc conformes à e2fsprogs 1.47.2 (juge de paix des tests).

use crate::crc::crc32c_update;
use crate::error::{Error, Result};
use crate::fs::Filesystem;
use crate::inode::{Inode, S_IFDIR, S_IFREG};
use crate::superblock::feat_incompat;
use crate::util::*;
use alloc::vec::Vec;

/// Magic du header de bloc/ibody EA.
pub const XATTR_MAGIC: u32 = 0xEA02_0000;
/// Namespace "user." (index 1) : celui qu'on expose (EAs Windows, ADS, SD).
pub const XIDX_USER: u8 = 1;
/// Namespace "system." (index 7) : system.data pour inline_data étendu.
pub const XIDX_SYSTEM: u8 = 7;

/// Taille du header de bloc EA (32 o).
pub const XATTR_HEADER_SIZE: usize = 32;

/// Entrée EA sur disque.
#[derive(Clone, Debug)]
pub struct EaEntry {
    pub name_index: u8,
    pub name: Vec<u8>,
    pub value_offs: u16,
    pub value_inum: u32,
    pub value_size: u32,
    pub hash: u32,
}

impl EaEntry {
    /// Taille totale sur disque (header 16 o + nom, arrondi 4).
    pub fn disk_len(&self) -> usize {
        (16 + self.name.len() + 3) & !3
    }
}

/// Parse la liste d'entrées : `[start, end)`, marqueur de fin = name_len 0.
/// Retourne (entrées, offset de la fin réelle des entrées).
fn parse_entries(raw: &[u8], start: usize, end: usize) -> Result<(Vec<EaEntry>, usize)> {
    let mut out = Vec::new();
    let mut off = start;
    while off + 4 <= end {
        let name_len = raw[off] as usize;
        if name_len == 0 {
            break;
        }
        if name_len > 255 || off + 16 + name_len > end {
            return Err(Error::Corrupt("entrée EA invalide"));
        }
        let e = EaEntry {
            name_index: raw[off + 1],
            name: raw[off + 16..off + 16 + name_len].to_vec(),
            value_offs: rd16_le(raw, off + 2),
            value_inum: rd32_le(raw, off + 4),
            value_size: rd32_le(raw, off + 8),
            hash: rd32_le(raw, off + 12),
        };
        let total = (16 + name_len + 3) & !3;
        if e.value_inum != 0 {
            return Err(Error::Corrupt("EA-inode non supporté"));
        }
        out.push(e);
        off += total;
    }
    Ok((out, off))
}

/// Hash d'une entrée EA (algo e2fsprogs ext2fs_ext_attr_hash_entry,
/// identique kernel ext4_xattr_hash_entry pour données non signées).
pub fn ea_entry_hash(name: &[u8], value: &[u8]) -> u32 {
    let mut hash: u32 = 0;
    for &b in name {
        hash = (hash << 5) ^ (hash >> 27) ^ b as u32;
    }
    if !value.is_empty() && value.len() < u32::MAX as usize {
        // mots 32 bits little-endian, padding inclus (l'e2fsprogs lit
        // (size+3)/4 mots ; les octets de padding sont à zéro sur disque)
        let words = (value.len() + 3) >> 2;
        for i in 0..words {
            let base = i * 4;
            let mut w = [0u8; 4];
            for j in 0..4 {
                if base + j < value.len() {
                    w[j] = value[base + j];
                }
            }
            let v = u32::from_le_bytes(w);
            hash = (hash << 16) ^ (hash >> 16) ^ v;
        }
    }
    hash
}

/// Entrée + valeur décodée.
#[derive(Clone, Debug)]
pub struct EaItem {
    pub entry: EaEntry,
    pub value: Vec<u8>,
}

/// Checksum d'un bloc EA (metadata_csum) — e2fsprogs ext2fs_ext_attr_block_csum :
/// seed sb, le64(nr de bloc), bloc entier avec h_checksum à zéro.
pub fn block_csum(seed: u32, block_nr: u64, raw: &[u8]) -> u32 {
    let mut c = crc32c_update(seed, &block_nr.to_le_bytes());
    let mut tmp = Vec::with_capacity(raw.len());
    tmp.extend_from_slice(raw);
    if tmp.len() >= 20 {
        tmp[16..20].fill(0);
    }
    crc32c_update(c, &tmp)
}

/// Vérifie le checksum d'un bloc EA (metadata_csum).
fn verify_block_csum(fs: &Filesystem, block_nr: u64, raw: &[u8]) -> Result<()> {
    if !fs.sb.has_metadata_csum() {
        return Ok(());
    }
    let stored = rd32_le(raw, 16);
    let want = block_csum(fs.sb.csum_seed, block_nr, raw);
    if stored != want {
        return Err(Error::ChecksumMismatch("xattr block"));
    }
    Ok(())
}

/// Lit un bloc EA et retourne ses entrées + valeurs.
pub fn read_block(fs: &mut Filesystem, block_nr: u64) -> Result<Vec<EaItem>> {
    let raw = fs.get_block(block_nr)?.clone();
    verify_block_csum(fs, block_nr, &raw)?;
    let magic = rd32_le(&raw, 0);
    if magic != XATTR_MAGIC {
        return Err(Error::Corrupt("bloc EA: magic invalide"));
    }
    let blocks = rd32_le(&raw, 8);
    if blocks != 1 {
        return Err(Error::Corrupt("bloc EA: h_blocks invalide"));
    }
    let bs = fs.block_size() as usize;
    let entries = parse_entries(&raw, XATTR_HEADER_SIZE, bs)?;
    let mut out = Vec::new();
    for e in entries.0 {
        if e.value_inum != 0 {
            return Err(Error::Corrupt("EA-inode non supporté"));
        }
        let offs = e.value_offs as usize;
        let sz = e.value_size as usize;
        if offs + sz > bs {
            return Err(Error::Corrupt("valeur EA hors bloc"));
        }
        let value = raw[offs..offs + sz].to_vec();
        if ea_entry_hash(&e.name, &value) != e.hash {
            return Err(Error::ChecksumMismatch("xattr entry hash"));
        }
        out.push(EaItem { entry: e, value });
    }
    Ok(out)
}

/// Lit les xattrs du corps d'inode (ibody) : magic 4 o à
/// 0x80 + i_extra_isize, entrées ensuite, valeurs en fin d'inode.
pub fn read_ibody_entries(inode: &Inode) -> Result<Vec<(EaEntry, Vec<u8>)>> {
    let isz = inode.raw.len();
    let start = 0x80 + inode.extra_isize() as usize + 4;
    if start + 4 > isz || rd32_le(&inode.raw, 0x80 + inode.extra_isize() as usize) != XATTR_MAGIC
    {
        return Ok(Vec::new());
    }
    let (entries, _) = parse_entries(&inode.raw, start, isz)?;
    let mut out = Vec::new();
    for e in entries {
        // offs RELATIFS au début des entrées (calibré empiriquement :
        // valeur "one" à start+88 = isz-4)
        let offs = start + e.value_offs as usize;
        let sz = e.value_size as usize;
        if offs > isz || sz > isz || offs < sz || offs - sz < start {
            return Err(Error::Corrupt("valeur EA ibody hors inode"));
        }
        let value = inode.raw[offs..offs + sz].to_vec();
        out.push((e, value));
    }
    Ok(out)
}

/// Toutes les xattrs d'un inode (ibody d'abord, puis bloc dédié).
pub fn list_xattrs(fs: &mut Filesystem, ino: u32) -> Result<Vec<(EaEntry, Vec<u8>)>> {
    let inode = fs.read_inode(ino)?;
    let mut out = read_ibody_entries(&inode)?;
    let facl = inode.file_acl();
    if facl != 0 {
        let items = read_block(fs, facl)?;
        for it in items {
            out.push((it.entry, it.value));
        }
    }
    Ok(out)
}

/// Cherche une xattr par namespace + nom.
pub fn get_xattr(fs: &mut Filesystem, ino: u32, name_index: u8, name: &[u8]) -> Result<Option<Vec<u8>>> {
    for (e, v) in list_xattrs(fs, ino)? {
        if e.name_index == name_index && e.name == name {
            return Ok(Some(v));
        }
    }
    Ok(None)
}

// =====================================================================
// Écriture
// =====================================================================

/// Place dispo dans l'ibody pour `used` octets d'entrées+valeurs
/// (région [0x80+extra+4, isz), marqueur de fin 4 o inclus).
fn ibody_room(inode: &Inode, used: usize) -> usize {
    let isz = inode.raw.len();
    let start = 0x80 + inode.extra_isize() as usize + 4;
    isz.saturating_sub(start + used + 4)
}

/// Construit un bloc EA complet (header + entrées avant + valeurs en
/// arrière + checksum). Retourne Err(NoSpace) si ça ne tient pas.
fn build_block(
    fs: &Filesystem,
    block_nr: u64,
    items: &[(EaEntry, Vec<u8>)],
) -> Result<Vec<u8>> {
    let bs = fs.block_size() as usize;
    let mut raw = alloc::vec![0u8; bs];
    wr32_le(&mut raw, 0, XATTR_MAGIC);
    wr32_le(&mut raw, 4, 1); // h_refcount
    wr32_le(&mut raw, 8, 1); // h_blocks
    // h_hash @12 = 0 (calibré), h_checksum @16 posé à la fin
    let mut eoff = 32usize;
    let mut voff = bs;
    for (e, v) in items {
        if e.value_inum != 0 {
            return Err(Error::Corrupt("EA-inode non supporté"));
        }
        let sz = v.len();
        let total = (16 + e.name.len() + 3) & !3;
        let vsz = (sz + 3) & !3;
        if eoff + total + 4 > voff - vsz || voff - vsz < eoff + total + 4 {
            return Err(Error::NoSpace);
        }
        let vstart = (voff - vsz) & !3;
        raw[eoff] = e.name.len() as u8;
        raw[eoff + 1] = e.name_index;
        wr16_le(&mut raw, eoff + 2, vstart as u16);
        wr32_le(&mut raw, eoff + 4, 0);
        wr32_le(&mut raw, eoff + 8, sz as u32);
        wr32_le(&mut raw, eoff + 12, ea_entry_hash(&e.name, v));
        raw[eoff + 16..eoff + 16 + e.name.len()].copy_from_slice(&e.name);
        for b in &mut raw[eoff + 16 + e.name.len()..eoff + total] {
            *b = 0;
        }
        raw[vstart..vstart + sz].copy_from_slice(v);
        voff = vstart;
        eoff += total;
    }
    let c = block_csum(fs.sb.csum_seed, block_nr, &raw);
    wr32_le(&mut raw, 16, c);
    Ok(raw)
}

/// Reconstruit la région ibody (magic + entrées avant + valeurs en
/// arrière depuis la fin d'inode, creux à zéro). `start` = début des
/// entrées. Écrit dans inode.raw.
fn build_ibody(inode: &mut Inode, start: usize, items: &[(EaEntry, Vec<u8>)]) -> Result<()> {
    let isz = inode.raw.len();
    // magic à 0x80 + i_extra_isize
    wr32_le(&mut inode.raw, start - 4, XATTR_MAGIC);
    let mut eoff = start;
    let mut voff = isz;
    for (e, v) in items {
        let sz = v.len();
        let total = (16 + e.name.len() + 3) & !3;
        let vsz = (sz + 3) & !3;
        if eoff + total + 4 > voff - vsz {
            return Err(Error::NoSpace);
        }
        let vstart = (voff - vsz) & !3;
        inode.raw[eoff] = e.name.len() as u8;
        inode.raw[eoff + 1] = e.name_index;
        wr16_le(&mut inode.raw, eoff + 2, (vstart - start) as u16);
        wr32_le(&mut inode.raw, eoff + 4, 0);
        wr32_le(&mut inode.raw, eoff + 8, sz as u32);
        // e_hash = 0 dans l'ibody (calibré : e2fsprogs ne le pose pas)
        wr32_le(&mut inode.raw, eoff + 12, 0);
        inode.raw[eoff + 16..eoff + 16 + e.name.len()].copy_from_slice(&e.name);
        for b in &mut inode.raw[eoff + 16 + e.name.len()..eoff + total] {
            *b = 0;
        }
        inode.raw[vstart..vstart + sz].copy_from_slice(v);
        voff = vstart;
        eoff += total;
    }
    // marqueur de fin + creux à zéro entre entrées et valeurs
    for b in &mut inode.raw[eoff..voff] {
        *b = 0;
    }
    Ok(())
}

/// Total disque d'une entrée : header+nom (aligné 4) + valeur (aligné 4).
fn entry_cost(e: &EaEntry, v: &[u8]) -> usize {
    ((16 + e.name.len() + 3) & !3) + ((v.len() + 3) & !3)
}

/// Pose ou remplace une xattr. Placement calibré e2fsprogs : ibody
/// d'abord (débordement en bloc dans l'ordre d'insertion) ; pour un
/// inode inline_data, bloc seul (l'ibody porte system.data).
pub fn set_xattr(
    fs: &mut Filesystem,
    ino: u32,
    name_index: u8,
    name: &[u8],
    value: &[u8],
) -> Result<()> {
    if fs.read_only {
        return Err(Error::ReadOnly);
    }
    if name.is_empty() || name.len() > 255 {
        return Err(Error::Corrupt("nom EA invalide"));
    }
    let mut inode = fs.read_inode(ino)?;
    let bs = fs.block_size() as usize;
    // capacité bloc : header 32 + fin 4 + entrée + valeur
    let cost = {
        let e = EaEntry {
            name_index,
            name: name.to_vec(),
            value_offs: 0,
            value_inum: 0,
            value_size: value.len() as u32,
            hash: 0,
        };
        entry_cost(&e, value)
    };
    if 32 + 4 + cost > bs {
        return Err(Error::NoSpace);
    }
    // ---- existant (ibody + bloc), fusion avec la cible ----
    let old_facl = inode.file_acl();
    let ibody_items = if inode.is_inline() {
        Vec::new() // ibody occupé par system.data
    } else {
        read_ibody_entries(&inode)?
    };
    let block_items = if old_facl != 0 {
        read_block(fs, old_facl)?
    } else {
        Vec::new()
    };
    let mut merged: Vec<(EaEntry, Vec<u8>)> = Vec::new();
    let mut replaced = false;
    for (e, v) in ibody_items.into_iter().chain(block_items.into_iter().map(|it| (it.entry, it.value))) {
        if !replaced && e.name_index == name_index && e.name == name {
            merged.push((
                EaEntry {
                    name_index,
                    name: name.to_vec(),
                    value_offs: 0,
                    value_inum: 0,
                    value_size: value.len() as u32,
                    hash: 0,
                },
                value.to_vec(),
            ));
            replaced = true;
        } else {
            merged.push((e, v));
        }
    }
    if !replaced {
        merged.push((
            EaEntry {
                name_index,
                name: name.to_vec(),
                value_offs: 0,
                value_inum: 0,
                value_size: value.len() as u32,
                hash: 0,
            },
            value.to_vec(),
        ));
    }
    // ---- répartition ibody / bloc (ordre d'insertion, calibré) ----
    let start = 0x80 + inode.extra_isize() as usize + 4;
    let mut ibody: Vec<(EaEntry, Vec<u8>)> = Vec::new();
    let mut block: Vec<(EaEntry, Vec<u8>)> = Vec::new();
    let mut used = 0usize;
    for (e, v) in &merged {
        let c = entry_cost(e, v);
        if !inode.is_inline() && used + c <= ibody_room(&inode, used) {
            ibody.push((e.clone(), v.clone()));
            used += c;
        } else {
            block.push((e.clone(), v.clone()));
        }
    }
    // capacité du bloc pour ses entrées
    let bneed: usize = block
        .iter()
        .map(|(e, v)| entry_cost(e, v))
        .sum::<usize>()
        + 32
        + 4;
    if !block.is_empty() && bneed > bs {
        return Err(Error::NoSpace);
    }
    // ---- écriture ----
    let (now, _) = fs.time.now();
    inode.set_ctime(now);
    // bloc dédié
    match (block.is_empty(), old_facl) {
        (true, 0) => {}
        (true, old) => {
            // le bloc se vide : libération
            fs.free_block(old)?;
            inode.set_blocks(inode.blocks() - (bs / 512) as u64);
            wr32_le(&mut inode.raw, 0x68, 0);
            wr16_le(&mut inode.raw, 0x76, 0);
        }
        (false, 0) => {
            let nr = fs.alloc_block()?;
            let raw = build_block(fs, nr, &block)?;
            fs.put_block(nr, &raw)?;
            inode.set_blocks(inode.blocks() + (bs / 512) as u64);
            wr32_le(&mut inode.raw, 0x68, nr as u32);
            wr16_le(&mut inode.raw, 0x76, (nr >> 32) as u16);
        }
        (false, old) => {
            let raw = build_block(fs, old, &block)?;
            fs.put_block(old, &raw)?;
        }
    }
    // ibody : si vide, efface le magic (region à zéro)
    if ibody.is_empty() {
        for b in &mut inode.raw[start - 4..] {
            *b = 0;
        }
    } else {
        build_ibody(&mut inode, start, &ibody)?;
    }
    fs.write_inode(&mut inode)?;
    fs.commit_transaction()
}

/// Supprime une xattr (ibody ou bloc) ; libère le bloc s'il se vide.
pub fn remove_xattr(fs: &mut Filesystem, ino: u32, name_index: u8, name: &[u8]) -> Result<()> {
    if fs.read_only {
        return Err(Error::ReadOnly);
    }
    let mut inode = fs.read_inode(ino)?;
    let bs = fs.block_size() as usize;
    let start = 0x80 + inode.extra_isize() as usize + 4;
    let old_facl = inode.file_acl();
    // d'abord l'ibody
    let ibody_items = if inode.is_inline() {
        Vec::new()
    } else {
        read_ibody_entries(&inode)?
    };
    let mut ibody: Vec<(EaEntry, Vec<u8>)> = Vec::new();
    let mut found = false;
    for (e, v) in ibody_items {
        if !found && e.name_index == name_index && e.name == name {
            found = true;
        } else {
            ibody.push((e, v));
        }
    }
    // puis le bloc
    let mut block: Vec<(EaEntry, Vec<u8>)> = Vec::new();
    if old_facl != 0 && !found {
        for it in read_block(fs, old_facl)? {
            let (e, v) = (it.entry, it.value);
            if !found && e.name_index == name_index && e.name == name {
                found = true;
            } else {
                block.push((e, v));
            }
        }
    }
    if !found {
        return Err(Error::Corrupt("EA absente"));
    }
    let (now, _) = fs.time.now();
    inode.set_ctime(now);
    if block.is_empty() && old_facl != 0 {
        fs.free_block(old_facl)?;
        inode.set_blocks(inode.blocks() - (bs / 512) as u64);
        wr32_le(&mut inode.raw, 0x68, 0);
        wr16_le(&mut inode.raw, 0x76, 0);
    } else if !block.is_empty() {
        let raw = build_block(fs, old_facl, &block)?;
        fs.put_block(old_facl, &raw)?;
    }
    if ibody.is_empty() {
        for b in &mut inode.raw[start - 4..] {
            *b = 0;
        }
    } else {
        build_ibody(&mut inode, start, &ibody)?;
    }
    fs.write_inode(&mut inode)?;
    fs.commit_transaction()
}

/// Découpe un nom complet en (index de namespace, nom court).
/// Ordre e2fsprogs : préfixes les plus spécifiques d'abord.
pub fn split_ns(full: &[u8]) -> (u8, Vec<u8>) {
    const PRE: [(&[u8], u8); 9] = [
        (b"gnu.", 10),
        (b"system.posix_acl_default", 3),
        (b"system.posix_acl_access", 2),
        (b"system.richacl", 8),
        (b"security.", 6),
        (b"trusted.", 4),
        (b"system.", 7),
        (b"user.", 1),
        (b"os2.", 1), // EAs OS/2 stockés en user.
    ];
    for (p, idx) in PRE {
        if full.starts_with(p) {
            return (idx, full[p.len()..].to_vec());
        }
    }
    (1, full.to_vec())
}

/// Reconstitue un nom complet depuis index + nom court.
pub fn join_ns(index: u8, short: &[u8]) -> Vec<u8> {
    let pre: &[u8] = match index {
        1 => b"user.",
        2 => b"system.posix_acl_access",
        3 => b"system.posix_acl_default",
        4 => b"trusted.",
        6 => b"security.",
        7 => b"system.",
        8 => b"system.richacl",
        10 => b"gnu.",
        _ => b"user.",
    };
    let mut out = Vec::with_capacity(pre.len() + short.len());
    out.extend_from_slice(pre);
    out.extend_from_slice(short);
    out
}
