//! Tests d'intégration ext4-core : vraies images mkfs.ext4 + validation
//! par e2fsck/debugfs. Lancer avec : cargo test -p ext4-core --features std-tests

#![cfg(feature = "std-tests")]

mod common;
use common::{expected_content, make_local_file, tmp_img, E2Tools, FileDevice};
use ext4_core::blockdev::BlockDevice;
use ext4_core::fs::Filesystem;
use ext4_core::inode::FileType;
use ext4_core::util::{FixedTime, TimeSource};
use std::path::{Path, PathBuf};

struct TestTime(u32);
impl TimeSource for TestTime {
    fn now(&self) -> (u32, u32) {
        (self.0, 0)
    }
}

fn tools() -> E2Tools {
    E2Tools::detect().expect("e2fsprogs requis pour les tests d'intégration")
}

fn mount_rw(img: &Path) -> Filesystem {
    let dev = Box::new(FileDevice::open(img, false).unwrap());
    Filesystem::mount(dev, Box::new(TestTime(1_700_000_000)), false).unwrap()
}

fn mount_ro(img: &Path) -> Filesystem {
    // device ouvert en écriture (le replay du journal écrit), montage RO
    let dev = Box::new(FileDevice::open(img, false).unwrap());
    Filesystem::mount(dev, Box::new(FixedTime(0)), true).unwrap()
}

/// Image standard : mkfs.ext4 défauts modernes (metadata_csum, 64bit).
fn make_basic_image(tools: &E2Tools, name: &str, mb: usize) -> PathBuf {
    let img = tmp_img(name);
    let _ = std::fs::remove_file(&img);
    std::fs::write(&img, vec![0u8; mb * 1024 * 1024]).unwrap();
    tools.mkfs(&img, &["-F", "-q"]);
    img
}

/// Peuple une image via debugfs : fichiers, arborescence, liens.
fn populate_basic(tools: &E2Tools, img: &Path) {
    let tmpdir = std::env::temp_dir();
    let f_small = tmpdir.join("ext4rs_small.bin");
    let f_mid = tmpdir.join("ext4rs_mid.bin");
    let f_big = tmpdir.join("ext4rs_big.bin");
    make_local_file(&f_small, 100, 0x11);
    make_local_file(&f_mid, 20_000, 0x22);
    make_local_file(&f_big, 300_000, 0x33);
    tools.debugfs(
        img,
        &[
            "mkdir /docs",
            "mkdir /docs/notes",
            &format!("write {} /hello.txt", f_small.to_str().unwrap()),
            &format!("write {} /mid.bin", f_mid.to_str().unwrap()),
            &format!("write {} /docs/big.bin", f_big.to_str().unwrap()),
            &format!("write {} /docs/notes/data.bin", f_mid.to_str().unwrap()),
            "symlink /link-to-hello /hello.txt",
            "ln /hello.txt /hello-hardlink",
            "sif /hello.txt links_count 2",
            "mkdir /répertoire_éàü",
            &format!("write {} /répertoire_éàü/fichier_ümlaut.txt", f_small.to_str().unwrap()),
        ],
    );
    let _ = std::fs::remove_file(&f_small);
    let _ = std::fs::remove_file(&f_mid);
    let _ = std::fs::remove_file(&f_big);
}

// ---------------------------------------------------------------------------
// 1. Parse & lecture pure
// ---------------------------------------------------------------------------

#[test]
fn test_mount_and_info() {
    let tools = tools();
    let img = make_basic_image(&tools, "info.img", 32);
    populate_basic(&tools, &img);
    let mut fs = mount_ro(&img);
    let info = fs.volume_info().unwrap();
    assert!(info.contains("ext4"), "{info}");
    // comparaison avec dumpe2fs
    let dump = tools.dumpe2fs(&img);
    let dump_blocks = dump
        .lines()
        .find(|l| l.contains("Block count:"))
        .and_then(|l| l.split(':').nth(1))
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap();
    assert_eq!(fs.sb.blocks_count, dump_blocks);
    let dump_inodes = dump
        .lines()
        .find(|l| l.contains("Inode count:"))
        .and_then(|l| l.split(':').nth(1))
        .and_then(|s| s.trim().parse::<u32>().ok())
        .unwrap();
    assert_eq!(fs.sb.inodes_count, dump_inodes);
    let dump_free_ino = dump
        .lines()
        .find(|l| l.contains("Free inodes:"))
        .and_then(|l| l.split(':').nth(1))
        .and_then(|s| s.trim().parse::<u32>().ok())
        .unwrap();
    assert_eq!(fs.sb.free_inodes_count, dump_free_ino, "compteur inodes libres");
    tools.fsck_clean(&img);
}

#[test]
fn test_read_dir_and_lookup() {
    let tools = tools();
    let img = make_basic_image(&tools, "listdir.img", 32);
    populate_basic(&tools, &img);
    let mut fs = mount_ro(&img);
    let root = fs.read_inode(ext4_core::INO_ROOT).unwrap();
    let entries = fs.read_dir(&root).unwrap();
    let names: Vec<String> = entries
        .iter()
        .map(|e| String::from_utf8_lossy(&e.name).to_string())
        .collect();
    for expected in ["docs", "hello.txt", "hello-hardlink", "link-to-hello", "répertoire_éàü", "lost+found"] {
        assert!(names.contains(&expected.to_string()), "{expected:?} absent de {names:?}");
    }
    // lookup
    let (ino, ft) = fs.lookup(&root, b"docs").unwrap().unwrap();
    assert_eq!(ft, FileType::Dir);
    let docs = fs.read_inode(ino).unwrap();
    let sub = fs.read_dir(&docs).unwrap();
    let subnames: Vec<String> = sub
        .iter()
        .map(|e| String::from_utf8_lossy(&e.name).to_string())
        .collect();
    assert!(subnames.contains(&"big.bin".to_string()), "{subnames:?}");
    assert!(subnames.contains(&"notes".to_string()), "{subnames:?}");
    // résolution de chemin
    let f = fs.resolve("/docs/big.bin").unwrap();
    assert_eq!(f.file_type(), Some(FileType::Regular));
    assert_eq!(f.size(), 300_000);
}

#[test]
fn test_read_files_content() {
    let tools = tools();
    let img = make_basic_image(&tools, "read.img", 32);
    populate_basic(&tools, &img);
    let mut fs = mount_ro(&img);
    // petit fichier
    let hello = fs.resolve("/hello.txt").unwrap();
    let data = fs.read_file_to_vec(hello.no, 0, 200).unwrap();
    assert_eq!(data, expected_content(100, 0x11));
    // fichier multi-blocs (20k > 4k)
    let mid = fs.resolve("/mid.bin").unwrap();
    let data = fs.read_file_to_vec(mid.no, 0, 100_000).unwrap();
    assert_eq!(data, expected_content(20_000, 0x22));
    // gros fichier avec arbre d'extents (300k)
    let big = fs.resolve("/docs/big.bin").unwrap();
    let data = fs.read_file_to_vec(big.no, 0, 400_000).unwrap();
    assert_eq!(data, expected_content(300_000, 0x33));
    // lecture partielle au milieu
    let mid_part = fs.read_file_to_vec(mid.no, 5_000, 1_000).unwrap();
    assert_eq!(mid_part, expected_content(20_000, 0x22)[5_000..6_000].to_vec());
    // symlink
    let link = fs.resolve("/link-to-hello").unwrap();
    assert_eq!(link.file_type(), Some(FileType::Symlink));
    let target = fs.readlink(link.no).unwrap();
    assert_eq!(target, b"/hello.txt");
    // hardlink : même inode
    let h1 = fs.resolve("/hello.txt").unwrap();
    let h2 = fs.resolve("/hello-hardlink").unwrap();
    assert_eq!(h1.no, h2.no);
    assert_eq!(h1.links_count(), 2);
}

#[test]
fn test_blocksize_1024() {
    let tools = tools();
    let img = tmp_img("bs1k.img");
    let _ = std::fs::remove_file(&img);
    std::fs::write(&img, vec![0u8; 32 * 1024 * 1024]).unwrap();
    tools.mkfs(&img, &["-F", "-q", "-b", "1024"]);
    let tmpdir = std::env::temp_dir();
    let f = tmpdir.join("ext4rs_1k.bin");
    make_local_file(&f, 9_000, 0x44);
    tools.debugfs(&img, &[&format!("write {} /file1k.bin", f.to_str().unwrap())]);
    let _ = std::fs::remove_file(&f);
    let mut fs = mount_ro(&img);
    assert_eq!(fs.sb.block_size, 1024);
    let ino = fs.resolve("/file1k.bin").unwrap();
    let data = fs.read_file_to_vec(ino.no, 0, 100_000).unwrap();
    assert_eq!(data, expected_content(9_000, 0x44));
}

// ---------------------------------------------------------------------------
// 2. Écriture + validation e2fsck
// ---------------------------------------------------------------------------

#[test]
fn test_write_create_and_verify() {
    let tools = tools();
    let img = make_basic_image(&tools, "write1.img", 32);
    populate_basic(&tools, &img);
    {
        let mut fs = mount_rw(&img);
        let ino = fs.create_file("/", "created.txt").unwrap();
        fs.write_file(ino, 0, b"Bonjour ext4 depuis Rust !").unwrap();
        // relecture immédiate
        let data = fs.read_file_to_vec(ino, 0, 1000).unwrap();
        assert_eq!(data, b"Bonjour ext4 depuis Rust !");
        fs.unmount().unwrap();
    }
    tools.fsck_clean(&img);
    // vérification via debugfs (lecture par e2fsprogs)
    let out = tools.debugfs_ro(&img, &["cat /created.txt"]);
    assert!(
        out.contains("Bonjour ext4 depuis Rust !"),
        "contenu debugfs: {out}"
    );
}

#[test]
fn test_write_large_multi_extent() {
    let tools = tools();
    let img = make_basic_image(&tools, "writе_big.img", 64);
    let payload: Vec<u8> = (0..256_000u32)
        .map(|i| (i as u8).wrapping_mul(0x9D) ^ (i >> 7) as u8)
        .collect();
    {
        let mut fs = mount_rw(&img);
        let ino = fs.create_file("/", "large.bin").unwrap();
        fs.write_file(ino, 0, &payload).unwrap();
        let back = fs.read_file_to_vec(ino, 0, payload.len() + 10).unwrap();
        assert_eq!(back, payload);
        fs.unmount().unwrap();
    }
    tools.fsck_clean(&img);
    // vérification debugfs via dump
    let out = tools.debugfs_ro(&img, &["dump /large.bin /tmp/ext4rs_large_out.bin"]);
    let _ = out;
    let dumped = std::fs::read("/tmp/ext4rs_large_out.bin").unwrap();
    assert_eq!(dumped.len(), payload.len());
    assert_eq!(dumped, payload);
}

#[test]
fn test_overwrite_middle() {
    let tools = tools();
    let img = make_basic_image(&tools, "overwr.img", 32);
    populate_basic(&tools, &img);
    {
        let mut fs = mount_rw(&img);
        let ino = fs.resolve("/mid.bin").unwrap().no;
        // écrase le milieu (blocs existants)
        fs.write_file(ino, 10_000, b"ECRASED-MIDDLE").unwrap();
        let back = fs.read_file_to_vec(ino, 0, 100_000).unwrap();
        let mut want = expected_content(20_000, 0x22);
        want[10_000..10_000 + 14].copy_from_slice(b"ECRASED-MIDDLE");
        assert_eq!(back, want);
        fs.unmount().unwrap();
    }
    tools.fsck_clean(&img);
}

#[test]
fn test_append_grow() {
    let tools = tools();
    let img = make_basic_image(&tools, "append.img", 32);
    populate_basic(&tools, &img);
    {
        let mut fs = mount_rw(&img);
        let ino = fs.resolve("/hello.txt").unwrap().no;
        fs.write_file(ino, 100, b"SUITE-APPENDEE").unwrap();
        let back = fs.read_file_to_vec(ino, 0, 1000).unwrap();
        assert_eq!(&back[..100], &expected_content(100, 0x11)[..]);
        assert_eq!(&back[100..114], b"SUITE-APPENDEE");
        assert_eq!(back.len(), 114);
        fs.unmount().unwrap();
    }
    tools.fsck_clean(&img);
}

#[test]
fn test_truncate_shrink() {
    let tools = tools();
    let img = make_basic_image(&tools, "trunc.img", 32);
    populate_basic(&tools, &img);
    {
        let mut fs = mount_rw(&img);
        let ino = fs.resolve("/docs/big.bin").unwrap().no;
        fs.set_len(ino, 1_234).unwrap();
        let back = fs.read_file_to_vec(ino, 0, 10_000).unwrap();
        assert_eq!(back, expected_content(1_234, 0x33));
        fs.unmount().unwrap();
    }
    tools.fsck_clean(&img);
}

#[test]
fn test_mkdir_unlink_rmdir() {
    let tools = tools();
    let img = make_basic_image(&tools, "nsops.img", 32);
    populate_basic(&tools, &img);
    {
        let mut fs = mount_rw(&img);
        // mkdir + fichier dedans
        fs.mkdir("/docs", "nouveau").unwrap();
        let ino = fs.create_file("/docs/nouveau", "f.txt").unwrap();
        fs.write_file(ino, 0, b"data").unwrap();
        // suppression
        fs.unlink("/docs/nouveau/f.txt").unwrap();
        fs.rmdir("/docs/nouveau").unwrap();
        // suppression du gros fichier
        fs.unlink("/docs/big.bin").unwrap();
        assert!(fs.resolve("/docs/big.bin").is_err());
        // rmdir non vide refusé
        assert!(fs.rmdir("/docs").is_err());
        fs.unmount().unwrap();
    }
    tools.fsck_clean(&img);
    let out = tools.debugfs_ro(&img, &["ls -l /docs"]);
    assert!(!out.contains("nouveau"), "répertoire encore présent: {out}");
    assert!(!out.contains("big.bin"), "fichier encore présent: {out}");
}

#[test]
fn test_hardlink_and_symlink_create() {
    let tools = tools();
    let img = make_basic_image(&tools, "links.img", 32);
    populate_basic(&tools, &img);
    {
        let mut fs = mount_rw(&img);
        fs.link("/mid.bin", "/", "mid-lien.bin").unwrap();
        let a = fs.resolve("/mid.bin").unwrap();
        let b = fs.resolve("/mid-lien.bin").unwrap();
        assert_eq!(a.no, b.no);
        assert_eq!(a.links_count(), 2);
        fs.symlink("/", "lien-rapide", "docs/notes").unwrap();
        let s = fs.resolve("/lien-rapide").unwrap();
        assert_eq!(fs.readlink(s.no).unwrap(), b"docs/notes");
        fs.unmount().unwrap();
    }
    tools.fsck_clean(&img);
    let out = tools.debugfs_ro(&img, &["stat /lien-rapide"]);
    assert!(out.contains("Type: symlink"), "{out}");
}

#[test]
fn test_write_1k_blocks() {
    let tools = tools();
    let img = tmp_img("write1k.img");
    let _ = std::fs::remove_file(&img);
    std::fs::write(&img, vec![0u8; 32 * 1024 * 1024]).unwrap();
    tools.mkfs(&img, &["-F", "-q", "-b", "1024"]);
    {
        let mut fs = mount_rw(&img);
        let ino = fs.create_file("/", "f1k.txt").unwrap();
        let payload: Vec<u8> = (0..50_000u32).map(|i| (i * 7 % 251) as u8).collect();
        fs.write_file(ino, 0, &payload).unwrap();
        let back = fs.read_file_to_vec(ino, 0, 60_000).unwrap();
        assert_eq!(back, payload);
        fs.unmount().unwrap();
    }
    tools.fsck_clean(&img);
}

// ---------------------------------------------------------------------------
// 3. Journal : crash simulé + replay (mon replay vs e2fsck)
// ---------------------------------------------------------------------------

#[test]
fn test_journal_replay_after_crash() {
    let tools = tools();
    let img = make_basic_image(&tools, "crash.img", 32);
    populate_basic(&tools, &img);
    // 1. écriture avec crash simulé (journal armé, pas de finalize)
    {
        let mut fs = mount_rw(&img);
        let ino = fs.create_file("/", "crash-file.txt").unwrap();
        fs.write_file(ino, 0, b"ECRIT-AVANT-CRASH-1234567890").unwrap();
        fs.commit_crash_simulate().unwrap();
        // pas de unmount : le "crash" survient ici
    }
    let img_copy_a = tmp_img("crash_a.img");
    let img_copy_b = tmp_img("crash_b.img");
    std::fs::copy(&img, &img_copy_a).unwrap();
    std::fs::copy(&img, &img_copy_b).unwrap();

    // 2a. MON replay doit retrouver le fichier
    {
        let mut fs = mount_rw(&img_copy_a);
        let ino = fs.resolve("/crash-file.txt").unwrap();
        let data = fs.read_file_to_vec(ino.no, 0, 100).unwrap();
        assert_eq!(data, b"ECRIT-AVANT-CRASH-1234567890");
        fs.unmount().unwrap();
    }
    tools.fsck_clean(&img_copy_a);

    // 2b. e2fsck (replay noyau) doit aussi retrouver le fichier
    tools.fsck_replay(&img_copy_b);
    let out = tools.debugfs_ro(&img_copy_b, &["cat /crash-file.txt"]);
    assert!(
        out.contains("ECRIT-AVANT-CRASH-1234567890"),
        "replay e2fsck: {out}"
    );
    tools.fsck_clean(&img_copy_b);
}

#[test]
fn test_journal_multi_txn_chunks() {
    let tools = tools();
    let img = make_basic_image(&tools, "chunks.img", 32);
    // écrit un fichier assez gros pour plusieurs transactions (chunk = 512 blocs max)
    {
        let mut fs = mount_rw(&img);
        let ino = fs.create_file("/", "chunked.bin").unwrap();
        let payload: Vec<u8> = (0..600_000u32).map(|i| (i % 251) as u8).collect();
        fs.write_file(ino, 0, &payload).unwrap();
        fs.commit_crash_simulate().unwrap();
    }
    // e2fsck rejoue tous les chunks
    tools.fsck_replay(&img);
    let out = tools.debugfs_ro(&img, &["dump /chunked.bin /tmp/ext4rs_chunk_out.bin"]);
    let _ = out;
    let dumped = std::fs::read("/tmp/ext4rs_chunk_out.bin").unwrap();
    let payload: Vec<u8> = (0..600_000u32).map(|i| (i % 251) as u8).collect();
    assert_eq!(dumped.len(), payload.len());
    assert_eq!(dumped, payload);
    tools.fsck_clean(&img);
}

// ---------------------------------------------------------------------------
// 4. inline_data
// ---------------------------------------------------------------------------

#[test]
fn test_inline_data_lifecycle() {
    let tools = tools();
    let img = tmp_img("inline.img");
    let _ = std::fs::remove_file(&img);
    std::fs::write(&img, vec![0u8; 32 * 1024 * 1024]).unwrap();
    tools.mkfs(&img, &["-F", "-q", "-O", "inline_data"]);
    {
        let mut fs = mount_rw(&img);
        let ino = fs.create_file("/", "tiny.txt").unwrap();
        fs.write_file(ino, 0, b"INLINE!").unwrap();
        // reste inline : relecture
        let data = fs.read_file_to_vec(ino, 0, 100).unwrap();
        assert_eq!(data, b"INLINE!");
        // conversion en extents au-delà de 60 octets
        fs.write_file(ino, 6, &[b'X'; 200]).unwrap();
        let data2 = fs.read_file_to_vec(ino, 0, 1000).unwrap();
        // l'écriture à l'offset 6 écrase le '!' (comportement attendu)
        let mut want = b"INLINE".to_vec();
        want.extend_from_slice(&[b'X'; 200]);
        assert_eq!(data2, want);
        fs.unmount().unwrap();
    }
    tools.fsck_clean(&img);
    // e2fsprogs lit le fichier inline créé par mon moteur
    let out = tools.debugfs_ro(&img, &["cat /tiny.txt"]);
    assert!(out.contains("INLINEX"), "cat inline: {out}");
}

// ---------------------------------------------------------------------------
// 5. HTree : arbre construit par e2fsck -D (créateur Linux), mes insertions
// ---------------------------------------------------------------------------

#[test]
fn test_htree_read_and_insert() {
    let tools = tools();
    let img = make_basic_image(&tools, "htree.img", 64);
    // 8000 fichiers dans un même répertoire (script debugfs unique)
    let tmpdir = std::env::temp_dir();
    let f = tmpdir.join("ext4rs_htree_seed.bin");
    make_local_file(&f, 50, 0x55);
    let mut cmds = vec!["mkdir /many".to_string()];
    for i in 0..8000 {
        cmds.push(format!(
            "write {} /many/file_{:05}.dat",
            f.to_str().unwrap(),
            i
        ));
    }
    let cmds: Vec<&str> = cmds.iter().map(|s| s.as_str()).collect();
    tools.debugfs(&img, &cmds);
    let _ = std::fs::remove_file(&f);
    // e2fsck -fD : reconstruit les répertoires → crée le HTree indexé
    {
        let out = tools.fsck_optimize(&img);
        let _ = out;
    }
    // vérifie l'indexation via mon moteur (EXT4_INDEX_FL = 0x1000)
    {
        let mut fs = mount_ro(&img);
        let dir = fs.resolve("/many").unwrap();
        assert!(
            dir.flags() & 0x1000 != 0,
            "le répertoire devrait être indexé après -D (flags {:x})",
            dir.flags()
        );
    }
    // lecture + lookup dans l'arbre
    {
        let mut fs = mount_ro(&img);
        let dir = fs.resolve("/many").unwrap();
        let entries = fs.read_dir(&dir).unwrap();
        assert_eq!(entries.len(), 8002); // 8000 fichiers + "." + ".."
        let hit = fs.lookup(&dir, b"file_07777.dat").unwrap();
        assert!(hit.is_some(), "lookup htree");
        let _ = hit.unwrap();
    }
    // insertion dans une feuille htree + validation e2fsck
    {
        let mut fs = mount_rw(&img);
        fs.create_file("/many", "zzz-nouveau.dat").unwrap();
        fs.unmount().unwrap();
    }
    tools.fsck_clean(&img);
    {
        let mut fs = mount_ro(&img);
        let dir = fs.resolve("/many").unwrap();
        let hit = fs.lookup(&dir, b"zzz-nouveau.dat").unwrap();
        assert!(hit.is_some(), "entrée insérée introuvable dans le htree");
    }
}

// ---------------------------------------------------------------------------
// 6. Divers / robustesse
// ---------------------------------------------------------------------------

#[test]
fn test_readonly_mount_refuses_write() {
    let tools = tools();
    let img = make_basic_image(&tools, "ro.img", 32);
    populate_basic(&tools, &img);
    let mut fs = mount_ro(&img);
    assert!(fs.create_file("/", "nope.txt").is_err());
    let ino = fs.resolve("/hello.txt").unwrap().no;
    assert!(fs.write_file(ino, 0, b"x").is_err());
}

#[test]
fn test_bad_magic_rejected() {
    let img = tmp_img("garbage.img");
    std::fs::write(&img, vec![0xEEu8; 1024 * 1024]).unwrap();
    let dev = Box::new(FileDevice::open(&img, false).unwrap());
    let err = match Filesystem::mount(dev, Box::new(FixedTime(0)), true) {
        Ok(_) => panic!("le montage devait échouer"),
        Err(e) => e,
    };
    assert!(matches!(err, ext4_core::Error::BadMagic));
}

#[test]
fn test_free_counts_consistency_after_writes() {
    let tools = tools();
    let img = make_basic_image(&tools, "counts.img", 32);
    {
        let mut fs = mount_rw(&img);
        for i in 0..10 {
            let ino = fs.create_file("/", &format!("f{i}.txt")).unwrap();
            fs.write_file(ino, 0, &[b'A' + (i as u8); 5000]).unwrap();
        }
        fs.unmount().unwrap();
    }
    // e2fsck -fn valide les compteurs libres sb/gdt/bitmaps checksums inclus
    tools.fsck_clean(&img);
    // ré-import : compteurs identiques à dumpe2fs
    let dump = tools.dumpe2fs(&img);
    let dump_free_blocks = dump
        .lines()
        .find(|l| l.contains("Free blocks:"))
        .and_then(|l| l.split(':').nth(1))
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap();
    let mut fs = mount_ro(&img);
    assert_eq!(fs.sb.free_blocks_count, dump_free_blocks, "blocs libres");
}

#[test]
fn test_encrypt_dir_refused() {
    let tools = tools();
    let img = make_basic_image(&tools, "enc.img", 32);
    // marque un répertoire ENCRYPT via debugfs : mon moteur doit refuser
    tools.debugfs(&img, &["mkdir /secret", "sif /secret flags 0x808"]);
    let mut fs = mount_ro(&img);
    let d = fs.resolve("/secret").unwrap();
    let err = fs.read_dir(&d).unwrap_err();
    assert!(matches!(err, ext4_core::Error::Encrypted), "{err}");
}

// ---------------------------------------------------------------------------
// 9. rename (déplacement/renommage) + set_times
// ---------------------------------------------------------------------------

#[test]
fn test_rename_same_dir() {
    let tools = tools();
    let img = make_basic_image(&tools, "ren1.img", 32);
    populate_basic(&tools, &img);
    {
        let mut fs = mount_rw(&img);
        fs.rename("/hello.txt", "/renamed-hello.txt", true).unwrap();
        // l'ancien nom a disparu
        assert!(fs.resolve("/hello.txt").is_err());
        // contenu intact au nouveau nom
        let ino = fs.resolve("/renamed-hello.txt").unwrap();
        let data = fs.read_file_to_vec(ino.no, 0, 200).unwrap();
        assert_eq!(data, expected_content(100, 0x11));
        fs.unmount().unwrap();
    }
    tools.fsck_clean(&img);
    // debugfs voit le nouveau nom, pas l'ancien
    let out = tools.debugfs_ro(&img, &["ls /"]);
    assert!(out.contains("renamed-hello.txt"), "{out}");
    assert!(!out.lines().any(|l| l.contains("hello.txt") && !l.contains("renamed") && !l.contains("hardlink")), "{out}");
}

#[test]
fn test_rename_cross_dir() {
    let tools = tools();
    let img = make_basic_image(&tools, "ren2.img", 32);
    populate_basic(&tools, &img);
    {
        let mut fs = mount_rw(&img);
        fs.mkdir("/", "dest").unwrap();
        let payload = expected_content(50_000, 0x77);
        let ino = fs.create_file("/", "mover.txt").unwrap();
        fs.write_file(ino, 0, &payload).unwrap();
        fs.rename("/mover.txt", "/dest/arrived.txt", true).unwrap();
        assert!(fs.resolve("/mover.txt").is_err());
        let d = fs.resolve("/dest/arrived.txt").unwrap();
        let data = fs.read_file_to_vec(d.no, 0, 100_000).unwrap();
        assert_eq!(data, payload);
        fs.unmount().unwrap();
    }
    tools.fsck_clean(&img);
    // debugfs confirme l'entrée dans dest
    let out = tools.debugfs_ro(&img, &["ls /dest"]);
    assert!(out.contains("arrived.txt"), "{out}");
}

#[test]
fn test_rename_dir_updates_dotdot() {
    let tools = tools();
    let img = make_basic_image(&tools, "ren3.img", 32);
    {
        let mut fs = mount_rw(&img);
        fs.mkdir("/", "p1").unwrap();
        fs.mkdir("/", "p2").unwrap();
        let child = fs.mkdir("/p1", "child").unwrap();
        let fin = fs.create_file("/p1/child", "inner.txt").unwrap();
        fs.write_file(fin, 0, b"inside").unwrap();
        fs.rename("/p1/child", "/p2/child", true).unwrap();
        // '..' doit maintenant pointer sur p2 : on lit le dirent '..' du child
        let cdir = fs.resolve("/p2/child").unwrap();
        let entries = fs.read_dir(&cdir).unwrap();
        let dotdot = entries.iter().find(|e| e.name == b"..").unwrap();
        let p2 = fs.resolve("/p2").unwrap();
        assert_eq!(dotdot.inode, p2.no, "'..' doit pointer sur le nouveau parent");
        let _ = child;
        // contenu intact
        let inner = fs.resolve("/p2/child/inner.txt").unwrap();
        assert_eq!(fs.read_file_to_vec(inner.no, 0, 10).unwrap(), b"inside");
        // liens : p1 a perdu un lien de répertoire, p2 en a gagné un
        let p1 = fs.resolve("/p1").unwrap();
        assert_eq!(p1.links_count(), 2, "p1 ne doit plus compter child");
        assert_eq!(p2.links_count(), 3, "p2 doit compter child");
        fs.unmount().unwrap();
    }
    tools.fsck_clean(&img);
}

#[test]
fn test_rename_overwrite() {
    let tools = tools();
    let img = make_basic_image(&tools, "ren4.img", 32);
    {
        let mut fs = mount_rw(&img);
        let a = fs.create_file("/", "a.txt").unwrap();
        fs.write_file(a, 0, b"contenu AAA").unwrap();
        let b = fs.create_file("/", "b.txt").unwrap();
        fs.write_file(b, 0, b"contenu BBB").unwrap();
        fs.rename("/a.txt", "/b.txt", true).unwrap();
        // b.txt = maintenant l'ancien inode de a
        let nb = fs.resolve("/b.txt").unwrap();
        assert_eq!(nb.no, a);
        assert_eq!(fs.read_file_to_vec(a, 0, 100).unwrap(), b"contenu AAA");
        assert!(fs.resolve("/a.txt").is_err());
        // l'ancien b a été libéré : 1 seul fichier nommé b.txt
        fs.unmount().unwrap();
    }
    tools.fsck_clean(&img);
}

#[test]
fn test_rename_overwrite_dir() {
    let tools = tools();
    let img = make_basic_image(&tools, "ren5.img", 32);
    {
        let mut fs = mount_rw(&img);
        fs.mkdir("/", "src").unwrap();
        let f = fs.create_file("/src", "data.bin").unwrap();
        fs.write_file(f, 0, b"SRC").unwrap();
        fs.mkdir("/", "dst").unwrap();
        fs.rename("/src", "/dst", true).unwrap();
        // src a disparu, dst contient data.bin
        assert!(fs.resolve("/src").is_err());
        let d = fs.resolve("/dst").unwrap();
        let f = fs.resolve("/dst/data.bin").unwrap();
        assert_eq!(fs.read_file_to_vec(f.no, 0, 10).unwrap(), b"SRC");
        // '..' de l'ex-src pointe vers la racine
        let entries = fs.read_dir(&d).unwrap();
        let dotdot = entries.iter().find(|e| e.name == b"..").unwrap();
        assert_eq!(dotdot.inode, 2, "'..' doit pointer sur la racine");
        fs.unmount().unwrap();
    }
    tools.fsck_clean(&img);
}

#[test]
fn test_rename_errors() {
    let tools = tools();
    let img = make_basic_image(&tools, "ren6.img", 32);
    populate_basic(&tools, &img);
    {
        let mut fs = mount_rw(&img);
        fs.mkdir("/", "sub").unwrap();
        let f = fs.create_file("/sub", "x.txt").unwrap();
        fs.write_file(f, 0, b"x").unwrap();
        // répertoire dans son propre sous-arbre
        assert!(matches!(fs.rename("/sub", "/sub/deeper", true), Err(ext4_core::Error::DirLoop)));
        // la racine n'a pas d'entrée parente : refus (BadPath)
        assert!(fs.rename("/", "/sub/root", true).is_err());
        // fichier sur un répertoire : interdit
        assert!(fs.rename("/hello.txt", "/sub", true).is_err());
        // sans overwrite sur cible existante
        fs.create_file("/", "c1.txt").unwrap();
        fs.create_file("/", "c2.txt").unwrap();
        assert!(matches!(fs.rename("/c1.txt", "/c2.txt", false), Err(ext4_core::Error::AlreadyExists)));
        // introuvable
        assert!(matches!(fs.rename("/inexistant", "/sub/y", true), Err(ext4_core::Error::NotFound)));
        // répertoire non vide comme cible overwrite
        fs.mkdir("/", "full").unwrap();
        fs.create_file("/full", "keep.txt").unwrap();
        assert!(matches!(fs.rename("/sub", "/full", true), Err(ext4_core::Error::DirectoryNotEmpty)));
        // no-op rename(x,x)
        fs.rename("/hello.txt", "/hello.txt", true).unwrap();
        fs.unmount().unwrap();
    }
    tools.fsck_clean(&img);
}

#[test]
fn test_rename_htree_dir() {
    let tools = tools();
    let img = make_basic_image(&tools, "ren7.img", 64);
    {
        let mut fs = mount_rw(&img);
        fs.mkdir("/", "big").unwrap();
        fs.mkdir("/", "other").unwrap();
        // peuple via l'API (crée l'index au-delà d'un bloc)
        let mut names: Vec<String> = Vec::new();
        for i in 0..3000 {
            let n = format!("entry-{i:05}.dat");
            let ino = fs.create_file("/big", &n).unwrap();
            fs.write_file(ino, 0, &[b'k'; 40]).unwrap();
            names.push(n);
        }
        // renomme dans le même répertoire indexé
        fs.rename("/big/entry-00010.dat", "/big/renamed-entry.dat", true).unwrap();
        // et vers un autre répertoire
        fs.rename("/big/entry-02999.dat", "/other/moved-entry.dat", true).unwrap();
        // vérifications de lookup
        assert!(fs.resolve("/big/entry-00010.dat").is_err());
        assert!(fs.resolve("/big/renamed-entry.dat").is_ok());
        let m = fs.resolve("/other/moved-entry.dat").unwrap();
        assert_eq!(fs.read_file_to_vec(m.no, 0, 40).unwrap(), vec![b'k'; 40]);
        fs.unmount().unwrap();
    }
    tools.fsck_clean(&img);
    tools.fsck_optimize(&img); // -fD : l'index htree doit rester cohérent
}

#[test]
fn test_set_times() {
    let tools = tools();
    let img = make_basic_image(&tools, "times.img", 32);
    populate_basic(&tools, &img);
    {
        let mut fs = mount_rw(&img);
        let ino = fs.resolve("/hello.txt").unwrap().no;
        fs.set_times(ino, Some(1_600_000_000), Some(1_700_000_000), Some(1_650_000_000)).unwrap();
        let st = fs.stat(ino).unwrap();
        assert_eq!(st.atime, 1_600_000_000);
        assert_eq!(st.mtime, 1_700_000_000);
        assert_eq!(st.ctime, 1_650_000_000);
        // None = inchangé
        fs.set_times(ino, Some(1_610_000_000), None, None).unwrap();
        let st2 = fs.stat(ino).unwrap();
        assert_eq!(st2.atime, 1_610_000_000);
        assert_eq!(st2.mtime, 1_700_000_000);
        fs.unmount().unwrap();
    }
    tools.fsck_clean(&img);
    // debugfs confirme le mtime
    let out = tools.debugfs_ro(&img, &["stat /hello.txt"]);
    assert!(out.contains("1700000000") || out.contains("2023"), "{out}");
}
