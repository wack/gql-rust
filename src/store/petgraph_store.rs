use super::{
    EdgeId, EdgeRef, GraphStore, GraphStoreMut, Incidence, NodeId, StoreError, StoreErrorKind,
    StoreResult, async_trait,
};
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

    fn node(&self, id: NodeId) -> StoreResult<&Element> {
        self.graph
            .node_weight(Self::n(id))
            .ok_or_else(|| not_found("node", id.0))
    }

    fn edge_element(&self, id: EdgeId) -> StoreResult<&Element> {
        self.graph
            .edge_weight(Self::e(id))
            .ok_or_else(|| not_found("edge", id.0))
    }
}

#[async_trait]
impl GraphStore for PetgraphStore {
    async fn nodes(&self) -> StoreResult<Vec<NodeId>> {
        Ok(self
            .graph
            .node_indices()
            .map(|i| NodeId(i.index() as u64))
            .collect())
    }

    async fn node_labels(&self, node: NodeId) -> StoreResult<Vec<String>> {
        Ok(self.node(node)?.labels.clone())
    }

    async fn node_has_label(&self, node: NodeId, label: &str) -> StoreResult<bool> {
        Ok(self
            .node(node)?
            .labels
            .binary_search_by(|l| l.as_str().cmp(label))
            .is_ok())
    }

    async fn node_property(&self, node: NodeId, key: &str) -> StoreResult<Option<Value>> {
        Ok(self.node(node)?.props.get(key).cloned())
    }

    async fn node_properties(&self, node: NodeId) -> StoreResult<BTreeMap<String, Value>> {
        Ok(self.node(node)?.props.clone())
    }

    async fn edges(&self, node: NodeId, incidence: Incidence) -> StoreResult<Vec<EdgeRef>> {
        self.node(node)?;
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
        Ok(v)
    }

    async fn edge(&self, edge: EdgeId) -> StoreResult<EdgeRef> {
        let (s, t) = self
            .graph
            .edge_endpoints(Self::e(edge))
            .ok_or_else(|| not_found("edge", edge.0))?;
        Ok(EdgeRef {
            id: edge,
            source: NodeId(s.index() as u64),
            target: NodeId(t.index() as u64),
        })
    }

    async fn edge_labels(&self, edge: EdgeId) -> StoreResult<Vec<String>> {
        Ok(self.edge_element(edge)?.labels.clone())
    }

    async fn edge_property(&self, edge: EdgeId, key: &str) -> StoreResult<Option<Value>> {
        Ok(self.edge_element(edge)?.props.get(key).cloned())
    }

    async fn edge_properties(&self, edge: EdgeId) -> StoreResult<BTreeMap<String, Value>> {
        Ok(self.edge_element(edge)?.props.clone())
    }
}

#[async_trait]
impl GraphStoreMut for PetgraphStore {
    async fn add_node(
        &mut self,
        labels: Vec<String>,
        props: BTreeMap<String, Value>,
    ) -> StoreResult<NodeId> {
        Ok(NodeId(
            self.graph.add_node(Element::new(labels, props)).index() as u64,
        ))
    }

    async fn add_edge(
        &mut self,
        source: NodeId,
        target: NodeId,
        labels: Vec<String>,
        props: BTreeMap<String, Value>,
    ) -> StoreResult<EdgeId> {
        self.node(source)?;
        self.node(target)?;
        let idx = self.graph.add_edge(
            Self::n(source),
            Self::n(target),
            Element::new(labels, props),
        );
        Ok(EdgeId(idx.index() as u64))
    }

    async fn set_node_property(
        &mut self,
        node: NodeId,
        key: &str,
        value: Value,
    ) -> StoreResult<()> {
        let props = &mut self
            .graph
            .node_weight_mut(Self::n(node))
            .ok_or_else(|| not_found("node", node.0))?
            .props;
        if value.is_null() {
            props.remove(key);
        } else {
            props.insert(key.to_string(), value);
        }
        Ok(())
    }
}

fn not_found(what: &str, id: u64) -> StoreError {
    StoreError::new(StoreErrorKind::NotFound, format!("no {what} with id {id}"))
}
