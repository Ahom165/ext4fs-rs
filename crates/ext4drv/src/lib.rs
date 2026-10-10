//! # ext4drv — pilote système de fichiers ext4 pour Windows (WDM, wdk-rs)
//!
//! Phase 1 (conservée) : device de contrôle `\\.\Ext4Fs` + IOCTLs pour
//! explorer les images/labels ext4 en user-mode (ext4cli / ext4ctl).
//!
//! Phase 2 (FSD) : device système de fichiers sans nom enregistré auprès de
//! l'I/O manager (`IoRegisterFileSystem`). Quand Windows tente de monter un
//! volume (branche USB, mount manager, ouverture d'un volume), le pilote
//! reçoit IRP_MN_MOUNT_VOLUME, vérifie le superblock ext4, crée le device
//! volume, renseigne la VPB et notifie le mount manager → lettre de lecteur
//! dans l'Explorateur, lecture/écriture/copies/renommages natifs.
//!
//! Le cœur ext4 (ext4-core, no_std) fait tout le travail disque : extents,
//! HTree, journal jbd2, checksums metadata_csum.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

#[cfg(not(test))]
extern crate wdk_panic;

mod fsd;
mod fsd_create;
mod fsd_fsctl;
mod fsd_info;
mod fsd_io;
mod fsd_types;
mod fsd_xattr;
mod ioctl_defs;
mod ntdefs;
mod ntshim;
mod uni;
mod volume;
mod zpdev;

use core::ffi::c_void;
use ntdefs as nd;
use wdk::println;
use wdk_sys::{
    ntddk::{IoCreateDevice, IoCreateSymbolicLink, IoDeleteDevice, IoDeleteSymbolicLink},
    DEVICE_OBJECT, DRIVER_OBJECT, PDRIVER_OBJECT, PIRP, UNICODE_STRING,
};

#[cfg(not(test))]
use wdk_alloc::WdkAllocator;

#[cfg(not(test))]
#[global_allocator]
static GLOBAL_ALLOCATOR: WdkAllocator = WdkAllocator;

use alloc::boxed::Box;
use alloc::string::ToString;
use alloc::vec;
use alloc::vec::Vec;
use ioctl_defs as proto;

const NT_DEVICE_NAME: &[u16] = proto::DEVICE_NAME;
const DOS_DEVICE_NAME: &[u16] = proto::SYMLINK_NAME;

/// Extension du device de contrôle (phase 1) : registre des images montées.
struct DeviceExtension {
    registry: volume::VolumeRegistry,
}

static mut CONTROL_DEVICE: *mut DEVICE_OBJECT = core::ptr::null_mut();

#[inline]
pub fn control_device() -> *mut DEVICE_OBJECT {
    unsafe { core::ptr::read(core::ptr::addr_of!(CONTROL_DEVICE)) }
}
#[inline]
pub fn set_control_device(d: *mut DEVICE_OBJECT) {
    unsafe { core::ptr::write(core::ptr::addr_of_mut!(CONTROL_DEVICE), d) }
}

// ---------------------------------------------------------------------------
// DriverEntry / Unload
// ---------------------------------------------------------------------------

#[cfg_attr(not(test), unsafe(export_name = "DriverEntry"))]
pub unsafe extern "system" fn driver_entry(
    driver: &mut DRIVER_OBJECT,
    _registry_path: *const UNICODE_STRING,
) -> i32 {
    driver.DriverUnload = Some(driver_unload);

    // --- device de contrôle \\.\Ext4Fs (phase 1) ---
    let mut nt_name = UNICODE_STRING {
        Length: (NT_DEVICE_NAME.len() * 2) as u16,
        MaximumLength: (NT_DEVICE_NAME.len() * 2) as u16,
        Buffer: NT_DEVICE_NAME.as_ptr() as *mut u16,
    };
    let mut dos_name = UNICODE_STRING {
        Length: (DOS_DEVICE_NAME.len() * 2) as u16,
        MaximumLength: (DOS_DEVICE_NAME.len() * 2) as u16,
        Buffer: DOS_DEVICE_NAME.as_ptr() as *mut u16,
    };
    let mut control: *mut DEVICE_OBJECT = core::ptr::null_mut();
    let status = IoCreateDevice(
        driver as PDRIVER_OBJECT,
        core::mem::size_of::<DeviceExtension>() as u32,
        &mut nt_name,
        proto::FILE_DEVICE_UNKNOWN,
        0,
        0,
        &mut control,
    );
    if status != 0 {
        println!("ext4drv: IoCreateDevice contrôle échoué {status:#x}");
        return status;
    }
    let ext = (*control).DeviceExtension as *mut DeviceExtension;
    core::ptr::write(ext, DeviceExtension {
        registry: volume::VolumeRegistry::new(),
    });
    let status = IoCreateSymbolicLink(&mut dos_name, &mut nt_name);
    if status != 0 {
        IoDeleteDevice(control);
        println!("ext4drv: IoCreateSymbolicLink échoué {status:#x}");
        return status;
    }
    (*control).Flags &= !nd::DO_DEVICE_INITIALIZING;
    set_control_device(control);

    // --- device système de fichiers (phase 2 : FSD) ---
    let mut fsdev: *mut DEVICE_OBJECT = core::ptr::null_mut();
    let status = IoCreateDevice(
        driver as PDRIVER_OBJECT,
        core::mem::size_of::<fsd::FsDevExt>() as u32,
        core::ptr::null_mut(), // sans nom
        nd::FILE_DEVICE_DISK_FILE_SYSTEM,
        0,
        0,
        &mut fsdev,
    );
    if status != 0 {
        println!("ext4fsd: IoCreateDevice FS échoué {status:#x}");
        IoDeleteSymbolicLink(&mut dos_name);
        IoDeleteDevice(control);
        set_control_device(core::ptr::null_mut());
        return status;
    }
    (*fsdev).DeviceExtension = Box::into_raw(Box::new(fsd::FsDevExt::new())) as *mut c_void;
    (*fsdev).Flags &= !nd::DO_DEVICE_INITIALIZING;
    fsd::set_fs_device(fsdev);

    // --- table de dispatch : TOUT passe par le routeur FSD ---
    for slot in (*driver).MajorFunction.iter_mut() {
        *slot = Some(fsd::fsd_dispatch);
    }

    // enregistre le système de fichiers (lettre de lecteur possible)
    let status = fsd::register_filesystem();
    if status != 0 {
        println!("ext4fsd: IoRegisterFileSystem échoué {status:#x}");
    }

    // v2 : worker de commit périodique (flush Cc + journal toutes les 5 s)
    let wst = fsd_xattr::start_commit_worker();
    if wst != 0 {
        println!("ext4fsd: worker de commit non démarré {wst:#x}");
    }

    println!("ext4fsd: prêt — FSD actif, \\\\.\\Ext4Fs disponible (images)");
    0
}

unsafe extern "C" fn driver_unload(_driver: *mut DRIVER_OBJECT) {
    // v2 : arrête le worker de commit avant le démontage (pas de course)
    fsd_xattr::stop_commit_worker();

    fsd::unregister_filesystem();

    // volumes FSD : démontage forcé (flush)
    if let Some(fsdev) = core::ptr::read(core::ptr::addr_of!(fsd::FS_DEVICE)).as_mut() {
        let ext = (*fsdev).DeviceExtension as *mut fsd::FsDevExt;
        if !ext.is_null() {
            let vols: Vec<*mut fsd_types::Vcb> = (*ext).volumes.clone();
            for v in vols {
                let _ = fsd::dismount(v, true);
            }
            core::ptr::drop_in_place(ext);
        }
    }
    fsd::set_fs_device(core::ptr::null_mut());

    // device de contrôle (phase 1)
    let device = control_device();
    if device.is_null() {
        return;
    }
    let ext = (*device).DeviceExtension as *mut DeviceExtension;
    while let Some(id) = (*ext).registry.first_volume_id() {
        let _ = (*ext).registry.unmount(id);
    }
    core::ptr::drop_in_place(ext);
    let mut dos_name = UNICODE_STRING {
        Length: (DOS_DEVICE_NAME.len() * 2) as u16,
        MaximumLength: (DOS_DEVICE_NAME.len() * 2) as u16,
        Buffer: DOS_DEVICE_NAME.as_ptr() as *mut u16,
    };
    IoDeleteSymbolicLink(&mut dos_name);
    IoDeleteDevice(device);
    set_control_device(core::ptr::null_mut());
    println!("ext4drv: déchargé");
}

// ---------------------------------------------------------------------------
// Device de contrôle (phase 1 : protocol IOCTL pour images) — conservé tel quel
// ---------------------------------------------------------------------------

/// Routeur pour le device de contrôle ET le device FS (IOCTLs images).
pub unsafe extern "system" fn dispatch_control(
    device: *mut DEVICE_OBJECT,
    irp: PIRP,
) -> i32 {
    let stack: *mut wdk_sys::IO_STACK_LOCATION = (*irp).Tail.Overlay.__bindgen_anon_2.__bindgen_anon_1.CurrentStackLocation;

    // CREATE/CLOSE/CLEANUP sur le device de contrôle : succès simple.
    // Bug corrigé : CreateFile(\\.\Ext4Fs) envoyait MJ_CREATE ici ; l'ancien
    // code lisait directement DeviceIoControl.IoControlCode (union : champ
    // non valide hors MJ_DEVICE_CONTROL), tombait dans le bras
    // `_ => INVALID_DEVICE_REQUEST` et l'ouverture échouait en erreur Win32 1
    // (ERROR_INVALID_FUNCTION). Même modèle que le device FS (fsd.rs).
    let major = fsd_types::sp_major(stack);
    if major == nd::MJ_CREATE || major == nd::MJ_CLOSE || major == nd::MJ_CLEANUP {
        complete(irp, 0, 0);
        return 0;
    }
    if major != nd::MJ_DEVICE_CONTROL {
        complete(irp, nd::st::INVALID_DEVICE_REQUEST, 0);
        return nd::st::INVALID_DEVICE_REQUEST;
    }

    let code = (*stack).Parameters.DeviceIoControl.IoControlCode;
    let in_len = (*stack).Parameters.DeviceIoControl.InputBufferLength as usize;
    let out_len = (*stack).Parameters.DeviceIoControl.OutputBufferLength as usize;
    let sysbuf = (*irp).AssociatedIrp.SystemBuffer as *mut u8;

    // les IOCTLs images ne sont valides que sur le device de contrôle
    if device != control_device() {
        complete(irp, nd::st::INVALID_DEVICE_REQUEST, 0);
        return nd::st::INVALID_DEVICE_REQUEST;
    }

    let ext = (*device).DeviceExtension as *mut DeviceExtension;

    let input = || -> &[u8] {
        if sysbuf.is_null() || in_len == 0 {
            &[]
        } else {
            core::slice::from_raw_parts(sysbuf, in_len)
        }
    };

    let result: Result<usize, i32> = match code {
        proto::IOCTL_EXT4_OPEN_IMAGE => handle_open(&(*ext).registry, input(), sysbuf, out_len),
        proto::IOCTL_EXT4_CLOSE => {
            if let Some(req) = parse::<proto::VolumeReq>(input()) {
                (*ext).registry.unmount(req.volume_id).map(|_| 0)
            } else {
                Err(nd::st::INVALID_PARAMETER)
            }
        }
        proto::IOCTL_EXT4_VOLUME_INFO => handle_volume_info(&(*ext).registry, input(), sysbuf, out_len),
        proto::IOCTL_EXT4_LIST_DIR => handle_list_dir(&(*ext).registry, input(), sysbuf, out_len),
        proto::IOCTL_EXT4_READ_FILE => handle_read_file(&(*ext).registry, input(), sysbuf, out_len),
        proto::IOCTL_EXT4_WRITE_FILE => handle_write_file(&(*ext).registry, input()),
        proto::IOCTL_EXT4_CREATE_FILE => handle_create(&(*ext).registry, input()),
        proto::IOCTL_EXT4_MKDIR => handle_mkdir(&(*ext).registry, input()),
        proto::IOCTL_EXT4_UNLINK => handle_path_req(&(*ext).registry, input(), PathOp::Unlink),
        proto::IOCTL_EXT4_RMDIR => handle_path_req(&(*ext).registry, input(), PathOp::Rmdir),
        proto::IOCTL_EXT4_SET_LEN => handle_set_len(&(*ext).registry, input()),
        proto::IOCTL_EXT4_STAT => handle_stat(&(*ext).registry, input(), sysbuf, out_len),
        proto::IOCTL_EXT4_FLUSH => handle_flush(&(*ext).registry, input()),
        proto::IOCTL_EXT4_SYMLINK => handle_symlink(&(*ext).registry, input()),
        _ => Err(nd::st::INVALID_DEVICE_REQUEST),
    };

    match result {
        Ok(info) => {
            complete(irp, 0, info);
            0
        }
        Err(status) => {
            complete(irp, status, 0);
            status
        }
    }
}

fn complete(irp: PIRP, status: i32, info: usize) {
    unsafe {
        (*irp).IoStatus.__bindgen_anon_1.Status = status;
        (*irp).IoStatus.Information = info as u64;
        wdk_sys::ntddk::IofCompleteRequest(irp, 0);
    }
}

fn parse<T>(bytes: &[u8]) -> Option<&T> {
    if bytes.len() >= core::mem::size_of::<T>() {
        Some(unsafe { &*(bytes.as_ptr() as *const T) })
    } else {
        None
    }
}

fn parse_path(bytes: &[u8], offset: usize, len_bytes: usize) -> Option<&[u8]> {
    let end = offset.checked_add(len_bytes)?;
    if end > bytes.len() {
        return None;
    }
    Some(&bytes[offset..end])
}

use ext4_core::inode::FileType;

unsafe fn handle_open(
    reg: &volume::VolumeRegistry,
    input: &[u8],
    out: *mut u8,
    out_len: usize,
) -> Result<usize, i32> {
    let hdr = parse::<proto::OpenImageReq>(input).ok_or(nd::st::INVALID_PARAMETER)?;
    let path_bytes = parse_path(input, 8, hdr.path_len_bytes as usize)
        .ok_or(nd::st::INVALID_PARAMETER)?;
    if path_bytes.len() % 2 != 0 {
        return Err(nd::st::INVALID_PARAMETER);
    }
    let path_u16: Vec<u16> = path_bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    let id = reg.mount(&path_u16, hdr.read_only != 0)?;
    let reply = proto::VolumeReq { volume_id: id };
    if out_len < core::mem::size_of::<proto::VolumeReq>() || out.is_null() {
        return Err(nd::st::BUFFER_TOO_SMALL);
    }
    core::ptr::copy_nonoverlapping(
        &reply as *const _ as *const u8,
        out,
        core::mem::size_of::<proto::VolumeReq>(),
    );
    Ok(core::mem::size_of::<proto::VolumeReq>())
}

fn handle_volume_info(
    reg: &volume::VolumeRegistry,
    input: &[u8],
    out: *mut u8,
    out_len: usize,
) -> Result<usize, i32> {
    let req = parse::<proto::VolumeReq>(input).ok_or(nd::st::INVALID_PARAMETER)?;
    reg.with_volume(req.volume_id, |v| {
        let sb = &v.fs.sb;
        let label = sb.volume_name();
        let needed = core::mem::size_of::<proto::VolumeInfoReply>() + label.len();
        if out_len < needed || out.is_null() {
            write_needed(out, out_len, needed);
            return Err(nd::st::BUFFER_TOO_SMALL);
        }
        let info = proto::VolumeInfoReply {
            volume_id: v.id,
            block_size: sb.block_size,
            blocks_count: sb.blocks_count,
            free_blocks: sb.free_blocks_count,
            free_inodes: sb.free_inodes_count,
            read_only: v.read_only as u32,
            label_len: label.len() as u32,
            uuid: sb.uuid(),
        };
        unsafe {
            core::ptr::copy_nonoverlapping(
                &info as *const _ as *const u8,
                out,
                core::mem::size_of::<proto::VolumeInfoReply>(),
            );
            core::ptr::copy_nonoverlapping(
                label.as_ptr(),
                out.add(core::mem::size_of::<proto::VolumeInfoReply>()),
                label.len(),
            );
        }
        Ok(needed)
    })
    .map_err(|c| c)
}

fn write_needed(out: *mut u8, out_len: usize, needed: usize) {
    if !out.is_null() && out_len >= core::mem::size_of::<u32>() {
        unsafe {
            *(out as *mut u32) = needed as u32;
        }
    }
}

fn handle_list_dir(
    reg: &volume::VolumeRegistry,
    input: &[u8],
    out: *mut u8,
    out_len: usize,
) -> Result<usize, i32> {
    let req = parse::<proto::PathReq>(input).ok_or(nd::st::INVALID_PARAMETER)?;
    let path = parse_path(input, 8, req.path_len_bytes as usize)
        .ok_or(nd::st::INVALID_PARAMETER)?;
    reg.with_volume(req.volume_id, |v| {
        let dir = v
            .fs
            .resolve(core::str::from_utf8(path).map_err(|_| nd::st::INVALID_PARAMETER)?)
            .map_err(|e| nt_from_ext4(&e))?;
        let entries = v.fs.read_dir(&dir).map_err(|e| nt_from_ext4(&e))?;
        let mut payload: Vec<u8> = Vec::new();
        payload.extend_from_slice(&(entries.len() as u32).to_le_bytes());
        for e in &entries {
            let st = v.fs.stat(e.inode).unwrap_or(ext4_core::fsops::Stat {
                ino: e.inode,
                mode: 0,
                size: 0,
                nlink: 0,
                atime: 0,
                mtime: 0,
                ctime: 0,
                uid: 0,
                gid: 0,
                kind: None,
            });
            let kind = match st.kind {
                Some(FileType::Dir) => 2u8,
                Some(FileType::Symlink) => 7u8,
                Some(FileType::Regular) => 1u8,
                _ => 0u8,
            };
            let name = &e.name;
            payload.extend_from_slice(&st.ino.to_le_bytes());
            payload.extend_from_slice(&st.size.to_le_bytes());
            payload.extend_from_slice(&st.mode.to_le_bytes());
            payload.push(kind);
            payload.extend_from_slice(&(name.len() as u16).to_le_bytes());
            payload.extend_from_slice(name);
            while payload.len() % 4 != 0 {
                payload.push(0);
            }
        }
        let needed = payload.len();
        if out_len < needed || out.is_null() {
            write_needed(out, out_len, needed);
            return Err(nd::st::BUFFER_TOO_SMALL);
        }
        unsafe {
            core::ptr::copy_nonoverlapping(payload.as_ptr(), out, needed);
        }
        Ok(needed)
    })
    .map_err(|c| c)
}

fn handle_read_file(
    reg: &volume::VolumeRegistry,
    input: &[u8],
    out: *mut u8,
    out_len: usize,
) -> Result<usize, i32> {
    let req = parse::<proto::ReadFileReq>(input).ok_or(nd::st::INVALID_PARAMETER)?;
    let path = parse_path(input, core::mem::size_of::<proto::ReadFileReq>(), req.path_len_bytes as usize)
        .ok_or(nd::st::INVALID_PARAMETER)?;
    reg.with_volume(req.volume_id, |v| {
        let ino = v
            .fs
            .resolve(core::str::from_utf8(path).map_err(|_| nd::st::INVALID_PARAMETER)?)
            .map_err(|e| nt_from_ext4(&e))?;
        if ino.is_encrypted() {
            return Err(nd::st::DATA_ENCRYPTION_UNSUPPORTED);
        }
        let want = (req.length as usize).min(out_len);
        let mut buf = vec![0u8; want];
        let n = v
            .fs
            .read_file(ino.no, req.offset, &mut buf)
            .map_err(|e| nt_from_ext4(&e))?;
        if out.is_null() && n > 0 {
            return Err(nd::st::BUFFER_TOO_SMALL);
        }
        unsafe {
            core::ptr::copy_nonoverlapping(buf.as_ptr(), out, n);
        }
        Ok(n)
    })
    .map_err(|c| c)
}

fn handle_write_file(reg: &volume::VolumeRegistry, input: &[u8]) -> Result<usize, i32> {
    let req = parse::<proto::WriteFileReq>(input).ok_or(nd::st::INVALID_PARAMETER)?;
    let base = core::mem::size_of::<proto::WriteFileReq>();
    let path = parse_path(input, base, req.path_len_bytes as usize)
        .ok_or(nd::st::INVALID_PARAMETER)?;
    let data = parse_path(input, base + req.path_len_bytes as usize, req.data_len as usize)
        .ok_or(nd::st::INVALID_PARAMETER)?;
    reg.with_volume(req.volume_id, |v| {
        if v.read_only {
            return Err(nd::st::MEDIA_WRITE_PROTECTED);
        }
        let path_str = core::str::from_utf8(path).map_err(|_| nd::st::INVALID_PARAMETER)?;
        let ino = match v.fs.resolve(path_str) {
            Ok(i) if i.file_type() == Some(FileType::Regular) => i.no,
            Ok(_) => return Err(nd::st::FILE_IS_A_DIRECTORY),
            Err(ext4_core::Error::NotFound) => {
                let (dir, name) = split_last(path_str)?;
                v.fs.create_file(dir, &name).map_err(|e| nt_from_ext4(&e))?
            }
            Err(e) => return Err(nt_from_ext4(&e)),
        };
        let n = v
            .fs
            .write_file(ino, req.offset, data)
            .map_err(|e| nt_from_ext4(&e))?;
        Ok(n)
    })
    .map_err(|c| c)
}

fn handle_create(reg: &volume::VolumeRegistry, input: &[u8]) -> Result<usize, i32> {
    path_op_write(reg, input, |fs, path| {
        let (dir, name) = split_last(path)?;
        fs.create_file(dir, &name).map(|ino| ino as usize).map_err(|e| nt_from_ext4(&e))
    })
}

fn handle_mkdir(reg: &volume::VolumeRegistry, input: &[u8]) -> Result<usize, i32> {
    path_op_write(reg, input, |fs, path| {
        let (dir, name) = split_last(path)?;
        fs.mkdir(dir, &name).map(|ino| ino as usize).map_err(|e| nt_from_ext4(&e))
    })
}

fn handle_symlink(reg: &volume::VolumeRegistry, input: &[u8]) -> Result<usize, i32> {
    let req = parse::<proto::SymlinkReq>(input).ok_or(nd::st::INVALID_PARAMETER)?;
    let base = core::mem::size_of::<proto::SymlinkReq>();
    let dlen = req.dir_len_bytes as usize;
    let nlen = req.name_len_bytes as usize;
    let tlen = req.target_len_bytes as usize;
    let dir = parse_path(input, base, dlen).ok_or(nd::st::INVALID_PARAMETER)?;
    let name = parse_path(input, base + dlen, nlen).ok_or(nd::st::INVALID_PARAMETER)?;
    let target = parse_path(input, base + dlen + nlen, tlen).ok_or(nd::st::INVALID_PARAMETER)?;
    reg.with_volume(req.volume_id, |v| {
        if v.read_only {
            return Err(nd::st::MEDIA_WRITE_PROTECTED);
        }
        let dir = core::str::from_utf8(dir).map_err(|_| nd::st::INVALID_PARAMETER)?;
        let name = core::str::from_utf8(name).map_err(|_| nd::st::INVALID_PARAMETER)?;
        let target = core::str::from_utf8(target).map_err(|_| nd::st::INVALID_PARAMETER)?;
        v.fs.symlink(dir, name, target).map(|ino| ino as usize).map_err(|e| nt_from_ext4(&e))
    })
    .map_err(|c| c)
}

enum PathOp {
    Unlink,
    Rmdir,
}

fn handle_path_req(reg: &volume::VolumeRegistry, input: &[u8], op: PathOp) -> Result<usize, i32> {
    path_op_write(reg, input, |fs, path| {
        match op {
            PathOp::Unlink => fs.unlink(path).map_err(|e| nt_from_ext4(&e))?,
            PathOp::Rmdir => fs.rmdir(path).map_err(|e| nt_from_ext4(&e))?,
        }
        Ok(0)
    })
}

fn handle_set_len(reg: &volume::VolumeRegistry, input: &[u8]) -> Result<usize, i32> {
    let req = parse::<proto::SetLenReq>(input).ok_or(nd::st::INVALID_PARAMETER)?;
    let path = parse_path(input, core::mem::size_of::<proto::SetLenReq>(), req.path_len_bytes as usize)
        .ok_or(nd::st::INVALID_PARAMETER)?;
    reg.with_volume(req.volume_id, |v| {
        if v.read_only {
            return Err(nd::st::MEDIA_WRITE_PROTECTED);
        }
        let ino = v
            .fs
            .resolve(core::str::from_utf8(path).map_err(|_| nd::st::INVALID_PARAMETER)?)
            .map_err(|e| nt_from_ext4(&e))?;
        v.fs
            .set_len(ino.no, req.new_size)
            .map_err(|e| nt_from_ext4(&e))?;
        Ok(0)
    })
    .map_err(|c| c)
}

fn handle_stat(
    reg: &volume::VolumeRegistry,
    input: &[u8],
    out: *mut u8,
    out_len: usize,
) -> Result<usize, i32> {
    let req = parse::<proto::PathReq>(input).ok_or(nd::st::INVALID_PARAMETER)?;
    let path = parse_path(input, 8, req.path_len_bytes as usize)
        .ok_or(nd::st::INVALID_PARAMETER)?;
    reg.with_volume(req.volume_id, |v| {
        let ino = v
            .fs
            .resolve(core::str::from_utf8(path).map_err(|_| nd::st::INVALID_PARAMETER)?)
            .map_err(|e| nt_from_ext4(&e))?;
        let st = v.fs.stat(ino.no).map_err(|e| nt_from_ext4(&e))?;
        let needed = core::mem::size_of::<proto::StatReply>();
        if out_len < needed || out.is_null() {
            write_needed(out, out_len, needed);
            return Err(nd::st::BUFFER_TOO_SMALL);
        }
        let reply = proto::StatReply {
            ino: st.ino,
            mode: st.mode,
            nlink: st.nlink,
            size: st.size,
            atime: st.atime,
            mtime: st.mtime,
            ctime: st.ctime,
            kind: match st.kind {
                Some(FileType::Dir) => 2u8,
                Some(FileType::Symlink) => 7u8,
                Some(FileType::Regular) => 1u8,
                _ => 0u8,
            },
            _pad: [0u8; 3],
        };
        unsafe {
            core::ptr::copy_nonoverlapping(&reply as *const _ as *const u8, out, needed);
        }
        Ok(needed)
    })
    .map_err(|c| c)
}

fn handle_flush(reg: &volume::VolumeRegistry, input: &[u8]) -> Result<usize, i32> {
    let req = parse::<proto::VolumeReq>(input).ok_or(nd::st::INVALID_PARAMETER)?;
    reg.with_volume(req.volume_id, |v| {
        v.fs.flush().map_err(|e| nt_from_ext4(&e))?;
        Ok(0)
    })
    .map_err(|c| c)
}

fn path_op_write(
    reg: &volume::VolumeRegistry,
    input: &[u8],
    f: impl FnOnce(&mut ext4_core::fs::Filesystem, &str) -> Result<usize, i32>,
) -> Result<usize, i32> {
    let req = parse::<proto::PathReq>(input).ok_or(nd::st::INVALID_PARAMETER)?;
    let path = parse_path(input, 8, req.path_len_bytes as usize)
        .ok_or(nd::st::INVALID_PARAMETER)?;
    reg.with_volume(req.volume_id, |v| {
        if v.read_only {
            return Err(nd::st::MEDIA_WRITE_PROTECTED);
        }
        let path = core::str::from_utf8(path).map_err(|_| nd::st::INVALID_PARAMETER)?;
        f(&mut v.fs, path)
    })
    .map_err(|c| c)
}

/// Sépare chemin parent / composant final.
fn split_last(path: &str) -> Result<(&str, alloc::string::String), i32> {
    match path.rfind('/') {
        Some(0) if path.len() > 1 => Ok(("/", path[1..].to_string())),
        Some(i) if i > 0 => Ok((&path[..i], path[i + 1..].to_string())),
        _ => Err(nd::st::INVALID_PARAMETER),
    }
}

pub fn nt_from_ext4(e: &ext4_core::Error) -> i32 {
    fsd_types::nt_from_ext4(e)
}
