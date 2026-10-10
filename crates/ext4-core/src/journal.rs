//! Journal jbd2 : chargement, replay, écriture de transactions avec
//! checksums (CSUM v2/v3), wrap circulaire, gestion du superblock journal.
//!
//! Conventions (confirmées par fs/jbd2/{journal,commit,recovery}.c) :
//! - tous les champs disque jbd2 sont gros-boutistes ;
//! - tag v3 (16 o) : blocknr@0, inutilisé@4, flags BE16@6, blocknr_high@8,
//!   checksum BE32@12 ; tag v2 (8/12 o) : blocknr@0, checksum BE16@4,
//!   flags BE16@6, [high@8] ;
//! - premier tag suivi de l'UUID (16 o), suivants : FLAG_SAME_UUID ;
//! - checksums CRC32C, seed = crc32c(~0, j_uuid) ; desc/commit/revoke :
//!   checksum sur le bloc entier (champ csum à zéro) ; tag : seq BE + bloc
//!   de données (forme journal, échappé).

use crate::error::{Error, Result};
use crate::extent::{ExtentCtx, ExtMap};
use crate::inode::Inode;
use crate::superblock::Superblock;
use crate::util::*;

pub const JBD2_MAGIC: u32 = 0xC03B_3998;
pub const BT_DESCRIPTOR: u32 = 1;
pub const BT_COMMIT: u32 = 2;
pub const BT_SUPERBLOCK_V1: u32 = 3;
pub const BT_SUPERBLOCK_V2: u32 = 4;
pub const BT_REVOKE: u32 = 5;

pub const FLAG_ESCAPE: u16 = 0x1;
pub const FLAG_SAME_UUID: u16 = 0x2;
pub const FLAG_DELETED: u16 = 0x4;
pub const FLAG_LAST: u16 = 0x8;

pub const FEAT_INCOMPAT_REVOKE: u32 = 0x1;
pub const FEAT_INCOMPAT_64BIT: u32 = 0x2;
pub const FEAT_INCOMPAT_ASYNC_COMMIT: u32 = 0x4;
pub const FEAT_INCOMPAT_CSUM_V2: u32 = 0x8;
pub const FEAT_INCOMPAT_CSUM_V3: u32 = 0x10;
pub const FEAT_INCOMPAT_FAST_COMMIT: u32 = 0x20;

const HEADER_SIZE: usize = 12;
const JSB_SIZE: usize = 1024;

/// Journal interne chargé.
pub struct Journal {
    pub jmap: alloc::vec::Vec<u64>, // lblock journal -> bloc fs
    pub maxlen: u32,                // blocs de log utilisables
    pub first: u32,                 // s_first : début du log
    pub start: u32,                 // s_start (0 = log vide)
    pub sequence: u32,              // s_sequence
    pub head: u32,                  // prochaine position libre
    pub blocksize: u32,
    pub has_64bit: bool,
    pub has_csum: bool, // v2 ou v3
    pub csum_v3: bool,
    pub seed: u32,
    pub uuid: [u8; 16],
    pub jsb_raw: alloc::vec::Vec<u8>,
    pub armed: bool, // s_start armé sur disque (log en cours)
}

/// Lecture directe d'un inode sans Filesystem (avant montage / cache).
pub fn read_inode_direct(
    dev: &mut dyn crate::blockdev::BlockDevice,
    sb: &Superblock,
    ino: u32,
) -> Result<Inode> {
    if ino == 0 || ino > sb.inodes_count {
        return Err(Error::InodeNotFound(ino));
    }
    let group = ((ino - 1) / sb.inodes_per_group) as u64;
    let index = ((ino - 1) % sb.inodes_per_group) as u64;
    let gdt = crate::gdt::Gdt::load(dev, sb)?;
    let gd = gdt.get(sb, group)?;
    let bs = sb.block_size as u64;
    let ino_block = gd.inode_table + (index * sb.inode_size as u64) / bs;
    let ino_off = (index * sb.inode_size as u64) % bs;
    let mut blk = alloc::vec![0u8; bs as usize];
    dev.read_at(ino_block * bs, &mut blk)?;
    let isz = sb.inode_size as usize;
    let raw = blk[ino_off as usize..ino_off as usize + isz].to_vec();
    let inode = Inode { no: ino, raw };
    if !inode.verify_checksum(sb) {
        return Err(Error::ChecksumMismatch("inode"));
    }
    Ok(inode)
}

/// Contexte extent en lecture directe (flatten avant montage).
struct DirectCtx<'a> {
    dev: &'a mut dyn crate::blockdev::BlockDevice,
    sb: &'a Superblock,
}

impl<'a> ExtentCtx for DirectCtx<'a> {
    fn sb(&self) -> &Superblock {
        self.sb
    }
    fn read_block(&mut self, bno: u64, buf: &mut [u8]) -> Result<()> {
        self.dev.read_at(bno * self.sb.block_size as u64, buf)
    }
    fn write_block(&mut self, _bno: u64, _buf: &[u8]) -> Result<()> {
        Err(Error::ReadOnly)
    }
    fn alloc_block(&mut self) -> Result<u64> {
        Err(Error::ReadOnly)
    }
    fn free_block(&mut self, _bno: u64) -> Result<()> {
        Err(Error::ReadOnly)
    }
}

/// Mappe l'inode journal en liste de blocs fs.
pub fn journal_block_map(
    dev: &mut dyn crate::blockdev::BlockDevice,
    sb: &Superblock,
) -> Result<(alloc::vec::Vec<u64>, u64)> {
    let jino = read_inode_direct(dev, sb, sb.journal_inum)?;
    if jino.is_encrypted() {
        return Err(Error::Corrupt("journal chiffré"));
    }
    let bs = sb.block_size;
    let mut ctx = DirectCtx { dev, sb };
    let maps: alloc::vec::Vec<ExtMap> = if jino.has_extents() {
        crate::extent::flatten(&mut ctx, &jino)?
    } else {
        return Err(Error::Corrupt("journal sans extents"));
    };
    let j_blocks = (jino.size() / bs as u64) as u32;
    let mut jmap = alloc::vec![0u64; j_blocks as usize];
    for m in maps {
        for i in 0..m.len {
            let lb = m.logical + i;
            if lb < j_blocks {
                jmap[lb as usize] = m.phys + i as u64;
            }
        }
    }
    Ok((jmap, jino.size()))
}

impl Journal {
    /// Charge le journal interne (inode 8) : jmap + superblock journal.
    pub fn load(dev: &mut dyn crate::blockdev::BlockDevice, sb: &Superblock) -> Result<Journal> {
        if sb.journal_inum == 0 {
            return Err(Error::Corrupt("journal_inum = 0"));
        }
        let (jmap, _jsize) = journal_block_map(dev, sb)?;
        let bs = sb.block_size as usize;
        let mut jsb = alloc::vec![0u8; JSB_SIZE];
        let mut blk = alloc::vec![0u8; bs];
        dev.read_at(jmap[0] * sb.block_size as u64, &mut blk)?;
        jsb.copy_from_slice(&blk[..JSB_SIZE]);
        if rd32_be(&jsb, 0) != JBD2_MAGIC {
            return Err(Error::JournalCorrupt("magic superblock journal"));
        }
        let jbs = rd32_be(&jsb, 0xC);
        if jbs as usize != bs {
            return Err(Error::JournalCorrupt("blocksize journal != blocksize fs"));
        }
        let j_blocks = jmap.len() as u32;
        let maxlen = rd32_be(&jsb, 0x10).min(j_blocks);
        let first = rd32_be(&jsb, 0x14);
        if first == 0 || first >= maxlen {
            return Err(Error::JournalCorrupt("s_first invalide"));
        }
        let sequence = rd32_be(&jsb, 0x18);
        let start = rd32_be(&jsb, 0x1C);
        let incompat = rd32_be(&jsb, 0x28);
        if incompat & FEAT_INCOMPAT_FAST_COMMIT != 0 {
            return Err(Error::UnsupportedFeature("fast commit jbd2"));
        }
        let mut uuid = [0u8; 16];
        uuid.copy_from_slice(&jsb[0x30..0x40]);
        let csum_type = jsb[0x50];
        if csum_type != 0 && csum_type != 4 {
            return Err(Error::UnsupportedFeature("checksum journal non-CRC32C"));
        }
        let csum_v3 = incompat & FEAT_INCOMPAT_CSUM_V3 != 0;
        let has_csum = csum_v3 || incompat & FEAT_INCOMPAT_CSUM_V2 != 0;
        let seed = crate::crc::crc32c_update(0xFFFF_FFFF, &uuid);
        let head = rd32_be(&jsb, 0x58);
        let j = Journal {
            jmap,
            maxlen,
            first,
            start,
            sequence,
            head,
            blocksize: sb.block_size,
            has_64bit: incompat & FEAT_INCOMPAT_64BIT != 0,
            has_csum,
            csum_v3,
            seed,
            uuid,
            jsb_raw: jsb,
            armed: false,
        };
        j.verify_jsb_checksum()?;
        Ok(j)
    }

    fn verify_jsb_checksum(&self) -> Result<()> {
        if !self.has_csum {
            return Ok(());
        }
        let mut tmp = self.jsb_raw.clone();
        tmp[0xFC..0x100].fill(0);
        let c = crate::crc::crc32c_update(0xFFFF_FFFF, &tmp);
        if rd32_be(&self.jsb_raw, 0xFC) != c {
            return Err(Error::ChecksumMismatch("superblock journal"));
        }
        Ok(())
    }

    #[inline]
    fn jblock_offset(&self, lblock: u32) -> u64 {
        self.jmap[lblock as usize] * self.blocksize as u64
    }

    fn read_jblock(
        &self,
        dev: &mut dyn crate::blockdev::BlockDevice,
        lblock: u32,
    ) -> Result<alloc::vec::Vec<u8>> {
        let mut buf = alloc::vec![0u8; self.blocksize as usize];
        dev.read_at(self.jblock_offset(lblock), &mut buf)?;
        Ok(buf)
    }

    fn write_jblock(
        &self,
        dev: &mut dyn crate::blockdev::BlockDevice,
        lblock: u32,
        buf: &[u8],
    ) -> Result<()> {
        dev.write_at(self.jblock_offset(lblock), buf)
    }

    fn tag_size(&self) -> usize {
        if self.csum_v3 {
            16
        } else if self.has_64bit {
            12
        } else {
            8
        }
    }

    /// Checksum d'un bloc journal (desc/revoke) : champ tail à zéro.
    fn block_csum(&self, blk: &[u8]) -> u32 {
        let mut tmp = blk.to_vec();
        let bs = tmp.len();
        tmp[bs - 4..bs].fill(0);
        crate::crc::crc32c_update(self.seed, &tmp)
    }

    fn verify_block_csum(&self, blk: &[u8]) -> bool {
        if !self.has_csum {
            return true;
        }
        let bs = blk.len();
        rd32_be(blk, bs - 4) == self.block_csum(blk)
    }

    /// Replay : applique les transactions engagées puis remet le log à vide.
    /// Retourne le nombre de transactions rejouées.
    pub fn replay(&mut self, dev: &mut dyn crate::blockdev::BlockDevice) -> Result<u32> {
        let mut applied: u32 = 0;
        if self.start == 0 {
            // log vide : rien à rejouer
            self.finalize(dev)?;
            return Ok(0);
        }
        let total = self.maxlen;
        let mut pos = self.start;
        let mut seq = self.sequence;
        let start_pos = pos;
        let mut revoked: alloc::vec::Vec<u64> = alloc::vec::Vec::new();
        let mut pending: alloc::collections::BTreeMap<u64, alloc::vec::Vec<u8>> =
            alloc::collections::BTreeMap::new();

        loop {
            let blk = self.read_jblock(dev, pos)?;
            pos = (pos + 1) % total;
            if rd32_be(&blk, 0) != JBD2_MAGIC {
                break;
            }
            let btype = rd32_be(&blk, 4);
            let hseq = rd32_be(&blk, 8);
            if hseq != seq {
                break;
            }
            match btype {
                BT_DESCRIPTOR => {
                    if !self.verify_block_csum(&blk) {
                        break;
                    }
                    let tsize = self.tag_size();
                    let usable = self.blocksize as usize - if self.has_csum { 4 } else { 0 };
                    let mut off = HEADER_SIZE;
                    let mut first_tag = true;
                    let mut corrupt = false;
                    while off + tsize <= usable {
                        let flags = rd16_be(&blk, off + 6);
                        let bnr = rd32_be(&blk, off) as u64
                            | if (self.has_64bit || self.csum_v3) && off + 12 <= self.blocksize as usize {
                                (rd32_be(&blk, off + 8) as u64) << 32
                            } else {
                                0
                            };
                        // bloc de données suivant dans le log
                        let data = self.read_jblock(dev, pos)?;
                        pos = (pos + 1) % total;
                        if self.has_csum && !self.verify_tag_csum(&blk, off, &data, seq) {
                            corrupt = true;
                            break;
                        }
                        if flags & FLAG_DELETED == 0 {
                            let mut content = data;
                            if flags & FLAG_ESCAPE != 0 {
                                content[0..4].copy_from_slice(&JBD2_MAGIC.to_be_bytes());
                            }
                            pending.insert(bnr, content);
                        }
                        off += tsize;
                        if first_tag {
                            if flags & FLAG_SAME_UUID == 0 {
                                off += 16; // UUID explicite après le premier tag
                            }
                            first_tag = false;
                        } else if flags & FLAG_SAME_UUID == 0 {
                            off += 16;
                        }
                        if flags & FLAG_LAST != 0 {
                            break;
                        }
                    }
                    if corrupt {
                        break;
                    }
                }
                BT_REVOKE => {
                    if !self.verify_block_csum(&blk) {
                        break;
                    }
                    let count = rd32_be(&blk, 12) as usize;
                    let wsz = if self.has_64bit { 8 } else { 4 };
                    let mut off = 16usize;
                    while off + wsz <= count.min(self.blocksize as usize) {
                        let bnr = if wsz == 8 {
                            rd64_be(&blk, off)
                        } else {
                            rd32_be(&blk, off) as u64
                        };
                        revoked.push(bnr);
                        off += wsz;
                    }
                }
                BT_COMMIT => {
                    for (bnr, data) in pending.iter() {
                        if revoked.contains(bnr) {
                            continue;
                        }
                        dev.write_at(bnr * self.blocksize as u64, data)?;
                    }
                    applied += 1;
                    pending.clear();
                    seq = seq.wrapping_add(1);
                }
                _ => break,
            }
            if pos == start_pos {
                break;
            }
        }
        self.start = 0;
        self.sequence = seq;
        self.head = pos;
        self.finalize(dev)?;
        Ok(applied)
    }

    fn verify_tag_csum(&self, desc: &[u8], tag_off: usize, data: &[u8], seq: u32) -> bool {
        let mut c = self.seed;
        c = crate::crc::crc32c_update(c, &seq.to_be_bytes());
        c = crate::crc::crc32c_update(c, data);
        if self.csum_v3 {
            rd32_be(desc, tag_off + 12) == c
        } else {
            (rd16_be(desc, tag_off + 4) as u32) == (c & 0xFFFF)
        }
    }

    /// Arme le log si nécessaire (s_start = head) AVANT d'écrire les
    /// transactions : un crash pendant l'écriture laissera le replay
    /// s'arrêter proprement sur le contenu invalide.
    pub fn begin(&mut self, dev: &mut dyn crate::blockdev::BlockDevice) -> Result<()> {
        if self.armed {
            return Ok(());
        }
        self.start = self.head;
        self.write_jsb(dev)?;
        self.armed = true;
        Ok(())
    }

    /// Écrit une transaction (desc + données + commit), gère le wrap
    /// circulaire. `blocks` = (bloc fs, contenu final).
    pub fn write_txn(
        &mut self,
        dev: &mut dyn crate::blockdev::BlockDevice,
        blocks: &[(u64, alloc::vec::Vec<u8>)],
    ) -> Result<()> {
        if blocks.is_empty() {
            return Ok(());
        }
        let _total = self.maxlen;
        let seq = self.sequence;
        let bs = self.blocksize as usize;
        let tsize = self.tag_size();
        let usable = bs - if self.has_csum { 4 } else { 0 };
        let max_tags = (usable - HEADER_SIZE - 16) / tsize; // -16 : uuid du 1er tag
        debug_assert!(max_tags >= 1);

        let mut it = blocks;
        while !it.is_empty() {
            let chunk_len = it.len().min(max_tags);
            let chunk = &it[..chunk_len];
            it = &it[chunk_len..];

            // descripteur
            let mut desc = alloc::vec![0u8; bs];
            wr32_be(&mut desc, 0, JBD2_MAGIC);
            wr32_be(&mut desc, 4, BT_DESCRIPTOR);
            wr32_be(&mut desc, 8, seq);
            let mut off = HEADER_SIZE;
            for (i, (bnr, content)) in chunk.iter().enumerate() {
                let last = i + 1 == chunk.len();
                let escaped = content[0..4] == JBD2_MAGIC.to_be_bytes();
                let mut flags: u16 = 0;
                if escaped {
                    flags |= FLAG_ESCAPE;
                }
                if i > 0 {
                    flags |= FLAG_SAME_UUID;
                }
                if last {
                    flags |= FLAG_LAST;
                }
                // tag
                wr32_be(&mut desc, off, *bnr as u32);
                if self.csum_v3 {
                    wr32_be(&mut desc, off + 8, (bnr >> 32) as u32);
                } else if self.has_64bit {
                    wr32_be(&mut desc, off + 8, (bnr >> 32) as u32);
                }
                wr16_be(&mut desc, off + 6, flags);
                // checksum du tag : seq + contenu (forme journal)
                let mut c = self.seed;
                c = crate::crc::crc32c_update(c, &seq.to_be_bytes());
                c = crate::crc::crc32c_update(c, content);
                if self.csum_v3 {
                    wr32_be(&mut desc, off + 12, c);
                } else if self.has_csum {
                    wr16_be(&mut desc, off + 4, (c & 0xFFFF) as u16);
                }
                off += tsize;
                if i == 0 {
                    desc[off..off + 16].copy_from_slice(&self.uuid);
                    off += 16;
                }
            }
            if self.has_csum {
                let c = self.block_csum(&desc);
                wr32_be(&mut desc, bs - 4, c);
            }
            // écrire le descripteur (peut wraper)
            self.write_circular(dev, &desc)?;
            // données
            for (_bnr, content) in chunk.iter() {
                let mut data = content.clone();
                if data[0..4] == JBD2_MAGIC.to_be_bytes() {
                    data[0..4].fill(0);
                }
                self.write_circular(dev, &data)?;
            }
            // commit
            let mut commit = alloc::vec![0u8; bs];
            wr32_be(&mut commit, 0, JBD2_MAGIC);
            wr32_be(&mut commit, 4, BT_COMMIT);
            wr32_be(&mut commit, 8, seq);
            commit[0xC] = 0; // h_chksum_type = 0 sur disque (v2/v3)
            commit[0xD] = 0; // h_chksum_size = 0
            if self.has_csum {
                let c = crate::crc::crc32c_update(self.seed, &commit);
                wr32_be(&mut commit, 0x10, c);
            }
            self.write_circular(dev, &commit)?;
            self.sequence = self.sequence.wrapping_add(1);
        }
        Ok(())
    }

    /// Écrit un bloc dans le log en suivant le wrap circulaire.
    fn write_circular(
        &mut self,
        dev: &mut dyn crate::blockdev::BlockDevice,
        buf: &[u8],
    ) -> Result<()> {
        self.write_jblock(dev, self.head, buf)?;
        self.head = (self.head + 1) % self.maxlen;
        Ok(())
    }

    /// Remet le log à vide (s_start = 0) et écrit le superblock journal.
    pub fn finalize(&mut self, dev: &mut dyn crate::blockdev::BlockDevice) -> Result<()> {
        self.start = 0;
        self.armed = false;
        self.write_jsb(dev)
    }

    /// Écrit le superblock journal avec les champs dynamiques courants.
    fn write_jsb(&mut self, dev: &mut dyn crate::blockdev::BlockDevice) -> Result<()> {
        let mut jsb = self.jsb_raw.clone();
        wr32_be(&mut jsb, 0x18, self.sequence);
        wr32_be(&mut jsb, 0x1C, self.start);
        wr32_be(&mut jsb, 0x58, self.head);
        wr32_be(&mut jsb, 0x20, 0); // s_errno
        if self.has_csum {
            jsb[0xFC..0x100].fill(0);
            let c = crate::crc::crc32c_update(0xFFFF_FFFF, &jsb);
            wr32_be(&mut jsb, 0xFC, c);
        }
        self.jsb_raw = jsb.clone();
        let mut blk = alloc::vec![0u8; self.blocksize as usize];
        let n = jsb.len().min(blk.len());
        blk[..n].copy_from_slice(&jsb[..n]);
        self.write_jblock(dev, 0, &blk)
    }
}
