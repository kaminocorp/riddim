//! The subcommands. Each returns `Ok(true)` on success, `Ok(false)` when it ran but the answer
//! is "no" (a failed check, a state mismatch), and `Err` when it could not run at all.

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use riddim_io::midi::MidiInput;
use riddim_io::AudioStream;
use riddim_plugin::check;
use riddim_plugin::host::instantiate;
use riddim_plugin::scan::Scanner;
use riddim_plugin::{
    ActivateConfig, EventBuffer, PluginDescriptor, PluginEvent, PluginInstance, ProcessContext,
    ProcessMode, StateBlob, TransportInfo,
};
use riddim_rt::spsc;

use crate::resolve::{resolve, scan_everything};
use crate::spike::{FromAudio, Player, Sequencer, Stats, ToAudio};
use crate::wav;

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

    fn resolve(&self, spec: &str) -> Result<PluginDescriptor, String> {
        resolve(spec, &self.scanner()?, &self.vst3_dirs)
    }
}

fn load_state(path: &Path) -> Result<StateBlob, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    StateBlob::from_bytes(&bytes).map_err(|e| format!("{}: {e}", path.display()))
}

pub fn scan(context: &Context, json: bool) -> CommandResult {
    let plugins = scan_everything(&context.scanner()?, &context.vst3_dirs);
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

pub fn render(
    context: &Context,
    spec: &str,
    out: &Path,
    seconds: f64,
    rate: f64,
    block: u32,
    state: Option<&Path>,
) -> CommandResult {
    let descriptor = context.resolve(spec)?;
    let mut instance = instantiate(&descriptor).map_err(|e| e.to_string())?;
    if let Some(path) = state {
        instance
            .restore_state(&load_state(path)?)
            .map_err(|e| e.to_string())?;
    }
    let config = ActivateConfig {
        sample_rate: rate,
        max_block: block,
        mode: ProcessMode::Offline,
    };
    let mut processor = instance.activate(&config).map_err(|e| e.to_string())?;
    let layout = instance.bus_layout();
    let block = block as usize;
    let total = (seconds * rate) as usize;

    let inputs = vec![vec![0.0f32; block]; layout.active_input_channels()];
    let mut outputs = vec![vec![0.0f32; block]; layout.active_output_channels()];
    let keep = outputs.len().clamp(1, 2);
    let mut rendered = vec![Vec::with_capacity(total); keep];
    let mut events_in = EventBuffer::with_capacity(256);
    let mut events_out = EventBuffer::with_capacity(1024);
    let mut sequencer = Sequencer::new(rate);
    let mut transport = TransportInfo::stopped(rate);
    transport.playing = true;
    let mut failed = 0u32;

    let mut done = 0;
    while done < total {
        let frames = block.min(total - done);
        events_in.clear();
        events_out.clear();
        sequencer.emit(frames, &mut events_in);
        let input_refs: Vec<&[f32]> = inputs.iter().map(|c| &c[..frames]).collect();
        let mut output_refs: Vec<&mut [f32]> =
            outputs.iter_mut().map(|c| &mut c[..frames]).collect();
        let mut process = ProcessContext {
            frames,
            inputs: &input_refs,
            outputs: &mut output_refs,
            events_in: events_in.as_slice(),
            events_out: &mut events_out,
            transport: &transport,
        };
        if processor.process(&mut process) != riddim_plugin::ProcessStatus::Ok {
            failed += 1;
        }
        for (channel, sink) in rendered.iter_mut().enumerate() {
            match outputs.get(channel) {
                Some(source) => sink.extend_from_slice(&source[..frames]),
                None => sink.extend(std::iter::repeat_n(0.0, frames)),
            }
        }
        transport.position_samples += frames as i64;
        transport.continuous_samples += frames as i64;
        transport.position_quarters = transport.position_samples as f64 / rate * 2.0;
        done += frames;
    }
    instance.deactivate(processor);

    wav::write_f32(out, rate as u32, &rendered).map_err(|e| format!("{}: {e}", out.display()))?;
    let peak = rendered
        .iter()
        .flatten()
        .fold(0.0f32, |m, s| m.max(s.abs()));
    println!(
        "rendered {:.2} s of {} to {} (peak {:.4}, {} failed block(s))",
        seconds,
        instance.descriptor().name,
        out.display(),
        peak,
        failed
    );
    Ok(failed == 0 && peak > 0.0)
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

pub struct PlayOptions {
    pub rate: f64,
    pub buffer: u32,
    pub seconds: Option<f64>,
    pub sequence: bool,
    pub editor: bool,
    pub null_device: bool,
    pub state: Option<PathBuf>,
    pub save_state: Option<PathBuf>,
}

fn open_null_stream(options: &PlayOptions, player: Player) -> Box<dyn AudioStream> {
    let config = riddim_io::null::NullConfig {
        sample_rate: options.rate,
        buffer_frames: options.buffer,
        input_channels: 0,
        ..riddim_io::null::NullConfig::default()
    };
    Box::new(riddim_io::null::NullStream::new(config, player))
}

#[cfg(target_os = "macos")]
fn open_stream(options: &PlayOptions, player: Player) -> Result<Box<dyn AudioStream>, String> {
    if options.null_device {
        return Ok(open_null_stream(options, player));
    }
    let config = riddim_io::StreamConfig {
        device: None,
        sample_rate: options.rate,
        buffer_frames: options.buffer,
        capture: false,
    };
    let stream =
        riddim_io::coreaudio::CoreAudioStream::open(&config, player).map_err(|e| e.to_string())?;
    Ok(Box::new(stream))
}

#[cfg(not(target_os = "macos"))]
fn open_stream(options: &PlayOptions, player: Player) -> Result<Box<dyn AudioStream>, String> {
    if !options.null_device {
        eprintln!("note: no audio hardware backend on this platform; using the silent null device");
    }
    Ok(open_null_stream(options, player))
}

#[cfg(target_os = "macos")]
mod ui {
    use super::*;
    pub use riddim_plugin::editor_window::EditorWindow;

    pub fn init() -> Result<(), String> {
        riddim_plugin::editor_window::init_application().map_err(|e| e.to_string())
    }

    pub fn pump(wait: Duration) {
        riddim_plugin::editor_window::pump_events(wait);
    }

    pub fn open(instance: &mut dyn PluginInstance, title: &str) -> Result<EditorWindow, String> {
        EditorWindow::open(instance, title).map_err(|e| e.to_string())
    }
}

#[cfg(not(target_os = "macos"))]
mod ui {
    use super::*;

    /// Stand-in so the control flow is the same on every platform.
    pub struct EditorWindow;

    impl EditorWindow {
        pub fn handle_event(&mut self, _instance: &mut dyn PluginInstance, _event: &PluginEvent) {}
        pub fn sync(&mut self, _instance: &mut dyn PluginInstance) -> bool {
            true
        }
        pub fn close(self, _instance: &mut dyn PluginInstance) {}
    }

    pub fn init() -> Result<(), String> {
        Ok(())
    }

    pub fn pump(wait: Duration) {
        std::thread::sleep(wait);
    }

    pub fn open(_instance: &mut dyn PluginInstance, _title: &str) -> Result<EditorWindow, String> {
        Err("plugin editor windows are only implemented on macOS".into())
    }
}

pub fn play(context: &Context, spec: &str, options: PlayOptions) -> CommandResult {
    let descriptor = context.resolve(spec)?;
    ui::init()?;
    let mut instance = instantiate(&descriptor).map_err(|e| e.to_string())?;
    if let Some(path) = &options.state {
        instance
            .restore_state(&load_state(path)?)
            .map_err(|e| e.to_string())?;
        println!("restored state from {}", path.display());
    }
    let layout = instance.bus_layout();

    let (mut to_audio, control) = spsc::channel::<ToAudio>(1024);
    let (replies, mut from_audio) = spsc::channel::<FromAudio>(4096);
    let (midi_tx, midi_rx) = spsc::channel(4096);
    let stats = Arc::new(Stats::default());
    // The plugin never sees a block larger than this, whatever the device delivers.
    let max_block = options.buffer.max(16) as usize;
    let player = Player::new(
        control,
        replies,
        midi_rx,
        options.sequence.then(|| Sequencer::new(options.rate)),
        layout.active_input_channels(),
        layout.active_output_channels(),
        options.rate,
        max_block,
        stats.clone(),
    );
    let mut stream = open_stream(&options, player)?;
    let info = stream.info().clone();
    println!(
        "device: {} at {} Hz, {} frames, {} out; output latency {} frames",
        info.device_name,
        info.sample_rate,
        info.max_frames,
        info.output_channels,
        info.output_latency_frames
    );

    let config = ActivateConfig {
        sample_rate: info.sample_rate,
        max_block: max_block as u32,
        mode: ProcessMode::Realtime,
    };
    let processor = instance.activate(&config).map_err(|e| e.to_string())?;
    if to_audio.push(ToAudio::Install(processor)).is_err() {
        return Err("could not hand the processor to the audio thread".into());
    }

    let midi = MidiInput::open_all("Riddim", midi_tx)?;
    for port in midi.ports() {
        println!("midi in: {}", port.name);
    }
    stream.start().map_err(|e| e.to_string())?;

    let mut window = if options.editor && instance.has_editor() {
        let title = instance.descriptor().name.clone();
        match ui::open(instance.as_mut(), &title) {
            Ok(window) => Some(window),
            Err(why) => {
                eprintln!("note: no editor window: {why}");
                None
            }
        }
    } else {
        None
    };

    // Enter on stdin ends the session; so does closing the editor or running out of time.
    let (quit_tx, quit_rx) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).is_ok_and(|n| n > 0) {
            let _ = quit_tx.send(());
        }
    });
    match options.seconds {
        Some(seconds) => println!("playing for {seconds} s (Enter to stop early)"),
        None => println!("playing; press Enter to stop"),
    }

    let started = Instant::now();
    let mut events = Vec::new();
    loop {
        ui::pump(Duration::from_millis(10));
        events.clear();
        instance.poll_events(&mut events);
        for event in &events {
            match *event {
                // An edit made in the plugin's editor has to be forwarded to its processor.
                PluginEvent::ParamChanged { id, value } => {
                    let _ = to_audio.push(ToAudio::Param { id, value });
                }
                PluginEvent::LatencyChanged => {
                    println!("latency is now {} samples", instance.latency_samples());
                }
                _ => {}
            }
            if let Some(window) = window.as_mut() {
                window.handle_event(instance.as_mut(), event);
            }
        }
        while let Ok(message) = from_audio.pop() {
            if let FromAudio::Param { id, value } = message {
                instance.set_param_value(id, value);
            }
        }
        if let Some(open) = window.as_mut() {
            if !open.sync(instance.as_mut()) {
                break;
            }
        }
        if quit_rx.try_recv().is_ok()
            || options
                .seconds
                .is_some_and(|s| started.elapsed().as_secs_f64() >= s)
        {
            break;
        }
    }

    if let Some(window) = window.take() {
        window.close(instance.as_mut());
    }
    // Ask the audio thread for the processor back, then stop the device.
    let _ = to_audio.push(ToAudio::Release);
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut returned = None;
    while returned.is_none() && Instant::now() < deadline {
        match from_audio.pop() {
            Ok(FromAudio::Released(processor)) => returned = Some(processor),
            Ok(_) => {}
            Err(_) => std::thread::sleep(Duration::from_millis(2)),
        }
    }
    stream.stop();
    let xruns = stream.xruns();
    drop(stream);
    match returned {
        Some(processor) => instance.deactivate(processor),
        None => eprintln!("warning: the audio thread did not return the processor"),
    }

    if let Some(path) = &options.save_state {
        let state = instance.save_state().map_err(|e| e.to_string())?;
        std::fs::write(path, state.to_bytes()).map_err(|e| format!("{}: {e}", path.display()))?;
        println!(
            "saved {} bytes of state to {}",
            state.data.len(),
            path.display()
        );
    }

    let violations = stats.rt_violations.load(Ordering::Relaxed);
    println!(
        "blocks {}  failed {}  xruns {}  midi messages {}  peak {:.4}  audio-thread heap operations {}",
        stats.blocks.load(Ordering::Relaxed),
        stats.failed_blocks.load(Ordering::Relaxed),
        xruns,
        stats.midi_messages.load(Ordering::Relaxed),
        stats.peak.load(Ordering::Relaxed),
        violations
    );
    Ok(stats.failed_blocks.load(Ordering::Relaxed) == 0 && violations == 0)
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
    #[cfg(target_os = "macos")]
    {
        for device in riddim_io::coreaudio::devices() {
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
    #[cfg(not(target_os = "macos"))]
    {
        println!("null (no hardware backend on this platform)");
        Ok(true)
    }
}
