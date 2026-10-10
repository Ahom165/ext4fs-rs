//! Filesystem : montage, cache de blocs, allocateurs (blocs/inodes),
//! intégration journal (transactions checkpointées immédiatement), inodes.

use crate::bitmap::Bitmap;
use crate::blockdev::BlockDevice;
use crate::error::{Error, Result};
use crate::extent::ExtentCtx;
use crate::gdt::{set_bitmap_csums, Gdt};
use crate::inode::{Inode, S_IFDIR, S_IFMT, S_IFREG};
use crate::journal::Journal;
use crate::superblock::{
    Superblock, STATE_VALID, SB_OFFSET, feat_incompat,
};
use crate::util::TimeSource;
use alloc::boxed::Box;
use alloc::vec::Vec;
use alloc::collections::BTreeMap;

/// Volume ext4 monté.
pub struct Filesystem {
    pub dev: Box<dyn BlockDevice>,
    pub sb: Superblock,
    pub gdt: Gdt,
    pub cache: BTreeMap<u64, Vec<u8>>,
    pub time: Box<dyn TimeSource>,
    pub read_only: bool,
    pub journal: Option<Journal>,
    pub alloc_hint: u64,
    pub dirty: bool,
}

impl Filesystem {
    /// Monte un volume ext4. Rejoue le journal si nécessaire.
    pub fn mount(
        mut dev: Box<dyn BlockDevice>,
        time: Box<dyn TimeSource>,
        read_only: bool,
    ) -> Result<Filesystem> {
        let mut raw = [0u8; 1024];
        dev.read_at(SB_OFFSET, &mut raw)?;
        let sb = Superblock::parse(raw)?;
        sb.check_supported()?;
        if !sb.verify_checksum() {
            return Err(Error::ChecksumMismatch("superblock"));
        }
        let effective_ro = read_only || dev.is_read_only();
        let mut gdt = Gdt::load(&mut *dev, &sb)?;
        gdt.verify_checksums(&sb)?;
        let mut fs = Filesystem {
            dev,
            sb,
            gdt,
            cache: BTreeMap::new(),
            time,
            read_only: effective_ro,
            journal: None,
            alloc_hint: 0,
            dirty: false,
        };
        // Replay du journal (écritures directes, cache encore vide)
        if fs.sb.has_journal() && fs.sb.journal_inum != 0 {
            let mut j = Journal::load(&mut *fs.dev, &fs.sb)?;
            j.replay(&mut *fs.dev)?;
            fs.sb.set_incompat(feat_incompat::RECOVER, false);
            fs.journal = Some(j);
        }
        if !effective_ro {
            fs.sb.set_state(fs.sb.state() & !STATE_VALID);
            fs.sb.set_mnt_count(fs.sb.mnt_count().wrapping_add(1));
            if fs.journal.is_some() {
                fs.sb.set_incompat(feat_incompat::RECOVER, true);
            }
            fs.dirty = true;
            fs.commit_transaction()?;
        }
        Ok(fs)
    }

    /// Bloc contenant le superblock (1024..2048) : bloc 1 en 1K, bloc 0 sinon.
    pub fn sb_block(&self) -> u64 {
        if self.sb.block_size == 1024 {
            1
        } else {
            0
        }
    }

    pub fn block_size(&self) -> u64 {
        self.sb.block_size as u64
    }

    /// Récupère (en le chargeant si besoin) le bloc du cache.
    pub fn get_block(&mut self, bno: u64) -> Result<&mut Vec<u8>> {
        let bs = self.block_size();
        if !self.cache.contains_key(&bno) {
            let mut buf = alloc::vec![0u8; bs as usize];
            self.dev.read_at(bno * bs, &mut buf)?;
            self.cache.insert(bno, buf);
        }
        Ok(self.cache.get_mut(&bno).expect("bloc vient d'être inséré"))
    }

    /// Pose un bloc complet dans le cache (contenu final).
    pub fn put_block(&mut self, bno: u64, buf: &[u8]) -> Result<()> {
        debug_assert_eq!(buf.len(), self.block_size() as usize);
        self.cache.insert(bno, buf.to_vec());
        Ok(())
    }

    /// Groupe contenant un bloc.
    pub fn group_of_block(&self, bno: u64) -> u64 {
        (bno - self.sb.first_data_block as u64) / self.sb.blocks_per_group as u64
    }

    // ------------------------------------------------------------------
    // ExtentCtx
    // ------------------------------------------------------------------
    fn ctx_read_block(&mut self, bno: u64, buf: &mut [u8]) -> Result<()> {
        let blk = self.get_block(bno)?;
        buf.copy_from_slice(blk);
        Ok(())
    }

    fn ctx_write_block(&mut self, bno: u64, buf: &[u8]) -> Result<()> {
        self.put_block(bno, buf)
    }

    // ------------------------------------------------------------------
    // Inodes
    // ------------------------------------------------------------------
    /// Localise un inode : (bloc table, offset dans le bloc).
    fn inode_location(&self, ino: u32) -> Result<(u64, usize)> {
        if ino < 2 || ino > self.sb.inodes_count {
            return Err(Error::InodeNotFound(ino));
        }
        let group = ((ino - 1) / self.sb.inodes_per_group) as u64;
        let index = ((ino - 1) % self.sb.inodes_per_group) as u64;
        let gd = self.gdt.get(&self.sb, group)?;
        let bs = self.block_size();
        let isize_ = self.sb.inode_size as u64;
        let blk = gd.inode_table + (index * isize_) / bs;
        let off = (index * isize_) % bs;
        Ok((blk, off as usize))
    }

    pub fn read_inode(&mut self, ino: u32) -> Result<Inode> {
        let (blk, off) = self.inode_location(ino)?;
        let isz = self.sb.inode_size as usize;
        let buf = self.get_block(blk)?;
        let raw = buf[off..off + isz].to_vec();
        let inode = Inode { no: ino, raw };
        if !inode.verify_checksum(&self.sb) {
            return Err(Error::ChecksumMismatch("inode"));
        }
        Ok(inode)
    }

    pub fn write_inode(&mut self, ino: &mut Inode) -> Result<()> {
        let (blk, off) = self.inode_location(ino.no)?;
        ino.store_checksum(&self.sb);
        let isz = self.sb.inode_size as usize;
        let buf = self.get_block(blk)?;
        buf[off..off + isz].copy_from_slice(&ino.raw);
        Ok(())
    }

    // ------------------------------------------------------------------
    // Allocation de blocs
    // ------------------------------------------------------------------
    fn load_bitmap(&mut self, bno: u64) -> Result<(alloc::vec::Vec<u8>,)> {
        let blk = self.get_block(bno)?;
        Ok((blk.clone(),))
    }

    pub fn alloc_block(&mut self) -> Result<u64> {
        let groups = self.sb.groups_count;
        let start = if groups == 0 { 0 } else { self.alloc_hint % groups };
        for gi in 0..groups {
            let g = (start + gi) % groups;
            let gd = self.gdt.get(&self.sb, g)?;
            if gd.free_blocks_count == 0 {
                continue;
            }
            let (bdata,) = self.load_bitmap(gd.block_bitmap)?;
            let max = self.sb.blocks_in_group(g) as usize;
            let mut bmp = Bitmap::load(gd.block_bitmap, bdata);
            bmp.check(max)?;
            if let Some(idx) = bmp.find_clear(0, max) {
                bmp.set(idx);
                let bno = self.sb.first_data_block as u64 + g * self.sb.blocks_per_group as u64
                    + idx as u64;
                let mut gd2 = gd.clone();
                gd2.free_blocks_count -= 1;
                let nbytes = (max + 7) / 8;
                set_bitmap_csums(&self.sb, &mut gd2, true, &bmp.data[..nbytes]);
                self.gdt.put(&self.sb, g, &gd2)?;
                self.cache.insert(gd.block_bitmap, bmp.data);
                self.sb
                    .set_free_blocks(self.sb.free_blocks_count - 1);
                self.alloc_hint = g;
                self.dirty = true;
                return Ok(bno);
            }
        }
        Err(Error::NoSpace)
    }

    pub fn free_block(&mut self, bno: u64) -> Result<()> {
        let g = self.group_of_block(bno);
        let idx = (bno - self.sb.first_data_block as u64 - g * self.sb.blocks_per_group as u64)
            as usize;
        let gd = self.gdt.get(&self.sb, g)?;
        let (bdata,) = self.load_bitmap(gd.block_bitmap)?;
        let max = self.sb.blocks_in_group(g) as usize;
        let mut bmp = Bitmap::load(gd.block_bitmap, bdata);
        if idx >= max {
            return Err(Error::Corrupt("bloc hors groupe (free)"));
        }
        if !bmp.test(idx) {
            return Err(Error::Corrupt("double libération de bloc"));
        }
        bmp.clear(idx);
        let mut gd2 = gd.clone();
        gd2.free_blocks_count += 1;
        let nbytes = (max + 7) / 8;
        set_bitmap_csums(&self.sb, &mut gd2, true, &bmp.data[..nbytes]);
        self.gdt.put(&self.sb, g, &gd2)?;
        self.cache.insert(gd.block_bitmap, bmp.data);
        self.sb.set_free_blocks(self.sb.free_blocks_count + 1);
        self.dirty = true;
        Ok(())
    }

    // ------------------------------------------------------------------
    // Allocation d'inodes
    // ------------------------------------------------------------------
    pub fn alloc_inode(&mut self, prefer_group: u64, mode: u16) -> Result<Inode> {
        let groups = self.sb.groups_count;
        let start = if groups == 0 {
            0
        } else {
            prefer_group.min(groups - 1)
        };
        for gi in 0..groups {
            let g = (start + gi) % groups;
            let gd = self.gdt.get(&self.sb, g)?;
            if gd.free_inodes_count == 0 {
                continue;
            }
            let (bdata,) = self.load_bitmap(gd.inode_bitmap)?;
            let max = self.sb.inodes_per_group as usize;
            let mut bmp = Bitmap::load(gd.inode_bitmap, bdata);
            bmp.check(max)?;
            if let Some(idx) = bmp.find_clear(0, max) {
                bmp.set(idx);
                let ino = g * self.sb.inodes_per_group as u64 + idx as u64 + 1;
                let mut gd2 = gd.clone();
                gd2.free_inodes_count -= 1;
                if mode & S_IFMT == S_IFDIR {
                    gd2.used_dirs_count += 1;
                }
                // Règle kernel ext4_claim_inode : inode pris dans la zone
                // "non initialisée" => réduire bg_itable_unused
                {
                    let ipg = self.sb.inodes_per_group as u64;
                    let unused = gd2.itable_unused as u64;
                    if gd2.flags & crate::gdt::GD_FLAGS_INODE_UNINIT != 0 {
                        gd2.flags &= !crate::gdt::GD_FLAGS_INODE_UNINIT;
                        gd2.itable_unused = ipg as u32;
                        // ext4_mark_bitmap_end : le padding de la bitmap
                        // (bits >= inodes_per_group) doit être à 1 dès que le
                        // groupe devient initialisé (exigé par e2fsck)
                        let bits = self.sb.inodes_per_group as usize;
                        let total = self.block_size() as usize * 8;
                        if bits < total {
                            for b in bmp.data[bits / 8..].iter_mut() {
                                *b = 0xFF;
                            }
                            let rem = bits % 8;
                            if rem != 0 {
                                bmp.data[bits / 8] |= 0xFFu8 << rem;
                            }
                        }
                    }
                    let free = ipg - gd2.itable_unused as u64;
                    if idx as u64 >= free {
                        gd2.itable_unused = (ipg - idx as u64 - 1) as u32;
                    }
                    let _ = unused;
                }
                let nbytes = (max + 7) / 8;
                set_bitmap_csums(&self.sb, &mut gd2, false, &bmp.data[..nbytes]);
                self.gdt.put(&self.sb, g, &gd2)?;
                self.cache.insert(gd.inode_bitmap, bmp.data);
                self.sb.set_free_inodes(self.sb.free_inodes_count - 1);
                let mut ino_st = Inode::new(ino as u32, self.sb.inode_size as usize);
                ino_st.set_mode(mode);
                let (now, _) = self.time.now();
                ino_st.set_atime(now);
                ino_st.set_ctime(now);
                ino_st.set_mtime(now);
                // extra_isize minimal pour checksum hi + timestamps étendus
                if self.sb.has_ro(crate::superblock::feat_ro::EXTRA_ISIZE)
                    && ino_st.raw.len() >= 0x84
                {
                    let want = crate::util::rd16_le(&self.sb.raw, 0x15E);
                    crate::util::wr16_le(&mut ino_st.raw, 0x80, want.max(0x1C));
                }
                self.dirty = true;
                return Ok(ino_st);
            }
        }
        Err(Error::NoInodes)
    }

    pub fn free_inode(&mut self, ino: u32) -> Result<()> {
        let group = ((ino - 1) / self.sb.inodes_per_group) as u64;
        let idx = ((ino - 1) % self.sb.inodes_per_group) as usize;
        let gd = self.gdt.get(&self.sb, group)?;
        let (bdata,) = self.load_bitmap(gd.inode_bitmap)?;
        let mut bmp = Bitmap::load(gd.inode_bitmap, bdata);
        if !bmp.test(idx) {
            return Err(Error::Corrupt("double libération d'inode"));
        }
        bmp.clear(idx);
        // dtime + inode à zéro dans la table
        let (tbl_blk, off) = {
            let index = ((ino - 1) % self.sb.inodes_per_group) as u64;
            let bs = self.block_size();
            let isize_ = self.sb.inode_size as u64;
            (
                gd.inode_table + (index * isize_) / bs,
                ((index * isize_) % bs) as usize,
            )
        };
        let (now, _) = self.time.now();
        let mut zeroed = alloc::vec![0u8; self.sb.inode_size as usize];
        crate::util::wr32_le(&mut zeroed, 0x14, now); // dtime
        // checksum valide même pour un inode libéré (exigence e2fsck/kernel)
        let mut dead = Inode { no: ino, raw: zeroed };
        dead.store_checksum(&self.sb);
        {
            let buf = self.get_block(tbl_blk)?;
            buf[off..off + dead.raw.len()].copy_from_slice(&dead.raw);
        }
        let mut gd2 = gd.clone();
        gd2.free_inodes_count += 1;
        // Règle kernel ext4_free_inode : élargir la zone initialisée si besoin
        {
            let ipg = self.sb.inodes_per_group as u64;
            let free = ipg - gd2.itable_unused as u64;
            let rel = idx as u64 + 1;
            if rel > free {
                gd2.itable_unused = (ipg - rel) as u32;
            }
        }
        let nbytes = (self.sb.inodes_per_group as usize + 7) / 8;
        set_bitmap_csums(&self.sb, &mut gd2, false, &bmp.data[..nbytes]);
        self.gdt.put(&self.sb, group, &gd2)?;
        self.cache.insert(gd.inode_bitmap, bmp.data);
        self.sb.set_free_inodes(self.sb.free_inodes_count + 1);
        self.dirty = true;
        Ok(())
    }

    // ------------------------------------------------------------------
    // Transactions (journal + checkpoint immédiat)
    // ------------------------------------------------------------------
    /// Synchronise superblock et GDT dans le cache, puis commit : journal
    /// (desc+données+commit), flush du cache, journal remis à vide.
    pub fn commit_transaction(&mut self) -> Result<()> {
        if self.read_only {
            return Err(Error::ReadOnly);
        }
        let bs = self.block_size() as usize;
        // recalcul du checksum sb (après toutes les modifications)
        self.sb.recompute_checksum();
        // sb dans le cache
        {
            let sb_off = 1024usize;
            let sb_raw = self.sb.raw;
            let sb_blk = self.sb_block();
            let blk = self.get_block(sb_blk)?;
            if sb_off + 1024 <= bs {
                blk[sb_off..sb_off + 1024].copy_from_slice(&sb_raw);
            } else {
                // bloc de 1024 : sb couvre tout le bloc
                blk[..1024].copy_from_slice(&sb_raw);
            }
        }
        // GDT dans le cache
        {
            let gdt_raw = self.gdt.raw.clone();
            let primary = self.gdt.primary_block;
            for (i, chunk) in gdt_raw.chunks(bs).enumerate() {
                let bno = primary + i as u64;
                let blk = self.get_block(bno)?;
                blk[..chunk.len()].copy_from_slice(chunk);
            }
        }
        if let Some(j) = self.journal.as_mut() {
            j.begin(&mut *self.dev)?;
            let dirty: alloc::vec::Vec<(u64, Vec<u8>)> = self
                .cache
                .iter()
                .map(|(k, v)| (*k, v.clone()))
                .collect();
            j.write_txn(&mut *self.dev, &dirty)?;
        }
        // flush du cache
        let bs_u = self.block_size();
        for (bno, buf) in self.cache.iter() {
            self.dev.write_at(bno * bs_u, buf)?;
        }
        self.cache.clear();
        if let Some(j) = self.journal.as_mut() {
            j.finalize(&mut *self.dev)?;
        }
        // flush de l'appareil (volume Windows : IRP_MJ_FLUSH_BUFFERS)
        self.dev.flush()?;
        self.dirty = false;
        Ok(())
    }

    /// Démontage propre : état VALID, RECOVER effacé, mnt_count remis à zéro.
    pub fn unmount(&mut self) -> Result<()> {
        if !self.read_only {
            self.sb.set_state(STATE_VALID);
            self.sb.set_mnt_count(0);
            self.sb.set_incompat(feat_incompat::RECOVER, false);
            let (now, _) = self.time.now();
            crate::util::wr32_le(&mut self.sb.raw, 0x30, now); // s_wtime
            crate::util::wr32_le(&mut self.sb.raw, 0x40, now); // s_lastcheck
            self.commit_transaction()?;
        }
        Ok(())
    }

    /// Simulation de crash : commit SANS remise à vide du journal
    /// (s_start armé, transactions en attente de replay). Réservé aux tests
    /// et à l'outil de validation.
    #[doc(hidden)]
    pub fn commit_crash_simulate(&mut self) -> Result<()> {
        if self.read_only {
            return Err(Error::ReadOnly);
        }
        let bs = self.block_size() as usize;
        self.sb.recompute_checksum();
        {
            let sb_off = 1024usize;
            let sb_raw = self.sb.raw;
            let sb_blk = self.sb_block();
            let blk = self.get_block(sb_blk)?;
            if sb_off + 1024 <= bs {
                blk[sb_off..sb_off + 1024].copy_from_slice(&sb_raw);
            } else {
                blk[..1024].copy_from_slice(&sb_raw);
            }
        }
        {
            let gdt_raw = self.gdt.raw.clone();
            let primary = self.gdt.primary_block;
            for (i, chunk) in gdt_raw.chunks(bs).enumerate() {
                let bno = primary + i as u64;
                let blk = self.get_block(bno)?;
                blk[..chunk.len()].copy_from_slice(chunk);
            }
        }
        if let Some(j) = self.journal.as_mut() {
            j.begin(&mut *self.dev)?;
            let dirty: alloc::vec::Vec<(u64, Vec<u8>)> =
                self.cache.iter().map(|(k, v)| (*k, v.clone())).collect();
            j.write_txn(&mut *self.dev, &dirty)?;
        }
        let bs_u = self.block_size();
        for (bno, buf) in self.cache.iter() {
            self.dev.write_at(bno * bs_u, buf)?;
        }
        self.cache.clear();
        self.dirty = false;
        Ok(())
    }

    /// Flush forcé (sans changer l'état de montage).
    pub fn flush(&mut self) -> Result<()> {
        if !self.read_only && self.dirty {
            self.commit_transaction()?;
        } else {
            self.cache.clear();
        }
        Ok(())
    }
}

impl ExtentCtx for Filesystem {
    fn sb(&self) -> &Superblock {
        &self.sb
    }
    fn read_block(&mut self, bno: u64, buf: &mut [u8]) -> Result<()> {
        self.ctx_read_block(bno, buf)
    }
    fn write_block(&mut self, bno: u64, buf: &[u8]) -> Result<()> {
        self.ctx_write_block(bno, buf)
    }
    fn alloc_block(&mut self) -> Result<u64> {
        Filesystem::alloc_block(self)
    }
    fn free_block(&mut self, bno: u64) -> Result<()> {
        Filesystem::free_block(self, bno)
    }
}

// Empêche les avertissements sur les imports conditionnels de tests.
#[allow(unused)]
fn _assert_types() {
    let _ = S_IFDIR | S_IFREG;
}
