//! Arbres d'extents ext4 : lecture (aplatissage, lookup) et reconstruction
//! complète après modification (insertion/suppression de mappings).
//!
//! Stratégie : l'arbre est aplati en liste de mappings, modifié en mémoire
//! (avec fusion des extents adjacents), puis re-sérialisé de bas en haut.
//! Les blocs de nœuds anciens sont libérés, de nouveaux sont alloués.
//! Les blocs de DONNÉES référencés ne sont jamais touchés.

use alloc::vec::Vec;

use crate::error::{Error, Result};
use crate::inode::{Inode, INLINE_MAX};
use crate::superblock::Superblock;

pub const EH_MAGIC: u16 = 0xF30A;
pub const MAX_DEPTH: u32 = 5;
/// Longueur max d'un extent initialisé (16 bits, bit haut réservé aux non-écrits).
pub const MAX_INIT_LEN: u32 = 0x8000;

/// Un mapping logique→physique aplati.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExtMap {
    pub logical: u32,
    pub len: u32, // en blocs
    pub phys: u64,
    pub unwritten: bool,
}

/// Contexte fourni par le FS (cache blocs + allocateur).
pub trait ExtentCtx {
    fn sb(&self) -> &Superblock;
    /// Lit un bloc (via cache).
    fn read_block(&mut self, bno: u64, buf: &mut [u8]) -> Result<()>;
    /// Écrit un bloc (via cache, sera journalisé).
    fn write_block(&mut self, bno: u64, buf: &[u8]) -> Result<()>;
    /// Alloue un bloc (bitmap) — contenu indéfini.
    fn alloc_block(&mut self) -> Result<u64>;
    /// Libère un bloc (bitmap).
    fn free_block(&mut self, bno: u64) -> Result<()>;
}

#[inline]
fn eh_entries(buf: &[u8]) -> usize {
    crate::util::rd16_le(buf, 2) as usize
}
#[inline]
fn eh_max(buf: &[u8]) -> usize {
    crate::util::rd16_le(buf, 4) as usize
}
#[inline]
pub fn eh_depth(buf: &[u8]) -> u32 {
    crate::util::rd16_le(buf, 6) as u32
}

fn read_extent_entry(buf: &[u8], off: usize) -> ExtMap {
    // struct ext4_extent : ee_block(4), ee_len(2), ee_start_hi(2), ee_start_lo(4)
    let logical = crate::util::rd32_le(buf, off);
    let raw_len = crate::util::rd16_le(buf, off + 4);
    let phys = crate::util::rd32_le(buf, off + 8) as u64
        | (crate::util::rd16_le(buf, off + 6) as u64) << 32;
    let unwritten = raw_len >= 0x8000;
    let len = if unwritten {
        raw_len as u32 - 0x8000
    } else {
        raw_len as u32
    };
    ExtMap {
        logical,
        len,
        phys,
        unwritten,
    }
}

fn read_index_entry(buf: &[u8], off: usize) -> (u32, u64) {
    let logical = crate::util::rd32_le(buf, off);
    let leaf = crate::util::rd32_le(buf, off + 4) as u64
        | (crate::util::rd16_le(buf, off + 8) as u64) << 32;
    (logical, leaf)
}

/// Aplatit tout l'arbre d'extents de l'inode (récursif, profondeur ≤ 5).
pub fn flatten(ctx: &mut dyn ExtentCtx, ino: &Inode) -> Result<Vec<ExtMap>> {
    let mut out = alloc::vec::Vec::new();
    if !ino.has_extents() {
        return Ok(out);
    }
    let mut root = [0u8; 60];
    root.copy_from_slice(ino.i_block());
    let root_depth = eh_depth(&root);
    flatten_node(ctx, &root, root_depth, &mut out)?;
    out.sort_by_key(|m| m.logical);
    Ok(out)
}

fn flatten_node(
    ctx: &mut dyn ExtentCtx,
    node: &[u8],
    depth: u32,
    out: &mut alloc::vec::Vec<ExtMap>,
) -> Result<()> {
    if crate::util::rd16_le(node, 0) != EH_MAGIC {
        return Err(Error::Corrupt("magic extent header"));
    }
    let n = eh_entries(node);
    let d = eh_depth(node);
    if d != depth {
        return Err(Error::Corrupt("profondeur extent incohérente"));
    }
    if d == 0 {
        for i in 0..n {
            out.push(read_extent_entry(node, 12 + i * 12));
        }
    } else {
        for i in 0..n {
            let (_, leaf) = read_index_entry(node, 12 + i * 12);
            let mut blk = alloc::vec![0u8; ctx.sb().block_size as usize];
            ctx.read_block(leaf, &mut blk)?;
            flatten_node(ctx, &blk, d - 1, out)?;
        }
    }
    Ok(())
}

/// Recherche du mapping couvrant `lblock` : (phys_base, run_len, unwritten).
pub fn lookup(
    ctx: &mut dyn ExtentCtx,
    ino: &Inode,
    lblock: u32,
) -> Result<Option<(u64, u32, bool)>> {
    let maps = flatten(ctx, ino)?;
    for m in maps {
        if lblock >= m.logical && lblock < m.logical + m.len {
            let delta = lblock - m.logical;
            return Ok(Some((m.phys + delta as u64, m.len - delta, m.unwritten)));
        }
    }
    Ok(None)
}

/// Insère/remplace le mapping `lblock..lblock+len` (physique contigu).
pub fn map_range(
    ctx: &mut dyn ExtentCtx,
    ino: &mut Inode,
    lblock: u32,
    len: u32,
    phys: u64,
    unwritten: bool,
) -> Result<()> {
    let mut maps = if ino.has_extents() {
        flatten(ctx, ino)?
    } else {
        alloc::vec::Vec::new()
    };
    let end = lblock as u64 + len as u64;
    maps.retain(|m| {
        let m_end = m.logical as u64 + m.len as u64;
        !((m.logical as u64) < end && (lblock as u64) < m_end)
    });
    maps.push(ExtMap {
        logical: lblock,
        len,
        phys,
        unwritten,
    });
    merge_and_rebuild(ctx, ino, maps)
}

/// Retire le mapping couvrant `lblock..lblock+len`.
pub fn unmap_range(
    ctx: &mut dyn ExtentCtx,
    ino: &mut Inode,
    lblock: u32,
    len: u32,
) -> Result<()> {
    let maps = flatten(ctx, ino)?;
    let mut out: alloc::vec::Vec<ExtMap> = alloc::vec::Vec::new();
    let end = lblock as u64 + len as u64;
    for m in maps {
        let m_end = m.logical as u64 + m.len as u64;
        if m_end <= lblock as u64 || (m.logical as u64) >= end {
            out.push(m);
            continue;
        }
        // Chevauchement : découpe
        if m.logical < lblock {
            out.push(ExtMap {
                logical: m.logical,
                len: lblock - m.logical,
                phys: m.phys,
                unwritten: m.unwritten,
            });
        }
        if m_end > end {
            out.push(ExtMap {
                logical: end as u32,
                len: (m_end - end) as u32,
                phys: m.phys + (end - m.logical as u64),
                unwritten: m.unwritten,
            });
        }
    }
    merge_and_rebuild(ctx, ino, out)
}

fn merge_and_rebuild(
    ctx: &mut dyn ExtentCtx,
    ino: &mut Inode,
    mut maps: alloc::vec::Vec<ExtMap>,
) -> Result<()> {
    maps.sort_by_key(|m| m.logical);
    // Fusion des extents adjacents physiquement contigus
    let mut merged: alloc::vec::Vec<ExtMap> = alloc::vec::Vec::with_capacity(maps.len() + 1);
    for m in maps {
        if let Some(last) = merged.last_mut() {
            let last_end = last.logical + last.len;
            if last_end == m.logical
                && !last.unwritten
                && !m.unwritten
                && last.phys + last.len as u64 == m.phys
            {
                last.len += m.len;
                continue;
            }
        }
        merged.push(m);
    }
    // Scission des extents trop longs pour le champ 16 bits
    let mut split: alloc::vec::Vec<ExtMap> = alloc::vec::Vec::new();
    for m in merged {
        let cap = if m.unwritten { 0x7FFF } else { MAX_INIT_LEN };
        let mut remaining = m.len;
        let mut base = m.logical;
        let mut pbase = m.phys;
        while remaining > 0 {
            let n = remaining.min(cap);
            split.push(ExtMap {
                logical: base,
                len: n,
                phys: pbase,
                unwritten: m.unwritten,
            });
            base += n;
            pbase += n as u64;
            remaining -= n;
        }
    }
    rebuild(ctx, ino, split)
}

/// Reconstruit l'arbre complet depuis les mappings aplatis.
fn rebuild(ctx: &mut dyn ExtentCtx, ino: &mut Inode, maps: alloc::vec::Vec<ExtMap>) -> Result<()> {
    let bs = ctx.sb().block_size as usize;
    let sectors = (bs / 512) as u64;
    // 1. Libérer les anciens nœuds (sauf la racine qui vit dans l'inode)
    let mut old_nodes: u64 = 0;
    if ino.has_extents() {
        let root_depth = eh_depth(ino.i_block());
        if root_depth > 0 {
            let mut root = [0u8; 60];
            root.copy_from_slice(ino.i_block());
            old_nodes = free_node_children(ctx, &root, root_depth)?;
        }
    }
    let mut new_nodes: u64 = 0;
    // 2. Cas simple : tout tient dans la racine (4 entrées max)
    let root_max = 4usize; // (60-12)/12
    if maps.len() <= root_max {
        let root = ino.i_block_mut();
        write_eh_header(root, maps.len() as u16, root_max as u16, 0);
        for (i, m) in maps.iter().enumerate() {
            write_extent_entry(root, 12 + i * 12, m);
        }
        ino.set_flags(ino.flags() | crate::inode::FI_EXTENTS);
        return Ok(());
    }
    // 3. Sérialiser les feuilles (+ tail checksum + i_blocks)
    let per_leaf = (bs - 12) / 12;
    let seed = crate::dir::inode_csum_seed(ctx.sb(), ino.no, ino.generation());
    let mut level: alloc::vec::Vec<(u32, u64)> = alloc::vec::Vec::new();
    for chunk in maps.chunks(per_leaf) {
        let bno = ctx.alloc_block()?;
        let mut blk = alloc::vec![0u8; bs];
        write_eh_header(&mut blk, chunk.len() as u16, per_leaf as u16, 0);
        for (i, m) in chunk.iter().enumerate() {
            write_extent_entry(&mut blk, 12 + i * 12, m);
        }
        write_extent_tail(ctx, &mut blk, seed);
        ctx.write_block(bno, &blk)?;
        level.push((chunk[0].logical, bno));
        new_nodes += 1;
    }
    // 4. Remonter les niveaux d'index jusqu'à tenir dans la racine.
    //    IMPORTANT : dès qu'il existe des feuilles hors inode, la racine est
    //    un nœud d'INDEX (depth >= 1) — même avec une seule feuille. Un
    //    depth=0 avec entrée d'index serait relu comme extent corrompu.
    let mut depth = 1u32;
    while level.len() > root_max {
        let mut next: alloc::vec::Vec<(u32, u64)> = alloc::vec::Vec::new();
        for chunk in level.chunks(per_leaf) {
            let bno = ctx.alloc_block()?;
            let mut blk = alloc::vec![0u8; bs];
            write_eh_header(&mut blk, chunk.len() as u16, per_leaf as u16, depth);
            for (i, (logical, child)) in chunk.iter().enumerate() {
                let off = 12 + i * 12;
                crate::util::wr32_le(&mut blk, off, *logical);
                crate::util::wr32_le(&mut blk, off + 4, *child as u32);
                crate::util::wr16_le(&mut blk, off + 8, (*child >> 32) as u16);
            }
            write_extent_tail(ctx, &mut blk, seed);
            ctx.write_block(bno, &blk)?;
            next.push((chunk[0].0, bno));
            new_nodes += 1;
        }
        level = next;
        depth += 1;
        if depth > MAX_DEPTH {
            return Err(Error::ExtentFull);
        }
    }
    // i_blocks : les nœuds d'index/feuille comptent dans i_blocks (512 o)
    if new_nodes != old_nodes {
        let delta = new_nodes as i64 - old_nodes as i64;
        let cur = ino.blocks() as i64;
        ino.set_blocks((cur + delta * sectors as i64).max(0) as u64);
    }
    // 5. Racine = niveau d'index le plus haut (profondeur `depth`)
    let root = ino.i_block_mut();
    write_eh_header(root, level.len() as u16, root_max as u16, depth);
    for (i, (logical, child)) in level.iter().enumerate() {
        let off = 12 + i * 12;
        crate::util::wr32_le(root, off, *logical);
        crate::util::wr32_le(root, off + 4, *child as u32);
        crate::util::wr16_le(root, off + 8, (*child >> 32) as u16);
    }
    ino.set_flags(ino.flags() | crate::inode::FI_EXTENTS);
    Ok(())
}

/// Tail checksum d'un bloc de nœud d'extents (ext4_extent_tail, 4 o à bs-4) :
/// crc32c(seed, bloc[..bs-4]) avec seed = seed d'inode (uuid ^ ino ^ génération).
fn write_extent_tail(ctx: &dyn ExtentCtx, blk: &mut [u8], seed: u32) {
    let bs = blk.len();
    if bs < 16 || !ctx.sb().has_metadata_csum() {
        return;
    }
    let c = crate::crc::crc32c_update(seed, &blk[..bs - 4]);
    crate::util::wr32_le(blk, bs - 4, c);
}

/// Libère récursivement les nœuds enfants d'un nœud d'index (le nœud passé
/// vit dans l'inode ou dans un bloc que l'appelant libère lui-même).
/// Retourne le nombre de blocs libérés.
fn free_node_children(ctx: &mut dyn ExtentCtx, node: &[u8], depth: u32) -> Result<u64> {
    if crate::util::rd16_le(node, 0) != EH_MAGIC {
        return Err(Error::Corrupt("magic extent header (free)"));
    }
    if depth == 0 {
        return Ok(0);
    }
    let mut freed: u64 = 0;
    for i in 0..eh_entries(node) {
        let (_, child) = read_index_entry(node, 12 + i * 12);
        let mut blk = alloc::vec![0u8; ctx.sb().block_size as usize];
        ctx.read_block(child, &mut blk)?;
        freed += free_node_children(ctx, &blk, depth - 1)?;
        ctx.free_block(child)?;
        freed += 1;
    }
    Ok(freed)
}

/// Libère tous les blocs de nœuds (index) de l'arbre d'extents d'un inode
/// (la racine vit dans l'inode, les feuilles de données sont gérées par
/// l'appelant). Retourne le nombre de blocs libérés.
pub fn free_tree_nodes(ctx: &mut dyn ExtentCtx, ino: &Inode) -> Result<u64> {
    if !ino.has_extents() {
        return Ok(0);
    }
    let root_depth = eh_depth(ino.i_block());
    if root_depth == 0 {
        return Ok(0);
    }
    let mut root = [0u8; 60];
    root.copy_from_slice(ino.i_block());
    free_node_children(ctx, &root, root_depth)
}

fn write_extent_entry(buf: &mut [u8], off: usize, m: &ExtMap) {
    crate::util::wr32_le(buf, off, m.logical);
    let raw_len = if m.unwritten {
        (m.len as u16) | 0x8000
    } else {
        m.len as u16
    };
    crate::util::wr16_le(buf, off + 4, raw_len);
    crate::util::wr16_le(buf, off + 6, (m.phys >> 32) as u16);
    crate::util::wr32_le(buf, off + 8, m.phys as u32);
}

fn write_eh_header(buf: &mut [u8], entries: u16, max: u16, depth: u32) {
    crate::util::wr16_le(buf, 0, EH_MAGIC);
    crate::util::wr16_le(buf, 2, entries);
    crate::util::wr16_le(buf, 4, max);
    crate::util::wr16_le(buf, 6, depth as u16);
    crate::util::wr32_le(buf, 8, 0); // generation
}

/// Convertit un inode inline en arbre d'extents ; `first_block` doit être un
/// bloc déjà alloué par l'appelant, recevant les données inline.
pub fn inline_to_extent(
    ctx: &mut dyn ExtentCtx,
    ino: &mut Inode,
    first_block: u64,
    inline_len: usize,
) -> Result<()> {
    if !ino.is_inline() || inline_len > INLINE_MAX {
        return Err(Error::Corrupt("inline_to_extent : état invalide"));
    }
    let bs = ctx.sb().block_size as usize;
    let mut blk = alloc::vec![0u8; bs];
    blk[..inline_len].copy_from_slice(&ino.raw[0x28..0x28 + inline_len]);
    ctx.write_block(first_block, &blk)?;
    let root = ino.i_block_mut();
    write_eh_header(root, 1, 4, 0);
    write_extent_entry(
        root,
        12,
        &ExtMap {
            logical: 0,
            len: 1,
            phys: first_block,
            unwritten: false,
        },
    );
    ino.set_flags((ino.flags() | crate::inode::FI_EXTENTS) & !crate::inode::FI_INLINE_DATA);
    // i_blocks : + 1 bloc de données (les données inline n'en avaient pas)
    ino.set_blocks(ino.blocks() + (bs as u64 / 512));
    Ok(())
}

/// Vérifie les helpers de header.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::rd16_le;

    #[test]
    fn eh_header_roundtrip() {
        let mut buf = [0u8; 60];
        write_eh_header(&mut buf, 3, 4, 1);
        assert_eq!(rd16_le(&buf, 0), EH_MAGIC);
        assert_eq!(eh_entries(&buf), 3);
        assert_eq!(eh_max(&buf), 4);
        assert_eq!(eh_depth(&buf), 1);
    }

    #[test]
    fn extent_entry_roundtrip() {
        let mut buf = [0u8; 60];
        let m = ExtMap {
            logical: 42,
            len: 128,
            phys: 0x1_0000_0010,
            unwritten: true,
        };
        write_extent_entry(&mut buf, 12, &m);
        let back = read_extent_entry(&buf, 12);
        assert_eq!(back, m);
    }
}
