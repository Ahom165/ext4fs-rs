//! Appareil bloc ext4-core sur fichier Windows (Zw*), + source de temps kernel.

use alloc::boxed::Box;
use alloc::vec;
use core::ffi::c_void;
use wdk_sys::{
    ntddk::{ZwClose, ZwCreateFile, ZwQueryInformationFile, ZwReadFile, ZwWriteFile},
    FILE_STANDARD_INFORMATION, HANDLE, IO_STATUS_BLOCK, LARGE_INTEGER, OBJECT_ATTRIBUTES,
    PWSTR, UNICODE_STRING,
};
use wdk_sys::{OBJ_CASE_INSENSITIVE, OBJ_KERNEL_HANDLE};

use ext4_core::blockdev::{check_range, BlockDevice};
use ext4_core::error::{Error, Result};
use ext4_core::util::TimeSource;

/// Convertit un chemin UTF-16 Windows en UNICODE_STRING (sans allocation :
/// le buffer vit dans l'appelant).
pub fn unicode_string(buf: &mut [u16]) -> UNICODE_STRING {
    let len = buf.len() * 2;
    UNICODE_STRING {
        Length: len as u16,
        MaximumLength: len as u16,
        Buffer: buf.as_mut_ptr(),
    }
}

/// Appareil bloc : fichier image / volume ouvert via ZwCreateFile en
/// mode synchrone. Toutes les E/S sont positionnées (offset explicite).
pub struct ZwBlockDevice {
    handle: HANDLE,
    size: u64,
    read_only: bool,
}

/// Préfixe \??\ si nécessaire (chemins Win32 → NT). Les chemins déjà NT
/// (\Device\... ou \??\...) sont laissés tels quels.
pub fn nt_path_prefix(path: &[u16]) -> alloc::vec::Vec<u16> {
    const PREFIX: [u16; 4] = [0x5C, 0x3F, 0x3F, 0x5C]; // \??\
    const DEVICE: [u16; 8] = [
        0x5C, 0x44, 0x65, 0x76, 0x69, 0x63, 0x65, 0x5C, // \Device\
    ];
    if path.len() >= 4 && path[..4] == PREFIX {
        alloc::vec::Vec::from(path)
    } else if path.len() >= 8 && path[..8] == DEVICE {
        alloc::vec::Vec::from(path)
    } else {
        let mut v = alloc::vec::Vec::with_capacity(path.len() + 5);
        v.extend_from_slice(&PREFIX);
        v.extend_from_slice(path);
        v.push(0);
        v
    }
}

impl ZwBlockDevice {
    /// Ouvre le fichier image. `path` = UTF-16 sans terminateur (Win32 ou NT).
    pub fn open(path: &[u16], read_only: bool) -> Result<ZwBlockDevice> {
        let full = nt_path_prefix(path);
        let mut name_buf = full;
        let mut nt_name = unicode_string(&mut name_buf);
        let mut oa = OBJECT_ATTRIBUTES {
            Length: core::mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
            RootDirectory: core::ptr::null_mut(),
            ObjectName: &mut nt_name,
            Attributes: (OBJ_KERNEL_HANDLE | OBJ_CASE_INSENSITIVE) as u32,
            SecurityDescriptor: core::ptr::null_mut(),
            SecurityQualityOfService: core::ptr::null_mut(),
        };
        let mut iosb = IO_STATUS_BLOCK::default();
        let desired = if read_only {
            0x0001 | 0x0010 // FILE_READ_DATA | SYNCHRONIZE
        } else {
            0x0001 | 0x0002 | 0x0010 // + FILE_WRITE_DATA
        };
        let share = 0x1 | 0x2 | 0x4; // FILE_SHARE_READ | WRITE | DELETE
        let mut handle: HANDLE = core::ptr::null_mut();
        let status = unsafe {
            ZwCreateFile(
                &mut handle,
                desired,
                &mut oa,
                &mut iosb,
                core::ptr::null_mut(),      // AllocationSize
                0x0000_0080,                // FILE_ATTRIBUTE_NORMAL
                share,
                0x0000_0001,                // FILE_OPEN
                0x0000_0020,                // FILE_SYNCHRONOUS_IO_NONALERT
                core::ptr::null_mut(),      // EaBuffer
                0,
            )
        };
        if status != 0 {
            wdk::println!("ext4drv: ZwCreateFile échoué {status:#x}");
            return Err(Error::Device(status));
        }
        // taille via FileStandardInformation
        let mut std_info = FILE_STANDARD_INFORMATION::default();
        let mut qiosb = IO_STATUS_BLOCK::default();
        let qstatus = unsafe {
            ZwQueryInformationFile(
                handle,
                &mut qiosb,
                &mut std_info as *mut _ as *mut c_void,
                core::mem::size_of::<FILE_STANDARD_INFORMATION>() as u32,
                5i32, // FileStandardInformation
            )
        };
        if qstatus != 0 {
            unsafe { ZwClose(handle) };
            return Err(Error::Device(qstatus));
        }
        let size = unsafe { core::ptr::read(core::ptr::addr_of!(std_info.EndOfFile)).QuadPart } as u64;
        Ok(ZwBlockDevice {
            handle,
            size,
            read_only,
        })
    }
}

impl BlockDevice for ZwBlockDevice {
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        check_range(offset, buf.len(), self.size)?;
        if buf.is_empty() {
            return Ok(());
        }
        let mut off = LARGE_INTEGER::default();
        off.QuadPart = offset as i64;
        let mut iosb = IO_STATUS_BLOCK::default();
        let status = unsafe {
            ZwReadFile(
                self.handle,
                core::ptr::null_mut(),
                None,
                core::ptr::null_mut(),
                &mut iosb,
                buf.as_mut_ptr() as *mut c_void,
                buf.len() as u32,
                &mut off,
                core::ptr::null_mut(),
            )
        };
        if status != 0 {
            return Err(Error::Device(status));
        }
        if iosb.Information < buf.len() as u64 {
            return Err(Error::Device(0xC000_0001u32 as i32)); // STATUS_UNSUCCESSFUL
        }
        Ok(())
    }

    fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<()> {
        if self.read_only {
            return Err(Error::ReadOnly);
        }
        check_range(offset, buf.len(), self.size)?;
        if buf.is_empty() {
            return Ok(());
        }
        let mut off = LARGE_INTEGER::default();
        off.QuadPart = offset as i64;
        let mut iosb = IO_STATUS_BLOCK::default();
        let status = unsafe {
            ZwWriteFile(
                self.handle,
                core::ptr::null_mut(),
                None,
                core::ptr::null_mut(),
                &mut iosb,
                buf.as_ptr() as *const c_void as *mut c_void,
                buf.len() as u32,
                &mut off,
                core::ptr::null_mut(),
            )
        };
        if status != 0 {
            return Err(Error::Device(status));
        }
        Ok(())
    }

    fn size(&mut self) -> Result<u64> {
        Ok(self.size)
    }

    fn is_read_only(&self) -> bool {
        self.read_only
    }
}

impl Drop for ZwBlockDevice {
    fn drop(&mut self) {
        unsafe {
            ZwClose(self.handle);
        }
    }
}

/// Source de temps kernel (KeQuerySystemTime, 100 ns depuis 1601).
pub struct KernelTime;

impl TimeSource for KernelTime {
    fn now(&self) -> (u32, u32) {
        let mut t = LARGE_INTEGER::default();
        unsafe {
            crate::ntshim::KeQuerySystemTime(&mut t as *mut LARGE_INTEGER as *mut i64);
            // union : lecture unsafe requise
            let q = core::ptr::read(core::ptr::addr_of!(t.QuadPart));
            let unix = (q / 10_000_000) - 11_644_473_600;
            (unix as u32, 0)
        }
    }
}

// ---------------------------------------------------------------------------
// Appareil bloc sur DEVICE OBJECT réel (FSD) : stream file object + IRPs
// manuels. Utilisé par le montage IRP_MN_MOUNT_VOLUME — NE PAS passer par un
// chemin NT (récursion de montage).
// ---------------------------------------------------------------------------

use wdk_sys::DEVICE_OBJECT;

/// Device block FSD : possède un stream file object sur le volume réel.
/// Toutes les E/S sont positionnées (offset explicite) et SYNCHRONES.
pub struct DevBlockDevice {
    pub fo: *mut core::ffi::c_void, // PFILE_OBJECT (stream)
    pub dev: *mut DEVICE_OBJECT,    // haut de pile associé
    size: u64,
    read_only: bool,
}

unsafe impl Send for DevBlockDevice {}
unsafe impl Sync for DevBlockDevice {}

impl DevBlockDevice {
    /// Crée le stream file object sur le device réel et interroge la taille.
    pub fn open(real_dev: *mut DEVICE_OBJECT) -> Result<DevBlockDevice> {
        unsafe {
            let fo = IoCreateStreamFileObject(
                core::ptr::null_mut(),
                real_dev as *mut c_void,
            );
            if fo.is_null() {
                return Err(Error::Device(nd::st::UNSUCCESSFUL));
            }
            let dev = IoGetRelatedDeviceObject(fo) as *mut DEVICE_OBJECT;
            if dev.is_null() {
                ObDereferenceObject(fo as *mut c_void);
                return Err(Error::Device(nd::st::UNSUCCESSFUL));
            }
            // taille du volume : IOCTL_DISK_GET_LENGTH_INFO
            let mut out: i64 = 0;
            let mut iosb = IO_STATUS_BLOCK::default();
            let status = sync_devctrl(dev, 0x0007_4050, core::ptr::null_mut(), 0, &mut out as *mut i64 as *mut c_void, 8, &mut iosb);
            // 0x00074050 = CTL_CODE(IOCTL_DISK_BASE=7, 0x14, BUFFERED, READ) GET_LENGTH_INFO
            if status != 0 {
                wdk::println!("ext4fsd: IOCTL_DISK_GET_LENGTH_INFO échoué {status:#x}");
                // volume sans taille connue : bornes désactivées (0 = infini)
            }
            Ok(DevBlockDevice { fo: fo as *mut c_void, dev, size: if out > 0 { out as u64 } else { u64::MAX }, read_only: false })
        }
    }

    /// Flush du cache disque (IRP_MJ_FLUSH_BUFFERS).
    pub fn flush_device(&mut self) -> Result<()> {
        unsafe {
            let mut iosb = IO_STATUS_BLOCK::default();
            let status = sync_fsd(self.dev, 0x09, core::ptr::null_mut(), 0, 0, &mut iosb);
            if status != 0 {
                return Err(Error::Device(status));
            }
            Ok(())
        }
    }
}

impl BlockDevice for DevBlockDevice {
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        if buf.is_empty() {
            return Ok(());
        }
        unsafe {
            let mut iosb = IO_STATUS_BLOCK::default();
            let status = sync_fsd(self.dev, 0x03, buf.as_mut_ptr() as *mut c_void, buf.len(), offset as i64, &mut iosb);
            if status != 0 {
                return Err(Error::Device(status));
            }
            if (iosb.Information as usize) < buf.len() {
                return Err(Error::Device(0xC000_0001u32 as i32));
            }
            Ok(())
        }
    }

    fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<()> {
        if self.read_only {
            return Err(Error::ReadOnly);
        }
        if buf.is_empty() {
            return Ok(());
        }
        unsafe {
            let mut iosb = IO_STATUS_BLOCK::default();
            let status = sync_fsd(self.dev, 0x04, buf.as_ptr() as *mut c_void as *mut c_void, buf.len(), offset as i64, &mut iosb);
            if status != 0 {
                return Err(Error::Device(status));
            }
            Ok(())
        }
    }

    fn size(&mut self) -> Result<u64> {
        Ok(self.size)
    }

    fn is_read_only(&self) -> bool {
        self.read_only
    }

    fn flush(&mut self) -> Result<()> {
        self.flush_device()
    }
}

impl Drop for DevBlockDevice {
    fn drop(&mut self) {
        unsafe {
            ObDereferenceObject(self.fo);
        }
    }
}

/// Lecture/écriture synchrone sur device (IRP_MJ_READ/WRITE).
unsafe fn sync_fsd(
    dev: *mut DEVICE_OBJECT,
    major: u8,
    buf: *mut c_void,
    len: usize,
    offset: i64,
    iosb: *mut IO_STATUS_BLOCK,
) -> i32 {
    use crate::ntshim::{IoBuildSynchronousFsdRequest, KeInitializeEvent, KeWaitForSingleObject};
    let event: *mut c_void = alloc_event();
    KeInitializeEvent(event as *mut _, 0 /* NotificationEvent */, 0);
    let mut off = LARGE_INTEGER::default();
    off.QuadPart = offset;
    let irp = IoBuildSynchronousFsdRequest(
        major as u32,
        dev as *mut c_void,
        buf,
        len as u32,
        &mut off,
        event as *mut _,
        iosb as *mut c_void,
    );
    if irp.is_null() {
        free_event(event);
        return 0xC000_009Au32 as i32; // STATUS_INSUFFICIENT_RESOURCES
    }
    let status = IofCallDriver(dev as *mut c_void, irp);
    if status == 0x0000_0103u32 as i32 {
        // STATUS_PENDING : attente indéfinie de l'événement
        let _ = KeWaitForSingleObject(event as *mut _, 0 /* Executive */, KernelMode, 0, core::ptr::null_mut());
    }
    free_event(event);
    core::ptr::read(core::ptr::addr_of!((*iosb).__bindgen_anon_1.Status))
}

/// IOCTL synchrone sur device.
unsafe fn sync_devctrl(
    dev: *mut DEVICE_OBJECT,
    code: u32,
    input: *mut c_void,
    in_len: u32,
    output: *mut c_void,
    out_len: u32,
    iosb: *mut IO_STATUS_BLOCK,
) -> i32 {
    use crate::ntshim::{IoBuildDeviceIoControlRequest, KeInitializeEvent, KeWaitForSingleObject};
    let event: *mut c_void = alloc_event();
    KeInitializeEvent(event as *mut _, 0, 0);
    let irp = IoBuildDeviceIoControlRequest(
        code,
        dev as *mut c_void,
        input,
        in_len,
        output,
        out_len,
        0, // InternalDeviceControl = FALSE
        event as *mut _,
        iosb as *mut c_void,
    );
    if irp.is_null() {
        free_event(event);
        return 0xC000_009Au32 as i32;
    }
    let status = IofCallDriver(dev as *mut c_void, irp);
    if status == 0x0000_0103u32 as i32 {
        let _ = KeWaitForSingleObject(event as *mut _, 0, KernelMode, 0, core::ptr::null_mut());
    }
    free_event(event);
    core::ptr::read(core::ptr::addr_of!((*iosb).__bindgen_anon_1.Status))
}

// KEVENT (layout x64 : DISPATCHER_HEADER 0x18 octets)
#[repr(C, align(8))]
struct KeEvent {
    header: [u8; 0x18],
}

/// Alloue un KEVENT en NonPaged pool (ExAllocatePoolWithTag est blocklisté
/// dans wdk-sys : on passe par l'allocateur Rust, NonPaged via wdk_alloc).
unsafe fn alloc_event() -> *mut c_void {
    Box::into_raw(Box::new(KeEvent { header: [0u8; 0x18] })) as *mut c_void
}
unsafe fn free_event(e: *mut c_void) {
    if !e.is_null() {
        drop(Box::from_raw(e as *mut KeEvent));
    }
}

use crate::ntdefs as nd;
use crate::ntshim::*;
