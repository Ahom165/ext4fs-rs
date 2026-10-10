//! IRP_MJ_CLEANUP / CLOSE / FLUSH / LOCK_CONTROL, IRP_MJ_FILE_SYSTEM_CONTROL
//! sur volume (FSCTL reparse + dismount), IRP_MJ_DEVICE_CONTROL volume.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use core::ffi::c_void;
use core::ptr::{addr_of, addr_of_mut, null_mut};

use crate::fsd::complete_irp;
use crate::fsd_io;
use crate::fsd_info::{complete_err, complete_err_raw};
use crate::fsd_types::*;
use crate::ntdefs as nd;
use crate::ntshim::*;
use crate::uni;
use wdk_sys::{
    ntddk::{ExAcquireFastMutex, ExReleaseFastMutex},
    IO_STATUS_BLOCK, LARGE_INTEGER, PIRP,
};

// ---------------------------------------------------------------------------
// CLEANUP : dernière fermeture logique (delete, flush, notifs)
// ---------------------------------------------------------------------------

pub unsafe fn dispatch_cleanup(vcb: *mut Vcb, irp: PIRP) -> i32 {
    let sp = (*irp).Tail.Overlay.__bindgen_anon_2.__bindgen_anon_1.CurrentStackLocation;
    let fo = file_object(sp);
    let ccb = fs_context2(fo) as *mut Ccb;
    let fcb = fs_context(fo) as *mut Fcb;
    if ccb.is_null() || fcb.is_null() {
        return complete_err_raw(irp, nd::st::INVALID_DEVICE_REQUEST);
    }

    if (*ccb).cctype == CCB_TYPE_VOLUME {
        complete_irp(irp, 0, 0);
        return 0;
    }

    // 0) oplocks : sémantique cleanup (libère les oplocks de ce handle)
    crate::fsd_xattr::oplock_cleanup(fcb, irp);

    // v2 : flux alternatifs — chemin dédié (xattr user.ADS$*, sans Cc)
    if !(*fcb).stream.is_none() {
        return crate::fsd_xattr::stream_cleanup(vcb, irp, fo, ccb, fcb);
    }

    // 1) flush des pages sales
    if !(*fcb).sps.SharedCacheMap.is_null() || !(*fcb).sps.DataSectionObject.is_null() {
        let mut iosb = IO_STATUS_BLOCK::default();
        CcFlushCache(
            &(*fcb).sps as *const _ as *mut c_void,
            null_mut(),
            0,
            &mut iosb as *mut IO_STATUS_BLOCK as *mut c_void,
        );
    }

    // 2) suppression demandée ? uniquement à la DERNIÈRE fermeture
    let is_last = (*fcb).open_count == 1;
    let mut deleted = false;
    if (*ccb).delete_pending && !(*vcb).read_only && is_last {
        let is_dir = (*fcb).ftype == FCB_TYPE_DIR || (*fcb).ftype == FCB_TYPE_ROOT;
        // les exécutables en cours d'exécution ne peuvent pas être supprimés
        if !is_dir && !(*fcb).sps.ImageSectionObject.is_null() {
            let can_delete = MmFlushImageSection(
                &(*fcb).sps as *const _ as *mut c_void,
                MmFlushImageDelete,
            );
            if can_delete == 0 {
                // image mappée : la suppression est annulée
                (*ccb).delete_pending = false;
            }
        }
    }
    if (*ccb).delete_pending && !(*vcb).read_only && is_last {
        // 3) détache le Cc de CE file object (truncate à 0 si suppression)
        if !(*fcb).sps.DataSectionObject.is_null() || !(*fcb).sps.SharedCacheMap.is_null() {
            let mut sz = 0i64;
            CcUninitializeCacheMap(fo as *mut c_void, &mut sz, null_mut());
        }
        // 4) unlink / rmdir via ext4-core
        let path = alloc::string::String::from_utf8_lossy(&uni::utf16_to_utf8(&(*ccb).full_path)).into_owned();
        let dir_w = components_utf16(&parent_comps(&(*ccb).full_path));
        let name_w = last_component(&(*ccb).full_path);
        ExAcquireFastMutex(&mut (*vcb).mutex);
        let res = if (*fcb).ftype == FCB_TYPE_DIR || (*fcb).ftype == FCB_TYPE_ROOT {
            (*vcb).fs.rmdir(&path)
        } else {
            (*vcb).fs.unlink(&path)
        };
        let _ = (*vcb).fs.flush();
        ExReleaseFastMutex(&mut (*vcb).mutex);
        if res.is_ok() {
            deleted = true;
            fsd_io::report_removed(vcb, &dir_w, &name_w);
        }
        // en cas d'échec : le fichier reste
    } else {
        // pas de suppression : détache simplement le Cc
        if !(*fcb).sps.SharedCacheMap.is_null() {
            CcUninitializeCacheMap(fo as *mut c_void, null_mut(), null_mut());
        }
    }

    // 5) notifications (le FCB doit être encore vivant ici)
    if !(*vcb).notify_sync.is_null() && (*fcb).ftype != FCB_TYPE_VOLUME {
        FsRtlNotifyCleanup(
            (*vcb).notify_sync,
            (&(*fcb).notify_list) as *const _ as *mut c_void,
            ccb as *mut c_void,
        );
    }

    // 6) compteurs + purge du FCB si supprimé
    if (*fcb).open_count > 0 {
        (*fcb).open_count -= 1;
    }
    ExAcquireFastMutex(&mut (*vcb).mutex);
    if (*vcb).open_file_count > 0 {
        (*vcb).open_file_count -= 1;
    }
    if deleted {
        if !(*fcb).lock.is_null() {
            FsRtlUninitializeFileLock((*fcb).lock);
        }
        let ino = (*fcb).ino;
        if let Some(b) = (*vcb).fcbs.remove(&ino) {
            drop(b);
        }
    }
    ExReleaseFastMutex(&mut (*vcb).mutex);

    complete_irp(irp, 0, 0);
    0
}

// ---------------------------------------------------------------------------
// CLOSE : libération mémoire du CCB
// ---------------------------------------------------------------------------

pub unsafe fn dispatch_close(vcb: *mut Vcb, irp: PIRP) -> i32 {
    let sp = (*irp).Tail.Overlay.__bindgen_anon_2.__bindgen_anon_1.CurrentStackLocation;
    let fo = file_object(sp);
    let ccb = fs_context2(fo) as *mut Ccb;
    let fcb = fs_context(fo) as *mut Fcb;
    if !ccb.is_null() {
        drop(Box::from_raw(ccb));
        core::ptr::write(addr_of_mut!((*fo).FsContext2), null_mut());
    }
    let _ = (vcb, fcb);
    complete_irp(irp, 0, 0);
    0
}

// ---------------------------------------------------------------------------
// FLUSH_BUFFERS
// ---------------------------------------------------------------------------

pub unsafe fn dispatch_flush(vcb: *mut Vcb, irp: PIRP) -> i32 {
    let sp = (*irp).Tail.Overlay.__bindgen_anon_2.__bindgen_anon_1.CurrentStackLocation;
    let fo = file_object(sp);
    let ccb = fs_context2(fo) as *mut Ccb;
    let fcb = fs_context(fo) as *mut Fcb;
    if !ccb.is_null() && (*ccb).cctype == CCB_TYPE_VOLUME {
        // flush du volume entier
        let _ = crate::fsd::flush_volume(vcb);
        complete_irp(irp, 0, 0);
        return 0;
    }
    if !fcb.is_null() && !(*fcb).sps.DataSectionObject.is_null() {
        let mut iosb = IO_STATUS_BLOCK::default();
        CcFlushCache(
            &(*fcb).sps as *const _ as *mut c_void,
            null_mut(),
            0,
            &mut iosb as *mut IO_STATUS_BLOCK as *mut c_void,
        );
    }
    // synchronise la taille vue Windows avec l'inode ext4
    if !fcb.is_null() && (*fcb).ftype != FCB_TYPE_VOLUME && !(*vcb).read_only {
        let sz = (*fcb).size;
        ExAcquireFastMutex(&mut (*vcb).mutex);
        let res = (*vcb).fs.set_len((*fcb).ino, sz);
        let _ = (*vcb).fs.flush();
        ExReleaseFastMutex(&mut (*vcb).mutex);
        if let Err(e) = res {
            return complete_err(irp, &e);
        }
    }
    complete_irp(irp, 0, 0);
    0
}

// ---------------------------------------------------------------------------
// LOCK_CONTROL (verrous de plage via FsRtl)
// ---------------------------------------------------------------------------

pub unsafe fn dispatch_lock(vcb: *mut Vcb, irp: PIRP) -> i32 {
    let sp = (*irp).Tail.Overlay.__bindgen_anon_2.__bindgen_anon_1.CurrentStackLocation;
    let fo = file_object(sp);
    let fcb = fs_context(fo) as *mut Fcb;
    if fcb.is_null() || (*fcb).lock.is_null() {
        return complete_err_raw(irp, nd::st::LOCK_NOT_GRANTED);
    }
    let _ = vcb;
    let status = FsRtlProcessFileLock((*fcb).lock, irp, null_mut());
    status
}

// ---------------------------------------------------------------------------
// FILE_SYSTEM_CONTROL sur VOLUME (IRP_MN_USER_FS_REQUEST / VERIFY)
// ---------------------------------------------------------------------------

pub unsafe fn dispatch_fsctrl_volume(vcb: *mut Vcb, irp: PIRP) -> i32 {
    let sp = (*irp).Tail.Overlay.__bindgen_anon_2.__bindgen_anon_1.CurrentStackLocation;
    let minor: u8 = core::ptr::read(core::ptr::addr_of!((*sp).MinorFunction));
    match minor {
        nd::MN_USER_FS_REQUEST => {
            let fo = file_object(sp);
            let (_o, _i, code) = p_fsctl(sp);
            let ccb = fs_context2(fo) as *mut Ccb;
            let fcb = fs_context(fo) as *mut Fcb;
            match code {
                nd::FSCTL_IS_VOLUME_MOUNTED => {
                    complete_irp(irp, 0, 0);
                    0
                }
                nd::FSCTL_DISMOUNT_VOLUME => {
                    let res = crate::fsd::dismount(vcb, false);
                    match res {
                        Ok(()) => {
                            complete_irp(irp, 0, 0);
                            0
                        }
                        Err(s) => complete_err_raw(irp, s),
                    }
                }
                nd::FSCTL_GET_REPARSE_POINT => {
                    if ccb.is_null() || fcb.is_null() || (*fcb).reparse_tag == 0 {
                        return complete_err_raw(irp, nd::st::NOT_A_REPARSE_POINT);
                    }
                    get_reparse(vcb, irp, fcb)
                }
                nd::FSCTL_SET_REPARSE_POINT => {
                    if (*vcb).read_only {
                        return complete_err_raw(irp, nd::st::MEDIA_WRITE_PROTECTED);
                    }
                    if ccb.is_null() || fcb.is_null() {
                        return complete_err_raw(irp, nd::st::INVALID_DEVICE_REQUEST);
                    }
                    set_reparse(vcb, irp, ccb, fcb)
                }
                nd::FSCTL_DELETE_REPARSE_POINT => {
                    delete_reparse(vcb, irp, ccb, fcb)
                }
                nd::FSCTL_ALLOW_EXTENDED_DASD_IO => {
                    complete_irp(irp, 0, 0);
                    0
                }
                // oplocks v2 : requêtes + acquittements délégués à FsRtl
                nd::FSCTL_REQUEST_OPLOCK_LEVEL_1
                | nd::FSCTL_REQUEST_OPLOCK_LEVEL_2
                | nd::FSCTL_REQUEST_BATCH_OPLOCK
                | nd::FSCTL_REQUEST_FILTER_OPLOCK
                | nd::FSCTL_OPLOCK_BREAK_ACKNOWLEDGE
                | nd::FSCTL_OPBATCH_ACK_CLOSE_PENDING
                | nd::FSCTL_OPLOCK_BREAK_NOTIFY
                | nd::FSCTL_OPLOCK_BREAK_ACK_NO_2 => {
                    crate::fsd_xattr::oplock_fsctrl(vcb, irp, fcb)
                }
                nd::FSCTL_SET_SPARSE | nd::FSCTL_GET_COMPRESSION => {
                    // traité comme succès no-op (les apps copient des
                    // attributs de compression)
                    if code == nd::FSCTL_GET_COMPRESSION {
                        let (out_len0, _i, _c, _il) = p_userfs(sp);
                        let out_len = out_len0 as usize;
                        if out_len >= 2 {
                            if let Ok(b) = crate::fsd_io::irp_buffer_pub(irp, out_len) {
                                core::ptr::write_unaligned(b as *mut u16, 0u16); // COMPRESSION_FORMAT_NONE
                                complete_irp(irp, 0, 2);
                                return 0;
                            }
                        }
                    }
                    complete_irp(irp, 0, 0);
                    0
                }
                _ => complete_err_raw(irp, nd::st::INVALID_DEVICE_REQUEST),
            }
        }
        nd::MN_VERIFY_VOLUME => {
            complete_irp(irp, 0, 0);
            0
        }
        _ => complete_err_raw(irp, nd::st::INVALID_DEVICE_REQUEST),
    }
}

// ---------------------------------------------------------------------------
// Reparse points (liens symboliques Windows)
// ---------------------------------------------------------------------------

unsafe fn get_reparse(vcb: *mut Vcb, irp: PIRP, fcb: *mut Fcb) -> i32 {
    let sp = (*irp).Tail.Overlay.__bindgen_anon_2.__bindgen_anon_1.CurrentStackLocation;
    let (out_len0, _ib, _c, _il) = p_userfs(sp);
    let out_len = out_len0 as usize;
    if (*fcb).ftype != FCB_TYPE_SYMLINK {
        return complete_err_raw(irp, nd::st::NOT_A_REPARSE_POINT);
    }
    // cible ext4 → substitute NT
    let target = match &(*fcb).target {
        Some(t) => String::from_utf8_lossy(t).into_owned(),
        None => return complete_err_raw(irp, nd::st::NOT_A_REPARSE_POINT),
    };
    let substitute = build_substitute_name(vcb, &target);
    let sub_w = uni::utf8_to_utf16(substitute.as_bytes());
    let print_w = printable_name(&sub_w);
    let mut out = vec![0u8; nd::MAXIMUM_REPARSE_DATA_BUFFER_SIZE];
    let total = match nd::build_symlink_reparse(&mut out, &sub_w, &print_w, false) {
        Some(t) => t,
        None => return complete_err_raw(irp, nd::st::UNSUCCESSFUL),
    };
    if out_len < total {
        return complete_err_raw(irp, nd::st::BUFFER_TOO_SMALL);
    }
    let buf = match crate::fsd_io::irp_buffer_pub(irp, total) {
        Ok(b) => b as *mut u8,
        Err(s) => return complete_err_raw(irp, s),
    };
    core::ptr::copy_nonoverlapping(out.as_ptr(), buf, total);
    complete_irp(irp, 0, total);
    0
}

/// Convertit une cible ext4 en substitute NT (stockée, donc déjà NT si
/// créée depuis Windows ; POSIX si créée depuis Linux → traduction).
fn build_substitute_name(vcb: *mut Vcb, target: &str) -> String {
    if target.starts_with("\\??\\") || target.starts_with("\\Device\\") {
        return target.to_string();
    }
    if target.len() >= 3 && target.as_bytes()[0].is_ascii_alphabetic() && target.as_bytes()[1] == b':' {
        return format!("\\??\\{target}");
    }
    unsafe {
        let letter = match &(*vcb).dos_letter {
            Some(l) => uni::wide_to_string(l),
            None => return target.to_string(),
        };
        if target.starts_with('/') {
            format!("{letter}{target}")
        } else {
            target.to_string() // relatif : conservé brut
        }
    }
}

/// Print name : substitut sans le préfixe \??\.
fn printable_name(sub: &[u16]) -> Vec<u16> {
    if sub.len() >= 4 && sub[0] == 0x5C && sub[1] == 0x3F && sub[2] == 0x3F && sub[3] == 0x5C {
        sub[4..].to_vec()
    } else {
        sub.to_vec()
    }
}

unsafe fn set_reparse(vcb: *mut Vcb, irp: PIRP, ccb: *mut Ccb, fcb: *mut Fcb) -> i32 {
    let sp = (*irp).Tail.Overlay.__bindgen_anon_2.__bindgen_anon_1.CurrentStackLocation;
    let (_o, _ib, _c, in_len0) = p_userfs(sp);
    let in_len = in_len0 as usize;
    let input = match crate::fsd_io::irp_buffer_pub(irp, in_len) {
        Ok(b) if in_len > 0 => b as *const u8,
        _ => return complete_err_raw(irp, nd::st::INVALID_PARAMETER),
    };
    let bytes = core::slice::from_raw_parts(input, in_len);
    let (substitute, relative) = match nd::parse_symlink_reparse(bytes) {
        Some(x) => x,
        None => return complete_err_raw(irp, nd::st::REPARSE_ATTRIBUTE_CONFLICT),
    };
    // le substitute est la cible stockée dans le symlink ext4
    let target_str = uni::wide_to_string(&substitute);
    if target_str.len() >= 59 {
        return complete_err_raw(irp, nd::st::UNSUCCESSFUL); // limite symlink rapide v1
    }
    ExAcquireFastMutex(&mut (*vcb).mutex);
    // on RÉÉCRIT le contenu du symlink existant (l'objet est déjà un
    // symlink créable via CREATE ; si c'est un fichier normal on refuse)
    if (*fcb).ftype != FCB_TYPE_SYMLINK {
        ExReleaseFastMutex(&mut (*vcb).mutex);
        return complete_err_raw(irp, nd::st::REPARSE_ATTRIBUTE_CONFLICT);
    }
    let ino = (*fcb).ino;
    let res = write_symlink_target(vcb, ino, &target_str, relative);
    let _ = (*vcb).fs.flush();
    ExReleaseFastMutex(&mut (*vcb).mutex);
    if let Err(e) = res {
        return complete_err(irp, &e);
    }
    (*fcb).reparse_tag = nd::IO_REPARSE_TAG_SYMLINK;
    (*fcb).target = Some(target_str.into_bytes());
    (*fcb).attributes |= nd::FILE_ATTRIBUTE_REPARSE_POINT;
    complete_irp(irp, 0, 0);
    0
}

/// Réécrit la cible d'un symlink ext4 (i_block, symlink rapide).
unsafe fn write_symlink_target(
    vcb: *mut Vcb,
    ino: u32,
    target: &str,
    _relative: bool,
) -> Result<(), ext4_core::Error> {
    use ext4_core::inode::INLINE_MAX;
    let mut inode = (*vcb).fs.read_inode(ino)?;
    if inode.file_type() != Some(ext4_core::inode::FileType::Symlink) {
        return Err(ext4_core::Error::Corrupt("pas un symlink"));
    }
    let tb = target.as_bytes();
    if tb.len() >= INLINE_MAX {
        return Err(ext4_core::Error::InvalidSize);
    }
    inode.i_block_mut()[..INLINE_MAX].fill(0);
    inode.i_block_mut()[..tb.len()].copy_from_slice(tb);
    inode.set_size(tb.len() as u64);
    (*vcb).fs.write_inode(&mut inode)?;
    (*vcb).fs.commit_transaction()?;
    Ok(())
}

unsafe fn delete_reparse(vcb: *mut Vcb, irp: PIRP, ccb: *mut Ccb, fcb: *mut Fcb) -> i32 {
    let _ = ccb;
    if (*fcb).reparse_tag == 0 {
        return complete_err_raw(irp, nd::st::NOT_A_REPARSE_POINT);
    }
    // v1 : le tag reste (l'objet ext4 reste un symlink)
    complete_irp(irp, 0, 0);
    0
}

// ---------------------------------------------------------------------------
// DEVICE_CONTROL sur VOLUME : relais vers la pile disque
// ---------------------------------------------------------------------------

pub unsafe fn dispatch_volume_devctrl(vcb: *mut Vcb, irp: PIRP) -> i32 {
    let sp = (*irp).Tail.Overlay.__bindgen_anon_2.__bindgen_anon_1.CurrentStackLocation;
    let (out_len0, _ib, code) = p_devctrl(sp);
    // IOCTL_MOUNTDEV_QUERY_DEVICE_NAME : répond depuis notre nom NT
    if code == 0x004D_0008 {
        // CTL_CODE('M', 2, BUFFERED, ANY)
        let out_len = out_len0 as usize;
        let need = 2 + (*vcb).nt_device_name.len() * 2;
        if out_len < need {
            if out_len >= 4 {
                if let Ok(b) = crate::fsd_io::irp_buffer_pub(irp, out_len) {
                    core::ptr::write_unaligned(b as *mut u16, (need - 2) as u16);
                }
            }
            return complete_err_raw(irp, 0x8000_0005u32 as i32); // BUFFER_OVERFLOW
        }
        if let Ok(b) = crate::fsd_io::irp_buffer_pub(irp, need) {
            let p = b as *mut u8;
            core::ptr::write_unaligned(p as *mut u16, ((*vcb).nt_device_name.len() * 2) as u16);
            for (i, wc) in (*vcb).nt_device_name.iter().enumerate() {
                core::ptr::write_unaligned(p.add(2 + i * 2) as *mut u16, *wc);
            }
            complete_irp(irp, 0, need);
            return 0;
        }
        return complete_err_raw(irp, nd::st::INVALID_PARAMETER);
    }
    // le reste : vers la pile du disque
    let target = (*vcb).target_dev as wdk_sys::PDEVICE_OBJECT;
    if target.is_null() {
        return complete_err_raw(irp, nd::st::INVALID_DEVICE_REQUEST);
    }
    skip_current_stack_location(irp);
    IofCallDriver(target as *mut c_void, irp)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn parent_comps(path: &[u16]) -> Vec<Vec<u16>> {
    let mut comps: Vec<Vec<u16>> = Vec::new();
    for w in path.split(|c| *c == 0x5C) {
        if !w.is_empty() {
            comps.push(w.to_vec());
        }
    }
    comps.pop();
    comps
}

fn last_component(path: &[u16]) -> Vec<u16> {
    path.rsplit(|c| *c == 0x5C)
        .next()
        .unwrap_or(&[])
        .to_vec()
}

fn components_utf16(comps: &[Vec<u16>]) -> Vec<u16> {
    let mut v = Vec::new();
    for c in comps {
        v.push(0x5C);
        v.extend_from_slice(c);
    }
    v
}

#[allow(dead_code)]
fn touch_now(fcb: *mut Fcb) {
    unsafe {
        let mut t = LARGE_INTEGER::default();
        KeQuerySystemTime(&mut t as *mut LARGE_INTEGER as *mut i64);
        let now = core::ptr::read(core::ptr::addr_of!(t.QuadPart));
        (*fcb).write = now;
        (*fcb).change = now;
    }
}
