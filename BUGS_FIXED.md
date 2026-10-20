# graphdblite: bugs fixed

This lists the 18 bugs found in the original `graphdblite` code and fixed in the patched build, plus one Python binding bug. Each entry says what was wrong, what it did, and how it is fixed. Every fix has a regression test in `tests/bug_regressions.rs`, except the Python fix, which has `bindings/python/tests/test_relative_path.py`.

| # | Area | Problem | Fixed in |
|---|------|---------|----------|
| 1 | Label matching | Label matching was case-insensitive, and `_` acted as a wildcard | `src/storage/node.rs` |
| 2 | Index lookup | Index lookups ignored extra labels in the pattern | `src/cypher/planner/pattern.rs` |
| 3 | Index equality | Indexed equality lookups returned different rows from a full scan for numbers, `-0.0` and lists | `src/cypher/executor/read.rs` |
| 4 | shortestPath | Untyped `shortestPath` (`[*]`) found no path | `src/cypher/planner/pattern.rs`, `src/storage/edge.rs` |
| 5 | shortestPath | Only the first relationship type in a list was used | `src/cypher/ir.rs`, `src/storage/edge.rs` |
| 6 | MERGE direction | MERGE ignored relationship direction (incoming and undirected) | `src/cypher/executor/merge.rs` |
| 7 | MERGE labels | MERGE dropped labels after the first one on a node | `src/cypher/executor/merge.rs` |
| 8 | MERGE ON CREATE | ON CREATE SET could update the wrong edge when parallel edges existed | `src/cypher/executor/merge.rs`, `src/storage/edge.rs` |
| 9 | Variable-length paths | Property filters on `[:R*1..3 {w: $w}]` ignored parameters and variables | `src/cypher/executor/read.rs`, `correlated.rs` |
| 10 | DELETE / SET / REMOVE | On a plain value (for example `WITH n.k AS x DELETE x`) it acted on whichever node had that id | `src/cypher/executor/write.rs` |
| 11 | Full-text (CI index) | A case-insensitive index changed the results of plain `CONTAINS`, `STARTS WITH` and `ENDS WITH` | `src/cypher/executor/read.rs`, `src/cypher/ir.rs` |
| 12 | Plan cache | Cached plans went stale after a rolled-back `CREATE INDEX` or a `DROP INDEX` from another handle | `src/cypher/plan_cache.rs`, `src/db.rs` |
| 13 | Null properties | Null values were stored and indexed instead of removing the property | `src/transaction.rs`, `src/cypher/executor/merge.rs` |
| 14 | Long names | Names with multi-byte characters near the length limit could cause a panic | `src/types.rs` |
| 15 | Write limits | Cypher writes accepted map-valued properties and had no size limits | `src/cypher/executor/write.rs`, `src/cypher/executor/mod.rs` |
| 16 | shortestPath | An unbound endpoint was not handled correctly | `src/cypher/executor/path.rs`, `src/cypher/executor/correlated.rs` |
| 17 | MERGE ON MATCH | `ON MATCH SET n.visits = n.visits + 1` failed with "property access on Integer" | `src/cypher/executor/merge.rs` |
| 18 | shortestPath in a later MATCH | A second `MATCH` with `shortestPath` paired every node with every node and returned one row per pair | `src/cypher/executor/correlated.rs`, `src/cypher/executor/path.rs` |
| P1 | Python binding | `Database("my.db")` with a bare file name failed with "invalid path" | `bindings/python/src/lib.rs` |

---

## Details

### 1. Label matching ignored case and treated `_` as a wildcard
Label lookups used SQL `LIKE`, which is case-insensitive, and `_` matches any single character. So `MATCH (n:person)` matched `:Person`, and `:A_B` matched `:AxB`. Labels now compare exactly.

### 2. Index lookups dropped extra labels
With an index on `:A(x)`, `MATCH (n:A:B {x: 1})` returned nodes that had `:A` but not `:B`. Index lookups now still apply every label in the pattern.

### 3. Index equality disagreed with a scan
An indexed lookup on `{x: 1}` did not find a node stored with `1.0`, and it treated `-0.0` and list values differently from a full scan. Index lookups now expand these values into every form that compares equal, and fall back to a scan when that would be too many. `{x: null}` never matches, as in Cypher.

### 4. Untyped shortestPath found no path
`MATCH p = shortestPath((a)-[*]->(b))` with no relationship type returned nothing. It now searches all relationship types.

### 5. shortestPath used only one relationship type
`[:A|B*]` used only the first type. It now uses every type listed.

### 6. MERGE ignored direction
`MERGE (a)<-[:R]-(b)` could create or match the relationship in the wrong direction. Undirected MERGE also needed to match either direction. Both now follow the pattern.

### 7. MERGE dropped labels
`MERGE (n:A:B {id: 1})` kept only the first label. All labels are now kept, both when matching and when creating.

### 8. ON CREATE SET targeted the wrong parallel edge
When two relationships of the same type existed between the same nodes, `MERGE ... ON CREATE SET r.x = 1` could update an existing edge instead of the one it had just created. It now updates the new edge.

### 9. Variable-length path filters ignored parameters
`[:R*1..3 {w: $w}]` ignored `$w`, and property filters referencing variables did not work. They are now evaluated for every hop.

### 10. DELETE and SET acted on the wrong thing
`WITH n.k AS x DELETE x` deleted the node whose id equalled the value of `n.k`. Writes now reject values that are not nodes or relationships with an error.

### 11. Case-insensitive index changed plain predicates
With a case-insensitive full-text index, `WHERE n.name CONTAINS 'alice'` matched `Alice`. Cypher is case-sensitive, so it no longer does. The index still speeds up the case-insensitive form `toLower(n.name) CONTAINS toLower('alice')`.

**Behavior change:** code that relied on the old behavior needs to use the `toLower` form.

### 12. Stale cached plans
A query planned before an index was created or dropped could keep using the old plan. Plans are now tied to the schema version, so a rolled-back `CREATE INDEX` or a `DROP INDEX` from another connection is picked up.

### 13. Null values were stored
`SET n.x = null` and `CREATE (:N {x: null})` stored a null value, and indexes picked it up. Null now removes the property, and nulls are not indexed.

### 14. Panic on multi-byte names
Truncating a name that contained multi-byte characters could split a character. Truncation now stops at a character boundary.

### 15. Writes accepted maps and unlimited sizes
Cypher writes accepted map values as properties, and property values and names had no size limit. Maps are now rejected. Limits are configurable, with defaults of 1 MiB for property values and 256 bytes for names.

**Behavior change:** these now raise an error instead of being silently stored.

### 16. shortestPath with an unbound endpoint
When one endpoint was not bound by an earlier clause, the search did not handle it correctly. Both bound and unbound endpoints now work.

### 17. ON MATCH SET could not read node properties
`MERGE (n:Counter {k: 'a'}) ON MATCH SET n.visits = n.visits + 1` failed because `n` was not bound inside the ON MATCH clause. It is now bound with the node's current properties. Found during testing against your `make_test_graph.py`.

### 18. shortestPath in a second MATCH paired every node
`MATCH (a ...), (b ...) MATCH p = shortestPath((a)-[*]->(b))` ignored `a` and `b` from the first clause, so each pair of nodes produced a row (121 rows, hops 0 in your test). It now reuses the bound endpoints. Found during testing against your `make_test_graph.py`.

### P1. Python: bare file name failed
`Database("my.db")` resolved the parent folder of the bare name to an empty path and failed with "invalid path". A bare name now means the current folder.

---

## Known limitations (not fixed)

These are documented in `CHANGELOG.md` and `docs/cypher.md` and were not changed:

- Relationship values have no identity. Deleting a relationship from a list removes all parallel relationships of that list.
- MERGE of a standalone relationship finds or creates each node separately.
- `shortestPath` with no path returns a row with null values.
- `CALL { }`, `reduce`, map projections and `FOREACH` are not supported.
