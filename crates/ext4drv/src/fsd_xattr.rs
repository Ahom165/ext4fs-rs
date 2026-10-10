//! Couche v2 du FSD : attributs étendus Windows (IRP_MJ_QUERY_EA/SET_EA),
//! flux alternatifs (ADS), descripteurs de sécurité, oplocks FsRtl et
//! worker de commit périodique.
//!
//! Persistance : tout est stocké en xattrs ext4 via le moteur ext4-core
//! (calibré contre e2fsprogs 1.47.2) :
//!   - EAs Windows           : xattr `user.EA$<nom>`
//!   - flux alternatifs      : xattr `user.ADS$<nom>` (≤ ~1 bloc EA, ~4 Ko)
//!   - descripteur sécurité  : xattr `user.WSD` (SD self-relative brut)
//! Les junctions / points de montage ciblant ce volume sont tolérés sans
//! traitement particulier : l'I/O manager résout les reparse avant nous.
//!
//! Limites v2 (documentées dans docs/DESIGN.md §15) : pas d'EA-inodes
//! (valeur ≤ capacité d'un bloc EA), ADS sans Cc (pas de section mappée),
//! pas de break d'oplock à la réouverture (level 1 casse à l'écriture),
//! descripteurs stockés bruts (pas de SeAccessCheck).

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::ffi::c_void;
use core::ptr::{addr_of, addr_of_mut, null_mut};

use ext4_core::xattr::{self, XIDX_USER};

use crate::fsd::complete_irp;
use crate::fsd_info::{complete_err, complete_err_raw};
use crate::fsd_types::*;
use crate::ntdefs as nd;
use crate::ntshim::*;
use crate::uni;
use wdk_sys::{
    ntddk::{ExAcquireFastMutex, ExReleaseFastMutex},
    FAST_MUTEX, LARGE_INTEGER, PIRP,
};

// ---------------------------------------------------------------------------
// Noms de stockage xattr
// ---------------------------------------------------------------------------

const EA_PREFIX: &[u8] = b"EA$";
const ADS_PREFIX: &[u8] = b"ADS$";
const SD_SHORT: &[u8] = b"WSD";
/// "ADS$" + nom ≤ 255 (limite xattr ext4).
const ADS_MAX_NAME: usize = 255 - ADS_PREFIX.len();

#[inline]
fn align4(n: usize) -> usize {
    (n + 3) & !3
}

fn stream_short_name(name8: &[u8]) -> Vec<u8> {
    let mut v = ADS_PREFIX.to_vec();
    v.extend_from_slice(name8);
    v
}

// ---------------------------------------------------------------------------
// Descripteur de sécurité par défaut : Everyone (S-1-1-0) contrôle total,
// self-relative, 48 octets — utilisé tant qu'aucun SD n'a été posé via
// SET_SECURITY (stocké ensuite dans user.WSD).
// ---------------------------------------------------------------------------

pub fn default_security_descriptor() -> Vec<u8> {
    const SID_EVERYONE: [u8; 12] = [1, 1, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0];
    const FILE_ALL_ACCESS: u32 = 0x000F_01FF;
    let mut v = Vec::with_capacity(48);
    // SECURITY_DESCRIPTOR_RELATIVE : rev 1, sbz1 0, control, 4 offsets u32
    // control = SE_OWNER_DEFAULTED | SE_GROUP_DEFAULTED | SE_DACL_PRESENT
    //         | SE_SELF_RELATIVE = 0x8007
    v.extend_from_slice(&[1u8, 0, 0x07, 0x80]);
    v.extend_from_slice(&0u32.to_le_bytes()); // OffsetOwner (défaut)
    v.extend_from_slice(&0u32.to_le_bytes()); // OffsetGroup (défaut)
    v.extend_from_slice(&0u32.to_le_bytes()); // OffsetSacl (aucun)
    v.extend_from_slice(&20u32.to_le_bytes()); // OffsetDacl @20
    // ACL : rev 2, sbz1, size 28, ace count 1, sbz2
    v.extend_from_slice(&[2u8, 0]);
    v.extend_from_slice(&28u16.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&0u16.to_le_bytes());
    // ACE ACCESS_ALLOWED : type 0, flags 0, size 20, mask, SID
    v.extend_from_slice(&[0u8, 0]);
    v.extend_from_slice(&20u16.to_le_bytes());
    v.extend_from_slice(&FILE_ALL_ACCESS.to_le_bytes());
    v.extend_from_slice(&SID_EVERYONE);
    v
}

// ---------------------------------------------------------------------------
// IRP_MJ_QUERY_EA — FILE_FULL_EA_INFORMATION chaînée depuis user.EA$*
// ---------------------------------------------------------------------------

pub unsafe fn dispatch_query_ea(vcb: *mut Vcb, irp: PIRP) -> i32 {
    let sp = (*irp).Tail.Overlay.__bindgen_anon_2.__bindgen_anon_1.CurrentStackLocation;
    let fo = file_object(sp);
    let ccb = fs_context2(fo) as *mut Ccb;
    let fcb = fs_context(fo) as *mut Fcb;
    if ccb.is_null() || fcb.is_null() {
        return complete_err_raw(irp, nd::st::INVALID_DEVICE_REQUEST);
    }
    if (*ccb).cctype == CCB_TYPE_VOLUME {
        return complete_err_raw(irp, nd::st::INVALID_DEVICE_REQUEST);
    }
    let (len, ea_list, ea_list_len) = p_query_ea(sp);
    let out_len = len as usize;

    // collecte des EAs (xattrs user.EA$*)
    let ino = (*fcb).ino;
    ExAcquireFastMutex(&mut (*vcb).mutex);
    let list = xattr::list_xattrs(&mut (*vcb).fs, ino);
    ExReleaseFastMutex(&mut (*vcb).mutex);
    let mut items: Vec<(Vec<u8>, Vec<u8>)> = match list {
        Ok(l) => {
            let mut it = Vec::new();
            for (e, v) in l {
                if e.name_index == XIDX_USER && e.name.starts_with(EA_PREFIX) {
                    it.push((e.name[EA_PREFIX.len()..].to_vec(), v));
                }
            }
            it
        }
        Err(e) => return complete_err(irp, &e),
    };

    // filtre optionnel : liste FILE_GET_EA_INFORMATION
    if !ea_list.is_null() && ea_list_len >= 5 {
        let raw =
            core::slice::from_raw_parts(ea_list as *const u8, ea_list_len as usize);
        let mut wanted: Vec<Vec<u8>> = Vec::new();
        let mut off = 0usize;
        while off + 5 <= raw.len() {
            let next =
                u32::from_le_bytes([raw[off], raw[off + 1], raw[off + 2], raw[off + 3]])
                    as usize;
            let nlen = raw[off + 4] as usize;
            if nlen == 0 || off + 5 + nlen > raw.len() {
                break;
            }
            wanted.push(raw[off + 5..off + 5 + nlen].to_vec());
            if next == 0 {
                break;
            }
            off += next;
        }
        items.retain(|(n, _)| wanted.iter().any(|w| w == n));
    }

    if items.is_empty() {
        complete_irp(irp, 0, 0);
        return 0;
    }
    let buf = match crate::fsd_io::irp_buffer_pub(irp, out_len) {
        Ok(b) if !b.is_null() => b as *mut u8,
        _ => return complete_err_raw(irp, nd::st::INVALID_PARAMETER),
    };

    // construction des entrées (NextEntryOffset chaîné, aligné 4)
    let mut written = 0usize;
    let mut prev: Option<usize> = None;
    for (name, value) in &items {
        let need = align4(8 + name.len() + value.len());
        if written + need > out_len {
            break;
        }
        let base = written;
        let tail = need - 8 - name.len() - value.len();
        core::ptr::write_unaligned(buf.add(base) as *mut u32, 0u32); // Next
        *buf.add(base + 4) = 0; // Flags
        *buf.add(base + 5) = name.len() as u8; // EaNameLength
        core::ptr::write_unaligned(
            buf.add(base + 6) as *mut u16,
            value.len() as u16, // EaValueLength
        );
        for (i, b) in name.iter().enumerate() {
            *buf.add(base + 8 + i) = *b;
        }
        for (i, b) in value.iter().enumerate() {
            *buf.add(base + 8 + name.len() + i) = *b;
        }
        for b in core::slice::from_raw_parts_mut(
            buf.add(base + 8 + name.len() + value.len()),
            tail,
        ) {
            *b = 0;
        }
        if let Some(p) = prev {
            core::ptr::write_unaligned(buf.add(p) as *mut u32, (base - p) as u32);
        }
        prev = Some(base);
        written = base + need;
    }
    if written == 0 {
        return complete_err_raw(irp, nd::st::BUFFER_OVERFLOW);
    }
    complete_irp(irp, 0, written);
    0
}

// ---------------------------------------------------------------------------
// IRP_MJ_SET_EA — FILE_FULL_EA_INFORMATION chaînée → user.EA$*
// ---------------------------------------------------------------------------

pub unsafe fn dispatch_set_ea(vcb: *mut Vcb, irp: PIRP) -> i32 {
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
        return complete_err_raw(irp, nd::st::INVALID_DEVICE_REQUEST);
    }
    let in_len = p_set_ea(sp) as usize;
    if in_len < 8 {
        return complete_err_raw(irp, nd::st::INVALID_PARAMETER);
    }
    let input = match crate::fsd_io::irp_buffer_pub(irp, in_len) {
        Ok(b) if !b.is_null() => b as *const u8,
        _ => return complete_err_raw(irp, nd::st::INVALID_PARAMETER),
    };
    let raw = core::slice::from_raw_parts(input, in_len);

    let mut off = 0usize;
    let mut applied = false;
    loop {
        if off + 8 > in_len {
            break;
        }
        let next =
            u32::from_le_bytes([raw[off], raw[off + 1], raw[off + 2], raw[off + 3]])
                as usize;
        let nlen = raw[off + 5] as usize;
        let vlen = u16::from_le_bytes([raw[off + 6], raw[off + 7]]) as usize;
        if nlen == 0 || nlen > 254 || off + 8 + nlen + vlen > in_len {
            return complete_err_raw(irp, nd::st::INVALID_PARAMETER);
        }
        let name = &raw[off + 8..off + 8 + nlen];
        let value = &raw[off + 8 + nlen..off + 8 + nlen + vlen];
        if name.contains(&b':') || name.contains(&b'/') || name.contains(&b'\\') {
            return complete_err_raw(irp, nd::st::INVALID_PARAMETER);
        }
        let mut full = EA_PREFIX.to_vec();
        full.extend_from_slice(name);
        ExAcquireFastMutex(&mut (*vcb).mutex);
        let res = xattr::set_xattr(&mut (*vcb).fs, (*fcb).ino, XIDX_USER, &full, value);
        ExReleaseFastMutex(&mut (*vcb).mutex);
        if let Err(e) = res {
            return complete_err(irp, &e);
        }
        applied = true;
        if next == 0 {
            break;
        }
        off += next;
    }
    if applied {
        touch_change(fcb);
        notify_attr_change(vcb, ccb, fcb, nd::FILE_NOTIFY_CHANGE_EA);
    }
    complete_irp(irp, 0, 0);
    0
}

// ---------------------------------------------------------------------------
// IRP_MJ_QUERY_SECURITY / SET_SECURITY — SD self-relative brut (user.WSD)
// ---------------------------------------------------------------------------

pub unsafe fn dispatch_query_security(vcb: *mut Vcb, irp: PIRP) -> i32 {
    let sp = (*irp).Tail.Overlay.__bindgen_anon_2.__bindgen_anon_1.CurrentStackLocation;
    let fo = file_object(sp);
    let ccb = fs_context2(fo) as *mut Ccb;
    let fcb = fs_context(fo) as *mut Fcb;
    if ccb.is_null() || fcb.is_null() {
        return complete_err_raw(irp, nd::st::INVALID_DEVICE_REQUEST);
    }
    let (_info, len, sd_out, ret_len) = p_query_sec(sp);
    if sd_out.is_null() || ret_len.is_null() {
        return complete_err_raw(irp, nd::st::INVALID_PARAMETER);
    }
    let ino = (*fcb).ino;
    ExAcquireFastMutex(&mut (*vcb).mutex);
    let stored = xattr::get_xattr(&mut (*vcb).fs, ino, XIDX_USER, SD_SHORT);
    ExReleaseFastMutex(&mut (*vcb).mutex);
    let sd = match stored {
        Ok(Some(v)) if v.len() >= 20 && v[0] == 1 => v,
        _ => default_security_descriptor(),
    };
    core::ptr::write_unaligned(ret_len, sd.len() as u32);
    if (len as usize) < sd.len() {
        return complete_err_raw(irp, nd::st::BUFFER_OVERFLOW);
    }
    core::ptr::copy_nonoverlapping(sd.as_ptr(), sd_out as *mut u8, sd.len());
    complete_irp(irp, 0, sd.len());
    0
}

pub unsafe fn dispatch_set_security(vcb: *mut Vcb, irp: PIRP) -> i32 {
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
        return complete_err_raw(irp, nd::st::INVALID_DEVICE_REQUEST);
    }
    let (_info, sd_in, len) = p_set_sec(sp);
    if sd_in.is_null() || len < 20 || len > 65_536 {
        return complete_err_raw(irp, nd::st::INVALID_PARAMETER);
    }
    let raw = core::slice::from_raw_parts(sd_in as *const u8, len as usize);
    if raw[0] != 1 {
        // SECURITY_DESCRIPTOR_REVISION attendu
        return complete_err_raw(irp, nd::st::INVALID_PARAMETER);
    }
    let bytes = raw.to_vec();
    let ino = (*fcb).ino;
    ExAcquireFastMutex(&mut (*vcb).mutex);
    let res = xattr::set_xattr(&mut (*vcb).fs, ino, XIDX_USER, SD_SHORT, &bytes);
    ExReleaseFastMutex(&mut (*vcb).mutex);
    if let Err(e) = res {
        return complete_err(irp, &e);
    }
    notify_attr_change(vcb, ccb, fcb, nd::FILE_NOTIFY_CHANGE_SECURITY);
    complete_irp(irp, 0, 0);
    0
}

// ---------------------------------------------------------------------------
// Oplocks (package FsRtl)
// ---------------------------------------------------------------------------

/// Oplock d'un FCB, initialisé paresseusement.
pub unsafe fn ensure_oplock(fcb: *mut Fcb) -> *mut c_void {
    if (*fcb).oplock.is_null() {
        FsRtlInitializeOplock(addr_of_mut!((*fcb).oplock) as *mut c_void);
    }
    (*fcb).oplock
}

pub unsafe fn uninitialize_oplock(fcb: *mut Fcb) {
    if !(*fcb).oplock.is_null() {
        FsRtlUninitializeOplock((*fcb).oplock);
        (*fcb).oplock = null_mut();
    }
}

/// Cassure d'oplock avant écriture. 0 = continuer le traitement normal ;
/// PENDING = IRP repris par le package oplock (ne pas le compléter) ;
/// autre statut = IRP déjà complété avec l'erreur correspondante.
pub unsafe fn oplock_write_check(irp: PIRP, fcb: *mut Fcb) -> i32 {
    if (*fcb).oplock.is_null() {
        return 0;
    }
    let st = FsRtlCheckOplock((*fcb).oplock, irp, null_mut(), null_mut(), null_mut());
    if st == nd::st::PENDING {
        mark_pending(irp);
        return nd::st::PENDING;
    }
    if st == 0 || st == nd::st::OPLOCK_BREAK_IN_PROGRESS {
        0
    } else if st < 0 {
        complete_err_raw(irp, st)
    } else {
        0
    }
}

/// Sémantique cleanup : libère les oplocks portés par ce handle.
pub unsafe fn oplock_cleanup(fcb: *mut Fcb, irp: PIRP) {
    if !(*fcb).oplock.is_null() {
        let _ = FsRtlCheckOplock((*fcb).oplock, irp, null_mut(), null_mut(), null_mut());
    }
}

/// FSCTL oplock (LEVEL_1/2, BATCH, FILTER + acquittements) : FsRtlOplockFsctrl
/// complète l'IRP elle-même et retourne le statut (OPLOCK_NOT_GRANTED inclus).
pub unsafe fn oplock_fsctrl(vcb: *mut Vcb, irp: PIRP, fcb: *mut Fcb) -> i32 {
    let _ = vcb;
    if fcb.is_null() {
        return complete_err_raw(irp, nd::st::INVALID_DEVICE_REQUEST);
    }
    let op = ensure_oplock(fcb);
    FsRtlOplockFsctrl(op, irp, 1)
}

// ---------------------------------------------------------------------------
// Worker de commit périodique (thread système, 5 s)
// ---------------------------------------------------------------------------

/// KEVENT (DISPATCHER_HEADER 24 o, aligné 8) + état du worker.
static mut WORKER_EVENT: (u64, u64, u64) = (0, 0, 0);
static mut WORKER_HANDLE: *mut c_void = null_mut();
static mut WORKER_ACTIVE: bool = false;
static mut WORKER_STOP: bool = false;

const THREAD_ALL_ACCESS: u32 = 0x001F_03FF;
const EVENT_NOTIFICATION: i32 = 0;
const WAIT_EXECUTIVE: i32 = 0;

/// Démarre le worker global (un seul thread pour tous les volumes).
pub fn start_commit_worker() -> i32 {
    unsafe {
        if WORKER_ACTIVE {
            return 0;
        }
        KeInitializeEvent(
            addr_of_mut!(WORKER_EVENT) as *mut c_void,
            EVENT_NOTIFICATION,
            0,
        );
        WORKER_STOP = false;
        let mut h: *mut c_void = null_mut();
        let st = PsCreateSystemThread(
            &mut h,
            THREAD_ALL_ACCESS,
            null_mut(),
            null_mut(),
            null_mut(),
            worker_routine,
            null_mut(),
        );
        if st != 0 {
            return st;
        }
        WORKER_HANDLE = h;
        WORKER_ACTIVE = true;
        0
    }
}

/// Arrête le worker (déchargement) : signale, attend jusqu'à 6 s.
pub unsafe fn stop_commit_worker() {
    if !WORKER_ACTIVE {
        return;
    }
    core::ptr::write(addr_of_mut!(WORKER_STOP), true);
    KeSetEvent(addr_of_mut!(WORKER_EVENT) as *mut c_void, 0, 0);
    let h = core::ptr::read(addr_of!(WORKER_HANDLE));
    if !h.is_null() {
        let mut to = LARGE_INTEGER::default();
        to.QuadPart = -60_000_000; // 6 s max (100 ns units, négatif = relatif)
        let _ = ZwWaitForSingleObject(h, 0, &mut to);
        wdk_sys::ntddk::ZwClose(h);
        core::ptr::write(addr_of_mut!(WORKER_HANDLE), null_mut());
    }
    core::ptr::write(addr_of_mut!(WORKER_ACTIVE), false);
}

unsafe extern "system" fn worker_routine(_ctx: *mut c_void) {
    loop {
        let mut to = LARGE_INTEGER::default();
        to.QuadPart = -50_000_000; // 5 s
        let st = KeWaitForSingleObject(
            addr_of!(WORKER_EVENT) as *mut c_void,
            WAIT_EXECUTIVE,
            KernelMode,
            0,
            &mut to,
        );
        let stop = core::ptr::read(core::ptr::addr_of!(WORKER_STOP));
        if stop || st == 0 {
            break;
        }
        worker_flush_pass();
    }
    PsTerminateSystemThread(0);
}

/// Une passe : flush Cc + journal de chaque volume actif.
unsafe fn worker_flush_pass() {
    let dev = crate::fsd::fs_device();
    if dev.is_null() {
        return;
    }
    let ext = (*dev).DeviceExtension as *mut crate::fsd::FsDevExt;
    if ext.is_null() {
        return;
    }
    ExAcquireFastMutex(&mut (*ext).mutex);
    let vols: Vec<*mut Vcb> = (*ext).volumes.clone();
    ExReleaseFastMutex(&mut (*ext).mutex);
    for v in vols {
        if (*v).flags & VCB_FLAG_DISMOUNTED != 0 {
            continue;
        }
        let _ = crate::fsd::flush_volume(v);
    }
}

// ---------------------------------------------------------------------------
// Flux alternatifs (ADS) — contenu en xattr user.ADS$<nom>, sans Cc
// (SectionObjectPointers laissés nuls : aucune section mappable, donc aucun
// paging IO). Taille maximale : capacité d'un bloc EA (~bloc - 36 o).
// ---------------------------------------------------------------------------

/// Ouverture/création d'un flux : `fichier:flux` (déjà découpé par
/// fsd_create). Retourne après complétion de l'IRP.
pub unsafe fn open_stream(
    vcb: *mut Vcb,
    irp: PIRP,
    fo: wdk_sys::PFILE_OBJECT,
    ino: u32,
    prefix: &[Vec<u8>],
    stream_name: &[u8],
    access: u32,
    share: u32,
    options: u32,
) -> i32 {
    if stream_name.is_empty() || stream_name.len() > ADS_MAX_NAME {
        return complete_err_raw(irp, nd::st::OBJECT_NAME_INVALID);
    }
    let write_bits = access
        & (nd::FILE_WRITE_DATA
            | nd::FILE_APPEND_DATA
            | nd::DELETE
            | nd::GENERIC_WRITE
            | nd::GENERIC_ALL);
    if write_bits != 0 && (*vcb).read_only {
        return complete_err_raw(irp, nd::st::MEDIA_WRITE_PROTECTED);
    }
    let disp = (options >> 24) & 0x7;
    let short = stream_short_name(stream_name);

    ExAcquireFastMutex(&mut (*vcb).mutex);
    let cur = xattr::get_xattr(&mut (*vcb).fs, ino, XIDX_USER, &short);
    if let Err(e) = cur {
        ExReleaseFastMutex(&mut (*vcb).mutex);
        return complete_err(irp, &e);
    }
    let cur = cur.unwrap_or_default(); // Option<Vec<u8>> (Err déjà traité ci-dessus)
    let exists = cur.is_some();
    let mut value = cur.unwrap_or_default();
    match disp {
        nd::FILE_CREATE if exists => {
            ExReleaseFastMutex(&mut (*vcb).mutex);
            return complete_err_raw(irp, nd::st::OBJECT_NAME_COLLISION);
        }
        nd::FILE_OPEN if !exists => {
            ExReleaseFastMutex(&mut (*vcb).mutex);
            return complete_err_raw(irp, nd::st::OBJECT_NAME_NOT_FOUND);
        }
        nd::FILE_CREATE => {
            // matérialise le flux créé (xattr vide) pour que FILE_OPEN le
            // trouve ensuite
            let res = xattr::set_xattr(&mut (*vcb).fs, ino, XIDX_USER, &short, &[]);
            if let Err(e) = res {
                ExReleaseFastMutex(&mut (*vcb).mutex);
                return complete_err(irp, &e);
            }
            value = Vec::new();
        }
        nd::FILE_OVERWRITE | nd::FILE_OVERWRITE_IF | nd::FILE_SUPERSEDE => {
            let res = xattr::set_xattr(&mut (*vcb).fs, ino, XIDX_USER, &short, &[]);
            if let Err(e) = res {
                ExReleaseFastMutex(&mut (*vcb).mutex);
                return complete_err(irp, &e);
            }
            value = Vec::new();
        }
        nd::FILE_OPEN_IF if !exists => {
            let res = xattr::set_xattr(&mut (*vcb).fs, ino, XIDX_USER, &short, &[]);
            if let Err(e) = res {
                ExReleaseFastMutex(&mut (*vcb).mutex);
                return complete_err(irp, &e);
            }
            value = Vec::new();
        }
        _ => {}
    }
    let fsize = value.len() as u64;

    // FCB de flux : réutilisé si déjà ouvert (sinon créé), purge au cleanup
    let key = (ino, stream_name.to_vec());
    let fcb_ptr: *mut Fcb = match (*vcb).streams.get_mut(&key) {
        Some(f) => {
            let p = core::ptr::from_mut(f.as_mut());
            if access & nd::DELETE != 0
                && share & nd::FILE_SHARE_DELETE == 0
                && (*p).open_count > 0
            {
                ExReleaseFastMutex(&mut (*vcb).mutex);
                return complete_err_raw(irp, nd::st::SHARING_VIOLATION);
            }
            (*p).size = fsize;
            (*p).alloc_size = fsize;
            p
        }
        None => {
            let mut f = Box::new(Fcb {
                ino,
                ftype: FCB_TYPE_FILE,
                size: fsize,
                alloc_size: fsize,
                attributes: nd::FILE_ATTRIBUTE_ARCHIVE,
                links: 1,
                creation: 0,
                access: 0,
                write: 0,
                change: 0,
                reparse_tag: 0,
                lazy_mutex: FAST_MUTEX::default(),
                sps: E4SectionObjectPointers {
                    DataSectionObject: null_mut(),
                    SharedCacheMap: null_mut(),
                    ImageSectionObject: null_mut(),
                },
                notify_list: [0; 2],
                vcb: vcb as *mut c_void,
                target: None,
                lock: FsRtlAllocateFileLock(null_mut(), null_mut()),
                oplock: null_mut(),
                stream: Some(stream_name.to_vec()),
                open_count: 0,
            });
            init_fast_mutex(&mut f.lazy_mutex);
            init_list_head(&mut f.notify_list as *mut [u64; 2]);
            let p = core::ptr::from_mut(f.as_mut());
            (*vcb).streams.insert(key, f);
            p
        }
    };
    ExReleaseFastMutex(&mut (*vcb).mutex);

    // CCB
    let mut ccb = Box::new(Ccb::new(CCB_TYPE_FILE, ino));
    let mut full: Vec<u16> = Vec::new();
    for c in prefix {
        full.push(0x5C);
        full.extend(uni::utf8_to_utf16(c));
    }
    full.push(0x3A); // ':'
    full.extend(uni::utf8_to_utf16(stream_name));
    ccb.full_path = full;
    if options & nd::FILE_DELETE_ON_CLOSE != 0 {
        ccb.delete_pending = true;
    }
    let ccb_ptr = Box::into_raw(ccb);
    set_fs_context(fo, fcb_ptr as *mut c_void, ccb_ptr as *mut c_void);
    (*fcb_ptr).open_count += 1;
    ExAcquireFastMutex(&mut (*vcb).mutex);
    (*vcb).open_file_count += 1;
    ExReleaseFastMutex(&mut (*vcb).mutex);

    complete_irp(irp, 0, 1); // FILE_OPENED
    0
}

/// Valeur "flux inexistant" : xattr absente (le moteur retourne None).
/// (conservée pour lisibilité — l'existence est testée via cur.is_some())
#[allow(dead_code)]
fn missing(value: &Option<Vec<u8>>) -> bool {
    value.is_none()
}

/// Lecture d'un flux : tranche de la valeur xattr (zéros au-delà).
pub unsafe fn stream_read(vcb: *mut Vcb, irp: PIRP, fcb: *mut Fcb) -> i32 {
    let sp = (*irp).Tail.Overlay.__bindgen_anon_2.__bindgen_anon_1.CurrentStackLocation;
    let (len, offset) = p_read(sp);
    let fsize = (*fcb).size;
    if offset < 0 || (offset as u64) >= fsize || len == 0 {
        complete_irp(irp, 0, 0);
        return 0;
    }
    let buf = match crate::fsd_io::irp_buffer_pub(irp, len as usize) {
        Ok(b) => b,
        Err(s) => return complete_err_raw(irp, s),
    };
    let out_len = (len as u64).min(fsize - offset as u64) as usize;
    let sname = match &(*fcb).stream {
        Some(s) => s.clone(),
        None => return complete_err_raw(irp, nd::st::INTERNAL_ERROR),
    };
    let short = stream_short_name(&sname);
    ExAcquireFastMutex(&mut (*vcb).mutex);
    let res = xattr::get_xattr(&mut (*vcb).fs, (*fcb).ino, XIDX_USER, &short);
    ExReleaseFastMutex(&mut (*vcb).mutex);
    match res {
        Ok(Some(v)) => {
            let start = offset as usize;
            let out = core::slice::from_raw_parts_mut(buf as *mut u8, out_len);
            let n = (start..v.len().min(start + out_len)).count();
            if n > 0 {
                out[..n].copy_from_slice(&v[start..start + n]);
            }
            if n < out_len {
                out[n..].fill(0);
            }
            complete_irp(irp, 0, out_len);
            0
        }
        Ok(None) => {
            // flux supprimé sous nos pieds : lecture vide
            complete_irp(irp, 0, 0);
            0
        }
        Err(e) => complete_err(irp, &e),
    }
}

/// Écriture d'un flux : réécriture xattr entière (≤ ~4 Ko par design).
pub unsafe fn stream_write(vcb: *mut Vcb, irp: PIRP, fcb: *mut Fcb) -> i32 {
    if (*vcb).read_only {
        return complete_err_raw(irp, nd::st::MEDIA_WRITE_PROTECTED);
    }
    let sp = (*irp).Tail.Overlay.__bindgen_anon_2.__bindgen_anon_1.CurrentStackLocation;
    let (len, offset) = p_read(sp);
    if len == 0 {
        complete_irp(irp, 0, 0);
        return 0;
    }
    if offset < 0 {
        return complete_err_raw(irp, nd::st::INVALID_PARAMETER);
    }
    let buf = match crate::fsd_io::irp_buffer_pub(irp, len as usize) {
        Ok(b) => b,
        Err(s) => return complete_err_raw(irp, s),
    };
    let sname = match &(*fcb).stream {
        Some(s) => s.clone(),
        None => return complete_err_raw(irp, nd::st::INTERNAL_ERROR),
    };
    let short = stream_short_name(&sname);
    ExAcquireFastMutex(&mut (*vcb).mutex);
    let cur = xattr::get_xattr(&mut (*vcb).fs, (*fcb).ino, XIDX_USER, &short);
    if let Err(e) = cur {
        ExReleaseFastMutex(&mut (*vcb).mutex);
        return complete_err(irp, &e);
    }
    // Err déjà traité ci-dessus : Option<Vec<u8>> → Vec<u8>
    let mut value = cur.unwrap_or_default().unwrap_or_default();
    let end = offset as usize + len as usize;
    if value.len() < end {
        value.resize(end, 0);
    }
    core::ptr::copy_nonoverlapping(
        buf as *const u8,
        value.as_mut_ptr().add(offset as usize),
        len as usize,
    );
    let res = xattr::set_xattr(&mut (*vcb).fs, (*fcb).ino, XIDX_USER, &short, &value);
    ExReleaseFastMutex(&mut (*vcb).mutex);
    match res {
        Ok(()) => {
            ExAcquireFastMutex(&mut (*fcb).lazy_mutex);
            (*fcb).size = end as u64;
            (*fcb).alloc_size = (*fcb).size;
            ExReleaseFastMutex(&mut (*fcb).lazy_mutex);
            touch_change(fcb);
            complete_irp(irp, 0, len as usize);
            0
        }
        Err(e) => complete_err(irp, &e),
    }
}

/// CLEANUP d'un flux : suppression demandée, purge du FCB à la dernière
/// fermeture. Complète l'IRP.
pub unsafe fn stream_cleanup(
    vcb: *mut Vcb,
    irp: PIRP,
    _fo: wdk_sys::PFILE_OBJECT,
    ccb: *mut Ccb,
    fcb: *mut Fcb,
) -> i32 {
    let is_last = (*fcb).open_count == 1;
    let mut deleted = false;
    if (*ccb).delete_pending && !(*vcb).read_only && is_last {
        let sname = match &(*fcb).stream {
            Some(s) => s.clone(),
            None => Vec::new(),
        };
        let short = stream_short_name(&sname);
        ExAcquireFastMutex(&mut (*vcb).mutex);
        let res = xattr::remove_xattr(&mut (*vcb).fs, (*fcb).ino, XIDX_USER, &short);
        let _ = (*vcb).fs.flush();
        ExReleaseFastMutex(&mut (*vcb).mutex);
        deleted = res.is_ok();
    }
    if (*fcb).open_count > 0 {
        (*fcb).open_count -= 1;
    }
    ExAcquireFastMutex(&mut (*vcb).mutex);
    if (*vcb).open_file_count > 0 {
        (*vcb).open_file_count -= 1;
    }
    if (*fcb).open_count == 0 {
        if !(*fcb).lock.is_null() {
            FsRtlUninitializeFileLock((*fcb).lock);
            (*fcb).lock = null_mut();
        }
        uninitialize_oplock(fcb);
        let key = ((*fcb).ino, (*fcb).stream.clone().unwrap_or_default());
        if let Some(b) = (*vcb).streams.remove(&key) {
            drop(b);
        }
    }
    ExReleaseFastMutex(&mut (*vcb).mutex);
    let _ = deleted;
    complete_irp(irp, 0, 0);
    0
}

/// SET_INFORMATION sur un flux : EOF/allocation (redimension), disposition,
/// position ; le reste est refusé (renommage de flux non supporté v2).
pub unsafe fn stream_set_info(
    vcb: *mut Vcb,
    irp: PIRP,
    ccb: *mut Ccb,
    fcb: *mut Fcb,
    class: u32,
    input: *const u8,
    in_len: usize,
) -> i32 {
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
    let rd32 = |o: usize| -> u32 {
        if input.is_null() || o + 4 > in_len {
            0
        } else {
            u32::from_le_bytes([
                *input.add(o),
                *input.add(o + 1),
                *input.add(o + 2),
                *input.add(o + 3),
            ])
        }
    };
    match class {
        nd::FIC_END_OF_FILE_INFORMATION | nd::FIC_ALLOCATION_INFORMATION => {
            let new_size = rd64(0);
            if new_size < 0 {
                return complete_err_raw(irp, nd::st::INVALID_PARAMETER);
            }
            if class == nd::FIC_ALLOCATION_INFORMATION && (new_size as u64) <= (*fcb).size
            {
                complete_irp(irp, 0, 0);
                return 0;
            }
            let sname = match &(*fcb).stream {
                Some(s) => s.clone(),
                None => return complete_err_raw(irp, nd::st::INTERNAL_ERROR),
            };
            let short = stream_short_name(&sname);
            ExAcquireFastMutex(&mut (*vcb).mutex);
            let cur = xattr::get_xattr(&mut (*vcb).fs, (*fcb).ino, XIDX_USER, &short);
            let res = match cur {
                Ok(v) => {
                    let mut value = v.unwrap_or_default();
                    value.resize(new_size as usize, 0);
                    xattr::set_xattr(
                        &mut (*vcb).fs,
                        (*fcb).ino,
                        XIDX_USER,
                        &short,
                        &value,
                    )
                }
                Err(e) => Err(e),
            };
            ExReleaseFastMutex(&mut (*vcb).mutex);
            if let Err(e) = res {
                return complete_err(irp, &e);
            }
            ExAcquireFastMutex(&mut (*fcb).lazy_mutex);
            (*fcb).size = new_size as u64;
            (*fcb).alloc_size = (*fcb).size;
            ExReleaseFastMutex(&mut (*fcb).lazy_mutex);
            touch_change(fcb);
            complete_irp(irp, 0, 0);
            0
        }
        nd::FIC_DISPOSITION_INFORMATION => {
            if in_len < 1 {
                return complete_err_raw(irp, nd::st::INVALID_PARAMETER);
            }
            (*ccb).delete_pending = *input != 0;
            complete_irp(irp, 0, 0);
            0
        }
        nd::FIC_DISPOSITION_INFORMATION_EX => {
            let flags = rd32(0);
            (*ccb).delete_pending = flags & nd::FILE_DISPOSITION_FLAG_DELETE != 0;
            complete_irp(irp, 0, 0);
            0
        }
        nd::FIC_POSITION_INFORMATION => {
            (*ccb).position = rd64(0).max(0) as u64;
            complete_irp(irp, 0, 0);
            0
        }
        nd::FIC_BASIC_INFORMATION | nd::FIC_MODE_INFORMATION => {
            complete_irp(irp, 0, 0);
            0
        }
        _ => complete_err_raw(irp, nd::st::UNSUCCESSFUL),
    }
}

/// Liste des flux d'un inode : (nom UTF-8, taille).
pub unsafe fn stream_list(vcb: *mut Vcb, ino: u32) -> Vec<(Vec<u8>, u64)> {
    ExAcquireFastMutex(&mut (*vcb).mutex);
    let r = xattr::list_xattrs(&mut (*vcb).fs, ino);
    ExReleaseFastMutex(&mut (*vcb).mutex);
    let mut out = Vec::new();
    if let Ok(l) = r {
        for (e, v) in l {
            if e.name_index == XIDX_USER && e.name.starts_with(ADS_PREFIX) {
                out.push((e.name[ADS_PREFIX.len()..].to_vec(), v.len() as u64));
            }
        }
    }
    out
}

/// Taille totale des EAs Windows d'un inode (FILE_EA_INFORMATION).
pub unsafe fn ea_total_size(vcb: *mut Vcb, ino: u32) -> u32 {
    ExAcquireFastMutex(&mut (*vcb).mutex);
    let r = xattr::list_xattrs(&mut (*vcb).fs, ino);
    ExReleaseFastMutex(&mut (*vcb).mutex);
    match r {
        Ok(l) => l
            .iter()
            .filter(|(e, _)| e.name_index == XIDX_USER && e.name.starts_with(EA_PREFIX))
            .map(|(e, v)| {
                align4(8 + (e.name.len() - EA_PREFIX.len()) + v.len()) as u32
            })
            .sum(),
        Err(_) => 0,
    }
}

// ---------------------------------------------------------------------------
// Helpers communs
// ---------------------------------------------------------------------------

/// Rafraîchit les temps de modification vus Windows (l'ext4 ctime est déjà
/// posé par le moteur).
unsafe fn touch_change(fcb: *mut Fcb) {
    let mut t = LARGE_INTEGER::default();
    KeQuerySystemTime(&mut t as *mut LARGE_INTEGER as *mut i64);
    let now = core::ptr::read(core::ptr::addr_of!(t.QuadPart));
    (*fcb).write = now;
    (*fcb).change = now;
}

/// Notification de changement (filtre EA/sécurité) sur le chemin du CCB.
unsafe fn notify_attr_change(vcb: *mut Vcb, ccb: *mut Ccb, fcb: *mut Fcb, filter: u32) {
    if (*vcb).notify_sync.is_null() {
        return;
    }
    let root_fcb = match (*vcb).fcbs.get(&ext4_core::INO_ROOT) {
        Some(f) => core::ptr::from_ref(f.as_ref()),
        None => return,
    };
    let mut buf = (*ccb).full_path.clone();
    if buf.is_empty() {
        buf.push(0x5C);
    }
    buf.push(0);
    let len = (buf.len() - 1) * 2;
    let mut us = wdk_sys::UNICODE_STRING {
        Length: len as u16,
        MaximumLength: (len + 2) as u16,
        Buffer: Box::into_raw(buf.into_boxed_slice()) as *mut u16,
    };
    FsRtlNotifyFullReportChange(
        (*vcb).notify_sync,
        (&(*root_fcb).notify_list) as *const _ as *mut c_void,
        &mut us,
        0,
        null_mut(),
        0,
        0,
        filter,
        nd::FILE_ACTION_MODIFIED,
    );
    let _ = fcb;
}
