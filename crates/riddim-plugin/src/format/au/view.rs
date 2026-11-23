//! Creating an Audio Unit's Cocoa view.

use std::ffi::c_void;
use std::ptr::NonNull;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{msg_send, MainThreadMarker};
use objc2_app_kit::NSView;
use objc2_audio_toolbox::{AudioUnit, AudioUnitGetProperty, AudioUnitGetPropertyInfo};
use objc2_core_audio_kit::AUGenericView;
use objc2_foundation::{NSBundle, NSSize, NSString, NSURL};

use super::sys::{NO_ERR, PROP_COCOA_UI, SCOPE_GLOBAL};

/// `AudioUnitCocoaViewInfo` with untyped references.
#[repr(C)]
struct CocoaViewInfo {
    bundle_location: *mut c_void,
    view_class: *mut c_void,
}

/// The unit's own Cocoa view (`kAudioUnitProperty_CocoaUI`), if it publishes one.
fn custom_view(unit: AudioUnit) -> Option<Retained<NSView>> {
    let mut size = 0u32;
    let mut writable = 0u8;
    // SAFETY: valid out-pointers; the property data is read into a buffer of the size the unit
    // reported, which is at least one CocoaViewInfo.
    unsafe {
        if AudioUnitGetPropertyInfo(
            unit,
            PROP_COCOA_UI,
            SCOPE_GLOBAL,
            0,
            &mut size,
            &mut writable,
        ) != NO_ERR
            || (size as usize) < std::mem::size_of::<CocoaViewInfo>()
        {
            return None;
        }
        let mut storage = vec![0u64; (size as usize).div_ceil(8)];
        let status = AudioUnitGetProperty(
            unit,
            PROP_COCOA_UI,
            SCOPE_GLOBAL,
            0,
            NonNull::new_unchecked(storage.as_mut_ptr() as *mut c_void),
            NonNull::from(&mut size),
        );
        if status != NO_ERR {
            return None;
        }
        let info = &*(storage.as_ptr() as *const CocoaViewInfo);
        // The caller owns both references. CFURL and CFString are toll-free bridged to NSURL
        // and NSString, so they can be adopted directly.
        let url = Retained::from_raw(info.bundle_location as *mut NSURL)?;
        let class_name = Retained::from_raw(info.view_class as *mut NSString)?;
        // Any further class names in the property are extra references we must drop too.
        let extra = (size as usize - std::mem::size_of::<CocoaViewInfo>()) / 8;
        for i in 0..extra {
            let name = storage[2 + i] as *mut NSString;
            drop(Retained::from_raw(name));
        }

        let bundle = NSBundle::bundleWithURL(&url)?;
        let class = bundle.classNamed(&class_name)?;
        // The class implements the AUCocoaUIBase protocol: a factory for the actual view.
        let factory: Option<Retained<AnyObject>> = msg_send![class, new];
        let factory = factory?;
        let view: Option<Retained<NSView>> = msg_send![
            &*factory,
            uiViewForAudioUnit: unit,
            withSize: NSSize::new(0.0, 0.0)
        ];
        view
    }
}

/// Apple's generic parameter view, for units without a view of their own.
fn generic_view(unit: AudioUnit, mtm: MainThreadMarker) -> Option<Retained<NSView>> {
    // SAFETY: initWithAudioUnit takes a live unit; we are on the main thread.
    let view = unsafe { AUGenericView::initWithAudioUnit(mtm.alloc::<AUGenericView>(), unit) };
    Some(Retained::into_super(view))
}

/// Creates the best available view for `unit`. Main thread only.
pub(super) fn create_view(unit: AudioUnit, mtm: MainThreadMarker) -> Option<Retained<NSView>> {
    custom_view(unit).or_else(|| generic_view(unit, mtm))
}
