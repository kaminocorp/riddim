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
    /// A plugin. Its input ports are its main input (if it has one) and its sidechain (if that
    /// is switched on); output port 0 is its main output and the ports after it its extra
    /// outputs, in the plugin's own order.
    Plugin(TrackId, SlotId),
    /// Fader, pan, mute and meter.
    Strip(TrackId),
    /// The gain stage of a send, between its tap on the track and the bus it feeds.
    Send(TrackId, SendId),
    /// The switch on a track's way to its output. Only tracks with post-fader sends have one:
    /// those sends tap the strip itself, so this is what lets a solo elsewhere close the
    /// track's own output while a send from it stays open (soloing an effect return).
    MainOut(TrackId),
    Metronome,
    /// The device output.
    Output,
}

/// What the graph needs to know about a loaded plugin.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct PluginPorts {
    /// Channel count of each active audio input bus, in the plugin's order.
    pub inputs: Vec<u32>,
    /// Whether the first of `inputs` is the plugin's main input. (An instrument may have a
    /// sidechain input and no main one.)
    pub main_input: bool,
    /// Which of `inputs` is the sidechain, if the plugin has one and it is switched on.
    pub sidechain: Option<usize>,
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
        self.connect_port(from, 0, to, to_port);
    }

    fn connect_port(&mut self, from: NodeIndex, from_port: usize, to: NodeIndex, to_port: usize) {
        self.spec.edges.push(Edge {
            from,
            from_port,
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
    // Connections between tracks that can only be made once every track has its nodes.
    let mut plugin_outputs: Vec<(NodeIndex, usize, TrackId)> = Vec::new();
    let mut sidechains: Vec<(NodeIndex, usize, TrackId, bool)> = Vec::new();
    let mut note_feeds: Vec<(NodeIndex, TrackId)> = Vec::new();
    // Per track: the signal just before the strip, the strip, and where its notes end up.
    let mut pre_fader: HashMap<TrackId, NodeIndex> = HashMap::new();
    let mut post_fader: HashMap<TrackId, NodeIndex> = HashMap::new();
    let mut notes_out: HashMap<TrackId, NodeIndex> = HashMap::new();

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

        let mut fed = false;
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
            if let (Some(source), true) = (audio, ports.main_input) {
                graph.connect(source, node, 0);
            }
            if let (Some(source), true) = (events, ports.takes_events) {
                graph.spec.event_edges.push(EventEdge {
                    from: source,
                    to: node,
                });
            }
            // Notes from another track join this track's own at its first plugin that takes
            // them (the instrument, or a MIDI effect in front of it).
            if let (Some(from), true, false) = (track.events_from, ports.takes_events, fed) {
                note_feeds.push((node, from));
                fed = true;
            }
            // A sidechain needs the plugin to have a sidechain input, switched on.
            if let (Some(sidechain), Some(port)) = (slot.sidechain, ports.sidechain) {
                sidechains.push((node, port, sidechain.source, sidechain.pre_fader));
            }
            // Extra outputs the plugin actually has; one that is routed but missing (the
            // plugin could not switch the bus on) is simply not connected.
            for output in &slot.outputs {
                if (output.bus as usize) < ports.outputs.len() {
                    plugin_outputs.push((node, output.bus as usize, output.target));
                }
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
            pre_fader.insert(track.id, source);
        }
        post_fader.insert(track.id, strip);
        if let Some(source) = events {
            notes_out.insert(track.id, source);
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
        let mut last = strip;
        if output.is_some() && track.sends.iter().any(|send| !send.pre_fader) {
            last = graph.add(NodeKind::MainOut(track.id), stereo_through());
            graph.connect(strip, last, 0);
        }
        strips.push((track.id, last, output));
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
    for (plugin, port, target) in plugin_outputs {
        if let Some(&bus) = bus_inputs.get(&target) {
            graph.connect_port(plugin, port, bus, 0);
        }
    }
    for (plugin, port, source, before_fader) in sidechains {
        // A pre-fader tap on a track with no audio yet (an instrument whose plugin is missing)
        // has nothing to listen to.
        let taps = if before_fader {
            &pre_fader
        } else {
            &post_fader
        };
        if let Some(&tap) = taps.get(&source) {
            graph.connect(tap, plugin, port);
        }
    }
    for (plugin, source) in note_feeds {
        if let Some(&from) = notes_out.get(&source) {
            graph.spec.event_edges.push(EventEdge { from, to: plugin });
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
            outputs: Vec::new(),
            sidechain: None,
            bridged: false,
        }
    }

    fn effect(latency: u32) -> PluginPorts {
        PluginPorts {
            inputs: vec![2],
            main_input: true,
            sidechain: None,
            outputs: vec![2],
            latency,
            takes_events: false,
            emits_events: false,
        }
    }

    fn instrument() -> PluginPorts {
        PluginPorts {
            inputs: vec![],
            main_input: false,
            sidechain: None,
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
        // The track has a post-fader send, so its own output goes through a switch that solo
        // can close without silencing the send. The bus has no sends and needs none.
        assert!(has_edge(
            NodeKind::Strip(TrackId(1)),
            NodeKind::MainOut(TrackId(1))
        ));
        assert!(graph.index_of(NodeKind::MainOut(TrackId(3))).is_none());
        assert_eq!(delay_from(NodeKind::MainOut(TrackId(1))), 100);
        assert_eq!(delay_from(NodeKind::Strip(TrackId(3))), 0);
        assert_eq!(plan.max_latency, 100);
    }

    #[test]
    fn plugin_outputs_sidechains_and_notes_cross_between_tracks() {
        use riddim_model::{PluginOutput, Sidechain};
        // Track 2: a multi-out instrument (slot 10) whose second output goes to bus 3, where a
        // compressor (slot 30) is keyed from audio track 1. Track 4: a second instrument
        // (slot 40) that plays the notes the first one puts out.
        let mut model = session();
        model
            .apply(&Op::AddTrack {
                index: 3,
                track: Track::new(TrackId(4), "t", TrackKind::Instrument),
            })
            .unwrap();
        for (track, id) in [(2, 10), (3, 30), (4, 40)] {
            model
                .apply(&Op::InsertPlugin {
                    track: TrackId(track),
                    index: 0,
                    slot: slot(id),
                })
                .unwrap();
        }
        model
            .apply(&Op::SetPluginRouting {
                track: TrackId(2),
                slot: SlotId(10),
                outputs: vec![
                    PluginOutput {
                        bus: 1,
                        target: TrackId(3),
                    },
                    // The plugin turns out to have no third output: routed, but not there.
                    PluginOutput {
                        bus: 2,
                        target: TrackId(3),
                    },
                ],
                sidechain: None,
            })
            .unwrap();
        model
            .apply(&Op::SetPluginRouting {
                track: TrackId(3),
                slot: SlotId(30),
                outputs: vec![],
                sidechain: Some(Sidechain {
                    source: TrackId(1),
                    pre_fader: false,
                }),
            })
            .unwrap();
        model
            .apply(&Op::SetTrack {
                id: TrackId(4),
                change: TrackChange::EventsFrom(Some(TrackId(2))),
            })
            .unwrap();

        let instrument = |outputs: Vec<u32>, emits_events| PluginPorts {
            inputs: vec![],
            main_input: false,
            sidechain: None,
            outputs,
            latency: 0,
            takes_events: true,
            emits_events,
        };
        let plugins = HashMap::from([
            (SlotId(10), instrument(vec![2, 2], true)),
            (
                SlotId(30),
                PluginPorts {
                    inputs: vec![2, 2],
                    main_input: true,
                    sidechain: Some(1),
                    outputs: vec![2],
                    latency: 32,
                    takes_events: false,
                    emits_events: false,
                },
            ),
            (SlotId(40), instrument(vec![2], false)),
        ]);
        let graph = build(&model, &plugins);
        let index = |kind| graph.index_of(kind).unwrap();
        let sampler = index(NodeKind::Plugin(TrackId(2), SlotId(10)));
        let compressor = index(NodeKind::Plugin(TrackId(3), SlotId(30)));
        let follower = index(NodeKind::Plugin(TrackId(4), SlotId(40)));
        let edge = |from, from_port, to, to_port| Edge {
            from,
            from_port,
            to,
            to_port,
        };
        let edges = &graph.spec.edges;
        // Main output down the track, second output into the bus, and no edge for the third.
        assert!(edges.contains(&edge(sampler, 0, index(NodeKind::Strip(TrackId(2))), 0)));
        assert!(edges.contains(&edge(sampler, 1, index(NodeKind::BusInput(TrackId(3))), 0)));
        assert_eq!(edges.iter().filter(|e| e.from == sampler).count(), 2);
        // The key signal arrives on the compressor's second input, from after track 1's fader.
        assert!(edges.contains(&edge(index(NodeKind::Strip(TrackId(1))), 0, compressor, 1)));
        // The follower hears its own track's notes and the sampler's.
        let feeds: Vec<NodeIndex> = graph
            .spec
            .event_edges
            .iter()
            .filter(|e| e.to == follower)
            .map(|e| e.from)
            .collect();
        assert_eq!(feeds, vec![index(NodeKind::Source(TrackId(4))), sampler]);

        // It compiles, and the key is delayed to line up with... nothing: both of the
        // compressor's inputs arrive on time, and its own 32 samples are made up at the master.
        let plan = compile(&graph.spec).unwrap();
        let step = &plan.steps[plan.step_of_node[compressor]];
        assert_eq!(step.inputs.len(), 2);
        assert!(step
            .inputs
            .iter()
            .all(|i| i.sources.iter().all(|s| s.delay == 0)));
        assert_eq!(plan.max_latency, 32);

        // A sidechain on a plugin that has no sidechain input is left unconnected.
        let mut narrow = plugins.clone();
        narrow.get_mut(&SlotId(30)).unwrap().inputs = vec![2];
        narrow.get_mut(&SlotId(30)).unwrap().sidechain = None;
        let graph = build(&model, &narrow);
        let compressor = graph
            .index_of(NodeKind::Plugin(TrackId(3), SlotId(30)))
            .unwrap();
        assert!(graph
            .spec
            .edges
            .iter()
            .all(|e| !(e.to == compressor && e.to_port == 1)));
        compile(&graph.spec).unwrap();
    }

    #[test]
    fn an_instrument_whose_only_input_is_a_sidechain_is_keyed_on_that_input() {
        use riddim_model::Sidechain;
        // Surge XT is like this: no main input, one auxiliary "Sidechain" input.
        let mut model = session();
        model
            .apply(&Op::InsertPlugin {
                track: TrackId(2),
                index: 0,
                slot: slot(10),
            })
            .unwrap();
        model
            .apply(&Op::SetPluginRouting {
                track: TrackId(2),
                slot: SlotId(10),
                outputs: vec![],
                sidechain: Some(Sidechain {
                    source: TrackId(1),
                    pre_fader: true,
                }),
            })
            .unwrap();
        let keyed = PluginPorts {
            inputs: vec![2],
            main_input: false,
            sidechain: Some(0),
            ..instrument()
        };
        let graph = build(&model, &HashMap::from([(SlotId(10), keyed)]));
        let synth = graph
            .index_of(NodeKind::Plugin(TrackId(2), SlotId(10)))
            .unwrap();
        let into: Vec<&Edge> = graph.spec.edges.iter().filter(|e| e.to == synth).collect();
        // One audio connection into the synth: the key, on its only input, from before
        // track 1's fader.
        assert_eq!(into.len(), 1);
        assert_eq!(
            into[0].from,
            graph.index_of(NodeKind::Source(TrackId(1))).unwrap()
        );
        assert_eq!(into[0].to_port, 0);
        compile(&graph.spec).unwrap();
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
