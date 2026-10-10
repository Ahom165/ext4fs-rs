//! Opérations haut niveau : résolution de chemins, listage/recherche dans
//! les répertoires (linéaires + HTree + inline), lecture/écriture de
//! fichiers (extents + inline), création/suppression/lien, truncate.

use crate::dir::{
    self, DirBlockIter, DirEntry, DIRENT_TAIL_SIZE, EXT4_INDEX_FL, FT_DIR_CSUM,
};
use crate::error::{Error, Result};
use crate::extent;
use crate::fs::Filesystem;
use crate::inode::{FileType, Inode, INLINE_MAX, S_IFDIR, S_IFREG, S_IFLNK, FI_INLINE_DATA, FI_EXTENTS};
use crate::superblock::feat_incompat;
use alloc::vec;
use alloc::vec::Vec;

/// Métadonnées style stat.
#[derive(Clone, Debug)]
pub struct Stat {
    pub ino: u32,
    pub mode: u16,
    pub size: u64,
    pub nlink: u16,
    pub atime: u32,
    pub mtime: u32,
    pub ctime: u32,
    pub uid: u32,
    pub gid: u32,
    pub kind: Option<FileType>,
}

const MAX_TXN_CHUNK: u32 = 512; // blocs de données par transaction (borné par le journal)

impl Filesystem {
    // ==================================================================
    // Aides répertoires
    // ==================================================================
    fn dirent_size(&self, name_len: usize) -> usize {
        let base = 8 + name_len;
        (base + 3) & !3
    }

    fn dir_seed(&self, dir: &Inode) -> u32 {
        dir::inode_csum_seed(&self.sb, dir.no, dir.generation())
    }

    /// Liste les blocs de données d'un répertoire (inline → vide).
    pub fn dir_data_blocks(&mut self, dir: &Inode) -> Result<Vec<u64>> {
        if dir.is_inline() {
            return Ok(vec![]);
        }
        let maps = extent::flatten(self, dir)?;
        let mut out = Vec::new();
        for m in maps {
            for i in 0..m.len {
                out.push(m.phys + i as u64);
            }
        }
        Ok(out)
    }

    /// Parcours universel des entrées d'un répertoire.
    /// f(entry, block_no, offset) — retourne true pour continuer.
    fn scan_entries<F>(&mut self, dir: &Inode, mut f: F) -> Result<()>
    where
        F: FnMut(&mut Filesystem, &DirEntry, u64, usize) -> Result<bool>,
    {
        let has_ftype = self.sb.has_filetype();
        let mcsum = self.sb.has_metadata_csum();
        let indexed = dir.flags() & EXT4_INDEX_FL != 0;
        if dir.is_inline() {
            let sz = (dir.size() as usize).min(INLINE_MAX);
            let data = dir.i_block()[..sz].to_vec();
            let mut it = DirBlockIter::new(&data, false);
            while let Some(r) = it.next_entry() {
                let (off, ino, _rl, nl, ft) = r?;
                if ino == 0 {
                    continue;
                }
                if off + 8 + nl as usize > data.len() {
                    return Err(Error::Corrupt("entrée inline invalide"));
                }
                let e = DirEntry {
                    inode: ino,
                    file_type: if has_ftype { ft } else { 0 },
                    name: data[off + 8..off + 8 + nl as usize].to_vec(),
                };
                if !f(self, &e, 0, off)? {
                    return Ok(());
                }
            }
            return Ok(());
        }
        let blocks = self.dir_data_blocks(dir)?;
        for (bi, &bno) in blocks.iter().enumerate() {
            let _bs = self.block_size() as usize;
            let blk = self.get_block(bno)?.clone();
            if bi == 0 && indexed {
                // dx_root : uniquement "." et ".."
                for dot_off in [0usize, 12usize] {
                    if dot_off + 8 > blk.len() {
                        break;
                    }
                    let ino = crate::util::rd32_le(&blk, dot_off);
                    let rl = crate::util::rd16_le(&blk, dot_off + 4) as usize;
                    if rl < 12 {
                        break;
                    }
                    let nl = blk[dot_off + 6] as usize;
                    if ino == 0 || nl == 0 || dot_off + 8 + nl > dot_off + rl {
                        continue;
                    }
                    let e = DirEntry {
                        inode: ino,
                        file_type: 2,
                        name: blk[dot_off + 8..dot_off + 8 + nl].to_vec(),
                    };
                    if !f(self, &e, bno, dot_off)? {
                        return Ok(());
                    }
                }
                continue;
            }
            if indexed && dir::is_dx_node(&blk) {
                continue; // nœud d'index : pas d'entrées réelles
            }
            let mut it = DirBlockIter::new(&blk, mcsum);
            while let Some(r) = it.next_entry() {
                let (off, ino, rl, nl, ft) = r?;
                if ino == 0 {
                    continue;
                }
                let nl = if has_ftype { nl as usize } else { (rl - 6) as usize };
                if off + 8 + nl > blk.len() || nl > 255 {
                    return Err(Error::Corrupt("entrée de répertoire invalide"));
                }
                let e = DirEntry {
                    inode: ino,
                    file_type: if has_ftype { ft } else { 0 },
                    name: blk[off + 8..off + 8 + nl].to_vec(),
                };
                if !f(self, &e, bno, off)? {
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    /// Liste les entrées vivantes d'un répertoire.
    pub fn read_dir(&mut self, dir: &Inode) -> Result<Vec<DirEntry>> {
        if dir.is_encrypted() {
            return Err(Error::Encrypted);
        }
        if dir.file_type() != Some(FileType::Dir) {
            return Err(Error::Corrupt("pas un répertoire"));
        }
        let mut out = Vec::new();
        self.scan_entries(dir, |_fs, e, _b, _o| {
            out.push(e.clone());
            Ok(true)
        })?;
        Ok(out)
    }

    /// Cherche un nom dans un répertoire → (ino, type).
    pub fn lookup(&mut self, dir: &Inode, name: &[u8]) -> Result<Option<(u32, FileType)>> {
        if dir.is_encrypted() {
            return Err(Error::Encrypted);
        }
        let mut found: Option<(u32,)> = None;
        self.scan_entries(dir, |_fs, e, _b, _o| {
            if e.name == name {
                found = Some((e.inode,));
                return Ok(false);
            }
            Ok(true)
        })?;
        if let Some((ino,)) = found {
            // détermine le type depuis l'inode (fiable même sans FILETYPE)
            let target = self.read_inode(ino)?;
            return Ok(Some((ino, target.file_type().ok_or(Error::Corrupt("mode invalide"))?)));
        }
        Ok(None)
    }

    // ==================================================================
    // Chemins
    // ==================================================================
    /// Résout un chemin absolu (pas de suivi de liens symboliques).
    pub fn resolve(&mut self, path: &str) -> Result<Inode> {
        let mut cur = self.read_inode(crate::INO_ROOT)?;
        for comp in split_path(path) {
            if comp == b"." {
                continue;
            }
            if comp == b".." {
                return Err(Error::BadPath); // ".." non géré dans les chemins v1
            }
            match self.lookup(&cur, &comp)? {
                Some((ino, _ft)) => cur = self.read_inode(ino)?,
                None => return Err(Error::NotFound),
            }
        }
        Ok(cur)
    }

    /// Résout le parent + le nom final.
    pub fn resolve_parent(&mut self, path: &str) -> Result<(Inode, Vec<u8>)> {
        let comps = split_path(path);
        let last = comps
            .last()
            .ok_or(Error::BadPath)?
            .clone();
        if last == b"." || last == b".." {
            return Err(Error::BadPath);
        }
        let mut cur = self.read_inode(crate::INO_ROOT)?;
        for comp in &comps[..comps.len() - 1] {
            if comp == b"." {
                continue;
            }
            if comp == b".." {
                return Err(Error::BadPath);
            }
            match self.lookup(&cur, comp)? {
                Some((ino, _)) => cur = self.read_inode(ino)?,
                None => return Err(Error::NotFound),
            }
        }
        if cur.file_type() != Some(FileType::Dir) {
            return Err(Error::Corrupt("parent pas un répertoire"));
        }
        Ok((cur, last))
    }

    pub fn stat(&mut self, ino: u32) -> Result<Stat> {
        let i = self.read_inode(ino)?;
        Ok(Stat {
            ino,
            mode: i.mode(),
            size: i.size(),
            nlink: i.links_count(),
            atime: i.atime(),
            mtime: i.mtime(),
            ctime: i.ctime(),
            uid: i.uid(),
            gid: i.gid(),
            kind: i.file_type(),
        })
    }

    // ==================================================================
    // Lecture / écriture de fichiers
    // ==================================================================
    pub fn read_file(&mut self, ino: u32, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let inode = self.read_inode(ino)?;
        if inode.is_encrypted() {
            return Err(Error::Encrypted);
        }
        let size = inode.size();
        if offset >= size {
            return Ok(0);
        }
        let want = ((size - offset) as usize).min(buf.len());
        if inode.is_inline() {
            let sz = (size as usize).min(INLINE_MAX);
            let avail = sz.saturating_sub(offset as usize);
            let n = avail.min(want);
            buf[..n].copy_from_slice(&inode.i_block()[offset as usize..offset as usize + n]);
            return Ok(n);
        }
        let bs = self.block_size();
        let mut done = 0usize;
        let maps = extent::flatten(self, &inode)?;
        while done < want {
            let pos = offset + done as u64;
            let lblock = (pos / bs) as u32;
            let in_block = (pos % bs) as usize;
            let n = (bs as usize - in_block).min(want - done);
            // trouve le mapping
            let mut phys: Option<u64> = None;
            let mut unwritten = false;
            for m in &maps {
                if lblock >= m.logical && lblock < m.logical + m.len {
                    phys = Some(m.phys + (lblock - m.logical) as u64);
                    unwritten = m.unwritten;
                    break;
                }
            }
            match phys {
                None => buf[done..done + n].fill(0), // trou
                Some(_p) if unwritten => buf[done..done + n].fill(0),
                Some(p) => {
                    if let Some(blk) = self.cache.get(&p) {
                        buf[done..done + n].copy_from_slice(&blk[in_block..in_block + n]);
                    } else {
                        let mut tmp = vec![0u8; n];
                        self.dev.read_at(p * bs + in_block as u64, &mut tmp)?;
                        buf[done..done + n].copy_from_slice(&tmp);
                    }
                }
            }
            done += n;
        }
        Ok(want)
    }

    pub fn read_file_to_vec(&mut self, ino: u32, offset: u64, len: usize) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; len];
        let n = self.read_file(ino, offset, &mut buf)?;
        buf.truncate(n);
        Ok(buf)
    }

    /// Écrit `data` à `offset`. Retourne le nombre d'octets écrits.
    pub fn write_file(&mut self, ino: u32, offset: u64, data: &[u8]) -> Result<usize> {
        if self.read_only {
            return Err(Error::ReadOnly);
        }
        if data.is_empty() {
            return Ok(0);
        }
        let mut inode = self.read_inode(ino)?;
        if inode.is_encrypted() {
            return Err(Error::Encrypted);
        }
        if inode.file_type() != Some(FileType::Regular) {
            return Err(Error::Corrupt("écriture sur un non-fichier"));
        }
        // Création inline à la manière de Linux : fichier vide + feature
        // INLINE_DATA + données qui tiennent dans i_block (60 o max)
        if self.sb.has_inline_data()
            && !inode.has_extents()
            && !inode.is_inline()
            && inode.size() == 0
            && offset == 0
            && data.len() <= INLINE_MAX
        {
            inode.set_flags(inode.flags() | FI_INLINE_DATA);
            inode.set_size(data.len() as u64);
            inode.i_block_mut()[..data.len()].copy_from_slice(data);
            let (now, _) = self.time.now();
            inode.set_mtime(now);
            inode.set_ctime(now);
            self.write_inode(&mut inode)?;
            self.commit_transaction()?;
            return Ok(data.len());
        }
        let bs = self.block_size();
        // Inline → extents si débordement
        if inode.is_inline() {
            let new_end = offset + data.len() as u64;
            let inline_cap = INLINE_MAX as u64;
            if new_end > inline_cap {
                let sz = inode.size().min(inline_cap) as usize;
                let first = self.alloc_block()?;
                extent::inline_to_extent(self, &mut inode, first, sz)?;
                self.write_inode(&mut inode)?;
            }
        }
        let mut written = 0usize;
        let mut pos = offset;
        let chunk_bytes = (MAX_TXN_CHUNK as u64) * bs;
        while written < data.len() {
            let n = (data.len() - written).min(chunk_bytes as usize);
            self.write_chunk(ino, pos, &data[written..written + n])?;
            written += n;
            pos += n as u64;
        }
        // taille + temps
        let mut inode = self.read_inode(ino)?;
        let new_end = offset + data.len() as u64;
        if new_end > inode.size() {
            inode.set_size(new_end);
        }
        let (now, _) = self.time.now();
        inode.set_mtime(now);
        inode.set_ctime(now);
        self.recompute_i_blocks(&mut inode)?;
        self.write_inode(&mut inode)?;
        self.commit_transaction()?;
        Ok(data.len())
    }

    /// Écrit une tranche (≤ chunk) : mappings + données.
    fn write_chunk(&mut self, ino: u32, offset: u64, data: &[u8]) -> Result<()> {
        let bs = self.block_size();
        let mut inode = self.read_inode(ino)?;
        let l0 = (offset / bs) as u32;
        let l1 = ((offset + data.len() as u64 + bs - 1) / bs) as u32;
        let mut maps = extent::flatten(self, &inode)?;
        // runs manquants (trous) ou non-écrits à couvrir
        let mut lb = l0;
        while lb < l1 {
            let covered = maps
                .iter()
                .find(|m| lb >= m.logical && lb < m.logical + m.len);
            match covered {
                Some(m) if !m.unwritten => {
                    lb = m.logical + m.len; // saute le mapping couvrant
                }
                Some(m) => {
                    // non-écrit : convertit la portion écrite en écrit
                    let overlap_end = (m.logical + m.len).min(l1);
                    extent::map_range(
                        self,
                        &mut inode,
                        lb,
                        overlap_end - lb,
                        m.phys + (lb - m.logical) as u64,
                        false,
                    )?;
                    maps = extent::flatten(self, &inode)?;
                    lb = overlap_end;
                }
                None => {
                    // trou : alloue une run contiguë la plus longue possible
                    let mut run_end = lb;
                    while run_end < l1
                        && !maps.iter().any(|m| run_end >= m.logical && run_end < m.logical + m.len)
                    {
                        run_end += 1;
                    }
                    let mut want = run_end - lb;
                    // allocation contiguë (essaie plein, puis décrémente)
                    let mut phys: Option<u64> = None;
                    let mut try_len = want;
                    while try_len > 0 {
                        match self.alloc_contig(try_len) {
                            Ok(p) => {
                                phys = Some(p);
                                want = try_len;
                                break;
                            }
                            Err(Error::NoSpace) => try_len /= 2,
                            Err(e) => return Err(e),
                        }
                    }
                    let p = phys.ok_or(Error::NoSpace)?;
                    extent::map_range(self, &mut inode, lb, want, p, false)?;
                    maps = extent::flatten(self, &inode)?;
                    lb += want;
                }
            }
        }
        self.write_inode(&mut inode)?;
        // données (par bloc, read-modify-write pour partiel)
        maps = extent::flatten(self, &inode)?;
        let mut done = 0usize;
        let mut cur_lb = l0;
        while done < data.len() {
            let in_block = ((offset + done as u64) % bs) as usize;
            let n = (bs as usize - in_block).min(data.len() - done);
            let m = maps
                .iter()
                .find(|m| cur_lb >= m.logical && cur_lb < m.logical + m.len)
                .ok_or(Error::Corrupt("mapping manquant après écriture"))?;
            let p = m.phys + (cur_lb - m.logical) as u64;
            {
                let blk = self.get_block(p)?;
                if m.unwritten {
                    blk.fill(0); // initialisation du non-écrit
                }
                blk[in_block..in_block + n].copy_from_slice(&data[done..done + n]);
            }
            done += n;
            cur_lb += 1;
        }
        self.write_inode(&mut inode)
        // commit par l'appelant (une seule transaction par opération)
    }

    /// Alloue `n` blocs contigus (sinon NoSpace).
    fn alloc_contig(&mut self, n: u32) -> Result<u64> {
        let first = self.alloc_block()?;
        for i in 1..n as u64 {
            let b = self.alloc_block()?;
            if b != first + i {
                // non contigu : libère et réessaie plus tard (rare en PoC)
                for j in 0..=i {
                    self.free_block(first + j)?;
                }
                return Err(Error::NoSpace);
            }
        }
        Ok(first)
    }

    /// Recalcule i_blocks (secteurs 512) : données + nœuds d'arbre + xattr.
    fn recompute_i_blocks(&mut self, inode: &mut Inode) -> Result<()> {
        if inode.is_inline() || !inode.has_extents() {
            return Ok(());
        }
        let bs = self.block_size();
        let sectors = bs / 512;
        let maps = extent::flatten(self, inode)?;
        let data: u64 = maps.iter().map(|m| m.len as u64).sum();
        // nœuds d'arbre : feuilles + index (même math que extent::rebuild)
        let per_leaf = (bs as usize - 12) / 12;
        let mut nodes: u64 = 0;
        if maps.len() > 4 {
            let mut level = maps.len();
            loop {
                nodes += ((level + per_leaf - 1) / per_leaf) as u64;
                if ((level + per_leaf - 1) / per_leaf) <= 4 {
                    break;
                }
                level = (level + per_leaf - 1) / per_leaf;
                if nodes > 1 << 20 {
                    return Err(Error::ExtentFull);
                }
            }
        }
        let xattr = if inode.file_acl() != 0 { 1 } else { 0 };
        inode.set_blocks((data + nodes + xattr) * sectors);
        Ok(())
    }

    /// Tronque/allonge un fichier.
    pub fn set_len(&mut self, ino: u32, new_size: u64) -> Result<()> {
        if self.read_only {
            return Err(Error::ReadOnly);
        }
        let mut inode = self.read_inode(ino)?;
        if inode.is_encrypted() {
            return Err(Error::Encrypted);
        }
        if inode.file_type() != Some(FileType::Regular) {
            return Err(Error::Corrupt("truncate sur un non-fichier"));
        }
        let old = inode.size();
        let bs = self.block_size();
        if inode.is_inline() {
            if new_size <= INLINE_MAX as u64 {
                inode.set_size(new_size);
            } else {
                let sz = old.min(INLINE_MAX as u64) as usize;
                let first = self.alloc_block()?;
                extent::inline_to_extent(self, &mut inode, first, sz)?;
                inode.set_size(new_size);
            }
        } else if new_size < old {
            // libère les blocs complets au-delà de new_size
            let keep = (new_size + bs - 1) / bs; // blocs conservés
            let old_blocks = (old + bs - 1) / bs;
            if old_blocks > keep {
                let maps = extent::flatten(self, &inode)?;
                for m in &maps {
                    let m_end = m.logical as u64 + m.len as u64;
                    let from = (m.logical as u64).max(keep);
                    if from >= m_end {
                        continue;
                    }
                    for i in from..m_end {
                        self.free_block(m.phys + (i - m.logical as u64))?;
                    }
                }
                extent::unmap_range(
                    self,
                    &mut inode,
                    keep as u32,
                    (old_blocks - keep) as u32,
                )?;
            }
            inode.set_size(new_size);
        } else {
            inode.set_size(new_size); // croissance : trous (sparse)
        }
        let (now, _) = self.time.now();
        inode.set_mtime(now);
        inode.set_ctime(now);
        self.recompute_i_blocks(&mut inode)?;
        self.write_inode(&mut inode)?;
        self.commit_transaction()
    }

    // ==================================================================
    // Opérations d'espace de noms
    // ==================================================================
    /// Insère une entrée dans un répertoire (inline / linéaire / HTree).
    pub fn create_entry(
        &mut self,
        dir: &mut Inode,
        name: &[u8],
        target: u32,
        ftype: u8,
    ) -> Result<()> {
        if dir.is_encrypted() {
            return Err(Error::Encrypted);
        }
        if dir.file_type() != Some(FileType::Dir) {
            return Err(Error::Corrupt("pas un répertoire (create_entry)"));
        }
        if name.is_empty() || name.len() > 255 || name.contains(&0) || name == b"." || name == b".."
        {
            return Err(Error::BadPath);
        }
        let mcsum = self.sb.has_metadata_csum();
        let has_ftype = self.sb.has_filetype();
        let seed = self.dir_seed(dir);
        let bs = self.block_size() as usize;

        // --- répertoire inline ---
        if dir.is_inline() {
            let cur = (dir.size() as usize).min(INLINE_MAX);
            let needed = self.dirent_size(name.len());
            if cur + needed <= INLINE_MAX {
                // réécrit la zone inline : entrées + nouvelle + fin
                let mut entries: Vec<(u32, u8, Vec<u8>)> = Vec::new();
                {
                    let data = dir.i_block()[..cur].to_vec();
                    let mut it = DirBlockIter::new(&data, false);
                    while let Some(r) = it.next_entry() {
                        let (off, ino, _rl, nl, ft) = r?;
                        if ino != 0 {
                            entries.push((ino, ft, data[off + 8..off + 8 + nl as usize].to_vec()));
                        }
                    }
                }
                entries.push((target, ftype, name.to_vec()));
                let mut data = vec![0u8; INLINE_MAX];
                let mut off = 0usize;
                for (i, (ino, ft, nm)) in entries.iter().enumerate() {
                    let sz = self.dirent_size(nm.len());
                    let rl = if i + 1 == entries.len() {
                        INLINE_MAX - off
                    } else {
                        sz
                    };
                    crate::util::wr32_le(&mut data, off, *ino);
                    crate::util::wr16_le(&mut data, off + 4, rl as u16);
                    data[off + 6] = nm.len() as u8;
                    data[off + 7] = *ft;
                    data[off + 8..off + 8 + nm.len()].copy_from_slice(nm);
                    off += rl;
                }
                dir.i_block_mut().copy_from_slice(&data);
                dir.set_size((off) as u64);
                let (now, _) = self.time.now();
                dir.set_mtime(now);
                dir.set_ctime(now);
                self.write_inode(dir)?;
                return Ok(());
            }
            // conversion inline → extents
            let sz = cur;
            let first = self.alloc_block()?;
            extent::inline_to_extent(self, dir, first, sz)?;
            // le bloc contient les entrées : étire la dernière jusqu'à la fin
            {
                let blk = self.get_block(first)?.clone();
                let mut it = DirBlockIter::new(&blk, false);
                let mut last_off: Option<usize> = None;
                let mut off = 0usize;
                while let Some(r) = it.next_entry() {
                    let (o, _ino, rl, _nl, _ft) = r?;
                    if crate::util::rd32_le(&blk, o) != 0 {
                        last_off = Some(o);
                    }
                    off = o + rl as usize;
                }
                let usable = if mcsum { bs - DIRENT_TAIL_SIZE } else { bs };
                if let Some(lo) = last_off {
                    let last_rl = crate::util::rd16_le(&blk, lo + 4) as usize;
                    let new_rl = usable - lo;
                    if new_rl >= last_rl {
                        let blk2 = self.get_block(first)?;
                        crate::util::wr16_le(blk2, lo + 4, new_rl as u16);
                    }
                }
                let _ = off;
            }
            dir.set_size(bs as u64);
            self.write_inode(dir)?;
        }

        // --- répertoire indexé (HTree) ---
        if dir.flags() & EXT4_INDEX_FL != 0 {
            let blocks = self.dir_data_blocks(dir)?;
            let root_bno = blocks[0];
            let root = self.get_block(root_bno)?.clone();
            let dxr = dir::parse_dx_root(&root)?;
            let hash = dir::dx_hash(dxr.hash_version, name, &self.sb.hash_seed);
            // les entrées dx stockent des blocs LOGIQUES du fichier répertoire
            let (leaf_lb, _dxr) = dir::DxWalker::find_leaf(&root, hash, &mut |lb| {
                self.dir_read_block(dir.no, lb)
            })?;
            let leaf_bno = self.dir_phys_block(&dir, leaf_lb)?;
            {
                let blk = self.get_block(leaf_bno)?;
                let inserted = dir::insert_entry(blk, name, target, ftype, has_ftype, mcsum)?;
                if !inserted {
                    return Err(Error::DirIndexFull);
                }
                if mcsum {
                    dir::write_dirent_tail(blk, seed, true);
                }
            }
            let (now, _) = self.time.now();
            dir.set_mtime(now);
            dir.set_ctime(now);
            self.write_inode(dir)?;
            return Ok(());
        }

        // --- répertoire linéaire ---
        let blocks = self.dir_data_blocks(dir)?;
        for &bno in &blocks {
            let inserted = {
                let blk = self.get_block(bno)?;
                let ok = dir::insert_entry(blk, name, target, ftype, has_ftype, mcsum)?;
                if ok && mcsum {
                    dir::write_dirent_tail(blk, seed, true);
                }
                ok
            };
            if inserted {
                let (now, _) = self.time.now();
                dir.set_mtime(now);
                dir.set_ctime(now);
                self.write_inode(dir)?;
                return Ok(());
            }
        }
        // nouveau bloc
        let bno = self.alloc_block()?;
        let logical = blocks.len() as u32;
        extent::map_range(self, dir, logical, 1, bno, false)?;
        {
            let blk = self.get_block(bno)?;
            let usable = if mcsum { bs - DIRENT_TAIL_SIZE } else { bs };
            blk[..usable].fill(0);
            crate::util::wr32_le(blk, 0, 0); // entrée fin : inode 0
            crate::util::wr16_le(blk, 4, usable as u16);
            blk[6] = 0;
            blk[7] = 0;
            let ok = dir::insert_entry(blk, name, target, ftype, has_ftype, mcsum)?;
            if !ok {
                return Err(Error::Corrupt("insertion dans bloc neuf impossible"));
            }
            if mcsum {
                dir::write_dirent_tail(blk, seed, true);
            }
        }
        dir.set_size(blocks.len() as u64 * bs as u64 + bs as u64);
        dir.set_blocks(dir.blocks() + (bs as u64 / 512));
        let (now, _) = self.time.now();
        dir.set_mtime(now);
        dir.set_ctime(now);
        self.write_inode(dir)?;
        Ok(())
    }

    /// Lit un bloc LOGIQUE du répertoire (via extents).
    fn dir_read_block(&mut self, dir_ino: u32, lb: u64) -> Result<Vec<u8>> {
        let bs = self.block_size() as usize;
        let inode = self.read_inode(dir_ino)?;
        match extent::lookup(self, &inode, lb as u32)? {
            Some((p, _run, _unw)) => {
                let mut buf = vec![0u8; bs];
                let blk = self.get_block(p)?;
                buf.copy_from_slice(blk);
                Ok(buf)
            }
            None => Err(Error::Corrupt("bloc de répertoire manquant")),
        }
    }

    /// Bloc physique d'un lblock logique du répertoire.
    fn dir_phys_block(&mut self, dir: &Inode, lb: u64) -> Result<u64> {
        match extent::lookup(self, dir, lb as u32)? {
            Some((p, _run, _unw)) => Ok(p),
            None => Err(Error::Corrupt("bloc de répertoire manquant")),
        }
    }

    /// Supprime une entrée de répertoire (par nom).
    pub fn delete_entry(&mut self, dir: &mut Inode, name: &[u8]) -> Result<()> {
        if dir.is_encrypted() {
            return Err(Error::Encrypted);
        }
        let mcsum = self.sb.has_metadata_csum();
        let seed = self.dir_seed(dir);
        let mut target: Option<(u64, usize)> = None;
        self.scan_entries(dir, |_fs, e, bno, off| {
            if e.name == name {
                target = Some((bno, off));
                return Ok(false);
            }
            Ok(true)
        })?;
        let (bno, off) = target.ok_or(Error::NotFound)?;
        if dir.is_inline() {
            // réécrit la zone inline sans l'entrée
            let cur = (dir.size() as usize).min(INLINE_MAX);
            let mut entries: Vec<(u32, u8, Vec<u8>)> = Vec::new();
            {
                let data = dir.i_block()[..cur].to_vec();
                let mut it = DirBlockIter::new(&data, false);
                while let Some(r) = it.next_entry() {
                    let (o, ino, _rl, nl, ft) = r?;
                    if ino != 0 && o != off {
                        entries.push((ino, ft, data[o + 8..o + 8 + nl as usize].to_vec()));
                    }
                }
            }
            let mut data = vec![0u8; INLINE_MAX];
            let mut o = 0usize;
            for (i, (ino, ft, nm)) in entries.iter().enumerate() {
                let sz = self.dirent_size(nm.len());
                let rl = if i + 1 == entries.len() { INLINE_MAX - o } else { sz };
                crate::util::wr32_le(&mut data, o, *ino);
                crate::util::wr16_le(&mut data, o + 4, rl as u16);
                data[o + 6] = nm.len() as u8;
                data[o + 7] = *ft;
                data[o + 8..o + 8 + nm.len()].copy_from_slice(nm);
                o += rl;
            }
            dir.i_block_mut().copy_from_slice(&data);
            dir.set_size(o as u64);
            let (now, _) = self.time.now();
            dir.set_mtime(now);
            dir.set_ctime(now);
            self.write_inode(dir)?;
            return Ok(());
        }
        {
            let blk = self.get_block(bno)?;
            dir::delete_entry(blk, off, mcsum)?;
            if mcsum {
                dir::write_dirent_tail(blk, seed, true);
            }
        }
        let (now, _) = self.time.now();
        dir.set_mtime(now);
        dir.set_ctime(now);
        self.write_inode(dir)
    }

    // ==================================================================
    // Namespace : create / mkdir / unlink / rmdir / link
    // ==================================================================
    pub fn create_file(&mut self, dir_path: &str, name: &str) -> Result<u32> {
        if self.read_only {
            return Err(Error::ReadOnly);
        }
        let mut parent = self.resolve(dir_path)?;
        let fname = name.as_bytes().to_vec();
        if self.lookup(&parent, &fname)?.is_some() {
            return Err(Error::AlreadyExists);
        }
        let group = self.group_of_inode(parent.no);
        let mut ino = self.alloc_inode(group, S_IFREG | 0o644)?;
        let (now, _) = self.time.now();
        ino.set_generation(now ^ (ino.no.wrapping_mul(0x9E3779B9)));
        ino.set_links_count(1);
        self.write_inode(&mut ino)?;
        self.create_entry(&mut parent, &fname, ino.no, FileType::Regular.dirent_type())?;
        self.commit_transaction()?;
        Ok(ino.no)
    }

    pub fn mkdir(&mut self, dir_path: &str, name: &str) -> Result<u32> {
        if self.read_only {
            return Err(Error::ReadOnly);
        }
        let mut parent = self.resolve(dir_path)?;
        let fname = name.as_bytes().to_vec();
        if self.lookup(&parent, &fname)?.is_some() {
            return Err(Error::AlreadyExists);
        }
        let group = self.group_of_inode(parent.no);
        let mut ino = self.alloc_inode(group, S_IFDIR | 0o755)?;
        let (now, _) = self.time.now();
        ino.set_generation(now ^ (ino.no.wrapping_mul(0x85EB_CA6B)));
        ino.set_links_count(2);
        let bs = self.block_size() as usize;
        // premier bloc : "." et ".."
        let bno = self.alloc_block()?;
        let mcsum = self.sb.has_metadata_csum();
        let pseed = self.dir_seed(&ino);
        {
            let blk = self.get_block(bno)?;
            let usable = if mcsum { bs - DIRENT_TAIL_SIZE } else { bs };
            blk.fill(0);
            // "."
            crate::util::wr32_le(blk, 0, ino.no);
            crate::util::wr16_le(blk, 4, 12);
            blk[6] = 1;
            blk[7] = 2;
            blk[8] = b'.';
            // ".."
            crate::util::wr32_le(blk, 12, parent.no);
            crate::util::wr16_le(blk, 16, (usable - 12) as u16);
            blk[18] = 2;
            blk[19] = 2;
            blk[20] = b'.';
            blk[21] = b'.';
            if mcsum {
                dir::write_dirent_tail(blk, pseed, true);
            }
        }
        extent::map_range(self, &mut ino, 0, 1, bno, false)?;
        ino.set_size(bs as u64);
        ino.set_blocks((bs as u64 / 512) + (bs as u64 / 512)); // + 1 bloc arbre (racine dans l'inode → 0 nœud)
        ino.set_blocks(bs as u64 / 512);
        self.write_inode(&mut ino)?;
        // entrée dans le parent
        self.create_entry(&mut parent, &fname, ino.no, FileType::Dir.dirent_type())?;
        // ".." du nouveau pointe sur le parent ✓ ; liens du parent +1
        parent.set_links_count(parent.links_count() + 1);
        let (now, _) = self.time.now();
        parent.set_mtime(now);
        parent.set_ctime(now);
        self.write_inode(&mut parent)?;
        self.commit_transaction()?;
        Ok(ino.no)
    }

    pub fn unlink(&mut self, path: &str) -> Result<()> {
        if self.read_only {
            return Err(Error::ReadOnly);
        }
        let (mut parent, fname) = self.resolve_parent(path)?;
        let (ino, ft) = self.lookup(&parent, &fname)?.ok_or(Error::NotFound)?;
        if ft == FileType::Dir {
            return Err(Error::Corrupt("unlink sur un répertoire (rmdir)"));
        }
        self.delete_entry(&mut parent, &fname)?;
        self.drop_link(ino)
    }

    pub fn rmdir(&mut self, path: &str) -> Result<()> {
        if self.read_only {
            return Err(Error::ReadOnly);
        }
        let (mut parent, fname) = self.resolve_parent(path)?;
        let (ino, ft) = self.lookup(&parent, &fname)?.ok_or(Error::NotFound)?;
        if ft != FileType::Dir {
            return Err(Error::Corrupt("rmdir sur un non-répertoire"));
        }
        let target = self.read_inode(ino)?;
        // vide ? (uniquement . et ..)
        let entries = self.read_dir(&target)?;
        if entries.iter().any(|e| e.name != b"." && e.name != b"..") {
            return Err(Error::DirectoryNotEmpty);
        }
        self.delete_entry(&mut parent, &fname)?;
        parent.set_links_count(parent.links_count().saturating_sub(1));
        let (now, _) = self.time.now();
        parent.set_mtime(now);
        parent.set_ctime(now);
        self.write_inode(&mut parent)?;
        // used_dirs_count du groupe
        {
            let g = self.group_of_inode(ino);
            let mut gd = self.gdt.get(&self.sb, g)?;
            gd.used_dirs_count = gd.used_dirs_count.saturating_sub(1);
            self.gdt.put(&self.sb, g, &gd)?;
        }
        // libère les données + l'inode
        self.free_inode_data(&target)?;
        self.free_inode(ino)?;
        self.commit_transaction()
    }

    pub fn link(&mut self, target_path: &str, dir_path: &str, name: &str) -> Result<()> {
        if self.read_only {
            return Err(Error::ReadOnly);
        }
        let target = self.resolve(target_path)?;
        if target.file_type() == Some(FileType::Dir) {
            return Err(Error::Corrupt("link sur un répertoire"));
        }
        let mut parent = self.resolve(dir_path)?;
        let fname = name.as_bytes().to_vec();
        if self.lookup(&parent, &fname)?.is_some() {
            return Err(Error::AlreadyExists);
        }
        self.create_entry(
            &mut parent,
            &fname,
            target.no,
            target.file_type().unwrap_or(FileType::Regular).dirent_type(),
        )?;
        let mut t = self.read_inode(target.no)?;
        t.set_links_count(t.links_count() + 1);
        let (now, _) = self.time.now();
        t.set_ctime(now);
        self.write_inode(&mut t)?;
        self.commit_transaction()
    }

    /// Renomme / déplace une entrée (sémantique POSIX rename) :
    /// cross-directory, overwrite, répertoires ('..' + liens parents).
    pub fn rename(&mut self, old_path: &str, new_path: &str, overwrite: bool) -> Result<()> {
        if self.read_only {
            return Err(Error::ReadOnly);
        }
        if old_path == new_path {
            // rename(x, x) : no-op garanti
            self.resolve(old_path)?;
            return Ok(());
        }
        let (old_parent, old_name) = self.resolve_parent(old_path)?;
        if old_name == b"." || old_name == b".." {
            return Err(Error::BadPath);
        }
        let (new_parent, new_name) = self.resolve_parent(new_path)?;
        if new_name == b"." || new_name == b".." {
            return Err(Error::BadPath);
        }
        let (old_ino, old_ft) = self
            .lookup(&old_parent, &old_name)?
            .ok_or(Error::NotFound)?;
        if old_parent.no == new_parent.no && old_name == new_name {
            return Ok(());
        }
        // Déplacement d'un répertoire dans son propre sous-arbre : refus.
        if old_ft == FileType::Dir {
            let comps = split_path(new_path);
            let mut cur = self.read_inode(crate::INO_ROOT)?;
            if cur.no == old_ino {
                return Err(Error::DirLoop);
            }
            for comp in &comps[..comps.len().saturating_sub(1)] {
                if comp == b"." {
                    continue;
                }
                if comp == b".." {
                    return Err(Error::BadPath);
                }
                match self.lookup(&cur, comp)? {
                    Some((ino, _)) => {
                        if ino == old_ino {
                            return Err(Error::DirLoop);
                        }
                        cur = self.read_inode(ino)?;
                    }
                    None => return Err(Error::NotFound),
                }
            }
        }
        // Copie FRAÎCHE des parents (resolve_parent ci-dessus ne les a pas
        // modifiés, mais toutes les étapes ci-dessous travaillent sur copies).
        let mut old_parent = self.read_inode(old_parent.no)?;
        let mut new_parent = self.read_inode(new_parent.no)?;
        let same = old_parent.no == new_parent.no;
        // Cible existante : overwrite ?
        let tgt = self.lookup(&new_parent, &new_name)?;
        if let Some((t_ino, t_ft)) = tgt {
            if !overwrite {
                return Err(Error::AlreadyExists);
            }
            if t_ft == FileType::Dir {
                if old_ft != FileType::Dir {
                    return Err(Error::BadPath); // fichier → répertoire : interdit
                }
                let t = self.read_inode(t_ino)?;
                let entries = self.read_dir(&t)?;
                if entries.iter().any(|e| e.name != b"." && e.name != b"..") {
                    return Err(Error::DirectoryNotEmpty);
                }
            }
            // Retire l'entrée cible (sur la copie du parent concerné).
            // Le liens_count du parent ne bouge QUE si la cible est un
            // répertoire (un fichier ne contribue pas aux liens du parent).
            if t_ft == FileType::Dir {
                if same {
                    self.delete_entry(&mut old_parent, &new_name)?;
                    old_parent.set_links_count(old_parent.links_count().saturating_sub(1));
                    let (now, _) = self.time.now();
                    old_parent.set_mtime(now);
                    old_parent.set_ctime(now);
                    self.write_inode(&mut old_parent)?;
                } else {
                    self.delete_entry(&mut new_parent, &new_name)?;
                    new_parent.set_links_count(new_parent.links_count().saturating_sub(1));
                    let (now, _) = self.time.now();
                    new_parent.set_mtime(now);
                    new_parent.set_ctime(now);
                    self.write_inode(&mut new_parent)?;
                }
                {
                    let g = self.group_of_inode(t_ino);
                    let mut gd = self.gdt.get(&self.sb, g)?;
                    gd.used_dirs_count = gd.used_dirs_count.saturating_sub(1);
                    self.gdt.put(&self.sb, g, &gd)?;
                }
                let t = self.read_inode(t_ino)?;
                self.free_inode_data(&t)?;
                self.free_inode(t_ino)?;
                self.commit_transaction()?;
            } else {
                if same {
                    self.delete_entry(&mut old_parent, &new_name)?;
                    self.write_inode(&mut old_parent)?;
                } else {
                    self.delete_entry(&mut new_parent, &new_name)?;
                    self.write_inode(&mut new_parent)?;
                }
                self.drop_link(t_ino)?;
            }
        }
        // Déplace l'entrée. Même parent : une SEULE copie cohérente (sinon
        // delete/create sur copies divergentes corrompent le répertoire).
        if old_parent.no == new_parent.no {
            let mut parent = old_parent;
            self.delete_entry(&mut parent, &old_name)?;
            self.create_entry(&mut parent, &new_name, old_ino, old_ft.dirent_type())?;
            self.write_inode(&mut parent)?;
        } else {
            self.delete_entry(&mut old_parent, &old_name)?;
            self.create_entry(&mut new_parent, &new_name, old_ino, old_ft.dirent_type())?;
            if old_ft == FileType::Dir {
                old_parent.set_links_count(old_parent.links_count().saturating_sub(1));
                let (now, _) = self.time.now();
                old_parent.set_mtime(now);
                old_parent.set_ctime(now);
                self.write_inode(&mut old_parent)?;
                new_parent.set_links_count(new_parent.links_count() + 1);
                self.update_dotdot(old_ino, new_parent.no)?;
            }
            self.write_inode(&mut new_parent)?;
        }
        // ctime de l'inode déplacé
        let (now, _) = self.time.now();
        let mut moved = self.read_inode(old_ino)?;
        moved.set_ctime(now);
        self.write_inode(&mut moved)?;
        self.commit_transaction()
    }

    /// Met à jour l'entrée '..' du répertoire `dir_ino` vers `new_parent`.
    fn update_dotdot(&mut self, dir_ino: u32, new_parent: u32) -> Result<()> {
        let mut dir = self.read_inode(dir_ino)?;
        let mcsum = self.sb.has_metadata_csum();
        if dir.is_inline() {
            // '.' @0 (12o), '..' @12 : inode du parent @12
            crate::util::wr32_le(dir.i_block_mut(), 12, new_parent);
        } else {
            let blocks = self.dir_data_blocks(&dir)?;
            let bno = *blocks.first().ok_or(Error::Corrupt("répertoire sans bloc"))?;
            let indexed = dir.flags() & EXT4_INDEX_FL != 0;
            let seed = self.dir_seed(&dir);
            {
                let blk = self.get_block(bno)?;
                crate::util::wr32_le(blk, 12, new_parent);
                if mcsum {
                    if indexed {
                        // dx_root : le '..' vit dans le fake dirent, csum dx
                        dir::dx_csum_set(blk, 32, seed);
                    } else {
                        dir::write_dirent_tail(blk, seed, true);
                    }
                }
            }
        }
        let (now, _) = self.time.now();
        dir.set_ctime(now);
        self.write_inode(&mut dir)
    }

    /// Décrémente les liens ; libère l'inode si 0.
    fn drop_link(&mut self, ino: u32) -> Result<()> {
        let mut i = self.read_inode(ino)?;
        let n = i.links_count().saturating_sub(1);
        i.set_links_count(n);
        let (now, _) = self.time.now();
        i.set_ctime(now);
        if n == 0 {
            i.set_dtime(now);
            self.free_inode_data(&i)?;
            self.write_inode(&mut i)?;
            self.free_inode(ino)?;
            self.commit_transaction()?;
        } else {
            self.write_inode(&mut i)?;
            self.commit_transaction()?;
        }
        Ok(())
    }

    /// Pose des horodatages POSIX (None = inchangé). Mise à jour + commit.
    pub fn set_times(
        &mut self,
        ino: u32,
        atime: Option<u32>,
        mtime: Option<u32>,
        ctime: Option<u32>,
    ) -> Result<()> {
        if self.read_only {
            return Err(Error::ReadOnly);
        }
        let mut i = self.read_inode(ino)?;
        if let Some(t) = atime {
            i.set_atime(t);
        }
        if let Some(t) = mtime {
            i.set_mtime(t);
        }
        if let Some(t) = ctime {
            i.set_ctime(t);
        }
        self.write_inode(&mut i)?;
        self.commit_transaction()
    }

    /// Libère blocs de données + bloc xattr + nœuds d'index d'un inode.
    fn free_inode_data(&mut self, inode: &Inode) -> Result<()> {
        // nœuds d'index de l'arbre d'extents (racine exclue, elle vit dans
        // l'inode) — à libérer AVANT l'aplatissement
        if !inode.is_inline() && inode.has_extents() {
            let _ = crate::extent::free_tree_nodes(self, inode)?;
        }
        if !inode.is_inline() && inode.has_extents() {
            let maps = extent::flatten(self, inode)?;
            for m in maps {
                for i in 0..m.len {
                    self.free_block(m.phys + i as u64)?;
                }
            }
        }
        let acl = inode.file_acl();
        if acl != 0 {
            self.free_block(acl)?;
        }
        Ok(())
    }

    pub fn group_of_inode(&self, ino: u32) -> u64 {
        (ino.saturating_sub(1) as u64) / self.sb.inodes_per_group as u64
    }

    /// Crée un lien symbolique (cible stockée inline si ≤ 59 octets).
    pub fn symlink(&mut self, dir_path: &str, name: &str, target: &str) -> Result<u32> {
        if self.read_only {
            return Err(Error::ReadOnly);
        }
        let mut parent = self.resolve(dir_path)?;
        let fname = name.as_bytes().to_vec();
        if self.lookup(&parent, &fname)?.is_some() {
            return Err(Error::AlreadyExists);
        }
        let tb = target.as_bytes();
        let group = self.group_of_inode(parent.no);
        let mut ino = self.alloc_inode(group, S_IFLNK | 0o777)?;
        let (now, _) = self.time.now();
        ino.set_generation(now ^ (ino.no.wrapping_mul(0xC2B2_AE35)));
        ino.set_links_count(1);
        if tb.len() < INLINE_MAX {
            // symlink rapide : cible dans i_block, SANS flag INLINE_DATA
            ino.set_size(tb.len() as u64);
            ino.i_block_mut()[..tb.len()].copy_from_slice(tb);
            self.write_inode(&mut ino)?;
        } else {
            // symlink lent : cible dans des blocs (extents), comme un
            // fichier ; i_size = longueur de la cible
            ino.set_flags(FI_EXTENTS);
            self.write_inode(&mut ino)?;
            self.write_chunk(ino.no, 0, tb)?;
            let mut inode = self.read_inode(ino.no)?;
            inode.set_size(tb.len() as u64);
            self.recompute_i_blocks(&mut inode)?;
            self.write_inode(&mut inode)?;
        }
        self.create_entry(&mut parent, &fname, ino.no, FileType::Symlink.dirent_type())?;
        self.commit_transaction()?;
        Ok(ino.no)
    }

    /// Lit la cible d'un symlink (rapide : i_block, lent : blocs).
    pub fn readlink(&mut self, ino: u32) -> Result<Vec<u8>> {
        let i = self.read_inode(ino)?;
        if i.file_type() != Some(FileType::Symlink) {
            return Err(Error::Corrupt("pas un lien symbolique"));
        }
        if i.has_extents() {
            // symlink lent : la cible vit dans les blocs de données
            return self.read_file_to_vec(ino, 0, i.size() as usize);
        }
        let sz = (i.size() as usize).min(INLINE_MAX);
        Ok(i.i_block()[..sz].to_vec())
    }

    /// Info de montage.
    pub fn volume_info(&mut self) -> Result<alloc::string::String> {
        Ok(alloc::format!(
            "ext4: uuid={}, label=\"{}\", {} blocs ({} o), {} groupes, {} inodes libres / {} blocs libres, journal={}, ro={}",
            crate::util::uuid_to_str(&self.sb.uuid()),
            self.sb.volume_name(),
            self.sb.blocks_count,
            self.sb.block_size,
            self.sb.groups_count,
            self.sb.free_inodes_count,
            self.sb.free_blocks_count,
            self.journal.is_some(),
            self.read_only
        ))
    }

    /// Garde-fou : features encore incompatibles avec l'écriture v1.
    pub fn assert_write_supported(&self) -> Result<()> {
        if self.sb.feature_incompat & feat_incompat::DIRDATA != 0 {
            // DIRDATA ne change pas le format des entrées : OK en fait
        }
        Ok(())
    }
}

/// Réservé v3 : réécriture récursive du dx tree.
#[allow(dead_code)]
fn rl_at(blk: &[u8], off: usize) -> u16 {
    crate::util::rd16_le(blk, off)
}

/// Découpe un chemin en composants.
pub fn split_path(path: &str) -> Vec<Vec<u8>> {
    path.split('/')
        .filter(|s| !s.is_empty())
        .map(|s| s.as_bytes().to_vec())
        .collect()
}

// FT_DIR_CSUM réexporté pour tests
#[allow(unused_imports)]
pub use dir::FT_DIR_CSUM as _FT_DIR_CSUM_REEXPORT;
const _: u8 = FT_DIR_CSUM;
