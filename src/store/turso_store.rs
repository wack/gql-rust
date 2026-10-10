//! [Turso](https://github.com/tursodatabase/turso) backend: the graph lives in
//! SQL tables, either in memory or in a database file.
//!
//! ## Connection strings
//!
//! | Connection string            | Database                                   |
//! |------------------------------|--------------------------------------------|
//! | `:memory:`                   | in memory, discarded when the store drops  |
//! | `file::memory:`              | in memory (SQLite URI spelling)            |
//! | `path/to/graph.db`           | persistent file, created if missing        |
//! | `file:path/to/graph.db`      | persistent file (SQLite URI spelling)      |
//! | `file:///abs/path/graph.db`  | persistent file (absolute `file://` URL)   |
//!
//! Remote databases (`libsql://`, `http(s)://` and similar) and URI query
//! parameters are rejected rather than ignored.
//!
//! ## Schema
//!
//! Tables are prefixed `gql_`, so the graph can share a database with other
//! data. Element ids are rowids, so iteration follows insertion order.
//!
//! ```sql
//! gql_meta        (key TEXT PRIMARY KEY, value)                  -- schema_version
//! gql_nodes       (id INTEGER PRIMARY KEY)
//! gql_node_labels (node, label)                                  -- PRIMARY KEY (node, label)
//! gql_node_props  (node, key, kind INTEGER, value)               -- PRIMARY KEY (node, key)
//! gql_edges       (id INTEGER PRIMARY KEY, source, target)
//! gql_edge_labels (edge, label)
//! gql_edge_props  (edge, key, kind INTEGER, value)
//! ```
//!
//! Property values keep their SQL type where one exists: booleans are `0`/`1`
//! integers, integers are integers, floats are reals (NaN, which SQL cannot
//! store, is a `NULL` value of float kind), strings are text, and lists and
//! maps are blobs in a private binary encoding. `kind` disambiguates.

use super::{
    EdgeId, EdgeRef, GraphStore, GraphStoreMut, Incidence, NodeId, StoreError, StoreErrorKind,
    StoreResult, async_trait,
};
use crate::value::Value;
use async_lock::Mutex;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use turso::{Builder, Connection, Database, IntoParams, Row};

const SCHEMA_VERSION: i64 = 1;

const SCHEMA: &str = "
CREATE TABLE gql_nodes (id INTEGER PRIMARY KEY);
CREATE TABLE gql_node_labels (
    node INTEGER NOT NULL REFERENCES gql_nodes (id),
    label TEXT NOT NULL,
    PRIMARY KEY (node, label)
);
CREATE INDEX gql_node_labels_by_label ON gql_node_labels (label, node);
CREATE TABLE gql_node_props (
    node INTEGER NOT NULL REFERENCES gql_nodes (id),
    key TEXT NOT NULL,
    kind INTEGER NOT NULL,
    value,
    PRIMARY KEY (node, key)
);
CREATE TABLE gql_edges (
    id INTEGER PRIMARY KEY,
    source INTEGER NOT NULL REFERENCES gql_nodes (id),
    target INTEGER NOT NULL REFERENCES gql_nodes (id)
);
CREATE INDEX gql_edges_by_source ON gql_edges (source, id);
CREATE INDEX gql_edges_by_target ON gql_edges (target, id);
CREATE TABLE gql_edge_labels (
    edge INTEGER NOT NULL REFERENCES gql_edges (id),
    label TEXT NOT NULL,
    PRIMARY KEY (edge, label)
);
CREATE TABLE gql_edge_props (
    edge INTEGER NOT NULL REFERENCES gql_edges (id),
    key TEXT NOT NULL,
    kind INTEGER NOT NULL,
    value,
    PRIMARY KEY (edge, key)
);
";

/// `kind` column values.
const KIND_BOOL: i64 = 1;
const KIND_INT: i64 = 2;
const KIND_FLOAT: i64 = 3;
const KIND_STRING: i64 = 4;
const KIND_ENCODED: i64 = 5;

#[derive(Debug, thiserror::Error)]
pub enum TursoStoreError {
    #[error("invalid Turso connection string `{input}`: {reason}")]
    ConnectionString { input: String, reason: String },
    #[error(
        "database has graph schema version {found}; this build supports version {SCHEMA_VERSION}"
    )]
    SchemaVersion { found: String },
    #[error("turso: {0}")]
    Turso(#[from] turso::Error),
}

/// Where a [`TursoStore`] keeps its database, parsed from a connection string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TursoLocation {
    /// A private in-memory database.
    Memory,
    /// A database file, created if it does not exist.
    File(PathBuf),
}

impl FromStr for TursoLocation {
    type Err = TursoStoreError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = |reason: &str| TursoStoreError::ConnectionString {
            input: s.to_string(),
            reason: reason.to_string(),
        };
        if s.is_empty() {
            return Err(err("empty"));
        }
        if s == ":memory:" {
            return Ok(TursoLocation::Memory);
        }
        let path = if let Some(rest) = s.strip_prefix("file://") {
            if !rest.starts_with('/') {
                return Err(err(
                    "file:// URLs must be absolute, like file:///path/graph.db",
                ));
            }
            rest
        } else if let Some(rest) = s.strip_prefix("file:") {
            rest
        } else if let Some((scheme, _)) = s.split_once("://") {
            return Err(err(&format!(
                "`{scheme}://` databases are not supported; use :memory: or a file path"
            )));
        } else {
            return Ok(TursoLocation::File(PathBuf::from(s)));
        };
        if path.contains(['?', '#']) {
            return Err(err("URI query parameters are not supported"));
        }
        match path {
            "" => Err(err("missing file path")),
            ":memory:" => Ok(TursoLocation::Memory),
            p => Ok(TursoLocation::File(PathBuf::from(p))),
        }
    }
}

/// Graph backend over a [Turso](https://github.com/tursodatabase/turso)
/// database, chosen with a connection string; see the [module
/// docs](self) for the accepted forms and the table layout.
///
/// ```
/// use slcr_gql::{Params, TursoStore, load_fixture, query};
///
/// # pollster::block_on(async {
/// let mut g = TursoStore::open(":memory:").await?;
/// g.transaction(async |g| load_fixture(g, "INSERT (:Requirement {id: 'REQ-1'})").await)
///     .await?;
/// let r = query(&g, "MATCH (r:Requirement) RETURN r.id", &Params::new()).await?;
/// assert_eq!(r.rows[0][0].to_string(), "'REQ-1'");
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// # }).unwrap();
/// ```
///
/// # Errors
///
/// Database failures surface as [`StoreError`]s: a busy or locked database,
/// an interrupted statement or an I/O failure as
/// [`Unavailable`](StoreErrorKind::Unavailable), a corrupt file or
/// undecodable value as [`Corrupt`](StoreErrorKind::Corrupt), and any read or
/// mutation of an unknown node or edge id as
/// [`NotFound`](StoreErrorKind::NotFound). Each read checks the id in the
/// same statement that fetches the data, so an element that exists without
/// labels or properties is never confused with one that does not exist.
///
/// Mutations outside [`transaction`](Self::transaction) commit one element
/// at a time, which costs a disk sync per element for a file database; load
/// large graphs inside a transaction.
pub struct TursoStore {
    /// A Turso connection runs one statement at a time, but `&self` reads
    /// can run concurrently (joined futures, other threads), so every use
    /// holds this lock. Declared before `_db` so the connection closes first.
    conn: Mutex<Connection>,
    _db: Database,
    location: TursoLocation,
    /// What to roll back because a transaction or a mutation's savepoint
    /// was abandoned unfinished: its future was dropped or panicked, or its
    /// COMMIT or ROLLBACK failed. `Drop` cannot run async SQL, so the next
    /// operation does it. One of the `ABANDONED_*` levels.
    abandoned: Arc<AtomicU8>,
}

impl std::fmt::Debug for TursoStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TursoStore")
            .field("location", &self.location)
            .finish_non_exhaustive()
    }
}

impl TursoStore {
    /// Open the database named by `connection_string`, creating the graph
    /// schema if the database has none.
    pub async fn open(connection_string: &str) -> Result<Self, TursoStoreError> {
        Self::open_location(connection_string.parse()?).await
    }

    /// A new, empty in-memory store.
    pub async fn in_memory() -> Result<Self, TursoStoreError> {
        Self::open_location(TursoLocation::Memory).await
    }

    pub async fn open_location(location: TursoLocation) -> Result<Self, TursoStoreError> {
        let path = match &location {
            TursoLocation::Memory => ":memory:",
            TursoLocation::File(p) => {
                p.to_str()
                    .ok_or_else(|| TursoStoreError::ConnectionString {
                        input: p.display().to_string(),
                        reason: "path is not valid UTF-8".into(),
                    })?
            }
        };
        let db = Builder::new_local(path).build().await?;
        let conn = db.connect()?;
        let store = TursoStore {
            conn: Mutex::new(conn),
            _db: db,
            location,
            abandoned: Arc::new(AtomicU8::new(ABANDONED_NONE)),
        };
        store.init_schema().await?;
        Ok(store)
    }

    pub fn location(&self) -> &TursoLocation {
        &self.location
    }

    /// The database file, or `None` for an in-memory store.
    pub fn path(&self) -> Option<&Path> {
        match &self.location {
            TursoLocation::Memory => None,
            TursoLocation::File(p) => Some(p),
        }
    }

    pub async fn node_count(&self) -> StoreResult<usize> {
        self.count("SELECT count(*) FROM gql_nodes").await
    }

    pub async fn edge_count(&self) -> StoreResult<usize> {
        self.count("SELECT count(*) FROM gql_edges").await
    }

    /// Run `f` in one transaction: it commits if `f` returns `Ok` and rolls
    /// back if `f` returns `Err`, panics, or is dropped before finishing.
    /// Failing to begin or commit is reported as `f`'s error type, which
    /// must accept a [`StoreError`]; the crate's [`Error`](crate::Error) and
    /// [`SpecError`](crate::SpecError) both do.
    ///
    /// ```
    /// # use slcr_gql::{TursoStore, load_spec};
    /// # let src = include_str!("../../tests/data/todo-api.spec.json");
    /// # pollster::block_on(async {
    /// let mut g = TursoStore::in_memory().await?;
    /// let index = g.transaction(async |g| load_spec(src, g).await).await?;
    /// assert!(index.get("REQ-001").is_some());
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// # }).unwrap();
    /// ```
    pub async fn transaction<T, E: From<StoreError>>(
        &mut self,
        f: impl AsyncFnOnce(&mut Self) -> Result<T, E>,
    ) -> Result<T, E> {
        {
            let conn = self.conn.lock().await;
            settle(&conn, &self.abandoned).await?;
            conn.execute("BEGIN", ()).await.map_err(store_error)?;
        }
        let mut guard = Abandon::arm(&self.abandoned, ABANDONED_TRANSACTION);
        let out = f(self).await;
        let conn = self.conn.lock().await;
        // On a failed COMMIT or ROLLBACK the guard stays armed: if the
        // transaction is still open, the next operation rolls it back.
        match out {
            Ok(v) => {
                conn.execute("COMMIT", ()).await.map_err(store_error)?;
                guard.disarm();
                Ok(v)
            }
            Err(e) => {
                if conn.execute("ROLLBACK", ()).await.is_ok() {
                    guard.disarm();
                }
                Err(e)
            }
        }
    }

    async fn init_schema(&self) -> Result<(), TursoStoreError> {
        let guard = self.conn.lock().await;
        let c = &*guard;
        c.execute("PRAGMA foreign_keys = ON", ()).await?;
        c.execute(
            "CREATE TABLE IF NOT EXISTS gql_meta (key TEXT PRIMARY KEY, value)",
            (),
        )
        .await?;
        let mut rows = c
            .query(
                "SELECT value FROM gql_meta WHERE key = 'schema_version'",
                (),
            )
            .await?;
        let version = match rows.next().await? {
            Some(row) => Some(row.get_value(0)?),
            None => None,
        };
        drop(rows);
        match version {
            Some(turso::Value::Integer(SCHEMA_VERSION)) => Ok(()),
            Some(turso::Value::Integer(n)) => Err(TursoStoreError::SchemaVersion {
                found: n.to_string(),
            }),
            Some(other) => Err(TursoStoreError::SchemaVersion {
                found: format!("{other:?}"),
            }),
            None => {
                c.execute("BEGIN", ()).await?;
                let created = async {
                    c.execute_batch(SCHEMA).await?;
                    c.execute(
                        "INSERT INTO gql_meta (key, value) VALUES ('schema_version', ?1)",
                        (SCHEMA_VERSION,),
                    )
                    .await?;
                    c.execute("COMMIT", ()).await
                }
                .await;
                if let Err(e) = created {
                    let _ = c.execute("ROLLBACK", ()).await;
                    return Err(e.into());
                }
                Ok(())
            }
        }
    }

    async fn rows(&self, sql: &str, params: impl IntoParams + Send) -> StoreResult<Vec<Row>> {
        let conn = self.conn.lock().await;
        settle(&conn, &self.abandoned).await?;
        let res: turso::Result<Vec<Row>> = async {
            let mut stmt = conn.prepare_cached(sql).await?;
            let mut rows = stmt.query(params).await?;
            let mut out = Vec::new();
            while let Some(row) = rows.next().await? {
                out.push(row);
            }
            Ok(out)
        }
        .await;
        res.map_err(store_error)
    }

    async fn exec(&self, sql: &str, params: impl IntoParams + Send) -> StoreResult<()> {
        self.insert(sql, params).await.map(|_| ())
    }

    /// Execute `sql`, returning the rowid it inserted, if any.
    async fn insert(&self, sql: &str, params: impl IntoParams + Send) -> StoreResult<i64> {
        let conn = self.conn.lock().await;
        settle(&conn, &self.abandoned).await?;
        let res: turso::Result<i64> = async {
            let mut stmt = conn.prepare_cached(sql).await?;
            stmt.execute(params).await?;
            Ok(conn.last_insert_rowid())
        }
        .await;
        res.map_err(store_error)
    }

    async fn count(&self, sql: &str) -> StoreResult<usize> {
        let rows = self.rows(sql, ()).await?;
        let row = rows
            .first()
            .ok_or_else(|| corrupt("count returned no row"))?;
        Ok(int(row, 0)? as usize)
    }

    async fn ids(&self, sql: &str, params: impl IntoParams + Send) -> StoreResult<Vec<u64>> {
        self.rows(sql, params)
            .await?
            .iter()
            .map(|r| int(r, 0).map(|n| n as u64))
            .collect()
    }

    /// Rows of a query about one element: the node or edge (`owner`) with
    /// id `id`. The query is `gql_{owner}s LEFT JOIN` its data, so no rows
    /// means the element does not exist, and a row whose first column is
    /// NULL means it exists without matching data; that row is dropped.
    async fn owned(
        &self,
        owner: &str,
        id: u64,
        sql: &str,
        params: impl IntoParams + Send,
    ) -> StoreResult<Vec<Row>> {
        let rows = self.rows(sql, params).await?;
        if rows.is_empty() {
            return Err(not_found(owner, id));
        }
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            if !matches!(column(&row, 0)?, turso::Value::Null) {
                out.push(row);
            }
        }
        Ok(out)
    }

    async fn owned_strings(&self, owner: &str, id: u64, sql: &str) -> StoreResult<Vec<String>> {
        self.owned(owner, id, sql, (id as i64,))
            .await?
            .iter()
            .map(|r| text(r, 0))
            .collect()
    }

    async fn owned_flag(&self, owner: &str, id: u64, sql: &str, arg: &str) -> StoreResult<bool> {
        let rows = self.owned(owner, id, sql, (id as i64, arg)).await?;
        let row = rows
            .first()
            .ok_or_else(|| corrupt("EXISTS returned NULL"))?;
        Ok(int(row, 0)? != 0)
    }

    async fn owned_property(
        &self,
        owner: &str,
        id: u64,
        sql: &str,
        key: &str,
    ) -> StoreResult<Option<Value>> {
        self.owned(owner, id, sql, (id as i64, key))
            .await?
            .first()
            .map(|r| decode_property(r, 0))
            .transpose()
    }

    async fn owned_properties(
        &self,
        owner: &str,
        id: u64,
        sql: &str,
    ) -> StoreResult<BTreeMap<String, Value>> {
        self.owned(owner, id, sql, (id as i64,))
            .await?
            .iter()
            .map(|r| Ok((text(r, 0)?, decode_property(r, 1)?)))
            .collect()
    }

    /// Run one mutation atomically: in a transaction of its own, or, inside
    /// the caller's transaction, in a savepoint. Either commits if `f`
    /// succeeds and is undone if it fails.
    async fn atomically<T>(&self, f: impl AsyncFnOnce() -> StoreResult<T>) -> StoreResult<T> {
        let (level, begin, commit, undo): (_, _, _, &[&str]) = {
            let conn = self.conn.lock().await;
            settle(&conn, &self.abandoned).await?;
            if conn.is_autocommit().map_err(store_error)? {
                (ABANDONED_TRANSACTION, "BEGIN", "COMMIT", &["ROLLBACK"])
            } else {
                (
                    ABANDONED_SAVEPOINT,
                    "SAVEPOINT gql_mutation",
                    "RELEASE gql_mutation",
                    &["ROLLBACK TO gql_mutation", "RELEASE gql_mutation"],
                )
            }
        };
        self.exec(begin, ()).await?;
        let mut guard = Abandon::arm(&self.abandoned, level);
        let out = f().await;
        let conn = self.conn.lock().await;
        let ends: &[&str] = if out.is_ok() { &[commit] } else { undo };
        let mut ended = Ok(());
        for sql in ends {
            ended = conn
                .execute(*sql, ())
                .await
                .map(|_| ())
                .map_err(store_error);
            if ended.is_err() {
                break;
            }
        }
        // If ending fails, the armed guard undoes the mutation later.
        if ended.is_ok() {
            guard.disarm();
        }
        let v = out?;
        ended?;
        Ok(v)
    }

    async fn insert_element(
        &self,
        table: &str,
        id: i64,
        labels: Vec<String>,
        props: BTreeMap<String, Value>,
    ) -> StoreResult<()> {
        for label in labels {
            self.exec(
                &format!("INSERT OR IGNORE INTO gql_{table}_labels VALUES (?1, ?2)"),
                (id, label),
            )
            .await?;
        }
        for (key, value) in props {
            if let Some((kind, value)) = encode_property(&value) {
                self.exec(
                    &format!("INSERT INTO gql_{table}_props VALUES (?1, ?2, ?3, ?4)"),
                    (id, key, kind, value),
                )
                .await?;
            }
        }
        Ok(())
    }
}

#[async_trait]
impl GraphStore for TursoStore {
    async fn nodes(&self) -> StoreResult<Vec<NodeId>> {
        let ids = self.ids("SELECT id FROM gql_nodes ORDER BY id", ()).await?;
        Ok(ids.into_iter().map(NodeId).collect())
    }

    async fn nodes_with_label(&self, label: &str) -> StoreResult<Vec<NodeId>> {
        let ids = self
            .ids(
                "SELECT node FROM gql_node_labels WHERE label = ?1 ORDER BY node",
                (label,),
            )
            .await?;
        Ok(ids.into_iter().map(NodeId).collect())
    }

    async fn node_labels(&self, node: NodeId) -> StoreResult<Vec<String>> {
        self.owned_strings(
            "node",
            node.0,
            "SELECT l.label FROM gql_nodes n \
             LEFT JOIN gql_node_labels l ON l.node = n.id \
             WHERE n.id = ?1 ORDER BY l.label",
        )
        .await
    }

    async fn node_has_label(&self, node: NodeId, label: &str) -> StoreResult<bool> {
        self.owned_flag(
            "node",
            node.0,
            "SELECT EXISTS (SELECT 1 FROM gql_node_labels l WHERE l.node = n.id AND l.label = ?2) \
             FROM gql_nodes n WHERE n.id = ?1",
            label,
        )
        .await
    }

    async fn node_property(&self, node: NodeId, key: &str) -> StoreResult<Option<Value>> {
        self.owned_property(
            "node",
            node.0,
            "SELECT p.kind, p.value FROM gql_nodes n \
             LEFT JOIN gql_node_props p ON p.node = n.id AND p.key = ?2 \
             WHERE n.id = ?1",
            key,
        )
        .await
    }

    async fn node_properties(&self, node: NodeId) -> StoreResult<BTreeMap<String, Value>> {
        self.owned_properties(
            "node",
            node.0,
            "SELECT p.key, p.kind, p.value FROM gql_nodes n \
             LEFT JOIN gql_node_props p ON p.node = n.id \
             WHERE n.id = ?1",
        )
        .await
    }

    async fn edges(&self, node: NodeId, incidence: Incidence) -> StoreResult<Vec<EdgeRef>> {
        let sql = match incidence {
            Incidence::Outgoing => {
                "SELECT e.id, e.source, e.target FROM gql_nodes n \
                 LEFT JOIN gql_edges e ON e.source = n.id \
                 WHERE n.id = ?1 ORDER BY e.id"
            }
            Incidence::Incoming => {
                "SELECT e.id, e.source, e.target FROM gql_nodes n \
                 LEFT JOIN gql_edges e ON e.target = n.id \
                 WHERE n.id = ?1 ORDER BY e.id"
            }
        };
        self.owned("node", node.0, sql, (node.0 as i64,))
            .await?
            .iter()
            .map(edge_ref)
            .collect()
    }

    async fn edge(&self, edge: EdgeId) -> StoreResult<EdgeRef> {
        let rows = self
            .owned(
                "edge",
                edge.0,
                "SELECT id, source, target FROM gql_edges WHERE id = ?1",
                (edge.0 as i64,),
            )
            .await?;
        edge_ref(rows.first().ok_or_else(|| not_found("edge", edge.0))?)
    }

    async fn edge_labels(&self, edge: EdgeId) -> StoreResult<Vec<String>> {
        self.owned_strings(
            "edge",
            edge.0,
            "SELECT l.label FROM gql_edges e \
             LEFT JOIN gql_edge_labels l ON l.edge = e.id \
             WHERE e.id = ?1 ORDER BY l.label",
        )
        .await
    }

    async fn edge_has_label(&self, edge: EdgeId, label: &str) -> StoreResult<bool> {
        self.owned_flag(
            "edge",
            edge.0,
            "SELECT EXISTS (SELECT 1 FROM gql_edge_labels l WHERE l.edge = e.id AND l.label = ?2) \
             FROM gql_edges e WHERE e.id = ?1",
            label,
        )
        .await
    }

    async fn edge_property(&self, edge: EdgeId, key: &str) -> StoreResult<Option<Value>> {
        self.owned_property(
            "edge",
            edge.0,
            "SELECT p.kind, p.value FROM gql_edges e \
             LEFT JOIN gql_edge_props p ON p.edge = e.id AND p.key = ?2 \
             WHERE e.id = ?1",
            key,
        )
        .await
    }

    async fn edge_properties(&self, edge: EdgeId) -> StoreResult<BTreeMap<String, Value>> {
        self.owned_properties(
            "edge",
            edge.0,
            "SELECT p.key, p.kind, p.value FROM gql_edges e \
             LEFT JOIN gql_edge_props p ON p.edge = e.id \
             WHERE e.id = ?1",
        )
        .await
    }
}

#[async_trait]
impl GraphStoreMut for TursoStore {
    async fn add_node(
        &mut self,
        labels: Vec<String>,
        props: BTreeMap<String, Value>,
    ) -> StoreResult<NodeId> {
        self.atomically(async || {
            let id = self
                .insert("INSERT INTO gql_nodes DEFAULT VALUES", ())
                .await?;
            self.insert_element("node", id, labels, props).await?;
            Ok(NodeId(id as u64))
        })
        .await
    }

    async fn add_edge(
        &mut self,
        source: NodeId,
        target: NodeId,
        labels: Vec<String>,
        props: BTreeMap<String, Value>,
    ) -> StoreResult<EdgeId> {
        self.atomically(async || {
            let id = self
                .insert(
                    "INSERT INTO gql_edges (source, target) VALUES (?1, ?2)",
                    (source.0 as i64, target.0 as i64),
                )
                .await?;
            self.insert_element("edge", id, labels, props).await?;
            Ok(EdgeId(id as u64))
        })
        .await
    }

    async fn set_node_property(
        &mut self,
        node: NodeId,
        key: &str,
        value: Value,
    ) -> StoreResult<()> {
        match encode_property(&value) {
            // Removing a property of an unknown node is NotFound, as setting
            // one is (through the foreign key).
            None => {
                self.atomically(async || {
                    self.owned(
                        "node",
                        node.0,
                        "SELECT id FROM gql_nodes WHERE id = ?1",
                        (node.0 as i64,),
                    )
                    .await?;
                    self.exec(
                        "DELETE FROM gql_node_props WHERE node = ?1 AND key = ?2",
                        (node.0 as i64, key),
                    )
                    .await
                })
                .await
            }
            Some((kind, value)) => {
                self.exec(
                    "INSERT INTO gql_node_props VALUES (?1, ?2, ?3, ?4) \
                     ON CONFLICT (node, key) DO UPDATE SET kind = excluded.kind, value = excluded.value",
                    (node.0 as i64, key, kind, value),
                )
                .await
            }
        }
    }
}

/// Classify a Turso error for callers deciding whether to retry.
fn store_error(e: turso::Error) -> StoreError {
    use turso::Error as T;
    let kind = match &e {
        T::Busy(_) | T::BusySnapshot(_) | T::Interrupt(_) | T::IoError(..) => {
            StoreErrorKind::Unavailable
        }
        T::Corrupt(_) | T::NotAdb(_) => StoreErrorKind::Corrupt,
        // The schema's only foreign keys are element ids.
        T::Constraint(m) if m.contains("FOREIGN KEY") => StoreErrorKind::NotFound,
        _ => StoreErrorKind::Other,
    };
    StoreError::new(kind, e)
}

fn not_found(what: &str, id: u64) -> StoreError {
    StoreError::new(StoreErrorKind::NotFound, format!("no {what} with id {id}"))
}

fn corrupt(msg: impl Into<String>) -> StoreError {
    StoreError::new(StoreErrorKind::Corrupt, msg.into())
}

const ABANDONED_NONE: u8 = 0;
/// A mutation's savepoint inside the caller's transaction.
const ABANDONED_SAVEPOINT: u8 = 1;
/// A whole transaction; rolling it back also discards any savepoint.
const ABANDONED_TRANSACTION: u8 = 2;

/// Roll back what an abandoned future left open.
async fn settle(conn: &Connection, abandoned: &AtomicU8) -> StoreResult<()> {
    let level = abandoned.swap(ABANDONED_NONE, Ordering::SeqCst);
    if level == ABANDONED_NONE || conn.is_autocommit().map_err(store_error)? {
        return Ok(());
    }
    let undo: &[&str] = if level == ABANDONED_TRANSACTION {
        &["ROLLBACK"]
    } else {
        &["ROLLBACK TO gql_mutation", "RELEASE gql_mutation"]
    };
    for sql in undo {
        if let Err(e) = conn.execute(*sql, ()).await {
            // Still open: try again on the next operation.
            abandoned.fetch_max(level, Ordering::SeqCst);
            return Err(store_error(e));
        }
    }
    Ok(())
}

/// Records its level in the store's `abandoned` flag when dropped armed.
struct Abandon(Option<(Arc<AtomicU8>, u8)>);

impl Abandon {
    fn arm(flag: &Arc<AtomicU8>, level: u8) -> Self {
        Abandon(Some((flag.clone(), level)))
    }

    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for Abandon {
    fn drop(&mut self) {
        if let Some((flag, level)) = self.0.take() {
            flag.fetch_max(level, Ordering::SeqCst);
        }
    }
}

fn column(row: &Row, i: usize) -> StoreResult<turso::Value> {
    row.get_value(i).map_err(store_error)
}

fn int(row: &Row, i: usize) -> StoreResult<i64> {
    match column(row, i)? {
        turso::Value::Integer(n) => Ok(n),
        other => Err(corrupt(format!("expected an integer, found {other:?}"))),
    }
}

fn text(row: &Row, i: usize) -> StoreResult<String> {
    match column(row, i)? {
        turso::Value::Text(s) => Ok(s),
        other => Err(corrupt(format!("expected text, found {other:?}"))),
    }
}

fn edge_ref(row: &Row) -> StoreResult<EdgeRef> {
    Ok(EdgeRef {
        id: EdgeId(int(row, 0)? as u64),
        source: NodeId(int(row, 1)? as u64),
        target: NodeId(int(row, 2)? as u64),
    })
}

/// `(kind, value)` columns for a property, or `None` for null, which is not
/// stored.
fn encode_property(v: &Value) -> Option<(i64, turso::Value)> {
    Some(match v {
        Value::Null => return None,
        Value::Bool(b) => (KIND_BOOL, turso::Value::Integer(*b as i64)),
        Value::Int(i) => (KIND_INT, turso::Value::Integer(*i)),
        Value::Float(x) if x.is_nan() => (KIND_FLOAT, turso::Value::Null),
        Value::Float(x) => (KIND_FLOAT, turso::Value::Real(*x)),
        Value::String(s) => (KIND_STRING, turso::Value::Text(s.clone())),
        other => {
            let mut buf = Vec::new();
            encode(other, &mut buf);
            (KIND_ENCODED, turso::Value::Blob(buf))
        }
    })
}

/// Decode the `kind, value` columns starting at `i`.
fn decode_property(row: &Row, i: usize) -> StoreResult<Value> {
    let kind = int(row, i)?;
    Ok(match (kind, column(row, i + 1)?) {
        (KIND_BOOL, turso::Value::Integer(n)) => Value::Bool(n != 0),
        (KIND_INT, turso::Value::Integer(n)) => Value::Int(n),
        (KIND_FLOAT, turso::Value::Real(x)) => Value::Float(x),
        (KIND_FLOAT, turso::Value::Null) => Value::Float(f64::NAN),
        // A real that holds an integral value may come back as an integer.
        (KIND_FLOAT, turso::Value::Integer(n)) => Value::Float(n as f64),
        (KIND_STRING, turso::Value::Text(s)) => Value::String(s),
        (KIND_ENCODED, turso::Value::Blob(b)) => {
            let mut input = b.as_slice();
            match decode(&mut input) {
                Some(v) if input.is_empty() => v,
                _ => return Err(corrupt("undecodable encoded property value")),
            }
        }
        (kind, value) => {
            return Err(corrupt(format!("property of kind {kind} holds {value:?}")));
        }
    })
}

// Binary encoding of lists, maps and element references: a tag byte, then a
// little-endian payload. Lengths and counts are u64.
const TAG_NULL: u8 = 0;
const TAG_BOOL: u8 = 1;
const TAG_INT: u8 = 2;
const TAG_FLOAT: u8 = 3;
const TAG_STRING: u8 = 4;
const TAG_LIST: u8 = 5;
const TAG_MAP: u8 = 6;
const TAG_NODE: u8 = 7;
const TAG_EDGE: u8 = 8;

fn encode(v: &Value, out: &mut Vec<u8>) {
    fn str(s: &str, out: &mut Vec<u8>) {
        out.extend((s.len() as u64).to_le_bytes());
        out.extend(s.as_bytes());
    }
    match v {
        Value::Null => out.push(TAG_NULL),
        Value::Bool(b) => out.extend([TAG_BOOL, *b as u8]),
        Value::Int(i) => {
            out.push(TAG_INT);
            out.extend(i.to_le_bytes());
        }
        Value::Float(x) => {
            out.push(TAG_FLOAT);
            out.extend(x.to_bits().to_le_bytes());
        }
        Value::String(s) => {
            out.push(TAG_STRING);
            str(s, out);
        }
        Value::List(xs) => {
            out.push(TAG_LIST);
            out.extend((xs.len() as u64).to_le_bytes());
            for x in xs {
                encode(x, out);
            }
        }
        Value::Map(m) => {
            out.push(TAG_MAP);
            out.extend((m.len() as u64).to_le_bytes());
            for (k, x) in m {
                str(k, out);
                encode(x, out);
            }
        }
        Value::Node(n) => {
            out.push(TAG_NODE);
            out.extend(n.0.to_le_bytes());
        }
        Value::Edge(e) => {
            out.push(TAG_EDGE);
            out.extend(e.0.to_le_bytes());
        }
    }
}

fn decode(input: &mut &[u8]) -> Option<Value> {
    fn take<'a>(input: &mut &'a [u8], n: usize) -> Option<&'a [u8]> {
        let (head, rest) = input.split_at_checked(n)?;
        *input = rest;
        Some(head)
    }
    fn u64_(input: &mut &[u8]) -> Option<u64> {
        Some(u64::from_le_bytes(take(input, 8)?.try_into().ok()?))
    }
    fn str(input: &mut &[u8]) -> Option<String> {
        let len = usize::try_from(u64_(input)?).ok()?;
        String::from_utf8(take(input, len)?.to_vec()).ok()
    }
    Some(match take(input, 1)?[0] {
        TAG_NULL => Value::Null,
        TAG_BOOL => Value::Bool(take(input, 1)?[0] != 0),
        TAG_INT => Value::Int(u64_(input)? as i64),
        TAG_FLOAT => Value::Float(f64::from_bits(u64_(input)?)),
        TAG_STRING => Value::String(str(input)?),
        TAG_LIST => {
            let n = u64_(input)?;
            let mut xs = Vec::new();
            for _ in 0..n {
                xs.push(decode(input)?);
            }
            Value::List(xs)
        }
        TAG_MAP => {
            let n = u64_(input)?;
            let mut m = BTreeMap::new();
            for _ in 0..n {
                let k = str(input)?;
                m.insert(k, decode(input)?);
            }
            Value::Map(m)
        }
        TAG_NODE => Value::Node(NodeId(u64_(input)?)),
        TAG_EDGE => Value::Edge(EdgeId(u64_(input)?)),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;

    #[test]
    fn connection_strings() {
        let file = |p: &str| Ok(TursoLocation::File(PathBuf::from(p)));
        for (input, want) in [
            (":memory:", Ok(TursoLocation::Memory)),
            ("file::memory:", Ok(TursoLocation::Memory)),
            ("graph.db", file("graph.db")),
            ("./data/graph.db", file("./data/graph.db")),
            ("/var/lib/slcr/graph.db", file("/var/lib/slcr/graph.db")),
            ("file:graph.db", file("graph.db")),
            ("file:/abs/graph.db", file("/abs/graph.db")),
            ("file:///abs/graph.db", file("/abs/graph.db")),
            ("", Err(())),
            ("file:", Err(())),
            ("file://host/graph.db", Err(())),
            ("file:graph.db?mode=ro", Err(())),
            ("libsql://my-db.turso.io", Err(())),
            ("https://my-db.turso.io", Err(())),
        ] {
            let got = input.parse::<TursoLocation>().map_err(|_| ());
            assert_eq!(got, want, "{input:?}");
        }
    }

    #[test]
    fn encoding_round_trips() {
        let mut m = BTreeMap::new();
        m.insert(
            "k".to_string(),
            Value::List(vec![Value::Null, Value::Bool(true)]),
        );
        m.insert(String::new(), Value::Float(-0.0));
        for v in [
            Value::List(vec![]),
            Value::List(vec![
                Value::Int(i64::MIN),
                Value::Float(f64::NAN),
                Value::String("é".into()),
            ]),
            Value::Map(m),
            Value::Node(NodeId(7)),
            Value::Edge(EdgeId(u64::MAX)),
        ] {
            let mut buf = Vec::new();
            encode(&v, &mut buf);
            let mut input = buf.as_slice();
            let back = decode(&mut input).unwrap();
            assert!(input.is_empty());
            assert_eq!(format!("{back:?}"), format!("{v:?}"));
        }
        assert_eq!(decode(&mut [TAG_LIST, 1, 0].as_slice()), None);
        assert_eq!(decode(&mut [42].as_slice()), None);
    }

    #[test]
    fn schema_shares_the_database_and_is_versioned() {
        pollster::block_on(async {
            let g = TursoStore::in_memory().await.unwrap();
            g.exec("CREATE TABLE app_users (name TEXT)", ())
                .await
                .unwrap();
            g.init_schema().await.unwrap();
            g.exec(
                "UPDATE gql_meta SET value = 2 WHERE key = 'schema_version'",
                (),
            )
            .await
            .unwrap();
            let err = g.init_schema().await.unwrap_err();
            assert!(
                matches!(err, TursoStoreError::SchemaVersion { .. }),
                "{err}"
            );
        });
    }

    #[test]
    fn dangling_edge_is_not_found() {
        pollster::block_on(async {
            let mut g = TursoStore::in_memory().await.unwrap();
            let a = g.add_node(vec!["A".into()], BTreeMap::new()).await.unwrap();
            let err = g
                .add_edge(a, NodeId(999), vec!["R".into()], BTreeMap::new())
                .await
                .unwrap_err();
            assert_eq!(err.kind(), StoreErrorKind::NotFound, "{err}");
            assert_eq!(
                g.edge(EdgeId(1)).await.unwrap_err().kind(),
                StoreErrorKind::NotFound
            );
            assert_eq!(g.edge_count().await.unwrap(), 0);
            assert!(g.conn.lock().await.is_autocommit().unwrap());
            let b = g.add_node(vec![], BTreeMap::new()).await.unwrap();
            g.add_edge(a, b, vec!["R".into()], BTreeMap::new())
                .await
                .unwrap();
            assert_eq!(g.edge_count().await.unwrap(), 1);
        });
    }

    /// A mutation that writes a node, then fails.
    async fn half_done(g: &TursoStore) -> StoreResult<()> {
        g.atomically(async || {
            g.insert("INSERT INTO gql_nodes DEFAULT VALUES", ()).await?;
            Err(StoreError::new(StoreErrorKind::Unavailable, "link lost"))
        })
        .await
    }

    #[test]
    fn failed_mutation_is_undone() {
        pollster::block_on(async {
            let mut g = TursoStore::in_memory().await.unwrap();
            // On its own: its transaction rolls back.
            assert!(half_done(&g).await.is_err());
            assert_eq!(g.node_count().await.unwrap(), 0);
            assert!(g.conn.lock().await.is_autocommit().unwrap());
            // In the caller's transaction: only its savepoint is undone, and
            // the transaction carries on.
            g.transaction(async |g| {
                g.add_node(vec!["Kept".into()], BTreeMap::new()).await?;
                assert!(half_done(g).await.is_err());
                g.add_node(vec!["AlsoKept".into()], BTreeMap::new()).await?;
                Ok::<_, StoreError>(())
            })
            .await
            .unwrap();
            assert_eq!(g.node_count().await.unwrap(), 2);
        });
    }

    #[test]
    fn cancelled_mutation_in_a_transaction_is_undone() {
        pollster::block_on(async {
            let mut g = TursoStore::in_memory().await.unwrap();
            g.transaction(async |g| {
                g.add_node(vec!["Kept".into()], BTreeMap::new()).await?;
                // Poll a mutation until it has written, then drop it.
                let wrote = std::sync::atomic::AtomicBool::new(false);
                {
                    let g = &*g;
                    let mut m = Box::pin(g.atomically(async || {
                        g.insert("INSERT INTO gql_nodes DEFAULT VALUES", ()).await?;
                        wrote.store(true, Ordering::SeqCst);
                        std::future::pending::<StoreResult<()>>().await
                    }));
                    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
                    while !wrote.load(Ordering::SeqCst) {
                        assert!(m.as_mut().poll(&mut cx).is_pending());
                    }
                }
                g.add_node(vec!["AlsoKept".into()], BTreeMap::new()).await?;
                Ok::<_, StoreError>(())
            })
            .await
            .unwrap();
            let labels: Vec<_> = {
                let mut out = Vec::new();
                for n in g.nodes().await.unwrap() {
                    out.extend(g.node_labels(n).await.unwrap());
                }
                out
            };
            assert_eq!(labels, ["Kept", "AlsoKept"]);
        });
    }

    #[test]
    fn dropped_transaction_rolls_back() {
        pollster::block_on(async {
            let mut g = TursoStore::in_memory().await.unwrap();
            // Poll the transaction until it parks after its write, then drop it.
            let wrote = std::sync::atomic::AtomicBool::new(false);
            let mut tx = Box::pin(g.transaction(async |g| {
                g.add_node(vec!["Lost".into()], BTreeMap::new())
                    .await
                    .unwrap();
                wrote.store(true, Ordering::SeqCst);
                std::future::pending::<()>().await;
                Ok::<_, StoreError>(())
            }));
            let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
            while !wrote.load(Ordering::SeqCst) {
                assert!(tx.as_mut().poll(&mut cx).is_pending());
            }
            drop(tx);
            assert_eq!(g.node_count().await.unwrap(), 0);
            assert!(g.conn.lock().await.is_autocommit().unwrap());
        });
    }
}
