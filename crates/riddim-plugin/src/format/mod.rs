//! Plugin formats. Each implements [`PluginInstance`](crate::PluginInstance) and offers an
//! in-process `scan_*` function for the scanner binary to call.

pub mod clap;
pub mod vst3;

#[cfg(target_os = "macos")]
pub mod au;
