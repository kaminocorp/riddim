//! Out-of-process plugin scanning.
//!
//! Probing a plugin means loading its binary and running its code, and plugins crash. So the
//! host never does it itself: it spawns the `riddim-scan` binary once per plugin bundle and
//! reads descriptors back as JSON lines. If the child dies, hangs or prints nonsense, only that
//! one bundle is marked as failed.
//!
//! The wire format is one [`ScanMessage`] per line on the child's stdout.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::descriptor::{PluginDescriptor, PluginFormat};
use crate::error::{PluginError, Result};
use crate::format;

/// Name of the scanner executable (without a platform suffix).
pub const SCANNER_BINARY: &str = "riddim-scan";

/// Environment variable that overrides where the scanner executable is looked for.
pub const SCANNER_ENV: &str = "RIDDIM_SCAN";

/// One line of scanner output.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ScanMessage {
    Plugin { descriptor: PluginDescriptor },
    /// The bundle loaded but could not be scanned.
    Error { message: String },
    /// Always the last line of a scan that ran to the end.
    Done { count: usize },
}

/// What scanning one target produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScanOutcome {
    Ok(Vec<PluginDescriptor>),
    /// The scanner reported a problem and exited normally.
    Failed(String),
    /// The scanner process died (signal, abort, non-zero exit without a report).
    Crashed(String),
    /// The scanner did not finish in time and was killed.
    TimedOut,
}

impl ScanOutcome {
    pub fn is_ok(&self) -> bool {
        matches!(self, ScanOutcome::Ok(_))
    }
}

/// A scan target: one bundle for file-based formats, or the whole registry for Audio Units.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ScanTarget {
    pub format: PluginFormat,
    pub path: Option<PathBuf>,
}

/// The result of scanning many targets.
#[derive(Clone, Debug, Default)]
pub struct ScanReport {
    pub plugins: Vec<PluginDescriptor>,
    /// Targets that did not scan cleanly, with what happened.
    pub failures: Vec<(ScanTarget, ScanOutcome)>,
}

// ---------------------------------------------------------------------------------------------
// Child side
// ---------------------------------------------------------------------------------------------

/// Scans `target` in this process. Only the scanner binary should call this.
pub fn scan_in_process(target: &ScanTarget) -> Result<Vec<PluginDescriptor>> {
    match target.format {
        PluginFormat::Vst3 => {
            let path = target
                .path
                .as_deref()
                .ok_or_else(|| PluginError::Scan("a VST3 scan needs a bundle path".into()))?;
            format::vst3::scan_bundle(path)
        }
        #[cfg(target_os = "macos")]
        PluginFormat::AudioUnit => format::au::scan(),
        #[cfg(not(target_os = "macos"))]
        PluginFormat::AudioUnit => Err(PluginError::Unsupported("Audio Units need macOS")),
        PluginFormat::Clap => Err(PluginError::Unsupported("CLAP scanning arrives after v1")),
        PluginFormat::Internal => Ok(Vec::new()),
    }
}

/// The scanner binary's whole job: scan, write messages to `out`, return the exit code.
pub fn run_scanner(target: &ScanTarget, out: &mut dyn Write) -> i32 {
    let mut emit = |message: &ScanMessage| {
        let line = serde_json::to_string(message).expect("scan messages always serialise");
        let _ = writeln!(out, "{line}");
        let _ = out.flush();
    };
    match scan_in_process(target) {
        Ok(descriptors) => {
            let count = descriptors.len();
            for descriptor in descriptors {
                emit(&ScanMessage::Plugin { descriptor });
            }
            emit(&ScanMessage::Done { count });
            0
        }
        Err(error) => {
            emit(&ScanMessage::Error { message: error.to_string() });
            1
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Host side
// ---------------------------------------------------------------------------------------------

/// Drives the scanner child process.
#[derive(Clone, Debug)]
pub struct Scanner {
    executable: PathBuf,
    timeout: Duration,
    env: Vec<(String, String)>,
}

impl Scanner {
    /// Uses the scanner at `executable`.
    pub fn new(executable: impl Into<PathBuf>) -> Self {
        Self { executable: executable.into(), timeout: Duration::from_secs(120), env: Vec::new() }
    }

    /// Finds the scanner: `$RIDDIM_SCAN`, else next to the running executable.
    pub fn locate() -> Result<Self> {
        if let Some(path) = std::env::var_os(SCANNER_ENV) {
            return Ok(Self::new(path));
        }
        let name = format!("{SCANNER_BINARY}{}", std::env::consts::EXE_SUFFIX);
        let here = std::env::current_exe()?;
        // Next to the executable, or one level up (cargo puts test binaries in `deps/`).
        let candidates = here.ancestors().skip(1).take(2).map(|dir| dir.join(&name));
        for candidate in candidates {
            if candidate.is_file() {
                return Ok(Self::new(candidate));
            }
        }
        Err(PluginError::Scan(format!(
            "could not find the {name} executable; set {SCANNER_ENV} to its path"
        )))
    }

    /// How long one target may take before the scanner is killed.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Sets an environment variable for the scanner child only.
    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    pub fn executable(&self) -> &Path {
        &self.executable
    }

    /// Scans one target in a fresh child process.
    pub fn scan(&self, target: &ScanTarget) -> ScanOutcome {
        let mut command = Command::new(&self.executable);
        command.arg(target.format.as_str());
        if let Some(path) = &target.path {
            command.arg(path);
        }
        command.envs(self.env.iter().map(|(k, v)| (k, v)));
        command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(e) => {
                return ScanOutcome::Failed(format!(
                    "could not start {}: {e}",
                    self.executable.display()
                ))
            }
        };

        // Read both pipes on their own threads so a chatty plugin cannot block the child.
        let stdout = child.stdout.take().expect("stdout was piped");
        let stderr = child.stderr.take().expect("stderr was piped");
        let out_reader = std::thread::spawn(move || {
            BufReader::new(stdout).lines().map_while(|l| l.ok()).collect::<Vec<String>>()
        });
        let err_reader = std::thread::spawn(move || {
            let mut text = String::new();
            let _ = BufReader::new(stderr).take(64 * 1024).read_to_string(&mut text);
            text
        });

        let deadline = Instant::now() + self.timeout;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(5)),
                Err(e) => return ScanOutcome::Crashed(format!("lost the scanner process: {e}")),
            }
        };
        let lines = out_reader.join().unwrap_or_default();
        let stderr_text = err_reader.join().unwrap_or_default();
        let Some(status) = status else { return ScanOutcome::TimedOut };

        let mut plugins = Vec::new();
        let mut reported_error = None;
        let mut done = false;
        for line in &lines {
            // Plugins print to stdout too; anything that is not one of our messages is noise.
            match serde_json::from_str::<ScanMessage>(line) {
                Ok(ScanMessage::Plugin { descriptor }) => plugins.push(descriptor),
                Ok(ScanMessage::Error { message }) => reported_error = Some(message),
                Ok(ScanMessage::Done { .. }) => done = true,
                Err(_) => {}
            }
        }

        if let Some(message) = reported_error {
            return ScanOutcome::Failed(message);
        }
        if status.success() && done {
            return ScanOutcome::Ok(plugins);
        }
        let mut detail = describe_exit(&status);
        let tail = stderr_text.trim();
        if !tail.is_empty() {
            let last = tail.lines().last().unwrap_or(tail);
            detail.push_str(&format!(" ({last})"));
        }
        ScanOutcome::Crashed(detail)
    }

    /// Scans every target, one child process each, and gathers the results.
    pub fn scan_all(
        &self,
        targets: &[ScanTarget],
        mut progress: impl FnMut(&ScanTarget, &ScanOutcome),
    ) -> ScanReport {
        let mut report = ScanReport::default();
        for target in targets {
            let outcome = self.scan(target);
            progress(target, &outcome);
            match outcome {
                ScanOutcome::Ok(plugins) => report.plugins.extend(plugins),
                other => report.failures.push((target.clone(), other)),
            }
        }
        report
    }
}

/// Everything there is to scan in the default locations (plus `extra_vst3_dirs`).
pub fn default_targets(extra_vst3_dirs: &[PathBuf]) -> Vec<ScanTarget> {
    let mut dirs = format::vst3::default_search_paths();
    dirs.extend(extra_vst3_dirs.iter().cloned());
    let mut targets: Vec<ScanTarget> = format::vst3::find_bundles(&dirs)
        .into_iter()
        .map(|path| ScanTarget { format: PluginFormat::Vst3, path: Some(path) })
        .collect();
    if cfg!(target_os = "macos") {
        targets.push(ScanTarget { format: PluginFormat::AudioUnit, path: None });
    }
    targets
}

#[cfg(unix)]
fn describe_exit(status: &std::process::ExitStatus) -> String {
    use std::os::unix::process::ExitStatusExt;
    match (status.code(), status.signal()) {
        (_, Some(signal)) => format!("killed by signal {signal}"),
        (Some(code), _) => format!("exited with code {code}"),
        _ => "exited abnormally".to_string(),
    }
}

#[cfg(not(unix))]
fn describe_exit(status: &std::process::ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("exited with code {code}"),
        None => "exited abnormally".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::descriptor::{PluginId, PluginKind};

    #[test]
    fn messages_are_single_json_lines() {
        let message = ScanMessage::Plugin {
            descriptor: PluginDescriptor {
                id: PluginId::new(PluginFormat::Vst3, "AB"),
                name: "N\new\"line".into(),
                vendor: "V".into(),
                version: "1".into(),
                kind: PluginKind::Effect,
                categories: vec![],
                path: None,
            },
        };
        let line = serde_json::to_string(&message).unwrap();
        assert!(!line.contains('\n'));
        assert_eq!(serde_json::from_str::<ScanMessage>(&line).unwrap(), message);
    }

    #[test]
    fn missing_scanner_is_a_failure_not_a_panic() {
        let scanner = Scanner::new("/nonexistent/riddim-scan");
        let target = ScanTarget { format: PluginFormat::Vst3, path: Some("/tmp/x.vst3".into()) };
        assert!(matches!(scanner.scan(&target), ScanOutcome::Failed(_)));
    }

    #[test]
    fn scanning_a_missing_bundle_reports_an_error_message() {
        let target =
            ScanTarget { format: PluginFormat::Vst3, path: Some("/nonexistent/Nope.vst3".into()) };
        let mut out = Vec::new();
        assert_eq!(run_scanner(&target, &mut out), 1);
        let text = String::from_utf8(out).unwrap();
        assert!(matches!(
            serde_json::from_str::<ScanMessage>(text.trim()).unwrap(),
            ScanMessage::Error { .. }
        ));
    }
}
