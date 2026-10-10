use crate::store::StoreError;
use std::ops::Range;

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum Error {
    /// The text is not a well-formed query in the profile grammar.
    #[error("syntax error at {span:?}: {message}")]
    Syntax { message: String, span: Range<usize> },
    /// The query uses a GQL/Cypher feature that is deliberately outside the
    /// SLCR profile. Never silently approximated.
    #[error("unsupported feature `{feature}` (outside the SLCR query profile)")]
    Unsupported { feature: String, span: Range<usize> },
    /// Well-formed but invalid, e.g. an undefined variable.
    #[error("semantic error: {0}")]
    Semantic(String),
    /// Runtime data exception, e.g. a type mismatch.
    #[error("data exception: {0}")]
    Data(String),
    /// The graph store failed, e.g. a remote backend could not be reached.
    #[error("store: {0}")]
    Store(#[from] StoreError),
}

impl Error {
    pub fn is_compile_time(&self) -> bool {
        matches!(self, Error::Syntax { .. } | Error::Semantic(_))
    }
}

const UNSUPPORTED: &[&str] = &[
    "OPTIONAL",
    "WITH",
    "MERGE",
    "CREATE",
    "INSERT",
    "DELETE",
    "DETACH",
    "SET",
    "REMOVE",
    "CALL",
    "UNION",
    "YIELD",
    "CASE",
    "EXISTS",
    "LET",
    "NEXT",
    "FOREACH",
    "USE",
    "ALL",
    "ANY",
    "NONE",
    "SINGLE",
    "REDUCE",
    "SHORTEST",
    "TRAIL",
    "ACYCLIC",
    "SIMPLE",
    "WALK",
    "GROUP",
    "DIFFERENT",
    "REPEATABLE",
    "KEEP",
    "LOAD",
    "EXCEPT",
    "INTERSECT",
];

pub(crate) fn unsupported_keyword(word: &str) -> Option<&'static str> {
    UNSUPPORTED
        .iter()
        .copied()
        .find(|k| k.eq_ignore_ascii_case(word))
}
