//! Helpers shared by integration tests across the workspace.
//!
//! The fixture plugins are `cdylib` crates. Cargo cannot hand a test the path of another
//! package's dynamic library, so these helpers build the fixtures with a nested `cargo build`
//! (a no-op when they are fresh) and wrap the result in the bundle layout hosts expect.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// Class ids and parameter ids of the fixture plugins, mirrored here so tests do not need to
/// link the plugin crate.
pub mod ids {
    pub const SYNTH_UID: &str = "52444D5453594E3150524F4300000001";
    pub const DELAY_UID: &str = "52444D54444C593150524F4300000001";
    pub const GAIN_UID: &str = "52444D5447414E3153494E4700000001";
    /// The gain plugin again, taking 400 ms to initialise.
    pub const SLOW_UID: &str = "52444D54534C4F5753494E4700000001";
    pub const SLOW_LOAD_MS: u64 = 400;

    pub const SYNTH_GAIN: u32 = 100;
    pub const SYNTH_MOD: u32 = 101;
    pub const SYNTH_METER: u32 = 103;
    pub const SYNTH_TEMPO_SEEN: u32 = 104;
    pub const SYNTH_PLAYING_SEEN: u32 = 105;

    pub const DELAY_DELAY: u32 = 0;
    pub const DELAY_GAIN: u32 = 1;
    pub const DELAY_MAX: u32 = 1024;

    pub const GAIN_GAIN: u32 = 7;

    /// Setting this on a fixture's controller makes the plugin act of its own accord. Use the
    /// `COMMAND_*` values.
    pub const COMMAND: u32 = 0x7E57_0001;
    /// Drag the first parameter to 0.75 in the "editor" (begin, perform, end).
    pub const COMMAND_EDIT: f64 = 1.0 / 8.0;
    pub const COMMAND_DIRTY: f64 = 2.0 / 8.0;
    pub const COMMAND_VALUES_CHANGED: f64 = 3.0 / 8.0;
    pub const COMMAND_INFO_CHANGED: f64 = 4.0 / 8.0;
    pub const COMMAND_RESTART: f64 = 5.0 / 8.0;
    /// Abort the process the plugin is running in.
    pub const COMMAND_ABORT: f64 = 6.0 / 8.0;
    /// Make every later `process` call hang.
    pub const COMMAND_HANG: f64 = 7.0 / 8.0;
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root")
}

/// Builds the fixture crates in the same profile as the running test and returns the path of
/// each one's dynamic library, as reported by cargo itself.
fn fixture_libraries() -> &'static Vec<(String, PathBuf)> {
    static BUILT: OnceLock<Vec<(String, PathBuf)>> = OnceLock::new();
    BUILT.get_or_init(|| {
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let mut command = Command::new(cargo);
        command.current_dir(workspace_root()).args([
            "build",
            "--quiet",
            "--message-format=json",
            "-p",
            "riddim-testplug",
            "-p",
            "riddim-crashplug",
            "-p",
            "riddim-plugin",
            "--bin",
            "riddim-scan",
            "--bin",
            "riddim-bridge",
            "--lib",
        ]);
        if !cfg!(debug_assertions) {
            command.arg("--release");
        }
        let output = command
            .output()
            .expect("could not run cargo to build the fixture plugins");
        assert!(
            output.status.success(),
            "building the fixture plugins failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );

        let mut libraries = Vec::new();
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let Ok(message) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if message["reason"] != "compiler-artifact" {
                continue;
            }
            let name = message["target"]["name"].as_str().unwrap_or_default();
            if name == "riddim-scan" || name == "riddim-bridge" {
                if let Some(executable) = message["executable"].as_str() {
                    libraries.push((name.to_string(), PathBuf::from(executable)));
                }
                continue;
            }
            let is_cdylib = message["target"]["kind"]
                .as_array()
                .is_some_and(|kinds| kinds.iter().any(|k| k == "cdylib"));
            let Some(name) = message["target"]["name"].as_str() else {
                continue;
            };
            if !is_cdylib {
                continue;
            }
            let library = message["filenames"].as_array().and_then(|files| {
                files.iter().filter_map(|f| f.as_str()).find(|f| {
                    f.ends_with(std::env::consts::DLL_SUFFIX)
                        && !std::env::consts::DLL_SUFFIX.is_empty()
                })
            });
            if let Some(library) = library {
                libraries.push((name.replace('-', "_"), PathBuf::from(library)));
            }
        }
        libraries
    })
}

/// Wraps a built fixture library in a `.vst3` bundle and returns the bundle path.
fn vst3_bundle(krate: &str, bundle_name: &str) -> PathBuf {
    let wanted = krate.replace('-', "_");
    let library = fixture_libraries()
        .iter()
        .find(|(name, _)| *name == wanted)
        .map(|(_, path)| path.clone())
        .unwrap_or_else(|| panic!("cargo did not report a library for {krate}"));
    assert!(
        library.is_file(),
        "fixture library missing: {}",
        library.display()
    );

    let bundle = library
        .parent()
        .expect("library directory")
        .join("test-bundles")
        .join(format!("{bundle_name}.vst3"));
    let contents = bundle.join("Contents");
    let binary = if cfg!(target_os = "macos") {
        let plist = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
             \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
             <plist version=\"1.0\"><dict>\n\
             <key>CFBundleExecutable</key><string>{bundle_name}</string>\n\
             <key>CFBundleIdentifier</key><string>dev.riddim.test.{bundle_name}</string>\n\
             <key>CFBundleName</key><string>{bundle_name}</string>\n\
             <key>CFBundlePackageType</key><string>BNDL</string>\n\
             <key>CFBundleVersion</key><string>1.0.0</string>\n\
             </dict></plist>\n"
        );
        std::fs::create_dir_all(contents.join("MacOS")).unwrap();
        write_if_changed(&contents.join("Info.plist"), plist.as_bytes());
        contents.join("MacOS").join(bundle_name)
    } else if cfg!(target_os = "windows") {
        let dir = contents.join(format!("{}-win", std::env::consts::ARCH));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(format!("{bundle_name}.vst3"))
    } else {
        let dir = contents.join(format!("{}-linux", std::env::consts::ARCH));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(format!("{bundle_name}.so"))
    };
    let bytes = std::fs::read(&library).unwrap();
    write_if_changed(&binary, &bytes);
    bundle
}

/// Writes atomically, and only when the content differs, so parallel tests never see a
/// half-written plugin binary.
fn write_if_changed(path: &Path, bytes: &[u8]) {
    if std::fs::read(path).is_ok_and(|existing| existing == bytes) {
        return;
    }
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, bytes).unwrap();
    std::fs::rename(&tmp, path).unwrap();
}

/// The engine's C ABI as a dynamic library, built on demand. Returns its path.
pub fn ffi_library() -> PathBuf {
    static BUILT: OnceLock<PathBuf> = OnceLock::new();
    BUILT
        .get_or_init(|| {
            let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
            let mut command = Command::new(cargo);
            command.current_dir(workspace_root()).args([
                "build",
                "--quiet",
                "--message-format=json",
                "-p",
                "riddim-ffi",
                "--lib",
            ]);
            if !cfg!(debug_assertions) {
                command.arg("--release");
            }
            let output = command
                .output()
                .expect("could not run cargo to build riddim-ffi");
            assert!(
                output.status.success(),
                "building riddim-ffi failed:\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
            for line in String::from_utf8_lossy(&output.stdout).lines() {
                let Ok(message) = serde_json::from_str::<serde_json::Value>(line) else {
                    continue;
                };
                if message["reason"] != "compiler-artifact"
                    || message["target"]["name"] != "riddim_ffi"
                {
                    continue;
                }
                let library = message["filenames"].as_array().and_then(|files| {
                    files
                        .iter()
                        .filter_map(|f| f.as_str())
                        .find(|f| f.ends_with(std::env::consts::DLL_SUFFIX))
                });
                if let Some(library) = library {
                    return PathBuf::from(library);
                }
            }
            panic!("cargo did not report a dynamic library for riddim-ffi");
        })
        .clone()
}

/// The scanner child-process executable, built on demand.
pub fn scanner_exe() -> PathBuf {
    fixture_libraries()
        .iter()
        .find(|(name, _)| name == "riddim-scan")
        .map(|(_, path)| path.clone())
        .expect("cargo did not report the riddim-scan executable")
}

/// The plugin bridge child-process executable, built on demand.
pub fn bridge_exe() -> PathBuf {
    fixture_libraries()
        .iter()
        .find(|(name, _)| name == "riddim-bridge")
        .map(|(_, path)| path.clone())
        .expect("cargo did not report the riddim-bridge executable")
}

/// The bundle holding the test synth, delay and gain plugins.
pub fn testplug_bundle() -> PathBuf {
    static BUNDLE: OnceLock<PathBuf> = OnceLock::new();
    BUNDLE
        .get_or_init(|| vst3_bundle("riddim-testplug", "RiddimTest"))
        .clone()
}

/// A bundle that crashes the process that scans it.
pub fn crashplug_bundle() -> PathBuf {
    static BUNDLE: OnceLock<PathBuf> = OnceLock::new();
    BUNDLE
        .get_or_init(|| vst3_bundle("riddim-crashplug", "RiddimCrash"))
        .clone()
}
