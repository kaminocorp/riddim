//! The generic compiler.

use std::collections::BTreeSet;
use std::fmt;

/// Index of a node in [`GraphSpec::nodes`].
pub type NodeIndex = usize;

/// One processing node, as the compiler sees it.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct NodeSpec {
    /// Channel count of each audio input port. Several edges into one port are summed.
    pub inputs: Vec<u32>,
    /// Channel count of each audio output port.
    pub outputs: Vec<u32>,
    /// Samples of delay this node introduces between its inputs and outputs.
    pub latency: u32,
    /// Whether the node produces an event (MIDI) stream.
    pub event_output: bool,
}

/// An audio connection from an output port to an input port.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Edge {
    pub from: NodeIndex,
    pub from_port: usize,
    pub to: NodeIndex,
    pub to_port: usize,
}

/// An event connection: `to` receives the events `from` produces.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EventEdge {
    pub from: NodeIndex,
    pub to: NodeIndex,
}

/// What to compile.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct GraphSpec {
    pub nodes: Vec<NodeSpec>,
    pub edges: Vec<Edge>,
    pub event_edges: Vec<EventEdge>,
}

/// Why a graph cannot be compiled.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphError {
    /// An edge names a node or port that does not exist.
    DanglingEdge(Edge),
    DanglingEventEdge(EventEdge),
    /// An event edge starts at a node that produces no events.
    NoEventOutput(EventEdge),
    /// The nodes listed feed back into themselves.
    Cycle(Vec<NodeIndex>),
}

impl fmt::Display for GraphError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GraphError::DanglingEdge(edge) => {
                write!(f, "edge {edge:?} names a missing node or port")
            }
            GraphError::DanglingEventEdge(edge) => {
                write!(f, "event edge {edge:?} names a missing node")
            }
            GraphError::NoEventOutput(edge) => {
                write!(f, "node {} has no event output to connect from", edge.from)
            }
            GraphError::Cycle(nodes) => {
                write!(f, "the graph has a feedback loop through nodes {nodes:?}")
            }
        }
    }
}

impl std::error::Error for GraphError {}

/// One upstream signal feeding an input port.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioSource {
    /// The edge this came from: the stable identity of its delay line.
    pub edge: Edge,
    /// First buffer of the upstream output port; its channels follow consecutively.
    pub first_buffer: usize,
    pub channels: u32,
    /// Compensation delay in samples to apply to this source before summing.
    pub delay: u32,
}

/// An input port of a step: its sources are summed, in this order, always.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputPlan {
    pub channels: u32,
    pub sources: Vec<AudioSource>,
}

/// An output port of a step and the buffers it owns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OutputPlan {
    pub first_buffer: usize,
    pub channels: u32,
}

/// One node's turn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    pub node: NodeIndex,
    pub inputs: Vec<InputPlan>,
    pub outputs: Vec<OutputPlan>,
    /// Steps (by position in [`Plan::steps`]) whose event output this node reads, in order.
    pub event_sources: Vec<usize>,
    /// How many distinct steps must finish before this one can start.
    pub dependencies: u32,
    /// Steps that are waiting on this one.
    pub dependents: Vec<usize>,
    /// Latency accumulated on the way into this node, after compensation: every input arrives
    /// this many samples late relative to the timeline.
    pub input_latency: u32,
    /// `input_latency` plus the node's own latency.
    pub output_latency: u32,
}

/// The compiled graph.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Plan {
    /// In an order that respects every edge. Running them first to last is always valid.
    pub steps: Vec<Step>,
    /// For each node, its position in `steps`.
    pub step_of_node: Vec<usize>,
    /// Number of mono buffers the plan uses. Every output port has its own; nothing is shared,
    /// so steps with no dependency between them can run at the same time.
    pub buffer_count: usize,
    /// The largest `output_latency` of any node: how late the final output is.
    pub max_latency: u32,
}

/// Compiles `spec`.
///
/// The result is a pure function of the spec: the same nodes and the same *set* of edges give
/// the same plan whatever order the edges were listed in. Sources into a port are ordered by
/// (upstream node, upstream port), which fixes the order of floating-point summation.
pub fn compile(spec: &GraphSpec) -> Result<Plan, GraphError> {
    let node_count = spec.nodes.len();

    // Validate and normalise the edges.
    let mut edges = spec.edges.clone();
    edges.sort_unstable();
    edges.dedup();
    for edge in &edges {
        let ok = spec
            .nodes
            .get(edge.from)
            .is_some_and(|n| edge.from_port < n.outputs.len())
            && spec
                .nodes
                .get(edge.to)
                .is_some_and(|n| edge.to_port < n.inputs.len());
        if !ok {
            return Err(GraphError::DanglingEdge(*edge));
        }
    }
    let mut event_edges = spec.event_edges.clone();
    event_edges.sort_unstable();
    event_edges.dedup();
    for edge in &event_edges {
        if edge.from >= node_count || edge.to >= node_count {
            return Err(GraphError::DanglingEventEdge(*edge));
        }
        if !spec.nodes[edge.from].event_output {
            return Err(GraphError::NoEventOutput(*edge));
        }
    }

    // Distinct upstream nodes per node, across both kinds of edge.
    let mut upstream: Vec<BTreeSet<NodeIndex>> = vec![BTreeSet::new(); node_count];
    let mut downstream: Vec<BTreeSet<NodeIndex>> = vec![BTreeSet::new(); node_count];
    let pairs = edges
        .iter()
        .map(|e| (e.from, e.to))
        .chain(event_edges.iter().map(|e| (e.from, e.to)));
    for (from, to) in pairs {
        upstream[to].insert(from);
        downstream[from].insert(to);
    }

    // Kahn's algorithm, always taking the ready node with the lowest index: deterministic.
    let mut pending: Vec<usize> = upstream.iter().map(BTreeSet::len).collect();
    let mut ready: BTreeSet<NodeIndex> = (0..node_count).filter(|&n| pending[n] == 0).collect();
    let mut order = Vec::with_capacity(node_count);
    while let Some(node) = ready.pop_first() {
        order.push(node);
        for &next in &downstream[node] {
            pending[next] -= 1;
            if pending[next] == 0 {
                ready.insert(next);
            }
        }
    }
    if order.len() != node_count {
        let stuck = (0..node_count).filter(|&n| pending[n] > 0).collect();
        return Err(GraphError::Cycle(stuck));
    }
    let mut step_of_node = vec![0; node_count];
    for (position, &node) in order.iter().enumerate() {
        step_of_node[node] = position;
    }

    // A private run of buffers for every output port, in step order.
    let mut first_buffer: Vec<Vec<usize>> = vec![Vec::new(); node_count];
    let mut buffer_count = 0;
    for &node in &order {
        for &channels in &spec.nodes[node].outputs {
            first_buffer[node].push(buffer_count);
            buffer_count += channels as usize;
        }
    }

    // Latency: each node's inputs are aligned to its latest-arriving source.
    let mut output_latency = vec![0u32; node_count];
    let mut steps = Vec::with_capacity(node_count);
    for &node in &order {
        let spec_node = &spec.nodes[node];
        let incoming: Vec<&Edge> = edges.iter().filter(|e| e.to == node).collect();
        let input_latency = incoming
            .iter()
            .map(|e| output_latency[e.from])
            .max()
            .unwrap_or(0);
        output_latency[node] = input_latency + spec_node.latency;

        let inputs = spec_node
            .inputs
            .iter()
            .enumerate()
            .map(|(port, &channels)| InputPlan {
                channels,
                sources: incoming
                    .iter()
                    .filter(|e| e.to_port == port)
                    .map(|e| AudioSource {
                        edge: **e,
                        first_buffer: first_buffer[e.from][e.from_port],
                        channels: spec.nodes[e.from].outputs[e.from_port],
                        delay: input_latency - output_latency[e.from],
                    })
                    .collect(),
            })
            .collect();
        let outputs = spec_node
            .outputs
            .iter()
            .enumerate()
            .map(|(port, &channels)| OutputPlan {
                first_buffer: first_buffer[node][port],
                channels,
            })
            .collect();
        let event_sources = event_edges
            .iter()
            .filter(|e| e.to == node)
            .map(|e| step_of_node[e.from])
            .collect();

        steps.push(Step {
            node,
            inputs,
            outputs,
            event_sources,
            dependencies: upstream[node].len() as u32,
            dependents: downstream[node].iter().map(|&n| step_of_node[n]).collect(),
            input_latency,
            output_latency: output_latency[node],
        });
    }
    for step in &mut steps {
        step.dependents.sort_unstable();
    }

    Ok(Plan {
        max_latency: output_latency.iter().copied().max().unwrap_or(0),
        steps,
        step_of_node,
        buffer_count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(inputs: &[u32], outputs: &[u32], latency: u32) -> NodeSpec {
        NodeSpec {
            inputs: inputs.to_vec(),
            outputs: outputs.to_vec(),
            latency,
            event_output: false,
        }
    }

    fn edge(from: NodeIndex, to: NodeIndex) -> Edge {
        Edge {
            from,
            from_port: 0,
            to,
            to_port: 0,
        }
    }

    #[test]
    fn a_dry_and_a_delayed_path_are_aligned_where_they_sum() {
        // source -> plugin(latency 64) -> mix
        //        `-------------------->´
        let spec = GraphSpec {
            nodes: vec![
                node(&[], &[2], 0),
                node(&[2], &[2], 64),
                node(&[2], &[2], 0),
            ],
            edges: vec![edge(0, 1), edge(1, 2), edge(0, 2)],
            event_edges: vec![],
        };
        let plan = compile(&spec).unwrap();
        assert_eq!(
            plan.steps.iter().map(|s| s.node).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        assert_eq!(plan.buffer_count, 6);
        assert_eq!(plan.max_latency, 64);

        let mix = &plan.steps[2];
        assert_eq!(mix.input_latency, 64);
        assert_eq!(mix.dependencies, 2);
        let sources = &mix.inputs[0].sources;
        assert_eq!(sources.len(), 2);
        // The dry path (from node 0) is delayed by the plugin's latency; the wet one is not.
        assert_eq!((sources[0].edge.from, sources[0].delay), (0, 64));
        assert_eq!((sources[1].edge.from, sources[1].delay), (1, 0));
        assert_eq!(sources[0].first_buffer, 0);
        assert_eq!(sources[1].first_buffer, 2);
        assert_eq!(plan.steps[0].dependents, vec![1, 2]);
    }

    #[test]
    fn order_ignores_how_the_spec_lists_things_and_respects_event_edges() {
        let mut spec = GraphSpec {
            nodes: vec![
                node(&[2], &[2], 0), // 0: strip, fed by 2
                NodeSpec {
                    inputs: vec![],
                    outputs: vec![],
                    latency: 0,
                    event_output: true,
                }, // 1: notes
                node(&[], &[2], 10), // 2: instrument, fed events by 1
            ],
            edges: vec![edge(2, 0)],
            event_edges: vec![EventEdge { from: 1, to: 2 }],
        };
        let plan = compile(&spec).unwrap();
        assert_eq!(
            plan.steps.iter().map(|s| s.node).collect::<Vec<_>>(),
            vec![1, 2, 0]
        );
        assert_eq!(plan.steps[1].event_sources, vec![0]);
        assert_eq!(plan.steps[1].dependencies, 1);
        assert_eq!(plan.steps[2].input_latency, 10);

        spec.edges.push(edge(2, 0)); // a duplicate changes nothing
        assert_eq!(compile(&spec).unwrap(), plan);
    }

    #[test]
    fn errors() {
        let two = vec![node(&[2], &[2], 0), node(&[2], &[2], 0)];
        let cyclic = GraphSpec {
            nodes: two.clone(),
            edges: vec![edge(0, 1), edge(1, 0)],
            event_edges: vec![],
        };
        assert_eq!(compile(&cyclic), Err(GraphError::Cycle(vec![0, 1])));

        let dangling = GraphSpec {
            nodes: two.clone(),
            edges: vec![edge(0, 7)],
            event_edges: vec![],
        };
        assert!(matches!(
            compile(&dangling),
            Err(GraphError::DanglingEdge(_))
        ));
        let bad_port = Edge {
            from: 0,
            from_port: 3,
            to: 1,
            to_port: 0,
        };
        let dangling = GraphSpec {
            nodes: two.clone(),
            edges: vec![bad_port],
            event_edges: vec![],
        };
        assert!(matches!(
            compile(&dangling),
            Err(GraphError::DanglingEdge(_))
        ));

        let silent = GraphSpec {
            nodes: two,
            edges: vec![],
            event_edges: vec![EventEdge { from: 0, to: 1 }],
        };
        assert!(matches!(
            compile(&silent),
            Err(GraphError::NoEventOutput(_))
        ));
        assert_eq!(compile(&GraphSpec::default()).unwrap(), Plan::default());
    }
}
