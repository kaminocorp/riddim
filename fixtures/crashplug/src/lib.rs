//! A "plugin" that takes the process down when scanned. It exists to prove that a crashing
//! plugin kills only the scanner child, never the host.

#[no_mangle]
pub extern "system" fn ModuleEntry(_handle: *mut std::ffi::c_void) -> bool {
    true
}

#[no_mangle]
pub extern "system" fn ModuleExit() -> bool {
    true
}

#[no_mangle]
#[allow(non_snake_case)]
pub extern "system" fn bundleEntry(_bundle: *mut std::ffi::c_void) -> bool {
    true
}

#[no_mangle]
#[allow(non_snake_case)]
pub extern "system" fn bundleExit() -> bool {
    true
}

/// How to die is chosen by `RIDDIM_CRASHPLUG_MODE`: `abort` (default), `segv`, or `hang`.
#[no_mangle]
#[allow(non_snake_case)]
pub extern "system" fn GetPluginFactory() -> *mut std::ffi::c_void {
    match std::env::var("RIDDIM_CRASHPLUG_MODE").as_deref() {
        Ok("segv") => {
            // SAFETY: deliberately not safe. Writing through a null pointer is the crash under
            // test.
            unsafe { std::ptr::write_volatile(std::ptr::null_mut::<u32>(), 0xDEAD) };
            std::ptr::null_mut()
        }
        Ok("hang") => loop {
            std::thread::sleep(std::time::Duration::from_secs(3600));
        },
        _ => std::process::abort(),
    }
}
