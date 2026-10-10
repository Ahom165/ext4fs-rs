//! ext4ctl — outil de contrôle du pilote ext4drv (\\.\Ext4Fs).
//!
//! Compile sous Windows uniquement (windows-sys). Mirroir user-mode du
//! protocole défini dans crates/ext4drv/src/ioctl_defs.rs.

#![allow(non_snake_case)]

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::DeviceIoControl;

// ---- Protocole (miroir de ioctl_defs.rs) -----------------------------------
const FILE_DEVICE_UNKNOWN: u32 = 0x0000_0022;
const fn ctl_code(dt: u32, f: u32, m: u32, a: u32) -> u32 {
    (dt << 16) | (a << 14) | (f << 2) | m
}
const IOCTL_EXT4_OPEN_IMAGE: u32 = ctl_code(FILE_DEVICE_UNKNOWN, 0x800, 0, 3);
const IOCTL_EXT4_CLOSE: u32 = ctl_code(FILE_DEVICE_UNKNOWN, 0x801, 0, 0);
const IOCTL_EXT4_VOLUME_INFO: u32 = ctl_code(FILE_DEVICE_UNKNOWN, 0x802, 0, 1);
const IOCTL_EXT4_LIST_DIR: u32 = ctl_code(FILE_DEVICE_UNKNOWN, 0x803, 0, 1);
const IOCTL_EXT4_READ_FILE: u32 = ctl_code(FILE_DEVICE_UNKNOWN, 0x804, 0, 1);
const IOCTL_EXT4_WRITE_FILE: u32 = ctl_code(FILE_DEVICE_UNKNOWN, 0x805, 0, 2);
const IOCTL_EXT4_CREATE_FILE: u32 = ctl_code(FILE_DEVICE_UNKNOWN, 0x806, 0, 2);
const IOCTL_EXT4_MKDIR: u32 = ctl_code(FILE_DEVICE_UNKNOWN, 0x807, 0, 2);
const IOCTL_EXT4_UNLINK: u32 = ctl_code(FILE_DEVICE_UNKNOWN, 0x808, 0, 2);
const IOCTL_EXT4_RMDIR: u32 = ctl_code(FILE_DEVICE_UNKNOWN, 0x809, 0, 2);
const IOCTL_EXT4_STAT: u32 = ctl_code(FILE_DEVICE_UNKNOWN, 0x80B, 0, 1);

#[repr(C)]
#[derive(Clone, Copy)]
struct VolumeReq {
    volume_id: u32,
}

#[repr(C)]
struct OpenImageReq {
    path_len_bytes: u32,
    read_only: u32,
}

#[repr(C)]
struct PathReq {
    volume_id: u32,
    path_len_bytes: u32,
}

#[repr(C)]
struct ReadFileReq {
    volume_id: u32,
    path_len_bytes: u32,
    offset: u64,
    length: u32,
}

#[repr(C)]
struct WriteFileReq {
    volume_id: u32,
    path_len_bytes: u32,
    offset: u64,
    data_len: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct DirEntryWire {
    ino: u32,
    size: u64,
    mode: u16,
    kind: u8,
    name_len: u16,
}

fn driver_handle() -> HANDLE {
    let path: Vec<u16> = "\\\\.\\Ext4Fs\0".encode_utf16().collect();
    unsafe {
        let h = CreateFileW(
            path.as_ptr(),
            0x8000_0000 | 0x4000_0000, // GENERIC_READ | GENERIC_WRITE
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        );
        if h == INVALID_HANDLE_VALUE {
            let err = GetLastError();
            eprintln!(
                "impossible d'ouvrir \\\\.\\Ext4Fs (erreur {err}) — le pilote ext4drv est-il démarré ?"
            );
            std::process::exit(1);
        }
        h
    }
}

fn ioctl(h: HANDLE, code: u32, in_buf: &mut [u8], out_buf: &mut [u8]) -> Result<usize, u32> {
    let mut returned: u32 = 0;
    let ok = unsafe {
        DeviceIoControl(
            h,
            code,
            in_buf.as_mut_ptr() as *const _,
            in_buf.len() as u32,
            out_buf.as_mut_ptr() as *mut _,
            out_buf.len() as u32,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        Err(unsafe { GetLastError() })
    } else {
        Ok(returned as usize)
    }
}

fn cmd_mount(h: HANDLE, args: &[String]) {
    let image = args.first().expect("usage: ext4ctl mount <image|\\\\.\\D:> [ro]");
    let ro = args.get(2).map(|s| s == "ro").unwrap_or(false);
    let path16: Vec<u16> = image.encode_utf16().collect();
    let mut in_buf = vec![0u8; 8 + path16.len() * 2];
    let hdr = OpenImageReq {
        path_len_bytes: (path16.len() * 2) as u32,
        read_only: ro as u32,
    };
    in_buf[..8].copy_from_slice(unsafe {
        core::slice::from_raw_parts(&hdr as *const _ as *const u8, 8)
    });
    for (i, w) in path16.iter().enumerate() {
        in_buf[8 + i * 2..10 + i * 2].copy_from_slice(&w.to_le_bytes());
    }
    let mut out_buf = vec![0u8; 8];
    match ioctl(h, IOCTL_EXT4_OPEN_IMAGE, &mut in_buf, &mut out_buf) {
        Ok(_) => {
            let id = u32::from_le_bytes(out_buf[0..4].try_into().unwrap());
            println!("volume monté : id={id}{}", if ro { " (lecture seule)" } else { "" });
        }
        Err(e) => {
            eprintln!("montage échoué (erreur {e:#x}) — chemin \\??\\ préfixé si nécessaire");
            std::process::exit(1);
        }
    }
}

fn volume_id_of(args: &[String]) -> u32 {
    args.first().expect("volume id").parse().expect("id numérique")
}

fn cmd_volume_info(h: HANDLE, args: &[String]) {
    let id = volume_id_of(args);
    let mut in_buf: Vec<u8> = {
        let r = VolumeReq { volume_id: id };
        unsafe { core::slice::from_raw_parts(&r as *const _ as *const u8, 4) }.to_vec()
    };
    let mut out_buf = vec![0u8; 1024];
    match ioctl(h, IOCTL_EXT4_VOLUME_INFO, &mut in_buf, &mut out_buf) {
        Ok(n) => {
            let uuid: Vec<String> = out_buf[12..28].iter().map(|b| format!("{b:02x}")).collect();
            let blocks = u64::from_le_bytes(out_buf[4..12].try_into().unwrap());
            let bs = u32::from_le_bytes(out_buf[32..36].try_into().unwrap());
            let free_blocks = u64::from_le_bytes(out_buf[36..44].try_into().unwrap());
            let free_inodes = u32::from_le_bytes(out_buf[44..48].try_into().unwrap());
            let ro = u32::from_le_bytes(out_buf[48..52].try_into().unwrap());
            let label_len = u32::from_le_bytes(out_buf[52..56].try_into().unwrap()) as usize;
            let label = String::from_utf8_lossy(&out_buf[56..56 + label_len]).to_string();
            let _ = n;
            println!(
                "volume {id}: uuid={} label=\"{label}\" blocs={blocks} (bs={bs}) libres={free_blocks} inodes libres={free_inodes} ro={ro}",
                uuid.join("-")
            );
        }
        Err(e) => {
            eprintln!("info échouée (erreur {e:#x})");
            std::process::exit(1);
        }
    }
}

fn cmd_list(h: HANDLE, args: &[String]) {
    let id = volume_id_of(args);
    let path = args.get(1).cloned().unwrap_or_else(|| "/".into());
    let pathb = path.as_bytes();
    let mut in_buf = vec![0u8; 8 + pathb.len()];
    let r = PathReq { volume_id: id, path_len_bytes: pathb.len() as u32 };
    in_buf[..8].copy_from_slice(unsafe {
        core::slice::from_raw_parts(&r as *const _ as *const u8, 8)
    });
    in_buf[8..].copy_from_slice(pathb);
    let mut out_buf = vec![0u8; 65536];
    match ioctl(h, IOCTL_EXT4_LIST_DIR, &mut in_buf, &mut out_buf) {
        Ok(n) => {
            let count = u32::from_le_bytes(out_buf[0..4].try_into().unwrap());
            let mut off = 4usize;
            println!("{:<12} {:>10}  {:<4} {}", "mode", "taille", "ino", "nom");
            for _ in 0..count {
                let e: DirEntryWire = unsafe { core::ptr::read_unaligned(out_buf[off..].as_ptr() as *const _) };
                let name = String::from_utf8_lossy(&out_buf[off + 15..off + 15 + e.name_len as usize]).to_string();
                let kindc = match e.kind {
                    2 => 'd',
                    7 => 'l',
                    1 => '-',
                    _ => '?',
                };
                println!("{:<12} {:>10}  {:<4} {}", format!("{kindc}{:o}", e.mode & 0o7777), e.size, e.ino, name);
                off += 15 + e.name_len as usize;
                off = (off + 3) & !3;
            }
            let _ = n;
        }
        Err(e) => {
            eprintln!("list échouée (erreur {e:#x})");
            std::process::exit(1);
        }
    }
}

fn cmd_read(h: HANDLE, args: &[String]) {
    let id = volume_id_of(args);
    let path = args.get(1).cloned().expect("usage: read <vol> <chemin> <dest|->");
    let dest = args.get(2).cloned().unwrap_or_else(|| "-".into());
    let pathb = path.as_bytes();
    let mut in_buf = vec![0u8; 20 + pathb.len()];
    let r = ReadFileReq {
        volume_id: id,
        path_len_bytes: pathb.len() as u32,
        offset: 0,
        length: 0, // tout le fichier : on lit par morceaux
    };
    in_buf[..20].copy_from_slice(unsafe {
        core::slice::from_raw_parts(&r as *const _ as *const u8, 20)
    });
    in_buf[20..].copy_from_slice(pathb);
    let mut file_out: Option<std::fs::File> = None;
    let stdout = std::io::stdout();
    let mut w: Box<dyn std::io::Write> = if dest == "-" {
        Box::new(stdout.lock())
    } else {
        Box::new(std::fs::File::create(&dest).expect("création destination"))
    };
    let mut chunk = 1u32;
    loop {
        let r = ReadFileReq {
            volume_id: id,
            path_len_bytes: pathb.len() as u32,
            offset: (chunk - 1) as u64 * 1024 * 1024,
            length: 1024 * 1024,
        };
        in_buf[..20].copy_from_slice(unsafe {
            core::slice::from_raw_parts(&r as *const _ as *const u8, 20)
        });
        let mut out_buf = vec![0u8; 1024 * 1024];
        match ioctl(h, IOCTL_EXT4_READ_FILE, &mut in_buf, &mut out_buf) {
            Ok(n) if n > 0 => {
                w.write_all(&out_buf[..n]).expect("écriture");
                chunk += 1;
                if n < 1024 * 1024 {
                    break;
                }
            }
            _ => break,
        }
    }
    let _ = file_out.take();
}

fn cmd_write(h: HANDLE, args: &[String]) {
    let id = volume_id_of(args);
    let path = args.get(1).cloned().expect("usage: write <vol> <chemin> <src|->");
    let src = args.get(2).cloned().unwrap_or_else(|| "-".into());
    let data = if src == "-" {
        let mut v = Vec::new();
        std::io::Read::read_to_end(&mut std::io::stdin(), &mut v).expect("stdin");
        v
    } else {
        std::fs::read(&src).expect("lecture source")
    };
    let pathb = path.as_bytes();
    let mut in_buf = vec![0u8; 20 + pathb.len() + data.len()];
    let r = WriteFileReq {
        volume_id: id,
        path_len_bytes: pathb.len() as u32,
        offset: 0,
        data_len: data.len() as u32,
    };
    in_buf[..20].copy_from_slice(unsafe {
        core::slice::from_raw_parts(&r as *const _ as *const u8, 20)
    });
    in_buf[20..20 + pathb.len()].copy_from_slice(pathb);
    in_buf[20 + pathb.len()..].copy_from_slice(&data);
    let mut out_buf = vec![0u8; 4];
    match ioctl(h, IOCTL_EXT4_WRITE_FILE, &mut in_buf, &mut out_buf) {
        Ok(n) => println!("écrit {n} octets dans {path}"),
        Err(e) => {
            eprintln!("écriture échouée (erreur {e:#x})");
            std::process::exit(1);
        }
    }
}

fn cmd_simple_path(h: HANDLE, code: u32, args: &[String], what: &str) {
    let id = volume_id_of(args);
    let path = args.get(1).cloned().expect("chemin requis");
    let pathb = path.as_bytes();
    let mut in_buf = vec![0u8; 8 + pathb.len()];
    let r = PathReq { volume_id: id, path_len_bytes: pathb.len() as u32 };
    in_buf[..8].copy_from_slice(unsafe {
        core::slice::from_raw_parts(&r as *const _ as *const u8, 8)
    });
    in_buf[8..].copy_from_slice(pathb);
    let mut out_buf = vec![0u8; 4];
    match ioctl(h, code, &mut in_buf, &mut out_buf) {
        Ok(_) => println!("{what} : OK"),
        Err(e) => {
            eprintln!("{what} échouée (erreur {e:#x})");
            std::process::exit(1);
        }
    }
}

fn cmd_umount(h: HANDLE, args: &[String]) {
    let id = volume_id_of(args);
    let mut in_buf: Vec<u8> = {
        let r = VolumeReq { volume_id: id };
        unsafe { core::slice::from_raw_parts(&r as *const _ as *const u8, 4) }.to_vec()
    };
    let mut out_buf = vec![0u8; 4];
    match ioctl(h, IOCTL_EXT4_CLOSE, &mut in_buf, &mut out_buf) {
        Ok(_) => println!("volume {id} démonté (journal finalisé, état propre)"),
        Err(e) => {
            eprintln!("umount échoué (erreur {e:#x})");
            std::process::exit(1);
        }
    }
}

fn usage() -> ! {
    eprintln!(
        "ext4ctl — contrôle du pilote ext4drv\n\n\
         USAGE: ext4ctl <commande> [args...]\n\n\
         mount    <image|\\\\.\\D:> [ro]   monte un volume ext4 (id retourné)\n\
         umount   <vol>                  démonte proprement\n\
         info     <vol>                  infos volume\n\
         list     <vol> [chemin]         liste un répertoire\n\
         cat      <vol> <fichier>        affiche un fichier\n\
         extract  <vol> <fichier> <dest> extrait un fichier\n\
         write    <vol> <chemin> <src|-> écrit un fichier\n\
         create   <vol> <chemin>         crée un fichier vide\n\
         mkdir    <vol> <chemin>         crée un répertoire\n\
         rm       <vol> <chemin>         supprime un fichier\n\
         rmdir    <vol> <chemin>         supprime un répertoire vide\n\n\
         Le chemin du mount doit être Win32 (ex: \\\\.\\D: ou C:\\images\\ext4.img).\n\
         Les chemins internes suivent la convention ext4 (/docs/fichier).\n\
\n\
Commandes FSD (phase 2 — volume natif dans l'Explorateur) :\n\
  fsd-mount <\\\\.\\Lettre: | \\\\.\\HarddiskVolumeN>  force le montage ext4 d'un volume\n\
  fsd-unmount <Lettre:>                                 démonte proprement (flush + journal)\n\
  fsd-status                                            liste les volumes ext4 montés par le FSD"
    );
    std::process::exit(2);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        usage();
    }
    let h = driver_handle();
    let cmd = args[0].as_str();
    let rest = &args[1..];
    match cmd {
        "mount" => cmd_mount(h, rest),
        "umount" => cmd_umount(h, rest),
        "info" => cmd_volume_info(h, rest),
        "list" => cmd_list(h, rest),
        "cat" => cmd_read(h, rest),
        "extract" => cmd_read(h, rest),
        "write" => cmd_write(h, rest),
        "create" => cmd_simple_path(h, IOCTL_EXT4_CREATE_FILE, rest, "create"),
        "mkdir" => cmd_simple_path(h, IOCTL_EXT4_MKDIR, rest, "mkdir"),
        "rm" => cmd_simple_path(h, IOCTL_EXT4_UNLINK, rest, "rm"),
        "rmdir" => cmd_simple_path(h, IOCTL_EXT4_RMDIR, rest, "rmdir"),
        // --- commandes FSD (phase 2) : pas d'IOCTL, manipulation Win32 directe
        "fsd-mount" => cmd_fsd_mount(rest),
        "fsd-unmount" => cmd_fsd_unmount(rest),
        "fsd-status" => cmd_fsd_status(rest),
        _ => usage(),
    }
    unsafe {
        CloseHandle(h);
    }
}


// ---------------------------------------------------------------------------
// Commandes FSD (phase 2) : le montage passe par l'ouverture du volume
// (IopParseDevice → IRP_MN_MOUNT_VOLUME), le reste par l'API Win32 standard.
// ---------------------------------------------------------------------------

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// Déclenche le montage : ouvre \\.\X: ou \\.\HarddiskVolumeN en accès brut.
/// Si le volume est ext4, le pilote le monte (IRP_MN_MOUNT_VOLUME) et le
/// mount manager lui attribue une lettre de lecteur.
fn cmd_fsd_mount(rest: &[String]) {
    if rest.is_empty() {
        eprintln!("usage: ext4ctl fsd-mount <\\\\.\\Lettre: | \\\\.\\HarddiskVolumeN>");
        std::process::exit(2);
    }
    let target = if rest[0].starts_with("\\\\.\\") {
        rest[0].clone()
    } else {
        format!("\\\\.\\{}", rest[0])
    };
    let mut wpath = wide(&target);
    wpath.push(0);
    println!("ext4ctl : tentative de montage de {target} ...");
    unsafe {
        let h = CreateFileW(
            wpath.as_ptr(),
            0x8000_0000 | 0x4000_0000, // GENERIC_READ | GENERIC_WRITE
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0, // pas de flag
            0,
        );
        if h == INVALID_HANDLE_VALUE {
            // réessaie en lecture seule
            let h2 = CreateFileW(
                wpath.as_ptr(),
                0x8000_0000, // GENERIC_READ
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                0,
            );
            if h2 == INVALID_HANDLE_VALUE {
                let e = GetLastError();
                eprintln!("échec d'ouverture ({e:#x}) : le volume n'est peut-être pas ext4");
                std::process::exit(1);
            }
            CloseHandle(h2);
        } else {
            CloseHandle(h);
        }
    }
    println!("montage déclenché. Le mount manager attribue la lettre de lecteur ;\nvérifiez avec fsd-status ou l'Explorateur (F5 pour rafraîchir).");
}

/// Démonte proprement : FSCTL_DISMOUNT_VOLUME sur la lettre.
fn cmd_fsd_unmount(rest: &[String]) {
    if rest.is_empty() {
        eprintln!("usage: ext4ctl fsd-unmount <Lettre:>");
        std::process::exit(2);
    }
    let letter = rest[0].trim_end_matches(':').to_string();
    let target = format!("\\\\.\\{}:", letter);
    let mut wpath = wide(&target);
    wpath.push(0);
    const FSCTL_DISMOUNT_VOLUME: u32 = 0x0009_0020u32; // CTL_CODE(9, 8, BUFFERED, ANY)
    unsafe {
        let h = CreateFileW(
            wpath.as_ptr(),
            0xC000_0000, // GENERIC_READ | GENERIC_WRITE
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            0,
        );
        if h == INVALID_HANDLE_VALUE {
            let e = GetLastError();
            eprintln!("échec d'ouverture de {target} ({e:#x})");
            std::process::exit(1);
        }
        let mut ret: u32 = 0;
        let ok = DeviceIoControl(
            h,
            FSCTL_DISMOUNT_VOLUME,
            std::ptr::null(),
            0,
            std::ptr::null_mut(),
            0,
            &mut ret,
            std::ptr::null_mut(),
        );
        CloseHandle(h);
        if ok == 0 {
            let e = GetLastError();
            eprintln!("démontage refusé ({e:#x}) : fermez les fichiers ouverts puis réessayez");
            std::process::exit(1);
        }
    }
    println!("volume {target} démonté (journal ext4 finalisé, état propre).");
}

/// Statut : liste les volumes ext4 "à la mode ext4fsd" en scannant les lettres.
fn cmd_fsd_status(_rest: &[String]) {
    println!("Recherche des volumes ext4 montés (A:..Z:) ...");
    let mut found = 0u32;
    for l in b'A'..=b'Z' {
        let target = format!("\\\\.\\{}:", l as char);
        let mut wpath = wide(&target);
        wpath.push(0);
        unsafe {
            let h = CreateFileW(
                wpath.as_ptr(),
                0, // query attributes only
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                0,
            );
            if h == INVALID_HANDLE_VALUE {
                continue;
            }
            // volume info via GetVolumeInformationW
            let mut fsname = [0u16; 64];
            let mut volname = [0u16; 64];
            let mut serial: u32 = 0;
            let mut flags: u32 = 0;
            let mut maxlen: u32 = 0;
            let ok = windows_sys::Win32::Storage::FileSystem::GetVolumeInformationW(
                format!("{}:\\\0", l as char).encode_utf16().collect::<Vec<u16>>().as_ptr(),
                volname.as_mut_ptr(),
                volname.len() as u32,
                &mut serial,
                &mut maxlen,
                &mut flags,
                fsname.as_mut_ptr(),
                fsname.len() as u32,
            );
            CloseHandle(h);
            if ok != 0 {
                let fs = from_wide(&fsname);
                if fs.eq_ignore_ascii_case("ext4") || fs.eq_ignore_ascii_case("EXT2/3/4") {
                    found += 1;
                    println!(
                        "  {}:\\  label=\"{}\"  fs={fs}  serial={serial:08X}",
                        l as char,
                        from_wide(&volname)
                    );
                }
            }
        }
    }
    if found == 0 {
        println!("aucun volume ext4 monté trouvé.");
    }
}
