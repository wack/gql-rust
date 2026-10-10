//! Storage failures reach the caller as errors, never panics, and every
//! backend reports an unknown id as `NotFound`. `Remote` is
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

/// The `GraphStore` id contract, for any backend: an element that exists
/// with no labels, properties or edges answers with empty results, and an id
/// the store does not hold answers `NotFound` from every method, including
/// when it reaches a query as a parameter.
async fn checks_ids(g: &mut dyn GraphStoreMut) {
    let a = g.add_node(vec![], BTreeMap::new()).await.unwrap();
    let b = g.add_node(vec![], BTreeMap::new()).await.unwrap();
    let e = g.add_edge(a, b, vec![], BTreeMap::new()).await.unwrap();

    // Bare elements exist: empty answers, not errors.
    assert!(g.node_labels(b).await.unwrap().is_empty());
    assert!(!g.node_has_label(b, "X").await.unwrap());
    assert_eq!(g.node_property(b, "k").await.unwrap(), None);
    assert!(g.node_properties(b).await.unwrap().is_empty());
    assert!(g.edges(b, Incidence::Outgoing).await.unwrap().is_empty());
    assert_eq!(g.edges(b, Incidence::Incoming).await.unwrap().len(), 1);
    assert_eq!(g.edge(e).await.unwrap().target, b);
    assert!(g.edge_labels(e).await.unwrap().is_empty());
    assert!(!g.edge_has_label(e, "R").await.unwrap());
    assert_eq!(g.edge_property(e, "k").await.unwrap(), None);
    assert!(g.edge_properties(e).await.unwrap().is_empty());
    g.set_node_property(b, "k", Value::Null).await.unwrap();

    // Unknown ids: NotFound from every method.
    let (ghost, ghost_edge) = (NodeId(42), EdgeId(42));
    let not_found = |what: &str, r: StoreResult<()>| match r {
        Err(err) => assert_eq!(err.kind(), StoreErrorKind::NotFound, "{what}: {err}"),
        Ok(()) => panic!("{what}: unknown id answered Ok"),
    };
    not_found("node_labels", g.node_labels(ghost).await.map(drop));
    not_found(
        "node_has_label",
        g.node_has_label(ghost, "X").await.map(drop),
    );
    not_found("node_property", g.node_property(ghost, "k").await.map(drop));
    not_found("node_properties", g.node_properties(ghost).await.map(drop));
    for inc in [Incidence::Outgoing, Incidence::Incoming] {
        not_found("edges", g.edges(ghost, inc).await.map(drop));
    }
    not_found("edge", g.edge(ghost_edge).await.map(drop));
    not_found("edge_labels", g.edge_labels(ghost_edge).await.map(drop));
    not_found(
        "edge_has_label",
        g.edge_has_label(ghost_edge, "R").await.map(drop),
    );
    not_found(
        "edge_property",
        g.edge_property(ghost_edge, "k").await.map(drop),
    );
    not_found(
        "edge_properties",
        g.edge_properties(ghost_edge).await.map(drop),
    );
    let add = g
        .add_edge(a, ghost, vec!["R".into()], BTreeMap::new())
        .await;
    not_found("add_edge", add.map(drop));
    not_found("set", g.set_node_property(ghost, "k", Value::Int(1)).await);
    not_found("remove", g.set_node_property(ghost, "k", Value::Null).await);

    // Through query parameters: an error, not a null.
    let params = Params::from([
        ("n".to_string(), Value::Node(ghost)),
        ("r".to_string(), Value::Edge(ghost_edge)),
    ]);
    for q in [
        "RETURN $n.k",
        "RETURN $n['k']",
        "RETURN labels($n)",
        "RETURN $n IS LABELED X",
        "RETURN $r.k",
        "RETURN type($r)",
    ] {
        match query(&*g, q, &params).await {
            Err(Error::Store(err)) => assert_eq!(err.kind(), StoreErrorKind::NotFound, "{q}"),
            other => panic!("{q}: expected NotFound, got {other:?}"),
        }
    }

    // Nothing above changed the graph.
    assert_eq!(g.nodes().await.unwrap(), [a, b]);
    assert!(g.edges(a, Incidence::Outgoing).await.unwrap().len() == 1);
}

#[test]
fn petgraph_checks_ids() {
    block_on(checks_ids(&mut PetgraphStore::new()));
}

#[test]
fn remote_checks_ids() {
    block_on(checks_ids(&mut Remote::new(usize::MAX)));
}

#[cfg(feature = "turso")]
#[test]
fn turso_checks_ids() {
    let mut g = block_on(slcr_gql::TursoStore::in_memory()).unwrap();
    block_on(checks_ids(&mut g));
}
