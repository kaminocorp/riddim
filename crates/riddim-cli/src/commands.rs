//! The subcommands. Each returns `Ok(true)` on success, `Ok(false)` when it ran but the answer
//! is "no" (a failed check, a state mismatch), and `Err` when it could not run at all.

use std::path::{Path, PathBuf};
use std::time::Duration;

use riddim_plugin::check;
use riddim_plugin::host::instantiate;
use riddim_plugin::scan::Scanner;
use riddim_plugin::{PluginDescriptor, PluginInstance, StateBlob};

use crate::resolve::{resolve, scan_everything};
use crate::ui;

pub type CommandResult = Result<bool, String>;

pub struct Context {
    pub vst3_dirs: Vec<PathBuf>,
    pub scanner: Option<PathBuf>,
}

impl Context {
    fn scanner(&self) -> Result<Scanner, String> {
        match &self.scanner {
            Some(path) => Ok(Scanner::new(path)),
            None => Scanner::locate().map_err(|e| e.to_string()),
        }
    }

    pub fn resolve(&self, spec: &str) -> Result<PluginDescriptor, String> {
        resolve(spec, &self.scanner()?, &self.vst3_dirs)
    }
}

pub fn load_state(path: &Path) -> Result<StateBlob, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    StateBlob::from_bytes(&bytes).map_err(|e| format!("{}: {e}", path.display()))
}

/// What `riddim scan` should do besides listing.
pub struct ScanOptions {
    pub json: bool,
    /// Probe everything again, cached or blocked.
    pub rescan: bool,
    /// List the blocklist instead of scanning.
    pub blocklist: bool,
    /// Take this bundle off the blocklist.
    pub unblock: Option<PathBuf>,
}

pub fn scan(context: &Context, options: ScanOptions) -> CommandResult {
    use riddim_plugin::scan::ScanCache;
    let cache_file = ScanCache::default_path();
    if let Some(path) = &options.unblock {
        let mut cache = ScanCache::load(&cache_file);
        let path = path.canonicalize().unwrap_or_else(|_| path.clone());
        let removed = cache.forget(&path);
        cache.save(&cache_file).map_err(|e| e.to_string())?;
        println!(
            "{} {}",
            path.display(),
            if removed {
                "will be scanned again next time"
            } else {
                "was not in the cache"
            }
        );
        return Ok(removed);
    }
    if options.blocklist {
        let blocked = ScanCache::load(&cache_file).blocked();
        for (path, reason) in &blocked {
            println!("{}\n    {reason}", path.display());
        }
        eprintln!(
            "{} blocked bundle(s) in {}",
            blocked.len(),
            cache_file.display()
        );
        return Ok(true);
    }
    let json = options.json;
    let plugins = scan_everything(&context.scanner()?, &context.vst3_dirs, options.rescan);
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&plugins).map_err(|e| e.to_string())?
        );
        return Ok(true);
    }
    for plugin in &plugins {
        println!(
            "{:<5} {:<12} {:<34} {:<24} {:<10} {}",
            plugin.id.format,
            format!("{:?}", plugin.kind).to_lowercase(),
            plugin.name,
            plugin.vendor,
            plugin.version,
            plugin.id
        );
    }
    eprintln!("{} plugin(s)", plugins.len());
    Ok(true)
}

pub fn info(context: &Context, spec: &str, all: bool) -> CommandResult {
    let descriptor = context.resolve(spec)?;
    let instance = instantiate(&descriptor).map_err(|e| e.to_string())?;
    let descriptor = instance.descriptor();
    println!("{} ({})", descriptor.name, descriptor.id);
    println!("  vendor   {}", descriptor.vendor);
    println!("  version  {}", descriptor.version);
    println!("  kind     {:?}", descriptor.kind);
    if let Some(path) = &descriptor.path {
        println!("  path     {}", path.display());
    }
    let layout = instance.bus_layout();
    for (label, buses) in [
        ("in ", &layout.audio_inputs),
        ("out", &layout.audio_outputs),
    ] {
        for (index, bus) in buses.iter().enumerate() {
            println!(
                "  audio {label} {index}: {} ({} ch, {:?}{})",
                bus.name,
                bus.channels,
                bus.kind,
                if bus.active { "" } else { ", inactive" }
            );
        }
    }
    println!(
        "  event buses in/out: {}/{}",
        layout.event_inputs, layout.event_outputs
    );
    println!("  latency  {} samples", instance.latency_samples());
    println!(
        "  editor   {}",
        if instance.has_editor() { "yes" } else { "no" }
    );

    let params = instance.params();
    println!("  parameters: {}", params.len());
    let shown = if all {
        params.len()
    } else {
        params.len().min(40)
    };
    for param in &params[..shown] {
        let value = instance.param_value(param.id).unwrap_or(f64::NAN);
        let text = instance.param_text(param.id, value).unwrap_or_default();
        println!(
            "    {:>10}  {:<32} {:>7.4}  {} {}{}",
            param.id,
            param.name,
            value,
            text,
            param.unit,
            if param.flags.automatable {
                ""
            } else {
                "  (not automatable)"
            }
        );
    }
    if shown < params.len() {
        println!("    ... {} more (use --all)", params.len() - shown);
    }
    Ok(true)
}

pub fn check(context: &Context, specs: &[String], rate: f64, block: u32) -> CommandResult {
    if specs.is_empty() {
        return Err("name at least one plugin to check".into());
    }
    let options = check::Options {
        sample_rate: rate,
        block,
        ..check::Options::default()
    };
    let mut all_passed = true;
    for spec in specs {
        let descriptor = context.resolve(spec)?;
        let report = check::run(&descriptor, &options);
        println!("{report}");
        all_passed &= report.passed();
    }
    Ok(all_passed)
}

pub fn state_save(context: &Context, spec: &str, out: &Path) -> CommandResult {
    let descriptor = context.resolve(spec)?;
    let mut instance = instantiate(&descriptor).map_err(|e| e.to_string())?;
    let state = instance.save_state().map_err(|e| e.to_string())?;
    std::fs::write(out, state.to_bytes()).map_err(|e| format!("{}: {e}", out.display()))?;
    println!(
        "saved {} bytes of state to {}",
        state.data.len(),
        out.display()
    );
    Ok(true)
}

pub fn state_roundtrip(context: &Context, spec: &str, state: Option<&Path>) -> CommandResult {
    let descriptor = context.resolve(spec)?;
    let first = match state {
        Some(path) => load_state(path)?,
        None => {
            let mut instance = instantiate(&descriptor).map_err(|e| e.to_string())?;
            instance.save_state().map_err(|e| e.to_string())?
        }
    };
    let mut instance = instantiate(&descriptor).map_err(|e| e.to_string())?;
    instance.restore_state(&first).map_err(|e| e.to_string())?;
    let second = instance.save_state().map_err(|e| e.to_string())?;
    if first == second {
        println!(
            "identical: {} bytes survive save -> restore -> save",
            first.data.len()
        );
        return Ok(true);
    }
    let differing = first
        .data
        .iter()
        .zip(&second.data)
        .filter(|(a, b)| a != b)
        .count();
    let first_diff = first
        .data
        .iter()
        .zip(&second.data)
        .position(|(a, b)| a != b);
    println!(
        "DIFFERENT: {} bytes before, {} after; {} byte(s) differ in the common prefix{}",
        first.data.len(),
        second.data.len(),
        differing,
        first_diff.map_or(String::new(), |at| format!(", first at offset {at}"))
    );
    Ok(false)
}

pub fn editor_cycle(context: &Context, spec: &str, count: u32) -> CommandResult {
    let descriptor = context.resolve(spec)?;
    ui::init()?;
    let mut instance = instantiate(&descriptor).map_err(|e| e.to_string())?;
    if !instance.has_editor() {
        return Err(format!("{} has no editor", instance.descriptor().name));
    }
    let title = instance.descriptor().name.clone();
    let mut events = Vec::new();
    for round in 1..=count {
        let mut window = ui::open(instance.as_mut(), &title)?;
        let mut settle = |window: &mut ui::EditorWindow, instance: &mut dyn PluginInstance| {
            for _ in 0..10 {
                ui::pump(Duration::from_millis(20));
                events.clear();
                instance.poll_events(&mut events);
                for event in &events {
                    window.handle_event(instance, event);
                }
                window.sync(instance);
            }
        };
        settle(&mut window, instance.as_mut());
        #[cfg(target_os = "macos")]
        {
            let opened = window.size();
            let mut line = format!(
                "round {round}: opened at {}x{}",
                opened.width, opened.height
            );
            if instance.editor_resizable() {
                let bigger = riddim_plugin::EditorSize {
                    width: opened.width + 60,
                    height: opened.height + 40,
                };
                let got = window.request_size(instance.as_mut(), bigger);
                settle(&mut window, instance.as_mut());
                let back = window.request_size(instance.as_mut(), opened);
                line.push_str(&format!(
                    ", resized to {}x{}, back to {}x{}",
                    got.width, got.height, back.width, back.height
                ));
            } else {
                line.push_str(", fixed size");
            }
            println!("{line}");
        }
        #[cfg(not(target_os = "macos"))]
        println!("round {round}");
        window.close(instance.as_mut());
        ui::pump(Duration::from_millis(50));
    }
    println!("{count} open/resize/close cycle(s) completed");
    Ok(true)
}

pub fn devices() -> CommandResult {
    let devices = riddim::devices::list();
    if devices.is_empty() {
        println!(
            "no audio hardware backend on this platform (the null device is always available)"
        );
    }
    for device in devices {
        println!(
            "{}{} in {} out {}  {} Hz  buffers {}..{}  [{}]",
            device.name,
            match (device.is_default_input, device.is_default_output) {
                (true, true) => " (default in+out)",
                (true, false) => " (default in)",
                (false, true) => " (default out)",
                _ => "",
            },
            device.input_channels,
            device.output_channels,
            device.sample_rate,
            device.buffer_range.0,
            device.buffer_range.1,
            device.id.0
        );
    }
    Ok(true)
}
