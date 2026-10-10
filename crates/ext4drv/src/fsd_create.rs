//! IRP_MJ_CREATE : résolution de chemin, création de fichiers/répertoires,
//! suivi des liens symboliques (STATUS_REPARSE), partage, volume DASD.

use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;
use core::ffi::c_void;
use core::ptr::{addr_of, null_mut};

use ext4_core::inode::FileType;

use crate::fsd_io;
use crate::fsd_types::*;
use crate::ntdefs as nd;
use crate::ntshim::*;
use crate::uni;
use wdk_sys::{
    ntddk::{ExAcquireFastMutex, ExReleaseFastMutex},
    IO_STACK_LOCATION, IO_STATUS_BLOCK, PIRP, UNICODE_STRING,
};

pub unsafe fn dispatch_create(vcb: *mut Vcb, irp: PIRP) -> i32 {
    let sp = (*irp).Tail.Overlay.CurrentStackLocation;
    let fo = file_object(sp);
    let options = p_create_options(sp);
    let _attrs = p_create_attrs(sp);
    let share = p_create_share(sp) as u32;
    let access = p_create_access(sp);
    let sl_flags: u8 = sp_flags(sp);

    let mut us = file_name(fo);
    let name_wide: Vec<u16> = if us.Length > 0 {
        let n = (us.Length as usize) / 2;
        (0..n).map(|i| core::ptr::read(us.Buffer.add(i))).collect()
    } else {
        Vec::new()
    };

    // --- ouverture du volume (DASD) : nom vide ---
    if name_wide.is_empty() {
        let ccb = Box::new(Ccb::new(CCB_TYPE_VOLUME, 0));
        let ccb_ptr = Box::into_raw(ccb);
        set_fs_context(fo, vcb as *mut c_void, ccb_ptr as *mut c_void);
        // pas de Cc pour le volume
        complete_create(irp, fo, nd::st::SUCCESS, options);
        return nd::st::SUCCESS;
    }

    // --- accès refusés d'office sur volume lecture seule ---
    let write_bits = access & (nd::FILE_WRITE_DATA | nd::FILE_APPEND_DATA | nd::DELETE | nd::GENERIC_WRITE | nd::GENERIC_ALL);
    if write_bits != 0 && (*vcb).read_only {
        complete_irp(irp, nd::st::MEDIA_WRITE_PROTECTED, 0);
        return nd::st::MEDIA_WRITE_PROTECTED;
    }

    // --- décode le chemin en composants (UTF-8 pour ext4) ---
    let mut components: Vec<Vec<u8>> = Vec::new();
    for w in name_wide.split(|c| *c == 0x5C /* '\' */) {
        if w.is_empty() {
            continue;
        }
        let bytes = uni::utf16_to_utf8(w);
        if bytes.len() > 255 {
            complete_irp(irp, nd::st::OBJECT_NAME_INVALID, 0);
            return nd::st::OBJECT_NAME_INVALID;
        }
        if bytes == b"." {
            continue;
        }
        if bytes == b".." {
            if components.is_empty() {
                // .. au-dessus de la racine : toléré = reste à la racine
                continue;
            }
            components.pop();
            continue;
        }
        components.push(bytes);
    }

    // --- v2 : syntaxe de flux NT (':' dans le composant final) ---
    // Les composants intermédiaires n'en tolèrent pas ; le composant final
    // "fichier:flux" est découpé (suffixe de type ":$DATA" retiré).
    let mut stream_name8: Vec<u8> = Vec::new();
    if !components.is_empty() {
        for c in &components[..components.len() - 1] {
            if c.contains(&b':') {
                complete_irp(irp, nd::st::OBJECT_NAME_INVALID, 0);
                return nd::st::OBJECT_NAME_INVALID;
            }
        }
        let last = components.last().unwrap();
        if let Some(pos) = last.iter().position(|b| *b == b':') {
            if sl_flags & nd::SL_OPEN_TARGET_DIRECTORY != 0 {
                // cible de renommage portant un flux : non supporté v2
                complete_irp(irp, nd::st::OBJECT_NAME_INVALID, 0);
                return nd::st::OBJECT_NAME_INVALID;
            }
            let base = last[..pos].to_vec();
            let mut rest = last[pos + 1..].to_vec();
            // suffixe de type ":$DATA" (insensible à la casse)
            if rest.len() >= 6 && rest[rest.len() - 6..].eq_ignore_ascii_case(b":$DATA") {
                rest.truncate(rest.len() - 6);
            }
            *components.last_mut().unwrap() = base;
            stream_name8 = rest;
        }
    }

    // SL_OPEN_TARGET_DIRECTORY : ouvre le parent, FileObject->FileName = nom
    // final (utilisé par les renames vers des cibles inexistantes)
    let open_target_dir = sl_flags & nd::SL_OPEN_TARGET_DIRECTORY != 0;
    let (walk, final_name) = if open_target_dir {
        if components.is_empty() {
            complete_irp(irp, nd::st::INVALID_PARAMETER, 0);
            return nd::st::INVALID_PARAMETER;
        }
        let last = components.pop().unwrap();
        (components.clone(), last)
    } else {
        (components.clone(), Vec::new())
    };
    let rewrite_name = if open_target_dir {
        Some(uni::utf8_to_utf16(&final_name))
    } else {
        None
    };

    // --- ouverture ciblée par inode (pas utilisé par Win32) ---
    if options & nd::FILE_OPEN_BY_FILE_ID != 0 {
        complete_irp(irp, nd::st::INVALID_PARAMETER, 0);
        return nd::st::INVALID_PARAMETER;
    }

    // --- marche le chemin ---
    let mut cur_ino = ext4_core::INO_ROOT;
    let mut cur_prefix: Vec<Vec<u8>> = Vec::new(); // composants traversés
    let wlen = walk.len();
    for (i, comp) in walk.iter().enumerate() {
        let parent = match (*vcb).fs.read_inode(cur_ino) {
            Ok(p) => p,
            Err(e) => return complete_err(irp, &e),
        };
        match (*vcb).fs.lookup(&parent, comp) {
            Ok(Some((ino, ft))) => {
                // symlink intermédiaire : reparse
                if ft == FileType::Symlink {
                    return reparse_symlink(vcb, irp, fo, &cur_prefix, comp, cur_ino, false);
                }
                if ft != FileType::Dir && i + 1 < wlen {
                    // composant intermédiaire non-répertoire
                    complete_irp(irp, nd::st::OBJECT_PATH_NOT_FOUND, 0);
                    return nd::st::OBJECT_PATH_NOT_FOUND;
                }
                cur_ino = ino;
                cur_prefix.push(comp.clone());
            }
            Ok(None) => {
                complete_irp(irp, nd::st::OBJECT_PATH_NOT_FOUND, 0);
                return nd::st::OBJECT_PATH_NOT_FOUND;
            }
            Err(e) => return complete_err(irp, &e),
        }
    }

    // --- v2 : flux alternatif — le chemin a résolu le fichier de base ---
    if !stream_name8.is_empty() {
        let base = match (*vcb).fs.stat(cur_ino) {
            Ok(s) => s,
            Err(e) => return complete_err(irp, &e),
        };
        if base.mode & 0xF000 != 0x8000 {
            // pas un fichier régulier (répertoire...) : pas de flux v2
            return complete_err_raw(irp, nd::st::FILE_IS_A_DIRECTORY);
        }
        return crate::fsd_xattr::open_stream(
            vcb,
            irp,
            fo,
            cur_ino,
            &cur_prefix,
            &stream_name8,
            access,
            share,
            options,
        );
    }

    // --- composant final ---
    let disposition = options & 0x00FF_FFFF; // FILE_ disposition bits 24+? voir plus bas
    let disp = (options >> 24) & 0x7; // CreateDisposition sur 3 bits hauts
    let _ = disposition;

    let final_is_dir_req = options & nd::FILE_DIRECTORY_FILE != 0;
    let final_non_dir_req = options & nd::FILE_NON_DIRECTORY_FILE != 0;
    let open_reparse = options & nd::FILE_OPEN_REPARSE_POINT != 0;

    let parent_inode = match (*vcb).fs.read_inode(cur_ino) {
        Ok(p) => p,
        Err(e) => return complete_err(irp, &e),
    };

    if final_name.is_empty() {
        // ouverture d'un répertoire existant (chemin complet dans walk)
        return open_existing(
            vcb, irp, fo, cur_ino, access, share, options, open_reparse, cur_prefix, rewrite_name,
        );
    }

    let existing = match (*vcb).fs.lookup(&parent_inode, &final_name) {
        Ok(x) => x,
        Err(e) => return complete_err(irp, &e),
    };

    match existing {
        Some((ino, ft)) => {
            if ft == FileType::Symlink && !open_reparse {
                return reparse_symlink(vcb, irp, fo, &cur_prefix, &final_name, cur_ino, true);
            }
            // disposition ?
            match disp {
                nd::FILE_CREATE => {
                    complete_irp(irp, nd::st::OBJECT_NAME_COLLISION, 0);
                    return nd::st::OBJECT_NAME_COLLISION;
                }
                nd::FILE_OVERWRITE | nd::FILE_OVERWRITE_IF | nd::FILE_SUPERSEDE => {
                    if ft == FileType::Dir {
                        complete_irp(irp, nd::st::FILE_IS_A_DIRECTORY, 0);
                        return nd::st::FILE_IS_A_DIRECTORY;
                    }
                    if (*vcb).read_only {
                        complete_irp(irp, nd::st::MEDIA_WRITE_PROTECTED, 0);
                        return nd::st::MEDIA_WRITE_PROTECTED;
                    }
                    // truncate
                    if let Err(e) = (*vcb).fs.set_len(ino, 0) {
                        return complete_err(irp, &e);
                    }
                }
                _ => {}
            }
            open_existing(vcb, irp, fo, ino, access, share, options, open_reparse, cur_prefix, rewrite_name)
        }
        None => {
            // création demandée ?
            let create_it = matches!(disp, nd::FILE_CREATE | nd::FILE_OPEN_IF | nd::FILE_OVERWRITE_IF | nd::FILE_SUPERSEDE);
            if !create_it {
                complete_irp(irp, nd::st::OBJECT_NAME_NOT_FOUND, 0);
                return nd::st::OBJECT_NAME_NOT_FOUND;
            }
            if (*vcb).read_only {
                complete_irp(irp, nd::st::MEDIA_WRITE_PROTECTED, 0);
                return nd::st::MEDIA_WRITE_PROTECTED;
            }
            if final_is_dir_req && final_non_dir_req {
                complete_irp(irp, nd::st::INVALID_PARAMETER, 0);
                return nd::st::INVALID_PARAMETER;
            }
            // création
            let name_str = match core::str::from_utf8(&final_name) {
                Ok(s) => s.to_string(),
                Err(_) => String::from_utf8_lossy(&final_name).into_owned(),
            };
            let parent_path = components_to_path(&cur_prefix);
            let result = if final_is_dir_req {
                (*vcb).fs.mkdir(&parent_path, &name_str)
            } else {
                (*vcb).fs.create_file(&parent_path, &name_str)
            };
            let new_ino = match result {
                Ok(ino) => ino,
                Err(e) => return complete_err(irp, &e),
            };
            // notifie le changement au répertoire parent
            report_change(vcb, &cur_prefix, 0, nd::FILE_ACTION_ADDED);
            let ftype = if final_is_dir_req { FCB_TYPE_DIR } else { FCB_TYPE_FILE };
            open_by_ino(vcb, irp, fo, new_ino, ftype, access, share, options, cur_prefix, None)
        }
    }
}
/// Ouvre un inode existant (avec vérifications dir/non-dir).
unsafe fn open_existing(
    vcb: *mut Vcb,
    irp: PIRP,
    fo: wdk_sys::PFILE_OBJECT,
    ino: u32,
    access: u32,
    share: u32,
    options: u32,
    _open_reparse: bool,
    prefix: Vec<Vec<u8>>,
    rewrite_name: Option<Vec<u16>>,
) -> i32 {
    let inode = match (*vcb).fs.read_inode(ino) {
        Ok(i) => i,
        Err(e) => return complete_err(irp, &e),
    };
    let ftype = match inode.file_type() {
        Some(FileType::Dir) => {
            if ino == ext4_core::INO_ROOT { FCB_TYPE_ROOT } else { FCB_TYPE_DIR }
        }
        Some(FileType::Symlink) => FCB_TYPE_SYMLINK,
        _ => FCB_TYPE_FILE,
    };
    let is_dir = ftype == FCB_TYPE_DIR || ftype == FCB_TYPE_ROOT;
    let want_dir = options & nd::FILE_DIRECTORY_FILE != 0;
    let want_file = options & nd::FILE_NON_DIRECTORY_FILE != 0;
    if is_dir && want_file {
        complete_irp(irp, nd::st::FILE_IS_A_DIRECTORY, 0);
        return nd::st::FILE_IS_A_DIRECTORY;
    }
    if !is_dir && want_dir {
        complete_irp(irp, nd::st::NOT_A_DIRECTORY, 0);
        return nd::st::NOT_A_DIRECTORY;
    }
    open_by_ino(vcb, irp, fo, ino, ftype, access, share, options, prefix, rewrite_name)
}

#[allow(clippy::too_many_arguments)]
unsafe fn open_by_ino(
    vcb: *mut Vcb,
    irp: PIRP,
    fo: wdk_sys::PFILE_OBJECT,
    ino: u32,
    ftype: u8,
    access: u32,
    share: u32,
    options: u32,
    prefix: Vec<Vec<u8>>,
    rewrite_name: Option<Vec<u16>>,
) -> i32 {
    let fcb = match get_fcb(vcb, ino, ftype) {
        Ok(f) => f,
        Err(e) => return complete_err_raw(irp, e),
    };
    // refresh des métadonnées (les temps/tailles peuvent avoir changé)
    if let Err(e) = refresh_fcb(vcb, &mut *fcb) {
        return complete_err_raw(irp, e);
    }

    let ccb_type = if ftype == FCB_TYPE_DIR || ftype == FCB_TYPE_ROOT {
        CCB_TYPE_DIR
    } else {
        CCB_TYPE_FILE
    };
    let mut ccb = Box::new(Ccb::new(ccb_type, ino));
    ccb.position = 0;

    // partage minimal : ouverture en suppression-exclusive refusée si le
    // fichier a déjà d'autres ouvreurs sans partage delete
    if access & nd::DELETE != 0
        && share & nd::FILE_SHARE_DELETE == 0
        && (*fcb).open_count > 0
    {
        return complete_err_raw(irp, nd::st::SHARING_VIOLATION);
    }

    // FILE_DELETE_ON_CLOSE
    if options & nd::FILE_DELETE_ON_CLOSE != 0 {
        if (*vcb).read_only {
            return complete_err_raw(irp, nd::st::MEDIA_WRITE_PROTECTED);
        }
        ccb.delete_pending = true;
    }

    // chemins pour FileNameInformation / notifications
    let mut full: Vec<u16> = Vec::new();
    for c in &prefix {
        full.push(0x5C);
        full.extend(uni::utf8_to_utf16(c));
    }
    if let Some(rewrite) = &rewrite_name {
        let mut p = full.clone();
        p.push(0x5C);
        p.extend_from_slice(rewrite);
        ccb.full_path = p;
    } else {
        ccb.full_path = full.clone();
    }
    ccb.dir_path = full;
    let _ = &prefix;

    // SL_OPEN_TARGET_DIRECTORY : FileObject->FileName = composant final
    if let Some(new_name) = rewrite_name {
        let mut buf = new_name;
        buf.push(0);
        let len = (buf.len() - 1) * 2;
        let ptr = Box::into_raw(buf.into_boxed_slice()) as *mut u16;
        // remplace le nom (l'ancien buffer I/O manager est laissé : v1)
        core::ptr::write(core::ptr::addr_of_mut!((*fo).FileName.Buffer), ptr);
        let nus = UNICODE_STRING {
            Length: len as u16,
            MaximumLength: (len + 2) as u16,
            Buffer: ptr,
        };
        core::ptr::write(core::ptr::addr_of_mut!((*fo).FileName), nus);
    }

    let ccb_ptr = Box::into_raw(ccb);
    set_fs_context(fo, fcb as *mut c_void, ccb_ptr as *mut c_void);
    (*fcb).open_count += 1;
    ExAcquireFastMutex(&mut (*vcb).mutex);
    (*vcb).open_file_count += 1;
    ExReleaseFastMutex(&mut (*vcb).mutex);

    complete_create(irp, fo, 0, options);
    0
}

/// Suit un lien symbolique : STATUS_REPARSE avec le chemin NT absolu.
unsafe fn reparse_symlink(
    vcb: *mut Vcb,
    irp: PIRP,
    fo: wdk_sys::PFILE_OBJECT,
    prefix: &[Vec<u8>],
    name: &[u8],
    dir_ino: u32,
    is_final: bool,
) -> i32 {
    let dir_inode = match (*vcb).fs.read_inode(dir_ino) {
        Ok(d) => d,
        Err(e) => return complete_err(irp, &e),
    };
    let ino = match (*vcb).fs.lookup(&dir_inode, name) {
        Ok(Some((i, _))) => i,
        _ => {
            complete_irp(irp, nd::st::OBJECT_NAME_NOT_FOUND, 0);
            return nd::st::OBJECT_NAME_NOT_FOUND;
        }
    };
    let target = match (*vcb).fs.readlink(ino) {
        Ok(t) => t,
        Err(e) => return complete_err(irp, &e),
    };
    let target_str = String::from_utf8_lossy(&target).into_owned();

    let new_name = match resolve_symlink_target(vcb, &target_str, prefix, is_final) {
        Some(n) => n,
        None => {
            complete_irp(irp, nd::st::IO_REPARSE_TAG_NOT_HANDLED, 0);
            return nd::st::IO_REPARSE_TAG_NOT_HANDLED;
        }
    };

    // remplace FileObject->FileName (buffer NUL-terminé, owned par nous)
    // et retourne STATUS_REPARSE : l'I/O manager re-résout depuis la racine
    let mut buf = new_name;
    buf.push(0);
    let len = (buf.len() - 1) * 2;
    let ptr = Box::into_raw(buf.into_boxed_slice()) as *mut u16;
    core::ptr::write(core::ptr::addr_of_mut!((*fo).FileName.Buffer), ptr);
    let nus = UNICODE_STRING {
        Length: len as u16,
        MaximumLength: (len + 2) as u16,
        Buffer: ptr,
    };
    core::ptr::write(core::ptr::addr_of_mut!((*fo).FileName), nus);
    complete_irp(irp, nd::st::REPARSE, 0); // Information = IO_REPARSE (0)
    nd::st::REPARSE
}

/// Construit le chemin NT absolu d'une cible de symlink.
fn resolve_symlink_target(
    vcb: *mut Vcb,
    target: &str,
    prefix: &[Vec<u8>],
    is_final: bool,
) -> Option<Vec<u16>> {
    let letter: Vec<u16> = unsafe {
        match &(*vcb).dos_letter {
            Some(l) => l.clone(),
            None => return None,
        }
    };
    let _ = is_final;
    // cible NT déjà (\??\X:\... ou \Device\...) : la passer telle quelle
    if target.starts_with("\\??\\") || target.starts_with("\\Device\\") {
        return Some(uni::utf8_to_utf16(target.as_bytes()));
    }
    // cible Win32 "C:\..." : la préfixer
    if target.len() >= 3
        && target.as_bytes()[0].is_ascii_alphabetic()
        && target.as_bytes()[1] == b':'
    {
        let mut v = vec![0x5C, 0x3F, 0x3F, 0x5C];
        v.extend(uni::utf8_to_utf16(target.as_bytes()));
        return Some(v);
    }
    if target.starts_with('/') {
        // absolute sur ce volume
        let mut v = letter.clone();
        v.extend(uni::utf8_to_utf16(target.as_bytes()));
        return Some(v);
    }
    // relative : par rapport au répertoire contenant le lien
    let mut v = letter.clone();
    for comp in prefix {
        v.push(0x5C);
        v.extend(uni::utf8_to_utf16(comp));
    }
    v.push(0x5C);
    v.extend(uni::utf8_to_utf16(target.as_bytes()));
    Some(v)
}

fn components_to_path(comps: &[Vec<u8>]) -> alloc::string::String {
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

/// Termine un CREATE (Information = FILE_OPENED).
unsafe fn complete_create(irp: PIRP, _fo: wdk_sys::PFILE_OBJECT, status: i32, _options: u32) {
    complete_irp(irp, status, 1); // FILE_OPENED
}

pub fn complete_err(irp: PIRP, e: &ext4_core::Error) -> i32 {
    let s = nt_from_ext4(e);
    complete_irp(irp, s, 0);
    s
}

pub fn complete_err_raw(irp: PIRP, s: i32) -> i32 {
    complete_irp(irp, s, 0);
    s
}

/// Signale un changement (délègue au moteur de notifications de fsd_io).
pub unsafe fn report_change(vcb: *mut Vcb, prefix: &[Vec<u8>], _dir_ino: u32, action: u32) {
    let mut dir: Vec<u16> = Vec::new();
    for c in prefix {
        dir.push(0x5C);
        dir.extend(uni::utf8_to_utf16(c));
    }
    fsd_io::report_path_change(vcb, &dir, &[], action);
}
