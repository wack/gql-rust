# slcr-gql

Reference implementation of the read-only **SLCR query profile**, a subset of
ISO GQL that is also valid openCypher. It queries the SLCR specification
graph. See [PROFILE.md](PROFILE.md) for the normative grammar and semantics.

```rust
use slcr_gql::{Params, PetgraphStore, load_fixture, query};

let mut g = PetgraphStore::new();
load_fixture(&mut g, "INSERT (:Requirement {id: 'REQ-2'})-[:REFINES]->(:Requirement {id: 'REQ-1'})")?;
let r = query(
    &g,
    "MATCH (d)-[:REFINES]->{1,}(:Requirement {id: 'REQ-1'}) RETURN d.id AS dependent",
    &Params::new(),
)?;
```

## Command line

```
cargo run --bin slcr-query -- tests/data/todo-api.spec.json \
  "MATCH (d:Requirement)-[:REFINES]->{1,}(:Requirement {id: 'REQ-001'})
   RETURN DISTINCT d.id AS dependent, d.modality ORDER BY dependent"

dependent  d.modality
---------  ----------
REQ-002    MUST
REQ-003    MUST
REQ-004    MUST
(3 rows)
```

The command also accepts the following options:

- `--tsv` prints tab-separated output instead of an aligned table.
- `--param NAME=JSON` binds `$NAME`.
- `--overlay FILE` attaches derived properties, for example `{"REQ-007": {"status": "satisfied"}}`.

## Layout

| Path | Purpose |
|---|---|
| `src/lexer.rs` | chumsky tokenizer (case-insensitive keywords, Cypher/GQL literals, comments) |
| `src/parser.rs` | chumsky recursive-descent parser over tokens; classifies failures as `Syntax` vs `Unsupported` |
| `src/ast.rs` | profile AST |
| `src/exec.rs` | semantic checks, DIFFERENT EDGES pattern matcher, projection/aggregation/ordering |
| `src/store/` | `GraphStore` trait + `PetgraphStore` (`StableDiGraph`) reference backend + `TursoStore` (`--features turso`) |
| `src/spec.rs` | loads the v1 SLCR JSON format into a `GraphStoreMut`, plus `apply_overlay` |
| `src/bin/slcr-query.rs` | CLI: query a spec file, table/TSV output |
| `src/fixture.rs` | non-normative `CREATE`/`INSERT` loader for tests |
| `tests/tck/` | Gherkin runner for the vendored TCKs; baseline gate |
| `tests/profile.rs` | SLCR-shaped behaviour tests |
| `tests/spec.rs` | loader tests against the canon `todo-api` example |
| `tests/oracle.rs` | differential testing against Grafeo (`--features grafeo`) |
| `tests/turso.rs` | `TursoStore` against the reference backend, persistence (`--features turso`) |
| `schema/` | the reviewed spec JSON Schema and canon example, copied from Notion |
| `tck/` | vendored openCypher + opengql TCKs (Apache-2.0), `VERSIONS`, `baseline.tsv` |

## Backends

The evaluator only uses the `GraphStore` trait. To back queries with SLCR's
own graph, implement `nodes`, `node_labels`, `node_property(ies)`, `edges`,
`edge`, `edge_labels` and `edge_property(ies)`. If you have a label index,
override `nodes_with_label`. Iteration must be deterministic.

### Turso

The `turso` feature adds `TursoStore`, which keeps the graph in a
[Turso](https://github.com/tursodatabase/turso) database. The connection
string decides whether the graph is in memory or persisted to a file:

```rust
use slcr_gql::{Params, TursoStore, load_spec, query};

let mut mem = TursoStore::open(":memory:")?;      // discarded on drop
let mut disk = TursoStore::open("graph.db")?;     // created if missing, reopened later
if disk.node_count() == 0 {
    disk.transaction(|g| load_spec(src, g))??;    // one commit for the whole load
}
let r = query(&disk, "MATCH (r:Requirement) RETURN r.id", &Params::new())?;
```

| Connection string | Database |
|---|---|
| `:memory:` or `file::memory:` | in memory, private to the store |
| `path/to/graph.db` or `file:path/to/graph.db` | persistent file |
| `file:///abs/path/graph.db` | persistent file |

Remote databases (`libsql://`, `https://`) and URI query parameters are
rejected. The graph lives in `gql_`-prefixed tables, so it can share a
database with other data. See `src/store/turso_store.rs` for the layout.
`GraphStore` is infallible, so a database failure after opening panics.
Outside `transaction`, every `add_node`/`add_edge` is its own commit, which
costs a disk sync on a file database.

Try it with `cargo run --features turso --example turso -- graph.db`. The first
run loads the canon spec, and later runs query the stored graph.

## Testing

CI runs the [cargo-make](https://github.com/sagiegurari/cargo-make) tasks in
`Makefile.toml`. Run the same tasks locally to predict CI: `cargo make
check-format`, `cargo make clippy`, `cargo make test`, the `grafeo` variants
`cargo make clippy-grafeo` and `cargo make test-grafeo`, the `turso` variants
`cargo make clippy-turso` and `cargo make test-turso`, or the full `cargo make
ci-flow`. Pull requests get the fast format and clippy gate, and the merge queue
runs the full suite with coverage. To work with individual scenarios:

```
cargo test                                    # unit, profile and TCK gate
TCK_VERBOSE=1 cargo test --test tck           # list every non-passing scenario
TCK_FILTER=Match5 cargo test --test tck       # subset
TCK_UPDATE_BASELINE=1 cargo test --test tck   # accept new passes
scripts/update-tck.sh                         # re-vendor at the commits in tck/VERSIONS
cargo test --features grafeo --test oracle -- --nocapture   # differential run
TCK_STORE=turso cargo test --features turso --test tck      # TCK on the Turso backend
```

The oracle pins Grafeo 0.5.42 and its sub-crates in `Cargo.lock`. That
pinning matters: `grafeo` depends on its own sub-crates with caret
requirements, so a plain `=0.5.42` pin still resolves `grafeo-core` and
`grafeo-common` to the latest 0.5.x. It also needs the `lpg` feature.
Without it, a GQL-only build panics at session creation.
