//! # ext4-core
//!
//! Moteur ext4 pur Rust, `no_std` + `alloc`, conçu pour être embarqué à la
//! fois dans un pilote kernel Windows (wdk-rs) et dans des outils user-mode
//! (CLI, tests). Lecture ET écriture : superblock, GDT 32/64 octets,
//! checksums metadata_csum, inodes, extents, répertoires linéaires et HTree,
//! inline_data, journal jbd2 (replay + transactions écrites).
//!
//! Points d'entrée : [`Filesystem::mount`] puis les méthodes haut niveau
//! (`read_dir`, `lookup`, `read_file`, `write_file`, `create_file`, ...).

#![cfg_attr(not(test), no_std)]
#![deny(unsafe_op_in_unsafe_fn)]
extern crate alloc;

pub mod bitmap;
pub mod blockdev;
pub mod crc;
pub mod dir;
pub mod error;
pub mod extent;
pub mod fs;
pub mod fsops;
pub mod gdt;
pub mod inode;
pub mod journal;
pub mod mkfs;
pub mod superblock;
pub mod util;
pub mod xattr;

pub use blockdev::BlockDevice;
pub use error::{Error, Result};
pub use fs::Filesystem;
pub use superblock::Superblock;
pub use util::TimeSource;

/// Numéros d'inodes réservés.
pub const INO_ROOT: u32 = 2;
pub const INO_JOURNAL: u32 = 8;
pub const INO_RESIZE: u32 = 7;
