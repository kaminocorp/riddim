//! The plugin bridge child process.
//!
//! Usage: `riddim-bridge <plugin-descriptor-json>`
//!
//! Loads one plugin and serves it to the host that started this process: requests and replies
//! as JSON lines on standard input and output, audio through shared memory (see
//! `riddim_plugin::bridge`). If the plugin takes this process down, that is all it takes down.

#[cfg(unix)]
fn main() {
    let Some(descriptor) = std::env::args().nth(1) else {
        eprintln!("usage: riddim-bridge <plugin-descriptor-json>");
        std::process::exit(2);
    };
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let code = riddim_plugin::bridge::serve(&descriptor, stdin.lock(), stdout.lock());
    // Skip static destructors, as the scanner does: plugins leave threads and globals behind.
    std::process::exit(code);
}

#[cfg(not(unix))]
fn main() {
    eprintln!("riddim-bridge is not available on this platform");
    std::process::exit(2);
}
