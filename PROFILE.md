# SLCR Query Profile (draft 0.1)

The SLCR query language is a **read-only subset of ISO/IEC 39075:2024 (GQL)**.
Queries in the profile are, as far as possible, also valid openCypher, and a
small set of Cypher spellings is accepted as documented aliases that desugar to
the same AST. Anything outside the profile is rejected with
`Error::Unsupported`. The engine never silently approximates it.

## Statements

```
query        ::= clause* return [';']
clause       ::= match | for | filter
match        ::= MATCH path_pattern (',' path_pattern)* [WHERE expr]
for          ::= FOR var IN expr                      -- alias: UNWIND expr AS var
filter       ::= FILTER [WHERE] expr
return       ::= RETURN [DISTINCT] ('*' | item (',' item)*)
                 [ORDER BY sort (',' sort)*]
                 [(OFFSET | SKIP) expr] [LIMIT expr]
item         ::= expr [AS var]
sort         ::= expr [ASC | ASCENDING | DESC | DESCENDING]
```

## Graph patterns

```
path_pattern ::= node (edge node)*
node         ::= '(' [var] [label_spec] [props] [WHERE expr] ')'
edge         ::= full_edge [quantifier] | abbrev_edge [quantifier]
full_edge    ::= '-[' body ']->' | '<-[' body ']-' | '-[' body ']-'
abbrev_edge  ::= '->' | '<-' | '-'                     -- aliases: '-->' '<--' '--'
body         ::= [var] [label_spec] [cypher_star] [props] [WHERE expr]
quantifier   ::= '{' m ',' [n] '}' | '{' ',' n '}' | '{' n '}' | '*' | '+'
cypher_star  ::= '*' [m] ['..' [n]]                    -- alias for a quantifier
label_spec   ::= (':' | IS) label_expr (':' label_expr)*
label_expr   ::= label_expr '|' label_expr | label_expr '&' label_expr
               | '!' label_expr | '%' | name | '(' label_expr ')'
```

## Expressions

The profile covers literals (null, booleans, INT64, FLOAT64, strings, lists and
maps), `$parameters`, variables, property access `x.k`, and indexing `xs[i]`
(where a negative index counts from the end). Three-valued `AND`/`OR`/`XOR`/`NOT`
are supported, as are comparisons `= <> != < <= > >=`. Comparisons may be
chained, so `a < b < c` means `a < b AND b < c`. Arithmetic covers
`+ - * /`, and `||` concatenates strings and lists. GQL spells modulo and
exponentiation as `mod(a, b)` and `power(a, b)`; Cypher's `%` and `^`
operators are accepted as aliases for them. The profile also
includes `IS [NOT] NULL`, `IS [NOT] LABELED`, `IN`, `STARTS WITH`, `ENDS WITH`
and `CONTAINS`.

Operator precedence, from lowest to highest:

1. `OR`
2. `XOR`
3. `AND`
4. `NOT`
5. comparisons
6. `IS`, `IN` and string predicates
7. `+`, `-`, `||`
8. `*`, `/`, `%`
9. `^`
10. unary `-` and `+`
11. postfix `.k`, `[i]` and `:Label`

Functions use their canonical GQL names; Cypher spellings in parentheses are
accepted aliases.

| Group | Functions |
|---|---|
| Aggregates | `count` (including `count(*)` and `DISTINCT`), `sum`, `avg`, `min`, `max`, `collect_list` (`collect`) |
| Strings | `upper` (`toUpper`), `lower` (`toLower`), `trim`, `char_length` |
| Lists | `cardinality`, `size` |
| Numeric | `abs`, `floor`, `ceil`, `sqrt`, `mod` (`%`), `power` (`^`) |
| General | `coalesce` |
| Non-GQL extensions | `labels(node)` and `type(edge)` are Cypher compatibility extensions; portable queries should prefer `IS LABELED`. |

## Semantics

**Matching.**
- Match mode is DIFFERENT EDGES: within one `MATCH`, an edge binds at most once, while nodes may repeat. This is GQL's default and coincides with Cypher's relationship isomorphism.
- Because each edge binds at most once, unbounded quantifiers always terminate on finite graphs.
- Path mode is WALK.
- Reusing an edge variable twice in one `MATCH` is a compile-time error.

**Quantified edges.**
- A variable on a quantified edge is a *group variable*: a list of edges.
- Inside the edge's own `WHERE`, that variable refers to a single edge.
- `{m,n}` with `m > n` is a syntax error. Cypher would instead return an empty result.

**Aggregation.**
- Grouping is implicit: non-aggregate return items are the grouping keys.
- Outside an aggregate, an aggregating item may reuse only grouping keys that are a variable or a property access.
- `ORDER BY` after aggregation or `DISTINCT` may use only projected columns, plus the grouping keys under aggregation.

**Ordering.**
- Without `ORDER BY`, row order is unspecified by the profile. The reference implementation is deterministic: it follows graph insertion order.
- Across types, the order is MAP < NODE < EDGE < LIST < STRING < BOOLEAN < NUMBER.
- NaN sorts after every other number.
- Null sorts as the greatest value: last under `ASC`, first under `DESC`.

**Nulls and NaN.**
- Comparisons involving null yield null.
- Comparisons involving NaN yield false.
- `IN` follows three-valued logic.

**Numbers and errors.**
- Integer arithmetic is checked: overflow and division by zero raise data exceptions.
- `INT / INT` truncates.
- `^` always yields FLOAT.
- `+` is numeric only. Strings and lists concatenate with `||`, per GQL. This is a deliberate divergence from Cypher, which overloads `+`.

## The SLCR graph

`load_spec` maps a v1 specification document onto the property graph that
queries see:

| JSON | Graph |
|---|---|
| `section`, including the glossary | `:Section {id, title, body?}` |
| `requirement` | `:Requirement {id, title, body, modality, refinement?, rationale?}` |
| `acceptanceCriterion` | `:AcceptanceCriterion {id, title, body}` |
| `term` | `:Term {id, title, definition, aliases?}` |
| `children` / `terms` nesting | `(parent)-[:CONTAINS {position}]->(child)` |
| the glossary | the root's last `CONTAINS` child |
| `acceptanceCriteria` | `(req)-[:HAS_CRITERION {position}]->(ac)` |
| `refines` | `(req)-[:REFINES]->(refined)` |
| `dependsOn` | `(req)-[:DEPENDS_ON]->(target)` |
| `usesTerm` | `(node)-[:USES_TERM]->(term)` |

Every node also carries `spec`, the specification name. `position` is the
sibling index, so `ORDER BY e.position` yields document order. Derived data
such as satisfaction status is not part of the format; `apply_overlay`
attaches it as extra node properties until the verification-results schema
exists.

Because the path mode is WALK, a node reachable along several paths matches
once per path. Transitive queries such as "dependents of REQ-001" therefore
need `RETURN DISTINCT`.

## Deliberately excluded

The following raise `Error::Unsupported`:

- Writes of any kind. `CREATE`/`INSERT` are accepted only by the non-normative `load_fixture` loader.
- Clause-level features: `OPTIONAL MATCH`, `WITH`, `CALL`, `UNION` and other set operations, `LET`, `NEXT`.
- Path variables (`p = ...`), path modes (`TRAIL`, `ACYCLIC`, `SIMPLE`, `WALK`) and path selectors (`SHORTEST`, `ANY`, `ALL`).
- Expression forms: `CASE`, `EXISTS {}`, list and pattern comprehensions, pattern predicates, quantifier functions (`all`, `any`, `none`, `single`) and map projections.
- Temporal types, and any function not listed above.

## Conformance

Run `cargo test --test tck`. It executes the vendored openCypher and opengql
TCKs (see `tck/VERSIONS`) and classifies every scenario into one of four outcomes:

- **pass**: the expected result or expected error was produced.
- **wrong**: a silent wrong answer. CI fails on any.
- **errored**: a loud divergence.
- **rejected**: the scenario is outside the profile.

CI also fails if any scenario recorded as `pass` in `tck/baseline.tsv` regresses.

Current status:

| Suite | pass | wrong | errored | rejected |
|---|---|---|---|---|
| openCypher | 828 | 0 | 9 | 3060 |
| opengql | 164 | 0 | 0 | 42 |

All nine `errored` scenarios are the `+`-as-concatenation divergence described
under Semantics. Scenarios that expect an error, but where the profile rejects
the query for being out of scope, count as `rejected`, not `pass`.

The opengql TCK currently covers only value expressions and catalog
statements; it has no `MATCH` scenarios. Graph-pattern conformance therefore
rests on the openCypher suite until opengql grows.
