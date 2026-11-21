//! A host-side conformance run for one plugin.
//!
//! [`run`] takes a plugin through the same lifecycle the engine will (instantiate, inspect,
//! activate, process, automate, save and restore, reactivate) and records what happened at each
//! step. It is how we check a plugin we cannot ship in CI: `riddim check <plugin>` prints the
//! report on the machine that has the plugin installed.
//!
//! A passing report means *this host and this plugin got through these steps on this machine*.
//! It is evidence, not a guarantee of compatibility.

use std::fmt;

use crate::descriptor::{PluginDescriptor, PluginKind};
use crate::event::{Event, EventBuffer};
use crate::host::instantiate;
use crate::instance::{ActivateConfig, PluginInstance, PluginProcessor, ProcessMode, Tail};
use crate::process::{ProcessContext, ProcessStatus, TransportInfo};

/// How one step went.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    Fail,
    /// The step did not apply or could not be run here.
    Skip,
    /// Not a test: something worth knowing.
    Info,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    pub name: &'static str,
    pub verdict: Verdict,
    pub detail: String,
}

/// Everything [`run`] found.
#[derive(Clone, Debug, Default)]
pub struct Report {
    pub plugin: String,
    pub steps: Vec<Step>,
}

impl Report {
    pub fn passed(&self) -> bool {
        !self.steps.iter().any(|s| s.verdict == Verdict::Fail)
    }

    pub fn step(&self, name: &str) -> Option<&Step> {
        self.steps.iter().find(|s| s.name == name)
    }

    fn add(&mut self, name: &'static str, verdict: Verdict, detail: impl Into<String>) {
        self.steps.push(Step {
            name,
            verdict,
            detail: detail.into(),
        });
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "{}", self.plugin)?;
        for step in &self.steps {
            let mark = match step.verdict {
                Verdict::Pass => "PASS",
                Verdict::Fail => "FAIL",
                Verdict::Skip => "skip",
                Verdict::Info => "info",
            };
            writeln!(f, "  [{mark}] {:<22} {}", step.name, step.detail)?;
        }
        write!(
            f,
            "  => {}",
            if self.passed() { "passed" } else { "FAILED" }
        )
    }
}

/// What to run the plugin at.
#[derive(Clone, Copy, Debug)]
pub struct Options {
    pub sample_rate: f64,
    pub block: u32,
    /// Blocks to wait for an instrument to make sound after a note-on. Samplers that load
    /// asynchronously need a generous number here.
    pub sound_wait_blocks: u32,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            sample_rate: 48_000.0,
            block: 128,
            sound_wait_blocks: 400,
        }
    }
}

/// Buffers for driving a processor, allocated once.
struct Driver {
    processor: Box<dyn PluginProcessor>,
    inputs: Vec<Vec<f32>>,
    outputs: Vec<Vec<f32>>,
    events_out: EventBuffer,
    transport: TransportInfo,
    block: usize,
    rt_violations: u64,
    failed_blocks: u32,
    non_finite: bool,
    seed: u32,
}

impl Driver {
    fn new(instance: &mut dyn PluginInstance, config: &ActivateConfig) -> crate::Result<Driver> {
        let processor = instance.activate(config)?;
        let layout = instance.bus_layout();
        let block = config.max_block as usize;
        let mut transport = TransportInfo::stopped(config.sample_rate);
        transport.playing = true;
        Ok(Driver {
            processor,
            inputs: vec![vec![0.0; block]; layout.active_input_channels()],
            outputs: vec![vec![0.0; block]; layout.active_output_channels()],
            events_out: EventBuffer::with_capacity(1024),
            transport,
            block,
            rt_violations: 0,
            failed_blocks: 0,
            non_finite: false,
            seed: 0x1234_5678,
        })
    }

    /// Fills the inputs with quiet deterministic noise (for effects).
    fn fill_noise(&mut self) {
        for channel in &mut self.inputs {
            for sample in channel.iter_mut() {
                self.seed = self
                    .seed
                    .wrapping_mul(1_664_525)
                    .wrapping_add(1_013_904_223);
                *sample = ((self.seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5) * 0.25;
            }
        }
    }

    /// Runs one block and returns the peak across all outputs.
    fn block(&mut self, events: &[Event]) -> f32 {
        let frames = self.block;
        self.events_out.clear();
        let input_refs: Vec<&[f32]> = self.inputs.iter().map(|c| c.as_slice()).collect();
        let mut output_refs: Vec<&mut [f32]> =
            self.outputs.iter_mut().map(|c| c.as_mut_slice()).collect();
        let mut context = ProcessContext {
            frames,
            inputs: &input_refs,
            outputs: &mut output_refs,
            events_in: events,
            events_out: &mut self.events_out,
            transport: &self.transport,
        };
        let processor = &mut self.processor;
        let (status, violations) =
            riddim_rt::guard::rt_scope_counted(|| processor.process(&mut context));
        self.rt_violations += violations;
        if status != ProcessStatus::Ok {
            self.failed_blocks += 1;
        }
        self.transport.position_samples += frames as i64;
        self.transport.continuous_samples += frames as i64;
        self.transport.position_quarters =
            self.transport.position_samples as f64 / self.transport.sample_rate * 2.0;

        let mut peak = 0.0f32;
        for channel in &self.outputs {
            for &sample in channel {
                if !sample.is_finite() {
                    self.non_finite = true;
                } else {
                    peak = peak.max(sample.abs());
                }
            }
        }
        peak
    }
}

/// Runs the conformance steps against `descriptor`. Must be called on the thread that may own
/// plugin instances (the main thread on macOS).
pub fn run(descriptor: &PluginDescriptor, options: &Options) -> Report {
    let mut report = Report {
        plugin: format!(
            "{} [{}] by {}",
            descriptor.name, descriptor.id, descriptor.vendor
        ),
        steps: Vec::new(),
    };

    let mut instance = match instantiate(descriptor) {
        Ok(instance) => {
            report.add("instantiate", Verdict::Pass, "");
            instance
        }
        Err(e) => {
            report.add("instantiate", Verdict::Fail, e.to_string());
            return report;
        }
    };

    let layout = instance.bus_layout();
    let describe = |buses: &[crate::BusInfo]| {
        buses
            .iter()
            .map(|b| {
                format!(
                    "{}:{}ch{}",
                    b.name,
                    b.channels,
                    if b.active { "" } else { " (off)" }
                )
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    report.add(
        "buses",
        if layout.audio_outputs.is_empty() {
            Verdict::Fail
        } else {
            Verdict::Pass
        },
        format!(
            "in [{}] out [{}] events in/out {}/{}",
            describe(&layout.audio_inputs),
            describe(&layout.audio_outputs),
            layout.event_inputs,
            layout.event_outputs
        ),
    );

    let params = instance.params();
    let readable = params
        .iter()
        .filter(|p| instance.param_value(p.id).is_some())
        .count();
    let unique: std::collections::HashSet<u32> = params.iter().map(|p| p.id).collect();
    let params_ok = readable == params.len() && unique.len() == params.len();
    report.add(
        "parameters",
        if params_ok {
            Verdict::Pass
        } else {
            Verdict::Fail
        },
        format!(
            "{} parameters, {} automatable{}",
            params.len(),
            params.iter().filter(|p| p.flags.automatable).count(),
            if unique.len() != params.len() {
                ", DUPLICATE IDS"
            } else {
                ""
            }
        ),
    );

    report.add(
        "latency",
        Verdict::Info,
        format!(
            "{} samples, tail {}",
            instance.latency_samples(),
            match instance.tail() {
                Tail::Infinite => "infinite".to_string(),
                Tail::Samples(n) => format!("{n} samples"),
            }
        ),
    );
    report.add(
        "editor",
        Verdict::Info,
        if instance.has_editor() {
            "has an editor (not opened by this check)"
        } else {
            "none"
        },
    );

    let config = ActivateConfig {
        sample_rate: options.sample_rate,
        max_block: options.block,
        mode: ProcessMode::Realtime,
    };
    let mut driver = match Driver::new(instance.as_mut(), &config) {
        Ok(driver) => {
            report.add(
                "activate",
                Verdict::Pass,
                format!("{} Hz, {} frames", options.sample_rate, options.block),
            );
            driver
        }
        Err(e) => {
            report.add("activate", Verdict::Fail, e.to_string());
            return report;
        }
    };

    // Idle blocks first: nothing in, nothing asked.
    for _ in 0..16 {
        driver.block(&[]);
    }
    report.add(
        "process idle",
        if driver.failed_blocks == 0 && !driver.non_finite {
            Verdict::Pass
        } else {
            Verdict::Fail
        },
        format!("16 blocks, {} failed", driver.failed_blocks),
    );

    // Make it do its job.
    let is_instrument = descriptor.kind == PluginKind::Instrument || layout.event_inputs > 0;
    if is_instrument {
        let notes = [
            Event::note_on(0, 0, 60, 0.8),
            Event::note_on(0, 0, 64, 0.8),
            Event::note_on(0, 0, 67, 0.8),
        ];
        let mut peak = driver.block(&notes);
        let mut waited = 1;
        while peak < 1e-5 && waited < options.sound_wait_blocks {
            peak = peak.max(driver.block(&[]));
            waited += 1;
        }
        let offs = [
            Event::note_off(0, 0, 60),
            Event::note_off(0, 0, 64),
            Event::note_off(0, 0, 67),
        ];
        driver.block(&offs);
        report.add(
            "sound",
            if peak >= 1e-5 {
                Verdict::Pass
            } else {
                Verdict::Fail
            },
            format!("peak {peak:.4} after {waited} block(s) of a C major triad"),
        );
    } else {
        driver.fill_noise();
        let mut peak = 0.0f32;
        for _ in 0..32 {
            peak = peak.max(driver.block(&[]));
        }
        report.add(
            "sound",
            if driver.failed_blocks == 0 {
                Verdict::Pass
            } else {
                Verdict::Fail
            },
            format!("noise in, peak {peak:.4} out over 32 blocks"),
        );
    }

    // Automation: move the first few automatable parameters mid-block and put them back.
    let automatable: Vec<_> = params
        .iter()
        .filter(|p| p.flags.automatable && !p.flags.bypass)
        .take(8)
        .collect();
    if automatable.is_empty() {
        report.add("automation", Verdict::Skip, "no automatable parameters");
    } else {
        let half = options.block / 2;
        for param in &automatable {
            driver.block(&[
                Event::param(0, param.id, 0.0),
                Event::param(half, param.id, 1.0),
            ]);
            driver.block(&[Event::param(0, param.id, param.default)]);
        }
        report.add(
            "automation",
            if driver.failed_blocks == 0 && !driver.non_finite {
                Verdict::Pass
            } else {
                Verdict::Fail
            },
            format!(
                "swept {} parameter(s) with sample offsets",
                automatable.len()
            ),
        );
    }

    report.add(
        "output is finite",
        if driver.non_finite {
            Verdict::Fail
        } else {
            Verdict::Pass
        },
        if driver.non_finite {
            "NaN or infinity in the output"
        } else {
            ""
        },
    );
    if riddim_rt::guard::allocator_installed() {
        report.add(
            "host rt-safety",
            if driver.rt_violations == 0 {
                Verdict::Pass
            } else {
                Verdict::Fail
            },
            format!(
                "{} heap operation(s) on the host side of process",
                driver.rt_violations
            ),
        );
    } else {
        report.add(
            "host rt-safety",
            Verdict::Skip,
            "the checking allocator is not installed",
        );
    }
    instance.deactivate(driver.processor);

    // State: save, restore into a fresh instance, save again.
    match instance.save_state() {
        Err(e) => report.add("state save", Verdict::Fail, e.to_string()),
        Ok(saved) => {
            report.add(
                "state save",
                Verdict::Pass,
                format!("{} bytes", saved.data.len()),
            );
            match instantiate(descriptor) {
                Err(e) => report.add("state restore", Verdict::Fail, e.to_string()),
                Ok(mut second) => match second.restore_state(&saved) {
                    Err(e) => report.add("state restore", Verdict::Fail, e.to_string()),
                    Ok(()) => {
                        report.add("state restore", Verdict::Pass, "into a fresh instance");
                        match second.save_state() {
                            Err(e) => report.add("state round trip", Verdict::Fail, e.to_string()),
                            Ok(resaved) if resaved == saved => {
                                report.add("state round trip", Verdict::Pass, "identical blob");
                            }
                            Ok(resaved) => {
                                let differing = saved
                                    .data
                                    .iter()
                                    .zip(&resaved.data)
                                    .filter(|(a, b)| a != b)
                                    .count();
                                report.add(
                                    "state round trip",
                                    Verdict::Fail,
                                    format!(
                                        "blob differs after restore: {} -> {} bytes, {} byte(s) \
                                         changed in the common prefix",
                                        saved.data.len(),
                                        resaved.data.len(),
                                        differing
                                    ),
                                );
                            }
                        }
                    }
                },
            }
        }
    }

    // Reactivate at another rate and block size, as a device change would.
    let other = ActivateConfig {
        sample_rate: 44_100.0,
        max_block: 512,
        mode: ProcessMode::Realtime,
    };
    match Driver::new(instance.as_mut(), &other) {
        Err(e) => report.add("reactivate", Verdict::Fail, e.to_string()),
        Ok(mut driver) => {
            for _ in 0..4 {
                driver.block(&[]);
            }
            report.add(
                "reactivate",
                if driver.failed_blocks == 0 && !driver.non_finite {
                    Verdict::Pass
                } else {
                    Verdict::Fail
                },
                "44100 Hz, 512 frames",
            );
            instance.deactivate(driver.processor);
        }
    }

    let mut pending = Vec::new();
    instance.poll_events(&mut pending);
    report.add(
        "notifications",
        Verdict::Info,
        format!("{} pending at the end", pending.len()),
    );
    report
}
