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
use std::fmt;
use std::sync::Arc;

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

/// Why a storage operation failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum StoreErrorKind {
    /// An id that does not name an element of this store.
    NotFound,
    /// The storage could not be reached or is busy: a network failure, a
    /// timeout, a lock held by another writer. Retrying may succeed.
    Unavailable,
    /// Stored data could not be decoded.
    Corrupt,
    /// Any other failure.
    Other,
}

/// A failed [`GraphStore`] or [`GraphStoreMut`] operation. Backends wrap
/// their own errors, which stay reachable through
/// [`source`](std::error::Error::source).
///
/// ```
/// use slcr_gql::store::{StoreError, StoreErrorKind};
///
/// let io = std::io::Error::new(std::io::ErrorKind::TimedOut, "no reply from graph.example");
/// let e = StoreError::new(StoreErrorKind::Unavailable, io);
/// assert_eq!(e.kind(), StoreErrorKind::Unavailable);
/// assert_eq!(e.to_string(), "storage unavailable: no reply from graph.example");
/// ```
#[derive(Debug, Clone)]
pub struct StoreError {
    kind: StoreErrorKind,
    source: Arc<dyn std::error::Error + Send + Sync>,
}

impl StoreError {
    /// Wrap `error` (any error type, or a message) as a failure of `kind`.
    pub fn new(
        kind: StoreErrorKind,
        error: impl Into<Box<dyn std::error::Error + Send + Sync>>,
    ) -> Self {
        StoreError {
            kind,
            source: Arc::from(error.into()),
        }
    }

    pub fn kind(&self) -> StoreErrorKind {
        self.kind
    }
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self.kind {
            StoreErrorKind::NotFound => "not found",
            StoreErrorKind::Unavailable => "storage unavailable",
            StoreErrorKind::Corrupt => "corrupt stored data",
            StoreErrorKind::Other => "storage error",
        };
        write!(f, "{kind}: {}", self.source)
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&*self.source)
    }
}

/// Equal when the kind and the message match, so results carrying a
/// `StoreError` stay comparable.
impl PartialEq for StoreError {
    fn eq(&self, other: &Self) -> bool {
        self.kind == other.kind && self.source.to_string() == other.source.to_string()
    }
}

pub type StoreResult<T> = Result<T, StoreError>;

/// Read-only property-graph access used by the query evaluator.
///
/// Every method is async and fallible, so a backend can do I/O (a database,
/// a remote service) without blocking, and report failures as a
/// [`StoreError`] instead of panicking. Implement it with
/// [`#[async_trait]`](macro@async_trait); the returned futures are `Send`, so
/// queries can run on a multi-threaded runtime.
///
/// Every method that takes a node or edge id must answer an id the store
/// does not hold with [`StoreErrorKind::NotFound`], never with an empty
/// result: an element that exists with no labels, properties or edges must
/// stay distinguishable from one that does not exist. Ids can come from
/// outside the store, for example as query parameters. No method may panic.
///
/// Node and edge enumeration order must be deterministic for a given graph
/// state; SLCR relies on this for reproducible output when a query has no
/// `ORDER BY`.
#[async_trait]
pub trait GraphStore: Send + Sync {
    async fn nodes(&self) -> StoreResult<Vec<NodeId>>;

    /// Nodes carrying `label`. Backends with a label index should override.
    async fn nodes_with_label(&self, label: &str) -> StoreResult<Vec<NodeId>> {
        let mut out = Vec::new();
        for n in self.nodes().await? {
            if self.node_has_label(n, label).await? {
                out.push(n);
            }
        }
        Ok(out)
    }

    async fn node_labels(&self, node: NodeId) -> StoreResult<Vec<String>>;

    async fn node_has_label(&self, node: NodeId, label: &str) -> StoreResult<bool> {
        Ok(self.node_labels(node).await?.iter().any(|l| l == label))
    }

    async fn node_property(&self, node: NodeId, key: &str) -> StoreResult<Option<Value>>;

    async fn node_properties(&self, node: NodeId) -> StoreResult<BTreeMap<String, Value>>;

    /// Edges incident to `node` in the given direction.
    async fn edges(&self, node: NodeId, incidence: Incidence) -> StoreResult<Vec<EdgeRef>>;

    async fn edge(&self, edge: EdgeId) -> StoreResult<EdgeRef>;

    async fn edge_labels(&self, edge: EdgeId) -> StoreResult<Vec<String>>;

    async fn edge_has_label(&self, edge: EdgeId, label: &str) -> StoreResult<bool> {
        Ok(self.edge_labels(edge).await?.iter().any(|l| l == label))
    }

    async fn edge_property(&self, edge: EdgeId, key: &str) -> StoreResult<Option<Value>>;

    async fn edge_properties(&self, edge: EdgeId) -> StoreResult<BTreeMap<String, Value>>;
}

/// Mutation used by fixtures and loaders. Not part of the query profile.
#[async_trait]
pub trait GraphStoreMut: GraphStore {
    async fn add_node(
        &mut self,
        labels: Vec<String>,
        props: BTreeMap<String, Value>,
    ) -> StoreResult<NodeId>;

    async fn add_edge(
        &mut self,
        source: NodeId,
        target: NodeId,
        labels: Vec<String>,
        props: BTreeMap<String, Value>,
    ) -> StoreResult<EdgeId>;

    /// Set (or, with null, remove) a node property. Used to overlay derived
    /// data such as satisfaction status.
    async fn set_node_property(&mut self, node: NodeId, key: &str, value: Value)
    -> StoreResult<()>;
}
