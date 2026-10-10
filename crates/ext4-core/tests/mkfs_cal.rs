//! Tests de calibration du moteur mkfs ext4-core : l'image produite par
//! `ext4_core::mkfs` doit être PROPRE pour e2fsck (-fn), lisible ET
//! écrivable par debugfs (e2fsprogs 1.47.2 = juge de paix), et montable
//! par le moteur lui-même (superblock + GDT vérifiés, checksums inclus).
//! Lancer avec : cargo test -p ext4-core --features std-tests --test mkfs_cal

#![cfg(feature = "std-tests")]

mod common;
use common::{expected_content, make_local_file, tmp_img, E2Tools, FileDevice};
use ext4_core::mkfs::{mkfs, MkfsOptions};
use ext4_core::Filesystem;
use std::path::Path;

fn tools() -> E2Tools {
    E2Tools::detect().expect("e2fsprogs requis pour les tests mkfs")
}

/// Crée une image creuse de `mb` MiB et la formate avec NOTRE moteur.
fn mkfs_ours(img: &Path, mb: u64, opts: &MkfsOptions) {
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(img)
        .unwrap();
    f.set_len(mb * 1024 * 1024).unwrap();
    drop(f);
    let mut dev = FileDevice::open(img, false).unwrap();
    mkfs(&mut dev, opts, 1_700_000_000).expect("mkfs ext4-core doit réussir");
}

/// Montage lecture seule par NOTRE moteur : parse le superblock, vérifie
/// son checksum, charge et vérifie la GDT (checksums compris).
fn mount_ro(img: &Path) {
    let dev = Box::new(FileDevice::open(img, true).unwrap());
    let _fs = Filesystem::mount(dev, Box::new(ext4_core::util::FixedTime(1_700_000_000)), true)
        .expect("le moteur doit monter sa propre image");
}

// ---------------------------------------------------------------------------
// 1. Défauts modernes : 4K + journal 4 MiB, image mono-groupe 64 MiB
// ---------------------------------------------------------------------------

#[test]
fn mkfs_default_single_group() {
    let t = tools();
    let img = tmp_img("mkfs_ours_64m.img");
    mkfs_ours(&img, 64, &MkfsOptions::default());
    // e2fsck -fn : superblock, GDT, bitmaps+csums, tables d'inodes,
    // root/lost+found, journal jsb, metadata_csum — tout doit être propre.
    t.fsck_clean(&img);
    // le moteur accepte sa propre image (checksums sb/GDT revérifiés)
    mount_ro(&img);
    // debugfs lit la racine : lost+found présent
    let out = t.debugfs_ro(&img, &["ls -l /"]);
    assert!(out.contains("lost+found"), "racine sans lost+found : {out}");
}

// ---------------------------------------------------------------------------
// 2. Blocs 1K, sans journal, 32 MiB = 4 groupes (fdb=1, backup groupe 1)
// ---------------------------------------------------------------------------

#[test]
fn mkfs_1k_nojournal_multi_group() {
    let t = tools();
    let img = tmp_img("mkfs_ours_1k_32m.img");
    let opts = MkfsOptions {
        block_log: 0,
        journal_mib: 0,
        ..MkfsOptions::default()
    };
    mkfs_ours(&img, 32, &opts);
    t.fsck_clean(&img);
    mount_ro(&img);
    let out = t.debugfs_ro(&img, &["ls -l /"]);
    assert!(out.contains("lost+found"), "{out}");
}

// ---------------------------------------------------------------------------
// 3. 256 MiB multi-groupes avec sauvegardes sb+GDT (groupe 1), puis aller-
//    retour d'écriture PAR DEBUGFS (notre image doit rester cohérente pour
//    e2fsprogs et lisible par notre moteur après écriture externe).
// ---------------------------------------------------------------------------

#[test]
fn mkfs_multi_group_debugfs_roundtrip() {
    let t = tools();
    let img = tmp_img("mkfs_ours_256m.img");
    mkfs_ours(&img, 256, &MkfsOptions::default());
    // Valide notamment la sauvegarde sb+GDT du groupe 1 et le journal.
    t.fsck_clean(&img);
    mount_ro(&img);

    // debugfs écrit un fichier de 70 000 octets DANS notre image…
    let local = tmp_img("mkfs_local_seed.bin");
    make_local_file(&local, 70_000, 0x5A);
    let wcmd = format!("write {} /par_debugfs.bin", local.to_str().unwrap());
    t.debugfs(&img, &[wcmd.as_str()]);

    // … puis le relit octet pour octet (debugfs dump).
    let out_bin = tmp_img("mkfs_roundtrip_out.bin");
    let dcmd = format!("dump /par_debugfs.bin {}", out_bin.to_str().unwrap());
    t.debugfs_ro(&img, &[dcmd.as_str()]);
    assert_eq!(
        std::fs::read(&out_bin).unwrap(),
        expected_content(70_000, 0x5A),
        "contenu relu par debugfs != fichier écrit"
    );

    // Notre image reste PROPRE après écriture par e2fsprogs…
    t.fsck_clean(&img);
    // … et reste montable par notre moteur (extents écrits par debugfs).
    mount_ro(&img);
}

// ---------------------------------------------------------------------------
// 4. Label + pourcentage réservé personnalisés (16 MiB, mono-groupe 1K... 4K)
// ---------------------------------------------------------------------------

#[test]
fn mkfs_label_and_reserved_pct() {
    let t = tools();
    let img = tmp_img("mkfs_ours_label.img");
    let opts = MkfsOptions {
        label: b"test-vol".to_vec(),
        reserved_pct: 1,
        ..MkfsOptions::default()
    };
    mkfs_ours(&img, 16, &opts);
    t.fsck_clean(&img);
    let dump = t.dumpe2fs(&img);
    assert!(dump.contains("test-vol"), "label absent de dumpe2fs : {dump}");
    // 16 MiB / 4K = 4096 blocs → 1 % réservé = 40 blocs exactement.
    assert!(
        dump.contains("Reserved block count:     40"),
        "reserved_pct ignoré (attendu 40) : {dump}"
    );
}

// ---------------------------------------------------------------------------
// 5. Refus propre d'un volume trop petit (64 Ko < 128 blocs 4K)
// ---------------------------------------------------------------------------

#[test]
fn mkfs_rejects_tiny_volume() {
    let img = tmp_img("mkfs_too_small.img");
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&img)
        .unwrap();
    f.set_len(64 * 1024).unwrap();
    drop(f);
    let mut dev = FileDevice::open(&img, false).unwrap();
    let r = mkfs(&mut dev, &MkfsOptions::default(), 1_700_000_000);
    assert!(r.is_err(), "mkfs doit refuser un volume de 64 Ko");
}
