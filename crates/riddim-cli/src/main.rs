//! `riddim`: a headless tool for exercising the engine's parts.
//!
//! Phase 1 scope: scan plugins, inspect and check them, render a test sequence offline, round-
//! trip their state, and play one live through the audio device with its editor open.

mod commands;
mod resolve;
mod spike;
mod wav;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

// Count heap traffic inside real-time sections in every build of the tool, so "audio-thread
// heap operations: 0" in its output is a measurement and not an assumption.
#[global_allocator]
static ALLOC: riddim_rt::guard::RtCheckAllocator = riddim_rt::guard::RtCheckAllocator::system();

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
    /// Render a test sequence through a plugin, offline, to a WAV file.
    Render {
        plugin: String,
        #[arg(long, short)]
        out: PathBuf,
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
    /// Play a plugin live: audio device out, MIDI keyboards in, editor window open.
    Play {
        plugin: String,
        #[arg(long, default_value_t = 48_000.0)]
        rate: f64,
        #[arg(long, default_value_t = 128)]
        buffer: u32,
        /// Stop after this many seconds (default: until Enter is pressed or the editor closes).
        #[arg(long)]
        seconds: Option<f64>,
        /// Play the built-in test arpeggio.
        #[arg(long)]
        sequence: bool,
        /// Do not open the plugin's editor.
        #[arg(long)]
        no_editor: bool,
        /// Use the silent null device instead of real audio hardware.
        #[arg(long)]
        null_device: bool,
        /// Restore this state file after loading.
        #[arg(long)]
        state: Option<PathBuf>,
        /// Save the plugin's state to this file on exit.
        #[arg(long)]
        save_state: Option<PathBuf>,
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
        Command::Scan { json } => commands::scan(&context, json),
        Command::Info { plugin, all } => commands::info(&context, &plugin, all),
        Command::Check {
            plugins,
            rate,
            block,
        } => commands::check(&context, &plugins, rate, block),
        Command::Render {
            plugin,
            out,
            seconds,
            rate,
            block,
            state,
        } => commands::render(
            &context,
            &plugin,
            &out,
            seconds,
            rate,
            block,
            state.as_deref(),
        ),
        Command::State {
            action: StateAction::Save { plugin, out },
        } => commands::state_save(&context, &plugin, &out),
        Command::State {
            action: StateAction::Roundtrip { plugin, state },
        } => commands::state_roundtrip(&context, &plugin, state.as_deref()),
        Command::Play {
            plugin,
            rate,
            buffer,
            seconds,
            sequence,
            no_editor,
            null_device,
            state,
            save_state,
        } => commands::play(
            &context,
            &plugin,
            commands::PlayOptions {
                rate,
                buffer,
                seconds,
                sequence,
                editor: !no_editor,
                null_device,
                state,
                save_state,
            },
        ),
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
