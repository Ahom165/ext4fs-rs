//! Déclarations FFI explicites des services NT utilisés par la couche FSD.
//!
//! wdk-sys génère la plupart depuis ntifs.h/ntddk.h ; pour tout ce qui
//! n'est pas garanti (fonctions ntifs supplémentaires, macros C comme
//! MmGetSystemAddressForMdlSafe), on déclare ici une liaison identique
//! (même symbole NTOSKRNL : aucune contradiction de linking possible).

#![allow(non_snake_case, dead_code, non_upper_case_globals)]

use core::ffi::c_void;

use wdk_sys::{FAST_MUTEX, LARGE_INTEGER, PIRP, UNICODE_STRING};

// Types opaques (pointeurs vers des blocs alloués par les services NT)
pub type PMDL = *mut c_void;
pub type PNOTIFY_SYNC = *mut c_void;
pub type PIRPSP = *mut core::ffi::c_void;
pub type PEPROCESS = *mut c_void;

// ---------------------------------------------------------------------------
// Cache Manager (Cc*)
// ---------------------------------------------------------------------------

/// CC_FILE_SIZES
#[repr(C)]
#[derive(Clone, Copy)]
pub struct CC_FILE_SIZES {
    pub AllocationSize: i64,
    pub FileSize: i64,
    pub ValidDataLength: i64,
}

/// CACHE_MANAGER_CALLBACKS (layout ntifs.h : 4 pointeurs de fonctions)
#[repr(C)]
pub struct CACHE_MANAGER_CALLBACKS {
    pub AcquireForLazyWrite: unsafe extern "system" fn(*mut c_void, u8) -> u8,
    pub ReleaseFromLazyWrite: unsafe extern "system" fn(*mut c_void),
    pub AcquireForReadAhead: unsafe extern "system" fn(*mut c_void, u8) -> u8,
    pub ReleaseFromReadAhead: unsafe extern "system" fn(*mut c_void),
}

unsafe extern "system" {
    pub fn CcInitializeCacheMap(
        FileObject: *mut c_void,
        FileSizes: *mut CC_FILE_SIZES,
        PinAccess: u8,
        Callbacks: *mut CACHE_MANAGER_CALLBACKS,
        LazyWriteContext: *mut c_void,
    );

    pub fn CcUninitializeCacheMap(
        FileObject: *mut c_void,
        TruncateSize: *mut i64,
        UninitializeEvent: *mut c_void,
    ) -> u8;

    pub fn CcCopyRead(
        FileObject: *mut c_void,
        FileOffset: *mut i64,
        Length: u32,
        Wait: u8,
        Buffer: *mut c_void,
        IoStatus: *mut c_void, // PIO_STATUS_BLOCK
    ) -> u8;

    pub fn CcCopyWrite(
        FileObject: *mut c_void,
        FileOffset: *mut i64,
        Length: u32,
        Wait: u8,
        Buffer: *mut c_void,
    ) -> u8;

    pub fn CcSetFileSizes(FileObject: *mut c_void, FileSizes: *mut CC_FILE_SIZES);

    pub fn CcFlushCache(
        SectionObjectPointer: *mut c_void,
        FileOffset: *mut i64,
        Length: u32,
        IoStatus: *mut c_void,
    );

    pub fn CcPurgeCacheSection(
        SectionObjectPointer: *mut c_void,
        FileOffset: *mut i64,
        Length: u32,
        UninitializeCacheMaps: u8,
    );
}

// ---------------------------------------------------------------------------
// FsRtl (notifications + matching de noms)
// ---------------------------------------------------------------------------

unsafe extern "system" {
    pub fn FsRtlNotifyInitializeSync(NotifySync: *mut PNOTIFY_SYNC);
    pub fn FsRtlNotifyUninitializeSync(NotifySync: *mut PNOTIFY_SYNC);

    pub fn FsRtlNotifyFullChangeDirectory(
        NotifySync: PNOTIFY_SYNC,
        NotifyList: *mut c_void, // PLIST_ENTRY (dans le Fcb)
        FsContext: *mut c_void,
        FullDirectoryName: *mut UNICODE_STRING,
        WatchTree: u8,
        IgnoreFilters: u8,
        CompletionFilter: u32,
        NotifyIrp: PIRP,
        TraverseCallback: *mut c_void,
        SubjectContext: *mut c_void,
    );

    pub fn FsRtlNotifyFullReportChange(
        NotifySync: PNOTIFY_SYNC,
        NotifyList: *mut c_void,
        FullTargetName: *mut UNICODE_STRING,
        TargetNameOffset: u16,
        OldFullTargetName: *mut UNICODE_STRING,
        OldTargetNameOffset: u16,
        OldTargetNameLength: u16,
        FilterMatch: u32,
        Action: u32,
    );

    pub fn FsRtlNotifyCleanup(
        NotifySync: PNOTIFY_SYNC,
        NotifyList: *mut c_void,
        FsContext: *mut c_void,
    );

    pub fn FsRtlIsNameInExpression(
        Expression: *mut UNICODE_STRING,
        Name: *mut UNICODE_STRING,
        IgnoreCase: u8,
        UpcaseTable: *mut c_void,
    ) -> u8;

    // Oplocks (package FsRtl, opéré côté FSD v2)
    pub fn FsRtlInitializeOplock(Oplock: *mut c_void);
    pub fn FsRtlUninitializeOplock(Oplock: *mut c_void);
    pub fn FsRtlCheckOplock(
        Oplock: *mut c_void,
        Irp: PIRP,
        Context: *mut c_void,
        CompletionRoutine: *mut c_void,
        PostIrpRoutine: *mut c_void,
    ) -> i32;
    pub fn FsRtlOplockFsctrl(Oplock: *mut c_void, Irp: PIRP, OpenCount: u32) -> i32;
}

// Filtres de notification (FILE_NOTIFY_CHANGE_*)
pub const FILE_NOTIFY_CHANGE_FILE_NAME: u32 = 0x1;
pub const FILE_NOTIFY_CHANGE_DIR_NAME: u32 = 0x2;
pub const FILE_NOTIFY_CHANGE_ATTRIBUTES: u32 = 0x4;
pub const FILE_NOTIFY_CHANGE_SIZE: u32 = 0x8;
pub const FILE_NOTIFY_CHANGE_LAST_WRITE: u32 = 0x10;
pub const FILE_NOTIFY_CHANGE_LAST_ACCESS: u32 = 0x20;
pub const FILE_NOTIFY_CHANGE_CREATION: u32 = 0x40;
pub const FILE_NOTIFY_CHANGE_EA: u32 = 0x80;
pub const FILE_NOTIFY_CHANGE_SECURITY: u32 = 0x100;
pub const FILE_NOTIFY_CHANGE_STREAM_NAME: u32 = 0x200;
pub const FILE_NOTIFY_CHANGE_STREAM_SIZE: u32 = 0x400;
pub const FILE_NOTIFY_CHANGE_STREAM_WRITE: u32 = 0x800;

// FILE_NOTIFY_ACTION (FsRtlNotifyFullReportChange FilterMatch)
pub const FILE_NOTIFY_CHANGE_NAME: u32 = 0x3; // FILE|DIR NAME
pub const FILE_ACTION_ADDED: u32 = 0x1;
pub const FILE_ACTION_REMOVED: u32 = 0x2;
pub const FILE_ACTION_MODIFIED: u32 = 0x3;
pub const FILE_ACTION_RENAMED_OLD_NAME: u32 = 0x4;
pub const FILE_ACTION_RENAMED_NEW_NAME: u32 = 0x5;

// ---------------------------------------------------------------------------
// I/O manager / Ob
// ---------------------------------------------------------------------------

pub const KernelMode: i8 = 0;
pub const MmCached: u32 = 6; // MmCached
pub const NormalPagePriority: u32 = 0x0;
pub const LowPagePriority: u32 = 0x1;

unsafe extern "system" {
    pub fn IoRegisterFileSystem(DeviceObject: *mut c_void);
    pub fn IoUnregisterFileSystem(DeviceObject: *mut c_void);
    pub fn IoRegisterShutdownNotification(DeviceObject: *mut c_void);
    pub fn IoUnregisterShutdownNotification(DeviceObject: *mut c_void);

    pub fn IoCheckShareAccess(
        DesiredAccess: u32,
        DesiredShareAccess: u32,
        FileObject: *mut c_void,
        ShareAccess: *mut SHARE_ACCESS,
        Update: u8,
    ) -> i32;

    pub fn IoSetShareAccess(
        DesiredAccess: u32,
        DesiredShareAccess: u32,
        FileObject: *mut c_void,
        ShareAccess: *mut SHARE_ACCESS,
    );

    pub fn IoRemoveShareAccess(FileObject: *mut c_void, ShareAccess: *mut SHARE_ACCESS);
    pub fn IoUpdateShareAccess(FileObject: *mut c_void, ShareAccess: *mut SHARE_ACCESS);

    pub fn ObQueryNameString(
        Object: *mut c_void,
        ObjectNameInfo: *mut OBJECT_NAME_INFORMATION,
        Length: u32,
        ReturnLength: *mut u32,
    ) -> i32;

    pub fn MmGetSystemAddressForMdlSafe(Mdl: PMDL, Priority: u32) -> *mut c_void;

    pub fn MmMapLockedPagesSpecifyCache(
        MemoryDescriptorList: PMDL,
        AccessMode: i8,
        CacheType: u32,
        RequestedAddress: *mut c_void,
        BugCheckOnFailure: u8,
        Priority: u32,
    ) -> *mut c_void;

    pub fn MmFlushImageSection(SectionObjectPointer: *mut c_void, FlushProtocol: u32) -> u8;
    pub fn MmCanFileBeTruncated(SectionObjectPointer: *mut c_void, NewFileSize: *mut i64) -> u8;

    // Byte range locks (FILE_LOCK opaque géré par FsRtl)
    pub fn FsRtlAllocateFileLock(
        PoolCharge: *mut c_void,
        CompleteLockIrpRoutine: *mut c_void,
    ) -> *mut c_void;
    pub fn FsRtlUninitializeFileLock(FileLock: *mut c_void);
    pub fn FsRtlProcessFileLock(FileLock: *mut c_void, Irp: PIRP, Context: *mut c_void) -> i32;
    pub fn FsRtlCheckLockForOplock(FileLock: *mut c_void) -> u8;

    pub fn ZwDeviceIoControlFile(
        FileHandle: *mut c_void,
        Event: *mut c_void,
        ApcRoutine: *mut c_void,
        ApcContext: *mut c_void,
        IoStatusBlock: *mut c_void,
        IoControlCode: u32,
        InputBuffer: *mut c_void,
        InputBufferLength: u32,
        OutputBuffer: *mut c_void,
        OutputBufferLength: u32,
    ) -> i32;

    pub fn ZwOpenFile(
        FileHandle: *mut *mut c_void,
        DesiredAccess: u32,
        ObjectAttributes: *mut wdk_sys::OBJECT_ATTRIBUTES,
        IoStatusBlock: *mut c_void,
        ShareAccess: u32,
        OpenOptions: u32,
    ) -> i32;

    pub fn ObReferenceObjectByPointer(
        Object: *mut c_void,
        DesiredAccess: u32,
        ObjectType: *mut c_void,
        AccessMode: i8,
    ) -> i32;
    pub fn ObDereferenceObject(Object: *mut c_void);

    pub fn ExTryToAcquireFastMutex(FastMutex: *mut FAST_MUTEX) -> u8;
    pub fn KeDelayExecutionThread(WaitMode: i8, Alertable: u8, Interval: *mut LARGE_INTEGER) -> i32;

    pub fn IofCallDriver(DeviceObject: *mut c_void, Irp: PIRP) -> i32;

    pub fn IoCreateStreamFileObject(
        FileObject: *mut c_void,
        DeviceObject: *mut c_void,
    ) -> *mut c_void;
    pub fn IoGetRelatedDeviceObject(FileObject: *mut c_void) -> *mut c_void;
    pub fn IoBuildSynchronousFsdRequest(
        MajorFunction: u32,
        DeviceObject: *mut c_void,
        Buffer: *mut c_void,
        Length: u32,
        StartingOffset: *mut LARGE_INTEGER,
        Event: *mut c_void,
        IoStatusBlock: *mut c_void,
    ) -> PIRP;
    pub fn IoBuildDeviceIoControlRequest(
        IoControlCode: u32,
        DeviceObject: *mut c_void,
        InputBuffer: *mut c_void,
        InputBufferLength: u32,
        OutputBuffer: *mut c_void,
        OutputBufferLength: u32,
        InternalDeviceControl: u8,
        Event: *mut c_void,
        IoStatusBlock: *mut c_void,
    ) -> PIRP;
    pub fn KeInitializeEvent(Event: *mut c_void, Type: i32, State: u8);
    pub fn KeSetEvent(Event: *mut c_void, Increment: i32, Wait: u8) -> i32;
    pub fn KeWaitForSingleObject(
        Object: *mut c_void,
        WaitReason: i32,
        WaitMode: i8,
        Alertable: u8,
        Timeout: *mut LARGE_INTEGER,
    ) -> i32;

    // Threads système (worker de commit périodique)
    pub fn PsCreateSystemThread(
        ThreadHandle: *mut *mut c_void,
        DesiredAccess: u32,
        ObjectAttributes: *mut c_void,
        ProcessHandle: *mut c_void,
        ClientId: *mut c_void,
        StartRoutine: unsafe extern "system" fn(*mut c_void),
        StartContext: *mut c_void,
    ) -> i32;
    pub fn PsTerminateSystemThread(Status: i32) -> i32;
    pub fn ZwWaitForSingleObject(
        Handle: *mut c_void,
        Alertable: u8,
        Timeout: *mut LARGE_INTEGER,
    ) -> i32;
}

/// IoSkipCurrentIrpStackLocation (macro C réimplémentée).
#[inline]
pub unsafe fn skip_current_stack_location(irp: PIRP) {
    (*irp).CurrentLocation += 1;
    (*irp).Tail.Overlay.CurrentStackLocation =
        ((*irp).Tail.Overlay.CurrentStackLocation as *mut u8)
            .add(core::mem::size_of::<wdk_sys::IO_STACK_LOCATION>()) as *mut _;
}

pub const MmFlushImageWriteToDisk: u32 = 0; // MmFlushImageWriteToDisk
pub const MmFlushImageDelete: u32 = 1; // MmFlushImageDeleteForSection

/// SHARE_ACCESS (wdm.h)
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct SHARE_ACCESS {
    pub OpenCount: u32,
    pub Readers: u32,
    pub Writers: u32,
    pub Deleters: u32,
    pub SharedRead: u32,
    pub SharedWrite: u32,
    pub SharedDelete: u32,
}

/// OBJECT_NAME_INFORMATION (réponse d'ObQueryNameString)
#[repr(C)]
pub struct OBJECT_NAME_INFORMATION {
    pub Name: UNICODE_STRING,
    // buffer alloué juste derrière
}

/// MdlFlags utiles (lecture directe de (*irp).MdlAddress->MdlFlags)
pub const MDL_PAGES_LOCKED: u16 = 0x0008;
