//! Editor windows, opened for real on macOS.
//!
//! AppKit insists on the process's main thread, so this test brings its own `main` instead of
//! using the test harness (which runs tests on worker threads). On other platforms it does
//! nothing.

#[cfg(target_os = "macos")]
fn main() {
    use std::time::Duration;

    use riddim_plugin::editor_window::{init_application, pump_events, EditorWindow};
    use riddim_plugin::format::au;
    use riddim_plugin::host::instantiate;
    use riddim_plugin::EditorSize;

    if std::env::var_os("RIDDIM_SKIP_WINDOW_TESTS").is_some() {
        println!("mac_editor: skipped (RIDDIM_SKIP_WINDOW_TESTS is set)");
        return;
    }
    init_application().expect("NSApplication");

    // Apple's units have no custom Cocoa view, so these exercise the generic-view path; the
    // window and resize plumbing is the same for any view.
    for uid in ["aufx:dely:appl", "aumu:dls :appl"] {
        let descriptor = au::scan()
            .expect("AU scan")
            .into_iter()
            .find(|d| d.id.uid == uid)
            .unwrap_or_else(|| panic!("{uid} is not registered"));
        let mut instance = instantiate(&descriptor).expect("instantiate");
        assert!(instance.has_editor());
        let mut events = Vec::new();
        for round in 0..10 {
            let mut window =
                EditorWindow::open(instance.as_mut(), &descriptor.name).expect("open editor");
            assert!(instance.is_editor_open());
            let opened = window.size();
            assert!(
                opened.width > 0 && opened.height > 0,
                "editor opened with no size"
            );
            for _ in 0..3 {
                pump_events(Duration::from_millis(10));
                events.clear();
                instance.poll_events(&mut events);
                for event in &events {
                    window.handle_event(instance.as_mut(), event);
                }
                assert!(
                    window.sync(instance.as_mut()),
                    "the window closed by itself"
                );
            }
            if instance.editor_resizable() {
                let wanted = EditorSize {
                    width: opened.width + 80,
                    height: opened.height + 50,
                };
                let got = window.request_size(instance.as_mut(), wanted);
                assert!(got.width > 0 && got.height > 0);
                window.request_size(instance.as_mut(), opened);
            }
            window.close(instance.as_mut());
            assert!(!instance.is_editor_open());
            pump_events(Duration::from_millis(10));
            println!(
                "mac_editor: {} round {} ok ({}x{})",
                descriptor.name,
                round + 1,
                opened.width,
                opened.height
            );
        }
    }
    println!("mac_editor: all editor cycles completed");
}

#[cfg(not(target_os = "macos"))]
fn main() {
    println!("mac_editor: nothing to do off macOS");
}
