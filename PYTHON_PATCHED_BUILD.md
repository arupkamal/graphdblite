# Using the patched graphdblite from Python

The `graphdblite` Python package is a Rust extension. Running `pip install graphdblite`
from PyPI gives you the **original, unpatched** code. To use the fixes from Python,
build the package from the patched source. Installing it compiles the Rust core.

The patched build was tested: all 23 tests in the Python binding's suite pass, and the
fixes were also checked by running queries from Python.

---

## Prerequisites

- **Rust toolchain**: install from <https://rustup.rs>
- **Python 3.10 or newer**
- One of these:
  - `graphdblite-main-fixed.zip` (the patched source tree), **or**
  - your own copy of the original source plus `graphdblite-fixes.patch`

---

## 1. Get the patched source

**Option A: use the zip**

```bash
unzip graphdblite-main-fixed.zip
cd graphdblite-main
```

**Option B: apply the patch to your copy**

Run this from the project root, the folder that contains `Cargo.toml`:

```bash
cd graphdblite-main
git apply -p2 --check /path/to/graphdblite-fixes.patch   # dry run
git apply -p2 /path/to/graphdblite-fixes.patch
```

Without git:

```bash
patch -p2 < /path/to/graphdblite-fixes.patch
```

> `-p2` is required. Paths inside the patch start with two folder levels that have to
> be stripped, e.g. `orig/graphdblite-main/src/db.rs` → `src/db.rs`.

---

## 2. Build and install into your environment

Use a fresh virtual environment so the PyPI version can't shadow the patched build:

```bash
python -m venv .venv
source .venv/bin/activate          # Windows: .venv\Scripts\activate
pip install maturin
maturin develop --release --manifest-path bindings/python/Cargo.toml
```

If the PyPI version is already installed in that environment, remove it first:

```bash
pip uninstall graphdblite
```

### Alternative: build a wheel (to install on other machines)

```bash
maturin build --release --manifest-path bindings/python/Cargo.toml
pip install target/wheels/graphdblite-*.whl
```

---

## 3. Confirm you're running the patched build

```python
from graphdblite import Database

db = Database("test.db")
db.execute("CREATE (:Person {name: 'Alice'})")
print(db.query("MATCH (n:person) RETURN count(n) AS c"))
# patched:  [{'c': 0}]
# original: [{'c': 1}]   <- old build: label matching ignored case
db.close()
```

---

## 4. (Optional) Run the binding's tests

```bash
pip install pytest
pytest bindings/python/tests
```

---

## What changes for your Python code

### Case-insensitive full-text search

With a case-insensitive full-text index (`create_fulltext_index_ci`), a plain
`CONTAINS` / `STARTS WITH` / `ENDS WITH` is now **case-sensitive**, as the openCypher
spec requires. An index no longer changes query results.

To match case-insensitively, use the `toLower` form. It's still accelerated by the
case-insensitive index:

```python
# before (no longer matches "Alice Smith"):
db.query("MATCH (n:Person) WHERE n.name CONTAINS 'alice' RETURN n.name")

# now:
db.query("MATCH (n:Person) WHERE toLower(n.name) CONTAINS toLower('alice') RETURN n.name")
```

### Queries that now raise instead of silently misbehaving

| Query pattern | Before | Now |
|---|---|---|
| `DELETE` / `SET` / `REMOVE` on a plain value, e.g. `WITH n.k AS x DELETE x` | Acted on the node whose id equalled the value | Raises an error |
| Map-valued properties in `CREATE`, `MERGE`, `ON CREATE/ON MATCH SET`, `SET n = {...}`, `SET n += {...}` | Stored the map | Raises an error |
| Property values over `max_property_value_bytes` (default 1 MiB), or names over `max_name_bytes` (default 256) | Accepted from Cypher | Raises an error |

Catch the base class `graphdblite.GraphDBError`, not a specific subclass. For example,
the integer-`DELETE` case surfaces as `ParseError` even though it's a runtime type error:

```python
from graphdblite import GraphDBError

try:
    db.execute("MATCH (n:K) WITH n.k AS x DELETE x")
except GraphDBError as e:
    print("rejected:", e)
```

### Other behaviour you get for free

- `MATCH (n:person)` no longer matches `:Person` nodes. A label like `:A_B` no longer
  matches `:AxB`.
- With an index on `:A(x)`, `MATCH (n:A:B {x: 1})` now requires the node to have `:B` too.
- Index lookups compare numbers the way Cypher does (`{x: 1}` matches a stored `1.0`).
  `{x: null}` never matches.
- `shortestPath` works without a relationship type (`[*]`) and with type lists
  (`[:A|B*]`).
- `MERGE` respects relationship direction and keeps every label.
  `ON CREATE SET` updates the edge it just created.
- `[:R*1..3 {w: $w}]` filters by the parameter instead of ignoring it.
- Cached query plans stay valid after a rolled-back `CREATE INDEX`, or a `DROP INDEX`
  from another process.

---

## Troubleshooting

- **`maturin: command not found`**: activate the virtual environment, then
  `pip install maturin`.
- **Rust compiler errors / `cargo` not found**: install Rust from <https://rustup.rs>,
  then open a new terminal.
- **Step 3 still prints `[{'c': 1}]`**: the PyPI build is being imported. Run
  `pip uninstall graphdblite` (repeat until nothing is left), then rerun
  `maturin develop`.
- **The patch doesn't apply**: your source differs from the snapshot the patch was
  made against. Try `git apply -p2 --3way ...`, or use `graphdblite-main-fixed.zip`.
