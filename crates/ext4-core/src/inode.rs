//! Inodes ext4 : parse/encode, checksums, types de fichiers, inline_data.

use alloc::vec::Vec;

use crate::crc::crc32c_update;
use crate::error::{Error, Result};
use crate::superblock::Superblock;
use crate::util::*;

// ---- Modes ----------------------------------------------------------------
pub const S_IFMT: u16 = 0xF000;
pub const S_IFREG: u16 = 0x8000;
pub const S_IFDIR: u16 = 0x4000;
pub const S_IFLNK: u16 = 0xA000;
pub const S_IFCHR: u16 = 0x2000;
pub const S_IFBLK: u16 = 0x6000;
pub const S_IFIFO: u16 = 0x1000;
pub const S_IFSOCK: u16 = 0xC000;

pub const FI_EXTENTS: u32 = 0x8_0000;
pub const FI_INLINE_DATA: u32 = 0x1000_0000;
pub const FI_ENCRYPT: u32 = 0x800;

/// Champ `i_block` (60 octets) interprété comme inline data ou racine extents.
pub const INLINE_MAX: usize = 60;

/// Copie d'un inode indépendante du disque.
#[derive(Clone, Debug)]
pub struct Inode {
    pub no: u32,
    pub raw: Vec<u8>, // taille = sb.inode_size
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileType {
    Regular,
    Dir,
    Symlink,
    CharDev,
    BlockDev,
    Fifo,
    Socket,
}

impl FileType {
    pub fn from_mode(mode: u16) -> Option<FileType> {
        Some(match mode & S_IFMT {
            S_IFREG => FileType::Regular,
            S_IFDIR => FileType::Dir,
            S_IFLNK => FileType::Symlink,
            S_IFCHR => FileType::CharDev,
            S_IFBLK => FileType::BlockDev,
            S_IFIFO => FileType::Fifo,
            S_IFSOCK => FileType::Socket,
            _ => return None,
        })
    }
    /// file_type attendu dans les entrées de répertoire (INCOMPAT_FILETYPE).
    pub fn dirent_type(self) -> u8 {
        match self {
            FileType::Regular => 1,
            FileType::Dir => 2,
            FileType::CharDev => 3,
            FileType::BlockDev => 4,
            FileType::Fifo => 5,
            FileType::Socket => 6,
            FileType::Symlink => 7,
        }
    }
}

impl Inode {
    pub fn new(no: u32, inode_size: usize) -> Inode {
        Inode {
            no,
            raw: alloc::vec![0u8; inode_size],
        }
    }

    // ---- Champs de base ----
    pub fn mode(&self) -> u16 {
        rd16_le(&self.raw, 0)
    }
    pub fn set_mode(&mut self, m: u16) {
        wr16_le(&mut self.raw, 0, m);
    }
    pub fn file_type(&self) -> Option<FileType> {
        FileType::from_mode(self.mode())
    }
    pub fn uid(&self) -> u32 {
        rd16_le(&self.raw, 2) as u32 | (rd16_le(&self.raw, 0x78) as u32) << 16
    }
    pub fn gid(&self) -> u32 {
        rd16_le(&self.raw, 0x18) as u32 | (rd16_le(&self.raw, 0x7A) as u32) << 16
    }
    pub fn size(&self) -> u64 {
        match self.file_type() {
            Some(FileType::Dir) | Some(FileType::Regular) | Some(FileType::Symlink) => {
                rd32_le(&self.raw, 4) as u64 | (rd32_le(&self.raw, 0x6C) as u64) << 32
            }
            _ => rd32_le(&self.raw, 4) as u64,
        }
    }
    pub fn set_size(&mut self, v: u64) {
        wr32_le(&mut self.raw, 4, v as u32);
        wr32_le(&mut self.raw, 0x6C, (v >> 32) as u32);
    }
    pub fn links_count(&self) -> u16 {
        rd16_le(&self.raw, 0x1A)
    }
    pub fn set_links_count(&mut self, v: u16) {
        wr16_le(&mut self.raw, 0x1A, v);
    }
    /// i_blocks : secteurs 512 octets, métadonnées incluses.
    pub fn blocks(&self) -> u64 {
        rd32_le(&self.raw, 0x1C) as u64 | (rd16_le(&self.raw, 0x74) as u64) << 32
    }
    pub fn set_blocks(&mut self, v: u64) {
        wr32_le(&mut self.raw, 0x1C, v as u32);
        wr16_le(&mut self.raw, 0x74, (v >> 32) as u16);
    }
    pub fn flags(&self) -> u32 {
        rd32_le(&self.raw, 0x20)
    }
    pub fn set_flags(&mut self, v: u32) {
        wr32_le(&mut self.raw, 0x20, v);
    }
    pub fn atime(&self) -> u32 {
        rd32_le(&self.raw, 8)
    }
    pub fn ctime(&self) -> u32 {
        rd32_le(&self.raw, 0xC)
    }
    pub fn mtime(&self) -> u32 {
        rd32_le(&self.raw, 0x10)
    }
    pub fn dtime(&self) -> u32 {
        rd32_le(&self.raw, 0x14)
    }
    pub fn set_atime(&mut self, v: u32) {
        wr32_le(&mut self.raw, 8, v);
    }
    pub fn set_ctime(&mut self, v: u32) {
        wr32_le(&mut self.raw, 0xC, v);
    }
    pub fn set_mtime(&mut self, v: u32) {
        wr32_le(&mut self.raw, 0x10, v);
    }
    pub fn set_dtime(&mut self, v: u32) {
        wr32_le(&mut self.raw, 0x14, v);
    }
    pub fn generation(&self) -> u32 {
        rd32_le(&self.raw, 0x64)
    }
    pub fn set_generation(&mut self, v: u32) {
        wr32_le(&mut self.raw, 0x64, v);
    }
    /// Pointeur vers le bloc de xattrs (0 = aucun).
    pub fn file_acl(&self) -> u64 {
        rd32_le(&self.raw, 0x68) as u64 | (rd16_le(&self.raw, 0x76) as u64) << 32
    }
    pub fn set_file_acl(&mut self, v: u64) {
        wr32_le(&mut self.raw, 0x68, v as u32);
        wr16_le(&mut self.raw, 0x76, (v >> 32) as u16);
    }

    pub fn is_inline(&self) -> bool {
        self.flags() & FI_INLINE_DATA != 0
    }
    pub fn has_extents(&self) -> bool {
        self.flags() & FI_EXTENTS != 0
    }
    pub fn is_encrypted(&self) -> bool {
        self.flags() & FI_ENCRYPT != 0
    }

    pub fn i_block(&self) -> &[u8] {
        &self.raw[0x28..0x64]
    }
    pub fn i_block_mut(&mut self) -> &mut [u8] {
        &mut self.raw[0x28..0x64]
    }

    /// i_extra_isize (>= 0x80 présent ?).
    pub fn extra_isize(&self) -> u16 {
        rd16_le(&self.raw, 0x80)
    }

    /// Checksum d'inode (metadata_csum) : recalcul sur l'inode brut avec
    /// i_checksum_lo (0x7C) et i_checksum_hi (0x82) mis à zéro.
    pub fn checksum(&self, sb: &Superblock) -> u32 {
        let mut c = sb.csum_seed;
        c = crc32c_update(c, &self.no.to_le_bytes());
        c = crc32c_update(c, &self.generation().to_le_bytes());
        let mut tmp = self.raw.clone();
        let isz = tmp.len();
        if isz >= 0x7E {
            tmp[0x7C..0x7E].fill(0);
        }
        if isz >= 0x84 && self.extra_isize() >= 0x04 {
            tmp[0x82..0x84].fill(0);
        }
        c = crc32c_update(c, &tmp);
        c
    }

    pub fn verify_checksum(&self, sb: &Superblock) -> bool {
        if !sb.has_metadata_csum() {
            return true;
        }
        let want = self.checksum(sb);
        let lo = rd16_le(&self.raw, 0x7C) as u32;
        let has_hi = self.raw.len() >= 0x84 && self.extra_isize() >= 0x04;
        if has_hi {
            let hi = rd16_le(&self.raw, 0x82) as u32;
            want == (lo | hi << 16)
        } else {
            (want & 0xFFFF) == lo
        }
    }

    /// Pose le checksum d'inode.
    pub fn store_checksum(&mut self, sb: &Superblock) {
        if !sb.has_metadata_csum() {
            return;
        }
        let want = self.checksum(sb);
        wr16_le(&mut self.raw, 0x7C, (want & 0xFFFF) as u16);
        if self.raw.len() >= 0x84 && self.extra_isize() >= 0x04 {
            wr16_le(&mut self.raw, 0x82, (want >> 16) as u16);
        }
    }

    /// Contenu inline (i_block) — retourne None si l'inode n'est pas inline.
    pub fn inline_data(&self) -> Option<&[u8]> {
        if !self.is_inline() {
            return None;
        }
        let sz = self.size() as usize;
        if sz <= INLINE_MAX {
            Some(&self.raw[0x28..0x28 + sz])
        } else {
            None // inline étendu via xattr system.data : lecture seule directe non gérée ici
        }
    }
}
