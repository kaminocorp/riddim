//! Graph compilation: from "what is connected to what" to "what to run, in which order, into
//! which buffers, with which delays".
//!
//! This crate is a pure function and nothing else. It never touches a device, a plugin or a
//! thread, which is what makes it testable in isolation and by property.
//!
//! * [`compile`] turns a [`GraphSpec`] (nodes with ports and latencies, edges between ports)
//!   into a [`Plan`]: a deterministic topological order, a private buffer for every output,
//!   a delay on every edge that needs one for latency compensation, and the dependency counts
//!   a parallel executor needs.
//! * [`model_graph`] builds the `GraphSpec` for an engine [`Model`](riddim_model::Model).

mod compile;
pub mod model_graph;

pub use compile::{
    compile, AudioSource, Edge, EventEdge, GraphError, GraphSpec, InputPlan, NodeIndex, NodeSpec,
    OutputPlan, Plan, Step,
};
