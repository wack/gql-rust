//! The Turso backend: it must answer every query exactly as the reference
//! `PetgraphStore` does, and persist the graph when given a file.
//!
//! The TCK also runs against it: TCK_STORE=turso cargo test --features turso --test tck

#![cfg(feature = "turso")]

use slcr_gql::{
    GraphStore, GraphStoreMut, Params, PetgraphStore, TursoLocation, TursoStore, Value,
    apply_overlay, load_fixture, load_spec, query,
};
use std::collections::BTreeMap;
use std::path::PathBuf;

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

fn render(g: &dyn GraphStore, v: &Value) -> String {
    match v {
        Value::Node(n) => format!(
            "(:{} {})",
            g.node_labels(*n).join(":"),
            render(g, &Value::Map(g.node_properties(*n)))
        ),
        Value::Edge(e) => format!(
            "[:{} {}]",
            g.edge_labels(*e).join(":"),
            render(g, &Value::Map(g.edge_properties(*e)))
        ),
        Value::List(xs) => {
            let xs: Vec<String> = xs.iter().map(|x| render(g, x)).collect();
            format!("[{}]", xs.join(", "))
        }
        Value::Map(m) => {
            let kv: Vec<String> = m
                .iter()
                .map(|(k, x)| format!("{k}: {}", render(g, x)))
                .collect();
            format!("{{{}}}", kv.join(", "))
        }
        other => other.to_string(),
    }
}

fn rows(g: &dyn GraphStore, q: &str) -> Vec<Vec<String>> {
    query(g, q, &Params::new())
        .unwrap_or_else(|e| panic!("{q}: {e}"))
        .rows
        .iter()
        .map(|r| r.iter().map(|v| render(g, v)).collect())
        .collect()
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
    let mut reference = PetgraphStore::new();
    let ref_index = load_spec(TODO, &mut reference).unwrap();
    let mut g = TursoStore::open(":memory:").unwrap();
    let index = g.transaction(|g| load_spec(TODO, g)).unwrap().unwrap();

    assert_eq!(index.len(), ref_index.len());
    assert_eq!(g.node_count(), reference.node_count());
    assert_eq!(g.edge_count(), reference.edge_count());
    for q in CORPUS {
        let want = rows(&reference, q);
        assert!(!want.is_empty() || q.contains("Nonexistent"), "{q}");
        assert_eq!(rows(&g, q), want, "{q}");
    }
}

#[test]
fn errors_match_reference_backend() {
    let mut reference = PetgraphStore::new();
    let mut g = TursoStore::in_memory().unwrap();
    let fixture = "INSERT (a {n: 1})-[:R]->(b {n: 0})";
    load_fixture(&mut reference, fixture).unwrap();
    load_fixture(&mut g, fixture).unwrap();
    for q in [
        "MATCH (a)-[:R]->(b) RETURN a.n / b.n",
        "MATCH (n) RETURN n.n + 'x'",
        "MATCH (n) WITH n RETURN n",
    ] {
        let want = query(&reference, q, &Params::new()).map_err(|e| e.to_string());
        let got = query(&g, q, &Params::new()).map_err(|e| e.to_string());
        assert!(want.is_err(), "{q}");
        assert_eq!(got.err(), want.err(), "{q}");
    }
}

#[test]
fn in_memory_stores_are_independent() {
    let mut a = TursoStore::open(":memory:").unwrap();
    let b = TursoStore::open("file::memory:").unwrap();
    load_fixture(&mut a, "INSERT (:A), (:A)").unwrap();
    assert_eq!(a.location(), &TursoLocation::Memory);
    assert_eq!(a.path(), None);
    assert_eq!((a.node_count(), b.node_count()), (2, 0));
}

#[test]
fn file_store_persists_across_reopen() {
    let db = TempDb::new("persists");
    let q = "MATCH (d:Requirement)-[:REFINES]->{1,}(:Requirement {id: 'REQ-001'}) \
             RETURN DISTINCT d.id, d.status ORDER BY d.id";
    let before = {
        let mut g = TursoStore::open(&db.conn()).unwrap();
        assert_eq!(g.path(), Some(db.0.as_path()));
        let index = g.transaction(|g| load_spec(TODO, g)).unwrap().unwrap();
        apply_overlay(r#"{"REQ-002": {"status": "satisfied"}}"#, &index, &mut g).unwrap();
        rows(&g, q)
    };
    assert_eq!(before[0], ["'REQ-002'", "'satisfied'"]);

    for conn in [db.conn(), format!("file:{}", db.conn())] {
        let mut g = TursoStore::open(&conn).unwrap();
        assert_eq!(rows(&g, q), before, "{conn}");
        assert_eq!(rows(&g, "MATCH (n:!Extra) RETURN n").len(), 18);
        // Writes after reopening extend the same graph.
        let n = g.add_node(vec!["Extra".into()], BTreeMap::new());
        g.set_node_property(n, "seen", Value::Bool(true));
    }
    let g = TursoStore::open(&db.conn()).unwrap();
    assert_eq!(
        rows(&g, "MATCH (n:Extra) RETURN n.seen"),
        [["true"], ["true"]]
    );
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
    {
        let mut g = TursoStore::open(&db.conn()).unwrap();
        let a = g.add_node(vec!["A".into()], props.clone());
        let b = g.add_node(vec![], BTreeMap::new());
        let e = g.add_edge(a, b, vec!["E".into()], props.clone());
        for (k, v) in &props {
            g.set_node_property(b, k, v.clone());
        }
        g.set_node_property(b, "p0", Value::Null);
        g.set_node_property(b, "p0", Value::Null);
        g.set_node_property(b, "p1", Value::Int(7));
        assert_eq!(g.node_property(b, "p0"), None);
        assert_eq!(g.node_property(b, "p1"), Some(Value::Int(7)));
        assert_eq!(debug(&g.edge_properties(e)), debug(&expected));
    }
    let g = TursoStore::open(&db.conn()).unwrap();
    let nodes: Vec<_> = g.nodes().collect();
    assert_eq!(debug(&g.node_properties(nodes[0])), debug(&expected));
    for (k, v) in &expected {
        assert_eq!(
            format!("{:?}", g.node_property(nodes[0], k)),
            format!("{:?}", Some(v))
        );
    }
    assert_eq!(g.node_property(nodes[0], "absent"), None);
    assert_eq!(g.node_properties(nodes[1]).len(), expected.len() - 1);
}

#[test]
fn labels_are_sorted_and_deduplicated() {
    let mut g = TursoStore::in_memory().unwrap();
    let n = g.add_node(vec!["B".into(), "A".into(), "B".into()], BTreeMap::new());
    let e = g.add_edge(n, n, vec!["R".into(), "R".into()], BTreeMap::new());
    assert_eq!(g.node_labels(n), ["A", "B"]);
    assert!(g.node_has_label(n, "A") && !g.node_has_label(n, "C"));
    assert_eq!(g.nodes_with_label("B").collect::<Vec<_>>(), [n]);
    assert_eq!(g.edge_labels(e), ["R"]);
    assert!(g.edge_has_label(e, "R") && !g.edge_has_label(e, "S"));
    assert_eq!(g.edge(e).source, n);
}

#[test]
fn transaction_commits_or_rolls_back() {
    let db = TempDb::new("transactions");
    let mut g = TursoStore::open(&db.conn()).unwrap();

    let failed = g.transaction(|g| {
        load_fixture(g, "INSERT (:Kept)").unwrap();
        load_spec("{not json", g)
    });
    assert!(failed.unwrap().is_err());
    assert_eq!(g.node_count(), 0);

    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        g.transaction(|g| -> Result<(), ()> {
            load_fixture(g, "INSERT (:Lost)").unwrap();
            panic!("boom")
        })
    }));
    assert!(panicked.is_err());
    assert_eq!(g.node_count(), 0);

    let index = g.transaction(|g| load_spec(TODO, g)).unwrap().unwrap();
    assert_eq!(index.len(), 18);
    drop(g);
    assert_eq!(TursoStore::open(&db.conn()).unwrap().node_count(), 18);
}

#[test]
fn bad_connection_strings_are_rejected() {
    for conn in ["", "libsql://my-db.turso.io", "file:graph.db?mode=ro"] {
        let err = TursoStore::open(conn).unwrap_err().to_string();
        assert!(err.contains("invalid Turso connection string"), "{err}");
    }
    let missing_dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("no/such/dir/graph.db");
    assert!(TursoStore::open(missing_dir.to_str().unwrap()).is_err());
}
