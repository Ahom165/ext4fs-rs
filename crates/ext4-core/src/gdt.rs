//! Descripteurs de groupes de blocs (GDT) — format 32 ou 64 octets,
//! checksums metadata_csum.

use alloc::vec::Vec;

use crate::error::{Error, Result};
use crate::superblock::Superblock;
use crate::crc::crc32c_update;
use crate::util::*;

/// Copie d'un descripteur de groupe, indépendante du format disque.
#[derive(Clone, Debug, Default)]
pub struct GroupDesc {
    pub block_bitmap: u64,
    pub inode_bitmap: u64,
    pub inode_table: u64,
    pub free_blocks_count: u32,
    pub free_inodes_count: u32,
    pub used_dirs_count: u32,
    pub flags: u16,
    pub itable_unused: u32,
    pub block_bitmap_csum: u32,
    pub inode_bitmap_csum: u32,
    pub checksum: u16,
}

pub const GD_FLAGS_INODE_UNINIT: u16 = 0x1;
pub const GD_FLAGS_BLOCK_UNINIT: u16 = 0x2;
pub const GD_FLAGS_INODE_ZEROED: u16 = 0x4;

impl GroupDesc {
    /// Parse depuis un buffer GDT à `off` (desc de `desc_size` octets).
    pub fn parse(buf: &[u8], off: usize, desc_size: u16) -> GroupDesc {
        let lo = |o: usize| rd32_le(buf, off + o) as u64;
        let mut gd = GroupDesc {
            block_bitmap: lo(0),
            inode_bitmap: lo(4),
            inode_table: lo(8),
            free_blocks_count: rd16_le(buf, off + 0xC) as u32,
            free_inodes_count: rd16_le(buf, off + 0xE) as u32,
            used_dirs_count: rd16_le(buf, off + 0x10) as u32,
            flags: rd16_le(buf, off + 0x12),
            itable_unused: 0,
            block_bitmap_csum: rd16_le(buf, off + 0x18) as u32,
            inode_bitmap_csum: rd16_le(buf, off + 0x1A) as u32,
            checksum: rd16_le(buf, off + 0x1E),
        };
        if desc_size >= 64 {
            gd.block_bitmap |= lo(0x20) << 32;
            gd.inode_bitmap |= lo(0x24) << 32;
            gd.inode_table |= lo(0x28) << 32;
            gd.free_blocks_count = (rd16_le(buf, off + 0x30) as u32) << 16 | gd.free_blocks_count;
            gd.free_inodes_count = (rd16_le(buf, off + 0x32) as u32) << 16 | gd.free_inodes_count;
            gd.used_dirs_count = (rd16_le(buf, off + 0x34) as u32) << 16 | gd.used_dirs_count;
            gd.itable_unused = rd16_le(buf, off + 0x1C) as u32 | (rd16_le(buf, off + 0x36) as u32) << 16;
            gd.block_bitmap_csum = (rd16_le(buf, off + 0x38) as u32) << 16 | gd.block_bitmap_csum;
            gd.inode_bitmap_csum = (rd16_le(buf, off + 0x3A) as u32) << 16 | gd.inode_bitmap_csum;
        }
        gd
    }

    /// Sérialise dans un buffer GDT (high words seulement si desc >= 64).
    pub fn encode(&self, buf: &mut [u8], off: usize, sb: &Superblock) {
        let w32 = |b: &mut [u8], o: usize, v: u64| wr32_le(b, off + o, v as u32);
        w32(buf, 0, self.block_bitmap);
        w32(buf, 4, self.inode_bitmap);
        w32(buf, 8, self.inode_table);
        wr16_le(buf, off + 0xC, self.free_blocks_count as u16);
        wr16_le(buf, off + 0xE, self.free_inodes_count as u16);
        wr16_le(buf, off + 0x10, self.used_dirs_count as u16);
        wr16_le(buf, off + 0x12, self.flags);
        wr16_le(buf, off + 0x18, self.block_bitmap_csum as u16);
        wr16_le(buf, off + 0x1A, self.inode_bitmap_csum as u16);
        wr16_le(buf, off + 0x1E, self.checksum);
        if sb.desc_size >= 64 {
            wr32_le(buf, off + 0x20, (self.block_bitmap >> 32) as u32);
            wr32_le(buf, off + 0x24, (self.inode_bitmap >> 32) as u32);
            wr32_le(buf, off + 0x28, (self.inode_table >> 32) as u32);
            wr16_le(buf, off + 0x30, (self.free_blocks_count >> 16) as u16);
            wr16_le(buf, off + 0x32, (self.free_inodes_count >> 16) as u16);
            wr16_le(buf, off + 0x34, (self.used_dirs_count >> 16) as u16);
            wr16_le(buf, off + 0x1C, self.itable_unused as u16);
            wr16_le(buf, off + 0x36, (self.itable_unused >> 16) as u16);
            wr16_le(buf, off + 0x38, (self.block_bitmap_csum >> 16) as u16);
            wr16_le(buf, off + 0x3A, (self.inode_bitmap_csum >> 16) as u16);
        }
    }
}

/// Checksum d'un descripteur de groupe. metadata_csum : crc32c(seed, numéro
/// de groupe + descripteur avec champ checksum à zéro), tronqué à 16 bits.
pub fn group_desc_csum(sb: &Superblock, group: u64, raw_desc: &[u8]) -> u16 {
    if sb.has_metadata_csum() {
        let mut c = sb.csum_seed;
        c = crc32c_update(c, &(group as u32).to_le_bytes());
        c = crc32c_update(c, raw_desc);
        (c & 0xFFFF) as u16
    } else {
        // GDT_CSUM legacy : crc16(uuid + group + desc hors checksum)
        let mut data = alloc::vec::Vec::with_capacity(24 + raw_desc.len());
        data.extend_from_slice(&sb.uuid());
        data.extend_from_slice(&group.to_le_bytes());
        data.extend_from_slice(raw_desc);
        crate::crc::crc16(&data)
    }
}

/// Checksum d'un bitmap de blocs ou d'inodes (metadata_csum).
pub fn bitmap_csum(sb: &Superblock, data: &[u8]) -> u32 {
    if !sb.has_metadata_csum() {
        return 0;
    }
    crate::crc::crc32c_update(sb.csum_seed, data)
}

/// Mise à jour des champs de checksum bitmap dans un descripteur.
pub fn set_bitmap_csums(sb: &Superblock, gd: &mut GroupDesc, block_bitmap: bool, data: &[u8]) {
    if !sb.has_metadata_csum() {
        return;
    }
    let c = bitmap_csum(sb, data);
    if block_bitmap {
        gd.block_bitmap_csum = c;
    } else {
        gd.inode_bitmap_csum = c;
    }
}

pub struct Gdt {
    pub raw: Vec<u8>, // GDT primaire complète (groups_count * desc_size)
    pub primary_block: u64,
}

impl Gdt {
    /// Lit la GDT primaire (commence au bloc `first_data_block + 1`).
    pub fn load(dev: &mut dyn crate::blockdev::BlockDevice, sb: &Superblock) -> Result<Gdt> {
        let len = sb.groups_count as usize * sb.desc_size as usize;
        let gdt_blocks =
            (len + sb.block_size as usize - 1) / sb.block_size as usize;
        let start_block = (sb.first_data_block + 1) as u64;
        let mut raw = alloc::vec![0u8; gdt_blocks * sb.block_size as usize];
        dev.read_at(start_block * sb.block_size as u64, &mut raw)?;
        Ok(Gdt {
            raw,
            primary_block: start_block,
        })
    }

    pub fn get(&self, sb: &Superblock, group: u64) -> Result<GroupDesc> {
        if group >= sb.groups_count {
            return Err(Error::Corrupt("numéro de groupe hors limites"));
        }
        let off = group as usize * sb.desc_size as usize;
        if off + sb.desc_size as usize > self.raw.len() {
            return Err(Error::Corrupt("descripteur de groupe tronqué"));
        }
        Ok(GroupDesc::parse(&self.raw, off, sb.desc_size))
    }

    /// Remplace le descripteur `group` et recalcule son checksum.
    pub fn put(&mut self, sb: &Superblock, group: u64, gd: &GroupDesc) -> Result<()> {
        if group >= sb.groups_count {
            return Err(Error::Corrupt("numéro de groupe hors limites"));
        }
        let off = group as usize * sb.desc_size as usize;
        let dsz = sb.desc_size as usize;
        // Encodage puis checksum couvrant bg_checksum (offset 0x1E) à zéro.
        gd.encode(&mut self.raw, off, sb);
        self.raw[off + 0x1E..off + 0x20].fill(0);
        let csum = group_desc_csum(sb, group, &self.raw[off..off + dsz]);
        wr16_le(&mut self.raw, off + 0x1E, csum);
        Ok(())
    }

    pub fn verify_checksums(&mut self, sb: &Superblock) -> Result<()> {
        if !sb.has_metadata_csum() {
            return Ok(());
        }
        let dsz = sb.desc_size as usize;
        for g in 0..sb.groups_count {
            let off = g as usize * dsz;
            let stored = rd16_le(&self.raw, off + 0x1E);
            self.raw[off + 0x1E..off + 0x20].fill(0);
            let want = group_desc_csum(sb, g, &self.raw[off..off + dsz]);
            wr16_le(&mut self.raw, off + 0x1E, stored);
            if stored != want {
                return Err(Error::ChecksumMismatch("group descriptor"));
            }
        }
        Ok(())
    }

    /// Écrit la GDT primaire (les sauvegardes éparses ne sont pas mises à
    /// jour ; e2fsck les resynchronise au besoin).
    pub fn store(&self, dev: &mut dyn crate::blockdev::BlockDevice, sb: &Superblock) -> Result<()> {
        dev.write_at(self.primary_block * sb.block_size as u64, &self.raw)
    }
}
