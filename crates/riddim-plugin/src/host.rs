//! Format dispatch: turn a descriptor into a live instance.

use crate::descriptor::{PluginDescriptor, PluginFormat};
use crate::error::{PluginError, Result};
use crate::format;
use crate::instance::PluginInstance;

/// The name plugins see as their host application.
pub const DEFAULT_HOST_NAME: &str = "Riddim";

/// Creates an instance of the described plugin on the calling thread.
///
/// The calling thread becomes the instance's home: on macOS it must be the main thread if an
/// editor will ever be opened.
pub fn instantiate(descriptor: &PluginDescriptor) -> Result<Box<dyn PluginInstance>> {
    match descriptor.id.format {
        PluginFormat::Vst3 => {
            Ok(Box::new(format::vst3::instantiate(descriptor, DEFAULT_HOST_NAME)?))
        }
        #[cfg(target_os = "macos")]
        PluginFormat::AudioUnit => Ok(Box::new(format::au::instantiate(descriptor)?)),
        #[cfg(not(target_os = "macos"))]
        PluginFormat::AudioUnit => Err(PluginError::Unsupported("Audio Units need macOS")),
        PluginFormat::Clap => Err(PluginError::Unsupported("CLAP hosting arrives after v1")),
        PluginFormat::Internal => {
            Err(PluginError::Unsupported("internal processors are created by the engine"))
        }
    }
}
