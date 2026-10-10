//! IRP_MJ_QUERY_INFORMATION / SET_INFORMATION / QUERY_VOLUME_INFORMATION.
//! Cœur des interactions Explorateur : propriétés, renommage, suppression,
//! truncature, liens.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::ffi::c_void;
use core::ptr::{addr_of, null_mut};

use crate::fsd::{complete_irp, refresh_fcb};
use crate::fsd_io;
use crate::fsd_io::irp_buffer;
use crate::fsd_types::*;
use crate::fsd_xattr;
use crate::ntdefs as nd;
use crate::ntshim::*;
use crate::uni;
use wdk_sys::{
    ntddk::{ExAcquireFastMutex, ExReleaseFastMutex},
    IO_STATUS_BLOCK, LARGE_INTEGER, PIRP,
};

// ---------------------------------------------------------------------------
// QUERY_INFORMATION
// ---------------------------------------------------------------------------

pub unsafe fn dispatch_query_info(vcb: *mut Vcb, irp: PIRP) -> i32 {
    let sp = (*irp).Tail.Overlay.__bindgen_anon_2.__bindgen_anon_1.CurrentStackLocation;
    let fo = file_object(sp);
    let ccb = fs_context2(fo) as *mut Ccb;
    let fcb = fs_context(fo) as *mut Fcb;
    if ccb.is_null() || fcb.is_null() {
        return complete_err_raw(irp, nd::st::INVALID_DEVICE_REQUEST);
    }
    let (out_len, class) = p_query_file(sp);
    let out_len = out_len as usize;
    let out = match irp_buffer(irp, out_len) {
        Ok(b) if !b.is_null() || out_len == 0 => b as *mut u8,
        _ => return complete_err_raw(irp, nd::st::INVALID_PARAMETER),
    };

    // refresh léger depuis ext4 (tailles/temps à jour) — jamais pour un flux
    // (les métadonnées d'un flux vivent dans le FCB, pas dans l'inode)
    if (*ccb).cctype != CCB_TYPE_VOLUME && (*fcb).stream.is_none() {
        let _ = refresh_fcb(vcb, &mut *fcb);
    }

    let w = |o: usize, b: &[u8]| {
        for (i, byte) in b.iter().enumerate() {
            if o + i < out_len {
                core::ptr::write(out.add(o + i), *byte);
            }
        }
    };
    let put64 = |v: i64| v.to_le_bytes();
    let put32 = |v: u32| v.to_le_bytes();

    match class {
        nd::FIC_BASIC_INFORMATION => {
            if out_len < 40 {
                return complete_err_raw(irp, nd::st::BUFFER_TOO_SMALL);
            }
            w(0, &put64((*fcb).creation));
            w(8, &put64((*fcb).access));
            w(16, &put64((*fcb).write));
            w(24, &put64((*fcb).change));
            w(32, &put32((*fcb).attributes));
            complete_ok(irp, 40)
        }
        nd::FIC_STANDARD_INFORMATION => {
            if out_len < 24 {
                return complete_err_raw(irp, nd::st::BUFFER_TOO_SMALL);
            }
            w(0, &put64((*fcb).alloc_size as i64));
            w(8, &put64((*fcb).size as i64));
            w(16, &put32((*fcb).links));
            w(20, &[if (*ccb).delete_pending { 1 } else { 0 }]);
            w(21, &[if (*fcb).ftype == FCB_TYPE_DIR || (*fcb).ftype == FCB_TYPE_ROOT { 1 } else { 0 }]);
            complete_ok(irp, 24)
        }
        nd::FIC_INTERNAL_INFORMATION => {
            if out_len < 8 {
                return complete_err_raw(irp, nd::st::BUFFER_TOO_SMALL);
            }
            w(0, &put64((*fcb).ino as i64));
            complete_ok(irp, 8)
        }
        nd::FIC_EA_INFORMATION => {
            if out_len < 4 {
                return complete_err_raw(irp, nd::st::BUFFER_TOO_SMALL);
            }
            let total = if (*ccb).cctype == CCB_TYPE_VOLUME {
                0
            } else {
                fsd_xattr::ea_total_size(vcb, (*fcb).ino)
            };
            w(0, &put32(total));
            complete_ok(irp, 4)
        }
        nd::FIC_ACCESS_INFORMATION => {
            if out_len < 4 {
                return complete_err_raw(irp, nd::st::BUFFER_TOO_SMALL);
            }
            w(0, &put32(0x1F01FF)); // FILE_ALL_ACCESS simplifié
            complete_ok(irp, 4)
        }
        nd::FIC_NAME_INFORMATION => {
            let mut path = (*ccb).full_path.clone();
            if path.is_empty() {
                path.push(0x5C);
            }
            let bytes = path.len() * 2;
            if out_len < 8 + bytes {
                // première écriture de la longueur quand même
                if out_len >= 4 {
                    w(0, &put32(bytes as u32));
                }
                return complete_err_raw(irp, 0x8000_0005u32 as i32); // STATUS_BUFFER_OVERFLOW
            }
            w(0, &put32(bytes as u32));
            for (i, wc) in path.iter().enumerate() {
                w(4 + i * 2, &wc.to_le_bytes());
            }
            complete_ok(irp, 4 + bytes)
        }
        nd::FIC_POSITION_INFORMATION => {
            if out_len < 8 {
                return complete_err_raw(irp, nd::st::BUFFER_TOO_SMALL);
            }
            w(0, &put64((*ccb).position as i64));
            complete_ok(irp, 8)
        }
        nd::FIC_MODE_INFORMATION => {
            if out_len < 4 {
                return complete_err_raw(irp, nd::st::BUFFER_TOO_SMALL);
            }
            w(0, &put32(0));
            complete_ok(irp, 4)
        }
        nd::FIC_ATTRIBUTE_TAG_INFORMATION => {
            if out_len < 8 {
                return complete_err_raw(irp, nd::st::BUFFER_TOO_SMALL);
            }
            w(0, &put32((*fcb).attributes));
            w(4, &put32((*fcb).reparse_tag));
            complete_ok(irp, 8)
        }
        nd::FIC_NETWORK_OPEN_INFORMATION => {
            if out_len < 56 {
                return complete_err_raw(irp, nd::st::BUFFER_TOO_SMALL);
            }
            w(0, &put64((*fcb).creation));
            w(8, &put64((*fcb).access));
            w(16, &put64((*fcb).write));
            w(24, &put64((*fcb).change));
            w(32, &put64((*fcb).alloc_size as i64));
            w(40, &put64((*fcb).size as i64));
            w(48, &put32((*fcb).attributes));
            complete_ok(irp, 56)
        }
        nd::FIC_ALL_INFORMATION => {
            let fixed = 96usize; // E4_ALL_INFO sans Name
            if out_len < fixed + 8 {
                return complete_err_raw(irp, nd::st::BUFFER_TOO_SMALL);
            }
            // Basic
            w(0, &put64((*fcb).creation));
            w(8, &put64((*fcb).access));
            w(16, &put64((*fcb).write));
            w(24, &put64((*fcb).change));
            w(32, &put32((*fcb).attributes));
            // Standard @40
            w(40, &put64((*fcb).alloc_size as i64));
            w(48, &put64((*fcb).size as i64));
            w(56, &put32((*fcb).links));
            w(60, &[if (*ccb).delete_pending { 1 } else { 0 }]);
            w(61, &[if (*fcb).ftype == FCB_TYPE_DIR || (*fcb).ftype == FCB_TYPE_ROOT { 1 } else { 0 }]);
            // Internal @64
            w(64, &put64((*fcb).ino as i64));
            // Ea @72, Access @76
            let ea_size = if (*ccb).cctype == CCB_TYPE_VOLUME {
                0
            } else {
                fsd_xattr::ea_total_size(vcb, (*fcb).ino)
            };
            w(72, &put32(ea_size));
            w(76, &put32(0x1F01FF));
            // Position @80
            w(80, &put64((*ccb).position as i64));
            // Mode @88, Alignment @92
            w(88, &put32(0));
            w(92, &put32(0));
            // Name (UNICODE_STRING + buffer) @96
            let mut path = (*ccb).full_path.clone();
            if path.is_empty() {
                path.push(0x5C);
            }
            let nb = path.len() * 2;
            w(96, &put32(nb as u16 as u32)); // Name.Length
            w(100, &put32(nb as u16 as u32)); // Name.MaximumLength
            for (i, wc) in path.iter().enumerate() {
                w(104 + i * 2, &wc.to_le_bytes());
            }
            complete_ok(irp, 104 + nb)
        }
        nd::FIC_STREAM_INFORMATION => query_streams(vcb, irp, fcb, out, out_len),
        _ => complete_err_raw(irp, nd::st::INVALID_PARAMETER),
    }
}

/// FILE_STREAM_INFORMATION (v2) : ::$DATA + les flux user.ADS$* du fichier.
/// Sur un répertoire : aucune entrée (succès, 0 octet).
unsafe fn query_streams(
    vcb: *mut Vcb,
    irp: PIRP,
    fcb: *mut Fcb,
    out: *mut u8,
    out_len: usize,
) -> i32 {
    let mut entries: Vec<(Vec<u16>, i64)> = Vec::new();
    if !(*fcb).stream.is_none() {
        // handle de flux ouvert : la seule entrée est le flux lui-même
        let sname8 = (*fcb).stream.clone().unwrap_or_default();
        entries.push((uni::utf8_to_utf16(&sname8), (*fcb).size as i64));
    } else if (*fcb).ftype == FCB_TYPE_FILE || (*fcb).ftype == FCB_TYPE_SYMLINK {
        entries.push((
            b"::$DATA".iter().map(|b| *b as u16).collect(),
            (*fcb).size as i64,
        ));
        for (name8, size) in fsd_xattr::stream_list(vcb, (*fcb).ino) {
            entries.push((uni::utf8_to_utf16(&name8), size as i64));
        }
    }
    if entries.is_empty() {
        return complete_ok(irp, 0);
    }
    let w32 = |o: usize, v: u32| core::ptr::write_unaligned(out.add(o) as *mut u32, v);
    let w64 = |o: usize, v: i64| core::ptr::write_unaligned(out.add(o) as *mut i64, v);
    let w16 = |o: usize, v: u16| core::ptr::write_unaligned(out.add(o) as *mut u16, v);
    let mut written = 0usize;
    let mut prev: Option<usize> = None;
    for (name, size) in &entries {
        let need = (24 + name.len() * 2 + 7) & !7;
        if written + need > out_len {
            if written == 0 {
                return complete_err_raw(irp, nd::st::BUFFER_OVERFLOW);
            }
            break;
        }
        let base = written;
        w32(base, 0); // NextEntryOffset (chaîné ensuite)
        w32(base + 4, (name.len() * 2) as u32); // StreamNameLength
        w64(base + 8, *size); // StreamSize
        w64(base + 16, *size); // StreamAllocationSize
        for (i, c) in name.iter().enumerate() {
            w16(base + 24 + i * 2, *c);
        }
        if let Some(p) = prev {
            w32(p, (base - p) as u32);
        }
        prev = Some(base);
        written += need;
    }
    complete_ok(irp, written)
}

fn complete_ok(irp: PIRP, info: usize) -> i32 {
    complete_irp(irp, 0, info);
    0
}

pub fn complete_err_raw(irp: PIRP, s: i32) -> i32 {
    complete_irp(irp, s, 0);
    s
}

// ---------------------------------------------------------------------------
// SET_INFORMATION
// ---------------------------------------------------------------------------

pub unsafe fn dispatch_set_info(vcb: *mut Vcb, irp: PIRP) -> i32 {
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
    let (in_len, class, target_fo, advance_only) = p_set_file(sp);
    let in_len = in_len as usize;
    let input = match irp_buffer(irp, in_len) {
        Ok(b) if !b.is_null() || in_len == 0 => b as *const u8,
        _ => return complete_err_raw(irp, nd::st::INVALID_PARAMETER),
    };

    let rd32 = |o: usize| -> u32 {
        if input.is_null() || o + 4 > in_len {
            0
        } else {
            u32::from_le_bytes([*input.add(o), *input.add(o + 1), *input.add(o + 2), *input.add(o + 3)])
        }
    };
    let rd64 = |o: usize| -> i64 {
        if input.is_null() || o + 8 > in_len {
            0
        } else {
            let mut b = [0u8; 8];
            for i in 0..8 {
                b[i] = *input.add(o + i);
            }
            i64::from_le_bytes(b)
        }
    };

    // v2 : flux alternatif — classes gérées côté fsd_xattr (EOF, disposition,
    // position ; le reste est refusé)
    if !(*fcb).stream.is_none() {
        return crate::fsd_xattr::stream_set_info(
            vcb, irp, ccb, fcb, class, input, in_len,
        );
    }

    match class {
        nd::FIC_BASIC_INFORMATION => {
            if in_len < 40 {
                return complete_err_raw(irp, nd::st::INVALID_PARAMETER);
            }
            let cr = rd64(0);
            let ac = rd64(8);
            let wr = rd64(16);
            let ch = rd64(24);
            let attrs = rd32(32);
            ExAcquireFastMutex(&mut (*fcb).lazy_mutex);
            let at = if ac != 0 { Some(nt_to_unix(ac)) } else { None };
            let mt = if wr != 0 { Some(nt_to_unix(wr)) } else { None };
            let ct = if ch != 0 { Some(nt_to_unix(ch)) } else { None };
            if at.is_some() || mt.is_some() || ct.is_some() {
                let ino = (*fcb).ino;
                if let Err(e) = (*vcb).fs.set_times(ino, at, mt, ct) {
                    ExReleaseFastMutex(&mut (*fcb).lazy_mutex);
                    return complete_err(irp, &e);
                }
                // miroir dans le FCB (les propriétés Windows restent à jour)
                if let Some(t) = at {
                    (*fcb).access = ac;
                }
                if let Some(t) = mt {
                    (*fcb).write = wr;
                }
                if let Some(t) = ct {
                    (*fcb).change = ch;
                }
            }
            if cr != 0 {
                (*fcb).creation = cr;
            }
            if attrs != 0 {
                // attributs supportés en écriture : readonly/archive/hidden/system
                (*fcb).attributes = attrs & !(nd::FILE_ATTRIBUTE_DIRECTORY | nd::FILE_ATTRIBUTE_REPARSE_POINT);
                if (*fcb).ftype == FCB_TYPE_DIR || (*fcb).ftype == FCB_TYPE_ROOT {
                    (*fcb).attributes |= nd::FILE_ATTRIBUTE_DIRECTORY;
                }
            }
            ExReleaseFastMutex(&mut (*fcb).lazy_mutex);
            complete_ok(irp, 0)
        }
        nd::FIC_END_OF_FILE_INFORMATION | nd::FIC_ALLOCATION_INFORMATION => {
            let new_size = rd64(0);
            if new_size < 0 {
                return complete_err_raw(irp, nd::st::INVALID_PARAMETER);
            }
            if class == nd::FIC_ALLOCATION_INFORMATION && (new_size as u64) <= (*fcb).size {
                complete_ok(irp, 0) // allocation ≤ taille : no-op
            } else {
                set_file_size(vcb, irp, fo, fcb, ccb, new_size as u64, advance_only)
            }
        }
        nd::FIC_DISPOSITION_INFORMATION => {
            if in_len < 1 {
                return complete_err_raw(irp, nd::st::INVALID_PARAMETER);
            }
            let del = *input != 0;
            if del && (*fcb).ftype == FCB_TYPE_ROOT {
                return complete_err_raw(irp, nd::st::ACCESS_DENIED);
            }
            (*ccb).delete_pending = del;
            complete_ok(irp, 0)
        }
        nd::FIC_DISPOSITION_INFORMATION_EX => {
            let flags = rd32(0);
            if flags & nd::FILE_DISPOSITION_FLAG_DELETE != 0 && (*fcb).ftype == FCB_TYPE_ROOT {
                return complete_err_raw(irp, nd::st::ACCESS_DENIED);
            }
            (*ccb).delete_pending = flags & nd::FILE_DISPOSITION_FLAG_DELETE != 0;
            (*ccb).posix_delete = flags & nd::FILE_DISPOSITION_FLAG_POSIX_SEMANTICS != 0;
            complete_ok(irp, 0)
        }
        nd::FIC_POSITION_INFORMATION => {
            (*ccb).position = rd64(0).max(0) as u64;
            complete_ok(irp, 0)
        }
        nd::FIC_MODE_INFORMATION => complete_ok(irp, 0),
        nd::FIC_RENAME_INFORMATION | nd::FIC_RENAME_INFORMATION_EX => {
            do_rename(vcb, irp, fo, ccb, fcb, input, in_len, target_fo, class == nd::FIC_RENAME_INFORMATION_EX)
        }
        nd::FIC_LINK_INFORMATION => {
            do_link(vcb, irp, fo, ccb, fcb, input, in_len, target_fo)
        }
        _ => complete_err_raw(irp, nd::st::INVALID_PARAMETER),
    }
}

/// SET_END_OF_FILE / SET_ALLOCATION : truncate/extend + Cc.
unsafe fn set_file_size(
    vcb: *mut Vcb,
    irp: PIRP,
    fo: wdk_sys::PFILE_OBJECT,
    fcb: *mut Fcb,
    _ccb: *mut Ccb,
    new_size: u64,
    _advance_only: bool,
) -> i32 {
    if (*fcb).ftype == FCB_TYPE_DIR || (*fcb).ftype == FCB_TYPE_ROOT {
        return complete_err_raw(irp, nd::st::FILE_IS_A_DIRECTORY);
    }
    ExAcquireFastMutex(&mut (*fcb).lazy_mutex);
    // flush les pages puis ajuste (truncate purge les pages au-delà)
    let mut iosb = IO_STATUS_BLOCK::default();
    CcFlushCache(
        &(*fcb).sps as *const _ as *mut c_void,
        null_mut(),
        0,
        &mut iosb as *mut IO_STATUS_BLOCK as *mut c_void,
    );
    ExAcquireFastMutex(&mut (*vcb).mutex);
    let res = (*vcb).fs.set_len((*fcb).ino, new_size);
    ExReleaseFastMutex(&mut (*vcb).mutex);
    if let Err(e) = res {
        ExReleaseFastMutex(&mut (*fcb).lazy_mutex);
        return complete_err(irp, &e);
    }
    (*fcb).size = new_size;
    let bs = (*vcb).fs.block_size();
    (*fcb).alloc_size = (new_size + bs - 1) / bs * bs;
    let mut sizes = CC_FILE_SIZES {
        AllocationSize: (*fcb).alloc_size as i64,
        FileSize: (*fcb).size as i64,
        ValidDataLength: (*fcb).size as i64,
    };
    CcSetFileSizes(fo as *mut c_void, &mut sizes);
    let mut t = LARGE_INTEGER::default();
    KeQuerySystemTime(&mut t as *mut LARGE_INTEGER as *mut i64);
    let now = core::ptr::read(core::ptr::addr_of!(t.QuadPart));
    (*fcb).write = now;
    (*fcb).change = now;
    ExReleaseFastMutex(&mut (*fcb).lazy_mutex);
    complete_ok(irp, 0)
}

/// Renommage / déplacement (FILE_RENAME_INFORMATION[_EX]).
unsafe fn do_rename(
    vcb: *mut Vcb,
    irp: PIRP,
    _fo: wdk_sys::PFILE_OBJECT,
    ccb: *mut Ccb,
    fcb: *mut Fcb,
    input: *const u8,
    in_len: usize,
    target_fo: wdk_sys::PFILE_OBJECT,
    ex: bool,
) -> i32 {
    if in_len < 12 {
        return complete_err_raw(irp, nd::st::INVALID_PARAMETER);
    }
    let replace = u32::from_le_bytes([*input, *input.add(1), *input.add(2), *input.add(3)]);
    let root_dir = u64::from_le_bytes({
        let mut b = [0u8; 8];
        for i in 0..8 {
            b[i] = *input.add(4 + i);
        }
        b
    });
    let name_len = u32::from_le_bytes([*input.add(12), *input.add(13), *input.add(14), *input.add(15)]) as usize;
    if 16 + name_len > in_len || name_len % 2 != 0 {
        return complete_err_raw(irp, nd::st::INVALID_PARAMETER);
    }
    let mut new_name: Vec<u16> = Vec::with_capacity(name_len / 2);
    for i in 0..name_len / 2 {
        let o = 16 + i * 2;
        new_name.push(u16::from_le_bytes([*input.add(o), *input.add(o + 1)]));
    }

    // chemin NT du nouveau nom : relatif à RootDirectory (handle du parent)
    // ou absolu si RootDirectory == 0
    let new_path: Vec<Vec<u8>> = if root_dir != 0 {
        // RootDirectory = FILE_OBJECT du répertoire parent (ouvert par le caller)
        if target_fo.is_null() {
            return complete_err_raw(irp, nd::st::INVALID_PARAMETER);
        }
        let parent_ccb = fs_context2(target_fo) as *mut Ccb;
        let parent_fcb = fs_context(target_fo) as *mut Fcb;
        if parent_ccb.is_null() || parent_fcb.is_null() {
            return complete_err_raw(irp, nd::st::INVALID_PARAMETER);
        }
        if (*parent_fcb).ftype != FCB_TYPE_DIR && (*parent_fcb).ftype != FCB_TYPE_ROOT {
            return complete_err_raw(irp, nd::st::NOT_A_DIRECTORY);
        }
        // reconstruit : chemin du parent + composants du nom
        let mut comps = path_to_components(&(*parent_ccb).full_path);
        comps.extend(split_components(&new_name));
        comps
    } else {
        // absolu volume-relatif : commence par '\'
        split_components(&new_name)
    };
    if new_path.is_empty() {
        return complete_err_raw(irp, nd::st::OBJECT_NAME_INVALID);
    }
    let _ = ex;

    let old_comps = path_to_components(&(*ccb).full_path);
    let old_path = components_path(&old_comps);
    let new_path_str = components_path(&new_path);
    if old_path == new_path_str {
        complete_ok(irp, 0);
        return 0;
    }

    ExAcquireFastMutex(&mut (*vcb).mutex);
    let res = (*vcb).fs.rename(&old_path, &new_path_str, replace != 0);
    // rafraîchit le FCB (l'inode est le même, seule l'entrée change)
    let _ = refresh_fcb(vcb, &mut *fcb);
    ExReleaseFastMutex(&mut (*vcb).mutex);
    if let Err(e) = res {
        return complete_err(irp, &e);
    }
    // le CCB suit le nouveau chemin
    (*ccb).full_path = {
        let mut p = Vec::new();
        for c in &new_path {
            p.push(0x5C);
            p.extend(uni::utf8_to_utf16(c));
        }
        p
    };
    // notifications : répertoires source et destination
    let old_dir_w = components_utf16(&parent_components(&old_comps));
    let old_last_w = uni::utf8_to_utf16(old_comps.last().unwrap_or(&Vec::new()));
    let new_dir_w = components_utf16(&new_path);
    let last_w = uni::utf8_to_utf16(new_path.last().unwrap());
    fsd_io::report_added(vcb, &new_dir_w, &last_w);
    if old_dir_w != new_dir_w {
        fsd_io::report_removed(vcb, &old_dir_w, &old_last_w);
    }
    complete_ok(irp, 0)
}

/// Hard link (FILE_LINK_INFORMATION).
unsafe fn do_link(
    vcb: *mut Vcb,
    irp: PIRP,
    _fo: wdk_sys::PFILE_OBJECT,
    ccb: *mut Ccb,
    fcb: *mut Fcb,
    input: *const u8,
    in_len: usize,
    target_fo: wdk_sys::PFILE_OBJECT,
) -> i32 {
    if in_len < 12 {
        return complete_err_raw(irp, nd::st::INVALID_PARAMETER);
    }
    let root_dir = u64::from_le_bytes({
        let mut b = [0u8; 8];
        for i in 0..8 {
            b[i] = *input.add(4 + i);
        }
        b
    });
    let name_len = u32::from_le_bytes([*input.add(12), *input.add(13), *input.add(14), *input.add(15)]) as usize;
    if 16 + name_len > in_len || name_len % 2 != 0 {
        return complete_err_raw(irp, nd::st::INVALID_PARAMETER);
    }
    let mut new_name: Vec<u16> = Vec::with_capacity(name_len / 2);
    for i in 0..name_len / 2 {
        let o = 16 + i * 2;
        new_name.push(u16::from_le_bytes([*input.add(o), *input.add(o + 1)]));
    }
    let new_comps: Vec<Vec<u8>> = if root_dir != 0 {
        if target_fo.is_null() {
            return complete_err_raw(irp, nd::st::INVALID_PARAMETER);
        }
        let parent_ccb = fs_context2(target_fo) as *mut Ccb;
        let parent_fcb = fs_context(target_fo) as *mut Fcb;
        if parent_ccb.is_null() || parent_fcb.is_null() {
            return complete_err_raw(irp, nd::st::INVALID_PARAMETER);
        }
        let mut comps = path_to_components(&(*parent_ccb).full_path);
        comps.extend(split_components(&new_name));
        comps
    } else {
        split_components(&new_name)
    };
    if new_comps.is_empty() {
        return complete_err_raw(irp, nd::st::OBJECT_NAME_INVALID);
    }
    let target_path = components_path(&path_to_components(&(*ccb).full_path));
    let dir_path = components_path(&parent_components(&new_comps));
    let name = String::from_utf8_lossy(new_comps.last().unwrap()).into_owned();

    ExAcquireFastMutex(&mut (*vcb).mutex);
    let res = (*vcb).fs.link(&target_path, &dir_path, &name);
    let _ = refresh_fcb(vcb, &mut *fcb);
    ExReleaseFastMutex(&mut (*vcb).mutex);
    if let Err(e) = res {
        return complete_err(irp, &e);
    }
    let dir_w = components_utf16(&parent_components(&new_comps));
    fsd_io::report_added(vcb, &dir_w, &uni::utf8_to_utf16(new_comps.last().unwrap()));
    complete_ok(irp, 0)
}

// ---------------------------------------------------------------------------
// VOLUME
// ---------------------------------------------------------------------------

pub unsafe fn dispatch_query_volume(vcb: *mut Vcb, irp: PIRP) -> i32 {
    let sp = (*irp).Tail.Overlay.__bindgen_anon_2.__bindgen_anon_1.CurrentStackLocation;
    let (out_len, class) = p_query_volume(sp);
    let out_len = out_len as usize;
    let out = match irp_buffer(irp, out_len) {
        Ok(b) if !b.is_null() || out_len == 0 => b as *mut u8,
        _ => return complete_err_raw(irp, nd::st::INVALID_PARAMETER),
    };
    let w = |o: usize, b: &[u8]| {
        for (i, byte) in b.iter().enumerate() {
            if o + i < out_len {
                core::ptr::write(out.add(o + i), *byte);
            }
        }
    };
    let put32 = |v: u32| v.to_le_bytes();
    let put64 = |v: i64| v.to_le_bytes();

    match class {
        1 => {
            // FileFsVolumeInformation
            let label = &(*vcb).label;
            let lb = label.len() * 2;
            if out_len < 18 + lb {
                if out_len >= 12 {
                    w(8, &put32(lb as u32));
                }
                return complete_err_raw(irp, 0x8000_0005u32 as i32);
            }
            w(0, &put64(0)); // creation time
            w(8, &put32((*vcb).serial));
            w(12, &put32(lb as u32));
            w(16, &[1]); // SupportsObjects
            for (i, wc) in label.iter().enumerate() {
                w(18 + i * 2, &wc.to_le_bytes());
            }
            complete_ok(irp, 18 + lb)
        }
        3 => {
            // FileFsSizeInformation
            if out_len < 24 {
                return complete_err_raw(irp, nd::st::BUFFER_TOO_SMALL);
            }
            let bs = (*vcb).fs.block_size();
            let total = (*vcb).fs.sb.blocks_count as i64;
            let free = (*vcb).fs.sb.free_blocks_count as i64;
            w(0, &put64(total));
            w(8, &put64(free));
            w(16, &put32(1)); // secteurs par unité d'allocation
            w(20, &put32(bs as u32));
            complete_ok(irp, 24)
        }
        4 => {
            // FileFsDeviceInformation
            if out_len < 8 {
                return complete_err_raw(irp, nd::st::BUFFER_TOO_SMALL);
            }
            w(0, &put32(7)); // FILE_DEVICE_DISK
            w(4, &put32(if (*vcb).read_only { 0x2 } else { 0 })); // FILE_READ_ONLY_DEVICE
            complete_ok(irp, 8)
        }
        5 => {
            // FileFsAttributeInformation
            let name = b"ext4";
            let nb = name.len();
            if out_len < 12 + nb {
                return complete_err_raw(irp, 0x8000_0005u32 as i32);
            }
            let fattrs = 0x1 // FILE_CASE_SENSITIVE_SEARCH
                | 0x2 // FILE_CASE_PRESERVED_NAMES
                | 0x4 // FILE_UNICODE_ON_DISK
                | 0x40 // FILE_SUPPORTS_SPARSE_FILES
                | 0x80 // FILE_SUPPORTS_REPARSE_POINTS
                | 0x400000; // FILE_SUPPORTS_HARD_LINKS
            w(0, &put32(fattrs));
            w(4, &put32(255)); // longueur max composant
            w(8, &put32(nb as u32));
            for (i, b) in name.iter().enumerate() {
                w(12 + i * 2, &(*b as u16).to_le_bytes());
            }
            complete_ok(irp, 12 + nb * 2)
        }
        7 => {
            // FileFsFullSizeInformation
            if out_len < 32 {
                return complete_err_raw(irp, nd::st::BUFFER_TOO_SMALL);
            }
            let bs = (*vcb).fs.block_size();
            let total = (*vcb).fs.sb.blocks_count as i64;
            let free = (*vcb).fs.sb.free_blocks_count as i64;
            w(0, &put64(total));
            w(8, &put64(free));
            w(16, &put64(free));
            w(24, &put32(1));
            w(28, &put32(bs as u32));
            complete_ok(irp, 32)
        }
        _ => complete_err_raw(irp, nd::st::INVALID_PARAMETER),
    }
}

pub unsafe fn dispatch_set_volume(vcb: *mut Vcb, irp: PIRP) -> i32 {
    // changement de label : non supporté v1
    let _ = vcb;
    complete_err_raw(irp, nd::st::ACCESS_DENIED)
}

// ---------------------------------------------------------------------------
// Helpers chemins
// ---------------------------------------------------------------------------

pub fn path_to_components(path: &[u16]) -> Vec<Vec<u8>> {
    let mut comps: Vec<Vec<u8>> = Vec::new();
    for w in path.split(|c| *c == 0x5C) {
        if w.is_empty() {
            continue;
        }
        let bytes = uni::utf16_to_utf8(w);
        if bytes == b"." {
            continue;
        }
        if bytes == b".." {
            comps.pop();
            continue;
        }
        comps.push(bytes);
    }
    comps
}

pub fn split_components(name: &[u16]) -> Vec<Vec<u8>> {
    let mut comps: Vec<Vec<u8>> = Vec::new();
    for w in name.split(|c| *c == 0x5C) {
        if w.is_empty() {
            continue;
        }
        let bytes = uni::utf16_to_utf8(w);
        if bytes == b"." {
            continue;
        }
        if bytes == b".." {
            comps.pop();
            continue;
        }
        comps.push(bytes);
    }
    comps
}

pub fn components_path(comps: &[Vec<u8>]) -> alloc::string::String {
    if comps.is_empty() {
        return "/".into();
    }
    let mut s = alloc::string::String::new();
    for c in comps {
        s.push('/');
        s.push_str(&String::from_utf8_lossy(c));
    }
    s
}

pub fn parent_components(comps: &[Vec<u8>]) -> Vec<Vec<u8>> {
    comps[..comps.len().saturating_sub(1)].to_vec()
}

pub fn components_utf16(comps: &[Vec<u8>]) -> Vec<u16> {
    let mut v = Vec::new();
    for c in comps {
        v.push(0x5C);
        v.extend(uni::utf8_to_utf16(c));
    }
    v
}

pub fn complete_err(irp: PIRP, e: &ext4_core::Error) -> i32 {
    let s = nt_from_ext4(e);
    complete_irp(irp, s, 0);
    s
}
