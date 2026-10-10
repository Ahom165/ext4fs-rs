//! Superblock ext4 (1024 octets à l'offset 1024 du volume).

use crate::crc::crc32c_update;
use crate::error::{Error, Result};
use crate::util::*;

pub const SB_OFFSET: u64 = 1024;
pub const SB_SIZE: usize = 1024;
pub const EXT4_MAGIC: u16 = 0xEF53;

// ---- Feature bits ---------------------------------------------------------
pub mod feat_compat {
    pub const HAS_JOURNAL: u32 = 0x4;
    pub const EXT_ATTR: u32 = 0x8;
    pub const RESIZE_INODE: u32 = 0x10;
    pub const DIR_INDEX: u32 = 0x20;
}
pub mod feat_incompat {
    pub const FILETYPE: u32 = 0x2;
    pub const RECOVER: u32 = 0x4;
    pub const JOURNAL_DEV: u32 = 0x8;
    pub const META_BG: u32 = 0x10;
    pub const EXTENTS: u32 = 0x40;
    pub const B64BIT: u32 = 0x80;
    pub const MMP: u32 = 0x100;
    pub const FLEX_BG: u32 = 0x200;
    pub const EA_INODE: u32 = 0x400;
    pub const DIRDATA: u32 = 0x1000;
    pub const CSUM_SEED: u32 = 0x2000;
    pub const LARGEDIR: u32 = 0x4000;
    pub const INLINE_DATA: u32 = 0x8000;
    pub const ENCRYPT: u32 = 0x1_0000;
}
pub mod feat_ro {
    pub const SPARSE_SUPER: u32 = 0x1;
    pub const LARGE_FILE: u32 = 0x2;
    pub const HUGE_FILE: u32 = 0x8;
    pub const GDT_CSUM: u32 = 0x10;
    pub const DIR_NLINK: u32 = 0x20;
    pub const EXTRA_ISIZE: u32 = 0x40;
    pub const QUOTA: u32 = 0x100;
    pub const BIGALLOC: u32 = 0x200;
    pub const METADATA_CSUM: u32 = 0x400;
    pub const PROJECT: u32 = 0x2000;
    pub const VERITY: u32 = 0x8000;
}

// ---- s_state ---------------------------------------------------------------
pub const STATE_VALID: u16 = 0x1;
pub const STATE_ERROR: u16 = 0x2;
pub const STATE_ORPHAN: u16 = 0x4;

/// Superblock parsé (les octets bruts sont conservés pour écriture fidèle).
#[derive(Clone, Debug)]
pub struct Superblock {
    pub raw: [u8; SB_SIZE],
    pub block_size: u32,
    pub blocks_count: u64,
    pub free_blocks_count: u64,
    pub free_inodes_count: u32,
    pub inodes_count: u32,
    pub first_data_block: u32,
    pub blocks_per_group: u32,
    pub inodes_per_group: u32,
    pub inode_size: u16,
    pub desc_size: u16,
    pub feature_compat: u32,
    pub feature_incompat: u32,
    pub feature_ro_compat: u32,
    pub csum_seed: u32,
    pub hash_seed: [u32; 4],
    pub journal_inum: u32,
    pub journal_uuid: [u8; 16],
    pub groups_count: u64,
}

impl Superblock {
    /// Parse les 1024 octets du superblock depuis la position 1024 du volume.
    pub fn parse(raw: [u8; SB_SIZE]) -> Result<Superblock> {
        if rd16_le(&raw, 0x38) != EXT4_MAGIC {
            return Err(Error::BadMagic);
        }
        let log_bs = rd32_le(&raw, 0x18);
        if log_bs > 6 {
            return Err(Error::Corrupt("log_block_size hors limites"));
        }
        let block_size: u32 = 1024 << log_bs;
        let blocks_count = rd32_le(&raw, 0x4) as u64 | (rd32_le(&raw, 0x150) as u64) << 32;
        let first_data_block = rd32_le(&raw, 0x14);
        let blocks_per_group = rd32_le(&raw, 0x20);
        if blocks_per_group == 0 {
            return Err(Error::Corrupt("blocks_per_group = 0"));
        }
        let inodes_per_group = rd32_le(&raw, 0x28);
        if inodes_per_group == 0 {
            return Err(Error::Corrupt("inodes_per_group = 0"));
        }
        let inode_size = if rd32_le(&raw, 0x4C) < 1 {
            // rev 0 : 128 octets
            128
        } else {
            rd16_le(&raw, 0x58)
        };
        let b64 = rd32_le(&raw, 0x60) & feat_incompat::B64BIT != 0;
        let desc_size = if b64 {
            let d = rd16_le(&raw, 0xFE);
            if d < 64 {
                return Err(Error::Corrupt("desc_size < 64 avec 64BIT"));
            }
            d
        } else {
            32
        };
        let feature_incompat = rd32_le(&raw, 0x60);
        let feature_ro_compat = rd32_le(&raw, 0x64);
        let blocks_count_eff = if feature_incompat & feat_incompat::B64BIT != 0 {
            blocks_count
        } else {
            blocks_count & 0xFFFF_FFFF
        };
        let groups_count = (blocks_count_eff - first_data_block as u64 + blocks_per_group as u64
            - 1)
            / blocks_per_group as u64;

        let mut uuid = [0u8; 16];
        uuid.copy_from_slice(&raw[0x68..0x78]);
        let csum_seed = if feature_incompat & feat_incompat::CSUM_SEED != 0 {
            rd32_le(&raw, 0x270)
        } else {
            crc32c_update(0xFFFF_FFFF, &uuid)
        };
        let mut hash_seed = [0u32; 4];
        for (i, h) in hash_seed.iter_mut().enumerate() {
            *h = rd32_le(&raw, 0xEC + i * 4);
        }
        let mut journal_uuid = [0u8; 16];
        journal_uuid.copy_from_slice(&raw[0xD0..0xE0]);

        let sb = Superblock {
            raw,
            block_size,
            blocks_count: blocks_count_eff,
            free_blocks_count: rd32_le(&raw, 0xC) as u64 | (rd32_le(&raw, 0x158) as u64) << 32,
            free_inodes_count: rd32_le(&raw, 0x10),
            inodes_count: rd32_le(&raw, 0),
            first_data_block,
            blocks_per_group,
            inodes_per_group,
            inode_size,
            desc_size,
            feature_compat: rd32_le(&raw, 0x5C),
            feature_incompat,
            feature_ro_compat,
            csum_seed,
            hash_seed,
            journal_inum: rd32_le(&raw, 0xE0),
            journal_uuid,
            groups_count,
        };
        Ok(sb)
    }

    // ---- Accesseurs features ----
    pub fn has(&self, m: u32) -> bool {
        self.feature_compat & m != 0
    }
    pub fn has_incompat(&self, m: u32) -> bool {
        self.feature_incompat & m != 0
    }
    /// Modifie s_feature_incompat (champ + octets bruts synchronisés).
    pub fn set_incompat(&mut self, mask: u32, on: bool) {
        if on {
            self.feature_incompat |= mask;
        } else {
            self.feature_incompat &= !mask;
        }
        wr32_le(&mut self.raw, 0x60, self.feature_incompat);
    }
    pub fn has_ro(&self, m: u32) -> bool {
        self.feature_ro_compat & m != 0
    }
    pub fn has_metadata_csum(&self) -> bool {
        self.has_ro(feat_ro::METADATA_CSUM)
    }
    pub fn has_journal(&self) -> bool {
        self.has(feat_compat::HAS_JOURNAL)
    }
    pub fn has_64bit(&self) -> bool {
        self.has_incompat(feat_incompat::B64BIT)
    }
    pub fn has_filetype(&self) -> bool {
        self.has_incompat(feat_incompat::FILETYPE)
    }
    pub fn has_extents(&self) -> bool {
        self.has_incompat(feat_incompat::EXTENTS)
    }
    pub fn has_inline_data(&self) -> bool {
        self.has_incompat(feat_incompat::INLINE_DATA)
    }
    pub fn has_largedir(&self) -> bool {
        self.has_incompat(feat_incompat::LARGEDIR)
    }
    pub fn has_encrypt(&self) -> bool {
        self.has_incompat(feat_incompat::ENCRYPT)
    }
    pub fn has_dir_index(&self) -> bool {
        self.has(feat_compat::DIR_INDEX)
    }

    /// Contrôle des fonctionnalités critiques au montage.
    pub fn check_supported(&self) -> Result<()> {
        if self.has_ro(feat_ro::BIGALLOC) {
            return Err(Error::UnsupportedFeature("BIGALLOC (clusters)"));
        }
        if self.has_incompat(feat_incompat::META_BG) {
            return Err(Error::UnsupportedFeature("META_BG"));
        }
        if self.has_incompat(feat_incompat::MMP) {
            return Err(Error::UnsupportedFeature("MMP (multi-mount protection)"));
        }
        if self.has_incompat(feat_incompat::JOURNAL_DEV) {
            return Err(Error::ExternalJournal);
        }
        if !self.has_extents() {
            return Err(Error::UnsupportedFeature("sans EXTENTS (ext2/3 purs)"));
        }
        if self.state() & STATE_ORPHAN != 0 {
            return Err(Error::OrphansPending);
        }
        Ok(())
    }

    // ---- Champs simples ----
    pub fn state(&self) -> u16 {
        rd16_le(&self.raw, 0x3A)
    }
    pub fn set_state(&mut self, v: u16) {
        wr16_le(&mut self.raw, 0x3A, v);
    }
    pub fn mnt_count(&self) -> u16 {
        rd16_le(&self.raw, 0x34)
    }
    pub fn set_mnt_count(&mut self, v: u16) {
        wr16_le(&mut self.raw, 0x34, v);
    }
    pub fn volume_name(&self) -> alloc::string::String {
        let end = self.raw[0x78..0x88]
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(16);
        alloc::string::String::from_utf8_lossy(&self.raw[0x78..0x78 + end]).into_owned()
    }
    pub fn uuid(&self) -> [u8; 16] {
        let mut u = [0u8; 16];
        u.copy_from_slice(&self.raw[0x68..0x78]);
        u
    }

    // ---- Compteurs libres (lo+hi) ----
    /// Met à jour le raw ET le champ cache (sinon les appels suivants
    /// repartent de la valeur périmée du montage).
    pub fn set_free_blocks(&mut self, v: u64) {
        self.free_blocks_count = v;
        wr32_le(&mut self.raw, 0xC, v as u32);
        wr32_le(&mut self.raw, 0x158, (v >> 32) as u32);
    }
    pub fn set_free_inodes(&mut self, v: u32) {
        self.free_inodes_count = v;
        wr32_le(&mut self.raw, 0x10, v);
    }
    pub fn blocks_count_minus_fdb(&self) -> u64 {
        self.blocks_count - self.first_data_block as u64
    }
    /// Nombre de blocs du groupe `g` (le dernier groupe peut être tronqué).
    pub fn blocks_in_group(&self, g: u64) -> u64 {
        let total = self.blocks_count_minus_fdb();
        let start = g * self.blocks_per_group as u64;
        let rest = total.saturating_sub(start);
        rest.min(self.blocks_per_group as u64)
    }

    /// Recalcule et pose s_checksum (metadata_csum).
    /// Convention kernel/e2fsprogs : crc32c(~0, sb[0..0x3FC)), état courant.
    pub fn recompute_checksum(&mut self) {
        if !self.has_metadata_csum() {
            return;
        }
        let c = crate::crc::crc32c_update(0xFFFF_FFFF, &self.raw[..0x3FC]);
        wr32_le(&mut self.raw, 0x3FC, c);
    }

    /// Vérifie le checksum du superblock.
    pub fn verify_checksum(&self) -> bool {
        if !self.has_metadata_csum() {
            return true;
        }
        let stored = crate::util::rd32_le(&self.raw, 0x3FC);
        let c = crate::crc::crc32c_update(0xFFFF_FFFF, &self.raw[..0x3FC]);
        stored == c
    }

    /// Crc de référence de l'hôte pour les tests : reflète ce que e2fsck attend.
    pub fn checksum_value(&self) -> u32 {
        crate::crc::crc32c_update(0xFFFF_FFFF, &self.raw[..0x3FC])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_bad_magic() {
        let mut raw = [0u8; 1024];
        wr16_le(&mut raw, 0x38, 0x1234);
        assert_eq!(Superblock::parse(raw).unwrap_err(), Error::BadMagic);
    }
}
