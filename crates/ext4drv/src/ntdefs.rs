//! Définitions NT pour la couche FSD : constantes, structures d'information
//! fichiers/répertoires, reparse points, interface mount manager.
//!
//! Stratégie anti-fragilité vis-à-vis de wdk-sys : les STRUCTURES NTIFS sont
//! définies ici en `repr(C)` exact (préfixe E4) et castées en `*mut c_void`
//! aux frontières d'appel ; seules les fonctions (noms C stables) et les
//! structures wdm de base (IRP, IO_STACK_LOCATION, VPB, FILE_OBJECT) sont
//! tirées de `wdk_sys`.

#![allow(non_snake_case, dead_code, non_upper_case_globals, non_camel_case_types)]

use core::ffi::c_void;

pub type E4U8 = u8;
pub type E4U16 = u16;
pub type E4U32 = u32;
pub type E4I64 = i64;

// ---------------------------------------------------------------------------
// CTL_CODE
// ---------------------------------------------------------------------------

pub const fn ctl_code(dev: u32, func: u32, method: u32, access: u32) -> u32 {
    (dev << 16) | (access << 14) | (func << 2) | method
}

pub const METHOD_BUFFERED: u32 = 0;
pub const METHOD_IN_DIRECT: u32 = 1;
pub const METHOD_OUT_DIRECT: u32 = 2;
pub const METHOD_NEITHER: u32 = 3;
pub const FILE_ANY_ACCESS: u32 = 0;
pub const FILE_READ_ACCESS: u32 = 0x1;
pub const FILE_WRITE_ACCESS: u32 = 0x2;

pub const FILE_DEVICE_FILE_SYSTEM: u32 = 0x0000_0009;
pub const FILE_DEVICE_DISK_FILE_SYSTEM: u32 = 0x0000_0008;
pub const FILE_DEVICE_UNKNOWN: u32 = 0x0000_0022;

// ---------------------------------------------------------------------------
// IRP major/minor
// ---------------------------------------------------------------------------

pub const MJ_CREATE: u8 = 0x00;
pub const MJ_CLOSE: u8 = 0x02;
pub const MJ_READ: u8 = 0x03;
pub const MJ_WRITE: u8 = 0x04;
pub const MJ_QUERY_INFORMATION: u8 = 0x05;
pub const MJ_SET_INFORMATION: u8 = 0x06;
pub const MJ_QUERY_EA: u8 = 0x07;
pub const MJ_SET_EA: u8 = 0x08;
pub const MJ_FLUSH_BUFFERS: u8 = 0x09;
pub const MJ_QUERY_VOLUME_INFORMATION: u8 = 0x0A;
pub const MJ_SET_VOLUME_INFORMATION: u8 = 0x0B;
pub const MJ_DIRECTORY_CONTROL: u8 = 0x0C;
pub const MJ_FILE_SYSTEM_CONTROL: u8 = 0x0D;
pub const MJ_DEVICE_CONTROL: u8 = 0x0E;
pub const MJ_SHUTDOWN: u8 = 0x10;
pub const MJ_LOCK_CONTROL: u8 = 0x11;
pub const MJ_CLEANUP: u8 = 0x12;
pub const MJ_QUERY_SECURITY: u8 = 0x14;
pub const MJ_SET_SECURITY: u8 = 0x15;
pub const MJ_PNP: u8 = 0x1B;

pub const MN_USER_FS_REQUEST: u8 = 0x00;
pub const MN_MOUNT_VOLUME: u8 = 0x01;
pub const MN_VERIFY_VOLUME: u8 = 0x02;
pub const MN_LOAD_FILE_SYSTEM: u8 = 0x03;
pub const MN_QUERY_DIRECTORY: u8 = 0x01;
pub const MN_NOTIFY_CHANGE_DIRECTORY: u8 = 0x02;
pub const MN_SURPRISE_REMOVAL: u8 = 0x13;

// Flags IRP
pub const IRP_PAGING_IO: u32 = 0x0000_0010;
pub const IRP_NOCACHE: u32 = 0x0000_0020;
pub const IRP_SYNCHRONOUS_PAGING_IO: u32 = 0x0000_0040;

// Flags device
pub const DO_BUFFERED_IO: u32 = 0x0000_0004;
pub const DO_DIRECT_IO: u32 = 0x0000_0010;
pub const DO_DEVICE_INITIALIZING: u32 = 0x0000_0080;
pub const DO_EXCLUSIVE: u32 = 0x0000_0100;

// Flags SL (IrpSp->Flags)
pub const SL_FORCE_ACCESS_CHECK: u8 = 0x01;
pub const SL_OPEN_PAGING_FILE: u8 = 0x02;
pub const SL_OPEN_TARGET_DIRECTORY: u8 = 0x04;
pub const SL_STOP_ON_SYMLINK: u8 = 0x08;
pub const SL_CASE_SENSITIVE: u8 = 0x80;

// Flags FILE_OBJECT (plats, FieldOffset connus)
pub const FO_FILE_OPEN_CANCELLED: u32 = 0x0080_0000;

// VPB flags (VPB_MOUNTED en u16 : appliqué à (*vpb).Flags qui est u16 en x64)
pub const VPB_MOUNTED: u16 = 0x0000_0001;
pub const VPB_LOCKED: u32 = 0x0000_0002;
pub const VPB_PERSISTENT: u32 = 0x0000_0004;

// Ré-export : les sites d'appel écrivent nd::FILE_ACTION_* / nd::FILE_NOTIFY_CHANGE_*
// (les constantes vivent dans ntshim, uniques dans le crate)
pub use crate::ntshim::{
    FILE_ACTION_ADDED, FILE_ACTION_MODIFIED, FILE_ACTION_REMOVED, FILE_NOTIFY_CHANGE_EA,
    FILE_NOTIFY_CHANGE_NAME, FILE_NOTIFY_CHANGE_SECURITY,
};

// ---------------------------------------------------------------------------
// Create (IrpSp->Parameters.Create.Options / desired access / share)
// ---------------------------------------------------------------------------

pub const FILE_OPEN: u32 = 0x0000_0001;
pub const FILE_CREATE: u32 = 0x0000_0002;
pub const FILE_OPEN_IF: u32 = 0x0000_0003;
pub const FILE_OVERWRITE: u32 = 0x0000_0004;
pub const FILE_OVERWRITE_IF: u32 = 0x0000_0005;
pub const FILE_SUPERSEDE: u32 = 0x0000_0000;

pub const FILE_DIRECTORY_FILE: u32 = 0x0000_0001;
pub const FILE_WRITE_THROUGH: u32 = 0x0000_0002;
pub const FILE_SEQUENTIAL_ONLY: u32 = 0x0000_0004;
pub const FILE_SYNCHRONOUS_IO_NONALERT: u32 = 0x0000_0020;
pub const FILE_NON_DIRECTORY_FILE: u32 = 0x0000_0040;
pub const FILE_NO_INTERMEDIATE_BUFFERING: u32 = 0x0000_0080;
pub const FILE_DELETE_ON_CLOSE: u32 = 0x0000_1000;
pub const FILE_OPEN_BY_FILE_ID: u32 = 0x0000_2000;
pub const FILE_OPEN_FOR_BACKUP_INTENT: u32 = 0x0000_4000;
pub const FILE_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
pub const FILE_OPEN_FOR_FREE_SPACE_QUERY: u32 = 0x0080_0000;

pub const FILE_READ_DATA: u32 = 0x0000_0001;
pub const FILE_WRITE_DATA: u32 = 0x0000_0002;
pub const FILE_APPEND_DATA: u32 = 0x0000_0004;
pub const FILE_EXECUTE: u32 = 0x0000_0020;
pub const FILE_READ_ATTRIBUTES: u32 = 0x0000_0080;
pub const FILE_WRITE_ATTRIBUTES: u32 = 0x0000_0100;
pub const DELETE: u32 = 0x0001_0000;
pub const READ_CONTROL: u32 = 0x0002_0000;
pub const SYNCHRONIZE: u32 = 0x0010_0000;
pub const GENERIC_READ: u32 = 0x8000_0000;
pub const GENERIC_WRITE: u32 = 0x4000_0000;
pub const GENERIC_EXECUTE: u32 = 0x2000_0000;
pub const GENERIC_ALL: u32 = 0x1000_0000;

pub const FILE_SHARE_READ: u32 = 0x1;
pub const FILE_SHARE_WRITE: u32 = 0x2;
pub const FILE_SHARE_DELETE: u32 = 0x4;

// ---------------------------------------------------------------------------
// Attributs / tags reparse
// ---------------------------------------------------------------------------

pub const FILE_ATTRIBUTE_READONLY: u32 = 0x1;
pub const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
pub const FILE_ATTRIBUTE_SYSTEM: u32 = 0x4;
pub const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
pub const FILE_ATTRIBUTE_ARCHIVE: u32 = 0x20;
pub const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;
pub const FILE_ATTRIBUTE_TEMPORARY: u32 = 0x100;
pub const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

pub const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;
pub const IO_REPARSE_TAG_SYMLINK: u32 = 0xA000_000C;
pub const IO_REPARSE_TAG_LX_SYMLINK: u32 = 0xA000_001D;
pub const SYMLINK_FLAG_RELATIVE: u32 = 0x1;

pub const MAXIMUM_REPARSE_DATA_BUFFER_SIZE: usize = 0x4000;

// ---------------------------------------------------------------------------
// Classes FILE_INFORMATION_CLASS (valeurs standard)
// ---------------------------------------------------------------------------

pub const FIC_DIRECTORY_INFORMATION: u32 = 1;
pub const FIC_FULL_DIRECTORY_INFORMATION: u32 = 2;
pub const FIC_BOTH_DIRECTORY_INFORMATION: u32 = 3;
pub const FIC_BASIC_INFORMATION: u32 = 4;
pub const FIC_STANDARD_INFORMATION: u32 = 5;
pub const FIC_INTERNAL_INFORMATION: u32 = 6;
pub const FIC_EA_INFORMATION: u32 = 7;
pub const FIC_ACCESS_INFORMATION: u32 = 8;
pub const FIC_NAME_INFORMATION: u32 = 9;
pub const FIC_RENAME_INFORMATION: u32 = 10;
pub const FIC_LINK_INFORMATION: u32 = 11;
pub const FIC_NAMES_INFORMATION: u32 = 12;
pub const FIC_DISPOSITION_INFORMATION: u32 = 13;
pub const FIC_POSITION_INFORMATION: u32 = 14;
pub const FIC_ALL_INFORMATION: u32 = 18;
pub const FIC_ALLOCATION_INFORMATION: u32 = 19;
pub const FIC_END_OF_FILE_INFORMATION: u32 = 20;
pub const FIC_ALTERNATE_NAME_INFORMATION: u32 = 21;
pub const FIC_STREAM_INFORMATION: u32 = 22;
pub const FIC_ATTRIBUTE_TAG_INFORMATION: u32 = 35;
pub const FIC_ID_BOTH_DIRECTORY_INFORMATION: u32 = 37;
pub const FIC_ID_FULL_DIRECTORY_INFORMATION: u32 = 38;
pub const FIC_SHORT_NAME_INFORMATION: u32 = 40;
pub const FIC_NETWORK_OPEN_INFORMATION: u32 = 34;
pub const FIC_MODE_INFORMATION: u32 = 16;
pub const FIC_DISPOSITION_INFORMATION_EX: u32 = 64;
pub const FIC_RENAME_INFORMATION_EX: u32 = 65;

// ---------------------------------------------------------------------------
// FSCTL (FILE_DEVICE_FILE_SYSTEM)
// ---------------------------------------------------------------------------

pub const FSCTL_REQUEST_OPLOCK_LEVEL_1: u32 = ctl_code(FILE_DEVICE_FILE_SYSTEM, 0, METHOD_BUFFERED, FILE_ANY_ACCESS);
pub const FSCTL_REQUEST_OPLOCK_LEVEL_2: u32 = ctl_code(FILE_DEVICE_FILE_SYSTEM, 1, METHOD_BUFFERED, FILE_ANY_ACCESS);
pub const FSCTL_REQUEST_BATCH_OPLOCK: u32 = ctl_code(FILE_DEVICE_FILE_SYSTEM, 2, METHOD_BUFFERED, FILE_ANY_ACCESS);
pub const FSCTL_OPLOCK_BREAK_ACKNOWLEDGE: u32 = ctl_code(FILE_DEVICE_FILE_SYSTEM, 3, METHOD_BUFFERED, FILE_ANY_ACCESS);
pub const FSCTL_OPBATCH_ACK_CLOSE_PENDING: u32 = ctl_code(FILE_DEVICE_FILE_SYSTEM, 4, METHOD_BUFFERED, FILE_ANY_ACCESS);
pub const FSCTL_OPLOCK_BREAK_NOTIFY: u32 = ctl_code(FILE_DEVICE_FILE_SYSTEM, 5, METHOD_BUFFERED, FILE_ANY_ACCESS);
pub const FSCTL_OPLOCK_BREAK_ACK_NO_2: u32 = ctl_code(FILE_DEVICE_FILE_SYSTEM, 6, METHOD_BUFFERED, FILE_ANY_ACCESS);
pub const FSCTL_REQUEST_FILTER_OPLOCK: u32 = ctl_code(FILE_DEVICE_FILE_SYSTEM, 7, METHOD_BUFFERED, FILE_ANY_ACCESS);
pub const FSCTL_IS_VOLUME_MOUNTED: u32 = ctl_code(FILE_DEVICE_FILE_SYSTEM, 10, METHOD_BUFFERED, FILE_ANY_ACCESS);
pub const FSCTL_DISMOUNT_VOLUME: u32 = ctl_code(FILE_DEVICE_FILE_SYSTEM, 8, METHOD_BUFFERED, FILE_ANY_ACCESS);
pub const FSCTL_ALLOW_EXTENDED_DASD_IO: u32 = ctl_code(FILE_DEVICE_FILE_SYSTEM, 32, METHOD_BUFFERED, FILE_ANY_ACCESS);
pub const FSCTL_GET_REPARSE_POINT: u32 = ctl_code(FILE_DEVICE_FILE_SYSTEM, 42, METHOD_BUFFERED, FILE_ANY_ACCESS);
pub const FSCTL_SET_REPARSE_POINT: u32 = ctl_code(FILE_DEVICE_FILE_SYSTEM, 43, METHOD_BUFFERED, FILE_READ_ACCESS | FILE_WRITE_ACCESS);
pub const FSCTL_DELETE_REPARSE_POINT: u32 = ctl_code(FILE_DEVICE_FILE_SYSTEM, 44, METHOD_BUFFERED, FILE_READ_ACCESS | FILE_WRITE_ACCESS);
pub const FSCTL_GET_RETRIEVAL_POINTERS: u32 = ctl_code(FILE_DEVICE_FILE_SYSTEM, 19, METHOD_BUFFERED, FILE_ANY_ACCESS);
pub const FSCTL_SET_SPARSE: u32 = ctl_code(FILE_DEVICE_FILE_SYSTEM, 35, METHOD_BUFFERED, FILE_WRITE_ACCESS);
pub const FSCTL_GET_COMPRESSION: u32 = ctl_code(FILE_DEVICE_FILE_SYSTEM, 15, METHOD_BUFFERED, FILE_ANY_ACCESS);
pub const FSCTL_SET_COMPRESSION: u32 = ctl_code(FILE_DEVICE_FILE_SYSTEM, 16, METHOD_BUFFERED, FILE_READ_ACCESS | FILE_WRITE_ACCESS);
pub const FSCTL_READ_DATA: u32 = ctl_code(FILE_DEVICE_FILE_SYSTEM, 171, METHOD_OUT_DIRECT, FILE_READ_ACCESS);
pub const FSCTL_WRITE_DATA: u32 = ctl_code(FILE_DEVICE_FILE_SYSTEM, 172, METHOD_IN_DIRECT, FILE_WRITE_ACCESS);

// ---------------------------------------------------------------------------
// Mount manager (mountmgr.h) — MOUNTMGRCONTROLTYPE 'm' = 0x6D
// ---------------------------------------------------------------------------

pub const MOUNTMGRCONTROLTYPE: u32 = 0x6D; // 'm'

pub const IOCTL_MOUNTMGR_CREATE_POINT: u32 =
    ctl_code(MOUNTMGRCONTROLTYPE, 0, METHOD_BUFFERED, FILE_READ_ACCESS | FILE_WRITE_ACCESS);
pub const IOCTL_MOUNTMGR_QUERY_POINTS: u32 =
    ctl_code(MOUNTMGRCONTROLTYPE, 2, METHOD_BUFFERED, FILE_ANY_ACCESS);
pub const IOCTL_MOUNTMGR_VOLUME_ARRIVAL_NOTIFICATION: u32 =
    ctl_code(MOUNTMGRCONTROLTYPE, 11, METHOD_BUFFERED, FILE_READ_ACCESS);
pub const IOCTL_MOUNTMGR_QUERY_DOS_VOLUME_PATH: u32 =
    ctl_code(MOUNTMGRCONTROLTYPE, 12, METHOD_BUFFERED, FILE_ANY_ACCESS);
pub const IOCTL_MOUNTMGR_QUERY_DOS_VOLUME_PATHS: u32 =
    ctl_code(MOUNTMGRCONTROLTYPE, 13, METHOD_BUFFERED, FILE_ANY_ACCESS);

/// \Device\MountPointManager
pub const MOUNTMGR_DEVICE_NAME: &[u16] = &[
    0x5C, 0x44, 0x65, 0x76, 0x69, 0x63, 0x65, 0x5C, 0x4D, 0x6F, 0x75, 0x6E, 0x74, 0x50, 0x6F, 0x69,
    0x6E, 0x74, 0x4D, 0x61, 0x6E, 0x61, 0x67, 0x65, 0x72,
];

#[repr(C)]
pub struct E4_MOUNTMGR_TARGET_NAME {
    pub DeviceNameLength: E4U16, // en octets, sans terminateur
    pub DeviceName: [u16; 1],
}

#[repr(C)]
pub struct E4_MOUNTMGR_VOLUME_PATHS {
    pub MultiSzLength: E4U32, // longueur du multi-sz en octets
    pub MultiSz: [u16; 1],
}

// ---------------------------------------------------------------------------
// Structures d'information fichiers (layouts x64 exacts de ntifs.h)
// ---------------------------------------------------------------------------

/// FILE_BASIC_INFORMATION
#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct E4_BASIC_INFO {
    pub CreationTime: E4I64,
    pub LastAccessTime: E4I64,
    pub LastWriteTime: E4I64,
    pub ChangeTime: E4I64,
    pub FileAttributes: E4U32,
}

/// FILE_STANDARD_INFORMATION
#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct E4_STANDARD_INFO {
    pub AllocationSize: E4I64,
    pub EndOfFile: E4I64,
    pub NumberOfLinks: E4U32,
    pub DeletePending: E4U8,
    pub Directory: E4U8,
}

/// FILE_INTERNAL_INFORMATION
#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct E4_INTERNAL_INFO {
    pub IndexNumber: E4I64,
}

/// FILE_EA_INFORMATION
#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct E4_EA_INFO {
    pub EaAccessSize: E4U32,
}

/// FILE_ACCESS_INFORMATION
#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct E4_ACCESS_INFO {
    pub AccessFlags: E4U32,
}

/// FILE_POSITION_INFORMATION
#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct E4_POSITION_INFO {
    pub CurrentByteOffset: E4I64,
}

/// FILE_MODE_INFORMATION
#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct E4_MODE_INFO {
    pub Mode: E4U32,
}

/// FILE_ATTRIBUTE_TAG_INFORMATION
#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct E4_ATTRTAG_INFO {
    pub FileAttributes: E4U32,
    pub ReparseTag: E4U32,
}

/// FILE_NETWORK_OPEN_INFORMATION
#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct E4_NETWORK_OPEN_INFO {
    pub CreationTime: E4I64,
    pub LastAccessTime: E4I64,
    pub LastWriteTime: E4I64,
    pub ChangeTime: E4I64,
    pub AllocationSize: E4I64,
    pub EndOfFile: E4I64,
    pub FileAttributes: E4U32,
}

/// FILE_DIRECTORY_INFORMATION (FileName à l'offset 64)
#[repr(C)]
pub struct E4_DIR_INFO {
    pub NextEntryOffset: E4U32,
    pub FileIndex: E4U32,
    pub CreationTime: E4I64,
    pub LastAccessTime: E4I64,
    pub LastWriteTime: E4I64,
    pub ChangeTime: E4I64,
    pub EndOfFile: E4I64,
    pub AllocationSize: E4I64,
    pub FileAttributes: E4U32,
    pub FileNameLength: E4U32,
    pub _name: [u16; 1],
}

/// FILE_FULL_DIR_INFORMATION (EaSize en +, FileName @ 68)
#[repr(C)]
pub struct E4_FULL_DIR_INFO {
    pub NextEntryOffset: E4U32,
    pub FileIndex: E4U32,
    pub CreationTime: E4I64,
    pub LastAccessTime: E4I64,
    pub LastWriteTime: E4I64,
    pub ChangeTime: E4I64,
    pub EndOfFile: E4I64,
    pub AllocationSize: E4I64,
    pub FileAttributes: E4U32,
    pub FileNameLength: E4U32,
    pub EaSize: E4U32,
    pub _name: [u16; 1],
}

/// FILE_BOTH_DIR_INFORMATION (ShortName en +, FileName @ 94)
#[repr(C)]
pub struct E4_BOTH_DIR_INFO {
    pub NextEntryOffset: E4U32,
    pub FileIndex: E4U32,
    pub CreationTime: E4I64,
    pub LastAccessTime: E4I64,
    pub LastWriteTime: E4I64,
    pub ChangeTime: E4I64,
    pub EndOfFile: E4I64,
    pub AllocationSize: E4I64,
    pub FileAttributes: E4U32,
    pub FileNameLength: E4U32,
    pub EaSize: E4U32,
    pub ShortNameLength: E4U8,
    pub ShortName: [u16; 12],
    pub _name: [u16; 1],
}

/// FILE_ID_BOTH_DIR_INFORMATION (FileId en +, FileName @ 102)
#[repr(C)]
pub struct E4_ID_BOTH_DIR_INFO {
    pub NextEntryOffset: E4U32,
    pub FileIndex: E4U32,
    pub CreationTime: E4I64,
    pub LastAccessTime: E4I64,
    pub LastWriteTime: E4I64,
    pub ChangeTime: E4I64,
    pub EndOfFile: E4I64,
    pub AllocationSize: E4I64,
    pub FileAttributes: E4U32,
    pub FileNameLength: E4U32,
    pub EaSize: E4U32,
    pub ShortNameLength: E4U8,
    pub ShortName: [u16; 12],
    pub FileId: E4I64,
    pub _name: [u16; 1],
}

/// FILE_ID_FULL_DIR_INFORMATION (FileId en +, FileName @ 72)
#[repr(C)]
pub struct E4_ID_FULL_DIR_INFO {
    pub NextEntryOffset: E4U32,
    pub FileIndex: E4U32,
    pub CreationTime: E4I64,
    pub LastAccessTime: E4I64,
    pub LastWriteTime: E4I64,
    pub ChangeTime: E4I64,
    pub EndOfFile: E4I64,
    pub AllocationSize: E4I64,
    pub FileAttributes: E4U32,
    pub FileNameLength: E4U32,
    pub EaSize: E4U32,
    pub FileId: E4I64,
    pub _name: [u16; 1],
}

/// FILE_NAMES_INFORMATION
#[repr(C)]
pub struct E4_NAMES_INFO {
    pub NextEntryOffset: E4U32,
    pub FileNameLength: E4U32,
    pub _name: [u16; 1],
}

/// Offsets du champ FileName selon la classe (convention x64 ntifs.h).
pub fn dir_name_offset(class: u32) -> Option<usize> {
    match class {
        FIC_DIRECTORY_INFORMATION => Some(64),
        FIC_FULL_DIRECTORY_INFORMATION => Some(68),
        FIC_BOTH_DIRECTORY_INFORMATION => Some(94),
        FIC_ID_BOTH_DIRECTORY_INFORMATION => Some(102),
        FIC_ID_FULL_DIRECTORY_INFORMATION => Some(72),
        FIC_NAMES_INFORMATION => Some(8),
        _ => None,
    }
}

/// Taille de l'en-tête fixe de chaque structure (sans le nom).
pub fn dir_header_size(class: u32) -> Option<usize> {
    match class {
        FIC_DIRECTORY_INFORMATION => Some(64),
        FIC_FULL_DIRECTORY_INFORMATION => Some(68),
        FIC_BOTH_DIRECTORY_INFORMATION => Some(94),
        FIC_ID_BOTH_DIRECTORY_INFORMATION => Some(102),
        FIC_ID_FULL_DIRECTORY_INFORMATION => Some(72),
        FIC_NAMES_INFORMATION => Some(8),
        _ => None,
    }
}

/// FILE_ALL_INFORMATION (sans le champ Name final géré à la main)
#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct E4_ALL_INFO {
    pub Basic: E4_BASIC_INFO,
    pub Standard: E4_STANDARD_INFO,
    pub Internal: E4_INTERNAL_INFO,
    pub EaInformation: E4_EA_INFO,
    pub AccessInformation: E4_ACCESS_INFO,
    pub Position: E4_POSITION_INFO,
    pub Mode: E4_MODE_INFO,
    pub AlignmentRequirement: E4U32,
}

/// FILE_RENAME_INFORMATION / FILE_LINK_INFORMATION (même layout)
#[repr(C)]
pub struct E4_RENAME_INFO {
    pub ReplaceIfExists: E4U32,
    pub RootDirectory: *mut c_void,
    pub FileNameLength: E4U32,
    pub _name: [u16; 1],
}

/// FILE_DISPOSITION_INFORMATION
#[repr(C)]
pub struct E4_DISPOSITION_INFO {
    pub DeleteFile: E4U8,
}

/// FILE_DISPOSITION_INFORMATION_EX (flags)
#[repr(C)]
pub struct E4_DISPOSITION_INFO_EX {
    pub Flags: E4U32,
}

pub const FILE_DISPOSITION_FLAG_DELETE: u32 = 0x0000_0001;
pub const FILE_DISPOSITION_FLAG_POSIX_SEMANTICS: u32 = 0x0000_0002;
pub const FILE_DISPOSITION_FLAG_FORCE_IMAGE_CHECK: u32 = 0x0000_0010;
pub const FILE_DISPOSITION_FLAG_ON_CLOSE: u32 = 0x0000_1000;

/// FILE_END_OF_FILE_INFORMATION / FILE_ALLOCATION_INFORMATION
#[repr(C)]
pub struct E4_LENGTH_INFO {
    pub Length: E4I64,
}

/// CC_FILE_SIZES (Cache Manager)
#[repr(C)]
pub struct E4_FILE_SIZES {
    pub AllocationSize: E4I64,
    pub FileSize: E4I64,
    pub ValidDataLength: E4I64,
}

/// REPARSE_DATA_BUFFER (MAXIMUM_REPARSE_DATA_BUFFER_SIZE max) — on ne
/// modélise que la partie : tag, longueurs, puis un tampon de payload brut
/// que l'on remplit à la main (équivalent de l'union DataBuffer).
pub const E4_REPARSE_HEADER: usize = 8;

pub fn build_symlink_reparse(out: &mut [u8], substitute: &[u16], print: &[u16], relative: bool) -> Option<usize> {
    // REPARSE_DATA_BUFFER { tag u32, ReparseDataLength u16, reserved u16,
    //  SymbolicLinkReparseBuffer { sub_off u16, sub_len u16, print_off u16,
    //  print_len u16, flags u32, PathBuffer[] } }
    if out.len() < MAXIMUM_REPARSE_DATA_BUFFER_SIZE {
        return None;
    }
    out[..MAXIMUM_REPARSE_DATA_BUFFER_SIZE].fill(0);
    let path_off = 20usize; // 8 (hdr) + 12 (sym buffer)
    let sub_bytes = substitute.len() * 2;
    let print_bytes = print.len() * 2;
    let total = path_off + sub_bytes + print_bytes;
    if total > MAXIMUM_REPARSE_DATA_BUFFER_SIZE {
        return None;
    }
    // closures paramétrées par le buffer (pas de capture : deux closures
    // ne peuvent pas emprunter `out` de façon unique simultanément)
    let w = |out: &mut [u8], off: usize, v: u32| {
        out[off..off + 4].copy_from_slice(&v.to_le_bytes());
    };
    let w16 = |out: &mut [u8], off: usize, v: u16| {
        out[off..off + 2].copy_from_slice(&v.to_le_bytes());
    };
    w(out, 0, IO_REPARSE_TAG_SYMLINK);
    w16(out, 4, (total - 8) as u16); // ReparseDataLength (sans l'en-tête 8)
    w16(out, 8, 0); // sub offset (octets depuis PathBuffer)
    w16(out, 10, sub_bytes as u16);
    w16(out, 12, sub_bytes as u16); // print offset
    w16(out, 14, print_bytes as u16);
    w(out, 16, if relative { SYMLINK_FLAG_RELATIVE } else { 0 });
    for (i, wc) in substitute.iter().enumerate() {
        let o = path_off + i * 2;
        out[o..o + 2].copy_from_slice(&wc.to_le_bytes());
    }
    for (i, wc) in print.iter().enumerate() {
        let o = path_off + sub_bytes + i * 2;
        out[o..o + 2].copy_from_slice(&wc.to_le_bytes());
    }
    Some(total)
}

/// Décode un FSCTL_SET_REPARSE_POINT (IO_REPARSE_TAG_SYMLINK) :
/// (substitute UTF-16, relative)
pub fn parse_symlink_reparse(input: &[u8]) -> Option<(alloc::vec::Vec<u16>, bool)> {
    if input.len() < 20 {
        return None;
    }
    let rd = |off: usize| u32::from_le_bytes([input[off], input[off + 1], input[off + 2], input[off + 3]]);
    let r16 = |off: usize| u16::from_le_bytes([input[off], input[off + 1]]);
    let tag = rd(0);
    if tag != IO_REPARSE_TAG_SYMLINK {
        return None;
    }
    let sub_off = r16(8) as usize;
    let sub_len = r16(10) as usize; // octets
    let _print_off = r16(12) as usize;
    let flags = rd(16);
    let path_off = 20usize;
    let start = path_off + sub_off;
    if start + sub_len > input.len() || sub_len % 2 != 0 {
        return None;
    }
    let mut v = alloc::vec::Vec::with_capacity(sub_len / 2);
    for i in 0..sub_len / 2 {
        let o = start + i * 2;
        v.push(u16::from_le_bytes([input[o], input[o + 1]]));
    }
    Some((v, flags & SYMLINK_FLAG_RELATIVE != 0))
}

// ---------------------------------------------------------------------------
// NTSTATUS usuels (redondants avec wdk-sys mais autonomes)
// ---------------------------------------------------------------------------

pub mod st {
    pub const SUCCESS: i32 = 0x0000_0000u32 as i32;
    pub const UNSUCCESSFUL: i32 = 0xC000_0001u32 as i32;
    pub const INVALID_PARAMETER: i32 = 0xC000_000Du32 as i32;
    pub const INVALID_DEVICE_REQUEST: i32 = 0xC000_0010u32 as i32;
    pub const ACCESS_DENIED: i32 = 0xC000_0022u32 as i32;
    pub const BUFFER_TOO_SMALL: i32 = 0xC000_0023u32 as i32;
    pub const OBJECT_NAME_INVALID: i32 = 0xC000_0033u32 as i32;
    pub const OBJECT_NAME_NOT_FOUND: i32 = 0xC000_0034u32 as i32;
    pub const OBJECT_PATH_NOT_FOUND: i32 = 0xC000_003Au32 as i32;
    pub const OBJECT_NAME_COLLISION: i32 = 0xC000_0035u32 as i32;
    pub const END_OF_FILE: i32 = 0xC000_0011u32 as i32;
    pub const NO_MORE_FILES: i32 = 0x8000_0012u32 as i32;
    pub const NO_SUCH_FILE: i32 = 0xC000_000Fu32 as i32;
    pub const FILE_IS_A_DIRECTORY: i32 = 0xC000_00BAu32 as i32;
    pub const NOT_A_DIRECTORY: i32 = 0xC000_0103u32 as i32;
    pub const FILE_EXISTS: i32 = 0xC000_0050u32 as i32;
    pub const DEVICE_BUSY: i32 = 0xC000_0046u32 as i32;
    pub const MEDIA_WRITE_PROTECTED: i32 = 0xC000_00A2u32 as i32;
    pub const DIRECTORY_NOT_EMPTY: i32 = 0xC000_0101u32 as i32;
    pub const UNRECOGNIZED_VOLUME: i32 = 0xC000_014Fu32 as i32;
    pub const IO_REPARSE_TAG_NOT_HANDLED: i32 = 0xC000_0279u32 as i32;
    pub const REPARSE_ATTRIBUTE_CONFLICT: i32 = 0xC000_0275u32 as i32;
    pub const NOT_A_REPARSE_POINT: i32 = 0xC000_0270u32 as i32;
    pub const FILE_DELETED: i32 = 0xC000_0056u32 as i32;
    pub const LOCK_NOT_GRANTED: i32 = 0xC000_0055u32 as i32;
    pub const FILE_INVALID: i32 = 0xC000_0120u32 as i32;
    pub const CANT_ACCESS_FILE: i32 = 0xC000_0020u32 as i32;
    pub const NONEXISTENT_EA_ENTRY: i32 = 0xC000_0059u32 as i32;
    pub const PENDING: i32 = 0x0000_0103u32 as i32;
    pub const DEVICE_NOT_READY: i32 = 0xC000_00A3u32 as i32;
    pub const REPARSE: i32 = 0x0000_0104u32 as i32;
    pub const DELETE_PENDING: i32 = 0xC000_0056u32 as i32;
    pub const CANNOT_DELETE: i32 = 0xC000_0235u32 as i32;
    pub const INVALID_USER_BUFFER: i32 = 0xC000_00F0u32 as i32;
    pub const LOG_FILE_FULL: i32 = 0xC000_0219u32 as i32;
    pub const INTERNAL_ERROR: i32 = 0xC000_00E5u32 as i32;
    pub const DEVICE_DATA_ERROR: i32 = 0xC000_009Cu32 as i32;
    pub const SHARING_VIOLATION: i32 = 0xC000_0043u32 as i32;
    pub const BUFFER_OVERFLOW: i32 = 0x8000_0005u32 as i32;
    pub const OPLOCK_NOT_GRANTED: i32 = 0xC000_0088u32 as i32;
    pub const OPLOCK_BREAK_IN_PROGRESS: i32 = 0x0000_0108u32 as i32;
    // STATUS_NOT_SUPPORTED : valeur générique certaine, à affiner contre ntstatus.h
    pub const DATA_ENCRYPTION_UNSUPPORTED: i32 = 0xC000_00BBu32 as i32;
}
