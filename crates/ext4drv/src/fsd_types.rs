//! Types de la couche FSD : VCB (volume), FCB (fichier), CCB (ouverture),
//! conversions temps NT↔Unix, petits helpers d'accès aux structures wdm.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::ffi::c_void;
use core::ptr::addr_of_mut;

use ext4_core::fs::Filesystem;

use crate::ntshim::{SHARE_ACCESS, *};
use crate::ntdefs as nd;
use wdk_sys::{FAST_MUTEX, UNICODE_STRING};

// ---------------------------------------------------------------------------
// Temps : Unix u32 (ext4) ↔ NT FILETIME i64 (100 ns depuis 1601)
// ---------------------------------------------------------------------------

pub const EPOCH_DIFF: i64 = 11_644_473_600;

#[inline]
pub fn unix_to_nt(t: u32) -> i64 {
    (t as i64 + EPOCH_DIFF) * 10_000_000
}

#[inline]
pub fn nt_to_unix(t: i64) -> u32 {
    let s = (t / 10_000_000) - EPOCH_DIFF;
    if s < 0 {
        0
    } else {
        s as u32
    }
}

// ---------------------------------------------------------------------------
// FCB
// ---------------------------------------------------------------------------

/// Object pointers du Cache Manager (SectionObjectPointers)
#[repr(C)]
pub struct E4SectionObjectPointers {
    pub DataSectionObject: *mut c_void,
    pub SharedCacheMap: *mut c_void,
    pub ImageSectionObject: *mut c_void,
}

pub const FCB_TYPE_FILE: u8 = 1;
pub const FCB_TYPE_DIR: u8 = 2;
pub const FCB_TYPE_ROOT: u8 = 3;
pub const FCB_TYPE_SYMLINK: u8 = 4;
pub const FCB_TYPE_VOLUME: u8 = 5;

/// File Control Block. La durée de vie est celle du VOLUME (cache FCB par
/// inode, purgé au démontage) : simplifie radicalement la gestion mémoire
/// vis-à-vis du Cache Manager.
pub struct Fcb {
    pub ino: u32,
    pub ftype: u8, // FCB_TYPE_*
    /// taille courante vue Windows (octets)
    pub size: u64,
    /// taille d'allocation vue Windows (arrondie au bloc)
    pub alloc_size: u64,
    pub attributes: u32,
    pub links: u32,
    pub creation: i64,
    pub access: i64,
    pub write: i64,
    pub change: i64,
    pub reparse_tag: u32,
    pub lazy_mutex: FAST_MUTEX,
    pub sps: E4SectionObjectPointers,
    /// tête de liste FsRtlNotify (LIST_ENTRY x64 inline, alignée 8)
    pub notify_list: [u64; 2],
    /// VCB propriétaire (pour les callbacks Cc)
    pub vcb: *mut c_void,
    /// métadonnées ext4 étendues (symbole cible, etc.)
    pub target: Option<Vec<u8>>,
    /// FILE_LOCK opaque (FsRtl) pour les verrous de plage
    pub lock: *mut c_void,
    /// oplock opaque (FsRtl), initialisé paresseusement au premier FSCTL
    pub oplock: *mut c_void,
    /// nom du flux alternatif (ADS) si ce FCB porte un flux (None = fichier)
    pub stream: Option<Vec<u8>>,
    pub open_count: u32,
}

pub const NOTIFY_LIST_ENTRY_SIZE: usize = 16; // LIST_ENTRY x64

/// Initialise une LIST_ENTRY vide (Flink = Blink = self).
pub unsafe fn init_list_head(entry: *mut [u64; 2]) {
    let p = entry as *mut u64;
    core::ptr::write(p, entry as u64);
    core::ptr::write(p.add(1), entry as u64);
}

#[inline]
pub unsafe fn list_is_empty(entry: *mut [u64; 2]) -> bool {
    core::ptr::read(entry as *const u64) == entry as u64
}

pub const VCB_FLAG_DISMOUNTED: u32 = 0x1;
pub const VCB_FLAG_READ_ONLY: u32 = 0x2;

/// Volume Control Block. Tout accès est sérialisé par `mutex` (FAST_MUTEX).
/// Règle d'or : JAMAIS d'appel Cc* sous le mutex (risque de re-entrance
/// paging IO) ; les seuls points Cc sous lock sont CcSetFileSizes/CcPurge
/// (aucun IRP synchrone vers nous).
pub struct Vcb {
    pub device: *mut c_void,       // PDEVICE_OBJECT volume
    pub target_dev: *mut c_void,   // PDEVICE_OBJECT disque/volume réel
    pub vpb: *mut c_void,          // PVPB
    pub fs: Filesystem,
    pub mutex: FAST_MUTEX,
    pub fcbs: BTreeMap<u32, Box<Fcb>>,
    /// FCB des flux alternatifs, clé (inode, nom de flux UTF-8)
    pub streams: BTreeMap<(u32, Vec<u8>), Box<Fcb>>,
    pub root: u32,                 // ino racine (= ext4 INO_ROOT)
    pub serial: u32,
    pub label: Vec<u16>,           // sans terminateur
    pub nt_device_name: Vec<u16>,  // \Device\HarddiskVolumeN (RealDevice)
    pub dos_letter: Option<Vec<u16>>, // \??\X: si mount manager a attribué
    pub notify_sync: PNOTIFY_SYNC,
    pub flags: u32,
    pub open_file_count: u32,
    pub read_only: bool,
}

unsafe impl Send for Vcb {}
unsafe impl Sync for Vcb {}

impl Vcb {
    pub unsafe fn is_read_only(&self) -> bool {
        self.read_only
    }
}

// ---------------------------------------------------------------------------
// CCB
// ---------------------------------------------------------------------------

pub const CCB_TYPE_FILE: u8 = 1;
pub const CCB_TYPE_DIR: u8 = 2;
pub const CCB_TYPE_VOLUME: u8 = 3;

/// Context Control Block : un par FILE_OBJECT ouvert.
pub struct Ccb {
    pub cctype: u8,
    pub fcb_ino: u32,
    /// chemin absolu volume-relatif de l'ouverture (UTF-16, sans drive)
    pub full_path: Vec<u16>,
    /// chemin du répertoire contenant (pour les notifications)
    pub dir_path: Vec<u16>,
    // énumération de répertoire
    pub dir_snapshot: Option<Vec<(Vec<u8>, u32, u8)>>, // (nom, ino, type dirent)
    pub dir_cursor: usize,
    pub pattern: Option<Vec<u16>>,
    pub delete_pending: bool,
    pub posix_delete: bool,
    pub position: u64,
    pub share: SHARE_ACCESS,
}

impl Ccb {
    pub fn new(cctype: u8, fcb_ino: u32) -> Ccb {
        Ccb {
            cctype,
            fcb_ino,
            full_path: Vec::new(),
            dir_path: Vec::new(),
            dir_snapshot: None,
            dir_cursor: 0,
            pattern: None,
            delete_pending: false,
            posix_delete: false,
            position: 0,
            share: SHARE_ACCESS::default(),
        }
    }
}

// ---------------------------------------------------------------------------
// Accès Parameters.IO_STACK_LOCATION par offsets x64 (indépendant de bindgen)
//
// IO_STACK_LOCATION x64 : MajorFunction@0, MinorFunction@1, Flags@2,
// Control@3, (pad 4), Parameters@0x08, FileObject@0x28, Completion@0x30.
// Tous les layouts ci-dessous sont issus de wdm.h x64 (stable depuis NT).
// ---------------------------------------------------------------------------

pub type IrpSp = wdk_sys::PIO_STACK_LOCATION;

#[inline]
pub unsafe fn sp_major(sp: IrpSp) -> u8 {
    core::ptr::read(sp as *const u8)
}
#[inline]
pub unsafe fn sp_minor(sp: IrpSp) -> u8 {
    core::ptr::read(sp.add(1))
}
#[inline]
pub unsafe fn sp_flags(sp: IrpSp) -> u8 {
    core::ptr::read(sp.add(2))
}
#[inline]
pub unsafe fn sp_control(sp: IrpSp) -> u8 {
    core::ptr::read(sp.add(3))
}
#[inline]
pub unsafe fn sp_file_object(sp: IrpSp) -> wdk_sys::PFILE_OBJECT {
    core::ptr::read((sp as *const u8).add(0x28) as *const wdk_sys::PFILE_OBJECT)
}

#[inline]
unsafe fn pbase(sp: IrpSp) -> *const u8 {
    (sp as *const u8).add(8)
}
unsafe fn p32(sp: IrpSp, off: usize) -> u32 {
    let p = pbase(sp).add(off) as *const u32;
    core::ptr::read_unaligned(p)
}
unsafe fn p64(sp: IrpSp, off: usize) -> i64 {
    let p = pbase(sp).add(off) as *const i64;
    core::ptr::read_unaligned(p)
}
unsafe fn pptr(sp: IrpSp, off: usize) -> *mut c_void {
    let p = pbase(sp).add(off) as *const u64;
    core::ptr::read_unaligned(p) as *mut c_void
}

// --- Create ---
pub unsafe fn p_create_options(sp: IrpSp) -> u32 {
    p32(sp, 8)
}
pub unsafe fn p_create_attrs(sp: IrpSp) -> u16 {
    let p = pbase(sp).add(0xC) as *const u16;
    core::ptr::read_unaligned(p)
}
pub unsafe fn p_create_share(sp: IrpSp) -> u16 {
    let p = pbase(sp).add(0xE) as *const u16;
    core::ptr::read_unaligned(p)
}
/// DesiredAccess via IO_SECURITY_CONTEXT { SecurityQos@0, SecurityContext@8,
/// DesiredAccess*@16 }
pub unsafe fn p_create_access(sp: IrpSp) -> u32 {
    let sc = pptr(sp, 0);
    if sc.is_null() {
        return 0;
    }
    let da = core::ptr::read_unaligned((sc as *const u8).add(16) as *const *const u32);
    if da.is_null() {
        0
    } else {
        core::ptr::read_unaligned(da)
    }
}

// --- Read / Write ---
pub unsafe fn p_read(sp: IrpSp) -> (u32, i64) {
    (p32(sp, 0), p64(sp, 0x10))
}

// --- QueryDirectory ---
pub unsafe fn p_query_dir(sp: IrpSp) -> (u32, *mut wdk_sys::UNICODE_STRING, u32, u32) {
    (
        p32(sp, 0),
        pptr(sp, 8) as *mut wdk_sys::UNICODE_STRING,
        p32(sp, 0x10),
        p32(sp, 0x14),
    )
}

// --- QueryFile ---
pub unsafe fn p_query_file(sp: IrpSp) -> (u32, u32) {
    (p32(sp, 0), p32(sp, 8))
}

// --- SetFile ---
pub unsafe fn p_set_file(sp: IrpSp) -> (u32, u32, wdk_sys::PFILE_OBJECT, bool) {
    let advance = core::ptr::read_unaligned(pbase(sp).add(0x1C));
    (p32(sp, 0), p32(sp, 8), pptr(sp, 0x10) as wdk_sys::PFILE_OBJECT, advance != 0)
}

// --- NotifyDirectory ---
pub unsafe fn p_notify(sp: IrpSp) -> (u32, *mut wdk_sys::UNICODE_STRING, u32) {
    (p32(sp, 0), pptr(sp, 8) as *mut wdk_sys::UNICODE_STRING, p32(sp, 0x10))
}

// --- FileSystemControl ---
pub unsafe fn p_fsctl(sp: IrpSp) -> (u32, *mut c_void, u32) {
    (p32(sp, 0), pptr(sp, 8), p32(sp, 0x10))
}
/// USER_FS_REQUEST : (OutputBufferLength, InputBuffer, FsControlCode,
/// InputBufferLength) — layout UserFsRequest x64
pub unsafe fn p_userfs(sp: IrpSp) -> (u32, *mut c_void, u32, u32) {
    (p32(sp, 0), pptr(sp, 8), p32(sp, 0x10), p32(sp, 0x18))
}
/// MountVolume : (Vpb, DeviceObject)
pub unsafe fn p_mount(sp: IrpSp) -> (*mut wdk_sys::VPB, *mut wdk_sys::DEVICE_OBJECT) {
    (
        pptr(sp, 0) as *mut wdk_sys::VPB,
        pptr(sp, 8) as *mut wdk_sys::DEVICE_OBJECT,
    )
}

// --- DeviceIoControl ---
pub unsafe fn p_devctrl(sp: IrpSp) -> (u32, *mut c_void, u32) {
    (p32(sp, 0), pptr(sp, 8), p32(sp, 0x10))
}

// --- QueryEa : (Length, EaList, EaListLength) ---
pub unsafe fn p_query_ea(sp: IrpSp) -> (u32, *mut c_void, u32) {
    (p32(sp, 0), pptr(sp, 8), p32(sp, 0x10))
}

// --- SetEa : Length ---
pub unsafe fn p_set_ea(sp: IrpSp) -> u32 {
    p32(sp, 0)
}

// --- QuerySecurity : (SecurityInformation, Length, SecurityDescriptor,
//     ReturnLength) ---
pub unsafe fn p_query_sec(sp: IrpSp) -> (u32, u32, *mut c_void, *mut u32) {
    (p32(sp, 0), p32(sp, 8), pptr(sp, 0x10), pptr(sp, 0x18) as *mut u32)
}

// --- SetSecurity : (SecurityInformation, SecurityDescriptor, Length) ---
pub unsafe fn p_set_sec(sp: IrpSp) -> (u32, *mut c_void, u32) {
    (p32(sp, 0), pptr(sp, 8), p32(sp, 0x10))
}

// --- QueryVolume ---
pub unsafe fn p_query_volume(sp: IrpSp) -> (u32, u32) {
    (p32(sp, 0), p32(sp, 8))
}

// ---------------------------------------------------------------------------
// Accès aux structures wdm par les champs wdk-sys
// ---------------------------------------------------------------------------

#[inline]
pub unsafe fn irp_flags(irp: wdk_sys::PIRP) -> u32 {
    core::ptr::read(core::ptr::addr_of!((*irp).Flags))
}

#[inline]
pub unsafe fn file_object(sp: wdk_sys::PIO_STACK_LOCATION) -> wdk_sys::PFILE_OBJECT {
    core::ptr::read(core::ptr::addr_of!((*sp).FileObject))
}

#[inline]
pub unsafe fn fs_context(fo: wdk_sys::PFILE_OBJECT) -> *mut c_void {
    core::ptr::read(core::ptr::addr_of!((*fo).FsContext))
}

#[inline]
pub unsafe fn fs_context2(fo: wdk_sys::PFILE_OBJECT) -> *mut c_void {
    core::ptr::read(core::ptr::addr_of!((*fo).FsContext2))
}

#[inline]
pub unsafe fn set_fs_context(fo: wdk_sys::PFILE_OBJECT, ctx: *mut c_void, ccb: *mut c_void) {
    core::ptr::write(addr_of_mut!((*fo).FsContext), ctx);
    core::ptr::write(addr_of_mut!((*fo).FsContext2), ccb);
}

#[inline]
pub unsafe fn sps_of(fo: wdk_sys::PFILE_OBJECT) -> *mut E4SectionObjectPointers {
    core::ptr::read(core::ptr::addr_of!((*fo).SectionObjectPointer)) as *mut E4SectionObjectPointers
}

#[inline]
pub unsafe fn set_sps(fo: wdk_sys::PFILE_OBJECT, sps: *mut E4SectionObjectPointers) {
    core::ptr::write(addr_of_mut!((*fo).SectionObjectPointer), sps as *mut c_void);
}

/// FileName (UNICODE_STRING) du FILE_OBJECT.
#[inline]
pub unsafe fn file_name(fo: wdk_sys::PFILE_OBJECT) -> UNICODE_STRING {
    core::ptr::read(core::ptr::addr_of!((*fo).FileName))
}

#[inline]
pub unsafe fn related_file_object(fo: wdk_sys::PFILE_OBJECT) -> wdk_sys::PFILE_OBJECT {
    core::ptr::read(core::ptr::addr_of!((*fo).RelatedFileObject))
}

/// IoMarkIrpPending (bit SL_PENDING_RETURNED de la stack location courante).
pub unsafe fn mark_pending(irp: wdk_sys::PIRP) {
    let sp = (*irp).Tail.Overlay.CurrentStackLocation;
    let ctl = core::ptr::read((sp as *const u8).add(3));
    core::ptr::write((sp as *mut u8).add(3), ctl | 0x01);
}

// ---------------------------------------------------------------------------
// Mapping erreurs ext4 → NTSTATUS
// ---------------------------------------------------------------------------

pub fn nt_from_ext4(e: &ext4_core::Error) -> i32 {
    use ext4_core::Error as E;
    match e {
        E::NotFound | E::InodeNotFound(_) => nd::st::OBJECT_NAME_NOT_FOUND,
        E::BadPath => nd::st::OBJECT_NAME_INVALID,
        E::AlreadyExists => nd::st::OBJECT_NAME_COLLISION,
        E::DirectoryNotEmpty => nd::st::DIRECTORY_NOT_EMPTY,
        E::ReadOnly => nd::st::MEDIA_WRITE_PROTECTED,
        E::Encrypted => nd::st::DATA_ENCRYPTION_UNSUPPORTED,
        E::NoSpace => 0xC000_007Fu32 as i32, // STATUS_DISK_FULL
        E::NoInodes => 0xC000_007Fu32 as i32,
        E::InvalidSize | E::ExtentFull | E::DirIndexFull => nd::st::UNSUCCESSFUL,
        E::Device(c) => *c,
        _ => nd::st::DEVICE_DATA_ERROR,
    }
}

pub const STATUS_DISK_FULL: i32 = 0xC000_007Fu32 as i32;
