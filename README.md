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
| `src/store/` | `GraphStore` trait + `PetgraphStore` (`StableDiGraph`) reference backend |
| `src/spec.rs` | loads the v1 SLCR JSON format into a `GraphStoreMut`, plus `apply_overlay` |
| `src/bin/slcr-query.rs` | CLI: query a spec file, table/TSV output |
| `src/fixture.rs` | non-normative `CREATE`/`INSERT` loader for tests |
| `tests/tck/` | Gherkin runner for the vendored TCKs; baseline gate |
| `tests/profile.rs` | SLCR-shaped behaviour tests |
| `tests/spec.rs` | loader tests against the canon `todo-api` example |
| `tests/oracle.rs` | differential testing against Grafeo (`--features grafeo`) |
| `schema/` | the reviewed spec JSON Schema and canon example, copied from Notion |
| `tck/` | vendored openCypher + opengql TCKs (Apache-2.0), `VERSIONS`, `baseline.tsv` |

## Backends

The evaluator only uses the `GraphStore` trait. To back queries with SLCR's
own graph, implement `nodes`, `node_labels`, `node_property(ies)`, `edges`,
`edge`, `edge_labels` and `edge_property(ies)`. If you have a label index,
override `nodes_with_label`. Iteration must be deterministic.

## Testing

```
cargo test                                    # unit, profile and TCK gate
TCK_VERBOSE=1 cargo test --test tck           # list every non-passing scenario
TCK_FILTER=Match5 cargo test --test tck       # subset
TCK_UPDATE_BASELINE=1 cargo test --test tck   # accept new passes
scripts/update-tck.sh                         # re-vendor at the commits in tck/VERSIONS
cargo test --features grafeo --test oracle -- --nocapture   # differential run
```

The oracle pins Grafeo 0.5.42 and its sub-crates in `Cargo.lock`. That
pinning matters: `grafeo` depends on its own sub-crates with caret
requirements, so a plain `=0.5.42` pin still resolves `grafeo-core` and
`grafeo-common` to the latest 0.5.x. It also needs the `lpg` feature.
Without it, a GQL-only build panics at session creation.
