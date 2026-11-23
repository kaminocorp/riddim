//! Audio Unit hosting through the AudioToolbox C API.
//!
//! One code path serves both generations of Audio Unit: version 2 units natively, and version 3
//! units through the bridge AudioToolbox provides for hosts that use the C API. (A version 3
//! unit's custom view controller is not requested yet; such units get the generic view.)
//!
//! **UNVERIFIED ON MACOS.** This module is written against the objc2 bindings and type-checked
//! for `aarch64-apple-darwin` from Linux. Nothing in it has been run on a Mac except where the
//! macOS CI job's log says so, and no third-party Audio Unit has been through it at all.

mod instance;
mod processor;
mod sys;
mod view;

use std::ptr::NonNull;

use objc2_audio_toolbox::{
    AudioComponent, AudioComponentCopyName, AudioComponentDescription, AudioComponentFindNext,
    AudioComponentGetDescription, AudioComponentGetVersion,
};
use objc2_core_foundation::{CFRetained, CFString};

pub use instance::AuInstance;
pub use processor::AuProcessor;

use crate::descriptor::{PluginDescriptor, PluginFormat, PluginId, PluginKind};
use crate::error::{PluginError, Result};
use sys::{fourcc_from_str, fourcc_to_string, NO_ERR};

fn kind_of(component_type: u32) -> Option<PluginKind> {
    match component_type {
        sys::TYPE_MUSIC_DEVICE => Some(PluginKind::Instrument),
        sys::TYPE_EFFECT => Some(PluginKind::Effect),
        sys::TYPE_MUSIC_EFFECT => Some(PluginKind::MidiEffect),
        sys::TYPE_GENERATOR => Some(PluginKind::Generator),
        _ => None,
    }
}

fn any_description() -> AudioComponentDescription {
    AudioComponentDescription {
        componentType: 0,
        componentSubType: 0,
        componentManufacturer: 0,
        componentFlags: 0,
        componentFlagsMask: 0,
    }
}

fn describe(component: AudioComponent) -> Option<PluginDescriptor> {
    let mut desc = any_description();
    // SAFETY: valid component and out-pointers throughout.
    unsafe {
        if AudioComponentGetDescription(component, NonNull::from(&mut desc)) != NO_ERR {
            return None;
        }
        let kind = kind_of(desc.componentType)?;

        let mut name_ref: *const CFString = std::ptr::null();
        let mut full_name = String::new();
        if AudioComponentCopyName(component, NonNull::from(&mut name_ref)) == NO_ERR {
            if let Some(name) = NonNull::new(name_ref as *mut CFString) {
                // "Copy" in the name: we own this reference.
                full_name = CFRetained::from_raw(name).to_string();
            }
        }
        // Components are registered as "Manufacturer: Name".
        let (vendor, name) = match full_name.split_once(": ") {
            Some((vendor, name)) => (vendor.to_string(), name.to_string()),
            None => (String::new(), full_name),
        };

        let mut version = 0u32;
        AudioComponentGetVersion(component, NonNull::from(&mut version));

        let type_code = fourcc_to_string(desc.componentType);
        Some(PluginDescriptor {
            id: PluginId::new(
                PluginFormat::AudioUnit,
                format!(
                    "{}:{}:{}",
                    type_code,
                    fourcc_to_string(desc.componentSubType),
                    fourcc_to_string(desc.componentManufacturer)
                ),
            ),
            name,
            vendor,
            version: format!(
                "{}.{}.{}",
                version >> 16,
                (version >> 8) & 0xFF,
                version & 0xFF
            ),
            kind,
            categories: vec![type_code],
            path: None,
        })
    }
}

/// Lists the instruments, effects and generators in the system's Audio Unit registry.
///
/// Enumeration reads the registry and does not load plugin code, but it still runs in the
/// scanner process like every other format.
pub fn scan() -> Result<Vec<PluginDescriptor>> {
    let mut found = Vec::new();
    let mut desc = any_description();
    let mut component: AudioComponent = std::ptr::null_mut();
    loop {
        // SAFETY: a null "previous" starts the search; the description outlives the call.
        component = unsafe { AudioComponentFindNext(component, NonNull::from(&mut desc)) };
        if component.is_null() {
            break;
        }
        if let Some(descriptor) = describe(component) {
            found.push(descriptor);
        }
    }
    found.sort_by(|a, b| (&a.vendor, &a.name).cmp(&(&b.vendor, &b.name)));
    Ok(found)
}

/// Finds the registered component for a `type:subtype:manufacturer` id.
fn find_component(uid: &str) -> Result<(AudioComponent, AudioComponentDescription)> {
    let bad = || PluginError::NotFound(format!("'{uid}' is not a type:subtype:manufacturer id"));
    let mut parts = uid.split(':');
    let mut code = || parts.next().and_then(fourcc_from_str).ok_or_else(bad);
    let mut desc = AudioComponentDescription {
        componentType: code()?,
        componentSubType: code()?,
        componentManufacturer: code()?,
        componentFlags: 0,
        componentFlagsMask: 0,
    };
    // SAFETY: as in `scan`.
    let component =
        unsafe { AudioComponentFindNext(std::ptr::null_mut(), NonNull::from(&mut desc)) };
    if component.is_null() {
        return Err(PluginError::NotFound(format!(
            "no Audio Unit registered as {uid}"
        )));
    }
    let mut full = any_description();
    // SAFETY: valid component and out-pointer.
    unsafe { AudioComponentGetDescription(component, NonNull::from(&mut full)) };
    Ok((component, full))
}

/// Creates an instance of the described Audio Unit on the calling thread.
pub fn instantiate(descriptor: &PluginDescriptor) -> Result<AuInstance> {
    let (component, full) = find_component(&descriptor.id.uid)?;
    // Prefer what the registry says now over what a possibly stale scan recorded.
    let descriptor = describe(component).unwrap_or_else(|| descriptor.clone());
    AuInstance::new(component, full.componentFlags, descriptor)
}
