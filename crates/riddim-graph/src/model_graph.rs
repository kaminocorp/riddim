//! The graph of an engine model.
//!
//! Every track becomes a short chain: a head (clip and input playback for audio and instrument
//! tracks, a summing input for buses and the master), its plugins in order, and a strip (fader,
//! pan, mute, meter). Strips feed the summing input of whatever they are routed to. The master's
//! strip and the metronome feed the device output.
//!
//! ```text
//!  audio track:       Source ──audio──▶ plugins… ──▶ Strip ─┐
//!  instrument track:  Source ──events─▶ instrument ─▶ … ──▶ Strip ─┤
//!  bus:               BusInput ───────▶ plugins… ──▶ Strip ─┤
//!                                                           ▼
//!  master:            BusInput ───────▶ plugins… ──▶ Strip ──▶ Output ◀── Metronome
//! ```
//!
//! A send is one more node: it taps its track before the strip (pre-fader) or after it
//! (post-fader), applies its own gain, and feeds the summing input of its target bus.

use std::collections::HashMap;

use riddim_model::{Model, SendId, SlotId, TrackId, TrackKind, TrackOutput};

use crate::compile::{Edge, EventEdge, GraphSpec, NodeIndex, NodeSpec};

/// Channels of every internal signal path. Mono material is spread to both.
pub const STRIP_CHANNELS: u32 = 2;

/// What a node in the model's graph is. This, not the node's index, is its stable identity:
/// state that must survive a recompile (plugin processors, delay lines, meters) is keyed by it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum NodeKind {
    /// Clip playback and live input for an audio or instrument track.
    Source(TrackId),
    /// The summing point of a bus or of the master.
    BusInput(TrackId),
    Plugin(TrackId, SlotId),
    /// Fader, pan, mute and meter.
    Strip(TrackId),
    /// The gain stage of a send, between its tap on the track and the bus it feeds.
    Send(TrackId, SendId),
    Metronome,
    /// The device output.
    Output,
}

/// What the graph needs to know about a loaded plugin.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct PluginPorts {
    /// Channel count of each active audio input bus (the first is the main input).
    pub inputs: Vec<u32>,
    /// Channel count of each active audio output bus (the first is the main output).
    pub outputs: Vec<u32>,
    pub latency: u32,
    pub takes_events: bool,
    pub emits_events: bool,
}

/// A model's graph: the spec to compile, and what each node is.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct ModelGraph {
    pub spec: GraphSpec,
    /// `kinds[i]` describes `spec.nodes[i]`.
    pub kinds: Vec<NodeKind>,
}

impl ModelGraph {
    pub fn index_of(&self, kind: NodeKind) -> Option<NodeIndex> {
        self.kinds.iter().position(|&k| k == kind)
    }

    fn add(&mut self, kind: NodeKind, node: NodeSpec) -> NodeIndex {
        self.kinds.push(kind);
        self.spec.nodes.push(node);
        self.spec.nodes.len() - 1
    }

    fn connect(&mut self, from: NodeIndex, to: NodeIndex, to_port: usize) {
        self.spec.edges.push(Edge {
            from,
            from_port: 0,
            to,
            to_port,
        });
    }
}

fn stereo_through() -> NodeSpec {
    NodeSpec {
        inputs: vec![STRIP_CHANNELS],
        outputs: vec![STRIP_CHANNELS],
        latency: 0,
        event_output: false,
    }
}

/// Builds the graph for `model`.
///
/// `plugins` describes the plugins that are actually loaded. A slot that is missing from it (a
/// plugin that is not installed, or still loading) contributes no node: the signal passes it
/// by, so a project with a missing plugin still plays.
pub fn build(model: &Model, plugins: &HashMap<SlotId, PluginPorts>) -> ModelGraph {
    let mut graph = ModelGraph::default();
    // Pass 1: every track's own chain, remembering its strip and its summing input.
    let mut strips: Vec<(TrackId, NodeIndex, Option<TrackOutput>)> = Vec::new();
    let mut bus_inputs: HashMap<TrackId, NodeIndex> = HashMap::new();
    let mut sends: Vec<(NodeIndex, TrackId)> = Vec::new();

    for track in model.all_tracks() {
        // The head of the chain: where audio and events for this track come from.
        let mut audio: Option<NodeIndex> = None;
        let mut events: Option<NodeIndex> = None;
        match track.kind {
            TrackKind::Audio => {
                audio = Some(graph.add(
                    NodeKind::Source(track.id),
                    NodeSpec {
                        inputs: vec![],
                        outputs: vec![STRIP_CHANNELS],
                        latency: 0,
                        event_output: false,
                    },
                ));
            }
            TrackKind::Instrument => {
                events = Some(graph.add(
                    NodeKind::Source(track.id),
                    NodeSpec {
                        inputs: vec![],
                        outputs: vec![],
                        latency: 0,
                        event_output: true,
                    },
                ));
            }
            TrackKind::Bus | TrackKind::Master => {
                let input = graph.add(NodeKind::BusInput(track.id), stereo_through());
                bus_inputs.insert(track.id, input);
                audio = Some(input);
            }
        }

        for slot in &track.plugins {
            let Some(ports) = plugins.get(&slot.id) else {
                continue;
            };
            let node = graph.add(
                NodeKind::Plugin(track.id, slot.id),
                NodeSpec {
                    inputs: ports.inputs.clone(),
                    outputs: ports.outputs.clone(),
                    latency: ports.latency,
                    event_output: ports.emits_events,
                },
            );
            if let (Some(source), false) = (audio, ports.inputs.is_empty()) {
                graph.connect(source, node, 0);
            }
            if let (Some(source), true) = (events, ports.takes_events) {
                graph.spec.event_edges.push(EventEdge {
                    from: source,
                    to: node,
                });
            }
            if !ports.outputs.is_empty() {
                audio = Some(node);
            }
            if ports.emits_events {
                events = Some(node);
            }
        }

        let strip = graph.add(NodeKind::Strip(track.id), stereo_through());
        if let Some(source) = audio {
            graph.connect(source, strip, 0);
        }
        // Sends tap the track before or after its strip. A pre-fader send on a track with no
        // audio yet (an instrument whose plugin is missing) has nothing to tap and is left out.
        for send in &track.sends {
            let tap = if send.pre_fader { audio } else { Some(strip) };
            let Some(tap) = tap else { continue };
            let node = graph.add(NodeKind::Send(track.id, send.id), stereo_through());
            graph.connect(tap, node, 0);
            sends.push((node, send.target));
        }
        let output = (track.kind != TrackKind::Master).then_some(track.output);
        strips.push((track.id, strip, output));
    }

    // Pass 2: routing between tracks, and the way out.
    let metronome = graph.add(
        NodeKind::Metronome,
        NodeSpec {
            inputs: vec![],
            outputs: vec![STRIP_CHANNELS],
            latency: 0,
            event_output: false,
        },
    );
    let output = graph.add(
        NodeKind::Output,
        NodeSpec {
            inputs: vec![STRIP_CHANNELS],
            outputs: vec![],
            latency: 0,
            event_output: false,
        },
    );
    for (_, strip, destination) in strips {
        match destination {
            None => graph.connect(strip, output, 0),
            Some(TrackOutput::Master) => graph.connect(strip, bus_inputs[&TrackId::MASTER], 0),
            Some(TrackOutput::Bus(bus)) => {
                // The model guarantees outputs name existing buses; fall back to the master
                // rather than drop the signal if that ever fails to hold.
                let target = bus_inputs
                    .get(&bus)
                    .copied()
                    .unwrap_or(bus_inputs[&TrackId::MASTER]);
                graph.connect(strip, target, 0);
            }
        }
    }
    for (node, target) in sends {
        // The model guarantees a send names an existing bus; one that somehow does not is
        // simply not connected.
        if let Some(&bus) = bus_inputs.get(&target) {
            graph.connect(node, bus, 0);
        }
    }
    graph.connect(metronome, output, 0);
    graph
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile::compile;
    use riddim_model::{Op, PluginRef, PluginSlot, Track, TrackChange};

    fn slot(id: u64) -> PluginSlot {
        PluginSlot {
            id: SlotId(id),
            plugin: PluginRef {
                format: "vst3".into(),
                uid: "X".into(),
                name: String::new(),
                path: None,
            },
            state: None,
            bypass: false,
        }
    }

    fn effect(latency: u32) -> PluginPorts {
        PluginPorts {
            inputs: vec![2],
            outputs: vec![2],
            latency,
            takes_events: false,
            emits_events: false,
        }
    }

    fn instrument() -> PluginPorts {
        PluginPorts {
            inputs: vec![],
            outputs: vec![2],
            latency: 0,
            takes_events: true,
            emits_events: false,
        }
    }

    fn session() -> Model {
        let mut model = Model::new();
        for (index, (id, kind)) in [
            (1, TrackKind::Audio),
            (2, TrackKind::Instrument),
            (3, TrackKind::Bus),
        ]
        .into_iter()
        .enumerate()
        {
            model
                .apply(&Op::AddTrack {
                    index,
                    track: Track::new(TrackId(id), "t", kind),
                })
                .unwrap();
        }
        model
    }

    #[test]
    fn an_empty_project_still_has_a_master_path() {
        let graph = build(&Model::new(), &HashMap::new());
        assert_eq!(
            graph.kinds,
            vec![
                NodeKind::BusInput(TrackId::MASTER),
                NodeKind::Strip(TrackId::MASTER),
                NodeKind::Metronome,
                NodeKind::Output
            ]
        );
        let plan = compile(&graph.spec).unwrap();
        assert_eq!(
            plan.steps.last().unwrap().node,
            graph.index_of(NodeKind::Output).unwrap()
        );
        assert_eq!(
            plan.steps.last().unwrap().inputs[0].sources.len(),
            2,
            "master strip and metronome"
        );
    }

    #[test]
    fn chains_and_routing() {
        let mut model = session();
        model
            .apply(&Op::InsertPlugin {
                track: TrackId(2),
                index: 0,
                slot: slot(10),
            })
            .unwrap();
        model
            .apply(&Op::InsertPlugin {
                track: TrackId(1),
                index: 0,
                slot: slot(11),
            })
            .unwrap();
        model
            .apply(&Op::InsertPlugin {
                track: TrackId(1),
                index: 1,
                slot: slot(12),
            })
            .unwrap(); // not loaded
        model
            .apply(&Op::SetTrack {
                id: TrackId(1),
                change: TrackChange::Output(TrackOutput::Bus(TrackId(3))),
            })
            .unwrap();
        let plugins = HashMap::from([(SlotId(10), instrument()), (SlotId(11), effect(256))]);
        let graph = build(&model, &plugins);
        let index = |kind| {
            graph
                .index_of(kind)
                .unwrap_or_else(|| panic!("no node {kind:?}"))
        };
        assert!(graph
            .index_of(NodeKind::Plugin(TrackId(1), SlotId(12)))
            .is_none());

        let has_edge = |from, to| {
            graph
                .spec
                .edges
                .iter()
                .any(|e| e.from == index(from) && e.to == index(to))
        };
        // Audio track: source -> effect -> strip -> bus.
        assert!(has_edge(
            NodeKind::Source(TrackId(1)),
            NodeKind::Plugin(TrackId(1), SlotId(11))
        ));
        assert!(has_edge(
            NodeKind::Plugin(TrackId(1), SlotId(11)),
            NodeKind::Strip(TrackId(1))
        ));
        assert!(has_edge(
            NodeKind::Strip(TrackId(1)),
            NodeKind::BusInput(TrackId(3))
        ));
        // Instrument track: events into the instrument, its audio to the strip, strip to master.
        assert!(graph.spec.event_edges.contains(&EventEdge {
            from: index(NodeKind::Source(TrackId(2))),
            to: index(NodeKind::Plugin(TrackId(2), SlotId(10))),
        }));
        assert!(has_edge(
            NodeKind::Plugin(TrackId(2), SlotId(10)),
            NodeKind::Strip(TrackId(2))
        ));
        assert!(has_edge(
            NodeKind::Strip(TrackId(2)),
            NodeKind::BusInput(TrackId::MASTER)
        ));
        assert!(has_edge(
            NodeKind::Strip(TrackId(3)),
            NodeKind::BusInput(TrackId::MASTER)
        ));
        assert!(has_edge(NodeKind::Strip(TrackId::MASTER), NodeKind::Output));
        assert!(has_edge(NodeKind::Metronome, NodeKind::Output));

        // The effect's 256 samples are compensated where the paths meet at the master, and the
        // metronome is delayed to match at the output.
        let plan = compile(&graph.spec).unwrap();
        let step = |kind| &plan.steps[plan.step_of_node[index(kind)]];
        let master_in = step(NodeKind::BusInput(TrackId::MASTER));
        assert_eq!(master_in.input_latency, 256);
        let delay_from = |kind| {
            master_in.inputs[0]
                .sources
                .iter()
                .find(|s| s.edge.from == index(kind))
                .unwrap()
                .delay
        };
        assert_eq!(delay_from(NodeKind::Strip(TrackId(2))), 256);
        assert_eq!(delay_from(NodeKind::Strip(TrackId(3))), 0);
        let out = step(NodeKind::Output);
        assert_eq!(
            out.inputs[0]
                .sources
                .iter()
                .map(|s| s.delay)
                .collect::<Vec<_>>(),
            vec![0, 256]
        );
        assert_eq!(plan.max_latency, 256);
    }

    #[test]
    fn sends_tap_before_or_after_the_strip_and_are_compensated() {
        use riddim_model::{Send, SendId};
        let mut model = session();
        model
            .apply(&Op::InsertPlugin {
                track: TrackId(3),
                index: 0,
                slot: slot(20),
            })
            .unwrap();
        let send = |id, pre_fader| Send {
            id: SendId(id),
            target: TrackId(3),
            gain: 1.0,
            pre_fader,
            mute: false,
        };
        model
            .apply(&Op::AddSend {
                track: TrackId(1),
                index: 0,
                send: send(30, false),
            })
            .unwrap();
        model
            .apply(&Op::AddSend {
                track: TrackId(1),
                index: 1,
                send: send(31, true),
            })
            .unwrap();
        // The bus carries an effect with 100 samples of latency.
        let graph = build(&model, &HashMap::from([(SlotId(20), effect(100))]));
        let index = |kind| {
            graph
                .index_of(kind)
                .unwrap_or_else(|| panic!("no node {kind:?}"))
        };
        let has_edge = |from, to| {
            graph
                .spec
                .edges
                .iter()
                .any(|e| e.from == index(from) && e.to == index(to))
        };
        let post = NodeKind::Send(TrackId(1), SendId(30));
        let pre = NodeKind::Send(TrackId(1), SendId(31));
        assert!(has_edge(NodeKind::Strip(TrackId(1)), post));
        assert!(has_edge(NodeKind::Source(TrackId(1)), pre));
        assert!(has_edge(post, NodeKind::BusInput(TrackId(3))));
        assert!(has_edge(pre, NodeKind::BusInput(TrackId(3))));

        // At the master, the track's direct path is delayed to match its path through the bus.
        let plan = compile(&graph.spec).unwrap();
        let master_in = &plan.steps[plan.step_of_node[index(NodeKind::BusInput(TrackId::MASTER))]];
        let delay_from = |kind| {
            master_in.inputs[0]
                .sources
                .iter()
                .find(|s| s.edge.from == index(kind))
                .unwrap()
                .delay
        };
        assert_eq!(delay_from(NodeKind::Strip(TrackId(1))), 100);
        assert_eq!(delay_from(NodeKind::Strip(TrackId(3))), 0);
        assert_eq!(plan.max_latency, 100);
    }

    #[test]
    fn an_instrument_track_without_its_plugin_is_silent_not_broken() {
        let model = session();
        let graph = build(&model, &HashMap::new());
        let plan = compile(&graph.spec).unwrap();
        let strip = graph.index_of(NodeKind::Strip(TrackId(2))).unwrap();
        assert!(plan.steps[plan.step_of_node[strip]].inputs[0]
            .sources
            .is_empty());
    }
}
