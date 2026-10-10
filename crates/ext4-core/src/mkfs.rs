//! mkfs ext4 : construction complète d'un système de fichiers (superblock
//! + sauvegardes éparses, GDT 64 octets + backups, bitmaps + checksums,
//! tables d'inodes, root + lost+found, journal jbd2 v3, checksums
//! metadata_csum). Calibré e2fsprogs 1.47.2 : la sortie passe `e2fsck -fn`
//! et se monte par le moteur comme par debugfs.
//!
//! Choix v1 (documentés dans docs/DESIGN.md §16) :
//! - pas de resize_inode (pas de blocs GDT réservés) ;
//! - pas de flex_bg (métadonnées dans leur groupe, placement traditionnel) ;
//! - pas de metadata_csum_seed (seed dérivé de l'uuid, convention noyau) ;
//! - 64bit + desc_size 64, dir_index/half_md4, extents, filetype ;
//! - le journal (inode 8) tient dans ≤ 4 bandes contiguës (≤ 4 extents).
//!
//! Conventions checksum (calibrées) :
//! - sb : crc32c(~0, raw[0..0x3FC)) à 0x3FC ;
//! - GDT : crc32c(seed, groupe u32 + desc avec checksum à zéro) tronqué 16 bits ;
//! - bitmaps : crc32c(seed, (blocs+7)/8 octets pour les blocs = bloc entier
//!   quand bpg = 8·bs ; (ipg+7)/8 octets pour les inodes) ;
//! - inodes : via `Inode::store_checksum` (0x7C/0x82) ;
//! - répertoires : tail 12 octets via `write_dirent_tail` ;
//! - jsb : crc32c(~0, 1024 octets avec 0xFC..0x100 à zéro), gros-boutiste,
//!   sans complément final (convention jbd2, cf. journal.rs::write_jsb).

use alloc::vec::Vec;

use crate::blockdev::BlockDevice;
use crate::dir::{inode_csum_seed, write_dirent_tail};
use crate::error::{Error, Result};
use crate::gdt::{group_desc_csum, set_bitmap_csums, GroupDesc, GD_FLAGS_INODE_ZEROED};
use crate::inode::{Inode, FI_EXTENTS, S_IFDIR, S_IFREG};
use crate::journal::{BT_SUPERBLOCK_V2, JBD2_MAGIC};
use crate::superblock::{
    feat_compat, feat_incompat, feat_ro, STATE_VALID, SB_OFFSET, Superblock,
};
use crate::util::*;

/// Inode racine.
pub const ROOT_INO: u32 = 2;
/// Inode du journal (has_journal).
pub const JOURNAL_INO: u32 = 8;
/// Premier inode utilisateur (lost+found).
pub const FIRST_INO: u32 = 11;
/// Taille d'inode (rev 1, ext4 moderne).
pub const INODE_SIZE: u32 = 256;

/// Options de formatage.
#[derive(Clone, Debug)]
pub struct MkfsOptions {
    /// log2(bs/1024) : 0 = 1K, 1 = 2K, 2 = 4K (…6 = 64K).
    pub block_log: u8,
    /// Nom de volume (UTF-8, tronqué à 16 octets).
    pub label: Vec<u8>,
    /// UUID du système de fichiers (16 octets ; zéro → dérivé déterministe).
    /// NOTE : le journal interne ne référence PAS son UUID dans le
    /// superblock (s_journal_uuid = 0 sinon e2fsck cherche un journal
    /// externe) et le jsb recopie l'UUID du FS (mkjournal.c:72).
    pub uuid: [u8; 16],
    /// Taille du journal en MiB (0 = volume sans journal).
    pub journal_mib: u32,
    /// Ratio octets/inode (défaut 16384).
    pub inode_ratio: u32,
    /// Seed de hachage HTree (zéros → dérivé de l'uuid).
    pub hash_seed: [u32; 4],
    /// Version de hachage dx (1 = half_md4, défaut e2fsprogs).
    pub hash_version: u8,
    /// Pourcentage de blocs réservés (défaut 5).
    pub reserved_pct: u32,
}

impl Default for MkfsOptions {
    fn default() -> Self {
        MkfsOptions {
            block_log: 2,
            label: Vec::new(),
            uuid: [0; 16],
            journal_mib: 4,
            inode_ratio: 16384,
            hash_seed: [0; 4],
            hash_version: 1,
            reserved_pct: 5,
        }
    }
}

/// Géométrie calculée.
struct Geo {
    bs: u32,
    fdb: u32,
    bpg: u32,
    ipg: u32,
    itable_blocks: u32,
    groups: u64,
    gdt_blocks: u64,
    total_blocks: u64,
}

impl Geo {
    /// Bloc absolu du début du groupe g.
    fn group_start(&self, g: u64) -> u64 {
        self.fdb as u64 + g * self.bpg as u64
    }
    /// Nombre de blocs du groupe g (le dernier peut être tronqué).
    fn group_len(&self, g: u64) -> u64 {
        let total = self.total_blocks - self.fdb as u64;
        let rest = total.saturating_sub(g * self.bpg as u64);
        rest.min(self.bpg as u64)
    }
    /// Groupe de sauvegarde sb+GDT ? (g == 1 ou puissance de 3/5/7, sparse_super)
    fn is_backup(&self, g: u64) -> bool {
        if g == 0 {
            return false;
        }
        g == 1 || g == 3 || g == 5 || g == 7 || g == 9 || g == 25 || g == 27
            || g == 49 || g == 81 || g == 125 || g == 243 || g == 343 || g == 625
            || g == 729 || g == 2187 || g == 3125
    }
}

/// Disposition d'un groupe.
struct GroupLayout {
    start: u64,
    len: u64,
    /// Bloc portant le superblock (principal ou sauvegarde).
    sb_block: Option<u64>,
    /// Début de la GDT (primaire ou sauvegarde).
    gdt_start: Option<u64>,
    bbitmap: u64,
    ibitmap: u64,
    itable: u64,
    data_from: u64,
    data_to: u64,
}

fn layout_group(geo: &Geo, g: u64) -> GroupLayout {
    let start = geo.group_start(g);
    let len = geo.group_len(g);
    let backup = geo.is_backup(g);
    // sb + gdt : g0 → sb dans le bloc fdb, GDT à fdb+1 ; backup → sb à start
    let (sb_block, gdt_start) = if g == 0 {
        (Some(geo.fdb as u64), Some(geo.fdb as u64 + 1))
    } else if backup {
        (Some(start), Some(start + 1))
    } else {
        (None, None)
    };
    let after = if g == 0 || backup {
        1 + geo.gdt_blocks
    } else {
        0
    };
    let bbitmap = start + after;
    let ibitmap = bbitmap + 1;
    let itable = ibitmap + 1;
    let data_from = itable + geo.itable_blocks as u64;
    let data_to = start + len;
    GroupLayout {
        start,
        len,
        sb_block,
        gdt_start,
        bbitmap,
        ibitmap,
        itable,
        data_from,
        data_to,
    }
}

/// Dérive déterministe 64 bits (splitmix64).
fn splitmix(x: &mut u64) -> u64 {
    *x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// UUID v4 déterministe à partir du temps et de la taille du volume.
fn derive_uuid(now: u32, total_bytes: u64) -> [u8; 16] {
    let mut st = (now as u64) << 32 ^ total_bytes ^ 0x4578_7434_4653_5F55;
    let mut u = [0u8; 16];
    for i in 0..2 {
        let v = splitmix(&mut st);
        u[i * 8..i * 8 + 8].copy_from_slice(&v.to_le_bytes());
    }
    u[6] = (u[6] & 0x0F) | 0x40; // version 4
    u[8] = (u[8] & 0x3F) | 0x80; // variant RFC 4122
    u
}

/// Hash seed déterministe depuis l'uuid.
fn derive_hash_seed(uuid: &[u8; 16]) -> [u32; 4] {
    let mut seed = [0u32; 4];
    for (i, s) in seed.iter_mut().enumerate() {
        *s = crate::crc::crc32c_update(0xFFFF_FFFF, &uuid[i * 4..(i + 1) * 4]);
    }
    seed
}

/// Recalcule s_checksum (convention e2fsprogs : crc32c(~0, raw[0..0x3FC))).
fn recompute_sb_csum(raw: &mut [u8]) {
    let c = crate::crc::crc32c_update(0xFFFF_FFFF, &raw[..0x3FC]);
    wr32_le(raw, 0x3FC, c);
}

/// Blocs de données de g0 hors journal : (root, lost+found).
fn root_lf_blocks(geo: &Geo, journal: &[(u64, u32)]) -> (u64, u64) {
    let lo0 = layout_group(geo, 0);
    let mut d = lo0.data_from;
    for (p, l) in journal {
        if *p == d {
            d += *l as u64;
        }
    }
    (d, d + 1)
}

/// Formatte un volume ext4 complet sur `dev`. `now` = timestamp unix.
pub fn mkfs(dev: &mut dyn BlockDevice, opts: &MkfsOptions, now: u32) -> Result<()> {
    if opts.block_log > 6 {
        return Err(Error::Corrupt("block_log hors limites"));
    }
    let bs: u32 = 1024u32 << opts.block_log;
    let total_bytes = dev.size()?;
    let total_blocks = total_bytes / bs as u64;
    if total_blocks < 128 {
        return Err(Error::Corrupt("volume trop petit (min ~128 blocs)"));
    }
    let fdb: u32 = if bs == 1024 { 1 } else { 0 };
    let bpg: u32 = 8 * bs; // bitmap d'un bloc
    let groups: u64 = (total_blocks - fdb as u64 + bpg as u64 - 1) / bpg as u64;

    // inodes par groupe : ratio, multiple de bs/256 (et de 8) pour l'itable
    let unit = (bs / INODE_SIZE).max(8);
    let raw_ipg = (bpg as u64 * bs as u64 / opts.inode_ratio.max(1024) as u64) as u32;
    let ipg = raw_ipg
        .min(bpg)
        .max(unit)
        .div_ceil(unit)
        .saturating_mul(unit);
    let itable_blocks = (ipg * INODE_SIZE + bs - 1) / bs;
    let desc_size: u16 = 64; // 64bit
    let gdt_blocks = (groups * desc_size as u64 + bs as u64 - 1) / bs as u64;

    let geo = Geo {
        bs,
        fdb,
        bpg,
        ipg,
        itable_blocks,
        groups,
        gdt_blocks,
        total_blocks,
    };

    // ---- Journal : bandes contiguës (≤ 4 runs) dans les zones de données ---
    let journal_len: u32 = if opts.journal_mib > 0 {
        let want = (opts.journal_mib as u64 * (1 << 20) / bs as u64) as u32;
        want.clamp(64, 0x8000) // min 64 blocs, max 1 extent (32768)
    } else {
        0
    };
    let mut journal_extents: Vec<(u64, u32)> = Vec::new(); // (phys, len)
    if journal_len > 0 {
        let mut need = journal_len;
        for g in 0..groups {
            let lo = layout_group(&geo, g);
            let free = lo.data_to.saturating_sub(lo.data_from);
            if free == 0 {
                continue;
            }
            let take = need.min(free as u32).min(0x8000) as u64;
            journal_extents.push((lo.data_from, take as u32));
            need -= take as u32;
            if need == 0 {
                break;
            }
        }
        if need > 0 || journal_extents.len() > 4 {
            return Err(Error::Corrupt(
                "journal trop gros pour la disposition (réduire --journal)",
            ));
        }
    }
    let (root_block, lf_block) = root_lf_blocks(&geo, &journal_extents);

    // ---- Comptes libres -----------------------------------------------------
    let mut used_total: u64 = 0;
    let mut free_per_group: Vec<u64> = Vec::with_capacity(groups as usize);
    for g in 0..groups {
        let lo = layout_group(&geo, g);
        let meta = lo.data_from - lo.start;
        let mut used = meta;
        for (p, l) in &journal_extents {
            if *p >= lo.data_from && *p + *l as u64 <= lo.data_to {
                used += *l as u64;
            }
        }
        if g == 0 {
            used += 2; // root + lost+found
        }
        let len = geo.group_len(g);
        if used > len {
            return Err(Error::Corrupt("groupe trop petit pour ses métadonnées"));
        }
        free_per_group.push(len - used);
        used_total += used;
    }
    let free_total = total_blocks - fdb as u64 - used_total;
    let free_inodes = ipg as u64 * groups - FIRST_INO as u64;
    if free_inodes > u32::MAX as u64 {
        return Err(Error::Corrupt("trop d'inodes pour u32"));
    }

    // ---- Superblock principal ----------------------------------------------
    let mut uuid = opts.uuid;
    if uuid == [0u8; 16] {
        uuid = derive_uuid(now, total_bytes);
    }
    // NOTE : s_journal_uuid (sb 0xD0) reste À ZÉRO pour un journal interne
    // (e2fsprogs journal.c:1000 : uuid non nul ⇒ recherche d'un journal
    // EXTERNE par blkid ⇒ "Can't find external journal"). Le jsb du journal
    // recopie l'UUID du FS (mkjournal.c:72).
    let mut hash_seed = opts.hash_seed;
    if hash_seed == [0; 4] {
        hash_seed = derive_hash_seed(&uuid);
    }
    let label: Vec<u8> = opts.label.iter().copied().take(16).collect();

    let mut sb_raw = build_sb_raw(
        &geo, now, &uuid, &hash_seed, &label, free_total,
        free_inodes as u32, journal_len, 0, opts.reserved_pct,
    );
    recompute_sb_csum(&mut sb_raw);
    // Superblock parsé : source des seeds/conventions pour tous les checksums
    let mut arr = [0u8; 1024];
    arr.copy_from_slice(&sb_raw);
    let sb = Superblock::parse(arr)?;

    // ---- GDT -----------------------------------------------------------------
    let mut gdt_raw = alloc::vec![0u8; (gdt_blocks * bs as u64) as usize];
    for g in 0..groups {
        let lo = layout_group(&geo, g);
        let mut gd = GroupDesc {
            block_bitmap: lo.bbitmap,
            inode_bitmap: lo.ibitmap,
            inode_table: lo.itable,
            free_blocks_count: free_per_group[g as usize] as u32,
            free_inodes_count: if g == 0 { ipg - FIRST_INO } else { ipg },
            used_dirs_count: if g == 0 { 2 } else { 0 },
            flags: if g == 0 { 0 } else { GD_FLAGS_INODE_ZEROED },
            itable_unused: if g == 0 { 0 } else { ipg },
            block_bitmap_csum: 0,
            inode_bitmap_csum: 0,
            checksum: 0,
        };
        // checksums des bitmaps (e2fsprogs : blocs = (bpg+7)/8 o, inodes = (ipg+7)/8 o)
        let bb = build_block_bitmap(&geo, g, &journal_extents, root_block, lf_block);
        let nbytes = (geo.bpg as usize + 7) / 8;
        set_bitmap_csums(&sb, &mut gd, true, &bb[..nbytes]);
        let ib = build_inode_bitmap(&geo, g);
        let inbytes = (ipg as usize + 7) / 8;
        set_bitmap_csums(&sb, &mut gd, false, &ib[..inbytes]);
        let off = g as usize * desc_size as usize;
        gd.encode(&mut gdt_raw, off, &sb);
        gdt_raw[off + 0x1E..off + 0x20].fill(0);
        let csum = group_desc_csum(&sb, g, &gdt_raw[off..off + desc_size as usize]);
        wr16_le(&mut gdt_raw, off + 0x1E, csum);
    }

    // ---- Écritures -----------------------------------------------------------
    let bs_u = bs as u64;
    let zero = alloc::vec![0u8; bs as usize];

    // bloc(s) superblock principal (boot à zéro + sb à 1024 pour bs>1K)
    if bs == 1024 {
        dev.write_at(0, &zero)?; // boot block à zéro
        dev.write_at(1024, &sb_raw)?; // sb = bloc 1 entier
    } else {
        let mut b0 = zero.clone();
        b0[SB_OFFSET as usize..SB_OFFSET as usize + 1024].copy_from_slice(&sb_raw);
        dev.write_at(0, &b0)?;
    }
    // par groupe : backup sb+GDT, bitmaps, table d'inodes
    for g in 0..groups {
        let lo = layout_group(&geo, g);
        if let Some(gb) = lo.gdt_start {
            dev.write_at(gb * bs_u, &gdt_raw)?;
        }
        if let Some(sbb) = lo.sb_block {
            if g > 0 {
                let mut raw = build_sb_raw(
                    &geo, now, &uuid, &hash_seed, &label,
                    free_total, free_inodes as u32, journal_len, g as u32,
                    opts.reserved_pct,
                );
                recompute_sb_csum(&mut raw);
                if bs == 1024 {
                    dev.write_at(sbb * bs_u, &raw)?;
                } else {
                    let mut b = zero.clone();
                    b[1024..2048].copy_from_slice(&raw);
                    dev.write_at(sbb * bs_u, &b)?;
                }
            }
        }
        let bb = build_block_bitmap(&geo, g, &journal_extents, root_block, lf_block);
        dev.write_at(lo.bbitmap * bs_u, &bb)?;
        let ib = build_inode_bitmap(&geo, g);
        dev.write_at(lo.ibitmap * bs_u, &ib)?;
        if g == 0 {
            write_group0_itable(dev, &geo, &sb, &lo, now, &journal_extents, root_block, lf_block)?;
        } else {
            for i in 0..geo.itable_blocks as u64 {
                dev.write_at((lo.itable + i) * bs_u, &zero)?;
            }
        }
    }
    // journal : jsb + blocs à zéro
    if journal_len > 0 {
        let jphys = journal_extents[0].0;
        let jsb = build_jsb(bs, journal_len, &uuid);
        let mut jb = zero.clone();
        jb[..1024].copy_from_slice(&jsb);
        dev.write_at(jphys * bs_u, &jb)?;
        let mut off = 1u64;
        let zbuf = alloc::vec![0u8; 256 * bs as usize];
        while off < journal_len as u64 {
            let n = (journal_len as u64 - off).min(256) as usize;
            dev.write_at((jphys + off) * bs_u, &zbuf[..n * bs as usize])?;
            off += n as u64;
        }
    }
    // root + lost+found (premiers blocs data de g0 hors journal)
    let root_blk = build_root_dir_block(&geo, &sb, root_block, lf_block);
    dev.write_at(root_block * bs_u, &root_blk)?;
    let lf_blk = build_lf_dir_block(&geo, &sb);
    dev.write_at(lf_block * bs_u, &lf_blk)?;

    dev.flush()?;
    Ok(())
}

/// Construit le superblock 1024 octets (principal ou sauvegarde).
#[allow(clippy::too_many_arguments)]
fn build_sb_raw(
    geo: &Geo, now: u32, uuid: &[u8; 16], hash_seed: &[u32; 4],
    label: &[u8], free_blocks: u64, free_inodes: u32, journal_len: u32, group_nr: u32,
    reserved_pct: u32,
) -> Vec<u8> {
    let mut raw = alloc::vec![0u8; 1024];
    let inodes_count = geo.ipg as u64 * geo.groups;
    let reserved = reserved_pct_blocks(geo.total_blocks, reserved_pct);
    wr32_le(&mut raw, 0x00, inodes_count as u32);
    wr32_le(&mut raw, 0x04, geo.total_blocks as u32);
    wr32_le(&mut raw, 0x08, reserved as u32);
    wr32_le(&mut raw, 0x0C, free_blocks as u32);
    wr32_le(&mut raw, 0x10, free_inodes);
    wr32_le(&mut raw, 0x14, geo.fdb);
    wr32_le(&mut raw, 0x18, geo_log_bs(geo.bs));
    wr32_le(&mut raw, 0x1C, geo_log_bs(geo.bs));
    wr32_le(&mut raw, 0x20, geo.bpg);
    wr32_le(&mut raw, 0x24, geo.bpg);
    wr32_le(&mut raw, 0x28, geo.ipg);
    wr32_le(&mut raw, 0x2C, 0); // s_mtime
    wr32_le(&mut raw, 0x30, now); // s_wtime
    wr16_le(&mut raw, 0x34, 0); // s_mnt_count
    wr16_le(&mut raw, 0x36, 0xFFFF); // s_max_mnt_count = -1
    wr16_le(&mut raw, 0x38, crate::superblock::EXT4_MAGIC);
    wr16_le(&mut raw, 0x3A, STATE_VALID);
    wr16_le(&mut raw, 0x3C, 1); // s_errors = continue
    wr16_le(&mut raw, 0x3E, 0); // s_minor_rev_level
    wr32_le(&mut raw, 0x40, now); // s_lastcheck
    wr32_le(&mut raw, 0x44, 0); // s_checkinterval
    wr32_le(&mut raw, 0x48, 0); // s_creator_os = Linux
    wr32_le(&mut raw, 0x4C, 1); // s_rev_level
    wr16_le(&mut raw, 0x50, 0); // s_def_resuid
    wr16_le(&mut raw, 0x52, 0); // s_def_resgid
    wr32_le(&mut raw, 0x54, FIRST_INO);
    wr16_le(&mut raw, 0x58, INODE_SIZE as u16);
    wr16_le(&mut raw, 0x5A, group_nr as u16); // s_block_group_nr
    let compat = feat_compat::HAS_JOURNAL * (journal_len > 0) as u32
        | feat_compat::EXT_ATTR
        | feat_compat::DIR_INDEX;
    let incompat = feat_incompat::FILETYPE | feat_incompat::EXTENTS | feat_incompat::B64BIT;
    let ro = feat_ro::SPARSE_SUPER
        | feat_ro::LARGE_FILE
        | feat_ro::HUGE_FILE
        | feat_ro::DIR_NLINK
        | feat_ro::EXTRA_ISIZE
        | feat_ro::METADATA_CSUM;
    wr32_le(&mut raw, 0x5C, compat);
    wr32_le(&mut raw, 0x60, incompat);
    wr32_le(&mut raw, 0x64, ro);
    raw[0x68..0x78].copy_from_slice(uuid);
    raw[0x78..0x78 + label.len().min(16)].copy_from_slice(&label[..label.len().min(16)]);
    raw[0x88] = b'/'; // s_last_mounted = "/"
    wr16_le(&mut raw, 0xCE, 0); // s_reserved_gdt_blocks
    if journal_len > 0 {
        // s_journal_uuid (0xD0) LAISSÉ À ZÉRO : journal interne uniquement.
        // (e2fsprogs journal.c:1000 — UUID non nul ⇒ journal externe.)
        wr32_le(&mut raw, 0xE0, JOURNAL_INO);
    }
    for (i, h) in hash_seed.iter().enumerate() {
        wr32_le(&mut raw, 0xEC + i * 4, *h);
    }
    raw[0xFC] = 1; // s_def_hash_version (half_md4)
    wr16_le(&mut raw, 0xFE, 64); // s_desc_size
    wr32_le(&mut raw, 0x108, now); // s_mkfs_time
    wr32_le(&mut raw, 0x150, (geo.total_blocks >> 32) as u32);
    wr32_le(&mut raw, 0x154, (reserved >> 32) as u32);
    wr32_le(&mut raw, 0x158, (free_blocks >> 32) as u32);
    wr16_le(&mut raw, 0x15C, 0x20); // s_min_extra_isize
    wr16_le(&mut raw, 0x15E, 0x20); // s_want_extra_isize
    raw[0x174] = 0; // s_log_groups_per_flex (pas de flex_bg)
    raw[0x175] = 1; // s_checksum_type = crc32c
    raw
}

/// log2(bs/1024).
fn geo_log_bs(bs: u32) -> u32 {
    match bs {
        1024 => 0,
        2048 => 1,
        4096 => 2,
        8192 => 3,
        16384 => 4,
        32768 => 5,
        _ => 6,
    }
}

/// Pourcentage de blocs réservés (borné à 50 % comme mke2fs).
fn reserved_pct_blocks(total: u64, pct: u32) -> u64 {
    total * pct.min(50) as u64 / 100
}

/// Bitmap de blocs du groupe g (bloc complet, padding à 1).
fn build_block_bitmap(
    geo: &Geo, g: u64, journal: &[(u64, u32)], root_blk: u64, lf_blk: u64,
) -> Vec<u8> {
    let lo = layout_group(geo, g);
    let mut b = alloc::vec![0u8; geo.bs as usize];
    let len = lo.len as usize;
    let used_before = (lo.data_from - lo.start) as usize;
    for i in 0..used_before.min(len) {
        b[i / 8] |= 1 << (i % 8);
    }
    let mark = |b: &mut [u8], blk: u64| {
        if blk >= lo.start && blk < lo.data_to {
            let i = (blk - lo.start) as usize;
            b[i / 8] |= 1 << (i % 8);
        }
    };
    for (p, l) in journal {
        for k in 0..*l as u64 {
            mark(&mut b, p + k);
        }
    }
    if g == 0 {
        mark(&mut b, root_blk);
        mark(&mut b, lf_blk);
    }
    // padding : bits au-delà de la fin du groupe
    if len < geo.bs as usize * 8 {
        for i in len..geo.bs as usize * 8 {
            b[i / 8] |= 1 << (i % 8);
        }
    }
    b
}

/// Bitmap d'inodes du groupe g (g0 : inodes 1..11 pris ; padding à 1).
fn build_inode_bitmap(geo: &Geo, g: u64) -> Vec<u8> {
    let mut b = alloc::vec![0u8; geo.bs as usize];
    if g == 0 {
        for i in 0..FIRST_INO as usize {
            b[i / 8] |= 1 << (i % 8);
        }
    }
    let ipg = geo.ipg as usize;
    if ipg < geo.bs as usize * 8 {
        for i in ipg..geo.bs as usize * 8 {
            b[i / 8] |= 1 << (i % 8);
        }
    }
    b
}

/// Écrit la table d'inodes du groupe 0 : 1..11 (réservés checksumés,
/// root, journal, lost+found).
fn write_group0_itable(
    dev: &mut dyn BlockDevice,
    geo: &Geo,
    sb: &Superblock,
    lo: &GroupLayout,
    now: u32,
    journal: &[(u64, u32)],
    root_block: u64,
    lf_block: u64,
) -> Result<()> {
    let bs = geo.bs as usize;
    let nb = geo.itable_blocks as usize;
    let mut blocks = alloc::vec![alloc::vec![0u8; bs]; nb];
    let put = |blocks: &mut Vec<Vec<u8>>, ino: u32, raw: &[u8]| {
        let i = (ino - 1) as usize;
        let blk = i * INODE_SIZE as usize / bs;
        let off = i * INODE_SIZE as usize % bs;
        let n = INODE_SIZE as usize;
        blocks[blk][off..off + n].copy_from_slice(&raw[..n]);
    };
    // réservés : corps nul + checksum valide
    for ino in [1u32, 3, 4, 5, 6, 9, 10] {
        let mut ino_st = Inode::new(ino, INODE_SIZE as usize);
        ino_st.store_checksum(sb);
        put(&mut blocks, ino, &ino_st.raw);
    }
    // racine
    {
        let mut ino = Inode::new(ROOT_INO, INODE_SIZE as usize);
        ino.set_mode(S_IFDIR | 0o755);
        ino.set_size(bs as u64);
        ino.set_links_count(3); // . .. lost+found
        ino.set_blocks((bs / 512) as u64);
        ino.set_flags(FI_EXTENTS);
        ino.set_atime(now);
        ino.set_ctime(now);
        ino.set_mtime(now);
        wr16_le(&mut ino.raw, 0x80, 0x20); // i_extra_isize
        write_extent_leaf(&mut ino.raw, 0, &[(0, root_block, 1)]);
        ino.store_checksum(sb);
        put(&mut blocks, ROOT_INO, &ino.raw);
    }
    // journal (inode 8) — extents multi-runs si besoin
    if !journal.is_empty() {
        let jlen: u64 = journal.iter().map(|(_, l)| *l as u64).sum();
        let mut ino = Inode::new(JOURNAL_INO, INODE_SIZE as usize);
        ino.set_mode(S_IFREG | 0o600);
        ino.set_size(jlen * bs as u64);
        ino.set_links_count(1);
        ino.set_blocks(jlen * bs as u64 / 512);
        ino.set_flags(FI_EXTENTS);
        ino.set_atime(now);
        ino.set_ctime(now);
        ino.set_mtime(now);
        wr16_le(&mut ino.raw, 0x80, 0x20);
        let mut acc = 0u32;
        let logical: Vec<(u32, u64, u32)> = journal
            .iter()
            .map(|(p, l)| {
                let v = (acc, *p, *l);
                acc += *l;
                v
            })
            .collect();
        write_extent_leaf(&mut ino.raw, 0, &logical);
        ino.store_checksum(sb);
        put(&mut blocks, JOURNAL_INO, &ino.raw);
    }
    // lost+found
    {
        let mut ino = Inode::new(FIRST_INO, INODE_SIZE as usize);
        ino.set_mode(S_IFDIR | 0o700);
        ino.set_size(bs as u64);
        ino.set_links_count(2);
        ino.set_blocks((bs / 512) as u64);
        ino.set_flags(FI_EXTENTS);
        ino.set_atime(now);
        ino.set_ctime(now);
        ino.set_mtime(now);
        wr16_le(&mut ino.raw, 0x80, 0x20);
        write_extent_leaf(&mut ino.raw, 0, &[(0, lf_block, 1)]);
        ino.store_checksum(sb);
        put(&mut blocks, FIRST_INO, &ino.raw);
    }
    for (i, blk) in blocks.iter().enumerate() {
        dev.write_at((lo.itable + i as u64) * bs as u64, blk)?;
    }
    Ok(())
}

/// Encadre un arbre d'extents depth 0 (≤ 4 entrées) dans i_block (0x28).
fn write_extent_leaf(raw: &mut [u8], generation: u32, exts: &[(u32, u64, u32)]) {
    wr16_le(raw, 0x28, 0xF30A); // eh_magic
    wr16_le(raw, 0x2A, exts.len() as u16); // eh_entries
    wr16_le(raw, 0x2C, 4); // eh_max (60-12)/12
    wr16_le(raw, 0x2E, 0); // eh_depth
    wr32_le(raw, 0x30, generation);
    for (i, (block, phys, len)) in exts.iter().enumerate() {
        let o = 0x34 + i * 12;
        wr32_le(raw, o, *block); // ee_block
        wr16_le(raw, o + 4, *len as u16); // ee_len
        wr16_le(raw, o + 6, (phys >> 32) as u16); // ee_start_hi
        wr32_le(raw, o + 8, *phys as u32); // ee_start_lo
    }
}

fn put_dirent(b: &mut [u8], off: usize, ino: u32, name: &[u8], rec: u16) {
    wr32_le(b, off, ino);
    wr16_le(b, off + 4, rec);
    b[off + 6] = name.len() as u8; // name_len
    b[off + 7] = 2; // FT_DIR (FILETYPE)
    b[off + 8..off + 8 + name.len()].copy_from_slice(name);
}

/// Bloc de répertoire racine : ".", "..", "lost+found", tail.
fn build_root_dir_block(geo: &Geo, sb: &Superblock, root_block: u64, _lf_block: u64) -> Vec<u8> {
    let bs = geo.bs as usize;
    let mut b = alloc::vec![0u8; bs];
    put_dirent(&mut b, 0, ROOT_INO, b".", 12);
    put_dirent(&mut b, 12, ROOT_INO, b"..", 12);
    let rec = (bs - 24 - 12) as u16;
    put_dirent(&mut b, 24, FIRST_INO, b"lost+found", rec);
    let _ = root_block;
    let seed = inode_csum_seed(sb, ROOT_INO, 0);
    write_dirent_tail(&mut b, seed, true);
    b
}

/// Bloc lost+found : ".", "..", entrée nulle comblant l'espace, tail.
fn build_lf_dir_block(geo: &Geo, sb: &Superblock) -> Vec<u8> {
    let bs = geo.bs as usize;
    let mut b = alloc::vec![0u8; bs];
    put_dirent(&mut b, 0, FIRST_INO, b".", 12);
    let rec = (bs - 12 - 12) as u16;
    put_dirent(&mut b, 12, ROOT_INO, b"..", rec);
    let seed = inode_csum_seed(sb, FIRST_INO, 0);
    write_dirent_tail(&mut b, seed, true);
    b
}

/// jsb jbd2 (1024 octets) : recette EXACTE de mkjournal.c
/// (ext2fs_create_journal_superblock2) — memset 0, puis :
/// magic, blocktype V2, blocksize, maxlen, first=1, sequence=1,
/// nr_users=1 (0x40), s_uuid = UUID DU FS (0x30). TOUT le reste à zéro :
/// pas de feature flags (e2fsck les posera au premier montage, comme le
/// noyau), pas de checksum (jsb V1 nu sur disque neuf).
fn build_jsb(bs: u32, journal_len: u32, fs_uuid: &[u8; 16]) -> Vec<u8> {
    let mut j = alloc::vec![0u8; 1024];
    wr32_be(&mut j, 0x00, JBD2_MAGIC);
    wr32_be(&mut j, 0x04, BT_SUPERBLOCK_V2);
    wr32_be(&mut j, 0x0C, bs);
    wr32_be(&mut j, 0x10, journal_len); // s_maxlen
    wr32_be(&mut j, 0x14, 1); // s_first
    wr32_be(&mut j, 0x18, 1); // s_sequence
    wr32_be(&mut j, 0x1C, 0); // s_start = 0 (log vide)
    j[0x30..0x40].copy_from_slice(fs_uuid);
    wr32_be(&mut j, 0x40, 1); // s_nr_users = 1
    j
}
