//! VST3 hosting, written directly on the `vst3` bindings.
//!
//! Status: exercised on Linux against Riddim's own test plugin (`fixtures/testplug`). The
//! macOS-specific parts (CFBundle loading, `NSView` editors) are **unverified on macOS**, and no
//! third-party plugin has been run through this code yet.

// The binding typedefs differ between platforms, so casts that look redundant here are not.
#![allow(clippy::unnecessary_cast)]

mod com;
mod instance;
mod module;
mod processor;
mod strings;

use std::path::Path;

pub use instance::Vst3Instance;
pub use module::{default_search_paths, find_bundles};
pub use processor::Vst3Processor;

use crate::descriptor::PluginDescriptor;
use crate::error::{PluginError, Result};

/// Loads the module at `path` in this process and lists its audio plugins.
///
/// This runs plugin code. Call it from the scanner child process, not from the host.
pub fn scan_bundle(path: &Path) -> Result<Vec<PluginDescriptor>> {
    let module = module::Module::load(path)?;
    Ok(module.classes().into_iter().map(|c| c.descriptor).collect())
}

/// Creates an instance of the plugin described by `descriptor`.
///
/// `host_name` is what the plugin sees as the host application's name.
pub fn instantiate(descriptor: &PluginDescriptor, host_name: &str) -> Result<Vst3Instance> {
    let path = descriptor
        .path
        .as_deref()
        .ok_or_else(|| PluginError::Load("a VST3 descriptor needs a bundle path".into()))?;
    let module = module::Module::load(path)?;
    let class = module
        .classes()
        .into_iter()
        .find(|c| c.descriptor.id == descriptor.id)
        .ok_or_else(|| PluginError::NotFound(descriptor.id.to_string()))?;
    Vst3Instance::new(module, class, host_name)
}
