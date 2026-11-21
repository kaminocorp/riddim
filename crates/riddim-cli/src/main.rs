//! `riddim`: a headless tool for exercising the engine.
//!
//! Hosting-level commands (`scan`, `info`, `check`, `state`, `editor-cycle`) talk to plugins
//! directly. Everything that makes sound (`play`, `render`, `session ...`) runs the engine.

mod commands;
mod resolve;
mod session;
mod ui;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};

// Count heap traffic inside real-time sections in every build of the tool, so "audio-thread
// heap operations: 0" in its output is a measurement and not an assumption.
#[global_allocator]
static ALLOC: riddim::rt::RtCheckAllocator = riddim::rt::RtCheckAllocator::system();

#[derive(Parser)]
#[command(
    name = "riddim",
    version,
    about = "Headless tool for the Riddim audio engine"
)]
struct Cli {
    /// Extra directory to search for VST3 bundles (repeatable).
    #[arg(long = "vst3-dir", global = true, value_name = "DIR")]
    vst3_dirs: Vec<PathBuf>,

    /// Path to the riddim-scan executable (default: next to this one, or $RIDDIM_SCAN).
    #[arg(long, global = true, value_name = "PATH")]
    scanner: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List installed plugins (each bundle is probed in a separate process).
    Scan {
        /// Print descriptors as JSON instead of a table.
        #[arg(long)]
        json: bool,
        /// Probe every bundle again, ignoring the cache and the blocklist.
        #[arg(long)]
        rescan: bool,
        /// List the bundles that crashed, hung or failed their scan and are being skipped.
        #[arg(long)]
        blocklist: bool,
        /// Take a bundle off the blocklist so it is scanned again.
        #[arg(long, value_name = "BUNDLE")]
        unblock: Option<PathBuf>,
    },
    /// Show a plugin's buses and parameters.
    Info {
        plugin: String,
        /// List every parameter instead of the first 40.
        #[arg(long)]
        all: bool,
    },
    /// Run the host conformance steps against one or more plugins.
    Check {
        plugins: Vec<String>,
        #[arg(long, default_value_t = 48_000.0)]
        rate: f64,
        #[arg(long, default_value_t = 128)]
        block: u32,
    },
    /// Render a plugin playing a MIDI file (or the test arpeggio), offline, to a WAV file.
    Render {
        plugin: String,
        #[arg(long, short)]
        out: PathBuf,
        /// A Standard MIDI File to play. Without it, the built-in arpeggio loops.
        #[arg(long)]
        midi: Option<PathBuf>,
        #[arg(long, default_value_t = 4.0)]
        seconds: f64,
        #[arg(long, default_value_t = 48_000.0)]
        rate: f64,
        #[arg(long, default_value_t = 128)]
        block: u32,
        /// Restore this state file before rendering.
        #[arg(long)]
        state: Option<PathBuf>,
    },
    /// Save, restore and compare plugin state.
    State {
        #[command(subcommand)]
        action: StateAction,
    },
    /// Play a plugin live through the engine: device out, MIDI keyboards in, editor open.
    Play {
        plugin: String,
        #[command(flatten)]
        device: DeviceArgs,
        /// Stop after this many seconds (default: until Enter is pressed or the editor closes).
        #[arg(long)]
        seconds: Option<f64>,
        /// Loop the built-in test arpeggio.
        #[arg(long)]
        sequence: bool,
        /// Play this Standard MIDI File.
        #[arg(long)]
        midi: Option<PathBuf>,
        /// Do not open the plugin's editor.
        #[arg(long)]
        no_editor: bool,
        /// Restore this state file after loading.
        #[arg(long)]
        state: Option<PathBuf>,
        /// Save the plugin's state to this file on exit.
        #[arg(long)]
        save_state: Option<PathBuf>,
    },
    /// Small projects: build one, play it, record a take into it, render it.
    Session {
        #[command(subcommand)]
        action: SessionAction,
    },
    /// Measure recording alignment through a cable from an output to an input.
    Loopback {
        #[command(flatten)]
        device: DeviceArgs,
        /// Device input the cable returns on, e.g. "1" or "1-2".
        #[arg(long, default_value = "1")]
        input: String,
        #[arg(long, default_value_t = 5.0)]
        seconds: f64,
    },
    /// Open, resize and close a plugin's editor repeatedly (macOS).
    EditorCycle {
        plugin: String,
        #[arg(long, default_value_t = 10)]
        count: u32,
    },
    /// List audio devices.
    Devices,
}

#[derive(Args, Clone, Copy)]
struct DeviceArgs {
    #[arg(long, default_value_t = 48_000.0)]
    rate: f64,
    #[arg(long, default_value_t = 128)]
    buffer: u32,
    /// Use the silent, timer-driven null device instead of audio hardware.
    #[arg(long)]
    null_device: bool,
    /// With --null-device: feed the output back to the input after this many frames.
    #[arg(long, value_name = "FRAMES")]
    loopback: Option<u32>,
}

impl DeviceArgs {
    fn options(self) -> session::DeviceOptions {
        session::DeviceOptions {
            rate: self.rate,
            buffer: self.buffer,
            null_device: self.null_device,
            loopback: self.loopback,
            capture: false,
        }
    }
}

#[derive(Subcommand)]
enum SessionAction {
    /// Create a project file with an instrument track and, optionally, an armed audio track.
    New {
        project: PathBuf,
        /// Plugin for the instrument track.
        #[arg(long)]
        instrument: Option<String>,
        /// Standard MIDI File for the instrument track.
        #[arg(long)]
        midi: Option<PathBuf>,
        /// Put the test arpeggio on the instrument track.
        #[arg(long)]
        sequence: bool,
        /// Add an audio track armed to record these device inputs, e.g. "1" or "1-2".
        #[arg(long, value_name = "INPUTS")]
        audio_input: Option<String>,
        /// How many armed audio tracks to create on that input.
        #[arg(long, default_value_t = 1)]
        audio_tracks: u8,
        /// Bars of metronome before recording starts.
        #[arg(long, default_value_t = 0)]
        count_in: u8,
        /// Insert this effect on the instrument track (repeatable, in order).
        #[arg(long, value_name = "PLUGIN")]
        insert: Vec<String>,
        /// Put this effect on a bus of its own, fed by a send from the instrument track.
        #[arg(long, value_name = "PLUGIN")]
        send_effect: Option<String>,
        /// Automate the instrument's fader from silence to full over this many beats.
        #[arg(long, value_name = "BEATS")]
        fade_in: Option<u32>,
    },
    /// Describe a project file.
    Info { project: PathBuf },
    /// Play a project.
    Play {
        project: PathBuf,
        #[command(flatten)]
        device: DeviceArgs,
        #[arg(long)]
        seconds: Option<f64>,
        /// Open the instrument's editor.
        #[arg(long)]
        editor: bool,
        /// On exit, save the plugins' current settings back into the project.
        #[arg(long)]
        save: bool,
    },
    /// Play a project and record its armed tracks; the takes and plugin states are saved back.
    Record {
        project: PathBuf,
        #[command(flatten)]
        device: DeviceArgs,
        #[arg(long)]
        seconds: Option<f64>,
        #[arg(long)]
        editor: bool,
    },
    /// Render a project offline to a WAV file and print a fingerprint of the audio.
    Render {
        project: PathBuf,
        #[arg(long, short)]
        out: PathBuf,
        #[arg(long, default_value_t = 10.0)]
        seconds: f64,
        #[arg(long, default_value_t = 48_000.0)]
        rate: f64,
        #[arg(long, default_value_t = 128)]
        block: u32,
        /// Extra seconds after the end, for reverb and release tails.
        #[arg(long, default_value_t = 0.0)]
        tail: f64,
        /// Render only this track id, as a stem (repeatable; see `session info` for ids).
        #[arg(long, value_name = "TRACK")]
        stem: Vec<u64>,
    },
}

/// Parses "1" or "1-2" (1-based device inputs) into a first channel and a count.
fn parse_inputs(text: &str) -> Result<(u16, u8), String> {
    let bad = || format!("'{text}' is not an input like \"1\" or \"1-2\"");
    let (first, last) = match text.split_once('-') {
        Some((first, last)) => (
            first.parse::<u16>().map_err(|_| bad())?,
            last.parse::<u16>().map_err(|_| bad())?,
        ),
        None => {
            let only = text.parse::<u16>().map_err(|_| bad())?;
            (only, only)
        }
    };
    if first == 0 || last < first || last - first > 1 {
        return Err(bad());
    }
    Ok((first - 1, (last - first + 1) as u8))
}

#[derive(Subcommand)]
enum StateAction {
    /// Save a plugin's current (default) state to a file.
    Save {
        plugin: String,
        #[arg(long, short)]
        out: PathBuf,
    },
    /// Check save -> restore -> save gives an identical blob, starting from a state file if given.
    Roundtrip {
        plugin: String,
        #[arg(long)]
        state: Option<PathBuf>,
    },
}

/// Lets `riddim scan | head` end quietly instead of panicking on a closed pipe.
#[cfg(unix)]
fn restore_default_sigpipe() {
    extern "C" {
        fn signal(signum: i32, handler: usize) -> usize;
    }
    const SIGPIPE: i32 = 13;
    const SIG_DFL: usize = 0;
    // SAFETY: resetting a signal disposition to its default before any threads exist.
    unsafe { signal(SIGPIPE, SIG_DFL) };
}

#[cfg(not(unix))]
fn restore_default_sigpipe() {}

fn main() -> ExitCode {
    restore_default_sigpipe();
    let cli = Cli::parse();
    let context = commands::Context {
        vst3_dirs: cli.vst3_dirs,
        scanner: cli.scanner,
    };
    let result = match cli.command {
        Command::Scan {
            json,
            rescan,
            blocklist,
            unblock,
        } => commands::scan(
            &context,
            commands::ScanOptions {
                json,
                rescan,
                blocklist,
                unblock,
            },
        ),
        Command::Info { plugin, all } => commands::info(&context, &plugin, all),
        Command::Check {
            plugins,
            rate,
            block,
        } => commands::check(&context, &plugins, rate, block),
        Command::Render {
            plugin,
            out,
            midi,
            seconds,
            rate,
            block,
            state,
        } => session::render(
            &context,
            &plugin,
            session::RenderOptions {
                out,
                seconds,
                rate,
                buffer: block,
                state,
                midi,
            },
        ),
        Command::State {
            action: StateAction::Save { plugin, out },
        } => commands::state_save(&context, &plugin, &out),
        Command::State {
            action: StateAction::Roundtrip { plugin, state },
        } => commands::state_roundtrip(&context, &plugin, state.as_deref()),
        Command::Play {
            plugin,
            device,
            seconds,
            sequence,
            midi,
            no_editor,
            state,
            save_state,
        } => session::play(
            &context,
            &plugin,
            session::PlayOptions {
                device: device.options(),
                seconds,
                sequence,
                midi,
                editor: !no_editor,
                state,
                save_state,
            },
        ),
        Command::Session { action } => match action {
            SessionAction::New {
                project,
                instrument,
                midi,
                sequence,
                audio_input,
                audio_tracks,
                count_in,
                insert,
                send_effect,
                fade_in,
            } => match audio_input.as_deref().map(parse_inputs).transpose() {
                Ok(audio_input) => session::session_new(
                    &context,
                    &project,
                    session::NewOptions {
                        instrument,
                        midi,
                        sequence,
                        audio_input,
                        audio_tracks,
                        count_in,
                        inserts: insert,
                        send_effect,
                        fade_in,
                    },
                ),
                Err(message) => Err(message),
            },
            SessionAction::Info { project } => session::session_info(&project),
            SessionAction::Play {
                project,
                device,
                seconds,
                editor,
                save,
            } => session::session_play(&project, device.options(), seconds, editor, save),
            SessionAction::Record {
                project,
                device,
                seconds,
                editor,
            } => session::session_record(&project, device.options(), seconds, editor),
            SessionAction::Render {
                project,
                out,
                seconds,
                rate,
                block,
                tail,
                stem,
            } => session::session_render(
                &project,
                session::SessionRenderOptions {
                    out,
                    seconds,
                    rate,
                    buffer: block,
                    tail,
                    stem,
                },
            ),
        },
        Command::Loopback {
            device,
            input,
            seconds,
        } => match parse_inputs(&input) {
            Ok(input) => session::loopback(device.options(), input, seconds),
            Err(message) => Err(message),
        },
        Command::EditorCycle { plugin, count } => commands::editor_cycle(&context, &plugin, count),
        Command::Devices => commands::devices(),
    };
    match result {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::from(2)
        }
    }
}
