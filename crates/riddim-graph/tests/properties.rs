//! Properties every compiled plan must have, checked on random graphs.

use std::collections::BTreeSet;

use proptest::prelude::*;
use riddim_graph::{compile, Edge, EventEdge, GraphError, GraphSpec, NodeSpec};

/// A random acyclic graph: nodes are given a hidden rank and edges only go up in rank, but the
/// node list itself is in arbitrary order, so the compiler cannot lean on it.
fn dags() -> impl Strategy<Value = GraphSpec> {
    (2usize..24).prop_flat_map(|count| {
        let nodes = prop::collection::vec((0u32..3, 0u32..3, 0u32..500, any::<bool>()), count);
        let ranks = Just((0..count).collect::<Vec<usize>>()).prop_shuffle();
        let edges = prop::collection::vec(
            (0..count, 0..count, 0usize..4, 0usize..4, any::<bool>()),
            0..80,
        );
        (nodes, ranks, edges).prop_map(|(nodes, rank, raw_edges)| {
            let nodes: Vec<NodeSpec> = nodes
                .into_iter()
                .map(|(inputs, outputs, latency, event_output)| NodeSpec {
                    inputs: (0..inputs).map(|i| 1 + i % 2).collect(),
                    outputs: (0..outputs).map(|i| 1 + (i + 1) % 2).collect(),
                    latency,
                    event_output,
                })
                .collect();
            let mut spec = GraphSpec {
                nodes,
                edges: Vec::new(),
                event_edges: Vec::new(),
            };
            for (a, b, from_port, to_port, as_event) in raw_edges {
                if rank[a] == rank[b] {
                    continue;
                }
                let (from, to) = if rank[a] < rank[b] { (a, b) } else { (b, a) };
                if as_event {
                    if spec.nodes[from].event_output {
                        spec.event_edges.push(EventEdge { from, to });
                    }
                } else if !spec.nodes[from].outputs.is_empty() && !spec.nodes[to].inputs.is_empty()
                {
                    spec.edges.push(Edge {
                        from,
                        from_port: from_port % spec.nodes[from].outputs.len(),
                        to,
                        to_port: to_port % spec.nodes[to].inputs.len(),
                    });
                }
            }
            spec
        })
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    #[test]
    fn plans_are_valid_orders_with_aligned_latency(spec in dags()) {
        let plan = compile(&spec).unwrap();
        let count = spec.nodes.len();

        // Every node exactly once.
        prop_assert_eq!(plan.steps.len(), count);
        let seen: BTreeSet<usize> = plan.steps.iter().map(|s| s.node).collect();
        prop_assert_eq!(seen.len(), count);
        for (position, step) in plan.steps.iter().enumerate() {
            prop_assert_eq!(plan.step_of_node[step.node], position);
        }

        // Every edge runs forwards.
        for edge in &spec.edges {
            prop_assert!(plan.step_of_node[edge.from] < plan.step_of_node[edge.to]);
        }
        for edge in &spec.event_edges {
            prop_assert!(plan.step_of_node[edge.from] < plan.step_of_node[edge.to]);
        }

        // Buffers: every output port owns its own, and sources point at the right ones.
        let mut owned = vec![false; plan.buffer_count];
        for step in &plan.steps {
            for (port, output) in step.outputs.iter().enumerate() {
                prop_assert_eq!(output.channels, spec.nodes[step.node].outputs[port]);
                for buffer in output.first_buffer..output.first_buffer + output.channels as usize {
                    prop_assert!(!owned[buffer], "buffer {} has two owners", buffer);
                    owned[buffer] = true;
                }
            }
        }
        prop_assert!(owned.iter().all(|&o| o));

        let mut max_latency = 0;
        for step in &plan.steps {
            let node = &spec.nodes[step.node];
            prop_assert_eq!(step.inputs.len(), node.inputs.len());
            prop_assert_eq!(step.output_latency, step.input_latency + node.latency);
            max_latency = max_latency.max(step.output_latency);

            let mut any_source = false;
            let mut any_undelayed = false;
            let mut feeders = BTreeSet::new();
            for (port, input) in step.inputs.iter().enumerate() {
                let mut previous = None;
                for source in &input.sources {
                    any_source = true;
                    any_undelayed |= source.delay == 0;
                    feeders.insert(source.edge.from);
                    prop_assert_eq!(source.edge.to, step.node);
                    prop_assert_eq!(source.edge.to_port, port);
                    let upstream = &plan.steps[plan.step_of_node[source.edge.from]];
                    let upstream_port = upstream.outputs[source.edge.from_port];
                    prop_assert_eq!(source.first_buffer, upstream_port.first_buffer);
                    prop_assert_eq!(source.channels, upstream_port.channels);
                    // The point of the exercise: after its delay, every source is equally late.
                    prop_assert_eq!(upstream.output_latency + source.delay, step.input_latency);
                    // Sources are in a fixed order, so summation order is fixed.
                    let key = (source.edge.from, source.edge.from_port);
                    prop_assert!(previous < Some(key));
                    previous = Some(key);
                }
            }
            // No needless delay: if anything feeds the node, something arrives undelayed.
            prop_assert!(!any_source || any_undelayed);

            for &source_step in &step.event_sources {
                feeders.insert(plan.steps[source_step].node);
            }
            prop_assert_eq!(step.dependencies as usize, feeders.len());
        }
        prop_assert_eq!(plan.max_latency, max_latency);

        // Every edge in the spec made it into the plan.
        let planned: BTreeSet<Edge> = plan
            .steps
            .iter()
            .flat_map(|s| s.inputs.iter().flat_map(|i| i.sources.iter().map(|s| s.edge)))
            .collect();
        prop_assert_eq!(planned, spec.edges.iter().copied().collect::<BTreeSet<Edge>>());
    }

    #[test]
    fn compilation_is_a_function_of_the_edge_set(spec in dags(), seed in any::<u64>()) {
        let plan = compile(&spec).unwrap();
        let mut shuffled = spec.clone();
        // A cheap deterministic shuffle, plus duplicates.
        let n = shuffled.edges.len();
        for i in 0..n {
            shuffled.edges.swap(i, (seed as usize).wrapping_mul(i + 7) % n);
        }
        shuffled.edges.extend(spec.edges.iter().take(3).copied());
        shuffled.event_edges.reverse();
        prop_assert_eq!(compile(&shuffled).unwrap(), plan);
    }

    #[test]
    fn the_dependency_counts_drive_a_complete_parallel_run(spec in dags(), choices in prop::collection::vec(any::<usize>(), 32)) {
        // Simulate a scheduler that starts any ready step, in an arbitrary order, and releases
        // dependents as steps finish. It must run everything, and never a step before its inputs.
        let plan = compile(&spec).unwrap();
        let mut remaining: Vec<u32> = plan.steps.iter().map(|s| s.dependencies).collect();
        let mut ready: Vec<usize> = (0..plan.steps.len()).filter(|&s| remaining[s] == 0).collect();
        let mut done = vec![false; plan.steps.len()];
        let mut turn = 0;
        while !ready.is_empty() {
            let pick = choices[turn % choices.len()] % ready.len();
            turn += 1;
            let step = ready.swap_remove(pick);
            for input in &plan.steps[step].inputs {
                for source in &input.sources {
                    prop_assert!(done[plan.step_of_node[source.edge.from]], "ran before its input");
                }
            }
            done[step] = true;
            for &dependent in &plan.steps[step].dependents {
                remaining[dependent] -= 1;
                if remaining[dependent] == 0 {
                    ready.push(dependent);
                }
            }
        }
        prop_assert!(done.iter().all(|&d| d), "the schedule stalled");
    }

    #[test]
    fn any_back_edge_is_reported_as_a_cycle(spec in dags()) {
        let plan = compile(&spec).unwrap();
        // Take a real edge and add its reverse.
        if let Some(edge) = spec.edges.first() {
            if !spec.nodes[edge.to].outputs.is_empty() && !spec.nodes[edge.from].inputs.is_empty() {
                let mut cyclic = spec.clone();
                cyclic.edges.push(Edge { from: edge.to, from_port: 0, to: edge.from, to_port: 0 });
                match compile(&cyclic) {
                    Err(GraphError::Cycle(nodes)) => {
                        prop_assert!(nodes.contains(&edge.from) && nodes.contains(&edge.to));
                    }
                    other => prop_assert!(false, "expected a cycle, got {:?}", other.map(|p| p.steps.len())),
                }
            }
        }
        prop_assert!(plan.steps.len() == spec.nodes.len());
    }
}
