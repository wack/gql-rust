//! The Turso backend: it must answer every query exactly as the reference
//! `PetgraphStore` does, and persist the graph when given a file.
//!
//! The TCK also runs against it: TCK_STORE=turso cargo test --features turso --test tck

#![cfg(feature = "turso")]

use pollster::block_on;
use slcr_gql::{
    GraphStore, GraphStoreMut, Params, PetgraphStore, TursoLocation, TursoStore, Value,
    apply_overlay, load_fixture, load_spec, query,
};
use std::collections::BTreeMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

const TODO: &str = include_str!("data/todo-api.spec.json");

/// Queries over the canon spec. Those without ORDER BY check that both stores
/// iterate in insertion order; those returning elements compare them by
/// content, since element ids are backend-specific.
const CORPUS: &[&str] = &[
    "MATCH (n) RETURN n",
    "MATCH (n) RETURN labels(n), n.id",
    "MATCH ()-[e]->() RETURN e",
    "MATCH (a)-[e]->(b) RETURN a.id, type(e), e.position, b.id",
    "MATCH (a)<-[e]-(b) RETURN a.id, type(e), b.id",
    "MATCH (a)-[e]-(b) RETURN a.id, b.id",
    "MATCH (r:Requirement) RETURN r.id",
    "MATCH (r:Requirement) RETURN r ORDER BY r.id",
    "MATCH (r:Requirement) RETURN count(*)",
    "MATCH (s:Section) RETURN s.title",
    "MATCH (r:Requirement {modality: 'MAY'}) RETURN r.id",
    "MATCH (:Requirement {id: 'REQ-001'})-[e:CONTAINS]->(c) RETURN c.id ORDER BY e.position",
    "MATCH (d:Requirement)-[:REFINES]->{1,}(:Requirement {id: 'REQ-001'}) RETURN d.id",
    "MATCH (d:Requirement)-[:REFINES]->{2,2}(t) RETURN d.id, t.id",
    "MATCH (d)-[es:REFINES]->{1,}(t) RETURN d.id, es, t.id",
    "MATCH (r:Requirement)<-[:REFINES]-(c:Requirement) RETURN r.id, count(c) AS n",
    "MATCH (s:Section)-[:CONTAINS]->{1,}(r:Requirement) RETURN s.id, count(r) AS n",
    "MATCH (r)-[:USES_TERM]->(t:Term) RETURN r.id, t",
    "MATCH (r:Requirement) WHERE r.refinement IS NULL RETURN r.id",
    "MATCH (r:Requirement) RETURN r.modality, count(*) AS n",
    "MATCH (n) WHERE n IS LABELED Requirement | Term RETURN n.id",
    "MATCH (n:Section|Term) RETURN n.id, n",
    "MATCH (n:!Requirement) RETURN n.id",
    "MATCH (n:%) RETURN count(*)",
    "MATCH (n:Nonexistent) RETURN n",
    "MATCH (r:Requirement) RETURN collect_list(r.id) AS ids",
    "MATCH (a:Requirement)-[:REFINES]->(p)<-[:REFINES]-(b:Requirement) WHERE a.id < b.id RETURN a.id, b.id, p.id",
    "MATCH (a)-[:DEPENDS_ON]->(b), (c)-[:REFINES]->(b) RETURN a.id, b.id, c.id",
];

fn render<'a>(
    g: &'a dyn GraphStore,
    v: &'a Value,
) -> Pin<Box<dyn Future<Output = String> + Send + 'a>> {
    Box::pin(async move {
        match v {
            Value::Node(n) => format!(
                "(:{} {})",
                g.node_labels(*n).await.join(":"),
                render(g, &Value::Map(g.node_properties(*n).await)).await
            ),
            Value::Edge(e) => format!(
                "[:{} {}]",
                g.edge_labels(*e).await.join(":"),
                render(g, &Value::Map(g.edge_properties(*e).await)).await
            ),
            Value::List(xs) => {
                let mut out = Vec::new();
                for x in xs {
                    out.push(render(g, x).await);
                }
                format!("[{}]", out.join(", "))
            }
            Value::Map(m) => {
                let mut kv = Vec::new();
                for (k, x) in m {
                    kv.push(format!("{k}: {}", render(g, x).await));
                }
                format!("{{{}}}", kv.join(", "))
            }
            other => other.to_string(),
        }
    })
}

async fn rows(g: &dyn GraphStore, q: &str) -> Vec<Vec<String>> {
    let r = query(g, q, &Params::new())
        .await
        .unwrap_or_else(|e| panic!("{q}: {e}"));
    let mut out = Vec::new();
    for row in &r.rows {
        let mut cells = Vec::new();
        for v in row {
            cells.push(render(g, v).await);
        }
        out.push(cells);
    }
    out
}

/// A fresh database file path under the target directory.
struct TempDb(PathBuf);

impl TempDb {
    fn new(name: &str) -> Self {
        let db = TempDb(PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("{name}.db")));
        db.remove();
        db
    }

    fn remove(&self) {
        for suffix in ["", "-wal", "-shm"] {
            let mut p = self.0.clone().into_os_string();
            p.push(suffix);
            let _ = std::fs::remove_file(p);
        }
    }

    fn conn(&self) -> String {
        self.0.to_str().unwrap().to_string()
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        self.remove();
    }
}

#[test]
fn canon_spec_matches_reference_backend() {
    block_on(async {
        let mut reference = PetgraphStore::new();
        let ref_index = load_spec(TODO, &mut reference).await.unwrap();
        let mut g = TursoStore::open(":memory:").await.unwrap();
        let index = g
            .transaction(async |g| load_spec(TODO, g).await)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(index.len(), ref_index.len());
        assert_eq!(g.node_count().await, reference.node_count());
        assert_eq!(g.edge_count().await, reference.edge_count());
        for q in CORPUS {
            let want = rows(&reference, q).await;
            assert!(!want.is_empty() || q.contains("Nonexistent"), "{q}");
            assert_eq!(rows(&g, q).await, want, "{q}");
        }
    });
}

#[test]
fn errors_match_reference_backend() {
    block_on(async {
        let mut reference = PetgraphStore::new();
        let mut g = TursoStore::in_memory().await.unwrap();
        let fixture = "INSERT (a {n: 1})-[:R]->(b {n: 0})";
        load_fixture(&mut reference, fixture).await.unwrap();
        load_fixture(&mut g, fixture).await.unwrap();
        for q in [
            "MATCH (a)-[:R]->(b) RETURN a.n / b.n",
            "MATCH (n) RETURN n.n + 'x'",
            "MATCH (n) WITH n RETURN n",
        ] {
            let want = query(&reference, q, &Params::new())
                .await
                .map_err(|e| e.to_string());
            let got = query(&g, q, &Params::new())
                .await
                .map_err(|e| e.to_string());
            assert!(want.is_err(), "{q}");
            assert_eq!(got.err(), want.err(), "{q}");
        }
    });
}

#[test]
fn in_memory_stores_are_independent() {
    block_on(async {
        let mut a = TursoStore::open(":memory:").await.unwrap();
        let b = TursoStore::open("file::memory:").await.unwrap();
        load_fixture(&mut a, "INSERT (:A), (:A)").await.unwrap();
        assert_eq!(a.location(), &TursoLocation::Memory);
        assert_eq!(a.path(), None);
        assert_eq!((a.node_count().await, b.node_count().await), (2, 0));
    });
}

#[test]
fn file_store_persists_across_reopen() {
    block_on(async {
        let db = TempDb::new("persists");
        let q = "MATCH (d:Requirement)-[:REFINES]->{1,}(:Requirement {id: 'REQ-001'}) \
                 RETURN DISTINCT d.id, d.status ORDER BY d.id";
        let before = {
            let mut g = TursoStore::open(&db.conn()).await.unwrap();
            assert_eq!(g.path(), Some(db.0.as_path()));
            let index = g
                .transaction(async |g| load_spec(TODO, g).await)
                .await
                .unwrap()
                .unwrap();
            apply_overlay(r#"{"REQ-002": {"status": "satisfied"}}"#, &index, &mut g)
                .await
                .unwrap();
            rows(&g, q).await
        };
        assert_eq!(before[0], ["'REQ-002'", "'satisfied'"]);

        for conn in [db.conn(), format!("file:{}", db.conn())] {
            let mut g = TursoStore::open(&conn).await.unwrap();
            assert_eq!(rows(&g, q).await, before, "{conn}");
            assert_eq!(rows(&g, "MATCH (n:!Extra) RETURN n").await.len(), 18);
            // Writes after reopening extend the same graph.
            let n = g.add_node(vec!["Extra".into()], BTreeMap::new()).await;
            g.set_node_property(n, "seen", Value::Bool(true)).await;
        }
        let g = TursoStore::open(&db.conn()).await.unwrap();
        assert_eq!(
            rows(&g, "MATCH (n:Extra) RETURN n.seen").await,
            [["true"], ["true"]]
        );
    });
}

#[test]
fn property_values_round_trip_exactly() {
    let db = TempDb::new("values");
    let mut map = BTreeMap::new();
    map.insert(
        "nested".to_string(),
        Value::List(vec![Value::Null, Value::Float(1.5)]),
    );
    map.insert(String::new(), Value::String("é\u{0}'\"".into()));
    let values = [
        Value::Bool(true),
        Value::Bool(false),
        Value::Int(0),
        Value::Int(i64::MIN),
        Value::Int(i64::MAX),
        Value::Float(0.1),
        Value::Float(-0.0),
        Value::Float(1.0),
        Value::Float(f64::NAN),
        Value::Float(f64::INFINITY),
        Value::Float(f64::MIN_POSITIVE),
        Value::String(String::new()),
        Value::String("multi\nline ünïcødé 🦀".into()),
        Value::List(vec![]),
        Value::List(vec![
            Value::Int(1),
            Value::String("a".into()),
            Value::Float(f64::NAN),
        ]),
        Value::Map(BTreeMap::new()),
        Value::Map(map),
    ];
    let props: BTreeMap<String, Value> = values
        .iter()
        .enumerate()
        .map(|(i, v)| (format!("p{i}"), v.clone()))
        .chain([("absent".to_string(), Value::Null)])
        .collect();
    // `{:?}` distinguishes -0.0 from 0.0 and compares NaN to itself.
    let debug = |m: &BTreeMap<String, Value>| format!("{m:?}");
    let mut expected = props.clone();
    expected.remove("absent");
    block_on(async {
        let mut g = TursoStore::open(&db.conn()).await.unwrap();
        let a = g.add_node(vec!["A".into()], props.clone()).await;
        let b = g.add_node(vec![], BTreeMap::new()).await;
        let e = g.add_edge(a, b, vec!["E".into()], props.clone()).await;
        for (k, v) in &props {
            g.set_node_property(b, k, v.clone()).await;
        }
        g.set_node_property(b, "p0", Value::Null).await;
        g.set_node_property(b, "p0", Value::Null).await;
        g.set_node_property(b, "p1", Value::Int(7)).await;
        assert_eq!(g.node_property(b, "p0").await, None);
        assert_eq!(g.node_property(b, "p1").await, Some(Value::Int(7)));
        assert_eq!(debug(&g.edge_properties(e).await), debug(&expected));
    });
    block_on(async {
        let g = TursoStore::open(&db.conn()).await.unwrap();
        let nodes = g.nodes().await;
        assert_eq!(debug(&g.node_properties(nodes[0]).await), debug(&expected));
        for (k, v) in &expected {
            assert_eq!(
                format!("{:?}", g.node_property(nodes[0], k).await),
                format!("{:?}", Some(v))
            );
        }
        assert_eq!(g.node_property(nodes[0], "absent").await, None);
        assert_eq!(g.node_properties(nodes[1]).await.len(), expected.len() - 1);
    });
}

#[test]
fn labels_are_sorted_and_deduplicated() {
    block_on(async {
        let mut g = TursoStore::in_memory().await.unwrap();
        let labels = vec!["B".into(), "A".into(), "B".into()];
        let n = g.add_node(labels, BTreeMap::new()).await;
        let e = g
            .add_edge(n, n, vec!["R".into(), "R".into()], BTreeMap::new())
            .await;
        assert_eq!(g.node_labels(n).await, ["A", "B"]);
        assert!(g.node_has_label(n, "A").await && !g.node_has_label(n, "C").await);
        assert_eq!(g.nodes_with_label("B").await, [n]);
        assert_eq!(g.edge_labels(e).await, ["R"]);
        assert!(g.edge_has_label(e, "R").await && !g.edge_has_label(e, "S").await);
        assert_eq!(g.edge(e).await.source, n);
    });
}

#[test]
fn transaction_commits_or_rolls_back() {
    let db = TempDb::new("transactions");
    let mut g = block_on(TursoStore::open(&db.conn())).unwrap();

    block_on(async {
        let failed = g
            .transaction(async |g| {
                load_fixture(g, "INSERT (:Kept)").await.unwrap();
                load_spec("{not json", g).await
            })
            .await;
        assert!(failed.unwrap().is_err());
        assert_eq!(g.node_count().await, 0);
    });

    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        block_on(g.transaction(async |g| -> Result<(), ()> {
            load_fixture(g, "INSERT (:Lost)").await.unwrap();
            panic!("boom")
        }))
    }));
    assert!(panicked.is_err());

    block_on(async {
        assert_eq!(g.node_count().await, 0);
        let index = g
            .transaction(async |g| load_spec(TODO, g).await)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(index.len(), 18);
    });
    drop(g);
    let g = block_on(TursoStore::open(&db.conn())).unwrap();
    assert_eq!(block_on(g.node_count()), 18);
}

#[test]
fn futures_are_send_and_the_store_is_shareable() {
    fn assert_send<T: Send>(_: &T) {}
    let mut g = block_on(TursoStore::in_memory()).unwrap();
    let tx = g.transaction(async |g| load_spec(TODO, g).await);
    assert_send(&tx);
    block_on(tx).unwrap().unwrap();

    // One store answers queries from several threads at once.
    let q = "MATCH (d:Requirement)-[:REFINES]->{1,}(:Requirement {id: 'REQ-001'}) \
             RETURN DISTINCT d.id ORDER BY d.id";
    let want = block_on(rows(&g, q));
    std::thread::scope(|s| {
        let handles: Vec<_> = (0..4).map(|_| s.spawn(|| block_on(rows(&g, q)))).collect();
        for h in handles {
            assert_eq!(h.join().unwrap(), want);
        }
    });
}

#[test]
fn bad_connection_strings_are_rejected() {
    block_on(async {
        for conn in ["", "libsql://my-db.turso.io", "file:graph.db?mode=ro"] {
            let err = TursoStore::open(conn).await.unwrap_err().to_string();
            assert!(err.contains("invalid Turso connection string"), "{err}");
        }
        let missing_dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("no/such/dir/graph.db");
        assert!(
            TursoStore::open(missing_dir.to_str().unwrap())
                .await
                .is_err()
        );
    });
}
