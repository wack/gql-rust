//! Run with: cargo run --example spec_queries -- "MATCH (r:Requirement) RETURN r.id"
use slcr_gql::{Params, PetgraphStore, load_fixture, query};

fn main() {
    let mut g = PetgraphStore::new();
    load_fixture(
        &mut g,
        "INSERT (s:Section {title: 'Models'}),
                (r1:Requirement {id: 'REQ-1', status: 'implemented'}),
                (r2:Requirement {id: 'REQ-2'}), (r3:Requirement {id: 'REQ-3'})
         INSERT (s)-[:CONTAINS]->(r1), (s)-[:CONTAINS]->(r2), (r2)-[:CONTAINS]->(r3),
                (r2)-[:REFINES]->(r1), (r3)-[:REFINES]->(r2)",
    )
    .expect("fixture");
    let q = std::env::args().nth(1).unwrap_or_else(|| {
        "MATCH (d:Requirement)-[:REFINES]->{1,}(:Requirement {id: 'REQ-1'}) RETURN d.id AS dependent ORDER BY dependent".into()
    });
    match query(&g, &q, &Params::new()) {
        Ok(r) => {
            println!("{}", r.columns.join("\t"));
            for row in r.rows {
                println!(
                    "{}",
                    row.iter()
                        .map(|v| v.to_string())
                        .collect::<Vec<_>>()
                        .join("\t")
                );
            }
        }
        Err(e) => eprintln!("error: {e}"),
    }
}
