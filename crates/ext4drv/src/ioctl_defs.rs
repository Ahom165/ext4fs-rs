//! Protocole IOCTL partagé entre le pilote `ext4drv` et l'outil `ext4ctl`.
//!
//! Toutes les structures sont #[repr(C)] sur des buffers METHOD_BUFFERED.
//! Les chaînes sont UTF-16LE sans terminateur (longueur en octets) pour
//! les entrées, UTF-8 pour les sorties (noms de fichiers ext4).

#![no_std]

/// FILE_DEVICE_UNKNOWN
pub const FILE_DEVICE_UNKNOWN: u32 = 0x0000_0022;
pub const METHOD_BUFFERED: u32 = 0;
pub const FILE_ANY_ACCESS: u32 = 0;
pub const FILE_READ_ACCESS: u32 = 0x0001;
pub const FILE_WRITE_ACCESS: u32 = 0x0002;

/// Équivalent Rust de la macro C CTL_CODE.
pub const fn ctl_code(device_type: u32, function: u32, method: u32, access: u32) -> u32 {
    (device_type << 16) | (access << 14) | (function << 2) | method
}

pub const IOCTL_EXT4_OPEN_IMAGE: u32 = ctl_code(FILE_DEVICE_UNKNOWN, 0x800, METHOD_BUFFERED, FILE_READ_ACCESS | FILE_WRITE_ACCESS);
pub const IOCTL_EXT4_CLOSE: u32 = ctl_code(FILE_DEVICE_UNKNOWN, 0x801, METHOD_BUFFERED, FILE_ANY_ACCESS);
pub const IOCTL_EXT4_VOLUME_INFO: u32 = ctl_code(FILE_DEVICE_UNKNOWN, 0x802, METHOD_BUFFERED, FILE_READ_ACCESS);
pub const IOCTL_EXT4_LIST_DIR: u32 = ctl_code(FILE_DEVICE_UNKNOWN, 0x803, METHOD_BUFFERED, FILE_READ_ACCESS);
pub const IOCTL_EXT4_READ_FILE: u32 = ctl_code(FILE_DEVICE_UNKNOWN, 0x804, METHOD_BUFFERED, FILE_READ_ACCESS);
pub const IOCTL_EXT4_WRITE_FILE: u32 = ctl_code(FILE_DEVICE_UNKNOWN, 0x805, METHOD_BUFFERED, FILE_WRITE_ACCESS);
pub const IOCTL_EXT4_CREATE_FILE: u32 = ctl_code(FILE_DEVICE_UNKNOWN, 0x806, METHOD_BUFFERED, FILE_WRITE_ACCESS);
pub const IOCTL_EXT4_MKDIR: u32 = ctl_code(FILE_DEVICE_UNKNOWN, 0x807, METHOD_BUFFERED, FILE_WRITE_ACCESS);
pub const IOCTL_EXT4_UNLINK: u32 = ctl_code(FILE_DEVICE_UNKNOWN, 0x808, METHOD_BUFFERED, FILE_WRITE_ACCESS);
pub const IOCTL_EXT4_RMDIR: u32 = ctl_code(FILE_DEVICE_UNKNOWN, 0x809, METHOD_BUFFERED, FILE_WRITE_ACCESS);
pub const IOCTL_EXT4_SET_LEN: u32 = ctl_code(FILE_DEVICE_UNKNOWN, 0x80A, METHOD_BUFFERED, FILE_WRITE_ACCESS);
pub const IOCTL_EXT4_STAT: u32 = ctl_code(FILE_DEVICE_UNKNOWN, 0x80B, METHOD_BUFFERED, FILE_READ_ACCESS);
pub const IOCTL_EXT4_FLUSH: u32 = ctl_code(FILE_DEVICE_UNKNOWN, 0x80C, METHOD_BUFFERED, FILE_WRITE_ACCESS);
pub const IOCTL_EXT4_SYMLINK: u32 = ctl_code(FILE_DEVICE_UNKNOWN, 0x80D, METHOD_BUFFERED, FILE_WRITE_ACCESS);

/// Nom du device de contrôle et de son lien Win32 (\\.\Ext4Fs).
pub const DEVICE_NAME: &[u16] = &[0x005C, 0x0044, 0x0065, 0x0076, 0x0069, 0x0063, 0x0065, 0x005C, 0x0045, 0x0078, 0x0074, 0x0034, 0x0046, 0x0073]; // \Device\Ext4Fs
pub const SYMLINK_NAME: &[u16] = &[0x005C, 0x0044, 0x006F, 0x0073, 0x0044, 0x0065, 0x0076, 0x0069, 0x0063, 0x0065, 0x0073, 0x005C, 0x0045, 0x0078, 0x0074, 0x0034, 0x0046, 0x0073]; // \DosDevices\Ext4Fs

// ---- Entrées ---------------------------------------------------------------

/// IOCTL_EXT4_OPEN_IMAGE : chemin UTF-16 + flags.
#[repr(C)]
pub struct OpenImageReq {
    pub path_len_bytes: u32, // longueur du chemin en OCTETS (sans terminateur)
    pub read_only: u32,      // 1 = montage lecture seule
    // suivi de: u8 path[path_len_bytes] en UTF-16LE
}

/// IOCTL_EXT4_CLOSE : id du volume.
#[repr(C)]
pub struct VolumeReq {
    pub volume_id: u32,
}

/// Chemin UTF-8 + données.
#[repr(C)]
pub struct PathReq {
    pub volume_id: u32,
    pub path_len_bytes: u32, // UTF-8
    // suivi de: u8 path[path_len_bytes]
}

#[repr(C)]
pub struct ReadFileReq {
    pub volume_id: u32,
    pub path_len_bytes: u32,
    pub offset: u64,
    pub length: u32,
    // suivi de: u8 path[path_len_bytes]
}

#[repr(C)]
pub struct WriteFileReq {
    pub volume_id: u32,
    pub path_len_bytes: u32,
    pub offset: u64,
    pub data_len: u32,
    // suivi de: u8 path[path_len_bytes] + u8 data[data_len]
}

#[repr(C)]
pub struct SetLenReq {
    pub volume_id: u32,
    pub path_len_bytes: u32,
    pub new_size: u64,
    // suivi de: u8 path[path_len_bytes]
}

#[repr(C)]
pub struct SymlinkReq {
    pub volume_id: u32,
    pub dir_len_bytes: u32,
    pub name_len_bytes: u32,
    pub target_len_bytes: u32,
    // suivi de: dir + name + target (UTF-8)
}

// ---- Sorties ---------------------------------------------------------------

/// En-tête de réponse générique : taille nécessaire si BUFFER_TOO_SMALL.
#[repr(C)]
pub struct ReplyHeader {
    pub needed_size: u32,
}

/// IOCTL_EXT4_VOLUME_INFO.
#[repr(C)]
pub struct VolumeInfoReply {
    pub volume_id: u32,
    pub block_size: u32,
    pub blocks_count: u64,
    pub free_blocks: u64,
    pub free_inodes: u32,
    pub read_only: u32,
    pub label_len: u32,
    pub uuid: [u8; 16],
    // suivi de: label (UTF-8)
}

/// Entrée de répertoire (liste allouée à plat).
#[repr(C)]
pub struct DirEntryWire {
    pub ino: u32,
    pub size: u64,
    pub mode: u16,
    pub kind: u8, // 1=reg 2=dir 7=symlink (convention ext4 FT_*)
    pub name_len: u16,
    // suivi de: nom UTF-8 (name_len octets), aligné à 4
}

#[repr(C)]
pub struct ListDirReply {
    pub count: u32,
    // suivi de: DirEntryWire[count] à plat
}

#[repr(C)]
pub struct StatReply {
    pub ino: u32,
    pub mode: u16,
    pub nlink: u16,
    pub size: u64,
    pub atime: u32,
    pub mtime: u32,
    pub ctime: u32,
    pub kind: u8,
    pub _pad: [u8; 3],
}
