//! IRP_MJ_READ / IRP_MJ_WRITE (Cache Manager + paging IO direct ext4-core)
//! et IRP_MJ_DIRECTORY_CONTROL (énumération + notifications de changement).

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::ffi::c_void;
use core::ptr::{addr_of, null_mut};

use crate::fsd::ext4_attrs_to_win;
use crate::fsd::complete_irp;
use crate::fsd_types::*;
use crate::fsd_info::{complete_err, complete_err_raw};
use crate::ntdefs as nd;
use crate::ntshim::*;
use crate::uni;
use wdk_sys::{
    ntddk::{ExAcquireFastMutex, ExReleaseFastMutex},
    IO_STATUS_BLOCK, LARGE_INTEGER, PIRP, UNICODE_STRING,
};

// Callbacks du Cache Manager (statiques, tous les volumes/fichiers)
static mut CACHE_CALLBACKS: Option<CACHE_MANAGER_CALLBACKS> = None;

unsafe fn cache_callbacks() -> *mut CACHE_MANAGER_CALLBACKS {
    let ptr = core::ptr::addr_of_mut!(CACHE_CALLBACKS);
    if (*ptr).is_none() {
        (*ptr) = Some(CACHE_MANAGER_CALLBACKS {
            AcquireForLazyWrite: cc_acquire_lazy,
            ReleaseFromLazyWrite: cc_release_lazy,
            AcquireForReadAhead: cc_acquire_readahead,
            ReleaseFromReadAhead: cc_release_readahead,
        });
    }
    match (*ptr).as_mut() {
        Some(cb) => cb as *mut CACHE_MANAGER_CALLBACKS,
        None => core::ptr::null_mut(),
    }
}

unsafe extern "system" fn cc_acquire_lazy(ctx: *mut c_void, _wait: u8) -> u8 {
    if ctx.is_null() {
        return 0;
    }
    let fcb = ctx as *mut Fcb;
    ExAcquireFastMutex(&mut (*fcb).lazy_mutex);
    1
}

unsafe extern "system" fn cc_release_lazy(ctx: *mut c_void) {
    if ctx.is_null() {
        return;
    }
    let fcb = ctx as *mut Fcb;
    ExReleaseFastMutex(&mut (*fcb).lazy_mutex);
}

unsafe extern "system" fn cc_acquire_readahead(_ctx: *mut c_void, _wait: u8) -> u8 {
    1
}

unsafe extern "system" fn cc_release_readahead(_ctx: *mut c_void) {}

/// Récupère le buffer système d'un IRP (MDL ou SystemBuffer).
pub unsafe fn irp_buffer(irp: PIRP, len: usize) -> Result<*mut c_void, i32> {
    let mdl: *mut c_void = core::ptr::read(addr_of!((*irp).MdlAddress) as *const *mut c_void);
    if !mdl.is_null() {
        let addr = MmGetSystemAddressForMdlSafe(mdl, NormalPagePriority);
        if addr.is_null() {
            return Err(nd::st::UNSUCCESSFUL);
        }
        return Ok(addr);
    }
    let sys: *mut c_void = core::ptr::read(addr_of!((*irp).AssociatedIrp.SystemBuffer));
    if !sys.is_null() {
        return Ok(sys);
    }
    let user: *mut c_void = core::ptr::read(addr_of!((*irp).UserBuffer));
    if !user.is_null() {
        return Ok(user);
    }
    if len == 0 {
        Ok(null_mut())
    } else {
        Err(nd::st::INVALID_USER_BUFFER)
    }
}



#[inline]
unsafe fn is_paging(irp: PIRP) -> bool {
    irp_flags(irp) & (nd::IRP_PAGING_IO | nd::IRP_NOCACHE) != 0
}

/// Initialise le Cc pour un fichier si pas encore fait.
unsafe fn ensure_cache_map(vcb: *mut Vcb, fo: wdk_sys::PFILE_OBJECT, fcb: *mut Fcb) {
    // le marqueur fiable "cache initialisé" est le SharedCacheMap
    if !(*fcb).sps.SharedCacheMap.is_null() {
        return;
    }
    if (*fcb).ftype == FCB_TYPE_DIR || (*fcb).ftype == FCB_TYPE_ROOT {
        return; // pas de Cc pour les répertoires v1
    }
    let mut sizes = CC_FILE_SIZES {
        AllocationSize: (*fcb).alloc_size as i64,
        FileSize: (*fcb).size as i64,
        ValidDataLength: (*fcb).size as i64,
    };
    CcInitializeCacheMap(
        fo as *mut c_void,
        &mut sizes,
        0,
        cache_callbacks(),
        fcb as *mut c_void,
    );
    let _ = vcb;
}

// ---------------------------------------------------------------------------
// READ
// ---------------------------------------------------------------------------

pub unsafe fn dispatch_read(vcb: *mut Vcb, irp: PIRP) -> i32 {
    let sp = (*irp).Tail.Overlay.__bindgen_anon_2.__bindgen_anon_1.CurrentStackLocation;
    let fo = file_object(sp);
    let ccb = fs_context2(fo) as *mut Ccb;
    let fcb = fs_context(fo) as *mut Fcb;
    if ccb.is_null() || fcb.is_null() {
        return complete_err_raw(irp, nd::st::INVALID_DEVICE_REQUEST);
    }
    if (*ccb).cctype == CCB_TYPE_VOLUME {
        // DASD read v1 : refusé (le contenu passe par le control device)
        return complete_err_raw(irp, nd::st::INVALID_DEVICE_REQUEST);
    }
    // v2 : flux alternatif (xattr user.ADS$*, pas de Cc)
    if !(*fcb).stream.is_none() {
        return crate::fsd_xattr::stream_read(vcb, irp, fcb);
    }
    if (*fcb).ftype == FCB_TYPE_DIR || (*fcb).ftype == FCB_TYPE_ROOT {
        return complete_err_raw(irp, nd::st::ACCESS_DENIED);
    }
    let (len, offset) = p_read(sp);
    let fsize = (*fcb).size;
    if offset >= fsize as i64 || len == 0 {
        complete_irp(irp, 0, 0);
        return 0;
    }
    let buf = match irp_buffer(irp, len as usize) {
        Ok(b) => b,
        Err(s) => return complete_err_raw(irp, s),
    };
    let out_len = (len as u64).min(fsize - offset as u64) as usize;

    if is_paging(irp) {
        // lecture directe via ext4-core (sous le verrou volume)
        ExAcquireFastMutex(&mut (*vcb).mutex);
        let slice = core::slice::from_raw_parts_mut(buf as *mut u8, out_len);
        let res = (*vcb).fs.read_file((*fcb).ino, offset as u64, slice);
        ExReleaseFastMutex(&mut (*vcb).mutex);
        match res {
            Ok(n) => {
                // complète le reste avec des zéros (fin de fichier, pages)
                if n < out_len {
                    core::slice::from_raw_parts_mut(buf as *mut u8, out_len)[n..].fill(0);
                }
                complete_irp(irp, 0, out_len);
                0
            }
            Err(e) => complete_err(irp, &e),
        }
    } else {
        ensure_cache_map(vcb, fo, fcb);
        let mut off = offset;
        let mut iosb = IO_STATUS_BLOCK::default();
        let ok = CcCopyRead(
            fo as *mut c_void,
            &mut off,
            len,
            1, // Wait
            buf,
            &mut iosb as *mut IO_STATUS_BLOCK as *mut c_void,
        );
        if ok == 0 {
            let status = core::ptr::read(core::ptr::addr_of!(iosb.__bindgen_anon_1.Status));
            return complete_err_raw(irp, if status == 0 { nd::st::UNSUCCESSFUL } else { status });
        }
        let info = core::ptr::read(core::ptr::addr_of!(iosb.Information));
        complete_irp(irp, 0, info as usize);
        0
    }
}

// ---------------------------------------------------------------------------
// WRITE
// ---------------------------------------------------------------------------

pub unsafe fn dispatch_write(vcb: *mut Vcb, irp: PIRP) -> i32 {
    if (*vcb).read_only {
        return complete_err_raw(irp, nd::st::MEDIA_WRITE_PROTECTED);
    }
    let sp = (*irp).Tail.Overlay.__bindgen_anon_2.__bindgen_anon_1.CurrentStackLocation;
    let fo = file_object(sp);
    let ccb = fs_context2(fo) as *mut Ccb;
    let fcb = fs_context(fo) as *mut Fcb;
    if ccb.is_null() || fcb.is_null() {
        return complete_err_raw(irp, nd::st::INVALID_DEVICE_REQUEST);
    }
    if (*ccb).cctype == CCB_TYPE_VOLUME {
        // DASD write : refusé (le volume est géré par ext4-core)
        return complete_err_raw(irp, nd::st::ACCESS_DENIED);
    }
    // v2 : flux alternatif (xattr user.ADS$*, pas de Cc)
    if !(*fcb).stream.is_none() {
        return crate::fsd_xattr::stream_write(vcb, irp, fcb);
    }
    if (*fcb).ftype == FCB_TYPE_DIR || (*fcb).ftype == FCB_TYPE_ROOT {
        return complete_err_raw(irp, nd::st::ACCESS_DENIED);
    }
    if (*ccb).delete_pending {
        return complete_err_raw(irp, nd::st::FILE_DELETED);
    }
    // oplocks : casser avant d'écrire (jamais pour le paging IO)
    if !(*fcb).oplock.is_null() && !is_paging(irp) {
        let s = crate::fsd_xattr::oplock_write_check(irp, fcb);
        if s != 0 {
            return s;
        }
    }
    let (len, offset) = p_read(sp);
    if len == 0 {
        complete_irp(irp, 0, 0);
        return 0;
    }
    let buf = match irp_buffer(irp, len as usize) {
        Ok(b) => b,
        Err(s) => return complete_err_raw(irp, s),
    };

    // étend la taille vue Windows si besoin
    let new_end = (offset as u64) + len as u64;
    if new_end > (*fcb).size {
        ExAcquireFastMutex(&mut (*fcb).lazy_mutex);
        (*fcb).size = new_end;
        let bs = (*vcb).fs.block_size();
        (*fcb).alloc_size = (new_end + bs - 1) / bs * bs;
        let mut sizes = CC_FILE_SIZES {
            AllocationSize: (*fcb).alloc_size as i64,
            FileSize: (*fcb).size as i64,
            ValidDataLength: (*fcb).size as i64,
        };
        CcSetFileSizes(fo as *mut c_void, &mut sizes);
        ExReleaseFastMutex(&mut (*fcb).lazy_mutex);
    }

    if is_paging(irp) {
        // paging write : borne à la taille du fichier (MM n'écrit jamais
        // au-delà) puis écrit via ext4-core
        let fsize = (*fcb).size;
        let out_len = ((offset as u64) + len as u64).min(fsize).saturating_sub(offset as u64) as usize;
        if out_len == 0 {
            complete_irp(irp, 0, 0);
            return 0;
        }
        ExAcquireFastMutex(&mut (*vcb).mutex);
        let slice = core::slice::from_raw_parts(buf as *const u8, out_len);
        let res = (*vcb).fs.write_file((*fcb).ino, offset as u64, slice);
        ExReleaseFastMutex(&mut (*vcb).mutex);
        match res {
            Ok(n) => {
                touch_times(vcb, fcb, false);
                complete_irp(irp, 0, n);
                0
            }
            Err(e) => complete_err(irp, &e),
        }
    } else {
        ensure_cache_map(vcb, fo, fcb);
        let mut off = offset;
        let ok = CcCopyWrite(
            fo as *mut c_void,
            &mut off,
            len,
            1, // Wait
            buf,
        );
        if ok == 0 {
            return complete_err_raw(irp, nd::st::UNSUCCESSFUL);
        }
        // write-through : flush immédiat des pages sales (paging writes)
        // puis commit journal si dirty
        let mut iosb = IO_STATUS_BLOCK::default();
        CcFlushCache(
            &(*fcb).sps as *const _ as *mut c_void,
            null_mut(),
            0,
            &mut iosb as *mut IO_STATUS_BLOCK as *mut c_void,
        );
        let flush_status = core::ptr::read(core::ptr::addr_of!(iosb.__bindgen_anon_1.Status));
        if flush_status != 0 {
            return complete_err_raw(irp, flush_status);
        }
        ExAcquireFastMutex(&mut (*vcb).mutex);
        let res = (*vcb).fs.flush();
        ExReleaseFastMutex(&mut (*vcb).mutex);
        if let Err(e) = res {
            return complete_err(irp, &e);
        }
        touch_times(vcb, fcb, false);
        complete_irp(irp, 0, len as usize);
        0
    }
}

/// Met à jour les temps de modification (RAM FCB).
unsafe fn touch_times(vcb: *mut Vcb, fcb: *mut Fcb, create: bool) {
    let mut t = LARGE_INTEGER::default();
    KeQuerySystemTime(&mut t as *mut LARGE_INTEGER as *mut i64);
    let now = core::ptr::read(core::ptr::addr_of!(t.QuadPart));
    (*fcb).write = now;
    (*fcb).change = now;
    if create {
        (*fcb).creation = now;
    }
    let _ = vcb;
}

// ---------------------------------------------------------------------------
// DIRECTORY CONTROL
// ---------------------------------------------------------------------------

pub unsafe fn dispatch_dirctl(vcb: *mut Vcb, irp: PIRP) -> i32 {
    let sp = (*irp).Tail.Overlay.__bindgen_anon_2.__bindgen_anon_1.CurrentStackLocation;
    let minor: u8 = core::ptr::read(core::ptr::addr_of!((*sp).MinorFunction));
    match minor {
        nd::MN_QUERY_DIRECTORY => query_directory(vcb, irp),
        nd::MN_NOTIFY_CHANGE_DIRECTORY => notify_change(vcb, irp),
        _ => complete_err_raw(irp, nd::st::INVALID_DEVICE_REQUEST),
    }
}

/// Flags IrpSp pour QUERY_DIRECTORY
pub const SL_RESTART_SCAN: u8 = 0x01;
pub const SL_RETURN_SINGLE_ENTRY: u8 = 0x02;
pub const SL_INDEX_SPECIFIED: u8 = 0x04;

unsafe fn query_directory(vcb: *mut Vcb, irp: PIRP) -> i32 {
    let sp = (*irp).Tail.Overlay.__bindgen_anon_2.__bindgen_anon_1.CurrentStackLocation;
    let fo = file_object(sp);
    let ccb = fs_context2(fo) as *mut Ccb;
    let fcb = fs_context(fo) as *mut Fcb;
    if ccb.is_null() || fcb.is_null() {
        return complete_err_raw(irp, nd::st::INVALID_DEVICE_REQUEST);
    }
    if (*ccb).cctype != CCB_TYPE_DIR {
        return complete_err_raw(irp, nd::st::INVALID_PARAMETER);
    }
    let (out_len, fname, class, file_index) = p_query_dir(sp);
    let out_len = out_len as usize;
    let file_index = file_index;
    let flags: u8 = sp_flags(sp);
    let restart = flags & SL_RESTART_SCAN != 0 || (*ccb).dir_snapshot.is_none();
    let single = flags & SL_RETURN_SINGLE_ENTRY != 0;
    let use_index = flags & SL_INDEX_SPECIFIED != 0;

    let name_off = match nd::dir_name_offset(class) {
        Some(o) => o,
        None => return complete_err_raw(irp, nd::st::INVALID_PARAMETER),
    };
    let hdr_size = nd::dir_header_size(class).unwrap();

    let out = match irp_buffer(irp, out_len) {
        Ok(b) if !b.is_null() => b as *mut u8,
        _ => return complete_err_raw(irp, nd::st::INVALID_PARAMETER),
    };

    // pattern (mise à jour si fourni sur cet appel)
    if !fname.is_null() {
        let us = core::ptr::read(fname);
        if us.Length > 0 {
            let n = (us.Length as usize) / 2;
            let mut w = Vec::with_capacity(n);
            for i in 0..n {
                w.push(core::ptr::read(us.Buffer.add(i)));
            }
            (*ccb).pattern = Some(w);
        } else {
            (*ccb).pattern = None;
        }
    } else if restart {
        (*ccb).pattern = None;
    }

    // snapshot du contenu (au premier appel / restart)
    if restart {
        let dir = match (*vcb).fs.read_inode((*fcb).ino) {
            Ok(d) => d,
            Err(e) => return complete_err(irp, &e),
        };
        match (*vcb).fs.read_dir(&dir) {
            Ok(entries) => {
                let mut snap: Vec<(Vec<u8>, u32, u8)> = Vec::new();
                for e in entries {
                    if e.name == b"." || e.name == b".." || e.inode == 0 {
                        continue;
                    }
                    snap.push((e.name, e.inode, e.file_type));
                }
                (*ccb).dir_snapshot = Some(snap);
                (*ccb).dir_cursor = 0;
            }
            Err(e) => return complete_err(irp, &e),
        }
    }
    if use_index {
        (*ccb).dir_cursor = file_index as usize;
    }

    // expression de filtrage
    let mut expr: Option<UNICODE_STRING> = None;
    if let Some(p) = &(*ccb).pattern {
        if !p.is_empty() {
            let mut b = p.clone();
            b.push(0);
            let len = (b.len() - 1) * 2;
            let ptr = Box::into_raw(b.into_boxed_slice()) as *mut u16;
            expr = Some(UNICODE_STRING {
                Length: len as u16,
                MaximumLength: (len + 2) as u16,
                Buffer: ptr,
            });
        }
    }

    let bs = (*vcb).fs.block_size();
    let snap_len = (*ccb).dir_snapshot.as_ref().map(|s| s.len()).unwrap_or(0);
    let mut written = 0usize;
    let mut prev_entry: Option<usize> = None;
    let mut idx = (*ccb).dir_cursor;
    let mut matched_any = false;

    while idx < snap_len {
        let (name8, ino, _ftype) = {
            let snap = (*ccb).dir_snapshot.as_ref().unwrap();
            (snap[idx].0.clone(), snap[idx].1, snap[idx].2)
        };
        let mut name = uni::utf8_to_utf16(&name8);

        // filtre pattern (case sensitive : ext4)
        if let Some(e) = &expr {
            let mut nb = name.clone();
            nb.push(0);
            let nlen = nb.len() * 2;
            let nbuf = Box::into_raw(nb.into_boxed_slice()) as *mut u16;
            let nus = UNICODE_STRING {
                Length: (nlen - 2) as u16,
                MaximumLength: nlen as u16,
                Buffer: nbuf,
            };
            let expr_mut = e as *const UNICODE_STRING as *mut UNICODE_STRING;
            matched_any = FsRtlIsNameInExpression(
                expr_mut,
                &nus as *const UNICODE_STRING as *mut UNICODE_STRING,
                0,
                null_mut(),
            ) != 0;
            if !matched_any {
                idx += 1;
                continue;
            }
        }
        matched_any = true;

        let st = match (*vcb).fs.stat(ino) {
            Ok(s) => s,
            Err(_) => {
                idx += 1;
                continue;
            }
        };
        let reparse = st.mode & 0xF000 == 0xA000;
        let mut attrs = ext4_attrs_to_win(st.mode, ftype_of(st.mode));
        if reparse {
            attrs |= nd::FILE_ATTRIBUTE_REPARSE_POINT;
        }

        let rec_len = align8(hdr_size + name.len() * 2);
        if written + rec_len > out_len {
            if written == 0 {
                return complete_err_raw(irp, nd::st::BUFFER_TOO_SMALL);
            }
            break; // complet au prochain appel
        }

        fill_entry(out, written, class, rec_len, idx, &st, attrs, &name, ino, bs);

        // chaînage de l'entrée précédente
        if let Some(p) = prev_entry {
            core::ptr::write_unaligned(out.add(p) as *mut u32, (written - p) as u32);
        }
        prev_entry = Some(written);
        written += rec_len;
        idx += 1;
        let _ = &mut name;
        if single {
            break;
        }
    }

    if let Some(p) = prev_entry {
        core::ptr::write_unaligned(out.add(p) as *mut u32, 0u32); // fin de chaîne
        (*ccb).dir_cursor = idx;
        complete_irp(irp, 0, written);
        return 0;
    }

    // rien trouvé : pattern sans correspondance au premier appel ?
    let status = if !matched_any && (*ccb).pattern.is_some() {
        nd::st::NO_SUCH_FILE
    } else {
        nd::st::NO_MORE_FILES
    };
    complete_err_raw(irp, status)
}

/// Remplit une entrée de répertoire (layouts ntifs pack(4) x64).
#[allow(clippy::too_many_arguments)]
unsafe fn fill_entry(
    out: *mut u8,
    base: usize,
    class: u32,
    rec_len: usize,
    index: usize,
    st: &ext4_core::fsops::Stat,
    attrs: u32,
    name: &[u16],
    ino: u32,
    bs: u64,
) {
    let w32 = |o: usize, v: u32| core::ptr::write_unaligned(out.add(o) as *mut u32, v);
    let w64 = |o: usize, v: i64| core::ptr::write_unaligned(out.add(o) as *mut i64, v);
    let w16 = |o: usize, v: u16| core::ptr::write_unaligned(out.add(o) as *mut u16, v);
    let w8 = |o: usize, v: u8| *out.add(o) = v;

    if class == nd::FIC_NAMES_INFORMATION {
        w32(base, rec_len as u32);
        w32(base + 4, name.len() as u32 * 2);
        for (i, wc) in name.iter().enumerate() {
            w16(base + 8 + i * 2, *wc);
        }
        return;
    }

    // préfixe commun : Next, Index, Cr, Ac, Wr, Ch, Eof, Alloc, Attr, NameLen
    w32(base, rec_len as u32);
    w32(base + 4, index as u32);
    w64(base + 8, unix_to_nt(st.ctime));
    w64(base + 16, unix_to_nt(st.atime));
    w64(base + 24, unix_to_nt(st.mtime));
    w64(base + 32, unix_to_nt(st.ctime));
    w64(base + 40, st.size as i64);
    w64(base + 48, ((st.size + bs - 1) / bs * bs) as i64);
    w32(base + 56, attrs);
    w32(base + 60, name.len() as u32 * 2);

    match class {
        nd::FIC_DIRECTORY_INFORMATION => {}
        nd::FIC_FULL_DIRECTORY_INFORMATION => {
            w32(base + 64, 0); // EaSize
        }
        nd::FIC_BOTH_DIRECTORY_INFORMATION => {
            w32(base + 64, 0); // EaSize
            w8(base + 68, 0); // ShortNameLength
            for i in 0..12 {
                w16(base + 70 + i * 2, 0); // ShortName[12]
            }
        }
        nd::FIC_ID_FULL_DIRECTORY_INFORMATION => {
            w32(base + 64, 0); // EaSize
            w64(base + 68, ino as i64); // FileId
        }
        nd::FIC_ID_BOTH_DIRECTORY_INFORMATION => {
            w32(base + 64, 0); // EaSize
            w8(base + 68, 0); // ShortNameLength
            for i in 0..12 {
                w16(base + 70 + i * 2, 0); // ShortName[12]
            }
            w64(base + 94, ino as i64); // FileId
        }
        _ => {}
    }

    // nom au bon offset (dir_name_offset(class))
    if let Some(off) = nd::dir_name_offset(class) {
        for (i, wc) in name.iter().enumerate() {
            w16(base + off + i * 2, *wc);
        }
    }
}

#[inline]
fn align8(n: usize) -> usize {
    (n + 7) & !7
}

fn ftype_of(mode: u16) -> u8 {
    match mode & 0xF000 {
        0x4000 => FCB_TYPE_DIR,
        0xA000 => FCB_TYPE_SYMLINK,
        _ => FCB_TYPE_FILE,
    }
}

unsafe fn notify_change(vcb: *mut Vcb, irp: PIRP) -> i32 {
    let sp = (*irp).Tail.Overlay.__bindgen_anon_2.__bindgen_anon_1.CurrentStackLocation;
    let fo = file_object(sp);
    let ccb = fs_context2(fo) as *mut Ccb;
    let fcb = fs_context(fo) as *mut Fcb;
    if ccb.is_null() || fcb.is_null() || (*vcb).notify_sync.is_null() {
        return complete_err_raw(irp, nd::st::INVALID_DEVICE_REQUEST);
    }
    let (_nlen, _nf, filter) = p_notify(sp);
    // WatchTree vient des flags de la stack location (SL_WATCH_TREE)
    let watch_tree: u8 = if sp_flags(sp) & 0x01 != 0 { 1 } else { 0 };

    // nom complet du répertoire surveillé
    let mut name = (*ccb).dir_path.clone();
    if name.is_empty() {
        name.push(0x5C);
    }
    let mut buf = name;
    buf.push(0);
    let len = (buf.len() - 1) * 2;
    let mut us = UNICODE_STRING {
        Length: len as u16,
        MaximumLength: (len + 2) as u16,
        Buffer: Box::into_raw(buf.into_boxed_slice()) as *mut u16,
    };
    FsRtlNotifyFullChangeDirectory(
        (*vcb).notify_sync,
        (&(*fcb).notify_list) as *const _ as *mut c_void,
        ccb as *mut c_void,
        &mut us,
        watch_tree,
        1, // IgnoreFilters
        filter,
        irp,
        null_mut(),
        null_mut(),
    );
    if (*irp).PendingReturned != 0 {
        // IRP mis en attente par FsRtl : marque le slot (SL_PENDING_RETURNED)
        // et retourne STATUS_PENDING
        let sp_cur = (*irp).Tail.Overlay.__bindgen_anon_2.__bindgen_anon_1.CurrentStackLocation;
        let ctl = core::ptr::read((sp_cur as *const u8).add(3));
        core::ptr::write((sp_cur as *mut u8).add(3), ctl | 0x01);
        return 0x0000_0103u32 as i32; // STATUS_PENDING
    }
    // complété immédiatement par FsRtl : relais du statut (sans re-compléter)
    core::ptr::read(core::ptr::addr_of!((*irp).IoStatus.__bindgen_anon_1.Status))
}

// ---------------------------------------------------------------------------
// Notifications helpers (utilisés par fsd_info/fsd_fsctl)
// ---------------------------------------------------------------------------

pub unsafe fn report_removed(vcb: *mut Vcb, dir_path: &[u16], name: &[u16]) {
    report_path_change(vcb, dir_path, name, nd::FILE_ACTION_REMOVED);
}

pub unsafe fn report_added(vcb: *mut Vcb, dir_path: &[u16], name: &[u16]) {
    report_path_change(vcb, dir_path, name, nd::FILE_ACTION_ADDED);
}

pub unsafe fn report_path_change(vcb: *mut Vcb, dir_path: &[u16], name: &[u16], action: u32) {
    if (*vcb).notify_sync.is_null() {
        return;
    }
    let root_fcb = match (*vcb).fcbs.get(&ext4_core::INO_ROOT) {
        Some(f) => core::ptr::from_ref(f.as_ref()),
        None => return,
    };
    // chemin complet : dir_path + "\" + name
    let mut full: Vec<u16> = dir_path.to_vec();
    if !full.is_empty() && *full.last().unwrap() != 0x5C {
        full.push(0x5C);
    }
    full.extend_from_slice(name);
    let dir_bytes = (dir_path.len() + 1) * 2;
    let mut buf = full;
    buf.push(0);
    let len = (buf.len() - 1) * 2;
    let mut us = UNICODE_STRING {
        Length: len as u16,
        MaximumLength: (len + 2) as u16,
        Buffer: Box::into_raw(buf.into_boxed_slice()) as *mut u16,
    };
    FsRtlNotifyFullReportChange(
        (*vcb).notify_sync,
        (&(*root_fcb).notify_list) as *const _ as *mut c_void,
        &mut us,
        dir_bytes as u16,
        core::ptr::null_mut(),
        0,
        0,
        nd::FILE_NOTIFY_CHANGE_NAME,
        action,
    );
}

/// Accès public au buffer d'un IRP (utilisé par fsd_fsctl/fsd_info).
pub unsafe fn irp_buffer_pub(irp: PIRP, len: usize) -> Result<*mut c_void, i32> {
    irp_buffer(irp, len)
}
