//! Proof that a RealtimeSanitizer run is not vacuous.
//!
//! When the suite is built for RTSan (`scripts/rtsan.sh`), this test re-runs itself as a child
//! process that deliberately breaks the rules inside a real-time section, and checks that the
//! sanitizer stopped it. If the sanitizer were not actually linked in, the children would exit
//! cleanly and this test would fail, so "the RTSan run was green" always means "RTSan was on".
//!
//! In ordinary builds there is nothing to check and the test says so.

use std::process::Command;
use std::sync::Mutex;

const CHILD: &str = "RIDDIM_RTSAN_CHILD";

/// Runs in the child: does the forbidden thing named by the environment variable.
fn misbehave(kind: &str) {
    match kind {
        "alloc" => riddim_rt::guard::rt_scope(|| {
            let boxed = std::hint::black_box(Box::new([0u8; 4096]));
            drop(boxed);
        }),
        "lock" => {
            let mutex = Mutex::new(0u32);
            // Hold the lock on another thread so taking it here has to block in the kernel.
            std::thread::scope(|scope| {
                let guard = mutex.lock().unwrap();
                let waiter = scope.spawn(|| {
                    riddim_rt::guard::rt_scope(|| {
                        *mutex.lock().unwrap() += 1;
                    })
                });
                std::thread::sleep(std::time::Duration::from_millis(200));
                drop(guard);
                waiter.join().unwrap();
            });
        }
        "io" => riddim_rt::guard::rt_scope(|| {
            let _ = std::hint::black_box(std::fs::File::open("/dev/null"));
        }),
        // The same things are fine where the checks are lifted (third-party plugin calls).
        "permitted" => riddim_rt::guard::rt_scope(|| {
            riddim_rt::guard::permit(|| {
                drop(std::hint::black_box(vec![1u8; 4096]));
                let _ = std::hint::black_box(std::fs::File::open("/dev/null"));
            })
        }),
        other => panic!("unknown child mode {other}"),
    }
}

#[test]
fn the_sanitizer_stops_real_time_violations() {
    if let Ok(kind) = std::env::var(CHILD) {
        misbehave(&kind);
        return;
    }
    if !riddim_rt::guard::rtsan_feature() || option_env!("RTSAN_ENABLE").is_none() {
        eprintln!("not a RealtimeSanitizer build: nothing to check");
        return;
    }
    let run = |kind: &str| {
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "the_sanitizer_stops_real_time_violations",
                "--nocapture",
            ])
            .env(CHILD, kind)
            .output()
            .unwrap()
    };
    for kind in ["alloc", "lock", "io"] {
        let output = run(kind);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !output.status.success() && stderr.contains("RealtimeSanitizer"),
            "RealtimeSanitizer did not stop a deliberate '{kind}' violation; is it linked in?\n{stderr}"
        );
    }
    let permitted = run("permitted");
    assert!(
        permitted.status.success(),
        "permit() must suspend the sanitizer:\n{}",
        String::from_utf8_lossy(&permitted.stderr)
    );
}
