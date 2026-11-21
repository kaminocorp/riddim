//! Turning what the user typed into a plugin descriptor.

use std::path::{Path, PathBuf};

use riddim_plugin::scan::{default_targets, ScanCache, ScanOutcome, ScanTarget, Scanner};
use riddim_plugin::{PluginDescriptor, PluginFormat, PluginId, PluginKind};

/// Scans `targets` through the on-disk cache, reporting what did not scan on stderr.
///
/// Bundles that have not changed since the last run are not loaded again, and bundles on the
/// blocklist are not touched. `rescan` ignores what is cached (and gives blocked bundles another
/// chance).
pub fn scan_targets(
    scanner: &Scanner,
    targets: &[ScanTarget],
    rescan: bool,
) -> Vec<PluginDescriptor> {
    let cache_file = ScanCache::default_path();
    let mut cache = ScanCache::load(&cache_file);
    if rescan {
        for target in targets {
            if let Some(path) = &target.path {
                cache.forget(path);
            }
        }
    }
    let report = scanner.scan_cached(targets, &mut cache, Some(&cache_file), |_, _, _| {});
    if let Err(error) = cache.save(&cache_file) {
        eprintln!(
            "warning: could not save the scan cache to {}: {error}",
            cache_file.display()
        );
    }
    for (target, outcome) in &report.failures {
        let what = target
            .path
            .as_ref()
            .map_or_else(|| target.format.to_string(), |p| p.display().to_string());
        let why = match outcome {
            ScanOutcome::Failed(why) => why.clone(),
            ScanOutcome::Crashed(why) => format!("the scanner crashed ({why})"),
            ScanOutcome::TimedOut => "the scanner timed out".to_string(),
            ScanOutcome::Ok(_) => continue,
        };
        eprintln!("warning: {what} did not scan: {why}");
    }
    report.plugins
}

/// Scans one bundle (out of process, or from the cache).
pub fn scan_bundle(scanner: &Scanner, path: &Path) -> Result<Vec<PluginDescriptor>, String> {
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let target = ScanTarget {
        format: PluginFormat::Vst3,
        path: Some(path.clone()),
    };
    let plugins = scan_targets(scanner, &[target], false);
    if plugins.is_empty() {
        Err(format!("{} has no usable plugins", path.display()))
    } else {
        Ok(plugins)
    }
}

/// Scans everything in the default locations plus `extra_dirs`.
pub fn scan_everything(
    scanner: &Scanner,
    extra_dirs: &[PathBuf],
    rescan: bool,
) -> Vec<PluginDescriptor> {
    scan_targets(scanner, &default_targets(extra_dirs), rescan)
}

/// Finds the plugin the user means.
///
/// `spec` may be:
/// * a path to a `.vst3` bundle, optionally followed by `#name` to pick one plugin from it;
/// * a full id such as `vst3:<32 hex digits>` or `au:aumu:dls :appl`;
/// * part of a plugin name (case-insensitive), which must match exactly one scanned plugin.
pub fn resolve(
    spec: &str,
    scanner: &Scanner,
    extra_dirs: &[PathBuf],
) -> Result<PluginDescriptor, String> {
    let (path_part, pick) = match spec.split_once('#') {
        Some((path, pick)) => (path, Some(pick)),
        None => (spec, None),
    };
    if Path::new(path_part).exists() {
        let plugins = scan_bundle(scanner, Path::new(path_part))?;
        return pick_one(plugins, pick.unwrap_or(""), spec);
    }

    if let Ok(id) = spec.parse::<PluginId>() {
        if id.format == PluginFormat::AudioUnit {
            // Audio Units are looked up in the system registry at instantiation; no scan needed.
            return Ok(PluginDescriptor {
                id,
                name: spec.to_string(),
                vendor: String::new(),
                version: String::new(),
                kind: if spec.starts_with("au:aumu") {
                    PluginKind::Instrument
                } else {
                    PluginKind::Effect
                },
                categories: Vec::new(),
                path: None,
            });
        }
        return scan_everything(scanner, extra_dirs, false)
            .into_iter()
            .find(|d| d.id == id)
            .ok_or_else(|| format!("no installed plugin has the id {id}"));
    }

    pick_one(scan_everything(scanner, extra_dirs, false), spec, spec)
}

fn pick_one(
    plugins: Vec<PluginDescriptor>,
    needle: &str,
    spec: &str,
) -> Result<PluginDescriptor, String> {
    let needle = needle.to_lowercase();
    let mut matches: Vec<PluginDescriptor> = plugins
        .into_iter()
        .filter(|d| d.name.to_lowercase().contains(&needle))
        .collect();
    // An exact name beats a substring.
    if let Some(exact) = matches.iter().position(|d| d.name.to_lowercase() == needle) {
        return Ok(matches.swap_remove(exact));
    }
    match matches.len() {
        0 => Err(format!("no plugin matches '{spec}'")),
        1 => Ok(matches.remove(0)),
        _ => {
            let mut message = format!("'{spec}' is ambiguous; it matches:\n");
            for d in &matches {
                message.push_str(&format!("  {}  ({})\n", d.name, d.id));
            }
            message.push_str("use the id, or path#name, to pick one");
            Err(message)
        }
    }
}
