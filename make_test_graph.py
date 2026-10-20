"""
make_test_graph.py — build a small, deterministic 11-node graph in graphdblite
for testing queries, traversals and tooling.

Install your fork first:
    pip install "git+https://github.com/arupkamal/graphdblite.git#subdirectory=bindings/python"

Run:
    python make_test_graph.py                    # writes test_graph.db (replacing it)
    python make_test_graph.py --db other.db      # custom path
    python make_test_graph.py --memory           # in-memory only, just runs the checks

Or reuse in tests:
    from graphdblite import Database
    from make_test_graph import build_test_graph
    db = Database.open_memory()
    build_test_graph(db)

Graph shape (all :LINK unless noted; number = weight):

    A -1-> B -1-> C -1-> D -1-> A        4-cycle A-B-C-D
    A -2-> E -2-> F -2-> G -2-> D        longer alternative route A -> D
    B -3-> H -3-> I -3-> H               2-cycle H <-> I
    I -1-> J                             J is a leaf (no outgoing edges)
    C -5-> C                             self-loop
    E -[:ALSO]-> B                       second relationship type
    K                                    isolated, no relationships

Features covered: multiple labels, two relationship types, cycles (4-cycle,
2-cycle, self-loop), an alternative longer path for shortest-path tests,
weights for weighted-path tests, a leaf node and an isolated node.
"""

import argparse
import os
import sys

from graphdblite import Database

# ---------------------------------------------------------------------------
# Data
# ---------------------------------------------------------------------------

# (id, extra label, name, group, value, active)
NODES = [
    ("A", "Core",   "Alpha",   "core",   10, True),
    ("B", "Core",   "Bravo",   "core",   20, True),
    ("C", "Core",   "Charlie", "core",   30, False),
    ("D", "Core",   "Delta",   "core",   40, True),
    ("E", "Branch", "Echo",    "branch", 15, True),
    ("F", "Branch", "Foxtrot", "branch", 25, False),
    ("G", "Branch", "Golf",    "branch", 35, True),
    ("H", "Leaf",   "Hotel",   "leaf",    5, True),
    ("I", "Leaf",   "India",   "leaf",   45, True),
    ("J", "Leaf",   "Juliett", "leaf",   50, False),
    ("K", "Orphan", "Kilo",    "orphan",  0, False),
]

# (source, target, relationship type, weight)
EDGES = [
    ("A", "B", "LINK", 1),
    ("B", "C", "LINK", 1),
    ("C", "D", "LINK", 1),
    ("D", "A", "LINK", 1),   # closes the A-B-C-D cycle
    ("A", "E", "LINK", 2),
    ("E", "F", "LINK", 2),
    ("F", "G", "LINK", 2),
    ("G", "D", "LINK", 2),   # longer alternative route A -> D
    ("B", "H", "LINK", 3),
    ("H", "I", "LINK", 3),
    ("I", "H", "LINK", 3),   # 2-cycle H <-> I
    ("I", "J", "LINK", 1),   # J is a leaf (no outgoing edges)
    ("C", "C", "LINK", 5),   # self-loop
    ("E", "B", "ALSO", 1),   # second relationship type
]

ALLOWED_LABELS = {"Core", "Branch", "Leaf", "Orphan"}
ALLOWED_REL_TYPES = {"LINK", "ALSO"}


# ---------------------------------------------------------------------------
# Build
# ---------------------------------------------------------------------------

def build_test_graph(db: Database) -> None:
    """Create the 11 nodes and 14 relationships in one write transaction.

    Assumes an empty database. Labels and relationship types cannot be
    Cypher parameters, so they come from the fixed allow-lists above;
    everything else is passed as $params.
    """
    with db.begin_write() as tx:
        for node_id, label, name, group, value, active in NODES:
            assert label in ALLOWED_LABELS
            tx.execute(
                f"CREATE (:Node:{label} {{id: $id, name: $name, "
                f"group: $group, value: $value, active: $active}})",
                {"id": node_id, "name": name, "group": group,
                 "value": value, "active": active},
            )

        for src, dst, rel_type, weight in EDGES:
            assert rel_type in ALLOWED_REL_TYPES
            tx.execute(
                f"MATCH (a:Node {{id: $src}}), (b:Node {{id: $dst}}) "
                f"CREATE (a)-[:{rel_type} {{weight: $w}}]->(b)",
                {"src": src, "dst": dst, "w": weight},
            )
    # leaving the `with` block commits; an exception rolls everything back


# ---------------------------------------------------------------------------
# Sanity checks (expected values follow directly from NODES / EDGES)
# ---------------------------------------------------------------------------

def _one(db: Database, cypher: str, key: str):
    rows = db.query(cypher)
    return rows[0][key] if rows else None


def verify_test_graph(db: Database) -> None:
    checks = []

    def check(desc, actual, expected):
        ok = actual == expected
        checks.append(ok)
        mark = "ok  " if ok else "FAIL"
        print(f"  [{mark}] {desc}: {actual!r}" + ("" if ok else f" (expected {expected!r})"))

    check("node count",
          _one(db, "MATCH (n:Node) RETURN count(n) AS c", "c"), 11)
    check("relationship count",
          _one(db, "MATCH ()-[r]->() RETURN count(r) AS c", "c"), 14)
    check(":LINK count",
          _one(db, "MATCH ()-[r:LINK]->() RETURN count(r) AS c", "c"), 13)
    check(":Core nodes",
          _one(db, "MATCH (n:Core) RETURN count(n) AS c", "c"), 4)
    check("isolated nodes",
          [r["id"] for r in db.query(
              "MATCH (n:Node) WHERE NOT (n)--() RETURN n.id AS id ORDER BY id")],
          ["K"])
    check("self-loops",
          [r["id"] for r in db.query(
              "MATCH (n:Node)-[:LINK]->(n) RETURN n.id AS id")],
          ["C"])
    check("out-degree of A (:LINK)",
          _one(db, "MATCH (:Node {id:'A'})-[r:LINK]->() RETURN count(r) AS c", "c"), 2)
    check("nodes reachable from A via :LINK*",
          _one(db, "MATCH (a:Node {id:'A'})-[:LINK*]->(b:Node) WHERE b <> a "
                   "RETURN count(DISTINCT b) AS c", "c"), 9)
    check("shortest A->D hops",
          _one(db, "MATCH (a:Node {id:'A'}), (d:Node {id:'D'}) "
                   "MATCH p = shortestPath((a)-[:LINK*]->(d)) "
                   "RETURN length(p) AS hops", "hops"), 3)
    check("leaf nodes (no outgoing :LINK, but connected)",
          [r["id"] for r in db.query(
              "MATCH (n:Node) WHERE NOT (n)-[:LINK]->() AND (n)--() "
              "RETURN n.id AS id ORDER BY id")],
          ["J"])
    check("sum of values",
          _one(db, "MATCH (n:Node) RETURN sum(n.value) AS s", "s"),
          sum(n[4] for n in NODES))

    passed = sum(checks)
    print(f"\n{passed}/{len(checks)} checks passed")
    if passed != len(checks):
        sys.exit(1)


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------

def _remove_db_files(path: str) -> None:
    for suffix in ("", "-wal", "-shm"):
        try:
            os.remove(path + suffix)
        except FileNotFoundError:
            pass


def main() -> None:
    parser = argparse.ArgumentParser(description="Build an 11-node graphdblite test graph.")
    parser.add_argument("--db", default="test_graph.db",
                        help="database file to create (replaced if it exists)")
    parser.add_argument("--memory", action="store_true",
                        help="use an in-memory database instead of a file")
    args = parser.parse_args()

    if args.memory:
        db = Database.open_memory()
        target = "in-memory database"
    else:
        _remove_db_files(args.db)
        db = Database(args.db)
        target = args.db

    with db:
        build_test_graph(db)
        print(f"Built test graph in {target}: {len(NODES)} nodes, {len(EDGES)} relationships\n")
        verify_test_graph(db)


if __name__ == "__main__":
    main()
