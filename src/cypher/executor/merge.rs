//! MERGE evaluation and find-or-create helpers.

use rusqlite::Connection;

use crate::cypher::ast::*;
use crate::cypher::eval::eval_expr;
use crate::cypher::record::NamedRecord;
use crate::types::*;
use crate::{edge, fts, index, node};

use super::write::{validate_write_labels, validate_write_properties, validate_write_value};
use super::*;

pub(in crate::cypher::executor) fn apply_merge_set_item_node(
    conn: &Connection,
    item: &SetItem,
    node_id: NodeId,
    rec: &NamedRecord,
) -> Result<()> {
    // Bind the merged node (id, labels and current properties) under its
    // variable so ON CREATE / ON MATCH expressions can read it, e.g.
    // `ON MATCH SET n.visits = n.visits + 1`.
    let bound_rec = |variable: &str| -> Result<NamedRecord> {
        let node = node::get_node(conn, node_id)?;
        let mut r = rec.clone();
        super::util::populate_node_bindings(&mut r, &node, variable);
        Ok(r)
    };
    match item {
        SetItem::Property(assignment) => {
            let old = node::get_node(conn, node_id)?;
            let a_rec = bound_rec(&assignment.variable)?;
            let val = eval_expr(
                &assignment.value,
                &a_rec,
                crate::cypher::eval::EvalCx::new(conn),
            )?;
            validate_write_value(&assignment.property, &val)?;
            node::set_node_property(conn, node_id, &assignment.property, val.clone())?;
            let mut new_props = old.properties.clone();
            // `SET n.p = null` removes the property; never index a Null.
            if val == Value::Null {
                new_props.remove(&assignment.property);
            } else {
                new_props.insert(assignment.property.clone(), val);
            }
            index::update_indexes_for_node(
                conn,
                node_id,
                &old.labels,
                Some(&old.properties),
                &new_props,
            )?;
            fts::update_fts_for_node(
                conn,
                node_id,
                &old.labels,
                Some(&old.properties),
                &new_props,
            )?;
        }
        SetItem::Label {
            variable: _,
            labels,
        } => {
            // Adding labels can bring new per-label indexes into scope for this
            // node, so re-insert index/FTS entries across the full label set.
            validate_write_labels(labels)?;
            for label in labels {
                node::add_node_label(conn, node_id, label)?;
            }
            let new = node::get_node(conn, node_id)?;
            index::update_indexes_for_node(conn, node_id, &new.labels, None, &new.properties)?;
            fts::update_fts_for_node(conn, node_id, &new.labels, None, &new.properties)?;
        }
        SetItem::MapMerge { variable, value } => {
            let map = resolve_to_map(value, &bound_rec(variable)?, conn)?;
            validate_non_null_entries(&map)?;
            let old = node::get_node(conn, node_id)?;
            let mut new_props = old.properties.clone();
            for (k, v) in &map {
                if *v == Value::Null {
                    new_props.remove(k);
                } else {
                    new_props.insert(k.clone(), v.clone());
                }
                node::set_node_property(conn, node_id, k, v.clone())?;
            }
            maintain_indexes_after_prop_change(conn, node_id, &old, &new_props)?;
        }
        SetItem::MapOverwrite { variable, value } => {
            let map = resolve_to_map(value, &bound_rec(variable)?, conn)?;
            validate_non_null_entries(&map)?;
            // Clear existing properties and set new ones.
            let old = node::get_node(conn, node_id)?;
            for key in old.properties.keys() {
                node::set_node_property(conn, node_id, key, Value::Null)?;
            }
            let mut new_props = Properties::new();
            for (k, v) in &map {
                if *v != Value::Null {
                    new_props.insert(k.clone(), v.clone());
                }
                node::set_node_property(conn, node_id, k, v.clone())?;
            }
            maintain_indexes_after_prop_change(conn, node_id, &old, &new_props)?;
        }
    }
    Ok(())
}

/// Bring secondary + FTS indexes in sync after a MERGE `SET`/`SET +=` map write
/// mutates a node's properties, mirroring the property-change maintenance in
/// `write.rs`. `old` is the node as read *before* the write; `new_props` is the
/// full post-write property set.
fn maintain_indexes_after_prop_change(
    conn: &Connection,
    node_id: NodeId,
    old: &Node,
    new_props: &Properties,
) -> Result<()> {
    index::update_indexes_for_node(conn, node_id, &old.labels, Some(&old.properties), new_props)?;
    fts::update_fts_for_node(conn, node_id, &old.labels, Some(&old.properties), new_props)?;
    Ok(())
}

/// Apply a single SetItem to a specific parallel edge by sequence number.
pub(in crate::cypher::executor) fn apply_merge_set_item_edge_at(
    conn: &Connection,
    item: &SetItem,
    src_id: NodeId,
    dst_id: NodeId,
    edge_type: &str,
    seq: u64,
    rec: &NamedRecord,
) -> Result<()> {
    match item {
        SetItem::Property(assignment) => {
            let val = eval_expr(
                &assignment.value,
                rec,
                crate::cypher::eval::EvalCx::new(conn),
            )?;
            validate_write_value(&assignment.property, &val)?;
            edge::set_edge_property_at(
                conn,
                src_id,
                dst_id,
                edge_type,
                seq,
                &assignment.property,
                val,
            )?;
        }
        SetItem::Label { .. } => {}
        SetItem::MapMerge { variable: _, value } => {
            let map = resolve_to_map(value, rec, conn)?;
            validate_non_null_entries(&map)?;
            for (k, v) in &map {
                edge::set_edge_property_at(conn, src_id, dst_id, edge_type, seq, k, v.clone())?;
            }
        }
        SetItem::MapOverwrite { variable: _, value } => {
            let map = resolve_to_map(value, rec, conn)?;
            validate_non_null_entries(&map)?;
            let old_props = edge::get_edge_properties_at(conn, src_id, dst_id, edge_type, seq)?;
            for key in old_props.keys() {
                edge::set_edge_property_at(conn, src_id, dst_id, edge_type, seq, key, Value::Null)?;
            }
            for (k, v) in &map {
                edge::set_edge_property_at(conn, src_id, dst_id, edge_type, seq, k, v.clone())?;
            }
        }
    }
    Ok(())
}

/// Validate every non-null entry of a `SET x = map` / `SET x += map` payload
/// (null entries mean "remove" and are always allowed).
fn validate_non_null_entries(map: &Properties) -> Result<()> {
    for (k, v) in map {
        if *v != Value::Null {
            validate_write_value(k, v)?;
            crate::types::validate_name(k)?;
        }
    }
    Ok(())
}

/// Resolve an expression to a property map. If the expression evaluates to a
/// node ID, load that node's properties. If it evaluates to an edge, use the
/// edge's properties. If it's already a map, use it directly.
pub(in crate::cypher::executor) fn resolve_to_map(
    expr: &Expr,
    rec: &NamedRecord,
    conn: &Connection,
) -> Result<Properties> {
    let val = eval_expr(expr, rec, crate::cypher::eval::EvalCx::new(conn))?;
    match val {
        Value::Map(map) => Ok(map.into_iter().collect()),
        Value::Node(n) => Ok(n.properties),
        Value::Edge(e) => Ok(e.properties),
        Value::I64(id) => {
            // Could be a node ID — try to load its properties.
            match node::get_node(conn, NodeId(id as u64)) {
                Ok(n) => Ok(n.properties),
                Err(_) => Ok(Properties::new()),
            }
        }
        Value::Null => Ok(Properties::new()),
        _ => Ok(Properties::new()),
    }
}

pub(in crate::cypher::executor) fn exec_merge(
    conn: &Connection,
    pattern: &crate::cypher::ast::Pattern,
    on_create: &[SetItem],
    on_match: &[SetItem],
) -> Result<Vec<NamedRecord>> {
    if pattern.elements.len() == 1 {
        return exec_merge_node(conn, pattern, on_create, on_match);
    }
    // 3-element relationship MERGE: (a:L {p})-[:TYPE]->(b:L {p})
    exec_merge_relationship(conn, pattern, on_create, on_match)
}

/// Single-node MERGE: find-or-create a node matching the pattern.
pub(in crate::cypher::executor) fn exec_merge_node(
    conn: &Connection,
    pattern: &crate::cypher::ast::Pattern,
    on_create: &[SetItem],
    on_match: &[SetItem],
) -> Result<Vec<NamedRecord>> {
    let node_pat = match pattern.elements.first() {
        Some(PatternElement::Node(n)) => n,
        _ => unreachable!("MERGE pattern validated at plan time"),
    };

    let all_labels: Vec<&str> = node_pat.labels.iter().map(|s| s.as_str()).collect();
    let alias = node_pat.variable.as_deref().unwrap_or("_merge");

    // Pre-evaluate properties.
    let dummy_rec = NamedRecord::new();
    let mut props = Properties::new();
    for (key, expr) in &node_pat.properties {
        let val = eval_expr(expr, &dummy_rec, crate::cypher::eval::EvalCx::new(conn))?;
        // Null property in MERGE is MergeReadOwnWrites.
        if val == Value::Null {
            return Err(GraphError::semantic("MERGE with null property value")
                .with_code(ErrorCode::MergeReadOwnWrites));
        }
        props.insert(key.clone(), val);
    }

    // Find a node that has ALL required labels and matching properties.
    let matched = find_merge_match_multi_label(conn, &all_labels, &props)?;

    let node_id = match matched {
        Some(n) => {
            let rec = NamedRecord::new();
            for item in on_match {
                apply_merge_set_item_node(conn, item, n.id, &rec)?;
            }
            n.id
        }
        None => {
            let labels: Vec<String> = all_labels.iter().map(|s| s.to_string()).collect();
            validate_write_labels(&labels)?;
            validate_write_properties(&props)?;
            let id = node::create_node(conn, &labels, props.clone())?;
            index::update_indexes_for_node(conn, id, &labels, None, &props)?;
            fts::update_fts_for_node(conn, id, &labels, None, &props)?;

            let rec = NamedRecord::new();
            for item in on_create {
                apply_merge_set_item_node(conn, item, id, &rec)?;
            }
            id
        }
    };

    let mut rec = NamedRecord::new();
    rec.set(alias.to_string(), Value::I64(node_id.0 as i64));
    rec.set(format!("{alias}.__id"), Value::I64(node_id.0 as i64));
    // Populate property bindings from the resolved node so slot-path Property
    // reads land on populated slots. Skip `__label`/`__labels`: that pair
    // triggers `build_compound_binding`, shifting Variable resolution from
    // `Value::I64` to `Value::Node` and breaking downstream code paths that
    // expect the flat shape (see exec_create_node note).
    let resolved = node::get_node(conn, node_id)?;
    for (k, v) in &resolved.properties {
        rec.set(format!("{alias}.{k}"), v.clone());
    }

    // Bind path variable if present: MERGE p = (a {props})
    if let Some(ref path_var) = pattern.path_variable {
        rec.set(path_var.clone(), Value::Path(PathValue::single(resolved)));
    }

    Ok(vec![rec])
}

/// Relationship MERGE: find-or-create nodes and the edge between them.
pub(in crate::cypher::executor) fn exec_merge_relationship(
    conn: &Connection,
    pattern: &crate::cypher::ast::Pattern,
    on_create: &[SetItem],
    on_match: &[SetItem],
) -> Result<Vec<NamedRecord>> {
    let src_pat = match &pattern.elements[0] {
        PatternElement::Node(n) => n,
        _ => unreachable!(),
    };
    let rel = match &pattern.elements[1] {
        PatternElement::Relationship(r) => r,
        _ => unreachable!(),
    };
    let dst_pat = match &pattern.elements[2] {
        PatternElement::Node(n) => n,
        _ => unreachable!(),
    };

    // Find or create source and destination nodes (all labels respected).
    let src_id = find_or_create_merge_node(conn, &src_pat.labels, &src_pat.properties)?;
    let dst_id = find_or_create_merge_node(conn, &dst_pat.labels, &dst_pat.properties)?;

    let mut base = NamedRecord::new();
    for (pat, id) in [(src_pat, src_id), (dst_pat, dst_id)] {
        if let Some(ref v) = pat.variable {
            base.set(v.clone(), Value::I64(id.0 as i64));
            base.set(format!("{v}.__id"), Value::I64(id.0 as i64));
        }
    }
    merge_edge_between(
        conn,
        &base,
        src_id,
        dst_id,
        rel,
        pattern.path_variable.as_ref(),
        on_create,
        on_match,
    )
}

/// Match-or-create the relationship described by `rel` between the pattern's
/// left node `left_id` and right node `right_id`, honouring the pattern's
/// direction: `(l)-[]->(r)` is `l→r`, `(l)<-[]-(r)` is `r→l`, and an undirected
/// `(l)-[]-(r)` matches either orientation and creates `l→r`.
///
/// Returns one row per matched parallel edge (with ON MATCH applied to that
/// edge), or a single row for the newly created edge (with ON CREATE applied
/// to exactly that edge). The relationship variable and the optional path
/// variable are bound in every row.
#[allow(clippy::too_many_arguments)]
fn merge_edge_between(
    conn: &Connection,
    base_rec: &NamedRecord,
    left_id: NodeId,
    right_id: NodeId,
    rel: &crate::cypher::ast::RelPattern,
    path_var: Option<&String>,
    on_create: &[SetItem],
    on_match: &[SetItem],
) -> Result<Vec<NamedRecord>> {
    use crate::cypher::ast::RelDirection;

    let edge_type = rel.rel_types.first().cloned().unwrap_or_default();
    let (src_id, dst_id) = match rel.direction {
        RelDirection::Incoming => (right_id, left_id),
        _ => (left_id, right_id),
    };
    let undirected = matches!(rel.direction, RelDirection::Undirected);

    // Pre-evaluate merge pattern edge properties.
    let mut merge_edge_props = Properties::new();
    for (key, expr) in &rel.properties {
        let val = eval_expr(expr, base_rec, crate::cypher::eval::EvalCx::new(conn))?;
        if val == Value::Null {
            return Err(GraphError::semantic("MERGE with null property value")
                .with_code(ErrorCode::MergeReadOwnWrites));
        }
        merge_edge_props.insert(key.clone(), val);
    }

    let find_matching_edges = |s: NodeId, d: NodeId| -> Result<Vec<(u64, Properties)>> {
        let all = edge::get_all_edge_props(conn, s, d, &edge_type)?;
        Ok(all
            .into_iter()
            .filter(|(_, props)| {
                merge_edge_props
                    .iter()
                    .all(|(k, v)| props.get(k) == Some(v))
            })
            .collect())
    };

    let mut matched: Vec<(NodeId, NodeId, u64)> = Vec::new();
    for (seq, _) in find_matching_edges(src_id, dst_id)? {
        matched.push((src_id, dst_id, seq));
    }
    if undirected && src_id != dst_id {
        for (seq, _) in find_matching_edges(dst_id, src_id)? {
            matched.push((dst_id, src_id, seq));
        }
    }

    if matched.is_empty() {
        validate_write_labels(std::slice::from_ref(&edge_type))?;
        validate_write_properties(&merge_edge_props)?;
        let seq = edge::create_edge(conn, src_id, dst_id, &edge_type, merge_edge_props)?;
        for item in on_create {
            apply_merge_set_item_edge_at(conn, item, src_id, dst_id, &edge_type, seq, base_rec)?;
        }
        matched.push((src_id, dst_id, seq));
    } else {
        for (s, d, seq) in &matched {
            for item in on_match {
                apply_merge_set_item_edge_at(conn, item, *s, *d, &edge_type, *seq, base_rec)?;
            }
        }
    }

    let mut rows = Vec::with_capacity(matched.len());
    for (s, d, seq) in matched {
        let mut out = base_rec.clone();
        let props = edge::get_edge_properties_at(conn, s, d, &edge_type, seq)?;
        if let Some(ref r_alias) = rel.variable {
            out.set(r_alias.clone(), Value::String(edge_type.clone()));
            out.set(format!("{r_alias}.__src"), Value::I64(s.0 as i64));
            out.set(format!("{r_alias}.__dst"), Value::I64(d.0 as i64));
            out.set(
                format!("{r_alias}.__type"),
                Value::String(edge_type.clone()),
            );
            out.set(format!("{r_alias}.__edge_seq"), Value::I64(seq as i64));
            for (key, val) in &props {
                out.set(format!("{r_alias}.{key}"), val.clone());
            }
        }
        if let Some(path_var) = path_var {
            out.set(
                path_var.clone(),
                Value::Path(PathValue {
                    nodes: vec![
                        node::get_node(conn, left_id)?,
                        node::get_node(conn, right_id)?,
                    ],
                    edges: vec![crate::types::Edge {
                        src: s,
                        dst: d,
                        label: edge_type.clone(),
                        properties: props,
                    }],
                }),
            );
        }
        rows.push(out);
    }
    Ok(rows)
}

/// Find a node carrying every label in `labels` and matching the pattern
/// properties, or create it with all of those labels.
pub(in crate::cypher::executor) fn find_or_create_merge_node(
    conn: &Connection,
    labels: &[String],
    properties: &HashMap<String, Expr>,
) -> Result<NodeId> {
    let dummy_rec = NamedRecord::new();
    let mut props = Properties::new();
    for (key, expr) in properties {
        let val = eval_expr(expr, &dummy_rec, crate::cypher::eval::EvalCx::new(conn))?;
        if val == Value::Null {
            return Err(GraphError::semantic("MERGE with null property value")
                .with_code(ErrorCode::MergeReadOwnWrites));
        }
        props.insert(key.clone(), val);
    }
    let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();
    match find_merge_match_multi_label(conn, &label_refs, &props)? {
        Some(n) => Ok(n.id),
        None => {
            validate_write_labels(labels)?;
            validate_write_properties(&props)?;
            let id = node::create_node(conn, labels, props.clone())?;
            index::update_indexes_for_node(conn, id, labels, None, &props)?;
            fts::update_fts_for_node(conn, id, labels, None, &props)?;
            Ok(id)
        }
    }
}

pub(in crate::cypher::executor) fn exec_match_merge(
    conn: &Connection,
    input: &LogicalOp,
    merge_pattern: &crate::cypher::ast::Pattern,
    on_create: &[SetItem],
    on_match: &[SetItem],
    ctx: &ExecContext,
) -> Result<Vec<NamedRecord>> {
    let records = exec(conn, input, ctx)?;
    let mut result = Vec::with_capacity(records.len());

    // Extract the merge pattern structure: (src_node)-[:TYPE]->(dst_node) or single node.
    let elements = &merge_pattern.elements;

    if elements.len() == 1 {
        // Single node MERGE — match-or-create per input record.
        // MERGE produces one output row per matching node (all matches, not just first).
        for rec in &records {
            let node_pat = match &elements[0] {
                PatternElement::Node(n) => n,
                _ => unreachable!(),
            };
            let label_refs: Vec<&str> = node_pat.labels.iter().map(String::as_str).collect();
            let label = label_refs.first().copied().unwrap_or("");
            let mut props = Properties::new();
            for (key, expr) in &node_pat.properties {
                let val = eval_expr(expr, rec, crate::cypher::eval::EvalCx::new(conn))?;
                // Null property in MERGE is MergeReadOwnWrites.
                if val == Value::Null {
                    return Err(GraphError::semantic("MERGE with null property value")
                        .with_code(ErrorCode::MergeReadOwnWrites));
                }
                props.insert(key.clone(), val);
            }
            // Candidates come from the first label (index-assisted); the node
            // must also carry every other label in the pattern.
            let matches: Vec<crate::types::Node> =
                find_merge_matches_evaluated(conn, label, &props)?
                    .into_iter()
                    .filter(|n| {
                        label_refs
                            .iter()
                            .all(|req| n.labels.iter().any(|l| l == req))
                    })
                    .collect();
            let alias = node_pat.variable.as_deref().unwrap_or("_merge");

            if matches.is_empty() {
                // No match — create a new node carrying all pattern labels.
                let labels: Vec<String> = node_pat.labels.clone();
                validate_write_labels(&labels)?;
                validate_write_properties(&props)?;
                let id = node::create_node(conn, &labels, props.clone())?;
                index::update_indexes_for_node(conn, id, &labels, None, &props)?;
                fts::update_fts_for_node(conn, id, &labels, None, &props)?;
                for item in on_create {
                    apply_merge_set_item_node(conn, item, id, rec)?;
                }
                let mut out_rec = rec.clone();
                out_rec.set(alias.to_string(), Value::I64(id.0 as i64));
                out_rec.set(format!("{alias}.__id"), Value::I64(id.0 as i64));
                // Populate prop keys (post-ON CREATE) so slot-path Property
                // reads land on populated slots. Skip __label/__labels —
                // see exec_create_node note re: compound-binding heuristic.
                let resolved = node::get_node(conn, id)?;
                for (k, v) in &resolved.properties {
                    out_rec.set(format!("{alias}.{k}"), v.clone());
                }
                // Bind path variable if present.
                if let Some(ref path_var) = merge_pattern.path_variable {
                    out_rec.set(path_var.clone(), Value::Path(PathValue::single(resolved)));
                }
                result.push(out_rec);
            } else {
                // One or more matches — produce one row per matching node.
                for n in &matches {
                    for item in on_match {
                        apply_merge_set_item_node(conn, item, n.id, rec)?;
                    }
                    let mut out_rec = rec.clone();
                    out_rec.set(alias.to_string(), Value::I64(n.id.0 as i64));
                    out_rec.set(format!("{alias}.__id"), Value::I64(n.id.0 as i64));
                    // Re-fetch (ON MATCH may have written) and populate prop keys.
                    let resolved = node::get_node(conn, n.id)?;
                    for (k, v) in &resolved.properties {
                        out_rec.set(format!("{alias}.{k}"), v.clone());
                    }
                    // Bind path variable if present.
                    if let Some(ref path_var) = merge_pattern.path_variable {
                        out_rec.set(path_var.clone(), Value::Path(PathValue::single(resolved)));
                    }
                    result.push(out_rec);
                }
            }
        }
        return Ok(result);
    }

    // Relationship merge: (src)-[:TYPE {props}]->(dst)
    if elements.len() != 3 {
        return Err(GraphError::semantic(
            "MERGE pattern must be a single node or (node)-[rel]->(node)",
        ));
    }

    let src_node = match &elements[0] {
        PatternElement::Node(n) => n,
        _ => return Err(GraphError::semantic("expected node pattern")),
    };
    let rel = match &elements[1] {
        PatternElement::Relationship(r) => r,
        _ => return Err(GraphError::semantic("expected relationship pattern")),
    };
    let dst_node = match &elements[2] {
        PatternElement::Node(n) => n,
        _ => return Err(GraphError::semantic("expected node pattern")),
    };

    let src_var = src_node
        .variable
        .as_deref()
        .ok_or_else(|| GraphError::semantic("MERGE relationship source must have a variable"))?;
    let dst_var = dst_node
        .variable
        .as_deref()
        .ok_or_else(|| GraphError::semantic("MERGE relationship target must have a variable"))?;
    for rec in &records {
        let src_id = match rec.get(src_var) {
            Some(Value::I64(id)) => NodeId(*id as u64),
            _ => continue,
        };
        let dst_id = match rec.get(dst_var) {
            Some(Value::I64(id)) => NodeId(*id as u64),
            _ => continue,
        };
        result.extend(merge_edge_between(
            conn,
            rec,
            src_id,
            dst_id,
            rel,
            merge_pattern.path_variable.as_ref(),
            on_create,
            on_match,
        )?);
    }

    Ok(result)
}

/// Find a node carrying every label in `labels` and matching `properties`.
pub(in crate::cypher::executor) fn find_merge_match_multi_label(
    conn: &Connection,
    labels: &[&str],
    properties: &Properties,
) -> Result<Option<crate::types::Node>> {
    let primary = labels.first().copied().unwrap_or("");
    let candidates = find_merge_matches_by_label_and_props(conn, primary, properties)?;
    // Require the node to carry every label in the pattern.
    Ok(candidates
        .into_iter()
        .find(|n| labels.iter().all(|req| n.labels.iter().any(|l| l == req))))
}

/// Collect all nodes matching a single label + property values (no multi-label filter).
///
/// When `label` is empty, performs a full node scan (handles label-less `MERGE (a)`).
pub(in crate::cypher::executor) fn find_merge_matches_by_label_and_props(
    conn: &Connection,
    label: &str,
    properties: &Properties,
) -> Result<Vec<crate::types::Node>> {
    // Try indexed lookup on the primary label (skipped when label is empty).
    let indexes = index::list_indexes_for_label(conn, label)?;
    let indexed_props: Vec<String> = indexes
        .iter()
        .filter(|info| info.properties.len() == 1)
        .map(|info| info.properties[0].clone())
        .collect();
    let indexed_props: Vec<&str> = indexed_props.iter().map(|s| s.as_str()).collect();

    for (key, value) in properties {
        if indexed_props.contains(&key.as_str()) {
            let ids = index::index_lookup(conn, label, key, value)?;
            let mut matches = Vec::new();
            for id in ids {
                let n = node::get_node(conn, id)?;
                let all_match = properties
                    .iter()
                    .all(|(k, expected)| n.properties.get(k) == Some(expected));
                if all_match {
                    matches.push(n);
                }
            }
            return Ok(matches);
        }
    }

    // Fall back to label scan.
    let existing = node::find_nodes_by_label(conn, label)?;
    Ok(existing
        .into_iter()
        .filter(|n| {
            properties
                .iter()
                .all(|(key, expected)| n.properties.get(key) == Some(expected))
        })
        .collect())
}

/// Find ALL nodes matching label + pre-evaluated property values.
pub(in crate::cypher::executor) fn find_merge_matches_evaluated(
    conn: &Connection,
    label: &str,
    properties: &Properties,
) -> Result<Vec<crate::types::Node>> {
    // Try indexed lookup first.
    let indexes = index::list_indexes_for_label(conn, label)?;
    let indexed_props: Vec<String> = indexes
        .iter()
        .filter(|info| info.properties.len() == 1)
        .map(|info| info.properties[0].clone())
        .collect();
    let indexed_props: Vec<&str> = indexed_props.iter().map(|s| s.as_str()).collect();

    for (key, value) in properties {
        if indexed_props.contains(&key.as_str()) {
            let ids = index::index_lookup(conn, label, key, value)?;
            let mut matches = Vec::new();
            for id in ids {
                let n = node::get_node(conn, id)?;
                let all_match = properties
                    .iter()
                    .all(|(k, expected)| n.properties.get(k) == Some(expected));
                if all_match {
                    matches.push(n);
                }
            }
            return Ok(matches);
        }
    }

    // No index — label scan.
    let existing = node::find_nodes_by_label(conn, label)?;
    Ok(existing
        .into_iter()
        .filter(|n| {
            properties
                .iter()
                .all(|(key, expected)| n.properties.get(key) == Some(expected))
        })
        .collect())
}
