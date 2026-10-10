//! Profile-level behaviour on an SLCR-shaped specification graph.

use pollster::block_on;
use slcr_gql::{Error, Params, PetgraphStore, Value, load_fixture, query};

/// Sections contain requirements (a tree); requirements refine one another (a DAG).
fn spec() -> PetgraphStore {
    let mut g = PetgraphStore::new();
    block_on(load_fixture(
        &mut g,
        "
        INSERT (models:Section {title: 'Models'}),
               (r1:Requirement {id: 'REQ-1', modality: 'MUST', status: 'implemented'}),
               (r2:Requirement {id: 'REQ-2', modality: 'MUST'}),
               (r3:Requirement {id: 'REQ-3', modality: 'MAY'}),
               (r4:Requirement {id: 'REQ-4', modality: 'MUST', status: 'implemented'}),
               (t:Term {name: 'Slice'})
        INSERT (models)-[:CONTAINS]->(r1), (models)-[:CONTAINS]->(r2),
               (r2)-[:CONTAINS]->(r3), (models)-[:CONTAINS]->(r4)
        INSERT (r2)-[:REFINES]->(r1), (r3)-[:REFINES]->(r2), (r4)-[:REFINES]->(r1)
        ",
    ))
    .unwrap();
    g
}

fn strings(g: &PetgraphStore, q: &str) -> Vec<Vec<String>> {
    block_on(query(g, q, &Params::new()))
        .unwrap_or_else(|e| panic!("{q}: {e}"))
        .rows
        .iter()
        .map(|r| r.iter().map(Value::to_string).collect())
        .collect()
}

#[test]
fn children_of_a_requirement() {
    let g = spec();
    assert_eq!(
        strings(
            &g,
            "MATCH (:Requirement {id: 'REQ-2'})-[:CONTAINS]->(c) RETURN c.id"
        ),
        vec![vec!["'REQ-3'"]]
    );
}

#[test]
fn transitive_dependents_gql_and_cypher_spellings_agree() {
    let g = spec();
    let gql = "MATCH (d:Requirement)-[:REFINES]->{1,}(:Requirement {id: 'REQ-1'}) RETURN d.id AS id ORDER BY id";
    let cypher = "MATCH (d:Requirement)-[:REFINES*1..]->(:Requirement {id: 'REQ-1'}) RETURN d.id AS id ORDER BY id";
    let expected = vec![vec!["'REQ-2'"], vec!["'REQ-3'"], vec!["'REQ-4'"]];
    assert_eq!(strings(&g, gql), expected);
    assert_eq!(strings(&g, cypher), expected);
}

#[test]
fn implementation_status_rollup() {
    let g = spec();
    assert_eq!(
        strings(
            &g,
            "MATCH (r:Requirement) WHERE r.modality = 'MUST'
             RETURN r.status = 'implemented' AS done, count(*) AS n, collect_list(r.id) AS ids
             ORDER BY done DESC"
        ),
        // Null sorts as the greatest value, so it comes first under DESC.
        vec![
            vec!["null", "1", "['REQ-2']"],
            vec!["true", "2", "['REQ-1', 'REQ-4']"]
        ]
    );
}

#[test]
fn label_expressions() {
    let g = spec();
    assert_eq!(
        strings(&g, "MATCH (n IS Requirement | Term) RETURN count(*)"),
        vec![vec!["5"]]
    );
    assert_eq!(
        strings(&g, "MATCH (n:!Requirement) RETURN count(*)"),
        vec![vec!["2"]]
    );
}

#[test]
fn parameters_offset_limit() {
    let g = spec();
    let mut p = Params::new();
    p.insert("m".into(), Value::String("MUST".into()));
    let r = block_on(query(
        &g,
        "MATCH (r:Requirement) WHERE r.modality = $m RETURN r.id ORDER BY r.id OFFSET 1 LIMIT 1",
        &p,
    ))
    .unwrap();
    assert_eq!(r.rows, vec![vec![Value::String("REQ-2".into())]]);
}

#[test]
fn unbounded_quantifier_terminates_on_cycles() {
    let mut g = PetgraphStore::new();
    block_on(load_fixture(
        &mut g,
        "INSERT (a {n: 1})-[:R]->(b {n: 2})-[:R]->(c {n: 3})-[:R]->(a)",
    ))
    .unwrap();
    // Edges are bound at most once per MATCH, so every walk is finite.
    let rows = strings(&g, "MATCH ({n: 1})-[:R]->{1,}(x) RETURN x.n ORDER BY x.n");
    assert_eq!(rows, vec![vec!["1"], vec!["2"], vec!["3"]]);
}

#[test]
fn unordered_results_are_deterministic() {
    let g = spec();
    let q = "MATCH (s)-[:CONTAINS]->(c) RETURN s, c";
    let first = block_on(query(&g, q, &Params::new())).unwrap();
    for _ in 0..10 {
        assert_eq!(block_on(query(&g, q, &Params::new())).unwrap(), first);
    }
}

#[test]
fn out_of_profile_is_rejected_loudly() {
    let g = spec();
    for (q, feature) in [
        ("MATCH (n) WITH n RETURN n", "WITH"),
        ("OPTIONAL MATCH (n) RETURN n", "OPTIONAL"),
        ("MATCH p = (a)-->(b) RETURN p", "path variable"),
        ("MATCH TRAIL (a)-->(b) RETURN a", "TRAIL"),
        ("RETURN toString(1)", "function toString()"),
        ("MATCH (n) SET n.x = 1 RETURN n", "SET"),
    ] {
        match block_on(query(&g, q, &Params::new())) {
            Err(Error::Unsupported { feature: f, .. }) => assert_eq!(f, feature, "{q}"),
            other => panic!("{q}: expected Unsupported({feature}), got {other:?}"),
        }
    }
}

#[test]
fn semantic_errors() {
    let g = spec();
    for q in [
        "MATCH (n) RETURN m",
        "MATCH (n)-[r]->(m), (m)-[r]->(o) RETURN n",
        "MATCH (n) WHERE count(*) > 1 RETURN n",
        "MATCH (n) RETURN n.id, n.id",
        "MATCH (n) RETURN n.id + count(*)",
        "MATCH (n) RETURN n.id AS a ORDER BY $missing",
    ] {
        assert!(
            matches!(
                block_on(query(&g, q, &Params::new())),
                Err(Error::Semantic(_))
            ),
            "{q} should be a semantic error"
        );
    }
}

#[test]
fn gql_concatenation_not_plus() {
    let g = spec();
    assert_eq!(
        strings(&g, "RETURN 'a' || 'b', [1] || [2]"),
        vec![vec!["'ab'", "[1, 2]"]]
    );
    assert!(matches!(
        block_on(query(&g, "RETURN 'a' + 'b'", &Params::new())),
        Err(Error::Data(_))
    ));
}

#[test]
fn query_futures_are_send() {
    fn assert_send<T: Send>(_: &T) {}
    let mut g = spec();
    let p = Params::new();
    assert_send(&load_fixture(&mut g, "INSERT (:X)"));
    assert_send(&query(&g, "MATCH (n)-->{1,}(m) RETURN count(*)", &p));
}
