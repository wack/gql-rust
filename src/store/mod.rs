//! Storage abstraction. The evaluator only talks to a [`GraphStore`], so SLCR
//! can back queries with its own in-memory specification graph; the
//! [`petgraph`](PetgraphStore) implementation is the reference backend.

mod petgraph_store;

pub use petgraph_store::PetgraphStore;

use crate::value::Value;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EdgeId(pub u64);

/// An edge as seen from one of its endpoints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EdgeRef {
    pub id: EdgeId,
    pub source: NodeId,
    pub target: NodeId,
}

/// Which incident edges to enumerate from a node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Incidence {
    Outgoing,
    Incoming,
}

/// Read-only property-graph access used by the query evaluator.
///
/// Node and edge iteration order must be deterministic for a given graph
/// state; SLCR relies on this for reproducible output when a query has no
/// `ORDER BY`.
pub trait GraphStore {
    fn nodes(&self) -> Box<dyn Iterator<Item = NodeId> + '_>;

    /// Nodes carrying `label`. Backends with a label index should override.
    fn nodes_with_label<'a>(&'a self, label: &'a str) -> Box<dyn Iterator<Item = NodeId> + 'a> {
        Box::new(self.nodes().filter(move |n| self.node_has_label(*n, label)))
    }

    fn node_labels(&self, node: NodeId) -> Vec<String>;

    fn node_has_label(&self, node: NodeId, label: &str) -> bool {
        self.node_labels(node).iter().any(|l| l == label)
    }

    fn node_property(&self, node: NodeId, key: &str) -> Option<Value>;

    fn node_properties(&self, node: NodeId) -> BTreeMap<String, Value>;

    /// Edges incident to `node` in the given direction.
    fn edges(&self, node: NodeId, incidence: Incidence) -> Box<dyn Iterator<Item = EdgeRef> + '_>;

    fn edge(&self, edge: EdgeId) -> EdgeRef;

    fn edge_labels(&self, edge: EdgeId) -> Vec<String>;

    fn edge_has_label(&self, edge: EdgeId, label: &str) -> bool {
        self.edge_labels(edge).iter().any(|l| l == label)
    }

    fn edge_property(&self, edge: EdgeId, key: &str) -> Option<Value>;

    fn edge_properties(&self, edge: EdgeId) -> BTreeMap<String, Value>;
}

/// Mutation used by fixtures and loaders. Not part of the query profile.
pub trait GraphStoreMut: GraphStore {
    fn add_node(&mut self, labels: Vec<String>, props: BTreeMap<String, Value>) -> NodeId;

    fn add_edge(
        &mut self,
        source: NodeId,
        target: NodeId,
        labels: Vec<String>,
        props: BTreeMap<String, Value>,
    ) -> EdgeId;

    /// Set (or, with null, remove) a node property. Used to overlay derived
    /// data such as satisfaction status.
    fn set_node_property(&mut self, node: NodeId, key: &str, value: Value);
}
