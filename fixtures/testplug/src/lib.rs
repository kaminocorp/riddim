//! Deterministic VST3 plugins used to test the Riddim host. Not shipped.
//!
//! * **Riddim Test Synth** — an instrument with a separate edit controller, a second (inactive
//!   by default) stereo output, a MIDI-mapped parameter, output parameters and a note echo on
//!   its event output.
//! * **Riddim Test Delay** — an effect that delays by a parameter-controlled number of samples
//!   and reports that as latency, with a sidechain input.
//! * **Riddim Test Gain** — a single-component effect (processor and controller in one object).
//! * **Riddim Test Slow Loader** — the same effect, but it takes 400 ms to initialise.
//!
//! Nothing here allocates, locks or does I/O inside `process`.

#![allow(non_snake_case)]
#![allow(clippy::missing_safety_doc)]
// The binding typedefs differ between platforms, so casts that look redundant here are not.
#![allow(clippy::unnecessary_cast)]
#![allow(clippy::new_without_default)]

mod framework;
mod gain;
mod kernels;

use std::ffi::c_void;

use vst3::Steinberg::{
    int32, kInvalidArgument, kResultOk, tresult, FIDString, FUnknown, IPluginFactory,
    IPluginFactory2, IPluginFactory2Trait, IPluginFactoryTrait, PClassInfo, PClassInfo2,
    PFactoryInfo, TUID,
};
use vst3::{Class, ComWrapper};

use framework::{copy_cstring, Controller, Kernel, Processor};
use kernels::{DelayKernel, SynthKernel};

const VENDOR: &str = "Riddim Test";
const VERSION: &str = "1.0.0";
const SDK_VERSION: &str = "VST 3.8.0";

struct ClassRow {
    cid: TUID,
    category: &'static str,
    name: &'static str,
    sub_categories: &'static str,
    create: fn() -> vst3::ComPtr<FUnknown>,
}

fn classes() -> [ClassRow; 6] {
    [
        ClassRow {
            cid: SynthKernel::CID,
            category: "Audio Module Class",
            name: SynthKernel::NAME,
            sub_categories: SynthKernel::SUB_CATEGORIES,
            create: || {
                ComWrapper::new(Processor::<SynthKernel>::new())
                    .to_com_ptr()
                    .unwrap()
            },
        },
        ClassRow {
            cid: SynthKernel::CONTROLLER_CID,
            category: "Component Controller Class",
            name: SynthKernel::NAME,
            sub_categories: "",
            create: || {
                ComWrapper::new(Controller::<SynthKernel>::new())
                    .to_com_ptr()
                    .unwrap()
            },
        },
        ClassRow {
            cid: DelayKernel::CID,
            category: "Audio Module Class",
            name: DelayKernel::NAME,
            sub_categories: DelayKernel::SUB_CATEGORIES,
            create: || {
                ComWrapper::new(Processor::<DelayKernel>::new())
                    .to_com_ptr()
                    .unwrap()
            },
        },
        ClassRow {
            cid: DelayKernel::CONTROLLER_CID,
            category: "Component Controller Class",
            name: DelayKernel::NAME,
            sub_categories: "",
            create: || {
                ComWrapper::new(Controller::<DelayKernel>::new())
                    .to_com_ptr()
                    .unwrap()
            },
        },
        ClassRow {
            cid: gain::GainSingle::CID,
            category: "Audio Module Class",
            name: gain::GainSingle::NAME,
            sub_categories: "Fx",
            create: || {
                ComWrapper::new(gain::GainSingle::new())
                    .to_com_ptr()
                    .unwrap()
            },
        },
        ClassRow {
            cid: gain::GainSingle::SLOW_CID,
            category: "Audio Module Class",
            name: gain::GainSingle::SLOW_NAME,
            sub_categories: "Fx",
            create: || {
                ComWrapper::new(gain::GainSingle::new_slow())
                    .to_com_ptr()
                    .unwrap()
            },
        },
    ]
}

struct Factory;

impl Class for Factory {
    type Interfaces = (IPluginFactory, IPluginFactory2);
}

impl IPluginFactoryTrait for Factory {
    unsafe fn getFactoryInfo(&self, info: *mut PFactoryInfo) -> tresult {
        let info = unsafe { &mut *info };
        copy_cstring(VENDOR, &mut info.vendor);
        copy_cstring("https://github.com/kaminocorp/riddim", &mut info.url);
        copy_cstring("", &mut info.email);
        info.flags = 1 << 4; // kUnicode
        kResultOk
    }

    unsafe fn countClasses(&self) -> int32 {
        classes().len() as int32
    }

    unsafe fn getClassInfo(&self, index: int32, info: *mut PClassInfo) -> tresult {
        let rows = classes();
        let Some(row) = rows.get(index as usize) else {
            return kInvalidArgument;
        };
        let info = unsafe { &mut *info };
        info.cid = row.cid;
        info.cardinality = 0x7FFF_FFFF; // kManyInstances
        copy_cstring(row.category, &mut info.category);
        copy_cstring(row.name, &mut info.name);
        kResultOk
    }

    unsafe fn createInstance(
        &self,
        cid: FIDString,
        iid: FIDString,
        obj: *mut *mut c_void,
    ) -> tresult {
        let wanted = unsafe { *(cid as *const TUID) };
        for row in classes() {
            if row.cid == wanted {
                let instance = (row.create)();
                let ptr = instance.as_ptr();
                return unsafe { ((*(*ptr).vtbl).queryInterface)(ptr, iid as *const TUID, obj) };
            }
        }
        kInvalidArgument
    }
}

impl IPluginFactory2Trait for Factory {
    unsafe fn getClassInfo2(&self, index: int32, info: *mut PClassInfo2) -> tresult {
        let rows = classes();
        let Some(row) = rows.get(index as usize) else {
            return kInvalidArgument;
        };
        let info = unsafe { &mut *info };
        info.cid = row.cid;
        info.cardinality = 0x7FFF_FFFF;
        copy_cstring(row.category, &mut info.category);
        copy_cstring(row.name, &mut info.name);
        info.classFlags = 0;
        copy_cstring(row.sub_categories, &mut info.subCategories);
        copy_cstring(VENDOR, &mut info.vendor);
        copy_cstring(VERSION, &mut info.version);
        copy_cstring(SDK_VERSION, &mut info.sdkVersion);
        kResultOk
    }
}

#[cfg(target_os = "windows")]
#[no_mangle]
pub extern "system" fn InitDll() -> bool {
    true
}

#[cfg(target_os = "windows")]
#[no_mangle]
pub extern "system" fn ExitDll() -> bool {
    true
}

#[cfg(target_os = "macos")]
#[no_mangle]
pub extern "system" fn bundleEntry(_bundle_ref: *mut c_void) -> bool {
    true
}

#[cfg(target_os = "macos")]
#[no_mangle]
pub extern "system" fn bundleExit() -> bool {
    true
}

#[cfg(all(unix, not(target_os = "macos")))]
#[no_mangle]
pub extern "system" fn ModuleEntry(_library_handle: *mut c_void) -> bool {
    true
}

#[cfg(all(unix, not(target_os = "macos")))]
#[no_mangle]
pub extern "system" fn ModuleExit() -> bool {
    true
}

#[no_mangle]
pub extern "system" fn GetPluginFactory() -> *mut IPluginFactory {
    ComWrapper::new(Factory)
        .to_com_ptr::<IPluginFactory>()
        .unwrap()
        .into_raw()
}
