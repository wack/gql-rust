//! Loading the canon `todo-api` example (from the SLCR Schemas page) and
//! querying it.

use pollster::block_on;
use slcr_gql::{Params, PetgraphStore, SpecError, Value, apply_overlay, load_spec, query};

const TODO: &str = include_str!("data/todo-api.spec.json");

fn load() -> (PetgraphStore, slcr_gql::SpecIndex) {
    let mut g = PetgraphStore::new();
    let idx = block_on(load_spec(TODO, &mut g)).expect("canon example loads");
    (g, idx)
}

fn col(g: &PetgraphStore, q: &str) -> Vec<String> {
    block_on(query(g, q, &Params::new()))
        .unwrap_or_else(|e| panic!("{q}: {e}"))
        .rows
        .into_iter()
        .map(|r| match &r[0] {
            Value::String(s) => s.clone(),
            v => v.to_string(),
        })
        .collect()
}

#[test]
fn node_and_edge_inventory() {
    let (g, idx) = load();
    // 6 sections (incl. glossary), 10 requirements, 1 criterion, 1 term.
    assert_eq!(idx.len(), 18);
    assert_eq!(
        col(
            &g,
            "MATCH (n) RETURN labels(n)[0] AS kind, count(*) AS n ORDER BY kind"
        )
        .len(),
        4
    );
    assert_eq!(col(&g, "MATCH ()-[:REFINES]->() RETURN count(*)"), ["7"]);
    assert_eq!(col(&g, "MATCH ()-[:DEPENDS_ON]->() RETURN count(*)"), ["2"]);
    assert_eq!(col(&g, "MATCH ()-[:USES_TERM]->() RETURN count(*)"), ["1"]);
}

#[test]
fn document_order_via_position() {
    let (g, _) = load();
    assert_eq!(
        col(
            &g,
            "MATCH (:Section {id: 'SEC-001'})-[e:CONTAINS]->(c) RETURN c.title ORDER BY e.position"
        ),
        [
            "Views",
            "Controllers",
            "Models",
            "Authentication",
            "Glossary"
        ]
    );
}

#[test]
fn children_and_dependents() {
    let (g, _) = load();
    assert_eq!(
        col(
            &g,
            "MATCH (:Requirement {id: 'REQ-001'})-[e:CONTAINS]->(c) RETURN c.id ORDER BY e.position"
        ),
        ["REQ-002", "REQ-003"]
    );
    // REQ-004 refines both REQ-002 and REQ-003, so it is reachable by two
    // walks; DISTINCT collapses them.
    assert_eq!(
        col(
            &g,
            "MATCH (d)-[:REFINES]->{1,}(:Requirement {id: 'REQ-001'}) RETURN d.id ORDER BY d.id"
        ),
        ["REQ-002", "REQ-003", "REQ-004", "REQ-004"]
    );
    assert_eq!(
        col(
            &g,
            "MATCH (d)-[:REFINES]->{1,}(:Requirement {id: 'REQ-001'}) RETURN DISTINCT d.id ORDER BY d.id"
        ),
        ["REQ-002", "REQ-003", "REQ-004"]
    );
}

#[test]
fn containment_is_not_refinement() {
    let (g, _) = load();
    // Nested under REQ-008 and refining it vs. nested without refining:
    // REQ-006 is nested under REQ-005 and refines it; REQ-004 is nested under
    // REQ-002 and refines REQ-002 and REQ-003.
    assert_eq!(
        col(
            &g,
            "MATCH (p:Requirement)-[:CONTAINS]->(c:Requirement)
             WHERE NOT c.id IN ['REQ-002', 'REQ-003', 'REQ-004', 'REQ-006', 'REQ-009', 'REQ-010']
             RETURN c.id"
        ),
        Vec::<String>::new()
    );
    assert_eq!(
        col(
            &g,
            "MATCH (r:Requirement {id: 'REQ-004'})-[:REFINES]->(t) RETURN t.id ORDER BY t.id"
        ),
        ["REQ-002", "REQ-003"]
    );
}

#[test]
fn refining_modalities_per_parent() {
    let (g, _) = load();
    let r = block_on(query(
        &g,
        "MATCH (p:Requirement)<-[:REFINES]-(c:Requirement)
         RETURN p.id AS parent, p.refinement AS how, collect(DISTINCT c.modality) AS modalities
         ORDER BY parent",
        &Params::new(),
    ))
    .unwrap();
    let rows: Vec<String> = r
        .rows
        .iter()
        .map(|row| {
            row.iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();
    assert_eq!(
        rows,
        [
            "'REQ-001' 'AND' ['MUST']",
            "'REQ-002' 'AND' ['MUST']",
            "'REQ-003' 'AND' ['MUST']",
            "'REQ-005' 'AND' ['MUST']",
            "'REQ-008' 'OR' ['MAY']",
        ]
    );
}

#[test]
fn overlay_status() {
    let (mut g, idx) = load();
    block_on(apply_overlay(
        r#"{"REQ-007": {"status": "satisfied"}}"#,
        &idx,
        &mut g,
    ))
    .unwrap();
    assert_eq!(
        col(
            &g,
            "MATCH (r:Requirement) WHERE r.status = 'satisfied' RETURN r.id"
        ),
        ["REQ-007"]
    );
    assert!(
        block_on(apply_overlay(
            r#"{"REQ-999": {"status": "x"}}"#,
            &idx,
            &mut g
        ))
        .is_err()
    );
}

#[test]
fn rejects_dangling_and_duplicate_ids() {
    let mut g = PetgraphStore::new();
    let dangling = TODO.replace(r#""refines": ["REQ-005"]"#, r#""refines": ["REQ-404"]"#);
    assert!(matches!(
        block_on(load_spec(&dangling, &mut g)),
        Err(SpecError::DanglingReference { .. })
    ));
    let wrong_kind = TODO.replace(r#""refines": ["REQ-005"]"#, r#""refines": ["SEC-002"]"#);
    assert!(matches!(
        block_on(load_spec(&wrong_kind, &mut PetgraphStore::new())),
        Err(SpecError::DanglingReference { .. })
    ));
    let dup = TODO.replace(r#""id": "REQ-010""#, r#""id": "REQ-009""#);
    assert!(matches!(
        block_on(load_spec(&dup, &mut PetgraphStore::new())),
        Err(SpecError::DuplicateId(_))
    ));
    let unknown = TODO.replace(
        r#""modality": "MAY", "refines": ["REQ-008"]"#,
        r#""modality": "MAY", "status": "x", "refines": ["REQ-008"]"#,
    );
    assert!(matches!(
        block_on(load_spec(&unknown, &mut PetgraphStore::new())),
        Err(SpecError::Json(_))
    ));
}
