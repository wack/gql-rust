//! A read-only GQL query profile over the SLCR specification graph.
//!
//! ```
//! use slcr_gql::{PetgraphStore, load_fixture, query, Params};
//!
//! let mut g = PetgraphStore::new();
//! load_fixture(&mut g, "CREATE (:Requirement {id: 'REQ-1'})-[:REFINES]->(:Requirement {id: 'REQ-0'})").unwrap();
//! let r = query(&g, "MATCH (a)-[:REFINES]->(b {id: 'REQ-0'}) RETURN a.id AS id", &Params::new()).unwrap();
//! assert_eq!(r.rows[0][0].to_string(), "'REQ-1'");
//! ```

pub mod ast;
pub mod error;
pub mod exec;
pub mod fixture;
pub mod functions;
pub mod lexer;
pub mod parser;
pub mod spec;
pub mod store;
pub mod value;

pub use error::Error;
pub use exec::{Params, QueryResult, execute};
pub use fixture::load_fixture;
pub use parser::{parse_fixture, parse_query};
pub use spec::{SpecError, SpecIndex, apply_overlay, load_spec};
pub use store::{GraphStore, GraphStoreMut, PetgraphStore};
pub use value::Value;

/// Parse, check and evaluate `text` against `store`.
pub fn query(store: &dyn GraphStore, text: &str, params: &Params) -> Result<QueryResult, Error> {
    execute(store, &parse_query(text)?, params)
}
