//! Registre des volumes montés (protégé par FAST_MUTEX).

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use core::cell::UnsafeCell;
use ext4_core::fs::Filesystem;
use ext4_core::BlockDevice;
use wdk_sys::ntddk::{ExAcquireFastMutex, ExReleaseFastMutex};
use wdk_sys::FAST_MUTEX;

use crate::ntshim::init_fast_mutex;
use crate::zpdev::{KernelTime, ZwBlockDevice};

/// Un volume ext4 monté côté kernel. Le Filesystem possède l'appareil bloc
/// (ZwBlockDevice) : son Drop referme le handle fichier.
pub struct Volume {
    pub id: u32,
    pub read_only: bool,
    pub fs: Filesystem,
}

/// Registre global : BTreeMap<id, Volume> sous FAST_MUTEX.
pub struct VolumeRegistry {
    // UnsafeCell : les méthodes prennent &self mais le mutex noyau mute par
    // pointeur brut (ExAcquireFastMutex) — cohérent avec `inner`.
    mutex: UnsafeCell<FAST_MUTEX>,
    inner: UnsafeCell<RegistryInner>,
    next_id: u32,
}

struct RegistryInner {
    volumes: BTreeMap<u32, Volume>,
    open_count: usize, // handles ouverts sur le device de contrôle
}

unsafe impl Sync for VolumeRegistry {}

impl VolumeRegistry {
    pub fn new() -> VolumeRegistry {
        let mutex = UnsafeCell::new(FAST_MUTEX::default());
        unsafe {
            init_fast_mutex(mutex.get());
        }
        VolumeRegistry {
            mutex,
            inner: UnsafeCell::new(RegistryInner {
                volumes: BTreeMap::new(),
                open_count: 0,
            }),
            next_id: 1,
        }
    }

    pub fn open_count_inc(&self) {
        unsafe {
            ExAcquireFastMutex(self.mutex.get());
            (*self.inner.get()).open_count += 1;
            ExReleaseFastMutex(self.mutex.get());
        }
    }

    pub fn open_count_dec(&self) -> usize {
        unsafe {
            ExAcquireFastMutex(self.mutex.get());
            let inner = &mut *self.inner.get();
            inner.open_count -= 1;
            let n = inner.open_count;
            ExReleaseFastMutex(self.mutex.get());
            n
        }
    }

    pub fn open_count(&self) -> usize {
        unsafe {
            ExAcquireFastMutex(self.mutex.get());
            let n = (*self.inner.get()).open_count;
            ExReleaseFastMutex(self.mutex.get());
            n
        }
    }

    /// Monte un volume ext4 (replay journal inclus). Retourne l'id du volume.
    pub fn mount(&self, path: &[u16], read_only: bool) -> Result<u32, i32> {
        let device = match ZwBlockDevice::open(path, read_only) {
            Ok(d) => d,
            Err(ext4_core::Error::Device(c)) => return Err(c),
            Err(_) => return Err(0xC000_00BBu32 as i32), // STATUS_NOT_SUPPORTED
        };
        let ro = read_only || device.is_read_only();
        let fs = match Filesystem::mount(Box::new(device), Box::new(KernelTime), read_only) {
            Ok(fs) => fs,
            Err(e) => {
                wdk::println!("ext4drv: montage échoué: {e}");
                return Err(0xC000_00BBu32 as i32); // STATUS_NOT_SUPPORTED
            }
        };
        let id = unsafe {
            ExAcquireFastMutex(self.mutex.get());
            let inner = &mut *self.inner.get();
            let id = self.next_id;
            self.next_id = self.next_id.wrapping_add(1);
            inner.volumes.insert(id, Volume { id, read_only: ro, fs });
            ExReleaseFastMutex(self.mutex.get());
            id
        };
        Ok(id)
    }

    pub fn unmount(&self, id: u32) -> Result<(), i32> {
        unsafe {
            ExAcquireFastMutex(self.mutex.get());
            let inner = &mut *self.inner.get();
            match inner.volumes.remove(&id) {
                Some(mut v) => {
                    let _ = v.fs.unmount();
                    ExReleaseFastMutex(self.mutex.get());
                    Ok(())
                }
                None => {
                    ExReleaseFastMutex(self.mutex.get());
                    Err(0xC000_0004u32 as i32) // STATUS_INVALID_HANDLE
                }
            }
        }
    }

    /// Accès exclusif à un volume pendant le traitement d'une requête.
    pub fn with_volume<R>(
        &self,
        id: u32,
        f: impl FnOnce(&mut Volume) -> Result<R, i32>,
    ) -> Result<R, i32> {
        unsafe {
            ExAcquireFastMutex(self.mutex.get());
            let inner = &mut *self.inner.get();
            let result = match inner.volumes.get_mut(&id) {
                Some(v) => f(v),
                None => Err(0xC000_0004u32 as i32), // STATUS_INVALID_HANDLE
            };
            ExReleaseFastMutex(self.mutex.get());
            result
        }
    }

    pub fn first_volume_id(&self) -> Option<u32> {
        unsafe {
            ExAcquireFastMutex(self.mutex.get());
            let id = (*self.inner.get()).volumes.keys().next().copied();
            ExReleaseFastMutex(self.mutex.get());
            id
        }
    }

    pub fn has_volumes(&self) -> bool {
        unsafe {
            ExAcquireFastMutex(self.mutex.get());
            let n = !(*self.inner.get()).volumes.is_empty();
            ExReleaseFastMutex(self.mutex.get());
            n
        }
    }
}
