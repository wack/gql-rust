//! Storage abstraction. The evaluator only talks to a [`GraphStore`], so SLCR
//! can back queries with its own in-memory specification graph; the
//! [`petgraph`](PetgraphStore) implementation is the reference backend. With
//! the `turso` feature, `TursoStore` keeps the graph in a Turso database,
//! in memory or in a file.

mod petgraph_store;
#[cfg(feature = "turso")]
pub mod turso_store;

pub use petgraph_store::PetgraphStore;
#[cfg(feature = "turso")]
pub use turso_store::{TursoLocation, TursoStore, TursoStoreError};

use crate::value::Value;
pub use async_trait::async_trait;
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
/// Every method is async, so a backend can do I/O (a database, a remote
/// service) without blocking. Implement it with [`#[async_trait]`](macro@async_trait); the
/// returned futures are `Send`, so queries can run on a multi-threaded
/// runtime.
///
/// Node and edge enumeration order must be deterministic for a given graph
/// state; SLCR relies on this for reproducible output when a query has no
/// `ORDER BY`.
#[async_trait]
pub trait GraphStore: Send + Sync {
    async fn nodes(&self) -> Vec<NodeId>;

    /// Nodes carrying `label`. Backends with a label index should override.
    async fn nodes_with_label(&self, label: &str) -> Vec<NodeId> {
        let mut out = Vec::new();
        for n in self.nodes().await {
            if self.node_has_label(n, label).await {
                out.push(n);
            }
        }
        out
    }

    async fn node_labels(&self, node: NodeId) -> Vec<String>;

    async fn node_has_label(&self, node: NodeId, label: &str) -> bool {
        self.node_labels(node).await.iter().any(|l| l == label)
    }

    async fn node_property(&self, node: NodeId, key: &str) -> Option<Value>;

    async fn node_properties(&self, node: NodeId) -> BTreeMap<String, Value>;

    /// Edges incident to `node` in the given direction.
    async fn edges(&self, node: NodeId, incidence: Incidence) -> Vec<EdgeRef>;

    async fn edge(&self, edge: EdgeId) -> EdgeRef;

    async fn edge_labels(&self, edge: EdgeId) -> Vec<String>;

    async fn edge_has_label(&self, edge: EdgeId, label: &str) -> bool {
        self.edge_labels(edge).await.iter().any(|l| l == label)
    }

    async fn edge_property(&self, edge: EdgeId, key: &str) -> Option<Value>;

    async fn edge_properties(&self, edge: EdgeId) -> BTreeMap<String, Value>;
}

/// Mutation used by fixtures and loaders. Not part of the query profile.
#[async_trait]
pub trait GraphStoreMut: GraphStore {
    async fn add_node(&mut self, labels: Vec<String>, props: BTreeMap<String, Value>) -> NodeId;

    async fn add_edge(
        &mut self,
        source: NodeId,
        target: NodeId,
        labels: Vec<String>,
        props: BTreeMap<String, Value>,
    ) -> EdgeId;

    /// Set (or, with null, remove) a node property. Used to overlay derived
    /// data such as satisfaction status.
    async fn set_node_property(&mut self, node: NodeId, key: &str, value: Value);
}
