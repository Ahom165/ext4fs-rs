//! Calibration xattr : format de bloc EA (i_file_acl) et corps d'inode
//! (ibody) contre e2fsprogs 1.47.2. debugfs écrit, e2fsck juge, on décode.

#![cfg(feature = "std-tests")]

mod common;
use common::{tmp_img, E2Tools, FileDevice};
use ext4_core::blockdev::BlockDevice;
use ext4_core::crc::crc32c_update;
use ext4_core::fs::Filesystem;
use ext4_core::util::{rd16_le, rd32_le, FixedTime, TimeSource};
use std::path::Path;

struct TestTime(u32);
impl TimeSource for TestTime {
    fn now(&self) -> (u32, u32) {
        (self.0, 0)
    }
}

fn mount_ro(img: &Path) -> Filesystem {
    let dev = Box::new(FileDevice::open(img, false).unwrap());
    Filesystem::mount(dev, Box::new(FixedTime(0)), true).unwrap()
}

fn mount_rw(img: &Path) -> Filesystem {
    let dev = Box::new(FileDevice::open(img, false).unwrap());
    Filesystem::mount(dev, Box::new(FixedTime(1_700_000_000)), false).unwrap()
}

static IMG_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn make_image(tools: &E2Tools, name: &str) -> std::path::PathBuf {
    let n = IMG_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let img = tmp_img(&format!("{name}.{n}"));
    let _ = std::fs::remove_file(&img);
    std::fs::write(&img, vec![0u8; 32 * 1024 * 1024]).unwrap();
    tools.mkfs(&img, &["-F", "-q", "-b", "1024", "-I", "256"]);
    img
}

/// Entrée EA sur disque.
#[derive(Debug)]
struct EaEntry {
    name_index: u8,
    name: Vec<u8>,
    value_offs: u16,
    value_inum: u32,
    value_size: u32,
    hash: u32,
}

fn parse_entries(raw: &[u8], start: usize, end: usize) -> Vec<EaEntry> {
    let mut out = Vec::new();
    let mut off = start;
    while off + 4 <= end {
        let name_len = raw[off] as usize;
        if name_len == 0 {
            break;
        }
        out.push(EaEntry {
            name_index: raw[off + 1],
            name: raw[off + 16..off + 16 + name_len].to_vec(),
            value_offs: rd16_le(raw, off + 2),
            value_inum: rd32_le(raw, off + 4),
            value_size: rd32_le(raw, off + 8),
            hash: rd32_le(raw, off + 12),
        });
        off += (16 + name_len + 3) & !3;
    }
    out
}

/// Hash d'entrée (e2fsprogs ext2fs_ext_attr_hash_entry).
fn ea_entry_hash(name: &[u8], value: &[u8]) -> u32 {
    let mut hash: u32 = 0;
    for &b in name {
        hash = (hash << 5) ^ (hash >> 27) ^ b as u32;
    }
    if !value.is_empty() {
        let words = (value.len() + 3) >> 2;
        for i in 0..words {
            let base = i * 4;
            let mut w = [0u8; 4];
            for j in 0..4 {
                if base + j < value.len() {
                    w[j] = value[base + j];
                }
            }
            let v = u32::from_le_bytes(w);
            hash = (hash << 16) ^ (hash >> 16) ^ v;
        }
    }
    hash
}

/// Bloc EA dédié : user.big (700 o) dans le bloc, user.a (5 o) dans l'inode.
#[test]
fn xattr_reference_block_format() {
    let tools = E2Tools::detect().expect("e2fsprogs requis");
    let img = make_image(&tools, "xattr_cal_block.img");
    let big = "Z".repeat(700);
    tools.debugfs(
        &img,
        &[
            "write /etc/hostname /f",
            "ea_set /f user.a hello",
            &format!("ea_set /f user.big {big}"),
        ],
    );
    tools.fsck_clean(&img);

    let mut fs = mount_ro(&img);
    let inode = fs.resolve("/f").unwrap();
    let facl = inode.file_acl();
    assert_ne!(facl, 0, "EA dans un bloc dédié");
    let raw = fs.get_block(facl).unwrap().clone();
    let bs = fs.block_size() as usize;
    eprintln!("bloc EA @{} : {:02x?}", facl, &raw[..64]);

    assert_eq!(rd32_le(&raw, 0), 0xEA02_0000, "h_magic");
    assert_eq!(rd32_le(&raw, 4), 1, "h_refcount");
    assert_eq!(rd32_le(&raw, 8), 1, "h_blocks");
    let stored_csum = rd32_le(&raw, 16);

    let entries = parse_entries(&raw, 32, bs);
    eprintln!("entrées bloc : {entries:#?}");
    assert_eq!(entries.len(), 1, "seul user.big est dans le bloc");
    let e0 = &entries[0];
    assert_eq!(e0.name_index, 1, "namespace user.");
    assert_eq!(e0.name, b"big");
    assert_eq!(e0.value_size, 700);
    assert_eq!(e0.value_inum, 0, "pas d'EA-inode");
    let want_offs = (bs - 700) & !3;
    eprintln!("value_offs stocké={} ; attendu (fin de bloc, aligné 4)={want_offs}", e0.value_offs);
    let value = &raw[e0.value_offs as usize..e0.value_offs as usize + 700];
    assert!(value.iter().all(|&b| b == b'Z'));
    let h = ea_entry_hash(&e0.name, value);
    eprintln!("hash calculé={h:#010x} ; stocké={:#010x}", e0.hash);
    assert_eq!(h, e0.hash, "e_hash user.big");

    // ---- checksum de bloc : candidats (calibration) ----
    let raw0 = {
        let mut r = raw.clone();
        r[16..20].fill(0);
        r
    };
    let seed_sb = fs.sb.csum_seed;
    let seed_ino = ext4_core::dir::inode_csum_seed(&fs.sb, inode.no, inode.generation());
    let le_blk = facl.to_le_bytes();
    let cands: [(u32, &str); 6] = [
        (
            {
                let c = crc32c_update(seed_sb, &le_blk);
                crc32c_update(c, &raw0)
            },
            "A: seed_sb + le64(blk) + bloc entier",
        ),
        (
            {
                let c = crc32c_update(seed_sb, &le_blk);
                crc32c_update(c, &raw0[..32])
            },
            "B: seed_sb + le64(blk) + header 32o",
        ),
        (crc32c_update(seed_sb, &raw0), "C: seed_sb + bloc entier"),
        (
            {
                let c = crc32c_update(seed_ino, &le_blk);
                crc32c_update(c, &raw0)
            },
            "D: seed_ino + le64(blk) + bloc entier",
        ),
        (
            {
                let c = crc32c_update(seed_ino, &le_blk);
                crc32c_update(c, &raw0[..32])
            },
            "E: seed_ino + le64(blk) + header 32o",
        ),
        (crc32c_update(seed_ino, &raw0), "F: seed_ino + bloc entier"),
    ];
    match cands.iter().find(|(c, _)| *c == stored_csum) {
        Some((_, tag)) => eprintln!("CALIBRATION CHECKSUM : variante = {tag}"),
        None => panic!(
            "aucun candidat (stocké={stored_csum:#010x}, candidats={:?})",
            cands.iter().map(|(c, t)| (format!("{c:#010x}"), *t)).collect::<Vec<_>>()
        ),
    }
}

/// Corps d'inode (ibody) : magic 4 o à 0x80 + i_extra_isize, entrées
/// ensuite, valeurs en fin d'inode (croissance arrière).
#[test]
fn xattr_reference_ibody_format() {
    let tools = E2Tools::detect().expect("e2fsprogs requis");
    let img = make_image(&tools, "xattr_cal_ibody.img");
    tools.debugfs(
        &img,
        &[
            "write /etc/hostname /g",
            "ea_set /g user.small one",
            "ea_set /g user.tiny xy",
        ],
    );
    tools.fsck_clean(&img);

    let mut fs = mount_ro(&img);
    let inode = fs.resolve("/g").unwrap();
    assert_eq!(inode.file_acl(), 0, "pas de bloc EA dédié");
    let isz = inode.raw.len();
    let extra = inode.extra_isize() as usize;
    eprintln!("ibody : isz={isz} extra_isize={extra:#x} raw fin={:02x?}", &inode.raw[isz - 16..isz]);

    let magic = rd32_le(&inode.raw, 0x80 + extra);
    eprintln!("magic @0x{:x} = {magic:#010x}", 0x80 + extra);
    assert_eq!(magic, 0xEA02_0000, "IHDR à 0x80 + i_extra_isize");

    let entries = parse_entries(&inode.raw, 0x80 + extra + 4, isz);
    eprintln!("entrées ibody : {entries:#?}");
    assert_eq!(entries.len(), 2, "user.small + user.tiny");
    // valeurs : arrière depuis la fin d'inode, ordre d'insertion inversé
    // (dernier inséré = le plus proche de la fin), alignées 4
    let tail = &inode.raw[isz - 8..isz];
    eprintln!("8 derniers o : {tail:02x?}");
    let v = &inode.raw[isz - 4..isz - 1];
    eprintln!("valeur à isz-4 : {:02x?}", v);
    assert_eq!(v, b"one", "user.small (inséré en premier) à isz-4");
    let v2 = &inode.raw[isz - 8..isz - 6];
    assert_eq!(v2, b"xy", "user.tiny (inséré en second) à isz-8");
    // hash d'entrée
    // e2fsprogs ne calcule PAS les hash d'entrée ibody (calibré : e_hash=0)
    assert_eq!(entries[0].hash, 0, "hash d'entrée ibody = 0");
}

/// Aller-retour : écriture par NOTRE moteur, jugée par e2fsprogs.
/// set (ibody + bloc) → e2fsck -fn propre → debugfs ea_get roundtrip
/// → remove → e2fsck propre. Calibration permanente contre 1.47.2.
#[test]
fn xattr_roundtrip_written_by_engine() {
    let tools = E2Tools::detect().expect("e2fsprogs requis");
    let img = make_image(&tools, "xattr_rt.img");
    let big = "W".repeat(700);

    // image peuplée par debugfs (fichier + répertoire)
    tools.debugfs(
        &img,
        &[
            "write /etc/hostname /f",
            "mkdir /d",
            "write /etc/hosts /d/h",
        ],
    );
    tools.fsck_clean(&img);

    // ---- écriture par notre moteur ----
    {
        let mut fs = mount_rw(&img);
        let ino_f = fs.resolve("/f").unwrap().no;
        let ino_d = fs.resolve("/d").unwrap().no;
        ext4_core::xattr::set_xattr(&mut fs, ino_f, 1, b"small", b"one").unwrap();
        ext4_core::xattr::set_xattr(&mut fs, ino_f, 1, b"big", big.as_bytes()).unwrap();
        ext4_core::xattr::set_xattr(&mut fs, ino_d, 1, b"dir.a", b"xy").unwrap();
        // remplacement
        ext4_core::xattr::set_xattr(&mut fs, ino_f, 1, b"small", b"TWO").unwrap();
        fs.unmount().unwrap();
    }
    tools.fsck_clean(&img);

    // ---- relecture par debugfs (juge) ----
    let out = tools.debugfs_ro(
        &img,
        &["ea_get /f user.small", "ea_get /f user.big", "ea_get /d user.dir.a"],
    );
    assert!(out.contains("TWO"), "user.small remplacé : {out}");
    assert!(out.contains(&big), "user.big 700 o roundtrip");
    assert!(out.contains("xy"), "user.dir.a sur répertoire");

    // ---- relecture par notre moteur ----
    {
        let mut fs = mount_ro(&img);
        let ino_f = fs.resolve("/f").unwrap().no;
        let v = ext4_core::xattr::get_xattr(&mut fs, ino_f, 1, b"small").unwrap();
        assert_eq!(v.as_deref(), Some(&b"TWO"[..]));
        let v2 = ext4_core::xattr::get_xattr(&mut fs, ino_f, 1, b"big").unwrap();
        assert_eq!(v2.as_deref(), Some(big.as_bytes()));
        let list = ext4_core::xattr::list_xattrs(&mut fs, ino_f).unwrap();
        assert_eq!(list.len(), 2);
    }

    // ---- suppression ----
    {
        let mut fs = mount_rw(&img);
        let ino_f = fs.resolve("/f").unwrap().no;
        ext4_core::xattr::remove_xattr(&mut fs, ino_f, 1, b"big").unwrap();
        fs.unmount().unwrap();
    }
    tools.fsck_clean(&img);
    let out = tools.debugfs_ro(&img, &["ea_list /f"]);
    assert!(out.contains("user.small"), "user.small reste : {out}");
    assert!(!out.contains("user.big"), "user.big supprimé");
    let v = {
        let mut fs = mount_ro(&img);
        let ino_f = fs.resolve("/f").unwrap().no;
        ext4_core::xattr::get_xattr(&mut fs, ino_f, 1, b"big").unwrap()
    };
    assert!(v.is_none(), "user.big absent après remove");
}

/// Aller-retour : écriture PAR NOTRE MOTEUR, jugée par e2fsprogs.
/// set (ibody + bloc) → e2fsck -fn propre → debugfs ea_get roundtrip
/// → remove → e2fsck propre → ea_get absent.
#[test]
fn xattr_write_path_roundtrip() {
    let tools = E2Tools::detect().expect("e2fsprogs requis");
    let img = make_image(&tools, "xattr_rt.img");
    let big = "Z".repeat(700);

    // fichier créé par debugfs, xattrs posés par NOTRE moteur
    tools.debugfs(&img, &["write /etc/hostname /f"]);
    {
        let dev = Box::new(FileDevice::open(&img, false).unwrap());
        let mut fs = Filesystem::mount(dev, Box::new(FixedTime(1_700_000_000)), false).unwrap();
        let ino = fs.resolve("/f").unwrap();
        ext4_core::xattr::set_xattr(&mut fs, ino.no, 1, b"a", b"one").unwrap();
        ext4_core::xattr::set_xattr(&mut fs, ino.no, 1, b"big", big.as_bytes()).unwrap();
        fs.unmount().unwrap();
    }
    tools.fsck_clean(&img);

    // ---- relecture par debugfs (juge de paix) ----
    let out = tools.debugfs_ro(&img, &["ea_list /f"]);
    assert!(out.contains("user.a"), "user.a lu par debugfs : {out}");
    assert!(out.contains("user.big"), "user.big lu par debugfs : {out}");

    // ---- relecture par notre moteur ----
    {
        let mut fs = mount_ro(&img);
        let ino = fs.resolve("/f").unwrap();
        let v1 = ext4_core::xattr::get_xattr(&mut fs, ino.no, 1, b"a").unwrap();
        assert_eq!(v1.as_deref(), Some(&b"one"[..]), "user.a relit (ibody)");
        let v2 = ext4_core::xattr::get_xattr(&mut fs, ino.no, 1, b"big").unwrap();
        assert_eq!(v2.as_ref().unwrap().len(), 700, "user.big relit (bloc)");
        assert!(v2.unwrap().iter().all(|&b| b == b'Z'));
    }

    // ---- suppression ----
    {
        let dev = Box::new(FileDevice::open(&img, false).unwrap());
        let mut fs = Filesystem::mount(dev, Box::new(FixedTime(1_700_000_000)), false).unwrap();
        let ino = fs.resolve("/f").unwrap();
        ext4_core::xattr::remove_xattr(&mut fs, ino.no, 1, b"big").unwrap();
        fs.unmount().unwrap();
    }
    tools.fsck_clean(&img);
    let out = tools.debugfs_ro(&img, &["ea_list /f"]);
    assert!(out.contains("user.a"), "user.a reste : {out}");
    assert!(!out.contains("user.big"), "user.big supprimé : {out}");
}
