//! Harnais commun aux tests d'intégration : appareil fichier + pilots
//! e2fsprogs (mkfs.ext4, debugfs, e2fsck, dumpe2fs).

#![cfg(feature = "std-tests")]
#![allow(dead_code)]

use ext4_core::blockdev::{check_range, BlockDevice};
use ext4_core::error::{Error, Result};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

static SCRIPT_SEQ: AtomicU64 = AtomicU64::new(0);

fn unique_script(ro: bool) -> PathBuf {
    let n = SCRIPT_SEQ.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir().join(format!(
        "debugfs_cmds_{}_{}_{}.txt",
        std::process::id(),
        ro as u8,
        n
    ))
}

pub struct FileDevice {
    file: std::fs::File,
    pub size: u64,
    pub read_only: bool,
}

impl FileDevice {
    pub fn open(path: &Path, read_only: bool) -> std::io::Result<FileDevice> {
        let mut opts = std::fs::OpenOptions::new();
        opts.read(true).write(!read_only);
        let mut file = opts.open(path)?;
        let size = file.seek(SeekFrom::End(0))?;
        file.seek(SeekFrom::Start(0))?;
        Ok(FileDevice {
            file,
            size,
            read_only,
        })
    }
}

impl BlockDevice for FileDevice {
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        check_range(offset, buf.len(), self.size)?;
        self.file
            .seek(SeekFrom::Start(offset))
            .map_err(|_| Error::Device(1))?;
        self.file.read_exact(buf).map_err(|_| Error::Device(2))?;
        Ok(())
    }
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<()> {
        if self.read_only {
            return Err(Error::ReadOnly);
        }
        check_range(offset, buf.len(), self.size)?;
        self.file
            .seek(SeekFrom::Start(offset))
            .map_err(|_| Error::Device(1))?;
        self.file.write_all(buf).map_err(|_| Error::Device(3))?;
        self.file.sync_data().map_err(|_| Error::Device(4))?;
        Ok(())
    }
    fn size(&mut self) -> Result<u64> {
        Ok(self.size)
    }
    fn is_read_only(&self) -> bool {
        self.read_only
    }
}

/// Outils e2fsprogs extraits sans root.
pub struct E2Tools {
    pub dir: PathBuf,
}

fn libdir(dir: &Path) -> String {
    let p = dir.parent().unwrap().join("lib/x86_64-linux-gnu");
    p.to_str().unwrap().to_string()
}

impl E2Tools {
    pub fn detect() -> Option<E2Tools> {
        for d in ["/home/z/tools/e2f/root/usr/sbin", "/usr/sbin", "/sbin"] {
            if Path::new(d).join("mkfs.ext4").exists() {
                return Some(E2Tools { dir: d.into() });
            }
        }
        None
    }

    fn run(&self, tool: &str, args: &[&str]) -> std::io::Result<std::process::Output> {
        let bin = self.dir.join(tool);
        let mut cmd = Command::new(&bin);
        cmd.args(args)
            .env("LD_LIBRARY_PATH", libdir(&self.dir))
            .stdin(Stdio::null());
        cmd.output()
    }

    pub fn mkfs(&self, img: &Path, opts: &[&str]) {
        let mut a = opts.to_vec();
        a.push(img.to_str().unwrap());
        let out = self.run("mkfs.ext4", &a).expect("mkfs.ext4 introuvable");
        assert!(
            out.status.success(),
            "mkfs.ext4 a échoué: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn debugfs_common(&self, ro: bool, img: &Path, cmds: &[&str]) -> String {
        let script = unique_script(ro);
        std::fs::write(&script, cmds.join("\n") + "\n").unwrap();
        let a: Vec<&str> = if ro {
            vec!["-f", script.to_str().unwrap(), img.to_str().unwrap()]
        } else {
            vec![
                "-w",
                "-f",
                script.to_str().unwrap(),
                img.to_str().unwrap(),
            ]
        };
        let out = self.run("debugfs", &a).expect("debugfs introuvable");
        let _ = std::fs::remove_file(&script);
        let all = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(out.status.success(), "debugfs a échoué: {all}");
        all
    }

    pub fn debugfs(&self, img: &Path, cmds: &[&str]) -> String {
        self.debugfs_common(false, img, cmds)
    }

    pub fn debugfs_ro(&self, img: &Path, cmds: &[&str]) -> String {
        self.debugfs_common(true, img, cmds)
    }

    /// e2fsck -fn : doit être PROPRE (exit 0).
    pub fn fsck_clean(&self, img: &Path) -> String {
        let out = self
            .run("e2fsck", &["-fn", img.to_str().unwrap()])
            .expect("e2fsck introuvable");
        let all = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            out.status.code(),
            Some(0),
            "e2fsck -fn a détecté des erreurs:\n{all}"
        );
        all
    }

    /// e2fsck complet (rejoue le journal comme le kernel).
    pub fn fsck_replay(&self, img: &Path) -> String {
        let out = self
            .run("e2fsck", &["-p", img.to_str().unwrap()])
            .expect("e2fsck introuvable");
        let all = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let code = out.status.code().unwrap_or(99);
        assert!(
            code == 0 || code == 1,
            "e2fsck -p a échoué (code {code}):\n{all}"
        );
        all
    }

    /// e2fsck -fD : reconstruction des répertoires (crée les HTrees).
    pub fn fsck_optimize(&self, img: &Path) -> String {
        let out = self
            .run("e2fsck", &["-fD", "-y", img.to_str().unwrap()])
            .expect("e2fsck introuvable");
        let all = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(out.status.success(), "e2fsck -fD a échoué:\n{all}");
        all
    }

    pub fn dumpe2fs(&self, img: &Path) -> String {
        let out = self
            .run("dumpe2fs", &["-h", img.to_str().unwrap()])
            .expect("dumpe2fs introuvable");
        String::from_utf8_lossy(&out.stdout).to_string()
    }
}

pub fn tmp_img(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join("ext4rs_tests");
    std::fs::create_dir_all(&d).unwrap();
    d.join(name)
}

/// Fichier temporaire local avec un motif déterministe.
pub fn make_local_file(path: &Path, size: usize, seed: u8) {
    let data: Vec<u8> = (0..size)
        .map(|i| ((i as u8).wrapping_mul(seed).wrapping_add(seed)) ^ (i >> 8) as u8)
        .collect();
    std::fs::write(path, &data).unwrap();
}

pub fn expected_content(size: usize, seed: u8) -> Vec<u8> {
    (0..size)
        .map(|i| ((i as u8).wrapping_mul(seed).wrapping_add(seed)) ^ (i >> 8) as u8)
        .collect()
}
