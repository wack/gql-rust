use super::{EdgeId, EdgeRef, GraphStore, GraphStoreMut, Incidence, NodeId, async_trait};
use crate::value::Value;
use petgraph::Direction;
use petgraph::stable_graph::{EdgeIndex, NodeIndex, StableDiGraph};
use petgraph::visit::EdgeRef as _;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Default)]
pub struct Element {
    /// Sorted, deduplicated.
    pub labels: Vec<String>,
    pub props: BTreeMap<String, Value>,
}

impl Element {
    fn new(mut labels: Vec<String>, mut props: BTreeMap<String, Value>) -> Self {
        labels.sort();
        labels.dedup();
        // Storing null is the same as not storing the property.
        props.retain(|_, v| !v.is_null());
        Self { labels, props }
    }
}

/// Reference in-memory backend over `petgraph::StableDiGraph`.
#[derive(Debug, Clone, Default)]
pub struct PetgraphStore {
    graph: StableDiGraph<Element, Element>,
}

impl PetgraphStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn graph(&self) -> &StableDiGraph<Element, Element> {
        &self.graph
    }

    pub fn node_count(&self) -> usize {
        self.graph.node_count()
    }

    pub fn edge_count(&self) -> usize {
        self.graph.edge_count()
    }

    fn n(id: NodeId) -> NodeIndex {
        NodeIndex::new(id.0 as usize)
    }

    fn e(id: EdgeId) -> EdgeIndex {
        EdgeIndex::new(id.0 as usize)
    }
}

#[async_trait]
impl GraphStore for PetgraphStore {
    async fn nodes(&self) -> Vec<NodeId> {
        self.graph
            .node_indices()
            .map(|i| NodeId(i.index() as u64))
            .collect()
    }

    async fn node_labels(&self, node: NodeId) -> Vec<String> {
        self.graph[Self::n(node)].labels.clone()
    }

    async fn node_has_label(&self, node: NodeId, label: &str) -> bool {
        self.graph[Self::n(node)]
            .labels
            .binary_search_by(|l| l.as_str().cmp(label))
            .is_ok()
    }

    async fn node_property(&self, node: NodeId, key: &str) -> Option<Value> {
        self.graph[Self::n(node)].props.get(key).cloned()
    }

    async fn node_properties(&self, node: NodeId) -> BTreeMap<String, Value> {
        self.graph[Self::n(node)].props.clone()
    }

    async fn edges(&self, node: NodeId, incidence: Incidence) -> Vec<EdgeRef> {
        let dir = match incidence {
            Incidence::Outgoing => Direction::Outgoing,
            Incidence::Incoming => Direction::Incoming,
        };
        // petgraph yields edges newest-first; reverse for insertion order.
        let mut v: Vec<EdgeRef> = self
            .graph
            .edges_directed(Self::n(node), dir)
            .map(|e| EdgeRef {
                id: EdgeId(e.id().index() as u64),
                source: NodeId(e.source().index() as u64),
                target: NodeId(e.target().index() as u64),
            })
            .collect();
        v.sort_by_key(|e| e.id);
        v
    }

    async fn edge(&self, edge: EdgeId) -> EdgeRef {
        let (s, t) = self
            .graph
            .edge_endpoints(Self::e(edge))
            .expect("dangling edge id");
        EdgeRef {
            id: edge,
            source: NodeId(s.index() as u64),
            target: NodeId(t.index() as u64),
        }
    }

    async fn edge_labels(&self, edge: EdgeId) -> Vec<String> {
        self.graph[Self::e(edge)].labels.clone()
    }

    async fn edge_property(&self, edge: EdgeId, key: &str) -> Option<Value> {
        self.graph[Self::e(edge)].props.get(key).cloned()
    }

    async fn edge_properties(&self, edge: EdgeId) -> BTreeMap<String, Value> {
        self.graph[Self::e(edge)].props.clone()
    }
}

#[async_trait]
impl GraphStoreMut for PetgraphStore {
    async fn add_node(&mut self, labels: Vec<String>, props: BTreeMap<String, Value>) -> NodeId {
        NodeId(self.graph.add_node(Element::new(labels, props)).index() as u64)
    }

    async fn add_edge(
        &mut self,
        source: NodeId,
        target: NodeId,
        labels: Vec<String>,
        props: BTreeMap<String, Value>,
    ) -> EdgeId {
        let idx = self.graph.add_edge(
            Self::n(source),
            Self::n(target),
            Element::new(labels, props),
        );
        EdgeId(idx.index() as u64)
    }

    async fn set_node_property(&mut self, node: NodeId, key: &str, value: Value) {
        let props = &mut self.graph[Self::n(node)].props;
        if value.is_null() {
            props.remove(key);
        } else {
            props.insert(key.to_string(), value);
        }
    }
}
