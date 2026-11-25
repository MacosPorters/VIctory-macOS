//! The title's allocation hooks (`__XMemAllocationHooks__`, as Grand Theft
//! Auto VI exports them) against our memory API, with a synthetic caller:
//! the stand-in title from `tests/fixtures/hle-probe` is mapped by the
//! loader, its `DllMain` fills the hooks in, and `XMemAlloc`/`XMemFree`
//! must reach them with the caller's arguments while the `…Default`
//! exports take the system path. Integration test: the hooks are looked
//! up in the mapped main image, so the process is this test's alone.
#![cfg(windows)]

use std::ffi::CString;
use std::path::{Path, PathBuf};
use std::process::Command;

use victory_loader::{ImportResolver, Provenance, Resolution, UnimplementedPolicy, prepare};
use victory_pe::ImportSymbol;
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryA};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
}

/// Builds the fixture into its own target dir (the outer cargo holds the
/// lock on the main one while tests run), in the outer one's scratch
/// folder for tests, so that it follows `CARGO_TARGET_DIR` like the rest
/// of the build.
fn build_probe() -> PathBuf {
    // A cross build runs the tests under Wine, where cargo cannot be
    // started, so the fixture can be built beforehand and named here.
    if let Some(prebuilt) = std::env::var_os("VICTORY_HLE_PROBE") {
        return PathBuf::from(prebuilt);
    }
    let manifest = workspace_root().join("tests/fixtures/hle-probe/Cargo.toml");
    let target = Path::new(env!("CARGO_TARGET_TMPDIR")).join("hle-probe");
    let status = Command::new(env!("CARGO"))
        .args(["build", "--quiet", "--manifest-path"])
        .arg(&manifest)
        .arg("--target-dir")
        .arg(&target)
        .status()
        .expect("running cargo");
    assert!(status.success(), "building hle-probe failed");
    target.join("debug").join("hle_probe.dll")
}

/// Passes named imports through to the host; everything else is missing.
/// The probe's hook exports need no import at all, so this only satisfies
/// the loader's contract that an attached image had its imports bound.
struct HostOnly;

impl ImportResolver for HostOnly {
    fn resolve(&self, dll: &str, symbol: &ImportSymbol) -> Resolution {
        let ImportSymbol::Name { name, .. } = symbol else {
            return Resolution::Missing;
        };
        let (Ok(dll), Ok(name)) = (CString::new(dll), CString::new(name.as_str())) else {
            return Resolution::Missing;
        };
        // SAFETY: NUL-terminated strings; loading system DLLs into the
        // test process.
        unsafe {
            let module = LoadLibraryA(dll.as_ptr().cast());
            if module.is_null() {
                return Resolution::Missing;
            }
            GetProcAddress(module, name.as_ptr().cast())
                .map(|f| Resolution::Found {
                    address: f as *const () as usize,
                    provenance: Provenance::Host,
                })
                .unwrap_or(Resolution::Missing)
        }
    }
}

/// Attributes of an `XMemAlloc` call: an alignment of 4096 (the field is
/// a power-of-two exponent) and a graphics memory type, as a title passes
/// them. Opaque to the hook; honoured by the default path.
const ATTRIBUTES: u64 = 12 << 24 | 1 << 29;

#[test]
fn hooks_receive_the_calls_the_defaults_bypass() {
    use victory_hle::modules::xmem::{XMemAlloc, XMemAllocDefault, XMemFree, XMemFreeDefault};

    let dll = build_probe();
    let prepared = prepare(&std::fs::read(&dll).unwrap()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let image = victory_loader::load(&prepared, dir.path()).unwrap();
    image
        .bind_imports(&HostOnly, UnimplementedPolicy::ReturnZero)
        .unwrap();
    // The probe's `DllMain` fills its allocation hooks in at attach, as a
    // title's initialized data is in place before its code runs.
    // SAFETY: the image's imports are bound and its `DllMain` calls none.
    unsafe { image.attach() }.unwrap();
    victory_hle::runtime::set_main_image(image.base());

    // The hook calls the probe has seen: allocations in the high half,
    // frees in the low; its last call's arguments, attributes over size;
    // and the frees it refused.
    let export =
        |name: &str| -> usize { image.export(name).unwrap_or_else(|| panic!("no {name}")) };
    let calls: extern "system" fn() -> u64 =
        unsafe { std::mem::transmute(export("probe_xmem_hook_calls")) };
    let last: extern "system" fn() -> u128 =
        unsafe { std::mem::transmute(export("probe_xmem_hook_last_arguments")) };
    let refused: extern "system" fn() -> u32 =
        unsafe { std::mem::transmute(export("probe_xmem_hook_mismatches")) };

    // The memory API hands the hook exactly the caller's arguments, and
    // hands back what the hook returned.
    let block = XMemAlloc(100, ATTRIBUTES);
    assert!(!block.is_null(), "the hook's block");
    assert_eq!(calls(), 1 << 32, "one allocation, no frees yet");
    assert_eq!(
        last(),
        u128::from(ATTRIBUTES) << 64 | 100,
        "the hook saw (size, attributes) as they were passed"
    );

    // A free of the same block with the same attributes matches; a free
    // of anything else is the hook's to refuse, so ownership stays with
    // it and nothing of ours is freed by mistake.
    XMemFree(block, ATTRIBUTES);
    assert_eq!(calls(), (1 << 32) | 1, "the free reached the hook");
    assert_eq!(refused(), 0, "a matching free");
    XMemFree((block as usize + 0x100) as *mut _, ATTRIBUTES);
    assert_eq!(refused(), 1, "a foreign pointer is not the hook's");
    assert_eq!(calls(), (1 << 32) | 2);

    // The `…Default` exports bypass the hooks: a system block of our own,
    // at the alignment the attributes ask for, and the hook's counters
    // stand still through both calls.
    let before = calls();
    let system = XMemAllocDefault(100, ATTRIBUTES);
    assert!(!system.is_null(), "the default path allocates");
    assert_eq!(
        system as usize % 4096,
        0,
        "the default path honours the alignment attribute"
    );
    assert_eq!(calls(), before, "XMemAllocDefault does not call the hook");
    // SAFETY: 100 bytes were allocated.
    unsafe { system.cast::<u8>().write_bytes(0x5a, 100) };
    XMemFreeDefault(system, ATTRIBUTES);
    assert_eq!(calls(), before, "XMemFreeDefault does not call the hook");
    assert_eq!(refused(), 1, "the default path frees only its own");

    // SAFETY: no title code runs and no pointer into the image is used
    // afterwards.
    unsafe { image.unload() };
}
