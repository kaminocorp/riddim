//! The plugin host: the part of the engine that lives on the main thread.
//!
//! Plugin instances are tied to the thread that created them, and editors to the application's
//! main thread. So instances live here, in a [`MainThread`] object that is pumped either by the
//! application's own event loop or, for headless use, by a thread the engine starts. The
//! control thread talks to it only through messages.

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::sync::Arc;

use riddim_graph::model_graph::PluginPorts;
use riddim_model::{PluginRef, SlotId};
use riddim_plugin::{
    ActivateConfig, BusDirection, BusKind, EventBuffer, PluginDescriptor, PluginEvent,
    PluginFormat, PluginId, PluginInstance, PluginKind, PluginProcessor, ProcessContext,
    ProcessMode, StateBlob, TransportInfo,
};

/// Which of a plugin's optional buses the project uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(crate) struct BusRequest {
    /// Output buses to switch on beyond the main one: all of them up to the highest in use,
    /// so that a bus's place among the active outputs is its own index.
    pub extra_outputs: u8,
    /// Whether the first input bus after the main one (the sidechain) is fed.
    pub sidechain: bool,
}

pub(crate) enum HostRequest {
    /// `serial` names this instance from now on. A slot can be emptied and filled again
    /// before the first instance has been let go of, so the slot alone does not say which
    /// instance a later request means.
    Load {
        slot: SlotId,
        serial: u64,
        plugin: PluginRef,
        state: Option<Arc<Vec<u8>>>,
        config: ActivateConfig,
        buses: BusRequest,
        /// Host it in a child process.
        bridged: bool,
    },
    /// Deactivate a loaded plugin and activate it again with other buses or another block
    /// size. The instance, and with it everything the user set up in it, stays.
    Restart {
        slot: SlotId,
        serial: u64,
        processor: Option<Box<dyn PluginProcessor>>,
        config: ActivateConfig,
        buses: BusRequest,
    },
    /// Deactivate (with the processor, if the audio side had one) and destroy the instance.
    Unload {
        serial: u64,
        processor: Option<Box<dyn PluginProcessor>>,
    },
    SaveState {
        slot: SlotId,
    },
    /// Mirror a value the processor already has into the plugin's control side.
    NotifyParam {
        slot: SlotId,
        id: u32,
        value: f64,
    },
    Shutdown,
}

pub(crate) struct Loaded {
    pub processor: Box<dyn PluginProcessor>,
    pub ports: PluginPorts,
    pub parameters: usize,
}

pub(crate) enum HostReply {
    Loaded {
        slot: SlotId,
        serial: u64,
        result: Result<Loaded, String>,
        /// This answers a [`HostRequest::Restart`], not a first load.
        restarted: bool,
    },
    State {
        slot: SlotId,
        result: Result<Vec<u8>, String>,
    },
    Event {
        slot: SlotId,
        event: PluginEvent,
    },
    Latency {
        slot: SlotId,
        latency: u32,
    },
    Unloaded,
}

fn descriptor_for(plugin: &PluginRef) -> Result<PluginDescriptor, String> {
    let format: PluginFormat = plugin.format.parse()?;
    Ok(PluginDescriptor {
        id: PluginId::new(format, plugin.uid.clone()),
        name: plugin.name.clone(),
        vendor: String::new(),
        version: String::new(),
        kind: PluginKind::Other,
        categories: Vec::new(),
        path: plugin.path.as_ref().map(Into::into),
    })
}

/// Starts the plugin in a child process of its own.
#[cfg(unix)]
fn instantiate_bridged(
    descriptor: &PluginDescriptor,
    executable: Option<&std::path::Path>,
) -> Result<Box<dyn PluginInstance>, String> {
    let executable = executable
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(riddim_plugin::bridge::default_executable);
    riddim_plugin::bridge::instantiate(descriptor, &executable)
        .map(|instance| Box::new(instance) as Box<dyn PluginInstance>)
        .map_err(|e| e.to_string())
}

#[cfg(not(unix))]
fn instantiate_bridged(
    _: &PluginDescriptor,
    _: Option<&std::path::Path>,
) -> Result<Box<dyn PluginInstance>, String> {
    Err("plugins cannot be run in a process of their own on this platform yet".into())
}

/// Switches the plugin's optional buses to what the project uses. Must be called while the
/// instance is inactive. A bus the plugin refuses to change is left as it is: the graph is
/// built from what ends up active, so a missing bus is simply not connected.
fn configure_buses(instance: &mut dyn PluginInstance, buses: BusRequest) {
    let layout = instance.bus_layout();
    for (index, bus) in layout.audio_outputs.iter().enumerate().skip(1) {
        let wanted = index <= buses.extra_outputs as usize;
        if bus.active != wanted {
            let _ = instance.set_bus_active(BusDirection::Output, index, wanted);
        }
    }
    // The sidechain is the plugin's first auxiliary input, wherever it comes in its list (an
    // instrument may have no main input in front of it). Other auxiliary inputs stay off.
    let sidechain = layout
        .audio_inputs
        .iter()
        .position(|bus| bus.kind == BusKind::Aux);
    for (index, bus) in layout.audio_inputs.iter().enumerate() {
        if bus.kind != BusKind::Aux {
            continue;
        }
        let wanted = buses.sidechain && Some(index) == sidechain;
        if bus.active != wanted {
            let _ = instance.set_bus_active(BusDirection::Input, index, wanted);
        }
    }
}

fn ports_of(instance: &dyn PluginInstance) -> PluginPorts {
    let layout = instance.bus_layout();
    let active = |buses: &[riddim_plugin::BusInfo]| {
        buses
            .iter()
            .filter(|b| b.active)
            .map(|b| b.channels)
            .collect::<Vec<u32>>()
    };
    let active_inputs: Vec<&riddim_plugin::BusInfo> =
        layout.audio_inputs.iter().filter(|b| b.active).collect();
    PluginPorts {
        inputs: active(&layout.audio_inputs),
        main_input: active_inputs
            .first()
            .is_some_and(|bus| bus.kind == BusKind::Main),
        sidechain: active_inputs
            .iter()
            .position(|bus| bus.kind == BusKind::Aux),
        outputs: active(&layout.audio_outputs),
        latency: instance.latency_samples(),
        takes_events: layout.event_inputs > 0,
        emits_events: layout.event_outputs > 0,
    }
}

/// Gives a processor that has just been activated one block of silence, here on the thread
/// that loaded it, and resets it.
///
/// Plugins do things once in the first block they are ever given: touch memory they were
/// handed at activation, build tables, start threads. On the audio thread that is a dropout
/// at small buffer sizes each time a plugin joins a playing session. The processor has not
/// been handed to the audio thread yet, so for this one block this thread is its audio
/// thread; the engine's render threads already call a processor from one thread and then
/// another. Whatever the plugin sends out in that block is dropped, and so is the outcome: a
/// plugin that fails here will fail again where failures are handled.
///
/// Not done for an offline render, where nothing is waiting on the first block and the
/// result should come from a plugin nobody has run.
fn warm_up(processor: &mut dyn PluginProcessor, ports: &PluginPorts, config: &ActivateConfig) {
    if config.mode != ProcessMode::Realtime || config.max_block == 0 {
        return;
    }
    let frames = config.max_block as usize;
    let channels = |buses: &[u32]| buses.iter().map(|c| *c as usize).sum::<usize>();
    let silence = vec![0.0f32; frames];
    let inputs: Vec<&[f32]> = (0..channels(&ports.inputs))
        .map(|_| silence.as_slice())
        .collect();
    let mut output_storage = vec![vec![0.0f32; frames]; channels(&ports.outputs)];
    let mut outputs: Vec<&mut [f32]> = output_storage
        .iter_mut()
        .map(|channel| channel.as_mut_slice())
        .collect();
    let mut events_out = EventBuffer::with_capacity(WARM_UP_EVENTS);
    let transport = TransportInfo::stopped(config.sample_rate);
    let mut context = ProcessContext {
        frames,
        inputs: &inputs,
        outputs: &mut outputs,
        events_in: &[],
        events_out: &mut events_out,
        transport: &transport,
    };
    let _ = processor.first_block(&mut context);
    processor.reset();
}

/// Room for what a plugin sends out during its warm-up block, all of which is dropped.
const WARM_UP_EVENTS: usize = 256;

/// The main-thread half of the engine. Call [`pump`](Self::pump) regularly from the thread that
/// owns it.
pub struct MainThread {
    requests: Receiver<HostRequest>,
    replies: Sender<HostReply>,
    instances: HashMap<SlotId, Held>,
    /// Instances whose slot has since been given to a newer one, kept until the engine has
    /// handed back their processors.
    displaced: Vec<Held>,
    events: Vec<PluginEvent>,
    alive: bool,
    /// The bridge executable for bridged slots, if the engine was told of one.
    bridge: Option<std::path::PathBuf>,
    /// Give each plugin its first block here before the audio thread gets it.
    warm_up: bool,
}

struct Held {
    serial: u64,
    instance: Box<dyn PluginInstance>,
    /// The latency last reported to the engine.
    latency: u32,
}

impl MainThread {
    pub(crate) fn new(
        requests: Receiver<HostRequest>,
        replies: Sender<HostReply>,
        bridge: Option<std::path::PathBuf>,
        warm_up: bool,
    ) -> Self {
        Self {
            requests,
            replies,
            instances: HashMap::new(),
            displaced: Vec::new(),
            events: Vec::new(),
            alive: true,
            bridge,
            warm_up,
        }
    }

    /// Handles what the engine has asked for and collects what plugins have to report.
    /// Returns false once the engine has shut down.
    ///
    /// Loading a plugin can take seconds, and it has to happen on this thread. At most one
    /// plugin is loaded per call, so that an application pumping from its UI thread gets to
    /// handle its own events between one plugin and the next. Call it again soon: a few dozen
    /// times a second is plenty.
    pub fn pump(&mut self) -> bool {
        loop {
            match self.requests.try_recv() {
                Ok(request) => {
                    let slow = matches!(
                        request,
                        HostRequest::Load { .. } | HostRequest::Restart { .. }
                    );
                    self.handle(request);
                    if slow {
                        break;
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.alive = false;
                    break;
                }
            }
        }
        for (slot, held) in self.instances.iter_mut() {
            let instance = &mut held.instance;
            self.events.clear();
            instance.poll_events(&mut self.events);
            for event in &self.events {
                let _ = self.replies.send(HostReply::Event {
                    slot: *slot,
                    event: *event,
                });
            }
            // Latency is asked for on every turn, not only when the plugin says it changed:
            // the question is cheap, some plugins announce a change before their audio side
            // has made it (so asking at that moment gets the old answer), and some never
            // announce it at all.
            let latency = instance.latency_samples();
            if latency != held.latency {
                held.latency = latency;
                let _ = self.replies.send(HostReply::Latency {
                    slot: *slot,
                    latency,
                });
            }
        }
        if !self.alive {
            self.instances.clear();
            self.displaced.clear();
        }
        self.alive
    }

    fn handle(&mut self, request: HostRequest) {
        match request {
            HostRequest::Load {
                slot,
                serial,
                plugin,
                state,
                config,
                buses,
                bridged,
            } => {
                // An instance still in this slot belongs to an earlier load whose unload has
                // not arrived yet. Keep it aside: its processor is still out.
                if let Some(earlier) = self.instances.remove(&slot) {
                    self.displaced.push(earlier);
                }
                let result = self.load(
                    slot,
                    serial,
                    &plugin,
                    state.as_deref(),
                    &config,
                    buses,
                    bridged,
                );
                let _ = self.replies.send(HostReply::Loaded {
                    slot,
                    serial,
                    result,
                    restarted: false,
                });
            }
            HostRequest::Restart {
                slot,
                serial,
                processor,
                config,
                buses,
            } => {
                let result = match self.instances.get_mut(&slot) {
                    Some(held) if held.serial == serial => {
                        let instance = &mut held.instance;
                        if let Some(processor) = processor {
                            instance.deactivate(processor);
                        }
                        configure_buses(instance.as_mut(), buses);
                        let warm = self.warm_up;
                        let result = instance
                            .activate(&config)
                            .map(|mut processor| {
                                let ports = ports_of(instance.as_ref());
                                if warm {
                                    warm_up(processor.as_mut(), &ports, &config);
                                }
                                Loaded {
                                    processor,
                                    ports,
                                    parameters: instance.params().len(),
                                }
                            })
                            .map_err(|e| e.to_string());
                        if let Ok(loaded) = &result {
                            held.latency = loaded.ports.latency;
                        }
                        result
                    }
                    _ => {
                        // Not the instance in that slot any more: all that is left to do
                        // with it is let it go.
                        self.unload(serial, processor);
                        Err("the plugin is not loaded".to_string())
                    }
                };
                if result.is_err()
                    && self
                        .instances
                        .get(&slot)
                        .is_some_and(|h| h.serial == serial)
                {
                    // An instance that will not activate again is of no use to anyone.
                    self.instances.remove(&slot);
                }
                let _ = self.replies.send(HostReply::Loaded {
                    slot,
                    serial,
                    result,
                    restarted: true,
                });
            }
            HostRequest::Unload { serial, processor } => {
                self.unload(serial, processor);
                let _ = self.replies.send(HostReply::Unloaded);
            }
            HostRequest::SaveState { slot } => {
                let result = match self.instances.get_mut(&slot) {
                    Some(held) => held
                        .instance
                        .save_state()
                        .map(|s| s.to_bytes())
                        .map_err(|e| e.to_string()),
                    None => Err("the plugin is not loaded".to_string()),
                };
                let _ = self.replies.send(HostReply::State { slot, result });
            }
            HostRequest::NotifyParam { slot, id, value } => {
                if let Some(held) = self.instances.get_mut(&slot) {
                    held.instance.set_param_value(id, value);
                }
            }
            HostRequest::Shutdown => self.alive = false,
        }
    }

    /// Deactivates and destroys the instance with this serial, wherever it is kept.
    fn unload(&mut self, serial: u64, processor: Option<Box<dyn PluginProcessor>>) {
        let slot = self
            .instances
            .iter()
            .find(|(_, held)| held.serial == serial)
            .map(|(slot, _)| *slot);
        let held = match slot {
            Some(slot) => self.instances.remove(&slot),
            None => self
                .displaced
                .iter()
                .position(|held| held.serial == serial)
                .map(|index| self.displaced.remove(index)),
        };
        // (No such instance: its load failed, and there is nothing to let go of. A processor
        // cannot arrive without one.)
        if let (Some(mut held), Some(processor)) = (held, processor) {
            held.instance.deactivate(processor);
        }
    }

    fn load(
        &mut self,
        slot: SlotId,
        serial: u64,
        plugin: &PluginRef,
        state: Option<&Vec<u8>>,
        config: &ActivateConfig,
        buses: BusRequest,
        bridged: bool,
    ) -> Result<Loaded, String> {
        let descriptor = descriptor_for(plugin)?;
        let mut instance = if bridged {
            instantiate_bridged(&descriptor, self.bridge.as_deref())?
        } else {
            riddim_plugin::host::instantiate(&descriptor).map_err(|e| e.to_string())?
        };
        if let Some(bytes) = state {
            let blob = StateBlob::from_bytes(bytes).map_err(|e| e.to_string())?;
            instance.restore_state(&blob).map_err(|e| e.to_string())?;
        }
        configure_buses(instance.as_mut(), buses);
        let mut processor = instance.activate(config).map_err(|e| e.to_string())?;
        let ports = ports_of(instance.as_ref());
        if self.warm_up {
            warm_up(processor.as_mut(), &ports, config);
        }
        let loaded = Loaded {
            processor,
            ports,
            parameters: instance.params().len(),
        };
        self.instances.insert(
            slot,
            Held {
                serial,
                instance,
                latency: loaded.ports.latency,
            },
        );
        Ok(loaded)
    }

    /// The live instance in a slot, for opening its editor or inspecting its parameters. Only
    /// meaningful on the thread that pumps this object.
    pub fn instance_mut(&mut self, slot: SlotId) -> Option<&mut (dyn PluginInstance + 'static)> {
        self.instances
            .get_mut(&slot)
            .map(|held| held.instance.as_mut())
    }

    /// Slots that currently hold a loaded plugin.
    pub fn loaded_slots(&self) -> Vec<SlotId> {
        self.instances.keys().copied().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use riddim_plugin::ProcessStatus;
    use std::sync::{Arc, Mutex};

    /// Writes down what it is asked to do.
    struct Recorder(Arc<Mutex<Vec<String>>>);

    impl PluginProcessor for Recorder {
        fn first_block(&mut self, context: &mut ProcessContext<'_>) -> ProcessStatus {
            self.0.lock().unwrap().push("first block".into());
            self.process(context)
        }

        fn process(&mut self, context: &mut ProcessContext<'_>) -> ProcessStatus {
            let silent = context.inputs.iter().all(|c| c.iter().all(|s| *s == 0.0));
            self.0.lock().unwrap().push(format!(
                "process frames={} in={} out={} events={} playing={} silent={} lengths_ok={}",
                context.frames,
                context.inputs.len(),
                context.outputs.len(),
                context.events_in.len(),
                context.transport.playing,
                silent,
                context.inputs.iter().all(|c| c.len() == context.frames)
                    && context.outputs.iter().all(|c| c.len() == context.frames),
            ));
            ProcessStatus::Failed
        }

        fn reset(&mut self) {
            self.0.lock().unwrap().push("reset".into());
        }

        fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
            self
        }
    }

    fn ports() -> PluginPorts {
        PluginPorts {
            inputs: vec![2, 1],
            main_input: true,
            sidechain: Some(1),
            outputs: vec![2, 2, 2],
            latency: 0,
            takes_events: true,
            emits_events: false,
        }
    }

    fn config(mode: ProcessMode) -> ActivateConfig {
        ActivateConfig {
            sample_rate: 48_000.0,
            max_block: 64,
            mode,
        }
    }

    #[test]
    fn a_warm_up_is_one_silent_block_of_the_plugins_own_shape_and_then_a_reset() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut processor = Recorder(calls.clone());
        // (The recorder says the block failed. That is not the warm-up's business.)
        warm_up(&mut processor, &ports(), &config(ProcessMode::Realtime));
        assert_eq!(
            *calls.lock().unwrap(),
            [
                "first block",
                "process frames=64 in=3 out=6 events=0 playing=false silent=true lengths_ok=true",
                "reset"
            ]
        );
    }

    #[test]
    fn an_offline_render_gets_a_plugin_nobody_has_run() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut processor = Recorder(calls.clone());
        warm_up(&mut processor, &ports(), &config(ProcessMode::Offline));
        assert!(calls.lock().unwrap().is_empty());
    }
}
