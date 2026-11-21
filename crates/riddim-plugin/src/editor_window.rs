//! A native window that hosts a plugin's editor (macOS).
//!
//! Riddim draws nothing itself; this is only the frame around the plugin's own view, plus the
//! size negotiation between the two. Everything here must run on the main thread.
//!
//! **UNVERIFIED ON MACOS**: type-checked from Linux, not yet run on a Mac.

use std::ptr::NonNull;
use std::time::Duration;

use objc2::rc::Retained;
use objc2::MainThreadMarker;
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSEventMask, NSView,
    NSWindow, NSWindowStyleMask,
};
use objc2_foundation::{NSDate, NSDefaultRunLoopMode, NSPoint, NSRect, NSSize, NSString};

use crate::error::{PluginError, Result};
use crate::instance::{EditorSize, ParentWindow, PluginEvent, PluginInstance};

/// A window with one plugin editor in it.
pub struct EditorWindow {
    window: Retained<NSWindow>,
    size: EditorSize,
}

fn ns_size(size: EditorSize) -> NSSize {
    NSSize::new(size.width as f64, size.height as f64)
}

impl EditorWindow {
    /// Creates a window, opens `instance`'s editor in it, and shows it.
    pub fn open(instance: &mut dyn PluginInstance, title: &str) -> Result<EditorWindow> {
        let mtm = MainThreadMarker::new().ok_or(PluginError::Unsupported(
            "editor windows need the main thread",
        ))?;
        let style = NSWindowStyleMask::Titled
            | NSWindowStyleMask::Closable
            | NSWindowStyleMask::Miniaturizable;
        let provisional = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(640.0, 480.0));
        // SAFETY: a plain NSWindow created on the main thread with valid arguments.
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                mtm.alloc(),
                provisional,
                style,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        // Closing must only hide the window: the plugin's view is still attached, and we take
        // it down in an orderly way in `close`.
        // SAFETY: plain property set on a window we own.
        unsafe { window.setReleasedWhenClosed(false) };
        window.setTitle(&NSString::from_str(title));

        let container: Retained<NSView> = window
            .contentView()
            .ok_or_else(|| PluginError::Plugin("the window has no content view".into()))?;
        let parent = ParentWindow::AppKit(NonNull::from(&*container).cast());
        let size = instance.open_editor(parent)?;

        window.setContentSize(ns_size(size));
        if instance.editor_resizable() {
            window.setStyleMask(style | NSWindowStyleMask::Resizable);
        }
        window.center();
        window.makeKeyAndOrderFront(None);
        Ok(EditorWindow { window, size })
    }

    /// Size of the editor area in points.
    pub fn size(&self) -> EditorSize {
        self.size
    }

    fn content_size(&self) -> EditorSize {
        let frame = self.window.contentRectForFrameRect(self.window.frame());
        EditorSize {
            width: frame.size.width.max(0.0) as u32,
            height: frame.size.height.max(0.0) as u32,
        }
    }

    /// Reacts to a notification from the plugin. Only resize requests concern the window.
    pub fn handle_event(&mut self, instance: &mut dyn PluginInstance, event: &PluginEvent) {
        if let PluginEvent::EditorResizeRequest { width, height } = *event {
            let wanted = EditorSize { width, height };
            if wanted != self.size && wanted.width > 0 && wanted.height > 0 {
                self.window.setContentSize(ns_size(wanted));
                // The plugin asked for this size; tell it the window followed.
                self.size = instance.set_editor_size(wanted).unwrap_or(wanted);
            }
        }
    }

    /// Carries a user resize of the window through to the plugin. Returns false once the user
    /// has closed the window.
    pub fn sync(&mut self, instance: &mut dyn PluginInstance) -> bool {
        if !self.window.isVisible() {
            return false;
        }
        let current = self.content_size();
        if current != self.size {
            // Ask the plugin; it may snap to its own constraints, and the window adopts
            // whatever it accepted.
            let accepted = instance.set_editor_size(current).unwrap_or(self.size);
            if accepted != current {
                self.window.setContentSize(ns_size(accepted));
            }
            self.size = accepted;
        }
        true
    }

    /// Programmatically resizes the window as a user drag would.
    pub fn request_size(
        &mut self,
        instance: &mut dyn PluginInstance,
        size: EditorSize,
    ) -> EditorSize {
        self.window.setContentSize(ns_size(size));
        self.sync(instance);
        self.size
    }

    /// Detaches the editor from the plugin and closes the window.
    pub fn close(self, instance: &mut dyn PluginInstance) {
        instance.close_editor();
        self.window.close();
    }
}

/// Prepares the process to show windows: a regular (Dock-visible) application, activated.
pub fn init_application() -> Result<()> {
    let mtm = MainThreadMarker::new().ok_or(PluginError::Unsupported(
        "the application must start on the main thread",
    ))?;
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    app.finishLaunching();
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);
    Ok(())
}

/// Dispatches pending window-system events for up to `wait`, then returns. Hosts without their
/// own event loop (the CLI, tests) call this in a loop.
pub fn pump_events(wait: Duration) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let app = NSApplication::sharedApplication(mtm);
    let deadline = NSDate::dateWithTimeIntervalSinceNow(wait.as_secs_f64());
    loop {
        // SAFETY: the standard manual event loop; the mode constant is a valid static.
        let event = unsafe {
            app.nextEventMatchingMask_untilDate_inMode_dequeue(
                NSEventMask::Any,
                Some(&deadline),
                NSDefaultRunLoopMode,
                true,
            )
        };
        match event {
            Some(event) => app.sendEvent(&event),
            None => break,
        }
    }
}
