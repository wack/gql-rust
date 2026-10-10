//! A read-only GQL query profile over the SLCR specification graph.
//!
//! The API is async, as is the [`GraphStore`] trait behind it, and does not
//! depend on a particular runtime; this example drives it with `pollster`.
//!
//! ```
//! use slcr_gql::{PetgraphStore, load_fixture, query, Params};
//!
//! # pollster::block_on(async {
//! let mut g = PetgraphStore::new();
//! load_fixture(&mut g, "CREATE (:Requirement {id: 'REQ-1'})-[:REFINES]->(:Requirement {id: 'REQ-0'})").await.unwrap();
//! let r = query(&g, "MATCH (a)-[:REFINES]->(b {id: 'REQ-0'}) RETURN a.id AS id", &Params::new()).await.unwrap();
//! assert_eq!(r.rows[0][0].to_string(), "'REQ-1'");
//! # });
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
pub use store::{GraphStore, GraphStoreMut, PetgraphStore, async_trait};
#[cfg(feature = "turso")]
pub use store::{TursoLocation, TursoStore, TursoStoreError};
pub use value::Value;

/// Parse, check and evaluate `text` against `store`.
pub async fn query(
    store: &dyn GraphStore,
    text: &str,
    params: &Params,
) -> Result<QueryResult, Error> {
    execute(store, &parse_query(text)?, params).await
}

/// A boxed `Send` future, for the recursive async functions.
pub(crate) type BoxFuture<'a, T> =
    std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;
