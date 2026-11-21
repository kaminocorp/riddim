//! Loading a VST3 module (bundle) and reading its class factory.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use vst3::Steinberg::{
    IPluginFactory, IPluginFactory2, IPluginFactory2Trait, IPluginFactory3, IPluginFactory3Trait,
    IPluginFactoryTrait, PClassInfo, PClassInfo2, PClassInfoW, PFactoryInfo, TUID,
};
use vst3::{ComPtr, Interface};

use super::strings::{from_char16, from_char8, tuid_to_string};
use crate::descriptor::{PluginDescriptor, PluginFormat, PluginId, PluginKind};
use crate::error::{PluginError, Result};

const AUDIO_MODULE_CLASS: &str = "Audio Module Class";

type GetFactoryFn = unsafe extern "system" fn() -> *mut IPluginFactory;

/// A loaded VST3 module. Modules stay loaded for the life of the process: unloading plugin
/// binaries is a classic source of crashes (static destructors, threads still running), and the
/// scanner process exits anyway.
pub struct Module {
    path: PathBuf,
    factory: ComPtr<IPluginFactory>,
    _binary: platform::Binary,
}

/// One audio-processor class exported by a module.
#[derive(Clone, Debug)]
pub struct ClassEntry {
    pub cid: TUID,
    pub descriptor: PluginDescriptor,
}

thread_local! {
    static LOADED: RefCell<HashMap<PathBuf, Rc<Module>>> = RefCell::new(HashMap::new());
}

impl Module {
    /// Loads the module at `path` (a `.vst3` bundle directory, or a bare shared library), or
    /// returns the already-loaded one. Modules are cached per thread because VST3 objects are
    /// tied to the thread that created them.
    pub fn load(path: &Path) -> Result<Rc<Module>> {
        let key = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        if let Some(module) = LOADED.with(|m| m.borrow().get(&key).cloned()) {
            return Ok(module);
        }
        let binary = platform::Binary::open(&key)?;
        // SAFETY: GetPluginFactory is the one symbol every VST3 module must export, with this
        // signature; the module's entry function has already run.
        let factory = unsafe {
            let get: GetFactoryFn = binary.symbol(b"GetPluginFactory\0").ok_or_else(|| {
                PluginError::Load(format!("{} does not export GetPluginFactory", key.display()))
            })?;
            ComPtr::from_raw(get())
        }
        .ok_or_else(|| PluginError::Load(format!("{} returned no factory", key.display())))?;

        let module = Rc::new(Module { path: key.clone(), factory, _binary: binary });
        LOADED.with(|m| m.borrow_mut().insert(key, module.clone()));
        Ok(module)
    }

    /// Lists the audio-processor classes in this module.
    pub fn classes(&self) -> Vec<ClassEntry> {
        let factory = &self.factory;
        let factory2 = factory.cast::<IPluginFactory2>();
        let factory3 = factory.cast::<IPluginFactory3>();

        // SAFETY: all calls below pass valid out-pointers to zero-initialised POD structs.
        unsafe {
            let mut factory_info: PFactoryInfo = std::mem::zeroed();
            factory.getFactoryInfo(&mut factory_info);
            let factory_vendor = from_char8(&factory_info.vendor);

            let mut out = Vec::new();
            for index in 0..factory.countClasses() {
                let mut info: PClassInfo = std::mem::zeroed();
                if factory.getClassInfo(index, &mut info) != vst3::Steinberg::kResultOk {
                    continue;
                }
                if from_char8(&info.category) != AUDIO_MODULE_CLASS {
                    continue;
                }
                let mut name = from_char8(&info.name);
                let mut vendor = String::new();
                let mut version = String::new();
                let mut sub_categories = String::new();

                if let Some(f2) = &factory2 {
                    let mut info2: PClassInfo2 = std::mem::zeroed();
                    if f2.getClassInfo2(index, &mut info2) == vst3::Steinberg::kResultOk {
                        vendor = from_char8(&info2.vendor);
                        version = from_char8(&info2.version);
                        sub_categories = from_char8(&info2.subCategories);
                    }
                }
                if let Some(f3) = &factory3 {
                    let mut info_w: PClassInfoW = std::mem::zeroed();
                    if f3.getClassInfoUnicode(index, &mut info_w) == vst3::Steinberg::kResultOk {
                        let wide_name = from_char16(&info_w.name);
                        if !wide_name.is_empty() {
                            name = wide_name;
                        }
                        let wide_vendor = from_char16(&info_w.vendor);
                        if !wide_vendor.is_empty() {
                            vendor = wide_vendor;
                        }
                        let wide_version = from_char16(&info_w.version);
                        if !wide_version.is_empty() {
                            version = wide_version;
                        }
                        sub_categories = from_char8(&info_w.subCategories);
                    }
                }
                if vendor.is_empty() {
                    vendor = factory_vendor.clone();
                }

                let categories: Vec<String> =
                    sub_categories.split('|').filter(|s| !s.is_empty()).map(String::from).collect();
                out.push(ClassEntry {
                    cid: info.cid,
                    descriptor: PluginDescriptor {
                        id: PluginId::new(PluginFormat::Vst3, tuid_to_string(&info.cid)),
                        name,
                        vendor,
                        version,
                        kind: kind_from_categories(&categories),
                        categories,
                        path: Some(self.path.clone()),
                    },
                });
            }
            out
        }
    }

    /// Asks the factory for an instance of class `cid` as interface `I`.
    pub fn create_instance<I: Interface>(&self, cid: &TUID) -> Option<ComPtr<I>> {
        let mut obj: *mut c_void = std::ptr::null_mut();
        // SAFETY: cid and iid point to 16-byte ids; obj is a valid out-pointer.
        unsafe {
            let result = self.factory.createInstance(
                cid.as_ptr(),
                I::IID.as_ptr() as *const std::ffi::c_char,
                &mut obj,
            );
            if result != vst3::Steinberg::kResultOk {
                return None;
            }
            ComPtr::from_raw(obj as *mut I)
        }
    }
}

fn kind_from_categories(categories: &[String]) -> PluginKind {
    let has = |needle: &str| categories.iter().any(|c| c.eq_ignore_ascii_case(needle));
    if has("Instrument") {
        PluginKind::Instrument
    } else if has("Fx") {
        PluginKind::Effect
    } else if has("Generator") {
        PluginKind::Generator
    } else {
        PluginKind::Other
    }
}

/// Where VST3 plugins are installed on this platform.
pub fn default_search_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if cfg!(target_os = "macos") {
        if let Some(home) = &home {
            paths.push(home.join("Library/Audio/Plug-Ins/VST3"));
        }
        paths.push("/Library/Audio/Plug-Ins/VST3".into());
        paths.push("/Network/Library/Audio/Plug-Ins/VST3".into());
    } else if cfg!(target_os = "windows") {
        if let Some(common) = std::env::var_os("COMMONPROGRAMFILES") {
            paths.push(PathBuf::from(common).join("VST3"));
        }
    } else {
        if let Some(home) = &home {
            paths.push(home.join(".vst3"));
        }
        paths.push("/usr/lib/vst3".into());
        paths.push("/usr/local/lib/vst3".into());
    }
    paths
}

/// Finds `.vst3` bundles (and single-file modules) under `dirs`, without loading anything.
pub fn find_bundles(dirs: &[PathBuf]) -> Vec<PathBuf> {
    fn walk(dir: &Path, depth: u32, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let path = entry.path();
            let is_vst3 = path.extension().is_some_and(|e| e.eq_ignore_ascii_case("vst3"));
            if is_vst3 {
                out.push(path);
            } else if path.is_dir() && depth < 8 {
                walk(&path, depth + 1, out);
            }
        }
    }
    let mut out = Vec::new();
    for dir in dirs {
        walk(dir, 0, &mut out);
    }
    out.sort();
    out.dedup();
    out
}

#[cfg(not(target_os = "macos"))]
mod platform {
    use super::*;

    /// A module binary opened with the dynamic loader.
    pub struct Binary {
        // Never unloaded; see `Module`.
        library: std::mem::ManuallyDrop<libloading::Library>,
    }

    impl Binary {
        pub fn open(path: &Path) -> Result<Binary> {
            let file = binary_path(path)?;
            // SAFETY: loading a plugin runs its initialisers; that is the point of loading it.
            let library = unsafe { libloading::Library::new(&file) }
                .map_err(|e| PluginError::Load(format!("{}: {e}", file.display())))?;
            let binary = Binary { library: std::mem::ManuallyDrop::new(library) };
            binary.run_entry(&file)?;
            Ok(binary)
        }

        #[cfg(target_os = "windows")]
        fn run_entry(&self, file: &Path) -> Result<()> {
            type InitFn = unsafe extern "system" fn() -> bool;
            // SAFETY: InitDll is optional and takes no arguments.
            unsafe {
                if let Some(init) = self.symbol::<InitFn>(b"InitDll\0") {
                    if !init() {
                        return Err(PluginError::Load(format!("{}: InitDll failed", file.display())));
                    }
                }
            }
            Ok(())
        }

        #[cfg(not(target_os = "windows"))]
        fn run_entry(&self, file: &Path) -> Result<()> {
            type EntryFn = unsafe extern "system" fn(*mut c_void) -> bool;
            // SAFETY: ModuleEntry takes the library handle; hosts conventionally pass the
            // dlopen handle, which plugins treat as opaque.
            unsafe {
                if let Some(entry) = self.symbol::<EntryFn>(b"ModuleEntry\0") {
                    let handle = &*self.library as *const libloading::Library as *mut c_void;
                    if !entry(handle) {
                        return Err(PluginError::Load(format!(
                            "{}: ModuleEntry failed",
                            file.display()
                        )));
                    }
                }
            }
            Ok(())
        }

        /// # Safety
        /// `T` must be the correct function-pointer type for `name`.
        pub unsafe fn symbol<T: Copy>(&self, name: &[u8]) -> Option<T> {
            // SAFETY: forwarded to the caller.
            unsafe { self.library.get::<T>(name).ok().map(|s| *s) }
        }
    }

    /// Resolves a bundle directory to the shared library inside it.
    fn binary_path(path: &Path) -> Result<PathBuf> {
        if path.is_file() {
            return Ok(path.to_path_buf());
        }
        let stem = path
            .file_stem()
            .ok_or_else(|| PluginError::Load(format!("{}: not a bundle", path.display())))?;
        let (arch_dir, ext) = if cfg!(target_os = "windows") {
            (format!("{}-win", std::env::consts::ARCH), "vst3")
        } else {
            (format!("{}-linux", std::env::consts::ARCH), "so")
        };
        let mut file = path.join("Contents").join(arch_dir).join(stem);
        file.set_extension(ext);
        if file.is_file() {
            Ok(file)
        } else {
            Err(PluginError::Load(format!(
                "{}: no binary for this platform (expected {})",
                path.display(),
                file.display()
            )))
        }
    }
}

#[cfg(target_os = "macos")]
mod platform {
    //! macOS loads VST3 modules through CFBundle, the way the SDK's own host does: plugins are
    //! handed their `CFBundleRef` in `bundleEntry` and use it to find their resources.
    //!
    //! UNVERIFIED ON MACOS: written against the objc2 bindings and type-checked only.

    use super::*;
    use objc2_core_foundation::{CFBundle, CFRetained, CFString, CFURL};

    pub struct Binary {
        // Never released; see `Module`.
        bundle: std::mem::ManuallyDrop<CFRetained<CFBundle>>,
    }

    impl Binary {
        pub fn open(path: &Path) -> Result<Binary> {
            let err = |why: &str| PluginError::Load(format!("{}: {why}", path.display()));
            let url = CFURL::from_directory_path(path)
                .or_else(|| CFURL::from_file_path(path))
                .ok_or_else(|| err("path is not valid UTF-8"))?;
            let bundle = CFBundle::new(None, Some(&url)).ok_or_else(|| err("not a bundle"))?;
            // SAFETY: a null error pointer is allowed.
            if !unsafe { bundle.load_executable_and_return_error(std::ptr::null_mut()) } {
                return Err(err("the bundle executable could not be loaded"));
            }
            let binary = Binary { bundle: std::mem::ManuallyDrop::new(bundle) };

            type EntryFn = unsafe extern "system" fn(*mut c_void) -> bool;
            // SAFETY: bundleEntry takes the CFBundleRef of the bundle being loaded.
            unsafe {
                if let Some(entry) = binary.symbol::<EntryFn>(b"bundleEntry\0") {
                    let bundle_ref = CFRetained::as_ptr(&binary.bundle).as_ptr() as *mut c_void;
                    if !entry(bundle_ref) {
                        return Err(err("bundleEntry failed"));
                    }
                }
            }
            Ok(binary)
        }

        /// # Safety
        /// `T` must be the correct function-pointer type for `name`.
        pub unsafe fn symbol<T: Copy>(&self, name: &[u8]) -> Option<T> {
            let name = std::str::from_utf8(&name[..name.len() - 1]).ok()?;
            let name = CFString::from_str(name);
            let ptr = self.bundle.function_pointer_for_name(Some(&name));
            if ptr.is_null() {
                return None;
            }
            debug_assert_eq!(std::mem::size_of::<T>(), std::mem::size_of::<*const c_void>());
            // SAFETY: T is a function pointer type per the caller's contract, and function
            // pointers have the same size and representation as data pointers on macOS.
            Some(unsafe { std::mem::transmute_copy::<*const c_void, T>(&(ptr as *const c_void)) })
        }
    }
}
