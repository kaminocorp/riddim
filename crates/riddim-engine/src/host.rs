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
    ActivateConfig, BusDirection, PluginDescriptor, PluginEvent, PluginFormat, PluginId,
    PluginInstance, PluginKind, PluginProcessor, StateBlob,
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
    for (index, bus) in layout.audio_inputs.iter().enumerate().skip(1) {
        let wanted = index == 1 && buses.sidechain;
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
    instances: HashMap<SlotId, Held>,
    /// Instances whose slot has since been given to a newer one, kept until the engine has
    /// handed back their processors.
    displaced: Vec<Held>,
    events: Vec<PluginEvent>,
    alive: bool,
}

struct Held {
    serial: u64,
    instance: Box<dyn PluginInstance>,
}

impl MainThread {
    pub(crate) fn new(requests: Receiver<HostRequest>, replies: Sender<HostReply>) -> Self {
        Self {
            requests,
            replies,
            instances: HashMap::new(),
            displaced: Vec::new(),
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
            } => {
                // An instance still in this slot belongs to an earlier load whose unload has
                // not arrived yet. Keep it aside: its processor is still out.
                if let Some(earlier) = self.instances.remove(&slot) {
                    self.displaced.push(earlier);
                }
                let result = self.load(slot, serial, &plugin, state.as_deref(), &config, buses);
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
                        instance
                            .activate(&config)
                            .map(|processor| Loaded {
                                processor,
                                ports: ports_of(instance.as_ref()),
                                parameters: instance.params().len(),
                            })
                            .map_err(|e| e.to_string())
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
    ) -> Result<Loaded, String> {
        let descriptor = descriptor_for(plugin)?;
        let mut instance =
            riddim_plugin::host::instantiate(&descriptor).map_err(|e| e.to_string())?;
        if let Some(bytes) = state {
            let blob = StateBlob::from_bytes(bytes).map_err(|e| e.to_string())?;
            instance.restore_state(&blob).map_err(|e| e.to_string())?;
        }
        configure_buses(instance.as_mut(), buses);
        let processor = instance.activate(config).map_err(|e| e.to_string())?;
        let loaded = Loaded {
            processor,
            ports: ports_of(instance.as_ref()),
            parameters: instance.params().len(),
        };
        self.instances.insert(slot, Held { serial, instance });
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
