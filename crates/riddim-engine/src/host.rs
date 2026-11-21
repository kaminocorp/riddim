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
    ActivateConfig, PluginDescriptor, PluginEvent, PluginFormat, PluginId, PluginInstance,
    PluginKind, PluginProcessor, StateBlob,
};

pub(crate) enum HostRequest {
    Load {
        slot: SlotId,
        plugin: PluginRef,
        state: Option<Arc<Vec<u8>>>,
        config: ActivateConfig,
    },
    /// Deactivate (with the processor, if the audio side had one) and destroy the instance.
    Unload {
        slot: SlotId,
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
        result: Result<Loaded, String>,
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

fn ports_of(instance: &dyn PluginInstance) -> PluginPorts {
    let layout = instance.bus_layout();
    let active = |buses: &[riddim_plugin::BusInfo]| {
        buses
            .iter()
            .filter(|b| b.active)
            .map(|b| b.channels)
            .collect::<Vec<u32>>()
    };
    PluginPorts {
        inputs: active(&layout.audio_inputs),
        outputs: active(&layout.audio_outputs),
        latency: instance.latency_samples(),
        takes_events: layout.event_inputs > 0,
        emits_events: layout.event_outputs > 0,
    }
}

/// The main-thread half of the engine. Call [`pump`](Self::pump) regularly from the thread that
/// owns it.
pub struct MainThread {
    requests: Receiver<HostRequest>,
    replies: Sender<HostReply>,
    instances: HashMap<SlotId, Box<dyn PluginInstance>>,
    events: Vec<PluginEvent>,
    alive: bool,
}

impl MainThread {
    pub(crate) fn new(requests: Receiver<HostRequest>, replies: Sender<HostReply>) -> Self {
        Self {
            requests,
            replies,
            instances: HashMap::new(),
            events: Vec::new(),
            alive: true,
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
                    let slow = matches!(request, HostRequest::Load { .. });
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
        for (slot, instance) in self.instances.iter_mut() {
            self.events.clear();
            instance.poll_events(&mut self.events);
            for event in &self.events {
                if *event == PluginEvent::LatencyChanged {
                    let latency = instance.latency_samples();
                    let _ = self.replies.send(HostReply::Latency {
                        slot: *slot,
                        latency,
                    });
                }
                let _ = self.replies.send(HostReply::Event {
                    slot: *slot,
                    event: *event,
                });
            }
        }
        if !self.alive {
            self.instances.clear();
        }
        self.alive
    }

    fn handle(&mut self, request: HostRequest) {
        match request {
            HostRequest::Load {
                slot,
                plugin,
                state,
                config,
            } => {
                let result = self.load(slot, &plugin, state.as_deref(), &config);
                let _ = self.replies.send(HostReply::Loaded { slot, result });
            }
            HostRequest::Unload { slot, processor } => {
                if let Some(mut instance) = self.instances.remove(&slot) {
                    if let Some(processor) = processor {
                        instance.deactivate(processor);
                    }
                }
                let _ = self.replies.send(HostReply::Unloaded);
            }
            HostRequest::SaveState { slot } => {
                let result = match self.instances.get_mut(&slot) {
                    Some(instance) => instance
                        .save_state()
                        .map(|s| s.to_bytes())
                        .map_err(|e| e.to_string()),
                    None => Err("the plugin is not loaded".to_string()),
                };
                let _ = self.replies.send(HostReply::State { slot, result });
            }
            HostRequest::NotifyParam { slot, id, value } => {
                if let Some(instance) = self.instances.get_mut(&slot) {
                    instance.set_param_value(id, value);
                }
            }
            HostRequest::Shutdown => self.alive = false,
        }
    }

    fn load(
        &mut self,
        slot: SlotId,
        plugin: &PluginRef,
        state: Option<&Vec<u8>>,
        config: &ActivateConfig,
    ) -> Result<Loaded, String> {
        let descriptor = descriptor_for(plugin)?;
        let mut instance =
            riddim_plugin::host::instantiate(&descriptor).map_err(|e| e.to_string())?;
        if let Some(bytes) = state {
            let blob = StateBlob::from_bytes(bytes).map_err(|e| e.to_string())?;
            instance.restore_state(&blob).map_err(|e| e.to_string())?;
        }
        let processor = instance.activate(config).map_err(|e| e.to_string())?;
        let loaded = Loaded {
            processor,
            ports: ports_of(instance.as_ref()),
            parameters: instance.params().len(),
        };
        self.instances.insert(slot, instance);
        Ok(loaded)
    }

    /// The live instance in a slot, for opening its editor or inspecting its parameters. Only
    /// meaningful on the thread that pumps this object.
    pub fn instance_mut(&mut self, slot: SlotId) -> Option<&mut (dyn PluginInstance + 'static)> {
        self.instances
            .get_mut(&slot)
            .map(|instance| instance.as_mut())
    }

    /// Slots that currently hold a loaded plugin.
    pub fn loaded_slots(&self) -> Vec<SlotId> {
        self.instances.keys().copied().collect()
    }
}
