//! Storage failures reach the caller as errors, never panics. `Remote` is
//! shaped like a network-backed store: it implements the traits the way an
//! external backend would, and its "network" can drop after a number of
//! calls.

use pollster::block_on;
use slcr_gql::store::{EdgeId, EdgeRef, Incidence, NodeId};
use slcr_gql::{
    Error, GraphStore, GraphStoreMut, Params, PetgraphStore, SpecError, StoreError, StoreErrorKind,
    StoreResult, Value, apply_overlay, async_trait, load_fixture, load_spec, query,
};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};

const TODO: &str = include_str!("data/todo-api.spec.json");

struct Remote {
    graph: PetgraphStore,
    /// Calls that succeed before the connection drops.
    budget: AtomicUsize,
}

impl Remote {
    fn new(budget: usize) -> Self {
        Remote {
            graph: PetgraphStore::new(),
            budget: AtomicUsize::new(budget),
        }
    }

    fn call(&self) -> StoreResult<()> {
        let left = self.budget.load(Ordering::SeqCst);
        if left == 0 {
            let io = std::io::Error::new(std::io::ErrorKind::ConnectionReset, "connection reset");
            return Err(StoreError::new(StoreErrorKind::Unavailable, io));
        }
        self.budget.store(left - 1, Ordering::SeqCst);
        Ok(())
    }
}

#[async_trait]
impl GraphStore for Remote {
    async fn nodes(&self) -> StoreResult<Vec<NodeId>> {
        self.call()?;
        self.graph.nodes().await
    }

    async fn node_labels(&self, node: NodeId) -> StoreResult<Vec<String>> {
        self.call()?;
        self.graph.node_labels(node).await
    }

    async fn node_property(&self, node: NodeId, key: &str) -> StoreResult<Option<Value>> {
        self.call()?;
        self.graph.node_property(node, key).await
    }

    async fn node_properties(&self, node: NodeId) -> StoreResult<BTreeMap<String, Value>> {
        self.call()?;
        self.graph.node_properties(node).await
    }

    async fn edges(&self, node: NodeId, incidence: Incidence) -> StoreResult<Vec<EdgeRef>> {
        self.call()?;
        self.graph.edges(node, incidence).await
    }

    async fn edge(&self, edge: EdgeId) -> StoreResult<EdgeRef> {
        self.call()?;
        self.graph.edge(edge).await
    }

    async fn edge_labels(&self, edge: EdgeId) -> StoreResult<Vec<String>> {
        self.call()?;
        self.graph.edge_labels(edge).await
    }

    async fn edge_property(&self, edge: EdgeId, key: &str) -> StoreResult<Option<Value>> {
        self.call()?;
        self.graph.edge_property(edge, key).await
    }

    async fn edge_properties(&self, edge: EdgeId) -> StoreResult<BTreeMap<String, Value>> {
        self.call()?;
        self.graph.edge_properties(edge).await
    }
}

#[async_trait]
impl GraphStoreMut for Remote {
    async fn add_node(
        &mut self,
        labels: Vec<String>,
        props: BTreeMap<String, Value>,
    ) -> StoreResult<NodeId> {
        self.call()?;
        self.graph.add_node(labels, props).await
    }

    async fn add_edge(
        &mut self,
        source: NodeId,
        target: NodeId,
        labels: Vec<String>,
        props: BTreeMap<String, Value>,
    ) -> StoreResult<EdgeId> {
        self.call()?;
        self.graph.add_edge(source, target, labels, props).await
    }

    async fn set_node_property(
        &mut self,
        node: NodeId,
        key: &str,
        value: Value,
    ) -> StoreResult<()> {
        self.call()?;
        self.graph.set_node_property(node, key, value).await
    }
}

fn unavailable(e: &StoreError) {
    assert_eq!(e.kind(), StoreErrorKind::Unavailable, "{e}");
    let io = std::error::Error::source(e)
        .and_then(|s| s.downcast_ref::<std::io::Error>())
        .expect("the backend's own error is the source");
    assert_eq!(io.kind(), std::io::ErrorKind::ConnectionReset);
}

#[test]
fn a_dropped_connection_fails_the_query() {
    let q = "MATCH (d:Requirement)-[:REFINES]->{1,}(:Requirement {id: 'REQ-001'}) \
             RETURN DISTINCT d.id ORDER BY d.id";
    let mut g = Remote::new(usize::MAX);
    block_on(load_spec(TODO, &mut g)).unwrap();
    let want = block_on(query(&g, q, &Params::new())).unwrap();

    // Cut the connection at every point the query touches the store.
    let mut failed = 0;
    for budget in 0.. {
        g.budget.store(budget, Ordering::SeqCst);
        match block_on(query(&g, q, &Params::new())) {
            Ok(r) => {
                assert_eq!(r, want);
                break;
            }
            Err(Error::Store(e)) => {
                unavailable(&e);
                assert!(e.to_string().contains("connection reset"));
                failed += 1;
            }
            Err(other) => panic!("budget {budget}: {other}"),
        }
    }
    assert!(failed > 10, "the query only made {failed} store calls");
}

#[test]
fn loaders_report_store_errors() {
    match block_on(load_fixture(&mut Remote::new(1), "INSERT (:A)-[:R]->(:B)")) {
        Err(Error::Store(e)) => unavailable(&e),
        other => panic!("{other:?}"),
    }
    match block_on(load_spec(TODO, &mut Remote::new(5))) {
        Err(SpecError::Store(e)) => unavailable(&e),
        other => panic!("{other:?}"),
    }
    let mut g = Remote::new(usize::MAX);
    let index = block_on(load_spec(TODO, &mut g)).unwrap();
    g.budget.store(0, Ordering::SeqCst);
    match block_on(apply_overlay(
        r#"{"REQ-001": {"status": "x"}}"#,
        &index,
        &mut g,
    )) {
        Err(SpecError::Store(e)) => unavailable(&e),
        other => panic!("{other:?}"),
    }
}

#[test]
fn unknown_ids_are_not_found() {
    let mut g = PetgraphStore::new();
    let a = block_on(g.add_node(vec![], BTreeMap::new())).unwrap();
    let ghost = NodeId(42);
    let not_found = |r: StoreResult<()>| {
        assert_eq!(r.unwrap_err().kind(), StoreErrorKind::NotFound);
    };
    not_found(block_on(g.node_labels(ghost)).map(drop));
    not_found(block_on(g.node_property(ghost, "k")).map(drop));
    not_found(block_on(g.edges(ghost, Incidence::Outgoing)).map(drop));
    not_found(block_on(g.edge(EdgeId(7))).map(drop));
    not_found(block_on(g.edge_labels(EdgeId(7))).map(drop));
    not_found(block_on(g.add_edge(a, ghost, vec!["R".into()], BTreeMap::new())).map(drop));
    not_found(block_on(g.set_node_property(ghost, "k", Value::Int(1))));
    assert_eq!(g.edge_count(), 0);
}
