//! Windows for plugin editors: the real thing on macOS, a stand-in elsewhere so the commands
//! read the same on every platform.

use std::time::Duration;

use riddim_plugin::PluginInstance;

#[cfg(target_os = "macos")]
mod platform {
    use super::*;
    pub use riddim_plugin::editor_window::EditorWindow;

    pub fn init() -> Result<(), String> {
        riddim_plugin::editor_window::init_application().map_err(|e| e.to_string())
    }

    pub fn pump(wait: Duration) {
        riddim_plugin::editor_window::pump_events(wait);
    }

    pub fn open(instance: &mut dyn PluginInstance, title: &str) -> Result<EditorWindow, String> {
        EditorWindow::open(instance, title).map_err(|e| e.to_string())
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    use super::*;
    use riddim_plugin::PluginEvent;

    pub struct EditorWindow;

    impl EditorWindow {
        pub fn handle_event(&mut self, _instance: &mut dyn PluginInstance, _event: &PluginEvent) {}

        pub fn sync(&mut self, _instance: &mut dyn PluginInstance) -> bool {
            true
        }

        pub fn close(self, _instance: &mut dyn PluginInstance) {}
    }

    pub fn init() -> Result<(), String> {
        Ok(())
    }

    pub fn pump(wait: Duration) {
        std::thread::sleep(wait);
    }

    pub fn open(_instance: &mut dyn PluginInstance, _title: &str) -> Result<EditorWindow, String> {
        Err("plugin editor windows are only implemented on macOS".into())
    }
}

pub use platform::{init, open, pump, EditorWindow};
