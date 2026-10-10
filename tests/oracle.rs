//! Differential testing against Grafeo (pure-Rust, Apache-2.0), an
//! independent GQL implementation. Not a CI gate on its own: Grafeo is
//! pre-1.0, so disagreements are triaged into `KNOWN_DIVERGENCES` with a
//! note on which side is wrong.
//!
//! Run with: cargo test --features grafeo --test oracle -- --nocapture

#![cfg(feature = "grafeo")]

use grafeo::GrafeoDB;
use pollster::block_on;
use slcr_gql::{GraphStore, Params, PetgraphStore, Value, load_spec, query};

const TODO: &str = include_str!("data/todo-api.spec.json");

/// Queries over the canon spec whose results are scalars, so both engines'
/// outputs can be compared directly. ORDER BY makes row order comparable;
/// queries without it are compared as multisets.
const CORPUS: &[&str] = &[
    "MATCH (r:Requirement) RETURN r.id ORDER BY r.id",
    "MATCH (r:Requirement) RETURN count(*)",
    "MATCH (s:Section) RETURN s.title ORDER BY s.title",
    "MATCH (r:Requirement {modality: 'MAY'}) RETURN r.id ORDER BY r.id",
    "MATCH (:Requirement {id: 'REQ-001'})-[e:CONTAINS]->(c) RETURN c.id ORDER BY e.position",
    "MATCH (d:Requirement)-[:REFINES]->(p:Requirement) RETURN d.id, p.id ORDER BY d.id, p.id",
    "MATCH (d:Requirement)-[:REFINES]->{1,}(:Requirement {id: 'REQ-001'}) RETURN d.id ORDER BY d.id",
    "MATCH (d:Requirement)-[:REFINES]->{1,}(:Requirement {id: 'REQ-001'}) RETURN DISTINCT d.id ORDER BY d.id",
    "MATCH (d:Requirement)-[:REFINES]->{2,2}(t) RETURN d.id, t.id ORDER BY d.id",
    "MATCH (r:Requirement)<-[:REFINES]-(c:Requirement) RETURN r.id, count(c) AS n ORDER BY r.id",
    "MATCH (s:Section)-[:CONTAINS]->{1,}(r:Requirement) RETURN s.id, count(r) AS n ORDER BY s.id",
    "MATCH (r:Requirement)-[:DEPENDS_ON]->(t) RETURN r.id, t.id ORDER BY t.id",
    "MATCH (r)-[:USES_TERM]->(t:Term) RETURN r.id, t.title",
    "MATCH (r:Requirement) WHERE r.refinement IS NULL RETURN r.id ORDER BY r.id",
    "MATCH (r:Requirement) WHERE r.title STARTS WITH 'Accepts' RETURN r.id ORDER BY r.id",
    "MATCH (r:Requirement) RETURN r.modality, count(*) AS n ORDER BY r.modality",
    "MATCH (r:Requirement) RETURN r.id ORDER BY r.id DESC LIMIT 3",
    "MATCH (r:Requirement) RETURN r.id ORDER BY r.id OFFSET 8",
    "MATCH (a:Requirement)-[:REFINES]->(p)<-[:REFINES]-(b:Requirement) WHERE a.id < b.id RETURN a.id, b.id, p.id ORDER BY p.id, a.id",
    "MATCH (r:Requirement) WHERE r.id IN ['REQ-001', 'REQ-404'] RETURN r.title",
    "MATCH (n) WHERE n IS LABELED Requirement | Term RETURN count(*)",
    "MATCH (n:Section|Term) RETURN count(*)",
    "MATCH (r:Requirement) RETURN collect_list(r.id) AS ids",
    "RETURN 7 / 2 AS q",
    "RETURN 7 % 3 AS m",
    "RETURN mod(7, 3) AS m",
    "RETURN 2 ^ 3 AS p",
    "RETURN power(2, 3) AS p",
    "RETURN 'a' || 'b' AS s",
    "MATCH (r:Requirement) RETURN collect(r.id) AS ids",
    "RETURN null = null AS a, null IS NULL AS b, 1 < 'a' AS c",
];

/// Triaged disagreements: (query, note). Each note says which side we
/// believe is wrong and why. Observed against grafeo 0.5.42.
const KNOWN_DIVERGENCES: &[(&str, &str)] = &[
    (
        "MATCH (n) WHERE n IS LABELED Requirement | Term RETURN count(*)",
        "grafeo: rejects label disjunction in a labeled predicate; GQL label expressions allow `|`",
    ),
    (
        "MATCH (n:Section|Term) RETURN count(*)",
        "grafeo: rejects label disjunction in an element pattern",
    ),
    (
        "MATCH (r:Requirement) RETURN collect_list(r.id) AS ids",
        "grafeo: treats GQL's COLLECT_LIST aggregate as an unknown scalar, returning a null per row (silent)",
    ),
    (
        "RETURN mod(7, 3) AS m",
        "grafeo: MOD() returns null (silent)",
    ),
    (
        "RETURN 2 ^ 3 AS p",
        "grafeo: returns 2 (silent). `^` is a Cypher operator, accepted by the profile as an alias of GQL POWER()",
    ),
];

fn gql_literal(v: &Value) -> String {
    match v {
        Value::String(s) => format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'")),
        Value::List(xs) => format!(
            "[{}]",
            xs.iter().map(gql_literal).collect::<Vec<_>>().join(", ")
        ),
        other => other.to_string(),
    }
}

fn props_literal(props: &std::collections::BTreeMap<String, Value>, extra: Option<u64>) -> String {
    let mut parts: Vec<String> = props
        .iter()
        .map(|(k, v)| format!("`{k}`: {}", gql_literal(v)))
        .collect();
    if let Some(n) = extra {
        parts.push(format!("`_oracle`: {n}"));
    }
    format!("{{{}}}", parts.join(", "))
}

/// Copy a store into Grafeo with GQL INSERT statements.
fn mirror(g: &PetgraphStore) -> GrafeoDB {
    let db = GrafeoDB::new_in_memory();
    let session = db.session();
    for n in block_on(g.nodes()).unwrap() {
        let labels: String = block_on(g.node_labels(n))
            .unwrap()
            .iter()
            .map(|l| format!(":{l}"))
            .collect();
        let stmt = format!(
            "INSERT ({labels} {})",
            props_literal(&block_on(g.node_properties(n)).unwrap(), Some(n.0))
        );
        session
            .execute(&stmt)
            .unwrap_or_else(|e| panic!("{stmt}: {e}"));
    }
    for n in block_on(g.nodes()).unwrap() {
        for e in block_on(g.edges(n, slcr_gql::store::Incidence::Outgoing)).unwrap() {
            let label = block_on(g.edge_labels(e.id)).unwrap().join("");
            let stmt = format!(
                "MATCH (a {{`_oracle`: {}}}), (b {{`_oracle`: {}}}) INSERT (a)-[:{label} {}]->(b)",
                e.source.0,
                e.target.0,
                props_literal(&block_on(g.edge_properties(e.id)).unwrap(), None)
            );
            session
                .execute(&stmt)
                .unwrap_or_else(|e| panic!("{stmt}: {e}"));
        }
    }
    db
}

fn ours(g: &PetgraphStore, q: &str) -> Result<Vec<Vec<String>>, String> {
    block_on(query(g, q, &Params::new()))
        .map(|r| {
            r.rows
                .iter()
                .map(|row| row.iter().map(Value::to_string).collect())
                .collect()
        })
        .map_err(|e| e.to_string())
}

fn canon(v: &grafeo::Value) -> String {
    use grafeo::Value as G;
    match v {
        G::Null => "null".into(),
        G::Bool(b) => b.to_string(),
        G::Int64(i) => i.to_string(),
        G::Float64(x) => Value::Float(*x).to_string(),
        G::String(s) => Value::String(s.to_string()).to_string(),
        G::List(xs) => format!("[{}]", xs.iter().map(canon).collect::<Vec<_>>().join(", ")),
        other => format!("{other:?}"),
    }
}

fn theirs(db: &GrafeoDB, q: &str) -> Result<Vec<Vec<String>>, String> {
    db.session()
        .execute(q)
        .map(|r| {
            r.rows()
                .iter()
                .map(|row| row.iter().map(canon).collect())
                .collect()
        })
        .map_err(|e| e.to_string())
}

#[test]
fn differential() {
    let mut g = PetgraphStore::new();
    block_on(load_spec(TODO, &mut g)).unwrap();
    let db = mirror(&g);

    let (mut agree, mut new_divergences) = (0, Vec::new());
    for q in CORPUS {
        let (a, b) = (ours(&g, q), theirs(&db, q));
        let same = match (&a, &b) {
            (Ok(x), Ok(y)) if q.contains("ORDER BY") => x == y,
            (Ok(x), Ok(y)) => {
                let (mut x, mut y) = (x.clone(), y.clone());
                x.sort();
                y.sort();
                x == y
            }
            (Err(_), Err(_)) => true,
            _ => false,
        };
        if same {
            agree += 1;
        } else if let Some((_, note)) = KNOWN_DIVERGENCES.iter().find(|(k, _)| k == q) {
            println!("known divergence ({note}): {q}");
        } else {
            println!("DIVERGENCE {q}\n  slcr-gql: {a:?}\n  grafeo:   {b:?}");
            new_divergences.push(*q);
        }
    }
    println!("oracle: {agree}/{} queries agree", CORPUS.len());
    assert!(
        new_divergences.is_empty(),
        "untriaged divergences: {new_divergences:#?}"
    );
}
