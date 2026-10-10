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

use super::{EdgeId, EdgeRef, GraphStore, GraphStoreMut, Incidence, NodeId, async_trait};
use crate::value::Value;
use async_lock::Mutex;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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
///     .await??;
/// let r = query(&g, "MATCH (r:Requirement) RETURN r.id", &Params::new()).await?;
/// assert_eq!(r.rows[0][0].to_string(), "'REQ-1'");
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// # }).unwrap();
/// ```
///
/// # Panics
///
/// [`GraphStore`] and [`GraphStoreMut`] are infallible, so their methods panic
/// if the database fails (an I/O error, a corrupt file, or another writer
/// holding the lock), and on ids that do not belong to this store, as
/// [`PetgraphStore`](super::PetgraphStore) does. Opening the store and
/// [`transaction`](Self::transaction) report errors instead.
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
    /// Set when a transaction is abandoned unfinished, because its future
    /// was dropped or panicked. `Drop` cannot run async SQL, so the next
    /// operation on the store rolls it back.
    abandoned: Arc<AtomicBool>,
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
            abandoned: Arc::new(AtomicBool::new(false)),
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

    pub async fn node_count(&self) -> usize {
        self.count("SELECT count(*) FROM gql_nodes").await
    }

    pub async fn edge_count(&self) -> usize {
        self.count("SELECT count(*) FROM gql_edges").await
    }

    /// Run `f` in one transaction: it commits if `f` returns `Ok` and rolls
    /// back if `f` returns `Err`, panics, or is dropped before finishing. The
    /// outer error reports a failure to begin or commit; the inner result is
    /// `f`'s.
    ///
    /// ```
    /// # use slcr_gql::{TursoStore, load_spec};
    /// # let src = include_str!("../../tests/data/todo-api.spec.json");
    /// # pollster::block_on(async {
    /// let mut g = TursoStore::in_memory().await?;
    /// let index = g.transaction(async |g| load_spec(src, g).await).await??;
    /// assert!(index.get("REQ-001").is_some());
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// # }).unwrap();
    /// ```
    pub async fn transaction<T, E>(
        &mut self,
        f: impl AsyncFnOnce(&mut Self) -> Result<T, E>,
    ) -> Result<Result<T, E>, TursoStoreError> {
        {
            let conn = self.conn.lock().await;
            settle(&conn, &self.abandoned).await?;
            conn.execute("BEGIN", ()).await?;
        }
        let mut guard = Abandon::arm(&self.abandoned);
        let out = f(self).await;
        let conn = self.conn.lock().await;
        let finished = match out {
            Ok(v) => conn.execute("COMMIT", ()).await.map(|_| Ok(v)),
            Err(e) => conn.execute("ROLLBACK", ()).await.map(|_| Err(e)),
        };
        // On error the guard stays armed: if the transaction is still open,
        // the next operation rolls it back.
        let out = finished?;
        guard.disarm();
        Ok(out)
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

    async fn rows(&self, sql: &str, params: impl IntoParams + Send) -> Vec<Row> {
        let res: turso::Result<Vec<Row>> = async {
            let conn = self.conn.lock().await;
            settle(&conn, &self.abandoned).await?;
            let mut stmt = conn.prepare_cached(sql).await?;
            let mut rows = stmt.query(params).await?;
            let mut out = Vec::new();
            while let Some(row) = rows.next().await? {
                out.push(row);
            }
            Ok(out)
        }
        .await;
        res.unwrap_or_else(|e| panic!("turso store: `{sql}`: {e}"))
    }

    async fn exec(&self, sql: &str, params: impl IntoParams + Send) {
        self.insert(sql, params).await;
    }

    /// Execute `sql`, returning the rowid it inserted, if any.
    async fn insert(&self, sql: &str, params: impl IntoParams + Send) -> i64 {
        let res: turso::Result<i64> = async {
            let conn = self.conn.lock().await;
            settle(&conn, &self.abandoned).await?;
            let mut stmt = conn.prepare_cached(sql).await?;
            stmt.execute(params).await?;
            Ok(conn.last_insert_rowid())
        }
        .await;
        res.unwrap_or_else(|e| panic!("turso store: `{sql}`: {e}"))
    }

    async fn count(&self, sql: &str) -> usize {
        let rows = self.rows(sql, ()).await;
        int(&rows[0], 0) as usize
    }

    async fn ids(&self, sql: &str, params: impl IntoParams + Send) -> Vec<u64> {
        self.rows(sql, params)
            .await
            .iter()
            .map(|r| int(r, 0) as u64)
            .collect()
    }

    async fn strings(&self, sql: &str, params: impl IntoParams + Send) -> Vec<String> {
        self.rows(sql, params)
            .await
            .iter()
            .map(|r| match r.get_value(0) {
                Ok(turso::Value::Text(s)) => s,
                other => panic!("turso store: expected text, found {other:?}"),
            })
            .collect()
    }

    async fn property(&self, sql: &str, id: u64, key: &str) -> Option<Value> {
        self.rows(sql, (id as i64, key))
            .await
            .first()
            .map(|r| decode_property(r, 0))
    }

    async fn properties(&self, sql: &str, id: u64) -> BTreeMap<String, Value> {
        self.rows(sql, (id as i64,))
            .await
            .iter()
            .map(|r| match r.get_value(0) {
                Ok(turso::Value::Text(k)) => (k, decode_property(r, 1)),
                other => panic!("turso store: expected text, found {other:?}"),
            })
            .collect()
    }

    /// Begin a transaction for one mutation, unless the caller's transaction
    /// is already open. Pass the result to [`Self::end_own`].
    async fn begin_own(&self) -> Option<Abandon> {
        let res: turso::Result<bool> = async {
            let conn = self.conn.lock().await;
            settle(&conn, &self.abandoned).await?;
            if !conn.is_autocommit()? {
                return Ok(false);
            }
            conn.execute("BEGIN", ()).await?;
            Ok(true)
        }
        .await;
        let began = res.unwrap_or_else(|e| panic!("turso store: BEGIN: {e}"));
        began.then(|| Abandon::arm(&self.abandoned))
    }

    async fn end_own(&self, own: Option<Abandon>) {
        if let Some(mut guard) = own {
            // Armed through COMMIT: if it fails, the transaction must not
            // stay open and swallow every later write.
            self.exec("COMMIT", ()).await;
            guard.disarm();
        }
    }

    async fn insert_element(
        &self,
        table: &str,
        id: i64,
        labels: Vec<String>,
        props: BTreeMap<String, Value>,
    ) {
        for label in labels {
            self.exec(
                &format!("INSERT OR IGNORE INTO gql_{table}_labels VALUES (?1, ?2)"),
                (id, label),
            )
            .await;
        }
        for (key, value) in props {
            if let Some((kind, value)) = encode_property(&value) {
                self.exec(
                    &format!("INSERT INTO gql_{table}_props VALUES (?1, ?2, ?3, ?4)"),
                    (id, key, kind, value),
                )
                .await;
            }
        }
    }
}

#[async_trait]
impl GraphStore for TursoStore {
    async fn nodes(&self) -> Vec<NodeId> {
        let ids = self.ids("SELECT id FROM gql_nodes ORDER BY id", ()).await;
        ids.into_iter().map(NodeId).collect()
    }

    async fn nodes_with_label(&self, label: &str) -> Vec<NodeId> {
        let ids = self
            .ids(
                "SELECT node FROM gql_node_labels WHERE label = ?1 ORDER BY node",
                (label,),
            )
            .await;
        ids.into_iter().map(NodeId).collect()
    }

    async fn node_labels(&self, node: NodeId) -> Vec<String> {
        self.strings(
            "SELECT label FROM gql_node_labels WHERE node = ?1 ORDER BY label",
            (node.0 as i64,),
        )
        .await
    }

    async fn node_has_label(&self, node: NodeId, label: &str) -> bool {
        !self
            .rows(
                "SELECT 1 FROM gql_node_labels WHERE node = ?1 AND label = ?2",
                (node.0 as i64, label),
            )
            .await
            .is_empty()
    }

    async fn node_property(&self, node: NodeId, key: &str) -> Option<Value> {
        self.property(
            "SELECT kind, value FROM gql_node_props WHERE node = ?1 AND key = ?2",
            node.0,
            key,
        )
        .await
    }

    async fn node_properties(&self, node: NodeId) -> BTreeMap<String, Value> {
        self.properties(
            "SELECT key, kind, value FROM gql_node_props WHERE node = ?1",
            node.0,
        )
        .await
    }

    async fn edges(&self, node: NodeId, incidence: Incidence) -> Vec<EdgeRef> {
        let sql = match incidence {
            Incidence::Outgoing => {
                "SELECT id, source, target FROM gql_edges WHERE source = ?1 ORDER BY id"
            }
            Incidence::Incoming => {
                "SELECT id, source, target FROM gql_edges WHERE target = ?1 ORDER BY id"
            }
        };
        self.rows(sql, (node.0 as i64,))
            .await
            .iter()
            .map(edge_ref)
            .collect()
    }

    async fn edge(&self, edge: EdgeId) -> EdgeRef {
        let rows = self
            .rows(
                "SELECT id, source, target FROM gql_edges WHERE id = ?1",
                (edge.0 as i64,),
            )
            .await;
        edge_ref(rows.first().expect("dangling edge id"))
    }

    async fn edge_labels(&self, edge: EdgeId) -> Vec<String> {
        self.strings(
            "SELECT label FROM gql_edge_labels WHERE edge = ?1 ORDER BY label",
            (edge.0 as i64,),
        )
        .await
    }

    async fn edge_has_label(&self, edge: EdgeId, label: &str) -> bool {
        !self
            .rows(
                "SELECT 1 FROM gql_edge_labels WHERE edge = ?1 AND label = ?2",
                (edge.0 as i64, label),
            )
            .await
            .is_empty()
    }

    async fn edge_property(&self, edge: EdgeId, key: &str) -> Option<Value> {
        self.property(
            "SELECT kind, value FROM gql_edge_props WHERE edge = ?1 AND key = ?2",
            edge.0,
            key,
        )
        .await
    }

    async fn edge_properties(&self, edge: EdgeId) -> BTreeMap<String, Value> {
        self.properties(
            "SELECT key, kind, value FROM gql_edge_props WHERE edge = ?1",
            edge.0,
        )
        .await
    }
}

#[async_trait]
impl GraphStoreMut for TursoStore {
    async fn add_node(&mut self, labels: Vec<String>, props: BTreeMap<String, Value>) -> NodeId {
        let own = self.begin_own().await;
        let id = self
            .insert("INSERT INTO gql_nodes DEFAULT VALUES", ())
            .await;
        self.insert_element("node", id, labels, props).await;
        self.end_own(own).await;
        NodeId(id as u64)
    }

    async fn add_edge(
        &mut self,
        source: NodeId,
        target: NodeId,
        labels: Vec<String>,
        props: BTreeMap<String, Value>,
    ) -> EdgeId {
        let own = self.begin_own().await;
        let id = self
            .insert(
                "INSERT INTO gql_edges (source, target) VALUES (?1, ?2)",
                (source.0 as i64, target.0 as i64),
            )
            .await;
        self.insert_element("edge", id, labels, props).await;
        self.end_own(own).await;
        EdgeId(id as u64)
    }

    async fn set_node_property(&mut self, node: NodeId, key: &str, value: Value) {
        match encode_property(&value) {
            None => {
                self.exec(
                    "DELETE FROM gql_node_props WHERE node = ?1 AND key = ?2",
                    (node.0 as i64, key),
                )
                .await;
            }
            Some((kind, value)) => {
                self.exec(
                    "INSERT INTO gql_node_props VALUES (?1, ?2, ?3, ?4) \
                     ON CONFLICT (node, key) DO UPDATE SET kind = excluded.kind, value = excluded.value",
                    (node.0 as i64, key, kind, value),
                )
                .await;
            }
        }
    }
}

/// Roll back a transaction abandoned by a dropped or panicked future.
async fn settle(conn: &Connection, abandoned: &AtomicBool) -> turso::Result<()> {
    if abandoned.swap(false, Ordering::SeqCst) && !conn.is_autocommit()? {
        conn.execute("ROLLBACK", ()).await?;
    }
    Ok(())
}

/// Marks the store's transaction abandoned when dropped while armed.
struct Abandon(Option<Arc<AtomicBool>>);

impl Abandon {
    fn arm(flag: &Arc<AtomicBool>) -> Self {
        Abandon(Some(flag.clone()))
    }

    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for Abandon {
    fn drop(&mut self) {
        if let Some(flag) = self.0.take() {
            flag.store(true, Ordering::SeqCst);
        }
    }
}

fn int(row: &Row, i: usize) -> i64 {
    match row.get_value(i) {
        Ok(turso::Value::Integer(n)) => n,
        other => panic!("turso store: expected integer, found {other:?}"),
    }
}

fn edge_ref(row: &Row) -> EdgeRef {
    EdgeRef {
        id: EdgeId(int(row, 0) as u64),
        source: NodeId(int(row, 1) as u64),
        target: NodeId(int(row, 2) as u64),
    }
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
fn decode_property(row: &Row, i: usize) -> Value {
    let kind = int(row, i);
    let value = row
        .get_value(i + 1)
        .unwrap_or_else(|e| panic!("turso store: {e}"));
    match (kind, value) {
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
                _ => panic!("turso store: corrupt encoded property value"),
            }
        }
        (kind, value) => panic!("turso store: property of kind {kind} holds {value:?}"),
    }
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
            g.exec("CREATE TABLE app_users (name TEXT)", ()).await;
            g.init_schema().await.unwrap();
            g.exec(
                "UPDATE gql_meta SET value = 2 WHERE key = 'schema_version'",
                (),
            )
            .await;
            let err = g.init_schema().await.unwrap_err();
            assert!(
                matches!(err, TursoStoreError::SchemaVersion { .. }),
                "{err}"
            );
        });
    }

    #[test]
    fn failed_mutation_rolls_back() {
        let mut g = pollster::block_on(TursoStore::in_memory()).unwrap();
        let a = pollster::block_on(g.add_node(vec!["A".into()], BTreeMap::new()));
        // The edge row is inserted, then the dangling target fails the
        // foreign key; the whole edge must roll back.
        let dangling = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            pollster::block_on(g.add_edge(a, NodeId(999), vec!["R".into()], BTreeMap::new()))
        }));
        assert!(dangling.is_err());
        pollster::block_on(async {
            assert_eq!(g.edge_count().await, 0);
            assert!(g.conn.lock().await.is_autocommit().unwrap());
            let b = g.add_node(vec![], BTreeMap::new()).await;
            g.add_edge(a, b, vec!["R".into()], BTreeMap::new()).await;
            assert_eq!((g.node_count().await, g.edge_count().await), (2, 1));
        });
    }

    #[test]
    fn dropped_transaction_rolls_back() {
        pollster::block_on(async {
            let mut g = TursoStore::in_memory().await.unwrap();
            // Poll the transaction until it parks after its write, then drop it.
            let wrote = AtomicBool::new(false);
            let mut tx = Box::pin(g.transaction(async |g| {
                g.add_node(vec!["Lost".into()], BTreeMap::new()).await;
                wrote.store(true, Ordering::SeqCst);
                std::future::pending::<()>().await;
                Ok::<_, ()>(())
            }));
            let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
            while !wrote.load(Ordering::SeqCst) {
                assert!(tx.as_mut().poll(&mut cx).is_pending());
            }
            drop(tx);
            assert_eq!(g.node_count().await, 0);
            assert!(g.conn.lock().await.is_autocommit().unwrap());
        });
    }
}
