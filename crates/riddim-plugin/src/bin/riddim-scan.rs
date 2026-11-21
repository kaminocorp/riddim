//! The plugin scanner child process.
//!
//! Usage: `riddim-scan <format> [bundle-path]`
//!
//! Loads one plugin bundle (or, for Audio Units, enumerates the system registry), prints one
//! JSON message per line on stdout, and exits. It is allowed to crash: that is why it exists.

use std::io::Write;
use std::path::PathBuf;

use riddim_plugin::scan::{run_scanner, ScanMessage, ScanTarget};

fn main() {
    let mut args = std::env::args_os().skip(1);
    let format = args
        .next()
        .and_then(|f| f.to_str().and_then(|f| f.parse().ok()));
    let path = args.next().map(PathBuf::from);
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    let Some(format) = format else {
        let message = ScanMessage::Error {
            message: "usage: riddim-scan <vst3|au|clap> [path]".into(),
        };
        let _ = writeln!(out, "{}", serde_json::to_string(&message).unwrap());
        std::process::exit(2);
    };
    let code = run_scanner(&ScanTarget { format, path }, &mut out);
    let _ = out.flush();
    // Skip static destructors: plugins left threads and globals behind, and unwinding through
    // them at exit is how scanners crash *after* doing their job.
    std::process::exit(code);
}
