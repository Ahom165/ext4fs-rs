//! ext4cli — CLI ext4 multiplateforme (Linux/Windows user-mode).
//!
//! Commandes : info, list, tree, cat, extract, mkdir, write, rm, rmdir,
//! link, symlink, stat. L'appareil peut être une image fichier ou un
//! périphérique bloc (sous Windows : \\.\E: etc.).

use ext4_core::blockdev::BlockDevice;
use ext4_core::fs::Filesystem;
use ext4_core::inode::FileType;
use ext4_core::util::TimeSource;
use ext4_core::xattr::split_ns;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

struct HostTime;
impl TimeSource for HostTime {
    fn now(&self) -> (u32, u32) {
        let t = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        (t.as_secs() as u32, t.subsec_nanos())
    }
}

struct FileDev {
    f: std::fs::File,
    size: u64,
    ro: bool,
}

impl FileDev {
    fn open(p: &Path, ro: bool) -> std::io::Result<Self> {
        let mut o = std::fs::OpenOptions::new();
        o.read(true).write(!ro);
        let mut f = o.open(p)?;
        let size = f.seek(SeekFrom::End(0))?;
        f.seek(SeekFrom::Start(0))?;
        Ok(FileDev { f, size, ro })
    }
}

impl BlockDevice for FileDev {
    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> ext4_core::Result<()> {
        use ext4_core::error::Error;
        self.f.seek(SeekFrom::Start(off)).map_err(|_| Error::Device(1))?;
        self.f.read_exact(buf).map_err(|_| Error::Device(2))?;
        Ok(())
    }
    fn write_at(&mut self, off: u64, buf: &[u8]) -> ext4_core::Result<()> {
        use ext4_core::error::Error;
        if self.ro {
            return Err(Error::ReadOnly);
        }
        self.f.seek(SeekFrom::Start(off)).map_err(|_| Error::Device(1))?;
        self.f.write_all(buf).map_err(|_| Error::Device(3))?;
        Ok(())
    }
    fn size(&mut self) -> ext4_core::Result<u64> {
        Ok(self.size)
    }
    fn is_read_only(&self) -> bool {
        self.ro
    }
}

fn open_fs(image: &str, ro: bool) -> Filesystem {
    // appareil ouvert en RW (le replay du journal écrit), montage ro si demandé
    let dev = Box::new(FileDev::open(Path::new(image), false).expect("ouverture image"));
    Filesystem::mount(dev, Box::new(HostTime), ro).expect("montage ext4")
}

fn usage() -> ! {
    eprintln!(
        "ext4cli — accès ext4 multiplateforme\n\n\
         USAGE: ext4cli <commande> <image> [args...]\n\n\
         Commandes:\n\
         \x20 info <image>                          infos du volume\n\
         \x20 list <image> <chemin>                 liste un répertoire\n\
         \x20 tree <image> [chemin]                 arborescence récursive\n\
         \x20 cat <image> <fichier>                 contenu vers stdout\n\
         \x20 extract <image> <fichier> <dest>      extraction vers un fichier\n\
         \x20 stat <image> <chemin>                 métadonnées\n\
         \x20 mkdir <image> <dir> <nom>             crée un répertoire\n\
         \x20 write <image> <fichier> <src|-]      écrit un fichier (src=- : stdin)\n\
         \x20 rm <image> <fichier>                  supprime un fichier\n\
         \x20 rmdir <image> <dir>                   supprime un répertoire vide\n\
         \x20 link <image> <cible> <dir> <nom>      lien matériel\n\
         \x20 symlink <image> <dir> <nom> <cible>   lien symbolique\n\n\
         Le montage est lecture seule sauf pour les commandes d'écriture."
    );
    std::process::exit(2);
}

fn mode_str(mode: u16) -> String {
    let t = match mode & 0xF000 {
        0x8000 => '-',
        0x4000 => 'd',
        0xA000 => 'l',
        0x2000 => 'c',
        0x6000 => 'b',
        0x1000 => 'p',
        0xC000 => 's',
        _ => '?',
    };
    let rwx = |m: u16| {
        format!(
            "{}{}{}",
            if m & 4 != 0 { "r" } else { "-" },
            if m & 2 != 0 { "w" } else { "-" },
            if m & 1 != 0 { "x" } else { "-" }
        )
    };
    format!(
        "{}{}{}{}",
        t,
        rwx((mode >> 6) & 7),
        rwx((mode >> 3) & 7),
        rwx(mode & 7)
    )
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        usage();
    }
    let cmd = args[1].as_str();
    let image = args[2].as_str();
    let write_cmd = matches!(
        cmd,
        "mkdir" | "write" | "rm" | "rmdir" | "link" | "symlink" | "mv" | "touch" | "ea_set" | "ea_rm"
    );
    let mut fs = open_fs(image, !write_cmd);

    match cmd {
        "info" => {
            println!("{}", fs.volume_info().unwrap());
            println!(
                "features: compat={:#x} incompat={:#x} ro_compat={:#x}",
                fs.sb.feature_compat, fs.sb.feature_incompat, fs.sb.feature_ro_compat
            );
            println!(
                "block_size={} inodes_per_group={} desc_size={} groups={}",
                fs.sb.block_size, fs.sb.inodes_per_group, fs.sb.desc_size, fs.sb.groups_count
            );
        }
        "list" => {
            let path = args.get(3).map(|s| s.as_str()).unwrap_or("/");
            let ino = fs.resolve(path).expect("chemin introuvable");
            let entries = fs.read_dir(&ino).expect("lecture répertoire");
            println!("{:<12} {:>10}  {:<6} {}", "mode", "taille", "ino", "nom");
            for e in &entries {
                let st = fs.stat(e.inode).expect("stat");
                println!(
                    "{:<12} {:>10}  {:<6} {}",
                    mode_str(st.mode),
                    st.size,
                    st.ino,
                    String::from_utf8_lossy(&e.name)
                );
            }
        }
        "tree" => {
            let path = args.get(3).map(|s| s.as_str()).unwrap_or("/");
            tree(&mut fs, path, 0);
        }
        "cat" => {
            let path = args.get(3).unwrap_or_else(|| usage());
            let ino = fs.resolve(path).expect("chemin introuvable");
            let size = ino.size();
            let mut off = 0u64;
            let mut buf = vec![0u8; 64 * 1024];
            let stdout = std::io::stdout();
            let mut lock = stdout.lock();
            while off < size {
                let n = fs.read_file(ino.no, off, &mut buf).expect("lecture");
                if n == 0 {
                    break;
                }
                lock.write_all(&buf[..n]).expect("écriture stdout");
                off += n as u64;
            }
        }
        "extract" => {
            let src = args.get(3).unwrap_or_else(|| usage());
            let dst = args.get(4).unwrap_or_else(|| usage());
            let ino = fs.resolve(src).expect("chemin introuvable");
            let mut out = std::fs::File::create(dst).expect("création destination");
            let mut off = 0u64;
            let mut buf = vec![0u8; 256 * 1024];
            while off < ino.size() {
                let n = fs.read_file(ino.no, off, &mut buf).expect("lecture");
                if n == 0 {
                    break;
                }
                out.write_all(&buf[..n]).expect("écriture");
                off += n as u64;
            }
            println!("extrait {} -> {} ({} octets)", src, dst, ino.size());
        }
        "stat" => {
            let path = args.get(3).unwrap_or_else(|| usage());
            let ino = fs.resolve(path).expect("chemin introuvable");
            let st = fs.stat(ino.no).unwrap();
            println!(
                "ino={} mode={} ({}) taille={} liens={} uid={} gid={}\natime={} mtime={} ctime={}",
                st.ino,
                st.mode,
                mode_str(st.mode),
                st.size,
                st.nlink,
                st.uid,
                st.gid,
                st.atime,
                st.mtime,
                st.ctime
            );
            if ino.file_type() == Some(FileType::Symlink) {
                println!(
                    "cible: {}",
                    String::from_utf8_lossy(&fs.readlink(ino.no).unwrap())
                );
            }
        }
        "mkdir" => {
            let dir = args.get(3).unwrap_or_else(|| usage());
            let name = args.get(4).unwrap_or_else(|| usage());
            let ino = fs.mkdir(dir, name).expect("mkdir");
            println!("répertoire créé: {}/{} (ino {})", dir, name, ino);
        }
        // =============================================================
        // xattrs : ea_get / ea_set / ea_rm / ea_list (namespace user.)
        // =============================================================
        "ea_list" => {
            let path = args.get(3).unwrap_or_else(|| usage());
            let ino = fs.resolve(path).expect("chemin introuvable");
            for (e, v) in ext4_core::xattr::list_xattrs(&mut fs, ino.no).expect("ea_list") {
                let prefix = match e.name_index {
                    ext4_core::xattr::XIDX_USER => "user.",
                    ext4_core::xattr::XIDX_SYSTEM => "system.",
                    4 => "trusted.",
                    6 => "security.",
                    _ => "?",
                };
                println!(
                    "{}{} ({} o) hash={:#010x}",
                    prefix,
                    String::from_utf8_lossy(&e.name),
                    v.len(),
                    e.hash
                );
            }
        }
        "ea_get" => {
            let path = args.get(3).unwrap_or_else(|| usage());
            let name = args.get(4).unwrap_or_else(|| usage());
            let ino = fs.resolve(path).expect("chemin introuvable");
            // "user.a" / "system.data" : préfixe -> index
            let (idx, rest) = split_ns(name.as_bytes());
            match ext4_core::xattr::get_xattr(&mut fs, ino.no, idx, &rest) {
                Ok(Some(v)) => println!("{}: {} o", name, v.len()),
                Ok(None) => {
                    println!("{name}: absent");
                    std::process::exit(2);
                }
                Err(e) => {
                    eprintln!("ea_get {name}: {e}");
                    std::process::exit(1);
                }
            }
        }
        "ea_set" => {
            let path = args.get(3).unwrap_or_else(|| usage());
            let name = args.get(4).unwrap_or_else(|| usage());
            let value = args.get(5).unwrap_or_else(|| usage());
            let ino = fs.resolve(path).expect("chemin introuvable");
            if fs.sb.state() & 1 != 0 || fs.read_only {
                // monté RO : erreur
            }
            let (idx, rest) = split_ns(name.as_bytes());
            ext4_core::xattr::set_xattr(&mut fs, ino.no, idx, &rest, value.as_bytes())
                .expect("ea_set");
            println!("{name}: {} o écrits", value.len());
        }
        "ea_rm" => {
            let path = args.get(3).unwrap_or_else(|| usage());
            let name = args.get(4).unwrap_or_else(|| usage());
            let ino = fs.resolve(path).expect("chemin introuvable");
            let (idx, rest) = split_ns(name.as_bytes());
            ext4_core::xattr::remove_xattr(&mut fs, ino.no, idx, &rest).expect("ea_rm");
            println!("{name}: supprimé");
        }
        "write" => {
            let path = args.get(3).unwrap_or_else(|| usage());
            let src = args.get(4).unwrap_or_else(|| usage());
            let data = if src == "-" {
                let mut v = Vec::new();
                std::io::stdin().read_to_end(&mut v).expect("stdin");
                v
            } else {
                std::fs::read(src).expect("lecture source")
            };
            // fichier existant ou création
            let ino = match fs.resolve(path) {
                Ok(i) if i.file_type() == Some(FileType::Regular) => i.no,
                Ok(_) => {
                    eprintln!("la cible existe et n'est pas un fichier régulier");
                    std::process::exit(1);
                }
                Err(ext4_core::Error::NotFound) => {
                    let (dir, name) = split_last(path);
                    fs.create_file(dir, &name).expect("création")
                }
                Err(e) => panic!("{e}"),
            };
            fs.write_file(ino, 0, &data).expect("écriture");
            println!("écrit {} octets dans {}", data.len(), path);
        }
        "rm" => {
            let path = args.get(3).unwrap_or_else(|| usage());
            fs.unlink(path).expect("suppression");
            println!("supprimé: {path}");
        }
        "rmdir" => {
            let path = args.get(3).unwrap_or_else(|| usage());
            fs.rmdir(path).expect("suppression répertoire");
            println!("répertoire supprimé: {path}");
        }
        "link" => {
            let target = args.get(3).unwrap_or_else(|| usage());
            let dir = args.get(4).unwrap_or_else(|| usage());
            let name = args.get(5).unwrap_or_else(|| usage());
            fs.link(target, dir, name).expect("link");
            println!("lien créé");
        }
        "mv" => {
            let old = args[3].clone();
            let new = args[4].clone();
            fs.rename(&old, &new, true).expect("rename");
            println!("déplacé : {old} -> {new}");
        }
        "touch" => {
            // touch <img> <chemin> [atime] [mtime] : pose les temps POSIX
            let path = args[3].clone();
            let (at, mt) = match (args.get(4), args.get(5)) {
                (Some(a), Some(m)) => (a.parse::<u32>().unwrap(), m.parse::<u32>().unwrap()),
                _ => {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_secs() as u32;
                    (now, now)
                }
            };
            let ino = fs.resolve(&path).expect("chemin introuvable");
            fs.set_times(ino.no, Some(at), Some(mt), None).expect("set_times");
            println!("temps mis à jour : {path}");
        }
        "symlink" => {
            let dir = args.get(3).unwrap_or_else(|| usage());
            let name = args.get(4).unwrap_or_else(|| usage());
            let target = args.get(5).unwrap_or_else(|| usage());
            fs.symlink(dir, name, target).expect("symlink");
            println!("lien symbolique créé");
        }
        _ => usage(),
    }
    // finalisation propre (journal finalisé, état clean) pour les écritures
    if write_cmd {
        fs.unmount().expect("unmount");
    }
}

fn split_last(path: &str) -> (&str, String) {
    match path.rfind('/') {
        Some(0) => ("/", path[1..].to_string()),
        Some(i) => (&path[..i], path[i + 1..].to_string()),
        None => ("/", path.to_string()),
    }
}

fn tree(fs: &mut Filesystem, path: &str, depth: usize) {
    let ino = fs.resolve(path).expect("chemin introuvable");
    let entries = match fs.read_dir(&ino) {
        Ok(e) => e,
        Err(_) => return,
    };
    for e in &entries {
        let name = String::from_utf8_lossy(&e.name);
        if name == "." || name == ".." {
            continue;
        }
        let st = fs.stat(e.inode).expect("stat");
        println!(
            "{}{}{} {}",
            "  ".repeat(depth),
            if st.kind == Some(FileType::Dir) { "+ " } else { "- " },
            name,
            if st.kind == Some(FileType::Dir) {
                "/".to_string()
            } else {
                format!(" ({} o)", st.size)
            }
        );
        if st.kind == Some(FileType::Dir) && depth < 8 {
            let child = format!("{}/{}", path.trim_end_matches('/'), name);
            tree(fs, &child, depth + 1);
        }
    }
}
