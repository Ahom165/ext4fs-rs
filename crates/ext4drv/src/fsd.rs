//! Couche FSD : dispatch IRP global, montage IRP_MN_MOUNT_VOLUME (lettre de
//! lecteur via le mount manager), cycle de vie VCB/FCB.

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::ffi::c_void;
use core::ptr::{addr_of, addr_of_mut, null_mut};

use ext4_core::fs::Filesystem;

use crate::fsd_types::*;
use crate::ntdefs as nd;
use crate::ntshim::*;
use crate::uni;
use crate::zpdev::{DevBlockDevice, KernelTime};
use wdk_sys::{
    ntddk::{ExAcquireFastMutex, ExInitializeFastMutex, ExReleaseFastMutex, IoCreateDevice},
    DEVICE_OBJECT, FAST_MUTEX, IO_STATUS_BLOCK, LARGE_INTEGER, OBJECT_ATTRIBUTES,
    PDRIVER_OBJECT, PIRP, UNICODE_STRING,
};

// Magic DeviceExtension du FS device
pub const FSDE_MAGIC: u32 = 0x4653_4445; // "FSDE"

/// Extension du device système de fichiers : liste des VCB actifs.
pub struct FsDevExt {
    pub magic: u32,
    pub mutex: FAST_MUTEX,
    pub volumes: Vec<*mut Vcb>,
}

impl FsDevExt {
    pub fn new() -> FsDevExt {
        let mut mutex = FAST_MUTEX::default();
        unsafe {
            ExInitializeFastMutex(&mut mutex);
        }
        FsDevExt {
            magic: FSDE_MAGIC,
            mutex,
            volumes: Vec::new(),
        }
    }
}

// Device global
pub static mut FS_DEVICE: *mut DEVICE_OBJECT = null_mut();

#[inline]
pub fn fs_device() -> *mut DEVICE_OBJECT {
    unsafe { core::ptr::read(core::ptr::addr_of!(FS_DEVICE)) }
}

#[inline]
pub fn set_fs_device(d: *mut DEVICE_OBJECT) {
    unsafe { core::ptr::write(core::ptr::addr_of_mut!(FS_DEVICE), d) }
}

/// Enregistre le device FS après DriverEntry (toute la table dispatch prête).
pub fn register_filesystem() -> i32 {
    unsafe {
        let dev = fs_device();
        if dev.is_null() {
            return nd::st::UNSUCCESSFUL;
        }
        IoRegisterFileSystem(dev as *mut c_void);
        IoRegisterShutdownNotification(dev as *mut c_void);
        0
    }
}

pub fn unregister_filesystem() {
    unsafe {
        let dev = fs_device();
        if !dev.is_null() {
            IoUnregisterShutdownNotification(dev as *mut c_void);
            IoUnregisterFileSystem(dev as *mut c_void);
        }
    }
}

// ---------------------------------------------------------------------------
// Dispatch principal
// ---------------------------------------------------------------------------

/// Route un IRP selon le device : FS device, device volume, device contrôle.
pub unsafe extern "system" fn fsd_dispatch(device: *mut DEVICE_OBJECT, irp: PIRP) -> i32 {
    if device.is_null() {
        complete_irp(irp, nd::st::INVALID_DEVICE_REQUEST, 0);
        return nd::st::INVALID_DEVICE_REQUEST;
    }
    let sp = (*irp).Tail.Overlay.CurrentStackLocation;
    let major: u8 = sp_major(sp);

    // device de contrôle (phase 1 : images) ?
    if device as *mut c_void == crate::control_device() as *mut c_void {
        return crate::dispatch_control(device, irp);
    }

    // device FS ?
    if device == fs_device() {
        match major {
            nd::MJ_FILE_SYSTEM_CONTROL => return fsctrl_fs_device(device, irp),
            nd::MJ_SHUTDOWN => return shutdown_all(device, irp),
            nd::MJ_CREATE | nd::MJ_CLOSE | nd::MJ_CLEANUP => {
                // open/close du device FS lui-même
                complete_irp(irp, 0, 0);
                return 0;
            }
            nd::MJ_DEVICE_CONTROL => {
                return crate::dispatch_control(device, irp);
            }
            _ => {
                complete_irp(irp, nd::st::INVALID_DEVICE_REQUEST, 0);
                return nd::st::INVALID_DEVICE_REQUEST;
            }
        }
    }

    // device volume : DeviceExtension = *mut Vcb
    let vcb = (*device).DeviceExtension as *mut Vcb;
    if vcb.is_null() {
        complete_irp(irp, nd::st::INVALID_DEVICE_REQUEST, 0);
        return nd::st::INVALID_DEVICE_REQUEST;
    }

    match major {
        nd::MJ_CREATE => crate::fsd_create::dispatch_create(vcb, irp),
        nd::MJ_CLOSE => crate::fsd_fsctl::dispatch_close(vcb, irp),
        nd::MJ_CLEANUP => crate::fsd_fsctl::dispatch_cleanup(vcb, irp),
        nd::MJ_READ => crate::fsd_io::dispatch_read(vcb, irp),
        nd::MJ_WRITE => crate::fsd_io::dispatch_write(vcb, irp),
        nd::MJ_QUERY_INFORMATION => crate::fsd_info::dispatch_query_info(vcb, irp),
        nd::MJ_SET_INFORMATION => crate::fsd_info::dispatch_set_info(vcb, irp),
        nd::MJ_DIRECTORY_CONTROL => crate::fsd_io::dispatch_dirctl(vcb, irp),
        nd::MJ_FILE_SYSTEM_CONTROL => crate::fsd_fsctl::dispatch_fsctrl_volume(vcb, irp),
        nd::MJ_DEVICE_CONTROL => crate::fsd_fsctl::dispatch_volume_devctrl(vcb, irp),
        nd::MJ_FLUSH_BUFFERS => crate::fsd_fsctl::dispatch_flush(vcb, irp),
        nd::MJ_QUERY_VOLUME_INFORMATION => crate::fsd_info::dispatch_query_volume(vcb, irp),
        nd::MJ_SET_VOLUME_INFORMATION => crate::fsd_info::dispatch_set_volume(vcb, irp),
        nd::MJ_LOCK_CONTROL => crate::fsd_fsctl::dispatch_lock(vcb, irp),
        nd::MJ_QUERY_EA => crate::fsd_xattr::dispatch_query_ea(vcb, irp),
        nd::MJ_SET_EA => crate::fsd_xattr::dispatch_set_ea(vcb, irp),
        nd::MJ_QUERY_SECURITY => crate::fsd_xattr::dispatch_query_security(vcb, irp),
        nd::MJ_SET_SECURITY => crate::fsd_xattr::dispatch_set_security(vcb, irp),
        nd::MJ_PNP => dispatch_pnp(vcb, irp),
        _ => {
            complete_irp(irp, nd::st::INVALID_DEVICE_REQUEST, 0);
            nd::st::INVALID_DEVICE_REQUEST
        }
    }
}

unsafe extern "system" fn dispatch_pnp(vcb: *mut Vcb, irp: PIRP) -> i32 {
    let sp = (*irp).Tail.Overlay.CurrentStackLocation;
    let minor: u8 = sp_minor(sp);
    if minor == nd::MN_SURPRISE_REMOVAL {
        let _ = dismount(vcb, true);
    }
    complete_irp(irp, 0, 0);
    0
}

/// Complète un IRP.
pub fn complete_irp(irp: PIRP, status: i32, info: usize) {
    unsafe {
        (*irp).IoStatus.Status = status;
        (*irp).IoStatus.Information = info;
        wdk_sys::ntddk::IofCompleteRequest(irp, 0);
    }
}

// ---------------------------------------------------------------------------
// IRP_MJ_FILE_SYSTEM_CONTROL sur le device FS : montage
// ---------------------------------------------------------------------------

unsafe extern "system" fn fsctrl_fs_device(_device: *mut DEVICE_OBJECT, irp: PIRP) -> i32 {
    let sp = (*irp).Tail.Overlay.CurrentStackLocation;
    let minor: u8 = sp_minor(sp);
    match minor {
        nd::MN_MOUNT_VOLUME => mount_volume(irp),
        nd::MN_VERIFY_VOLUME => {
            // v1 : volume toujours considéré valide si le VCB existe
            let (vpb, _dev) = p_mount(sp);
            if !vpb.is_null() {
                let vcb = core::ptr::read(core::ptr::addr_of!((*vpb).DeviceObject));
                if !vcb.is_null() {
                    let v = vcb as *mut Vcb;
                    if (*v).flags & VCB_FLAG_DISMOUNTED == 0 {
                        complete_irp(irp, 0, 0);
                        return 0;
                    }
                }
            }
            complete_irp(irp, nd::st::UNSUCCESSFUL, 0);
            nd::st::UNSUCCESSFUL
        }
        _ => {
            complete_irp(irp, nd::st::INVALID_DEVICE_REQUEST, 0);
            nd::st::INVALID_DEVICE_REQUEST
        }
    }
}

/// IRP_MN_MOUNT_VOLUME : ouvre le volume réel, lit le superblock ext4, crée
/// le device volume, renseigne la VPB puis notifie le mount manager.
unsafe extern "system" fn mount_volume(irp: PIRP) -> i32 {
    let sp = (*irp).Tail.Overlay.CurrentStackLocation;
    let (vpb, real_dev) = p_mount(sp);
    if vpb.is_null() || real_dev.is_null() {
        complete_irp(irp, nd::st::INVALID_PARAMETER, 0);
        return nd::st::INVALID_PARAMETER;
    }

    // Nom NT du device réel (\Device\HarddiskVolumeN)
    let name = query_device_name(real_dev);
    if name.is_none() {
        complete_irp(irp, nd::st::UNSUCCESSFUL, 0);
        return nd::st::UNSUCCESSFUL;
    }
    let nt_name = name.unwrap();

    // Ouverture du volume via un stream file object (PAS de chemin NT :
    // récursion de montage impossible), puis montage ext4 (replay journal).
    // Les volumes Windows sont accessibles RW via la pile disque ; si le
    // volume est verrouillé en lecture seule, les écritures échoueront
    // proprement aux IRP_MJ_WRITE.
    let read_only = false;
    let fs = match DevBlockDevice::open(real_dev) {
        Ok(dev) => match Filesystem::mount(Box::new(dev), Box::new(KernelTime), false) {
            Ok(fs) => fs,
            Err(e) => {
                wdk::println!("ext4fsd: pas un volume ext4 ({e})");
                complete_irp(irp, nd::st::UNRECOGNIZED_VOLUME, 0);
                return nd::st::UNRECOGNIZED_VOLUME;
            }
        },
        Err(e) => {
            wdk::println!("ext4fsd: ouverture du volume échouée: {e:?}");
            complete_irp(irp, nd::st::UNRECOGNIZED_VOLUME, 0);
            return nd::st::UNRECOGNIZED_VOLUME;
        }
    };

    // Device volume
    let driver = (*fs_device()).DriverObject;
    let mut vol_dev: *mut DEVICE_OBJECT = null_mut();
    let status = IoCreateDevice(
        driver as PDRIVER_OBJECT,
        core::mem::size_of::<Vcb>(),
        core::ptr::null_mut(), // sans nom
        nd::FILE_DEVICE_DISK_FILE_SYSTEM,
        0,
        0, // non exclusif
        &mut vol_dev,
    );
    if status != 0 {
        wdk::println!("ext4fsd: IoCreateDevice volume échoué {status:#x}");
        drop(fs);
        complete_irp(irp, status, 0);
        return status;
    }
    (*vol_dev).Flags |= nd::DO_DIRECT_IO;

    // VCB
    let mut mutex = FAST_MUTEX::default();
    ExInitializeFastMutex(&mut mutex);
    let label_raw = fs.sb.volume_name();
    let mut label = uni::utf8_to_utf16(label_raw.as_bytes());
    if label.is_empty() {
        label = b"EXT4".iter().map(|b| *b as u16).collect();
    }
    if label.len() > 31 {
        label.truncate(31);
    }
    let uuid = fs.sb.uuid();
    let serial = u32::from_le_bytes([uuid[0] ^ uuid[4], uuid[1] ^ uuid[5], uuid[2] ^ uuid[6], uuid[3] ^ uuid[7]]);

    let vcb_box = Box::new(Vcb {
        device: vol_dev as *mut c_void,
        target_dev: real_dev as *mut c_void,
        vpb: vpb as *mut c_void,
        fs,
        mutex,
        fcbs: alloc::collections::BTreeMap::new(),
        streams: alloc::collections::BTreeMap::new(),
        root: ext4_core::INO_ROOT,
        serial,
        label,
        nt_device_name: nt_name.clone(),
        dos_letter: None,
        notify_sync: null_mut(),
        flags: if read_only { VCB_FLAG_READ_ONLY } else { 0 },
        open_file_count: 0,
        read_only,
    });
    let vcb = Box::into_raw(vcb_box);
    (*vol_dev).DeviceExtension = vcb as *mut c_void;

    // root FCB
    let root_fcb = alloc_fcb(vcb, ext4_core::INO_ROOT, FCB_TYPE_ROOT);
    if root_fcb.is_err() {
        wdk::println!("ext4fsd: FCB racine échoué");
        Box::from_raw(vcb);
        wdk_sys::ntddk::IoDeleteDevice(vol_dev);
        complete_irp(irp, nd::st::UNSUCCESSFUL, 0);
        return nd::st::UNSUCCESSFUL;
    }

    // notify sync FsRtl
    FsRtlNotifyInitializeSync(addr_of_mut!((*vcb).notify_sync));

    // VPB : DeviceObject + label + numéro de série
    core::ptr::write(addr_of_mut!((*vpb).DeviceObject), vol_dev);
    core::ptr::write(addr_of_mut!((*vpb).SerialNumber), serial);
    core::ptr::write(addr_of_mut!((*vpb).Flags), core::ptr::read(addr_of!((*vpb).Flags)) | nd::VPB_MOUNTED);
    let vpbl = core::ptr::addr_of_mut!((*vpb).VolumeLabel) as *mut u16;
    let lbl = &(*vcb).label;
    let mut i = 0usize;
    while i < lbl.len() && i < 31 {
        core::ptr::write(vpbl.add(i), lbl[i]);
        i += 1;
    }
    core::ptr::write(addr_of_mut!((*vpb).VolumeLabelLength), (lbl.len() * 2) as u16);

    (*vol_dev).Flags &= !nd::DO_DEVICE_INITIALIZING;

    // enregistrement dans la liste FS device
    let ext = (*fs_device()).DeviceExtension as *mut FsDevExt;
    ExAcquireFastMutex(&mut (*ext).mutex);
    (*ext).volumes.push(vcb);
    ExReleaseFastMutex(&mut (*ext).mutex);

    // mount manager : attribue la lettre de lecteur
    let letter = notify_mount_manager(&nt_name);
    if let Some(l) = &letter {
        (*vcb).dos_letter = Some(l.clone());
    }
    wdk::println!(
        "ext4fsd: volume monté {} -> {} (label=\"{}\")",
        uni::wide_to_string(&nt_name),
        match &(*vcb).dos_letter { Some(l) => uni::wide_to_string(l), None => "(sans lettre)".into() },
        uni::wide_to_string(&(*vcb).label)
    );

    complete_irp(irp, 0, 0);
    0
}

/// ObQueryNameString → chemin NT du device (\Device\HarddiskVolumeN).
unsafe fn query_device_name(dev: *mut DEVICE_OBJECT) -> Option<Vec<u16>> {
    let mut buf = [0u8; 1024];
    let mut ret: u32 = 0;
    let status = ObQueryNameString(
        dev as *mut c_void,
        buf.as_mut_ptr() as *mut OBJECT_NAME_INFORMATION,
        buf.len() as u32,
        &mut ret,
    );
    if status != 0 {
        return None;
    }
    let info = buf.as_ptr() as *mut OBJECT_NAME_INFORMATION;
    let us = core::ptr::read(core::ptr::addr_of!((*info).Name));
    if us.Length == 0 || us.Buffer.is_null() {
        return None;
    }
    let n = (us.Length as usize) / 2;
    let mut v = Vec::with_capacity(n);
    for i in 0..n {
        v.push(core::ptr::read(us.Buffer.add(i)));
    }
    Some(v)
}

/// Notifie le mount manager et récupère la lettre de lecteur attribuée.
unsafe fn notify_mount_manager(nt_name: &[u16]) -> Option<Vec<u16>> {
    // ouvre \Device\MountPointManager
    let mut name_buf = nd::MOUNTMGR_DEVICE_NAME.to_vec();
    let name_len = (name_buf.len() * 2) as u16;
    let mut us = UNICODE_STRING {
        Length: name_len,
        MaximumLength: name_len,
        Buffer: name_buf.as_mut_ptr(),
    };
    let mut oa = OBJECT_ATTRIBUTES {
        Length: core::mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: null_mut(),
        ObjectName: &mut us,
        Attributes: wdk_sys::OBJ_KERNEL_HANDLE | wdk_sys::OBJ_CASE_INSENSITIVE,
        SecurityDescriptor: null_mut(),
        SecurityQualityOfService: null_mut(),
    };
    let mut handle: *mut c_void = null_mut();
    let mut iosb = IO_STATUS_BLOCK::default();
    let status = wdk_sys::ntddk::ZwCreateFile(
        &mut handle,
        nd::FILE_READ_DATA | nd::SYNCHRONIZE,
        &mut oa,
        &mut iosb,
        null_mut(),
        0,
        0x1 | 0x2 | 0x4,
        nd::FILE_OPEN,
        nd::FILE_SYNCHRONOUS_IO_NONALERT,
        null_mut(),
        0,
    );
    if status != 0 {
        wdk::println!("ext4fsd: mount manager inaccessible {status:#x}");
        return None;
    }

    // MOUNTMGR_TARGET_NAME
    let mut target: Vec<u8> = Vec::new();
    target.extend_from_slice(&((nt_name.len() * 2) as u16).to_le_bytes());
    for w in nt_name {
        target.extend_from_slice(&w.to_le_bytes());
    }

    // 1) notification d'arrivée
    let mut iosb2 = IO_STATUS_BLOCK::default();
    let st = ZwDeviceIoControlFile(
        handle,
        null_mut(),
        null_mut(),
        null_mut(),
        &mut iosb2 as *mut IO_STATUS_BLOCK as *mut c_void,
        nd::IOCTL_MOUNTMGR_VOLUME_ARRIVAL_NOTIFICATION,
        target.as_ptr() as *mut c_void,
        target.len() as u32,
        null_mut(),
        0,
    );
    if st != 0 {
        wdk::println!("ext4fsd: volume arrival notification échouée {st:#x}");
    }

    // 2) lettre attribuée ? (léger délai : le mount manager s'exécute en
    //    système, la notification est synchrone dans la pratique)
    let mut letter: Option<Vec<u16>> = None;
    for _ in 0..3 {
        let mut out = [0u8; 512];
        let mut iosb3 = IO_STATUS_BLOCK::default();
        let st = ZwDeviceIoControlFile(
            handle,
            null_mut(),
            null_mut(),
            null_mut(),
            &mut iosb3 as *mut IO_STATUS_BLOCK as *mut c_void,
            nd::IOCTL_MOUNTMGR_QUERY_DOS_VOLUME_PATH,
            target.as_ptr() as *mut c_void,
            target.len() as u32,
            out.as_mut_ptr() as *mut c_void,
            out.len() as u32,
        );
        if st == 0 {
            // MOUNTMGR_VOLUME_PATHS { MultiSzLength u32, MultiSz[] }
            let mlen = u32::from_le_bytes([out[0], out[1], out[2], out[3]]) as usize;
            if mlen >= 4 {
                let mut w = Vec::new();
                for i in 0..(mlen / 2) {
                    let o = 4 + i * 2;
                    if o + 1 >= out.len() {
                        break;
                    }
                    w.push(u16::from_le_bytes([out[o], out[o + 1]]));
                }
                // première chaîne du multi-sz : "X:"
                if let Some(end) = w.iter().position(|c| *c == 0) {
                    w.truncate(end);
                }
                if w.len() >= 2 && w[1] == b':' as u16 {
                    let mut full = vec![0x5C, 0x3F, 0x3F, 0x5C]; // \??\
                    full.extend_from_slice(&w);
                    letter = Some(full);
                }
            }
            break;
        }
        // 200 ms puis nouvelle tentative
        let mut interval = LARGE_INTEGER::default();
        interval.QuadPart = -2_000_000; // 200 ms (100 ns units)
        KeDelayExecutionThread(KernelMode, 0, &mut interval);
    }
    wdk_sys::ntddk::ZwClose(handle);
    letter
}

// ---------------------------------------------------------------------------
// Démontage
// ---------------------------------------------------------------------------

/// Démonte un volume. Échoue si des fichiers sont ouverts (sauf forcé).
pub unsafe fn dismount(vcb: *mut Vcb, force: bool) -> Result<(), i32> {
    if (*vcb).open_file_count != 0 && !force {
        return Err(nd::st::DEVICE_BUSY);
    }
    if (*vcb).flags & VCB_FLAG_DISMOUNTED != 0 {
        return Ok(());
    }
    (*vcb).flags |= VCB_FLAG_DISMOUNTED;

    // retire de la liste du FS device
    let ext = (*fs_device()).DeviceExtension as *mut FsDevExt;
    ExAcquireFastMutex(&mut (*ext).mutex);
    (*ext).volumes.retain(|&v| v != vcb);
    ExReleaseFastMutex(&mut (*ext).mutex);

    // nettoyage FsRtl
    if !(*vcb).notify_sync.is_null() {
        FsRtlNotifyUninitializeSync(addr_of_mut!((*vcb).notify_sync));
    }

    // purge des FCB de flux + oplocks (le VCB est en voie de disparition)
    let stream_keys: Vec<(u32, Vec<u8>)> = (*vcb).streams.keys().copied().collect();
    for k in stream_keys {
        if let Some(mut f) = (*vcb).streams.remove(&k) {
            crate::fsd_xattr::uninitialize_oplock(core::ptr::from_mut(f.as_mut()));
            if !f.lock.is_null() {
                FsRtlUninitializeFileLock(f.lock);
            }
        }
    }
    for f in (*vcb).fcbs.values_mut() {
        crate::fsd_xattr::uninitialize_oplock(core::ptr::from_mut(f.as_mut()));
    }

    // démontage ext4-core (flush + état clean)
    let _ = (*vcb).fs.unmount();

    // retire la VPB
    let vpb = (*vcb).vpb as *mut wdk_sys::VPB;
    if !vpb.is_null() {
        core::ptr::write(addr_of_mut!((*vpb).Flags), core::ptr::read(addr_of!((*vpb).Flags)) & !nd::VPB_MOUNTED);
        core::ptr::write(addr_of_mut!((*vpb).DeviceObject), null_mut());
    }

    let dev = (*vcb).device as *mut DEVICE_OBJECT;
    (*dev).DeviceExtension = null_mut();
    drop(Box::from_raw(vcb)); // flush/drop ext4-core (ferme le handle volume)
    wdk_sys::ntddk::IoDeleteDevice(dev);
    Ok(())
}

/// Tous les volumes (shutdown/unload) : flush propre.
pub unsafe fn shutdown_all(_device: *mut DEVICE_OBJECT, irp: PIRP) -> i32 {
    let ext = (*fs_device()).DeviceExtension as *mut FsDevExt;
    ExAcquireFastMutex(&mut (*ext).mutex);
    let vols: Vec<*mut Vcb> = (*ext).volumes.clone();
    ExReleaseFastMutex(&mut (*ext).mutex);
    for v in vols {
        // flush Cc de tous les fichiers puis journal
        let _ = flush_volume(v);
        let _ = (*v).fs.flush();
    }
    complete_irp(irp, 0, 0);
    0
}

/// Flush Cc de tous les FCB ouverts + commit journal.
pub unsafe fn flush_volume(vcb: *mut Vcb) -> Result<(), i32> {
    let inos: Vec<u32> = (*vcb).fcbs.keys().copied().collect();
    for ino in inos {
        if let Some(fcb) = (*vcb).fcbs.get(&ino) {
            if fcb.sps.DataSectionObject.is_null() {
                continue;
            }
            let mut iosb = IO_STATUS_BLOCK::default();
            CcFlushCache(
                &fcb.sps as *const _ as *mut c_void,
                null_mut(),
                0,
                &mut iosb as *mut IO_STATUS_BLOCK as *mut c_void,
            );
        }
    }
    (*vcb).fs.flush().map_err(|e| nt_from_ext4(&e))
}

// ---------------------------------------------------------------------------
// Création FCB + refresh des métadonnées depuis ext4-core
// ---------------------------------------------------------------------------

/// Crée (ou réutilise) le FCB d'un inode.
pub unsafe fn alloc_fcb(vcb: *mut Vcb, ino: u32, ftype: u8) -> Result<*mut Fcb, i32> {
    if let Some(existing) = (*vcb).fcbs.get_mut(&ino) {
        return Ok(core::ptr::from_mut(existing.as_mut()));
    }
    let mut fcb = Box::new(Fcb {
        ino,
        ftype,
        size: 0,
        alloc_size: 0,
        attributes: 0,
        links: 1,
        creation: 0,
        access: 0,
        write: 0,
        change: 0,
        reparse_tag: 0,
        lazy_mutex: FAST_MUTEX::default(),
        sps: E4SectionObjectPointers {
            DataSectionObject: null_mut(),
            SharedCacheMap: null_mut(),
            ImageSectionObject: null_mut(),
        },
        notify_list: [0; 2],
        vcb: vcb as *mut c_void,
        target: None,
        lock: FsRtlAllocateFileLock(null_mut(), null_mut()),
        oplock: null_mut(),
        stream: None,
        open_count: 0,
    });
    ExInitializeFastMutex(&mut fcb.lazy_mutex);
    refresh_fcb(vcb, &mut fcb)?;
    init_list_head(&mut fcb.notify_list as *mut [u64; 2]);
    let ptr = core::ptr::from_mut(fcb.as_mut());
    (*vcb).fcbs.insert(ino, fcb);
    Ok(ptr)
}

/// Recharge les métadonnées d'un FCB depuis l'inode ext4.
pub unsafe fn refresh_fcb(vcb: *mut Vcb, fcb: &mut Fcb) -> Result<(), i32> {
    let ino = fcb.ino;
    let st = (*vcb).fs.stat(ino).map_err(|e| nt_from_ext4(&e))?;
    fcb.size = st.size;
    let bs = (*vcb).fs.block_size();
    let blocks = (fcb.size + bs - 1) / bs * bs;
    fcb.alloc_size = blocks;
    fcb.links = st.nlink as u32;
    fcb.creation = unix_to_nt(st.ctime); // ext4 ctime ≈ création (windows ns)
    fcb.access = unix_to_nt(st.atime);
    fcb.write = unix_to_nt(st.mtime);
    fcb.change = unix_to_nt(st.ctime);
    fcb.attributes = ext4_attrs_to_win(st.mode, fcb.ftype);
    if fcb.ftype == FCB_TYPE_SYMLINK {
        if let Ok(t) = (*vcb).fs.readlink(ino) {
            fcb.target = Some(t);
        }
        fcb.reparse_tag = nd::IO_REPARSE_TAG_SYMLINK;
        fcb.attributes |= nd::FILE_ATTRIBUTE_REPARSE_POINT;
    }
    Ok(())
}

pub fn ext4_attrs_to_win(mode: u16, ftype: u8) -> u32 {
    match ftype {
        FCB_TYPE_DIR | FCB_TYPE_ROOT => nd::FILE_ATTRIBUTE_DIRECTORY,
        FCB_TYPE_SYMLINK => nd::FILE_ATTRIBUTE_REPARSE_POINT | nd::FILE_ATTRIBUTE_READONLY,
        _ => {
            // fichier : exécutable → lecture seule côté Windows si 0o111 ? non :
            // on laisse tout accessible (les permissions Unix ne sont pas
            // modélisées v1), archive pour inciter aux copies incrémentales
            let exec = mode & 0o111 != 0;
            let mut a = nd::FILE_ATTRIBUTE_ARCHIVE;
            if exec {
                a |= nd::FILE_ATTRIBUTE_READONLY;
            }
            a
        }
    }
}

/// Obtient le FCB d'un inode (création si absent).
pub unsafe fn get_fcb(vcb: *mut Vcb, ino: u32, ftype: u8) -> Result<*mut Fcb, i32> {
    alloc_fcb(vcb, ino, ftype)
}
