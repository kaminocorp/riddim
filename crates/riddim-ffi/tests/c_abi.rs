//! Builds a C program against the checked-in header and the real dynamic library, and runs it.
//! If this passes, a client written in another language can do what the C program did.

use std::path::Path;
use std::process::Command;

#[test]
fn a_c_program_can_drive_the_engine() {
    let compiler = std::env::var("CC").unwrap_or_else(|_| "cc".to_string());
    if Command::new(&compiler).arg("--version").output().is_err() {
        eprintln!("no C compiler ({compiler}) on this machine: the C ABI smoke test was skipped");
        return;
    }
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let library = riddim_test_support::ffi_library();
    let library_dir = library.parent().unwrap();
    let work = std::env::temp_dir().join(format!("riddim-ffi-{}", std::process::id()));
    std::fs::create_dir_all(&work).unwrap();
    let binary = work.join("smoke");

    let compile = Command::new(&compiler)
        .arg("-std=c99")
        .args(["-Wall", "-Wextra", "-Werror", "-pedantic"])
        .arg("-I")
        .arg(crate_dir.join("include"))
        .arg(crate_dir.join("tests/smoke.c"))
        .arg("-L")
        .arg(library_dir)
        .arg("-lriddim_ffi")
        .arg("-o")
        .arg(&binary)
        .output()
        .expect("running the C compiler");
    assert!(
        compile.status.success(),
        "the C program did not compile against riddim.h:\n{}",
        String::from_utf8_lossy(&compile.stderr)
    );

    let run = Command::new(&binary)
        .arg(&work)
        .env("LD_LIBRARY_PATH", library_dir)
        .env("DYLD_LIBRARY_PATH", library_dir)
        .output()
        .expect("running the C program");
    assert!(
        run.status.success(),
        "the C program failed ({:?}):\n{}",
        run.status.code(),
        String::from_utf8_lossy(&run.stderr)
    );
    assert!(String::from_utf8_lossy(&run.stdout).contains("c abi smoke test passed"));
    std::fs::remove_dir_all(&work).ok();
}
