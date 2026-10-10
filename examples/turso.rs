//! Query the canon spec from a Turso store chosen by connection string.
//!
//! Run with: cargo run --features turso --example turso -- [CONNECTION] [QUERY]
//!
//! CONNECTION defaults to `:memory:`. With a file such as `graph.db`, the
//! first run loads the spec and later runs query the persisted graph.
use slcr_gql::{Params, TursoStore, load_spec, query};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

const TODO: &str = include_str!("../tests/data/todo-api.spec.json");

fn main() -> Result<()> {
    pollster::block_on(run())
}

async fn run() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let conn = args.next().unwrap_or_else(|| ":memory:".into());
    let q = args.next().unwrap_or_else(|| {
        "MATCH (d:Requirement)-[:REFINES]->{1,}(:Requirement {id: 'REQ-001'}) RETURN DISTINCT d.id AS dependent ORDER BY dependent".into()
    });

    let mut g = TursoStore::open(&conn).await?;
    if g.node_count().await? == 0 {
        g.transaction(async |g| load_spec(TODO, g).await).await?;
        eprintln!("{conn}: loaded {} nodes", g.node_count().await?);
    } else {
        eprintln!("{conn}: reusing {} stored nodes", g.node_count().await?);
    }

    let r = query(&g, &q, &Params::new()).await?;
    println!("{}", r.columns.join("\t"));
    for row in r.rows {
        let cells: Vec<String> = row.iter().map(|v| v.to_string()).collect();
        println!("{}", cells.join("\t"));
    }
    Ok(())
}
