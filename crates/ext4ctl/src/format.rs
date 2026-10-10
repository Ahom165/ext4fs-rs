//! ext4ctl format — formatage ext4 d'un volume Windows (user-mode).
//!
//! Chemin classique des outils de formatage :
//!   1. ouverture du volume `\\.\X:` en GENERIC_READ|WRITE (partage r/w) ;
//!   2. FSCTL_LOCK_VOLUME : accès exclusif (refuse si fichiers ouverts) ;
//!   3. FSCTL_DISMOUNT_VOLUME : démonte le FS présent ;
//!   4. taille via IOCTL_DISK_GET_LENGTH_INFO, secteur via
//!      IOCTL_DISK_GET_DRIVE_GEOMETRY ;
//!   5. écriture de l'image ext4 par le moteur `ext4_core::mkfs` à travers
//!      un `BlockDevice` sur HANDLE (écritures alignées au secteur, RMW) ;
//!   6. FSCTL_UNLOCK_VOLUME.
//!
//! Aucun IOCTL spécifique au pilote n'est nécessaire : le formatage est
//! purement user-mode (comme format.com). Le volume devient ensuite
//! montable par le FSD (`ext4ctl fsd-mount \\.\X:`) ou par Linux.
//!
//! NOTE alignement : les handles de volume imposent des E/S alignées au
//! secteur natif (512 sur la plupart des supports, 4096 sur les SSD 4Kn).
//! `RawVolume` fusionne les écritures du moteur dans des secteurs entiers
//! (lecture-modification-écriture) pour rester correct dans tous les cas.

use ext4_core::blockdev::{check_range, BlockDevice};
use ext4_core::error::{Error, Result};
use ext4_core::mkfs::{mkfs, MkfsOptions};
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_BEGIN, FILE_SHARE_READ, FILE_SHARE_WRITE, FlushFileBuffers, OPEN_EXISTING,
    ReadFile, SetFilePointerEx, WriteFile,
};
use windows_sys::Win32::System::IO::DeviceIoControl;

// ---- Codes de contrôle (valeur stables, pas besoin de Win32_System_Ioctl) --
const FSCTL_LOCK_VOLUME: u32 = 0x0009_0018; // CTL_CODE(9, 6, BUFFERED, ANY)
const FSCTL_UNLOCK_VOLUME: u32 = 0x0009_001C; // CTL_CODE(9, 7, BUFFERED, ANY)
const FSCTL_DISMOUNT_VOLUME: u32 = 0x0009_0020; // CTL_CODE(9, 8, BUFFERED, ANY)
const IOCTL_DISK_GET_LENGTH_INFO: u32 = 0x0007_4450; // CTL_CODE(7, 0x114, BUFFERED, READ)
const IOCTL_DISK_GET_DRIVE_GEOMETRY: u32 = 0x0007_0000; // CTL_CODE(7, 0, BUFFERED, ANY)
const IOCTL_DISK_GET_DRIVE_GEOMETRY_EX: u32 = 0x0007_00A0; // CTL_CODE(7, 0x28, BUFFERED, ANY)

const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;

// Garde-fous du moteur v1 (calibré et testé) :
const MIN_FORMAT_BYTES: u64 = 1024 * 1024; // 1 MiB
const MAX_FORMAT_BYTES: u64 = 16 * 1024 * 1024 * 1024 * 1024u64; // 16 TiB

fn wide0(s: &str) -> Vec<u16> {
    let mut w: Vec<u16> = s.encode_utf16().collect();
    w.push(0);
    w
}

fn last_error() -> u32 {
    unsafe { GetLastError() }
}

// ---------------------------------------------------------------------------
// RawVolume : BlockDevice sur handle de volume Windows
// ---------------------------------------------------------------------------

struct RawVolume {
    h: HANDLE,
    size: u64,
    sector: u32,
    /// Secteur entier en attente d'écriture (offset aligné, contenu complet).
    pending: Option<(u64, Vec<u8>)>,
}

impl RawVolume {
    unsafe fn pos_read(&self, off: u64, buf: &mut [u8]) -> Result<()> {
        unsafe {
            let mut np: i64 = 0;
            if SetFilePointerEx(self.h, off as i64, &mut np, FILE_BEGIN) == 0 {
                return Err(Error::Device(last_error() as i32));
            }
            let mut done: u32 = 0;
            let ok = ReadFile(
                self.h,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut done,
                std::ptr::null_mut(),
            );
            if ok == 0 || done as usize != buf.len() {
                return Err(Error::Device(last_error() as i32));
            }
        }
        Ok(())
    }

    unsafe fn pos_write(&self, off: u64, buf: &[u8]) -> Result<()> {
        unsafe {
            let mut np: i64 = 0;
            if SetFilePointerEx(self.h, off as i64, &mut np, FILE_BEGIN) == 0 {
                return Err(Error::Device(last_error() as i32));
            }
            let mut done: u32 = 0;
            let ok = WriteFile(
                self.h,
                buf.as_ptr(),
                buf.len() as u32,
                &mut done,
                std::ptr::null_mut(),
            );
            if ok == 0 || done as usize != buf.len() {
                return Err(Error::Device(last_error() as i32));
            }
        }
        Ok(())
    }

    fn flush_pending(&mut self) -> Result<()> {
        if let Some((off, data)) = self.pending.take() {
            unsafe {
                self.pos_write(off, &data)?;
            }
        }
        Ok(())
    }

    /// Programme `buf` à `off` : fusion dans les secteurs entiers (RMW).
    fn stage(&mut self, off: u64, buf: &[u8]) -> Result<()> {
        let sec = self.sector as u64;
        let end = off + buf.len() as u64;
        let mut pos = off;
        while pos < end {
            let rs = pos / sec * sec;
            let re = rs + sec;
            let take = (re.min(end) - pos) as usize;
            let src = (pos - off) as usize;
            if let Some((poff, pdata)) = self.pending.as_mut() {
                if *poff == rs {
                    pdata[(pos - rs) as usize..(pos - rs) as usize + take]
                        .copy_from_slice(&buf[src..src + take]);
                    pos = re.min(end);
                    continue;
                }
                self.flush_pending()?;
            }
            let mut data = alloc_sector(self.sector as usize);
            // RMW : ne lire que si le secteur n'est pas couvert en entier
            // (secteur final du volume exclu : jamais lu, toujours écrit).
            let partial = (pos > rs) || (end < re);
            if partial && rs + sec <= self.size {
                unsafe {
                    self.pos_read(rs, &mut data)?;
                }
            }
            data[(pos - rs) as usize..(pos - rs) as usize + take]
                .copy_from_slice(&buf[src..src + take]);
            self.pending = Some((rs, data));
            pos = re.min(end);
        }
        Ok(())
    }
}

fn alloc_sector(n: usize) -> Vec<u8> {
    vec![0u8; n]
}

impl BlockDevice for RawVolume {
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        check_range(offset, buf.len(), self.size)?;
        // Cohérence lecture-après-écriture : d'abord vider le tampon.
        self.flush_pending()?;
        let sec = self.sector as u64;
        if offset % sec == 0 && buf.len() as u64 % sec == 0 {
            unsafe {
                return self.pos_read(offset, buf);
            }
        }
        let rs = offset / sec * sec;
        let re = ((offset as usize + buf.len() + self.sector as usize - 1)
            / self.sector as usize) as u64
            * sec;
        let mut tmp = alloc_sector((re - rs) as usize);
        unsafe {
            self.pos_read(rs, &mut tmp)?;
        }
        let start = (offset - rs) as usize;
        buf.copy_from_slice(&tmp[start..start + buf.len()]);
        Ok(())
    }

    fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<()> {
        check_range(offset, buf.len(), self.size)?;
        self.stage(offset, buf)
    }

    fn size(&mut self) -> Result<u64> {
        Ok(self.size)
    }

    fn is_read_only(&self) -> bool {
        false
    }

    fn flush(&mut self) -> Result<()> {
        self.flush_pending()?;
        // Force l'écriture du cache Windows sur le support (durabilité).
        unsafe {
            if FlushFileBuffers(self.h) == 0 {
                return Err(Error::Device(last_error() as i32));
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Options + CLI
// ---------------------------------------------------------------------------

/// UUID pseudo-aléatoire (RandomState tire sa graine de l'OS). Les UUID de
/// FS ne sont pas des secrets : l'entropie std suffit ; évitons une
/// dépendance CNG pour rester minimal.
fn gen_uuid() -> [u8; 16] {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    use std::time::{SystemTime, UNIX_EPOCH};
    let mut out = [0u8; 16];
    for chunk in out.chunks_exact_mut(8) {
        let mut h = RandomState::new().build_hasher();
        let t = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        h.write_u64(t);
        chunk.copy_from_slice(&h.finish().to_le_bytes());
    }
    // Marqueur RFC4122 v4 (cosmétique mais conventionnel).
    out[6] = (out[6] & 0x0F) | 0x40;
    out[8] = (out[8] & 0x3F) | 0x80;
    out
}

fn parse_uuid(s: &str) -> Option<[u8; 16]> {
    let hex: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if hex.len() != 32 {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

fn fmt_uuid(u: &[u8; 16]) -> String {
    let hex: Vec<String> = u.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        hex[0..4].join(""),
        hex[4..6].join(""),
        hex[6..8].join(""),
        hex[8..10].join(""),
        hex[10..16].join("")
    )
}

fn open_volume(target: &str) -> HANDLE {
    let w = wide0(target);
    unsafe {
        let h = CreateFileW(
            w.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        );
        if h == INVALID_HANDLE_VALUE {
            let e = last_error();
            eprintln!(
                "échec d'ouverture de {target} (erreur {e:#x})\n  — exécutez en tant qu'administrateur ;\n  — fermez l'Explorateur sur cette lettre et toute fenêtre qui l'utilise ;\n  — une pagefile/hibernation sur ce volume bloque aussi l'accès."
            );
            std::process::exit(1);
        }
        h
    }
}

fn volume_ioctl(h: HANDLE, code: u32, out_len: usize) -> Result<Vec<u8>> {
    let mut out = vec![0u8; out_len];
    let mut ret: u32 = 0;
    let ok = unsafe {
        DeviceIoControl(
            h,
            code,
            std::ptr::null(),
            0,
            out.as_mut_ptr() as *mut _,
            out.len() as u32,
            &mut ret,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(Error::Device(last_error() as i32));
    }
    Ok(out)
}

fn ioctl_err_phrase(e: &Error, what: &str) -> ! {
    let code = match e {
        Error::Device(c) => *c,
        _ => 0,
    };
    eprintln!("{what} échoué (erreur {code:#x})");
    if code == 5 {
        eprintln!("  accès refusé : lancez ext4ctl en tant qu'administrateur.");
    } else if code == (0xC000_0043u32 as i32) || code == 32 {
        eprintln!("  le volume est utilisé (fichiers ouverts) : verrou impossible.");
    }
    std::process::exit(1);
}

/// Analyse des arguments de format.
struct FormatArgs {
    target: String,
    label: Vec<u8>,
    block_log: u8,
    journal_mib: u32,
    inode_ratio: u32,
    uuid: [u8; 16],
    reserved_pct: u32,
    yes: bool,
}

fn parse_args(rest: &[String]) -> FormatArgs {
    let mut a = FormatArgs {
        target: String::new(),
        label: Vec::new(),
        block_log: 2,
        journal_mib: 4,
        inode_ratio: 16384,
        uuid: gen_uuid(),
        reserved_pct: 5,
        yes: false,
    };
    let mut it = rest.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--label" => {
                a.label = it.next().expect("--label <nom>").bytes().take(16).collect();
            }
            "--no-journal" => a.journal_mib = 0,
            "--journal" => {
                a.journal_mib = it
                    .next()
                    .expect("--journal <MiB>")
                    .parse()
                    .expect("MiB numérique");
            }
            "--1k" => a.block_log = 0,
            "--2k" => a.block_log = 1,
            "--4k" => a.block_log = 2,
            "--ratio" => {
                a.inode_ratio = it
                    .next()
                    .expect("--ratio <octets/inode>")
                    .parse()
                    .expect("ratio numérique");
            }
            "--uuid" => {
                a.uuid = parse_uuid(it.next().expect("--uuid <hex>")).expect("uuid 32 hex");
            }
            "--reserved" => {
                a.reserved_pct = it
                    .next()
                    .expect("--reserved <%>")
                    .parse()
                    .expect("pourcentage numérique");
            }
            "--yes" | "-y" => a.yes = true,
            s if s.starts_with("--") => {
                eprintln!("option inconnue : {s}");
                std::process::exit(2);
            }
            s => {
                if !a.target.is_empty() {
                    eprintln!("argument superflu : {s}");
                    std::process::exit(2);
                }
                a.target = s.to_string();
            }
        }
    }
    if a.target.is_empty() {
        eprintln!("usage: ext4ctl format <\\\\.\\Lettre: | \\\\.\\HarddiskVolumeN | \\\\.\\PhysicalDriveN> [options]");
        std::process::exit(2);
    }
    a
}

pub fn cmd_format(rest: &[String]) {
    let a = parse_args(rest);
    // Normalisation : "X:" → "\\.\X:"
    let target = if a.target.starts_with("\\\\.\\") {
        a.target.clone()
    } else {
        format!("\\\\.\\{}", a.target.trim_end_matches(':'))
    };
    println!(
        "FORMATAGE EXT4 de {target}\n  taille-bloc : {} octets\n  journal     : {}\n  label       : {:?}\n  ratio       : {} o/inode\n  réservé     : {} %\n\n  ⚠  TOUTES les données de {target} seront PERDUES.",
        1024u32 << a.block_log,
        if a.journal_mib == 0 {
            "aucun".into()
        } else {
            format!("{} MiB", a.journal_mib)
        },
        String::from_utf8_lossy(&a.label),
        a.inode_ratio,
        a.reserved_pct,
    );
    if !a.yes {
        println!("\nTapez le nom EXACT du volume (« {target} ») pour confirmer :");
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).expect("stdin");
        if line.trim().trim_end_matches(':') != target.trim_end_matches(':') {
            eprintln!("confirmation différente — annulé, aucune écriture.");
            std::process::exit(2);
        }
    }

    // 1) Ouverture du volume brut
    let h = open_volume(&target);

    // 2) Taille exacte + secteur natif.
    //    GET_LENGTH_INFO (répondu par partmgr) peut être refusé avec
    //    INVALID_DEVICE_REQUEST sur certains volumes virtuels tout juste
    //    attachés (constaté sur VHD Windows 11 Insider, erreur 0x1).
    //    Chaîne de repli : GET_DRIVE_GEOMETRY_EX (DiskSize exact, répondu
    //    par disk.sys/vhdmp) puis produit-geometry (borne haute, dernier
    //    recours — le format reste valide, juste calé sur cette taille).
    let geo_ex = volume_ioctl(h, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, 64).ok();
    let (size, sector) = if let Ok(li) = volume_ioctl(h, IOCTL_DISK_GET_LENGTH_INFO, 8) {
        let size = i64::from_le_bytes(li[0..8].try_into().unwrap()) as u64;
        let geo = match &geo_ex {
            Some(g) => g[0..24].to_vec(),
            None => volume_ioctl(h, IOCTL_DISK_GET_DRIVE_GEOMETRY, 24)
                .unwrap_or_else(|e| ioctl_err_phrase(&e, "IOCTL_DISK_GET_DRIVE_GEOMETRY")),
        };
        let sector = u32::from_le_bytes(geo[20..24].try_into().unwrap()).max(512);
        (size, sector)
    } else if let Some(g) = &geo_ex {
        let size = i64::from_le_bytes(g[24..32].try_into().unwrap()) as u64;
        let sector = u32::from_le_bytes(g[20..24].try_into().unwrap()).max(512);
        eprintln!("note : taille via GET_DRIVE_GEOMETRY_EX (GET_LENGTH_INFO refusé par ce volume)");
        (size, sector)
    } else {
        let geo = volume_ioctl(h, IOCTL_DISK_GET_DRIVE_GEOMETRY, 24)
            .unwrap_or_else(|e| ioctl_err_phrase(&e, "IOCTL_DISK_GET_DRIVE_GEOMETRY"));
        let sector = u32::from_le_bytes(geo[20..24].try_into().unwrap()).max(512);
        let cyl = i64::from_le_bytes(geo[0..8].try_into().unwrap()) as u64;
        let tpc = u32::from_le_bytes(geo[12..16].try_into().unwrap()) as u64;
        let spt = u32::from_le_bytes(geo[16..20].try_into().unwrap()) as u64;
        eprintln!("note : taille via produit-geometry (borne haute) — GET_LENGTH_INFO refusé");
        (cyl * tpc * spt * sector as u64, sector)
    };
    if size < MIN_FORMAT_BYTES || size > MAX_FORMAT_BYTES {
        eprintln!(
            "volume de {size} octets hors bornes supportées ({MIN_FORMAT_BYTES}..{MAX_FORMAT_BYTES})"
        );
        std::process::exit(1);
    }

    // 3) Verrou exclusif + démontage du FS existant
    if let Err(e) = volume_ioctl(h, FSCTL_LOCK_VOLUME, 0) {
        ioctl_err_phrase(&e, "FSCTL_LOCK_VOLUME");
    }
    if volume_ioctl(h, FSCTL_DISMOUNT_VOLUME, 0).is_err() {
        // Non bloquant : sur un volume brut (VHD/partition neuve, aucun FS
        // monte), le dismount peut echouer. Le verrou exclusif obtenu
        // ci-dessus suffit deja (aucun autre handle ouvert sur le volume).
        eprintln!("note : dismount ignore (volume brut ?)");
    }

    // 4) Formatage par le moteur ext4-core
    let opts = MkfsOptions {
        block_log: a.block_log,
        label: a.label.clone(),
        uuid: a.uuid,
        journal_mib: a.journal_mib,
        inode_ratio: a.inode_ratio,
        hash_seed: [0; 4],
        hash_version: 1,
        reserved_pct: a.reserved_pct,
    };
    let mut dev = RawVolume {
        h,
        size,
        sector,
        pending: None,
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as u32)
        .unwrap_or(0);
    let res = mkfs(&mut dev, &opts, now);
    // flush impératif AVANT tout (même en cas d'erreur, écrire ce qui est prêt)
    let flushed = dev.flush();
    if let Err(e) = res {
        let msg = match &e {
            Error::Corrupt(m) => m.to_string(),
            _ => "erreur moteur".to_string(),
        };
        eprintln!("mkfs a échoué : {msg}");
        let _ = volume_ioctl(h, FSCTL_UNLOCK_VOLUME, 0);
        std::process::exit(1);
    }
    if let Err(e) = flushed {
        eprintln!("flush final a échoué : {e:?}");
        let _ = volume_ioctl(h, FSCTL_UNLOCK_VOLUME, 0);
        std::process::exit(1);
    }

    // 5) Déverrouillage
    if let Err(e) = volume_ioctl(h, FSCTL_UNLOCK_VOLUME, 0) {
        ioctl_err_phrase(&e, "FSCTL_UNLOCK_VOLUME");
    }
    unsafe {
        CloseHandle(h);
    }

    println!(
        "✓ {target} formaté en ext4 ({:.1} GiB, secteur {sector})\n  label : {:?}\n  uuid  : {}\n\nSuite : ext4ctl fsd-mount {target}\n  → la lettre apparaît dans l'Explorateur (montage FSD natif).",
        size as f64 / (1024.0 * 1024.0 * 1024.0),
        String::from_utf8_lossy(&a.label),
        fmt_uuid(&a.uuid),
    );
}
