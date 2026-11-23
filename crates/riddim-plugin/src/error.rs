//! Errors from the hosting layer.

use std::fmt;

pub type Result<T> = std::result::Result<T, PluginError>;

/// Why a hosting operation failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginError {
    /// No plugin matches the requested identity.
    NotFound(String),
    /// The plugin's binary could not be loaded or did not look like a plugin.
    Load(String),
    /// The plugin refused a call or returned an error code.
    Plugin(String),
    /// The host does not support this on the current platform or format.
    Unsupported(&'static str),
    /// A state blob was malformed or belongs to a different plugin.
    State(String),
    /// The instance is in the wrong lifecycle state for the call.
    InvalidState(&'static str),
    /// The scanner child process failed.
    Scan(String),
    /// An I/O error, flattened to text.
    Io(String),
}

impl fmt::Display for PluginError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PluginError::NotFound(what) => write!(f, "plugin not found: {what}"),
            PluginError::Load(why) => write!(f, "could not load plugin: {why}"),
            PluginError::Plugin(why) => write!(f, "plugin error: {why}"),
            PluginError::Unsupported(what) => write!(f, "not supported: {what}"),
            PluginError::State(why) => write!(f, "bad plugin state: {why}"),
            PluginError::InvalidState(why) => write!(f, "invalid lifecycle state: {why}"),
            PluginError::Scan(why) => write!(f, "scan failed: {why}"),
            PluginError::Io(why) => write!(f, "i/o error: {why}"),
        }
    }
}

impl std::error::Error for PluginError {}

impl From<std::io::Error> for PluginError {
    fn from(e: std::io::Error) -> Self {
        PluginError::Io(e.to_string())
    }
}
