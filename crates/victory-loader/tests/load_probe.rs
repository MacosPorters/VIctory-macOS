//! End-to-end: prepare, load, bind and call into a stand-in title
//! (tests/fixtures/hle-probe), without any game data.
#![cfg(windows)]

use std::ffi::CString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use victory_loader::{
    ImportResolver, Provenance, Resolution, UnimplementedPolicy, WithTitleDlls, prepare,
    unimplemented_call_count,
};
use victory_pe::{ImageInfo, ImportSymbol};
use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::System::LibraryLoader::{
    GetModuleFileNameW, GetModuleHandleA, GetProcAddress, LoadLibraryA,
};
use windows_sys::Win32::System::Threading::{CreateThread, INFINITE, WaitForSingleObject};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
}

/// Builds the fixture into its own target dir (the outer cargo holds the
/// lock on the main one while tests run), in the outer one's scratch folder
/// for tests, so that it follows `CARGO_TARGET_DIR` like the rest of the
/// build.
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

extern "system" fn fake_xgameruntime_initialize() -> i32 {
    0x1234
}
/// Asks for the current thread, as our logging does: on a thread std is
/// still starting, that must not abort the process.
extern "system" fn fake_get_tick_count() -> u32 {
    let _ = std::thread::current().id();
    777
}
extern "system" fn fake_delayed() -> u32 {
    4242
}

/// The ordinal the host's kernel32 exports `GetTickCount` at, so a test
/// can rewrite the probe's import of it into that ordinal.
fn tick_ordinal() -> u16 {
    static ORDINAL: OnceLock<u16> = OnceLock::new();
    *ORDINAL.get_or_init(|| {
        export_ordinal(&host_module_file("kernel32.dll"), "GetTickCount")
            .expect("kernel32 exports GetTickCount")
    })
}

/// The file of a module already loaded, by name.
fn host_module_file(name: &str) -> PathBuf {
    let name = CString::new(name).unwrap();
    // SAFETY: a NUL-terminated name of a loaded module.
    let module = unsafe { GetModuleHandleA(name.as_ptr().cast()) };
    assert!(!module.is_null(), "{name:?} is not loaded");
    let mut path = [0u16; 260];
    // SAFETY: a valid module and a sized buffer.
    let len = unsafe { GetModuleFileNameW(module, path.as_mut_ptr(), path.len() as u32) } as usize;
    PathBuf::from(String::from_utf16_lossy(&path[..len]))
}

/// The ordinal `symbol` is exported at from the PE at `path`.
fn export_ordinal(path: &Path, symbol: &str) -> Option<u16> {
    let bytes = std::fs::read(path).ok()?;
    let info = ImageInfo::parse(&bytes).ok()?;
    info.exports
        .iter()
        .find(|e| e.name.as_deref() == Some(symbol))
        .map(|e| e.ordinal)
}

/// Rewrites the probe's import of `symbol` from `dll` into an import by
/// `ordinal`: the import's lookup-table entry gets the ordinal flag in
/// place of its hint and name. `prepare` reads that table, so the binding
/// and the probe's call then go through the ordinal path.
fn import_by_ordinal(bytes: &mut [u8], dll: &str, symbol: &str, ordinal: u16) {
    // The headers: `e_lfanew` at 0x3c, the optional header at nt + 24, the
    // import directory at optional + 120 (the second of the data
    // directories, which start at 112).
    let lfanew = u32::from_le_bytes(bytes[0x3c..0x40].try_into().unwrap()) as usize;
    let optional = lfanew + 24;
    let imports_rva = u32::from_le_bytes(bytes[optional + 120..optional + 124].try_into().unwrap());
    let info = ImageInfo::parse(bytes).expect("the probe parses");
    let file_of = |rva: u32| {
        info.rva_to_file_offset(rva)
            .expect("an rva backed by the file") as usize
    };
    let string_at = |rva: u32| {
        let at = file_of(rva);
        let end = at + bytes[at..].iter().position(|&b| b == 0).expect("a NUL");
        String::from_utf8_lossy(&bytes[at..end]).into_owned()
    };
    let mut descriptor = file_of(imports_rva);
    loop {
        let original = u32::from_le_bytes(bytes[descriptor..descriptor + 4].try_into().unwrap());
        let name_rva =
            u32::from_le_bytes(bytes[descriptor + 12..descriptor + 16].try_into().unwrap());
        let first = u32::from_le_bytes(bytes[descriptor + 16..descriptor + 20].try_into().unwrap());
        assert!(name_rva != 0, "{dll} is not imported by the probe");
        if string_at(name_rva).eq_ignore_ascii_case(dll) {
            // The parser prefers the unbound name table, falling back to
            // the IAT for old linkers, as this does.
            let table = if original != 0 { original } else { first };
            let mut entry = file_of(table);
            loop {
                let value = u64::from_le_bytes(bytes[entry..entry + 8].try_into().unwrap());
                assert!(
                    value != 0,
                    "{dll} does not import {symbol}, so it cannot become an ordinal import"
                );
                assert_eq!(value >> 63, 0, "{dll}'s entry is already an ordinal");
                let hint = (value & u64::from(u32::MAX)) as u32;
                if string_at(hint + 2) == symbol {
                    let rewritten = 0x8000_0000_0000_0000u64 | u64::from(ordinal);
                    bytes[entry..entry + 8].copy_from_slice(&rewritten.to_le_bytes());
                    return;
                }
                entry += 8;
            }
        }
        descriptor += 20;
    }
}

struct TestResolver;

impl ImportResolver for TestResolver {
    fn resolve(&self, dll: &str, symbol: &ImportSymbol) -> Resolution {
        let dll = dll.to_ascii_lowercase();
        match symbol {
            ImportSymbol::Name { name, .. } => TestResolver::by_name(&dll, name),
            ImportSymbol::Ordinal(o) => TestResolver::by_ordinal(&dll, *o),
        }
    }
}

impl TestResolver {
    fn by_name(dll: &str, name: &str) -> Resolution {
        let hle = match (dll, name) {
            ("xgameruntime.dll", "XGameRuntimeInitialize") => {
                Some(fake_xgameruntime_initialize as *const () as usize)
            }
            ("kernel32.dll", "GetTickCount") => Some(fake_get_tick_count as *const () as usize),
            ("victory-probe-delayed.dll", "VIctoryProbeDelayed") => {
                Some(fake_delayed as *const () as usize)
            }
            _ => None,
        };
        if let Some(address) = hle {
            return Resolution::Found {
                address,
                provenance: Provenance::Hle,
            };
        }
        host(dll, name)
            .map(|address| Resolution::Found {
                address,
                provenance: Provenance::Host,
            })
            .unwrap_or(Resolution::Missing)
    }

    fn by_ordinal(dll: &str, ordinal: u16) -> Resolution {
        // The import the tests rewrite to GetTickCount's own ordinal must
        // still reach our override, as it does by name.
        if dll == "kernel32.dll" && ordinal == tick_ordinal() {
            return Resolution::Found {
                address: fake_get_tick_count as *const () as usize,
                provenance: Provenance::Hle,
            };
        }
        let Ok(module) = CString::new(dll) else {
            return Resolution::Missing;
        };
        // SAFETY: a NUL-terminated DLL name; the module stays loaded.
        unsafe {
            let module = LoadLibraryA(module.as_ptr().cast());
            if module.is_null() {
                // A DLL the host does not have (a console one, say):
                // nobody can answer an ordinal of it.
                return Resolution::Missing;
            }
            GetProcAddress(module, ordinal as usize as _)
                .map(|f| Resolution::Found {
                    address: f as *const () as usize,
                    provenance: Provenance::Host,
                })
                .unwrap_or(Resolution::Missing)
        }
    }
}

fn host(dll: &str, name: &str) -> Option<usize> {
    let dll = CString::new(dll).ok()?;
    let name = CString::new(name).ok()?;
    // SAFETY: NUL-terminated strings; loading system DLLs into the test process.
    unsafe {
        let module = LoadLibraryA(dll.as_ptr().cast());
        if module.is_null() {
            return None;
        }
        GetProcAddress(module, name.as_ptr().cast()).map(|f| f as *const () as usize)
    }
}

#[test]
fn loads_binds_and_calls_probe() {
    let dll = build_probe();
    let original = std::fs::read(&dll).unwrap();
    let prepared = prepare(&original).unwrap();

    let imports: Vec<_> = prepared
        .info
        .imports
        .iter()
        .map(|m| m.dll.to_ascii_lowercase())
        .collect();
    assert!(
        imports.contains(&"xgameruntime.dll".to_owned()),
        "{imports:?}"
    );
    assert!(
        imports.contains(&"victory-probe-missing.dll".to_owned()),
        "{imports:?}"
    );
    assert!(
        prepared
            .info
            .delay_imports
            .iter()
            .any(|m| m.dll.eq_ignore_ascii_case("victory-probe-delayed.dll")),
        "fixture must delay-load victory-probe-delayed.dll"
    );

    let dir = tempfile::tempdir().unwrap();
    let image = victory_loader::load(&prepared, dir.path()).unwrap();

    // The "title" ships victory-probe-title.dll: any real DLL will do.
    let title_dir = tempfile::tempdir().unwrap();
    std::fs::copy(
        r"C:\Windows\System32\version.dll",
        title_dir.path().join("victory-probe-title.dll"),
    )
    .unwrap();
    let dlls =
        victory_loader::load_title_dlls(image.info(), title_dir.path(), dir.path(), &|_| true)
            .unwrap();
    let names: Vec<_> = dlls.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["victory-probe-title.dll"]);
    let resolver = WithTitleDlls {
        inner: &TestResolver,
        dlls: &dlls,
    };
    for (_, dll) in &dlls {
        dll.bind_imports(&resolver, UnimplementedPolicy::ReturnZero)
            .unwrap();
        // SAFETY: its imports are bound, and it depends on no other title DLL.
        unsafe { dll.attach() }.unwrap();
    }
    let report = image
        .bind_imports(&resolver, UnimplementedPolicy::ReturnZero)
        .unwrap();
    assert_eq!(report.title, 1);

    assert_eq!(report.hle, 3);
    assert!(report.host > 0);
    assert_eq!(report.missing.len(), 1);
    assert_eq!(report.missing[0].symbol, "VIctoryProbeNotImplemented");

    let call = |name: &str| -> usize { image.export(name).unwrap_or_else(|| panic!("{name}")) };
    // SAFETY: the exports have the signatures declared in the fixture and
    // every import they use has been bound.
    unsafe {
        let f: extern "system" fn() -> i32 = std::mem::transmute(call("probe_xgameruntime_init"));
        assert_eq!(f(), 0x1234, "console API must reach our implementation");

        let f: extern "system" fn() -> u32 = std::mem::transmute(call("probe_tick"));
        assert_eq!(f(), 777, "host API must be overridable");

        let f: extern "system" fn() -> u32 = std::mem::transmute(call("probe_pid"));
        assert_eq!(f(), std::process::id(), "host API must pass through");

        let f: extern "system" fn() -> u32 = std::mem::transmute(call("probe_title"));
        assert_ne!(f(), 0, "title DLL export must be bound");

        let f: extern "system" fn() -> u32 = std::mem::transmute(call("probe_delayed"));
        assert_eq!(f(), 4242, "delay-loaded API must be bound");

        let before = unimplemented_call_count();
        let f: extern "system" fn() -> u64 = std::mem::transmute(call("probe_missing"));
        assert_eq!(f(), 0);
        assert_eq!(unimplemented_call_count(), before + 1);

        // Once attached, every thread start reaches its TLS callback and
        // DllMain (other tests may start threads too, hence at least one).
        assert!(
            image
                .info()
                .tls
                .as_ref()
                .is_some_and(|t| !t.callback_rvas.is_empty())
        );
        image.attach().unwrap();
        let f: extern "system" fn() -> u64 = std::mem::transmute(call("probe_thread_starts"));
        let before = f();
        // A thread std starts, as our runtime's workers are: notified
        // before std sets it up, which must still find its state pristine.
        let worker = std::thread::Builder::new()
            .name("probe-worker".into())
            .spawn(|| std::thread::current().name().map(str::to_owned))
            .unwrap();
        assert_eq!(worker.join().unwrap().as_deref(), Some("probe-worker"));
        let mid = f();
        assert!(mid >> 32 > before >> 32 && mid as u32 > before as u32);
        let before = mid;
        // Starting in title code, as a title's threads do; the extra
        // argument is ignored.
        let start: unsafe extern "system" fn(*mut std::ffi::c_void) -> u32 =
            std::mem::transmute(call("probe_pid"));
        let thread = CreateThread(
            std::ptr::null(),
            0,
            Some(start),
            std::ptr::null(),
            0,
            std::ptr::null_mut(),
        );
        assert!(!thread.is_null());
        WaitForSingleObject(thread, INFINITE);
        CloseHandle(thread);
        let after = f();
        assert!(
            after >> 32 > before >> 32,
            "TLS callback: {before:#x} → {after:#x}"
        );
        assert!(
            after as u32 > before as u32,
            "DllMain: {before:#x} → {after:#x}"
        );
        // It has static TLS, so Windows would refuse too.
        assert_eq!(
            victory_loader::disable_thread_calls(image.base()),
            Some(false)
        );
        assert_eq!(victory_loader::disable_thread_calls(1), None);

        // `FreeLibrary`'s notice, as a failed late load sends it.
        let f: extern "system" fn() -> u32 = std::mem::transmute(call("probe_detaches"));
        assert_eq!(f(), 0);
        image.detach();
        assert_eq!(f(), 1);
        image.unload();
    }
}

#[test]
fn maps_a_title_dll_the_title_loads_itself() {
    let title_dir = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    // Any real DLL will do; it imports only host DLLs, which are not in the folder.
    std::fs::copy(
        r"C:\Windows\System32\version.dll",
        title_dir.path().join("Late.dll"),
    )
    .unwrap();
    let load = |name: &str, wanted: &dyn Fn(&str) -> bool| {
        victory_loader::load_title_dll(name, title_dir.path(), cache.path(), wanted).unwrap()
    };
    let dlls = load("LATE.DLL", &|_| true);
    let names: Vec<_> = dlls.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["late.dll"]);
    assert!(dlls[0].1.export("GetFileVersionInfoW").is_some());
    assert!(load("absent.dll", &|_| true).is_empty());
    assert!(load("late.dll", &|_| false).is_empty());
}

/// A probe whose imports of `GetTickCount` and of the title DLL's
/// `GetFileVersionInfoSizeW` are rewritten to those functions' own
/// ordinals: the loader parses ordinals, the resolver answers them
/// (our override for the one, the title DLL for the other), and the
/// probe's calls land where they did by name.
#[test]
fn ordinal_imports_bind_and_call() {
    let dll = build_probe();
    let mut original = std::fs::read(&dll).unwrap();
    let title_dir = tempfile::tempdir().unwrap();
    let title_dll = title_dir.path().join("victory-probe-title.dll");
    std::fs::copy(r"C:\Windows\System32\version.dll", &title_dll).unwrap();
    let title_ordinal =
        export_ordinal(&title_dll, "GetFileVersionInfoSizeW").expect("version.dll exports it");
    import_by_ordinal(
        &mut original,
        "kernel32.dll",
        "GetTickCount",
        tick_ordinal(),
    );
    import_by_ordinal(
        &mut original,
        "victory-probe-title.dll",
        "GetFileVersionInfoSizeW",
        title_ordinal,
    );
    let prepared = prepare(&original).unwrap();
    // The parser read both entries as ordinals.
    let kernel32 = prepared
        .info
        .imports
        .iter()
        .find(|m| m.dll.eq_ignore_ascii_case("kernel32.dll"))
        .unwrap();
    let tick = kernel32
        .entries
        .iter()
        .find(|e| matches!(e.symbol, ImportSymbol::Ordinal(o) if o == tick_ordinal()))
        .expect("GetTickCount became an ordinal import");

    let dir = tempfile::tempdir().unwrap();
    let image = victory_loader::load(&prepared, dir.path()).unwrap();
    let dlls =
        victory_loader::load_title_dlls(image.info(), title_dir.path(), dir.path(), &|_| true)
            .unwrap();
    let resolver = WithTitleDlls {
        inner: &TestResolver,
        dlls: &dlls,
    };
    let report = image
        .bind_imports(&resolver, UnimplementedPolicy::ReturnZero)
        .unwrap();
    // Our overrides (the tick by ordinal, the console API and the delayed
    // one by name) and the title DLL's export, reached through its ordinal;
    // the one import nobody implements is reported as ever.
    assert_eq!(report.hle, 3, "{:?}", report.missing);
    assert_eq!(report.title, 1, "{:?}", report.missing);
    assert_eq!(
        report.missing.len(),
        1,
        "only the by-design missing import: {:?}",
        report.missing
    );
    assert_eq!(report.missing[0].symbol, "VIctoryProbeNotImplemented");
    // The IAT slot the parser paired with the ordinal entry was written.
    let slot = image.base() + tick.iat_slot_rva as usize;
    let fake: extern "system" fn() -> u32 = fake_get_tick_count;
    // SAFETY: the slot the parser reported inside the image.
    unsafe {
        assert_eq!(
            (slot as *const u64).read_volatile(),
            fake as *const () as u64
        );
    }

    let call = |name: &str| -> usize { image.export(name).unwrap_or_else(|| panic!("{name}")) };
    // SAFETY: the exports have the signatures declared in the fixture and
    // every import they use has been bound.
    unsafe {
        let f: extern "system" fn() -> u32 = std::mem::transmute(call("probe_tick"));
        assert_eq!(f(), 777, "an ordinal import must reach our override");

        let f: extern "system" fn() -> u32 = std::mem::transmute(call("probe_title"));
        assert_ne!(f(), 0, "the title DLL's ordinal must be bound");
        // SAFETY: no title code runs and no pointer into the image is used
        // afterwards.
        image.unload();
    }
}

/// A probe whose console API import is rewritten to an ordinal nothing
/// implements: the binding reports `#n` (the registry answers no ordinal
/// names), and the call lands in the reporting stub, which says exactly
/// what was called. This pins how an ordinal import of a console DLL is
/// isolated today.
#[test]
fn an_ordinal_console_import_lands_in_the_reporting_stub() {
    const NOBODY_IMPLEMENTS: u16 = 0x1234;
    let dll = build_probe();
    let mut original = std::fs::read(&dll).unwrap();
    import_by_ordinal(
        &mut original,
        "xgameruntime.dll",
        "XGameRuntimeInitialize",
        NOBODY_IMPLEMENTS,
    );
    let prepared = prepare(&original).unwrap();
    assert!(
        prepared
            .info
            .imports
            .iter()
            .find(|m| m.dll.eq_ignore_ascii_case("xgameruntime.dll"))
            .unwrap()
            .entries
            .iter()
            .any(|e| matches!(e.symbol, ImportSymbol::Ordinal(o) if o == NOBODY_IMPLEMENTS))
    );

    let dir = tempfile::tempdir().unwrap();
    let image = victory_loader::load(&prepared, dir.path()).unwrap();
    let report = image
        .bind_imports(&TestResolver, UnimplementedPolicy::ReturnZero)
        .unwrap();
    // The by-design missing import and the unbound title DLL's too, plus
    // the ordinal console one, which is isolated as `#n`: the registry
    // answers no ordinal names.
    assert_eq!(report.missing.len(), 3, "{:?}", report.missing);
    let ordinal = report
        .missing
        .iter()
        .find(|m| m.dll == "xgameruntime.dll")
        .expect("the ordinal import is reported");
    assert_eq!(ordinal.symbol, "#4660", "{:?}", report.missing);

    let call = |name: &str| -> usize { image.export(name).unwrap_or_else(|| panic!("{name}")) };
    // SAFETY: the export has the signature declared in the fixture; the
    // import it uses is bound to the reporting stub.
    unsafe {
        let before = unimplemented_call_count();
        let f: extern "system" fn() -> i32 = std::mem::transmute(call("probe_xgameruntime_init"));
        assert_eq!(f(), 0);
        assert_eq!(unimplemented_call_count(), before + 1);
        image.unload();
    }
}
