//! A stand-in for a title, written by us. It imports from DLLs that do not
//! exist on a PC, which is exactly the situation victory-loader handles.
//! Loaded normally, this DLL would fail with "module not found".

use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

const DLL_THREAD_ATTACH: u32 = 2;

#[link(name = "xgameruntime", kind = "raw-dylib")]
unsafe extern "system" {
    fn XGameRuntimeInitialize() -> i32;
}

#[link(name = "victory-probe-missing", kind = "raw-dylib")]
unsafe extern "system" {
    fn VIctoryProbeNotImplemented(x: u64) -> u64;
}

// A DLL the "title" ships; the test supplies a renamed copy of version.dll.
#[link(name = "victory-probe-title", kind = "raw-dylib")]
unsafe extern "system" {
    fn GetFileVersionInfoSizeW(path: *const u16, handle: *mut u32) -> u32;
}

// Delay-loaded (see build.rs): bound through the delay-load table.
#[cfg_attr(
    any(target_env = "msvc", target_abi = "llvm"),
    link(name = "victory-probe-delayed", kind = "raw-dylib")
)]
#[cfg_attr(
    all(target_env = "gnu", not(target_abi = "llvm")),
    link(name = "probe_delayed", kind = "static")
)]
unsafe extern "system" {
    fn VIctoryProbeDelayed() -> u32;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetTickCount() -> u32;
    fn GetCurrentProcessId() -> u32;
}

/// Console-only API, answered by the resolver's HLE implementation.
#[unsafe(no_mangle)]
pub extern "system" fn probe_xgameruntime_init() -> i32 {
    unsafe { XGameRuntimeInitialize() }
}

/// Exists on the host too, but the resolver overrides it.
#[unsafe(no_mangle)]
pub extern "system" fn probe_tick() -> u32 {
    unsafe { GetTickCount() }
}

/// Exists on the host and is passed through to it.
#[unsafe(no_mangle)]
pub extern "system" fn probe_pid() -> u32 {
    unsafe { GetCurrentProcessId() }
}

/// An export of a title-shipped DLL: the version-info size of kernel32.dll.
#[unsafe(no_mangle)]
pub extern "system" fn probe_title() -> u32 {
    let path: Vec<u16> = r"C:\Windows\System32\kernel32.dll"
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let mut handle = 0;
    unsafe { GetFileVersionInfoSizeW(path.as_ptr(), &mut handle) }
}

/// A delay-loaded console API.
#[unsafe(no_mangle)]
pub extern "system" fn probe_delayed() -> u32 {
    unsafe { VIctoryProbeDelayed() }
}

/// Nobody implements this one: it must land in an unimplemented-import stub.
#[unsafe(no_mangle)]
pub extern "system" fn probe_missing() -> u64 {
    unsafe { VIctoryProbeNotImplemented(42) }
}

static TLS_THREAD_STARTS: AtomicU32 = AtomicU32::new(0);
static MAIN_THREAD_STARTS: AtomicU32 = AtomicU32::new(0);

/// A TLS callback, as titles have: counts thread starts.
unsafe extern "system" fn on_thread(_: *mut c_void, reason: u32, _: *mut c_void) {
    if reason == DLL_THREAD_ATTACH {
        TLS_THREAD_STARTS.fetch_add(1, Ordering::Relaxed);
    }
}

#[used]
#[unsafe(link_section = ".CRT$XLB")]
static ON_THREAD: unsafe extern "system" fn(*mut c_void, u32, *mut c_void) = on_thread;

const DLL_PROCESS_ATTACH: u32 = 1;

/// Called by the CRT's DLL entry point: counts thread starts too, and calls
/// an import the resolver overrides, as title code calls our runtime.
#[unsafe(no_mangle)]
pub extern "system" fn DllMain(_: *mut c_void, reason: u32, _: *mut c_void) -> i32 {
    if reason == DLL_PROCESS_ATTACH {
        // SAFETY: our own static, written once before any title code can
        // call the memory API (the export is read only through it).
        unsafe {
            __XMemAllocationHooks__ = [hook_alloc as usize, hook_free as usize];
        }
    }
    if reason == DLL_THREAD_ATTACH {
        MAIN_THREAD_STARTS.fetch_add(1, Ordering::Relaxed);
        unsafe { GetTickCount() };
    }
    if reason == DLL_PROCESS_DETACH {
        DETACHES.fetch_add(1, Ordering::Relaxed);
    }
    1
}

const DLL_PROCESS_DETACH: u32 = 0;
static DETACHES: AtomicU32 = AtomicU32::new(0);

/// `DLL_PROCESS_DETACH` notices seen by `DllMain`.
#[unsafe(no_mangle)]
pub extern "system" fn probe_detaches() -> u32 {
    DETACHES.load(Ordering::Relaxed)
}

/// Thread starts seen by the TLS callback (high half) and `DllMain` (low).
#[unsafe(no_mangle)]
pub extern "system" fn probe_thread_starts() -> u64 {
    // SAFETY: reading our own static keeps the linker from dropping it.
    unsafe { std::ptr::from_ref(&ON_THREAD).read_volatile() };
    u64::from(TLS_THREAD_STARTS.load(Ordering::Relaxed)) << 32
        | u64::from(MAIN_THREAD_STARTS.load(Ordering::Relaxed))
}

// The title's allocation hooks, as a title that ships them exports them
// (Grand Theft Auto VI does): `XMemAlloc` and `XMemFree` call the two
// routines, while `XMemAllocDefault` and `XMemFreeDefault` do not. Each
// call is recorded so a test can assert what the memory API passed.
static HOOK_ALLOC_CALLS: AtomicU32 = AtomicU32::new(0);
static HOOK_FREE_CALLS: AtomicU32 = AtomicU32::new(0);
/// Frees whose address or attributes did not match their allocation.
static HOOK_MISMATCHES: AtomicU32 = AtomicU32::new(0);
static HOOK_LAST_SIZE: AtomicU32 = AtomicU32::new(0);
static HOOK_LAST_ATTRIBUTES: AtomicU64 = AtomicU64::new(0);

/// One hook allocation: what it was handed out with, to check the free.
struct HookBlock {
    address: std::sync::atomic::AtomicUsize,
    size: std::sync::atomic::AtomicUsize,
    attributes: AtomicU64,
}
/// The allocation the hook hands out is remembered here, so the free can
/// be checked against it. One at a time is enough: the caller allocates
/// and frees in turn.
static HOOK_BLOCK: HookBlock = HookBlock {
    address: std::sync::atomic::AtomicUsize::new(0),
    size: std::sync::atomic::AtomicUsize::new(0),
    attributes: AtomicU64::new(0),
};
const HOOK_BYTES: usize = 256;

extern "system" fn hook_alloc(size: usize, attributes: u64) -> *mut c_void {
    HOOK_ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
    HOOK_LAST_SIZE.store(size as u32, Ordering::Relaxed);
    HOOK_LAST_ATTRIBUTES.store(attributes, Ordering::Relaxed);
    // A block from the host heap, remembered with what it was handed out
    // for, so the free can be checked.
    let layout = std::alloc::Layout::from_size_align(HOOK_BYTES.max(size), 16).unwrap();
    // SAFETY: a non-zero size.
    let at = unsafe { std::alloc::alloc(layout) };
    HOOK_BLOCK.address.store(at as usize, Ordering::Relaxed);
    HOOK_BLOCK.size.store(size, Ordering::Relaxed);
    HOOK_BLOCK.attributes.store(attributes, Ordering::Relaxed);
    at.cast()
}

extern "system" fn hook_free(address: *mut c_void, attributes: u64) {
    HOOK_FREE_CALLS.fetch_add(1, Ordering::Relaxed);
    HOOK_LAST_ATTRIBUTES.store(attributes, Ordering::Relaxed);
    let (size, wanted) = (
        HOOK_BLOCK.size.load(Ordering::Relaxed),
        HOOK_BLOCK.attributes.load(Ordering::Relaxed),
    );
    if HOOK_BLOCK.address.load(Ordering::Relaxed) != address as usize
        || attributes != wanted
    {
        HOOK_MISMATCHES.fetch_add(1, Ordering::Relaxed);
        return;
    }
    HOOK_BLOCK.address.store(0, Ordering::Relaxed);
    let layout = std::alloc::Layout::from_size_align(HOOK_BYTES.max(size), 16).unwrap();
    // SAFETY: the block came from the same layout in `hook_alloc`.
    unsafe { std::alloc::dealloc(address.cast(), layout) };
}

/// The title's allocation hooks: two routine pointers, `XMemAlloc` and
/// `XMemFree` call them for the title and its middleware. A C title fills
/// this in at link time; Rust cannot cast code pointers to integers in a
/// constant, so `DllMain` fills it in at attach, before any title code
/// calls the memory API.
#[unsafe(no_mangle)]
pub static mut __XMemAllocationHooks__: [usize; 2] = [0, 2];

/// Hook calls so far: allocations in the high half, frees in the low.
#[unsafe(no_mangle)]
pub extern "system" fn probe_xmem_hook_calls() -> u64 {
    u64::from(HOOK_ALLOC_CALLS.load(Ordering::Relaxed)) << 32
        | u64::from(HOOK_FREE_CALLS.load(Ordering::Relaxed))
}

/// Frees the hook refused: an address it did not hand out, or other
/// attributes than the allocation's.
#[unsafe(no_mangle)]
pub extern "system" fn probe_xmem_hook_mismatches() -> u32 {
    HOOK_MISMATCHES.load(Ordering::Relaxed)
}

/// The arguments of the hook's last call: the attributes in the high
/// half, the size in the low.
#[unsafe(no_mangle)]
pub extern "system" fn probe_xmem_hook_last_arguments() -> u128 {
    u128::from(HOOK_LAST_ATTRIBUTES.load(Ordering::Relaxed)) << 64
        | u128::from(HOOK_LAST_SIZE.load(Ordering::Relaxed))
}

/// Writes through a null pointer: a fault in title code, for the crash
/// report to name this image and unwind its frames.
#[unsafe(no_mangle)]
pub extern "system" fn probe_fault() {
    // SAFETY: the write faults; that is the point.
    unsafe { std::ptr::null_mut::<u8>().write_volatile(0x2a) };
}
