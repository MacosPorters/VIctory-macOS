#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::process::Command;

use windows_sys::Win32::Foundation::EXCEPTION_BREAKPOINT;
use windows_sys::Win32::System::Diagnostics::Debug::{
    AddVectoredExceptionHandler, EXCEPTION_CONTINUE_EXECUTION, EXCEPTION_CONTINUE_SEARCH,
    EXCEPTION_POINTERS, RaiseException,
};

const TEST_LOG: &str = "VICTORY_TEST_CRASH_LOG";
const TITLE_FAULT_LOG: &str = "VICTORY_TEST_TITLE_FAULT_LOG";

#[test]
fn first_chance_breakpoint_logging() {
    if let Some(log) = std::env::var_os(TEST_LOG) {
        victory_loader::log_crashes_to(PathBuf::from(log));
        victory_loader::note_title_output("synthetic title output");
        victory_loader::report_crashes();

        unsafe extern "system" fn handle_breakpoint(info: *mut EXCEPTION_POINTERS) -> i32 {
            // SAFETY: the system passes valid exception pointers.
            if unsafe { (*(*info).ExceptionRecord).ExceptionCode } == EXCEPTION_BREAKPOINT {
                EXCEPTION_CONTINUE_EXECUTION
            } else {
                EXCEPTION_CONTINUE_SEARCH
            }
        }

        // SAFETY: register after VIctory's vectored handler. This synthetic
        // breakpoint is continuable and the handler needs no extra data.
        unsafe {
            assert!(!AddVectoredExceptionHandler(0, Some(handle_breakpoint)).is_null());
            RaiseException(EXCEPTION_BREAKPOINT as u32, 0, 0, std::ptr::null());
        }
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    for opt_in in [false, true] {
        let log = dir.path().join(format!("breakpoint-{opt_in}.log"));
        let mut child = Command::new(std::env::current_exe().unwrap());
        child
            .args(["--exact", "first_chance_breakpoint_logging", "--nocapture"])
            .env(TEST_LOG, &log)
            .env_remove("VICTORY_CRASH_BREAKPOINTS");
        if opt_in {
            child.env("VICTORY_CRASH_BREAKPOINTS", "1");
        }
        let output = child.output().unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{}\n{stderr}", output.status);
        assert!(stderr.contains("first-chance exception 0x80000003"));

        if cfg!(feature = "diagnostics") && opt_in {
            let report = std::fs::read_to_string(&log).unwrap();
            assert!(report.contains("first-chance exception 0x80000003 at "));
            assert!(report.contains("  #0 "));
            assert!(report.contains("  title said: synthetic title output"));
            assert!(!report.contains("unhandled exception"));
        } else {
            assert!(!log.exists());
        }
    }
}

/// A fault in mapped title code (the probe writing through a null
/// pointer) ends the process through the unhandled-exception filter, and
/// the report names the mapped image and walks its frames: the Windows
/// loader registered the prepared copy's unwind tables, so the stack shows
/// the fixture's function, not just the raw address. Exception handling
/// for mapped title code, exercised without any game data.
#[test]
fn a_fault_in_mapped_title_code_is_reported_with_its_image() {
    if let Some(log) = std::env::var_os(TITLE_FAULT_LOG) {
        victory_loader::log_crashes_to(PathBuf::from(log));
        victory_loader::report_crashes();
        let image = mapped_probe();
        // The probe's faulting export: imports are not bound, as the
        // fixture's faulting code calls none.
        let fault: unsafe extern "system" fn() =
            unsafe { std::mem::transmute(image.export("probe_fault").expect("probe_fault")) };
        // SAFETY: the export has the signature declared in the fixture.
        unsafe { fault() };
        unreachable!("the fault ends the process before this");
    }

    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("title-fault.log");
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "a_fault_in_mapped_title_code_is_reported_with_its_image",
            // The reports the handlers print while the test dies would stay
            // in the harness's capture otherwise.
            "--nocapture",
        ])
        .env(TITLE_FAULT_LOG, &log)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "the write through null must end the process"
    );
    // The report on the way out: the exception, the faulting instruction
    // in the mapped copy, and the frames walked through its unwind data.
    assert!(
        stderr.contains("unhandled exception 0xc0000005"),
        "{stderr}"
    );
    assert!(
        stderr.contains("victory-image-"),
        "the faulting instruction is in the mapped copy\n{stderr}"
    );
    let report = std::fs::read_to_string(&log).expect("the unhandled report is kept");
    assert!(
        report.contains("unhandled exception 0xc0000005 at victory-image-")
            && report.contains("(write 0x0"),
        "{report}"
    );
    assert!(
        report.contains("  #0  victory-image-"),
        "the first frame is the fixture's own function\n{report}"
    );
}

/// The probe mapped as the title's main image, without binding or
/// attaching: `probe_fault` needs neither.
fn mapped_probe() -> victory_loader::LoadedImage {
    let dll = build_probe();
    let original = std::fs::read(&dll).unwrap();
    let prepared = victory_loader::prepare(&original).unwrap();
    let dir = tempfile::tempdir().unwrap();
    victory_loader::load(&prepared, dir.path()).unwrap()
}

/// As `load_probe.rs` builds it: the fixture from a normal cargo build, or
/// the prebuilt one a Wine run names.
fn build_probe() -> PathBuf {
    if let Some(prebuilt) = std::env::var_os("VICTORY_HLE_PROBE") {
        return PathBuf::from(prebuilt);
    }
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("tests/fixtures/hle-probe/Cargo.toml");
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
